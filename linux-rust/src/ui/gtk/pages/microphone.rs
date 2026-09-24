//! The hi-res microphone section of the AirPods page: its switch, a level
//! meter while an app records, and the microphone test.

use {
    crate::ui::gtk::{
        model::{AirPods, Input, MicInput, Model, PlaybackView},
        pages::airpods::{Section, SectionContext},
        widgets::{Dispatch, Guarded, switch_row},
    },
    adw::prelude::*,
    std::time::Duration,
};

/// Scale step of the playback position, in seconds.
const SEEK_STEP: f64 = 0.1;

pub(crate) struct MicrophoneSection {
    group: adw::PreferencesGroup,
    hires: Guarded<gtk::Switch>,
    level_row: adw::ActionRow,
    level: gtk::LevelBar,
    test: TestRow,
    playback: PlaybackRows,
}

impl MicrophoneSection {
    pub(crate) fn new(cx: &SectionContext<'_>) -> Self {
        let (hires_row, hires) = {
            let dispatch = cx.dispatch.clone();
            let mac = cx.mac.clone();
            switch_row(
                "Hi-Res Microphone",
                Some(
                    "Captures the AirPods' high-quality AAC-ELD microphone stream and exposes \
                     it as an 'AirPodsHiRes' input.",
                ),
                move |on| dispatch.send(Input::Microphone(MicInput::SetHiRes(mac.get(), on))),
            )
        };

        let level = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .width_request(160)
            .build();
        // The default "low" block is drawn as a warning, which a quiet
        // voice is not.
        level.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_LOW));
        let level_row = adw::ActionRow::builder().title("Input level").build();
        level_row.add_suffix(&level);

        let test = TestRow::new(cx.dispatch);
        let playback = PlaybackRows::new(cx.dispatch);

        let group = adw::PreferencesGroup::builder().title("Microphone").build();
        group.add(&hires_row);
        group.add(&level_row);
        group.add(&test.row);
        group.add(&playback.seek_row);
        group.add(&playback.controls_row);
        Self {
            group,
            hires,
            level_row,
            level,
            test,
            playback,
        }
    }
}

impl Section for MicrophoneSection {
    fn group(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    fn render(&self, mac: &str, _airpods: &AirPods, model: &Model) {
        let enabled = model.settings().hires_mic_enabled;
        self.hires.set_active(enabled);

        let capture = model.mic_capture(mac);
        self.level_row.set_visible(capture.is_some());
        if let Some(capture) = capture {
            let title = capture.title();
            if self.level_row.title() != title {
                self.level_row.set_title(&title);
            }
            let level = f64::from(capture.level);
            if (self.level.value() - level).abs() > f64::EPSILON {
                self.level.set_value(level);
            }
        }

        let view = model.mic_test().view();
        self.test.row.set_visible(enabled);
        if self.test.row.subtitle().as_deref() != Some(view.status.as_str()) {
            self.test.row.set_subtitle(&view.status);
        }
        self.test.record.set_visible(view.record.is_some());
        if let Some(label) = view.record {
            set_label(&self.test.record, label);
        }
        self.test.stop.set_visible(view.stop);
        self.test.done.set_visible(view.playback.is_some());
        self.playback
            .render(view.playback.as_ref().filter(|_| enabled));
    }
}

/// The microphone test row: its status and the Record, Stop and Done
/// buttons.
struct TestRow {
    row: adw::ActionRow,
    record: gtk::Button,
    stop: gtk::Button,
    done: gtk::Button,
}

impl TestRow {
    fn new(dispatch: &Dispatch) -> Self {
        let record = button(dispatch, "Record", MicInput::Record);
        let stop = button(dispatch, "Stop", MicInput::Stop);
        let done = button(dispatch, "Done", MicInput::Done);
        record.add_css_class("suggested-action");
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        for widget in [&record, &stop, &done] {
            buttons.append(widget);
        }
        let row = adw::ActionRow::builder()
            .title("Microphone Test")
            .subtitle_lines(0)
            .build();
        row.add_suffix(&buttons);
        Self {
            row,
            record,
            stop,
            done,
        }
    }
}

/// The player of a finished take: position slider and transport buttons.
struct PlaybackRows {
    seek_row: adw::PreferencesRow,
    seek: Guarded<gtk::Scale>,
    position: gtk::Label,
    controls_row: adw::PreferencesRow,
    back: gtk::Button,
    play: gtk::Button,
    pause: gtk::Button,
    forward: gtk::Button,
}

impl PlaybackRows {
    fn new(dispatch: &Dispatch) -> Self {
        let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, SEEK_STEP, SEEK_STEP);
        seek.set_draw_value(false);
        seek.set_hexpand(true);
        let seek = {
            let dispatch = dispatch.clone();
            Guarded::new(seek, move |s| {
                s.connect_value_changed(move |s| {
                    let to = Duration::try_from_secs_f64(s.value()).unwrap_or_default();
                    dispatch.send(Input::Microphone(MicInput::Seek(to)));
                })
            })
        };
        let position = gtk::Label::builder()
            .css_classes(["dim-label", "numeric"])
            .build();
        let seek_box = row_box(12);
        seek_box.append(seek.widget());
        seek_box.append(&position);

        let back = button(dispatch, "", MicInput::Skip { forward: false });
        let play = button(dispatch, "Play", MicInput::Play);
        let pause = button(dispatch, "Pause", MicInput::Pause);
        let forward = button(dispatch, "", MicInput::Skip { forward: true });
        let controls = row_box(6);
        controls.set_halign(gtk::Align::Center);
        for widget in [&back, &play, &pause, &forward] {
            controls.append(widget);
        }
        Self {
            seek_row: plain_row(&seek_box),
            seek,
            position,
            controls_row: plain_row(&controls),
            back,
            play,
            pause,
            forward,
        }
    }

    /// Show the player for `playback`, or hide it for None.
    fn render(&self, playback: Option<&PlaybackView>) {
        self.seek_row.set_visible(playback.is_some());
        self.controls_row.set_visible(playback.is_some());
        let Some(playback) = playback else {
            return;
        };
        let upper = playback.duration.as_secs_f64().max(SEEK_STEP);
        let value = playback.position.as_secs_f64();
        let adjustment = self.seek.widget().adjustment();
        if (adjustment.upper() - upper).abs() > f64::EPSILON
            || (adjustment.value() - value).abs() > f64::EPSILON
        {
            self.seek.set(|s| {
                s.set_range(0.0, upper);
                s.set_value(value);
            });
        }
        if self.position.label() != playback.label {
            self.position.set_label(&playback.label);
        }
        set_label(&self.back, &playback.back);
        set_label(&self.forward, &playback.forward);
        self.play.set_visible(!playback.playing);
        self.pause.set_visible(playback.playing);
    }
}

/// A button that sends `input` when clicked. Rendering never clicks, so it
/// needs no guard.
fn button(dispatch: &Dispatch, label: &str, input: MicInput) -> gtk::Button {
    let button = gtk::Button::builder()
        .label(label)
        .valign(gtk::Align::Center)
        .build();
    let dispatch = dispatch.clone();
    button.connect_clicked(move |_| dispatch.send(Input::Microphone(input.clone())));
    button
}

fn set_label(button: &gtk::Button, label: &str) {
    if button.label().as_deref() != Some(label) {
        button.set_label(label);
    }
}

fn row_box(spacing: i32) -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(spacing)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build()
}

/// A list row holding `child` as is, for widgets that do not fit a title and
/// suffixes.
fn plain_row(child: &gtk::Box) -> adw::PreferencesRow {
    let row = adw::PreferencesRow::builder()
        .activatable(false)
        .selectable(false)
        .build();
    row.set_child(Some(child));
    row
}
