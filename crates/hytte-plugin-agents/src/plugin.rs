//! The Elm Architecture core: manifest / init / sources / update / view.
//!
//! No transport surface at all — which is what keeps every method here
//! unit-testable without a socket or a host. The transport is
//! [`hytte_plugin::run`]; the plugin's own I/O is [`crate::poll::poll_task`].

use std::collections::BTreeMap;

use hytte_plugin::proto::{Capability, Effect, EventKind, Manifest, Mount, Node, Page, StateKey};
use hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream;
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};
use tokio::sync::mpsc;

use crate::config::AgentsConfig;
use crate::hive::wire::{HiveUrls, Request, Scope};
use crate::hive::{AgentStatusRow, HiveError};
use crate::model::{Agent, AgentName, ExpandedGroups, Hive, Status, agent_url};
use crate::poll::{Cmd, Msg, poll_task};
use crate::view::{self, PanelContext, ids};

/// Stable plugin id — the host's mount-slot ownership key, the audit-log
/// subject, and the `programs.trollshell.plugins.<id>` config key.
///
/// **One const, on purpose** (open question 2 on #947 is still open: `agents`,
/// `hive` or `choom`). The crate name, the binary name and the unit name are
/// nix- and cargo-side and cannot be a Rust const, but nothing in the Rust
/// tree spells this id twice.
pub const PLUGIN_ID: &str = "agents";

/// The two edge-triggered flags §8's `Effect::Notify` watches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Alarms {
    failed: bool,
    needs_login: bool,
}

impl Alarms {
    fn of(row: &AgentStatusRow) -> Self {
        Self {
            failed: row.failed,
            needs_login: row.needs_login,
        }
    }
}

/// The plugin's whole state. Rebuilt on every (re)connect and re-derived from
/// the next poll — the design's per-session stance.
pub struct Agents {
    /// What the hive is doing.
    pub hive: Hive,
    /// The loaded `agents.toml`.
    pub cfg: AgentsConfig,
    /// The hive's `Urls`, once fetched.
    pub urls: Option<HiveUrls>,
    /// The agent whose detail the panel shows, if any.
    pub selected: Option<AgentName>,
    /// The agent whose details are unfolded **in the card**, if any.
    ///
    /// One at a time on purpose: the card is 320 px of a sidebar that also
    /// holds three other cards, and a roster where every row can be open is a
    /// roster with no rows visible. A second click on the same row, or a click
    /// on another row's disclosure, replaces it — the model is the single
    /// source of truth, exactly as it is for [`Agents::expanded`].
    pub opened: Option<AgentName>,
    /// Latest unix seconds off the host's clock subscription.
    pub now_unix: i64,
    /// When the last poll answered, in the clock's own unix seconds.
    pub last_poll_unix: Option<i64>,
    /// Which project groups the operator has explicitly opened or collapsed.
    /// Absent means the default (open unless the group is all-stopped).
    pub expanded: ExpandedGroups,
    /// The previous poll's alarm flags, per agent — the **edge** detector §8
    /// requires ("a hive with one wedged agent must not toast every 5 s").
    prev_alarms: BTreeMap<String, Alarms>,
    /// The next correlation token for a reply-bearing effect (#1060).
    ///
    /// **One counter, not one per effect kind**, which is the allocation
    /// contract `Input::EffectResult`'s own docs state: `RunCommand`,
    /// `OpenUri` and `RequestConsent`'s `request_id` share a single id space,
    /// so per-kind counters would collide the moment two effects were in
    /// flight at once. P1 emits only `OpenUri`, and this is still the shared
    /// counter so P2's `choom` launch and P3's approvals inherit it rather
    /// than opening a second one.
    next_effect_id: u64,
    /// The command lane to [`poll_task`].
    cmd_tx: CmdSender<Cmd>,
}

impl Agents {
    /// Build a model with an explicit command sender — the shape
    /// [`hytte_plugin::cmd_channel`] documents for unit tests.
    #[must_use]
    pub fn with_cmds(cmd_tx: CmdSender<Cmd>) -> Self {
        Self {
            hive: Hive::Connecting,
            cfg: AgentsConfig::default(),
            urls: None,
            selected: None,
            opened: None,
            expanded: ExpandedGroups::new(),
            now_unix: 0,
            last_poll_unix: None,
            prev_alarms: BTreeMap::new(),
            next_effect_id: 0,
            cmd_tx,
        }
    }

    /// Take the next correlation token. See [`Agents::next_effect_id`].
    ///
    /// `wrapping_add` rather than `+= 1`: the counter is a *token*, not a
    /// count, so the only thing that matters is that two effects in flight at
    /// once cannot share a value — and an overflow panic in a release build
    /// over a click counter would be a worse outcome than the wrap nobody will
    /// reach (2^64 clicks).
    fn take_effect_id(&mut self) -> u64 {
        let id = self.next_effect_id;
        self.next_effect_id = self.next_effect_id.wrapping_add(1);
        id
    }

    /// Whether the group headed `project` currently draws expanded — the
    /// operator's explicit choice if there is one, else the default (open
    /// unless every agent in it is stopped).
    fn group_is_open(&self, project: &str) -> bool {
        if let Some(explicit) = self.expanded.get(project) {
            return *explicit;
        }
        crate::model::group(self.hive.agents(), &self.cfg)
            .iter()
            .find(|g| g.header() == project)
            .is_none_or(|g| g.agents.iter().any(|a| a.status() != Status::Stopped))
    }

    /// The panel's non-model inputs.
    ///
    /// This is where [`Node::Scrolled`](hytte_plugin::proto::Node::Scrolled)'s
    /// negotiation is resolved, once, for the whole view: the variant is
    /// vocabulary-gated (#966), so a host that never advertised it gets `0` —
    /// the pre-#969 unbounded panel — and everything downstream just reads a
    /// number. `view.rs` then builds the node unnegotiated, which is both
    /// correct (the branch already happened) and testable (the SDK's own
    /// `build()` consults a process-global no test can set).
    fn panel_ctx(&self) -> PanelContext<'_> {
        PanelContext {
            now_unix: self.now_unix,
            last_poll_unix: self.last_poll_unix,
            socket: &self.cfg.socket,
            urls: self.urls.as_ref(),
            viewport_px: if hytte_plugin::nodes::host_speaks_scrolled() {
                view::PANEL_VIEWPORT_PX
            } else {
                0
            },
        }
    }

    /// Fold one poll answer, returning the toasts its **edges** earned.
    fn fold_status(&mut self, result: Result<Vec<AgentStatusRow>, HiveError>) -> Vec<Effect> {
        match result {
            Err(HiveError::Version(mismatch)) => {
                self.hive = Hive::Incompatible(mismatch);
                Vec::new()
            }
            // A hive that answered — with `ok: false`, or with a line this
            // build cannot parse — is up, and saying so as "unreachable"
            // sends the operator to `systemctl` for a problem that is not
            // there.
            Err(e @ (HiveError::Refused { .. } | HiveError::Protocol { .. })) => {
                self.hive = Hive::Error {
                    reason: e.to_string(),
                };
                Vec::new()
            }
            Err(e) => {
                self.hive = Hive::Unreachable {
                    reason: e.to_string(),
                };
                Vec::new()
            }
            Ok(rows) => {
                let mut effects = Vec::new();
                let mut agents = Vec::with_capacity(rows.len());
                let mut alarms = BTreeMap::new();
                for row in rows {
                    // §11 rule two: a name that fails the whitelist never
                    // reaches a node id or a request frame. Dropping the row is
                    // the safe direction — the hive's own `Ident` is stricter
                    // than this whitelist, so a rejection means something is
                    // wrong upstream, not that a legitimate agent is hidden.
                    let Some(name) = AgentName::parse(&row.name) else {
                        tracing::warn!(name = %row.name, "agent name failed the whitelist; row dropped");
                        continue;
                    };
                    let now = Alarms::of(&row);
                    // Edge, never level: a first sighting is not a transition.
                    // A plugin restart therefore stays quiet about an already
                    // wedged agent — the row says so, and toasting the whole
                    // roster on every reconnect would be the louder bug.
                    if let Some(prev) = self.prev_alarms.get(row.name.as_str()) {
                        let label = self.cfg.label_for(&row.name).to_owned();
                        if !prev.failed && now.failed {
                            effects.push(Effect::Notify {
                                summary: format!("{label} failed"),
                                body:
                                    "the agent's container unit gave up after its bounded restarts"
                                        .to_owned(),
                            });
                        }
                        if !prev.needs_login && now.needs_login {
                            effects.push(Effect::Notify {
                                summary: format!("{label} needs login"),
                                body: "the agent has no live claude session and is parked on the re-auth flow".to_owned(),
                            });
                        }
                    }
                    alarms.insert(row.name.clone(), now);
                    agents.push(Agent {
                        name,
                        row,
                        // The optimistic flip lasts exactly until the next poll
                        // answers, whatever it says (spec §6.3, "reconciled by
                        // the next poll"). Keeping it any longer would let a
                        // refused write leave a lie on screen forever.
                        pending_paused: None,
                    });
                }
                self.prev_alarms = alarms;
                if self.now_unix > 0 {
                    self.last_poll_unix = Some(self.now_unix);
                }
                self.hive = Hive::Up { agents };
                // A selection whose agent vanished falls back to the overview.
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|n| self.hive.agent(n).is_none())
                {
                    self.selected = None;
                }
                // …and so does an unfolded row: the card would otherwise hold a
                // name that no longer has a row to unfold under.
                if self
                    .opened
                    .as_ref()
                    .is_some_and(|n| self.hive.agent(n).is_none())
                {
                    self.opened = None;
                }
                effects
            }
        }
    }

    /// The pause toggle: flip optimistically, then ask the hive.
    fn toggle_pause(&mut self, name: &AgentName) {
        let Some(agent) = self.hive.agent(name) else {
            return;
        };
        let want = !agent.paused();
        let req = Request::SetPaused {
            name: name.as_str().to_owned(),
            paused: want,
        };
        if let Some(agent) = self.hive.agent_mut(name) {
            agent.pending_paused = Some(want);
        }
        let _ = self.cmd_tx.send(Cmd::Send(req));
    }

    /// How a refused frame reads in a toast: the verb plus the agent's
    /// **display label**, so it matches the row the operator just clicked.
    ///
    /// Phrased here rather than in the poll task for the same reason the
    /// alarm toasts are: `update` is where this plugin's human-facing strings
    /// live, and it is the only place that holds the config the labels come
    /// from.
    fn describe(&self, request: &Request) -> String {
        match request {
            Request::SetPaused { name, paused } => {
                let verb = if *paused { "pause" } else { "resume" };
                format!("{verb} {}", self.cfg.label_for(name))
            }
            Request::Start { scope } => {
                format!("start {}", self.scope_label(scope))
            }
            Request::Stop { scope, .. } => {
                format!("stop {}", self.scope_label(scope))
            }
            Request::Restart { name } => format!("restart {}", self.cfg.label_for(name)),
            // The read verbs never travel as a `Cmd::Send`, so this arm is
            // unreachable in practice; a generic phrasing beats a panic.
            Request::List
            | Request::AgentStatus
            | Request::SubscribeAgentStatus
            | Request::Urls => "the request".to_owned(),
        }
    }

    /// A scope names exactly one agent by construction (spec §11 rule one),
    /// so this reads the first — and says so rather than indexing blindly.
    fn scope_label(&self, scope: &Scope) -> String {
        scope.agent_names().first().map_or_else(
            || "the hive".to_owned(),
            |n| self.cfg.label_for(n).to_owned(),
        )
    }

    /// Select an agent and open this plugin's own panel.
    fn open_detail(&mut self, name: AgentName) -> Vec<Effect> {
        self.selected = Some(name);
        vec![Effect::OpenPage(Page::PluginSelf)]
    }

    /// Follow one URL through the desktop's default handler (#1045).
    ///
    /// The plugin names a **destination** and never a program — that is the
    /// whole difference between this and the `RunCommand` route, and the
    /// reason the manifest can stay three narrow capabilities wide. The host
    /// judges the scheme (`http`/`https`/`file` only) and answers on
    /// [`Input::EffectResult`].
    fn open_uri(&mut self, url: String) -> Vec<Effect> {
        vec![Effect::open_uri(self.take_effect_id(), url)]
    }

    /// The `agent page` link: **the hive's URL for this agent, as the model
    /// holds it now** — never the string the node id carried.
    ///
    /// Every other arm of [`Agents::click`] re-parses its name for the same
    /// reason ("the id came back over a socket"), and a URL is the one payload
    /// where trusting that round trip would matter: it is what the desktop is
    /// then asked to open. An agent that vanished between the render and the
    /// click, or a hive whose domain is unconfigured, opens nothing — the same
    /// silence the row's own `if let Some(url)` already guarantees, since
    /// neither renders the link in the first place.
    fn open_agent_page(&mut self, name: &AgentName) -> Vec<Effect> {
        let Some(url) = self.hive.agent(name).and_then(agent_url).map(str::to_owned) else {
            return Vec::new();
        };
        self.open_uri(url)
    }

    /// The panel's `dashboard` link — the hive's own root, from the `Urls`
    /// answer rather than from the clicked id, for [`Agents::open_agent_page`]'s
    /// reason.
    fn open_dashboard(&mut self) -> Vec<Effect> {
        let Some(home) = self
            .urls
            .as_ref()
            .and_then(|u| u.home.as_deref())
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_owned)
        else {
            return Vec::new();
        };
        self.open_uri(home)
    }

    /// Route one click. Split out of [`Plugin::update`] so the id-parsing
    /// contract is one readable table.
    fn click(&mut self, node: &str) -> Vec<Effect> {
        // Each arm re-validates the name rather than trusting the round trip
        // through the host: the id came back over a socket.
        if let Some(rest) = node.strip_prefix(ids::PAUSE) {
            if let Some(name) = AgentName::parse(rest) {
                self.toggle_pause(&name);
            }
            return Vec::new();
        }
        if let Some(rest) = node.strip_prefix(ids::CHAT) {
            // P1's primary click opens the detail panel. P2 replaces this with
            // a detached `RunCommand` launching the chat companion (spec §7),
            // which needs #953; what must NOT change either way is that it
            // never touches `SetPaused` — the chat surface is designed to be
            // used while the loop runs.
            if let Some(name) = AgentName::parse(rest) {
                return self.open_detail(name);
            }
            return Vec::new();
        }
        if let Some(rest) = node.strip_prefix(ids::DETAILS) {
            // Unfolds **in the card**, where the click happened — the one
            // thing @kaesaecracker's second round asked for by name ("its very
            // weird the panel opens in the top right after clicking bottom
            // left"). Read-only either way; real editing waits for #952
            // (spec §6.3).
            if let Some(name) = AgentName::parse(rest) {
                self.opened = if self.opened.as_ref() == Some(&name) {
                    None
                } else {
                    Some(name)
                };
            }
            return Vec::new();
        }
        if let Some(rest) = node.strip_prefix(ids::OPEN) {
            // The row Mara's 2026-09-10 retest could read but not follow
            // (#1045). One `OpenUri`, the URL taken from the model.
            if let Some(name) = AgentName::parse(rest) {
                return self.open_agent_page(&name);
            }
            return Vec::new();
        }
        if node == ids::OPEN_DASHBOARD {
            return self.open_dashboard();
        }
        if node == view::OVERVIEW_ID {
            // The card's title row is the one place that jumps to the drawer,
            // and it jumps to the hive overview rather than to an agent.
            self.selected = None;
            return vec![Effect::OpenPage(Page::PluginSelf)];
        }
        if let Some(project) = node.strip_prefix(ids::GROUP) {
            // The expander is plugin-driven: the host never self-toggles, so
            // the model is the single source of truth for what is open
            // (`crates/hytte-plugin-proto/src/wire.rs:365-405`).
            let open = self.group_is_open(project);
            self.expanded.insert(project.to_owned(), !open);
            return Vec::new();
        }
        if let Some(rest) = node.strip_prefix(ids::START) {
            if let Some(name) = AgentName::parse(rest) {
                let _ = self.cmd_tx.send(Cmd::Send(Request::Start {
                    scope: Scope::agent(name.as_str()),
                }));
            }
            return Vec::new();
        }
        if let Some(rest) = node.strip_prefix(ids::STOP) {
            if let Some(name) = AgentName::parse(rest) {
                let _ = self.cmd_tx.send(Cmd::Send(Request::Stop {
                    scope: Scope::agent(name.as_str()),
                    graceful: true,
                }));
            }
            return Vec::new();
        }
        if node == view::BACK_ID {
            self.selected = None;
        }
        Vec::new()
    }
}

impl Plugin for Agents {
    type Msg = Msg;
    type Cmd = Cmd;

    /// Mounts [`Mount::SidebarTop`] as a card (spec §6.1). Subscribes
    /// [`StateKey::Clock`] for the panel's relative ages and
    /// [`StateKey::SlotVisible`] so the host actually pushes the visibility
    /// edges that park the poll (#305: the push is opt-in via the manifest,
    /// so a poller MUST subscribe to keep being gated).
    ///
    /// Three capabilities, and only three: `OpenPage` (open its own panel),
    /// `Notify` (§8's edge toast) and `OpenUri` (#1045 — follow an agent's own
    /// page link). **Not** `RunCommand` — the `choom` argv is phase P2 — and
    /// **not** `Consent`: approvals are phase P3 precisely because the row must
    /// be trustworthy before it is allowed to raise a modal that approves a
    /// config change (spec §13). The plugin declares no secret slot; its entire
    /// authority is the desktop user's `hive-admin` group membership (spec §11
    /// rule four).
    ///
    /// # `OpenUri` is declared because the effect is emitted, not for symmetry
    ///
    /// Declaring it is **load-bearing**, not documentation: since #1058 the SDK
    /// itself drops an effect whose gating capability the manifest omits
    /// (`hytte-plugin/src/runtime.rs`'s `drop_ungranted_effects`, over proto's
    /// one `Effect::required_capability` table), so omitting it here would make
    /// every link click a silent no-op on a current host and the #437
    /// crash-loop on a host older than #1045. `the_manifest_grants_what_the_link_click_emits`
    /// asserts that pairing through that same table rather than restating the
    /// list.
    ///
    /// It is also the narrowest thing that opens a link: `OpenUri` names a
    /// *destination* and the host resolves it through the desktop's default
    /// handler, where `RunCommand` — the other way to reach `xdg-open` — is
    /// arbitrary argv as the user and the highest-trust capability in the
    /// vocabulary (#1045's own argument).
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::SidebarTop);
        m.subscribes = vec![StateKey::Clock, StateKey::SlotVisible];
        m.capabilities = vec![
            Capability::OpenPage,
            Capability::Notify,
            Capability::OpenUri,
        ];
        m
    }

    fn init(cmds: CmdSender<Self::Cmd>) -> Self {
        Self::with_cmds(cmds)
    }

    fn sources(cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        tokio::spawn(poll_task(cmds, msg_tx));
        Some(Box::pin(UnboundedReceiverStream::new(msg_rx)))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            Input::Snapshot(snapshot) => {
                if let Some(clock) = snapshot.clock {
                    self.now_unix = clock.unix;
                    // The first clock after a poll stamps it, so "last poll"
                    // is never stuck on "never" just because the snapshot
                    // arrived second.
                    if self.last_poll_unix.is_none() && matches!(self.hive, Hive::Up { .. }) {
                        self.last_poll_unix = Some(clock.unix);
                    }
                }
                Vec::new()
            }
            Input::SlotVisible(visible) => {
                let _ = self.cmd_tx.send(Cmd::SetVisible(visible));
                Vec::new()
            }
            Input::App(Msg::Config(cfg)) => {
                self.cfg = *cfg;
                Vec::new()
            }
            Input::App(Msg::Urls(urls)) => {
                self.urls = Some(*urls);
                Vec::new()
            }
            // Exactly one toast per refusal. The row un-sticks on the next
            // poll regardless; this is what says why it did.
            Input::App(Msg::WriteRefused { request, reason }) => {
                vec![Effect::Notify {
                    summary: format!("hive refused: {}", self.describe(&request)),
                    body: reason,
                }]
            }
            Input::App(Msg::Status(result)) => self.fold_status(result),
            // A link the desktop would not open (#1045). The only reply-bearing
            // effect P1 emits is `OpenUri`, so no correlation table is needed —
            // and the host's own reason is already the sentence worth showing,
            // which is why `EffectOutcome::output` exists ("so a plugin can
            // toast it instead of leaving a click that silently does nothing",
            // `hytte-plugin/src/lib.rs`). A success says nothing: the browser
            // appearing IS the feedback.
            Input::EffectResult { outcome, .. } if !outcome.ok => {
                vec![Effect::Notify {
                    summary: "couldn't open the link".to_owned(),
                    body: outcome
                        .output
                        .unwrap_or_else(|| "the desktop refused it".to_owned()),
                }]
            }
            // `..` is mandatory since #1083 made `Input::Event`
            // `#[non_exhaustive]` and gave it an `output`. The card is
            // per-monitor decoration with no output-dependent behaviour, so
            // which output the click came from is deliberately ignored here
            // rather than threaded into the model.
            Input::Event {
                node,
                kind: EventKind::Click,
                ..
            } => self.click(&node),
            // Additive `Input` variants — this plugin issues no `RunCommand`,
            // no datasource query, and declares neither `Consent` nor the
            // domain-state caps, so none of the remaining kinds can reach it.
            // A wildcard keeps a new variant from breaking the build.
            _ => Vec::new(),
        }
    }

    fn view(&self) -> View {
        View::new(card_of(self)).panel(view::panel(
            &self.hive,
            &self.cfg,
            self.selected.as_ref(),
            self.panel_ctx(),
        ))
    }
}

/// The root node of the rendered card — exposed for the golden tests, which
/// assert on the tree rather than on a screenshot.
#[must_use]
pub fn card_of(model: &Agents) -> Node {
    view::card(
        &model.hive,
        &model.cfg,
        &model.expanded,
        model.opened.as_ref(),
    )
}
