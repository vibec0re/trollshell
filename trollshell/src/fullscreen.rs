//! Per-monitor bridge feeding niri's fullscreen state into the
//! `fullscreen_inhibit` service (#404).
//!
//! Not an overlay — there's no surface. It's the GTK-side half of the feature:
//! for each mounted monitor it subscribes [`niri::fullscreen_window_on`] (which
//! needs the output's *live* logical size, only available from the GTK
//! `Monitor`) and forwards each change to
//! [`fullscreen_inhibit::set_output_fullscreen`]. The service aggregates the
//! per-output bits and holds/releases the single logind idle inhibitor.
//!
//! Lifecycle mirrors the per-monitor overlays: [`install`] one watcher per
//! monitor, [`close_all`] to abort them all before rebuilding on hot-plug. The
//! subscription pins clones of its `Monitor`, so it can't self-terminate when
//! an output vanishes — it must be aborted explicitly (same reason
//! `overlays::frame`'s sidebar tick-loop stores its `JoinHandle`). Pair
//! `close_all` with [`fullscreen_inhibit::retain_outputs`] so a vanished
//! output's fullscreen bit doesn't pin the inhibitor forever.

use std::cell::RefCell;

use hytte::gtk::glib;
use hytte::prelude::*;
use hytte::services::{fullscreen_inhibit, niri};

thread_local! {
    /// One live subscription per mounted monitor, aborted + rebuilt on hot-plug.
    static WATCHERS: RefCell<Vec<glib::JoinHandle<()>>> = const { RefCell::new(Vec::new()) };
}

/// Start watching `monitor`'s active-workspace fullscreen state, forwarding
/// each change into `fullscreen_inhibit`. No-op for a monitor without a
/// connector.
pub fn install(monitor: &Monitor) {
    let Some(connector) = monitor.connector().filter(|c| !c.is_empty()) else {
        tracing::debug!("fullscreen::install: monitor has no connector; skipping");
        return;
    };

    // Live logical `(width, height)` — a mode switch (kanshi profile change)
    // re-evaluates the fullscreen threshold without a monitor hot-plug (#442).
    let size = monitor
        .size_changed()
        .map(|(w, h)| (f64::from(w), f64::from(h)));

    let sub = glib::MainContext::default().spawn_local(
        niri::fullscreen_window_on(connector.clone(), size).for_each(move |fullscreen| {
            fullscreen_inhibit::set_output_fullscreen(&connector, fullscreen);
            std::future::ready(())
        }),
    );

    WATCHERS.with(|w| w.borrow_mut().push(sub));
}

/// Abort every per-monitor fullscreen watcher. Called before rebuilding on
/// hot-plug; the caller then re-`install`s for the current monitor set and
/// calls [`fullscreen_inhibit::retain_outputs`] to drop vanished outputs' bits.
pub fn close_all() {
    WATCHERS.with(|w| {
        for sub in w.borrow_mut().drain(..) {
            sub.abort();
        }
    });
}
