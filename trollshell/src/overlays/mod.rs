//! Per-monitor layer-shell overlays — lock screen, OSD, dialogs, toast.
//! Each module exposes a `pub fn install(...)` that wires the overlay
//! to the relevant signal source. Moved out of `widgets/` so that
//! `widgets/` reads strictly as bar chips.

pub mod lock_screen;
pub mod notifications;
pub mod osd;
pub mod polkit_dialog;
pub mod prompt;
