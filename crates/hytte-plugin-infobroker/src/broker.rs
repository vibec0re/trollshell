//! The broker itself: the consent decisions, the audit trail, and the socket
//! server that binds `$XDG_RUNTIME_DIR/hytte-infobroker.sock` and answers the
//! [`crate::wire`] JSON-lines protocol.
//!
//! The **decision** functions ([`authorize_auth`], [`authorize_get`]) are pure
//! and unit-tested here; [`serve`] is the thin async shell that wires sockets +
//! the panel command lane around them. The broker owns the [`GrantStore`]
//! (durable) and the [`TokenStore`] (ephemeral) — no shared mutex, because one
//! loop owns them and handles one request or one panel command at a time.
//! Reading a client's request line is the one per-connection step that runs
//! **off** that loop, in a task of its own (see `run_session`), so a client
//! that connects and sends nothing holds up nobody but itself.
//!
//! Consent policy (phase 1b — interactive Allow/Deny prompting, over the 1a
//! grant/token machinery). Every `auth` asks for **one** datasource — the one
//! it names, or `departures` when it names none (what the prompt named before
//! the request could) — and every decision below is about that
//! `(agent, datasource)` pair:
//! - `auth` mints a token **silently** iff an `always` grant covers the pair.
//! - A standing `deny` grant for the pair refuses `auth` **silently** — a
//!   settled "no" isn't re-prompted — with an informational [`Toast`] so the
//!   knock is still visible.
//! - Otherwise (no standing grant) `auth` **parks** the socket request and fires
//!   a consent prompt at the human naming the datasource
//!   (`Effect::RequestConsent` via the plugin, #487). The decision resolves the
//!   parked request per `BrokerState::apply_consent`: `AllowAlways`/`Deny`
//!   persist to grants.toml, `AllowSession` mints a session-scoped token,
//!   `AllowOnce` a single-fetch token — both of the latter scoped to the
//!   datasource the prompt named, and to nothing else. An unanswered prompt
//!   times out to a **transient** deny + the 1a toast, and persists nothing
//!   (`BrokerState::on_consent_timeout`), so the agent may ask again. That
//!   covers every way a card goes unanswered — ignored for its 60 s, dismissed
//!   with `Esc`, superseded by another card, never drawn for want of an
//!   output — because the shell sends **no** decision for any of them (#1542
//!   item 5: silence decides nothing, so only a click on **Deny** persists a
//!   standing no), as well as a wedged or pre-1b host that never shows one.
//! - `get <datasource>` requires a valid token, and either an `always` grant
//!   for `(agent, datasource)` or a token whose own [`TokenScope`] — a session,
//!   or a single unspent once — names that datasource; anything else is denied
//!   *without* a toast (re-auth naming the datasource to re-consent). So **a
//!   token never reaches a datasource the human has not consented to for that
//!   agent**: either in the prompt it was minted from, or as a durable grant.
//! - An **invalid/expired token** is a transient technical failure (the agent
//!   just re-auths), so it is denied *without* a toast — only genuine consent
//!   knocks alert the human.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use crate::grants::{Decision, GrantStore};
use crate::paths;
use crate::tokens::{Token, TokenScope, TokenStore};
use crate::wire::{
    CalendarEntry, DATASOURCE_CALENDAR, DATASOURCE_DEPARTURES, DATASOURCE_WEATHER, DepartureOut,
    GrantOut, MAX_AGENT_NAME_CHARS, Request, Response, WeatherOut, check_agent_name,
    encode_response, parse_request,
};

/// Cap on the in-memory audit ring shown in the panel. Oldest entries fall off.
const AUDIT_CAP: usize = 30;

/// How long one (agent, datasource) pair's refusal stays quiet after it raised
/// a toast (#1556): a same-uid process re-asking in a loop under a standing
/// deny is audited every time but toasts once per window. The audit ring has no
/// per-pair window to share (it is only [`AUDIT_CAP`]-bounded), so this is its
/// own number: long enough that a tight loop is one toast, short enough that a
/// pair the human is still ignoring resurfaces within the minute.
const DENY_TOAST_WINDOW_SECS: i64 = 60;

/// How many (agent, datasource) pairs [`DenyToastLimiter`] tracks; past it the
/// pair whose toast is oldest is forgotten, so a process inventing fresh agent
/// names cannot grow the table.
const DENY_TOAST_KEYS_CAP: usize = 256;

/// Toasts of any pair allowed per [`DENY_TOAST_WINDOW_SECS`]. A per-pair limit
/// alone is defeated by inventing a fresh agent name per request; this is the
/// backstop. Refusals past it are still audited and visible in the panel.
const DENY_TOAST_GLOBAL_BURST: u32 = 5;

/// How long a connected client has to send its one request line before the
/// broker gives up on it. Bounds one reader task (`read_request`) — which
/// is all a silent client can hold up, since the read runs off the session
/// loop — and so how long that client keeps one of the
/// [`MAX_CONCURRENT_READS`] slots.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The longest request line the broker reads, newline excluded. A real request
/// is well under a kilobyte (an op, an agent name, a token, a datasource);
/// this exists so a client that streams bytes without a newline is refused at
/// a fixed size instead of growing the broker's buffer for the whole of
/// [`REQUEST_TIMEOUT`]. Overflow is answered with an error naming the limit.
const MAX_REQUEST_LINE: usize = 64 * 1024;

/// How many connections may be mid-read at once. Each one is a reader task
/// holding a client's fd and up to [`MAX_REQUEST_LINE`] of buffer; at the cap,
/// the session stops accepting until one finishes (new connections wait in
/// the kernel's listen backlog), so a burst of silent clients cannot grow
/// either without bound. A real request reads in microseconds, so 16 is far
/// above anything the CLI produces.
const MAX_CONCURRENT_READS: usize = 16;

/// How many consent prompts may be parked at once. A resource bound, not a UX
/// choice: the shell shows one consent card at a time (a new one supersedes
/// the card on screen), so more than one parked knock is already a degraded
/// state, and a stream of knocks under fresh agent names could otherwise park
/// sockets without limit for [`CONSENT_PARK_TIMEOUT`] each. A knock past the
/// cap is refused with a retryable error, as is a second knock for an
/// `(agent, datasource)` pair that already has one parked (see
/// `Session::consent_refusal`).
const MAX_PENDING_CONSENTS: usize = 8;

/// How long the broker holds a **parked** consent request before giving up and
/// timing it out (#487 phase 1b).
///
/// This deadline is how an unanswered prompt ends. The shell owns the
/// user-facing countdown (`CONSENT_PROMPT_TIMEOUT_SECS`, 60 s) and sends a
/// decision only for a click: since #1542 item 5 a card that times out, is
/// dismissed with `Esc`, is superseded or has no output to be drawn on sends
/// nothing, and this timeout answers the agent instead — transiently, with
/// nothing persisted (`BrokerState::on_consent_timeout`). It also still
/// covers a wedged or pre-1b host that never surfaces the prompt at all.
///
/// So it must outlast the card, or a click in the card's last seconds would
/// land after the broker gave up and be dropped as late. It is the card's
/// [`CONSENT_CARD_TIMEOUT`] plus the [`CONSENT_PROMPT_GRACE`] race margin.
///
/// Public because it is also the longest a well-behaved broker leaves a client
/// waiting for its one reply line (an `auth` parked on the human), which is
/// what the CLI sizes its own read timeout from.
pub const CONSENT_PARK_TIMEOUT: std::time::Duration =
    CONSENT_CARD_TIMEOUT.saturating_add(CONSENT_PROMPT_GRACE);

/// How long the shell keeps a consent card on screen before it closes it: the
/// proto's `CONSENT_PROMPT_TIMEOUT_SECS` (60 s).
///
/// Mirrored rather than imported because this library stays SDK-free; the
/// plugin binary, which links the proto, asserts at compile time that the two
/// are equal. Besides sizing [`CONSENT_PARK_TIMEOUT`], it is the line
/// `deny_outlived_its_card` draws: no card can be clicked after it closes, so
/// a `Deny` that arrives this long after its knock parked is a shell's own
/// timeout, not a human.
pub const CONSENT_CARD_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(1);

/// The proto's `CONSENT_PROMPT_GRACE_SECS` (5 s): how much longer than the
/// card the broker waits, so a click in the card's last instant still lands.
/// Mirrored and compile-time checked like [`CONSENT_CARD_TIMEOUT`].
pub const CONSENT_PROMPT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the session loop pauses after a failed `accept()` before it tries
/// again. An accept error is usually a resource one — `EMFILE`/`ENFILE` with the
/// connection still queued in the backlog — and retrying at once just fails
/// again, so without a pause the loop spun on it, one journal line per lap.
/// Short enough that a consent click or a revoke queued behind it waits no
/// longer than this.
const ACCEPT_ERROR_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

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

/// The outcome of an `auth` request for one `(agent, datasource)` pair against
/// the grant store (#487 phase 1b).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    /// The pair has an `always` grant — mint a token silently.
    Granted,
    /// A standing `deny` grant covers the pair — a settled "no": refuse
    /// silently (no re-prompt), with a how-to-grant hint.
    Denied { hint: String },
    /// No standing grant either way — knock the human with an interactive consent
    /// prompt naming the datasource and park the request until the decision (or
    /// the 60 s bound).
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

/// The hint for a `get` the agent's token does not reach: a fresh `auth`
/// naming the datasource puts a prompt for exactly that datasource to the
/// human — which is the only way a consent token for it comes to exist — or
/// the human grants it durably.
fn reauth_hint(agent: &str, datasource: &str) -> String {
    format!(
        "ask the human for it with `hytte-infobroker auth --agent {agent} --datasource \
         {datasource}`, or {}",
        grant_hint(agent, datasource)
    )
}

/// The hint listing the datasources the broker serves.
fn known_datasources_hint() -> String {
    format!(
        "known datasources: '{DATASOURCE_DEPARTURES}', '{DATASOURCE_WEATHER}', \
         '{DATASOURCE_CALENDAR}'"
    )
}

/// The hint on an `auth` whose agent name [`check_agent_name`] refused.
fn agent_name_hint() -> String {
    format!(
        "pick a short, stable name such as 'claude': 1 to {MAX_AGENT_NAME_CHARS} characters, \
         no control, line-break, bidi or zero-width characters, and no leading or trailing \
         whitespace"
    )
}

/// `"<path> could not be read (<why>)"` for a grant store standing in for an
/// unreadable `grants.toml` (the #1489 follow-up audit, M3), or `None` for one
/// that loaded. The shared core of what the panel, the seed toast and the
/// `grants` reply each say about it.
fn unreadable_grants(grants: &GrantStore) -> Option<String> {
    let error = grants.load_error()?;
    let path = grants
        .path()
        .map_or_else(|| "grants.toml".to_owned(), |p| p.display().to_string());
    Some(format!("{path} could not be read ({error})"))
}

/// The consent card's detail line for `agent` asking to read `datasource` —
/// the plain-words version of the card's "⟨agent⟩ wants: read access from
/// ⟨datasource⟩". The departures wording is byte-for-byte what every prompt
/// read before the request could name a datasource.
fn consent_detail(agent: &str, datasource: &str) -> String {
    let what = match datasource {
        DATASOURCE_DEPARTURES => "the departures board",
        DATASOURCE_WEATHER => "the current weather",
        DATASOURCE_CALENDAR => "your upcoming calendar events",
        // Unreachable from `handle_auth`, which refuses an unknown datasource
        // before it prompts; naming it raw is the honest fallback anyway.
        other => other,
    };
    format!("{agent} wants to read {what}")
}

/// Decide an `auth` for `(agent, datasource)` (#487 phase 1b): a standing
/// `always` grant for the pair mints silently; a standing `deny` for it refuses
/// silently (no re-prompt); anything else needs an interactive consent
/// decision about exactly that pair. `always` wins over `deny` if both somehow
/// exist (a live "yes" beats a stale "no").
///
/// Per pair rather than per agent: the prompt this falls through to names one
/// datasource, so the silent answers on either side of it must be about that
/// same datasource — a departures grant does not answer a calendar ask.
#[must_use]
pub fn authorize_auth(grants: &GrantStore, agent: &str, datasource: &str) -> AuthOutcome {
    let decided = |decision: Decision| {
        grants
            .grants()
            .iter()
            .any(|g| g.agent == agent && g.datasource == datasource && g.decision == decision)
    };
    if decided(Decision::Always) {
        AuthOutcome::Granted
    } else if decided(Decision::Deny) {
        AuthOutcome::Denied {
            hint: grant_hint(agent, datasource),
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
    /// A one-line warning that this broker is **not saving grant changes**,
    /// because `grants.toml` is there but could not be read (the #1489
    /// follow-up audit, M3) — rendered under [`notice`](Self::notice). `None`
    /// in the normal case. A separate field because both can be true at once
    /// and say different things: `notice` is about the socket, this is about
    /// the grant store.
    pub store_notice: Option<String>,
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
    /// request rather than through `BrokerState::apply_cmd`.
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
    /// The datasource the request was about: the one a `get` named, or the
    /// one an `auth` asked for (`resource` is then `"auth"`). What a denied
    /// entry's pending row offers to Allow, so the panel's one-click grant
    /// names what the agent actually asked for.
    datasource: String,
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
    /// Which refusals may raise a toast (#1556).
    deny_toasts: DenyToastLimiter,
}

/// Rate limiter for the "denied" toast (#1556), keyed by (agent, datasource).
/// Time is injected (`now`, unix seconds, like the rest of the state) so the
/// tests need no sleeps.
#[derive(Default)]
struct DenyToastLimiter {
    /// Pair -> when it last toasted. Bounded by [`DENY_TOAST_KEYS_CAP`].
    last: HashMap<(String, String), i64>,
    /// Start of the current global window and the toasts raised in it.
    window_start: i64,
    window_count: u32,
}

impl DenyToastLimiter {
    /// Whether a refusal of `agent` on `datasource` at `now` may raise a toast;
    /// records it when it may. The first refusal of a pair always may (unless
    /// the global burst is spent), and the next one after the window.
    fn admit(&mut self, agent: &str, datasource: &str, now: i64) -> bool {
        let key = (agent.to_owned(), datasource.to_owned());
        if let Some(&at) = self.last.get(&key)
            && now.saturating_sub(at) < DENY_TOAST_WINDOW_SECS
            && now >= at
        {
            return false;
        }
        if now.saturating_sub(self.window_start) >= DENY_TOAST_WINDOW_SECS
            || now < self.window_start
        {
            self.window_start = now;
            self.window_count = 0;
        }
        if self.window_count >= DENY_TOAST_GLOBAL_BURST {
            return false;
        }
        self.window_count += 1;
        if !self.last.contains_key(&key)
            && self.last.len() >= DENY_TOAST_KEYS_CAP
            && let Some(oldest) = self
                .last
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(k, _)| k.clone())
        {
            self.last.remove(&oldest);
        }
        self.last.insert(key, now);
        true
    }
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
            deny_toasts: DenyToastLimiter::default(),
        }
    }

    /// The refusal toast for `agent` on `datasource`, or `None` when this pair
    /// toasted within [`DENY_TOAST_WINDOW_SECS`] (or the global burst is spent).
    /// Only the toast is limited; callers audit every refusal before asking.
    fn deny_toast(&mut self, agent: &str, datasource: &str, now: i64) -> Option<Toast> {
        self.deny_toasts
            .admit(agent, datasource, now)
            .then(|| denied_toast(agent, datasource))
    }

    /// Record one request in the capped audit ring (oldest falls off).
    /// `resource` is what the panel's activity row shows (`"auth"`, or the
    /// datasource a `get` named); `datasource` is what the request was about —
    /// see [`AuditEntry::datasource`].
    fn record(
        &mut self,
        agent: &str,
        resource: &str,
        datasource: &str,
        outcome: Outcome,
        now: i64,
    ) {
        if self.audit.len() == AUDIT_CAP {
            self.audit.pop_front();
        }
        self.audit.push_back(AuditEntry {
            at_unix: now,
            agent: agent.to_owned(),
            resource: resource.to_owned(),
            datasource: datasource.to_owned(),
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

        // Pending = (agent, datasource) pairs that were denied and still lack an
        // `always` grant — keyed by the datasource the request was about, so the
        // row's Allow grants what the agent asked for rather than departures
        // (all a phase-1a auth could ask for). A request for a datasource the
        // broker does not serve has nothing to grant, so it never makes a row.
        let mut pending: Vec<PendingView> = Vec::new();
        for e in &self.audit {
            if e.outcome != Outcome::Denied || !is_known_datasource(&e.datasource) {
                continue;
            }
            if self.grants.decision_for(&e.agent, &e.datasource) == Some(Decision::Always) {
                continue;
            }
            let pv = PendingView {
                agent: e.agent.clone(),
                datasource: e.datasource.clone(),
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
            store_notice: unreadable_grants(&self.grants).map(|what| {
                format!(
                    "Not saving grant changes: {what}. The file is left as it is and none of \
                     its grants are in force; Allow, Deny and Revoke still apply until the \
                     broker restarts. Fix the file (or move it away), then restart the broker."
                )
            }),
        }
    }

    /// The toast a session raises with its seed snapshot when `grants.toml`
    /// could not be read (M3) — the panel's notice reaches only someone who
    /// opens the panel, and a human answering a consent card with "Always"
    /// never does. Raised once per session start, so it repeats on a restart
    /// for as long as the file stays broken, and not otherwise.
    fn unreadable_store_toast(&self) -> Option<Toast> {
        unreadable_grants(&self.grants).map(|what| Toast {
            summary: "infobroker: grant changes are not being saved".to_owned(),
            body: format!("{what}. Fix it and restart the broker — see the infobroker panel."),
        })
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

    /// Dispatch one parsed request (read off the loop by `read_request`) to its
    /// handler.
    fn dispatch(&mut self, req: Request) -> Dispatch {
        match req {
            Request::Auth { agent, datasource } => self.handle_auth(&agent, datasource.as_deref()),
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

    /// `auth` for `datasource` — the one the request named, or departures when
    /// it named none (#487 phase 1b): mint silently on an `always` grant for the
    /// pair; refuse silently (+ toast) on a standing `deny` for it; otherwise
    /// ask the human about exactly that pair (the caller parks the request).
    ///
    /// An unknown datasource is refused before any of that, without an audit
    /// entry or a prompt: there is nothing it could be granted, and the prompt
    /// must never show the human a datasource name the agent made up.
    ///
    /// So is an agent name [`check_agent_name`] refuses (the #1489 follow-up
    /// audit, M2) — over [`MAX_AGENT_NAME_CHARS`], or carrying a control, a
    /// line break, a bidi override or a zero-width character. This is the one
    /// place a name enters the broker: every token is minted here or from a
    /// consent this parked, and every audit row and pending row names the
    /// agent of a request that got past this check.
    fn handle_auth(&mut self, agent: &str, datasource: Option<&str>) -> Dispatch {
        let now = now_unix();
        if let Err(reason) = check_agent_name(agent) {
            return Dispatch::Answer(
                Response::denied(
                    format!("auth: invalid agent name: {reason}"),
                    agent_name_hint(),
                ),
                None,
            );
        }
        let datasource = datasource.unwrap_or(DATASOURCE_DEPARTURES);
        if !is_known_datasource(datasource) {
            return Dispatch::Answer(
                Response::denied(
                    format!("auth: unknown datasource '{datasource}'"),
                    known_datasources_hint(),
                ),
                None,
            );
        }
        match authorize_auth(&self.grants, agent, datasource) {
            AuthOutcome::Granted => {
                let token = self.tokens.mint(agent, now);
                self.record(agent, "auth", datasource, Outcome::Granted, now);
                Dispatch::Answer(auth_ok_response(agent, &token), None)
            }
            AuthOutcome::Denied { hint } => {
                self.record(agent, "auth", datasource, Outcome::Denied, now);
                Dispatch::Answer(
                    Response::denied(
                        format!("no grant for agent '{agent}' on '{datasource}'"),
                        hint,
                    ),
                    self.deny_toast(agent, datasource, now),
                )
            }
            AuthOutcome::NeedsConsent => Dispatch::Consent {
                agent: agent.to_owned(),
                datasource: datasource.to_owned(),
                scope: "read access".to_owned(),
                detail: consent_detail(agent, datasource),
            },
        }
    }

    /// Resolve a parked consent knock (#487 phase 1b) once the human decides.
    /// `AllowAlways`/`Deny` persist to grants.toml; `AllowSession` mints a
    /// session-scoped token; `AllowOnce` a single-fetch token. Every allow answers
    /// the parked `auth` with a fresh token; a deny persists a standing "no".
    ///
    /// Only a click reaches here: the shell sends no decision for a card
    /// nobody answered (#1542 item 5), and [`Self::on_consent_timeout`]
    /// answers those instead. A shell from before that change still sends
    /// `Deny` on its own timeout, indistinguishable from a click on the wire;
    /// `Session::on_cmd` routes such a `Deny` to `on_consent_timeout` when it
    /// arrives after the card could have closed (`deny_outlived_its_card`),
    /// but an older shell's `Deny` on `Esc` or with no output still lands
    /// here. See `ConsentChoices::unanswered`'s "Version skew".
    ///
    /// `datasource` is the one the prompt named, and every arm is about it and
    /// nothing wider: the two persisted decisions write that pair's row, and the
    /// two consent tokens carry it in their [`TokenScope`], which is the only
    /// datasource their own authority reaches.
    fn apply_consent(
        &mut self,
        agent: &str,
        datasource: &str,
        decision: ConsentDecision,
        now: i64,
    ) -> (Response, Option<Toast>) {
        let scope = match decision {
            ConsentDecision::Deny => {
                // #1065: persists off-thread; failures self-log from inside
                // the detached write, not here.
                self.grants.grant_deny(agent, datasource);
                self.record(agent, "auth", datasource, Outcome::Denied, now);
                return (
                    Response::denied(
                        format!("consent denied for agent '{agent}' on '{datasource}'"),
                        grant_hint(agent, datasource),
                    ),
                    Some(denied_toast(agent, datasource)),
                );
            }
            ConsentDecision::AllowAlways => {
                self.grants.grant_always(agent, datasource);
                TokenScope::Grant
            }
            ConsentDecision::AllowSession => TokenScope::Session {
                datasource: datasource.to_owned(),
            },
            ConsentDecision::AllowOnce => TokenScope::Once {
                datasource: datasource.to_owned(),
            },
        };
        let token = self.tokens.mint_scoped(agent, now, scope);
        self.record(agent, "auth", datasource, Outcome::Granted, now);
        (auth_ok_response(agent, &token), None)
    }

    /// A parked consent knock that ran out its [`CONSENT_PARK_TIMEOUT`] with no
    /// decision: a prompt nobody answered (timed out, dismissed with `Esc`,
    /// superseded, or never drawn — the shell sends nothing for any of them
    /// since #1542 item 5), or a pre-1b / wedged host. Deny THIS request
    /// **transiently** — no persisted "no", so the agent may re-ask — and raise
    /// the 1a informational toast so the missed knock stays visible (the
    /// phase-1a fallback path).
    fn on_consent_timeout(
        &mut self,
        agent: &str,
        datasource: &str,
        now: i64,
    ) -> (Response, Option<Toast>) {
        self.record(agent, "auth", datasource, Outcome::Denied, now);
        (
            Response::denied(
                format!("consent request for agent '{agent}' on '{datasource}' timed out"),
                grant_hint(agent, datasource),
            ),
            self.deny_toast(agent, datasource, now),
        )
    }

    /// `get <datasource>` (#487 phase 1b / #509): token → `(agent, scope)` →
    /// data-access authority → serve. Authority is the agent's `always` grant
    /// for the datasource, or the token's own [`TokenScope`] when that names the
    /// datasource (a `Once` is spent by the fetch it authorizes) — never a
    /// consent token's scope stretched to a datasource its prompt did not name.
    /// `get` never re-prompts; it honors (or spends) authority already granted.
    /// Once authorized, `calendar`
    /// is served inline from the host-fed live copy, while `departures`/`weather` are
    /// **routed through their provider plugins** over the host's datasource query
    /// protocol ([`GetDisposition::Query`]) rather than fetched here — the #509 dedup
    /// of the broker's former internal departures fetch.
    ///
    /// A `get` for a datasource the broker does not serve is refused without an
    /// audit entry, the rule `handle_auth` already keeps (the #1489 follow-up
    /// audit, M2). It used to be recorded with the requested name as the
    /// activity row's resource — the one string in a `get` the client picks,
    /// unchecked and up to the 64 KiB request cap, rendered into the panel for
    /// the life of the ring. The agent hears the refusal; the panel shows only
    /// datasources that exist.
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
            return GetDisposition::Answer(
                Response::denied(
                    format!("unknown datasource '{datasource}'"),
                    known_datasources_hint(),
                ),
                None,
            );
        }
        // Data-access authority: a durable `always` grant for this datasource
        // (whatever the token's scope), or the token's own consent scope when it
        // names this datasource — an open session, or a single as-yet-unspent
        // once, which this get consumes. The grant is checked first so a get the
        // grant already covers never spends a once.
        let allowed = matches!(
            authorize_get(&self.grants, &agent, datasource),
            GetOutcome::Allowed
        ) || (auth.scope.covers(datasource)
            && match auth.scope {
                TokenScope::Once { .. } => !auth.spent && self.tokens.spend_once(token),
                TokenScope::Grant | TokenScope::Session { .. } => true,
            });
        if !allowed {
            // A spent once, a consent token for another datasource, or an
            // identity token whose grant is gone: transient — re-auth naming the
            // datasource to re-consent — so no toast (only genuine knocks alert).
            self.record(&agent, datasource, datasource, Outcome::Denied, now);
            return GetDisposition::Answer(
                Response::denied(
                    format!("agent '{agent}' has no current grant for '{datasource}'"),
                    reauth_hint(&agent, datasource),
                ),
                None,
            );
        }
        // The access decision is granted; record it once here (the eventual serve
        // outcome — a live copy, or the routed query's success/failure — is separate,
        // exactly as the old inline fetch recorded granted before it could fail).
        self.record(&agent, datasource, datasource, Outcome::Granted, now);
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
    ///
    /// When `grants.toml` could not be read (M3) the list is the session's
    /// in-memory set rather than the file's, and the reply's `hint` says so —
    /// `ok` stays `true` because the list is still the truth about what this
    /// broker enforces, but an empty one must not read as "no grants".
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
            hint: unreadable_grants(&self.grants).map(|what| {
                format!(
                    "{what}: these are this session's grants, not the file's, and changes to \
                     them are not saved — fix the file, then restart the broker"
                )
            }),
            ..Response::default()
        }
    }
}

/// The informational toast for a denied knock on `resource` (the datasource
/// the agent asked for).
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

// The other side of that bracket (#1074 review M3): the measured ~1 ms
// crossing is ≈15 000 rows, so a threshold above it would be offloading only
// responses that already cost more than the starvation budget this constant
// exists to protect — the value would have stopped meaning what its doc says
// it means. Together with the bound above, the threshold is pinned to
// 10 001..=15 000; `tests::offload_branch_is_taken_exactly_at_the_threshold`
// pins where inside that range it actually sits, since the const asserts alone
// cannot tell 12 000 from 15 000.
const _: () = assert!(LARGE_RESPONSE_ROWS <= 15_000);

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

/// Read and parse one client's request line — the per-connection step that
/// runs **off** the session loop, as one task in `run_session`'s reader set.
///
/// Before this ran on its own task the loop did it inline, between `accept()`
/// and its next `select!`: a client that connected and sent nothing held the
/// whole loop for [`REQUEST_TIMEOUT`], so for those five seconds no
/// `Cmd::Decision` (a human's consent click), `Cmd::Revoke` or `QueryResult`
/// was handled, and no other client was accepted — and a client that kept
/// reconnecting kept it held, long enough for a consent answer to land after
/// its park deadline and be dropped as late. Now the slow part is here, where
/// it can only cost the client its own answer.
///
/// Touches no broker state, which is what lets it run off the loop: it hands
/// a parsed [`Request`] back with the stream, and the loop — still the only
/// owner of the grants, tokens and parked requests — dispatches it. A
/// connection that yields no request is settled here instead: one that closes
/// without sending anything gets nothing; a read that fails, times out,
/// overflows [`MAX_REQUEST_LINE`] or does not parse gets its error line
/// written back from this task, and `None` tells the loop there is nothing
/// left to do.
async fn read_request(stream: UnixStream) -> Option<(UnixStream, Request)> {
    use tokio::io::AsyncReadExt as _;

    // One byte past the cap, so a line of exactly `MAX_REQUEST_LINE` bytes plus
    // its newline still reads whole, and one byte more is distinguishable from
    // a client that sent exactly the cap and then closed.
    let limit = u64::try_from(MAX_REQUEST_LINE + 1).unwrap_or(u64::MAX);
    let mut reader = BufReader::new(stream).take(limit);
    let mut line = String::new();
    let read = tokio::time::timeout(REQUEST_TIMEOUT, reader.read_line(&mut line)).await;
    let mut stream = reader.into_inner().into_inner();
    let refusal = match read {
        Ok(Ok(0)) => return None, // the client closed without a request
        // The cap cut the line off before any newline: refuse at the cap rather
        // than wait out the timeout for a newline that may never come.
        Ok(Ok(_)) if line.len() > MAX_REQUEST_LINE && !line.ends_with('\n') => Response::error(
            format!("request line longer than {MAX_REQUEST_LINE} bytes; refused"),
        ),
        Ok(Ok(_)) => match parse_request(line.trim()) {
            Ok(request) => return Some((stream, request)),
            Err(e) => Response::error(e),
        },
        Ok(Err(e)) => Response::error(format!("read error: {e}")),
        Err(_) => Response::error("timed out waiting for a request"),
    };
    write_response(&mut stream, refusal).await;
    None
}

/// A dispatched request's disposition, before the stream is written or parked.
// `Answer` carries a full `Response` (the common case, not a rare large variant),
// so the large-variant delta is expected — same as [`GetDisposition`].
#[allow(clippy::large_enum_variant)]
enum Dispatch {
    /// A ready response (+ optional toast) to write back immediately.
    Answer(Response, Option<Toast>),
    /// The request needs a consent decision; [`run_session`] parks the
    /// connection.
    Consent {
        agent: String,
        datasource: String,
        scope: String,
        detail: String,
    },
    /// The request needs a routed datasource query (#509); [`run_session`]
    /// parks it.
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

/// [`serve_inner`]'s shutdown arm (#1079), normalized to one future: `Some`
/// when the SDK's shutdown hook asks this session to drain the grant store
/// (the payload is the ack sender to reply on once that is done), forever
/// pending when there is no shutdown wiring at all — plain `serve`/
/// `serve_with_grant_loader` and every existing test that calls them
/// directly — so that `select!` branch simply never fires there rather than
/// needing its own `if` guard. A `shutdown` that resolves to `Err` (the
/// sender dropped without ever asking — an ordinary session end) also maps to
/// `None`: only a genuine ask should end up looking like one.
///
/// **Clears the slot on any resolution, not just the genuine-ask one**
/// (#1092 review M3): `tokio::sync::oneshot::Receiver::poll` panics
/// ("called after complete") on any poll *after* it already returned
/// `Ready` — including `Ready(Err(_))` — and `serve_inner`'s `loop` builds a
/// fresh `select!` (and so a fresh call to this function) every iteration.
/// Without clearing it, an `Err` here (the ordinary-teardown case the doc
/// above already names as expected) would arm exactly that panic on the
/// very next iteration — not reachable from the shipped `Infobroker` today
/// (`cmds.recv()` is `biased` and always resolves first when both channels
/// close together, ending the loop before this function is ever polled
/// again — see `serve_inner`'s comment at its call site), but a caller that
/// clones its `CmdSender` into a task, or a future reorder that drops
/// `biased`, would panic the broker task on the very next `select!`.
/// Reproduced live through the public `serve_with_shutdown` seam (a seeded
/// session, `drop`ping the shutdown sender, then one `Cmd::Revoke`) before
/// this fix.
async fn recv_shutdown(
    shutdown: &mut Option<oneshot::Receiver<oneshot::Sender<()>>>,
) -> Option<oneshot::Sender<()>> {
    match shutdown {
        Some(rx) => {
            let ack = rx.await.ok();
            if ack.is_none() {
                // The sender is gone for good (a oneshot fires at most
                // once); re-polling `rx` on the next iteration would panic.
                // `None` is already this function's "nothing to do" answer
                // for that case, so parking here forever changes nothing an
                // ordinary caller observes.
                *shutdown = None;
            }
            ack
        }
        None => std::future::pending().await,
    }
}

/// Whether a `Deny` for the knock parked until `deadline` arrived at or after
/// the moment that knock's card closed — [`CONSENT_CARD_TIMEOUT`] after it
/// parked — which no click can do (#1601 review, LOW-3).
///
/// Since #1542 item 5 a shell sends nothing for a card nobody answered. A
/// shell from before that change still sends `Deny` from its card's own 60 s
/// timer, and the frame carries nothing that tells it from a click; after a
/// deploy the plugin restarts onto the new build while the shell often keeps
/// running the old one. The card goes up after the knock parks and is torn
/// down 60 s later, so a `Deny` at or past that point can only be that timer —
/// or a click in the very last instant of a card that went up late, which this
/// then answers transiently instead of persisting: the safe side to err on.
///
/// The knock parked at `deadline - CONSENT_PARK_TIMEOUT`, so no extra field is
/// needed. What this cannot catch is an older shell's `Deny` on `Esc` or with
/// no output to draw on: those arrive at once, like a click.
fn deny_outlived_its_card(deadline: tokio::time::Instant, now: tokio::time::Instant) -> bool {
    let parked_for = CONSENT_PARK_TIMEOUT.saturating_sub(deadline.saturating_duration_since(now));
    parked_for >= CONSENT_CARD_TIMEOUT
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
/// `BACKOFF_BASE` = 100 ms — while session N's `serve` can still be busy
/// answering a client, listener still open: blocked in `write_response` for up
/// to its `WRITE_RESPONSE_TIMEOUT` = 2 s on a client that does not read. (It
/// used to be up to `REQUEST_TIMEOUT` = 5 s on a client that sent nothing,
/// too, until request lines were read off the loop.)
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
#[derive(Debug)]
enum BindOutcome {
    /// This process owns the socket.
    Bound(UnixListener),
    /// A live broker already answers on the path. Stand down: do NOT unlink it.
    StoodDown,
}

/// How the bind sequence sets the socket's own mode (the directory's mode is
/// only ever *checked*, never set — see [`prepare_dir`]). A function pointer
/// purely so the fail-closed chmod path is testable (#1169): a same-uid
/// process's own `chmod(2)` on its own socket does not naturally fail, so
/// making it fail for a test needs an injected seam rather than a second uid
/// — the same reason `hytte_claude_bridge::socket::Chmod` exists.
type Chmod = fn(&Path, u32) -> std::io::Result<()>;

/// The real one.
fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Refuse to bind unless the socket's parent directory is already `0700`
/// (same-uid-only) — checked, not mutated. The broker's socket lives
/// *directly* in `$XDG_RUNTIME_DIR` (`paths.rs:16-19`: deliberately **not** a
/// subdirectory this crate owns), unlike `hytte_claude_bridge::socket::prepare_dir`
/// / `trollshell::plugins::listener`, which each tighten a directory *they*
/// created — copying their chmod onto this path would mutate a directory the
/// broker does not own, shared with Wayland, `PipeWire`, dbus and the shell's
/// own socket dir. `mode & 0o077 == 0` is the exact predicate
/// `hytte_ai_providers::check_key_file_permissions` already uses for the key
/// files (#1169), so both halves of that PR state one rule.
///
/// Normally a no-op: `$XDG_RUNTIME_DIR` (the socket's actual parent) is
/// already `0700` by the XDG runtime-dir spec. A directory that fails the
/// check (a hand-rolled session, a container bind-mount, a non-logind login
/// sharing a loose `/tmp`) means no broker socket at all, with the refusal
/// naming the directory and its mode, rather than either mutating somebody
/// else's directory or serving a socket behind a door group/other can open.
fn prepare_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    let mode = std::fs::metadata(parent)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing to bind inside {} (mode {mode:03o}): readable, writable, or \
                 searchable by group or other, and this directory is not the broker's to \
                 tighten (chmod 700 it)",
                parent.display(),
            ),
        ));
    }
    Ok(())
}

/// Take the broker socket: probe for a live incumbent, then unlink any *stale*
/// socket, bind, and tighten to `0600` (same-user-only, exactly like the host's
/// own plugin socket). The parent is `$XDG_RUNTIME_DIR`, already `0700` — but
/// [`prepare_dir`] checks that invariant and refuses to bind rather than
/// assuming it or mutating the directory to match (#1169).
///
/// The probe is the whole point (#995): this is a probe→unlink→bind sequence,
/// not a lock→probe→bind one — there is no listen lock here (contrast
/// `hytte_claude_bridge::socket::take_socket_with`, which does take one). This runs from
/// `plugin`'s `sources()`, which the SDK calls **after** it writes
/// `Register` but **before** it reads a single host frame — so it runs even for
/// a duplicate the host is about to reject on its `IdGuard`. The old
/// unconditional `remove_file` → `bind` therefore destroyed the *incumbent*
/// broker's socket on every one of the duplicate's ≤5 s redials: the incumbent
/// kept a live listener on an unlinked inode, the path was left holding the
/// duplicate's soon-dead socket, and every `hytte-infobroker` CLI dial got
/// `ECONNREFUSED` while both processes logged success.
///
/// #1169: the socket's own `chmod` to `0600` used to be best-effort
/// (`let _ = …`) — a failure there left the socket at whatever mode the
/// inherited umask produced (`0755`/`0775` on a default login) with nobody
/// told. That is now fatal: a failed chmod unbinds and unlinks the socket
/// rather than serving it at an open mode.
async fn bind_socket(path: &Path) -> std::io::Result<BindOutcome> {
    bind_socket_with(path, chmod).await
}

/// [`bind_socket`] with the mode-setting call injected — see [`Chmod`].
async fn bind_socket_with(path: &Path, chmod: Chmod) -> std::io::Result<BindOutcome> {
    prepare_dir(path)?;
    if socket_in_use(path).await {
        return Ok(BindOutcome::StoodDown);
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    if let Err(e) = chmod(path, 0o600) {
        drop(listener);
        let _ = std::fs::remove_file(path);
        return Err(e);
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
/// restart; the *socket* is not, see `SOCKET`.
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
/// (no `HOME`/`XDG_STATE_HOME`). See [`load_grants_at`] for a file that is
/// there but does not load.
///
/// **Wholly synchronous** — a `std::fs::read_to_string` plus a TOML parse, with
/// no `.await` anywhere inside it — which is the entire reason
/// [`serve_with_grant_loader`] hands it to `tokio::task::spawn_blocking`
/// instead of calling it inline (#1059).
fn load_grants() -> GrantStore {
    load_grants_at(paths::grants_path())
}

/// [`load_grants`] for an explicit path, so the unreadable-file arm is testable
/// without the process environment (`std::env::set_var` is `unsafe` here).
///
/// A `grants.toml` that is there but does not parse, or cannot be read, gets
/// [`GrantStore::unreadable`] (the #1489 follow-up audit, M3): empty, but bound
/// to the path, and refusing to write it. It used to get a path-less store,
/// which never overwrote the file but also never told anyone past this one
/// journal line that every grant change this session would be dropped — see
/// that constructor for the whole story and for why the file is left in place
/// rather than moved aside. The session surfaces the store's state on the
/// panel ([`BrokerSnapshot::store_notice`]), in a toast with its seed, and in
/// the `grants` reply's hint.
fn load_grants_at(path: Option<PathBuf>) -> GrantStore {
    let Some(path) = path else {
        tracing_eprintln("no HOME/XDG_STATE_HOME; grants are in-memory only this session");
        return GrantStore::from_grants(Vec::new());
    };
    GrantStore::load(&path).unwrap_or_else(|e| {
        tracing_eprintln(&format!(
            "{} could not be read ({e}); leaving it untouched and starting with no grants in \
             force — grant changes this session are not saved until it is fixed and the broker \
             restarts",
            path.display()
        ));
        GrantStore::unreadable(path, e)
    })
}

/// [`serve`], with the synchronous grant-load step supplied by the caller —
/// see [`serve_inner`] (this function's whole body, `shutdown: None`) for why
/// the seam is public and what it guards against. `serve` *is*
/// `serve_with_grant_loader(cmds, out, load_grants)`; everything [`serve`]'s
/// doc says applies here unchanged.
#[doc(hidden)]
pub async fn serve_with_grant_loader<L>(
    cmds: mpsc::UnboundedReceiver<Cmd>,
    out: mpsc::UnboundedSender<BrokerMsg>,
    load_grants: L,
) where
    L: FnOnce() -> GrantStore + Send + 'static,
{
    serve_inner(cmds, out, load_grants, None).await;
}

/// Like [`serve`], but wired to the SDK's shutdown hook (#1079, the
/// `hytte-plugin-infobroker` **binary**'s `Infobroker::shutdown` in
/// `src/plugin.rs`): `shutdown` is a oneshot whose payload is itself an ack
/// sender — the caller `send`s an ack channel on it to *request* the drain,
/// and this loop `send`s back on that ack once [`GrantStore::drain`]
/// (including its in-flight `spawn_blocking`) has actually finished, then
/// ends the session rather than continuing to serve. `#[doc(hidden)]` rather
/// than `pub(crate)`: `src/plugin.rs` is a separate bin crate over this same
/// package's library (same shape as `serve`/`serve_with_grant_loader`), so it
/// needs real `pub` visibility — this is still not a seam anything outside
/// that one caller should reach for, and this library stays SDK-free either
/// way (`shutdown` is an ordinary `tokio::sync::oneshot` pair, nothing
/// SDK-shaped).
#[doc(hidden)]
pub async fn serve_with_shutdown(
    cmds: mpsc::UnboundedReceiver<Cmd>,
    out: mpsc::UnboundedSender<BrokerMsg>,
    shutdown: oneshot::Receiver<oneshot::Sender<()>>,
) {
    serve_inner(cmds, out, load_grants, Some(shutdown)).await;
}

/// [`serve`]'s doc, unabridged: `serve` is `serve_with_grant_loader(cmds, out,
/// load_grants)` is `serve_inner(cmds, out, load_grants, None)`, and
/// [`serve_with_shutdown`] is the same body with a live shutdown channel.
/// Split out of the public `serve_with_grant_loader` name (#1079) so adding
/// the shutdown wiring didn't have to touch that function's signature or its
/// existing external callers (`tests/serve_socket_handover.rs` included).
///
/// A session without a socket **still runs**: it seeds the panel (with a
/// [`BrokerSnapshot::notice`] saying why it is not serving) and keeps draining
/// the command lane. Returning early instead left the duplicate's chip painting
/// a default snapshot with dead buttons.
///
/// SDK-free: `cmds`/`out` are plain tokio channels (the plugin passes the SDK's
/// per-session lane ends, which are exactly these types), so this whole module
/// never links the plugin runtime — `shutdown` is the same story, an ordinary
/// `tokio::sync::oneshot` pair rather than anything SDK-shaped.
///
/// # Why the grant-loader seam is public (#1059)
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
/// `serve` runs from `plugin`'s `sources()` under a plain
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
/// mutable borrow and queues the bytes on the store's single-writer lane,
/// whose task does the `spawn_blocking` write in submission order
/// (see [`crate::grants::GrantStore::save`]), and `write_response` offloads
/// `encode_response` past [`LARGE_RESPONSE_ROWS`] the same way.
///
/// Everything after the socket is taken is [`run_session`].
async fn serve_inner<L>(
    cmds: mpsc::UnboundedReceiver<Cmd>,
    out: mpsc::UnboundedSender<BrokerMsg>,
    load_grants: L,
    shutdown: Option<oneshot::Receiver<oneshot::Sender<()>>>,
) where
    L: FnOnce() -> GrantStore + Send + 'static,
{
    let Some(sock) = paths::socket_path() else {
        tracing_eprintln("XDG_RUNTIME_DIR unset; broker socket not created");
        return;
    };

    // #1059: off the session's own thread — see this function's doc comment.
    // A panicking loader is a bug in the loader, not a reason to take the
    // broker down with it: log it and serve with an empty in-memory store.
    // (An unreadable `grants.toml` does not come through here: `load_grants`
    // answers it with `GrantStore::unreadable`, which keeps the path.)
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
    run_session(state, socket.as_ref(), cmds, out, shutdown).await;
}

/// One session's loop-owned state: the broker state, the requests parked on
/// it, and the lane up to the plugin. [`run_session`]'s loop owns it and
/// nothing else touches it — the reader tasks that loop spawns each hold one
/// client stream and no reference into this.
struct Session {
    state: BrokerState,
    out: mpsc::UnboundedSender<BrokerMsg>,
    /// Parked consent requests (#487 phase 1b), keyed by the `request_id` the
    /// broker minted; each holds the client stream open until its decision or
    /// deadline. At most [`MAX_PENDING_CONSENTS`] of them.
    pending: HashMap<u64, PendingConsent>,
    /// Parked datasource queries (#509), keyed by the same minted `request_id`
    /// counter; each holds the client stream open until the plugin relays a
    /// `Cmd::QueryResult` (or the query deadline elapses).
    pending_queries: HashMap<u64, PendingQuery>,
    next_request_id: u64,
}

impl Session {
    fn new(state: BrokerState, out: mpsc::UnboundedSender<BrokerMsg>) -> Self {
        Self {
            state,
            out,
            pending: HashMap::new(),
            pending_queries: HashMap::new(),
            next_request_id: 1,
        }
    }

    /// Push a fresh panel snapshot (+ optional toast) to the plugin.
    fn push_update(&mut self, toast: Option<Toast>) {
        send_update(&self.out, self.state.snapshot(now_unix()), toast);
    }

    /// The next correlation id for a parked request (consent or query).
    fn mint_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        id
    }

    /// The nearest parked deadline (consent OR query), which drives the loop's
    /// timeout arm; `None` = park forever.
    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        self.pending
            .values()
            .map(|p| p.deadline)
            .chain(self.pending_queries.values().map(|q| q.deadline))
            .min()
    }

    /// A panel command or a host relay off the plugin lane.
    async fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Revoke { .. } | Cmd::Allow { .. } | Cmd::Calendar(_) => {
                self.state.apply_cmd(cmd);
                self.push_update(None);
            }
            Cmd::Decision {
                request_id,
                decision,
            } => {
                if let Some(mut p) = self.pending.remove(&request_id) {
                    let (resp, toast) = if matches!(decision, ConsentDecision::Deny)
                        && deny_outlived_its_card(p.deadline, tokio::time::Instant::now())
                    {
                        // An older shell's own timeout, not a click: answer
                        // it as the silence it is (#1601 review, LOW-3).
                        self.state
                            .on_consent_timeout(&p.agent, &p.datasource, now_unix())
                    } else {
                        self.state
                            .apply_consent(&p.agent, &p.datasource, decision, now_unix())
                    };
                    write_response(&mut p.stream, resp).await;
                    self.push_update(toast);
                }
                // else: a late/unknown decision (already timed out) — ignore.
            }
            // #509: a routed query's answer — shape the response from the
            // provider's payload and write it back to the parked client.
            Cmd::QueryResult {
                request_id,
                outcome,
            } => {
                if let Some(mut q) = self.pending_queries.remove(&request_id) {
                    let resp = query_response(&q.datasource, outcome);
                    write_response(&mut q.stream, resp).await;
                }
                // else: a late/unknown result (already timed out) — ignore.
            }
        }
    }

    /// One parsed request, handed back by its reader task.
    async fn on_request(&mut self, mut stream: UnixStream, request: Request) {
        match self.state.dispatch(request) {
            Dispatch::Answer(response, toast) => {
                write_response(&mut stream, response).await;
                self.push_update(toast);
            }
            Dispatch::Consent {
                agent,
                datasource,
                scope,
                detail,
            } => {
                if let Some(refusal) = self.consent_refusal(&agent, &datasource) {
                    write_response(&mut stream, refusal).await;
                    return;
                }
                let request_id = self.mint_request_id();
                self.pending.insert(
                    request_id,
                    PendingConsent {
                        agent: agent.clone(),
                        datasource: datasource.clone(),
                        stream,
                        deadline: tokio::time::Instant::now() + CONSENT_PARK_TIMEOUT,
                    },
                );
                // Ask the shell to prompt the human; the request stays parked
                // until the answering `Cmd::Decision` arrives.
                let _ = self.out.send(BrokerMsg::RequestConsent(ConsentPrompt {
                    request_id,
                    agent,
                    datasource,
                    scope,
                    detail,
                }));
                self.push_update(None);
            }
            // #509: a routed datasource query — park the client and ask the
            // plugin to route it to the shell host; the answering
            // `Cmd::QueryResult` (or the deadline) resolves it.
            Dispatch::Query {
                datasource,
                scope,
                params,
            } => {
                let request_id = self.mint_request_id();
                self.pending_queries.insert(
                    request_id,
                    PendingQuery {
                        datasource: datasource.clone(),
                        stream,
                        deadline: tokio::time::Instant::now() + QUERY_PARK_TIMEOUT,
                    },
                );
                let _ = self.out.send(BrokerMsg::Query(QueryRequest {
                    request_id,
                    provider: datasource,
                    scope,
                    params,
                }));
            }
        }
    }

    /// Why a consent knock for `(agent, datasource)` cannot be parked right
    /// now, as the error to answer it with — or `None` when it can.
    ///
    /// Two refusals, both retryable and neither audited (no human decided
    /// anything). A second knock for a pair that already has one parked is
    /// refused rather than prompted again: the shell's one consent card would
    /// supersede the first prompt with an identical second one, and the first
    /// knock would then wait out its whole [`CONSENT_PARK_TIMEOUT`] for an
    /// answer the human gave on the second card. (Before #1542 item 5 and the
    /// shell's supersede fix, the first card's own countdown sent a `Deny`
    /// this broker persisted, against whatever the human clicked on the
    /// second.) And past [`MAX_PENDING_CONSENTS`] parked knocks
    /// any knock is refused, which bounds the open sockets a stream of knocks
    /// under fresh agent names can park.
    fn consent_refusal(&self, agent: &str, datasource: &str) -> Option<Response> {
        if self
            .pending
            .values()
            .any(|p| p.agent == agent && p.datasource == datasource)
        {
            Some(Response::error(format!(
                "auth: a consent prompt for agent '{agent}' on '{datasource}' is already \
                 waiting on the human; retry once it is answered"
            )))
        } else if self.pending.len() >= MAX_PENDING_CONSENTS {
            Some(Response::error(format!(
                "auth: {MAX_PENDING_CONSENTS} consent prompts are already waiting on the \
                 human; retry later"
            )))
        } else {
            None
        }
    }

    /// The bound elapsed on the earliest parked request(s): time them out. A
    /// consent knock times out with a transient deny + the 1a toast (the
    /// fallback path); a routed query with a transient error.
    async fn on_deadline(&mut self) {
        let now_inst = tokio::time::Instant::now();
        let expired: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now_inst)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(mut p) = self.pending.remove(&id) {
                let (resp, toast) =
                    self.state
                        .on_consent_timeout(&p.agent, &p.datasource, now_unix());
                write_response(&mut p.stream, resp).await;
                self.push_update(toast);
            }
        }
        // #509: expire parked queries the plugin never answered (a wedged /
        // pre-#509 host). A transient error — the agent may retry.
        let expired_q: Vec<u64> = self
            .pending_queries
            .iter()
            .filter(|(_, q)| q.deadline <= now_inst)
            .map(|(id, _)| *id)
            .collect();
        for id in expired_q {
            if let Some(mut q) = self.pending_queries.remove(&id) {
                let resp = Response::error(format!(
                    "{}: datasource query timed out (no host response)",
                    q.datasource
                ));
                write_response(&mut q.stream, resp).await;
            }
        }
    }
}

/// One session, once its socket is settled: seed the panel, then loop over
/// panel commands, the shutdown ask, parked deadlines, requests the reader
/// tasks have read, and new connections, until the command lane closes.
///
/// The loop is the single owner of the [`Session`]. What it does not do is
/// read a request line: each accepted connection gets a [`read_request`]
/// task in `readers`, and only the parsed request comes back to the loop. So a
/// client that connects and says nothing costs one reader slot for
/// [`REQUEST_TIMEOUT`] and stalls nothing else — a human's consent click, a
/// revoke or a query result is handled as soon as it arrives. At
/// [`MAX_CONCURRENT_READS`] the accept arm pauses until a reader finishes.
///
/// The readers belong to the session: when the loop ends, `readers` drops
/// and aborts any still in flight, closing those clients unanswered — the same
/// end a parked consent or query meets, and the honest one, since a token
/// minted by an ending session dies with it.
///
/// Takes the listener rather than finding it (`None` = no socket this
/// session), so a test can drive the loop over a scratch socket without the
/// process-wide [`SOCKET`] or `XDG_RUNTIME_DIR`.
async fn run_session(
    state: BrokerState,
    listener: Option<&UnixListener>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    out: mpsc::UnboundedSender<BrokerMsg>,
    mut shutdown: Option<oneshot::Receiver<oneshot::Sender<()>>>,
) {
    let mut session = Session::new(state, out);
    // Seed the panel with the current grants/status before any request — and,
    // when there is no socket, with the reason. When `grants.toml` could not
    // be read, the seed also carries a toast saying so (M3).
    let toast = session.state.unreadable_store_toast();
    session.push_update(toast);

    let mut readers: JoinSet<Option<(UnixStream, Request)>> = JoinSet::new();
    loop {
        let next_deadline = session.next_deadline();
        tokio::select! {
            // Prefer draining commands (revoke / allow / a consent decision) so a
            // click — or an answer to a live prompt — lands promptly rather than
            // behind a slow fetch; then a due deadline; then the requests already
            // read; and new connections last, so a burst of them can hold up
            // none of the others.
            biased;
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    break; // lane closed → session teardown
                };
                session.on_cmd(cmd).await;
            }
            // #1079: the SDK's shutdown hook asking this session to drain the
            // grant store before the process exits — see `recv_shutdown`.
            // `Some` only for a genuine ask (the ack sender); an ordinary
            // session end (the model dropping its `shutdown_req` sender
            // without ever calling `shutdown`) resolves to `None`, which the
            // `Some(ack) = …` pattern guard disables rather than treats as a
            // shutdown — the `cmd = cmds.recv()` arm above already handles
            // that ordinary teardown, and it is listed first (`biased`) so it
            // wins the tie when both channels close in the same moment.
            Some(ack) = recv_shutdown(&mut shutdown) => {
                session.state.grants.drain().await;
                let _ = ack.send(());
                break;
            }
            () = wait_for_deadline(next_deadline) => session.on_deadline().await,
            // An empty set answers `None` at once, which this pattern turns
            // into "no branch here" for this pass rather than a busy loop.
            Some(read) = readers.join_next() => match read {
                Ok(Some((stream, request))) => session.on_request(stream, request).await,
                // The reader already answered (an error), or the client left.
                Ok(None) => {}
                Err(e) => tracing_eprintln(&format!("request reader failed: {e}")),
            },
            accepted = accept_or_park(listener), if readers.len() < MAX_CONCURRENT_READS => {
                match accepted {
                    Ok((stream, _addr)) => {
                        readers.spawn(read_request(stream));
                    }
                    // Pause before the next lap rather than spin on an error
                    // that is usually still true (`EMFILE`) — see
                    // `ACCEPT_ERROR_PAUSE`.
                    Err(e) => {
                        tracing_eprintln(&format!(
                            "accept error: {e}; retrying in {} ms",
                            ACCEPT_ERROR_PAUSE.as_millis()
                        ));
                        tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
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

    /// #1092 review M3: `recv_shutdown` must be safe to call again after it
    /// already resolved to `None` — `serve_inner`'s `loop` builds a fresh
    /// `select!` (and so a fresh call to this function) every iteration.
    /// Before the fix, a second call re-polled the same already-completed
    /// `oneshot::Receiver`, which panics ("called after complete"); the
    /// bounded `tokio::time::timeout` around the second call is what turns
    /// "safely pending forever" (this function's documented behaviour once
    /// there is nothing left to ask) into a passing assertion instead of an
    /// actual hang — a panic during that `.await` still unwinds straight
    /// through the timeout and fails this test, since a timeout races a
    /// deadline, it does not catch a panic.
    #[tokio::test]
    async fn recv_shutdown_is_safe_to_call_again_after_the_sender_drops() {
        let (tx, rx) = oneshot::channel::<oneshot::Sender<()>>();
        let mut slot = Some(rx);
        drop(tx);

        assert!(
            recv_shutdown(&mut slot).await.is_none(),
            "a dropped sender with no ask must resolve to None, not panic"
        );

        let second = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            recv_shutdown(&mut slot),
        )
        .await;
        assert!(
            second.is_err(),
            "a second call must stay a harmless no-op (pending forever), not panic"
        );
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
        // `tempfile::tempdir()` creates at the ambient umask, not `0700` —
        // this test is about the probe/reclaim sequence, not `prepare_dir`'s
        // parent check, so give it a parent that check accepts.
        chmod(dir.path(), 0o700).expect("chmod 0700");
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

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .expect("exists")
            .permissions()
            .mode()
            & 0o7777
    }

    /// **The boundary, measured** (#1169): the socket is `0600` inside a
    /// `0700` directory, so no second uid can reach it at all, even during the
    /// brief window between `bind` and the socket's own `chmod`. And the
    /// other half of #1169's fix — `prepare_dir` *refuses* to bind at all
    /// through a parent that is not already `0700`, rather than mutating it
    /// to match, because that parent is `$XDG_RUNTIME_DIR` itself, a
    /// directory this crate does not own (see `prepare_dir`'s doc).
    ///
    /// `tempfile::tempdir()` creates its directory with `create_dir` at the
    /// ambient umask (`0755` in this suite's environment, not `mkdir`'s
    /// `0700`), so both halves below chmod the tempdir explicitly rather than
    /// trusting what the harness happened to hand back.
    ///
    /// Mutation: relax `prepare_dir`'s `mode & 0o077 != 0` check (or delete
    /// it) and the refusal half goes red — a bind through a `0755` parent
    /// would succeed instead of refusing. Drop the socket's own
    /// `chmod(path, 0o600)` call and the acceptance half's socket-mode
    /// assertion goes red instead.
    #[tokio::test]
    async fn the_socket_is_0600_inside_a_0700_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hytte-infobroker.sock");

        // A loose parent (readable/searchable by group or other) is refused
        // outright — checked, not tightened — and nothing is left on disk.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod 0755");
        let err = bind_socket(&path)
            .await
            .expect_err("a loose parent must refuse the bind, not mutate it");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!path.exists(), "nothing was bound through a loose parent");

        // A `0700` parent — the normal `$XDG_RUNTIME_DIR` shape — binds fine,
        // and the socket itself comes out `0600`.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0700");
        let BindOutcome::Bound(listener) = bind_socket(&path).await.expect("binds") else {
            panic!("a 0700 parent must bind");
        };

        assert_eq!(mode_of(&path), 0o600, "the socket must be same-uid only");
        assert_eq!(
            mode_of(path.parent().expect("the socket has a parent")),
            0o700,
            "…inside a directory only this uid may even traverse",
        );
        drop(listener);
    }

    /// A [`Chmod`] that refuses the socket's `0600` and does the real thing
    /// otherwise.
    fn chmod_denied_on_socket(path: &Path, mode: u32) -> std::io::Result<()> {
        if mode == 0o600 {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        chmod(path, mode)
    }

    /// **Fail closed on a socket that cannot be tightened** (#1169). Before
    /// this, a failed `chmod` was swallowed (`let _ = …`) and the socket was
    /// served at whatever mode the inherited umask left it — the whole defect.
    /// Now the bind is undone and the file unlinked rather than served.
    ///
    /// Mutation: revert `bind_socket_with`'s `if let Err(e) = chmod(…)` arm to
    /// `let _ = …` and this goes red — the bind reports success and a listener
    /// is left on the path.
    #[tokio::test]
    async fn a_socket_that_cannot_be_tightened_is_unbound_rather_than_served() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Give `prepare_dir`'s parent check a directory it accepts — this
        // test is about the socket's own chmod, not the parent's.
        chmod(dir.path(), 0o700).expect("chmod 0700");
        let path = dir.path().join("hytte-infobroker.sock");

        let err = bind_socket_with(&path, chmod_denied_on_socket)
            .await
            .expect_err("an untightenable socket is a refusal, not a swallowed error");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            !socket_in_use(&path).await,
            "the endpoint must NOT be answering at the umask's mode",
        );
        assert!(
            !path.exists(),
            "…and the file is gone, so the next start's probe cannot mistake it \
             for a stale socket worth reclaiming",
        );
    }

    /// #995 follow-up: a **second session of the same process** must keep the
    /// socket its predecessor bound, not probe it.
    ///
    /// The sequence is the one a `systemctl --user restart trollshell` produces
    /// and it is not rare: the SDK ends session N, waits `BACKOFF_BASE` = 100 ms,
    /// redials and calls `sources()` → a fresh `serve` — while session N's
    /// `serve` can still be busy answering a client (see [`SOCKET`]) with its
    /// listener open. Probing there answers "live" against **our
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
        // Give `prepare_dir`'s parent check a directory it accepts — this
        // test is about ownership vs. probing, not the parent's mode.
        chmod(dir.path(), 0o700).expect("chmod 0700");
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

        // Session 1 has ended, but its listener is still open (it is still
        // answering a client) — so the path IS live. Session 2 starts inside
        // that window.
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

    /// #1074 review M3: the offload **branch** itself, observed rather than
    /// assumed. The timer test above cannot see it — its fixture is
    /// `(0..LARGE_RESPONSE_ROWS)`, so it takes the `spawn_blocking` arm by
    /// construction whatever the constant says, and moving the constant (or
    /// deleting the inline arm outright) left the whole suite green.
    ///
    /// The fixture sizes are **literals on purpose**. Deriving them from
    /// `LARGE_RESPONSE_ROWS` is what made the constant unfalsifiable in the
    /// first place; spelling 11 999 / 12 000 here means moving the threshold
    /// anywhere inside the range the two `const _` asserts allow turns this
    /// red, and the fix is to re-measure the crossing (that constant's doc has
    /// the method) and update all three sites together.
    ///
    /// The injected encoder reports the thread it ran on: the caller's thread
    /// below the threshold, one of the blocking pool's at it.
    #[tokio::test]
    async fn offload_branch_is_taken_exactly_at_the_threshold() {
        for (rows, want_offthread) in [(11_999usize, false), (12_000usize, true)] {
            let (mut server, mut client) =
                UnixStream::pair().expect("a connected socketpair needs no listener at all");
            let drain = tokio::spawn(async move {
                let mut buf = Vec::new();
                let _ = client.read_to_end(&mut buf).await;
            });
            let resp = Response {
                ok: true,
                grants: Some(
                    (0..rows)
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

            let here = std::thread::current().id();
            let (tx, rx) = std::sync::mpsc::channel();
            write_response_with_encoder(&mut server, resp, move |r| {
                let _ = tx.send(std::thread::current().id());
                encode_response(r)
            })
            .await;
            drop(server); // lets the drain task observe EOF

            let ran_on = rx.recv().expect("the encoder must have run exactly once");
            assert_eq!(
                ran_on != here,
                want_offthread,
                "rows={rows}: encoder ran off-thread = {}, wanted {want_offthread} \
                 (LARGE_RESPONSE_ROWS = {LARGE_RESPONSE_ROWS})",
                ran_on != here,
            );
            drain.await.expect("drain task must not panic");
        }
    }

    #[test]
    fn authorize_auth_grants_denies_or_prompts() {
        // An `always` grant → silent mint.
        let s = store(vec![Grant::always("claude", "departures")]);
        assert_eq!(
            authorize_auth(&s, "claude", "departures"),
            AuthOutcome::Granted
        );
        // A stranger with no standing grant now needs a consent decision (#487).
        assert_eq!(
            authorize_auth(&s, "stranger", "departures"),
            AuthOutcome::NeedsConsent
        );
    }

    /// Auth decides per `(agent, datasource)`: the prompt it falls through to
    /// names one datasource, so a grant for another one answers nothing. Before,
    /// any `always` grant minted silently and any `deny` refused silently,
    /// whatever was being asked for.
    #[test]
    fn authorize_auth_decides_about_the_datasource_it_was_asked_for() {
        let s = store(vec![
            Grant::always("claude", "departures"),
            Grant {
                agent: "claude".to_owned(),
                datasource: "weather".to_owned(),
                scope: "*".to_owned(),
                decision: Decision::Deny,
            },
        ]);
        assert_eq!(
            authorize_auth(&s, "claude", "calendar"),
            AuthOutcome::NeedsConsent,
            "a departures grant does not answer a calendar ask — the human is asked"
        );
        assert!(
            matches!(
                authorize_auth(&s, "claude", "weather"),
                AuthOutcome::Denied { .. }
            ),
            "a standing weather deny still refuses a weather ask silently"
        );
        assert_eq!(
            authorize_auth(&s, "claude", "departures"),
            AuthOutcome::Granted,
            "…and the departures grant still mints silently for departures"
        );
    }

    /// The prompt names what the agent asked for, and a request that names
    /// nothing gets exactly the prompt it always did (departures, same detail
    /// line) — the old-client path.
    #[test]
    fn the_consent_knock_names_the_requested_datasource_and_defaults_to_departures() {
        let mut state = BrokerState::new(store(Vec::new()));
        let Dispatch::Consent {
            datasource, detail, ..
        } = state.handle_auth("claude", Some("calendar"))
        else {
            panic!("a no-grant auth for calendar must knock");
        };
        assert_eq!(datasource, "calendar");
        assert_eq!(detail, "claude wants to read your upcoming calendar events");

        let Dispatch::Consent {
            datasource, detail, ..
        } = state.handle_auth("claude", None)
        else {
            panic!("a no-grant auth naming nothing must knock");
        };
        assert_eq!(
            datasource, "departures",
            "an absent datasource is departures"
        );
        assert_eq!(
            detail, "claude wants to read the departures board",
            "the pre-field prompt, byte for byte"
        );
    }

    /// An auth naming a datasource the broker does not serve is refused
    /// outright: no prompt (the human must never be shown a datasource name
    /// the agent invented), no audit entry, no toast.
    #[test]
    fn an_auth_for_an_unknown_datasource_is_refused_without_a_prompt() {
        let mut state = BrokerState::new(store(Vec::new()));
        let (resp, toast) = answer(state.handle_auth("claude", Some("bank-account")));
        assert!(!resp.ok);
        assert!(
            resp.error
                .as_deref()
                .is_some_and(|e| e.contains("unknown datasource 'bank-account'")),
            "{resp:?}"
        );
        assert!(toast.is_none());
        assert!(
            state.audit.is_empty(),
            "nothing was decided, so nothing is audited"
        );
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
        let AuthOutcome::Denied { hint } = authorize_auth(&s, "scratch", "departures") else {
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
        let (resp, toast) = answer(state.handle_auth("claude", None));
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
        } = state.handle_auth("stranger", None)
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
        let (resp, toast) = answer(state.handle_auth("   ", None));
        assert!(!resp.ok);
        assert!(toast.is_none());
        assert!(state.audit.is_empty(), "a blank auth isn't audited");
    }

    /// M2: a name the wire rule refuses is answered with that reason and the
    /// rule as a hint, before anything else — no prompt (so it never reaches
    /// the consent card), no audit row (so never the panel), no toast, no
    /// token. Covers both a name with no grant (which would otherwise prompt)
    /// and one that matches a legacy `grants.toml` row byte for byte (which
    /// would otherwise mint silently).
    ///
    /// Mutation: restore the old blank-only check, and the first name here
    /// comes back as `Dispatch::Consent` — a prompt carrying a newline.
    #[test]
    fn an_auth_with_an_unprintable_or_overlong_name_is_refused_before_anything() {
        let mut state = BrokerState::new(store(vec![Grant::always("old\nrow", "departures")]));
        let long = "x".repeat(MAX_AGENT_NAME_CHARS + 1);
        for name in [
            "claude\n\nThe system updater",
            "evil\u{202E}edualc",
            "old\nrow",
            "\u{1b}[31mred",
            long.as_str(),
        ] {
            let (resp, toast) = answer(state.handle_auth(name, Some("calendar")));
            assert!(!resp.ok, "{name:?} must be refused");
            let error = resp.error.expect("a reason");
            assert!(error.starts_with("auth: invalid agent name:"), "{error}");
            assert!(
                resp.hint.is_some_and(|h| h.contains("1 to 64 characters")),
                "the hint states the rule"
            );
            assert!(toast.is_none(), "{name:?}: no toast");
            assert!(resp.token.is_none(), "{name:?}: no token");
        }
        assert!(state.audit.is_empty(), "no audit row, so no panel row");
        assert!(state.tokens.active(now_unix()).is_empty());
    }

    /// M2: a `grants.toml` whose names are the ones agents actually use still
    /// works after the check — including a name with interior spaces and one
    /// with non-ASCII letters — and a row the check would now refuse loads
    /// without costing the file (inert: nothing can auth as it any more, and
    /// the panel can still revoke it).
    #[test]
    fn existing_grants_with_valid_names_still_mint_silently() {
        let mut state = BrokerState::new(store(vec![
            Grant::always("claude", "departures"),
            Grant::always("Claude Code", "calendar"),
            Grant::always("Kätzchen", "weather"),
            Grant::always("old\nrow", "departures"),
        ]));
        for (agent, datasource) in [
            ("claude", "departures"),
            ("Claude Code", "calendar"),
            ("Kätzchen", "weather"),
        ] {
            let (resp, toast) = answer(state.handle_auth(agent, Some(datasource)));
            assert!(resp.ok, "{agent:?} must still mint: {:?}", resp.error);
            assert!(toast.is_none());
        }
        state.apply_cmd(Cmd::Revoke {
            agent: "old\nrow".to_owned(),
            datasource: "departures".to_owned(),
        });
        assert_eq!(state.grants.grants().len(), 3, "the legacy row revokes");
    }

    /// M2, `handle_get`'s half: a `get` for a datasource the broker does not
    /// serve is refused without an audit row, so the panel's activity list
    /// never renders the one free-text string a `get` carries.
    ///
    /// Mutation: restore the `record` call, and the ring holds the newline.
    #[test]
    fn a_get_for_an_unknown_datasource_leaves_no_audit_row() {
        let mut state = BrokerState::new(store(Vec::new()));
        let token = state.tokens.mint("claude", now_unix());
        let (resp, _) = get_answer(state.handle_get(&token.value, "stocks\n[infobroker] x", None));
        assert!(!resp.ok);
        assert!(
            state.audit.is_empty(),
            "{:?}",
            state.snapshot(now_unix()).audit
        );
    }

    /// M3: a `grants.toml` that does not parse becomes a store bound to its
    /// path that will not write it — and every surface says so: the panel's
    /// `store_notice`, the seed toast, and the `grants` reply's hint, each
    /// naming the file and the parse error. A file that loads says nothing.
    ///
    /// Mutation: restore the old `GrantStore::from_grants(Vec::new())`
    /// fallback, and all three are `None`.
    #[test]
    fn an_unparsable_grants_file_is_named_on_every_surface_and_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        let corrupt = "[[grant]]\nagent = \"claude\"\ndecision = \n";
        std::fs::write(&path, corrupt).expect("seed");

        let mut state = BrokerState::new(load_grants_at(Some(path.clone())));
        assert_eq!(
            state.grants.path(),
            Some(path.as_path()),
            "the path is kept"
        );
        let shown = path.display().to_string();

        let notice = state
            .snapshot(now_unix())
            .store_notice
            .expect("a panel notice");
        assert!(notice.starts_with("Not saving grant changes:"), "{notice}");
        assert!(
            notice.contains(&shown) && notice.contains("line 3"),
            "{notice}"
        );
        let toast = state.unreadable_store_toast().expect("a seed toast");
        assert!(toast.body.contains(&shown), "{toast:?}");
        let hint = state.handle_grants().hint.expect("a grants hint");
        assert!(hint.contains(&shown), "{hint}");

        // Changes still apply for the session, and the file is untouched.
        state.apply_cmd(Cmd::Allow {
            agent: "claude".to_owned(),
            datasource: "departures".to_owned(),
        });
        assert!(matches!(
            state.handle_auth("claude", None),
            Dispatch::Answer(Response { ok: true, .. }, None)
        ));
        assert_eq!(std::fs::read_to_string(&path).expect("reads"), corrupt);

        // A file that loads is quiet on all three.
        std::fs::write(&path, "").expect("an empty file is an empty store");
        let state = BrokerState::new(load_grants_at(Some(path)));
        assert_eq!(state.unreadable_store_toast(), None);
        assert_eq!(state.handle_grants().hint, None);
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
            authorize_auth(&state.grants, "scratch", "departures"),
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
            authorize_auth(&state.grants, "claude", "departures"),
            AuthOutcome::NeedsConsent
        );
    }

    /// Whether a routed `get` was let through (it became a query) — the
    /// allowed answer for `departures`/`weather`, whose rows a provider plugin
    /// supplies. A denial is an inline `Answer`.
    fn routed(d: &GetDisposition) -> bool {
        matches!(d, GetDisposition::Query { .. })
    }

    #[test]
    fn get_authority_follows_the_token_scope() {
        let mut state = BrokerState::new(store(Vec::new()));
        // A session token authorizes its datasource with no durable grant (the
        // routed query is the "allowed" answer; the fetch itself is a provider's).
        let session = state.tokens.mint_scoped(
            "claude",
            now_unix(),
            TokenScope::Session {
                datasource: "departures".to_owned(),
            },
        );
        assert!(routed(&state.handle_get(
            &session.value,
            "departures",
            None
        )));
        assert!(
            routed(&state.handle_get(&session.value, "departures", None)),
            "a session token is not spent by a fetch"
        );

        // A once token authorizes exactly one fetch of its datasource.
        let once = state.tokens.mint_scoped(
            "claude",
            now_unix(),
            TokenScope::Once {
                datasource: "departures".to_owned(),
            },
        );
        assert!(
            routed(&state.handle_get(&once.value, "departures", None)),
            "first fetch allowed"
        );
        let (resp, toast) = get_answer(state.handle_get(&once.value, "departures", None));
        assert!(!resp.ok, "the once authority is spent after one fetch");
        assert!(toast.is_none(), "a spent once is transient, not a knock");

        // A Grant-scoped token with NO durable grant is denied (no toast).
        let identity = state.tokens.mint("stranger", now_unix()); // Grant scope
        let (resp, toast) = get_answer(state.handle_get(&identity.value, "departures", None));
        assert!(!resp.ok, "an identity token without a grant can't fetch");
        assert!(toast.is_none(), "a scope miss is transient, not a knock");
    }

    /// **The finding**, end to end through the broker's own handlers: the
    /// human answers "This session" on a prompt that read "claude wants: read
    /// access from departures", and that token then reads the calendar. Before
    /// this, `handle_get` took `TokenScope::Session` as "every datasource" —
    /// the prompt said departures, the authority said everything.
    ///
    /// Mutation: restore `TokenScope::Session => true` in `handle_get` (or make
    /// `TokenScope::covers` answer `true`) and the calendar `get` comes back
    /// `ok` with the rows.
    #[test]
    fn an_allow_session_token_for_departures_cannot_read_the_calendar() {
        let mut state = BrokerState::new(store(Vec::new()));
        state.apply_cmd(Cmd::Calendar(calendar_fixture()));
        // The knock an old client gets (no datasource on the wire) names
        // departures…
        let Dispatch::Consent {
            agent, datasource, ..
        } = state.handle_auth("claude", None)
        else {
            panic!("a no-grant auth must knock for consent");
        };
        assert_eq!(datasource, "departures");
        // …and the human answers that prompt with "This session".
        let (resp, _) = state.apply_consent(
            &agent,
            &datasource,
            ConsentDecision::AllowSession,
            now_unix(),
        );
        let token = resp.token.expect("an allow mints a token");

        let (resp, toast) = get_answer(state.handle_get(&token, "calendar", None));
        assert!(
            !resp.ok,
            "a departures consent must not read the calendar: {resp:?}"
        );
        assert!(resp.calendar.is_none(), "no calendar rows leak");
        assert!(toast.is_none(), "a scope miss is transient, not a knock");
        assert!(
            resp.hint
                .as_deref()
                .is_some_and(|h| h.contains("--datasource calendar")),
            "the hint says how to ask for the calendar: {resp:?}"
        );
        assert!(
            !routed(&state.handle_get(&token, "weather", None)),
            "…nor route a weather query"
        );
        assert!(
            routed(&state.handle_get(&token, "departures", None)),
            "what the prompt named still reads"
        );
    }

    /// "Allow once" is scoped the same way, and a get for another datasource
    /// is refused **without** spending the once — so the one fetch the human
    /// allowed is still there for what they allowed it for.
    #[test]
    fn an_allow_once_token_reads_only_its_own_datasource_and_only_once() {
        let mut state = BrokerState::new(store(Vec::new()));
        state.apply_cmd(Cmd::Calendar(calendar_fixture()));
        let (resp, _) =
            state.apply_consent("claude", "calendar", ConsentDecision::AllowOnce, now_unix());
        let token = resp.token.expect("minted");

        assert!(
            !routed(&state.handle_get(&token, "departures", None)),
            "a calendar once must not route a departures query"
        );
        let (resp, _) = get_answer(state.handle_get(&token, "calendar", None));
        assert!(resp.ok, "the refused departures get did not spend the once");
        assert_eq!(resp.calendar.expect("rows").len(), 2);
        let (resp, _) = get_answer(state.handle_get(&token, "calendar", None));
        assert!(!resp.ok, "…and the once is spent after its one fetch");
    }

    /// A consent token still identifies its agent, so the agent's durable
    /// grants answer for it exactly as they do for a Grant-scoped token: the
    /// scoping removes only what nobody consented to. And a read the grant
    /// already covers does not spend a once on the way.
    #[test]
    fn a_consent_token_still_reaches_what_the_agents_durable_grants_cover() {
        let mut state = BrokerState::new(store(vec![Grant::always("claude", "calendar")]));
        state.apply_cmd(Cmd::Calendar(calendar_fixture()));
        let (resp, _) = state.apply_consent(
            "claude",
            "departures",
            ConsentDecision::AllowOnce,
            now_unix(),
        );
        let token = resp.token.expect("minted");
        let (resp, _) = get_answer(state.handle_get(&token, "calendar", None));
        assert!(resp.ok, "the durable calendar grant covers it: {resp:?}");
        assert!(
            routed(&state.handle_get(&token, "departures", None)),
            "the departures once is still unspent"
        );
    }

    /// The panel's pending row offers to Allow what the agent asked for: a
    /// denied calendar knock is a calendar row, not the departures row every
    /// denial used to make (all a phase-1a auth could ask for). A get for a
    /// datasource the broker does not serve makes no row — there is nothing to
    /// grant.
    #[test]
    fn a_denied_knock_offers_its_own_datasource_in_the_panel() {
        let mut state = BrokerState::new(store(Vec::new()));
        let _ = state.apply_consent("claude", "calendar", ConsentDecision::Deny, now_unix());
        let stranger = state.tokens.mint("stranger", now_unix());
        let _ = state.handle_get(&stranger.value, "stocks", None);

        assert_eq!(
            state.snapshot(now_unix()).pending,
            vec![PendingView {
                agent: "claude".to_owned(),
                datasource: "calendar".to_owned(),
            }],
        );
    }

    /// #1556: a loop re-asking under a standing deny toasts once, but every
    /// refusal is still audited.
    #[test]
    fn a_standing_deny_toasts_once_and_audits_every_refusal() {
        let mut state = BrokerState::new(store(vec![Grant {
            agent: "scratch".to_owned(),
            datasource: "departures".to_owned(),
            scope: "*".to_owned(),
            decision: Decision::Deny,
        }]));
        let mut toasts = 0;
        for _ in 0..40 {
            let (resp, toast) = answer(state.handle_auth("scratch", Some("departures")));
            assert!(!resp.ok);
            toasts += usize::from(toast.is_some());
        }
        assert_eq!(toasts, 1, "the flood raises one toast, not one per refusal");
        // The ring is AUDIT_CAP-bounded, so a full ring of Denied is the proof
        // every refusal reached `record` (40 > AUDIT_CAP).
        assert_eq!(state.audit.len(), AUDIT_CAP);
        assert!(state.audit.iter().all(|e| e.outcome == Outcome::Denied));
    }

    #[test]
    fn deny_toast_limiter_first_per_key_then_window_then_again() {
        let mut l = DenyToastLimiter::default();
        let t0 = 1_000_000;
        assert!(l.admit("a", "departures", t0), "first refusal toasts");
        assert!(!l.admit("a", "departures", t0 + 1));
        assert!(!l.admit("a", "departures", t0 + DENY_TOAST_WINDOW_SECS - 1));
        assert!(
            l.admit("a", "departures", t0 + DENY_TOAST_WINDOW_SECS),
            "toasts again once the window has passed"
        );
        assert!(!l.admit("a", "departures", t0 + DENY_TOAST_WINDOW_SECS + 1));
    }

    #[test]
    fn deny_toast_limiter_keys_are_independent() {
        let mut l = DenyToastLimiter::default();
        let t0 = 1_000_000;
        assert!(l.admit("a", "departures", t0));
        assert!(l.admit("b", "departures", t0), "another agent toasts");
        assert!(l.admit("a", "weather", t0), "another datasource toasts");
        assert!(!l.admit("b", "departures", t0 + 1));
    }

    /// Fresh agent names are the starvation route: the table stays bounded, and
    /// the global burst keeps the toasts themselves bounded too.
    #[test]
    fn deny_toast_limiter_stays_bounded_under_10k_distinct_keys() {
        let mut l = DenyToastLimiter::default();
        let t0 = 1_000_000;
        let mut toasts = 0;
        for i in 0..10_000 {
            // Spread over many windows so the burst refills and the key cap,
            // not the burst, is what is exercised.
            let now = t0 + i64::from(i) * DENY_TOAST_WINDOW_SECS;
            toasts += usize::from(l.admit(&format!("agent{i}"), "departures", now));
            assert!(l.last.len() <= DENY_TOAST_KEYS_CAP);
        }
        assert_eq!(toasts, 10_000);
        assert_eq!(l.last.len(), DENY_TOAST_KEYS_CAP);

        // Inside one window, 10k fresh names raise at most the burst.
        let mut l = DenyToastLimiter::default();
        let toasts = (0..10_000)
            .filter(|i| l.admit(&format!("agent{i}"), "departures", t0))
            .count();
        assert_eq!(toasts, DENY_TOAST_GLOBAL_BURST as usize);
        assert!(l.last.len() <= DENY_TOAST_KEYS_CAP);
    }

    #[test]
    fn deny_toast_limiter_evicts_the_oldest_key() {
        let mut l = DenyToastLimiter::default();
        let t0 = 1_000_000;
        // Oldest first, one window apart so the burst never gates.
        for i in 0..DENY_TOAST_KEYS_CAP {
            let now = t0 + i64::try_from(i).unwrap() * DENY_TOAST_WINDOW_SECS;
            assert!(l.admit(&format!("a{i}"), "departures", now));
        }
        let now = t0 + i64::try_from(DENY_TOAST_KEYS_CAP).unwrap() * DENY_TOAST_WINDOW_SECS;
        assert!(l.admit("fresh", "departures", now));
        assert_eq!(l.last.len(), DENY_TOAST_KEYS_CAP);
        assert!(
            !l.last
                .contains_key(&("a0".to_owned(), "departures".to_owned()))
        );
        assert!(
            l.last
                .contains_key(&("fresh".to_owned(), "departures".to_owned()))
        );
    }

    #[test]
    fn audit_ring_is_capped() {
        let mut state = BrokerState::new(store(Vec::new()));
        for _ in 0..(AUDIT_CAP + 5) {
            state.record("claude", "auth", "departures", Outcome::Denied, now_unix());
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
        let token = answer(state.handle_auth("claude", None))
            .0
            .token
            .expect("token");
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
        let token = answer(state.handle_auth("claude", Some("calendar")))
            .0
            .token
            .expect("token");
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
        let token = answer(state.handle_auth("claude", None))
            .0
            .token
            .expect("token");
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
        // A departures session token authorizes a departures get; departures now
        // routes a query rather than fetching inline (#509 dedup).
        let mut state = BrokerState::new(store(Vec::new()));
        let token = state.tokens.mint_scoped(
            "claude",
            now_unix(),
            TokenScope::Session {
                datasource: "departures".to_owned(),
            },
        );
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
        let token = answer(state.handle_auth("claude", Some("weather")))
            .0
            .token
            .expect("token");
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

    // ── The session loop, over a scratch socket ────────────────────────────────

    /// How long a test below gives an answer the loop should produce at once.
    /// Generous for a local socket round trip, and far under the
    /// [`REQUEST_TIMEOUT`] (5 s) a stalled loop would make it wait.
    const PROMPT: std::time::Duration = std::time::Duration::from_secs(1);

    /// How long a test sleeps so the loop can accept a connection it just
    /// made. There is no observable for "accepted"; a local accept on this
    /// same runtime takes microseconds.
    const ACCEPT_SETTLE: std::time::Duration = std::time::Duration::from_millis(100);

    /// A listener in a scratch `0700` dir — `run_session` takes its listener
    /// as an argument, so these tests need neither the process-wide `SOCKET`
    /// nor `XDG_RUNTIME_DIR`. The `TempDir` must outlive the test.
    fn scratch_listener() -> (tempfile::TempDir, std::path::PathBuf, UnixListener) {
        let dir = tempfile::tempdir().expect("tempdir");
        chmod(dir.path(), 0o700).expect("chmod 0700");
        let path = dir.path().join("hytte-infobroker.sock");
        let listener = UnixListener::bind(&path).expect("bind the scratch socket");
        (dir, path, listener)
    }

    /// Send one request line.
    async fn send_line(stream: &mut UnixStream, line: &str) {
        stream
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write the request line");
    }

    /// Read one response line (`""` on EOF).
    async fn read_reply(stream: &mut UnixStream) -> String {
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .await
            .expect("read the response line");
        line
    }

    /// Connect, send `line`, and return the stream for the reply.
    async fn knock(path: &Path, line: &str) -> UnixStream {
        let mut stream = UnixStream::connect(path).await.expect("connect");
        send_line(&mut stream, line).await;
        stream
    }

    /// The next consent prompt the session raises, skipping panel updates.
    async fn next_prompt(out: &mut mpsc::UnboundedReceiver<BrokerMsg>) -> ConsentPrompt {
        let wait = async {
            loop {
                match out.recv().await.expect("the session's out lane is open") {
                    BrokerMsg::RequestConsent(prompt) => return prompt,
                    BrokerMsg::Update { .. } | BrokerMsg::Query(_) => {}
                }
            }
        };
        tokio::time::timeout(PROMPT, wait)
            .await
            .expect("the session raises the consent prompt promptly")
    }

    /// Every prompt still queued on `out` (the session has had its turn).
    fn queued_prompts(out: &mut mpsc::UnboundedReceiver<BrokerMsg>) -> Vec<ConsentPrompt> {
        let mut prompts = Vec::new();
        while let Ok(msg) = out.try_recv() {
            if let BrokerMsg::RequestConsent(prompt) = msg {
                prompts.push(prompt);
            }
        }
        prompts
    }

    /// Item 2, the reviewers' scenario: a consent prompt is out, and a client
    /// connects and sends nothing. The human's answer must still reach the
    /// parked agent at once, and another client must still be served —
    /// neither may wait out the silent client's [`REQUEST_TIMEOUT`].
    ///
    /// Before the request read moved off the loop, `handle_conn` awaited it
    /// inside the accept arm: the loop sat in the silent client's `read_line`
    /// for 5 s with the `Cmd::Decision` queued behind it, and a client that kept
    /// reconnecting could hold it there past the consent park deadline.
    ///
    /// Mutation: `.await` `read_request(stream)` inside the accept arm (and hand
    /// its result straight to `on_request`) instead of spawning it into
    /// `readers` — the decision then lands only after the silent client times
    /// out, and the `PROMPT`-bounded read of the token fails.
    #[tokio::test]
    async fn a_silent_client_holds_up_neither_a_consent_answer_nor_another_client() {
        let (_dir, path, listener) = scratch_listener();
        let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let session = run_session(
            BrokerState::new(store(Vec::new())),
            Some(&listener),
            cmds_rx,
            out_tx,
            None,
        );
        let drive = async move {
            // An agent knocks; the broker parks it and raises the prompt.
            let mut asker = knock(
                &path,
                r#"{"op":"auth","agent":"claude","datasource":"calendar"}"#,
            )
            .await;
            let prompt = next_prompt(&mut out_rx).await;
            assert_eq!(prompt.datasource, "calendar", "the prompt names the ask");

            // A client connects and says nothing; let the loop accept it.
            let silent = UnixStream::connect(&path).await.expect("connect");
            tokio::time::sleep(ACCEPT_SETTLE).await;

            // The human answers: the token must arrive now, not in 5 s.
            cmds_tx
                .send(Cmd::Decision {
                    request_id: prompt.request_id,
                    decision: ConsentDecision::AllowSession,
                })
                .expect("the session is alive");
            let reply = tokio::time::timeout(PROMPT, read_reply(&mut asker))
                .await
                .expect("a consent answer must not wait out a silent client's request timeout");
            assert!(reply.contains(r#""token":"#), "{reply}");

            // And a second client is served now, too.
            let mut other = knock(&path, r#"{"op":"grants"}"#).await;
            let reply = tokio::time::timeout(PROMPT, read_reply(&mut other))
                .await
                .expect("a second client must not wait out a silent one");
            assert!(reply.contains(r#""ok":true"#), "{reply}");

            drop(silent);
            drop(cmds_tx); // ends the session
        };
        tokio::join!(session, drive);
    }

    /// Item 3: a client that streams bytes with no newline is refused **at
    /// the cap**, with an error naming it — not buffered for the whole
    /// [`REQUEST_TIMEOUT`] and then answered "timed out".
    ///
    /// Mutation: drop the `.take(limit)` (or the overflow arm) from
    /// `read_request` and the reader keeps reading, so the bounded wait below
    /// fails.
    #[tokio::test]
    async fn a_request_line_past_the_cap_is_refused_at_the_cap() {
        let (server, mut client) = UnixStream::pair().expect("a connected socketpair");
        let reader = tokio::spawn(read_request(server));
        // One byte past the cap, no newline. Fits the socket buffer, so the
        // write completes whether or not the reader is draining it.
        client
            .write_all(&vec![b'x'; MAX_REQUEST_LINE + 1])
            .await
            .expect("write the oversized line");
        let read = tokio::time::timeout(PROMPT, reader)
            .await
            .expect("an overflowing line is refused at the cap, not at the request timeout")
            .expect("the reader does not panic");
        assert!(read.is_none(), "an overflowing line is not a request");
        let reply = read_reply(&mut client).await;
        assert!(
            reply.contains(&format!("longer than {MAX_REQUEST_LINE} bytes")),
            "the refusal names the cap: {reply}"
        );
    }

    /// Item 3, the cap's other edge: a line of exactly [`MAX_REQUEST_LINE`]
    /// bytes plus its newline is a request, read whole and parsed.
    #[tokio::test]
    async fn a_request_line_at_the_cap_is_read_whole() {
        let (server, mut client) = UnixStream::pair().expect("a connected socketpair");
        let op = r#"{"op":"grants"}"#;
        let line = format!("{op}{}\n", " ".repeat(MAX_REQUEST_LINE - op.len()));
        assert_eq!(line.len(), MAX_REQUEST_LINE + 1, "the cap plus the newline");
        let reader = tokio::spawn(read_request(server));
        client.write_all(line.as_bytes()).await.expect("write");
        let read = tokio::time::timeout(PROMPT, reader)
            .await
            .expect("a line at the cap reads promptly")
            .expect("the reader does not panic");
        let (_stream, request) = read.expect("a line at the cap is a request");
        assert_eq!(request, Request::Grants);
    }

    /// Item 4: a second knock for an `(agent, datasource)` pair that already
    /// has one parked is refused at once with a retryable error, and raises no
    /// second prompt — the shell would otherwise supersede the first card with
    /// an identical one whose answer the first one's own timer later
    /// contradicts. A knock for another datasource still prompts.
    #[tokio::test]
    async fn a_second_knock_for_a_parked_pair_is_refused_without_a_second_prompt() {
        let (_dir, path, listener) = scratch_listener();
        let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let session = run_session(
            BrokerState::new(store(Vec::new())),
            Some(&listener),
            cmds_rx,
            out_tx,
            None,
        );
        let drive = async move {
            let _first = knock(&path, r#"{"op":"auth","agent":"claude"}"#).await;
            let first = next_prompt(&mut out_rx).await;

            let mut again = knock(&path, r#"{"op":"auth","agent":"claude"}"#).await;
            let reply = tokio::time::timeout(PROMPT, read_reply(&mut again))
                .await
                .expect("the duplicate is answered at once");
            assert!(reply.contains("already waiting"), "{reply}");
            assert!(
                queued_prompts(&mut out_rx).is_empty(),
                "the duplicate raised no second prompt"
            );

            let _other = knock(
                &path,
                r#"{"op":"auth","agent":"claude","datasource":"weather"}"#,
            )
            .await;
            let other = next_prompt(&mut out_rx).await;
            assert_eq!(other.datasource, "weather", "another pair still prompts");
            assert_ne!(other.request_id, first.request_id);

            drop(cmds_tx);
        };
        tokio::join!(session, drive);
    }

    /// M2 over the socket: an `auth` whose name carries a newline is answered
    /// with the refusal on the wire, and nothing goes up the lane — no
    /// consent prompt for the shell to put on screen, no panel update.
    #[tokio::test]
    async fn a_bad_agent_name_is_refused_on_the_wire_without_a_prompt() {
        let (_dir, path, listener) = scratch_listener();
        let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let session = run_session(
            BrokerState::new(store(Vec::new())),
            Some(&listener),
            cmds_rx,
            out_tx,
            None,
        );
        let drive = async move {
            // The seed snapshot.
            assert!(matches!(
                out_rx.recv().await,
                Some(BrokerMsg::Update { toast: None, .. })
            ));
            let mut asker = knock(
                &path,
                r#"{"op":"auth","agent":"claude\nwants your passwords"}"#,
            )
            .await;
            let reply = tokio::time::timeout(PROMPT, read_reply(&mut asker))
                .await
                .expect("refused at once");
            let reply: Response = serde_json::from_str(&reply).expect("one response line");
            assert!(!reply.ok);
            assert!(
                reply
                    .error
                    .is_some_and(|e| e.starts_with("auth: invalid agent name:")),
                "the refusal says why"
            );
            // Whatever the session sends after answering it (a panel update) must
            // not be a prompt.
            tokio::time::sleep(ACCEPT_SETTLE).await;
            assert!(queued_prompts(&mut out_rx).is_empty(), "no consent prompt");
            drop(cmds_tx);
        };
        tokio::join!(session, drive);
    }

    /// M3: a session whose grant store could not be loaded raises a toast
    /// with its seed snapshot — the one surface a human who only ever
    /// answers consent cards will see — and the snapshot carries the notice.
    #[tokio::test]
    async fn a_session_over_an_unreadable_grants_file_toasts_with_its_seed() {
        let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let state = BrokerState::new(GrantStore::unreadable(
            "/nonexistent/grants.toml",
            "grants.toml: line 1, column 1: bad",
        ));
        let session = run_session(state, None, cmds_rx, out_tx, None);
        let drive = async move {
            let Some(BrokerMsg::Update { snapshot, toast }) = out_rx.recv().await else {
                panic!("the seed is an update");
            };
            assert!(snapshot.store_notice.is_some(), "the panel says so");
            let toast = toast.expect("the seed toasts");
            assert!(toast.body.contains("/nonexistent/grants.toml"), "{toast:?}");
            drop(cmds_tx);
        };
        tokio::join!(session, drive);
    }

    /// Item 4: past [`MAX_PENDING_CONSENTS`] parked knocks, any knock — even
    /// under a fresh agent name, which the per-pair refusal cannot catch — is
    /// refused with a retryable error instead of parking another socket.
    #[tokio::test]
    async fn knocks_past_the_pending_cap_are_refused() {
        let (_dir, path, listener) = scratch_listener();
        let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let session = run_session(
            BrokerState::new(store(Vec::new())),
            Some(&listener),
            cmds_rx,
            out_tx,
            None,
        );
        let drive = async move {
            let mut parked = Vec::new();
            for i in 0..MAX_PENDING_CONSENTS {
                let line = format!(r#"{{"op":"auth","agent":"agent-{i}"}}"#);
                parked.push(knock(&path, &line).await);
                next_prompt(&mut out_rx).await;
            }
            let mut over = knock(&path, r#"{"op":"auth","agent":"one-too-many"}"#).await;
            let reply = tokio::time::timeout(PROMPT, read_reply(&mut over))
                .await
                .expect("the knock past the cap is answered at once");
            assert!(
                reply.contains(&format!("{MAX_PENDING_CONSENTS} consent prompts")),
                "{reply}"
            );
            assert!(
                queued_prompts(&mut out_rx).is_empty(),
                "no prompt past the cap"
            );
            drop(parked);
            drop(cmds_tx);
        };
        tokio::join!(session, drive);
    }

    // ── #1542 item 5: silence persists nothing, a Deny click does ──────────────
    //
    // These drive one `Session` directly rather than `run_session`, so the
    // park deadline can be made to pass without a test waiting out the real
    // `CONSENT_PARK_TIMEOUT` (65 s).

    /// Park one `auth` knock from `agent` (no datasource, so `departures`) on
    /// `session`, returning the agent's end of the socket and the prompt the
    /// session raised for it.
    async fn park_knock(
        session: &mut Session,
        out: &mut mpsc::UnboundedReceiver<BrokerMsg>,
        agent: &str,
    ) -> (UnixStream, ConsentPrompt) {
        let (server, client) = UnixStream::pair().expect("a connected socketpair");
        session
            .on_request(
                server,
                Request::Auth {
                    agent: agent.to_owned(),
                    datasource: None,
                },
            )
            .await;
        (client, next_prompt(out).await)
    }

    /// The one response line the session wrote on `stream`, parsed.
    async fn reply_on(stream: &mut UnixStream) -> Response {
        let line = tokio::time::timeout(PROMPT, read_reply(stream))
            .await
            .expect("the session answered the parked agent");
        serde_json::from_str(&line).expect("one response line")
    }

    /// Let every parked knock's park deadline pass, as the loop's deadline arm
    /// does once [`CONSENT_PARK_TIMEOUT`] has elapsed with no decision.
    async fn park_deadline_passes(session: &mut Session) {
        let now = tokio::time::Instant::now();
        for parked in session.pending.values_mut() {
            parked.deadline = now;
        }
        session.on_deadline().await;
    }

    /// #1542 item 5, the broker's half. A grant card nobody answers — it times
    /// out, or is dismissed with `Esc`, or is superseded, or has no output to
    /// be drawn on — sends **no** decision, so no `Cmd::Decision` arrives and
    /// the park deadline is what answers the knock: a transient refusal the
    /// agent may retry, with nothing persisted. The plugin binary's
    /// `an_unanswered_card_puts_no_decision_on_the_broker_lane` pins the other
    /// half, that nothing does arrive.
    ///
    /// Falsification: have `on_deadline` resolve an expired knock with
    /// `apply_consent(…, ConsentDecision::Deny, …)` — the old "silence is a
    /// deny" outcome — and this reds: a standing deny is persisted, the reply
    /// says "consent denied", and the retry is refused without a prompt. Have
    /// `next_deadline` look at parked queries only and the first assertion
    /// reds: the loop would never wake for an unanswered knock.
    #[tokio::test]
    async fn an_unanswered_consent_knock_persists_nothing_and_is_refused_transiently() {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let mut session = Session::new(BrokerState::new(store(Vec::new())), out_tx);
        let (mut agent, _prompt) = park_knock(&mut session, &mut out_rx, "claude").await;

        // The loop sleeps until `next_deadline`, so a parked knock must be in
        // it: since #1542 item 5 its park deadline is the only answer a silent
        // card gets (#1601 review, LOW-1).
        assert_eq!(
            session.next_deadline(),
            session.pending.values().map(|p| p.deadline).min(),
            "the loop must wake for a parked knock: its deadline is the only answer silence gets"
        );
        assert!(session.next_deadline().is_some());

        // Nobody answers the card; the shell sends nothing.
        park_deadline_passes(&mut session).await;

        let reply = reply_on(&mut agent).await;
        assert!(!reply.ok, "{reply:?}");
        assert!(
            reply
                .error
                .as_deref()
                .is_some_and(|e| e.contains("timed out")),
            "the agent hears the transient timeout refusal: {reply:?}"
        );
        assert_eq!(
            session.state.grants.decision_for("claude", "departures"),
            None,
            "an unanswered card persists nothing"
        );
        assert!(session.pending.is_empty(), "the knock is answered and gone");

        // Retryable: the agent's next knock goes back to the human rather
        // than meeting a standing no.
        let (_again, prompt) = park_knock(&mut session, &mut out_rx, "claude").await;
        assert_eq!(
            (prompt.agent.as_str(), prompt.datasource.as_str()),
            ("claude", "departures")
        );
    }

    /// The control: a **click** on Deny is the one answer that persists a
    /// standing no, and it still does — the reply says so, `grants.toml` gets
    /// the row, and the agent's next knock is refused without a prompt.
    ///
    /// Falsification: skip the `grant_deny` in `apply_consent`'s `Deny` arm
    /// (the over-correction: "a deny never persists") and this reds.
    #[tokio::test]
    async fn an_explicit_deny_click_persists_a_standing_no() {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let mut session = Session::new(BrokerState::new(store(Vec::new())), out_tx);
        let (mut agent, prompt) = park_knock(&mut session, &mut out_rx, "claude").await;

        session
            .on_cmd(Cmd::Decision {
                request_id: prompt.request_id,
                decision: ConsentDecision::Deny,
            })
            .await;

        let reply = reply_on(&mut agent).await;
        assert!(!reply.ok, "{reply:?}");
        assert!(
            reply
                .error
                .as_deref()
                .is_some_and(|e| e.contains("consent denied")),
            "{reply:?}"
        );
        assert_eq!(
            session.state.grants.decision_for("claude", "departures"),
            Some(Decision::Deny),
            "a Deny click persists a standing no"
        );

        // The next knock meets that standing no: refused, and not re-prompted.
        let (server, mut again) = UnixStream::pair().expect("a connected socketpair");
        session
            .on_request(
                server,
                Request::Auth {
                    agent: "claude".to_owned(),
                    datasource: None,
                },
            )
            .await;
        let reply = reply_on(&mut again).await;
        assert!(!reply.ok, "{reply:?}");
        assert!(
            queued_prompts(&mut out_rx).is_empty(),
            "a standing no is not put to the human again"
        );
    }

    // ── #1601 review, LOW-3: an older shell's timeout Deny ─────────────────────

    /// Where the broker stops taking a `Deny` for a click: the moment the
    /// knock's card closes, [`CONSENT_CARD_TIMEOUT`] after the knock parked.
    /// No card can be clicked after that, so a `Deny` from then on is an
    /// older shell's own timer.
    ///
    /// Falsification: `>=` → `>` in `deny_outlived_its_card` reds the 60 s
    /// row; comparing against [`CONSENT_PARK_TIMEOUT`] instead reds it too.
    #[test]
    fn a_deny_counts_as_a_click_only_while_its_card_could_be_up() {
        use std::time::Duration;
        let parked = tokio::time::Instant::now();
        let deadline = parked + CONSENT_PARK_TIMEOUT;
        let at = |after: Duration| parked + after;
        assert!(!deny_outlived_its_card(deadline, at(Duration::ZERO)));
        assert!(!deny_outlived_its_card(
            deadline,
            at(Duration::from_secs(59))
        ));
        assert!(!deny_outlived_its_card(
            deadline,
            at(Duration::from_millis(59_999))
        ));
        assert!(
            deny_outlived_its_card(deadline, at(Duration::from_mins(1))),
            "the card is gone at 60 s: a Deny then is its timer"
        );
        assert!(deny_outlived_its_card(
            deadline,
            at(Duration::from_secs(64))
        ));
        assert!(
            deny_outlived_its_card(deadline, at(Duration::from_secs(66))),
            "past the park deadline too"
        );
    }

    /// Make every parked knock look as if it parked `ago` before now.
    fn knock_parked_for(session: &mut Session, ago: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + CONSENT_PARK_TIMEOUT - ago;
        for parked in session.pending.values_mut() {
            parked.deadline = deadline;
        }
    }

    /// The guard end to end, on both sides of its line: a `Deny` 59 s after
    /// the knock parked is a click and persists a standing no; one 60 s after
    /// is an older shell's timeout and is answered like silence — the
    /// transient "timed out" refusal, nothing persisted.
    ///
    /// Falsification: drop the guard from `Session::on_cmd` (always
    /// `apply_consent`) and the 60 s row reds: a deny row is persisted.
    #[tokio::test]
    async fn a_deny_after_the_card_closed_is_answered_as_silence() {
        for (parked_secs, persists) in [(59, true), (60, false)] {
            let (out_tx, mut out_rx) = mpsc::unbounded_channel();
            let mut session = Session::new(BrokerState::new(store(Vec::new())), out_tx);
            let (mut agent, prompt) = park_knock(&mut session, &mut out_rx, "claude").await;
            knock_parked_for(&mut session, std::time::Duration::from_secs(parked_secs));

            session
                .on_cmd(Cmd::Decision {
                    request_id: prompt.request_id,
                    decision: ConsentDecision::Deny,
                })
                .await;

            let reply = reply_on(&mut agent).await;
            assert!(!reply.ok, "{parked_secs} s: {reply:?}");
            let error = reply.error.unwrap_or_default();
            let persisted = session.state.grants.decision_for("claude", "departures");
            if persists {
                assert!(error.contains("consent denied"), "{parked_secs} s: {error}");
                assert_eq!(persisted, Some(Decision::Deny), "{parked_secs} s");
            } else {
                assert!(error.contains("timed out"), "{parked_secs} s: {error}");
                assert_eq!(persisted, None, "{parked_secs} s: nothing persisted");
            }
        }
    }

    /// The guard is about `Deny` only: no shell's timer ever sent an allow,
    /// so an allow that arrives 60 s or more after its knock parked is a
    /// click on a card that went up late, and it still mints (#1601
    /// re-review, NIT-1).
    ///
    /// Falsification: drop the `matches!(decision, ConsentDecision::Deny)`
    /// half of the guard in `Session::on_cmd`, so every late decision is
    /// answered as silence, and this reds: no token.
    #[tokio::test]
    async fn a_late_allow_still_mints() {
        for decision in [
            ConsentDecision::AllowOnce,
            ConsentDecision::AllowSession,
            ConsentDecision::AllowAlways,
        ] {
            let (out_tx, mut out_rx) = mpsc::unbounded_channel();
            let mut session = Session::new(BrokerState::new(store(Vec::new())), out_tx);
            let (mut agent, prompt) = park_knock(&mut session, &mut out_rx, "claude").await;
            knock_parked_for(&mut session, std::time::Duration::from_mins(1));

            session
                .on_cmd(Cmd::Decision {
                    request_id: prompt.request_id,
                    decision,
                })
                .await;

            let reply = reply_on(&mut agent).await;
            assert!(reply.ok, "{decision:?} at 60 s: {reply:?}");
            assert!(reply.token.is_some(), "{decision:?} at 60 s mints a token");
        }
    }
}
