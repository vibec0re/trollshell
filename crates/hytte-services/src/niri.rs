//! Niri compositor IPC client.
//!
//! Uses the synchronous `niri_ipc::socket::Socket` from a dedicated
//! `spawn_blocking` task on the tokio runtime. Niri's IPC is line-based
//! JSON over a unix socket; the `niri-ipc` crate handles the framing and
//! event deserialisation.
//!
//! On connection loss the loop sleeps 1s then reconnects.
//!
//! # API notes (niri-ipc 26.4.0)
//!
//! - `Socket::send()` returns `io::Result<Reply>`.
//! - After sending `Request::EventStream`, call `socket.read_events()` which
//!   returns `impl FnMut() -> io::Result<Event>` that blocks until the next
//!   event arrives.
//! - Commands open a fresh short-lived socket (cheap unix-socket connect)
//!   so they don't have to share the long-lived event-stream socket.

use crate::retry;
use anyhow::{Context, Result, anyhow};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry, runtime, spawn_supervised_blocking};
use niri_ipc::{Action, Event, Reply, Request, Response, WorkspaceReferenceArg, socket::Socket};
use std::thread;
use std::time::{Duration, Instant};

// Re-export the niri-ipc data types consumers need so trollshell etc.
// don't have to depend on niri-ipc directly.
pub use niri_ipc::{Cast, CastKind, CastTarget, Window, WindowLayout, Workspace};

/// The niri IPC service handle.
pub struct NiriService;

/// A completed niri screenshot capture (`Event::ScreenshotCaptured`).
///
/// `path` mirrors the event's own field: `Some(path)` when niri wrote the
/// screenshot to disk, `None` when it was only copied to the clipboard (or
/// the path wasn't valid UTF-8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedShot {
    pub path: Option<String>,
}

/// Internal handles holding the reactive state.
#[doc(hidden)]
pub struct NiriHandles {
    pub(crate) workspaces: Mutable<Vec<Workspace>>,
    pub(crate) windows: Mutable<Vec<Window>>,
    pub(crate) focused_window: Mutable<Option<Window>>,
    pub(crate) casts: Mutable<Vec<Cast>>,
    pub(crate) screenshot_captured: Mutable<Option<CapturedShot>>,
}

impl Default for NiriHandles {
    fn default() -> Self {
        Self {
            workspaces: Mutable::new(Vec::new()),
            windows: Mutable::new(Vec::new()),
            focused_window: Mutable::new(None),
            casts: Mutable::new(Vec::new()),
            screenshot_captured: Mutable::new(None),
        }
    }
}

impl Service for NiriService {
    type Handles = NiriHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NiriHandles::default();
        let ws_writer = handles.workspaces.clone();
        let win_list_writer = handles.windows.clone();
        let win_focus_writer = handles.focused_window.clone();
        let casts_writer = handles.casts.clone();
        let screenshot_writer = handles.screenshot_captured.clone();

        // Supervised, not a bare `spawn_blocking` (#654). The reconnect loop
        // below only covers the `Err` arm: a *panic* anywhere under
        // `listen_once` — `apply_event` parses compositor-supplied data — kills
        // the thread and takes the retry with it, freezing `workspaces`,
        // `windows`, `focused_window`, `casts` and `screenshot_captured` for
        // the rest of the session with nothing to restart them. The loop is
        // restart-safe: every run reconnects from scratch and the compositor,
        // not this process, holds the state it republishes.
        spawn_supervised_blocking("niri", move || {
            // Reconnect on `retry::RECONNECT_RETRY`'s ramp with the streak
            // latched, rather than a flat 1s with a `warn!` per attempt (#1170
            // item 4). The case that matters is a **missing** `NIRI_SOCKET`:
            // `Socket::connect` fails instantly, so the old loop was 1 Hz of
            // identical warnings, forever, on a condition nothing here can fix.
            let mut reporter = retry::ReconnectReporter::new();
            loop {
                let started = Instant::now();
                let outcome = listen_once(
                    &ws_writer,
                    &win_list_writer,
                    &win_focus_writer,
                    &casts_writer,
                    &screenshot_writer,
                );
                let (_report, delay) =
                    reconnect_after(&mut reporter, started.elapsed(), outcome.as_ref());
                thread::sleep(delay);
            }
        });

        handles
    }
}

/// Record a finished `listen_once` and say how long to wait before redialling.
///
/// The cadence is [`retry::ReconnectReporter`]'s — a run that stayed up at least
/// its reset threshold is healthy, anything shorter is a failure streak — and
/// the wording is this call site's, per `retry`'s mechanism/judgement split.
/// The shape is #668/#669's: warn once on the edge into failure, `debug!` while
/// nothing changes, `info!` to retract it.
///
/// **A run's health, not its `Result`, decides.** A `NIRI_SOCKET` that is not
/// there fails instantly with an `Err`; a niri that restarts mid-session gives
/// an `Err` too but only after hours; and an event stream that closes cleanly
/// after hours is an `Ok`. Only the elapsed time separates "we are stuck" from
/// "that was one hiccup", which is also exactly what the ramp resets on — so
/// the two can never disagree.
///
/// Returns the report alongside the delay so the tests can read the cadence
/// without a subscriber; the loop only needs the delay.
fn reconnect_after(
    reporter: &mut retry::ReconnectReporter,
    ran_for: Duration,
    outcome: Result<&(), &anyhow::Error>,
) -> (retry::Report, Duration) {
    let (report, delay) = reporter.record(ran_for);
    let retry_in_secs = delay.as_secs_f64();
    let cause = match outcome {
        Ok(()) => "the event stream closed".to_owned(),
        Err(e) => format!("{e:#}"),
    };
    match report {
        retry::Report::Opened => tracing::warn!(
            cause,
            retry_in_secs,
            "niri: cannot hold an IPC event stream (is NIRI_SOCKET set and is niri running?). \
             Workspaces, windows, casts and the frame overlay are stale until it comes back. \
             Redialling with backoff; this line will not repeat until it does"
        ),
        retry::Report::Repeating => {
            tracing::debug!(cause, retry_in_secs, "niri: still no IPC event stream");
        }
        retry::Report::Recovered => {
            tracing::info!("niri: IPC event stream held again; workspaces and windows are live");
        }
        // A stream that ran healthily and then ended — a niri restart, say.
        // Worth a line each time: nothing is outstanding for the latch to
        // retract, and these are rare by construction.
        retry::Report::Quiet => tracing::warn!(
            cause,
            retry_in_secs,
            "niri: IPC event stream ended, reconnecting"
        ),
    }
    (report, delay)
}

fn listen_once(
    workspaces: &Mutable<Vec<Workspace>>,
    windows: &Mutable<Vec<Window>>,
    focused_window: &Mutable<Option<Window>>,
    casts: &Mutable<Vec<Cast>>,
    screenshot_captured: &Mutable<Option<CapturedShot>>,
) -> Result<()> {
    let mut socket = Socket::connect().context("connect to NIRI_SOCKET")?;

    let reply = socket
        .send(Request::EventStream)
        .context("send EventStream request")?;

    match reply {
        Ok(Response::Handled) => {}
        Ok(other) => return Err(anyhow!("unexpected EventStream reply: {other:?}")),
        Err(msg) => return Err(anyhow!("niri returned error for EventStream: {msg}")),
    }

    let mut read_event = socket.read_events();

    loop {
        let event = read_event().map_err(|e| anyhow!("read niri event: {e}"))?;
        apply_event(
            event,
            workspaces,
            windows,
            focused_window,
            casts,
            screenshot_captured,
        );
    }
}

fn apply_event(
    event: Event,
    workspaces: &Mutable<Vec<Workspace>>,
    windows: &Mutable<Vec<Window>>,
    focused_window: &Mutable<Option<Window>>,
    casts: &Mutable<Vec<Cast>>,
    screenshot_captured: &Mutable<Option<CapturedShot>>,
) {
    match event {
        Event::WorkspacesChanged { workspaces: ws } => {
            workspaces.set(ws);
        }
        Event::WorkspaceActivated { id, focused } => {
            let mut ws_lock = workspaces.lock_mut();
            // Resolve the activated workspace's output so we know which
            // monitor's previously-active workspace to deactivate.
            let output = ws_lock
                .iter()
                .find(|w| w.id == id)
                .and_then(|w| w.output.clone());
            for w in ws_lock.iter_mut() {
                if w.id == id {
                    w.is_active = true;
                    if focused {
                        w.is_focused = true;
                    }
                } else {
                    if w.output == output {
                        w.is_active = false;
                    }
                    if focused {
                        w.is_focused = false;
                    }
                }
            }
        }
        Event::WindowsChanged { windows: list } => {
            let focused = list.iter().find(|w| w.is_focused).cloned();
            focused_window.set(focused);
            windows.set(list);
        }
        Event::WindowOpenedOrChanged { window } => {
            if window.is_focused {
                focused_window.set(Some(window.clone()));
            }
            let mut list = windows.lock_mut();
            // If the incoming window claims focus, clear it on every
            // other entry so the cache has a single source of truth.
            if window.is_focused {
                for w in list.iter_mut() {
                    if w.id != window.id {
                        w.is_focused = false;
                    }
                }
            }
            if let Some(existing) = list.iter_mut().find(|w| w.id == window.id) {
                *existing = window;
            } else {
                list.push(window);
            }
        }
        Event::WindowClosed { id } => {
            windows.lock_mut().retain(|w| w.id != id);
            let currently_focused = focused_window.lock_ref();
            if currently_focused.as_ref().map(|w| w.id) == Some(id) {
                drop(currently_focused);
                focused_window.set(None);
            }
        }
        Event::WindowFocusChanged { id } => {
            // Mirror is_focused into the windows list so per-window
            // subscribers (window-list widget) see the change too.
            let mut list = windows.lock_mut();
            for w in list.iter_mut() {
                w.is_focused = Some(w.id) == id;
            }
            let new_focused = id.and_then(|id| list.iter().find(|w| w.id == id).cloned());
            drop(list);
            focused_window.set(new_focused);
        }
        // Fullscreen / maximize-to-edges / resize-to-edges land here, not in
        // WindowsChanged or WindowOpenedOrChanged. Without this arm the cached
        // tile_size stays stale and `edge_window_on` (which the frame uses to
        // hide itself) never flips.
        Event::WindowLayoutsChanged { changes } => {
            let mut list = windows.lock_mut();
            for (id, layout) in &changes {
                if let Some(w) = list.iter_mut().find(|w| w.id == *id) {
                    w.layout = layout.clone();
                }
            }
            let focused_id = focused_window.lock_ref().as_ref().map(|w| w.id);
            if let Some(fid) = focused_id
                && let Some(updated) = list.iter().find(|w| w.id == fid).cloned()
            {
                drop(list);
                focused_window.set(Some(updated));
            }
        }
        // Screencast session state. Mirrors niri's own `CastsState::apply`
        // (niri-ipc 26.4.0 `state.rs`): `CastsChanged` is a full replace,
        // `CastStartedOrChanged` upserts by `stream_id`, and `CastStopped`
        // removes by `stream_id`. Dropping the `CastStopped` arm would leave
        // the privacy indicator stuck on forever once a stream ends without
        // a fresh full `CastsChanged` — it's the one arm that's
        // non-negotiable here.
        Event::CastsChanged { casts: list } => {
            casts.set(list);
        }
        Event::CastStartedOrChanged { cast } => {
            let mut list = casts.lock_mut();
            if let Some(existing) = list.iter_mut().find(|c| c.stream_id == cast.stream_id) {
                *existing = cast;
            } else {
                list.push(cast);
            }
        }
        Event::CastStopped { stream_id } => {
            casts.lock_mut().retain(|c| c.stream_id != stream_id);
        }
        // Fired once niri's own screenshot UI (opened via `screenshot()`)
        // completes a capture. `path` is `Some` when niri wrote the image to
        // disk, `None` when it only went to the clipboard. Every emission is
        // a fresh capture, so this always `set`s rather than merging.
        Event::ScreenshotCaptured { path } => {
            screenshot_captured.set(Some(CapturedShot { path }));
        }
        _ => {}
    }
}

/// Returns the niri service to register with the hytte runtime.
#[must_use]
pub fn service() -> NiriService {
    NiriService
}

/// Signal of the current niri workspaces.
pub fn workspaces() -> impl Signal<Item = Vec<Workspace>> {
    registry::with(|r| {
        r.get::<NiriHandles>()
            .expect("niri::service() not registered")
            .workspaces
            .signal_cloned()
    })
}

/// Connector name of the currently focused monitor (e.g. `"DP-1"`).
/// Derived from [`workspaces()`] by finding the workspace whose
/// `is_focused == true` and reading its `output`. `None` when no
/// workspace is focused or the focused workspace has no output (rare
/// during reconnect / niri startup).
pub fn focused_output() -> impl Signal<Item = Option<String>> {
    use futures_signals::signal::SignalExt;
    workspaces().map(|ws| {
        ws.iter()
            .find(|w| w.is_focused)
            .and_then(|w| w.output.clone())
    })
}

/// Signal of the current niri windows.
pub fn windows() -> impl Signal<Item = Vec<Window>> {
    registry::with(|r| {
        r.get::<NiriHandles>()
            .expect("niri::service() not registered")
            .windows
            .signal_cloned()
    })
}

/// Signal of the currently focused window, if any.
pub fn focused_window() -> impl Signal<Item = Option<Window>> {
    registry::with(|r| {
        r.get::<NiriHandles>()
            .expect("niri::service() not registered")
            .focused_window
            .signal_cloned()
    })
}

/// Signal of the currently active niri screencast sessions.
///
/// Non-empty whenever niri reports at least one live cast session — this is
/// the "session exists" gate (the safe default for a privacy affordance),
/// not "actively streaming frames": a paused cast (`Cast::is_active ==
/// false`, e.g. an OBS scene switch) still counts, since the compositor is
/// still capturing on the consumer's behalf.
pub fn active_casts() -> impl Signal<Item = Vec<Cast>> {
    registry::with(|r| {
        r.get::<NiriHandles>()
            .expect("niri::service() not registered")
            .casts
            .signal_cloned()
    })
}

/// Signal of the most recent completed screenshot capture, if any.
///
/// `None` until the first capture of this process's lifetime; thereafter
/// `Some` and updated on every subsequent `Event::ScreenshotCaptured` (never
/// reset back to `None` between captures — subscribers should treat every
/// emission of `Some` as "a fresh capture just happened", not react to level.
pub fn screenshot_captured() -> impl Signal<Item = Option<CapturedShot>> {
    registry::with(|r| {
        r.get::<NiriHandles>()
            .expect("niri::service() not registered")
            .screenshot_captured
            .signal_cloned()
    })
}

/// Tolerance (logical pixels) when comparing a window's tile width to a
/// monitor's logical width to detect an edge-spanning window. niri
/// reports sizes in logical pixels; a few pixels of slack cover
/// fractional-scale rounding (e.g. at 1.25× scale, logical sizes are
/// non-integer).
const EDGE_TOL: f64 = 4.0;

/// Signal: `true` when the active workspace on `connector` contains a
/// window whose tile width spans the full output (within [`EDGE_TOL`]).
/// That covers fullscreen, niri's `MaximizeWindowToEdges`, and any
/// floating window manually sized to `mon_w` — every state where a
/// window touches the L/R edges of the output.
///
/// Useful for overlays that paint along those edges (e.g. the trollshell
/// frame): they must hide when an edge-spanning window is active, since
/// every pixel of their gradient or border would overlap the window.
///
/// `mon_w` is a *signal* of the output's logical width, not a snapshot: a
/// resolution/mode switch (kanshi profile change) resizes the output without a
/// monitor connect/disconnect, so a captured width would leave the edge-span
/// threshold stale (#442). Feed it a live width — e.g.
/// `monitor.size_changed().map(|(w, _)| f64::from(w))` — so the detection
/// re-evaluates when the mode changes.
pub fn edge_window_on(
    connector: String,
    mon_w: impl Signal<Item = f64> + 'static,
) -> impl Signal<Item = bool> {
    use futures_signals::map_ref;
    let workspaces = workspaces();
    let windows = windows();
    map_ref! {
        let ws = workspaces,
        let w = windows,
        let mw = mon_w =>
        has_edge_window(ws, w, &connector, *mw)
    }
}

/// Pure predicate behind [`edge_window_on`]. Returns `true` when the
/// active workspace on `connector` contains any window whose tile width
/// is within [`EDGE_TOL`] of `mon_w`.
///
/// Width alone suffices: niri's maximize-to-edges always covers the
/// full available width AND height (no horizontal-only maximize state),
/// fullscreen does the same, and an edge-stretched floating window is
/// treated identically — also the correct visual response. The `>=`
/// comparison is robust against fractional-scale rounding: tile width
/// can never *exceed* `mon_w` in practice.
fn has_edge_window(
    workspaces: &[Workspace],
    windows: &[Window],
    connector: &str,
    mon_w: f64,
) -> bool {
    let active_id = workspaces
        .iter()
        .find(|ws| ws.output.as_deref() == Some(connector) && ws.is_active)
        .map(|ws| ws.id);
    active_id.is_some_and(|id| {
        windows
            .iter()
            .any(|w| w.workspace_id == Some(id) && w.layout.tile_size.0 >= mon_w - EDGE_TOL)
    })
}

/// Signal: `true` when the active (visible) workspace on `connector` contains
/// a window that is **fullscreen** — its tile spans the full output in *both*
/// dimensions (within [`EDGE_TOL`]).
///
/// Distinct from [`edge_window_on`], which is width-only and therefore also
/// fires for niri's `MaximizeWindowToEdges` and edge-stretched floating
/// windows. Fullscreen additionally covers the full *height* — including the
/// area the bar's exclusive zone reserves (a niri fullscreen window ignores
/// layer-shell exclusive zones and covers the whole output), whereas a
/// maximize-to-edges window stops at the bar and so is ~one bar-height short.
/// That height check is what tells the two apart. This is the signal the
/// fullscreen idle-inhibitor rides (#404): "don't dim/lock/suspend while a
/// movie/game/presentation is genuinely fullscreen", *not* while an ordinary
/// window happens to be maximized.
///
/// Scoped to the **active** workspace on `connector` (the one currently
/// visible on that output), so a fullscreen window scrolled off onto a
/// non-active workspace doesn't count. A disabled output never has a live
/// `Monitor` to feed this, so it's excluded structurally by the caller.
///
/// `mon_size` is a *signal* of the output's logical `(width, height)`, not a
/// snapshot — feed it a live source (e.g.
/// `monitor.size_changed().map(|(w, h)| (f64::from(w), f64::from(h)))`) so a
/// resolution/mode switch (kanshi profile change, #442) re-evaluates the
/// threshold rather than leaving it stale.
pub fn fullscreen_window_on(
    connector: String,
    mon_size: impl Signal<Item = (f64, f64)> + 'static,
) -> impl Signal<Item = bool> {
    use futures_signals::map_ref;
    let workspaces = workspaces();
    let windows = windows();
    map_ref! {
        let ws = workspaces,
        let w = windows,
        let sz = mon_size =>
        has_fullscreen_window(ws, w, &connector, sz.0, sz.1)
    }
}

/// Pure predicate behind [`fullscreen_window_on`]. Returns `true` when the
/// active workspace on `connector` contains any window whose tile spans the
/// full output in **both** width and height (each within [`EDGE_TOL`]).
///
/// The height term is the discriminator against maximize-to-edges: a
/// fullscreen tile is `(mon_w, mon_h)`; a maximize-to-edges tile is
/// `(mon_w, mon_h - bar_zone)` — short by the bar's exclusive zone (tens of
/// logical px, far larger than [`EDGE_TOL`]), so it fails the height check.
/// The `>=` comparisons are robust against fractional-scale rounding: a tile
/// can never *exceed* the output size in practice.
fn has_fullscreen_window(
    workspaces: &[Workspace],
    windows: &[Window],
    connector: &str,
    mon_w: f64,
    mon_h: f64,
) -> bool {
    let active_id = workspaces
        .iter()
        .find(|ws| ws.output.as_deref() == Some(connector) && ws.is_active)
        .map(|ws| ws.id);
    active_id.is_some_and(|id| {
        windows.iter().any(|w| {
            w.workspace_id == Some(id)
                && w.layout.tile_size.0 >= mon_w - EDGE_TOL
                && w.layout.tile_size.1 >= mon_h - EDGE_TOL
        })
    })
}

/// Focus the workspace with the given id (fire-and-forget).
pub fn focus_workspace(id: u64) {
    send_action(Action::FocusWorkspace {
        reference: WorkspaceReferenceArg::Id(id),
    });
}

/// Focus the window with the given id (fire-and-forget).
pub fn focus_window(id: u64) {
    send_action(Action::FocusWindow { id });
}

/// Open niri's own interactive screenshot UI (region/window selection is
/// niri's UI, not trollshell's) — fire-and-forget.
///
/// `show_pointer: true` matches niri's own CLI default. `path: None` lets
/// niri save according to its configured `screenshot-path` rather than
/// trollshell dictating a location. The eventual capture (or cancellation —
/// niri simply never emits the event) surfaces via
/// [`Event::ScreenshotCaptured`] → [`screenshot_captured()`].
pub fn screenshot() {
    send_action(Action::Screenshot {
        show_pointer: true,
        path: None,
    });
}

/// Stop the screencast session `session_id` (fire-and-forget).
///
/// Backs the screencast privacy chip's click (#221/#578). niri stops *every*
/// stream belonging to the session, so a session that fanned out into
/// multiple [`Cast`]s needs only one call — callers iterating
/// [`active_casts()`] should dedup by `Cast::session_id` rather than send one
/// request per cast.
///
/// **Only `CastKind::PipeWire` casts can be stopped this way.** niri's IPC has
/// no equivalent for `CastKind::WlrScreencopy` (wf-recorder,
/// xdg-desktop-portal-wlr); calling this for such a session is a no-op on
/// niri's side, so filter on `kind` before offering the affordance.
pub fn stop_cast(session_id: u64) {
    send_action(Action::StopCast { session_id });
}

/// Ask niri to exit the session (fire-and-forget).
///
/// `skip_confirmation = false` lets niri's built-in confirmation overlay
/// fire, which is the right UX when this is invoked from a power menu
/// where the menu itself is the only confirmation. Pass `true` if the
/// caller has already confirmed externally.
pub fn quit(skip_confirmation: bool) {
    send_action(Action::Quit { skip_confirmation });
}

fn send_action(action: Action) {
    runtime::handle().spawn_blocking(move || match Socket::connect() {
        Ok(mut sock) => {
            if let Err(e) = sock.send(Request::Action(action)) {
                tracing::warn!(error = %e, "niri action send failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "niri socket open for action failed"),
    });
}

// ── Batched, id-addressed actions (#1071 §3.4) ───────────────────────────────

/// One step of a niri batch, addressed by **id**.
///
/// A deliberately narrower vocabulary than [`niri_ipc::Action`]. Almost every
/// id-addressed action in niri-ipc 26.4 spells its target `Option<u64>`, where
/// `None` means *the focused one* — `CloseWindow { id: None }` closes whatever
/// happens to be focused, `SetWorkspaceName { workspace: None }` names whatever
/// workspace happens to be active. #1071's claim check found that a Start built
/// out of those is a race against the user's own focus, so the target is
/// **mandatory here**: the wrong spelling is unrepresentable rather than a
/// comment asking the next caller not to write it.
///
/// It also keeps `niri-ipc` out of the shell binary, the same way the crate
/// graph keeps `gtk` out — the binary names `WorkspaceAction`, never `Action`.
///
/// Lowered to real niri actions by [`lower`], which is the one place a target
/// could be dropped and is unit-tested for exactly that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceAction {
    /// Give workspace `workspace` the name `name`.
    ///
    /// **niri silently does nothing if the name is already taken** (measured on
    /// 26.4; `find_workspace_by_name` matches case-insensitively and the action
    /// returns `Handled` either way). So a caller must read the workspace list
    /// back and verify rather than assume — see [`query_workspaces`].
    SetName { workspace: u64, name: String },
    /// Remove workspace `workspace`'s name, freeing it for the next
    /// [`Self::SetName`] (#1071 §3.4's housekeeping).
    UnsetName { workspace: u64 },
    /// Focus workspace `workspace`. New windows open on the focused workspace,
    /// so a launch batch leads with this.
    Focus { workspace: u64 },
    /// Move window `window` to workspace `workspace`, taking focus with it.
    MoveWindow { window: u64, workspace: u64 },
    /// Close window `window`.
    CloseWindow { window: u64 },
    /// Focus window `window` — and, with it, the column that window is in.
    ///
    /// niri-ipc 26.4 spells this one's target `id: u64` rather than
    /// `Option<u64>` (`lib.rs:322`), so it is the only action here whose
    /// mandatory target is niri's own.
    FocusWindow { window: u64 },
    /// Move the **focused** column to `index` (1-based) on its workspace.
    ///
    /// # No id form, and what stands in for one
    ///
    /// `Action::MoveColumnToIndex { index: usize }` (niri-ipc 26.4,
    /// `lib.rs:402`) takes no target at all: niri offers no
    /// "move *that* column" spelling, only "move the focused one". That is
    /// exactly the "whatever is focused right now" shape the rest of this enum
    /// exists to make unrepresentable — so the target is supplied by the action
    /// **immediately before it in the same batch**, a [`Self::FocusWindow`],
    /// and a batch is one socket in order ([`send_actions`]), which is what
    /// makes "immediately before" mean anything. [`Self::MoveColumnToFirst`]
    /// and [`Self::MoveColumnToLast`] share this exact shape.
    ///
    /// Never emit one on its own. The single producer in the tree is
    /// `trollshell`'s `workspace_stacks::column_order_batch`, and
    /// `every_move_column_is_addressed_by_the_focus_before_it` pins that every
    /// one it emits is preceded by its own `FocusWindow`.
    MoveColumnToIndex { index: usize },
    /// Move the **focused** column to the start of its workspace — niri's own
    /// `Action::MoveColumnToFirst` (niri-ipc 26.4, `lib.rs:394`).
    ///
    /// Same no-id shape as [`Self::MoveColumnToIndex`]: a preceding
    /// [`Self::FocusWindow`] in the same batch is the target. The single
    /// producer is [`reflow_batch`] (#1129) — moving a column to an end and
    /// back is a change niri reflows the workspace for, unlike a change in
    /// the space *around* the columns (closing the sidebar, an
    /// out-of-process layout applying `SetWindowWidth`), which niri does not
    /// reflow for.
    MoveColumnToFirst,
    /// Move the **focused** column to the end of its workspace — niri's own
    /// `Action::MoveColumnToLast` (niri-ipc 26.4, `lib.rs:396`). Same shape
    /// and same producer as [`Self::MoveColumnToFirst`].
    MoveColumnToLast,
    /// Move workspace `workspace` to `index` (1-based) **on its own monitor**
    /// (#1071 §3.6).
    ///
    /// niri's index is per output, not global: a workspace is placed among the
    /// workspaces of the monitor it is on.
    MoveWorkspaceToIndex { workspace: u64, index: usize },
    /// Move workspace `workspace` to the output `output` (#1071 §5).
    MoveWorkspaceToMonitor { workspace: u64, output: String },
}

/// [`WorkspaceAction`] as the `niri-ipc` action it sends.
///
/// The one seam where a mandatory id becomes niri's optional one, so it is the
/// one place that could reintroduce "whatever is focused" — hence pure, and
/// pinned by [`tests::every_action_names_its_target`].
fn lower(action: WorkspaceAction) -> Action {
    match action {
        WorkspaceAction::SetName { workspace, name } => Action::SetWorkspaceName {
            name,
            workspace: Some(WorkspaceReferenceArg::Id(workspace)),
        },
        WorkspaceAction::UnsetName { workspace } => Action::UnsetWorkspaceName {
            reference: Some(WorkspaceReferenceArg::Id(workspace)),
        },
        WorkspaceAction::Focus { workspace } => Action::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(workspace),
        },
        WorkspaceAction::MoveWindow { window, workspace } => Action::MoveWindowToWorkspace {
            window_id: Some(window),
            reference: WorkspaceReferenceArg::Id(workspace),
            focus: true,
        },
        WorkspaceAction::CloseWindow { window } => Action::CloseWindow { id: Some(window) },
        WorkspaceAction::FocusWindow { window } => Action::FocusWindow { id: window },
        // No target to drop: niri has none to give. See the variant's doc.
        WorkspaceAction::MoveColumnToIndex { index } => Action::MoveColumnToIndex { index },
        WorkspaceAction::MoveColumnToFirst => Action::MoveColumnToFirst {},
        WorkspaceAction::MoveColumnToLast => Action::MoveColumnToLast {},
        WorkspaceAction::MoveWorkspaceToIndex { workspace, index } => {
            Action::MoveWorkspaceToIndex {
                index,
                reference: Some(WorkspaceReferenceArg::Id(workspace)),
            }
        }
        WorkspaceAction::MoveWorkspaceToMonitor { workspace, output } => {
            Action::MoveWorkspaceToMonitor {
                output,
                reference: Some(WorkspaceReferenceArg::Id(workspace)),
            }
        }
    }
}

/// One niri connection, reusable for many request/reply round trips.
///
/// `Socket::send` already takes `&mut self` and niri answers one reply per
/// non-`EventStream` request, so a connection is a sequence — which is exactly
/// what a batch needs and what [`send_action`]'s connect-per-action shape
/// cannot give.
trait Conn {
    fn send(&mut self, request: Request) -> Result<Reply, String>;
}

/// Opens [`Conn`]s.
///
/// The seam that makes a batch testable without a socket — and, more to the
/// point, the seam that lets a test **count connections**, which is the only way
/// to assert #1071 §3.4's "one socket, in order" as a property rather than as a
/// comment. A `Conn`-only seam could not tell a batch from five separate sends.
trait Connector {
    fn connect(&mut self) -> Result<Box<dyn Conn>, String>;
}

struct SocketConn(Socket);

impl Conn for SocketConn {
    fn send(&mut self, request: Request) -> Result<Reply, String> {
        self.0.send(request).map_err(|e| format!("niri ipc: {e}"))
    }
}

struct SocketConnector;

impl Connector for SocketConnector {
    fn connect(&mut self) -> Result<Box<dyn Conn>, String> {
        Socket::connect()
            .map(|s| Box::new(SocketConn(s)) as Box<dyn Conn>)
            .map_err(|e| format!("cannot reach niri over $NIRI_SOCKET: {e}"))
    }
}

/// Send `actions` over **one** connection, in order, checking every reply.
///
/// Pure over the [`Connector`] seam. Three properties, each of which
/// [`send_action`] lacks and #1071 §3.4 needs:
///
/// 1. **One connection.** `connect` is called at most once, so the actions
///    cannot interleave with another caller's the way five independent
///    `spawn_blocking` connects can.
/// 2. **In order, fail-fast.** A refused action stops the batch; the remaining
///    actions are not sent. A Start that could not name its workspace must not
///    go on to launch apps onto someone else's.
/// 3. **Every reply checked** — including niri's *inner* refusal
///    ([`Reply`] is `Result<Response, String>`), which `send_action` drops on
///    the floor.
///
/// An empty batch connects nothing and succeeds.
fn send_actions_over(
    connector: &mut impl Connector,
    actions: Vec<WorkspaceAction>,
) -> Result<(), String> {
    if actions.is_empty() {
        return Ok(());
    }
    let mut conn = connector.connect()?;
    for action in actions {
        let described = format!("{action:?}");
        match conn.send(Request::Action(lower(action)))? {
            Ok(_) => {}
            Err(refusal) => return Err(format!("niri refused {described}: {refusal}")),
        }
    }
    Ok(())
}

/// One query over a fresh connection, with both reply layers checked.
fn query_over<T>(
    connector: &mut impl Connector,
    request: Request,
    extract: impl FnOnce(Response) -> Option<T>,
) -> Result<T, String> {
    let described = format!("{request:?}");
    let mut conn = connector.connect()?;
    match conn.send(request)? {
        Ok(response) => extract(response).ok_or_else(|| format!("unexpected reply to {described}")),
        Err(refusal) => Err(format!("niri refused {described}: {refusal}")),
    }
}

/// Send `actions` to niri in order, over one socket, checking every reply
/// (#1071 §3.4).
///
/// Runs the blocking socket work on the tokio runtime's blocking pool and
/// `await`s it, so the caller learns whether the batch landed — unlike
/// [`focus_workspace`] and its fire-and-forget siblings, which cannot.
///
/// # Errors
/// The socket could not be opened, a reply could not be read, or niri refused
/// one of the actions (naming which).
pub async fn send_actions(actions: Vec<WorkspaceAction>) -> Result<(), String> {
    runtime::handle()
        .spawn_blocking(move || send_actions_over(&mut SocketConnector, actions))
        .await
        .map_err(|e| format!("niri batch task failed: {e}"))?
}

/// niri's workspace list, read **directly** rather than off [`workspaces()`].
///
/// The read-back #1071 §3.4 verifies a `SetName` with. The signal is fed by the
/// event stream, so reading it would mean waiting for a `WorkspacesChanged` to
/// arrive and could not distinguish "the name did not land" from "the event has
/// not arrived yet"; a direct query answers as of now. It is also reachable from
/// a tokio task, which the thread-local registry the signal lives in is not.
///
/// # Errors
/// As [`send_actions`], plus a reply that was not a workspace list.
pub async fn query_workspaces() -> Result<Vec<Workspace>, String> {
    runtime::handle()
        .spawn_blocking(|| {
            query_over(&mut SocketConnector, Request::Workspaces, |r| match r {
                Response::Workspaces(w) => Some(w),
                _ => None,
            })
        })
        .await
        .map_err(|e| format!("niri workspaces task failed: {e}"))?
}

/// niri's window list, read directly — see [`query_workspaces`] for why a
/// transaction running on the runtime cannot use the signal.
///
/// # Errors
/// As [`query_workspaces`].
pub async fn query_windows() -> Result<Vec<Window>, String> {
    runtime::handle()
        .spawn_blocking(|| {
            query_over(&mut SocketConnector, Request::Windows, |r| match r {
                Response::Windows(w) => Some(w),
                _ => None,
            })
        })
        .await
        .map_err(|e| format!("niri windows task failed: {e}"))?
}

// ── Post-close/post-layout reflow nudge (#1129) ──────────────────────────────

/// The id of the workspace currently active on `connector`, or `None` when no
/// workspace claims that output — an unknown connector (a fallback,
/// pointer-keyed monitor key; a screen that has gone away), or the brief
/// window between reconnect and the first `WorkspacesChanged`.
///
/// A **snapshot** of the cached [`workspaces()`], not a signal: callers that
/// need "the answer right now" from a synchronous, non-reactive context (e.g.
/// `overlays::sidebar`'s settle callback, which runs off a GTK frame-cadence
/// timer with nothing to hang a subscription off of) read this instead of
/// setting up a subscription they would have to tear down again immediately.
#[must_use]
pub fn active_workspace_id(connector: &str) -> Option<u64> {
    registry::with(|r| {
        active_workspace_id_in(
            connector,
            &r.get::<NiriHandles>()
                .expect("niri::service() not registered")
                .workspaces
                .lock_ref(),
        )
    })
}

/// Pure predicate behind [`active_workspace_id`] — same split as
/// [`has_edge_window`]/[`has_fullscreen_window`], so it's unit-testable
/// without a registered [`NiriHandles`].
fn active_workspace_id_in(connector: &str, workspaces: &[Workspace]) -> Option<u64> {
    workspaces
        .iter()
        .find(|w| w.output.as_deref() == Some(connector) && w.is_active)
        .map(|w| w.id)
}

/// Pure batch builder behind [`reflow_workspace`] (#1129).
///
/// **The bug:** niri lays out columns when something *about* them changes,
/// not when the space *around* them does. Closing the sidebar commits
/// `exclusive_zone = 0` and niri stops reserving the strip; the
/// `hytte-plugin-niri-layouts` `apply` sends `SetWindowWidth` per column.
/// Neither is a change niri reflows the workspace for, so a column that was
/// sitting flush against the old reserved edge is left exactly where it was —
/// partly off screen once the edge moves (or the widths change) out from
/// under it.
///
/// **The nudge:** moving a column to one end of the workspace and back **is**
/// a change niri reflows for, and starting and ending at the first index
/// leaves the column order exactly as it was. Column order in `windows` is
/// the same key `trollshell`'s `workspace_stacks::column_order_batch` sorts
/// by — 1-based `(column, row)` — with floating windows
/// (`pos_in_scrolling_layout == None`) excluded: a floating window is in no
/// column, so there is nothing to reflow it into. The **first** tiled window
/// in that order is the nudge's target; if `workspace` has none (empty, or
/// floating-only), the batch is empty and [`reflow_workspace`] sends nothing.
///
/// The final [`WorkspaceAction::FocusWindow`] restores whatever window
/// `windows` reports as focused (`Window::is_focused`, checked across every
/// workspace, not just this one — the nudge's own `FocusWindow` on the first
/// column can steal focus from a window on a workspace other than the one
/// being reflowed) so the user doesn't land on column 1. If nothing is
/// focused, the batch ends after the reflow with no restore step.
#[must_use]
pub(crate) fn reflow_batch(workspace: u64, windows: &[Window]) -> Vec<WorkspaceAction> {
    let mut here: Vec<&Window> = windows
        .iter()
        .filter(|w| w.workspace_id == Some(workspace) && w.layout.pos_in_scrolling_layout.is_some())
        .collect();
    here.sort_by_key(|w| {
        (
            w.layout
                .pos_in_scrolling_layout
                .expect("filtered to Some above"),
            w.id,
        )
    });
    let Some(first) = here.first() else {
        // No tiled column on this workspace — nothing to reflow, and no
        // spurious FocusWindow to steal focus for no reason.
        return Vec::new();
    };

    let mut batch = vec![
        WorkspaceAction::FocusWindow { window: first.id },
        WorkspaceAction::MoveColumnToFirst,
        WorkspaceAction::MoveColumnToLast,
        WorkspaceAction::MoveColumnToFirst,
    ];
    if let Some(focused) = windows.iter().find(|w| w.is_focused) {
        batch.push(WorkspaceAction::FocusWindow { window: focused.id });
    }
    batch
}

/// Nudge niri to reflow workspace `workspace`'s columns (#1129): fire and
/// forget, mirroring [`focus_workspace`]/[`focus_window`].
///
/// Reads the current window list out of the registry's cached [`windows()`]
/// (so, like every other free-function accessor here, must run on the GTK
/// main thread the registry lives on), builds [`reflow_batch`], and — when
/// it's non-empty — sends it over one [`send_actions`] connection on the
/// tokio runtime. Callers (`overlays::sidebar`'s post-close settle,
/// `hytte-plugin-niri-layouts`'s own mirrored chain over its own
/// `Transport`) don't need to await anything back; a refused or unreachable
/// niri just leaves the columns exactly where the triggering action left
/// them, which is the pre-#1129 behaviour.
pub fn reflow_workspace(workspace: u64) {
    let windows = registry::with(|r| {
        r.get::<NiriHandles>()
            .expect("niri::service() not registered")
            .windows
            .get_cloned()
    });
    let batch = reflow_batch(workspace, &windows);
    if batch.is_empty() {
        return;
    }
    runtime::handle().spawn(async move {
        if let Err(e) = send_actions(batch).await {
            tracing::warn!(error = %e, workspace, "niri reflow batch failed (#1129)");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── The reconnect ramp and its latched warn (#1170 item 4) ───────────────

    /// A failed dial, the shape `Socket::connect` gives when `NIRI_SOCKET` is
    /// unset or nothing is listening on it.
    fn missing_socket() -> anyhow::Error {
        anyhow!("connect to NIRI_SOCKET: No such file or directory (os error 2)")
    }

    /// #1170's item 4, stated exactly: five consecutive missing-socket attempts
    /// are worth one loud line.
    ///
    /// The ramp is asserted alongside it, because the two are the same decision
    /// in `ReconnectReporter`: a cadence that spoke on every attempt would have
    /// to treat every attempt as a fresh incident, and reset the delay too.
    ///
    /// Falsify by dropping the reporter and going back to
    /// `warn!(…); thread::sleep(Duration::from_secs(1))`: five `Opened`s and
    /// five identical 1s delays (measured).
    #[test]
    fn five_missing_socket_attempts_log_one_warn_and_climb_one_ramp() {
        let mut reporter = retry::ReconnectReporter::new();
        let err = missing_socket();
        let instant = Duration::from_millis(1);

        let turns: Vec<(retry::Report, Duration)> = (0..5)
            .map(|_| reconnect_after(&mut reporter, instant, Err(&err)))
            .collect();

        let loud = turns
            .iter()
            .filter(|(r, _)| *r == retry::Report::Opened)
            .count();
        assert_eq!(
            loud, 1,
            "a missing NIRI_SOCKET warns per attempt, forever: {turns:?}"
        );
        assert_eq!(
            turns[0].0,
            retry::Report::Opened,
            "the outage is not reported at all until later; the first attempt must be the loud one"
        );

        let delays: Vec<Duration> = turns.iter().map(|(_, d)| *d).collect();
        assert!(
            delays[0] > Duration::ZERO,
            "the first redial is immediate, i.e. a hot loop"
        );
        for pair in delays.windows(2) {
            assert!(
                pair[1] > pair[0],
                "the ramp is flat across a streak: {delays:?}"
            );
        }
    }

    /// The other half: the streak is *one* incident, so the ramp does not reset
    /// mid-outage — which is the same fact as the warn not repeating, since
    /// both read `ReconnectReporter`'s one notion of a healthy run.
    #[test]
    fn a_held_stream_resets_the_ramp_and_a_short_one_does_not() {
        let mut reporter = retry::ReconnectReporter::new();
        let err = missing_socket();
        let instant = Duration::from_millis(1);

        let (_, first) = reconnect_after(&mut reporter, instant, Err(&err));
        for _ in 0..4 {
            reconnect_after(&mut reporter, instant, Err(&err));
        }
        // A stream that held for the reset threshold prices its own redial at
        // the bottom of the ramp again (#806's ordering) *and* retracts the
        // warning — one fact, two consequences.
        let (report, after_healthy) =
            reconnect_after(&mut reporter, Duration::from_mins(1), Ok(&()));
        assert_eq!(
            after_healthy, first,
            "a long-lived stream's own reconnect still paid the streak's ratcheted delay"
        );
        assert_eq!(
            report,
            retry::Report::Recovered,
            "the outage was never retracted, so a journal shows every death and no recovery"
        );
    }

    const MON_W: f64 = 1920.0;
    const MON_H: f64 = 1080.0;
    const BAR_H: f64 = 44.0;
    const CONNECTOR: &str = "DP-1";

    fn mk_workspace(id: u64, output: &str, is_active: bool) -> Workspace {
        Workspace {
            id,
            idx: 1,
            name: None,
            output: Some(output.to_string()),
            is_urgent: false,
            is_active,
            is_focused: is_active,
            active_window_id: None,
        }
    }

    fn mk_window(id: u64, workspace_id: u64, tile: (f64, f64)) -> Window {
        Window {
            id,
            title: None,
            app_id: None,
            pid: None,
            workspace_id: Some(workspace_id),
            is_focused: false,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: Some((1, 1)),
                tile_size: tile,
                // window_size isn't read by has_edge_window; arbitrary stub.
                window_size: (0, 0),
                tile_pos_in_workspace_view: Some((0.0, 0.0)),
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    #[test]
    fn has_edge_window_normal_tiled() {
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        let w = vec![mk_window(10, 1, (MON_W - 16.0, MON_H - BAR_H - 8.0))];
        assert!(!has_edge_window(&ws, &w, CONNECTOR, MON_W));
    }

    #[test]
    fn has_edge_window_fullscreen() {
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        let w = vec![mk_window(10, 1, (MON_W, MON_H))];
        assert!(has_edge_window(&ws, &w, CONNECTOR, MON_W));
    }

    #[test]
    fn has_edge_window_other_workspace_ignored() {
        let ws = vec![
            mk_workspace(1, CONNECTOR, true),
            mk_workspace(2, CONNECTOR, false),
        ];
        let w = vec![
            mk_window(10, 1, (MON_W - 16.0, MON_H - BAR_H - 8.0)),
            mk_window(20, 2, (MON_W, MON_H)),
        ];
        assert!(!has_edge_window(&ws, &w, CONNECTOR, MON_W));
    }

    #[test]
    fn has_edge_window_other_output_ignored() {
        let ws = vec![mk_workspace(1, "HDMI-A-1", true)];
        let w = vec![mk_window(10, 1, (MON_W, MON_H))];
        assert!(!has_edge_window(&ws, &w, CONNECTOR, MON_W));
    }

    #[test]
    fn has_edge_window_no_active_workspace() {
        let ws = vec![mk_workspace(1, CONNECTOR, false)];
        let w = vec![mk_window(10, 1, (MON_W, MON_H))];
        assert!(!has_edge_window(&ws, &w, CONNECTOR, MON_W));
    }

    #[test]
    fn has_edge_window_maximize_to_edges() {
        // niri's MaximizeWindowToEdges: window covers full output width
        // AND full height-minus-bar (bar's exclusive zone still applies).
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        let w = vec![mk_window(10, 1, (MON_W, MON_H - BAR_H))];
        assert!(has_edge_window(&ws, &w, CONNECTOR, MON_W));
    }

    #[test]
    fn has_edge_window_within_tolerance() {
        // Fractional-scale rounding can put tile width a hair under
        // mon_w. EDGE_TOL = 4.0, so mon_w - 2.0 should still trigger.
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        let w = vec![mk_window(10, 1, (MON_W - 2.0, MON_H - BAR_H))];
        assert!(has_edge_window(&ws, &w, CONNECTOR, MON_W));
    }

    #[test]
    fn has_fullscreen_window_fullscreen() {
        // A genuinely fullscreen window: tile spans the whole output in both
        // dimensions (covering the bar's exclusive zone too).
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        let w = vec![mk_window(10, 1, (MON_W, MON_H))];
        assert!(has_fullscreen_window(&ws, &w, CONNECTOR, MON_W, MON_H));
    }

    #[test]
    fn has_fullscreen_window_maximize_to_edges_is_not_fullscreen() {
        // The key discriminator vs. `has_edge_window`: maximize-to-edges spans
        // the full WIDTH but stops one bar-height short of full HEIGHT (the bar
        // keeps its exclusive zone), so it is NOT treated as fullscreen — an
        // ordinary maximized window must not pin the idle inhibitor.
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        let w = vec![mk_window(10, 1, (MON_W, MON_H - BAR_H))];
        assert!(has_edge_window(&ws, &w, CONNECTOR, MON_W)); // width-only: yes
        assert!(!has_fullscreen_window(&ws, &w, CONNECTOR, MON_W, MON_H)); // both: no
    }

    #[test]
    fn has_fullscreen_window_normal_tiled_false() {
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        let w = vec![mk_window(10, 1, (MON_W - 200.0, MON_H - BAR_H - 8.0))];
        assert!(!has_fullscreen_window(&ws, &w, CONNECTOR, MON_W, MON_H));
    }

    #[test]
    fn has_fullscreen_window_within_tolerance() {
        // Fractional-scale rounding can put the tile a hair under the output
        // size in either dimension; EDGE_TOL (4.0) slack keeps it fullscreen.
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        let w = vec![mk_window(10, 1, (MON_W - 2.0, MON_H - 2.0))];
        assert!(has_fullscreen_window(&ws, &w, CONNECTOR, MON_W, MON_H));
    }

    #[test]
    fn has_fullscreen_window_other_workspace_ignored() {
        // Fullscreen window sits on an inactive (scrolled-off) workspace — not
        // visible, so it must not count.
        let ws = vec![
            mk_workspace(1, CONNECTOR, true),
            mk_workspace(2, CONNECTOR, false),
        ];
        let w = vec![
            mk_window(10, 1, (MON_W - 200.0, MON_H - BAR_H - 8.0)),
            mk_window(20, 2, (MON_W, MON_H)),
        ];
        assert!(!has_fullscreen_window(&ws, &w, CONNECTOR, MON_W, MON_H));
    }

    #[test]
    fn has_fullscreen_window_other_output_ignored() {
        let ws = vec![mk_workspace(1, "HDMI-A-1", true)];
        let w = vec![mk_window(10, 1, (MON_W, MON_H))];
        assert!(!has_fullscreen_window(&ws, &w, CONNECTOR, MON_W, MON_H));
    }

    #[test]
    fn has_fullscreen_window_no_active_workspace() {
        let ws = vec![mk_workspace(1, CONNECTOR, false)];
        let w = vec![mk_window(10, 1, (MON_W, MON_H))];
        assert!(!has_fullscreen_window(&ws, &w, CONNECTOR, MON_W, MON_H));
    }

    fn mk_layout(tile: (f64, f64)) -> WindowLayout {
        WindowLayout {
            pos_in_scrolling_layout: Some((1, 1)),
            tile_size: tile,
            window_size: (0, 0),
            tile_pos_in_workspace_view: Some((0.0, 0.0)),
            window_offset_in_tile: (0.0, 0.0),
        }
    }

    /// Regression: niri emits `WindowLayoutsChanged` (not `WindowsChanged`
    /// or `WindowOpenedOrChanged`) on fullscreen / maximize-to-edges
    /// toggles. The arm must update each cached window's `layout` so
    /// `has_edge_window` flips and the frame hides.
    #[test]
    fn window_layouts_changed_updates_tile_size() {
        let workspaces = Mutable::new(vec![mk_workspace(1, CONNECTOR, true)]);
        // Seed the cache with a tiled window — tile_size below MON_W.
        let small = mk_window(10, 1, (MON_W - 200.0, MON_H - BAR_H - 8.0));
        let windows = Mutable::new(vec![small]);
        let focused_window = Mutable::new(None);
        let casts = Mutable::new(Vec::new());
        let screenshot_captured = Mutable::new(None);

        // Pre-condition: not edge-spanning.
        assert!(!has_edge_window(
            &workspaces.lock_ref(),
            &windows.lock_ref(),
            CONNECTOR,
            MON_W,
        ));

        // Fullscreen toggle: layout grows to full output.
        apply_event(
            Event::WindowLayoutsChanged {
                changes: vec![(10, mk_layout((MON_W, MON_H)))],
            },
            &workspaces,
            &windows,
            &focused_window,
            &casts,
            &screenshot_captured,
        );

        assert!(has_edge_window(
            &workspaces.lock_ref(),
            &windows.lock_ref(),
            CONNECTOR,
            MON_W,
        ));
    }

    /// When the changed window is also the currently-focused one, the
    /// `focused_window` mirror must pick up the new layout too so any
    /// subscribers reading `focused_window` directly see fresh state.
    #[test]
    fn window_layouts_changed_mirrors_into_focused_window() {
        let workspaces = Mutable::new(vec![mk_workspace(1, CONNECTOR, true)]);
        let mut w = mk_window(10, 1, (MON_W - 200.0, MON_H - BAR_H - 8.0));
        w.is_focused = true;
        let windows = Mutable::new(vec![w.clone()]);
        let focused_window = Mutable::new(Some(w));
        let casts = Mutable::new(Vec::new());
        let screenshot_captured = Mutable::new(None);

        apply_event(
            Event::WindowLayoutsChanged {
                changes: vec![(10, mk_layout((MON_W, MON_H)))],
            },
            &workspaces,
            &windows,
            &focused_window,
            &casts,
            &screenshot_captured,
        );

        let focused = focused_window.lock_ref().clone().expect("focused set");
        assert_eq!(focused.layout.tile_size, (MON_W, MON_H));
    }

    /// A layout change for a window the cache hasn't seen yet is a no-op
    /// (no insertion, no panic). Niri-side ordering guarantees the prior
    /// add event arrives first in practice, but the arm shouldn't trust
    /// that — the cache must remain consistent on stray ids.
    #[test]
    fn window_layouts_changed_ignores_unknown_id() {
        let workspaces = Mutable::new(vec![mk_workspace(1, CONNECTOR, true)]);
        let windows: Mutable<Vec<Window>> = Mutable::new(Vec::new());
        let focused_window = Mutable::new(None);
        let casts = Mutable::new(Vec::new());
        let screenshot_captured = Mutable::new(None);

        apply_event(
            Event::WindowLayoutsChanged {
                changes: vec![(999, mk_layout((MON_W, MON_H)))],
            },
            &workspaces,
            &windows,
            &focused_window,
            &casts,
            &screenshot_captured,
        );

        assert!(windows.lock_ref().is_empty());
        assert!(focused_window.lock_ref().is_none());
    }

    fn mk_cast(stream_id: u64, target: CastTarget) -> Cast {
        Cast {
            stream_id,
            session_id: stream_id,
            kind: CastKind::PipeWire,
            target,
            is_dynamic_target: false,
            is_active: true,
            pid: None,
            pw_node_id: None,
        }
    }

    /// `CastsChanged` is a full replace (mirrors niri's own
    /// `CastsState::apply`), not a merge.
    #[test]
    fn casts_changed_replaces_list() {
        let workspaces = Mutable::new(Vec::new());
        let windows = Mutable::new(Vec::new());
        let focused_window = Mutable::new(None);
        let casts = Mutable::new(vec![mk_cast(1, CastTarget::Nothing {})]);
        let screenshot_captured = Mutable::new(None);

        apply_event(
            Event::CastsChanged {
                casts: vec![mk_cast(
                    2,
                    CastTarget::Output {
                        name: "DP-1".into(),
                    },
                )],
            },
            &workspaces,
            &windows,
            &focused_window,
            &casts,
            &screenshot_captured,
        );

        let list = casts.lock_ref();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].stream_id, 2);
    }

    /// `CastStartedOrChanged` upserts by `stream_id`: a fresh id is
    /// appended, a known id is replaced in place.
    #[test]
    fn cast_started_or_changed_upserts_by_stream_id() {
        let workspaces = Mutable::new(Vec::new());
        let windows = Mutable::new(Vec::new());
        let focused_window = Mutable::new(None);
        let casts = Mutable::new(Vec::new());
        let screenshot_captured = Mutable::new(None);

        apply_event(
            Event::CastStartedOrChanged {
                cast: mk_cast(1, CastTarget::Nothing {}),
            },
            &workspaces,
            &windows,
            &focused_window,
            &casts,
            &screenshot_captured,
        );
        assert_eq!(casts.lock_ref().len(), 1);

        // Same stream_id, target resolved: replace in place, not append.
        apply_event(
            Event::CastStartedOrChanged {
                cast: mk_cast(
                    1,
                    CastTarget::Output {
                        name: "DP-1".into(),
                    },
                ),
            },
            &workspaces,
            &windows,
            &focused_window,
            &casts,
            &screenshot_captured,
        );

        let list = casts.lock_ref();
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].target,
            CastTarget::Output {
                name: "DP-1".into(),
            }
        );
    }

    /// `CastStopped` removes only the matching `stream_id`. This is the
    /// non-negotiable arm — without it a cast announced via
    /// `CastStartedOrChanged` and later torn down never leaves the list,
    /// so the privacy indicator would stay lit forever.
    #[test]
    fn cast_stopped_removes_by_stream_id() {
        let workspaces = Mutable::new(Vec::new());
        let windows = Mutable::new(Vec::new());
        let focused_window = Mutable::new(None);
        let casts = Mutable::new(vec![
            mk_cast(1, CastTarget::Nothing {}),
            mk_cast(2, CastTarget::Nothing {}),
        ]);
        let screenshot_captured = Mutable::new(None);

        apply_event(
            Event::CastStopped { stream_id: 1 },
            &workspaces,
            &windows,
            &focused_window,
            &casts,
            &screenshot_captured,
        );

        let list = casts.lock_ref();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].stream_id, 2);
    }

    // ── Batched, id-addressed actions (#1071 §3.4) ───────────────────────────

    /// A scripted niri: records every request, tagged with the **connection**
    /// it arrived on, and answers from a queued script.
    ///
    /// The connection tag is the point. #1071 §3.4's requirement is not "these
    /// actions were sent" but "these actions were sent *over one socket, in
    /// order*", and a fake that only saw requests could not tell the two apart.
    #[derive(Default)]
    struct Script {
        /// `(connection index, request)`, in arrival order.
        seen: Vec<(usize, Request)>,
        /// How many times a connection was opened.
        connects: usize,
        /// Replies, consumed in order. Exhausted → `Ok(Response::Handled)`.
        replies: std::collections::VecDeque<Reply>,
        /// When set, `connect` fails with it instead of opening anything.
        connect_error: Option<String>,
        /// When set, the first `send` fails at the transport layer with it.
        transport_error: Option<String>,
    }

    #[derive(Clone, Default)]
    struct Fake(std::rc::Rc<std::cell::RefCell<Script>>);

    struct FakeConn {
        script: std::rc::Rc<std::cell::RefCell<Script>>,
        index: usize,
    }

    impl Conn for FakeConn {
        fn send(&mut self, request: Request) -> Result<Reply, String> {
            let mut script = self.script.borrow_mut();
            script.seen.push((self.index, request));
            if let Some(err) = script.transport_error.take() {
                return Err(err);
            }
            Ok(script.replies.pop_front().unwrap_or(Ok(Response::Handled)))
        }
    }

    impl Connector for Fake {
        fn connect(&mut self) -> Result<Box<dyn Conn>, String> {
            let mut script = self.0.borrow_mut();
            if let Some(err) = script.connect_error.take() {
                return Err(err);
            }
            script.connects += 1;
            let index = script.connects - 1;
            drop(script);
            Ok(Box::new(FakeConn {
                script: self.0.clone(),
                index,
            }))
        }
    }

    impl Fake {
        fn connects(&self) -> usize {
            self.0.borrow().connects
        }

        /// The actions seen, in arrival order, paired with their connection and
        /// rendered as the **JSON niri actually receives**.
        ///
        /// `niri_ipc::Action` derives no `PartialEq`, so an assertion has to
        /// pick a representation — and the wire one is the right pick rather
        /// than a workaround: it is what niri parses, so a niri-ipc upgrade that
        /// renames a field or changes a tag reds here instead of compiling into
        /// a request niri silently ignores.
        fn seen(&self) -> Vec<(usize, String)> {
            self.0
                .borrow()
                .seen
                .iter()
                .filter_map(|(conn, request)| match request {
                    Request::Action(action) => Some((*conn, wire(action))),
                    _ => None,
                })
                .collect()
        }

        fn refuse_nth(&self, n: usize, message: &str) {
            let mut script = self.0.borrow_mut();
            for _ in 0..n {
                script.replies.push_back(Ok(Response::Handled));
            }
            script.replies.push_back(Err(message.to_owned()));
        }
    }

    /// One action as the JSON niri receives — see [`Fake::seen`] for why the
    /// assertions are stated on the wire rather than on the Rust value.
    fn wire(action: &Action) -> String {
        serde_json::to_string(action).expect("an Action serialises")
    }

    fn batch() -> Vec<WorkspaceAction> {
        vec![
            WorkspaceAction::SetName {
                workspace: 7,
                name: "chat".to_owned(),
            },
            WorkspaceAction::Focus { workspace: 7 },
            WorkspaceAction::MoveWindow {
                window: 42,
                workspace: 7,
            },
        ]
    }

    /// §3.4's first requirement, and the only one a request-only fake could not
    /// see: the whole batch rides **one** connection, in the order given.
    ///
    /// Falsified by connecting per action (`connects` becomes 3) — the
    /// mutation #1071 §7 names.
    #[test]
    fn a_batch_is_one_connection_in_order() {
        let mut fake = Fake::default();
        send_actions_over(&mut fake, batch()).expect("the batch lands");

        assert_eq!(fake.connects(), 1, "one socket for the whole batch");
        let seen = fake.seen();
        assert!(
            seen.iter().all(|(conn, _)| *conn == 0),
            "every action on the same connection: {seen:?}"
        );
        assert_eq!(
            seen.into_iter().map(|(_, a)| a).collect::<Vec<_>>(),
            vec![
                r#"{"SetWorkspaceName":{"name":"chat","workspace":{"Id":7}}}"#,
                r#"{"FocusWorkspace":{"reference":{"Id":7}}}"#,
                r#"{"MoveWindowToWorkspace":{"window_id":42,"reference":{"Id":7},"focus":true}}"#,
            ],
            "in the order given"
        );
    }

    /// niri's own refusal is the **inner** `Err` of a `Reply`, which
    /// `send_action` discards. A batch must stop there and say which action was
    /// refused — a Start that could not name its workspace must not go on to
    /// launch apps onto someone else's.
    #[test]
    fn a_refused_action_stops_the_batch_and_names_itself() {
        let mut fake = Fake::default();
        fake.refuse_nth(1, "no such workspace");
        let err = send_actions_over(&mut fake, batch()).expect_err("the refusal surfaces");

        assert!(err.contains("no such workspace"), "{err}");
        assert!(err.contains("Focus"), "names the refused action: {err}");
        assert_eq!(
            fake.seen().len(),
            2,
            "the third action was never sent: {:?}",
            fake.seen()
        );
    }

    /// A transport failure surfaces too, and also stops the batch.
    #[test]
    fn a_transport_failure_stops_the_batch() {
        let mut fake = Fake::default();
        fake.0.borrow_mut().transport_error = Some("socket went away".to_owned());
        let err = send_actions_over(&mut fake, batch()).expect_err("the failure surfaces");
        assert!(err.contains("socket went away"), "{err}");
        assert_eq!(fake.seen().len(), 1, "nothing after the failure");
    }

    /// An unreachable niri fails the batch before anything is sent.
    #[test]
    fn an_unopenable_socket_sends_nothing() {
        let mut fake = Fake::default();
        fake.0.borrow_mut().connect_error = Some("NIRI_SOCKET is not set".to_owned());
        let err = send_actions_over(&mut fake, batch()).expect_err("the failure surfaces");
        assert!(err.contains("NIRI_SOCKET"), "{err}");
        assert!(fake.seen().is_empty());
    }

    /// An empty batch opens no socket at all.
    #[test]
    fn an_empty_batch_connects_nothing() {
        let mut fake = Fake::default();
        send_actions_over(&mut fake, Vec::new()).expect("trivially lands");
        assert_eq!(fake.connects(), 0);
    }

    /// The lowering never spells a target `None`, pinned on the **wire**.
    ///
    /// `None` means *the focused one* in niri-ipc, and #1071 §3.4's whole point
    /// is that a Start addressed at "whatever is focused right now" is a race
    /// against the user. `WorkspaceAction` makes the target mandatory, so
    /// [`lower`] is the one place it could still be dropped — which is what
    /// this pins, including §7's named `CloseWindow { id: None }` mutation.
    ///
    /// Stated as the JSON niri receives rather than as a Rust value: `Action`
    /// derives no `PartialEq`, and the wire is the representation that actually
    /// decides what the compositor does. `{"CloseWindow":{"id":null}}` is a
    /// *different message* from `{"CloseWindow":{"id":9}}`, and that difference
    /// is exactly what the mutation introduces.
    #[test]
    fn every_action_names_its_target() {
        assert_eq!(
            wire(&lower(WorkspaceAction::SetName {
                workspace: 3,
                name: "dev".to_owned()
            })),
            r#"{"SetWorkspaceName":{"name":"dev","workspace":{"Id":3}}}"#
        );
        assert_eq!(
            wire(&lower(WorkspaceAction::UnsetName { workspace: 3 })),
            r#"{"UnsetWorkspaceName":{"reference":{"Id":3}}}"#
        );
        assert_eq!(
            wire(&lower(WorkspaceAction::Focus { workspace: 3 })),
            r#"{"FocusWorkspace":{"reference":{"Id":3}}}"#
        );
        assert_eq!(
            wire(&lower(WorkspaceAction::MoveWindow {
                window: 9,
                workspace: 3
            })),
            r#"{"MoveWindowToWorkspace":{"window_id":9,"reference":{"Id":3},"focus":true}}"#
        );
        // §7: `id: None` here would close the *focused* window instead of the
        // one the Stop plan named — and would go out as `"id":null`.
        assert_eq!(
            wire(&lower(WorkspaceAction::CloseWindow { window: 9 })),
            r#"{"CloseWindow":{"id":9}}"#
        );
        // #1071 phase 3. `FocusWindow`'s target is mandatory in niri-ipc
        // itself, and both workspace moves take the `Id` reference rather than
        // `null` — a `null` reference would move whatever workspace happened
        // to be focused, which during an autostart run is another stack's.
        assert_eq!(
            wire(&lower(WorkspaceAction::FocusWindow { window: 9 })),
            r#"{"FocusWindow":{"id":9}}"#
        );
        assert_eq!(
            wire(&lower(WorkspaceAction::MoveWorkspaceToIndex {
                workspace: 3,
                index: 2
            })),
            r#"{"MoveWorkspaceToIndex":{"index":2,"reference":{"Id":3}}}"#
        );
        assert_eq!(
            wire(&lower(WorkspaceAction::MoveWorkspaceToMonitor {
                workspace: 3,
                output: "HDMI-A-1".to_owned()
            })),
            r#"{"MoveWorkspaceToMonitor":{"output":"HDMI-A-1","reference":{"Id":3}}}"#
        );
        // #1129: no target to drop either, same as MoveColumnToIndex above.
        assert_eq!(
            wire(&lower(WorkspaceAction::MoveColumnToFirst)),
            r#"{"MoveColumnToFirst":{}}"#
        );
        assert_eq!(
            wire(&lower(WorkspaceAction::MoveColumnToLast)),
            r#"{"MoveColumnToLast":{}}"#
        );
    }

    /// The one action niri gives no target for, pinned as what it is.
    ///
    /// `MoveColumnToIndex` moves the **focused** column, so its correctness is
    /// a property of the batch rather than of the message: the `FocusWindow`
    /// before it is the target. This pins the message; the pairing is pinned
    /// where the batch is built
    /// (`trollshell`'s `every_move_column_is_addressed_by_the_focus_before_it`).
    #[test]
    fn move_column_to_index_carries_only_an_index() {
        assert_eq!(
            wire(&lower(WorkspaceAction::MoveColumnToIndex { index: 1 })),
            r#"{"MoveColumnToIndex":{"index":1}}"#
        );
    }

    /// A focus/move pair rides **one** socket in the order given — which is
    /// the whole reason `MoveColumnToIndex`'s missing target is safe (#1071
    /// §3.4 step 3). Two sockets, or a reordering, and the move would land on
    /// whatever column the compositor had focused in between.
    #[test]
    fn a_focus_and_its_column_move_ride_one_socket_in_order() {
        let mut fake = Fake::default();
        send_actions_over(
            &mut fake,
            vec![
                WorkspaceAction::FocusWindow { window: 9 },
                WorkspaceAction::MoveColumnToIndex { index: 1 },
                WorkspaceAction::FocusWindow { window: 10 },
                WorkspaceAction::MoveColumnToIndex { index: 2 },
            ],
        )
        .expect("the batch lands");

        assert_eq!(fake.connects(), 1, "one socket for the whole batch");
        assert_eq!(
            fake.seen().into_iter().map(|(_, a)| a).collect::<Vec<_>>(),
            vec![
                r#"{"FocusWindow":{"id":9}}"#,
                r#"{"MoveColumnToIndex":{"index":1}}"#,
                r#"{"FocusWindow":{"id":10}}"#,
                r#"{"MoveColumnToIndex":{"index":2}}"#,
            ]
        );
    }

    /// A query checks both reply layers and answers as of now.
    #[test]
    fn a_query_returns_the_reply_and_rejects_the_wrong_one() {
        let fake = Fake::default();
        fake.0
            .borrow_mut()
            .replies
            .push_back(Ok(Response::Workspaces(vec![mk_workspace(
                1, CONNECTOR, true,
            )])));
        let got = query_over(&mut fake.clone(), Request::Workspaces, |r| match r {
            Response::Workspaces(w) => Some(w),
            _ => None,
        })
        .expect("the list comes back");
        assert_eq!(got.len(), 1);

        let wrong = Fake::default();
        wrong
            .0
            .borrow_mut()
            .replies
            .push_back(Ok(Response::Handled));
        let err = query_over(&mut wrong.clone(), Request::Workspaces, |r| match r {
            Response::Workspaces(w) => Some(w),
            _ => None,
        })
        .expect_err("a reply of the wrong shape is an error, not an empty list");
        assert!(err.contains("unexpected reply"), "{err}");
    }

    // ── Post-close/post-layout reflow nudge (#1129) ─────────────────────────

    #[test]
    fn active_workspace_id_finds_the_active_workspace_on_the_named_output() {
        let ws = vec![
            mk_workspace(1, CONNECTOR, true),
            mk_workspace(2, "HDMI-A-1", true),
        ];
        assert_eq!(active_workspace_id_in(CONNECTOR, &ws), Some(1));
        assert_eq!(active_workspace_id_in("HDMI-A-1", &ws), Some(2));
    }

    #[test]
    fn active_workspace_id_is_none_for_an_unknown_connector() {
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        assert_eq!(active_workspace_id_in("nonexistent", &ws), None);
    }

    #[test]
    fn active_workspace_id_ignores_an_inactive_workspace_on_the_output() {
        let ws = vec![mk_workspace(1, CONNECTOR, false)];
        assert_eq!(active_workspace_id_in(CONNECTOR, &ws), None);
    }

    /// A window in column `column`, row 1 — [`mk_window`] with a real column
    /// instead of the fixed `(1, 1)` `has_edge_window`'s fixtures use, so the
    /// reflow tests can pin ordering across more than one column.
    fn mk_tiled(id: u64, workspace_id: u64, column: usize, focused: bool) -> Window {
        let mut w = mk_window(id, workspace_id, (100.0, 100.0));
        w.layout.pos_in_scrolling_layout = Some((column, 1));
        w.is_focused = focused;
        w
    }

    /// A floating window: no column at all (`pos_in_scrolling_layout ==
    /// None`, niri-ipc 26.4 `lib.rs:1373`) — [`mk_window`] with that field
    /// cleared, named here so the reflow tests that specifically want a
    /// floater read as intentional rather than "used the wrong constructor".
    fn mk_floating(id: u64, workspace_id: u64, focused: bool) -> Window {
        let mut w = mk_window(id, workspace_id, (100.0, 100.0));
        w.layout.pos_in_scrolling_layout = None;
        w.is_focused = focused;
        w
    }

    /// The chain Annika verified live (#1129 triage): focus the first
    /// column, move it to the end and back to the start twice, then restore
    /// whatever had focus before the nudge ran.
    #[test]
    fn reflow_batch_is_the_verified_chain_ending_in_the_restored_focus() {
        let windows = vec![
            mk_tiled(20, 1, 2, true), // focused, but NOT first in column order
            mk_tiled(10, 1, 1, false),
            mk_tiled(30, 1, 3, false),
        ];
        assert_eq!(
            reflow_batch(1, &windows),
            vec![
                WorkspaceAction::FocusWindow { window: 10 },
                WorkspaceAction::MoveColumnToFirst,
                WorkspaceAction::MoveColumnToLast,
                WorkspaceAction::MoveColumnToFirst,
                WorkspaceAction::FocusWindow { window: 20 },
            ],
            "leads with the first column in order, ends restoring the window \
             that was actually focused (20), not the one the nudge focused (10)"
        );
    }

    /// Mutation guard: dropping the trailing restore-focus step must redden —
    /// a batch that ends on `MoveColumnToFirst` leaves the user parked on
    /// column 1 instead of back on whatever they had focused.
    #[test]
    fn reflow_batch_ends_with_the_restore_focus_step() {
        let windows = vec![mk_tiled(10, 1, 1, true)];
        let batch = reflow_batch(1, &windows);
        assert_eq!(
            batch.last(),
            Some(&WorkspaceAction::FocusWindow { window: 10 }),
            "the batch must end by restoring focus, not on the bare move chain: {batch:?}"
        );
    }

    /// A workspace with no window at all sends nothing — no `FocusWindow`
    /// with an id that doesn't exist, and no socket for
    /// [`send_actions_over`]/[`send_actions`] to open.
    #[test]
    fn reflow_batch_is_empty_for_a_workspace_with_no_window() {
        assert!(reflow_batch(1, &[]).is_empty());
    }

    /// A workspace whose only windows are floating (no column at all) is the
    /// same "nothing to reflow" case as no window: floating windows are
    /// excluded from the column-order search entirely, so the batch must stay
    /// empty rather than emitting a `FocusWindow` for one of them (which would
    /// steal focus without anything actually reflowing).
    #[test]
    fn reflow_batch_is_empty_when_every_window_is_floating() {
        let windows = vec![mk_floating(10, 1, false), mk_floating(20, 1, true)];
        assert!(reflow_batch(1, &windows).is_empty());
    }

    /// Windows on other workspaces must not leak into either the column-order
    /// search or the restore-focus lookup by id — only whether *some* window
    /// is focused should matter for the restore, but the reflow's own target
    /// column comes only from `workspace`'s own windows.
    #[test]
    fn reflow_batch_only_reflows_the_named_workspace() {
        let windows = vec![
            mk_tiled(10, 1, 1, false),
            mk_tiled(99, 2, 1, true), // different workspace, focused
        ];
        assert_eq!(
            reflow_batch(1, &windows),
            vec![
                WorkspaceAction::FocusWindow { window: 10 },
                WorkspaceAction::MoveColumnToFirst,
                WorkspaceAction::MoveColumnToLast,
                WorkspaceAction::MoveColumnToFirst,
                WorkspaceAction::FocusWindow { window: 99 },
            ],
            "workspace 1 supplies the column target; the globally focused \
             window (on workspace 2) is still what gets restored"
        );
    }

    /// No window anywhere is focused: the batch ends on the move chain with
    /// no restore step, rather than a `FocusWindow` for a nonexistent focus.
    #[test]
    fn reflow_batch_has_no_restore_step_when_nothing_is_focused() {
        let windows = vec![mk_tiled(10, 1, 1, false), mk_tiled(20, 1, 2, false)];
        assert_eq!(
            reflow_batch(1, &windows),
            vec![
                WorkspaceAction::FocusWindow { window: 10 },
                WorkspaceAction::MoveColumnToFirst,
                WorkspaceAction::MoveColumnToLast,
                WorkspaceAction::MoveColumnToFirst,
            ]
        );
    }

    /// [`reflow_batch`] rides one socket in order, over the same
    /// [`send_actions_over`] property #1071 §3.4 pins for every other batch —
    /// pinned here at the wire, the same way
    /// `a_focus_and_its_column_move_ride_one_socket_in_order` pins
    /// `MoveColumnToIndex`'s pairing.
    #[test]
    fn reflow_batch_rides_one_socket_in_order() {
        let windows = vec![mk_tiled(10, 1, 1, true)];
        let mut fake = Fake::default();
        send_actions_over(&mut fake, reflow_batch(1, &windows)).expect("the batch lands");

        assert_eq!(fake.connects(), 1, "one socket for the whole nudge");
        assert_eq!(
            fake.seen().into_iter().map(|(_, a)| a).collect::<Vec<_>>(),
            vec![
                r#"{"FocusWindow":{"id":10}}"#,
                r#"{"MoveColumnToFirst":{}}"#,
                r#"{"MoveColumnToLast":{}}"#,
                r#"{"MoveColumnToFirst":{}}"#,
                r#"{"FocusWindow":{"id":10}}"#,
            ]
        );
    }

    /// An empty batch (no tiled window on the workspace) opens no socket at
    /// all — the same "empty batch connects nothing" contract every other
    /// batch gets, restated here so a regression that stopped short-circuiting
    /// empty reflows specifically would redden.
    #[test]
    fn an_empty_reflow_batch_connects_nothing() {
        let mut fake = Fake::default();
        send_actions_over(&mut fake, reflow_batch(1, &[])).expect("trivially lands");
        assert_eq!(fake.connects(), 0);
    }
}
