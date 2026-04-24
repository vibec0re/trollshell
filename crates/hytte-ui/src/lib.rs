//! GTK4 + libadwaita + gtk4-layer-shell window primitives.

mod app;
mod bar;
mod error;
mod layer_window;
mod monitor;

pub use app::{App, AppBuilder};
pub use bar::{Bar, BarHandle, Edge};
pub use error::{Error, Result};
pub use layer_window::{layer_window, Anchor, LayerWindowBuilder, Margin};
pub use monitor::Monitor;

pub use gtk4_layer_shell::Layer;

pub(crate) const DEFAULT_STYLESHEET: &str = include_str!("style.css");
