//! Take the AirPods from another device when media starts playing on this PC.
//!
//! Apple devices hand AirPods over through iCloud. Here the takeover in
//! media_controller only runs while this PC holds a connection, so once the
//! AirPods move to the phone nothing brings them back. This watches the local
//! MPRIS players and, when playback starts, connects paired AirPods that are not
//! connected; the existing takeover path then claims audio as the device comes up.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::str::FromStr;
use std::time::{Duration, Instant};

use bluer::{Adapter, Address};
use log::{debug, info, warn};

use crate::audio::output::playing_media_players;
use crate::devices::enums::{DeviceData, DeviceType};
use crate::utils::{AppSettings, get_devices_path};

const POLL_INTERVAL: Duration = Duration::from_millis(750);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// After an attempt, wait this long before the next one, so AirPods that stay in
/// the case or refuse the connection are not retried on every play/pause.
const RETRY_COOLDOWN: Duration = Duration::from_secs(5);

/// How long a takeover request stays valid. Covers connecting plus AACP setup;
/// an older request must not grab the audio on some later, unrelated connect.
const TAKEOVER_REQUEST_TTL: Duration = Duration::from_secs(30);

/// AirPods this PC connected on purpose, keyed by MAC. The media controller
/// normally ignores media already playing when a connection comes up (a plain
/// reconnect must not steal the audio from another device); for these it acts.
/// A static because the controller is created per connection, after the
/// request, and has no handle back to the code that asked for it.
static TAKEOVER_REQUESTS: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn request_takeover(addr: Address) {
    if let Ok(mut requests) = TAKEOVER_REQUESTS.lock() {
        requests.insert(addr.to_string(), Instant::now());
    }
}

/// Consume the takeover request for `mac`; true when one was made recently.
pub(crate) fn take_takeover_request(mac: &str) -> bool {
    TAKEOVER_REQUESTS
        .lock()
        .ok()
        .and_then(|mut requests| requests.remove(mac))
        .is_some_and(|at| at.elapsed() < TAKEOVER_REQUEST_TTL)
}

fn players_playing() -> HashSet<String> {
    playing_media_players().into_iter().collect()
}

pub async fn run(adapter: Adapter) {
    // Seed from the current state so media already playing at startup does not
    // count as a new start.
    let mut was_playing = tokio::task::spawn_blocking(players_playing)
        .await
        .unwrap_or_default();
    let mut last_attempt: Option<Instant> = None;
    info!("[switch] watching local media playback");

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        let playing = tokio::task::spawn_blocking(players_playing)
            .await
            .unwrap_or_default();
        // Act on the start of playback only, like an Apple device: media that was
        // already playing when the AirPods left does not pull them back. Tracked
        // per player, so a browser that stays "Playing" in the background does not
        // hide a new start in another player.
        let started = playing.difference(&was_playing).next().is_some();
        was_playing = playing;
        if !started || !AppSettings::load().auto_switch_on_playback {
            continue;
        }
        if last_attempt.is_some_and(|t| t.elapsed() < RETRY_COOLDOWN) {
            debug!("[switch] playback started, but still in retry cooldown");
            continue;
        }

        let mut attempted = false;
        for addr in known_airpods() {
            attempted |= connect_if_away(&adapter, addr).await;
        }
        if attempted {
            last_attempt = Some(Instant::now());
        }
    }
}

/// AirPods recorded in the devices file.
pub(crate) fn known_airpods() -> Vec<Address> {
    let Ok(json) = std::fs::read_to_string(get_devices_path()) else {
        return Vec::new();
    };
    let devices: HashMap<String, DeviceData> = serde_json::from_str(&json).unwrap_or_default();
    devices
        .iter()
        .filter(|(_, d)| d.type_ == DeviceType::AirPods)
        .filter_map(|(mac, _)| Address::from_str(mac).ok())
        .collect()
}

/// Connect `addr` to this PC on request (UI button, tray item). The error is a
/// sentence for the user, not a BlueZ code.
pub async fn connect_airpods(addr: Address) -> Result<(), String> {
    let session = bluer::Session::new()
        .await
        .map_err(|e| format!("Bluetooth is not available: {e}"))?;
    let adapter = session
        .default_adapter()
        .await
        .map_err(|e| format!("No Bluetooth adapter found: {e}"))?;
    info!("[switch] connect requested for AirPods {}", addr);
    connect_device(&adapter, addr).await
}

async fn connect_device(adapter: &Adapter, addr: Address) -> Result<(), String> {
    let device = adapter
        .device(addr)
        .map_err(|e| format!("Unknown device {addr}: {e}"))?;
    if device.is_connected().await.unwrap_or(false) {
        return Ok(());
    }
    request_takeover(addr);
    let result = match tokio::time::timeout(CONNECT_TIMEOUT, device.connect()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(explain_connect_error(&e.to_string())),
        Err(_) => Err(NOT_ANSWERING.to_string()),
    };
    match &result {
        Ok(()) => info!("[switch] connected {}", addr),
        Err(e) => warn!("[switch] could not connect {}: {}", addr, e),
    }
    result
}

const NOT_ANSWERING: &str = "The AirPods did not answer. Take them out of the case, pause \
     audio on the other device and stay near the PC, then try again.";

fn explain_connect_error(err: &str) -> String {
    if err.contains("InProgress") || err.contains("busy") {
        "Already connecting. Wait a few seconds and try again.".to_string()
    } else if err.contains("Host is down") || err.contains("page-timeout") {
        NOT_ANSWERING.to_string()
    } else {
        format!("Could not connect: {err}")
    }
}

/// Connect `addr` if it is paired but not connected here. Returns whether a
/// connection was attempted.
async fn connect_if_away(adapter: &Adapter, addr: Address) -> bool {
    let Ok(device) = adapter.device(addr) else {
        return false;
    };
    match (device.is_paired().await, device.is_connected().await) {
        (Ok(true), Ok(false)) => {}
        _ => return false,
    }

    info!(
        "[switch] media started on this PC, connecting AirPods {}",
        addr
    );
    let _ = connect_device(adapter, addr).await;
    true
}
