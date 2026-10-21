//! BlueZ's view of the AirPods, for the log: the A2DP transport
//! (org.bluez.MediaTransport1) going idle, pending or active, and the device
//! link (org.bluez.Device1) coming and going. When the audio stops these lines
//! show whether BlueZ still had a streaming transport at that moment, which
//! the sound server and AACP logs cannot tell. The transport state is also
//! kept for the audio health summary.

use {
    dbus::{
        Message, Path,
        arg::{PropMap, RefArg},
        blocking::{
            Connection,
            stdintf::org_freedesktop_dbus::{ObjectManager, Properties},
        },
    },
    std::{
        collections::HashMap,
        fmt,
        sync::{Arc, Mutex, PoisonError},
        time::{Duration, Instant},
    },
    tracing::{debug, info, warn},
};

/// The AACP service UUID that AirPods list among their services.
const AIRPODS_UUID: &str = "74ec2172-0bad-4d01-8f77-997b2be0722a";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const TRANSPORT_IFACE: &str = "org.bluez.MediaTransport1";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";
const OBJECT_MANAGER_IFACE: &str = "org.freedesktop.DBus.ObjectManager";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether a device's service list has the AirPods' AACP service. BlueZ
/// reports UUIDs in lowercase, but nothing guarantees it.
pub(crate) fn airpods_uuid_in(uuids: &[String]) -> bool {
    uuids.iter().any(|u| u.eq_ignore_ascii_case(AIRPODS_UUID))
}

/// Ask BlueZ whether the device at `device_path` is AirPods. None when BlueZ
/// does not answer, so the question is asked again next time.
pub(crate) fn has_airpods_uuid(conn: &Connection, device_path: &str) -> Option<bool> {
    conn.with_proxy("org.bluez", device_path, CALL_TIMEOUT)
        .get::<Vec<String>>(DEVICE_IFACE, "UUIDs")
        .ok()
        .map(|uuids| airpods_uuid_in(&uuids))
}

/// MediaTransport1.State. "active" is the only state in which audio flows:
/// "idle" is a suspended stream (AVDTP SUSPEND, or no stream started yet) and
/// "pending" a start that the AirPods have not confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TransportState {
    Idle,
    Pending,
    Active,
    /// A state this code does not know, such as LE Audio's "broadcasting".
    Other(String),
}

impl TransportState {
    fn parse(state: &str) -> Self {
        match state {
            "idle" => Self::Idle,
            "pending" => Self::Pending,
            "active" => Self::Active,
            other => Self::Other(other.to_string()),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        *self == Self::Active
    }
}

impl fmt::Display for TransportState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => f.write_str("idle"),
            Self::Pending => f.write_str("pending"),
            Self::Active => f.write_str("active"),
            Self::Other(state) => f.write_str(state),
        }
    }
}

/// An A2DP codec id as MediaTransport1.Codec carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Codec(pub(crate) u8);

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            0x00 => f.write_str("SBC"),
            0x01 => f.write_str("MPEG"),
            0x02 => f.write_str("AAC"),
            0x04 => f.write_str("ATRAC"),
            0xFF => f.write_str("vendor"),
            id => write!(f, "{id:#04x}"),
        }
    }
}

/// The MediaTransport1 properties one signal carried. A PropertiesChanged
/// signal holds only the properties that changed.
#[derive(Debug, Default, PartialEq, Eq)]
struct TransportProps {
    state: Option<TransportState>,
    codec: Option<Codec>,
    volume: Option<u64>,
    /// In units of 1/10 ms, as BlueZ reports it.
    delay: Option<u64>,
}

impl TransportProps {
    fn parse(props: &PropMap) -> Self {
        Self {
            state: props
                .get("State")
                .and_then(|v| v.0.as_str())
                .map(TransportState::parse),
            codec: props
                .get("Codec")
                .and_then(|v| v.0.as_u64())
                .and_then(|id| u8::try_from(id).ok())
                .map(Codec),
            volume: props.get("Volume").and_then(|v| v.0.as_u64()),
            delay: props.get("Delay").and_then(|v| v.0.as_u64()),
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// State and codec changes are what a reader of the log follows; volume
    /// and delay move with every volume key press.
    fn is_notable(&self) -> bool {
        self.state.is_some() || self.codec.is_some()
    }
}

impl fmt::Display for TransportProps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut sep = "";
        if let Some(state) = &self.state {
            write!(f, "state={state}")?;
            sep = " ";
        }
        if let Some(codec) = self.codec {
            write!(f, "{sep}codec={codec}")?;
            sep = " ";
        }
        if let Some(volume) = self.volume {
            write!(f, "{sep}volume={volume}")?;
            sep = " ";
        }
        if let Some(delay) = self.delay {
            write!(f, "{sep}delay={}.{}ms", delay / 10, delay % 10)?;
        }
        Ok(())
    }
}

/// The Device1 properties one signal carried that matter for the audio.
#[derive(Debug, Default, PartialEq, Eq)]
struct DeviceProps {
    connected: Option<bool>,
    services_resolved: Option<bool>,
}

impl DeviceProps {
    fn parse(props: &PropMap) -> Self {
        let flag = |name| props.get(name).and_then(|v| v.0.as_u64()).map(|v| v != 0);
        Self {
            connected: flag("Connected"),
            services_resolved: flag("ServicesResolved"),
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl fmt::Display for DeviceProps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let yes_no = |b: bool| if b { "yes" } else { "no" };
        let mut sep = "";
        if let Some(connected) = self.connected {
            write!(f, "connected={}", yes_no(connected))?;
            sep = " ";
        }
        if let Some(resolved) = self.services_resolved {
            write!(f, "{sep}services_resolved={}", yes_no(resolved))?;
        }
        Ok(())
    }
}

/// The device object a BlueZ object belongs to: the path up to its
/// `dev_XX_XX_XX_XX_XX_XX` element. Transports live below it, as
/// `.../dev_XX_XX_XX_XX_XX_XX/sep1/fd0`.
fn device_path(path: &str) -> Option<&str> {
    let start = path.find("/dev_")?;
    let end = path[start + 1..]
        .find('/')
        .map_or(path.len(), |i| start + 1 + i);
    Some(&path[..end])
}

/// The MAC address in a device path, as `AA:BB:CC:DD:EE:FF`.
fn mac_of(device_path: &str) -> Option<String> {
    let (_, element) = device_path.rsplit_once("/dev_")?;
    let mac = element.replace('_', ":").to_ascii_uppercase();
    (mac.len() == 17).then_some(mac)
}

/// The A2DP transport of one AirPods, as last reported by BlueZ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Transport {
    pub(crate) path: String,
    pub(crate) state: TransportState,
    pub(crate) codec: Option<Codec>,
    /// When `state` was entered, or when this process first saw it.
    pub(crate) since: Instant,
}

/// The current transport of each connected AirPods, by MAC. Written by the
/// BlueZ watcher, read by the audio health summary.
#[derive(Clone, Default)]
pub(crate) struct TransportStates {
    transports: Arc<Mutex<HashMap<String, Transport>>>,
}

impl TransportStates {
    // Single inserts and removes leave the map consistent under any panic, so
    // a poisoned lock is still usable.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Transport>> {
        self.transports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn get(&self, mac: &str) -> Option<Transport> {
        self.lock().get(mac).cloned()
    }

    /// Record what a signal reported about the transport at `path`. A new
    /// transport replaces the one the device had: BlueZ creates one object per
    /// stream and removes the old one.
    fn update(&self, mac: &str, path: &str, props: &TransportProps, now: Instant) {
        let fresh = || Transport {
            path: path.to_string(),
            state: TransportState::Idle,
            codec: None,
            since: now,
        };
        let mut transports = self.lock();
        let entry = transports.entry(mac.to_string()).or_insert_with(fresh);
        if entry.path != path {
            *entry = fresh();
        }
        if let Some(state) = &props.state
            && *state != entry.state
        {
            entry.state = state.clone();
            entry.since = now;
        }
        if props.codec.is_some() {
            entry.codec = props.codec;
        }
    }

    fn remove(&self, mac: &str, path: &str) {
        let mut transports = self.lock();
        if transports.get(mac).is_some_and(|t| t.path == path) {
            transports.remove(mac);
        }
    }

    fn remove_device(&self, mac: &str) {
        self.lock().remove(mac);
    }
}

/// Logs the BlueZ signals about AirPods and keeps their transport states.
/// Other devices are skipped; whether a device is AirPods is asked once per
/// connection through the lookup the caller passes.
pub(crate) struct BluezWatch {
    /// Whether each device path seen since it connected is AirPods. Entries go
    /// when the device disconnects, so the random addresses of nearby LE
    /// devices do not pile up here.
    airpods: HashMap<String, bool>,
    transports: TransportStates,
}

impl BluezWatch {
    pub(crate) fn new(transports: TransportStates) -> Self {
        Self {
            airpods: HashMap::new(),
            transports,
        }
    }

    /// Log the transports the AirPods already have when the app starts, so the
    /// first line of a run shows where the audio stood.
    pub(crate) fn seed_from_bus(&mut self, conn: &Connection, now: Instant) {
        match conn
            .with_proxy("org.bluez", "/", CALL_TIMEOUT)
            .get_managed_objects()
        {
            Ok(objects) => self.seed(&objects, now),
            Err(e) => warn!("[bluez] could not list BlueZ objects: {e}"),
        }
    }

    fn seed(&mut self, objects: &HashMap<Path<'static>, HashMap<String, PropMap>>, now: Instant) {
        for (path, interfaces) in objects {
            let Some(props) = interfaces.get(DEVICE_IFACE) else {
                continue;
            };
            let uuids: Vec<String> = props
                .get("UUIDs")
                .and_then(|v| v.0.as_iter())
                .map(|uuids| uuids.filter_map(|u| u.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if airpods_uuid_in(&uuids) {
                info!(
                    "[bluez] {path}: present at startup, {}",
                    DeviceProps::parse(props)
                );
                self.airpods.insert(path.to_string(), true);
            }
        }
        for (path, interfaces) in objects {
            if let Some(props) = interfaces.get(TRANSPORT_IFACE) {
                let mut known = |_: &str| Some(false);
                self.transport_appeared(path, props, "present at startup", now, &mut known);
            }
        }
    }

    /// Handle one BlueZ signal. `is_airpods` answers for a device path not
    /// seen yet on this connection, or None when it cannot tell.
    pub(crate) fn on_signal(
        &mut self,
        msg: &Message,
        now: Instant,
        mut is_airpods: impl FnMut(&str) -> Option<bool>,
    ) {
        let (Some(path), Some(iface), Some(member)) = (msg.path(), msg.interface(), msg.member())
        else {
            return;
        };
        match (&*iface, &*member) {
            (PROPERTIES_IFACE, "PropertiesChanged") => {
                if let Ok((changed_iface, changed)) = msg.read2::<String, PropMap>() {
                    self.properties_changed(&path, &changed_iface, &changed, now, &mut is_airpods);
                }
            },
            (OBJECT_MANAGER_IFACE, "InterfacesAdded") => {
                if let Ok((object, interfaces)) = msg.read2::<Path<'_>, HashMap<String, PropMap>>()
                    && let Some(props) = interfaces.get(TRANSPORT_IFACE)
                {
                    self.transport_appeared(&object, props, "added", now, &mut is_airpods);
                }
            },
            (OBJECT_MANAGER_IFACE, "InterfacesRemoved") => {
                if let Ok((object, interfaces)) = msg.read2::<Path<'_>, Vec<String>>()
                    && interfaces.iter().any(|i| i == TRANSPORT_IFACE)
                {
                    self.transport_removed(&object);
                }
            },
            _ => {},
        }
    }

    fn properties_changed(
        &mut self,
        path: &str,
        iface: &str,
        changed: &PropMap,
        now: Instant,
        is_airpods: &mut impl FnMut(&str) -> Option<bool>,
    ) {
        match iface {
            DEVICE_IFACE => {
                let props = DeviceProps::parse(changed);
                if props.is_empty() {
                    return;
                }
                let airpods = self.is_airpods(path, is_airpods);
                if airpods {
                    info!("[bluez] {path}: {props}");
                }
                if props.connected == Some(false) {
                    self.airpods.remove(path);
                    if airpods && let Some(mac) = mac_of(path) {
                        self.transports.remove_device(&mac);
                    }
                }
            },
            TRANSPORT_IFACE => {
                let props = TransportProps::parse(changed);
                let Some(device) = device_path(path) else {
                    return;
                };
                if props.is_empty() || !self.is_airpods(device, is_airpods) {
                    return;
                }
                if props.is_notable() {
                    info!("[bluez] transport {path}: {props}");
                } else {
                    debug!("[bluez] transport {path}: {props}");
                }
                if let Some(mac) = mac_of(device) {
                    self.transports.update(&mac, path, &props, now);
                }
            },
            _ => {},
        }
    }

    fn transport_appeared(
        &mut self,
        path: &str,
        props: &PropMap,
        how: &str,
        now: Instant,
        is_airpods: &mut impl FnMut(&str) -> Option<bool>,
    ) {
        let Some(device) = device_path(path) else {
            return;
        };
        if !self.is_airpods(device, is_airpods) {
            return;
        }
        let props = TransportProps::parse(props);
        info!("[bluez] transport {path} {how}: {props}");
        if let Some(mac) = mac_of(device) {
            self.transports.update(&mac, path, &props, now);
        }
    }

    fn transport_removed(&self, path: &str) {
        let Some(device) = device_path(path) else {
            return;
        };
        // Only a device already known as AirPods can have a transport in the
        // state; asking BlueZ about a device whose transport just went away is
        // not worth a round trip.
        if self.airpods.get(device) != Some(&true) {
            return;
        }
        info!("[bluez] transport {path} removed");
        if let Some(mac) = mac_of(device) {
            self.transports.remove(&mac, path);
        }
    }

    fn is_airpods(
        &mut self,
        device_path: &str,
        lookup: &mut impl FnMut(&str) -> Option<bool>,
    ) -> bool {
        if let Some(&known) = self.airpods.get(device_path) {
            return known;
        }
        let Some(answer) = lookup(device_path) else {
            return false;
        };
        self.airpods.insert(device_path.to_string(), answer);
        answer
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        dbus::arg::{RefArg, Variant},
    };

    const DEVICE: &str = "/org/bluez/hci0/dev_BC_80_4E_76_42_4B";
    const TRANSPORT: &str = "/org/bluez/hci0/dev_BC_80_4E_76_42_4B/sep1/fd0";
    const MAC: &str = "BC:80:4E:76:42:4B";

    fn props(entries: Vec<(&str, Box<dyn RefArg>)>) -> PropMap {
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), Variant(v)))
            .collect()
    }

    fn state(value: &str) -> PropMap {
        props(vec![("State", Box::new(value.to_string()))])
    }

    fn properties_changed(path: &str, iface: &str, changed: PropMap) -> Message {
        Message::new_signal(path, PROPERTIES_IFACE, "PropertiesChanged")
            .unwrap()
            .append3(iface, changed, Vec::<String>::new())
    }

    fn interfaces_added(path: &str, transport: PropMap) -> Message {
        let interfaces: HashMap<String, PropMap> =
            HashMap::from([(TRANSPORT_IFACE.to_string(), transport)]);
        Message::new_signal("/", OBJECT_MANAGER_IFACE, "InterfacesAdded")
            .unwrap()
            .append2(Path::from(path), interfaces)
    }

    fn interfaces_removed(path: &str) -> Message {
        Message::new_signal("/", OBJECT_MANAGER_IFACE, "InterfacesRemoved")
            .unwrap()
            .append2(Path::from(path), vec![TRANSPORT_IFACE.to_string()])
    }

    #[test]
    fn device_path_cuts_after_the_device_element() {
        assert_eq!(device_path(TRANSPORT), Some(DEVICE));
        assert_eq!(device_path(DEVICE), Some(DEVICE));
        assert_eq!(device_path("/org/bluez/hci0"), None);
        assert_eq!(mac_of(DEVICE).as_deref(), Some(MAC));
        assert_eq!(mac_of("/org/bluez/hci0/dev_BC_80"), None);
    }

    #[test]
    fn transport_props_print_only_what_the_signal_carried() {
        let all = TransportProps::parse(&props(vec![
            ("State", Box::new("active".to_string())),
            ("Codec", Box::new(0x02_u8)),
            ("Volume", Box::new(100_u16)),
            ("Delay", Box::new(1505_u16)),
        ]));
        assert_eq!(
            all.to_string(),
            "state=active codec=AAC volume=100 delay=150.5ms"
        );
        assert!(all.is_notable());

        let volume = TransportProps::parse(&props(vec![("Volume", Box::new(64_u16))]));
        assert_eq!(volume.to_string(), "volume=64");
        assert!(!volume.is_notable());

        assert!(TransportProps::parse(&props(vec![("Device", Box::new(0_u8))])).is_empty());
    }

    #[test]
    fn unknown_codecs_and_states_are_kept_as_they_came() {
        assert_eq!(Codec(0x00).to_string(), "SBC");
        assert_eq!(Codec(0x07).to_string(), "0x07");
        assert_eq!(
            TransportState::parse("broadcasting"),
            TransportState::Other("broadcasting".to_string())
        );
        assert_eq!(
            TransportState::parse("broadcasting").to_string(),
            "broadcasting"
        );
        assert!(!TransportState::parse("pending").is_active());
    }

    #[test]
    fn device_props_read_connected_and_services_resolved() {
        let p = DeviceProps::parse(&props(vec![
            ("Connected", Box::new(false)),
            ("ServicesResolved", Box::new(true)),
            ("RSSI", Box::new(-60_i16)),
        ]));
        assert_eq!(p.to_string(), "connected=no services_resolved=yes");
        assert!(DeviceProps::parse(&props(vec![("RSSI", Box::new(-60_i16))])).is_empty());
    }

    #[test]
    fn transport_state_changes_are_tracked_with_when_they_happened() {
        let transports = TransportStates::default();
        let mut watch = BluezWatch::new(transports.clone());
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(3);
        let t2 = t1 + Duration::from_secs(4);

        let added = props(vec![
            ("State", Box::new("idle".to_string())),
            ("Codec", Box::new(0x02_u8)),
        ]);
        watch.on_signal(&interfaces_added(TRANSPORT, added), t0, |_| Some(true));
        watch.on_signal(
            &properties_changed(TRANSPORT, TRANSPORT_IFACE, state("active")),
            t1,
            |_| panic!("the device is already known"),
        );
        let volume = props(vec![("Volume", Box::new(90_u16))]);
        watch.on_signal(
            &properties_changed(TRANSPORT, TRANSPORT_IFACE, volume),
            t2,
            |_| None,
        );

        assert_eq!(
            transports.get(MAC),
            Some(Transport {
                path: TRANSPORT.to_string(),
                state: TransportState::Active,
                codec: Some(Codec(0x02)),
                since: t1,
            })
        );

        watch.on_signal(&interfaces_removed(TRANSPORT), t2, |_| None);
        assert_eq!(transports.get(MAC), None);
    }

    #[test]
    fn other_devices_are_asked_about_once_and_not_tracked() {
        let transports = TransportStates::default();
        let mut watch = BluezWatch::new(transports.clone());
        let mut lookups = 0;
        for _ in 0..3 {
            watch.on_signal(
                &properties_changed(TRANSPORT, TRANSPORT_IFACE, state("active")),
                Instant::now(),
                |path| {
                    assert_eq!(path, DEVICE);
                    lookups += 1;
                    Some(false)
                },
            );
        }
        assert_eq!(lookups, 1);
        assert_eq!(transports.get(MAC), None);
    }

    #[test]
    fn other_devices_are_forgotten_when_they_disconnect() {
        let mut watch = BluezWatch::new(TransportStates::default());
        let connected = |on: bool| props(vec![("Connected", Box::new(on))]);
        watch.on_signal(
            &properties_changed(DEVICE, DEVICE_IFACE, connected(true)),
            Instant::now(),
            |_| Some(false),
        );
        watch.on_signal(
            &properties_changed(DEVICE, DEVICE_IFACE, connected(false)),
            Instant::now(),
            |_| panic!("the device is still known until it disconnects"),
        );

        assert!(watch.airpods.is_empty());
    }

    #[test]
    fn a_failed_lookup_is_asked_again() {
        let mut watch = BluezWatch::new(TransportStates::default());
        let mut lookups = 0;
        for _ in 0..2 {
            watch.on_signal(
                &properties_changed(TRANSPORT, TRANSPORT_IFACE, state("idle")),
                Instant::now(),
                |_| {
                    lookups += 1;
                    None
                },
            );
        }
        assert_eq!(lookups, 2);
    }

    #[test]
    fn changes_without_audio_properties_skip_the_lookup() {
        let mut watch = BluezWatch::new(TransportStates::default());
        let rssi = props(vec![("RSSI", Box::new(-70_i16))]);
        watch.on_signal(
            &properties_changed(DEVICE, DEVICE_IFACE, rssi),
            Instant::now(),
            |_| panic!("an RSSI change must not cost a D-Bus call"),
        );
    }

    #[test]
    fn a_disconnect_forgets_the_device_and_its_transport() {
        let transports = TransportStates::default();
        let mut watch = BluezWatch::new(transports.clone());
        watch.on_signal(
            &interfaces_added(TRANSPORT, state("active")),
            Instant::now(),
            |_| Some(true),
        );
        let disconnected = props(vec![("Connected", Box::new(false))]);
        watch.on_signal(
            &properties_changed(DEVICE, DEVICE_IFACE, disconnected),
            Instant::now(),
            |_| None,
        );

        assert_eq!(transports.get(MAC), None);
        let mut asked = false;
        watch.on_signal(
            &properties_changed(TRANSPORT, TRANSPORT_IFACE, state("idle")),
            Instant::now(),
            |_| {
                asked = true;
                Some(true)
            },
        );
        assert!(asked, "the device is looked up again after reconnecting");
    }

    #[test]
    fn startup_seed_tracks_transports_of_airpods_only() {
        let transports = TransportStates::default();
        let mut watch = BluezWatch::new(transports.clone());
        let other = "/org/bluez/hci0/dev_11_22_33_44_55_66";
        let device = |uuids: Vec<String>| {
            HashMap::from([(
                DEVICE_IFACE.to_string(),
                props(vec![
                    ("UUIDs", Box::new(uuids)),
                    ("Connected", Box::new(true)),
                ]),
            )])
        };
        let transport = || HashMap::from([(TRANSPORT_IFACE.to_string(), state("active"))]);
        let objects = HashMap::from([
            (
                Path::from(DEVICE),
                device(vec![AIRPODS_UUID.to_uppercase()]),
            ),
            (Path::from(TRANSPORT), transport()),
            (
                Path::from(other),
                device(vec!["0000110b-0000-1000-8000-00805f9b34fb".to_string()]),
            ),
            (Path::from(format!("{other}/sep1/fd2")), transport()),
        ]);
        let now = Instant::now();

        watch.seed(&objects, now);

        assert_eq!(
            transports.get(MAC).map(|t| t.state),
            Some(TransportState::Active)
        );
        assert_eq!(transports.get("11:22:33:44:55:66"), None);
    }
}
