mod airpods;
mod airpods_settings;
mod connect;
pub(crate) mod equalizer;
mod format;
#[expect(dead_code, unused_imports, reason = "main.rs still starts the iced UI")]
pub mod gtk;
pub mod messages;
mod nothing;
pub mod tray;
mod tray_icon;
pub mod window;
