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
//! - `Window` does not derive `Default`, so `WindowFocusChanged { id }` is
//!   handled by updating the cached window list and filtering for `is_focused`.

use anyhow::{anyhow, Context, Result};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use niri_ipc::{socket::Socket, Event, Request, Response, Window, Workspace};
use std::thread;
use std::time::Duration;

/// The niri IPC service handle.
pub struct NiriService;

/// Internal handles holding the reactive state.
#[doc(hidden)]
pub struct NiriHandles {
    pub(crate) workspaces: Mutable<Vec<Workspace>>,
    pub(crate) focused_window: Mutable<Option<Window>>,
}

impl Default for NiriHandles {
    fn default() -> Self {
        Self {
            workspaces: Mutable::new(Vec::new()),
            focused_window: Mutable::new(None),
        }
    }
}

impl Service for NiriService {
    type Handles = NiriHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NiriHandles::default();
        let ws_writer = handles.workspaces.clone();
        let win_writer = handles.focused_window.clone();

        // niri-ipc Socket is sync; isolate it on a dedicated blocking thread.
        rt.spawn_blocking(move || loop {
            match listen_once(&ws_writer, &win_writer) {
                Ok(()) => {
                    tracing::warn!("niri event stream closed, reconnecting in 1s");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "niri ipc error, reconnecting in 1s");
                }
            }
            thread::sleep(Duration::from_secs(1));
        });

        handles
    }
}

fn listen_once(
    workspaces: &Mutable<Vec<Workspace>>,
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

    // read_events() consumes the socket and returns a blocking FnMut closure.
    let mut read_event = socket.read_events();

    // Maintain a local window cache so that WindowFocusChanged (which only
    // carries an id) can be resolved to a full Window.
    let mut window_cache: Vec<Window> = Vec::new();

    loop {
        let event = read_event().map_err(|e| anyhow!("read niri event: {e}"))?;
        apply_event(event, workspaces, focused_window, &mut window_cache);
    }
}

fn apply_event(
    event: Event,
    workspaces: &Mutable<Vec<Workspace>>,
    focused_window: &Mutable<Option<Window>>,
    window_cache: &mut Vec<Window>,
) {
    match event {
        Event::WorkspacesChanged { workspaces: ws } => {
            workspaces.set(ws);
        }
        Event::WorkspaceActivated { id, focused } => {
            workspaces.lock_mut().iter_mut().for_each(|w| {
                if w.id == id {
                    w.is_active = true;
                    if focused {
                        w.is_focused = true;
                    }
                } else if focused {
                    w.is_focused = false;
                }
            });
        }
        Event::WindowsChanged { windows } => {
            // Full replacement of the window list. Update cache and focused window.
            let focused = windows.iter().find(|w| w.is_focused).cloned();
            focused_window.set(focused);
            *window_cache = windows;
        }
        Event::WindowOpenedOrChanged { window } => {
            // Update or insert into cache.
            if window.is_focused {
                focused_window.set(Some(window.clone()));
            }
            if let Some(existing) = window_cache.iter_mut().find(|w| w.id == window.id) {
                *existing = window;
            } else {
                window_cache.push(window);
            }
        }
        Event::WindowClosed { id } => {
            window_cache.retain(|w| w.id != id);
            // If the closed window was focused, clear the focused window.
            let currently_focused = focused_window.lock_ref();
            if currently_focused.as_ref().map(|w| w.id) == Some(id) {
                drop(currently_focused);
                focused_window.set(None);
            }
        }
        Event::WindowFocusChanged { id } => {
            let new_focused = id.and_then(|id| window_cache.iter().find(|w| w.id == id).cloned());
            focused_window.set(new_focused);
        }
        // TODO(v0.2): handle WorkspaceUrgencyChanged, KeyboardLayoutSwitched, etc.
        _ => {}
    }
}

/// Returns the niri service to register with the hytte runtime.
#[must_use]
pub fn service() -> NiriService {
    NiriService
}

/// Returns a signal that emits the current list of niri workspaces.
pub fn workspaces() -> impl Signal<Item = Vec<Workspace>> {
    registry::with(|r| {
        r.get::<NiriHandles>()
            .expect("niri::service() not registered")
            .workspaces
            .signal_cloned()
    })
}

/// Returns a signal that emits the currently focused window, if any.
pub fn focused_window() -> impl Signal<Item = Option<Window>> {
    registry::with(|r| {
        r.get::<NiriHandles>()
            .expect("niri::service() not registered")
            .focused_window
            .signal_cloned()
    })
}
