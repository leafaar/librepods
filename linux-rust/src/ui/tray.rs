use {
    crate::{
        bluetooth::aacp::{BatteryStatus, ControlCommandIdentifiers},
        ui::{messages::BluetoothUIMessage, tray_icon},
        utils::get_app_settings_path,
    },
    ksni::{
        Icon, MenuItem, ToolTip,
        menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem},
    },
    tokio::sync::mpsc::UnboundedSender,
};

#[derive(Debug)]
pub struct MyTray {
    pub conversation_detect_enabled: Option<bool>,
    pub battery_headphone: Option<u8>,
    pub battery_headphone_status: Option<BatteryStatus>,
    pub battery_l: Option<u8>,
    pub battery_l_status: Option<BatteryStatus>,
    pub battery_r: Option<u8>,
    pub battery_r_status: Option<BatteryStatus>,
    pub battery_c: Option<u8>,
    pub battery_c_status: Option<BatteryStatus>,
    pub connected: bool,
    pub listening_mode: Option<u8>,
    pub allow_off_option: Option<u8>,
    pub command_tx: Option<UnboundedSender<(ControlCommandIdentifiers, Vec<u8>)>>,
    pub ui_tx: Option<UnboundedSender<BluetoothUIMessage>>,
    pub shutdown_tx: Option<UnboundedSender<()>>,
}

impl ksni::Tray for MyTray {
    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").into()
    }
    fn title(&self) -> String {
        "AirPods".into()
    }
    fn icon_pixmap(&self) -> Vec<Icon> {
        let app_settings_path = get_app_settings_path();
        let settings = std::fs::read_to_string(&app_settings_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
        let text_mode = settings
            .clone()
            .and_then(|v| v.get("tray_text_mode").cloned())
            .and_then(|ttm| serde_json::from_value(ttm).ok())
            .unwrap_or(false);
        let icon = generate_icon(self.icon_level(), text_mode, self.any_bud_charging());
        vec![icon]
    }
    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: vec![],
            title: "Battery Status".to_string(),
            description: self.battery_description(),
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.open_window();
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let allow_off = self.allow_off_option == Some(0x01);
        let options = if allow_off {
            vec![
                ("Off", 0x01),
                ("Noise Cancellation", 0x02),
                ("Transparency", 0x03),
                ("Adaptive", 0x04),
            ]
        } else {
            vec![
                ("Noise Cancellation", 0x02),
                ("Transparency", 0x03),
                ("Adaptive", 0x04),
            ]
        };
        let selected = self
            .listening_mode
            .and_then(|mode| options.iter().position(|&(_, val)| val == mode))
            .unwrap_or(0);
        let options_clone = options.clone();
        vec![
            StandardItem {
                label: "Open Window".into(),
                icon_name: "window-new".into(),
                activate: Box::new(|this: &mut Self| this.open_window()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Connect AirPods to this PC".into(),
                icon_name: "bluetooth-active".into(),
                enabled: !self.connected,
                activate: Box::new(|this: &mut Self| {
                    if let Some(tx) = &this.ui_tx {
                        let _ = tx.send(BluetoothUIMessage::ConnectAirPods);
                    }
                }),
                ..Default::default()
            }
            .into(),
            RadioGroup {
                selected,
                select: Box::new(move |this: &mut Self, current| {
                    if let Some(tx) = &this.command_tx {
                        let value = options_clone.get(current).map_or(0x02, |&(_, val)| val);
                        let _ = tx.send((ControlCommandIdentifiers::ListeningMode, vec![value]));
                    }
                }),
                options: options
                    .into_iter()
                    .map(|(label, _)| RadioItem {
                        label: label.into(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Conversation Detection".into(),
                checked: self.conversation_detect_enabled.unwrap_or(false),
                enabled: self.conversation_detect_enabled.is_some(),
                activate: Box::new(|this: &mut Self| {
                    if let Some(tx) = &this.command_tx
                        && let Some(is_enabled) = this.conversation_detect_enabled
                    {
                        let new_state = !is_enabled;
                        let value = if new_state { 0x01 } else { 0x02 };
                        let _ = tx.send((
                            ControlCommandIdentifiers::ConversationDetectConfig,
                            vec![value],
                        ));
                        this.conversation_detect_enabled = Some(new_state);
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Exit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| match &this.shutdown_tx {
                    Some(tx) => {
                        let _ = tx.send(());
                    },
                    None => std::process::exit(0),
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

impl MyTray {
    fn is_headphone(&self) -> bool {
        self.battery_headphone.is_some() || self.battery_headphone_status.is_some()
    }

    /// The level the icon shows: the headphone level, or the lower of the two
    /// buds. None when nothing reports a usable level.
    fn icon_level(&self) -> Option<u8> {
        if self.is_headphone() {
            return known_level(self.battery_headphone, self.battery_headphone_status);
        }
        [
            known_level(self.battery_l, self.battery_l_status),
            known_level(self.battery_r, self.battery_r_status),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn any_bud_charging(&self) -> bool {
        [
            self.battery_headphone_status,
            self.battery_l_status,
            self.battery_r_status,
        ]
        .into_iter()
        .flatten()
        .any(BatteryStatus::is_charging)
    }

    fn battery_description(&self) -> String {
        if self.is_headphone() {
            return format!(
                "Battery: {}",
                battery_text(self.battery_headphone, self.battery_headphone_status)
            );
        }
        format!(
            "L: {} R: {} C: {}",
            battery_text(self.battery_l, self.battery_l_status),
            battery_text(self.battery_r, self.battery_r_status),
            battery_text(self.battery_c, self.battery_c_status),
        )
    }

    fn open_window(&self) {
        if let Some(tx) = &self.ui_tx {
            let _ = tx.send(BluetoothUIMessage::OpenWindow);
        }
    }
}

/// A level worth drawing: reported, in range and not flagged as disconnected.
fn known_level(level: Option<u8>, status: Option<BatteryStatus>) -> Option<u8> {
    level.filter(|&l| l <= 100 && status != Some(BatteryStatus::Disconnected))
}

/// "80%" with a charging mark, or "-" when the level is unknown.
fn battery_text(level: Option<u8>, status: Option<BatteryStatus>) -> String {
    match known_level(level, status) {
        Some(l) => {
            let mark = if status.is_some_and(BatteryStatus::is_charging) {
                "⚡"
            } else {
                ""
            };
            format!("{l}%{mark}")
        },
        None => "-".to_string(),
    }
}

fn generate_icon(level: Option<u8>, text_mode: bool, charging: bool) -> Icon {
    let pixmap = tray_icon::render(level, text_mode, charging);
    Icon {
        width: pixmap.width as i32,
        height: pixmap.height as i32,
        data: pixmap.argb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_activation_opens_window() {
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tray = MyTray {
            conversation_detect_enabled: None,
            battery_headphone: None,
            battery_headphone_status: None,
            battery_l: None,
            battery_l_status: None,
            battery_r: None,
            battery_r_status: None,
            battery_c: None,
            battery_c_status: None,
            connected: false,
            listening_mode: None,
            allow_off_option: None,
            command_tx: None,
            ui_tx: Some(ui_tx),
            shutdown_tx: None,
        };

        <MyTray as ksni::Tray>::activate(&mut tray, TRAY_ACTIVATION_X, TRAY_ACTIVATION_Y);

        assert!(matches!(
            ui_rx.try_recv(),
            Ok(BluetoothUIMessage::OpenWindow)
        ));
    }

    const TRAY_ACTIVATION_X: i32 = 0;
    const TRAY_ACTIVATION_Y: i32 = 0;

    fn empty_tray() -> MyTray {
        MyTray {
            conversation_detect_enabled: None,
            battery_headphone: None,
            battery_headphone_status: None,
            battery_l: None,
            battery_l_status: None,
            battery_r: None,
            battery_r_status: None,
            battery_c: None,
            battery_c_status: None,
            connected: false,
            listening_mode: None,
            allow_off_option: None,
            command_tx: None,
            ui_tx: None,
            shutdown_tx: None,
        }
    }

    #[test]
    fn unknown_battery_has_no_level() {
        let mut tray = empty_tray();
        assert_eq!(tray.icon_level(), None);
        assert_eq!(tray.battery_description(), "L: - R: - C: -");

        tray.battery_l = Some(0);
        tray.battery_l_status = Some(BatteryStatus::Disconnected);
        tray.battery_r = Some(255);
        tray.battery_r_status = Some(BatteryStatus::NotCharging);
        assert_eq!(tray.icon_level(), None);
        assert_eq!(tray.battery_description(), "L: - R: - C: -");
    }

    #[test]
    fn icon_level_is_lowest_known_bud() {
        let mut tray = empty_tray();
        tray.battery_l = Some(80);
        tray.battery_l_status = Some(BatteryStatus::NotCharging);
        tray.battery_r = Some(40);
        tray.battery_r_status = Some(BatteryStatus::Disconnected);
        tray.battery_c = Some(0);
        tray.battery_c_status = Some(BatteryStatus::Disconnected);
        assert_eq!(tray.icon_level(), Some(80));
        assert_eq!(tray.battery_description(), "L: 80% R: - C: -");
    }

    #[test]
    fn headphones_show_a_single_level() {
        let mut tray = empty_tray();
        tray.battery_headphone = Some(55);
        tray.battery_headphone_status = Some(BatteryStatus::Charging);
        assert_eq!(tray.icon_level(), Some(55));
        assert!(tray.any_bud_charging());
        assert_eq!(tray.battery_description(), "Battery: 55%⚡");
    }

    #[test]
    fn optimized_charging_counts_as_charging() {
        let mut tray = empty_tray();
        tray.battery_l = Some(90);
        tray.battery_l_status = Some(BatteryStatus::OptimizedCharging);
        assert!(tray.any_bud_charging());
        assert_eq!(tray.battery_description(), "L: 90%⚡ R: - C: -");
    }
}
