//! The durable grant store: `grants.toml` under the XDG state dir.
//!
//! A grant is the policy half of the design (issue #487): it is **durable**
//! (survives a broker/shell restart), keyed `(agent × datasource × scope)`, and
//! carries a [`Decision`]. Tokens — the ephemeral half — live only in memory
//! ([`crate::tokens`]).
//!
//! Phase 1a only ever writes `always`/`deny` decisions (the interactive
//! `once`/`session` prompting shipped in phase 1b (#514), but those decisions
//! are ephemeral and never reach this durable store), and `scope` is always
//! [`SCOPE_ALL`] — the field exists so a finer scope (a specific station, a
//! read-vs-subscribe split) is additive later without a schema break.
//!
//! ```toml
//! # ~/.local/state/hytte-infobroker/grants.toml
//! [[grant]]
//! agent = "claude"
//! datasource = "departures"
//! scope = "*"
//! decision = "always"
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// The wildcard scope: the whole datasource. The only scope phase 1a mints.
pub const SCOPE_ALL: &str = "*";

/// A grant's decision. Phase 1a persists only these two; the interactive
/// `once`/`session` decisions are a 1b concern and never reach the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Always allow — auth mints silently and the matching `get` is served.
    Always,
    /// Always deny — a standing "no" that blocks even a re-ask.
    Deny,
}

impl Decision {
    /// The wire/CLI string form (`"always"` / `"deny"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Always => "always",
            Decision::Deny => "deny",
        }
    }
}

/// One durable grant row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The agent identity (the `--agent` name the CLI authed with).
    pub agent: String,
    /// The datasource the grant covers (e.g. `"departures"`).
    pub datasource: String,
    /// The scope within the datasource; [`SCOPE_ALL`] in phase 1a.
    #[serde(default = "default_scope")]
    pub scope: String,
    /// Allow or deny.
    pub decision: Decision,
}

fn default_scope() -> String {
    SCOPE_ALL.to_owned()
}

impl Grant {
    /// An `always` grant for `(agent, datasource)` at the wildcard scope.
    #[must_use]
    pub fn always(agent: impl Into<String>, datasource: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            datasource: datasource.into(),
            scope: SCOPE_ALL.to_owned(),
            decision: Decision::Always,
        }
    }
}

/// The TOML envelope: a table of `[[grant]]` arrays.
#[derive(Debug, Default, Serialize, Deserialize)]
struct GrantsFile {
    #[serde(default)]
    grant: Vec<Grant>,
}

/// The in-memory grant set plus the file it persists to. Construct via
/// [`GrantStore::load`] (disk) or [`GrantStore::from_grants`] (tests).
#[derive(Debug)]
pub struct GrantStore {
    path: Option<PathBuf>,
    grants: Vec<Grant>,
}

impl GrantStore {
    /// Load the store from `path`, treating a missing file as an empty store
    /// (the first-run case — no grants yet). A present-but-malformed file is an
    /// error rather than silently dropped, so a typo doesn't quietly grant/deny.
    ///
    /// # Errors
    /// If the file exists but can't be read or parsed as the grant schema.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let grants = match std::fs::read_to_string(&path) {
            Ok(text) => parse_grants(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        Ok(Self {
            path: Some(path),
            grants,
        })
    }

    /// An in-memory store with no backing file — for tests and for the rare
    /// no-`HOME` runtime where [`crate::paths::grants_path`] yields `None`.
    #[must_use]
    pub fn from_grants(grants: Vec<Grant>) -> Self {
        Self { path: None, grants }
    }

    /// All grants, in file order.
    #[must_use]
    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }

    /// The standing decision for `(agent, datasource)`, or `None` if no grant
    /// covers it. Scope-agnostic in phase 1a (only [`SCOPE_ALL`] exists).
    #[must_use]
    pub fn decision_for(&self, agent: &str, datasource: &str) -> Option<Decision> {
        self.grants
            .iter()
            .find(|g| g.agent == agent && g.datasource == datasource)
            .map(|g| g.decision)
    }

    /// Whether `agent` has at least one `always` grant (any datasource) — the
    /// gate `auth` mints a token on.
    #[must_use]
    pub fn has_any_always(&self, agent: &str) -> bool {
        self.grants
            .iter()
            .any(|g| g.agent == agent && g.decision == Decision::Always)
    }

    /// Whether `agent` has at least one `deny` grant (any datasource) — a
    /// standing "no" that `auth` refuses **without** re-prompting (#487 phase
    /// 1b): a deliberate `Deny` decision is remembered, so the agent is denied
    /// silently rather than knocking again.
    #[must_use]
    pub fn has_any_deny(&self, agent: &str) -> bool {
        self.grants
            .iter()
            .any(|g| g.agent == agent && g.decision == Decision::Deny)
    }

    /// Add (or upgrade an existing row to) an `always` grant for
    /// `(agent, datasource)` and persist (off the caller's thread — see
    /// [`save`](GrantStore::save)). Idempotent: a matching row is updated in
    /// place rather than duplicated.
    pub fn grant_always(&mut self, agent: &str, datasource: &str) {
        if let Some(g) = self
            .grants
            .iter_mut()
            .find(|g| g.agent == agent && g.datasource == datasource)
        {
            g.decision = Decision::Always;
            SCOPE_ALL.clone_into(&mut g.scope);
        } else {
            self.grants.push(Grant::always(agent, datasource));
        }
        self.save();
    }

    /// Add (or downgrade an existing row to) a `deny` grant for
    /// `(agent, datasource)` and persist (off the caller's thread — see
    /// [`save`](GrantStore::save)) — the durable half of an `AllowAlways`
    /// decision's opposite: a deliberate `Deny` consent (#487 phase 1b). Idempotent
    /// (a matching row is updated in place), mirroring [`grant_always`](GrantStore::grant_always).
    pub fn grant_deny(&mut self, agent: &str, datasource: &str) {
        if let Some(g) = self
            .grants
            .iter_mut()
            .find(|g| g.agent == agent && g.datasource == datasource)
        {
            g.decision = Decision::Deny;
            SCOPE_ALL.clone_into(&mut g.scope);
        } else {
            self.grants.push(Grant {
                agent: agent.to_owned(),
                datasource: datasource.to_owned(),
                scope: SCOPE_ALL.to_owned(),
                decision: Decision::Deny,
            });
        }
        self.save();
    }

    /// Remove the grant for `(agent, datasource)` and persist (off the
    /// caller's thread — see [`save`](GrantStore::save)). Returns whether a
    /// row was actually removed (so the caller only kills tokens on a real
    /// revoke).
    pub fn revoke(&mut self, agent: &str, datasource: &str) -> bool {
        let before = self.grants.len();
        self.grants
            .retain(|g| !(g.agent == agent && g.datasource == datasource));
        let removed = self.grants.len() != before;
        if removed {
            self.save();
        }
        removed
    }

    /// Persist the current grant set to the backing file, off the runtime
    /// thread and atomically (#1065 — deferred from #1064/#1059).
    ///
    /// Serializes the grant rows right here, under the caller's `&mut self`
    /// borrow — a `to_toml` call, cheap even for a large table, and the only
    /// part of this that touches `self`. The rendered bytes are then handed to
    /// a **detached** [`tokio::task::spawn_blocking`] that does the actual
    /// I/O: `mkdir -p` the state dir (tightened to `0700`), write to a sibling
    /// `.<file>.<pid>.<ticket>.tmp`, then `rename(2)` over the target — the
    /// same write-then-rename shape `hytte_config::file::write_atomic` uses
    /// for `places.toml`/the `~/.config/trollshell/*` writer. This crate
    /// doesn't otherwise depend on `hytte-config` (and doesn't gain that
    /// dependency here), so [`write_atomic`] is the sequence copied rather
    /// than the function imported — `grants.toml` doesn't need that helper's
    /// symlink-following or parent-directory `fsync`, the same durability
    /// bucket as its `Durability::FileOnly`: a click-driven file where the
    /// in-memory `GrantStore` is the session's source of truth and the file is
    /// a convenience for the *next* restart.
    ///
    /// Atomicity closes PR #1064's review finding F2: since #1059 moved the
    /// session-start grant *load* to `spawn_blocking`, it is genuinely
    /// concurrent with a `save` for the first time in one process (they used
    /// to share the single runtime thread, so they were mutually exclusive by
    /// construction). A reader now always sees either the whole old file or
    /// the whole new one — never a truncated write in progress.
    ///
    /// `save` itself never blocks and the `select!` loop that reaches it
    /// through `apply_cmd`/`apply_consent` never awaits the write — it
    /// returns as soon as the write is queued on the blocking pool. The trade
    /// that buys: two `save`s queued close enough together (e.g. an Allow
    /// immediately followed by a Revoke) are not guaranteed to *finish* in
    /// the order they were issued, so the file can transiently hold an
    /// earlier full snapshot after a later one lands. Each write is always
    /// internally consistent (never torn — that's what the tmp+rename buys)
    /// and captures the *complete* `self.grants` at the moment `save` was
    /// called, and the very next grant change queues a fresh complete save —
    /// so the only way this can matter is a crash landing inside that narrow
    /// completion-order window, a strictly smaller exposure than the
    /// whole-process torn-write this change closes.
    ///
    /// A store with no backing file is a no-op (test / no-`HOME`). A failure —
    /// encoding, creating the state dir, the write, or the rename — is logged
    /// from wherever it happens: encoding is still synchronous here, so it
    /// logs and returns without spawning; the rest happens inside the
    /// detached task, since by the time it can fail `save` has already
    /// returned to its caller.
    fn save(&self) {
        self.save_with(write_atomic);
    }

    /// [`save`](GrantStore::save), with the blocking write step supplied by
    /// the caller — the test seam for the property `save` exists to buy:
    /// "a slow synchronous write does not stall a concurrent timer on this
    /// runtime". Production is just `save`, i.e. `self.save_with(write_atomic)`;
    /// `grants::tests::save_offloads_a_slow_writer_without_delaying_a_concurrent_timer`
    /// injects one that blocks for two seconds instead — same shape as
    /// `broker::serve_with_grant_loader`'s injected loader (#1059's test
    /// seam), one level down. Kept `fn(&self)`-private: nothing outside this
    /// module needs it, so unlike `serve_with_grant_loader` it never has to
    /// be `pub`.
    fn save_with<W>(&self, writer: W)
    where
        W: FnOnce(&Path, &str) -> std::io::Result<()> + Send + 'static,
    {
        let Some(path) = self.path.clone() else {
            return;
        };
        let text = match to_toml(&self.grants) {
            Ok(text) => text,
            Err(e) => {
                log(&format!("encoding grants.toml: {e}"));
                return;
            }
        };
        tokio::task::spawn_blocking(move || {
            if let Err(e) = writer(&path, &text) {
                log(&format!("writing {}: {e}", path.display()));
            }
        });
    }
}

/// `[infobroker]`-prefixed eprintln, matching `broker::tracing_eprintln`'s
/// format. Duplicated rather than shared across the module boundary: `grants`
/// is the lower-level, `broker`-independent module (it's the one `broker`
/// imports from), so reaching back up to `crate::broker` for one log line
/// would invert that.
fn log(msg: &str) {
    eprintln!("[infobroker] {msg}");
}

/// Distinguishes the temp files of two [`GrantStore::save`] calls that
/// overlap in time (e.g. an Allow immediately followed by a Revoke, each
/// spawning its own detached write) — mirrors `hytte_config::file`'s
/// `TMP_TICKET`. Without a distinct name per write, two in-flight writers to
/// the same target would share one temp file and interleave into it: two
/// open file descriptors to one inode isn't a "last write wins" race, it's
/// byte-level corruption of the tmp file itself, before `rename` even runs.
static TMP_TICKET: AtomicU64 = AtomicU64::new(0);

/// Atomically replace `path`'s contents with `text`: create its parent
/// directory (tightened to `0700`, same as [`GrantStore::load`]'s caller
/// expects), write to a sibling temp file, then `rename(2)` over the target.
///
/// Copies `hytte_config::file::write_atomic`'s core sequence — a temp file in
/// the target's own directory, so the rename stays on one filesystem and is
/// genuinely atomic — without linking that crate; see
/// [`save`](GrantStore::save)'s doc for why this doesn't need that helper's
/// symlink-following or parent-`fsync` machinery.
///
/// Synchronous by design: every caller runs it from
/// [`tokio::task::spawn_blocking`], never inline on the runtime thread.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::other(format!(
            "{}: grants path has no parent directory",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(dir)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", dir.display())))?;
    tighten_dir(dir);
    let ticket = TMP_TICKET.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("grants.toml");
    let tmp = dir.join(format!(".{name}.{}.{ticket}.tmp", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, text) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Best-effort `0700` on the state dir (same-user-only, like the socket dir).
#[cfg(unix)]
fn tighten_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn tighten_dir(_dir: &Path) {}

/// Parse a `grants.toml` body into grant rows. Pure, so the schema round-trip is
/// unit-testable without disk.
///
/// # Errors
/// If `text` isn't valid TOML for the grant schema.
pub fn parse_grants(text: &str) -> Result<Vec<Grant>, String> {
    let file: GrantsFile = toml::from_str(text).map_err(|e| format!("grants.toml: {e}"))?;
    Ok(file.grant)
}

/// Serialize grant rows back to a `grants.toml` body. Pure.
///
/// # Errors
/// If the rows can't be serialized (not expected for the closed schema).
pub fn to_toml(grants: &[Grant]) -> Result<String, String> {
    let file = GrantsFile {
        grant: grants.to_vec(),
    };
    toml::to_string_pretty(&file).map_err(|e| format!("encoding grants.toml: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
        [[grant]]\n\
        agent = \"claude\"\n\
        datasource = \"departures\"\n\
        scope = \"*\"\n\
        decision = \"always\"\n\
        \n\
        [[grant]]\n\
        agent = \"scratch\"\n\
        datasource = \"departures\"\n\
        decision = \"deny\"\n";

    #[test]
    fn parses_always_and_deny_and_defaults_scope() {
        let grants = parse_grants(SAMPLE).expect("parses");
        assert_eq!(grants.len(), 2);
        assert_eq!(grants[0].agent, "claude");
        assert_eq!(grants[0].decision, Decision::Always);
        assert_eq!(grants[0].scope, "*");
        assert_eq!(grants[1].decision, Decision::Deny);
        // The second row omits `scope` → defaults to the wildcard.
        assert_eq!(grants[1].scope, SCOPE_ALL);
    }

    #[test]
    fn empty_body_is_an_empty_store() {
        assert!(parse_grants("").expect("parses").is_empty());
    }

    #[test]
    fn malformed_body_is_an_error() {
        let err = parse_grants("[[grant]]\ndecision = ").unwrap_err();
        assert!(err.starts_with("grants.toml:"), "got: {err}");
        // An unknown decision value is also rejected loudly.
        assert!(
            parse_grants("[[grant]]\nagent=\"a\"\ndatasource=\"d\"\ndecision=\"maybe\"\n").is_err()
        );
    }

    #[test]
    fn toml_round_trips_through_parse() {
        let grants = parse_grants(SAMPLE).expect("parses");
        let text = to_toml(&grants).expect("encodes");
        let back = parse_grants(&text).expect("re-parses");
        assert_eq!(grants, back);
    }

    #[test]
    fn decision_for_finds_the_matching_pair() {
        let store = GrantStore::from_grants(parse_grants(SAMPLE).unwrap());
        assert_eq!(
            store.decision_for("claude", "departures"),
            Some(Decision::Always)
        );
        assert_eq!(
            store.decision_for("scratch", "departures"),
            Some(Decision::Deny)
        );
        assert_eq!(store.decision_for("nobody", "departures"), None);
        assert_eq!(store.decision_for("claude", "weather"), None);
    }

    #[test]
    fn has_any_always_gates_auth() {
        let store = GrantStore::from_grants(parse_grants(SAMPLE).unwrap());
        assert!(
            store.has_any_always("claude"),
            "an always grant covers auth"
        );
        assert!(
            !store.has_any_always("scratch"),
            "a deny-only agent has no always grant → auth denied"
        );
        assert!(!store.has_any_always("nobody"));
    }

    #[test]
    fn grant_always_is_idempotent_and_revoke_reports_removal() {
        let mut store = GrantStore::from_grants(Vec::new());
        store.grant_always("claude", "departures");
        store.grant_always("claude", "departures");
        assert_eq!(
            store.grants().len(),
            1,
            "a re-grant updates in place, no dup"
        );
        assert_eq!(
            store.decision_for("claude", "departures"),
            Some(Decision::Always)
        );

        assert!(store.revoke("claude", "departures"), "row removed");
        assert!(store.decision_for("claude", "departures").is_none());
        assert!(
            !store.revoke("claude", "departures"),
            "revoking a missing grant reports false"
        );
    }

    #[test]
    fn grant_deny_persists_and_downgrades_an_always_in_place() {
        let mut store = GrantStore::from_grants(Vec::new());
        // A fresh deny is added…
        store.grant_deny("scratch", "departures");
        assert_eq!(store.grants().len(), 1);
        assert_eq!(
            store.decision_for("scratch", "departures"),
            Some(Decision::Deny)
        );
        assert!(store.has_any_deny("scratch"));
        assert!(!store.has_any_always("scratch"));

        // …and an existing `always` is downgraded in place (no dup).
        store.grant_always("claude", "departures");
        store.grant_deny("claude", "departures");
        assert_eq!(store.grants().len(), 2, "downgrade updates in place");
        assert_eq!(
            store.decision_for("claude", "departures"),
            Some(Decision::Deny)
        );
        assert!(!store.has_any_always("claude"));
    }

    #[test]
    fn grant_always_upgrades_a_deny_in_place() {
        let mut store = GrantStore::from_grants(vec![Grant {
            agent: "scratch".to_owned(),
            datasource: "departures".to_owned(),
            scope: SCOPE_ALL.to_owned(),
            decision: Decision::Deny,
        }]);
        store.grant_always("scratch", "departures");
        assert_eq!(store.grants().len(), 1);
        assert_eq!(
            store.decision_for("scratch", "departures"),
            Some(Decision::Always)
        );
    }

    #[test]
    fn load_missing_file_is_empty_store() {
        let path = std::env::temp_dir().join("hytte-infobroker-test-does-not-exist-42.toml");
        let _ = std::fs::remove_file(&path);
        let store = GrantStore::load(&path).expect("missing file → empty");
        assert!(store.grants().is_empty());
    }

    // ── `write_atomic` (#1065) ───────────────────────────────────────────────

    #[test]
    fn write_atomic_creates_parent_and_writes_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("grants.toml");
        write_atomic(&path, "hello\n").expect("writes");
        assert_eq!(std::fs::read_to_string(&path).expect("reads"), "hello\n");
        // No temp litter left behind in the parent it just created.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .expect("reads dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left a tmp file: {leftovers:?}");
    }

    #[test]
    fn write_atomic_replaces_existing_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        write_atomic(&path, "old\n").expect("writes");
        write_atomic(&path, "new\n").expect("overwrites");
        assert_eq!(std::fs::read_to_string(&path).expect("reads"), "new\n");
        // Only the target remains — no orphaned tmp file from either write.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("reads dir")
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("grants.toml")]);
    }

    #[test]
    fn write_atomic_uses_distinct_tmp_names_across_calls() {
        // Two overlapping writers must never share one tmp path (see
        // `TMP_TICKET`'s doc) — same pid, so only the ticket can tell them
        // apart. Exercised indirectly: two `write_atomic` calls on the same
        // target must not error out from a tmp-path collision.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        for i in 0..5 {
            write_atomic(&path, &format!("body {i}\n"))
                .unwrap_or_else(|e| panic!("write {i}: {e}"));
        }
        assert_eq!(std::fs::read_to_string(&path).expect("reads"), "body 4\n");
    }

    /// `save` (reached only through `grant_always`/`grant_deny`/`revoke`) is
    /// fire-and-forget: it queues the write on the blocking pool and returns
    /// before it lands. A `GrantStore` with a real backing path only comes
    /// from `load`, so this seeds an empty file first and then polls for the
    /// mutation to actually reach disk — proof the detached path really does
    /// persist, not just that it doesn't panic.
    #[tokio::test]
    async fn grant_always_persists_to_disk_off_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        std::fs::write(&path, "").expect("seed empty file");

        let mut store = GrantStore::load(&path).expect("loads the empty seed");
        store.grant_always("claude", "departures");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.contains("claude") {
                let grants = parse_grants(&text).expect("valid toml");
                assert_eq!(grants, vec![Grant::always("claude", "departures")]);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "grant_always never reached disk within 2s (got: {text:?})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// The #1065 property, the #1059 way: an injected **slow** write step
    /// must not delay a concurrent 100 ms timer on the same current-thread
    /// runtime. `#[tokio::test]` defaults to exactly that flavor — the same
    /// one `hytte-plugin/src/runtime.rs` runs `serve` on in production.
    ///
    /// Before this restructure `save` ran `write_atomic` inline under
    /// `state`'s mutable borrow; a slow write (a wedged/slow disk, or just a
    /// giant grant table) would have held the only thread the runtime has,
    /// starving every other task on it — the same class #1059 already fixed
    /// for `GrantStore::load`. `save_with` moving the writer to
    /// `spawn_blocking` is what this test pins: reverting that one call
    /// (mutation table, PR body) makes this red at ~2 s instead of ~100 ms.
    #[tokio::test]
    async fn save_offloads_a_slow_writer_without_delaying_a_concurrent_timer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        std::fs::write(&path, "").expect("seed empty file");
        let store = GrantStore::load(&path).expect("loads the empty seed");

        let start = std::time::Instant::now();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            start.elapsed()
        });

        // The injected "writer": two seconds of `std::thread::sleep` standing
        // in for a slow/wedged disk. Real `write_atomic` never sleeps.
        store.save_with(|_path, _text| {
            std::thread::sleep(std::time::Duration::from_secs(2));
            Ok(())
        });

        let elapsed = timer
            .await
            .expect("the concurrent 100ms timer task must not panic");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "the 100ms timer fired at {elapsed:?} — the injected 2s writer stalled this \
             runtime's other tasks, so `save` is not actually off-thread",
        );
    }

    /// The #1065 atomicity property: a concurrent reader of `grants.toml`
    /// must at every instant see either the whole *old* snapshot or the
    /// whole *new* one — never a torn mix, and never a parse failure. This is
    /// PR #1064's review finding F2: since #1059 made the session-start grant
    /// *load* genuinely concurrent with a `save`'s write for the first time,
    /// a non-atomic write could hand a reader a half-written file.
    ///
    /// One writer thread alternates between two very differently-sized,
    /// content-distinguishable snapshots (`small`/`large`) many times over,
    /// while this thread polls the file and — whenever it can read *and*
    /// parse it — asserts the result is exactly one of those two snapshots.
    /// Checking against the *known values*, not just "did it parse", is the
    /// point: an interrupted `std::fs::write`'s `O_TRUNC` can leave a reader
    /// looking at a perfectly valid-TOML *empty* file, which `parse_grants`
    /// accepts (`empty_body_is_an_empty_store`) — a plain "parse succeeded"
    /// check would miss exactly that failure mode.
    #[test]
    fn write_atomic_never_exposes_a_torn_file_to_a_concurrent_reader() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");

        let small_grants = vec![Grant::always("a", "departures")];
        let small = to_toml(&small_grants).expect("encodes");
        let large_grants: Vec<Grant> = (0..4000)
            .map(|i| Grant {
                agent: format!("agent-{i:05}"),
                datasource: "departures".to_owned(),
                scope: SCOPE_ALL.to_owned(),
                decision: Decision::Deny,
            })
            .collect();
        let large = to_toml(&large_grants).expect("encodes");

        // Seed so the reader's very first read is a real (if racy) target,
        // not "file doesn't exist yet".
        write_atomic(&path, &small).expect("seed write");

        const ITERATIONS: usize = 20_000;
        let writer_path = path.clone();
        let (small_w, large_w) = (small.clone(), large.clone());
        let writer = std::thread::spawn(move || {
            for i in 0..ITERATIONS {
                let body = if i % 2 == 0 { &small_w } else { &large_w };
                write_atomic(&writer_path, body).expect("write_atomic");
            }
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut reads = 0usize;
        while !writer.is_finished() && std::time::Instant::now() < deadline {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue; // a transient read error is not tearing; keep polling
            };
            if text.is_empty() {
                // Only reachable if `write_atomic` regresses to a non-atomic
                // truncate-then-write; a real rename(2) never exposes this.
                panic!("read a fully empty grants.toml mid-write — a torn (truncated) write");
            }
            reads += 1;
            match parse_grants(&text) {
                Ok(grants) => assert!(
                    grants == small_grants || grants == large_grants,
                    "read {} grants (first: {:?}) — neither the old nor the new snapshot, \
                     i.e. a torn write",
                    grants.len(),
                    grants.first().map(|g| &g.agent),
                ),
                Err(e) => panic!("read invalid TOML mid-write (a torn write): {e}\n---\n{text}"),
            }
        }
        writer.join().expect("writer thread panicked");
        assert!(
            reads > 50,
            "the reader only raced the writer {reads} times — widen the fixture or \
             iteration count so this test actually contends",
        );
    }
}
