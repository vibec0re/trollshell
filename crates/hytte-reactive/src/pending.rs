//! [`Pending`] — the one model for "the user flipped a toggle and the daemon
//! hasn't echoed it back yet".
//!
//! # Why this exists (#599)
//!
//! Every toggle in the shell is a write to a daemon the shell does not own:
//! `systemctl --user start`, a niri `Output` request, a `BlueZ` property set.
//! The read side of the same toggle comes back from that daemon — a poll, a
//! `PropertiesChanged`, an `ActiveState` re-read — which means there is always a
//! window between "the user flipped the switch" and "the daemon says so".
//! system-daemon-as-state-store (see `CLAUDE.md`) is deliberate and is not what
//! this module questions: the daemon still owns the value. What it does not own
//! is the user's *intent* during that window, because the user has not told the
//! daemon yet — the shell has, and it is still waiting.
//!
//! Rendering the daemon's reading alone during that window is what produces the
//! two symptoms this module exists to remove:
//!
//! - **The switch snaps back.** The next poll lands before the daemon has
//!   applied the write, so the row is rebuilt from a reading that still says
//!   "off" and the switch moves out from under the user's hand (#594).
//! - **Nothing happens for seconds.** A toggle that parks — on a location fix,
//!   a unit restart, a D-Bus round-trip — shows no motion at all, so "flip it
//!   on, see nothing, flip it back off" is the reasonable reaction (#597).
//!
//! Two different fixes for that had grown in the tree: a service-side tri-state
//! (`nightlight`'s `Off`/`Resolving`/`On`) and a widget-local
//! `Rc<RefCell<HashMap<_, bool>>>` of intent with a `glib` timeout (the displays
//! panel). #599 converged them here, on the service-side shape, and deleted the
//! widget-local one. The short version of that argument: intent kept in a widget
//! is state the registry cannot see, so two surfaces rendering the same toggle
//! each keep their own and time out independently — and clearing a widget-local
//! map re-renders nothing, so a write that never landed left the switch pinned
//! with no way back.
//!
//! # The shape
//!
//! One value, not two. A `confirmed: Mutable<T>` beside a
//! `pending: Mutable<bool>` would be two emissions a widget observes at
//! different times, so a row could momentarily render "off + spinning" or
//! "on + no spinner" out of a single logical transition. [`Pending`] carries
//! both halves, so one emission moves them together and no inconsistent pair is
//! reachable.
//!
//! ```
//! use hytte_reactive::Pending;
//! use std::time::Duration;
//!
//! // The daemon says the toggle is off.
//! let mut state = Pending::settled(false);
//! assert!(!*state.displayed());
//!
//! // The user flips it on. The write is in flight; the daemon still says off.
//! // The grace is how long we will hold the switch there without an echo.
//! state.request(true, Duration::from_secs(10));
//! assert!(state.is_pending());
//! assert!(!*state.confirmed(), "the daemon's reading is untouched");
//! assert!(*state.displayed(), "the switch stays where the user put it");
//!
//! // The daemon echoes. The intent has been honoured, so it retires.
//! state.settle(true);
//! assert!(!state.is_pending());
//! assert!(*state.displayed());
//! ```
//!
//! # The give-up is part of the type, not the call site
//!
//! A pending marker with no way out is worse than none: it pins the switch in
//! the position the user chose and never admits the write failed. So the
//! deadline is recorded *in the value* — [`Pending::request`] takes the grace,
//! [`Pending::deadline`] reads it back, and [`Pending::expire`] enforces it —
//! rather than being left to each service to remember. The two existing
//! parameterisations are `nightlight`'s coordinate wait and `displays`' niri
//! round-trip; they differ only in the [`Duration`] they pass.
//!
//! An intent retires on exactly three events, and something must own all three:
//!
//! 1. the daemon agrees — [`Pending::settle`] retires it for you;
//! 2. the request is cancelled or superseded — [`Pending::withdraw`];
//! 3. nothing happens before the deadline — [`Pending::expire`], which puts the
//!    row back on the daemon's reading so the failure is at least visible.
//!
//! Path 3 is the one that is easy to forget and the one whose absence is a bug
//! rather than a rough edge. Whatever holds the value has to call [`expire`] on
//! some tick it already has — a poll loop, a bounded `await`, a timer — because
//! a deadline nobody checks is just a comment.
//!
//! [`expire`]: Pending::expire

use std::time::{Duration, Instant};

/// One outstanding request: what the user asked for, and when we stop waiting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Request<T> {
    value: T,
    deadline: Instant,
}

/// A daemon-owned value plus the user's not-yet-echoed intent for it.
///
/// `confirmed` is the daemon's reading and only the daemon's reading — nothing
/// here ever guesses at it. The intent is what the user asked for while a write
/// is in flight; it is shell-side, always transient, and carries the deadline it
/// expires at (see the module docs).
///
/// `Copy` when `T` is, so the common `Pending<bool>` reads out of a
/// `Mutable::get()` without cloning.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Pending<T> {
    confirmed: T,
    intent: Option<Request<T>>,
}

impl<T> Pending<T> {
    /// The daemon's reading, with nothing in flight. The normal resting state,
    /// and what a failed, cancelled or expired write falls back to.
    #[must_use]
    pub const fn settled(confirmed: T) -> Self {
        Self {
            confirmed,
            intent: None,
        }
    }

    /// What the daemon last reported. Callers asking "is the thing *actually*
    /// running" want this, not [`Pending::displayed`].
    #[must_use]
    pub const fn confirmed(&self) -> &T {
        &self.confirmed
    }

    /// The in-flight request, if any.
    #[must_use]
    pub fn intent(&self) -> Option<&T> {
        self.intent.as_ref().map(|r| &r.value)
    }

    /// When the in-flight request gives up, if there is one. `None` means
    /// nothing is outstanding, never "waits forever".
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.intent.as_ref().map(|r| r.deadline)
    }

    /// What a widget bound to this state should show: the intent while one is
    /// outstanding, otherwise the daemon's reading.
    ///
    /// The intent wins on purpose. The user has just put the switch somewhere
    /// and the request is being honoured, so publishing the daemon's stale
    /// reading over it would move the control under their hand — the exact
    /// symptom this module exists to remove.
    #[must_use]
    pub const fn displayed(&self) -> &T {
        match &self.intent {
            Some(request) => &request.value,
            None => &self.confirmed,
        }
    }

    /// Whether a write is outstanding — the cue a spinner, a "waiting" subtitle
    /// or a progress affordance hangs off.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.intent.is_some()
    }

    /// Drop the intent without touching the daemon's reading: the explicit
    /// give-up, for a cancel or a supersession that does not want to wait out
    /// the deadline.
    pub fn withdraw(&mut self) {
        self.intent = None;
    }

    /// Drop the intent if its deadline has passed; returns whether it did.
    ///
    /// The timeout half of the contract. Call it from whatever tick already
    /// exists — a poll loop, a bounded `await` — since a deadline nobody checks
    /// changes nothing. Retiring here reverts the display to
    /// [`Pending::confirmed`], which is how a write that never landed becomes
    /// *visible* instead of sticking pending forever.
    pub fn expire(&mut self, now: Instant) -> bool {
        let expired = self.deadline().is_some_and(|deadline| now >= deadline);
        if expired {
            self.intent = None;
        }
        expired
    }
}

impl<T: PartialEq> Pending<T> {
    /// Record the user's request, to be given up on `grace` from now.
    ///
    /// A request for the value the daemon already reports is **not** pending:
    /// there is nothing to wait for, and putting a spinner up for a write that
    /// is already true would be the same dishonest feedback from the other
    /// side. Replaces any earlier intent — deadline included — so a rapid
    /// re-toggle leaves exactly the newest one outstanding with a full grace,
    /// never the older one's clock.
    pub fn request(&mut self, value: T, grace: Duration) {
        self.request_until(value, Instant::now() + grace);
    }

    /// [`Pending::request`] with the deadline stated outright. For services that
    /// compute it themselves, and for tests that must not read a wall clock.
    pub fn request_until(&mut self, value: T, deadline: Instant) {
        self.intent = (value != self.confirmed).then_some(Request { value, deadline });
    }

    /// Record a fresh reading from the daemon, retiring the intent **iff** the
    /// daemon now agrees with it.
    ///
    /// A reading that still disagrees leaves the intent standing, deadline
    /// untouched: the write is simply not there yet, and this is the ordinary
    /// case for a poller that ticks faster than the daemon applies. That wait is
    /// bounded by [`Pending::expire`], not by this.
    pub fn settle(&mut self, confirmed: T) {
        if self.intent.as_ref().is_some_and(|r| r.value == confirmed) {
            self.intent = None;
        }
        self.confirmed = confirmed;
    }
}

impl<T: Clone + PartialEq> Pending<T> {
    /// This state's outstanding request — deadline and all — carried onto a
    /// fresher reading of the same thing.
    ///
    /// For services whose daemon reading is re-derived wholesale each tick
    /// (`displays` rebuilds its whole `Vec<Output>` from every niri snapshot),
    /// so intent kept alongside can be re-seated without inventing a new
    /// deadline. Equivalent to [`Pending::settle`] on a copy: an echo retires
    /// the request, a disagreement keeps it, and the clock does not restart.
    #[must_use]
    pub fn rebased_on(&self, confirmed: T) -> Self {
        let mut next = Self::settled(confirmed);
        if let Some(request) = &self.intent {
            next.request_until(request.value.clone(), request.deadline);
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::Pending;
    use std::time::{Duration, Instant};

    const GRACE: Duration = Duration::from_secs(10);

    /// A request pinned to a deadline we choose, so nothing here reads a clock.
    fn requesting(confirmed: bool, value: bool, deadline: Instant) -> Pending<bool> {
        let mut state = Pending::settled(confirmed);
        state.request_until(value, deadline);
        state
    }

    #[test]
    fn a_settled_value_shows_the_daemons_reading() {
        let state = Pending::settled(true);
        assert!(!state.is_pending());
        assert_eq!(state.intent(), None);
        assert_eq!(
            state.deadline(),
            None,
            "nothing outstanding, nothing to time"
        );
        assert!(*state.confirmed());
        assert!(*state.displayed());
    }

    #[test]
    fn the_default_is_settled_on_the_types_default() {
        let state = Pending::<bool>::default();
        assert!(!state.is_pending());
        assert!(!*state.displayed());
    }

    #[test]
    fn an_intent_wins_the_display_without_touching_the_reading() {
        // The whole point: the switch stays where the user put it while the
        // daemon is still reporting the old value.
        let now = Instant::now();
        let state = requesting(false, true, now + GRACE);
        assert!(state.is_pending());
        assert_eq!(state.intent(), Some(&true));
        assert!(
            !*state.confirmed(),
            "the daemon's reading is never guessed at"
        );
        assert!(*state.displayed());
    }

    #[test]
    fn a_request_records_the_deadline_it_will_be_given_up_at() {
        // The give-up is a property of the value, not something a call site is
        // trusted to remember elsewhere.
        let mut state = Pending::settled(false);
        let before = Instant::now();
        state.request(true, GRACE);
        let deadline = state.deadline().expect("a request carries its deadline");
        assert!(deadline >= before + GRACE);
        assert!(deadline <= Instant::now() + GRACE);
    }

    #[test]
    fn requesting_what_the_daemon_already_reports_is_not_pending() {
        // No wait, so no spinner and no deadline: a pending affordance here
        // would be a claim that something is in flight when nothing is.
        let now = Instant::now();
        let state = requesting(true, true, now + GRACE);
        assert!(!state.is_pending());
        assert_eq!(state.deadline(), None);
        assert!(*state.displayed());
    }

    #[test]
    fn the_echo_retires_the_intent() {
        let now = Instant::now();
        let mut state = requesting(false, true, now + GRACE);
        state.settle(true);
        assert!(!state.is_pending());
        assert!(*state.confirmed());
        assert!(*state.displayed());
    }

    #[test]
    fn a_reading_that_still_disagrees_keeps_the_intent_standing() {
        // The ordinary case for a poller faster than the daemon: the write just
        // isn't there yet, and dropping the intent here is what made the switch
        // snap back in the first place. The clock must not restart either.
        let deadline = Instant::now() + GRACE;
        let mut state = requesting(false, true, deadline);
        state.settle(false);
        assert!(state.is_pending());
        assert!(!*state.confirmed());
        assert!(
            *state.displayed(),
            "the switch must not move under the user"
        );
        assert_eq!(
            state.deadline(),
            Some(deadline),
            "a poll tick is not a reason to extend the wait"
        );
    }

    #[test]
    fn a_daemon_side_change_lands_under_a_standing_intent() {
        // Someone toggled the thing outside the shell while our write was in
        // flight. The reading updates; the intent still disagrees, so it stands
        // until the echo or the deadline.
        let mut state = Pending::settled(0_u8);
        state.request_until(2, Instant::now() + GRACE);
        state.settle(1);
        assert_eq!(*state.confirmed(), 1);
        assert_eq!(*state.displayed(), 2);
        assert!(state.is_pending());
    }

    #[test]
    fn withdrawing_reverts_to_the_daemons_reading() {
        let now = Instant::now();
        let mut state = requesting(false, true, now + GRACE);
        state.withdraw();
        assert!(!state.is_pending());
        assert!(!*state.displayed());
        assert!(!*state.confirmed());
    }

    #[test]
    fn expiry_reverts_to_the_daemons_reading_and_only_at_the_deadline() {
        // Confirm-never-arrives. Before the deadline the switch holds; at it,
        // the write is admitted to have failed and the row goes back to truth.
        let asked_at = Instant::now();
        let deadline = asked_at + GRACE;
        let mut state = requesting(false, true, deadline);

        assert!(!state.expire(asked_at + GRACE / 2));
        assert!(state.is_pending(), "a healthy write still has its grace");
        assert!(*state.displayed());

        assert!(state.expire(deadline), "the deadline is inclusive");
        assert!(!state.is_pending());
        assert!(
            !*state.displayed(),
            "the failed toggle becomes visible instead of sticking pending"
        );
        assert!(!state.expire(deadline), "expiring twice is not an event");
    }

    #[test]
    fn expiring_a_settled_value_is_a_no_op() {
        let mut state = Pending::settled(true);
        assert!(!state.expire(Instant::now() + GRACE * 100));
        assert!(*state.displayed());
    }

    #[test]
    fn a_second_request_replaces_the_first_and_brings_its_own_deadline() {
        // Rapid double-toggle: exactly one intent is ever outstanding, it is the
        // newest, and the older request's clock leaves with it. The widget-local
        // model this replaced got that wrong — its first timer disarmed the
        // second toggle's intent early.
        let first_deadline = Instant::now() + GRACE;
        let second_deadline = first_deadline + GRACE;
        let mut state = Pending::settled(0_u8);
        state.request_until(1, first_deadline);
        state.request_until(2, second_deadline);
        assert_eq!(state.intent(), Some(&2));
        assert_eq!(state.deadline(), Some(second_deadline));
        assert!(!state.expire(first_deadline));
        assert!(state.is_pending());

        // Toggling back to the daemon's reading cancels the wait outright.
        let mut state = Pending::settled(false);
        state.request_until(true, first_deadline);
        state.request_until(false, second_deadline);
        assert!(!state.is_pending());
    }

    #[test]
    fn a_stale_echo_cannot_retire_a_newer_intent() {
        // The first request's echo arrives after the user has already asked for
        // something else. It updates the reading and nothing more.
        let deadline = Instant::now() + GRACE;
        let mut state = Pending::settled(0_u8);
        state.request_until(1, deadline);
        state.request_until(2, deadline);
        state.settle(1);
        assert_eq!(*state.confirmed(), 1);
        assert_eq!(*state.displayed(), 2);
        assert!(state.is_pending());
    }

    #[test]
    fn rebasing_carries_the_request_onto_a_fresh_reading_without_restarting_it() {
        let deadline = Instant::now() + GRACE;
        let state = requesting(false, true, deadline);

        let still_waiting = state.rebased_on(false);
        assert!(still_waiting.is_pending());
        assert_eq!(
            still_waiting.deadline(),
            Some(deadline),
            "re-seating on a new reading must not hand the write a fresh grace"
        );

        let echoed = state.rebased_on(true);
        assert!(!echoed.is_pending(), "a rebase onto agreement is the echo");
        assert!(*echoed.displayed());
    }

    #[test]
    fn rebasing_a_settled_value_stays_settled() {
        let state = Pending::settled(false);
        assert!(!state.rebased_on(true).is_pending());
        assert!(*state.rebased_on(true).displayed());
    }
}
