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
    Layout, Ops, Stack, StackApp, StackState, StartError, StopStep, app_launch, names_to_release,
    plan_start, start, state_of, stop, stop_plan, stray_moves,
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
        self.0
            .borrow_mut()
            .calls
            .push(Call::StopSlice(name.to_owned()));
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

/// A stray window — one whose `app_id` belongs to the stack but which landed
/// elsewhere — is moved home inside the grace window, by id.
#[test]
fn a_stray_window_is_moved_home() {
    assert_eq!(
        stray_moves(&stack(&["firefox"]), 1, &[win(9, 2, "firefox")]),
        vec![WorkspaceAction::MoveWindow {
            window: 9,
            workspace: 1
        }]
    );
    assert!(
        stray_moves(&stack(&["firefox"]), 1, &[win(9, 1, "firefox")]).is_empty(),
        "a window already home is not moved"
    );
    assert!(
        stray_moves(&stack(&["firefox"]), 1, &[win(9, 2, "mpv")]).is_empty(),
        "and a window that is not the stack's is left alone"
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
        stop_plan(&[&a, &b], &units),
        vec![
            StopStep::StopUnit("app-niri-firefox-1234.scope".to_owned()),
            StopStep::Close(10),
        ]
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
        .with_unit(1009, "app-niri-firefox-1234.scope");

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

/// A Stop of a stack that is not on screen stops its slice and does nothing
/// else — no windows to walk, no name to release. It must not fail.
#[test]
fn stopping_an_inactive_stack_is_just_the_slice() {
    let script = Script::default().with_workspaces(&[vec![ws(1, 1, LEFT, None, true)]]);
    run(stop(&script, "chat")).expect("stops");
    assert_eq!(
        script.calls(),
        vec![Call::StopSlice("chat".to_owned()), Call::Workspaces],
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
