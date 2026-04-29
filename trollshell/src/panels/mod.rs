//! Drawer pages mounted into `modal.rs`'s per-monitor `gtk::Stack`.
//! Each page is one `pub fn panel_<name>() -> gtk::Widget` re-exported
//! at the module root. Per-panel private helpers stay in their owning
//! file.

pub mod appearance;
pub mod audio;

pub use appearance::panel_appearance;
pub use audio::panel_audio;
