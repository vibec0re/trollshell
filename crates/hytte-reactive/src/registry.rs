//! Typed, thread-local registry of service handles.
//!
//! Each `Service` produces a `Handles` value at startup (typically a struct
//! of `Mutable<T>` / `MutableVec<T>` from `futures-signals`). Handles are
//! stored keyed by their concrete type. Service free-functions in
//! `hytte-services` retrieve them via [`with`].
//!
//! The registry lives in a `thread_local!` because GTK is single-threaded —
//! widgets only subscribe from the main thread. Cross-thread updates from
//! tokio tasks happen by writing to the `Mutable` (which is `Send + Sync`)
//! that the handle holds; the registry itself is never crossed thread
//! boundaries.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;

/// A service that can be registered on an `App`.
pub trait Service: Sized + 'static {
    /// Bag of handles (typically `Mutable<T>` / `MutableVec<T>`) that
    /// widgets subscribe to.
    type Handles: 'static;

    /// Spawn background tasks on the supplied tokio handle and return the
    /// handle bag to be inserted in the registry.
    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles;
}

/// Type-erased shim used internally by `App` to store heterogeneous
/// services in a single `Vec`.
pub trait ServiceErased: 'static {
    fn start_erased(self: Box<Self>, rt: &tokio::runtime::Handle, registry: &mut Registry);
}

impl<S: Service> ServiceErased for S {
    fn start_erased(self: Box<Self>, rt: &tokio::runtime::Handle, registry: &mut Registry) {
        let handles = (*self).start(rt);
        registry.insert::<S::Handles>(handles);
    }
}

/// Storage for service handles, keyed by their concrete `TypeId`.
#[derive(Default)]
pub struct Registry {
    entries: HashMap<TypeId, Box<dyn Any>>,
}

impl Registry {
    pub fn insert<T: 'static>(&mut self, value: T) {
        self.entries.insert(TypeId::of::<T>(), Box::new(value));
    }

    pub fn get<T: 'static>(&self) -> Option<&T> {
        self.entries
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>())
    }
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

/// Run a closure with shared read access to the thread-local registry.
///
/// # Panics
/// Panics if called from a thread other than the one where services were
/// installed (typically the GTK main thread). In practice all subscriptions
/// happen on the main thread.
pub fn with<R>(f: impl FnOnce(&Registry) -> R) -> R {
    REGISTRY.with(|cell| f(&cell.borrow()))
}

/// Install a single service. Called by `App::run` once per registered
/// service before invoking the consumer's body closure.
pub fn install(service: Box<dyn ServiceErased>, rt: &tokio::runtime::Handle) {
    REGISTRY.with(|cell| {
        let mut reg = cell.borrow_mut();
        service.start_erased(rt, &mut reg);
    });
}

/// Wipe the registry — exposed for tests only.
#[doc(hidden)]
pub fn reset_for_tests() {
    REGISTRY.with(|cell| *cell.borrow_mut() = Registry::default());
}
