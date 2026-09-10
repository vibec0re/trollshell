//! "Does the focused workspace hold more than one window?" — tracked in this
//! process, off a second niri connection.
//!
//! Annika's third ask on #1019 (2026-09-10) is "Only show when more than 1
//! window in workspace". The host has **no niri state topic** to subscribe to:
//! [`StateKey`](hytte_plugin::proto::StateKey) covers `Clock`, `SlotVisible`,
//! `Accent`, `AudioSpectrum` and friends, and nothing about windows or
//! workspaces. So the plugin answers the question itself, the same way it
//! already answers "which columns are on the focused workspace" — by talking to
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
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How many windows the focused workspace needs before the chip appears.
///
/// "more than 1" (#1019) — so two.
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

/// How long to wait for niri's reply to the `EventStream` request.
///
/// The handshake is one round trip on a socket that just accepted us, so a
/// timeout here means niri is wedged, not busy: give up and let [`backoff`]
/// schedule the retry rather than parking a thread on it forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

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

/// The compositor state the show/hide verdict is a function of, plus the last
/// verdict emitted.
///
/// Starts **empty**, which reads as hidden: a plugin that has not heard from
/// niri yet must not flash a chip it may be about to take away. That matches
/// [`crate::plugin::NiriLayouts`]'s own initial model, so the first frame the
/// host ever renders and this struct agree without either being told.
#[derive(Clone, Debug, Default)]
pub(crate) struct Watch {
    /// Window id → the workspace niri says it is on (`None` when niri reports
    /// none). Every window, tiled or floating — see the module docs.
    windows: HashMap<u64, Option<u64>>,
    /// The single focused workspace, or `None` before the first snapshot.
    focused: Option<u64>,
    /// Whether the opening `WorkspacesChanged` of this connection has landed.
    seen_workspaces: bool,
    /// Whether the opening `WindowsChanged` of this connection has landed.
    seen_windows: bool,
    /// The last verdict [`Watch::observe`] handed out. `false` because the chip
    /// starts hidden.
    emitted: bool,
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
    pub(crate) fn observe(&mut self, event: Event) -> Option<bool> {
        self.apply(event);
        if !self.snapshot_complete() {
            return None;
        }
        let verdict = self.visible();
        (verdict != self.emitted).then(|| {
            self.emitted = verdict;
            verdict
        })
    }

    /// Whether this connection has delivered both halves of its opening
    /// snapshot; see [`Watch::observe`].
    fn snapshot_complete(&self) -> bool {
        self.seen_workspaces && self.seen_windows
    }

    /// The verdict as it stands, whether or not it just changed.
    pub(crate) fn visible(&self) -> bool {
        self.window_count() >= MIN_WINDOWS
    }

    /// Windows on the focused workspace. `0` while no workspace is focused —
    /// which is also what niri reports on a fresh session before the first
    /// `WorkspacesChanged`.
    pub(crate) fn window_count(&self) -> usize {
        let Some(focused) = self.focused else {
            return 0;
        };
        self.windows
            .values()
            .filter(|workspace| **workspace == Some(focused))
            .count()
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
        self.focused = None;
        self.seen_workspaces = false;
        self.seen_windows = false;
    }

    /// The fold itself. Five of niri 26.4's twenty-odd events move this state;
    /// the rest (keyboard layouts, casts, screenshots, urgency, focus, overview)
    /// cannot change how many windows sit on the focused workspace.
    fn apply(&mut self, event: Event) {
        match event {
            // The full workspace list — this is where the focused workspace
            // comes from on connect, and after any workspace add/remove/move.
            Event::WorkspacesChanged { workspaces } => {
                self.focused = workspaces.iter().find(|w| w.is_focused).map(|w| w.id);
                self.seen_workspaces = true;
            }
            // A workspace became *active on its output*, which is not the same
            // as focused: niri's own docs say so, and sets `focused` only when
            // it is. Switching workspaces on a second monitor must not move the
            // chip's answer.
            Event::WorkspaceActivated { id, focused } => {
                if focused {
                    self.focused = Some(id);
                }
            }
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
    fn send(&mut self, visible: bool) -> bool;
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
    fn emit(&mut self, visible: bool) -> bool;
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
                        "[{prefix}] WARNING: cannot watch niri for the focused workspace's \
                         window count ({why}) — the layout chip stays hidden until this \
                         succeeds"
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

    fn emit(&mut self, visible: bool) -> bool {
        self.verdicts.send(visible)
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
    /// The line being assembled. **Not cleared between polls**: a timed-out
    /// `read_line` keeps whatever bytes had already arrived appended here, and
    /// dropping them would corrupt the next event rather than lose an idle tick.
    line: String,
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

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("no reply to the EventStream request: {e}"))?;
        let reply: Reply = serde_json::from_str(&line)
            .map_err(|e| format!("cannot decode niri's reply {line:?}: {e}"))?;
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
            line: String::new(),
        })
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
    fn next_event(&mut self) -> Incoming {
        match self.reader.read_line(&mut self.line) {
            Ok(0) => Incoming::Ended("niri closed the event stream".to_owned()),
            Ok(_) => {
                if !self.line.ends_with('\n') {
                    // `read_line` only returns `Ok` without the delimiter at end
                    // of stream, so this is a truncated final event.
                    return Incoming::Ended("niri closed the event stream mid-event".to_owned());
                }
                let line = std::mem::take(&mut self.line);
                match serde_json::from_str::<Event>(&line) {
                    Ok(event) => Incoming::Event(Box::new(event)),
                    Err(e) => Incoming::Ended(format!("cannot decode a niri event: {e}")),
                }
            }
            Err(e) if timed_out(&e) || e.kind() == io::ErrorKind::Interrupted => Incoming::Idle,
            Err(e) => Incoming::Ended(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BACKOFF_CEILING, Backend, EventSource, Incoming, MIN_WINDOWS, POLL_INTERVAL, SocketEvents,
        Watch, backoff, drive,
    };
    use niri_ipc::{Event, Window, WindowLayout, Workspace};
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn workspace(id: u64, is_focused: bool) -> Workspace {
        Workspace {
            id,
            idx: 1,
            name: None,
            output: Some("DP-1".to_owned()),
            is_urgent: false,
            is_active: true,
            is_focused,
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

    fn workspaces_changed(focused: u64) -> Event {
        Event::WorkspacesChanged {
            workspaces: vec![workspace(focused, true), workspace(focused + 100, false)],
        }
    }

    /// Fold a whole burst and collect every verdict it emitted.
    fn observe_all(watch: &mut Watch, events: Vec<Event>) -> Vec<bool> {
        events
            .into_iter()
            .filter_map(|event| watch.observe(event))
            .collect()
    }

    #[test]
    fn a_fresh_watch_is_hidden_and_says_nothing() {
        let watch = Watch::default();

        assert!(!watch.visible(), "the chip starts hidden");
        assert_eq!(watch.window_count(), 0, "nothing known about niri yet");
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
        assert_eq!(watch.window_count(), 1);

        let emitted = observe_all(
            &mut watch,
            vec![Event::WindowOpenedOrChanged {
                window: window(20, Some(1)),
            }],
        );
        assert_eq!(emitted, vec![true], "the second window reveals the chip");
        assert!(watch.visible());
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
        assert!(watch.visible(), "two windows: shown");

        let emitted = observe_all(&mut watch, vec![Event::WindowClosed { id: 20 }]);

        assert_eq!(emitted, vec![false]);
        assert!(!watch.visible());
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

        assert_eq!(emitted, vec![true], "one column, two windows, chip shown");
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

        assert_eq!(emitted, vec![true]);
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

        assert!(emitted.is_empty(), "the focused workspace holds one window");
        assert_eq!(watch.window_count(), 1);
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
        assert!(!watch.visible(), "workspace 1 holds one window");

        let emitted = observe_all(
            &mut watch,
            vec![Event::WorkspaceActivated {
                id: 2,
                focused: true,
            }],
        );

        assert_eq!(emitted, vec![true], "workspace 2 holds two");
    }

    /// `WorkspaceActivated { focused: false }` is "active on *its* output", not
    /// "focused" — niri says so in the event's own docs. A second monitor
    /// changing workspace must not move this chip.
    #[test]
    fn an_activation_that_is_not_a_focus_change_is_ignored() {
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

        let emitted = observe_all(
            &mut watch,
            vec![Event::WorkspaceActivated {
                id: 2,
                focused: false,
            }],
        );

        assert!(emitted.is_empty(), "the focused workspace did not change");
        assert_eq!(watch.window_count(), 1, "still workspace 1's one window");
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
        assert!(watch.visible());
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
        assert_eq!(watch.window_count(), 1);
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
        assert!(watch.visible());

        let emitted = observe_all(
            &mut watch,
            vec![Event::WindowOpenedOrChanged {
                window: window(20, Some(2)),
            }],
        );

        assert_eq!(emitted, vec![false], "it left, so we are back to one");
        assert_eq!(watch.window_count(), 1, "and it is not double-counted");
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

        assert_eq!(emitted, vec![false], "the old two are gone, not kept");
        assert_eq!(watch.window_count(), 1);
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
        assert!(watch.visible());

        watch.forget_compositor_state();
        assert_eq!(watch.window_count(), 0, "niri's ids are meaningless now");

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
        assert_eq!(emitted, vec![false]);
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
            assert!(watch.visible(), "precondition: the chip is up");
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
            vec![false],
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
        emitted: Vec<bool>,
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

        fn emit(&mut self, visible: bool) -> bool {
            self.emitted.push(visible);
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
    /// — no window on the new focused workspace — and emits `false` a
    /// millisecond before `WindowsChanged` emits `true` again. The `Watch`-level
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
            vec![true],
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
            vec![true],
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
}
