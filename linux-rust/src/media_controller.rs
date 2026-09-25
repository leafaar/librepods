use {
    crate::{
        audio::{
            mpris::{self, DbusMediaPlayers, MediaPlayerError, MediaPlayers, PlayerCommand},
            pulse::{CardProfiles, PulseSoundServer, SoundServer, SoundServerError},
        },
        auto_switch::TakeoverRequests,
        bluetooth::aacp::{AACPManager, ControlCommandIdentifiers, EarDetectionStatus},
        utils::SettingsStore,
    },
    std::{
        collections::HashMap,
        sync::{Arc, LazyLock, Mutex as StdMutex, PoisonError},
        time::Duration,
    },
    tokio::{
        sync::{Mutex, mpsc::UnboundedSender},
        task::AbortHandle,
        time::Instant,
    },
    tracing::{debug, error, info, warn},
};

/// How long after our own pause a player resuming is still attributed to us.
const OWN_PAUSE_WINDOW: Duration = Duration::from_secs(10);
/// How long after the buds came out putting them back resumes local media.
const RESUME_WINDOW: Duration = Duration::from_secs(120);
/// How often the playback listener polls the local players.
const PLAYBACK_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Number of one-second attempts to wait for A2DP profiles to be enumerated.
const A2DP_ENUMERATION_ATTEMPTS: u32 = 5;
const A2DP_ENUMERATION_INTERVAL: Duration = Duration::from_secs(1);
/// Taking the connection back from another device makes the earbuds
/// renegotiate, and the card briefly disappears from the sound server while
/// that happens, so a card lookup is retried this often, this far apart.
const CARD_LOOKUP_ATTEMPTS: u32 = 12;
const CARD_LOOKUP_INTERVAL: Duration = Duration::from_millis(250);

type ControlSender = UnboundedSender<(ControlCommandIdentifiers, Vec<u8>)>;

/// Playback listener tasks by device MAC. Reconnecting AirPods creates a fresh
/// MediaController, so the listener is registered outside the controller and
/// the replacement stops its predecessor by MAC. Production code shares one
/// registry from `shared()`; tests build their own with `default()`.
#[derive(Clone, Default)]
pub struct PlaybackListeners {
    tasks: Arc<StdMutex<HashMap<String, AbortHandle>>>,
}

impl PlaybackListeners {
    pub fn shared() -> Self {
        static SHARED: LazyLock<PlaybackListeners> = LazyLock::new(PlaybackListeners::default);
        SHARED.clone()
    }

    /// Abort the listener registered for `mac`, if any, and register the one
    /// `spawn` starts in its place.
    fn replace(&self, mac: &str, spawn: impl FnOnce() -> AbortHandle) -> AbortHandle {
        // Single inserts and removes leave the map consistent under any panic,
        // so a poisoned lock is still usable.
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(previous) = tasks.remove(mac) {
            previous.abort();
        }
        let handle = spawn();
        tasks.insert(mac.to_string(), handle.clone());
        handle
    }
}

/// Everything the controller reaches outside the process through.
#[derive(Clone)]
pub struct MediaDeps {
    pub sound: Arc<dyn SoundServer>,
    pub players: Arc<dyn MediaPlayers>,
    pub settings: SettingsStore,
    pub takeovers: TakeoverRequests,
    pub listeners: PlaybackListeners,
}

impl MediaDeps {
    /// The system's sound server and media players, the default settings file
    /// and the shared registries.
    pub fn system() -> Self {
        Self {
            sound: Arc::new(PulseSoundServer),
            players: Arc::new(DbusMediaPlayers),
            settings: SettingsStore::default_location(),
            takeovers: TakeoverRequests::shared(),
            listeners: PlaybackListeners::shared(),
        }
    }
}

#[derive(Default)]
struct MediaControllerState {
    is_playing: bool,
    paused_by_app_services: Vec<String>,
    paused_by_app_at: Option<Instant>,
    cached_a2dp_profile: Option<String>,
    /// When this controller last paused local players itself. Players tend to
    /// resume once the audio profile comes back; within OWN_PAUSE_WINDOW that
    /// resume is ours, not the user asking for the audio.
    i_paused_the_media_at: Option<Instant>,
    conv_original_volume: Option<u32>,
    conv_conversation_started: bool,
}

/// The playback listener's view of the local players between two polls.
#[derive(Default)]
struct PlaybackWatch {
    baseline_taken: bool,
}

impl PlaybackWatch {
    /// Record one poll and decide whether it asks to take the audio (the
    /// caller still checks that a bud is in an ear). `requested` is whether
    /// this PC connected the AirPods on purpose to play here.
    fn observe(
        &mut self,
        state: &mut MediaControllerState,
        is_playing: bool,
        requested: bool,
        now: Instant,
    ) -> bool {
        let was_playing = std::mem::replace(&mut state.is_playing, is_playing);

        // `is_playing` starts false, which is a placeholder rather than an
        // observation. Whatever is already playing when the listener starts
        // would otherwise read as playback that just began, and on a
        // reconnect, with another device holding the audio, that escalates
        // into taking it away from a device the user was happily listening on.
        if !self.baseline_taken {
            self.baseline_taken = true;
            // Exception: this PC connected the AirPods itself to play here
            // (auto-switch or the connect button), so media already playing
            // is exactly what should take the audio.
            if !(is_playing && requested) {
                debug!("Recorded initial playback state ({is_playing}); not a transition");
                return false;
            }
            info!("Connection was requested to play here; taking ownership");
        }

        let started = is_playing && !was_playing;
        // Losing ownership pauses local players and drops the audio profile.
        // Players tend to resume on their own once the profile comes back, and
        // that resume is not the user asking for the audio: treating it as one
        // starts a tug of war with the device that just took over, ending with
        // neither side playing.
        if started
            && state
                .i_paused_the_media_at
                .take()
                .is_some_and(|at| now.duration_since(at) < OWN_PAUSE_WINDOW)
        {
            debug!("Playback resumed after our own pause; not taking ownership");
            return false;
        }

        // A requested takeover is retried on every poll until it runs or
        // expires: the first readings can come before the ear status does.
        started || (is_playing && requested)
    }
}

#[derive(Clone)]
pub struct MediaController {
    mac: Arc<str>,
    local_mac: Arc<str>,
    state: Arc<Mutex<MediaControllerState>>,
    deps: MediaDeps,
}

impl MediaController {
    pub fn new(connected_mac: String, local_mac: String) -> Self {
        Self::with_deps(connected_mac, local_mac, MediaDeps::system())
    }

    pub fn with_deps(connected_mac: String, local_mac: String, deps: MediaDeps) -> Self {
        MediaController {
            mac: connected_mac.into(),
            local_mac: local_mac.into(),
            state: Arc::new(Mutex::new(MediaControllerState::default())),
            deps,
        }
    }

    /// Run `f` against the sound server on the blocking pool.
    async fn sound<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn SoundServer) -> Result<T, SoundServerError> + Send + 'static,
    ) -> Result<T, SoundServerError> {
        let sound = Arc::clone(&self.deps.sound);
        tokio::task::spawn_blocking(move || f(sound.as_ref()))
            .await
            .map_err(|_| SoundServerError::OperationCancelled)?
    }

    /// Run `f` against the media players on the blocking pool.
    async fn players<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn MediaPlayers) -> Result<T, MediaPlayerError> + Send + 'static,
    ) -> Result<T, MediaPlayerError> {
        let players = Arc::clone(&self.deps.players);
        tokio::task::spawn_blocking(move || f(players.as_ref()))
            .await
            .map_err(|_| MediaPlayerError::Interrupted)?
    }

    /// Start polling the local players and take the audio when playback
    /// starts here. Replaces the listener of an earlier controller for the
    /// same device.
    pub fn start_playback_listener(
        &self,
        aacp_manager: AACPManager,
        control_tx: ControlSender,
    ) -> AbortHandle {
        self.deps.listeners.replace(&self.mac, || {
            tokio::spawn(
                self.clone()
                    .playback_listener_loop(aacp_manager, control_tx),
            )
            .abort_handle()
        })
    }

    async fn playback_listener_loop(self, aacp_manager: AACPManager, control_tx: ControlSender) {
        info!("Starting playback listener loop");
        let mut watch = PlaybackWatch::default();
        loop {
            tokio::time::sleep(PLAYBACK_POLL_INTERVAL).await;

            let is_playing = self.any_player_playing().await;
            let requested = is_playing && self.deps.takeovers.has(&self.mac);
            let take = watch.observe(
                &mut *self.state.lock().await,
                is_playing,
                requested,
                Instant::now(),
            );
            if take {
                self.take_over(&aacp_manager, &control_tx).await;
            }
        }
    }

    async fn any_player_playing(&self) -> bool {
        self.players(|p| p.players())
            .await
            .inspect_err(|e| debug!("Could not check the media players: {}", e))
            .is_ok_and(|players| players.iter().any(|p| p.playing))
    }

    /// Claim the audio for this PC if a bud is in an ear. Without the ear
    /// status the takeover request is kept, so the next poll tries again.
    async fn take_over(&self, aacp_manager: &AACPManager, control_tx: &ControlSender) {
        let (bud_in_ear, connected_devices) = {
            let aacp_state = aacp_manager.state.lock().await;
            (
                aacp_state
                    .ear_detection_status
                    .contains(&EarDetectionStatus::InEar),
                aacp_state.connected_devices.clone(),
            )
        };
        if !bud_in_ear {
            info!("Media playback started but buds not in ear, skipping takeover");
            return;
        }
        self.deps.takeovers.clear(&self.mac);
        info!("Media playback started, taking ownership and activating a2dp");
        let _ = control_tx.send((ControlCommandIdentifiers::OwnsConnection, vec![0x01]));
        self.activate_a2dp_profile().await;

        info!("already connected locally, hijacking connection by asking AirPods");
        let local_mac = &*self.local_mac;
        for device in connected_devices.iter().filter(|d| d.mac != local_mac) {
            if let Err(e) = aacp_manager
                .send_media_information(local_mac, &device.mac, true)
                .await
            {
                error!("Failed to send media information to {}: {}", device.mac, e);
            }
            if let Err(e) = aacp_manager.send_smart_routing_show_ui(&device.mac).await {
                error!(
                    "Failed to send smart routing show ui to {}: {}",
                    device.mac, e
                );
            }
            if let Err(e) = aacp_manager.send_hijack_request(&device.mac).await {
                error!("Failed to send hijack request to {}: {}", device.mac, e);
            }
        }

        debug!("completed playback takeover process");
    }

    pub async fn handle_ear_detection(
        &self,
        old_statuses: Vec<EarDetectionStatus>,
        new_statuses: Vec<EarDetectionStatus>,
    ) {
        debug!(
            "Entering handle_ear_detection with old_statuses: {:?}, new_statuses: {:?}",
            old_statuses, new_statuses
        );

        // No previous reading means this is the first report after connecting,
        // not a change the wearer made. It matters because "all out" is
        // vacuously true for an empty list, so an already-worn pair looks like
        // it was just put in: playback resumes, and with a second device
        // connected the resume escalates into taking the audio away from it.
        // Wait for a real transition instead.
        if old_statuses.is_empty() {
            debug!("First ear reading after connecting: recording baseline, not acting");
            return;
        }

        let in_ear = |statuses: &[EarDetectionStatus]| -> Vec<bool> {
            statuses
                .iter()
                .map(|s| *s == EarDetectionStatus::InEar)
                .collect()
        };
        let mut old_in_ear = in_ear(&old_statuses);
        let mut new_in_ear = in_ear(&new_statuses);
        let old_all_out = old_in_ear.iter().all(|&b| !b);
        let new_all_in = new_in_ear.iter().all(|&b| b);
        let new_any_in = new_in_ear.iter().any(|&b| b);

        if new_any_in && old_all_out {
            debug!("Buds inserted, activating A2DP");
            self.activate_a2dp_profile().await;
        } else if !new_any_in {
            debug!("Buds removed, pausing media and deactivating A2DP");
            self.pause().await;
            self.deactivate_a2dp_profile().await;
        }

        info!(
            "Ear Detection - old_in_ear_data: {:?}, new_in_ear_data: {:?}",
            old_in_ear, new_in_ear
        );

        // Which bud is which does not matter, only how many are in.
        old_in_ear.sort_unstable();
        new_in_ear.sort_unstable();
        if new_in_ear != old_in_ear {
            if new_all_in || old_all_out {
                debug!("Buds went in, resuming media");
                self.resume().await;
                self.state.lock().await.i_paused_the_media_at = None;
            } else {
                debug!("Pausing media as buds are not fully in ear");
                self.pause().await;
            }
        }
    }

    /// Resolve the card by MAC, retrying while the earbuds renegotiate.
    async fn find_card_with_retry(&self) -> Option<u32> {
        // A single lookup can land in the renegotiation window and give up,
        // leaving the audio nowhere: ownership already taken from the other
        // device, but no local profile to play through.
        for attempt in 1..=CARD_LOOKUP_ATTEMPTS {
            if let Ok(index) = self.find_card().await {
                if attempt > 1 {
                    debug!(
                        "Found audio device for {} after {attempt} attempts",
                        self.mac
                    );
                }
                info!("Found audio device index for MAC {}: {}", self.mac, index);
                return Some(index);
            }
            if attempt < CARD_LOOKUP_ATTEMPTS {
                tokio::time::sleep(CARD_LOOKUP_INTERVAL).await;
            }
        }
        error!(
            "No matching Bluetooth card found for MAC address: {} after {:?}",
            self.mac,
            CARD_LOOKUP_INTERVAL * CARD_LOOKUP_ATTEMPTS
        );
        None
    }

    /// One lookup of the card by MAC. PipeWire can assign a different index
    /// after a disconnect and reuse old ones, so a cached index is never used.
    async fn find_card(&self) -> Result<u32, SoundServerError> {
        let mac = self.mac.to_string();
        self.sound(move |s| s.find_card(&mac)).await
    }

    async fn card_profiles(&self, card: u32) -> Result<CardProfiles, SoundServerError> {
        self.sound(move |s| s.card_profiles(card)).await
    }

    async fn has_a2dp_sink(&self, card: u32) -> bool {
        let available = self
            .card_profiles(card)
            .await
            .is_ok_and(|p| p.has_a2dp_sink());
        debug!("A2DP profile available: {}", available);
        available
    }

    /// Waits for the card to expose an A2DP profile, re-resolving the card on
    /// every attempt, and returns its index. Sleeps between attempts, but never
    /// after the last one. Each attempt looks the card up once: this loop is
    /// the retry, and nesting the retrying lookup inside it would stretch the
    /// wait to tens of seconds.
    async fn wait_for_a2dp_profile(&self) -> Option<u32> {
        for attempt in 0..A2DP_ENUMERATION_ATTEMPTS {
            if let Ok(index) = self.find_card().await
                && self.has_a2dp_sink(index).await
            {
                return Some(index);
            }
            if attempt + 1 < A2DP_ENUMERATION_ATTEMPTS {
                tokio::time::sleep(A2DP_ENUMERATION_INTERVAL).await;
            }
        }
        None
    }

    /// Resolve the card and make sure it exposes an A2DP profile, waiting for
    /// enumeration. The session manager is never restarted here: that tears
    /// down every audio stream on the system, including a call in progress.
    async fn card_with_a2dp(&self) -> Option<u32> {
        // Always resolve the card by MAC first: a reconnect can change its index.
        let Some(index) = self.find_card_with_retry().await else {
            warn!("Could not get device index. Cannot activate A2DP profile.");
            return None;
        };
        if self.has_a2dp_sink(index).await {
            return Some(index);
        }
        // A freshly connected card can show up before its profiles are
        // enumerated, so give that a grace period.
        warn!("A2DP profile not available yet, waiting for enumeration");
        if let Some(index) = self.wait_for_a2dp_profile().await {
            return Some(index);
        }
        error!("A2DP profile unavailable, skipping profile activation");
        None
    }

    pub async fn activate_a2dp_profile(&self) {
        debug!("Entering activate_a2dp_profile");

        if self.mac.is_empty() {
            warn!("Connected device MAC is empty, cannot activate A2DP profile");
            return;
        }
        let Some(index) = self.card_with_a2dp().await else {
            return;
        };
        let profiles = match self.card_profiles(index).await {
            Ok(profiles) => profiles,
            Err(e) => {
                error!("Could not read the profiles of card {}: {}", index, e);
                return;
            },
        };

        // Leave an already-active A2DP variant alone. Switching profiles
        // recreates the PipeWire sink and stops the stream that triggered this
        // activation, so players pause again right after the user pressed play.
        if let Some(active) = &profiles.active
            && active.starts_with("a2dp")
        {
            debug!(
                "A2DP profile {} already active, leaving it unchanged",
                active
            );
            return;
        }

        let Some(profile) = self.preferred_a2dp_profile(&profiles).await else {
            error!("No suitable A2DP profile found");
            return;
        };

        info!("Activating A2DP profile for AirPods: {}", profile);
        let requested = profile.clone();
        match self
            .sound(move |s| s.set_card_profile(index, &requested))
            .await
        {
            Ok(()) => info!("Successfully activated A2DP profile: {}", profile),
            Err(e) => warn!("Failed to activate A2DP profile {}: {}", profile, e),
        }
    }

    /// The profile last chosen if the card still has it, otherwise the first
    /// available one in the order of the preferred codec setting.
    async fn preferred_a2dp_profile(&self, profiles: &CardProfiles) -> Option<String> {
        let cached = self.state.lock().await.cached_a2dp_profile.clone();
        if let Some(cached) = cached
            && profiles.has(&cached)
        {
            debug!("Using cached A2DP profile: {}", cached);
            return Some(cached);
        }

        let codec = self
            .deps
            .settings
            .load()
            .unwrap_or_default()
            .preferred_codec;
        let Some(profile) = codec.profile_order().into_iter().find(|p| profiles.has(p)) else {
            debug!("No suitable profile found");
            return None;
        };
        info!("Selected best available A2DP profile: {}", profile);
        self.state.lock().await.cached_a2dp_profile = Some(profile.to_string());
        Some(profile.to_string())
    }

    pub async fn deactivate_a2dp_profile(&self) {
        debug!("Entering deactivate_a2dp_profile");
        // Resolve by MAC every time: card indices are reused, so a cached one
        // can belong to another sound card by now.
        let index = match self.find_card().await {
            Ok(index) => index,
            Err(e) => {
                warn!("Cannot deactivate A2DP profile: {}", e);
                return;
            },
        };

        info!("Deactivating A2DP profile for AirPods by setting to off");
        match self.sound(move |s| s.set_card_profile(index, "off")).await {
            Ok(()) => info!("Successfully deactivated A2DP profile"),
            Err(e) => warn!("Failed to deactivate A2DP profile: {}", e),
        }
    }

    async fn pause(&self) {
        debug!("Pausing playback");
        let paused = self
            .players(mpris::pause_playing)
            .await
            .unwrap_or_else(|e| {
                warn!("Could not pause media players: {}", e);
                Vec::new()
            });

        if paused.is_empty() {
            info!("No playing media players found to pause");
            return;
        }
        info!("Paused {} media player(s) via DBus", paused.len());
        let now = Instant::now();
        let mut state = self.state.lock().await;
        state.paused_by_app_services = paused;
        state.paused_by_app_at = Some(now);
        state.i_paused_the_media_at = Some(now);
        state.is_playing = false;
    }

    /// Pause every playing player without remembering them for a resume, as
    /// when another device takes the audio.
    pub async fn pause_all_media(&self) {
        debug!("Pausing all media (without tracking for resume)");
        let paused = self.players(mpris::pause_playing).await.map_or_else(
            |e| {
                warn!("Could not pause media players: {}", e);
                0
            },
            |paused| paused.len(),
        );

        if paused == 0 {
            debug!("No playing media players found to pause");
            return;
        }
        // Remember that the pause came from us, so the playback listener does
        // not mistake the players coming back for the user.
        let mut state = self.state.lock().await;
        state.i_paused_the_media_at = Some(Instant::now());
        state.is_playing = false;
        info!("Paused {} media player(s) due to ownership loss", paused);
    }

    async fn resume(&self) {
        debug!("Resuming playback");
        // Take the list whatever happens next: a resume is attempted once.
        let (services, paused_at) = {
            let mut state = self.state.lock().await;
            (
                std::mem::take(&mut state.paused_by_app_services),
                state.paused_by_app_at.take(),
            )
        };

        if services.is_empty() {
            debug!("No services to resume");
            return;
        }
        // Putting the buds back in much later is not "continue what I was
        // doing here": the user may be on another device by now.
        if paused_at.is_none_or(|at| at.elapsed() >= RESUME_WINDOW) {
            debug!("Media was paused too long ago; not resuming");
            return;
        }

        let resumed = self
            .players(move |players| {
                let mut resumed = 0;
                for service in services {
                    match players.send(&service, PlayerCommand::Play) {
                        Ok(()) => {
                            info!("Resumed playback for: {}", service);
                            resumed += 1;
                        },
                        Err(e) => warn!("Failed to resume: {}", e),
                    }
                }
                Ok(resumed)
            })
            .await
            .unwrap_or(0);

        if resumed > 0 {
            info!("Resumed {} media player(s) via DBus", resumed);
        } else {
            error!("Failed to resume any media players via DBus");
        }
    }

    pub async fn next_track(&self) {
        info!("Skipping to next track");
        self.player_command(PlayerCommand::Next).await;
    }

    pub async fn previous_track(&self) {
        info!("Going to previous track");
        self.player_command(PlayerCommand::Previous).await;
    }

    /// Send `command` to the first playing player, or to the first player if
    /// none is playing.
    async fn player_command(&self, command: PlayerCommand) {
        let result = self
            .players(move |players| {
                let all = players.players()?;
                let target = all.iter().find(|p| p.playing).or_else(|| all.first());
                if let Some(target) = target {
                    players.send(&target.service, command)?;
                    info!("Sent {} to: {}", command, target.service);
                }
                Ok(())
            })
            .await;
        if let Err(e) = result {
            debug!("{}", e);
        }
    }

    async fn set_sink_volume(&self, sink: &str, percent: u32) {
        let sink = sink.to_string();
        if let Err(e) = self.sound(move |s| s.set_sink_volume(&sink, percent)).await {
            error!("Could not set the volume: {}", e);
        }
    }

    pub async fn handle_conversational_awareness(&self, status: u8) {
        debug!(
            "Entering handle_conversational_awareness with status: {}",
            status
        );

        if self.mac.is_empty() {
            debug!("No connected device MAC, skipping conversational awareness");
            return;
        }
        let mac = self.mac.to_string();
        let sink = match self.sound(move |s| s.find_sink(&mac)).await {
            Ok(sink) => sink,
            Err(e) => {
                warn!("{}, skipping conversational awareness", e);
                return;
            },
        };
        let current_volume = {
            let sink = sink.clone();
            self.sound(move |s| s.sink_volume(&sink)).await.ok()
        };

        match status {
            1 => self.conversation_started(&sink, current_volume).await,
            2 => self.conversation_deepened(&sink).await,
            3 => self.conversation_easing(&sink, current_volume).await,
            4 | 6 | 7 => {
                debug!("Conversation end ({}), restoring volume if needed", status);
                self.restore_volume_if_needed(&sink).await;
            },
            _ => debug!("Conversation status ({}), ignoring", status),
        }
    }

    /// Status 1: lower the volume to 25%, remembering the original.
    async fn conversation_started(&self, sink: &str, current_volume: Option<u32>) {
        let original = current_volume.unwrap_or(0);
        debug!("Conversation start (1). Current volume: {}", original);
        {
            let mut state = self.state.lock().await;
            if state.conv_conversation_started {
                debug!("Conversation already started; not overwriting conv_original_volume");
            } else {
                state.conv_original_volume = Some(original);
                state.conv_conversation_started = true;
            }
        }
        if original > 25 {
            self.set_sink_volume(sink, 25).await;
            info!(
                "Conversation start: lowered volume to 25% (original {})",
                original
            );
        } else {
            debug!("Original volume {} <= 25, not reducing to 25", original);
        }
    }

    /// Status 2: lower the volume further, to 15%.
    async fn conversation_deepened(&self, sink: &str) {
        let Some(original) = self.state.lock().await.conv_original_volume else {
            debug!("No original volume known for status 2, skipping");
            return;
        };
        debug!("Conversation reduce (2). Original: {}", original);
        if original > 15 {
            self.set_sink_volume(sink, 15).await;
            info!(
                "Conversation reduce: lowered volume to 15% (original {})",
                original
            );
        } else {
            debug!("Original {} <= 15, not reducing to 15", original);
        }
    }

    /// Status 3: back up to at most 25%.
    async fn conversation_easing(&self, sink: &str, current_volume: Option<u32>) {
        let (started, original) = {
            let state = self.state.lock().await;
            (state.conv_conversation_started, state.conv_original_volume)
        };
        if !started {
            debug!("Received status 3 but conversation was not started; ignoring increase");
            return;
        }
        let Some(reference) = original.or(current_volume) else {
            debug!("No original volume known for status 3, skipping");
            return;
        };
        let target = reference.min(25);
        self.set_sink_volume(sink, target).await;
        info!(
            "Conversation partial increase (3): set volume to {} (reference {})",
            target, reference
        );
    }

    async fn restore_volume_if_needed(&self, sink: &str) {
        let original = {
            let mut state = self.state.lock().await;
            if !state.conv_conversation_started {
                return;
            }
            state.conv_conversation_started = false;
            state.conv_original_volume.take()
        };
        if let Some(original) = original {
            self.set_sink_volume(sink, original).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            audio::mpris::fake::FakePlayers,
            utils::{AppSettings, PreferredCodec},
        },
        tempfile::TempDir,
        tokio::sync::mpsc::unbounded_channel,
    };

    const MAC: &str = "AA:BB:CC:DD:EE:FF";
    const CARD: u32 = 7;
    const SINK: &str = "bluez_output.AA_BB_CC_DD_EE_FF.1";

    use EarDetectionStatus::{InEar, OutOfEar};

    #[derive(Default)]
    struct FakeSoundState {
        card_present: bool,
        profiles: CardProfiles,
        profile_sets: Vec<String>,
        volume: u32,
        volume_sets: Vec<u32>,
    }

    #[derive(Default)]
    struct FakeSound {
        state: StdMutex<FakeSoundState>,
    }

    impl FakeSound {
        fn with_card(active: &str, available: &[&str]) -> Self {
            let sound = Self::default();
            {
                let mut state = sound.state.lock().unwrap();
                state.card_present = true;
                state.profiles = CardProfiles {
                    active: Some(active.to_string()),
                    available: available.iter().map(ToString::to_string).collect(),
                };
            }
            sound
        }

        fn profile_sets(&self) -> Vec<String> {
            self.state.lock().unwrap().profile_sets.clone()
        }
    }

    impl SoundServer for FakeSound {
        fn find_card(&self, mac: &str) -> Result<u32, SoundServerError> {
            if self.state.lock().unwrap().card_present && mac == MAC {
                Ok(CARD)
            } else {
                Err(SoundServerError::CardNotFound(mac.to_string()))
            }
        }

        fn card_profiles(&self, card: u32) -> Result<CardProfiles, SoundServerError> {
            assert_eq!(card, CARD);
            Ok(self.state.lock().unwrap().profiles.clone())
        }

        fn set_card_profile(&self, card: u32, profile: &str) -> Result<(), SoundServerError> {
            assert_eq!(card, CARD);
            let mut state = self.state.lock().unwrap();
            state.profile_sets.push(profile.to_string());
            if !state.profiles.has(profile) {
                return Err(SoundServerError::ProfileRejected {
                    card,
                    profile: profile.to_string(),
                });
            }
            state.profiles.active = Some(profile.to_string());
            Ok(())
        }

        fn find_sink(&self, _mac: &str) -> Result<String, SoundServerError> {
            Ok(SINK.to_string())
        }

        fn sink_volume(&self, _sink: &str) -> Result<u32, SoundServerError> {
            Ok(self.state.lock().unwrap().volume)
        }

        fn set_sink_volume(&self, _sink: &str, percent: u32) -> Result<(), SoundServerError> {
            let mut state = self.state.lock().unwrap();
            state.volume = percent;
            state.volume_sets.push(percent);
            Ok(())
        }
    }

    struct Harness {
        controller: MediaController,
        sound: Arc<FakeSound>,
        players: Arc<FakePlayers>,
        takeovers: TakeoverRequests,
        listeners: PlaybackListeners,
        _dir: TempDir,
    }

    fn harness_with(sound: FakeSound, codec: Option<PreferredCodec>) -> Harness {
        let dir = TempDir::new().unwrap();
        let settings = SettingsStore::new(dir.path().join("app_settings.json"));
        if let Some(preferred_codec) = codec {
            settings
                .save(&AppSettings {
                    preferred_codec,
                    ..AppSettings::default()
                })
                .unwrap();
        }
        let sound = Arc::new(sound);
        let players = Arc::new(FakePlayers::default());
        let takeovers = TakeoverRequests::default();
        let listeners = PlaybackListeners::default();
        let deps = MediaDeps {
            sound: sound.clone(),
            players: players.clone(),
            settings,
            takeovers: takeovers.clone(),
            listeners: listeners.clone(),
        };
        Harness {
            controller: MediaController::with_deps(MAC.to_string(), String::new(), deps),
            sound,
            players,
            takeovers,
            listeners,
            _dir: dir,
        }
    }

    fn harness() -> Harness {
        harness_with(
            FakeSound::with_card("off", &["off", "a2dp-sink", "a2dp-sink-sbc"]),
            None,
        )
    }

    #[test]
    fn baseline_reading_is_not_a_transition() {
        let mut watch = PlaybackWatch::default();
        let mut state = MediaControllerState::default();
        let now = Instant::now();

        assert!(!watch.observe(&mut state, true, false, now));
        assert!(!watch.observe(&mut state, true, false, now));
        assert!(!watch.observe(&mut state, false, false, now));
        assert!(watch.observe(&mut state, true, false, now));
    }

    #[test]
    fn requested_takeover_acts_on_the_first_playing_reading() {
        let mut watch = PlaybackWatch::default();
        let mut state = MediaControllerState::default();

        assert!(watch.observe(&mut state, true, true, Instant::now()));
    }

    #[test]
    fn requested_takeover_is_asked_again_on_every_playing_poll() {
        let mut watch = PlaybackWatch::default();
        let mut state = MediaControllerState::default();
        let now = Instant::now();
        watch.observe(&mut state, true, true, now);

        assert!(watch.observe(&mut state, true, true, now));
        assert!(watch.observe(&mut state, true, true, now));
        // Once the request is done or expired, steady playback is not a start.
        assert!(!watch.observe(&mut state, true, false, now));
    }

    #[test]
    fn resume_within_own_pause_window_does_not_take_ownership() {
        let mut watch = PlaybackWatch::default();
        let mut state = MediaControllerState::default();
        let paused_at = Instant::now();
        watch.observe(&mut state, false, false, paused_at);
        state.i_paused_the_media_at = Some(paused_at);

        let later = paused_at + OWN_PAUSE_WINDOW - Duration::from_millis(1);
        assert!(!watch.observe(&mut state, true, false, later));
        // The pause is forgotten once it explained one resume.
        assert!(state.i_paused_the_media_at.is_none());
    }

    #[test]
    fn resume_after_own_pause_window_takes_ownership() {
        let mut watch = PlaybackWatch::default();
        let mut state = MediaControllerState::default();
        let paused_at = Instant::now();
        watch.observe(&mut state, false, false, paused_at);
        state.i_paused_the_media_at = Some(paused_at);

        assert!(watch.observe(&mut state, true, false, paused_at + OWN_PAUSE_WINDOW));
    }

    #[tokio::test(start_paused = true)]
    async fn takeover_waits_for_ear_status_then_claims_the_audio() {
        let h = harness();
        let aacp = AACPManager::new();
        let (tx, mut rx) = unbounded_channel();
        h.takeovers.request(MAC);

        h.controller.take_over(&aacp, &tx).await;

        assert!(rx.try_recv().is_err());
        assert!(h.takeovers.has(MAC), "request kept for the next poll");
        assert!(h.sound.profile_sets().is_empty());

        aacp.state.lock().await.ear_detection_status = vec![InEar, OutOfEar];
        h.controller.take_over(&aacp, &tx).await;

        let (command, payload) = rx.try_recv().unwrap();
        assert_eq!(command, ControlCommandIdentifiers::OwnsConnection);
        assert_eq!(payload, [0x01]);
        assert!(!h.takeovers.has(MAC));
        assert_eq!(h.sound.profile_sets(), ["a2dp-sink"]);
    }

    #[tokio::test(start_paused = true)]
    async fn first_ear_reading_after_connecting_does_nothing() {
        let h = harness();
        h.players.set("spotify", true);

        h.controller
            .handle_ear_detection(Vec::new(), vec![InEar, InEar])
            .await;

        assert!(h.players.sent().is_empty());
        assert!(h.sound.profile_sets().is_empty());
    }

    async fn take_buds_out_and_back_after(h: &Harness, away: Duration) {
        h.players.set("spotify", true);
        h.controller
            .handle_ear_detection(vec![InEar, InEar], vec![OutOfEar, OutOfEar])
            .await;
        assert_eq!(
            h.players.sent(),
            [("spotify".to_string(), PlayerCommand::Pause)]
        );
        assert_eq!(h.sound.profile_sets(), ["off"]);
        h.players.clear_sent();

        tokio::time::advance(away).await;
        h.controller
            .handle_ear_detection(vec![OutOfEar, OutOfEar], vec![InEar, InEar])
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn ear_insertion_within_resume_window_resumes_paused_players() {
        let h = harness();

        take_buds_out_and_back_after(
            &h,
            RESUME_WINDOW.checked_sub(Duration::from_secs(1)).unwrap(),
        )
        .await;

        assert_eq!(
            h.players.sent(),
            [("spotify".to_string(), PlayerCommand::Play)]
        );
        assert_eq!(h.sound.profile_sets(), ["off", "a2dp-sink"]);
    }

    #[tokio::test(start_paused = true)]
    async fn ear_insertion_after_resume_window_does_not_resume() {
        let h = harness();

        take_buds_out_and_back_after(&h, RESUME_WINDOW).await;

        assert!(h.players.sent().is_empty());
        // The audio profile still comes back for whatever the user plays next.
        assert_eq!(h.sound.profile_sets(), ["off", "a2dp-sink"]);
    }

    #[tokio::test(start_paused = true)]
    async fn codec_preference_falls_back_in_order() {
        let available = ["off", "a2dp-sink-sbc", "a2dp-sink-sbc_xq"];
        for (codec, expected) in [
            (PreferredCodec::Aac, "a2dp-sink-sbc_xq"),
            (PreferredCodec::SbcXq, "a2dp-sink-sbc_xq"),
            (PreferredCodec::Sbc, "a2dp-sink-sbc"),
        ] {
            let h = harness_with(FakeSound::with_card("off", &available), Some(codec));

            h.controller.activate_a2dp_profile().await;

            assert_eq!(h.sound.profile_sets(), [expected], "{codec}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn aac_is_the_default_codec() {
        let h = harness_with(
            FakeSound::with_card("off", &["off", "a2dp-sink-sbc", "a2dp-sink"]),
            None,
        );

        h.controller.activate_a2dp_profile().await;

        assert_eq!(h.sound.profile_sets(), ["a2dp-sink"]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_active_a2dp_profile_is_left_alone() {
        let h = harness_with(
            FakeSound::with_card("a2dp-sink-sbc", &["off", "a2dp-sink", "a2dp-sink-sbc"]),
            Some(PreferredCodec::Aac),
        );

        h.controller.activate_a2dp_profile().await;

        assert!(h.sound.profile_sets().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn missing_a2dp_profile_leaves_the_card_alone() {
        let h = harness_with(
            FakeSound::with_card("off", &["off", "headset-head-unit"]),
            None,
        );

        h.controller.activate_a2dp_profile().await;

        assert!(h.sound.profile_sets().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn missing_card_gives_up_after_the_lookup_retries() {
        let h = harness_with(FakeSound::default(), None);
        let started = Instant::now();

        h.controller.activate_a2dp_profile().await;

        assert_eq!(
            started.elapsed(),
            CARD_LOOKUP_INTERVAL * (CARD_LOOKUP_ATTEMPTS - 1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn conversation_lowers_the_volume_and_restores_it() {
        let h = harness();
        h.sound.state.lock().unwrap().volume = 60;

        h.controller.handle_conversational_awareness(1).await;
        h.controller.handle_conversational_awareness(2).await;
        h.controller.handle_conversational_awareness(3).await;
        h.controller.handle_conversational_awareness(4).await;

        assert_eq!(h.sound.state.lock().unwrap().volume_sets, [25, 15, 25, 60]);
    }

    #[tokio::test(start_paused = true)]
    async fn next_track_goes_to_the_playing_player() {
        let h = harness();
        h.players.set("idle", false);
        h.players.set("spotify", true);

        h.controller.next_track().await;

        assert_eq!(
            h.players.sent(),
            [("spotify".to_string(), PlayerCommand::Next)]
        );
    }

    #[tokio::test]
    async fn playback_listener_replacement_is_scoped_to_device() {
        let first = harness();
        let (first_tx, _first_rx) = unbounded_channel();
        let first_handle = first
            .controller
            .start_playback_listener(AACPManager::new(), first_tx);

        let other = MediaController::with_deps(
            "00:00:00:00:00:02".to_string(),
            String::new(),
            first.controller.deps.clone(),
        );
        let (other_tx, _other_rx) = unbounded_channel();
        let other_handle = other.start_playback_listener(AACPManager::new(), other_tx);
        assert!(!first_handle.is_finished());

        let replacement = MediaController::with_deps(
            MAC.to_string(),
            String::new(),
            first.controller.deps.clone(),
        );
        let (replacement_tx, _replacement_rx) = unbounded_channel();
        let replacement_handle =
            replacement.start_playback_listener(AACPManager::new(), replacement_tx);

        tokio::time::timeout(Duration::from_secs(1), async {
            while !first_handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("superseded playback listener should stop");
        assert!(!other_handle.is_finished());
        assert_eq!(first.listeners.tasks.lock().unwrap().len(), 2);

        other_handle.abort();
        replacement_handle.abort();
    }
}
