//! Drawer pages mounted into `modal.rs`'s per-monitor `gtk::Stack`.
//! Each page is one `pub fn panel_<name>() -> gtk::Widget` re-exported
//! at the module root. Per-panel private helpers stay in their owning
//! file.

pub mod appearance;
pub mod audio;
pub mod bluetooth;
pub mod calendar;
pub mod clipboard;
pub mod connections;
pub mod displays;
pub mod media;
pub mod network;
pub mod notifications;
pub mod power;
pub mod power_menu;
pub mod settings;
pub mod stats;
pub mod vpn;

pub use appearance::panel_appearance;
pub use audio::panel_audio;
pub use bluetooth::panel_bluetooth;
pub use calendar::panel_calendar;
pub use clipboard::panel_clipboard;
pub use connections::panel_connections;
pub use displays::panel_displays;
pub use media::panel_media;
pub use network::panel_network;
pub use notifications::panel_notifications;
pub use power::panel_power;
pub use power_menu::panel_power_menu;
pub use settings::panel_settings;
pub use stats::{
    panel_stats_cpu, panel_stats_disks, panel_stats_gpu, panel_stats_memory, panel_stats_services,
};
pub use vpn::panel_vpn;
