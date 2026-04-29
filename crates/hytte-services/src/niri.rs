//! Niri compositor IPC client.
//!
//! Uses the synchronous `niri_ipc::socket::Socket` from a dedicated
//! `spawn_blocking` task on the tokio runtime. Niri's IPC is line-based
//! JSON over a unix socket; the `niri-ipc` crate handles the framing and
//! event deserialisation.
//!
//! On connection loss the loop sleeps 1s then reconnects.
//!
//! # API notes (niri-ipc 25.11.0)
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
pub use niri_ipc::{Window, Workspace};

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
                Err(e) => tracing::warn!(error = %e, "niri ipc error, reconnecting in 1s"),
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
        // TODO(v0.3+): handle WorkspaceUrgencyChanged, KeyboardLayoutSwitched, etc.
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
