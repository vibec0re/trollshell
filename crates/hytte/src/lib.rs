//! Library-first toolkit for composing GTK4 + Wayland desktop shells. This
//! crate just re-exports `hytte_ui`, `hytte_reactive`, and `hytte_services`
//! under shorter module paths.

pub use hytte_reactive as reactive;
pub use hytte_services as services;
pub use hytte_ui as ui;
