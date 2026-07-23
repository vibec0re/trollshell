//! Shared resilience primitives for the two EDS worker threads
//! ([`crate::calendar`] and [`crate::tasks`]) — issue #432.
//!
//! Both services own a dedicated thread that opens a
//! [`hytte_ecal::Registry`] at startup and caches per-source
//! [`hytte_ecal::CalClient`] handles. Two failure modes used to be
//! permanent:
//!
//! 1. **Init failure** — at session bring-up trollshell and
//!    evolution-data-server activate concurrently, so the blocking
//!    `Registry::new()` D-Bus round-trip can time out. The worker used to
//!    drain its channel and return, leaving the service inert for the whole
//!    session. Now it retries with the exponential backoff defined here.
//! 2. **Dead cached handles** — an EDS crash/restart (or a source removed
//!    at runtime) kills every cached `CalClient`, but the caches had no
//!    eviction, so every poll failed quietly forever. The workers now evict
//!    on error and, when *every* known source keeps failing, rebuild the
//!    whole session ([`SourceFailureStreak`] decides when).
//!
//! Everything here is pure logic + std channels — hermetically testable,
//! no EDS required.

use std::sync::mpsc;
use std::time::{Duration, Instant};

/// First retry delay after a failed EDS worker init.
pub(crate) const INIT_BACKOFF_START: Duration = Duration::from_secs(1);

/// Ceiling for the init retry delay. Keeps the steady-state retry cost
/// negligible on a machine where EDS never comes up (one cheap D-Bus
/// activation attempt per minute) while bounding how stale the boot race
/// can leave us once it *does* come up.
pub(crate) const INIT_BACKOFF_CAP: Duration = Duration::from_mins(1);

/// Next delay in the doubling-with-cap backoff progression.
pub(crate) fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(INIT_BACKOFF_CAP)
}

/// Sleep for `delay`, consuming any messages that arrive on `rx` meanwhile
/// (handing each to `on_msg` — buffer or drop as the caller sees fit) so the
/// backoff can't be short-circuited by a burst of refresh requests. Returns
/// `false` when every sender has disconnected (shutdown) — the caller should
/// stop retrying and exit.
pub(crate) fn wait_backoff<T>(
    rx: &mpsc::Receiver<T>,
    delay: Duration,
    mut on_msg: impl FnMut(T),
) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(msg) => on_msg(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => return true,
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Consecutive all-sources-failed scans before the worker tears down and
/// rebuilds its whole EDS session (registry + client caches). Per-client
/// evict-and-reconnect handles a plain EDS restart within one poll; the
/// session rebuild is the deeper fallback for a registry connection that
/// itself died. Three polls of total failure is unambiguous without being
/// trigger-happy about one bad pass.
const REBUILD_THRESHOLD: u32 = 3;

/// Tracks consecutive scans in which **every** known source failed — the
/// signature of a dead [`hytte_ecal::Registry`] session rather than one
/// flaky calendar. [`Self::record`] says when to rebuild.
#[derive(Debug, Default)]
pub(crate) struct SourceFailureStreak {
    consecutive: u32,
}

impl SourceFailureStreak {
    /// Record one scan's outcome (`total` sources seen, `failed` of them
    /// erroring). Returns `true` when the streak reaches the rebuild
    /// threshold; the streak then resets, so a failed rebuild attempt is
    /// naturally re-paced to every [`REBUILD_THRESHOLD`] polls. A scan with
    /// zero sources never counts — "no calendars configured" is not
    /// distinguishable from a dead registry, and rebuilding on it would
    /// churn forever on machines without EDS sources.
    pub(crate) fn record(&mut self, total: usize, failed: usize) -> bool {
        if total == 0 || failed < total {
            self.consecutive = 0;
            return false;
        }
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive >= REBUILD_THRESHOLD {
            self.consecutive = 0;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn backoff_doubles_and_caps() {
        let mut d = INIT_BACKOFF_START;
        let mut seen = Vec::new();
        for _ in 0..8 {
            seen.push(d.as_secs());
            d = next_backoff(d);
        }
        assert_eq!(seen, vec![1, 2, 4, 8, 16, 32, 60, 60]);
    }

    #[test]
    fn wait_backoff_times_out_true() {
        let (_tx, rx) = mpsc::channel::<()>();
        let start = Instant::now();
        assert!(wait_backoff(&rx, Duration::from_millis(30), |()| {}));
        assert!(start.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn wait_backoff_disconnect_false() {
        let (tx, rx) = mpsc::channel::<()>();
        drop(tx);
        assert!(!wait_backoff(&rx, Duration::from_mins(1), |()| {}));
    }

    #[test]
    fn wait_backoff_buffers_messages_and_holds_full_delay() {
        let (tx, rx) = mpsc::channel::<u32>();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        let mut got = Vec::new();
        let start = Instant::now();
        // Messages arriving must be handed to on_msg but must NOT
        // short-circuit the backoff sleep.
        assert!(wait_backoff(&rx, Duration::from_millis(30), |m| got.push(m)));
        assert!(start.elapsed() >= Duration::from_millis(30));
        assert_eq!(got, vec![1, 2]);
    }

    #[test]
    fn streak_ignores_partial_failure() {
        let mut s = SourceFailureStreak::default();
        for _ in 0..10 {
            assert!(!s.record(3, 2));
        }
    }

    #[test]
    fn streak_ignores_zero_sources() {
        let mut s = SourceFailureStreak::default();
        for _ in 0..10 {
            assert!(!s.record(0, 0));
        }
    }

    #[test]
    fn streak_triggers_on_third_consecutive_total_failure() {
        let mut s = SourceFailureStreak::default();
        assert!(!s.record(2, 2));
        assert!(!s.record(2, 2));
        assert!(s.record(2, 2));
        // Reset after triggering: the next trigger needs three more.
        assert!(!s.record(2, 2));
        assert!(!s.record(2, 2));
        assert!(s.record(2, 2));
    }

    #[test]
    fn streak_resets_on_success() {
        let mut s = SourceFailureStreak::default();
        assert!(!s.record(1, 1));
        assert!(!s.record(1, 1));
        assert!(!s.record(1, 0)); // one healthy scan resets
        assert!(!s.record(1, 1));
        assert!(!s.record(1, 1));
        assert!(s.record(1, 1));
    }
}
