//! Generic keyed visibility-gate registry (#443).
//!
//! Several "some backend should stay active only while a matching page/widget
//! is actually on screen" gates share one shape: a lazily-created
//! `Mutable<bool>` per gate, a recompute step that walks live state and calls
//! `set` only when the value actually changed (so subscribers — typically a
//! service's `set_active`, parking an always-on poller — don't get redundant
//! wakeups), and a signal for the wiring site (via [`GateRegistry::mutable`]'s
//! `.signal()`). `modal.rs` used to hand-roll this three times over
//! (`NETCONN_VISIBLE`/`STATS_VISIBLE`/
//! `MEDIA_VISIBLE`, each with its own `recompute_*_visible` function) — this
//! module is the one shared registry; callers key it with their own `enum`
//! and own the predicate logic that decides each gate's value.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::Hash;

use hytte::futures_signals::signal::Mutable;

/// A set of named `bool` gates, keyed by `K`. Not `Send`/`Sync` — intended to
/// live in a `thread_local!` on the GTK main thread alongside the state its
/// recompute step reads, matching every other piece of shared UI state in
/// this binary (see `hytte-reactive`'s registry design).
pub(crate) struct GateRegistry<K> {
    gates: RefCell<HashMap<K, Mutable<bool>>>,
}

impl<K: Eq + Hash + Copy> GateRegistry<K> {
    pub(crate) fn new() -> Self {
        GateRegistry {
            gates: RefCell::new(HashMap::new()),
        }
    }

    /// `key`'s underlying `Mutable`, lazily created at `false` if this is
    /// the first reference to it. Returned owned (a cheap `Arc`-backed
    /// clone, per `Mutable`'s own design) so callers can call `.signal()`
    /// on it themselves without tying the result to this registry's
    /// borrow: an `impl Signal` accessor method here, on `&self`, would
    /// capture `self`'s lifetime under edition 2024's return-position
    /// `impl Trait` capture rules and fail to satisfy a `'static` bound.
    pub(crate) fn mutable(&self, key: K) -> Mutable<bool> {
        self.gates
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| Mutable::new(false))
            .clone()
    }

    /// Set `key`'s gate to `visible`, skipping the write when unchanged so
    /// `Mutable`'s notify doesn't wake subscribers for a no-op recompute.
    pub(crate) fn set(&self, key: K, visible: bool) {
        let m = self.mutable(key);
        if m.get() != visible {
            m.set(visible);
        }
    }
}
