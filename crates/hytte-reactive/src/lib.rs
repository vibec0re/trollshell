//! Bridge crate between GTK4's main loop and the `futures-signals` reactive
//! primitives, plus the hytte service registry.

pub mod bind;
pub mod health;
pub mod pending;
pub mod poll;
pub mod registry;
pub mod runtime;
pub mod shared;
pub mod supervisor;
pub mod test_lock;

pub use bind::{bind, bind_class, bind_text, bind_two_way, bind_two_way_drag_safe, bind_visible};
pub use health::{TaskHealth, TaskId, TaskState};
pub use pending::Pending;
pub use poll::gated_poll;
pub use registry::{Registry, Service, ServiceErased};
pub use supervisor::{
    SupervisorHandle, install_panic_hook, spawn_supervised, spawn_supervised_blocking,
    spawn_supervised_handle,
};

// Re-export so consumers don't need their own dep on futures-signals.
pub use ::futures_signals;
