//! Crate-wide serialization for tests that mutate [`crate::shared`]'s
//! process-global `SHARED` map.
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

use std::sync::Mutex;

/// Take this lock (`.lock().unwrap_or_else(PoisonError::into_inner)`) around
/// any test body that calls into [`crate::shared`] or
/// [`crate::registry::reset_for_tests`]. Poison-tolerant on purpose: one
/// panicking test under the lock must not cascade into every test that runs
/// after it.
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());
