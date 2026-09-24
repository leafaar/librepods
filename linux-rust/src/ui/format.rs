//! Display text and input checks for the device pages. Nothing here depends on
//! the UI toolkit, so any front end can share it.

use {
    crate::bluetooth::aacp::{BatteryComponent, BatteryInfo, BatteryStatus},
    std::time::Duration,
};

/// Longest name the AirPods accept in a rename packet, in bytes.
pub const MAX_NAME_BYTES: usize = 32;

/// The name to send, trimmed, or a short hint saying why it cannot be sent.
pub fn validate_device_name(name: &str) -> Result<&str, &'static str> {
    let name = name.trim();
    if name.is_empty() {
        Err("Name can't be empty")
    } else if name.len() > MAX_NAME_BYTES {
        Err("Name is too long")
    } else {
        Ok(name)
    }
}

/// A duration as minutes and zero-padded seconds, such as "1:05".
pub fn mmss(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The level of a battery entry, or None when the component is disconnected or
/// the level is out of range.
pub fn known_level(info: &BatteryInfo) -> Option<u8> {
    (info.status != BatteryStatus::Disconnected && info.level <= 100).then_some(info.level)
}

/// The case level a battery report carries, if any. AirPods only know the case
/// level while a bud sits in it: with both buds out, the case entry reports
/// Disconnected with a level of 0 or 255.
pub fn live_case_level(battery: &[BatteryInfo]) -> Option<u8> {
    battery
        .iter()
        .find(|b| b.component == BatteryComponent::Case)
        .and_then(known_level)
}

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
    #[test]
    fn mmss_pads_seconds() {
        assert_eq!(mmss(Duration::ZERO), "0:00");
        assert_eq!(mmss(Duration::from_millis(59_999)), "0:59");
        assert_eq!(mmss(Duration::from_secs(65)), "1:05");
        assert_eq!(mmss(Duration::from_secs(300)), "5:00");
        assert_eq!(mmss(Duration::from_secs(3600)), "60:00");
    }

    #[test]
    fn device_name_is_trimmed_and_bounded() {
        assert_eq!(validate_device_name("  Pods  "), Ok("Pods"));
        assert!(validate_device_name("").is_err());
        assert!(validate_device_name("   ").is_err());
        let longest = "a".repeat(MAX_NAME_BYTES);
        assert_eq!(validate_device_name(&longest), Ok(longest.as_str()));
        assert!(validate_device_name(&"a".repeat(MAX_NAME_BYTES + 1)).is_err());
        // Eleven three-byte characters are 33 bytes: the limit is in bytes, not chars.
        assert!(validate_device_name(&"\u{20AC}".repeat(11)).is_err());
        assert!(validate_device_name(&"\u{20AC}".repeat(10)).is_ok());
    }

    #[test]
    fn device_name_hints_say_what_is_wrong() {
        assert_eq!(validate_device_name(" "), Err("Name can't be empty"));
        assert_eq!(
            validate_device_name(&"a".repeat(MAX_NAME_BYTES + 1)),
            Err("Name is too long")
        );
    }

    #[test]
    fn case_level_is_remembered_only_when_reported() {
        let in_case = [entry(
            BatteryComponent::Case,
            60,
            BatteryStatus::NotCharging,
        )];
        let buds_out = [entry(
            BatteryComponent::Case,
            0,
            BatteryStatus::Disconnected,
        )];
        let buds_out_255 = [entry(
            BatteryComponent::Case,
            255,
            BatteryStatus::Disconnected,
        )];
        let bad_level = [entry(BatteryComponent::Case, 101, BatteryStatus::Charging)];
        assert_eq!(live_case_level(&in_case), Some(60));
        assert_eq!(live_case_level(&buds_out), None);
        assert_eq!(live_case_level(&buds_out_255), None);
        assert_eq!(live_case_level(&bad_level), None);
        assert_eq!(live_case_level(&[]), None);
    }
}
