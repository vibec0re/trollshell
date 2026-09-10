//! The node tree: the sidebar card (spec §6.1) and the drawer panel (§6.4).
//!
//! There is no row-activate event (a list is selection-less), so every
//! interaction is a `Button`, whose `id` is required and is the click target
//! (`crates/hytte-plugin-proto/src/wire.rs:249-255`). That is why the agent's
//! name is a button rather than a label.
//!
//! # What this file is built around, as of #966/#969/#971
//!
//! The first two rounds of @kaesaecracker's live screenshots turned up three
//! host limits, and all three have since been **fixed on main**. This file is
//! written against the fixed vocabulary; the history is kept only where it
//! still explains a choice.
//!
//! 1. **[`Node::Row`] carries a `spacing`** (#966/#969), mapped straight onto
//!    the backing `gtk::Box`. Before it, a `Row` of icon + label rendered them
//!    touching (`⚙argus` in @kaesaecracker's panel screenshot) and this file
//!    spelled every row as a horizontal `Box` instead. It no longer does: a
//!    row is a `Row`, one spelling, and `Box` is left for the **vertical**
//!    stacks (the card root, the two-line agent row, the inline details).
//! 2. **[`Node::Text`] carries a `tooltip`** (#961/#971). That is what retired
//!    the row-level tooltip: the untruncated status used to have nowhere to
//!    live except the enclosing row's `Box`, which put one legend over every
//!    glyph in the row. Now the string that got cut carries its own hover, and
//!    the second line shows the harness's status **in full** anyway — the
//!    ellipsis is the last resort for a status past [`STATUS_WRAP_MAX`], not
//!    the normal case.
//! 3. **[`Node::ListBox`] can be `dense`** (#966/#969). The host materializes a
//!    `ListBox` as a real `GtkListBox`, which auto-wraps every child in a
//!    `GtkListBoxRow` carrying libadwaita's row min-height — most of why twelve
//!    agents came to ~700 px. `dense: true` marks those wrappers so the shipped
//!    stylesheet zeroes their `min-height` and `padding`, handing the height
//!    budget back to `.ts-agent-row`. The roster is therefore the `ListBox`
//!    spec §6.1's mock always asked for.
//!
//! # Where the bounds are
//!
//! - **The sidebar card does not bound itself.** #967 put the sidebar's card
//!   stack in a `gtk::ScrolledWindow`, so a tall card no longer hides the cards
//!   below it, and wrapping the roster in a second viewport would hide rows
//!   behind an inner scrollbar inside a surface that already scrolls.
//!   [`MAX_ROWS`] stays as belt and braces — see its own docs for what it is
//!   actually bounding now.
//! - **The drawer panel does bound itself**, with [`Node::Scrolled`] (#966/#969)
//!   capped at [`PANEL_VIEWPORT_PX`]. The plugin drawer child is a plain
//!   `gtk::Box` with no scroller of its own (`trollshell/src/plugins/region.rs`,
//!   `build_panel_child`), so before this a panel taller than the drawer simply
//!   ran off the bottom — and this round gives the overview the **full** roster,
//!   which is exactly the content that gets long. `Scrolled` is a negotiated
//!   variant, so the negotiation happens once in `plugin.rs` and arrives here as
//!   [`PanelContext::viewport_px`] (`0` = the pre-#969 unbounded shape).

use hytte_plugin::nodes;
use hytte_plugin::proto::{Dir, Node};

use crate::config::AgentsConfig;
use crate::model::{
    Agent, AgentName, ExpandedGroups, Group, Hive, Status, UPDATE_BADGE_CLASS, UPDATE_BADGE_ICON,
    agent_url, group, headers_wanted,
};

/// The card's root node id.
pub const ROOT_ID: &str = "agents-root";
/// The panel's root node id.
pub const PANEL_ID: &str = "agents-panel";
/// The panel's "back to the hive overview" button.
pub const BACK_ID: &str = "agents-back";
/// The card title row's button: open the drawer panel at the hive overview.
///
/// The drawer is now the **only** thing on the card that opens somewhere else,
/// and it is reached from the card's own title rather than from a row —
/// @kaesaecracker, [#963](https://github.com/vibec0re/trollshell/pull/963):
/// "its very weird the panel opens in the top right after clicking bottom
/// left". A row's own details unfold in place ([`ids::DETAILS`]); the panel is
/// for the hive overview and the full roster, and its title-row button is where
/// a jump-to-another-surface belongs.
pub const OVERVIEW_ID: &str = "agents-overview";

/// Button id prefixes. Each is `"<prefix><name>"`; the reducer strips the
/// prefix and re-validates the remainder as an [`AgentName`] rather than
/// trusting the round trip.
pub mod ids {
    /// The row's primary click — the agent's name. Opens the drawer panel on
    /// that agent (spec §6.3's primary; P2 replaces it with the chat window).
    pub const CHAT: &str = "chat:";
    /// The row's pause/resume toggle.
    pub const PAUSE: &str = "pause:";
    /// The row's **details** disclosure.
    ///
    /// Was `edit:` until @kaesaecracker pointed out that a pen means *edit*
    /// and this panel edits nothing (editing is P4/P5; spec §10: "v1 is
    /// read-only … it is what the hive supports"). It then briefly opened the
    /// drawer, which is what the second round of screenshots objected to.
    /// It now unfolds the agent's details **inside the card**, where the click
    /// happened, one agent at a time.
    pub const DETAILS: &str = "details:";
    /// The panel's per-agent start.
    pub const START: &str = "start:";
    /// The panel's per-agent stop.
    pub const STOP: &str = "stop:";
    /// A project group's expander header.
    pub const GROUP: &str = "group:";
}

/// The most agent rows the card will draw.
///
/// **This is no longer a pixel budget.** It was one, when the sidebar could not
/// scroll and a long card pushed the pet card off-screen with no way back; #967
/// fixed that, and the rows have since gone from one line back to two, which
/// would have halved a pixel-derived cap. What is left is the bound that does
/// not depend on row height at all: a card renders a whole node tree on every
/// poll and that tree is msgpack over a socket, so the cap is on **how much
/// tree a runaway hive can make this plugin serialise every two seconds**, not
/// on how tall it draws. Twenty is comfortably above @kaesaecracker's live
/// twelve and still bounds a hive that grows an order of magnitude.
///
/// Overflow is **stated, never silent**: the card draws a "+N more" line and
/// the panel's overview lists the full roster, uncapped.
pub const MAX_ROWS: usize = 20;

/// The drawer panel's viewport cap, in pixels.
///
/// The plugin drawer child has no scroller of its own, so this is the panel's
/// only bound. A child shorter than the cap is not stretched, so the number
/// only ever matters for a panel that would otherwise overflow — which, with
/// the full roster on the overview, is any hive past a handful of agents. 560
/// is the shell's own historic drawer-card baseline (`modal.rs`'s
/// `MIN_CARD_HEIGHT` lineage); a plugin cannot see the monitor, so it cannot
/// derive the number the way the Stats page now does.
pub const PANEL_VIEWPORT_PX: u16 = 560;

/// How many characters of the agent's display name line 1 shows.
const NAME_CHARS: i32 = 20;

/// Past this many characters the status caption stops wrapping and ellipsizes.
///
/// The caption's job is to show the harness's line **in full** — that is the
/// whole point of giving the row its second line back. Wrapping does that for
/// anything a status realistically says; past this it would turn one row into
/// five, so the tail moves to the hover instead.
const STATUS_WRAP_MAX: usize = 88;

/// The ellipsized width of a status past [`STATUS_WRAP_MAX`].
const STATUS_CHARS: i32 = 34;

/// The ellipsized width of a key/value line's value.
const VALUE_CHARS: i32 = 28;

fn cls(classes: &[&str]) -> Vec<String> {
    classes.iter().map(|c| (*c).to_owned()).collect()
}

fn label(text: impl Into<String>, classes: &[&str]) -> Node {
    Node::Label {
        id: None,
        text: text.into(),
        tooltip: None,
        classes: cls(classes),
    }
}

/// A **wrapping** label: the whole string, on as many lines as it needs.
fn wrapped(body: impl Into<String>, classes: &[&str]) -> Node {
    Node::Text {
        id: None,
        text: body.into(),
        max_width_chars: None,
        ellipsize: false,
        tooltip: None,
        classes: cls(classes),
    }
}

/// An ellipsizing single-line label capped at `chars`, carrying its **own**
/// full text as hover (#961/#971).
///
/// The width cap is what makes the ellipsis actually happen in a row that also
/// holds a [`Node::Spacer`]: without a natural-width bound the label asks for
/// its full text and the spacer has nothing left to give, so the row grows
/// instead of the text shrinking. The tooltip is set explicitly rather than
/// left to the host's ellipsize default, so the falsification is local: delete
/// it here and the assertion reds here.
fn clipped(body: impl Into<String>, chars: i32, classes: &[&str]) -> Node {
    let body = body.into();
    Node::Text {
        id: None,
        text: body.clone(),
        max_width_chars: Some(chars),
        ellipsize: true,
        tooltip: Some(body),
        classes: cls(classes),
    }
}

fn icon(name: impl Into<String>, classes: &[&str]) -> Node {
    Node::Icon {
        id: None,
        name: name.into(),
        tooltip: None,
        classes: cls(classes),
    }
}

/// An icon whose meaning is not obvious from the glyph, so it carries hover
/// text (#957's `tooltip`).
fn icon_titled(name: impl Into<String>, hover: impl Into<String>, classes: &[&str]) -> Node {
    Node::Icon {
        id: None,
        name: name.into(),
        tooltip: Some(hover.into()),
        classes: cls(classes),
    }
}

fn button(id: impl Into<String>, classes: &[&str], child: Node) -> Node {
    Node::Button {
        id: id.into(),
        classes: cls(classes),
        child: Box::new(child),
    }
}

/// A compact icon button — every unlabelled control on the card and in the
/// panel header, each carrying the words its glyph does not.
fn icon_button(id: impl Into<String>, glyph: &str, hover: &str, classes: &[&str]) -> Node {
    button(id, classes, icon_titled(glyph, hover, &[]))
}

/// A horizontal [`Node::Row`] with a real gap (#966/#969).
fn hrow(spacing: u16, classes: &[&str], children: Vec<Node>) -> Node {
    let mut builder = nodes::row(children).spacing(spacing);
    for class in classes {
        builder = builder.class(*class);
    }
    builder.build()
}

/// A vertical stack. `Row` is horizontal-only, so the two-line agent row, the
/// inline details and the card root are all `Box`es.
fn vstack(spacing: i32, classes: &[&str], children: Vec<Node>) -> Node {
    Node::Box {
        id: None,
        dir: Dir::Vertical,
        spacing,
        scroll: false,
        tooltip: None,
        classes: cls(classes),
        children,
    }
}

/// A dense list surface: the roster, and every key/value group in the panel.
///
/// `dense` zeroes the auto-wrapper's height floor so the row's own padding is
/// the row's height; `boxed-list` is what the shell's stylesheet keys the
/// hairline separators off.
fn list(dense: bool, classes: &[&str], children: Vec<Node>) -> Node {
    let mut builder = nodes::list(children).dense(dense).class("boxed-list");
    for class in classes {
        builder = builder.class(*class);
    }
    builder.build()
}

/// Wrap `child` in a bounded viewport, or hand it back bare.
///
/// `max_height` of `0` means unbounded — which is also what a host that never
/// advertised `SCROLLED_VOCAB` gets, since the negotiation is resolved once in
/// `plugin.rs` and arrives here as the number. Building the node
/// **unnegotiated** here is correct precisely because that branch already
/// happened; it is also what makes the viewport testable, since
/// `nodes::scrolled(..).build()` consults a process-global that no test can set.
fn viewport(max_height: u16, classes: &[&str], child: Node) -> Node {
    if max_height == 0 {
        return child;
    }
    let mut builder = nodes::scrolled(max_height, child);
    for class in classes {
        builder = builder.class(*class);
    }
    builder.build_unnegotiated()
}

/// One key/value line.
fn detail(key: &str, value: &str) -> Node {
    hrow(
        6,
        &["ts-agent-detail"],
        vec![
            label(key, &["dim-label", "caption"]),
            Node::Spacer,
            clipped(value, VALUE_CHARS, &["caption", "numeric"]),
        ],
    )
}

/// A titled group: a heading over a list surface.
fn section(title: &str, body: Node) -> Node {
    vstack(
        4,
        &["ts-agents-section"],
        vec![label(title, &["heading"]), body],
    )
}

/// A single explanatory line — the shape every non-`Up` hive state renders.
///
/// The reason **wraps** rather than ellipsizing: an unreachable hive's reason
/// is the one string on the card that has to be read in full to be acted on
/// ("permission denied — needs `hive-admin` group"), and it is one row, not
/// twelve.
fn notice(icon_name: &str, body: &str, tone: &str) -> Node {
    hrow(
        6,
        &["ts-agents-notice"],
        vec![
            icon(icon_name, &["ts-agent-state", tone]),
            wrapped(body, &["dim-label", "caption"]),
        ],
    )
}

/// The pause button's glyph and hover: what the click will **do**, which is the
/// only reading that stays honest through the optimistic flip.
fn pause_affordance(agent: &Agent) -> (&'static str, &'static str) {
    if agent.paused() {
        ("media-playback-start-symbolic", "resume this agent")
    } else {
        ("media-playback-pause-symbolic", "pause this agent")
    }
}

/// The harness's status line, as line 2 of the row: **in full**, dim, wrapping.
///
/// Ellipsizing is the last resort ([`STATUS_WRAP_MAX`]), and when it happens the
/// `Text` carries the whole string as its own hover.
fn status_caption(agent: &Agent) -> Node {
    let line = agent.status_line();
    if line.chars().count() > STATUS_WRAP_MAX {
        clipped(
            line,
            STATUS_CHARS,
            &["dim-label", "caption", "ts-agent-status"],
        )
    } else {
        wrapped(line, &["dim-label", "caption", "ts-agent-status"])
    }
}

/// Only the flags that are **on**, as chips.
///
/// A key/value dump of five booleans says nothing when four of them are the
/// default — `running yes / failed no / paused no / needs login no` is four
/// lines that carry one bit between them. `running` is not here at all: it is
/// the status glyph and the status line, both already on the row.
fn flag_chips(agent: &Agent) -> Vec<Node> {
    [
        (agent.row.failed, "failed", "error"),
        (agent.row.needs_login, "needs login", "warning"),
        (agent.paused(), "paused", "dim-label"),
        (agent.needs_update(), "needs update", UPDATE_BADGE_CLASS),
    ]
    .into_iter()
    .filter(|(on, _, _)| *on)
    .map(|(_, text, tone)| label(text, &["ts-agent-chip", "caption", tone]))
    .collect()
}

/// The hive's own `deployed_sha`, clamped to 12 characters.
///
/// The wire doc says the field is already the first 12 (`hive-sh4re`), so this
/// is belt and braces against a hive that ever sends the full 40 — a raw sha in
/// a 320 px card is one string that would push everything else out.
fn short_sha(agent: &Agent) -> Option<String> {
    agent
        .row
        .deployed_sha
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(12).collect())
}

/// One agent, on **two compact lines** (spec §6.1's mock).
///
/// Line 1 is the identity and the controls; line 2 is the harness's own status,
/// in full. The one-line row that shipped between the two screenshot rounds was
/// an overcorrection for a sidebar that could not scroll (#967 fixed that) and
/// is what @kaesaecracker read as "crammed".
///
/// `open` unfolds this agent's details underneath, in place.
fn agent_row(agent: &Agent, cfg: &AgentsConfig, open: bool) -> Node {
    let name = agent.name.as_str();
    let status = agent.status();
    let (pause_glyph, pause_hover) = pause_affordance(agent);

    let mut head = vec![
        icon(cfg.icon_for(name), &["ts-agent-runtime"]),
        button(
            format!("{}{name}", ids::CHAT),
            &["flat", "ts-agent-name"],
            // Clipped, not a bare `Label`: a `Label`'s natural width forces its
            // container wider (the #281 sidebar blow-out), and an agent name may
            // be up to 63 bytes.
            clipped(cfg.label_for(name), NAME_CHARS, &["heading"]),
        ),
        icon_titled(
            status.icon(),
            status.text(),
            &["ts-agent-state", status.class()],
        ),
        Node::Spacer,
    ];
    if agent.needs_update() {
        // Spec §6.2's badge row says "(tooltip only)" for its text.
        head.push(icon_titled(
            UPDATE_BADGE_ICON,
            "config commit pending — a rebuild would change this agent's locked rev",
            &["ts-agent-badge", UPDATE_BADGE_CLASS],
        ));
    }
    head.push(icon_button(
        format!("{}{name}", ids::PAUSE),
        pause_glyph,
        pause_hover,
        &["flat", "ts-agent-btn"],
    ));
    head.push(icon_button(
        format!("{}{name}", ids::DETAILS),
        if open {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        },
        if open { "hide details" } else { "details" },
        &["flat", "ts-agent-btn"],
    ));

    let mut children = vec![hrow(6, &["ts-agent-head"], head), status_caption(agent)];
    if open {
        children.push(details_block(agent));
    }
    vstack(2, &["ts-agent-row"], children)
}

/// The in-place unfold: the row's own details, inside the card.
///
/// Deliberately a **summary**, not the panel: the flags that are on, the
/// deployment triple, and the agent's page. The panel keeps the hive-level
/// context (socket, poll age, dashboard) and the full roster.
fn details_block(agent: &Agent) -> Node {
    let mut children = Vec::new();
    let chips = flag_chips(agent);
    if !chips.is_empty() {
        children.push(hrow(4, &["ts-agent-chips"], chips));
    }
    if let Some(sha) = short_sha(agent) {
        children.push(detail("deployed", &sha));
    }
    children.push(detail(
        "parent",
        agent.row.parent.as_deref().unwrap_or("— (root)"),
    ));
    if let Some(model) = agent.row.active_model.as_deref() {
        children.push(detail("model", model));
    }
    if let Some(url) = agent_url(agent) {
        children.push(detail("agent page", url));
    }
    vstack(2, &["ts-agent-details"], children)
}

/// The card's one-line summary of the hive, right-pinned in the title row.
#[must_use]
pub fn hive_summary(hive: &Hive) -> String {
    match hive {
        Hive::Connecting => "connecting…".to_owned(),
        Hive::Unreachable { .. } => "no hive".to_owned(),
        Hive::Error { .. } => "hive error".to_owned(),
        Hive::Incompatible(m) => format!("v{} ≠ v{}", m.theirs, m.ours),
        Hive::Up { agents } if agents.is_empty() => "no agents".to_owned(),
        Hive::Up { agents } => format!("up · {}", agents.len()),
    }
}

/// The sidebar card: a titled surface, then the roster as a dense list.
///
/// The title matches the `Tasks` card above it — the same all-caps caption
/// class and the same header rhythm — so the two read as siblings rather than
/// as a card and a pile of rows. The host supplies the card surface itself
/// (`.ts-plugin-card`, #319) and deliberately no padding, so the root carries
/// `ts-agents-card` for its own inset.
#[must_use]
pub fn card(
    hive: &Hive,
    cfg: &AgentsConfig,
    expanded: &ExpandedGroups,
    opened: Option<&AgentName>,
) -> Node {
    let title = hrow(
        6,
        &["ts-agents-title"],
        vec![
            label("AGENTS", &["ts-agents-heading"]),
            Node::Spacer,
            label(hive_summary(hive), &["dim-label", "caption", "numeric"]),
            icon_button(
                OVERVIEW_ID,
                "view-list-symbolic",
                "hive overview and the full roster",
                &["flat", "ts-agent-btn"],
            ),
        ],
    );

    let body = match hive {
        Hive::Connecting => vec![notice(
            "content-loading-symbolic",
            "connecting…",
            "dim-label",
        )],
        Hive::Unreachable { reason } => {
            vec![notice("network-offline-symbolic", reason, "dim-label")]
        }
        // Reachable, and saying no — a different problem from "no hive", so a
        // different icon (spec §5.3 covers only the unreachable case; this is
        // its reachable sibling).
        Hive::Error { reason } => vec![notice("dialog-error-symbolic", reason, "error")],
        Hive::Incompatible(mismatch) => vec![notice(
            "dialog-warning-symbolic",
            &format!(
                "hive protocol v{}, plugin speaks v{}",
                mismatch.theirs, mismatch.ours
            ),
            "warning",
        )],
        Hive::Up { agents } if agents.is_empty() => {
            vec![notice("system-run-symbolic", "no agents", "dim-label")]
        }
        Hive::Up { agents } => roster(agents, cfg, expanded, opened),
    };

    Node::Box {
        id: Some(ROOT_ID.to_owned()),
        dir: Dir::Vertical,
        spacing: 0,
        scroll: false,
        tooltip: None,
        classes: vec!["ts-agents-card".to_owned()],
        children: vec![title, list(true, &["ts-agents-list"], body)],
    }
}

/// The roster body: grouped rows, capped at [`MAX_ROWS`].
fn roster(
    agents: &[Agent],
    cfg: &AgentsConfig,
    expanded: &ExpandedGroups,
    opened: Option<&AgentName>,
) -> Vec<Node> {
    let groups = group(agents, cfg);
    let headers = headers_wanted(&groups);
    let mut out = Vec::new();
    let mut drawn = 0usize;
    let mut skipped = 0usize;

    for g in &groups {
        // A collapsed group costs one line, so its rows do not count against
        // the budget and are not "skipped" either — they are one click away.
        let open = group_open(g, expanded);
        if headers && !open {
            out.push(group_node(g, false, Vec::new()));
            continue;
        }

        let mut rows = Vec::new();
        for agent in &g.agents {
            if drawn >= MAX_ROWS {
                skipped += 1;
                continue;
            }
            rows.push(agent_row(
                agent,
                cfg,
                opened.is_some_and(|n| *n == agent.name),
            ));
            drawn += 1;
        }
        if headers {
            out.push(group_node(g, true, rows));
        } else {
            out.extend(rows);
        }
    }

    if skipped > 0 {
        out.push(notice(
            "view-more-symbolic",
            &format!("+{skipped} more — open the panel for the full roster"),
            "dim-label",
        ));
    }
    out
}

/// Whether a group draws expanded.
///
/// Default: **open, unless every agent in it is stopped** — a group of
/// parked agents is the one nobody is watching, and collapsing it by default
/// is what buys back the vertical space on a busy hive. An explicit click
/// always wins over the default.
fn group_open(g: &Group<'_>, expanded: &ExpandedGroups) -> bool {
    let key = g.header();
    expanded
        .get(key)
        .copied()
        .unwrap_or_else(|| g.agents.iter().any(|a| a.status() != Status::Stopped))
}

/// One project group as an `Expander` — header plus its rows.
fn group_node(g: &Group<'_>, open: bool, rows: Vec<Node>) -> Node {
    let live = g
        .agents
        .iter()
        .filter(|a| a.status() != Status::Stopped)
        .count();
    Node::Expander {
        id: format!("{}{}", ids::GROUP, g.header()),
        header: Box::new(hrow(
            6,
            &["ts-agents-group"],
            vec![
                label(g.header(), &["heading"]),
                Node::Spacer,
                label(
                    format!("{live}/{}", g.agents.len()),
                    &["dim-label", "caption", "numeric"],
                ),
            ],
        )),
        children: rows,
        expanded: open,
        tooltip: None,
        classes: vec!["ts-agents-group-row".to_owned()],
    }
}

/// Everything the panel needs that the model does not carry: the clock, the
/// socket in use, when the last poll answered, and the negotiated viewport.
#[derive(Clone, Copy, Debug)]
pub struct PanelContext<'a> {
    /// Unix seconds from the host's clock subscription, `0` before the first
    /// snapshot lands.
    pub now_unix: i64,
    /// When the last poll answered, in unix seconds.
    pub last_poll_unix: Option<i64>,
    /// The socket path actually in use, from `agents.toml`.
    pub socket: &'a str,
    /// The hive's `Urls` answer, when one has landed.
    pub urls: Option<&'a crate::hive::wire::HiveUrls>,
    /// The panel viewport's cap in pixels, or `0` for unbounded — which is both
    /// "no cap wanted" and "this host predates [`Node::Scrolled`]". The
    /// negotiation happens once, in `plugin.rs`.
    pub viewport_px: u16,
}

/// A coarse relative age. Minute granularity on purpose: a per-second string
/// would force one render frame per second through the SDK's dedup for a label
/// nobody reads that precisely.
#[must_use]
pub fn age(now_unix: i64, then_unix: i64) -> String {
    let secs = now_unix.saturating_sub(then_unix);
    if secs < 0 {
        return "just now".to_owned();
    }
    match secs {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// `status_set_at` (RFC 3339 UTC on the wire) as unix seconds, or `None` when
/// it is absent or unparseable — one missing age label, never a lost row.
#[must_use]
pub fn parse_set_at(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp())
}

/// The hive-level section every panel carries: reachability, the socket in
/// use, and the last poll's age (spec §6.4).
fn hive_section(hive: &Hive, ctx: PanelContext<'_>) -> Node {
    let reachability = match hive {
        Hive::Connecting => "connecting…".to_owned(),
        Hive::Unreachable { reason } => format!("unreachable — {reason}"),
        Hive::Error { reason } => format!("reachable, but refused — {reason}"),
        Hive::Incompatible(m) => {
            format!(
                "refused — hive protocol v{}, plugin speaks v{}",
                m.theirs, m.ours
            )
        }
        Hive::Up { agents } => format!("up — {} agent(s)", agents.len()),
    };
    let last_poll = match ctx.last_poll_unix {
        Some(at) if ctx.now_unix > 0 => age(ctx.now_unix, at),
        Some(_) => "just now".to_owned(),
        None => "never".to_owned(),
    };
    let mut rows = vec![
        detail("state", &reachability),
        detail("socket", ctx.socket),
        detail("last poll", &last_poll),
    ];
    // The dashboard root, when the hive says it is reachable from a browser.
    if let Some(home) = hive_home(ctx) {
        rows.push(detail("dashboard", home));
    }
    section("hive", list(false, &["ts-agents-panel-list"], rows))
}

/// The hive's dashboard root, when it has one.
fn hive_home(ctx: PanelContext<'_>) -> Option<&str> {
    ctx.urls
        .and_then(|u| u.home.as_deref())
        .map(str::trim)
        .filter(|h| !h.is_empty())
}

/// One agent as a single line in the panel's full roster.
fn panel_roster_row(agent: &Agent, cfg: &AgentsConfig) -> Node {
    let name = agent.name.as_str();
    let status = agent.status();
    hrow(
        6,
        &["ts-agent-detail"],
        vec![
            icon(cfg.icon_for(name), &["ts-agent-runtime"]),
            button(
                format!("{}{name}", ids::CHAT),
                &["flat", "ts-agent-name"],
                label(cfg.label_for(name), &[]),
            ),
            icon_titled(
                status.icon(),
                status.text(),
                &["ts-agent-state", status.class()],
            ),
            Node::Spacer,
            clipped(agent.status_line(), STATUS_CHARS, &["dim-label", "caption"]),
        ],
    )
}

/// The selected agent's page: header, status, chips, deployment, links.
fn agent_page(agent: &Agent, cfg: &AgentsConfig, ctx: PanelContext<'_>) -> Vec<Node> {
    let name = agent.name.as_str();
    let status = agent.status();
    let mut out = Vec::new();

    out.push(hrow(
        8,
        &["ts-agents-panel-head"],
        vec![
            icon(cfg.icon_for(name), &["ts-agent-runtime"]),
            label(cfg.label_for(name), &["title-4"]),
            icon_titled(
                status.icon(),
                status.text(),
                &["ts-agent-state", status.class()],
            ),
            Node::Spacer,
            // Spec §11 rule one: both frames are scoped to this one agent.
            icon_button(
                format!("{}{name}", ids::START),
                "media-playback-start-symbolic",
                "start this agent",
                &["flat", "circular", "ts-agent-btn"],
            ),
            icon_button(
                format!("{}{name}", ids::STOP),
                "media-playback-stop-symbolic",
                "stop this agent",
                &["flat", "circular", "ts-agent-btn"],
            ),
        ],
    ));
    if cfg.label_for(name) != name {
        out.push(wrapped(name, &["dim-label", "caption", "ts-mono"]));
    }
    out.push(wrapped(agent.status_line(), &["dim-label"]));

    let chips = flag_chips(agent);
    if !chips.is_empty() {
        out.push(hrow(4, &["ts-agent-chips"], chips));
    }

    let mut deployment = vec![detail(
        "parent",
        agent.row.parent.as_deref().unwrap_or("— (root)"),
    )];
    if let Some(sha) = short_sha(agent) {
        deployment.insert(0, detail("deployed", &sha));
    }
    if let Some(model) = agent.row.active_model.as_deref() {
        deployment.push(detail("model", model));
    }
    if let Some(at) = agent
        .row
        .status_set_at
        .as_deref()
        .and_then(parse_set_at)
        .filter(|_| ctx.now_unix > 0)
    {
        deployment.push(detail("status set", &age(ctx.now_unix, at)));
    }
    out.push(section(
        "deployment",
        list(false, &["ts-agents-panel-list"], deployment),
    ));

    let mut links = Vec::new();
    if let Some(url) = agent_url(agent) {
        links.push(detail("agent page", url));
    }
    if let Some(home) = hive_home(ctx) {
        links.push(detail("dashboard", home));
    }
    if !links.is_empty() {
        out.push(section(
            "links",
            list(false, &["ts-agents-panel-list"], links),
        ));
    }

    out.push(hrow(
        6,
        &["ts-agents-panel-actions"],
        vec![
            Node::Spacer,
            button(BACK_ID, &["flat"], label("all agents", &[])),
        ],
    ));
    out
}

/// The drawer panel (spec §6.4): the selected agent's full detail, or the hive
/// overview **plus the full, uncapped roster** when nothing is selected.
///
/// The roster is what makes the card's `+N more — open the panel for the full
/// roster` line true; before this round the panel showed no rows at all.
#[must_use]
pub fn panel(
    hive: &Hive,
    cfg: &AgentsConfig,
    selected: Option<&AgentName>,
    ctx: PanelContext<'_>,
) -> Node {
    let mut children = Vec::new();

    if let Some(agent) = selected.and_then(|name| hive.agent(name)) {
        children.extend(agent_page(agent, cfg, ctx));
    }

    children.push(hive_section(hive, ctx));

    if selected.is_none() {
        let agents = hive.agents();
        if !agents.is_empty() {
            children.push(section(
                "roster",
                list(
                    false,
                    &["ts-agents-panel-list"],
                    agents
                        .iter()
                        .map(|a| panel_roster_row(a, cfg))
                        .collect::<Vec<_>>(),
                ),
            ));
        }
    }

    let body = vstack(8, &["ts-agents-panel-body"], children);
    Node::Box {
        id: Some(PANEL_ID.to_owned()),
        dir: Dir::Vertical,
        spacing: 0,
        scroll: false,
        tooltip: None,
        classes: vec!["ts-agents-panel".to_owned()],
        children: vec![viewport(ctx.viewport_px, &["ts-agents-viewport"], body)],
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_ROWS, PANEL_VIEWPORT_PX, STATUS_CHARS, STATUS_WRAP_MAX, age, ids, parse_set_at,
    };
    use crate::config::AgentsConfig;
    use crate::hive::wire::AgentStatusRow;
    use crate::model::{Agent, AgentName, ExpandedGroups, Hive};
    use hytte_plugin::proto::Node;

    fn agent(name: &str, row: AgentStatusRow) -> Agent {
        Agent {
            name: AgentName::parse(name).expect("legal"),
            row,
            pending_paused: None,
        }
    }

    fn running(name: &str, status: &str) -> Agent {
        agent(
            name,
            AgentStatusRow {
                name: name.to_owned(),
                running: true,
                status_text: Some(status.to_owned()),
                ..AgentStatusRow::default()
            },
        )
    }

    fn card_of(agents: Vec<Agent>, opened: Option<&AgentName>) -> Node {
        super::card(
            &Hive::Up { agents },
            &AgentsConfig::default(),
            &ExpandedGroups::new(),
            opened,
        )
    }

    #[test]
    fn ages_render_at_minute_granularity() {
        assert_eq!(age(1000, 1000), "just now");
        assert_eq!(age(1059, 1000), "just now");
        assert_eq!(age(1060, 1000), "1m ago");
        assert_eq!(age(1000 + 3599, 1000), "59m ago");
        assert_eq!(age(1000 + 3600, 1000), "1h ago");
        assert_eq!(age(1000 + 86_400, 1000), "1d ago");
        // A clock that ran backwards must not print a negative age.
        assert_eq!(age(1000, 2000), "just now");
    }

    #[test]
    fn a_status_timestamp_parses_and_a_broken_one_costs_only_the_label() {
        assert_eq!(parse_set_at("1970-01-01T00:00:42Z"), Some(42));
        // An offset timestamp is normalised to UTC, not read as wall-clock:
        // 12:00+02:00 is 10:00Z, so the two must agree.
        assert_eq!(
            parse_set_at("2026-09-07T12:00:00+02:00"),
            Some(1_788_775_200)
        );
        assert_eq!(
            parse_set_at("2026-09-07T10:00:00Z"),
            parse_set_at("2026-09-07T12:00:00+02:00")
        );
        assert_eq!(parse_set_at("not a timestamp"), None);
        assert_eq!(parse_set_at("1757239200"), None);
    }

    /// @kaesaecracker's "crammed": the row is **two lines**, and line 2 is the
    /// harness's status **in full** — wrapping, not cut.
    ///
    /// Falsification: swap `status_caption`'s wrapping arm for the ellipsizing
    /// one (i.e. clip every status the way the one-line row did) and the
    /// `ellipsize` assertion goes red.
    #[test]
    fn line_two_carries_the_whole_status_and_does_not_clip_it() {
        // 63 chars — a realistic harness line, and the length that was being
        // cut at 22 characters on the one-line row.
        const LINE: &str = "idle — #4077 approved, watching for review assignments";
        assert!(LINE.chars().count() <= STATUS_WRAP_MAX);

        let tree = card_of(vec![running("argus", LINE)], None);
        let status = find_text(&tree, LINE).expect("the status renders somewhere");
        assert!(
            !status.ellipsize,
            "the status line must be shown in full, not ellipsized"
        );
        assert_eq!(
            status.max_width_chars, None,
            "a width cap would truncate the line the row exists to show"
        );
        assert!(
            status.classes.iter().any(|c| c == "ts-agent-status"),
            "line 2 is the status caption: {:?}",
            status.classes
        );

        // …and it really is a second line: the row is a vertical stack whose
        // first child is the head row and whose second is that caption.
        let row = find_class(&tree, "ts-agent-row").expect("the row renders");
        let Node::Box { dir, children, .. } = &row else {
            panic!("an agent row is a vertical stack, got {row:?}");
        };
        assert_eq!(*dir, hytte_plugin::proto::Dir::Vertical);
        assert_eq!(children.len(), 2, "two lines, no unfold: {children:?}");
    }

    /// Past [`STATUS_WRAP_MAX`] the caption ellipsizes — and then the **`Text`
    /// itself** carries the whole string as hover (#971), not the row.
    ///
    /// Falsification: drop the `tooltip: Some(body)` from `clipped` and the
    /// hover assertion goes red; raise `STATUS_WRAP_MAX` past the fixture and
    /// the `ellipsize` one does.
    #[test]
    fn an_absurdly_long_status_clips_and_hovers_itself_in_full() {
        // No trailing whitespace: `Agent::status_line` trims, so a padded
        // fixture would not be the string the tree actually carries.
        let long = format!(
            "idle — {}",
            ["watching for review assignments"; 6].join(", ")
        );
        assert!(long.chars().count() > STATUS_WRAP_MAX);

        let tree = card_of(vec![running("argus", &long)], None);
        let status = find_text(&tree, &long).expect("the status renders");
        assert!(status.ellipsize, "past the wrap budget it must ellipsize");
        assert_eq!(status.max_width_chars, Some(STATUS_CHARS));
        assert_eq!(
            status.tooltip.as_deref(),
            Some(long.as_str()),
            "the cut string carries its own untruncated hover"
        );
    }

    /// The details button unfolds **in place**: the row grows a third child
    /// holding that agent's details, and no other row does.
    ///
    /// Falsification: drop the `if open { children.push(details_block(..)) }`
    /// arm in `agent_row` and the first assertion goes red; ignore `opened` in
    /// `roster` (pass `false` unconditionally) and it does too.
    #[test]
    fn the_open_row_unfolds_its_details_and_only_that_row() {
        let agents = vec![running("argus", "idle"), running("bosun", "idle")];
        let opened = AgentName::parse("argus").expect("legal");

        let closed = card_of(agents.clone(), None);
        assert_eq!(
            count_class(&closed, "ts-agent-details"),
            0,
            "nothing is unfolded until a details click"
        );

        let open = card_of(agents, Some(&opened));
        assert_eq!(
            count_class(&open, "ts-agent-details"),
            1,
            "exactly one row unfolds — one agent open at a time"
        );
        // …and it is the *right* row: the unfolded block sits inside the row
        // whose buttons address `argus`.
        let row = rows_with_class(&open, "ts-agent-row")
            .into_iter()
            .find(|r| count_class(r, "ts-agent-details") == 1)
            .expect("some row unfolded");
        assert!(
            button_ids(&row).iter().any(|id| id == "details:argus"),
            "the unfolded row must be argus's: {:?}",
            button_ids(&row)
        );
    }

    /// The disclosure glyph says which way the click goes, and carries the
    /// words the glyph does not.
    ///
    /// Falsification: make `agent_row` render one fixed chevron regardless of
    /// `open` and this goes red.
    #[test]
    fn the_disclosure_glyph_flips_with_the_unfold() {
        let name = AgentName::parse("argus").expect("legal");
        let closed = card_of(vec![running("argus", "idle")], None);
        let open = card_of(vec![running("argus", "idle")], Some(&name));

        assert_eq!(
            icon_hover(&closed, "pan-end-symbolic").as_deref(),
            Some("details")
        );
        assert_eq!(
            icon_hover(&open, "pan-down-symbolic").as_deref(),
            Some("hide details")
        );
        assert!(
            icon_hover(&open, "pan-end-symbolic").is_none(),
            "an unfolded row must not still offer to unfold"
        );
    }

    /// Only the flags that are **on** become chips; a clean agent gets none.
    ///
    /// Falsification: drop the `.filter(|(on, ..)| *on)` in `flag_chips` and
    /// the clean-agent assertion goes red (four chips instead of zero).
    #[test]
    fn only_the_flags_that_are_on_become_chips() {
        let name = AgentName::parse("argus").expect("legal");

        let clean = card_of(vec![running("argus", "idle")], Some(&name));
        assert_eq!(
            chip_texts(&clean),
            Vec::<String>::new(),
            "a healthy agent's details say nothing about flags that are off"
        );

        let flagged = card_of(
            vec![agent(
                "argus",
                AgentStatusRow {
                    name: "argus".to_owned(),
                    running: true,
                    needs_login: true,
                    needs_update: true,
                    ..AgentStatusRow::default()
                },
            )],
            Some(&name),
        );
        assert_eq!(chip_texts(&flagged), vec!["needs login", "needs update"]);
    }

    /// A hive bigger than the card can show draws [`MAX_ROWS`] rows and then
    /// **says so** — silently dropping the tail would be indistinguishable from
    /// the agents not existing.
    ///
    /// Falsification: remove the `drawn >= MAX_ROWS` guard in `roster` and the
    /// row count assertion goes red; remove the overflow `notice` and the
    /// "+N more" one does.
    #[test]
    fn a_hive_past_the_cap_draws_max_rows_and_says_how_many_it_hid() {
        let agents: Vec<Agent> = (0..MAX_ROWS + 7)
            .map(|i| running(&format!("agent-{i}"), "idle"))
            .collect();

        let tree = card_of(agents, None);
        assert_eq!(
            count_class(&tree, "ts-agent-row"),
            MAX_ROWS,
            "the card must cap what it draws"
        );
        assert!(
            texts(&tree).iter().any(|t| t.contains("+7 more")),
            "the hidden tail must be stated, not silent; got {:?}",
            texts(&tree)
        );
    }

    /// Whatever the card hides, the panel's overview shows — uncapped. That is
    /// what makes the "+N more — open the panel for the full roster" line true.
    ///
    /// Falsification: drop the roster `section` from `panel` and this goes red.
    #[test]
    fn the_panel_overview_lists_every_agent_the_card_capped() {
        let agents: Vec<Agent> = (0..MAX_ROWS + 7)
            .map(|i| running(&format!("agent-{i}"), "idle"))
            .collect();
        let hive = Hive::Up { agents };
        let panel = super::panel(&hive, &AgentsConfig::default(), None, ctx());

        let ids = button_ids(&panel);
        for i in 0..MAX_ROWS + 7 {
            let want = format!("chat:agent-{i}");
            assert!(ids.contains(&want), "the panel roster must list {want}");
        }
    }

    /// The panel bounds itself, because the plugin drawer child has no scroller
    /// of its own. `0` — an old host, or no cap — is the bare tree.
    ///
    /// Falsification: return `child` unconditionally from `viewport` and the
    /// first assertion goes red.
    #[test]
    fn the_panel_wraps_its_body_in_a_bounded_viewport() {
        let hive = Hive::Up {
            agents: vec![running("argus", "idle")],
        };
        let cfg = AgentsConfig::default();

        let bounded = super::panel(&hive, &cfg, None, ctx());
        let Node::Box { children, .. } = &bounded else {
            panic!("the panel root is a Box");
        };
        match children.as_slice() {
            [Node::Scrolled { max_height, .. }] => {
                assert_eq!(*max_height, PANEL_VIEWPORT_PX);
            }
            other => panic!("the panel body must sit in a viewport, got {other:?}"),
        }

        let unbounded = super::panel(
            &hive,
            &cfg,
            None,
            super::PanelContext {
                viewport_px: 0,
                ..ctx()
            },
        );
        let Node::Box { children, .. } = &unbounded else {
            panic!("the panel root is a Box");
        };
        assert!(
            !matches!(children.as_slice(), [Node::Scrolled { .. }]),
            "a host that cannot decode Scrolled gets the bare body"
        );
    }

    /// The id prefixes are the reducer's parsing contract; a colon terminator
    /// is what makes `strip_prefix` unambiguous against a name whitelist that
    /// excludes `:`.
    #[test]
    fn every_button_prefix_ends_in_a_colon() {
        for prefix in [
            ids::CHAT,
            ids::PAUSE,
            ids::DETAILS,
            ids::START,
            ids::STOP,
            ids::GROUP,
        ] {
            assert!(prefix.ends_with(':'), "{prefix}");
        }
        // The two whole-id buttons are not prefixes and must not look like one,
        // or `strip_prefix` would match them against an agent name.
        for id in [super::BACK_ID, super::OVERVIEW_ID] {
            assert!(!id.contains(':'), "{id}");
        }
    }

    // ── tree walkers ─────────────────────────────────────────────────────────

    fn ctx() -> super::PanelContext<'static> {
        super::PanelContext {
            now_unix: 1_788_785_100,
            last_poll_unix: Some(1_788_785_040),
            socket: "/run/hive/host.sock",
            urls: None,
            viewport_px: PANEL_VIEWPORT_PX,
        }
    }

    /// The `Text` fields the layout assertions read.
    struct TextNode {
        ellipsize: bool,
        max_width_chars: Option<i32>,
        tooltip: Option<String>,
        classes: Vec<String>,
    }

    /// Walk every node in `node`, applying `f` to each.
    fn walk(node: &Node, f: &mut impl FnMut(&Node)) {
        f(node);
        match node {
            Node::Box { children, .. }
            | Node::Row { children, .. }
            | Node::ListBox { children, .. } => {
                for c in children {
                    walk(c, f);
                }
            }
            Node::Expander {
                header, children, ..
            } => {
                walk(header, f);
                for c in children {
                    walk(c, f);
                }
            }
            Node::Button { child, .. } | Node::Scrolled { child, .. } => walk(child, f),
            _ => {}
        }
    }

    fn find_text(node: &Node, needle: &str) -> Option<TextNode> {
        let mut found = None;
        walk(node, &mut |n| {
            if let Node::Text {
                text,
                ellipsize,
                max_width_chars,
                tooltip,
                classes,
                ..
            } = n
                && text == needle
                && found.is_none()
            {
                found = Some(TextNode {
                    ellipsize: *ellipsize,
                    max_width_chars: *max_width_chars,
                    tooltip: tooltip.clone(),
                    classes: classes.clone(),
                });
            }
        });
        found
    }

    /// Every `Label`/`Text` body in the tree.
    fn texts(node: &Node) -> Vec<String> {
        let mut out = Vec::new();
        walk(node, &mut |n| match n {
            Node::Label { text, .. } | Node::Text { text, .. } => out.push(text.clone()),
            _ => {}
        });
        out
    }

    /// Every `Button` id in the tree.
    fn button_ids(node: &Node) -> Vec<String> {
        let mut out = Vec::new();
        walk(node, &mut |n| {
            if let Node::Button { id, .. } = n {
                out.push(id.clone());
            }
        });
        out
    }

    /// The tooltip on the first `Icon` named `glyph`.
    fn icon_hover(node: &Node, glyph: &str) -> Option<String> {
        let mut found = None;
        walk(node, &mut |n| {
            if let Node::Icon { name, tooltip, .. } = n
                && name == glyph
                && found.is_none()
            {
                found = tooltip.clone();
            }
        });
        found
    }

    /// Every chip's text, in tree order.
    fn chip_texts(node: &Node) -> Vec<String> {
        let mut out = Vec::new();
        walk(node, &mut |n| {
            if let Node::Label { text, classes, .. } = n
                && classes.iter().any(|c| c == "ts-agent-chip")
            {
                out.push(text.clone());
            }
        });
        out
    }

    fn classes_of(node: &Node) -> &[String] {
        match node {
            Node::Box { classes, .. }
            | Node::Row { classes, .. }
            | Node::ListBox { classes, .. }
            | Node::Expander { classes, .. }
            | Node::Scrolled { classes, .. }
            | Node::Label { classes, .. }
            | Node::Text { classes, .. }
            | Node::Icon { classes, .. }
            | Node::Button { classes, .. } => classes,
            _ => &[],
        }
    }

    /// How many nodes carry `class`.
    fn count_class(node: &Node, class: &str) -> usize {
        let mut n = 0;
        walk(node, &mut |node| {
            if classes_of(node).iter().any(|c| c == class) {
                n += 1;
            }
        });
        n
    }

    /// Every node carrying `class`, cloned.
    fn rows_with_class(node: &Node, class: &str) -> Vec<Node> {
        let mut out = Vec::new();
        walk(node, &mut |node| {
            if classes_of(node).iter().any(|c| c == class) {
                out.push(node.clone());
            }
        });
        out
    }

    /// The first node carrying `class`.
    fn find_class(node: &Node, class: &str) -> Option<Node> {
        rows_with_class(node, class).into_iter().next()
    }
}
