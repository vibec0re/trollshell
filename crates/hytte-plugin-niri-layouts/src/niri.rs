//! The niri IPC half: a one-request/one-reply [`Transport`] seam and the
//! [`apply`] orchestration over it.
//!
//! [`apply`] is the single entry point both hats use — the chip's click worker
//! and the CLI's `apply` subcommand — so the two can never drift. It fetches the
//! snapshots [`plan`](crate::layout::plan) and [`Layout::Golden`]'s pair
//! resolution need — windows, workspaces, outputs, and (only when the caller
//! did not name a screen itself, #1050) the focused output — then sends one
//! `SetWindowWidth` per column. All the deciding lives in
//! [`layout`](crate::layout); this module only moves bytes (plus one debug
//! line, #1052, for when the width resolution comes back empty).
//!
//! The [`Transport`] indirection is what makes the whole path testable: the
//! tests below drive [`apply`] against a scripted fake and never open a socket.

use crate::layout::{self, Layout};
use crate::plugin::PLUGIN_ID;
use niri_ipc::socket::Socket;
use niri_ipc::{Action, Output, Reply, Request, Response, SizeChange, Window, Workspace};
use std::collections::HashMap;

/// One niri request, one niri reply.
///
/// `Err` is a *transport* failure (no `$NIRI_SOCKET`, the socket went away, a
/// reply that would not parse). A niri-level refusal arrives as the inner
/// `Err(String)` of [`Reply`] and carries niri's own error text, which is what
/// [`apply`] surfaces verbatim.
pub(crate) trait Transport {
    fn send(&mut self, request: Request) -> Result<Reply, String>;

    /// Where a diagnostic that isn't a niri error goes — real stderr for
    /// [`SocketTransport`], captured into [`fake::Fake::logs`] for the tests
    /// below (#1056 review, LOW-1). The only diagnostic today is
    /// [`apply`]'s Golden missing-width fallback; this seam is what lets a
    /// test assert it was emitted without piping the process's real stderr
    /// through a pipe — the same shape `watch.rs`'s `Backend::log` already
    /// uses for its own diagnostics.
    fn log(&mut self, line: &str);
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
/// held no tiled columns, which is a no-op and not an error. `Err` carries
/// niri's own text for the first request that failed; nothing is retried and no
/// widths are rolled back, because a partially applied layout is still a
/// coherent one and the next click fixes it.
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
    Ok(plan.len())
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
                Request::Action(_) => match self.action_error.take() {
                    Some(msg) => Err(msg),
                    None => Ok(Response::Handled),
                },
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
    use super::fake::{self, Fake, tile};
    use super::{Transport, apply, missing_width_diagnostic};
    use crate::layout::Layout;
    use niri_ipc::{Reply, Request, Response};
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

    /// The serialised actions of one `apply`, in send order.
    fn wire_bytes(niri: &Fake) -> Vec<String> {
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
