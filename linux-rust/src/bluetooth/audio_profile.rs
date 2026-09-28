//! Asks BlueZ to connect the AirPods' audio profile. AirPods put in the case
//! drop A2DP but keep the ACL link, and when they come out again only AACP
//! comes back: the sound server then has no card for them until something
//! connects the profile.

use {
    dbus::{
        Path,
        arg::RefArg,
        blocking::{Connection, stdintf::org_freedesktop_dbus::ObjectManager},
    },
    std::time::Duration,
    thiserror::Error,
};

/// A2DP sink, the profile that carries the music to the AirPods.
const A2DP_SINK_UUID: &str = "0000110b-0000-1000-8000-00805f9b34fb";
/// Connecting a profile waits for the AirPods to answer.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum AudioProfileError {
    #[error("cannot reach BlueZ on the system bus: {0}")]
    Bus(#[source] dbus::Error),
    #[error("BlueZ does not know the device {0}")]
    UnknownDevice(String),
    #[error("BlueZ could not connect the audio profile: {0}")]
    Connect(#[source] dbus::Error),
    #[error("the audio profile request was cancelled")]
    Cancelled,
}

/// Connects the audio profile of a Bluetooth device.
pub trait AudioProfile: Send + Sync {
    /// Connect the A2DP sink profile of the device with this MAC. Blocks until
    /// BlueZ answers.
    fn connect_a2dp(&self, mac: &str) -> Result<(), AudioProfileError>;
}

/// BlueZ over the system bus.
pub struct BluezAudioProfile;

impl AudioProfile for BluezAudioProfile {
    fn connect_a2dp(&self, mac: &str) -> Result<(), AudioProfileError> {
        let conn = Connection::new_system().map_err(AudioProfileError::Bus)?;
        let path = device_path(&conn, mac)?;
        conn.with_proxy("org.bluez", path, CALL_TIMEOUT)
            .method_call::<(), _, _, _>("org.bluez.Device1", "ConnectProfile", (A2DP_SINK_UUID,))
            .map_err(AudioProfileError::Connect)
    }
}

/// The device's object path, on whichever adapter it is paired with.
fn device_path(conn: &Connection, mac: &str) -> Result<Path<'static>, AudioProfileError> {
    let objects = conn
        .with_proxy("org.bluez", "/", CALL_TIMEOUT)
        .get_managed_objects()
        .map_err(AudioProfileError::Bus)?;
    objects
        .into_iter()
        .find(|(_, interfaces)| {
            interfaces
                .get("org.bluez.Device1")
                .and_then(|props| props.get("Address"))
                .and_then(|address| address.0.as_str())
                .is_some_and(|address| address.eq_ignore_ascii_case(mac))
        })
        .map(|(path, _)| path)
        .ok_or_else(|| AudioProfileError::UnknownDevice(mac.to_string()))
}
