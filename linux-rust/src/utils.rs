use aes::Aes128;
use aes::cipher::Array;
use aes::cipher::{BlockCipherEncrypt, KeyInit};
use iced::Theme;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub fn get_devices_path() -> PathBuf {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .unwrap_or_else(|_| format!("{}/.local/share", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(data_dir)
        .join("librepods")
        .join("devices.json")
}

pub fn ensure_device_registered(mac: &str, name: &str, type_: crate::devices::enums::DeviceType) {
    use crate::devices::enums::DeviceData;

    let result = update_devices_file(|devices| {
        if devices.contains_key(mac) {
            return;
        }
        tracing::info!("Registering device {} ({}) as {:?}", name, mac, type_);
        devices.insert(
            mac.to_string(),
            DeviceData {
                name: name.to_string(),
                type_,
                information: None,
            },
        );
    });
    if let Err(e) = result {
        tracing::error!("Failed to register device {}: {}", mac, e);
    }
}

pub fn get_preferences_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .unwrap_or_else(|_| format!("{}/.config", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(config_dir)
        .join("librepods")
        .join("preferences.json")
}

pub fn get_app_settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();

    let config_dir =
        std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{}/.config", home));

    let data_dir =
        std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{}/.local/share", home));

    let new_path = PathBuf::from(&config_dir)
        .join("librepods")
        .join("app_settings.json");

    let old_path = PathBuf::from(&data_dir).join("app_settings.json");

    // migrate if needed
    if old_path.exists() && !new_path.exists() {
        if let Some(parent) = new_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        if std::fs::copy(&old_path, &new_path).is_ok() {
            let _ = std::fs::remove_file(&old_path);
        }
    }

    new_path
}

/// Preferred A2DP codec. The chosen one is tried first, the rest act as fallbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PreferredCodec {
    #[default]
    Aac,
    SbcXq,
    Sbc,
}

impl PreferredCodec {
    pub const ALL: [PreferredCodec; 3] = [Self::Aac, Self::SbcXq, Self::Sbc];

    /// PulseAudio/PipeWire card profile name for this codec.
    pub fn profile_name(self) -> &'static str {
        match self {
            Self::Aac => "a2dp-sink",
            Self::SbcXq => "a2dp-sink-sbc_xq",
            Self::Sbc => "a2dp-sink-sbc",
        }
    }

    /// Profiles to try, most preferred first.
    pub fn profile_order(self) -> Vec<&'static str> {
        std::iter::once(self.profile_name())
            .chain(
                Self::ALL
                    .iter()
                    .filter(|c| **c != self)
                    .map(|c| c.profile_name()),
            )
            .collect()
    }
}

impl std::fmt::Display for PreferredCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Aac => "AAC",
            Self::SbcXq => "SBC-XQ",
            Self::Sbc => "SBC",
        })
    }
}

fn e(key: &[u8; 16], data: &[u8; 16]) -> [u8; 16] {
    let mut swapped_key = *key;
    swapped_key.reverse();
    let mut swapped_data = *data;
    swapped_data.reverse();
    let cipher = Aes128::new(&Array::from(swapped_key));
    let mut block = Array::from(swapped_data);
    cipher.encrypt_block(&mut block);
    let mut result: [u8; 16] = block.into();
    result.reverse();
    result
}

pub fn ah(k: &[u8; 16], r: &[u8; 3]) -> [u8; 3] {
    let mut r_padded = [0u8; 16];
    r_padded[..3].copy_from_slice(r);
    let encrypted = e(k, &r_padded);
    let mut hash = [0u8; 3];
    hash.copy_from_slice(&encrypted[..3]);
    hash
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MyTheme {
    Light,
    Dark,
    Dracula,
    Nord,
    SolarizedLight,
    SolarizedDark,
    GruvboxLight,
    GruvboxDark,
    CatppuccinLatte,
    CatppuccinFrappe,
    CatppuccinMacchiato,
    CatppuccinMocha,
    TokyoNight,
    TokyoNightStorm,
    TokyoNightLight,
    KanagawaWave,
    KanagawaDragon,
    KanagawaLotus,
    Moonfly,
    Nightfly,
    Oxocarbon,
    Ferra,
}

impl std::fmt::Display for MyTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
            Self::SolarizedLight => "Solarized Light",
            Self::SolarizedDark => "Solarized Dark",
            Self::GruvboxLight => "Gruvbox Light",
            Self::GruvboxDark => "Gruvbox Dark",
            Self::CatppuccinLatte => "Catppuccin Latte",
            Self::CatppuccinFrappe => "Catppuccin Frappé",
            Self::CatppuccinMacchiato => "Catppuccin Macchiato",
            Self::CatppuccinMocha => "Catppuccin Mocha",
            Self::TokyoNight => "Tokyo Night",
            Self::TokyoNightStorm => "Tokyo Night Storm",
            Self::TokyoNightLight => "Tokyo Night Light",
            Self::KanagawaWave => "Kanagawa Wave",
            Self::KanagawaDragon => "Kanagawa Dragon",
            Self::KanagawaLotus => "Kanagawa Lotus",
            Self::Moonfly => "Moonfly",
            Self::Nightfly => "Nightfly",
            Self::Oxocarbon => "Oxocarbon",
            Self::Ferra => "Ferra",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub theme: MyTheme,
    pub tray_text_mode: bool,
    pub stem_control: bool,
    pub hires_mic_enabled: bool,
    pub hires_mic_agc: bool,
    pub hires_mic_pause_convo: bool,
    pub a2dp_reset: bool,
    /// Connect the AirPods to this PC when local media starts playing, taking
    /// them from the phone like an Apple device does.
    pub auto_switch_on_playback: bool,
    /// A2DP codec activated for playback; the others are fallbacks.
    pub preferred_codec: PreferredCodec,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: MyTheme::Dark,
            tray_text_mode: false,
            stem_control: false,
            hires_mic_enabled: true,
            hires_mic_agc: true,
            hires_mic_pause_convo: true,
            a2dp_reset: true,
            auto_switch_on_playback: true,
            preferred_codec: PreferredCodec::default(),
        }
    }
}

impl AppSettings {
    pub fn load() -> Self {
        std::fs::read_to_string(get_app_settings_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = get_app_settings_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = write_atomic(&path, &json) {
                    tracing::error!("Failed to write app settings: {}", e);
                }
            }
            Err(e) => tracing::error!("Failed to serialize app settings: {}", e),
        }
    }
}

impl From<MyTheme> for Theme {
    fn from(my_theme: MyTheme) -> Self {
        match my_theme {
            MyTheme::Light => Theme::Light,
            MyTheme::Dark => Theme::Dark,
            MyTheme::Dracula => Theme::Dracula,
            MyTheme::Nord => Theme::Nord,
            MyTheme::SolarizedLight => Theme::SolarizedLight,
            MyTheme::SolarizedDark => Theme::SolarizedDark,
            MyTheme::GruvboxLight => Theme::GruvboxLight,
            MyTheme::GruvboxDark => Theme::GruvboxDark,
            MyTheme::CatppuccinLatte => Theme::CatppuccinLatte,
            MyTheme::CatppuccinFrappe => Theme::CatppuccinFrappe,
            MyTheme::CatppuccinMacchiato => Theme::CatppuccinMacchiato,
            MyTheme::CatppuccinMocha => Theme::CatppuccinMocha,
            MyTheme::TokyoNight => Theme::TokyoNight,
            MyTheme::TokyoNightStorm => Theme::TokyoNightStorm,
            MyTheme::TokyoNightLight => Theme::TokyoNightLight,
            MyTheme::KanagawaWave => Theme::KanagawaWave,
            MyTheme::KanagawaDragon => Theme::KanagawaDragon,
            MyTheme::KanagawaLotus => Theme::KanagawaLotus,
            MyTheme::Moonfly => Theme::Moonfly,
            MyTheme::Nightfly => Theme::Nightfly,
            MyTheme::Oxocarbon => Theme::Oxocarbon,
            MyTheme::Ferra => Theme::Ferra,
        }
    }
}

/// Serializes every read-modify-write of devices.json in this process.
static DEVICES_FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Read devices.json, let `f` change it, and write it back atomically. Every
/// writer goes through here so no one overwrites the file from a stale copy.
pub fn update_devices_file(
    f: impl FnOnce(&mut std::collections::HashMap<String, crate::devices::enums::DeviceData>),
) -> std::io::Result<()> {
    let _guard = DEVICES_FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = get_devices_path();
    let mut devices = match std::fs::read_to_string(&path) {
        // Refuse to write over a file we cannot parse: starting from an empty
        // map here would silently drop every saved device.
        Ok(json) => serde_json::from_str(&json).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not valid, not overwriting it: {e}", path.display()),
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(e) => return Err(e),
    };
    f(&mut devices);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(&path, &serde_json::to_string(&devices)?)
}

/// Write to a temp file and rename it into place, so a crash mid-write cannot
/// leave a truncated file, and a concurrent reader sees the old or the new
/// contents, never half of one.
fn write_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}
