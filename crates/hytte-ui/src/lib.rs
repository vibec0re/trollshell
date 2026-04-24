//! GTK4 + libadwaita + gtk4-layer-shell window primitives. Provides the
//! `App` entry point and (filled in by later tasks) `Bar` / `LayerWindow`
//! builders.

mod app;
mod error;
mod monitor;

pub use app::{App, AppBuilder};
pub use error::{Error, Result};
pub use monitor::Monitor;

/// Default stylesheet, replaced with real content in Task 8.
pub(crate) const DEFAULT_STYLESHEET: &str = "";
