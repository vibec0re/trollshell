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
///
/// Split into two steps — run `start`, *then* hand back a thunk that inserts
/// its result — rather than one method that takes `&mut Registry` and does
/// both: that used to let [`install`] hold the registry's mutable borrow for
/// the whole span of `start`, which panics the moment `start` calls
/// [`with`] itself (a service reading a sibling's already-installed
/// handles). Splitting it means `start` runs with *no* registry borrow held
/// at all, and the returned thunk only ever does the trivial insert — no
/// user code — so it can never re-enter [`with`] or [`install`] either.
pub trait ServiceErased: 'static {
    fn start_erased(self: Box<Self>, rt: &tokio::runtime::Handle) -> Box<dyn FnOnce(&mut Registry)>;
}

impl<S: Service> ServiceErased for S {
    fn start_erased(self: Box<Self>, rt: &tokio::runtime::Handle) -> Box<dyn FnOnce(&mut Registry)> {
        let handles = self.start(rt);
        Box::new(move |registry: &mut Registry| registry.insert::<S::Handles>(handles))
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
/// Panics if a mutable borrow is already active on this thread's registry (a
/// `RefCell` borrow conflict). That cannot happen from inside a `Service::start`
/// call: [`install`] runs `start` to completion *before* it ever borrows the
/// registry, taking a (mutable) borrow only afterwards, to perform the
/// resulting insert — a step that runs no service code and so cannot recurse
/// into `with` or `install`. So a service's `start` calling this to read an
/// already-installed sibling's handles is a supported pattern, not a hazard.
pub fn with<R>(f: impl FnOnce(&Registry) -> R) -> R {
    REGISTRY.with(|cell| f(&cell.borrow()))
}

/// Install a single service. Called by `App::run` once per registered
/// service before invoking the consumer's body closure.
///
/// `start_erased` runs the service's `start` here, with no registry borrow
/// held — see [`ServiceErased`] — so `start` may itself call [`with`] to read
/// a sibling service that was installed earlier in the same list. Only the
/// trivial insert that follows touches `REGISTRY`, and only for the instant
/// it takes to run.
pub fn install(service: Box<dyn ServiceErased>, rt: &tokio::runtime::Handle) {
    let insert = service.start_erased(rt);
    REGISTRY.with(|cell| insert(&mut cell.borrow_mut()));
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
    use super::{REGISTRY, Registry, Service, install, reset_for_tests, with};
    use crate::test_lock::TEST_LOCK;
    use std::sync::PoisonError;

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
    /// `reset_for_tests` also delegates to [`crate::shared::reset_for_tests`]
    /// (the process-global mirror), which `shared.rs`'s own tests mutate
    /// too. `cargo test` runs a crate's unit tests on a shared thread pool
    /// in one process, so any test touching that process-global map must
    /// serialize against every other one — this test takes the same
    /// crate-level `TEST_LOCK` (`crate::test_lock`) that `shared.rs`'s tests
    /// take, rather than a lock private to either module (#743: an earlier
    /// version of this test called `reset_for_tests` without holding it and
    /// flaked `shared.rs`'s tests in roughly 1 of 20 runs).
    #[test]
    fn reset_for_tests_clears_the_thread_local_registry() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);

        REGISTRY.with(|cell| cell.borrow_mut().insert::<u32>(7));
        assert_eq!(with(|r| r.get::<u32>().copied()), Some(7));

        reset_for_tests();

        assert_eq!(
            with(|r| r.get::<u32>().copied()),
            None,
            "reset_for_tests must clear the thread-local registry"
        );
    }

    /// A sibling service's handles, read by [`ReaderService::start`] below.
    struct SiblingHandles(u32);

    struct SiblingService;

    impl Service for SiblingService {
        type Handles = SiblingHandles;

        fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
            SiblingHandles(7)
        }
    }

    struct ReaderHandles(u32);

    /// A service whose `start` reads another service's already-installed
    /// handles via [`with`] — an accessor function in `hytte-services`
    /// (`upower::state()`, say) doing exactly this from a sibling's `start`
    /// is the real-world shape this regression-tests.
    struct ReaderService;

    impl Service for ReaderService {
        type Handles = ReaderHandles;

        fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
            let sibling = with(|r| r.get::<SiblingHandles>().map(|h| h.0))
                .expect("SiblingService must already be installed");
            ReaderHandles(sibling)
        }
    }

    /// Before the fix, `install` held `REGISTRY`'s mutable borrow for the
    /// whole span of `Service::start`, so `ReaderService::start`'s call to
    /// [`with`] (an immutable borrow, on the *same* thread, while that
    /// mutable borrow was still live) panicked with "already mutably
    /// borrowed" — exactly the case `with`'s own Panics doc claimed could
    /// not happen. Reverting the `install`/`ServiceErased` restructure
    /// reproduces that panic here; see the fix in `install` and
    /// `ServiceErased::start_erased`.
    #[test]
    fn install_lets_a_services_start_read_an_already_installed_sibling() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_tests();

        install(Box::new(SiblingService), crate::runtime::handle());
        install(Box::new(ReaderService), crate::runtime::handle());

        assert_eq!(
            with(|r| r.get::<ReaderHandles>().map(|h| h.0)),
            Some(7),
            "ReaderService::start must have seen SiblingHandles"
        );

        reset_for_tests();
    }
}
