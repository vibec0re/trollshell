//! App-usage service — walks `/proc` every ~2 s and exposes the top processes
//! by CPU share and by resident memory, aggregated by **systemd app-scope**
//! (`/proc/<pid>/cgroup`). PIDs that don't live inside a recognised app scope
//! but do belong to a named systemd **service** collapse into a per-service row;
//! everything else folds into a single "System" bucket.
//!
//! The actual `/proc` walk, the cgroup-leaf parsing, and their pure data
//! shapes (including [`ProcSample`], re-exported below) moved into
//! [`hytte_sensors::app_usage`] byte-for-byte (#1419 item 3, the #1249
//! `hytte-sensors` precedent) — a plugin cannot link this crate, and
//! `hytte-plugin-stats` wants the same walker for its "Top apps" rows. This
//! module keeps everything that needs `Mutable`/a tokio runtime: the
//! `Service`/`AppUsageHandles` wrapper, the poll loop, panel-gating, and the
//! battery-aware cadence, calling into [`hytte_sensors::app_usage::sample_proc`]
//! for the actual sample.
//!
//! # Panel-gating
//!
//! The poller is gated on Stats-drawer visibility via [`set_active`]: it parks
//! (walking nothing) while the panel is hidden and resumes — taking a fresh
//! sample immediately — when it reappears (#50, item 5 of #42).
//!
//! # Battery-aware cadence
//!
//! Independent of panel-gating, the poll period itself stretches from
//! `POLL` to `BATTERY_POLL` (4x) while [`crate::upower::on_battery`]
//! reports the system on battery power — the heaviest of the always-on
//! pollers gets the biggest cadence cut (#505).
//!
//! # Public API
//!
//! ```ignore
//! .with(app_usage::service())              // register once
//! app_usage::top_by_cpu() -> impl Signal<Item = Vec<ProcSample>>
//! app_usage::top_by_mem() -> impl Signal<Item = Vec<ProcSample>>
//! ```

use std::collections::HashMap;
use std::time::Duration;

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, gated_poll, registry, spawn_supervised};

// The data shape moved to `hytte-sensors` byte-for-byte (#1419 item 3) as a
// plain `pub struct` directly in that crate's `app_usage` submodule, exactly
// where it used to live here — re-exporting it keeps
// `hytte_services::app_usage::ProcSample` resolving to the same path every
// existing caller (this crate's own tests, `trollshell`'s panels) already
// uses.
pub use hytte_sensors::app_usage::ProcSample;
use hytte_sensors::app_usage::sample_proc;

/// Poll period on AC power. Heavier than the aggregate `sensors` reads (2
/// files per PID), so it runs at half that cadence; it's additionally gated
/// to "Stats panel visible" via [`set_active`] so it idles entirely when no
/// one's looking.
const POLL: Duration = Duration::from_secs(2);

/// Poll period on battery power: 4x AC. This is the priciest of the
/// always-on pollers (a full `/proc` walk — two file reads per PID), so it
/// gets the biggest stretch in the battery-aware sweep (#505).
const BATTERY_POLL: Duration = Duration::from_secs(8);

/// Battery-aware poll cadence: [`BATTERY_POLL`] while on battery power, else
/// [`POLL`]. Pure so the on-battery → interval mapping is unit-testable.
fn cadence(on_battery: bool) -> Duration {
    if on_battery { BATTERY_POLL } else { POLL }
}

/// Best-effort on-battery snapshot — see
/// [`crate::upower::on_battery_snapshot`] for why this reads the cross-thread
/// `shared` bag rather than the thread-local registry (#505).
fn on_battery() -> bool {
    crate::upower::on_battery_snapshot()
}

#[doc(hidden)]
pub struct AppUsageHandles {
    /// Both ranked lists, published together (#1172): they come from the same
    /// `/proc` walk and `gated_poll` dedups one `T`, so splitting them into two
    /// independent `Mutable`s would need either two walks (doubling the cost
    /// this module exists to amortize) or a dedup that only covers one half.
    /// In practice the jiffy deltas that drive `cpu_frac` move on essentially
    /// every tick, so this costs no *observable* coalescing over the old
    /// always-`.set()` pair — see [`top_by_cpu`]/[`top_by_mem`] for how the
    /// public per-list signals are recovered from it.
    pub(crate) usage: Mutable<(Vec<ProcSample>, Vec<ProcSample>)>,
    /// Gate for the `/proc` poller. While `false`, the poll loop parks and
    /// walks nothing; flipping it back to `true` resumes sampling immediately
    /// (the loop `select!`s on this so reactivation isn't delayed a full tick).
    ///
    /// Defaults to `true` so the first sample is taken eagerly at startup —
    /// the top-apps lists are then already populated the instant the Stats
    /// drawer opens, and `set_active(false)` parks the poller once the binary
    /// reports that panel is hidden. See [`set_active`].
    pub(crate) active: Mutable<bool>,
}

pub struct AppUsageService;

impl Service for AppUsageService {
    type Handles = AppUsageHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = AppUsageHandles {
            usage: Mutable::new((Vec::new(), Vec::new())),
            active: Mutable::new(true),
        };
        let usage = handles.usage.clone();
        let active = handles.active.clone();
        spawn_supervised("app_usage", move || {
            poll_loop(usage.clone(), active.clone())
        });
        handles
    }
}

#[must_use]
pub fn service() -> AppUsageService {
    AppUsageService
}

/// Top processes by CPU share (descending), capped to the top N.
pub fn top_by_cpu() -> impl Signal<Item = Vec<ProcSample>> {
    registry::with(|r| {
        r.get::<AppUsageHandles>()
            .expect("app_usage::service() not registered")
            .usage
            .signal_ref(|(cpu, _mem)| cpu.clone())
    })
}

/// Top processes by resident memory (descending), capped to the top N.
pub fn top_by_mem() -> impl Signal<Item = Vec<ProcSample>> {
    registry::with(|r| {
        r.get::<AppUsageHandles>()
            .expect("app_usage::service() not registered")
            .usage
            .signal_ref(|(_cpu, mem)| mem.clone())
    })
}

/// Gate the `/proc` poller: `true` resumes ~2 s sampling (immediately taking a
/// fresh sample), `false` parks it so it walks nothing while the Stats drawer
/// panel — the only consumer of these lists — is hidden.
///
/// Fire-and-forget command: the binary wires the Stats-drawer-visibility signal
/// to this so the always-on poller idles when no one's looking (#50, realizing
/// item 5 of #42). A no-op `set` to the same value is skipped to avoid spurious
/// loop wakeups.
pub fn set_active(active: bool) {
    registry::with(|r| {
        let handle = &r
            .get::<AppUsageHandles>()
            .expect("app_usage::service() not registered")
            .active;
        if handle.get() != active {
            handle.set(active);
        }
    });
}

async fn poll_loop(usage: Mutable<(Vec<ProcSample>, Vec<ProcSample>)>, active: Mutable<bool>) {
    // The park/dedup/select-bail scaffolding this hand-rolled now lives in
    // `hytte_reactive::gated_poll` (#1172) — this was one of the pollers whose
    // battery-aware duration the old fixed-`Duration` `gated_poll` couldn't
    // express; it now takes `cadence` as a live source, called fresh before
    // every sleep.
    //
    // The CPU fractions are jiffy *deltas* over the inter-sample interval, so
    // a resume sample's delta spans the whole parked gap. That's fine:
    // `prev_total` grows by the same wall-clock span as the per-PID jiffies,
    // so the ratio stays a valid "share of total capacity" — a process that
    // was idle the whole time still reads ~0.
    //
    // `gated_poll`'s sampler is a plain `FnMut() -> Fut` (not an `AsyncFnMut`
    // borrowing its own captures — see `hytte_plugin::poll`'s doc comment on
    // the identical choice: the sugar's future is higher-ranked over the
    // call's lifetime, which the compiler can't prove `Send` for a task handed
    // to `tokio::spawn`). So the per-PID jiffy map and the running total —
    // state that must survive from one call to the next — live behind a
    // `std::sync::Mutex` the closure clones an `Arc` handle to on every call,
    // rather than as captured-by-reference locals: this task is never
    // actually contended (one loop, calls never overlap), the `Mutex` is only
    // what lets an owned, `'static` future leave and return through a plain
    // `FnMut`.
    let jiffy_state =
        std::sync::Arc::new(std::sync::Mutex::new((HashMap::<u32, u64>::new(), 0u64)));

    gated_poll(
        active,
        || cadence(on_battery()),
        usage,
        move || {
            let jiffy_state = jiffy_state.clone();
            async move {
                // `mem::take` keeps the shared map valid (empty) on the
                // join-error path below, mirroring the pre-#1172 loop.
                let (prev_pid, prev_total) = {
                    let mut guard = jiffy_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    (std::mem::take(&mut guard.0), guard.1)
                };

                // The whole `/proc` walk is hundreds of synchronous file reads —
                // far too much blocking I/O for a shared tokio worker (#434).
                match tokio::task::spawn_blocking(move || sample_proc(&prev_pid, prev_total)).await
                {
                    Ok(sample) => {
                        let mut guard = jiffy_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.0 = sample.cur_pid;
                        guard.1 = sample.total_now;
                        Some((sample.by_cpu, sample.by_mem))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "app_usage: /proc sample task failed");
                        None
                    }
                }
            }
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Battery-aware cadence (#505) ─────────────────────────────────────────

    #[test]
    fn cadence_is_poll_on_ac() {
        assert_eq!(cadence(false), POLL);
    }

    #[test]
    fn cadence_stretches_on_battery() {
        assert_eq!(cadence(true), BATTERY_POLL);
        assert!(BATTERY_POLL > POLL);
    }
}
