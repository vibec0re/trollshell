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
//! session map); #1226 moved the seven shell-side toggle files here too —
//! `crates/hytte-services/src/{dnd,notifications_mute,bluetooth_audio,
//! fullscreen_inhibit,screensaver,wallpaper}.rs` each read/write their state
//! through [`load_or_migrate_from`]/[`store`] now (`dnd.toml`,
//! `muted-apps.toml`, `bluetooth-audio.toml`, `fullscreen-inhibit.toml`,
//! `keep-awake.toml`, `wallpaper.toml`). `nightlight.rs`'s `wlsunset.args` is
//! the one file #1226 left behind: `nix/hm-module.nix`'s `wlsunset.service`
//! `ExecStart` hardcodes `%h/.config/trollshell/wlsunset.args`, so moving it
//! would break that unit for anyone who hasn't (and, being a nix-rendered
//! unit, can't on their own) picked up a state-aware version — the same
//! reason `wallpaper.rs`'s `swaybg.args` and `wallpaper.path` stay in the
//! config dir while only its structured `wallpaper.json` moved (now
//! `wallpaper.toml`, since a state file is always TOML — see [`store`]).
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

/// Load `subsystem`'s state, migrating a legacy `~/.config/trollshell/*` file
/// the first time the state file doesn't exist yet (#1226).
///
/// **State wins once it exists.** If the state file is already there, `old`
/// is never consulted, even if the state file fails to parse — exactly
/// [`load`]'s own warn-and-fall-back-to-`T::default()` behaviour, unchanged
/// by having a migration source available. That is the second half of
/// #1226's contract: once migrated, a hand-edited or corrupted legacy file
/// has no effect, ever again.
///
/// **Migrate once, non-destructively.** If the state file is absent, `old`
/// names a path, that path is readable, and `parse_old` accepts its
/// contents, the parsed value is written to state — best-effort, like
/// [`store`] — and returned. `old` itself is left completely alone: never
/// deleted, never renamed. A file a person's own daemon unit might still be
/// reading (or that they just haven't looked at in months) is not this
/// shell's to remove. Logs once at `info`, naming both paths.
///
/// **Otherwise, the zero state.** No state file and either no `old` path, no
/// file there, or a `parse_old` that returns `None` all fall back to
/// `T::default()` — the same outcome a bare [`load`] gives a caller with
/// nothing to migrate.
pub fn load_or_migrate_from<T, F>(subsystem: &str, old: Option<&Path>, parse_old: F) -> T
where
    T: serde::de::DeserializeOwned + serde::Serialize + Default,
    F: FnOnce(&str) -> Option<T>,
{
    if path(subsystem).is_some_and(|p| p.exists()) {
        return load(subsystem).unwrap_or_default();
    }
    let Some(old) = old else {
        return T::default();
    };
    let Some(text) = std::fs::read_to_string(old).ok() else {
        return T::default();
    };
    let Some(value) = parse_old(&text) else {
        return T::default();
    };
    tracing::info!(
        subsystem,
        old = %old.display(),
        "migrating a shell-written toggle file from the config directory to state (#1226)"
    );
    store(subsystem, &value);
    value
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

    // ── `load_or_migrate_from` (#1226) ──────────────────────────────────────

    #[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Flag {
        #[serde(default)]
        enabled: bool,
    }

    /// Clears `$XDG_STATE_HOME`/`$XDG_CONFIG_HOME` and points `$HOME` at a
    /// tempdir before running `body`, so `path()`'s process-environment read
    /// can never resolve into a real, ambient state or config directory —
    /// the #1101 rule this whole feature is built to respect.
    fn with_scratch_home<R>(body: impl FnOnce(&std::path::Path) -> R) -> R {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().to_path_buf();
        temp_env::with_vars(
            [
                ("HOME", Some(home.to_str().expect("utf8 tempdir"))),
                ("XDG_STATE_HOME", None::<&str>),
                ("XDG_CONFIG_HOME", None::<&str>),
            ],
            || body(&home),
        )
    }

    #[test]
    fn load_or_migrate_from_migrates_once_and_leaves_the_old_file_alone() {
        with_scratch_home(|home| {
            let old = home.join("old.toml");
            std::fs::write(&old, "enabled = true\n").expect("seed old");
            let before = std::fs::metadata(&old).expect("meta");

            let value: Flag = load_or_migrate_from("migrate-a", Some(&old), |text| {
                Some(Flag {
                    enabled: text.contains("true"),
                })
            });
            assert_eq!(value, Flag { enabled: true });

            let state_path = path("migrate-a").expect("state path resolves");
            assert!(state_path.exists(), "the migrated value must land in state");
            assert_eq!(
                load::<Flag>("migrate-a"),
                Some(Flag { enabled: true }),
                "the written state must read back as the migrated value"
            );

            let after = std::fs::metadata(&old).expect("meta");
            assert_eq!(
                std::fs::read_to_string(&old).expect("old survives"),
                "enabled = true\n",
                "the old file's bytes must be untouched"
            );
            assert_eq!(
                before.modified().expect("mtime"),
                after.modified().expect("mtime"),
                "the old file's mtime must be untouched — it is never written"
            );
        });
    }

    #[test]
    fn load_or_migrate_from_prefers_state_and_never_reads_old_again() {
        with_scratch_home(|_home| {
            let state_path = path("migrate-b").expect("state path resolves");
            store_at(&state_path, &Flag { enabled: false }).expect("seed state");

            let old = state_path.with_file_name("old-b.toml");
            // Deliberately unparseable — if `parse_old` is ever called, the
            // closure panics, which is the falsification for "old is never
            // read again once state exists".
            std::fs::write(&old, "not valid toml {{{").expect("seed unparseable old");

            let value: Flag = load_or_migrate_from("migrate-b", Some(&old), |_text| {
                panic!("old must not be read once a state file exists")
            });
            assert_eq!(
                value,
                Flag { enabled: false },
                "state's value must win over the (unreadable) old file"
            );
        });
    }

    #[test]
    fn load_or_migrate_from_defaults_when_neither_file_exists() {
        with_scratch_home(|home| {
            let old = home.join("never-existed.toml");
            let value: Flag = load_or_migrate_from("migrate-c", Some(&old), |_text| {
                panic!("old does not exist; parse_old must not run")
            });
            assert_eq!(value, Flag::default());
            assert!(
                path("migrate-c").is_some_and(|p| !p.exists()),
                "no state file should be created when there was nothing to migrate"
            );
        });
    }
}
