use {
    crate::{
        bluetooth::{
            att::{ATTHandles, ATTManager, AttError},
            l2cap::ConnectError,
        },
        devices::enums::{DeviceData, DeviceInformation, DeviceType},
        ui::messages::BluetoothUIMessage,
        utils::update_devices_file,
    },
    bluer::Address,
    serde::{Deserialize, Serialize},
    std::time::Duration,
    tokio::{sync::mpsc, time::sleep},
    tracing::{debug, error, info},
};

/// Asks for the firmware version, answered with `FIRMWARE_VERSION_PREFIX`.
const REQUEST_FIRMWARE_VERSION: [u8; 10] =
    [0x55, 0x20, 0x01, 0x42, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00];
/// Asks for the serial number, answered with `SERIAL_NUMBER_PREFIX`.
const REQUEST_SERIAL_NUMBER: [u8; 10] =
    [0x55, 0x20, 0x01, 0x06, 0xC0, 0x00, 0x00, 0x13, 0x00, 0x00];
const FIRMWARE_VERSION_PREFIX: [u8; 5] = [0x55, 0x20, 0x01, 0x42, 0x40];
const SERIAL_NUMBER_PREFIX: [u8; 5] = [0x55, 0x20, 0x01, 0x06, 0x40];

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct NothingInformation {
    pub serial_number: String,
    pub firmware_version: String,
}

/// Why connected Nothing earbuds could not be set up. The cause is part of the
/// message because the callers log these with `{}`.
#[derive(Debug, thiserror::Error)]
pub enum NothingSetupError {
    #[error("opening the ATT channel failed: {0}")]
    Connect(ConnectError),
    #[error("requesting the firmware version failed: {0}")]
    RequestFirmwareVersion(AttError),
    #[error("requesting the serial number failed: {0}")]
    RequestSerialNumber(AttError),
}

/// A notification from the earbuds on the read handle, decoded.
#[derive(Debug, PartialEq, Eq)]
enum Report {
    FirmwareVersion(String),
    SerialNumber(String),
    /// A serial number response whose serial does not start with "SH".
    UnexpectedSerialNumber,
}

/// The firmware version follows the 8 byte header. The serial number starts
/// at the first 'S' (byte 8 when there is none), must continue with 'H', and
/// ends before the next newline or at the end.
fn parse_report(data: &[u8]) -> Option<Report> {
    if data.starts_with(&FIRMWARE_VERSION_PREFIX) {
        let version_bytes = data.get(8..)?;
        return Some(Report::FirmwareVersion(
            String::from_utf8_lossy(version_bytes).into_owned(),
        ));
    }
    if !data.starts_with(&SERIAL_NUMBER_PREFIX) {
        return None;
    }
    let start = data.iter().position(|&b| b == b'S').unwrap_or(8);
    if data.get(start + 1) != Some(&b'H') {
        return Some(Report::UnexpectedSerialNumber);
    }
    let end = data[start..]
        .iter()
        .position(|&b| b == 0x0A)
        .map_or(data.len(), |pos| pos + start);
    Some(Report::SerialNumber(
        String::from_utf8_lossy(&data[start..end]).into_owned(),
    ))
}

pub struct NothingDevice {
    pub att_manager: ATTManager,
}

impl NothingDevice {
    pub async fn new(
        mac_address: Address,
        _ui_tx: mpsc::UnboundedSender<BluetoothUIMessage>,
    ) -> Result<Self, NothingSetupError> {
        let mut att_manager = ATTManager::new();
        att_manager
            .connect(mac_address)
            .await
            .map_err(NothingSetupError::Connect)?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        att_manager
            .register_listener(ATTHandles::NothingEverythingRead, tx)
            .await;

        att_manager
            .write(ATTHandles::NothingEverything, &REQUEST_FIRMWARE_VERSION)
            .await
            .map_err(NothingSetupError::RequestFirmwareVersion)?;

        sleep(Duration::from_millis(100)).await;

        att_manager
            .write(ATTHandles::NothingEverything, &REQUEST_SERIAL_NUMBER)
            .await
            .map_err(NothingSetupError::RequestSerialNumber)?;

        let device_key = mac_address.to_string();
        tokio::spawn(async move {
            while let Some(data) = rx.recv().await {
                match parse_report(&data) {
                    Some(Report::FirmwareVersion(firmware_version)) => {
                        info!(
                            "Received firmware version from Nothing device {mac_address}: {firmware_version}"
                        );
                        save_nothing_info(device_key.clone(), move |info| {
                            info.firmware_version = firmware_version;
                        });
                    },
                    Some(Report::SerialNumber(serial_number)) => {
                        info!(
                            "Received serial number from Nothing device {mac_address}: {serial_number}"
                        );
                        save_nothing_info(device_key.clone(), move |info| {
                            info.serial_number = serial_number;
                        });
                    },
                    Some(Report::UnexpectedSerialNumber) => {
                        debug!(
                            "Serial number format unexpected from Nothing device {mac_address}: {data:?}"
                        );
                    },
                    None => {},
                }
                debug!("Received data from (Nothing) device {mac_address}, data: {data:?}");
            }
        });

        Ok(NothingDevice { att_manager })
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
            error!("Failed to save Nothing device information: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_prefix(prefix: [u8; 5], rest: &[u8]) -> Vec<u8> {
        [prefix.as_slice(), rest].concat()
    }

    #[test]
    fn firmware_version_is_the_text_after_the_header() {
        let data = with_prefix(FIRMWARE_VERSION_PREFIX, b"\x00\x00\x001.0.2.3");

        assert_eq!(
            parse_report(&data),
            Some(Report::FirmwareVersion("1.0.2.3".to_string()))
        );
    }

    #[test]
    fn firmware_version_header_without_text_is_empty_or_ignored() {
        let header_only = with_prefix(FIRMWARE_VERSION_PREFIX, &[0x00, 0x00, 0x00]);
        assert_eq!(
            parse_report(&header_only),
            Some(Report::FirmwareVersion(String::new()))
        );
        let short = with_prefix(FIRMWARE_VERSION_PREFIX, &[0x00]);
        assert_eq!(parse_report(&short), None);
    }

    #[test]
    fn serial_number_runs_from_sh_to_the_newline() {
        let data = with_prefix(SERIAL_NUMBER_PREFIX, b"\x00\x00\x00\x07SH12345\nrest");

        assert_eq!(
            parse_report(&data),
            Some(Report::SerialNumber("SH12345".to_string()))
        );
    }

    #[test]
    fn serial_number_without_newline_runs_to_the_end() {
        let data = with_prefix(SERIAL_NUMBER_PREFIX, b"\x00\x00\x00SH9");

        assert_eq!(
            parse_report(&data),
            Some(Report::SerialNumber("SH9".to_string()))
        );
    }

    #[test]
    fn serial_number_not_starting_with_sh_is_unexpected() {
        let wrong_letter = with_prefix(SERIAL_NUMBER_PREFIX, b"\x00\x00\x00SX123");
        assert_eq!(
            parse_report(&wrong_letter),
            Some(Report::UnexpectedSerialNumber)
        );
        let no_s = with_prefix(SERIAL_NUMBER_PREFIX, b"\x00\x00");
        assert_eq!(parse_report(&no_s), Some(Report::UnexpectedSerialNumber));
    }

    #[test]
    fn other_notifications_are_ignored() {
        assert_eq!(parse_report(&[]), None);
        assert_eq!(parse_report(&[0x55, 0x60, 0x01, 0x0F, 0xF0, 0x03]), None);
    }
}
