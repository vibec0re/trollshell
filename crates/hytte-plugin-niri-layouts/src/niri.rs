//! The niri IPC half: a one-request/one-reply [`Transport`] seam and the
//! [`apply`] orchestration over it.
//!
//! [`apply`] is the single entry point both hats use — the chip's click worker
//! and the CLI's `apply` subcommand — so the two can never drift. It fetches the
//! snapshots [`plan`](crate::layout::plan) and [`Layout::Golden`]'s pair
//! resolution need — windows, workspaces, outputs, and (only when the caller
//! did not name a screen itself, #1050) the focused output — then sends one
//! `SetWindowWidth` per column, followed by a niri reflow nudge (#1129,
//! `move_column_to_first_then_last_then_first`): `SetWindowWidth` changes a
//! column's *width*, not anything niri reflows the workspace layout for, so a
//! column sitting flush against the old edge is left exactly where it was —
//! partly off screen once the widths around it change. All the deciding lives
//! in [`layout`]; this module only moves bytes (plus one debug
//! line, #1052, for when the width resolution comes back empty).
//!
//! The [`Transport`] indirection is what makes the whole path testable: the
//! tests below drive [`apply`] against a scripted fake and never open a socket.

use crate::layout::{self, Layout};
use crate::plugin::PLUGIN_ID;
use niri_ipc::socket::SOCKET_PATH_ENV;
use niri_ipc::{
    Action, Output, Reply, Request, Response, SizeChange, Window, Workspace, WorkspaceReferenceArg,
};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// One niri request, one niri reply.
///
/// `Err` is a *transport* failure (no `$NIRI_SOCKET`, the socket went away, a
/// reply that would not parse). A niri-level refusal arrives as the inner
/// `Err(String)` of [`Reply`] and carries niri's own error text, which is what
/// [`apply`] surfaces verbatim.
pub(crate) trait Transport {
    fn send(&mut self, request: Request) -> Result<Reply, String>;

    /// Where a diagnostic that isn't a niri error goes — real stderr for
    /// [`SocketTransport`], captured into `fake::Fake::logs` for the tests
    /// below (#1056 review, LOW-1). The only diagnostic today is
    /// [`apply`]'s Golden missing-width fallback; this seam is what lets a
    /// test assert it was emitted without piping the process's real stderr
    /// through a pipe — the same shape `watch.rs`'s `Backend::log` already
    /// uses for its own diagnostics.
    fn log(&mut self, line: &str);
}

/// How long one request may take against niri, connect, write and the whole
/// reply together (one deadline, not a timeout per read: a peer dribbling bytes
/// must not stretch it).
///
/// `niri_ipc::socket::Socket` arms no timeouts and hides its stream, so a niri
/// that accepted the connection and then wedged (or never answers) parked the
/// calling thread forever: the CLI hung under a `spawn` bind, and the chip's
/// blocking-pool worker was lost for the session, with every later click queued
/// behind it. #1526 deferred this until the #1516 reflow restore landed, since
/// a timeout mid-chain is only safe to surface once the chain puts things
/// back. A healthy niri answers in milliseconds; 5 s is generous.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// The real transport: one short-lived `$NIRI_SOCKET` connection per request.
///
/// A fresh connect per request rather than one long-lived socket, matching the
/// shell's own command path (`hytte-services`' `niri::send_action`): a unix
/// socket connect is cheap, and niri's IPC only guarantees one reply per
/// connection for a non-`EventStream` request.
///
/// It dials the stream itself (framing exactly as `Socket::send` does: one JSON
/// request line, one JSON reply line) because only then can the read/write
/// timeouts be armed. The long-lived `EventStream` reader in `watch.rs` shares
/// none of this and deliberately keeps no request timeout: it idles legitimately.
/// A timeout is not retried here: it is an `Err` like any transport failure,
/// and `apply` stops at the first one.
pub(crate) struct SocketTransport {
    timeout: Duration,
}

impl SocketTransport {
    pub(crate) fn new() -> Self {
        Self::with_timeout(REQUEST_TIMEOUT)
    }

    /// A transport with its own timeout; the seam the timeout test uses so it
    /// need not wait five seconds.
    pub(crate) fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// One request/reply over an already-connected stream, all of it within
    /// one `deadline`: the write, and every read of the reply. A per-read
    /// timeout alone lets a peer that dribbles a byte at a time stretch one
    /// reply indefinitely, so each read is re-armed with only the time left.
    fn exchange(
        &self,
        stream: &UnixStream,
        request: &Request,
        deadline: Instant,
    ) -> Result<Reply, String> {
        let mut line = serde_json::to_string(request)
            .map_err(|e| format!("cannot encode the niri request: {e}"))?;
        line.push('\n');
        stream
            .set_write_timeout(Some(self.remaining(deadline)?))
            .map_err(|e| format!("cannot arm the niri ipc timeouts: {e}"))?;
        let mut writer = stream;
        writer
            .write_all(line.as_bytes())
            .map_err(|e| self.describe(&e, "niri ipc write failed"))?;

        let mut reply = Vec::new();
        let mut reader = stream;
        let mut chunk = [0_u8; 4096];
        loop {
            stream
                .set_read_timeout(Some(self.remaining(deadline)?))
                .map_err(|e| format!("cannot arm the niri ipc timeouts: {e}"))?;
            match reader.read(&mut chunk) {
                // EOF: take what arrived, as `read_line` would have.
                Ok(0) => break,
                Ok(n) => {
                    if let Some(pos) = chunk[..n].iter().position(|&b| b == b'\n') {
                        reply.extend_from_slice(&chunk[..=pos]);
                        break;
                    }
                    reply.extend_from_slice(&chunk[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(self.describe(&e, "niri ipc failed")),
            }
        }
        serde_json::from_slice(&reply).map_err(|e| format!("niri ipc failed: bad reply: {e}"))
    }

    /// The time left before `deadline`, or the timeout error once it is spent
    /// (a zero socket timeout means "block forever", so it never reaches the
    /// socket).
    fn remaining(&self, deadline: Instant) -> Result<Duration, String> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(format!("niri ipc timed out after {:?}", self.timeout))
        } else {
            Ok(left)
        }
    }

    /// Dial `path` within `deadline`. `UnixStream::connect` has no timeout and
    /// blocks while niri's listen backlog is full, so it runs on a helper
    /// thread the caller stops waiting for at the deadline (the thread ends
    /// when the connect does).
    fn connect(&self, path: &Path, deadline: Instant) -> Result<UnixStream, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let path = path.to_owned();
        std::thread::spawn(move || {
            // The receiver may be gone after a timeout; the stream just drops.
            drop(tx.send(UnixStream::connect(path)));
        });
        match rx.recv_timeout(self.remaining(deadline)?) {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(e)) => Err(format!("cannot reach niri over $NIRI_SOCKET: {e}")),
            Err(_) => Err(format!(
                "cannot reach niri over $NIRI_SOCKET: timed out after {:?}",
                self.timeout
            )),
        }
    }

    fn describe(&self, e: &io::Error, what: &str) -> String {
        if matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            format!("niri ipc timed out after {:?}", self.timeout)
        } else {
            format!("{what}: {e}")
        }
    }

    fn send_to(&self, path: &Path, request: &Request) -> Result<Reply, String> {
        let deadline = Instant::now() + self.timeout;
        let stream = self.connect(path, deadline)?;
        self.exchange(&stream, request, deadline)
    }
}

impl Transport for SocketTransport {
    fn send(&mut self, request: Request) -> Result<Reply, String> {
        let path = std::env::var_os(SOCKET_PATH_ENV).ok_or_else(|| {
            format!("cannot reach niri over $NIRI_SOCKET: {SOCKET_PATH_ENV} is not set")
        })?;
        self.send_to(Path::new(&path), &request)
    }

    fn log(&mut self, line: &str) {
        // stderr, which systemd routes to the journal for a plugin unit —
        // the same destination `watch.rs`'s `SocketBackend::log` writes to.
        eprintln!("{line}");
    }
}

/// Send one request and unwrap both error layers: the transport's, and niri's
/// own message (kept verbatim — it is the most useful thing to show a human).
fn ask(transport: &mut impl Transport, request: Request) -> Result<Response, String> {
    transport.send(request)?
}

/// Apply `layout` to the columns of the active workspace on `on_output` — or,
/// when `on_output` is `None`, on niri's focused output.
///
/// `on_output` is #1050's other half: the chip is mirrored onto every monitor,
/// so a click carries the connector name of the screen it came from
/// ([`Input::Event::output`](hytte_plugin::Input::Event::output)) and the
/// layout is applied **there**, not on whichever screen happens to hold
/// keyboard focus. `None` means *not attributable* — the CLI hat, which has no
/// screen, and the drawer panel, which the host cannot attribute — and falls
/// back to the focused output, which is exactly what every build before #1050
/// did.
///
/// The `FocusedOutput` round trip is **skipped** when `on_output` names a
/// screen: its answer would be discarded, and one fewer unix-socket round trip
/// per click is worth the two request shapes.
/// [`target_workspace`](crate::layout) treats both the same way — a connector
/// name selects that output's `is_active` workspace either way — so the
/// planner cannot tell which arm produced it.
///
/// A connector niri has no workspace on (a screen that has since gone away)
/// resolves to no target workspace, so the apply is `Ok(0)`: a no-op with the
/// usual debug line, never a layout applied to the wrong screen.
///
/// Returns how many columns were resized — `Ok(0)` means the target workspace
/// held no tiled columns, which is a no-op and not an error (and, since
/// #1129, sends no reflow nudge either — there is no column to nudge). `Err`
/// carries niri's own text for the first request that failed, resize or
/// reflow. No widths are rolled back, because a partially applied layout is
/// still a coherent one and the next click fixes it. A reflow nudge that fails
/// partway is different: it would leave the leftmost column at the far end
/// and focus where the nudge put it. So [`reflow`] puts both back,
/// best-effort, before the error is returned.
///
/// Always fetches `Outputs` (#1052), even for `equal`/`split`, which never
/// look at it: `apply` doesn't special-case the layout when asking niri, only
/// when deciding what to do with the answer, so the two hats' requests stay
/// identical regardless of which button was pressed.
pub(crate) fn apply(
    transport: &mut impl Transport,
    layout: Layout,
    on_output: Option<&str>,
) -> Result<usize, String> {
    let windows = windows(transport)?;
    let workspaces = workspaces(transport)?;
    let outputs = outputs(transport)?;
    let output = match on_output {
        Some(named) => Some(named.to_owned()),
        None => focused_output(transport)?,
    };

    let target_output = layout::target_output_name(&workspaces, output.as_deref());
    let logical_width = target_output.and_then(|name| layout::logical_width_of(&outputs, name));
    if layout == Layout::Golden && logical_width.is_none() {
        transport.log(&missing_width_diagnostic(target_output));
    }
    let golden = layout::golden_pair(logical_width);

    let plan = layout::plan(&windows, &workspaces, output.as_deref(), layout, golden);
    for &(id, proportion) in &plan {
        ask(
            transport,
            Request::Action(Action::SetWindowWidth {
                // Always by id: `None` would mean "the focused window", which is
                // exactly one of the n columns we are walking.
                id: Some(id),
                change: SizeChange::SetProportion(percent(proportion)),
            }),
        )?;
    }

    // #1129: the resize loop above changes column *widths*, which is not a
    // change niri reflows the workspace layout for — nudge it with the same
    // move-to-first/move-to-last/move-to-first chain
    // `hytte_services::niri::reflow_workspace` sends after the shell's
    // sidebar settles closed (that module's the twin of this one: same
    // chain, same reason, over its own `send_actions` rather than this
    // plugin's `Transport` — the plugin can't link `hytte-services`, #35).
    //
    // `plan`'s first entry is already the leftmost column's representative
    // window (`layout::plan`'s `BTreeMap` is keyed by column index), so no
    // second windows-in-column-order pass is needed. No columns → no nudge:
    // there is nothing to reflow, and no `FocusWindow` to steal focus for no
    // reason. `windows` and `workspaces` were fetched once, at the top of
    // `apply`, before anything above changed niri's state — so "whatever had
    // focus" in `reflow` really does mean "before this apply ran".
    if let Some(&(first_id, _)) = plan.first() {
        reflow(transport, first_id, &windows, &workspaces)?;
    }

    Ok(plan.len())
}

/// [`apply`]'s #1129 reflow nudge, plus the two things it has to put back
/// however it ends: the leftmost column's place, and focus.
///
/// The chain is `FocusWindow` (the leftmost column) → `MoveColumnToFirst` →
/// `MoveColumnToLast` → `MoveColumnToFirst`. It is not atomic: niri can refuse
/// any step, or the socket can go away between two of them. Once the
/// `FocusWindow` has landed, a failure has always left something out of place:
///
/// * focus is on the leftmost column, not wherever the user had it;
/// * a failure after `MoveColumnToLast` landed leaves the leftmost column as
///   the **rightmost** one.
///
/// So once the chain has started, the put-back steps run however it ended. One
/// more `MoveColumnToFirst` follows a failed move (a no-op unless the column
/// was left at the end, since it is the leftmost column), and then
/// [`restore_focus`]. Each is best-effort. The **first** error is returned, and
/// a later one is logged rather than dropped.
///
/// A refused `FocusWindow` sends nothing further. Nothing has moved yet, focus
/// included, so there is nothing to put back, and a focus "restore" sent then
/// would act on a workspace that never lost focus (see [`restore_focus`] for why
/// that is not harmless).
///
/// Mirrored in `hytte-services`' `niri.rs` as `reflow_over` (#1516, #1517),
/// which sends the same chain and puts back the same two things, and cannot
/// share this code: the shell never links this plugin crate. Keep the two in
/// step. (The target, the first tiled column, is picked there by
/// `reflow_plan` and here by `apply`'s `plan.first()`.)
fn reflow(
    transport: &mut impl Transport,
    first_id: u64,
    windows: &[Window],
    workspaces: &[Workspace],
) -> Result<(), String> {
    ask(
        transport,
        Request::Action(Action::FocusWindow { id: first_id }),
    )?;
    // The workspace the nudge just focused, for `restore_focus`'s one guard.
    let nudged = windows
        .iter()
        .find(|w| w.id == first_id)
        .and_then(|w| w.workspace_id);

    let moved = [
        Action::MoveColumnToFirst {},
        Action::MoveColumnToLast {},
        Action::MoveColumnToFirst {},
    ]
    .into_iter()
    .try_for_each(|action| ask(transport, Request::Action(action)).map(drop));
    if moved.is_err() {
        // Every move acts on the focused column, and the focused column is
        // still the leftmost one: none of the moves changes focus.
        if let Err(e) = ask(transport, Request::Action(Action::MoveColumnToFirst {})) {
            transport.log(&format!(
                "[{PLUGIN_ID}] reflow: the first column was not moved back after a failed move: {e}"
            ));
        }
    }

    let restored = restore_focus(transport, windows, workspaces, nudged);
    match (moved, restored) {
        (Err(first), Err(later)) => {
            transport.log(&format!(
                "[{PLUGIN_ID}] reflow: focus was not restored after a failed move: {later}"
            ));
            Err(first)
        }
        (moved, restored) => moved.and(restored),
    }
}

/// Put focus back where it was before [`apply`] ran, from the snapshots taken
/// at its top. Three cases, in order:
///
/// 1. **A focused window**: focus it again, by id.
/// 2. **No focused window, but the focused workspace has an active one**:
///    keyboard focus was elsewhere, most likely on a layer-shell surface such
///    as the shell's own drawer. Focusing that workspace's active window puts
///    back the column the nudge moved off.
/// 3. **The focused workspace is empty**: focus it by id. This is the case the
///    restore used to skip. A click on a chip on one screen, with focus on an
///    empty workspace on another, left focus on the clicked screen.
///
/// Case 3 is guarded: it never targets the workspace the nudge itself
/// focused. A `FocusWorkspace` on the workspace that is already active on the
/// active output is not a no-op under niri's `workspace-auto-back-and-forth`,
/// which turns it into a switch to the *previous* workspace. That cannot happen
/// on the path above, because an empty workspace is never the one with columns
/// to lay out. The guard keeps it true if that ever changes.
///
/// With none of the three (niri reported no focused workspace at all), there
/// is nothing to restore to and nothing is sent.
///
/// Mirrored in `hytte-services`' `niri.rs` as `restore_focus_action` (#1516).
/// Keep the two in step.
fn restore_focus(
    transport: &mut impl Transport,
    windows: &[Window],
    workspaces: &[Workspace],
    nudged: Option<u64>,
) -> Result<(), String> {
    let focused_workspace = workspaces.iter().find(|w| w.is_focused);
    let action = if let Some(window) = windows.iter().find(|w| w.is_focused) {
        Action::FocusWindow { id: window.id }
    } else if let Some(id) = focused_workspace.and_then(|w| w.active_window_id) {
        Action::FocusWindow { id }
    } else if let Some(workspace) = focused_workspace.filter(|w| Some(w.id) != nudged) {
        Action::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(workspace.id),
        }
    } else {
        return Ok(());
    };
    ask(transport, Request::Action(action)).map(drop)
}

/// The line [`apply`] logs when [`Layout::Golden`] can't resolve a logical
/// width for the target output (#1052) — built from
/// [`layout::GOLDEN_WIDE_MAJOR`]/[`layout::GOLDEN_WIDE_MINOR`] rather than a
/// hand-typed "75 % / 25 %", so a change to the fallback pair can't leave
/// this message naming the wrong one (#1056 review, LOW-1).
fn missing_width_diagnostic(target_output: Option<&str>) -> String {
    format!(
        "[{PLUGIN_ID}] golden: no logical width for the target output{}; \
         defaulting to the >= {}px pair ({:.0} % / {:.0} %)",
        target_output.map_or_else(String::new, |name| format!(" ({name})")),
        layout::GOLDEN_BREAKPOINT,
        layout::GOLDEN_WIDE_MAJOR * 100.0,
        layout::GOLDEN_WIDE_MINOR * 100.0,
    )
}

/// The planner's fraction as the **percentage** `SizeChange::SetProportion`
/// actually carries.
///
/// This is the one seam where the domain unit meets the wire unit, and the wire
/// unit is a percentage of the working area, `0.0..=100.0` — not a fraction.
/// `niri msg action set-window-width 50%` puts `50.0` on this wire: niri-ipc's
/// `impl FromStr for SizeChange` strips the `%` and keeps the number unscaled
/// (its own test asserts `"10%".parse() == SetProportion(10.)`), and niri's wiki
/// equates the action's `"100%"` with the KDL config's `proportion 1.0`, so full
/// width is `100.0`.
///
/// Keeping [`layout`] in fractions and converting here means `1.0 / n` stays
/// readable as "one n-th of the screen" and exactly one line knows the unit.
fn percent(proportion: f64) -> f64 {
    proportion * 100.0
}

fn windows(transport: &mut impl Transport) -> Result<Vec<Window>, String> {
    match ask(transport, Request::Windows)? {
        Response::Windows(windows) => Ok(windows),
        other => Err(unexpected("Windows", &other)),
    }
}

fn workspaces(transport: &mut impl Transport) -> Result<Vec<Workspace>, String> {
    match ask(transport, Request::Workspaces)? {
        Response::Workspaces(workspaces) => Ok(workspaces),
        other => Err(unexpected("Workspaces", &other)),
    }
}

/// Every connected output, keyed by connector name — the exact shape niri's
/// `Outputs` request replies with, left unflattened (#1056 review, NIT-2): a
/// `Vec` would make [`layout::logical_width_of`] linear-search by
/// `Output.name`, which is both O(n) and assumes the map key and that field
/// never diverge. `HashMap::get` is O(1) and needs no such assumption.
fn outputs(transport: &mut impl Transport) -> Result<HashMap<String, Output>, String> {
    match ask(transport, Request::Outputs)? {
        Response::Outputs(outputs) => Ok(outputs),
        other => Err(unexpected("Outputs", &other)),
    }
}

/// The focused output's connector name, or `None` when niri reports no focused
/// output (see [`layout::plan`]'s fallback).
fn focused_output(transport: &mut impl Transport) -> Result<Option<String>, String> {
    match ask(transport, Request::FocusedOutput)? {
        Response::FocusedOutput(output) => Ok(output.map(|o| o.name)),
        other => Err(unexpected("FocusedOutput", &other)),
    }
}

fn unexpected(request: &str, response: &Response) -> String {
    format!("niri answered a {request} request with {response:?}")
}

/// A scripted niri for tests — shared with [`crate::plugin`]'s tests, which
/// exercise the click → apply → toast path against this same fake rather than
/// building a second one that could drift from it.
#[cfg(test)]
pub(crate) mod fake {
    use super::Transport;
    use niri_ipc::{
        Action, LogicalOutput, Output, Reply, Request, Response, SizeChange, Transform, Window,
        WindowLayout, Workspace,
    };
    use std::collections::HashMap;

    pub(crate) const OUTPUT: &str = "DP-1";
    /// The second screen the #1050 tests add — the one a click can name.
    pub(crate) const OTHER_OUTPUT: &str = "DP-2";

    pub(crate) fn workspace() -> Workspace {
        workspace_on(1, OUTPUT, true)
    }

    /// A workspace that is **active on `output`**, focused iff `is_focused`.
    ///
    /// The two flags are separate here (unlike `watch`'s fixture, which has one
    /// screen) precisely because #1050 turns on their difference: the clicked
    /// screen's workspace is active there and usually *not* the focused one.
    pub(crate) fn workspace_on(id: u64, output: &str, is_focused: bool) -> Workspace {
        Workspace {
            id,
            idx: 1,
            name: None,
            output: Some(output.to_owned()),
            is_urgent: false,
            is_active: true,
            is_focused,
            active_window_id: None,
        }
    }

    /// A tiled window alone in column `column` of workspace 1.
    pub(crate) fn tile(id: u64, column: usize) -> Window {
        tile_on(id, 1, column)
    }

    /// A tiled window alone in column `column` of workspace `workspace`.
    pub(crate) fn tile_on(id: u64, workspace: u64, column: usize) -> Window {
        Window {
            id,
            title: None,
            app_id: None,
            pid: None,
            workspace_id: Some(workspace),
            is_focused: false,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: Some((column, 1)),
                tile_size: (100.0, 100.0),
                window_size: (100, 100),
                tile_pos_in_workspace_view: Some((0.0, 0.0)),
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    pub(crate) fn output() -> Output {
        Output {
            name: OUTPUT.to_owned(),
            make: "test".to_owned(),
            model: "test".to_owned(),
            serial: None,
            physical_size: None,
            modes: Vec::new(),
            current_mode: None,
            is_custom_mode: false,
            vrr_supported: false,
            vrr_enabled: false,
            logical: None,
        }
    }

    /// [`OTHER_OUTPUT`]'s entry in the `Outputs` reply.
    pub(crate) fn other_output() -> Output {
        Output {
            name: OTHER_OUTPUT.to_owned(),
            ..output()
        }
    }

    /// [`other_output`] with a logical geometry reporting `width` px — the
    /// second-screen twin of [`output_with_logical_width`], so a #1050 test can
    /// give the two monitors different widths.
    pub(crate) fn other_output_with_logical_width(width: u32) -> Output {
        Output {
            name: OTHER_OUTPUT.to_owned(),
            ..output_with_logical_width(width)
        }
    }

    /// [`output`] with a logical geometry reporting `width` px (#1052) —
    /// height and scale are arbitrary but plausible, since nothing here reads
    /// them.
    pub(crate) fn output_with_logical_width(width: u32) -> Output {
        Output {
            logical: Some(LogicalOutput {
                x: 0,
                y: 0,
                width,
                height: 1080,
                scale: 1.0,
                transform: Transform::Normal,
            }),
            ..output()
        }
    }

    /// Answers the four queries from canned state and records every request.
    pub(crate) struct Fake {
        pub(crate) windows: Vec<Window>,
        pub(crate) workspaces: Vec<Workspace>,
        pub(crate) focused_output: Option<Output>,
        /// Every output the `Outputs` request would list, keyed by name
        /// (#1052). Defaults to one entry for [`OUTPUT`] with no logical
        /// geometry, which resolves to `golden_pair`'s default (wide) pair —
        /// the same 75/25 every test before #1052 already expected.
        pub(crate) outputs: HashMap<String, Output>,
        /// Fail the next `Action` the way niri itself would: an inner `Err`
        /// carrying niri's own text.
        pub(crate) action_error: Option<String>,
        /// Fail the `Action` at each 0-based position (counted over every
        /// `Action` sent, resizes included) with niri's own text. This is how a
        /// test refuses one step of the reflow chain in the middle rather than
        /// the first action of the apply.
        pub(crate) failing_actions: HashMap<usize, String>,
        /// Fail every request at the transport layer (no socket at all).
        pub(crate) transport_error: Option<String>,
        pub(crate) seen: Vec<Request>,
        /// Every line [`Transport::log`] was called with, in order (#1056
        /// review, LOW-1) — the captured-diagnostic seam `watch.rs`'s
        /// `Script::logs` already uses for the same purpose.
        pub(crate) logs: Vec<String>,
    }

    impl Fake {
        pub(crate) fn with(windows: Vec<Window>) -> Self {
            Self {
                windows,
                workspaces: vec![workspace()],
                focused_output: Some(output()),
                outputs: HashMap::from([(OUTPUT.to_owned(), output())]),
                action_error: None,
                failing_actions: HashMap::new(),
                transport_error: None,
                seen: Vec::new(),
                logs: Vec::new(),
            }
        }

        /// Two tiled columns on the focused workspace — the happy default.
        pub(crate) fn two_columns() -> Self {
            Self::with(vec![tile(10, 1), tile(20, 2)])
        }

        /// A two-monitor desktop (#1050): `DP-1` focused with **two** columns
        /// (windows 10, 20), `DP-2` unfocused with **three** (30, 40, 50).
        ///
        /// The column counts differ and the window ids do not overlap, so which
        /// screen an apply targeted is legible from the `SetWindowWidth` ids
        /// alone — no proportion arithmetic, and no way for a "targets the
        /// focused output" regression to look the same as a correct answer.
        pub(crate) fn two_outputs() -> Self {
            let mut fake = Self::with(vec![
                tile_on(10, 1, 1),
                tile_on(20, 1, 2),
                tile_on(30, 2, 1),
                tile_on(40, 2, 2),
                tile_on(50, 2, 3),
            ]);
            fake.workspaces = vec![
                workspace_on(1, OUTPUT, true),
                workspace_on(2, OTHER_OUTPUT, false),
            ];
            fake.outputs.insert(OTHER_OUTPUT.to_owned(), other_output());
            fake
        }

        /// [`Self::with`], with [`OUTPUT`]'s logical width set to `width` px
        /// (#1052) — the fixture the Golden breakpoint tests build on.
        pub(crate) fn with_output_width(windows: Vec<Window>, width: u32) -> Self {
            let mut fake = Self::with(windows);
            fake.outputs
                .insert(OUTPUT.to_owned(), output_with_logical_width(width));
            fake
        }

        /// The `(id, proportion)` pairs the fake was actually asked to set.
        pub(crate) fn widths(&self) -> Vec<(u64, f64)> {
            self.seen
                .iter()
                .filter_map(|r| match r {
                    Request::Action(Action::SetWindowWidth {
                        id: Some(id),
                        change: SizeChange::SetProportion(p),
                    }) => Some((*id, *p)),
                    _ => None,
                })
                .collect()
        }

        /// The non-`Action` requests, by name, in the order they were sent.
        pub(crate) fn queries(&self) -> Vec<String> {
            self.seen
                .iter()
                .filter(|r| !matches!(r, Request::Action(_)))
                .map(|r| format!("{r:?}"))
                .collect()
        }
    }

    impl Transport for Fake {
        fn send(&mut self, request: Request) -> Result<Reply, String> {
            self.seen.push(request.clone());
            if let Some(e) = &self.transport_error {
                return Err(e.clone());
            }
            Ok(match request {
                Request::Windows => Ok(Response::Windows(self.windows.clone())),
                Request::Workspaces => Ok(Response::Workspaces(self.workspaces.clone())),
                Request::Outputs => Ok(Response::Outputs(self.outputs.clone())),
                Request::FocusedOutput => Ok(Response::FocusedOutput(self.focused_output.clone())),
                Request::Action(_) => {
                    // This one is already in `seen`, so the count includes it.
                    let position = self
                        .seen
                        .iter()
                        .filter(|r| matches!(r, Request::Action(_)))
                        .count()
                        - 1;
                    match self
                        .action_error
                        .take()
                        .or_else(|| self.failing_actions.remove(&position))
                    {
                        Some(msg) => Err(msg),
                        None => Ok(Response::Handled),
                    }
                }
                other => Err(format!("fake got an unscripted request: {other:?}")),
            })
        }

        fn log(&mut self, line: &str) {
            self.logs.push(line.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    /// A niri that accepts and never replies must surface a timeout error in
    /// bounded time instead of parking the caller forever.
    #[test]
    fn a_silent_niri_times_out_instead_of_hanging() {
        use super::SocketTransport;
        use std::os::unix::net::UnixListener;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        // Accept and hold the connection open, say nothing.
        let holder = std::thread::spawn(move || listener.accept().map(|(s, _)| s));

        let transport = SocketTransport::with_timeout(Duration::from_millis(100));
        let started = Instant::now();
        let err = transport
            .send_to(&path, &Request::Version)
            .expect_err("no reply must be an error");
        assert!(err.contains("timed out"), "got {err:?}");
        assert!(started.elapsed() < Duration::from_secs(3), "not bounded");
        drop(holder.join());
    }

    /// A peer that dribbles one byte every ~100 ms and never sends the newline
    /// must still hit the one whole-reply deadline: every read succeeds in
    /// time, so only a deadline tracked across reads can stop it.
    #[test]
    fn a_dribbling_niri_hits_the_whole_reply_deadline() {
        use super::SocketTransport;
        use std::io::Write;
        use std::os::unix::net::UnixListener;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Dribble for far longer than the deadline; stop on a broken pipe
            // once the client gives up.
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(100));
                if stream.write_all(b" ").is_err() {
                    break;
                }
            }
        });

        let transport = SocketTransport::with_timeout(Duration::from_millis(500));
        let started = Instant::now();
        let err = transport
            .send_to(&path, &Request::Version)
            .expect_err("a dribbled reply must not complete");
        let took = started.elapsed();
        assert!(err.contains("timed out"), "got {err:?}");
        assert!(took < Duration::from_millis(2500), "not bounded: {took:?}");
        drop(peer.join());
    }

    /// A niri whose listen backlog is full (it stopped accepting) must not
    /// park the connect: `UnixStream::connect` blocks there until a slot
    /// frees, so only the deadline on the connect itself can end it.
    #[test]
    fn a_full_listen_backlog_times_out_the_connect() {
        use super::SocketTransport;
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("niri.sock");
        // Bound and never accepted from, like a wedged niri.
        let listener = UnixListener::bind(&path).expect("bind");
        // Fill the backlog. A closed client keeps its slot until it is
        // accepted, so nothing has to stay open; the filler's last connect
        // parks, and ends with ECONNREFUSED once the listener is dropped.
        let (slot_taken, slots) = mpsc::channel();
        let filler_path = path.clone();
        let filler = std::thread::spawn(move || {
            while UnixStream::connect(&filler_path).is_ok() {
                if slot_taken.send(()).is_err() {
                    break;
                }
            }
        });
        // Full once no connect has completed for 200 ms.
        while slots.recv_timeout(Duration::from_millis(200)).is_ok() {}

        let (done, outcome) = mpsc::channel();
        let request_path = path.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = SocketTransport::with_timeout(Duration::from_millis(300))
                .send_to(&request_path, &Request::Version);
            drop(done.send((result, started.elapsed())));
        });
        let outcome = outcome.recv_timeout(Duration::from_secs(3));
        // Frees the filler's parked connect (and an unbounded one, so a
        // failing run still ends).
        drop(listener);
        filler.join().expect("filler");
        let (result, took) = outcome.expect("the connect was not bounded by the deadline");
        let err = result.expect_err("no backlog slot, no reply");
        // The connect's own timeout, not the read's: a connect that got a
        // slot would time out on the reply instead and prove nothing here.
        assert!(
            err.starts_with("cannot reach niri") && err.contains("timed out"),
            "got {err:?}"
        );
        assert!(took < Duration::from_secs(2), "not bounded: {took:?}");
    }

    use super::fake::{self, Fake, tile};
    use super::{Transport, apply, missing_width_diagnostic};
    use crate::layout::Layout;
    use niri_ipc::{Action, Reply, Request, Response};
    use std::collections::HashMap;

    // ── The wire unit ────────────────────────────────────────────────────────
    //
    // Every expectation below is a **literal percentage**, deliberately not
    // derived from this crate's `GOLDEN_WIDE_MAJOR` / `SPLIT_SHARE` / `1.0 / n`.
    // Comparing the wire back to the constants it came from is a closed loop —
    // it pins *a* number without ever stating what niri means by it, and it is
    // how this crate shipped 0.5 % where it meant 50 % (#1026 review, HIGH-1).
    //
    // The numbers come from niri, not from here: `set-window-width 50%` parses
    // to `SetProportion(50.0)` (niri-ipc's `impl FromStr for SizeChange`, whose
    // own test asserts `"10%" == SetProportion(10.)`), and `"100%"` is the KDL
    // config's `proportion 1.0` (niri wiki, Fullscreen-and-Maximize) — so full
    // width is 100.0 and a half is 50.0.

    #[test]
    fn split_asks_for_fifty_percent_per_column_not_half_a_percent() {
        let mut niri = Fake::with(vec![tile(10, 1), tile(20, 2), tile(30, 3)]);

        let applied = apply(&mut niri, Layout::Split, None).expect("the fake answers everything");

        assert_eq!(applied, 3);
        assert_eq!(
            niri.widths(),
            vec![(10, 50.0), (20, 50.0), (30, 50.0)],
            "half the working area is 50.0 on this wire, not 0.5"
        );
    }

    #[test]
    fn equal_over_four_columns_asks_for_twenty_five_percent_each() {
        let mut niri = Fake::with(vec![tile(10, 1), tile(20, 2), tile(30, 3), tile(40, 4)]);

        apply(&mut niri, Layout::Equal, None).expect("the fake answers everything");

        assert_eq!(
            niri.widths(),
            vec![(10, 25.0), (20, 25.0), (30, 25.0), (40, 25.0)],
            "a quarter of the working area is 25.0, not 0.25"
        );
    }

    #[test]
    fn sends_one_set_width_per_column_left_to_right() {
        let mut niri = Fake::with(vec![tile(10, 1), tile(20, 2), tile(30, 3)]);

        let applied = apply(&mut niri, Layout::Golden, None).expect("the fake answers everything");

        assert_eq!(applied, 3);
        assert_eq!(
            niri.widths(),
            vec![(10, 75.0), (20, 25.0), (30, 25.0)],
            "golden's wide column is 75 % of the working area, not 0.75 %"
        );
    }

    /// The serialised `SetWindowWidth` resize actions of one `apply`, in send
    /// order — scoped to just the resize half (not [`all_action_wire_bytes`]'
    /// full action log) so the percentage-literal tests below keep pinning
    /// only that, unaffected by the #1129 reflow tail `apply` now sends after
    /// it.
    fn wire_bytes(niri: &Fake) -> Vec<String> {
        niri.seen
            .iter()
            .filter(|r| matches!(r, Request::Action(Action::SetWindowWidth { .. })))
            .map(|r| serde_json::to_string(r).expect("a Request serialises"))
            .collect()
    }

    /// Every action's wire bytes, in send order — the #1129 reflow-chain
    /// tests below need the whole action log, not just the resize half
    /// [`wire_bytes`] scopes itself to.
    fn all_action_wire_bytes(niri: &Fake) -> Vec<String> {
        niri.seen
            .iter()
            .filter(|r| matches!(r, Request::Action(_)))
            .map(|r| serde_json::to_string(r).expect("a Request serialises"))
            .collect()
    }

    /// The unit pinned at the **bytes**, not at a Rust enum: this is the exact
    /// line `niri msg action set-window-width --id 10 50%` writes to the socket.
    #[test]
    fn the_request_serialises_to_niris_own_percentage_bytes() {
        let mut niri = Fake::two_columns();

        apply(&mut niri, Layout::Split, None).expect("the fake answers everything");

        assert_eq!(
            wire_bytes(&niri),
            vec![
                r#"{"Action":{"SetWindowWidth":{"id":10,"change":{"SetProportion":50.0}}}}"#
                    .to_owned(),
                r#"{"Action":{"SetWindowWidth":{"id":20,"change":{"SetProportion":50.0}}}}"#
                    .to_owned(),
            ],
            "these are the bytes `niri msg action set-window-width --id N 50%` writes"
        );
    }

    /// Golden's two wide-pair numbers, at the bytes, as literals (#1019 round
    /// 2). The default `Fake` output reports no logical geometry, which is
    /// exactly the "unknown width" case [`crate::layout::golden_pair`]
    /// defaults to the wide pair for (#1052) — see
    /// `golden_narrow_serialises_to_sixty_one_point_eight_then_thirty_eight_point_two_percent`
    /// for the same pinning on a narrow output.
    ///
    /// Written out rather than built from `GOLDEN_WIDE_MAJOR * 100.0` for the
    /// reason the header above gives, and for a second one: `0.75_f64 * 100.0`
    /// is not *obviously* `75.0` — it is only exactly 75 because the rounding
    /// lands there, and 61.8 stayed `61.8` for the same non-obvious reason.
    /// Spelling the bytes is what proves it rather than assuming it; a change
    /// that made the product `75.00000000000001` would serialise those digits
    /// and fail here, where a `(id, f64)` comparison against the same
    /// expression would not.
    #[test]
    fn golden_serialises_to_seventy_five_then_twenty_five_percent() {
        let mut niri = Fake::with(vec![tile(10, 1), tile(20, 2), tile(30, 3)]);

        apply(&mut niri, Layout::Golden, None).expect("the fake answers everything");

        assert_eq!(
            wire_bytes(&niri),
            vec![
                r#"{"Action":{"SetWindowWidth":{"id":10,"change":{"SetProportion":75.0}}}}"#
                    .to_owned(),
                r#"{"Action":{"SetWindowWidth":{"id":20,"change":{"SetProportion":25.0}}}}"#
                    .to_owned(),
                r#"{"Action":{"SetWindowWidth":{"id":30,"change":{"SetProportion":25.0}}}}"#
                    .to_owned(),
            ],
            "these are the bytes `niri msg action set-window-width --id N 75%` \
             (then 25%, then 25%) writes"
        );
    }

    /// The golden cut's two numbers, at the bytes, as literals (#1052) — the
    /// same pinning as the wide pair above, on an output narrower than
    /// [`crate::layout::GOLDEN_BREAKPOINT`].
    #[test]
    fn golden_narrow_serialises_to_sixty_one_point_eight_then_thirty_eight_point_two_percent() {
        let mut niri = Fake::with_output_width(vec![tile(10, 1), tile(20, 2), tile(30, 3)], 1920);

        apply(&mut niri, Layout::Golden, None).expect("the fake answers everything");

        assert_eq!(
            wire_bytes(&niri),
            vec![
                r#"{"Action":{"SetWindowWidth":{"id":10,"change":{"SetProportion":61.8}}}}"#
                    .to_owned(),
                r#"{"Action":{"SetWindowWidth":{"id":20,"change":{"SetProportion":38.2}}}}"#
                    .to_owned(),
                r#"{"Action":{"SetWindowWidth":{"id":30,"change":{"SetProportion":38.2}}}}"#
                    .to_owned(),
            ],
            "these are the bytes `niri msg action set-window-width --id N 61.8%` \
             (then 38.2%, then 38.2%) writes on a 1920 px-wide output"
        );
    }

    #[test]
    fn queries_all_four_snapshots_before_acting() {
        let mut niri = Fake::with(vec![tile(10, 1)]);

        apply(&mut niri, Layout::Equal, None).expect("the fake answers everything");

        // Asserted against the **raw, unfiltered** `seen` log rather than
        // `niri.queries()` (#1056 review, NIT-1): `queries()` filters actions
        // out before this test ever sees the vec, so an action slipped in
        // between two snapshots would be invisible to it and the "before
        // acting" half of this test's name would be untested. Slicing the
        // first four entries of `seen` pins both the identity *and* the
        // position of each snapshot relative to everything else `apply` sends.
        let first_four: Vec<String> = niri.seen[..4].iter().map(|r| format!("{r:?}")).collect();
        assert_eq!(
            first_four,
            vec![
                "Windows".to_owned(),
                "Workspaces".to_owned(),
                "Outputs".to_owned(),
                "FocusedOutput".to_owned()
            ],
            "the plan needs all four (#1052 adds Outputs), and asks for them \
             before it sends anything — even for `equal`, which never reads it"
        );
    }

    // ── The clicked screen (#1050) ──────────────────────────────────────────

    /// The whole point of `Event.output`: a click on DP-2's copy of the chip
    /// lays out **DP-2's** active workspace, not the focused one.
    ///
    /// DP-1 is the focused output and holds windows 10/20; DP-2 holds 30/40/50.
    /// A regression that ignored the argument would resize 10 and 20 — the ids
    /// are what makes "wrong screen" and "right screen" different answers
    /// rather than the same proportions on different windows.
    #[test]
    fn a_named_output_lays_out_that_screens_active_workspace() {
        let mut niri = Fake::two_outputs();

        let applied = apply(&mut niri, Layout::Split, Some(fake::OTHER_OUTPUT))
            .expect("the fake answers everything");

        assert_eq!(applied, 3, "DP-2's workspace has three columns");
        assert_eq!(
            niri.widths(),
            vec![(30, 50.0), (40, 50.0), (50, 50.0)],
            "DP-2's own windows, not the focused screen's"
        );
    }

    /// …and `None` — the CLI hat, and an event the host could not attribute —
    /// keeps the pre-#1050 behaviour exactly: the focused output.
    #[test]
    fn no_named_output_falls_back_to_the_focused_one() {
        let mut niri = Fake::two_outputs();

        let applied = apply(&mut niri, Layout::Split, None).expect("the fake answers everything");

        assert_eq!(applied, 2, "DP-1 is focused and has two columns");
        assert_eq!(niri.widths(), vec![(10, 50.0), (20, 50.0)]);
    }

    /// A named screen makes the `FocusedOutput` round trip pointless, so it is
    /// not sent — one fewer unix-socket connect per click.
    #[test]
    fn a_named_output_skips_the_focused_output_round_trip() {
        let mut niri = Fake::two_outputs();

        apply(&mut niri, Layout::Equal, Some(fake::OTHER_OUTPUT))
            .expect("the fake answers everything");

        assert_eq!(
            niri.queries(),
            vec![
                "Windows".to_owned(),
                "Workspaces".to_owned(),
                "Outputs".to_owned()
            ],
            "the answer would have been discarded"
        );
    }

    /// A connector niri has no workspace on — a screen unplugged between the
    /// render and the click — resolves to no target workspace, so nothing is
    /// resized. Never "fall back to the focused screen", which would lay out a
    /// monitor the human was not pointing at.
    #[test]
    fn a_connector_niri_does_not_know_lays_out_nothing() {
        let mut niri = Fake::two_outputs();

        let applied =
            apply(&mut niri, Layout::Split, Some("DP-99")).expect("the fake answers everything");

        assert_eq!(applied, 0);
        assert!(
            niri.widths().is_empty(),
            "no screen was named, so no screen is resized: {:?}",
            niri.widths()
        );
    }

    /// Golden's per-screen pair (#1052) is resolved off the **clicked** screen
    /// too, not the focused one: the width lookup goes through the same target
    /// workspace the planner uses.
    #[test]
    fn golden_resolves_its_pair_from_the_clicked_screens_width() {
        let mut niri = Fake::two_outputs();
        // A wide focused screen and a narrow second one — so the two arms give
        // visibly different numbers.
        niri.outputs.insert(
            fake::OUTPUT.to_owned(),
            fake::output_with_logical_width(3440),
        );
        niri.outputs.insert(
            fake::OTHER_OUTPUT.to_owned(),
            fake::other_output_with_logical_width(1920),
        );

        apply(&mut niri, Layout::Golden, Some(fake::OTHER_OUTPUT))
            .expect("the fake answers everything");

        assert_eq!(
            niri.widths(),
            vec![(30, 61.8), (40, 38.2), (50, 38.2)],
            "1920 px is below the breakpoint, so the clicked screen gets the \
             golden cut — the focused screen's 3440 px must not decide it"
        );
    }

    // ── The width-based Golden pair (#1052) ─────────────────────────────────

    #[test]
    fn golden_uses_the_wide_pair_at_or_above_the_breakpoint() {
        let mut niri = Fake::with_output_width(vec![tile(10, 1), tile(20, 2)], 3440);

        apply(&mut niri, Layout::Golden, None).expect("the fake answers everything");

        assert_eq!(
            niri.widths(),
            vec![(10, 75.0), (20, 25.0)],
            "3440 logical px is >= 2560, so it gets the wide pair"
        );
    }

    #[test]
    fn golden_uses_the_golden_cut_below_the_breakpoint() {
        let mut niri = Fake::with_output_width(vec![tile(10, 1), tile(20, 2)], 1920);

        apply(&mut niri, Layout::Golden, None).expect("the fake answers everything");

        assert_eq!(
            niri.widths(),
            vec![(10, 61.8), (20, 38.2)],
            "1920 logical px is < 2560, so it gets the golden ratio cut"
        );
    }

    /// Only Golden's pair depends on the output width — `equal` and `split`
    /// must send the exact same widths on a narrow output as on a wide one.
    #[test]
    fn equal_and_split_ignore_the_output_width() {
        let mut narrow = Fake::with_output_width(vec![tile(10, 1), tile(20, 2)], 1920);
        let mut wide = Fake::with_output_width(vec![tile(10, 1), tile(20, 2)], 3440);

        apply(&mut narrow, Layout::Equal, None).expect("narrow, equal");
        apply(&mut wide, Layout::Equal, None).expect("wide, equal");
        assert_eq!(narrow.widths(), wide.widths());
        // Pinned against the literal too (#1056 review, NIT-3): comparing
        // narrow against wide alone would also pass if both were wrong in the
        // same way (`Equal` and `Split` coincide at two columns — see
        // `layout::tests::equal_and_split_coincide_at_two_columns`), so this
        // half of the test was really comparing one value against itself.
        assert_eq!(
            narrow.widths(),
            vec![(10, 50.0), (20, 50.0)],
            "two equal columns are 50 % each, regardless of output width"
        );

        let mut narrow = Fake::with_output_width(vec![tile(10, 1), tile(20, 2)], 1920);
        let mut wide = Fake::with_output_width(vec![tile(10, 1), tile(20, 2)], 3440);
        apply(&mut narrow, Layout::Split, None).expect("narrow, split");
        apply(&mut wide, Layout::Split, None).expect("wide, split");
        assert_eq!(narrow.widths(), wide.widths());
        assert_eq!(
            narrow.widths(),
            vec![(10, 50.0), (20, 50.0)],
            "split at two columns is also 50 % each — same literal pin as above"
        );
    }

    /// "Missing output" (#1052): the target output isn't in what `Outputs`
    /// reported at all — as opposed to being reported with no logical
    /// geometry, which `golden_serialises_to_seventy_five_then_twenty_five_percent`
    /// already covers via the default `Fake`.
    #[test]
    fn golden_falls_back_to_the_wide_pair_when_the_output_is_missing_entirely() {
        let mut niri = Fake::two_columns();
        niri.outputs.clear();

        let applied = apply(&mut niri, Layout::Golden, None).expect("querying still works");

        assert_eq!(applied, 2);
        assert_eq!(niri.widths(), vec![(10, 75.0), (20, 25.0)]);
    }

    /// The diagnostic (#1056 review, LOW-1) that goes with the fallback
    /// above: unpinned before this, so a `Transport::log` call that silently
    /// stopped firing — or drifted from the fallback pair it names — would
    /// have shipped unnoticed. `missing_width_diagnostic` builds the exact
    /// line from [`crate::layout::GOLDEN_WIDE_MAJOR`]/`GOLDEN_WIDE_MINOR`, so
    /// this also guards against the message naming a stale pair.
    #[test]
    fn golden_logs_the_fallback_pair_from_the_constants_when_width_is_unknown() {
        let mut niri = Fake::two_columns();
        niri.outputs.clear();

        apply(&mut niri, Layout::Golden, None).expect("querying still works");

        assert_eq!(
            niri.logs,
            vec![missing_width_diagnostic(Some(fake::OUTPUT))],
            "one diagnostic line, naming the target output and the fallback \
             pair actually sent"
        );
    }

    #[test]
    fn golden_asks_outputs_exactly_once_per_apply() {
        let mut niri = Fake::with_output_width(vec![tile(10, 1), tile(20, 2), tile(30, 3)], 1920);

        apply(&mut niri, Layout::Golden, None).expect("the fake answers everything");

        assert_eq!(
            niri.queries().iter().filter(|q| *q == "Outputs").count(),
            1,
            "one Outputs request per apply, not one per column"
        );
    }

    // ── `apply`'s own targeting invariant (#1056 review, MED-2) ─────────────
    //
    // `apply` resolves Golden's width through `layout::target_output_name`,
    // which the PR body calls "the same rule `plan()` uses internally, so the
    // two can never pick different workspaces" — but nothing wired that
    // invariant to a test at the `apply` level before this. The obvious wrong
    // refactor (`output.as_deref()` instead of `target_output_name(&workspaces,
    // output.as_deref())`) passed the whole suite: `target_output_name`'s own
    // fallback behaviour was unit-tested in isolation
    // (`layout::tests::target_output_name_falls_back_like_plan_does`) but
    // never exercised through `apply` itself.

    /// With no focused output niri-side, `plan` falls back to the globally
    /// focused workspace — so the *width* has to be resolved through that
    /// workspace's own output too, not off the (absent) focused-output name.
    /// Reddens against `let target_output = output.as_deref();` in place of
    /// `layout::target_output_name(&workspaces, output.as_deref())`.
    #[test]
    fn golden_resolves_the_width_through_the_workspace_when_no_output_is_focused() {
        let mut niri = Fake::with_output_width(vec![tile(10, 1), tile(20, 2)], 1920);
        niri.focused_output = None;

        apply(&mut niri, Layout::Golden, None).expect("the fallback path");

        assert_eq!(
            niri.widths(),
            vec![(10, 61.8), (20, 38.2)],
            "the target workspace still sits on a 1920 px output, so it gets the golden cut"
        );
    }

    /// Same invariant, but with two *different* widths in play so a
    /// wrong-**name** resolution (M7-shaped: resolving off `output` instead
    /// of `target_output_name`) actually diverges from the right answer —
    /// the single-output fixture above can't tell "resolved through the
    /// right name" apart from "resolved through the wrong one" when both
    /// names map to the same width. This is deterministic regardless of
    /// [`outputs`]'s `HashMap`-vs-`Vec` shape (#1056 review, NIT-2): a
    /// keyed lookup by the *correct* name doesn't care what order the
    /// output map iterates in. NIT-2 buys something narrower but real — a
    /// mutant that also drops the name comparison (e.g. "take any output")
    /// stays only probabilistically caught, because it inherits whatever
    /// order the map's own randomised hasher produces; keying by name at
    /// least removes the *assumption* that the map key and `Output.name`
    /// never diverge, and the O(n) scan.
    #[test]
    fn golden_resolves_the_width_through_the_focused_workspaces_own_output_with_two_outputs() {
        let mut niri = Fake::two_columns();
        niri.outputs = HashMap::from([
            (
                fake::OUTPUT.to_owned(),
                fake::output_with_logical_width(1920),
            ),
            ("HDMI-A-1".to_owned(), fake::output_with_logical_width(3440)),
        ]);

        apply(&mut niri, Layout::Golden, None).expect("the fake answers everything");

        assert_eq!(
            niri.widths(),
            vec![(10, 61.8), (20, 38.2)],
            "the focused workspace sits on fake::OUTPUT (1920 px), not HDMI-A-1 (3440 px)"
        );
    }

    #[test]
    fn an_empty_workspace_sends_no_action_at_all() {
        let mut niri = Fake::with(Vec::new());

        let applied = apply(&mut niri, Layout::Split, None).expect("querying still works");

        assert_eq!(applied, 0, "reported as a no-op, not an error");
        assert!(niri.widths().is_empty(), "and nothing was sent");
        // #1129: "no action at all" now also has to cover the reflow chain —
        // a workspace with no tiled column has no first column to nudge, so
        // no `FocusWindow`/`MoveColumnToFirst`/`MoveColumnToLast` either.
        assert!(
            all_action_wire_bytes(&niri).is_empty(),
            "an empty workspace must send no Action request whatsoever, resize \
             or reflow: {:?}",
            all_action_wire_bytes(&niri)
        );
    }

    // ── Post-resize niri reflow nudge (#1129) ───────────────────────────────

    /// The chain `apply` sends after resizing columns, mirroring
    /// `hytte_services::niri::reflow_over`'s verified sequence: focus the
    /// leftmost column, move it to the end and back to the start twice, then
    /// restore whatever window was actually focused before `apply` ran — not
    /// the window the nudge itself focused.
    #[test]
    fn apply_ends_with_the_reflow_chain_and_restores_the_original_focus() {
        let mut focused = tile(20, 2);
        focused.is_focused = true;
        let mut niri = Fake::with(vec![tile(10, 1), focused, tile(30, 3)]);

        apply(&mut niri, Layout::Equal, None).expect("the fake answers everything");

        let actions = all_action_wire_bytes(&niri);
        assert_eq!(
            actions[actions.len() - 5..],
            [
                r#"{"Action":{"FocusWindow":{"id":10}}}"#.to_owned(),
                r#"{"Action":{"MoveColumnToFirst":{}}}"#.to_owned(),
                r#"{"Action":{"MoveColumnToLast":{}}}"#.to_owned(),
                r#"{"Action":{"MoveColumnToFirst":{}}}"#.to_owned(),
                r#"{"Action":{"FocusWindow":{"id":20}}}"#.to_owned(),
            ],
            "must end on the verified chain, restoring focus to window 20 (the \
             one actually focused), not window 10 (the one the nudge itself \
             focused): {actions:?}"
        );
    }

    /// Mutation guard: dropping the reflow tail (or the restore step alone)
    /// must redden. `apply` must not stop at the resize half.
    ///
    /// It is also `restore_focus`'s back-and-forth guard. Here the focused
    /// workspace is the one the nudge focused, and it names no active window.
    /// So there is nothing to restore to, and a `FocusWorkspace` on it would be
    /// a switch to the *previous* workspace under niri's
    /// `workspace-auto-back-and-forth`. **Mutation:** drop the `!= nudged`
    /// filter and a `FocusWorkspace` lands at the end of this chain.
    #[test]
    fn apply_reflow_chain_has_no_restore_step_when_nothing_is_focused() {
        let mut niri = Fake::two_columns(); // both windows unfocused

        apply(&mut niri, Layout::Split, None).expect("the fake answers everything");

        assert_eq!(
            all_action_wire_bytes(&niri),
            vec![
                r#"{"Action":{"SetWindowWidth":{"id":10,"change":{"SetProportion":50.0}}}}"#
                    .to_owned(),
                r#"{"Action":{"SetWindowWidth":{"id":20,"change":{"SetProportion":50.0}}}}"#
                    .to_owned(),
                r#"{"Action":{"FocusWindow":{"id":10}}}"#.to_owned(),
                r#"{"Action":{"MoveColumnToFirst":{}}}"#.to_owned(),
                r#"{"Action":{"MoveColumnToLast":{}}}"#.to_owned(),
                r#"{"Action":{"MoveColumnToFirst":{}}}"#.to_owned(),
            ],
            "no window is focused, so the chain must end on the move back to \
             first — no trailing FocusWindow restore step"
        );
    }

    /// Two columns with window 20 focused. The `Action` positions are 0–1 for
    /// the resizes, then 2 `FocusWindow{10}`, 3 `MoveColumnToFirst`,
    /// 4 `MoveColumnToLast`, 5 `MoveColumnToFirst`, then the restore.
    fn focused_two_columns() -> Fake {
        let mut focused = tile(20, 2);
        focused.is_focused = true;
        Fake::with(vec![tile(10, 1), focused])
    }

    /// The #1129 chain is not atomic, so whichever move niri refuses, the
    /// column goes back to the front and focus goes back to window 20. The
    /// error is still niri's own text for the refused step.
    ///
    /// Position 5 is the case that leaves real damage. `MoveColumnToLast`
    /// landed, so before this the leftmost column stayed at the far end with
    /// focus stolen onto it.
    #[test]
    fn a_reflow_refused_partway_still_puts_the_column_and_focus_back() {
        for (position, step) in [
            (3, "MoveColumnToFirst"),
            (4, "MoveColumnToLast"),
            (5, "MoveColumnToFirst"),
        ] {
            let mut niri = focused_two_columns();
            niri.failing_actions
                .insert(position, format!("refused {step}"));

            let err = apply(&mut niri, Layout::Split, None).expect_err("niri refused a move");

            assert_eq!(err, format!("refused {step}"), "position {position}");
            let actions = all_action_wire_bytes(&niri);
            assert_eq!(
                actions.len(),
                position + 1 + 2,
                "position {position}: the chain stops at the refusal, then \
                 exactly two put-back steps: {actions:?}"
            );
            assert_eq!(
                actions[actions.len() - 2..],
                [
                    r#"{"Action":{"MoveColumnToFirst":{}}}"#.to_owned(),
                    r#"{"Action":{"FocusWindow":{"id":20}}}"#.to_owned(),
                ],
                "position {position}: one more move to the front, then the \
                 original focus: {actions:?}"
            );
        }
    }

    /// A refused `FocusWindow` on the leftmost column has moved nothing, focus
    /// included, so nothing follows it: no move, and no focus "restore".
    #[test]
    fn a_refused_first_focus_sends_nothing_more() {
        let mut niri = focused_two_columns();
        niri.failing_actions.insert(2, "no such window".to_owned());

        let err = apply(&mut niri, Layout::Split, None).expect_err("niri refused the focus");

        assert_eq!(err, "no such window");
        let actions = all_action_wire_bytes(&niri);
        assert_eq!(
            actions.len(),
            3,
            "the two resizes and the refused focus, nothing after: {actions:?}"
        );
    }

    /// The moves landed but the restore was refused: that refusal is the
    /// error, since it is the first one.
    #[test]
    fn a_refused_restore_is_the_error_when_the_moves_landed() {
        let mut niri = focused_two_columns();
        niri.failing_actions
            .insert(6, "refused FocusWindow".to_owned());

        let err = apply(&mut niri, Layout::Split, None).expect_err("niri refused the restore");

        assert_eq!(err, "refused FocusWindow");
    }

    /// A move *and* the restore refused: the move's error is returned (it came
    /// first), and the restore's is logged rather than dropped.
    #[test]
    fn the_first_reflow_error_wins_and_a_later_one_is_logged() {
        let mut niri = focused_two_columns();
        niri.failing_actions
            .insert(4, "refused MoveColumnToLast".to_owned());
        // 5 is the put-back `MoveColumnToFirst`, 6 the restore.
        niri.failing_actions
            .insert(6, "refused FocusWindow".to_owned());

        let err = apply(&mut niri, Layout::Split, None).expect_err("niri refused twice");

        assert_eq!(err, "refused MoveColumnToLast");
        assert!(
            niri.logs
                .iter()
                .any(|line| line.contains("refused FocusWindow")),
            "the later error must reach the log: {:?}",
            niri.logs
        );
    }

    /// Focus on an **empty** workspace on one screen, and a click on the other
    /// screen's chip. The nudge's `FocusWindow` moves focus to the clicked
    /// screen. With no window to refocus, the restore used to send nothing and
    /// focus stayed there. Now it focuses that workspace again, by id.
    #[test]
    fn focus_on_an_empty_workspace_elsewhere_is_restored_by_workspace_id() {
        let mut niri = Fake::with(vec![fake::tile_on(30, 2, 1), fake::tile_on(40, 2, 2)]);
        niri.workspaces = vec![
            fake::workspace_on(1, fake::OUTPUT, true),
            fake::workspace_on(2, fake::OTHER_OUTPUT, false),
        ];
        niri.outputs
            .insert(fake::OTHER_OUTPUT.to_owned(), fake::other_output());

        apply(&mut niri, Layout::Split, Some(fake::OTHER_OUTPUT))
            .expect("the fake answers everything");

        let actions = all_action_wire_bytes(&niri);
        assert_eq!(
            actions.last(),
            Some(&r#"{"Action":{"FocusWorkspace":{"reference":{"Id":1}}}}"#.to_owned()),
            "focus goes back to the empty workspace on DP-1: {actions:?}"
        );
    }

    /// No window has focus, but the focused workspace names an active one. That
    /// is keyboard focus on a layer-shell surface, the shell's drawer among
    /// them. The nudge made the leftmost column active, so the restore makes
    /// the workspace's own active column active again.
    #[test]
    fn a_workspaces_active_window_is_restored_when_no_window_has_focus() {
        let mut niri = Fake::with(vec![tile(10, 1), tile(20, 2), tile(30, 3)]);
        niri.workspaces[0].active_window_id = Some(30);

        apply(&mut niri, Layout::Equal, None).expect("the fake answers everything");

        let actions = all_action_wire_bytes(&niri);
        assert_eq!(
            actions.last(),
            Some(&r#"{"Action":{"FocusWindow":{"id":30}}}"#.to_owned()),
            "the workspace's active column, not the leftmost one: {actions:?}"
        );
    }

    #[test]
    fn a_niri_refusal_surfaces_its_own_text_verbatim() {
        let mut niri = Fake::two_columns();
        niri.action_error = Some("unknown action SetWindowWidth".to_owned());

        let err = apply(&mut niri, Layout::Equal, None).expect_err("niri refused the first action");

        assert_eq!(err, "unknown action SetWindowWidth");
        assert_eq!(
            niri.widths().len(),
            1,
            "and the walk stops at the first failure rather than hammering on"
        );
    }

    #[test]
    fn a_transport_failure_surfaces_too() {
        let mut niri = Fake::with(vec![tile(10, 1)]);
        niri.transport_error = Some("cannot reach niri over $NIRI_SOCKET: no such file".to_owned());

        let err = apply(&mut niri, Layout::Equal, None).expect_err("the socket is gone");

        assert!(err.contains("$NIRI_SOCKET"), "got {err:?}");
    }

    #[test]
    fn an_unexpected_response_shape_is_an_error_not_a_panic() {
        struct Confused;
        impl Transport for Confused {
            fn send(&mut self, _request: Request) -> Result<Reply, String> {
                Ok(Ok(Response::Handled))
            }

            fn log(&mut self, _line: &str) {}
        }

        let err =
            apply(&mut Confused, Layout::Equal, None).expect_err("Handled is not a window list");

        assert!(err.contains("Windows"), "got {err:?}");
    }

    #[test]
    fn no_focused_output_still_plans_off_the_focused_workspace() {
        let mut niri = Fake::two_columns();
        niri.focused_output = None;

        let applied = apply(&mut niri, Layout::Split, None).expect("the fallback path");

        assert_eq!(applied, 2);
        assert_eq!(niri.widths(), vec![(10, 50.0), (20, 50.0)]);
    }
}
