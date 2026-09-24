//! Typed values for the AACP control commands behind the AirPods settings page.
//!
//! Encodings follow the LibrePods Android app, which talks to the AirPods without
//! changing the host's Bluetooth DeviceID. `from_value` is lenient: anything it does
//! not recognise returns `None`, so a new firmware value never takes the UI down.
//!
//! Values reported by the AirPods arrive with trailing zero bytes trimmed (see
//! `AACPManager::receive_packet`), and outgoing values are zero padded to four bytes
//! by `send_control_command`, so decoders treat missing trailing bytes as zero.

/// A value carried by one control command.
pub trait ControlValue: Sized {
    fn to_value(self) -> Vec<u8>;
    fn from_value(value: &[u8]) -> Option<Self>;
}

/// Reads the first `N` bytes of a control command value, padding trimmed trailing
/// bytes with zero. Empty values and values with non-zero bytes past `N` are rejected.
fn fixed<const N: usize>(value: &[u8]) -> Option<[u8; N]> {
    if value.is_empty() || value.iter().skip(N).any(|&b| b != 0) {
        return None;
    }
    let mut out = [0u8; N];
    for (dst, src) in out.iter_mut().zip(value) {
        *dst = *src;
    }
    Some(out)
}

/// On/off settings: 0x01 is on, 0x02 is off. Used for OneBudAncMode (0x1B),
/// VolumeSwipeMode (0x25) and SleepDetectionConfig (0x35).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toggle(pub bool);

impl ControlValue for Toggle {
    fn to_value(self) -> Vec<u8> {
        vec![if self.0 { 0x01 } else { 0x02 }]
    }

    fn from_value(value: &[u8]) -> Option<Self> {
        match fixed::<1>(value)? {
            [0x01] => Some(Toggle(true)),
            [0x02] => Some(Toggle(false)),
            _ => None,
        }
    }
}

/// Which bud's microphone is used (MicMode, 0x01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicMode {
    Automatic,
    AlwaysRight,
    AlwaysLeft,
}

impl MicMode {
    pub const ALL: [MicMode; 3] = [
        MicMode::Automatic,
        MicMode::AlwaysRight,
        MicMode::AlwaysLeft,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MicMode::Automatic => "Automatic",
            MicMode::AlwaysRight => "Always Right",
            MicMode::AlwaysLeft => "Always Left",
        }
    }
}

impl ControlValue for MicMode {
    fn to_value(self) -> Vec<u8> {
        vec![match self {
            MicMode::Automatic => 0x00,
            MicMode::AlwaysRight => 0x01,
            MicMode::AlwaysLeft => 0x02,
        }]
    }

    fn from_value(value: &[u8]) -> Option<Self> {
        match fixed::<1>(value)? {
            [0x00] => Some(MicMode::Automatic),
            [0x01] => Some(MicMode::AlwaysRight),
            [0x02] => Some(MicMode::AlwaysLeft),
            _ => None,
        }
    }
}

/// What a long press on one stem does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickHoldAction {
    NoiseControl,
    Siri,
}

impl ClickHoldAction {
    pub const ALL: [ClickHoldAction; 2] = [ClickHoldAction::NoiseControl, ClickHoldAction::Siri];

    pub fn label(self) -> &'static str {
        match self {
            ClickHoldAction::NoiseControl => "Listening Mode",
            ClickHoldAction::Siri => "Siri",
        }
    }

    fn to_byte(self) -> u8 {
        match self {
            ClickHoldAction::NoiseControl => 0x01,
            ClickHoldAction::Siri => 0x05,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x01 => Some(ClickHoldAction::NoiseControl),
            0x05 => Some(ClickHoldAction::Siri),
            _ => None,
        }
    }
}

/// Long press action per bud (ClickHoldMode, 0x16): first byte right, second byte left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickHoldMode {
    pub right: ClickHoldAction,
    pub left: ClickHoldAction,
}

impl ControlValue for ClickHoldMode {
    fn to_value(self) -> Vec<u8> {
        vec![self.right.to_byte(), self.left.to_byte()]
    }

    fn from_value(value: &[u8]) -> Option<Self> {
        let [right, left] = fixed::<2>(value)?;
        Some(ClickHoldMode {
            right: ClickHoldAction::from_byte(right)?,
            left: ClickHoldAction::from_byte(left)?,
        })
    }
}

/// One listening mode in the press and hold cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleMode {
    Off,
    NoiseCancellation,
    Transparency,
    Adaptive,
}

impl CycleMode {
    /// Display order used by the Android app.
    pub const ALL: [CycleMode; 4] = [
        CycleMode::Off,
        CycleMode::Transparency,
        CycleMode::Adaptive,
        CycleMode::NoiseCancellation,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CycleMode::Off => "Off",
            CycleMode::NoiseCancellation => "Noise Cancellation",
            CycleMode::Transparency => "Transparency",
            CycleMode::Adaptive => "Adaptive",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            CycleMode::Off => "Turns off noise management",
            CycleMode::NoiseCancellation => "Blocks out external sounds",
            CycleMode::Transparency => "Lets in external sounds",
            CycleMode::Adaptive => "Dynamically adjust external noise",
        }
    }

    fn bit(self) -> u8 {
        match self {
            CycleMode::Off => 0x01,
            CycleMode::NoiseCancellation => 0x02,
            CycleMode::Transparency => 0x04,
            CycleMode::Adaptive => 0x08,
        }
    }
}

/// Listening modes a long press cycles through (ListeningModeConfigs, 0x1A), as a bitmask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListeningModeCycle(u8);

impl ListeningModeCycle {
    /// The AirPods need at least two modes to cycle between.
    pub const MIN_MODES: u32 = 2;

    pub fn contains(self, mode: CycleMode) -> bool {
        self.0 & mode.bit() != 0
    }

    /// Adds or removes `mode`. Removing is refused when it would leave fewer than
    /// `MIN_MODES`, matching the Android app.
    pub fn toggled(self, mode: CycleMode) -> Option<Self> {
        let next = ListeningModeCycle(self.0 ^ mode.bit());
        if self.contains(mode) && next.0.count_ones() < Self::MIN_MODES {
            return None;
        }
        Some(next)
    }
}

impl ControlValue for ListeningModeCycle {
    fn to_value(self) -> Vec<u8> {
        vec![self.0]
    }

    fn from_value(value: &[u8]) -> Option<Self> {
        let [bits] = fixed::<1>(value)?;
        (bits & !0x0F == 0).then_some(ListeningModeCycle(bits))
    }
}

/// Which stem press mutes and which hangs up during a call (CallManagementConfig, 0x24).
/// Answering is always a single press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallControls {
    /// Press once to mute or unmute, press twice to hang up.
    PressOnceToMute,
    /// Press twice to mute or unmute, press once to hang up.
    PressTwiceToMute,
}

impl CallControls {
    pub const ALL: [CallControls; 2] = [
        CallControls::PressOnceToMute,
        CallControls::PressTwiceToMute,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CallControls::PressOnceToMute => "Press once to mute, press twice to hang up",
            CallControls::PressTwiceToMute => "Press twice to mute, press once to hang up",
        }
    }
}

impl ControlValue for CallControls {
    fn to_value(self) -> Vec<u8> {
        match self {
            CallControls::PressOnceToMute => vec![0x00, 0x03],
            CallControls::PressTwiceToMute => vec![0x00, 0x02],
        }
    }

    fn from_value(value: &[u8]) -> Option<Self> {
        match fixed::<2>(value)? {
            [0x00, 0x03] => Some(CallControls::PressOnceToMute),
            [0x00, 0x02] => Some(CallControls::PressTwiceToMute),
            _ => None,
        }
    }
}

/// How slow a press may be: press speed (DoubleClickInterval, 0x17) and press and
/// hold duration (ClickHoldInterval, 0x18) share this encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressInterval {
    Default,
    Slower,
    Slowest,
}

impl PressInterval {
    pub const ALL: [PressInterval; 3] = [
        PressInterval::Default,
        PressInterval::Slower,
        PressInterval::Slowest,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PressInterval::Default => "Default",
            PressInterval::Slower => "Slower",
            PressInterval::Slowest => "Slowest",
        }
    }
}

impl ControlValue for PressInterval {
    fn to_value(self) -> Vec<u8> {
        vec![match self {
            PressInterval::Default => 0x00,
            PressInterval::Slower => 0x01,
            PressInterval::Slowest => 0x02,
        }]
    }

    fn from_value(value: &[u8]) -> Option<Self> {
        match fixed::<1>(value)? {
            [0x00] => Some(PressInterval::Default),
            [0x01] => Some(PressInterval::Slower),
            [0x02] => Some(PressInterval::Slowest),
            _ => None,
        }
    }
}

/// Wait time between volume swipes (VolumeSwipeInterval, 0x23). Unlike the press
/// intervals this one starts at 0x01.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeInterval {
    Default,
    Longer,
    Longest,
}

impl SwipeInterval {
    pub const ALL: [SwipeInterval; 3] = [
        SwipeInterval::Default,
        SwipeInterval::Longer,
        SwipeInterval::Longest,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SwipeInterval::Default => "Default",
            SwipeInterval::Longer => "Longer",
            SwipeInterval::Longest => "Longest",
        }
    }
}

impl ControlValue for SwipeInterval {
    fn to_value(self) -> Vec<u8> {
        vec![match self {
            SwipeInterval::Default => 0x01,
            SwipeInterval::Longer => 0x02,
            SwipeInterval::Longest => 0x03,
        }]
    }

    fn from_value(value: &[u8]) -> Option<Self> {
        match fixed::<1>(value)? {
            [0x01] => Some(SwipeInterval::Default),
            [0x02] => Some(SwipeInterval::Longer),
            [0x03] => Some(SwipeInterval::Longest),
            _ => None,
        }
    }
}

/// Upper bound of the percentage settings below.
pub const PERCENT_MAX: u8 = 100;

/// Volume of the AirPods' own sound effects, 0 to 100 (ChimeVolume, 0x1F).
///
/// The Android app always sends 0x50 as the second byte; its meaning is not known, so
/// it is sent the same way and ignored when decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChimeVolume(u8);

impl ChimeVolume {
    const SECOND_BYTE: u8 = 0x50;

    pub fn new(percent: u8) -> Self {
        ChimeVolume(percent.min(PERCENT_MAX))
    }

    pub fn percent(self) -> u8 {
        self.0
    }
}

impl ControlValue for ChimeVolume {
    fn to_value(self) -> Vec<u8> {
        vec![self.0, Self::SECOND_BYTE]
    }

    fn from_value(value: &[u8]) -> Option<Self> {
        let [percent, _] = fixed::<2>(value)?;
        (percent <= PERCENT_MAX).then_some(ChimeVolume(percent))
    }
}

/// Adaptive Audio strength, 0 to 100 (AutoAncStrength, 0x2E). Higher values cancel
/// more noise; the Android slider shows `100 - value` so that moving right lets more
/// noise in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveStrength(u8);

impl AdaptiveStrength {
    pub fn new(strength: u8) -> Self {
        AdaptiveStrength(strength.min(PERCENT_MAX))
    }

    pub fn strength(self) -> u8 {
        self.0
    }
}

impl ControlValue for AdaptiveStrength {
    fn to_value(self) -> Vec<u8> {
        vec![self.0]
    }

    fn from_value(value: &[u8]) -> Option<Self> {
        let [strength] = fixed::<1>(value)?;
        (strength <= PERCENT_MAX).then_some(AdaptiveStrength(strength))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn round_trip<T: ControlValue + Copy + PartialEq + std::fmt::Debug>(v: T) {
        assert_eq!(T::from_value(&v.to_value()), Some(v));
        // Outgoing values are zero padded to four bytes on the wire.
        let mut padded = v.to_value();
        padded.resize(4, 0);
        assert_eq!(T::from_value(&padded), Some(v));
    }

    /// What the AirPods report after `receive_packet` trims trailing zeros.
    fn trimmed(mut value: Vec<u8>) -> Vec<u8> {
        while value.len() > 1 && value.last() == Some(&0) {
            value.pop();
        }
        value
    }

    #[track_caller]
    fn rejects<T: ControlValue + std::fmt::Debug>(value: &[u8]) {
        assert_eq!(T::from_value(value).map(|_| ()), None, "accepted {value:?}");
    }

    #[test]
    fn fixed_pads_and_rejects() {
        assert_eq!(fixed::<2>(&[0x01]), Some([0x01, 0x00]));
        assert_eq!(fixed::<2>(&[0x01, 0x02, 0x00, 0x00]), Some([0x01, 0x02]));
        assert_eq!(fixed::<2>(&[0x01, 0x02, 0x03]), None);
        assert_eq!(fixed::<1>(&[]), None);
    }

    #[test]
    fn toggle() {
        round_trip(Toggle(true));
        round_trip(Toggle(false));
        assert_eq!(Toggle(true).to_value(), [0x01]);
        assert_eq!(Toggle(false).to_value(), [0x02]);
        rejects::<Toggle>(&[0x00]);
        rejects::<Toggle>(&[0x03]);
        rejects::<Toggle>(&[0x01, 0x01]);
        rejects::<Toggle>(&[]);
    }

    #[test]
    fn mic_mode() {
        for mode in MicMode::ALL {
            round_trip(mode);
        }
        assert_eq!(MicMode::Automatic.to_value(), [0x00]);
        assert_eq!(MicMode::AlwaysRight.to_value(), [0x01]);
        assert_eq!(MicMode::AlwaysLeft.to_value(), [0x02]);
        rejects::<MicMode>(&[0x03]);
        rejects::<MicMode>(&[0x01, 0x01]);
        rejects::<MicMode>(&[]);
    }

    #[test]
    fn click_hold_mode() {
        for right in ClickHoldAction::ALL {
            for left in ClickHoldAction::ALL {
                round_trip(ClickHoldMode { right, left });
            }
        }
        let mode = ClickHoldMode {
            right: ClickHoldAction::Siri,
            left: ClickHoldAction::NoiseControl,
        };
        assert_eq!(mode.to_value(), [0x05, 0x01]);
        rejects::<ClickHoldMode>(&[0x01]);
        rejects::<ClickHoldMode>(&[0x01, 0x02]);
        rejects::<ClickHoldMode>(&[0x01, 0x01, 0x01]);
        rejects::<ClickHoldMode>(&[]);
    }

    #[test]
    fn listening_mode_cycle() {
        for bits in 0..=0x0F {
            round_trip(ListeningModeCycle(bits));
        }
        let all = ListeningModeCycle::from_value(&[0x0F]);
        assert_eq!(all, Some(ListeningModeCycle(0x0F)));
        let reported = ListeningModeCycle::from_value(&[0x06]).map(|c| {
            (
                c.contains(CycleMode::Off),
                c.contains(CycleMode::NoiseCancellation),
                c.contains(CycleMode::Transparency),
                c.contains(CycleMode::Adaptive),
            )
        });
        assert_eq!(reported, Some((false, true, true, false)));
        rejects::<ListeningModeCycle>(&[0x10]);
        rejects::<ListeningModeCycle>(&[0x06, 0x01]);
        rejects::<ListeningModeCycle>(&[]);
    }

    #[test]
    fn listening_mode_cycle_keeps_two_modes() {
        let two = ListeningModeCycle(0x06);
        assert_eq!(two.toggled(CycleMode::Transparency), None);
        assert_eq!(
            two.toggled(CycleMode::Adaptive),
            Some(ListeningModeCycle(0x0E))
        );
        let three = ListeningModeCycle(0x0E);
        assert_eq!(three.toggled(CycleMode::Adaptive), Some(two));
    }

    #[test]
    fn call_controls() {
        for controls in CallControls::ALL {
            round_trip(controls);
            let reported = trimmed(controls.to_value());
            assert_eq!(CallControls::from_value(&reported), Some(controls));
        }
        assert_eq!(CallControls::PressOnceToMute.to_value(), [0x00, 0x03]);
        assert_eq!(CallControls::PressTwiceToMute.to_value(), [0x00, 0x02]);
        rejects::<CallControls>(&[0x00]);
        rejects::<CallControls>(&[0x01, 0x03]);
        rejects::<CallControls>(&[0x00, 0x04]);
        rejects::<CallControls>(&[]);
    }

    #[test]
    fn press_interval() {
        for interval in PressInterval::ALL {
            round_trip(interval);
        }
        assert_eq!(PressInterval::Default.to_value(), [0x00]);
        assert_eq!(PressInterval::Slowest.to_value(), [0x02]);
        rejects::<PressInterval>(&[0x03]);
        rejects::<PressInterval>(&[]);
    }

    #[test]
    fn swipe_interval() {
        for interval in SwipeInterval::ALL {
            round_trip(interval);
        }
        assert_eq!(SwipeInterval::Default.to_value(), [0x01]);
        assert_eq!(SwipeInterval::Longest.to_value(), [0x03]);
        rejects::<SwipeInterval>(&[0x00]);
        rejects::<SwipeInterval>(&[0x04]);
        rejects::<SwipeInterval>(&[]);
    }

    #[test]
    fn chime_volume() {
        for percent in 0..=PERCENT_MAX {
            round_trip(ChimeVolume::new(percent));
        }
        assert_eq!(ChimeVolume::new(75).to_value(), [75, 0x50]);
        assert_eq!(ChimeVolume::new(200).percent(), PERCENT_MAX);
        assert_eq!(ChimeVolume::from_value(&[0x46]), Some(ChimeVolume(0x46)));
        assert_eq!(ChimeVolume::from_value(&[0x00, 0x50]), Some(ChimeVolume(0)));
        rejects::<ChimeVolume>(&[101, 0x50]);
        rejects::<ChimeVolume>(&[0x46, 0x50, 0x01]);
        rejects::<ChimeVolume>(&[]);
    }

    #[test]
    fn adaptive_strength() {
        for strength in 0..=PERCENT_MAX {
            round_trip(AdaptiveStrength::new(strength));
        }
        assert_eq!(AdaptiveStrength::new(0x45).to_value(), [0x45]);
        assert_eq!(AdaptiveStrength::new(255).strength(), PERCENT_MAX);
        rejects::<AdaptiveStrength>(&[101]);
        rejects::<AdaptiveStrength>(&[0x45, 0x01]);
        rejects::<AdaptiveStrength>(&[]);
    }
}
