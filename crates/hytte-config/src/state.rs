//! `$XDG_STATE_HOME/trollshell/<subsystem>.toml` — the shell's own scratch
//! space (#866 decision 3, #868).
//!
//! State is what the *shell* writes when you flip a toggle; config is what
//! *you* write. #866 settled that they must not share a directory, and this
//! module is that separation in code: nothing here can resolve a path under
//! `$XDG_CONFIG_HOME`, so a subsystem cannot accidentally drop a toggle file
//! into the directory a person edits by hand. [`crate::xdg`] has a test that
//! the two directories differ.
//!
//! Three plugins already keep state in the right place (`caw`'s
//! `expression.json`, `infobroker`'s `grants.toml`, the claude bridge's
//! session map); the six misplaced ones are all shell-side toggle files still
//! sitting in `~/.config/trollshell/`. Moving them is Phase 2 and deliberately
//! **not** part of #868 — this module is the destination they will move to,
//! and nothing is migrated onto it yet.
//!
//! # Why this is not the format-preserving writer
//!
//! A state file has exactly one author, no comments and nothing to preserve,
//! so it is re-rendered from the value rather than patched — the opposite
//! choice from [`crate::subsystem::save_overlay`], and for the opposite
//! reason. Writes are best-effort and take [`Durability::FileOnly`]: these are
//! click-driven, the in-memory handle is the source of truth for the running
//! process, and losing the last toggle to a power cut leaves the previous
//! state whole. See [`Durability`] for the full argument.

use std::path::{Path, PathBuf};

use crate::file::{self, Durability};
use crate::xdg;

/// `$XDG_STATE_HOME/trollshell/<subsystem>.toml`, or `None` when neither
/// `$XDG_STATE_HOME` nor `$HOME` is set.
#[must_use]
pub fn path(subsystem: &str) -> Option<PathBuf> {
    xdg::state_path(subsystem)
}

/// Read the state file as text; `None` on any failure (missing, unreadable,
/// non-UTF-8) — a caller falls back to its zero state, the same contract as
/// [`crate::file::read`].
#[must_use]
pub fn read(subsystem: &str) -> Option<String> {
    std::fs::read_to_string(path(subsystem)?).ok()
}

/// Deserialize the state file into `T`; `None` if it is missing, unreadable or
/// does not match `T`.
///
/// A state file that no longer parses is the shell's own doing, so it is a
/// `warn!` and a fall back to the zero state rather than an error a caller has
/// to handle: the next write repairs it.
#[must_use]
pub fn load<T: serde::de::DeserializeOwned>(subsystem: &str) -> Option<T> {
    let text = read(subsystem)?;
    match toml::from_str(&text) {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!(subsystem, error = %e, "state file does not parse; ignoring it");
            None
        }
    }
}

/// Serialize `value` and replace the state file atomically.
///
/// Best-effort, like [`crate::file::write`]: any failure logs a `warn!` and
/// returns `false`.
pub fn store<T: serde::Serialize>(subsystem: &str, value: &T) -> bool {
    let Some(path) = path(subsystem) else {
        tracing::warn!(
            subsystem,
            "state write skipped: neither $XDG_STATE_HOME nor $HOME is set"
        );
        return false;
    };
    match store_at(&path, value) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(subsystem, error = %e, path = %path.display(), "state write failed");
            false
        }
    }
}

/// [`store`] against an already-resolved path — the whole of `store` except
/// the environment lookup, split out so tests drive it against a tempdir
/// without mutating the process environment.
///
/// # Errors
/// The serialisation error if `value` cannot be rendered as a TOML table, or
/// the I/O error from the atomic replace.
pub fn store_at<T: serde::Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let body = toml::to_string(value).map_err(std::io::Error::other)?;
    file::write_atomic(path, &body, Durability::FileOnly)
}

/// Delete the state file if it exists, returning a subsystem to its zero
/// state. Best-effort; a missing file is success.
pub fn remove(subsystem: &str) {
    let Some(path) = path(subsystem) else {
        return;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(subsystem, error = %e, path = %path.display(), "state remove failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Toggle {
        enabled: bool,
        apps: Vec<String>,
    }

    #[test]
    fn a_state_value_round_trips_through_the_atomic_writer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/dnd.toml");
        let want = Toggle {
            enabled: true,
            apps: vec!["spotify".into()],
        };

        store_at(&path, &want).expect("writes");

        let text = std::fs::read_to_string(&path).expect("the parent dir was created");
        assert_eq!(toml::from_str::<Toggle>(&text).expect("re-reads"), want);
        assert_eq!(
            std::fs::read_dir(dir.path().join("nested"))
                .expect("dir")
                .count(),
            1,
            "the atomic writer must not leave a temp file behind"
        );
    }

    /// State is re-rendered, not patched: unlike the config overlay there is
    /// no second author whose comments have to survive.
    #[test]
    fn a_state_write_replaces_the_file_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dnd.toml");
        std::fs::write(
            &path,
            "# hand-written\nenabled = true\napps = []\nstray = 1\n",
        )
        .expect("seed");

        store_at(
            &path,
            &Toggle {
                enabled: false,
                apps: Vec::new(),
            },
        )
        .expect("writes");

        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(
            !text.contains("stray"),
            "state is not merged with what was there"
        );
        assert!(!text.contains("hand-written"));
        assert_eq!(text, "enabled = false\napps = []\n");
    }
}
