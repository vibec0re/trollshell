//! `hytte-plugin-infobroker` — the broker's plugin face 🛡️ (issue #487, phase 1a).
//!
//! A normal out-of-process [`hytte_plugin`] widget: a bar chip that opens the
//! broker's own drawer **panel** (#349/#415 `View { tree, panel }`), which lists
//! the durable grants (with per-row **Revoke**), the pending knocks (with
//! one-click **Allow**), the datasource status, the live sessions, and a
//! recent-requests audit trail. The heavy lifting — the socket server, the grant
//! store, the token TTL machine, the consent decisions — lives in the SDK-free
//! [`hytte_plugin_infobroker`] library ([`broker::serve`]); this file is just the
//! TEA shell that spawns it and paints its state.
//!
//! # Why a bar chip, not a sidebar card
//!
//! The broker must serve agents whether or not anyone is looking, so it must
//! **not** park on visibility — a bar chip is always "visible" and its poller is
//! never gated. The socket server runs from [`sources`](Plugin::sources) for the
//! session's whole life.
//!
//! # The two host effects
//!
//! - [`Effect::OpenPage(Page::PluginSelf)`] — clicking the chip opens this
//!   plugin's own panel (cap [`Capability::OpenPage`]).
//! - [`Effect::Notify`] — a denied auth/data knock raises one informational
//!   toast so the human sees it (cap [`Capability::Notify`], #414). Interactive
//!   Allow/Deny prompting shipped in phase 1b (#514) — a parked request surfaces
//!   as an interactive consent prompt via `Effect::RequestConsent`.

use hytte_plugin::proto::{
    Capability, ConsentDecision, DatasourceError, DatasourceOutcome, Dir, Effect, EventKind,
    Manifest, Mount, Node, Page, StateKey, UpcomingEvent,
};
use hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream;
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};
use hytte_plugin_infobroker::broker::{
    AuditView, BrokerMsg, BrokerSnapshot, Cmd, ConsentDecision as BrokerConsentDecision,
    DatasourceView, GrantView, Outcome, PendingView, QueryOutcome, QueryRequest, TokenView,
};
use hytte_plugin_infobroker::wire::CalendarEntry;
use tokio::sync::mpsc;

/// Stable plugin id — the host's mount-slot key, the audit-log subject, and the
/// notification app name.
const PLUGIN_ID: &str = "infobroker";

// Node ids. The chip button opens the panel; the per-row allow/revoke buttons
// carry the row index (resolved against the model snapshot in `update`).
const ROOT_ID: &str = "infobroker-root";
const CHIP_ID: &str = "infobroker-chip";
const PANEL_ID: &str = "infobroker-panel";

// ── The model ─────────────────────────────────────────────────────────────────

/// The plugin's whole state: the latest broker snapshot, the clock (for the
/// panel's relative labels), and the command lane to the broker task.
struct Infobroker {
    snapshot: BrokerSnapshot,
    now_unix: i64,
    /// Whether the session is locked (#484). While `true` the panel blanks its
    /// sensitive content (grants / sessions / audit) — the privacy note from the
    /// domain-StateKeys design.
    locked: bool,
    cmd_tx: CmdSender<Cmd>,
}

impl Plugin for Infobroker {
    type Msg = BrokerMsg;
    /// The panel drives the broker over the command lane (#280): revoke/allow.
    type Cmd = Cmd;

    /// Mounts [`Mount::BarRight`] as a chip. Requests [`Capability::OpenPage`]
    /// (open its own panel) and [`Capability::Notify`] (the denied-knock toast).
    /// Subscribes [`StateKey::Clock`] for the panel's "expires in" / "N ago"
    /// labels; the SDK adds the accent subscription on its behalf.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::BarRight);
        // `Clock` relabels the panel; `CalendarUpcoming` feeds the live copy that
        // `get calendar` serves (#484); `SessionLocked` drives the privacy blank.
        m.subscribes = vec![
            StateKey::Clock,
            StateKey::CalendarUpcoming,
            StateKey::SessionLocked,
        ];
        // `OpenPage` (open its own panel), `Notify` (the denied-knock toast),
        // `Consent` (#487 phase 1b — the interactive prompt + decision), plus the
        // two #484 push gates: `Calendar` (the calendar datasource) and
        // `SessionState` (the lock signal). `Consent`/`Calendar`/`SessionState` are
        // also the #305 opt-ins gating their respective host pushes.
        m.capabilities = vec![
            Capability::OpenPage,
            Capability::Notify,
            Capability::Consent,
            Capability::Calendar,
            Capability::SessionState,
            // #509: the requester side of the datasource protocol — the broker
            // sources `departures`/`weather` through their provider plugins via
            // `Effect::DatasourceQuery`, so it needs this cap.
            Capability::DatasourceQuery,
        ];
        m
    }

    fn init(cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            snapshot: BrokerSnapshot::default(),
            now_unix: 0,
            locked: false,
            cmd_tx: cmds,
        }
    }

    /// The broker socket server is the plugin's I/O source: it drains the panel
    /// command lane (`cmds`) and re-emits every state change as a [`BrokerMsg`]
    /// on this stream. Created per session; a disconnect drops it (rebinding the
    /// socket fresh on reconnect — which is what drops in-memory tokens on a
    /// shell restart, per the design).
    fn sources(cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        tokio::spawn(hytte_plugin_infobroker::serve(cmds, msg_tx));
        Some(Box::pin(UnboundedReceiverStream::new(msg_rx)))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            // The clock drives the panel's relative labels only.
            Input::Snapshot(snapshot) => {
                if let Some(clock) = snapshot.clock {
                    self.now_unix = clock.unix;
                }
                Vec::new()
            }
            // A broker state change: adopt the snapshot, and raise the toast on a
            // denied knock (the only place this plugin asks the shell for
            // anything beyond opening its panel).
            Input::App(BrokerMsg::Update { snapshot, toast }) => {
                self.snapshot = snapshot;
                toast
                    .map(|t| Effect::Notify {
                        summary: t.summary,
                        body: t.body,
                    })
                    .into_iter()
                    .collect()
            }
            // A parked request wants human consent (#487 phase 1b): surface it as
            // the shell's interactive prompt. The decision returns via
            // `Input::ConsentDecision` below.
            Input::App(BrokerMsg::RequestConsent(prompt)) => {
                vec![Effect::RequestConsent {
                    request_id: prompt.request_id,
                    agent: prompt.agent,
                    datasource: prompt.datasource,
                    scope: prompt.scope,
                    detail: prompt.detail,
                }]
            }
            // The broker wants a datasource query routed to the host (#509): turn it
            // into `Effect::DatasourceQuery`. The result returns via
            // `Input::DatasourceResult` below and is relayed back down the lane.
            Input::App(BrokerMsg::Query(QueryRequest {
                request_id,
                provider,
                scope,
                params,
            })) => {
                vec![Effect::DatasourceQuery {
                    request_id,
                    provider,
                    scope,
                    params,
                }]
            }
            // The human answered (or the host's 60 s timeout fired → `Deny`):
            // forward the decision down the lane to the broker, which resolves the
            // parked request. Fire-and-forget, no effect of its own.
            Input::ConsentDecision {
                request_id,
                decision,
            } => {
                let _ = self.cmd_tx.send(Cmd::Decision {
                    request_id,
                    decision: map_decision(decision),
                });
                Vec::new()
            }
            // The host's upcoming-calendar digest (#484): relay it down the lane
            // to the broker, which replaces the live copy `get calendar` serves.
            // Mapped from the proto type at the boundary (the library is SDK-free).
            Input::CalendarUpcoming(events) => {
                let entries = events.into_iter().map(map_event).collect();
                let _ = self.cmd_tx.send(Cmd::Calendar(entries));
                Vec::new()
            }
            // The session lock state (#484): drives the panel's privacy blank.
            Input::SessionLocked(locked) => {
                self.locked = locked;
                Vec::new()
            }
            // A routed datasource query's answer (#509): relay it down the lane to
            // the broker, which resolves the parked `get`. Mapped from the proto
            // type at the boundary (the library is SDK-free), exactly as consent
            // decisions and calendar digests are.
            Input::DatasourceResult {
                request_id,
                outcome,
            } => {
                let _ = self.cmd_tx.send(Cmd::QueryResult {
                    request_id,
                    outcome: map_query_outcome(outcome),
                });
                Vec::new()
            }
            Input::Event { node, kind } => self.on_event(&node, &kind),
            // No RunCommand / spectrum / now-playing (unsubscribed) / bar-chip
            // visibility handling needed.
            _ => Vec::new(),
        }
    }

    fn view(&self) -> View {
        View::new(self.chip()).panel(self.panel())
    }
}

impl Infobroker {
    /// Resolve a click: the chip opens the panel; an `allow:<n>` / `revoke:<n>`
    /// button resolves its row index against the current snapshot and dispatches
    /// the matching command down the lane.
    fn on_event(&mut self, node: &str, kind: &EventKind) -> Vec<Effect> {
        if *kind != EventKind::Click {
            return Vec::new();
        }
        if node == CHIP_ID {
            return vec![Effect::OpenPage(Page::PluginSelf)];
        }
        if let Some(pending) =
            parse_index(node, "allow:").and_then(|n| self.snapshot.pending.get(n))
        {
            let _ = self.cmd_tx.send(Cmd::Allow {
                agent: pending.agent.clone(),
                datasource: pending.datasource.clone(),
            });
        } else if let Some(grant) =
            parse_index(node, "revoke:").and_then(|n| self.snapshot.grants.get(n))
        {
            let _ = self.cmd_tx.send(Cmd::Revoke {
                agent: grant.agent.clone(),
                datasource: grant.datasource.clone(),
            });
        }
        Vec::new()
    }

    /// The bar chip: a neutral data-sharing icon (a warning triangle when
    /// something is pending), plus a small badge of the pending / live-session
    /// count. Clicking it opens the panel. The host wraps this in its own
    /// `.ts-plugin-chip` pill, so the root adds no card/chip class of its own.
    ///
    /// The idle icon is `emblem-shared-symbolic` — a neutral "shared data"
    /// glyph — deliberately **not** a padlock (#503): a red lock on the bar read
    /// as an alarm rather than a data broker. Lock imagery stays where it's
    /// semantically right: the interactive consent prompt.
    fn chip(&self) -> Node {
        let pending = self.snapshot.pending.len();
        let sessions = self.snapshot.tokens.len();
        let (icon_name, badge, badge_class) = if pending > 0 {
            ("dialog-warning-symbolic", pending.to_string(), "warning")
        } else if sessions > 0 {
            ("emblem-shared-symbolic", sessions.to_string(), "dim-label")
        } else {
            ("emblem-shared-symbolic", String::new(), "dim-label")
        };

        let mut chip_children = vec![Node::Icon {
            id: None,
            name: icon_name.to_owned(),
            classes: Vec::new(),
        }];
        if !badge.is_empty() {
            chip_children.push(label(&badge, &["numeric", badge_class]));
        }

        Node::Box {
            id: Some(ROOT_ID.to_owned()),
            dir: Dir::Horizontal,
            spacing: 0,
            scroll: false,
            classes: Vec::new(),
            children: vec![Node::Button {
                id: CHIP_ID.to_owned(),
                classes: vec!["flat".to_owned()],
                child: Box::new(Node::Row {
                    id: None,
                    classes: Vec::new(),
                    children: chip_children,
                }),
            }],
        }
    }

    /// The drawer panel: pending knocks, grants, datasources, sessions, activity.
    /// The drawer supplies the card chrome, so the panel root adds no
    /// `.card`/`.ts-plugin-*` class — only its own vertical spacing.
    ///
    /// # Layout (#500)
    ///
    /// A `title-4` panel title, then one grouped **section** per topic. Each
    /// section is an emphasized `heading` above a `.boxed-list` card (or a single
    /// muted line when empty); a hairline [`divider`] between sections gives the
    /// preferences-page hierarchy. Rows are two-line action rows (an emphasized
    /// title over muted secondary text) rather than a run of adjacent labels —
    /// the node vocab's `Row` has no inter-child spacing, so packing several bare
    /// labels into one rendered them concatenated ("fnorddeparturesalways").
    ///
    /// While the session is locked (#484) the panel blanks its sensitive content
    /// (grants / live sessions / audit trail expose who accessed what) behind a
    /// placeholder — the privacy note from the domain-StateKeys design.
    fn panel(&self) -> Node {
        if self.locked {
            return Node::Box {
                id: Some(PANEL_ID.to_owned()),
                dir: Dir::Vertical,
                spacing: 8,
                scroll: false,
                classes: Vec::new(),
                children: vec![
                    label("Info broker", &["title-4"]),
                    muted_text("Hidden while the session is locked."),
                ],
            };
        }
        // The topic sections, in order. Pending knocks lead when present (the
        // actionable top of the panel); the rest always render.
        let mut sections: Vec<Node> = Vec::new();
        if !self.snapshot.pending.is_empty() {
            sections.push(section(
                "Pending requests",
                list(
                    self.snapshot
                        .pending
                        .iter()
                        .enumerate()
                        .map(|(i, p)| pending_row(i, p))
                        .collect(),
                ),
            ));
        }
        sections.push(section(
            "Grants",
            if self.snapshot.grants.is_empty() {
                muted_text("No grants yet — an agent must be allowed before it can read.")
            } else {
                list(
                    self.snapshot
                        .grants
                        .iter()
                        .enumerate()
                        .map(|(i, g)| grant_row(i, g))
                        .collect(),
                )
            },
        ));
        sections.push(section(
            "Datasources",
            list(
                self.snapshot
                    .datasources
                    .iter()
                    .map(datasource_row)
                    .collect(),
            ),
        ));
        sections.push(section(
            "Sessions",
            if self.snapshot.tokens.is_empty() {
                muted_text("No active sessions.")
            } else {
                list(
                    self.snapshot
                        .tokens
                        .iter()
                        .map(|t| token_row(t, self.now_unix))
                        .collect(),
                )
            },
        ));
        sections.push(section(
            "Recent activity",
            if self.snapshot.audit.is_empty() {
                muted_text("No requests yet.")
            } else {
                list(
                    self.snapshot
                        .audit
                        .iter()
                        .map(|a| audit_row(a, self.now_unix))
                        .collect(),
                )
            },
        ));

        // Assemble: the panel title, then each section preceded by a divider so
        // the sections read as distinct groups (and the title is set off from the
        // first one).
        let mut children = vec![label("Info broker", &["title-4"])];
        for s in sections {
            children.push(divider());
            children.push(s);
        }

        Node::Box {
            id: Some(PANEL_ID.to_owned()),
            dir: Dir::Vertical,
            spacing: 8,
            scroll: true,
            classes: Vec::new(),
            children,
        }
    }
}

// ── Node builders ─────────────────────────────────────────────────────────────

fn label(text: &str, classes: &[&str]) -> Node {
    Node::Label {
        id: None,
        text: text.to_owned(),
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
    }
}

/// A wrapping, muted status line (won't force the drawer wider). Used for the
/// empty-state line under a section header.
fn muted_text(text: &str) -> Node {
    Node::Text {
        id: None,
        text: text.to_owned(),
        max_width_chars: None,
        ellipsize: false,
        classes: vec!["dim-label".to_owned()],
    }
}

/// A native carded list of rows.
fn list(children: Vec<Node>) -> Node {
    Node::ListBox {
        id: None,
        classes: vec!["boxed-list".to_owned()],
        children,
    }
}

/// One topic **section**: an emphasized `heading` above its `body` (a boxed
/// list, or a single muted line when empty), grouped tightly in a small-spacing
/// vertical box so the header reads as belonging to the rows beneath it. Sections
/// are separated by a [`divider`] in [`Infobroker::panel`].
fn section(title: &str, body: Node) -> Node {
    Node::Box {
        id: None,
        dir: Dir::Vertical,
        spacing: 6,
        scroll: false,
        classes: Vec::new(),
        children: vec![label(title, &["heading"]), body],
    }
}

/// A hairline section divider (`gtk::Separator`).
fn divider() -> Node {
    Node::Separator {
        classes: Vec::new(),
    }
}

/// A two-line list row: an emphasized `title` over muted `subtitle` secondary
/// text, with an optional right-pinned `trailing` node (a Revoke/Allow button or
/// a status label). The stacked title/subtitle live in a small-spacing vertical
/// box — a `Row`'s own inter-child spacing is 0, so folding several bare labels
/// into one row rendered them concatenated (#500); the explicit box spaces the
/// two lines, and single formatted strings carry any inline separators.
fn action_row(title: &str, subtitle: &str, trailing: Option<Node>) -> Node {
    let stack = Node::Box {
        id: None,
        dir: Dir::Vertical,
        spacing: 2,
        scroll: false,
        classes: Vec::new(),
        children: vec![
            label(title, &["heading"]),
            label(subtitle, &["dim-label", "caption"]),
        ],
    };
    let mut children = vec![stack];
    if let Some(t) = trailing {
        children.push(Node::Spacer);
        children.push(t);
    }
    Node::Row {
        id: None,
        classes: Vec::new(),
        children,
    }
}

/// A small labelled button (`id` carries the action, e.g. `revoke:0`).
fn action_button(id: String, text: &str, classes: &[&str]) -> Node {
    Node::Button {
        id,
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
        child: Box::new(label(text, &[])),
    }
}

/// A pending knock: `<agent>` over `wants <datasource>`, Allow right-pinned.
fn pending_row(index: usize, p: &PendingView) -> Node {
    action_row(
        &p.agent,
        &format!("wants {}", p.datasource),
        Some(action_button(
            format!("allow:{index}"),
            "Allow",
            &["suggested-action"],
        )),
    )
}

/// A durable grant: `<agent>` over `<datasource> · <decision>`, Revoke
/// right-pinned. The `datasource · decision` subtitle is one formatted string so
/// its two fields stay legibly separated (they used to render as adjacent bare
/// labels — "departuresalways").
fn grant_row(index: usize, g: &GrantView) -> Node {
    action_row(
        &g.agent,
        &format!("{} · {}", g.datasource, g.decision),
        Some(action_button(
            format!("revoke:{index}"),
            "Revoke",
            &["destructive-action"],
        )),
    )
}

/// A datasource: `<name>` over its muted `<status>` (e.g. `station 900192001`).
fn datasource_row(d: &DatasourceView) -> Node {
    action_row(&d.name, &d.status, None)
}

/// A live session token: `<agent>` over `expires in <label>`.
fn token_row(t: &TokenView, now_unix: i64) -> Node {
    action_row(
        &t.agent,
        &format!("expires in {}", expires_label(now_unix, t.expires_unix)),
        None,
    )
}

/// One audit-trail entry: a colored `granted`/`denied` tag and the
/// `<agent> · <resource>` it applied to on one spaced line, with the relative
/// timestamp right-pinned and muted. The outcome tag and the target sit in a
/// small-spacing horizontal box so they don't concatenate ("grantedfnord").
fn audit_row(a: &AuditView, now_unix: i64) -> Node {
    let outcome_class = match a.outcome {
        Outcome::Granted => "success",
        Outcome::Denied => "warning",
    };
    let head = Node::Box {
        id: None,
        dir: Dir::Horizontal,
        spacing: 6,
        scroll: false,
        classes: Vec::new(),
        children: vec![
            label(a.outcome.label(), &[outcome_class, "caption-heading"]),
            label(&format!("{} · {}", a.agent, a.resource), &[]),
        ],
    };
    Node::Row {
        id: None,
        classes: Vec::new(),
        children: vec![
            head,
            Node::Spacer,
            label(&ago_label(now_unix, a.at_unix), &["dim-label", "caption"]),
        ],
    }
}

// ── Pure helpers (unit-tested) ────────────────────────────────────────────────

/// Parse `"<prefix><n>"` into the row index `n`, or `None` if the node id isn't
/// a numbered action button of that kind.
fn parse_index(node: &str, prefix: &str) -> Option<usize> {
    node.strip_prefix(prefix).and_then(|s| s.parse().ok())
}

/// Map a proto [`UpcomingEvent`] onto the broker library's SDK-free
/// [`CalendarEntry`] (#484) — the boundary translation that keeps the library
/// free of the plugin proto, the same shape as [`map_decision`].
fn map_event(event: UpcomingEvent) -> CalendarEntry {
    CalendarEntry {
        start_unix: event.start_unix,
        end_unix: event.end_unix,
        title: event.title,
        calendar: event.calendar,
    }
}

/// Map the proto's consent decision onto the broker library's own mirror enum
/// (#487 phase 1b) — the SDK-free library defines its own so it never links the
/// proto, so the plugin translates at the boundary (as it maps a broker `Toast`
/// onto `Effect::Notify`).
fn map_decision(decision: ConsentDecision) -> BrokerConsentDecision {
    match decision {
        ConsentDecision::AllowOnce => BrokerConsentDecision::AllowOnce,
        ConsentDecision::AllowSession => BrokerConsentDecision::AllowSession,
        ConsentDecision::AllowAlways => BrokerConsentDecision::AllowAlways,
        ConsentDecision::Deny => BrokerConsentDecision::Deny,
    }
}

/// Map the proto's datasource query outcome onto the broker library's SDK-free
/// mirror (#509), translating at the boundary like [`map_decision`]. A failure
/// keeps the host's/provider's message and prefixes a short kind label so the
/// broker's `Response::error` reads well (`no provider`, `denied scope`, …).
fn map_query_outcome(outcome: DatasourceOutcome) -> QueryOutcome {
    match outcome {
        DatasourceOutcome::Ready(payload) => QueryOutcome::Ready(payload),
        DatasourceOutcome::Failed { error, message } => {
            let label = match error {
                DatasourceError::NotFound => "no provider",
                DatasourceError::ScopeDenied => "denied scope",
                DatasourceError::Timeout => "timed out",
                DatasourceError::Provider => "provider error",
            };
            QueryOutcome::Failed(format!("{label}: {message}"))
        }
    }
}

/// A coarse "expires in" label: `"2h 5m"`, `"9m"`, or `"<1m"`. Clock ticks
/// relabel it live; an already-past value (shouldn't reach the panel, since the
/// broker prunes) reads `"soon"`.
fn expires_label(now_unix: i64, expires_unix: i64) -> String {
    let secs = expires_unix - now_unix;
    if secs <= 0 {
        return "soon".to_owned();
    }
    let mins = secs / 60;
    if mins >= 60 {
        format!("{}h {}m", mins / 60, mins % 60)
    } else if mins >= 1 {
        format!("{mins}m")
    } else {
        "<1m".to_owned()
    }
}

/// A coarse "time ago" label for the audit trail. Tz-free (relative to the
/// clock's unix seconds), so it never depends on the machine timezone.
fn ago_label(now_unix: i64, at_unix: i64) -> String {
    let secs = now_unix - at_unix;
    if secs < 60 {
        "just now".to_owned()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn main() {
    hytte_plugin::run::<Infobroker>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use hytte_plugin_infobroker::broker::{BrokerSnapshot, PendingView};

    fn model() -> (Infobroker, CmdReceiver<Cmd>) {
        let (tx, rx) = hytte_plugin::cmd_channel();
        (
            Infobroker {
                snapshot: BrokerSnapshot::default(),
                now_unix: 1_750_000_000,
                locked: false,
                cmd_tx: tx,
            },
            rx,
        )
    }

    #[test]
    fn manifest_requests_openpage_notify_consent_and_subscribes_clock() {
        let m = Infobroker::manifest();
        assert_eq!(m.mount, Mount::BarRight);
        assert!(
            m.capabilities.contains(&Capability::OpenPage),
            "opens its own panel"
        );
        assert!(
            m.capabilities.contains(&Capability::Notify),
            "toasts denied knocks"
        );
        assert!(
            m.capabilities.contains(&Capability::Consent),
            "raises the interactive consent prompt (#487)"
        );
        assert!(
            m.subscribes.contains(&StateKey::Clock),
            "Clock relabels the panel"
        );
        // #484: the calendar datasource + the privacy blank, each capability-gated.
        assert!(m.subscribes.contains(&StateKey::CalendarUpcoming));
        assert!(m.subscribes.contains(&StateKey::SessionLocked));
        assert!(
            m.capabilities.contains(&Capability::Calendar),
            "gates the calendar push (get calendar)"
        );
        assert!(
            m.capabilities.contains(&Capability::SessionState),
            "gates the session-lock push (privacy blank)"
        );
    }

    #[test]
    fn a_calendar_input_dispatches_a_mapped_calendar_command() {
        let (mut m, mut rx) = model();
        let fx = m.update(Input::CalendarUpcoming(vec![UpcomingEvent {
            start_unix: 100,
            end_unix: 200,
            title: "standup".to_owned(),
            calendar: "Work".to_owned(),
        }]));
        assert!(fx.is_empty(), "the digest rides the lane, not an effect");
        match rx.try_recv() {
            Ok(Cmd::Calendar(entries)) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].title, "standup");
                assert_eq!(entries[0].start_unix, 100);
            }
            other => panic!("expected a Calendar command, got {other:?}"),
        }
    }

    #[test]
    fn a_locked_session_blanks_the_sensitive_panel_content() {
        let (mut m, _rx) = model();
        // Seed some sensitive state (a grant + audit) that the panel would show.
        m.snapshot = BrokerSnapshot {
            grants: vec![GrantView {
                agent: "claude".to_owned(),
                datasource: "departures".to_owned(),
                decision: "always",
            }],
            ..BrokerSnapshot::default()
        };
        // Unlocked: the panel lists the grant.
        assert!(
            panel_mentions(&m.panel(), "claude"),
            "unlocked shows grants"
        );
        // Locked: the sensitive content is blanked behind the placeholder.
        let _ = m.update(Input::SessionLocked(true));
        let panel = m.panel();
        assert!(
            !panel_mentions(&panel, "claude"),
            "a locked session hides the grants"
        );
        assert!(
            panel_mentions(&panel, "Hidden while the session is locked."),
            "the locked placeholder shows"
        );
        // Unlocking restores it.
        let _ = m.update(Input::SessionLocked(false));
        assert!(
            panel_mentions(&m.panel(), "claude"),
            "unlock restores grants"
        );
    }

    /// Whether any `Label`/`Text` node under `node` carries exactly `needle`.
    fn panel_mentions(node: &Node, needle: &str) -> bool {
        match node {
            Node::Label { text, .. } | Node::Text { text, .. } => text == needle,
            Node::Box { children, .. }
            | Node::Row { children, .. }
            | Node::ListBox { children, .. } => children.iter().any(|c| panel_mentions(c, needle)),
            Node::Button { child, .. } => panel_mentions(child, needle),
            _ => false,
        }
    }

    #[test]
    fn a_request_consent_broker_msg_raises_the_prompt_effect() {
        let (mut m, _rx) = model();
        let fx = m.update(Input::App(BrokerMsg::RequestConsent(
            hytte_plugin_infobroker::broker::ConsentPrompt {
                request_id: 5,
                agent: "claude".to_owned(),
                datasource: "departures".to_owned(),
                scope: "read access".to_owned(),
                detail: "claude wants to read the departures board".to_owned(),
            },
        )));
        match fx.as_slice() {
            [
                Effect::RequestConsent {
                    request_id,
                    agent,
                    datasource,
                    ..
                },
            ] => {
                assert_eq!(*request_id, 5);
                assert_eq!(agent, "claude");
                assert_eq!(datasource, "departures");
            }
            other => panic!("expected one RequestConsent effect, got {other:?}"),
        }
    }

    #[test]
    fn a_consent_decision_input_dispatches_a_mapped_decision_command() {
        let (mut m, mut rx) = model();
        let fx = m.update(Input::ConsentDecision {
            request_id: 5,
            decision: ConsentDecision::AllowSession,
        });
        assert!(fx.is_empty(), "the decision rides the lane, not an effect");
        assert_eq!(
            rx.try_recv(),
            Ok(Cmd::Decision {
                request_id: 5,
                decision: BrokerConsentDecision::AllowSession,
            })
        );
    }

    #[test]
    fn map_decision_covers_every_variant() {
        assert_eq!(
            map_decision(ConsentDecision::AllowOnce),
            BrokerConsentDecision::AllowOnce
        );
        assert_eq!(
            map_decision(ConsentDecision::AllowSession),
            BrokerConsentDecision::AllowSession
        );
        assert_eq!(
            map_decision(ConsentDecision::AllowAlways),
            BrokerConsentDecision::AllowAlways
        );
        assert_eq!(
            map_decision(ConsentDecision::Deny),
            BrokerConsentDecision::Deny
        );
    }

    #[test]
    fn map_query_outcome_translates_ready_and_every_error() {
        assert_eq!(
            map_query_outcome(DatasourceOutcome::Ready("payload".to_owned())),
            QueryOutcome::Ready("payload".to_owned()),
        );
        // Each proto error kind becomes a labeled, still-human broker failure.
        for (error, label) in [
            (DatasourceError::NotFound, "no provider"),
            (DatasourceError::ScopeDenied, "denied scope"),
            (DatasourceError::Timeout, "timed out"),
            (DatasourceError::Provider, "provider error"),
        ] {
            let QueryOutcome::Failed(msg) = map_query_outcome(DatasourceOutcome::Failed {
                error,
                message: "detail".to_owned(),
            }) else {
                panic!("a failure maps to QueryOutcome::Failed");
            };
            assert!(msg.starts_with(label), "{msg}");
            assert!(msg.contains("detail"));
        }
    }

    #[test]
    fn clicking_the_chip_opens_the_panel() {
        let (mut m, _rx) = model();
        let fx = m.on_event(CHIP_ID, &EventKind::Click);
        assert_eq!(fx, vec![Effect::OpenPage(Page::PluginSelf)]);
    }

    /// End-to-end through `update`: a chip click routes to `on_event` and opens
    /// the panel (guards the `Input::Event` arm — a dropped arm would make clicks
    /// silently dead while the direct-`on_event` test above still passed).
    #[test]
    fn a_chip_click_through_update_opens_the_panel() {
        let (mut m, _rx) = model();
        let fx = m.update(Input::Event {
            node: CHIP_ID.to_owned(),
            kind: EventKind::Click,
        });
        assert_eq!(fx, vec![Effect::OpenPage(Page::PluginSelf)]);
    }

    #[test]
    fn a_broker_update_with_a_toast_raises_notify() {
        let (mut m, _rx) = model();
        let fx = m.update(Input::App(BrokerMsg::Update {
            snapshot: BrokerSnapshot::default(),
            toast: Some(hytte_plugin_infobroker::Toast {
                summary: "infobroker: x denied".to_owned(),
                body: "x requested departures — denied.".to_owned(),
            }),
        }));
        assert!(matches!(fx.as_slice(), [Effect::Notify { .. }]));
    }

    #[test]
    fn a_broker_update_without_a_toast_is_silent_and_adopts_the_snapshot() {
        let (mut m, _rx) = model();
        let snap = BrokerSnapshot {
            pending: vec![PendingView {
                agent: "claude".to_owned(),
                datasource: "departures".to_owned(),
            }],
            ..BrokerSnapshot::default()
        };
        let fx = m.update(Input::App(BrokerMsg::Update {
            snapshot: snap.clone(),
            toast: None,
        }));
        assert!(fx.is_empty(), "no toast → asks the shell for nothing");
        assert_eq!(m.snapshot.pending, snap.pending, "the snapshot is adopted");
    }

    #[test]
    fn allow_button_dispatches_allow_for_the_indexed_pending_row() {
        let (mut m, mut rx) = model();
        m.snapshot.pending = vec![PendingView {
            agent: "claude".to_owned(),
            datasource: "departures".to_owned(),
        }];
        let fx = m.on_event("allow:0", &EventKind::Click);
        assert!(fx.is_empty(), "the command rides the lane, not an effect");
        assert_eq!(
            rx.try_recv(),
            Ok(Cmd::Allow {
                agent: "claude".to_owned(),
                datasource: "departures".to_owned(),
            })
        );
    }

    #[test]
    fn revoke_button_dispatches_revoke_for_the_indexed_grant_row() {
        let (mut m, mut rx) = model();
        m.snapshot.grants = vec![GrantView {
            agent: "claude".to_owned(),
            datasource: "departures".to_owned(),
            decision: "always",
        }];
        m.on_event("revoke:0", &EventKind::Click);
        assert_eq!(
            rx.try_recv(),
            Ok(Cmd::Revoke {
                agent: "claude".to_owned(),
                datasource: "departures".to_owned(),
            })
        );
    }

    #[test]
    fn a_stale_index_dispatches_nothing() {
        let (mut m, mut rx) = model();
        // No pending rows → allow:0 resolves to nothing (a click that raced a
        // re-render can't panic or mis-fire).
        m.on_event("allow:0", &EventKind::Click);
        assert!(
            rx.try_recv().is_err(),
            "no command for an out-of-range index"
        );
    }

    #[test]
    fn parse_index_only_matches_its_prefix() {
        assert_eq!(parse_index("allow:3", "allow:"), Some(3));
        assert_eq!(parse_index("revoke:0", "revoke:"), Some(0));
        assert_eq!(parse_index("allow:3", "revoke:"), None);
        assert_eq!(parse_index("allow:x", "allow:"), None);
        assert_eq!(parse_index("chip", "allow:"), None);
    }

    #[test]
    fn expires_label_is_coarse_and_positive() {
        let now = 1_000_000;
        assert_eq!(expires_label(now, now + 2 * 3600 + 5 * 60), "2h 5m");
        assert_eq!(expires_label(now, now + 9 * 60), "9m");
        assert_eq!(expires_label(now, now + 30), "<1m");
        assert_eq!(expires_label(now, now - 10), "soon");
    }

    #[test]
    fn ago_label_buckets_by_magnitude() {
        let now = 1_000_000;
        assert_eq!(ago_label(now, now - 5), "just now");
        assert_eq!(ago_label(now, now - 5 * 60), "5m ago");
        assert_eq!(ago_label(now, now - 3 * 3600), "3h ago");
        assert_eq!(ago_label(now, now - 2 * 86_400), "2d ago");
        assert_eq!(
            ago_label(now, now + 100),
            "just now",
            "future clamps to just now"
        );
    }

    #[test]
    fn panel_renders_sections_and_the_chip_badges_pending() {
        let (mut m, _rx) = model();
        m.snapshot = BrokerSnapshot {
            grants: vec![GrantView {
                agent: "claude".to_owned(),
                datasource: "departures".to_owned(),
                decision: "always",
            }],
            pending: vec![PendingView {
                agent: "scratch".to_owned(),
                datasource: "departures".to_owned(),
            }],
            ..BrokerSnapshot::default()
        };
        // The chip badges the pending count with the warning class.
        let View { tree, panel } = m.view();
        let Node::Box { children, .. } = &tree else {
            panic!("chip root is a Box");
        };
        let Node::Button { id, .. } = &children[0] else {
            panic!("chip is a Button");
        };
        assert_eq!(id, CHIP_ID);
        // The panel exists and leads with the title heading, then a divider +
        // the first section (the #500 grouped-section layout).
        let panel = panel.expect("the plugin defines a panel");
        let Node::Box { children, .. } = panel else {
            panic!("panel root is a Box");
        };
        assert!(matches!(&children[0], Node::Label { text, .. } if text == "Info broker"));
        assert!(
            matches!(&children[1], Node::Separator { .. }),
            "sections are set off by a divider"
        );
        assert!(
            matches!(&children[2], Node::Box { .. }),
            "each section is a grouped vertical box"
        );
    }

    #[test]
    fn grant_row_folds_datasource_and_decision_into_one_spaced_subtitle() {
        // The #500 regression: three bare labels in a spacing-0 `Row` rendered
        // concatenated ("fnorddeparturesalways"). Now the agent is the row title
        // and "<datasource> · <decision>" is a single formatted subtitle string.
        let g = GrantView {
            agent: "fnord".to_owned(),
            datasource: "departures".to_owned(),
            decision: "always",
        };
        let Node::Row { children, .. } = grant_row(0, &g) else {
            panic!("grant_row is a Row");
        };
        let Node::Box {
            children: stack, ..
        } = &children[0]
        else {
            panic!("row leads with the title/subtitle stack");
        };
        assert!(matches!(&stack[0], Node::Label { text, .. } if text == "fnord"));
        assert!(
            matches!(&stack[1], Node::Label { text, .. } if text == "departures · always"),
            "datasource and decision are one spaced subtitle, not adjacent bare labels"
        );
        // Revoke stays right-pinned after a spacer.
        assert!(matches!(&children[1], Node::Spacer));
        assert!(matches!(&children[2], Node::Button { id, .. } if id == "revoke:0"));
    }

    #[test]
    fn audit_row_spaces_the_outcome_tag_from_its_target() {
        let a = AuditView {
            at_unix: 1_750_000_000,
            agent: "fnord".to_owned(),
            resource: "departures".to_owned(),
            outcome: Outcome::Granted,
        };
        let Node::Row { children, .. } = audit_row(&a, 1_750_000_000) else {
            panic!("audit_row is a Row");
        };
        // The outcome tag + target share a spaced horizontal box (spacing > 0), so
        // "granted" and "fnord · departures" can't concatenate ("grantedfnord").
        let Node::Box {
            spacing,
            dir,
            children: head,
            ..
        } = &children[0]
        else {
            panic!("audit head is a Box");
        };
        assert_eq!(*dir, Dir::Horizontal);
        assert!(*spacing > 0, "the outcome tag is spaced off its target");
        assert!(matches!(&head[0], Node::Label { text, .. } if text == "granted"));
        assert!(matches!(&head[1], Node::Label { text, .. } if text == "fnord · departures"));
    }
}
