mod audio;
mod auto_switch;
mod bluetooth;
mod devices;
mod media_controller;
mod ui;
mod utils;

use {
    crate::{
        auto_switch::AutoSwitchDeps,
        bluetooth::{
            discovery::{find_connected_airpods, find_other_managed_devices},
            le::start_le_monitor,
            managers::DeviceManagers,
        },
        devices::{
            airpods::AirPodsDevice,
            enums::{DeviceData, DeviceType},
            nothing::NothingDevice,
        },
        ui::{messages::BluetoothUIMessage, tray::MyTray},
        utils::{DevicesStore, ensure_device_registered},
    },
    anyhow::Context as _,
    bluer::{Adapter, Address},
    clap::Parser,
    dbus::{
        Message,
        arg::{RefArg, Variant},
        blocking::{Connection, stdintf::org_freedesktop_dbus::Properties},
        message::MatchRule,
    },
    gtk::glib,
    ksni::{Handle, TrayMethods},
    std::{collections::HashMap, env, sync::Arc, time::Duration},
    tokio::sync::{
        RwLock,
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    },
    tracing::{error, info, warn},
};

const AIRPODS_UUID: &str = "74ec2172-0bad-4d01-8f77-997b2be0722a";

type Managers = Arc<RwLock<HashMap<String, DeviceManagers>>>;

// Command line flags are independent switches by nature.
#[allow(clippy::struct_excessive_bools)]
#[derive(Parser)]
struct Args {
    #[arg(long, short = 'd', help = "Enable debug logging")]
    debug: bool,
    #[arg(
        long,
        help = "Disable system tray, useful if your environment doesn't support AppIndicator or StatusNotifier"
    )]
    no_tray: bool,
    #[arg(long, help = "Start the application minimized to tray")]
    start_minimized: bool,
    #[arg(
        long,
        help = "Enable Bluetooth LE debug logging. Only use when absolutely necessary; this produces a lot of logs."
    )]
    le_debug: bool,
    #[arg(long, short = 'v', help = "Show application version and exit")]
    version: bool,
}

fn main() -> glib::ExitCode {
    let args = Args::parse();

    if args.version {
        print_version();
        return glib::ExitCode::SUCCESS;
    }

    init_tracing(args.debug, args.le_debug);

    if !args.no_tray && ui::gtk::present_running_instance() {
        info!("LibrePods is already running; showing its window");
        return glib::ExitCode::SUCCESS;
    }

    let (ui_tx, ui_rx) = unbounded_channel::<BluetoothUIMessage>();
    let device_managers: Managers = Arc::new(RwLock::new(HashMap::new()));
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            error!("LibrePods could not start the async runtime: {e}");
            return glib::ExitCode::FAILURE;
        },
    };

    if args.no_tray {
        info!("Running in headless mode (no GUI)");
        run_backend(&runtime, ui_tx, device_managers, args.no_tray);
        return glib::ExitCode::SUCCESS;
    }

    // The UI spawns device work on the backend runtime through this handle.
    let backend = runtime.handle().clone();
    let backend_managers = device_managers.clone();
    let no_tray = args.no_tray;
    std::thread::spawn(move || run_backend(&runtime, ui_tx, backend_managers, no_tray));
    ui::gtk::run(
        ui_rx,
        device_managers,
        backend,
        ui::gtk::Options {
            start_minimized: args.start_minimized,
            tray: true,
        },
    )
}

// --version is the one output meant for stdout rather than the log.
#[allow(clippy::print_stdout)]
fn print_version() {
    println!(
        "You are running LibrePods version {}",
        env!("CARGO_PKG_VERSION")
    );
}

/// Run the Bluetooth side on its own tokio runtime until the process exits.
/// A failure to start ends the process, UI included.
fn run_backend(
    runtime: &tokio::runtime::Runtime,
    ui_tx: UnboundedSender<BluetoothUIMessage>,
    device_managers: Managers,
    no_tray: bool,
) {
    if let Err(e) = runtime.block_on(async_main(ui_tx, device_managers, no_tray)) {
        error!("LibrePods could not start: {e:#}");
        std::process::exit(1);
    }
}

async fn async_main(
    ui_tx: UnboundedSender<BluetoothUIMessage>,
    device_managers: Managers,
    no_tray: bool,
) -> anyhow::Result<()> {
    let devices_list = DevicesStore::default_location().load().unwrap_or_else(|e| {
        error!("Failed to load devices: {}", e);
        HashMap::new()
    });
    // Only non-AirPods devices; AirPods are recognised by their UUID.
    let managed_devices_mac: Vec<String> = devices_list
        .iter()
        .filter(|(_, d)| d.type_ == DeviceType::Nothing)
        .map(|(mac, _)| mac.clone())
        .collect();

    // Held for the whole run: when the last sender is dropped the shutdown
    // handler reads it as a request to quit.
    let (shutdown_tx, shutdown_rx) = unbounded_channel::<()>();
    spawn_shutdown_handler(device_managers.clone(), shutdown_rx);

    let tray_handle = if no_tray {
        None
    } else {
        spawn_tray(&ui_tx, &shutdown_tx).await
    };

    let adapter = bluetooth_adapter().await?;

    tokio::spawn(auto_switch::run(adapter.clone(), AutoSwitchDeps::system()));

    let le_tray = tray_handle.clone();
    let le_ui_tx = ui_tx.clone();
    tokio::spawn(async move {
        info!("Starting LE monitor...");
        if let Err(e) = start_le_monitor(le_tray, le_ui_tx).await {
            error!("LE monitor error: {}", e);
        }
    });

    info!("Listening for new connections.");
    info!("Checking for connected devices...");
    set_up_connected_airpods(&adapter, tray_handle.as_ref(), &ui_tx, &device_managers).await;
    set_up_connected_managed_devices(
        &adapter,
        &managed_devices_mac,
        &devices_list,
        &ui_tx,
        &device_managers,
    )
    .await;

    let watcher = ConnectionWatcher {
        ui_tx,
        tray_handle,
        managed_devices_mac,
        devices_list,
        device_managers,
    };
    let result = watch_connections(watcher);
    drop(shutdown_tx);
    result
}

async fn spawn_tray(
    ui_tx: &UnboundedSender<BluetoothUIMessage>,
    shutdown_tx: &UnboundedSender<()>,
) -> Option<Handle<MyTray>> {
    let tray = MyTray {
        conversation_detect_enabled: None,
        battery_headphone: None,
        battery_headphone_status: None,
        battery_l: None,
        battery_l_status: None,
        battery_r: None,
        battery_r_status: None,
        battery_c: None,
        battery_c_status: None,
        connected: false,
        listening_mode: None,
        allow_off_option: None,
        command_tx: None,
        ui_tx: Some(ui_tx.clone()),
        shutdown_tx: Some(shutdown_tx.clone()),
    };
    // LibrePods can be started by the session manager before the desktop's
    // StatusNotifierWatcher is ready. Assume it will appear so the tray
    // registers when it does, instead of failing for the whole session.
    match tray.assume_sni_available(true).spawn().await {
        Ok(handle) => Some(handle),
        Err(e) => {
            warn!(
                "Failed to start system tray ({e}); continuing without tray. \
                 Your environment may lack a StatusNotifier/AppIndicator watcher."
            );
            None
        },
    }
}

/// The default adapter, powered on. The errors say what to check.
async fn bluetooth_adapter() -> anyhow::Result<Adapter> {
    let session = bluer::Session::new().await.context(
        "cannot talk to BlueZ over D-Bus. Is the bluetooth service running? \
         Check with `systemctl status bluetooth`",
    )?;
    let adapter = session.default_adapter().await.context(
        "no Bluetooth adapter available. Make sure an adapter is present and not \
         blocked - see `rfkill list bluetooth`",
    )?;
    adapter.set_powered(true).await.context(
        "cannot power on the Bluetooth adapter. It is likely soft-blocked, try \
         `rfkill unblock bluetooth`",
    )?;
    Ok(adapter)
}

async fn set_up_connected_airpods(
    adapter: &Adapter,
    tray_handle: Option<&Handle<MyTray>>,
    ui_tx: &UnboundedSender<BluetoothUIMessage>,
    device_managers: &Managers,
) {
    let Ok(device) = find_connected_airpods(adapter).await else {
        info!("No connected AirPods found.");
        return;
    };
    // The device can vanish between the scan and this call; that is not a
    // reason to exit.
    let name = device
        .name()
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "Unknown".to_string());
    info!("Found connected AirPods: {}, initializing.", name);
    let addr_str = device.address().to_string();
    ensure_device_registered(&addr_str, &name, DeviceType::AirPods);
    match AirPodsDevice::new(device.address(), tray_handle.cloned(), ui_tx.clone()).await {
        Ok(airpods_device) => {
            register_airpods(airpods_device, addr_str, device_managers, ui_tx).await;
        },
        Err(e) => error!("Could not set up AirPods {}: {}", device.address(), e),
    }
}

async fn set_up_connected_managed_devices(
    adapter: &Adapter,
    managed_devices_mac: &[String],
    devices_list: &HashMap<String, DeviceData>,
    ui_tx: &UnboundedSender<BluetoothUIMessage>,
    device_managers: &Managers,
) {
    let devices = match find_other_managed_devices(adapter, managed_devices_mac.to_vec()).await {
        Ok(devices) => devices,
        // "None found" is Ok(vec![]); an Err is a real lookup failure.
        Err(e) => {
            error!("Error finding other managed devices: {}", e);
            return;
        },
    };
    for device in devices {
        let addr_str = device.address().to_string();
        info!(
            "Found connected managed device: {}, initializing.",
            addr_str
        );
        match devices_list.get(&addr_str).map(|d| &d.type_) {
            Some(DeviceType::Nothing) => spawn_nothing_setup(
                device.address(),
                addr_str,
                ui_tx.clone(),
                device_managers.clone(),
            ),
            Some(_) => {},
            None => warn!("Managed device {} is not in the devices list", addr_str),
        }
    }
}

/// Connect a Nothing device and make it reachable from the UI.
fn spawn_nothing_setup(
    addr: Address,
    addr_str: String,
    ui_tx: UnboundedSender<BluetoothUIMessage>,
    device_managers: Managers,
) {
    tokio::spawn(async move {
        // Connect before taking the lock: view() reads it on every frame and
        // would freeze for the whole connect.
        let dev = match NothingDevice::new(addr, ui_tx.clone()).await {
            Ok(dev) => dev,
            Err(e) => {
                error!("Could not set up device {}: {}", addr_str, e);
                return;
            },
        };
        let mut managers = device_managers.write().await;
        let dev_managers = DeviceManagers::with_att(dev.att_manager.clone());
        managers
            .entry(addr_str.clone())
            .or_insert(dev_managers)
            .set_att(dev.att_manager);
        drop(managers);
        if let Err(e) = ui_tx.send(BluetoothUIMessage::DeviceConnected(addr_str)) {
            warn!("Failed to send DeviceConnected UI message: {:?}", e);
        }
    });
}

/// Reacts to BlueZ reporting a device connecting or disconnecting.
struct ConnectionWatcher {
    ui_tx: UnboundedSender<BluetoothUIMessage>,
    tray_handle: Option<Handle<MyTray>>,
    managed_devices_mac: Vec<String>,
    devices_list: HashMap<String, DeviceData>,
    device_managers: Managers,
}

/// Process BlueZ PropertiesChanged signals until the system bus fails.
fn watch_connections(watcher: ConnectionWatcher) -> anyhow::Result<()> {
    let conn = Connection::new_system().context("cannot connect to the D-Bus system bus")?;
    let rule = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
    conn.add_match(rule, move |(): (), conn, msg| {
        watcher.on_properties_changed(conn, msg);
        true
    })
    .context("cannot watch BlueZ device changes")?;

    info!("Listening for Bluetooth connections via D-Bus...");
    loop {
        conn.process(Duration::from_millis(1000))
            .context("lost the D-Bus system bus")?;
    }
}

impl ConnectionWatcher {
    fn on_properties_changed(&self, conn: &Connection, msg: &Message) {
        let Some(path) = msg.path() else {
            return;
        };
        if !path.contains("/org/bluez/hci") || !path.contains("/dev_") {
            return;
        }
        let Ok((iface, changed, _)) =
            msg.read3::<String, HashMap<String, Variant<Box<dyn RefArg>>>, Vec<String>>()
        else {
            return;
        };
        if iface != "org.bluez.Device1" {
            return;
        }
        let Some(is_connected) = changed.get("Connected").and_then(|v| v.0.as_u64()) else {
            return;
        };
        let proxy = conn.with_proxy("org.bluez", path, Duration::from_millis(5000));
        let Ok(uuids) = proxy.get::<Vec<String>>("org.bluez.Device1", "UUIDs") else {
            return;
        };
        // BlueZ reports UUIDs in lowercase, but nothing guarantees it.
        let is_airpods = uuids.iter().any(|u| u.eq_ignore_ascii_case(AIRPODS_UUID));

        let Ok(addr_str) = proxy.get::<String>("org.bluez.Device1", "Address") else {
            return;
        };
        let Ok(addr) = addr_str.parse::<Address>() else {
            return;
        };
        if is_connected == 0 {
            self.on_disconnected(addr_str, is_airpods);
        } else if self.managed_devices_mac.contains(&addr_str) {
            self.on_managed_connected(addr, addr_str);
        } else if is_airpods {
            let name = proxy
                .get::<String>("org.bluez.Device1", "Name")
                .unwrap_or_else(|_| "Unknown".to_string());
            self.on_airpods_connected(addr, addr_str, &name);
        }
    }

    fn on_disconnected(&self, addr_str: String, is_airpods: bool) {
        if let Err(e) = self
            .ui_tx
            .send(BluetoothUIMessage::DeviceDisconnected(addr_str))
        {
            warn!("Failed to send DeviceConnected UI message: {:?}", e);
        }
        // AirPodsDevice::new sets `connected` on connect; clear it so the
        // tray's connect item comes back, and drop what the AirPods reported
        // so the tray does not show it as current.
        if is_airpods && let Some(handle) = self.tray_handle.clone() {
            tokio::spawn(async move {
                handle
                    .update(|tray: &mut MyTray| {
                        tray.connected = false;
                        tray.battery_headphone = None;
                        tray.battery_headphone_status = None;
                        tray.battery_l = None;
                        tray.battery_l_status = None;
                        tray.battery_r = None;
                        tray.battery_r_status = None;
                        tray.battery_c = None;
                        tray.battery_c_status = None;
                        tray.listening_mode = None;
                        tray.conversation_detect_enabled = None;
                    })
                    .await;
            });
        }
    }

    fn on_managed_connected(&self, addr: Address, addr_str: String) {
        info!("Managed device connected: {}, initializing", addr_str);
        match self.devices_list.get(&addr_str).map(|d| &d.type_) {
            Some(DeviceType::Nothing) => spawn_nothing_setup(
                addr,
                addr_str,
                self.ui_tx.clone(),
                self.device_managers.clone(),
            ),
            Some(_) => {},
            None => warn!("Managed device {} is not in the devices list", addr_str),
        }
    }

    fn on_airpods_connected(&self, addr: Address, addr_str: String, name: &str) {
        info!("AirPods connected: {}, initializing", name);
        ensure_device_registered(&addr_str, name, DeviceType::AirPods);
        let tray_handle = self.tray_handle.clone();
        let ui_tx = self.ui_tx.clone();
        let device_managers = self.device_managers.clone();
        tokio::spawn(async move {
            let airpods_device = match AirPodsDevice::new(addr, tray_handle, ui_tx.clone()).await {
                Ok(device) => device,
                Err(e) => {
                    error!("Could not set up AirPods {}: {}", addr_str, e);
                    return;
                },
            };
            register_airpods(airpods_device, addr_str, &device_managers, &ui_tx).await;
        });
    }
}

/// Make freshly connected AirPods reachable from the UI. Used by both the
/// startup scan and the connect signal, so a reconnect is set up the same way.
async fn register_airpods(
    airpods_device: AirPodsDevice,
    addr_str: String,
    device_managers: &RwLock<HashMap<String, DeviceManagers>>,
    ui_tx: &UnboundedSender<BluetoothUIMessage>,
) {
    let aacp_manager = airpods_device.aacp_manager;
    let mut managers = device_managers.write().await;
    managers
        .entry(addr_str.clone())
        .or_insert_with(|| DeviceManagers::with_aacp(aacp_manager.clone()))
        .set_aacp(aacp_manager.clone());
    drop(managers);
    if let Err(e) = ui_tx.send(BluetoothUIMessage::DeviceConnected(addr_str)) {
        warn!("Failed to send DeviceConnected UI message: {:?}", e);
    }
    // LIBREPODS_HIRES_MIC=1: enable the hi-res mic feature headlessly.
    if env::var("LIBREPODS_HIRES_MIC").is_ok() {
        tokio::spawn(async move {
            aacp_manager.set_hires_mic_enabled(true).await;
        });
    }
}

fn spawn_shutdown_handler(device_managers: Managers, mut shutdown_rx: UnboundedReceiver<()>) {
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        let mut sigint = signal(SignalKind::interrupt()).ok();
        let sigterm = async {
            match sigterm.as_mut() {
                Some(s) => s.recv().await,
                None => std::future::pending().await,
            }
        };
        let sigint = async {
            match sigint.as_mut() {
                Some(s) => s.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = shutdown_rx.recv() => {}
            _ = sigterm => {}
            _ = sigint => {}
        }

        info!("Shutting down: tearing down hi-res mic streams...");
        let cleanup = async {
            let managers = device_managers.read().await;
            for dm in managers.values() {
                if let Some(aacp) = dm.get_aacp() {
                    aacp.disarm_hires_mic().await;
                }
            }
        };
        if tokio::time::timeout(Duration::from_secs(3), cleanup)
            .await
            .is_err()
        {
            warn!("Shutdown cleanup timed out; exiting anyway");
        }
        std::process::exit(0);
    });
}

/// Log to stderr through tracing. RUST_LOG overrides the defaults; libraries that
/// use the `log` crate (bluer, iced, wgpu) are bridged into the same subscriber.
fn init_tracing(debug: bool, le_debug: bool) {
    let level = if debug { "debug" } else { "info" };
    let le_level = if le_debug { "debug" } else { "info" };
    let default_filter = format!(
        "{level},zbus=warn,winit=warn,iced_wgpu=warn,wgpu_hal=warn,wgpu_core=warn,\
         cosmic_text=warn,naga=warn,iced_winit=warn,librepods::bluetooth::le={le_level}"
    );
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
