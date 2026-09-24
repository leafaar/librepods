//! Display text and input checks for the device pages. Nothing here depends on
//! the UI toolkit, so any front end can share it.

use {
    crate::bluetooth::aacp::{BatteryComponent, BatteryInfo, BatteryStatus},
    std::time::Duration,
};

/// SF Symbols glyph drawn after a level that is charging.
pub const CHARGING_MARK: &str = "\u{1002E6}";

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
fn known_level(info: &BatteryInfo) -> Option<u8> {
    (info.status != BatteryStatus::Disconnected && info.level <= 100).then_some(info.level)
}

/// "80%" with a charging mark, or "-" when the component is absent or disconnected.
pub fn battery_text(info: Option<&BatteryInfo>) -> String {
    match info.and_then(|b| known_level(b).map(|level| (level, b.status))) {
        Some((level, status)) => {
            let mark = if status.is_charging() {
                CHARGING_MARK
            } else {
                ""
            };
            format!("{level}%{mark}")
        },
        None => "-".to_string(),
    }
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

/// The sidebar battery line as (text, stale) parts. Headphones show a single
/// level. For earbuds, a case that cannot report falls back to `last_case`,
/// marked stale so it is drawn dimmed.
pub fn battery_parts(battery: &[BatteryInfo], last_case: Option<u8>) -> Vec<(String, bool)> {
    let find = |component| battery.iter().find(|b| b.component == component);
    if let Some(headphone) = find(BatteryComponent::Headphone) {
        return vec![(format!("􀺹 {}", battery_text(Some(headphone))), false)];
    }
    let (case, stale) = match (live_case_level(battery), last_case) {
        (Some(_), _) => (battery_text(find(BatteryComponent::Case)), false),
        (None, Some(level)) => (format!("{level}%"), true),
        (None, None) => ("-".to_string(), false),
    };
    vec![
        (
            format!("\u{1018E5} {}", battery_text(find(BatteryComponent::Left))),
            false,
        ),
        (
            format!("\u{1018E8} {}", battery_text(find(BatteryComponent::Right))),
            false,
        ),
        (format!("\u{100E6C} {case}"), stale),
    ]
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

    fn texts(parts: &[(String, bool)]) -> Vec<&str> {
        parts.iter().map(|(t, _)| t.as_str()).collect()
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
    fn battery_text_marks_charging_and_unknown() {
        let charging = entry(BatteryComponent::Left, 40, BatteryStatus::Charging);
        let optimized = entry(BatteryComponent::Left, 80, BatteryStatus::OptimizedCharging);
        let idle = entry(BatteryComponent::Left, 100, BatteryStatus::NotCharging);
        let gone = entry(BatteryComponent::Left, 0, BatteryStatus::Disconnected);
        let bogus = entry(BatteryComponent::Left, 255, BatteryStatus::NotCharging);
        assert_eq!(battery_text(Some(&charging)), format!("40%{CHARGING_MARK}"));
        assert_eq!(
            battery_text(Some(&optimized)),
            format!("80%{CHARGING_MARK}")
        );
        assert_eq!(battery_text(Some(&idle)), "100%");
        assert_eq!(battery_text(Some(&gone)), "-");
        assert_eq!(battery_text(Some(&bogus)), "-");
        assert_eq!(battery_text(None), "-");
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
        let battery = [entry(
            BatteryComponent::Headphone,
            30,
            BatteryStatus::NotCharging,
        )];
        assert_eq!(texts(&battery_parts(&battery, Some(70))), ["􀺹 30%"]);
    }
}
