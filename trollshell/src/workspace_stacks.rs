//! Start and Stop for a workspace stack (#1071 §3.3/§3.4), and the three-state
//! derivation the cards are drawn from.
//!
//! # Why this is not in `panels/workspaces.rs`
//!
//! None of it is GTK. A Start runs on the tokio runtime — it opens niri sockets,
//! calls the user manager and waits out a grace window — and the registry the
//! page's signals live in is thread-local to the GTK main thread, so a
//! transaction cannot read them. It publishes into [`starting`] instead, a
//! process-global `Mutable` the page binds to, which is the ordinary
//! handles-from-work split (`hytte-reactive`'s module doc) with the handle
//! living here rather than in a service, because in-flight Starts are the
//! shell's own transient state and not a daemon's.
//!
//! # The seam
//!
//! Everything the transactions need from the world is [`Ops`], and every
//! *decision* is a pure function over a snapshot ([`plan_start`],
//! [`stop_plan`], [`state_of`], [`stray_moves`]). That split is what lets
//! #1071 §7's Start and Stop mutations be falsified without a compositor: the
//! pure planners answer "what would this do", and [`start`]/[`stop`] over a
//! scripted `Ops` answer "and in what order, and what did it check first".

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
use std::time::Duration;

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::services::niri::{self, Window, Workspace, WorkspaceAction};
use hytte::services::systemd;

use crate::config::workspaces::{Layout, Stack, StackApp};
use crate::launch::{self, Launch};

/// How long after the launches a Start keeps reconciling stray windows
/// (#1071 §3.4 step 2). An app that opens its window later than this keeps it
/// wherever niri put it; the card still shows the stack, because the icons come
/// from the workspace's windows.
///
/// `RUN_COMMAND_TIMEOUT`'s class — long enough for a browser's first window,
/// short enough that a Start does not look wedged.
const GRACE: Duration = Duration::from_secs(10);

/// How often the grace window re-reads niri's window list.
const GRACE_TICK: Duration = Duration::from_millis(500);

/// The layout CLI (#1019/#1026). Resolved off `PATH` the way the user's niri
/// binds already assume; absent is one warning, not an error.
const LAYOUT_BIN: &str = "hytte-plugin-niri-layouts";

/// How long a Stop waits between checks for its slice to go down, and how many
/// times. `StopUnit` enqueues a job; see [`wait_for_slice_down`].
const SLICE_STOP_TICK: Duration = Duration::from_millis(200);
const SLICE_STOP_TICKS: u32 = 25;

/// The ceiling on `Starting`.
///
/// `niri-ipc`'s `Socket` has no read timeout and `spawn_blocking` is not
/// cancellable, so a compositor that accepts a request and never answers parks
/// the task **and the card's button with it**, for the life of the shell. This
/// does not rescue the task — nothing can — but it does hand the button back,
/// so the worst case is a Start the user can retry rather than a card that is
/// dead until the shell restarts. Generous: it has to outlast a real Start,
/// which is [`GRACE`] plus however long the apps take to launch.
const STARTING_CEILING: Duration = Duration::from_secs(90);

/// A card's state (#1071 §3.3).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum StackState {
    /// The named workspace exists **and** the slice has running units or the
    /// workspace has at least one window.
    Active,
    /// Neither. The card is greyed, "not on a screen".
    #[default]
    Inactive,
    /// A Start is in flight. The button is disabled and the card says so.
    Starting,
}

/// The names with a Start in flight.
///
/// A process-global `Mutable` rather than a registry handle: it is written from
/// the runtime and read on the GTK thread, which is exactly the direction the
/// registry cannot go. Same shape as `hytte-reactive`'s own `health::TASKS` and
/// `hytte-services`' `logind::SESSION_LOCKED`.
static STARTING: LazyLock<Mutable<BTreeSet<String>>> =
    LazyLock::new(|| Mutable::new(BTreeSet::new()));

/// Signal of the names with a Start in flight.
pub(crate) fn starting() -> impl Signal<Item = BTreeSet<String>> {
    STARTING.signal_cloned()
}

/// The stacks with at least one unit up, as of the last poll.
static SLICES_UP: LazyLock<Mutable<BTreeSet<String>>> =
    LazyLock::new(|| Mutable::new(BTreeSet::new()));

/// Signal of the stacks with at least one unit up — the *other* source
/// [`state_of`] derives `Active` from.
pub(crate) fn slices_up() -> impl Signal<Item = BTreeSet<String>> {
    SLICES_UP.signal_cloned()
}

/// How often the shell asks systemd which stacks are up.
///
/// systemd has no signal for "a unit inside this slice went away" short of
/// `Subscribe()` plus a `JobRemoved` filter, and the answer only has to be
/// right within a beat — the *window* half of the derivation is event-driven
/// and covers every ordinary case; this is what catches the two it cannot
/// (units lingering after the last window closed, and the first draw after a
/// shell restart). One `ListUnitsByPatterns` per tick, for every stack at once.
const SLICE_POLL: Duration = Duration::from_secs(3);

/// Start the slice poll. Called once, from `main.rs`.
///
/// A `spawn_supervised` task rather than a `Service` because there is no daemon
/// state to hold: the answer is a systemd query, and the handle it publishes to
/// is the same process-global shape [`STARTING`] uses and for the same reason —
/// it is written from the runtime and read on the GTK thread.
///
/// `main.rs` is its only caller, and `main.rs` is not part of the shadow `[lib]`
/// target (#674/#738) — so inside that target this is an orphan `dead_code`
/// cannot tell from a genuine one, the same situation `scale`'s own
/// `#[allow(dead_code)]` in `lib.rs` documents. Scoped to this one item rather
/// than the module, so anything else here that stops being called still reds.
#[allow(dead_code)]
pub(crate) fn spawn_pollers() {
    hytte::reactive::spawn_supervised("workspace-slices", || async {
        loop {
            match systemd::workspace_slices_up().await {
                Ok(up) => SLICES_UP.set_neq(up),
                Err(e) => tracing::debug!(error = %e, "workspace slice poll failed"),
            }
            tokio::time::sleep(SLICE_POLL).await;
        }
    });
}

/// One card's state, from the two sources #1071 §3.3 derives it from.
///
/// Deriving from windows alone misreads three ordinary cases — mid-Start, every
/// window closed by hand while the units linger, and the first poll after a
/// shell restart — and each one offered a Start that would have hit §3.4's
/// naming hazard.
#[must_use]
pub(crate) fn state_of(
    name: &str,
    workspaces: &[Workspace],
    windows: &[Window],
    slice_up: bool,
    starting: &BTreeSet<String>,
) -> StackState {
    if starting.contains(name) {
        return StackState::Starting;
    }
    let Some(workspace) = named(workspaces, name) else {
        return StackState::Inactive;
    };
    let has_windows = windows.iter().any(|w| w.workspace_id == Some(workspace.id));
    if slice_up || has_windows {
        StackState::Active
    } else {
        StackState::Inactive
    }
}

/// The workspace carrying `name`, matched the way niri matches it — case
/// **insensitively** (`find_workspace_by_name`).
///
/// Using `==` here would let a workspace named `Chat` hide from a stack named
/// `chat` while still occupying the name, so the next Start's `SetWorkspaceName`
/// would silently no-op and the card would sit Inactive with no error.
fn named<'w>(workspaces: &'w [Workspace], name: &str) -> Option<&'w Workspace> {
    workspaces.iter().find(|w| {
        w.name
            .as_ref()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

/// The lingering empty workspaces whose names should be released (#1071 §3.4's
/// housekeeping).
///
/// A stack goes Inactive when its windows are gone, but niri does not remove the
/// workspace immediately — `clean_up_workspaces` skips the active and the
/// trailing one — so the name stays taken. And `SetWorkspaceName` **silently
/// does nothing** when the name is taken, so without this the next Start names
/// nothing, launches onto the focused workspace and leaves the card Inactive
/// with no error anywhere. Releasing the name is what makes Start's "the name is
/// free" precondition true rather than hoped for.
///
/// Only workspaces that are *named after a known stack*, empty, and whose slice
/// is down. A workspace the user named by hand is not ours to unname.
#[must_use]
pub(crate) fn names_to_release(
    stacks: &BTreeMap<String, Stack>,
    workspaces: &[Workspace],
    windows: &[Window],
    slice_up: &dyn Fn(&str) -> bool,
) -> Vec<WorkspaceAction> {
    stacks
        .keys()
        .filter_map(|name| {
            let workspace = named(workspaces, name)?;
            let empty = !windows.iter().any(|w| w.workspace_id == Some(workspace.id));
            (empty && !slice_up(name)).then_some(WorkspaceAction::UnsetName {
                workspace: workspace.id,
            })
        })
        .collect()
}

/// Why a Start could not be planned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StartError {
    /// Something already holds the name — including, before housekeeping has
    /// run, this stack's own lingering empty workspace.
    NameTaken,
    /// No output to start on: niri has reported none.
    NoOutput,
    /// The chosen output has no workspace a Start could take.
    NoFreeWorkspace,
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NameTaken => f.write_str("that name is already on a workspace"),
            Self::NoOutput => f.write_str("niri has reported no outputs"),
            Self::NoFreeWorkspace => f.write_str("no free workspace on that screen"),
        }
    }
}

/// What a Start will do, decided from one snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StartPlan {
    /// The workspace the stack will occupy.
    pub workspace: u64,
    /// The output it is on.
    pub output: String,
    /// `true` when the *current* workspace was empty and got adopted rather
    /// than a new one being taken (#1071 §3.4 step 1, Annika's ruling).
    pub adopted: bool,
    /// The one batch, in order.
    pub batch: Vec<WorkspaceAction>,
}

/// Plan a Start (#1071 §3.4 step 1).
///
/// Annika, 2026-09-10: *"if you start an entire stopped stack a new workspace is
/// in order. If nothing is on the current workspace, adopt current workspace"*.
/// So:
///
/// 1. Pick the output — the stack's, if it is connected; otherwise the focused
///    one. (The card says which, so a stack whose screen is unplugged does not
///    start somewhere surprising in silence.)
/// 2. If the **focused** workspace is on that output and has no windows, adopt
///    it. That is also the ephemeral-just-saved case, where the card *is* the
///    current workspace.
/// 3. Otherwise take that output's empty trailing workspace — niri keeps one per
///    output, so a stopped stack gets a genuinely new one.
///
/// The name must be free first: `SetWorkspaceName` silently does nothing when it
/// is taken, so a Start that skipped this check would launch onto whatever was
/// focused and report success.
///
/// # Errors
/// [`StartError`], every variant of which is shown on the card rather than
/// falling through to a launch.
pub(crate) fn plan_start(
    name: &str,
    stack: &Stack,
    workspaces: &[Workspace],
    windows: &[Window],
) -> Result<StartPlan, StartError> {
    if named(workspaces, name).is_some() {
        return Err(StartError::NameTaken);
    }
    let focused = workspaces.iter().find(|w| w.is_focused);
    let connected: BTreeSet<&str> = workspaces
        .iter()
        .filter_map(|w| w.output.as_deref())
        .collect();
    let output = stack
        .monitor
        .as_deref()
        .filter(|m| connected.contains(m))
        .or_else(|| focused.and_then(|w| w.output.as_deref()))
        .or_else(|| connected.iter().next().copied())
        .ok_or(StartError::NoOutput)?
        .to_owned();

    let is_empty = |w: &Workspace| !windows.iter().any(|win| win.workspace_id == Some(w.id));

    // Step 2 — adopt, but ONLY when it really is empty. Adopting a workspace
    // with windows on it would drop the stack on top of whatever the user was
    // doing, which is the #1071 §7 mutation for this line.
    let adoptable = focused.filter(|w| w.output.as_deref() == Some(&output) && is_empty(w));
    // Step 3 — with nothing to adopt, the trailing empty workspace on that
    // output. Highest `idx`, because that is where niri keeps the spare one.
    let (workspace, adopted) = if let Some(current) = adoptable {
        (current.id, true)
    } else {
        let target = workspaces
            .iter()
            .filter(|w| w.output.as_deref() == Some(&output) && w.name.is_none() && is_empty(w))
            .max_by_key(|w| w.idx)
            .ok_or(StartError::NoFreeWorkspace)?;
        (target.id, false)
    };

    Ok(StartPlan {
        workspace,
        output,
        adopted,
        // One batch, in order: name it, then focus it — new windows open on the
        // focused workspace, so the focus has to land before anything launches.
        // `MoveWorkspaceToIndex` for the saved card order is phase 3 (#1071 §6).
        batch: vec![
            WorkspaceAction::SetName {
                workspace,
                name: name.to_owned(),
            },
            WorkspaceAction::Focus { workspace },
        ],
    })
}

/// The `Launch` for one app of `name`'s stack.
///
/// Every app goes in the stack's own slice, so Stop is one `StopUnit` on the
/// slice rather than a walk (#1071 §3.3). No `Restart=`: an app the user closed
/// has finished, it is not a supervised service.
#[must_use]
pub(crate) fn app_launch(name: &str, index: usize, app: &StackApp) -> Launch {
    Launch {
        unit: systemd::workspace_unit_name(name, index),
        description: format!("trollshell workspace {name}: {}", app.id),
        slice: Some(systemd::workspace_slice_name(name)),
        properties: Vec::new(),
        // The display/IPC variables a launched program needs, forwarded from
        // this shell the way a detached `RunCommand` already forwards them
        // (#953 L5): the user manager's `import-environment` does not carry
        // `NIRI_SOCKET` or `DISPLAY`.
        env: forwarded_env(),
        // A stack app is the user's own program; #392's keyring injection is a
        // property of a *plugin* unit.
        secret_env: Vec::new(),
        argv: exec_argv(app),
    }
}

/// The argv for one app.
///
/// Phase 2 launches the `exec` override verbatim (shell-word split) or, with no
/// override, the desktop-entry **id** as a command. Resolving the entry's own
/// `Exec` and stripping its field codes (`%u`, `%F`, …) is phase 4 (#1071 §6),
/// and until then an entry whose id is not also a command is exactly the case
/// the Edit form's per-app launch command exists for.
fn exec_argv(app: &StackApp) -> Vec<String> {
    app.exec.as_deref().map_or_else(
        || vec![app.id.clone()],
        |exec| exec.split_whitespace().map(str::to_owned).collect(),
    )
}

/// Display/IPC variables to forward, and their values, for the ones this shell
/// actually has.
fn forwarded_env() -> Vec<(String, String)> {
    [
        "WAYLAND_DISPLAY",
        "NIRI_SOCKET",
        "DISPLAY",
        "XDG_RUNTIME_DIR",
    ]
    .into_iter()
    .filter_map(|name| {
        let value = std::env::var(name).ok()?;
        (!value.is_empty()).then(|| (name.to_owned(), value))
    })
    .collect()
}

/// What a Start knows about its own launch, so the grace window can tell its
/// windows from the user's (#1071 §3.4 step 2).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Launched {
    /// Every window id that already existed when the Start began.
    ///
    /// The load-bearing half. `app_id` alone identifies an *application*, not a
    /// launch: a stack listing `org.mozilla.firefox` would otherwise reach out
    /// and take the Firefox window the user already had open on another
    /// workspace — and `MoveWindowToWorkspace` carries focus with it, so the
    /// user gets dragged along too.
    pub before: BTreeSet<u64>,
    /// The units this Start asked systemd to start.
    pub units: BTreeSet<String>,
}

/// Which off-workspace windows are worth asking systemd about.
///
/// The cheap filters, so a grace-window tick resolves a unit for a handful of
/// windows rather than for every window in the session.
fn stray_candidates<'w>(
    workspace: u64,
    windows: &'w [Window],
    launched: &Launched,
) -> Vec<&'w Window> {
    windows
        .iter()
        .filter(|w| w.workspace_id != Some(workspace))
        .filter(|w| !launched.before.contains(&w.id))
        .collect()
}

/// The windows **this Start opened** that landed elsewhere, as the batch that
/// brings them home (#1071 §3.4 step 2).
///
/// Three rules, in order, and the first two are what keep a Start from moving
/// windows that are not its own:
///
/// 1. A window that **already existed** when the Start began is never moved. It
///    is the user's, whatever it happens to be running.
/// 2. A window whose pid belongs to **one of this Start's units** is moved. This
///    is §3.4's pid leg, and it is exact rather than heuristic: the unit name
///    came from [`app_launch`], so there is nothing to guess.
/// 3. Otherwise a *new* window whose `app_id` is in the stack is moved — the
///    fallback for a window that has appeared but whose pid systemd cannot place
///    yet (a unit still activating, a pid niri has not reported). Scoped to
///    windows that appeared **after** the Start began, so its worst case is a
///    window the user opened of the same app inside the same ten seconds rather
///    than every such window they have ever had open.
///
/// A window already on the target workspace produces no action, so a settled
/// Start sends an empty batch and `send_actions` opens no socket.
#[must_use]
pub(crate) fn stray_moves(
    stack: &Stack,
    workspace: u64,
    windows: &[Window],
    launched: &Launched,
    unit_of: &BTreeMap<u64, Option<String>>,
) -> Vec<WorkspaceAction> {
    let wanted: BTreeSet<&str> = stack.apps.iter().map(|a| a.id.as_str()).collect();
    stray_candidates(workspace, windows, launched)
        .into_iter()
        .filter(|w| {
            // Rule 2.
            let ours = unit_of
                .get(&w.id)
                .and_then(Option::as_ref)
                .is_some_and(|unit| launched.units.contains(unit));
            // Rule 3.
            ours || w.app_id.as_deref().is_some_and(|id| wanted.contains(id))
        })
        .map(|w| WorkspaceAction::MoveWindow {
            window: w.id,
            workspace,
        })
        .collect()
}

/// One step of a Stop (#1071 §3.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StopStep {
    /// The window's pid belongs to a unit this Stop is **allowed** to stop —
    /// see [`may_stop`].
    StopUnit(String),
    /// No unit at all (a program started from a nested shell inside a
    /// terminal). niri closes it.
    ///
    /// **By id.** `CloseWindow { id: None }` closes the *focused* window, which
    /// during a Stop is very likely not this one — #1071 §7's named mutation.
    Close(u64),
    /// The window's pid resolves to a unit this Stop **must not** stop. The
    /// window is closed through niri instead, and the refused unit is named so
    /// the journal says why.
    CloseInstead { window: u64, refused: String },
}

/// Whether a Stop of the stack `name` may stop the unit `unit`.
///
/// **This is a guard, not a filter.** The epic assumed the unit behind a window
/// on a workspace is always an `app-niri-*.scope`. It is not, and two of the
/// ways it is not are catastrophic:
///
/// * The shell opens links with `gio::AppInfo::launch_default_for_uri`, and glib
///   2.88 creates **no** transient scope for that — the browser it starts is
///   forked into `trollshell.service`'s own cgroup. Without this guard, ⏹ on a
///   workspace holding that window is `StopUnit("trollshell.service")`: the
///   shell stops itself.
/// * niri's own `StartTransientUnit` has a fallback path, and anything that
///   missed its scope resolves to `niri.service` — ⏹ then takes the compositor
///   down, and the session with it.
///
/// So the rule is an **allowlist**, not a denylist: only two shapes pass.
///
/// * `app-*.scope` — the freedesktop convention for a user application started
///   by a launcher, which is what `app-niri-*.scope` is an instance of. These
///   really are "a program on this workspace".
/// * `trollshell-ws-<escaped>-<n>.service` — **this stack's own** units, and no
///   other stack's. The slice stop above should already have taken them; this
///   is the residue of one that outlived its slice job.
///
/// Everything else — the shell's unit, the compositor's, a plugin's, another
/// stack's, a detached `RunCommand`, anything in the user's session slice — is
/// refused, and the window is closed through niri instead. Pure, so the
/// allowlist is unit-testable without a bus.
#[must_use]
pub(crate) fn may_stop(name: &str, unit: &str) -> bool {
    // `strip_suffix` rather than `ends_with`, which clippy reads as a
    // case-insensitive file-extension comparison — systemd unit suffixes are
    // case-sensitive and `.scope` is not a file extension.
    if unit.starts_with("app-") && unit.strip_suffix(".scope").is_some() {
        return true;
    }
    // `parse_workspace_unit` reverses the `\x2d` escape, so this compares the
    // *names* rather than the spellings — and it answers `None` for anything
    // that is not a stack unit at all, including `trollshell-plugin-*` and
    // `trollshell-launch-*`.
    systemd::parse_workspace_unit(unit).is_some_and(|owner| owner == name)
}

/// What remains to be stopped after the slice is down, given each window's unit
/// (`None` = systemd knows no unit for its pid).
///
/// The slice stop comes first and is not in this list: it is one call that takes
/// every app this shell launched. It is a *job enqueue* rather than a
/// synchronous stop, though, so [`stop`] waits for the slice to actually go down
/// before walking — see [`wait_for_slice_down`].
#[must_use]
pub(crate) fn stop_plan(
    name: &str,
    windows: &[&Window],
    units: &BTreeMap<u64, Option<String>>,
) -> Vec<StopStep> {
    windows
        .iter()
        .map(|w| match units.get(&w.id).and_then(Option::as_ref) {
            Some(unit) if may_stop(name, unit) => StopStep::StopUnit(unit.clone()),
            Some(unit) => StopStep::CloseInstead {
                window: w.id,
                refused: unit.clone(),
            },
            None => StopStep::Close(w.id),
        })
        .collect()
}

// ── The world ────────────────────────────────────────────────────────────────

/// Everything a transaction needs from outside this process.
///
/// One trait rather than free calls so #1071 §7's Start and Stop rows can be
/// falsified against a scripted world: the order of the calls, and what was
/// checked before what, is the substance of both transactions and neither is
/// observable from a pure planner alone.
pub(crate) trait Ops {
    /// One batch, one socket, in order (`niri::send_actions`).
    fn send_actions(
        &self,
        actions: Vec<WorkspaceAction>,
    ) -> impl Future<Output = Result<(), String>>;
    fn workspaces(&self) -> impl Future<Output = Result<Vec<Workspace>, String>>;
    fn windows(&self) -> impl Future<Output = Result<Vec<Window>, String>>;
    /// Run one `systemd-run --user` invocation to completion.
    fn launch(&self, launch: &Launch) -> impl Future<Output = Result<(), String>>;
    /// Persist a stack to `workspaces.toml`.
    ///
    /// On the seam rather than called through directly, and that is not a
    /// stylistic choice: [`crate::config::workspaces::save_stack`] resolves its
    /// own path through `xdg::overlay_path`, so a transaction test that reached
    /// it would write the **developer's real** `~/.config/trollshell/`. It did,
    /// once, before this moved (#1101 re-review). Behind `Ops` the write is part
    /// of the world like every other side effect here, and a test cannot perform
    /// it by construction rather than by remembering not to.
    fn save_stack(&self, name: &str, stack: &Stack) -> impl Future<Output = Result<(), String>>;
    fn unit_for_pid(&self, pid: u32) -> impl Future<Output = Option<String>>;
    fn stop_unit(&self, unit: &str) -> impl Future<Output = Result<(), String>>;
    fn stop_slice(&self, name: &str) -> impl Future<Output = Result<(), String>>;
    fn slice_is_up(&self, name: &str) -> impl Future<Output = bool>;
    /// Wait. Injected so the grace window costs a test nothing.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()>;
    /// Spawn the layout CLI for `layout` on the focused workspace.
    fn apply_layout(&self, layout: Layout) -> impl Future<Output = Result<(), String>>;
}

/// The real world.
pub(crate) struct Live;

impl Ops for Live {
    async fn send_actions(&self, actions: Vec<WorkspaceAction>) -> Result<(), String> {
        niri::send_actions(actions).await
    }

    async fn workspaces(&self) -> Result<Vec<Workspace>, String> {
        niri::query_workspaces().await
    }

    async fn windows(&self) -> Result<Vec<Window>, String> {
        niri::query_windows().await
    }

    async fn launch(&self, launch: &Launch) -> Result<(), String> {
        let output = launch::command(launch::SYSTEMD_RUN, launch)
            .output()
            .await
            .map_err(|e| format!("spawning systemd-run --user: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        Err(format!(
            "systemd-run --user failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }

    async fn save_stack(&self, name: &str, stack: &Stack) -> Result<(), String> {
        crate::config::workspaces::save_stack(name, stack).map_err(|e| e.to_string())
    }

    async fn unit_for_pid(&self, pid: u32) -> Option<String> {
        systemd::unit_for_pid(pid).await.ok().flatten()
    }

    async fn stop_unit(&self, unit: &str) -> Result<(), String> {
        systemd::stop_unit(unit).await.map_err(|e| e.to_string())
    }

    async fn stop_slice(&self, name: &str) -> Result<(), String> {
        systemd::stop_workspace_slice(name)
            .await
            .map_err(|e| e.to_string())
    }

    async fn slice_is_up(&self, name: &str) -> bool {
        systemd::workspace_slice_is_up(name).await.unwrap_or(false)
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn apply_layout(&self, layout: Layout) -> Result<(), String> {
        if layout == Layout::None {
            return Ok(());
        }
        let status = tokio::process::Command::new(LAYOUT_BIN)
            .arg("apply")
            .arg(layout.name())
            .status()
            .await
            .map_err(|e| format!("{LAYOUT_BIN} could not be run: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{LAYOUT_BIN} apply {} ({status})", layout.name()))
        }
    }
}

// ── The transactions ─────────────────────────────────────────────────────────

/// Release the lingering names of stacks that have gone Inactive
/// (#1071 §3.4 housekeeping).
///
/// Run before a Start, so the name a Start is about to claim is actually free.
///
/// # Errors
/// Whatever niri said.
pub(crate) async fn release_lingering_names(
    ops: &impl Ops,
    stacks: &BTreeMap<String, Stack>,
) -> Result<(), String> {
    let workspaces = ops.workspaces().await?;
    let windows = ops.windows().await?;
    let mut up = BTreeSet::new();
    for name in stacks.keys() {
        if ops.slice_is_up(name).await {
            up.insert(name.clone());
        }
    }
    let actions = names_to_release(stacks, &workspaces, &windows, &|name| up.contains(name));
    ops.send_actions(actions).await
}

/// Start one stack, as #1071 §3.4's transaction.
///
/// In order, and every step is a precondition for the next:
///
/// 1. **Housekeeping.** Release any lingering empty workspace's name, so
///    `SetWorkspaceName` below cannot silently no-op.
/// 2. **Plan** against a fresh snapshot: a new workspace, or the current one if
///    it is empty.
/// 3. **One batch**: name it, focus it.
/// 4. **Verify** by reading the workspace list back. `SetWorkspaceName` reports
///    success either way, so this is the only thing that can tell a landed name
///    from a swallowed one — and it happens *before* any app is launched, so a
///    failed naming never drops apps on someone else's workspace.
/// 5. **Launch** every app into the stack's slice.
/// 6. **Reconcile** for [`GRACE`]: a window whose `app_id` belongs to the stack
///    but landed elsewhere is moved home.
/// 7. **Layout**, once, after all of it.
///
/// # Errors
/// The first step that failed, as a line for the card.
pub(crate) async fn start(
    ops: &impl Ops,
    name: &str,
    stack: &Stack,
    stacks: &BTreeMap<String, Stack>,
) -> Result<StartPlan, String> {
    release_lingering_names(ops, stacks).await?;

    let workspaces = ops.workspaces().await?;
    let windows = ops.windows().await?;
    let plan = plan_start(name, stack, &workspaces, &windows).map_err(|e| e.to_string())?;

    ops.send_actions(plan.batch.clone()).await?;

    // Step 4. Never skip: a silently-swallowed name looks exactly like a
    // successful one from the action's own reply.
    let after = ops.workspaces().await?;
    if !after
        .iter()
        .any(|w| w.id == plan.workspace && w.name.as_deref() == Some(name))
    {
        return Err(format!(
            "niri did not take the name {name:?} — nothing was launched"
        ));
    }

    // The pre-launch snapshot is what tells this Start's windows from the
    // user's in step 6 (#1071 §3.4 step 2). Taken from the *same* `windows`
    // read the plan was made from, so nothing that opened between the two can
    // be mistaken for pre-existing.
    let mut launched = Launched {
        before: windows.iter().map(|w| w.id).collect(),
        units: BTreeSet::new(),
    };
    for (index, app) in stack.apps.iter().enumerate() {
        let unit = app_launch(name, index, app);
        let unit_name = unit.unit.clone();
        if let Err(e) = ops.launch(&unit).await {
            tracing::warn!(workspace = name, app = app.id, error = %e, "stack app failed to launch");
            continue;
        }
        launched.units.insert(unit_name);
    }

    reconcile(ops, stack, plan.workspace, &launched).await;

    if let Err(e) = ops.apply_layout(stack.layout).await {
        tracing::warn!(workspace = name, error = %e, "layout not applied");
    }

    Ok(plan)
}

/// The grace window: move home the windows **this Start opened** that landed
/// elsewhere, until every app has one or [`GRACE`] is up (#1071 §3.4 step 2).
///
/// `launched` is what makes "this Start's" mean anything — see [`stray_moves`].
/// A unit is resolved only for the windows the cheap filters leave standing, so
/// a tick costs a D-Bus round trip per genuinely-new off-workspace window rather
/// than one per window in the session.
async fn reconcile(ops: &impl Ops, stack: &Stack, workspace: u64, launched: &Launched) {
    if stack.apps.is_empty() {
        return;
    }
    let ticks = (GRACE.as_millis() / GRACE_TICK.as_millis()).max(1);
    for _ in 0..ticks {
        ops.sleep(GRACE_TICK).await;
        let Ok(windows) = ops.windows().await else {
            continue;
        };
        let mut unit_of = BTreeMap::new();
        for window in stray_candidates(workspace, &windows, launched) {
            let unit = match window.pid {
                Some(pid) if pid >= 0 => ops.unit_for_pid(u32::try_from(pid).unwrap_or(0)).await,
                _ => None,
            };
            unit_of.insert(window.id, unit);
        }
        let moves = stray_moves(stack, workspace, &windows, launched, &unit_of);
        if !moves.is_empty() {
            let _ = ops.send_actions(moves).await;
        }
        // Settled: every app has a window here.
        let here: BTreeSet<&str> = windows
            .iter()
            .filter(|w| w.workspace_id == Some(workspace))
            .filter_map(|w| w.app_id.as_deref())
            .collect();
        if stack.apps.iter().all(|a| here.contains(a.id.as_str())) {
            return;
        }
    }
}

/// Wait for `name`'s slice to actually be down, bounded.
///
/// `StopUnit` is a **job enqueue**, not a synchronous stop. Measured on systemd
/// 260.2: the call returns a job path in ~15 ms and the unit is still
/// `deactivating` at that point. So "the slice is down before the walk" — which
/// is the reasoning the whole per-window pass rests on, and the reason
/// [`may_stop`]'s allowlist only has to consider the *compositor's* units — is
/// not something `stop_slice` gives you on its own.
///
/// Polling [`Ops::slice_is_up`] rather than subscribing to `JobRemoved`: the
/// subscription would need `Manager.Subscribe()` plus a filter for one job path,
/// and this is a handful of ticks on a path the user is already waiting on.
/// Bounded, because a unit that ignores SIGTERM would otherwise park the Stop
/// until systemd's own `TimeoutStopSec` escalation — the walk below is correct
/// either way, it just re-handles a window or two.
async fn wait_for_slice_down(ops: &impl Ops, name: &str) {
    for _ in 0..SLICE_STOP_TICKS {
        if !ops.slice_is_up(name).await {
            return;
        }
        ops.sleep(SLICE_STOP_TICK).await;
    }
    tracing::warn!(
        workspace = name,
        "the stack's slice was still up after {:?}; stopping its windows anyway",
        SLICE_STOP_TICK * SLICE_STOP_TICKS
    );
}

/// Stop one stack (#1071 §3.3).
///
/// The slice first — one call that takes every app this shell launched, SIGTERM
/// then systemd's own escalation, and idempotent (stopping a never-created slice
/// exits 0, measured) — and then a **wait** for it to really be down, because
/// `StopUnit` only enqueues a job ([`wait_for_slice_down`]). Then, for whatever
/// windows are still on the workspace, per window: stop its unit if systemd
/// names one **and this Stop is allowed to stop it** ([`may_stop`]), else close
/// it through niri **by id**.
///
/// Finally the name is released, so the next Start's `SetWorkspaceName` has a
/// free name rather than this workspace's lingering one.
///
/// # Errors
/// Whatever the slice stop or the final batch said. A single window that would
/// not stop is logged, not fatal — the rest still go.
pub(crate) async fn stop(ops: &impl Ops, name: &str) -> Result<(), String> {
    ops.stop_slice(name).await?;
    wait_for_slice_down(ops, name).await;

    let workspaces = ops.workspaces().await?;
    let Some(workspace) = named(&workspaces, name).map(|w| w.id) else {
        return Ok(());
    };
    let windows = ops.windows().await?;
    // Scoped to *this* workspace. The single most dangerous line here: without
    // it the walk below stops or closes every window in the session, which
    // `a_stop_leaves_windows_on_other_workspaces_alone` is the test for.
    let remaining: Vec<&Window> = windows
        .iter()
        .filter(|w| w.workspace_id == Some(workspace))
        .collect();

    let mut units = BTreeMap::new();
    for window in &remaining {
        let unit = match window.pid {
            Some(pid) if pid >= 0 => ops.unit_for_pid(u32::try_from(pid).unwrap_or(0)).await,
            _ => None,
        };
        units.insert(window.id, unit);
    }

    let mut closes = Vec::new();
    for step in stop_plan(name, &remaining, &units) {
        match step {
            StopStep::StopUnit(unit) => {
                if let Err(e) = ops.stop_unit(&unit).await {
                    tracing::warn!(workspace = name, %unit, error = %e, "unit would not stop");
                }
            }
            StopStep::Close(id) => closes.push(WorkspaceAction::CloseWindow { window: id }),
            StopStep::CloseInstead { window, refused } => {
                // Loud on purpose: this is the shell declining to stop
                // something it could have stopped, and the reason is worth
                // having in the journal the first time someone wonders why a
                // window closed instead of its program exiting.
                tracing::warn!(
                    workspace = name,
                    unit = %refused,
                    "not a unit this workspace may stop; closing the window instead"
                );
                closes.push(WorkspaceAction::CloseWindow { window });
            }
        }
    }
    // The close batch and the name release ride one socket, in that order: the
    // name must not be freed before the windows it identifies are dealt with.
    closes.push(WorkspaceAction::UnsetName { workspace });
    ops.send_actions(closes).await
}

/// Run a Start on the runtime, holding the card in [`StackState::Starting`] for
/// its whole life.
///
/// The `Mutable` is cleared on every exit path — including the error one —
/// because a card stuck on `Starting` has a permanently disabled button.
pub(crate) fn spawn_start(name: String, stack: Stack, stacks: BTreeMap<String, Stack>) {
    if !STARTING.lock_mut().insert(name.clone()) {
        // Already in flight; a second click is not a second Start.
        return;
    }
    hytte::reactive::runtime::handle().spawn(async move {
        // The ceiling is on the *card*, not on the transaction: a `spawn_blocking`
        // waiting on a niri that will never answer cannot be cancelled, so this
        // releases the button and lets the task finish whenever it does. See
        // `STARTING_CEILING`.
        let outcome =
            tokio::time::timeout(STARTING_CEILING, start(&Live, &name, &stack, &stacks)).await;
        // Released before the reporting, so no `?`-shaped edit can ever leave a
        // card disabled for the life of the shell.
        STARTING.lock_mut().remove(&name);
        match outcome {
            Ok(Ok(plan)) => tracing::info!(
                workspace = name,
                output = plan.output,
                adopted = plan.adopted,
                "workspace stack started"
            ),
            Ok(Err(e)) => {
                report(&name, &format!("{name} did not start: {e}"));
            }
            Err(_) => report(
                &name,
                &format!(
                    "{name} is still starting after {}s — the compositor did not answer",
                    STARTING_CEILING.as_secs()
                ),
            ),
        }
    });
}

/// Persist an ephemeral workspace as the stack `name`, and **name the niri
/// workspace in the same breath** (#1071 §3.7).
///
/// §3.7: *"The batch names the niri workspace immediately, so the saved
/// workspace **is** the Active card."* Writing the file alone does not do that —
/// the workspace stays unnamed, so it keeps rendering as an ephemeral card while
/// the new stack renders as a second, Inactive one whose ▶ would launch a second
/// copy of everything. The naming is what collapses the two into one Active
/// card.
///
/// Same precondition as a Start, for the same reason: `SetWorkspaceName`
/// silently does nothing when the name is taken, so the name is verified free
/// **before** the file is written. Getting that order wrong would leave a stack
/// in the file that can never be this workspace.
///
/// # Errors
/// A name niri already holds, a file the writer refused, or a naming that did
/// not land — each as a line for the user.
pub(crate) async fn save(
    ops: &impl Ops,
    workspace: u64,
    name: &str,
    stack: &Stack,
) -> Result<(), String> {
    let workspaces = ops.workspaces().await?;
    if named(&workspaces, name).is_some() {
        return Err(StartError::NameTaken.to_string());
    }
    ops.save_stack(name, stack).await?;

    ops.send_actions(vec![WorkspaceAction::SetName {
        workspace,
        name: name.to_owned(),
    }])
    .await?;

    // Same read-back as a Start's step 4, and for the same reason: the action's
    // reply says nothing about whether the name landed.
    let after = ops.workspaces().await?;
    if after
        .iter()
        .any(|w| w.id == workspace && w.name.as_deref() == Some(name))
    {
        Ok(())
    } else {
        Err(format!(
            "saved, but niri did not take the name {name:?} — the card will show as stopped"
        ))
    }
}

/// Run a Save on the runtime.
pub(crate) fn spawn_save(workspace: u64, name: String, stack: Stack) {
    hytte::reactive::runtime::handle().spawn(async move {
        match save(&Live, workspace, &name, &stack).await {
            Ok(()) => tracing::info!(workspace = name, apps = stack.apps.len(), "workspace saved"),
            Err(e) => report(&name, &format!("{name} was not saved: {e}")),
        }
    });
}

/// Run a Stop on the runtime.
pub(crate) fn spawn_stop(name: String) {
    hytte::reactive::runtime::handle().spawn(async move {
        if let Err(e) = stop(&Live, &name).await {
            report(&name, &format!("{name} did not stop: {e}"));
        }
    });
}

/// Surface a failed Start or Stop to the **user**, not only to the journal.
///
/// The transactions build careful messages — `send_actions` names the action
/// niri refused, `plan_start` says which precondition failed — and before this
/// every one of them ended in the journal, where a user who pressed a button and
/// saw nothing happen will not look. `post_local` is the shell's own existing
/// surface for "something you asked for did not work"; it rate-limits identical
/// toasts and is a no-op when the notifications service is not registered, which
/// is why the `tracing::warn!` stays as the durable record.
pub(crate) fn report(name: &str, message: &str) {
    tracing::warn!(workspace = name, "{message}");
    hytte::services::notifications::post_local(
        "Workspaces",
        "Workspaces",
        message,
        hytte::services::notifications::Urgency::Normal,
    );
}

#[cfg(test)]
mod tests;
