use {
    bluer::Adapter,
    tracing::{debug, info, warn},
    uuid::Uuid,
};

/// The AACP service UUID that AirPods advertise over SDP.
const AIRPODS_SERVICE_UUID: Uuid = Uuid::from_u128(0x74ec_2172_0bad_4d01_8f77_997b_2be0_722a);

/// Why no connected AirPods were returned.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DiscoveryError {
    #[error("listing the adapter's devices failed")]
    ListDevices(#[source] bluer::Error),
    #[error("no connected AirPods found")]
    NoAirPods,
}

pub(crate) async fn find_connected_airpods(
    adapter: &Adapter,
) -> Result<bluer::Device, DiscoveryError> {
    let addrs = adapter
        .device_addresses()
        .await
        .map_err(DiscoveryError::ListDevices)?;
    for addr in addrs {
        let device = match adapter.device(addr) {
            Ok(device) => device,
            Err(e) => {
                warn!("Skipping device {addr}: {e}");
                continue;
            },
        };
        if device.is_connected().await.unwrap_or(false)
            && let Ok(uuids) = device.uuids().await
            && let Some(uuids) = uuids
            && uuids.contains(&AIRPODS_SERVICE_UUID)
        {
            return Ok(device);
        }
    }
    Err(DiscoveryError::NoAirPods)
}

/// The connected devices among `managed_macs`, possibly none.
pub async fn find_other_managed_devices(
    adapter: &Adapter,
    managed_macs: Vec<String>,
) -> bluer::Result<Vec<bluer::Device>> {
    let addrs = adapter.device_addresses().await?;
    let mut devices = Vec::new();
    for addr in addrs {
        // One device that cannot be looked up must not hide the others.
        let device = match adapter.device(addr) {
            Ok(device) => device,
            Err(e) => {
                warn!("Skipping device {addr}: {e}");
                continue;
            },
        };
        let device_mac = device.address().to_string();
        let connected = device.is_connected().await.unwrap_or(false);
        debug!("Checking device: {device_mac}, connected: {connected}");
        if connected && managed_macs.contains(&device_mac) {
            debug!("Found managed device: {device_mac}");
            devices.push(device);
        }
    }
    if devices.is_empty() {
        info!("No other managed devices found.");
    }
    Ok(devices)
}
