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
        }
    }

    /// Apply a panel command (revoke / allow). Returns nothing — the caller
    /// pushes a fresh snapshot afterwards.
    fn apply_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Revoke { agent, datasource } => match self.grants.revoke(&agent, &datasource) {
                Ok(true) => {
                    // Revoking a grant invalidates any live tokens riding on it.
                    let killed = self.tokens.revoke_agent(&agent);
                    tracing_eprintln(&format!(
                        "revoked {agent}/{datasource}; killed {killed} token(s)"
                    ));
                }
                Ok(false) => {}
                Err(e) => tracing_eprintln(&format!("revoke {agent}/{datasource} failed: {e}")),
            },
            Cmd::Allow { agent, datasource } => {
                if let Err(e) = self.grants.grant_always(&agent, &datasource) {
                    tracing_eprintln(&format!("allow {agent}/{datasource} failed: {e}"));
                }
            }
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
                write_response(&mut stream, &response).await;
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
                if let Err(e) = self.grants.grant_deny(agent, datasource) {
                    tracing_eprintln(&format!("persisting deny for {agent}/{datasource}: {e}"));
                }
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
                if let Err(e) = self.grants.grant_always(agent, datasource) {
                    tracing_eprintln(&format!("persisting always for {agent}/{datasource}: {e}"));
                }
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

/// Write one response line (JSON + `\n`) to a client stream, best-effort.
async fn write_response(stream: &mut UnixStream, response: &Response) {
    let mut out = encode_response(response);
    out.push('\n');
    if let Err(e) = stream.write_all(out.as_bytes()).await {
        tracing_eprintln(&format!("writing response failed: {e}"));
    }
    let _ = stream.flush().await;
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

/// Bind the broker socket: unlink any stale socket, bind, tighten to `0600`
/// (same-user-only, exactly like the host's own plugin socket). The parent is
/// `$XDG_RUNTIME_DIR`, already `0700`.
fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
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
    Ok(listener)
}

/// Run the broker: load the durable grants, bind the socket, then loop serving
/// client connections and panel commands until the command lane closes (the
/// plugin session tearing down — the socket is rebound fresh on the next
/// session, which is what drops in-memory tokens on a shell restart).
///
/// SDK-free: `cmds`/`out` are plain tokio channels (the plugin passes the SDK's
/// per-session lane ends, which are exactly these types), so this whole module
/// never links the plugin runtime.
// One cohesive `select!` loop (accept / command / timeout) over the parked-request
// state (consent + query maps); splitting its arms into helpers would scatter that
// shared state for no readability gain — same stance as the host's `handle_conn`.
#[allow(clippy::too_many_lines)]
pub async fn serve(mut cmds: mpsc::UnboundedReceiver<Cmd>, out: mpsc::UnboundedSender<BrokerMsg>) {
    let Some(sock) = paths::socket_path() else {
        tracing_eprintln("XDG_RUNTIME_DIR unset; broker socket not created");
        return;
    };

    let grants = paths::grants_path().map_or_else(
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
    );
    let mut state = BrokerState::new(grants);

    let listener = match bind_socket(&sock) {
        Ok(l) => l,
        Err(e) => {
            tracing_eprintln(&format!("failed to bind {}: {e}", sock.display()));
            return;
        }
    };
    tracing_eprintln(&format!("listening on {}", sock.display()));

    // Seed the panel with the current grants/status before any request.
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
                            write_response(&mut p.stream, &resp).await;
                            send_update(&out, state.snapshot(now_unix()), toast);
                        }
                        // else: a late/unknown decision (already timed out) — ignore.
                    }
                    // #509: a routed query's answer — shape the response from the
                    // provider's payload and write it back to the parked client.
                    Cmd::QueryResult { request_id, outcome } => {
                        if let Some(mut q) = pending_queries.remove(&request_id) {
                            let resp = query_response(&q.datasource, outcome);
                            write_response(&mut q.stream, &resp).await;
                        }
                        // else: a late/unknown result (already timed out) — ignore.
                    }
                }
            }
            accepted = listener.accept() => {
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
                        write_response(&mut p.stream, &resp).await;
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
                        write_response(&mut q.stream, &resp).await;
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

    fn store(grants: Vec<Grant>) -> GrantStore {
        GrantStore::from_grants(grants)
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
