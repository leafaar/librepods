use {
    crate::{
        audio::hires_mic::{HiResMic, MicStatus},
        bluetooth::{
            eq::{self, CustomEq},
            l2cap::{self, ConnectError},
        },
        devices::{
            airpods::AirPodsInformation,
            enums::{DeviceData, DeviceInformation, DeviceType},
        },
        utils::{AppSettings, get_devices_path, update_devices_file},
    },
    bluer::{Address, l2cap::SeqPacket},
    serde::{Deserialize, Serialize},
    serde_json,
    std::{
        collections::HashMap,
        io,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    },
    tokio::{
        sync::{Mutex, Notify, mpsc},
        task::{AbortHandle, JoinSet},
        time::sleep,
    },
    tracing::{debug, error, info, warn},
};

const PSM: u16 = 0x1001;
const HEADER_BYTES: [u8; 4] = [0x04, 0x00, 0x04, 0x00];
/// Longest wait for the state lock before a send gives up.
const STATE_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// L2CAP recv buffer. 0x58 hi-res audio SDUs can exceed 1 KB; SOCK_SEQPACKET
/// silently truncates an undersized buffer, so this must comfortably exceed the
/// largest SDU
const RECV_BUF_LEN: usize = 4096;

/// 0x58 microphone-stream control packets (include the 04 00 04 00 header).
const AACP_START_AUDIO: [u8; 19] = [
    0x04, 0x00, 0x04, 0x00, 0x58, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x01, 0x82, 0x00, 0x00, 0x00,
    0x04, 0x96, 0x00,
];
const AACP_STOP_AUDIO: [u8; 12] = [
    0x04, 0x00, 0x04, 0x00, 0x58, 0x00, 0x00, 0x00, 0x02, 0x00, 0x03, 0x01,
];

/// Bound for the audio SDU forwarding channel. Realtime: if the decoder falls
/// behind we drop SDUs (a brief glitch) rather than back-pressure the L2CAP
/// recv loop, which would stall control traffic too.
const AUDIO_CHANNEL_CAP: usize = 256;

pub mod opcodes {
    pub const SET_FEATURE_FLAGS: u8 = 0x4D;
    pub const REQUEST_NOTIFICATIONS: u8 = 0x0F;
    pub const BATTERY_INFO: u8 = 0x04;
    pub const CONTROL_COMMAND: u8 = 0x09;
    pub const EAR_DETECTION: u8 = 0x06;
    pub const CONVERSATION_AWARENESS: u8 = 0x4B;
    pub const INFORMATION: u8 = 0x1D;
    pub const RENAME: u8 = 0x1A;
    pub const PROXIMITY_KEYS_REQ: u8 = 0x30;
    pub const PROXIMITY_KEYS_RSP: u8 = 0x31;
    pub const STEM_PRESS: u8 = 0x19;
    pub const EQ_DATA: u8 = 0x53;
    pub const CONNECTED_DEVICES: u8 = 0x2E;
    pub const AUDIO_SOURCE: u8 = 0x0E;
    pub const SMART_ROUTING: u8 = 0x10;
    pub const SMART_ROUTING_RESP: u8 = 0x11;
    pub const CUSTOM_EQ: u8 = 0x63;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ControlCommandStatus {
    pub identifier: ControlCommandIdentifiers,
    pub value: Vec<u8>,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlCommandIdentifiers {
    MicMode = 0x01,
    ButtonSendMode = 0x05,
    VoiceTrigger = 0x12,
    SingleClickMode = 0x14,
    DoubleClickMode = 0x15,
    ClickHoldMode = 0x16,
    DoubleClickInterval = 0x17,
    ClickHoldInterval = 0x18,
    ListeningModeConfigs = 0x1A,
    OneBudAncMode = 0x1B,
    CrownRotationDirection = 0x1C,
    ListeningMode = 0x0D,
    AutoAnswerMode = 0x1E,
    ChimeVolume = 0x1F,
    VolumeSwipeInterval = 0x23,
    CallManagementConfig = 0x24,
    VolumeSwipeMode = 0x25,
    AdaptiveVolumeConfig = 0x26,
    SoftwareMuteConfig = 0x27,
    ConversationDetectConfig = 0x28,
    Ssl = 0x29,
    HearingAid = 0x2C,
    AutoAncStrength = 0x2E,
    HpsGainSwipe = 0x2F,
    HrmState = 0x30,
    InCaseToneConfig = 0x31,
    SiriMultitoneConfig = 0x32,
    HearingAssistConfig = 0x33,
    AllowOffOption = 0x34,
    StemConfig = 0x39,
    SleepDetectionConfig = 0x35,
    AllowAutoConnect = 0x36,
    EarDetectionConfig = 0x0A,
    AutomaticConnectionConfig = 0x20,
    OwnsConnection = 0x06,
}

impl ControlCommandIdentifiers {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(Self::MicMode),
            0x05 => Some(Self::ButtonSendMode),
            0x12 => Some(Self::VoiceTrigger),
            0x14 => Some(Self::SingleClickMode),
            0x15 => Some(Self::DoubleClickMode),
            0x16 => Some(Self::ClickHoldMode),
            0x17 => Some(Self::DoubleClickInterval),
            0x18 => Some(Self::ClickHoldInterval),
            0x1A => Some(Self::ListeningModeConfigs),
            0x1B => Some(Self::OneBudAncMode),
            0x1C => Some(Self::CrownRotationDirection),
            0x0D => Some(Self::ListeningMode),
            0x1E => Some(Self::AutoAnswerMode),
            0x1F => Some(Self::ChimeVolume),
            0x23 => Some(Self::VolumeSwipeInterval),
            0x24 => Some(Self::CallManagementConfig),
            0x25 => Some(Self::VolumeSwipeMode),
            0x26 => Some(Self::AdaptiveVolumeConfig),
            0x27 => Some(Self::SoftwareMuteConfig),
            0x28 => Some(Self::ConversationDetectConfig),
            0x29 => Some(Self::Ssl),
            0x2C => Some(Self::HearingAid),
            0x2E => Some(Self::AutoAncStrength),
            0x2F => Some(Self::HpsGainSwipe),
            0x30 => Some(Self::HrmState),
            0x31 => Some(Self::InCaseToneConfig),
            0x32 => Some(Self::SiriMultitoneConfig),
            0x33 => Some(Self::HearingAssistConfig),
            0x34 => Some(Self::AllowOffOption),
            0x39 => Some(Self::StemConfig),
            0x35 => Some(Self::SleepDetectionConfig),
            0x36 => Some(Self::AllowAutoConnect),
            0x0A => Some(Self::EarDetectionConfig),
            0x20 => Some(Self::AutomaticConnectionConfig),
            0x06 => Some(Self::OwnsConnection),
            _ => None,
        }
    }
}

impl std::fmt::Display for ControlCommandIdentifiers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ControlCommandIdentifiers::MicMode => "Mic Mode",
            ControlCommandIdentifiers::ButtonSendMode => "Button Send Mode",
            ControlCommandIdentifiers::VoiceTrigger => "Voice Trigger",
            ControlCommandIdentifiers::SingleClickMode => "Single Click Mode",
            ControlCommandIdentifiers::DoubleClickMode => "Double Click Mode",
            ControlCommandIdentifiers::ClickHoldMode => "Click Hold Mode",
            ControlCommandIdentifiers::DoubleClickInterval => "Double Click Interval",
            ControlCommandIdentifiers::ClickHoldInterval => "Click Hold Interval",
            ControlCommandIdentifiers::ListeningModeConfigs => "Listening Mode Configs",
            ControlCommandIdentifiers::OneBudAncMode => "One Bud ANC Mode",
            ControlCommandIdentifiers::CrownRotationDirection => "Crown Rotation Direction",
            ControlCommandIdentifiers::ListeningMode => "Listening Mode",
            ControlCommandIdentifiers::AutoAnswerMode => "Auto Answer Mode",
            ControlCommandIdentifiers::ChimeVolume => "Chime Volume",
            ControlCommandIdentifiers::VolumeSwipeInterval => "Volume Swipe Interval",
            ControlCommandIdentifiers::CallManagementConfig => "Call Management Config",
            ControlCommandIdentifiers::VolumeSwipeMode => "Volume Swipe Mode",
            ControlCommandIdentifiers::AdaptiveVolumeConfig => "Adaptive Volume Config",
            ControlCommandIdentifiers::SoftwareMuteConfig => "Software Mute Config",
            ControlCommandIdentifiers::ConversationDetectConfig => "Conversation Detect Config",
            ControlCommandIdentifiers::Ssl => "SSL",
            ControlCommandIdentifiers::HearingAid => "Hearing Aid",
            ControlCommandIdentifiers::AutoAncStrength => "Auto ANC Strength",
            ControlCommandIdentifiers::HpsGainSwipe => "HPS Gain Swipe",
            ControlCommandIdentifiers::HrmState => "HRM State",
            ControlCommandIdentifiers::InCaseToneConfig => "In Case Tone Config",
            ControlCommandIdentifiers::SiriMultitoneConfig => "Siri Multitone Config",
            ControlCommandIdentifiers::HearingAssistConfig => "Hearing Assist Config",
            ControlCommandIdentifiers::AllowOffOption => "Allow Off Option",
            ControlCommandIdentifiers::StemConfig => "Stem Config",
            ControlCommandIdentifiers::SleepDetectionConfig => "Sleep Detection Config",
            ControlCommandIdentifiers::AllowAutoConnect => "Allow Auto Connect",
            ControlCommandIdentifiers::EarDetectionConfig => "Ear Detection Config",
            ControlCommandIdentifiers::AutomaticConnectionConfig => "Automatic Connection Config",
            ControlCommandIdentifiers::OwnsConnection => "Owns Connection",
        };
        f.write_str(name)
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum ProximityKeyType {
    Irk = 0x01,
    EncKey = 0x04,
}

impl ProximityKeyType {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(Self::Irk),
            0x04 => Some(Self::EncKey),
            _ => None,
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StemPressType {
    Single = 0x05,
    Double = 0x06,
    Triple = 0x07,
    Long = 0x08,
}

impl StemPressType {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x05 => Some(Self::Single),
            0x06 => Some(Self::Double),
            0x07 => Some(Self::Triple),
            0x08 => Some(Self::Long),
            _ => None,
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StemPressBudType {
    Left = 0x01,
    Right = 0x02,
}

impl StemPressBudType {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(Self::Left),
            0x02 => Some(Self::Right),
            _ => None,
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSourceType {
    None = 0x00,
    Call = 0x01,
    Media = 0x02,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryComponent {
    Headphone = 1,
    Left = 4,
    Right = 2,
    Case = 8,
}

impl BatteryComponent {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(Self::Headphone),
            0x02 => Some(Self::Right),
            0x04 => Some(Self::Left),
            0x08 => Some(Self::Case),
            _ => None,
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryStatus {
    Charging = 1,
    NotCharging = 2,
    Disconnected = 4,
    /// Apple's optimized charging, reported by a bud sitting in the case.
    OptimizedCharging = 5,
}

impl BatteryStatus {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(Self::Charging),
            0x02 => Some(Self::NotCharging),
            0x04 => Some(Self::Disconnected),
            0x05 => Some(Self::OptimizedCharging),
            _ => None,
        }
    }

    pub fn is_charging(self) -> bool {
        matches!(self, Self::Charging | Self::OptimizedCharging)
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarDetectionStatus {
    InEar = 0x00,
    OutOfEar = 0x01,
    InCase = 0x02,
    Disconnected = 0x03,
}

impl EarDetectionStatus {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x00 => Some(Self::InEar),
            0x01 => Some(Self::OutOfEar),
            0x02 => Some(Self::InCase),
            0x03 => Some(Self::Disconnected),
            _ => None,
        }
    }
}

impl AudioSourceType {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x00 => Some(Self::None),
            0x01 => Some(Self::Call),
            0x02 => Some(Self::Media),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSource {
    pub mac: String,
    pub r#type: AudioSourceType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatteryInfo {
    pub component: BatteryComponent,
    pub level: u8,
    pub status: BatteryStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedDevice {
    pub mac: String,
    pub info1: u8,
    pub info2: u8,
    pub r#type: Option<String>,
}

#[derive(Debug, Clone)]
pub enum AACPEvent {
    BatteryInfo(Vec<BatteryInfo>),
    ControlCommand(ControlCommandStatus),
    EarDetection(Vec<EarDetectionStatus>, Vec<EarDetectionStatus>),
    ConversationalAwareness(u8),
    ConnectedDevices(Vec<ConnectedDevice>, Vec<ConnectedDevice>),
    OwnershipToFalseRequest,
    StemPress(StemPressType, StemPressBudType),
    CustomEq(CustomEq),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AirPodsLEKeys {
    pub irk: String,
    pub enc_key: String,
}

/// Why an AACP packet was not handed to the connection.
#[derive(Debug, thiserror::Error)]
pub enum AacpError {
    #[error("AACP channel is not connected")]
    NotConnected,
    #[error("AACP send channel closed")]
    SendChannelClosed,
    #[error("AACP state lock held for over {STATE_LOCK_TIMEOUT:?}, packet not sent")]
    StateBusy,
    #[error("invalid MAC address {0:?}")]
    InvalidMac(String),
    #[error("name is {0} bytes long, at most 255 fit in the rename packet")]
    NameTooLong(usize),
    #[error("smart routing body is {0} bytes, more than its 16-bit length holds")]
    BodyTooLong(usize),
}

/// Where device records (AirPods information and LE keys) are kept between
/// runs.
pub(crate) trait DeviceStore: Send + Sync {
    fn load(&self) -> HashMap<String, DeviceData>;
    fn save(&self, mac: String, data: DeviceData) -> io::Result<()>;
}

/// devices.json in the app's data directory.
struct DevicesFile;

impl DeviceStore for DevicesFile {
    fn load(&self) -> HashMap<String, DeviceData> {
        std::fs::read_to_string(get_devices_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, mac: String, data: DeviceData) -> io::Result<()> {
        update_devices_file(|devices| {
            devices.insert(mac, data);
        })
    }
}

/// A device record changed by a received packet, to be written to the store.
#[derive(Debug)]
struct DeviceUpdate {
    mac: String,
    data: DeviceData,
}

/// One message from the AirPods, decoded from its bytes.
#[derive(Debug)]
enum Incoming {
    BatteryInfo(Vec<BatteryInfo>),
    ControlCommand(ControlCommandStatus),
    EarDetection(Vec<EarDetectionStatus>),
    ConversationAwareness(u8),
    Information(Box<AirPodsInformation>),
    ProximityKeys(Vec<(u8, Vec<u8>)>),
    StemPress(StemPressType, StemPressBudType),
    AudioSource(AudioSource),
    ConnectedDevices(Vec<ConnectedDevice>),
    OwnershipToFalse,
    CustomEq(CustomEq),
}

/// Decode one AACP packet, header included. Malformed and unhandled packets
/// are logged and give `None`.
fn parse_packet(packet: &[u8]) -> Option<Incoming> {
    if !packet.starts_with(&HEADER_BYTES) {
        debug!(
            "Received packet does not start with expected header: {}",
            hex::encode(packet)
        );
        return None;
    }
    let Some(payload @ [opcode, ..]) = packet.get(HEADER_BYTES.len()..) else {
        debug!("Received packet too short: {}", hex::encode(packet));
        return None;
    };
    match *opcode {
        opcodes::BATTERY_INFO => parse_battery_info(payload).map(Incoming::BatteryInfo),
        opcodes::CONTROL_COMMAND => parse_control_command(payload).map(Incoming::ControlCommand),
        opcodes::EAR_DETECTION => parse_ear_detection(payload).map(Incoming::EarDetection),
        opcodes::CONVERSATION_AWARENESS => {
            parse_conversation_awareness(packet).map(Incoming::ConversationAwareness)
        },
        opcodes::INFORMATION => {
            parse_information(payload).map(|info| Incoming::Information(Box::new(info)))
        },
        opcodes::PROXIMITY_KEYS_RSP => parse_proximity_keys(payload).map(Incoming::ProximityKeys),
        opcodes::STEM_PRESS => {
            parse_stem_press(payload).map(|(press, bud)| Incoming::StemPress(press, bud))
        },
        opcodes::AUDIO_SOURCE => parse_audio_source(payload).map(Incoming::AudioSource),
        opcodes::CONNECTED_DEVICES => {
            parse_connected_devices(payload).map(Incoming::ConnectedDevices)
        },
        opcodes::SMART_ROUTING_RESP => {
            parse_smart_routing_response(payload).then_some(Incoming::OwnershipToFalse)
        },
        opcodes::EQ_DATA => {
            debug!("Received EQ Data");
            None
        },
        opcodes::CUSTOM_EQ => {
            let custom_eq = eq::parse(payload);
            if custom_eq.is_none() {
                warn!(
                    "Ignoring malformed custom EQ packet: {}",
                    hex::encode(packet)
                );
            }
            custom_eq.map(Incoming::CustomEq)
        },
        _ => {
            debug!("Received unknown packet with opcode {opcode:#04x}");
            None
        },
    }
}

/// Opcode, 0x00, count, then per component: component, 0x01, level, status,
/// 0x01. Entries with an unknown component or status are skipped.
fn parse_battery_info(payload: &[u8]) -> Option<Vec<BatteryInfo>> {
    let Some(&count) = payload.get(2) else {
        error!("Battery Info packet too short: {}", hex::encode(payload));
        return None;
    };
    let Some(entries) = payload.get(3..3 + usize::from(count) * 5) else {
        error!(
            "Battery Info packet length mismatch: {}",
            hex::encode(payload)
        );
        return None;
    };
    Some(
        entries
            .as_chunks::<5>()
            .0
            .iter()
            .copied()
            .filter_map(parse_battery_entry)
            .collect(),
    )
}

fn parse_battery_entry([component, _, level, status, _]: [u8; 5]) -> Option<BatteryInfo> {
    let Some(component) = BatteryComponent::from_u8(component) else {
        error!("Unknown battery component: {component:#04x}");
        return None;
    };
    let Some(status) = BatteryStatus::from_u8(status) else {
        error!("Unknown battery status: {status:#04x}");
        return None;
    };
    Some(BatteryInfo {
        component,
        level,
        status,
    })
}

/// Opcode, 0x00, identifier, four value bytes. The value is reported with its
/// trailing zero bytes trimmed, keeping at least one byte.
fn parse_control_command(payload: &[u8]) -> Option<ControlCommandStatus> {
    let &[_, _, identifier_byte, v0, v1, v2, v3, ..] = payload else {
        error!("Control Command packet too short: {}", hex::encode(payload));
        return None;
    };
    let value_bytes = [v0, v1, v2, v3];
    let value = match value_bytes.iter().rposition(|&b| b != 0) {
        Some(i) => value_bytes[..=i].to_vec(),
        None => vec![0],
    };
    let Some(identifier) = ControlCommandIdentifiers::from_u8(identifier_byte) else {
        error!("Unknown Control Command identifier: {identifier_byte:#04x}");
        return None;
    };
    Some(ControlCommandStatus { identifier, value })
}

/// Opcode, 0x00, primary bud status, secondary bud status. An unknown status
/// reads as out of ear.
fn parse_ear_detection(payload: &[u8]) -> Option<Vec<EarDetectionStatus>> {
    let &[_, _, primary, secondary, ..] = payload else {
        error!("Ear Detection packet too short: {}", hex::encode(payload));
        return None;
    };
    let status = |byte: u8| {
        EarDetectionStatus::from_u8(byte).unwrap_or_else(|| {
            error!("Unknown ear detection status: {byte:#04x}");
            EarDetectionStatus::OutOfEar
        })
    };
    Some(vec![status(primary), status(secondary)])
}

/// Exactly 10 bytes with the header; the status is the last one.
fn parse_conversation_awareness(packet: &[u8]) -> Option<u8> {
    if let [.., status] = *packet
        && packet.len() == 10
    {
        info!("Received Conversation Awareness: {status}");
        return Some(status);
    }
    info!(
        "Received Conversation Awareness packet with unexpected length: {}",
        packet.len()
    );
    None
}

/// Opcode, 0x00 and two more bytes, then zero separated strings. The first
/// two strings are not part of the information; the rest are its fields in
/// order, and missing ones are empty.
fn parse_information(payload: &[u8]) -> Option<AirPodsInformation> {
    if payload.len() < 6 {
        error!("Information packet too short: {}", hex::encode(payload));
        return None;
    }
    let data = &payload[4..];
    let first_zero = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    // Lossy, not skipped: the fields are positional, so dropping one bad
    // string would shift every field after it.
    let mut strings = data[first_zero..]
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned());
    if strings.next().is_none() {
        error!(
            "Information packet has no strings: {}",
            hex::encode(payload)
        );
        return None;
    }
    let mut field = || strings.next().unwrap_or_default();
    Some(AirPodsInformation {
        name: field(),
        model_number: field(),
        manufacturer: field(),
        serial_number: field(),
        version1: field(),
        version2: field(),
        hardware_revision: field(),
        updater_identifier: field(),
        left_serial_number: field(),
        right_serial_number: field(),
        version3: field(),
        le_keys: AirPodsLEKeys::default(),
    })
}

/// Opcode, 0x00, key count, then per key: type, 0x00, length, 0x00, key.
/// A key that runs past the end drops the whole packet.
fn parse_proximity_keys(payload: &[u8]) -> Option<Vec<(u8, Vec<u8>)>> {
    if payload.len() < 4 {
        error!(
            "Proximity Keys Response packet too short: {}",
            hex::encode(payload)
        );
        return None;
    }
    let key_count = payload[2];
    debug!("Proximity Keys Response contains {key_count} keys.");
    let mut offset = 3;
    let mut keys = Vec::new();
    for _ in 0..key_count {
        let Some(&[key_type, _, key_length, _]) = payload.get(offset..offset + 4) else {
            error!(
                "Proximity Keys Response packet too short while parsing keys: {}",
                hex::encode(payload)
            );
            return None;
        };
        offset += 4;
        let key_end = offset + usize::from(key_length);
        let Some(key_data) = payload.get(offset..key_end) else {
            error!(
                "Proximity Keys Response packet too short for key data: {}",
                hex::encode(payload)
            );
            return None;
        };
        keys.push((key_type, key_data.to_vec()));
        offset = key_end;
    }
    info!(
        "Received Proximity Keys Response: {:?}",
        keys.iter()
            .map(|(kt, kd)| (kt, hex::encode(kd)))
            .collect::<Vec<_>>()
    );
    Some(keys)
}

/// Opcode, 0x00, press type, bud.
fn parse_stem_press(payload: &[u8]) -> Option<(StemPressType, StemPressBudType)> {
    let &[_, _, press, bud, ..] = payload else {
        error!("Stem Press packet too short: {}", hex::encode(payload));
        return None;
    };
    let press_type = StemPressType::from_u8(press);
    let bud_type = StemPressBudType::from_u8(bud);
    if let (Some(press), Some(bud)) = (press_type, bud_type) {
        info!("Received Stem Press: {press:?} on {bud:?}");
        Some((press, bud))
    } else {
        error!("Invalid Stem Press packet - type: {press_type:?}, bud: {bud_type:?}");
        None
    }
}

/// Opcode, 0x00, the source MAC least significant byte first, source type.
fn parse_audio_source(payload: &[u8]) -> Option<AudioSource> {
    let &[_, _, m0, m1, m2, m3, m4, m5, typ, ..] = payload else {
        error!("Audio Source packet too short: {}", hex::encode(payload));
        return None;
    };
    Some(AudioSource {
        mac: format!("{m5:02X}:{m4:02X}:{m3:02X}:{m2:02X}:{m1:02X}:{m0:02X}"),
        r#type: AudioSourceType::from_u8(typ).unwrap_or(AudioSourceType::None),
    })
}

/// Opcode, 0x00, count, two bytes, then per device: MAC most significant
/// byte first and two info bytes.
fn parse_connected_devices(payload: &[u8]) -> Option<Vec<ConnectedDevice>> {
    let Some(&count) = payload.get(2) else {
        error!(
            "Connected Devices packet too short: {}",
            hex::encode(payload)
        );
        return None;
    };
    let Some(entries) = payload.get(5..5 + usize::from(count) * 8) else {
        error!(
            "Connected Devices packet length mismatch: {}",
            hex::encode(payload)
        );
        return None;
    };
    let devices = entries
        .as_chunks::<8>()
        .0
        .iter()
        .map(|&[m0, m1, m2, m3, m4, m5, info1, info2]| ConnectedDevice {
            mac: format!("{m0:02X}:{m1:02X}:{m2:02X}:{m3:02X}:{m4:02X}:{m5:02X}"),
            info1,
            info2,
            r#type: None,
        })
        .collect();
    Some(devices)
}

/// Whether a smart routing response asks this host to give up ownership.
fn parse_smart_routing_response(payload: &[u8]) -> bool {
    let packet_string = String::from_utf8_lossy(payload.get(2..).unwrap_or_default());
    info!("Received Smart Routing Response: {packet_string}");
    let give_up = packet_string.contains("SetOwnershipToFalse");
    if give_up {
        info!("Received OwnershipToFalse request");
    }
    give_up
}

pub struct AACPManagerState {
    pub sender: Option<mpsc::Sender<Vec<u8>>>,
    pub control_command_status_list: Vec<ControlCommandStatus>,
    pub control_command_subscribers:
        HashMap<ControlCommandIdentifiers, Vec<mpsc::UnboundedSender<Vec<u8>>>>,
    pub owns: bool,
    pub old_connected_devices: Vec<ConnectedDevice>,
    pub connected_devices: Vec<ConnectedDevice>,
    pub audio_source: Option<AudioSource>,
    pub battery_info: Vec<BatteryInfo>,
    pub conversational_awareness_status: u8,
    pub old_ear_detection_status: Vec<EarDetectionStatus>,
    pub ear_detection_status: Vec<EarDetectionStatus>,
    event_tx: Option<mpsc::UnboundedSender<AACPEvent>>,
    pub devices: HashMap<String, DeviceData>,
    pub airpods_mac: Option<Address>,
    /// When set, recv_thread forwards raw 0x58 audio SDUs here (hi-res mic).
    pub audio_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Last custom EQ the AirPods reported, for a window opened after it arrived.
    pub custom_eq: Option<CustomEq>,
}

impl AACPManagerState {
    fn new(devices: HashMap<String, DeviceData>) -> Self {
        AACPManagerState {
            sender: None,
            control_command_status_list: Vec::new(),
            control_command_subscribers: HashMap::new(),
            owns: false,
            old_connected_devices: Vec::new(),
            connected_devices: Vec::new(),
            audio_source: None,
            battery_info: Vec::new(),
            conversational_awareness_status: 0,
            old_ear_detection_status: Vec::new(),
            ear_detection_status: Vec::new(),
            event_tx: None,
            devices,
            airpods_mac: None,
            audio_tx: None,
            custom_eq: None,
        }
    }

    fn emit(&self, event: AACPEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event);
        }
    }

    /// Update the state from one decoded message and emit its event. Returns
    /// the device record to persist when the message changed one.
    fn apply(&mut self, incoming: Incoming) -> Option<DeviceUpdate> {
        match incoming {
            Incoming::BatteryInfo(batteries) => {
                self.battery_info.clone_from(&batteries);
                self.emit(AACPEvent::BatteryInfo(batteries));
                info!("Received Battery Info: {:?}", self.battery_info);
            },
            Incoming::ControlCommand(status) => {
                info!(
                    "Received Control Command: {:?}, value: {}",
                    status.identifier,
                    hex::encode(&status.value)
                );
                self.record_control_command(status);
            },
            Incoming::EarDetection(statuses) => self.apply_ear_detection(statuses),
            Incoming::ConversationAwareness(status) => {
                self.conversational_awareness_status = status;
                self.emit(AACPEvent::ConversationalAwareness(status));
            },
            Incoming::Information(info) => return self.apply_information(*info),
            Incoming::ProximityKeys(keys) => return self.apply_proximity_keys(&keys),
            Incoming::StemPress(press, bud) => self.emit(AACPEvent::StemPress(press, bud)),
            Incoming::AudioSource(audio_source) => {
                self.audio_source = Some(audio_source);
                info!("Received Audio Source: {:?}", self.audio_source);
            },
            Incoming::ConnectedDevices(devices) => {
                self.old_connected_devices =
                    std::mem::replace(&mut self.connected_devices, devices.clone());
                self.emit(AACPEvent::ConnectedDevices(
                    self.old_connected_devices.clone(),
                    devices,
                ));
                info!("Received Connected Devices: {:?}", self.connected_devices);
            },
            Incoming::OwnershipToFalse => self.emit(AACPEvent::OwnershipToFalseRequest),
            Incoming::CustomEq(custom_eq) => {
                info!("Received custom EQ: {custom_eq:?}");
                self.custom_eq = Some(custom_eq);
                self.emit(AACPEvent::CustomEq(custom_eq));
            },
        }
        None
    }

    /// Store a control command value, tell its subscribers and emit it.
    fn record_control_command(&mut self, status: ControlCommandStatus) {
        let identifier = status.identifier;
        if let Some(existing) = self
            .control_command_status_list
            .iter_mut()
            .find(|s| s.identifier == identifier)
        {
            existing.value.clone_from(&status.value);
        } else {
            self.control_command_status_list.push(status.clone());
        }
        if identifier == ControlCommandIdentifiers::OwnsConnection {
            self.owns = status.value.first().is_some_and(|&b| b != 0);
        }
        if let Some(subscribers) = self.control_command_subscribers.get(&identifier) {
            for sub in subscribers {
                let _ = sub.send(status.value.clone());
            }
        }
        self.emit(AACPEvent::ControlCommand(status));
    }

    fn apply_ear_detection(&mut self, statuses: Vec<EarDetectionStatus>) {
        self.old_ear_detection_status =
            std::mem::replace(&mut self.ear_detection_status, statuses.clone());
        if self.event_tx.is_some() {
            debug!(
                "Sending Ear Detection event: old: {:?}, new: {:?}",
                self.old_ear_detection_status, statuses
            );
            self.emit(AACPEvent::EarDetection(
                self.old_ear_detection_status.clone(),
                statuses,
            ));
        }
        info!(
            "Received Ear Detection Status: {:?}",
            self.ear_detection_status
        );
    }

    /// Store the information in the record of the connected AirPods, if it
    /// has one.
    fn apply_information(&mut self, mut info: AirPodsInformation) -> Option<DeviceUpdate> {
        let update = if let Some(mac) = self.airpods_mac
            && let Some(device_data) = self.devices.get_mut(&mac.to_string())
        {
            // The LE keys come from a separate response; this packet must
            // not wipe the ones already stored.
            if let Some(DeviceInformation::AirPods(old)) = &device_data.information {
                info.le_keys = old.le_keys.clone();
            }
            device_data.name.clone_from(&info.name);
            device_data.information = Some(DeviceInformation::AirPods(info.clone()));
            Some(DeviceUpdate {
                mac: mac.to_string(),
                data: device_data.clone(),
            })
        } else {
            None
        };
        info!("Received Information: {info:?}");
        update
    }

    /// Store the IRK and encryption key in the record of the connected
    /// AirPods, creating the record when the keys arrive first.
    fn apply_proximity_keys(&mut self, keys: &[(u8, Vec<u8>)]) -> Option<DeviceUpdate> {
        let mac = self.airpods_mac?.to_string();
        let device_data = self.devices.entry(mac.clone()).or_insert(DeviceData {
            name: mac.clone(),
            type_: DeviceType::AirPods,
            information: None,
        });
        // The keys may arrive before the information packet; start an empty
        // record for them instead of dropping them.
        if !matches!(device_data.information, Some(DeviceInformation::AirPods(_))) {
            device_data.information =
                Some(DeviceInformation::AirPods(AirPodsInformation::default()));
        }
        if let Some(DeviceInformation::AirPods(info)) = device_data.information.as_mut() {
            for (key_type, key_data) in keys {
                match ProximityKeyType::from_u8(*key_type) {
                    Some(ProximityKeyType::Irk) => info.le_keys.irk = hex::encode(key_data),
                    Some(ProximityKeyType::EncKey) => {
                        info.le_keys.enc_key = hex::encode(key_data);
                    },
                    None => {},
                }
            }
        }
        Some(DeviceUpdate {
            mac,
            data: device_data.clone(),
        })
    }

    /// Forget everything read from a connection that has ended.
    fn reset_connection(&mut self) {
        self.sender = None;
        self.audio_tx = None;
        self.owns = false;
        self.connected_devices.clear();
        self.control_command_status_list.clear();
        self.ear_detection_status.clear();
        self.battery_info.clear();
    }
}

#[derive(Clone)]
pub struct AACPManager {
    pub state: Arc<Mutex<AACPManagerState>>,
    store: Arc<dyn DeviceStore>,
    tasks: Arc<Mutex<JoinSet<()>>>,
    /// Tasks that serve this connection (event handling, subscribers, the
    /// playback listener). They hold clones of the manager, so they never end on
    /// their own; recv_thread aborts them when the link goes away.
    connection_tasks: Arc<std::sync::Mutex<Vec<AbortHandle>>>,
    hires_enabled: Arc<AtomicBool>,
    hires_mic: Arc<Mutex<Option<HiResMic>>>,
    /// Wakes the hi-res monitor so it re-polls promptly when the feature is
    /// toggled, instead of waiting out the poll interval.
    hires_wake: Arc<Notify>,
    mic_status: MicStatus,
    /// Handle to the long-lived backend runtime, so the hi-res monitor task
    /// survives even when armed from a throwaway runtime (the UI toggle thread).
    runtime: tokio::runtime::Handle,
}

impl AACPManager {
    /// A manager backed by devices.json and the saved app settings. Must be
    /// called inside the backend runtime, whose handle it keeps.
    pub fn new() -> Self {
        Self::with_deps(Arc::new(DevicesFile), AppSettings::load().hires_mic_enabled)
    }

    /// A manager that keeps device records in `store`, with the hi-res
    /// microphone feature initially `hires_enabled`.
    pub(crate) fn with_deps(store: Arc<dyn DeviceStore>, hires_enabled: bool) -> Self {
        AACPManager {
            state: Arc::new(Mutex::new(AACPManagerState::new(store.load()))),
            store,
            tasks: Arc::new(Mutex::new(JoinSet::new())),
            connection_tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
            hires_enabled: Arc::new(AtomicBool::new(hires_enabled)),
            hires_mic: Arc::new(Mutex::new(None)),
            hires_wake: Arc::new(Notify::new()),
            mic_status: MicStatus::new(),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    pub async fn conversation_detection_enabled(&self) -> bool {
        self.state
            .lock()
            .await
            .control_command_status_list
            .iter()
            .find(|s| s.identifier == ControlCommandIdentifiers::ConversationDetectConfig)
            .is_some_and(|s| s.value.first() == Some(&0x01))
    }

    pub async fn set_conversation_detection(&self, enabled: bool) {
        let value = if enabled { 0x01 } else { 0x02 };
        info!("[aacp] setting conversation detection to {enabled}");
        if let Err(e) = self
            .send_control_command(
                ControlCommandIdentifiers::ConversationDetectConfig,
                &[value],
            )
            .await
        {
            warn!("[aacp] failed to set conversation detection: {e}");
            return;
        }
        // AirPods don't echo this back, so record it in our own status list
        // (the source of truth for conversation_detection_enabled) and push it
        // to the UI and to the subscribers (the tray checkmark) ourselves.
        self.state
            .lock()
            .await
            .record_control_command(ControlCommandStatus {
                identifier: ControlCommandIdentifiers::ConversationDetectConfig,
                value: vec![value],
            });
    }

    /// Whether battery or ear detection status has arrived on this connection.
    pub async fn has_device_status(&self) -> bool {
        let state = self.state.lock().await;
        !state.battery_info.is_empty() || !state.ear_detection_status.is_empty()
    }

    pub fn mic_level(&self) -> f32 {
        self.mic_status.level()
    }

    pub fn mic_active(&self) -> bool {
        self.mic_status.active()
    }

    pub fn mic_app(&self) -> Option<String> {
        self.mic_status.app()
    }

    /// Spawn a task that lives as long as this connection.
    pub fn spawn_connection_task(&self, task: impl Future<Output = ()> + Send + 'static) {
        self.track_connection_task(tokio::spawn(task).abort_handle());
    }

    /// Stop `handle` when this connection ends.
    pub fn track_connection_task(&self, handle: AbortHandle) {
        if let Ok(mut tasks) = self.connection_tasks.lock() {
            tasks.push(handle);
        }
    }

    fn stop_connection_tasks(&self) {
        if let Ok(mut tasks) = self.connection_tasks.lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
    }

    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }

    pub fn hires_mic_enabled(&self) -> bool {
        self.hires_enabled.load(Ordering::Relaxed)
    }

    pub fn hires_wake(&self) -> Arc<Notify> {
        self.hires_wake.clone()
    }

    pub async fn set_hires_mic_enabled(&self, enabled: bool) {
        self.hires_enabled.store(enabled, Ordering::Relaxed);
        self.hires_wake.notify_one();
        if enabled {
            self.arm_hires_mic().await;
        }
    }

    // Create the virtual device + monitor if the feature is enabled, AirPods are
    // connected, and it is not already armed. Called on enable and on connect.
    pub async fn arm_hires_mic(&self) {
        if !self.hires_enabled.load(Ordering::Relaxed) {
            return;
        }
        let mut guard = self.hires_mic.lock().await;
        if guard.as_ref().is_some_and(HiResMic::is_running) {
            return;
        }
        let addr = self.state.lock().await.airpods_mac.map(|a| a.to_string());
        let Some(addr) = addr else {
            return;
        };
        if let Some(mic) = HiResMic::start(self, addr, self.mic_status.clone()).await {
            *guard = Some(mic);
        } else {
            error!("Failed to start hi-res microphone");
        }
    }

    // Tear down the virtual device + monitor without clearing the enabled flag,
    // so a later reconnect re-arms. Called on disable and on disconnect.
    pub async fn disarm_hires_mic(&self) {
        let mic = self.hires_mic.lock().await.take();
        if let Some(mic) = mic {
            mic.stop().await;
        }
    }

    /// Open the AACP channel and start the send/receive tasks.
    pub async fn connect(&mut self, addr: Address) -> Result<(), ConnectError> {
        info!("AACPManager connecting to {addr} on PSM {PSM:#06X}...");
        self.state.lock().await.airpods_mac = Some(addr);

        let seq_packet = Arc::new(l2cap::connect_seq_packet(addr, PSM).await?);

        let (tx, rx) = mpsc::channel(128);
        self.attach_transport(tx).await;

        let manager_clone = self.clone();
        let mut tasks = self.tasks.lock().await;
        tasks.spawn(recv_thread(manager_clone, seq_packet.clone()));
        tasks.spawn(send_thread(rx, seq_packet));
        Ok(())
    }

    /// Route outgoing packets to `tx`. `connect` passes the channel its send
    /// task drains; tests pass their own and read what was sent.
    pub(crate) async fn attach_transport(&self, tx: mpsc::Sender<Vec<u8>>) {
        self.state.lock().await.sender = Some(tx);
    }

    async fn send_packet(&self, data: &[u8]) -> Result<(), AacpError> {
        // The sender is cloned and the lock released before awaiting. Holding
        // the state mutex across a full channel blocks every other task,
        // including the receive loop, and the manager stops responding
        // entirely. The timeout turns a stuck mutex into a logged error
        // instead of a hang with no diagnosis.
        let Ok(sender) = tokio::time::timeout(STATE_LOCK_TIMEOUT, async {
            self.state.lock().await.sender.clone()
        })
        .await
        else {
            error!("send_packet: state mutex held for over 2s, giving up");
            return Err(AacpError::StateBusy);
        };
        let Some(sender) = sender else {
            error!("Cannot send packet, sender is not available.");
            return Err(AacpError::NotConnected);
        };
        sender.send(data.to_vec()).await.map_err(|e| {
            error!("Failed to send packet to channel: {e}");
            AacpError::SendChannelClosed
        })
    }

    async fn send_data_packet(&self, data: &[u8]) -> Result<(), AacpError> {
        let packet = [HEADER_BYTES.as_slice(), data].concat();
        self.send_packet(&packet).await
    }

    pub async fn set_event_channel(&self, tx: mpsc::UnboundedSender<AACPEvent>) {
        let mut state = self.state.lock().await;
        state.event_tx = Some(tx);
    }

    pub async fn subscribe_to_control_command(
        &self,
        identifier: ControlCommandIdentifiers,
        tx: mpsc::UnboundedSender<Vec<u8>>,
    ) {
        let mut state = self.state.lock().await;
        // send initial value if available
        if let Some(status) = state
            .control_command_status_list
            .iter()
            .find(|s| s.identifier == identifier)
        {
            let _ = tx.send(status.value.clone());
        }
        state
            .control_command_subscribers
            .entry(identifier)
            .or_default()
            .push(tx);
    }

    /// Handle one control packet from the AirPods: update the state, emit its
    /// event and persist a changed device record.
    pub async fn receive_packet(&self, packet: &[u8]) {
        let Some(incoming) = parse_packet(packet) else {
            return;
        };
        let update = self.state.lock().await.apply(incoming);
        if let Some(update) = update {
            self.save_device(update).await;
        }
    }

    /// Persist one device record without blocking the runtime.
    async fn save_device(&self, update: DeviceUpdate) {
        let store = Arc::clone(&self.store);
        let result = tokio::task::spawn_blocking(move || store.save(update.mac, update.data)).await;
        match result {
            Ok(Ok(())) => {},
            Ok(Err(e)) => error!("Failed to save devices: {e}"),
            Err(e) => error!("Failed to save devices: {e}"),
        }
    }

    /// Create the channel that recv_thread forwards 0x58 audio SDUs to, and
    /// return the receiving half for the hi-res decode task. Replaces any
    /// previously installed channel.
    pub async fn take_audio_channel(&self) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel(AUDIO_CHANNEL_CAP);
        let mut state = self.state.lock().await;
        state.audio_tx = Some(tx);
        rx
    }

    /// Stop forwarding audio SDUs
    pub async fn clear_audio_channel(&self) {
        let mut state = self.state.lock().await;
        state.audio_tx = None;
    }

    /// Start the proprietary hi-res microphone stream (0x58 START).
    pub async fn send_start_audio(&self) -> Result<(), AacpError> {
        self.send_packet(&AACP_START_AUDIO).await
    }

    /// Stop the proprietary hi-res microphone stream (0x58 STOP).
    pub async fn send_stop_audio(&self) -> Result<(), AacpError> {
        self.send_packet(&AACP_STOP_AUDIO).await
    }

    pub async fn send_custom_eq(&self, custom_eq: &CustomEq) -> Result<(), AacpError> {
        self.send_data_packet(&custom_eq.to_packet()).await
    }

    pub async fn send_notification_request(&self) -> Result<(), AacpError> {
        let opcode = [opcodes::REQUEST_NOTIFICATIONS, 0x00];
        let data = [0xFF, 0xFF, 0xFF, 0xFF];
        let packet = [opcode.as_slice(), data.as_slice()].concat();
        self.send_data_packet(&packet).await
    }

    pub async fn send_set_feature_flags_packet(&self) -> Result<(), AacpError> {
        let opcode = [opcodes::SET_FEATURE_FLAGS, 0x00];
        // let data = [0xD7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let data = [0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]; // adaptive volume is actually useful, seeing if it works
        let packet = [opcode.as_slice(), data.as_slice()].concat();
        self.send_data_packet(&packet).await
    }

    pub async fn send_handshake(&self) -> Result<(), AacpError> {
        let packet = [
            0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        self.send_packet(&packet).await
    }

    pub async fn send_proximity_keys_request(
        &self,
        key_types: Vec<ProximityKeyType>,
    ) -> Result<(), AacpError> {
        let mask = key_types.iter().fold(0u8, |acc, kt| acc | (*kt as u8));
        self.send_data_packet(&[opcodes::PROXIMITY_KEYS_REQ, 0x00, mask, 0x00])
            .await
    }

    pub async fn send_rename_packet(&self, name: &str) -> Result<(), AacpError> {
        let packet = rename_payload(name)?;
        self.send_data_packet(&packet).await
    }

    pub async fn send_control_command(
        &self,
        identifier: ControlCommandIdentifiers,
        value: &[u8],
    ) -> Result<(), AacpError> {
        let mut packet = vec![opcodes::CONTROL_COMMAND, 0x00, identifier as u8];
        packet.extend((0..4).map(|i| value.get(i).copied().unwrap_or(0)));
        self.send_data_packet(&packet).await
    }

    pub async fn send_media_information_new_device(
        &self,
        self_mac_address: &str,
        target_mac_address: &str,
    ) -> Result<(), AacpError> {
        let opcode = [opcodes::SMART_ROUTING, 0x00];
        let mut buffer = Vec::with_capacity(112);
        buffer.extend_from_slice(&mac_to_wire(target_mac_address)?);

        buffer.extend_from_slice(&[0x68, 0x00]);
        buffer.extend_from_slice(&[0x01, 0xE5, 0x4A]);
        buffer.extend_from_slice(b"playingApp");
        buffer.push(0x42);
        buffer.extend_from_slice(b"NA");
        buffer.push(0x52);
        buffer.extend_from_slice(b"hostStreamingState");
        buffer.push(0x42);
        buffer.extend_from_slice(b"NO");
        buffer.push(0x49);
        buffer.extend_from_slice(b"btAddress");
        buffer.push(0x51);
        buffer.extend_from_slice(self_mac_address.as_bytes());
        buffer.push(0x46);
        buffer.extend_from_slice(b"btName");
        buffer.push(0x43);
        buffer.extend_from_slice(b"Mac");
        buffer.push(0x58);
        buffer.extend_from_slice(b"otherDevice");
        buffer.extend_from_slice(b"AudioCategory");
        buffer.extend_from_slice(&[0x30, 0x64]);

        let packet = [opcode.as_slice(), buffer.as_slice()].concat();
        self.send_data_packet(&packet).await
    }

    pub async fn send_hijack_request(&self, target_mac_address: &str) -> Result<(), AacpError> {
        let opcode = [opcodes::SMART_ROUTING, 0x00];
        let mut buffer = Vec::with_capacity(106);
        buffer.extend_from_slice(&mac_to_wire(target_mac_address)?);
        buffer.extend_from_slice(&[0x62, 0x00]);
        buffer.extend_from_slice(&[0x01, 0xE5]);
        buffer.push(0x4A);
        buffer.extend_from_slice(b"localscore");
        buffer.extend_from_slice(&[0x30, 0x64]);
        buffer.push(0x46);
        buffer.extend_from_slice(b"reason");
        buffer.push(0x48);
        buffer.extend_from_slice(b"Hijackv2");
        buffer.push(0x51);
        buffer.extend_from_slice(b"audioRoutingScore");
        buffer.extend_from_slice(&[0x31, 0x2D, 0x01, 0x5F]);
        buffer.extend_from_slice(b"audioRoutingSetOwnershipToFalse");
        buffer.push(0x01);
        buffer.push(0x4B);
        buffer.extend_from_slice(b"remotescore");
        buffer.push(0xA5);

        while buffer.len() < 106 {
            buffer.push(0x00);
        }

        let packet = [opcode.as_slice(), buffer.as_slice()].concat();
        self.send_data_packet(&packet).await
    }

    pub async fn send_media_information(
        &self,
        self_mac_address: &str,
        target_mac_address: &str,
        streaming_state: bool,
    ) -> Result<(), AacpError> {
        let packet =
            media_information_payload(self_mac_address, target_mac_address, streaming_state)?;
        self.send_data_packet(&packet).await
    }

    pub async fn send_smart_routing_show_ui(
        &self,
        target_mac_address: &str,
    ) -> Result<(), AacpError> {
        let opcode = [opcodes::SMART_ROUTING, 0x00];
        let mut buffer = Vec::with_capacity(134);
        buffer.extend_from_slice(&mac_to_wire(target_mac_address)?);
        buffer.extend_from_slice(&[0x7E, 0x00]);
        buffer.extend_from_slice(&[0x01, 0xE6, 0x5B]);
        buffer.extend_from_slice(b"SmartRoutingKeyShowNearbyUI");
        buffer.push(0x01);
        buffer.push(0x4A);
        buffer.extend_from_slice(b"localscore");
        buffer.extend_from_slice(&[0x31, 0x2D]);
        buffer.push(0x01);
        buffer.push(0x46);
        buffer.extend_from_slice(b"reasonHhijackv2");
        buffer.push(0x51);
        buffer.extend_from_slice(b"audioRoutingScore");
        buffer.push(0xA2);
        buffer.push(0x5F);
        buffer.extend_from_slice(b"audioRoutingSetOwnershipToFalse");
        buffer.push(0x01);
        buffer.push(0x4B);
        buffer.extend_from_slice(b"remotescore");
        buffer.push(0xA2);

        while buffer.len() < 134 {
            buffer.push(0x00);
        }

        let packet = [opcode.as_slice(), buffer.as_slice()].concat();
        self.send_data_packet(&packet).await
    }

    pub async fn send_add_tipi_device(
        &self,
        self_mac_address: &str,
        target_mac_address: &str,
    ) -> Result<(), AacpError> {
        let opcode = [opcodes::SMART_ROUTING, 0x00];
        let mut buffer = Vec::with_capacity(86);
        buffer.extend_from_slice(&mac_to_wire(target_mac_address)?);
        buffer.extend_from_slice(&[0x4E, 0x00]);
        buffer.extend_from_slice(&[0x01, 0xE5]);
        buffer.push(0x48);
        buffer.extend_from_slice(b"idleTime");
        buffer.extend_from_slice(&[0x08, 0x47]);
        buffer.extend_from_slice(b"newTipi");
        buffer.extend_from_slice(&[0x01, 0x49]);
        buffer.extend_from_slice(b"btAddress");
        buffer.push(0x51);
        buffer.extend_from_slice(self_mac_address.as_bytes());
        buffer.push(0x46);
        buffer.extend_from_slice(b"btName");
        buffer.push(0x43);
        buffer.extend_from_slice(b"Mac");
        buffer.push(0x50);
        buffer.extend_from_slice(b"nearbyAudioScore");
        buffer.push(0x0E);

        let packet = [opcode.as_slice(), buffer.as_slice()].concat();
        self.send_data_packet(&packet).await
    }

    pub async fn send_some_packet(&self) -> Result<(), AacpError> {
        self.send_data_packet(&[0x29, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
            .await
    }
}

async fn recv_thread(manager: AACPManager, sp: Arc<SeqPacket>) {
    let mut buf = vec![0u8; RECV_BUF_LEN];
    manager.arm_hires_mic().await;
    loop {
        match sp.recv(&mut buf).await {
            Ok(0) => {
                info!("Remote closed the connection.");
                break;
            },
            Ok(n) => {
                let data = &buf[..n];

                // Forward audio SDUs to the audio thread, leaving control SDUs for the control thread.
                if crate::bluetooth::aacp_audio::is_audio(data) {
                    // Device-level liveness for the hi-res stall watchdog.
                    manager.mic_status.mark_sdu();
                    let audio_tx = manager.state.lock().await.audio_tx.clone();
                    if let Some(tx) = audio_tx {
                        let _ = tx.try_send(data.to_vec());
                    }
                    continue;
                }

                debug!("Received {} bytes: {}", n, hex::encode(data));
                manager.receive_packet(data).await;
            },
            Err(e) => {
                info!("Read error, the AirPods probably disconnected: {e}");
                break;
            },
        }
    }
    // Both exits end the connection: nothing read from it is current any more.
    manager.state.lock().await.reset_connection();
    manager.disarm_hires_mic().await;
    manager.stop_connection_tasks();
}

async fn send_thread(mut rx: mpsc::Receiver<Vec<u8>>, sp: Arc<SeqPacket>) {
    while let Some(data) = rx.recv().await {
        let mut attempts = 0;
        loop {
            match sp.send(&data).await {
                Ok(_) => {
                    debug!("Sent {} bytes: {}", data.len(), hex::encode(&data));
                    break;
                },
                Err(e) if e.kind() == io::ErrorKind::NotConnected && attempts < 10 => {
                    attempts += 1;
                    sleep(Duration::from_millis(100)).await;
                },
                Err(e) => {
                    error!("Failed to send data: {e}");
                    return;
                },
            }
        }
    }
    info!("Send thread finished.");
}

/// A colon-separated MAC address as the 6 bytes AACP sends, least significant
/// byte first.
fn mac_to_wire(mac: &str) -> Result<[u8; 6], AacpError> {
    let addr: Address = mac
        .parse()
        .map_err(|_| AacpError::InvalidMac(mac.to_string()))?;
    let mut bytes = addr.0;
    bytes.reverse();
    Ok(bytes)
}

/// Rename payload: opcode, 0x00, 0x01, name length (one byte), 0x00, name.
fn rename_payload(name: &str) -> Result<Vec<u8>, AacpError> {
    let name_bytes = name.as_bytes();
    let size =
        u8::try_from(name_bytes.len()).map_err(|_| AacpError::NameTooLong(name_bytes.len()))?;
    let mut packet = Vec::with_capacity(5 + name_bytes.len());
    packet.extend_from_slice(&[opcodes::RENAME, 0x00, 0x01, size, 0x00]);
    packet.extend_from_slice(name_bytes);
    Ok(packet)
}

/// Smart routing media information for `target_mac_address`.
///
/// After the target MAC comes a little-endian u16 with the number of bytes
/// that follow it, not counting the zero padding; every other smart routing
/// packet here follows that rule. Each string is prefixed with 0x40 plus its
/// length (0x4A before the 10 bytes of "PlayingApp", 0x46 before "btName").
fn media_information_payload(
    self_mac_address: &str,
    target_mac_address: &str,
    streaming_state: bool,
) -> Result<Vec<u8>, AacpError> {
    // The 0x51 tag below is for a 17 character address.
    mac_to_wire(self_mac_address)?;
    let (state_tag, state): (u8, &[u8]) = if streaming_state {
        (0x43, b"YES")
    } else {
        (0x42, b"NO")
    };

    let mut body = Vec::with_capacity(128);
    body.extend_from_slice(&[0x01, 0xE5, 0x4A]);
    body.extend_from_slice(b"PlayingApp");
    body.push(0x56);
    body.extend_from_slice(b"com.google.ios.youtube");
    body.push(0x52);
    body.extend_from_slice(b"HostStreamingState");
    body.push(state_tag);
    body.extend_from_slice(state);
    body.push(0x49);
    body.extend_from_slice(b"btAddress");
    body.push(0x51);
    body.extend_from_slice(self_mac_address.as_bytes());
    body.push(0x46);
    body.extend_from_slice(b"btName");
    body.push(0x43);
    body.extend_from_slice(b"Mac");
    body.push(0x58);
    body.extend_from_slice(b"otherDevice");
    body.extend_from_slice(b"AudioCategory");
    body.extend_from_slice(&[0x31, 0x2D, 0x01]);
    let body_len = u16::try_from(body.len()).map_err(|_| AacpError::BodyTooLong(body.len()))?;

    let mut packet = Vec::with_capacity(2 + 138);
    packet.extend_from_slice(&[opcodes::SMART_ROUTING, 0x00]);
    packet.extend_from_slice(&mac_to_wire(target_mac_address)?);
    packet.extend_from_slice(&body_len.to_le_bytes());
    packet.extend_from_slice(&body);
    packet.resize(packet.len().max(2 + 138), 0x00);
    Ok(packet)
}
/// Fakes that let tests drive an `AACPManager` without Bluetooth or files.
#[cfg(test)]
pub(crate) mod testing {
    use {
        super::{AACPManager, DeviceStore},
        crate::devices::enums::DeviceData,
        std::{
            collections::HashMap,
            io,
            sync::{Arc, Mutex},
        },
        tokio::sync::mpsc,
    };

    /// Device records in memory, with every save recorded in order.
    #[derive(Default)]
    pub(crate) struct MemoryStore {
        pub(crate) initial: HashMap<String, DeviceData>,
        pub(crate) saved: Mutex<Vec<(String, DeviceData)>>,
    }

    impl MemoryStore {
        pub(crate) fn saved(&self) -> Vec<(String, DeviceData)> {
            self.saved.lock().unwrap().clone()
        }
    }

    impl DeviceStore for MemoryStore {
        fn load(&self) -> HashMap<String, DeviceData> {
            self.initial.clone()
        }

        fn save(&self, mac: String, data: DeviceData) -> io::Result<()> {
            self.saved.lock().unwrap().push((mac, data));
            Ok(())
        }
    }

    /// A manager over `store` whose outgoing packets land in the returned
    /// receiver, as if connected.
    pub(crate) async fn connected_manager(
        store: Arc<MemoryStore>,
    ) -> (AACPManager, mpsc::Receiver<Vec<u8>>) {
        let manager = AACPManager::with_deps(store, false);
        let (tx, rx) = mpsc::channel(64);
        manager.attach_transport(tx).await;
        (manager, rx)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{
            testing::{MemoryStore, connected_manager},
            *,
        },
        proptest::prelude::*,
    };

    #[test]
    fn media_information_length_counts_the_bytes_after_it() {
        for streaming in [false, true] {
            let packet =
                media_information_payload("11:22:33:44:55:66", "AA:BB:CC:DD:EE:FF", streaming)
                    .unwrap();
            assert_eq!(packet.len(), 2 + 138);
            assert_eq!(packet[2..8], [0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA]);
            let len = usize::from(u16::from_le_bytes([packet[8], packet[9]]));
            let body = &packet[10..10 + len];
            assert!(body.ends_with(&[0x31, 0x2D, 0x01]));
            assert!(packet[10 + len..].iter().all(|&b| b == 0));
            let name = body.windows(7).position(|w| w == b"\x46btName");
            assert!(name.is_some(), "btName has no string tag");
        }
    }

    #[test]
    fn media_information_tags_the_streaming_state_by_length() {
        let yes =
            media_information_payload("11:22:33:44:55:66", "AA:BB:CC:DD:EE:FF", true).unwrap();
        assert!(yes.windows(4).any(|w| w == b"\x43YES"));
        let no =
            media_information_payload("11:22:33:44:55:66", "AA:BB:CC:DD:EE:FF", false).unwrap();
        assert!(no.windows(3).any(|w| w == b"\x42NO"));
    }

    #[test]
    fn mac_to_wire_reverses_the_bytes() {
        assert_eq!(
            mac_to_wire("AA:BB:CC:DD:EE:0f").unwrap(),
            [0x0F, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA]
        );
    }

    #[test]
    fn mac_to_wire_rejects_malformed_addresses() {
        for mac in [
            "",
            "AA:BB:CC:DD:EE",
            "AA:BB:CC:DD:EE:FF:00",
            "AA:BB:CC:DD:EE:GG",
            "AABBCCDDEEFF",
            "AA:BB:CC:DD:EE:FFF",
        ] {
            assert!(mac_to_wire(mac).is_err(), "accepted {mac:?}");
        }
    }

    #[test]
    fn rename_payload_carries_the_name_length() {
        assert_eq!(
            rename_payload("Pods").unwrap(),
            [
                opcodes::RENAME,
                0x00,
                0x01,
                0x04,
                0x00,
                b'P',
                b'o',
                b'd',
                b's'
            ]
        );
        let longest = "a".repeat(255);
        assert_eq!(rename_payload(&longest).unwrap()[3], 255);
    }

    #[test]
    fn rename_payload_rejects_names_over_255_bytes() {
        assert!(rename_payload(&"a".repeat(256)).is_err());
        // 128 two-byte characters: 128 chars but 256 bytes.
        assert!(rename_payload(&"é".repeat(128)).is_err());
    }

    const AIRPODS_MAC: &str = "AA:BB:CC:DD:EE:FF";

    /// Sample information packet from AirPods Pro 2, from the AAP definitions.
    const INFORMATION_SAMPLE: &str = "040004001d0002d5000400416972506f64732050726f004133303438004170706c6520496e632e0051584e524848595850360036312e313836383034303030323030303030302e323731330036312e313836383034303030323030303030302e3237313300312e302e3000636f6d2e6170706c652e6163636573736f72792e757064617465722e6170702e3731004859394c5432454632364a59004833504c5748444a32364b3000363335373533360089312a6567a5400f84a3ca234947efd40b90d78436ae5946748d70273e66066a2589300035333935303630363400";

    /// An AACP packet: the header, then `body` starting at the opcode.
    fn packet(body: &[u8]) -> Vec<u8> {
        [HEADER_BYTES.as_slice(), body].concat()
    }

    struct Harness {
        manager: AACPManager,
        store: Arc<MemoryStore>,
        sent: mpsc::Receiver<Vec<u8>>,
        events: mpsc::UnboundedReceiver<AACPEvent>,
    }

    impl Harness {
        async fn with_store(store: MemoryStore) -> Self {
            let store = Arc::new(store);
            let (manager, sent) = connected_manager(store.clone()).await;
            let (event_tx, events) = mpsc::unbounded_channel();
            manager.set_event_channel(event_tx).await;
            manager.state.lock().await.airpods_mac = Some(AIRPODS_MAC.parse().unwrap());
            Self {
                manager,
                store,
                sent,
                events,
            }
        }

        async fn new() -> Self {
            Self::with_store(MemoryStore::default()).await
        }

        async fn receive(&self, body: &[u8]) {
            self.manager.receive_packet(&packet(body)).await;
        }

        fn next_event(&mut self) -> AACPEvent {
            self.events.try_recv().expect("an event was emitted")
        }

        fn assert_no_event(&mut self) {
            if let Ok(event) = self.events.try_recv() {
                panic!("unexpected event {event:?}");
            }
        }

        fn next_sent(&mut self) -> Vec<u8> {
            self.sent.try_recv().expect("a packet was sent")
        }
    }

    fn airpods_record(le_keys: AirPodsLEKeys) -> DeviceData {
        DeviceData {
            name: "Old name".to_string(),
            type_: DeviceType::AirPods,
            information: Some(DeviceInformation::AirPods(AirPodsInformation {
                le_keys,
                ..AirPodsInformation::default()
            })),
        }
    }

    // Battery (0x04)

    #[tokio::test]
    async fn battery_sample_from_airpods_pro_2_is_stored_and_emitted() {
        let mut h = Harness::new().await;
        let expected = vec![
            BatteryInfo {
                component: BatteryComponent::Right,
                level: 100,
                status: BatteryStatus::NotCharging,
            },
            BatteryInfo {
                component: BatteryComponent::Left,
                level: 99,
                status: BatteryStatus::Charging,
            },
            BatteryInfo {
                component: BatteryComponent::Case,
                level: 17,
                status: BatteryStatus::NotCharging,
            },
        ];

        h.receive(&[
            0x04, 0x00, 0x03, 0x02, 0x01, 0x64, 0x02, 0x01, 0x04, 0x01, 0x63, 0x01, 0x01, 0x08,
            0x01, 0x11, 0x02, 0x01,
        ])
        .await;

        assert_eq!(h.manager.state.lock().await.battery_info, expected);
        assert!(matches!(h.next_event(), AACPEvent::BatteryInfo(b) if b == expected));
        assert!(h.manager.has_device_status().await);
    }

    #[tokio::test]
    async fn battery_status_0x05_is_optimized_charging_and_counts_as_charging() {
        let mut h = Harness::new().await;

        h.receive(&[0x04, 0x00, 0x01, 0x04, 0x01, 0x50, 0x05, 0x01])
            .await;

        let AACPEvent::BatteryInfo(batteries) = h.next_event() else {
            panic!("expected a battery event");
        };
        assert_eq!(batteries.len(), 1);
        assert_eq!(batteries[0].status, BatteryStatus::OptimizedCharging);
        assert!(batteries[0].status.is_charging());
        assert_eq!(batteries[0].level, 80);
    }

    #[tokio::test]
    async fn battery_entries_with_unknown_component_or_status_are_skipped() {
        let mut h = Harness::new().await;

        h.receive(&[
            0x04, 0x00, 0x03, 0x10, 0x01, 0x32, 0x01, 0x01, 0x02, 0x01, 0x33, 0x00, 0x01, 0x08,
            0x01, 0x34, 0x04, 0x01,
        ])
        .await;

        assert_eq!(
            h.manager.state.lock().await.battery_info,
            [BatteryInfo {
                component: BatteryComponent::Case,
                level: 0x34,
                status: BatteryStatus::Disconnected,
            }]
        );
        assert!(matches!(h.next_event(), AACPEvent::BatteryInfo(_)));
    }

    #[tokio::test]
    async fn battery_packet_shorter_than_its_count_is_dropped() {
        let mut h = Harness::new().await;

        h.receive(&[0x04, 0x00, 0x02, 0x02, 0x01, 0x64, 0x02, 0x01])
            .await;
        h.receive(&[0x04, 0x00]).await;

        assert!(h.manager.state.lock().await.battery_info.is_empty());
        h.assert_no_event();
    }

    // Ear detection (0x06)

    #[tokio::test]
    async fn ear_detection_reports_the_previous_and_new_status() {
        let mut h = Harness::new().await;

        h.receive(&[0x06, 0x00, 0x00, 0x01]).await;
        h.receive(&[0x06, 0x00, 0x02, 0x03]).await;

        let AACPEvent::EarDetection(old, new) = h.next_event() else {
            panic!("expected an ear detection event");
        };
        assert!(old.is_empty());
        assert_eq!(
            new,
            [EarDetectionStatus::InEar, EarDetectionStatus::OutOfEar]
        );
        let AACPEvent::EarDetection(old, new) = h.next_event() else {
            panic!("expected an ear detection event");
        };
        assert_eq!(
            old,
            [EarDetectionStatus::InEar, EarDetectionStatus::OutOfEar]
        );
        assert_eq!(
            new,
            [EarDetectionStatus::InCase, EarDetectionStatus::Disconnected]
        );
        let state = h.manager.state.lock().await;
        assert_eq!(state.old_ear_detection_status, old);
        assert_eq!(state.ear_detection_status, new);
    }

    #[tokio::test]
    async fn unknown_ear_detection_status_reads_as_out_of_ear() {
        let mut h = Harness::new().await;

        h.receive(&[0x06, 0x00, 0x07, 0x00]).await;

        let AACPEvent::EarDetection(_, new) = h.next_event() else {
            panic!("expected an ear detection event");
        };
        assert_eq!(
            new,
            [EarDetectionStatus::OutOfEar, EarDetectionStatus::InEar]
        );
    }

    #[tokio::test]
    async fn short_ear_detection_packet_is_dropped() {
        let mut h = Harness::new().await;

        h.receive(&[0x06, 0x00, 0x00]).await;

        h.assert_no_event();
        assert!(!h.manager.has_device_status().await);
    }

    // Control commands (0x09)

    #[tokio::test]
    async fn listening_mode_is_stored_emitted_and_sent_to_subscribers() {
        let mut h = Harness::new().await;
        let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
        h.manager
            .subscribe_to_control_command(ControlCommandIdentifiers::ListeningMode, sub_tx)
            .await;

        // Noise control notification: 09 00 0D [mode] 00 00 00, 03 is transparency.
        h.receive(&[0x09, 0x00, 0x0D, 0x03, 0x00, 0x00, 0x00]).await;

        let expected = ControlCommandStatus {
            identifier: ControlCommandIdentifiers::ListeningMode,
            value: vec![0x03],
        };
        assert_eq!(
            h.manager.state.lock().await.control_command_status_list,
            std::slice::from_ref(&expected)
        );
        assert!(matches!(h.next_event(), AACPEvent::ControlCommand(s) if s == expected));
        assert_eq!(sub_rx.try_recv().unwrap(), [0x03]);
    }

    #[tokio::test]
    async fn control_command_value_trims_trailing_zeros_only() {
        let h = Harness::new().await;

        h.receive(&[0x09, 0x00, 0x1A, 0x03, 0x00, 0x01, 0x00]).await;
        h.receive(&[0x09, 0x00, 0x1F, 0x00, 0x00, 0x00, 0x00]).await;

        let state = h.manager.state.lock().await;
        let value = |id| {
            state
                .control_command_status_list
                .iter()
                .find(|s| s.identifier == id)
                .map(|s| s.value.clone())
        };
        assert_eq!(
            value(ControlCommandIdentifiers::ListeningModeConfigs),
            Some(vec![0x03, 0x00, 0x01])
        );
        assert_eq!(
            value(ControlCommandIdentifiers::ChimeVolume),
            Some(vec![0x00])
        );
    }

    #[tokio::test]
    async fn control_command_update_replaces_the_stored_value() {
        let h = Harness::new().await;

        h.receive(&[0x09, 0x00, 0x28, 0x01, 0x00, 0x00, 0x00]).await;
        assert!(h.manager.conversation_detection_enabled().await);
        h.receive(&[0x09, 0x00, 0x28, 0x02, 0x00, 0x00, 0x00]).await;

        assert!(!h.manager.conversation_detection_enabled().await);
        assert_eq!(
            h.manager
                .state
                .lock()
                .await
                .control_command_status_list
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn owns_connection_command_sets_ownership() {
        let h = Harness::new().await;

        h.receive(&[0x09, 0x00, 0x06, 0x01, 0x00, 0x00, 0x00]).await;
        assert!(h.manager.state.lock().await.owns);
        h.receive(&[0x09, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00]).await;

        assert!(!h.manager.state.lock().await.owns);
    }

    #[tokio::test]
    async fn unknown_or_short_control_commands_are_ignored() {
        let mut h = Harness::new().await;

        h.receive(&[0x09, 0x00, 0x7F, 0x01, 0x00, 0x00, 0x00]).await;
        h.receive(&[0x09, 0x00, 0x0D, 0x03, 0x00, 0x00]).await;

        h.assert_no_event();
        assert!(
            h.manager
                .state
                .lock()
                .await
                .control_command_status_list
                .is_empty()
        );
    }

    // Conversation awareness (0x4B)

    #[tokio::test]
    async fn conversation_awareness_level_is_stored_and_emitted() {
        let mut h = Harness::new().await;

        h.receive(&[0x4B, 0x00, 0x02, 0x00, 0x01, 0x03]).await;

        assert_eq!(
            h.manager.state.lock().await.conversational_awareness_status,
            0x03
        );
        assert!(matches!(
            h.next_event(),
            AACPEvent::ConversationalAwareness(0x03)
        ));
    }

    #[tokio::test]
    async fn conversation_awareness_of_another_length_is_ignored() {
        let mut h = Harness::new().await;

        h.receive(&[0x4B, 0x00, 0x02, 0x00, 0x01]).await;
        h.receive(&[0x4B, 0x00, 0x02, 0x00, 0x01, 0x03, 0x00]).await;

        h.assert_no_event();
    }

    // Information (0x1D)

    #[test]
    fn information_sample_is_parsed_field_by_field() {
        let sample = hex::decode(INFORMATION_SAMPLE).unwrap();

        let Some(Incoming::Information(info)) = parse_packet(&sample) else {
            panic!("expected information");
        };

        assert_eq!(
            *info,
            AirPodsInformation {
                name: "AirPods Pro".to_string(),
                model_number: "A3048".to_string(),
                manufacturer: "Apple Inc.".to_string(),
                serial_number: "QXNRHHYXP6".to_string(),
                version1: "61.1868040002000000.2713".to_string(),
                version2: "61.1868040002000000.2713".to_string(),
                hardware_revision: "1.0.0".to_string(),
                updater_identifier: "com.apple.accessory.updater.app.71".to_string(),
                left_serial_number: "HY9LT2EF26JY".to_string(),
                right_serial_number: "H3PLWHDJ26K0".to_string(),
                version3: "6357536".to_string(),
                le_keys: AirPodsLEKeys::default(),
            }
        );
    }

    #[test]
    fn information_skips_a_leading_run_before_the_first_zero() {
        let parsed = parse_information(b"\x1d\x00\x02\x00junk\x00skipped\x00Name\x00A1\x00");

        let info = parsed.unwrap();
        assert_eq!(info.name, "Name");
        assert_eq!(info.model_number, "A1");
        assert_eq!(info.version3, "");
    }

    #[test]
    fn information_without_strings_or_too_short_is_dropped() {
        assert!(parse_information(&[0x1D, 0x00, 0x02, 0x00, 0x00, 0x00]).is_none());
        assert!(parse_information(&[0x1D, 0x00, 0x02, 0x00, 0x00]).is_none());
    }

    #[tokio::test]
    async fn information_updates_the_record_and_keeps_its_le_keys() {
        let keys = AirPodsLEKeys {
            irk: "01".repeat(16),
            enc_key: "02".repeat(16),
        };
        let store = MemoryStore {
            initial: HashMap::from([(AIRPODS_MAC.to_string(), airpods_record(keys.clone()))]),
            ..MemoryStore::default()
        };
        let h = Harness::with_store(store).await;

        h.manager
            .receive_packet(&hex::decode(INFORMATION_SAMPLE).unwrap())
            .await;

        let saved = h.store.saved();
        assert_eq!(saved.len(), 1);
        let (mac, data) = &saved[0];
        assert_eq!(mac, AIRPODS_MAC);
        assert_eq!(data.name, "AirPods Pro");
        let Some(DeviceInformation::AirPods(info)) = &data.information else {
            panic!("expected AirPods information");
        };
        assert_eq!(info.model_number, "A3048");
        assert_eq!(info.le_keys, keys);
        assert_eq!(h.manager.state.lock().await.devices[AIRPODS_MAC], *data);
    }

    #[tokio::test]
    async fn information_for_airpods_without_a_record_is_not_saved() {
        let h = Harness::new().await;

        h.manager
            .receive_packet(&hex::decode(INFORMATION_SAMPLE).unwrap())
            .await;

        assert!(h.store.saved().is_empty());
        assert!(h.manager.state.lock().await.devices.is_empty());
    }

    // Proximity keys (0x31)

    /// Keys response as the Qt client parses it: count 2, then an IRK (type
    /// 0x01) and an encryption key (type 0x04), each with a 16 byte length.
    fn proximity_keys_body() -> Vec<u8> {
        let mut body = vec![0x31, 0x00, 0x02, 0x01, 0x00, 0x10, 0x00];
        body.extend([0x11; 16]);
        body.extend([0x04, 0x00, 0x10, 0x00]);
        body.extend([0x22; 16]);
        body
    }

    #[tokio::test]
    async fn proximity_keys_start_a_record_for_new_airpods() {
        let h = Harness::new().await;

        h.receive(&proximity_keys_body()).await;

        let saved = h.store.saved();
        assert_eq!(saved.len(), 1);
        let (mac, data) = &saved[0];
        assert_eq!(mac, AIRPODS_MAC);
        assert_eq!(data.name, AIRPODS_MAC);
        assert_eq!(data.type_, DeviceType::AirPods);
        let Some(DeviceInformation::AirPods(info)) = &data.information else {
            panic!("expected AirPods information");
        };
        assert_eq!(info.le_keys.irk, "11".repeat(16));
        assert_eq!(info.le_keys.enc_key, "22".repeat(16));
    }

    #[tokio::test]
    async fn proximity_keys_keep_the_rest_of_an_existing_record() {
        let store = MemoryStore {
            initial: HashMap::from([(
                AIRPODS_MAC.to_string(),
                airpods_record(AirPodsLEKeys::default()),
            )]),
            ..MemoryStore::default()
        };
        let h = Harness::with_store(store).await;

        h.receive(&proximity_keys_body()).await;

        let (_, data) = &h.store.saved()[0];
        assert_eq!(data.name, "Old name");
    }

    #[tokio::test]
    async fn proximity_keys_without_a_connected_mac_are_not_saved() {
        let h = Harness::new().await;
        h.manager.state.lock().await.airpods_mac = None;

        h.receive(&proximity_keys_body()).await;

        assert!(h.store.saved().is_empty());
    }

    #[test]
    fn proximity_keys_running_past_the_end_are_dropped() {
        let body = proximity_keys_body();

        assert!(parse_proximity_keys(&body[..body.len() - 1]).is_none());
        // The second key's header cut short.
        assert!(parse_proximity_keys(&body[..3 + 4 + 16 + 3]).is_none());
        assert!(parse_proximity_keys(&[0x31, 0x00, 0x01]).is_none());
        assert_eq!(parse_proximity_keys(&body).map(|k| k.len()), Some(2));
    }

    // Stem press (0x19)

    #[tokio::test]
    async fn stem_press_emits_the_press_and_the_bud() {
        let mut h = Harness::new().await;

        h.receive(&[0x19, 0x00, 0x06, 0x02]).await;

        assert!(matches!(
            h.next_event(),
            AACPEvent::StemPress(StemPressType::Double, StemPressBudType::Right)
        ));
    }

    #[tokio::test]
    async fn stem_press_with_unknown_type_or_bud_is_ignored() {
        let mut h = Harness::new().await;

        h.receive(&[0x19, 0x00, 0x09, 0x01]).await;
        h.receive(&[0x19, 0x00, 0x05, 0x03]).await;
        h.receive(&[0x19, 0x00, 0x05]).await;

        h.assert_no_event();
    }

    // Audio source (0x0E)

    #[tokio::test]
    async fn audio_source_mac_is_read_least_significant_byte_first() {
        let h = Harness::new().await;

        h.receive(&[0x0E, 0x00, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x02])
            .await;
        let unknown_type = parse_audio_source(&[0x0E, 0x00, 1, 2, 3, 4, 5, 6, 0x09]);

        assert_eq!(
            h.manager.state.lock().await.audio_source,
            Some(AudioSource {
                mac: "11:22:33:44:55:66".to_string(),
                r#type: AudioSourceType::Media,
            })
        );
        assert_eq!(unknown_type.unwrap().r#type, AudioSourceType::None);
    }

    // Connected devices (0x2E)

    #[tokio::test]
    async fn connected_devices_replace_the_list_and_report_the_old_one() {
        let mut h = Harness::new().await;
        let first = ConnectedDevice {
            mac: "11:22:33:44:55:66".to_string(),
            info1: 0x01,
            info2: 0x02,
            r#type: None,
        };
        let second = ConnectedDevice {
            mac: "A1:B2:C3:D4:E5:F6".to_string(),
            info1: 0x03,
            info2: 0x04,
            r#type: None,
        };

        h.receive(&[
            0x2E, 0x00, 0x01, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x01, 0x02,
        ])
        .await;
        h.receive(&[
            0x2E, 0x00, 0x02, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x01, 0x02, 0xA1,
            0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x03, 0x04,
        ])
        .await;

        let _ = h.next_event();
        let AACPEvent::ConnectedDevices(old, new) = h.next_event() else {
            panic!("expected a connected devices event");
        };
        assert_eq!(old, std::slice::from_ref(&first));
        assert_eq!(new, [first, second]);
        assert_eq!(h.manager.state.lock().await.connected_devices, new);
    }

    #[tokio::test]
    async fn connected_devices_shorter_than_their_count_are_dropped() {
        let mut h = Harness::new().await;

        h.receive(&[
            0x2E, 0x00, 0x02, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x01, 0x02,
        ])
        .await;

        h.assert_no_event();
    }

    // Smart routing response (0x11)

    #[tokio::test]
    async fn smart_routing_ownership_to_false_is_emitted() {
        let mut h = Harness::new().await;
        let mut body = vec![0x11, 0x00];
        body.extend_from_slice(b"\x01\x02audioRoutingSetOwnershipToFalse\x01");

        h.receive(&body).await;
        h.receive(b"\x11\x00otherKey").await;

        assert!(matches!(h.next_event(), AACPEvent::OwnershipToFalseRequest));
        h.assert_no_event();
    }

    // Custom EQ (0x63)

    #[tokio::test]
    async fn custom_eq_is_stored_and_emitted() {
        let mut h = Harness::new().await;
        let expected = CustomEq {
            enabled: true,
            low: 65,
            mid: 50,
            high: 70,
        };

        h.receive(&[0x63, 0x00, 0x05, 0x00, 0x01, 0x02, 0x41, 0x32, 0x46])
            .await;

        assert_eq!(h.manager.state.lock().await.custom_eq, Some(expected));
        assert!(matches!(h.next_event(), AACPEvent::CustomEq(eq) if eq == expected));
    }

    #[tokio::test]
    async fn malformed_custom_eq_is_ignored() {
        let mut h = Harness::new().await;

        h.receive(&[0x63, 0x00, 0x05, 0x00, 0x01, 0x02, 101, 50, 50])
            .await;

        assert_eq!(h.manager.state.lock().await.custom_eq, None);
        h.assert_no_event();
    }

    // Framing

    #[tokio::test]
    async fn packets_without_header_or_opcode_and_unknown_opcodes_are_ignored() {
        let mut h = Harness::new().await;

        h.manager
            .receive_packet(&[0x05, 0x00, 0x04, 0x00, 0x04, 0x00, 0x01])
            .await;
        h.manager.receive_packet(&HEADER_BYTES).await;
        h.manager.receive_packet(&[]).await;
        h.receive(&[0x53, 0x00, 0x01]).await;
        h.receive(&[0x7E, 0x00]).await;

        h.assert_no_event();
    }

    #[tokio::test]
    async fn reset_connection_forgets_what_the_connection_reported() {
        let h = Harness::new().await;
        h.receive(&[0x04, 0x00, 0x01, 0x02, 0x01, 0x64, 0x02, 0x01])
            .await;
        h.receive(&[0x06, 0x00, 0x00, 0x00]).await;
        h.receive(&[0x09, 0x00, 0x06, 0x01, 0x00, 0x00, 0x00]).await;

        h.manager.state.lock().await.reset_connection();

        let state = h.manager.state.lock().await;
        assert!(state.sender.is_none());
        assert!(!state.owns);
        assert!(state.battery_info.is_empty());
        assert!(state.ear_detection_status.is_empty());
        assert!(state.control_command_status_list.is_empty());
        drop(state);
        assert!(!h.manager.has_device_status().await);
    }

    const KNOWN_OPCODES: [u8; 12] = [
        opcodes::BATTERY_INFO,
        opcodes::CONTROL_COMMAND,
        opcodes::EAR_DETECTION,
        opcodes::CONVERSATION_AWARENESS,
        opcodes::INFORMATION,
        opcodes::PROXIMITY_KEYS_RSP,
        opcodes::STEM_PRESS,
        opcodes::AUDIO_SOURCE,
        opcodes::CONNECTED_DEVICES,
        opcodes::SMART_ROUTING_RESP,
        opcodes::EQ_DATA,
        opcodes::CUSTOM_EQ,
    ];

    /// Run `packet` through `receive_packet` on a fresh manager.
    fn receive_on_fresh_manager(packet: &[u8]) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let h = Harness::new().await;
                h.manager.receive_packet(packet).await;
            });
    }

    proptest! {
        #[test]
        fn receive_packet_never_panics_on_arbitrary_bytes(
            bytes in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            receive_on_fresh_manager(&bytes);
        }

        #[test]
        fn receive_packet_never_panics_on_known_opcodes_with_any_body(
            opcode in proptest::sample::select(KNOWN_OPCODES.as_slice()),
            // Small counts and lengths so the count and length fields often
            // point just inside or just past the end.
            body in proptest::collection::vec(
                prop_oneof![0u8..=4, any::<u8>()],
                0..300,
            ),
        ) {
            receive_on_fresh_manager(&packet(&[&[opcode][..], &body].concat()));
        }
    }

    // Commands sent to the AirPods

    #[tokio::test]
    async fn control_command_is_padded_to_four_value_bytes() {
        let mut h = Harness::new().await;

        h.manager
            .send_control_command(ControlCommandIdentifiers::ListeningMode, &[0x02])
            .await
            .unwrap();
        h.manager
            .send_control_command(
                ControlCommandIdentifiers::ClickHoldMode,
                &[0x01, 0x05, 0x07, 0x08, 0x09],
            )
            .await
            .unwrap();

        assert_eq!(
            h.next_sent(),
            [
                0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x0D, 0x02, 0x00, 0x00, 0x00
            ]
        );
        assert_eq!(
            h.next_sent(),
            [
                0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x16, 0x01, 0x05, 0x07, 0x08
            ]
        );
    }

    #[tokio::test]
    async fn rename_sends_the_length_prefixed_name() {
        let mut h = Harness::new().await;

        h.manager.send_rename_packet("Pods").await.unwrap();

        assert_eq!(h.next_sent(), packet(b"\x1a\x00\x01\x04\x00Pods"));
    }

    #[tokio::test]
    async fn rename_with_a_long_name_fails_before_sending() {
        let mut h = Harness::new().await;

        let result = h.manager.send_rename_packet(&"a".repeat(256)).await;

        assert!(matches!(result, Err(AacpError::NameTooLong(256))));
        assert!(h.sent.try_recv().is_err());
    }

    #[tokio::test]
    async fn setup_requests_match_the_captured_packets() {
        let mut h = Harness::new().await;

        h.manager.send_notification_request().await.unwrap();
        h.manager
            .send_proximity_keys_request(vec![ProximityKeyType::Irk, ProximityKeyType::EncKey])
            .await
            .unwrap();
        h.manager.send_handshake().await.unwrap();

        assert_eq!(
            h.next_sent(),
            [0x04, 0x00, 0x04, 0x00, 0x0F, 0x00, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(
            h.next_sent(),
            [0x04, 0x00, 0x04, 0x00, 0x30, 0x00, 0x05, 0x00]
        );
        assert_eq!(
            h.next_sent(),
            [
                0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00
            ]
        );
    }

    #[tokio::test]
    async fn hi_res_audio_start_and_stop_are_sent_as_is() {
        let mut h = Harness::new().await;

        h.manager.send_start_audio().await.unwrap();
        h.manager.send_stop_audio().await.unwrap();

        assert_eq!(h.next_sent(), AACP_START_AUDIO);
        assert_eq!(h.next_sent(), AACP_STOP_AUDIO);
    }

    #[tokio::test]
    async fn media_information_is_sent_with_the_header() {
        let mut h = Harness::new().await;

        h.manager
            .send_media_information("11:22:33:44:55:66", AIRPODS_MAC, true)
            .await
            .unwrap();

        let expected = media_information_payload("11:22:33:44:55:66", AIRPODS_MAC, true).unwrap();
        assert_eq!(h.next_sent(), packet(&expected));
    }

    #[tokio::test]
    async fn hijack_request_length_counts_the_body_before_the_padding() {
        let mut h = Harness::new().await;

        h.manager.send_hijack_request(AIRPODS_MAC).await.unwrap();

        let sent = h.next_sent();
        assert_eq!(sent.len(), 4 + 2 + 106);
        assert_eq!(sent[4..6], [opcodes::SMART_ROUTING, 0x00]);
        assert_eq!(sent[6..12], [0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA]);
        let len = usize::from(u16::from_le_bytes([sent[12], sent[13]]));
        let body = &sent[14..14 + len];
        assert!(body.ends_with(b"remotescore\xa5"));
        assert!(sent[14 + len..].iter().all(|&b| b == 0));
    }

    #[tokio::test]
    async fn smart_routing_with_a_malformed_mac_fails_before_sending() {
        let mut h = Harness::new().await;

        let result = h.manager.send_hijack_request("not a mac").await;

        assert!(matches!(result, Err(AacpError::InvalidMac(mac)) if mac == "not a mac"));
        assert!(h.sent.try_recv().is_err());
    }

    #[tokio::test]
    async fn send_without_a_connection_fails_with_not_connected() {
        let manager = AACPManager::with_deps(Arc::new(MemoryStore::default()), false);

        let result = manager.send_notification_request().await;

        assert!(matches!(result, Err(AacpError::NotConnected)));
    }

    #[tokio::test]
    async fn send_after_the_socket_task_ended_fails_with_channel_closed() {
        let Harness { manager, sent, .. } = Harness::new().await;
        drop(sent);

        let result = manager.send_notification_request().await;

        assert!(matches!(result, Err(AacpError::SendChannelClosed)));
    }

    #[tokio::test]
    async fn set_conversation_detection_sends_and_records_the_setting() {
        let mut h = Harness::new().await;
        let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
        h.manager
            .subscribe_to_control_command(
                ControlCommandIdentifiers::ConversationDetectConfig,
                sub_tx,
            )
            .await;

        h.manager.set_conversation_detection(true).await;

        assert_eq!(
            h.next_sent(),
            [
                0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x28, 0x01, 0x00, 0x00, 0x00
            ]
        );
        assert!(h.manager.conversation_detection_enabled().await);
        assert_eq!(sub_rx.try_recv().unwrap(), [0x01]);
        assert!(matches!(h.next_event(), AACPEvent::ControlCommand(_)));
    }

    #[tokio::test]
    async fn subscriber_gets_the_current_value_first() {
        let h = Harness::new().await;
        h.receive(&[0x09, 0x00, 0x34, 0x01, 0x00, 0x00, 0x00]).await;
        let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();

        h.manager
            .subscribe_to_control_command(ControlCommandIdentifiers::AllowOffOption, sub_tx)
            .await;

        assert_eq!(sub_rx.try_recv().unwrap(), [0x01]);
    }
}
