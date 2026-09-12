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
            .find(|t| tokens_match(&t.value, value))
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
            .find(|t| tokens_match(&t.value, value) && t.scope == TokenScope::Once && !t.spent)
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

/// Constant-time equality for a stored token value against a client-supplied
/// bearer `value` (#1169). This socket is local, same-uid-only (#995's
/// `bind_socket` — see [`crate::broker`]), so the timing channel a naive
/// `==` opens is a same-uid one, not a network attacker's — but closing it
/// costs four lines, so there is no reason to leave a bearer comparison
/// short-circuiting on the first differing byte.
///
/// No new dependency (`subtle` is not a direct dependency of this crate, and
/// this workspace adds no `Cargo.lock` entry for a fix this small): a length
/// check — revealing *how long* a token is leaks nothing a fixed-length
/// `random_value` doesn't already fix — followed by an XOR-fold over every
/// byte pair, so the number of operations depends only on the length, never
/// on *where* a mismatch falls.
fn tokens_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Fill `bytes` from `read`, with no second branch that synthesizes a value
/// on failure (#1162 lens 7 item 3).
///
/// Before this, a failed `/dev/urandom` read fell back to the wall clock plus
/// a per-call counter — not just weaker than OS randomness but a value an
/// attacker with a rough idea of process start time could guess outright,
/// which defeats the entire property a bearer token exists for: this value
/// authenticates an agent to the broker, so "unguessable" is not negotiable
/// just because the read that produces it is expected to always succeed on
/// Linux. Split out from [`random_value`] as its own function — taking the
/// read as a parameter rather than hardcoding `/dev/urandom` — purely so a
/// test can inject a failing reader and observe the refusal without needing
/// root or a broken VM to make the real device unreadable.
fn read_random_bytes(
    read: impl FnOnce(&mut [u8; 16]) -> std::io::Result<()>,
) -> std::io::Result<[u8; 16]> {
    let mut bytes = [0u8; 16];
    read(&mut bytes)?;
    Ok(bytes)
}

/// 16 bytes of OS randomness, hex-encoded to a 32-char token. Reads exactly 16
/// bytes from `/dev/urandom` (no extra crate; `read_exact`, never a full-file
/// read — `/dev/urandom` has no EOF).
///
/// **Refuses rather than falls back** (#1162 lens 7 item 3): a failed read
/// panics naming `/dev/urandom` instead of minting a guessable value. This is
/// reachable only from [`TokenStore::mint_scoped`], whose signature — and
/// every call site in `broker.rs` — returns a bare [`Token`], not a
/// `Result`; threading a `Result` up through that public API for a read that
/// is, in practice, never observed to fail on Linux would push a fallible
/// path onto every caller for a "recoverable" condition that in fact means
/// the box's randomness source is broken in a way nothing downstream can
/// paper over. A broken `/dev/urandom` is exactly the kind of failure a loud
/// crash is right for.
fn random_value() -> String {
    use std::io::Read as _;

    let bytes = read_random_bytes(|buf| std::fs::File::open("/dev/urandom")?.read_exact(buf))
        .unwrap_or_else(|e| {
            panic!("hytte-infobroker: refusing to mint a session token: /dev/urandom: {e}")
        });
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

    /// #1169: the compare is by value, not by identity — equal tokens match
    /// regardless of length or content, one differing byte anywhere refuses,
    /// and a length mismatch refuses without even reaching the fold.
    ///
    /// Falsification: revert `tokens_match` to `a == b` and this test alone
    /// stays green (the *behaviour* is unchanged — this doesn't measure
    /// timing) — it exists so the mechanism's presence is pinned by a call
    /// site, not so the test itself proves constant time.
    #[test]
    fn tokens_match_by_value() {
        assert!(tokens_match("abcdef0123456789", "abcdef0123456789"));
        assert!(!tokens_match("abcdef0123456789", "abcdef0123456780"));
        assert!(!tokens_match("abcdef0123456789", "abcdef012345678"));
        assert!(!tokens_match("short", "muchlonger"));
        assert!(tokens_match("", ""));
    }

    /// #1162 lens 7 item 3: a failed read must come back as `Err`, never as a
    /// synthesized value. This is the whole fallback branch the fix removes —
    /// pinned directly, not just by way of `random_value`'s panic, since that
    /// panic is the only other place this shape shows up.
    #[test]
    fn read_random_bytes_refuses_rather_than_falls_back_on_a_failed_read() {
        let err =
            read_random_bytes(|_buf| Err(std::io::Error::other("simulated /dev/urandom failure")))
                .expect_err("a failed read must not synthesize bytes");
        assert_eq!(err.to_string(), "simulated /dev/urandom failure");
    }

    /// A successful read is the only `Ok` path, and produces real bytes
    /// (not a fixed/zeroed buffer left untouched by a reader that lied about
    /// succeeding without writing anything meaningful).
    #[test]
    fn read_random_bytes_returns_what_the_reader_wrote() {
        let bytes = read_random_bytes(|buf| {
            buf.copy_from_slice(&[0xAB; 16]);
            Ok(())
        })
        .expect("a succeeding reader must produce Ok");
        assert_eq!(bytes, [0xAB; 16]);
    }

    /// [`random_value`] hardcodes `/dev/urandom` rather than taking an
    /// injected reader, so this cannot drive that function's own panic
    /// directly — but its panic path is textually
    /// `read_random_bytes(..).unwrap_or_else(|e| panic!(...))`, so exercising
    /// that exact combinator against a failing reader pins the same shape:
    /// refuse loudly, naming the device, rather than mint a fallback value.
    ///
    /// Falsification: restoring the old wall-clock+counter fallback branch
    /// makes this panic never fire for a real caller (the read failure would
    /// be swallowed into a still-`Ok`-shaped mint) even though this test
    /// itself keeps passing — which is why item 1's mechanism-falsification
    /// discipline also asks for a manual revert-and-rerun of `random_value`
    /// itself, not just this pinned combinator.
    #[test]
    fn the_panic_combinator_refuses_naming_dev_urandom_on_a_failed_read() {
        let result = std::panic::catch_unwind(|| {
            read_random_bytes(|_buf| Err(std::io::Error::other("boom"))).unwrap_or_else(|e| {
                panic!("hytte-infobroker: refusing to mint a session token: /dev/urandom: {e}")
            })
        });
        let payload = result.expect_err("must panic rather than return a fallback value");
        let msg = payload
            .downcast_ref::<String>()
            .expect("panic payload is a String");
        assert!(msg.contains("/dev/urandom"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

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
