mod audio;
mod auto_switch;
mod bluetooth;
mod devices;
#[cfg(test)]
mod log_capture;
mod media_controller;
mod ui;
mod utils;

use {
    crate::{
        auto_switch::{AutoSwitchDeps, ConnectSource},
        bluetooth::{
            bluez_watch::{self, BluezWatch, TransportStates},
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
        message::{MatchRule, MessageType},
    },
    gtk::glib,
    ksni::{Handle, TrayMethods},
    std::{
        collections::{HashMap, HashSet},
        env,
        io::IsTerminal,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    },
    tokio::sync::{
        RwLock,
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    },
    tracing::{debug, error, info, warn},
};

type Managers = Arc<RwLock<HashMap<String, DeviceManagers>>>;

/// AirPods busy with another device accept the Bluetooth link but refuse the
/// AACP channel. When they later switch to this PC BlueZ raises no new connect
/// signal, so setup is retried this often for as long as the link stays up.
const SETUP_RETRY_INTERVAL: Duration = Duration::from_secs(3);

// Command line flags are independent switches by nature.
#[allow(clippy::struct_excessive_bools)]
#[derive(Parser)]
struct Args {
    #[arg(long, short = 'd', help = "Enable debug logging")]
    debug: bool,
    #[arg(
        long,
        help = "Run headless: no window and no tray, only the Bluetooth logic"
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
    let transports = TransportStates::default();
    let airpods_setup = AirPodsSetup {
        adapter: adapter.clone(),
        tray_handle: tray_handle.clone(),
        ui_tx: ui_tx.clone(),
        device_managers: device_managers.clone(),
        transports: transports.clone(),
        in_progress: Arc::default(),
    };
    set_up_connected_airpods(&adapter, &airpods_setup).await;
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
        airpods_setup,
        bluez: BluezWatch::new(transports),
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

async fn set_up_connected_airpods(adapter: &Adapter, airpods_setup: &AirPodsSetup) {
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
    ensure_device_registered(&device.address().to_string(), &name, DeviceType::AirPods);
    airpods_setup.spawn(device.address());
}

/// Sets up the AACP connection to connected AirPods, retrying while the
/// Bluetooth link stays up.
#[derive(Clone)]
struct AirPodsSetup {
    adapter: Adapter,
    tray_handle: Option<Handle<MyTray>>,
    ui_tx: UnboundedSender<BluetoothUIMessage>,
    device_managers: Managers,
    transports: TransportStates,
    /// AirPods with a setup running, so a second connect signal during the
    /// retries does not start another one.
    in_progress: Arc<Mutex<HashSet<Address>>>,
}

impl AirPodsSetup {
    fn spawn(&self, addr: Address) {
        if !self
            .in_progress
            .lock()
            .expect("in_progress is never poisoned: no code panics while holding it")
            .insert(addr)
        {
            debug!("Setup for AirPods {addr} is already running");
            return;
        }
        let setup = self.clone();
        tokio::spawn(async move {
            if let Some(airpods_device) = setup.connect(addr).await {
                register_airpods(
                    airpods_device,
                    addr.to_string(),
                    &setup.device_managers,
                    &setup.ui_tx,
                )
                .await;
            }
            setup
                .in_progress
                .lock()
                .expect("in_progress is never poisoned: no code panics while holding it")
                .remove(&addr);
        });
    }

    async fn connect(&self, addr: Address) -> Option<AirPodsDevice> {
        let mut attempt: u32 = 1;
        loop {
            let device = AirPodsDevice::new(
                addr,
                self.tray_handle.clone(),
                self.ui_tx.clone(),
                self.transports.clone(),
            )
            .await;
            match device {
                Ok(device) if attempt > 1 => {
                    info!(
                        "AirPods {addr} set up on attempt {attempt} (source: {})",
                        ConnectSource::SetupRetry
                    );
                    return Some(device);
                },
                Ok(device) => return Some(device),
                Err(e) if attempt == 1 => warn!(
                    "Could not set up AirPods {addr}: {e}. They may be in use by another \
                     device; retrying while they stay connected"
                ),
                Err(e) => debug!("Could not set up AirPods {addr} on attempt {attempt}: {e}"),
            }
            tokio::time::sleep(SETUP_RETRY_INTERVAL).await;
            if !self.is_connected(addr).await {
                info!("AirPods {addr} disconnected, stopping setup");
                return None;
            }
            attempt = attempt.saturating_add(1);
            debug!(
                "Retrying AirPods {addr} setup, attempt {attempt} (source: {})",
                ConnectSource::SetupRetry
            );
        }
    }

    async fn is_connected(&self, addr: Address) -> bool {
        match self.adapter.device(addr) {
            Ok(device) => device.is_connected().await.unwrap_or(false),
            Err(_) => false,
        }
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
    airpods_setup: AirPodsSetup,
    bluez: BluezWatch,
}

/// Process BlueZ signals until the system bus fails.
fn watch_connections(mut watcher: ConnectionWatcher) -> anyhow::Result<()> {
    let conn = Connection::new_system().context("cannot connect to the D-Bus system bus")?;
    watcher.bluez.seed_from_bus(&conn, Instant::now());
    // Every signal BlueZ sends: PropertiesChanged for devices and transports,
    // and the ObjectManager signals that announce transports coming and going.
    let rule = MatchRule::new()
        .with_type(MessageType::Signal)
        .with_sender("org.bluez");
    conn.add_match(rule, move |(): (), conn, msg| {
        watcher.on_signal(conn, msg);
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
    fn on_signal(&mut self, conn: &Connection, msg: &Message) {
        self.bluez.on_signal(msg, Instant::now(), |device_path| {
            bluez_watch::has_airpods_uuid(conn, device_path)
        });
        if msg.member().as_deref() == Some("PropertiesChanged") {
            self.on_properties_changed(conn, msg);
        }
    }

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
        let is_airpods = bluez_watch::airpods_uuid_in(&uuids);

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
            self.on_airpods_connected(addr, &addr_str, &name);
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

    fn on_airpods_connected(&self, addr: Address, addr_str: &str, name: &str) {
        info!("AirPods connected: {}, initializing", name);
        ensure_device_registered(addr_str, name, DeviceType::AirPods);
        self.airpods_setup.spawn(addr);
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

/// Log to stderr through tracing; libraries that use the `log` crate (bluer)
/// are bridged into the same subscriber. Colours only on a terminal: under
/// systemd stderr goes to the journal, where escape codes are noise.
fn init_tracing(debug: bool, le_debug: bool) {
    let rust_log = env::var("RUST_LOG").ok();
    let filter = tracing_subscriber::EnvFilter::builder().parse_lossy(log_filter(
        debug,
        le_debug,
        rust_log.as_deref(),
    ));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

/// The log filter directives: the defaults from the flags, then RUST_LOG.
/// RUST_LOG adds to the defaults instead of replacing them, and a directive
/// for the same target replaces the default one, so `RUST_LOG=librepods=debug`
/// turns on the app's debug lines and leaves the rest of the defaults alone.
fn log_filter(debug: bool, le_debug: bool, rust_log: Option<&str>) -> String {
    let level = if debug { "debug" } else { "info" };
    let le_level = if le_debug { "debug" } else { "info" };
    let defaults = format!("{level},zbus=warn,librepods::bluetooth::le={le_level}");
    match rust_log.map(str::trim).filter(|s| !s.is_empty()) {
        Some(rust_log) => format!("{defaults},{rust_log}"),
        None => defaults,
    }
}

#[cfg(test)]
mod tests {
    use {super::log_filter, tracing_subscriber::EnvFilter};

    #[test]
    fn log_filter_defaults_follow_the_flags() {
        assert_eq!(
            log_filter(false, false, None),
            "info,zbus=warn,librepods::bluetooth::le=info"
        );
        assert_eq!(
            log_filter(true, true, Some("  ")),
            "debug,zbus=warn,librepods::bluetooth::le=debug"
        );
    }

    #[test]
    fn rust_log_is_added_after_the_defaults() {
        let filter = log_filter(false, false, Some("librepods=debug"));
        assert_eq!(
            filter,
            "info,zbus=warn,librepods::bluetooth::le=info,librepods=debug"
        );
        assert!(EnvFilter::builder().parse(&filter).is_ok());
    }
}
