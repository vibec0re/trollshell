//! The chrome's **models** — what the header, the buttons and the settings
//! page show, as plain data derived from [`AgentState`].
//!
//! Split from the widgets ([`crate::ui`]) for the reason the plugin splits its
//! reducer from its view: every rule about what the window says is then
//! testable without a display server, and the widget layer is a set of
//! `set_label` calls a single display test can pin.

use hytte_plugin_agents::config::AgentsConfig;
use hytte_plugin_agents::hive::wire::{Approval, HiveUrls, Request};
use hytte_plugin_agents::model::{
    AgentName, PendingApprovals, Status, agent_url, model_family, single_line,
};
use hytte_plugin_agents::plugin::Shown;

use crate::feed::AgentState;

/// What the window's header line shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderModel {
    /// The agent's display label (`agents.toml`'s `display.<name>.label`, else
    /// the name).
    pub title: String,
    /// The symbolic icon name, from the same config the card reads.
    pub icon: String,
    /// The **short** model word — `Opus`, never `opus-5.2-2026…`. Annika on
    /// #963: "keep Model name short — Opus is enough."
    pub model_word: Option<String>,
    /// The full model id, for the short word's hover.
    pub model_full: Option<String>,
    /// The status line — the harness's own text for a running agent, the
    /// state's word otherwise, the failure's sentence when there is one.
    pub status: String,
    /// The symbolic icon beside it.
    pub status_icon: String,
    /// Its style class.
    pub status_class: String,
}

/// The connecting state's text, before the first answer.
pub const CONNECTING: &str = "connecting to the hive…";
/// What a window shows when its agent is not on this hive's roster.
pub const UNKNOWN_AGENT: &str = "no agent by that name on this hive";

impl HeaderModel {
    /// Derive the header from the agent's state.
    #[must_use]
    pub fn of(name: &AgentName, cfg: &AgentsConfig, state: &AgentState) -> Self {
        let (status, status_icon, status_class) = match state {
            AgentState::Connecting => (
                CONNECTING.to_owned(),
                "content-loading-symbolic".to_owned(),
                "dim-label".to_owned(),
            ),
            AgentState::Unreachable { reason } => (
                reason.clone(),
                "dialog-error-symbolic".to_owned(),
                "error".to_owned(),
            ),
            AgentState::Unknown => (
                UNKNOWN_AGENT.to_owned(),
                "dialog-warning-symbolic".to_owned(),
                "warning".to_owned(),
            ),
            AgentState::Up(agent) => {
                let s = agent.status();
                (
                    agent.status_line().to_owned(),
                    s.icon().to_owned(),
                    s.class().to_owned(),
                )
            }
        };
        let model_full = state
            .agent()
            .and_then(|a| a.row.active_model.as_deref())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned);
        Self {
            title: cfg.label_for(name.as_str()).to_owned(),
            icon: cfg.icon_for(name.as_str()).to_owned(),
            model_word: model_full.as_deref().and_then(model_family),
            model_full,
            status,
            status_icon,
            status_class,
        }
    }
}

/// Which of the lifecycle buttons are live, and what the pause toggle shows.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per widget property, the way `AgentStatusRow` carries one per hive flag \
              (and with the same allow): these are three separate widgets' states plus the \
              gate, not a state machine — `can_start`/`can_stop` are exclusive but `paused` is \
              orthogonal to both, so an enum over them would have to enumerate the product"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Controls {
    /// `Start` is offered — the agent is stopped.
    pub can_start: bool,
    /// `Stop` is offered — the agent is not stopped.
    pub can_stop: bool,
    /// The pause toggle's position.
    pub paused: bool,
    /// Whether any of them may be pressed at all. False until the hive has
    /// said what this agent is: a verb sent into an unknown state is a verb
    /// sent on a guess.
    pub live: bool,
}

impl Controls {
    /// Derive the buttons from the agent's state.
    ///
    /// `Start` and `Stop` are **mutually exclusive**, keyed on the same
    /// `Status::Stopped` test the card's lifecycle button uses, so the two
    /// surfaces cannot disagree about which verb an agent is offered.
    #[must_use]
    pub fn of(state: &AgentState) -> Self {
        let Some(agent) = state.agent() else {
            return Self {
                can_start: false,
                can_stop: false,
                paused: false,
                live: false,
            };
        };
        let stopped = agent.status() == Status::Stopped;
        Self {
            can_start: stopped,
            can_stop: !stopped,
            paused: agent.paused(),
            live: true,
        }
    }
}

/// One read-only row on the settings page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    /// The row's title.
    pub label: &'static str,
    /// Its value, already rendered — [`Facts::ABSENT`] when the hive has none.
    pub value: String,
}

/// The settings page's content.
///
/// **v1 is read-only, deliberately.** Editing an agent belongs to phase P5 of
/// #947 and is mostly not the shell's to edit at all: each agent has its own
/// config flake, and only the host-level remainder is the hive's (spec §9,
/// three owners). So this page shows what `host.sock` already says, plus where
/// this window is reading it from, and the window's buttons are the only
/// writes it performs.
pub struct Facts;

impl Facts {
    /// What a row shows when the hive has no value for it.
    pub const ABSENT: &'static str = "—";

    /// The agent's own facts.
    #[must_use]
    pub fn agent(name: &AgentName, state: &AgentState) -> Vec<Fact> {
        let row = state.agent().map(|a| &a.row);
        let opt = |v: Option<&String>| {
            v.map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map_or_else(|| Self::ABSENT.to_owned(), str::to_owned)
        };
        vec![
            Fact {
                label: "Name",
                value: name.as_str().to_owned(),
            },
            Fact {
                label: "Model",
                value: opt(row.and_then(|r| r.active_model.as_ref())),
            },
            Fact {
                label: "Status",
                value: match state {
                    AgentState::Up(a) => a.status_line().to_owned(),
                    AgentState::Connecting => CONNECTING.to_owned(),
                    AgentState::Unknown => UNKNOWN_AGENT.to_owned(),
                    AgentState::Unreachable { reason } => reason.clone(),
                },
            },
            Fact {
                label: "Status set",
                value: opt(row.and_then(|r| r.status_set_at.as_ref())),
            },
            Fact {
                label: "Deployed",
                value: opt(row.and_then(|r| r.deployed_sha.as_ref())),
            },
            Fact {
                label: "Agent page",
                value: state
                    .agent()
                    .and_then(agent_url)
                    .map_or_else(|| Self::ABSENT.to_owned(), str::to_owned),
            },
        ]
    }

    /// Where this window is reading from.
    #[must_use]
    pub fn hive(cfg: &AgentsConfig, urls: Option<&HiveUrls>) -> Vec<Fact> {
        let opt = |v: Option<&String>| {
            v.map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map_or_else(|| Self::ABSENT.to_owned(), str::to_owned)
        };
        vec![
            Fact {
                label: "Domain",
                value: opt(urls.and_then(|u| u.domain.as_ref())),
            },
            Fact {
                label: "Dashboard",
                value: opt(urls.and_then(|u| u.home.as_ref())),
            },
            Fact {
                label: "Socket",
                value: cfg.socket.clone(),
            },
        ]
    }
}

/// One row in this window's approval list — spec §6.5's queue, narrowed to
/// the one agent this window is for.
///
/// **v1 is not read-only.** Unlike [`Fact`], a row here carries the two verbs
/// that answer it; see [`crate::ui::Approvals`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRow {
    /// The queue id — what a click's `Approve { id }` / `Deny { id }` names.
    pub id: i64,
    /// What this row shows about the request, as the hive sent it — the
    /// identity its two buttons are bound to ([`Decision::shown`]).
    ///
    /// The plugin's own [`Shown`], not a copy of it: the sidebar's consent
    /// card and this row answer the same queue, so "is this still the request
    /// the operator saw?" has one answer. The id alone is not that identity —
    /// a hive whose store was reset or restored hands the same id to a
    /// different request — so [`crate::ui::Approvals::apply`] keeps a row's
    /// widgets only while both match, and [`should_send`] sends only while the
    /// row queued under the id still is this one.
    pub shown: Shown,
    /// What granting it will do, in English (`ApprovalKind::human`).
    pub title: String,
    /// The manager's description plus the request stamp, bounded.
    pub detail: String,
    /// The hive's own reason the **last** decision for this row was refused,
    /// if it was. Cleared the moment the row itself leaves the queue —
    /// `Window::update`'s job, not this type's: a row that no longer exists
    /// has nothing to keep showing a reason for.
    pub refused: Option<String>,
    /// Whether a decision for this row is **in flight** — sent and not yet
    /// answered, either by a poll that takes the approval out of `Pending` or
    /// by a refusal (#1146's review, H1).
    ///
    /// Renders as two insensitive buttons. `Approve`/`Deny` act immediately on
    /// the far side and nothing is flipped optimistically here, so without
    /// this the row sits for a whole cadence with two live buttons: a
    /// double-click sends the same decision twice, and an impatient operator
    /// can send `Deny` behind an `Approve` that already succeeded — which
    /// spec §6.5 spends its whole "silence decides nothing" argument
    /// preventing.
    pub in_flight: bool,
}

impl ApprovalRow {
    /// Derive one row from the hive's `Approval` plus whatever this window
    /// last heard back about a decision on it.
    #[must_use]
    pub fn of(approval: &Approval, refused: Option<String>, in_flight: bool) -> Self {
        Self {
            id: approval.id,
            shown: Shown::of(approval),
            // Flattened and bounded at its source since #1509 (an unknown
            // kind's raw token is wire text), so the sidebar and this row
            // cannot disagree about how tall it may get.
            title: approval.kind.human(),
            // The plugin's own renderer, not a copy of it (#1146's review,
            // M4): the sidebar's consent card and this row show the same
            // manager-written free text under the same bounds, and a shared
            // function makes that a compile-time fact. It flattens the
            // agent-written description to one line before the clamp (#1509).
            detail: hytte_plugin_agents::plugin::detail_line(approval),
            refused,
            in_flight,
        }
    }

    /// The subtitle text: the detail line, plus the refusal reason on its own
    /// line when the last write for this row came back refused (#1141's "a
    /// refused write keeps the row and shows why inline" — this is the
    /// "inline": the row's own text, not a separate banner).
    ///
    /// The reason is the hive's free text, so it is flattened to **one** line
    /// ([`single_line`]) the way the detail line is: the `\n` before it is
    /// this row's own layout, and a reason carrying line breaks or a bidi
    /// override of its own would otherwise decide how the rest of the row
    /// reads, beside the two buttons that answer it.
    #[must_use]
    pub fn subtitle(&self) -> String {
        match &self.refused {
            Some(reason) => format!("{}\ncouldn't answer: {}", self.detail, single_line(reason)),
            None => self.detail.clone(),
        }
    }
}

/// Which of a row's two buttons was pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    /// Approve.
    Approve,
    /// Deny.
    Deny,
}

/// One decision a row's Approve/Deny button asks for (#1141, spec §6.5),
/// **bound to what that row showed**.
///
/// It used to be the bare id, and so did the guard: a click was sent whenever
/// *some* row was queued under its id. That is the hole #1509 closed for the
/// sidebar's consent card — a hive that re-numbered its queue (a store reset
/// or restore, possibly faster than one poll) holds a different request under
/// the same id, and an `Approve { id }` would spend the operator's authority
/// on a request this window never showed them. The row's [`Shown`] rides the
/// click, and [`should_send`] compares it against the row queued now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// Which button.
    pub verb: Verb,
    /// The queue id the frame will name.
    pub id: i64,
    /// What the row showed about the request when it was built.
    pub shown: Shown,
}

impl Decision {
    /// The click on `row`'s `verb` button.
    #[must_use]
    pub fn on(row: &ApprovalRow, verb: Verb) -> Self {
        Self {
            verb,
            id: row.id,
            shown: row.shown.clone(),
        }
    }

    /// The frame this decision becomes once [`should_send`] lets it through.
    #[must_use]
    pub fn request(&self) -> Request {
        match self.verb {
            Verb::Approve => crate::feed::approve(self.id),
            Verb::Deny => crate::feed::deny(self.id),
        }
    }
}

/// What the approvals group says when the hive refused to hand over the queue
/// at all (#1146's review, M1) — a state of its own, because the rows are
/// cleared for this the same way they are cleared for an empty queue.
pub const APPROVALS_REFUSED: &str = "The hive refused the approval queue";

/// What the approvals group says while the hive is unreachable.
pub const APPROVALS_UNREACHABLE: &str = "The hive is unreachable";

/// The line under [`APPROVALS_UNREACHABLE`].
pub const UNREACHABLE_QUEUE: &str = "Its approval queue can't be read, so nothing here can be answered until it is \
     back. A request that was waiting is still waiting.";

/// What this window knows about its agent's approval queue — the state a
/// click is judged against, and what the approvals group renders.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Queue {
    /// Nothing read yet: no status poll has answered since launch. The
    /// header says "connecting"; the group has nothing to say yet.
    #[default]
    Unread,
    /// The hive's last answer, narrowed to this agent and oldest first
    /// ([`pending_for`]).
    Read(Vec<Approval>),
    /// The status poll answered and the hive refused `Pending`, in its own
    /// words (#1146's review, M1).
    Refused(String),
    /// The status poll failed. The last queue is **forgotten** rather than
    /// kept, as the plugin forgets its own (#1509): keeping it would leave
    /// live buttons on rows nobody can vouch for, and a click on one would
    /// send a bare id to a hive that may come back holding something else
    /// under it.
    Unreachable,
}

impl Queue {
    /// The rows to render — none unless the hive's last answer is in hand.
    #[must_use]
    pub fn rows(&self) -> &[Approval] {
        match self {
            Self::Read(rows) => rows,
            Self::Unread | Self::Refused(_) | Self::Unreachable => &[],
        }
    }

    /// Whether the row queued under `id` is still the request `shown`
    /// describes — `false` for a queue this window cannot read.
    #[must_use]
    pub fn holds(&self, id: i64, shown: &Shown) -> bool {
        self.rows()
            .iter()
            .find(|a| a.id == id)
            .is_some_and(|row| shown.is(row))
    }

    /// What the group shows **instead of** rows, when it cannot show rows and
    /// an empty group would read as "nothing to decide".
    #[must_use]
    pub fn notice(&self) -> Option<QueueNotice> {
        match self {
            Self::Unread | Self::Read(_) => None,
            Self::Refused(reason) => Some(QueueNotice {
                title: APPROVALS_REFUSED,
                detail: reason.clone(),
            }),
            Self::Unreachable => Some(QueueNotice {
                title: APPROVALS_UNREACHABLE,
                detail: UNREACHABLE_QUEUE.to_owned(),
            }),
        }
    }
}

/// The one row the approvals group shows in place of a queue it cannot read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueNotice {
    /// The row's title.
    pub title: &'static str,
    /// Its subtitle.
    pub detail: String,
}

/// The `Pending` answer, filtered to this window's one agent.
///
/// Spec §6.5's policy — still-waiting only, oldest first — is
/// [`PendingApprovals::new`], reused rather than re-derived: two readers of
/// one queue's rules have to agree, exactly as `hive::wire` argues for the
/// bytes. What this adds on top is the one thing the sidebar's model does not
/// need: **one hive answer carries every agent's queue**, and this window
/// renders exactly one agent's rows.
#[must_use]
pub fn pending_for(name: &AgentName, queue: Vec<Approval>) -> Vec<Approval> {
    PendingApprovals::new(queue)
        .all()
        .iter()
        .filter(|a| a.agent == name.as_str())
        .cloned()
        .collect()
}

/// What [`should_send`] makes of one click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The row queued under the click's id is still the request it showed:
    /// send the frame.
    Send,
    /// Nothing is queued under that id any more — answered on the dashboard,
    /// by `hivectl`, or withdrawn, between the render and the click. Spec
    /// §6.5: "dropped with a debug line, not an error."
    Gone,
    /// A **different** request is queued under that id: the hive re-numbered
    /// its queue. Dropped unsent — the operator never saw what it would
    /// decide — and the window repaints from the queue as it now is.
    Replaced,
    /// The window cannot read the queue right now (the hive is unreachable,
    /// refused `Pending`, or has not answered yet). Not sent, and **the
    /// operator is told** ([`not_sent`]): they pressed a button, and silence
    /// would read as success.
    Unread,
}

/// Whether one click is still worth sending, against the queue this window
/// last read — the guard the window's `on_decision` applies before a
/// decision becomes a frame.
///
/// The same rule the plugin's `Agents::decide` applies to the consent card
/// (#1509), over the same [`Shown`]: the id must still be queued, it must
/// still be **the request the row showed**, and a queue nobody could read
/// decides nothing. A pure function, so every race — a poll landing between
/// the render and the click, a re-numbered id, a hive that went away — needs
/// no display server to test.
#[must_use]
pub fn should_send(queue: &Queue, decision: &Decision) -> Verdict {
    let Queue::Read(rows) = queue else {
        return Verdict::Unread;
    };
    match rows.iter().find(|a| a.id == decision.id) {
        None => Verdict::Gone,
        Some(row) if decision.shown.is(row) => Verdict::Send,
        Some(_) => Verdict::Replaced,
    }
}

/// The banner for a decision [`should_send`] answered [`Verdict::Unread`]
/// for.
///
/// Named from what the row **showed** rather than from the queue, which is
/// exactly what is missing at this moment — the same choice the plugin's
/// "not sent" toast makes (#1509). The kind's wording is already flattened
/// and bounded at its source (`ApprovalKind::human`).
#[must_use]
pub fn not_sent(decision: &Decision) -> String {
    let what = match decision.verb {
        Verb::Approve => "approval",
        Verb::Deny => "denial",
    };
    format!(
        "{what} not sent — the hive's approval queue can't be read right now. \"{}\" (request #{}) \
         is still waiting on you; answer it here once the hive is back",
        decision.shown.kind().human(),
        decision.id
    )
}

#[cfg(test)]
mod approvals_tests {
    use super::{
        APPROVALS_REFUSED, APPROVALS_UNREACHABLE, ApprovalRow, Decision, Queue, QueueNotice, Verb,
        Verdict, not_sent, pending_for, should_send,
    };
    use hytte_plugin_agents::hive::wire::{Approval, ApprovalKind, ApprovalStatus, Request};
    use hytte_plugin_agents::model::AgentName;

    fn name(s: &str) -> AgentName {
        AgentName::parse(s).expect("a legal test name")
    }

    fn approval(id: i64, agent: &str, status: ApprovalStatus) -> Approval {
        Approval {
            id,
            agent: agent.to_owned(),
            status,
            ..Approval::default()
        }
    }

    /// **Rows render from `Pending`.** Only this agent's still-waiting
    /// approvals survive, oldest id first — another agent's row and a
    /// resolved row are both dropped, which is the two filters
    /// `PendingApprovals::new` plus the per-agent narrowing are for.
    ///
    /// Mutation (verified red): drop the `filter(|a| a.agent == …)` line and
    /// the length assertion reds (the other agent's row leaks in); pass the
    /// raw `queue` straight through instead of via `PendingApprovals::new`
    /// and the length assertion reds the other way (the `Approved` row
    /// leaks in).
    #[test]
    fn rows_render_from_pending_scoped_to_this_agent_oldest_first() {
        let queue = vec![
            approval(9, "stray", ApprovalStatus::Pending),
            approval(3, "stray", ApprovalStatus::Pending),
            approval(5, "other-agent", ApprovalStatus::Pending),
            approval(1, "stray", ApprovalStatus::Approved),
        ];
        let rows = pending_for(&name("stray"), queue);
        assert_eq!(
            rows.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![3, 9],
            "oldest first, this agent only, still pending only"
        );
    }

    /// One row of `approval(id, "stray", Pending)` with a description and a
    /// stamp — so the identity a decision carries has something in it to
    /// differ on.
    fn request(id: i64, description: &str) -> Approval {
        Approval {
            description: Some(description.to_owned()),
            requested_at: "2026-09-12T00:00:00Z".to_owned(),
            ..approval(id, "stray", ApprovalStatus::Pending)
        }
    }

    /// The click on `approval`'s row, as its button would raise it.
    fn click(approval: &Approval, verb: Verb) -> Decision {
        Decision::on(&ApprovalRow::of(approval, None, false), verb)
    }

    /// **A stale id is not sent.** `should_send` is the guard
    /// [`crate::window::Window::on_decision`] applies before a click ever
    /// reaches the command lane — an id the last poll no longer carries has
    /// left the queue between the render and the click.
    ///
    /// Mutation (verified red): answer `Verdict::Send` for a missing row and
    /// the `Gone` assertions red.
    #[test]
    fn a_stale_id_is_not_worth_sending() {
        let shown = request(7, "merge PR #12");
        let decision = click(&shown, Verb::Approve);
        assert_eq!(
            should_send(&Queue::Read(vec![shown.clone()]), &decision),
            Verdict::Send
        );
        assert_eq!(
            should_send(&Queue::Read(vec![request(99, "merge PR #12")]), &decision),
            Verdict::Gone,
            "a different id is not this row"
        );
        assert_eq!(
            should_send(&Queue::Read(Vec::new()), &decision),
            Verdict::Gone,
            "an emptied queue carries nothing"
        );
    }

    /// **A matching request still sends** — the binding refuses nothing it
    /// should not. The row the click was raised from is still queued under
    /// its id, unchanged, beside rows that came and went around it; and what
    /// the guard lets through is the pinned frame for the button pressed.
    ///
    /// Mutation (verified red): answer `Replaced` for every row found under
    /// the id (i.e. a guard so strict it refuses the request it was built
    /// from) and the first assertion reds.
    #[test]
    fn a_matching_request_still_sends() {
        let shown = request(7, "merge PR #12");
        let approve = click(&shown, Verb::Approve);
        let deny = click(&shown, Verb::Deny);
        let refreshed = Queue::Read(vec![
            request(3, "an older one"),
            shown.clone(),
            request(9, "a newer one"),
        ]);
        assert_eq!(should_send(&refreshed, &approve), Verdict::Send);
        assert_eq!(should_send(&refreshed, &deny), Verdict::Send);
        assert_eq!(approve.request(), Request::Approve { id: 7 });
        assert_eq!(deny.request(), Request::Deny { id: 7 });
    }

    /// **A re-numbered id is not sent** — the hole #1509 closed for the
    /// sidebar, in this window's own guard.
    ///
    /// The row was rendered from request A under id 7. Then the hive's store
    /// was reset or restored — possibly faster than one poll, so this window
    /// never saw it go — and id 7 now names request B. A queue refresh
    /// landing between the render and the click is the same case seen from
    /// the click's side. An `Approve { id: 7 }` would approve B, which the
    /// operator never saw; the verdict is `Replaced`, and nothing is sent.
    /// Every field the row shows is part of that identity, so a change to
    /// any one of them alone is a different request.
    ///
    /// Mutation (verified red): revert `should_send` to the id-only
    /// `rows.iter().any(|a| a.id == decision.id)` and every assertion here
    /// answers `Send`.
    #[test]
    fn a_renumbered_id_is_not_sent() {
        let shown = request(7, "merge PR #12");
        let decision = click(&shown, Verb::Approve);

        let renumbered = request(7, "merge PR #99 (force-push to main)");
        assert_eq!(
            should_send(&Queue::Read(vec![renumbered]), &decision),
            Verdict::Replaced
        );

        let other_kind = Approval {
            kind: ApprovalKind::Spawn,
            ..shown.clone()
        };
        let other_stamp = Approval {
            requested_at: "2026-10-25T09:00:00Z".to_owned(),
            ..shown.clone()
        };
        let no_description = Approval {
            description: None,
            ..shown.clone()
        };
        let other_agent = Approval {
            agent: "other-agent".to_owned(),
            ..shown.clone()
        };
        for row in [other_kind, other_stamp, no_description, other_agent] {
            assert_eq!(
                should_send(&Queue::Read(vec![row.clone()]), &decision),
                Verdict::Replaced,
                "{row:?} is not the request the row showed"
            );
        }
    }

    /// **A stale queue while the hive is unreachable sends nothing, and says
    /// so.** A window that could not read the queue — the hive is down, it
    /// refused `Pending`, or it has not answered yet — has no row to vouch
    /// for, so even the request the click was raised from is `Unread`; and
    /// the banner names it from what the row showed, since the queue it came
    /// from is exactly what is missing.
    ///
    /// Mutation (verified red): answer `Gone` rather than `Unread` for a
    /// queue that is not `Read` — the silent drop the plugin keeps for a
    /// request resolved elsewhere — and the first assertion reds; the
    /// operator would click and hear nothing.
    #[test]
    fn a_stale_queue_while_unreachable_sends_nothing_and_says_so() {
        let shown = request(7, "merge PR #12");
        let approve = click(&shown, Verb::Approve);
        for queue in [
            Queue::Unreachable,
            Queue::Refused("permission denied".to_owned()),
            Queue::Unread,
        ] {
            assert_eq!(should_send(&queue, &approve), Verdict::Unread, "{queue:?}");
            assert!(!queue.holds(7, &approve.shown), "{queue:?}");
            assert!(queue.rows().is_empty(), "{queue:?} renders no rows");
        }

        let banner = not_sent(&approve);
        assert!(banner.starts_with("approval not sent"), "{banner}");
        assert!(
            banner.contains(&ApprovalKind::MergeConfigPr.human()),
            "{banner}"
        );
        assert!(banner.contains("#7"), "{banner}");
        assert!(
            not_sent(&click(&shown, Verb::Deny)).starts_with("denial not sent"),
            "a Deny is named as one"
        );
    }

    /// **A queue the window cannot read says why, rather than looking empty**
    /// — the refused state #1146's review (M1) added, and the unreachable one
    /// beside it. An answered queue, empty or not, has nothing to say instead
    /// of its rows; neither has a window still connecting, whose header
    /// already says so.
    #[test]
    fn an_unreadable_queue_has_a_notice_and_a_read_one_does_not() {
        let refused = Queue::Refused("permission denied".to_owned()).notice();
        assert_eq!(
            refused,
            Some(QueueNotice {
                title: APPROVALS_REFUSED,
                detail: "permission denied".to_owned(),
            })
        );
        let unreachable = Queue::Unreachable.notice().expect("a notice");
        assert_eq!(unreachable.title, APPROVALS_UNREACHABLE);
        assert_eq!(Queue::Read(Vec::new()).notice(), None);
        assert_eq!(Queue::Read(vec![request(1, "x")]).notice(), None);
        assert_eq!(Queue::Unread.notice(), None);
    }

    /// **Everything beside the two buttons is one line of text** (#1509's
    /// sanitising, reaching this window). The title and the detail line are
    /// the plugin's own renderers, which flatten the agent-written text; the
    /// refusal reason is this window's own addition and is flattened here.
    /// A run of line breaks would otherwise stack the row tall enough to
    /// push its buttons away, and a right-to-left override would reverse
    /// the stamp beside them.
    ///
    /// Mutation (verified red): drop the `single_line` around the reason in
    /// `ApprovalRow::subtitle` and the line-count assertion reds.
    #[test]
    fn the_text_beside_the_buttons_is_one_line_each() {
        let hostile = Approval {
            kind: ApprovalKind::Unknown("spawn\n\n\nreplica".to_owned()),
            description: Some("looks fine\n\n\n\napproved by annika\u{202E}21# RP".to_owned()),
            requested_at: "2026-09-12\n00:00".to_owned(),
            ..approval(11, "stray", ApprovalStatus::Pending)
        };
        let row = ApprovalRow::of(
            &hostile,
            Some("busy\n\n\nyour approval was accepted\u{202E}".to_owned()),
            false,
        );
        for text in [&row.title, &row.detail] {
            assert!(!text.contains('\n'), "{text:?}");
            assert!(!text.contains('\u{202E}'), "{text:?}");
        }
        let subtitle = row.subtitle();
        assert_eq!(
            subtitle.lines().count(),
            2,
            "the detail line and the reason, one line each: {subtitle:?}"
        );
        assert!(!subtitle.contains('\u{202E}'), "{subtitle:?}");
    }

    /// **A refused write keeps the row** — the row is [`ApprovalRow`], built
    /// the same way whether or not a decision on it was ever refused, and the
    /// refusal reaches the screen through the row's own subtitle rather than
    /// removing the row or routing through a separate banner.
    ///
    /// Mutation (verified red): make `subtitle` ignore `refused` and the
    /// second assertion reds.
    #[test]
    fn a_refused_write_keeps_the_row_and_shows_why_inline() {
        let approval = approval(11, "stray", ApprovalStatus::Pending);
        let clean = ApprovalRow::of(&approval, None, false);
        assert_eq!(clean.subtitle(), clean.detail);

        let refused = ApprovalRow::of(&approval, Some("agent busy".to_owned()), false);
        assert_eq!(refused.id, 11, "the row is still the same approval");
        assert!(refused.subtitle().contains(&refused.detail));
        assert!(refused.subtitle().contains("agent busy"));
    }

    /// **The row's detail line is the plugin's, not a copy of it** (#1146's
    /// review, M4). The sidebar's consent card and this window's row render
    /// the same manager-written free text under the same bounds; the two used
    /// to be byte-identical bodies in two crates, so a change to one was a
    /// silent divergence.
    ///
    /// Mutation: re-introduce a local `detail_line` with any different bound
    /// or wording and this reds.
    #[test]
    fn the_detail_line_is_the_plugins_own_renderer() {
        let long = "x".repeat(hytte_plugin_agents::plugin::DETAIL_CHARS + 50);
        let a = Approval {
            description: Some(long.clone()),
            requested_at: "2026-09-12T00:00:00Z".to_owned(),
            ..approval(11, "stray", ApprovalStatus::Pending)
        };
        assert_eq!(
            ApprovalRow::of(&a, None, false).detail,
            hytte_plugin_agents::plugin::detail_line(&a),
            "the window must render the queue's free text through the plugin's own function"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{CONNECTING, Controls, Facts, HeaderModel, UNKNOWN_AGENT};
    use crate::feed::AgentState;
    use hytte_plugin_agents::config::{AgentsConfig, Display};
    use hytte_plugin_agents::hive::wire::{AgentStatusRow, HiveUrls};
    use hytte_plugin_agents::model::{Agent, AgentName, Status};

    fn name(s: &str) -> AgentName {
        AgentName::parse(s).expect("a legal test name")
    }

    fn up(row: AgentStatusRow) -> AgentState {
        AgentState::Up(Box::new(Agent {
            name: name(&row.name.clone()),
            row,
            pending_paused: None,
        }))
    }

    fn running() -> AgentStatusRow {
        AgentStatusRow {
            name: "stray".to_owned(),
            running: true,
            active_model: Some("claude-opus-5-20262981".to_owned()),
            status_text: Some("reviewing PR #963".to_owned()),
            ..AgentStatusRow::default()
        }
    }

    /// The header carries the harness's own words and the **short** model,
    /// with the full id kept for the hover.
    ///
    /// Falsification: show `active_model` verbatim and the
    /// `model_word` assertion reds — the chip would read
    /// `claude-opus-5-20262981` in a header sized for `Opus`.
    #[test]
    fn a_running_agents_header_is_its_status_text_and_a_short_model() {
        let h = HeaderModel::of(&name("stray"), &AgentsConfig::default(), &up(running()));
        assert_eq!(h.title, "stray");
        assert_eq!(h.status, "reviewing PR #963");
        assert_eq!(h.status_class, "accent");
        assert_eq!(h.model_word.as_deref(), Some("Opus"));
        assert_eq!(h.model_full.as_deref(), Some("claude-opus-5-20262981"));
    }

    /// `agents.toml`'s display block drives this window too — the same label
    /// and icon the sidebar card shows, from the same `AgentsConfig`.
    ///
    /// Falsification: read `name.as_str()` directly instead of
    /// `cfg.label_for` and both assertions red.
    #[test]
    fn the_display_config_the_card_reads_names_this_window_too() {
        let mut cfg = AgentsConfig::default();
        cfg.display.insert(
            "stray".to_owned(),
            Display {
                label: Some("Stray cat".to_owned()),
                icon: Some("face-smile-symbolic".to_owned()),
                project: None,
            },
        );
        let h = HeaderModel::of(&name("stray"), &cfg, &up(running()));
        assert_eq!(h.title, "Stray cat");
        assert_eq!(h.icon, "face-smile-symbolic");
    }

    /// The three non-agent states each say their own thing, and none of them
    /// borrows another's icon.
    #[test]
    fn every_state_has_its_own_sentence_and_glyph() {
        let cfg = AgentsConfig::default();
        let n = name("stray");
        let connecting = HeaderModel::of(&n, &cfg, &AgentState::Connecting);
        let unknown = HeaderModel::of(&n, &cfg, &AgentState::Unknown);
        let down = HeaderModel::of(
            &n,
            &cfg,
            &AgentState::Unreachable {
                reason: "no socket — hive-c0re is not running".to_owned(),
            },
        );
        assert_eq!(connecting.status, CONNECTING);
        assert_eq!(unknown.status, UNKNOWN_AGENT);
        assert_eq!(down.status, "no socket — hive-c0re is not running");
        let icons = [
            connecting.status_icon,
            unknown.status_icon,
            down.status_icon,
        ];
        let mut distinct = icons.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            icons.len(),
            "each state is distinguishable at a glance: {icons:?}"
        );
        assert!(connecting.model_word.is_none(), "no agent, no model chip");
    }

    /// Start and Stop are mutually exclusive, keyed on the same `Stopped` test
    /// the card uses, and neither is live until the hive has answered —
    /// pinned across **all five** `Status` rows.
    ///
    /// All five, and not the three the fixtures happened to cover, because of
    /// #1130's **M15**: `let stopped = !agent.row.running;` passed the whole
    /// suite. The two predicates agree everywhere the old test looked — the
    /// paused fixture is `running: true`, where both say "not stopped" — and
    /// diverge exactly on `failed` and `needs_login` with `running: false`,
    /// the two rows nothing covered. Driving the table off [`Status::ALL`] is
    /// what makes "iterate the enum" the assertion rather than a habit.
    ///
    /// Worth stating because it surprised the reviewer too: a **`Failed`**
    /// agent is offered **Stop**, not Start. That is P1's rule — the card's
    /// `lifecycle_affordance` keys on the same `Status::Stopped` — so if it is
    /// wrong it is wrong in both surfaces, and this test is where the two are
    /// held together.
    ///
    /// Mutation (re-run this round, red): the reviewer's **M15**, keying
    /// `stopped` on `!row.running`, now reds on the `Failed` and `NeedsLogin`
    /// rows.
    #[test]
    fn the_lifecycle_buttons_are_exclusive_and_dead_until_the_hive_answers() {
        for state in [
            AgentState::Connecting,
            AgentState::Unknown,
            AgentState::Unreachable {
                reason: "down".to_owned(),
            },
        ] {
            let c = Controls::of(&state);
            assert!(!c.live && !c.can_start && !c.can_stop, "{state:?}");
        }

        // One row per `Status`, built so `Status::of` collapses to it — the
        // same precedence the card renders (`failed > needs_login > paused >
        // running > stopped`).
        let rows: [(Status, AgentStatusRow); 5] = [
            (
                Status::Failed,
                AgentStatusRow {
                    failed: true,
                    running: false,
                    ..running()
                },
            ),
            (
                Status::NeedsLogin,
                AgentStatusRow {
                    needs_login: true,
                    running: false,
                    ..running()
                },
            ),
            (
                Status::Paused,
                AgentStatusRow {
                    paused: true,
                    ..running()
                },
            ),
            (
                Status::Stopped,
                AgentStatusRow {
                    running: false,
                    ..running()
                },
            ),
            (Status::Running, running()),
        ];
        let covered: Vec<Status> = rows.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            covered,
            Status::ALL.to_vec(),
            "every Status row must be pinned here, in precedence order — that is what M15 \
             slipped through"
        );

        for (expected, row) in rows {
            let state = up(row);
            let status = state.agent().expect("built as Up").status();
            assert_eq!(status, expected, "the fixture collapses to {expected:?}");

            let c = Controls::of(&state);
            assert!(c.live, "{expected:?}: the hive has answered");
            assert_eq!(
                c.can_start,
                expected == Status::Stopped,
                "{expected:?}: Start is offered exactly when the agent is stopped"
            );
            assert_eq!(
                c.can_stop,
                expected != Status::Stopped,
                "{expected:?}: Stop is offered exactly when it is not"
            );
            assert!(
                !(c.can_start && c.can_stop),
                "{expected:?}: the two are mutually exclusive"
            );
            assert_eq!(
                c.paused,
                expected == Status::Paused,
                "{expected:?}: the toggle follows the hive"
            );
        }
    }

    /// The settings page shows what the hive says, and a dash where it says
    /// nothing — never an empty row and never a guess.
    #[test]
    fn the_settings_facts_render_absence_as_a_dash() {
        let facts = Facts::agent(&name("stray"), &AgentState::Connecting);
        let by = |l: &str| {
            facts
                .iter()
                .find(|f| f.label == l)
                .unwrap_or_else(|| panic!("no {l} row"))
                .value
                .clone()
        };
        assert_eq!(by("Name"), "stray");
        assert_eq!(by("Model"), Facts::ABSENT);
        assert_eq!(by("Agent page"), Facts::ABSENT);
        assert_eq!(by("Status"), CONNECTING);

        let row = AgentStatusRow {
            url: Some("https://hive.local/agent/stray/".to_owned()),
            deployed_sha: Some("abc123def456".to_owned()),
            ..running()
        };
        let facts = Facts::agent(&name("stray"), &up(row));
        let by = |l: &str| {
            facts
                .iter()
                .find(|f| f.label == l)
                .expect("row")
                .value
                .clone()
        };
        assert_eq!(by("Model"), "claude-opus-5-20262981", "the FULL id here");
        assert_eq!(by("Agent page"), "https://hive.local/agent/stray/");
        assert_eq!(by("Deployed"), "abc123def456");
    }

    /// The hive block names where this window is reading from — the socket
    /// path included, because "the hive is unreachable" is unactionable
    /// without it.
    #[test]
    fn the_hive_facts_name_the_socket_even_with_no_urls() {
        let cfg = AgentsConfig::default();
        let facts = Facts::hive(&cfg, None);
        assert!(
            facts
                .iter()
                .any(|f| f.label == "Socket" && f.value == cfg.socket),
            "{facts:?}"
        );
        assert!(facts.iter().all(|f| !f.value.is_empty()));

        let urls = HiveUrls {
            domain: Some("hive.local".to_owned()),
            home: Some("  https://hive.local/  ".to_owned()),
            forge: None,
        };
        let facts = Facts::hive(&cfg, Some(&urls));
        assert!(
            facts
                .iter()
                .any(|f| f.label == "Dashboard" && f.value == "https://hive.local/"),
            "trimmed, like everywhere else the hive's strings are read: {facts:?}"
        );
    }
}
