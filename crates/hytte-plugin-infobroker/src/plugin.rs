//! `hytte-plugin-infobroker` — the broker's plugin face 🛡️ (issue #487, phase 1a).
//!
//! A normal out-of-process [`hytte_plugin`] widget: a bar chip that opens the
//! broker's own drawer **panel** (#349/#415 `View { tree, panel }`), which lists
//! the durable grants (with per-row **Revoke**), the pending knocks (with
//! one-click **Allow**), the datasource status, the live sessions, and a
//! recent-requests audit trail. The heavy lifting — the socket server, the grant
//! store, the token TTL machine, the consent decisions — lives in the SDK-free
//! [`hytte_plugin_infobroker`] library
//! ([`broker::serve`](hytte_plugin_infobroker::broker::serve)); this file is just the
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
//! - `Effect::OpenPage(Page::PluginSelf)` — clicking the chip opens this
//!   plugin's own panel (cap [`Capability::OpenPage`]).
//! - [`Effect::Notify`] — a denied auth/data knock raises one informational
//!   toast so the human sees it (cap [`Capability::Notify`], #414). Interactive
//!   Allow/Deny prompting shipped in phase 1b (#514) — a parked request surfaces
//!   as an interactive consent prompt via `Effect::RequestConsent`.

use hytte_plugin::proto::{
    CONSENT_PROMPT_GRACE_SECS, CONSENT_PROMPT_TIMEOUT_SECS, Capability, ConsentDecision,
    DatasourceError, DatasourceOutcome, Dir, Effect, EventKind, Manifest, Mount, Node, Page,
    StateKey, UpcomingEvent,
};
use hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream;
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};
use hytte_plugin_infobroker::broker::{
    AuditView, BrokerMsg, BrokerSnapshot, CONSENT_PARK_TIMEOUT, Cmd,
    ConsentDecision as BrokerConsentDecision, DatasourceView, GrantView, Outcome, PendingView,
    QueryOutcome, QueryRequest, TokenView,
};
use hytte_plugin_infobroker::wire::CalendarEntry;
use tokio::sync::{mpsc, oneshot};

/// Stable plugin id — the host's mount-slot key, the audit-log subject, and the
/// notification app name.
const PLUGIN_ID: &str = "infobroker";

std::thread_local! {
    /// Bridges [`Infobroker::init`] to [`Infobroker::sources`] — the shutdown
    /// half of the `#1079` handshake (see [`Infobroker::shutdown`]'s doc).
    /// The `Plugin::sources` signature takes only the command receiver, with
    /// no way to reach `self`, so there is no other way to hand it the
    /// receiver `init` just created; the SDK's `session()` calls `init` then
    /// `sources` back to back on the very same thread within one session
    /// (`hytte-plugin/src/runtime.rs`), which is exactly the precedent
    /// `hytte_plugin::display`'s own `NEGOTIATED` thread-local already relies
    /// on for the same reason. `sources` `.take()`s it the moment it reads it,
    /// so a stray second call
    /// (there is at most one per session) degrades to no shutdown-drain
    /// wiring rather than reusing a stale receiver.
    static SHUTDOWN_RX: std::cell::RefCell<Option<oneshot::Receiver<oneshot::Sender<()>>>> =
        const { std::cell::RefCell::new(None) };
}

// The broker's park deadline is what answers a consent card nobody clicked: the
// shell sends no decision for a timed-out, `Esc`-dismissed, superseded or
// undrawn card (#1542 item 5), so the parked `auth` waits for this deadline and
// gets a transient refusal. It must outlast the card by the proto's race
// margin, or a click in the card's last seconds lands after the broker gave up
// and is dropped as late. The library spells the number out (it stays
// SDK-free), so the check lives here, where the proto's two constants are in
// reach.
const _: () = assert!(
    CONSENT_PARK_TIMEOUT.as_secs() >= CONSENT_PROMPT_TIMEOUT_SECS + CONSENT_PROMPT_GRACE_SECS
);

// Node ids. The chip button opens the panel; the per-row allow/revoke buttons
// carry the `(agent, datasource)` pair their row names — see `row_action_id`.
const ROOT_ID: &str = "infobroker-root";
const CHIP_ID: &str = "infobroker-chip";
const PANEL_ID: &str = "infobroker-panel";
/// The prefix of a pending row's **Allow** button id (see [`row_action_id`]).
const ALLOW_PREFIX: &str = "allow:";
/// The prefix of a grant row's **Revoke** button id (see [`row_action_id`]).
const REVOKE_PREFIX: &str = "revoke:";

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
    /// The shutdown-drain handshake's request half (#1079): `send`ing an ack
    /// channel on this asks the broker task (`sources`'s spawned
    /// `broker::serve_with_shutdown`) to drain the grant store and reply on
    /// it once that finishes — see [`Plugin::shutdown`]. `None` once used (or
    /// if `sources` never ran) so a second `shutdown` call — the SDK only
    /// calls it once, but nothing enforces that at this layer — is a no-op
    /// rather than sending into an already-torn-down receiver.
    shutdown_req: Option<oneshot::Sender<oneshot::Sender<()>>>,
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
        let mut m =
            Manifest::new(PLUGIN_ID, Mount::BarRight).with_version(env!("CARGO_PKG_VERSION"));
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
        // #1079: mint the shutdown-drain handshake's pair now, keep the
        // request half on the model, and stash the other half where `sources`
        // (called next, same thread, same session — see `SHUTDOWN_RX`'s doc)
        // can hand it to the broker task it is about to spawn.
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        SHUTDOWN_RX.with_borrow_mut(|cell| *cell = Some(shutdown_rx));
        Self {
            snapshot: BrokerSnapshot::default(),
            now_unix: 0,
            locked: false,
            cmd_tx: cmds,
            shutdown_req: Some(shutdown_tx),
        }
    }

    /// The broker socket server is the plugin's I/O source: it drains the panel
    /// command lane (`cmds`) and re-emits every state change as a [`BrokerMsg`]
    /// on this stream. Created per session; a disconnect drops it (rebinding the
    /// socket fresh on reconnect — which is what drops in-memory tokens on a
    /// shell restart, per the design).
    ///
    /// The SDK calls this **after** it writes `Register` but **before** it reads
    /// any host frame, so it also runs in a duplicate process whose registration
    /// the host is about to reject on its `IdGuard`. `serve` therefore probes
    /// the path for a live incumbent and stands down instead of unlinking it
    /// (#995) — do not reintroduce an unconditional bind here.
    ///
    /// This `tokio::spawn` puts `serve` on the SDK's **current-thread** runtime
    /// (`hytte-plugin/src/runtime.rs`), which is the right shape — the broker
    /// is this plugin session's I/O source, not a second daemon with its own
    /// runtime the way `hytte-claude-bridge`'s HTTP listener is. The cost is
    /// that any synchronous step inside `serve` holds the one thread there is
    /// and stalls every timer on it, so `serve` runs its blocking work through
    /// `spawn_blocking` (#1059) — see `broker::serve_with_grant_loader`. Keep
    /// new blocking work in `serve` behind that same hop rather than moving
    /// this spawn.
    ///
    /// #1079: spawns `broker::serve_with_shutdown` instead of plain `serve`
    /// whenever `init` (this same session, this same thread — see
    /// `SHUTDOWN_RX`'s doc) left a receiver waiting, wiring the broker task to
    /// [`Infobroker::shutdown`]'s drain request. The `None` arm (the
    /// thread-local wasn't populated — not reachable from a real session, but
    /// cheap to fall back from) keeps the plain, undrained `serve` rather than
    /// failing the session over it.
    fn sources(cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        match SHUTDOWN_RX.with_borrow_mut(Option::take) {
            Some(shutdown_rx) => {
                tokio::spawn(hytte_plugin_infobroker::broker::serve_with_shutdown(
                    cmds,
                    msg_tx,
                    shutdown_rx,
                ));
            }
            None => {
                tokio::spawn(hytte_plugin_infobroker::serve(cmds, msg_tx));
            }
        }
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
                    // #947 P3 added the field; the broker's ask is a standing
                    // *grant* over a datasource, which is exactly the card this
                    // names — and it is `skip_serializing_if`-skipped, so these
                    // bytes are unchanged.
                    choices: hytte_plugin::proto::ConsentChoices::Grant,
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
            // The human clicked a button: forward the decision down the lane
            // to the broker, which resolves the parked request. Fire-and-forget,
            // no effect of its own. A card nobody answered sends nothing
            // (#1542 item 5), and the broker's own park deadline answers the
            // agent transiently instead — so a `Deny` here is a click (on a
            // current shell; see `ConsentChoices::unanswered`'s version skew).
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
            Input::Event { node, kind, .. } => self.on_event(&node, &kind),
            // No RunCommand / spectrum / now-playing (unsubscribed) / bar-chip
            // visibility handling needed.
            _ => Vec::new(),
        }
    }

    fn view(&self) -> View {
        View::new(self.chip()).panel(self.panel())
    }

    /// #1079: ask the broker task to drain the grant store's writer lane
    /// (`GrantStore::drain`, via `broker::serve_with_shutdown`'s shutdown
    /// arm) and wait for its ack before returning, so a queued grant change
    /// (an `Allow`/`Revoke` click applied just before the SDK noticed
    /// `SIGTERM`) is provably on disk before the process exits — the exact
    /// case #1074's re-review measured as lost otherwise (`Runtime::drop`
    /// cancelling the drain task in 39.5 µs). Runs under the SDK's own grace
    /// period, so a wedged broker task cannot hang the process past that.
    ///
    /// A no-op if `sources` never ran (`shutdown_req` stays `None`) or if
    /// this already ran once this session.
    async fn shutdown(&mut self) {
        let Some(req) = self.shutdown_req.take() else {
            return;
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        if req.send(ack_tx).is_ok() {
            let _ = ack_rx.await;
        }
    }
}

impl Infobroker {
    /// Resolve a click: the chip opens the panel; an Allow / Revoke button
    /// dispatches the matching command for the `(agent, datasource)` pair its
    /// own id carries ([`parse_row_action_id`]).
    ///
    /// The pair comes from the id and **not** from the model's snapshot (the
    /// #1489 follow-up audit, M1). The ids used to carry a row index, read
    /// against `self.snapshot` here — which is the *newest* snapshot, not the
    /// one the host had painted when the human clicked. The pending rows are
    /// derived from the 30-entry audit ring, so a same-uid process re-asking
    /// under a name with a standing deny (each refusal is an audit entry) can
    /// churn the ring fast enough to evict an older row: a click on the row
    /// that said `claude` then resolved `allow:0` against a list whose row 0
    /// was the other name — and wrote a durable `always` grant for it. Revoke
    /// had the same shape (a double-click on a row that a first revoke had
    /// just shifted). Now the id is the label: whatever the snapshot has
    /// become, a click acts on exactly the pair its row showed, and on nothing
    /// if the id is not one this plugin renders. No membership check against
    /// the snapshot either — a row that has since fallen off the ring was still
    /// what the human read and chose, and Allow/Revoke are idempotent.
    fn on_event(&mut self, node: &str, kind: &EventKind) -> Vec<Effect> {
        if *kind != EventKind::Click {
            return Vec::new();
        }
        if node == CHIP_ID {
            return vec![Effect::OpenPage(Page::PluginSelf)];
        }
        if let Some((agent, datasource)) = parse_row_action_id(node, ALLOW_PREFIX) {
            let _ = self.cmd_tx.send(Cmd::Allow { agent, datasource });
        } else if let Some((agent, datasource)) = parse_row_action_id(node, REVOKE_PREFIX) {
            let _ = self.cmd_tx.send(Cmd::Revoke { agent, datasource });
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
            tooltip: None,
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
                    spacing: 0,
                    id: None,
                    classes: Vec::new(),
                    children: chip_children,
                    tooltip: None,
                }),
            }],
            tooltip: None,
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
                tooltip: None,
            };
        }
        // The topic sections, in order. Pending knocks lead when present (the
        // actionable top of the panel); the rest always render.
        let mut sections: Vec<Node> = Vec::new();
        if !self.snapshot.pending.is_empty() {
            sections.push(section(
                "Pending requests",
                list(self.snapshot.pending.iter().map(pending_row).collect()),
            ));
        }
        sections.push(section(
            "Grants",
            if self.snapshot.grants.is_empty() {
                muted_text("No grants yet — an agent must be allowed before it can read.")
            } else {
                list(self.snapshot.grants.iter().map(grant_row).collect())
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
        // first one). A `notice` — this process is not serving the socket, e.g.
        // a duplicate that stood down (#995) — rides directly under the title,
        // ahead of everything, because it explains why the rest is empty. A
        // `store_notice` — `grants.toml` could not be read, so nothing here is
        // being saved (the #1489 follow-up audit, M3) — follows it, for the
        // same reason: it explains why the Grants section is empty, and why a
        // click on Allow or Revoke will not outlive the broker.
        let mut children = vec![label("Info broker", &["title-4"])];
        for notice in [&self.snapshot.notice, &self.snapshot.store_notice]
            .into_iter()
            .flatten()
        {
            children.push(notice_line(notice));
        }
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
            tooltip: None,
        }
    }
}

// ── Node builders ─────────────────────────────────────────────────────────────

fn label(text: &str, classes: &[&str]) -> Node {
    Node::Label {
        id: None,
        text: text.to_owned(),
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
        tooltip: None,
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
        tooltip: None,
    }
}

/// A wrapping, **warning**-styled line for a
/// [`BrokerSnapshot::notice`](hytte_plugin_infobroker::broker::BrokerSnapshot::notice):
/// this broker is not serving its socket (#995). Louder than [`muted_text`] on
/// purpose — an empty panel with no explanation reads as an idle broker.
fn notice_line(text: &str) -> Node {
    Node::Text {
        id: None,
        text: text.to_owned(),
        max_width_chars: None,
        ellipsize: false,
        classes: vec!["warning".to_owned()],
        tooltip: None,
    }
}

/// A native carded list of rows.
fn list(children: Vec<Node>) -> Node {
    Node::ListBox {
        dense: false,
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
        tooltip: None,
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
        tooltip: None,
    };
    let mut children = vec![stack];
    if let Some(t) = trailing {
        children.push(Node::Spacer);
        children.push(t);
    }
    Node::Row {
        spacing: 0,
        id: None,
        classes: Vec::new(),
        children,
        tooltip: None,
    }
}

/// A small labelled button (`id` carries the action — see [`row_action_id`]).
fn action_button(id: String, text: &str, classes: &[&str]) -> Node {
    Node::Button {
        id,
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
        child: Box::new(label(text, &[])),
    }
}

/// A pending knock: `<agent>` over `wants <datasource>`, Allow right-pinned,
/// its id naming the same pair the row shows.
fn pending_row(p: &PendingView) -> Node {
    action_row(
        &p.agent,
        &format!("wants {}", p.datasource),
        Some(action_button(
            row_action_id(ALLOW_PREFIX, &p.agent, &p.datasource),
            "Allow",
            &["suggested-action"],
        )),
    )
}

/// A durable grant: `<agent>` over `<datasource> · <decision>`, Revoke
/// right-pinned. The `datasource · decision` subtitle is one formatted string so
/// its two fields stay legibly separated (they used to render as adjacent bare
/// labels — "departuresalways"). The Revoke id names the row's pair.
fn grant_row(g: &GrantView) -> Node {
    action_row(
        &g.agent,
        &format!("{} · {}", g.datasource, g.decision),
        Some(action_button(
            row_action_id(REVOKE_PREFIX, &g.agent, &g.datasource),
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
        tooltip: None,
    };
    Node::Row {
        spacing: 0,
        id: None,
        classes: Vec::new(),
        children: vec![
            head,
            Node::Spacer,
            label(&ago_label(now_unix, a.at_unix), &["dim-label", "caption"]),
        ],
        tooltip: None,
    }
}

// ── Pure helpers (unit-tested) ────────────────────────────────────────────────

/// The id of a row's action button: `<prefix><agent>:<datasource>`, each part
/// [`encode_id_part`]-escaped — the pair the row's own labels show, so a click
/// resolves by value rather than by a row index (the #1489 follow-up audit,
/// M1; see [`Infobroker::on_event`]).
///
/// A [`NodeId`](hytte_plugin::proto::NodeId) is any string and the host
/// imposes no charset on it, so the escaping is not for the wire: it is so
/// that the `:` between the parts is unambiguous whatever a name contains.
/// `grants.toml` is hand-editable and is not re-checked against the agent-name
/// rule on load, so a Revoke button must still round-trip a name with a `:`, a
/// `%` or a newline in it. Two rows for the same pair (a hand-written
/// duplicate) share an id, which the host's keyed diff tolerates — each sits
/// in its own unkeyed row — and their buttons do the same thing anyway.
fn row_action_id(prefix: &str, agent: &str, datasource: &str) -> String {
    format!(
        "{prefix}{}:{}",
        encode_id_part(agent),
        encode_id_part(datasource)
    )
}

/// Parse a [`row_action_id`] with `prefix` back into its `(agent, datasource)`,
/// or `None` if `node` is not exactly such an id: a different prefix, a
/// missing or extra `:`, a malformed escape, or bytes that are not UTF-8.
fn parse_row_action_id(node: &str, prefix: &str) -> Option<(String, String)> {
    let (agent, datasource) = node.strip_prefix(prefix)?.split_once(':')?;
    Some((decode_id_part(agent)?, decode_id_part(datasource)?))
}

/// Percent-escape `part` for a [`row_action_id`]: the RFC 3986 unreserved
/// bytes (`A–Z a–z 0–9 - . _ ~`) stay as they are, so the ids of ordinary
/// names stay readable in a debug dump (`allow:claude:departures`); every
/// other UTF-8 byte — `:` and `%` among them — becomes `%XX`.
fn encode_id_part(part: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(part.len());
    for byte in part.bytes() {
        if is_unreserved(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0F)]));
        }
    }
    out
}

/// RFC 3986's unreserved set — the bytes [`encode_id_part`] leaves literal.
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// The inverse of [`encode_id_part`], strict: a byte outside the unreserved
/// set that is not part of a `%XX` escape (so `:` too) is refused rather than
/// passed through, as is an escape without two hex digits, and the decoded
/// bytes must be UTF-8.
fn decode_id_part(part: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(part.len());
    let mut rest = part.bytes();
    while let Some(byte) = rest.next() {
        if byte == b'%' {
            let hi = char::from(rest.next()?).to_digit(16)?;
            let lo = char::from(rest.next()?).to_digit(16)?;
            bytes.push(u8::try_from(hi * 16 + lo).ok()?);
        } else if is_unreserved(byte) {
            bytes.push(byte);
        } else {
            return None;
        }
    }
    String::from_utf8(bytes).ok()
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
                // These plain-`#[test]`s build the model directly, bypassing
                // `init`/`sources` (and the thread-local handshake between
                // them) entirely — so there is no shutdown-drain wiring to
                // give it here. `Infobroker::shutdown`'s own no-op-on-`None`
                // path is exactly what makes that safe.
                shutdown_req: None,
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

    /// #1542 item 5, the plugin's half: the card this plugin asks the shell for
    /// sends nothing when nobody answers it (a timeout, an `Esc`), so nothing
    /// reaches the broker's lane and the parked `auth` is left to the broker's
    /// own park deadline — a transient refusal with nothing persisted, which
    /// the library's `an_unanswered_consent_knock_persists_nothing_and_is_refused_transiently`
    /// pins.
    ///
    /// The card is read off the effect this plugin actually emits and the
    /// silence rule off the proto, so a change to either is seen here.
    ///
    /// Falsification: give `ConsentChoices::Grant` its `Some(Deny)` silence
    /// rule back and this reds — the timeout's `Deny` rides the lane as a
    /// `Cmd::Decision`, which the broker persists as a standing refusal. Have
    /// this plugin ask with `ConsentChoices::Approval` instead and it reds on
    /// the card assertion.
    #[test]
    fn an_unanswered_card_puts_no_decision_on_the_broker_lane() {
        let (mut m, mut rx) = model();
        let fx = m.update(Input::App(BrokerMsg::RequestConsent(
            hytte_plugin_infobroker::broker::ConsentPrompt {
                request_id: 5,
                agent: "claude".to_owned(),
                datasource: "departures".to_owned(),
                scope: "read access".to_owned(),
                detail: "claude wants to read the departures board".to_owned(),
            },
        )));
        let [
            Effect::RequestConsent {
                request_id,
                choices,
                ..
            },
        ] = fx.as_slice()
        else {
            panic!("expected one RequestConsent effect, got {fx:?}");
        };
        // The broker's ask is a standing grant over a datasource, so it is the
        // four-button card; an `Approval` card would lose Allow once, This
        // session and Always (#1601 review, NIT-1).
        assert_eq!(*choices, hytte_plugin::proto::ConsentChoices::Grant);

        // What the shell sends for this card when nobody answers it.
        if let Some(decision) = choices.unanswered() {
            let _ = m.update(Input::ConsentDecision {
                request_id: *request_id,
                decision,
            });
        }

        let lane = rx.try_recv();
        assert!(
            lane.is_err(),
            "an unanswered card must put no decision on the broker's lane (#1542), got {lane:?}"
        );
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
        let fx = m.update(Input::event(CHIP_ID, EventKind::Click));
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

    fn pending(agent: &str, datasource: &str) -> PendingView {
        PendingView {
            agent: agent.to_owned(),
            datasource: datasource.to_owned(),
        }
    }

    fn grant(agent: &str, datasource: &str) -> GrantView {
        GrantView {
            agent: agent.to_owned(),
            datasource: datasource.to_owned(),
            decision: "always",
        }
    }

    /// The id of the one action button in a `pending_row`/`grant_row` — what
    /// the host sends back when that row's button is clicked.
    fn button_id(row: &Node) -> String {
        let Node::Row { children, .. } = row else {
            panic!("an action row is a Row");
        };
        children
            .iter()
            .find_map(|c| match c {
                Node::Button { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("the row has its action button")
    }

    #[test]
    fn allow_button_dispatches_allow_for_its_own_row() {
        let (mut m, mut rx) = model();
        let row = pending("claude", "departures");
        m.snapshot.pending = vec![row.clone()];
        let fx = m.on_event(&button_id(&pending_row(&row)), &EventKind::Click);
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
    fn revoke_button_dispatches_revoke_for_its_own_row() {
        let (mut m, mut rx) = model();
        let row = grant("claude", "departures");
        m.snapshot.grants = vec![row.clone()];
        m.on_event(&button_id(&grant_row(&row)), &EventKind::Click);
        assert_eq!(
            rx.try_recv(),
            Ok(Cmd::Revoke {
                agent: "claude".to_owned(),
                datasource: "departures".to_owned(),
            })
        );
    }

    /// M1, the audit's scenario: the human reads a pending row for `claude`
    /// and clicks its Allow — but by the time the click reaches the plugin,
    /// ring churn has evicted `claude`'s denials and the other name now sits
    /// where `claude` was. The click must allow `claude`, never the row that
    /// now holds its old position.
    ///
    /// Mutation: resolve the click against `self.snapshot.pending` by position
    /// again (the pre-fix `allow:<n>`), and this dispatches
    /// `Allow { agent: "evil" }` — a durable grant the human never chose.
    #[test]
    fn an_allow_click_acts_on_the_row_it_labelled_after_the_rows_shift() {
        let (mut m, mut rx) = model();
        let claude = pending("claude", "departures");
        m.snapshot.pending = vec![claude.clone(), pending("evil", "departures")];
        // What the host painted, and so what the human clicked.
        let clicked = button_id(&pending_row(&claude));

        // The rows shift before the click lands.
        m.snapshot.pending = vec![pending("evil", "departures")];
        m.on_event(&clicked, &EventKind::Click);

        assert_eq!(
            rx.try_recv(),
            Ok(Cmd::Allow {
                agent: "claude".to_owned(),
                datasource: "departures".to_owned(),
            }),
            "the click allows exactly the pair its row showed"
        );
        assert!(rx.try_recv().is_err(), "and nothing else");
    }

    /// M1, Revoke's shape: a second click on a row a first revoke already
    /// removed (a double click, or a click on a stale paint) must revoke
    /// that row's pair again — a no-op broker-side — not the grant that
    /// moved up into its slot.
    #[test]
    fn a_revoke_click_acts_on_the_row_it_labelled_after_the_rows_shift() {
        let (mut m, mut rx) = model();
        let first = grant("claude", "departures");
        m.snapshot.grants = vec![first.clone(), grant("claude", "calendar")];
        let clicked = button_id(&grant_row(&first));

        m.snapshot.grants = vec![grant("claude", "calendar")];
        m.on_event(&clicked, &EventKind::Click);

        assert_eq!(
            rx.try_recv(),
            Ok(Cmd::Revoke {
                agent: "claude".to_owned(),
                datasource: "departures".to_owned(),
            })
        );
    }

    /// The ids round-trip any pair — including names with the separator, the
    /// escape character, a newline (a hand-edited or pre-M2 `grants.toml`
    /// row, which still needs a working Revoke), non-ASCII text, and the
    /// empty string — and an ordinary name's id stays readable.
    #[test]
    fn row_action_ids_round_trip_any_pair() {
        assert_eq!(
            row_action_id(ALLOW_PREFIX, "claude", "departures"),
            "allow:claude:departures"
        );
        for (agent, datasource) in [
            ("claude", "departures"),
            ("a:b", "c:d"),
            ("100%", "%41"),
            ("old\nrow", "calendar"),
            ("Claude Code", "weather"),
            ("Kätzchen 名前", "weather"),
            ("", ""),
        ] {
            for prefix in [ALLOW_PREFIX, REVOKE_PREFIX] {
                let id = row_action_id(prefix, agent, datasource);
                assert_eq!(
                    parse_row_action_id(&id, prefix),
                    Some((agent.to_owned(), datasource.to_owned())),
                    "{id:?}"
                );
            }
            assert_eq!(
                parse_row_action_id(
                    &row_action_id(ALLOW_PREFIX, agent, datasource),
                    REVOKE_PREFIX
                ),
                None,
                "an Allow id is never read as a Revoke"
            );
        }
    }

    /// Anything that is not exactly an id this plugin renders dispatches
    /// nothing: the old index form, a missing or extra separator, a malformed
    /// escape, an escape that decodes to invalid UTF-8, the chip, junk.
    #[test]
    fn a_malformed_action_id_dispatches_nothing() {
        let (mut m, mut rx) = model();
        m.snapshot.pending = vec![pending("claude", "departures")];
        m.snapshot.grants = vec![grant("claude", "departures")];
        for node in [
            "allow:0",
            "revoke:0",
            "allow:",
            "allow:claude",
            "allow:claude:departures:x",
            "allow:cla ude:departures",
            "allow:%4:departures",
            "allow:%zz:departures",
            "allow:%C3:departures",
            "infobroker-panel",
        ] {
            m.on_event(node, &EventKind::Click);
            assert!(rx.try_recv().is_err(), "{node:?} must dispatch nothing");
        }
    }

    /// M3: a broker whose `grants.toml` could not be read says so on the
    /// panel, under the title and styled as a warning — beside the socket
    /// notice when both apply, not instead of it.
    #[test]
    fn a_broker_that_is_not_saving_grants_says_so_on_the_panel() {
        let (mut m, _rx) = model();
        let store = "Not saving grant changes: /x/grants.toml could not be read (…).";
        let socket = "Not serving: another info broker already owns /x.sock.";
        m.snapshot = BrokerSnapshot {
            notice: Some(socket.to_owned()),
            store_notice: Some(store.to_owned()),
            ..BrokerSnapshot::default()
        };
        let Node::Box { children, .. } = m.panel() else {
            panic!("panel root is a Box");
        };
        for (i, text) in [(1, socket), (2, store)] {
            assert!(
                matches!(&children[i], Node::Text { text: t, classes, .. }
                    if t == text && classes.iter().any(|c| c == "warning")),
                "child {i} must be the warning {text:?}"
            );
        }
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
        let View { tree, panel, .. } = m.view();
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

    /// #995: a broker that is **not serving** — a duplicate that stood down
    /// against a live incumbent — says so on the panel, directly under the
    /// title. Without this the duplicate painted a default snapshot: an empty
    /// panel that reads exactly like an idle broker, with the only explanation
    /// one stderr line in that unit's journal.
    #[test]
    fn a_broker_that_is_not_serving_explains_itself_on_the_panel() {
        let (mut m, _rx) = model();
        let notice = "Not serving: another info broker already owns /run/user/1000/x.sock.";

        // The ordinary case says nothing.
        assert!(
            !panel_mentions(&m.panel(), notice),
            "a serving broker shows no notice"
        );

        m.snapshot = BrokerSnapshot {
            notice: Some(notice.to_owned()),
            ..BrokerSnapshot::default()
        };
        let panel = m.panel();
        assert!(
            panel_mentions(&panel, notice),
            "a stood-down broker renders the reason it is empty"
        );
        let Node::Box { children, .. } = &panel else {
            panic!("panel root is a Box");
        };
        assert!(
            matches!(&children[1], Node::Text { text, classes, .. }
                if text == notice && classes.iter().any(|c| c == "warning")),
            "the notice rides directly under the title, styled as a warning",
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
        let Node::Row { children, .. } = grant_row(&g) else {
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
        // Revoke stays right-pinned after a spacer, its id naming the row's pair.
        assert!(matches!(&children[1], Node::Spacer));
        assert!(matches!(&children[2], Node::Button { id, .. } if id == "revoke:fnord:departures"));
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
