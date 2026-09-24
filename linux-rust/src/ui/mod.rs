#[expect(
    dead_code,
    reason = "iced UI kept as the reference for the GTK port, deleted after it"
)]
mod airpods;
#[expect(
    dead_code,
    reason = "iced UI kept as the reference for the GTK port, deleted after it"
)]
mod airpods_settings;
mod connect;
#[expect(
    dead_code,
    reason = "iced UI kept as the reference for the GTK port, deleted after it"
)]
pub(crate) mod equalizer;
mod format;
pub mod gtk;
pub mod messages;
#[expect(
    dead_code,
    reason = "iced UI kept as the reference for the GTK port, deleted after it"
)]
mod nothing;
pub mod tray;
mod tray_icon;
#[expect(
    dead_code,
    reason = "iced UI kept as the reference for the GTK port, deleted after it"
)]
pub mod window;
