//! The node tree: the sidebar card (spec §6.1) and the drawer panel (§6.4).
//!
//! There is no row-activate event (a list is selection-less), so every
//! interaction is a `Button`, whose `id` is required and is the click target
//! (`crates/hytte-plugin-proto/src/wire.rs:249-255`).
//!
//! # The card is one pill per agent, two lines, and nothing else
//!
//! Annika settled the v1 card on
//! [#963](https://github.com/vibec0re/trollshell/pull/963) (2026-09-11), after
//! @kaesaecracker's second round of screenshots: *"the view still looks very
//! cluttered … Maybe we should try one card / pill per agent … deployed…
//! parent… agent page — too much information to display! Let's keep this slick
//! two lines."* Her mock, verbatim:
//!
//! ```text
//! [ [icon]  [Name] [Model]                    [startstop] [optionsedit] ]
//! [ (oO) Clauding...                                                    ]
//! ```
//!
//! So line 1 is the identity and exactly two controls, line 2 is the state
//! glyph (her `(oO)`) and the harness's own status text. What went, and where
//! it went:
//!
//! | gone from the card | why, and where it lives now |
//! | --- | --- |
//! | the chevron + the in-place details unfold | the clutter she named; the same content is the drawer page, which the **edit** button opens |
//! | the flag chips (`failed` / `needs login` / `paused` / `needs update`) | the first two are already the line-2 glyph; all four stay on the drawer page |
//! | `deployed` / `parent` / `status set` | dropped outright — the three rows she listed by name |
//! | the `agent page` link | the drawer page keeps it, as the "open it in the browser instead" fallback to #950's `WebView` |
//! | the pause/resume button | the drawer page; the mock has two buttons and `[startstop]` is the one she drew |
//! | the status line's **wrapping** | its own hover — @kaesaecracker's second retest (2026-09-11) caught it making rows different heights; see [`status_caption`] |
//!
//! **The row itself is deliberately not clickable.** Her spec is "click on
//! agent opens agent page in trollshell-webview", which is
//! [#950](https://github.com/vibec0re/trollshell/issues/950) and does not exist
//! yet. Wiring the click to the drawer page in the meantime would teach the
//! wrong surface — the exact thing @kaesaecracker objected to ("its very weird
//! the panel opens in the top right after clicking bottom left") — so the name
//! is a plain `Text` until the `WebView` is there to receive the click.
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
//!    glyph in the row. Now the string that got cut carries its own hover —
//!    which is what lets line 2 be a single ellipsized line without losing the
//!    tail (see [`status_caption`]), and why the hover stays on that `Text`
//!    rather than moving back up to the row.
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
    APPROVAL_BADGE_CLASS, APPROVAL_BADGE_ICON, Agent, AgentName, ExpandedGroups, Group, Hive,
    PendingApprovals, Status, UPDATE_BADGE_CLASS, UPDATE_BADGE_ICON, agent_url, group,
    headers_wanted, model_family,
};

/// The card's root node id.
pub const ROOT_ID: &str = "agents-root";
/// The panel's root node id.
pub const PANEL_ID: &str = "agents-panel";
/// The panel's "back to the hive overview" button.
pub const BACK_ID: &str = "agents-back";
/// The card title row's button: open the drawer panel at the hive overview.
///
/// The card's title is where a jump to the **hive-level** page belongs —
/// @kaesaecracker, [#963](https://github.com/vibec0re/trollshell/pull/963):
/// "its very weird the panel opens in the top right after clicking bottom
/// left". A row's own jump is its [`ids::EDIT`] button, which opens that
/// agent's page; this one opens the overview and the full roster.
pub const OVERVIEW_ID: &str = "agents-overview";

/// Button id prefixes. Each is `"<prefix><name>"`; the reducer strips the
/// prefix and re-validates the remainder as an [`AgentName`] rather than
/// trusting the round trip.
pub mod ids {
    /// The row's **edit** button — Annika's `[optionsedit]`, 2026-09-11 on
    /// [#963](https://github.com/vibec0re/trollshell/pull/963).
    ///
    /// Today it opens this plugin's drawer page on that agent, which is a
    /// **placeholder**. Its real destination is the agent's own companion
    /// window on its **settings tab** —
    /// [#950](https://github.com/vibec0re/trollshell/issues/950), settled by
    /// Annika on #947 (2026-09-11 07:43Z): one surface per agent, so the same
    /// window serves the row click (the agent page) and this button (its
    /// settings). **Not**
    /// [#1010](https://github.com/vibec0re/trollshell/issues/1010)'s modal —
    /// that stays the answer for every *other* plugin's page and is not this
    /// crate's concern at all.
    ///
    /// So this arm **will** change when #950 lands: opening a separate GTK
    /// window is not `OpenPage(PluginSelf)`, which names a page inside the
    /// shell. The id is stable, the effect behind it is not.
    ///
    /// It replaces `chat:`, the old name button. That id's documented future
    /// was a chat companion window, which is the same #950 window reached from
    /// the **row click** rather than from a button — see the module doc.
    pub const EDIT: &str = "edit:";
    /// The pause/resume toggle. **Panel only** since the card went to two
    /// lines: Annika's mock has exactly two buttons per row and start/stop is
    /// the one she named. `SetPaused` is a different hive verb from
    /// `Start`/`Stop` and is unchanged on the wire.
    pub const PAUSE: &str = "pause:";
    /// Per-agent start — the card's lifecycle button when the agent is
    /// stopped, and the panel's own.
    pub const START: &str = "start:";
    /// Per-agent stop — the card's lifecycle button when the agent is not
    /// stopped, and the panel's own.
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
    /// The pending-approval badge (#947 P3), carrying the **agent name**.
    ///
    /// Not the approval id, for [`OPEN`]'s reason and one more: the id the row
    /// was rendered with may have been resolved on the dashboard in the seconds
    /// since, and "raise the oldest approval this agent is *now* waiting on" is
    /// both what the operator means by clicking a badge and the only phrasing
    /// that cannot address a resolved request.
    ///
    /// Its prefix is deliberately not a prefix of any other id here: nothing
    /// else starts with `approvals`, and `strip_prefix` is how every arm
    /// routes.
    pub const APPROVALS: &str = "approvals:";
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

/// The ellipsized width of a status in the **panel's** roster row, which is
/// one line there and always has been.
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

/// The ellipsized width of line 1's model chip.
///
/// Every known family word fits (`Sonnet` is the longest at six); the cap is
/// for the **unknown** fallback, whose first token is whatever a provider
/// chose and is not bounded by anything this crate controls.
const MODEL_CHARS: i32 = 10;

/// The **ellipsized** width of the status on line 2 of a card row.
///
/// Narrower than [`CARD_TEXT_CHARS`] because line 2 is a `Row`: the state glyph
/// and, when set, the update badge sit to the left of the text, so the text's
/// share of the 320 px clamp is smaller than a full-width line's.
///
/// It was a *wrap* width until @kaesaecracker's 2026-09-11 retest — see
/// [`status_caption`] for why line 2 stopped wrapping.
const CARD_STATUS_CHARS: i32 = 30;

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

/// The harness's status line, as line 2 of the row: **one** dim line,
/// end-ellipsized, with the whole string on its own hover.
///
/// # Every row is the same height, and that is the whole point
///
/// This used to wrap (and ellipsize only past a `STATUS_WRAP_MAX` budget), on
/// the argument that the status should be readable in full without a hover.
/// @kaesaecracker's 2026-09-11 retest of `c55d0a4f` is what retired that: a
/// status of ordinary length — `argus` and `triage` in her screenshot — wraps
/// to two or three lines at this width, so the pills in one card came out
/// visibly different heights and the roster stopped reading as a list. Annika's
/// v1 mock says **two lines**, not "two lines plus however many the status
/// needs", and a row whose height depends on its content cannot honour that.
///
/// So the wrap is gone and the hover is no longer the last resort: it is where
/// a long status is read, every time. That is the trade — one hover for a
/// uniform roster — and it is the same one [`clipped`] makes everywhere else in
/// this file. `ellipsize: true` is `set_wrap(false)` plus
/// `EllipsizeMode::End` in the host (`hytte-ui`'s `apply_text_flow`), so the
/// cut is at the end and the beginning of the status — the part that says what
/// the agent is doing — is always the part that survives.
///
/// The hover lives on this `Text`, **not** on the enclosing row: #961/#971
/// added `Node::Text.tooltip` precisely to retire a row-level tooltip, which
/// put one legend over every glyph in the row (see the module doc). A row-level
/// hover would show the status over the buttons and the model pill too, which
/// is the regression that change undid.
fn status_caption(agent: &Agent) -> Node {
    clipped(
        agent.status_line(),
        CARD_STATUS_CHARS,
        &["dim-label", "caption", "ts-agent-status"],
    )
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

/// The lifecycle button's id prefix, glyph and hover — what the click will
/// **do**, which is the only reading that stays honest.
///
/// One button, two verbs, because Annika's mock has one `[startstop]` slot.
/// Which verb depends on the state the row is already showing: a stopped agent
/// can only be started, anything else can be stopped. Both are `Scope`d to this
/// one agent (spec §11 rule one).
///
/// # Why this has no optimistic flip, where pause does (#963 review, LOW-4)
///
/// [`Agent::paused`](crate::model::Agent::paused) carries a `pending_paused`
/// that flips the row the instant the button is clicked; these two verbs just
/// send and return, so the button keeps offering the stale verb until the next
/// poll — ≤ [`DEFAULT_POLL_SECONDS`](crate::config::DEFAULT_POLL_SECONDS), 2 s
/// by default. The round moved the control **with** the affordance off the card
/// and the one **without** it on, so the asymmetry is stated rather than left
/// to be rediscovered.
///
/// It is deliberate, and the reason is that the two verbs are not the same kind
/// of operation. `SetPaused` writes a **marker**, which `hive/wire.rs` records
/// as "applies immediately and works on a stopped container too … idempotent
/// both ways" — the hive's answer is knowable at click time, so predicting it
/// is not a guess. `Start`/`Stop` are **container lifecycle**: a start pulls,
/// boots a unit and waits for the harness, which can take far longer than one
/// poll and can end in [`Status::Failed`]. An optimistic `running` would be a
/// claim this plugin cannot back — visibly wrong for seconds, and wrong in the
/// worst direction on the one path that matters (a start that fails would read
/// as running until the poll corrected it).
///
/// The correct fix is not a flip but a **transient state** — `starting…` /
/// `stopping…` as its own [`Status`], which is neither a lie nor stale. That is
/// a model field, a precedence row, a glyph and a golden per state; a feature,
/// not a fix-round line. Until then the stale window is bounded by the poll and
/// harmless in both directions: `Stop` is `graceful: true` (hyperhive's
/// per-agent quiesce) and a second click on either verb is idempotent.
fn lifecycle_affordance(agent: &Agent) -> (&'static str, &'static str, &'static str) {
    if agent.status() == Status::Stopped {
        (
            ids::START,
            "media-playback-start-symbolic",
            "start this agent",
        )
    } else {
        (ids::STOP, "media-playback-stop-symbolic", "stop this agent")
    }
}

/// The model family as line 1's third item — `Opus`, never
/// `opus-5.2-20262981923899321898`.
///
/// `None` when the hive reports no model, or reports one with no word in it:
/// the row then simply has no chip, rather than an empty pill between the name
/// and the buttons. The **full** id is the chip's hover, so shortening loses
/// nothing — see [`crate::model::model_family`] for the rule.
fn model_chip(agent: &Agent) -> Option<Node> {
    let raw = agent.row.active_model.as_deref()?.trim();
    let family = model_family(raw)?;
    Some(clipped_titled(
        family,
        MODEL_CHARS,
        raw,
        &["caption", "dim-label", "ts-agent-model"],
    ))
}

/// The pending-approval badge (#947 P3), or `None` when this agent is waiting
/// on nothing.
///
/// A **button**, unlike the row's other two badges: those report a state the
/// operator can only fix elsewhere, this one is the way back into a prompt that
/// timed out or was dismissed — spec §6.5's "clicking it re-raises". That is
/// also why it carries a count: the badge has to distinguish "one thing is
/// waiting" from "four are", since answering only raises the oldest and the
/// operator needs to know more is queued behind it.
///
/// One approval draws the glyph alone; more draw the glyph plus the number,
/// rather than always drawing a `1` that would read as a version or an index.
fn approval_badge(name: &str, count: usize) -> Option<Node> {
    if count == 0 {
        return None;
    }
    let hover = if count == 1 {
        "1 approval waiting — click to answer it".to_owned()
    } else {
        format!("{count} approvals waiting — click to answer the oldest")
    };
    let id = format!("{}{name}", ids::APPROVALS);
    let glyph = icon_titled(APPROVAL_BADGE_ICON, hover.clone(), &[]);
    Some(if count == 1 {
        button(id, &["flat", "ts-agent-btn", APPROVAL_BADGE_CLASS], glyph)
    } else {
        button(
            id,
            &["flat", "ts-agent-btn", APPROVAL_BADGE_CLASS],
            hrow(
                2,
                &[],
                vec![
                    glyph,
                    clipped_titled(
                        count.to_string(),
                        // Two digits plus the ellipsis budget: a queue past 99
                        // is a hive problem, not a layout problem.
                        3,
                        hover,
                        &["caption", "numeric"],
                    ),
                ],
            ),
        )
    })
}

/// One agent as a **pill**: two lines, nothing else (Annika, 2026-09-11 —
/// see the module doc for her mock and for what each removed thing became).
///
/// Line 1: `[runtime icon] [Name] [Model] … [approvals?] [start|stop] [edit]`.
/// Line 2: the state glyph, the update badge if it is set, and the harness's
/// own status text in full.
///
/// The name is a `Text`, not a `Button`: the row's click belongs to #950's
/// `WebView` and does not exist yet, and a button that opened the drawer instead
/// would train the wrong surface.
fn agent_row(agent: &Agent, cfg: &AgentsConfig, approvals: usize) -> Node {
    let name = agent.name.as_str();
    let status = agent.status();
    let (lifecycle_id, lifecycle_glyph, lifecycle_hover) = lifecycle_affordance(agent);

    let mut head = vec![
        icon(cfg.icon_for(name), &["ts-agent-runtime"]),
        // Clipped, not a bare `Label`: a `Label`'s natural width forces its
        // container wider (the #281 sidebar blow-out), and an agent name may be
        // up to 63 bytes.
        clipped_titled(
            cfg.label_for(name),
            NAME_CHARS,
            name_hover(name, cfg),
            &["heading", "ts-agent-name"],
        ),
    ];
    head.extend(model_chip(agent));
    head.push(Node::Spacer);
    // First of the right-hand group: an approval is the one thing on this row
    // that is waiting on *the operator*, so it sits where the eye lands before
    // the lifecycle controls rather than after them.
    head.extend(approval_badge(name, approvals));
    head.push(icon_button(
        format!("{lifecycle_id}{name}"),
        lifecycle_glyph,
        lifecycle_hover,
        &["flat", "ts-agent-btn"],
    ));
    head.push(icon_button(
        format!("{}{name}", ids::EDIT),
        "document-edit-symbolic",
        "edit this agent",
        &["flat", "ts-agent-btn"],
    ));

    let mut tail = vec![icon_titled(
        status.icon(),
        status.text(),
        &["ts-agent-state", status.class()],
    )];
    if agent.needs_update() {
        // Spec §6.2's badge row says "(tooltip only)" for its text. It stays on
        // the card when the other three flag chips did not, because it is the
        // one flag line 2's glyph does **not** already encode — `Status` has no
        // `NeedsUpdate` — so dropping it would lose a signal rather than
        // de-duplicate one.
        tail.push(icon_titled(
            UPDATE_BADGE_ICON,
            "config commit pending — a rebuild would change this agent's locked rev",
            &["ts-agent-badge", UPDATE_BADGE_CLASS],
        ));
    }
    tail.push(status_caption(agent));

    vstack(
        2,
        &["ts-agent-row"],
        vec![
            hrow(6, &["ts-agent-head"], head),
            hrow(6, &["ts-agent-statusline"], tail),
        ],
    )
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
    approvals: &PendingApprovals,
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
        Hive::Up { agents } => roster(agents, cfg, expanded, approvals),
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
///
/// **The grouping survived the pill round** (Annika, 2026-09-11), because the
/// thing she called cluttered was what each row carried, not that the rows sit
/// under a project header — and grouping by multi-repo project is her own
/// earlier ask (spec §6.3). A header is one collapsible line above a run of
/// pills and adds nothing per row; with one project it is suppressed entirely
/// ([`headers_wanted`]), which is the single-hive case.
fn roster(
    agents: &[Agent],
    cfg: &AgentsConfig,
    expanded: &ExpandedGroups,
    approvals: &PendingApprovals,
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
                approvals.count_for(agent.name.as_str()),
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
            // The roster is how the drawer picks an agent, so this one name
            // stays a button — and it opens the same page the card's `edit`
            // does, which is why it carries that id rather than a second one.
            button(
                format!("{}{name}", ids::EDIT),
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

/// The selected agent's page — **the edit page, for now** (Annika,
/// 2026-09-11: "then opensedit can open edit dialog").
///
/// The card's `edit` button opens it, as a **placeholder** for the agent's own
/// companion window on its settings tab —
/// [#950](https://github.com/vibec0re/trollshell/issues/950), Annika's call on
/// #947 (2026-09-11 07:43Z). When that window exists this tree is what its
/// settings tab is built from, or is replaced by it; either way the button
/// stops pointing here. It is **not** #1010's modal, which stays the answer
/// for every other plugin's page.
///
/// Trimmed to the card's own two lines plus what the card gave up:
///
/// - the same pill, wider: identity + model on line 1 with the lifecycle and
///   pause controls, the state glyph and the status text on line 2;
/// - the hive's own name when a `[display.<name>].label` renamed the row —
///   the one place the string every request frame and every log line uses is
///   still readable;
/// - the flag chips, which are the card's dropped detail and have room here;
/// - the `agent page` link, which is #1045's button and, once #950's `WebView`
///   takes the row click, the "open it in the browser instead" fallback.
///
/// `deployed` / `parent` / `model` / `status set` are gone outright: three of
/// the four are the rows Annika named as too much information, and `model` is
/// now line 1's chip with the full id on its hover.
fn agent_page(agent: &Agent, cfg: &AgentsConfig, _ctx: PanelContext<'_>) -> Vec<Node> {
    let name = agent.name.as_str();
    let status = agent.status();
    let (lifecycle_id, lifecycle_glyph, lifecycle_hover) = lifecycle_affordance(agent);
    let (pause_glyph, pause_hover) = pause_affordance(agent);
    let mut out = Vec::new();

    let mut head = vec![
        icon(cfg.icon_for(name), &["ts-agent-runtime"]),
        // Bounded, and a `Text` rather than a `Label`: a `Label` neither wraps
        // nor ellipsizes, so a 63-byte agent name (legal on the wire) reports
        // its whole self as the header's natural width and widens the drawer —
        // see [`PANEL_TEXT_CHARS`]. The card guarded this from the start
        // (#281); the panel header did not until #963's review.
        clipped_titled(
            cfg.label_for(name),
            PANEL_NAME_CHARS,
            name_hover(name, cfg),
            &["title-4"],
        ),
    ];
    head.extend(model_chip(agent));
    head.push(Node::Spacer);
    // Spec §11 rule one: every frame here is scoped to this one agent. The
    // lifecycle button is the card's, and pause/resume is the control the
    // two-line card gave up — `SetPaused` is a different hive verb from
    // `Start`/`Stop`, so this is the only surface that still reaches it.
    head.push(icon_button(
        format!("{lifecycle_id}{name}"),
        lifecycle_glyph,
        lifecycle_hover,
        &["flat", "circular", "ts-agent-btn"],
    ));
    head.push(icon_button(
        format!("{}{name}", ids::PAUSE),
        pause_glyph,
        pause_hover,
        &["flat", "circular", "ts-agent-btn"],
    ));
    out.push(hrow(8, &["ts-agents-panel-head"], head));

    if cfg.label_for(name) != name {
        out.push(wrapped(
            name,
            PANEL_TEXT_CHARS,
            &["dim-label", "caption", "ts-mono"],
        ));
    }
    out.push(hrow(
        6,
        &["ts-agent-statusline"],
        vec![
            icon_titled(
                status.icon(),
                status.text(),
                &["ts-agent-state", status.class()],
            ),
            wrapped(agent.status_line(), PANEL_TEXT_CHARS, &["dim-label"]),
        ],
    ));

    let chips = flag_chips(agent);
    if !chips.is_empty() {
        out.push(hrow(4, &["ts-agent-chips"], chips));
    }

    // The one link that survived the trim. Annika dropped "agent page" from
    // the *card*; this is the surface it moved to, and with the row click
    // going to #950's `WebView` it is the "open it in the browser instead"
    // fallback rather than the only way to reach the page. The hive-level
    // `dashboard` link is not here — it belongs to the overview, which is
    // where `panel` now emits it, once.
    if let Some(url) = agent_url(agent) {
        out.push(section(
            "links",
            list(
                false,
                &["ts-agents-panel-list"],
                vec![link_detail(
                    format!("{}{}", ids::OPEN, agent.name.as_str()),
                    "agent page",
                    url,
                )],
            ),
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

    // The hive's dashboard root, and the **only** place it is emitted since the
    // agent page was trimmed to the agent's own link (Annika, 2026-09-11). It
    // is hive-level, so the hive overview is where it belongs; it was already
    // gated on `shown.is_none()` to stop the two pages rendering it twice, and
    // that gate is now the whole rule rather than half of it.
    //
    // It is a `link_detail`, not a `detail`: this row was left a plain,
    // unclickable label when #1045 turned the agent page's copy into a button,
    // which is the same dead-URL complaint one surface over.
    if shown.is_none()
        && let Some(home) = hive_home(ctx)
    {
        children.push(section(
            "links",
            list(
                false,
                &["ts-agents-panel-list"],
                vec![link_detail(ids::OPEN_DASHBOARD, "dashboard", home)],
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
    use super::{MAX_ROWS, PANEL_MAX_ROWS, PANEL_VIEWPORT_PX, age, ids, parse_set_at};
    use crate::config::AgentsConfig;
    use crate::hive::wire::AgentStatusRow;
    use crate::hive::wire::{Approval, ApprovalKind, ApprovalStatus};
    use crate::model::{Agent, AgentName, ExpandedGroups, Hive, PendingApprovals};
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

    /// An empty approval queue — what every pre-#947-P3 assertion here
    /// describes, named so a badge appearing in one of them is a visible diff.
    fn no_approvals() -> PendingApprovals {
        PendingApprovals::default()
    }

    fn card_of(agents: Vec<Agent>) -> Node {
        super::card(
            &Hive::Up { agents },
            &AgentsConfig::default(),
            &ExpandedGroups::new(),
            &no_approvals(),
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

    /// **Line 2 is exactly one line, at every status length** — the property
    /// that makes every pill in a card the same height.
    ///
    /// @kaesaecracker's 2026-09-11 retest of `c55d0a4f` is why this is asserted
    /// over a *range* rather than at one length: the old rule wrapped up to
    /// `STATUS_WRAP_MAX` (88 chars) and ellipsized past it, so the rows that
    /// came out wrong were the **ordinary** ones — long enough to wrap at 30
    /// chars, short enough never to reach the ellipsize arm. A single-length
    /// fixture is exactly what let that ship: the short case passed, the
    /// absurd case passed, and the middle of the range was untested.
    ///
    /// So: short, middling (her `argus`/`triage` shape) and absurd, all three
    /// `ellipsize: true` with the same cap and the whole string on hover.
    ///
    /// Falsification: put `status_caption`'s wrapping arm back for any subrange
    /// and the middling case reds; drop `clipped`'s `tooltip: Some(body)` and
    /// every hover assertion reds.
    #[test]
    fn line_two_is_one_ellipsized_line_at_every_status_length() {
        // No trailing whitespace anywhere: `Agent::status_line` trims, so a
        // padded fixture would not be the string the tree carries.
        let middling = "idle — #4077 approved, watching for review assignments".to_owned();
        let absurd = format!(
            "idle — {}",
            ["watching for review assignments"; 6].join(", ")
        );
        assert!(
            (31..=88).contains(&middling.chars().count()),
            "the middling case must sit in the old wrap band: {}",
            middling.chars().count()
        );
        assert!(absurd.chars().count() > 88, "and the absurd one past it");

        for line in [&"idle".to_owned(), &middling, &absurd] {
            let tree = card_of(vec![running("argus", line)]);
            let status = find_text(&tree, line).expect("the status renders");
            assert!(
                status.ellipsize,
                "line 2 must be one line at every length, or rows get uneven \
                 heights ({} chars)",
                line.chars().count()
            );
            assert_eq!(
                status.max_width_chars,
                Some(super::CARD_STATUS_CHARS),
                "…and bound, so a long status cannot widen the card"
            );
            assert_eq!(
                status.tooltip.as_deref(),
                Some(line.as_str()),
                "the cut string carries its own untruncated hover (#971)"
            );
            assert!(
                status.classes.iter().any(|c| c == "ts-agent-status"),
                "line 2 is the status caption: {:?}",
                status.classes
            );

            // …and it really is a second line: the row is a vertical stack of
            // exactly the head row and that caption.
            let row = find_class(&tree, "ts-agent-row").expect("the row renders");
            let Node::Box { dir, children, .. } = &row else {
                panic!("an agent row is a vertical stack, got {row:?}");
            };
            assert_eq!(*dir, hytte_plugin::proto::Dir::Vertical);
            assert_eq!(children.len(), 2, "two lines, no unfold: {children:?}");
        }
    }

    /// The hover stays on the status **`Text`**, never on the enclosing row.
    ///
    /// #961/#971 added `Node::Text.tooltip` to retire a row-level tooltip,
    /// which put one legend over every glyph in the row. Now that line 2 is
    /// always ellipsized, the hover is load-bearing rather than a last resort —
    /// which makes it exactly the moment somebody would be tempted to move it
    /// up to the row "so the whole pill shows it". This says no.
    ///
    /// Falsification: set the tooltip on `agent_row`'s `vstack` (or on line
    /// 2's `hrow`) and the row/line assertions red.
    #[test]
    fn the_status_hover_is_on_the_text_not_on_the_row() {
        let long = format!(
            "idle — {}",
            ["watching for review assignments"; 6].join(", ")
        );
        let tree = card_of(vec![running("argus", &long)]);

        let row = find_class(&tree, "ts-agent-row").expect("the row renders");
        let Node::Box { tooltip, .. } = &row else {
            panic!("an agent row is a vertical stack, got {row:?}")
        };
        assert_eq!(*tooltip, None, "the row must not carry the status legend");

        let line2 = find_class(&tree, "ts-agent-statusline").expect("line 2 renders");
        let Node::Row { tooltip, .. } = &line2 else {
            panic!("line 2 is a Row, got {line2:?}")
        };
        assert_eq!(*tooltip, None, "…nor line 2's own container");

        assert_eq!(
            find_text(&tree, &long).expect("the status renders").tooltip,
            Some(long.clone()),
            "the Text that got cut is what hovers"
        );
    }

    /// **The card row is a pill: two lines, and exactly two buttons.**
    ///
    /// Annika's v1 mock, asserted as a shape rather than as prose
    /// (2026-09-11, #963):
    ///
    /// ```text
    /// [ [icon]  [Name] [Model]              [startstop] [optionsedit] ]
    /// [ (oO) Clauding...                                              ]
    /// ```
    ///
    /// Falsification: push a third child onto `agent_row`'s `vstack` and the
    /// line count reds; put the chevron, the pause button or any of the removed
    /// detail rows back and the button-id assertion reds; move the state glyph
    /// back to line 1 and the last assertion does.
    #[test]
    fn a_card_row_is_two_lines_with_exactly_the_mocks_two_buttons() {
        let mut a = running("argus", "Clauding…");
        a.row.active_model = Some("claude-opus-4-6".to_owned());
        a.row.url = Some("https://hive.local/agent/argus/".to_owned());
        let tree = card_of(vec![a]);

        let row = find_class(&tree, "ts-agent-row").expect("a row renders");
        let Node::Box { children, .. } = &row else {
            panic!("the row is a vertical Box, got {row:?}")
        };
        assert_eq!(children.len(), 2, "two lines, nothing else: {children:?}");

        assert_eq!(
            button_ids(&row),
            vec!["stop:argus".to_owned(), "edit:argus".to_owned()],
            "line 1 carries the mock's two controls, in her order"
        );

        // Line 1: identity + model. Line 2: the state glyph and the status.
        let head = find_class(&row, "ts-agent-head").expect("line 1");
        assert!(find_text(&head, "argus").is_some(), "the name is on line 1");
        assert!(
            find_text(&head, "Opus").is_some(),
            "the model family is on line 1: {:?}",
            texts(&head)
        );
        let status = find_class(&row, "ts-agent-statusline").expect("line 2");
        assert!(
            find_text(&status, "Clauding…").is_some(),
            "the harness status is on line 2: {:?}",
            texts(&status)
        );
        assert!(
            icon_hover(&status, super::Status::Running.icon()).is_some(),
            "her `(oO)` — the state glyph moved to line 2"
        );
    }

    /// The card carries **none** of the detail Annika called too much
    /// information: no chevron, no unfolded block, no flag chips, no
    /// `deployed` / `parent` / `agent page` rows.
    ///
    /// Stated as an absence because that is what the round did; the positive
    /// half — that the same content is on the drawer page — is
    /// `the_agent_page_keeps_what_the_card_gave_up` below.
    ///
    /// Falsification: re-render any one of them on the card and exactly one
    /// assertion here reds.
    #[test]
    fn the_card_carries_none_of_the_detail_the_pill_round_removed() {
        let mut a = agent(
            "argus",
            AgentStatusRow {
                name: "argus".to_owned(),
                running: true,
                needs_login: true,
                parent: Some("bosun".to_owned()),
                deployed_sha: Some("1bfaf24abcde".to_owned()),
                url: Some("https://hive.local/agent/argus/".to_owned()),
                ..AgentStatusRow::default()
            },
        );
        a.row.active_model = Some("claude-opus-4-6".to_owned());
        let tree = card_of(vec![a]);

        assert_eq!(count_class(&tree, "ts-agent-details"), 0, "no unfold");
        assert_eq!(count_class(&tree, "ts-agent-chip"), 0, "no flag chips");
        assert!(
            icon_hover(&tree, "pan-end-symbolic").is_none()
                && icon_hover(&tree, "pan-down-symbolic").is_none(),
            "no chevron"
        );
        for gone in ["deployed", "parent", "agent page", "1bfaf24abcde"] {
            assert!(
                !texts(&tree).iter().any(|t| t == gone),
                "{gone} must not be on the card any more: {:?}",
                texts(&tree)
            );
        }
    }

    /// The drawer page keeps what the card gave up: the flag chips (only the
    /// ones that are **on**), the pause control, and the `agent page` link as
    /// a real button (#1045).
    ///
    /// "No dead button" is the second half: `running()` builds a row whose
    /// `url` is `None`, and the page then carries neither the link nor the key
    /// that labels one — there is no state in which this draws something
    /// clickable that opens nothing.
    ///
    /// Falsification: drop the `.filter(|(on, ..)| *on)` in `flag_chips` and
    /// the clean-agent assertion reds (four chips instead of zero); put
    /// `detail` back at `agent_page`'s `agent_url` call site and the button
    /// assertion reds; drop that site's `if let Some(url)` guard and the last
    /// two do.
    #[test]
    fn the_agent_page_keeps_what_the_card_gave_up() {
        let name = AgentName::parse("argus").expect("legal");
        let cfg = AgentsConfig::default();

        let clean = super::panel(
            &Hive::Up {
                agents: vec![running("argus", "idle")],
            },
            &cfg,
            Some(&name),
            ctx(),
        );
        assert_eq!(
            chip_texts(&clean),
            Vec::<String>::new(),
            "a healthy agent says nothing about flags that are off"
        );
        assert!(
            !button_ids(&clean)
                .iter()
                .any(|id| id.starts_with(super::ids::OPEN)),
            "no URL means no button: {:?}",
            button_ids(&clean)
        );
        assert!(
            !texts(&clean).iter().any(|t| t == "agent page"),
            "…and no orphaned key either"
        );

        let mut flagged = agent(
            "argus",
            AgentStatusRow {
                name: "argus".to_owned(),
                running: true,
                needs_login: true,
                needs_update: true,
                ..AgentStatusRow::default()
            },
        );
        flagged.row.url = Some("https://hive.local/agent/argus/".to_owned());
        let page = super::panel(
            &Hive::Up {
                agents: vec![flagged],
            },
            &cfg,
            Some(&name),
            ctx(),
        );
        assert_eq!(chip_texts(&page), vec!["needs login", "needs update"]);
        let ids = button_ids(&page);
        assert!(
            ids.iter().any(|id| id == "open:argus"),
            "the URL has to be followable: {ids:?}"
        );
        assert!(
            ids.iter().any(|id| id == "pause:argus"),
            "pause/resume moved here when the card went to two lines: {ids:?}"
        );
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

        let tree = card_of(agents);
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
            let want = format!("edit:agent-{i}");
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
            super::card(&hive, &cfg, &ExpandedGroups::new(), &no_approvals()),
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
            &no_approvals(),
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
            ids.contains(&"edit:bosun".to_owned()),
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
        // `argus` is running, so the lifecycle button offers `stop`; `pause` is
        // the control the two-line card handed to this page.
        assert!(ids.contains(&"stop:argus".to_owned()), "{ids:?}");
        assert!(ids.contains(&"pause:argus".to_owned()), "{ids:?}");
        assert!(ids.contains(&back));
        assert!(
            !ids.contains(&"edit:argus".to_owned()),
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
    /// **`ellipsize` is not a bound.** [`unbounded_texts`] used to exclude
    /// ellipsizing nodes, which made this rule blind to every identity node the
    /// pill round added — see that fn's own doc for the measurement. The
    /// fixture below therefore carries an `active_model`, so the **model chip**
    /// is in the tree this walks; without it the chip is absent and the
    /// predicate has nothing to judge. (The reviewer verified the converse too:
    /// adding the model to the fixture without fixing the predicate does *not*
    /// catch an unbounded chip — the clause was the hole, not the data.)
    ///
    /// Falsification, all reproduced red: drop the cap from either `wrapped`
    /// call in `agent_page`; swap the panel header back to
    /// `label(…, &["title-4"])`; make the group header a plain `label`; or drop
    /// `max_width_chars` from `clipped` / `clipped_titled` / `model_chip`,
    /// which is the class that used to ship green.
    #[test]
    fn no_free_text_reports_an_unbounded_width_on_either_surface() {
        // 63 bytes — `AgentName::MAX_LEN`, and legal on the wire.
        let long_name = "a".repeat(63);
        let long_status = ["watching for review assignments"; 7].join(", ");
        let long_url = format!("https://hive.local/agent/{long_name}/");
        let long_project = "p".repeat(80);
        // A model id whose *family* is unknown, so `model_family` falls through
        // to the first-token branch and the chip's text is bounded by nothing
        // but `MODEL_CHARS` — the case that cap actually exists for.
        let long_model = "q".repeat(90);

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
                        // Without this the model chip is not in the tree at
                        // all, so the rule has nothing to judge about it.
                        active_model: Some(long_model.clone()),
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
                super::card(&hive, &cfg, &ExpandedGroups::new(), &no_approvals()),
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

        // **On the card, the long status is one ellipsized line** — bounded is
        // not enough there. A wrapping `Text` is bounded and still makes that
        // row taller than its neighbours, which is what @kaesaecracker's
        // 2026-09-11 screenshot showed; `line_two_is_one_ellipsized_line_at_every_status_length`
        // owns the rule, and this asserts it survives at the pathological
        // length this test already builds.
        let card = &surfaces[0].1;
        let status = find_text(card, &long_status).expect("the status renders on the card");
        assert!(
            status.ellipsize,
            "the card's line 2 must never wrap, at any length"
        );
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
            .filter(|id| id.starts_with(ids::EDIT))
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
            &no_approvals(),
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

    /// The hive's dashboard root is **hive-level**, so it belongs to the hive
    /// overview and to nothing else: once there, never on an agent's page.
    ///
    /// It used to be emitted from both, which put two `dashboard` rows on one
    /// panel; the fix was a gate, and the pill round turned the gate into the
    /// whole rule by trimming the agent page's links to the agent's own.
    ///
    /// It is also a **button**, not a label: it was left a plain string when
    /// #1045 turned the agent page's copy into one, which is the same dead-URL
    /// complaint one surface over.
    ///
    /// Falsification: emit it from `agent_page` as well and the second case
    /// reds; drop the `shown.is_none()` links branch in `panel` and the first
    /// does; put `detail` back in place of `link_detail` and the button
    /// assertion does.
    #[test]
    fn the_dashboard_link_is_a_button_on_the_overview_and_nowhere_else() {
        let urls = crate::hive::wire::HiveUrls {
            domain: Some("hive.local".to_owned()),
            home: Some("https://hive.local/".to_owned()),
            forge: None,
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

        let overview = super::panel(&hive, &cfg, None, with_urls);
        assert_eq!(
            texts(&overview)
                .iter()
                .filter(|t| *t == "dashboard")
                .count(),
            1,
            "the overview carries it once"
        );
        assert!(
            button_ids(&overview).contains(&super::ids::OPEN_DASHBOARD.to_owned()),
            "…and it opens: {:?}",
            button_ids(&overview)
        );

        let page = super::panel(&hive, &cfg, Some(&name), with_urls);
        assert_eq!(
            texts(&page).iter().filter(|t| *t == "dashboard").count(),
            0,
            "an agent's page carries the agent's link, not the hive's"
        );
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
            ids::EDIT,
            ids::PAUSE,
            ids::START,
            ids::STOP,
            ids::GROUP,
            ids::OPEN,
        ] {
            assert!(prefix.ends_with(':'), "{prefix}");
        }
        // The whole-id buttons are not prefixes and must not look like one, or
        // `strip_prefix` would match them against an agent name. The dashboard
        // link is the one that would actually collide: it is the only whole id
        // that starts with a prefix's own word (`open`), and dropping its dash
        // for a colon would make `strip_prefix(ids::OPEN)` hand the reducer
        // `"dashboard"` as an agent name.
        for id in [super::BACK_ID, super::OVERVIEW_ID, ids::OPEN_DASHBOARD] {
            assert!(!id.contains(':'), "{id}");
            assert!(
                id.strip_prefix(ids::OPEN).is_none(),
                "{id} must not parse as an OPEN target"
            );
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

    // ── #947 P3: the approval badge ──────────────────────────────────────────

    fn approval(id: i64, agent: &str) -> Approval {
        Approval {
            id,
            agent: agent.to_owned(),
            kind: ApprovalKind::MergeConfigPr,
            requested_at: "2026-09-12T09:00:00Z".to_owned(),
            status: ApprovalStatus::Pending,
            description: None,
        }
    }

    fn card_with(agents: Vec<Agent>, queue: Vec<Approval>) -> Node {
        super::card(
            &Hive::Up { agents },
            &AgentsConfig::default(),
            &ExpandedGroups::new(),
            &PendingApprovals::new(queue),
        )
    }

    /// An agent waiting on nothing wears no badge — the property that keeps the
    /// quiet card exactly what P1 shipped.
    ///
    /// Falsification: render the badge unconditionally (drop
    /// `approval_badge`'s `count == 0` guard) and the id appears here.
    #[test]
    fn an_agent_with_no_approvals_has_no_badge() {
        let tree = card_with(vec![running("argus", "idle")], Vec::new());
        assert!(
            !button_ids(&tree)
                .iter()
                .any(|id| id.starts_with(ids::APPROVALS)),
            "{:?}",
            button_ids(&tree)
        );
    }

    /// The badge names its **agent**, sits before the lifecycle button, and
    /// says how many are waiting — one in the hover, more in the hover *and* a
    /// visible count.
    #[test]
    fn the_badge_counts_and_precedes_the_lifecycle_button() {
        let one = card_with(vec![running("argus", "idle")], vec![approval(1, "argus")]);
        let ids = button_ids(&one);
        let badge = ids
            .iter()
            .position(|id| id == "approvals:argus")
            .expect("a badge");
        let stop = ids
            .iter()
            .position(|id| id == "stop:argus")
            .expect("the lifecycle button");
        assert!(badge < stop, "{ids:?}");
        // One approval draws no number — just the glyph and its hover.
        assert!(find_text(&one, "1").is_none());

        let many = card_with(
            vec![running("argus", "idle")],
            vec![
                approval(1, "argus"),
                approval(2, "argus"),
                approval(3, "argus"),
            ],
        );
        let count = find_text(&many, "3").expect("the count renders");
        assert_eq!(
            count.tooltip.as_deref(),
            Some("3 approvals waiting — click to answer the oldest")
        );
    }

    /// A badge counts only its **own** agent's approvals — the bug a
    /// `queue.len()` would have shipped on a hive where one agent is noisy.
    ///
    /// Falsification: make `count_for` ignore the name and the second
    /// assertion goes red.
    #[test]
    fn a_badge_counts_only_its_own_agents_approvals() {
        let tree = card_with(
            vec![running("argus", "idle"), running("bosun", "idle")],
            vec![
                approval(1, "argus"),
                approval(2, "argus"),
                approval(3, "bosun"),
            ],
        );
        let ids = button_ids(&tree);
        assert!(ids.contains(&"approvals:argus".to_owned()), "{ids:?}");
        assert!(ids.contains(&"approvals:bosun".to_owned()), "{ids:?}");
        // argus wears a "2"; bosun wears no number at all (its single approval
        // draws the glyph alone), so a stray "3" would mean the count leaked.
        assert!(find_text(&tree, "2").is_some());
        assert!(find_text(&tree, "3").is_none());
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

    /// Every `Text` in the tree with **no `max_width_chars`** — i.e. every one
    /// whose natural width is its whole string.
    ///
    /// # `ellipsize` is not a bound, and excluding it blinded this rule (#963 review, MED-1)
    ///
    /// This used to require `&& !*ellipsize`, on the reading that an ellipsizing
    /// label cuts itself. It does not: `ellipsize` decides what the widget draws
    /// once it has been given a width, and `max_width_chars` is what *asks* for
    /// one. [`clipped`]'s own doc says so — "without a natural-width bound the
    /// label asks for its full text and the spacer has nothing left to give, so
    /// the row grows instead of the text shrinking" — so an ellipsizing `Text`
    /// with no cap is **precisely** the failure this rule exists for, and the
    /// clause excluded it by construction.
    ///
    /// That mattered the moment the pill round landed, because everything it
    /// added or moved goes through [`clipped`] / [`clipped_titled`], which are
    /// `ellipsize: true`: the agent **name**, the **model chip** and the
    /// `agent page` **URL**. The reviewer got all 52 lib tests green with all
    /// three rendering `max_width_chars: None` — the #281 blow-out shape on a
    /// 63-byte name, detected only by a **regenerable snapshot**, in a crate
    /// that ships an `#[ignore] regenerate` test whose job is to rewrite exactly
    /// those files. A golden regen would have blessed it.
    fn unbounded_texts(node: &Node) -> Vec<String> {
        let mut out = Vec::new();
        walk(node, &mut |n| {
            if let Node::Text {
                text,
                max_width_chars: None,
                ..
            } = n
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
