//! What the sidebar and the AirPods page show for a battery report: a level,
//! whether it charges, whether it is a remembered value, and the Adwaita icon
//! for it. Toolkit free so the decisions are tested without a display.

use crate::{
    bluetooth::aacp::{BatteryComponent, BatteryInfo},
    ui::format::{known_level, live_case_level},
};

/// One battery as shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Level {
    /// None when the component is absent, disconnected or reports garbage.
    pub(crate) percent: Option<u8>,
    pub(crate) charging: bool,
    /// A remembered case level, drawn dimmed: the case only reports while a
    /// bud sits in it.
    pub(crate) stale: bool,
}

impl Level {
    const UNKNOWN: Level = Level {
        percent: None,
        charging: false,
        stale: false,
    };

    fn from_info(info: Option<&BatteryInfo>) -> Self {
        let Some(info) = info else {
            return Self::UNKNOWN;
        };
        let percent = known_level(info);
        Level {
            percent,
            charging: percent.is_some() && info.status.is_charging(),
            stale: false,
        }
    }

    /// "80%", or "-" when unknown.
    pub(crate) fn text(self) -> String {
        self.percent
            .map_or_else(|| "-".to_string(), |p| format!("{p}%"))
    }

    /// Adwaita symbolic icon for the level, rounded to the nearest ten.
    pub(crate) fn icon_name(self) -> String {
        let Some(percent) = self.percent else {
            return "battery-missing-symbolic".to_string();
        };
        let step = (u16::from(percent.min(100)) + 5) / 10 * 10;
        match (step, self.charging) {
            (100, true) => "battery-level-100-charged-symbolic".to_string(),
            (_, true) => format!("battery-level-{step}-charging-symbolic"),
            (_, false) => format!("battery-level-{step}-symbolic"),
        }
    }
}

/// The batteries of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Batteries {
    /// Over-ear headphones report one level.
    Headphone(Level),
    Buds {
        left: Level,
        right: Level,
        case: Level,
    },
}

impl Batteries {
    /// The batteries with their short labels, in display order.
    pub(crate) fn labeled(&self) -> Vec<(&'static str, Level)> {
        match *self {
            Batteries::Headphone(level) => vec![("Battery", level)],
            Batteries::Buds { left, right, case } => {
                vec![("Left", left), ("Right", right), ("Case", case)]
            },
        }
    }
}

/// The batteries to show for a report. A case that cannot report falls back
/// to `last_case`, marked stale.
pub(crate) fn batteries(battery: &[BatteryInfo], last_case: Option<u8>) -> Batteries {
    let find = |component| battery.iter().find(|b| b.component == component);
    if let Some(headphone) = find(BatteryComponent::Headphone) {
        return Batteries::Headphone(Level::from_info(Some(headphone)));
    }
    let case = match (live_case_level(battery), last_case) {
        (Some(_), _) => Level::from_info(find(BatteryComponent::Case)),
        (None, Some(level)) => Level {
            percent: Some(level),
            charging: false,
            stale: true,
        },
        (None, None) => Level::UNKNOWN,
    };
    Batteries::Buds {
        left: Level::from_info(find(BatteryComponent::Left)),
        right: Level::from_info(find(BatteryComponent::Right)),
        case,
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::bluetooth::aacp::BatteryStatus};

    fn entry(component: BatteryComponent, level: u8, status: BatteryStatus) -> BatteryInfo {
        BatteryInfo {
            component,
            level,
            status,
        }
    }

    fn level(percent: u8, charging: bool) -> Level {
        Level {
            percent: Some(percent),
            charging,
            stale: false,
        }
    }

    #[test]
    fn icon_rounds_to_the_nearest_ten() {
        assert_eq!(level(0, false).icon_name(), "battery-level-0-symbolic");
        assert_eq!(level(4, false).icon_name(), "battery-level-0-symbolic");
        assert_eq!(level(45, false).icon_name(), "battery-level-50-symbolic");
        assert_eq!(level(94, false).icon_name(), "battery-level-90-symbolic");
        assert_eq!(level(100, false).icon_name(), "battery-level-100-symbolic");
    }

    #[test]
    fn charging_icons_and_full_charge() {
        assert_eq!(
            level(52, true).icon_name(),
            "battery-level-50-charging-symbolic"
        );
        assert_eq!(
            level(96, true).icon_name(),
            "battery-level-100-charged-symbolic"
        );
        assert_eq!(Level::UNKNOWN.icon_name(), "battery-missing-symbolic");
    }

    #[test]
    fn text_is_percent_or_dash() {
        assert_eq!(level(7, true).text(), "7%");
        assert_eq!(Level::UNKNOWN.text(), "-");
    }

    #[test]
    fn disconnected_buds_are_unknown_not_charging() {
        let report = [
            entry(BatteryComponent::Left, 0, BatteryStatus::Disconnected),
            entry(BatteryComponent::Right, 60, BatteryStatus::Charging),
        ];
        let Batteries::Buds { left, right, case } = batteries(&report, None) else {
            panic!("earbuds expected");
        };
        assert_eq!(left, Level::UNKNOWN);
        assert_eq!(right, level(60, true));
        assert_eq!(case, Level::UNKNOWN);
    }

    #[test]
    fn silent_case_shows_the_last_level_as_stale() {
        let report = [entry(
            BatteryComponent::Case,
            255,
            BatteryStatus::Disconnected,
        )];
        let Batteries::Buds { case, .. } = batteries(&report, Some(70)) else {
            panic!("earbuds expected");
        };
        assert_eq!(
            case,
            Level {
                percent: Some(70),
                charging: false,
                stale: true
            }
        );
    }

    #[test]
    fn live_case_wins_over_the_last_level() {
        let report = [entry(BatteryComponent::Case, 40, BatteryStatus::Charging)];
        let Batteries::Buds { case, .. } = batteries(&report, Some(70)) else {
            panic!("earbuds expected");
        };
        assert_eq!(case, level(40, true));
    }

    #[test]
    fn headphones_show_one_level() {
        let report = [entry(
            BatteryComponent::Headphone,
            30,
            BatteryStatus::NotCharging,
        )];
        let shown = batteries(&report, Some(90));
        assert_eq!(shown, Batteries::Headphone(level(30, false)));
        assert_eq!(shown.labeled(), vec![("Battery", level(30, false))]);
    }
}
