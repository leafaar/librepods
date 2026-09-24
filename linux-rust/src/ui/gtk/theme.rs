//! The color scheme choice offered in the settings page and how it maps onto
//! the theme stored in AppSettings.

use crate::utils::MyTheme;

/// Light or dark preference. System follows the desktop setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemePreference {
    System,
    Light,
    Dark,
}

/// Stored themes read as Dark. Every other name that is not "Light" reads as
/// System, so a light variant from the old theme list follows the desktop.
const DARK_THEMES: [&str; 14] = [
    "Dark",
    "Dracula",
    "Nord",
    "CatppuccinFrappe",
    "CatppuccinMacchiato",
    "CatppuccinMocha",
    "TokyoNight",
    "TokyoNightStorm",
    "KanagawaWave",
    "KanagawaDragon",
    "Moonfly",
    "Nightfly",
    "Oxocarbon",
    "Ferra",
];

impl ThemePreference {
    /// Options in the order the settings page lists them.
    pub(crate) const ALL: [ThemePreference; 3] = [Self::System, Self::Light, Self::Dark];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    /// Reads a stored theme name leniently: "Light" is Light, names of dark
    /// themes (anything with "Dark" in it included) are Dark, anything else,
    /// including names this version does not know, is System.
    pub(crate) fn from_name(name: &str) -> Self {
        if name == "Light" {
            Self::Light
        } else if name.contains("Dark") || DARK_THEMES.contains(&name) {
            Self::Dark
        } else {
            Self::System
        }
    }

    pub(crate) fn from_stored(theme: MyTheme) -> Self {
        Self::from_name(&format!("{theme:?}"))
    }

    /// The value written to AppSettings. The stored type still lists the old
    /// named themes and has no System entry, so System is written as a light
    /// variant that `from_name` reads back as System.
    pub(crate) fn to_stored(self) -> MyTheme {
        match self {
            Self::System => MyTheme::SolarizedLight,
            Self::Light => MyTheme::Light,
            Self::Dark => MyTheme::Dark,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_is_light_and_dark_names_are_dark() {
        assert_eq!(ThemePreference::from_name("Light"), ThemePreference::Light);
        for name in [
            "Dark",
            "SolarizedDark",
            "GruvboxDark",
            "Nord",
            "CatppuccinMocha",
        ] {
            assert_eq!(
                ThemePreference::from_name(name),
                ThemePreference::Dark,
                "{name}"
            );
        }
    }

    #[test]
    fn other_and_unknown_names_follow_the_system() {
        for name in [
            "SolarizedLight",
            "CatppuccinLatte",
            "KanagawaLotus",
            "Ferra2",
            "",
        ] {
            assert_eq!(
                ThemePreference::from_name(name),
                ThemePreference::System,
                "{name}"
            );
        }
    }

    #[test]
    fn every_old_theme_maps_to_a_preference() {
        assert_eq!(
            ThemePreference::from_stored(MyTheme::Light),
            ThemePreference::Light
        );
        assert_eq!(
            ThemePreference::from_stored(MyTheme::Dark),
            ThemePreference::Dark
        );
        assert_eq!(
            ThemePreference::from_stored(MyTheme::TokyoNight),
            ThemePreference::Dark
        );
        assert_eq!(
            ThemePreference::from_stored(MyTheme::TokyoNightLight),
            ThemePreference::System
        );
    }

    #[test]
    fn stored_value_reads_back_as_the_same_preference() {
        for preference in ThemePreference::ALL {
            assert_eq!(
                ThemePreference::from_stored(preference.to_stored()),
                preference
            );
        }
    }
}
