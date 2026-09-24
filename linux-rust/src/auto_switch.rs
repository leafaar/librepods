//! Take the AirPods from another device when media starts playing on this PC.
//!
//! Apple devices hand AirPods over through iCloud. Here the takeover in
//! media_controller only runs while this PC holds a connection, so once the
//! AirPods move to the phone nothing brings them back. This watches the local
//! MPRIS players and, when playback starts, connects paired AirPods that are not
//! connected; the existing takeover path then claims audio as the device comes up.

use {
    crate::{
        audio::mpris::{self, DbusMediaPlayers, MediaPlayers},
        devices::enums::DeviceType,
        utils::{DevicesStore, SettingsStore},
    },
    bluer::{Adapter, Address},
    std::{
        collections::{HashMap, HashSet},
        str::FromStr,
        sync::{Arc, LazyLock, Mutex, PoisonError},
        time::Duration,
    },
    thiserror::Error,
    tokio::time::Instant,
    tracing::{debug, info, warn},
};

const POLL_INTERVAL: Duration = Duration::from_millis(750);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// After an attempt, wait this long before the next one, so AirPods that stay in
/// the case or refuse the connection are not retried on every play/pause.
const RETRY_COOLDOWN: Duration = Duration::from_secs(5);

/// How long a takeover request stays valid. Covers connecting plus AACP setup;
/// an older request must not grab the audio on some later, unrelated connect.
const TAKEOVER_REQUEST_TTL: Duration = Duration::from_secs(30);

/// Why connecting the AirPods failed. The UI shows the message as is, so it is
/// a sentence for the user, not a BlueZ code.
#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("Bluetooth is not available: {0}")]
    BluetoothUnavailable(bluer::Error),
    #[error("No Bluetooth adapter found: {0}")]
    NoAdapter(bluer::Error),
    #[error("Unknown device {addr}: {err}")]
    UnknownDevice { addr: Address, err: bluer::Error },
    #[error("Already connecting. Wait a few seconds and try again.")]
    InProgress,
    #[error(
        "The AirPods did not answer. Take them out of the case, pause audio on the other \
         device and stay near the PC, then try again."
    )]
    NotAnswering,
    #[error("Could not connect: {0}")]
    Failed(bluer::Error),
}

impl ConnectError {
    /// Turn a failed BlueZ connect into the advice that fits it.
    fn from_connect(err: bluer::Error) -> Self {
        let text = err.to_string();
        if text.contains("InProgress") || text.contains("busy") {
            Self::InProgress
        } else if text.contains("Host is down") || text.contains("page-timeout") {
            Self::NotAnswering
        } else {
            Self::Failed(err)
        }
    }
}

/// AirPods this PC connected on purpose, keyed by MAC, with when that was
/// asked. The media controller normally ignores media already playing when a
/// connection comes up (a plain reconnect must not steal the audio from another
/// device); for these it acts.
///
/// The UI and the tray ask for connections through `connect_airpods` and the
/// controller is created per connection afterwards, with no handle back to the
/// code that asked, so production code shares one registry from `shared()`.
/// Tests build their own with `default()`.
#[derive(Clone, Default)]
pub struct TakeoverRequests {
    requests: Arc<Mutex<HashMap<String, Instant>>>,
}

impl TakeoverRequests {
    pub fn shared() -> Self {
        static SHARED: LazyLock<TakeoverRequests> = LazyLock::new(TakeoverRequests::default);
        SHARED.clone()
    }

    // The map stays consistent under any panic (single inserts and removes),
    // so a poisoned lock is still usable.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        self.requests.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn request(&self, mac: &str) {
        self.lock().insert(mac.to_string(), Instant::now());
    }

    /// Whether a takeover of `mac` was requested within TAKEOVER_REQUEST_TTL.
    /// Expired requests are dropped here.
    pub fn has(&self, mac: &str) -> bool {
        let mut requests = self.lock();
        requests.retain(|_, at| at.elapsed() < TAKEOVER_REQUEST_TTL);
        requests.contains_key(mac)
    }

    /// Mark the takeover of `mac` as done.
    pub fn clear(&self, mac: &str) {
        self.lock().remove(mac);
    }
}

/// What the auto-switch loop reads and where it records its requests.
pub struct AutoSwitchDeps {
    pub players: Arc<dyn MediaPlayers>,
    pub devices: DevicesStore,
    pub settings: SettingsStore,
    pub takeovers: TakeoverRequests,
}

impl AutoSwitchDeps {
    /// The system's players, the default storage locations and the shared
    /// takeover registry.
    pub fn system() -> Self {
        Self {
            players: Arc::new(DbusMediaPlayers),
            devices: DevicesStore::default_location(),
            settings: SettingsStore::default_location(),
            takeovers: TakeoverRequests::shared(),
        }
    }
}

/// Detects a player starting to play between two polls. Tracked per player,
/// so a browser that stays "Playing" in the background does not hide a new
/// start in another player.
#[derive(Default)]
struct PlaybackStarts {
    was_playing: HashSet<String>,
}

impl PlaybackStarts {
    /// Record the players playing now; true if one of them was not before.
    fn update(&mut self, playing: HashSet<String>) -> bool {
        let started = playing.difference(&self.was_playing).next().is_some();
        self.was_playing = playing;
        started
    }
}

/// Spaces connection attempts RETRY_COOLDOWN apart.
#[derive(Default)]
struct RetryCooldown {
    last_attempt: Option<Instant>,
}

impl RetryCooldown {
    fn ready(&self, now: Instant) -> bool {
        self.last_attempt
            .is_none_or(|at| now.duration_since(at) >= RETRY_COOLDOWN)
    }

    fn attempted(&mut self, now: Instant) {
        self.last_attempt = Some(now);
    }
}

async fn players_playing(players: &Arc<dyn MediaPlayers>) -> HashSet<String> {
    let players = Arc::clone(players);
    tokio::task::spawn_blocking(move || mpris::playing(players.as_ref()))
        .await
        .ok()
        .and_then(|playing| {
            playing
                .inspect_err(|e| debug!("[switch] could not list media players: {}", e))
                .ok()
        })
        .unwrap_or_default()
        .into_iter()
        .collect()
}

pub async fn run(adapter: Adapter, deps: AutoSwitchDeps) {
    // Seed from the current state so media already playing at startup does not
    // count as a new start.
    let mut starts = PlaybackStarts::default();
    starts.update(players_playing(&deps.players).await);
    let mut cooldown = RetryCooldown::default();
    info!("[switch] watching local media playback");

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        // Act on the start of playback only, like an Apple device: media that was
        // already playing when the AirPods left does not pull them back.
        let started = starts.update(players_playing(&deps.players).await);
        if !started
            || !deps
                .settings
                .load()
                .unwrap_or_default()
                .auto_switch_on_playback
        {
            continue;
        }
        if !cooldown.ready(Instant::now()) {
            debug!("[switch] playback started, but still in retry cooldown");
            continue;
        }

        let mut attempted = false;
        for addr in known_airpods_in(&deps.devices) {
            attempted |= connect_if_away(&adapter, addr, &deps.takeovers).await;
        }
        if attempted {
            cooldown.attempted(Instant::now());
        }
    }
}

/// AirPods recorded in the devices file.
pub(crate) fn known_airpods() -> Vec<Address> {
    known_airpods_in(&DevicesStore::default_location())
}

/// AirPods recorded in `devices`. An unreadable file gives none.
fn known_airpods_in(devices: &DevicesStore) -> Vec<Address> {
    let devices = devices.load().unwrap_or_else(|e| {
        debug!("[switch] {}", e);
        HashMap::new()
    });
    devices
        .iter()
        .filter(|(_, d)| d.type_ == DeviceType::AirPods)
        .filter_map(|(mac, _)| Address::from_str(mac).ok())
        .collect()
}

/// Connect `addr` to this PC on request (UI button, tray item).
pub async fn connect_airpods(addr: Address) -> Result<(), ConnectError> {
    let session = bluer::Session::new()
        .await
        .map_err(ConnectError::BluetoothUnavailable)?;
    let adapter = session
        .default_adapter()
        .await
        .map_err(ConnectError::NoAdapter)?;
    info!("[switch] connect requested for AirPods {}", addr);
    connect_device(&adapter, addr, &TakeoverRequests::shared()).await
}

async fn connect_device(
    adapter: &Adapter,
    addr: Address,
    takeovers: &TakeoverRequests,
) -> Result<(), ConnectError> {
    let device = adapter
        .device(addr)
        .map_err(|err| ConnectError::UnknownDevice { addr, err })?;
    if device.is_connected().await.unwrap_or(false) {
        return Ok(());
    }
    let mac = addr.to_string();
    takeovers.request(&mac);
    let result = match tokio::time::timeout(CONNECT_TIMEOUT, device.connect()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(ConnectError::from_connect(e)),
        Err(_) => Err(ConnectError::NotAnswering),
    };
    finish_connect(takeovers, &mac, result)
}

/// Log the outcome of a requested connect. A failed one drops the takeover
/// request: otherwise a later reconnect the user did not ask for would still
/// take the audio.
fn finish_connect(
    takeovers: &TakeoverRequests,
    mac: &str,
    result: Result<(), ConnectError>,
) -> Result<(), ConnectError> {
    match &result {
        Ok(()) => info!("[switch] connected {}", mac),
        Err(e) => {
            warn!("[switch] could not connect {}: {}", mac, e);
            takeovers.clear(mac);
        },
    }
    result
}

/// Connect `addr` if it is paired but not connected here. Returns whether a
/// connection was attempted.
async fn connect_if_away(adapter: &Adapter, addr: Address, takeovers: &TakeoverRequests) -> bool {
    let Ok(device) = adapter.device(addr) else {
        return false;
    };
    match (device.is_paired().await, device.is_connected().await) {
        (Ok(true), Ok(false)) => {},
        _ => return false,
    }

    info!(
        "[switch] media started on this PC, connecting AirPods {}",
        addr
    );
    let _ = connect_device(adapter, addr, takeovers).await;
    true
}

#[cfg(test)]
mod tests {
    use {super::*, crate::devices::enums::DeviceData, bluer::ErrorKind, tempfile::TempDir};

    const MAC: &str = "AA:BB:CC:DD:EE:FF";

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(ToString::to_string).collect()
    }

    fn bluez_error(message: &str) -> bluer::Error {
        bluer::Error {
            kind: ErrorKind::Failed,
            message: message.to_string(),
        }
    }

    #[test]
    fn a_player_that_starts_counts_even_while_another_keeps_playing() {
        let mut starts = PlaybackStarts::default();
        assert!(!starts.update(set(&[])));

        assert!(starts.update(set(&["browser"])));
        assert!(!starts.update(set(&["browser"])));
        assert!(starts.update(set(&["browser", "spotify"])));
    }

    #[test]
    fn stopping_or_staying_is_not_a_start() {
        let mut starts = PlaybackStarts::default();
        starts.update(set(&["spotify"]));

        assert!(!starts.update(set(&["spotify"])));
        assert!(!starts.update(set(&[])));
        assert!(starts.update(set(&["spotify"])));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_waits_for_the_cooldown() {
        let mut cooldown = RetryCooldown::default();
        assert!(cooldown.ready(Instant::now()));

        cooldown.attempted(Instant::now());
        tokio::time::advance(
            RETRY_COOLDOWN
                .checked_sub(Duration::from_millis(1))
                .unwrap(),
        )
        .await;
        assert!(!cooldown.ready(Instant::now()));

        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(cooldown.ready(Instant::now()));
    }

    #[tokio::test(start_paused = true)]
    async fn takeover_request_expires_after_its_ttl() {
        let takeovers = TakeoverRequests::default();
        takeovers.request(MAC);

        tokio::time::advance(
            TAKEOVER_REQUEST_TTL
                .checked_sub(Duration::from_secs(1))
                .unwrap(),
        )
        .await;
        assert!(takeovers.has(MAC));
        assert!(!takeovers.has("11:22:33:44:55:66"));

        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!takeovers.has(MAC));
    }

    #[tokio::test]
    async fn failed_connect_clears_the_takeover_request() {
        let takeovers = TakeoverRequests::default();
        takeovers.request(MAC);

        let result = finish_connect(&takeovers, MAC, Err(ConnectError::NotAnswering));

        assert!(result.is_err());
        assert!(!takeovers.has(MAC));
    }

    #[tokio::test]
    async fn successful_connect_keeps_the_takeover_request() {
        let takeovers = TakeoverRequests::default();
        takeovers.request(MAC);

        finish_connect(&takeovers, MAC, Ok(())).unwrap();

        assert!(takeovers.has(MAC));
    }

    #[test]
    fn bluez_failures_become_advice() {
        assert!(matches!(
            ConnectError::from_connect(bluez_error("org.bluez.Error.InProgress")),
            ConnectError::InProgress
        ));
        assert!(matches!(
            ConnectError::from_connect(bluez_error("Host is down")),
            ConnectError::NotAnswering
        ));
        assert!(matches!(
            ConnectError::from_connect(bluez_error("br-connection-page-timeout")),
            ConnectError::NotAnswering
        ));
        let other = ConnectError::from_connect(bluez_error("something else"));
        assert!(matches!(other, ConnectError::Failed(_)));
        assert!(other.to_string().starts_with("Could not connect: "));
        assert!(other.to_string().contains("something else"));
    }

    #[test]
    fn known_airpods_come_from_the_devices_file() {
        let dir = TempDir::new().unwrap();
        let store = DevicesStore::new(dir.path().join("devices.json"));
        store
            .update(|devices| {
                for (mac, type_) in [
                    (MAC, DeviceType::AirPods),
                    ("11:22:33:44:55:66", DeviceType::Nothing),
                    ("not a mac", DeviceType::AirPods),
                ] {
                    devices.insert(
                        mac.to_string(),
                        DeviceData {
                            name: String::new(),
                            type_,
                            information: None,
                        },
                    );
                }
            })
            .unwrap();

        let airpods = known_airpods_in(&store);

        assert_eq!(airpods, [Address::from_str(MAC).unwrap()]);
    }

    #[test]
    fn missing_devices_file_means_no_known_airpods() {
        let dir = TempDir::new().unwrap();
        let store = DevicesStore::new(dir.path().join("devices.json"));

        assert!(known_airpods_in(&store).is_empty());
    }
}
