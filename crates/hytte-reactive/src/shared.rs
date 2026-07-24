//! Process-global, `Send + Sync` mirror of the thread-local [`Registry`], for
//! the handful of service handles that must be reachable from tokio worker
//! threads.
//!
//! # Why this exists
//!
//! [`crate::registry`] is a `thread_local!` — only the GTK main thread can
//! read it. Most service state is fine with that (widgets subscribe on the
//! main thread, tokio tasks only *write* through the `Mutable` a handle holds).
//! But a few services expose handle bags that a *sibling* service's tokio task
//! needs to read cross-thread: `geoclue`'s location for the `weather` fetch
//! loop, `notifications`' active/history set for the auto-expire timer,
//! `screensaver`'s inhibitor map for the D-Bus `Inhibit` method, and so on.
//!
//! Before this module each such service grew its own `static SHARED:
//! OnceLock<…>` — a shadow registry that (a) never reset with
//! [`registry::reset_for_tests`], so it went silently stale on a second
//! in-process `App` run (tests), and (b) swallowed a double-`set` on
//! re-registration (`let _ = SHARED.set(…)`). This module is the one canonical
//! cross-thread path: handles are keyed by their concrete type (as in the
//! thread-local registry), a re-insert **overwrites** (so a second `App` run
//! gets fresh handles rather than a stale first set), and
//! [`registry::reset_for_tests`] clears it too.
//!
//! [`Registry`]: crate::registry::Registry
//! [`registry::reset_for_tests`]: crate::registry::reset_for_tests
//!
//! # Usage
//!
//! Publish a cheap-to-clone handle bag (a struct of `Mutable`/`Arc` fields)
//! from `Service::start`, and read it back from any thread:
//!
//! ```ignore
//! struct Shared { location: Mutable<LocationState> }
//!
//! // in Service::start:
//! shared::insert(Shared { location: location.clone() });
//!
//! // from a tokio task in another service:
//! if let Some(s) = shared::get::<Shared>() {
//!     let loc = s.location.get_cloned();
//! }
//! ```
//!
//! [`get`] hands out an `Arc<T>` you can hold across `.await` points and move
//! into spawned tasks — the same reach the old `&'static` `OnceLock::get()`
//! gave, minus the never-reset staleness.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

type SharedMap = HashMap<TypeId, Arc<dyn Any + Send + Sync>>;

static SHARED: LazyLock<RwLock<SharedMap>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// Publish a cross-thread handle bag, keyed by its concrete type `T`.
///
/// Overwrites any existing value of the same type — unlike the set-once
/// `OnceLock` statics this replaces, so a second in-process `App` run (tests)
/// gets fresh handles instead of a silently-stale first set. Call once per
/// service, from `Service::start`.
pub fn insert<T: Any + Send + Sync>(value: T) {
    SHARED
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(TypeId::of::<T>(), Arc::new(value));
}

/// Fetch a clone of the `Arc<T>` published by [`insert`], or `None` if the
/// owning service hasn't started.
///
/// The returned `Arc` is safe to hold across `.await` points and move into
/// spawned tasks — it keeps the handle bag alive independently of the map.
#[must_use]
pub fn get<T: Any + Send + Sync>() -> Option<Arc<T>> {
    SHARED
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&TypeId::of::<T>())
        .cloned()?
        .downcast::<T>()
        .ok()
}

/// Clear the shared map — exposed for tests only. Called by
/// [`crate::registry::reset_for_tests`] so one reset covers both registries.
#[doc(hidden)]
pub fn reset_for_tests() {
    SHARED
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

#[cfg(test)]
mod tests {
    use super::{get, insert, reset_for_tests};
    use std::sync::{Arc, Mutex, PoisonError};

    #[derive(Debug, PartialEq, Eq)]
    struct Alpha(u32);
    #[derive(Debug, PartialEq, Eq)]
    struct Beta(&'static str);

    // The shared map is process-global; cargo runs tests in parallel threads of
    // one process, so serialize the cases that assert on `reset_for_tests`
    // (which clears the *whole* map) to keep them from clobbering each other.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn insert_get_roundtrip_and_type_isolation() {
        let _g = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_tests();
        insert(Alpha(7));
        insert(Beta("hi"));
        assert_eq!(get::<Alpha>().as_deref(), Some(&Alpha(7)));
        assert_eq!(get::<Beta>().as_deref(), Some(&Beta("hi")));
        reset_for_tests();
    }

    #[test]
    fn get_is_none_before_insert_and_after_reset() {
        let _g = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_tests();
        assert!(get::<Alpha>().is_none());
        insert(Alpha(1));
        assert!(get::<Alpha>().is_some());
        reset_for_tests();
        assert!(get::<Alpha>().is_none());
    }

    #[test]
    fn reinsert_overwrites_rather_than_swallowing() {
        // The whole point vs. `OnceLock::set`: a second in-process App run must
        // replace the stale first handles, not keep them.
        let _g = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_tests();
        insert(Alpha(1));
        insert(Alpha(2));
        assert_eq!(get::<Alpha>().as_deref(), Some(&Alpha(2)));
        reset_for_tests();
    }

    #[test]
    fn arc_clone_outlives_the_map_entry() {
        let _g = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        reset_for_tests();
        insert(Alpha(99));
        let held: Arc<Alpha> = get::<Alpha>().expect("just inserted");
        reset_for_tests(); // drop the map's copy
        assert_eq!(*held, Alpha(99)); // the held Arc is still valid
    }
}
