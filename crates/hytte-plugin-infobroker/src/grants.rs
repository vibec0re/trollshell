//! The durable grant store: `grants.toml` under the XDG state dir.
//!
//! A grant is the policy half of the design (issue #487): it is **durable**
//! (survives a broker/shell restart), keyed `(agent × datasource × scope)`, and
//! carries a [`Decision`]. Tokens — the ephemeral half — live only in memory
//! ([`crate::tokens`]).
//!
//! Phase 1a only ever writes `always`/`deny` decisions (interactive
//! `once`/`session` prompting is deferred to 1b), and `scope` is always
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

    /// Add (or upgrade an existing row to) an `always` grant for
    /// `(agent, datasource)` and persist. Idempotent: a matching row is updated
    /// in place rather than duplicated.
    ///
    /// # Errors
    /// If persisting the updated store fails.
    pub fn grant_always(&mut self, agent: &str, datasource: &str) -> Result<(), String> {
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
        self.save()
    }

    /// Remove the grant for `(agent, datasource)` and persist. Returns whether a
    /// row was actually removed (so the caller only kills tokens on a real
    /// revoke).
    ///
    /// # Errors
    /// If a row was removed but persisting the store fails.
    pub fn revoke(&mut self, agent: &str, datasource: &str) -> Result<bool, String> {
        let before = self.grants.len();
        self.grants
            .retain(|g| !(g.agent == agent && g.datasource == datasource));
        let removed = self.grants.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Persist the current grant set to the backing file (creating the state dir
    /// `0700`). A store with no backing file is a no-op (test / no-`HOME`).
    ///
    /// # Errors
    /// If the state dir or file can't be created/written.
    fn save(&self) -> Result<(), String> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
            tighten_dir(parent);
        }
        let text = to_toml(&self.grants)?;
        std::fs::write(path, text).map_err(|e| format!("writing {}: {e}", path.display()))
    }
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
        store.grant_always("claude", "departures").expect("granted");
        store
            .grant_always("claude", "departures")
            .expect("re-granted");
        assert_eq!(
            store.grants().len(),
            1,
            "a re-grant updates in place, no dup"
        );
        assert_eq!(
            store.decision_for("claude", "departures"),
            Some(Decision::Always)
        );

        assert!(
            store.revoke("claude", "departures").expect("revoked"),
            "row removed"
        );
        assert!(store.decision_for("claude", "departures").is_none());
        assert!(
            !store.revoke("claude", "departures").expect("no-op"),
            "revoking a missing grant reports false"
        );
    }

    #[test]
    fn grant_always_upgrades_a_deny_in_place() {
        let mut store = GrantStore::from_grants(vec![Grant {
            agent: "scratch".to_owned(),
            datasource: "departures".to_owned(),
            scope: SCOPE_ALL.to_owned(),
            decision: Decision::Deny,
        }]);
        store
            .grant_always("scratch", "departures")
            .expect("upgraded");
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
}
