//! #1071 §7's Start, Stop and state rows.
//!
//! Every one is driven against [`Script`], a scripted world: it answers the
//! niri queries from a queue of snapshots and **records every call in order**,
//! which is what lets these tests assert the thing the transactions are actually
//! about — that the name was verified *before* anything launched, that the slice
//! went down *before* the per-window walk, that the layout ran *once* and last.
//! A pure planner cannot show any of that, and a live compositor is not
//! available on CI.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::Duration;

use hytte::services::niri::{Window, WindowLayout, Workspace, WorkspaceAction};

use super::{
    Launched, Layout, Ops, Stack, StackApp, StackState, StartError, StopStep, app_launch, may_stop,
    names_to_release, plan_start, save, start, state_of, stop, stop_plan, stray_moves,
};
use crate::launch::Launch;

// ── Fixtures ─────────────────────────────────────────────────────────────────

const LEFT: &str = "DP-1";
const RIGHT: &str = "HDMI-A-1";

fn ws(id: u64, idx: u8, output: &str, name: Option<&str>, focused: bool) -> Workspace {
    Workspace {
        id,
        idx,
        name: name.map(str::to_owned),
        output: Some(output.to_owned()),
        is_urgent: false,
        is_active: focused,
        is_focused: focused,
        active_window_id: None,
    }
}

/// [`ws`], focused.
fn ws_focused(id: u64, idx: u8, output: &str, name: Option<&str>) -> Workspace {
    ws(id, idx, output, name, true)
}

fn win(id: u64, workspace: u64, app_id: &str) -> Window {
    Window {
        id,
        title: None,
        app_id: Some(app_id.to_owned()),
        pid: Some(1000 + i32::try_from(id).expect("small id")),
        workspace_id: Some(workspace),
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

fn stack(apps: &[&str]) -> Stack {
    Stack {
        apps: apps
            .iter()
            .map(|id| StackApp {
                id: (*id).to_owned(),
                exec: None,
            })
            .collect(),
        ..Stack::default()
    }
}

// ── The scripted world ───────────────────────────────────────────────────────

/// One thing a transaction did, in the order it did it.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    Actions(Vec<WorkspaceAction>),
    Workspaces,
    Windows,
    Launch(String),
    /// A `workspaces.toml` write, **recorded rather than performed**. See
    /// `Ops::save_stack`'s doc for why the write is on the seam at all.
    SaveStack(String),
    UnitForPid(u32),
    StopUnit(String),
    StopSlice(String),
    SliceIsUp(String),
    Sleep,
    Layout(Layout),
}

#[derive(Default)]
struct State {
    calls: Vec<Call>,
    /// Workspace snapshots, one per `workspaces()` call; the last repeats.
    workspaces: Vec<Vec<Workspace>>,
    /// Window snapshots, one per `windows()` call; the last repeats.
    windows: Vec<Vec<Window>>,
    workspace_reads: usize,
    window_reads: usize,
    slices_up: BTreeSet<String>,
    /// pid → unit, for `unit_for_pid`.
    units: BTreeMap<u32, String>,
    launch_error: Option<String>,
    /// When set, the scripted `workspaces.toml` write fails with it.
    save_error: Option<String>,
}

#[derive(Clone, Default)]
struct Script(Rc<RefCell<State>>);

impl Script {
    fn with_workspaces(self, snapshots: &[Vec<Workspace>]) -> Self {
        self.0.borrow_mut().workspaces = snapshots.to_vec();
        self
    }

    fn with_windows(self, snapshots: &[Vec<Window>]) -> Self {
        self.0.borrow_mut().windows = snapshots.to_vec();
        self
    }

    fn with_unit(self, pid: u32, unit: &str) -> Self {
        self.0.borrow_mut().units.insert(pid, unit.to_owned());
        self
    }

    fn with_slice_up(self, name: &str) -> Self {
        self.0.borrow_mut().slices_up.insert(name.to_owned());
        self
    }

    fn calls(&self) -> Vec<Call> {
        self.0.borrow().calls.clone()
    }

    /// Every action sent, flattened, in order.
    fn actions(&self) -> Vec<WorkspaceAction> {
        self.0
            .borrow()
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::Actions(a) => Some(a.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn launches(&self) -> Vec<String> {
        self.0
            .borrow()
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::Launch(u) => Some(u.clone()),
                _ => None,
            })
            .collect()
    }

    /// The index of the first call matching `f`, for ordering assertions.
    fn position(&self, f: impl Fn(&Call) -> bool) -> Option<usize> {
        self.0.borrow().calls.iter().position(f)
    }
}

impl Ops for Script {
    async fn send_actions(&self, actions: Vec<WorkspaceAction>) -> Result<(), String> {
        // The real `send_actions` opens no socket for an empty batch; recording
        // one here would make "nothing was moved" indistinguishable from "one
        // empty batch was sent".
        if !actions.is_empty() {
            self.0.borrow_mut().calls.push(Call::Actions(actions));
        }
        Ok(())
    }

    async fn workspaces(&self) -> Result<Vec<Workspace>, String> {
        let mut state = self.0.borrow_mut();
        state.calls.push(Call::Workspaces);
        let n = state.workspace_reads;
        state.workspace_reads += 1;
        Ok(state
            .workspaces
            .get(n)
            .or_else(|| state.workspaces.last())
            .cloned()
            .unwrap_or_default())
    }

    async fn windows(&self) -> Result<Vec<Window>, String> {
        let mut state = self.0.borrow_mut();
        state.calls.push(Call::Windows);
        let n = state.window_reads;
        state.window_reads += 1;
        Ok(state
            .windows
            .get(n)
            .or_else(|| state.windows.last())
            .cloned()
            .unwrap_or_default())
    }

    async fn launch(&self, launch: &Launch) -> Result<(), String> {
        let mut state = self.0.borrow_mut();
        state.calls.push(Call::Launch(launch.unit.clone()));
        state.launch_error.clone().map_or(Ok(()), Err)
    }

    async fn save_stack(&self, name: &str, _stack: &Stack) -> Result<(), String> {
        // Records; never writes. This is the whole point of the seam — see
        // `Ops::save_stack` — so do not "improve" this into a real write behind
        // a tempdir either: the transaction has no business knowing where the
        // file is, and a test that owns a path is a test that can leak one.
        let mut state = self.0.borrow_mut();
        state.calls.push(Call::SaveStack(name.to_owned()));
        state.save_error.clone().map_or(Ok(()), Err)
    }

    async fn unit_for_pid(&self, pid: u32) -> Option<String> {
        let mut state = self.0.borrow_mut();
        state.calls.push(Call::UnitForPid(pid));
        state.units.get(&pid).cloned()
    }

    async fn stop_unit(&self, unit: &str) -> Result<(), String> {
        self.0
            .borrow_mut()
            .calls
            .push(Call::StopUnit(unit.to_owned()));
        Ok(())
    }

    async fn stop_slice(&self, name: &str) -> Result<(), String> {
        let mut state = self.0.borrow_mut();
        state.calls.push(Call::StopSlice(name.to_owned()));
        // systemd's job, granted immediately: the slice is down by the time the
        // *next* `slice_is_up` asks. A fake that left it up would make
        // `wait_for_slice_down` spin out its whole bound on every Stop test.
        state.slices_up.remove(name);
        Ok(())
    }

    async fn slice_is_up(&self, name: &str) -> bool {
        let mut state = self.0.borrow_mut();
        state.calls.push(Call::SliceIsUp(name.to_owned()));
        state.slices_up.contains(name)
    }

    async fn sleep(&self, _duration: Duration) {
        self.0.borrow_mut().calls.push(Call::Sleep);
    }

    async fn apply_layout(&self, layout: Layout) -> Result<(), String> {
        self.0.borrow_mut().calls.push(Call::Layout(layout));
        Ok(())
    }
}

/// `block_on` for the `async` transactions. A current-thread runtime, because
/// `Script` is `Rc`-backed and deliberately not `Send` — these tests drive one
/// transaction and assert on its trace, and nothing here needs a thread pool.
fn run<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime")
        .block_on(future)
}

// ── §7: state ────────────────────────────────────────────────────────────────

/// The three-state derivation, all five rows of #1071 §7's state list.
#[test]
fn state_derives_from_the_slice_union_the_windows() {
    let named = [ws(1, 1, LEFT, Some("chat"), false)];
    let none = BTreeSet::new();

    assert_eq!(
        state_of("chat", &named, &[], true, &none),
        StackState::Active,
        "named + units → Active"
    );
    assert_eq!(
        state_of("chat", &named, &[win(9, 1, "firefox")], false, &none),
        StackState::Active,
        "named + windows, no units → Active"
    );
    assert_eq!(
        state_of("chat", &named, &[], false, &none),
        StackState::Inactive,
        "named, no units, no windows → Inactive"
    );
    assert_eq!(
        state_of("chat", &[], &[], true, &none),
        StackState::Inactive,
        "no workspace of that name → Inactive, whatever the slice says"
    );
    assert_eq!(
        state_of(
            "chat",
            &named,
            &[],
            false,
            &BTreeSet::from(["chat".to_owned()])
        ),
        StackState::Starting,
        "in flight → Starting, which outranks both sources"
    );
}

/// The lingering-name check matches the way niri matches names — case
/// insensitively — or a workspace named `Chat` would occupy `chat`'s name while
/// looking to us like it did not exist.
#[test]
fn a_workspace_name_is_matched_case_insensitively() {
    let named = [ws(1, 1, LEFT, Some("Chat"), false)];
    assert_eq!(
        state_of("chat", &named, &[win(9, 1, "x")], false, &BTreeSet::new()),
        StackState::Active
    );
    assert_eq!(
        plan_start("chat", &Stack::default(), &named, &[]),
        Err(StartError::NameTaken)
    );
}

// ── §7: Start ────────────────────────────────────────────────────────────────

/// A stopped stack gets a **new** workspace when the current one is busy
/// (Annika, 2026-09-10).
#[test]
fn a_stopped_stack_takes_a_new_workspace_when_the_current_one_is_busy() {
    let workspaces = [
        ws(1, 1, LEFT, None, true),  // focused, and NOT empty
        ws(2, 2, LEFT, None, false), // the trailing spare
    ];
    let plan = plan_start(
        "chat",
        &Stack::default(),
        &workspaces,
        &[win(9, 1, "firefox")],
    )
    .expect("plans");

    assert_eq!(
        plan.workspace, 2,
        "the trailing empty one, not the busy one"
    );
    assert!(!plan.adopted);
    assert_eq!(plan.output, LEFT);
}

/// …and adopts the current one when it is empty.
///
/// **The §7 mutation for this line**: dropping the emptiness filter makes the
/// adopt arm take workspace 1 in the test above too, which reds it. Both tests
/// are needed — this one alone would stay green under that mutation, because
/// here the current workspace really is empty.
#[test]
fn an_empty_current_workspace_is_adopted() {
    let workspaces = [ws(1, 1, LEFT, None, true), ws(2, 2, LEFT, None, false)];
    let plan = plan_start("chat", &Stack::default(), &workspaces, &[]).expect("plans");

    assert_eq!(plan.workspace, 1, "the current one");
    assert!(plan.adopted);
}

/// **MEDIUM-1.** The *fallback* leg takes an **empty** workspace too, not just
/// any unnamed one.
///
/// `an_empty_current_workspace_is_adopted` covers the adopt leg; nothing covered
/// this one, so a future edit could drop a stack onto a populated workspace that
/// merely happens to have no name.
///
/// **Mutation:** drop `&& is_empty(w)` from the trailing-workspace filter →
/// this reds (the higher-`idx` occupied workspace wins on `max_by_key`).
#[test]
fn the_trailing_workspace_must_be_empty_too() {
    let workspaces = [
        // Focused and busy, so the adopt leg is out.
        ws_focused(1, 1, LEFT, None),
        // Empty and unnamed — the one a Start may take.
        ws(2, 2, LEFT, None, false),
        // Higher idx, unnamed, but **occupied**: `max_by_key` would pick this
        // one without the emptiness filter.
        ws(3, 3, LEFT, None, false),
    ];
    let windows = [win(9, 1, "firefox"), win(10, 3, "mpv")];
    let plan = plan_start("chat", &Stack::default(), &workspaces, &windows).expect("plans");

    assert_eq!(plan.workspace, 2, "the empty one, not the highest-idx one");
    assert!(!plan.adopted);
}

/// …and with no empty workspace at all, a Start refuses rather than landing on
/// someone's work.
#[test]
fn a_start_with_nowhere_to_go_refuses() {
    let workspaces = [ws_focused(1, 1, LEFT, None), ws(2, 2, LEFT, None, false)];
    let windows = [win(9, 1, "firefox"), win(10, 2, "mpv")];
    assert_eq!(
        plan_start("chat", &Stack::default(), &workspaces, &windows),
        Err(StartError::NoFreeWorkspace)
    );
}

/// A stack's own monitor wins when it is connected, and the focused output is
/// the fallback when it is not — which is what keeps a stack whose screen is
/// unplugged from silently starting nowhere.
#[test]
fn the_stacks_monitor_wins_when_connected_and_the_focused_one_otherwise() {
    let both = [
        ws(1, 1, LEFT, None, true),
        ws(2, 2, LEFT, None, false),
        ws(3, 1, RIGHT, None, false),
    ];
    let on_right = Stack {
        monitor: Some(RIGHT.to_owned()),
        ..Stack::default()
    };
    let plan = plan_start("chat", &on_right, &both, &[]).expect("plans");
    assert_eq!(plan.output, RIGHT);
    assert_eq!(plan.workspace, 3, "not the focused workspace on DP-1");
    assert!(
        !plan.adopted,
        "the focused workspace is on the other screen"
    );

    let only_left = [ws(1, 1, LEFT, None, true)];
    let plan = plan_start("chat", &on_right, &only_left, &[]).expect("plans");
    assert_eq!(plan.output, LEFT, "HDMI-A-1 is not connected");
    assert!(plan.adopted);
}

/// The batch is exactly the two id-addressed actions, in order: name, then
/// focus. Focus has to land before anything launches — new windows open on the
/// focused workspace.
#[test]
fn the_batch_names_then_focuses() {
    let workspaces = [ws(1, 1, LEFT, None, true)];
    let plan = plan_start("chat", &Stack::default(), &workspaces, &[]).expect("plans");
    assert_eq!(
        plan.batch,
        vec![
            WorkspaceAction::SetName {
                workspace: 1,
                name: "chat".to_owned()
            },
            WorkspaceAction::Focus { workspace: 1 },
        ]
    );
}

/// A name already on a workspace is refused before anything else happens.
#[test]
fn a_taken_name_is_refused() {
    let workspaces = [ws(1, 1, LEFT, Some("chat"), true)];
    assert_eq!(
        plan_start("chat", &Stack::default(), &workspaces, &[]),
        Err(StartError::NameTaken)
    );
}

/// #1071 §7's housekeeping row: a stack whose windows vanished has its lingering
/// name released.
///
/// **The mutation**: skip the release, and the next Start's `SetWorkspaceName`
/// silently no-ops — which the second half of this test shows as a concrete
/// `NameTaken` rather than as an assertion about an assertion.
#[test]
fn a_stack_whose_windows_vanished_has_its_name_released() {
    let stacks = BTreeMap::from([("chat".to_owned(), Stack::default())]);
    let workspaces = [ws(1, 1, LEFT, Some("chat"), true)];

    let released = names_to_release(&stacks, &workspaces, &[], &|_| false);
    assert_eq!(released, vec![WorkspaceAction::UnsetName { workspace: 1 }]);

    // Without the release, this is what the next Start runs into.
    assert_eq!(
        plan_start("chat", &Stack::default(), &workspaces, &[]),
        Err(StartError::NameTaken),
        "which is exactly why the release is not optional"
    );
}

/// …but a workspace that still has windows, or whose slice is up, keeps its
/// name. Releasing either would rename a *live* stack out from under itself.
#[test]
fn a_live_stacks_name_is_never_released() {
    let stacks = BTreeMap::from([("chat".to_owned(), Stack::default())]);
    let workspaces = [ws(1, 1, LEFT, Some("chat"), true)];

    assert!(
        names_to_release(&stacks, &workspaces, &[win(9, 1, "x")], &|_| false).is_empty(),
        "it still has a window"
    );
    assert!(
        names_to_release(&stacks, &workspaces, &[], &|name| name == "chat").is_empty(),
        "its units are still up"
    );
    // A workspace the user named by hand is not ours to unname.
    let unknown = [ws(1, 1, LEFT, Some("scratch"), true)];
    assert!(names_to_release(&stacks, &unknown, &[], &|_| false).is_empty());
}

/// #1071 §7: **the name is verified before anything is launched**, and the
/// verification reads niri back rather than trusting the action's reply.
///
/// **The mutation**: drop the read-back check, and a Start whose
/// `SetWorkspaceName` was swallowed goes on to launch the stack's apps onto
/// whatever workspace happened to be focused. Here the second snapshot shows the
/// name never landed, so a correct Start launches nothing at all.
#[test]
fn a_name_that_did_not_land_stops_the_start_before_any_launch() {
    let before = vec![ws(1, 1, LEFT, None, true)];
    let script = Script::default()
        // housekeeping read, plan read, then the verify read — which still
        // shows no name.
        .with_workspaces(&[before.clone(), before.clone(), before])
        .with_windows(&[Vec::new()]);

    let err = run(start(
        &script,
        "chat",
        &stack(&["firefox"]),
        &BTreeMap::new(),
    ))
    .expect_err("the Start fails");

    assert!(err.contains("did not take the name"), "{err}");
    assert!(
        script.launches().is_empty(),
        "nothing may launch after a failed naming: {:?}",
        script.calls()
    );
    assert!(
        script.position(|c| matches!(c, Call::Layout(_))).is_none(),
        "and no layout either"
    );
}

/// The happy path, as a trace: housekeeping, batch, **verify**, launches,
/// reconcile, layout — in that order, with the verify strictly before the first
/// launch and the layout strictly after the last.
#[test]
fn a_start_verifies_then_launches_then_lays_out_once() {
    let before = vec![ws(1, 1, LEFT, None, true)];
    let after = vec![ws(1, 1, LEFT, Some("chat"), true)];
    let settled = vec![win(9, 1, "firefox"), win(10, 1, "Alacritty")];
    let script = Script::default()
        .with_workspaces(&[before.clone(), before, after])
        .with_windows(&[Vec::new(), Vec::new(), settled]);

    let mut s = stack(&["firefox", "Alacritty"]);
    s.layout = Layout::Golden;
    let plan = run(start(&script, "chat", &s, &BTreeMap::new())).expect("starts");
    assert_eq!(plan.workspace, 1);

    assert_eq!(
        script.launches(),
        [
            "trollshell-ws-chat-0.service",
            "trollshell-ws-chat-1.service"
        ],
        "one unit per app, indexed by stack position"
    );

    let calls = script.calls();
    let verify = script
        .position(|c| matches!(c, Call::Workspaces))
        .map(|_| {
            // the *third* workspace read is the verify; the first two are
            // housekeeping and planning.
            calls
                .iter()
                .enumerate()
                .filter(|(_, c)| matches!(c, Call::Workspaces))
                .map(|(i, _)| i)
                .nth(2)
                .expect("a third workspace read — the verify")
        })
        .expect("a workspace read");
    let first_launch = script
        .position(|c| matches!(c, Call::Launch(_)))
        .expect("a launch");
    let layout = script
        .position(|c| matches!(c, Call::Layout(_)))
        .expect("a layout");

    assert!(verify < first_launch, "verify before launch: {calls:?}");
    assert!(
        first_launch < layout,
        "layout after the launches: {calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|c| matches!(c, Call::Layout(_)))
            .count(),
        1,
        "the layout is applied ONCE, not per app: {calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|c| **c == Call::Layout(Layout::Golden))
            .count(),
        1,
        "…and with the stack's layout"
    );
}

/// Nothing was open when the Start began, and no unit has resolved yet — the
/// `app_id` fallback (rule 3).
fn fresh() -> Launched {
    Launched::default()
}

/// A stray window — one this Start opened that landed elsewhere — is moved
/// home inside the grace window, by id.
#[test]
fn a_stray_window_is_moved_home() {
    let none = BTreeMap::new();
    assert_eq!(
        stray_moves(
            &stack(&["firefox"]),
            1,
            &[win(9, 2, "firefox")],
            &fresh(),
            &none
        ),
        vec![WorkspaceAction::MoveWindow {
            window: 9,
            workspace: 1
        }]
    );
    assert!(
        stray_moves(
            &stack(&["firefox"]),
            1,
            &[win(9, 1, "firefox")],
            &fresh(),
            &none
        )
        .is_empty(),
        "a window already home is not moved"
    );
    assert!(
        stray_moves(
            &stack(&["firefox"]),
            1,
            &[win(9, 2, "mpv")],
            &fresh(),
            &none
        )
        .is_empty(),
        "and a window that is not the stack's is left alone"
    );
}

/// **HIGH-1.** A window the user already had open is never moved, however well
/// its `app_id` matches.
///
/// You have Firefox on workspace 2. You press ▶ on a stack that lists
/// `org.mozilla.firefox`. Before this, the grace window yanked your existing
/// window onto the stack's workspace — and `MoveWindowToWorkspace` carries focus
/// with it, so you went too.
///
/// **Mutation:** drop the `!launched.before.contains` filter (rule 1) and this
/// reds; `a_stray_window_is_moved_home` stays green, because there the window is
/// new.
#[test]
fn a_window_that_predates_the_start_is_never_moved() {
    let mine = win(9, 2, "firefox");
    let launched = Launched {
        before: BTreeSet::from([mine.id]),
        units: BTreeSet::new(),
    };
    assert!(
        stray_moves(
            &stack(&["firefox"]),
            1,
            std::slice::from_ref(&mine),
            &launched,
            &BTreeMap::new()
        )
        .is_empty(),
        "the user's own window, of an app the stack happens to list"
    );

    // …and a *new* window of that same app, opened by this Start, still is.
    let ours = win(10, 2, "firefox");
    assert_eq!(
        stray_moves(
            &stack(&["firefox"]),
            1,
            &[mine, ours],
            &launched,
            &BTreeMap::new()
        ),
        vec![WorkspaceAction::MoveWindow {
            window: 10,
            workspace: 1
        }],
        "exactly one of the two moves"
    );
}

/// **HIGH-1, the pid leg.** A new window whose pid belongs to one of this
/// Start's units is moved even when its `app_id` is not in the stack — which is
/// the ordinary case for an app whose window reports a different `app_id` than
/// its desktop entry id.
///
/// And the converse: a new window in *another* unit is not moved on the strength
/// of its unit alone.
#[test]
fn a_window_in_one_of_this_starts_units_is_moved_whatever_its_app_id() {
    let ours = win(9, 2, "some-other-app-id");
    let theirs = win(10, 2, "some-other-app-id");
    let launched = Launched {
        before: BTreeSet::new(),
        units: BTreeSet::from(["trollshell-ws-chat-0.service".to_owned()]),
    };
    let unit_of = BTreeMap::from([
        (9, Some("trollshell-ws-chat-0.service".to_owned())),
        (10, Some("app-niri-firefox-1234.scope".to_owned())),
    ]);
    assert_eq!(
        stray_moves(
            &stack(&["firefox"]),
            1,
            &[ours, theirs],
            &launched,
            &unit_of
        ),
        vec![WorkspaceAction::MoveWindow {
            window: 9,
            workspace: 1
        }],
        "ours by unit; theirs is neither our unit nor our app_id"
    );
}

/// …end to end: the reconcile loop really sends the move, and stops as soon as
/// every app has a window on the workspace rather than sleeping out the whole
/// grace window.
#[test]
fn the_grace_window_moves_a_stray_and_then_settles() {
    let before = vec![ws(1, 1, LEFT, None, true)];
    let after = vec![ws(1, 1, LEFT, Some("chat"), true)];
    let script = Script::default()
        .with_workspaces(&[before.clone(), before, after])
        .with_windows(&[
            Vec::new(),
            Vec::new(),
            // First reconcile tick: the window opened on the wrong workspace.
            vec![win(9, 2, "firefox")],
            // Second: it is home.
            vec![win(9, 1, "firefox")],
        ]);

    run(start(
        &script,
        "chat",
        &stack(&["firefox"]),
        &BTreeMap::new(),
    ))
    .expect("starts");

    assert!(
        script.actions().contains(&WorkspaceAction::MoveWindow {
            window: 9,
            workspace: 1
        }),
        "the stray was moved: {:?}",
        script.actions()
    );
    let sleeps = script.calls().iter().filter(|c| **c == Call::Sleep).count();
    assert_eq!(
        sleeps,
        2,
        "the loop stops once every app is home rather than sleeping out the \
         whole grace window: {:?}",
        script.calls()
    );
}

/// One app's unit name, slice and argv.
#[test]
fn an_app_launches_into_the_stacks_own_slice() {
    let launch = app_launch(
        "chat",
        1,
        &StackApp {
            id: "Alacritty".to_owned(),
            exec: Some("alacritty -e weechat".to_owned()),
        },
    );
    assert_eq!(launch.unit, "trollshell-ws-chat-1.service");
    assert_eq!(launch.slice.as_deref(), Some("trollshell-ws-chat.slice"));
    assert_eq!(launch.argv, ["alacritty", "-e", "weechat"]);
    assert!(
        launch.properties.is_empty(),
        "an app the user closed has finished; it is not a supervised service"
    );
    assert!(launch.secret_env.is_empty(), "no keyring injection here");

    let bare = app_launch(
        "chat",
        0,
        &StackApp {
            id: "firefox".to_owned(),
            exec: None,
        },
    );
    assert_eq!(
        bare.argv,
        ["firefox"],
        "with no override the id is the command until phase 4 resolves entries"
    );
}

// ── §7: Stop ─────────────────────────────────────────────────────────────────

/// The per-window plan: a unit if systemd names one, else a close **by id**.
///
/// **The §7 mutation**: `CloseWindow { id: None }` closes the *focused* window,
/// which during a Stop is very likely not this one. `StopStep::Close` carries a
/// `u64`, so the mutation has to be made in the lowering — where
/// `hytte-services`' `every_action_names_its_target` pins it on the wire.
#[test]
fn stop_plans_a_unit_where_there_is_one_and_a_close_by_id_otherwise() {
    let a = win(9, 1, "firefox");
    let b = win(10, 1, "bash");
    let units = BTreeMap::from([
        (9, Some("app-niri-firefox-1234.scope".to_owned())),
        (10, None),
    ]);
    assert_eq!(
        stop_plan("chat", &[&a, &b], &units),
        vec![
            StopStep::StopUnit("app-niri-firefox-1234.scope".to_owned()),
            StopStep::Close(10),
        ]
    );
}

/// **HIGH-2.** The allowlist, stated as the units it must and must not stop.
///
/// The two that matter are not hypothetical. The shell opens links with
/// `gio::AppInfo::launch_default_for_uri`, and glib 2.88 creates no transient
/// scope for that — the browser is forked into **`trollshell.service`'s own
/// cgroup**, so `GetUnitByPID` on its window answers `trollshell.service` and an
/// unguarded Stop makes the shell stop itself. niri's `StartTransientUnit` has a
/// fallback path, so anything that missed its scope answers `niri.service` and an
/// unguarded Stop takes the session down.
///
/// **Mutation:** return `true` unconditionally from `may_stop` → this reds on
/// the first refusal.
#[test]
fn may_stop_allows_app_scopes_and_this_stacks_units_and_nothing_else() {
    // Allowed: a launcher-started application, whatever the launcher.
    assert!(may_stop("chat", "app-niri-firefox-1234.scope"));
    assert!(may_stop("chat", "app-fuzzel-mpv-99.scope"));
    // Allowed: this stack's own units, escaped name and all.
    assert!(may_stop("chat", "trollshell-ws-chat-0.service"));
    assert!(may_stop("chat-dev", r"trollshell-ws-chat\x2ddev-2.service"));

    // The two catastrophic ones.
    assert!(!may_stop("chat", "trollshell.service"), "the shell itself");
    assert!(!may_stop("chat", "niri.service"), "the compositor");

    // …and everything else that is not this workspace's business.
    assert!(
        !may_stop("chat", "trollshell-ws-dev-0.service"),
        "another stack"
    );
    assert!(!may_stop("chat", "trollshell-plugin-pet.service"));
    assert!(!may_stop("chat", "trollshell-launch-caw-7-4242-3.service"));
    assert!(!may_stop("chat", "session.slice"));
    assert!(!may_stop("chat", "dbus.service"));
    assert!(!may_stop("chat", "pipewire.service"));
    // An `app-` prefix is not enough on its own: a *service* named that way is
    // not a launcher scope.
    assert!(!may_stop("chat", "app-something.service"));
}

/// …and the planner really routes a refused unit to a close rather than
/// dropping the window or stopping it anyway.
#[test]
fn a_refused_unit_closes_the_window_instead() {
    let shell_spawned = win(9, 1, "firefox");
    let ours = win(10, 1, "Alacritty");
    let units = BTreeMap::from([
        (9, Some("trollshell.service".to_owned())),
        (10, Some("trollshell-ws-chat-0.service".to_owned())),
    ]);
    assert_eq!(
        stop_plan("chat", &[&shell_spawned, &ours], &units),
        vec![
            StopStep::CloseInstead {
                window: 9,
                refused: "trollshell.service".to_owned(),
            },
            StopStep::StopUnit("trollshell-ws-chat-0.service".to_owned()),
        ],
        "the window still goes; the shell does not"
    );
}

/// The whole Stop, as a trace: **slice first**, then the per-window walk, then
/// the name release.
///
/// The order is the substance. Stopping the slice first means the walk only ever
/// has to deal with what the *compositor* started; doing it the other way round
/// would have the walk racing systemd over this shell's own units. And the name
/// is released last — freeing it before the windows it identifies are dealt with
/// would leave them unfindable.
#[test]
fn a_stop_takes_the_slice_down_first_then_walks_what_is_left() {
    let workspaces = vec![ws(1, 1, LEFT, Some("chat"), true)];
    let windows = vec![win(9, 1, "firefox"), win(10, 1, "bash")];
    let script = Script::default()
        .with_workspaces(&[workspaces])
        .with_windows(&[windows])
        // window 9's pid is 1009 (see `win`), and systemd knows its scope.
        .with_unit(1009, "app-niri-firefox-1234.scope")
        .with_slice_up("chat");

    run(stop(&script, "chat")).expect("stops");

    let calls = script.calls();
    let slice = script
        .position(|c| matches!(c, Call::StopSlice(_)))
        .expect("the slice was stopped");
    assert_eq!(slice, 0, "the slice goes down FIRST: {calls:?}");
    assert_eq!(
        calls[slice],
        Call::StopSlice("chat".to_owned()),
        "by name, so the module builds the slice name"
    );

    let unit = script
        .position(|c| matches!(c, Call::StopUnit(_)))
        .expect("the scope was stopped");
    assert!(slice < unit, "after the slice: {calls:?}");
    assert_eq!(
        calls[unit],
        Call::StopUnit("app-niri-firefox-1234.scope".to_owned())
    );

    assert_eq!(
        script.actions(),
        vec![
            // window 10 had no unit, so niri closes it — by id.
            WorkspaceAction::CloseWindow { window: 10 },
            // and the name is released last, on the same socket.
            WorkspaceAction::UnsetName { workspace: 1 },
        ],
        "{calls:?}"
    );
}

/// **MEDIUM-2.** A Stop touches **only this workspace's** windows.
///
/// The `workspace_id` filter in `stop`'s `remaining` is the single most
/// dangerous line in the transaction: without it a Stop walks every window in
/// the session and stops or closes all of them. The trace test above scripts
/// only windows that are already on the workspace, so it cannot see the filter
/// at all.
///
/// **Mutation:** delete `.filter(|w| w.workspace_id == Some(workspace))` → this
/// reds on both assertions; every other Stop test stays green.
#[test]
fn a_stop_leaves_windows_on_other_workspaces_alone() {
    let workspaces = vec![
        ws(1, 1, LEFT, Some("chat"), true),
        ws(2, 2, LEFT, Some("dev"), false),
    ];
    let windows = vec![
        win(9, 1, "firefox"),
        // Another workspace's window, with a unit the allowlist would happily
        // have stopped if the scoping were gone.
        win(20, 2, "mpv"),
        // …and one with no unit at all, which would have been closed.
        win(21, 2, "bash"),
    ];
    let script = Script::default()
        .with_workspaces(&[workspaces])
        .with_windows(&[windows])
        .with_unit(1009, "app-niri-firefox-1234.scope")
        .with_unit(1020, "app-niri-mpv-5678.scope")
        .with_slice_up("chat");

    run(stop(&script, "chat")).expect("stops");

    assert_eq!(
        script
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::StopUnit(_)))
            .collect::<Vec<_>>(),
        vec![Call::StopUnit("app-niri-firefox-1234.scope".to_owned())],
        "only this workspace's unit: {:?}",
        script.calls()
    );
    assert_eq!(
        script.actions(),
        vec![WorkspaceAction::UnsetName { workspace: 1 }],
        "no window on another workspace is closed"
    );
}

/// **MEDIUM-3.** The slice is really down before the walk starts.
///
/// `StopUnit` is a job *enqueue*: measured on systemd 260.2 it returns a job
/// path in ~15 ms with the unit still `deactivating`. So "the walk only sees
/// what the compositor started" — which is how the per-window pass is justified
/// — needs an actual wait, not just an ordering.
///
/// **Mutation:** drop the `wait_for_slice_down` call → the first `SliceIsUp`
/// after the stop disappears and this reds.
#[test]
fn a_stop_waits_for_the_slice_to_be_down_before_walking() {
    let script = Script::default()
        .with_workspaces(&[vec![ws(1, 1, LEFT, Some("chat"), true)]])
        .with_windows(&[vec![win(9, 1, "firefox")]])
        // Still up when the stop is issued; the fake clears it on `stop_slice`,
        // the way systemd's job eventually does.
        .with_slice_up("chat");

    run(stop(&script, "chat")).expect("stops");

    let calls = script.calls();
    let stop_slice = script
        .position(|c| matches!(c, Call::StopSlice(_)))
        .expect("the slice was stopped");
    let checked = script
        .position(|c| matches!(c, Call::SliceIsUp(_)))
        .expect("and the stop waited for it to go down");
    let walked = script
        .position(|c| matches!(c, Call::UnitForPid(_)))
        .expect("then walked the windows");
    assert!(
        stop_slice < checked && checked < walked,
        "stop, wait, then walk: {calls:?}"
    );
}

/// A Stop of a stack that is not on screen stops its slice and does nothing
/// else — no windows to walk, no name to release. It must not fail.
#[test]
fn stopping_an_inactive_stack_is_just_the_slice() {
    let script = Script::default().with_workspaces(&[vec![ws(1, 1, LEFT, None, true)]]);
    run(stop(&script, "chat")).expect("stops");
    assert_eq!(
        script.calls(),
        vec![
            Call::StopSlice("chat".to_owned()),
            // One check, answered "down" straight away — the wait costs a
            // round trip and no sleep when there was nothing to wait for.
            Call::SliceIsUp("chat".to_owned()),
            Call::Workspaces,
        ],
        "the slice stop is idempotent, so there is nothing to guard"
    );
}

/// Housekeeping asks systemd about every stack, not only the one being started —
/// a lingering name blocks the Start that wants *that* name, and the shell does
/// not know in advance which that is.
#[test]
fn housekeeping_releases_every_stale_name_at_once() {
    let stacks = BTreeMap::from([
        ("chat".to_owned(), Stack::default()),
        ("dev".to_owned(), Stack::default()),
        ("music".to_owned(), Stack::default()),
    ]);
    let workspaces = [
        ws(1, 1, LEFT, Some("chat"), false),
        ws(2, 2, LEFT, Some("dev"), false),
        ws(3, 3, LEFT, Some("music"), false),
    ];
    // `dev` still has a window; `music`'s units are up. Only `chat` is stale.
    let released = names_to_release(&stacks, &workspaces, &[win(9, 2, "x")], &|name| {
        name == "music"
    });
    assert_eq!(released, vec![WorkspaceAction::UnsetName { workspace: 1 }]);
}

/// A Start runs housekeeping before it plans, so a stack that was Stopped by
/// closing every window by hand can be Started again straight away.
///
/// This is the §7 housekeeping mutation at the transaction level: with the
/// release skipped, the plan below sees its own lingering name and fails.
#[test]
fn a_start_releases_the_stale_name_before_planning_its_own() {
    let lingering = vec![ws(1, 1, LEFT, Some("chat"), true)];
    // After housekeeping, niri reports the name gone.
    let released = vec![ws(1, 1, LEFT, None, true)];
    let named = vec![ws(1, 1, LEFT, Some("chat"), true)];
    let script = Script::default()
        .with_workspaces(&[lingering, released, named])
        .with_windows(&[Vec::new()]);

    let stacks = BTreeMap::from([("chat".to_owned(), Stack::default())]);
    let plan = run(start(&script, "chat", &Stack::default(), &stacks)).expect("starts");

    assert_eq!(plan.workspace, 1);
    assert!(plan.adopted, "the empty current workspace was adopted");
    assert_eq!(
        script.actions().first(),
        Some(&WorkspaceAction::UnsetName { workspace: 1 }),
        "the release comes before the naming: {:?}",
        script.actions()
    );
}

/// A slice that is up is never released and never adopted from — the stack is
/// Active, and there is no Start to run.
#[test]
fn a_start_of_a_stack_whose_slice_is_up_finds_its_name_taken() {
    let named = vec![ws(1, 1, LEFT, Some("chat"), true)];
    let script = Script::default()
        .with_workspaces(&[named])
        .with_windows(&[Vec::new()])
        .with_slice_up("chat");

    let stacks = BTreeMap::from([("chat".to_owned(), Stack::default())]);
    let err = run(start(&script, "chat", &Stack::default(), &stacks)).expect_err("refuses");
    assert!(err.contains("already on a workspace"), "{err}");
    assert!(script.launches().is_empty());
}

// ── §3.7: Save names the workspace ───────────────────────────────────────────

/// **HIGH-3.** A Save **names the niri workspace**, so the card that was
/// ephemeral becomes the saved Active one.
///
/// §3.7: *"The batch names the niri workspace immediately, so the saved
/// workspace **is** the Active card."* Writing the file alone leaves the
/// workspace unnamed, so the page keeps drawing it as an "Unsaved workspace"
/// card *and* draws the new stack as a second, Inactive card whose ▶ would
/// launch a second copy of every app.
///
/// This half is the precondition: a name niri already holds is refused **before**
/// the file is touched, because `SetWorkspaceName` would silently no-op and
/// leave a stack in the file that can never be this workspace.
#[test]
fn a_save_verifies_the_name_is_free_before_writing_anything() {
    let script = Script::default().with_workspaces(&[vec![ws(1, 1, LEFT, Some("chat"), true)]]);
    let err = run(save(&script, 1, "chat", &Stack::default())).expect_err("refuses");
    assert!(err.contains("already on a workspace"), "{err}");
    assert!(
        script.actions().is_empty(),
        "nothing was sent to niri: {:?}",
        script.calls()
    );
}

/// …and with the name free, the Save writes, then issues the id-addressed
/// `SetName` for **this** workspace, then reads it back — in that order.
///
/// This test used to accept *either* outcome, because `save` called
/// `config::workspaces::save_stack` directly and that resolves its own path
/// through `xdg::overlay_path`. Two things were wrong with that, and the second
/// is the one that mattered:
///
/// 1. It wrote the developer's **real** `~/.config/trollshell/workspaces.toml`.
/// 2. Having written it once, every later run took the `Err` arm — whose
///    assertion "no actions were sent" is **true by construction** — so deleting
///    the `SetName` send left the suite green on any box that had run it before.
///    A test that only reds on a virgin machine is not a test.
///
/// The write is on the [`Ops`] seam now, so this asserts unconditionally.
///
/// **Mutation:** delete the `send_actions` from `save` → red here, on a virgin
/// box and a pre-run one alike.
#[test]
fn a_save_writes_then_names_this_workspace_and_verifies_it_landed() {
    let before = vec![ws(7, 2, LEFT, None, true)];
    let after = vec![ws(7, 2, LEFT, Some("chat"), true)];
    let script = Script::default().with_workspaces(&[before, after]);

    run(save(&script, 7, "chat", &Stack::default())).expect("saves");

    assert_eq!(
        script.actions(),
        vec![WorkspaceAction::SetName {
            workspace: 7,
            name: "chat".to_owned()
        }],
        "named by id, not by focus"
    );

    let calls = script.calls();
    let saved = script
        .position(|c| matches!(c, Call::SaveStack(_)))
        .expect("the stack was written");
    let named = script
        .position(|c| matches!(c, Call::Actions(_)))
        .expect("and then named");
    assert_eq!(calls[saved], Call::SaveStack("chat".to_owned()));
    assert!(saved < named, "write before naming: {calls:?}");
    // The read-back is the *last* workspace query, after the naming.
    let reads: Vec<usize> = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c, Call::Workspaces))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        reads.len(),
        2,
        "one free-name check, one read-back: {calls:?}"
    );
    assert!(
        reads[1] > named,
        "the read-back follows the naming: {calls:?}"
    );
}

/// A naming that did not land is reported rather than shrugged off.
///
/// `SetWorkspaceName` reports success either way, so the read-back is the only
/// thing that can tell. Here niri never takes the name.
///
/// **Mutation:** drop the read-back check from `save` → red.
#[test]
fn a_save_whose_naming_did_not_land_says_so() {
    let unnamed = vec![ws(7, 2, LEFT, None, true)];
    let script = Script::default().with_workspaces(&[unnamed.clone(), unnamed]);

    let err = run(save(&script, 7, "chat", &Stack::default())).expect_err("reports");
    assert!(err.contains("did not take the name"), "{err}");
    assert!(
        err.contains("saved"),
        "…while saying the file half did happen, since it did: {err}"
    );
}

/// A write that fails leaves the workspace **unnamed**.
///
/// The file and the name are one operation from the user's side; half of it is
/// worse than none, because a named workspace with no stack behind it is a card
/// that vanishes on the next reload.
#[test]
fn a_failed_write_never_names_the_workspace() {
    let script = Script::default().with_workspaces(&[vec![ws(7, 2, LEFT, None, true)]]);
    script.0.borrow_mut().save_error = Some("read-only file system".to_owned());

    let err = run(save(&script, 7, "chat", &Stack::default())).expect_err("reports");
    assert!(err.contains("read-only"), "{err}");
    assert!(
        script.actions().is_empty(),
        "nothing was named: {:?}",
        script.calls()
    );
}

/// **The guard.** No test in this module may touch the real config directory.
///
/// The re-review of #1101 found `save` reaching `xdg::overlay_path` through
/// `config::workspaces::save_stack`, which wrote a real
/// `~/.config/trollshell/workspaces.toml` on the reviewer's machine. The seam
/// makes that structurally impossible now; this asserts it, so a future edit
/// that calls the module function directly again reds here instead of appearing
/// in someone's home directory.
///
/// Stated as a before/after on the real overlay path rather than on a mock: the
/// question is literally "did the suite write that file", and only the file can
/// answer it.
#[test]
fn the_save_transaction_never_touches_the_real_config_directory() {
    let real = hytte_config::xdg::overlay_path("workspaces");
    let before = real
        .as_ref()
        .map(|p| (p.exists(), std::fs::metadata(p).ok()));

    // Every shape of Save, including the ones that go furthest.
    let ok = Script::default().with_workspaces(&[
        vec![ws(7, 2, LEFT, None, true)],
        vec![ws(7, 2, LEFT, Some("chat"), true)],
    ]);
    run(save(&ok, 7, "chat", &Stack::default())).expect("saves");
    let taken = Script::default().with_workspaces(&[vec![ws(1, 1, LEFT, Some("chat"), true)]]);
    let _ = run(save(&taken, 1, "chat", &Stack::default()));

    let after = real
        .as_ref()
        .map(|p| (p.exists(), std::fs::metadata(p).ok()));
    match (&before, &after) {
        (Some((false, _)), Some((exists, _))) => assert!(
            !exists,
            "the suite created {real:?} — the write escaped the Ops seam"
        ),
        (Some((true, Some(b))), Some((true, Some(a)))) => assert_eq!(
            b.modified().ok(),
            a.modified().ok(),
            "the suite modified {real:?} — the write escaped the Ops seam"
        ),
        // No `XDG_CONFIG_HOME` at all (a sandboxed CI runner): there is no real
        // path to protect, and `Script` still recorded rather than wrote.
        _ => {}
    }
    assert!(
        ok.calls().contains(&Call::SaveStack("chat".to_owned())),
        "…and the write really was attempted, so this is not vacuous: {:?}",
        ok.calls()
    );
}
