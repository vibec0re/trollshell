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
//!   ran off the bottom — and the overview carries a roster, which is exactly
//!   the content that gets long. Its own row cap is [`PANEL_MAX_ROWS`], which
//!   bounds the **frame** rather than the pixels; [`MAX_ROWS`] bounds the card. `Scrolled` is a negotiated
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
    /// The `agent page` link — opens that agent's own URL in the desktop's
    /// default handler (#1045).
    ///
    /// Carries the **agent name**, not the URL: the id travels to the host and
    /// comes back on the click, and this crate's standing rule is that nothing
    /// a click hands back is trusted as data (see [`super::ids::PAUSE`]'s
    /// neighbours in `plugin.rs`, each of which re-parses its name). The URL is
    /// re-read from the model instead, so a row whose agent has since vanished
    /// opens nothing rather than opening a stale string.
    pub const OPEN: &str = "open:";
    /// The hive dashboard link on the panel — the one link that names no
    /// agent, so it is a whole id rather than a prefix. Not `OPEN`-prefixed:
    /// `strip_prefix("open:")` must never match it by accident.
    pub const OPEN_DASHBOARD: &str = "open-dashboard";
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
/// the panel's overview lists up to [`PANEL_MAX_ROWS`] of them.
pub const MAX_ROWS: usize = 20;

/// The drawer panel's viewport cap, in pixels.
///
/// The plugin drawer child has no scroller of its own, so this is the panel's
/// only bound. A child shorter than the cap is not stretched, so the number
/// only ever matters for a panel that would otherwise overflow — which, with
/// the full roster on the overview, is any hive past a handful of agents.
///
/// **560 is a constant, and #701 is why that is a known limitation rather than
/// a good number.** It is the shell's own former Stats-page cap
/// (`stats_scrolled(&grid, 560)`, now `OLD_STATS_CAP` in `modal.rs`'s tests),
/// and #701 replaced it precisely because a constant is wrong on a real
/// output: on a 1440-tall screen the page clamped itself to 560 and scrolled
/// with the bottom half empty. The shell now derives that budget from the
/// monitor (`modal::clamp_card_height`); a plugin cannot, because the
/// vocabulary hands it no drawer height. So this errs toward the size of the
/// drawer in @kaesaecracker's screenshots (~600 px of content) and the
/// follow-up is a host-supplied height hint, not a better guess here.
pub const PANEL_VIEWPORT_PX: u16 = 560;

/// How many characters of the agent's display name line 1 shows.
const NAME_CHARS: i32 = 20;

/// The same, in the panel's roster — the drawer is wider than the sidebar.
const PANEL_NAME_CHARS: i32 = 32;

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

/// The wrap width of the **card's** free-text lines (a state notice's reason,
/// and the agent row's status caption).
///
/// The card sits inside the sidebar's `AdwClamp(320)`, so these would wrap
/// anyway; the cap is here so the two surfaces are bounded by the same rule
/// rather than one of them by an ancestor that happens to exist.
const CARD_TEXT_CHARS: i32 = 34;

/// The wrap width of the panel's free-text lines (the hive name under a renamed
/// header, and the agent's status).
///
/// **The drawer has nothing else bounding it.** The sidebar card is inside
/// `AdwClamp(320)` (`trollshell/src/overlays/sidebar.rs`), which is why a
/// wrapping `Text` wraps *there*; the plugin drawer child is two bare
/// `gtk::Box`es (`trollshell/src/plugins/region.rs`, `build_panel_child`), the
/// page's `AdwClamp` is never applied to a plugin panel, and the drawer's
/// `set_size_request` is a **minimum**. [`Node::Scrolled`] does not help either
/// — the host gives it `PolicyType::Never` horizontally, which propagates the
/// child's natural width unchanged. So a `Text` with no `max_width_chars`
/// reports its whole string as its natural width and *widens the drawer*: the
/// #281 blow-out, one surface over.
const PANEL_TEXT_CHARS: i32 = 56;

/// The ellipsized width of a project group's header.
///
/// `[display.<name>].project` is operator-typed and unbounded; the card's
/// `AdwClamp` stops it widening the sidebar but would clip it rather than
/// ellipsize, so it says nothing about what got cut.
const GROUP_CHARS: i32 = 22;

/// The most rows the **panel's** roster draws.
///
/// One paragraph for both caps, because they bound different things and #963's
/// review was right that quoting one justification for both is not honest:
///
/// - [`MAX_ROWS`] (20) bounds the **card**, which is 320 px of a sidebar that
///   holds three other cards. Its job is legibility; overflow says `+N more`
///   and points at this roster.
/// - This one bounds the **frame**. The card tree and the panel tree ship
///   together on every render, and the host truncates any tree past
///   `MAX_NODES_PER_TREE = 4096`
///   (`crates/hytte-plugin-proto/src/wire.rs`) keeping the prefix, with one
///   `tracing::warn!` per plugin per shell run — i.e. a silently short roster
///   on exactly the runaway hive the bound exists for. At ~7 nodes per
///   [`panel_roster_row`] that ceiling is around 580 rows, so 200 sits well
///   under it with the rest of the page's nodes to spare, and far enough above
///   any real hive that the `+N more` line is a statement about the hive rather
///   than about this plugin.
pub const PANEL_MAX_ROWS: usize = 200;

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

/// A **wrapping** label: the whole string, on as many lines as it needs, wrapped
/// at `chars` rather than at whatever its container happens to be.
///
/// The width is not optional. A `Text` with `max_width_chars: None` wraps only
/// where an ancestor constrains it, and on the drawer page nothing does — see
/// [`PANEL_TEXT_CHARS`]. Passing the cap explicitly is what keeps the two
/// surfaces from silently behaving differently.
fn wrapped(body: impl Into<String>, chars: i32, classes: &[&str]) -> Node {
    Node::Text {
        id: None,
        text: body.into(),
        max_width_chars: Some(chars),
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

/// [`clipped`] with an **explicit** hover instead of the node's own full text —
/// for the cases where the hover says more than the label does.
fn clipped_titled(
    body: impl Into<String>,
    chars: i32,
    hover: impl Into<String>,
    classes: &[&str],
) -> Node {
    Node::Text {
        id: None,
        text: body.into(),
        max_width_chars: Some(chars),
        ellipsize: true,
        tooltip: Some(hover.into()),
        classes: cls(classes),
    }
}

/// What an agent name's hover says: the display label, **plus the hive's own
/// name** whenever a `[display.<name>].label` renamed it.
///
/// The hive's name is what `hivectl` takes, what every request frame carries
/// and what the logs print, so a renamed row that shows only the label leaves
/// no way to find out what to type. This is the one thing the old row-level
/// tooltip carried that the status caption does not — the status now shows in
/// full on line 2, so this is all that is left of it.
fn name_hover(name: &str, cfg: &AgentsConfig) -> String {
    let label = cfg.label_for(name);
    if label == name {
        name.to_owned()
    } else {
        format!("{label} ({name})")
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

/// A [`detail`] whose value is a **link button** (#1045).
///
/// Same key/value rhythm as every other row — only the value is wrapped in a
/// [`Node::Button`], so the click has somewhere to land. @kaesaecracker's
/// 2026-09-10 retest is what this answers: the URL rendered as a plain,
/// ellipsized string and there was no way to follow it.
///
/// # Why there is no disabled variant
///
/// Every caller reaches this inside an `if let Some(url)` over
/// [`agent_url`](crate::model::agent_url) / [`hive_home`], both of which
/// already fold an absent **and** an all-whitespace value to `None`. A row
/// with no URL therefore does not render at all — which is the whole of "no
/// dead button": there is no state in which this draws something clickable
/// that opens nothing. The pre-#1045 behaviour for that case (no row) is
/// unchanged.
///
/// The hover says `open <url>` rather than repeating the text, because the
/// text is the URL and the thing a hover has to add is that clicking it does
/// something.
fn link_detail(id: impl Into<String>, key: &str, url: &str) -> Node {
    hrow(
        6,
        &["ts-agent-detail"],
        vec![
            label(key, &["dim-label", "caption"]),
            Node::Spacer,
            button(
                id,
                // `link` is GTK's own style class for a button that looks like
                // a link; `flat` drops the frame, and `ts-agent-link` is this
                // plugin's scoped rule (see `assets/trollshell/style.css`).
                &["flat", "link", "ts-agent-link"],
                clipped_titled(
                    url,
                    VALUE_CHARS,
                    format!("open {url}"),
                    &["caption", "numeric"],
                ),
            ),
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
            wrapped(body, CARD_TEXT_CHARS, &["dim-label", "caption"]),
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
        wrapped(
            line,
            CARD_TEXT_CHARS,
            &["dim-label", "caption", "ts-agent-status"],
        )
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
            clipped_titled(
                cfg.label_for(name),
                NAME_CHARS,
                name_hover(name, cfg),
                &["heading"],
            ),
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
        children.push(link_detail(
            format!("{}{}", ids::OPEN, agent.name.as_str()),
            "agent page",
            url,
        ));
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

/// One project group as an `Expander` — header plus its rows, the rows in a
/// **nested list of their own**.
///
/// The nesting is not decoration, it is what keeps the two roster paths looking
/// alike (#963 review, LOW-1). GTK auto-wraps each *direct* child of a
/// `GtkListBox` in a `GtkListBoxRow`, and that wrapper is what `boxed-list`'s
/// hairline separators key off. An `Expander` is one such child no matter how
/// many agents it reveals, so with a `[display.*].project` set the outer list
/// would draw one separator per **group** and none between the rows inside it —
/// the same twelve agents rendering two different ways depending on a config
/// key. Giving the expander's body its own dense `ListBox` restores the
/// per-row separator, and leaves the outer wrapper separating groups, which is
/// the distinction that should be visible.
fn group_node(g: &Group<'_>, open: bool, rows: Vec<Node>) -> Node {
    // An empty body would otherwise emit a list with no children, which paints
    // a stray surface under a collapsed header.
    let body = if rows.is_empty() {
        Vec::new()
    } else {
        vec![list(true, &["ts-agents-group-list"], rows)]
    };
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
                // `[display.<name>].project` is operator-typed and unbounded;
                // a `Label` would clip without saying so (#963 review, LOW-5).
                clipped(g.header(), GROUP_CHARS, &["heading"]),
                Node::Spacer,
                label(
                    format!("{live}/{}", g.agents.len()),
                    &["dim-label", "caption", "numeric"],
                ),
            ],
        )),
        children: body,
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
    // Reachability, not links: the dashboard root lives in the `links` group,
    // once — an agent's page and the hive overview both used to emit it, which
    // put two `dashboard` rows on the same panel.
    let rows = vec![
        detail("state", &reachability),
        detail("socket", ctx.socket),
        detail("last poll", &last_poll),
    ];
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
                clipped_titled(
                    cfg.label_for(name),
                    PANEL_NAME_CHARS,
                    name_hover(name, cfg),
                    &[],
                ),
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
            // Bounded, and a `Text` rather than a `Label`: a `Label` neither
            // wraps nor ellipsizes, so a 63-byte agent name (legal on the wire)
            // reports its whole self as the header's natural width and widens
            // the drawer — see [`PANEL_TEXT_CHARS`]. The card guarded this from
            // the start (#281); the panel header did not until #963's review.
            clipped_titled(
                cfg.label_for(name),
                PANEL_NAME_CHARS,
                name_hover(name, cfg),
                &["title-4"],
            ),
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
        out.push(wrapped(
            name,
            PANEL_TEXT_CHARS,
            &["dim-label", "caption", "ts-mono"],
        ));
    }
    out.push(wrapped(
        agent.status_line(),
        PANEL_TEXT_CHARS,
        &["dim-label"],
    ));

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

    // Both rows in this section are links, so both are buttons — a section
    // where one URL is clickable and its neighbour is not would be the exact
    // inconsistency #1045 was filed about, one row further down.
    let mut links = Vec::new();
    if let Some(url) = agent_url(agent) {
        links.push(link_detail(
            format!("{}{}", ids::OPEN, agent.name.as_str()),
            "agent page",
            url,
        ));
    }
    if let Some(home) = hive_home(ctx) {
        links.push(link_detail(ids::OPEN_DASHBOARD, "dashboard", home));
    }
    if !links.is_empty() {
        out.push(section(
            "links",
            list(false, &["ts-agents-panel-list"], links),
        ));
    }

    out
}

/// The "all agents" row — the panel's only way to drop a selection.
///
/// Lives here rather than at the end of [`agent_page`] because it has to be
/// emitted for a selection the roster **cannot resolve** too, which is exactly
/// the case that has no agent page: see [`panel`].
fn back_row() -> Node {
    hrow(
        6,
        &["ts-agents-panel-actions"],
        vec![
            Node::Spacer,
            button(BACK_ID, &["flat"], label("all agents", &[])),
        ],
    )
}

/// The drawer panel (spec §6.4): the selected agent's full detail, or the hive
/// overview plus the roster when no agent page is shown.
///
/// The roster is what makes the card's `+N more — open the panel for the full
/// roster` line true; before #963's UI round the panel showed no rows at all. It
/// is capped at [`PANEL_MAX_ROWS`] rather than uncapped — see that constant for
/// why the two caps exist and bound different things.
#[must_use]
pub fn panel(
    hive: &Hive,
    cfg: &AgentsConfig,
    selected: Option<&AgentName>,
    ctx: PanelContext<'_>,
) -> Node {
    // **Resolve once, then gate on the resolution.** `selected.is_some()` and
    // `hive.agent(selected)` are not complements: a selection whose lookup
    // fails — every render while the hive is `Unreachable` / `Error` /
    // `Connecting`, and the one render after an agent leaves the roster — used
    // to satisfy neither branch, so the page rendered the hive section and
    // nothing else: no agent page, no links, no roster, and no way back,
    // because the "all agents" button lived inside the agent page (#963
    // review, MED-1). `docs/live-verify.md`'s own "stop `hive-c0re`" step walks
    // straight into it.
    let shown = selected.and_then(|name| hive.agent(name));
    let mut children = Vec::new();

    if let Some(agent) = shown {
        children.extend(agent_page(agent, cfg, ctx));
    } else if let Some(name) = selected {
        // Say why the page is not here, rather than silently showing the
        // overview under a title the operator did not ask for.
        children.push(notice(
            "dialog-information-symbolic",
            &format!(
                "{} is not in the hive's current roster",
                cfg.label_for(name.as_str())
            ),
            "dim-label",
        ));
    }
    if selected.is_some() {
        children.push(back_row());
    }

    children.push(hive_section(hive, ctx));

    // The overview's own links group. The selected-agent page emits its own
    // (agent page + dashboard), so this is the branch that keeps the dashboard
    // reachable when no agent page is shown — without either panel showing it
    // twice.
    if shown.is_none()
        && let Some(home) = hive_home(ctx)
    {
        children.push(section(
            "links",
            list(
                false,
                &["ts-agents-panel-list"],
                vec![detail("dashboard", home)],
            ),
        ));
    }

    if shown.is_none() {
        let agents = hive.agents();
        if !agents.is_empty() {
            let drawn = agents.len().min(PANEL_MAX_ROWS);
            let mut rows: Vec<Node> = agents
                .iter()
                .take(PANEL_MAX_ROWS)
                .map(|a| panel_roster_row(a, cfg))
                .collect();
            // Stated, never silent — the same rule the card's overflow follows,
            // and the reason the panel is no longer described as "uncapped"
            // (#963 review, LOW-3).
            if agents.len() > drawn {
                rows.push(notice(
                    "view-more-symbolic",
                    &format!(
                        "+{} more — the hive is larger than this page draws",
                        agents.len() - drawn
                    ),
                    "dim-label",
                ));
            }
            children.push(section(
                "roster",
                list(false, &["ts-agents-panel-list"], rows),
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
        MAX_ROWS, PANEL_MAX_ROWS, PANEL_VIEWPORT_PX, STATUS_CHARS, STATUS_WRAP_MAX, age, ids,
        parse_set_at,
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
        // A **wrap** width, not a truncation: with `ellipsize: false` the host
        // sets `set_wrap(true)`, so `max_width_chars` caps the label's natural
        // width and the text flows onto more lines rather than being cut. It
        // was `None` until #963's review, which is the same absent bound MED-2
        // found widening the drawer one surface over — the string is shown in
        // full either way, but only this way without pushing its container.
        assert_eq!(
            status.max_width_chars,
            Some(super::CARD_TEXT_CHARS),
            "line 2 must cap its natural width while still showing everything"
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

    /// The `agent page` row is a **button** — and it exists at all only when
    /// there is a URL behind it (#1045).
    ///
    /// Both halves matter. The first is @kaesaecracker's 2026-09-10 finding:
    /// the URL rendered and there was no way to follow it. The second is the
    /// "no dead button" rule — `running()` builds a row whose `url` is `None`,
    /// and the unfolded details then carry neither a link nor the key that
    /// labels one, so there is no state in which this draws something
    /// clickable that opens nothing.
    ///
    /// Falsification: put `detail` back at `details_block`'s `agent_url` call
    /// site and the first assertion reds; drop that site's `if let Some(url)`
    /// guard (rendering the link with an empty string) and the last two do.
    #[test]
    fn the_agent_page_row_is_a_button_only_when_there_is_a_url() {
        let name = AgentName::parse("argus").expect("legal");

        let mut with_url = running("argus", "idle");
        with_url.row.url = Some("https://hive.local/agent/argus/".to_owned());
        let open = card_of(vec![with_url], Some(&name));
        assert!(
            button_ids(&open).iter().any(|id| id == "open:argus"),
            "the URL has to be followable: {:?}",
            button_ids(&open)
        );

        let bare = card_of(vec![running("argus", "idle")], Some(&name));
        assert!(
            !button_ids(&bare)
                .iter()
                .any(|id| id.starts_with(super::ids::OPEN)),
            "no URL means no button: {:?}",
            button_ids(&bare)
        );
        let mut labelled = false;
        walk(&bare, &mut |n| {
            if let Node::Label { text, .. } = n
                && text == "agent page"
            {
                labelled = true;
            }
        });
        assert!(!labelled, "…and no orphaned key either");
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

    /// Whatever the card hides, the panel's overview shows (up to its own, far
    /// higher cap). That is
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

    /// A `[display.<name>].label` renames the row, and the hive's own name is
    /// what `hivectl`, every request frame and every log line use — so the
    /// hover has to carry it, on the card **and** in the panel's roster.
    ///
    /// This is the one thing the retired row-level tooltip carried that the
    /// status caption does not, so it is the one that had to move rather than
    /// simply go.
    ///
    /// Falsification: hand `clipped_titled` the label instead of
    /// `name_hover(name, cfg)` at either call site and this goes red.
    #[test]
    fn a_renamed_row_still_hovers_the_hives_own_name() {
        let mut cfg = AgentsConfig::default();
        cfg.display.insert(
            "trollshell-choom".to_owned(),
            crate::config::Display {
                label: Some("choom".to_owned()),
                icon: None,
                project: None,
            },
        );
        let hive = Hive::Up {
            agents: vec![running("trollshell-choom", "idle")],
        };

        for tree in [
            super::card(&hive, &cfg, &ExpandedGroups::new(), None),
            super::panel(&hive, &cfg, None, ctx()),
        ] {
            let name = find_text(&tree, "choom").expect("the label renders");
            assert_eq!(
                name.tooltip.as_deref(),
                Some("choom (trollshell-choom)"),
                "a renamed row must still say what the hive calls it"
            );
        }

        // An agent nobody renamed hovers its plain name, not `name (name)`.
        let plain = super::card(
            &Hive::Up {
                agents: vec![running("argus", "idle")],
            },
            &AgentsConfig::default(),
            &ExpandedGroups::new(),
            None,
        );
        assert_eq!(
            find_text(&plain, "argus")
                .expect("renders")
                .tooltip
                .as_deref(),
            Some("argus")
        );
    }

    /// A selection the roster cannot resolve must not strand the panel.
    ///
    /// `selected.is_some()` and `hive.agent(selected)` are not complements, and
    /// before #963's review neither branch ran when they disagreed: the page
    /// rendered the hive section and nothing else — no agent page, no roster,
    /// and no way back, because `BACK_ID` lived inside the agent page. That is
    /// every render while the hive is down, which is exactly what
    /// `docs/live-verify.md`'s "stop `hive-c0re`" step does.
    ///
    /// Falsification: gate the overview on `selected.is_none()` again, or move
    /// `back_row()` back inside `agent_page`, and this goes red.
    #[test]
    fn an_unresolvable_selection_falls_back_to_the_overview_with_a_way_out() {
        let cfg = AgentsConfig::default();
        let name = AgentName::parse("argus").expect("legal");
        let back = super::BACK_ID.to_owned();

        // Every render while the hive is unreachable.
        let down = super::panel(
            &Hive::Unreachable {
                reason: "connection refused".to_owned(),
            },
            &cfg,
            Some(&name),
            ctx(),
        );
        assert!(
            button_ids(&down).contains(&back),
            "a selection with no page still needs a way out; got {:?}",
            button_ids(&down)
        );
        assert!(
            texts(&down)
                .iter()
                .any(|t| t.contains("not in the hive's current roster")),
            "…and it must say why the page is missing: {:?}",
            texts(&down)
        );

        // A live hive that no longer has the agent still lists the ones it has.
        let moved_on = super::panel(
            &Hive::Up {
                agents: vec![running("bosun", "idle")],
            },
            &cfg,
            Some(&name),
            ctx(),
        );
        let ids = button_ids(&moved_on);
        assert!(ids.contains(&back));
        assert!(
            ids.contains(&"chat:bosun".to_owned()),
            "the overview's roster must render when the selection cannot: {ids:?}"
        );

        // …and a selection that *does* resolve still gets its page instead.
        let resolved = super::panel(
            &Hive::Up {
                agents: vec![running("argus", "idle")],
            },
            &cfg,
            Some(&name),
            ctx(),
        );
        let ids = button_ids(&resolved);
        assert!(ids.contains(&"start:argus".to_owned()), "{ids:?}");
        assert!(ids.contains(&back));
        assert!(
            !ids.contains(&"chat:argus".to_owned()),
            "the agent page replaces the roster, it does not double it: {ids:?}"
        );
    }

    /// Nothing either surface renders reports an **unbounded natural width**.
    ///
    /// The card is inside the sidebar's `AdwClamp(320)`, so a wrapping label
    /// wraps there; the drawer page has no such ancestor — `build_panel_child`
    /// is two bare `gtk::Box`es, the drawer's `set_size_request` is a *minimum*,
    /// and `Node::Scrolled` is `PolicyType::Never` horizontally, so it
    /// propagates the child's natural width unchanged. A `Text` with no
    /// `max_width_chars`, or any `Label` at all, therefore widens the whole
    /// drawer: the #281 blow-out one surface over (#963 review, MED-2/LOW-5).
    ///
    /// Falsification: drop the cap from either `wrapped` call in `agent_page`,
    /// swap the panel header back to `label(…, &["title-4"])`, or make the
    /// group header a plain `label` again, and this goes red.
    #[test]
    fn no_free_text_reports_an_unbounded_width_on_either_surface() {
        // 63 bytes — `AgentName::MAX_LEN`, and legal on the wire.
        let long_name = "a".repeat(63);
        let long_status = ["watching for review assignments"; 7].join(", ");
        let long_url = format!("https://hive.local/agent/{long_name}/");
        let long_project = "p".repeat(80);

        let mut cfg = AgentsConfig::default();
        cfg.display.insert(
            long_name.clone(),
            crate::config::Display {
                label: Some(format!("{long_name}-renamed")),
                icon: None,
                project: Some(long_project.clone()),
            },
        );
        // A second project so the group headers are not suppressed.
        cfg.display.insert(
            "bosun".to_owned(),
            crate::config::Display {
                label: None,
                icon: None,
                project: Some("other".to_owned()),
            },
        );

        let hive = Hive::Up {
            agents: vec![
                agent(
                    &long_name,
                    AgentStatusRow {
                        name: long_name.clone(),
                        running: true,
                        status_text: Some(long_status.clone()),
                        url: Some(long_url.clone()),
                        deployed_sha: Some("0123456789abcdef0123".to_owned()),
                        ..AgentStatusRow::default()
                    },
                ),
                running("bosun", "idle"),
            ],
        };
        let selected = AgentName::parse(&long_name).expect("63 bytes is legal");

        let surfaces = [
            (
                "card",
                super::card(&hive, &cfg, &ExpandedGroups::new(), Some(&selected)),
            ),
            (
                "panel/agent",
                super::panel(&hive, &cfg, Some(&selected), ctx()),
            ),
            ("panel/overview", super::panel(&hive, &cfg, None, ctx())),
        ];

        for (where_, tree) in &surfaces {
            let loose = unbounded_texts(tree);
            assert!(
                loose.is_empty(),
                "{where_}: every Text must cap its natural width; loose: {loose:?}"
            );
            // A `Label` neither wraps nor ellipsizes at all, so no *dynamic*
            // string may be rendered as one. The short constants (`hive`,
            // `deployed`, `all agents`, the chips) are fine and stay `Label`s.
            for dynamic in [&long_name, &long_status, &long_url, &long_project] {
                assert!(
                    !label_texts(tree).iter().any(|t| t.contains(dynamic)),
                    "{where_}: {dynamic:?} is rendered as an unwrappable Label"
                );
            }
        }
    }

    /// The panel's roster is capped too, and states its overflow — the card's
    /// `+N more` points here, so "here" must not be the unbounded half of the
    /// same argument (#963 review, LOW-3).
    ///
    /// Falsification: drop the `.take(PANEL_MAX_ROWS)` and the row-count
    /// assertion goes red; drop the overflow `notice` and the other one does.
    #[test]
    fn the_panel_roster_is_capped_and_says_how_many_it_hid() {
        let agents: Vec<Agent> = (0..PANEL_MAX_ROWS + 3)
            .map(|i| running(&format!("agent-{i}"), "idle"))
            .collect();
        let tree = super::panel(&Hive::Up { agents }, &AgentsConfig::default(), None, ctx());

        let drawn = button_ids(&tree)
            .iter()
            .filter(|id| id.starts_with(ids::CHAT))
            .count();
        assert_eq!(drawn, PANEL_MAX_ROWS, "the panel roster must cap too");
        assert!(
            texts(&tree).iter().any(|t| t.contains("+3 more")),
            "the hidden tail must be stated: {:?}",
            texts(&tree)
        );
    }

    /// A group's rows sit in a **nested list of their own**, so grouped and
    /// ungrouped rosters draw the same per-row separators.
    ///
    /// GTK wraps each *direct* `ListBox` child in the `GtkListBoxRow` that
    /// `boxed-list`'s hairlines key off, and an `Expander` is one such child no
    /// matter how many agents it reveals — so without the nesting the same
    /// twelve agents separate per-row or per-group depending on whether a
    /// `[display.*].project` is set (#963 review, LOW-1).
    ///
    /// Falsification: hand `group_node` the rows directly as `children` and
    /// this goes red.
    #[test]
    fn a_groups_rows_sit_in_a_nested_list_of_their_own() {
        let mut cfg = AgentsConfig::default();
        for (agent, project) in [("argus", "viberoot"), ("bosun", "nixos")] {
            cfg.display.insert(
                agent.to_owned(),
                crate::config::Display {
                    label: None,
                    icon: None,
                    project: Some(project.to_owned()),
                },
            );
        }
        let tree = super::card(
            &Hive::Up {
                agents: vec![running("argus", "idle"), running("bosun", "idle")],
            },
            &cfg,
            &ExpandedGroups::new(),
            None,
        );

        let mut expanders = 0usize;
        walk(&tree, &mut |node| {
            if let Node::Expander { children, .. } = node {
                expanders += 1;
                match children.as_slice() {
                    [Node::ListBox { classes, dense, .. }] => {
                        assert!(*dense, "a group's list is dense like the outer one");
                        assert!(
                            classes.iter().any(|c| c == "ts-agents-group-list"),
                            "{classes:?}"
                        );
                    }
                    other => panic!("a group's body must be one nested list, got {other:?}"),
                }
            }
        });
        assert_eq!(expanders, 2, "two projects, two expanders");
    }

    /// The hive's dashboard root appears **once** on a panel, not once per
    /// section. Both the selected agent's `links` group and the hive overview
    /// want it, and emitting it from both put two `dashboard` rows on the same
    /// page.
    ///
    /// Falsification: put `detail("dashboard", home)` back into `hive_section`
    /// and the selected case goes red; drop the `selected.is_none()` links
    /// branch in `panel` and the overview case does.
    #[test]
    fn the_dashboard_link_is_emitted_exactly_once_either_way() {
        let urls = crate::hive::wire::HiveUrls {
            domain: Some("hive.local".to_owned()),
            home: Some("https://hive.local/".to_owned()),
        };
        let hive = Hive::Up {
            agents: vec![running("argus", "idle")],
        };
        let cfg = AgentsConfig::default();
        let with_urls = super::PanelContext {
            urls: Some(&urls),
            ..ctx()
        };
        let name = AgentName::parse("argus").expect("legal");

        for selected in [None, Some(&name)] {
            let tree = super::panel(&hive, &cfg, selected, with_urls);
            let seen = texts(&tree).iter().filter(|t| *t == "dashboard").count();
            assert_eq!(
                seen, 1,
                "one dashboard row per panel; selected={selected:?}"
            );
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

    /// Every `Text` in the tree that caps neither its width nor its flow — i.e.
    /// every one whose natural width is its whole string.
    fn unbounded_texts(node: &Node) -> Vec<String> {
        let mut out = Vec::new();
        walk(node, &mut |n| {
            if let Node::Text {
                text,
                max_width_chars,
                ellipsize,
                ..
            } = n
                && max_width_chars.is_none()
                && !*ellipsize
            {
                out.push(text.clone());
            }
        });
        out
    }

    /// Every `Label` body in the tree — the variant that neither wraps nor
    /// ellipsizes, so only short constants may be one.
    fn label_texts(node: &Node) -> Vec<String> {
        let mut out = Vec::new();
        walk(node, &mut |n| {
            if let Node::Label { text, .. } = n {
                out.push(text.clone());
            }
        });
        out
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
