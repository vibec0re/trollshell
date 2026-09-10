//! The broker itself: the consent decisions, the audit trail, and the socket
//! server that binds `$XDG_RUNTIME_DIR/hytte-infobroker.sock` and answers the
//! [`crate::wire`] JSON-lines protocol.
//!
//! The **decision** functions ([`authorize_auth`], [`authorize_get`]) are pure
//! and unit-tested here; [`serve`] is the thin async shell that wires sockets +
//! the panel command lane around them. The broker owns the [`GrantStore`]
//! (durable) and the [`TokenStore`] (ephemeral) — no shared mutex, because it
//! processes one connection or one panel command at a time.
//!
//! Consent policy (phase 1b — interactive Allow/Deny prompting, over the 1a
//! grant/token machinery):
//! - `auth` mints a token **silently** iff an `always` grant covers the agent.
//! - A standing `deny` grant refuses `auth` **silently** — a settled "no" isn't
//!   re-prompted — with an informational [`Toast`] so the knock is still visible.
//! - Otherwise (no standing grant) `auth` **parks** the socket request and fires
//!   a consent prompt at the human (`Effect::RequestConsent` via the plugin,
//!   #487). The decision resolves the parked request per
//!   [`BrokerState::apply_consent`]: `AllowAlways`/`Deny` persist to grants.toml,
//!   `AllowSession` mints a session-scoped token, `AllowOnce` a single-fetch
//!   token. An unanswered prompt (a pre-1b / wedged host, or a genuinely ignored
//!   one) times out to a **transient** deny + the 1a toast — the phase-1a
//!   fallback ([`BrokerState::on_consent_timeout`]).
//! - `get <datasource>` requires a valid token whose data-access authority
//!   ([`TokenScope`]) — durable grant, session, or a single once — covers the
//!   datasource; a spent/uncovered token is denied *without* a toast (re-auth to
//!   re-consent).
//! - An **invalid/expired token** is a transient technical failure (the agent
//!   just re-auths), so it is denied *without* a toast — only genuine consent
//!   knocks alert the human.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::grants::{Decision, GrantStore};
use crate::paths;
use crate::tokens::{Token, TokenScope, TokenStore};
use crate::wire::{
    CalendarEntry, DATASOURCE_CALENDAR, DATASOURCE_DEPARTURES, DATASOURCE_WEATHER, DepartureOut,
    GrantOut, Request, Response, WeatherOut, encode_response, parse_request,
};

/// Cap on the in-memory audit ring shown in the panel. Oldest entries fall off.
const AUDIT_CAP: usize = 30;

/// How long a connected client has to send its one request line before the
/// broker gives up on it (so a stuck client can't wedge the accept loop).
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the broker holds a **parked** consent request before giving up and
/// timing it out (#487 phase 1b). Deliberately a touch longer than the shell's
/// own 60 s prompt bound: the shell owns the user-facing countdown and always
/// sends a decision within it (an explicit click, or `Deny` on its own timeout),
/// so a live shell's answer reliably arrives first — this is the pure fallback
/// for a wedged or pre-1b host that never surfaces the prompt at all.
const CONSENT_PARK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(65);

/// How long the broker holds a **parked** datasource query before giving up
/// (#509). A backstop for a wedged/pre-#509 host: the shell's own query router has
/// a 10 s bound that ALWAYS relays a result (the provider's answer or a synthesized
/// timeout), so a live shell's `Cmd::QueryResult` reliably arrives first — this is
/// the pure fallback for a host that never relays one at all.
const QUERY_PARK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The default row `limit` for a departures query when the client sends none.
const DEFAULT_DEPARTURES_LIMIT: usize = 8;

/// The `departures` datasource scope the broker queries (#509).
const SCOPE_DEPARTURES_NEXT: &str = "next";
/// The `weather` datasource scope the broker queries (#509).
const SCOPE_WEATHER_CURRENT: &str = "current";

// ── Pure consent decisions (unit-tested) ──────────────────────────────────────

/// The outcome of an `auth` request against the grant store (#487 phase 1b).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    /// The agent has an `always` grant — mint a token silently.
    Granted,
    /// A standing `deny` grant covers the agent — a settled "no": refuse silently
    /// (no re-prompt), with a how-to-grant hint.
    Denied { hint: String },
    /// No standing grant either way — knock the human with an interactive consent
    /// prompt and park the request until the decision (or the 60 s bound).
    NeedsConsent,
}

/// The outcome of a `get <datasource>` request against the grant store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GetOutcome {
    /// An `always` grant covers `(agent, datasource)` — serve the fetch.
    Allowed,
    /// No `always` grant (missing or an explicit `deny`) — deny + hint.
    Denied { hint: String },
}

/// The calendar rows a `get calendar` serves: the live copy, clamped to `limit`
/// (the host already caps the copy at five). Pure — the copy is in-memory, so
/// there's nothing to fetch. `#[must_use]` isn't needed on a private helper.
fn calendar_scoped(calendar: &[CalendarEntry], limit: Option<usize>) -> Vec<CalendarEntry> {
    let n = limit.map_or(calendar.len(), |l| l.min(calendar.len()));
    calendar.iter().take(n).cloned().collect()
}

/// The calendar datasource's panel status line — how many events the live copy
/// holds (#484). Pure.
fn calendar_status(len: usize) -> String {
    match len {
        0 => "no upcoming events".to_owned(),
        1 => "1 upcoming".to_owned(),
        n => format!("{n} upcoming"),
    }
}

/// Whether `datasource` is one the broker serves (#509): departures/weather (routed
/// through their provider plugins) or calendar (the host-fed live copy).
fn is_known_datasource(datasource: &str) -> bool {
    matches!(
        datasource,
        DATASOURCE_DEPARTURES | DATASOURCE_WEATHER | DATASOURCE_CALENDAR
    )
}

/// The provider scope + opaque JSON params for a routed `get <datasource>` (#509).
/// departures carries the row `limit`; weather takes no params.
fn query_scope_and_params(datasource: &str, limit: Option<usize>) -> (String, String) {
    if datasource == DATASOURCE_WEATHER {
        (SCOPE_WEATHER_CURRENT.to_owned(), "{}".to_owned())
    } else {
        let n = limit.unwrap_or(DEFAULT_DEPARTURES_LIMIT);
        (
            SCOPE_DEPARTURES_NEXT.to_owned(),
            format!("{{\"limit\":{n}}}"),
        )
    }
}

/// Build a `get` [`Response`] from a routed datasource query's outcome (#509).
/// Decodes the provider's opaque JSON payload into the datasource's typed rows; a
/// failure (host routing error, provider error, or an unreadable payload) becomes a
/// transient `Response::error` the agent can retry. Pure — unit-tested.
fn query_response(datasource: &str, outcome: QueryOutcome) -> Response {
    match outcome {
        QueryOutcome::Ready(payload) => match datasource {
            DATASOURCE_DEPARTURES => match serde_json::from_str::<Vec<DepartureOut>>(&payload) {
                Ok(rows) => Response {
                    ok: true,
                    datasource: Some(datasource.to_owned()),
                    departures: Some(rows),
                    ..Response::default()
                },
                Err(e) => Response::error(format!("departures: unreadable provider payload: {e}")),
            },
            DATASOURCE_WEATHER => match serde_json::from_str::<WeatherOut>(&payload) {
                Ok(weather) => Response {
                    ok: true,
                    datasource: Some(datasource.to_owned()),
                    weather: Some(weather),
                    ..Response::default()
                },
                Err(e) => Response::error(format!("weather: unreadable provider payload: {e}")),
            },
            other => Response::error(format!("unexpected routed datasource '{other}'")),
        },
        QueryOutcome::Failed(message) => Response::error(format!("{datasource}: {message}")),
    }
}

/// The panel status line for a datasource routed through a provider plugin (#509) —
/// the broker doesn't hold the provider's live state, so it names the route.
fn routed_status(datasource: &str) -> String {
    format!("via the {datasource} plugin")
}

/// The how-to-grant hint pointing the human at the two grant surfaces.
fn grant_hint(agent: &str, datasource: &str) -> String {
    format!(
        "allow it in the infobroker panel, or add a grant to grants.toml: \
         [[grant]] agent = \"{agent}\" datasource = \"{datasource}\" decision = \"always\""
    )
}

/// Decide an `auth` (#487 phase 1b): a standing `always` grant mints silently; a
/// standing `deny` grant refuses silently (no re-prompt); anything else needs an
/// interactive consent decision. `always` wins over `deny` if both somehow exist
/// (a live "yes" beats a stale "no").
#[must_use]
pub fn authorize_auth(grants: &GrantStore, agent: &str) -> AuthOutcome {
    if grants.has_any_always(agent) {
        AuthOutcome::Granted
    } else if grants.has_any_deny(agent) {
        AuthOutcome::Denied {
            hint: grant_hint(agent, DATASOURCE_DEPARTURES),
        }
    } else {
        AuthOutcome::NeedsConsent
    }
}

/// Decide a `get`: allowed iff `(agent, datasource)` has an `always` grant.
#[must_use]
pub fn authorize_get(grants: &GrantStore, agent: &str, datasource: &str) -> GetOutcome {
    match grants.decision_for(agent, datasource) {
        Some(Decision::Always) => GetOutcome::Allowed,
        _ => GetOutcome::Denied {
            hint: grant_hint(agent, datasource),
        },
    }
}

// ── Panel-facing snapshot (SDK-free; the plugin maps it to a Node tree) ───────

/// Whether a request was allowed or denied — the audit trail's outcome column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Granted,
    Denied,
}

impl Outcome {
    /// A short label for the panel.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Granted => "granted",
            Outcome::Denied => "denied",
        }
    }
}

/// One durable grant, projected for the panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantView {
    pub agent: String,
    pub datasource: String,
    pub decision: &'static str,
}

/// An agent that knocked, was denied, and still has no `always` grant — the
/// panel offers a one-click **Allow** for it (writes an `always` grant).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingView {
    pub agent: String,
    pub datasource: String,
}

/// A live session token, projected for the panel's status readout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenView {
    pub agent: String,
    pub expires_unix: i64,
}

/// One audit-trail entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditView {
    pub at_unix: i64,
    pub agent: String,
    /// The requested resource: a datasource name, or `"auth"`.
    pub resource: String,
    pub outcome: Outcome,
}

/// The departures (and, later, other) datasource status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatasourceView {
    pub name: String,
    pub status: String,
}

/// The full panel state, rebuilt after every event and pushed to the plugin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrokerSnapshot {
    pub grants: Vec<GrantView>,
    pub pending: Vec<PendingView>,
    pub tokens: Vec<TokenView>,
    /// Newest first.
    pub audit: Vec<AuditView>,
    pub datasources: Vec<DatasourceView>,
    /// A one-line reason this broker is **not** serving its socket, rendered at
    /// the top of the panel. `None` in the normal case. Without it a stood-down
    /// duplicate (#995) painted a panel indistinguishable from an idle broker
    /// and the only explanation was one stderr line in that unit's journal.
    pub notice: Option<String>,
}

// ── The panel command lane + the outbound message ─────────────────────────────

/// The broker-local mirror of the proto's four-way consent choice (#487 phase
/// 1b). The library stays SDK-free — it never links `hytte_plugin`/its proto —
/// so the plugin binary (`plugin.rs`) maps `proto::ConsentDecision` onto this
/// when forwarding a decision down the [`Cmd`] lane, exactly as it maps a
/// [`Toast`] onto `Effect::Notify`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsentDecision {
    /// Allow exactly this one request (a single-fetch token).
    AllowOnce,
    /// Allow for the rest of this session (a session-scoped token, no persist).
    AllowSession,
    /// Allow always (persist an `always` grant + mint a token).
    AllowAlways,
    /// Deny (persist a standing `deny` grant).
    Deny,
}

/// The broker-local mirror of the proto's datasource query outcome (#509). The
/// library stays SDK-free — it never links `hytte_plugin`/its proto — so the
/// plugin binary (`plugin.rs`) maps `proto::DatasourceOutcome` onto this when
/// relaying a query result down the [`Cmd`] lane, exactly as it maps a
/// [`ConsentDecision`] onto the proto one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryOutcome {
    /// The provider answered; the opaque JSON payload (the provider↔broker
    /// contract) is decoded per-datasource into the [`Response`].
    Ready(String),
    /// The query failed — a host routing failure (no provider / denied scope /
    /// timeout) or the provider's own error. The string is the human message.
    Failed(String),
}

/// A datasource query the broker asks the plugin to route to the shell host
/// (#509): the plugin turns it into `Effect::DatasourceQuery`, and the answer
/// returns as [`Cmd::QueryResult`] keyed by the same `request_id`. The broker
/// itself never dials a provider — the host is the chokepoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryRequest {
    /// The broker-minted correlation, echoed back in [`Cmd::QueryResult`].
    pub request_id: u64,
    /// The datasource id / provider name (e.g. `"departures"`).
    pub provider: String,
    /// The provider scope (e.g. `"next"`).
    pub scope: String,
    /// The opaque JSON request payload (the provider↔broker contract).
    pub params: String,
}

/// A command from the plugin down to the broker task (the #280 lane pattern).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmd {
    /// Delete `(agent, datasource)`'s grant and kill that agent's live tokens.
    Revoke { agent: String, datasource: String },
    /// Add an `always` grant for `(agent, datasource)` — the panel's Allow.
    Allow { agent: String, datasource: String },
    /// The human's answer to a parked consent knock (#487 phase 1b), keyed by the
    /// `request_id` the broker minted for it. Routed to the matching parked
    /// request rather than through [`BrokerState::apply_cmd`].
    Decision {
        request_id: u64,
        decision: ConsentDecision,
    },
    /// The shell's latest upcoming-calendar digest (#484), relayed by the plugin
    /// from its `CalendarUpcoming` host push. Replaces the broker's live copy that
    /// `get calendar` serves — the broker can't read EDS itself.
    Calendar(Vec<CalendarEntry>),
    /// The result of a datasource query the broker issued (#509), relayed by the
    /// plugin from its `Input::DatasourceResult` host push, keyed by the
    /// `request_id` the broker minted on the originating [`BrokerMsg::Query`].
    /// Routed to the matching parked `get` rather than through `apply_cmd`.
    QueryResult {
        request_id: u64,
        outcome: QueryOutcome,
    },
}

/// An informational toast the plugin should post via `Effect::Notify`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub summary: String,
    pub body: String,
}

/// A pending consent knock the plugin should surface as `Effect::RequestConsent`
/// (#487 phase 1b). The human-facing strings the broker computes; `request_id`
/// correlates the eventual [`Cmd::Decision`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsentPrompt {
    pub request_id: u64,
    pub agent: String,
    pub datasource: String,
    /// A short human-readable scope phrase (e.g. `"read access"`), not the grant
    /// store's internal `*` scope.
    pub scope: String,
    /// A secondary detail line for the prompt.
    pub detail: String,
}

/// The broker → plugin message. One message type keeps the plugin reducer a
/// short match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrokerMsg {
    /// A fresh panel snapshot, plus an optional toast to raise (present only on a
    /// consent denial / timeout).
    Update {
        snapshot: BrokerSnapshot,
        toast: Option<Toast>,
    },
    /// Raise an interactive consent prompt for a parked request (#487 phase 1b).
    /// The plugin turns this into `Effect::RequestConsent`.
    RequestConsent(ConsentPrompt),
    /// Route a datasource query to the shell host (#509). The plugin turns this
    /// into `Effect::DatasourceQuery`; the answer returns as [`Cmd::QueryResult`].
    Query(QueryRequest),
}

// ── The broker state ──────────────────────────────────────────────────────────

/// One recorded request, before projection to an [`AuditView`].
#[derive(Clone, Debug)]
struct AuditEntry {
    at_unix: i64,
    agent: String,
    resource: String,
    outcome: Outcome,
}

/// The broker's owned state: durable grants, ephemeral tokens, the audit ring,
/// and the live calendar copy the host push feeds (#484).
struct BrokerState {
    grants: GrantStore,
    tokens: TokenStore,
    audit: VecDeque<AuditEntry>,
    /// The latest upcoming-calendar digest relayed from the shell's host push
    /// (#484). `get calendar` serves this — the broker can't read EDS itself, so
    /// the live copy *is* the datasource. Empty until the first push lands.
    calendar: Vec<CalendarEntry>,
    /// Set when this session has no socket of its own (#995): every snapshot
    /// carries the explanation up to the panel, for the session's whole life.
    notice: Option<String>,
}

/// The current wall clock in unix seconds.
fn now_unix() -> i64 {
    Utc::now().timestamp()
}

impl BrokerState {
    fn new(grants: GrantStore) -> Self {
        Self {
            grants,
            tokens: TokenStore::default(),
            audit: VecDeque::with_capacity(AUDIT_CAP),
            calendar: Vec::new(),
            notice: None,
        }
    }

    /// Record one request in the capped audit ring (oldest falls off).
    fn record(&mut self, agent: &str, resource: &str, outcome: Outcome, now: i64) {
        if self.audit.len() == AUDIT_CAP {
            self.audit.pop_front();
        }
        self.audit.push_back(AuditEntry {
            at_unix: now,
            agent: agent.to_owned(),
            resource: resource.to_owned(),
            outcome,
        });
    }

    /// Project the current state into a panel snapshot at `now`.
    fn snapshot(&mut self, now: i64) -> BrokerSnapshot {
        let grants: Vec<GrantView> = self
            .grants
            .grants()
            .iter()
            .map(|g| GrantView {
                agent: g.agent.clone(),
                datasource: g.datasource.clone(),
                decision: g.decision.as_str(),
            })
            .collect();

        // Pending = agents that were denied and still lack an `always` grant.
        // Phase 1a has one datasource, so a denied auth (no grant at all) surfaces
        // as a pending Allow for departures too.
        let mut pending: Vec<PendingView> = Vec::new();
        for e in &self.audit {
            if e.outcome != Outcome::Denied {
                continue;
            }
            if self.grants.decision_for(&e.agent, DATASOURCE_DEPARTURES) == Some(Decision::Always) {
                continue;
            }
            let pv = PendingView {
                agent: e.agent.clone(),
                datasource: DATASOURCE_DEPARTURES.to_owned(),
            };
            if !pending.contains(&pv) {
                pending.push(pv);
            }
        }

        let tokens: Vec<TokenView> = self
            .tokens
            .active(now)
            .iter()
            .map(|t| TokenView {
                agent: t.agent.clone(),
                expires_unix: t.expires_unix,
            })
            .collect();

        // Newest first.
        let audit: Vec<AuditView> = self
            .audit
            .iter()
            .rev()
            .map(|e| AuditView {
                at_unix: e.at_unix,
                agent: e.agent.clone(),
                resource: e.resource.clone(),
                outcome: e.outcome,
            })
            .collect();

        // departures/weather are routed through their provider plugins (#509), so
        // the broker names the route rather than a live status it no longer holds;
        // calendar is the host-fed live copy, so it reports its count.
        let datasources = vec![
            DatasourceView {
                name: DATASOURCE_DEPARTURES.to_owned(),
                status: routed_status(DATASOURCE_DEPARTURES),
            },
            DatasourceView {
                name: DATASOURCE_WEATHER.to_owned(),
                status: routed_status(DATASOURCE_WEATHER),
            },
            DatasourceView {
                name: DATASOURCE_CALENDAR.to_owned(),
                status: calendar_status(self.calendar.len()),
            },
        ];

        BrokerSnapshot {
            grants,
            pending,
            tokens,
            audit,
            datasources,
            notice: self.notice.clone(),
        }
    }

    /// Apply a panel command (revoke / allow). Returns nothing — the caller
    /// pushes a fresh snapshot afterwards.
    fn apply_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Revoke { agent, datasource } => {
                // `revoke` (#1065) queues its persist off-thread and returns
                // immediately; only whether a row was actually removed is
                // synchronous, which is all a token kill needs.
                if self.grants.revoke(&agent, &datasource) {
                    // Revoking a grant invalidates any live tokens riding on it.
                    let killed = self.tokens.revoke_agent(&agent);
                    tracing_eprintln(&format!(
                        "revoked {agent}/{datasource}; killed {killed} token(s)"
                    ));
                }
            }
            Cmd::Allow { agent, datasource } => self.grants.grant_always(&agent, &datasource),
            // The host-pushed calendar digest replaces the live copy `get calendar`
            // serves (#484).
            Cmd::Calendar(entries) => self.calendar = entries,
            // A consent decision / query result is routed to its parked request in
            // `serve`, not applied here; these arms keep the match exhaustive.
            Cmd::Decision { .. } | Cmd::QueryResult { .. } => {}
        }
    }

    /// Handle one client connection: read its request line and dispatch. Either
    /// writes the response inline ([`ConnResult::Answered`]) or hands the stream
    /// back to [`serve`] to **park** while a consent prompt is out
    /// ([`ConnResult::NeedsConsent`], #487 phase 1b).
    async fn handle_conn(&mut self, stream: UnixStream) -> ConnResult {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let read = tokio::time::timeout(REQUEST_TIMEOUT, reader.read_line(&mut line)).await;
        let mut stream = reader.into_inner();
        let dispatch = match read {
            Ok(Ok(0)) => return ConnResult::Answered { toast: None }, // client closed, no request
            Ok(Ok(_)) => self.dispatch(line.trim()),
            Ok(Err(e)) => Dispatch::Answer(Response::error(format!("read error: {e}")), None),
            Err(_) => Dispatch::Answer(Response::error("timed out waiting for a request"), None),
        };
        match dispatch {
            Dispatch::Answer(response, toast) => {
                write_response(&mut stream, response).await;
                ConnResult::Answered { toast }
            }
            Dispatch::Consent {
                agent,
                datasource,
                scope,
                detail,
            } => ConnResult::NeedsConsent {
                agent,
                datasource,
                scope,
                detail,
                stream,
            },
            // #509: a routed datasource query — park the client and ask the plugin
            // to route it to the shell host; the answering `Cmd::QueryResult`
            // resolves it.
            Dispatch::Query {
                datasource,
                scope,
                params,
            } => ConnResult::NeedsQuery {
                datasource,
                scope,
                params,
                stream,
            },
        }
    }

    /// Dispatch one parsed request to its handler.
    fn dispatch(&mut self, line: &str) -> Dispatch {
        let req = match parse_request(line) {
            Ok(req) => req,
            Err(e) => return Dispatch::Answer(Response::error(e), None),
        };
        match req {
            Request::Auth { agent } => self.handle_auth(&agent),
            Request::Get {
                token,
                datasource,
                limit,
            } => match self.handle_get(&token, &datasource, limit) {
                GetDisposition::Answer(resp, toast) => Dispatch::Answer(resp, toast),
                GetDisposition::Query {
                    datasource,
                    scope,
                    params,
                } => Dispatch::Query {
                    datasource,
                    scope,
                    params,
                },
            },
            Request::Grants => Dispatch::Answer(self.handle_grants(), None),
        }
    }

    /// `auth` (#487 phase 1b): mint silently on an `always` grant; refuse
    /// silently (+ toast) on a standing `deny`; otherwise ask for consent (the
    /// caller parks the request).
    fn handle_auth(&mut self, agent: &str) -> Dispatch {
        let now = now_unix();
        if agent.trim().is_empty() {
            return Dispatch::Answer(Response::error("auth: empty agent name"), None);
        }
        match authorize_auth(&self.grants, agent) {
            AuthOutcome::Granted => {
                let token = self.tokens.mint(agent, now);
                self.record(agent, "auth", Outcome::Granted, now);
                Dispatch::Answer(auth_ok_response(agent, &token), None)
            }
            AuthOutcome::Denied { hint } => {
                self.record(agent, "auth", Outcome::Denied, now);
                Dispatch::Answer(
                    Response::denied(format!("no grant for agent '{agent}'"), hint),
                    Some(denied_toast(agent, "access")),
                )
            }
            AuthOutcome::NeedsConsent => Dispatch::Consent {
                agent: agent.to_owned(),
                datasource: DATASOURCE_DEPARTURES.to_owned(),
                scope: "read access".to_owned(),
                detail: format!("{agent} wants to read the {DATASOURCE_DEPARTURES} board"),
            },
        }
    }

    /// Resolve a parked consent knock (#487 phase 1b) once the human decides.
    /// `AllowAlways`/`Deny` persist to grants.toml; `AllowSession` mints a
    /// session-scoped token; `AllowOnce` a single-fetch token. Every allow answers
    /// the parked `auth` with a fresh token; a deny persists a standing "no".
    fn apply_consent(
        &mut self,
        agent: &str,
        datasource: &str,
        decision: ConsentDecision,
        now: i64,
    ) -> (Response, Option<Toast>) {
        match decision {
            ConsentDecision::Deny => {
                // #1065: persists off-thread; failures self-log from inside
                // the detached write, not here.
                self.grants.grant_deny(agent, datasource);
                self.record(agent, "auth", Outcome::Denied, now);
                (
                    Response::denied(
                        format!("consent denied for agent '{agent}'"),
                        grant_hint(agent, datasource),
                    ),
                    Some(denied_toast(agent, "access")),
                )
            }
            ConsentDecision::AllowAlways => {
                self.grants.grant_always(agent, datasource);
                let token = self.tokens.mint_scoped(agent, now, TokenScope::Grant);
                self.record(agent, "auth", Outcome::Granted, now);
                (auth_ok_response(agent, &token), None)
            }
            ConsentDecision::AllowSession => {
                let token = self.tokens.mint_scoped(agent, now, TokenScope::Session);
                self.record(agent, "auth", Outcome::Granted, now);
                (auth_ok_response(agent, &token), None)
            }
            ConsentDecision::AllowOnce => {
                let token = self.tokens.mint_scoped(agent, now, TokenScope::Once);
                self.record(agent, "auth", Outcome::Granted, now);
                (auth_ok_response(agent, &token), None)
            }
        }
    }

    /// A parked consent knock that ran out its [`CONSENT_PARK_TIMEOUT`] with no
    /// decision (a pre-1b / wedged host, or a genuinely ignored prompt). Deny THIS
    /// request **transiently** — no persisted "no", so the agent may re-ask — and
    /// raise the 1a informational toast so the missed knock stays visible (the
    /// phase-1a fallback path).
    fn on_consent_timeout(
        &mut self,
        agent: &str,
        datasource: &str,
        now: i64,
    ) -> (Response, Option<Toast>) {
        self.record(agent, "auth", Outcome::Denied, now);
        (
            Response::denied(
                format!("consent request for agent '{agent}' timed out"),
                grant_hint(agent, datasource),
            ),
            Some(denied_toast(agent, "access")),
        )
    }

    /// `get <datasource>` (#487 phase 1b / #509): token → `(agent, scope)` →
    /// data-access authority → serve. A token's [`TokenScope`] carries the consent
    /// decision that minted it, so `get` never re-prompts — it just honors (or
    /// spends) the authority already granted at `auth`. Once authorized, `calendar`
    /// is served inline from the host-fed live copy, while `departures`/`weather` are
    /// **routed through their provider plugins** over the host's datasource query
    /// protocol ([`GetDisposition::Query`]) rather than fetched here — the #509 dedup
    /// of the broker's former internal departures fetch.
    fn handle_get(
        &mut self,
        token: &str,
        datasource: &str,
        limit: Option<usize>,
    ) -> GetDisposition {
        let now = now_unix();
        let Some(auth) = self.tokens.resolve(token, now) else {
            // Transient/technical — the agent just re-auths; no consent toast.
            return GetDisposition::Answer(
                Response::error("invalid or expired token — re-auth with `hytte-infobroker auth`"),
                None,
            );
        };
        let agent = auth.agent;
        if !is_known_datasource(datasource) {
            self.record(&agent, datasource, Outcome::Denied, now);
            return GetDisposition::Answer(
                Response::denied(
                    format!("unknown datasource '{datasource}'"),
                    format!(
                        "known datasources: '{DATASOURCE_DEPARTURES}', '{DATASOURCE_WEATHER}', '{DATASOURCE_CALENDAR}'"
                    ),
                ),
                None,
            );
        }
        // Data-access authority: a durable `always` grant (for a plain identity
        // token), an open session token, or a single as-yet-unspent once token —
        // which this get consumes.
        let allowed = match auth.scope {
            TokenScope::Grant => {
                matches!(
                    authorize_get(&self.grants, &agent, datasource),
                    GetOutcome::Allowed
                )
            }
            TokenScope::Session => true,
            TokenScope::Once => !auth.spent && self.tokens.spend_once(token),
        };
        if !allowed {
            // A spent once, or an identity token whose grant is gone: transient —
            // re-auth to re-consent — so no toast (only genuine knocks alert).
            self.record(&agent, datasource, Outcome::Denied, now);
            return GetDisposition::Answer(
                Response::denied(
                    format!("agent '{agent}' has no current grant for '{datasource}'"),
                    grant_hint(&agent, datasource),
                ),
                None,
            );
        }
        // The access decision is granted; record it once here (the eventual serve
        // outcome — a live copy, or the routed query's success/failure — is separate,
        // exactly as the old inline fetch recorded granted before it could fail).
        self.record(&agent, datasource, Outcome::Granted, now);
        // Calendar is a live in-memory copy (the host push feeds it), served straight.
        if datasource == DATASOURCE_CALENDAR {
            let resp = Response {
                ok: true,
                datasource: Some(datasource.to_owned()),
                calendar: Some(calendar_scoped(&self.calendar, limit)),
                ..Response::default()
            };
            return GetDisposition::Answer(resp, None);
        }
        // departures / weather: route through the running provider plugin. The
        // broker never fetches — `serve` parks the client and asks the plugin to
        // emit `Effect::DatasourceQuery`; the answer resolves the parked request.
        let (scope, params) = query_scope_and_params(datasource, limit);
        GetDisposition::Query {
            datasource: datasource.to_owned(),
            scope,
            params,
        }
    }

    /// `grants`: read-only introspection of the durable store.
    fn handle_grants(&self) -> Response {
        let grants: Vec<GrantOut> = self
            .grants
            .grants()
            .iter()
            .map(|g| GrantOut {
                agent: g.agent.clone(),
                datasource: g.datasource.clone(),
                scope: g.scope.clone(),
                decision: g.decision.as_str().to_owned(),
            })
            .collect();
        Response {
            ok: true,
            grants: Some(grants),
            ..Response::default()
        }
    }
}

/// The informational toast for a denied knock.
fn denied_toast(agent: &str, resource: &str) -> Toast {
    Toast {
        summary: format!("infobroker: {agent} denied"),
        body: format!("{agent} requested {resource} — denied. Allow it in the infobroker panel."),
    }
}

/// The `auth`-ok response carrying the minted token, its expiry, and the resolved
/// agent identity (the same shape for every mint path).
fn auth_ok_response(agent: &str, token: &Token) -> Response {
    Response {
        ok: true,
        token: Some(token.value.clone()),
        expires_unix: Some(token.expires_unix),
        agent: Some(agent.to_owned()),
        ..Response::default()
    }
}

/// stderr diagnostic — systemd routes it to the journal (the SDK plugin uses
/// stderr for diagnostics; `tracing` isn't wired on the plugin side).
fn tracing_eprintln(msg: &str) {
    eprintln!("[infobroker] {msg}");
}

/// How long [`write_response`] will try to hand a response to a client before
/// giving up and dropping the connection instead of blocking its caller (and,
/// since #995, the process-wide [`SOCKET`] guard `serve` holds for its whole
/// body) forever.
///
/// Deliberately shorter than [`REQUEST_TIMEOUT`]: a slow-to-*send* peer might
/// just be a human on the other end of `nc -U`, but a peer that already has
/// bytes sitting in its kernel receive buffer and simply never drains them is
/// unambiguously wedged, not merely slow, so it gets less grace. Response
/// sizes are far under a UDS socket buffer today (#1004 review, N4), so this
/// is a backstop against a hostile/broken client, not a knob anyone should
/// expect to actually hit in production.
const WRITE_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Row count (summed across `grants`/`departures`/`calendar`, the
/// `Response` fields that scale with data rather than being O(1)) above
/// which [`write_response`] serializes on [`tokio::task::spawn_blocking`]
/// instead of inline on the select loop's own thread (#1065).
///
/// Calibrated against `encode_response`'s own wall time crossing ~1 ms — the
/// bound #1059 already treats as "worth moving off this runtime thread" for
/// `GrantStore::load`. Measured directly (`--release`, the profile that
/// actually ships; `cargo test`'s debug profile is slower per row and would
/// only argue for offloading *earlier*) with a `grants` response shaped like
/// [`crate::wire::GrantOut`], `best_of_7` per size, on the 64-core box this
/// was measured on:
///
/// | rows | encode time |
/// | ---: | ---: |
/// | 8 000 | 542.5 µs |
/// | 10 000 | 679.5 µs |
/// | 20 000 | 1.325 ms |
///
/// ~65 ns/row (each grant row is exactly 82 bytes of JSON — #1059/#1024's own
/// figure), crossing 1 ms at ≈15 000 rows (≈1.2 MiB encoded). `12_000` sits a
/// margin below that measured crossing, so a CI runner slower per-core than
/// this box (`ubuntu-latest`'s 4 vCPU, the same machine #1059's
/// `STARVED_TIMER_SLACK` rationale names) still offloads before it would hit
/// 1 ms itself — while staying well above anything `hytte-plugin-infobroker`
/// serves in realistic use: `departures`/`weather`/`calendar` are all
/// bounded by a human-scale dataset, so only a pathologically large
/// `grants.toml` reaches this row count at all. (The existing
/// `write_response_gives_up_on_a_client_that_never_reads` fixture is 10 000
/// grant rows, deliberately below this threshold — its own doc explains why
/// that size was chosen, and this constant doesn't change its behaviour.)
const LARGE_RESPONSE_ROWS: usize = 12_000;

// Compile-time cross-check for
// `write_response_gives_up_on_a_client_that_never_reads`'s 10 000-row
// fixture (`tests` module below): it relies on `encode_response` running
// *inline* (its own doc explains why), which only holds while its row count
// stays under `LARGE_RESPONSE_ROWS`. A future change that lowers the
// threshold past 10 000 fails the build here instead of silently
// invalidating that test's timing assumptions. The anonymous `const _` form
// is the idiom for this: unlike a named const it needs no reference to avoid
// `dead_code`, and it's still evaluated (and so still enforced) at compile
// time regardless.
const _: () = assert!(10_000 < LARGE_RESPONSE_ROWS);

/// Cheap proxy for "how much JSON `encode_response` is about to render",
/// computed without doing the encode: the summed length of `Response`'s
/// variable-size fields. `ok`/`error`/`hint`/`token`/etc. are all O(1)
/// scalars, so they don't move this number regardless of how many are set.
fn response_row_count(resp: &Response) -> usize {
    resp.grants.as_ref().map_or(0, Vec::len)
        + resp.departures.as_ref().map_or(0, Vec::len)
        + resp.calendar.as_ref().map_or(0, Vec::len)
}

/// Write one response line (JSON + `\n`) to a client stream, best-effort —
/// bounded by [`WRITE_RESPONSE_TIMEOUT`] so a client that connects and never
/// reads cannot park `serve`'s caller (and the process-wide [`SOCKET`] mutex
/// it holds) indefinitely.
///
/// Takes `response` by value (rather than `&Response`) so a large payload can
/// move into the [`spawn_blocking`](tokio::task::spawn_blocking) branch below
/// without a clone — every call site already owns a freshly built `Response`
/// it doesn't reuse afterwards.
///
/// # `encode_response` off-thread for large responses (#1065)
///
/// A `grants`/`departures`/`calendar` response can carry thousands of rows —
/// [`response_row_count`] at or above [`LARGE_RESPONSE_ROWS`] means
/// `serde_json::to_string` costs real time (≥~1 ms at this crate's measured
/// rate — see that constant's doc), which, run inline like the write itself
/// used to be, would stall every other timer on this session's
/// current-thread runtime for the duration, the same starvation class #1059
/// fixed for `GrantStore::load`. Below the threshold `encode_response` runs
/// inline: the overwhelming majority of responses (`auth`, a single `get`)
/// are a handful of scalar fields, and `spawn_blocking`'s own scheduling hop
/// isn't free either.
async fn write_response(stream: &mut UnixStream, response: Response) {
    write_response_with_encoder(stream, response, encode_response).await;
}

/// [`write_response`], with the (potentially offloaded) encode step supplied
/// by the caller — the test seam for the property this function exists to
/// buy: "a slow `encode_response` does not stall a concurrent timer on this
/// runtime". Production is just `write_response`, i.e.
/// `write_response_with_encoder(stream, response, encode_response)`;
/// `tests::write_response_offloads_a_slow_encode_of_a_large_response_without_delaying_a_concurrent_timer`
/// injects one that blocks for two seconds instead — same shape as
/// `serve_with_grant_loader`'s injected loader (#1059) and
/// `grants::GrantStore::save_with`'s injected writer (#1065), one seam per
/// offloaded step.
async fn write_response_with_encoder<E>(stream: &mut UnixStream, response: Response, encode: E)
where
    E: FnOnce(&Response) -> String + Send + 'static,
{
    let mut out = if response_row_count(&response) >= LARGE_RESPONSE_ROWS {
        match tokio::task::spawn_blocking(move || encode(&response)).await {
            Ok(out) => out,
            Err(e) => {
                tracing_eprintln(&format!("encode_response panicked: {e}"));
                r#"{"ok":false,"error":"broker: response encode failed"}"#.to_owned()
            }
        }
    } else {
        encode(&response)
    };
    out.push('\n');
    let write = async {
        stream.write_all(out.as_bytes()).await?;
        stream.flush().await
    };
    match tokio::time::timeout(WRITE_RESPONSE_TIMEOUT, write).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing_eprintln(&format!("writing response failed: {e}")),
        Err(_) => tracing_eprintln(&format!(
            "writing response timed out after {WRITE_RESPONSE_TIMEOUT:?}; dropping the connection"
        )),
    }
}

// ── Consent parking (#487 phase 1b) ───────────────────────────────────────────

/// How a `get <datasource>` resolves (#509): answered inline (calendar / a
/// denial), or routed through a provider plugin (departures / weather).
// `Answer` carries a full `Response` — the common case (it IS the reply), not a
// rare large variant — so boxing it would just allocate on the hot path.
#[allow(clippy::large_enum_variant)]
enum GetDisposition {
    /// A ready response (+ optional toast) to write back immediately.
    Answer(Response, Option<Toast>),
    /// Route to the datasource's provider plugin over the host query protocol.
    Query {
        datasource: String,
        scope: String,
        params: String,
    },
}

/// The result of [`BrokerState::handle_conn`]: either the response is already
/// written, or the request is parked awaiting a human consent decision (#487) or a
/// routed datasource query answer (#509).
enum ConnResult {
    /// The response has been written; carries an optional toast to raise.
    Answered { toast: Option<Toast> },
    /// The request needs consent: the stream is handed back to [`serve`] to park
    /// until the decision (or [`CONSENT_PARK_TIMEOUT`]) resolves it.
    NeedsConsent {
        agent: String,
        datasource: String,
        scope: String,
        detail: String,
        stream: UnixStream,
    },
    /// The request needs a routed datasource query (#509): the stream is parked
    /// until the plugin relays a `Cmd::QueryResult` (or [`QUERY_PARK_TIMEOUT`]).
    NeedsQuery {
        datasource: String,
        scope: String,
        params: String,
        stream: UnixStream,
    },
}

/// A dispatched request's disposition, before the stream is written or parked.
// `Answer` carries a full `Response` (the common case, not a rare large variant),
// so the large-variant delta is expected — same as [`GetDisposition`].
#[allow(clippy::large_enum_variant)]
enum Dispatch {
    /// A ready response (+ optional toast) to write back immediately.
    Answer(Response, Option<Toast>),
    /// The request needs a consent decision; [`serve`] parks the connection.
    Consent {
        agent: String,
        datasource: String,
        scope: String,
        detail: String,
    },
    /// The request needs a routed datasource query (#509); [`serve`] parks it.
    Query {
        datasource: String,
        scope: String,
        params: String,
    },
}

/// A socket request parked awaiting a consent decision (#487 phase 1b): the
/// stream to answer on, the `(agent, datasource)` the decision applies to, and
/// the fallback deadline.
struct PendingConsent {
    agent: String,
    datasource: String,
    stream: UnixStream,
    deadline: tokio::time::Instant,
}

/// A socket request parked awaiting a routed datasource query answer (#509): the
/// stream to answer on, the `datasource` (to shape the response from the provider's
/// opaque payload), and the fallback deadline.
struct PendingQuery {
    datasource: String,
    stream: UnixStream,
    deadline: tokio::time::Instant,
}

/// Push a fresh panel snapshot (+ optional toast) to the plugin.
fn send_update(
    out: &mpsc::UnboundedSender<BrokerMsg>,
    snapshot: BrokerSnapshot,
    toast: Option<Toast>,
) {
    let _ = out.send(BrokerMsg::Update { snapshot, toast });
}

/// Sleep until the nearest parked deadline, or forever when nothing is parked —
/// the timeout arm of [`serve`]'s `select!`.
async fn wait_for_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

// ── The socket server ─────────────────────────────────────────────────────────

/// The socket this **process** owns, bound at most once and kept for the
/// process's whole life (#995 follow-up).
///
/// Two jobs, and both are the fix for the same bug. [`serve`] runs once per SDK
/// *session*, and a host restart ends session N and starts session N+1 after
/// `BACKOFF_BASE` = 100 ms — while session N's `serve` can still be parked
/// inside `handle_conn` for up to `REQUEST_TIMEOUT` = 5 s, listener still open.
/// A per-session bind therefore had the new session probing its **own
/// predecessor's** listener, concluding "another broker is live", and standing
/// down for the rest of the process — killing the socket, the panel and the
/// command lane on an ordinary `systemctl --user restart trollshell`.
///
/// So: the listener lives here rather than in a session, and a session that
/// already finds one **reuses it without probing** — a process never probes
/// against itself. And the `Mutex` is held for `serve`'s whole body, which
/// orders the two overlapping sessions: N+1 waits for N to finish before it
/// touches the socket, instead of the two racing over one listener. The socket
/// therefore stays up across the handover, where it used to be rebound (and
/// briefly refused) on every session.
static SOCKET: tokio::sync::Mutex<Option<UnixListener>> = tokio::sync::Mutex::const_new(None);

/// Set while [`serve`] is standing down for a *foreign* live broker (#995), so
/// the explanation is logged once per stand-down streak instead of once per SDK
/// redial (≤5 s apart, forever, for as long as the duplicate runs). Cleared
/// again by a successful bind ([`clear_stand_down`]), so a later genuine
/// duplicate is not silenced by an earlier episode.
static STOOD_DOWN: AtomicBool = AtomicBool::new(false);

/// Whether this stand-down should log: true exactly once per streak. Takes the
/// latch as an argument so the decision is testable without touching the
/// process-wide [`STOOD_DOWN`].
fn stand_down_once(latch: &AtomicBool) -> bool {
    !latch.swap(true, Ordering::Relaxed)
}

/// Reset the [`stand_down_once`] latch — called on a successful bind, so the
/// *next* stand-down streak explains itself again.
fn clear_stand_down(latch: &AtomicBool) {
    latch.store(false, Ordering::Relaxed);
}

/// Probe whether a live broker already owns the socket (#995). The plugin-side
/// twin of the host's `trollshell::plugins::listener::socket_in_use`: a
/// successful connect means another `hytte-plugin-infobroker` process is
/// serving the path, so this one must stand down rather than unlink a working
/// socket. A refused/failed connect means a stale socket file (a previous run
/// left it) or no file at all — safe to reclaim. The probe connection is
/// dropped immediately without sending a line, so the incumbent's accept loop
/// reads EOF and reaps it on the next poll.
async fn socket_in_use(path: &Path) -> bool {
    UnixStream::connect(path).await.is_ok()
}

/// The outcome of trying to take the broker socket (#995).
enum BindOutcome {
    /// This process owns the socket.
    Bound(UnixListener),
    /// A live broker already answers on the path. Stand down: do NOT unlink it.
    StoodDown,
}

/// Take the broker socket: probe for a live incumbent, then unlink any *stale*
/// socket, bind, and tighten to `0600` (same-user-only, exactly like the host's
/// own plugin socket). The parent is `$XDG_RUNTIME_DIR`, already `0700`.
///
/// The probe is the whole point (#995). This runs from
/// [`crate::plugin`]'s `sources()`, which the SDK calls **after** it writes
/// `Register` but **before** it reads a single host frame — so it runs even for
/// a duplicate the host is about to reject on its `IdGuard`. The old
/// unconditional `remove_file` → `bind` therefore destroyed the *incumbent*
/// broker's socket on every one of the duplicate's ≤5 s redials: the incumbent
/// kept a live listener on an unlinked inode, the path was left holding the
/// duplicate's soon-dead socket, and every `hytte-infobroker` CLI dial got
/// `ECONNREFUSED` while both processes logged success.
async fn bind_socket(path: &Path) -> std::io::Result<BindOutcome> {
    if socket_in_use(path).await {
        return Ok(BindOutcome::StoodDown);
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(BindOutcome::Bound(listener))
}

/// What one session's attempt to take the broker socket produced.
#[derive(Debug, PartialEq, Eq)]
enum SocketClaim {
    /// This process serves the socket — it bound it just now.
    Bound,
    /// This process already owned the socket from an earlier session and keeps
    /// it. **No probe was made**: a process must never probe its own listener
    /// (see [`SOCKET`]).
    Kept,
    /// A *foreign* live broker answers on the path. This session serves no
    /// socket — but it still runs, so the panel and the command lane live.
    StoodDown,
}

/// Take the broker socket for one session, given the socket this process
/// already owns (`owned`, normally [`SOCKET`]'s contents).
///
/// The order matters and is the whole fix: **own first, probe second**. If
/// `owned` already holds a listener this session simply keeps it
/// ([`SocketClaim::Kept`]) — the previous session's listener is this process's
/// listener, and probing it would be probing ourselves. Only a process with no
/// listener at all probes, and only *that* probe can legitimately find a
/// foreign incumbent.
async fn take_socket(
    path: &Path,
    owned: &mut Option<UnixListener>,
) -> std::io::Result<SocketClaim> {
    if owned.is_some() {
        return Ok(SocketClaim::Kept);
    }
    match bind_socket(path).await? {
        BindOutcome::Bound(listener) => {
            *owned = Some(listener);
            Ok(SocketClaim::Bound)
        }
        BindOutcome::StoodDown => Ok(SocketClaim::StoodDown),
    }
}

/// The accept arm of [`serve`]'s `select!`: a real `accept()` when this session
/// owns the socket, and a future that never resolves when it does not — the
/// same "park forever" shape as [`wait_for_deadline`]`(None)`.
///
/// A session without a socket used to `return` outright, which also dropped the
/// command receiver and skipped the seed snapshot: the duplicate's panel stayed
/// empty and its buttons dead, with the reason only in the journal. Parking
/// this one arm keeps every other lane alive.
async fn accept_or_park(
    listener: Option<&UnixListener>,
) -> std::io::Result<(UnixStream, tokio::net::unix::SocketAddr)> {
    match listener {
        Some(l) => l.accept().await,
        None => std::future::pending().await,
    }
}

/// Run the broker: load the durable grants, take the socket, then loop serving
/// client connections and panel commands until the command lane closes (the
/// plugin session tearing down). The `BrokerState` — and so every in-memory
/// token — is built fresh here, which is what drops session tokens on a shell
/// restart; the *socket* is not, see [`SOCKET`].
///
/// "Take", not "bind", in two senses (#995). A session that finds this process
/// already owns the socket keeps it and never probes — probing would mean
/// probing our own previous session's listener, which is exactly the
/// false-positive stand-down that killed the panel on a host restart. And a
/// process with no socket that finds a *foreign* live broker stands down
/// instead of unlinking it, because this function runs from `sources()` before
/// the host has accepted the session and so also runs in a duplicate the host
/// is about to reject.
///
/// A session without a socket **still runs**: it seeds the panel (with a
/// [`BrokerSnapshot::notice`] saying why it is not serving) and keeps draining
/// the command lane. Returning early instead left the duplicate's chip painting
/// a default snapshot with dead buttons.
///
/// SDK-free: `cmds`/`out` are plain tokio channels (the plugin passes the SDK's
/// per-session lane ends, which are exactly these types), so this whole module
/// never links the plugin runtime.
pub async fn serve(cmds: mpsc::UnboundedReceiver<Cmd>, out: mpsc::UnboundedSender<BrokerMsg>) {
    serve_with_grant_loader(cmds, out, load_grants).await;
}

/// Read and parse the durable grant store for one session: `grants.toml` when
/// there is a state dir to hold it, an empty in-memory store when there is not
/// (no `HOME`/`XDG_STATE_HOME`) or when the file on disk is unreadable.
///
/// **Wholly synchronous** — a `std::fs::read_to_string` plus a TOML parse, with
/// no `.await` anywhere inside it — which is the entire reason
/// [`serve_with_grant_loader`] hands it to `tokio::task::spawn_blocking`
/// instead of calling it inline (#1059).
fn load_grants() -> GrantStore {
    paths::grants_path().map_or_else(
        || {
            tracing_eprintln("no HOME/XDG_STATE_HOME; grants are in-memory only this session");
            GrantStore::from_grants(Vec::new())
        },
        |path| {
            GrantStore::load(&path).unwrap_or_else(|e| {
                tracing_eprintln(&format!("grant store unreadable ({e}); starting empty"));
                GrantStore::from_grants(Vec::new())
            })
        },
    )
}

/// [`serve`], with the synchronous grant-load step supplied by the caller.
/// `serve` *is* `serve_with_grant_loader(cmds, out, load_grants)`; everything
/// [`serve`]'s doc says applies here unchanged.
///
/// # Why the seam is public (#1059)
///
/// The property this parameter exists to test is "a slow synchronous step
/// inside `serve` does not stop the runtime's timers", and it is invisible
/// unless the step is slow. Production's step is fast unless the user's
/// `grants.toml` is enormous, and a test cannot make it slow without writing
/// hundreds of megabytes; nor can an integration test point `grants_path()` at
/// a scratch file in-process, because `std::env::set_var` is `unsafe` in
/// edition 2024 and this workspace `forbid`s `unsafe_code` (see
/// `tests/serve_socket_handover.rs`'s module doc). So the loader is injected,
/// and `tests/serve_socket_handover.rs`'s scenario D injects one that blocks
/// for two seconds and then asserts a concurrent 100 ms timer still fired on
/// time.
///
/// # The starvation this guards against
///
/// `serve` runs from [`crate::plugin`]'s `sources()` under a plain
/// `tokio::spawn` on the SDK's **current-thread** runtime
/// (`hytte-plugin/src/runtime.rs`), which is deliberate — the broker is one
/// plugin session's I/O source, not a second daemon (contrast
/// `hytte-claude-bridge`, whose HTTP listener owns its own multi-thread
/// runtime because its clients are *other* plugins). On a current-thread
/// runtime a synchronous step holds the only thread there is, so while it runs
/// nothing else on that runtime is polled: the broker's own `REQUEST_TIMEOUT`
/// / `CONSENT_PARK_TIMEOUT` / `QUERY_PARK_TIMEOUT` bounds, the SDK's clock
/// pump and the SDK's session loop all stall together. Measured on #1024's
/// tree: a 5 s `tokio::time::timeout` returning `Ok` at 7.310 s, because the
/// inline `GrantStore::load` had the thread for the intervening seconds.
///
/// `spawn_blocking` moves exactly that step to tokio's blocking pool (already
/// a dependency-level given — the `rt` feature this crate enables provides
/// both `spawn` and `spawn_blocking`), leaving the session's own thread free
/// to keep polling. It is the *step* that moves, not the runtime: `serve`
/// itself stays on the SDK's current-thread runtime, so the socket, the
/// listener and every parked request keep living exactly where they did.
///
/// The remaining synchronous work in this module is deliberately left inline
/// and is a different class: `bind_socket`'s `remove_file`/`bind`/
/// `set_permissions` are three syscalls on a tmpfs, not an unbounded parse.
/// `GrantStore::save` (reached from `apply_cmd`/`apply_consent`) *was* left
/// inline for the same reason as this module's own `encode_response` — a
/// per-click/per-request cost, not per-session-start, and out of #1059's
/// lane — but #1065 closed both: `save` now serializes under `state`'s
/// mutable borrow and hands the bytes to a detached `spawn_blocking` write
/// (see [`crate::grants::GrantStore::save`]), and `write_response` offloads
/// `encode_response` past [`LARGE_RESPONSE_ROWS`] the same way.
// One cohesive `select!` loop (accept / command / timeout) over the parked-request
// state (consent + query maps); splitting its arms into helpers would scatter that
// shared state for no readability gain — same stance as the host's `handle_conn`.
#[allow(clippy::too_many_lines)]
#[doc(hidden)]
pub async fn serve_with_grant_loader<L>(
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    out: mpsc::UnboundedSender<BrokerMsg>,
    load_grants: L,
) where
    L: FnOnce() -> GrantStore + Send + 'static,
{
    let Some(sock) = paths::socket_path() else {
        tracing_eprintln("XDG_RUNTIME_DIR unset; broker socket not created");
        return;
    };

    // #1059: off the session's own thread — see this function's doc comment.
    // A panicking loader is a bug in the loader, not a reason to take the
    // broker down with it: log it and serve with an empty store, the same
    // fallback an unreadable `grants.toml` already gets.
    let grants = match tokio::task::spawn_blocking(load_grants).await {
        Ok(grants) => grants,
        Err(e) => {
            tracing_eprintln(&format!("grant load failed ({e}); starting empty"));
            GrantStore::from_grants(Vec::new())
        }
    };
    let mut state = BrokerState::new(grants);

    // Hold the process's socket for this session's whole body: that is what
    // orders an outgoing session against the incoming one (see `SOCKET`), and
    // what lets `accept()` borrow the listener out of the guard.
    let mut socket = SOCKET.lock().await;
    match take_socket(&sock, &mut socket).await {
        Ok(SocketClaim::Bound) => {
            clear_stand_down(&STOOD_DOWN);
            tracing_eprintln(&format!("listening on {}", sock.display()));
        }
        Ok(SocketClaim::Kept) => {
            // The listener this process bound in an earlier session. Nothing to
            // probe, nothing to rebind — the socket never went down.
            clear_stand_down(&STOOD_DOWN);
        }
        Ok(SocketClaim::StoodDown) => {
            // #995: a *foreign* broker is live on the path. Standing down means
            // this session has no socket server — but it keeps running, so the
            // panel says why and the command lane stays alive. The SDK redials
            // at ≤5 s, so log ONCE per streak rather than once per redial.
            if stand_down_once(&STOOD_DOWN) {
                tracing_eprintln(&format!(
                    "{} already has a live broker listening; standing down rather than \
                     unlinking it (another hytte-plugin-infobroker is running — stop it, or \
                     stop this one). Further stand-downs are silent until this one binds.",
                    sock.display()
                ));
            }
            state.notice = Some(format!(
                "Not serving: another info broker already owns {}. Stop the other one \
                 (or this one) — grants and sessions shown here are the ones this process \
                 has, which is nothing.",
                sock.display()
            ));
        }
        Err(e) => {
            // The bind itself failed (a permission/EADDRINUSE surprise). Same
            // deal: no socket, but the session lives and the panel says so.
            tracing_eprintln(&format!("failed to bind {}: {e}", sock.display()));
            state.notice = Some(format!(
                "Not serving: binding {} failed ({e}).",
                sock.display()
            ));
        }
    }
    let listener = socket.as_ref();

    // Seed the panel with the current grants/status before any request — and,
    // when there is no socket, with the reason.
    send_update(&out, state.snapshot(now_unix()), None);

    // Parked consent requests (#487 phase 1b), keyed by the request_id the broker
    // minted; each holds the client stream open until its decision or deadline.
    let mut pending: HashMap<u64, PendingConsent> = HashMap::new();
    // Parked datasource queries (#509), keyed by the same minted `request_id`
    // counter; each holds the client stream open until the plugin relays a
    // `Cmd::QueryResult` (or the query deadline elapses).
    let mut pending_queries: HashMap<u64, PendingQuery> = HashMap::new();
    let mut next_request_id: u64 = 1;

    loop {
        // The nearest parked deadline (consent OR query) drives the timeout arm;
        // `None` = park forever.
        let next_deadline = pending
            .values()
            .map(|p| p.deadline)
            .chain(pending_queries.values().map(|q| q.deadline))
            .min();
        tokio::select! {
            // Prefer draining commands (revoke / allow / a consent decision) so a
            // click — or an answer to a live prompt — lands promptly rather than
            // behind a slow fetch.
            biased;
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    break; // lane closed → session teardown
                };
                match cmd {
                    Cmd::Revoke { .. } | Cmd::Allow { .. } | Cmd::Calendar(_) => {
                        state.apply_cmd(cmd);
                        send_update(&out, state.snapshot(now_unix()), None);
                    }
                    Cmd::Decision { request_id, decision } => {
                        if let Some(mut p) = pending.remove(&request_id) {
                            let (resp, toast) =
                                state.apply_consent(&p.agent, &p.datasource, decision, now_unix());
                            write_response(&mut p.stream, resp).await;
                            send_update(&out, state.snapshot(now_unix()), toast);
                        }
                        // else: a late/unknown decision (already timed out) — ignore.
                    }
                    // #509: a routed query's answer — shape the response from the
                    // provider's payload and write it back to the parked client.
                    Cmd::QueryResult { request_id, outcome } => {
                        if let Some(mut q) = pending_queries.remove(&request_id) {
                            let resp = query_response(&q.datasource, outcome);
                            write_response(&mut q.stream, resp).await;
                        }
                        // else: a late/unknown result (already timed out) — ignore.
                    }
                }
            }
            accepted = accept_or_park(listener) => {
                match accepted {
                    Ok((stream, _addr)) => match state.handle_conn(stream).await {
                        ConnResult::Answered { toast } => {
                            send_update(&out, state.snapshot(now_unix()), toast);
                        }
                        ConnResult::NeedsConsent { agent, datasource, scope, detail, stream } => {
                            let request_id = next_request_id;
                            next_request_id = next_request_id.wrapping_add(1);
                            pending.insert(request_id, PendingConsent {
                                agent: agent.clone(),
                                datasource: datasource.clone(),
                                stream,
                                deadline: tokio::time::Instant::now() + CONSENT_PARK_TIMEOUT,
                            });
                            // Ask the shell to prompt the human; the request stays
                            // parked until the answering `Cmd::Decision` arrives.
                            let _ = out.send(BrokerMsg::RequestConsent(ConsentPrompt {
                                request_id, agent, datasource, scope, detail,
                            }));
                            send_update(&out, state.snapshot(now_unix()), None);
                        }
                        // #509: a routed datasource query — park the client and ask
                        // the plugin to route it to the shell host; the answering
                        // `Cmd::QueryResult` (or the deadline) resolves it.
                        ConnResult::NeedsQuery { datasource, scope, params, stream } => {
                            let request_id = next_request_id;
                            next_request_id = next_request_id.wrapping_add(1);
                            pending_queries.insert(request_id, PendingQuery {
                                datasource: datasource.clone(),
                                stream,
                                deadline: tokio::time::Instant::now() + QUERY_PARK_TIMEOUT,
                            });
                            let _ = out.send(BrokerMsg::Query(QueryRequest {
                                request_id, provider: datasource, scope, params,
                            }));
                        }
                    },
                    Err(e) => tracing_eprintln(&format!("accept error: {e}")),
                }
            }
            () = wait_for_deadline(next_deadline) => {
                // The bound elapsed on the earliest parked request(s): time them
                // out. A consent knock times out with a transient deny + the 1a
                // toast (the fallback path); a routed query with a transient error.
                let now_inst = tokio::time::Instant::now();
                let expired: Vec<u64> = pending
                    .iter()
                    .filter(|(_, p)| p.deadline <= now_inst)
                    .map(|(id, _)| *id)
                    .collect();
                for id in expired {
                    if let Some(mut p) = pending.remove(&id) {
                        let (resp, toast) =
                            state.on_consent_timeout(&p.agent, &p.datasource, now_unix());
                        write_response(&mut p.stream, resp).await;
                        send_update(&out, state.snapshot(now_unix()), toast);
                    }
                }
                // #509: expire parked queries the plugin never answered (a wedged /
                // pre-#509 host). A transient error — the agent may retry.
                let expired_q: Vec<u64> = pending_queries
                    .iter()
                    .filter(|(_, q)| q.deadline <= now_inst)
                    .map(|(id, _)| *id)
                    .collect();
                for id in expired_q {
                    if let Some(mut q) = pending_queries.remove(&id) {
                        let resp = Response::error(format!(
                            "{}: datasource query timed out (no host response)",
                            q.datasource
                        ));
                        write_response(&mut q.stream, resp).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::Grant;
    use tokio::io::AsyncReadExt as _;

    fn store(grants: Vec<Grant>) -> GrantStore {
        GrantStore::from_grants(grants)
    }

    /// #995: the socket is taken, not seized. A duplicate broker — which the
    /// SDK starts through `sources()` *before* the host has accepted or
    /// rejected its session, so this path runs on every ≤5 s redial of a
    /// process the host is rejecting — must leave the incumbent's socket
    /// exactly where it is. A *stale* socket file is still reclaimed, so a
    /// normal restart rebinds.
    ///
    /// The inode assertion is the load-bearing one: before the fix
    /// `bind_socket` unlinked unconditionally, so the path's inode changed and
    /// the incumbent was left listening on an inode nothing could name.
    /// Hermetic: a scratch dir, no daemons, no `XDG_RUNTIME_DIR`.
    #[tokio::test]
    async fn bind_socket_stands_down_on_a_live_broker_and_reclaims_a_stale_one() {
        use std::os::unix::fs::MetadataExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hytte-infobroker.sock");

        // Nothing on the path yet: the incumbent binds.
        let BindOutcome::Bound(incumbent) = bind_socket(&path).await.expect("first bind") else {
            panic!("an absent socket must be bound, not stood down");
        };
        let incumbent_inode = std::fs::metadata(&path).expect("socket exists").ino();

        // The duplicate stands down — and does NOT touch the incumbent's inode.
        assert!(
            matches!(
                bind_socket(&path).await.expect("duplicate completes"),
                BindOutcome::StoodDown
            ),
            "a live broker on the path makes the duplicate stand down",
        );
        assert_eq!(
            std::fs::metadata(&path).expect("socket still exists").ino(),
            incumbent_inode,
            "the duplicate never unlinks the incumbent's socket",
        );

        // ...and the incumbent still answers on the path, which is what the
        // unconditional unlink actually broke for every CLI dial.
        let (client, accepted) = tokio::join!(UnixStream::connect(&path), incumbent.accept());
        client.expect("a client can still dial the path");
        accepted.expect("the dial lands on the incumbent's listener");
        drop(incumbent);

        // A stale socket file (listener gone) is reclaimable, so a normal
        // restart is not wedged by its predecessor's leftovers.
        assert!(path.exists(), "the stale socket file is still on disk");
        assert!(
            matches!(
                bind_socket(&path).await.expect("reclaim completes"),
                BindOutcome::Bound(_)
            ),
            "a stale socket is reclaimed",
        );
    }

    /// #995 follow-up: a **second session of the same process** must keep the
    /// socket its predecessor bound, not probe it.
    ///
    /// The sequence is the one a `systemctl --user restart trollshell` produces
    /// and it is not rare: the SDK ends session N, waits `BACKOFF_BASE` = 100 ms,
    /// redials and calls `sources()` → a fresh `serve` — while session N's
    /// `serve` can still be parked in `handle_conn` for up to `REQUEST_TIMEOUT`
    /// = 5 s with its listener open. Probing there answers "live" against **our
    /// own listener**, and the old code took that as a duplicate: it stood down,
    /// returned before the seed `send_update`, dropped the new session's command
    /// receiver, and logged "another hytte-plugin-infobroker is running" when
    /// there was exactly one. Only another restart recovered it.
    ///
    /// So the claim is decided by ownership *before* any probe: with a listener
    /// already owned the answer is [`SocketClaim::Kept`], the inode is untouched
    /// and clients keep being served straight through the handover.
    #[tokio::test]
    async fn a_second_session_keeps_the_process_listener_instead_of_probing_itself() {
        use std::os::unix::fs::MetadataExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hytte-infobroker.sock");
        // Stands in for the process-wide `SOCKET`, so the test drives the real
        // decision without touching a static shared with the other tests.
        let mut owned: Option<UnixListener> = None;

        // Session 1 binds.
        assert_eq!(
            take_socket(&path, &mut owned).await.expect("first take"),
            SocketClaim::Bound,
            "an unowned, unclaimed path is bound",
        );
        let inode = std::fs::metadata(&path).expect("socket exists").ino();
        assert!(owned.is_some(), "the process now owns the listener");

        // Session 1 has ended, but its listener is still open (its `handle_conn`
        // is parked) — so the path IS live. Session 2 starts inside that window.
        assert!(
            socket_in_use(&path).await,
            "precondition: the predecessor's listener still answers, which is what \
             the old code misread as a foreign duplicate",
        );
        assert_eq!(
            take_socket(&path, &mut owned).await.expect("second take"),
            SocketClaim::Kept,
            "a session must not stand down against its own process's listener",
        );
        assert_eq!(
            std::fs::metadata(&path).expect("socket still exists").ino(),
            inode,
            "the handover neither unlinks nor rebinds the socket",
        );

        // ...and the socket never went down across the handover.
        let listener = owned.as_ref().expect("the process still owns the listener");
        let (client, accepted) = tokio::join!(UnixStream::connect(&path), listener.accept());
        client.expect("a client can dial straight through the session handover");
        accepted.expect("the dial lands on the kept listener");
    }

    /// The stand-down log latch (#995): one line per stand-down *streak*, not
    /// one per ≤5 s SDK redial — and a successful bind rearms it, so a later
    /// genuine duplicate still explains itself instead of being silenced by an
    /// earlier episode. Takes its own latch, so it never races the process-wide
    /// `STOOD_DOWN` with the tests running in parallel.
    #[test]
    fn the_stand_down_line_is_logged_once_per_streak_and_rearms_on_a_bind() {
        let latch = AtomicBool::new(false);

        assert!(
            stand_down_once(&latch),
            "the first stand-down explains itself"
        );
        assert!(
            !stand_down_once(&latch),
            "the redials behind it are silent — the SDK retries every ≤5 s forever",
        );
        assert!(!stand_down_once(&latch), "...and stay silent");

        clear_stand_down(&latch);
        assert!(
            stand_down_once(&latch),
            "a successful bind rearms the line, so a later real duplicate is not silenced",
        );
    }

    /// #1024 N3/N4: `write_response` must not park its caller forever when the
    /// peer never reads. `UnixStream::pair()` gives a connected pair with no
    /// listener/socket file involved; the response is built oversized on
    /// purpose (thousands of grant rows — hundreds of KB of JSON) so the
    /// write genuinely fills the kernel buffer and blocks, rather than landing
    /// entirely in slack and returning instantly regardless of the bound.
    ///
    /// The outer `tokio::time::timeout` turns "the internal bound was deleted"
    /// into a failing assertion instead of a test binary that hangs forever.
    ///
    /// #1024 review New-2: `started` is taken before `write_response`, and
    /// `encode_response` runs *inside* it (before the write, and before the
    /// 2 s block can even begin), so the row count feeds this test's upper
    /// bound too, not just how hard the write blocks. Was `100_000` rows,
    /// which measured 2.82–3.46 s wall under ~2x CPU oversubscription against
    /// a 4 s ceiling (`WRITE_RESPONSE_TIMEOUT` + 2 s) — survived 100/100
    /// whole-binary runs there, but on a margin note, not a repro. `10_000`
    /// rows (~811 KiB, still >2× the default UDS buffer) keeps the write
    /// genuinely blocking (still RED with the timeout deleted) while cutting
    /// the encode cost this bound has to absorb.
    ///
    /// #1033 third pass / #1059 item 3: that figure read "~95 KB" until now
    /// and was wrong by 8.5×. Each row encodes to exactly 82 bytes
    /// (`{"agent":"agent-000000","datasource":"departures","scope":"*",
    /// "decision":"always"}`) plus its separating comma, so 10 000 of them are
    /// ~830 000 B ≈ 811 KiB — ~3.9× one `wmem_default`/`rmem_default`
    /// (212 992 B on a typical kernel) and ~1.95× the send+receive pair. The
    /// ">2×" claim the wrong number was attached to happens to survive the
    /// correction; the number itself did not.
    #[tokio::test]
    async fn write_response_gives_up_on_a_client_that_never_reads() {
        let (mut server, _client_that_never_reads) =
            UnixStream::pair().expect("a connected socketpair needs no listener at all");

        let huge = Response {
            ok: true,
            grants: Some(
                (0..10_000)
                    .map(|i| GrantOut {
                        agent: format!("agent-{i:06}"),
                        datasource: "departures".to_owned(),
                        scope: "*".to_owned(),
                        decision: "always".to_owned(),
                    })
                    .collect(),
            ),
            ..Response::default()
        };

        // #1065: this fixture's 10_000 rows stays below `LARGE_RESPONSE_ROWS`
        // on purpose (see the `const _: () = assert!(...)` guard above
        // `LARGE_RESPONSE_ROWS`'s definition), so `encode_response` still runs
        // inline here and the timing math below is unaffected by that change.
        let started = std::time::Instant::now();
        tokio::time::timeout(
            WRITE_RESPONSE_TIMEOUT + std::time::Duration::from_secs(5),
            write_response(&mut server, huge),
        )
        .await
        .expect(
            "write_response must give up on its own within its bound, not hang until this \
             test's outer safety net — RED if the internal timeout is removed",
        );
        assert!(
            started.elapsed() < WRITE_RESPONSE_TIMEOUT + std::time::Duration::from_secs(2),
            "write_response returned, but not within its own bound (took {:?}) — the write may \
             not actually have blocked; widen the payload if this becomes flaky",
            started.elapsed(),
        );
        // #1024 review L1: the write is only meaningful proof of the bound if
        // it actually blocked long enough to hit it. Without this lower
        // bound, a payload too small to fill the kernel buffer (or the
        // internal timeout wrapper deleted outright — see mutation E) both
        // return near-instantly and the test above stays green either way.
        assert!(
            started.elapsed() >= WRITE_RESPONSE_TIMEOUT,
            "write_response returned in {:?}, before its own timeout ({WRITE_RESPONSE_TIMEOUT:?}) \
             could have fired — the write may never have actually blocked, so this test cannot \
             tell \"correctly bounded\" from \"never blocked at all\"; widen the payload if this \
             fires without a mutation",
            started.elapsed(),
        );
    }

    /// The #1065 property, the #1059 way: an injected **slow**
    /// `encode_response` step must not delay a concurrent 100 ms timer on the
    /// same current-thread runtime — same methodology as
    /// `grants::tests::save_offloads_a_slow_writer_without_delaying_a_concurrent_timer`,
    /// one offloaded step over (`write_response_with_encoder`'s injected
    /// `encode` versus `GrantStore::save_with`'s injected `writer`).
    ///
    /// The peer drains its side so the *write* half (already covered by
    /// `write_response_gives_up_on_a_client_that_never_reads` above) isn't
    /// what this test is measuring — only the encode step is meant to be slow
    /// here.
    #[tokio::test]
    async fn write_response_offloads_a_slow_encode_of_a_large_response_without_delaying_a_concurrent_timer()
     {
        let (mut server, mut client) =
            UnixStream::pair().expect("a connected socketpair needs no listener at all");
        let drain = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = client.read_to_end(&mut buf).await;
        });

        // At `LARGE_RESPONSE_ROWS`, so `write_response_with_encoder` takes the
        // `spawn_blocking` branch — the property under test only holds there.
        let huge = Response {
            ok: true,
            grants: Some(
                (0..LARGE_RESPONSE_ROWS)
                    .map(|i| GrantOut {
                        agent: format!("agent-{i:06}"),
                        datasource: "departures".to_owned(),
                        scope: "*".to_owned(),
                        decision: "always".to_owned(),
                    })
                    .collect(),
            ),
            ..Response::default()
        };

        let start = std::time::Instant::now();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            start.elapsed()
        });

        // The injected "encode": two seconds of `std::thread::sleep` standing
        // in for a pathologically slow serialization. Real `encode_response`
        // never sleeps.
        write_response_with_encoder(&mut server, huge, |resp| {
            std::thread::sleep(std::time::Duration::from_secs(2));
            encode_response(resp)
        })
        .await;

        drop(server); // lets the drain task observe EOF
        let elapsed = timer
            .await
            .expect("the concurrent 100ms timer task must not panic");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "the 100ms timer fired at {elapsed:?} — the injected 2s encode stalled this \
             runtime's other tasks, so large responses are not actually encoded off-thread",
        );
        drain.await.expect("drain task must not panic");
    }

    #[test]
    fn authorize_auth_grants_denies_or_prompts() {
        // An `always` grant → silent mint.
        let s = store(vec![Grant::always("claude", "departures")]);
        assert_eq!(authorize_auth(&s, "claude"), AuthOutcome::Granted);
        // A stranger with no standing grant now needs a consent decision (#487).
        assert_eq!(authorize_auth(&s, "stranger"), AuthOutcome::NeedsConsent);
    }

    #[test]
    fn auth_denied_silently_for_a_deny_only_agent() {
        // A standing `deny` is a settled "no": denied with an actionable hint, and
        // — unlike a no-grant knock — NOT re-prompted for consent.
        let s = store(vec![Grant {
            agent: "scratch".to_owned(),
            datasource: "departures".to_owned(),
            scope: "*".to_owned(),
            decision: Decision::Deny,
        }]);
        let AuthOutcome::Denied { hint } = authorize_auth(&s, "scratch") else {
            panic!("a standing deny refuses without prompting");
        };
        assert!(
            hint.contains("grants.toml"),
            "hint points at the grant surface: {hint}"
        );
        assert!(hint.contains("infobroker panel"));
    }

    #[test]
    fn get_allowed_only_on_an_always_grant_for_that_datasource() {
        let s = store(vec![Grant::always("claude", "departures")]);
        assert_eq!(
            authorize_get(&s, "claude", "departures"),
            GetOutcome::Allowed
        );
        // Same agent, different datasource → denied.
        assert!(matches!(
            authorize_get(&s, "claude", "weather"),
            GetOutcome::Denied { .. }
        ));
        // Unknown agent → denied.
        assert!(matches!(
            authorize_get(&s, "stranger", "departures"),
            GetOutcome::Denied { .. }
        ));
    }

    #[test]
    fn get_denied_on_an_explicit_deny() {
        let s = store(vec![Grant {
            agent: "scratch".to_owned(),
            datasource: "departures".to_owned(),
            scope: "*".to_owned(),
            decision: Decision::Deny,
        }]);
        assert!(matches!(
            authorize_get(&s, "scratch", "departures"),
            GetOutcome::Denied { .. }
        ));
    }

    /// Destructure a [`Dispatch::Answer`], panicking on a consent knock or a
    /// routed query.
    fn answer(d: Dispatch) -> (Response, Option<Toast>) {
        match d {
            Dispatch::Answer(r, t) => (r, t),
            Dispatch::Consent { .. } => panic!("expected an immediate answer, got a consent knock"),
            Dispatch::Query { .. } => panic!("expected an immediate answer, got a routed query"),
        }
    }

    /// Destructure a [`GetDisposition::Answer`], panicking on a routed query.
    fn get_answer(d: GetDisposition) -> (Response, Option<Toast>) {
        match d {
            GetDisposition::Answer(r, t) => (r, t),
            GetDisposition::Query { .. } => {
                panic!("expected an inline answer, got a routed query")
            }
        }
    }

    #[test]
    fn handle_auth_mints_and_audits_then_get_serves_decision() {
        let mut state = BrokerState::new(store(vec![Grant::always("claude", "departures")]));
        // Auth mints a token and records a granted audit entry.
        let (resp, toast) = answer(state.handle_auth("claude"));
        assert!(resp.ok);
        assert!(toast.is_none(), "a silent mint raises no toast");
        let token = resp.token.expect("minted token");
        assert_eq!(state.audit.len(), 1);
        assert_eq!(state.audit[0].outcome, Outcome::Granted);

        // A snapshot reflects the grant, the live token, and the audit entry.
        let snap = state.snapshot(now_unix());
        assert_eq!(snap.grants.len(), 1);
        assert_eq!(snap.tokens.len(), 1);
        assert_eq!(snap.audit.len(), 1);
        assert!(snap.pending.is_empty(), "a granted agent is not pending");

        // The token now authorizes a departures get decision (Allowed) — we don't
        // do the live fetch here, just assert the authorization the handler uses.
        let agent = state
            .tokens
            .agent_for(&token, now_unix())
            .expect("token resolves");
        assert_eq!(
            authorize_get(&state.grants, &agent, "departures"),
            GetOutcome::Allowed
        );
    }

    #[test]
    fn no_grant_auth_knocks_for_consent_and_records_nothing_yet() {
        let mut state = BrokerState::new(store(Vec::new()));
        let Dispatch::Consent {
            agent, datasource, ..
        } = state.handle_auth("stranger")
        else {
            panic!("a no-grant auth must knock for consent, not deny");
        };
        assert_eq!(agent, "stranger");
        assert_eq!(datasource, "departures");
        // The knock parks; nothing is minted or audited until the decision lands.
        assert!(state.audit.is_empty(), "the knock itself isn't audited");
        assert!(state.tokens.active(now_unix()).is_empty(), "no token yet");
    }

    #[test]
    fn empty_agent_is_rejected_without_a_toast() {
        let mut state = BrokerState::new(store(Vec::new()));
        let (resp, toast) = answer(state.handle_auth("   "));
        assert!(!resp.ok);
        assert!(toast.is_none());
        assert!(state.audit.is_empty(), "a blank auth isn't audited");
    }

    #[test]
    fn apply_consent_allow_always_persists_grant_and_mints_a_usable_token() {
        let mut state = BrokerState::new(store(Vec::new()));
        let (resp, toast) = state.apply_consent(
            "claude",
            "departures",
            ConsentDecision::AllowAlways,
            now_unix(),
        );
        assert!(
            resp.ok && toast.is_none(),
            "an allow answers with a token, no toast"
        );
        let token = resp.token.expect("minted");
        // The durable grant persisted, and a Grant-scoped token was minted.
        assert_eq!(
            state.grants.decision_for("claude", "departures"),
            Some(Decision::Always)
        );
        assert!(state.tokens.agent_for(&token, now_unix()).is_some());
    }

    #[test]
    fn apply_consent_allow_session_mints_a_token_without_persisting_a_grant() {
        let mut state = BrokerState::new(store(Vec::new()));
        let (resp, _) = state.apply_consent(
            "claude",
            "departures",
            ConsentDecision::AllowSession,
            now_unix(),
        );
        let token = resp.token.expect("minted");
        assert!(
            state.grants.decision_for("claude", "departures").is_none(),
            "a session decision persists no durable grant"
        );
        assert!(state.tokens.agent_for(&token, now_unix()).is_some());
    }

    #[test]
    fn apply_consent_allow_once_mints_a_single_fetch_token() {
        let mut state = BrokerState::new(store(Vec::new()));
        let (resp, _) = state.apply_consent(
            "claude",
            "departures",
            ConsentDecision::AllowOnce,
            now_unix(),
        );
        assert!(resp.token.is_some(), "once still hands back a token");
        assert!(state.grants.decision_for("claude", "departures").is_none());
    }

    #[test]
    fn apply_consent_deny_persists_a_standing_no_and_toasts() {
        let mut state = BrokerState::new(store(Vec::new()));
        let (resp, toast) =
            state.apply_consent("scratch", "departures", ConsentDecision::Deny, now_unix());
        assert!(!resp.ok);
        assert_eq!(
            state.grants.decision_for("scratch", "departures"),
            Some(Decision::Deny),
            "a deliberate deny persists a standing no"
        );
        let toast = toast.expect("a deny toasts");
        assert!(toast.summary.contains("scratch"));
        // …and a subsequent auth is now refused silently (no re-prompt).
        assert!(matches!(
            authorize_auth(&state.grants, "scratch"),
            AuthOutcome::Denied { .. }
        ));
    }

    #[test]
    fn consent_timeout_is_a_transient_deny_with_a_toast_and_no_persist() {
        let mut state = BrokerState::new(store(Vec::new()));
        let (resp, toast) = state.on_consent_timeout("claude", "departures", now_unix());
        assert!(!resp.ok);
        let toast = toast.expect("a timeout raises the 1a fallback toast");
        assert!(toast.summary.contains("claude"));
        // A timeout is NOT a durable decision — no grant is written, so the agent
        // may re-ask (a fresh knock next time, not a standing no).
        assert!(
            state.grants.decision_for("claude", "departures").is_none(),
            "an unanswered prompt persists nothing"
        );
        assert_eq!(
            authorize_auth(&state.grants, "claude"),
            AuthOutcome::NeedsConsent
        );
    }

    #[test]
    fn get_authority_follows_the_token_scope() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let mut state = BrokerState::new(store(Vec::new()));
            // A session token authorizes a get with no durable grant. (We stop at
            // the authorization decision — the live fetch needs the network — by
            // asserting a Session token resolves as allowed via the scope, which
            // the handler consults before fetching.)
            let session = state
                .tokens
                .mint_scoped("claude", now_unix(), TokenScope::Session);
            let auth = state
                .tokens
                .resolve(&session.value, now_unix())
                .expect("resolves");
            assert_eq!(auth.scope, TokenScope::Session);

            // A once token authorizes exactly one fetch: the first spend succeeds,
            // the second is denied (spent).
            let once = state
                .tokens
                .mint_scoped("claude", now_unix(), TokenScope::Once);
            assert!(state.tokens.spend_once(&once.value), "first fetch allowed");
            assert!(
                !state.tokens.spend_once(&once.value),
                "the once authority is spent after one fetch"
            );

            // A Grant-scoped token with NO durable grant is denied (no toast).
            let identity = state.tokens.mint("stranger", now_unix()); // Grant scope
            let (resp, toast) = get_answer(state.handle_get(&identity.value, "departures", None));
            assert!(!resp.ok, "an identity token without a grant can't fetch");
            assert!(toast.is_none(), "a scope miss is transient, not a knock");
        });
    }

    #[test]
    fn audit_ring_is_capped() {
        let mut state = BrokerState::new(store(Vec::new()));
        for _ in 0..(AUDIT_CAP + 5) {
            state.record("claude", "auth", Outcome::Denied, now_unix());
        }
        assert_eq!(state.audit.len(), AUDIT_CAP, "the ring is bounded");
        assert_eq!(state.snapshot(now_unix()).audit.len(), AUDIT_CAP);
    }

    #[test]
    fn dispatch_grants_lists_the_store() {
        let state = BrokerState::new(store(vec![Grant::always("claude", "departures")]));
        let resp = state.handle_grants();
        assert!(resp.ok);
        let grants = resp.grants.expect("grants listed");
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].agent, "claude");
        assert_eq!(grants[0].decision, "always");
    }

    #[test]
    fn get_with_a_bad_token_is_a_toastless_error() {
        let mut state = BrokerState::new(store(vec![Grant::always("claude", "departures")]));
        let (resp, toast) = get_answer(state.handle_get("not-a-token", "departures", None));
        assert!(!resp.ok);
        assert!(
            toast.is_none(),
            "an expired/invalid token is transient, not a consent knock"
        );
        assert!(resp.error.unwrap().contains("re-auth"));
    }

    #[test]
    fn get_unknown_datasource_is_denied_without_a_toast() {
        let mut state = BrokerState::new(store(vec![Grant::always("claude", "departures")]));
        let token = answer(state.handle_auth("claude")).0.token.expect("token");
        // `stocks` is a genuinely unknown datasource (departures/weather/calendar
        // are the known set, #509).
        let (resp, toast) = get_answer(state.handle_get(&token, "stocks", None));
        assert!(!resp.ok);
        assert!(
            toast.is_none(),
            "an unknown datasource is a request error, not a consent knock"
        );
        assert!(resp.error.unwrap().contains("unknown datasource"));
    }

    // ── The calendar datasource (#484) ────────────────────────────────────────

    fn calendar_fixture() -> Vec<CalendarEntry> {
        vec![
            CalendarEntry {
                start_unix: 100,
                end_unix: 200,
                title: "standup".to_owned(),
                calendar: "Work".to_owned(),
            },
            CalendarEntry {
                start_unix: 300,
                end_unix: 400,
                title: "the thing".to_owned(),
                calendar: "Personal".to_owned(),
            },
        ]
    }

    #[test]
    fn apply_cmd_calendar_replaces_the_live_copy_and_labels_the_datasource() {
        let mut state = BrokerState::new(store(Vec::new()));
        assert!(state.calendar.is_empty());
        // A calendar status line rides the snapshot even when empty…
        let snap = state.snapshot(now_unix());
        let cal = snap
            .datasources
            .iter()
            .find(|d| d.name == "calendar")
            .expect("calendar datasource is listed");
        assert_eq!(cal.status, "no upcoming events");
        // …and updates when the host push replaces the copy.
        state.apply_cmd(Cmd::Calendar(calendar_fixture()));
        assert_eq!(state.calendar.len(), 2);
        let snap = state.snapshot(now_unix());
        let cal = snap
            .datasources
            .iter()
            .find(|d| d.name == "calendar")
            .expect("calendar datasource is listed");
        assert_eq!(cal.status, "2 upcoming");
    }

    #[test]
    fn get_calendar_serves_the_live_copy_under_the_grant_flow() {
        // An `always` grant for calendar → a Grant-scoped identity token can serve
        // it (the normal grant flow, datasource-scoped).
        let mut state = BrokerState::new(store(vec![Grant::always("claude", "calendar")]));
        state.apply_cmd(Cmd::Calendar(calendar_fixture()));
        let token = answer(state.handle_auth("claude")).0.token.expect("token");
        let (resp, toast) = get_answer(state.handle_get(&token, "calendar", None));
        assert!(resp.ok, "the granted agent gets the calendar copy");
        assert!(toast.is_none(), "a served datasource raises no toast");
        let rows = resp.calendar.expect("calendar rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "standup");
        // `limit` clamps the served rows.
        let (resp, _) = get_answer(state.handle_get(&token, "calendar", Some(1)));
        assert_eq!(resp.calendar.expect("rows").len(), 1);
    }

    #[test]
    fn get_calendar_without_a_grant_is_denied() {
        // A departures-only grant does NOT cover calendar (per-datasource).
        let mut state = BrokerState::new(store(vec![Grant::always("claude", "departures")]));
        state.apply_cmd(Cmd::Calendar(calendar_fixture()));
        let token = answer(state.handle_auth("claude")).0.token.expect("token");
        let (resp, toast) = get_answer(state.handle_get(&token, "calendar", None));
        assert!(!resp.ok, "no calendar grant → denied");
        assert!(toast.is_none(), "a scope miss is transient, not a knock");
        assert!(resp.calendar.is_none());
    }

    #[test]
    fn calendar_status_labels_by_count() {
        assert_eq!(calendar_status(0), "no upcoming events");
        assert_eq!(calendar_status(1), "1 upcoming");
        assert_eq!(calendar_status(4), "4 upcoming");
    }

    #[test]
    fn calendar_scoped_clamps_to_the_limit() {
        let cal = calendar_fixture();
        assert_eq!(calendar_scoped(&cal, None).len(), 2, "None = all");
        assert_eq!(calendar_scoped(&cal, Some(1)).len(), 1);
        assert_eq!(calendar_scoped(&cal, Some(9)).len(), 2, "over-limit clamps");
    }

    // ── Routed datasources: departures + weather (#509) ────────────────────────

    #[test]
    fn known_datasources_are_the_routed_two_plus_calendar() {
        assert!(is_known_datasource("departures"));
        assert!(is_known_datasource("weather"));
        assert!(is_known_datasource("calendar"));
        assert!(!is_known_datasource("stocks"));
    }

    #[test]
    fn query_scope_and_params_shapes_by_datasource() {
        let (scope, params) = query_scope_and_params("departures", Some(3));
        assert_eq!(scope, "next");
        assert!(
            params.contains("\"limit\":3"),
            "params carry the limit: {params}"
        );
        // A missing limit falls back to the default.
        let (_, params) = query_scope_and_params("departures", None);
        assert!(params.contains(&format!("\"limit\":{DEFAULT_DEPARTURES_LIMIT}")));
        // Weather takes the `current` scope and empty params.
        let (scope, params) = query_scope_and_params("weather", None);
        assert_eq!(scope, "current");
        assert_eq!(params, "{}");
    }

    #[test]
    fn query_response_departures_decodes_the_provider_payload() {
        let payload = serde_json::to_string(&vec![DepartureOut {
            line: "S9".to_owned(),
            direction: "Spandau".to_owned(),
            hhmm: "16:05".to_owned(),
            in_minutes: 7,
            delay_minutes: 1,
            cancelled: false,
        }])
        .unwrap();
        let resp = query_response("departures", QueryOutcome::Ready(payload));
        assert!(resp.ok);
        assert_eq!(resp.datasource.as_deref(), Some("departures"));
        let rows = resp.departures.expect("departures rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].line, "S9");
        assert!(resp.weather.is_none());
    }

    #[test]
    fn query_response_weather_decodes_the_provider_payload() {
        let payload = serde_json::to_string(&WeatherOut {
            location: "Berlin".to_owned(),
            temp_c: 21.0,
            apparent_c: 20.0,
            temp_max_c: 24.0,
            temp_min_c: 15.0,
            humidity_pct: 55,
            wind_kmh: 9.0,
            condition_code: 0,
            condition_label: "Clear".to_owned(),
            condition_icon: "weather-clear-symbolic".to_owned(),
        })
        .unwrap();
        let resp = query_response("weather", QueryOutcome::Ready(payload));
        assert!(resp.ok);
        assert_eq!(resp.datasource.as_deref(), Some("weather"));
        let w = resp.weather.expect("weather reading");
        assert_eq!(w.location, "Berlin");
        assert_eq!(w.condition_label, "Clear");
        assert!(resp.departures.is_none());
    }

    #[test]
    fn query_response_failure_is_a_transient_error() {
        let resp = query_response("departures", QueryOutcome::Failed("no provider".to_owned()));
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("no provider"));
    }

    #[test]
    fn query_response_unreadable_payload_is_an_error() {
        let resp = query_response("weather", QueryOutcome::Ready("not json".to_owned()));
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("unreadable"));
    }

    #[test]
    fn handle_get_departures_routes_a_query_under_the_grant_flow() {
        // A session token authorizes any datasource get; departures now routes a
        // query rather than fetching inline (#509 dedup).
        let mut state = BrokerState::new(store(Vec::new()));
        let token = state
            .tokens
            .mint_scoped("claude", now_unix(), TokenScope::Session);
        match state.handle_get(&token.value, "departures", Some(4)) {
            GetDisposition::Query {
                datasource,
                scope,
                params,
            } => {
                assert_eq!(datasource, "departures");
                assert_eq!(scope, "next");
                assert!(params.contains("\"limit\":4"));
            }
            GetDisposition::Answer(resp, _) => panic!("expected a routed query, got {resp:?}"),
        }
        // The access grant is audited even though the answer is deferred.
        assert_eq!(
            state.audit.back().expect("audited").outcome,
            Outcome::Granted
        );
    }

    #[test]
    fn handle_get_weather_routes_a_query_under_the_grant_flow() {
        let mut state = BrokerState::new(store(vec![Grant::always("claude", "weather")]));
        let token = answer(state.handle_auth("claude")).0.token.expect("token");
        match state.handle_get(&token, "weather", None) {
            GetDisposition::Query {
                datasource, scope, ..
            } => {
                assert_eq!(datasource, "weather");
                assert_eq!(scope, "current");
            }
            GetDisposition::Answer(resp, _) => panic!("expected a routed query, got {resp:?}"),
        }
    }

    #[test]
    fn handle_get_departures_without_a_grant_is_denied_inline() {
        // No standing grant + a plain identity token → denied inline, never routed.
        let mut state = BrokerState::new(store(Vec::new()));
        let identity = state.tokens.mint("stranger", now_unix());
        let (resp, toast) = get_answer(state.handle_get(&identity.value, "departures", None));
        assert!(!resp.ok, "no grant → denied, not routed");
        assert!(toast.is_none());
    }
}
