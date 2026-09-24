//! Custom EQ (AACP opcode 0x63): three bands on a 0..=100 scale plus an
//! on/off state.
//!
//! Both directions use the same body, shown here without the 04 00 04 00
//! AACP header:
//!
//! ```text
//! 63 00  05 00  01  state  low  mid  high
//! ^op    ^len   ^?  1 = off (recommended EQ), 2 = on (custom bands)
//! ```
//!
//! The length is little-endian and counts the five bytes after it. The byte
//! after the length is always 0x01 in what we send; its meaning is unknown, so
//! it is not checked on receive.

use crate::bluetooth::aacp::opcodes;

/// Highest value of a band. 50 is flat.
pub const BAND_MAX: u8 = 100;

const STATE_DISABLED: u8 = 0x01;
const STATE_ENABLED: u8 = 0x02;
const BODY_LEN: u8 = 0x05;
const PACKET_LEN: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CustomEq {
    pub enabled: bool,
    pub low: u8,
    pub mid: u8,
    pub high: u8,
}

impl Default for CustomEq {
    /// Off and flat, which is what the AirPods use until the user changes it.
    fn default() -> Self {
        Self {
            enabled: false,
            low: 50,
            mid: 50,
            high: 50,
        }
    }
}

impl CustomEq {
    /// Opcode and body, to be sent with `AACPManager::send_custom_eq`, which
    /// adds the AACP header. Bands above `BAND_MAX` are clamped so the
    /// AirPods never see an out-of-range value.
    pub fn to_packet(&self) -> [u8; PACKET_LEN] {
        [
            opcodes::CUSTOM_EQ,
            0x00,
            BODY_LEN,
            0x00,
            0x01,
            if self.enabled {
                STATE_ENABLED
            } else {
                STATE_DISABLED
            },
            self.low.min(BAND_MAX),
            self.mid.min(BAND_MAX),
            self.high.min(BAND_MAX),
        ]
    }
}

/// Model numbers of AirPods 1, 2, 3 and Pro 1. The Android app lists the
/// equalizer in its audio section, which it only shows for models with
/// adaptive volume, conversation awareness or loud sound reduction (AirPods 4,
/// AirPods 4 ANC, Pro 2, Pro 3). An unknown model number falls back to a Pro
/// profile there, so only these known older models go without the equalizer.
const MODELS_WITHOUT_EQ: [&str; 8] = [
    "A1523", "A1722", "A2032", "A2031", "A2565", "A2564", "A2084", "A2083",
];

/// Whether to offer the custom EQ for a model number, `None` when the AirPods
/// have not reported it yet.
pub fn model_supports_custom_eq(model_number: Option<&str>) -> bool {
    model_number.is_none_or(|m| !MODELS_WITHOUT_EQ.contains(&m))
}

/// Parse a custom EQ packet as the AirPods send it, starting at the opcode
/// (the AACP header already stripped).
///
/// The length field is not enforced and trailing bytes are ignored, since
/// firmware may append fields. Returns `None` for a short packet, a wrong
/// opcode, or a band above `BAND_MAX`. Any state other than 2 reads as off,
/// which is how the Android app interprets it.
pub fn parse(payload: &[u8]) -> Option<CustomEq> {
    let [opcode, _, _, _, _, state, low, mid, high, ..] = *payload else {
        return None;
    };
    if opcode != opcodes::CUSTOM_EQ || [low, mid, high].iter().any(|&b| b > BAND_MAX) {
        return None;
    }
    Some(CustomEq {
        enabled: state == STATE_ENABLED,
        low,
        mid,
        high,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_packet_matches_android_layout() {
        let eq = CustomEq {
            enabled: true,
            low: 65,
            mid: 50,
            high: 70,
        };
        assert_eq!(
            eq.to_packet(),
            [0x63, 0x00, 0x05, 0x00, 0x01, 0x02, 65, 50, 70]
        );
        let off = CustomEq {
            enabled: false,
            ..eq
        };
        assert_eq!(off.to_packet()[5], 0x01);
    }

    #[test]
    fn to_packet_clamps_bands() {
        let eq = CustomEq {
            enabled: true,
            low: 101,
            mid: 255,
            high: 100,
        };
        assert_eq!(&eq.to_packet()[6..], &[100, 100, 100]);
    }

    #[test]
    fn round_trip() {
        for enabled in [false, true] {
            for (low, mid, high) in [(0, 0, 0), (50, 50, 50), (100, 0, 100), (1, 99, 42)] {
                let eq = CustomEq {
                    enabled,
                    low,
                    mid,
                    high,
                };
                assert_eq!(parse(&eq.to_packet()), Some(eq));
            }
        }
    }

    #[test]
    fn parses_known_layouts() {
        // Body as the Android app sends and parses it.
        assert_eq!(
            parse(&[0x63, 0x00, 0x05, 0x00, 0x01, 0x02, 0x41, 0x32, 0x46]),
            Some(CustomEq {
                enabled: true,
                low: 65,
                mid: 50,
                high: 70,
            })
        );
        assert_eq!(
            parse(&[0x63, 0x00, 0x05, 0x00, 0x01, 0x01, 0x32, 0x32, 0x32]),
            Some(CustomEq::default())
        );
        // Unexpected length and extra trailing bytes are tolerated.
        assert_eq!(
            parse(&[0x63, 0x00, 0x07, 0x00, 0x00, 0x02, 0, 100, 0, 0xAA, 0xBB]),
            Some(CustomEq {
                enabled: true,
                low: 0,
                mid: 100,
                high: 0,
            })
        );
        // Unknown state reads as off.
        assert_eq!(
            parse(&[0x63, 0x00, 0x05, 0x00, 0x01, 0x00, 10, 20, 30]).map(|e| e.enabled),
            Some(false)
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[0x63]), None);
        assert_eq!(parse(&[0x63, 0x00, 0x05, 0x00, 0x01, 0x02, 50, 50]), None);
        // Wrong opcode.
        assert_eq!(
            parse(&[0x53, 0x00, 0x05, 0x00, 0x01, 0x02, 50, 50, 50]),
            None
        );
        // Band out of range.
        assert_eq!(
            parse(&[0x63, 0x00, 0x05, 0x00, 0x01, 0x02, 101, 50, 50]),
            None
        );
        assert_eq!(
            parse(&[0x63, 0x00, 0x05, 0x00, 0x01, 0x02, 50, 50, 0xFF]),
            None
        );
    }

    #[test]
    fn model_gate() {
        assert!(model_supports_custom_eq(None));
        assert!(model_supports_custom_eq(Some("A3048"))); // Pro 2 USB-C
        assert!(model_supports_custom_eq(Some("A3053"))); // AirPods 4
        assert!(model_supports_custom_eq(Some("A3064"))); // Pro 3
        assert!(model_supports_custom_eq(Some("A9999")));
        assert!(!model_supports_custom_eq(Some("A2084"))); // Pro 1
        assert!(!model_supports_custom_eq(Some("A2032"))); // AirPods 2
    }

    #[test]
    fn arbitrary_input_never_panics() {
        // Every length from empty to well past the packet, filled from a small
        // xorshift generator, with and without the right opcode in front. Any
        // result that parses must hold in-range bands.
        let mut seed: u32 = 0x9E37_79B9;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed.to_le_bytes()[0]
        };
        for len in 0..64 {
            for round in 0..64 {
                let mut buf: Vec<u8> = (0..len).map(|_| next()).collect();
                if round % 2 == 0
                    && let Some(first) = buf.first_mut()
                {
                    *first = opcodes::CUSTOM_EQ;
                }
                if let Some(eq) = parse(&buf) {
                    assert!(len >= PACKET_LEN);
                    assert!(eq.low <= BAND_MAX && eq.mid <= BAND_MAX && eq.high <= BAND_MAX);
                }
            }
        }
    }
}
