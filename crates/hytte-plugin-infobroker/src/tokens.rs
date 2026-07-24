//! The ephemeral half of the design (issue #487): in-memory session tokens.
//!
//! A token is a bearer "session cookie" the CLI exports into an agent's
//! environment (`HYTTE_INFOBROKER_TOKEN`). It carries no policy of its own — it
//! merely proves "this is agent X"; the durable [`grants`](crate::grants) decide
//! what X may read. A token dies at the **first** of: an explicit revoke
//! ([`TokenStore::revoke_agent`], driven by the panel), a broker/shell restart
//! (this store is never persisted), or the [`DEFAULT_TTL_SECS`] backstop.
//!
//! Everything is keyed on an injected `now_unix`, so the TTL machine is
//! unit-testable without a wall clock.

use std::fmt::Write as _;

/// The TTL backstop: 12 hours. A leaked token dies within this window even if
/// nothing revokes it and the shell never restarts.
pub const DEFAULT_TTL_SECS: i64 = 12 * 60 * 60;

/// The data-access authority a token carries — decided by the consent choice
/// that minted it (#487 phase 1b). Identity (which agent the token *is*) is
/// orthogonal; this is what a `get` request the token authenticates may fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TokenScope {
    /// Backed by a durable `always` grant (a phase-1a silent mint, or an
    /// `AllowAlways` consent): data access is authorized by the grant store and
    /// this token is pure identity. The default — a token minted the 1a way.
    #[default]
    Grant,
    /// `AllowSession`: this token itself authorizes data access for its whole
    /// life, with no durable grant. Dies with the session (restart / revoke /
    /// TTL) like every token.
    Session,
    /// `AllowOnce`: authorizes exactly one fetch, then its data authority is
    /// spent — a further fetch on the same token is denied until the agent
    /// re-auths and the human re-decides. The token still identifies the agent.
    Once,
}

/// One live session token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The opaque bearer value (32 hex chars of OS randomness).
    pub value: String,
    /// The agent identity this token authenticates as.
    pub agent: String,
    /// The data-access authority this token carries (#487 phase 1b).
    pub scope: TokenScope,
    /// Whether a [`TokenScope::Once`] token's single fetch has been spent.
    /// Meaningless (always `false`) for the other scopes.
    pub spent: bool,
    /// When it was minted, unix seconds.
    pub minted_unix: i64,
    /// Absolute expiry, unix seconds (`minted_unix + ttl`).
    pub expires_unix: i64,
}

impl Token {
    /// Whether this token is expired at `now_unix` (expiry is inclusive-past:
    /// a token is dead the instant `now >= expires`).
    #[must_use]
    pub fn is_expired(&self, now_unix: i64) -> bool {
        now_unix >= self.expires_unix
    }
}

/// A resolved token's authority — everything a `get` needs to decide access
/// without re-scanning the store: the [`agent`](TokenAuthority::agent) it
/// authenticates as, the [`scope`](TokenAuthority::scope) it was minted with,
/// and whether a [`TokenScope::Once`] token's fetch is already
/// [`spent`](TokenAuthority::spent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenAuthority {
    pub agent: String,
    pub scope: TokenScope,
    pub spent: bool,
}

/// The in-memory token set. Not `Clone` on purpose — there is exactly one, owned
/// by the broker task.
#[derive(Debug)]
pub struct TokenStore {
    tokens: Vec<Token>,
    ttl_secs: i64,
}

impl Default for TokenStore {
    fn default() -> Self {
        Self::with_ttl(DEFAULT_TTL_SECS)
    }
}

impl TokenStore {
    /// A store with a custom TTL (tests use a short one; production uses the
    /// [`Default`], i.e. [`DEFAULT_TTL_SECS`]).
    #[must_use]
    pub fn with_ttl(ttl_secs: i64) -> Self {
        Self {
            tokens: Vec::new(),
            ttl_secs,
        }
    }

    /// Mint a fresh [`TokenScope::Grant`] token — the phase-1a shape (a silent
    /// mint backed by an `always` grant). Delegates to [`mint_scoped`](TokenStore::mint_scoped).
    pub fn mint(&mut self, agent: &str, now_unix: i64) -> Token {
        self.mint_scoped(agent, now_unix, TokenScope::Grant)
    }

    /// Mint a fresh token for `agent` carrying `scope`, expiring `ttl` after
    /// `now_unix`. The value is fresh OS randomness, so it's unguessable and
    /// unique. Returns a clone of the stored token (the caller hands its
    /// `value`/`expires_unix` to the CLI). The `scope` records which consent
    /// choice minted it, so a `get` can later be authorized off the token alone
    /// (#487 phase 1b).
    pub fn mint_scoped(&mut self, agent: &str, now_unix: i64, scope: TokenScope) -> Token {
        let token = Token {
            value: random_value(),
            agent: agent.to_owned(),
            scope,
            spent: false,
            minted_unix: now_unix,
            expires_unix: now_unix + self.ttl_secs,
        };
        self.tokens.push(token.clone());
        token
    }

    /// Resolve a bearer `value` to its agent at `now_unix`, pruning expired
    /// tokens as a side effect. `None` = unknown or expired value (the agent must
    /// re-auth).
    pub fn agent_for(&mut self, value: &str, now_unix: i64) -> Option<String> {
        self.resolve(value, now_unix).map(|t| t.agent)
    }

    /// Resolve a bearer `value` to its `(agent, scope, spent)` at `now_unix`,
    /// pruning expired tokens first. `None` = unknown or expired value. The
    /// `get` path consults `scope`/`spent` to decide data authority (#487 phase
    /// 1b); [`agent_for`](TokenStore::agent_for) is the identity-only shorthand.
    pub fn resolve(&mut self, value: &str, now_unix: i64) -> Option<TokenAuthority> {
        self.prune(now_unix);
        self.tokens
            .iter()
            .find(|t| t.value == value)
            .map(|t| TokenAuthority {
                agent: t.agent.clone(),
                scope: t.scope,
                spent: t.spent,
            })
    }

    /// Spend a [`TokenScope::Once`] token's single fetch authority. Returns
    /// `true` if it flipped an as-yet-unspent once-token to spent (so the caller
    /// only counts a genuine consumption). A no-op — returning `false` — for an
    /// unknown token or any non-`Once` / already-spent one.
    pub fn spend_once(&mut self, value: &str) -> bool {
        if let Some(t) = self
            .tokens
            .iter_mut()
            .find(|t| t.value == value && t.scope == TokenScope::Once && !t.spent)
        {
            t.spent = true;
            true
        } else {
            false
        }
    }

    /// Drop every token belonging to `agent` (the panel's revoke kill-switch, and
    /// the "revoking a grant invalidates its live tokens" rule). Returns how many
    /// were killed.
    pub fn revoke_agent(&mut self, agent: &str) -> usize {
        let before = self.tokens.len();
        self.tokens.retain(|t| t.agent != agent);
        before - self.tokens.len()
    }

    /// Drop every expired token. Called on each resolve; also usable on a timer.
    pub fn prune(&mut self, now_unix: i64) {
        self.tokens.retain(|t| !t.is_expired(now_unix));
    }

    /// The live (non-expired) tokens at `now_unix`, for the panel's status
    /// readout. Prunes first so the view never shows a dead token.
    pub fn active(&mut self, now_unix: i64) -> &[Token] {
        self.prune(now_unix);
        &self.tokens
    }
}

/// 16 bytes of OS randomness, hex-encoded to a 32-char token. Reads exactly 16
/// bytes from `/dev/urandom` (no extra crate; `read_exact`, never a full-file
/// read — `/dev/urandom` has no EOF). The fallback path — used only if that read
/// ever fails — mixes the wall clock with a per-call counter so a token is still
/// unique, just not cryptographically strong (never hit in practice on Linux,
/// where `/dev/urandom` always reads).
fn random_value() -> String {
    use std::io::Read as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut bytes = [0u8; 16];
    let read_ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !read_ok {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_nanos()).ok())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        bytes[..8].copy_from_slice(&nanos.to_le_bytes());
        bytes[8..].copy_from_slice(&seq.to_le_bytes());
    }
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Infallible: writing to a String never errors.
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_750_000_000;

    #[test]
    fn mint_then_resolve_within_ttl() {
        let mut store = TokenStore::with_ttl(100);
        let tok = store.mint("claude", NOW);
        assert_eq!(tok.expires_unix, NOW + 100);
        assert_eq!(
            store.agent_for(&tok.value, NOW + 50).as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn token_expires_at_ttl_and_is_pruned() {
        let mut store = TokenStore::with_ttl(100);
        let tok = store.mint("claude", NOW);
        // Exactly at expiry it is already dead (>= is inclusive-past).
        assert!(store.agent_for(&tok.value, NOW + 100).is_none());
        // …and pruned, so it's gone from the store, not merely hidden.
        assert!(store.active(NOW + 100).is_empty());
    }

    #[test]
    fn unknown_value_never_resolves() {
        let mut store = TokenStore::with_ttl(100);
        store.mint("claude", NOW);
        assert!(store.agent_for("not-a-real-token", NOW).is_none());
    }

    #[test]
    fn minted_values_are_distinct() {
        let mut store = TokenStore::default();
        let a = store.mint("claude", NOW);
        let b = store.mint("claude", NOW);
        assert_ne!(a.value, b.value, "each mint is fresh randomness");
        assert_eq!(a.value.len(), 32, "16 bytes → 32 hex chars");
    }

    #[test]
    fn revoke_agent_kills_all_that_agents_tokens() {
        let mut store = TokenStore::with_ttl(1000);
        let a1 = store.mint("claude", NOW);
        let a2 = store.mint("claude", NOW);
        let other = store.mint("scratch", NOW);
        assert_eq!(
            store.revoke_agent("claude"),
            2,
            "both of claude's tokens die"
        );
        assert!(store.agent_for(&a1.value, NOW).is_none());
        assert!(store.agent_for(&a2.value, NOW).is_none());
        // Another agent's token is untouched.
        assert_eq!(
            store.agent_for(&other.value, NOW).as_deref(),
            Some("scratch")
        );
    }

    #[test]
    fn active_lists_only_live_tokens() {
        let mut store = TokenStore::with_ttl(100);
        store.mint("claude", NOW);
        store.mint("scratch", NOW);
        assert_eq!(store.active(NOW + 50).len(), 2);
        assert_eq!(store.active(NOW + 200).len(), 0, "all expired");
    }

    #[test]
    fn mint_defaults_to_grant_scope_and_mint_scoped_records_the_choice() {
        let mut store = TokenStore::with_ttl(100);
        // The 1a shape: a bare `mint` is a Grant-scoped identity token.
        let g = store.mint("claude", NOW);
        assert_eq!(g.scope, TokenScope::Grant);
        assert!(!g.spent);
        // The consent shapes carry their scope.
        let s = store.mint_scoped("claude", NOW, TokenScope::Session);
        assert_eq!(s.scope, TokenScope::Session);
        let o = store.mint_scoped("claude", NOW, TokenScope::Once);
        assert_eq!(o.scope, TokenScope::Once);
    }

    #[test]
    fn resolve_returns_agent_scope_and_spent() {
        let mut store = TokenStore::with_ttl(100);
        let tok = store.mint_scoped("claude", NOW, TokenScope::Session);
        let auth = store.resolve(&tok.value, NOW + 10).expect("resolves");
        assert_eq!(
            auth,
            TokenAuthority {
                agent: "claude".to_owned(),
                scope: TokenScope::Session,
                spent: false,
            }
        );
        // An expired token resolves to nothing (and `agent_for` agrees).
        assert!(store.resolve(&tok.value, NOW + 100).is_none());
        assert!(store.agent_for(&tok.value, NOW + 100).is_none());
    }

    #[test]
    fn spend_once_is_single_use_and_only_touches_once_tokens() {
        let mut store = TokenStore::with_ttl(1000);
        let once = store.mint_scoped("claude", NOW, TokenScope::Once);
        // First spend flips it; the second is a no-op (already spent).
        assert!(
            store.spend_once(&once.value),
            "first fetch consumes the once"
        );
        assert!(
            !store.spend_once(&once.value),
            "a spent once-token can't be spent again"
        );
        // The resolved authority now reports it spent.
        assert!(
            store
                .resolve(&once.value, NOW)
                .expect("still resolves")
                .spent
        );

        // A session/grant token is never a once — spend_once leaves it alone.
        let session = store.mint_scoped("claude", NOW, TokenScope::Session);
        assert!(!store.spend_once(&session.value));
        assert!(!store.resolve(&session.value, NOW).expect("resolves").spent);
        // Unknown token: no-op.
        assert!(!store.spend_once("not-a-token"));
    }
}
