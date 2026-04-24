//! Library-first toolkit for composing GTK4 + Wayland desktop shells. This
//! crate just re-exports `hytte_ui`, `hytte_reactive`, and `hytte_services`
//! under shorter module paths.

pub use hytte_reactive as reactive;
pub use hytte_services as services;
pub use hytte_ui as ui;

// Re-export third-party crates shells need so they don't take their own deps.
pub use hytte_reactive::futures_signals;
pub use hytte_ui::{adw, gtk};

/// Convenience re-exports for shell binaries:
///
/// ```ignore
/// use hytte::prelude::*;
/// use hytte::gtk::{self, prelude::*};
/// ```
pub mod prelude {
    pub use hytte_reactive::futures_signals::signal::SignalExt;
    pub use hytte_reactive::{bind, bind_class, bind_text, bind_visible, Service};
    pub use hytte_ui::{
        App, Anchor, Bar, BarHandle, Edge, KeyboardMode, Layer, Margin, Monitor, Popup,
        PopupBuilder, PopupPosition,
    };
}
