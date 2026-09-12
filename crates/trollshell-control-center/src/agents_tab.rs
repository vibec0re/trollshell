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
//! # Nothing off the wire is ever parsed as markup
//!
//! **The rule for this whole file: a string that came from the hive reaches a
//! label as text, never as Pango markup.** `status_text` is written by the
//! *agent itself*, `active_model` / `parent` / `deployed_sha` by the hive, and
//! the client's `reason` strings quote whatever the daemon said — a bare `&`
//! ("R&D", a URL with a query string) makes Pango fail the whole label and
//! render it **blank**, and a well-formed `<span …>` would be an injection
//! channel from agent-controlled text into the settings app's chrome (#1147
//! review, HIGH 1).
//!
//! Two mechanisms, because libadwaita offers two:
//!
//! - Every row built here is an `AdwPreferencesRow` subclass, whose title and
//!   subtitle labels bind `use-markup` from the row itself
//!   (`adw-action-row.ui`) — so every one of them is built with
//!   **`use_markup(false)`**, and that one flag covers both lines.
//! - `AdwStatusPage:description` has no such switch (its label is
//!   `use-markup: True` in `adw-status-page.ui`, unlike its title label, which
//!   is plain), so the one wire string that reaches it — the placeholder's
//!   reason — is **escaped** with [`glib::markup_escape_text`] instead. That is
//!   `places_tab`'s remedy, applied at the one place the other one cannot
//!   reach.
//!
//! `AdwNavigationPage:title` needs neither: it renders through `AdwWindowTitle`,
//! whose labels declare no `use-markup` and so are plain text.
//!
//! `adw::Toast:title` is the odd one out in this census, not because it needs
//! a third mechanism but because it needs **none**: Adw-1.gir documents that
//! toast titles use Pango markup by default, but the one caller
//! ([`toast`]) never puts wire text there — its argument is a GIO error's own
//! message, not a string the hive or an agent wrote (#1147 review, NIT N4).
//!
//! # Why the poll is a `glib` timer and not a task
//!
//! The tab has no runtime of its own: [`crate::spawn_on_runtime`] puts one
//! round trip on the shared `hytte-reactive` runtime and hands the answer back
//! to the GTK thread over a oneshot. That is the same seam the Plugins tab's
//! `Control` calls use, so the window's existing "drop the timer on close"
//! bookkeeping (#542) covers this tab with no new machinery.
//!
//! **One round trip at a time.** `client::REQUEST_TIMEOUT` is 5 s and the
//! default cadence is 2 s, so an unguarded poll would have two or three
//! `AgentStatus` requests in flight against a slow hive, resolving in
//! completion order rather than issue order — a 5 s timeout issued at t=0
//! landing *after* a good answer issued at t=4 s, flipping the roster to
//! "Hive unreachable" and back. [`AgentsState::claim`] is `ShellStatusUi`'s
//! one-slot guard (`main.rs`, #989's LOW 3) applied here for the same reason:
//! a tick with a request outstanding is skipped, so an older answer can never
//! land after a newer one (#1147 review, HIGH 2).
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
use gtk::{gdk, glib};

use hytte_plugin_agents::config::AgentsConfig;
use hytte_plugin_agents::hive::client::{self, HiveError};
use hytte_plugin_agents::hive::wire::{Approval, HiveUrls, Request, Response};
use hytte_plugin_agents::model::{
    Agent, AgentName, Hive, PendingApprovals, Status, agent_url, group,
};
use hytte_plugin_agents::plugin::detail_line;
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

/// How tall the "Unassigned approvals" group may grow before it scrolls
/// (#1149 N1, this round's review HIGH).
///
/// Measured against the row it holds: an unassigned row is ~84 px (three
/// subtitle lines), so this is about two and a half rows — enough that the
/// common case (one or two orphans) never scrolls at all, and small enough
/// that at the window's own 560 px the roster keeps the clear majority of the
/// sidebar. Everything past it is reachable by scrolling instead of being
/// allocated off the bottom of the window, which is what it did before.
const UNASSIGNED_MAX_HEIGHT_PX: i32 = 220;

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
    /// The agent's own page, as the hive states it — the **browser** route's
    /// destination, and the row's subtitle. `None` when the hive reports no
    /// `url` for this agent (every agent, on the hyperhive revision in the
    /// tree). That does **not** make the row dead: the companion window is
    /// launched by name and needs none of this — see [`route_for`].
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
    // In [`FACT_LABELS`] order — the built rows are zipped against this, so
    // the two lists are one contract and `the_built_row_labels_match_the_models_order`
    // is what keeps them honest.
    let facts: Vec<(&'static str, String)> = vec![
        ("Status", agent.status_line().to_owned()),
        (
            "Status set",
            status_set(row.status_set_at.as_deref(), now_unix),
        ),
        ("Model", opt(row.active_model.as_deref())),
        ("Parent", opt(row.parent.as_deref())),
        ("Deployed", opt(row.deployed_sha.as_deref())),
        (
            "Project",
            cfg.project_for(agent.name.as_str())
                .map_or_else(|| ABSENT.to_owned(), str::to_owned),
        ),
    ];
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

// ── Unassigned approvals (#1149 N1) ──────────────────────────────────────────
//
// The sidebar card and the companion window both filter `Pending` by agent —
// `hytte_plugin_agents::model::PendingApprovals::count_for`/`oldest_for` for
// the card, `trollshell-agent-window::chrome::pending_for`'s identical
// `a.agent == name` narrowing for the window — so a hive-side approval whose
// `agent` is absent (an empty string, the wire's decode for a missing key,
// `hive/wire.rs`) or names an agent not on this hive's roster matches neither
// and is never shown, never denied, anywhere on the desktop. This tab is the
// natural catch-all (#1149's own framing): it already links `hytte-plugin-
// agents` for the roster, and #1147 already established it as read-only, so
// an orphan approval fits the same contract every other row on this tab does
// — show what the wire says, write nothing.

/// The `Pending` answer, narrowed to the rows neither per-agent surface can
/// ever show: still-waiting (`PendingApprovals::new`, the one place "which
/// statuses count" is decided) and naming an agent outside `known`.
///
/// `known` is this tab's own roster at the moment the queue was polled — the
/// same one `Hive::agents()` already carries from the paired `AgentStatus`
/// answer, so this is never asked to guess about an agent it cannot see.
#[must_use]
pub(crate) fn unassigned_approvals(queue: Vec<Approval>, known: &[String]) -> Vec<Approval> {
    PendingApprovals::new(queue)
        .all()
        .iter()
        .filter(|a| a.agent.trim().is_empty() || !known.iter().any(|k| k == &a.agent))
        .cloned()
        .collect()
}

/// Where to answer one unassigned approval — the **first** line of its row.
///
/// This tab writes nothing (#1147's contract), so the row's only job once it
/// has named the request is to say which *other* surface might still be able
/// to decide it. Three answers, not two (this round's review, LOW 5):
///
/// - No name at all: nothing but the hive's own dashboard can reach it.
/// - A name this build will not render: [`hive_of`] drops any roster row whose
///   name fails [`AgentName::parse`], so such an agent has no sidebar card and
///   never will — telling the operator to wait for one is telling them to wait
///   forever.
/// - A legal name that is merely absent (renamed, or removed from
///   `agents.toml`): its own card is the place, if it comes back.
#[must_use]
fn unassigned_destination(agent: &str) -> String {
    let agent = agent.trim();
    if agent.is_empty() {
        "no agent named on this request — answer from the hive's dashboard".to_owned()
    } else if AgentName::parse(agent).is_none() {
        format!(
            "for \"{agent}\", a name this build will not show anywhere — answer from the hive's \
             dashboard"
        )
    } else {
        format!(
            "for \"{agent}\", not on this hive's roster — answer from that agent's own sidebar \
             card if it returns, or the hive's dashboard"
        )
    }
}

/// One unassigned row's subtitle: where to answer it, then the plugin's own
/// detail line (kind, description, stamp —
/// `hytte_plugin_agents::plugin::detail_line`, not a copy of it, for the
/// reason `chrome::ApprovalRow::of`'s doc gives: two renderers of one queue's
/// free text have to agree).
///
/// **The destination comes first** (this round's review, LOW 4). The row is
/// clamped to three lines and `detail_line` carries up to `DETAIL_CHARS` of
/// the manager's own prose, which wraps to three lines on its own at this
/// sidebar's width — so with the order the other way round, the one sentence
/// this row exists to say was ellipsized away exactly when the description was
/// long enough to need it.
#[must_use]
fn unassigned_subtitle(a: &Approval) -> String {
    format!("{}\n{}", unassigned_destination(&a.agent), detail_line(a))
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
    /// The tab's own toast overlay, between the bin and the split view — what
    /// a failed launch surfaces on (#1147 review, MEDIUM 7).
    toasts: adw::ToastOverlay,
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
    /// Whether an `AgentStatus` round trip is outstanding — the one-slot guard
    /// that keeps a slow hive from stacking connections, and so keeps a stale
    /// answer from landing after a newer one. See [`AgentsState::claim`].
    in_flight: Rc<Cell<bool>>,
    /// How a click reaches the outside world, injected for the tests. An
    /// `Rc<RefCell<…>>` rather than a plain field because the handlers hold a
    /// `WeakAgentsState` cloned at connect time, so a test that swaps the
    /// actions after `build_tab` has to swap them through shared ownership.
    actions: Rc<RefCell<Actions>>,
    /// The "Unassigned approvals" group (#1149 N1) — read-only, mounted below
    /// the roster so it is visible regardless of which agent (if any) is
    /// selected. Hidden whenever there is nothing to show.
    unassigned_group: adw::PreferencesGroup,
    /// The group's current rows. Rebuilt on every apply
    /// ([`render_unassigned`]) rather than retargeted by id: unlike the
    /// companion window's approvals (#1149 N2), these rows carry no buttons
    /// and no in-flight latch, so there is no widget state a rebuild could
    /// lose and no click that could land on one mid-replacement.
    unassigned_rows: Rc<RefCell<Vec<adw::ActionRow>>>,
    /// Whether this tab is on screen, and when it last asked because of that
    /// — the poll's park gate. See [`Presentation`].
    presentation: Rc<Presentation>,
}

/// The Agents tab's park gate (#1149 L4's argument, applied here on this
/// round's review, MEDIUM 3).
///
/// [`start_poll`] used to be an unconditional `glib::timeout_add_local`, so the
/// tab dialled `host.sock` every `poll_seconds` for the whole life of the
/// control-center — whichever tab was showing, mapped or not — and #1149 N1
/// then added a second round trip (`Pending`) to every one of those ticks, in
/// the same change whose headline is "spend fewer round trips on a surface
/// nobody is looking at".
///
/// The signal is the one the companion window parks on
/// (`trollshell_agent_window::window::watch_presentation`): the compositor's
/// `GdkToplevelState::SUSPENDED`, plus map/unmap. Here map/unmap is doing real
/// work rather than standing in for teardown, and it is the stronger half:
/// this tab is one child of an `AdwViewStack`, and GTK maps only the visible
/// child — so "another tab is showing" unmaps this one, which is exactly the
/// state the review wanted gated and needs no `visible-child-name` watching of
/// its own.
///
/// The flap guard is the window's, for the same reason (a workspace switch is
/// an edge): a resume less than one poll interval after the last gated refresh
/// rides the next scheduled tick instead of dialling again.
#[derive(Default)]
struct Presentation {
    /// Whether the tab is being presented — mapped, on an unsuspended window.
    on: Cell<bool>,
    /// When the gate last let a tick through, or forced one on a resume.
    /// Wall-clock, because the timer this guards is a `glib` one.
    last: Cell<Option<std::time::Instant>>,
}

/// The single in-flight slot's guard: releases it on drop, wherever that
/// happens.
struct InFlightSlot(Rc<Cell<bool>>);

impl Drop for InFlightSlot {
    fn drop(&mut self) {
        self.0.set(false);
    }
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
    toasts: glib::WeakRef<adw::ToastOverlay>,
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
    in_flight: Rc<Cell<bool>>,
    actions: Rc<RefCell<Actions>>,
    unassigned_group: glib::WeakRef<adw::PreferencesGroup>,
    unassigned_rows: Rc<RefCell<Vec<adw::ActionRow>>>,
    presentation: Rc<Presentation>,
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
    /// Claim the single in-flight slot for this tick, or `None` because a
    /// round trip is still outstanding. The returned guard releases the slot
    /// when it drops.
    ///
    /// `ShellStatusUi::claim` in this crate's `main.rs`, for the same reason
    /// it exists there (#989's LOW 3) and one more: with at most one request
    /// in flight, answers arrive in issue order, so a 5 s timeout cannot land
    /// after the good answer that followed it.
    fn claim(&self) -> Option<InFlightSlot> {
        if self.in_flight.replace(true) {
            return None;
        }
        Some(InFlightSlot(self.in_flight.clone()))
    }

    /// The handler-side view of this state.
    fn downgrade(&self) -> WeakAgentsState {
        WeakAgentsState {
            split: self.split.downgrade(),
            list: self.list.downgrade(),
            toasts: self.toasts.downgrade(),
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
                facts: self.detail.facts.iter().map(ObjectExt::downgrade).collect(),
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
            in_flight: self.in_flight.clone(),
            actions: self.actions.clone(),
            unassigned_group: self.unassigned_group.downgrade(),
            presentation: self.presentation.clone(),
            unassigned_rows: self.unassigned_rows.clone(),
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
            toasts: self.toasts.upgrade()?,
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
            in_flight: self.in_flight.clone(),
            actions: self.actions.clone(),
            unassigned_group: self.unassigned_group.upgrade()?,
            presentation: self.presentation.clone(),
            unassigned_rows: self.unassigned_rows.clone(),
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
    (bin, start_poll(&state, interval))
}

/// Start the tab's poll on `interval` and hand back its `SourceId`.
///
/// Split out of [`build_page`] so a test can start a real timer against a
/// scripted socket without [`build_page`]'s `config::load()` reading the real
/// `$XDG_CONFIG_HOME` (#1101) — which is what makes the `ControlFlow` below
/// testable at all. Before #1147's review, turning it into
/// [`glib::ControlFlow::Break`] — a tab that polls exactly once and then never
/// again — passed the whole suite (its MEDIUM 4).
///
/// The tick is **gated on [`Presentation`]**: the timer keeps running (a
/// `SourceId` the window still owns and still drops on close), but a tick that
/// fires while the tab is not on screen asks the hive for nothing. The resume
/// itself is what reconciles — [`on_presented`] — so switching back does not
/// wait out an interval.
fn start_poll(state: &AgentsState, interval: std::time::Duration) -> glib::SourceId {
    let state = state.clone();
    glib::timeout_add_local(interval, move || {
        if state.presentation.on.get() {
            state.presentation.last.set(Some(std::time::Instant::now()));
            refresh(&state);
        } else {
            tracing::trace!("the Agents tab is not on screen — this tick asks the hive nothing");
        }
        glib::ControlFlow::Continue
    })
}

/// Track whether the tab is on screen, so [`start_poll`] can park.
///
/// Wired on the tab's own root rather than on the window, which the tab never
/// sees (`main.rs` builds every page before the window exists): a widget's
/// `map`/`unmap` already carry both halves — the window being hidden, and
/// `AdwViewStack` unmapping every child but the visible one — and `realize` is
/// where the `GdkSurface` behind it first exists, which is the only place the
/// `GdkToplevel` whose `state` carries `SUSPENDED` can be reached. See
/// [`Presentation`].
fn connect_presentation(state: &AgentsState, bin: &adw::BreakpointBin) {
    let weak = state.downgrade();
    bin.connect_map(move |w| {
        if let Some(state) = weak.upgrade() {
            on_presented(&state, presenting(w));
        }
    });
    let weak = state.downgrade();
    bin.connect_unmap(move |_| {
        if let Some(state) = weak.upgrade() {
            on_presented(&state, false);
        }
    });
    let weak = state.downgrade();
    bin.connect_realize(move |w| {
        let Some(toplevel) = w
            .native()
            .and_then(|n| n.surface())
            .and_downcast::<gdk::Toplevel>()
        else {
            tracing::debug!("no GdkToplevel over this tab — the poll cannot park on suspension");
            return;
        };
        let weak = weak.clone();
        let widget = w.downgrade();
        toplevel.connect_state_notify(move |t| {
            let Some(state) = weak.upgrade() else { return };
            let on = widget
                .upgrade()
                .is_some_and(|w: adw::BreakpointBin| w.is_mapped())
                && !t.state().contains(gdk::ToplevelState::SUSPENDED);
            on_presented(&state, on);
        });
    });
}

/// Whether `widget` is being presented: mapped, on a toplevel the compositor
/// has not suspended. A widget with no toplevel (nothing realized yet) is
/// judged on its map state alone.
fn presenting(widget: &impl IsA<gtk::Widget>) -> bool {
    let widget = widget.as_ref();
    widget.is_mapped()
        && widget
            .native()
            .and_then(|n| n.surface())
            .and_downcast::<gdk::Toplevel>()
            .is_none_or(|t| !t.state().contains(gdk::ToplevelState::SUSPENDED))
}

/// Take one presentation edge: park, or resume with a reconciling poll.
///
/// The resume's poll is skipped when the gate let one through less than a poll
/// interval ago — the flap guard `feed::run` carries for the same signal, so
/// flicking between tabs (or across a workspace) costs the hive nothing beyond
/// its ordinary cadence.
fn on_presented(state: &AgentsState, on: bool) {
    if state.presentation.on.replace(on) || !on {
        return;
    }
    if state
        .presentation
        .last
        .get()
        .is_some_and(|t| t.elapsed() < state.cfg.poll_interval())
    {
        tracing::debug!("the Agents tab came back inside one poll interval — the tick reconciles");
        return;
    }
    state.presentation.last.set(Some(std::time::Instant::now()));
    refresh(state);
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

    // "Unassigned approvals" (#1149 N1) — mounted below the roster rather
    // than inside the detail pane, because it is not about any one agent
    // and has to stay visible whichever (if any) is selected. Hidden by
    // default; `render_unassigned` reveals it once there is something to
    // show.
    let unassigned_group = adw::PreferencesGroup::builder()
        .title("Unassigned approvals")
        .description(
            "Queued decisions the hive could not match to an agent on this roster. Read-only \
             here, like every other row on this tab — see each row for where to answer it.",
        )
        .build();
    unassigned_group.set_visible(false);

    // **In a scroller of its own, capped** (this round's review, HIGH). In a
    // plain `Box` the group had no scroller and no cap, so every row cost the
    // roster ~84 px until the roster hit its own floor, and past four rows the
    // group's own rows were allocated *below the window* with no scrollbar to
    // reach them. That defeats exactly what N1 is for: one agent renamed out
    // of `agents.toml` orphans its whole queue at once, not one row, so the
    // feature stops working at the size that triggers it.
    //
    // `propagate_natural_height` + `max_content_height` is "as tall as the
    // rows need, up to the cap"; a `ScrolledWindow`'s minimum height is its
    // own, not its child's, so under pressure it shrinks rather than pushing
    // anything off-screen. `Automatic` horizontally for `list_scroller`'s
    // reason above.
    let unassigned_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .max_content_height(UNASSIGNED_MAX_HEIGHT_PX)
        .vexpand(false)
        .child(&unassigned_group)
        .build();
    unassigned_scroller.set_visible(false);
    // One visibility, two widgets: `render_unassigned` still toggles the
    // group, and an empty scroller must not sit on the roster's height
    // budget. A property binding rather than a second `set_visible` call so
    // there is no way to hide one and leave the other showing.
    unassigned_group
        .bind_property("visible", &unassigned_scroller, "visible")
        .sync_create()
        .build();

    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_box.append(&list_scroller);
    sidebar_box.append(&unassigned_scroller);

    let sidebar_toolbar = adw::ToolbarView::new();
    sidebar_toolbar.add_top_bar(&crate::plugins_tab::tab_header_bar());
    sidebar_toolbar.set_content(Some(&sidebar_box));
    let sidebar_page = adw::NavigationPage::new(&sidebar_toolbar, "Agents");

    let detail = build_detail();

    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&sidebar_page));
    split.set_content(Some(&detail.page));
    split.set_min_sidebar_width(SIDEBAR_MIN_PX);
    split.set_max_sidebar_width(SIDEBAR_MAX_PX);
    split.set_sidebar_width_fraction(SIDEBAR_FRACTION);

    // The overlay sits between the bin and the split view so a toast floats
    // over the whole tab — `places_tab`'s arrangement, one level lower because
    // this tab's root is the breakpoint bin.
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));

    let bin = adw::BreakpointBin::new();
    bin.set_size_request(BIN_MIN_WIDTH_PX, BIN_MIN_HEIGHT_PX);
    bin.set_child(Some(&toasts));

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
        toasts,
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
        in_flight: Rc::new(Cell::new(false)),
        actions: Rc::new(RefCell::new(Actions::default())),
        unassigned_group,
        unassigned_rows: Rc::new(RefCell::new(Vec::new())),
        presentation: Rc::new(Presentation::default()),
    };

    connect_selection(&state);
    connect_links(&state);
    connect_presentation(&state, &bin);
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
            // `use_markup(false)`: the module doc's rule. This row's own text
            // is a constant, but the flag is set at *every* row this file
            // builds so a later value swap cannot quietly reintroduce a markup
            // parser.
            let row = adw::ActionRow::builder()
                .title(*label)
                .use_markup(false)
                .build();
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
                // Every subtitle this row ever carries is the hive's own
                // string — see the module doc's rule.
                .use_markup(false)
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
        .description(
            "Opened outside this window: the agent's own page, and the forge the hive publishes.",
        )
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
        // The subtitle is a URL the hive published; a query string's `&` is
        // enough to blank the row. See the module doc's rule.
        .use_markup(false)
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
                let open = state.actions.borrow().open_uri.clone();
                open(&uri);
            }
        });
    }
}

/// Where a click on **Agent page** goes.
///
/// Split out as a value, and computed by a pure function, because the route
/// *choice* is the part worth pinning: before #1147's review the branch could
/// be inverted wholesale without a single test noticing (its MEDIUM 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// Launch the companion window with this argv.
    Window(Vec<String>),
    /// Hand this URL to the desktop's default handler.
    Browser(String),
    /// Nothing to open.
    Nothing,
}

/// Pick the route for one agent: the companion window whenever it is
/// installed, the browser only as the fallback when it is not.
///
/// **The window route depends on the agent's *name* and nothing else** — the
/// launch is `--agent <name>` and the window reads `host.sock` itself, exactly
/// as `hytte_plugin_agents::window`'s doc states for the plugin ("The launch
/// carries **only** the agent's name"). Gating it on the hive publishing a
/// per-agent `url` would couple it to the *browser* route's precondition, and
/// on the hyperhive revision in the tree (`hive-c0re/src/server.rs`'s
/// `AgentStatus` rows carry no `url` at all) that would leave the companion
/// window with no entry point whatsoever (#1147 review, HIGH 3).
///
/// **The route is chosen before anything is launched**, by resolving the
/// binary — the same rule the plugin follows, and for the same reason: a
/// detached launch cannot report a missing program, because `systemd-run`
/// answers as soon as the user manager takes the start job.
#[must_use]
pub(crate) fn route_for(name: Option<&str>, window_installed: bool, url: Option<&str>) -> Route {
    let Some(name) = name else {
        return Route::Nothing;
    };
    if window_installed {
        return Route::Window(agent_window::argv(name, agent_window::Tab::Agent));
    }
    match url {
        Some(url) => Route::Browser(url.to_owned()),
        None => Route::Nothing,
    }
}

/// Whether **Agent page** is a live row at all: either destination will do.
///
/// The row is sensitive when the window is installed *or* the hive published a
/// URL, and insensitive only when neither exists — which is the one case where
/// a click genuinely has nowhere to go.
#[must_use]
pub(crate) fn agent_page_is_live(window_installed: bool, url: Option<&str>) -> bool {
    window_installed || url.is_some()
}

/// Open the selected agent's surface along [`route_for`]'s choice.
///
/// The probe caches, and complains exactly once, so a desktop without the
/// window does not log a line per click.
///
/// A launch that *does* fail is the one case where "one click, one
/// destination" is already broken — the probe promised the binary was there —
/// so the failure is surfaced three ways rather than swallowed into a `warn!`
/// (#1147 review, MEDIUM 7): a toast, the probe cache dropped so the next
/// click re-resolves `PATH` rather than repeating a decision made against a
/// binary that has since gone, and the browser as a late fallback where the
/// hive published a URL.
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

    let installed = state.probe.borrow_mut().available();
    // Cloned out, so no borrow of `actions` is live while the action runs.
    let (launch, open) = {
        let actions = state.actions.borrow();
        (actions.launch.clone(), actions.open_uri.clone())
    };
    match route_for(Some(&name), installed, url.as_deref()) {
        Route::Window(argv) => {
            if let Err(e) = launch(&argv) {
                tracing::warn!(?argv, error = %e, "could not launch the agent companion window");
                // The probe resolved `PATH` once, possibly hours ago. A launch
                // that failed is evidence that answer is stale, so drop it.
                *state.probe.borrow_mut() = agent_window::Probe::path();
                toast(state, &format!("Could not open the agent window: {e}"));
                if let Some(url) = url {
                    open(&url);
                }
            }
        }
        Route::Browser(url) => open(&url),
        Route::Nothing => {}
    }
}

/// Raise one toast on the tab's own overlay.
///
/// The tab carries its own [`adw::ToastOverlay`] rather than reaching for the
/// window's, the way `places_tab` does: a toast about a failed launch belongs
/// over the tab that launched it, and the tab is built (and tested) without a
/// window at all.
fn toast(state: &AgentsState, text: &str) {
    state.toasts.add_toast(adw::Toast::new(text));
}

/// How the tab reaches the world outside this process — injected so a GTK test
/// can record the route taken instead of spawning a window or handing a URL to
/// the operator's browser.
///
/// Two `Rc<dyn Fn>`s rather than a trait object with two methods: the default
/// is two free functions, and the tests replace one or both with a recorder.
#[derive(Clone)]
struct Actions {
    /// Start the companion window. `Err` carries an operator-facing reason.
    launch: Launch,
    /// Hand a URL to the desktop.
    open_uri: Open,
}

/// [`Actions::launch`]'s type, named so the field is readable.
type Launch = Rc<dyn Fn(&[String]) -> Result<(), String>>;

/// [`Actions::open_uri`]'s type — see [`Launch`].
type Open = Rc<dyn Fn(&str)>;

impl Default for Actions {
    fn default() -> Self {
        Self {
            launch: Rc::new(launch_detached),
            open_uri: Rc::new(open_uri),
        }
    }
}

/// `systemd-run`, as the shell's own detached launcher names it
/// (`trollshell/src/launch.rs`) — resolved on `PATH`, never by absolute path,
/// because a NixOS profile and an FHS distro put it in different places.
const SYSTEMD_RUN: &str = "systemd-run";

/// The environment variables a detached launch forwards from this process's
/// own environment (#1147 review N2) — **copied**, not reinvented, from the
/// shell's own list: `trollshell/src/plugins/effects.rs`'s `FORWARDED_ENV`,
/// documented there (#953 L5) as load-bearing precisely because the session's
/// `systemctl --user import-environment` (`etc/niri/session.kdl`) does not
/// carry `NIRI_SOCKET` or `DISPLAY`, and a hand-started dev loop inside a
/// nested compositor inherits the *outer* session's variables instead of
/// this process's own — landing the companion window on the wrong
/// compositor. Two forwarders have to agree on what "the display" means, so
/// this is the same four names, not a second list to keep in sync by hand.
const FORWARDED_ENV: [&str; 4] = [
    "WAYLAND_DISPLAY",
    "NIRI_SOCKET",
    "DISPLAY",
    "XDG_RUNTIME_DIR",
];

/// Read [`FORWARDED_ENV`] out of this process's environment, skipping
/// anything unset or empty (so a launch never asserts an empty `DISPLAY=`
/// over the manager's real one) — mirrors `effects.rs`'s `forwarded_env`.
/// Order follows [`FORWARDED_ENV`] so the argv is deterministic.
fn forwarded_env() -> Vec<(String, String)> {
    FORWARDED_ENV
        .iter()
        .filter_map(|name| {
            let value = std::env::var(name).ok()?;
            (!value.is_empty()).then(|| ((*name).to_owned(), value))
        })
        .collect()
}

/// The argv that starts `inner` as a **transient user unit**, with `env`
/// forwarded via `--setenv=` (#1147 review N2; see [`FORWARDED_ENV`]).
///
/// No `--unit=`: systemd allocates `run-u<N>.service` itself, so a second
/// launch for the same agent never collides with the first one's unit (the
/// window is single-instance per agent through `GApplication`, and that second
/// launch is what forwards `--tab` to the running window — a fixed unit name
/// would have systemd refuse it instead). `--collect` reaps a unit that failed
/// at exec so a run of misses does not accumulate failed units; `--quiet`
/// keeps the "Running as unit" line out of the control-center's own stderr.
/// `--setenv=` entries are emitted before the `--` terminator, the same
/// position `trollshell/src/launch.rs`'s `args` uses.
#[must_use]
pub(crate) fn systemd_run_argv(inner: &[String], env: &[(String, String)]) -> Vec<String> {
    let mut argv = vec![
        SYSTEMD_RUN.to_owned(),
        "--user".to_owned(),
        "--collect".to_owned(),
        "--quiet".to_owned(),
    ];
    for (k, v) in env {
        argv.push(format!("--setenv={k}={v}"));
    }
    argv.push("--".to_owned());
    argv.extend_from_slice(inner);
    argv
}

/// Launch `argv` **detached from this process**, the way the plugin's own
/// effect does (#953's detached mode).
///
/// `systemd-run --user` first, because that is the property the plugin's
/// `window` module documents and the only one that actually holds: a plain
/// child shares this process's cgroup, so a scope teardown of the
/// control-center takes the companion window with it. A transient unit is the
/// user manager's child, not ours.
///
/// # `systemd-run`'s own exit is read (#1147 review N1)
///
/// `gio::Subprocess::newv` succeeding only proves the **helper** could be
/// exec'd — it says nothing about whether the unit it asked for actually
/// started. Measured: `systemd-run --user --collect --quiet -- <missing
/// binary>` exits 1 *after* `newv` has already returned `Ok`, printing
/// `Failed to find executable …` to stderr; a session with no user manager
/// fails the same way, `newv` succeeding and the child later refusing on
/// `Failed to connect to … bus`. Both looked identical to the code that used
/// to stop at `newv`'s result, which is why a stale probe (the companion
/// window resolved on `PATH` once, since removed) used to launch silently
/// and successfully as far as this function was concerned. So this reads the
/// child's exit before calling anything launched: `gio::Subprocess::communicate_utf8`
/// (the synchronous form — there is no async runtime here to drive the
/// `_async`/`_future` pair, and a helper this short-lived, which only hands a
/// start job to the manager and returns, does not need one) drains its
/// stderr and waits for it to exit, so `is_successful()` is valid the moment
/// it returns.
///
/// # …but the two failure shapes above are not the same verdict
///
/// N1's own fix conflated them: every non-zero exit became a flat `Err`,
/// which is right for "the target program doesn't exist" but wrong for
/// "there is nobody to ask" — a session with no `systemd --user` manager
/// (CI's nix sandbox: `systemd-run` has been on `$PATH` since #1082, but
/// nothing runs `systemd --user` there) makes `systemd-run --user` itself
/// exit non-zero on a bus-connect failure for *every* launch, so a launch
/// that would otherwise succeed got reported as failed. This is exactly the
/// shape `trollshell/src/plugins/effects.rs`'s `classify_systemd_run_failure`
/// (#953 H1) already solved once for the plugin host: match the stderr
/// *family* — [`user_manager_unreachable`] — and treat only that one as "the
/// manager was never reached", not a verdict from a manager that ran. Any
/// **other** non-zero exit (a missing target executable, a bad argv, a
/// unit-name collision) stays a hard `Err`, exactly like an exec error, and
/// reaches the same toast / probe-reset / browser-fallback path in
/// [`open_agent_page`] — that is the behaviour N1 was actually about, and it
/// is unchanged.
///
/// The direct `gio::Subprocess` spawn stays as the **fallback** for a session
/// with no user manager, restoring the promise this module's doc made before
/// N1 narrowed it too far — the same two-case shape
/// `trollshell/src/plugins/effects.rs`'s `start_detached_with` takes
/// (`FallbackReason::{NoSystemdRun,NoUserManager}`): `newv` itself failing to
/// exec the helper (missing from `PATH`, or not executable) falls back
/// immediately below, and so does a `systemd-run` that ran and reported
/// [`user_manager_unreachable`]. It still outlives this process, GIO reaps it
/// rather than leaving a zombie.
fn launch_detached(argv: &[String]) -> Result<(), String> {
    let unit = systemd_run_argv(argv, &forwarded_env());
    let as_os: Vec<&std::ffi::OsStr> = unit.iter().map(AsRef::as_ref).collect();
    let child = match gtk::gio::Subprocess::newv(
        &as_os,
        gtk::gio::SubprocessFlags::STDOUT_SILENCE | gtk::gio::SubprocessFlags::STDERR_PIPE,
    ) {
        Ok(child) => child,
        Err(e) => {
            // `systemd-run` itself is missing or not executable — one of the
            // two cases that still fall back to spawning the program
            // directly (see the module doc above).
            tracing::warn!(error = %e, "no usable systemd-run; spawning the companion window directly");
            return spawn(argv).map(|()| {
                tracing::info!(?argv, "launched the agent companion window directly");
            });
        }
    };
    let stderr = match child.communicate_utf8(None, gtk::gio::Cancellable::NONE) {
        Ok((_, stderr)) => stderr,
        Err(e) => return Err(e.message().to_owned()),
    };
    if child.is_successful() {
        tracing::info!(
            ?argv,
            "launched the agent companion window as a transient user unit"
        );
        return Ok(());
    }
    let detail = stderr
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            format!(
                "systemd-run --user exited with status {}",
                child.exit_status()
            )
        });
    if user_manager_unreachable(&detail) {
        // `systemd-run` ran and never reached a manager — the other case that
        // falls back, see the module doc above.
        tracing::warn!(
            ?argv,
            detail = %detail,
            "systemd user manager unavailable; spawning the agent companion window directly",
        );
        return spawn(argv).map(|()| {
            tracing::info!(?argv, "launched the agent companion window directly");
        });
    }
    tracing::warn!(
        ?argv,
        error = %detail,
        "systemd-run --user did not start the agent companion window",
    );
    Err(detail)
}

/// Whether a non-zero `systemd-run --user` exit means the session has no
/// reachable user manager, rather than `systemd-run` refusing the launch for
/// a reason unrelated to the manager's reachability (a missing target
/// executable, a bad argv, a unit-name collision).
///
/// Mirrors `trollshell/src/plugins/effects.rs`'s
/// `classify_systemd_run_failure` (#953 H1) rather than reinventing it: the
/// match is the stderr **family**, `"Failed to connect to"` + `"bus"`, not one
/// exact sentence — systemd has worded the bus-connect failure differently
/// release to release (`Failed to connect to bus: No medium found`, `…
/// Connection refused`, systemd 260's `Failed to connect to user scope bus
/// via local transport: …`), and matching only the literal wording of one of
/// them is how a fallback like this becomes unreachable the moment the
/// wording changes underneath it — which is exactly what happened here once
/// already (#1147 review N1's own fix dropped the fallback entirely rather
/// than narrowing the match).
fn user_manager_unreachable(stderr: &str) -> bool {
    stderr.contains("Failed to connect to") && stderr.contains("bus")
}

/// Spawn one argv through GIO, silencing the child's stdio so a chatty
/// companion window does not write into the settings app's journal stream.
///
/// `gio::Subprocess` rather than `std::process::Command` because GIO reaps the
/// child itself — a window the operator closes must not leave a zombie
/// parented to a settings app that may outlive it by hours — and because it
/// does not kill the child when this window goes away.
///
/// Used only for [`launch_detached`]'s two fallback calls (no usable
/// `systemd-run`; a `systemd-run` that ran and reported
/// [`user_manager_unreachable`]): the program spawned here is the
/// long-running companion window itself (not `systemd-run`), so unlike that
/// function's own check, this never waits on the child — doing so would
/// block until the operator closes the window.
fn spawn(argv: &[String]) -> Result<(), String> {
    let as_os: Vec<&std::ffi::OsStr> = argv.iter().map(AsRef::as_ref).collect();
    gtk::gio::Subprocess::newv(
        &as_os,
        gtk::gio::SubprocessFlags::STDOUT_SILENCE | gtk::gio::SubprocessFlags::STDERR_SILENCE,
    )
    .map(|_child| ())
    .map_err(|e| e.message().to_owned())
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

/// One tick: ask for the roster — **unless the last ask is still
/// outstanding**, in which case this tick is skipped.
///
/// The `Urls` fetch is not issued here but from [`on_status`], so it only ever
/// rides a roster answer that actually arrived (see [`refresh_urls`]).
///
/// The slot guard is **moved into the completion closure**, so the slot is
/// released whether that closure runs or is dropped unrun — [`spawn_on_runtime`]
/// calls its callback only `if let Ok(v) = rx.await`, so a sender dropped
/// without sending would otherwise wedge `in_flight` at `true` and freeze the
/// tab on its last snapshot forever. That is `ShellStatusUi::poll`'s own note
/// in this crate, and it applies here unchanged.
fn refresh(state: &AgentsState) {
    let Some(slot) = state.claim() else {
        tracing::debug!("an AgentStatus round trip is still in flight — skipping this tick");
        return;
    };
    let socket = PathBuf::from(&state.cfg.socket);
    let weak = state.downgrade();
    spawn_on_runtime(
        async move {
            let status = client::request(&socket, &Request::AgentStatus).await;
            // `Pending` rides the same round trip, and only once `AgentStatus`
            // itself answered — `hytte_plugin_agents::poll::poll_once`'s own
            // order and reason (#1149 N1): a dead hive costs one failed
            // connect, not two, and the roster this tick's queue is scoped
            // against has to be the one that arrived with it.
            let approvals = if status.is_ok() {
                Some(
                    client::request(&socket, &Request::Pending)
                        .await
                        .map(|resp| resp.approvals.unwrap_or_default())
                        .map_err(|e| e.to_string()),
                )
            } else {
                None
            };
            (status, approvals)
        },
        move |(answer, approvals)| {
            drop(slot);
            if let Some(state) = weak.upgrade() {
                on_status(&state, &answer);
                apply_unassigned(&state, approvals);
            }
        },
    );
}

/// Ask for the hive's URLs — **once**, riding a roster answer that arrived.
///
/// Called from [`on_status`] with an `Up` snapshot, never from the tick: the
/// `Urls` answer does not change under a running window, so the only thing a
/// retry buys is a second round trip per tick. Three rules make "once" true
/// (#1147 review, LOW 8):
///
/// - The want is **claimed** (taken, not read) before the request is issued,
///   so two ticks cannot both dial.
/// - A hive that **answered** — with a `urls` block or without one, or by
///   refusing the verb outright — is never asked again. The earlier shape
///   cleared the want only inside `Some(urls)`, so a hive answering
///   `ok: true, urls: null` re-dialled every tick for the life of the window.
/// - Only a transport failure puts the want back, so the fetch resumes when
///   the hive comes back, with no backoff machinery of its own.
fn refresh_urls(state: &AgentsState) {
    if !state.urls_wanted.replace(false) {
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
            match answer {
                Ok(resp) => {
                    if let Some(urls) = resp.urls {
                        *state.urls.borrow_mut() = Some(urls);
                        refresh_detail(&state);
                    }
                }
                // Only a transport failure is worth another go. A hive that
                // **answered** — refusing the verb, speaking a dialect this
                // build cannot parse, or announcing a version it refuses to
                // guess at — would answer the same way next tick.
                Err(HiveError::Unreachable { .. }) => state.urls_wanted.set(true),
                Err(
                    HiveError::Refused { .. } | HiveError::Protocol { .. } | HiveError::Version(_),
                ) => {}
            }
        },
    );
}

/// Fold one roster answer into the snapshot and repaint.
///
/// Public to the crate's tests rather than private, so the GTK tests drive the
/// exact path a real poll does instead of a lookalike.
///
/// The `Urls` fetch hangs off **this** rather than off the tick, so it is
/// issued against a hive that has just answered rather than against the
/// *previous* tick's snapshot — the seed `Connecting` state used to spend one
/// request on a hive already known to be unreachable.
fn on_status(state: &AgentsState, answer: &Result<Response, HiveError>) {
    let hive = hive_of(answer);
    let up = matches!(hive, Hive::Up { .. });
    *state.snapshot.borrow_mut() = hive;
    apply(state);
    if up {
        refresh_urls(state);
    }
}

/// Render the "Unassigned approvals" group off this tick's `Pending` answer
/// (#1149 N1), scoped to the roster [`on_status`] just folded into
/// `state.snapshot` — call this **after** `on_status`, not before, so the two
/// are always one observation.
///
/// `None` (`refresh`'s own doc: `AgentStatus` itself failed, so `Pending` was
/// never asked) and `Some(Err(reason))` (the hive answered `AgentStatus` but
/// refused `Pending` — an older daemon, a permissions change) both clear the
/// group rather than leaving it on its last good answer: this tab cannot
/// vouch for rows it no longer has a fresh roster or a fresh queue for,
/// mirroring the rule `hytte_plugin_agents::poll` and
/// `trollshell-agent-window::feed` both follow for their own approval
/// surfaces. Only `Some(Ok(queue))` renders real rows.
fn apply_unassigned(state: &AgentsState, approvals: Option<Result<Vec<Approval>, String>>) {
    let rows = match approvals {
        Some(Ok(queue)) => {
            let known: Vec<String> = state
                .snapshot
                .borrow()
                .agents()
                .iter()
                .map(|a| a.name.as_str().to_owned())
                .collect();
            unassigned_approvals(queue, &known)
        }
        Some(Err(reason)) => {
            tracing::debug!(
                %reason,
                "the hive refused the approval queue; the Unassigned group is cleared until it \
                 answers"
            );
            Vec::new()
        }
        None => Vec::new(),
    };
    render_unassigned(state, &rows);
}

/// Replace the "Unassigned approvals" group's rows.
///
/// Rebuilt on every call rather than retargeted by id, unlike the companion
/// window's approvals (#1149 N2): these rows are read-only (#1147's
/// contract) — no buttons, no in-flight latch — so there is no widget state
/// a rebuild could lose and no click that could land on one mid-replacement,
/// which is the whole argument for retargeting there and does not apply
/// here.
fn render_unassigned(state: &AgentsState, rows: &[Approval]) {
    for row in state.unassigned_rows.borrow_mut().drain(..) {
        state.unassigned_group.remove(&row);
    }
    state.unassigned_group.set_visible(!rows.is_empty());

    let mut tracked = state.unassigned_rows.borrow_mut();
    for a in rows {
        let row = adw::ActionRow::builder()
            .title(a.kind.human())
            .subtitle(unassigned_subtitle(a))
            // The subtitle carries the manager's own free text
            // (`detail_line`) plus this tab's own sentence — never markup,
            // the module doc's rule for every hive-sourced string.
            .use_markup(false)
            .subtitle_selectable(true)
            .build();
        row.set_subtitle_lines(3);
        state.unassigned_group.add(&row);
        tracked.push(row);
    }
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
        reorder_rows(state, &listed);
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

/// Put the sidebar's rows in `listed`'s order **without rebuilding any of
/// them**.
///
/// [`same_agent_set`] is set equality by design — a reorder must not tear the
/// rows down — but the in-place branch used to key by name and never touch the
/// list's child order, so the sidebar kept the order of the poll that last
/// changed *membership*, forever. The card re-sorts on every poll because it
/// re-renders; this tab does not, so "the same list as the sidebar, in the same
/// order" was true only until hyperhive shuffled its answer (#1147 review,
/// MEDIUM 5).
///
/// A no-op when the order already matches, which is every poll but the rare
/// one: the walk compares first and only moves the rows that are out of place.
///
/// Removing a row from a `GtkListBox` drops the selection and emits
/// `row-selected(None)`, so the whole walk runs under the `selecting` latch
/// and the selection is put back silently at the end — the navigation state
/// (a pushed page, collapsed) is never touched.
fn reorder_rows(state: &AgentsState, listed: &[String]) {
    if on_screen_order(state) == listed {
        return;
    }
    let selected = state.selected.borrow().clone();
    state.selecting.set(true);
    for (index, name) in listed.iter().enumerate() {
        let position = i32::try_from(index).unwrap_or(i32::MAX);
        let Some(row) = state.by_name.borrow().get(name).map(|r| r.row.clone()) else {
            continue;
        };
        let at = state.list.row_at_index(position);
        if at.is_some_and(|at| &at == row.upcast_ref::<gtk::ListBoxRow>()) {
            continue;
        }
        state.list.remove(&row);
        state.list.insert(&row, position);
    }
    // The teardown bookkeeping follows the screen, so a later rebuild removes
    // the rows that are actually mounted.
    let ordered_widgets: Vec<gtk::Widget> = listed
        .iter()
        .filter_map(|name| {
            state
                .by_name
                .borrow()
                .get(name)
                .map(|r| r.row.clone().upcast())
        })
        .collect();
    *state.rows.borrow_mut() = ordered_widgets;
    state.selecting.set(false);
    if let Some(name) = selected {
        select_silently(state, &name);
    }
}

/// The agent names of the rows the sidebar is showing, top to bottom.
///
/// Read off the `GtkListBox` itself rather than off `rows`, because the screen
/// is the thing [`reorder_rows`] is asserting about.
fn on_screen_order(state: &AgentsState) -> Vec<String> {
    let by_name = state.by_name.borrow();
    let mut out = Vec::with_capacity(by_name.len());
    let mut child = state.list.first_child();
    while let Some(widget) = child {
        if let Some(row) = widget.downcast_ref::<adw::ActionRow>()
            && let Some((name, _)) = by_name.iter().find(|(_, arow)| &arow.row == row)
        {
            out.push(name.clone());
        }
        child = widget.next_sibling();
    }
    out
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
    //
    // The title is a fixed vocabulary from [`placeholder`] and its label is
    // plain text either way; the **description** carries the client's reason,
    // which quotes the daemon, and its label is the one surface in this file
    // with `use-markup: True` and no switch to turn it off — so it is escaped.
    // See the module doc's rule.
    state.detail.empty.set_title(title);
    state
        .detail
        .empty
        .set_description(Some(&glib::markup_escape_text(detail)));

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
        // The subtitle is the client's reason plus the socket path. See the
        // module doc's rule.
        .use_markup(false)
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
    // **Agent page** is live whenever *either* destination exists: the
    // companion window needs only the agent's name, so a hive that publishes
    // no per-agent `url` still has one (#1147 review, HIGH 3). The subtitle
    // stays the hive's own URL, or `—` where there is none — the row is a
    // statement that the hive was asked, and it says nothing about which of
    // the two routes the click will take.
    let window_installed = state.probe.borrow_mut().available();
    set_link_row(
        &state.detail.agent_page,
        model.agent_page.as_deref(),
        agent_page_is_live(window_installed, model.agent_page.as_deref()),
    );
    let forge = model.config_repo.as_deref();
    set_link_row(&state.detail.config_repo, forge, forge.is_some());
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

/// Drive one link row: what the hive reported as its subtitle, and whether the
/// row can be clicked at all.
///
/// `uri` and `live` are **separate** arguments on purpose. For the config repo
/// they say the same thing, but **Agent page** has two destinations and only
/// one of them is a URL: an agent with no reported `url` still opens in the
/// companion window, so the row reads `—` and stays live. Collapsing the two
/// is exactly the coupling HIGH 3 found.
///
/// Insensitive rather than hidden, for the reason a fact row shows [`ABSENT`]
/// rather than disappearing: "the hive publishes no forge" is an answer, and a
/// row that vanishes only raises the question again.
fn set_link_row(row: &adw::ActionRow, uri: Option<&str>, live: bool) {
    row.set_subtitle(uri.unwrap_or(ABSENT));
    row.set_sensitive(live);
}

/// Build one sidebar row from its model.
///
/// No per-row handler — drill-down is the list's `row-selected` /
/// `row-activated`, so a row is a display of one agent and nothing else.
fn build_agent_row(model: &RowModel) -> AgentRow {
    // `use_markup(false)`: the title is `agents.toml`'s label or the hive's
    // own name, and the subtitle is the harness's status line — the agent
    // writes that one itself. See the module doc's rule.
    let row = adw::ActionRow::builder()
        .activatable(true)
        .use_markup(false)
        .build();

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
        ABSENT, DetailModel, FACT_LABELS, Route, RowModel, agent_page_is_live, detail_of, flags_of,
        flags_of_labels, hive_of, ordered, placeholder, route_for, rows_of, same_agent_set,
        status_set, systemd_run_argv, unassigned_approvals, unassigned_destination,
        unassigned_subtitle, user_manager_unreachable,
    };
    use hytte_plugin_agents::config::{AgentsConfig, Display};
    use hytte_plugin_agents::hive::client::HiveError;
    use hytte_plugin_agents::hive::wire::{
        AgentStatusRow, Approval, ApprovalStatus, HOST_SOCK_VERSION, HiveUrls, Response,
        VersionMismatch,
    };
    use hytte_plugin_agents::model::{Agent, AgentName, Hive, PendingApprovals, Status};

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
                .map_or_else(|| panic!("no {label} row"), |(_, v)| v.clone())
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
        assert_eq!(
            detail_of(&a, &cfg, Some(&domain_only), now()).config_repo,
            None
        );

        let with_forge = HiveUrls {
            forge: Some("  https://forge.hive.local/  ".to_owned()),
            ..domain_only
        };
        assert_eq!(
            detail_of(&a, &cfg, Some(&with_forge), now())
                .config_repo
                .as_deref(),
            Some("https://forge.hive.local/")
        );

        // An agent the hive publishes no URL for gets no link — the hive's
        // domain being unconfigured is exactly when one would be dead.
        let no_url = agent(row("argus"));
        assert_eq!(
            detail_of(&no_url, &cfg, Some(&with_forge), now()).agent_page,
            None
        );
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

    // ── which surface a click opens ─────────────────────────────────────────

    /// **The companion window needs only the agent's name.** A hive that
    /// publishes no per-agent `url` — every agent on the hyperhive revision in
    /// the tree, whose `AgentStatus` rows carry no such field at all — must
    /// still open the window, because the launch is `--agent <name>` and the
    /// window dials `host.sock` itself.
    ///
    /// Mutation (run, verified red): require a `url` for the `Window` arm
    /// (`if window_installed && url.is_some()`) and the first assertion reds.
    #[test]
    fn the_window_route_needs_only_the_agents_name() {
        assert_eq!(
            route_for(Some("argus"), true, None),
            Route::Window(vec![
                "trollshell-agent-window".to_owned(),
                "--agent".to_owned(),
                "argus".to_owned(),
            ]),
            "a hive that publishes no url still opens the companion window"
        );
        // …and the window wins over the browser when both are possible: one
        // click, one destination.
        assert!(matches!(
            route_for(Some("argus"), true, Some("https://hive/a/argus")),
            Route::Window(_)
        ));
    }

    /// The browser is the **fallback**, taken only when the window is not
    /// installed, and only where the hive named a destination.
    ///
    /// Mutation (run, verified red): invert the `window_installed` branch and
    /// both arms red — the exact mutation that passed the whole suite before
    /// this test existed (#1147 review, MEDIUM 4).
    #[test]
    fn the_browser_is_the_fallback_and_only_with_a_url() {
        assert_eq!(
            route_for(Some("argus"), false, Some("https://hive/a/argus")),
            Route::Browser("https://hive/a/argus".to_owned())
        );
        assert_eq!(route_for(Some("argus"), false, None), Route::Nothing);
        assert_eq!(
            route_for(None, true, Some("https://hive/a/x")),
            Route::Nothing
        );
    }

    /// The **Agent page** row is live whenever either destination exists, and
    /// dead only when neither does — the coupling HIGH 3 found was this
    /// predicate being `url.is_some()` alone.
    #[test]
    fn the_agent_page_row_is_live_for_either_destination() {
        assert!(agent_page_is_live(true, None), "the window alone is enough");
        assert!(agent_page_is_live(false, Some("https://hive/a/argus")));
        assert!(agent_page_is_live(true, Some("https://hive/a/argus")));
        assert!(
            !agent_page_is_live(false, None),
            "no window and no url is the one dead click"
        );
    }

    /// The launch goes through a **transient user unit**, so the window is the
    /// user manager's child and survives a scope teardown of the
    /// control-center — the property `hytte_plugin_agents::window`'s doc
    /// claims for the plugin's own launch (#1147 review, MEDIUM 6).
    ///
    /// The absence of `--unit=` is asserted, not incidental: the window is
    /// single-instance per agent, and a second launch (what forwards `--tab`
    /// to the running window) would collide with the first one's unit name.
    #[test]
    fn the_detached_launch_is_a_transient_user_unit() {
        let inner = vec![
            "trollshell-agent-window".to_owned(),
            "--agent".to_owned(),
            "argus".to_owned(),
        ];
        let argv = systemd_run_argv(&inner, &[]);
        assert_eq!(argv[0], "systemd-run");
        assert!(argv.contains(&"--user".to_owned()), "{argv:?}");
        assert!(argv.contains(&"--collect".to_owned()), "{argv:?}");
        assert!(
            !argv.iter().any(|a| a.starts_with("--unit")),
            "a fixed unit name would refuse the second launch: {argv:?}"
        );
        let sep = argv
            .iter()
            .position(|a| a == "--")
            .expect("the argv is terminated before the program");
        assert_eq!(&argv[sep + 1..], inner.as_slice());
    }

    /// `--setenv=` carries the forwarded display environment, in the same
    /// `--setenv=K=V` shape `trollshell/src/launch.rs`'s `args` emits, and
    /// still before the `--` terminator (#1147 review N2).
    #[test]
    fn the_detached_launch_forwards_the_given_environment() {
        let inner = vec![
            "trollshell-agent-window".to_owned(),
            "--agent".to_owned(),
            "argus".to_owned(),
        ];
        let env = vec![
            ("WAYLAND_DISPLAY".to_owned(), "wayland-1".to_owned()),
            (
                "NIRI_SOCKET".to_owned(),
                "/run/user/1000/niri.sock".to_owned(),
            ),
        ];
        let argv = systemd_run_argv(&inner, &env);
        assert!(
            argv.contains(&"--setenv=WAYLAND_DISPLAY=wayland-1".to_owned()),
            "{argv:?}"
        );
        assert!(
            argv.contains(&"--setenv=NIRI_SOCKET=/run/user/1000/niri.sock".to_owned()),
            "{argv:?}"
        );
        let sep = argv
            .iter()
            .position(|a| a == "--")
            .expect("the argv is terminated before the program");
        assert_eq!(&argv[sep + 1..], inner.as_slice());
    }

    /// The three shapes [`user_manager_unreachable`] has to tell apart, pinned
    /// as a pure unit test so the discriminator itself is falsifiable without
    /// a real `systemd-run` (the `gtk_tests` scenarios below need one; this
    /// does not). A bus-connect failure is the one case that means "nobody to
    /// ask"; a missing-executable message and an empty string are both a
    /// verdict from a manager that *did* run, or no output at all, and must
    /// stay `false` or a bad launch would silently be retried as if it had
    /// never happened.
    #[test]
    fn user_manager_unreachable_matches_the_bus_connect_family_only() {
        assert!(user_manager_unreachable(
            "Failed to connect to user scope bus via local transport: \
             $DBUS_SESSION_BUS_ADDRESS and $XDG_RUNTIME_DIR not defined"
        ));
        assert!(!user_manager_unreachable(
            "Failed to find executable /nonexistent-1147-review-n1: No such file or directory"
        ));
        assert!(!user_manager_unreachable(""));
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

    fn approval(id: i64, agent: &str, status: ApprovalStatus) -> Approval {
        Approval {
            id,
            agent: agent.to_owned(),
            status,
            ..Approval::default()
        }
    }

    /// **The current behaviour, stated as intended** (#1149 N1): an approval
    /// with no `agent` (the wire's decode for an absent key, `hive/wire.rs`)
    /// or one naming an agent not on this hive's roster is invisible to
    /// *both* per-agent surfaces — the sidebar plugin's own
    /// [`PendingApprovals::count_for`]/`oldest_for` selectors, and the
    /// companion window's `chrome::pending_for`, which narrows the identical
    /// [`PendingApprovals::new`] answer by the same `a.agent == name`
    /// predicate restated here (a cross-crate call would need a dependency
    /// this tab has no other reason to take) — but this tab's
    /// [`unassigned_approvals`] is exactly the complement of that predicate,
    /// so the two rows neither surface can show land here and nowhere else.
    ///
    /// Mutation (verified red): drop the `!known.iter().any(…)` arm of the
    /// filter and the "unknown agent" row (id 2) vanishes from this tab too
    /// — landing on no desktop surface at all. Drop the `agent.trim().is_empty()`
    /// arm instead and the "no agent" row (id 1) vanishes the same way.
    #[test]
    fn an_agentless_or_unknown_agent_approval_is_invisible_to_the_per_agent_filters() {
        let queue = vec![
            approval(1, "", ApprovalStatus::Pending), // absent agent
            approval(2, "ghost", ApprovalStatus::Pending), // unknown agent
            approval(3, "stray", ApprovalStatus::Pending), // on the roster
            approval(4, "stray", ApprovalStatus::Approved), // already decided
        ];
        let known = vec!["stray".to_owned()];

        // The sidebar's own per-agent selectors, over the roster's one known
        // name — real production functions, not a restatement of them. The
        // sidebar only ever calls these with a name off its own roster
        // (`agents.toml`/the wire's own agent rows), never with `""` or
        // `"ghost"` — there is no row to raise a badge on for either — so
        // the true claim to pin is that summing every *reachable* query
        // never turns up ids 1 or 2, not that querying an unreachable one
        // would happen to answer `None` (`oldest_for` has no idea "" is not
        // a real name; it would cheerfully return the row if asked).
        let pending = PendingApprovals::new(queue.clone());
        let reachable_count: usize = known.iter().map(|a| pending.count_for(a)).sum();
        assert_eq!(
            reachable_count, 1,
            "only the roster's own approval is reachable through any agent's badge"
        );
        assert_eq!(
            pending.oldest_for("stray").map(|a| a.id),
            Some(3),
            "the one badge the roster can raise names the roster's own approval"
        );

        // The companion window's `chrome::pending_for` filter, restated:
        // `PendingApprovals` narrowed to one agent's name — the identical
        // expression that function applies after the identical `new`.
        let window_sees: Vec<i64> = pending
            .all()
            .iter()
            .filter(|a| a.agent == "stray")
            .map(|a| a.id)
            .collect();
        assert_eq!(window_sees, vec![3], "the window sees only its own agent");

        // This tab is the complement: exactly the two rows neither surface
        // shows, still-pending only (id 4 is excluded on status, not agent).
        let here = unassigned_approvals(queue, &known);
        assert_eq!(
            here.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![1, 2],
            "{here:?}"
        );
    }

    /// A roster agent's queue does not leak into "unassigned" — the filter's
    /// other arm, pinned on its own so a mutation that always returns `true`
    /// (or drops the roster check entirely) cannot pass by accident.
    #[test]
    fn a_roster_agents_approval_is_not_unassigned() {
        let queue = vec![approval(1, "stray", ApprovalStatus::Pending)];
        let here = unassigned_approvals(queue, &["stray".to_owned()]);
        assert!(here.is_empty(), "{here:?}");
    }

    /// **Each row is sent somewhere that can actually exist** (this round's
    /// review, LOW 5), and the destination is the row's **first** line (LOW
    /// 4) so a long description cannot ellipsize away the one sentence the
    /// row is for.
    ///
    /// The middle case is the one the review caught: `hive_of` drops any
    /// roster row whose name fails `AgentName::parse`, so an approval naming
    /// `Bad Name` lands here *and* the sidebar will never grow a card for it —
    /// "answer from that agent's own sidebar card if it returns" was advice
    /// that could not be taken.
    ///
    /// Mutation (verified red): drop the `AgentName::parse` arm and the
    /// illegal name is told to wait for a card again; put `detail_line` back
    /// in front and the ordering assertion reds.
    #[test]
    fn an_unrenderable_agent_name_is_sent_to_the_dashboard_not_to_a_card() {
        assert_eq!(
            unassigned_destination(""),
            "no agent named on this request — answer from the hive's dashboard"
        );
        let illegal = unassigned_destination("Bad Name");
        assert!(
            illegal.contains("dashboard") && !illegal.contains("card"),
            "a name this build will not render has no card to wait for: {illegal}"
        );
        let stale = unassigned_destination("ghost");
        assert!(
            stale.contains("card") && stale.contains("dashboard"),
            "a legal name that is merely absent keeps both routes: {stale}"
        );

        let row = Approval {
            description: Some("a very long description the manager wrote".to_owned()),
            ..approval(1, "ghost", ApprovalStatus::Pending)
        };
        let subtitle = unassigned_subtitle(&row);
        let (first, rest) = subtitle
            .split_once('\n')
            .expect("the subtitle is two lines");
        assert_eq!(first, stale, "the destination is the first line");
        assert!(
            rest.contains("a very long description the manager wrote"),
            "the manager's own detail line follows it: {rest}"
        );
    }
}

#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use adw::prelude::*;
    use gtk::glib;

    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::{
        ABSENT, Actions, AgentsState, AgentsView, FACT_LABELS, apply_unassigned, build_tab,
        flags_of_labels, launch_detached, on_status, refresh, start_poll,
    };
    use hytte_plugin_agents::config::{AgentsConfig, Display};
    use hytte_plugin_agents::hive::client::HiveError;
    use hytte_plugin_agents::hive::wire::{
        AgentStatusRow, Approval, ApprovalStatus, HOST_SOCK_VERSION, Response,
    };
    use hytte_plugin_agents::window as agent_window;

    /// Run the GTK main loop until it has nothing left to dispatch, so a
    /// queued resize/allocation actually happens.
    fn pump() {
        while glib::MainContext::default().iteration(false) {}
    }

    /// Pump until `done` or until `secs` have passed — for the tests that
    /// drive a **real** socket, where the answer arrives on the shared
    /// `hytte-reactive` runtime and comes back through
    /// [`crate::spawn_on_runtime`]'s oneshot.
    ///
    /// Deliberately a non-blocking iteration plus a short sleep rather than
    /// `iteration(true)`: a blocking iteration with nothing pending would park
    /// the test forever if the mechanism under test never fires, which is the
    /// failure a falsification run is *supposed* to produce as a red, not as a
    /// hang.
    fn pump_until(done: impl Fn() -> bool, secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !done() && Instant::now() < deadline {
            pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        pump();
    }

    /// A scripted `host.sock` in a tempdir — the shape `tests::scripted` uses,
    /// lifted here because these tests drive the tab's own poll rather than
    /// the client directly.
    ///
    /// Each connection is served on its own task (so a slow answer does not
    /// stop the next request from being *read*, which is exactly what the
    /// in-flight guard has to be measured against), answered with the reply
    /// the script picks for that request line, after that reply's delay.
    struct Scripted {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Scripted {
        /// The request lines the daemon has actually read, in arrival order.
        fn seen(&self) -> Vec<String> {
            self.seen.lock().expect("the script mutex").clone()
        }

        /// A config naming this socket. Built by hand — nothing here reads the
        /// real `$XDG_CONFIG_HOME` (#1101).
        fn cfg(&self) -> AgentsConfig {
            AgentsConfig {
                socket: self.path.to_string_lossy().into_owned(),
                ..AgentsConfig::default()
            }
        }
    }

    /// Bind a scripted socket whose answer to the *n*-th request is
    /// `reply(n, request_line)` — `None` to hang up without answering.
    fn scripted(
        reply: impl Fn(usize, &str) -> Option<(Duration, String)> + Send + Sync + 'static,
    ) -> Scripted {
        let dir = tempfile::tempdir().expect("a tempdir");
        let path = dir.path().join("host.sock");
        let runtime = hytte_reactive::runtime::handle();
        // Binding a `UnixListener` registers it with a reactor, and a
        // `#[gtk::test]` thread is in none — so the bind happens inside the
        // shared runtime's context, the same one the tab's own client uses.
        let listener = {
            let _guard = runtime.enter();
            tokio::net::UnixListener::bind(&path).expect("bind")
        };
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let reply = Arc::new(reply);
        let served = seen.clone();
        runtime.spawn(async move {
            let mut n = 0usize;
            while let Ok((stream, _)) = listener.accept().await {
                let reply = reply.clone();
                let served = served.clone();
                let index = n;
                n += 1;
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut reader = tokio::io::BufReader::new(read);
                    let mut line = String::new();
                    if tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
                        .await
                        .is_err()
                    {
                        return;
                    }
                    served.lock().expect("the script mutex").push(line.clone());
                    if let Some((delay, answer)) = reply(index, line.trim()) {
                        tokio::time::sleep(delay).await;
                        let _ = tokio::io::AsyncWriteExt::write_all(&mut write, answer.as_bytes())
                            .await;
                    }
                });
            }
        });

        Scripted {
            _dir: dir,
            path,
            seen,
        }
    }

    /// How many `AgentStatus` round trips the daemon has been asked for — the
    /// `Urls` request rides a good answer and is counted separately.
    fn rosters_asked(hive: &Scripted) -> usize {
        hive.seen()
            .iter()
            .filter(|line| line.contains("agent_status"))
            .count()
    }

    /// One `AgentStatus` answer line carrying exactly these agents.
    fn roster_line(names: &[&str]) -> String {
        let rows: Vec<String> = names
            .iter()
            .map(|n| format!("{{\"name\":\"{n}\",\"running\":true}}"))
            .collect();
        format!(
            "{{\"version\":1,\"ok\":true,\"agent_statuses\":[{}]}}\n",
            rows.join(",")
        )
    }

    /// Install a recording pair of [`Actions`] and hand back what they saw.
    ///
    /// `launch_result` is what the recorded launcher returns, so one helper
    /// covers both the happy route tests and the failed-launch one.
    fn record(state: &AgentsState, launch_result: Result<(), String>) -> Recorded {
        let launched: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
        let opened: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let (l, o) = (launched.clone(), opened.clone());
        *state.actions.borrow_mut() = Actions {
            launch: Rc::new(move |argv| {
                l.borrow_mut().push(argv.to_vec());
                launch_result.clone()
            }),
            open_uri: Rc::new(move |uri| o.borrow_mut().push(uri.to_owned())),
        };
        Recorded { launched, opened }
    }

    /// What a recording [`Actions`] saw.
    struct Recorded {
        launched: Rc<RefCell<Vec<Vec<String>>>>,
        opened: Rc<RefCell<Vec<String>>>,
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

    /// One `Pending` fixture row.
    fn approval(id: i64, agent: &str, status: ApprovalStatus) -> Approval {
        Approval {
            id,
            agent: agent.to_owned(),
            status,
            ..Approval::default()
        }
    }

    /// Every "Unassigned approvals" row's `title: subtitle`.
    fn unassigned_row_text(state: &AgentsState) -> Vec<String> {
        state
            .unassigned_rows
            .borrow()
            .iter()
            .map(|r| format!("{}: {}", r.title(), r.subtitle().unwrap_or_default()))
            .collect()
    }

    /// **An orphan approval surfaces here, and nowhere else** (#1149 N1): the
    /// group is hidden with nothing queued, shows exactly the rows neither
    /// per-agent surface can (an absent agent, an agent this roster does not
    /// carry) and none of the roster's own, and clears itself the moment the
    /// hive refuses the queue — the "cannot vouch for it" rule
    /// `apply_unassigned`'s doc states, mirroring what both per-agent
    /// approval surfaces already do on the same refusal.
    ///
    /// Mutation (verified red): drop the `!known.iter().any(…)` arm in
    /// `unassigned_approvals` and the row count assertion reds (the "ghost"
    /// row stops appearing, or if the `agent.trim().is_empty()` arm is
    /// dropped instead, the "no agent" row does); replace the `Err` arm's
    /// `Vec::new()` in `apply_unassigned` with keeping the old rows and the
    /// group stays visible after the refusal.
    #[gtk::test]
    fn an_orphan_approval_populates_the_group_and_clears_on_refusal() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);

        apply(&state, &["stray"]);
        assert!(
            !state.unassigned_group.is_visible(),
            "nothing queued yet — the group must not show empty"
        );

        apply_unassigned(
            &state,
            Some(Ok(vec![
                approval(1, "", ApprovalStatus::Pending),
                approval(2, "ghost", ApprovalStatus::Pending),
                approval(3, "stray", ApprovalStatus::Pending),
            ])),
        );
        pump();

        assert!(
            state.unassigned_group.is_visible(),
            "two rows are unassigned; the group must show"
        );
        let text = unassigned_row_text(&state);
        assert_eq!(
            text.len(),
            2,
            "the roster's own row (id 3) must not leak in: {text:?}"
        );

        apply_unassigned(&state, Some(Err("permission denied".to_owned())));
        pump();
        assert!(
            !state.unassigned_group.is_visible(),
            "a hive that refuses the queue must not leave stale rows showing"
        );
        assert!(unassigned_row_text(&state).is_empty());

        dismiss(&window);
    }

    /// **A status poll that never answered asks for nothing** (`refresh`'s
    /// own doc): `apply_unassigned(state, None)` is what a failed
    /// `AgentStatus` feeds it, and it must clear the group rather than leave
    /// whatever the last successful tick drew.
    #[gtk::test]
    fn a_never_asked_tick_clears_the_group_rather_than_freezing_it() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);

        apply(&state, &["stray"]);
        apply_unassigned(
            &state,
            Some(Ok(vec![approval(1, "ghost", ApprovalStatus::Pending)])),
        );
        pump();
        assert!(state.unassigned_group.is_visible());

        apply_unassigned(&state, None);
        pump();
        assert!(
            !state.unassigned_group.is_visible(),
            "a tick that never asked must not leave the last answer's rows on screen"
        );

        dismiss(&window);
    }

    /// Put the tab in a window `width` × `height` px and let GTK allocate it.
    fn present_sized(bin: &adw::BreakpointBin, width: i32, height: i32) -> gtk::Window {
        let window = gtk::Window::new();
        window.set_child(Some(bin));
        window.set_default_size(width, height);
        window.present();
        pump();
        window
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
        state
            .list
            .select_row(Some(row.upcast_ref::<gtk::ListBoxRow>()));
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
        state
            .list
            .select_row(Some(row.upcast_ref::<gtk::ListBoxRow>()));
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
        assert!(description.contains("/run/test/host.sock"), "{description}");
        assert_eq!(
            state.detail.stack.visible_child_name().as_deref(),
            Some("empty")
        );
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
        state
            .list
            .select_row(Some(row.upcast_ref::<gtk::ListBoxRow>()));
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

    // ── the hive's own strings are text, never markup ───────────────────────

    /// **Nothing off the wire is parsed as Pango markup.** A bare `&` — "R&D",
    /// any URL with a query string — makes Pango fail the whole label and
    /// render it blank, and a well-formed `<span …>` would be an injection
    /// channel from text the *agent itself* writes into the settings app's
    /// chrome (#1147 review, HIGH 1).
    ///
    /// Both halves of the rule are asserted: the mechanism (`use-markup` is
    /// off on every row this file builds, which is what stops the parse) and
    /// the outcome (the string on the row is the hive's, verbatim). The
    /// mechanism assertion is the load-bearing one — the getters return what
    /// was set either way, so only `uses_markup` can tell a rendered label
    /// from a blank one.
    ///
    /// Mutations (run, verified red): drop `.use_markup(false)` from
    /// `build_agent_row` (the sidebar assertion reds), from the fact rows (the
    /// fact assertion reds), from `link_row` (the link assertion reds), from
    /// the placeholder row (its assertion reds), and drop the
    /// `markup_escape_text` in `set_placeholder` (the status-page assertion
    /// reds).
    #[gtk::test]
    fn wire_text_is_never_parsed_as_markup() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);
        let hostile = "R&D <b>done</b>";
        on_status(
            &state,
            &Ok(Response {
                version: HOST_SOCK_VERSION,
                ok: true,
                agent_statuses: Some(vec![AgentStatusRow {
                    status_text: Some(hostile.to_owned()),
                    url: Some("https://hive.example/a/argus?tab=turns&full=1".to_owned()),
                    ..row("argus")
                }]),
                ..Response::default()
            }),
        );
        pump();

        let sidebar = state.by_name.borrow()["argus"].row.clone();
        assert!(
            !sidebar.uses_markup(),
            "the sidebar row parses the hive's text as markup"
        );
        assert_eq!(sidebar.subtitle().as_deref(), Some(hostile));

        assert!(
            !state.detail.facts[0].uses_markup(),
            "the fact rows parse the hive's text as markup"
        );
        assert_eq!(fact_values(&state)[0], hostile);

        assert!(
            !state.detail.agent_page.uses_markup(),
            "the link rows parse the hive's URL as markup"
        );
        assert!(!state.detail.config_repo.uses_markup());
        assert_eq!(
            state.detail.agent_page.subtitle().as_deref(),
            Some("https://hive.example/a/argus?tab=turns&full=1"),
            "the query string's & survived"
        );

        // …and the one surface with no `use-markup` switch: escaped instead.
        on_status(
            &state,
            &Err(HiveError::Unreachable {
                reason: hostile.to_owned(),
            }),
        );
        pump();
        let expected = format!("{hostile} (/run/test/host.sock)");
        assert_eq!(
            state
                .detail
                .empty
                .description()
                .unwrap_or_default()
                .as_str(),
            glib::markup_escape_text(&expected).as_str(),
            "the status page's description is not escaped"
        );
        let placeholder = state.rows.borrow()[0]
            .clone()
            .downcast::<adw::ActionRow>()
            .expect("the placeholder is an ActionRow");
        assert!(!placeholder.uses_markup());
        assert_eq!(placeholder.subtitle().as_deref(), Some(expected.as_str()));
        dismiss(&window);
    }

    // ── a reorder reaches the screen ────────────────────────────────────────

    /// A poll whose roster is the **same set in a different order** must
    /// reorder the rows on screen — without rebuilding them and without moving
    /// the selection.
    ///
    /// [`super::same_agent_set`] is set equality by design, so the in-place
    /// branch runs; before #1147's review (its MEDIUM 5) that branch rewrote
    /// each row's text by name and never touched the list's child order, so
    /// the sidebar kept the order of the poll that last changed *membership*
    /// — and the card, which re-renders, would disagree the moment hyperhive
    /// shuffled its answer.
    ///
    /// Mutation (run, verified red): delete the `reorder_rows` call from
    /// `apply`'s same-set branch and the second `row_titles` assertion reds
    /// with the reviewer's own left/right (`["abe","mid","zed"]` on screen vs.
    /// `["zed","abe","mid"]` asked for).
    #[gtk::test]
    fn a_reorder_reaches_the_screen_without_rebuilding_a_row() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);
        apply(&state, &["abe", "mid", "zed"]);
        assert_eq!(row_titles(&state), ["abe", "mid", "zed"]);

        // Select the middle one, so the reorder has a selection to preserve.
        let mid = state.by_name.borrow()["mid"].row.clone();
        state
            .list
            .select_row(Some(mid.upcast_ref::<gtk::ListBoxRow>()));
        pump();
        let abe = state.by_name.borrow()["abe"].row.clone();

        apply(&state, &["zed", "abe", "mid"]);
        assert_eq!(
            row_titles(&state),
            ["zed", "abe", "mid"],
            "the sidebar kept the previous poll's order"
        );
        assert!(
            state.by_name.borrow()["abe"].row == abe,
            "a reorder rebuilt a row instead of moving it"
        );
        assert_eq!(
            state.selected.borrow().as_deref(),
            Some("mid"),
            "the reorder moved the selection"
        );
        assert!(
            state
                .list
                .selected_row()
                .is_some_and(|r| &r == mid.upcast_ref::<gtk::ListBoxRow>()),
            "the selected row on screen is not the selected agent"
        );
        dismiss(&window);
    }

    // ── which surface a click opens ─────────────────────────────────────────

    /// The route, end to end through the row the operator actually clicks:
    /// the companion window when it is installed — **with no `url` on the
    /// wire**, which is every agent on the hyperhive revision in the tree —
    /// and the browser only when it is not.
    ///
    /// Mutations (run, verified red): invert `open_agent_page`'s
    /// `installed` branch (both halves red); gate `route_for`'s `Window` arm
    /// on `url.is_some()` (the first half reds, and so does the sensitivity
    /// assertion — HIGH 3's own case).
    #[gtk::test]
    fn the_probe_decides_which_surface_a_click_opens() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);
        let seen = record(&state, Ok(()));

        *state.probe.borrow_mut() = agent_window::Probe::fixed(true);
        apply(&state, &["argus"]);
        assert!(
            state.detail.agent_page.is_sensitive(),
            "the companion window cannot be opened when the hive publishes no url"
        );
        assert_eq!(
            state.detail.agent_page.subtitle().as_deref(),
            Some(ABSENT),
            "an absent url still reads as absent"
        );
        adw::prelude::ActionRowExt::activate(&state.detail.agent_page);
        pump();
        assert_eq!(
            seen.launched.borrow().as_slice(),
            [vec![
                "trollshell-agent-window".to_owned(),
                "--agent".to_owned(),
                "argus".to_owned(),
            ]]
        );
        assert!(
            seen.opened.borrow().is_empty(),
            "the browser was opened too"
        );

        // No window on this desktop: the browser, and only where the hive
        // named a destination.
        *state.probe.borrow_mut() = agent_window::Probe::fixed(false);
        on_status(
            &state,
            &Ok(Response {
                version: HOST_SOCK_VERSION,
                ok: true,
                agent_statuses: Some(vec![AgentStatusRow {
                    url: Some("https://hive.example/a/argus".to_owned()),
                    ..row("argus")
                }]),
                ..Response::default()
            }),
        );
        pump();
        adw::prelude::ActionRowExt::activate(&state.detail.agent_page);
        pump();
        assert_eq!(
            seen.opened.borrow().as_slice(),
            ["https://hive.example/a/argus".to_owned()]
        );
        assert_eq!(seen.launched.borrow().len(), 1, "no second launch");
        dismiss(&window);
    }

    /// A launch that **fails** is not silent: it toasts, it re-resolves the
    /// probe (whose answer may be hours old, and is now known wrong), and it
    /// falls back to the browser where the hive named one — the single case
    /// where "one click, one destination" is already broken, because the probe
    /// promised the binary was there (#1147 review, MEDIUM 7).
    ///
    /// Mutations (run, verified red): drop the `add_toast` (the overlay-child
    /// assertion reds); drop the `open(&url)` fallback (the browser assertion
    /// reds); drop the `*probe = Probe::path()` line (the probe assertion reds
    /// wherever `trollshell-agent-window` is **not** on `PATH`, which is every
    /// CI environment and this devShell — a machine with the window installed
    /// would make that one assertion vacuous, which is why it is stated as an
    /// equality against [`agent_window::on_path`] rather than a bare `false`).
    #[gtk::test]
    fn a_failed_launch_toasts_falls_back_and_reresolves_the_probe() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);
        let seen = record(&state, Err("no such file or directory".to_owned()));
        *state.probe.borrow_mut() = agent_window::Probe::fixed(true);

        on_status(
            &state,
            &Ok(Response {
                version: HOST_SOCK_VERSION,
                ok: true,
                agent_statuses: Some(vec![AgentStatusRow {
                    url: Some("https://hive.example/a/argus".to_owned()),
                    ..row("argus")
                }]),
                ..Response::default()
            }),
        );
        pump();

        let before = overlay_children(&state);
        adw::prelude::ActionRowExt::activate(&state.detail.agent_page);
        pump();

        assert_eq!(seen.launched.borrow().len(), 1, "the launch was attempted");
        assert!(
            overlay_children(&state) > before,
            "a failed launch raised no toast"
        );
        assert_eq!(
            seen.opened.borrow().as_slice(),
            ["https://hive.example/a/argus".to_owned()],
            "a failed launch did not fall back to the URL the hive named"
        );
        assert_eq!(
            state.probe.borrow_mut().available(),
            agent_window::on_path(),
            "the pinned probe survived a launch that proved it wrong"
        );
        dismiss(&window);
    }

    /// The exact #1147 review N1 scenario: `systemd-run --user` finds a
    /// missing target binary and exits 1 — *after* `gio::Subprocess::newv`
    /// has already succeeded. Before the N1 fix, [`launch_detached`] read only
    /// `newv`'s result and returned `Ok(())` here, which is the silent dead
    /// click M7 was filed about.
    ///
    /// This one **stays red-on-mutation regardless of the environment the
    /// test runs in**: with a live `systemd --user` manager (this box),
    /// `systemd-run` itself reports "Failed to find executable" and
    /// `user_manager_unreachable` correctly says no, so `launch_detached`
    /// never takes the fallback and returns the manager's own `Err`. Without
    /// one (CI's nix sandbox), `systemd-run` fails earlier on the bus-connect
    /// error, `user_manager_unreachable` says yes, and `launch_detached`
    /// *does* fall back — to `spawn`-ing `/nonexistent-1147-review-n1`
    /// directly, which fails for the same underlying reason (the path does
    /// not exist) and is still an `Err`. Either environment, this assertion
    /// holds. `system-tests`-gated because it needs a real `systemd-run` on
    /// `$PATH`; needs no display, so it is a plain `#[test]`, not
    /// `#[gtk::test]`.
    #[test]
    fn a_launch_that_fails_inside_systemd_run_is_a_failed_launch() {
        let result = launch_detached(&["/nonexistent-1147-review-n1".to_owned()]);
        assert!(
            result.is_err(),
            "systemd-run's own non-zero exit was not surfaced as a failed launch"
        );
    }

    /// The mirror of the test above: a target `systemd-run` *can* start is a
    /// successful launch, so the fix does not turn every launch into a
    /// failure.
    ///
    /// This is the test the fallback in [`launch_detached`] exists for: with a
    /// live `systemd --user` manager (this box), `systemd-run --user --
    /// true` starts the transient unit and this succeeds via the **normal**
    /// path (`child.is_successful()`). Without a user manager (CI's nix
    /// sandbox, or a manual run under `DBUS_SESSION_BUS_ADDRESS=/nonexistent
    /// XDG_RUNTIME_DIR=/nonexistent`), `systemd-run` itself fails to connect
    /// to the bus before ever asking to start anything, and this succeeds via
    /// the **fallback** path instead (`user_manager_unreachable` says yes,
    /// `spawn` runs `true` directly). Before this fix, the fallback branch did
    /// not exist and this test went red in exactly that second environment —
    /// which is what CI's `system-tests` check runs under (#1082: `systemd`
    /// on `$PATH`, no `systemd --user`).
    #[test]
    fn a_launch_that_succeeds_inside_systemd_run_is_a_launch() {
        let result = launch_detached(&["true".to_owned()]);
        assert!(result.is_ok(), "{result:?}");
    }

    /// Every widget the overlay holds, including the toasts it is showing —
    /// `AdwToastOverlay` exposes no accessor for its queue, so the count of
    /// its children is the observable.
    fn overlay_children(state: &AgentsState) -> usize {
        let mut n = 0;
        let mut child = state.toasts.first_child();
        while let Some(c) = child {
            n += 1;
            child = c.next_sibling();
        }
        n
    }

    // ── the socket, driven through the tab's own poll ───────────────────────

    /// The `Urls` fetch's whole **runtime** path: a real round trip lands, the
    /// answer reaches `state.urls`, and the **Config repo** row goes live with
    /// the hive's own forge URL.
    ///
    /// Before #1147's review only `detail_of`'s pure mapping was covered —
    /// making `refresh_urls` return early immediately passed the entire suite
    /// (its MEDIUM 4).
    ///
    /// Mutation (run, verified red): `return` at the top of `refresh_urls`, or
    /// drop the `refresh_urls` call from `on_status`, and both assertions red.
    #[gtk::test]
    fn the_urls_answer_lights_the_config_repo_row() {
        let forge = "https://forge.example/hive?ref=main&x=1";
        let hive = scripted(move |_, line| {
            let answer = if line.contains("urls") {
                format!("{{\"version\":1,\"ok\":true,\"urls\":{{\"forge\":\"{forge}\"}}}}\n")
            } else {
                roster_line(&["argus"])
            };
            Some((Duration::ZERO, answer))
        });
        let (bin, state) = build_tab(hive.cfg());
        let window = present(&bin, 900);

        // One roster answer, applied the way a real poll applies it — that is
        // what issues the `Urls` request.
        apply(&state, &["argus"]);
        pump_until(|| state.urls.borrow().is_some(), 10);

        assert!(
            state.urls.borrow().is_some(),
            "the Urls answer never landed: {:?}",
            hive.seen()
        );
        assert!(state.detail.config_repo.is_sensitive());
        assert_eq!(state.detail.config_repo.subtitle().as_deref(), Some(forge));

        // …and it is asked exactly once, however many good polls follow —
        // including one that answers with no `urls` block at all would be
        // enough to stop it (#1147 review, LOW 8).
        let asked = hive.seen().iter().filter(|l| l.contains("urls")).count();
        apply(&state, &["argus"]);
        apply(&state, &["argus"]);
        pump_until(|| false, 1);
        assert_eq!(
            hive.seen().iter().filter(|l| l.contains("urls")).count(),
            asked,
            "the Urls request is repeated on later polls"
        );
        dismiss(&window);
    }

    /// A hive that answers `Urls` **without a `urls` block** is asked once,
    /// not once per tick.
    ///
    /// The earlier shape cleared the want only inside `Some(urls)`, so
    /// `ok: true, urls: null` — or any handler that returns an empty block —
    /// re-dialled for the life of the window, doubling the tab's socket
    /// traffic with no backoff: the exact thing the gate exists to prevent for
    /// the down case (#1147 review, LOW 8).
    ///
    /// Mutation (run, verified red): put the want back when the answer has no
    /// block (`if resp.urls.is_none() { state.urls_wanted.set(true) }`) and
    /// this reds at three requests.
    #[gtk::test]
    fn a_hive_with_no_urls_block_is_asked_once() {
        let hive = scripted(|_, line| {
            let answer = if line.contains("urls") {
                "{\"version\":1,\"ok\":true}\n".to_owned()
            } else {
                roster_line(&["argus"])
            };
            Some((Duration::ZERO, answer))
        });
        let (bin, state) = build_tab(hive.cfg());
        let window = present(&bin, 900);

        let asked = || hive.seen().iter().filter(|l| l.contains("urls")).count();
        apply(&state, &["argus"]);
        pump_until(|| asked() >= 1, 10);
        // …and settle, so the *answer* has been folded in before the next
        // polls run: a want put back by the answer has to be able to show up.
        pump_until(|| false, 1);
        assert_eq!(asked(), 1, "the first good poll asks once");

        apply(&state, &["argus"]);
        apply(&state, &["argus"]);
        pump_until(|| asked() > 1, 1);
        assert_eq!(
            asked(),
            1,
            "a hive with no urls block is re-dialled forever"
        );

        assert!(
            !state.detail.config_repo.is_sensitive(),
            "an absent forge is still a dead row, not a guess"
        );
        dismiss(&window);
    }

    /// **A slow hive must not stack round trips.** With the default 2 s
    /// cadence against a 5 s client timeout, an unguarded poll has two or
    /// three `AgentStatus` requests outstanding at once and they resolve in
    /// completion order — so a timeout issued at t=0 lands after a good answer
    /// issued at t=4 s and the roster flips to "unreachable" and back (#1147
    /// review, HIGH 2).
    ///
    /// The guard makes that unrepresentable rather than merely unlikely: a
    /// tick with a request outstanding is skipped, so there is never a second
    /// answer to arrive out of order. Measured on the daemon's side — the
    /// script serves each connection on its own task, so a second dial would
    /// be *read* immediately and show up in `seen()`.
    ///
    /// Mutation (run, verified red): drop the `claim()` guard from `refresh`
    /// and the request-count assertion reds (three dials, not one).
    #[gtk::test]
    fn a_slow_hive_never_stacks_round_trips() {
        let hive = scripted(|n, _| {
            if n == 0 {
                Some((Duration::from_millis(300), roster_line(&["stale"])))
            } else {
                Some((Duration::ZERO, roster_line(&["fresh"])))
            }
        });
        let (bin, state) = build_tab(hive.cfg());
        let window = present(&bin, 900);

        refresh(&state); // issues the slow one
        refresh(&state); // must be skipped…
        refresh(&state); // …and so must this
        pump_until(|| row_titles(&state) == ["stale"], 10);

        assert_eq!(
            rosters_asked(&hive),
            1,
            "a slow hive stacked round trips: {:?}",
            hive.seen()
        );
        assert_eq!(row_titles(&state), ["stale"]);

        // The slot is released with the answer, so the next tick does dial —
        // and its newer answer is the one that lands.
        refresh(&state);
        pump_until(|| row_titles(&state) == ["fresh"], 10);
        assert_eq!(row_titles(&state), ["fresh"]);
        assert_eq!(rosters_asked(&hive), 2);
        dismiss(&window);
    }

    /// The poll **keeps** polling. `build_page`'s timer returns
    /// `glib::ControlFlow::Continue`; turning that into `Break` — a tab that
    /// asks once and then never again for the life of the window — passed the
    /// whole suite before this test existed (#1147 review, MEDIUM 4).
    ///
    /// Driven against a real socket rather than a counter, because the thing
    /// worth asserting is that the hive is *asked* again.
    ///
    /// Mutation (run, verified red): `glib::ControlFlow::Break` in
    /// [`start_poll`] and this reds at one request.
    #[gtk::test]
    fn the_poll_keeps_asking_the_hive() {
        let hive = scripted(|_, _| Some((Duration::ZERO, roster_line(&["argus"]))));
        let (bin, state) = build_tab(hive.cfg());
        let window = present(&bin, 900);

        let poll = start_poll(&state, Duration::from_millis(20));
        pump_until(|| rosters_asked(&hive) >= 3, 10);
        poll.remove();

        assert!(
            rosters_asked(&hive) >= 3,
            "the timer stopped after {} roster request(s)",
            rosters_asked(&hive)
        );
        dismiss(&window);
    }

    /// **A tab nobody is looking at asks the hive nothing**, and coming back
    /// reconciles at once (this round's review, MEDIUM 3).
    ///
    /// `start_poll` used to dial every `poll_seconds` for the whole life of
    /// the control-center whatever was on screen, and #1149 N1 doubled what
    /// each of those ticks costs by adding `Pending` to it. The gate is the
    /// companion window's — `GdkToplevelState::SUSPENDED` plus map/unmap — and
    /// on a tab the map half is the load-bearing one: `AdwViewStack` maps only
    /// its visible child, so "another tab is showing" is an unmap. That is the
    /// state driven here, with a plain `GtkStack` standing in for the view
    /// stack, because it is the case the review is actually about and it needs
    /// no compositor. (The `SUSPENDED` half has no setter and no compositor in
    /// CI — `docs/live-verify.md` carries it.)
    ///
    /// The absence is measured **after** letting anything already in flight
    /// land, then over 25 intervals' worth of main loop, so it is a parked
    /// timer rather than a slow one.
    ///
    /// Mutation (run, verified red): drop the `presentation.on` check in
    /// [`start_poll`] and the parked assertion reds with a double-digit
    /// request count.
    #[gtk::test]
    fn the_poll_parks_while_another_tab_is_showing() {
        let hive = scripted(|_, _| Some((Duration::ZERO, roster_line(&["argus"]))));
        let (bin, state) = build_tab(hive.cfg());

        let stack = gtk::Stack::new();
        stack.add_named(&bin, Some("agents"));
        stack.add_named(&gtk::Label::new(Some("another tab")), Some("other"));
        let window = gtk::Window::new();
        window.set_child(Some(&stack));
        window.set_default_size(900, 400);
        window.present();
        pump();

        let poll = start_poll(&state, Duration::from_millis(20));
        pump_until(|| rosters_asked(&hive) >= 2, 10);
        assert!(
            state.presentation.on.get(),
            "the visible child of the stack must read as presented"
        );

        stack.set_visible_child_name("other");
        pump();
        assert!(
            !state.presentation.on.get(),
            "another tab showing must park the poll"
        );
        // Let a round trip issued just before the switch land, so the count
        // below is a baseline and not a race.
        pump_for(Duration::from_millis(100));
        let parked_at = rosters_asked(&hive);
        pump_for(Duration::from_millis(500));
        assert_eq!(
            rosters_asked(&hive),
            parked_at,
            "a parked tab must ask the hive nothing over 25 intervals: {:?}",
            hive.seen()
        );

        // …and coming back reconciles immediately rather than waiting out an
        // interval — the tab's data must not be one cadence stale the moment
        // it is looked at.
        stack.set_visible_child_name("agents");
        pump_until(|| rosters_asked(&hive) > parked_at, 10);
        assert!(
            rosters_asked(&hive) > parked_at,
            "the tab came back and never re-asked: {:?}",
            hive.seen()
        );
        poll.remove();
        dismiss(&window);
    }

    /// Drive the main loop for `how_long`, dispatching whatever comes up.
    ///
    /// For the absence assertions: `pump_until` stops at its predicate, and
    /// "nothing happened" needs the opposite — a bounded stretch of real main
    /// loop with real timer ticks in it.
    fn pump_for(how_long: Duration) {
        let deadline = Instant::now() + how_long;
        while Instant::now() < deadline {
            pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A round trip that **fails** still releases the in-flight slot — the
    /// production code drops it unconditionally, but a one-slot guard whose
    /// release is conditional is a permanent freeze, and nothing else in the
    /// suite drives a hive that ever answers with an error (#1147 review,
    /// MEDIUM/coverage N3).
    ///
    /// `cfg()`'s socket does not exist, so this drives the real failure path
    /// — a connection refusal — rather than a scripted one.
    ///
    /// Mutation (run, verified red): leak the slot on any failed round trip
    /// (`if answer.is_ok() { drop(slot); } else { std::mem::forget(slot); }`)
    /// and the second assertion reds — the tab never polls again.
    #[gtk::test]
    fn a_failed_round_trip_releases_the_slot() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 900);

        refresh(&state);
        assert!(state.in_flight.get(), "the slot was not claimed at all");
        pump_until(|| !state.in_flight.get(), 5);
        assert!(
            !state.in_flight.get(),
            "a failed round trip never released the in-flight slot"
        );
        dismiss(&window);
    }

    // ── geometry (#851) ─────────────────────────────────────────────────────

    /// Every sidebar row is allocated **inside** the scroller that owns it, at
    /// the bin's own 360 px floor.
    ///
    /// Property assertions cannot see this class of bug: #851 shipped a chip
    /// drawn 250 px outside its `OVERFLOW_HIDDEN` bin with both of its
    /// geometry tests green, because both asserted state rather than
    /// allocation. This asserts the allocation.
    ///
    /// Mutation (run, verified red): give the sidebar a
    /// `set_margin_start(400)` — the rows are then allocated past the
    /// scroller's right edge and the containment assertion reds.
    #[gtk::test]
    fn the_sidebar_rows_are_allocated_inside_their_scroller() {
        let (bin, state) = build_tab(cfg());
        let window = present(&bin, 360);
        apply(&state, &["argus", "beta", "gamma"]);
        // An allocation is handed out on a frame-clock tick, not by draining
        // the queue — so this waits for one rather than assuming `pump` was
        // enough.
        pump_until(|| state.by_name.borrow()["gamma"].row.height() > 0, 5);

        let scroller = state
            .list
            .ancestor(gtk::ScrolledWindow::static_type())
            .expect("the sidebar list lives in a scroller");
        assert!(scroller.width() > 0, "the scroller was never allocated");

        for name in ["argus", "beta", "gamma"] {
            let row = state.by_name.borrow()[name].row.clone();
            let bounds = row
                .compute_bounds(&scroller)
                .unwrap_or_else(|| panic!("{name}'s row has no bounds in the scroller"));
            assert!(
                bounds.width() > 0.0 && bounds.height() > 0.0,
                "{name}'s row is allocated empty: {bounds:?}"
            );
            // The allocation is in pixels and fits an f32 exactly at any size
            // a window has; the cast is the only way to compare it with a
            // `graphene::Rect`.
            #[allow(clippy::cast_precision_loss)]
            let (w, h) = (scroller.width() as f32, scroller.height() as f32);
            assert!(
                bounds.x() >= -0.5
                    && bounds.y() >= -0.5
                    && bounds.x() + bounds.width() <= w + 0.5
                    && bounds.y() + bounds.height() <= h + 0.5,
                "{name}'s row is drawn outside its scroller: {bounds:?} in {w}×{h}"
            );
        }
        dismiss(&window);
    }

    /// **A full Unassigned queue scrolls; it does not push rows past the
    /// window, and it does not crush the roster** (#1149 N1, this round's
    /// review HIGH).
    ///
    /// The group used to sit in a plain `Box` under the roster scroller with
    /// no scroller and no cap of its own, so each ~84 px row came out of the
    /// roster's height until the roster hit its floor, after which the group's
    /// own rows were allocated **below the window** with no scrollbar to reach
    /// them — at the control-center's own default size, from about five rows.
    /// That is N1's own trigger: one agent renamed out of `agents.toml`
    /// orphans its whole queue at once.
    ///
    /// The existing behaviour test cannot see any of this — it asserts
    /// `unassigned_group.is_visible()` and reads text back out of
    /// `state.unassigned_rows`, i.e. its own bookkeeping. #851 is the standing
    /// lesson: `is_visible()` is orthogonal to being on screen, so this
    /// asserts the **allocation**, against the window.
    ///
    /// Three claims, because "inside the window" alone would pass for a group
    /// squeezed to nothing or one whose rows are simply unreachable:
    /// the scrolled container is inside the window, the rows past the cap are
    /// reachable (`upper > page_size`, i.e. there is something to scroll to
    /// rather than content quietly lost), and the roster still has room for
    /// more than one agent row.
    ///
    /// Mutation (run, verified red): mount `unassigned_group` straight into
    /// `sidebar_box` again, with no `ScrolledWindow` — the containment
    /// assertion reds, and in the sharpest possible way: twelve rows' natural
    /// height cannot be fitted into the sidebar at all, so GTK leaves the
    /// group **without a usable allocation in the window**, which is why that
    /// assertion has to fail loudly on a missing bounds rather than treat it
    /// as "nothing to check".
    #[gtk::test]
    fn a_full_unassigned_queue_scrolls_instead_of_leaving_the_window() {
        let (bin, state) = build_tab(cfg());
        // The control-center's own default is 760 × 560; 480 is a window an
        // operator can genuinely have, and the review's measurements were
        // taken at it.
        let window = present_sized(&bin, 760, 480);
        apply(&state, &["argus", "beta", "gamma"]);

        let queue: Vec<Approval> = (1..=12)
            .map(|id| approval(id, "", ApprovalStatus::Pending))
            .collect();
        apply_unassigned(&state, Some(Ok(queue)));
        pump_until(
            || {
                state
                    .unassigned_rows
                    .borrow()
                    .first()
                    .is_some_and(|r| r.height() > 0)
            },
            5,
        );
        assert_eq!(state.unassigned_rows.borrow().len(), 12);

        // The container whose extent is what an operator can actually see.
        // Falling back to the group itself (rather than unwrapping) is what
        // makes the mutation above red on the geometry rather than on a
        // missing widget.
        let container: gtk::Widget = state
            .unassigned_group
            .ancestor(gtk::ScrolledWindow::static_type())
            .unwrap_or_else(|| state.unassigned_group.clone().upcast());
        let bounds = container.compute_bounds(&window).unwrap_or_else(|| {
            panic!(
                "the Unassigned group has no bounds in the window at all — twelve rows could not \
                 be allocated inside it (the group measures {} px tall)",
                container.height()
            )
        });
        // The allocation is in pixels and fits an f32 exactly at any size a
        // window has; the cast is the only way to compare it with a
        // `graphene::Rect`.
        #[allow(clippy::cast_precision_loss)]
        let window_height = window.height() as f32;
        assert!(
            bounds.y() >= -0.5 && bounds.y() + bounds.height() <= window_height + 0.5,
            "the Unassigned group is allocated past the window: {bounds:?} in a window \
             {window_height} px tall"
        );

        let scroller: gtk::ScrolledWindow = container
            .downcast()
            .expect("the group must live in a scroller of its own");
        let adjustment = scroller.vadjustment();
        assert!(
            adjustment.upper() > adjustment.page_size() + 0.5,
            "twelve rows must overflow the cap and be reachable by scrolling, not silently \
             clipped: upper {} page {}",
            adjustment.upper(),
            adjustment.page_size()
        );

        let row_height = state.by_name.borrow()["argus"].row.height();
        assert!(row_height > 0, "the roster was never allocated");
        let roster = state
            .list
            .ancestor(gtk::ScrolledWindow::static_type())
            .expect("the sidebar list lives in a scroller");
        assert!(
            roster.height() > row_height * 2,
            "the Unassigned group crushed the roster: {} px left for {} px rows",
            roster.height(),
            row_height
        );
        dismiss(&window);
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
