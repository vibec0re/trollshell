//! GTK4 + libadwaita + gtk4-layer-shell window primitives.

mod app;
mod bar;
pub(crate) mod cast;
mod error;
mod layer_window;
mod monitor;
mod popup;
pub mod sparkline;

pub use app::{App, AppBuilder};
pub use bar::{Bar, BarHandle, Edge};
pub use error::{Error, Result};
pub use layer_window::{Anchor, LayerWindowBuilder, Margin, layer_window};
pub use monitor::Monitor;
pub use popup::{Popup, PopupBuilder, Position as PopupPosition, attach_dismiss_catcher};
pub use sparkline::Sparkline;

// `Edge` re-exported as `LayerEdge` to avoid colliding with `bar::Edge`.
pub use gtk4_layer_shell::{Edge as LayerEdge, KeyboardMode, Layer, LayerShell};

// Re-export so consumers don't need their own gtk/adw deps.
pub use ::adw;
pub use ::gtk;
