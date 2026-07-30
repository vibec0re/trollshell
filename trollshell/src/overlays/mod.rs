//! Per-monitor layer-shell overlays — consent prompts, the fullscreen frame
//! indicator, notification toasts, OSD, the Wi-Fi/VPN password prompt, and the
//! plugin sidebar.
//! Each module exposes a `pub fn install(...)` that wires the overlay
//! to the relevant signal source. Moved out of `widgets/` so that
//! `widgets/` reads strictly as bar chips.

pub mod consent;
pub mod frame;
pub mod notifications;
pub mod osd;
pub mod prompt;
pub mod sidebar;
