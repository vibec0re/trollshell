//! Crate-wide — and, since #777, crate-*spanning* — serialization for tests
//! that mutate [`crate::shared`]'s process-global `SHARED` map.
//!
//! `cargo test` runs a crate's unit tests on a shared thread pool inside one
//! process, so any test that touches the process-global map (directly via
//! `crate::shared`, or indirectly via `crate::registry::reset_for_tests`,
//! which delegates to it) races every other such test unless they all take
//! the same lock. `shared.rs`'s own tests used to guard a private
//! `TEST_LOCK` that only their own module could see; `registry.rs` gained a
//! same-binary caller of the shared-clearing wrapper (#738) that couldn't
//! join it, and flaked `shared.rs`'s tests roughly 1 run in 20 (#743). This
//! module is the fix: one lock, visible to every test in the crate that
//! needs it, so the class of race — not just this instance — is closed.
//!
//! # It spans crates too (#777)
//!
//! The map `crate::shared` guards is process-*global*, not merely
//! crate-`hytte-reactive`-global. Any downstream crate's tests that call
//! [`crate::shared::reset_for_tests`] or [`crate::registry::reset_for_tests`]
//! (both already `#[doc(hidden)] pub` for exactly this reason) mutate the
//! same map from the same libtest thread pool. `hytte-services` hit this
//! directly: `upower::tests` cleared the map via `registry::reset_for_tests()`
//! while holding no lock at all, while `places::tests` serialized on a
//! *private* `SHARED_LOCK` that `upower::tests` had no way to see — two locks
//! guarding one global, exactly the shape this module exists to close, just
//! re-created one crate over because the original `TEST_LOCK` couldn't be
//! reached from outside `hytte-reactive`.
//!
//! So `TEST_LOCK` is `#[doc(hidden)] pub` rather than `pub(crate)`, and this
//! module is not `#[cfg(test)]`-gated: it must exist in a normal (non-test)
//! compile of `hytte-reactive`, because that's the only compile a downstream
//! crate ever links against — it's *their* `#[cfg(test)]` code that needs to
//! reach in and take this lock.

use std::sync::Mutex;

/// Take this lock (`.lock().unwrap_or_else(PoisonError::into_inner)`) around
/// any test body — in this crate or any downstream one — that calls into
/// [`crate::shared`] or [`crate::registry::reset_for_tests`]. Poison-tolerant
/// on purpose: one panicking test under the lock must not cascade into every
/// test that runs after it.
///
/// `#[doc(hidden)]`, like [`crate::shared::reset_for_tests`] and
/// [`crate::registry::reset_for_tests`]: this exists purely so tests can
/// reach across the crate boundary to serialize on the same process-global
/// map those two already cross that boundary to clear. It is not part of
/// this crate's real API.
#[doc(hidden)]
pub static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::TEST_LOCK;

    /// Deterministic proof that `TEST_LOCK` actually excludes a second
    /// holder while the first is live — the property #777's whole fix
    /// depends on ("both `upower::tests` and `places::tests` take it" only
    /// closes the race if taking it actually serializes them). No sleeps,
    /// no timing thresholds, no scheduler race to get unlucky on: while this
    /// thread holds the guard, `try_lock` from another thread is `Mutex`'s
    /// own guarantee to return `Err`, not a probabilistic outcome, so this
    /// either passes every run or the standard library is broken. Once
    /// released, the same static is immediately acquirable again, ruling out
    /// a permanently-poisoned or single-use lock.
    #[test]
    fn test_lock_excludes_a_second_holder_and_releases_cleanly() {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let blocked = std::thread::scope(|s| s.spawn(|| TEST_LOCK.try_lock().is_err()).join())
            .expect("spawned thread panicked");
        assert!(
            blocked,
            "a second thread must not acquire TEST_LOCK while the first still holds it"
        );

        drop(guard);

        let reacquired = std::thread::scope(|s| s.spawn(|| TEST_LOCK.try_lock().is_ok()).join())
            .expect("spawned thread panicked");
        assert!(
            reacquired,
            "TEST_LOCK must be acquirable again once the holder releases it"
        );
    }
}
