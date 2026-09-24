//! Startup, the input loop and the effects: everything that touches the
//! outside world on behalf of the model.

use {
    crate::{
        bluetooth::{aacp::AACPManager, att::ATTHandles, managers::DeviceManagers},
        devices::enums::DeviceData,
        ui::{
            gtk::{
                model::{AirPodsSnapshot, DeviceSnapshot, Effect, Input, Model},
                theme::ThemePreference,
                widgets::Dispatch,
                window::{APP_ID, Window},
            },
            messages::BluetoothUIMessage,
        },
        utils::{AppSettings, get_devices_path, update_devices_file},
    },
    adw::prelude::*,
    gtk::{gio, glib},
    std::{
        cell::{Cell, RefCell},
        collections::HashMap,
        rc::Rc,
        sync::{Arc, mpsc},
        time::Duration,
    },
    tokio::{
        runtime::Handle,
        sync::{
            RwLock,
            mpsc::{UnboundedReceiver, unbounded_channel},
        },
    },
    tracing::{debug, error, warn},
};

mod equalizer;
mod mic;

/// How long to wait for DeviceConnected after the Bluetooth connect succeeded.
const CONNECT_SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How the window behaves at startup and on close.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Do not show the window at startup.
    pub start_minimized: bool,
    /// A tray icon can bring the window back, so closing it hides it and the
    /// app keeps running. Without a tray, closing the window quits.
    pub tray: bool,
}

/// Run the GTK UI on this thread until the application quits.
///
/// `backend` is the runtime the Bluetooth backend runs on: device commands
/// and connects are spawned there, never run on the GTK thread. A second
/// launch presents the window of the running instance (GApplication single
/// instance) and returns.
pub fn run(
    ui_rx: UnboundedReceiver<BluetoothUIMessage>,
    device_managers: Arc<RwLock<HashMap<String, DeviceManagers>>>,
    backend: Handle,
    options: Options,
) -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    // Read before the main loop starts, so the first frame is complete and
    // the loop never waits on the disk for them.
    let startup = RefCell::new(Some(Startup {
        ui_rx,
        device_managers,
        backend,
        settings: AppSettings::load(),
        devices: read_devices(),
    }));
    let controller: RefCell<Option<Rc<Controller>>> = RefCell::new(None);
    app.connect_activate(move |app| {
        if let Some(controller) = controller.borrow().as_ref() {
            controller.window.present();
            return;
        }
        let Some(startup) = startup.borrow_mut().take() else {
            return;
        };
        let started = Controller::start(app, startup, options);
        if !options.start_minimized {
            started.window.present();
        }
        *controller.borrow_mut() = Some(started);
    });
    // GTK must not parse the command line: clap already did, and GTK rejects
    // the app's own flags.
    app.run_with_args::<&str>(&[])
}

/// What `run` hands to the first activation.
struct Startup {
    ui_rx: UnboundedReceiver<BluetoothUIMessage>,
    device_managers: Arc<RwLock<HashMap<String, DeviceManagers>>>,
    backend: Handle,
    settings: AppSettings,
    devices: HashMap<String, DeviceData>,
}

/// Owns the model and the window, applies inputs and runs effects.
struct Controller {
    model: RefCell<Model>,
    window: Window,
    dispatch: Dispatch,
    backend: Handle,
    device_managers: Arc<RwLock<HashMap<String, DeviceManagers>>>,
    settings_writer: mpsc::Sender<AppSettings>,
    /// A devices.json read is running; a second request waits for it.
    devices_loading: Cell<bool>,
    devices_reload: Cell<bool>,

    // Hi-res microphone and equalizer sections.
    mic: RefCell<mic::MicDevices>,
    eq_sender: equalizer::EqSender,
}

impl Controller {
    fn start(app: &adw::Application, startup: Startup, options: Options) -> Rc<Self> {
        let (tx, mut rx) = unbounded_channel::<Input>();
        let dispatch = Dispatch::new(tx);
        let window = Window::new(app, &dispatch);
        // With a tray the window only hides, and the application keeps
        // running because the window still belongs to it.
        window.root().set_hide_on_close(options.tray);
        let model = Model::new(startup.settings, startup.devices);
        apply_theme(model.theme());
        let eq_sender = equalizer::spawn_eq_sender(
            &startup.backend,
            Arc::clone(&startup.device_managers),
            dispatch.clone(),
        );
        let controller = Rc::new(Controller {
            model: RefCell::new(model),
            window,
            dispatch: dispatch.clone(),
            backend: startup.backend,
            device_managers: startup.device_managers,
            settings_writer: spawn_settings_writer(),
            devices_loading: Cell::new(false),
            devices_reload: Cell::new(false),
            mic: RefCell::new(mic::MicDevices::default()),
            eq_sender,
        });
        controller.window.render(&controller.model.borrow());
        {
            let dispatch = dispatch.clone();
            controller
                .window
                .root()
                .connect_visible_notify(move |window| {
                    dispatch.send(Input::WindowVisible(window.is_visible()));
                });
        }

        let context = glib::MainContext::default();
        let mut ui_rx = startup.ui_rx;
        context.spawn_local(async move {
            while let Some(message) = ui_rx.recv().await {
                dispatch.send(Input::Backend(message));
            }
            error!("UI message channel closed, no more device updates");
        });
        let this = Rc::clone(&controller);
        context.spawn_local(async move {
            while let Some(input) = rx.recv().await {
                // Apply everything already queued, then draw once.
                let mut effects = this.apply(input);
                while let Ok(input) = rx.try_recv() {
                    effects.extend(this.apply(input));
                }
                this.window.render(&this.model.borrow());
                for effect in effects {
                    this.run(effect);
                }
                this.sync_mic_tick();
            }
        });
        controller
    }

    fn apply(&self, input: Input) -> Vec<Effect> {
        debug!("UI input: {:?}", input);
        self.model.borrow_mut().update(input)
    }

    fn run(self: &Rc<Self>, effect: Effect) {
        match effect {
            Effect::LoadDevices => self.load_devices(),
            Effect::Snapshot(mac) => {
                let managers = Arc::clone(&self.device_managers);
                let key = mac.clone();
                let task = self
                    .backend
                    .spawn(async move { snapshot(&managers, &key).await });
                let dispatch = self.dispatch.clone();
                glib::MainContext::default().spawn_local(async move {
                    let snapshot = task.await.unwrap_or_else(|e| {
                        error!("Reading the state of {} failed: {}", mac, e);
                        None
                    });
                    dispatch.send(Input::Snapshot(mac, snapshot));
                });
            },
            Effect::Connect {
                mac,
                address,
                attempt,
            } => {
                let task = self
                    .backend
                    .spawn(crate::auto_switch::connect_airpods(address));
                let dispatch = self.dispatch.clone();
                glib::MainContext::default().spawn_local(async move {
                    let result = match task.await {
                        Ok(result) => result.map_err(|e| e.to_string()),
                        Err(e) => Err(format!("Could not connect: {e}")),
                    };
                    dispatch.send(Input::ConnectFinished {
                        mac,
                        attempt,
                        result,
                    });
                });
            },
            Effect::SetupTimeout { mac, attempt } => {
                let dispatch = self.dispatch.clone();
                glib::MainContext::default().spawn_local(async move {
                    glib::timeout_future(CONNECT_SETUP_TIMEOUT).await;
                    dispatch.send(Input::ConnectSetupTimedOut { mac, attempt });
                });
            },
            Effect::SendControl {
                mac,
                identifier,
                value,
            } => self.with_aacp(mac, move |aacp, dispatch| async move {
                if let Err(e) = aacp.send_control_command(identifier, &value).await {
                    warn!("Failed to send {:?}: {}", identifier, e);
                    dispatch.send(Input::Toast(format!("Could not change the setting: {e}")));
                }
            }),
            Effect::SetConversationDetection { mac, enabled } => {
                self.with_aacp(mac, move |aacp, _| async move {
                    aacp.set_conversation_detection(enabled).await;
                });
            },
            Effect::Rename { mac, name } => self.rename(mac, name),
            Effect::SetNothingAnc { mac, mode } => self.set_nothing_anc(mac, mode),
            Effect::SaveSettings => {
                let settings = self.model.borrow().settings().clone();
                if self.settings_writer.send(settings).is_err() {
                    error!("The settings writer stopped, settings are not saved");
                }
            },
            Effect::ApplyTheme(theme) => apply_theme(theme),
            Effect::PresentWindow => self.window.present(),
            Effect::Toast(text) => self.window.toast(&text),

            Effect::Microphone(effect) => self.run_mic(effect),
            Effect::Equalizer(effect) => self.run_equalizer(effect),
        }
    }

    /// Read devices.json off the main thread. Requests while a read runs
    /// are folded into one more read after it.
    fn load_devices(self: &Rc<Self>) {
        if self.devices_loading.replace(true) {
            self.devices_reload.set(true);
            return;
        }
        let this = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            loop {
                if let Ok(devices) = gio::spawn_blocking(read_devices).await {
                    this.dispatch.send(Input::DevicesLoaded(devices));
                } else {
                    error!("Reading the devices file panicked");
                }
                if !this.devices_reload.replace(false) {
                    break;
                }
            }
            this.devices_loading.set(false);
        });
    }

    /// Run `work` on the backend runtime with the AACP manager of `mac`.
    fn with_aacp<F, Fut>(&self, mac: String, work: F)
    where
        F: FnOnce(Arc<AACPManager>, Dispatch) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let managers = Arc::clone(&self.device_managers);
        let dispatch = self.dispatch.clone();
        self.backend.spawn(async move {
            let aacp = managers
                .read()
                .await
                .get(&mac)
                .and_then(DeviceManagers::get_aacp);
            let Some(aacp) = aacp else {
                warn!("No AACP manager for {}", mac);
                dispatch.send(Input::Toast("The AirPods are not connected".to_string()));
                return;
            };
            work(aacp, dispatch).await;
        });
    }

    /// Send the new name to the AirPods, save it to devices.json and reload
    /// the file for the sidebar.
    fn rename(self: &Rc<Self>, mac: String, name: String) {
        let managers = Arc::clone(&self.device_managers);
        let dispatch = self.dispatch.clone();
        let task = self.backend.spawn(async move {
            let aacp = managers
                .read()
                .await
                .get(&mac)
                .and_then(DeviceManagers::get_aacp);
            let Some(aacp) = aacp else {
                dispatch.send(Input::Toast("The AirPods are not connected".to_string()));
                return;
            };
            if let Err(e) = aacp.send_rename_packet(&name).await {
                error!("Failed to send rename packet: {}", e);
                dispatch.send(Input::Toast(format!("Could not rename the AirPods: {e}")));
                return;
            }
            let saved = tokio::task::spawn_blocking(move || {
                update_devices_file(|devices| {
                    if let Some(device) = devices.get_mut(&mac) {
                        device.name = name;
                    }
                })
            })
            .await;
            match saved {
                Ok(Ok(())) => {},
                Ok(Err(e)) => error!("Failed to save the new name: {}", e),
                Err(e) => error!("Saving the new name failed: {}", e),
            }
        });
        let this = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let _ = task.await;
            this.load_devices();
        });
    }

    fn set_nothing_anc(&self, mac: String, mode: u8) {
        let managers = Arc::clone(&self.device_managers);
        let dispatch = self.dispatch.clone();
        self.backend.spawn(async move {
            let att = managers
                .read()
                .await
                .get(&mac)
                .and_then(DeviceManagers::get_att);
            let Some(att) = att else {
                error!("Cannot set noise control mode on {}, no ATT manager", mac);
                return;
            };
            let packet = [
                0x55, 0x60, 0x01, 0x0F, 0xF0, 0x03, 0x00, 0x00, 0x01, mode, 0x00, 0x00, 0x00,
            ];
            if let Err(e) = att.write(ATTHandles::NothingEverything, &packet).await {
                error!("Failed to set noise control mode for {}: {}", mac, e);
                dispatch.send(Input::Toast(format!("Could not change noise control: {e}")));
            }
        });
    }
}

/// The state a device's manager holds, read on the backend runtime.
async fn snapshot(
    managers: &RwLock<HashMap<String, DeviceManagers>>,
    mac: &str,
) -> Option<DeviceSnapshot> {
    let (aacp, att) = {
        let managers = managers.read().await;
        let device = managers.get(mac)?;
        (device.get_aacp(), device.get_att())
    };
    if let Some(aacp) = aacp {
        let state = aacp.state.lock().await;
        return Some(DeviceSnapshot::AirPods(AirPodsSnapshot {
            battery: state.battery_info.clone(),
            controls: state.control_command_status_list.clone(),
            custom_eq: state.custom_eq,
        }));
    }
    att.map(|_| DeviceSnapshot::Nothing)
}

fn apply_theme(theme: ThemePreference) {
    let scheme = match theme {
        ThemePreference::System => adw::ColorScheme::Default,
        ThemePreference::Light => adw::ColorScheme::ForceLight,
        ThemePreference::Dark => adw::ColorScheme::ForceDark,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

/// A thread that writes settings in the order they were changed, skipping to
/// the newest when several are queued.
fn spawn_settings_writer() -> mpsc::Sender<AppSettings> {
    let (tx, rx) = mpsc::channel::<AppSettings>();
    let spawned = std::thread::Builder::new()
        .name("settings-writer".to_string())
        .spawn(move || {
            while let Ok(mut settings) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    settings = newer;
                }
                settings.save();
            }
        });
    if let Err(e) = spawned {
        error!("Could not start the settings writer: {}", e);
    }
    tx
}

/// Read devices.json. A missing or unreadable file gives an empty list.
fn read_devices() -> HashMap<String, DeviceData> {
    let json = match std::fs::read_to_string(get_devices_path()) {
        Ok(json) => json,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                error!("Failed to read devices file: {}", e);
            }
            return HashMap::new();
        },
    };
    serde_json::from_str(&json).unwrap_or_else(|e| {
        error!("Devices file is not valid: {}", e);
        HashMap::new()
    })
}
