//! Gated polling loop: the shared park/resume + dedup scaffolding several
//! `hytte-services` pollers wrap around a periodic sampler.
//!
//! A number of pollers share the same shape: sample on an interval, but *park*
//! (forking nothing) while a gate `Mutable<bool>` is `false` — e.g. while the
//! drawer that consumes the data is hidden — and resume the instant it flips
//! back. Each hand-rolled the same top-of-loop `wait_for(true)` park, the same
//! `select!` that bails the inter-sample sleep early on deactivation, and a
//! dedup-before-write (some cloning the whole `Vec` just to compare).
//!
//! [`gated_poll`] captures that scaffolding — including dedup-by-reference — so
//! each service only supplies its per-tick sampler.
//!
//! The inter-sample period is a `FnMut() -> Duration` rather than a fixed
//! `Duration`, called fresh before every sleep (#1172) — so a poller whose
//! cadence stretches on battery power (mirroring #1081's config-poll design)
//! can express that by closing over its own `on_battery()` check, the same
//! shape `netconn`/`app_usage`'s inlined copies already used before adopting
//! this. A poller with no such concern just passes a closure returning the
//! same constant every time.
//!
//! **The cadence is sampled once per cycle, immediately before the sleep — not
//! continuously.** A source that changes *during* a sleep is therefore honoured
//! only on the next cycle, so a battery-slowed adopter sees up to a full slow
//! interval of latency on the battery→AC edge. That is not a regression (the
//! inlined copies read their `cadence(on_battery())` at exactly this point too),
//! but it is a boundary worth naming, because the tree's closest analogue does
//! the opposite on purpose: `hytte_services::places`'s `wait_cadence` (#505)
//! re-checks the target every second *inside* the wait, precisely so a mid-wait
//! power-state flip shortens or lengthens the remaining wait. Reach for that
//! shape when mid-wait responsiveness actually matters; this loop is the
//! per-cycle one.

use futures_signals::signal::{Mutable, SignalExt};
use std::future::Future;
use std::time::Duration;

/// Floor applied to whatever `cadence` returns, so a computed-and-wrong
/// `Duration::ZERO` cannot turn [`gated_poll`] into a busy loop around
/// `sample()`.
///
/// Before #1172 the period was a `Duration` literal at the call site and zero
/// was effectively unrepresentable; now that it is computed — and #1040-era
/// config-derived values are the obvious next source — a 0 (or a missing key
/// deserialising to one) would spin a tokio worker at 100 % with no timer to
/// yield on. 100 ms is far below every real adopter's period (the fastest is
/// 1 s) so it never alters live behaviour; it only bounds the pathological
/// case.
pub const MIN_CADENCE: Duration = Duration::from_millis(100);

/// Run a gated, deduplicated polling loop (until the task is cancelled).
///
/// On each active tick `sample` is called; a `Some(next)` that differs from
/// `writer`'s current value — compared **by reference** via `PartialEq`, no
/// clone — is written, while `None` or an unchanged value is skipped so the
/// signal doesn't re-fire for nothing. While `active` is `false` the loop parks
/// on it and forks nothing, resuming the instant it flips to `true`
/// (`Mutable::signal()` replays the current value, so an already-active gate
/// returns immediately — no lost wakeup). The inter-sample sleep likewise bails
/// early when `active` goes `false`, so parking is immediate rather than a tick
/// late.
///
/// `cadence` is called once per sleep, immediately before it — never cached —
/// so a live source (e.g. a battery-aware `Duration` mapping) is honoured on
/// every cycle without this loop needing to know anything about batteries.
/// Sampling it *per cycle* is also the boundary: a change that lands while a
/// sleep is already in flight takes effect on the following cycle, not this
/// one. See the module docs for the one place in the tree that re-checks
/// mid-wait instead (`hytte_services::places`'s `wait_cadence`, #505).
///
/// Whatever `cadence` returns is floored at [`MIN_CADENCE`], so a computed
/// zero degrades to a slow poll rather than a busy loop.
///
/// This is an `async fn` — the loop body itself, not a spawner. Wrap the call
/// in [`crate::spawn_supervised`] so a panicking sampler restarts with backoff.
pub async fn gated_poll<T, F, Fut, C>(
    active: Mutable<bool>,
    mut cadence: C,
    writer: Mutable<T>,
    mut sample: F,
) where
    T: PartialEq,
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
    C: FnMut() -> Duration,
{
    loop {
        // Park (forking nothing) while gated inactive.
        if !active.get() {
            let _ = active.signal().wait_for(true).await;
        }

        if let Some(next) = sample().await {
            // Dedup by reference: only write (and re-fire the signal) when the
            // sample actually differs from what's currently published.
            let changed = { *writer.lock_ref() != next };
            if changed {
                writer.set(next);
            }
        }

        // Sleep the inter-sample interval, but bail out early if we get gated
        // inactive mid-wait — no point holding the timer when parked. The
        // top-of-loop park then handles the resume edge.
        tokio::select! {
            () = tokio::time::sleep(cadence().max(MIN_CADENCE)) => {}
            _ = active.signal().wait_for(false) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MIN_CADENCE, gated_poll};
    use futures_signals::signal::{Mutable, SignalExt};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // Every test in here runs under `tokio::time::pause()`, so the durations
    // are *virtual* and cost nothing in wall-clock time. They are all at or
    // above [`MIN_CADENCE`] on purpose: a sub-floor cadence would be silently
    // rounded up to the floor and the test would measure the floor instead of
    // what it names.

    /// A `false` gate must park the loop: the sampler is never called and the
    /// published value never changes.
    #[test]
    fn parks_while_inactive() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        rt.block_on(async {
            tokio::time::pause();
            let active = Mutable::new(false);
            let writer = Mutable::new(0u32);
            let calls = Arc::new(AtomicUsize::new(0));
            let c = calls.clone();
            let poll = gated_poll(
                active,
                || Duration::from_millis(100),
                writer.clone(),
                move || {
                    c.fetch_add(1, Ordering::SeqCst);
                    async { Some(1u32) }
                },
            );
            let _ = tokio::time::timeout(Duration::from_secs(1), poll).await;
            assert_eq!(calls.load(Ordering::SeqCst), 0, "sampler ran while parked");
            assert_eq!(writer.get(), 0, "value changed while parked");
        });
    }

    /// Active ticks write changed samples (and skip `None`), so the writer
    /// tracks the sampler's latest distinct value.
    #[test]
    fn writes_changed_samples() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        rt.block_on(async {
            tokio::time::pause();
            let active = Mutable::new(true);
            let writer = Mutable::new(0u32);
            let calls = Arc::new(AtomicUsize::new(0));
            let c = calls.clone();
            let poll = gated_poll(
                active,
                || Duration::from_millis(100),
                writer.clone(),
                move || {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    async move {
                        match n {
                            0 => Some(1u32),
                            1 => Some(2u32),
                            _ => None, // idle: keep the last value
                        }
                    }
                },
            );
            let _ = tokio::time::timeout(Duration::from_secs(1), poll).await;
            assert_eq!(writer.get(), 2);
        });
    }

    /// Dedup-by-reference: when every sample equals the writer's current value,
    /// `set` is never called, so the downstream signal fires exactly once (the
    /// initial replay). `set` never running means there is nothing to coalesce,
    /// which makes the emission count deterministic — unlike counting across
    /// *distinct* writes, which a latest-value signal legitimately collapses.
    #[test]
    fn identical_samples_are_deduped_and_never_refire() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        rt.block_on(async {
            tokio::time::pause();
            let active = Mutable::new(true);
            let writer = Mutable::new(7u32);

            let emissions = Arc::new(AtomicUsize::new(0));
            let e = emissions.clone();
            let sig = writer.signal();
            let sub = tokio::spawn(async move {
                sig.for_each(move |_| {
                    e.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(())
                })
                .await;
            });

            // Always the current value → every tick is deduped, `set` never runs.
            let poll = gated_poll(
                active,
                || Duration::from_millis(100),
                writer.clone(),
                || async { Some(7u32) },
            );
            let _ = tokio::time::timeout(Duration::from_secs(1), poll).await;
            sub.abort();

            assert_eq!(writer.get(), 7);
            assert_eq!(
                emissions.load(Ordering::SeqCst),
                1,
                "identical samples must not re-fire the signal"
            );
        });
    }

    /// `cadence` is called fresh before every sleep — never cached from the
    /// first call — so a live source (mirroring a battery-aware duration
    /// mapping, #1081/#1172) is honoured on every cycle. A mutation that reads
    /// `cadence` once up front and reuses that `Duration` forever would sleep
    /// the *first* value (1 s, read before the first sample) on every
    /// subsequent cycle too, so ticks 2+ would never observe the flip to 100 ms
    /// and the sampler would run ~3 times in the 2.5 s budget instead of the
    /// ~25 a live 100 ms cadence allows.
    #[test]
    fn cadence_is_read_fresh_every_cycle_not_cached() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        rt.block_on(async {
            tokio::time::pause();
            let active = Mutable::new(true);
            let writer = Mutable::new(0u32);
            let calls = Arc::new(AtomicUsize::new(0));
            let c = calls.clone();

            // Start slow (1 s/tick) so a handful of early ticks are cheap to
            // account for, then drop to 100 ms/tick after the first sample —
            // a cached-once cadence would never see this flip. Both values are
            // at or above MIN_CADENCE, so the floor is not what is measured.
            let cadence_calls = Arc::new(AtomicUsize::new(0));
            let cc = cadence_calls.clone();
            let cadence = move || {
                cc.fetch_add(1, Ordering::SeqCst);
                if c.load(Ordering::SeqCst) == 0 {
                    Duration::from_secs(1)
                } else {
                    Duration::from_millis(100)
                }
            };

            let sample_calls = calls.clone();
            let poll = gated_poll(active, cadence, writer, move || {
                sample_calls.fetch_add(1, Ordering::SeqCst);
                async { None::<u32> }
            });
            let _ = tokio::time::timeout(Duration::from_millis(2_500), poll).await;

            // ~25 fast 100 ms ticks over the 2.5 s budget; a cached-once
            // cadence would keep sleeping the 1 s it read before the first
            // sample and manage ~3 ticks.
            assert!(
                calls.load(Ordering::SeqCst) > 5,
                "cadence must be re-read every cycle, not cached from the first call: only {} ticks in 2.5s",
                calls.load(Ordering::SeqCst)
            );
        });
    }

    /// A cadence source that returns `Duration::ZERO` must not turn the loop
    /// into a busy poll around `sample()`: [`MIN_CADENCE`] floors it. Before
    /// #1172 the period was a `Duration` literal at the call site and zero was
    /// effectively unrepresentable; now that it is *computed*, a config-derived
    /// 0 (or a missing key deserialising to one) would spin a tokio worker at
    /// 100 % forever, because a zero `sleep` is ready on its first poll and so
    /// never yields.
    ///
    /// At the 100 ms floor one virtual second allows ~11 samples. The `BUDGET`
    /// escape hatch — park on `pending()` rather than return, once the sampler
    /// has been called `BUDGET` times — is what makes an unfloored cadence fail
    /// this by *assertion* instead of by hanging the harness: a loop that never
    /// yields also never lets the paused clock auto-advance to the `timeout`,
    /// so without the hatch the mutation would hang rather than redden.
    ///
    /// Falsification: drop the `.max(MIN_CADENCE)` and the sampler runs
    /// `BUDGET` times at one virtual instant — far past the 20 asserted here.
    #[test]
    fn a_zero_cadence_is_floored_and_never_busy_polls() {
        const BUDGET: usize = 100;
        assert!(
            MIN_CADENCE > Duration::ZERO,
            "the floor has to be a floor for this test to mean anything"
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        rt.block_on(async {
            tokio::time::pause();
            let active = Mutable::new(true);
            let writer = Mutable::new(0u32);
            let calls = Arc::new(AtomicUsize::new(0));
            let c = calls.clone();

            let poll = gated_poll(active, || Duration::ZERO, writer, move || {
                let n = c.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n + 1 >= BUDGET {
                        std::future::pending::<()>().await;
                    }
                    None::<u32>
                }
            });
            let _ = tokio::time::timeout(Duration::from_secs(1), poll).await;

            let n = calls.load(Ordering::SeqCst);
            assert!(
                n <= 20,
                "a zero cadence must be floored at MIN_CADENCE, not busy-polled: \
                 {n} samples in one virtual second"
            );
        });
    }
}
