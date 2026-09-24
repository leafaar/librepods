use {
    crate::devices::enums::{DeviceData, DeviceType},
    aes::{
        Aes128,
        cipher::{Array, BlockCipherEncrypt, KeyInit},
    },
    serde::{Deserialize, Serialize},
    std::{
        collections::HashMap,
        io,
        path::{Path, PathBuf},
        sync::{Mutex, PoisonError},
    },
    thiserror::Error,
    tracing::{error, info},
};

/// A failure reading or writing one of the JSON files under the config and
/// data directories. The cause is part of the message because callers log
/// these with `{}` and nothing else.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("could not read {}: {err}", path.display())]
    Read { path: PathBuf, err: io::Error },
    #[error("{} is not valid JSON: {err}", path.display())]
    Parse {
        path: PathBuf,
        err: serde_json::Error,
    },
    #[error("could not encode {}: {err}", path.display())]
    Encode {
        path: PathBuf,
        err: serde_json::Error,
    },
    #[error("could not write {}: {err}", path.display())]
    Write { path: PathBuf, err: io::Error },
}

pub fn get_devices_path() -> PathBuf {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .unwrap_or_else(|_| format!("{}/.local/share", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(data_dir)
        .join("librepods")
        .join("devices.json")
}

pub fn ensure_device_registered(mac: &str, name: &str, type_: DeviceType) {
    let result = update_devices_file(|devices| {
        if devices.contains_key(mac) {
            return;
        }
        info!("Registering device {} ({}) as {:?}", name, mac, type_);
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
        error!("Failed to register device {}: {}", mac, e);
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

    let config_dir = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{home}/.config"));

    let data_dir =
        std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));

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

pub fn ah(k: &[u8; 16], r: [u8; 3]) -> [u8; 3] {
    let mut r_padded = [0u8; 16];
    r_padded[..3].copy_from_slice(&r);
    let encrypted = e(k, &r_padded);
    let mut hash = [0u8; 3];
    hash.copy_from_slice(&encrypted[..3]);
    hash
}

/// Light or dark appearance. System follows the desktop preference, so a
/// fresh install matches GNOME's light or dark setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

/// Names from the old theme list that read as Light, besides those
/// containing "Light".
const LIGHT_THEME_NAMES: [&str; 2] = ["CatppuccinLatte", "KanagawaLotus"];

/// Names from the old theme list that read as Dark, besides those containing
/// "Dark".
const DARK_THEME_NAMES: [&str; 13] = [
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
    /// Options in the order the settings pages list them.
    pub const ALL: [ThemePreference; 3] = [Self::System, Self::Light, Self::Dark];

    /// Reads a stored name leniently. Settings files written before this type
    /// hold a name from the old theme list: light variants read as Light, dark
    /// ones as Dark. Any other name, including one from a newer version, reads
    /// as System instead of failing the whole settings file.
    fn from_name(name: &str) -> Self {
        match name {
            "System" => Self::System,
            _ if name.contains("Light") || LIGHT_THEME_NAMES.contains(&name) => Self::Light,
            _ if name.contains("Dark") || DARK_THEME_NAMES.contains(&name) => Self::Dark,
            _ => Self::System,
        }
    }
}

impl<'de> Deserialize<'de> for ThemePreference {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Ok(Self::from_name(&name))
    }
}

impl std::fmt::Display for ThemePreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        })
    }
}

// Each flag is an independent user toggle stored as its own JSON field; folding
// them into an enum or bit set would change the settings file format.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub theme: ThemePreference,
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
            theme: ThemePreference::System,
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
    /// Settings from the default location. A missing or unreadable file gives
    /// the defaults, and fields missing from the file take their default.
    pub fn load() -> Self {
        SettingsStore::default_location().load().unwrap_or_default()
    }

    /// Save to the default location, logging a failure.
    pub fn save(&self) {
        if let Err(e) = SettingsStore::default_location().save(self) {
            error!("Failed to save app settings: {}", e);
        }
    }
}

/// Where the app settings live. Tests point it at a temporary directory.
#[derive(Clone, Debug)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The settings file under XDG_CONFIG_HOME, migrated from the old location
    /// on first use.
    pub fn default_location() -> Self {
        Self::new(get_app_settings_path())
    }

    /// A missing file is not an error: it gives the defaults.
    pub fn load(&self) -> Result<AppSettings, StorageError> {
        match read_json(&self.path)? {
            Some(settings) => Ok(settings),
            None => Ok(AppSettings::default()),
        }
    }

    pub fn save(&self, settings: &AppSettings) -> Result<(), StorageError> {
        let json = serde_json::to_string_pretty(settings).map_err(|err| StorageError::Encode {
            path: self.path.clone(),
            err,
        })?;
        write_atomic(&self.path, &json)
    }
}

/// Serializes every read-modify-write of devices.json in this process. One lock
/// for every path: there is one devices file in production, and a test store
/// only waits a moment longer.
static DEVICES_FILE_LOCK: Mutex<()> = Mutex::new(());

/// The saved devices, keyed by MAC. Tests point it at a temporary directory.
#[derive(Clone, Debug)]
pub struct DevicesStore {
    path: PathBuf,
}

impl DevicesStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_location() -> Self {
        Self::new(get_devices_path())
    }

    /// A missing file is not an error: it means no device was saved yet.
    pub fn load(&self) -> Result<HashMap<String, DeviceData>, StorageError> {
        Ok(read_json(&self.path)?.unwrap_or_default())
    }

    /// Read the file, let `f` change it, and write it back atomically. Every
    /// writer goes through here so no one overwrites the file from a stale
    /// copy. A file that does not parse is left alone: starting from an empty
    /// map would silently drop every saved device.
    pub fn update(
        &self,
        f: impl FnOnce(&mut HashMap<String, DeviceData>),
    ) -> Result<(), StorageError> {
        // The guarded data is (), so a poisoned lock carries no broken state.
        let _guard = DEVICES_FILE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut devices = self.load()?;
        f(&mut devices);
        let json = serde_json::to_string(&devices).map_err(|err| StorageError::Encode {
            path: self.path.clone(),
            err,
        })?;
        write_atomic(&self.path, &json)
    }
}

/// Update devices.json in its default location, see [`DevicesStore::update`].
pub fn update_devices_file(
    f: impl FnOnce(&mut HashMap<String, DeviceData>),
) -> Result<(), StorageError> {
    DevicesStore::default_location().update(f)
}

/// Parse the JSON file at `path`; Ok(None) when it does not exist.
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, StorageError> {
    let json = match std::fs::read_to_string(path) {
        Ok(json) => json,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(StorageError::Read {
                path: path.to_path_buf(),
                err,
            });
        },
    };
    serde_json::from_str(&json)
        .map(Some)
        .map_err(|err| StorageError::Parse {
            path: path.to_path_buf(),
            err,
        })
}

/// Write to a temp file and rename it into place, so a crash mid-write cannot
/// leave a truncated file, and a concurrent reader sees the old or the new
/// contents, never half of one. Creates the parent directory if needed.
fn write_atomic(path: &Path, contents: &str) -> Result<(), StorageError> {
    let write_err = |err| StorageError::Write {
        path: path.to_path_buf(),
        err,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(write_err)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, contents).map_err(write_err)?;
    std::fs::rename(&tmp, path).map_err(write_err)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::devices::enums::{DeviceData, DeviceType},
        tempfile::TempDir,
    };

    fn airpods(name: &str) -> DeviceData {
        DeviceData {
            name: name.to_string(),
            type_: DeviceType::AirPods,
            information: None,
        }
    }

    fn devices_store(dir: &TempDir) -> DevicesStore {
        DevicesStore::new(dir.path().join("librepods").join("devices.json"))
    }

    #[test]
    fn missing_devices_file_starts_empty_and_update_creates_it() {
        let dir = TempDir::new().unwrap();
        let store = devices_store(&dir);

        assert!(store.load().unwrap().is_empty());
        store
            .update(|devices| {
                devices.insert("AA:BB:CC:DD:EE:FF".to_string(), airpods("Pods"));
            })
            .unwrap();

        let devices = store.load().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices["AA:BB:CC:DD:EE:FF"].name, "Pods");
    }

    #[test]
    fn update_keeps_other_devices() {
        let dir = TempDir::new().unwrap();
        let store = devices_store(&dir);
        store
            .update(|d| {
                d.insert("01".to_string(), airpods("One"));
            })
            .unwrap();

        store
            .update(|d| {
                d.insert("02".to_string(), airpods("Two"));
            })
            .unwrap();

        let devices = store.load().unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices["01"].name, "One");
    }

    #[test]
    fn unparsable_devices_file_is_not_overwritten() {
        let dir = TempDir::new().unwrap();
        let store = devices_store(&dir);
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(&store.path, "{ not json").unwrap();
        let mut called = false;

        let result = store.update(|_| called = true);

        assert!(matches!(result, Err(StorageError::Parse { .. })));
        assert!(!called);
        assert_eq!(std::fs::read_to_string(&store.path).unwrap(), "{ not json");
    }

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_temp_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sub").join("file.json");
        write_atomic(&path, "old").unwrap();

        write_atomic(&path, "new").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("file.json")]);
    }

    #[test]
    fn write_error_names_the_path() {
        let dir = TempDir::new().unwrap();
        // A regular file where the parent directory should be.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "").unwrap();
        let path = blocker.join("file.json");

        let err = write_atomic(&path, "x").unwrap_err();

        assert!(matches!(err, StorageError::Write { .. }));
        assert!(err.to_string().contains("blocker"), "{err}");
    }

    #[test]
    fn missing_settings_file_gives_defaults() {
        let dir = TempDir::new().unwrap();
        let store = SettingsStore::new(dir.path().join("app_settings.json"));

        let settings = store.load().unwrap();

        assert!(settings.auto_switch_on_playback);
        assert_eq!(settings.preferred_codec, PreferredCodec::Aac);
    }

    #[test]
    fn settings_round_trip() {
        let dir = TempDir::new().unwrap();
        let store = SettingsStore::new(dir.path().join("cfg").join("app_settings.json"));
        let settings = AppSettings {
            theme: ThemePreference::Light,
            hires_mic_agc: false,
            preferred_codec: PreferredCodec::SbcXq,
            ..AppSettings::default()
        };

        store.save(&settings).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(loaded.theme, ThemePreference::Light);
        assert!(!loaded.hires_mic_agc);
        assert_eq!(loaded.preferred_codec, PreferredCodec::SbcXq);
    }

    #[test]
    fn settings_fields_missing_from_the_file_take_their_default() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("app_settings.json");
        std::fs::write(&path, r#"{"theme":"Nord","a2dp_reset":false}"#).unwrap();

        let settings = SettingsStore::new(path).load().unwrap();

        assert_eq!(settings.theme, ThemePreference::Dark);
        assert!(!settings.a2dp_reset);
        assert!(settings.hires_mic_enabled);
        assert_eq!(settings.preferred_codec, PreferredCodec::Aac);
    }

    #[test]
    fn fresh_settings_follow_the_system_theme() {
        assert_eq!(AppSettings::default().theme, ThemePreference::System);
    }

    #[track_caller]
    fn stored_theme(name: &str) -> ThemePreference {
        let json = format!(r#"{{"theme":"{name}","tray_text_mode":true}}"#);
        let settings: AppSettings = serde_json::from_str(&json).unwrap();
        // The rest of the file still loads.
        assert!(settings.tray_text_mode, "{name}");
        settings.theme
    }

    #[test]
    fn old_theme_names_map_to_light_or_dark() {
        for name in [
            "Light",
            "SolarizedLight",
            "GruvboxLight",
            "TokyoNightLight",
            "CatppuccinLatte",
            "KanagawaLotus",
        ] {
            assert_eq!(stored_theme(name), ThemePreference::Light, "{name}");
        }
        for name in [
            "Dark",
            "SolarizedDark",
            "GruvboxDark",
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
        ] {
            assert_eq!(stored_theme(name), ThemePreference::Dark, "{name}");
        }
    }

    #[test]
    fn unknown_theme_names_follow_the_system() {
        for name in ["System", "HighContrast", "light", ""] {
            assert_eq!(stored_theme(name), ThemePreference::System, "{name}");
        }
    }

    #[test]
    fn theme_preference_round_trips() {
        for theme in ThemePreference::ALL {
            let json = serde_json::to_string(&theme).unwrap();
            assert_eq!(json, format!("\"{theme}\""));
            assert_eq!(
                serde_json::from_str::<ThemePreference>(&json).unwrap(),
                theme
            );
        }
    }

    #[test]
    fn unparsable_settings_file_is_a_parse_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("app_settings.json");
        std::fs::write(&path, r#"{"theme": 5}"#).unwrap();

        let result = SettingsStore::new(path).load();

        assert!(matches!(result, Err(StorageError::Parse { .. })));
    }

    #[test]
    fn codec_profile_order_puts_the_preferred_codec_first() {
        assert_eq!(
            PreferredCodec::Aac.profile_order(),
            ["a2dp-sink", "a2dp-sink-sbc_xq", "a2dp-sink-sbc"]
        );
        assert_eq!(
            PreferredCodec::SbcXq.profile_order(),
            ["a2dp-sink-sbc_xq", "a2dp-sink", "a2dp-sink-sbc"]
        );
        assert_eq!(
            PreferredCodec::Sbc.profile_order(),
            ["a2dp-sink-sbc", "a2dp-sink", "a2dp-sink-sbc_xq"]
        );
    }
}
