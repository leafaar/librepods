use {
    crate::{
        bluetooth::aacp::{AACPEvent, BatteryComponent, BatteryInfo, BatteryStatus},
        devices::enums::{DeviceData, DeviceInformation, DeviceType},
        ui::{messages::BluetoothUIMessage, tray::MyTray},
        utils::{ah, get_devices_path, get_preferences_path},
    },
    aes::{
        Aes128,
        cipher::{Array, BlockCipherDecrypt, KeyInit},
    },
    bluer::{
        Address, DeviceEvent, DeviceProperty, Session,
        monitor::{Monitor, MonitorEvent, Pattern},
    },
    futures::{Stream, StreamExt},
    hex, serde_json,
    std::{
        collections::{HashMap, HashSet},
        sync::{Arc, Mutex, PoisonError},
        time::SystemTime,
    },
    tokio::{sync::mpsc::UnboundedSender, task::JoinHandle},
    tracing::{debug, info, warn},
};

/// Upper bound for the matched and rejected address caches. AirPods and every
/// other Apple device nearby rotate their resolvable private address about
/// every 15 minutes, so without a bound these caches grow for as long as the
/// app runs. Clearing them only costs a new IRK check per address.
const MAX_CACHED_ADDRESSES: usize = 1024;

fn decrypt(key: &[u8; 16], data: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new(&Array::from(*key));
    let mut block = Array::from(*data);
    cipher.decrypt_block(&mut block);
    block.into()
}

fn verify_rpa(addr: Address, irk: &[u8; 16]) -> bool {
    // The address is stored most significant byte first; the hash is its
    // three least significant bytes and prand the three most significant.
    let mut rpa = addr.0;
    rpa.reverse();
    let hash = [rpa[0], rpa[1], rpa[2]];
    let prand = [rpa[3], rpa[4], rpa[5]];
    let computed_hash = ah(irk, prand);
    debug!(
        "Verifying RPA: addr={}, hash={:?}, computed_hash={:?}",
        addr, hash, computed_hash
    );
    hash == computed_hash
}

/// LE keys of one paired AirPods, decoded from devices.json.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AirPodsKeys {
    irk: [u8; 16],
    enc_key: Option<[u8; 16]>,
}

fn decode_key(key: &str) -> Option<[u8; 16]> {
    hex::decode(key).ok()?.try_into().ok()
}

/// The keys of every AirPods that has an IRK, by the AirPods' public MAC.
fn parse_keys(devices: &HashMap<String, DeviceData>) -> HashMap<String, AirPodsKeys> {
    devices
        .iter()
        .filter(|(_, device)| device.type_ == DeviceType::AirPods)
        .filter_map(|(mac, device)| {
            let Some(DeviceInformation::AirPods(info)) = &device.information else {
                return None;
            };
            let keys = AirPodsKeys {
                irk: decode_key(&info.le_keys.irk)?,
                enc_key: decode_key(&info.le_keys.enc_key),
            };
            Some((mac.clone(), keys))
        })
        .collect()
}

/// Keys loaded from devices.json, with the file's modification time at load,
/// so a later change (AirPods paired while the app runs) can be picked up.
struct KnownKeys {
    keys: HashMap<String, AirPodsKeys>,
    modified: Option<SystemTime>,
}

async fn devices_file_modified() -> Option<SystemTime> {
    tokio::fs::metadata(get_devices_path())
        .await
        .and_then(|m| m.modified())
        .ok()
}

async fn load_keys() -> KnownKeys {
    // Read the time first: a write landing in between then shows up as a
    // change on the next check instead of being missed.
    let modified = devices_file_modified().await;
    let devices: HashMap<String, DeviceData> = tokio::fs::read_to_string(get_devices_path())
        .await
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    KnownKeys {
        keys: parse_keys(&devices),
        modified,
    }
}

/// AirPods with a `bluetoothctl connect` in flight, so advertisements seen
/// meanwhile (from this address or a rotated one) do not start another.
#[derive(Clone, Default)]
struct ConnectAttempts(Arc<Mutex<HashSet<String>>>);

impl ConnectAttempts {
    /// Mark `mac` as connecting, or `None` when an attempt is already running.
    /// The mark is removed when the returned guard is dropped, whatever way the
    /// attempt ends.
    fn begin(&self, mac: &str) -> Option<ConnectAttempt> {
        let mut in_flight = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        in_flight.insert(mac.to_string()).then(|| ConnectAttempt {
            attempts: self.clone(),
            mac: mac.to_string(),
        })
    }
}

struct ConnectAttempt {
    attempts: ConnectAttempts,
    mac: String,
}

impl Drop for ConnectAttempt {
    fn drop(&mut self) {
        self.attempts
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.mac);
    }
}

async fn connect_airpods(attempts: &ConnectAttempts, airpods_mac: &str) {
    let Some(_attempt) = attempts.begin(airpods_mac) else {
        info!(
            "Already connecting to {}, skipping duplicate attempt.",
            airpods_mac
        );
        return;
    };
    info!(
        "AirPods are disconnected, attempting to connect to {}",
        airpods_mac
    );
    // bluer's Device::connect does not bring the AirPods up here, bluetoothctl does.
    let output = tokio::process::Command::new("bluetoothctl")
        .arg("connect")
        .arg(airpods_mac)
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => {
            info!("Successfully connected to AirPods {}", airpods_mac);
        },
        Ok(output) => {
            info!(
                "Failed to connect to AirPods {}: {}",
                airpods_mac,
                String::from_utf8_lossy(&output.stderr)
            );
        },
        Err(e) => {
            info!(
                "Failed to execute bluetoothctl to connect to AirPods {}: {}",
                airpods_mac, e
            );
        },
    }
}

/// Maps advertising addresses to the public MAC of the paired AirPods they
/// belong to, by checking each new address against every known IRK.
struct AddressResolver {
    known: KnownKeys,
    verified: HashMap<Address, String>,
    failed: HashSet<Address>,
}

impl AddressResolver {
    async fn load() -> Self {
        Self {
            known: load_keys().await,
            verified: HashMap::new(),
            failed: HashSet::new(),
        }
    }

    /// The public MAC of the AirPods advertising from `addr`, if they are
    /// ours. Reloads the keys first when devices.json changed.
    async fn resolve(&mut self, addr: Address) -> Option<String> {
        if let Some(airpods_mac) = self.verified.get(&addr) {
            return Some(airpods_mac.clone());
        }
        let modified = devices_file_modified().await;
        if modified != self.known.modified {
            self.known = load_keys().await;
            self.verified.clear();
            self.failed.clear();
            info!(
                "Devices file changed, loaded LE keys for {} AirPods",
                self.known.keys.len()
            );
        }
        if self.failed.contains(&addr) {
            return None;
        }
        debug!("Checking RPA for device: {addr}");
        let found = self
            .known
            .keys
            .iter()
            .find(|(_, keys)| verify_rpa(addr, &keys.irk))
            .map(|(airpods_mac, _)| airpods_mac.clone());
        let Some(airpods_mac) = found else {
            if self.failed.len() >= MAX_CACHED_ADDRESSES {
                self.failed.clear();
            }
            self.failed.insert(addr);
            debug!("Device {addr} did not match any of our irks");
            return None;
        };
        info!("Matched our device ({addr}) with the irk for {airpods_mac}");
        if self.verified.len() >= MAX_CACHED_ADDRESSES {
            self.verified.clear();
        }
        self.verified.insert(addr, airpods_mac.clone());
        Some(airpods_mac)
    }

    fn enc_key(&self, airpods_mac: &str) -> Option<[u8; 16]> {
        self.known.keys.get(airpods_mac).and_then(|k| k.enc_key)
    }
}

pub async fn start_le_monitor(
    tray_handle: Option<ksni::Handle<MyTray>>,
    ui_tx: UnboundedSender<BluetoothUIMessage>,
) -> bluer::Result<()> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let mut resolver = AddressResolver::load().await;
    // One advertisement listener per address. BlueZ reports DeviceFound again
    // for an address it already reported, and each report used to add a
    // listener.
    let mut listeners: HashMap<Address, JoinHandle<()>> = HashMap::new();
    let connect_attempts = ConnectAttempts::default();

    let pattern = Pattern {
        data_type: 0xFF, // Manufacturer specific data
        start_position: 0,
        content: vec![0x4C, 0x00], // Apple manufacturer ID (76) in LE
    };

    let mm = adapter.monitor().await?;
    let mut monitor_handle = mm
        .register(Monitor {
            monitor_type: bluer::monitor::Type::OrPatterns,
            rssi_low_threshold: None,
            rssi_high_threshold: None,
            rssi_low_timeout: None,
            rssi_high_timeout: None,
            rssi_sampling_period: None,
            patterns: Some(vec![pattern]),
            ..Default::default()
        })
        .await?;

    debug!("Started LE monitor");

    while let Some(mevt) = monitor_handle.next().await {
        let addr = match mevt {
            MonitorEvent::DeviceFound(devid) => devid.device,
            MonitorEvent::DeviceLost(devid) => {
                if let Some(listener) = listeners.remove(&devid.device) {
                    debug!("Lost device {}, stopping its listener", devid.device);
                    listener.abort();
                }
                continue;
            },
            _ => continue,
        };

        if listeners.get(&addr).is_some_and(|l| !l.is_finished()) {
            continue;
        }
        let Some(airpods_mac) = resolver.resolve(addr).await else {
            continue;
        };
        let Some(enc_key) = resolver.enc_key(&airpods_mac) else {
            debug!("No advertisement key for {airpods_mac}, not listening to {addr}");
            continue;
        };

        // One device failing must not end the monitor for every other one.
        let events = match adapter.device(addr) {
            Ok(dev) => dev.events().await,
            Err(e) => Err(e),
        };
        let events = match events {
            Ok(events) => events,
            Err(e) => {
                warn!("Cannot listen to advertisements from {addr}: {e}");
                continue;
            },
        };

        listeners.retain(|_, l| !l.is_finished());
        let listener = tokio::spawn(watch_advertisements(
            events,
            airpods_mac,
            enc_key,
            tray_handle.clone(),
            ui_tx.clone(),
            connect_attempts.clone(),
        ));
        listeners.insert(addr, listener);
    }

    Ok(())
}

/// Battery of one component as advertised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AdvertisedBattery {
    level: u8,
    charging: bool,
}

impl AdvertisedBattery {
    /// Level in the low seven bits, charging in the high bit; 0xFF when the
    /// component is not connected.
    fn decode(byte: u8) -> Option<Self> {
        (byte != 0xFF).then_some(Self {
            level: byte & 0x7F,
            charging: byte & 0x80 != 0,
        })
    }

    fn status(self) -> BatteryStatus {
        if self.charging {
            BatteryStatus::Charging
        } else {
            BatteryStatus::NotCharging
        }
    }

    /// Tray level and status of a component that may not be connected.
    fn tray_entry(battery: Option<Self>) -> (Option<u8>, Option<BatteryStatus>) {
        match battery {
            Some(b) => (Some(b.level), Some(b.status())),
            None => (None, Some(BatteryStatus::Disconnected)),
        }
    }

    fn describe(battery: Option<Self>) -> String {
        match battery {
            Some(b) => format!("{}% (charging: {})", b.level, b.charging),
            None => "disconnected".to_string(),
        }
    }
}

/// What one advertisement says about the buds and the case.
#[derive(Debug, PartialEq, Eq)]
struct AdvertisedStatus {
    left: Option<AdvertisedBattery>,
    right: Option<AdvertisedBattery>,
    case: Option<AdvertisedBattery>,
    left_in_ear: bool,
    right_in_ear: bool,
}

impl AdvertisedStatus {
    /// Decode the status byte of the advertisement and the batteries from
    /// its decrypted payload. Which bud comes first in both depends on which
    /// one is primary and whether it sits in the case.
    fn decode(status: u8, decrypted: &[u8; 16]) -> Self {
        let primary_left = (status >> 5) & 0x01 == 1;
        let this_in_case = (status >> 6) & 0x01 == 1;
        let xor_factor = primary_left ^ this_in_case;
        let (left_in_ear_bit, right_in_ear_bit) = if xor_factor {
            (0x02, 0x08)
        } else {
            (0x08, 0x02)
        };
        let (left_index, right_index) = if primary_left { (1, 2) } else { (2, 1) };
        Self {
            left: AdvertisedBattery::decode(decrypted[left_index]),
            right: AdvertisedBattery::decode(decrypted[right_index]),
            case: AdvertisedBattery::decode(decrypted[3]),
            left_in_ear: status & left_in_ear_bit != 0,
            right_in_ear: status & right_in_ear_bit != 0,
        }
    }

    fn update_tray(&self, tray: &mut MyTray) {
        (tray.battery_l, tray.battery_l_status) = AdvertisedBattery::tray_entry(self.left);
        (tray.battery_r, tray.battery_r_status) = AdvertisedBattery::tray_entry(self.right);
        (tray.battery_c, tray.battery_c_status) = AdvertisedBattery::tray_entry(self.case);
    }

    /// The connected components, for the window. Over AACP the case reports
    /// itself as disconnected whenever the buds are outside it, so the
    /// advertisement is where its level comes from.
    fn battery_info(&self) -> Vec<BatteryInfo> {
        [
            (BatteryComponent::Left, self.left),
            (BatteryComponent::Right, self.right),
            (BatteryComponent::Case, self.case),
        ]
        .into_iter()
        .filter_map(|(component, battery)| {
            battery.map(|b| BatteryInfo {
                component,
                level: b.level,
                status: b.status(),
            })
        })
        .collect()
    }
}

/// Connect the AirPods unless their autoConnect preference is off.
async fn auto_connect(connect_attempts: &ConnectAttempts, airpods_mac: &str) {
    let preferences: HashMap<String, HashMap<String, bool>> =
        std::fs::read_to_string(get_preferences_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
    let auto_connect = preferences
        .get(airpods_mac)
        .and_then(|prefs| prefs.get("autoConnect"))
        .copied()
        .unwrap_or(true);
    debug!("Auto-connect preference for {airpods_mac}: {auto_connect}");
    if auto_connect {
        connect_airpods(connect_attempts, airpods_mac).await;
    } else {
        info!("Auto-connect is disabled for {airpods_mac}, not attempting to connect.");
    }
}

async fn watch_advertisements(
    mut events: impl Stream<Item = DeviceEvent> + Unpin,
    airpods_mac: String,
    enc_key: [u8; 16],
    tray_handle: Option<ksni::Handle<MyTray>>,
    ui_tx: UnboundedSender<BluetoothUIMessage>,
    connect_attempts: ConnectAttempts,
) {
    while let Some(ev) = events.next().await {
        let DeviceEvent::PropertyChanged(DeviceProperty::ManufacturerData(data)) = ev else {
            continue;
        };
        let Some(apple_data) = data.get(&76) else {
            continue;
        };
        if apple_data.len() <= 20 {
            continue;
        }
        let Some(encrypted) = apple_data.last_chunk::<16>() else {
            continue;
        };
        let decrypted = decrypt(&enc_key, encrypted);
        debug!(
            "Decrypted data from airpods_mac {}: {}",
            airpods_mac,
            hex::encode(decrypted)
        );

        let connection_state = apple_data[10];
        debug!("Connection state: {connection_state}");
        if connection_state == 0x00 {
            auto_connect(&connect_attempts, &airpods_mac).await;
        }

        let status = AdvertisedStatus::decode(apple_data[5], &decrypted);
        if let Some(handle) = &tray_handle {
            handle
                .update(|tray: &mut MyTray| status.update_tray(tray))
                .await;
        }

        let battery_info = status.battery_info();
        if !battery_info.is_empty() {
            let _ = ui_tx.send(BluetoothUIMessage::AACPUIEvent(
                airpods_mac.clone(),
                AACPEvent::BatteryInfo(battery_info),
            ));
        }

        debug!(
            "Battery status: Left: {}, Right: {}, Case: {}, InEar: L:{} R:{}",
            AdvertisedBattery::describe(status.left),
            AdvertisedBattery::describe(status.right),
            AdvertisedBattery::describe(status.case),
            status.left_in_ear,
            status.right_in_ear
        );
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{bluetooth::aacp::AirPodsLEKeys, devices::airpods::AirPodsInformation},
    };

    // Bluetooth Core Specification, Vol 3 Part H, D.7 (ah random address hash):
    // IRK ec0234a357c8ad05341010a60a397d9b, prand 708194, hash 0dfbaa. The key
    // is stored least significant byte first, as the AirPods send it.
    const SPEC_IRK: [u8; 16] = [
        0x9b, 0x7d, 0x39, 0x0a, 0xa6, 0x10, 0x10, 0x34, 0x05, 0xad, 0xc8, 0x57, 0xa3, 0x34, 0x02,
        0xec,
    ];

    #[test]
    fn verify_rpa_accepts_spec_sample_address() {
        let addr: Address = "70:81:94:0D:FB:AA".parse().unwrap();
        assert!(verify_rpa(addr, &SPEC_IRK));
    }

    #[test]
    fn verify_rpa_rejects_wrong_hash() {
        let addr: Address = "70:81:94:0D:FB:AB".parse().unwrap();
        assert!(!verify_rpa(addr, &SPEC_IRK));
    }

    fn airpods(irk: &str, enc_key: &str) -> DeviceData {
        DeviceData {
            name: "AirPods".to_string(),
            type_: DeviceType::AirPods,
            information: Some(DeviceInformation::AirPods(AirPodsInformation {
                le_keys: AirPodsLEKeys {
                    irk: irk.to_string(),
                    enc_key: enc_key.to_string(),
                },
                ..Default::default()
            })),
        }
    }

    #[test]
    fn parse_keys_skips_devices_without_a_valid_irk() {
        let irk = hex::encode(SPEC_IRK);
        let devices = HashMap::from([
            ("AA:AA:AA:AA:AA:AA".to_string(), airpods(&irk, &irk)),
            ("BB:BB:BB:BB:BB:BB".to_string(), airpods(&irk, "")),
            ("CC:CC:CC:CC:CC:CC".to_string(), airpods("", &irk)),
            ("DD:DD:DD:DD:DD:DD".to_string(), airpods("0102", &irk)),
            (
                "EE:EE:EE:EE:EE:EE".to_string(),
                DeviceData {
                    name: "Other".to_string(),
                    type_: DeviceType::Nothing,
                    information: None,
                },
            ),
        ]);

        let keys = parse_keys(&devices);

        assert_eq!(keys.len(), 2);
        assert_eq!(
            keys["AA:AA:AA:AA:AA:AA"],
            AirPodsKeys {
                irk: SPEC_IRK,
                enc_key: Some(SPEC_IRK)
            }
        );
        assert_eq!(keys["BB:BB:BB:BB:BB:BB"].enc_key, None);
    }

    #[test]
    fn connect_attempt_blocks_duplicates_until_dropped() {
        let attempts = ConnectAttempts::default();
        let first = attempts.begin("AA:BB:CC:DD:EE:FF");
        assert!(first.is_some());
        assert!(attempts.begin("AA:BB:CC:DD:EE:FF").is_none());
        assert!(attempts.begin("11:22:33:44:55:66").is_some());

        drop(first);

        assert!(attempts.begin("AA:BB:CC:DD:EE:FF").is_some());
    }

    #[test]
    fn advertised_battery_splits_level_and_charging_bit() {
        // 0xB7 is the charging bit (0x80) plus 55.
        assert_eq!(
            AdvertisedBattery::decode(0xB7),
            Some(AdvertisedBattery {
                level: 55,
                charging: true
            })
        );
        assert_eq!(
            AdvertisedBattery::decode(100),
            Some(AdvertisedBattery {
                level: 100,
                charging: false
            })
        );
        assert_eq!(AdvertisedBattery::decode(0xFF), None);
    }

    fn decrypted(first: u8, second: u8, case: u8) -> [u8; 16] {
        let mut data = [0u8; 16];
        data[1] = first;
        data[2] = second;
        data[3] = case;
        data
    }

    #[test]
    fn advertised_status_with_left_primary_reads_left_first() {
        // Primary left (bit 5), not in the case: left in ear is bit 1. 0xA8 is
        // the charging bit plus 40.
        let status = AdvertisedStatus::decode(0x20 | 0x02, &decrypted(90, 0xA8, 0xFF));

        assert_eq!(
            status,
            AdvertisedStatus {
                left: Some(AdvertisedBattery {
                    level: 90,
                    charging: false
                }),
                right: Some(AdvertisedBattery {
                    level: 40,
                    charging: true
                }),
                case: None,
                left_in_ear: true,
                right_in_ear: false,
            }
        );
    }

    #[test]
    fn advertised_status_with_right_primary_swaps_the_buds() {
        // Primary right and in the case: left in ear is still bit 1.
        let status = AdvertisedStatus::decode(0x40 | 0x02, &decrypted(10, 20, 30));

        assert_eq!(status.left.map(|b| b.level), Some(20));
        assert_eq!(status.right.map(|b| b.level), Some(10));
        assert_eq!(status.case.map(|b| b.level), Some(30));
        assert!(status.left_in_ear);
        assert!(!status.right_in_ear);
    }

    #[test]
    fn battery_info_leaves_out_disconnected_components() {
        let status = AdvertisedStatus::decode(0x20, &decrypted(0xFF, 0x85, 60));

        assert_eq!(
            status.battery_info(),
            vec![
                BatteryInfo {
                    component: BatteryComponent::Right,
                    level: 5,
                    status: BatteryStatus::Charging,
                },
                BatteryInfo {
                    component: BatteryComponent::Case,
                    level: 60,
                    status: BatteryStatus::NotCharging,
                },
            ]
        );
    }
}
