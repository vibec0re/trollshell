//! Async clients to system daemons exposed as hytte services.

/// The shared `~/.config/trollshell/*` persistence boilerplate — including the
/// workspace's one copy of the atomic tmp + `fsync` + `rename(2)` replacement
/// (#733/#739).
///
/// It moved to the GTK-free `hytte-config` leaf crate in #640, so
/// `trollshell-control-center`'s places editor could share this crate's
/// `places.toml` writer without linking `hytte-services` (and with it
/// libpipewire and evolution-data-server). Aliased here so every in-crate call
/// site still reads `config_file::…`.
pub(crate) use hytte_config::file as config_file;

pub mod app_usage;
pub mod audio_native;
pub mod bluetooth;
pub mod bluetooth_audio;
pub mod brightness;
pub mod calendar;
mod cast;
pub mod clipboard;
pub mod clock;
pub mod departures;
pub mod display_config;
pub mod displays;
pub mod dnd;
mod eds_retry;
pub mod fullscreen_inhibit;
pub mod geoclue;
pub mod hooks;
pub mod idle_notify;
pub mod logind;
pub mod mpris;
pub mod netconn;
pub mod networkd;
mod networkd_nm;
pub mod nightlight;
pub mod niri;
pub mod notifications;
pub mod notifications_mute;
pub mod pipewire;
pub mod places;
pub mod power_profiles;
pub mod recorder;
pub mod resolved;
mod retry;
pub mod screensaver;
pub mod sensors;
pub mod systemd;
pub mod tasks;
pub mod theme;
pub mod tray;
pub mod upower;
pub mod vpn;
pub mod wallpaper;
pub mod weather;
pub mod wifi;
pub mod wifi_backend;
pub mod wifi_nm;
pub mod wifiscan;
