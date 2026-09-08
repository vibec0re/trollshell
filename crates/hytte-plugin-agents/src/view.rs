//! The node tree: the sidebar card (spec §6.1) and the drawer panel (§6.4).
//!
//! Every node used here already exists in the wire vocabulary — `Box`,
//! `Label`, `Text`, `Icon`, `Button`, `Expander`, `Separator`, `Spacer`.
//! **No proto change, no `VOCAB` bump.**
//!
//! There is no row-activate event (a list is selection-less), so every
//! interaction is a `Button`, whose `id` is required and is the click target
//! (`crates/hytte-plugin-proto/src/wire.rs:249-255`). That is why the agent's
//! name is a button rather than a label.
//!
//! # Three host facts this file is built around
//!
//! Learned from @kaesaecracker's live screenshots against a 12-agent hive, and
//! each one is why the obvious spelling is *not* what is written below:
//!
//! 1. **`Node::Row` has no spacing.** The host builds it as
//!    `gtk::Box::new(Horizontal, 0)` (`crates/hytte-ui/src/widget_tree.rs`'s
//!    `build_node`) — the gap is hardcoded, not a field. A `Row` of
//!    icon + label renders them touching, which is exactly how `⚙argus` came
//!    out in the panel header. Everything here uses `Box { dir: Horizontal,
//!    spacing }` instead, and `Row` appears nowhere.
//! 2. **`ListBox` auto-wraps every child in a `GtkListBoxRow`**
//!    (`widget_tree.rs:708-716`), which carries libadwaita's row min-height.
//!    That is most of why twelve agents came to ~700 px. The card is a plain
//!    vertical `Box`; nothing here is a `ListBox`.
//! 3. **`Box { scroll: true }` is a scroll *event target*, not a viewport.**
//!    It attaches an `EventControllerScroll` that forwards deltas to the
//!    plugin (`wire.rs:134-145`, `widget_tree.rs:826-829`); the widget is a
//!    plain `gtk::Box` that neither clips nor scrolls. Combined with GTK CSS
//!    having no `max-height`, **a plugin cannot bound its own card with a
//!    scrollable region** — so the card bounds itself by *rendering less*:
//!    one-line rows, collapsible groups, and [`MAX_ROWS`]. A real inner
//!    scroll needs the shell-side sidebar fix.

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

/// Button id prefixes. Each is `"<prefix><name>"`; the reducer strips the
/// prefix and re-validates the remainder as an [`AgentName`] rather than
/// trusting the round trip.
pub mod ids {
    /// The row's primary click — the agent's name.
    pub const CHAT: &str = "chat:";
    /// The row's pause/resume toggle.
    pub const PAUSE: &str = "pause:";
    /// The row's **details** button (ⓘ).
    ///
    /// Was `edit:` until @kaesaecracker pointed out that a pen means *edit*
    /// and this panel edits nothing — editing is P4/P5, and the panel is
    /// read-only by design (spec §10: "v1 is read-only … it is what the hive
    /// supports"). The glyph and the id now both say what the button does.
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
/// The card cannot bound its own height — see fact 3 in the module docs — so
/// the only real bound is rendering fewer rows. Twelve is
/// @kaesaecracker's live hive and every one of hers is meant to be visible, so
/// this sits comfortably above that while still capping a hive that grows: at
/// roughly one text line per row, twenty rows plus the title is about the
/// height of the sidebar's other three cards put together, and anything past
/// it would push the pet card off-screen with no way to scroll back.
///
/// Overflow is **stated, never silent**: the card draws a "+N more" line and
/// the panel lists the full roster.
pub const MAX_ROWS: usize = 20;

fn label(text: impl Into<String>, classes: &[&str]) -> Node {
    Node::Label {
        id: None,
        text: text.into(),
        tooltip: None,
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
    }
}

fn text(body: impl Into<String>, ellipsize: bool, classes: &[&str]) -> Node {
    Node::Text {
        id: None,
        text: body.into(),
        max_width_chars: None,
        ellipsize,
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
    }
}

/// An ellipsizing single-line label capped at `chars`.
///
/// The cap is what makes the ellipsis actually happen in a row that also
/// holds a [`Node::Spacer`]: without a natural-width bound the label asks for
/// its full text and the spacer has nothing left to give, so the row grows
/// instead of the text shrinking. `Text` carries no `tooltip` field, so the
/// untruncated string lives on the enclosing row's `Box` — see [`agent_row`].
fn clipped(body: impl Into<String>, chars: i32, classes: &[&str]) -> Node {
    Node::Text {
        id: None,
        text: body.into(),
        max_width_chars: Some(chars),
        ellipsize: true,
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
    }
}

fn icon(name: impl Into<String>, classes: &[&str]) -> Node {
    Node::Icon {
        id: None,
        name: name.into(),
        tooltip: None,
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
    }
}

/// An icon whose meaning is not obvious from the glyph, so it carries hover
/// text (#957's `tooltip`, `crates/hytte-plugin-proto/src/wire.rs:244-252`).
fn icon_titled(name: impl Into<String>, hover: impl Into<String>, classes: &[&str]) -> Node {
    Node::Icon {
        id: None,
        name: name.into(),
        tooltip: Some(hover.into()),
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
    }
}

fn button(id: impl Into<String>, classes: &[&str], child: Node) -> Node {
    Node::Button {
        id: id.into(),
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
        child: Box::new(child),
    }
}

/// A horizontal box **with a real gap**.
///
/// Deliberately not [`Node::Row`]: the host builds that as
/// `gtk::Box::new(Horizontal, 0)` with the spacing hardcoded, so an icon and
/// a label rendered as a `Row` come out touching (`⚙argus`). See fact 1 in
/// the module docs.
fn row(spacing: i32, classes: &[&str], children: Vec<Node>) -> Node {
    row_titled(spacing, classes, None, children)
}

/// [`row`] plus hover text for the whole row — the only place an ellipsized
/// `Text`'s full string can live, since `Text` has no `tooltip` of its own.
fn row_titled(
    spacing: i32,
    classes: &[&str],
    tooltip: Option<String>,
    children: Vec<Node>,
) -> Node {
    Node::Box {
        id: None,
        dir: Dir::Horizontal,
        spacing,
        scroll: false,
        tooltip,
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
        children,
    }
}

/// A single explanatory line — the shape every non-`Up` hive state renders.
fn notice(icon_name: &str, body: &str, class: &str) -> Node {
    row_titled(
        6,
        &["ts-agents-notice"],
        Some(body.to_owned()),
        vec![
            icon(icon_name, &["ts-agent-state", class]),
            clipped(body, STATUS_CHARS, &["dim-label"]),
        ],
    )
}

/// How many characters of the harness's status line the row shows before
/// ellipsizing. The rest is on the row's tooltip and in the panel.
const STATUS_CHARS: i32 = 22;

/// How many characters of the agent's display name the row shows.
const NAME_CHARS: i32 = 14;

/// One agent, on **one line** (spec §6.1's two-line mock, compacted).
///
/// The mock was two lines and that is what shipped first; against a real
/// twelve-agent hive it came to roughly 700 px and pushed the pet card off a
/// sidebar that cannot scroll. Same information, one line: the harness text
/// moves inline and ellipsizes, and the **full, untruncated** string becomes
/// the row's hover text — which is why the row is a `Box` and not a `Row`,
/// since `Text` carries no `tooltip` of its own.
fn agent_row(agent: &Agent, cfg: &AgentsConfig) -> Node {
    let name = agent.name.as_str();
    let status = agent.status();

    // Paused shows the resume glyph: the button's icon is what it will DO,
    // which is the only reading that stays honest through the optimistic flip.
    let (pause_icon, pause_hint) = if agent.paused() {
        ("media-playback-start-symbolic", "resume")
    } else {
        ("media-playback-pause-symbolic", "pause")
    };

    let mut children = vec![
        icon(cfg.icon_for(name), &["ts-agent-runtime"]),
        button(
            format!("{}{name}", ids::CHAT),
            &["flat", "ts-agent-name"],
            // Clipped, not a bare `Label`: a `Label`.s natural width forces
            // its container wider (the #281 sidebar blow-out), and an agent
            // name may be up to 63 bytes.
            clipped(cfg.label_for(name), NAME_CHARS, &[]),
        ),
        icon_titled(
            status.icon(),
            status.text(),
            &["ts-agent-state", status.class()],
        ),
        clipped(agent.status_line(), STATUS_CHARS, &["dim-label"]),
        Node::Spacer,
    ];
    if agent.needs_update() {
        // Spec §6.2's badge row says "(tooltip only)" for its text, and until
        // #958 the vocabulary had no tooltip to put it in. It does now.
        children.push(icon_titled(
            UPDATE_BADGE_ICON,
            "config commit pending — a rebuild would change this agent's locked rev",
            &["ts-agent-badge", UPDATE_BADGE_CLASS],
        ));
    }
    children.push(button(
        format!("{}{name}", ids::PAUSE),
        &["flat", "ts-agent-btn"],
        icon_titled(pause_icon, pause_hint, &[]),
    ));
    children.push(button(
        format!("{}{name}", ids::DETAILS),
        &["flat", "ts-agent-btn"],
        // ⓘ, not a pen: this panel edits nothing (spec §10, "v1 is
        // read-only"), and a pen promised an edit that does not exist.
        icon_titled("dialog-information-symbolic", "details", &[]),
    ));

    row_titled(4, &["ts-agent-row"], Some(row_hover(agent, cfg)), children)
}

/// The row's hover text: the display name, then the harness's status
/// **in full** — the string the inline label had to cut.
fn row_hover(agent: &Agent, cfg: &AgentsConfig) -> String {
    let name = agent.name.as_str();
    let label = cfg.label_for(name);
    let mut hover = if label == name {
        name.to_owned()
    } else {
        format!("{label} ({name})")
    };
    hover.push('\n');
    hover.push_str(agent.status_line());
    hover
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

/// The sidebar card: a titled surface, then the rows.
///
/// The title is not decoration — without it the rows started straight under
/// the neighbouring card with nothing saying what they were, which is the
/// first thing @kaesaecracker's screenshot shows. `Tasks`, `Claude usage` and
/// the built-ins all lead with a heading; this now matches. The host supplies
/// the card surface itself (`.ts-plugin-card`, #319) and deliberately no
/// padding, so the root carries `ts-agents-card` for its own inset.
#[must_use]
pub fn card(hive: &Hive, cfg: &AgentsConfig, expanded: &ExpandedGroups) -> Node {
    let mut children = vec![row(
        6,
        &["ts-agents-title"],
        vec![
            label("Agents", &["heading"]),
            Node::Spacer,
            label(hive_summary(hive), &["dim-label", "numeric"]),
        ],
    )];

    match hive {
        Hive::Connecting => children.push(notice(
            "content-loading-symbolic",
            "connecting…",
            "dim-label",
        )),
        Hive::Unreachable { reason } => {
            children.push(notice("network-offline-symbolic", reason, "dim-label"));
        }
        // Reachable, and saying no — a different problem from "no hive", so a
        // different icon (spec §5.3 covers only the unreachable case; this is
        // its reachable sibling).
        Hive::Error { reason } => {
            children.push(notice("dialog-error-symbolic", reason, "error"));
        }
        Hive::Incompatible(mismatch) => children.push(notice(
            "dialog-warning-symbolic",
            &format!(
                "hive protocol v{}, plugin speaks v{}",
                mismatch.theirs, mismatch.ours
            ),
            "warning",
        )),
        Hive::Up { agents } if agents.is_empty() => {
            children.push(notice("system-run-symbolic", "no agents", "dim-label"));
        }
        Hive::Up { agents } => children.extend(roster(agents, cfg, expanded)),
    }

    Node::Box {
        id: Some(ROOT_ID.to_owned()),
        dir: Dir::Vertical,
        spacing: 2,
        scroll: false,
        tooltip: None,
        classes: vec!["ts-agents-card".to_owned()],
        children,
    }
}

/// The roster body: grouped rows, capped at [`MAX_ROWS`].
fn roster(agents: &[Agent], cfg: &AgentsConfig, expanded: &ExpandedGroups) -> Vec<Node> {
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
            out.push(group_node(g, cfg, false, Vec::new()));
            continue;
        }

        let mut rows = Vec::new();
        for agent in &g.agents {
            if drawn >= MAX_ROWS {
                skipped += 1;
                continue;
            }
            rows.push(agent_row(agent, cfg));
            drawn += 1;
        }
        if headers {
            out.push(group_node(g, cfg, true, rows));
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
fn group_node(g: &Group<'_>, cfg: &AgentsConfig, open: bool, rows: Vec<Node>) -> Node {
    let _ = cfg;
    let live = g
        .agents
        .iter()
        .filter(|a| a.status() != Status::Stopped)
        .count();
    Node::Expander {
        id: format!("{}{}", ids::GROUP, g.header()),
        header: Box::new(row(
            6,
            &["ts-agents-group"],
            vec![
                label(g.header(), &["heading"]),
                Node::Spacer,
                label(
                    format!("{live}/{}", g.agents.len()),
                    &["dim-label", "numeric"],
                ),
            ],
        )),
        children: rows,
        expanded: open,
        classes: vec!["ts-agents-group-row".to_owned()],
    }
}

/// Everything the panel needs that the model does not carry: the clock, the
/// socket in use, and when the last poll answered.
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

fn detail(key: &str, value: &str) -> Node {
    row(
        6,
        &["ts-agent-detail"],
        vec![
            label(key, &["dim-label"]),
            Node::Spacer,
            text(value, true, &["numeric"]),
        ],
    )
}

fn flag(key: &str, on: bool) -> Node {
    detail(key, if on { "yes" } else { "no" })
}

/// The hive-level section every panel carries: reachability, the socket in
/// use, and the last poll's age (spec §6.4).
fn hive_section(hive: &Hive, ctx: PanelContext<'_>) -> Vec<Node> {
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
    let mut section = vec![
        label("hive", &["heading"]),
        detail("state", &reachability),
        detail("socket", ctx.socket),
        detail("last poll", &last_poll),
    ];
    // The dashboard root, when the hive says it is reachable from a browser.
    // This is the only thing `Urls` is read for now that the agent page comes
    // off the row itself (hyperhive#4073) — `HiveUrls::home` is `None` under
    // exactly the same condition, so the two lines appear and vanish together.
    if let Some(home) = ctx
        .urls
        .and_then(|u| u.home.as_deref())
        .map(str::trim)
        .filter(|h| !h.is_empty())
    {
        section.push(detail("dashboard", home));
    }
    section
}

/// The drawer panel (spec §6.4): the selected agent's full detail, or the hive
/// overview when nothing is selected.
#[must_use]
pub fn panel(
    hive: &Hive,
    cfg: &AgentsConfig,
    selected: Option<&AgentName>,
    ctx: PanelContext<'_>,
) -> Node {
    let mut children = Vec::new();

    if let Some(agent) = selected.and_then(|name| hive.agent(name)) {
        let name = agent.name.as_str();
        let status = agent.status();
        // `<name> · <project>` — the title row @kaesaecracker's screenshot was
        // missing, and with a real gap (the old `Row` hardcodes spacing 0,
        // which is why it read as `⚙argus`).
        let mut title = vec![
            icon(cfg.icon_for(name), &["ts-agent-runtime"]),
            label(cfg.label_for(name), &["title-4"]),
        ];
        if let Some(project) = cfg.project_for(name) {
            title.push(label("·", &["dim-label"]));
            title.push(label(project, &["dim-label"]));
        }
        title.push(Node::Spacer);
        title.push(icon_titled(
            status.icon(),
            status.text(),
            &["ts-agent-state", status.class()],
        ));
        children.push(row(6, &["ts-agents-panel-head"], title));
        if cfg.label_for(name) != name {
            children.push(detail("agent", name));
        }
        children.push(text(agent.status_line(), false, &["dim-label"]));
        if let Some(at) = agent
            .row
            .status_set_at
            .as_deref()
            .and_then(parse_set_at)
            .filter(|_| ctx.now_unix > 0)
        {
            children.push(detail("status set", &age(ctx.now_unix, at)));
        }

        children.push(label("flags", &["heading"]));
        children.push(flag("running", agent.row.running));
        children.push(flag("failed", agent.row.failed));
        children.push(flag("paused", agent.paused()));
        children.push(flag("needs login", agent.row.needs_login));
        children.push(flag("needs update", agent.row.needs_update));
        if let Some(sha) = agent.row.deployed_sha.as_deref() {
            children.push(detail("deployed sha", sha));
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

        // Spec §11 rule one: both frames are scoped to this one agent.
        children.push(row(
            6,
            &["ts-agents-panel-actions"],
            vec![
                button(
                    format!("{}{name}", ids::START),
                    &["flat"],
                    label("start", &[]),
                ),
                button(
                    format!("{}{name}", ids::STOP),
                    &["flat"],
                    label("stop", &[]),
                ),
                Node::Spacer,
                button(BACK_ID, &["flat"], label("all agents", &[])),
            ],
        ));
        children.push(Node::Separator {
            classes: Vec::new(),
        });
    }

    children.extend(hive_section(hive, ctx));

    Node::Box {
        id: Some(PANEL_ID.to_owned()),
        dir: Dir::Vertical,
        spacing: 4,
        scroll: true,
        tooltip: None,
        classes: vec!["ts-agents-panel".to_owned()],
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_ROWS, STATUS_CHARS, age, ids, parse_set_at};
    use hytte_plugin::proto::Node;

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

    /// The row shows a truncated status but hovers the **full** one.
    ///
    /// This is the whole reason the row is a `Box` rather than a `Row`:
    /// `Node::Text` has no `tooltip` field, so the untruncated string has
    /// nowhere else to live, and without it @kaesaecracker's
    /// `idle — #4077 approved, watching for…` is simply unreadable.
    ///
    /// Falsification: pass `None` as `row_titled`'s tooltip in `agent_row`,
    /// or drop `max_width_chars` from the status `Text`, and one of the two
    /// assertions below goes red.
    #[test]
    fn a_row_clips_its_status_inline_and_hovers_it_in_full() {
        use crate::config::AgentsConfig;
        use crate::hive::wire::AgentStatusRow;
        use crate::model::{Agent, AgentName, ExpandedGroups, Hive};

        const LONG: &str =
            "idle — #4077 approved, watching for new review assignments across the whole swarm";

        let agent = Agent {
            name: AgentName::parse("argus").expect("legal"),
            row: AgentStatusRow {
                name: "argus".to_owned(),
                running: true,
                status_text: Some(LONG.to_owned()),
                ..AgentStatusRow::default()
            },
            pending_paused: None,
        };
        let tree = super::card(
            &Hive::Up {
                agents: vec![agent],
            },
            &AgentsConfig::default(),
            &ExpandedGroups::new(),
        );

        // The inline label is the full string but ellipsized and width-capped,
        // so GTK actually truncates it rather than widening the sidebar.
        let status = find_text(&tree, LONG).expect("the status text renders");
        assert!(status.0, "the inline status must ellipsize");
        assert_eq!(
            status.1,
            Some(STATUS_CHARS),
            "…and be width-capped, or the Spacer leaves it room to grow"
        );

        // The full string is reachable on hover, on the row's own Box.
        let hovers = box_tooltips(&tree);
        assert!(
            hovers.iter().any(|t| t.contains(LONG)),
            "the untruncated status must be the row's hover text; got {hovers:?}"
        );
    }

    /// A hive bigger than the card can show draws [`MAX_ROWS`] rows and then
    /// **says so** — the sidebar cannot scroll, so silently dropping the tail
    /// would be indistinguishable from the agents not existing.
    ///
    /// Falsification: remove the `drawn >= MAX_ROWS` guard in `roster` and the
    /// row count assertion goes red; remove the overflow `notice` and the
    /// "+N more" assertion does.
    #[test]
    fn a_hive_past_the_cap_draws_max_rows_and_says_how_many_it_hid() {
        use crate::config::AgentsConfig;
        use crate::hive::wire::AgentStatusRow;
        use crate::model::{Agent, AgentName, ExpandedGroups, Hive};

        let agents: Vec<Agent> = (0..MAX_ROWS + 7)
            .map(|i| {
                let name = format!("agent-{i}");
                Agent {
                    name: AgentName::parse(&name).expect("legal"),
                    row: AgentStatusRow {
                        name,
                        running: true,
                        ..AgentStatusRow::default()
                    },
                    pending_paused: None,
                }
            })
            .collect();

        let tree = super::card(
            &Hive::Up { agents },
            &AgentsConfig::default(),
            &ExpandedGroups::new(),
        );
        let rendered = count_class(&tree, "ts-agent-row");
        assert_eq!(rendered, MAX_ROWS, "the card must cap what it draws");

        let hovers = box_tooltips(&tree);
        assert!(
            hovers.iter().any(|t| t.contains("+7 more")),
            "the hidden tail must be stated, not silent; got {hovers:?}"
        );
    }

    /// `(ellipsize, max_width_chars)` of the first `Text` whose body is
    /// `needle`.
    fn find_text(node: &Node, needle: &str) -> Option<(bool, Option<i32>)> {
        match node {
            Node::Text {
                text,
                ellipsize,
                max_width_chars,
                ..
            } if text == needle => Some((*ellipsize, *max_width_chars)),
            Node::Box { children, .. }
            | Node::Row { children, .. }
            | Node::ListBox { children, .. } => children.iter().find_map(|c| find_text(c, needle)),
            Node::Expander {
                header, children, ..
            } => find_text(header, needle)
                .or_else(|| children.iter().find_map(|c| find_text(c, needle))),
            Node::Button { child, .. } => find_text(child, needle),
            _ => None,
        }
    }

    /// Every `Box` tooltip in the tree.
    fn box_tooltips(node: &Node) -> Vec<String> {
        fn walk(node: &Node, out: &mut Vec<String>) {
            match node {
                Node::Box {
                    tooltip, children, ..
                } => {
                    if let Some(t) = tooltip {
                        out.push(t.clone());
                    }
                    for c in children {
                        walk(c, out);
                    }
                }
                Node::Row { children, .. } | Node::ListBox { children, .. } => {
                    for c in children {
                        walk(c, out);
                    }
                }
                Node::Expander {
                    header, children, ..
                } => {
                    walk(header, out);
                    for c in children {
                        walk(c, out);
                    }
                }
                Node::Button { child, .. } => walk(child, out),
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(node, &mut out);
        out
    }

    /// How many nodes carry `class`.
    fn count_class(node: &Node, class: &str) -> usize {
        let hit = usize::from(match node {
            Node::Box { classes, .. }
            | Node::Row { classes, .. }
            | Node::ListBox { classes, .. }
            | Node::Expander { classes, .. } => classes.iter().any(|c| c == class),
            _ => false,
        });
        let kids = match node {
            Node::Box { children, .. }
            | Node::Row { children, .. }
            | Node::ListBox { children, .. } => {
                children.iter().map(|c| count_class(c, class)).sum()
            }
            Node::Expander {
                header, children, ..
            } => {
                count_class(header, class)
                    + children
                        .iter()
                        .map(|c| count_class(c, class))
                        .sum::<usize>()
            }
            Node::Button { child, .. } => count_class(child, class),
            _ => 0,
        };
        hit + kids
    }

    /// Spec §6.2's badge is "(tooltip only)" — the glyph alone does not say
    /// what `needs_update` means, and the row has no room for the words. Now
    /// that #958 put a tooltip in the vocabulary, the badge and the state
    /// glyph both carry one.
    ///
    /// Falsification: swap `icon_titled` back to `icon` for either and the
    /// matching assertion goes red.
    #[test]
    fn the_unlabelled_glyphs_carry_hover_text() {
        use crate::config::AgentsConfig;
        use crate::hive::wire::AgentStatusRow;
        use crate::model::{Agent, AgentName, Hive};

        /// Every `Icon` in the tree, as `(name, tooltip)`.
        fn icons(node: &Node, out: &mut Vec<(String, Option<String>)>) {
            match node {
                Node::Icon { name, tooltip, .. } => out.push((name.clone(), tooltip.clone())),
                Node::Box { children, .. }
                | Node::Row { children, .. }
                | Node::ListBox { children, .. } => {
                    for c in children {
                        icons(c, out);
                    }
                }
                Node::Button { child, .. } => icons(child, out),
                _ => {}
            }
        }

        let agent = Agent {
            name: AgentName::parse("busy").expect("legal"),
            row: AgentStatusRow {
                name: "busy".to_owned(),
                running: true,
                needs_update: true,
                ..AgentStatusRow::default()
            },
            pending_paused: None,
        };
        let tree = super::card(
            &Hive::Up {
                agents: vec![agent],
            },
            &AgentsConfig::default(),
            &crate::model::ExpandedGroups::new(),
        );

        let mut found = Vec::new();
        icons(&tree, &mut found);

        let badge = found
            .iter()
            .find(|(n, _)| n == crate::model::UPDATE_BADGE_ICON)
            .expect("the update badge renders");
        assert!(
            badge.1.as_deref().is_some_and(|t| t.contains("rebuild")),
            "the badge's meaning lives in its tooltip: {badge:?}"
        );

        let state = found
            .iter()
            .find(|(n, _)| n == "media-playback-start-symbolic")
            .expect("the running state glyph renders");
        assert_eq!(state.1.as_deref(), Some("running"));
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
    }
}
