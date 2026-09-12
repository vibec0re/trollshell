//! The **visibility-gated poll loop**: park while the mount surface is
//! hidden, refresh the instant it opens, then re-poll on a cadence until it
//! hides again.
//!
//! Three plugins had each written this out by hand — `departures`
//! (`feed.rs`), `usage` (`fetch.rs`) and `agents` (`poll.rs`) — down to
//! near-verbatim comments on the `biased;` and on the `, if visible` guard
//! (#1168, from the #1162 sweep). The loop is a dozen lines, but every one of
//! them is load-bearing and none of them is obvious, which is exactly the
//! shape that drifts: a fourth author copies the twelve lines, gets eleven of
//! them, and the one they dropped is a behaviour nobody notices until the
//! sidebar has been open for an hour.
//!
//! # What the gate owns (and why each part is there)
//!
//! - **`, if visible` — the parking.** While the surface is hidden the
//!   interval is not polled at all: no ticks, no I/O, no wakeups. This is the
//!   whole energy argument for a sidebar plugin, and the reason a poll-based
//!   plugin is acceptable at all.
//! - **[`MissedTickBehavior::Delay`](tokio::time::MissedTickBehavior::Delay).**
//!   A refresh that takes longer than the period, or a caller that is slow to
//!   come back for the next [`Wake`], must not be paid back as a *burst* of
//!   catch-up refreshes against whatever is on the other end of the fetch.
//!   The cadence is "at least `period` between refreshes", never "`period`
//!   on average".
//! - **`biased;` — the lane before the tick.** When a close and a due tick are
//!   both ready, the close wins deterministically, so hiding the surface parks
//!   the poller rather than firing one more round trip first. Without it
//!   `select!` picks at random and the extra fetch happens about half the time.
//! - **[`reset`](Gate::reset) on the open edge.** The edge owes an immediate
//!   refresh; resetting first means the refresh is followed by a *full* period
//!   of quiet rather than by whatever fraction of the old period happened to
//!   be left.
//! - **The edge, not the level.** Only a hidden→visible transition refreshes.
//!   A host that re-sends `visible = true` (a re-subscribe, a duplicate
//!   snapshot) must not turn into a fetch.
//! - **A closed lane is a teardown.** [`Gate::next`] answers `None` when the
//!   [`CmdReceiver`] closes, which is the session ending — the task returns
//!   rather than polling on against a dropped reducer.
//!
//! # What the gate deliberately does *not* do
//!
//! It **never cancels work in flight**. The caller awaits its own refresh
//! between two [`next`](Gate::next) calls, so the `select!` is not running
//! while that fetch is: a close arriving mid-fetch is read afterwards and
//! parks the *next* poll, and the in-flight one still delivers its message.
//! That is what all three hand-written copies did, and it is the behaviour a
//! reducer wants — a fetch that was already paid for should still land.
//!
//! # Two entry points
//!
//! [`gated`] is the whole loop for a plugin whose command lane carries
//! *nothing but* the visibility flip (`departures`, `usage`). [`Gate`] is the
//! driver underneath it, for a lane that carries more (`agents`, whose
//! `Cmd::Send` writes to the hive and then forces a re-poll) or a task that
//! needs to re-arm the cadence from a reloaded config.

use std::future::Future;
use std::time::Duration;

use crate::CmdReceiver;

/// Whether a requested visibility state opens the gate — the hidden→visible
/// **edge**, which is the only transition that owes an immediate refresh.
///
/// Pure, so the transition is testable without a runtime or a live fetch (it
/// is the `on_visibility` that `departures` and `usage` each carried a copy
/// of).
fn opens(current: bool, requested: bool) -> bool {
    requested && !current
}

/// Why [`Gate::next`] woke the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wake<C> {
    /// Do the periodic work now: either the gate just opened (hidden→visible)
    /// or the cadence fired while it was open.
    ///
    /// The two are deliberately one variant. All three loops this was hoisted
    /// from ran the *same* body for both — the open edge's whole difference is
    /// the [`reset`](Gate::reset) the gate has already done by the time this
    /// is handed over. If a caller ever genuinely needs to tell them apart,
    /// that is a field on this variant, not a second loop.
    Refresh,
    /// A command the gate does not understand — the classifier answered
    /// `None` — handed straight back for the caller to act on. The gate's own
    /// state is untouched, so a caller that wants this command to also force a
    /// refresh calls [`reset`](Gate::reset) and does the work itself.
    Cmd(C),
}

/// What one turn of the `select!` produced. Private: it exists only so the
/// futures borrowing `self`'s fields are dropped before the handler touches
/// `self` as a whole (`reset` takes `&mut self`).
enum Step<C> {
    /// The lane closed — the session is tearing down.
    Closed,
    /// A visibility command, already classified.
    Visibility(bool),
    /// Anything else on the lane.
    Cmd(C),
    /// The cadence fired while the gate was open.
    Tick,
}

/// The visibility-gated poll driver: owns the command lane, the cadence and
/// the gate state, and hands the caller one [`Wake`] at a time.
///
/// See the [module docs](self) for what each part of the loop buys. Use
/// [`gated`] instead when the lane carries nothing but the visibility flip.
///
/// ```
/// use std::time::Duration;
/// use hytte_plugin::poll::{Gate, Wake};
///
/// enum Cmd {
///     /// The mount surface showed or hid.
///     SetVisible(bool),
///     /// Write something, then reconcile against reality immediately.
///     Write(String),
/// }
///
/// async fn io_task(cmds: hytte_plugin::CmdReceiver<Cmd>) {
///     let mut gate = Gate::new(cmds, Duration::from_secs(30), |cmd: &Cmd| match cmd {
///         Cmd::SetVisible(v) => Some(*v),
///         Cmd::Write(_) => None,
///     });
///     while let Some(wake) = gate.next().await {
///         match wake {
///             Wake::Refresh => { /* one fetch; send the result to the reducer */ }
///             Wake::Cmd(Cmd::Write(body)) => {
///                 let _ = body; // … the write …
///                 gate.reset(); // the re-poll below restarts the cadence
///                 /* … one fetch … */
///             }
///             // The gate answers `SetVisible` itself, as an open edge.
///             Wake::Cmd(Cmd::SetVisible(_)) => unreachable!(),
///         }
///     }
/// }
/// ```
pub struct Gate<C, V> {
    cmds: CmdReceiver<C>,
    visibility: V,
    interval: tokio::time::Interval,
    visible: bool,
}

impl<C, V: Fn(&C) -> Option<bool>> Gate<C, V> {
    /// A gate over `cmds`, refreshing every `period` while open.
    ///
    /// `visibility` classifies each command: `Some(want)` is a visibility
    /// change the gate absorbs (answering an open edge with
    /// [`Wake::Refresh`]), `None` is anything else, handed back as
    /// [`Wake::Cmd`].
    ///
    /// The gate starts **closed** — the runtime's seeded
    /// [`Input::SlotVisible`](crate::Input::SlotVisible) is whatever the
    /// surface happens to be, and a task that wants one poll before any edge
    /// (`agents`' seed poll) does it itself before the loop.
    #[must_use]
    pub fn new(cmds: CmdReceiver<C>, period: Duration, visibility: V) -> Self {
        Self {
            cmds,
            visibility,
            interval: fresh(period, true),
            visible: false,
        }
    }

    /// Wait for the next thing worth doing: `None` once the command lane
    /// closes (the session is tearing down), otherwise one [`Wake`].
    ///
    /// Visibility commands are absorbed — a hidden→visible edge resets the
    /// cadence and answers [`Wake::Refresh`]; a redundant or closing one
    /// answers nothing at all and keeps waiting.
    pub async fn next(&mut self) -> Option<Wake<C>> {
        loop {
            // Scoped so both `select!` futures (each holding a `&mut` to one
            // of `self`'s fields) are dropped before the handler below calls
            // `self.reset()`, which needs `&mut self` whole.
            let step = {
                let cmds = &mut self.cmds;
                let interval = &mut self.interval;
                let visibility = &self.visibility;
                let open = self.visible;
                tokio::select! {
                    // The lane before the tick: a close must park the poller
                    // rather than let a simultaneously-due tick fire one more
                    // round trip first.
                    biased;
                    cmd = cmds.recv() => match cmd {
                        None => Step::Closed,
                        Some(cmd) => match visibility(&cmd) {
                            Some(want) => Step::Visibility(want),
                            None => Step::Cmd(cmd),
                        },
                    },
                    // Disabled while hidden — the poller parks: no ticks, no
                    // I/O. `Interval::tick` is cancel-safe, so losing this arm
                    // to the lane does not lose a tick.
                    _ = interval.tick(), if open => Step::Tick,
                }
            };

            match step {
                Step::Closed => return None,
                Step::Cmd(cmd) => return Some(Wake::Cmd(cmd)),
                Step::Tick => return Some(Wake::Refresh),
                Step::Visibility(want) => {
                    let opened = opens(self.visible, want);
                    self.visible = want;
                    if opened {
                        // Reset *before* handing the refresh over, so the
                        // immediate refresh the edge owes is followed by a
                        // full period rather than by the remains of the old
                        // one.
                        self.reset();
                        return Some(Wake::Refresh);
                    }
                }
            }
        }
    }

    /// Restart the cadence from now: the next [`Wake::Refresh`] is a full
    /// period away.
    ///
    /// The gate does this itself on an open edge. Call it when the caller
    /// refreshes for a reason of its own — `agents` does, after a
    /// [`Wake::Cmd`] write that is followed by a reconciling poll — so the
    /// cadence does not fire again immediately on top of it.
    pub fn reset(&mut self) {
        self.interval.reset();
    }

    /// Change the cadence, restarting it from now.
    ///
    /// For a task whose period comes from a config file that can be edited
    /// while it runs (`agents`' `agents.toml`). Unlike
    /// [`tokio::time::interval`], the new cadence has **no immediate leading
    /// tick**: the next [`Wake::Refresh`] is one whole `period` away, the same
    /// as after [`reset`](Gate::reset). A caller that wants a refresh right
    /// now does it itself — which is what re-arming from inside a refresh
    /// already amounts to.
    pub fn set_period(&mut self, period: Duration) {
        self.interval = fresh(period, false);
    }

    /// Whether the gate is currently open (the mount surface is visible).
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }
}

/// An interval with the gate's missed-tick policy. `leading` picks whether the
/// first tick is due immediately (construction, where the gate is closed
/// anyway so nothing can consume it) or one period out (a re-arm).
fn fresh(period: Duration, leading: bool) -> tokio::time::Interval {
    let mut interval = if leading {
        tokio::time::interval(period)
    } else {
        tokio::time::interval_at(tokio::time::Instant::now() + period, period)
    };
    // A slow refresh (or a slow caller) must not be paid back as a burst of
    // catch-up refreshes — see the module docs.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// The whole visibility-gated poll loop, for a plugin whose command lane
/// carries **nothing but** the visibility flip: park while hidden, refresh on
/// the open edge, re-poll every `period` until hidden again, return when the
/// lane closes.
///
/// `visibility` reads the flip out of a command. Spelling it as an irrefutable
/// pattern (`|&Cmd::SetVisible(v)| v`) is the point: a plugin that later grows
/// a second command variant gets a **compile error** here rather than a
/// silently misclassified command, and moves to [`Gate`], whose [`Wake::Cmd`]
/// hands the new variant back.
///
/// ```
/// use std::time::Duration;
/// use hytte_plugin::poll;
///
/// const REFRESH_WHILE_OPEN: Duration = Duration::from_secs(30);
///
/// enum Cmd {
///     SetVisible(bool),
/// }
///
/// async fn fetch_and_send() { /* one fetch; send the result to the reducer */ }
///
/// async fn poll_task(cmds: hytte_plugin::CmdReceiver<Cmd>) {
///     poll::gated(
///         cmds,
///         REFRESH_WHILE_OPEN,
///         |&Cmd::SetVisible(v)| v,
///         || fetch_and_send(),
///     )
///     .await;
/// }
/// ```
/// `on_refresh` is a plain closure returning a future rather than an
/// `AsyncFnMut`: the sugar's future is higher-ranked over the call's lifetime,
/// which the compiler cannot then prove `Send` for, and every caller here
/// hands this task to `tokio::spawn`. Measured — `departures` fails to compile
/// with *implementation of `Send` is not general enough*.
pub async fn gated<C, F: Future<Output = ()>>(
    cmds: CmdReceiver<C>,
    period: Duration,
    visibility: impl Fn(&C) -> bool,
    mut on_refresh: impl FnMut() -> F,
) {
    let mut gate = Gate::new(cmds, period, move |cmd: &C| Some(visibility(cmd)));
    while let Some(wake) = gate.next().await {
        match wake {
            Wake::Refresh => on_refresh().await,
            // Unreachable: the classifier above answers `Some` for every
            // command, so the gate absorbs the whole lane. A plugin whose lane
            // carries more drives `Gate` directly and matches this arm.
            Wake::Cmd(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    //! The gate's own behaviour, hermetically: no socket, no HTTP, no clock.
    //!
    //! Every test runs `#[tokio::test(start_paused = true)]` — the cadence
    //! *is* the subject, so the virtual clock is what makes "ten periods went
    //! by and nothing refreshed" a statement about the gate rather than about
    //! how long the test slept.
    //!
    //! | mechanism | test |
    //! | --- | --- |
    //! | `, if visible` | [`a_closed_gate_never_refreshes`] |
    //! | the hidden→visible edge (and only it) | [`only_the_hidden_to_visible_edge_refreshes`] |
    //! | `reset()` on the open edge | [`the_open_edge_refresh_is_followed_by_a_full_period`] |
    //! | `MissedTickBehavior::Delay` | [`a_slow_refresh_is_not_paid_back_as_a_burst`] |
    //! | `biased;` | [`a_close_beats_a_due_tick`] |
    //! | no cancellation of in-flight work | [`a_close_mid_refresh_parks_the_next_poll_not_this_one`] |
    //! | a closed lane ends the loop | [`a_closed_lane_ends_the_loop`] |
    //! | `Wake::Cmd` passthrough | [`an_unclassified_command_is_handed_back`] |
    //! | `set_period` | [`set_period_re_arms_from_now`] |

    use std::time::Duration;

    use super::{Gate, Wake};
    use crate::cmd_channel;

    const P: Duration = Duration::from_secs(10);

    #[derive(Debug, PartialEq, Eq)]
    enum Cmd {
        SetVisible(bool),
        Other(u8),
    }

    fn gate(cmds: crate::CmdReceiver<Cmd>) -> Gate<Cmd, fn(&Cmd) -> Option<bool>> {
        fn classify(cmd: &Cmd) -> Option<bool> {
            match cmd {
                Cmd::SetVisible(v) => Some(*v),
                Cmd::Other(_) => None,
            }
        }
        Gate::new(cmds, P, classify as fn(&Cmd) -> Option<bool>)
    }

    /// `next()` with a deadline, in **virtual** time — never a bare `.await`.
    /// A gate that stopped waking would otherwise hang the test binary instead
    /// of failing it by name.
    async fn wake_soon(gate: &mut Gate<Cmd, fn(&Cmd) -> Option<bool>>) -> Option<Wake<Cmd>> {
        tokio::time::timeout(Duration::from_hours(1), gate.next())
            .await
            .expect("the gate must wake within an hour of virtual time")
    }

    /// `true` iff the gate wakes at all within `window` of virtual time. Every
    /// caller below has only visibility commands in flight, which the gate
    /// absorbs, so the only wake it can produce is a [`Wake::Refresh`].
    async fn wakes_within(
        gate: &mut Gate<Cmd, fn(&Cmd) -> Option<bool>>,
        window: Duration,
    ) -> bool {
        tokio::time::timeout(window, gate.next()).await.is_ok()
    }

    /// **The parking.** A closed gate makes no refreshes at all, however much
    /// time passes.
    ///
    /// Falsification: drop `, if open` from the tick arm in [`Gate::next`] and
    /// this goes red immediately.
    #[tokio::test(start_paused = true)]
    async fn a_closed_gate_never_refreshes() {
        let (_tx, rx) = cmd_channel();
        let mut g = gate(rx);
        assert!(
            !wakes_within(&mut g, P * 100).await,
            "a parked gate must not refresh"
        );
        assert!(!g.is_visible());
    }

    /// **The edge, not the level.** Exactly the four transitions the two
    /// hand-written `on_visibility` copies asserted on, now observable through
    /// the gate itself: open refreshes once, a redundant open does not, and
    /// neither closing transition does.
    ///
    /// Falsification: change `opens` to `requested` and the redundant-open
    /// assertion goes red.
    #[tokio::test(start_paused = true)]
    async fn only_the_hidden_to_visible_edge_refreshes() {
        let (tx, rx) = cmd_channel();
        let mut g = gate(rx);

        // hidden → hidden: nothing.
        tx.send(Cmd::SetVisible(false)).expect("open lane");
        assert!(!wakes_within(&mut g, P / 2).await, "false → false");

        // hidden → visible: one immediate refresh.
        tx.send(Cmd::SetVisible(true)).expect("open lane");
        assert_eq!(wake_soon(&mut g).await, Some(Wake::Refresh));
        assert!(g.is_visible());

        // visible → visible: no *edge* refresh. (Checked well inside one
        // period, so the cadence cannot be what answers.)
        tx.send(Cmd::SetVisible(true)).expect("open lane");
        assert!(!wakes_within(&mut g, P / 2).await, "true → true");

        // visible → hidden: nothing, and the cadence stops.
        tx.send(Cmd::SetVisible(false)).expect("open lane");
        assert!(!wakes_within(&mut g, P * 10).await, "true → false");
        assert!(!g.is_visible());
    }

    /// **`reset()` on the open edge.** After the edge's immediate refresh the
    /// next one is a *full* period away, not whatever was left of the period
    /// that was already running when the gate was constructed.
    ///
    /// Falsification: delete the `self.reset()` in the `Step::Visibility` arm.
    /// The interval built at construction is already overdue by then, so the
    /// second refresh arrives instantly and the `P * 9 / 10` assertion reds.
    #[tokio::test(start_paused = true)]
    async fn the_open_edge_refresh_is_followed_by_a_full_period() {
        let (tx, rx) = cmd_channel();
        let mut g = gate(rx);

        // Let the construction-time interval fall well behind while parked.
        tokio::time::advance(P * 5).await;

        tx.send(Cmd::SetVisible(true)).expect("open lane");
        assert_eq!(wake_soon(&mut g).await, Some(Wake::Refresh), "the edge");

        assert!(
            !wakes_within(&mut g, P * 9 / 10).await,
            "the edge's refresh must be followed by a full period of quiet"
        );
        assert_eq!(wake_soon(&mut g).await, Some(Wake::Refresh), "the cadence");
    }

    /// **`MissedTickBehavior::Delay`.** A refresh that outlasts several
    /// periods is not paid back as a burst: the next refresh comes one whole
    /// period after the slow one finished, not immediately and not three times
    /// over.
    ///
    /// Falsification: swap the policy for `Burst` (or drop the
    /// `set_missed_tick_behavior` call, whose default *is* `Burst`) and the
    /// "no immediate catch-up" assertion goes red — the gate hands back three
    /// refreshes back to back.
    #[tokio::test(start_paused = true)]
    async fn a_slow_refresh_is_not_paid_back_as_a_burst() {
        let (tx, rx) = cmd_channel();
        let mut g = gate(rx);

        tx.send(Cmd::SetVisible(true)).expect("open lane");
        assert_eq!(wake_soon(&mut g).await, Some(Wake::Refresh), "the edge");

        // The caller's own work takes three and a half periods.
        tokio::time::sleep(P * 7 / 2).await;

        // The overdue tick is owed once…
        assert_eq!(
            wake_soon(&mut g).await,
            Some(Wake::Refresh),
            "the owed tick"
        );
        // …and then the cadence restarts from now, rather than firing the two
        // further periods that elapsed during the slow refresh.
        assert!(
            !wakes_within(&mut g, P * 9 / 10).await,
            "a missed tick must not be paid back as a burst"
        );
        assert_eq!(wake_soon(&mut g).await, Some(Wake::Refresh), "the cadence");
    }

    /// **`biased;`.** When a close and a due tick are both ready in the same
    /// poll, the close wins: the gate parks instead of firing one more
    /// refresh.
    ///
    /// Repeated twelve times deliberately. Without `biased;` `select!` picks
    /// at random, so a single round would pass about half the time; twelve
    /// leaves a 1-in-4096 chance of a deleted `biased;` surviving this test.
    ///
    /// Falsification: delete the `biased;` line — measured, this reds.
    #[tokio::test(start_paused = true)]
    async fn a_close_beats_a_due_tick() {
        for round in 0..12 {
            let (tx, rx) = cmd_channel();
            let mut g = gate(rx);

            tx.send(Cmd::SetVisible(true)).expect("open lane");
            assert_eq!(wake_soon(&mut g).await, Some(Wake::Refresh), "the edge");

            // Make the tick due, then queue the close: both are ready at the
            // next poll of the `select!`.
            tokio::time::advance(P).await;
            tx.send(Cmd::SetVisible(false)).expect("open lane");

            assert!(
                !wakes_within(&mut g, P * 10).await,
                "round {round}: a close must beat a tick that came due at the same moment"
            );
        }
    }

    /// **No cancellation of work in flight.** The gate's `select!` is not
    /// running while the caller awaits its refresh, so a close that arrives
    /// mid-refresh is read afterwards: it parks the *next* poll and leaves the
    /// one already in flight alone.
    ///
    /// The refresh here deliberately outlasts a whole period, so a tick also
    /// comes due while the surface is already hidden — the gate must still
    /// park, which is the case a level-triggered gate would get wrong.
    #[tokio::test(start_paused = true)]
    async fn a_close_mid_refresh_parks_the_next_poll_not_this_one() {
        let (tx, rx) = cmd_channel();
        let mut g = gate(rx);

        tx.send(Cmd::SetVisible(true)).expect("open lane");
        assert_eq!(wake_soon(&mut g).await, Some(Wake::Refresh), "the edge");

        // The caller's fetch is in flight; the surface hides under it, and the
        // cadence comes due before the fetch returns.
        tx.send(Cmd::SetVisible(false)).expect("open lane");
        tokio::time::sleep(P * 2).await;
        assert!(
            g.is_visible(),
            "the lane is read only between refreshes — a close cannot cancel one in flight"
        );

        assert!(
            !wakes_within(&mut g, P * 10).await,
            "a close observed mid-refresh must park the next poll"
        );
    }

    /// A dropped [`crate::CmdSender`] is the session tearing down: the loop
    /// ends rather than polling on against a gone reducer.
    #[tokio::test(start_paused = true)]
    async fn a_closed_lane_ends_the_loop() {
        let (tx, rx) = cmd_channel();
        let mut g = gate(rx);
        tx.send(Cmd::SetVisible(true)).expect("open lane");
        assert_eq!(wake_soon(&mut g).await, Some(Wake::Refresh), "the edge");
        drop(tx);
        assert_eq!(wake_soon(&mut g).await, None, "a closed lane ends the loop");
    }

    /// A command the classifier does not claim comes straight back, with the
    /// gate's own state untouched.
    #[tokio::test(start_paused = true)]
    async fn an_unclassified_command_is_handed_back() {
        let (tx, rx) = cmd_channel();
        let mut g = gate(rx);
        tx.send(Cmd::Other(7)).expect("open lane");
        assert_eq!(wake_soon(&mut g).await, Some(Wake::Cmd(Cmd::Other(7))));
        assert!(
            !g.is_visible(),
            "a passthrough command does not open the gate"
        );
    }

    /// **`set_period`.** The new cadence takes effect and restarts from now —
    /// no immediate leading tick, unlike a bare `tokio::time::interval`.
    #[tokio::test(start_paused = true)]
    async fn set_period_re_arms_from_now() {
        let (tx, rx) = cmd_channel();
        let mut g = gate(rx);

        tx.send(Cmd::SetVisible(true)).expect("open lane");
        assert_eq!(wake_soon(&mut g).await, Some(Wake::Refresh), "the edge");

        g.set_period(P * 4);
        assert!(
            !wakes_within(&mut g, P * 4 * 9 / 10).await,
            "the re-armed cadence must not fire early, and must not lead"
        );
        assert_eq!(
            wake_soon(&mut g).await,
            Some(Wake::Refresh),
            "the re-armed cadence"
        );
    }
}
