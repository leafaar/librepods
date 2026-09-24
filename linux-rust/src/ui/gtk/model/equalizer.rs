//! The custom EQ of connected AirPods: which models offer it and when a
//! change is sent.
//!
//! The switch and Reset send at once. A slider drag reports every step it
//! passes and the AirPods only need where it settles, so band changes are
//! sent once they stayed still for EQ_SEND_DELAY; every newer change, delayed
//! or not, cancels the delayed send before it.

use {
    super::{Effect, Model},
    crate::bluetooth::eq::{BAND_MAX, CustomEq, model_supports_custom_eq},
    std::{collections::HashMap, time::Duration},
};

/// How long the bands must stay still before they are sent.
pub(crate) const EQ_SEND_DELAY: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EqBand {
    Low,
    Mid,
    High,
}

impl EqBand {
    pub(crate) const ALL: [EqBand; 3] = [EqBand::Low, EqBand::Mid, EqBand::High];

    pub(crate) fn label(self) -> &'static str {
        match self {
            EqBand::Low => "Low",
            EqBand::Mid => "Mid",
            EqBand::High => "High",
        }
    }

    pub(crate) fn get(self, eq: CustomEq) -> u8 {
        match self {
            EqBand::Low => eq.low,
            EqBand::Mid => eq.mid,
            EqBand::High => eq.high,
        }
    }

    fn set(self, eq: &mut CustomEq, value: u8) {
        match self {
            EqBand::Low => eq.low = value,
            EqBand::Mid => eq.mid = value,
            EqBand::High => eq.high = value,
        }
    }
}

#[derive(Debug)]
pub(crate) enum EqInput {
    SetEnabled(String, bool),
    SetBand(String, EqBand, u8),
    /// Back to flat bands, keeping the custom EQ on.
    Reset(String),
    /// EQ_SEND_DELAY passed since the band change `generation` was made.
    SendDue {
        mac: String,
        generation: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EqEffect {
    /// Send to the AirPods after every EQ sent before it.
    Send { mac: String, eq: CustomEq },
    /// Come back with `EqInput::SendDue` after EQ_SEND_DELAY.
    SendLater { mac: String, generation: u64 },
}

/// Bookkeeping for delayed sends.
#[derive(Default)]
pub(super) struct EqSends {
    /// Bumped on every change.
    generation: u64,
    /// The change whose delayed send may still go out, per device.
    pending: HashMap<String, u64>,
}

/// Flat bands with the custom EQ on, what Reset sets.
const FLAT: CustomEq = CustomEq {
    enabled: true,
    low: 50,
    mid: 50,
    high: 50,
};

impl Model {
    pub(super) fn equalizer(&mut self, input: EqInput) -> Vec<Effect> {
        match input {
            EqInput::SetEnabled(mac, enabled) => self.change_eq(mac, |eq| {
                eq.enabled = enabled;
                true
            }),
            EqInput::Reset(mac) => self.change_eq(mac, |eq| {
                *eq = FLAT;
                true
            }),
            EqInput::SetBand(mac, band, value) => {
                let value = value.min(BAND_MAX);
                self.change_eq(mac, |eq| {
                    // Bands only show while the custom EQ is on.
                    if !eq.enabled {
                        return false;
                    }
                    band.set(eq, value);
                    false
                })
            },
            EqInput::SendDue { mac, generation } => {
                if self.eq_sends.pending.get(&mac) != Some(&generation) {
                    return Vec::new();
                }
                self.eq_sends.pending.remove(&mac);
                let Some(eq) = self.airpods.get(&mac).and_then(|a| a.custom_eq) else {
                    return Vec::new();
                };
                vec![Effect::Equalizer(EqEffect::Send { mac, eq })]
            },
        }
    }

    /// Apply `change` to the EQ of the AirPods at `mac`. It returns whether to
    /// send now; otherwise the send waits for EQ_SEND_DELAY. Nothing is sent
    /// when the EQ did not change or the model does not offer it.
    fn change_eq(
        &mut self,
        mac: String,
        change: impl FnOnce(&mut CustomEq) -> bool,
    ) -> Vec<Effect> {
        if !self.equalizer_supported(&mac) {
            return Vec::new();
        }
        let Some(airpods) = self.airpods.get_mut(&mac) else {
            return Vec::new();
        };
        let before = airpods.custom_eq.unwrap_or_default();
        let mut eq = before;
        let now = change(&mut eq);
        if eq == before {
            return Vec::new();
        }
        airpods.custom_eq = Some(eq);
        let sends = &mut self.eq_sends;
        sends.generation += 1;
        let effect = if now {
            sends.pending.remove(&mac);
            EqEffect::Send { mac, eq }
        } else {
            sends.pending.insert(mac.clone(), sends.generation);
            EqEffect::SendLater {
                mac,
                generation: sends.generation,
            }
        };
        vec![Effect::Equalizer(effect)]
    }

    pub(super) fn eq_disconnected(&mut self, mac: &str) {
        self.eq_sends.pending.remove(mac);
    }

    // Read access for the view.

    /// Whether to offer the custom EQ for the AirPods at `mac`, from the
    /// model number in devices.json.
    pub(crate) fn equalizer_supported(&self, mac: &str) -> bool {
        model_supports_custom_eq(self.information(mac).map(|i| i.model_number.as_str()))
    }

    /// The EQ to show: the last one reported or set, off and flat before the
    /// AirPods reported any.
    pub(crate) fn custom_eq(&self, mac: &str) -> CustomEq {
        self.airpods
            .get(mac)
            .and_then(|a| a.custom_eq)
            .unwrap_or_default()
    }

    /// Reset does something: the custom EQ is on and not flat.
    pub(crate) fn eq_can_reset(&self, mac: &str) -> bool {
        let eq = self.custom_eq(mac);
        eq.enabled && eq != FLAT
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bluetooth::aacp::AACPEvent,
            devices::{
                airpods::AirPodsInformation,
                enums::{DeviceData, DeviceInformation, DeviceType},
            },
            ui::{
                gtk::model::{
                    AirPodsSnapshot, Input,
                    tests::{PODS, connected, model},
                },
                messages::BluetoothUIMessage,
            },
            utils::AppSettings,
        },
    };

    fn eq(model: &mut Model, input: EqInput) -> Vec<Effect> {
        model.update(Input::Equalizer(input))
    }

    fn on(model: &mut Model) {
        eq(model, EqInput::SetEnabled(PODS.to_string(), true));
    }

    fn send(eq: CustomEq) -> Vec<Effect> {
        vec![Effect::Equalizer(EqEffect::Send {
            mac: PODS.to_string(),
            eq,
        })]
    }

    fn later(effects: &[Effect]) -> u64 {
        let [Effect::Equalizer(EqEffect::SendLater { generation, .. })] = effects else {
            panic!("delayed send expected, got {effects:?}");
        };
        *generation
    }

    fn due(model: &mut Model, generation: u64) -> Vec<Effect> {
        eq(
            model,
            EqInput::SendDue {
                mac: PODS.to_string(),
                generation,
            },
        )
    }

    fn with_model_number(number: &str) -> Model {
        let information = AirPodsInformation {
            model_number: number.to_string(),
            ..AirPodsInformation::default()
        };
        let devices = HashMap::from([(
            PODS.to_string(),
            DeviceData {
                name: "Pods".to_string(),
                type_: DeviceType::AirPods,
                information: Some(DeviceInformation::AirPods(information)),
            },
        )]);
        let mut model = Model::new(AppSettings::default(), devices);
        connected(&mut model, AirPodsSnapshot::default());
        model
    }

    #[test]
    fn switch_sends_at_once() {
        let mut model = model();
        connected(&mut model, AirPodsSnapshot::default());
        let effects = eq(&mut model, EqInput::SetEnabled(PODS.to_string(), true));
        let expected = CustomEq {
            enabled: true,
            ..CustomEq::default()
        };
        assert_eq!(effects, send(expected));
        assert_eq!(model.custom_eq(PODS), expected);
        // Switching to the state it already has sends nothing.
        assert!(eq(&mut model, EqInput::SetEnabled(PODS.to_string(), true)).is_empty());
    }

    #[test]
    fn band_changes_coalesce_into_the_last_one() {
        let mut model = model();
        connected(&mut model, AirPodsSnapshot::default());
        on(&mut model);
        let first = later(&eq(
            &mut model,
            EqInput::SetBand(PODS.to_string(), EqBand::Low, 60),
        ));
        let second = later(&eq(
            &mut model,
            EqInput::SetBand(PODS.to_string(), EqBand::Low, 70),
        ));
        assert!(due(&mut model, first).is_empty());
        let expected = CustomEq {
            enabled: true,
            low: 70,
            mid: 50,
            high: 50,
        };
        assert_eq!(due(&mut model, second), send(expected));
        // Each delayed send goes out once.
        assert!(due(&mut model, second).is_empty());
    }

    #[test]
    fn a_send_now_cancels_the_delayed_one() {
        let mut model = model();
        connected(&mut model, AirPodsSnapshot::default());
        on(&mut model);
        let pending = later(&eq(
            &mut model,
            EqInput::SetBand(PODS.to_string(), EqBand::High, 100),
        ));
        assert_eq!(model.custom_eq(PODS).high, 100);
        assert!(model.eq_can_reset(PODS));
        assert_eq!(eq(&mut model, EqInput::Reset(PODS.to_string())), send(FLAT));
        assert!(due(&mut model, pending).is_empty());
        assert!(!model.eq_can_reset(PODS));
        // Reset on flat bands sends nothing.
        assert!(eq(&mut model, EqInput::Reset(PODS.to_string())).is_empty());
    }

    #[test]
    fn bands_are_clamped_and_ignored_while_off() {
        let mut model = model();
        connected(&mut model, AirPodsSnapshot::default());
        assert!(
            eq(
                &mut model,
                EqInput::SetBand(PODS.to_string(), EqBand::Mid, 80)
            )
            .is_empty()
        );
        assert_eq!(model.custom_eq(PODS).mid, 50);
        on(&mut model);
        eq(
            &mut model,
            EqInput::SetBand(PODS.to_string(), EqBand::Mid, 200),
        );
        assert_eq!(model.custom_eq(PODS).mid, BAND_MAX);
        // The same value again is not a change.
        assert!(
            eq(
                &mut model,
                EqInput::SetBand(PODS.to_string(), EqBand::Mid, BAND_MAX)
            )
            .is_empty()
        );
    }

    #[test]
    fn reported_eq_replaces_the_shown_one() {
        let mut model = model();
        connected(
            &mut model,
            AirPodsSnapshot {
                custom_eq: Some(CustomEq {
                    enabled: true,
                    low: 10,
                    mid: 20,
                    high: 30,
                }),
                ..AirPodsSnapshot::default()
            },
        );
        assert_eq!(EqBand::High.get(model.custom_eq(PODS)), 30);
        let reported = CustomEq {
            enabled: false,
            ..CustomEq::default()
        };
        model.update(Input::Backend(BluetoothUIMessage::AACPUIEvent(
            PODS.to_string(),
            AACPEvent::CustomEq(reported),
        )));
        assert_eq!(model.custom_eq(PODS), reported);
    }

    #[test]
    fn older_models_do_not_offer_it() {
        let mut model = with_model_number("A2084");
        assert!(!model.equalizer_supported(PODS));
        assert!(eq(&mut model, EqInput::SetEnabled(PODS.to_string(), true)).is_empty());

        let mut model = with_model_number("A2698");
        assert!(model.equalizer_supported(PODS));
        assert_eq!(
            eq(&mut model, EqInput::SetEnabled(PODS.to_string(), true)).len(),
            1
        );
    }

    #[test]
    fn disconnected_airpods_send_nothing() {
        let mut model = model();
        connected(&mut model, AirPodsSnapshot::default());
        on(&mut model);
        let pending = later(&eq(
            &mut model,
            EqInput::SetBand(PODS.to_string(), EqBand::Low, 0),
        ));
        model.update(Input::Backend(BluetoothUIMessage::DeviceDisconnected(
            PODS.to_string(),
        )));
        assert!(due(&mut model, pending).is_empty());
        assert!(eq(&mut model, EqInput::SetEnabled(PODS.to_string(), false)).is_empty());
    }
}
