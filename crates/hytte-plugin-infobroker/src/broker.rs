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
//! Consent policy (phase 1a — interactive Allow/Deny prompting is 1b):
//! - `auth` mints a token **silently** iff an `always` grant covers the agent;
//!   otherwise it is **denied** with a hint, and an informational
//!   [`Toast`] is raised so the human sees the knock.
//! - `get <datasource>` requires a valid token *and* an `always` grant for that
//!   `(agent, datasource)`; a missing grant is denied + toasted the same way.
//! - An **invalid/expired token** is a transient technical failure (the agent
//!   just re-auths), so it is denied *without* a toast — only genuine consent
//!   knocks alert the human.

use std::collections::VecDeque;
use std::path::Path;

use chrono::Utc;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::grants::{Decision, GrantStore};
use crate::tokens::TokenStore;
use crate::wire::{
    DATASOURCE_DEPARTURES, GrantOut, Request, Response, encode_response, parse_request,
};
use crate::{departures, paths};

/// Cap on the in-memory audit ring shown in the panel. Oldest entries fall off.
const AUDIT_CAP: usize = 30;

/// How long a connected client has to send its one request line before the
/// broker gives up on it (so a stuck client can't wedge the accept loop).
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// ── Pure consent decisions (unit-tested) ──────────────────────────────────────

/// The outcome of an `auth` request against the grant store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    /// The agent has an `always` grant — mint a token silently.
    Granted,
    /// No `always` grant covers the agent — deny, with a how-to-grant hint.
    Denied { hint: String },
}

/// The outcome of a `get <datasource>` request against the grant store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GetOutcome {
    /// An `always` grant covers `(agent, datasource)` — serve the fetch.
    Allowed,
    /// No `always` grant (missing or an explicit `deny`) — deny + hint.
    Denied { hint: String },
}

/// The how-to-grant hint pointing the human at the two grant surfaces.
fn grant_hint(agent: &str, datasource: &str) -> String {
    format!(
        "allow it in the infobroker panel, or add a grant to grants.toml: \
         [[grant]] agent = \"{agent}\" datasource = \"{datasource}\" decision = \"always\""
    )
}

/// Decide an `auth`: granted iff the agent already has any `always` grant.
#[must_use]
pub fn authorize_auth(grants: &GrantStore, agent: &str) -> AuthOutcome {
    if grants.has_any_always(agent) {
        AuthOutcome::Granted
    } else {
        AuthOutcome::Denied {
            hint: grant_hint(agent, DATASOURCE_DEPARTURES),
        }
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

/// A command from the plugin's panel down to the broker task (the #280 lane
/// pattern). Both variants mutate the durable grant store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmd {
    /// Delete `(agent, datasource)`'s grant and kill that agent's live tokens.
    Revoke { agent: String, datasource: String },
    /// Add an `always` grant for `(agent, datasource)` — the panel's Allow.
    Allow { agent: String, datasource: String },
}

/// An informational toast the plugin should post via `Effect::Notify`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub summary: String,
    pub body: String,
}

/// The broker → plugin message: a fresh panel snapshot, plus an optional toast
/// to raise (present only on a consent denial). One message type keeps the
/// plugin reducer a one-liner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrokerMsg {
    Update {
        snapshot: BrokerSnapshot,
        toast: Option<Toast>,
    },
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

/// The broker's owned state: durable grants, ephemeral tokens, the audit ring.
struct BrokerState {
    grants: GrantStore,
    tokens: TokenStore,
    audit: VecDeque<AuditEntry>,
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

        let datasources = vec![DatasourceView {
            name: DATASOURCE_DEPARTURES.to_owned(),
            status: departures::status(),
        }];

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
        }
    }

    /// Handle one client connection: read its request line, dispatch, write the
    /// response line. Returns a [`Toast`] iff the request was a consent denial.
    async fn handle_conn(&mut self, stream: UnixStream) -> Option<Toast> {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let read = tokio::time::timeout(REQUEST_TIMEOUT, reader.read_line(&mut line)).await;
        let (response, toast) = match read {
            Ok(Ok(0)) => return None, // client closed without a request
            Ok(Ok(_)) => self.dispatch(line.trim()).await,
            Ok(Err(e)) => (Response::error(format!("read error: {e}")), None),
            Err(_) => (Response::error("timed out waiting for a request"), None),
        };
        let mut out = encode_response(&response);
        out.push('\n');
        let mut stream = reader.into_inner();
        if let Err(e) = stream.write_all(out.as_bytes()).await {
            tracing_eprintln(&format!("writing response failed: {e}"));
        }
        let _ = stream.flush().await;
        toast
    }

    /// Dispatch one parsed request to its handler.
    async fn dispatch(&mut self, line: &str) -> (Response, Option<Toast>) {
        let req = match parse_request(line) {
            Ok(req) => req,
            Err(e) => return (Response::error(e), None),
        };
        match req {
            Request::Auth { agent } => self.handle_auth(&agent),
            Request::Get {
                token,
                datasource,
                limit,
            } => self.handle_get(&token, &datasource, limit).await,
            Request::Grants => (self.handle_grants(), None),
        }
    }

    /// `auth`: mint silently on an `always` grant, else deny + toast.
    fn handle_auth(&mut self, agent: &str) -> (Response, Option<Toast>) {
        let now = now_unix();
        if agent.trim().is_empty() {
            return (Response::error("auth: empty agent name"), None);
        }
        match authorize_auth(&self.grants, agent) {
            AuthOutcome::Granted => {
                let token = self.tokens.mint(agent, now);
                self.record(agent, "auth", Outcome::Granted, now);
                let resp = Response {
                    ok: true,
                    token: Some(token.value),
                    expires_unix: Some(token.expires_unix),
                    agent: Some(agent.to_owned()),
                    ..Response::default()
                };
                (resp, None)
            }
            AuthOutcome::Denied { hint } => {
                self.record(agent, "auth", Outcome::Denied, now);
                let resp = Response::denied(format!("no grant for agent '{agent}'"), hint);
                (resp, Some(denied_toast(agent, "access")))
            }
        }
    }

    /// `get <datasource>`: token → agent → grant → scoped fetch.
    async fn handle_get(
        &mut self,
        token: &str,
        datasource: &str,
        limit: Option<usize>,
    ) -> (Response, Option<Toast>) {
        let now = now_unix();
        let Some(agent) = self.tokens.agent_for(token, now) else {
            // Transient/technical — the agent just re-auths; no consent toast.
            return (
                Response::error("invalid or expired token — re-auth with `infobroker auth`"),
                None,
            );
        };
        if datasource != DATASOURCE_DEPARTURES {
            self.record(&agent, datasource, Outcome::Denied, now);
            return (
                Response::denied(
                    format!("unknown datasource '{datasource}'"),
                    format!("the only datasource in phase 1a is '{DATASOURCE_DEPARTURES}'"),
                ),
                None,
            );
        }
        match authorize_get(&self.grants, &agent, datasource) {
            GetOutcome::Allowed => {
                // The access decision is granted; the fetch itself may still fail
                // (network/config) — that's not a consent problem, so no toast.
                let result = tokio::task::spawn_blocking(move || departures::fetch_scoped(limit))
                    .await
                    .unwrap_or_else(|e| Err(format!("join: {e}")));
                self.record(&agent, datasource, Outcome::Granted, now);
                match result {
                    Ok(rows) => {
                        let resp = Response {
                            ok: true,
                            datasource: Some(datasource.to_owned()),
                            departures: Some(rows),
                            ..Response::default()
                        };
                        (resp, None)
                    }
                    Err(e) => (
                        Response::error(format!("departures fetch failed: {e}")),
                        None,
                    ),
                }
            }
            GetOutcome::Denied { hint } => {
                self.record(&agent, datasource, Outcome::Denied, now);
                let resp = Response::denied(
                    format!("agent '{agent}' has no grant for '{datasource}'"),
                    hint,
                );
                (resp, Some(denied_toast(&agent, datasource)))
            }
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

/// stderr diagnostic — systemd routes it to the journal (the SDK plugin uses
/// stderr for diagnostics; `tracing` isn't wired on the plugin side).
fn tracing_eprintln(msg: &str) {
    eprintln!("[infobroker] {msg}");
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
    let _ = out.send(BrokerMsg::Update {
        snapshot: state.snapshot(now_unix()),
        toast: None,
    });

    loop {
        tokio::select! {
            // Prefer draining panel commands (revoke/allow) so a click lands
            // promptly rather than behind a slow fetch.
            biased;
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    break; // lane closed → session teardown
                };
                state.apply_cmd(cmd);
                let _ = out.send(BrokerMsg::Update {
                    snapshot: state.snapshot(now_unix()),
                    toast: None,
                });
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let toast = state.handle_conn(stream).await;
                        let _ = out.send(BrokerMsg::Update {
                            snapshot: state.snapshot(now_unix()),
                            toast,
                        });
                    }
                    Err(e) => tracing_eprintln(&format!("accept error: {e}")),
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
    fn auth_granted_only_with_an_always_grant() {
        let s = store(vec![Grant::always("claude", "departures")]);
        assert_eq!(authorize_auth(&s, "claude"), AuthOutcome::Granted);
        // An unknown agent is denied with an actionable hint.
        let AuthOutcome::Denied { hint } = authorize_auth(&s, "stranger") else {
            panic!("expected denial");
        };
        assert!(
            hint.contains("grants.toml"),
            "hint points at the grant surface: {hint}"
        );
        assert!(hint.contains("infobroker panel"));
    }

    #[test]
    fn auth_denied_for_a_deny_only_agent() {
        let s = store(vec![Grant {
            agent: "scratch".to_owned(),
            datasource: "departures".to_owned(),
            scope: "*".to_owned(),
            decision: Decision::Deny,
        }]);
        assert!(matches!(
            authorize_auth(&s, "scratch"),
            AuthOutcome::Denied { .. }
        ));
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

    #[test]
    fn handle_auth_mints_and_audits_then_get_serves_decision() {
        let mut state = BrokerState::new(store(vec![Grant::always("claude", "departures")]));
        // Auth mints a token and records a granted audit entry.
        let (resp, toast) = state.handle_auth("claude");
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
    fn denied_auth_toasts_and_surfaces_as_pending() {
        let mut state = BrokerState::new(store(Vec::new()));
        let (resp, toast) = state.handle_auth("stranger");
        assert!(!resp.ok);
        assert!(resp.hint.is_some(), "a denial carries a how-to-grant hint");
        let toast = toast.expect("a denied knock raises a toast");
        assert!(toast.summary.contains("stranger"));
        assert!(toast.body.contains("infobroker panel"));
        // It shows up as a pending Allow the human can click.
        let snap = state.snapshot(now_unix());
        assert_eq!(
            snap.pending,
            vec![PendingView {
                agent: "stranger".to_owned(),
                datasource: "departures".to_owned(),
            }]
        );
    }

    #[test]
    fn empty_agent_is_rejected_without_a_toast() {
        let mut state = BrokerState::new(store(Vec::new()));
        let (resp, toast) = state.handle_auth("   ");
        assert!(!resp.ok);
        assert!(toast.is_none());
        assert!(state.audit.is_empty(), "a blank auth isn't audited");
    }

    #[test]
    fn allow_command_clears_pending_and_revoke_kills_tokens() {
        let mut state = BrokerState::new(store(Vec::new()));
        // A knock leaves the agent pending.
        state.handle_auth("claude");
        assert_eq!(state.snapshot(now_unix()).pending.len(), 1);

        // Allow grants always → pending clears, auth now mints.
        state.apply_cmd(Cmd::Allow {
            agent: "claude".to_owned(),
            datasource: "departures".to_owned(),
        });
        assert!(
            state.snapshot(now_unix()).pending.is_empty(),
            "allow clears pending"
        );
        let (resp, _) = state.handle_auth("claude");
        let token = resp.token.expect("now mints");
        assert!(state.tokens.agent_for(&token, now_unix()).is_some());

        // Revoke deletes the grant AND kills the live token.
        state.apply_cmd(Cmd::Revoke {
            agent: "claude".to_owned(),
            datasource: "departures".to_owned(),
        });
        assert!(state.grants.decision_for("claude", "departures").is_none());
        assert!(
            state.tokens.agent_for(&token, now_unix()).is_none(),
            "revoking a grant invalidates its live tokens"
        );
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
        // Async handler, but no live fetch is reached on the bad-token path.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let mut state = BrokerState::new(store(vec![Grant::always("claude", "departures")]));
            let (resp, toast) = state.handle_get("not-a-token", "departures", None).await;
            assert!(!resp.ok);
            assert!(
                toast.is_none(),
                "an expired/invalid token is transient, not a consent knock"
            );
            assert!(resp.error.unwrap().contains("re-auth"));
        });
    }

    #[test]
    fn get_unknown_datasource_is_denied_without_a_toast() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let mut state = BrokerState::new(store(vec![Grant::always("claude", "departures")]));
            let token = state.handle_auth("claude").0.token.expect("token");
            let (resp, toast) = state.handle_get(&token, "weather", None).await;
            assert!(!resp.ok);
            assert!(
                toast.is_none(),
                "an unknown datasource is a request error, not a consent knock"
            );
            assert!(resp.error.unwrap().contains("unknown datasource"));
        });
    }
}
