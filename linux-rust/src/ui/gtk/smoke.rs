//! Builds the real widgets and renders a sequence of model states. Needs a
//! display, so it is ignored by default; run it with
//! `G_DEBUG=fatal-criticals cargo test smoke -- --ignored --test-threads=1`.
//! It fails when GTK logs a critical or when rendering sends an input, which
//! means a handler was not guarded.

use {
    crate::{
        bluetooth::{
            aacp::{
                AACPEvent, BatteryComponent, BatteryInfo, BatteryStatus,
                ControlCommandIdentifiers as Id, ControlCommandStatus,
            },
            settings::{CallControls, ClickHoldAction, CycleMode},
        },
        devices::enums::{DeviceData, DeviceType},
        ui::{
            gtk::{
                controls::{Bud, ControlChange},
                model::{AirPodsSnapshot, DeviceSnapshot, Input, Model, Selection, SettingChange},
                widgets::Dispatch,
                window::Window,
            },
            messages::BluetoothUIMessage,
        },
        utils::AppSettings,
    },
    adw::prelude::*,
    gtk::gio,
    std::collections::HashMap,
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
    render_settings_sections(&mut model, &mac, &step);
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

/// Report every setting of the AirPods settings sections, some with values
/// that do not decode, then change a few of them.
fn render_settings_sections(model: &mut Model, mac: &str, step: &impl Fn(&mut Model, Input)) {
    for (identifier, value) in [
        (Id::ClickHoldMode, &[0x05, 0x01][..]),
        (Id::ListeningModeConfigs, &[0x06]),
        (Id::CallManagementConfig, &[0x00, 0x03]),
        (Id::MicMode, &[0x09]),
        (Id::SleepDetectionConfig, &[0x01]),
        (Id::DoubleClickInterval, &[0x01]),
        (Id::ClickHoldInterval, &[0x00]),
        (Id::OneBudAncMode, &[0x02]),
        (Id::ChimeVolume, &[0x40, 0x50]),
        (Id::VolumeSwipeMode, &[0x01]),
        (Id::VolumeSwipeInterval, &[0x02]),
        (Id::AutoAncStrength, &[0xFF]),
    ] {
        step(
            model,
            Input::Backend(BluetoothUIMessage::AACPUIEvent(
                mac.to_string(),
                AACPEvent::ControlCommand(ControlCommandStatus {
                    identifier,
                    value: value.to_vec(),
                }),
            )),
        );
    }
    for change in [
        ControlChange::LongPress(Bud::Left, ClickHoldAction::Siri),
        ControlChange::CycleMode(CycleMode::Off, true),
        ControlChange::MuteControl(CallControls::PressTwiceToMute),
        ControlChange::ToneVolume(35),
        ControlChange::AdaptiveNoise(80),
        ControlChange::VolumeSwipe(false),
    ] {
        step(model, Input::AirPodsControl(mac.to_string(), change));
    }
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
