//! One retry policy for the crate's "the daemon was not there yet" loops —
//! issue #646.
//!
//! Two services grew the same policy independently: `wifi`'s backend probe
//! (#613) and `networkd`'s startup link seed (#621). They retry the same class
//! of transient system-bus failure, ship the same three numbers, and their
//! `backoff()` bodies were character-for-character identical. What kept them in
//! sync was a pair of doc comments cross-referencing each other **by hand** —
//! and that hand-sync had already failed once: #665 found that `networkd`
//! mirrored the policy but dropped the assertion pinning its ceiling.
//!
//! What lives here is the *mechanism*: the schedule, the pure decision, and the
//! invariants every shipped policy must hold. What stays at the call sites is
//! the *judgement* — which outcomes are worth retrying, why the shipped budget
//! is unbounded, and what each attempt logs.
//!
//! [`Policy::step`] only ever sees a plain `Result`, never the value inside it.
//! Deciding what counts as a retryable failure is therefore the caller's job,
//! which is exactly what lets one type serve both sites: `wifi` weighs
//! *inconclusive* ("I could not ask", retry) against *answered* ("the bus
//! replied", commit — including the answer "neither daemon is present") before
//! handing the `Result` over, while for `networkd` a refresh simply either read
//! the links or did not.
//!
//! #646's second half — `RECONNECT_RETRY` / `RECONNECT_RESET_AFTER` — reuses
//! [`Policy::backoff`] for a different shape of caller: `networkd`'s post-seed
//! listen loop, `mpris`, and `bluetooth` don't have a verdict to weigh at all
//! (a `listen()` stream ending is itself the reconnect signal, whatever it
//! returned), so they call `backoff` directly rather than going through
//! `step`, and layer on a caller-side reset threshold `step`'s callers don't
//! need. See both constants' docs.
//!
//! So one `Policy` type now carries **two shapes**: a budgeted, verdict-weighing
//! policy (`PROBE_RETRY`, `STARTUP_REFRESH_RETRY`) and an unbounded reconnect
//! ramp (`RECONNECT_RETRY`) that only borrows the schedule. Which of the two is
//! meant decides what this module can honestly be compared against — see the
//! `hytte_bus` bullet below, where the older wording quietly assumed the whole
//! type was the first shape.
//!
//! **Deliberately not folded in** — so nobody re-opens these as oversights:
//!
//! - `hytte_bus`'s ramp — `hytte_bus::backoff`'s `FailureStreak` / `delay_for`
//!   / `RetryStep`, hoisted out of `connection.rs` and `own.rs` by #797 (this
//!   bullet named `connection.rs`'s since-deleted private `Backoff` until #800).
//!   It is a stateful cursor over a pure 250 ms → 30 s schedule, with an
//!   attempt counter driving a log cadence, no attempt budget and no give-up.
//!   #646 rules it out by name, and #795 records that it is not to be
//!   relitigated — but the *reason* has to be stated as two halves, because
//!   only one of the two shapes above is genuinely a different kind of thing:
//!
//!   - [`Policy::step`]'s callers ([`crate::wifi::PROBE_RETRY`],
//!     [`crate::networkd::STARTUP_REFRESH_RETRY`]) weigh a `Result` per attempt
//!     against a budget that can answer [`Step::GiveUp`]. `hytte_bus`'s ramp has
//!     neither half — it never sees an outcome and never stops. **This is the
//!     shape that justifies the crate split.**
//!   - [`RECONNECT_RETRY`] does not. `max_attempts: None`, no verdict, callers
//!     that skip `step` for [`Policy::backoff`]: it is an unbounded ramp that
//!     differs from `hytte_bus`'s only in base delay (500 ms against 250 ms,
//!     same 30 s ceiling). It is kept here anyway, and not because it is a
//!     different kind of object: it retries a *subscription* on top of a
//!     connection `hytte_bus`'s supervisor is separately keeping alive, so
//!     during an outage both ramps run at once and their ceilings answer
//!     different questions — worst case for noticing the bus came back, versus
//!     per-service journal noise. Its companion [`RECONNECT_RESET_AFTER`] has
//!     no counterpart over there either (`FailureStreak` resets on a success,
//!     not on a run that stayed up), and `hytte_bus::backoff` is `pub(crate)`,
//!     so sharing it would mean publishing a transport crate's retry constants
//!     and dropping one of `SHIPPED`'s three entries.
//! - `crate::eds_retry` is the EDS worker threads' resilience kit — blocking,
//!   `std::sync::mpsc`-shaped, and it carries a failure-streak detector rather
//!   than an attempt budget.
//! - `hytte_reactive::spawn_supervised`'s private `Backoff` and
//!   `idle_notify::RetryBackoff` are the same reset-after-a-healthy-run idea as
//!   `RECONNECT_RESET_AFTER` below, but answer a different question each
//!   (restart-on-*panic*, and a `std::thread` observer loop's own reconnect)
//!   and are already stateful cursors rather than a pure schedule plus an
//!   external attempt count. Three independent shipped instances of the same
//!   30s judgement is itself worth a future look, but is not #646's scope.
//! - `wifi/watcher.rs`'s 2s sleep is a one-shot debounce before a `return`, not
//!   a retry loop. #646 names it explicitly so nobody "fixes" it into one.
//!
//! Everything here is pure — no bus, no clock, no tokio — so the tests are
//! hermetic and run in the default `cargo test` bucket.

use std::time::Duration;

/// A retry schedule: an attempt budget plus an exponential, capped delay.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Policy {
    /// Attempt budget, counting the first try. `None` means "retry forever".
    pub(crate) max_attempts: Option<u32>,
    /// Delay before the first retry; doubles with each further attempt.
    pub(crate) initial: Duration,
    /// Ceiling the doubling delay is clamped to.
    pub(crate) max_backoff: Duration,
}

/// What to do after one attempt.
///
/// The whole decision lives in [`Policy::step`] — a pure function — so it is
/// unit-testable without a bus.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Step {
    /// The attempt succeeded. Stop retrying.
    ///
    /// Carries no payload on purpose. What the success *was* is the caller's to
    /// read off its own `Result`, and that is what lets one policy serve a
    /// `Result<(), E>` seed and a `Result<BackendChoice, E>` probe alike (#646).
    /// It also makes it impossible for a caller to invent a verdict out of an
    /// error, which is the shape #613 regressed on.
    Proceed,
    /// It failed and attempts remain: wait, then try again.
    Retry {
        /// How long to wait before the next attempt.
        after: Duration,
    },
    /// Still failing and the attempt budget is spent: the caller should log
    /// loudly and stop.
    ///
    /// **Not reachable under either shipped policy** — both are unbounded (see
    /// `wifi::PROBE_RETRY` and `networkd::STARTUP_REFRESH_RETRY` for why, argued
    /// on #613 and #621 respectively). Kept expressible, and tested, so a
    /// bounded policy stays one field away.
    GiveUp,
}

impl Policy {
    /// Delay before the retry that follows `attempt` (1-based): [`Self::initial`]
    /// doubled once per elapsed attempt, clamped to [`Self::max_backoff`].
    ///
    /// Saturating throughout, so an absurd `attempt` clamps to the ceiling
    /// rather than overflowing: `checked_shl` returns `None` once the shift
    /// reaches 32, and the multiply saturates before the `min`.
    ///
    /// `pub(crate)`, not just used internally by [`Self::step`]: the crate's
    /// `listen()`-style reconnect loops (`RECONNECT_RETRY`, below) have no
    /// `Ok`/`Err`-shaped verdict to weigh — the stream ending is itself the
    /// signal to reconnect, whatever it returned — so those callers compute a
    /// delay directly rather than going through `step`.
    pub(crate) fn backoff(self, attempt: u32) -> Duration {
        let factor = 1_u32
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.initial.saturating_mul(factor).min(self.max_backoff)
    }

    /// The pure retry/proceed decision. `attempt` is 1-based and counts the try
    /// that produced `outcome`.
    ///
    /// Generic over the `Result` because the policy weighs only *whether* the
    /// attempt succeeded, never what it returned. A caller for which some `Ok`
    /// values are still worth retrying — or some `Err` values are not — must
    /// resolve that before calling; see `wifi::probe_until_conclusive`, where
    /// `Ok(BackendChoice::None)` is a real finding and must commit rather than
    /// be asked again.
    pub(crate) fn step<T, E>(self, outcome: &Result<T, E>, attempt: u32) -> Step {
        match outcome {
            Ok(_) => Step::Proceed,
            Err(_) if self.max_attempts.is_some_and(|max| attempt >= max) => Step::GiveUp,
            Err(_) => Step::Retry {
                after: self.backoff(attempt),
            },
        }
    }
}

/// Backoff for the crate's `listen()`-style reconnect loops — #646's second
/// half: `networkd`'s post-seed listen loop, `mpris`, and `bluetooth`. Same
/// 500ms floor as [`crate::wifi::PROBE_RETRY`] /
/// [`crate::networkd::STARTUP_REFRESH_RETRY`], but a 30s ceiling rather than
/// 8s — those two exist for a boot race that should resolve in seconds; a
/// daemon vanishing mid-session is a slower-moving problem, and a longer
/// ceiling means fewer `warn!` lines in an already-long-lived process when it
/// stays down. 30s also matches [`RECONNECT_RESET_AFTER`], and
/// `hytte_reactive::spawn_supervised`'s own panic-restart backoff, which caps
/// at the same 30s for the same "how long is too long to keep the journal
/// noisy" judgement — not reused directly here (that type answers "did the
/// task *panic*", a different question from "did `listen` return"), but not
/// re-derived either.
///
/// Callers own the reconnect-forever `loop`; there is no `Ok` verdict to
/// commit to here the way [`Step::Proceed`] means for [`Policy::step`]'s other
/// callers; delay is [`Policy::backoff`] directly. See `RECONNECT_RESET_AFTER`
/// for why every caller must also track how long the run that just ended
/// stayed up.
///
/// **This is the one shipped policy that is not a budget.** Unbounded, no
/// verdict, no give-up — the same object `hytte_bus::backoff` is, at 500 ms
/// rather than 250 ms and with the same 30 s ceiling. It is kept here anyway,
/// for reasons that are about layering rather than shape; the module doc's
/// `hytte_bus` bullet argues it. Do not read the resemblance as an invitation
/// to merge them (#646, #795), and do not read the module's "different shape"
/// framing as covering this constant — it does not.
pub(crate) const RECONNECT_RETRY: Policy = Policy {
    max_attempts: None,
    initial: Duration::from_millis(500),
    max_backoff: Duration::from_secs(30),
};

/// How long a `listen()` run must have stayed up before its caller treats the
/// *next* failure as a fresh problem — attempt count back to 1 — rather than a
/// continuation of the same outage.
///
/// [`Policy::backoff`] is pure and stateless; it does not know how long
/// anything ran, so this is a plain caller-side threshold, not a `Policy`
/// field — nothing about [`RECONNECT_RETRY`] not reset. Without a reset, a
/// daemon that is merely flaky — reconnecting cleanly every few minutes —
/// ratchets its caller's attempt counter to the 30s ceiling and stays there,
/// because nothing ever brings it back down: strictly worse than the flat 2s
/// cadence this replaces, for exactly the peer the change is meant to help.
///
/// 30s is not a fresh number: it is the same threshold
/// `hytte_reactive::spawn_supervised`'s `Backoff::reset_after` and
/// `idle_notify::RetryBackoff::reset_after` already use for the identical
/// "was that run healthy" judgement. Matching them here means a reader who
/// has seen either does not need to learn a third answer.
pub(crate) const RECONNECT_RESET_AFTER: Duration = Duration::from_secs(30);

/// Every retry policy this crate **ships**, so the `every_shipped_policy_*`
/// tests below see all of them.
///
/// **A new shipped policy belongs in this list.** These are the only tests that
/// look at real numbers rather than a test-local schedule, and #665 exists
/// precisely because the second policy shipped without them: its own test
/// compared `step`'s delay against `policy.backoff(attempt)` — the very
/// expression `step` computes internally — so `initial: Duration::ZERO` kept the
/// whole suite green while turning the retry into a tight `error!` flood.
#[cfg(test)]
const SHIPPED: &[(&str, Policy)] = &[
    ("wifi::PROBE_RETRY", crate::wifi::PROBE_RETRY),
    ("retry::RECONNECT_RETRY", RECONNECT_RETRY),
    (
        "networkd::STARTUP_REFRESH_RETRY",
        crate::networkd::STARTUP_REFRESH_RETRY,
    ),
];

#[cfg(test)]
mod tests {
    use super::{Policy, SHIPPED, Step};
    use std::time::Duration;

    /// A *bounded* policy with tiny delays, so the give-up path stays reachable
    /// and named. Deliberately not one of the shipped constants — these tests
    /// assert the mechanism, so tuning a shipped policy must not redden them.
    /// The shipped numbers are asserted separately, at the bottom of this file.
    fn bounded() -> Policy {
        Policy {
            max_attempts: Some(3),
            initial: Duration::from_millis(10),
            max_backoff: Duration::from_millis(40),
        }
    }

    /// A failed attempt. The error type is irrelevant to the policy — that is
    /// the whole point of the generic `step`.
    fn failed() -> Result<(), &'static str> {
        Err("the daemon did not answer")
    }

    #[test]
    fn a_successful_attempt_proceeds_whenever_it_arrives() {
        // A local binding rather than a helper fn: a function that always
        // returns `Ok` trips `clippy::unnecessary_wraps`, and the wrapper is the
        // whole point here — `step` decides on the `Result`, not on a bool.
        let landed: Result<(), &'static str> = Ok(());
        assert_eq!(bounded().step(&landed, 1), Step::Proceed);
        // Success ends the retrying whenever it arrives, not just first time.
        assert_eq!(bounded().step(&landed, 3), Step::Proceed);
        assert_eq!(bounded().step(&landed, u32::MAX), Step::Proceed);
    }

    #[test]
    fn a_failed_attempt_retries_while_the_budget_lasts() {
        assert_eq!(
            bounded().step(&failed(), 1),
            Step::Retry {
                after: Duration::from_millis(10)
            },
            "the first failure must schedule another attempt, not end the loop (#613, #621)"
        );
        assert_eq!(
            bounded().step(&failed(), 2),
            Step::Retry {
                after: Duration::from_millis(20)
            }
        );
    }

    #[test]
    fn a_bounded_policy_gives_up_once_its_budget_is_spent() {
        assert_eq!(bounded().step(&failed(), 3), Step::GiveUp);
        assert_eq!(bounded().step(&failed(), 4), Step::GiveUp);
        assert_eq!(bounded().step(&failed(), 99), Step::GiveUp);
    }

    #[test]
    fn an_unbounded_policy_never_gives_up() {
        let forever = Policy {
            max_attempts: None,
            ..bounded()
        };
        for attempt in [1_u32, 3, 100, u32::MAX] {
            assert!(
                matches!(forever.step(&failed(), attempt), Step::Retry { .. }),
                "attempt {attempt}: an unbounded policy stopped asking"
            );
        }
    }

    #[test]
    fn backoff_doubles_and_clamps_to_the_ceiling() {
        let policy = bounded();
        assert_eq!(policy.backoff(1), Duration::from_millis(10));
        assert_eq!(policy.backoff(2), Duration::from_millis(20));
        assert_eq!(policy.backoff(3), Duration::from_millis(40));
        // Clamped, and the shift can't overflow at absurd attempt counts.
        assert_eq!(policy.backoff(4), Duration::from_millis(40));
        assert_eq!(policy.backoff(u32::MAX), Duration::from_millis(40));
    }

    // ── The shipped constants (#665) ─────────────────────────────────────────
    //
    // Every assertion below compares against a *literal* — a field of the policy
    // under test, or `Duration::ZERO` — and never against `policy.backoff(..)`,
    // which is the expression `step` computes internally and so pins nothing
    // about the number it produces. That re-computation is the tautology #665
    // filed.

    /// The falsification #665 documented: set `initial: Duration::ZERO` on
    /// either shipped policy and everything else stays green, because expected
    /// and computed both become zero. The resulting behaviour is
    /// `sleep(Duration::ZERO)` on every retry — and since *both* loops log every
    /// attempt at `error!`, that is not a fast retry, it is a tight journal
    /// flood as fast as the bus can fail. Both policies' own docs argue that
    /// cannot happen "because the backoff caps"; these two lines are what make
    /// the argument true.
    #[test]
    fn every_shipped_policy_sleeps_a_nonzero_time_and_reaches_its_ceiling() {
        for (name, policy) in SHIPPED {
            assert!(
                policy.backoff(1) > Duration::ZERO,
                "{name}: the first retry delay is zero — every attempt logs at `error!`, so this \
                 is a tight log flood rather than a backoff (#665)"
            );
            assert_eq!(
                policy.backoff(u32::MAX),
                policy.max_backoff,
                "{name}: the doubling never actually reaches `max_backoff`, so the documented \
                 ceiling is not the real one (#665)"
            );
            // …and `step` is wired to that ceiling, not merely to some delay.
            // Unbounded policies only: a bounded one gives up long before
            // `u32::MAX` attempts, which `a_bounded_policy_gives_up_…` covers.
            if policy.max_attempts.is_none() {
                assert_eq!(
                    policy.step(&failed(), u32::MAX),
                    Step::Retry {
                        after: policy.max_backoff
                    },
                    "{name}: `step` does not settle at the literal `max_backoff` (#665)"
                );
            }
        }
    }

    /// Prompt, because the case both policies exist for is a boot race of well
    /// under a second — the first retry must not sit behind the ceiling.
    /// Audible, because every attempt logs at `error!`, which is only defensible
    /// while the delay stays capped: an uncapped backoff would decay into a
    /// silent poll, the failure mode the observability requirement rules out.
    #[test]
    fn every_shipped_policy_starts_promptly_and_stays_audible() {
        for (name, policy) in SHIPPED {
            assert!(
                policy.backoff(1) <= Duration::from_secs(1),
                "{name}: the first retry is too slow to catch a daemon that is only milliseconds \
                 behind the shell"
            );
            assert!(
                policy.max_backoff >= policy.initial,
                "{name}: the ceiling is below the floor"
            );
            assert!(
                policy.max_backoff <= Duration::from_secs(30),
                "{name}: the retry logs every attempt; a longer delay would let a dead daemon go \
                 quiet in the journal"
            );
        }
    }
}
