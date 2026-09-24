use {
    crate::{
        bluetooth::{
            aacp::{
                AACPEvent, AACPManager, AirPodsLEKeys, BatteryComponent, BatteryInfo,
                ConnectedDevice, ControlCommandIdentifiers, ProximityKeyType, StemPressType,
            },
            l2cap::ConnectError,
        },
        media_controller::MediaController,
        ui::{messages::BluetoothUIMessage, tray::MyTray},
        utils::get_app_settings_path,
    },
    bluer::Address,
    ksni::Handle,
    serde::{Deserialize, Serialize},
    tokio::{
        sync::mpsc::{UnboundedSender, unbounded_channel},
        time::{Duration, sleep},
    },
    tracing::{debug, error, info},
};

/// Gap between the handshake, feature flags and notification request of the
/// first setup.
const INITIAL_SETUP_GAP: Duration = Duration::from_millis(300);

// AirPods taken over from another device (the Connect button, auto-switch) can
// answer the first setup with information, keys and control commands but never
// start sending battery (0x04) or ear detection (0x06). The Android app repeats
// the setup right away with 200 ms gaps and again after 5 s, and the
// notification request may be sent at any point of the connection, so it is
// also repeated until one of the two arrives.
const SETUP_REPEAT_GAP: Duration = Duration::from_millis(200);
const SETUP_LATE_REPEAT: Duration = Duration::from_secs(5);
const STATUS_CHECK_INTERVAL: Duration = Duration::from_secs(3);
const STATUS_CHECKS: u32 = 10;

/// StemConfig bitmask asking for double and triple press events (single 0x01,
/// double 0x02, triple 0x04, long 0x08). These differ from the StemPressType
/// values the events carry.
const STEM_CONFIG_DOUBLE_AND_TRIPLE: u8 = 0x02 | 0x04;

/// Why connected AirPods could not be set up. The cause is part of the message
/// because the callers log these with `{}`.
#[derive(Debug, thiserror::Error)]
pub enum AirPodsSetupError {
    #[error("opening the AACP channel failed: {0}")]
    Connect(ConnectError),
    #[error("reading the local adapter address failed: {0}")]
    LocalAddress(bluer::Error),
}

/// Handshake, feature flags, notification request and the key request that
/// start an AACP connection, and stem press events when `stem_control` is on.
async fn send_initial_setup(aacp_manager: &AACPManager, stem_control: bool) {
    info!("Sending handshake");
    if let Err(e) = aacp_manager.send_handshake().await {
        error!("Failed to send handshake to AirPods device: {e}");
    }
    sleep(INITIAL_SETUP_GAP).await;

    info!("Setting feature flags");
    if let Err(e) = aacp_manager.send_set_feature_flags_packet().await {
        error!("Failed to set feature flags: {e}");
    }
    sleep(INITIAL_SETUP_GAP).await;

    info!("Requesting notifications");
    if let Err(e) = aacp_manager.send_notification_request().await {
        error!("Failed to request notifications: {e}");
    }

    info!("sending some packet");
    if let Err(e) = aacp_manager.send_some_packet().await {
        error!("Failed to send some packet: {e}");
    }

    info!("Requesting Proximity Keys: IRK and ENC_KEY");
    if let Err(e) = aacp_manager
        .send_proximity_keys_request(vec![ProximityKeyType::Irk, ProximityKeyType::EncKey])
        .await
    {
        error!("Failed to request proximity keys: {e}");
    }

    if stem_control {
        info!("Enabling stem press detection for double and triple tap");
        if let Err(e) = aacp_manager
            .send_control_command(
                ControlCommandIdentifiers::StemConfig,
                &[STEM_CONFIG_DOUBLE_AND_TRIPLE],
            )
            .await
        {
            error!("Failed to enable stem press detection: {e}");
        }
    }
}

/// Handshake, feature flags and notification request, `SETUP_REPEAT_GAP` apart.
async fn send_setup_sequence(aacp_manager: &AACPManager) {
    if let Err(e) = aacp_manager.send_handshake().await {
        error!("Failed to send handshake to AirPods device: {e}");
    }
    sleep(SETUP_REPEAT_GAP).await;
    if let Err(e) = aacp_manager.send_set_feature_flags_packet().await {
        error!("Failed to set feature flags: {e}");
    }
    sleep(SETUP_REPEAT_GAP).await;
    if let Err(e) = aacp_manager.send_notification_request().await {
        error!("Failed to request notifications: {e}");
    }
}

async fn request_notifications_if_silent(aacp_manager: &AACPManager) {
    if aacp_manager.has_device_status().await {
        return;
    }
    info!("No battery or ear detection from the AirPods yet, requesting notifications again");
    if let Err(e) = aacp_manager.send_notification_request().await {
        error!("Failed to request notifications: {e}");
    }
}

/// Repeat the setup twice, then keep asking for notifications every
/// `STATUS_CHECK_INTERVAL` until battery or ear detection status arrives, at
/// most `STATUS_CHECKS` times.
async fn repeat_setup_until_status(aacp_manager: &AACPManager) {
    sleep(SETUP_REPEAT_GAP).await;
    send_setup_sequence(aacp_manager).await;
    sleep(SETUP_LATE_REPEAT).await;
    send_setup_sequence(aacp_manager).await;
    for _ in 0..STATUS_CHECKS {
        sleep(STATUS_CHECK_INTERVAL).await;
        if aacp_manager.has_device_status().await {
            return;
        }
        request_notifications_if_silent(aacp_manager).await;
    }
}

/// Whether stem presses skip tracks, from the app settings file.
fn load_stem_control() -> bool {
    std::fs::read_to_string(get_app_settings_path())
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("stem_control").cloned())
        .and_then(|s| serde_json::from_value(s).ok())
        .unwrap_or(false)
}

async fn local_adapter_address() -> bluer::Result<String> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    Ok(adapter.address().await?.to_string())
}

/// Mark the tray connected, with no battery known yet.
async fn reset_tray(tray_handle: &Handle<MyTray>) {
    tray_handle
        .update(|tray: &mut MyTray| {
            tray.connected = true;
            tray.battery_headphone = None;
            tray.battery_headphone_status = None;
            tray.battery_l = None;
            tray.battery_l_status = None;
            tray.battery_r = None;
            tray.battery_r_status = None;
            tray.battery_c = None;
            tray.battery_c_status = None;
        })
        .await;
}

fn update_tray_battery(tray: &mut MyTray, battery_info: &[BatteryInfo]) {
    for b in battery_info {
        let (level, status) = match b.component {
            BatteryComponent::Headphone => (
                &mut tray.battery_headphone,
                &mut tray.battery_headphone_status,
            ),
            BatteryComponent::Right => (&mut tray.battery_r, &mut tray.battery_r_status),
            BatteryComponent::Left => (&mut tray.battery_l, &mut tray.battery_l_status),
            BatteryComponent::Case => (&mut tray.battery_c, &mut tray.battery_c_status),
        };
        *level = Some(b.level);
        *status = Some(b.status);
    }
}

/// Copy every value of control command `identifier` into the tray with
/// `apply`, for as long as the connection lasts.
async fn mirror_to_tray(
    aacp_manager: &AACPManager,
    identifier: ControlCommandIdentifiers,
    tray_handle: Option<Handle<MyTray>>,
    apply: fn(&mut MyTray, &[u8]),
) {
    let (tx, mut rx) = unbounded_channel();
    aacp_manager
        .subscribe_to_control_command(identifier, tx)
        .await;
    aacp_manager.spawn_connection_task(async move {
        while let Some(value) = rx.recv().await {
            if let Some(handle) = &tray_handle {
                handle.update(|tray: &mut MyTray| apply(tray, &value)).await;
            }
        }
    });
}

/// Everything the AACP event loop of one connection acts on.
struct EventHandler {
    mac_address: Address,
    local_mac: String,
    aacp_manager: AACPManager,
    media_controller: MediaController,
    tray_handle: Option<Handle<MyTray>>,
    ui_tx: UnboundedSender<BluetoothUIMessage>,
    command_tx: UnboundedSender<(ControlCommandIdentifiers, Vec<u8>)>,
    stem_control: bool,
}

impl EventHandler {
    fn forward_to_ui(&self, event: AACPEvent) {
        let _ = self.ui_tx.send(BluetoothUIMessage::AACPUIEvent(
            self.mac_address.to_string(),
            event,
        ));
    }

    async fn handle(&self, event: AACPEvent) {
        match event {
            AACPEvent::EarDetection(old_status, new_status) => {
                debug!(
                    "Received EarDetection event: old_status={old_status:?}, new_status={new_status:?}"
                );
                self.media_controller
                    .handle_ear_detection(old_status, new_status)
                    .await;
            },
            AACPEvent::BatteryInfo(ref battery_info) => {
                debug!("Received BatteryInfo event: {battery_info:?}");
                if let Some(handle) = &self.tray_handle {
                    handle
                        .update(|tray: &mut MyTray| update_tray_battery(tray, battery_info))
                        .await;
                }
                debug!("Updated tray with new battery info");
                self.forward_to_ui(event);
                debug!("Sent BatteryInfo event to UI");
            },
            AACPEvent::ControlCommand(ref status) => {
                debug!("Received ControlCommand event: {status:?}");
                self.forward_to_ui(event);
                debug!("Sent ControlCommand event to UI");
            },
            AACPEvent::ConversationalAwareness(status) => {
                debug!("Received ConversationalAwareness event: {status}");
                self.media_controller
                    .handle_conversational_awareness(status)
                    .await;
            },
            AACPEvent::ConnectedDevices(old_devices, new_devices) => {
                self.announce_to_new_devices(&old_devices, &new_devices);
            },
            AACPEvent::OwnershipToFalseRequest => {
                info!(
                    "Received ownership to false request. Setting ownership to false and pausing media."
                );
                let _ = self
                    .command_tx
                    .send((ControlCommandIdentifiers::OwnsConnection, vec![0x00]));
                self.media_controller.pause_all_media().await;
                self.media_controller.deactivate_a2dp_profile().await;
            },
            AACPEvent::StemPress(press_type, bud_type) => {
                info!("Received Stem Press: {press_type:?} on {bud_type:?}");
                self.handle_stem_press(press_type).await;
            },
            AACPEvent::CustomEq(_) => {
                debug!("Received unhandled AACP event: {event:?}");
                self.forward_to_ui(event);
                debug!("Sent unhandled AACP event to UI");
            },
        }
    }

    /// Send media information and a new Tipi packet to every device that
    /// connected to the AirPods since the last report, other than this host.
    fn announce_to_new_devices(
        &self,
        old_devices: &[ConnectedDevice],
        new_devices: &[ConnectedDevice],
    ) {
        let new_devices = new_devices.iter().filter(|new_device| {
            let not_in_old = old_devices
                .iter()
                .all(|old_device| old_device.mac != new_device.mac);
            not_in_old && new_device.mac != self.local_mac
        });
        for device in new_devices {
            info!(
                "New connected device: {}, info1: {}, info2: {}",
                device.mac, device.info1, device.info2
            );
            info!(
                "Sending new Tipi packet for device {}, and sending media info to the device",
                device.mac
            );
            let aacp_manager = self.aacp_manager.clone();
            let local_mac = self.local_mac.clone();
            let device_mac = device.mac.clone();
            tokio::spawn(async move {
                if let Err(e) = aacp_manager
                    .send_media_information_new_device(&local_mac, &device_mac)
                    .await
                {
                    error!("Failed to send media info new device: {e}");
                }
                if let Err(e) = aacp_manager
                    .send_add_tipi_device(&local_mac, &device_mac)
                    .await
                {
                    error!("Failed to send add tipi device: {e}");
                }
            });
        }
    }

    async fn handle_stem_press(&self, press_type: StemPressType) {
        if !self.stem_control {
            debug!("Stem control disabled, ignoring stem press event");
            return;
        }
        match press_type {
            StemPressType::Double => {
                info!("Double press detected, skipping to next track");
                self.media_controller.next_track().await;
            },
            StemPressType::Triple => {
                info!("Triple press detected, going to previous track");
                self.media_controller.previous_track().await;
            },
            StemPressType::Single | StemPressType::Long => {
                debug!("Unhandled stem press type: {press_type:?}");
            },
        }
    }
}

pub struct AirPodsDevice {
    pub aacp_manager: AACPManager,
}

impl AirPodsDevice {
    pub async fn new(
        mac_address: Address,
        tray_handle: Option<Handle<MyTray>>,
        ui_tx: UnboundedSender<BluetoothUIMessage>,
    ) -> Result<Self, AirPodsSetupError> {
        info!("Creating new AirPodsDevice for {mac_address}");
        let mut aacp_manager = AACPManager::new();
        aacp_manager
            .connect(mac_address)
            .await
            .map_err(AirPodsSetupError::Connect)?;

        if let Some(handle) = &tray_handle {
            reset_tray(handle).await;
        }

        let stem_control = load_stem_control();
        send_initial_setup(&aacp_manager, stem_control).await;

        let local_mac = local_adapter_address()
            .await
            .map_err(AirPodsSetupError::LocalAddress)?;

        // MediaController is a cheap handle over shared state and its methods
        // take &self, so each task gets a clone instead of sharing a lock:
        // activate_a2dp_profile can take many seconds, and a lock held for it
        // would stall every AACP event behind it.
        let media_controller = MediaController::new(mac_address.to_string(), local_mac.clone());
        let (event_tx, mut event_rx) = unbounded_channel();
        let (command_tx, mut command_rx) = unbounded_channel();

        aacp_manager.set_event_channel(event_tx).await;
        if let Some(handle) = &tray_handle {
            handle
                .update(|tray: &mut MyTray| tray.command_tx = Some(command_tx.clone()))
                .await;
        }

        let aacp_manager_commands = aacp_manager.clone();
        aacp_manager.spawn_connection_task(async move {
            while let Some((id, value)) = command_rx.recv().await {
                if let Err(e) = aacp_manager_commands.send_control_command(id, &value).await {
                    error!("Failed to send control command: {e}");
                }
            }
        });

        // PipeWire can leave the card on the "off" profile when it appears
        // before its A2DP transport is ready, and nothing revisits that choice
        // until playback starts - so freshly connected buds stay silent, and
        // the microphone has no transport either. Claim a profile right away.
        let mc_profile = media_controller.clone();
        aacp_manager.spawn_connection_task(async move {
            mc_profile.activate_a2dp_profile().await;
        });

        let aacp_manager_clone_listener = aacp_manager.clone();
        let listener = media_controller
            .start_playback_listener(aacp_manager_clone_listener, command_tx.clone());
        aacp_manager.track_connection_task(listener);

        Self::spawn_subscribers(&aacp_manager, &media_controller, tray_handle.clone()).await;

        let handler = EventHandler {
            mac_address,
            local_mac,
            aacp_manager: aacp_manager.clone(),
            media_controller,
            tray_handle,
            ui_tx,
            command_tx,
            stem_control,
        };
        aacp_manager.spawn_connection_task(async move {
            while let Some(event) = event_rx.recv().await {
                handler.handle(event).await;
            }
        });

        // Started last, so the event channel and subscribers are in place for
        // whatever the repeated setup brings.
        let aacp_manager_setup = aacp_manager.clone();
        aacp_manager.spawn_connection_task(async move {
            repeat_setup_until_status(&aacp_manager_setup).await;
        });

        Ok(AirPodsDevice { aacp_manager })
    }

    /// Tasks that follow control commands: tray entries for the listening
    /// mode, the off option and conversation detection, and media for a
    /// change of connection ownership.
    async fn spawn_subscribers(
        aacp_manager: &AACPManager,
        media_controller: &MediaController,
        tray_handle: Option<Handle<MyTray>>,
    ) {
        mirror_to_tray(
            aacp_manager,
            ControlCommandIdentifiers::ListeningMode,
            tray_handle.clone(),
            |tray, value| tray.listening_mode = value.first().copied(),
        )
        .await;
        mirror_to_tray(
            aacp_manager,
            ControlCommandIdentifiers::AllowOffOption,
            tray_handle.clone(),
            |tray, value| tray.allow_off_option = value.first().copied(),
        )
        .await;
        mirror_to_tray(
            aacp_manager,
            ControlCommandIdentifiers::ConversationDetectConfig,
            tray_handle,
            |tray, value| tray.conversation_detect_enabled = value.first().map(|&b| b == 0x01),
        )
        .await;

        let (owns_connection_tx, mut owns_connection_rx) = unbounded_channel();
        aacp_manager
            .subscribe_to_control_command(
                ControlCommandIdentifiers::OwnsConnection,
                owns_connection_tx,
            )
            .await;
        let mc_owns = media_controller.clone();
        let aacp_manager_owns = aacp_manager.clone();
        aacp_manager.spawn_connection_task(async move {
            while let Some(value) = owns_connection_rx.recv().await {
                let owns = value.first().copied().unwrap_or(0) != 0;
                if owns {
                    // A takeover on an open connection can leave the status
                    // notifications off as well.
                    request_notifications_if_silent(&aacp_manager_owns).await;
                } else {
                    info!("Lost ownership, pausing media and disconnecting audio");
                    mc_owns.pause_all_media().await;
                    mc_owns.deactivate_a2dp_profile().await;
                }
            }
        });
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AirPodsInformation {
    pub name: String,
    pub model_number: String,
    pub manufacturer: String,
    pub serial_number: String,
    pub version1: String,
    pub version2: String,
    pub hardware_revision: String,
    pub updater_identifier: String,
    pub left_serial_number: String,
    pub right_serial_number: String,
    pub version3: String,
    pub le_keys: AirPodsLEKeys,
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::bluetooth::aacp::testing::{MemoryStore, connected_manager},
        std::sync::Arc,
        tokio::{sync::mpsc, time::Instant},
    };

    const HANDSHAKE: [u8; 16] = [
        0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    const FEATURE_FLAGS: [u8; 14] = [
        0x04, 0x00, 0x04, 0x00, 0x4D, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    const NOTIFICATION_REQUEST: [u8; 10] =
        [0x04, 0x00, 0x04, 0x00, 0x0F, 0x00, 0xFF, 0xFF, 0xFF, 0xFF];

    /// Everything sent while `run` runs, with the time since the start at which
    /// it was sent. `reply` is called with the number of packets sent so far and
    /// may return a packet for the AirPods to answer with. Needs the paused
    /// clock so the times are exact.
    async fn record_sent(
        manager: &AACPManager,
        mut sent: mpsc::Receiver<Vec<u8>>,
        run: impl Future<Output = ()>,
        mut reply: impl FnMut(usize) -> Option<Vec<u8>>,
    ) -> Vec<(Duration, Vec<u8>)> {
        let start = Instant::now();
        let run = async {
            run.await;
            // Closes the channel so the recorder below ends.
            manager.state.lock().await.sender = None;
        };
        let record = async {
            let mut log = Vec::new();
            while let Some(packet) = sent.recv().await {
                log.push((start.elapsed(), packet));
                if let Some(answer) = reply(log.len()) {
                    manager.receive_packet(&answer).await;
                }
            }
            log
        };
        tokio::join!(run, record).1
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[tokio::test(start_paused = true)]
    async fn initial_setup_sends_handshake_flags_and_requests_300ms_apart() {
        let (manager, sent) = connected_manager(Arc::new(MemoryStore::default())).await;

        let log = record_sent(&manager, sent, send_initial_setup(&manager, false), |_| {
            None
        })
        .await;

        assert_eq!(
            log,
            [
                (ms(0), HANDSHAKE.to_vec()),
                (ms(300), FEATURE_FLAGS.to_vec()),
                (ms(600), NOTIFICATION_REQUEST.to_vec()),
                (
                    ms(600),
                    vec![
                        0x04, 0x00, 0x04, 0x00, 0x29, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
                        0xFF, 0xFF
                    ]
                ),
                (
                    ms(600),
                    vec![0x04, 0x00, 0x04, 0x00, 0x30, 0x00, 0x05, 0x00]
                ),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn initial_setup_with_stem_control_asks_for_double_and_triple_press() {
        let (manager, sent) = connected_manager(Arc::new(MemoryStore::default())).await;

        let log = record_sent(&manager, sent, send_initial_setup(&manager, true), |_| None).await;

        assert_eq!(log.len(), 6);
        assert_eq!(
            log[5],
            (
                ms(600),
                vec![
                    0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x39, 0x06, 0x00, 0x00, 0x00
                ]
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn silent_airpods_get_the_setup_twice_then_ten_notification_requests() {
        let (manager, sent) = connected_manager(Arc::new(MemoryStore::default())).await;

        let log = record_sent(&manager, sent, repeat_setup_until_status(&manager), |_| {
            None
        })
        .await;

        let mut expected = vec![
            (ms(200), HANDSHAKE.to_vec()),
            (ms(400), FEATURE_FLAGS.to_vec()),
            (ms(600), NOTIFICATION_REQUEST.to_vec()),
            (ms(5_600), HANDSHAKE.to_vec()),
            (ms(5_800), FEATURE_FLAGS.to_vec()),
            (ms(6_000), NOTIFICATION_REQUEST.to_vec()),
        ];
        expected.extend((1..=10).map(|i| (ms(6_000 + 3_000 * i), NOTIFICATION_REQUEST.to_vec())));
        assert_eq!(log, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn setup_repeats_stop_once_the_airpods_report_battery() {
        let (manager, sent) = connected_manager(Arc::new(MemoryStore::default())).await;
        let start = Instant::now();
        let battery = vec![
            0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x01, 0x02, 0x01, 0x64, 0x02, 0x01,
        ];

        // The battery report answers the second setup's notification request.
        let log = record_sent(
            &manager,
            sent,
            repeat_setup_until_status(&manager),
            |count| (count == 6).then(|| battery.clone()),
        )
        .await;

        assert_eq!(log.len(), 6);
        assert_eq!(start.elapsed(), ms(9_000));
    }

    #[tokio::test(start_paused = true)]
    async fn ear_detection_also_ends_the_notification_requests() {
        let (manager, sent) = connected_manager(Arc::new(MemoryStore::default())).await;
        let ear_detection = vec![0x04, 0x00, 0x04, 0x00, 0x06, 0x00, 0x00, 0x00];

        // Arrives after the first repeated notification request.
        let log = record_sent(
            &manager,
            sent,
            repeat_setup_until_status(&manager),
            |count| (count == 7).then(|| ear_detection.clone()),
        )
        .await;

        assert_eq!(log.len(), 7);
        assert_eq!(log[6].0, ms(9_000));
    }
}
