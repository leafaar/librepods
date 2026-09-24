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

pub async fn start_le_monitor(
    tray_handle: Option<ksni::Handle<MyTray>>,
    ui_tx: UnboundedSender<BluetoothUIMessage>,
) -> bluer::Result<()> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let mut known = load_keys().await;
    let mut verified_macs: HashMap<Address, String> = HashMap::new();
    let mut failed_macs: HashSet<Address> = HashSet::new();
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

        let airpods_mac = if let Some(airpods_mac) = verified_macs.get(&addr) {
            airpods_mac.clone()
        } else {
            let modified = devices_file_modified().await;
            if modified != known.modified {
                known = load_keys().await;
                verified_macs.clear();
                failed_macs.clear();
                info!(
                    "Devices file changed, loaded LE keys for {} AirPods",
                    known.keys.len()
                );
            }
            if failed_macs.contains(&addr) {
                continue;
            }
            debug!("Checking RPA for device: {}", addr);
            let found = known
                .keys
                .iter()
                .find(|(_, keys)| verify_rpa(addr, &keys.irk))
                .map(|(airpods_mac, _)| airpods_mac.clone());
            let Some(airpods_mac) = found else {
                if failed_macs.len() >= MAX_CACHED_ADDRESSES {
                    failed_macs.clear();
                }
                failed_macs.insert(addr);
                debug!("Device {} did not match any of our irks", addr);
                continue;
            };
            info!(
                "Matched our device ({}) with the irk for {}",
                addr, airpods_mac
            );
            if verified_macs.len() >= MAX_CACHED_ADDRESSES {
                verified_macs.clear();
            }
            verified_macs.insert(addr, airpods_mac.clone());
            airpods_mac
        };

        let Some(enc_key) = known.keys.get(&airpods_mac).and_then(|k| k.enc_key) else {
            debug!(
                "No advertisement key for {}, not listening to {}",
                airpods_mac, addr
            );
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
                warn!("Cannot listen to advertisements from {}: {}", addr, e);
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
        let last_16: [u8; 16] = apple_data[apple_data.len() - 16..]
            .try_into()
            .expect("slice is 16 bytes long");
        let decrypted = decrypt(&enc_key, &last_16);
        debug!(
            "Decrypted data from airpods_mac {}: {}",
            airpods_mac,
            hex::encode(decrypted)
        );

        let connection_state = apple_data[10] as usize;
        debug!("Connection state: {}", connection_state);
        if connection_state == 0x00 {
            let pref_path = get_preferences_path();
            let preferences: HashMap<String, HashMap<String, bool>> =
                std::fs::read_to_string(&pref_path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
            let auto_connect = preferences
                .get(&airpods_mac)
                .and_then(|prefs| prefs.get("autoConnect"))
                .copied()
                .unwrap_or(true);
            debug!(
                "Auto-connect preference for {}: {}",
                airpods_mac, auto_connect
            );
            if auto_connect {
                connect_airpods(&connect_attempts, &airpods_mac).await;
            } else {
                info!(
                    "Auto-connect is disabled for {}, not attempting to connect.",
                    airpods_mac
                );
            }
        }

        let status = apple_data[5] as usize;
        let primary_left = (status >> 5) & 0x01 == 1;
        let this_in_case = (status >> 6) & 0x01 == 1;
        let xor_factor = primary_left ^ this_in_case;
        let is_left_in_ear = if xor_factor {
            (status & 0x02) != 0
        } else {
            (status & 0x08) != 0
        };
        let is_right_in_ear = if xor_factor {
            (status & 0x08) != 0
        } else {
            (status & 0x02) != 0
        };
        let is_flipped = !primary_left;

        let left_byte_index = if is_flipped { 2 } else { 1 };
        let right_byte_index = if is_flipped { 1 } else { 2 };

        let left_byte = decrypted[left_byte_index] as i32;
        let right_byte = decrypted[right_byte_index] as i32;
        let case_byte = decrypted[3] as i32;

        let (left_battery, left_charging) = if left_byte == 0xff {
            (0, false)
        } else {
            (left_byte & 0x7F, (left_byte & 0x80) != 0)
        };
        let (right_battery, right_charging) = if right_byte == 0xff {
            (0, false)
        } else {
            (right_byte & 0x7F, (right_byte & 0x80) != 0)
        };
        let (case_battery, case_charging) = if case_byte == 0xff {
            (0, false)
        } else {
            (case_byte & 0x7F, (case_byte & 0x80) != 0)
        };

        if let Some(handle) = &tray_handle {
            handle
                .update(|tray: &mut MyTray| {
                    tray.battery_l = if left_byte == 0xff {
                        None
                    } else {
                        Some(left_battery as u8)
                    };
                    tray.battery_l_status = if left_byte == 0xff {
                        Some(BatteryStatus::Disconnected)
                    } else if left_charging {
                        Some(BatteryStatus::Charging)
                    } else {
                        Some(BatteryStatus::NotCharging)
                    };
                    tray.battery_r = if right_byte == 0xff {
                        None
                    } else {
                        Some(right_battery as u8)
                    };
                    tray.battery_r_status = if right_byte == 0xff {
                        Some(BatteryStatus::Disconnected)
                    } else if right_charging {
                        Some(BatteryStatus::Charging)
                    } else {
                        Some(BatteryStatus::NotCharging)
                    };
                    tray.battery_c = if case_byte == 0xff {
                        None
                    } else {
                        Some(case_battery as u8)
                    };
                    tray.battery_c_status = if case_byte == 0xff {
                        Some(BatteryStatus::Disconnected)
                    } else if case_charging {
                        Some(BatteryStatus::Charging)
                    } else {
                        Some(BatteryStatus::NotCharging)
                    };
                })
                .await;
        }

        // The tray is not the only consumer: the window shows the
        // case level too, and over AACP the case reports itself as
        // disconnected whenever the buds are outside it.
        let battery_info = [
            (
                BatteryComponent::Left,
                left_byte,
                left_battery,
                left_charging,
            ),
            (
                BatteryComponent::Right,
                right_byte,
                right_battery,
                right_charging,
            ),
            (
                BatteryComponent::Case,
                case_byte,
                case_battery,
                case_charging,
            ),
        ]
        .into_iter()
        .filter(|(_, raw, _, _)| *raw != 0xff)
        .map(|(component, _, level, charging)| BatteryInfo {
            component,
            level: level as u8,
            status: if charging {
                BatteryStatus::Charging
            } else {
                BatteryStatus::NotCharging
            },
        })
        .collect::<Vec<_>>();

        if !battery_info.is_empty() {
            let _ = ui_tx.send(BluetoothUIMessage::AACPUIEvent(
                airpods_mac.clone(),
                AACPEvent::BatteryInfo(battery_info),
            ));
        }

        debug!(
            "Battery status: Left: {}, Right: {}, Case: {}, InEar: L:{} R:{}",
            if left_byte == 0xff {
                "disconnected".to_string()
            } else {
                format!("{}% (charging: {})", left_battery, left_charging)
            },
            if right_byte == 0xff {
                "disconnected".to_string()
            } else {
                format!("{}% (charging: {})", right_battery, right_charging)
            },
            if case_byte == 0xff {
                "disconnected".to_string()
            } else {
                format!("{}% (charging: {})", case_battery, case_charging)
            },
            is_left_in_ear,
            is_right_in_ear
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
}
