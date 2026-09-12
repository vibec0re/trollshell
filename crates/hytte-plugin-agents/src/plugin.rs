//! The Elm Architecture core: manifest / init / sources / update / view.
//!
//! No transport surface at all — which is what keeps every method here
//! unit-testable without a socket or a host. The transport is
//! [`hytte_plugin::run`]; the plugin's own I/O is [`crate::poll::poll_task`].

use std::collections::{BTreeMap, BTreeSet};

use hytte_plugin::proto::{
    Capability, ConsentChoices, ConsentDecision, Effect, EventKind, Manifest, Mount, Node, Page,
    StateKey,
};
use hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream;
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};
use tokio::sync::mpsc;

use crate::config::AgentsConfig;
use crate::hive::wire::{Approval, HiveUrls, Request, Scope};
use crate::hive::{AgentStatusRow, HiveError};
use crate::model::{Agent, AgentName, ExpandedGroups, Hive, PendingApprovals, Status, agent_url};
use crate::poll::{Cmd, Msg, poll_task};
use crate::view::{self, PanelContext, ids};
use crate::window;

/// Stable plugin id — the host's mount-slot ownership key, the audit-log
/// subject, and the `programs.trollshell.plugins.<id>` config key.
///
/// **One const, on purpose** (open question 2 on #947 is still open: `agents`,
/// `hive` or `choom`). The crate name, the binary name and the unit name are
/// nix- and cargo-side and cannot be a Rust const, but nothing in the Rust
/// tree spells this id twice.
pub const PLUGIN_ID: &str = "agents";

/// How much of an approval's free-text description reaches the prompt's detail
/// line.
///
/// The manager wrote it, so it is neither trusted to be short nor wrong to
/// show: the consent card is a 480 px surface with a wrapping label, and an
/// unbounded description would push the buttons off it. Same reason the drawer
/// bounds every free-text value it renders (#963's review).
const DETAIL_CHARS: usize = 240;

/// One consent prompt this plugin has raised and not yet heard back on
/// (#947 P3).
///
/// **At most one**, because the host has at most one consent window: a second
/// `RequestConsent` supersedes the first, and a superseded card resolves to
/// nothing at all (`overlays/consent.rs`'s supersede path). Modelling it as an
/// `Option` rather than a map is therefore not a simplification, it is the
/// truth — a map keyed on `request_id` would imply a queue of live cards that
/// cannot exist, and would let this plugin raise prompts that silently replace
/// each other every poll.
///
/// It doubles as the **gate**: while one is in flight no second approval is
/// raised, so a burst of five approvals is answered one at a time instead of
/// five cards superseding each other inside one 60 s window. It opens again
/// when a decision arrives, when the approval leaves the queue (answered on the
/// dashboard), or when a badge click deliberately re-raises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RaisedPrompt {
    /// The correlation token, from [`Agents::take_effect_id`].
    request_id: u64,
    /// The approval it is asking about.
    approval: i64,
}

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
    /// The hive's approval queue as of the last `Pending` answer (#947 P3) —
    /// what the badges count and what a decision is validated against.
    pub pending: PendingApprovals,
    /// Approval ids a prompt has already been raised for.
    ///
    /// The dedup set spec §6.5 needs: an approval prompts **once**, and only
    /// leaving the queue lets it prompt again (it is pruned to the live queue
    /// on every fold). Without it a hive with one unanswered approval would
    /// raise a modal on every poll — every two seconds, by default.
    ///
    /// Deliberately **not** persisted anywhere: it is per-session state, so a
    /// plugin restart re-raises what is still waiting, which is the right side
    /// to err on for a queue whose whole point is that a human has not answered
    /// it yet.
    prompted: BTreeSet<i64>,
    /// The one consent prompt in flight — see [`RaisedPrompt`].
    prompt: Option<RaisedPrompt>,
    /// The previous poll's alarm flags, per agent — the **edge** detector §8
    /// requires ("a hive with one wedged agent must not toast every 5 s").
    prev_alarms: BTreeMap<String, Alarms>,
    /// The next correlation token for a reply-bearing effect (#1060).
    ///
    /// **One counter, not one per effect kind**, which is the allocation
    /// contract `Input::EffectResult`'s own docs state: `RunCommand`,
    /// `OpenUri` and `RequestConsent`'s `request_id` share a single id space,
    /// so per-kind counters would collide the moment two effects were in
    /// flight at once. P1 emitted only `OpenUri`; since #950 the companion
    /// window's detached `RunCommand` allocates from this same counter, and
    /// since #947 P3 so does every `RequestConsent` `request_id` — inherited
    /// rather than a second counter, exactly as that contract says.
    next_effect_id: u64,
    /// The command lane to [`poll_task`].
    cmd_tx: CmdSender<Cmd>,
    /// Whether the companion window (#950) can be launched — resolved once,
    /// see [`window::Probe`].
    window: window::Probe,
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
            pending: PendingApprovals::default(),
            prompted: BTreeSet::new(),
            prompt: None,
            now_unix: 0,
            last_poll_unix: None,
            prev_alarms: BTreeMap::new(),
            next_effect_id: 0,
            cmd_tx,
            window: window::Probe::path(),
        }
    }

    /// Pin whether the companion window is installed, instead of resolving it
    /// against this process's `PATH` (#950).
    ///
    /// The **test seam**, and the reason it exists rather than each test
    /// inheriting the machine: which of the two routes a click takes is now a
    /// property of the desktop, so a test that does not say which desktop it
    /// describes would pass or fail depending on whether the reviewer happens
    /// to have the window installed.
    pub fn set_window_probe(&mut self, probe: window::Probe) {
        self.window = probe;
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
                effects
            }
        }
    }

    /// Fold one `Pending` answer and raise a prompt if one is due (#947 P3,
    /// spec §6.5).
    ///
    /// Order matters, and each step is one of the rules:
    ///
    /// 1. **Prune the dedup set to the live queue.** An approval that left
    ///    `Pending` — answered here, on the dashboard, or by `hivectl` — stops
    ///    being remembered as prompted, so its id is free again. (Hive ids are
    ///    not reused, so this is hygiene rather than correctness; the
    ///    correctness it buys is that the set cannot grow without bound over a
    ///    long session.)
    /// 2. **Drop an in-flight prompt whose approval is gone.** §6.5: "an
    ///    approval that disappears from `Pending` between the prompt and the
    ///    answer … is dropped with a debug line, not an error". Dropping it
    ///    also reopens the gate.
    /// 3. **Raise the next one**, if nothing is in flight.
    fn fold_pending(&mut self, queue: Vec<Approval>) -> Vec<Effect> {
        let pending = PendingApprovals::new(queue);
        self.prompted.retain(|id| pending.contains(*id));
        if let Some(raised) = self.prompt
            && !pending.contains(raised.approval)
        {
            tracing::debug!(
                approval = raised.approval,
                "the approval left the queue before its prompt was answered; dropping the prompt"
            );
            self.prompt = None;
        }
        self.pending = pending;
        self.raise_next()
    }

    /// Raise a prompt for the oldest approval nobody has been asked about yet,
    /// if the one-card gate is open.
    fn raise_next(&mut self) -> Vec<Effect> {
        if self.prompt.is_some() {
            return Vec::new();
        }
        let Some(next) = self
            .pending
            .oldest_unprompted(&self.prompted)
            .map(Approval::clone)
        else {
            return Vec::new();
        };
        self.raise(&next)
    }

    /// Raise the consent prompt for one approval.
    ///
    /// Every human string is computed **here**, which is what keeps
    /// `RequestConsent` free of hive domain (the effect's own contract): the
    /// host renders *"⟨agent⟩ wants: ⟨kind⟩"* plus the detail line and learns
    /// nothing about approval queues.
    ///
    /// `datasource` is deliberately empty — an approval is about a queued
    /// item, not a data source, and the overlay drops the clause rather than
    /// inventing one (`overlays/consent.rs`'s `ask_line`).
    ///
    /// The `request_id` comes from the **shared** effect counter, not a second
    /// one: `Input::EffectResult`'s allocation contract makes `RunCommand`,
    /// `OpenUri` and `RequestConsent` one id space, and this plugin emits all
    /// three.
    fn raise(&mut self, approval: &Approval) -> Vec<Effect> {
        let request_id = self.take_effect_id();
        self.prompted.insert(approval.id);
        self.prompt = Some(RaisedPrompt {
            request_id,
            approval: approval.id,
        });
        vec![Effect::RequestConsent {
            request_id,
            agent: self.cfg.label_for(&approval.agent).to_owned(),
            datasource: String::new(),
            scope: approval.kind.human(),
            detail: detail_line(approval),
            choices: ConsentChoices::Approval,
        }]
    }

    /// The badge click: re-raise the prompt for the oldest approval this agent
    /// is waiting on (spec §6.5 / #947's default 2).
    ///
    /// This is the recovery path for a prompt that timed out or was dismissed —
    /// the operator's way back to a card that answered nothing — so it
    /// **replaces** whatever is in flight rather than deferring to it: the host
    /// has one consent window and a new `RequestConsent` supersedes what is in
    /// it, so pretending otherwise would leave the model describing a card that
    /// no longer exists.
    fn raise_for(&mut self, name: &AgentName) -> Vec<Effect> {
        let Some(next) = self.pending.oldest_for(name.as_str()).map(Approval::clone) else {
            // The badge and the queue disagree — the click raced a poll. The
            // next render simply has no badge.
            return Vec::new();
        };
        self.prompt = None;
        self.raise(&next)
    }

    /// Route the human's answer to one approval prompt (#947 P3, spec §6.5).
    ///
    /// Three guards, each one a way this could go wrong:
    ///
    /// - **The token must be the live one.** A decision for a `request_id` this
    ///   model is not waiting on is a superseded card's late answer; it is
    ///   dropped, never applied to whatever is in flight now.
    /// - **The prompt is consumed before the frame is built**, so a duplicate
    ///   `ConsentDecision` on the same token sends nothing. `Approve` runs the
    ///   action immediately on the far side; sending it twice is not idempotent
    ///   the way `SetPaused` is.
    /// - **The approval must still be queued.** One that was answered elsewhere
    ///   in the meantime is dropped with a debug line (§6.5), not re-decided.
    ///
    /// The mapping is §6.5's, and it is deliberately lossy: **every** `Allow*`
    /// becomes one `Approve { id }` for that one id, and no standing grant is
    /// persisted — which this plugin achieves by having nowhere to persist one.
    /// The two-button card only ever sends `AllowOnce`, but a host older than
    /// #947 draws four buttons for the same effect, so the other two arms are
    /// the reason that host degrades to "approved once" rather than to nothing.
    fn decide(&mut self, request_id: u64, decision: ConsentDecision) -> Vec<Effect> {
        let Some(raised) = self.prompt.filter(|p| p.request_id == request_id) else {
            tracing::debug!(
                request_id,
                "a consent decision for a prompt this session is not waiting on; ignored"
            );
            return Vec::new();
        };
        self.prompt = None;

        let id = raised.approval;
        if !self.pending.contains(id) {
            tracing::debug!(
                approval = id,
                "the approval was resolved elsewhere before the answer arrived; dropping it"
            );
            return Vec::new();
        }
        let request = match decision {
            ConsentDecision::Deny => Request::Deny { id },
            ConsentDecision::AllowOnce
            | ConsentDecision::AllowSession
            | ConsentDecision::AllowAlways => Request::Approve { id },
        };
        let _ = self.cmd_tx.send(Cmd::Send(request));
        Vec::new()
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
            // #947 P3. The id alone is a queue rowid nobody recognises, so the
            // toast names the agent and the action the way the prompt just did
            // — the operator has to be able to tell *which* click failed.
            Request::Approve { id } => format!("approve {}", self.approval_label(*id)),
            Request::Deny { id } => format!("deny {}", self.approval_label(*id)),
            // The read verbs never travel as a `Cmd::Send`, so this arm is
            // unreachable in practice; a generic phrasing beats a panic.
            Request::List
            | Request::AgentStatus
            | Request::SubscribeAgentStatus
            | Request::Pending
            | Request::Urls => "the request".to_owned(),
        }
    }

    /// How one approval reads inside a refusal toast.
    ///
    /// Falls back to the bare id when the queue no longer holds it: a refusal
    /// and a poll can land in either order, and "request #7" is still something
    /// the operator can look up on the dashboard.
    fn approval_label(&self, id: i64) -> String {
        self.pending.all().iter().find(|a| a.id == id).map_or_else(
            || format!("request #{id}"),
            |a| {
                format!(
                    "\"{}\" for {} (#{id})",
                    a.kind.human(),
                    self.cfg.label_for(&a.agent)
                )
            },
        )
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
    /// Open the agent's **companion window** (#950), if this desktop has one.
    ///
    /// `None` means "not this route" — either the window is not installed
    /// ([`window::Probe`], which also warns once) or the model no longer holds
    /// the agent whose row was clicked. Both callers then take their P1 route,
    /// so a desktop without the window keeps working exactly as it did.
    ///
    /// The launch carries **only the agent's name**: the window reads
    /// `host.sock` itself, so nothing the model holds — not the URL, not the
    /// status — has to survive the trip, and the window is correct even if the
    /// roster moved between the render and the click. That is why this needs
    /// no correlation table beyond the shared [`Agents::take_effect_id`]
    /// counter.
    fn open_window(&mut self, name: &AgentName, tab: window::Tab) -> Option<Vec<Effect>> {
        if !self.window.available() || self.hive.agent(name).is_none() {
            return None;
        }
        Some(vec![Effect::launch(
            self.take_effect_id(),
            window::argv(name.as_str(), tab),
        )])
    }

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
        if let Some(rest) = node.strip_prefix(ids::EDIT) {
            // Annika's `[optionsedit]` (2026-09-11). Its destination is the
            // agent's companion window **on its settings tab** — her call on
            // #947 at 07:43Z, so an agent has one surface. This plugin's own
            // drawer page was the placeholder for that window and is now its
            // fallback: a desktop without `trollshell-agent-window` still gets
            // the P1 behaviour rather than a dead button. Still read-only
            // either way until #952.
            if let Some(name) = AgentName::parse(rest) {
                if let Some(fx) = self.open_window(&name, window::Tab::Settings) {
                    return fx;
                }
                return self.open_detail(name);
            }
            return Vec::new();
        }
        if let Some(rest) = node.strip_prefix(ids::OPEN) {
            // The row Mara's 2026-09-10 retest could read but not follow
            // (#1045). Since #950 it opens the agent's companion window — our
            // chrome around hyperhive's own page — and falls back to the P1
            // route, one `OpenUri` with the URL taken from the model, when the
            // window is not installed. Annika on #947: a dedicated webview we
            // control, "not the browser"; the browser stays the fallback.
            if let Some(name) = AgentName::parse(rest) {
                if let Some(fx) = self.open_window(&name, window::Tab::Agent) {
                    return fx;
                }
                return self.open_agent_page(&name);
            }
            return Vec::new();
        }
        if let Some(rest) = node.strip_prefix(ids::APPROVALS) {
            // #947 P3. The badge re-raises the prompt for the **oldest**
            // approval this agent is waiting on *now* — re-read from the model,
            // never from the clicked id, for `ids::OPEN`'s reason.
            if let Some(name) = AgentName::parse(rest) {
                return self.raise_for(&name);
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
    /// Five capabilities: `OpenPage` (open its own panel), `Notify` (§8's edge
    /// toast), `OpenUri` (#1045 — follow an agent's own page link), `RunCommand`
    /// since #950 (the detached launch of the agent's **companion window**,
    /// [`window`]) and, since #947 P3, `Consent` — the approval prompt
    /// (spec §6.5). `Consent` was deliberately withheld through P1 and P2, so
    /// the row was trustworthy before it was allowed to raise a modal that
    /// approves a config change (spec §13); it is also the #305 opt-in that
    /// makes the host push `HostMsg::ConsentDecision` to this connection at
    /// all, so declaring it is load-bearing twice over — without it the SDK
    /// drops the effect (#1058) *and* the answer would never arrive. The plugin
    /// declares no secret slot; its entire authority is the desktop user's
    /// `hive-admin` group membership (spec §11 rule four).
    ///
    /// # `RunCommand` is the highest-trust capability in the vocabulary, and
    /// that is the price of the window
    ///
    /// It is arbitrary argv as the user, and one capability covers **both**
    /// spawn modes by the proto's own decision ("a plugin that may run an
    /// arbitrary `argv` at all can already launch a detacher of its own").
    /// #1045 chose `OpenUri` over it for the *link* precisely because a link
    /// needs only a destination. A window is not a destination: it is our own
    /// binary, with our chrome around hyperhive's page, which is the whole
    /// point Annika made on #947 ("we control it … additional buttons, status,
    /// agent settings"). So the capability is narrowed by **what this plugin
    /// spells** instead — [`window::argv`] is the only place an argv is built,
    /// it names one constant binary, and the only variable in it is an
    /// [`AgentName`] that was re-parsed after its trip through the host.
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
            Capability::RunCommand,
            Capability::Consent,
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
            // #947 P3: the approval queue, folded on the same tick as the
            // roster it badges.
            Input::App(Msg::Pending(queue)) => self.fold_pending(queue),
            // #947 P3: the human answered (or the two-button card would have
            // sent nothing at all, and this never arrives).
            Input::ConsentDecision {
                request_id,
                decision,
            } => self.decide(request_id, decision),
            // A link the desktop would not open (#1045), or a companion window
            // the host could not launch (#950). Two reply-bearing effects now,
            // and still **no correlation table**: both mean the same thing to
            // the operator ("the thing you clicked did not appear"), and the
            // host's own reason is already the sentence worth showing, which is
            // why `EffectOutcome::output` exists ("so a plugin can toast it
            // instead of leaving a click that silently does nothing",
            // `hytte-plugin/src/lib.rs`). A table would buy a better noun and
            // cost a map keyed on a counter that already has to be shared.
            //
            // A success says nothing: the window (or the browser) appearing IS
            // the feedback. Note the asymmetry the host documents — a detached
            // launch's `ok` reports only that the *launch* succeeded, never
            // that the program ran, which is exactly why the window is resolved
            // on `PATH` before it is launched (see [`window::Probe`]).
            Input::EffectResult { outcome, .. } if !outcome.ok => {
                vec![Effect::Notify {
                    summary: "couldn't open that".to_owned(),
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
            // Additive `Input` variants — this plugin issues no datasource
            // query and declares none of the domain-state caps, so none of the
            // remaining kinds can reach it. A wildcard keeps a new variant from
            // breaking the build.
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
    view::card(&model.hive, &model.cfg, &model.expanded, &model.pending)
}

/// The prompt's secondary line: the manager's own description, or a stand-in
/// naming the request.
///
/// The description is what the person who queued the approval wrote about it,
/// so it is the sentence worth showing — but it is free text from another
/// process, so it is bounded ([`DETAIL_CHARS`]) before it reaches a 480 px
/// card. An approval with none still gets a line, because "when was this asked"
/// is the next thing an operator wants and a blank detail would hide the
/// overlay's whole second row.
fn detail_line(approval: &Approval) -> String {
    let stamp = if approval.requested_at.is_empty() {
        format!("request #{}", approval.id)
    } else {
        format!("request #{}, asked {}", approval.id, approval.requested_at)
    };
    match approval
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        Some(description) => format!("{} — {stamp}", clamp(description, DETAIL_CHARS)),
        None => stamp,
    }
}

/// Truncate on a **char** boundary, appending an ellipsis when it bit.
///
/// `&s[..n]` would panic mid-codepoint, and a description is arbitrary UTF-8
/// somebody else wrote — a plugin that panicked on an emoji in a PR title would
/// take its whole session down.
fn clamp(s: &str, chars: usize) -> String {
    if s.chars().count() <= chars {
        return s.to_owned();
    }
    let head: String = s.chars().take(chars).collect();
    format!("{head}…")
}
