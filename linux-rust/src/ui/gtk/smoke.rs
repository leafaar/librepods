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
