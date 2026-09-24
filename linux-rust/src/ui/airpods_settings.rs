//! AirPods settings driven by AACP control commands: press and hold, call controls,
//! microphone, accessibility and Adaptive Audio, grouped like the Android app.
//!
//! A setting is only shown once the AirPods have reported a value for it, which
//! keeps controls for features a model lacks off the page. Changes are sent right
//! away and stored in `AirPodsState::control_values` so the page reflects them before
//! the AirPods confirm.

use {
    crate::{
        bluetooth::{
            aacp::{AACPManager, ControlCommandIdentifiers as Id},
            settings::{
                AdaptiveStrength, CallControls, ChimeVolume, ClickHoldAction, ClickHoldMode,
                ControlValue, CycleMode, ListeningModeCycle, MicMode, PERCENT_MAX, PressInterval,
                SwipeInterval, Toggle,
            },
        },
        devices::enums::{AirPodsState, DeviceState},
        ui::window::Message,
    },
    iced::{
        Background, Border, Center, Element, Length, Padding, Theme,
        border::Radius,
        widget::{Space, column, container, pick_list, row, rule, slider, text, toggler},
    },
    std::{rc::Rc, sync::Arc},
    tracing::error,
};

const PICK_LIST_WIDTH: f32 = 160.0;

/// Slider step for the percentage settings. Every step sends one control command
/// while dragging, so a coarse step keeps a full drag to about twenty packets.
const PERCENT_STEP: u8 = 5;

/// Tone volume shown when the AirPods report a value this page cannot decode; the
/// Android app uses the same fallback.
const DEFAULT_CHIME_VOLUME: u8 = 75;

/// Adaptive Audio strength shown when the reported value cannot be decoded.
const DEFAULT_ADAPTIVE_STRENGTH: u8 = 50;

/// What every control on the page needs to read the current value and send a change.
struct Settings {
    mac: String,
    state: AirPodsState,
    aacp_manager: Arc<AACPManager>,
}

impl Settings {
    fn reported(&self, id: Id) -> bool {
        self.state.control_values.contains_key(&(id as u8))
    }

    fn get<T: ControlValue>(&self, id: Id) -> Option<T> {
        self.state
            .control_values
            .get(&(id as u8))
            .and_then(|value| T::from_value(value))
    }

    /// Sends `value` to the AirPods and returns the state with the value applied.
    fn set<T: ControlValue>(&self, id: Id, value: T) -> Message {
        let value = value.to_value();
        let aacp_manager = Arc::clone(&self.aacp_manager);
        let sent = value.clone();
        self.aacp_manager.runtime().spawn(async move {
            if let Err(e) = aacp_manager.send_control_command(id, &sent).await {
                error!("Failed to send {} control command {:02x?}: {}", id, sent, e);
            }
        });
        let mut state = self.state.clone();
        state.control_values.insert(id as u8, value);
        Message::StateChanged(self.mac.clone(), DeviceState::AirPods(state))
    }
}

/// Builds the settings sections for the AirPods page. Sections without any
/// reported setting are left out, so the result can be empty.
pub fn settings_sections<'a>(
    mac: &str,
    state: &AirPodsState,
    aacp_manager: Arc<AACPManager>,
) -> Element<'a, Message> {
    let settings = Rc::new(Settings {
        mac: mac.to_string(),
        state: state.clone(),
        aacp_manager,
    });

    let sections = [
        press_and_hold(&settings),
        call_controls(&settings),
        microphone(&settings),
        sleep_detection(&settings),
        accessibility(&settings),
        adaptive_audio(&settings),
    ];

    let mut content = column![];
    for section in sections.into_iter().flatten() {
        content = content
            .push(section)
            .push(Space::new().height(Length::from(20)));
    }
    content.into()
}

fn press_and_hold<'a>(settings: &Rc<Settings>) -> Option<Element<'a, Message>> {
    let mut rows = Vec::new();

    if settings.reported(Id::ClickHoldMode) {
        let current = settings.get::<ClickHoldMode>(Id::ClickHoldMode);
        // With an unknown value for one bud, the other keeps the factory default.
        let base = current.unwrap_or(ClickHoldMode {
            right: ClickHoldAction::NoiseControl,
            left: ClickHoldAction::NoiseControl,
        });
        let left = {
            let settings = Rc::clone(settings);
            pick_list(
                &ClickHoldAction::ALL[..],
                current.map(|mode| mode.left),
                move |left| settings.set(Id::ClickHoldMode, ClickHoldMode { left, ..base }),
            )
        };
        let right = {
            let settings = Rc::clone(settings);
            pick_list(
                &ClickHoldAction::ALL[..],
                current.map(|mode| mode.right),
                move |right| settings.set(Id::ClickHoldMode, ClickHoldMode { right, ..base }),
            )
        };
        rows.push(setting_row("Left", None, styled_pick_list(left)));
        rows.push(setting_row("Right", None, styled_pick_list(right)));
    }

    if settings.reported(Id::ListeningModeConfigs) {
        let cycle = settings.get::<ListeningModeCycle>(Id::ListeningModeConfigs);
        rows.push(
            column![
                text("Listening Modes").size(16),
                dim_text("Press and hold the stem to cycle between the selected listening modes."),
            ]
            .into(),
        );
        for mode in CycleMode::ALL {
            if mode == CycleMode::Off && !settings.state.allow_off_mode {
                continue;
            }
            let enabled = cycle.is_some_and(|cycle| cycle.contains(mode));
            let mut switch = toggler(enabled).spacing(0).size(20);
            // A mode that cannot be removed without leaving fewer than two in the
            // cycle stays locked on, as in the Android app.
            if let Some(next) = cycle.and_then(|cycle| cycle.toggled(mode)) {
                let settings = Rc::clone(settings);
                switch = switch.on_toggle(move |_| settings.set(Id::ListeningModeConfigs, next));
            }
            rows.push(setting_row(mode.label(), Some(mode.description()), switch));
        }
    }

    section(Some("Press and Hold AirPods"), rows)
}

fn call_controls<'a>(settings: &Rc<Settings>) -> Option<Element<'a, Message>> {
    if !settings.reported(Id::CallManagementConfig) {
        return None;
    }
    let current = settings.get::<CallControls>(Id::CallManagementConfig);
    let mute = choice(settings, Id::CallManagementConfig, &CallControls::ALL[..]);
    let rows = vec![
        setting_row("Answer Call", None, text("Press Once").size(14)),
        setting_row("Mute/Unmute", None, mute),
        setting_row(
            "Hang Up",
            None,
            text(current.map_or("", CallControls::hang_up_press)).size(14),
        ),
    ];
    section(Some("Call Controls"), rows)
}

fn microphone<'a>(settings: &Rc<Settings>) -> Option<Element<'a, Message>> {
    if !settings.reported(Id::MicMode) {
        return None;
    }
    let rows = vec![setting_row(
        "Microphone Mode",
        None,
        choice(settings, Id::MicMode, &MicMode::ALL[..]),
    )];
    section(Some("Microphone"), rows)
}

fn sleep_detection<'a>(settings: &Rc<Settings>) -> Option<Element<'a, Message>> {
    if !settings.reported(Id::SleepDetectionConfig) {
        return None;
    }
    let rows = vec![setting_row(
        "Pause media when falling asleep",
        None,
        switch(settings, Id::SleepDetectionConfig),
    )];
    section(None, rows)
}

fn accessibility<'a>(settings: &Rc<Settings>) -> Option<Element<'a, Message>> {
    let mut rows = Vec::new();

    if settings.reported(Id::DoubleClickInterval) {
        rows.push(setting_row(
            "Press Speed",
            Some("Adjust the speed required to press two or three times on your AirPods."),
            choice(settings, Id::DoubleClickInterval, &PressInterval::ALL[..]),
        ));
    }
    if settings.reported(Id::ClickHoldInterval) {
        rows.push(setting_row(
            "Press and Hold Duration",
            Some("Adjust the duration required to press and hold on your AirPods."),
            choice(settings, Id::ClickHoldInterval, &PressInterval::ALL[..]),
        ));
    }
    if settings.reported(Id::OneBudAncMode) {
        rows.push(setting_row(
            "Noise Cancellation with Single AirPod",
            Some(
                "Allow AirPods to be put in noise cancellation mode when only one AirPod is in your ear.",
            ),
            switch(settings, Id::OneBudAncMode),
        ));
    }
    if settings.reported(Id::ChimeVolume) {
        let volume = settings
            .get::<ChimeVolume>(Id::ChimeVolume)
            .map_or(DEFAULT_CHIME_VOLUME, ChimeVolume::percent);
        let settings = Rc::clone(settings);
        rows.push(slider_row(
            "Tone Volume",
            "Adjust the tone volume of sound effects played by AirPods.",
            ("Quiet", "Loud"),
            volume,
            move |volume| settings.set(Id::ChimeVolume, ChimeVolume::new(volume)),
        ));
    }
    if settings.reported(Id::VolumeSwipeMode) {
        rows.push(setting_row(
            "Volume Control",
            Some("Adjust the volume by swiping up or down on the sensor located on the AirPods Pro stem."),
            switch(settings, Id::VolumeSwipeMode),
        ));
    }
    if settings.reported(Id::VolumeSwipeInterval) {
        rows.push(setting_row(
            "Volume Swipe Speed",
            Some("To prevent unintended volume adjustments, select preferred wait time between swipes."),
            choice(settings, Id::VolumeSwipeInterval, &SwipeInterval::ALL[..]),
        ));
    }

    section(Some("Accessibility"), rows)
}

fn adaptive_audio<'a>(settings: &Rc<Settings>) -> Option<Element<'a, Message>> {
    if !settings.reported(Id::AutoAncStrength) {
        return None;
    }
    let strength = settings
        .get::<AdaptiveStrength>(Id::AutoAncStrength)
        .map_or(DEFAULT_ADAPTIVE_STRENGTH, AdaptiveStrength::strength);
    let settings = Rc::clone(settings);
    // The slider runs from less to more outside noise, the inverse of the strength
    // the AirPods store.
    let rows = vec![slider_row(
        "Customize Adaptive Audio",
        "Adaptive audio dynamically responds to your environment and cancels or allows external noise. You can customize Adaptive Audio to allow more or less noise.",
        ("Less noise", "More noise"),
        PERCENT_MAX - strength,
        move |position| {
            settings.set(
                Id::AutoAncStrength,
                AdaptiveStrength::new(PERCENT_MAX.saturating_sub(position)),
            )
        },
    )];
    section(Some("Adaptive Audio"), rows)
}

fn choice<'a, T>(settings: &Rc<Settings>, id: Id, options: &'static [T]) -> Element<'a, Message>
where
    T: ControlValue + ToString + PartialEq + Clone + 'static,
{
    let current = settings.get::<T>(id);
    let settings = Rc::clone(settings);
    styled_pick_list(pick_list(options, current, move |value| {
        settings.set(id, value)
    }))
}

fn switch<'a>(settings: &Rc<Settings>, id: Id) -> Element<'a, Message> {
    let enabled = settings.get::<Toggle>(id).is_some_and(|toggle| toggle.0);
    let settings = Rc::clone(settings);
    toggler(enabled)
        .on_toggle(move |enabled| settings.set(id, Toggle(enabled)))
        .spacing(0)
        .size(20)
        .into()
}

fn styled_pick_list<'a, T>(
    list: iced::widget::PickList<'a, T, &'static [T], T, Message>,
) -> Element<'a, Message>
where
    T: ToString + PartialEq + Clone + 'static,
{
    list.width(Length::Fixed(PICK_LIST_WIDTH))
        .text_size(14)
        .padding(Padding {
            top: 5.0,
            bottom: 5.0,
            left: 10.0,
            right: 10.0,
        })
        .style(|theme: &Theme, _status| pick_list::Style {
            text_color: theme.palette().text,
            placeholder_color: theme.palette().text.scale_alpha(0.7),
            handle_color: theme.palette().text,
            background: Background::Color(theme.palette().primary.scale_alpha(0.2)),
            border: Border {
                width: 1.0,
                color: theme.palette().text.scale_alpha(0.3),
                radius: Radius::from(4.0),
            },
        })
        .into()
}

fn setting_row<'a>(
    title: &'static str,
    description: Option<&'static str>,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut label = column![text(title).size(16)].width(Length::Fill);
    if let Some(description) = description {
        label = label.push(dim_text(description).width(Length::Fill));
    }
    row![label, control.into()]
        .align_y(Center)
        .spacing(8)
        .into()
}

fn slider_row<'a>(
    title: &'static str,
    description: &'static str,
    (start, end): (&'static str, &'static str),
    value: u8,
    on_change: impl Fn(u8) -> Message + 'a,
) -> Element<'a, Message> {
    column![
        text(title).size(16),
        dim_text(description).width(Length::Fill),
        row![
            dim_text(start),
            slider(0..=PERCENT_MAX, value, on_change).step(PERCENT_STEP),
            dim_text(end),
        ]
        .align_y(Center)
        .spacing(8),
    ]
    .spacing(4)
    .into()
}

fn dim_text<'a>(content: &'static str) -> iced::widget::Text<'a> {
    text(content).size(12).style(|theme: &Theme| text::Style {
        color: Some(theme.palette().text.scale_alpha(0.7)),
    })
}

/// A titled card with one setting per row, separated by thin rules like the other
/// cards on the AirPods page. Returns `None` when there are no rows.
fn section<'a>(
    title: Option<&'static str>,
    rows: Vec<Element<'a, Message>>,
) -> Option<Element<'a, Message>> {
    if rows.is_empty() {
        return None;
    }

    let mut card = column![].spacing(4).padding(8);
    for (i, setting) in rows.into_iter().enumerate() {
        if i > 0 {
            card = card.push(rule::horizontal(1).style(|theme: &Theme| rule::Style {
                color: theme.palette().text.scale_alpha(0.2),
                radius: Radius::from(12),
                fill_mode: rule::FillMode::Full,
                snap: false,
            }));
        }
        card = card.push(setting);
    }

    let card = container(card)
        .padding(Padding {
            top: 5.0,
            bottom: 5.0,
            left: 10.0,
            right: 10.0,
        })
        .style(|theme: &Theme| container::Style {
            background: Some(Background::Color(theme.palette().primary.scale_alpha(0.1))),
            border: Border {
                color: theme.palette().primary.scale_alpha(0.5),
                ..Border::default()
            }
            .rounded(16),
            ..container::Style::default()
        });

    let mut content = column![];
    if let Some(title) = title {
        content = content.push(
            container(text(title).size(18).style(|theme: &Theme| text::Style {
                color: Some(theme.palette().primary),
            }))
            .padding(Padding {
                top: 5.0,
                bottom: 5.0,
                left: 18.0,
                right: 18.0,
            }),
        );
    }
    Some(content.push(card).into())
}
