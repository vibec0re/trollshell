//! The node tree: the sidebar card (spec §6.1) and the drawer panel (§6.4).
//!
//! Every node used here already exists in the wire vocabulary — `Box`
//! (`crates/hytte-plugin-proto/src/wire.rs:125`), `Row` (`:137`), `ListBox`
//! (`:146`), `Label` (`:152`), `Text` (`:179`), `Icon` (`:189`), `Button`
//! (`:250`), `Spacer` (`:343`). **No proto change, no `VOCAB` bump.**
//!
//! `ListBox` materializes as a selection-less list
//! (`crates/hytte-plugin-proto/src/wire.rs:141-150`), so there is no
//! row-activate event: every interaction is a `Button`, whose `id` is required
//! and is the click target (`:249-255`). That is why the agent's name is a
//! button rather than a label.

use hytte_plugin::proto::{Dir, Node};

use crate::config::AgentsConfig;
use crate::model::{
    Agent, AgentName, Group, Hive, UPDATE_BADGE_CLASS, UPDATE_BADGE_ICON, agent_url, group,
    headers_wanted,
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
    /// The row's detail button.
    pub const EDIT: &str = "edit:";
    /// The panel's per-agent start.
    pub const START: &str = "start:";
    /// The panel's per-agent stop.
    pub const STOP: &str = "stop:";
}

fn label(text: impl Into<String>, classes: &[&str]) -> Node {
    Node::Label {
        id: None,
        text: text.into(),
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

fn icon(name: impl Into<String>, classes: &[&str]) -> Node {
    Node::Icon {
        id: None,
        name: name.into(),
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

fn row(classes: &[&str], children: Vec<Node>) -> Node {
    Node::Row {
        id: None,
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
        children,
    }
}

fn column(classes: &[&str], children: Vec<Node>) -> Node {
    Node::Box {
        id: None,
        dir: Dir::Vertical,
        spacing: 2,
        scroll: false,
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
        children,
    }
}

/// A single explanatory row — the shape every non-`Up` hive state renders.
fn notice(icon_name: &str, head: &str, body: &str, class: &str) -> Node {
    column(
        &["ts-agent-row", "ts-agents-notice"],
        vec![
            row(
                &["ts-agent-head"],
                vec![
                    icon(icon_name, &["ts-agent-state", class]),
                    label(head, &["heading"]),
                ],
            ),
            row(&["ts-agent-foot"], vec![text(body, true, &["dim-label"])]),
        ],
    )
}

/// One agent's two-line card (spec §6.1).
fn agent_row(agent: &Agent, cfg: &AgentsConfig) -> Node {
    let name = agent.name.as_str();
    let status = agent.status();

    let mut head = vec![
        icon(cfg.icon_for(name), &["ts-agent-runtime"]),
        button(
            format!("{}{name}", ids::CHAT),
            &["flat", "ts-agent-name"],
            label(cfg.label_for(name), &[]),
        ),
        Node::Spacer,
    ];
    if agent.needs_update() {
        head.push(icon(
            UPDATE_BADGE_ICON,
            &["ts-agent-badge", UPDATE_BADGE_CLASS],
        ));
    }
    head.push(icon(status.icon(), &["ts-agent-state", status.class()]));

    // Paused shows the resume glyph: the button's icon is what it will DO,
    // which is the only reading that stays honest through the optimistic flip.
    let pause_icon = if agent.paused() {
        "media-playback-start-symbolic"
    } else {
        "media-playback-pause-symbolic"
    };

    let foot = row(
        &["ts-agent-foot"],
        vec![
            text(agent.status_line(), true, &["dim-label"]),
            Node::Spacer,
            button(
                format!("{}{name}", ids::PAUSE),
                &["flat", "ts-agent-pause"],
                icon(pause_icon, &[]),
            ),
            button(
                format!("{}{name}", ids::EDIT),
                &["flat", "ts-agent-edit"],
                icon("document-edit-symbolic", &[]),
            ),
        ],
    );

    column(&["ts-agent-row"], vec![row(&["ts-agent-head"], head), foot])
}

/// The sidebar card.
#[must_use]
pub fn card(hive: &Hive, cfg: &AgentsConfig) -> Node {
    let children = match hive {
        Hive::Connecting => vec![notice(
            "content-loading-symbolic",
            "hive",
            "connecting…",
            "dim-label",
        )],
        Hive::Unreachable { reason } => vec![notice(
            "network-offline-symbolic",
            "no hive",
            reason,
            "dim-label",
        )],
        // Reachable, and saying no — a different problem from "no hive", so a
        // different head and a different icon (spec §5.3 covers only the
        // unreachable case; this is its reachable sibling).
        Hive::Error { reason } => vec![notice(
            "dialog-error-symbolic",
            "hive error",
            reason,
            "error",
        )],
        Hive::Incompatible(mismatch) => vec![notice(
            "dialog-warning-symbolic",
            "hive protocol mismatch",
            &format!(
                "hive protocol v{}, plugin speaks v{}",
                mismatch.theirs, mismatch.ours
            ),
            "warning",
        )],
        Hive::Up { agents } if agents.is_empty() => vec![notice(
            "system-run-symbolic",
            "hive",
            "no agents",
            "dim-label",
        )],
        Hive::Up { agents } => {
            let groups = group(agents, cfg);
            let headers = headers_wanted(&groups);
            let mut children = Vec::new();
            for g in &groups {
                if headers {
                    children.push(group_header(g));
                }
                children.extend(g.agents.iter().map(|a| agent_row(a, cfg)));
            }
            children
        }
    };

    Node::ListBox {
        id: Some(ROOT_ID.to_owned()),
        classes: vec!["ts-agents-list".to_owned()],
        children,
    }
}

fn group_header(g: &Group<'_>) -> Node {
    label(g.header(), &["heading", "dim-label", "ts-agents-group"])
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
        children.push(row(
            &["ts-agents-panel-head"],
            vec![
                icon(cfg.icon_for(name), &["ts-agent-runtime"]),
                label(cfg.label_for(name), &["title-4"]),
                Node::Spacer,
                icon(status.icon(), &["ts-agent-state", status.class()]),
            ],
        ));
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
        classes: vec!["ts-agents-panel".to_owned()],
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::{age, ids, parse_set_at};

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

    /// The id prefixes are the reducer's parsing contract; a colon terminator
    /// is what makes `strip_prefix` unambiguous against a name whitelist that
    /// excludes `:`.
    #[test]
    fn every_button_prefix_ends_in_a_colon() {
        for prefix in [ids::CHAT, ids::PAUSE, ids::EDIT, ids::START, ids::STOP] {
            assert!(prefix.ends_with(':'), "{prefix}");
        }
    }
}
