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
use crate::model::{Agent, AgentName, ExpandedGroups, Hive, Status};
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
            expanded: ExpandedGroups::new(),
            now_unix: 0,
            last_poll_unix: None,
            prev_alarms: BTreeMap::new(),
            cmd_tx,
        }
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
    fn panel_ctx(&self) -> PanelContext<'_> {
        PanelContext {
            now_unix: self.now_unix,
            last_poll_unix: self.last_poll_unix,
            socket: &self.cfg.socket,
            urls: self.urls.as_ref(),
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
            // Read-only detail; real editing waits for #952 (spec §6.3).
            if let Some(name) = AgentName::parse(rest) {
                return self.open_detail(name);
            }
            return Vec::new();
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
    /// Two capabilities, and only two: `OpenPage` (open its own panel) and
    /// `Notify` (§8's edge toast). **Not** `RunCommand` — the `choom` argv is
    /// phase P2 — and **not** `Consent`: approvals are phase P3 precisely
    /// because the row must be trustworthy before it is allowed to raise a
    /// modal that approves a config change (spec §13). The plugin declares no
    /// secret slot; its entire authority is the desktop user's `hive-admin`
    /// group membership (spec §11 rule four).
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::SidebarTop);
        m.subscribes = vec![StateKey::Clock, StateKey::SlotVisible];
        m.capabilities = vec![Capability::OpenPage, Capability::Notify];
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
            Input::Event {
                node,
                kind: EventKind::Click,
            } => self.click(&node),
            // Additive `Input` variants — this plugin issues no `RunCommand`,
            // no datasource query, and declares neither `Consent` nor the
            // domain-state caps, so none of the remaining kinds can reach it.
            // A wildcard keeps a new variant from breaking the build.
            _ => Vec::new(),
        }
    }

    fn view(&self) -> View {
        View::new(view::card(&self.hive, &self.cfg, &self.expanded)).panel(view::panel(
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
    view::card(&model.hive, &model.cfg, &model.expanded)
}
