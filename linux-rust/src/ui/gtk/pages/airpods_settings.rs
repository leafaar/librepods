//! The AirPods settings sections of the AirPods page: press and hold, calls,
//! microphone, sleep detection, accessibility and Adaptive Audio.
//!
//! Each section is a `ControlGroup` of rows bound to one control command. A
//! row shows only once the AirPods reported a value for its command and a
//! group only while one of its rows shows. What a row shows and what a change
//! sends is decided in `gtk::controls`.

use {
    crate::{
        bluetooth::{
            aacp::ControlCommandIdentifiers as Id,
            settings::{
                CallControls, ClickHoldAction, ClickHoldMode, ControlValue, CycleMode, MicMode,
                PERCENT_MAX, PressInterval, SwipeInterval,
            },
        },
        ui::gtk::{
            controls::{
                Bud, ControlChange, SLIDER_STEP, adaptive_noise, cycle_rows, hang_up_press,
                slider_position, tone_volume,
            },
            model::{AirPods, Input, Model},
            pages::airpods::{Section, SectionContext},
            widgets::{CurrentDevice, Dispatch, Guarded, combo_row, switch_row, value_row},
        },
    },
    adw::prelude::*,
    std::fmt::Display,
};

/// One row, or a few rows that belong together, of a settings group.
trait ControlRow {
    fn widget(&self) -> &gtk::Widget;

    /// Set the row from the AirPods and return whether it shows.
    fn render(&self, airpods: &AirPods) -> bool;
}

/// A titled group of control rows, hidden while none of them shows.
pub(crate) struct ControlGroup {
    group: adw::PreferencesGroup,
    rows: Vec<Box<dyn ControlRow>>,
}

impl ControlGroup {
    fn new(title: Option<&str>, rows: Vec<Box<dyn ControlRow>>) -> Self {
        let group = adw::PreferencesGroup::new();
        if let Some(title) = title {
            group.set_title(title);
        }
        for row in &rows {
            group.add(row.widget());
        }
        Self { group, rows }
    }

    pub(crate) fn press_and_hold(cx: &SectionContext<'_>) -> Self {
        let long_press = |title, bud, pick: fn(ClickHoldMode) -> ClickHoldAction| {
            ChoiceRow::new(
                cx,
                title,
                None,
                Id::ClickHoldMode,
                &ClickHoldAction::ALL,
                pick,
                move |action| ControlChange::LongPress(bud, action),
            )
        };
        Self::new(
            Some("Press and Hold AirPods"),
            vec![
                Box::new(long_press("Left", Bud::Left, |mode| mode.left)),
                Box::new(long_press("Right", Bud::Right, |mode| mode.right)),
                Box::new(CycleRows::new(cx)),
            ],
        )
    }

    pub(crate) fn calls(cx: &SectionContext<'_>) -> Self {
        Self::new(
            Some("Call Controls"),
            vec![
                Box::new(TextRow::new("Answer Call", |a| {
                    a.reported(Id::CallManagementConfig).then_some("Press Once")
                })),
                Box::new(ChoiceRow::new(
                    cx,
                    "Mute/Unmute",
                    None,
                    Id::CallManagementConfig,
                    &CallControls::ALL,
                    |controls| controls,
                    ControlChange::MuteControl,
                )),
                Box::new(TextRow::new("Hang Up", hang_up_press)),
            ],
        )
    }

    pub(crate) fn microphone(cx: &SectionContext<'_>) -> Self {
        Self::new(
            Some("Microphone"),
            vec![Box::new(ChoiceRow::new(
                cx,
                "Microphone Mode",
                None,
                Id::MicMode,
                &MicMode::ALL,
                |mode| mode,
                ControlChange::MicMode,
            ))],
        )
    }

    pub(crate) fn sleep(cx: &SectionContext<'_>) -> Self {
        Self::new(
            None,
            vec![Box::new(ToggleRow::new(
                cx,
                "Pause media when falling asleep",
                None,
                Id::SleepDetectionConfig,
                ControlChange::SleepDetection,
            ))],
        )
    }

    pub(crate) fn accessibility(cx: &SectionContext<'_>) -> Self {
        Self::new(
            Some("Accessibility"),
            vec![
                Box::new(ChoiceRow::new(
                    cx,
                    "Press Speed",
                    Some("Adjust the speed required to press two or three times on your AirPods."),
                    Id::DoubleClickInterval,
                    &PressInterval::ALL,
                    |interval| interval,
                    ControlChange::PressSpeed,
                )),
                Box::new(ChoiceRow::new(
                    cx,
                    "Press and Hold Duration",
                    Some("Adjust the duration required to press and hold on your AirPods."),
                    Id::ClickHoldInterval,
                    &PressInterval::ALL,
                    |interval| interval,
                    ControlChange::HoldDuration,
                )),
                Box::new(ToggleRow::new(
                    cx,
                    "Noise Cancellation with Single AirPod",
                    Some(
                        "Allow AirPods to be put in noise cancellation mode when only one AirPod \
                         is in your ear.",
                    ),
                    Id::OneBudAncMode,
                    ControlChange::OneBudAnc,
                )),
                Box::new(SliderRow::new(
                    cx,
                    "Tone Volume",
                    "Adjust the tone volume of sound effects played by AirPods.",
                    ("Quiet", "Loud"),
                    tone_volume,
                    ControlChange::ToneVolume,
                )),
                Box::new(ToggleRow::new(
                    cx,
                    "Volume Control",
                    Some(
                        "Adjust the volume by swiping up or down on the sensor located on the \
                         AirPods Pro stem.",
                    ),
                    Id::VolumeSwipeMode,
                    ControlChange::VolumeSwipe,
                )),
                Box::new(ChoiceRow::new(
                    cx,
                    "Volume Swipe Speed",
                    Some(
                        "To prevent unintended volume adjustments, select preferred wait time \
                         between swipes.",
                    ),
                    Id::VolumeSwipeInterval,
                    &SwipeInterval::ALL,
                    |interval| interval,
                    ControlChange::SwipeSpeed,
                )),
            ],
        )
    }

    pub(crate) fn adaptive_audio(cx: &SectionContext<'_>) -> Self {
        Self::new(
            Some("Adaptive Audio"),
            vec![Box::new(SliderRow::new(
                cx,
                "Customize Adaptive Audio",
                "Adaptive Audio dynamically responds to your environment and cancels or allows \
                 external noise. You can customize Adaptive Audio to allow more or less noise.",
                ("Less noise", "More noise"),
                adaptive_noise,
                ControlChange::AdaptiveNoise,
            ))],
        )
    }
}

impl Section for ControlGroup {
    fn group(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    fn render(&self, _mac: &str, airpods: &AirPods, _model: &Model) {
        let mut any = false;
        for row in &self.rows {
            let shown = row.render(airpods);
            row.widget().set_visible(shown);
            any |= shown;
        }
        self.group.set_visible(any);
    }
}

/// Queues `change` for the AirPods the page shows.
fn sender<T: 'static>(
    cx: &SectionContext<'_>,
    change: impl Fn(T) -> ControlChange + 'static,
) -> impl Fn(T) + 'static {
    let dispatch: Dispatch = cx.dispatch.clone();
    let mac: CurrentDevice = cx.mac.clone();
    move |value| dispatch.send(Input::AirPodsControl(mac.get(), change(value)))
}

/// A combo row over the values of one command. `pick` reads the option out
/// of the decoded value, which for the long press is one bud of two.
struct ChoiceRow<V, T: 'static> {
    row: Guarded<adw::ComboRow>,
    id: Id,
    options: &'static [T],
    pick: fn(V) -> T,
}

impl<V, T: Copy + PartialEq + Display + 'static> ChoiceRow<V, T> {
    fn new(
        cx: &SectionContext<'_>,
        title: &str,
        subtitle: Option<&str>,
        id: Id,
        options: &'static [T],
        pick: fn(V) -> T,
        change: impl Fn(T) -> ControlChange + 'static,
    ) -> Self {
        let labels: Vec<String> = options.iter().map(ToString::to_string).collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let send = sender(cx, change);
        let row = combo_row(title, subtitle, &labels, move |index| {
            if let Some(option) = options.get(index as usize) {
                send(*option);
            }
        });
        Self {
            row,
            id,
            options,
            pick,
        }
    }
}

impl<V: ControlValue, T: PartialEq> ControlRow for ChoiceRow<V, T> {
    fn widget(&self) -> &gtk::Widget {
        self.row.widget().upcast_ref()
    }

    fn render(&self, airpods: &AirPods) -> bool {
        let current = airpods.control::<V>(self.id).map(self.pick);
        self.row
            .select(current.and_then(|c| self.options.iter().position(|o| *o == c)));
        airpods.reported(self.id)
    }
}

/// An on/off setting.
struct ToggleRow {
    row: adw::ActionRow,
    switch: Guarded<gtk::Switch>,
    id: Id,
}

impl ToggleRow {
    fn new(
        cx: &SectionContext<'_>,
        title: &str,
        subtitle: Option<&str>,
        id: Id,
        change: fn(bool) -> ControlChange,
    ) -> Self {
        let (row, switch) = switch_row(title, subtitle, sender(cx, change));
        Self { row, switch, id }
    }
}

impl ControlRow for ToggleRow {
    fn widget(&self) -> &gtk::Widget {
        self.row.upcast_ref()
    }

    fn render(&self, airpods: &AirPods) -> bool {
        let Some(on) = airpods.toggle(self.id) else {
            return false;
        };
        self.switch.set_active(on);
        true
    }
}

/// A read-only row with a text on the right.
struct TextRow {
    row: adw::ActionRow,
    label: gtk::Label,
    text: fn(&AirPods) -> Option<&'static str>,
}

impl TextRow {
    fn new(title: &str, text: fn(&AirPods) -> Option<&'static str>) -> Self {
        let (row, label) = value_row(title);
        Self { row, label, text }
    }
}

impl ControlRow for TextRow {
    fn widget(&self) -> &gtk::Widget {
        self.row.upcast_ref()
    }

    fn render(&self, airpods: &AirPods) -> bool {
        let Some(text) = (self.text)(airpods) else {
            return false;
        };
        if self.label.label() != text {
            self.label.set_label(text);
        }
        true
    }
}

/// A 0 to 100 slider under a title and a description, with a caption at each
/// end. The model holds each change until the slider rests, so a drag does
/// not flood the AirPods.
struct SliderRow {
    row: adw::PreferencesRow,
    scale: Guarded<gtk::Scale>,
    position: fn(&AirPods) -> Option<u8>,
}

impl SliderRow {
    fn new(
        cx: &SectionContext<'_>,
        title: &str,
        description: &str,
        (start, end): (&str, &str),
        position: fn(&AirPods) -> Option<u8>,
        change: fn(u8) -> ControlChange,
    ) -> Self {
        let step = f64::from(SLIDER_STEP);
        let scale = gtk::Scale::builder()
            .orientation(gtk::Orientation::Horizontal)
            .adjustment(&gtk::Adjustment::new(
                0.0,
                0.0,
                f64::from(PERCENT_MAX),
                step,
                step * 2.0,
                0.0,
            ))
            .draw_value(false)
            .hexpand(true)
            .build();
        let send = sender(cx, change);
        let scale = Guarded::new(scale, move |s| {
            s.connect_value_changed(move |s| send(slider_position(s.value())))
        });
        let caption = |text: &str| {
            gtk::Label::builder()
                .label(text)
                .css_classes(["caption", "dim-label"])
                .build()
        };
        let slider = gtk::Box::builder().spacing(12).build();
        slider.append(&caption(start));
        slider.append(scale.widget());
        slider.append(&caption(end));
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&gtk::Label::builder().label(title).xalign(0.0).build());
        content.append(
            &gtk::Label::builder()
                .label(description)
                .xalign(0.0)
                .wrap(true)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        content.append(&slider);
        let row = adw::PreferencesRow::builder()
            .title(title)
            .activatable(false)
            .child(&content)
            .build();
        Self {
            row,
            scale,
            position,
        }
    }
}

impl ControlRow for SliderRow {
    fn widget(&self) -> &gtk::Widget {
        self.row.upcast_ref()
    }

    fn render(&self, airpods: &AirPods) -> bool {
        let Some(position) = (self.position)(airpods) else {
            return false;
        };
        let value = f64::from(position);
        // Setting it during a drag snaps the knob to the step the model holds.
        if (self.scale.widget().value() - value).abs() > f64::EPSILON {
            self.scale.set(|s| s.set_value(value));
        }
        true
    }
}

/// The listening modes a long press cycles through, one switch per mode in
/// an expander row.
struct CycleRows {
    expander: adw::ExpanderRow,
    modes: Vec<(adw::ActionRow, Guarded<gtk::Switch>)>,
}

impl CycleRows {
    fn new(cx: &SectionContext<'_>) -> Self {
        let expander = adw::ExpanderRow::builder()
            .title("Listening Modes")
            .subtitle("Press and hold the stem to cycle between the selected listening modes.")
            .build();
        let modes = CycleMode::ALL
            .iter()
            .map(|&mode| {
                let send = sender(cx, move |on| ControlChange::CycleMode(mode, on));
                let (row, switch) = switch_row(mode.label(), Some(mode.description()), send);
                expander.add_row(&row);
                (row, switch)
            })
            .collect();
        Self { expander, modes }
    }
}

impl ControlRow for CycleRows {
    fn widget(&self) -> &gtk::Widget {
        self.expander.upcast_ref()
    }

    fn render(&self, airpods: &AirPods) -> bool {
        let Some(rows) = cycle_rows(airpods) else {
            return false;
        };
        for ((row, switch), shown) in self.modes.iter().zip(rows) {
            row.set_visible(shown.shown);
            row.set_sensitive(!shown.locked);
            switch.set_active(shown.on);
        }
        true
    }
}
