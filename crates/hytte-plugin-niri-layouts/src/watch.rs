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
//! - [`run`] is the thin plumbing: connect, hand each event to [`Watch`], emit
//!   what it returns, reconnect with [`backoff`] when the stream dies.
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

use niri_ipc::socket::Socket;
use niri_ipc::{Event, Request, Response};
use std::collections::HashMap;
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

/// Why a connection stopped — which decides whether the backoff grows.
enum Stopped {
    /// Never got as far as a live stream: no `$NIRI_SOCKET`, nothing listening,
    /// or niri refused `EventStream`. Back off further each time.
    Unreachable(String),
    /// We were streaming and the socket went away (niri restarted or exited).
    /// Reset the backoff: the next connect deserves a fresh 1 s.
    StreamEnded(String),
}

/// Watch niri forever, calling `emit` on each show/hide flip. **Never returns.**
///
/// Runs on its own OS thread rather than the SDK's current-thread runtime:
/// `Socket::read_events` hands back a blocking `FnMut`, and parking that on a
/// `spawn_blocking` slot for the life of the session is what that pool is
/// explicitly not for.
///
/// Nothing here panics on a missing niri — [`Socket::connect`] returns an
/// `io::Error` for an unset `$NIRI_SOCKET` like any other failure, so a plugin
/// session started outside a niri session simply never sees a verdict change
/// and leaves the chip hidden, logging one line per (exponentially rarer)
/// attempt.
pub(crate) fn run(prefix: &str, mut emit: impl FnMut(bool)) {
    let mut watch = Watch::default();
    let mut attempt = 0_u32;
    loop {
        watch.forget_compositor_state();
        let stopped = stream_once(&mut watch, &mut emit);
        attempt = match stopped {
            Stopped::Unreachable(why) => {
                eprintln!("[{prefix}] niri event stream unavailable ({why})");
                attempt.saturating_add(1)
            }
            Stopped::StreamEnded(why) => {
                eprintln!("[{prefix}] niri event stream ended ({why}), reconnecting");
                0
            }
        };
        std::thread::sleep(backoff(attempt));
    }
}

/// One connection's worth: dial, hand-shake `EventStream`, then fold events
/// until the socket gives out. Only ever returns because it stopped.
fn stream_once(watch: &mut Watch, emit: &mut impl FnMut(bool)) -> Stopped {
    let mut socket = match Socket::connect() {
        Ok(socket) => socket,
        Err(e) => return Stopped::Unreachable(format!("cannot reach $NIRI_SOCKET: {e}")),
    };
    match socket.send(Request::EventStream) {
        Ok(Ok(Response::Handled)) => {}
        Ok(Ok(other)) => {
            return Stopped::Unreachable(format!("unexpected EventStream reply: {other:?}"));
        }
        Ok(Err(msg)) => return Stopped::Unreachable(format!("niri refused EventStream: {msg}")),
        Err(e) => return Stopped::Unreachable(format!("EventStream request failed: {e}")),
    }

    let mut read_event = socket.read_events();
    loop {
        match read_event() {
            Ok(event) => {
                if let Some(verdict) = watch.observe(event) {
                    emit(verdict);
                }
            }
            Err(e) => return Stopped::StreamEnded(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BACKOFF_CEILING, MIN_WINDOWS, Watch, backoff};
    use niri_ipc::{Event, Window, WindowLayout, Workspace};
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
    /// what made a reconnect blink; this is the rule that stops it, stated
    /// on its own rather than only as a side effect of the reconnect test.
    #[test]
    fn half_an_opening_snapshot_emits_nothing_whichever_half_lands_first() {
        // Workspaces first, windows still to come.
        let mut watch = Watch::default();
        assert!(
            observe_all(&mut watch, vec![workspaces_changed(1)]).is_empty(),
            "no window list yet"
        );
        assert_eq!(
            observe_all(
                &mut watch,
                vec![windows_changed(vec![
                    window(10, Some(1)),
                    window(20, Some(1))
                ])]
            ),
            vec![true],
            "the snapshot completes and the verdict lands once"
        );

        // Windows first, workspaces still to come.
        let mut watch = Watch::default();
        assert!(
            observe_all(
                &mut watch,
                vec![windows_changed(vec![
                    window(10, Some(1)),
                    window(20, Some(1))
                ])]
            )
            .is_empty(),
            "nothing is focused yet, so nothing may be claimed"
        );
        assert_eq!(
            observe_all(&mut watch, vec![workspaces_changed(1)]),
            vec![true]
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
}
