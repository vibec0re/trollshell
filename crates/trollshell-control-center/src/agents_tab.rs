//! The **Agents** tab (#947 phase P4, spec
//! `docs/superpowers/specs/2026-09-07-agentic-desktop-design.md` §10) — the
//! hyperhive roster, read-only, polled straight off `host.sock`.
//!
//! # What it is
//!
//! One row per agent of the one hive `agents.toml` configures, with the detail
//! pane carrying **what the wire reports and nothing else**: the five flags
//! (running, paused, failed, needs update, needs login), the harness's status
//! text and when it was set, the active model, the parent, the deployed sha,
//! and two links — the agent's own page and the hive's config repo.
//!
//! **v1 is read-only, and read-only is not a placeholder** (spec §10): edits
//! hive-side are a forge PR plus an operator approval
//! (`ApprovalKind::MergeConfigPr`), with no field-level write to offer. So
//! there is no switch, no pause button, no `Start`/`Stop` — and, deliberately,
//! **no new `Control` D-Bus method and no new service**. The write path is
//! downstream of #952 and stays with P5.
//!
//! # It is the plugin's client, not a second one
//!
//! Everything on the wire — [`Request`](hytte_plugin_agents::hive::wire::Request),
//! the row, the version check, the one-connection-per-request round trip, the
//! `Status` precedence collapse, the `group` ordering and `agents.toml` itself
//! — comes from `hytte-plugin-agents`, which exposes `hive`, `model`, `config`
//! and `window` as a library for exactly this. That crate is GTK-free (it
//! drags the plugin SDK's rlib — proto + `hytte-preem` + tokio — and no GUI
//! closure, and no `hytte-services`), which is the #640 argument for
//! `hytte-config` applied to a wire: **three** readers of one socket now (the
//! sidebar card, the companion window, this tab) have to agree byte for byte
//! about its verbs, and a third mirror would be a third thing to update when
//! hyperhive moves.
//!
//! The **cadence** is `agents.toml`'s `poll_seconds` too, so an operator who
//! slowed the sidebar down has slowed this down as well.
//!
//! # The shape
//!
//! [`crate::plugins_tab`]'s, deliberately — an `AdwBreakpointBin` over an
//! `AdwNavigationSplitView`, split panes wide and push navigation narrow, one
//! widget tree for both, and a detail pane that is **retargeted** rather than
//! rebuilt so the poll stays invisible. Read that module's doc for the
//! reasoning behind each of those; what follows is only what differs here.
//!
//! - **The sidebar's order is the plugin's**, not this file's: [`ordered`]
//!   flattens `model::group`'s own output (named projects alphabetically, the
//!   ungrouped bucket last, the hive's own order inside a group), so the tab
//!   lists the same agents in the same order the sidebar card does. That is
//!   the one default the P4 note put up for veto, and re-deriving it here
//!   rather than reusing `group` is exactly how the two would drift.
//! - **A membership change is a rebuild, anything else is in place.** Same
//!   predicate shape as the Plugins tab ([`same_agent_set`]), for the same
//!   reason: a teardown costs the selection and, collapsed, the pushed page.
//! - **An unreachable hive is a state, never an empty tab.** Every non-`Up`
//!   [`Hive`] renders one non-selectable sidebar row plus a detail status page
//!   that **names the socket path** ([`placeholder`]), because the three ways
//!   this fails — no daemon, no `hive-admin` group, a wire version this build
//!   refuses to guess at — all send the operator somewhere different and all
//!   start with "which socket did you mean".
//!
//! # Why the poll is a `glib` timer and not a task
//!
//! The tab has no runtime of its own: [`crate::spawn_on_runtime`] puts one
//! round trip on the shared `hytte-reactive` runtime and hands the answer back
//! to the GTK thread over a oneshot. That is the same seam the Plugins tab's
//! `Control` calls use, so the window's existing "drop the timer on close"
//! bookkeeping (#542) covers this tab with no new machinery.
//!
//! # Testing without a hive
//!
//! The row and detail mapping is a pure function of one wire snapshot
//! ([`rows_of`], [`detail_of`], [`hive_of`]) and is tested as one; the socket
//! path itself is driven against a scripted `UnixListener` in a tempdir
//! (`tests::scripted`), the shape `hytte-plugin-agents/tests/fake_socket.rs`
//! uses. Nothing here needs a hive, and nothing here reads the real
//! `$XDG_CONFIG_HOME`: every test builds its own [`AgentsConfig`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use hytte_plugin_agents::config::AgentsConfig;
use hytte_plugin_agents::hive::client::{self, HiveError};
use hytte_plugin_agents::hive::wire::{HiveUrls, Request, Response};
use hytte_plugin_agents::model::{Agent, AgentName, Hive, Status, agent_url, group};
use hytte_plugin_agents::view::{age, parse_set_at};
use hytte_plugin_agents::window as agent_window;

use crate::spawn_on_runtime;

// ── Layout constants — the Plugins tab's, and why they are copied ────────────
//
// Not `pub(crate)`-imported from `plugins_tab`: those are that tab's numbers,
// derived from *its* content (a plugin id plus a unit line). These are derived
// the same way from this tab's, and they happen to land in the same place
// because both sit in the same 760 px window. Sharing them would make one
// tab's content a constraint on the other's.

/// The width at or below which the split view collapses to one pane at a time.
///
/// The window opens at 760 × 560 (`crate::build_window`) and the sidebar is
/// clamped to [`SIDEBAR_MIN_PX`] … [`SIDEBAR_MAX_PX`]. The detail pane here
/// carries `AdwActionRow`s whose subtitle is a free-text status line from the
/// harness, which stops being readable well before a plugin's unit line does —
/// so the same 520 px floor the Plugins tab derived is, if anything,
/// conservative here. Below it, one pane at a time is strictly better.
const COLLAPSE_WIDTH_PX: f64 = 520.0;

/// The sidebar's floor. Agent names are short (`argus`, `trollshell-choom`)
/// but the row carries a status caption as well, so much under this and the
/// caption starts ellipsizing.
const SIDEBAR_MIN_PX: f64 = 220.0;

/// The sidebar's ceiling — past this the list is whitespace and the detail
/// pane is what pays for it.
const SIDEBAR_MAX_PX: f64 = 300.0;

/// The share of a wide split the sidebar asks for, clamped by the two above.
const SIDEBAR_FRACTION: f64 = 0.32;

/// The bin's own minimum width. libadwaita strips an `AdwBreakpointBin`'s
/// minimum size in **both** directions once a breakpoint is added, and warns
/// on every allocation if a child's minimum then exceeds the bin's — see
/// `plugins_tab`'s `BIN_MIN_WIDTH_PX` for the full #856 note. Comfortably
/// below [`COLLAPSE_WIDTH_PX`] so the collapsed configuration is reachable.
const BIN_MIN_WIDTH_PX: i32 = 360;

/// The bin's own minimum height — the other half of the same contract.
const BIN_MIN_HEIGHT_PX: i32 = 200;

// ── The pure layer: one wire snapshot → what the widgets show ────────────────

/// Fold one `AgentStatus` round trip into the roster state the tab renders.
///
/// The same four-way split the plugin's own reducer makes
/// (`hytte_plugin_agents::plugin`'s `fold_status`), minus its notification
/// edges — this tab raises no toasts, so there is nothing to diff and nothing
/// to remember between polls.
///
/// The split that matters is [`Hive::Error`] vs [`Hive::Unreachable`]: a hive
/// that **answered** with `ok: false`, or with a line this build cannot parse,
/// is up, and calling that "unreachable" sends the operator to `systemctl` for
/// a problem that is not there.
///
/// A row whose name fails [`AgentName::parse`] is dropped rather than
/// rendered, which is the plugin's rule (spec §11 rule two) and matters here
/// too: the name is what a companion-window launch passes as `--agent`.
#[must_use]
pub(crate) fn hive_of(answer: &Result<Response, HiveError>) -> Hive {
    match answer {
        Err(HiveError::Version(mismatch)) => Hive::Incompatible(*mismatch),
        Err(e @ (HiveError::Refused { .. } | HiveError::Protocol { .. })) => Hive::Error {
            reason: e.to_string(),
        },
        Err(e) => Hive::Unreachable {
            reason: e.to_string(),
        },
        Ok(resp) => Hive::Up {
            agents: resp
                .agent_statuses
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter_map(|row| {
                    let Some(name) = AgentName::parse(&row.name) else {
                        tracing::warn!(
                            name = %row.name,
                            "agent name failed the whitelist; row dropped"
                        );
                        return None;
                    };
                    Some(Agent {
                        name,
                        row: row.clone(),
                        // Nothing here writes, so there is never an
                        // unconfirmed flip to reconcile.
                        pending_paused: None,
                    })
                })
                .collect(),
        },
    }
}

/// The roster in the **sidebar's** order.
///
/// Deliberately `model::group`'s own flattening rather than an ordering of
/// this file's own: named projects alphabetically, the ungrouped bucket last,
/// the hive's own order within a group. The P4 note's one vetoable default is
/// "the same list as the sidebar, in the same order", and reusing the
/// function that *makes* the sidebar's order is the only way to keep that true
/// without a second place to change.
///
/// The group headings themselves are **not** drawn: a `GtkListBox` sidebar in
/// an `AdwNavigationSplitView` is a flat selection list, and #963's finding
/// about the card — that the roster is what the operator reads, not the
/// scaffolding around it — applies at 240 px here too. The project survives as
/// a fact on the detail pane instead.
#[must_use]
pub(crate) fn ordered<'a>(hive: &'a Hive, cfg: &'a AgentsConfig) -> Vec<&'a Agent> {
    group(hive.agents(), cfg)
        .into_iter()
        .flat_map(|g| g.agents)
        .collect()
}

/// One sidebar row's text, derived from one agent.
///
/// A struct rather than a tuple because four fields with the same type would
/// otherwise be positional, and the row and its in-place update read it from
/// opposite ends of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RowModel {
    /// The hive's own agent name — the addressing key, never the label.
    pub(crate) name: String,
    /// What the row is titled: `agents.toml`'s label, falling back to `name`.
    pub(crate) title: String,
    /// The leading icon, from `agents.toml`.
    pub(crate) icon: String,
    /// The collapsed primary state.
    pub(crate) status: Status,
    /// The second line: the harness's own text for a running agent, otherwise
    /// the state's word.
    pub(crate) subtitle: String,
    /// Whether the update badge shows.
    pub(crate) needs_update: bool,
}

/// The sidebar's rows for one snapshot, in [`ordered`]'s order.
#[must_use]
pub(crate) fn rows_of(hive: &Hive, cfg: &AgentsConfig) -> Vec<RowModel> {
    ordered(hive, cfg)
        .into_iter()
        .map(|agent| RowModel {
            name: agent.name.as_str().to_owned(),
            title: cfg.label_for(agent.name.as_str()).to_owned(),
            icon: cfg.icon_for(agent.name.as_str()).to_owned(),
            status: agent.status(),
            subtitle: agent.status_line().to_owned(),
            needs_update: agent.needs_update(),
        })
        .collect()
}

/// The five flags, in the hive's own vocabulary and the spec's own order.
///
/// All five are shown whether set or not, because a flag reading `no` **is**
/// something the wire reported: "this agent does not need a login" is an
/// answer, and hiding the false ones would make an absent row ambiguous
/// between "false" and "this build does not know about it".
///
/// `running` is the raw wire flag, not [`Status`]: the collapse is a
/// *rendering* precedence (a failed agent's row says `failed`, not `running`),
/// and flattening it into these five would lose the orthogonality the hive
/// documents (`hive-sh4re/src/container.rs:26-30`).
#[must_use]
pub(crate) fn flags_of(agent: &Agent) -> [(&'static str, bool); 5] {
    [
        ("Running", agent.row.running),
        ("Paused", agent.row.paused),
        ("Failed", agent.row.failed),
        ("Needs update", agent.row.needs_update),
        ("Needs login", agent.row.needs_login),
    ]
}

/// The detail pane's content for one agent, at one instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DetailModel {
    /// The pane's title — the display label, as the sidebar row is titled.
    pub(crate) title: String,
    /// The five flags, [`flags_of`]'s order.
    pub(crate) flags: [(&'static str, bool); 5],
    /// `(label, value)` for the reported facts, in a fixed order. A fact the
    /// wire did not report renders [`ABSENT`] rather than being dropped — the
    /// row is a statement that the hive was asked.
    pub(crate) facts: Vec<(&'static str, String)>,
    /// The agent's own page, as the hive states it. `None` when the hive's
    /// domain is unconfigured, which is exactly when the link would be dead.
    pub(crate) agent_page: Option<String>,
    /// The hive's forge, where the agent's config repo lives. `None` until a
    /// `Urls` answer has landed, and on a hive whose gateway publishes none.
    pub(crate) config_repo: Option<String>,
}

/// What a fact the hive did not report renders as.
///
/// An em dash rather than an empty string or a hidden row: the label is the
/// question, and a blank value answers it ("the hive reports none") where a
/// missing row would only raise it again.
pub(crate) const ABSENT: &str = "—";

/// Build the detail pane's model for one agent.
///
/// `now_unix` is a parameter rather than a `SystemTime::now()` inside, so the
/// age label is testable at a fixed instant — the same seam
/// `hytte_plugin_agents::view::PanelContext` uses.
///
/// Nothing here is derived, computed or guessed: every value is a field of
/// [`AgentStatusRow`](hytte_plugin_agents::hive::wire::AgentStatusRow) or of
/// the hive's own `Urls` answer, and the two that are *rendered* rather than
/// echoed — the status age and the model chip — keep the raw value beside them
/// (the timestamp) or on the hover (the full model id), which is this
/// workspace's standing idiom for a shortened string.
#[must_use]
pub(crate) fn detail_of(
    agent: &Agent,
    cfg: &AgentsConfig,
    urls: Option<&HiveUrls>,
    now_unix: i64,
) -> DetailModel {
    let row = &agent.row;
    let mut facts: Vec<(&'static str, String)> = Vec::with_capacity(6);
    facts.push(("Status", agent.status_line().to_owned()));
    facts.push(("Status set", status_set(row.status_set_at.as_deref(), now_unix)));
    facts.push(("Model", opt(row.active_model.as_deref())));
    facts.push(("Parent", opt(row.parent.as_deref())));
    facts.push(("Deployed", opt(row.deployed_sha.as_deref())));
    facts.push((
        "Project",
        cfg.project_for(agent.name.as_str())
            .map_or_else(|| ABSENT.to_owned(), str::to_owned),
    ));
    DetailModel {
        title: cfg.label_for(agent.name.as_str()).to_owned(),
        flags: flags_of(agent),
        facts,
        agent_page: agent_url(agent).map(str::to_owned),
        config_repo: urls
            .and_then(|u| u.forge.as_deref())
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_owned),
    }
}

/// A reported string, or [`ABSENT`]. Trimmed, because the hive's own values
/// occasionally arrive padded and a row reading `" "` is worse than one
/// reading `—`.
fn opt(raw: Option<&str>) -> String {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| ABSENT.to_owned(), str::to_owned)
}

/// `status_set_at` as "⟨raw RFC 3339⟩ (⟨age⟩)", the raw value first.
///
/// The raw timestamp leads because it is what the wire said and what an
/// operator would paste into a hive-side query; the age trails because it is
/// what a human reads. A timestamp this build cannot parse keeps its raw form
/// and simply loses the parenthetical — one missing age label, never a lost
/// row, which is the rule the field's own wire doc states.
#[must_use]
pub(crate) fn status_set(raw: Option<&str>, now_unix: i64) -> String {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return ABSENT.to_owned();
    };
    match parse_set_at(raw) {
        Some(then) => format!("{raw} ({})", age(now_unix, then)),
        None => raw.to_owned(),
    }
}

/// The sidebar row and detail status page for a hive that is not `Up`.
///
/// Returns `None` for [`Hive::Up`] — there is no placeholder then, however
/// empty the roster is; an `Up` hive with no agents gets its own "no agents"
/// wording from the caller, because "the hive has none" and "we could not ask"
/// are different sentences and a status surface that conflates them has failed
/// at its one job.
///
/// Every arm **names the socket path**, which is the P4 note's explicit
/// requirement: all three failure modes (no daemon, no `hive-admin` group, a
/// wire version this build refuses to guess at) start with "which socket did
/// you mean", and the reason strings the client hands up deliberately do not
/// repeat it — they are sized for a 320 px sidebar row in the shell.
#[must_use]
pub(crate) fn placeholder(hive: &Hive, socket: &str) -> Option<(String, String)> {
    let (title, reason) = match hive {
        Hive::Up { .. } => return None,
        Hive::Connecting => ("Connecting…".to_owned(), String::new()),
        Hive::Unreachable { reason } => ("Hive unreachable".to_owned(), reason.clone()),
        Hive::Error { reason } => ("Hive refused".to_owned(), reason.clone()),
        Hive::Incompatible(mismatch) => (
            "Hive too new".to_owned(),
            format!(
                "hive protocol v{}, this build speaks v{}",
                mismatch.theirs, mismatch.ours
            ),
        ),
    };
    let detail = if reason.is_empty() {
        socket.to_owned()
    } else {
        format!("{reason} ({socket})")
    };
    Some((title, detail))
}

/// Whether the sidebar already holds exactly these agents.
///
/// Set equality, order-insensitive, for the reason `plugins_tab`'s twin is:
/// a reorder alone must not tear the rows down, because a teardown costs the
/// selection and, collapsed, the pushed page. `known` comes from a map's keys
/// so it is duplicate-free, and the hive yields one row per agent.
#[must_use]
pub(crate) fn same_agent_set(known: &[String], listed: &[String]) -> bool {
    known.len() == listed.len() && listed.iter().all(|name| known.contains(name))
}

// ── The widget tree ──────────────────────────────────────────────────────────

/// Which of the sidebar's two shapes is on screen, gating rebuild vs. in-place
/// update. The Plugins tab's `PluginsView`, with its `Empty` and `Unavailable`
/// collapsed into one `Placeholder` — here they are the same widget and differ
/// only in wording, which [`placeholder`] owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentsView {
    /// Nothing applied yet.
    Uninit,
    /// Real agent rows.
    List,
    /// A single non-selectable informational row.
    Placeholder,
}

/// One sidebar row's widgets.
#[derive(Clone)]
struct AgentRow {
    row: adw::ActionRow,
    icon: gtk::Image,
    badge: gtk::Image,
    status: gtk::Image,
}

/// One flag row and the suffix label that carries its `yes`/`no`.
#[derive(Clone)]
struct FlagRow {
    row: adw::ActionRow,
    value: gtk::Label,
}

/// The one detail pane's widgets.
#[derive(Clone)]
struct AgentDetail {
    page: adw::NavigationPage,
    stack: gtk::Stack,
    /// The status page shown when nothing is selected — its title and
    /// description carry [`placeholder`]'s two strings.
    empty: adw::StatusPage,
    flags: Vec<FlagRow>,
    facts: Vec<adw::ActionRow>,
    agent_page: adw::ActionRow,
    config_repo: adw::ActionRow,
}

/// The tab's state. The Plugins tab's shape, minus everything that exists
/// there to protect a **write** — there is no switch, so no `PendingToggle`,
/// no `syncing` guard and no toggle-result path.
#[derive(Clone)]
struct AgentsState {
    split: adw::NavigationSplitView,
    list: gtk::ListBox,
    detail: AgentDetail,
    /// `agents.toml`, read once at build. A change wants a restart, the same
    /// contract the plugin has.
    cfg: Rc<AgentsConfig>,
    /// Every child currently in `list`, for teardown before a rebuild.
    rows: Rc<RefCell<Vec<gtk::Widget>>>,
    /// The agent rows keyed by the hive's own name, for the in-place update.
    by_name: Rc<RefCell<HashMap<String, AgentRow>>>,
    /// The last poll's roster, so a selection change between polls renders
    /// from a snapshot rather than re-dialling.
    snapshot: Rc<RefCell<Hive>>,
    /// The hive's `Urls` answer, once it has landed.
    urls: Rc<RefCell<Option<HiveUrls>>>,
    /// The selected agent's name, or `None` for the empty state.
    selected: Rc<RefCell<Option<String>>>,
    /// The selection held over a placeholder, restored by the next good poll —
    /// `plugins_tab`'s `ParkedSelection` without the pending-toggle half.
    parked: Rc<RefCell<Option<ParkedSelection>>>,
    /// What is on screen, gating rebuild vs. in-place update.
    view: Rc<Cell<AgentsView>>,
    /// Guard so a *programmatic* selection — restoring one after a rebuild, or
    /// the `row-selected(None)` that removing rows emits — does not run the
    /// user-driven path.
    selecting: Rc<Cell<bool>>,
    /// Whether a `Urls` round trip is still worth making.
    urls_wanted: Rc<Cell<bool>>,
    /// The companion window's resolve-once probe. **Resolved before an action
    /// is taken, never after** — see [`open_agent_page`].
    probe: Rc<RefCell<agent_window::Probe>>,
}

/// The selection a placeholder set aside, for the next good poll to restore.
///
/// An unreachable hive says nothing about which agents exist, so the name is
/// kept rather than dropped; `pushed` remembers whether the user had drilled
/// in, because a failed poll is not them navigating out.
#[derive(Clone, Debug)]
struct ParkedSelection {
    name: String,
    pushed: bool,
}

/// [`AgentsState`] with its widget handles held **weakly** — what the tab's
/// handlers capture, and the reason they do not leak the tab.
///
/// GTK owns a signal handler for as long as it owns the widget it is connected
/// to, so a handler capturing a strong state closes a cycle `list` → handler →
/// state → `list` that nothing breaks. This is `plugins_tab`'s
/// `WeakPluginsState` convention, applied for the same reason; the `Rc` cells
/// are cloned strongly because none of them refers to an *ancestor* of a
/// widget carrying a handler.
#[derive(Clone)]
struct WeakAgentsState {
    split: glib::WeakRef<adw::NavigationSplitView>,
    list: glib::WeakRef<gtk::ListBox>,
    detail: WeakAgentDetail,
    cfg: Rc<AgentsConfig>,
    rows: Rc<RefCell<Vec<gtk::Widget>>>,
    by_name: Rc<RefCell<HashMap<String, AgentRow>>>,
    snapshot: Rc<RefCell<Hive>>,
    urls: Rc<RefCell<Option<HiveUrls>>>,
    selected: Rc<RefCell<Option<String>>>,
    parked: Rc<RefCell<Option<ParkedSelection>>>,
    view: Rc<Cell<AgentsView>>,
    selecting: Rc<Cell<bool>>,
    urls_wanted: Rc<Cell<bool>>,
    probe: Rc<RefCell<agent_window::Probe>>,
}

/// [`FlagRow`], weakly — see [`WeakAgentsState`].
#[derive(Clone)]
struct WeakFlagRow {
    row: glib::WeakRef<adw::ActionRow>,
    value: glib::WeakRef<gtk::Label>,
}

/// [`AgentDetail`]'s widgets, weakly — see [`WeakAgentsState`].
#[derive(Clone)]
struct WeakAgentDetail {
    page: glib::WeakRef<adw::NavigationPage>,
    stack: glib::WeakRef<gtk::Stack>,
    empty: glib::WeakRef<adw::StatusPage>,
    flags: Vec<WeakFlagRow>,
    facts: Vec<glib::WeakRef<adw::ActionRow>>,
    agent_page: glib::WeakRef<adw::ActionRow>,
    config_repo: glib::WeakRef<adw::ActionRow>,
}

impl AgentsState {
    /// The handler-side view of this state.
    fn downgrade(&self) -> WeakAgentsState {
        WeakAgentsState {
            split: self.split.downgrade(),
            list: self.list.downgrade(),
            detail: WeakAgentDetail {
                page: self.detail.page.downgrade(),
                stack: self.detail.stack.downgrade(),
                empty: self.detail.empty.downgrade(),
                flags: self
                    .detail
                    .flags
                    .iter()
                    .map(|f| WeakFlagRow {
                        row: f.row.downgrade(),
                        value: f.value.downgrade(),
                    })
                    .collect(),
                facts: self.detail.facts.iter().map(|r| r.downgrade()).collect(),
                agent_page: self.detail.agent_page.downgrade(),
                config_repo: self.detail.config_repo.downgrade(),
            },
            cfg: self.cfg.clone(),
            rows: self.rows.clone(),
            by_name: self.by_name.clone(),
            snapshot: self.snapshot.clone(),
            urls: self.urls.clone(),
            selected: self.selected.clone(),
            parked: self.parked.clone(),
            view: self.view.clone(),
            selecting: self.selecting.clone(),
            urls_wanted: self.urls_wanted.clone(),
            probe: self.probe.clone(),
        }
    }
}

impl WeakAgentsState {
    /// Rebuild the strong state for one callback, or `None` once the tab has
    /// been dropped. All-or-nothing: the widgets live and die as one tree, so
    /// a partial upgrade would mean a torn tab, not a case worth handling.
    fn upgrade(&self) -> Option<AgentsState> {
        Some(AgentsState {
            split: self.split.upgrade()?,
            list: self.list.upgrade()?,
            detail: AgentDetail {
                page: self.detail.page.upgrade()?,
                stack: self.detail.stack.upgrade()?,
                empty: self.detail.empty.upgrade()?,
                flags: self
                    .detail
                    .flags
                    .iter()
                    .map(|f| {
                        Some(FlagRow {
                            row: f.row.upgrade()?,
                            value: f.value.upgrade()?,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
                facts: self
                    .detail
                    .facts
                    .iter()
                    .map(glib::WeakRef::upgrade)
                    .collect::<Option<Vec<_>>>()?,
                agent_page: self.detail.agent_page.upgrade()?,
                config_repo: self.detail.config_repo.upgrade()?,
            },
            cfg: self.cfg.clone(),
            rows: self.rows.clone(),
            by_name: self.by_name.clone(),
            snapshot: self.snapshot.clone(),
            urls: self.urls.clone(),
            selected: self.selected.clone(),
            parked: self.parked.clone(),
            view: self.view.clone(),
            selecting: self.selecting.clone(),
            urls_wanted: self.urls_wanted.clone(),
            probe: self.probe.clone(),
        })
    }
}

/// Build the real **Agents** tab and start its poll.
///
/// Returns the tab's root widget and the poll `SourceId`; the caller ties the
/// latter to the window so the timer dies with it (#542) rather than dialling
/// `host.sock` forever after the window closes.
pub(crate) fn build_page() -> (adw::BreakpointBin, glib::SourceId) {
    // `agents.toml` through `hytte-config`'s XDG search path — the plugin's own
    // loader, so the tab and the sidebar cannot disagree about which hive is
    // configured or how often to ask it.
    let (bin, state) = build_tab(hytte_plugin_agents::config::load());
    let interval = state.cfg.poll_interval();
    refresh(&state);
    let poll = {
        let state = state.clone();
        glib::timeout_add_local(interval, move || {
            refresh(&state);
            glib::ControlFlow::Continue
        })
    };
    (bin, poll)
}

/// The widget tree and its state, with no socket traffic and no timer.
///
/// Split out of [`build_page`] the way `plugins_tab::build_tab` is, and for
/// the same reason: the GTK tests drive the layout and the apply path with
/// fabricated snapshots, which is the only way to test either — a test process
/// has no hive to answer `AgentStatus`.
///
/// `cfg` is a parameter rather than a `config::load()` inside, so **no test
/// ever reads the real `$XDG_CONFIG_HOME`** (#1101).
fn build_tab(cfg: AgentsConfig) -> (adw::BreakpointBin, AgentsState) {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("navigation-sidebar");

    // `Automatic`, not `Never`: a `Never` horizontal policy would make the
    // scroller's minimum width its child's and push the split view's minimum
    // past the bin's floor — the #856 warning on every collapsed allocation.
    let list_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&list)
        .build();

    let sidebar_toolbar = adw::ToolbarView::new();
    sidebar_toolbar.add_top_bar(&crate::plugins_tab::tab_header_bar());
    sidebar_toolbar.set_content(Some(&list_scroller));
    let sidebar_page = adw::NavigationPage::new(&sidebar_toolbar, "Agents");

    let detail = build_detail();

    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&sidebar_page));
    split.set_content(Some(&detail.page));
    split.set_min_sidebar_width(SIDEBAR_MIN_PX);
    split.set_max_sidebar_width(SIDEBAR_MAX_PX);
    split.set_sidebar_width_fraction(SIDEBAR_FRACTION);

    let bin = adw::BreakpointBin::new();
    bin.set_size_request(BIN_MIN_WIDTH_PX, BIN_MIN_HEIGHT_PX);
    bin.set_child(Some(&split));

    let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        COLLAPSE_WIDTH_PX,
        adw::LengthUnit::Px,
    ));
    breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
    bin.add_breakpoint(breakpoint);

    let state = AgentsState {
        split,
        list,
        detail,
        cfg: Rc::new(cfg),
        rows: Rc::new(RefCell::new(Vec::new())),
        by_name: Rc::new(RefCell::new(HashMap::new())),
        snapshot: Rc::new(RefCell::new(Hive::Connecting)),
        urls: Rc::new(RefCell::new(None)),
        selected: Rc::new(RefCell::new(None)),
        parked: Rc::new(RefCell::new(None)),
        view: Rc::new(Cell::new(AgentsView::Uninit)),
        selecting: Rc::new(Cell::new(false)),
        urls_wanted: Rc::new(Cell::new(true)),
        probe: Rc::new(RefCell::new(agent_window::Probe::path())),
    };

    connect_selection(&state);
    connect_links(&state);
    // Seed the sidebar and the pane from the `Connecting` snapshot, so the tab
    // mounts with a state rather than as a blank pane beside a blank list.
    apply(&state);

    (bin, state)
}

/// Build the detail pane once: a status page and the per-agent rows, in a
/// stack under a header bar that grows a back button when collapsed.
fn build_detail() -> AgentDetail {
    let empty = adw::StatusPage::builder()
        .icon_name("system-run-symbolic")
        .title("No agent selected")
        .description(
            "Agents are hyperhive containers this hive manages. Pick one to see what the hive \
             reports about it. This tab is read-only: an agent's settings are a change to its \
             config repo, reviewed and approved hive-side.",
        )
        .build();

    let flags_group = adw::PreferencesGroup::builder()
        .title("Flags")
        .description("The hive's own per-agent flags, as reported. They are independent, not a state machine.")
        .build();
    let flags: Vec<FlagRow> = flags_of_labels()
        .iter()
        .map(|label| {
            let row = adw::ActionRow::builder().title(*label).build();
            let value = flag_value_label();
            row.add_suffix(&value);
            flags_group.add(&row);
            FlagRow { row, value }
        })
        .collect();

    let facts_group = adw::PreferencesGroup::builder()
        .title("Reported")
        .description("What the hive says about this agent, and nothing derived from it.")
        .build();
    let facts: Vec<adw::ActionRow> = FACT_LABELS
        .iter()
        .map(|label| {
            let row = adw::ActionRow::builder()
                .title(*label)
                .subtitle(ABSENT)
                .build();
            // The harness's status text and a full model id both overrun a
            // 500 px pane; wrapping them is what keeps the pane a pane.
            row.set_subtitle_lines(2);
            facts_group.add(&row);
            row
        })
        .collect();

    let links_group = adw::PreferencesGroup::builder()
        .title("Links")
        .description("Opened outside this window: the agent's own page, and the forge the hive publishes.")
        .build();
    let agent_page = link_row("Agent page", "go-next-symbolic");
    let config_repo = link_row("Config repo", "go-next-symbolic");
    links_group.add(&agent_page);
    links_group.add(&config_repo);

    let agent_page_view = adw::PreferencesPage::new();
    agent_page_view.add(&flags_group);
    agent_page_view.add(&facts_group);
    agent_page_view.add(&links_group);

    let stack = gtk::Stack::new();
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&agent_page_view, Some("agent"));

    let toolbar = adw::ToolbarView::new();
    // No explicit back button: inside a collapsed `AdwNavigationSplitView` the
    // header bar is in a navigation stack and `AdwHeaderBar` grows one itself.
    toolbar.add_top_bar(&crate::plugins_tab::tab_header_bar());
    toolbar.set_content(Some(&stack));

    let page = adw::NavigationPage::new(&toolbar, "Agent");
    AgentDetail {
        page,
        stack,
        empty,
        flags,
        facts,
        agent_page,
        config_repo,
    }
}

/// The five flag labels, in [`flags_of`]'s order — the one place the detail
/// pane's row order is stated, so the built rows and the model cannot drift.
fn flags_of_labels() -> [&'static str; 5] {
    ["Running", "Paused", "Failed", "Needs update", "Needs login"]
}

/// The fact rows' labels, in [`detail_of`]'s order — same contract as
/// [`flags_of_labels`].
const FACT_LABELS: [&str; 6] = [
    "Status",
    "Status set",
    "Model",
    "Parent",
    "Deployed",
    "Project",
];

/// A flag row's `yes`/`no` suffix, styled once here rather than at each of the
/// five call sites.
fn flag_value_label() -> gtk::Label {
    let label = gtk::Label::builder().valign(gtk::Align::Center).build();
    label.add_css_class("caption");
    label
}

/// A link row: activatable, with a chevron, and starting insensitive because
/// no agent is selected yet.
fn link_row(title: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(ABSENT)
        .activatable(true)
        .sensitive(false)
        .build();
    let chevron = gtk::Image::from_icon_name(icon);
    chevron.set_valign(gtk::Align::Center);
    row.add_suffix(&chevron);
    row
}

/// Wire drill-down: `row-selected` retargets the detail pane (all a wide
/// layout needs, and what keyboard arrows drive); `row-activated` additionally
/// shows the content, which is a push when collapsed.
fn connect_selection(state: &AgentsState) {
    {
        let weak = state.downgrade();
        state.list.connect_row_selected(move |_, row| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if state.selecting.get() {
                return;
            }
            let name = row.and_then(|row| name_for_row(&state, row));
            *state.selected.borrow_mut() = name;
            refresh_detail(&state);
        });
    }
    {
        let weak = state.downgrade();
        state.list.connect_row_activated(move |_, row| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if let Some(name) = name_for_row(&state, row) {
                *state.selected.borrow_mut() = Some(name);
                refresh_detail(&state);
                state.split.set_show_content(true);
            }
        });
    }
}

/// Wire the two link rows. Both read the **current** selection out of the
/// snapshot rather than capturing a URL, so a row whose agent has since
/// vanished opens nothing instead of a stale destination.
fn connect_links(state: &AgentsState) {
    {
        let weak = state.downgrade();
        state.detail.agent_page.connect_activated(move |_| {
            if let Some(state) = weak.upgrade() {
                open_agent_page(&state);
            }
        });
    }
    {
        let weak = state.downgrade();
        state.detail.config_repo.connect_activated(move |_| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let uri = state
                .urls
                .borrow()
                .as_ref()
                .and_then(|u| u.forge.clone())
                .map(|u| u.trim().to_owned())
                .filter(|u| !u.is_empty());
            if let Some(uri) = uri {
                open_uri(&uri);
            }
        });
    }
}

/// Open the selected agent's surface: the companion window when it resolves on
/// `PATH`, the browser otherwise.
///
/// **The route is chosen before anything is launched**, by resolving the
/// binary — the same rule `hytte_plugin_agents::window` documents for the
/// plugin, and it holds here for a related reason: a `gio::Subprocess` spawn
/// does report `ENOENT`, but by then the browser fallback would be a *second*
/// action taken after a visible failure rather than the only one taken. One
/// click, one destination.
///
/// The probe caches, and complains exactly once, so a desktop without the
/// window does not log a line per click.
fn open_agent_page(state: &AgentsState) {
    let Some(name) = state.selected.borrow().clone() else {
        return;
    };
    let snapshot = state.snapshot.borrow();
    let Some(agent) = AgentName::parse(&name).and_then(|n| snapshot.agent(&n)) else {
        return;
    };
    let url = agent_url(agent).map(str::to_owned);
    drop(snapshot);

    if state.probe.borrow_mut().available() {
        let argv = agent_window::argv(&name, agent_window::Tab::Agent);
        launch(&argv);
        return;
    }
    if let Some(url) = url {
        open_uri(&url);
    }
}

/// Launch `argv` detached from this process.
///
/// `gio::Subprocess` rather than `std::process::Command` because GIO reaps the
/// child itself — a companion window the operator closes must not leave a
/// zombie parented to a settings app that may outlive it by hours — and
/// because it does not kill the child when this window goes away.
fn launch(argv: &[String]) {
    let args: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
    match gtk::gio::Subprocess::newv(&args, gtk::gio::SubprocessFlags::NONE) {
        Ok(_child) => tracing::info!(?argv, "launched the agent companion window"),
        Err(e) => tracing::warn!(?argv, error = %e, "could not launch the agent companion window"),
    }
}

/// Hand a URI to the desktop's default handler.
///
/// `GtkUriLauncher` rather than `gio::AppInfo::launch_default_for_uri` so the
/// call goes through the portal where one is in force, which is what makes it
/// work from a sandboxed install as well as a native one.
fn open_uri(uri: &str) {
    gtk::UriLauncher::new(uri).launch(
        None::<&gtk::Window>,
        gtk::gio::Cancellable::NONE,
        |result| {
            if let Err(e) = result {
                tracing::warn!(error = %e, "could not open the link");
            }
        },
    );
}

/// The agent name behind a sidebar row, or `None` for the placeholder row.
fn name_for_row(state: &AgentsState, row: &gtk::ListBoxRow) -> Option<String> {
    state
        .by_name
        .borrow()
        .iter()
        .find(|(_, arow)| arow.row.upcast_ref::<gtk::ListBoxRow>() == row)
        .map(|(name, _)| name.clone())
}

// ── The poll ─────────────────────────────────────────────────────────────────

/// One tick: ask for the roster, and — while the hive is up and has not
/// answered yet — for its URLs.
fn refresh(state: &AgentsState) {
    let socket = PathBuf::from(&state.cfg.socket);
    {
        let weak = state.downgrade();
        spawn_on_runtime(
            async move { client::request(&socket, &Request::AgentStatus).await },
            move |answer| {
                if let Some(state) = weak.upgrade() {
                    on_status(&state, &answer);
                }
            },
        );
    }
    refresh_urls(state);
}

/// Ask for the hive's URLs, at most once successfully, and **only while the
/// roster poll is succeeding**.
///
/// The gate is what keeps a down hive from doubling its own failed traffic:
/// the `Urls` answer never changes under a running window, so the only thing a
/// retry-while-down buys is a second timeout per tick. Riding the roster's own
/// success instead means the fetch resumes the moment the hive comes back,
/// with no backoff machinery of its own — the seed `Connecting` state counts
/// as "worth one attempt", so a hive that is up at open answers on tick one.
fn refresh_urls(state: &AgentsState) {
    if !state.urls_wanted.get() {
        return;
    }
    if matches!(
        &*state.snapshot.borrow(),
        Hive::Unreachable { .. } | Hive::Error { .. } | Hive::Incompatible(_)
    ) {
        return;
    }
    let socket = PathBuf::from(&state.cfg.socket);
    let weak = state.downgrade();
    spawn_on_runtime(
        async move { client::request(&socket, &Request::Urls).await },
        move |answer| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if let Ok(urls) = answer
                && let Some(urls) = urls.urls
            {
                state.urls_wanted.set(false);
                *state.urls.borrow_mut() = Some(urls);
                refresh_detail(&state);
            }
        },
    );
}

/// Fold one roster answer into the snapshot and repaint.
///
/// Public to the crate's tests rather than private, so the GTK tests drive the
/// exact path a real poll does instead of a lookalike.
fn on_status(state: &AgentsState, answer: &Result<Response, HiveError>) {
    *state.snapshot.borrow_mut() = hive_of(answer);
    apply(state);
}

// ── Applying a snapshot ──────────────────────────────────────────────────────

/// Render the current snapshot: rows in place when the roster's membership is
/// unchanged, a rebuild when it is not, a placeholder when the hive is not up.
fn apply(state: &AgentsState) {
    let snapshot = state.snapshot.borrow().clone();
    let Some(models) = up_rows(state, &snapshot) else {
        return;
    };

    let known: Vec<String> = state.by_name.borrow().keys().cloned().collect();
    let listed: Vec<String> = models.iter().map(|m| m.name.clone()).collect();
    let same_set = state.view.get() == AgentsView::List && same_agent_set(&known, &listed);

    if same_set {
        for model in &models {
            // Clone the handle out and let the borrow end at the `let`: the
            // setters below can drive GTK synchronously back into a handler
            // that re-enters `by_name`, and a `BorrowMutError` inside a glib
            // callback aborts the process rather than failing gracefully.
            let row = state.by_name.borrow().get(&model.name).cloned();
            if let Some(row) = row {
                update_agent_row(&row, model);
            }
        }
        refresh_detail(state);
        return;
    }

    // A structural change (or the first load). `parked` is the selection a
    // placeholder set aside; take it either way, because once real rows are
    // back it has either been restored or been proven gone. A live `selected`
    // wins — that path never lost its page.
    let parked = state.parked.take();
    let previously = state.selected.borrow().clone();
    let (wanted, restore_push) = match (previously, parked) {
        (Some(name), _) => (Some(name), false),
        (None, Some(p)) => (Some(p.name), p.pushed),
        (None, None) => (None, false),
    };

    clear_rows(state);
    for model in &models {
        let row = build_agent_row(model);
        state.list.append(&row.row);
        state.rows.borrow_mut().push(row.row.clone().upcast());
        // Bound, not a bare statement: `insert` returns the displaced entry,
        // and as a temporary it would drop GTK widgets *inside* the borrow.
        let displaced = state.by_name.borrow_mut().insert(model.name.clone(), row);
        drop(displaced);
    }
    state.view.set(AgentsView::List);

    match wanted {
        Some(name) if state.by_name.borrow().contains_key(&name) => {
            select_silently(state, &name);
            if restore_push {
                state.split.set_show_content(true);
            }
        }
        // The selected agent is gone, or this is the first load. Pop first —
        // the user asked to see *that* agent, and swapping another in
        // underneath a pushed page would be a lie — then settle the sidebar on
        // the first remaining agent so a wide layout shows a pane rather than
        // an empty one beside a full list. `GtkListBox` in `Single` mode does
        // not auto-select an appended row, so this is not redundant.
        _ => {
            state.split.set_show_content(false);
            if let Some(first) = models.first() {
                select_silently(state, &first.name);
            }
        }
    }
    refresh_detail(state);
}

/// The rows for an `Up` hive, or `None` after rendering a placeholder for
/// every other state — including an `Up` hive with an empty roster, which gets
/// its own wording because "the hive has none" and "we could not ask" are
/// different sentences.
fn up_rows(state: &AgentsState, snapshot: &Hive) -> Option<Vec<RowModel>> {
    if let Some((title, detail)) = placeholder(snapshot, &state.cfg.socket) {
        set_placeholder(state, &title, &detail);
        return None;
    }
    let models = rows_of(snapshot, &state.cfg);
    if models.is_empty() {
        set_placeholder(
            state,
            "No agents",
            &format!("the hive at {} manages none", state.cfg.socket),
        );
        return None;
    }
    Some(models)
}

/// Show one non-selectable informational row plus the matching detail status
/// page, rebuilding only on a *transition* into the placeholder view so a
/// steady failing poll does not flicker it.
///
/// Entering the placeholder **parks** the selection: a hive that did not
/// answer says nothing about which agents exist, so the name is kept for
/// [`apply`] to restore. The park is taken before [`clear_selection`] wipes
/// it, and a repeated failure returns early below, so the first failure's park
/// is never overwritten with the `None` it left behind.
fn set_placeholder(state: &AgentsState, title: &str, detail: &str) {
    // Wording can change while the view does not (one client reason replacing
    // another), so the status page is refreshed unconditionally and only the
    // *row* rebuild is gated.
    state.detail.empty.set_title(title);
    state.detail.empty.set_description(Some(detail));

    if state.view.get() == AgentsView::Placeholder {
        // The one row is already there; keep its text current.
        let row = state.rows.borrow().first().cloned();
        if let Some(row) = row
            && let Ok(row) = row.downcast::<adw::ActionRow>()
        {
            row.set_title(title);
            row.set_subtitle(detail);
        }
        return;
    }

    let park = state.selected.borrow().clone().map(|name| ParkedSelection {
        pushed: state.split.shows_content(),
        name,
    });
    *state.parked.borrow_mut() = park;
    clear_rows(state);
    clear_selection(state);
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(detail)
        .activatable(false)
        .selectable(false)
        .build();
    row.set_subtitle_lines(3);
    state.list.append(&row);
    state.rows.borrow_mut().push(row.upcast());
    state.view.set(AgentsView::Placeholder);
}

/// Drop every sidebar row.
fn clear_rows(state: &AgentsState) {
    // Removing the selected row emits `row-selected(None)`; that is
    // bookkeeping, not the user deselecting, so it must not run the selection
    // path.
    state.selecting.set(true);
    // `take()`, not `borrow_mut().drain(..)`: the chained `RefMut` would stay
    // live across every `list.remove()`, which can re-enter these cells from a
    // synchronous handler — and a `BorrowMutError` inside a glib callback
    // aborts the process.
    for row in state.rows.take() {
        state.list.remove(&row);
    }
    drop(state.by_name.take());
    state.selecting.set(false);
}

/// Drop the selection entirely: no row selected, the status page in the detail
/// pane, and — the part that matters when collapsed — pop back to the list.
fn clear_selection(state: &AgentsState) {
    *state.selected.borrow_mut() = None;
    state.selecting.set(true);
    state.list.select_row(None::<&gtk::ListBoxRow>);
    state.selecting.set(false);
    state.split.set_show_content(false);
    state.detail.stack.set_visible_child_name("empty");
    state.detail.page.set_title("Agent");
}

/// Select `name`'s row without running the user-driven selection path — used
/// to restore a selection across a rebuild, where the caller refreshes the
/// detail pane itself and the navigation state must not move.
fn select_silently(state: &AgentsState, name: &str) {
    let row = state.by_name.borrow().get(name).map(|r| r.row.clone());
    let Some(row) = row else {
        return;
    };
    *state.selected.borrow_mut() = Some(name.to_owned());
    state.selecting.set(true);
    state
        .list
        .select_row(Some(row.upcast_ref::<gtk::ListBoxRow>()));
    state.selecting.set(false);
}

/// Retarget the one detail pane at the selected agent, from the last poll's
/// snapshot.
///
/// **Retarget, never rebuild**: the rows were built once in [`build_detail`]
/// and every poll only rewrites their text. That is what makes the cadence
/// invisible — a rebuilt pane would drop the scroll position and, collapsed,
/// the pushed page — and it is why the flag and fact rows are held as ordered
/// `Vec`s rather than looked up by title.
fn refresh_detail(state: &AgentsState) {
    let selected = state.selected.borrow().clone();
    let Some(name) = selected else {
        state.detail.stack.set_visible_child_name("empty");
        state.detail.page.set_title("Agent");
        return;
    };
    let snapshot = state.snapshot.borrow().clone();
    let Some(agent) = AgentName::parse(&name).and_then(|n| snapshot.agent(&n).cloned()) else {
        // The selection's agent has gone: fall back rather than show stale
        // rows for an agent the hive no longer reports.
        clear_selection(state);
        return;
    };

    let model = detail_of(&agent, &state.cfg, state.urls.borrow().as_ref(), now_unix());
    state.detail.page.set_title(&model.title);
    for (row, (_, set)) in state.detail.flags.iter().zip(model.flags) {
        set_flag_row(row, set);
    }
    for (row, (_, value)) in state.detail.facts.iter().zip(&model.facts) {
        row.set_subtitle(value);
    }
    set_link_row(&state.detail.agent_page, model.agent_page.as_deref());
    set_link_row(&state.detail.config_repo, model.config_repo.as_deref());
    state.detail.stack.set_visible_child_name("agent");
}

/// Drive one flag row's suffix label.
///
/// The label is held beside its row in [`FlagRow`] rather than looked up:
/// `AdwActionRow` exposes no accessor for what `add_suffix` took, and walking
/// the row's widget tree for "the label that is not the title or the subtitle"
/// would make a libadwaita internal a load-bearing assumption of this file.
fn set_flag_row(flag: &FlagRow, set: bool) {
    flag.value.set_text(if set { "yes" } else { "no" });
    // `accent`, not `success`: four of the five flags are *not* good news when
    // set, and a green "yes" beside `Failed` would read as a verdict.
    flag.value.remove_css_class("accent");
    flag.value.remove_css_class("dim-label");
    flag.value
        .add_css_class(if set { "accent" } else { "dim-label" });
}

/// Drive one link row: the destination as its subtitle, insensitive when there
/// is none.
///
/// Insensitive rather than hidden, for the reason a fact row shows [`ABSENT`]
/// rather than disappearing: "the hive publishes no forge" is an answer, and a
/// row that vanishes only raises the question again.
fn set_link_row(row: &adw::ActionRow, uri: Option<&str>) {
    match uri {
        Some(uri) => {
            row.set_subtitle(uri);
            row.set_sensitive(true);
        }
        None => {
            row.set_subtitle(ABSENT);
            row.set_sensitive(false);
        }
    }
}

/// Build one sidebar row from its model.
///
/// No per-row handler — drill-down is the list's `row-selected` /
/// `row-activated`, so a row is a display of one agent and nothing else.
fn build_agent_row(model: &RowModel) -> AgentRow {
    let row = adw::ActionRow::builder().activatable(true).build();

    let icon = gtk::Image::new();
    icon.set_valign(gtk::Align::Center);
    row.add_prefix(&icon);

    let badge = gtk::Image::from_icon_name(hytte_plugin_agents::model::UPDATE_BADGE_ICON);
    badge.set_valign(gtk::Align::Center);
    badge.add_css_class("warning");
    badge.set_tooltip_text(Some("a rebuild would change this agent's locked rev"));
    row.add_suffix(&badge);

    let status = gtk::Image::new();
    status.set_valign(gtk::Align::Center);
    row.add_suffix(&status);

    let arow = AgentRow {
        row,
        icon,
        badge,
        status,
    };
    update_agent_row(&arow, model);
    arow
}

/// Reflect a model into an existing sidebar row — the in-place update that
/// makes the poll invisible.
fn update_agent_row(row: &AgentRow, model: &RowModel) {
    row.row.set_title(&model.title);
    row.row.set_subtitle(&model.subtitle);
    row.icon.set_icon_name(Some(&model.icon));
    row.badge.set_visible(model.needs_update);
    row.status.set_icon_name(Some(model.status.icon()));
    row.status.set_tooltip_text(Some(model.status.text()));
    for class in ["error", "warning", "accent", "dim-label"] {
        row.status.remove_css_class(class);
    }
    row.status.add_css_class(model.status.class());
}

/// Now, in unix seconds. Saturating rather than panicking on a clock before
/// the epoch: a wrong age label is not worth aborting a settings app over.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::{
        ABSENT, DetailModel, FACT_LABELS, RowModel, detail_of, flags_of, flags_of_labels, hive_of,
        ordered, placeholder, rows_of, same_agent_set, status_set,
    };
    use hytte_plugin_agents::config::{AgentsConfig, Display};
    use hytte_plugin_agents::hive::client::HiveError;
    use hytte_plugin_agents::hive::wire::{
        AgentStatusRow, HOST_SOCK_VERSION, HiveUrls, Response, VersionMismatch,
    };
    use hytte_plugin_agents::model::{Agent, AgentName, Hive, Status};

    /// The `status_set_at` the plugin's own fixtures carry.
    const SET_AT: &str = "2026-09-07T12:34:56Z";

    /// A fixed instant for every age assertion: one hour after [`SET_AT`].
    /// Derived through the same parser the renderer uses rather than written
    /// as an epoch literal, so the two cannot disagree about the timezone.
    fn now() -> i64 {
        hytte_plugin_agents::view::parse_set_at(SET_AT).expect("the fixture timestamp parses")
            + 3600
    }

    fn row(name: &str) -> AgentStatusRow {
        AgentStatusRow {
            name: name.to_owned(),
            running: true,
            ..AgentStatusRow::default()
        }
    }

    fn agent(row: AgentStatusRow) -> Agent {
        Agent {
            name: AgentName::parse(&row.name).expect("a legal test name"),
            row,
            pending_paused: None,
        }
    }

    fn up(rows: Vec<AgentStatusRow>) -> Hive {
        Hive::Up {
            agents: rows.into_iter().map(agent).collect(),
        }
    }

    fn answer(rows: Vec<AgentStatusRow>) -> Response {
        Response {
            version: HOST_SOCK_VERSION,
            ok: true,
            agent_statuses: Some(rows),
            ..Response::default()
        }
    }

    /// A config that groups `named` under `project` and leaves the rest
    /// ungrouped. Built by hand, never loaded — no test here reads the real
    /// `$XDG_CONFIG_HOME` (#1101).
    fn cfg_with(projects: &[(&str, &str)]) -> AgentsConfig {
        let mut cfg = AgentsConfig::default();
        for (agent, project) in projects {
            cfg.display.insert(
                (*agent).to_owned(),
                Display {
                    label: None,
                    icon: None,
                    project: Some((*project).to_owned()),
                },
            );
        }
        cfg
    }

    // ── the order default (the one the P4 note put up for veto) ─────────────

    /// The tab lists **the sidebar's order**, which is `model::group`'s:
    /// named projects alphabetically, the ungrouped bucket last, and the
    /// hive's own order within a group.
    ///
    /// Mutation (run, verified red): return `hive.agents().iter().collect()`
    /// from `ordered` — i.e. the hive's raw order — and this reds on the very
    /// first element, because `zed` (project `alpha`) must precede `abe`
    /// (project `beta`) and both must precede the ungrouped `mid`.
    #[test]
    fn the_sidebar_order_is_the_plugins_grouping_not_the_wire_order() {
        let hive = up(vec![row("mid"), row("abe"), row("zed")]);
        let cfg = cfg_with(&[("zed", "alpha"), ("abe", "beta")]);
        let names: Vec<&str> = ordered(&hive, &cfg)
            .into_iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names, ["zed", "abe", "mid"]);
    }

    /// With no projects configured every agent is ungrouped, and the hive's
    /// own order survives intact — the common case, and the one a grouping
    /// that quietly sorted by name would break.
    ///
    /// Mutation (run, verified red): sort the ungrouped bucket by name in
    /// `model::group` and this reds.
    #[test]
    fn an_unconfigured_roster_keeps_the_hives_own_order() {
        let hive = up(vec![row("zed"), row("abe"), row("mid")]);
        let names: Vec<String> = rows_of(&hive, &AgentsConfig::default())
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, ["zed", "abe", "mid"]);
    }

    // ── folding one wire answer ─────────────────────────────────────────────

    /// A hive that **answered** — with `ok: false`, or with a line this build
    /// cannot parse — is up. Calling that "unreachable" would send the
    /// operator to `systemctl` for a problem that is not there.
    ///
    /// Mutation (run, verified red): merge the `Refused`/`Protocol` arm into
    /// the catch-all `Err` arm and both `Error` assertions red.
    #[test]
    fn a_hive_that_answered_badly_is_an_error_not_unreachable() {
        assert!(matches!(
            hive_of(&Err(HiveError::Refused {
                reason: "no such agent".to_owned()
            })),
            Hive::Error { .. }
        ));
        assert!(matches!(
            hive_of(&Err(HiveError::Protocol {
                reason: "unparseable".to_owned()
            })),
            Hive::Error { .. }
        ));
        assert!(matches!(
            hive_of(&Err(HiveError::Unreachable {
                reason: "no socket".to_owned()
            })),
            Hive::Unreachable { .. }
        ));
        assert!(matches!(
            hive_of(&Err(HiveError::Version(VersionMismatch {
                theirs: 9,
                ours: HOST_SOCK_VERSION,
            }))),
            Hive::Incompatible(_)
        ));
    }

    /// A row whose name fails the whitelist is dropped, not rendered — the
    /// name is what a companion-window launch passes as `--agent`.
    #[test]
    fn an_illegal_name_costs_its_own_row_and_no_other() {
        let bad = AgentStatusRow {
            name: "../../etc/passwd".to_owned(),
            ..AgentStatusRow::default()
        };
        let hive = hive_of(&Ok(answer(vec![bad, row("argus")])));
        let names: Vec<&str> = hive.agents().iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["argus"]);
    }

    // ── the unreachable state names the socket ──────────────────────────────

    /// Every non-`Up` state renders, and every one of them **names the socket
    /// path** — the P4 note's explicit requirement, and the reason it is worth
    /// pinning is that the client's own reason strings deliberately omit it.
    ///
    /// Mutation (run, verified red): drop the `({socket})` suffix from
    /// `placeholder`'s `detail` and all four `contains` assertions red.
    #[test]
    fn every_unreachable_state_names_the_socket_path() {
        let socket = "/run/hyperhive/host.sock";
        for hive in [
            Hive::Connecting,
            Hive::Unreachable {
                reason: "no socket — hive-c0re is not running".to_owned(),
            },
            Hive::Error {
                reason: "agent \"ghost\" is not managed by this hive".to_owned(),
            },
            Hive::Incompatible(VersionMismatch {
                theirs: 9,
                ours: HOST_SOCK_VERSION,
            }),
        ] {
            let (title, detail) =
                placeholder(&hive, socket).expect("a non-Up hive always has a placeholder");
            assert!(!title.is_empty(), "{hive:?}");
            assert!(detail.contains(socket), "{hive:?} → {detail}");
        }
    }

    /// An `Up` hive has **no** placeholder, however empty its roster is: the
    /// caller's own "no agents" wording is a different sentence from "we could
    /// not ask", and conflating them is the one failure a status surface must
    /// not have.
    #[test]
    fn an_up_hive_has_no_unreachable_placeholder() {
        assert!(placeholder(&up(Vec::new()), "/run/x.sock").is_none());
        assert!(placeholder(&up(vec![row("argus")]), "/run/x.sock").is_none());
    }

    /// The operator-facing reason the client produced reaches the placeholder
    /// unreworded — this tab must not soften "needs `hive-admin` group" into
    /// something less actionable.
    #[test]
    fn the_clients_own_reason_survives_into_the_placeholder() {
        let reason = "permission denied — needs `hive-admin` group (re-login after adding)";
        let (_, detail) = placeholder(
            &Hive::Unreachable {
                reason: reason.to_owned(),
            },
            "/run/hyperhive/host.sock",
        )
        .expect("unreachable has a placeholder");
        assert!(detail.contains(reason), "{detail}");
    }

    // ── the row and detail mapping ──────────────────────────────────────────

    /// The sidebar row shows the config's label and icon, the collapsed status
    /// and the harness's own line — and keys on the **hive's** name, not the
    /// label, because that is what every request and every launch carries.
    #[test]
    fn a_row_is_labelled_by_config_and_keyed_by_the_hives_name() {
        let mut cfg = AgentsConfig::default();
        cfg.display.insert(
            "argus".to_owned(),
            Display {
                label: Some("Argus".to_owned()),
                icon: Some("face-smile-symbolic".to_owned()),
                project: None,
            },
        );
        let hive = up(vec![AgentStatusRow {
            status_text: Some("reviewing PR #947".to_owned()),
            ..row("argus")
        }]);
        let models = rows_of(&hive, &cfg);
        assert_eq!(
            models,
            vec![RowModel {
                name: "argus".to_owned(),
                title: "Argus".to_owned(),
                icon: "face-smile-symbolic".to_owned(),
                status: Status::Running,
                subtitle: "reviewing PR #947".to_owned(),
                needs_update: false,
            }]
        );
    }

    /// All five flags are shown whether set or not, in the spec's order, and
    /// `Running` is the **raw wire flag** rather than the collapsed status —
    /// a failed agent that is still running reports both.
    ///
    /// Mutation (run, verified red): derive `Running` from
    /// `agent.status() == Status::Running` and the `("Running", true)`
    /// assertion on the failed row goes false.
    #[test]
    fn the_flags_are_the_raw_wire_flags_not_the_collapsed_status() {
        let a = agent(AgentStatusRow {
            failed: true,
            running: true,
            needs_update: true,
            ..row("argus")
        });
        assert_eq!(a.status(), Status::Failed, "precedence still collapses");
        assert_eq!(
            flags_of(&a),
            [
                ("Running", true),
                ("Paused", false),
                ("Failed", true),
                ("Needs update", true),
                ("Needs login", false),
            ]
        );
    }

    /// The built rows and the model agree on order and count — the two lists
    /// are zipped, so a divergence would silently write each value into the
    /// wrong row.
    ///
    /// Mutation (run, verified red): swap two entries of `flags_of_labels`
    /// (or of `FACT_LABELS`) and the corresponding assertion reds.
    #[test]
    fn the_built_row_labels_match_the_models_order() {
        let a = agent(row("argus"));
        let flag_labels: Vec<&str> = flags_of(&a).iter().map(|(l, _)| *l).collect();
        assert_eq!(flag_labels, flags_of_labels().to_vec());

        let d = detail_of(&a, &AgentsConfig::default(), None, now());
        let fact_labels: Vec<&str> = d.facts.iter().map(|(l, _)| *l).collect();
        assert_eq!(fact_labels, FACT_LABELS.to_vec());
    }

    /// Every reported fact lands on its own labelled row, verbatim — nothing
    /// is derived, reordered or merged.
    ///
    /// Mutation (run, verified red): swap `parent` and `deployed_sha` in
    /// `detail_of` and the two assertions below red on each other's value.
    #[test]
    fn each_reported_fact_reaches_its_own_row() {
        let a = agent(AgentStatusRow {
            parent: Some("queen".to_owned()),
            deployed_sha: Some("0123456789ab".to_owned()),
            active_model: Some("claude-opus-4-6".to_owned()),
            status_text: Some("reviewing PR #947".to_owned()),
            status_set_at: Some("2026-09-07T12:34:56Z".to_owned()),
            ..row("argus")
        });
        let d = detail_of(&a, &cfg_with(&[("argus", "trollshell")]), None, now());
        let fact = |label: &str| {
            d.facts
                .iter()
                .find(|(l, _)| *l == label)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("no {label} row"))
        };
        assert_eq!(fact("Status"), "reviewing PR #947");
        assert_eq!(fact("Model"), "claude-opus-4-6");
        assert_eq!(fact("Parent"), "queen");
        assert_eq!(fact("Deployed"), "0123456789ab");
        assert_eq!(fact("Project"), "trollshell");
        assert!(fact("Status set").starts_with("2026-09-07T12:34:56Z"));
    }

    /// A fact the hive did not report renders [`ABSENT`] rather than being
    /// dropped: the label is the question, and a missing row would only raise
    /// it again.
    #[test]
    fn an_unreported_fact_is_a_dash_and_not_a_missing_row() {
        let d = detail_of(&agent(row("argus")), &AgentsConfig::default(), None, now());
        assert_eq!(d.facts.len(), FACT_LABELS.len());
        for label in ["Model", "Parent", "Deployed", "Status set", "Project"] {
            let (_, value) = d
                .facts
                .iter()
                .find(|(l, _)| *l == label)
                .expect("the row is present");
            assert_eq!(value, ABSENT, "{label}");
        }
    }

    /// `status_set_at` leads with the **raw** timestamp and trails the age, so
    /// the value an operator would paste hive-side is the one on screen.
    #[test]
    fn a_status_timestamp_keeps_its_raw_form_and_gains_an_age() {
        let s = status_set(Some(SET_AT), now());
        assert!(s.starts_with(SET_AT), "{s}");
        assert!(s.contains("1h ago"), "{s}");
    }

    /// An unparseable timestamp costs the **age label only** — the raw value
    /// still shows, and nothing else about the row moves. The field's own wire
    /// doc names this rule.
    ///
    /// Mutation (run, verified red): make `status_set` return `ABSENT` when
    /// `parse_set_at` fails and the first assertion reds.
    #[test]
    fn an_unparseable_timestamp_costs_the_age_and_not_the_row() {
        assert_eq!(status_set(Some("not a date"), now()), "not a date");
        assert_eq!(status_set(None, now()), ABSENT);
        assert_eq!(status_set(Some("   "), now()), ABSENT);
    }

    /// The two links come from the two places the wire states them: the
    /// agent's own `url` field, and the hive's `Urls` answer. Neither is
    /// derived from `domain`.
    ///
    /// Mutation (run, verified red): build `config_repo` from
    /// `urls.domain` and the `None`-forge assertion reds.
    #[test]
    fn the_links_are_stated_by_the_wire_and_never_derived() {
        let a = agent(AgentStatusRow {
            url: Some("https://hive.local/agent/argus/".to_owned()),
            ..row("argus")
        });
        let cfg = AgentsConfig::default();

        let no_urls = detail_of(&a, &cfg, None, now());
        assert_eq!(
            no_urls.agent_page.as_deref(),
            Some("https://hive.local/agent/argus/")
        );
        assert_eq!(no_urls.config_repo, None, "no Urls answer, no forge link");

        // A hive with a domain but no forge publishes no config repo, and the
        // row must stay empty rather than guess `forge.<domain>`.
        let domain_only = HiveUrls {
            domain: Some("hive.local".to_owned()),
            home: Some("https://hive.local/".to_owned()),
            forge: None,
        };
        assert_eq!(detail_of(&a, &cfg, Some(&domain_only), now()).config_repo, None);

        let with_forge = HiveUrls {
            forge: Some("  https://forge.hive.local/  ".to_owned()),
            ..domain_only
        };
        assert_eq!(
            detail_of(&a, &cfg, Some(&with_forge), now()).config_repo.as_deref(),
            Some("https://forge.hive.local/")
        );

        // An agent the hive publishes no URL for gets no link — the hive's
        // domain being unconfigured is exactly when one would be dead.
        let no_url = agent(row("argus"));
        assert_eq!(detail_of(&no_url, &cfg, Some(&with_forge), now()).agent_page, None);
    }

    /// The detail model is a pure function of its inputs: the same snapshot
    /// twice is the same model, which is what lets [`super::refresh_detail`]
    /// rewrite the same rows on every poll instead of rebuilding them.
    #[test]
    fn the_same_snapshot_twice_is_the_same_detail() {
        let a = agent(row("argus"));
        let cfg = AgentsConfig::default();
        let one: DetailModel = detail_of(&a, &cfg, None, now());
        let two: DetailModel = detail_of(&a, &cfg, None, now());
        assert_eq!(one, two);
    }

    // ── the rebuild predicate ───────────────────────────────────────────────

    /// A **reorder** is not a membership change: rebuilding on one would cost
    /// the selection and, collapsed, the pushed page, every time the hive
    /// shuffled its answer.
    ///
    /// Mutation (run, verified red): compare the two slices with `==` and the
    /// reorder assertion reds.
    #[test]
    fn a_reorder_is_not_a_membership_change() {
        let known = vec!["a".to_owned(), "b".to_owned()];
        assert!(same_agent_set(&known, &["b".to_owned(), "a".to_owned()]));
        assert!(!same_agent_set(&known, &["a".to_owned()]));
        assert!(!same_agent_set(
            &known,
            &["a".to_owned(), "b".to_owned(), "c".to_owned()]
        ));
        assert!(!same_agent_set(&known, &["a".to_owned(), "c".to_owned()]));
    }

    // ── the socket itself ───────────────────────────────────────────────────
    //
    // Deliberately **not** `system-tests`-gated, for the reason
    // `hytte-plugin-agents/tests/fake_socket.rs` states for its own: that
    // feature exists for tests needing a `dbus-daemon` or a display server,
    // and a `UnixListener` in a `TempDir` needs neither. Gating them would
    // mean the drift detector does not run in the hermetic bucket, which is
    // exactly the run where a wire change should go red.

    /// The tab's data path, end to end against a **real** unix socket: the
    /// client dials the path `agents.toml` names, writes exactly
    /// `{"cmd":"agent_status"}`, and the answer folds into the roster the
    /// widgets render.
    ///
    /// A scripted `UnixListener` in a tempdir rather than the plugin's own
    /// fake, which is a test-only module of another crate and so not reachable
    /// from here. Nothing is stubbed but the daemon's answer.
    ///
    /// Mutation (run, verified red): send `Request::Urls` instead and the
    /// pinned request line reds; make `hive_of`'s `Ok` arm return
    /// `Hive::Connecting` and the roster assertion reds.
    #[tokio::test]
    async fn the_client_dials_the_configured_socket_and_folds_the_answer() {
        let dir = tempfile::tempdir().expect("a tempdir");
        let path = dir.path().join("host.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (read, mut write) = stream.into_split();
            let mut reader = tokio::io::BufReader::new(read);
            let mut line = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
                .await
                .expect("read the request");
            tokio::io::AsyncWriteExt::write_all(
                &mut write,
                b"{\"version\":1,\"ok\":true,\"agent_statuses\":[{\"name\":\"argus\",\
                  \"running\":true,\"status_text\":\"reviewing PR #947\"}]}\n",
            )
            .await
            .expect("write the answer");
            line
        });

        // The socket path comes out of the config the tab reads, not a
        // literal: this is what pins "the tab dials what `agents.toml` says".
        let cfg = AgentsConfig {
            socket: path.to_string_lossy().into_owned(),
            ..AgentsConfig::default()
        };
        let answer = hytte_plugin_agents::hive::client::request(
            std::path::Path::new(&cfg.socket),
            &hytte_plugin_agents::hive::wire::Request::AgentStatus,
        )
        .await;

        let request_line = server.await.expect("the server task");
        assert_eq!(request_line.trim(), r#"{"cmd":"agent_status"}"#);

        let hive = hive_of(&answer);
        let agents = hive.agents();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name.as_str(), "argus");
        assert_eq!(agents[0].status_line(), "reviewing PR #947");
    }

    /// An **absent** socket folds to [`Hive::Unreachable`] with the client's
    /// own operator-facing reason — not a panic, and not an empty roster that
    /// would read as "this hive has no agents".
    #[tokio::test]
    async fn a_missing_socket_folds_to_unreachable_and_not_to_an_empty_roster() {
        let dir = tempfile::tempdir().expect("a tempdir");
        let path = dir.path().join("nothing-here.sock");
        let answer = hytte_plugin_agents::hive::client::request(
            &path,
            &hytte_plugin_agents::hive::wire::Request::AgentStatus,
        )
        .await;
        match hive_of(&answer) {
            Hive::Unreachable { reason } => assert!(reason.contains("not running"), "{reason}"),
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }
}

#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use adw::prelude::*;
    use gtk::glib;

    use super::{
        ABSENT, AgentsState, AgentsView, FACT_LABELS, build_tab, flags_of_labels, on_status,
    };
    use hytte_plugin_agents::config::{AgentsConfig, Display};
    use hytte_plugin_agents::hive::client::HiveError;
    use hytte_plugin_agents::hive::wire::{AgentStatusRow, HOST_SOCK_VERSION, Response};

    /// Run the GTK main loop until it has nothing left to dispatch, so a
    /// queued resize/allocation actually happens.
    fn pump() {
        while glib::MainContext::default().iteration(false) {}
    }

    /// A config naming this tab's socket, built by hand — nothing here reads
    /// the real `$XDG_CONFIG_HOME` (#1101).
    fn cfg() -> AgentsConfig {
        AgentsConfig {
            socket: "/run/test/host.sock".to_owned(),
            ..AgentsConfig::default()
        }
    }

    fn row(name: &str) -> AgentStatusRow {
        AgentStatusRow {
            name: name.to_owned(),
            running: true,
            ..AgentStatusRow::default()
        }
    }

    /// Feed the tab a roster, as an `AgentStatus` reply would.
    fn apply(state: &AgentsState, names: &[&str]) {
        let rows: Vec<AgentStatusRow> = names.iter().map(|n| row(n)).collect();
        on_status(
            state,
            &Ok(Response {
                version: HOST_SOCK_VERSION,
                ok: true,
                agent_statuses: Some(rows),
                ..Response::default()
            }),
        );
        pump();
    }

    /// Feed the tab a failed poll — the same call a real timeout makes.
    fn apply_failure(state: &AgentsState) {
        on_status(
            state,
            &Err(HiveError::Unreachable {
                reason: "no socket — hive-c0re is not running".to_owned(),
            }),
        );
        pump();
    }

    /// Put the tab in a window `width` px wide and let GTK allocate it. The
    /// window is returned so the caller keeps it alive — a destroyed window
    /// unmaps the tree, and these assertions are about a mapped tree.
    fn present(bin: &adw::BreakpointBin, width: i32) -> gtk::Window {
        let window = gtk::Window::new();
        window.set_child(Some(bin));
        window.set_default_size(width, 400);
        window.present();
        pump();
        window
    }

    fn dismiss(window: &gtk::Window) {
        window.set_child(None::<&gtk::Widget>);
        window.destroy();
        pump();
    }

    /// Every sidebar row's title, top to bottom.
    fn row_titles(state: &AgentsState) -> Vec<String> {
        let mut out = Vec::new();
        let mut child = state.list.first_child();
        while let Some(widget) = child {
            if let Some(row) = widget.downcast_ref::<adw::ActionRow>() {
                out.push(row.title().to_string());
            }
            child = widget.next_sibling();
        }
        out
    }

    /// Every fact row's subtitle, in [`FACT_LABELS`] order.
    fn fact_values(state: &AgentsState) -> Vec<String> {
        state
            .detail
            .facts
            .iter()
            .map(|r| r.subtitle().unwrap_or_default().to_string())
            .collect()
    }

    // ── retarget, not rebuild ───────────────────────────────────────────────

    /// **The load-bearing one.** A poll whose roster membership is unchanged
    /// must reuse the very same widgets — both the sidebar rows and the detail
    /// pane's rows — so neither the selection nor a pushed page is disturbed.
    ///
    /// Pointer identity, not text equality: text would still match if every
    /// widget had been thrown away and rebuilt with the same content, which is
    /// exactly the defect this guards.
    ///
    /// Mutation (run, verified red): make `same_agent_set` return `false`
    /// unconditionally — the rebuild path then runs on every poll and all
    /// three identity assertions red.
    #[gtk::test]
    fn an_unchanged_roster_reuses_its_widgets_rather_than_rebuilding() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);
        apply(&state, &["argus", "beta"]);

        let first_row = state.by_name.borrow()["argus"].row.clone();
        let first_fact = state.detail.facts[0].clone();
        let first_flag = state.detail.flags[0].row.clone();

        // Three more polls with the same membership and changed content.
        for text in ["one", "two", "three"] {
            on_status(
                &state,
                &Ok(Response {
                    version: HOST_SOCK_VERSION,
                    ok: true,
                    agent_statuses: Some(vec![
                        AgentStatusRow {
                            status_text: Some(text.to_owned()),
                            ..row("argus")
                        },
                        row("beta"),
                    ]),
                    ..Response::default()
                }),
            );
            pump();
        }

        assert!(
            state.by_name.borrow()["argus"].row == first_row,
            "the sidebar row was rebuilt"
        );
        assert!(
            state.detail.facts[0] == first_fact,
            "the detail fact row was rebuilt"
        );
        assert!(
            state.detail.flags[0].row == first_flag,
            "the detail flag row was rebuilt"
        );
        // …and it is genuinely tracking: the content did change.
        assert_eq!(fact_values(&state)[0], "three");
        dismiss(&window);
    }

    /// A steady poll must not move the selection either — the other half of
    /// "the poll is invisible".
    #[gtk::test]
    fn a_steady_poll_keeps_the_selection_and_the_pushed_page() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 400); // collapsed
        apply(&state, &["argus", "beta", "gamma"]);

        // Drill into the second agent.
        let row = state.by_name.borrow()["beta"].row.clone();
        state.list.select_row(Some(row.upcast_ref::<gtk::ListBoxRow>()));
        pump();
        state.split.set_show_content(true);
        pump();
        assert_eq!(state.selected.borrow().as_deref(), Some("beta"));
        assert!(state.split.shows_content());

        apply(&state, &["argus", "beta", "gamma"]);
        assert_eq!(state.selected.borrow().as_deref(), Some("beta"));
        assert!(state.split.shows_content(), "the pushed page was popped");
        dismiss(&window);
    }

    /// A **failed** poll parks the selection rather than losing it: one
    /// timeout on a two-second cadence must not quietly move the operator to
    /// whichever agent happens to be first.
    ///
    /// Mutation (run, verified red): drop the `park` assignment in
    /// `set_placeholder` and the restored-selection assertion reds.
    #[gtk::test]
    fn a_failed_poll_parks_the_selection_and_the_next_good_one_restores_it() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 400);
        apply(&state, &["argus", "beta"]);
        let row = state.by_name.borrow()["beta"].row.clone();
        state.list.select_row(Some(row.upcast_ref::<gtk::ListBoxRow>()));
        pump();
        state.split.set_show_content(true);
        pump();

        apply_failure(&state);
        assert_eq!(state.view.get(), AgentsView::Placeholder);
        assert_eq!(state.selected.borrow().as_deref(), None);

        apply(&state, &["argus", "beta"]);
        assert_eq!(
            state.selected.borrow().as_deref(),
            Some("beta"),
            "the parked selection came back"
        );
        assert!(state.split.shows_content(), "and so did the pushed page");
        dismiss(&window);
    }

    // ── the unreachable state ───────────────────────────────────────────────

    /// An unreachable hive renders **one row naming the socket**, never an
    /// empty tab — in the sidebar and on the detail status page both.
    ///
    /// Mutation (run, verified red): return early from `set_placeholder`
    /// before appending the row and the sidebar assertion reds.
    #[gtk::test]
    fn an_unreachable_hive_renders_a_row_that_names_the_socket() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);
        apply_failure(&state);

        let titles = row_titles(&state);
        assert_eq!(titles.len(), 1, "one placeholder row, not an empty list");
        assert_eq!(titles[0], "Hive unreachable");

        let description = state.detail.empty.description().unwrap_or_default();
        assert!(
            description.contains("/run/test/host.sock"),
            "{description}"
        );
        assert_eq!(state.detail.stack.visible_child_name().as_deref(), Some("empty"));
        dismiss(&window);
    }

    /// An `Up` hive with an empty roster says so in its own words — not "the
    /// hive is down".
    #[gtk::test]
    fn an_empty_roster_is_not_an_unreachable_hive() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);
        apply(&state, &[]);
        assert_eq!(row_titles(&state), vec!["No agents".to_owned()]);
        dismiss(&window);
    }

    /// A run of failed polls rebuilds the row **once**, not once per tick —
    /// the transition gate. Without it the list would flicker every two
    /// seconds for as long as the hive is down.
    #[gtk::test]
    fn a_run_of_failures_rebuilds_the_placeholder_once() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);
        apply_failure(&state);
        let first = state.rows.borrow()[0].clone();
        apply_failure(&state);
        apply_failure(&state);
        assert!(
            state.rows.borrow()[0] == first,
            "the placeholder row was rebuilt"
        );
        assert_eq!(row_titles(&state).len(), 1);
        dismiss(&window);
    }

    // ── the sidebar's order, on real widgets ────────────────────────────────

    /// The order default, end to end: the rows on screen are the sidebar's
    /// order, not the wire's.
    #[gtk::test]
    fn the_rows_on_screen_follow_the_sidebars_order() {
        let mut c = cfg();
        c.display.insert(
            "zed".to_owned(),
            Display {
                label: None,
                icon: None,
                project: Some("alpha".to_owned()),
            },
        );
        c.display.insert(
            "abe".to_owned(),
            Display {
                label: None,
                icon: None,
                project: Some("beta".to_owned()),
            },
        );
        let (bin, state) = build_tab(c);
        let window = present(&bin, 900);
        apply(&state, &["mid", "abe", "zed"]);
        assert_eq!(row_titles(&state), ["zed", "abe", "mid"]);
        dismiss(&window);
    }

    // ── the detail pane ─────────────────────────────────────────────────────

    /// Selecting an agent fills every labelled row, and the two link rows are
    /// **insensitive** when the hive publishes no destination — read-only, but
    /// never a dead click.
    #[gtk::test]
    fn the_detail_pane_fills_its_rows_and_disables_dead_links() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);
        on_status(
            &state,
            &Ok(Response {
                version: HOST_SOCK_VERSION,
                ok: true,
                agent_statuses: Some(vec![AgentStatusRow {
                    parent: Some("queen".to_owned()),
                    deployed_sha: Some("0123456789ab".to_owned()),
                    active_model: Some("claude-opus-4-6".to_owned()),
                    status_text: Some("reviewing PR #947".to_owned()),
                    ..row("argus")
                }]),
                ..Response::default()
            }),
        );
        pump();

        assert_eq!(state.selected.borrow().as_deref(), Some("argus"));
        assert_eq!(
            state.detail.stack.visible_child_name().as_deref(),
            Some("agent")
        );
        assert_eq!(state.detail.page.title(), "argus");
        assert_eq!(state.detail.facts.len(), FACT_LABELS.len());
        assert_eq!(state.detail.flags.len(), flags_of_labels().len());

        let values = fact_values(&state);
        assert_eq!(values[0], "reviewing PR #947");
        assert_eq!(values[2], "claude-opus-4-6");
        assert_eq!(values[3], "queen");
        assert_eq!(values[4], "0123456789ab");

        assert!(
            !state.detail.agent_page.is_sensitive(),
            "no url on the row, so no live link"
        );
        assert_eq!(state.detail.agent_page.subtitle().as_deref(), Some(ABSENT));
        assert!(!state.detail.config_repo.is_sensitive());
        dismiss(&window);
    }

    /// A selection whose agent leaves the roster falls back to the status page
    /// rather than showing stale rows for an agent the hive no longer reports.
    #[gtk::test]
    fn a_vanished_agent_drops_to_the_status_page() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);
        apply(&state, &["argus", "beta"]);
        let row = state.by_name.borrow()["beta"].row.clone();
        state.list.select_row(Some(row.upcast_ref::<gtk::ListBoxRow>()));
        pump();

        apply(&state, &["argus"]);
        assert_eq!(
            state.selected.borrow().as_deref(),
            Some("argus"),
            "the sidebar settles on the survivor"
        );
        assert!(!state.split.shows_content(), "and pops any pushed page");
        dismiss(&window);
    }

    // ── the adaptive shape ──────────────────────────────────────────────────

    /// Wide, both panes show; narrow, the breakpoint collapses to one at a
    /// time. One widget tree serves both, so a resize across the threshold
    /// cannot lose state.
    #[gtk::test]
    fn the_split_collapses_below_the_breakpoint_and_not_above_it() {
        let (bin, state) = build_tab(cfg());
        let wide = present(&bin, 900);
        apply(&state, &["argus"]);
        assert!(!state.split.is_collapsed(), "wide is two panes");
        dismiss(&wide);

        let narrow = present(&bin, 400);
        pump();
        assert!(state.split.is_collapsed(), "narrow is one pane at a time");
        dismiss(&narrow);
    }

    /// The tab draws no window controls of its own: it is mounted inside the
    /// app window's own header bar, and a second live close/minimise cluster
    /// appearing per tab is what `plugins_tab::tab_header_bar` exists to stop.
    #[gtk::test]
    fn the_tab_draws_no_window_controls() {
        let (bin, _state) = build_tab(cfg());
        let window = present(&bin, 900);
        assert!(
            !has_window_controls(bin.upcast_ref::<gtk::Widget>()),
            "a GtkWindowControls reached the tab tree"
        );
        dismiss(&window);
    }

    fn has_window_controls(widget: &gtk::Widget) -> bool {
        if widget.is::<gtk::WindowControls>() {
            return true;
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            if has_window_controls(&c) {
                return true;
            }
            child = c.next_sibling();
        }
        false
    }

}
