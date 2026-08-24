//! Rate-cap latch for the sensors poll loop's per-tick failure `warn!` sites
//! (#770).
//!
//! `poll_loop` runs every second; four of its branches (the blocking-I/O-task
//! panicked fallback, and the `/proc/stat` / `/proc/meminfo` / `/proc/net/dev`
//! read failures) `tracing::warn!` on failure. Uncapped, a *persistent*
//! failure would put one line per second into the journal indefinitely.
//! `hytte_bus::own::log_give_up` (`own.rs:627`) rate-caps a different
//! repeating-failure signal the same way for the same reason — see its doc
//! comment for the full argument. This module mirrors that spirit (cap the
//! repeat, but say when it started and, if anything was actually swallowed,
//! when it stopped) adapted to a straight 1Hz poller rather than an
//! event-driven one.
//!
//! [`WarnLatch`] is the pure decision function: it takes "now" as a
//! parameter instead of reading the clock itself, so its cooldown behavior
//! is unit-testable without sleeping. Each of the four call sites in
//! `poll_loop`/`apply_*` owns one independent `WarnLatch` (stored in
//! `PollState`) and drives its own log lines with the returned outcome — the
//! latch only decides *whether* to log, not *what*, since the four sites'
//! messages differ.

use std::time::{Duration, Instant};

/// How long a rate-capped failure stays silent after being logged, matching
/// `hytte_bus::own::log_give_up`'s default cooldown (`own.rs:252`).
pub(super) const WARN_COOLDOWN: Duration = Duration::from_mins(5);

/// Per-site rate-cap state for a repeating `warn!`. One instance per failure
/// site — see the module docs. Never shared between sites: each site's
/// failure/recovery cadence is independent.
#[derive(Default)]
pub(super) struct WarnLatch {
    /// `Some((last_logged_at, suppressed_since))` while a failure streak is
    /// in progress; `None` when healthy (no streak yet, or the last streak
    /// ended in [`WarnLatch::on_success`]).
    failing: Option<(Instant, u64)>,
}

impl WarnLatch {
    pub(super) const fn new() -> Self {
        Self { failing: None }
    }

    /// Record a failure observed at `now`. Returns `Some(suppressed)` when
    /// this occurrence should be logged: the first failure of a new streak
    /// (`suppressed == 0`), or the first at/after `cooldown` since the last
    /// logged one (`suppressed` = how many occurrences were swallowed in
    /// between). Returns `None` when this occurrence should stay silent.
    pub(super) fn on_failure(&mut self, now: Instant, cooldown: Duration) -> Option<u64> {
        let suppressed = match self.failing {
            Some((last_logged, suppressed)) if now.duration_since(last_logged) < cooldown => {
                self.failing = Some((last_logged, suppressed + 1));
                return None;
            }
            Some((_, suppressed)) => suppressed,
            None => 0,
        };
        self.failing = Some((now, 0));
        Some(suppressed)
    }

    /// Record a success. Returns `Some(suppressed)` when a recovery line is
    /// owed — the streak just ending suppressed at least one occurrence —
    /// carrying that count. Returns `None` when no streak was in progress, or
    /// every occurrence in it was already logged (so a recovery line would
    /// tell the reader nothing the failure line didn't already say, e.g. a
    /// single isolated blip that recovered before the next tick).
    pub(super) fn on_success(&mut self) -> Option<u64> {
        match self.failing.take() {
            Some((_, suppressed)) if suppressed > 0 => Some(suppressed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COOLDOWN: Duration = Duration::from_mins(5);

    #[test]
    fn first_failure_logs() {
        let mut latch = WarnLatch::new();
        let t0 = Instant::now();
        assert_eq!(latch.on_failure(t0, COOLDOWN), Some(0));
    }

    #[test]
    fn second_failure_inside_cooldown_is_suppressed() {
        let mut latch = WarnLatch::new();
        let t0 = Instant::now();
        assert_eq!(latch.on_failure(t0, COOLDOWN), Some(0));

        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(latch.on_failure(t1, COOLDOWN), None);
    }

    #[test]
    fn failure_past_cooldown_logs_again_with_suppressed_count() {
        let mut latch = WarnLatch::new();
        let t0 = Instant::now();
        assert_eq!(latch.on_failure(t0, COOLDOWN), Some(0));

        // Two occurrences suppressed inside the cooldown window.
        let t1 = t0 + Duration::from_mins(1);
        assert_eq!(latch.on_failure(t1, COOLDOWN), None);
        let t2 = t0 + Duration::from_mins(2);
        assert_eq!(latch.on_failure(t2, COOLDOWN), None);

        // Exactly at the cooldown boundary: logs again, reporting the two
        // occurrences that were swallowed since the last logged line.
        let t3 = t0 + COOLDOWN;
        assert_eq!(latch.on_failure(t3, COOLDOWN), Some(2));
    }

    #[test]
    fn recovery_resets_the_latch() {
        let mut latch = WarnLatch::new();
        let t0 = Instant::now();
        assert_eq!(latch.on_failure(t0, COOLDOWN), Some(0));

        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(latch.on_failure(t1, COOLDOWN), None); // suppressed once

        assert_eq!(latch.on_success(), Some(1));

        // The latch is back to fresh: an immediate new failure logs again
        // rather than staying suppressed from the ended streak.
        let t2 = t1 + Duration::from_millis(1);
        assert_eq!(latch.on_failure(t2, COOLDOWN), Some(0));
    }

    #[test]
    fn recovery_with_nothing_suppressed_stays_silent() {
        let mut latch = WarnLatch::new();
        let t0 = Instant::now();
        assert_eq!(latch.on_failure(t0, COOLDOWN), Some(0));

        // Recovers before anything was ever suppressed — no recovery line
        // owed, the single failure line already told the whole story.
        assert_eq!(latch.on_success(), None);
    }

    #[test]
    fn success_with_no_prior_failure_is_a_no_op() {
        let mut latch = WarnLatch::new();
        assert_eq!(latch.on_success(), None);
    }
}
