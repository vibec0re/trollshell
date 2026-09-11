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
    AppStart, AutostartPlan, Launchable, Launched, Layout, Ops, Stack, StackApp, StackState,
    StartError, StopStep, Unresolvable, Workspaces, app_start, autostart_all, autostart_driver,
    autostart_plan, autostart_tick, column_order_batch, may_stop, missing_apps, move_to_monitor,
    names_to_release, order_index, plan_start, release_lingering_names, save, start, state_of,
    stop, stop_plan, stray_moves,
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

/// [`win`], in a named scrolling-layout column.
///
/// The column is what #1071 §3.4 step 3 reads and reorders, so a test about
/// column order has to be able to say a window is in the *wrong* one.
fn win_at(id: u64, workspace: u64, app_id: &str, column: usize) -> Window {
    let mut window = win(id, workspace, app_id);
    window.layout.pos_in_scrolling_layout = Some((column, 1));
    window
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

/// A `workspaces.toml`, with `order` in the order given — which is what
/// `names_in_order` hands the Start and the autostart run.
fn stacked(stacks: &[(&str, Stack)]) -> Workspaces {
    Workspaces {
        order: stacks.iter().map(|(name, _)| (*name).to_owned()).collect(),
        stacks: stacks
            .iter()
            .map(|(name, stack)| ((*name).to_owned(), stack.clone()))
            .collect(),
    }
}

/// A stack that autostarts, optionally pinned to a monitor.
fn autostarting(monitor: Option<&str>, apps: &[&str]) -> Stack {
    Stack {
        monitor: monitor.map(str::to_owned),
        autostart: true,
        ..stack(apps)
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
    /// A `monitor` rewrite, likewise recorded rather than performed.
    SetMonitor(String, String),
    /// A `$XDG_DATA_DIRS` desktop-entry lookup, answered from the script
    /// (#1071 §3.2).
    DesktopEntry(String),
    /// A `DBusActivatable=true` entry started through its own entry rather than
    /// an `Exec` line (#1071 §3.2). Nothing forked, nothing in the slice.
    Activate(String),
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
    /// The last snapshot [`Script::workspaces`] handed back, for the
    /// consistency check in that method.
    last_workspaces: Vec<Workspace>,
    slices_up: BTreeSet<String>,
    /// pid → unit, for `unit_for_pid`.
    units: BTreeMap<u32, String>,
    launch_error: Option<String>,
    /// When set, the scripted `workspaces.toml` write fails with it.
    save_error: Option<String>,
    /// desktop id → the entry `$XDG_DATA_DIRS` would have yielded. Absent = the
    /// id names no installed entry, which is §3.2's "no desktop entry" case.
    entries: BTreeMap<String, Launchable>,
    /// When set, a scripted activation fails with it.
    activate_error: Option<String>,
    /// The argv of every launch, in order — `Call::Launch` carries only the unit
    /// name, and #1071 §3.2 is entirely about what ends up after the `--`.
    argvs: Vec<Vec<String>>,
    /// The `(name, stack)` of every `save_stack`, in order. `Call::SaveStack`
    /// carries only the name, and #1071 §3.7 is about what is *in* the entry the
    /// Save creates.
    saved_stacks: Vec<(String, Stack)>,
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

    /// An installed desktop entry per id whose `Exec=` is the id itself.
    ///
    /// The dull case, for the tests that are about something else entirely: from
    /// phase 4 on, an app whose id names **no** entry launches nothing at all
    /// (#1071 §3.2), so a test asserting that a unit was started has to say that
    /// the entry exists. It says only that, deliberately — the tests that are
    /// about §3.2 script real `Exec` lines with [`Script::with_entry`].
    fn with_plain_entries(self, ids: &[&str]) -> Self {
        {
            let mut state = self.0.borrow_mut();
            for id in ids {
                state.entries.insert(
                    (*id).to_owned(),
                    Launchable {
                        exec: (*id).to_owned(),
                        dbus_activatable: false,

                        try_exec_missing: false,
                    },
                );
            }
        }
        self
    }

    /// An installed desktop entry whose `Exec=` is `exec` (#1071 §3.2).
    fn with_entry(self, id: &str, exec: &str) -> Self {
        self.0.borrow_mut().entries.insert(
            id.to_owned(),
            Launchable {
                exec: exec.to_owned(),
                dbus_activatable: false,

                try_exec_missing: false,
            },
        );
        self
    }

    /// An installed desktop entry carrying `DBusActivatable=true`. Its `Exec` is
    /// still stated, because the point of every test using this is that the
    /// `Exec` is the thing **not** run.
    fn with_dbus_entry(self, id: &str, exec: &str) -> Self {
        self.0.borrow_mut().entries.insert(
            id.to_owned(),
            Launchable {
                exec: exec.to_owned(),
                dbus_activatable: true,

                try_exec_missing: false,
            },
        );
        self
    }

    /// The argv of every `systemd-run` launch, in order.
    fn launch_argvs(&self) -> Vec<Vec<String>> {
        self.0.borrow().argvs.clone()
    }

    /// The `(name, stack)` of every `workspaces.toml` write, in order.
    fn saved_stacks(&self) -> Vec<(String, Stack)> {
        self.0.borrow().saved_stacks.clone()
    }

    /// How many separate `send_actions` batches were sent.
    ///
    /// "One batch" is a property of the *count*, not of the contents — which is
    /// why it needs its own accessor rather than being read off `actions()`,
    /// which flattens them.
    fn batches(&self) -> usize {
        self.0
            .borrow()
            .calls
            .iter()
            .filter(|c| matches!(c, Call::Actions(_)))
            .count()
    }

    /// The ids activated through their desktop entry, in order.
    fn activations(&self) -> Vec<String> {
        self.0
            .borrow()
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::Activate(id) => Some(id.clone()),
                _ => None,
            })
            .collect()
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
        let snapshot: Vec<Workspace> = state
            .workspaces
            .get(n)
            .or_else(|| state.workspaces.last())
            .cloned()
            .unwrap_or_default();

        // A queued snapshot may only take a **name** off a workspace if an
        // `UnsetName` for it was actually sent.
        //
        // This is a guard on the fake, not a rewrite of it, and it is the
        // second of the two blind spots #1106's re-review found: a canned
        // sequence like `[lingering, released, …]` hands back "released"
        // whether or not the housekeeping did anything, so a `launches()`
        // assertion cannot tell a working release from a missing one. With
        // this, a test that scripts a released name is asserting that the
        // release happened.
        //
        // Only the Some → None transition is checked. A `SetName` that niri
        // silently swallowed is a real behaviour some tests script
        // deliberately, and a workspace disappearing entirely is niri's own
        // clean-up.
        let released: BTreeSet<u64> = state
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::Actions(actions) => Some(actions),
                _ => None,
            })
            .flatten()
            .filter_map(|a| match a {
                WorkspaceAction::UnsetName { workspace } => Some(*workspace),
                _ => None,
            })
            .collect();
        for was in &state.last_workspaces {
            let Some(name) = was.name.as_deref() else {
                continue;
            };
            let gone = snapshot
                .iter()
                .any(|now| now.id == was.id && now.name.is_none());
            assert!(
                !gone || released.contains(&was.id),
                "scripted niri is inconsistent: workspace {} lost the name {name:?} \
                 with no UnsetName sent for it. Either the transaction under test \
                 skipped the release, or the snapshot queue is lying about a \
                 release that never happened. Calls so far: {:?}",
                was.id,
                state.calls,
            );
        }
        state.last_workspaces = snapshot.clone();
        Ok(snapshot)
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
        state.argvs.push(launch.argv.clone());
        state.launch_error.clone().map_or(Ok(()), Err)
    }

    async fn desktop_entry(&self, id: &str) -> Option<Launchable> {
        let mut state = self.0.borrow_mut();
        state.calls.push(Call::DesktopEntry(id.to_owned()));
        state.entries.get(id).cloned()
    }

    async fn activate(&self, id: &str) -> Result<(), String> {
        let mut state = self.0.borrow_mut();
        state.calls.push(Call::Activate(id.to_owned()));
        state.activate_error.clone().map_or(Ok(()), Err)
    }

    async fn save_stack(&self, name: &str, stack: &Stack) -> Result<(), String> {
        // Records; never writes. This is the whole point of the seam — see
        // `Ops::save_stack` — so do not "improve" this into a real write behind
        // a tempdir either: the transaction has no business knowing where the
        // file is, and a test that owns a path is a test that can leak one.
        let mut state = self.0.borrow_mut();
        state.calls.push(Call::SaveStack(name.to_owned()));
        state.saved_stacks.push((name.to_owned(), stack.clone()));
        state.save_error.clone().map_or(Ok(()), Err)
    }

    async fn set_monitor(&self, name: &str, monitor: &str) -> Result<(), String> {
        // Recorded, never written — `Ops::set_monitor`'s doc says why the
        // write is on the seam. `set_stack_monitor` resolves its own
        // `XDG_CONFIG_HOME` path, so a test that reached it would edit the
        // developer's real `workspaces.toml`.
        let mut state = self.0.borrow_mut();
        state
            .calls
            .push(Call::SetMonitor(name.to_owned(), monitor.to_owned()));
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

/// Hold the `STARTING` mark for `name` the way `spawn_start` and
/// `spawn_autostart` do — synchronously, **before** the transaction runs — and
/// take it back off however the test ends.
///
/// The second of #1106's re-review blind spots: every release test called
/// `start` directly, so the one guard under test was never armed on the stack
/// under test, and a Start that skipped its own housekeeping passed.
///
/// `STARTING` is process-global, so the guard is `Drop`-based: a panicking
/// assertion must not leave a name marked for the rest of the binary.
struct StartingMark(String);

impl StartingMark {
    fn hold(name: &str) -> Self {
        super::STARTING.lock_mut().insert(name.to_owned());
        Self(name.to_owned())
    }
}

impl Drop for StartingMark {
    fn drop(&mut self) {
        super::STARTING.lock_mut().remove(&self.0);
    }
}

/// `block_on` for the `async` transactions. A current-thread runtime, because
/// `Script` is `Rc`-backed and deliberately not `Send` — these tests drive one
/// transaction and assert on its trace, and nothing here needs a thread pool.
///
/// `enable_time` is for [`super::run_start`]'s `STARTING_CEILING` only: every
/// wait a transaction does goes through `Ops::sleep`, which `Script` answers
/// instantly, so no test ever spends wall-clock time here. Without a time
/// driver `tokio::time::timeout` panics rather than returning.
/// Let the executor run whatever a `Mutable::set` just woke.
///
/// A handful of yields rather than a sleep: setting a `Mutable` calls the
/// subscribed task's waker, and the next turn of the current-thread executor
/// polls it. No timer is involved, so this is deterministic — and eight turns
/// is far more than the one or two a `for_each` over one signal needs.
async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

fn run<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
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
        plan_start("chat", &Stack::default(), &named, &[], &[]),
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
        &[],
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
    let plan = plan_start("chat", &Stack::default(), &workspaces, &[], &[]).expect("plans");

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
    let plan = plan_start("chat", &Stack::default(), &workspaces, &windows, &[]).expect("plans");

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
        plan_start("chat", &Stack::default(), &workspaces, &windows, &[]),
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
    let plan = plan_start("chat", &on_right, &both, &[], &[]).expect("plans");
    assert_eq!(plan.output, RIGHT);
    assert_eq!(plan.workspace, 3, "not the focused workspace on DP-1");
    assert!(
        !plan.adopted,
        "the focused workspace is on the other screen"
    );

    let only_left = [ws(1, 1, LEFT, None, true)];
    let plan = plan_start("chat", &on_right, &only_left, &[], &[]).expect("plans");
    assert_eq!(plan.output, LEFT, "HDMI-A-1 is not connected");
    assert!(plan.adopted);
}

/// The batch is exactly the two id-addressed actions, in order: name, then
/// focus. Focus has to land before anything launches — new windows open on the
/// focused workspace.
#[test]
fn the_batch_names_then_focuses() {
    let workspaces = [ws(1, 1, LEFT, None, true)];
    let plan = plan_start("chat", &Stack::default(), &workspaces, &[], &[]).expect("plans");
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
        plan_start("chat", &Stack::default(), &workspaces, &[], &[]),
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

    let released = names_to_release(&stacks, &workspaces, &[], &|_| false, &BTreeSet::new());
    assert_eq!(released, vec![WorkspaceAction::UnsetName { workspace: 1 }]);

    // Without the release, this is what the next Start runs into.
    assert_eq!(
        plan_start("chat", &Stack::default(), &workspaces, &[], &[]),
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
        names_to_release(
            &stacks,
            &workspaces,
            &[win(9, 1, "x")],
            &|_| false,
            &BTreeSet::new()
        )
        .is_empty(),
        "it still has a window"
    );
    assert!(
        names_to_release(
            &stacks,
            &workspaces,
            &[],
            &|name| name == "chat",
            &BTreeSet::new()
        )
        .is_empty(),
        "its units are still up"
    );
    // A workspace the user named by hand is not ours to unname.
    let unknown = [ws(1, 1, LEFT, Some("scratch"), true)];
    assert!(names_to_release(&stacks, &unknown, &[], &|_| false, &BTreeSet::new()).is_empty());
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
        &Workspaces::default(),
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
        .with_windows(&[Vec::new(), Vec::new(), settled])
        .with_plain_entries(&["firefox", "Alacritty"]);

    let mut s = stack(&["firefox", "Alacritty"]);
    s.layout = Layout::Golden;
    let plan = run(start(&script, "chat", &s, &Workspaces::default())).expect("starts");
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
        &Workspaces::default(),
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

// ── §3.2: `Exec` resolution (phase 4) ────────────────────────────────────────

/// A stack app with an `exec` override.
fn overridden(id: &str, exec: &str) -> StackApp {
    StackApp {
        id: id.to_owned(),
        exec: Some(exec.to_owned()),
    }
}

/// A stack app that is nothing but a desktop-entry id.
fn by_id(id: &str) -> StackApp {
    StackApp {
        id: id.to_owned(),
        exec: None,
    }
}

/// An installed entry with the given `Exec=`.
fn entry(exec: &str) -> Launchable {
    Launchable {
        exec: exec.to_owned(),
        dbus_activatable: false,

        try_exec_missing: false,
    }
}

/// The argv [`app_start`] resolved, or `None` for anything but a unit launch.
fn argv_of(start: &AppStart) -> Option<Vec<String>> {
    match start {
        AppStart::Unit(launch) => Some(launch.argv.clone()),
        _ => None,
    }
}

/// `&["a", "b"]` as the `Vec<String>` an argv actually is.
fn owned(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// One app's unit name, slice and argv.
#[test]
fn an_app_launches_into_the_stacks_own_slice() {
    let start = app_start(
        "chat",
        1,
        &overridden("Alacritty", "alacritty -e weechat"),
        None,
    );
    let AppStart::Unit(launch) = start else {
        panic!("an override is a unit launch, got {start:?}");
    };
    assert_eq!(launch.unit, "trollshell-ws-chat-1.service");
    assert_eq!(launch.slice.as_deref(), Some("trollshell-ws-chat.slice"));
    assert_eq!(launch.argv, ["alacritty", "-e", "weechat"]);
    assert!(
        launch.properties.is_empty(),
        "an app the user closed has finished; it is not a supervised service"
    );
    assert!(launch.secret_env.is_empty(), "no keyring injection here");
}

/// #1071 §3.2: *"Restore = the entry's `Exec` with field codes stripped"*.
///
/// The table, at the seam a Start actually uses. `desktop_entry`'s own tests
/// pin the stripper; this pins that the stripper is what a launch goes through —
/// **the mutation is `app_start` using `entry.exec` unstripped**, which reds
/// here and nowhere else.
#[test]
fn a_resolved_entry_launches_its_exec_with_the_field_codes_stripped() {
    let cases: [(&str, &[&str]); 5] = [
        ("firefox %u", &["firefox"]),
        ("firefox %U", &["firefox"]),
        (
            "/usr/bin/nautilus --new-window %F",
            &["/usr/bin/nautilus", "--new-window"],
        ),
        ("prog %i %c %k --flag", &["prog", "--flag"]),
        // `%%` is the spec's escape for a literal percent and survives as one.
        ("prog 100%% %f", &["prog", "100%"]),
    ];
    for (exec, want) in cases {
        let start = app_start("chat", 0, &by_id("app"), Some(&entry(exec)));
        assert_eq!(
            argv_of(&start),
            Some(owned(want)),
            "Exec={exec:?} resolved wrongly"
        );
    }
}

/// §3.2: *"or the override verbatim"*.
///
/// Verbatim means the field codes are **not** stripped from it: a `%` in a
/// launch command someone typed is theirs, and this is not an `Exec` line. The
/// override also wins over the entry entirely — including over a
/// `DBusActivatable=true` one, which is the whole reason the field exists.
#[test]
fn an_override_is_taken_verbatim_and_beats_the_entry() {
    let start = app_start(
        "chat",
        0,
        &overridden("app", "prog --pct 50%u"),
        Some(&entry("other")),
    );
    assert_eq!(
        argv_of(&start),
        Some(owned(&["prog", "--pct", "50%u"])),
        "the override was field-code stripped, or the entry won"
    );

    let dbus = Launchable {
        exec: "never-run".to_owned(),
        dbus_activatable: true,

        try_exec_missing: false,
    };
    let start = app_start("chat", 0, &overridden("app", "prog"), Some(&dbus));
    assert_eq!(
        argv_of(&start),
        Some(owned(&["prog"])),
        "a DBusActivatable entry swallowed the user's own launch command"
    );
}

/// §3.2: *"a `DBusActivatable=true` entry launches through … `launch` instead
/// of a raw `Exec`"*.
///
/// **The mutation**: treating `DBusActivatable` as an ordinary entry (i.e.
/// dropping the branch) reds this — the start becomes a `Unit` carrying
/// `never-run`.
#[test]
fn a_dbus_activatable_entry_is_activated_rather_than_executed() {
    let dbus = Launchable {
        exec: "never-run %U".to_owned(),
        dbus_activatable: true,

        try_exec_missing: false,
    };
    let start = app_start("chat", 0, &by_id("org.gnome.Nautilus"), Some(&dbus));
    assert_eq!(
        start,
        AppStart::Activate {
            id: "org.gnome.Nautilus".to_owned()
        }
    );
    assert!(
        argv_of(&start).is_none(),
        "a D-Bus activation must not also fork an Exec line"
    );
}

/// §3.2: *"A desktop id with no entry and no override → one warning naming it,
/// Start continues with the rest."*
///
/// Phase 2 ran the **id itself** as a command here, so `org.mozilla.firefox`
/// launched nothing while looking like it had launched something (#1106's own
/// "Known limits"). There is deliberately no such fallback any more.
#[test]
fn an_id_that_names_no_entry_resolves_to_nothing_rather_than_to_itself() {
    let start = app_start("chat", 0, &by_id("org.mozilla.firefox"), None);
    assert_eq!(
        start,
        AppStart::Unresolved {
            id: "org.mozilla.firefox".to_owned(),
            why: Unresolvable::NoEntry,
        },
        "the bare id was run as a command again"
    );

    // An entry whose `Exec` is nothing but field codes has no command left
    // either, and an empty argv is not a launch.
    let start = app_start("chat", 0, &by_id("app"), Some(&entry("%U")));
    assert_eq!(
        start,
        AppStart::Unresolved {
            id: "app".to_owned(),
            why: Unresolvable::NoCommand,
        }
    );
    // …and neither is an override that is only whitespace.
    let start = app_start("chat", 0, &overridden("app", "   "), None);
    assert_eq!(
        start,
        AppStart::Unresolved {
            id: "app".to_owned(),
            why: Unresolvable::NoCommand,
        }
    );
}

/// Review LOW 12: an entry whose `TryExec` names a missing program is not
/// launched — GIO would not have built a `GDesktopAppInfo` for it either.
///
/// Without this the stack launches a command that is not there, which surfaces
/// as a unit *start failure* rather than as §3.2's "nothing to start" warning
/// naming the app — and the three reasons send the user to different fixes, so
/// the warning says which one it was.
///
/// **The mutation**: dropping the `try_exec_missing` branch reds this.
#[test]
fn an_entry_whose_try_exec_is_missing_is_not_launched() {
    let absent = Launchable {
        exec: "ghost --window".to_owned(),
        dbus_activatable: false,
        try_exec_missing: true,
    };
    assert_eq!(
        app_start("chat", 0, &by_id("ghost"), Some(&absent)),
        AppStart::Unresolved {
            id: "ghost".to_owned(),
            why: Unresolvable::NotInstalled,
        }
    );

    // …even when it also asks for D-Bus activation: not installed is not
    // installed.
    let absent_dbus = Launchable {
        dbus_activatable: true,
        ..absent.clone()
    };
    assert!(matches!(
        app_start("chat", 0, &by_id("org.gnome.Ghost"), Some(&absent_dbus)),
        AppStart::Unresolved {
            why: Unresolvable::NotInstalled,
            ..
        }
    ));

    // But an **override** is the user's own statement about how to start it and
    // still wins — they may know something the packaged entry does not.
    assert_eq!(
        argv_of(&app_start(
            "chat",
            0,
            &overridden("ghost", "my-ghost"),
            Some(&absent)
        )),
        Some(owned(&["my-ghost"]))
    );
}

/// **Review MEDIUM 7**: `DBusActivatable=true` is honoured only for an id that
/// is a usable D-Bus name; everything else takes the ordinary launcher into the
/// stack's own slice.
///
/// GIO takes its D-Bus path only for a valid derived bus name and otherwise
/// forks the child into **this shell's own cgroup** (measured, and recorded on
/// `may_stop`) — so an entry declaring activation under a one-element id like
/// `Alacritty` was not being activated at all: it was forked under
/// `trollshell.service`, in no slice, dying with the next shell restart.
///
/// **The mutation**: dropping the `is_valid_bus_name` conjunct reds this.
#[test]
fn a_dbus_activatable_entry_with_an_unusable_id_is_launched_not_activated() {
    let dbus = Launchable {
        exec: "alacritty %U".to_owned(),
        dbus_activatable: true,
        try_exec_missing: false,
    };

    // One element: not a bus name, so GIO would have forked it under the shell.
    assert_eq!(
        argv_of(&app_start("chat", 0, &by_id("Alacritty"), Some(&dbus))),
        Some(owned(&["alacritty"])),
        "an unusable bus name must take the launcher, into the stack's slice"
    );

    // Two elements: a real bus name, so activation is the right path.
    assert_eq!(
        app_start("chat", 0, &by_id("org.gnome.Nautilus"), Some(&dbus)),
        AppStart::Activate {
            id: "org.gnome.Nautilus".to_owned()
        }
    );
}

/// End to end through a real Start: the three kinds side by side, so the
/// resolution is pinned where the transaction uses it and not only in the pure
/// function.
#[test]
fn a_start_resolves_each_apps_entry_and_launches_activates_or_warns() {
    let stack = Stack {
        apps: vec![
            by_id("org.mozilla.firefox"),
            by_id("org.gnome.Nautilus"),
            by_id("never.installed"),
            overridden("Alacritty", "alacritty -e weechat"),
        ],
        ..Stack::default()
    };
    let before = vec![ws(1, 1, LEFT, None, true)];
    let after = vec![ws(1, 1, LEFT, Some("chat"), true)];
    let script = Script::default()
        .with_workspaces(&[before.clone(), before, after])
        .with_entry("org.mozilla.firefox", "firefox --name firefox %u")
        .with_dbus_entry("org.gnome.Nautilus", "nautilus %U");

    run(start(&script, "chat", &stack, &Workspaces::default())).expect("starts");

    assert_eq!(
        script.launch_argvs(),
        vec![
            vec![
                "firefox".to_owned(),
                "--name".to_owned(),
                "firefox".to_owned()
            ],
            vec![
                "alacritty".to_owned(),
                "-e".to_owned(),
                "weechat".to_owned()
            ],
        ],
        "only the Exec-resolved app and the override forked: {:?}",
        script.calls()
    );
    assert_eq!(script.activations(), ["org.gnome.Nautilus"]);
    // The unresolved one launched nothing at all, and the apps after it still
    // ran — §3.2's "Start continues with the rest".
    assert_eq!(script.launches().len(), 2);

    // An app carrying an override costs no entry lookup: `app_start` would
    // ignore the answer.
    let looked_up: Vec<String> = script
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::DesktopEntry(id) => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(
        looked_up,
        [
            "org.mozilla.firefox",
            "org.gnome.Nautilus",
            "never.installed"
        ],
        "the overridden app was looked up anyway"
    );
}

/// A D-Bus activation records **no unit**, because the bus started the process
/// and it is in nobody's slice — claiming one would make Stop's
/// `Launched::units` name a unit that does not exist.
#[test]
fn an_activated_app_contributes_no_unit_to_the_launch_record() {
    let stack = Stack {
        apps: vec![by_id("org.gnome.Nautilus")],
        ..Stack::default()
    };
    let before = vec![ws(1, 1, LEFT, None, true)];
    let after = vec![ws(1, 1, LEFT, Some("chat"), true)];
    let script = Script::default()
        .with_workspaces(&[before.clone(), before, after])
        .with_dbus_entry("org.gnome.Nautilus", "nautilus %U");

    run(start(&script, "chat", &stack, &Workspaces::default())).expect("starts");

    assert!(
        script.launches().is_empty(),
        "an activation forked a systemd-run unit as well: {:?}",
        script.calls()
    );
    assert_eq!(script.activations(), ["org.gnome.Nautilus"]);
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
    let released = names_to_release(
        &stacks,
        &workspaces,
        &[win(9, 2, "x")],
        &|name| name == "music",
        &BTreeSet::new(),
    );
    assert_eq!(released, vec![WorkspaceAction::UnsetName { workspace: 1 }]);
}

/// A Start runs housekeeping before it plans, so a stack that was Stopped by
/// closing every window by hand can be Started again straight away.
///
/// This is the §7 housekeeping mutation at the transaction level: with the
/// release skipped, the plan below sees its own lingering name and fails.
///
/// **The mark matters** (#1106 re-review R1). `spawn_start` inserts into
/// `STARTING` *before* the transaction is scheduled and `run_start` only takes
/// it off afterwards, so the stack under test is in flight for the whole of its
/// own Start. Without [`StartingMark`] here, this test ran the transaction in a
/// world no caller ever produces — and stayed green while a guard that reads
/// `STARTING` skipped the one name the housekeeping exists to free.
///
/// The scripted "released" snapshot is now load-bearing too: `Script::workspaces`
/// refuses to hand back a snapshot that drops a name unless the `UnsetName`
/// really was sent.
#[test]
fn a_start_releases_the_stale_name_before_planning_its_own() {
    // A stack name no other test uses: this one holds the process-global
    // `STARTING` mark, and the suite runs in parallel.
    let name = "r1click";
    let lingering = vec![ws(1, 1, LEFT, Some(name), true)];
    // After housekeeping, niri reports the name gone.
    let released = vec![ws(1, 1, LEFT, None, true)];
    let named = vec![ws(1, 1, LEFT, Some(name), true)];
    let script = Script::default()
        .with_workspaces(&[lingering, released, named])
        .with_windows(&[Vec::new()]);

    let _mark = StartingMark::hold(name);
    let saved = stacked(&[(name, Stack::default())]);
    let plan = run(start(&script, name, &Stack::default(), &saved)).expect("starts");

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

    let saved = stacked(&[("chat", Stack::default())]);
    let err = run(start(&script, "chat", &Stack::default(), &saved)).expect_err("refuses");
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

/// #1071 §3.7, end to end: the entry a Save creates carries the workspace's app
/// ids, the command line of the one with no desktop entry, and the monitor — and
/// the naming rides **one** batch, so the saved workspace *is* the Active card.
///
/// The payload is built by `panels::workspace_edit` (`ephemeral_apps` +
/// `plan_save`, both pure and tested there); what this pins is that the
/// transaction carries it through unaltered and in the right order.
///
/// **The mutation the brief names**: naming the workspace in a *second* batch.
/// `batches()` counts `send_actions` calls rather than actions, so splitting the
/// `SetName` off into its own later send reds here — `actions()` alone could not
/// tell the difference, because it flattens.
#[test]
fn an_ephemeral_save_carries_the_whole_entry_and_names_the_workspace_in_one_batch() {
    let before = vec![ws(7, 2, LEFT, None, true)];
    let after = vec![ws(7, 2, LEFT, Some("chat"), true)];
    let script = Script::default().with_workspaces(&[before, after]);

    // What the Edit form hands over for an ephemeral card: the windows' app ids
    // in column order, the unknown one carrying its running command line, and
    // the screen niri reported.
    let stack = Stack {
        monitor: Some(LEFT.to_owned()),
        apps: vec![
            by_id("org.mozilla.firefox"),
            overridden("weird-app", "/home/me/bin/weird --flag"),
        ],
        ..Stack::default()
    };
    run(save(&script, 7, "chat", &stack)).expect("saves");

    assert_eq!(
        script.saved_stacks(),
        vec![("chat".to_owned(), stack)],
        "the entry written is not the one the form described"
    );
    assert_eq!(
        script.batches(),
        1,
        "§3.7's naming must ride ONE batch: {:?}",
        script.calls()
    );
    assert_eq!(
        script.actions(),
        vec![WorkspaceAction::SetName {
            workspace: 7,
            name: "chat".to_owned()
        }],
        "named by id — so the card the user was looking at becomes the saved one"
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

// ── §3.4 step 3: column order ────────────────────────────────────────────────

/// Focus, then move, once per app, **in stack order** and with 1-based indices.
///
/// The stack order *is* niri's column order (Annika, 2026-09-10), so this is
/// the whole feature stated once: the windows start in the wrong columns and
/// the batch says where each belongs.
#[test]
fn the_column_batch_is_a_focus_and_a_move_per_app_in_stack_order() {
    let windows = [
        win_at(11, 1, "Alacritty", 1),
        win_at(12, 1, "firefox", 2),
        win_at(13, 1, "thunderbird", 3),
    ];
    assert_eq!(
        column_order_batch(
            &stack(&["firefox", "thunderbird", "Alacritty"]),
            1,
            &windows
        ),
        [
            WorkspaceAction::FocusWindow { window: 12 },
            WorkspaceAction::MoveColumnToIndex { index: 1 },
            WorkspaceAction::FocusWindow { window: 13 },
            WorkspaceAction::MoveColumnToIndex { index: 2 },
            WorkspaceAction::FocusWindow { window: 11 },
            WorkspaceAction::MoveColumnToIndex { index: 3 },
        ]
    );
}

/// `MoveColumnToIndex` has **no target** — it moves the focused column — so
/// the `FocusWindow` in front of it is the only thing that says which column.
///
/// The §7 mutation for this row is dropping the move (the batch stops ordering
/// anything); this is the other half, and it is the one that would *corrupt*
/// rather than merely fail: an unpaired move relocates whatever the compositor
/// happened to have focused.
#[test]
fn every_move_column_is_addressed_by_the_focus_before_it() {
    let windows = [
        win_at(11, 1, "firefox", 3),
        win_at(12, 1, "Alacritty", 1),
        // Not on this workspace: never touched.
        win_at(13, 2, "thunderbird", 1),
    ];
    let batch = column_order_batch(
        &stack(&["firefox", "Alacritty", "thunderbird"]),
        1,
        &windows,
    );
    assert!(
        !batch.is_empty(),
        "the assertion below is vacuous on an empty batch"
    );
    for (i, action) in batch.iter().enumerate() {
        if matches!(action, WorkspaceAction::MoveColumnToIndex { .. }) {
            assert!(
                matches!(
                    i.checked_sub(1).and_then(|p| batch.get(p)),
                    Some(WorkspaceAction::FocusWindow { .. })
                ),
                "a column move with no focus in front of it at {i}: {batch:?}"
            );
        }
    }
    assert!(
        !batch.contains(&WorkspaceAction::FocusWindow { window: 13 }),
        "a window on another workspace is not this workspace's column: {batch:?}"
    );
}

/// §3.4 step 3: *"A window that never arrived leaves a gap that the next ones
/// close up."* So the index counts the apps that **have** a window, not their
/// position in the stack — otherwise the third app would be moved to index 3
/// with nothing at 2, and niri would clamp it back to 2 anyway, leaving the
/// order right only by luck.
#[test]
fn a_missing_app_leaves_a_gap_the_next_ones_close_up() {
    let windows = [win_at(11, 1, "firefox", 2), win_at(12, 1, "thunderbird", 1)];
    assert_eq!(
        column_order_batch(
            &stack(&["firefox", "Alacritty", "thunderbird"]),
            1,
            &windows
        ),
        [
            WorkspaceAction::FocusWindow { window: 11 },
            WorkspaceAction::MoveColumnToIndex { index: 1 },
            WorkspaceAction::FocusWindow { window: 12 },
            // 2, not 3: `Alacritty` never opened.
            WorkspaceAction::MoveColumnToIndex { index: 2 },
        ]
    );
}

/// Two entries of one app take two different windows, leftmost first — rather
/// than both naming the first one, which would move one column twice and leave
/// the other wherever it landed.
#[test]
fn two_entries_of_one_app_take_two_windows() {
    let windows = [win_at(11, 1, "Alacritty", 2), win_at(12, 1, "Alacritty", 1)];
    assert_eq!(
        column_order_batch(&stack(&["Alacritty", "Alacritty"]), 1, &windows),
        [
            // 12 is in column 1, so it is the leftmost and goes first.
            WorkspaceAction::FocusWindow { window: 12 },
            WorkspaceAction::MoveColumnToIndex { index: 1 },
            WorkspaceAction::FocusWindow { window: 11 },
            WorkspaceAction::MoveColumnToIndex { index: 2 },
        ]
    );
}

/// End to end: the ordering rides **one** batch, after the last launch and
/// before the layout.
///
/// One `Call::Actions` for the whole sequence is the load-bearing half —
/// `MoveColumnToIndex` addresses the focused column, so a per-app batch would
/// be a per-app socket and the user's own focus could land in between.
#[test]
fn a_start_orders_the_columns_in_one_batch_between_the_launches_and_the_layout() {
    let before = vec![ws(1, 1, LEFT, None, true)];
    let after = vec![ws(1, 1, LEFT, Some("chat"), true)];
    // Launched in stack order but opened in the other one.
    let settled = vec![win_at(9, 1, "Alacritty", 1), win_at(10, 1, "firefox", 2)];
    let script = Script::default()
        .with_workspaces(&[before.clone(), before, after])
        .with_windows(&[Vec::new(), Vec::new(), settled])
        .with_plain_entries(&["firefox", "Alacritty"]);

    let mut s = stack(&["firefox", "Alacritty"]);
    s.layout = Layout::Golden;
    run(start(&script, "chat", &s, &Workspaces::default())).expect("starts");

    let ordering = vec![
        WorkspaceAction::FocusWindow { window: 10 },
        WorkspaceAction::MoveColumnToIndex { index: 1 },
        WorkspaceAction::FocusWindow { window: 9 },
        WorkspaceAction::MoveColumnToIndex { index: 2 },
        // …and the focus the layout CLI needs, last in the same batch
        // (#1106 review F2).
        WorkspaceAction::Focus { workspace: 1 },
    ];
    let calls = script.calls();
    let batch = script
        .position(|c| *c == Call::Actions(ordering.clone()))
        .unwrap_or_else(|| panic!("the whole ordering in ONE batch: {calls:?}"));
    let last_launch = calls
        .iter()
        .rposition(|c| matches!(c, Call::Launch(_)))
        .expect("a launch");
    let layout = script
        .position(|c| matches!(c, Call::Layout(_)))
        .expect("a layout");
    assert!(last_launch < batch, "after the launches: {calls:?}");
    assert!(batch < layout, "before the layout: {calls:?}");
}

// ── §3.4 step 4: the layout ──────────────────────────────────────────────────

/// `none` spawns nothing at all (#1071 §3.4 step 4).
///
/// Stated against the transaction rather than against `Live::apply_layout`,
/// which is why `start` filters rather than the live arm: nothing in a test can
/// observe whether a `tokio::process::Command` was built.
#[test]
fn a_stack_with_no_layout_spawns_nothing() {
    let before = vec![ws(1, 1, LEFT, None, true)];
    let after = vec![ws(1, 1, LEFT, Some("chat"), true)];
    let script = Script::default()
        .with_workspaces(&[before.clone(), before, after])
        .with_windows(&[Vec::new(), Vec::new(), vec![win(9, 1, "firefox")]]);

    let s = stack(&["firefox"]);
    assert_eq!(
        s.layout,
        Layout::None,
        "the default, and the case under test"
    );
    run(start(&script, "chat", &s, &Workspaces::default())).expect("starts");

    assert!(
        script.position(|c| matches!(c, Call::Layout(_))).is_none(),
        "no layout was spawned: {:?}",
        script.calls()
    );
}

/// The apps a stack is still missing when the grace window ends, in stack
/// order — what the one warning names (#1071 §3.4 step 4).
#[test]
fn missing_apps_names_what_never_arrived() {
    let windows = [win(9, 1, "firefox"), win(10, 2, "thunderbird")];
    assert_eq!(
        missing_apps(
            &stack(&["firefox", "Alacritty", "thunderbird"]),
            1,
            &windows
        ),
        ["Alacritty", "thunderbird"],
        "a window on another workspace is not this stack's"
    );
    assert!(missing_apps(&stack(&["firefox"]), 1, &windows).is_empty());
}

/// An app that never opened a window does not stop the layout: §3.4 step 4 is
/// "after every app has launched **or the grace window ends**", and a browser
/// that is slow to map must not leave the workspace un-laid-out forever.
#[test]
fn the_layout_still_runs_when_an_app_never_arrived() {
    let before = vec![ws(1, 1, LEFT, None, true)];
    let after = vec![ws(1, 1, LEFT, Some("chat"), true)];
    let script = Script::default()
        .with_workspaces(&[before.clone(), before, after])
        .with_windows(&[Vec::new(), Vec::new(), vec![win(9, 1, "firefox")]]);

    let mut s = stack(&["firefox", "Alacritty"]);
    s.layout = Layout::Split;
    let (captured, _guard) = hytte_config::test_support::capture();
    run(start(&script, "chat", &s, &Workspaces::default())).expect("starts");

    assert_eq!(
        script
            .calls()
            .iter()
            .filter(|c| **c == Call::Layout(Layout::Split))
            .count(),
        1,
        "once, with the stack's layout: {:?}",
        script.calls()
    );
    let naming: Vec<String> = captured
        .events()
        .into_iter()
        .filter(|e| e.level == tracing::Level::WARN)
        .filter(|e| e.fields.get("missing").is_some_and(|m| m == "Alacritty"))
        .map(|e| e.message)
        .collect();
    assert_eq!(
        naming.len(),
        1,
        "exactly one warning naming the missing app: {:?}",
        captured.events()
    );
}

// ── §3.6: where a started workspace lands ────────────────────────────────────

/// The index is counted **among the peers on that output**, not globally.
///
/// The §7 mutation for this row is a global index: with `dev` on the other
/// screen ranking ahead of `chat`, a global count would put `chat` at 2 — a
/// position on `DP-1` that belongs to a workspace `chat` has nothing to do
/// with. Per output, `chat`'s only peer on `DP-1` is `music`, which ranks
/// *after* it, so `chat` goes where `music` is.
#[test]
fn the_saved_order_is_counted_per_output() {
    let order = ["dev".to_owned(), "chat".to_owned(), "music".to_owned()];
    let workspaces = [
        ws(1, 1, RIGHT, Some("dev"), false),
        ws(2, 1, LEFT, Some("music"), false),
        ws(3, 2, LEFT, None, true),
    ];
    assert_eq!(
        order_index("chat", LEFT, 3, &workspaces, &order),
        Some(1),
        "before `music`, which is where `music` sits on this screen"
    );
    assert_eq!(
        order_index("dev", LEFT, 3, &workspaces, &order),
        Some(1),
        "`dev` ranks before `music` too — and its namesake on the other screen \
         is not a peer here"
    );
}

/// A stack the saved order puts after every peer on its screen goes after the
/// last of them.
#[test]
fn a_stack_after_every_peer_lands_after_the_last_one() {
    let order = ["dev".to_owned(), "music".to_owned(), "chat".to_owned()];
    let workspaces = [
        ws(1, 1, LEFT, Some("dev"), false),
        ws(2, 2, LEFT, None, false),
        ws(3, 3, LEFT, Some("music"), false),
        ws(4, 4, LEFT, None, true),
    ];
    // `others` (this workspace removed) is [dev, spare, music]; `chat` goes
    // after `music`, which is position 3 — so index 4.
    assert_eq!(order_index("chat", LEFT, 4, &workspaces, &order), Some(4));
}

/// With no peer on the screen there is nothing to order against, so no move is
/// sent — the user's own workspaces are not shuffled to place the first stack.
#[test]
fn the_first_stack_on_a_screen_is_not_moved() {
    let order = ["chat".to_owned()];
    let workspaces = [
        ws(1, 1, LEFT, None, true),
        // A workspace the *user* named is not a peer: it is not a stack.
        ws(2, 2, LEFT, Some("scratch"), false),
        // Neither is a stack on the other screen.
        ws(3, 1, RIGHT, Some("chat"), false),
    ];
    assert_eq!(order_index("chat", LEFT, 1, &workspaces, &order), None);
}

/// …and end to end: the Start's one batch carries the placement, between the
/// naming and the focus.
#[test]
fn a_start_places_its_workspace_by_the_saved_order() {
    let saved = stacked(&[("chat", Stack::default()), ("music", Stack::default())]);
    let before = vec![
        ws(1, 1, LEFT, Some("music"), false),
        ws(2, 2, LEFT, None, true),
    ];
    let after = vec![
        ws(1, 1, LEFT, Some("music"), false),
        ws(2, 2, LEFT, Some("chat"), true),
    ];
    let script = Script::default()
        .with_workspaces(&[before.clone(), before, after])
        // `music` is Active — it has a window — so housekeeping leaves its name
        // alone and it stays a peer `chat` has to be ordered against.
        .with_windows(&[vec![win(9, 1, "spotify")]]);

    run(start(&script, "chat", &Stack::default(), &saved)).expect("starts");

    assert_eq!(
        script.actions(),
        [
            WorkspaceAction::SetName {
                workspace: 2,
                name: "chat".to_owned()
            },
            // `chat` is ordered before `music`, which is at position 1.
            WorkspaceAction::MoveWorkspaceToIndex {
                workspace: 2,
                index: 1
            },
            // Focus last: the launches below need it, and a workspace move
            // must not be able to carry it somewhere else afterwards.
            WorkspaceAction::Focus { workspace: 2 },
        ]
    );
}

// ── §3.5: autostart ──────────────────────────────────────────────────────────

/// Only `autostart = true`, only a connected (or unrecorded) screen, in **file
/// order** — not the `BTreeMap`'s alphabetical one.
#[test]
fn autostart_takes_the_connected_stacks_in_file_order() {
    let saved = stacked(&[
        ("zoo", autostarting(None, &["firefox"])),
        ("apt", autostarting(Some(LEFT), &["Alacritty"])),
        ("off", autostarting(Some("dp-9"), &["thunderbird"])),
        ("man", stack(&["firefox"])),
    ]);
    let connected = BTreeSet::from([LEFT.to_owned()]);

    assert_eq!(
        autostart_plan(&saved, &connected, &BTreeSet::new()),
        AutostartPlan {
            start: vec!["zoo".to_owned(), "apt".to_owned()],
            skipped: vec![("off".to_owned(), "dp-9".to_owned())],
        },
        "`man` does not autostart; `off` names a screen that is not here; and \
         `zoo` comes first because the file says so"
    );
    assert_eq!(
        autostart_plan(&saved, &connected, &BTreeSet::from(["zoo".to_owned()])).start,
        ["apt"],
        "a stack already handed to a launch never appears again"
    );
}

/// **Review LOW 7.** A screen that turns up a beat after niri's first snapshot
/// still counts: at login `trollshell.service` and `kanshi` come up together,
/// so the initial enumeration races the display setup.
///
/// So the skip is *deferred* while the settle window is open, and only becomes
/// final — one `info!` line, and the stack latched — once it closes. Three
/// ticks: inside the window with the screen absent (nothing), inside the window
/// with the screen present (started), and — on a second latch — the window
/// expiring with it still absent (the line).
#[test]
fn an_absent_monitor_waits_out_the_settle_window_then_is_skipped_with_one_line() {
    let saved = stacked(&[("off", autostarting(Some("dp-9"), &["firefox"]))]);
    let t0 = std::time::Instant::now();
    let only_left = [ws(1, 1, LEFT, None, true)];

    // 1. Inside the window, screen absent: nothing to launch, nothing final.
    let mut latch = super::AutostartLatch::default();
    let (captured, guard) = hytte_config::test_support::capture();
    assert!(autostart_tick(&mut latch, t0, &only_left, &saved).is_none());
    assert!(
        captured
            .events()
            .iter()
            .all(|e| e.level != tracing::Level::INFO),
        "no verdict yet: {:?}",
        captured.events()
    );

    // 2. The screen turns up a second later — still inside the window — and the
    //    stack starts after all. This is the whole point of LOW 7.
    let both = [ws(1, 1, LEFT, None, true), ws(2, 1, "dp-9", None, false)];
    let plan = autostart_tick(&mut latch, t0 + Duration::from_secs(1), &both, &saved)
        .expect("the screen arrived inside the settle window");
    assert_eq!(plan.start, ["off"]);
    drop(guard);

    // 3. On a fresh latch, the same stack with the screen still absent when the
    //    window closes: one info line, naming the stack and the screen.
    let mut latch = super::AutostartLatch::default();
    let (captured, _guard) = hytte_config::test_support::capture();
    assert!(autostart_tick(&mut latch, t0, &only_left, &saved).is_none());
    assert!(
        autostart_tick(
            &mut latch,
            t0 + super::AUTOSTART_SETTLE + Duration::from_secs(1),
            &only_left,
            &saved
        )
        .is_none(),
        "still nothing to start"
    );
    let lines: Vec<String> = captured
        .events()
        .into_iter()
        .filter(|e| e.level == tracing::Level::INFO)
        .filter(|e| {
            e.fields.get("workspace").is_some_and(|w| w == "off")
                && e.fields.get("monitor").is_some_and(|m| m == "dp-9")
        })
        .map(|e| e.message)
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "one line, naming the stack and the screen: {:?}",
        captured.events()
    );
}

/// Nothing happens until niri has reported an output — and then exactly once
/// per stack, however many more snapshots arrive.
///
/// **The §7 mutation**: drop the latch and every workspace or window change for
/// the rest of the session starts the stacks again.
#[test]
fn autostart_waits_for_an_output_and_then_runs_once() {
    let saved = stacked(&[("chat", autostarting(None, &["firefox"]))]);
    let mut latch = super::AutostartLatch::default();
    let t0 = std::time::Instant::now();

    assert!(
        autostart_tick(&mut latch, t0, &[], &saved).is_none(),
        "niri has reported nothing yet"
    );
    assert!(
        autostart_tick(&mut latch, t0, &[ws(1, 1, LEFT, None, true)], &saved).is_some(),
        "the first snapshot with an output fires it"
    );
    for n in 0..3 {
        assert!(
            autostart_tick(
                &mut latch,
                t0 + Duration::from_secs(n * 10),
                &[ws(1, 1, LEFT, None, true)],
                &saved
            )
            .is_none(),
            "and never again — including long after the settle window"
        );
    }
}

/// …and the latch really lives outside the per-snapshot closure, which the
/// pure `autostart_tick` test above cannot see: a latch created *inside*
/// `for_each` would be empty on every tick and pass it unchanged.
#[test]
fn the_driver_latches_across_snapshots() {
    use hytte::futures_signals::signal::Mutable;

    let saved = stacked(&[("chat", autostarting(None, &["firefox"]))]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime");
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        let outputs: Mutable<Vec<Workspace>> = Mutable::new(Vec::new());
        let launches: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let driver = autostart_driver(outputs.signal_cloned(), move || saved.clone(), {
            let launches = launches.clone();
            move |entries, _| {
                launches
                    .borrow_mut()
                    .push(entries.into_iter().map(|(name, _)| name).collect());
            }
        });
        let task = tokio::task::spawn_local(driver);

        settle().await;
        assert!(launches.borrow().is_empty(), "no outputs yet");

        outputs.set(vec![ws(1, 1, LEFT, None, true)]);
        settle().await;
        outputs.set(vec![
            ws(1, 1, LEFT, None, true),
            ws(2, 1, RIGHT, None, false),
        ]);
        settle().await;
        outputs.set(vec![ws(1, 1, LEFT, None, true)]);
        settle().await;

        assert_eq!(
            *launches.borrow(),
            [vec!["chat".to_owned()]],
            "one launch for three snapshots"
        );
        task.abort();
    });
}

/// The stacks are started **one after another, in order** — never concurrently.
///
/// Each Start focuses a workspace before it launches anything, so two in flight
/// at once would drop each other's apps on each other's screens. Asserted on
/// the trace: `chat`'s naming and its launches all precede `music`'s.
#[test]
fn autostart_runs_its_stacks_one_at_a_time_in_order() {
    let saved = stacked(&[
        ("chat", autostarting(None, &["firefox"])),
        ("music", autostarting(None, &["spotify"])),
    ]);
    let unnamed = vec![ws(1, 1, LEFT, None, true)];
    let chat_named = vec![ws(1, 1, LEFT, Some("chat"), true)];
    let script = Script::default()
        // Four `workspaces()` reads per stack, in order: the "is it already on
        // screen" probe, housekeeping, the plan, and the verify — and only the
        // verify may show the name that was just set.
        //
        // The world stays consistent with the actions sent, which the scripted
        // niri now enforces: `chat` keeps its name until `music`'s housekeeping
        // releases it. That release is correct here — this test calls
        // `autostart_all` directly, so nothing is in flight and `chat` really
        // is a finished, empty stack by then.
        .with_workspaces(&[
            unnamed.clone(),                           // chat: probe → Inactive
            unnamed.clone(),                           // chat: housekeeping
            unnamed.clone(),                           // chat: plan
            chat_named.clone(),                        // chat: verify
            chat_named.clone(),                        // music: probe → Inactive
            chat_named,                                // music: housekeeping
            unnamed,                                   // music: plan
            vec![ws(1, 1, LEFT, Some("music"), true)], // music: verify
        ])
        .with_windows(&[Vec::new()])
        .with_plain_entries(&["firefox", "spotify"]);

    let entries: Vec<(String, Stack)> = saved
        .names_in_order()
        .into_iter()
        .map(|name| {
            let stack = saved.stacks[&name].clone();
            (name, stack)
        })
        .collect();
    run(autostart_all(&script, &entries, &saved));

    assert_eq!(
        script.launches(),
        [
            "trollshell-ws-chat-0.service",
            "trollshell-ws-music-0.service"
        ],
        "file order, and `music` only after `chat` finished: {:?}",
        script.calls()
    );
    let chat = script
        .position(|c| *c == Call::Launch("trollshell-ws-chat-0.service".to_owned()))
        .expect("chat launched");
    let music_named = script
        .position(|c| {
            matches!(
                c,
                Call::Actions(a)
                    if a.contains(&WorkspaceAction::SetName {
                        workspace: 1,
                        name: "music".to_owned()
                    })
            )
        })
        .expect("music was named");
    assert!(
        chat < music_named,
        "`music`'s transaction begins only after `chat`'s launches: {:?}",
        script.calls()
    );
}

// ── §5: dragging a card to another screen ────────────────────────────────────

/// An **Active** stack's workspace goes with the file change (#1071 §5), and
/// the file is written first.
#[test]
fn moving_an_active_stack_writes_the_file_then_moves_the_workspace() {
    let script = Script::default();
    run(move_to_monitor(&script, "chat", RIGHT, Some(7))).expect("moves");

    assert_eq!(
        script.calls(),
        [
            Call::SetMonitor("chat".to_owned(), RIGHT.to_owned()),
            Call::Actions(vec![WorkspaceAction::MoveWorkspaceToMonitor {
                workspace: 7,
                output: RIGHT.to_owned(),
            }]),
        ],
        "the file first: a write that failed must not leave the workspace \
         somewhere the file disagrees with"
    );
}

/// **The §7 mutation for this row.** An Inactive stack has no live workspace,
/// so the drag is the file and nothing else — sending a move anyway would
/// relocate the lingering empty workspace the next Start is about to reuse,
/// shuffling the indices on two screens to move nothing the user can see.
#[test]
fn moving_an_inactive_stack_only_writes_the_file() {
    let script = Script::default();
    run(move_to_monitor(&script, "chat", RIGHT, None)).expect("moves");

    assert_eq!(
        script.calls(),
        [Call::SetMonitor("chat".to_owned(), RIGHT.to_owned())],
        "no niri action at all: {:?}",
        script.calls()
    );
}

/// A refused write never moves the workspace — same ordering rule as a Save.
#[test]
fn a_failed_monitor_write_never_moves_the_workspace() {
    let script = Script::default();
    script.0.borrow_mut().save_error = Some("read-only file system".to_owned());

    let err = run(move_to_monitor(&script, "chat", RIGHT, Some(7))).expect_err("refuses");
    assert!(err.contains("read-only"), "{err}");
    assert!(
        script.position(|c| matches!(c, Call::Actions(_))).is_none(),
        "nothing was moved: {:?}",
        script.calls()
    );
}

// ── Review fix round (#1106) ─────────────────────────────────────────────────

/// **Review F1.** A shell restart mid-session must not report a failed Start
/// for every stack that is already running.
///
/// `systemctl --user restart trollshell` is the documented dev loop and the
/// latch is per process, so the whole autostart set re-fires against stacks
/// that never stopped. Nothing is double-launched — `plan_start`'s `NameTaken`
/// precondition holds — but each one used to surface as a toast reading
/// *"chat did not start: that name is already on a workspace"*.
#[test]
fn autostart_skips_a_stack_that_is_already_active() {
    let saved = stacked(&[("chat", autostarting(None, &["firefox"]))]);
    let live = vec![ws_focused(1, 1, LEFT, Some("chat"))];
    let script = Script::default()
        .with_workspaces(&[live.clone(), live.clone(), live])
        .with_windows(&[vec![win(9, 1, "firefox")]]);
    let entries = vec![("chat".to_owned(), saved.stacks["chat"].clone())];
    let (captured, _guard) = hytte_config::test_support::capture();

    run(autostart_all(&script, &entries, &saved));

    assert!(
        script.launches().is_empty(),
        "nothing is launched onto a stack that is already on screen: {:?}",
        script.calls()
    );
    let warns: Vec<String> = captured
        .events()
        .into_iter()
        .filter(|e| e.level == tracing::Level::WARN)
        .map(|e| e.message)
        .collect();
    assert!(
        warns.is_empty(),
        "a shell restart must not report a failed Start for a stack that is \
         already running: {warns:?}"
    );
}

/// …and the skip really is a *derivation*, not "the name is taken": a stack
/// whose lingering empty workspace still carries its name, with no windows and
/// no units, is Inactive and **does** autostart.
#[test]
fn autostart_still_starts_a_stack_whose_name_is_merely_lingering() {
    // Unique, for `StartingMark`'s sake — see the Probe A test above.
    let name = "r1auto";
    let saved = stacked(&[(name, autostarting(None, &["firefox"]))]);
    let lingering = vec![ws_focused(1, 1, LEFT, Some(name))];
    let released = vec![ws_focused(1, 1, LEFT, None)];
    let named = vec![ws_focused(1, 1, LEFT, Some(name))];
    let script = Script::default()
        // The probe and the housekeeping both still see the lingering name; the
        // plan sees it gone, which the scripted niri only permits because the
        // housekeeping's `UnsetName` was actually sent.
        .with_workspaces(&[lingering.clone(), lingering, released, named])
        .with_windows(&[Vec::new()])
        .with_plain_entries(&["firefox"]);
    let entries = vec![(name.to_owned(), saved.stacks[name].clone())];

    // `spawn_autostart` marks every queued stack **before** handing off, so the
    // stack under test is in flight for the whole of its own Start (#1106
    // re-review R1, Probe D). Without this the transaction runs in a world no
    // caller produces.
    let _mark = StartingMark::hold(name);
    run(autostart_all(&script, &entries, &saved));

    assert_eq!(
        script.launches(),
        ["trollshell-ws-r1auto-0.service"],
        "an empty named workspace is Inactive, so this one starts: {:?}",
        script.calls()
    );
    assert!(
        script
            .actions()
            .contains(&WorkspaceAction::UnsetName { workspace: 1 }),
        "…and its own lingering name was released first, in spite of its own          in-flight mark: {:?}",
        script.actions()
    );
    assert!(
        script.actions().contains(&WorkspaceAction::SetName {
            workspace: 1,
            name: name.to_owned()
        }),
        "…and then claimed: {:?}",
        script.actions()
    );
}

/// **Review F11.** Stack B's housekeeping must not release the name stack A
/// claimed moments ago.
///
/// A Start's housekeeping sweeps *every* stack in the file, and between A's
/// naming and A's first window A looks exactly like a stale name — so during a
/// sequential autostart run B would `UnsetName` it. The in-flight set is the
/// guard.
#[test]
fn a_start_in_flight_is_never_swept_by_another_stacks_housekeeping() {
    let stacks = BTreeMap::from([
        ("chat".to_owned(), Stack::default()),
        ("music".to_owned(), Stack::default()),
    ]);
    // `chat` is named and still empty — its apps have not opened a window yet.
    let workspaces = [
        ws(1, 1, LEFT, Some("chat"), false),
        ws(2, 2, LEFT, None, true),
    ];

    assert_eq!(
        names_to_release(&stacks, &workspaces, &[], &|_| false, &BTreeSet::new()),
        vec![WorkspaceAction::UnsetName { workspace: 1 }],
        "with nothing in flight it really is a stale name — so the assertion \
         below is not vacuous"
    );
    assert!(
        names_to_release(
            &stacks,
            &workspaces,
            &[],
            &|_| false,
            &BTreeSet::from(["chat".to_owned()])
        )
        .is_empty(),
        "a stack with a Start in flight keeps the name it just claimed"
    );
}

/// …and `release_lingering_names` really reads the live in-flight set, which
/// the pure test above cannot see — **minus the stack it is being run for**.
///
/// The self-exclusion is the whole of #1106 re-review R1: both callers mark the
/// stack before the transaction is scheduled, so a sweep that honoured the raw
/// set would skip the one name it exists to free.
///
/// Names no other test touches, held through a `Drop` guard, because `STARTING`
/// is process-global.
#[test]
fn release_lingering_names_guards_other_stacks_but_never_its_own() {
    let mine = "r1self";
    let other = "r1other";
    let stacks = BTreeMap::from([
        (mine.to_owned(), Stack::default()),
        (other.to_owned(), Stack::default()),
    ]);
    let workspaces = vec![
        ws(1, 1, LEFT, Some(mine), false),
        ws(2, 2, LEFT, Some(other), false),
    ];

    // Nothing in flight: both stale names go.
    let loose = Script::default()
        .with_workspaces(std::slice::from_ref(&workspaces))
        .with_windows(&[Vec::new()]);
    run(release_lingering_names(&loose, mine, &stacks)).expect("sweeps");
    assert_eq!(
        loose.actions(),
        // `names_to_release` walks `stacks.keys()`, and that is a `BTreeMap`:
        // `r1other` sorts before `r1self`.
        [
            WorkspaceAction::UnsetName { workspace: 2 },
            WorkspaceAction::UnsetName { workspace: 1 },
        ],
        "with nothing in flight both stale names are released — so the \
         assertions below are not vacuous"
    );

    // Both marked, the sweep run for `mine`: `mine`'s own name is still freed
    // (it is in flight *because* this is its Start), `other`'s is not.
    let guarded = Script::default()
        .with_workspaces(&[workspaces])
        .with_windows(&[Vec::new()]);
    {
        let _self_mark = StartingMark::hold(mine);
        let _other_mark = StartingMark::hold(other);
        run(release_lingering_names(&guarded, mine, &stacks)).expect("sweeps");
    }

    assert_eq!(
        guarded.actions(),
        [WorkspaceAction::UnsetName { workspace: 1 }],
        "its own name is freed in spite of its own mark; another stack's \
         in-flight name is left alone: {:?}",
        guarded.calls()
    );
}

/// **Review F2.** The layout CLI acts on whatever workspace is focused when it
/// runs, and by the time it runs nothing has asserted focus for ten seconds and
/// a launch — so a focus rides the same batch, immediately before the spawn.
///
/// The second half is the stronger remedy the review's finding asks for: with
/// **no** app of the stack on the workspace there is nothing of ours to
/// arrange, and running the CLI anyway is precisely how a Start that launched
/// nothing reaches out and re-lays-out a workspace the user switched to. So it
/// is not spawned at all rather than spawned after a focus.
#[test]
fn the_layout_is_spawned_right_after_a_focus_and_never_with_nothing_to_arrange() {
    let run_with = |windows: Vec<Window>| {
        let before = vec![ws_focused(1, 1, LEFT, None)];
        let after = vec![ws_focused(1, 1, LEFT, Some("chat"))];
        let script = Script::default()
            .with_workspaces(&[before.clone(), before, after])
            .with_windows(&[Vec::new(), Vec::new(), windows]);
        let mut s = stack(&["firefox"]);
        s.layout = Layout::Split;
        run(start(&script, "chat", &s, &Workspaces::default())).expect("starts");
        script
    };

    let arrived = run_with(vec![win_at(9, 1, "firefox", 1)]);
    let calls = arrived.calls();
    let at = calls
        .iter()
        .position(|c| *c == Call::Layout(Layout::Split))
        .expect("every app arrived, so the layout runs");
    assert!(
        at > 0
            && matches!(&calls[at - 1], Call::Actions(a) if a.last()
                == Some(&WorkspaceAction::Focus { workspace: 1 })),
        "the layout must be spawned immediately after a focus on its own \
         workspace: {:?}",
        calls[at - 1]
    );

    let empty = run_with(Vec::new());
    assert!(
        empty.position(|c| matches!(c, Call::Layout(_))).is_none(),
        "no app of the stack is on the workspace, so there is nothing to lay \
         out and the user's own workspace is left alone: {:?}",
        empty.calls()
    );
}

/// **Review F3.** The peer is picked by **rank**, never by a count indexed into
/// a position-sorted list — the two agree only while niri's order already
/// matches the file's.
#[test]
fn the_saved_order_places_by_rank_not_by_niri_position() {
    let order = ["dev".to_owned(), "chat".to_owned(), "music".to_owned()];
    // niri's order on this screen disagrees with the file: `music` sits before
    // `dev`, so counting "one peer ranks ahead of chat" and taking `peers[0]`
    // lands `chat` after `music` — the peer it ranks *ahead* of.
    let workspaces = [
        ws(1, 1, LEFT, Some("music"), false),
        ws(2, 2, LEFT, Some("dev"), false),
        ws(3, 3, LEFT, None, true),
    ];
    assert_eq!(
        order_index("chat", LEFT, 3, &workspaces, &order),
        Some(3),
        "chat must land after dev, the only peer it ranks behind"
    );
}

/// **Review F4.** A floating window is in no column, so it gets no pair and
/// consumes no index — otherwise every *tiled* app of the stack shifts one
/// place right and the "a gap closes up" invariant is a silent lie for an app
/// that is present and merely floating.
#[test]
fn a_floating_window_consumes_no_column_index() {
    let mut floating = win(9, 1, "pavucontrol");
    floating.layout.pos_in_scrolling_layout = None;
    floating.is_floating = true;
    let windows = [
        floating,
        win_at(10, 1, "firefox", 1),
        win_at(11, 1, "Alacritty", 2),
    ];
    let s = stack(&["pavucontrol", "firefox", "Alacritty"]);
    assert_eq!(
        column_order_batch(&s, 1, &windows),
        vec![
            WorkspaceAction::FocusWindow { window: 10 },
            WorkspaceAction::MoveColumnToIndex { index: 1 },
            WorkspaceAction::FocusWindow { window: 11 },
            WorkspaceAction::MoveColumnToIndex { index: 2 },
        ]
    );
}

/// …and an app whose *only* window floats is simply absent from the batch,
/// while a second, tiled window of the same app is still found.
#[test]
fn a_tiled_window_is_still_found_past_a_floating_one() {
    let mut floating = win(9, 1, "Alacritty");
    floating.layout.pos_in_scrolling_layout = None;
    let windows = [floating, win_at(10, 1, "Alacritty", 1)];
    assert_eq!(
        column_order_batch(&stack(&["Alacritty"]), 1, &windows),
        vec![
            WorkspaceAction::FocusWindow { window: 10 },
            WorkspaceAction::MoveColumnToIndex { index: 1 },
        ]
    );
}
