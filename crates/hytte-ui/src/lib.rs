//! GTK4 + libadwaita + gtk4-layer-shell window primitives.

mod app;
mod error;
mod layer_window;
mod monitor;

pub use app::{App, AppBuilder};
pub use error::{Error, Result};
pub use layer_window::{layer_window, Anchor, LayerWindowBuilder, Margin};
pub use monitor::Monitor;

/// Default stylesheet, replaced with real content in Task 8.
pub(crate) const DEFAULT_STYLESHEET: &str = "";

// Re-export the layer-shell `Layer` enum so consumers can pick a layer
// without depending on `gtk4-layer-shell` directly.
pub use gtk4_layer_shell::Layer;
