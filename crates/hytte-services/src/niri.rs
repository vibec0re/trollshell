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

use anyhow::{anyhow, Context, Result};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, runtime, Service};
use niri_ipc::{
    socket::Socket, Action, Event, Request, Response, WorkspaceReferenceArg,
};
use std::thread;
use std::time::Duration;

// Re-export the niri-ipc data types consumers need so trollshell etc.
// don't have to depend on niri-ipc directly.
pub use niri_ipc::{Window, WindowLayout, Workspace};

/// The niri IPC service handle.
pub struct NiriService;

/// Internal handles holding the reactive state.
#[doc(hidden)]
pub struct NiriHandles {
    pub(crate) workspaces: Mutable<Vec<Workspace>>,
    pub(crate) windows: Mutable<Vec<Window>>,
    pub(crate) focused_window: Mutable<Option<Window>>,
}

impl Default for NiriHandles {
    fn default() -> Self {
        Self {
            workspaces: Mutable::new(Vec::new()),
            windows: Mutable::new(Vec::new()),
            focused_window: Mutable::new(None),
        }
    }
}

impl Service for NiriService {
    type Handles = NiriHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NiriHandles::default();
        let ws_writer = handles.workspaces.clone();
        let win_list_writer = handles.windows.clone();
        let win_focus_writer = handles.focused_window.clone();

        rt.spawn_blocking(move || loop {
            match listen_once(&ws_writer, &win_list_writer, &win_focus_writer) {
                Ok(()) => tracing::warn!("niri event stream closed, reconnecting in 1s"),
                Err(e) => tracing::warn!(error = ?e, "niri ipc error, reconnecting in 1s"),
            }
            thread::sleep(Duration::from_secs(1));
        });

        handles
    }
}

fn listen_once(
    workspaces: &Mutable<Vec<Workspace>>,
    windows: &Mutable<Vec<Window>>,
    focused_window: &Mutable<Option<Window>>,
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
        apply_event(event, workspaces, windows, focused_window);
    }
}

fn apply_event(
    event: Event,
    workspaces: &Mutable<Vec<Workspace>>,
    windows: &Mutable<Vec<Window>>,
    focused_window: &Mutable<Option<Window>>,
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
pub fn edge_window_on(connector: String, mon_w: f64) -> impl Signal<Item = bool> {
    use futures_signals::map_ref;
    let workspaces = workspaces();
    let windows = windows();
    map_ref! {
        let ws = workspaces,
        let w = windows =>
        has_edge_window(ws, w, &connector, mon_w)
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
        windows.iter().any(|w| {
            w.workspace_id == Some(id) && w.layout.tile_size.0 >= mon_w - EDGE_TOL
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
    runtime::handle().spawn_blocking(move || {
        match Socket::connect() {
            Ok(mut sock) => {
                if let Err(e) = sock.send(Request::Action(action)) {
                    tracing::warn!(error = %e, "niri action send failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "niri socket open for action failed"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
