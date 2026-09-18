//! Per-monitor layer-shell overlays — consent prompts, the plugin dialog, the
//! fullscreen frame indicator, notification toasts, OSD, the Wi-Fi/VPN password
//! prompt, and the plugin sidebar.
//! Each module exposes a `pub fn install(...)` that wires the overlay
//! to the relevant signal source. Moved out of `widgets/` so that
//! `widgets/` reads strictly as bar chips.

pub mod consent;
// `dialog` (#1010) — like `consent`, raised on demand by the effect broker
// rather than driven by a service signal, and `install`ed per monitor only to
// keep the connector map its single window resolves the focused output against.
// (A plain comment, not a doc one: the module's own `//!` header documents it,
// and an outer doc here would have its intra-doc links resolved in *this*
// module's scope rather than in `dialog`'s.)
pub mod dialog;
pub mod frame;
pub mod notifications;
pub mod osd;
pub mod prompt;
pub mod sidebar;
