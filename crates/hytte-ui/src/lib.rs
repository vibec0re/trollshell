//! GTK4 + libadwaita + gtk4-layer-shell window primitives.

mod app;
mod bar;
mod error;
mod layer_window;
mod monitor;
mod popup;
pub mod sparkline;

pub use app::{App, AppBuilder};
pub use bar::{Bar, BarHandle, Edge};
pub use error::{Error, Result};
pub use layer_window::{layer_window, Anchor, LayerWindowBuilder, Margin};
pub use monitor::Monitor;
pub use popup::{Popup, PopupBuilder, Position as PopupPosition};
pub use sparkline::Sparkline;

// `Edge` re-exported as `LayerEdge` to avoid colliding with `bar::Edge`.
pub use gtk4_layer_shell::{Edge as LayerEdge, KeyboardMode, Layer, LayerShell};

// Re-export the ext-session-lock-v1 crate so the lock-screen overlay
// doesn't need its own dep. Used only by `trollshell::overlays::lock_screen`.
pub use ::gtk4_session_lock as session_lock;

// Re-export so consumers don't need their own gtk/adw deps.
pub use ::adw;
pub use ::gtk;

pub(crate) const DEFAULT_STYLESHEET: &str = include_str!("style.css");
