//! "Does the active workspace on **this** screen hold more than one window?" —
//! tracked per output, in this process, off a second niri connection.
//!
//! Annika's third ask on #1019 (2026-09-10) is "Only show when more than 1
//! window in workspace". The host has **no niri state topic** to subscribe to:
//! [`StateKey`](hytte_plugin::proto::StateKey) covers `Clock`, `SlotVisible`,
//! `Accent`, `AudioSpectrum` and friends, and nothing about windows or
//! workspaces. So the plugin answers the question itself, the same way it
//! already answers "which columns are on this screen's workspace" — by talking to
//! `$NIRI_SOCKET`.
//!
//! # Why a second connection
//!
//! [`niri::SocketTransport`](crate::niri::SocketTransport) opens a fresh
//! short-lived socket **per request**, because niri only guarantees one reply
//! per connection for a non-`EventStream` request. `Request::EventStream` is
//! the exception: it answers `Handled` and then streams forever, so it needs a
//! connection of its own that nothing else writes to. That is exactly the split
//! the shell itself runs (`hytte-services`' `niri.rs`: a long-lived listener
//! plus a fresh socket per command).
//!
//! # One answer per screen (#1050)
//!
//! The verdict used to be a single `bool` off the **focused** workspace, and
//! the host mirrors one tree onto every monitor, so screen B's chip showed
//! screen A's answer — which is what Annika hit on glass (#1050). niri already
//! carries the fix: every [`Workspace`](niri_ipc::Workspace) reports its
//! `output` and whether it `is_active` **on that output**, so the fold keeps
//! *every* output's active workspace and counts windows on each. The result is
//! a [`Verdict`]: the connectors whose active workspace is below
//! [`MIN_WINDOWS`], plus whether any output is above it at all.
//!
//! [`crate::plugin`] turns that into `View::hidden_on`, which is the only
//! per-screen lever the wire has (#1068) — same tree everywhere, different
//! visibility.
//!
//! Two consequences worth stating:
//!
//! - **A workspace with `output: None` is counted nowhere.** niri-ipc 26.4.0
//!   is narrow about when that happens: the field "can be `None` if **no**
//!   outputs are currently connected" — the whole-desktop case (every screen
//!   asleep or unplugged), not one monitor of several going away. Unplugging
//!   one of two re-homes its workspaces to the survivor and never reaches this
//!   branch; the connector simply changes. Either way there is no screen to
//!   show a chip on, so such a workspace's windows raise no output's count, and
//!   they start counting again on the `WorkspacesChanged` that names an output.
//! - **`WorkspaceActivated`'s `focused` flag no longer matters.** niri's own
//!   docs: the event means the workspace is now active *on its output*, and all
//!   others on that output are not; `focused` says whether it also took
//!   keyboard focus. The old fold looked at nothing else, so a workspace switch
//!   on the second monitor moved the one global answer. Per-output, the flag is
//!   irrelevant — [`Watch::activate`] deactivates the siblings on the same
//!   output and leaves every other output alone.
//!
//! # The shape
//!
//! - [`Watch`] is **pure**: fold an [`Event`] in, get `Some(verdict)` back the
//!   moment the show/hide answer changes and `None` every other time. Every
//!   rule below is unit-tested against it with no socket in sight.
//! - [`backoff`] is pure too.
//! - [`drive`] is the plumbing loop: connect, hand each event to [`Watch`],
//!   emit what it returns, reconnect with [`backoff`] when the stream dies —
//!   and **stop** when the session that owns the watcher has ended.
//! - [`Backend`] is the seam under it, exactly like
//!   [`Transport`](crate::niri::Transport) is the seam under
//!   [`apply`](crate::niri::apply): the socket, the sleep and the verdict lane
//!   all live behind it, so the loop's own decisions (the reconnect priming,
//!   the backoff policy, the shutdown) are unit-tested against a scripted
//!   connection rather than against a compositor. [`run`] is the one-line
//!   production wiring: [`drive`] over [`SocketBackend`].
//!
//! # Lifetime — one watcher per **session**, not per process
//!
//! [`crate::plugin::NiriLayouts::sources`] spawns this, and the SDK calls
//! `sources` inside `session()` (`hytte-plugin`'s `runtime.rs`), which
//! `reconnect_loop` re-enters after **every** host `Shutdown` or transport
//! error. A plugin unit is `PartOf=graphical-session.target`, so it outlives
//! `systemctl --user restart trollshell` — the routine dev loop — and a watcher
//! that ran forever would leave one detached thread, one live `$NIRI_SOCKET`
//! event stream and one full decode of every niri event behind per shell
//! restart, for the life of the plugin process (#1038 review, HIGH-2).
//!
//! There is no session-scoped cancellation token in the SDK to hang this off —
//! `sources` is handed a `CmdReceiver` and nothing else — so **the receiver
//! drop is the contract**: the runtime owns the message channel for exactly one
//! session (`runtime.rs`: *"the runtime owns the channel, so its lifecycle is
//! exactly this session"*), so the moment the session's stream drops, this
//! thread's sends stop being deliverable. [`Verdicts::send`] and
//! [`Verdicts::open`] report that, [`drive`] returns on it, and
//! [`live_watchers`] lets a test measure that the thread really did end.
//!
//! Noticing requires the blocking read to be interruptible, which is why this
//! module dials the socket itself instead of using
//! [`niri_ipc::socket::Socket`]: `Socket::read_events` parks forever with no
//! timeout, so a watcher whose session ended between two niri events would sit
//! on a dead channel until the compositor happened to say something. The read
//! here carries a [`POLL_INTERVAL`] timeout, so the exit lands within one tick
//! of the session ending — measured, not assumed
//! (`plugin::sources_spawns_one_watcher_thread_and_it_exits_when_its_session_does`).
//!
//! # Windows, not columns
//!
//! [`plan`](crate::layout::plan) counts **columns** (a stacked column has one
//! width — #1019 question 3). The visibility rule counts **windows**, which is
//! Annika's literal wording: two windows stacked into a single column is two
//! windows, and the chip shows. The two counts are deliberately different
//! things and neither is derived from the other.
//!
//! Floating and fullscreen windows count as well. A workspace holding one tiled
//! window and one floating terminal is a workspace with two windows on it — and
//! nothing about the rule promises that every counted window is one a click
//! will resize.

use niri_ipc::socket::SOCKET_PATH_ENV;
use niri_ipc::{Event, Reply, Request, Response};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How many windows an output's **active** workspace needs before the chip
/// appears on that screen.
///
/// "more than 1" (#1019) — so two. Applied per output since #1050; see the
/// module docs.
pub(crate) const MIN_WINDOWS: usize = 2;

/// The first reconnect delay, and the base the doubling starts from.
const BACKOFF_BASE: Duration = Duration::from_secs(1);

/// The reconnect delay ceiling. A niri that never comes back costs one connect
/// attempt every half minute rather than a spin.
const BACKOFF_CEILING: Duration = Duration::from_secs(30);

/// The doubling stops here; `1 << 5` is 32 s, already past [`BACKOFF_CEILING`].
const BACKOFF_MAX_SHIFT: u32 = 5;

/// How long a blocking read parks before handing control back to [`drive`].
///
/// niri is usually silent for minutes at a time, so this — not an event — is
/// what wakes the loop often enough to notice that its session ended (see the
/// module docs). It is a `recv` timeout on an otherwise idle socket: two
/// returning-`EAGAIN` syscalls a second, and it bounds the shutdown latency at
/// one tick.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait for niri's reply to the `EventStream` request — as a
/// **total** deadline on the reply, not merely as the `SO_RCVTIMEO` armed on
/// each individual read.
///
/// The handshake is one round trip on a socket that just accepted us, so a
/// timeout here means niri is wedged, not busy: give up and let [`backoff`]
/// schedule the retry rather than parking a thread on it forever. A per-read
/// timeout alone cannot promise that: a peer dribbling in one byte just under
/// each read's own window never lets any single read time out, so
/// [`SocketEvents::over`] tracks an `Instant` deadline across the whole reply
/// read (#1053 review LOW-3) rather than relying on `SO_RCVTIMEO` to bound the
/// handshake by itself.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// The line length past which [`SocketEvents`] gives up on ever seeing a
/// trailing newline and drops the connection instead of buffering forever.
///
/// Comfortably above anything a real niri event needs: even a
/// `WindowsChanged` listing every window on every workspace, with a few
/// hundred long titles, is tens of KiB. 1 MiB leaves roughly two orders of
/// magnitude of margin while still bounding how much an unterminated
/// stream — malicious or merely broken — can make this thread buffer before
/// [`SocketEvents::next_event`] returns and [`stream_once`]'s `alive()` check
/// gets another turn (#1053 review LOW-2: without a cap, a stream that never
/// sends a `\n` is HIGH-2's leak shape again, just triggered by a flood
/// instead of silence).
const MAX_LINE: usize = 1024 * 1024;

/// How many [`run`] watcher threads are alive in this process.
///
/// Exists to be **asserted on**: the leak HIGH-2 found (#1038) is invisible from
/// inside a single session, so the test that pins the lifetime contract watches
/// this go 0 → 1 when `sources` spawns and back to 0 within a poll tick of the
/// session's receiver dropping. Nothing in the shipping path reads it.
static LIVE_WATCHERS: AtomicUsize = AtomicUsize::new(0);

/// How many watcher threads are running right now — see [`LIVE_WATCHERS`].
///
/// Test-only by construction: the count exists to be asserted on, and gating it
/// keeps the shipping binary from carrying a reader nothing calls.
#[cfg(test)]
pub(crate) fn live_watchers() -> usize {
    LIVE_WATCHERS.load(Ordering::SeqCst)
}

/// Keeps [`LIVE_WATCHERS`] honest across an early return **or** a panic.
struct Live;

impl Live {
    fn enter() -> Self {
        LIVE_WATCHERS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        LIVE_WATCHERS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The show/hide answer for **every screen at once** (#1050).
///
/// Two fields rather than one map because the two questions the plugin asks are
/// different: `hidden_on` is the wire value
/// ([`View::hidden_on`](hytte_plugin::View::hidden_on)) and `shows_anywhere`
/// decides which *tree* is rendered at all. They cannot be derived from each
/// other — an empty `hidden_on` means "nothing to hide", which is true both
/// when every screen shows the chip and when niri has told us about no outputs
/// yet.
///
/// [`Default`] is the pre-niri state: nothing hidden, nothing shown — the chip
/// is absent everywhere, which is what [`Watch`]'s own doc promises and what
/// [`crate::plugin::NiriLayouts`]'s initial model draws.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Verdict {
    /// The connector names whose active workspace holds fewer than
    /// [`MIN_WINDOWS`] windows — **sorted**, because this value is also the
    /// dedup key: [`Watch::observe`] only emits when the verdict *changes*, and
    /// two runs of the same fold that differed only in `HashMap` iteration
    /// order would emit a spurious change on every niri event.
    pub(crate) hidden_on: Vec<String>,
    /// Whether at least one output's active workspace is at or above
    /// [`MIN_WINDOWS`] — i.e. whether the chip is worth rendering at all.
    pub(crate) shows_anywhere: bool,
}

/// What niri says about one workspace, reduced to the two facts the verdict
/// needs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Slot {
    /// The connector this workspace lives on. `None` is what niri reports when
    /// **no outputs are connected at all** (niri-ipc 26.4.0: "Can be `None` if
    /// no outputs are currently connected") — not when one monitor of several
    /// goes away, which re-homes its workspaces to a survivor instead. Such a
    /// workspace is counted nowhere (module docs).
    output: Option<String>,
    /// Whether it is the workspace currently visible on that output. Exactly
    /// one per output, per niri's contract.
    is_active: bool,
}

/// The compositor state the show/hide verdict is a function of, plus the last
/// verdict emitted.
///
/// Starts **empty**, which reads as hidden everywhere: a plugin that has not
/// heard from niri yet must not flash a chip it may be about to take away. That
/// matches [`crate::plugin::NiriLayouts`]'s own initial model, so the first
/// frame the host ever renders and this struct agree without either being told.
#[derive(Clone, Debug, Default)]
pub(crate) struct Watch {
    /// Window id → the workspace niri says it is on (`None` when niri reports
    /// none). Every window, tiled or floating — see the module docs.
    windows: HashMap<u64, Option<u64>>,
    /// Workspace id → its output and whether it is active there (#1050). This
    /// replaced a single `focused: Option<u64>`: the verdict is per screen now,
    /// so which workspace holds *keyboard focus* is not a fact this module
    /// needs at all.
    workspaces: HashMap<u64, Slot>,
    /// Whether the opening `WorkspacesChanged` of this connection has landed.
    seen_workspaces: bool,
    /// Whether the opening `WindowsChanged` of this connection has landed.
    seen_windows: bool,
    /// The last verdict [`Watch::observe`] handed out. Empty because the chip
    /// starts hidden everywhere.
    emitted: Verdict,
}

impl Watch {
    /// Fold `event` in and return the new verdict **only if it changed**.
    ///
    /// Returning `Option` rather than a bare `bool` is what keeps the render
    /// lane quiet: niri emits an event per window motion, and a plugin that
    /// pushed a message per event would wake the session loop hundreds of times
    /// a drag to re-render an identical tree. (The SDK would dedup the frame,
    /// but not the wakeup.)
    ///
    /// **Nothing is emitted until the connection's opening snapshot is
    /// complete** — both a `WorkspacesChanged` and a `WindowsChanged`. niri
    /// opens an `EventStream` with a burst of them, in an order this makes no
    /// assumption about, and a half-arrived snapshot is not a state the chip
    /// should be shown a picture of: with the workspaces in and the windows
    /// still to come, the answer is momentarily "zero windows". Judging that
    /// moment is exactly what made a niri restart blink the chip off and back
    /// on, which is what [`Watch::forget_compositor_state`] promises it will
    /// not do.
    pub(crate) fn observe(&mut self, event: Event) -> Option<Verdict> {
        self.apply(event);
        if !self.snapshot_complete() {
            return None;
        }
        let verdict = self.verdict();
        (verdict != self.emitted).then(|| {
            self.emitted.clone_from(&verdict);
            verdict
        })
    }

    /// Whether this connection has delivered both halves of its opening
    /// snapshot; see [`Watch::observe`].
    fn snapshot_complete(&self) -> bool {
        self.seen_workspaces && self.seen_windows
    }

    /// The verdict as it stands, whether or not it just changed.
    ///
    /// Reads straight off [`Watch::window_counts`], so the two can never
    /// disagree about which screens are below the threshold.
    pub(crate) fn verdict(&self) -> Verdict {
        let mut hidden_on = Vec::new();
        let mut shows_anywhere = false;
        for (output, count) in self.window_counts() {
            if count >= MIN_WINDOWS {
                shows_anywhere = true;
            } else {
                hidden_on.push(output.to_owned());
            }
        }
        // **Normalised**: `hidden_on` only means anything while something is
        // shown. With no output above the threshold the plugin renders the
        // collapsed tree, which is invisible on every screen by itself
        // (`plugin::view`), so naming screens there would put bytes on the wire
        // that decide nothing — and it would make "hidden everywhere" a
        // *different value* on a one- and a two-monitor desktop, so merely
        // connecting to niri would look like a verdict change and wake the
        // session loop for a frame the SDK then dedups away.
        if !shows_anywhere {
            hidden_on.clear();
        }
        Verdict {
            hidden_on,
            shows_anywhere,
        }
    }

    /// Windows on each output's **active** workspace, keyed by connector name.
    ///
    /// A [`BTreeMap`] rather than a `HashMap` on purpose: the key order *is*
    /// [`Verdict::hidden_on`]'s order, and that vec is compared against the
    /// previous one to decide whether anything changed — a nondeterministic
    /// order would emit a "change" on every niri event and re-render the chip
    /// hundreds of times a drag.
    ///
    /// Every output niri named gets an entry, **including one whose active
    /// workspace is empty**: a screen missing from the map would be a screen
    /// missing from `hidden_on`, i.e. a chip that stays visible on an empty
    /// desktop. Outputs are learnt from the workspace list, which is the only
    /// place this module sees a connector name at all; an output with no
    /// workspaces on it does not exist as far as niri is concerned.
    ///
    /// Windows on a workspace niri reports with `output: None` (no outputs
    /// connected at all — see [`Slot::output`]) raise no count: there is no
    /// screen for them to show a chip on. Neither do windows on a workspace
    /// that is not the active one on its output — that is the whole point of
    /// the rule.
    fn window_counts(&self) -> BTreeMap<&str, usize> {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for slot in self.workspaces.values() {
            if let Some(output) = slot.output.as_deref() {
                counts.entry(output).or_default();
            }
        }
        for workspace in self.windows.values() {
            let Some(slot) = workspace.and_then(|id| self.workspaces.get(&id)) else {
                continue;
            };
            let Some(output) = slot.output.as_deref() else {
                continue;
            };
            if slot.is_active {
                *counts.entry(output).or_default() += 1;
            }
        }
        counts
    }

    /// Drop everything niri told us, keeping the last emitted verdict.
    ///
    /// Called on a reconnect: niri restarted, so its window and workspace ids
    /// are meaningless now and the fresh `EventStream` opens with a full
    /// snapshot anyway. **The verdict is deliberately kept** — a niri restart
    /// that lands on the same two-window workspace should not blink the chip
    /// off and on, and if it lands somewhere different the snapshot emits the
    /// change a moment later.
    ///
    /// Keeping `emitted` is only half of that promise; clearing the two
    /// snapshot flags is the other half, and is what suppresses the "zero
    /// windows" moment between the new stream's first event and its second.
    fn forget_compositor_state(&mut self) {
        self.windows.clear();
        self.workspaces.clear();
        self.seen_workspaces = false;
        self.seen_windows = false;
    }

    /// The fold itself. Five of niri 26.4's twenty-odd events move this state;
    /// the rest (keyboard layouts, casts, screenshots, urgency, focus, overview)
    /// cannot change how many windows sit on any output's active workspace.
    fn apply(&mut self, event: Event) {
        match event {
            // The full workspace list — this is where the outputs, and which
            // workspace is active on each, come from on connect and after any
            // workspace add/remove/move. "This configuration completely
            // replaces the previous configuration", so replace.
            Event::WorkspacesChanged { workspaces } => {
                self.workspaces = workspaces
                    .into_iter()
                    .map(|w| {
                        (
                            w.id,
                            Slot {
                                output: w.output,
                                is_active: w.is_active,
                            },
                        )
                    })
                    .collect();
                self.seen_workspaces = true;
            }
            // A workspace became *active on its output* — which is exactly the
            // question this module asks now, so the `focused` flag is ignored:
            // before #1050 it was the only thing read here, and that is why
            // switching workspaces on the second monitor moved the one global
            // answer instead of that monitor's own.
            Event::WorkspaceActivated { id, .. } => self.activate(id),
            // "This configuration completely replaces the previous
            // configuration" (niri's own wording), so replace rather than merge:
            // a window missing from the list was closed.
            Event::WindowsChanged { windows } => {
                self.windows = windows
                    .into_iter()
                    .map(|w| (w.id, w.workspace_id))
                    .collect();
                self.seen_windows = true;
            }
            // Opened, or moved to another workspace, or retitled — niri sends
            // the whole window either way, so an upsert covers all three.
            Event::WindowOpenedOrChanged { window } => {
                self.windows.insert(window.id, window.workspace_id);
            }
            Event::WindowClosed { id } => {
                self.windows.remove(&id);
            }
            _ => {}
        }
    }

    /// Make workspace `id` the active one **on its own output**, and no other
    /// output's.
    ///
    /// niri's contract for `WorkspaceActivated`: "all other workspaces on the
    /// same output become inactive". Deactivating every workspace instead — the
    /// obvious one-liner — would blank the other monitor's count until its own
    /// next event, i.e. reintroduce #1050 through the back door.
    ///
    /// An `id` this fold has never seen is **ignored**, not inserted: its
    /// output is unknown, so there is neither a set of siblings to deactivate
    /// nor a screen it could contribute a count to. `WorkspacesChanged` is what
    /// introduces a workspace, and niri sends one whenever the set changes.
    fn activate(&mut self, id: u64) {
        let Some(output) = self.workspaces.get(&id).map(|slot| slot.output.clone()) else {
            return;
        };
        for (&other, slot) in &mut self.workspaces {
            if slot.output == output {
                slot.is_active = other == id;
            }
        }
    }
}

/// How long to wait before reconnect attempt number `attempt` (0-based).
///
/// 1 s doubling to a 30 s ceiling. Pure, so the schedule is a unit test rather
/// than a thing you find out about from a journal at 3 a.m.
pub(crate) fn backoff(attempt: u32) -> Duration {
    let doubled = BACKOFF_BASE * (1_u32 << attempt.min(BACKOFF_MAX_SHIFT));
    doubled.min(BACKOFF_CEILING)
}

/// Why a connection stopped — which decides what [`drive`] does next.
#[cfg_attr(test, derive(Debug))]
enum Stopped {
    /// Never got as far as a live stream: no `$NIRI_SOCKET`, nothing listening,
    /// or niri refused `EventStream`. Back off further each time.
    Unreachable(String),
    /// We were streaming and the socket went away (niri restarted or exited).
    /// Reset the backoff: the next connect deserves a fresh
    /// [`BACKOFF_BASE`].
    StreamEnded(String),
    /// The session that owns this watcher has ended — its receiver is gone.
    /// **Stop**; see the module docs on why running on would leak a thread per
    /// shell restart.
    SessionOver,
}

/// One poll of a live event stream.
///
/// `Idle` is the timeout tick: nothing to fold, but [`drive`] gets its turn, so
/// a session that ended while niri was quiet is noticed within
/// [`POLL_INTERVAL`].
#[derive(Debug)]
pub(crate) enum Incoming {
    Event(Box<Event>),
    Idle,
    Ended(String),
}

/// One connection's worth of events. The seam the reconnect tests script.
pub(crate) trait EventSource {
    fn next_event(&mut self) -> Incoming;
}

/// Where verdicts go — this thread's end of the session's message lane.
///
/// Both methods answer the same question from different sides, and it is the
/// only shutdown signal available (module docs): `false` means the receiver has
/// been dropped, i.e. the session this watcher was spawned for is over.
pub(crate) trait Verdicts {
    /// Deliver a verdict. `false` = the receiver is gone.
    fn send(&mut self, verdict: Verdict) -> bool;
    /// Is the receiver still there? Asked between events, so an idle niri does
    /// not delay the shutdown.
    fn open(&self) -> bool;
}

/// Everything [`drive`] touches that is not pure: the socket, the verdict lane,
/// the sleep and the log.
///
/// One trait rather than four arguments because the fake in the tests below is
/// then one struct that records what the loop *did* — which connections it
/// asked for, what it emitted, how long it waited, what it logged — and every
/// one of `drive`'s own decisions is an assertion on that record.
trait Backend {
    type Source: EventSource;

    /// Dial niri and complete the `EventStream` handshake, or say why not.
    fn connect(&mut self) -> Result<Self::Source, String>;
    /// Hand a verdict to the session; `false` = the session is over.
    fn emit(&mut self, verdict: Verdict) -> bool;
    /// Whether the session is still there.
    fn alive(&self) -> bool;
    /// Wait out a backoff delay; `false` = the session ended while waiting.
    fn wait(&mut self, delay: Duration) -> bool;
    /// One line for the operator.
    fn log(&mut self, line: &str);
}

/// Watch niri for this session, calling back on each show/hide flip.
///
/// Returns when the session ends — see the module docs' lifetime section. Runs
/// on its own OS thread rather than the SDK's current-thread runtime: the read
/// is blocking std I/O, and parking that on a `spawn_blocking` slot for the life
/// of a session is what that pool is explicitly not for.
///
/// Nothing here panics on a missing niri: a plugin session started outside a
/// niri session simply never sees a verdict change and leaves the chip hidden,
/// with one WARNING naming that consequence.
pub(crate) fn run(prefix: &str, verdicts: impl Verdicts) {
    let _live = Live::enter();
    drive(prefix, &mut SocketBackend { verdicts });
}

/// The reconnect loop itself, over the [`Backend`] seam.
///
/// Three decisions live here and nowhere else, which is why they are worth a
/// seam: **the priming** (`forget_compositor_state` at the loop head — without
/// it a niri restart blinks the chip off and back on between the new stream's
/// first two events), **the backoff policy** (a stream that lived resets it, a
/// niri that was never there grows it) and **the shutdown**.
fn drive<B: Backend>(prefix: &str, backend: &mut B) {
    let mut watch = Watch::default();
    let mut attempt = 0_u32;
    // Whether the operator has already been told that niri is unreachable. One
    // WARNING per outage, not one per attempt: without it a broken
    // `$NIRI_SOCKET` is indistinguishable on glass from "one window on this
    // workspace" (#1038 review, LOW-8).
    let mut warned = false;
    loop {
        // Primed for a *fresh* connection: niri's ids mean nothing across a
        // restart, and the snapshot flags must be cleared or the new stream's
        // first event is judged against the old stream's window map.
        watch.forget_compositor_state();
        let delay = match stream_once(&mut watch, backend) {
            Stopped::SessionOver => return,
            Stopped::Unreachable(why) => {
                if warned {
                    backend.log(&format!(
                        "[{prefix}] niri event stream still unavailable ({why})"
                    ));
                } else {
                    warned = true;
                    backend.log(&format!(
                        "[{prefix}] WARNING: cannot watch niri for each output's active \
                         workspace window count ({why}) — the layout chip stays hidden on \
                         every screen until this succeeds"
                    ));
                }
                // `backoff(attempt)` *then* the increment, so the first retry is
                // BACKOFF_BASE and the documented schedule (1 → 2 → 4 → 8 → 16 →
                // 30 s) is the one that actually runs (#1038 review, LOW-7).
                let delay = backoff(attempt);
                attempt = attempt.saturating_add(1);
                delay
            }
            Stopped::StreamEnded(why) => {
                backend.log(&format!(
                    "[{prefix}] niri event stream ended ({why}), reconnecting"
                ));
                // We reached a live stream, so this is niri restarting rather
                // than niri being absent: the next connect deserves a fresh
                // first delay, and a later outage deserves its own WARNING.
                attempt = 0;
                warned = false;
                backoff(attempt)
            }
        };
        if !backend.wait(delay) {
            return;
        }
    }
}

/// One connection's worth: dial, then fold events until the socket gives out or
/// the session does.
fn stream_once<B: Backend>(watch: &mut Watch, backend: &mut B) -> Stopped {
    let mut source = match backend.connect() {
        Ok(source) => source,
        Err(why) => return Stopped::Unreachable(why),
    };
    loop {
        // Checked at the head rather than only on the idle tick: a busy niri
        // emits events without ever flipping the verdict, so `emit`'s return
        // alone would not be reached for an arbitrarily long time.
        if !backend.alive() {
            return Stopped::SessionOver;
        }
        match source.next_event() {
            Incoming::Event(event) => {
                if let Some(verdict) = watch.observe(*event)
                    && !backend.emit(verdict)
                {
                    return Stopped::SessionOver;
                }
            }
            // The read timed out. Nothing to fold — the point of the tick is the
            // liveness check above.
            Incoming::Idle => {}
            Incoming::Ended(why) => return Stopped::StreamEnded(why),
        }
    }
}

/// The production [`Backend`]: a real `$NIRI_SOCKET` connection, a real sleep,
/// and the session's own message lane.
struct SocketBackend<V> {
    verdicts: V,
}

impl<V: Verdicts> Backend for SocketBackend<V> {
    type Source = SocketEvents;

    fn connect(&mut self) -> Result<Self::Source, String> {
        SocketEvents::connect()
    }

    fn emit(&mut self, verdict: Verdict) -> bool {
        self.verdicts.send(verdict)
    }

    fn alive(&self) -> bool {
        self.verdicts.open()
    }

    /// Sleeps in [`POLL_INTERVAL`] slices rather than one long park: a session
    /// that ends 200 ms into a 30 s backoff must not keep the thread for the
    /// remaining 29.8 s.
    fn wait(&mut self, delay: Duration) -> bool {
        let mut left = delay;
        while !left.is_zero() {
            if !self.verdicts.open() {
                return false;
            }
            let slice = left.min(POLL_INTERVAL);
            std::thread::sleep(slice);
            left -= slice;
        }
        self.verdicts.open()
    }

    fn log(&mut self, line: &str) {
        // stderr, which systemd routes to the journal for a plugin unit.
        eprintln!("{line}");
    }
}

/// A live niri event stream: the reply line already consumed, the read timeout
/// already armed.
///
/// # Why this dials the socket by hand
///
/// [`niri_ipc::socket::Socket`] is used for every *request* this crate makes
/// ([`crate::niri::SocketTransport`]), but it cannot serve here: `read_events`
/// consumes the `Socket` and hands back a closure over a private `BufReader`,
/// so there is no way to arm a read timeout — and without one the thread parks
/// on a dead channel until niri happens to speak (the HIGH-2 leak). The framing
/// mirrors `Socket`'s own, line by line, over niri's own types: one JSON value
/// per line, the request first, then the reply, then events forever.
pub(crate) struct SocketEvents {
    reader: BufReader<UnixStream>,
    /// The line being assembled. **Not cleared between polls**: a byte read
    /// that times out mid-line leaves whatever bytes had already arrived
    /// appended here, and dropping them would corrupt the next event rather
    /// than lose an idle tick. Bounded by [`MAX_LINE`]: a stream that never
    /// sends a trailing newline ends the connection instead of growing this
    /// without bound (#1053 review LOW-2).
    line: Vec<u8>,
}

impl SocketEvents {
    /// Dial `$NIRI_SOCKET` and hand back a live stream.
    fn connect() -> Result<Self, String> {
        let path = std::env::var_os(SOCKET_PATH_ENV).ok_or_else(|| {
            format!("${SOCKET_PATH_ENV} is not set, are you running this within niri?")
        })?;
        let stream = UnixStream::connect(&path)
            .map_err(|e| format!("cannot reach ${SOCKET_PATH_ENV}: {e}"))?;
        Self::over(stream)
    }

    /// The handshake and the two timeouts, over an already-connected socket —
    /// the seam the framing test drives against a scripted niri.
    fn over(stream: UnixStream) -> Result<Self, String> {
        stream
            .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .map_err(|e| format!("cannot arm the handshake timeout: {e}"))?;

        let mut request = serde_json::to_string(&Request::EventStream)
            .map_err(|e| format!("cannot encode the EventStream request: {e}"))?;
        request.push('\n');
        let mut writer = &stream;
        writer
            .write_all(request.as_bytes())
            .map_err(|e| format!("cannot send the EventStream request: {e}"))?;

        // A deadline on the *whole* reply, not just the `SO_RCVTIMEO` armed
        // above on each individual read — see `read_line_before_deadline`'s
        // docs (#1053 review LOW-3).
        let deadline = std::time::Instant::now() + HANDSHAKE_TIMEOUT;
        let mut reader = BufReader::new(stream);
        let line = read_line_before_deadline(&mut reader, deadline)?;
        let reply: Reply = serde_json::from_slice(&line).map_err(|e| {
            format!(
                "cannot decode niri's reply {:?}: {e}",
                String::from_utf8_lossy(&line)
            )
        })?;
        match reply {
            Ok(Response::Handled) => {}
            Ok(other) => return Err(format!("unexpected EventStream reply: {other:?}")),
            Err(msg) => return Err(format!("niri refused EventStream: {msg}")),
        }

        // Nothing is ever written on this connection again — same half-close
        // `Socket::read_events` does, so niri can reap our write half.
        let _ = reader.get_ref().shutdown(Shutdown::Write);
        reader
            .get_ref()
            .set_read_timeout(Some(POLL_INTERVAL))
            .map_err(|e| format!("cannot arm the read timeout: {e}"))?;
        Ok(Self {
            reader,
            line: Vec::new(),
        })
    }
}

/// Reads one `\n`-terminated line, but gives up once `deadline` has passed —
/// checked before every underlying read, rather than relying on `SO_RCVTIMEO`
/// to bound any *individual* one (#1053 review LOW-3).
///
/// A peer dribbling in a byte just under each read's own timeout window never
/// lets any single read time out, so a per-read timeout alone cannot bound the
/// *whole* handshake the way [`HANDSHAKE_TIMEOUT`]'s own doc promises. This
/// checks wall-clock progress against `deadline` between reads instead, so the
/// total time is bounded even when every individual read keeps succeeding.
fn read_line_before_deadline(
    reader: &mut BufReader<UnixStream>,
    deadline: std::time::Instant,
) -> Result<Vec<u8>, String> {
    let mut line = Vec::new();
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "no complete reply to the EventStream request within {HANDSHAKE_TIMEOUT:?} \
                 total, even though individual reads kept succeeding"
            ));
        }
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(e) if timed_out(&e) || e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("no reply to the EventStream request: {e}")),
        };
        if available.is_empty() {
            return Err("niri closed the connection before replying to EventStream".to_owned());
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&available[..=pos]);
            let consumed = pos + 1;
            reader.consume(consumed);
            return Ok(line);
        }
        let n = available.len();
        line.extend_from_slice(available);
        reader.consume(n);
    }
}

/// Whether an error is the read timeout firing rather than the stream failing.
/// The kind is platform-dependent (`std`'s own wording), so both are accepted.
fn timed_out(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

impl EventSource for SocketEvents {
    /// Reads bytes a chunk at a time via `fill_buf`/`consume` rather than a
    /// single `read_until` call, so [`MAX_LINE`] is checked **between**
    /// chunks instead of only after the whole read unwinds (#1053 review
    /// LOW-2): a peer that streams continuously with no gap ever long enough
    /// to trip `SO_RCVTIMEO` would otherwise keep a single `read_until` call
    /// from ever returning at all, and no cap checked only on its way out
    /// could catch that — the call never comes back to be checked.
    fn next_event(&mut self) -> Incoming {
        loop {
            let available = match self.reader.fill_buf() {
                Ok(available) => available,
                Err(e) if timed_out(&e) || e.kind() == io::ErrorKind::Interrupted => {
                    return Incoming::Idle;
                }
                Err(e) => return Incoming::Ended(e.to_string()),
            };
            if available.is_empty() {
                // End of stream. A non-empty `self.line` here is a truncated
                // final event (`fill_buf` only returns empty at true EOF).
                return Incoming::Ended(if self.line.is_empty() {
                    "niri closed the event stream".to_owned()
                } else {
                    "niri closed the event stream mid-event".to_owned()
                });
            }
            if let Some(pos) = available.iter().position(|&b| b == b'\n') {
                self.line.extend_from_slice(&available[..=pos]);
                let consumed = pos + 1;
                self.reader.consume(consumed);
                let line = std::mem::take(&mut self.line);
                return match serde_json::from_slice::<Event>(&line) {
                    Ok(event) => Incoming::Event(Box::new(event)),
                    Err(e) => Incoming::Ended(format!("cannot decode a niri event: {e}")),
                };
            }
            let n = available.len();
            self.line.extend_from_slice(available);
            self.reader.consume(n);
            if self.line.len() > MAX_LINE {
                return Incoming::Ended(format!(
                    "niri sent a {}-byte line with no newline — treating the stream \
                     as dead rather than buffering forever",
                    self.line.len()
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BACKOFF_CEILING, Backend, EventSource, HANDSHAKE_TIMEOUT, Incoming, MIN_WINDOWS,
        POLL_INTERVAL, SocketBackend, SocketEvents, Stopped, Verdict, Verdicts, Watch, backoff,
        drive, stream_once,
    };
    use niri_ipc::{Event, Window, WindowLayout, Workspace};
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// The single screen every pre-#1050 test in this file implicitly assumed.
    const ONE_SCREEN: &str = "DP-1";
    /// The second screen the per-output tests add.
    const OTHER_SCREEN: &str = "DP-2";

    fn workspace(id: u64, is_focused: bool) -> Workspace {
        workspace_on(id, ONE_SCREEN, is_focused)
    }

    /// A workspace on `output`, active there iff `is_active`.
    ///
    /// `is_focused` and `is_active` are deliberately the same argument for the
    /// single-screen helper above: on a one-monitor desktop the active
    /// workspace *is* the focused one, and the fold reads only `is_active`
    /// now (#1050).
    fn workspace_on(id: u64, output: &str, is_active: bool) -> Workspace {
        Workspace {
            id,
            idx: 1,
            name: None,
            output: Some(output.to_owned()),
            is_urgent: false,
            is_active,
            is_focused: is_active,
            active_window_id: None,
        }
    }

    fn window(id: u64, workspace_id: Option<u64>) -> Window {
        Window {
            id,
            title: None,
            app_id: None,
            pid: None,
            workspace_id,
            is_focused: false,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: Some((1, 1)),
                tile_size: (100.0, 100.0),
                window_size: (100, 100),
                tile_pos_in_workspace_view: Some((0.0, 0.0)),
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    fn windows_changed(windows: Vec<Window>) -> Event {
        Event::WindowsChanged { windows }
    }

    /// One screen, three workspaces on it, `active` the one that is visible.
    ///
    /// The `active + 1` sibling exists so an activation test has a workspace to
    /// switch **to**: since #1050 a `WorkspaceActivated` for an id the fold has
    /// never seen is ignored (its output is unknown), so a fixture that listed
    /// only the active one would make every switch a no-op.
    fn workspaces_changed(active: u64) -> Event {
        Event::WorkspacesChanged {
            workspaces: vec![
                workspace(active, true),
                workspace(active + 1, false),
                workspace(active + 100, false),
            ],
        }
    }

    /// Two screens' worth of workspaces: `DP-1` showing `dp1`, `DP-2` showing
    /// `dp2`, each with one idle sibling so an activation has somewhere to go.
    fn two_screens(dp1: u64, dp2: u64) -> Event {
        Event::WorkspacesChanged {
            workspaces: vec![
                workspace_on(dp1, ONE_SCREEN, true),
                workspace_on(dp1 + 1, ONE_SCREEN, false),
                workspace_on(dp2, OTHER_SCREEN, true),
                workspace_on(dp2 + 1, OTHER_SCREEN, false),
            ],
        }
    }

    /// Fold a whole burst and collect every verdict it emitted.
    fn observe_all(watch: &mut Watch, events: Vec<Event>) -> Vec<Verdict> {
        events
            .into_iter()
            .filter_map(|event| watch.observe(event))
            .collect()
    }

    /// The verdict "the chip is up, and nothing is hiding it" — a one-screen
    /// desktop over the threshold.
    fn up() -> Verdict {
        Verdict {
            hidden_on: Vec::new(),
            shows_anywhere: true,
        }
    }

    /// The verdict "no screen wants the chip". Identical to
    /// [`Verdict::default`] by construction — see the normalisation note on
    /// [`Watch::verdict`] — which is exactly why a fresh watch says nothing
    /// when its first complete snapshot is an empty desktop.
    fn nowhere() -> Verdict {
        Verdict::default()
    }

    /// Whether the chip is drawn on `output`, read off the **verdict** rather
    /// than the raw counts — so a `verdict()` that mislabels a screen fails
    /// these too, not just a miscount.
    fn shows_on(watch: &Watch, output: &str) -> bool {
        let verdict = watch.verdict();
        verdict.shows_anywhere && !verdict.hidden_on.iter().any(|name| name == output)
    }

    /// Windows on `output`'s active workspace. `0` for a screen niri has not
    /// mentioned.
    fn count_on(watch: &Watch, output: &str) -> usize {
        watch.window_counts().get(output).copied().unwrap_or(0)
    }

    #[test]
    fn a_fresh_watch_is_hidden_and_says_nothing() {
        let watch = Watch::default();

        assert_eq!(watch.verdict(), nowhere(), "the chip starts hidden");
        assert!(!shows_on(&watch, ONE_SCREEN));
        assert_eq!(
            count_on(&watch, ONE_SCREEN),
            0,
            "nothing known about niri yet"
        );
    }

    /// The whole rule in one test: one window is hidden, the second reveals.
    #[test]
    fn the_chip_appears_at_the_second_window_and_not_before() {
        let mut watch = Watch::default();

        let emitted = observe_all(
            &mut watch,
            vec![
                workspaces_changed(1),
                windows_changed(vec![window(10, Some(1))]),
            ],
        );
        assert!(emitted.is_empty(), "one window stays hidden, silently");
        assert_eq!(count_on(&watch, ONE_SCREEN), 1);

        let emitted = observe_all(
            &mut watch,
            vec![Event::WindowOpenedOrChanged {
                window: window(20, Some(1)),
            }],
        );
        assert_eq!(emitted, vec![up()], "the second window reveals the chip");
        assert!(shows_on(&watch, ONE_SCREEN));
    }

    #[test]
    fn closing_back_down_to_one_window_hides_it_again() {
        let mut watch = Watch::default();
        observe_all(
            &mut watch,
            vec![
                workspaces_changed(1),
                windows_changed(vec![window(10, Some(1)), window(20, Some(1))]),
            ],
        );
        assert!(shows_on(&watch, ONE_SCREEN), "two windows: shown");

        let emitted = observe_all(&mut watch, vec![Event::WindowClosed { id: 20 }]);

        assert_eq!(emitted, vec![nowhere()]);
        assert!(!shows_on(&watch, ONE_SCREEN));
    }

    /// A stacked column of two windows is **two windows** — Annika's literal
    /// wording, and deliberately not the column count
    /// [`plan`](crate::layout::plan) uses.
    #[test]
    fn two_windows_stacked_in_one_column_still_count_as_two() {
        let mut watch = Watch::default();
        let mut lower = window(10, Some(1));
        lower.layout.pos_in_scrolling_layout = Some((1, 1));
        let mut upper = window(20, Some(1));
        upper.layout.pos_in_scrolling_layout = Some((1, 2));

        let emitted = observe_all(
            &mut watch,
            vec![workspaces_changed(1), windows_changed(vec![lower, upper])],
        );

        assert_eq!(emitted, vec![up()], "one column, two windows, chip shown");
    }

    /// A floating window is a window. The chip is a *visibility* rule, not a
    /// preview of what a click will resize.
    #[test]
    fn a_floating_window_counts_too() {
        let mut watch = Watch::default();
        let mut floater = window(20, Some(1));
        floater.is_floating = true;
        floater.layout.pos_in_scrolling_layout = None;

        let emitted = observe_all(
            &mut watch,
            vec![
                workspaces_changed(1),
                windows_changed(vec![window(10, Some(1)), floater]),
            ],
        );

        assert_eq!(emitted, vec![up()]);
    }

    #[test]
    fn windows_on_another_workspace_do_not_reveal_the_chip() {
        let mut watch = Watch::default();

        let emitted = observe_all(
            &mut watch,
            vec![
                workspaces_changed(1),
                windows_changed(vec![
                    window(10, Some(1)),
                    window(20, Some(2)),
                    window(30, Some(2)),
                    // A window niri gives no workspace at all.
                    window(40, None),
                ]),
            ],
        );

        assert!(
            emitted.is_empty(),
            "DP-1's active workspace holds one window"
        );
        assert_eq!(count_on(&watch, ONE_SCREEN), 1);
    }

    /// Switching to a busier workspace reveals the chip without a single window
    /// event — the count follows the focus.
    #[test]
    fn switching_workspaces_re_decides_on_the_new_one() {
        let mut watch = Watch::default();
        observe_all(
            &mut watch,
            vec![
                workspaces_changed(1),
                windows_changed(vec![
                    window(10, Some(1)),
                    window(20, Some(2)),
                    window(30, Some(2)),
                ]),
            ],
        );
        assert!(
            !shows_on(&watch, ONE_SCREEN),
            "workspace 1 holds one window"
        );

        let emitted = observe_all(
            &mut watch,
            vec![Event::WorkspaceActivated {
                id: 2,
                focused: true,
            }],
        );

        assert_eq!(emitted, vec![up()], "workspace 2 holds two");
    }

    /// `WorkspaceActivated`'s `focused` flag is **not** read (#1050).
    ///
    /// niri's own docs: the event means the workspace is now active on its
    /// output; `focused` merely adds "and it took keyboard focus". The pre-#1050
    /// fold read *only* the flag, which is why switching workspaces on the
    /// second monitor moved the one global answer — the bug this issue is. On
    /// this screen, an activation without focus is a real change and must land.
    #[test]
    fn an_activation_without_focus_still_re_decides_its_own_screen() {
        let mut watch = Watch::default();
        observe_all(
            &mut watch,
            vec![
                workspaces_changed(1),
                windows_changed(vec![
                    window(10, Some(1)),
                    window(20, Some(2)),
                    window(30, Some(2)),
                ]),
            ],
        );
        assert!(
            !shows_on(&watch, ONE_SCREEN),
            "workspace 1 holds one window"
        );

        let emitted = observe_all(
            &mut watch,
            vec![Event::WorkspaceActivated {
                id: 2,
                focused: false,
            }],
        );

        assert_eq!(
            emitted,
            vec![up()],
            "workspace 2 is what DP-1 shows now, focused or not"
        );
        assert_eq!(count_on(&watch, ONE_SCREEN), 2);
    }

    /// An activation for a workspace the fold has never heard of is ignored,
    /// not invented: its output is unknown, so there is neither a set of
    /// siblings to deactivate nor a screen it could raise a count on.
    #[test]
    fn an_activation_for_an_unknown_workspace_changes_nothing() {
        let mut watch = Watch::default();
        observe_all(
            &mut watch,
            vec![
                workspaces_changed(1),
                windows_changed(vec![window(10, Some(1)), window(20, Some(1))]),
            ],
        );
        assert!(shows_on(&watch, ONE_SCREEN));

        let emitted = observe_all(
            &mut watch,
            vec![Event::WorkspaceActivated {
                id: 9_999,
                focused: true,
            }],
        );

        assert!(emitted.is_empty(), "got {emitted:?}");
        assert_eq!(
            count_on(&watch, ONE_SCREEN),
            2,
            "DP-1's own active workspace must not be deactivated by an id that \
             belongs to no output we know"
        );
    }

    // ── One verdict per screen (#1050) ──────────────────────────────────────

    /// The bug, as a test: DP-1 is busy and DP-2 is not, so the chip belongs on
    /// DP-1 and nowhere else. Before #1050 the whole verdict was one bool off
    /// the *focused* workspace, and DP-2's chip followed DP-1's count.
    #[test]
    fn a_busy_screen_shows_while_a_quiet_one_is_named_in_hidden_on() {
        let mut watch = Watch::default();

        let emitted = observe_all(
            &mut watch,
            vec![
                two_screens(1, 10),
                windows_changed(vec![
                    window(100, Some(1)),
                    window(200, Some(1)),
                    window(300, Some(1)),
                    window(400, Some(10)),
                ]),
            ],
        );

        assert_eq!(
            emitted,
            vec![Verdict {
                hidden_on: vec![OTHER_SCREEN.to_owned()],
                shows_anywhere: true,
            }],
            "DP-1 has three windows, DP-2 has one"
        );
        assert!(shows_on(&watch, ONE_SCREEN));
        assert!(!shows_on(&watch, OTHER_SCREEN));
        assert_eq!(count_on(&watch, ONE_SCREEN), 3);
        assert_eq!(count_on(&watch, OTHER_SCREEN), 1);
    }

    /// Both screens below the threshold collapse to the same "nowhere" verdict
    /// a one-monitor desktop produces — **with an empty `hidden_on`**, because
    /// the plugin renders the empty tree there and #1042's region-collapse rule
    /// needs that (see `plugin::view`).
    #[test]
    fn both_screens_below_the_threshold_hide_everywhere_and_name_no_one() {
        let mut watch = Watch::default();

        let emitted = observe_all(
            &mut watch,
            vec![
                two_screens(1, 10),
                windows_changed(vec![window(100, Some(1)), window(400, Some(10))]),
            ],
        );

        assert!(
            emitted.is_empty(),
            "identical to the initial verdict, so nothing to say: {emitted:?}"
        );
        assert_eq!(watch.verdict(), nowhere());
    }

    /// The verdict follows **that screen's** active workspace: switching DP-2
    /// onto a busy workspace reveals DP-2's chip and leaves DP-1's answer
    /// exactly where it was.
    #[test]
    fn activating_a_busy_workspace_on_the_second_screen_reveals_only_that_chip() {
        let mut watch = Watch::default();
        observe_all(
            &mut watch,
            vec![
                two_screens(1, 10),
                windows_changed(vec![
                    window(100, Some(1)),
                    window(200, Some(1)),
                    // DP-2 shows workspace 10 (empty); workspace 11 holds two.
                    window(400, Some(11)),
                    window(500, Some(11)),
                ]),
            ],
        );
        assert_eq!(
            watch.verdict(),
            Verdict {
                hidden_on: vec![OTHER_SCREEN.to_owned()],
                shows_anywhere: true,
            },
            "precondition: only DP-1 shows"
        );

        let emitted = observe_all(
            &mut watch,
            vec![Event::WorkspaceActivated {
                id: 11,
                focused: false,
            }],
        );

        assert_eq!(
            emitted,
            vec![Verdict {
                hidden_on: Vec::new(),
                shows_anywhere: true,
            }],
            "DP-2 caught up; DP-1 never moved"
        );
        assert!(shows_on(&watch, ONE_SCREEN));
        assert!(shows_on(&watch, OTHER_SCREEN));
    }

    /// Activating on one output must not deactivate the *other* output's
    /// workspace — the one-line "mark every other workspace inactive" is wrong
    /// and would blank DP-1's count until its own next event.
    #[test]
    fn an_activation_on_one_screen_leaves_the_other_screens_answer_alone() {
        let mut watch = Watch::default();
        observe_all(
            &mut watch,
            vec![
                two_screens(1, 10),
                windows_changed(vec![
                    window(100, Some(1)),
                    window(200, Some(1)),
                    window(400, Some(10)),
                    window(500, Some(10)),
                ]),
            ],
        );
        assert!(shows_on(&watch, ONE_SCREEN) && shows_on(&watch, OTHER_SCREEN));

        // DP-2 switches to its empty sibling.
        let emitted = observe_all(
            &mut watch,
            vec![Event::WorkspaceActivated {
                id: 11,
                focused: true,
            }],
        );

        assert_eq!(
            emitted,
            vec![Verdict {
                hidden_on: vec![OTHER_SCREEN.to_owned()],
                shows_anywhere: true,
            }]
        );
        assert_eq!(
            count_on(&watch, ONE_SCREEN),
            2,
            "DP-1's two windows are still on DP-1's active workspace"
        );
    }

    /// `hidden_on` is **sorted**, whatever order niri lists the outputs in.
    ///
    /// Not cosmetic: the vec is the dedup key, so an order that depended on
    /// `HashMap` iteration would report a "change" on events that changed
    /// nothing and re-render the chip through every drag.
    #[test]
    fn hidden_on_is_sorted_however_niri_orders_the_outputs() {
        let mut watch = Watch::default();

        observe_all(
            &mut watch,
            vec![
                Event::WorkspacesChanged {
                    workspaces: vec![
                        workspace_on(1, "DP-9", true),
                        workspace_on(2, "eDP-1", true),
                        workspace_on(3, "DP-1", true),
                        workspace_on(4, "HDMI-A-2", true),
                    ],
                },
                // Only DP-1 is busy; the other three are named, in order.
                windows_changed(vec![window(10, Some(3)), window(20, Some(3))]),
            ],
        );

        assert_eq!(
            watch.verdict().hidden_on,
            vec!["DP-9".to_owned(), "HDMI-A-2".to_owned(), "eDP-1".to_owned()],
            "byte order, from the BTreeMap the counts are collected into"
        );
    }

    /// A workspace niri reports with no output (its monitor is unplugged) can
    /// raise no screen's count — there is no screen to show a chip on.
    #[test]
    fn windows_on_an_outputless_workspace_show_nowhere() {
        let mut watch = Watch::default();

        let emitted = observe_all(
            &mut watch,
            vec![
                Event::WorkspacesChanged {
                    workspaces: vec![
                        workspace_on(1, ONE_SCREEN, true),
                        Workspace {
                            output: None,
                            ..workspace_on(2, ONE_SCREEN, true)
                        },
                    ],
                },
                windows_changed(vec![
                    window(10, Some(1)),
                    window(20, Some(2)),
                    window(30, Some(2)),
                ]),
            ],
        );

        assert!(
            emitted.is_empty(),
            "DP-1 holds one window and the orphaned pair belongs nowhere: \
             {emitted:?}"
        );
        assert_eq!(count_on(&watch, ONE_SCREEN), 1);
        assert_eq!(watch.verdict(), nowhere());
    }

    /// An output whose active workspace is *empty* must be **named** in
    /// `hidden_on`, not merely absent from the counts — a screen missing from
    /// the list is a screen the host leaves the chip on.
    #[test]
    fn an_empty_screen_is_named_rather_than_omitted() {
        let mut watch = Watch::default();

        observe_all(
            &mut watch,
            vec![
                two_screens(1, 10),
                // Nothing at all on DP-2's active workspace.
                windows_changed(vec![window(100, Some(1)), window(200, Some(1))]),
            ],
        );

        assert_eq!(
            watch.verdict(),
            Verdict {
                hidden_on: vec![OTHER_SCREEN.to_owned()],
                shows_anywhere: true,
            },
            "zero windows still has to say 'hide me here'"
        );
        assert_eq!(count_on(&watch, OTHER_SCREEN), 0);
    }

    /// The reason [`Watch::observe`] returns an `Option` at all: niri is chatty.
    #[test]
    fn a_burst_that_does_not_cross_the_threshold_emits_nothing() {
        let mut watch = Watch::default();
        observe_all(
            &mut watch,
            vec![
                workspaces_changed(1),
                windows_changed(vec![window(10, Some(1)), window(20, Some(1))]),
            ],
        );

        let emitted = observe_all(
            &mut watch,
            vec![
                Event::WindowOpenedOrChanged {
                    window: window(30, Some(1)),
                },
                Event::WindowFocusChanged { id: Some(30) },
                Event::WindowClosed { id: 30 },
                Event::WindowOpenedOrChanged {
                    window: window(10, Some(1)),
                },
            ],
        );

        assert!(
            emitted.is_empty(),
            "three windows and two are both 'shown', so nothing to say: {emitted:?}"
        );
        assert!(shows_on(&watch, ONE_SCREEN));
    }

    /// Every event the fold ignores, fed to a watch that would otherwise be on
    /// the edge. None of them may move the count.
    #[test]
    fn unrelated_events_are_inert() {
        let mut watch = Watch::default();
        observe_all(
            &mut watch,
            vec![
                workspaces_changed(1),
                windows_changed(vec![window(10, Some(1))]),
            ],
        );

        let emitted = observe_all(
            &mut watch,
            vec![
                Event::WindowFocusChanged { id: Some(10) },
                Event::WindowUrgencyChanged {
                    id: 10,
                    urgent: true,
                },
                Event::WorkspaceUrgencyChanged {
                    id: 1,
                    urgent: true,
                },
                Event::WorkspaceActiveWindowChanged {
                    workspace_id: 1,
                    active_window_id: Some(10),
                },
                Event::OverviewOpenedOrClosed { is_open: true },
                Event::KeyboardLayoutSwitched { idx: 1 },
            ],
        );

        assert!(emitted.is_empty(), "got {emitted:?}");
        assert_eq!(count_on(&watch, ONE_SCREEN), 1);
    }

    /// A window moved to another workspace arrives as `WindowOpenedOrChanged`
    /// carrying its new `workspace_id`, so the upsert must overwrite rather than
    /// leave the old mapping in place.
    #[test]
    fn a_window_moved_off_the_focused_workspace_stops_counting() {
        let mut watch = Watch::default();
        observe_all(
            &mut watch,
            vec![
                workspaces_changed(1),
                windows_changed(vec![window(10, Some(1)), window(20, Some(1))]),
            ],
        );
        assert!(shows_on(&watch, ONE_SCREEN));

        let emitted = observe_all(
            &mut watch,
            vec![Event::WindowOpenedOrChanged {
                window: window(20, Some(2)),
            }],
        );

        assert_eq!(emitted, vec![nowhere()], "it left, so we are back to one");
        assert_eq!(
            count_on(&watch, ONE_SCREEN),
            1,
            "and it is not double-counted"
        );
    }

    /// `WindowsChanged` replaces; it must not merge into what was there.
    #[test]
    fn a_windows_snapshot_replaces_rather_than_merges() {
        let mut watch = Watch::default();
        observe_all(
            &mut watch,
            vec![
                workspaces_changed(1),
                windows_changed(vec![window(10, Some(1)), window(20, Some(1))]),
            ],
        );

        let emitted = observe_all(&mut watch, vec![windows_changed(vec![window(30, Some(1))])]);

        assert_eq!(emitted, vec![nowhere()], "the old two are gone, not kept");
        assert_eq!(count_on(&watch, ONE_SCREEN), 1);
    }

    /// A reconnect drops niri's ids (they mean nothing across a restart) but
    /// keeps the verdict, so the chip does not blink through a niri restart that
    /// lands back on the same workspace.
    #[test]
    fn a_reconnect_forgets_the_ids_and_re_emits_only_on_a_real_change() {
        let mut watch = Watch::default();
        observe_all(
            &mut watch,
            vec![
                workspaces_changed(1),
                windows_changed(vec![window(10, Some(1)), window(20, Some(1))]),
            ],
        );
        assert!(shows_on(&watch, ONE_SCREEN));

        watch.forget_compositor_state();
        assert_eq!(
            count_on(&watch, ONE_SCREEN),
            0,
            "niri's ids are meaningless now"
        );

        // The same shape comes back under fresh ids: no flip, so nothing is
        // said and the chip never blinks.
        let emitted = observe_all(
            &mut watch,
            vec![
                workspaces_changed(7),
                windows_changed(vec![window(70, Some(7)), window(80, Some(7))]),
            ],
        );
        assert!(
            emitted.is_empty(),
            "identical verdict, no churn: {emitted:?}"
        );

        // A restart that lands somewhere emptier does emit.
        watch.forget_compositor_state();
        let emitted = observe_all(
            &mut watch,
            vec![
                workspaces_changed(9),
                windows_changed(vec![window(90, Some(9))]),
            ],
        );
        assert_eq!(emitted, vec![nowhere()]);
    }

    /// A **half-arrived** opening snapshot must say nothing — in either order.
    ///
    /// niri opens an `EventStream` with a burst, and between its two halves the
    /// state momentarily reads "zero windows on no workspace". Emitting there is
    /// what made a reconnect blink the chip off and back on.
    ///
    /// It has to be tested **from a shown chip**, which is the only state where
    /// the bug is observable: on a *fresh* watch the half-arrived answer
    /// ("hidden") happens to equal the initial one, so the change-only gate
    /// masks the missing guard. Measured — a from-`default()` version of this
    /// test stayed green with `snapshot_complete` deleted, which is what the
    /// first draft of it did.
    #[test]
    fn half_an_opening_snapshot_emits_nothing_whichever_half_lands_first() {
        /// A watch that has already shown the chip and then lost its niri.
        fn reconnecting() -> Watch {
            let mut watch = Watch::default();
            observe_all(
                &mut watch,
                vec![
                    workspaces_changed(1),
                    windows_changed(vec![window(10, Some(1)), window(20, Some(1))]),
                ],
            );
            assert!(shows_on(&watch, ONE_SCREEN), "precondition: the chip is up");
            watch.forget_compositor_state();
            watch
        }

        // Workspaces first, windows still to come.
        let mut watch = reconnecting();
        assert!(
            observe_all(&mut watch, vec![workspaces_changed(7)]).is_empty(),
            "no window list yet — claiming 'hidden' here is the blink"
        );
        assert!(
            observe_all(
                &mut watch,
                vec![windows_changed(vec![
                    window(70, Some(7)),
                    window(80, Some(7))
                ])]
            )
            .is_empty(),
            "and the completed snapshot agrees with what was already shown"
        );

        // Windows first, workspaces still to come.
        let mut watch = reconnecting();
        assert!(
            observe_all(
                &mut watch,
                vec![windows_changed(vec![
                    window(70, Some(7)),
                    window(80, Some(7))
                ])]
            )
            .is_empty(),
            "nothing is focused yet, so nothing may be claimed"
        );
        assert!(
            observe_all(&mut watch, vec![workspaces_changed(7)]).is_empty(),
            "same verdict, still nothing to say"
        );

        // …and the guard only *defers* the answer, it never swallows one: a
        // snapshot that really did change the verdict still lands.
        let mut watch = reconnecting();
        assert_eq!(
            observe_all(
                &mut watch,
                vec![
                    workspaces_changed(7),
                    windows_changed(vec![window(70, Some(7))]),
                ]
            ),
            vec![nowhere()],
            "one window on the new snapshot: hide, exactly once"
        );
    }

    #[test]
    fn the_threshold_is_two() {
        assert_eq!(MIN_WINDOWS, 2, "\"more than 1 window\" (#1019)");
    }

    #[test]
    fn the_backoff_doubles_from_one_second_to_a_thirty_second_ceiling() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(4), Duration::from_secs(16));
        assert_eq!(backoff(5), BACKOFF_CEILING, "32 s clamps to the ceiling");
    }

    /// The clamp is what stops a shift overflow **and** a runaway wait: a niri
    /// that never returns must keep costing one attempt per 30 s, not one per
    /// eon, and `1_u32 << 32` is a panic in debug.
    #[test]
    fn the_backoff_is_clamped_however_many_attempts_fail() {
        for attempt in [6_u32, 31, 32, 64, u32::MAX] {
            assert_eq!(
                backoff(attempt),
                BACKOFF_CEILING,
                "attempt {attempt} must clamp, not overflow"
            );
        }
    }

    // ── the reconnect loop (#1038 review, MED-3) ─────────────────────────────
    //
    // `drive` is the half of this module that used to have no coverage at all:
    // deleting its priming call, its spawn wiring or its backoff policy left the
    // suite green. Everything below drives it over the [`Backend`] seam with a
    // scripted connection, so each of those decisions has a test that fails when
    // it is removed.

    /// One scripted connection: a refusal, or the polls it hands out in order.
    type Connection = Result<Vec<Incoming>, String>;

    fn ev(event: Event) -> Incoming {
        Incoming::Event(Box::new(event))
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Two windows on `workspace`, as the opening snapshot of one connection.
    fn opening_snapshot(ws: u64, first: u64, second: u64) -> Vec<Incoming> {
        vec![
            ev(workspaces_changed(ws)),
            ev(windows_changed(vec![
                window(first, Some(ws)),
                window(second, Some(ws)),
            ])),
        ]
    }

    struct Scripted(VecDeque<Incoming>);

    impl EventSource for Scripted {
        fn next_event(&mut self) -> Incoming {
            self.0
                .pop_front()
                .unwrap_or_else(|| Incoming::Ended("the scripted connection ran out".to_owned()))
        }
    }

    /// A [`Backend`] that records every decision [`drive`] makes.
    struct Script {
        connections: VecDeque<Connection>,
        /// Keep answering `connect` after the script runs out instead of ending
        /// the loop. **Load-bearing for the shutdown tests**: a fake that stops
        /// the loop by itself would let a `drive` that ignores the shutdown
        /// signal pass anyway.
        endless: bool,
        /// What `emit` reports; `false` is a dropped session receiver.
        accepts: bool,
        /// What `alive` reports.
        open: bool,
        /// Trips loudly if `drive` never returns, so a missing exit path is a
        /// failing test rather than a hanging one.
        budget: usize,
        emitted: Vec<Verdict>,
        waits: Vec<Duration>,
        logs: Vec<String>,
    }

    impl Script {
        fn of(connections: Vec<Connection>) -> Self {
            Self {
                connections: connections.into(),
                endless: false,
                accepts: true,
                open: true,
                budget: 32,
                emitted: Vec::new(),
                waits: Vec::new(),
                logs: Vec::new(),
            }
        }

        /// Never let the fake itself end the loop — see [`Script::endless`].
        fn endless(mut self) -> Self {
            self.endless = true;
            self
        }

        /// The session's receiver is gone: every send fails.
        fn rejecting(mut self) -> Self {
            self.accepts = false;
            self
        }

        /// The session's receiver is gone, and nothing has been sent since.
        fn closed(mut self) -> Self {
            self.open = false;
            self
        }

        fn warnings(&self) -> Vec<&String> {
            self.logs.iter().filter(|l| l.contains("WARNING")).collect()
        }
    }

    impl Backend for Script {
        type Source = Scripted;

        fn connect(&mut self) -> Result<Self::Source, String> {
            match self.connections.pop_front() {
                Some(Ok(polls)) => Ok(Scripted(polls.into())),
                Some(Err(why)) => Err(why),
                None => Err("the script ran out of connections".to_owned()),
            }
        }

        fn emit(&mut self, verdict: Verdict) -> bool {
            self.emitted.push(verdict);
            self.accepts
        }

        fn alive(&self) -> bool {
            self.open
        }

        fn wait(&mut self, delay: Duration) -> bool {
            self.waits.push(delay);
            self.budget = self.budget.checked_sub(1).expect(
                "drive() never returned: it kept reconnecting long after the session ended",
            );
            self.open && (self.endless || !self.connections.is_empty())
        }

        fn log(&mut self, line: &str) {
            self.logs.push(line.to_owned());
        }
    }

    /// The blink, at the loop level: a niri restart must not take the chip away
    /// and give it back.
    ///
    /// Two connections back to back over a two-window workspace. Between them
    /// `drive` re-primes the fold; without that the second connection's
    /// `WorkspacesChanged` is judged against the *first* connection's window map
    /// — no window on the new active workspace — and emits a hide a
    /// millisecond before `WindowsChanged` shows it again. The `Watch`-level
    /// tests cannot see this: they call `forget_compositor_state` themselves.
    #[test]
    fn a_reconnect_primes_the_fold_so_the_chip_never_blinks() {
        let mut script = Script::of(vec![
            Ok(opening_snapshot(1, 10, 20)),
            Ok(opening_snapshot(7, 70, 80)),
        ]);

        drive("test", &mut script);

        assert_eq!(
            script.emitted,
            vec![up()],
            "the chip goes up once and stays up across the reconnect"
        );
    }

    /// The backoff policy `drive` owns: absence grows the delay, a stream that
    /// actually lived resets it — and the **first** retry is one second, which
    /// is what the doc comment has always claimed (#1038 review, LOW-7).
    #[test]
    fn the_backoff_grows_while_niri_is_absent_and_a_live_stream_resets_it() {
        let absent = || Err("cannot reach $NIRI_SOCKET".to_owned());
        let mut script = Script::of(vec![
            absent(),
            absent(),
            absent(),
            Ok(opening_snapshot(1, 10, 20)),
            absent(),
        ]);

        drive("test", &mut script);

        assert_eq!(
            script.waits,
            vec![secs(1), secs(2), secs(4), secs(1), secs(1)],
            "1 → 2 → 4 while niri is absent; the live stream resets it, so the \
             retry after it starts at 1 s again"
        );
    }

    /// The shutdown contract, on the send path: the first verdict that cannot be
    /// delivered ends the watcher (#1038 review, HIGH-2).
    #[test]
    fn a_dropped_verdict_receiver_stops_the_watcher() {
        let mut script = Script::of(vec![Ok(opening_snapshot(1, 10, 20))])
            .endless()
            .rejecting();

        drive("test", &mut script);

        assert_eq!(
            script.emitted,
            vec![up()],
            "it stops at the first undeliverable verdict"
        );
        assert!(
            script.waits.is_empty(),
            "and never schedules a reconnect for a session that is over: {:?}",
            script.waits
        );
    }

    /// The same contract on the **quiet** path, which is the one that matters in
    /// practice: niri says nothing for minutes at a time, so the timeout tick is
    /// the only thing that can notice the session ended.
    #[test]
    fn a_session_that_ended_while_niri_was_quiet_stops_the_watcher_too() {
        let mut script = Script::of(vec![Ok(vec![Incoming::Idle, Incoming::Idle])])
            .endless()
            .closed();

        drive("test", &mut script);

        assert!(
            script.emitted.is_empty(),
            "nothing to say: {:?}",
            script.emitted
        );
        assert!(
            script.waits.is_empty(),
            "it returned instead of reconnecting: {:?}",
            script.waits
        );
    }

    /// One WARNING per outage, not one per attempt — and it has to name the
    /// consequence, because on glass an unreachable niri is indistinguishable
    /// from a workspace with one window on it (#1038 review, LOW-8).
    #[test]
    fn an_unreachable_niri_warns_once_per_outage_naming_the_hidden_chip() {
        let absent = || Err("$NIRI_SOCKET is not set".to_owned());
        let mut script = Script::of(vec![
            absent(),
            absent(),
            Ok(opening_snapshot(1, 10, 20)),
            absent(),
        ]);

        drive("niri-layouts", &mut script);

        let warnings = script.warnings();
        assert_eq!(
            warnings.len(),
            2,
            "one for the first outage and one for the outage after niri came \
             back — not one per attempt: {:?}",
            script.logs
        );
        assert!(
            warnings[0].contains("hidden"),
            "the operator has to be told what the silence looks like: {:?}",
            warnings[0]
        );
        assert_eq!(
            script.logs.len(),
            4,
            "every attempt still logs something, it just does not shout twice: \
             {:?}",
            script.logs
        );
    }

    /// The **real** backend's wait, which the scripted one above stands in for
    /// everywhere else: a 30 s backoff must not hold the thread for 30 s after
    /// the session ends, so it is slept in [`POLL_INTERVAL`] slices with the
    /// lane checked between them.
    #[test]
    fn the_real_backend_stops_waiting_out_a_backoff_once_the_session_is_over() {
        /// A lane whose receiver is already gone.
        struct Gone;
        impl Verdicts for Gone {
            fn send(&mut self, _verdict: Verdict) -> bool {
                false
            }
            fn open(&self) -> bool {
                false
            }
        }

        let mut backend = SocketBackend { verdicts: Gone };
        let started = std::time::Instant::now();

        let carry_on = backend.wait(BACKOFF_CEILING);

        assert!(
            !carry_on,
            "a dead lane ends the loop rather than reconnecting"
        );
        assert!(
            started.elapsed() < BACKOFF_CEILING / 4,
            "it returned after {:?} — a single long park would have held the \
             thread for the whole ceiling",
            started.elapsed()
        );
    }

    /// An event split across a timeout tick still decodes.
    ///
    /// This is the invariant that makes the read timeout safe at all: a byte
    /// read that times out mid-line keeps the bytes it already read appended
    /// to the buffer, so [`SocketEvents`] must **not** clear it between polls.
    /// Clearing it would turn every tick that lands mid-event into a decode
    /// failure — i.e. a dropped niri connection — under exactly the load (a
    /// burst of events) where the chip most needs to be right.
    #[test]
    fn an_event_split_across_a_timeout_tick_still_decodes() {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");

        let niri = std::thread::spawn(move || {
            let mut reader = BufReader::new(theirs.try_clone().expect("clone"));
            let mut request = String::new();
            reader.read_line(&mut request).expect("a request line");
            let mut writer = &theirs;
            writer
                .write_all(b"{\"Ok\":\"Handled\"}\n")
                .expect("the reply");
            // Half an event, then a silence longer than one poll tick, then the
            // rest of it.
            writer
                .write_all(b"{\"WindowClosed\":{\"id\":")
                .expect("half an event");
            std::thread::sleep(POLL_INTERVAL * 2);
            writer.write_all(b"7}}\n").expect("the other half");
            std::thread::sleep(POLL_INTERVAL);
        });

        let mut events = SocketEvents::over(ours).expect("the handshake completes");
        let mut ticks = 0;
        let decoded = loop {
            match events.next_event() {
                Incoming::Idle => {
                    ticks += 1;
                    assert!(ticks < 20, "the second half never arrived");
                }
                other => break other,
            }
        };

        assert!(ticks >= 1, "the split has to straddle at least one tick");
        assert!(
            matches!(&decoded, Incoming::Event(e) if matches!(**e, Event::WindowClosed { id: 7 })),
            "the halves were reassembled, not dropped: {decoded:?}"
        );

        niri.join().expect("the fake niri thread");
    }

    /// A read timeout landing **inside** a multi-byte UTF-8 character must not
    /// drop the connection (#1053, #1038 review LOW-9).
    ///
    /// [`BufReader::read_line`] validates UTF-8 on every call and silently
    /// truncates a partial sequence at the end of what it just read — so the
    /// sibling test above, which cuts on an ASCII boundary, only pins half the
    /// invariant. [`SocketEvents`] reads bytes (`read_until`/`from_slice`)
    /// precisely so a split like this one is invisible to it: the buffer holds
    /// raw bytes across the tick, and the whole line is decoded only once it is
    /// complete.
    #[test]
    fn an_event_split_inside_a_utf8_char_still_decodes() {
        let mut w = window(7, Some(1));
        w.title = Some("caf\u{e9} \u{2014} Mozilla Firefox".to_owned());
        let json =
            serde_json::to_string(&Event::WindowOpenedOrChanged { window: w }).expect("encode");
        // Between the two bytes of 'é' — cutting here is the whole point.
        let cut = json.find('\u{e9}').expect("the accent is in the title") + 1;
        let mut bytes = json.into_bytes();
        bytes.push(b'\n');
        let tail = bytes.split_off(cut);
        let head = bytes;

        let (ours, theirs) = UnixStream::pair().expect("socketpair");

        let niri = std::thread::spawn(move || {
            let mut reader = BufReader::new(theirs.try_clone().expect("clone"));
            let mut request = String::new();
            reader.read_line(&mut request).expect("a request line");
            let mut writer = &theirs;
            writer
                .write_all(b"{\"Ok\":\"Handled\"}\n")
                .expect("the reply");
            // Half of 'é', then a silence longer than one poll tick, then the
            // rest of the event.
            writer.write_all(&head).expect("the first half, cut mid-é");
            std::thread::sleep(POLL_INTERVAL * 2);
            writer.write_all(&tail).expect("the second half");
            std::thread::sleep(POLL_INTERVAL);
        });

        let mut events = SocketEvents::over(ours).expect("the handshake completes");
        let mut ticks = 0;
        let decoded = loop {
            match events.next_event() {
                Incoming::Idle => {
                    ticks += 1;
                    assert!(ticks < 20, "the second half never arrived");
                }
                other => break other,
            }
        };

        assert!(ticks >= 1, "the split has to straddle at least one tick");
        match decoded {
            Incoming::Event(e) => match *e {
                Event::WindowOpenedOrChanged { window } => assert_eq!(
                    window.title.as_deref(),
                    Some("caf\u{e9} \u{2014} Mozilla Firefox"),
                    "the title must survive whole, not truncated at the split"
                ),
                other => panic!("wrong event: {other:?}"),
            },
            other => panic!("the halves were reassembled, not dropped: {other:?}"),
        }

        niri.join().expect("the fake niri thread");
    }

    // ── the hand-rolled framing (#1038) ──────────────────────────────────────

    /// The bytes this module puts on `$NIRI_SOCKET`, pinned as **literals**
    /// against a scripted socket.
    ///
    /// [`SocketEvents`] frames the `EventStream` handshake itself rather than
    /// going through [`niri_ipc::socket::Socket`] (which cannot arm a read
    /// timeout, so a watcher would park forever on a dead channel). That means
    /// this crate now owns a piece of niri's wire protocol, and the #1026 lesson
    /// applies: a request assembled from the crate's own constants proves
    /// nothing about what the daemon on the other end will accept. So the fake
    /// niri here asserts the exact line `niri msg` writes, and answers with the
    /// exact reply niri writes back.
    #[test]
    fn the_event_stream_handshake_is_framed_the_way_niri_speaks_it() {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");

        // A fake niri: read the request line, check it, reply, then push one
        // event and go quiet (which is what a real niri does most of the time).
        let niri = std::thread::spawn(move || {
            let mut reader = BufReader::new(theirs.try_clone().expect("clone"));
            let mut request = String::new();
            reader.read_line(&mut request).expect("a request line");
            assert_eq!(
                request, "\"EventStream\"\n",
                "these are the bytes `niri msg event-stream` puts on the socket"
            );
            let mut writer = &theirs;
            writer
                .write_all(b"{\"Ok\":\"Handled\"}\n")
                .expect("the reply niri writes");
            writer
                .write_all(b"{\"WindowClosed\":{\"id\":7}}\n")
                .expect("one event");
            // Hold the socket open: a dropped write half would look like niri
            // exiting, and the point of the second half of this test is the
            // *idle* stream.
            std::thread::sleep(Duration::from_secs(2));
        });

        let mut events = SocketEvents::over(ours).expect("the handshake completes");
        let first = events.next_event();
        assert!(
            matches!(&first, Incoming::Event(e) if matches!(**e, Event::WindowClosed { id: 7 })),
            "niri's own bytes decode into niri's own event: {first:?}"
        );

        // …and an idle stream hands control back rather than parking forever.
        // Measured on another thread so a read with no timeout fails this test
        // instead of hanging it.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let idle = events.next_event();
            let _ = tx.send(matches!(idle, Incoming::Idle));
        });
        let idled = rx.recv_timeout(POLL_INTERVAL * 8).expect(
            "an idle event stream must time out, not park — the whole shutdown path \
                     hangs off this tick",
        );
        assert!(idled, "a timed-out read is an idle tick, not a dead stream");

        niri.join().expect("the fake niri thread");
    }

    // ── three delete-green framing mechanisms (#1053, #1038 review LOW-10) ───
    //
    // The review found these survived deletion with the suite still green:
    // the handshake timeout, the write half-close, and the truncated-tail
    // guard. One test each, each red when its mechanism is removed.

    /// The handshake timeout (`watch.rs`'s [`HANDSHAKE_TIMEOUT`]): a niri that
    /// accepts the connection and then never answers `EventStream` is exactly
    /// the "wedged, not busy" case the timeout exists for. Without it,
    /// [`SocketEvents::over`] would park in `read_line_before_deadline`
    /// forever — the same thread leak HIGH-2 fixed for a dead *session*, but
    /// for a dead *dial*.
    ///
    /// Run over a channel with a bounded `recv_timeout` rather than a bare
    /// call: if the timeout is deleted, `over` really does hang, and this
    /// must fail the one test that says so instead of hanging the whole
    /// binary.
    ///
    /// Bounded by **literals**, not by `HANDSHAKE_TIMEOUT` itself (#1053
    /// review MED-1): every bound here used to scale with the constant under
    /// test, so widening it from 5 s to 60 s stayed green — the suite just got
    /// 11× slower — and a wedged-niri window quietly growing from five seconds
    /// to an hour would have shipped the same way. The assertion below states
    /// the budget this module actually promises; the `recv_timeout` and
    /// `elapsed` bound state what a wedged handshake may cost a caller.
    #[test]
    fn a_niri_that_never_answers_the_handshake_times_out() {
        assert!(
            HANDSHAKE_TIMEOUT <= Duration::from_secs(10),
            "a wedged niri must not hold a dial for {HANDSHAKE_TIMEOUT:?} — if this \
             constant grew on purpose, widen the literal budget below to match"
        );

        let (ours, theirs) = UnixStream::pair().expect("socketpair");

        // Hold the peer open well past the deadline (rather than dropping it
        // immediately) so a missing timeout would see a live, silent socket —
        // not an EOF standing in for one.
        std::thread::spawn(move || {
            std::thread::sleep(HANDSHAKE_TIMEOUT * 4);
            drop(theirs);
        });

        let (tx, rx) = std::sync::mpsc::channel();
        // Deliberately not joined: if the mechanism is gone this thread parks
        // forever, and it must not take the test binary down with it.
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let result = SocketEvents::over(ours);
            let _ = tx.send((result.is_err(), started.elapsed()));
        });

        let (failed, elapsed) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the handshake must give up within HANDSHAKE_TIMEOUT, not park forever");

        assert!(failed, "no reply ever comes, so the handshake must fail");
        assert!(
            elapsed < Duration::from_secs(10),
            "gave up after {elapsed:?}, past the 10 s literal budget"
        );
    }

    /// A niri that never answers at all is the easy case for
    /// [`HANDSHAKE_TIMEOUT`] — `SO_RCVTIMEO` alone catches it. A niri that
    /// dribbles the reply in slower than one byte per timeout window is the
    /// hard case (#1053 review LOW-3): each individual read succeeds, just
    /// barely, so `SO_RCVTIMEO` never fires, and only a *total* deadline
    /// across the whole reply — [`read_line_before_deadline`] — can end this
    /// within the budget [`HANDSHAKE_TIMEOUT`]'s own doc promises.
    #[test]
    fn a_dribbling_niri_still_gives_up_within_the_handshake_timeout() {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");

        // Deliberately not joined: this thread runs on its own clock (one
        // byte per 400 ms) and outlives the assertions below by design; it
        // exits on its own once the reader side goes away (a broken-pipe
        // write error breaks the loop).
        std::thread::spawn(move || {
            let mut reader = BufReader::new(theirs.try_clone().expect("clone"));
            let mut request = String::new();
            reader.read_line(&mut request).expect("a request line");
            let mut writer = &theirs;
            // One byte every 400 ms — comfortably under HANDSHAKE_TIMEOUT's
            // own per-read window, so no individual read ever times out on
            // its own. Only a *total* deadline can end this early.
            for &b in b"{\"Ok\":\"Handled\"}\n" {
                std::thread::sleep(Duration::from_millis(400));
                if writer.write_all(&[b]).is_err() {
                    break;
                }
            }
        });

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let result = SocketEvents::over(ours);
            let _ = tx.send((result.is_err(), started.elapsed()));
        });

        // A literal margin over the constant, not a multiple of it (#1053
        // review MED-1's lesson applies here too): 18 bytes at 400 ms apart is
        // 7.2 s if the dribble is never cut off, safely past HANDSHAKE_TIMEOUT
        // (5 s) plus this margin, so a regression that removed the deadline
        // would either miss this bound or — worse — let the handshake
        // succeed late instead of failing.
        let budget = HANDSHAKE_TIMEOUT + Duration::from_secs(5);
        let (failed, elapsed) = rx.recv_timeout(budget).expect(
            "a dribbling niri must not keep the handshake parked past HANDSHAKE_TIMEOUT \
             plus a margin",
        );

        assert!(
            failed,
            "a reply that dribbles in slower than one byte per timeout window must not \
             be treated as a live connection, even though no single read ever times out"
        );
        assert!(
            elapsed < budget,
            "gave up after {elapsed:?}, past the {budget:?} budget"
        );
    }

    /// The write half-close after the handshake: [`SocketEvents`] never writes
    /// to niri again once it starts streaming, and shutting down the write
    /// side lets niri reap that half of the connection rather than holding it
    /// open forever expecting more requests.
    #[test]
    fn the_handshake_half_closes_the_write_side() {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");

        let (tx, rx) = std::sync::mpsc::channel();
        let niri = std::thread::spawn(move || {
            // Bounded, so a missing half-close times out here instead of
            // blocking this thread (and the test) forever.
            theirs
                .set_read_timeout(Some(POLL_INTERVAL * 4))
                .expect("arm a bounded read");
            let mut request = String::new();
            {
                let mut reader = BufReader::new(&theirs);
                reader.read_line(&mut request).expect("a request line");
            }
            let mut writer = &theirs;
            writer
                .write_all(b"{\"Ok\":\"Handled\"}\n")
                .expect("the reply");

            // A real half-close delivers EOF (`Ok(0)`) here; a missing one
            // blocks until the bounded timeout above fires and errors instead.
            let mut buf = [0_u8; 8];
            let outcome: std::io::Result<bool> = (&theirs).read(&mut buf).map(|n| n == 0);
            let _ = tx.send(outcome);
        });

        // Held alive for the whole test: dropping it would close `ours`
        // outright, which would deliver the peer its own EOF regardless of
        // whether the half-close under test ever ran.
        let _events = SocketEvents::over(ours).expect("the handshake completes");

        let saw_eof = rx
            .recv_timeout(POLL_INTERVAL * 8)
            .expect("the peer thread must report back")
            .expect("the read must not itself error out (a missing half-close times out)");
        assert!(
            saw_eof,
            "the write half must be closed so niri sees EOF, not silence"
        );

        niri.join().expect("the fake niri thread");
    }

    /// The truncated-tail guard: a stream that ends mid-line — a complete,
    /// individually-valid JSON value, but with no trailing newline because the
    /// connection died right after — must not be decoded as a real event. The
    /// framing contract is one JSON value per *line*; without the guard,
    /// `serde_json` happily parses the bytes anyway, since a JSON object needs
    /// no trailing separator to be complete.
    #[test]
    fn a_stream_that_ends_mid_line_is_not_parsed_as_an_event() {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");

        let niri = std::thread::spawn(move || {
            let mut reader = BufReader::new(theirs.try_clone().expect("clone"));
            let mut request = String::new();
            reader.read_line(&mut request).expect("a request line");
            let mut writer = &theirs;
            writer
                .write_all(b"{\"Ok\":\"Handled\"}\n")
                .expect("the reply");
            // A complete JSON value, but no trailing newline — then the
            // connection closes (both `theirs` and its clone drop here).
            writer
                .write_all(b"{\"WindowClosed\":{\"id\":7}}")
                .expect("the body, no newline");
        });

        let mut events = SocketEvents::over(ours).expect("the handshake completes");
        let mut ticks = 0;
        let outcome = loop {
            match events.next_event() {
                Incoming::Idle => {
                    ticks += 1;
                    assert!(ticks < 20, "the tail never arrived");
                }
                other => break other,
            }
        };

        assert!(
            matches!(outcome, Incoming::Ended(_)),
            "a stream that ends before its trailing newline must not be read \
             as a completed event, even though the bytes alone would parse: \
             {outcome:?}"
        );

        niri.join().expect("the fake niri thread");
    }

    /// `MAX_LINE` (#1053 review LOW-2): a niri that streams bytes with no
    /// `\n` at all must not let `SocketEvents::line` grow without bound —
    /// and, the part that actually matters, must not keep `next_event` from
    /// ever handing control back to [`stream_once`]'s liveness check. That is
    /// HIGH-2's leak shape again: a watcher that cannot notice its session
    /// ended, this time because the *stream* never goes idle rather than
    /// because it goes silent.
    ///
    /// Driven through [`stream_once`] rather than [`SocketEvents`] directly:
    /// before this cap, a continuous flood would keep a single `next_event`
    /// call from ever returning, so `stream_once`'s `alive()` check — which
    /// only runs *between* calls to `next_event` — would never get another
    /// turn, and a session that ended mid-flood would go unnoticed for as
    /// long as niri kept streaming (i.e. forever, for a real compositor).
    /// With the cap, `next_event` is guaranteed to return within one
    /// `MAX_LINE`'s worth of data, hanging `stream_once` back control —
    /// leaving [`drive`]'s own backoff `wait()` (already covered by
    /// `a_dropped_verdict_receiver_stops_the_watcher`) to pick up an ended
    /// session on its very next check, rather than never.
    #[test]
    fn an_unterminated_flood_hands_control_back_so_a_dropped_session_is_noticed() {
        /// A [`Backend`] whose one connection is this flooding source. `alive`
        /// only has to answer `true` once — for the check `stream_once` makes
        /// before its first (and, given the flood, only) call to `next_event`
        /// — since nothing calls it again until that call returns.
        struct FloodOnce(Option<SocketEvents>);
        impl Backend for FloodOnce {
            type Source = SocketEvents;
            fn connect(&mut self) -> Result<Self::Source, String> {
                self.0
                    .take()
                    .ok_or_else(|| "connect called twice".to_owned())
            }
            fn emit(&mut self, _verdict: Verdict) -> bool {
                true
            }
            fn alive(&self) -> bool {
                true
            }
            fn wait(&mut self, _delay: Duration) -> bool {
                true
            }
            fn log(&mut self, _line: &str) {}
        }

        let (ours, theirs) = UnixStream::pair().expect("socketpair");

        let keep_flooding = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flood_flag = std::sync::Arc::clone(&keep_flooding);
        let niri = std::thread::spawn(move || {
            let mut reader = BufReader::new(theirs.try_clone().expect("clone"));
            let mut request = String::new();
            reader.read_line(&mut request).expect("a request line");
            let mut writer = &theirs;
            writer
                .write_all(b"{\"Ok\":\"Handled\"}\n")
                .expect("the reply");
            // No '\n', ever: exactly the shape a cap has to bound. Stops once
            // the client side drops (a broken-pipe write error) or is told to.
            let chunk = vec![b'x'; 64 * 1024];
            while flood_flag.load(std::sync::atomic::Ordering::Relaxed) {
                if writer.write_all(&chunk).is_err() {
                    break;
                }
            }
        });

        let source = SocketEvents::over(ours).expect("the handshake completes");
        let mut backend = FloodOnce(Some(source));
        let mut watch = Watch::default();

        let (tx, rx) = std::sync::mpsc::channel();
        // Deliberately not joined: if the cap regresses, this thread hangs
        // forever inside a `next_event` reading a stream that never ends, and
        // it must not take the test binary down with it.
        std::thread::spawn(move || {
            let _ = tx.send(stream_once(&mut watch, &mut backend));
        });

        let stopped = rx.recv_timeout(Duration::from_secs(10)).expect(
            "an unterminated flood must not keep stream_once from ever handing control \
             back to the liveness check — the cap has to end the stream",
        );
        assert!(
            matches!(stopped, Stopped::StreamEnded(_)),
            "the cap ends the connection rather than growing it without bound: {stopped:?}"
        );

        keep_flooding.store(false, std::sync::atomic::Ordering::Relaxed);
        niri.join().expect("the fake niri thread");
    }
}
