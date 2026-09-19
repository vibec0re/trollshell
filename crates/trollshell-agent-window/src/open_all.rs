//! `--open-all`: one companion window per **running** agent, tiled on a fresh
//! niri workspace ([#1306](https://github.com/vibec0re/trollshell/issues/1306)).
//!
//! # Why this lives in the window binary
//!
//! @kaesaecracker asked for "a dynamic workspace where all agents are tiled
//! on", and then for it to be *the* system behind the shell's Workspaces page
//! — an unnamed workspace that becomes an ephemeral card the moment it has
//! windows on it (#1071 §3.7). Picking such a workspace needs niri's own
//! workspace list, and the agents plugin cannot ask for it: a plugin's only
//! lever on the world outside its socket is a detached `RunCommand`. A first
//! attempt to work around that with a *named* workspace plus a window rule was
//! the wrong layer and came back out of #1307.
//!
//! This binary, on the other hand, is a normal process with `$NIRI_SOCKET` in
//! its environment and hyperhive's `host.sock` already wired up for its own
//! header. So the deciding moved here (Annika on #1306, 2026-09-14, proposal
//! 1), and the card's button is one launch of this mode.
//!
//! # What one run does
//!
//! 1. Ask the hive for the roster and keep the agents whose collapsed
//!    [`Status`] [`wants_terminal`](Status::wants_terminal) — the *running*
//!    ones, which is the membership Annika settled on 2026-09-19 out of the
//!    issue's own title. Nothing running is **not an error**: one logged
//!    line and exit 0, because "nothing to open" is an answer.
//! 2. Ask niri for its workspaces and windows **once**. The window list
//!    answers two questions: which workspaces are empty ([`pick_workspace`]),
//!    and which of those agents already has a companion window
//!    ([`without_a_window`], matching `app_id` against [`crate::cli::app_id`]).
//! 3. If some agent still needs a window, pick the **empty unnamed workspace
//!    at the bottom of the focused output** and focus it, *then* launch. The
//!    order is the mechanism: niri opens a new window on the focused
//!    workspace, so a focus behind the launches would race them.
//! 4. Launch `trollshell-agent-window --agent <name>` for each agent that
//!    needs one, detached, in the hive's own roster order — which becomes the
//!    column order niri tiles them in.
//!
//! # The workspace is ephemeral, and that is the whole design
//!
//! Nothing here names the workspace, writes a window rule or touches
//! `workspaces.toml`. The windows land on a workspace that already existed
//! (niri keeps a spare at the bottom of every output), it becomes an
//! **ephemeral card** on the Workspaces page for as long as it holds windows,
//! and it is gone when the last one closes. A *saved* stack whose membership is
//! recomputed from the hive at every Start would be a new stack kind and a
//! schema change — Annika's call on #1071 if it is ever wanted, not something
//! this mode should grow towards.
//!
//! # A second run presents what is open, and picks no workspace
//!
//! Each window is its own `GApplication` id ([`crate::cli::app_id`], one per
//! agent), so a launch for an agent that already has a window finds the
//! running process over the session bus, hands it the command line
//! (`HANDLES_COMMAND_LINE`) and exits — `main`'s `connect_command_line` then
//! `present()`s the window that exists. That is what "a second press does not
//! double" rests on, and it is also why this mode keeps no bookkeeping of its
//! own: the application id *is* the bookkeeping, and [`without_a_window`] reads
//! it back off niri's window list.
//!
//! A second press therefore **picks no workspace and sends no focus**: there
//! is nothing to place, so every launch is a present, and each window is
//! presented **where it is** — this mode never *gathers*. (Focusing the spare
//! on a repeat press would mean a detour through an empty workspace at best,
//! and being stranded on one at worst; [`open_all_with`] argues both.) A
//! *mixed* press — some windows open, some not — places only the missing ones
//! and leaves the open ones alone, because presenting one mid-run would move
//! the focus out from under the next launch.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hytte_plugin_agents::hive::client::{self, HiveError};
use hytte_plugin_agents::hive::wire::{AgentStatusRow, Request as HiveRequest, Response};
use hytte_plugin_agents::model::{AgentName, Status};
use hytte_plugin_agents::window::{self as plugin_window, Tab};
use niri_ipc::{
    Action, Reply, Request as NiriRequest, Response as NiriResponse, Window, Workspace,
    WorkspaceReferenceArg,
};

/// Exit code when every window this run wanted was launched (including "there
/// was nothing to launch").
pub const EXIT_OK: u8 = 0;

/// Exit code when the hive could not be read, or no launch succeeded.
pub const EXIT_FAILED: u8 = 1;

/// The transient unit each launched window gets, prefixed so that the shell's
/// own `systemctl --user list-units 'trollshell-launch-*'` — the glob
/// `docs/live-verify.md` already tells an operator to run — finds the windows
/// this mode started as well as the ones a pill click did.
const UNIT_PREFIX: &str = "trollshell-launch-agent-window";

/// The slice every launched window joins:
/// `trollshell/src/plugins/effects.rs`'s `LAUNCH_SLICE`, spelled again here
/// because this binary cannot link the shell.
///
/// Sharing it is the point — `systemctl --user stop trollshell-launch.slice`
/// is the one command that reaches every detached thing trollshell has
/// started on the operator's behalf, and a fan-out that invented its own slice
/// would be the one exception nobody remembers.
const LAUNCH_SLICE: &str = "trollshell-launch.slice";

/// The variables a launch carries explicitly, because `systemd-run --user`
/// gives the unit the **manager's** environment and not this process's.
///
/// Byte-identical to `trollshell/src/plugins/effects.rs`'s `FORWARDED_ENV`,
/// and for its reasons: the session's own
/// `systemctl --user import-environment` covers `WAYLAND_DISPLAY` in a normal
/// deployment but not `NIRI_SOCKET` or `DISPLAY`, and in a nested dev
/// compositor the manager holds the *outer* session's values, so a launch
/// would land on the wrong screen.
const FORWARDED_ENV: [&str; 4] = [
    "WAYLAND_DISPLAY",
    "NIRI_SOCKET",
    "DISPLAY",
    "XDG_RUNTIME_DIR",
];

/// How long `systemd-run` may take to hand the start job to the user manager.
///
/// It returns as soon as the manager accepts the job rather than waiting out
/// the program (that is the whole reason a transient **service** is used and
/// not a scope), so this bounds a wedged manager and nothing else.
const LAUNCH_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-process launch counter, so two windows started by the same run cannot
/// collide on a unit name.
static LAUNCH_SEQ: AtomicU64 = AtomicU64::new(0);

// ── membership ───────────────────────────────────────────────────────────────

/// The agents this run opens a window for, in the hive's own roster order.
///
/// Two rules, neither of them new:
///
/// - **Running only**, through [`Status::wants_terminal`] — the single
///   definition this crate shares with the sidebar card, so the button's count
///   and this list cannot disagree.
/// - **Re-validated names.** A row's `name` is a plain string on the wire and
///   is about to become an `argv` element, so it goes through
///   [`AgentName::parse`] exactly as the plugin's reducer does; a row that
///   fails the whitelist is dropped with a line rather than launched.
#[must_use]
pub fn running_agents(rows: &[AgentStatusRow]) -> Vec<AgentName> {
    rows.iter()
        .filter(|row| Status::of(row).wants_terminal())
        .filter_map(|row| {
            let parsed = AgentName::parse(&row.name);
            if parsed.is_none() {
                tracing::warn!(name = %row.name, "agent name failed the whitelist; not launching a window for it");
            }
            parsed
        })
        .collect()
}

// ── the workspace ────────────────────────────────────────────────────────────

/// The workspace this run puts the windows on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// niri's stable workspace id — what the focus action names.
    pub id: u64,
    /// Its position on its monitor, for the diagnostic line only. Unstable by
    /// niri's own documentation ("this index *will* change as you move and
    /// re-order workspaces"), which is why it is not what gets focused.
    pub idx: u8,
    /// The connector it belongs to, or `None` when niri has no outputs.
    pub output: Option<String>,
}

/// Why no workspace could be picked. Both arms are a **warning**, never a
/// refusal to launch: the windows are the feature and the workspace is what
/// makes them tidy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoWorkspace {
    /// niri reported no focused workspace at all, so "the focused output" has
    /// no referent. Reachable only from an empty or malformed workspace list.
    NoFocus,
    /// The focused output has no empty unnamed workspace. niri keeps a spare
    /// at the bottom of every output, so this means something unusual —
    /// most plausibly a config that names every workspace.
    NoSpare {
        /// The connector that was searched.
        output: String,
    },
}

impl std::fmt::Display for NoWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFocus => f.write_str("niri reports no focused workspace"),
            Self::NoSpare { output } => write!(
                f,
                "no empty unnamed workspace on {output} — the windows will open on the current one"
            ),
        }
    }
}

/// Pick the empty workspace at the bottom of the **focused** output.
///
/// # The rule, and why each clause is there
///
/// - **The focused output.** A fan-out is something the operator just asked
///   for, so it belongs on the screen they are looking at — not on whichever
///   monitor happens to hold the highest-numbered spare.
/// - **Unnamed.** This is what makes the result an *ephemeral card* on the
///   Workspaces page: that page keys its ephemeral cards on
///   `name.is_none()` (`trollshell/src/panels/workspaces.rs`'s
///   `ephemeral_cards`), and a named workspace is "the user's own and is left
///   alone" by its own rule. Taking one would both steal a workspace somebody
///   named for something and produce a card that is not the one #1306 asks
///   for.
/// - **No windows.** Tested against niri's **window list**, not against
///   `Workspace::active_window_id`: the two agree in practice, but the
///   question here is "is this workspace empty" and the window list is what
///   actually answers it. The same test `ephemeral_cards` makes, for the same
///   reason.
/// - **The highest `idx`.** niri's spare sits at the bottom of the output, so
///   among the empty unnamed candidates the last one is the spare. Picking the
///   *first* would drop the windows into a gap between two populated
///   workspaces.
///
/// # Errors
/// [`NoWorkspace`] — which the caller reports and then launches anyway.
pub fn pick_workspace(workspaces: &[Workspace], windows: &[Window]) -> Result<Target, NoWorkspace> {
    let focused = workspaces
        .iter()
        .find(|w| w.is_focused)
        .ok_or(NoWorkspace::NoFocus)?;
    let output = focused.output.clone();
    workspaces
        .iter()
        .filter(|w| w.output == output)
        .filter(|w| w.name.is_none())
        .filter(|w| !windows.iter().any(|win| win.workspace_id == Some(w.id)))
        .max_by_key(|w| w.idx)
        .map(|w| Target {
            id: w.id,
            idx: w.idx,
            output: w.output.clone(),
        })
        .ok_or_else(|| NoWorkspace::NoSpare {
            output: output.unwrap_or_else(|| "this output".to_owned()),
        })
}

// ── the two seams ────────────────────────────────────────────────────────────

/// One niri request, one niri reply — the same seam shape
/// `hytte-plugin-niri-layouts` uses, and for the same reason: it is what makes
/// [`open_all_with`] testable without a compositor.
pub trait Compositor {
    /// Send one request. `Err` is a *transport* failure; a niri-level refusal
    /// arrives as [`Reply`]'s own inner `Err`.
    ///
    /// # Errors
    /// A string an operator can read — no `$NIRI_SOCKET`, a socket that went
    /// away, a reply that would not parse.
    fn send(&mut self, request: NiriRequest) -> Result<Reply, String>;
}

/// Start one program so that it **outlives this process**.
///
/// A seam rather than a bare function because the fan-out's interesting
/// property is *what it launches, in what order relative to the focus* — and
/// asserting that against a real `systemd-run` would need a user manager.
pub trait Spawner {
    /// Launch `argv`, detached.
    ///
    /// # Errors
    /// A string naming what could not be started.
    fn launch(&mut self, argv: &[String]) -> Result<String, String>;
}

/// The real compositor: one short-lived `$NIRI_SOCKET` connection per request,
/// which is what niri's IPC guarantees a reply on for a non-`EventStream`
/// request.
pub struct SocketCompositor;

impl Compositor for SocketCompositor {
    fn send(&mut self, request: NiriRequest) -> Result<Reply, String> {
        let mut socket = niri_ipc::socket::Socket::connect()
            .map_err(|e| format!("cannot reach niri over $NIRI_SOCKET: {e}"))?;
        socket
            .send(request)
            .map_err(|e| format!("niri ipc failed: {e}"))
    }
}

/// The real launcher: a transient `systemd-run --user` service, falling back
/// to a directly spawned child in its own process group.
///
/// The systemd path is what the shell's own detached launches use and is what
/// keeps a window alive after *this* process exits — and after a
/// `trollshell.service` restart. The fallback exists for a desktop with no user
/// manager; there it is genuinely the best available answer rather than a
/// lesser version of the same thing, because without a manager there is no
/// cgroup to escape either.
pub struct DetachedSpawner;

/// The agent an agent-window argv names: the element **after**
/// [`plugin_window::ARG_AGENT`], not `argv[2]`.
///
/// Positional indexing happened to be right — `plugin_window::argv` puts the
/// name third — but that is a fact about another crate's function rather than
/// about this argv, and nothing asserted it (#1390 review, LOW 3). Taking the
/// *first* `--agent` is also what keeps an agent legitimately named `--agent`
/// (hyperhive's `Ident` admits it) reading as a value.
#[must_use]
pub fn agent_in(argv: &[String]) -> Option<&str> {
    let flag = argv.iter().position(|a| a == plugin_window::ARG_AGENT)?;
    argv.get(flag + 1).map(String::as_str)
}

/// The transient unit one launched window runs as.
///
/// A pure function for the reason the shell's counterpart
/// (`trollshell/src/plugins/effects.rs`'s `launch_unit_name`) is one: the
/// format is a contract with `systemctl --user list-units 'trollshell-launch-*'`
/// and with a *second* run of this mode, so it is worth asserting rather than
/// spelling inline in an impl no test can reach.
///
/// The `pid` as well as the `seq`, for that counterpart's own reason: these
/// units outlive the process that asked for them, so a second run's `seq`
/// starts at 0 again while the first run's `…-0.service` may still be alive.
#[must_use]
pub fn unit_name(agent: &str, pid: u32, seq: u64) -> String {
    format!("{UNIT_PREFIX}-{agent}-{pid}-{seq}.service")
}

impl Spawner for DetachedSpawner {
    fn launch(&mut self, argv: &[String]) -> Result<String, String> {
        let Some(agent) = agent_in(argv) else {
            return Err(format!("not a window launch: {argv:?}"));
        };
        let seq = LAUNCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let unit = unit_name(agent, std::process::id(), seq);
        match systemd_run(&unit, argv) {
            Ok(()) => Ok(format!("unit {unit}")),
            Err(reason) => {
                tracing::warn!(
                    %reason,
                    "systemd user manager unavailable; spawning the window directly"
                );
                spawn_directly(argv)
            }
        }
    }
}

/// Ask the user manager to start `argv` as the transient service `unit`.
fn systemd_run(unit: &str, argv: &[String]) -> Result<(), String> {
    let mut cmd = std::process::Command::new("systemd-run");
    cmd.arg("--user")
        .arg("--quiet")
        .arg("--collect")
        .arg(format!("--slice={LAUNCH_SLICE}"))
        .arg(format!("--unit={unit}"))
        .arg(format!(
            "--description=trollshell agent window: {}",
            agent_in(argv).unwrap_or("?")
        ));
    for name in FORWARDED_ENV {
        // Skipped when unset or empty, so a launch never asserts an empty
        // `DISPLAY=` over the manager's real one.
        if let Ok(value) = std::env::var(name)
            && !value.is_empty()
        {
            cmd.arg(format!("--setenv={name}={value}"));
        }
    }
    cmd.arg("--")
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // A plain blocking wait, bounded the way it is because `systemd-run`
    // returns as soon as the manager takes the job (measured in the shell's
    // own notes: ~7 ms for a transient service). `output()` has no timeout of
    // its own, so the bound is the child being killed on the way out — which
    // is safe here and nowhere else, since the unit the manager already
    // accepted is untouched by this helper dying.
    let child = cmd
        .spawn()
        .map_err(|e| format!("systemd-run could not be run: {e}"))?;
    let out = wait_bounded(child, LAUNCH_CALL_TIMEOUT)?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "systemd-run {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// `Child::wait_with_output` with a deadline, by polling `try_wait`.
///
/// Deliberately not a thread plus a channel: the wait is milliseconds in every
/// healthy case, and the failure it guards is a manager that never answers —
/// where a killed helper is the correct outcome and a leaked thread would not
/// be.
fn wait_bounded(
    mut child: std::process::Child,
    deadline: Duration,
) -> Result<std::process::Output, String> {
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|e| format!("systemd-run: {e}"));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("systemd-run: {e}")),
        }
        if started.elapsed() >= deadline {
            let _ = child.kill();
            return Err(format!(
                "systemd-run --user did not answer within {}s",
                deadline.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Spawn `argv` into a **new process group**, so a signal delivered to this
/// process's group does not reach it.
///
/// That is the whole detachment `unsafe`-free code can buy — `setsid` would
/// need `pre_exec`, and `unsafe_code = "forbid"` is workspace policy. The
/// handle is dropped and never waited on; the child is reparented when this
/// process exits.
fn spawn_directly(argv: &[String]) -> Result<String, String> {
    use std::os::unix::process::CommandExt as _;
    let Some((program, tail)) = argv.split_first() else {
        return Err("empty argv; nothing to launch".to_owned());
    };
    let child = std::process::Command::new(program)
        .args(tail)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("spawning {program}: {e}"))?;
    Ok(format!("pid {} (detached process group)", child.id()))
}

// ── the orchestration ────────────────────────────────────────────────────────

/// What one fan-out did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Windows whose launch was accepted.
    pub launched: usize,
    /// Windows whose launch could not be started at all.
    pub failed: usize,
    /// The workspace the windows were sent to, or `None` when none could be
    /// picked (in which case they opened wherever the compositor was).
    pub workspace: Option<Target>,
}

/// Which of `agents` has **no** companion window open yet, in the order they
/// were given.
///
/// Asked of niri's window list by matching its `app_id` against
/// [`crate::cli::app_id`] — the very id `main` registers the application
/// under, which is what makes `GApplication`'s single-instance machinery "one
/// window per agent" in the first place. So this mode keeps no bookkeeping: it
/// reads back the fact the id already encodes, from the same window list
/// [`pick_workspace`] tests emptiness with.
///
/// A window list that could not be read is **not** "nothing is open" — the
/// caller passes an empty slice there and gets every agent back, which is the
/// answer that keeps the feature working when niri does not answer.
#[must_use]
pub fn without_a_window(windows: &[Window], agents: &[AgentName]) -> Vec<AgentName> {
    agents
        .iter()
        .filter(|agent| {
            let id = crate::cli::app_id(agent);
            !windows
                .iter()
                .any(|w| w.app_id.as_deref() == Some(id.as_str()))
        })
        .cloned()
        .collect()
}

/// Focus a workspace, then launch one window per agent that needs one.
///
/// # The two shapes one run can take
///
/// - **Something to place.** At least one running agent has no window yet, so
///   a workspace is picked, focused, and *then* the missing windows are
///   launched onto it. The **order is the mechanism** and is what the tests
///   pin: niri opens a new window on the focused workspace, so every launch
///   has to come after the focus.
/// - **Nothing to place** — the second press, with every running agent's
///   window already open. No workspace is picked and **no focus is sent**;
///   every launch is `GApplication` presenting the window that already exists,
///   wherever the operator left it.
///
/// # Why the second press must not pick a workspace (#1390 review, MED 1)
///
/// After the first press the fan-out's workspace holds windows, so niri's
/// trailing spare is the *next* one — and a second press that picked it would
/// focus an **empty** workspace before presenting anything. Either the
/// presents bounce the focus back out of it (a visible detour through an empty
/// workspace on every repeat press) or the compositor declines the activation
/// — a `systemd-run` child carries no `XDG_ACTIVATION_TOKEN` and this process
/// holds no focused surface — and the operator is left sitting on an empty
/// workspace with every agent window somewhere else.
///
/// # Why an agent that already has a window is not re-launched here
///
/// In the **mixed** case (one window open, one to go) presenting the open one
/// would move the focus off the workspace this run is filling, and the very
/// next launch would land wherever that present went — which is exactly the
/// "a focus that landed between two launches would scatter them" race the
/// deciding moved into this binary to avoid. So a run that has something to
/// place places only that, and leaves the rest where they are; that is the
/// same **no-gather** rule a window the operator dragged away already follows.
///
/// A niri that cannot be reached, or an output with no spare workspace, costs
/// the tidiness and not the windows — the fan-out still launches, on whatever
/// workspace is current, with one line saying so.
pub fn open_all_with(
    niri: &mut impl Compositor,
    spawner: &mut impl Spawner,
    agents: &[AgentName],
) -> Report {
    let mut report = Report::default();

    // **One** look at niri, up front: the same two lists answer both questions
    // this run has — which workspace is the spare, and which of these agents
    // already has a window.
    let seen = niri_lists(niri);
    let open_windows: &[Window] = match &seen {
        Ok((_, windows)) => windows,
        Err(_) => &[],
    };
    let missing = without_a_window(open_windows, agents);

    let launch: &[AgentName] = if missing.is_empty() {
        tracing::info!(
            open = agents.len(),
            "every running agent already has a window; presenting them where they are"
        );
        agents
    } else {
        match &seen {
            Ok((workspaces, windows)) => match pick_workspace(workspaces, windows) {
                Ok(target) => match focus(niri, &target) {
                    Ok(()) => {
                        tracing::info!(
                            workspace = target.id,
                            idx = target.idx,
                            output = target.output.as_deref().unwrap_or("<none>"),
                            opening = missing.len(),
                            "focused an empty workspace for the agent windows"
                        );
                        report.workspace = Some(target);
                    }
                    Err(reason) => tracing::warn!(
                        %reason,
                        "opening the agent windows on the current workspace instead"
                    ),
                },
                Err(reason) => tracing::warn!(
                    %reason,
                    "opening the agent windows on the current workspace instead"
                ),
            },
            Err(reason) => tracing::warn!(
                %reason,
                "opening the agent windows on the current workspace instead"
            ),
        }
        &missing
    };

    for agent in launch {
        // The plugin's own builder, not a second copy: `--agent <name>` as two
        // argv elements, which is what keeps a legal leading-hyphen name a
        // name rather than a flag.
        let argv = plugin_window::argv(agent.as_str(), Tab::Agent);
        match spawner.launch(&argv) {
            Ok(what) => {
                tracing::info!(agent = %agent, started = %what, "agent window launched");
                report.launched += 1;
            }
            Err(e) => {
                tracing::error!(agent = %agent, error = %e, "agent window could not be launched");
                report.failed += 1;
            }
        }
    }
    report
}

/// niri's workspace list and window list, in that order — the two answers one
/// run needs, fetched once.
fn niri_lists(niri: &mut impl Compositor) -> Result<(Vec<Workspace>, Vec<Window>), String> {
    Ok((workspaces(niri)?, windows(niri)?))
}

/// Go to `target`'s workspace.
///
/// **By id, never by index**: `Workspace::idx` is a current position that
/// changes as workspaces are re-ordered, and it is per-monitor besides.
fn focus(niri: &mut impl Compositor, target: &Target) -> Result<(), String> {
    ask(
        niri,
        NiriRequest::Action(Action::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(target.id),
        }),
    )?;
    Ok(())
}

fn ask(niri: &mut impl Compositor, request: NiriRequest) -> Result<NiriResponse, String> {
    niri.send(request)?
}

fn workspaces(niri: &mut impl Compositor) -> Result<Vec<Workspace>, String> {
    match ask(niri, NiriRequest::Workspaces)? {
        NiriResponse::Workspaces(w) => Ok(w),
        other => Err(format!("niri answered a Workspaces request with {other:?}")),
    }
}

fn windows(niri: &mut impl Compositor) -> Result<Vec<Window>, String> {
    match ask(niri, NiriRequest::Windows)? {
        NiriResponse::Windows(w) => Ok(w),
        other => Err(format!("niri answered a Windows request with {other:?}")),
    }
}

// ── the entry point ──────────────────────────────────────────────────────────

/// One `AgentStatus` round trip on `host.sock`, through the **plugin's own
/// client** — the same connect/write/read/version-check path the sidebar card
/// and this window's header already use, so three readers of one socket cannot
/// disagree about its verbs.
///
/// An answer with no `agent_statuses` field reads as an empty roster, which is
/// what `AgentState::of` does with the same shape: the hive answered, and it
/// listed nothing.
///
/// # Errors
/// The hive's own operator-facing sentence — unreachable, unparseable, a
/// wire version this build cannot read, or a refusal.
pub async fn roster_from(socket: &Path) -> Result<Vec<AgentStatusRow>, HiveError> {
    let answer: Response = client::request(socket, &HiveRequest::AgentStatus).await?;
    Ok(answer.agent_statuses.unwrap_or_default())
}

/// [`roster_from`] on a runtime of this mode's own — there is no ambient one,
/// since `--open-all` never reaches the window's `main`.
fn roster(socket: &Path) -> Result<Vec<AgentStatusRow>, HiveError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| HiveError::Unreachable {
            reason: format!("could not start a runtime: {e}"),
        })?;
    runtime.block_on(roster_from(socket))
}

/// `--open-all`, against the real hive, the real compositor and the real
/// launcher. Returns this process's exit code.
///
/// **Nothing running is exit 0.** A fan-out on a quiet hive has done exactly
/// what it was asked to, and a non-zero exit would make a keybind look broken;
/// the one logged line is the whole report. (The log goes to **stdout** —
/// `main` installs `tracing_subscriber::fmt`, whose default writer is
/// `io::stdout`; only `main`'s own usage line is an `eprintln!`. Both land in
/// the journal under `-t trollshell-agent-window` either way, but a caller
/// redirecting one stream should know which.)
///
/// Pinned end to end, as the real binary, by `tests/open_all_binary.rs` — the
/// exit codes are this function's whole observable contract, and nothing
/// asserted them before the #1390 review.
#[must_use]
pub fn run() -> u8 {
    let cfg = hytte_plugin_agents::config::load();
    let socket = PathBuf::from(&cfg.socket);
    let rows = match roster(&socket) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(socket = %socket.display(), error = %e, "cannot read the hive's roster");
            return EXIT_FAILED;
        }
    };

    let agents = running_agents(&rows);
    if agents.is_empty() {
        tracing::info!(
            listed = rows.len(),
            "no agent is running — nothing to open (only running agents get a window)"
        );
        return EXIT_OK;
    }

    let report = open_all_with(&mut SocketCompositor, &mut DetachedSpawner, &agents);
    tracing::info!(
        launched = report.launched,
        failed = report.failed,
        "agent windows opened"
    );
    if report.launched == 0 {
        EXIT_FAILED
    } else {
        EXIT_OK
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Compositor, NoWorkspace, Report, Spawner, Target, agent_in, open_all_with, pick_workspace,
        running_agents, unit_name, without_a_window,
    };
    use hytte_plugin_agents::hive::wire::AgentStatusRow;
    use hytte_plugin_agents::model::AgentName;
    use niri_ipc::{Action, Reply, Request as NiriRequest, Response as NiriResponse};

    // ── niri fixtures ────────────────────────────────────────────────────────
    //
    // Spelled as the JSON `niri msg -j workspaces` / `niri msg -j windows`
    // actually print, and deserialized through `niri-ipc`'s own types, so
    // these pin the **field names** as well as the rule: a niri-ipc bump that
    // renamed `active_window_id` or `is_focused` reds here rather than
    // silently changing what "empty" means. Inline rather than files under
    // `tests/fixtures/`, because the crane source filter keeps `.rs` and
    // strips `.json` — an `include_str!` of one would pass `cargo test` and
    // fail every `nix build` (the #480 trap).

    /// One output, one populated workspace and niri's trailing spare — the
    /// ordinary desktop.
    const ONE_OUTPUT: &str = r#"[
      {"id":1,"idx":1,"name":null,"output":"DP-2","is_urgent":false,"is_active":true,"is_focused":true,"active_window_id":7},
      {"id":2,"idx":2,"name":null,"output":"DP-2","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null}
    ]"#;

    /// Two outputs, where the **unfocused** one carries the higher-numbered
    /// spare. Picking by `idx` alone would land the windows on the other
    /// screen.
    const TWO_OUTPUTS: &str = r#"[
      {"id":10,"idx":1,"name":null,"output":"HDMI-A-1","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":31},
      {"id":11,"idx":2,"name":null,"output":"HDMI-A-1","is_urgent":false,"is_active":true,"is_focused":false,"active_window_id":32},
      {"id":12,"idx":3,"name":null,"output":"HDMI-A-1","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null},
      {"id":20,"idx":1,"name":null,"output":"DP-2","is_urgent":false,"is_active":true,"is_focused":true,"active_window_id":7},
      {"id":21,"idx":2,"name":null,"output":"DP-2","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null}
    ]"#;

    /// The focused output's only spare is **named**, which the Workspaces page
    /// treats as the user's own.
    const NAMED_SPARE: &str = r#"[
      {"id":1,"idx":1,"name":null,"output":"DP-2","is_urgent":false,"is_active":true,"is_focused":true,"active_window_id":7},
      {"id":2,"idx":2,"name":"scratch","output":"DP-2","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null}
    ]"#;

    /// Every workspace on the focused output is populated.
    const NO_SPARE: &str = r#"[
      {"id":1,"idx":1,"name":null,"output":"DP-2","is_urgent":false,"is_active":true,"is_focused":true,"active_window_id":7},
      {"id":2,"idx":2,"name":null,"output":"DP-2","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":9}
    ]"#;

    /// **Two** empty unnamed workspaces on the focused output, with a
    /// populated one between them: `[1 populated, 2 empty, 3 populated,
    /// 4 empty]`. The shape a workspace you have just emptied and not yet left
    /// produces, and the only fixture here that can tell "the highest `idx`"
    /// from "the first one" (#1390 review, MED 3).
    const GAP_AND_SPARE: &str = r#"[
      {"id":1,"idx":1,"name":null,"output":"DP-2","is_urgent":false,"is_active":true,"is_focused":true,"active_window_id":7},
      {"id":2,"idx":2,"name":null,"output":"DP-2","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null},
      {"id":3,"idx":3,"name":null,"output":"DP-2","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":9},
      {"id":4,"idx":4,"name":null,"output":"DP-2","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null}
    ]"#;

    /// A workspace holding a window that is **not** its active one — the
    /// shape that separates "the window list says empty" from
    /// "`active_window_id` is null".
    const NULL_ACTIVE_BUT_POPULATED: &str = r#"[
      {"id":1,"idx":1,"name":null,"output":"DP-2","is_urgent":false,"is_active":true,"is_focused":true,"active_window_id":7},
      {"id":2,"idx":2,"name":null,"output":"DP-2","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null},
      {"id":3,"idx":3,"name":null,"output":"DP-2","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null}
    ]"#;

    fn workspaces(json: &str) -> Vec<niri_ipc::Workspace> {
        serde_json::from_str(json).expect("the fixture is niri's own workspace JSON")
    }

    /// One window per `(id, workspace)` pair, with the rest of niri's window
    /// shape defaulted — `pick_workspace` reads only those two fields.
    fn windows(on: &[(u64, u64)]) -> Vec<niri_ipc::Window> {
        on.iter()
            .map(|&(id, workspace_id)| niri_ipc::Window {
                id,
                title: None,
                app_id: None,
                pid: None,
                workspace_id: Some(workspace_id),
                is_focused: false,
                is_floating: false,
                is_urgent: false,
                // `WindowLayout` has no `Default`, and nothing here reads it —
                // `pick_workspace` asks only "which workspace is this window
                // on", so one tile's worth of geometry stands in for any
                // window at all.
                layout: niri_ipc::WindowLayout {
                    pos_in_scrolling_layout: Some((1, 1)),
                    tile_size: (100.0, 100.0),
                    window_size: (100, 100),
                    tile_pos_in_workspace_view: Some((0.0, 0.0)),
                    window_offset_in_tile: (0.0, 0.0),
                },
                focus_timestamp: None,
            })
            .collect()
    }

    /// One window carrying the **per-agent application id** the fan-out
    /// matches on — built with [`crate::cli::app_id`] rather than spelled out,
    /// so a change to the mangling changes the fixture and the production
    /// lookup in the same commit. (The spelling itself is pinned as a literal
    /// in `an_agent_that_already_has_a_window_is_not_launched_again`.)
    fn agent_window(id: u64, workspace_id: u64, agent: &str) -> niri_ipc::Window {
        let mut w = windows(&[(id, workspace_id)]).remove(0);
        w.app_id = Some(crate::cli::app_id(&name(agent)));
        w
    }

    // ── pick_workspace ───────────────────────────────────────────────────────

    /// The ordinary desktop: the trailing empty workspace on the focused
    /// output.
    #[test]
    fn the_spare_at_the_bottom_of_the_focused_output_is_the_target() {
        assert_eq!(
            pick_workspace(&workspaces(ONE_OUTPUT), &windows(&[(7, 1)])),
            Ok(Target {
                id: 2,
                idx: 2,
                output: Some("DP-2".to_owned()),
            })
        );
    }

    /// Two outputs: the spare on the **focused** one wins, although the other
    /// screen's spare has a higher `idx`.
    ///
    /// Falsification: drop the `w.output == output` filter and this picks
    /// HDMI-A-1's `idx: 3` — the windows would open on the screen the operator
    /// is not looking at.
    #[test]
    fn the_unfocused_outputs_higher_spare_is_not_the_target() {
        assert_eq!(
            pick_workspace(
                &workspaces(TWO_OUTPUTS),
                &windows(&[(31, 10), (32, 11), (7, 20)]),
            ),
            Ok(Target {
                id: 21,
                idx: 2,
                output: Some("DP-2".to_owned()),
            })
        );
    }

    /// Among **two** empty unnamed workspaces on the focused output, the one
    /// at the bottom is the target: niri keeps its spare there, and the other
    /// is a gap between two populated workspaces.
    ///
    /// Falsification (verified red): `max_by_key(|w| w.idx)` →
    /// `min_by_key(|w| w.idx)` in `pick_workspace` answers `id: 2` here, which
    /// drops the fan-out into the gap. Every other fixture in this file has
    /// exactly one candidate, so that mutation was green before this test
    /// existed (#1390 review, MED 3).
    #[test]
    fn the_bottom_spare_wins_over_an_empty_workspace_in_a_gap() {
        assert_eq!(
            pick_workspace(&workspaces(GAP_AND_SPARE), &windows(&[(7, 1), (9, 3)])),
            Ok(Target {
                id: 4,
                idx: 4,
                output: Some("DP-2".to_owned()),
            })
        );
    }

    /// A **named** empty workspace is not a spare: the Workspaces page calls
    /// it the user's own, and an ephemeral card needs an unnamed one.
    ///
    /// Falsification: drop the `name.is_none()` filter and this returns
    /// `scratch` instead of refusing — the windows would take over a workspace
    /// somebody named, and would render as no ephemeral card at all.
    #[test]
    fn a_named_empty_workspace_is_not_a_spare() {
        assert_eq!(
            pick_workspace(&workspaces(NAMED_SPARE), &windows(&[(7, 1)])),
            Err(NoWorkspace::NoSpare {
                output: "DP-2".to_owned(),
            })
        );
    }

    /// Nothing empty on the focused output at all — the caller launches
    /// anyway, so this only has to be an error that names the screen.
    #[test]
    fn a_full_output_has_no_target_and_says_which_output() {
        let e = pick_workspace(&workspaces(NO_SPARE), &windows(&[(7, 1), (9, 2)]))
            .expect_err("nothing is empty");
        assert_eq!(
            e,
            NoWorkspace::NoSpare {
                output: "DP-2".to_owned(),
            }
        );
        assert!(e.to_string().contains("DP-2"), "{e}");
        assert!(
            e.to_string().contains("current"),
            "the sentence says what happens next: {e}"
        );
    }

    /// Emptiness is the **window list**, not `active_window_id`.
    ///
    /// Workspace 2 reports `active_window_id: null` while holding a window;
    /// workspace 3 is genuinely empty. Falsification: test emptiness with
    /// `w.active_window_id.is_none()` and this picks 3 either way — so the
    /// assertion is written the other way round: with workspace 3 removed
    /// from the fixture, the `active_window_id` test would pick the populated
    /// workspace 2 and stack the fan-out on top of somebody's windows.
    #[test]
    fn a_workspace_with_no_active_window_can_still_hold_windows() {
        let ws = workspaces(NULL_ACTIVE_BUT_POPULATED);
        let wins = windows(&[(7, 1), (8, 2)]);
        assert_eq!(
            pick_workspace(&ws, &wins).map(|t| t.id),
            Ok(3),
            "the empty one is 3; 2 holds a window despite its null active id"
        );

        // …and with the genuinely empty workspace gone, there is no spare at
        // all — which an `active_window_id`-based test would answer with the
        // populated workspace 2.
        let truncated: Vec<_> = ws.into_iter().filter(|w| w.id != 3).collect();
        assert!(pick_workspace(&truncated, &wins).is_err());
    }

    /// No focused workspace at all — an empty or malformed list.
    #[test]
    fn a_workspace_list_with_no_focus_has_no_target() {
        assert_eq!(pick_workspace(&[], &[]), Err(NoWorkspace::NoFocus));
        assert!(!NoWorkspace::NoFocus.to_string().is_empty());
    }

    // ── membership ───────────────────────────────────────────────────────────

    fn row(name: &str, running: bool, paused: bool, failed: bool) -> AgentStatusRow {
        AgentStatusRow {
            name: name.to_owned(),
            running,
            paused,
            failed,
            ..AgentStatusRow::default()
        }
    }

    /// Exactly the running agents, in the hive's own order — which becomes the
    /// column order niri tiles them in.
    ///
    /// Falsification: filter on `row.running` instead of on the collapsed
    /// [`Status`] and `parked` (running **and** paused) joins the list, which
    /// is the "up" reading #1306 settled against.
    #[test]
    fn only_the_running_agents_get_a_window_in_roster_order() {
        let rows = vec![
            row("wedged", true, false, true),
            row("busy", true, false, false),
            row("parked", true, true, false),
            row("off", false, false, false),
            row("argus", true, false, false),
        ];
        assert_eq!(
            running_agents(&rows)
                .iter()
                .map(AgentName::as_str)
                .collect::<Vec<_>>(),
            vec!["busy", "argus"],
        );
    }

    /// A name the whitelist refuses never reaches an argv — it is dropped,
    /// and the rest of the roster still opens.
    #[test]
    fn a_name_that_fails_the_whitelist_is_dropped_not_launched() {
        let rows = vec![
            row("../etc/passwd", true, false, false),
            row("argus", true, false, false),
        ];
        assert_eq!(
            running_agents(&rows)
                .iter()
                .map(AgentName::as_str)
                .collect::<Vec<_>>(),
            vec!["argus"],
        );
    }

    // ── the launched unit (#1390 review, LOW 3) ──────────────────────────────

    /// The agent is read out of the argv **by its flag**, against the builder
    /// that writes it — both tabs, and the leading-hyphen name the whole
    /// two-element shape exists for.
    ///
    /// Falsification: go back to `argv.get(2)` and the `--tab settings` row
    /// still passes while an argv that ever grew a flag in front of `--agent`
    /// would silently name the wrong thing; go back to it *and* swap the
    /// builder's two flags and the `-leading-hyphen` row answers `"--agent"`.
    #[test]
    fn the_agent_is_read_out_of_the_argv_by_its_flag() {
        use hytte_plugin_agents::window::{Tab as PluginTab, argv as plugin_argv, open_all_argv};

        assert_eq!(
            agent_in(&plugin_argv("argus", PluginTab::Agent)),
            Some("argus")
        );
        assert_eq!(
            agent_in(&plugin_argv("argus", PluginTab::Settings)),
            Some("argus")
        );
        assert_eq!(
            agent_in(&plugin_argv("-leading-hyphen", PluginTab::Agent)),
            Some("-leading-hyphen"),
        );
        // The fan-out's own argv names no agent — which is what makes
        // `DetachedSpawner::launch`'s refusal reachable rather than decorative.
        assert_eq!(agent_in(&open_all_argv()), None);
        assert_eq!(agent_in(&[]), None);
        assert_eq!(
            agent_in(&["trollshell-agent-window".to_owned(), "--agent".to_owned()]),
            None,
            "a flag with no value is not a name"
        );
    }

    /// The unit name a launched window runs as — the format
    /// `systemctl --user list-units 'trollshell-launch-*'` globs, and the one
    /// a *second* run must not collide with.
    ///
    /// Falsification: drop the `pid` (or the `seq`) and the two rows below
    /// stop differing, which on a real desktop is `systemd-run` refusing the
    /// second window with "unit … already exists".
    #[test]
    fn the_unit_name_carries_the_agent_the_pid_and_the_sequence() {
        assert_eq!(
            unit_name("argus", 4242, 0),
            "trollshell-launch-agent-window-argus-4242-0.service"
        );
        assert!(
            unit_name("argus", 4242, 0).starts_with("trollshell-launch-"),
            "the shell's own list-units glob has to find it"
        );
        assert_ne!(unit_name("argus", 4242, 0), unit_name("argus", 4242, 1));
        assert_ne!(unit_name("argus", 4242, 0), unit_name("argus", 99, 0));
    }

    // ── the fan-out ──────────────────────────────────────────────────────────

    /// A niri that records every request and answers from a fixture.
    struct FakeNiri {
        workspaces: Vec<niri_ipc::Workspace>,
        windows: Vec<niri_ipc::Window>,
        /// `None` makes every send fail, as an unreachable `$NIRI_SOCKET`
        /// does.
        reachable: bool,
    }

    /// Everything the fan-out did, in order — the one recording both fakes
    /// write to, because *the order between them* is the assertion.
    #[derive(Default)]
    struct Journal(std::rc::Rc<std::cell::RefCell<Vec<String>>>);

    impl Journal {
        fn push(&self, line: String) {
            self.0.borrow_mut().push(line);
        }
        fn lines(&self) -> Vec<String> {
            self.0.borrow().clone()
        }
        fn share(&self) -> Self {
            Self(std::rc::Rc::clone(&self.0))
        }
    }

    struct RecordingNiri {
        niri: FakeNiri,
        journal: Journal,
    }

    impl Compositor for RecordingNiri {
        fn send(&mut self, request: NiriRequest) -> Result<Reply, String> {
            if !self.niri.reachable {
                return Err("cannot reach niri over $NIRI_SOCKET: no such file".to_owned());
            }
            Ok(match request {
                NiriRequest::Workspaces => {
                    Ok(NiriResponse::Workspaces(self.niri.workspaces.clone()))
                }
                NiriRequest::Windows => Ok(NiriResponse::Windows(self.niri.windows.clone())),
                NiriRequest::Action(Action::FocusWorkspace { reference }) => {
                    self.journal.push(format!("focus {reference:?}"));
                    Ok(NiriResponse::Handled)
                }
                other => return Err(format!("the fake was not asked for {other:?}")),
            })
        }
    }

    struct RecordingSpawner {
        journal: Journal,
        fail: bool,
    }

    impl Spawner for RecordingSpawner {
        fn launch(&mut self, argv: &[String]) -> Result<String, String> {
            self.journal.push(format!("launch {}", argv.join(" ")));
            if self.fail {
                return Err("no".to_owned());
            }
            Ok("unit x.service".to_owned())
        }
    }

    fn name(s: &str) -> AgentName {
        AgentName::parse(s).expect("a legal test name")
    }

    /// **The focus comes first, then every launch, in roster order.**
    ///
    /// This is the whole mechanism: niri opens a new window on the *focused*
    /// workspace, so a focus that landed after the launches — or between two of
    /// them — would scatter the windows across two workspaces. One shared
    /// journal is what makes the relative order assertable at all.
    ///
    /// Falsification (verified red): move the `focus_fresh_workspace` call
    /// below the launch loop in `open_all_with` and the first journal line
    /// becomes a launch.
    #[test]
    fn the_workspace_is_focused_before_the_first_window_is_launched() {
        let journal = Journal::default();
        let mut niri = RecordingNiri {
            niri: FakeNiri {
                workspaces: workspaces(ONE_OUTPUT),
                windows: windows(&[(7, 1)]),
                reachable: true,
            },
            journal: journal.share(),
        };
        let mut spawner = RecordingSpawner {
            journal: journal.share(),
            fail: false,
        };

        let report = open_all_with(
            &mut niri,
            &mut spawner,
            &[name("argus"), name("bosun"), name("cinder")],
        );

        assert_eq!(
            journal.lines(),
            vec![
                "focus Id(2)".to_owned(),
                "launch trollshell-agent-window --agent argus".to_owned(),
                "launch trollshell-agent-window --agent bosun".to_owned(),
                "launch trollshell-agent-window --agent cinder".to_owned(),
            ],
        );
        assert_eq!(
            report,
            Report {
                launched: 3,
                failed: 0,
                workspace: Some(Target {
                    id: 2,
                    idx: 2,
                    output: Some("DP-2".to_owned()),
                }),
            }
        );
    }

    // ── who still needs a window (#1390 review, MED 1) ───────────────────────

    /// [`without_a_window`]'s three cases, stated directly: a window list that
    /// is empty (or could not be read) means *everyone* needs one, a window
    /// belonging to somebody else does not count, and only the agent's own
    /// application id does.
    #[test]
    fn only_an_agent_windows_own_app_id_counts_as_open() {
        let agents = [name("argus"), name("bosun")];
        let names = |v: Vec<AgentName>| {
            v.iter()
                .map(|a| a.as_str().to_owned())
                .collect::<Vec<String>>()
        };

        assert_eq!(
            names(without_a_window(&[], &agents)),
            vec!["argus", "bosun"]
        );

        // A terminal on the fan-out's own workspace is not an agent window.
        let mut stranger = windows(&[(9, 2)]).remove(0);
        stranger.app_id = Some("org.wezfurlong.wezterm".to_owned());
        assert_eq!(
            names(without_a_window(&[stranger], &agents)),
            vec!["argus", "bosun"]
        );

        assert_eq!(
            names(without_a_window(&[agent_window(8, 1, "argus")], &agents)),
            vec!["bosun"],
        );
    }

    /// **A mixed press places only what is missing.** `argus` already has a
    /// window; `bosun` does not. The run focuses the spare and launches
    /// `bosun` alone — `argus` is left where it is rather than presented,
    /// because presenting it mid-run would move the focus out from under the
    /// launch that follows, which is the scatter the deciding moved into this
    /// binary to avoid.
    ///
    /// Falsification (verified red): launch `agents` instead of `missing` in
    /// `open_all_with` and the journal grows a `launch … --agent argus`.
    #[test]
    fn an_agent_that_already_has_a_window_is_not_launched_again() {
        // The spelling the match depends on, pinned once as a literal: it is
        // what *niri* reports, so a silent change to the mangling would make
        // every agent look closed forever.
        assert_eq!(
            crate::cli::app_id(&name("argus")),
            "mov.vibec0re.trollshell.AgentWindow.argus"
        );

        let journal = Journal::default();
        let mut niri = RecordingNiri {
            niri: FakeNiri {
                workspaces: workspaces(ONE_OUTPUT),
                windows: vec![windows(&[(7, 1)]).remove(0), agent_window(8, 1, "argus")],
                reachable: true,
            },
            journal: journal.share(),
        };
        let mut spawner = RecordingSpawner {
            journal: journal.share(),
            fail: false,
        };

        let report = open_all_with(&mut niri, &mut spawner, &[name("argus"), name("bosun")]);

        assert_eq!(
            journal.lines(),
            vec![
                "focus Id(2)".to_owned(),
                "launch trollshell-agent-window --agent bosun".to_owned(),
            ],
        );
        assert_eq!(report.launched, 1);
        assert_eq!(report.workspace.map(|t| t.id), Some(2));
    }

    /// **A second press picks no workspace at all.** Every running agent
    /// already has a window, so there is nothing to place: no `FocusWorkspace`
    /// is sent, and each launch is `GApplication` presenting the window that
    /// exists, wherever the operator left it.
    ///
    /// Falsification (verified red): focus unconditionally — the shape before
    /// the #1390 review — and the journal gains a leading `focus Id(2)`, which
    /// on a real desktop is the **empty** workspace below the one holding the
    /// windows: a detour at best, and being stranded there at worst.
    #[test]
    fn a_second_press_focuses_nothing_and_presents_what_is_open() {
        let journal = Journal::default();
        let mut niri = RecordingNiri {
            niri: FakeNiri {
                workspaces: workspaces(ONE_OUTPUT),
                windows: vec![agent_window(8, 1, "argus")],
                reachable: true,
            },
            journal: journal.share(),
        };
        let mut spawner = RecordingSpawner {
            journal: journal.share(),
            fail: false,
        };

        let report = open_all_with(&mut niri, &mut spawner, &[name("argus")]);

        assert_eq!(
            journal.lines(),
            vec!["launch trollshell-agent-window --agent argus".to_owned()],
            "the present goes out; nothing is focused"
        );
        assert_eq!(
            report.workspace, None,
            "no workspace was picked, so none is reported"
        );
        assert_eq!(report.launched, 1);
    }

    /// A niri nobody can reach costs the workspace, **not** the windows: the
    /// same launches go out, byte for byte, with no focus in front of them.
    ///
    /// Falsification: return early from `open_all_with` when the focus fails
    /// and the launch lines disappear.
    #[test]
    fn an_unreachable_niri_still_opens_every_window() {
        let journal = Journal::default();
        let mut niri = RecordingNiri {
            niri: FakeNiri {
                workspaces: Vec::new(),
                windows: Vec::new(),
                reachable: false,
            },
            journal: journal.share(),
        };
        let mut spawner = RecordingSpawner {
            journal: journal.share(),
            fail: false,
        };

        let report = open_all_with(&mut niri, &mut spawner, &[name("argus"), name("bosun")]);

        assert_eq!(
            journal.lines(),
            vec![
                "launch trollshell-agent-window --agent argus".to_owned(),
                "launch trollshell-agent-window --agent bosun".to_owned(),
            ],
            "no focus, and nothing else moved"
        );
        assert_eq!(report.launched, 2);
        assert_eq!(report.workspace, None);
    }

    /// An output with no spare is the same degradation as an unreachable niri:
    /// the windows open on the current workspace.
    #[test]
    fn an_output_with_no_spare_still_opens_every_window() {
        let journal = Journal::default();
        let mut niri = RecordingNiri {
            niri: FakeNiri {
                workspaces: workspaces(NO_SPARE),
                windows: windows(&[(7, 1), (9, 2)]),
                reachable: true,
            },
            journal: journal.share(),
        };
        let mut spawner = RecordingSpawner {
            journal: journal.share(),
            fail: false,
        };

        let report = open_all_with(&mut niri, &mut spawner, &[name("argus")]);
        assert_eq!(
            journal.lines(),
            vec!["launch trollshell-agent-window --agent argus".to_owned()],
        );
        assert_eq!(report.workspace, None);
        assert_eq!(report.launched, 1);
    }

    /// Launches that cannot start are counted rather than swallowed — `run`
    /// turns "none of them started" into a non-zero exit.
    #[test]
    fn launch_failures_are_counted() {
        let journal = Journal::default();
        let mut niri = RecordingNiri {
            niri: FakeNiri {
                workspaces: workspaces(ONE_OUTPUT),
                windows: windows(&[(7, 1)]),
                reachable: true,
            },
            journal: journal.share(),
        };
        let mut spawner = RecordingSpawner {
            journal: journal.share(),
            fail: true,
        };
        let report = open_all_with(&mut niri, &mut spawner, &[name("argus"), name("bosun")]);
        assert_eq!(report.launched, 0);
        assert_eq!(report.failed, 2);
    }
}
