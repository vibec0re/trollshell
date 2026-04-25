//! Bridge crate between GTK4's main loop and the `futures-signals` reactive
//! primitives, plus the hytte service registry.

pub mod bind;
pub mod registry;
pub mod runtime;

pub use bind::{bind, bind_class, bind_text, bind_two_way, bind_visible};
pub use registry::{Registry, Service, ServiceErased};

// Re-export so consumers don't need their own dep on futures-signals.
pub use ::futures_signals;
