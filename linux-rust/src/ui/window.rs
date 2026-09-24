use crate::bluetooth::aacp::{
    AACPEvent, BatteryComponent, BatteryInfo, BatteryStatus, ControlCommandIdentifiers,
};
use crate::bluetooth::managers::DeviceManagers;
use crate::devices::enums::{
    AirPodsNoiseControlMode, AirPodsState, DeviceData, DeviceState, DeviceType, NothingAncMode,
    NothingState,
};
use crate::audio::{mic_test, output};
use crate::ui::airpods::airpods_view;
use crate::ui::messages::BluetoothUIMessage;
use crate::ui::nothing::nothing_view;
use crate::utils::{
    AppSettings, MyTheme, PreferredCodec, get_devices_path, update_devices_file,
};
use bluer::Address;
use iced::border::Radius;
use iced::overlay::menu;
use iced::widget::button::Style;
use iced::widget::rule::FillMode;
use iced::widget::{
    Space, button, column, combo_box, container, pane_grid, pick_list, row, rule, scrollable,
    text, text_input, toggler,
};
use iced::{
    Background, Border, Center, Element, Font, Length, Padding, Program, Settings, Size,
    Subscription, Task, Theme, daemon, window,
};
use log::{debug, error, warn};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{Mutex, RwLock};

pub fn start_ui(
    ui_rx: UnboundedReceiver<BluetoothUIMessage>,
    start_minimized: bool,
    device_managers: Arc<RwLock<HashMap<String, DeviceManagers>>>,
    // stem_control: Arc<AtomicBool>,
) -> iced::Result {
    let ui_rx = Arc::new(Mutex::new(ui_rx));

    // not sure if this is a good idea
    daemon(
        move || {
            App::new(
                Arc::clone(&ui_rx),
                start_minimized,
                Arc::clone(&device_managers),
                // Arc::clone(&stem_control),
            )
        },
        App::update,
        App::view,
    )
    .subscription(App::subscription)
    .theme(App::theme)
    .title(App::title)
    .settings(Settings {
        id: Some("librepods".to_string()),
        fonts: vec![
            include_bytes!("../../assets/font/sf_pro.otf")
                .as_slice()
                .into(),
        ],
        default_font: Font::with_name("SF Pro Text"),
        ..Settings::default()
    })
    .run()
}

/// Redraw rate while the level meter or the microphone test is on screen.
const MIC_TICK: Duration = Duration::from_millis(50);
/// Rate at which to check whether an app opened the hi-res microphone.
const MIC_WATCH: Duration = Duration::from_secs(1);

pub struct App {
    window: Option<window::Id>,
    panes: pane_grid::State<Pane>,
    selected_tab: Tab,
    theme_state: combo_box::State<MyTheme>,
    selected_theme: MyTheme,
    ui_rx: Arc<Mutex<UnboundedReceiver<BluetoothUIMessage>>>,
    bluetooth_state: BluetoothState,
    paired_devices: HashMap<String, Address>,
    device_states: HashMap<String, DeviceState>,
    device_managers: Arc<RwLock<HashMap<String, DeviceManagers>>>,
    pending_add_device: Option<(String, Address)>,
    device_type_state: combo_box::State<DeviceType>,
    selected_device_type: Option<DeviceType>,
    tray_text_mode: bool,
    stem_control: bool,
    hires_mic_enabled: bool,
    hires_mic_agc: bool,
    hires_mic_pause_convo: bool,
    a2dp_reset: bool,
    auto_switch_on_playback: bool,
    preferred_codec: PreferredCodec,
    // Manual connect requests from the UI or tray, keyed by MAC; cleared once
    // the device connects.
    connect_status: HashMap<String, ConnectStatus>,
    mic_test: MicTest,
    // Media players paused for the microphone test, resumed when it ends.
    mic_test_paused: Vec<String>,
    // Last case level each device reported. The case only reports while a bud
    // is in it, so the sidebar shows this, dimmed, the rest of the time.
    last_case_level: HashMap<String, u8>,
    // Contents of devices.json. Reloaded on the messages that can change the
    // file, never from view(), which runs on every frame.
    devices: HashMap<String, DeviceData>,
}

/// The microphone test on the AirPods page. Holds the live recorder or player.
pub enum MicTest {
    Idle,
    Recording(mic_test::Recorder),
    Ready(mic_test::Player),
    Failed(String),
}

#[derive(Debug, Clone)]
enum ConnectStatus {
    Connecting,
    Failed(String),
}

// The icon is embedded: a path is resolved against the working directory, which
// is wherever the app was launched from. The application id becomes the X11
// WM_CLASS / Wayland app_id, which GNOME matches against the .desktop file name
// to show the right icon in the dock and the alt-tab switcher.
fn main_window_settings() -> window::Settings {
    let mut settings = window::Settings::default();
    settings.min_size = Some(Size::new(400.0, 300.0));
    settings.icon = window::icon::from_file_data(include_bytes!("../../assets/icon.png"), None)
        .map_err(|e| warn!("Failed to load window icon: {}", e))
        .ok();
    settings.platform_specific.application_id = "me.kavishdevar.librepods".to_string();
    settings
}

pub struct BluetoothState {
    connected_devices: Vec<String>,
}

impl BluetoothState {
    pub fn new() -> Self {
        Self {
            connected_devices: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    WindowOpened(window::Id),
    WindowClosed(window::Id),
    Resized(pane_grid::ResizeEvent),
    SelectTab(Tab),
    ThemeSelected(MyTheme),
    CopyToClipboard(String),
    BluetoothMessage(BluetoothUIMessage),
    ConnectDevice(String),
    ConnectFinished(String, Result<(), String>),
    MicTestRecord,
    MicTestStop,
    MicTestPlay,
    MicTestPause,
    MicTestSkip(bool), // true = forward
    MicTestSeek(f32),  // seconds
    MicTestDone,
    // ShowNewDialogTab,
    GotPairedDevices(HashMap<String, Address>),
    StartAddDevice(String, Address),
    SelectDeviceType(DeviceType),
    ConfirmAddDevice,
    CancelAddDevice,
    StateChanged(String, DeviceState),
    TrayTextModeChanged(bool), // yes, I know I should add all settings to a struct, but I'm lazy
    StemControlChanged(bool),
    A2dpResetChanged(bool),
    AutoSwitchChanged(bool),
    PreferredCodecChanged(PreferredCodec),
    HiResMicAgcChanged(bool),
    HiResMicPauseConvoChanged(bool),
    MicLevelTick,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Tab {
    Device(String),
    Settings,
    AddDevice,
}

#[derive(Clone, Copy)]
pub enum Pane {
    Sidebar,
    Content,
}

impl App {
    pub fn new(
        ui_rx: Arc<Mutex<UnboundedReceiver<BluetoothUIMessage>>>,
        start_minimized: bool,
        device_managers: Arc<RwLock<HashMap<String, DeviceManagers>>>,
        // stem_control: Arc<AtomicBool>,
    ) -> (Self, Task<Message>) {
        let (mut panes, first_pane) = pane_grid::State::new(Pane::Sidebar);
        let split = panes.split(pane_grid::Axis::Vertical, first_pane, Pane::Content);
        panes.resize(split.unwrap().1, 0.2);

        let wait_task = Task::perform(wait_for_message(Arc::clone(&ui_rx)), |msg| msg);

        let (window, open_task) = if start_minimized {
            (None, Task::none())
        } else {
            let (id, open) = window::open(main_window_settings());
            (Some(id), open.map(Message::WindowOpened))
        };

        let app_settings = AppSettings::load();
        let selected_theme = app_settings.theme;
        let tray_text_mode = app_settings.tray_text_mode;
        let stem_control = app_settings.stem_control;
        let hires_mic_enabled = app_settings.hires_mic_enabled;
        let hires_mic_agc = app_settings.hires_mic_agc;
        let hires_mic_pause_convo = app_settings.hires_mic_pause_convo;
        let a2dp_reset = app_settings.a2dp_reset;
        let auto_switch_on_playback = app_settings.auto_switch_on_playback;
        let preferred_codec = app_settings.preferred_codec;

        let bluetooth_state = BluetoothState::new();

        // let dummy_device_state = DeviceState::AirPods(AirPodsState {
        //     conversation_awareness_enabled: false,
        // });
        // let device_states = HashMap::from([
        //     ("28:2D:7F:C2:05:5B".to_string(), dummy_device_state),
        // ]);

        let device_states = HashMap::new();
        (
            Self {
                window,
                panes,
                selected_tab: Tab::Device("none".to_string()),
                theme_state: combo_box::State::new(vec![
                    MyTheme::Light,
                    MyTheme::Dark,
                    MyTheme::Dracula,
                    MyTheme::Nord,
                    MyTheme::SolarizedLight,
                    MyTheme::SolarizedDark,
                    MyTheme::GruvboxLight,
                    MyTheme::GruvboxDark,
                    MyTheme::CatppuccinLatte,
                    MyTheme::CatppuccinFrappe,
                    MyTheme::CatppuccinMacchiato,
                    MyTheme::CatppuccinMocha,
                    MyTheme::TokyoNight,
                    MyTheme::TokyoNightStorm,
                    MyTheme::TokyoNightLight,
                    MyTheme::KanagawaWave,
                    MyTheme::KanagawaDragon,
                    MyTheme::KanagawaLotus,
                    MyTheme::Moonfly,
                    MyTheme::Nightfly,
                    MyTheme::Oxocarbon,
                    MyTheme::Ferra,
                ]),
                selected_theme,
                ui_rx,
                bluetooth_state,
                paired_devices: HashMap::new(),
                device_states,
                pending_add_device: None,
                device_type_state: combo_box::State::new(vec![DeviceType::Nothing]),
                selected_device_type: None,
                device_managers,
                tray_text_mode,
                stem_control,
                hires_mic_enabled,
                hires_mic_agc,
                hires_mic_pause_convo,
                a2dp_reset,
                auto_switch_on_playback,
                preferred_codec,
                connect_status: HashMap::new(),
                mic_test: MicTest::Idle,
                mic_test_paused: Vec::new(),
                last_case_level: HashMap::new(),
                devices: load_devices(),
            },
            Task::batch(vec![open_task, wait_task]),
        )
    }

    fn save_settings(&self) {
        AppSettings {
            theme: self.selected_theme,
            tray_text_mode: self.tray_text_mode,
            stem_control: self.stem_control,
            hires_mic_enabled: self.hires_mic_enabled,
            hires_mic_agc: self.hires_mic_agc,
            hires_mic_pause_convo: self.hires_mic_pause_convo,
            a2dp_reset: self.a2dp_reset,
            auto_switch_on_playback: self.auto_switch_on_playback,
            preferred_codec: self.preferred_codec,
        }
        .save();
    }

    fn title(&self, _id: window::Id) -> String {
        "LibrePods".to_string()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowOpened(id) => {
                self.window = Some(id);
                self.devices = load_devices();
                Task::none()
            }
            Message::WindowClosed(id) => {
                if self.window == Some(id) {
                    self.window = None;
                }
                Task::none()
            }
            Message::Resized(event) => {
                self.panes.resize(event.split, event.ratio);
                Task::none()
            }
            Message::SelectTab(tab) => {
                self.selected_tab = tab;
                Task::none()
            }
            Message::ThemeSelected(theme) => {
                self.selected_theme = theme;
                self.save_settings();
                Task::none()
            }
            Message::CopyToClipboard(data) => iced::clipboard::write(data),
            Message::ConnectDevice(mac) => self.start_connect(mac),
            Message::MicTestRecord => {
                // Music would play over the recording and the playback, so pause it
                // for the whole test. Players already paused by an earlier take stay listed.
                if self.mic_test_paused.is_empty() {
                    self.mic_test_paused = output::pause_media_players();
                }
                self.mic_test = MicTest::Recording(mic_test::Recorder::start());
                Task::none()
            }
            Message::MicTestStop => {
                self.stop_recording();
                Task::none()
            }
            Message::MicTestPlay => {
                if let MicTest::Ready(player) = &self.mic_test {
                    player.play();
                }
                Task::none()
            }
            Message::MicTestPause => {
                if let MicTest::Ready(player) = &self.mic_test {
                    player.pause();
                }
                Task::none()
            }
            Message::MicTestSkip(forward) => {
                if let MicTest::Ready(player) = &self.mic_test {
                    let at = player.position();
                    let to = if forward {
                        at + mic_test::SKIP
                    } else {
                        at.saturating_sub(mic_test::SKIP)
                    };
                    player.seek(to.min(player.duration()));
                }
                Task::none()
            }
            Message::MicTestSeek(secs) => {
                if let MicTest::Ready(player) = &self.mic_test {
                    player.seek(Duration::from_secs_f32(secs.max(0.0)));
                }
                Task::none()
            }
            Message::MicTestDone => {
                self.mic_test = MicTest::Idle;
                output::resume_media_players(&std::mem::take(&mut self.mic_test_paused));
                Task::none()
            }
            Message::ConnectFinished(mac, result) => {
                match result {
                    // DeviceConnected arrives separately once the device is set up.
                    Ok(()) => {
                        self.connect_status.remove(&mac);
                    }
                    Err(e) => {
                        self.connect_status.insert(mac, ConnectStatus::Failed(e));
                    }
                }
                Task::none()
            }
            Message::MicLevelTick => {
                if matches!(&self.mic_test, MicTest::Recording(r) if r.finished()) {
                    self.stop_recording();
                }
                Task::none()
            }
            Message::BluetoothMessage(ui_message) => {
                match ui_message {
                    BluetoothUIMessage::NoOp => {
                        let ui_rx = Arc::clone(&self.ui_rx);

                        Task::perform(wait_for_message(ui_rx), |msg| msg)
                    }
                    BluetoothUIMessage::ConnectAirPods => {
                        let ui_rx = Arc::clone(&self.ui_rx);
                        let mut tasks = vec![Task::perform(wait_for_message(ui_rx), |msg| msg)];
                        let away: Vec<String> = crate::auto_switch::known_airpods()
                            .iter()
                            .map(|a| a.to_string())
                            .filter(|mac| !self.bluetooth_state.connected_devices.contains(mac))
                            .collect();
                        for mac in away {
                            tasks.push(self.start_connect(mac));
                        }
                        Task::batch(tasks)
                    }
                    BluetoothUIMessage::OpenWindow => {
                        let ui_rx = Arc::clone(&self.ui_rx);
                        let wait_task = Task::perform(wait_for_message(ui_rx), |msg| msg);
                        debug!("Opening main window...");
                        self.devices = load_devices();
                        if let Some(window_id) = self.window {
                            Task::batch(vec![window::gain_focus(window_id), wait_task])
                        } else {
                            let (new_window_task, open_task) = window::open(main_window_settings());
                            self.window = Some(new_window_task);
                            Task::batch(vec![open_task.map(Message::WindowOpened), wait_task])
                        }
                    }
                    BluetoothUIMessage::DeviceConnected(mac) => {
                        let ui_rx = Arc::clone(&self.ui_rx);
                        let wait_task = Task::perform(wait_for_message(ui_rx), |msg| msg);
                        debug!(
                            "Device connected: {}. Adding to connected devices list",
                            mac
                        );
                        let mut already_connected = false;
                        for device in &self.bluetooth_state.connected_devices {
                            if device == &mac {
                                already_connected = true;
                                break;
                            }
                        }
                        if !already_connected {
                            self.bluetooth_state.connected_devices.push(mac.clone());
                        }
                        self.connect_status.remove(&mac);

                        // self.device_states.insert(mac.clone(), DeviceState::AirPods(AirPodsState {
                        //     conversation_awareness_enabled: false,
                        // }));

                        self.devices = load_devices();
                        let type_ = self.devices.get(&mac).map(|d| d.type_.clone());
                        match type_ {
                            Some(DeviceType::AirPods) => {
                                let managers = Arc::clone(&self.device_managers);
                                let device_managers = managers.blocking_read();
                                let device_manager = device_managers.get(&mac).unwrap();
                                let aacp_manager = device_manager.get_aacp().unwrap();
                                let aacp_manager_state = aacp_manager.state.clone();
                                let state = aacp_manager_state.blocking_lock();
                                debug!("AACP manager found for AirPods device {}", mac);
                                let device_name = self
                                    .devices
                                    .get(&mac)
                                    .map(|d| d.name.clone())
                                    .unwrap_or_else(|| "Unknown Device".to_string());
                                self.remember_case_level(&mac, &state.battery_info);
                                self.device_states.insert(mac.clone(), DeviceState::AirPods(AirPodsState {
                                    device_name,
                                    battery: state.battery_info.clone(),
                                    noise_control_mode: state.control_command_status_list.iter().find_map(|status| {
                                        if status.identifier == ControlCommandIdentifiers::ListeningMode {
                                            status.value.first().map(AirPodsNoiseControlMode::from_byte)
                                        } else {
                                            None
                                        }
                                    }).unwrap_or(AirPodsNoiseControlMode::Transparency),
                                    noise_control_state: combo_box::State::new(
                                        {
                                            let mut modes = vec![
                                                AirPodsNoiseControlMode::Transparency,
                                                AirPodsNoiseControlMode::NoiseCancellation,
                                                AirPodsNoiseControlMode::Adaptive
                                            ];
                                            if state.control_command_status_list.iter().any(|status| {
                                                status.identifier == ControlCommandIdentifiers::AllowOffOption &&
                                                matches!(status.value.as_slice(), [0x01])
                                            }) {
                                                modes.insert(0, AirPodsNoiseControlMode::Off);
                                            }
                                            modes
                                        }
                                    ),
                                    conversation_awareness_enabled: state.control_command_status_list.iter().any(|status| {
                                        status.identifier == ControlCommandIdentifiers::ConversationDetectConfig &&
                                        matches!(status.value.as_slice(), [0x01])
                                    }),
                                    personalized_volume_enabled: state.control_command_status_list.iter().any(|status| {
                                        status.identifier == ControlCommandIdentifiers::AdaptiveVolumeConfig &&
                                        matches!(status.value.as_slice(), [0x01])
                                    }),
                                    allow_off_mode: state.control_command_status_list.iter().any(|status| {
                                        status.identifier == ControlCommandIdentifiers::AllowOffOption &&
                                        matches!(status.value.as_slice(), [0x01])
                                    }),
                                    hires_mic_enabled: self.hires_mic_enabled,
                                }));
                            }
                            Some(DeviceType::Nothing) => {
                                self.device_states.insert(
                                    mac.clone(),
                                    DeviceState::Nothing(NothingState {
                                        anc_mode: NothingAncMode::Off,
                                        anc_mode_state: combo_box::State::new(vec![
                                            NothingAncMode::Off,
                                            NothingAncMode::Transparency,
                                            NothingAncMode::AdaptiveNoiseCancellation,
                                            NothingAncMode::LowNoiseCancellation,
                                            NothingAncMode::MidNoiseCancellation,
                                            NothingAncMode::HighNoiseCancellation,
                                        ]),
                                    }),
                                );
                            }
                            _ => {}
                        }

                        Task::batch(vec![wait_task])
                    }
                    BluetoothUIMessage::DeviceDisconnected(mac) => {
                        let ui_rx = Arc::clone(&self.ui_rx);
                        let wait_task = Task::perform(wait_for_message(ui_rx), |msg| msg);
                        debug!("Device disconnected: {}", mac);

                        self.bluetooth_state
                            .connected_devices
                            .retain(|device| device != &mac);

                        self.device_states.remove(&mac);

                        let is_airpods = crate::auto_switch::known_airpods()
                            .iter()
                            .any(|a| a.to_string() == mac);
                        if !is_airpods
                            && matches!(&self.selected_tab, Tab::Device(selected_mac) if selected_mac == &mac)
                        {
                            self.selected_tab = Tab::Device("none".to_string());
                        }

                        Task::batch(vec![wait_task])
                    }
                    BluetoothUIMessage::AACPUIEvent(mac, event) => {
                        let ui_rx = Arc::clone(&self.ui_rx);
                        let wait_task = Task::perform(wait_for_message(ui_rx), |msg| msg);
                        debug!("AACP UI Event for {}: {:?}", mac, event);
                        // The AACP handlers save the device information and name
                        // to devices.json without a UI event of their own; the
                        // events that follow are the next chance to pick it up.
                        self.devices = load_devices();
                        match event {
                            AACPEvent::ControlCommand(status) => match status.identifier {
                                ControlCommandIdentifiers::ListeningMode => {
                                    let mode = status
                                        .value
                                        .first()
                                        .map(AirPodsNoiseControlMode::from_byte)
                                        .unwrap_or(AirPodsNoiseControlMode::Transparency);
                                    if let Some(DeviceState::AirPods(state)) =
                                        self.device_states.get_mut(&mac)
                                    {
                                        state.noise_control_mode = mode;
                                    }
                                }
                                ControlCommandIdentifiers::ConversationDetectConfig => {
                                    let is_enabled = match status.value.as_slice() {
                                        [0x01] => true,
                                        [0x02] => false,
                                        _ => {
                                            error!(
                                                "Unknown Conversation Detect Config value: {:?}",
                                                status.value
                                            );
                                            false
                                        }
                                    };
                                    if let Some(DeviceState::AirPods(state)) =
                                        self.device_states.get_mut(&mac)
                                    {
                                        state.conversation_awareness_enabled = is_enabled;
                                    }
                                }
                                ControlCommandIdentifiers::AdaptiveVolumeConfig => {
                                    let is_enabled = match status.value.as_slice() {
                                        [0x01] => true,
                                        [0x02] => false,
                                        _ => {
                                            error!(
                                                "Unknown Adaptive Volume Config value: {:?}",
                                                status.value
                                            );
                                            false
                                        }
                                    };
                                    if let Some(DeviceState::AirPods(state)) =
                                        self.device_states.get_mut(&mac)
                                    {
                                        state.personalized_volume_enabled = is_enabled;
                                    }
                                }
                                ControlCommandIdentifiers::AllowOffOption => {
                                    let is_enabled = match status.value.as_slice() {
                                        [0x01] => true,
                                        [0x02] => false,
                                        _ => {
                                            error!(
                                                "Unknown Allow Off Option value: {:?}",
                                                status.value
                                            );
                                            false
                                        }
                                    };
                                    if let Some(DeviceState::AirPods(state)) =
                                        self.device_states.get_mut(&mac)
                                    {
                                        state.allow_off_mode = is_enabled;
                                        state.noise_control_state = combo_box::State::new({
                                            let mut modes = vec![
                                                AirPodsNoiseControlMode::Transparency,
                                                AirPodsNoiseControlMode::NoiseCancellation,
                                                AirPodsNoiseControlMode::Adaptive,
                                            ];
                                            if is_enabled {
                                                modes.insert(0, AirPodsNoiseControlMode::Off);
                                            }
                                            modes
                                        });
                                    }
                                }
                                _ => {
                                    debug!("Unhandled Control Command Status: {:?}", status);
                                }
                            },
                            AACPEvent::BatteryInfo(battery_info) => {
                                self.remember_case_level(&mac, &battery_info);
                                if let Some(DeviceState::AirPods(state)) =
                                    self.device_states.get_mut(&mac)
                                {
                                    state.battery = battery_info;
                                    debug!("Updated battery info for {}: {:?}", mac, state.battery);
                                }
                            }
                            _ => {}
                        }
                        Task::batch(vec![wait_task])
                    }
                    BluetoothUIMessage::ATTNotification(mac, handle, value) => {
                        debug!(
                            "ATT Notification for {}: handle=0x{:04X}, value={:?}",
                            mac, handle, value
                        );

                        // TODO: Handle Nothing's ANC Mode changes here

                        let ui_rx = Arc::clone(&self.ui_rx);
                        let wait_task = Task::perform(wait_for_message(ui_rx), |msg| msg);
                        Task::batch(vec![wait_task])
                    }
                }
            }
            // Message::ShowNewDialogTab => {
            //     debug!("switching to Add Device tab");
            //     self.selected_tab = Tab::AddDevice;
            //     Task::perform(load_paired_devices(), Message::GotPairedDevices)
            // }
            Message::GotPairedDevices(map) => {
                self.paired_devices = map;
                Task::none()
            }
            Message::StartAddDevice(name, addr) => {
                self.pending_add_device = Some((name, addr));
                self.selected_device_type = None;
                Task::none()
            }
            Message::SelectDeviceType(device_type) => {
                self.selected_device_type = Some(device_type);
                Task::none()
            }
            Message::ConfirmAddDevice => {
                if let Some((name, addr)) = self.pending_add_device.take()
                    && let Some(type_) = self.selected_device_type.take()
                {
                    let key = addr.to_string();
                    let result = update_devices_file(|devices| {
                        devices.insert(
                            key,
                            DeviceData {
                                name,
                                type_: type_.clone(),
                                information: None,
                            },
                        );
                    });
                    if let Err(e) = result {
                        error!("Failed to save device: {}", e);
                    }
                    self.devices = load_devices();
                    self.selected_tab = Tab::Device(addr.to_string());
                }
                Task::none()
            }
            Message::CancelAddDevice => {
                self.pending_add_device = None;
                self.selected_device_type = None;
                Task::none()
            }
            Message::StateChanged(mac, state) => {
                if let DeviceState::AirPods(a) = &state
                    && a.hires_mic_enabled != self.hires_mic_enabled
                {
                    self.hires_mic_enabled = a.hires_mic_enabled;
                    self.save_settings();
                }
                self.device_states.insert(mac.clone(), state);
                // if airpods, update the noise control state combo box based on allow off mode
                let type_ = self.devices.get(&mac).map(|d| d.type_.clone());
                if let Some(DeviceType::AirPods) = type_
                    && let Some(DeviceState::AirPods(state)) = self.device_states.get_mut(&mac)
                {
                    state.noise_control_state = combo_box::State::new({
                        let mut modes = vec![
                            AirPodsNoiseControlMode::Transparency,
                            AirPodsNoiseControlMode::NoiseCancellation,
                            AirPodsNoiseControlMode::Adaptive,
                        ];
                        if state.allow_off_mode {
                            modes.insert(0, AirPodsNoiseControlMode::Off);
                        }
                        modes
                    });
                }
                Task::none()
            }
            Message::TrayTextModeChanged(is_enabled) => {
                self.tray_text_mode = is_enabled;
                self.save_settings();
                Task::none()
            }
            Message::StemControlChanged(is_enabled) => {
                self.stem_control = is_enabled;
                self.save_settings();
                Task::none()
            }
            Message::A2dpResetChanged(is_enabled) => {
                self.a2dp_reset = is_enabled;
                self.save_settings();
                Task::none()
            }
            Message::AutoSwitchChanged(is_enabled) => {
                self.auto_switch_on_playback = is_enabled;
                self.save_settings();
                Task::none()
            }
            Message::PreferredCodecChanged(codec) => {
                self.preferred_codec = codec;
                self.save_settings();
                Task::none()
            }
            Message::HiResMicAgcChanged(is_enabled) => {
                self.hires_mic_agc = is_enabled;
                self.save_settings();
                Task::none()
            }
            Message::HiResMicPauseConvoChanged(is_enabled) => {
                self.hires_mic_pause_convo = is_enabled;
                self.save_settings();
                Task::none()
            }
        }
    }

    fn remember_case_level(&mut self, mac: &str, battery: &[BatteryInfo]) {
        if let Some(level) = live_case_level(battery) {
            self.last_case_level.insert(mac.to_string(), level);
        }
    }

    fn stop_recording(&mut self) {
        if let MicTest::Recording(recorder) = std::mem::replace(&mut self.mic_test, MicTest::Idle) {
            self.mic_test = match recorder.stop() {
                Ok(pcm) => MicTest::Ready(mic_test::Player::new(pcm)),
                Err(e) => MicTest::Failed(e),
            };
        }
    }

    fn start_connect(&mut self, mac: String) -> Task<Message> {
        if matches!(self.connect_status.get(&mac), Some(ConnectStatus::Connecting)) {
            return Task::none();
        }
        let Ok(addr) = mac.parse::<Address>() else {
            error!("Cannot connect, invalid address {}", mac);
            return Task::none();
        };
        self.connect_status.insert(mac.clone(), ConnectStatus::Connecting);
        Task::perform(crate::auto_switch::connect_airpods(addr), move |result| {
            Message::ConnectFinished(mac, result)
        })
    }

    fn disconnected_view(&self, mac: &str) -> iced::widget::Container<'_, Message> {
        let (status, connecting) = match self.connect_status.get(mac) {
            Some(ConnectStatus::Connecting) => ("Connecting…".to_string(), true),
            Some(ConnectStatus::Failed(e)) => (e.clone(), false),
            None => (
                "Not connected to this PC. If they are on your phone, connecting here takes them over."
                    .to_string(),
                false,
            ),
        };
        let label = if connecting { "Connecting…" } else { "Connect to this PC" };
        let mut connect = button(text(label).size(16)).padding(Padding {
            top: 10.0,
            bottom: 10.0,
            left: 20.0,
            right: 20.0,
        });
        if !connecting {
            connect = connect.on_press(Message::ConnectDevice(mac.to_string()));
        }
        container(
            column![text("AirPods not connected").size(20), text(status).size(14), connect]
                .spacing(16)
                .align_x(Center)
                .max_width(440),
        )
        .center_x(Length::Fill)
        .center_y(Length::Fill)
    }

    fn view(&self, _id: window::Id) -> Element<'_, Message> {
        let devices_list = &self.devices;
        let pane_grid = pane_grid::PaneGrid::new(&self.panes, |_pane_id, pane, _is_maximized| {
            match pane {
                Pane::Sidebar => {
                    let create_tab_button = |tab: Tab, label: &str, mac_addr: &str, connected: bool| -> Element<'_, Message> {
                        let label = label.to_string() + if connected { " 􀉣" } else { "" };
                        let is_selected = self.selected_tab == tab;
                        let status: Element<'_, Message> = if connected {
                            match self.device_states.get(mac_addr) {
                                Some(DeviceState::AirPods(state)) => {
                                    let parts = battery_parts(
                                        &state.battery,
                                        self.last_case_level.get(mac_addr).copied(),
                                    );
                                    let mut line = row![].spacing(4);
                                    for (part, stale) in parts {
                                        let mut part_text = text(part).size(12);
                                        if stale {
                                            part_text = part_text.style(move |theme: &Theme| {
                                                let mut style = text::Style::default();
                                                let color = if is_selected {
                                                    Style::default().text_color
                                                } else {
                                                    theme.palette().text
                                                };
                                                style.color = Some(color.scale_alpha(0.5));
                                                style
                                            });
                                        }
                                        line = line.push(part_text);
                                    }
                                    line.into()
                                }
                                _ => text("Connected").size(12).into(),
                            }
                        } else {
                            text(match self.connect_status.get(mac_addr) {
                                Some(ConnectStatus::Connecting) => "Connecting…",
                                Some(ConnectStatus::Failed(_)) => "Couldn't connect",
                                None => "Not connected",
                            })
                            .size(12)
                            .into()
                        };
                        let col = column![text(label).size(16), status];
                        let content = container(col)
                            .padding(8);
                        let style = move |theme: &Theme, _status| {
                            if is_selected {
                                let mut style = Style::default()
                                    .with_background(theme.palette().primary);
                                let mut border = Border::default();
                                border.color = theme.palette().text;
                                style.border = border.rounded(12);
                                style
                            } else {
                                let mut style = Style::default()
                                    .with_background(theme.palette().primary.scale_alpha(0.1));
                                let mut border = Border::default();
                                border.color = theme.palette().primary.scale_alpha(0.1);
                                style.border = border.rounded(8);
                                style.text_color = theme.palette().text;
                                style
                            }
                        };
                        button(content)
                            .style(style)
                            .padding(5)
                            .on_press(Message::SelectTab(tab))
                            .width(Length::Fill)
                            .into()
                    };

                    let create_settings_button = || -> Element<'_, Message> {
                        let label = "Settings".to_string();
                        let is_selected = self.selected_tab == Tab::Settings;
                        let col = column![text(label).size(16)];
                        let content = container(col)
                            .padding(8);
                        let style = move |theme: &Theme, _status| {
                            if is_selected {
                                let mut style = Style::default()
                                    .with_background(theme.palette().primary);
                                let mut border = Border::default();
                                border.color = theme.palette().text;
                                style.border = border.rounded(12);
                                style
                            } else {
                                let mut style = Style::default()
                                    .with_background(theme.palette().primary.scale_alpha(0.1));
                                let mut border = Border::default();
                                border.color = theme.palette().primary.scale_alpha(0.1);
                                style.border = border.rounded(8);
                                style.text_color = theme.palette().text;
                                style
                            }
                        };
                        button(content)
                            .style(style)
                            .padding(5)
                            .on_press(Message::SelectTab(Tab::Settings))
                            .width(Length::Fill)
                            .into()
                    };

                    let mut devices = column!().spacing(4);
                    let mut devices_vec: Vec<(&String, &DeviceData)> = devices_list.iter().collect();
                    devices_vec.sort_by(|a, b| a.1.name.cmp(&b.1.name));
                    for (mac, device) in devices_vec {
                        let tab_button = create_tab_button(
                            Tab::Device(mac.clone()),
                            &device.name,
                            mac,
                            self.bluetooth_state.connected_devices.contains(mac)
                        );
                        devices = devices.push(tab_button);
                    }

                    let settings = create_settings_button();

                    let content = column![
                        row![
                            text("Devices").size(18),
                            // Removing until I actually add support for devices other than AirPods
                            // Space::new().width(Length::Fill),
                            // button(
                            //     container(text("+").size(18)).center_x(Length::Fill).center_y(Length::Fill)
                            // )
                            //     .style(
                            //         |theme: &Theme, _status| {
                            //             let mut style = Style::default();
                            //             style.text_color = theme.palette().text;
                            //             style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                            //             style.border = Border {
                            //                 width: 1.0,
                            //                 color: theme.palette().primary.scale_alpha(0.1),
                            //                 radius: Radius::from(8.0),
                            //             };
                            //             style
                            //         }
                            //     )
                            //     .padding(0)
                            //     .width(Length::from(28))
                            //     .height(Length::from(28))
                            //     .on_press(Message::ShowNewDialogTab)
                        ]
                        .align_y(Center)
                        .padding(4),
                        Space::new().height(Length::from(8)),
                        devices,
                        Space::new().height(Length::Fill),
                        settings
                    ]
                        .padding(12);
                    pane_grid::Content::new(
                        row![
                            content,
                            rule::vertical(1).style(
                                |theme: &Theme| {
                                    rule::Style{
                                        color: theme.palette().primary.scale_alpha(0.2),
                                        radius: Radius::from(8.0),
                                        fill_mode: FillMode::Full,
                                        snap: false
                                    }
                                }
                            )
                        ]
                    )
                }

                Pane::Content => {
                    let device_managers = self.device_managers.blocking_read();
                    let content = match &self.selected_tab {
                        Tab::Device(id) => {
                            if id == "none" {
                                container(
                                    text("Select a device".to_string()).size(16)
                                )
                                    .center_x(Length::Fill)
                                    .center_y(Length::Fill)
                            } else {
                                let device_type = devices_list.get(id).map(|d| d.type_.clone());
                                let device_state = self.device_states.get(id);
                                debug!("Rendering device view for {}: type={:?}, state={:?}", id, device_type, device_state);
                                match device_type {
                                    Some(DeviceType::AirPods) => {

                                        device_state.as_ref().and_then(|state| {
                                            match state {
                                                DeviceState::AirPods(state) => {
                                                    device_managers.get(id).and_then(|managers| {
                                                        managers.get_aacp().map(|aacp_manager| airpods_view(
                                                                    id,
                                                                    devices_list,
                                                                    state,
                                                                    aacp_manager.clone(),
                                                                    self.hires_mic_pause_convo,
                                                                    &self.mic_test
                                                                ))
                                                    })
                                                }
                                                _ => None,
                                            }
                                        }).unwrap_or_else(|| {
                                            if self.bluetooth_state.connected_devices.contains(id) {
                                                container(
                                                    text("Waiting for the AirPods to report their state…").size(16)
                                                )
                                                    .center_x(Length::Fill)
                                                    .center_y(Length::Fill)
                                            } else {
                                                self.disconnected_view(id)
                                            }
                                        })
                                    }
                                    Some(DeviceType::Nothing) => {
                                        if let Some(DeviceState::Nothing(state)) = device_state {
                                            if let Some(device_managers) = device_managers.get(id) {
                                                if let Some(att_manager) = device_managers.get_att() {
                                                    nothing_view(id, devices_list, state, att_manager.clone())
                                                } else {
                                                    error!("No ATT manager found for Nothing device {}", id);
                                                    container(
                                                        text("No valid ATT manager found for this Nothing device").size(16)
                                                    )
                                                        .center_x(Length::Fill)
                                                        .center_y(Length::Fill)
                                                }
                                            } else {
                                                error!("No manager found for Nothing device {}", id);
                                                container(
                                                    text("No manager found for this Nothing device").size(16)
                                                )
                                                    .center_x(Length::Fill)
                                                    .center_y(Length::Fill)
                                            }
                                        } else {
                                            container(
                                                text("No state available for this Nothing device").size(16)
                                            )
                                                .center_x(Length::Fill)
                                                .center_y(Length::Fill)
                                        }
                                    }
                                    _ => {
                                        container(text("Unsupported device").size(16))
                                            .center_x(Length::Fill)
                                            .center_y(Length::Fill)
                                    }
                                }
                            }
                        }
                        Tab::Settings => {
                            let tray_text_mode_toggle = container(
                                row![
                                    column![
                                        text("Use text in tray").size(16),
                                        text("Use text for battery status in tray instead of a progress bar.").size(12).style(
                                            |theme: &Theme| {
                                                let mut style = text::Style::default();
                                                style.color = Some(theme.palette().text.scale_alpha(0.7));
                                                style
                                            }
                                        ).width(Length::Fill)
                                    ].width(Length::Fill),
                                    toggler(self.tray_text_mode)
                                        .on_toggle(move |is_enabled| {
                                            Message::TrayTextModeChanged(is_enabled)
                                        })
                                    .spacing(0)
                                    .size(20)
                                    ]
                                        .align_y(Center)
                                        .spacing(12)
                                    )
                                        .padding(Padding{
                                            top: 5.0,
                                            bottom: 5.0,
                                            left: 18.0,
                                            right: 18.0,
                                        })
                                        .style(
                                            |theme: &Theme| {
                                                let mut style = container::Style::default();
                                                style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                                                let mut border = Border::default();
                                                border.color = theme.palette().primary.scale_alpha(0.5);
                                                style.border = border.rounded(16);
                                                style
                                            }
                                        )
                                    .align_y(Center);

                            let appearance_settings_col = column![
                                container(
                                    text("Appearance").size(20).style(
                                        |theme: &Theme| {
                                            let mut style = text::Style::default();
                                            style.color = Some(theme.palette().primary);
                                            style
                                        }
                                    )
                                )
                                .padding(Padding{
                                    top: 0.0,
                                    bottom: 0.0,
                                    left: 18.0,
                                    right: 18.0,
                                }),
                                container(
                                    row![
                                        text("Theme")
                                            .size(16),
                                        Space::new().width(Length::Fill),
                                        combo_box(
                                            &self.theme_state,
                                            "Select theme",
                                            Some(&self.selected_theme),
                                            Message::ThemeSelected
                                        )
                                        .input_style(
                                            |theme: &Theme, _status| {
                                                text_input::Style {
                                                    background: Background::Color(theme.palette().primary.scale_alpha(0.2)),
                                                    border: Border {
                                                        width: 1.0,
                                                        color: theme.palette().text.scale_alpha(0.3),
                                                        radius: Radius::from(4.0)
                                                    },
                                                    icon: Default::default(),
                                                    placeholder: theme.palette().text,
                                                    value: theme.palette().text,
                                                    selection: Default::default(),
                                                }
                                            }
                                        )
                                        .menu_style(
                                            |theme: &Theme| {
                                                menu::Style {
                                                    background: Background::Color(theme.palette().background),
                                                    border: Border {
                                                        width: 1.0,
                                                        color: theme.palette().text,
                                                        radius: Radius::from(4.0)
                                                    },
                                                    text_color: theme.palette().text,
                                                    selected_text_color: theme.palette().text,
                                                    selected_background: Background::Color(theme.palette().primary.scale_alpha(0.3)),
                                                    shadow: Default::default()
                                                }
                                            }
                                        )
                                        .padding(Padding{
                                            top: 5.0,
                                            bottom: 5.0,
                                            left: 10.0,
                                            right: 10.0,
                                        })
                                        .width(Length::from(200))
                                    ]
                                    .align_y(Center)
                                )
                                    .padding(Padding{
                                        top: 5.0,
                                        bottom: 5.0,
                                        left: 18.0,
                                        right: 18.0,
                                    })
                                    .style(
                                        |theme: &Theme| {
                                            let mut style = container::Style::default();
                                            style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                                            let mut border = Border::default();
                                            border.color = theme.palette().primary.scale_alpha(0.5);
                                            style.border = border.rounded(16);
                                            style
                                        }
                                    )
                                ]
                                .spacing(12);

                            let stem_control_value = self.stem_control;
                            let stem_control_toggle = container(
                                row![
                                    column![
                                        text("Stem press track control").size(16),
                                        text("Double press = next track, triple press = previous track. Disable if your environment handles AirPods AVRCP commands natively.").size(12).style(
                                            |theme: &Theme| {
                                                let mut style = text::Style::default();
                                                style.color = Some(theme.palette().text.scale_alpha(0.7));
                                                style
                                            }
                                        ).width(Length::Fill)
                                    ].width(Length::Fill),
                                    toggler(stem_control_value)
                                        .on_toggle(move |is_enabled| {
                                            Message::StemControlChanged(is_enabled)
                                        })
                                    .spacing(0)
                                    .size(20)
                                    ]
                                        .align_y(Center)
                                        .spacing(12)
                                    )
                                        .padding(Padding{
                                            top: 5.0,
                                            bottom: 5.0,
                                            left: 18.0,
                                            right: 18.0,
                                        })
                                        .style(
                                            |theme: &Theme| {
                                                let mut style = container::Style::default();
                                                style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                                                let mut border = Border::default();
                                                border.color = theme.palette().primary.scale_alpha(0.5);
                                                style.border = border.rounded(16);
                                                style
                                            }
                                        )
                                    .align_y(Center);

                            let a2dp_reset_value = self.a2dp_reset;
                            let a2dp_reset_toggle = container(
                                row![
                                    column![
                                        text("Reset A2DP transport").size(16),
                                        text("Briefly suspends and resumes A2DP after the hi-res mic starts or stops. Disabling removes the short pause/stutter when the mic turns on or off, but on some setups it causes playback on one AirPod to drop once capture ends.").size(12).style(
                                            |theme: &Theme| {
                                                let mut style = text::Style::default();
                                                style.color = Some(theme.palette().text.scale_alpha(0.7));
                                                style
                                            }
                                        ).width(Length::Fill)
                                    ].width(Length::Fill),
                                    toggler(a2dp_reset_value)
                                        .on_toggle(move |is_enabled| {
                                            Message::A2dpResetChanged(is_enabled)
                                        })
                                    .spacing(0)
                                    .size(20)
                                    ]
                                        .align_y(Center)
                                        .spacing(12)
                                    )
                                        .padding(Padding{
                                            top: 5.0,
                                            bottom: 5.0,
                                            left: 18.0,
                                            right: 18.0,
                                        })
                                        .style(
                                            |theme: &Theme| {
                                                let mut style = container::Style::default();
                                                style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                                                let mut border = Border::default();
                                                border.color = theme.palette().primary.scale_alpha(0.5);
                                                style.border = border.rounded(16);
                                                style
                                            }
                                        )
                                    .align_y(Center);

                            let preferred_codec_picker = container(
                                row![
                                    column![
                                        text("Preferred audio codec").size(16),
                                        text("Codec to activate for playback. The others are used as fallbacks if the chosen one is unavailable.").size(12).style(
                                            |theme: &Theme| {
                                                let mut style = text::Style::default();
                                                style.color = Some(theme.palette().text.scale_alpha(0.7));
                                                style
                                            }
                                        ).width(Length::Fill)
                                    ].width(Length::Fill),
                                    pick_list(
                                        PreferredCodec::ALL,
                                        Some(self.preferred_codec),
                                        Message::PreferredCodecChanged,
                                    )
                                    ]
                                        .align_y(Center)
                                        .spacing(12)
                                    )
                                        .padding(Padding{
                                            top: 5.0,
                                            bottom: 5.0,
                                            left: 18.0,
                                            right: 18.0,
                                        })
                                        .style(
                                            |theme: &Theme| {
                                                let mut style = container::Style::default();
                                                style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                                                let mut border = Border::default();
                                                border.color = theme.palette().primary.scale_alpha(0.5);
                                                style.border = border.rounded(16);
                                                style
                                            }
                                        )
                                    .align_y(Center);

                            let auto_switch_value = self.auto_switch_on_playback;
                            let auto_switch_toggle = container(
                                row![
                                    column![
                                        text("Switch to this PC on playback").size(16),
                                        text("When media starts playing here and the AirPods are on another device, connect them to this PC. They leave the other device, even mid-call.").size(12).style(
                                            |theme: &Theme| {
                                                let mut style = text::Style::default();
                                                style.color = Some(theme.palette().text.scale_alpha(0.7));
                                                style
                                            }
                                        ).width(Length::Fill)
                                    ].width(Length::Fill),
                                    toggler(auto_switch_value)
                                        .on_toggle(move |is_enabled| {
                                            Message::AutoSwitchChanged(is_enabled)
                                        })
                                    .spacing(0)
                                    .size(20)
                                    ]
                                        .align_y(Center)
                                        .spacing(12)
                                    )
                                        .padding(Padding{
                                            top: 5.0,
                                            bottom: 5.0,
                                            left: 18.0,
                                            right: 18.0,
                                        })
                                        .style(
                                            |theme: &Theme| {
                                                let mut style = container::Style::default();
                                                style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                                                let mut border = Border::default();
                                                border.color = theme.palette().primary.scale_alpha(0.5);
                                                style.border = border.rounded(16);
                                                style
                                            }
                                        )
                                    .align_y(Center);

                            let hires_mic_agc_value = self.hires_mic_agc;
                            let hires_mic_agc_toggle = container(
                                row![
                                    column![
                                        text("Hi-res mic auto gain").size(16),
                                        text("Automatically normalizes the hi-res microphone level. For most usecases this should remain on. Disable for a raw, unprocessed capture.").size(12).style(
                                            |theme: &Theme| {
                                                let mut style = text::Style::default();
                                                style.color = Some(theme.palette().text.scale_alpha(0.7));
                                                style
                                            }
                                        ).width(Length::Fill)
                                    ].width(Length::Fill),
                                    toggler(hires_mic_agc_value)
                                        .on_toggle(move |is_enabled| {
                                            Message::HiResMicAgcChanged(is_enabled)
                                        })
                                    .spacing(0)
                                    .size(20)
                                    ]
                                        .align_y(Center)
                                        .spacing(12)
                                    )
                                        .padding(Padding{
                                            top: 5.0,
                                            bottom: 5.0,
                                            left: 18.0,
                                            right: 18.0,
                                        })
                                        .style(
                                            |theme: &Theme| {
                                                let mut style = container::Style::default();
                                                style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                                                let mut border = Border::default();
                                                border.color = theme.palette().primary.scale_alpha(0.5);
                                                style.border = border.rounded(16);
                                                style
                                            }
                                        )
                                    .align_y(Center);

                            let hires_mic_pause_convo_value = self.hires_mic_pause_convo;
                            let hires_mic_pause_convo_toggle = container(
                                row![
                                    column![
                                        text("Pause conversation awareness during capture").size(16),
                                        text("Turns off conversation awareness while the hi-res microphone is capturing, then restores it afterwards.").size(12).style(
                                            |theme: &Theme| {
                                                let mut style = text::Style::default();
                                                style.color = Some(theme.palette().text.scale_alpha(0.7));
                                                style
                                            }
                                        ).width(Length::Fill)
                                    ].width(Length::Fill),
                                    toggler(hires_mic_pause_convo_value)
                                        .on_toggle(move |is_enabled| {
                                            Message::HiResMicPauseConvoChanged(is_enabled)
                                        })
                                    .spacing(0)
                                    .size(20)
                                    ]
                                        .align_y(Center)
                                        .spacing(12)
                                    )
                                        .padding(Padding{
                                            top: 5.0,
                                            bottom: 5.0,
                                            left: 18.0,
                                            right: 18.0,
                                        })
                                        .style(
                                            |theme: &Theme| {
                                                let mut style = container::Style::default();
                                                style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                                                let mut border = Border::default();
                                                border.color = theme.palette().primary.scale_alpha(0.5);
                                                style.border = border.rounded(16);
                                                style
                                            }
                                        )
                                    .align_y(Center);

                            let controls_settings_col = column![
                                container(
                                    text("Controls").size(20).style(
                                        |theme: &Theme| {
                                            let mut style = text::Style::default();
                                            style.color = Some(theme.palette().primary);
                                            style
                                        }
                                    )
                                )
                                .padding(Padding{
                                    top: 0.0,
                                    bottom: 0.0,
                                    left: 18.0,
                                    right: 18.0,
                                }),
                                stem_control_toggle
                            ]
                            .spacing(12);

                            container(
                                column![
                                    appearance_settings_col,
                                    Space::new().height(Length::from(20)),
                                    tray_text_mode_toggle,
                                    Space::new().height(Length::from(20)),
                                    controls_settings_col,
                                    Space::new().height(Length::from(20)),
                                    preferred_codec_picker,
                                    Space::new().height(Length::from(20)),
                                    auto_switch_toggle,
                                    Space::new().height(Length::from(20)),
                                    a2dp_reset_toggle,
                                    Space::new().height(Length::from(20)),
                                    hires_mic_agc_toggle,
                                    Space::new().height(Length::from(20)),
                                    hires_mic_pause_convo_toggle,
                                ]
                            )
                                .padding(20)
                                .width(Length::Fill)
                                .height(Length::Fill)
                        },
                        Tab::AddDevice => {
                            container(
                                column![
                                    text("Pick a paired device to add:").size(18),
                                    Space::new().height(Length::from(10)),
                                    {
                                        let mut list_col = column![].spacing(12);
                                        for device in self.paired_devices.clone() {
                                            if !devices_list.contains_key(&device.1.to_string()) {
                                                let mut item_col = column![].spacing(8);
                                                let mut row_elements = vec![
                                                    column![
                                                        text(device.0.to_string()).size(16),
                                                        text(device.1.to_string()).size(12)
                                                    ].into(),
                                                    Space::new().height(Length::Fill).into(),
                                                ];
                                                if !matches!(&self.pending_add_device, Some((_, addr)) if addr == &device.1) {
                                                    row_elements.push(
                                                        button(
                                                            text("Add").size(14).width(120).align_y(Center).align_x(Center)
                                                        )
                                                            .style(
                                                                |theme: &Theme, _status| {
                                                                    let mut style = Style::default();
                                                                    style.text_color = theme.palette().text;
                                                                    style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.5)));
                                                                    style.border = Border {
                                                                        width: 1.0,
                                                                        color: theme.palette().primary,
                                                                        radius: Radius::from(8.0),
                                                                    };
                                                                    style
                                                                }
                                                            )
                                                            .padding(8)
                                                            .on_press(Message::StartAddDevice(device.0.clone(), device.1))
                                                            .into()
                                                    );
                                                }
                                                item_col = item_col.push(row(row_elements).align_y(Center));

                                                if let Some((_, pending_addr)) = &self.pending_add_device
                                                    && pending_addr == &device.1 {
                                                        item_col = item_col.push(
                                                            row![
                                                                text("Device Type:").size(16),
                                                                Space::new().width(Length::Fill),
                                                                combo_box(
                                                                    &self.device_type_state,
                                                                    "Select device type",
                                                                    self.selected_device_type.as_ref(),
                                                                    Message::SelectDeviceType
                                                                )
                                                                    .input_style(
                                                                        |theme: &Theme, _status| {
                                                                            text_input::Style {
                                                                                background: Background::Color(theme.palette().background),
                                                                                border: Border {
                                                                                    width: 1.0,
                                                                                    color: theme.palette().text,
                                                                                    radius: Radius::from(8.0),
                                                                                },
                                                                                icon: Default::default(),
                                                                                placeholder: theme.palette().text.scale_alpha(0.5),
                                                                                value: theme.palette().text,
                                                                                selection: theme.palette().primary
                                                                            }
                                                                        }
                                                                    )
                                                                    .menu_style(
                                                                        |theme: &Theme| {
                                                                            menu::Style {
                                                                                background: Background::Color(theme.palette().background),
                                                                                border: Border {
                                                                                    width: 1.0,
                                                                                    color: theme.palette().text,
                                                                                    radius: Radius::from(8.0)
                                                                                },
                                                                                text_color: theme.palette().text,
                                                                                selected_text_color: theme.palette().text,
                                                                                selected_background: Background::Color(theme.palette().primary.scale_alpha(0.3)),
                                                                                shadow: Default::default()
                                                                            }
                                                                        }
                                                                    )
                                                                    .width(Length::from(200))
                                                            ]
                                                        );
                                                        item_col = item_col.push(
                                                            row![
                                                                Space::new().width(Length::Fill),
                                                                button(text("Cancel").size(16).width(Length::Fill).center())
                                                                    .on_press(Message::CancelAddDevice)
                                                                    .style(|theme: &Theme, _status| {
                                                                        let mut style = Style::default();
                                                                        style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                                                                        style.text_color = theme.palette().text;
                                                                        style.border = Border::default().rounded(8.0);
                                                                        style
                                                                    })
                                                                    .width(Length::from(120))
                                                                    .padding(4),
                                                                Space::new().width(Length::from(20)),
                                                                button(text("Add Device").size(16).width(Length::Fill).center())
                                                                    .on_press(Message::ConfirmAddDevice)
                                                                    .style(|theme: &Theme, _status| {
                                                                        let mut style = Style::default();
                                                                        style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.3)));
                                                                        style.text_color = theme.palette().text;
                                                                        style.border = Border::default().rounded(8.0);
                                                                        style
                                                                    })
                                                                    .width(Length::from(120))
                                                                    .padding(4),
                                                            ]
                                                            .align_y(Center)
                                                            .width(Length::Fill)
                                                        );
                                                    }
                                                list_col = list_col.push(
                                                    container(item_col)
                                                        .padding(8)
                                                        .style(
                                                            |theme: &Theme| {
                                                                let mut style = container::Style::default();
                                                                style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
                                                                let mut border = Border::default();
                                                                border.color = theme.palette().text;
                                                                style.border = border.rounded(8);
                                                                style
                                                            }
                                                        )
                                                );
                                            }
                                        }
                                        if self.paired_devices.iter().all(|device| devices_list.contains_key(&device.1.to_string())) && self.pending_add_device.is_none() {
                                            list_col = list_col.push(
                                                container(
                                                    text("No new paired devices found. All paired devices are already added.").size(16)
                                                )
                                                .width(Length::Fill)
                                            );
                                        }
                                        scrollable(list_col)
                                            .height(Length::Fill)
                                            .width(Length::Fill)
                                    }
                                ]
                            )
                            .padding(20)
                            .height(Length::Fill)
                            .width(Length::Fill)
                        }
                    };

                    pane_grid::Content::new(content)
                }
            }
        })
            .width(Length::Fill)
            .height(Length::Fill)
            .on_resize(20, Message::Resized);

        container(pane_grid).into()
    }

    fn theme(&self, _id: window::Id) -> Theme {
        self.selected_theme.into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let close = window::close_events().map(Message::WindowClosed);

        match self.tick_interval() {
            Some(interval) => {
                let tick = iced::time::every(interval).map(|_| Message::MicLevelTick);
                Subscription::batch([close, tick])
            }
            None => close,
        }
    }

    /// How often to redraw for the level meter and the microphone test, or None
    /// when nothing on screen moves by itself.
    fn tick_interval(&self) -> Option<Duration> {
        // Polled even with the window closed, to notice the recorder hitting
        // MAX_RECORDING.
        if matches!(self.mic_test, MicTest::Recording(_)) {
            return Some(MIC_TICK);
        }
        self.window?;
        if matches!(self.mic_test, MicTest::Ready(_)) || self.airpods_mic_active() {
            return Some(MIC_TICK);
        }
        // Nothing tells the UI when an app opens the hi-res mic, so watch for it
        // slowly to bring up the level meter.
        let airpods_connected = self
            .device_states
            .values()
            .any(|s| matches!(s, DeviceState::AirPods(_)));
        (self.hires_mic_enabled && airpods_connected).then_some(MIC_WATCH)
    }

    fn airpods_mic_active(&self) -> bool {
        let managers = self.device_managers.blocking_read();
        self.device_states.iter().any(|(mac, state)| {
            matches!(state, DeviceState::AirPods(_))
                && managers
                    .get(mac)
                    .and_then(|m| m.get_aacp())
                    .is_some_and(|aacp| aacp.mic_active())
        })
    }
}

/// Read devices.json. A missing or unreadable file gives an empty list.
fn load_devices() -> HashMap<String, DeviceData> {
    let devices_json = match std::fs::read_to_string(get_devices_path()) {
        Ok(json) => json,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                error!("Failed to read devices file: {}", e);
            }
            return HashMap::new();
        }
    };
    serde_json::from_str(&devices_json).unwrap_or_else(|e| {
        error!("Deserialization failed: {}", e);
        HashMap::new()
    })
}

const CHARGING_MARK: &str = "\u{1002E6}";

/// The level of a battery entry, or None when the component is disconnected or
/// the level is out of range.
fn known_level(info: &BatteryInfo) -> Option<u8> {
    (info.status != BatteryStatus::Disconnected && info.level <= 100).then_some(info.level)
}

/// "80%" with a charging mark, or "-" when the component is absent or disconnected.
fn battery_text(info: Option<&BatteryInfo>) -> String {
    match info.and_then(|b| known_level(b).map(|level| (level, b.status))) {
        Some((level, status)) => {
            let mark = if status.is_charging() { CHARGING_MARK } else { "" };
            format!("{}%{}", level, mark)
        }
        None => "-".to_string(),
    }
}

/// The case level a battery report carries, if any. AirPods only know the case
/// level while a bud sits in it: with both buds out, the case entry reports
/// Disconnected with a level of 0 or 255.
fn live_case_level(battery: &[BatteryInfo]) -> Option<u8> {
    battery
        .iter()
        .find(|b| b.component == BatteryComponent::Case)
        .and_then(known_level)
}

/// The sidebar battery line as (text, stale) parts. Headphones show a single
/// level. For earbuds, a case that cannot report falls back to `last_case`,
/// marked stale so it is drawn dimmed.
fn battery_parts(battery: &[BatteryInfo], last_case: Option<u8>) -> Vec<(String, bool)> {
    let find = |component| battery.iter().find(|b| b.component == component);
    if let Some(headphone) = find(BatteryComponent::Headphone) {
        return vec![(format!("􀺹 {}", battery_text(Some(headphone))), false)];
    }
    let (case, stale) = match (live_case_level(battery), last_case) {
        (Some(_), _) => (battery_text(find(BatteryComponent::Case)), false),
        (None, Some(level)) => (format!("{}%", level), true),
        (None, None) => ("-".to_string(), false),
    };
    vec![
        (format!("\u{1018E5} {}", battery_text(find(BatteryComponent::Left))), false),
        (format!("\u{1018E8} {}", battery_text(find(BatteryComponent::Right))), false),
        (format!("\u{100E6C} {}", case), stale),
    ]
}

async fn wait_for_message(ui_rx: Arc<Mutex<UnboundedReceiver<BluetoothUIMessage>>>) -> Message {
    let mut rx = ui_rx.lock().await;
    match rx.recv().await {
        Some(msg) => Message::BluetoothMessage(msg),
        None => {
            error!("UI message channel closed");
            Message::BluetoothMessage(BluetoothUIMessage::NoOp)
        }
    }
}

// async fn load_paired_devices() -> HashMap<String, Address> {
//     let mut devices = HashMap::new();
//
//     let session = Session::new().await.ok().unwrap();
//     let adapter = session.default_adapter().await.ok().unwrap();
//     let addresses = adapter.device_addresses().await.ok().unwrap();
//     for addr in addresses {
//         let device = adapter.device(addr).ok().unwrap();
//         let paired = device.is_paired().await.ok().unwrap();
//         if paired {
//             let name = device
//                 .name()
//                 .await
//                 .ok()
//                 .flatten()
//                 .unwrap_or_else(|| "Unknown".to_string());
//             devices.insert(name, addr);
//         }
//     }
//
//     devices
// }

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(component: BatteryComponent, level: u8, status: BatteryStatus) -> BatteryInfo {
        BatteryInfo {
            component,
            level,
            status,
        }
    }

    fn texts(parts: &[(String, bool)]) -> Vec<&str> {
        parts.iter().map(|(t, _)| t.as_str()).collect()
    }

    #[test]
    fn battery_text_marks_charging_and_unknown() {
        let charging = entry(BatteryComponent::Left, 40, BatteryStatus::Charging);
        let optimized = entry(BatteryComponent::Left, 80, BatteryStatus::OptimizedCharging);
        let idle = entry(BatteryComponent::Left, 100, BatteryStatus::NotCharging);
        let gone = entry(BatteryComponent::Left, 0, BatteryStatus::Disconnected);
        let bogus = entry(BatteryComponent::Left, 255, BatteryStatus::NotCharging);
        assert_eq!(battery_text(Some(&charging)), format!("40%{CHARGING_MARK}"));
        assert_eq!(battery_text(Some(&optimized)), format!("80%{CHARGING_MARK}"));
        assert_eq!(battery_text(Some(&idle)), "100%");
        assert_eq!(battery_text(Some(&gone)), "-");
        assert_eq!(battery_text(Some(&bogus)), "-");
        assert_eq!(battery_text(None), "-");
    }

    #[test]
    fn case_level_is_remembered_only_when_reported() {
        let in_case = [entry(BatteryComponent::Case, 60, BatteryStatus::NotCharging)];
        let buds_out = [entry(BatteryComponent::Case, 0, BatteryStatus::Disconnected)];
        let buds_out_255 = [entry(BatteryComponent::Case, 255, BatteryStatus::Disconnected)];
        let bad_level = [entry(BatteryComponent::Case, 101, BatteryStatus::Charging)];
        assert_eq!(live_case_level(&in_case), Some(60));
        assert_eq!(live_case_level(&buds_out), None);
        assert_eq!(live_case_level(&buds_out_255), None);
        assert_eq!(live_case_level(&bad_level), None);
        assert_eq!(live_case_level(&[]), None);
    }

    #[test]
    fn case_falls_back_to_last_known_level() {
        let battery = [
            entry(BatteryComponent::Left, 90, BatteryStatus::NotCharging),
            entry(BatteryComponent::Right, 85, BatteryStatus::NotCharging),
            entry(BatteryComponent::Case, 0, BatteryStatus::Disconnected),
        ];
        let parts = battery_parts(&battery, Some(60));
        assert_eq!(
            texts(&parts),
            ["\u{1018E5} 90%", "\u{1018E8} 85%", "\u{100E6C} 60%"]
        );
        assert!(parts[2].1);

        let parts = battery_parts(&battery, None);
        assert_eq!(parts[2], ("\u{100E6C} -".to_string(), false));
    }

    #[test]
    fn live_case_level_wins_over_last_known() {
        let battery = [
            entry(BatteryComponent::Left, 90, BatteryStatus::Charging),
            entry(BatteryComponent::Case, 50, BatteryStatus::NotCharging),
        ];
        let parts = battery_parts(&battery, Some(70));
        assert_eq!(
            texts(&parts),
            [
                format!("\u{1018E5} 90%{CHARGING_MARK}").as_str(),
                "\u{1018E8} -",
                "\u{100E6C} 50%"
            ]
        );
        assert!(!parts[2].1);
    }

    #[test]
    fn headphones_show_one_level() {
        let battery = [entry(BatteryComponent::Headphone, 30, BatteryStatus::NotCharging)];
        assert_eq!(texts(&battery_parts(&battery, Some(70))), ["􀺹 30%"]);
    }
}
