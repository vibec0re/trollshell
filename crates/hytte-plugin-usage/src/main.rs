//! `hytte-plugin-usage` — the Claude usage-limits monitor 🙈 (issue #320), an
//! out-of-process trollshell sidebar card on the [`hytte_plugin`] SDK.
//!
//! It renders how much of a spend budget has been **burned within a window** as
//! an accent-tinted gauge: a slow, visibility-gated `ureq` poll of the
//! exponentials Grafana **public dashboard** feeds a [`Node::Progress`] bar plus
//! the figures. Pure TEA — the model below, the visibility-gated worker in
//! [`fetch`], and a `card`/`panel` pair.
//!
//! # Honest metric: spend, not headroom
//!
//! Claude Code's OTEL surface exports **spend** (`claude_code.cost.usage` /
//! `token.usage`), not the rolling 5-hour/weekly rate-limit percentages `/usage`
//! shows — and they aren't derivable from it. So this card is honestly
//! "burned within a window ÷ a budget you set", never "headroom left".
//!
//! # The URL is configuration, not a build input (the #320 unblock)
//!
//! The plugin ships with no dashboard. Until [`config`] resolves a URL — from
//! `TROLLSHELL_USAGE_DASHBOARD_URL` / `TROLLSHELL_USAGE_BUDGET` /
//! `TROLLSHELL_USAGE_PANEL` / `TROLLSHELL_USAGE_WINDOW`, or
//! `~/.config/trollshell/usage.toml` — it renders a calm empty-state card and
//! makes **no** network calls (the pet's keyless short-circuit, #438/#472). Drop
//! the dashboard URL into the env or config and it goes live; nothing else to
//! rebuild.
//!
//! # Visibility gating
//!
//! Mounts [`Mount::SidebarTop`] and subscribes [`StateKey::SlotVisible`] (#288):
//! the [`fetch::poll_task`] parks while the sidebar is closed and refreshes on
//! open, then re-polls every [`fetch::POLL_INTERVAL`] (60 s) — the same reference
//! gate the departures board proved. The card is a flat button opening the
//! plugin's own detail panel (#349).

mod config;
mod fetch;

use config::ConfigState;
use hytte_plugin::proto::{
    Capability, Dir, Effect, EventKind, Manifest, Mount, Node, Page, StateKey,
};
use hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream;
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};
use tokio::sync::mpsc;

/// Stable plugin id — the host's mount-region ownership key and audit-log
/// subject.
const PLUGIN_ID: &str = "usage";
/// Placement within [`Mount::SidebarTop`]: a low `order` renders earlier
/// (higher). Sits near the top of the plugin region, above the pet.
const ORDER: i32 = -5;

// Node ids: stable so the host reconciler mutates props in place across renders.
/// The whole compact card is a flat button — the "open my panel" affordance.
const CARD_BTN: &str = "usage-card";
const CARD_FIGURE: &str = "usage-figure";
const CARD_BAR: &str = "usage-bar";
const PANEL_ROOT: &str = "usage-panel";
const PANEL_BAR: &str = "usage-panel-bar";
const PANEL_PERCENT: &str = "usage-panel-percent";
const PANEL_SPENT: &str = "usage-spent";
const PANEL_BUDGET: &str = "usage-budget";
const PANEL_WINDOW: &str = "usage-window";
const PANEL_UPDATED: &str = "usage-updated";

/// Card/panel title.
const HEADING: &str = "Claude usage";
/// Shown while the first fetch is in flight.
const LOADING_TEXT: &str = "Loading usage…";
/// Empty-state title when no dashboard URL is configured.
const UNCONFIGURED_TITLE: &str = "No dashboard configured";
/// Empty-state hint — the actionable line the calm card shows.
const UNCONFIGURED_HINT: &str = "Set TROLLSHELL_USAGE_DASHBOARD_URL (or dashboard_url in ~/.config/trollshell/usage.toml) \
     to show your Claude spend.";

/// The gauge tints from accent to warning once spend crosses this share of the
/// budget (80%), and to error at/over budget.
const WARN_FRACTION: f64 = 0.8;

// ── Messages / commands ──────────────────────────────────────────────────────

/// A message from the [`fetch`] worker back into the reducer (an [`Input::App`]).
#[derive(Debug)]
pub(crate) enum UsageMsg {
    /// A successful poll: the spend within the window, plus the local `HH:MM`
    /// it was read.
    Reading { spend: f64, updated: String },
    /// A poll failed. Any last-good reading is kept; a first failure surfaces.
    FetchError(String),
}

/// A command from the reducer to the [`fetch::poll_task`] over the per-session
/// lane (#280) — the outbound bridge for the visibility gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageCmd {
    /// The mount surface became visible (`true`) / hidden (`false`).
    SetVisible(bool),
}

// ── State ────────────────────────────────────────────────────────────────────

/// The data half of a configured card.
#[derive(Debug, PartialEq)]
enum DataState {
    /// Configured; awaiting the first successful poll.
    Loading,
    /// A good reading is on hand.
    Ready { spend: f64, updated: String },
    /// Configured but no reading yet and the latest poll failed.
    Error(String),
}

/// The plugin's whole mode. Rebuilt on every (re)connect from [`config::load`];
/// the host stores nothing.
#[derive(Debug, PartialEq)]
enum Mode {
    /// No dashboard URL — a calm empty-state card, no network.
    Unconfigured,
    /// A live dashboard. `budget` (the gauge denominator) is optional — without
    /// it the card shows the raw spend figure and no gauge.
    Configured {
        budget: Option<f64>,
        window_label: String,
        state: DataState,
    },
}

/// The plugin model.
struct Usage {
    mode: Mode,
    /// The command lane to the worker (#280): a visibility push forwards here.
    cmd_tx: CmdSender<UsageCmd>,
}

impl Usage {
    /// Fold one worker message into the model (only meaningful once configured).
    fn on_msg(&mut self, msg: UsageMsg) {
        let Mode::Configured { state, .. } = &mut self.mode else {
            return;
        };
        match msg {
            UsageMsg::Reading { spend, updated } => *state = DataState::Ready { spend, updated },
            // Keep a good prior reading rather than flashing an error on a blip.
            UsageMsg::FetchError(e) => {
                if !matches!(state, DataState::Ready { .. }) {
                    *state = DataState::Error(e);
                }
            }
        }
    }
}

// ── The gauge: the window math ───────────────────────────────────────────────

/// The "burned ÷ budget" gauge. `fraction` is clamped to `0.0..=1.0` for the
/// progress bar and the tint; `percent` is the raw ratio (which may exceed 100
/// when over budget) for the honest headline figure.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Gauge {
    fraction: f64,
    percent: f64,
}

impl Gauge {
    /// The gauge for a spend against a budget, or `None` when there's no usable
    /// (positive, finite) budget to divide by.
    fn new(spend: f64, budget: f64) -> Option<Self> {
        if budget > 0.0 && budget.is_finite() && spend.is_finite() {
            let ratio = spend / budget;
            Some(Self {
                fraction: ratio.clamp(0.0, 1.0),
                percent: ratio * 100.0,
            })
        } else {
            None
        }
    }

    /// The libadwaita tint by how much of the budget is burned: accent below the
    /// warn line, warning past it, error at/over budget.
    fn tint(self) -> &'static str {
        if self.fraction >= 1.0 {
            "error"
        } else if self.fraction >= WARN_FRACTION {
            "warning"
        } else {
            "accent"
        }
    }

    /// The headline percent label, e.g. `"42%"` (or `"120%"` over budget).
    fn percent_label(self) -> String {
        format!("{:.0}%", self.percent)
    }
}

/// Format a spend/budget figure: an integer when whole, else two decimals (the
/// unit is unknown — could be USD or tokens — so this stays unit-agnostic).
fn fmt_figure(v: f64) -> String {
    if !v.is_finite() {
        return "—".to_owned();
    }
    if v.fract().abs() < 1e-9 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

// ── Plugin ───────────────────────────────────────────────────────────────────

impl Plugin for Usage {
    type Msg = UsageMsg;
    type Cmd = UsageCmd;

    fn manifest() -> Manifest {
        // SidebarTop card. Subscribes SlotVisible so the poller parks while the
        // sidebar is closed (#288/#305); requests OpenPage to open its own detail
        // panel (#349). The SDK adds the accent subscription (#376) on its behalf.
        let mut m = Manifest::new(PLUGIN_ID, Mount::SidebarTop).with_order(ORDER);
        m.subscribes = vec![StateKey::SlotVisible];
        m.capabilities = vec![Capability::OpenPage];
        m
    }

    fn init(cmds: CmdSender<Self::Cmd>) -> Self {
        // Resolve config synchronously so the seed render is already correct
        // (empty-state or Loading) with no flash and no premature network.
        let mode = match config::load() {
            ConfigState::Unconfigured => Mode::Unconfigured,
            ConfigState::Configured(cfg) => Mode::Configured {
                budget: cfg.budget,
                window_label: config::humanize_window(&cfg.window),
                state: DataState::Loading,
            },
        };
        Self { mode, cmd_tx: cmds }
    }

    fn sources(cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        // Unconfigured → no worker, no polling, no network (the #320 unblock).
        let ConfigState::Configured(cfg) = config::load() else {
            return None;
        };
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        tokio::spawn(fetch::poll_task(cfg, cmds, msg_tx));
        Some(Box::pin(UnboundedReceiverStream::new(msg_rx)))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            Input::App(msg) => self.on_msg(msg),
            // Bridge the sidebar's visibility to the poll task (#288). Harmless
            // when unconfigured: there's no worker, so the send just errs and is
            // dropped.
            Input::SlotVisible(visible) => {
                let _ = self.cmd_tx.send(UsageCmd::SetVisible(visible));
            }
            // A card click opens the plugin's own detail panel (#349). Only the
            // configured card is a button, so this never fires in empty-state.
            Input::Event { node, kind } if node == CARD_BTN && matches!(kind, EventKind::Click) => {
                return vec![Effect::OpenPage(Page::PluginSelf)];
            }
            // Foreign events, snapshots, effect results, audio (and any future
            // additive Input) are no-ops — no other host state is consumed.
            _ => {}
        }
        Vec::new()
    }

    fn view(&self) -> View {
        match &self.mode {
            Mode::Unconfigured => unconfigured_card().into(),
            Mode::Configured {
                budget,
                window_label,
                state,
            } => View::new(card(*budget, state)).panel(panel(*budget, window_label, state)),
        }
    }
}

// ── View ─────────────────────────────────────────────────────────────────────

/// A vertical box helper (the card and panel are vertical stacks). No `.card` /
/// `.ts-plugin-*` — the host's region wrapper supplies the card chrome (#319).
fn vbox(spacing: i32, children: Vec<Node>) -> Node {
    Node::Box {
        id: None,
        dir: Dir::Vertical,
        spacing,
        scroll: false,
        classes: Vec::new(),
        children,
    }
}

/// A `left … right` row: the [`Node::Spacer`] eats the slack so `right`
/// right-pins (the #299 justification primitive).
fn spaced_row(left: Node, right: Node) -> Node {
    Node::Box {
        id: None,
        dir: Dir::Horizontal,
        spacing: 8,
        scroll: false,
        classes: Vec::new(),
        children: vec![left, Node::Spacer, right],
    }
}

fn heading() -> Node {
    Node::Label {
        id: None,
        text: HEADING.to_owned(),
        classes: vec!["heading".to_owned()],
    }
}

fn dim_caption(text: &str) -> Node {
    Node::Label {
        id: None,
        text: text.to_owned(),
        classes: vec!["dim-label".to_owned()],
    }
}

/// An id'd numeric label (tabular figures) — the headline number slot.
fn numeric(id: &str, text: String, extra: &[&str]) -> Node {
    let mut classes = vec!["numeric".to_owned()];
    classes.extend(extra.iter().map(|c| (*c).to_owned()));
    Node::Label {
        id: Some(id.to_owned()),
        text,
        classes,
    }
}

/// The tinted gauge bar.
fn progress(id: &str, g: Gauge) -> Node {
    Node::Progress {
        id: Some(id.to_owned()),
        fraction: g.fraction,
        classes: vec![g.tint().to_owned()],
    }
}

/// A wrapping error line: a warning glyph beside a dim message that never blows
/// the card wide.
fn error_line(msg: &str) -> Node {
    Node::Box {
        id: None,
        dir: Dir::Horizontal,
        spacing: 8,
        scroll: false,
        classes: Vec::new(),
        children: vec![
            Node::Icon {
                id: None,
                name: "dialog-warning-symbolic".to_owned(),
                classes: vec!["error".to_owned()],
            },
            Node::Text {
                id: None,
                text: msg.to_owned(),
                max_width_chars: None,
                ellipsize: false,
                classes: vec!["dim-label".to_owned()],
            },
        ],
    }
}

/// The calm empty-state card (no button, no panel) — no dashboard configured.
fn unconfigured_card() -> Node {
    vbox(
        6,
        vec![
            heading(),
            dim_caption(UNCONFIGURED_TITLE),
            Node::Text {
                id: None,
                text: UNCONFIGURED_HINT.to_owned(),
                max_width_chars: None,
                ellipsize: false,
                classes: vec!["dim-label".to_owned()],
            },
        ],
    )
}

/// The compact card: a flat button wrapping the heading + headline figure (and,
/// with a budget, the gauge bar). Clicking it opens the detail panel.
fn card(budget: Option<f64>, state: &DataState) -> Node {
    Node::Button {
        id: CARD_BTN.to_owned(),
        classes: vec!["flat".to_owned()],
        child: Box::new(card_content(budget, state)),
    }
}

fn card_content(budget: Option<f64>, state: &DataState) -> Node {
    match state {
        DataState::Loading => vbox(4, vec![heading(), dim_caption(LOADING_TEXT)]),
        DataState::Error(msg) => vbox(4, vec![heading(), error_line(msg)]),
        DataState::Ready { spend, .. } => match budget.and_then(|b| Gauge::new(*spend, b)) {
            Some(g) => vbox(
                6,
                vec![
                    spaced_row(heading(), numeric(CARD_FIGURE, g.percent_label(), &[])),
                    progress(CARD_BAR, g),
                ],
            ),
            // No budget: honest spend figure, no gauge.
            None => spaced_row(heading(), numeric(CARD_FIGURE, fmt_figure(*spend), &[])),
        },
    }
}

/// The drawer detail panel: the gauge, the headline percent, and the window
/// figures + last-updated. Its root carries no card class — the drawer supplies
/// the chrome (#349).
fn panel(budget: Option<f64>, window_label: &str, state: &DataState) -> Node {
    let mut children = vec![heading()];
    match state {
        DataState::Loading => children.push(dim_caption(LOADING_TEXT)),
        DataState::Error(msg) => children.push(error_line(msg)),
        DataState::Ready { spend, updated } => {
            if let Some(g) = budget.and_then(|b| Gauge::new(*spend, b)) {
                children.push(progress(PANEL_BAR, g));
                children.push(numeric(PANEL_PERCENT, g.percent_label(), &["title-2"]));
            }
            children.push(detail_rows(*spend, budget, window_label, updated));
        }
    }
    Node::Box {
        id: Some(PANEL_ROOT.to_owned()),
        dir: Dir::Vertical,
        spacing: 8,
        scroll: false,
        classes: Vec::new(),
        children,
    }
}

/// The "name … value" rows under the gauge.
fn detail_rows(spend: f64, budget: Option<f64>, window_label: &str, updated: &str) -> Node {
    let mut rows = vec![detail_row("Spent", PANEL_SPENT, fmt_figure(spend))];
    if let Some(b) = budget {
        rows.push(detail_row("Budget", PANEL_BUDGET, fmt_figure(b)));
    }
    rows.push(detail_row("Window", PANEL_WINDOW, window_label.to_owned()));
    rows.push(detail_row("Updated", PANEL_UPDATED, updated.to_owned()));
    vbox(2, rows)
}

fn detail_row(name: &str, value_id: &str, value: String) -> Node {
    spaced_row(dim_caption(name), numeric(value_id, value, &[]))
}

fn main() {
    hytte_plugin::run::<Usage>();
}

#[cfg(test)]
mod tests {
    use super::{
        CARD_BAR, CARD_BTN, CARD_FIGURE, DataState, Gauge, Mode, PANEL_ROOT, PANEL_SPENT,
        PLUGIN_ID, UNCONFIGURED_HINT, Usage, UsageCmd, UsageMsg, fmt_figure,
    };
    use hytte_plugin::proto::{
        Capability, Effect, EventKind, Manifest, Mount, Node, Page, StateKey,
    };
    use hytte_plugin::{CmdReceiver, Input, Plugin};

    fn model(mode: Mode) -> (Usage, CmdReceiver<UsageCmd>) {
        let (tx, rx) = hytte_plugin::cmd_channel();
        (Usage { mode, cmd_tx: tx }, rx)
    }

    fn configured(state: DataState, budget: Option<f64>) -> Mode {
        Mode::Configured {
            budget,
            window_label: "last 5h".to_owned(),
            state,
        }
    }

    fn ready(spend: f64) -> DataState {
        DataState::Ready {
            spend,
            updated: "16:42".to_owned(),
        }
    }

    /// Depth-first search for a `Label`/`Text`/`Progress` node by id.
    fn find(node: &Node, id: &str) -> Option<Node> {
        match node {
            Node::Label { id: Some(n), .. }
            | Node::Text { id: Some(n), .. }
            | Node::Progress { id: Some(n), .. }
                if n == id =>
            {
                Some(node.clone())
            }
            Node::Box { children, .. } => children.iter().find_map(|c| find(c, id)),
            Node::Button { child, .. } => find(child, id),
            _ => None,
        }
    }

    fn text_of(node: &Node, id: &str) -> Option<String> {
        match find(node, id)? {
            Node::Label { text, .. } | Node::Text { text, .. } => Some(text),
            _ => None,
        }
    }

    fn classes_of(node: &Node, id: &str) -> Option<Vec<String>> {
        match find(node, id)? {
            Node::Label { classes, .. } | Node::Progress { classes, .. } => Some(classes),
            _ => None,
        }
    }

    fn tree_has_text(node: &Node, needle: &str) -> bool {
        match node {
            Node::Label { text, .. } | Node::Text { text, .. } => text == needle,
            Node::Box { children, .. } => children.iter().any(|c| tree_has_text(c, needle)),
            Node::Button { child, .. } => tree_has_text(child, needle),
            _ => false,
        }
    }

    // ── The window math (the testable heart) ──────────────────────────────────

    #[test]
    fn gauge_math_fraction_percent_and_over_budget() {
        let g = Gauge::new(12.0, 30.0).expect("has a budget");
        assert!((g.fraction - 0.4).abs() < 1e-9);
        assert!((g.percent - 40.0).abs() < 1e-9);
        assert_eq!(g.percent_label(), "40%");

        // Over budget: percent is honest (>100) but the bar clamps to full.
        let over = Gauge::new(36.0, 30.0).unwrap();
        assert!((over.fraction - 1.0).abs() < 1e-9, "bar clamps to full");
        assert_eq!(over.percent_label(), "120%", "percent stays honest");
    }

    #[test]
    fn gauge_none_without_a_usable_budget() {
        assert_eq!(Gauge::new(10.0, 0.0), None);
        assert_eq!(Gauge::new(10.0, -5.0), None);
        assert_eq!(Gauge::new(f64::NAN, 30.0), None);
        assert_eq!(Gauge::new(10.0, f64::INFINITY), None);
    }

    #[test]
    fn gauge_tint_climbs_accent_warning_error() {
        assert_eq!(Gauge::new(10.0, 100.0).unwrap().tint(), "accent"); // 10%
        assert_eq!(Gauge::new(79.0, 100.0).unwrap().tint(), "accent"); // just under
        assert_eq!(Gauge::new(80.0, 100.0).unwrap().tint(), "warning"); // the line
        assert_eq!(Gauge::new(99.0, 100.0).unwrap().tint(), "warning");
        assert_eq!(Gauge::new(100.0, 100.0).unwrap().tint(), "error"); // at budget
        assert_eq!(Gauge::new(150.0, 100.0).unwrap().tint(), "error"); // over
    }

    #[test]
    fn fmt_figure_is_integer_when_whole_else_two_dp() {
        assert_eq!(fmt_figure(30.0), "30");
        assert_eq!(fmt_figure(12.5), "12.50");
        assert_eq!(fmt_figure(12.345), "12.35");
        assert_eq!(fmt_figure(1234.0), "1234");
    }

    // ── Manifest ──────────────────────────────────────────────────────────────

    #[test]
    fn manifest_mounts_sidebar_top_and_subscribes_visibility() {
        let m: Manifest = Usage::manifest();
        assert_eq!(m.id, PLUGIN_ID);
        assert_eq!(m.mount, Mount::SidebarTop);
        assert_eq!(m.order, Some(super::ORDER));
        assert!(
            m.subscribes.contains(&StateKey::SlotVisible),
            "gates its poller on the sidebar visibility push (#288/#305)"
        );
        assert!(
            m.capabilities.contains(&Capability::OpenPage),
            "opens its own detail panel (#349)"
        );
        m.check_proto().expect("current proto version");
    }

    // ── View states ───────────────────────────────────────────────────────────

    #[test]
    fn unconfigured_is_a_calm_card_with_no_panel_and_no_button() {
        let (m, _rx) = model(Mode::Unconfigured);
        let v = m.view();
        assert!(v.panel.is_none(), "empty-state has no detail panel");
        assert!(
            !matches!(v.tree, Node::Button { .. }),
            "empty-state is not clickable"
        );
        assert!(
            tree_has_text(&v.tree, UNCONFIGURED_HINT),
            "shows the actionable hint"
        );
    }

    #[test]
    fn configured_loading_seed_is_a_button_with_a_panel() {
        let (m, _rx) = model(configured(DataState::Loading, Some(30.0)));
        let v = m.view();
        assert!(
            matches!(v.tree, Node::Button { .. }),
            "the card is clickable"
        );
        assert!(v.panel.is_some(), "a detail panel is published");
        assert!(
            find(&v.tree, CARD_BAR).is_none(),
            "no gauge before a reading"
        );
    }

    #[test]
    fn ready_with_budget_renders_percent_and_a_tinted_gauge() {
        let (m, _rx) = model(configured(ready(24.0), Some(30.0)));
        let v = m.view();
        // 24 / 30 = 80% → the warn line.
        assert_eq!(text_of(&v.tree, CARD_FIGURE).as_deref(), Some("80%"));
        assert_eq!(
            classes_of(&v.tree, CARD_BAR).as_deref(),
            Some(["warning".to_owned()].as_slice())
        );
        // The panel carries the figures.
        let board = v.panel.expect("a panel");
        assert!(
            matches!(&board, Node::Box { id: Some(id), .. } if id == PANEL_ROOT),
            "the panel root is the id'd board box"
        );
        assert_eq!(text_of(&board, PANEL_SPENT).as_deref(), Some("24"));
    }

    #[test]
    fn ready_without_budget_shows_spend_only() {
        let (m, _rx) = model(configured(ready(1234.0), None));
        let v = m.view();
        assert_eq!(text_of(&v.tree, CARD_FIGURE).as_deref(), Some("1234"));
        assert!(
            find(&v.tree, CARD_BAR).is_none(),
            "no gauge without a budget"
        );
    }

    // ── update / reducer ──────────────────────────────────────────────────────

    #[test]
    fn a_fetch_error_keeps_the_last_good_reading() {
        let (mut m, _rx) = model(configured(DataState::Loading, Some(30.0)));
        m.update(Input::App(UsageMsg::Reading {
            spend: 12.0,
            updated: "10:00".to_owned(),
        }));
        m.update(Input::App(UsageMsg::FetchError("boom".to_owned())));
        assert_eq!(text_of(&m.view().tree, CARD_FIGURE).as_deref(), Some("40%"));
    }

    #[test]
    fn a_first_fetch_error_surfaces() {
        let (mut m, _rx) = model(configured(DataState::Loading, Some(30.0)));
        m.update(Input::App(UsageMsg::FetchError("no route".to_owned())));
        assert!(
            tree_has_text(&m.view().tree, "no route"),
            "the first failure shows its reason"
        );
    }

    #[test]
    fn clicking_the_card_opens_the_panel() {
        let (mut m, _rx) = model(configured(ready(1.0), Some(30.0)));
        let fx = m.update(Input::Event {
            node: CARD_BTN.to_owned(),
            kind: EventKind::Click,
        });
        assert_eq!(fx, vec![Effect::OpenPage(Page::PluginSelf)]);
    }

    #[test]
    fn a_foreign_click_does_nothing() {
        let (mut m, _rx) = model(configured(ready(1.0), Some(30.0)));
        let fx = m.update(Input::Event {
            node: "not-ours".to_owned(),
            kind: EventKind::Click,
        });
        assert!(fx.is_empty());
    }

    #[test]
    fn slot_visibility_forwards_to_the_poll_lane() {
        let (mut m, mut rx) = model(configured(DataState::Loading, Some(30.0)));
        let fx = m.update(Input::SlotVisible(true));
        assert!(fx.is_empty(), "gating is plugin I/O, not a shell effect");
        assert!(matches!(rx.try_recv(), Ok(UsageCmd::SetVisible(true))));
        m.update(Input::SlotVisible(false));
        assert!(matches!(rx.try_recv(), Ok(UsageCmd::SetVisible(false))));
    }
}
