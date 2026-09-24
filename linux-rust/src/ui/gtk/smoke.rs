//! Builds the real widgets and renders a sequence of model states. Needs a
//! display, so it is ignored by default; run it with
//! `G_DEBUG=fatal-criticals cargo test smoke -- --ignored --test-threads=1`.
//! It fails when GTK logs a critical or when rendering sends an input, which
//! means a handler was not guarded.

use {
    crate::{
        bluetooth::aacp::{AACPEvent, BatteryComponent, BatteryInfo, BatteryStatus},
        devices::enums::{DeviceData, DeviceType},
        ui::{
            gtk::{
                model::{
                    AirPodsSnapshot, DeviceSnapshot, EqBand, EqInput, Input, MicCapture, MicInput,
                    MicSample, Model, PlayerSample, RecorderSample, Recording, Selection,
                    SettingChange,
                },
                widgets::Dispatch,
                window::Window,
            },
            messages::BluetoothUIMessage,
        },
        utils::AppSettings,
    },
    adw::prelude::*,
    gtk::gio,
    std::{collections::HashMap, time::Duration},
};

#[test]
#[ignore = "needs a display"]
fn render_builds_widgets_and_sends_nothing() {
    let app = adw::Application::builder()
        .application_id("me.kavishdevar.librepods.smoketest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let dispatch = Dispatch::new(tx);
    let window = Window::new(&app, &dispatch);
    let mac = "AA:BB:CC:DD:EE:01".to_string();
    let ear = "AA:BB:CC:DD:EE:02".to_string();
    let devices = HashMap::from([
        (
            mac.clone(),
            DeviceData {
                name: "Pods".into(),
                type_: DeviceType::AirPods,
                information: None,
            },
        ),
        (
            ear.clone(),
            DeviceData {
                name: "Ear".into(),
                type_: DeviceType::Nothing,
                information: None,
            },
        ),
    ]);
    let mut model = Model::new(AppSettings::default(), devices);
    let step = |model: &mut Model, input: Input| {
        model.update(input);
        window.render(model);
    };
    window.render(&model);
    step(&mut model, Input::Connect(mac.clone()));
    step(
        &mut model,
        Input::Backend(BluetoothUIMessage::DeviceConnected(mac.clone())),
    );
    step(
        &mut model,
        Input::Snapshot(
            mac.clone(),
            Some(DeviceSnapshot::AirPods(AirPodsSnapshot::default())),
        ),
    );
    step(&mut model, battery(&mac, 70, BatteryStatus::NotCharging));
    step(&mut model, battery(&mac, 0, BatteryStatus::Disconnected));
    step(&mut model, Input::SetAllowOff(mac.clone(), true));
    for input in equalizer_steps(&mac) {
        step(&mut model, input);
    }
    for input in microphone_steps(&mac) {
        step(&mut model, input);
    }
    step(&mut model, Input::NameEdited(mac.clone(), String::new()));
    step(
        &mut model,
        Input::Setting(SettingChange::TrayTextMode(true)),
    );
    step(&mut model, Input::Select(Selection::Settings));
    step(&mut model, Input::Select(Selection::Device(ear.clone())));
    step(
        &mut model,
        Input::Backend(BluetoothUIMessage::DeviceConnected(ear.clone())),
    );
    step(
        &mut model,
        Input::Snapshot(ear.clone(), Some(DeviceSnapshot::Nothing)),
    );
    step(&mut model, Input::Select(Selection::Device(mac.clone())));
    step(
        &mut model,
        Input::Backend(BluetoothUIMessage::DeviceDisconnected(mac.clone())),
    );
    let mut queued = Vec::new();
    while let Ok(input) = rx.try_recv() {
        queued.push(format!("{input:?}"));
    }
    assert!(queued.is_empty(), "render sent inputs: {queued:?}");
}

/// Custom EQ on, bands moved, reset and off.
fn equalizer_steps(mac: &str) -> Vec<Input> {
    let eq = |input| Input::Equalizer(input);
    vec![
        eq(EqInput::SetEnabled(mac.to_string(), true)),
        eq(EqInput::SetBand(mac.to_string(), EqBand::Low, 80)),
        eq(EqInput::SetBand(mac.to_string(), EqBand::High, 0)),
        eq(EqInput::SendDue {
            mac: mac.to_string(),
            generation: 3,
        }),
        eq(EqInput::Reset(mac.to_string())),
        eq(EqInput::SetEnabled(mac.to_string(), false)),
    ]
}

/// An app capturing, then a microphone test through every phase, a failed
/// take, and the hi-res microphone turned off and on.
fn microphone_steps(mac: &str) -> Vec<Input> {
    let mic = |input| Input::Microphone(input);
    let tick = |sample| mic(MicInput::Tick(sample));
    let capture = |active, level, app: Option<&str>| MicSample {
        capture: Some(vec![(
            mac.to_string(),
            MicCapture {
                active,
                level,
                app: app.map(str::to_string),
            },
        )]),
        ..MicSample::default()
    };
    let recorder = |secs| MicSample {
        recorder: Some(RecorderSample {
            elapsed: Duration::from_secs(secs),
            finished: false,
        }),
        ..MicSample::default()
    };
    let player = |position, playing, error: Option<&str>| MicSample {
        player: Some(PlayerSample {
            position: Duration::from_millis(position),
            duration: Duration::from_secs(12),
            playing,
            error: error.map(str::to_string),
        }),
        ..MicSample::default()
    };
    vec![
        Input::WindowVisible(true),
        tick(capture(true, 0.4, Some("Zoom"))),
        tick(capture(true, 0.95, None)),
        Input::SetConversationAwareness(mac.to_string(), true),
        tick(capture(false, 0.0, None)),
        mic(MicInput::Record),
        mic(MicInput::MediaPaused(vec![
            "org.mpris.MediaPlayer2.test".into(),
        ])),
        tick(recorder(3)),
        tick(recorder(64)),
        mic(MicInput::Stop),
        mic(MicInput::Recorded {
            take: 1,
            result: Ok(Recording(vec![0; 4])),
        }),
        tick(player(0, false, None)),
        mic(MicInput::Play),
        tick(player(2500, true, None)),
        mic(MicInput::Skip { forward: true }),
        mic(MicInput::Seek(Duration::from_secs(11))),
        mic(MicInput::Pause),
        tick(player(11_000, false, Some("Could not play the recording"))),
        mic(MicInput::Record),
        mic(MicInput::Stop),
        mic(MicInput::Recorded {
            take: 2,
            result: Err("No sound was recorded".into()),
        }),
        mic(MicInput::Done),
        mic(MicInput::SetHiRes(mac.to_string(), false)),
        mic(MicInput::SetHiRes(mac.to_string(), true)),
    ]
}

/// A battery report with a charging left bud and the given case entry.
fn battery(mac: &str, case: u8, case_status: BatteryStatus) -> Input {
    Input::Backend(BluetoothUIMessage::AACPUIEvent(
        mac.to_string(),
        AACPEvent::BatteryInfo(vec![
            BatteryInfo {
                component: BatteryComponent::Left,
                level: 55,
                status: BatteryStatus::Charging,
            },
            BatteryInfo {
                component: BatteryComponent::Case,
                level: case,
                status: case_status,
            },
        ]),
    ))
}
