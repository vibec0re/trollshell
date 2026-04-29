//! Drawer pages mounted into `modal.rs`'s per-monitor `gtk::Stack`.
//! Each page is one `pub fn panel_<name>() -> gtk::Widget` re-exported
//! at the module root. Per-panel private helpers stay in their owning
//! file. Phase 3 of the reorg fills this out one panel at a time;
//! after the first move, this module will list `pub mod <name>;`
//! lines and re-exports.
