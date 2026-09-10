//! The niri IPC half: a one-request/one-reply [`Transport`] seam and the
//! [`apply`] orchestration over it.
//!
//! [`apply`] is the single entry point both hats use — the chip's click worker
//! and the CLI's `apply` subcommand — so the two can never drift. It fetches the
//! three snapshots [`plan`](crate::layout::plan) needs, then sends one
//! `SetWindowWidth` per column. All the deciding lives in
//! [`layout`](crate::layout); this module only moves bytes.
//!
//! The [`Transport`] indirection is what makes the whole path testable: the
//! tests below drive [`apply`] against a scripted fake and never open a socket.

use crate::layout::{self, Layout};
use niri_ipc::socket::Socket;
use niri_ipc::{Action, Reply, Request, Response, SizeChange, Window, Workspace};

/// One niri request, one niri reply.
///
/// `Err` is a *transport* failure (no `$NIRI_SOCKET`, the socket went away, a
/// reply that would not parse). A niri-level refusal arrives as the inner
/// `Err(String)` of [`Reply`] and carries niri's own error text, which is what
/// [`apply`] surfaces verbatim.
pub(crate) trait Transport {
    fn send(&mut self, request: Request) -> Result<Reply, String>;
}

/// The real transport: one short-lived `$NIRI_SOCKET` connection per request.
///
/// A fresh connect per request rather than one long-lived socket, matching the
/// shell's own command path (`hytte-services`' `niri::send_action`): a unix
/// socket connect is cheap, and niri's IPC only guarantees one reply per
/// connection for a non-`EventStream` request.
pub(crate) struct SocketTransport;

impl Transport for SocketTransport {
    fn send(&mut self, request: Request) -> Result<Reply, String> {
        let mut socket =
            Socket::connect().map_err(|e| format!("cannot reach niri over $NIRI_SOCKET: {e}"))?;
        socket
            .send(request)
            .map_err(|e| format!("niri ipc failed: {e}"))
    }
}

/// Send one request and unwrap both error layers: the transport's, and niri's
/// own message (kept verbatim — it is the most useful thing to show a human).
fn ask(transport: &mut impl Transport, request: Request) -> Result<Response, String> {
    transport.send(request)?
}

/// Apply `layout` to the focused workspace's columns.
///
/// Returns how many columns were resized — `Ok(0)` means the focused workspace
/// held no tiled columns, which is a no-op and not an error. `Err` carries
/// niri's own text for the first request that failed; nothing is retried and no
/// widths are rolled back, because a partially applied layout is still a
/// coherent one and the next click fixes it.
pub(crate) fn apply(transport: &mut impl Transport, layout: Layout) -> Result<usize, String> {
    let windows = windows(transport)?;
    let workspaces = workspaces(transport)?;
    let output = focused_output(transport)?;

    let plan = layout::plan(&windows, &workspaces, output.as_deref(), layout);
    for &(id, proportion) in &plan {
        ask(
            transport,
            Request::Action(Action::SetWindowWidth {
                // Always by id: `None` would mean "the focused window", which is
                // exactly one of the n columns we are walking.
                id: Some(id),
                change: SizeChange::SetProportion(proportion),
            }),
        )?;
    }
    Ok(plan.len())
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
        Action, Output, Reply, Request, Response, SizeChange, Window, WindowLayout, Workspace,
    };

    pub(crate) const OUTPUT: &str = "DP-1";

    pub(crate) fn workspace() -> Workspace {
        Workspace {
            id: 1,
            idx: 1,
            name: None,
            output: Some(OUTPUT.to_owned()),
            is_urgent: false,
            is_active: true,
            is_focused: true,
            active_window_id: None,
        }
    }

    /// A tiled window alone in column `column` of workspace 1.
    pub(crate) fn tile(id: u64, column: usize) -> Window {
        Window {
            id,
            title: None,
            app_id: None,
            pid: None,
            workspace_id: Some(1),
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

    /// Answers the three queries from canned state and records every request.
    pub(crate) struct Fake {
        pub(crate) windows: Vec<Window>,
        pub(crate) workspaces: Vec<Workspace>,
        pub(crate) focused_output: Option<Output>,
        /// Fail the next `Action` the way niri itself would: an inner `Err`
        /// carrying niri's own text.
        pub(crate) action_error: Option<String>,
        /// Fail every request at the transport layer (no socket at all).
        pub(crate) transport_error: Option<String>,
        pub(crate) seen: Vec<Request>,
    }

    impl Fake {
        pub(crate) fn with(windows: Vec<Window>) -> Self {
            Self {
                windows,
                workspaces: vec![workspace()],
                focused_output: Some(output()),
                action_error: None,
                transport_error: None,
                seen: Vec::new(),
            }
        }

        /// Two tiled columns on the focused workspace — the happy default.
        pub(crate) fn two_columns() -> Self {
            Self::with(vec![tile(10, 1), tile(20, 2)])
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
                Request::FocusedOutput => Ok(Response::FocusedOutput(self.focused_output.clone())),
                Request::Action(_) => match self.action_error.take() {
                    Some(msg) => Err(msg),
                    None => Ok(Response::Handled),
                },
                other => Err(format!("fake got an unscripted request: {other:?}")),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Fake, tile};
    use super::{Transport, apply};
    use crate::layout::{GOLDEN_MAJOR, GOLDEN_MINOR, Layout};
    use niri_ipc::{Reply, Request, Response};

    #[test]
    fn sends_one_set_width_per_column_left_to_right() {
        let mut niri = Fake::with(vec![tile(10, 1), tile(20, 2), tile(30, 3)]);

        let applied = apply(&mut niri, Layout::Golden).expect("the fake answers everything");

        assert_eq!(applied, 3);
        assert_eq!(
            niri.widths(),
            vec![(10, GOLDEN_MAJOR), (20, GOLDEN_MINOR), (30, GOLDEN_MINOR)]
        );
    }

    #[test]
    fn queries_all_three_snapshots_before_acting() {
        let mut niri = Fake::with(vec![tile(10, 1)]);

        apply(&mut niri, Layout::Equal).expect("the fake answers everything");

        assert_eq!(
            niri.queries(),
            vec![
                "Windows".to_owned(),
                "Workspaces".to_owned(),
                "FocusedOutput".to_owned()
            ],
            "the plan needs all three, and asks for them before it sends anything"
        );
    }

    #[test]
    fn an_empty_workspace_sends_no_action_at_all() {
        let mut niri = Fake::with(Vec::new());

        let applied = apply(&mut niri, Layout::Split).expect("querying still works");

        assert_eq!(applied, 0, "reported as a no-op, not an error");
        assert!(niri.widths().is_empty(), "and nothing was sent");
    }

    #[test]
    fn a_niri_refusal_surfaces_its_own_text_verbatim() {
        let mut niri = Fake::two_columns();
        niri.action_error = Some("unknown action SetWindowWidth".to_owned());

        let err = apply(&mut niri, Layout::Equal).expect_err("niri refused the first action");

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

        let err = apply(&mut niri, Layout::Equal).expect_err("the socket is gone");

        assert!(err.contains("$NIRI_SOCKET"), "got {err:?}");
    }

    #[test]
    fn an_unexpected_response_shape_is_an_error_not_a_panic() {
        struct Confused;
        impl Transport for Confused {
            fn send(&mut self, _request: Request) -> Result<Reply, String> {
                Ok(Ok(Response::Handled))
            }
        }

        let err = apply(&mut Confused, Layout::Equal).expect_err("Handled is not a window list");

        assert!(err.contains("Windows"), "got {err:?}");
    }

    #[test]
    fn no_focused_output_still_plans_off_the_focused_workspace() {
        let mut niri = Fake::two_columns();
        niri.focused_output = None;

        let applied = apply(&mut niri, Layout::Split).expect("the fallback path");

        assert_eq!(applied, 2);
        assert_eq!(niri.widths(), vec![(10, 0.5), (20, 0.5)]);
    }
}
