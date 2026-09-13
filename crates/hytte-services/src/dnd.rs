//! Do-Not-Disturb toggle for notification toasts.
//!
//! Pure UI-state service: holds a single `enabled: bool` exposed as a signal.
//! The notifications service still records every notification in its history
//! ring; only the consumer-side toast widget gates on this flag.
//!
//! Critical-urgency notifications BYPASS DND at the toast call site (per
//! freedesktop spec — `urgency=2` always shows). This module is unaware of
//! that policy; it just publishes the bool.
//!
//! # Persistence
//!
//! User toggle persisted to `$XDG_STATE_HOME/trollshell/dnd.toml` (#1226) as
//! `enabled = true|false`. Default OFF (toasts on). One-time read-migration
//! from the legacy `~/.config/trollshell/dnd.toml`: if state is absent and
//! that file exists, its value is adopted into state and the old file is
//! left untouched; once state exists it is authoritative and the old file is
//! never read again. Writes are best-effort; failure is logged and the
//! in-memory state is the source of truth for the running process.

use crate::config_file;
use futures_signals::signal::{Mutable, Signal};
use hytte_config::state;
use hytte_reactive::{Service, registry, runtime};
use serde::{Deserialize, Serialize};

// ── Persistence ──────────────────────────────────────────────────────────────

/// The state subsystem name — `$XDG_STATE_HOME/trollshell/dnd.toml`.
const SUBSYSTEM: &str = "dnd";

/// Legacy config file under `~/.config/trollshell/`, migrated once (#1226).
const LEGACY_CONFIG_FILE: &str = "dnd.toml";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct DndState {
    #[serde(default)]
    enabled: bool,
}

/// Permissive legacy parser: looks for `enabled = true` anywhere in the old
/// config file's text; anything else (missing key, malformed value, garbage)
/// keeps the historical default OFF. Only used for the one-time migration —
/// always succeeds, so [`load_enabled_from_disk`] wraps it in `Some` for
/// [`state::load_or_migrate_from`]'s `parse_old`.
fn parse_legacy(text: &str) -> DndState {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rhs) = trimmed.strip_prefix("enabled") {
            let rhs = rhs.trim_start_matches([' ', '=', '\t']).trim();
            if rhs.eq_ignore_ascii_case("true") {
                return DndState { enabled: true };
            }
            if rhs.eq_ignore_ascii_case("false") {
                return DndState { enabled: false };
            }
        }
    }
    DndState::default()
}

fn load_enabled_from_disk() -> bool {
    let old = config_file::path(LEGACY_CONFIG_FILE);
    let loaded: DndState =
        state::load_or_migrate_from(SUBSYSTEM, old.as_deref(), |text| Some(parse_legacy(text)));
    loaded.enabled
}

fn save_enabled_to_disk(enabled: bool) {
    state::store(SUBSYSTEM, &DndState { enabled });
}

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct DndHandles {
    pub(crate) enabled: Mutable<bool>,
}

impl Default for DndHandles {
    fn default() -> Self {
        Self {
            enabled: Mutable::new(load_enabled_from_disk()),
        }
    }
}

/// Marker type for the Do-Not-Disturb service.
pub struct DndService;

impl Service for DndService {
    type Handles = DndHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        DndHandles::default()
    }
}

#[must_use]
pub fn service() -> DndService {
    DndService
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Signal of the Do-Not-Disturb flag. `true` means suppress toast popups
/// (critical-urgency bypass is enforced at the toast call site, not here).
pub fn enabled() -> impl Signal<Item = bool> {
    registry::with(|r| {
        r.get::<DndHandles>()
            .expect("dnd::service() not registered")
            .enabled
            .signal_cloned()
    })
}

/// Update the DND flag and persist it to disk. Idempotent — no-op when the
/// value already matches.
pub fn set_enabled(on: bool) {
    // `Some(true)` only when the service is registered AND the value actually
    // flipped. `None` (service unregistered) must NOT persist — the old
    // `prev != Some(on)` guard wrote the file even then, since `None != Some(_)`
    // (mirrors `notifications_mute`'s correct guard).
    let changed = registry::with(|r| {
        r.get::<DndHandles>().map(|h| {
            if h.enabled.get() == on {
                false
            } else {
                h.enabled.set(on);
                true
            }
        })
    });
    if changed == Some(true) {
        // File I/O off the GTK main thread.
        runtime::handle().spawn_blocking(move || save_enabled_to_disk(on));
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Points `$HOME` at a fresh tempdir and clears `$XDG_STATE_HOME` /
    /// `$XDG_CONFIG_HOME`, so `load_enabled_from_disk`'s process-environment
    /// reads can never resolve into a real config or state directory (#1101).
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

    fn legacy_path(home: &std::path::Path) -> std::path::PathBuf {
        home.join(".config/trollshell/dnd.toml")
    }

    #[test]
    fn migrates_the_legacy_file_once_and_leaves_it_untouched() {
        with_scratch_home(|home| {
            let legacy = legacy_path(home);
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, "enabled = true\n").unwrap();
            let before = std::fs::metadata(&legacy).unwrap();

            assert!(
                load_enabled_from_disk(),
                "the legacy value must be adopted into state"
            );

            let state_path = state::path(SUBSYSTEM).unwrap();
            assert!(
                state_path.exists(),
                "state must now hold the migrated value"
            );

            let after = std::fs::metadata(&legacy).unwrap();
            assert_eq!(
                std::fs::read_to_string(&legacy).unwrap(),
                "enabled = true\n",
                "the legacy file's bytes must survive the migration untouched"
            );
            assert_eq!(
                before.modified().unwrap(),
                after.modified().unwrap(),
                "the legacy file's mtime must survive the migration untouched"
            );
        });
    }

    #[test]
    fn state_wins_once_it_exists_and_the_legacy_file_is_never_read_again() {
        with_scratch_home(|home| {
            // Seed state directly with a value that disagrees with the
            // legacy file, so a read of the wrong source is observable.
            state::store(SUBSYSTEM, &DndState { enabled: true });

            let legacy = legacy_path(home);
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, "not even valid toml {{{").unwrap();

            assert!(
                load_enabled_from_disk(),
                "state's value must win over an unparseable legacy file"
            );
        });
    }

    #[test]
    fn neither_file_present_defaults_to_off() {
        with_scratch_home(|_home| {
            assert!(!load_enabled_from_disk());
        });
    }
}
