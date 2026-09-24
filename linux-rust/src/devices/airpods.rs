use {
    crate::{
        bluetooth::aacp::{
            AACPEvent, AACPManager, AirPodsLEKeys, ControlCommandIdentifiers, ProximityKeyType,
        },
        media_controller::MediaController,
        ui::{messages::BluetoothUIMessage, tray::MyTray},
        utils::get_app_settings_path,
    },
    bluer::Address,
    ksni::Handle,
    serde::{Deserialize, Serialize},
    tokio::time::{Duration, sleep},
    tracing::{debug, error, info},
};

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

/// Handshake, feature flags and notification request, `SETUP_REPEAT_GAP` apart.
async fn send_setup_sequence(aacp_manager: &AACPManager) {
    if let Err(e) = aacp_manager.send_handshake().await {
        error!("Failed to send handshake to AirPods device: {}", e);
    }
    sleep(SETUP_REPEAT_GAP).await;
    if let Err(e) = aacp_manager.send_set_feature_flags_packet().await {
        error!("Failed to set feature flags: {}", e);
    }
    sleep(SETUP_REPEAT_GAP).await;
    if let Err(e) = aacp_manager.send_notification_request().await {
        error!("Failed to request notifications: {}", e);
    }
}

async fn request_notifications_if_silent(aacp_manager: &AACPManager) {
    if aacp_manager.has_device_status().await {
        return;
    }
    info!("No battery or ear detection from the AirPods yet, requesting notifications again");
    if let Err(e) = aacp_manager.send_notification_request().await {
        error!("Failed to request notifications: {}", e);
    }
}

pub struct AirPodsDevice {
    pub mac_address: Address,
    pub aacp_manager: AACPManager,
    // pub att_manager: ATTManager,
    pub media_controller: MediaController,
    // pub command_tx: Option<tokio::sync::mpsc::UnboundedSender<(ControlCommandIdentifiers, Vec<u8>)>>,
}

impl AirPodsDevice {
    pub async fn new(
        mac_address: Address,
        tray_handle: Option<Handle<MyTray>>,
        ui_tx: tokio::sync::mpsc::UnboundedSender<BluetoothUIMessage>,
    ) -> bluer::Result<Self> {
        info!("Creating new AirPodsDevice for {}", mac_address);
        let mut aacp_manager = AACPManager::new();
        aacp_manager.connect(mac_address).await?;

        // let mut att_manager = ATTManager::new();
        // att_manager.connect(mac_address).await.expect("Failed to connect ATT");

        if let Some(handle) = &tray_handle {
            handle
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

        info!("Sending handshake");
        if let Err(e) = aacp_manager.send_handshake().await {
            error!("Failed to send handshake to AirPods device: {}", e);
        }

        sleep(Duration::from_millis(300)).await;

        info!("Setting feature flags");
        if let Err(e) = aacp_manager.send_set_feature_flags_packet().await {
            error!("Failed to set feature flags: {}", e);
        }

        sleep(Duration::from_millis(300)).await;

        info!("Requesting notifications");
        if let Err(e) = aacp_manager.send_notification_request().await {
            error!("Failed to request notifications: {}", e);
        }

        info!("sending some packet");
        if let Err(e) = aacp_manager.send_some_packet().await {
            error!("Failed to send some packet: {}", e);
        }

        info!("Requesting Proximity Keys: IRK and ENC_KEY");
        if let Err(e) = aacp_manager
            .send_proximity_keys_request(vec![ProximityKeyType::Irk, ProximityKeyType::EncKey])
            .await
        {
            error!("Failed to request proximity keys: {}", e);
        }

        let app_settings_path = get_app_settings_path();
        let settings = std::fs::read_to_string(&app_settings_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
        let stem_control = settings
            .clone()
            .and_then(|v| v.get("stem_control").cloned())
            .and_then(|s| serde_json::from_value(s).ok())
            .unwrap_or(false);

        if stem_control {
            // Enable stem press detection (double and triple tap)
            // StemConfig bitmask for the control command: single=0x01, double=0x02, triple=0x04, long=0x08
            // We want double and triple: 0x02 | 0x04 = 0x06
            // Note: these bitmask values differ from the StemPressType event enum values (0x05–0x08)
            info!("Enabling stem press detection for double and triple tap");
            if let Err(e) = aacp_manager
                .send_control_command(ControlCommandIdentifiers::StemConfig, &[0x06])
                .await
            {
                error!("Failed to enable stem press detection: {}", e);
            }
        }

        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;
        let local_mac = adapter.address().await?.to_string();

        // MediaController is a cheap handle over shared state and its methods
        // take &self, so each task gets a clone instead of sharing a lock:
        // activate_a2dp_profile can take many seconds, and a lock held for it
        // would stall every AACP event behind it.
        let media_controller = MediaController::new(mac_address.to_string(), local_mac.clone());
        let mc_clone = media_controller.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();

        aacp_manager.set_event_channel(tx).await;
        if let Some(handle) = &tray_handle {
            handle
                .update(|tray: &mut MyTray| tray.command_tx = Some(command_tx.clone()))
                .await;
        }

        let aacp_manager_clone = aacp_manager.clone();
        aacp_manager.spawn_connection_task(async move {
            while let Some((id, value)) = command_rx.recv().await {
                if let Err(e) = aacp_manager_clone.send_control_command(id, &value).await {
                    tracing::error!("Failed to send control command: {}", e);
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
        if let Some(listener) = media_controller
            .start_playback_listener(aacp_manager_clone_listener, command_tx.clone())
            .await
        {
            aacp_manager.track_connection_task(listener);
        }

        let (listening_mode_tx, mut listening_mode_rx) = tokio::sync::mpsc::unbounded_channel();
        aacp_manager
            .subscribe_to_control_command(
                ControlCommandIdentifiers::ListeningMode,
                listening_mode_tx,
            )
            .await;
        let tray_handle_clone = tray_handle.clone();
        aacp_manager.spawn_connection_task(async move {
            while let Some(value) = listening_mode_rx.recv().await {
                if let Some(handle) = &tray_handle_clone {
                    handle
                        .update(|tray: &mut MyTray| {
                            tray.listening_mode = value.first().copied();
                        })
                        .await;
                }
            }
        });

        let (allow_off_tx, mut allow_off_rx) = tokio::sync::mpsc::unbounded_channel();
        aacp_manager
            .subscribe_to_control_command(ControlCommandIdentifiers::AllowOffOption, allow_off_tx)
            .await;
        let tray_handle_clone = tray_handle.clone();
        aacp_manager.spawn_connection_task(async move {
            while let Some(value) = allow_off_rx.recv().await {
                if let Some(handle) = &tray_handle_clone {
                    handle
                        .update(|tray: &mut MyTray| {
                            tray.allow_off_option = Some(value[0]);
                        })
                        .await;
                }
            }
        });

        let (conversation_detect_tx, mut conversation_detect_rx) =
            tokio::sync::mpsc::unbounded_channel();
        aacp_manager
            .subscribe_to_control_command(
                ControlCommandIdentifiers::ConversationDetectConfig,
                conversation_detect_tx,
            )
            .await;
        let tray_handle_clone = tray_handle.clone();
        aacp_manager.spawn_connection_task(async move {
            while let Some(value) = conversation_detect_rx.recv().await {
                if let Some(handle) = &tray_handle_clone {
                    handle
                        .update(|tray: &mut MyTray| {
                            tray.conversation_detect_enabled = Some(value[0] == 0x01);
                        })
                        .await;
                }
            }
        });

        let (owns_connection_tx, mut owns_connection_rx) = tokio::sync::mpsc::unbounded_channel();
        aacp_manager
            .subscribe_to_control_command(
                ControlCommandIdentifiers::OwnsConnection,
                owns_connection_tx,
            )
            .await;
        let mc_clone_owns = media_controller.clone();
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
                    mc_clone_owns.pause_all_media().await;
                    mc_clone_owns.deactivate_a2dp_profile().await;
                }
            }
        });

        let aacp_manager_clone_events = aacp_manager.clone();
        let local_mac_events = local_mac.clone();
        let ui_tx_clone = ui_tx.clone();
        let command_tx_clone = command_tx.clone();
        aacp_manager.spawn_connection_task(async move {
            while let Some(event) = rx.recv().await {
                let event_clone = event.clone();
                match event {
                    AACPEvent::EarDetection(old_status, new_status) => {
                        debug!(
                            "Received EarDetection event: old_status={:?}, new_status={:?}",
                            old_status, new_status
                        );
                        debug!(
                            "Calling handle_ear_detection with old_status: {:?}, new_status: {:?}",
                            old_status, new_status
                        );
                        mc_clone
                            .handle_ear_detection(old_status, new_status)
                            .await;
                    }
                    AACPEvent::BatteryInfo(battery_info) => {
                        debug!("Received BatteryInfo event: {:?}", battery_info);
                        if let Some(handle) = &tray_handle {
                            handle
                                .update(|tray: &mut MyTray| {
                                    for b in &battery_info {
                                        match b.component as u8 {
                                            0x01 => {
                                                tray.battery_headphone = Some(b.level);
                                                tray.battery_headphone_status = Some(b.status);
                                            }
                                            0x02 => {
                                                tray.battery_r = Some(b.level);
                                                tray.battery_r_status = Some(b.status);
                                            }
                                            0x04 => {
                                                tray.battery_l = Some(b.level);
                                                tray.battery_l_status = Some(b.status);
                                            }
                                            0x08 => {
                                                tray.battery_c = Some(b.level);
                                                tray.battery_c_status = Some(b.status);
                                            }
                                            _ => {}
                                        }
                                    }
                                })
                                .await;
                        }
                        debug!("Updated tray with new battery info");

                        let _ = ui_tx_clone.send(BluetoothUIMessage::AACPUIEvent(
                            mac_address.to_string(),
                            event_clone,
                        ));
                        debug!("Sent BatteryInfo event to UI");
                    }
                    AACPEvent::ControlCommand(status) => {
                        debug!("Received ControlCommand event: {:?}", status);
                        let _ = ui_tx_clone.send(BluetoothUIMessage::AACPUIEvent(
                            mac_address.to_string(),
                            event_clone,
                        ));
                        debug!("Sent ControlCommand event to UI");
                    }
                    AACPEvent::ConversationalAwareness(status) => {
                        debug!("Received ConversationalAwareness event: {}", status);
                        mc_clone.handle_conversational_awareness(status).await;
                    }
                    AACPEvent::ConnectedDevices(old_devices, new_devices) => {
                        let local_mac = local_mac_events.clone();
                        let new_devices_filtered = new_devices.iter().filter(|new_device| {
                            let not_in_old = old_devices
                                .iter()
                                .all(|old_device| old_device.mac != new_device.mac);
                            let not_local = new_device.mac != local_mac;
                            not_in_old && not_local
                        });

                        for device in new_devices_filtered {
                            info!(
                                "New connected device: {}, info1: {}, info2: {}",
                                device.mac, device.info1, device.info2
                            );
                            info!(
                                "Sending new Tipi packet for device {}, and sending media info to the device",
                                device.mac
                            );
                            let aacp_manager_clone = aacp_manager_clone_events.clone();
                            let local_mac_clone = local_mac.clone();
                            let device_mac_clone = device.mac.clone();
                            tokio::spawn(async move {
                                if let Err(e) = aacp_manager_clone
                                    .send_media_information_new_device(
                                        &local_mac_clone,
                                        &device_mac_clone,
                                    )
                                    .await
                                {
                                    error!("Failed to send media info new device: {}", e);
                                }
                                if let Err(e) = aacp_manager_clone
                                    .send_add_tipi_device(&local_mac_clone, &device_mac_clone)
                                    .await
                                {
                                    error!("Failed to send add tipi device: {}", e);
                                }
                            });
                        }
                    }
                    AACPEvent::OwnershipToFalseRequest => {
                        info!(
                            "Received ownership to false request. Setting ownership to false and pausing media."
                        );
                        let _ = command_tx_clone
                            .send((ControlCommandIdentifiers::OwnsConnection, vec![0x00]));
                        mc_clone.pause_all_media().await;
                        mc_clone.deactivate_a2dp_profile().await;
                    }
                    AACPEvent::StemPress(press_type, bud_type) => {
                        use crate::bluetooth::aacp::StemPressType;
                        info!(
                            "Received Stem Press: {:?} on {:?}",
                            press_type, bud_type
                        );
                        if stem_control {
                            match press_type {
                                StemPressType::DoublePress => {
                                    info!("Double press detected, skipping to next track");
                                    mc_clone.next_track().await;
                                }
                                StemPressType::TriplePress => {
                                    info!("Triple press detected, going to previous track");
                                    mc_clone.previous_track().await;
                                }
                                _ => {
                                    debug!("Unhandled stem press type: {:?}", press_type);
                                }
                            }
                        } else {
                            debug!("Stem control disabled, ignoring stem press event");
                        }
                    }
                    _ => {
                        debug!("Received unhandled AACP event: {:?}", event);
                        let _ = ui_tx_clone.send(BluetoothUIMessage::AACPUIEvent(
                            mac_address.to_string(),
                            event_clone,
                        ));
                        debug!("Sent unhandled AACP event to UI");
                    }
                }
            }
        });

        // Started last, so the event channel and subscribers are in place for
        // whatever the repeated setup brings.
        let aacp_manager_setup = aacp_manager.clone();
        aacp_manager.spawn_connection_task(async move {
            sleep(SETUP_REPEAT_GAP).await;
            send_setup_sequence(&aacp_manager_setup).await;
            sleep(SETUP_LATE_REPEAT).await;
            send_setup_sequence(&aacp_manager_setup).await;
            for _ in 0..STATUS_CHECKS {
                sleep(STATUS_CHECK_INTERVAL).await;
                if aacp_manager_setup.has_device_status().await {
                    return;
                }
                request_notifications_if_silent(&aacp_manager_setup).await;
            }
        });

        Ok(AirPodsDevice {
            mac_address,
            aacp_manager,
            // att_manager,
            media_controller,
            // command_tx: Some(command_tx.clone()),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
