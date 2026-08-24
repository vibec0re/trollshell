//! The crate's one retry ramp: 250 ms doubling to a 30 s ceiling, indexed by
//! **attempt number** rather than by the last delay.
//!
//! Two loops in this crate drive their own retry rate because nothing else
//! will wake them — a bus that is down publishes no events to wait on:
//!
//! - [`connection`](crate::connection)'s supervisor, reconnecting a
//!   [`SharedConnection`](crate::connection::SharedConnection) after a failed
//!   `Connection::session`/`system`.
//! - [`own`](crate::own)'s acquisition-error streak, retrying everything on
//!   the way to a `RequestName` *reply* — connect, subscribe, build the
//!   `DBusProxy`, and `RequestName` itself.
//!
//! They shipped as two implementations of the same 250 ms → 30 s ladder, in
//! two different shapes: the supervisor's was a duration cursor (`next_ms`,
//! doubled in place) and `own`'s was the pure function [`delay_for`] over a
//! 1-based attempt counter. #646 recorded that the attempt-indexed one is
//! strictly the more useful primitive and that the collapse must go that way
//! round, because **a duration cursor cannot drive a log cadence**: there is
//! no attempt number to test, so [`logs_at`] has nothing to be a function of.
//! That matters more since #766 made the shell default to `INFO` — these
//! lines are visible in the deployed journal for the first time.
//!
//! [`logs_at`] is why the ramp is worth sharing rather than merely
//! deduplicating. Pairing a geometric log cadence with the geometric delay
//! bounds a permanently-failing path's *log volume* as well as its retry
//! rate: a bus that is down for a day costs ~11 lines instead of one per
//! attempt. A caller opts into that by testing [`RetryStep::log`]; it is not
//! automatic, and the supervisor deliberately does not yet take it (see
//! [`FailureStreak`]).
//!
//! **Not to be folded into anything else.** `hytte_services`' `retry::Policy`
//! is a different shape (an attempt *budget* with a give-up, weighing a
//! `Result` per step) and #646 settled that two is the right number. The
//! public [`RetryPolicy`](crate::RetryPolicy) in [`call`](mod@crate::call) is a
//! third, unrelated concept — a per-call `Never`/`Once`/`Backoff` selector.

use std::time::Duration;

/// First delay of the ramp.
pub(crate) const RAMP_BASE: Duration = Duration::from_millis(250);

/// Ceiling of the ramp. Deliberately seconds, not minutes: the paths that use
/// it have no event to wake on — a broken bus emits no `NameOwnerChanged` and
/// accepts no connection — so the cap is also the worst-case time to notice
/// the bus came back. Thirty seconds is three orders of magnitude cheaper than
/// a flat 250 ms while still recovering well inside a human's attention span.
pub(crate) const RAMP_CAP: Duration = Duration::from_secs(30);

/// A failure streak shorter than this is still consistent with "the bus
/// blipped", which `connection.rs` already warns about; from here on it is
/// consistent only with "this is not working", which nothing else reports.
/// At the [`delay_for`] ramp this is ~4 s of consecutive failures.
pub(crate) const STREAK_IS_NO_LONGER_A_BLIP: u32 = 5;

/// Delay before the `attempt`-th consecutive retry (1-based).
///
/// 250 ms doubling, capped at [`RAMP_CAP`]: 250 ms, 500 ms, 1 s, 2 s, 4 s,
/// 8 s, 16 s, then 30 s for as long as it keeps failing.
///
/// This is deliberately **not** a contention path. Losing a race for a name
/// somebody else holds is bounded by `permanent_after` and then by an
/// event-driven wait on `NameOwnerChanged` (see `own::wait_for_release_or_cooldown`);
/// it needs no timer ramp because the broker tells us when the situation
/// changes. An *error* has no such event, so this is the one place left where
/// the retry rate is set by us alone, and before #653 the acquisition arm was
/// a flat 250 ms forever — a 4 Hz spin.
pub(crate) fn delay_for(attempt: u32) -> Duration {
    // `min(16)` keeps the shift in range; 250 ms << 16 is ~4.5 h, already far
    // past the cap, so clamping there cannot change the answer.
    let doublings = attempt.saturating_sub(1).min(16);
    RAMP_BASE.saturating_mul(1u32 << doublings).min(RAMP_CAP)
}

/// Whether the `attempt`-th consecutive failure earns a log line (1-based).
///
/// True at every doubling — 1, 2, 4, 8, 16, … — and nowhere else. Pairing a
/// geometric log cadence with the geometric delay of [`delay_for`] is what
/// gives the *logging* a ceiling as well as the retry: a bus that is down for
/// a day costs ~11 lines rather than one per attempt. Without this the
/// non-transient arm of `own` emitted a `warn!` on every retry forever, which
/// is the uncapped-retry-logging #646 objects to, and the transient arm
/// emitted nothing at all, which is the silence #653 objects to.
pub(crate) fn logs_at(attempt: u32) -> bool {
    attempt.is_power_of_two()
}

/// How to react to one failure in a streak: how long to wait, and whether to
/// say anything about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetryStep {
    /// 1-based position in the current streak.
    pub(crate) attempt: u32,
    /// How long to wait before the next attempt.
    pub(crate) delay: Duration,
    /// Whether this failure earns a log line (see [`logs_at`]).
    pub(crate) log: bool,
}

impl RetryStep {
    /// Whether a streak this long has stopped being explicable as a bus blip.
    ///
    /// Below the line a caller should stay at `debug!` — `connection.rs`
    /// already warns about the disconnect itself and a single failure is only
    /// its echo. At or above it, the bus is not answering at all: unlike a
    /// contended name, no configuration change fixes this and the primitive
    /// cannot work around it, so it earns `error!` on its own merits rather
    /// than by borrowing a give-up's level. That borrowing is what #765 undid:
    /// three lines in `own.rs` were at `error!` only to clear the shell's
    /// then-`ERROR`-only default filter, which #766 fixed at the source.
    pub(crate) fn is_serious(self) -> bool {
        self.attempt >= STREAK_IS_NO_LONGER_A_BLIP
    }
}

/// A run of consecutive failures — the cursor form of [`delay_for`].
///
/// Owned by the loop that retries, and deliberately outliving that loop's
/// inner iterations: in `own::run_ownership` it lives across reconnects, so a
/// bus that refuses to connect cannot reset its own ramp just because the
/// outer loop went round again. It is cleared the moment something works —
/// for `own`, a `RequestName` *reply* of any kind, including "somebody else
/// has it", which proves the broker is answering and hands the situation over
/// to the contention accounting. That reset is what keeps the ramp from
/// turning a busy-loop into a stall: a blip costs at most one extra 250 ms,
/// never the 30 s cap.
///
/// The `connection` supervisor uses it purely as the delay cursor it replaced
/// (`record().delay` where it used to say `next()`, `reset()` unchanged) and
/// still emits its `warn!` on every attempt. Adopting [`RetryStep::log`]
/// there would bound that line's volume too, but it is a behaviour change and
/// not part of the #795 dedup — the attempt counter it needs now exists,
/// which is the point.
#[derive(Debug, Default)]
pub(crate) struct FailureStreak {
    attempts: u32,
}

impl FailureStreak {
    /// Record one failure and say how to back off from it.
    pub(crate) fn record(&mut self) -> RetryStep {
        self.attempts = self.attempts.saturating_add(1);
        RetryStep {
            attempt: self.attempts,
            delay: delay_for(self.attempts),
            log: logs_at(self.attempts),
        }
    }

    /// Forget the streak — something worked.
    pub(crate) fn reset(&mut self) {
        self.attempts = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{FailureStreak, RAMP_BASE, RAMP_CAP, delay_for, logs_at};
    use std::time::Duration;

    // ── #653: the ramp, as a pure function of the attempt number ────────────

    /// The backoff progression, pinned exactly. 250 ms doubling to a 30 s
    /// ceiling: the flat `Duration::from_millis(250)` this replaces would fail
    /// every assertion from the second onwards.
    #[test]
    fn delay_for_doubles_then_caps() {
        let expected_ms = [250u64, 500, 1_000, 2_000, 4_000, 8_000, 16_000];
        for (i, ms) in expected_ms.iter().enumerate() {
            let attempt = u32::try_from(i).expect("index fits u32") + 1;
            assert_eq!(
                delay_for(attempt),
                Duration::from_millis(*ms),
                "attempt {attempt} must double the previous delay"
            );
        }
        // Attempt 8 would be 32 s, past the ceiling, and everything after it
        // pins there rather than growing without bound.
        for attempt in [8u32, 9, 100, 10_000, u32::MAX] {
            assert_eq!(
                delay_for(attempt),
                RAMP_CAP,
                "attempt {attempt} must be clamped to the ceiling"
            );
        }
    }

    /// The ceiling is seconds, not minutes: these paths have no
    /// `NameOwnerChanged` to wake them, so the cap is also the worst case for
    /// noticing the bus came back. A regression that "fixed" the busy-loop by
    /// reaching for the 5-minute give-up cooldown would trade #653's spin for
    /// the stall the issue explicitly does not want.
    #[test]
    fn the_ceiling_recovers_within_seconds_not_minutes() {
        assert!(
            RAMP_CAP <= Duration::from_mins(1),
            "a name that becomes acquirable must be retried within a minute"
        );
        assert!(RAMP_BASE < RAMP_CAP, "the ramp must actually ramp");
    }

    /// The log cadence is geometric, so the *logging* has a ceiling too — not
    /// just the retry. Before #653 the non-transient arm logged on every
    /// attempt forever and the transient arm logged nothing at all.
    #[test]
    fn logs_only_at_each_doubling() {
        let logged: Vec<u32> = (1..=1024u32).filter(|a| logs_at(*a)).collect();
        assert_eq!(logged, vec![1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024]);
    }

    // ── The cursor form ─────────────────────────────────────────────────────

    /// The whole point of the ramp, stated as the budget it buys: an hour of
    /// unbroken failure must cost a bounded number of attempts and a handful
    /// of log lines.
    ///
    /// The flat 250 ms retry it replaces would spend 14 400 attempts in the
    /// same hour, so this fails loudly if [`delay_for`] is flattened back.
    #[test]
    fn an_hour_of_failure_is_bounded_in_attempts_and_lines() {
        let mut streak = FailureStreak::default();
        let mut elapsed = Duration::ZERO;
        let mut attempts = 0u32;
        let mut lines = 0u32;
        while elapsed < Duration::from_hours(1) {
            let backoff = streak.record();
            attempts += 1;
            if backoff.log {
                lines += 1;
            }
            elapsed += backoff.delay;
        }
        assert!(
            attempts <= 130,
            "an hour of failure must not cost more than ~2 attempts a minute, got {attempts}"
        );
        assert!(
            lines <= 8,
            "an hour of failure must not cost more than a handful of log lines, got {lines}"
        );
        // And it must not have gone so quiet that it is no longer retrying.
        assert!(
            attempts >= 100,
            "the 30 s cap implies ~120 attempts an hour"
        );
    }

    /// The other half of the trade: anything that works clears the streak, so
    /// a blip costs one 250 ms wait and never leaves a 30 s delay armed for
    /// the *next* unrelated failure.
    #[test]
    fn a_success_resets_the_ramp_to_the_base_delay() {
        let mut streak = FailureStreak::default();
        for _ in 0..12 {
            let _ = streak.record();
        }
        assert_eq!(
            streak.record().delay,
            RAMP_CAP,
            "a long streak must be at the ceiling before the reset means anything"
        );

        streak.reset();

        let after = streak.record();
        assert_eq!(after.attempt, 1);
        assert_eq!(
            after.delay, RAMP_BASE,
            "a success must put the next failure back at the base delay"
        );
        assert!(
            after.log,
            "the first failure of a fresh streak is always worth a line"
        );
    }

    /// The first few failures stay at `debug!` (a reconnect blip is already
    /// reported by `connection.rs`); a streak that outlives that explanation
    /// escalates and stays escalated.
    #[test]
    fn a_streak_escalates_from_blip_to_serious() {
        let mut streak = FailureStreak::default();
        let mut first_serious = None;
        for _ in 0..64 {
            let backoff = streak.record();
            if backoff.is_serious() && first_serious.is_none() {
                first_serious = Some(backoff.attempt);
            }
        }
        let first = first_serious.expect("a 64-failure streak must become serious");
        assert!(
            (2..=8).contains(&first),
            "escalation must happen within seconds, not attempts later; got {first}"
        );
    }

    // ── The cursor the `connection` supervisor collapsed into this ──────────
    //
    // Its private `Backoff` was duration-indexed (`next_ms`, doubled in
    // place); these four assertions came over with it, restated against
    // `FailureStreak::record().delay` — the call the supervisor now makes
    // where it used to say `next()`. They pin the *cursor* walk, which the
    // pure-function tests above do not: `delay_for(n)` staying correct would
    // not catch a `record()` that failed to advance.

    #[test]
    fn cursor_starts_at_250ms() {
        let mut b = FailureStreak::default();
        assert_eq!(b.record().delay, Duration::from_millis(250));
    }

    #[test]
    fn cursor_doubles_each_call() {
        let mut b = FailureStreak::default();
        assert_eq!(b.record().delay, Duration::from_millis(250));
        assert_eq!(b.record().delay, Duration::from_millis(500));
        assert_eq!(b.record().delay, Duration::from_secs(1));
        assert_eq!(b.record().delay, Duration::from_secs(2));
        assert_eq!(b.record().delay, Duration::from_secs(4));
    }

    #[test]
    fn cursor_clamps_at_30s_cap() {
        let mut b = FailureStreak::default();
        // 250 -> 500 -> 1000 -> 2000 -> 4000 -> 8000 -> 16000 -> (32000 clamped to) 30000.
        let mut last = Duration::default();
        for _ in 0..7 {
            last = b.record().delay;
        }
        assert_eq!(last, Duration::from_secs(16));
        // The next call is the first to clamp: 16000 * 2 = 32000 > cap, so the
        // *following* returned duration is capped at 30s.
        assert_eq!(b.record().delay, Duration::from_secs(30));
        // And it stays capped — doubling a capped value only re-clamps.
        assert_eq!(b.record().delay, Duration::from_secs(30));
        assert_eq!(b.record().delay, Duration::from_secs(30));
    }

    #[test]
    fn cursor_reset_returns_to_initial_value() {
        let mut b = FailureStreak::default();
        b.record();
        b.record();
        b.record();
        b.reset();
        assert_eq!(b.record().delay, Duration::from_millis(250));
    }
}
