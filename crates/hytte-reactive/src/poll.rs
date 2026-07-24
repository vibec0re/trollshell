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

use futures_signals::signal::{Mutable, SignalExt};
use std::future::Future;
use std::time::Duration;

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
/// This is an `async fn` — the loop body itself, not a spawner. Wrap the call
/// in [`crate::spawn_supervised`] so a panicking sampler restarts with backoff.
pub async fn gated_poll<T, F, Fut>(
    active: Mutable<bool>,
    interval: Duration,
    writer: Mutable<T>,
    mut sample: F,
) where
    T: PartialEq,
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
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
            () = tokio::time::sleep(interval) => {}
            _ = active.signal().wait_for(false) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::gated_poll;
    use futures_signals::signal::{Mutable, SignalExt};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

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
                Duration::from_millis(1),
                writer.clone(),
                move || {
                    c.fetch_add(1, Ordering::SeqCst);
                    async { Some(1u32) }
                },
            );
            let _ = tokio::time::timeout(Duration::from_millis(50), poll).await;
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
                Duration::from_millis(1),
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
            let _ = tokio::time::timeout(Duration::from_millis(30), poll).await;
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
            let poll = gated_poll(active, Duration::from_millis(1), writer.clone(), || async {
                Some(7u32)
            });
            let _ = tokio::time::timeout(Duration::from_millis(30), poll).await;
            sub.abort();

            assert_eq!(writer.get(), 7);
            assert_eq!(
                emissions.load(Ordering::SeqCst),
                1,
                "identical samples must not re-fire the signal"
            );
        });
    }
}
