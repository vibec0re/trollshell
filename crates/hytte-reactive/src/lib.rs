//! Bridge crate between GTK4's main loop and the `futures-signals` reactive
//! primitives, plus the hytte service registry. Service modules in
//! `hytte-services` register typed handles here at startup; widgets in
//! `hytte-ui` subscribe to them via `bind`.

pub mod runtime;
