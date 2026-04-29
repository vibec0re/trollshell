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
