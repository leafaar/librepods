//! The AirPods settings driven by AACP control commands (press and hold,
//! calls, microphone, accessibility, Adaptive Audio, sleep detection): what
//! the page shows and what a change sends, with no GTK in it.
//!
//! A setting shows only once the AirPods reported a value for it, which keeps
//! controls for features a model lacks off the page. A change is stored in
//! `AirPods::control_values` at once, so the page shows it before the AirPods
//! confirm. Slider changes are held until the slider rests for
//! `SETTLE_DELAY`, so a drag sends one command instead of one per step.

use {
    crate::{
        bluetooth::{
            aacp::ControlCommandIdentifiers as Id,
            settings::{
                AdaptiveStrength, CallControls, ChimeVolume, ClickHoldAction, ClickHoldMode,
                ControlValue, CycleMode, ListeningModeCycle, MicMode, PERCENT_MAX, PressInterval,
                SwipeInterval, Toggle,
            },
        },
        ui::gtk::model::{AirPods, Effect},
    },
    std::{collections::HashMap, time::Duration},
};

/// How long a slider must rest before its value is sent.
pub(crate) const SETTLE_DELAY: Duration = Duration::from_millis(300);

/// Slider positions snap to this step.
pub(crate) const SLIDER_STEP: u8 = 5;

/// Tone volume shown when the AirPods report a value that does not decode;
/// the Android app uses the same fallback.
const DEFAULT_TONE_VOLUME: u8 = 75;

/// Adaptive Audio strength shown when the reported value does not decode.
const DEFAULT_ADAPTIVE_STRENGTH: u8 = 50;

/// The long press action of both buds out of the box. A change to one bud
/// keeps this for the other when the current value does not decode.
const DEFAULT_CLICK_HOLD: ClickHoldMode = ClickHoldMode {
    right: ClickHoldAction::NoiseControl,
    left: ClickHoldAction::NoiseControl,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bud {
    Left,
    Right,
}

/// A change made in the settings sections of the AirPods page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlChange {
    LongPress(Bud, ClickHoldAction),
    /// Add (true) or remove (false) a mode of the press and hold cycle.
    CycleMode(CycleMode, bool),
    /// Which press mutes during a call; the other one hangs up.
    MuteControl(CallControls),
    MicMode(MicMode),
    PressSpeed(PressInterval),
    HoldDuration(PressInterval),
    OneBudAnc(bool),
    /// Tone volume slider position, 0 to 100.
    ToneVolume(u8),
    VolumeSwipe(bool),
    SwipeSpeed(SwipeInterval),
    /// Adaptive Audio slider position, from 0 (less noise) to 100 (more
    /// noise), the inverse of the strength the AirPods store.
    AdaptiveNoise(u8),
    SleepDetection(bool),
}

impl ControlChange {
    /// The command and value to send, or None when the change is not
    /// allowed.
    fn command(self, airpods: &AirPods) -> Option<(Id, Vec<u8>)> {
        let toggle = |on| Toggle(on).to_value();
        Some(match self {
            Self::LongPress(bud, action) => {
                let mut mode = airpods
                    .control::<ClickHoldMode>(Id::ClickHoldMode)
                    .unwrap_or(DEFAULT_CLICK_HOLD);
                match bud {
                    Bud::Left => mode.left = action,
                    Bud::Right => mode.right = action,
                }
                (Id::ClickHoldMode, mode.to_value())
            },
            Self::CycleMode(mode, on) => {
                let cycle = airpods.control::<ListeningModeCycle>(Id::ListeningModeConfigs)?;
                if cycle.contains(mode) == on
                    || (mode == CycleMode::Off && on && !airpods.allow_off)
                {
                    return None;
                }
                (Id::ListeningModeConfigs, cycle.toggled(mode)?.to_value())
            },
            Self::MuteControl(controls) => (Id::CallManagementConfig, controls.to_value()),
            Self::MicMode(mode) => (Id::MicMode, mode.to_value()),
            Self::PressSpeed(interval) => (Id::DoubleClickInterval, interval.to_value()),
            Self::HoldDuration(interval) => (Id::ClickHoldInterval, interval.to_value()),
            Self::OneBudAnc(on) => (Id::OneBudAncMode, toggle(on)),
            Self::ToneVolume(position) => (Id::ChimeVolume, ChimeVolume::new(position).to_value()),
            Self::VolumeSwipe(on) => (Id::VolumeSwipeMode, toggle(on)),
            Self::SwipeSpeed(interval) => (Id::VolumeSwipeInterval, interval.to_value()),
            Self::AdaptiveNoise(position) => (
                Id::AutoAncStrength,
                AdaptiveStrength::new(PERCENT_MAX.saturating_sub(position)).to_value(),
            ),
            Self::SleepDetection(on) => (Id::SleepDetectionConfig, toggle(on)),
        })
    }

    /// Slider changes wait for the slider to rest before they are sent.
    fn settles(self) -> bool {
        matches!(self, Self::ToneVolume(_) | Self::AdaptiveNoise(_))
    }
}

/// Slider values waiting for their slider to rest, per device and command.
/// Each hold gets a new generation; only the newest one is sent.
#[derive(Debug, Default)]
pub(crate) struct Settling {
    next: u64,
    pending: HashMap<(String, Id), (u64, Vec<u8>)>,
}

impl Settling {
    fn hold(&mut self, mac: &str, identifier: Id, value: Vec<u8>) -> u64 {
        self.next = self.next.wrapping_add(1);
        self.pending
            .insert((mac.to_string(), identifier), (self.next, value));
        self.next
    }

    /// The value to send when `generation` is still the newest hold.
    fn release(&mut self, mac: &str, identifier: Id, generation: u64) -> Option<Vec<u8>> {
        let key = (mac.to_string(), identifier);
        match self.pending.get(&key) {
            Some((newest, _)) if *newest == generation => {
                self.pending.remove(&key).map(|(_, value)| value)
            },
            _ => None,
        }
    }
}

/// Apply `change` to the AirPods at `mac` and return what to send.
pub(crate) fn apply(
    mac: String,
    airpods: &mut AirPods,
    settling: &mut Settling,
    change: ControlChange,
) -> Vec<Effect> {
    let Some((identifier, value)) = change.command(airpods) else {
        return Vec::new();
    };
    airpods
        .control_values
        .insert(identifier as u8, value.clone());
    if change.settles() {
        let generation = settling.hold(&mac, identifier, value);
        return vec![Effect::SettleControl {
            mac,
            identifier,
            generation,
        }];
    }
    vec![Effect::SendControl {
        mac,
        identifier,
        value,
    }]
}

/// The settle delay of a slider change ran out: send its value unless a
/// newer change replaced it or the AirPods went away.
pub(crate) fn settled(
    mac: String,
    connected: bool,
    settling: &mut Settling,
    identifier: Id,
    generation: u64,
) -> Vec<Effect> {
    match settling.release(&mac, identifier, generation) {
        Some(value) if connected => vec![Effect::SendControl {
            mac,
            identifier,
            value,
        }],
        _ => Vec::new(),
    }
}

impl AirPods {
    /// The AirPods reported a value for this setting, so it is shown.
    pub(crate) fn reported(&self, id: Id) -> bool {
        self.control_values.contains_key(&(id as u8))
    }

    /// The decoded value of a setting; None when it was not reported or does
    /// not decode.
    pub(crate) fn control<T: ControlValue>(&self, id: Id) -> Option<T> {
        self.control_values
            .get(&(id as u8))
            .and_then(|value| T::from_value(value))
    }

    /// A reported on/off setting; an undecodable value shows as off.
    pub(crate) fn toggle(&self, id: Id) -> Option<bool> {
        self.reported(id)
            .then(|| self.control::<Toggle>(id).is_some_and(|t| t.0))
    }
}

/// One listening mode of the press and hold cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CycleRow {
    pub(crate) mode: CycleMode,
    /// Off is only offered while the AirPods allow the Off mode.
    pub(crate) shown: bool,
    pub(crate) on: bool,
    /// The switch cannot change: removing the mode would leave fewer than two
    /// in the cycle, or the reported cycle does not decode.
    pub(crate) locked: bool,
}

/// The rows of the press and hold cycle in `CycleMode::ALL` order, or None
/// when the AirPods did not report the cycle.
pub(crate) fn cycle_rows(airpods: &AirPods) -> Option<[CycleRow; 4]> {
    if !airpods.reported(Id::ListeningModeConfigs) {
        return None;
    }
    let cycle = airpods.control::<ListeningModeCycle>(Id::ListeningModeConfigs);
    Some(CycleMode::ALL.map(|mode| {
        let on = cycle.is_some_and(|c| c.contains(mode));
        CycleRow {
            mode,
            shown: mode != CycleMode::Off || airpods.allow_off,
            on,
            locked: ControlChange::CycleMode(mode, !on)
                .command(airpods)
                .is_none(),
        }
    }))
}

/// The press that hangs up a call, shown next to the mute choice.
pub(crate) fn hang_up_press(airpods: &AirPods) -> Option<&'static str> {
    airpods.reported(Id::CallManagementConfig).then(|| {
        airpods
            .control::<CallControls>(Id::CallManagementConfig)
            .map_or("", CallControls::hang_up_press)
    })
}

/// Tone volume slider position, or None when not reported.
pub(crate) fn tone_volume(airpods: &AirPods) -> Option<u8> {
    airpods.reported(Id::ChimeVolume).then(|| {
        airpods
            .control::<ChimeVolume>(Id::ChimeVolume)
            .map_or(DEFAULT_TONE_VOLUME, ChimeVolume::percent)
    })
}

/// Adaptive Audio slider position, or None when not reported. The slider runs
/// from less to more outside noise, the inverse of the stored strength.
pub(crate) fn adaptive_noise(airpods: &AirPods) -> Option<u8> {
    airpods.reported(Id::AutoAncStrength).then(|| {
        let strength = airpods
            .control::<AdaptiveStrength>(Id::AutoAncStrength)
            .map_or(DEFAULT_ADAPTIVE_STRENGTH, AdaptiveStrength::strength);
        PERCENT_MAX - strength
    })
}

/// A slider value as a position from 0 to 100 on a `SLIDER_STEP` grid.
pub(crate) fn slider_position(value: f64) -> u8 {
    let step = f64::from(SLIDER_STEP);
    // A NaN casts to 0; the clamp keeps the rest in range.
    ((value / step).round() * step).clamp(0.0, f64::from(PERCENT_MAX)) as u8
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bluetooth::aacp::ControlCommandStatus,
            devices::enums::{DeviceData, DeviceType},
            ui::{
                gtk::model::{AirPodsSnapshot, DeviceSnapshot, Input, Model},
                messages::BluetoothUIMessage,
            },
            utils::AppSettings,
        },
    };

    const PODS: &str = "AA:BB:CC:DD:EE:01";

    /// A model with PODS connected, reporting `controls`.
    fn connected(controls: &[(Id, &[u8])]) -> Model {
        let devices = HashMap::from([(
            PODS.to_string(),
            DeviceData {
                name: "Pods".to_string(),
                type_: DeviceType::AirPods,
                information: None,
            },
        )]);
        let mut model = Model::new(AppSettings::default(), devices);
        model.update(Input::Backend(BluetoothUIMessage::DeviceConnected(
            PODS.to_string(),
        )));
        let controls = controls
            .iter()
            .map(|(identifier, value)| ControlCommandStatus {
                identifier: *identifier,
                value: value.to_vec(),
            })
            .collect();
        model.update(Input::Snapshot(
            PODS.to_string(),
            Some(DeviceSnapshot::AirPods(AirPodsSnapshot {
                controls,
                ..AirPodsSnapshot::default()
            })),
        ));
        model
    }

    fn change(model: &mut Model, change: ControlChange) -> Vec<Effect> {
        model.update(Input::AirPodsControl(PODS.to_string(), change))
    }

    fn send(identifier: Id, value: &[u8]) -> Effect {
        Effect::SendControl {
            mac: PODS.to_string(),
            identifier,
            value: value.to_vec(),
        }
    }

    fn airpods(model: &Model) -> &AirPods {
        model.airpods(PODS).unwrap()
    }

    #[test]
    fn only_reported_settings_are_shown() {
        let model = connected(&[
            (Id::ChimeVolume, &[0x40, 0x50]),
            (Id::OneBudAncMode, &[0x01]),
            (Id::SleepDetectionConfig, &[0x07]),
        ]);
        let pods = airpods(&model);
        assert_eq!(tone_volume(pods), Some(0x40));
        assert_eq!(pods.toggle(Id::OneBudAncMode), Some(true));
        // Reported but unknown: shown, as off.
        assert_eq!(pods.toggle(Id::SleepDetectionConfig), Some(false));
        assert_eq!(pods.toggle(Id::VolumeSwipeMode), None);
        assert!(!pods.reported(Id::MicMode));
        assert_eq!(adaptive_noise(pods), None);
        assert_eq!(cycle_rows(pods), None);
        assert_eq!(hang_up_press(pods), None);
    }

    #[test]
    fn undecodable_values_fall_back_to_the_defaults() {
        let model = connected(&[
            (Id::ChimeVolume, &[0xFF]),
            (Id::AutoAncStrength, &[0xFF]),
            (Id::CallManagementConfig, &[0x07]),
        ]);
        let pods = airpods(&model);
        assert_eq!(tone_volume(pods), Some(DEFAULT_TONE_VOLUME));
        assert_eq!(adaptive_noise(pods), Some(50));
        assert_eq!(hang_up_press(pods), Some(""));
    }

    #[test]
    fn choices_and_switches_send_at_once_and_show_the_new_value() {
        let mut model = connected(&[
            (Id::MicMode, &[0x00]),
            (Id::CallManagementConfig, &[0x00, 0x03]),
            (Id::VolumeSwipeMode, &[0x01]),
        ]);
        assert_eq!(
            change(&mut model, ControlChange::MicMode(MicMode::AlwaysLeft)),
            [send(Id::MicMode, &[0x02])]
        );
        assert_eq!(
            airpods(&model).control::<MicMode>(Id::MicMode),
            Some(MicMode::AlwaysLeft)
        );

        assert_eq!(
            change(
                &mut model,
                ControlChange::MuteControl(CallControls::PressTwiceToMute)
            ),
            [send(Id::CallManagementConfig, &[0x00, 0x02])]
        );
        assert_eq!(hang_up_press(airpods(&model)), Some("Press Once"));

        assert_eq!(
            change(&mut model, ControlChange::VolumeSwipe(false)),
            [send(Id::VolumeSwipeMode, &[0x02])]
        );
        assert_eq!(airpods(&model).toggle(Id::VolumeSwipeMode), Some(false));

        for (change_to, identifier, value) in [
            (
                ControlChange::PressSpeed(PressInterval::Slowest),
                Id::DoubleClickInterval,
                &[0x02][..],
            ),
            (
                ControlChange::HoldDuration(PressInterval::Slower),
                Id::ClickHoldInterval,
                &[0x01],
            ),
            (
                ControlChange::SwipeSpeed(SwipeInterval::Longest),
                Id::VolumeSwipeInterval,
                &[0x03],
            ),
            (ControlChange::OneBudAnc(true), Id::OneBudAncMode, &[0x01]),
            (
                ControlChange::SleepDetection(false),
                Id::SleepDetectionConfig,
                &[0x02],
            ),
        ] {
            assert_eq!(
                change(&mut model, change_to),
                [send(identifier, value)],
                "{change_to:?}"
            );
        }
    }

    #[test]
    fn long_press_changes_one_bud_and_keeps_the_other() {
        let mut model = connected(&[(Id::ClickHoldMode, &[0x05, 0x01])]);
        assert_eq!(
            change(
                &mut model,
                ControlChange::LongPress(Bud::Left, ClickHoldAction::Siri)
            ),
            [send(Id::ClickHoldMode, &[0x05, 0x05])]
        );
        assert_eq!(
            change(
                &mut model,
                ControlChange::LongPress(Bud::Right, ClickHoldAction::NoiseControl)
            ),
            [send(Id::ClickHoldMode, &[0x01, 0x05])]
        );
    }

    #[test]
    fn long_press_on_an_unknown_value_keeps_the_default_for_the_other_bud() {
        let mut model = connected(&[(Id::ClickHoldMode, &[0x09, 0x09])]);
        assert_eq!(
            change(
                &mut model,
                ControlChange::LongPress(Bud::Right, ClickHoldAction::Siri)
            ),
            [send(Id::ClickHoldMode, &[0x05, 0x01])]
        );
    }

    #[test]
    fn cycle_keeps_two_modes_and_locks_the_last_two() {
        // Noise cancellation and transparency.
        let mut model = connected(&[(Id::ListeningModeConfigs, &[0x06])]);
        let rows = cycle_rows(airpods(&model)).unwrap();
        let row = |mode| *rows.iter().find(|r| r.mode == mode).unwrap();
        assert!(!row(CycleMode::Off).shown);
        assert!(row(CycleMode::Transparency).on && row(CycleMode::Transparency).locked);
        assert!(!row(CycleMode::Adaptive).on && !row(CycleMode::Adaptive).locked);

        assert!(
            change(
                &mut model,
                ControlChange::CycleMode(CycleMode::Transparency, false)
            )
            .is_empty()
        );
        assert_eq!(
            change(
                &mut model,
                ControlChange::CycleMode(CycleMode::Adaptive, true)
            ),
            [send(Id::ListeningModeConfigs, &[0x0E])]
        );
        // With three modes, one can go again.
        assert_eq!(
            change(
                &mut model,
                ControlChange::CycleMode(CycleMode::Transparency, false)
            ),
            [send(Id::ListeningModeConfigs, &[0x0A])]
        );
        // A repeated request for the current state sends nothing.
        assert!(
            change(
                &mut model,
                ControlChange::CycleMode(CycleMode::Adaptive, true)
            )
            .is_empty()
        );
    }

    #[test]
    fn off_joins_the_cycle_only_when_allowed() {
        let mut model = connected(&[(Id::ListeningModeConfigs, &[0x06])]);
        assert!(change(&mut model, ControlChange::CycleMode(CycleMode::Off, true)).is_empty());

        model.update(Input::SetAllowOff(PODS.to_string(), true));
        let rows = cycle_rows(airpods(&model)).unwrap();
        assert!(rows[0].shown && !rows[0].locked);
        assert_eq!(
            change(&mut model, ControlChange::CycleMode(CycleMode::Off, true)),
            [send(Id::ListeningModeConfigs, &[0x07])]
        );
    }

    #[test]
    fn an_unknown_cycle_is_shown_locked_and_never_sent() {
        let mut model = connected(&[(Id::ListeningModeConfigs, &[0x30])]);
        let rows = cycle_rows(airpods(&model)).unwrap();
        assert!(rows.iter().all(|r| !r.on && r.locked));
        assert!(
            change(
                &mut model,
                ControlChange::CycleMode(CycleMode::Adaptive, true)
            )
            .is_empty()
        );
    }

    /// Run the settle input for `effect`, which must be a SettleControl.
    fn settle(model: &mut Model, effect: &Effect) -> Vec<Effect> {
        let Effect::SettleControl {
            mac,
            identifier,
            generation,
        } = effect
        else {
            panic!("settle expected, got {effect:?}");
        };
        model.update(Input::ControlSettled {
            mac: mac.clone(),
            identifier: *identifier,
            generation: *generation,
        })
    }

    #[test]
    fn a_slider_drag_sends_only_the_value_it_rests_on() {
        let mut model = connected(&[(Id::ChimeVolume, &[0x40, 0x50])]);
        let first = change(&mut model, ControlChange::ToneVolume(30));
        assert!(matches!(first.as_slice(), [Effect::SettleControl { .. }]));
        // Shown at once, before anything is sent.
        assert_eq!(tone_volume(airpods(&model)), Some(30));
        let second = change(&mut model, ControlChange::ToneVolume(20));

        assert!(settle(&mut model, &first[0]).is_empty());
        assert_eq!(
            settle(&mut model, &second[0]),
            [send(Id::ChimeVolume, &[20, 0x50])]
        );
        // Settling twice sends once.
        assert!(settle(&mut model, &second[0]).is_empty());
    }

    #[test]
    fn adaptive_slider_sends_the_inverse_strength() {
        let mut model = connected(&[(Id::AutoAncStrength, &[70])]);
        assert_eq!(adaptive_noise(airpods(&model)), Some(30));
        let effects = change(&mut model, ControlChange::AdaptiveNoise(80));
        assert_eq!(adaptive_noise(airpods(&model)), Some(80));
        assert_eq!(
            settle(&mut model, &effects[0]),
            [send(Id::AutoAncStrength, &[20])]
        );
    }

    #[test]
    fn sliders_settle_per_setting() {
        let mut model = connected(&[
            (Id::ChimeVolume, &[0x40, 0x50]),
            (Id::AutoAncStrength, &[50]),
        ]);
        let volume = change(&mut model, ControlChange::ToneVolume(10));
        let noise = change(&mut model, ControlChange::AdaptiveNoise(10));
        assert_eq!(
            settle(&mut model, &volume[0]),
            [send(Id::ChimeVolume, &[10, 0x50])]
        );
        assert_eq!(
            settle(&mut model, &noise[0]),
            [send(Id::AutoAncStrength, &[90])]
        );
    }

    #[test]
    fn a_settled_value_is_dropped_after_a_disconnect() {
        let mut model = connected(&[(Id::ChimeVolume, &[0x40, 0x50])]);
        let effects = change(&mut model, ControlChange::ToneVolume(10));
        model.update(Input::Backend(BluetoothUIMessage::DeviceDisconnected(
            PODS.to_string(),
        )));
        assert!(settle(&mut model, &effects[0]).is_empty());
        assert!(change(&mut model, ControlChange::ToneVolume(10)).is_empty());
    }

    #[test]
    fn slider_positions_snap_to_the_step_and_stay_in_range() {
        assert_eq!(slider_position(0.0), 0);
        assert_eq!(slider_position(42.4), 40);
        assert_eq!(slider_position(42.6), 45);
        assert_eq!(slider_position(100.0), 100);
        assert_eq!(slider_position(180.0), 100);
        assert_eq!(slider_position(-3.0), 0);
        assert_eq!(slider_position(f64::NAN), 0);
    }
}
