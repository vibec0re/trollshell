//! The window's own `host.sock` client: one agent's live state, its pending
//! approval queue (#1141), and the five verbs its buttons send.
//!
//! # Why the window reads the socket itself
//!
//! The chrome around the embedded page is ours, and Annika's requirement
//! ([#947](https://github.com/vibec0re/trollshell/issues/947), 2026-09-11
//! 07:16Z) is that we control it — "additional buttons, status, agent
//! settings". So the header's status is read from the hive directly, exactly
//! as the sidebar card reads it, and **not** scraped out of the embedded
//! page's DOM: that page is slated for a swarm-level rewrite (Mara, same
//! thread), and the whole point of the `?hide=` contract is that nothing here
//! depends on its markup.
//!
//! # It is the plugin's client, not a second one
//!
//! Everything on the wire — [`Request`], the row, the version check, the
//! one-connection-per-request round trip, the `Scope` that cannot mean "the
//! whole hive" — comes from `hytte-plugin-agents`, which exposes `hive` and
//! `model` as a library. Two readers of one socket have to agree byte for byte
//! about its verbs; a second mirror would be a second thing to update when
//! hyperhive moves.
//!
//! The **cadence** is `agents.toml`'s (`poll_seconds`, shared with the card),
//! so an operator who slowed the sidebar down has slowed this down too.

use std::path::PathBuf;
use std::time::Duration;

use hytte_plugin_agents::hive::client::{self, HiveError};
use hytte_plugin_agents::hive::wire::{Approval, HiveUrls, Request, Response, Scope};
use hytte_plugin_agents::model::{Agent, AgentName};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// What the chrome knows about this window's agent right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentState {
    /// Before the first answer.
    Connecting,
    /// The socket could not be read — the reason is already operator-facing
    /// (`hive::client::connect_reason`).
    Unreachable {
        /// What to do about it, not the errno.
        reason: String,
    },
    /// The hive answered, and has no agent by this name. A window opened for
    /// an agent that has since been destroyed lands here.
    Unknown,
    /// The agent, as the hive holds it.
    Up(Box<Agent>),
}

impl AgentState {
    /// Fold one `AgentStatus` answer into this window's one-agent view.
    #[must_use]
    pub fn of(answer: &Result<Response, HiveError>, name: &AgentName) -> Self {
        match answer {
            Err(e) => Self::Unreachable {
                reason: e.to_string(),
            },
            Ok(resp) => resp
                .agent_statuses
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|row| row.name == name.as_str())
                .map_or(Self::Unknown, |row| {
                    Self::Up(Box::new(Agent {
                        name: name.clone(),
                        row: row.clone(),
                        // The window re-polls the moment a verb is accepted, so
                        // it has no optimistic flip to reconcile — the card's
                        // `pending_paused` exists because its pause button sits
                        // on a 2 s cadence with no write-triggered refresh.
                        pending_paused: None,
                    }))
                }),
        }
    }

    /// The agent, when there is one.
    #[must_use]
    pub fn agent(&self) -> Option<&Agent> {
        match self {
            Self::Up(a) => Some(a),
            _ => None,
        }
    }
}

/// Everything the poll task tells the GTK side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// The agent's state changed (or arrived for the first time).
    State(AgentState),
    /// The hive's own URLs, fetched once per session.
    Urls(Box<HiveUrls>),
    /// The approval queue as the hive returned it (#1141), **unfiltered** —
    /// same contract as `hytte_plugin_agents::poll::Msg::Pending`: "which
    /// statuses are still actionable" and "which agent is this window's" are
    /// both model policy (`crate::chrome::pending_for`), not something the
    /// I/O task decides. A `Pending` refusal (an older daemon, a permissions
    /// change) also arrives here as an empty `Vec` — the window can no longer
    /// vouch for the rows, so it clears them rather than freezing on the last
    /// good answer.
    Approvals(Vec<Approval>),
    /// A verb this window sent was refused, with the hive's own words.
    Refused {
        /// The verb, for the sentence the window shows.
        request: Request,
        /// The hive's reason.
        reason: String,
    },
}

/// `Start`, scoped to this one agent.
///
/// The same bytes the card sends (`{"cmd":"start","scope":{"agent_names":[…]}}`)
/// because it is the same constructor: [`Scope::agent`] is the only way to
/// build a scope, and an all-false `LifecycleScope` would mean *the entire
/// hive* to hyperhive.
#[must_use]
pub fn start(name: &AgentName) -> Request {
    Request::Start {
        scope: Scope::agent(name.as_str()),
    }
}

/// `Stop`, scoped to this one agent, **graceful** — the per-agent quiesce, not
/// a hard stop. The card's choice, kept.
#[must_use]
pub fn stop(name: &AgentName) -> Request {
    Request::Stop {
        scope: Scope::agent(name.as_str()),
        graceful: true,
    }
}

/// `SetPaused` — park or resume the turn loop. Idempotent both ways on the
/// hive's side, and it works on a stopped container too.
#[must_use]
pub fn set_paused(name: &AgentName, paused: bool) -> Request {
    Request::SetPaused {
        name: name.as_str().to_owned(),
        paused,
    }
}

/// `Approve` one queued approval by id (#1141, spec §6.5) — the action runs
/// **immediately** on the far side, so this is sent for a click and nothing
/// else; [`crate::chrome::should_send`] is the guard the window applies
/// before it ever reaches [`run`]'s command lane.
#[must_use]
pub fn approve(id: i64) -> Request {
    Request::Approve { id }
}

/// `Deny` one queued approval by id. Only ever sent for a click — an
/// unanswered prompt is not a `Deny` (spec §6.5).
#[must_use]
pub fn deny(id: i64) -> Request {
    Request::Deny { id }
}

/// Poll one agent forever, and send what the buttons ask for.
///
/// Ends when either channel is closed — which is how the window's own exit
/// stops it: the GTK side drops the receiver.
///
/// The command lane is **biased** over the tick, and every accepted command is
/// followed by an immediate re-poll, so a button's effect shows up at once
/// instead of up to `interval` later.
pub async fn run(
    socket: PathBuf,
    name: AgentName,
    interval: Duration,
    mut cmds: UnboundedReceiver<Request>,
    out: UnboundedSender<Update>,
) {
    let mut last: Option<AgentState> = None;
    let mut last_approvals: Option<Vec<Approval>> = None;
    let mut urls = UrlsFetch::default();
    if urls.attempt(&socket, &out).await.is_err() {
        return;
    }
    if poll_once(&socket, &name, &out, &mut last, &mut last_approvals)
        .await
        .is_err()
    {
        return;
    }

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // the first tick is immediate; the seed above was it.

    loop {
        tokio::select! {
            biased;
            cmd = cmds.recv() => {
                let Some(req) = cmd else { return };
                if let Err(reason) = write(&socket, &req).await {
                    if out.send(Update::Refused { request: req, reason }).is_err() {
                        return;
                    }
                    // **Force the next poll to re-emit.** A refusal means the
                    // controls are showing what the operator *asked for* and
                    // the hive said no — most visibly the pause toggle, which
                    // GTK has already flipped. The reconciling state is by
                    // definition the state we last sent, so the dedup in
                    // `poll_once` would swallow it and leave the window
                    // claiming a pause the daemon never made (#1130 M2).
                    last = None;
                }
                if urls.attempt(&socket, &out).await.is_err() {
                    return;
                }
                if poll_once(&socket, &name, &out, &mut last, &mut last_approvals)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            _ = ticker.tick() => {
                if urls.attempt(&socket, &out).await.is_err() {
                    return;
                }
                if poll_once(&socket, &name, &out, &mut last, &mut last_approvals)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// The `Urls` answer, fetched once — but **retried** until it lands.
///
/// It used to be a single attempt before the loop, which meant a window opened
/// while the hive was down showed `—` for Domain and Dashboard on the Settings
/// tab for the rest of the session, even after the header went green (#1130
/// L6). It is still once per *success*: the hive's domain does not change under
/// a running window, and re-asking every two seconds for a value that never
/// moves is the kind of chatter the poll's own dedup exists to avoid.
///
/// Backoff is in **polls**, not in time, because this rides the caller's ticker
/// and has no timer of its own: 1, 2, 4, 8 … up to [`UrlsFetch::MAX_SKIP`]
/// polls between attempts. At the default two-second cadence that settles at
/// roughly one attempt a minute, which is the right order for "the hive came
/// back".
#[derive(Debug, Default)]
struct UrlsFetch {
    /// `true` once the hive has answered with urls; nothing is asked after.
    done: bool,
    /// Polls still to skip before the next attempt.
    skip: u32,
    /// How many to skip after the next failure.
    backoff: u32,
}

impl UrlsFetch {
    /// The ceiling on the backoff, in polls.
    const MAX_SKIP: u32 = 32;

    /// Ask, if this attempt is due. `Err(())` means the GTK side is gone.
    async fn attempt(
        &mut self,
        socket: &std::path::Path,
        out: &UnboundedSender<Update>,
    ) -> Result<(), ()> {
        if self.done {
            return Ok(());
        }
        if self.skip > 0 {
            self.skip -= 1;
            return Ok(());
        }
        if let Ok(resp) = client::request(socket, &Request::Urls).await
            && let Some(urls) = resp.urls
        {
            self.done = true;
            return out.send(Update::Urls(Box::new(urls))).map_err(|_| ());
        }
        self.backoff = (self.backoff.saturating_mul(2)).clamp(1, Self::MAX_SKIP);
        self.skip = self.backoff;
        Ok(())
    }
}

/// One write. `Ok(())` when the hive accepted it.
async fn write(socket: &std::path::Path, req: &Request) -> Result<(), String> {
    client::request(socket, req)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// One poll: `AgentStatus`, sent only when it **changed**, then `Pending`
/// (#1141) — only once the status call already proved the socket answering,
/// same order [`hytte_plugin_agents::poll::poll_once`] uses and for the same
/// reason: a dead hive should cost one failed connect, not two.
///
/// Dedup on `Update::State` is not an optimisation: it drives a label rewrite
/// and a button-sensitivity pass on the GTK thread, and at the default
/// two-second cadence an idle hive would otherwise repaint the header
/// forever. `Update::Approvals` is deduped too, on the **raw** answer — the
/// per-agent filter runs downstream, in [`crate::chrome::pending_for`], so two
/// polls that changed only another agent's queue still cost this window
/// nothing.
///
/// `Err(())` means the GTK side is gone.
async fn poll_once(
    socket: &std::path::Path,
    name: &AgentName,
    out: &UnboundedSender<Update>,
    last: &mut Option<AgentState>,
    last_approvals: &mut Option<Vec<Approval>>,
) -> Result<(), ()> {
    let answer = client::request(socket, &Request::AgentStatus).await;
    let status_ok = answer.is_ok();
    let state = AgentState::of(&answer, name);

    // `Pending` rides the same tick, right after `AgentStatus` and only once
    // it answered — same order and reason `hytte_plugin_agents::poll`'s
    // `poll_once` uses: a dead hive should cost one failed connect, not two.
    // A refusal (an older daemon, a permissions change) clears this window's
    // rows rather than freezing on the last good answer — the same rule
    // `hytte_plugin_agents::plugin::Agents`'s `Input::App(Msg::Pending(Err))`
    // arm follows for the sidebar's badges.
    let approvals = if status_ok {
        Some(match client::request(socket, &Request::Pending).await {
            Ok(resp) => resp.approvals.unwrap_or_default(),
            Err(e) => {
                tracing::debug!(
                    %e,
                    "the hive refused the approval queue; clearing this window's rows until it answers"
                );
                Vec::new()
            }
        })
    } else {
        None
    };

    // Every send below is **synchronous** (an unbounded channel never
    // blocks) — deliberately no `.await` between here and `return`. The seed
    // call relies on that: a test observes `Update::State` the moment it is
    // sent and then assumes this task has already reached its ticker
    // (`run`'s next few lines, none of which await either), which held only
    // because nothing here used to await *after* the send. Fetching the two
    // answers above **first**, and sending only once both are in hand,
    // keeps that property true with a second round trip in the mix.
    if last.as_ref() != Some(&state) {
        *last = Some(state.clone());
        out.send(Update::State(state)).map_err(|_| ())?;
    }
    if let Some(approvals) = approvals
        && last_approvals.as_ref() != Some(&approvals)
    {
        *last_approvals = Some(approvals.clone());
        out.send(Update::Approvals(approvals)).map_err(|_| ())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AgentState, approve, deny, set_paused, start, stop};
    use hytte_plugin_agents::hive::client::HiveError;
    use hytte_plugin_agents::hive::wire::{AgentStatusRow, Response};
    use hytte_plugin_agents::model::{AgentName, Status};

    fn name(s: &str) -> AgentName {
        AgentName::parse(s).expect("a legal test name")
    }

    /// One good `AgentStatus` answer, in the shape the client hands over.
    fn answer(rows: Vec<AgentStatusRow>) -> Response {
        Response {
            version: 1,
            ok: true,
            agent_statuses: Some(rows),
            ..Response::default()
        }
    }

    /// The three verbs serialize to the **exact** lines the card sends — the
    /// bytes `hive::wire`'s own test pins, restated here because this window is
    /// a second writer on the same socket and a divergence would be silent.
    ///
    /// Mutation (verified red, #1130 review M6): drop `graceful`, or build the scope any other
    /// way, and the line changes.
    #[test]
    fn the_three_verbs_put_their_pinned_bytes_on_the_socket() {
        let n = name("trollshell-choom");
        let line = |r| serde_json::to_string(&r).expect("a Request serializes");
        assert_eq!(
            line(start(&n)),
            r#"{"cmd":"start","scope":{"agent_names":["trollshell-choom"]}}"#
        );
        assert_eq!(
            line(stop(&n)),
            r#"{"cmd":"stop","scope":{"agent_names":["trollshell-choom"]},"graceful":true}"#
        );
        assert_eq!(
            line(set_paused(&n, true)),
            r#"{"cmd":"set_paused","name":"trollshell-choom","paused":true}"#
        );
        assert_eq!(
            line(set_paused(&n, false)),
            r#"{"cmd":"set_paused","name":"trollshell-choom","paused":false}"#
        );
    }

    /// `Approve` and `Deny` put the **exact** bytes hyperhive's own host
    /// documents on the socket — cited from `hive-host-sock/src/lib.rs:230-233`
    /// via `hive::wire::Request`'s own committed test
    /// (`crates/hytte-plugin-agents/src/hive/wire.rs`), not derived from this
    /// crate's `Request` variants: a serializer bug that renamed the `cmd` tag
    /// or dropped the `id` field would still round-trip through `Request`
    /// itself and stay invisible to an assertion built the same way.
    ///
    /// Mutation (verified red): swap the two bodies (`approve` builds `Deny`,
    /// `deny` builds `Approve`) and both assertions red.
    #[test]
    fn approve_and_deny_put_their_pinned_bytes_on_the_socket() {
        let line = |r| serde_json::to_string(&r).expect("a Request serializes");
        assert_eq!(line(approve(42)), r#"{"cmd":"approve","id":42}"#);
        assert_eq!(line(deny(42)), r#"{"cmd":"deny","id":42}"#);
    }

    /// Every verb names **exactly one** agent — spec §11 rule one, which
    /// matters more here than anywhere because an all-false scope means the
    /// entire hive to hyperhive.
    #[test]
    fn every_verb_is_scoped_to_one_agent() {
        use hytte_plugin_agents::hive::wire::Request;
        let n = name("stray");
        for req in [start(&n), stop(&n), set_paused(&n, true)] {
            match req {
                Request::Start { scope } | Request::Stop { scope, .. } => {
                    assert_eq!(scope.agent_names(), ["stray".to_owned()]);
                }
                Request::SetPaused { name, .. } => assert_eq!(name, "stray"),
                other => panic!("unexpected verb {other:?}"),
            }
        }
    }

    /// The window's agent is picked **out of the roster by name**, not taken
    /// as the first row — a hive serves every agent on one answer.
    ///
    /// Mutation (verified red, #1130 review M18): take `rows[0]` and the second assertion reds.
    #[test]
    fn the_state_is_this_windows_agent_and_not_the_first_row() {
        let rows = vec![
            AgentStatusRow {
                name: "other".to_owned(),
                running: true,
                ..AgentStatusRow::default()
            },
            AgentStatusRow {
                name: "stray".to_owned(),
                paused: true,
                running: true,
                ..AgentStatusRow::default()
            },
        ];
        let s = AgentState::of(&Ok(answer(rows)), &name("stray"));
        let agent = s.agent().expect("the roster carries stray");
        assert_eq!(agent.name.as_str(), "stray");
        assert_eq!(agent.status(), Status::Paused);
    }

    /// A roster without this agent is its own state — "the hive has no such
    /// agent" is not "the hive is down", and the window says different things.
    #[test]
    fn a_roster_without_this_agent_is_unknown_not_unreachable() {
        let rows = vec![AgentStatusRow {
            name: "other".to_owned(),
            ..AgentStatusRow::default()
        }];
        assert_eq!(
            AgentState::of(&Ok(answer(rows)), &name("stray")),
            AgentState::Unknown
        );
        assert_eq!(
            AgentState::of(&Ok(answer(Vec::new())), &name("stray")),
            AgentState::Unknown
        );
    }

    /// A client error carries the client's own operator-facing sentence
    /// through unchanged — this window must not re-word "needs `hive-admin`
    /// group" into something less actionable.
    #[test]
    fn an_error_keeps_the_clients_own_reason() {
        let e = HiveError::Unreachable {
            reason: "permission denied — needs `hive-admin` group (re-login after adding)"
                .to_owned(),
        };
        assert_eq!(
            AgentState::of(&Err(e.clone()), &name("stray")),
            AgentState::Unreachable {
                reason: e.to_string()
            }
        );
    }
}
