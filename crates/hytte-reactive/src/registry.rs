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
        let handles = self.start(rt);
        registry.insert::<S::Handles>(handles);
    }
}

/// Storage for service handles, keyed by their concrete `TypeId`.
#[derive(Default)]
pub struct Registry {
    entries: HashMap<TypeId, Box<dyn Any>>,
}

impl Registry {
    /// Insert a service's handle bag, keyed by its concrete type.
    ///
    /// Registering the same `Handles` type twice is a `main.rs` bug — a second
    /// `App::with(foo::service())` for an already-registered service spawns a
    /// full second task set while orphaning the first set's handles (duplicate
    /// D-Bus subscriptions, and widgets keep reading the *first* set). That used
    /// to happen silently; now it trips a `tracing::error!` in every build and a
    /// `debug_assert!` panic in debug/test builds so the stray `.with(…)` call
    /// surfaces immediately instead of as a runtime mystery.
    pub fn insert<T: 'static>(&mut self, value: T) {
        let replaced = self
            .entries
            .insert(TypeId::of::<T>(), Box::new(value))
            .is_some();
        if replaced {
            tracing::error!(
                handles = std::any::type_name::<T>(),
                "duplicate service registration: overwrote already-registered \
                 handles; the previous set's background tasks are now orphaned \
                 (a service was passed to App::with more than once)"
            );
        }
        debug_assert!(
            !replaced,
            "duplicate service registration for {}",
            std::any::type_name::<T>()
        );
    }

    #[must_use]
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
/// Panics if a mutable borrow is already active on this thread's registry
/// (a `RefCell` borrow conflict). In practice this never happens because
/// the registry is only mutated during `install`, which runs before any
/// subscriptions.
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

/// Wipe both registries — exposed for tests only.
///
/// Clears the thread-local registry *and* the process-global
/// [`crate::shared`] mirror, so one reset gives a second in-process `App` run
/// a clean slate on both cross-thread paths.
#[doc(hidden)]
pub fn reset_for_tests() {
    REGISTRY.with(|cell| *cell.borrow_mut() = Registry::default());
    crate::shared::reset_for_tests();
}

#[cfg(test)]
mod tests {
    use super::{REGISTRY, Registry, reset_for_tests, with};

    #[test]
    fn insert_then_get_roundtrips() {
        let mut reg = Registry::default();
        reg.insert::<u32>(42);
        assert_eq!(reg.get::<u32>(), Some(&42));
        // A different type is independent (keyed by TypeId).
        assert_eq!(reg.get::<i64>(), None);
    }

    #[test]
    #[should_panic(expected = "duplicate service registration")]
    #[cfg(debug_assertions)] // the tripwire is a debug_assert!; release doCheck (crane) compiles it out
    fn duplicate_insert_trips_the_debug_assert() {
        // Two registrations of the same handle type is the `main.rs` diff
        // mistake the tripwire exists to catch. In debug/test builds it panics;
        // in release it logs + overwrites (can't assert the log here).
        let mut reg = Registry::default();
        reg.insert::<u32>(1);
        reg.insert::<u32>(2);
    }

    /// Regression pin for #738: nothing previously called the actual
    /// `reset_for_tests` free function and checked its effect on the
    /// thread-local registry — the other tests in this module construct a
    /// bare `Registry::default()` instead of touching the `REGISTRY`
    /// thread-local this wrapper clears.
    ///
    /// This pins only the thread-local half of the contract on purpose.
    /// `reset_for_tests` also delegates to
    /// [`crate::shared::reset_for_tests`] (the process-global mirror), but
    /// deliberately isn't exercised from *here*: `cargo test` runs a
    /// crate's unit tests on a shared thread pool, and `shared.rs`'s own
    /// tests already mutate that same process-global map, serializing
    /// amongst themselves via a `TEST_LOCK` private to their test module.
    /// A test in this module that also touched `crate::shared` would race
    /// them from outside that lock — confirmed empirically (an earlier
    /// draft of this test flaked shared.rs's tests in roughly 1 of 15
    /// runs). Fixing that needs `shared.rs`'s `TEST_LOCK` hoisted to
    /// something this module can join too, which is out of scope here.
    ///
    /// The shared-delegation half of the contract *is* covered, race-free,
    /// by `hytte-services`'s `upower::tests::on_battery_snapshot_contract`:
    /// it calls this same `reset_for_tests` wrapper and checks the shared
    /// bag cleared, from a separate crate's test binary (its own process,
    /// its own instance of `crate::shared`'s static — no race with
    /// `shared.rs`'s tests here).
    #[test]
    fn reset_for_tests_clears_the_thread_local_registry() {
        REGISTRY.with(|cell| cell.borrow_mut().insert::<u32>(7));
        assert_eq!(with(|r| r.get::<u32>().copied()), Some(7));

        reset_for_tests();

        assert_eq!(
            with(|r| r.get::<u32>().copied()),
            None,
            "reset_for_tests must clear the thread-local registry"
        );
    }
}
