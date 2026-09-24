use crate::bluetooth::att::{ATTHandles, ATTManager};
use crate::devices::enums::{DeviceData, DeviceInformation, DeviceType};
use crate::ui::messages::BluetoothUIMessage;
use crate::utils::{get_devices_path, update_devices_file};
use bluer::Address;
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NothingInformation {
    pub serial_number: String,
    pub firmware_version: String,
}

pub struct NothingDevice {
    pub att_manager: ATTManager,
    pub information: NothingInformation,
}

impl NothingDevice {
    pub async fn new(
        mac_address: Address,
        ui_tx: mpsc::UnboundedSender<BluetoothUIMessage>,
    ) -> bluer::Result<Self> {
        let mut att_manager = ATTManager::new();
        att_manager.connect(mac_address).await?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();

        att_manager
            .register_listener(ATTHandles::NothingEverythingRead, tx)
            .await;

        let devices: HashMap<String, DeviceData> = std::fs::read_to_string(get_devices_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let device_key = mac_address.to_string();
        let information = if let Some(device_data) = devices.get(&device_key) {
            let info = device_data.information.clone();
            if let Some(DeviceInformation::Nothing(ref nothing_info)) = info {
                nothing_info.clone()
            } else {
                NothingInformation {
                    serial_number: String::new(),
                    firmware_version: String::new(),
                }
            }
        } else {
            NothingInformation {
                serial_number: String::new(),
                firmware_version: String::new(),
            }
        };

        // Request version information
        att_manager
            .write(
                ATTHandles::NothingEverything,
                &[
                    0x55, 0x20, 0x01, 0x42, 0xC0, 0x00, 0x00, 0x00, 0x00,
                    0x00, // something, idk
                ],
            )
            .await?;

        sleep(Duration::from_millis(100)).await;

        // Request serial number
        att_manager
            .write(
                ATTHandles::NothingEverything,
                &[0x55, 0x20, 0x01, 0x06, 0xC0, 0x00, 0x00, 0x13, 0x00, 0x00],
            )
            .await?;

        // let ui_tx_clone = ui_tx.clone();
        tokio::spawn(async move {
            while let Some(data) = rx.recv().await {
                if data.starts_with(&[0x55, 0x20, 0x01, 0x42, 0x40]) {
                    let Some(version_bytes) = data.get(8..) else {
                        continue;
                    };
                    let firmware_version = String::from_utf8_lossy(version_bytes).to_string();
                    info!(
                        "Received firmware version from Nothing device {}: {}",
                        mac_address, firmware_version
                    );
                    save_nothing_info(device_key.clone(), move |info| {
                        info.firmware_version = firmware_version;
                    });
                } else if data.starts_with(&[0x55, 0x20, 0x01, 0x06, 0x40]) {
                    let serial_number_start_position = data
                        .iter()
                        .position(|&b| b == "S".as_bytes()[0])
                        .unwrap_or(8);
                    let serial_number_end = data
                        .iter()
                        .skip(serial_number_start_position)
                        .position(|&b| b == 0x0A)
                        .map(|pos| pos + serial_number_start_position)
                        .unwrap_or(data.len());
                    if data.get(serial_number_start_position + 1) == Some(&"H".as_bytes()[0]) {
                        let serial_number = String::from_utf8_lossy(
                            &data[serial_number_start_position..serial_number_end],
                        )
                        .to_string();
                        info!(
                            "Received serial number from Nothing device {}: {}",
                            mac_address, serial_number
                        );
                        save_nothing_info(device_key.clone(), move |info| {
                            info.serial_number = serial_number;
                        });
                    } else {
                        debug!(
                            "Serial number format unexpected from Nothing device {}: {:?}",
                            mac_address, data
                        );
                    }
                }

                debug!(
                    "Received data from (Nothing) device {}, data: {:?}",
                    mac_address, data
                );
            }
        });

        Ok(NothingDevice {
            att_manager,
            information,
        })
    }
}

/// Update this device's saved information from the file's current contents, so
/// the other field and every other device are kept.
fn save_nothing_info(key: String, update: impl FnOnce(&mut NothingInformation) + Send + 'static) {
    tokio::task::spawn_blocking(move || {
        let result = update_devices_file(|devices| {
            let entry = devices.entry(key).or_insert_with(|| DeviceData {
                name: "Nothing Device".to_string(),
                type_: DeviceType::Nothing,
                information: None,
            });
            if !matches!(entry.information, Some(DeviceInformation::Nothing(_))) {
                entry.information = Some(DeviceInformation::Nothing(NothingInformation::default()));
            }
            if let Some(DeviceInformation::Nothing(info)) = entry.information.as_mut() {
                update(info);
            }
        });
        if let Err(e) = result {
            error!("Failed to save Nothing device information: {}", e);
        }
    });
}
