//! The state of the window and every decision about it, with no GTK in it.
//!
//! `Model::update` takes one `Input` (a backend message, a user action, the
//! result of earlier work) and returns the `Effect`s to run: commands to the
//! AirPods, file writes, timers, toasts. The app runs the effects and the view
//! redraws from the model; neither decides anything on its own. Tests drive
//! `update` directly.

use {
    crate::{
        bluetooth::{
            aacp::{AACPEvent, BatteryInfo, ControlCommandIdentifiers, ControlCommandStatus},
            eq::CustomEq,
        },
        devices::{
            airpods::AirPodsInformation,
            enums::{
                AirPodsNoiseControlMode, DeviceData, DeviceInformation, DeviceType, NothingAncMode,
            },
        },
        ui::{
            connect::{ConnectRequests, ConnectStatus, sidebar_status},
            format::{live_case_level, validate_device_name},
            gtk::battery::{Batteries, batteries},
            messages::BluetoothUIMessage,
        },
        utils::{AppSettings, PreferredCodec, ThemePreference},
    },
    bluer::Address,
    std::collections::{HashMap, HashSet},
    tracing::{debug, error, warn},
};

/// Everything that can change the model, in the order it happened.
#[derive(Debug)]
pub(crate) enum Input {
    Backend(BluetoothUIMessage),
    /// devices.json was read.
    DevicesLoaded(HashMap<String, DeviceData>),
    /// The state a device's manager held when it connected. None when no
    /// manager was found for it.
    Snapshot(String, Option<DeviceSnapshot>),
    Select(Selection),
    Connect(String),
    ConnectFinished {
        mac: String,
        attempt: u64,
        result: Result<(), String>,
    },
    ConnectSetupTimedOut {
        mac: String,
        attempt: u64,
    },
    SetListeningMode(String, AirPodsNoiseControlMode),
    SetConversationAwareness(String, bool),
    SetPersonalizedVolume(String, bool),
    SetAllowOff(String, bool),
    /// The name field changed; the text is not sent until `Rename`.
    NameEdited(String, String),
    Rename(String, String),
    SetNothingAnc(String, NothingAncMode),
    Setting(SettingChange),
    /// A message for the user, such as a command that failed.
    Toast(String),
}

/// Work for the app to do after an update.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Effect {
    /// Read devices.json and come back with `Input::DevicesLoaded`.
    LoadDevices,
    /// Read the state of a device that just connected and come back with
    /// `Input::Snapshot`.
    Snapshot(String),
    Connect {
        mac: String,
        address: Address,
        attempt: u64,
    },
    /// Come back with `Input::ConnectSetupTimedOut` after CONNECT_SETUP_TIMEOUT.
    SetupTimeout {
        mac: String,
        attempt: u64,
    },
    SendControl {
        mac: String,
        identifier: ControlCommandIdentifiers,
        value: Vec<u8>,
    },
    SetConversationDetection {
        mac: String,
        enabled: bool,
    },
    /// Send the rename packet, save the name to devices.json, then reload it.
    Rename {
        mac: String,
        name: String,
    },
    SetNothingAnc {
        mac: String,
        mode: u8,
    },
    /// Write `Model::settings` to disk.
    SaveSettings,
    ApplyTheme(ThemePreference),
    PresentWindow,
    Toast(String),
}

/// What a connected device reported when the UI first saw it.
#[derive(Debug, Clone)]
pub(crate) enum DeviceSnapshot {
    AirPods(AirPodsSnapshot),
    Nothing,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AirPodsSnapshot {
    pub(crate) battery: Vec<BatteryInfo>,
    pub(crate) controls: Vec<ControlCommandStatus>,
    pub(crate) custom_eq: Option<CustomEq>,
}

/// The sidebar entry that is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Selection {
    None,
    Device(String),
    Settings,
}

/// What the content pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Content {
    /// Nothing selected and no device known.
    Empty,
    Settings,
    AirPods(String),
    /// Connected, waiting for the snapshot of its state.
    Waiting(String),
    /// Known AirPods that are not connected to this PC.
    Disconnected(String),
    Nothing(String),
    /// A known device of another kind that is not connected.
    Unavailable(String),
}

/// Live state of connected AirPods.
#[derive(Debug, Clone)]
pub(crate) struct AirPods {
    pub(crate) name: String,
    pub(crate) battery: Vec<BatteryInfo>,
    pub(crate) listening_mode: AirPodsNoiseControlMode,
    pub(crate) conversation_awareness: bool,
    pub(crate) personalized_volume: bool,
    pub(crate) allow_off: bool,
    /// Last known raw value per control command id, as reported by the
    /// AirPods or last set from the UI. The settings sections read these.
    pub(crate) control_values: HashMap<u8, Vec<u8>>,
    pub(crate) custom_eq: Option<CustomEq>,
}

impl AirPods {
    fn from_snapshot(name: String, snapshot: AirPodsSnapshot) -> Self {
        let mut airpods = AirPods {
            name,
            battery: snapshot.battery,
            listening_mode: AirPodsNoiseControlMode::Transparency,
            conversation_awareness: false,
            personalized_volume: false,
            allow_off: false,
            control_values: HashMap::new(),
            custom_eq: snapshot.custom_eq,
        };
        for status in snapshot.controls {
            airpods.apply_control(status);
        }
        airpods
    }

    fn apply_control(&mut self, status: ControlCommandStatus) {
        let enabled = || match status.value.as_slice() {
            [0x01] => true,
            [0x02] => false,
            other => {
                warn!("Unknown value {:?} for {:?}", other, status.identifier);
                false
            },
        };
        match status.identifier {
            ControlCommandIdentifiers::ListeningMode => {
                self.listening_mode = status.value.first().map_or(
                    AirPodsNoiseControlMode::Transparency,
                    AirPodsNoiseControlMode::from_byte,
                );
            },
            ControlCommandIdentifiers::ConversationDetectConfig => {
                self.conversation_awareness = enabled();
            },
            ControlCommandIdentifiers::AdaptiveVolumeConfig => {
                self.personalized_volume = enabled();
            },
            ControlCommandIdentifiers::AllowOffOption => self.allow_off = enabled(),
            _ => {},
        }
        self.control_values
            .insert(status.identifier as u8, status.value);
    }

    /// Listening modes to offer, in menu order. Off only when the AirPods
    /// allow it.
    pub(crate) fn listening_modes(&self) -> Vec<AirPodsNoiseControlMode> {
        let mut modes = vec![
            AirPodsNoiseControlMode::Transparency,
            AirPodsNoiseControlMode::NoiseCancellation,
            AirPodsNoiseControlMode::Adaptive,
        ];
        if self.allow_off {
            modes.insert(0, AirPodsNoiseControlMode::Off);
        }
        modes
    }
}

/// Live state of a connected Nothing device.
#[derive(Debug, Clone)]
pub(crate) struct Nothing {
    pub(crate) anc_mode: NothingAncMode,
}

/// Nothing noise control modes, in menu order.
pub(crate) const NOTHING_ANC_MODES: [NothingAncMode; 6] = [
    NothingAncMode::Off,
    NothingAncMode::Transparency,
    NothingAncMode::AdaptiveNoiseCancellation,
    NothingAncMode::LowNoiseCancellation,
    NothingAncMode::MidNoiseCancellation,
    NothingAncMode::HighNoiseCancellation,
];

/// A change made on the settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingChange {
    Theme(ThemePreference),
    TrayTextMode(bool),
    PreferredCodec(PreferredCodec),
    AutoSwitchOnPlayback(bool),
    A2dpReset(bool),
    HiResMicAgc(bool),
    HiResMicPauseConvo(bool),
    StemControl(bool),
}

/// The hint under the name field while the user edits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameHint {
    /// The draft can be sent.
    PressEnter,
    Invalid(&'static str),
}

impl NameHint {
    pub(crate) fn text(self) -> &'static str {
        match self {
            NameHint::PressEnter => "Press Enter to rename",
            NameHint::Invalid(hint) => hint,
        }
    }
}

/// The sidebar line of one device.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SidebarEntry {
    pub(crate) mac: String,
    pub(crate) name: String,
    pub(crate) connected: bool,
    pub(crate) status: SidebarStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SidebarStatus {
    Batteries(Batteries),
    Text(&'static str),
}

/// What the disconnected page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DisconnectedView {
    pub(crate) status: String,
    pub(crate) connecting: bool,
}

impl DisconnectedView {
    pub(crate) fn button_label(&self) -> &'static str {
        if self.connecting {
            "Connecting…"
        } else {
            "Connect to this PC"
        }
    }
}

const NOT_CONNECTED_HINT: &str =
    "Not connected to this PC. If they are on your phone, connecting here takes them over.";

pub(crate) struct Model {
    /// Contents of devices.json.
    devices: HashMap<String, DeviceData>,
    connected: HashSet<String>,
    airpods: HashMap<String, AirPods>,
    nothing: HashMap<String, Nothing>,
    /// Last case level each device reported, shown dimmed while the case is
    /// silent.
    last_case_level: HashMap<String, u8>,
    connects: ConnectRequests,
    selection: Selection,
    settings: AppSettings,
    /// Hint for the name field of the device being renamed.
    name_hint: Option<(String, NameHint)>,
}

impl Model {
    pub(crate) fn new(settings: AppSettings, devices: HashMap<String, DeviceData>) -> Self {
        let mut model = Model {
            devices: HashMap::new(),
            connected: HashSet::new(),
            airpods: HashMap::new(),
            nothing: HashMap::new(),
            last_case_level: HashMap::new(),
            connects: ConnectRequests::default(),
            selection: Selection::None,
            settings,
            name_hint: None,
        };
        model.set_devices(devices);
        model
    }

    pub(crate) fn update(&mut self, input: Input) -> Vec<Effect> {
        match input {
            Input::Backend(message) => self.backend(message),
            Input::DevicesLoaded(devices) => {
                self.set_devices(devices);
                Vec::new()
            },
            Input::Snapshot(mac, snapshot) => {
                self.snapshot(mac, snapshot);
                Vec::new()
            },
            Input::Select(selection) => {
                self.selection = selection;
                self.name_hint = None;
                Vec::new()
            },
            Input::Connect(mac) => self.start_connect(mac).into_iter().collect(),
            Input::ConnectFinished {
                mac,
                attempt,
                result,
            } => self.connect_finished(mac, attempt, result),
            Input::ConnectSetupTimedOut { mac, attempt } => {
                self.connects.setup_timed_out(&mac, attempt);
                self.connect_failure_toast(&mac).into_iter().collect()
            },
            Input::SetListeningMode(mac, mode) => {
                let value = vec![mode.to_byte()];
                self.change_airpods(mac, ControlCommandIdentifiers::ListeningMode, value, |a| {
                    a.listening_mode = mode;
                })
            },
            Input::SetConversationAwareness(mac, enabled) => {
                let Some(airpods) = self.airpods.get_mut(&mac) else {
                    return Vec::new();
                };
                airpods.conversation_awareness = enabled;
                vec![Effect::SetConversationDetection { mac, enabled }]
            },
            Input::SetPersonalizedVolume(mac, enabled) => self.change_airpods(
                mac,
                ControlCommandIdentifiers::AdaptiveVolumeConfig,
                on_off(enabled),
                |a| a.personalized_volume = enabled,
            ),
            Input::SetAllowOff(mac, enabled) => self.change_airpods(
                mac,
                ControlCommandIdentifiers::AllowOffOption,
                on_off(enabled),
                |a| a.allow_off = enabled,
            ),
            Input::NameEdited(mac, text) => {
                self.name_edited(mac, &text);
                Vec::new()
            },
            Input::Rename(mac, text) => self.rename(mac, &text),
            Input::SetNothingAnc(mac, mode) => {
                let Some(nothing) = self.nothing.get_mut(&mac) else {
                    return Vec::new();
                };
                let byte = mode.to_byte();
                nothing.anc_mode = mode;
                vec![Effect::SetNothingAnc { mac, mode: byte }]
            },
            Input::Setting(change) => self.change_setting(change),
            Input::Toast(message) => vec![Effect::Toast(message)],
        }
    }

    fn backend(&mut self, message: BluetoothUIMessage) -> Vec<Effect> {
        match message {
            BluetoothUIMessage::OpenWindow => vec![Effect::PresentWindow, Effect::LoadDevices],
            BluetoothUIMessage::ConnectAirPods => {
                let away: Vec<String> = self
                    .devices_of_type(&DeviceType::AirPods)
                    .filter(|mac| !self.connected.contains(*mac))
                    .cloned()
                    .collect();
                away.into_iter()
                    .filter_map(|mac| self.start_connect(mac))
                    .collect()
            },
            BluetoothUIMessage::DeviceConnected(mac) => {
                debug!("Device connected: {}", mac);
                self.connected.insert(mac.clone());
                self.connects.connected(&mac);
                vec![Effect::LoadDevices, Effect::Snapshot(mac)]
            },
            BluetoothUIMessage::DeviceDisconnected(mac) => {
                debug!("Device disconnected: {}", mac);
                self.connected.remove(&mac);
                self.connects.disconnected(&mac);
                self.airpods.remove(&mac);
                self.nothing.remove(&mac);
                if self.name_hint.as_ref().is_some_and(|(m, _)| *m == mac) {
                    self.name_hint = None;
                }
                Vec::new()
            },
            BluetoothUIMessage::AACPUIEvent(mac, event) => {
                self.aacp_event(&mac, event);
                // The AACP handlers save the device information and name to
                // devices.json without an event of their own; the events that
                // follow are the next chance to pick it up.
                vec![Effect::LoadDevices]
            },
        }
    }

    fn aacp_event(&mut self, mac: &str, event: AACPEvent) {
        match event {
            AACPEvent::BatteryInfo(battery) => {
                if let Some(level) = live_case_level(&battery) {
                    self.last_case_level.insert(mac.to_string(), level);
                }
                if let Some(airpods) = self.airpods.get_mut(mac) {
                    airpods.battery = battery;
                }
            },
            AACPEvent::ControlCommand(status) => {
                if let Some(airpods) = self.airpods.get_mut(mac) {
                    airpods.apply_control(status);
                }
            },
            AACPEvent::CustomEq(custom_eq) => {
                if let Some(airpods) = self.airpods.get_mut(mac) {
                    airpods.custom_eq = Some(custom_eq);
                }
            },
            _ => {},
        }
    }

    fn snapshot(&mut self, mac: String, snapshot: Option<DeviceSnapshot>) {
        // The device may have gone away while its state was being read.
        if !self.connected.contains(&mac) {
            return;
        }
        match snapshot {
            Some(DeviceSnapshot::AirPods(snapshot)) => {
                if let Some(level) = live_case_level(&snapshot.battery) {
                    self.last_case_level.insert(mac.clone(), level);
                }
                let name = self
                    .devices
                    .get(&mac)
                    .map_or_else(|| "AirPods".to_string(), |d| d.name.clone());
                self.airpods
                    .insert(mac, AirPods::from_snapshot(name, snapshot));
            },
            Some(DeviceSnapshot::Nothing) => {
                self.nothing.insert(
                    mac,
                    Nothing {
                        anc_mode: NothingAncMode::Off,
                    },
                );
            },
            None => error!("No manager for connected device {}", mac),
        }
    }

    fn set_devices(&mut self, devices: HashMap<String, DeviceData>) {
        self.devices = devices;
        if self.selection == Selection::None {
            self.selection = self
                .sorted_macs()
                .into_iter()
                .find(|mac| self.devices[mac].type_ == DeviceType::AirPods)
                .map_or(Selection::None, Selection::Device);
        }
    }

    fn start_connect(&mut self, mac: String) -> Option<Effect> {
        if self.connects.in_progress(&mac) {
            return None;
        }
        let Ok(address) = mac.parse::<Address>() else {
            error!("Cannot connect, invalid address {}", mac);
            return None;
        };
        let attempt = self.connects.start(mac.clone());
        Some(Effect::Connect {
            mac,
            address,
            attempt,
        })
    }

    fn connect_finished(
        &mut self,
        mac: String,
        attempt: u64,
        result: Result<(), String>,
    ) -> Vec<Effect> {
        if self.connects.finished(&mac, attempt, result) {
            return vec![Effect::SetupTimeout { mac, attempt }];
        }
        self.connect_failure_toast(&mac).into_iter().collect()
    }

    fn connect_failure_toast(&self, mac: &str) -> Option<Effect> {
        matches!(self.connects.get(mac), Some(ConnectStatus::Failed(_)))
            .then(|| Effect::Toast(format!("Could not connect {}", self.device_name(mac))))
    }

    /// Update the AirPods state at once so the UI does not jump back while the
    /// command is on its way, and send the command.
    fn change_airpods(
        &mut self,
        mac: String,
        identifier: ControlCommandIdentifiers,
        value: Vec<u8>,
        change: impl FnOnce(&mut AirPods),
    ) -> Vec<Effect> {
        let Some(airpods) = self.airpods.get_mut(&mac) else {
            return Vec::new();
        };
        change(airpods);
        airpods
            .control_values
            .insert(identifier as u8, value.clone());
        vec![Effect::SendControl {
            mac,
            identifier,
            value,
        }]
    }

    fn name_edited(&mut self, mac: String, text: &str) {
        let unchanged = self.airpods.get(&mac).is_some_and(|a| a.name == text);
        self.name_hint = if unchanged {
            None
        } else {
            let hint = match validate_device_name(text) {
                Ok(_) => NameHint::PressEnter,
                Err(e) => NameHint::Invalid(e),
            };
            Some((mac, hint))
        };
    }

    /// Rename the AirPods. An invalid name is not sent and its hint stays.
    fn rename(&mut self, mac: String, text: &str) -> Vec<Effect> {
        let Some(airpods) = self.airpods.get_mut(&mac) else {
            return Vec::new();
        };
        let name = match validate_device_name(text) {
            Ok(name) => name.to_string(),
            Err(e) => {
                self.name_hint = Some((mac, NameHint::Invalid(e)));
                return Vec::new();
            },
        };
        self.name_hint = None;
        if airpods.name == name {
            return Vec::new();
        }
        airpods.name.clone_from(&name);
        vec![Effect::Rename { mac, name }]
    }

    fn change_setting(&mut self, change: SettingChange) -> Vec<Effect> {
        let settings = &mut self.settings;
        let mut effects = Vec::with_capacity(2);
        match change {
            SettingChange::Theme(theme) => {
                settings.theme = theme;
                effects.push(Effect::ApplyTheme(theme));
            },
            SettingChange::TrayTextMode(on) => settings.tray_text_mode = on,
            SettingChange::PreferredCodec(codec) => settings.preferred_codec = codec,
            SettingChange::AutoSwitchOnPlayback(on) => settings.auto_switch_on_playback = on,
            SettingChange::A2dpReset(on) => settings.a2dp_reset = on,
            SettingChange::HiResMicAgc(on) => settings.hires_mic_agc = on,
            SettingChange::HiResMicPauseConvo(on) => settings.hires_mic_pause_convo = on,
            SettingChange::StemControl(on) => settings.stem_control = on,
        }
        effects.push(Effect::SaveSettings);
        effects
    }

    fn devices_of_type<'a>(&'a self, type_: &'a DeviceType) -> impl Iterator<Item = &'a String> {
        self.devices
            .iter()
            .filter(move |(_, d)| d.type_ == *type_)
            .map(|(mac, _)| mac)
    }

    /// Known devices by name, then address, so the order is stable.
    fn sorted_macs(&self) -> Vec<String> {
        let mut macs: Vec<&String> = self.devices.keys().collect();
        macs.sort_by(|a, b| {
            (self.devices[*a].name.as_str(), *a).cmp(&(self.devices[*b].name.as_str(), *b))
        });
        macs.into_iter().cloned().collect()
    }

    fn device_name<'a>(&'a self, mac: &'a str) -> &'a str {
        self.devices.get(mac).map_or(mac, |d| d.name.as_str())
    }

    // Read access for the view.

    pub(crate) fn selection(&self) -> &Selection {
        &self.selection
    }

    pub(crate) fn settings(&self) -> &AppSettings {
        &self.settings
    }

    pub(crate) fn theme(&self) -> ThemePreference {
        self.settings.theme
    }

    pub(crate) fn airpods(&self, mac: &str) -> Option<&AirPods> {
        self.airpods.get(mac)
    }

    pub(crate) fn nothing(&self, mac: &str) -> Option<&Nothing> {
        self.nothing.get(mac)
    }

    pub(crate) fn name_hint(&self, mac: &str) -> Option<NameHint> {
        self.name_hint
            .as_ref()
            .filter(|(m, _)| m == mac)
            .map(|(_, hint)| *hint)
    }

    /// The name shown in headers: the live name for connected AirPods, else
    /// the saved one.
    pub(crate) fn title<'a>(&'a self, mac: &'a str) -> &'a str {
        self.airpods
            .get(mac)
            .map_or_else(|| self.device_name(mac), |a| a.name.as_str())
    }

    pub(crate) fn information(&self, mac: &str) -> Option<&AirPodsInformation> {
        match self.devices.get(mac)?.information.as_ref()? {
            DeviceInformation::AirPods(info) => Some(info),
            DeviceInformation::Nothing(_) => None,
        }
    }

    pub(crate) fn batteries(&self, mac: &str) -> Option<Batteries> {
        let airpods = self.airpods.get(mac)?;
        Some(batteries(
            &airpods.battery,
            self.last_case_level.get(mac).copied(),
        ))
    }

    pub(crate) fn sidebar(&self) -> Vec<SidebarEntry> {
        self.sorted_macs()
            .into_iter()
            .map(|mac| {
                let connected = self.connected.contains(&mac);
                let status = match self.batteries(&mac) {
                    Some(shown) => SidebarStatus::Batteries(shown),
                    None if connected => SidebarStatus::Text("Connected"),
                    None => SidebarStatus::Text(sidebar_status(self.connects.get(&mac))),
                };
                SidebarEntry {
                    name: self.devices[&mac].name.clone(),
                    mac,
                    connected,
                    status,
                }
            })
            .collect()
    }

    pub(crate) fn content(&self) -> Content {
        let mac = match &self.selection {
            Selection::None => return Content::Empty,
            Selection::Settings => return Content::Settings,
            Selection::Device(mac) => mac.clone(),
        };
        let connected = self.connected.contains(&mac);
        match self.devices.get(&mac).map(|d| &d.type_) {
            _ if self.airpods.contains_key(&mac) => Content::AirPods(mac),
            _ if self.nothing.contains_key(&mac) => Content::Nothing(mac),
            _ if connected => Content::Waiting(mac),
            Some(DeviceType::AirPods) => Content::Disconnected(mac),
            Some(DeviceType::Nothing) => Content::Unavailable(mac),
            // Forgotten from devices.json while selected.
            None => Content::Empty,
        }
    }

    pub(crate) fn disconnected(&self, mac: &str) -> DisconnectedView {
        match self.connects.get(mac) {
            Some(ConnectStatus::Connecting(_) | ConnectStatus::SettingUp(_)) => DisconnectedView {
                status: "Connecting…".to_string(),
                connecting: true,
            },
            Some(ConnectStatus::Failed(e)) => DisconnectedView {
                status: e.clone(),
                connecting: false,
            },
            None => DisconnectedView {
                status: NOT_CONNECTED_HINT.to_string(),
                connecting: false,
            },
        }
    }
}

fn on_off(enabled: bool) -> Vec<u8> {
    vec![if enabled { 0x01 } else { 0x02 }]
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bluetooth::aacp::{BatteryComponent, BatteryStatus},
            ui::connect::SETUP_TIMEOUT_ERROR,
        },
    };

    const PODS: &str = "AA:BB:CC:DD:EE:01";
    const OTHER: &str = "AA:BB:CC:DD:EE:02";
    const EAR: &str = "AA:BB:CC:DD:EE:03";

    fn device(name: &str, type_: DeviceType) -> DeviceData {
        DeviceData {
            name: name.to_string(),
            type_,
            information: None,
        }
    }

    fn model() -> Model {
        let devices = HashMap::from([
            (PODS.to_string(), device("Pods", DeviceType::AirPods)),
            (OTHER.to_string(), device("Alpha Pods", DeviceType::AirPods)),
            (EAR.to_string(), device("Ear", DeviceType::Nothing)),
        ]);
        Model::new(AppSettings::default(), devices)
    }

    fn status(identifier: ControlCommandIdentifiers, value: &[u8]) -> ControlCommandStatus {
        ControlCommandStatus {
            identifier,
            value: value.to_vec(),
        }
    }

    fn battery(component: BatteryComponent, level: u8, status: BatteryStatus) -> BatteryInfo {
        BatteryInfo {
            component,
            level,
            status,
        }
    }

    /// Connect PODS and deliver its snapshot.
    fn connected(model: &mut Model, snapshot: AirPodsSnapshot) {
        let effects = model.update(Input::Backend(BluetoothUIMessage::DeviceConnected(
            PODS.to_string(),
        )));
        assert_eq!(
            effects,
            [Effect::LoadDevices, Effect::Snapshot(PODS.to_string())]
        );
        model.update(Input::Snapshot(
            PODS.to_string(),
            Some(DeviceSnapshot::AirPods(snapshot)),
        ));
    }

    fn airpods_event(model: &mut Model, event: AACPEvent) -> Vec<Effect> {
        model.update(Input::Backend(BluetoothUIMessage::AACPUIEvent(
            PODS.to_string(),
            event,
        )))
    }

    #[test]
    fn first_airpods_by_name_is_selected_at_start() {
        let model = model();
        assert_eq!(model.selection(), &Selection::Device(OTHER.to_string()));
        assert_eq!(model.content(), Content::Disconnected(OTHER.to_string()));
    }

    #[test]
    fn no_devices_means_empty_content() {
        let model = Model::new(AppSettings::default(), HashMap::new());
        assert_eq!(model.content(), Content::Empty);
        assert!(model.sidebar().is_empty());
    }

    #[test]
    fn sidebar_is_sorted_by_name_with_status() {
        let mut model = model();
        let names: Vec<String> = model.sidebar().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["Alpha Pods", "Ear", "Pods"]);

        model.update(Input::Backend(BluetoothUIMessage::DeviceConnected(
            PODS.to_string(),
        )));
        let pods = model.sidebar().into_iter().find(|e| e.mac == PODS).unwrap();
        assert!(pods.connected);
        assert_eq!(pods.status, SidebarStatus::Text("Connected"));
        let ear = model.sidebar().into_iter().find(|e| e.mac == EAR).unwrap();
        assert_eq!(ear.status, SidebarStatus::Text("Not connected"));
    }

    #[test]
    fn selection_switches_the_content() {
        let mut model = model();
        model.update(Input::Select(Selection::Settings));
        assert_eq!(model.content(), Content::Settings);
        model.update(Input::Select(Selection::Device(EAR.to_string())));
        assert_eq!(model.content(), Content::Unavailable(EAR.to_string()));
        model.update(Input::Select(Selection::Device(PODS.to_string())));
        assert_eq!(model.content(), Content::Disconnected(PODS.to_string()));
    }

    #[test]
    fn connected_airpods_wait_for_their_snapshot() {
        let mut model = model();
        model.update(Input::Select(Selection::Device(PODS.to_string())));
        model.update(Input::Backend(BluetoothUIMessage::DeviceConnected(
            PODS.to_string(),
        )));
        assert_eq!(model.content(), Content::Waiting(PODS.to_string()));

        model.update(Input::Snapshot(
            PODS.to_string(),
            Some(DeviceSnapshot::AirPods(AirPodsSnapshot {
                controls: vec![
                    status(ControlCommandIdentifiers::ListeningMode, &[0x02]),
                    status(ControlCommandIdentifiers::ConversationDetectConfig, &[0x01]),
                    status(ControlCommandIdentifiers::AdaptiveVolumeConfig, &[0x02]),
                    status(ControlCommandIdentifiers::AllowOffOption, &[0x01]),
                    status(ControlCommandIdentifiers::ChimeVolume, &[0x40]),
                ],
                ..AirPodsSnapshot::default()
            })),
        ));
        assert_eq!(model.content(), Content::AirPods(PODS.to_string()));
        let airpods = model.airpods(PODS).unwrap();
        assert_eq!(airpods.name, "Pods");
        assert_eq!(
            airpods.listening_mode.to_byte(),
            AirPodsNoiseControlMode::NoiseCancellation.to_byte()
        );
        assert!(airpods.conversation_awareness);
        assert!(!airpods.personalized_volume);
        assert!(airpods.allow_off);
        assert_eq!(
            airpods
                .control_values
                .get(&(ControlCommandIdentifiers::ChimeVolume as u8)),
            Some(&vec![0x40])
        );
    }

    #[test]
    fn snapshot_after_disconnect_is_dropped() {
        let mut model = model();
        model.update(Input::Backend(BluetoothUIMessage::DeviceConnected(
            PODS.to_string(),
        )));
        model.update(Input::Backend(BluetoothUIMessage::DeviceDisconnected(
            PODS.to_string(),
        )));
        model.update(Input::Snapshot(
            PODS.to_string(),
            Some(DeviceSnapshot::AirPods(AirPodsSnapshot::default())),
        ));
        assert!(model.airpods(PODS).is_none());
    }

    #[test]
    fn disconnect_drops_state_and_keeps_the_airpods_selected() {
        let mut model = model();
        model.update(Input::Select(Selection::Device(PODS.to_string())));
        connected(&mut model, AirPodsSnapshot::default());
        model.update(Input::NameEdited(PODS.to_string(), "New".to_string()));

        model.update(Input::Backend(BluetoothUIMessage::DeviceDisconnected(
            PODS.to_string(),
        )));

        assert!(model.airpods(PODS).is_none());
        assert_eq!(model.name_hint(PODS), None);
        assert_eq!(model.content(), Content::Disconnected(PODS.to_string()));
    }

    #[test]
    fn nothing_devices_get_their_own_page() {
        let mut model = model();
        model.update(Input::Select(Selection::Device(EAR.to_string())));
        model.update(Input::Backend(BluetoothUIMessage::DeviceConnected(
            EAR.to_string(),
        )));
        model.update(Input::Snapshot(
            EAR.to_string(),
            Some(DeviceSnapshot::Nothing),
        ));
        assert_eq!(model.content(), Content::Nothing(EAR.to_string()));

        let effects = model.update(Input::SetNothingAnc(
            EAR.to_string(),
            NothingAncMode::Transparency,
        ));
        assert_eq!(
            effects,
            [Effect::SetNothingAnc {
                mac: EAR.to_string(),
                mode: 0x07
            }]
        );
    }

    #[test]
    fn events_update_state_and_reload_devices() {
        let mut model = model();
        connected(&mut model, AirPodsSnapshot::default());

        let effects = airpods_event(
            &mut model,
            AACPEvent::ControlCommand(status(ControlCommandIdentifiers::AllowOffOption, &[0x01])),
        );
        assert_eq!(effects, [Effect::LoadDevices]);
        let offered: Vec<u8> = model
            .airpods(PODS)
            .unwrap()
            .listening_modes()
            .iter()
            .map(AirPodsNoiseControlMode::to_byte)
            .collect();
        assert_eq!(offered, [0x01, 0x03, 0x02, 0x04]);

        airpods_event(
            &mut model,
            AACPEvent::ControlCommand(status(ControlCommandIdentifiers::AllowOffOption, &[0x02])),
        );
        assert_eq!(model.airpods(PODS).unwrap().listening_modes().len(), 3);
    }

    #[test]
    fn events_for_unknown_devices_change_nothing() {
        let mut model = model();
        let effects = airpods_event(
            &mut model,
            AACPEvent::BatteryInfo(vec![battery(
                BatteryComponent::Left,
                50,
                BatteryStatus::NotCharging,
            )]),
        );
        assert_eq!(effects, [Effect::LoadDevices]);
        assert!(model.airpods(PODS).is_none());
    }

    #[test]
    fn silent_case_shows_the_remembered_level() {
        let mut model = model();
        connected(
            &mut model,
            AirPodsSnapshot {
                battery: vec![battery(
                    BatteryComponent::Case,
                    64,
                    BatteryStatus::NotCharging,
                )],
                ..AirPodsSnapshot::default()
            },
        );
        airpods_event(
            &mut model,
            AACPEvent::BatteryInfo(vec![
                battery(BatteryComponent::Left, 90, BatteryStatus::NotCharging),
                battery(BatteryComponent::Right, 80, BatteryStatus::Charging),
                battery(BatteryComponent::Case, 0, BatteryStatus::Disconnected),
            ]),
        );
        let Some(Batteries::Buds { left, right, case }) = model.batteries(PODS) else {
            panic!("earbuds expected");
        };
        assert_eq!(left.percent, Some(90));
        assert!(right.charging);
        assert_eq!((case.percent, case.stale), (Some(64), true));

        let pods = model.sidebar().into_iter().find(|e| e.mac == PODS).unwrap();
        assert!(matches!(pods.status, SidebarStatus::Batteries(_)));
    }

    #[test]
    fn toggles_update_at_once_and_send_the_command() {
        let mut model = model();
        connected(&mut model, AirPodsSnapshot::default());

        let effects = model.update(Input::SetPersonalizedVolume(PODS.to_string(), true));
        assert_eq!(
            effects,
            [Effect::SendControl {
                mac: PODS.to_string(),
                identifier: ControlCommandIdentifiers::AdaptiveVolumeConfig,
                value: vec![0x01],
            }]
        );
        assert!(model.airpods(PODS).unwrap().personalized_volume);

        let effects = model.update(Input::SetAllowOff(PODS.to_string(), false));
        assert!(matches!(
            effects.as_slice(),
            [Effect::SendControl { value, .. }] if *value == [0x02]
        ));

        let effects = model.update(Input::SetListeningMode(
            PODS.to_string(),
            AirPodsNoiseControlMode::Adaptive,
        ));
        assert!(matches!(
            effects.as_slice(),
            [Effect::SendControl { identifier: ControlCommandIdentifiers::ListeningMode, value, .. }]
                if *value == [0x04]
        ));

        let effects = model.update(Input::SetConversationAwareness(PODS.to_string(), true));
        assert_eq!(
            effects,
            [Effect::SetConversationDetection {
                mac: PODS.to_string(),
                enabled: true
            }]
        );
        assert!(model.airpods(PODS).unwrap().conversation_awareness);
    }

    #[test]
    fn toggles_for_disconnected_airpods_send_nothing() {
        let mut model = model();
        assert!(
            model
                .update(Input::SetPersonalizedVolume(PODS.to_string(), true))
                .is_empty()
        );
        assert!(
            model
                .update(Input::SetConversationAwareness(PODS.to_string(), true))
                .is_empty()
        );
    }

    #[test]
    fn rename_sends_a_valid_trimmed_name() {
        let mut model = model();
        connected(&mut model, AirPodsSnapshot::default());

        model.update(Input::NameEdited(PODS.to_string(), " Mine ".to_string()));
        assert_eq!(model.name_hint(PODS), Some(NameHint::PressEnter));
        let effects = model.update(Input::Rename(PODS.to_string(), " Mine ".to_string()));

        assert_eq!(
            effects,
            [Effect::Rename {
                mac: PODS.to_string(),
                name: "Mine".to_string()
            }]
        );
        assert_eq!(model.airpods(PODS).unwrap().name, "Mine");
        assert_eq!(model.title(PODS), "Mine");
        assert_eq!(model.name_hint(PODS), None);
    }

    #[test]
    fn invalid_name_is_not_sent_and_keeps_its_hint() {
        let mut model = model();
        connected(&mut model, AirPodsSnapshot::default());

        model.update(Input::NameEdited(PODS.to_string(), "  ".to_string()));
        assert_eq!(
            model.name_hint(PODS),
            Some(NameHint::Invalid("Name can't be empty"))
        );
        let long = "a".repeat(33);
        let effects = model.update(Input::Rename(PODS.to_string(), long));
        assert!(effects.is_empty());
        assert_eq!(
            model.name_hint(PODS).map(NameHint::text),
            Some("Name is too long")
        );
        assert_eq!(model.airpods(PODS).unwrap().name, "Pods");
    }

    #[test]
    fn editing_back_to_the_current_name_clears_the_hint() {
        let mut model = model();
        connected(&mut model, AirPodsSnapshot::default());
        model.update(Input::NameEdited(PODS.to_string(), "Pod".to_string()));
        model.update(Input::NameEdited(PODS.to_string(), "Pods".to_string()));
        assert_eq!(model.name_hint(PODS), None);
        assert!(
            model
                .update(Input::Rename(PODS.to_string(), "Pods".to_string()))
                .is_empty()
        );
    }

    #[test]
    fn selecting_another_page_drops_the_name_hint() {
        let mut model = model();
        connected(&mut model, AirPodsSnapshot::default());
        model.update(Input::NameEdited(PODS.to_string(), String::new()));
        model.update(Input::Select(Selection::Settings));
        assert_eq!(model.name_hint(PODS), None);
    }

    #[test]
    fn connect_flow_success() {
        let mut model = model();
        let effects = model.update(Input::Connect(PODS.to_string()));
        let [Effect::Connect { attempt, .. }] = effects.as_slice() else {
            panic!("connect expected, got {effects:?}");
        };
        let attempt = *attempt;
        assert!(model.disconnected(PODS).connecting);
        assert_eq!(model.disconnected(PODS).button_label(), "Connecting…");
        // A second click while connecting does nothing.
        assert!(model.update(Input::Connect(PODS.to_string())).is_empty());

        let effects = model.update(Input::ConnectFinished {
            mac: PODS.to_string(),
            attempt,
            result: Ok(()),
        });
        assert_eq!(
            effects,
            [Effect::SetupTimeout {
                mac: PODS.to_string(),
                attempt
            }]
        );
        assert!(model.disconnected(PODS).connecting);

        model.update(Input::Backend(BluetoothUIMessage::DeviceConnected(
            PODS.to_string(),
        )));
        assert!(
            model
                .update(Input::ConnectSetupTimedOut {
                    mac: PODS.to_string(),
                    attempt
                })
                .is_empty()
        );
        let pods = model.sidebar().into_iter().find(|e| e.mac == PODS).unwrap();
        assert_eq!(pods.status, SidebarStatus::Text("Connected"));
    }

    #[test]
    fn connect_flow_failure_shows_the_error_and_a_toast() {
        let mut model = model();
        model.update(Input::Connect(PODS.to_string()));
        let effects = model.update(Input::ConnectFinished {
            mac: PODS.to_string(),
            attempt: 1,
            result: Err("The AirPods did not answer.".to_string()),
        });
        assert_eq!(
            effects,
            [Effect::Toast("Could not connect Pods".to_string())]
        );
        let view = model.disconnected(PODS);
        assert_eq!(view.status, "The AirPods did not answer.");
        assert!(!view.connecting);
        assert_eq!(view.button_label(), "Connect to this PC");
        let pods = model.sidebar().into_iter().find(|e| e.mac == PODS).unwrap();
        assert_eq!(pods.status, SidebarStatus::Text("Couldn't connect"));
    }

    #[test]
    fn setup_timeout_fails_the_attempt() {
        let mut model = model();
        model.update(Input::Connect(PODS.to_string()));
        model.update(Input::ConnectFinished {
            mac: PODS.to_string(),
            attempt: 1,
            result: Ok(()),
        });
        let effects = model.update(Input::ConnectSetupTimedOut {
            mac: PODS.to_string(),
            attempt: 1,
        });
        assert_eq!(
            effects,
            [Effect::Toast("Could not connect Pods".to_string())]
        );
        assert_eq!(model.disconnected(PODS).status, SETUP_TIMEOUT_ERROR);
    }

    #[test]
    fn tray_connect_starts_every_away_airpods() {
        let mut model = model();
        model.update(Input::Backend(BluetoothUIMessage::DeviceConnected(
            PODS.to_string(),
        )));
        let effects = model.update(Input::Backend(BluetoothUIMessage::ConnectAirPods));
        let macs: Vec<&str> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Connect { mac, .. } => Some(mac.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(macs, [OTHER]);
    }

    #[test]
    fn open_window_presents_and_reloads() {
        let mut model = model();
        assert_eq!(
            model.update(Input::Backend(BluetoothUIMessage::OpenWindow)),
            [Effect::PresentWindow, Effect::LoadDevices]
        );
    }

    #[test]
    fn settings_change_saves_and_theme_applies() {
        let mut model = model();
        let effects = model.update(Input::Setting(SettingChange::TrayTextMode(true)));
        assert_eq!(effects, [Effect::SaveSettings]);
        assert!(model.settings().tray_text_mode);

        let effects = model.update(Input::Setting(SettingChange::Theme(ThemePreference::Dark)));
        assert_eq!(
            effects,
            [
                Effect::ApplyTheme(ThemePreference::Dark),
                Effect::SaveSettings
            ]
        );
        assert_eq!(model.settings().theme, ThemePreference::Dark);
        assert_eq!(model.theme(), ThemePreference::Dark);
    }

    #[test]
    fn messages_become_toasts() {
        let mut model = model();
        assert_eq!(
            model.update(Input::Toast("x".to_string())),
            [Effect::Toast("x".to_string())]
        );
    }
}
