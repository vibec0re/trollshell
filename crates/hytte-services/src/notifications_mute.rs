//! Per-app notification toast mute set.
//!
//! Pure UI-state service: holds a `HashSet<String>` of `app_name`s whose
//! TOAST popups should be suppressed. The notifications service still records
//! every notification in its history ring; only the consumer-side toast widget
//! gates on this set.
//!
//! Critical-urgency notifications BYPASS the mute set at the toast call site
//! (mirroring the DND policy — `urgency=2` always shows). This module is
//! unaware of that policy; it just publishes the set.
//!
//! # Persistence
//!
//! Persisted to `$XDG_STATE_HOME/trollshell/muted-apps.toml` (#1226) as
//! `apps = ["Discord", "Slack"]`. One-time read-migration from the legacy
//! `~/.config/trollshell/muted-apps.toml`: if state is absent and that file
//! exists, its value is adopted into state and the old file is left
//! untouched; once state exists it is authoritative and the old file is
//! never read again. Writes are best-effort; failure is logged and the
//! in-memory state is the source of truth for the running process.

use crate::config_file;
use futures_signals::signal::{Mutable, Signal};
use hytte_config::state;
use hytte_reactive::{Service, registry, runtime};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

// ── Persistence ──────────────────────────────────────────────────────────────

/// The state subsystem name — `$XDG_STATE_HOME/trollshell/muted-apps.toml`.
const SUBSYSTEM: &str = "muted-apps";

/// Legacy config file under `~/.config/trollshell/`, migrated once (#1226).
const LEGACY_CONFIG_FILE: &str = "muted-apps.toml";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct MutedAppsState {
    #[serde(default)]
    apps: HashSet<String>,
}

fn load_from_disk() -> HashSet<String> {
    let old = config_file::path(LEGACY_CONFIG_FILE);
    let loaded: MutedAppsState = state::load_or_migrate_from(SUBSYSTEM, old.as_deref(), |text| {
        Some(MutedAppsState {
            apps: parse_apps_line(text),
        })
    });
    loaded.apps
}

/// Parse a single `apps = ["X", "Y", ...]` line out of the TOML body.
/// Permissive: ignores comments, anything after the last `]`, and entries with
/// embedded `"` are not supported (we just emit raw strings on save). App
/// names with double quotes are dropped on load.
fn parse_apps_line(text: &str) -> HashSet<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rhs) = trimmed.strip_prefix("apps") {
            let rhs = rhs.trim_start_matches([' ', '=', '\t']).trim();
            // rhs should look like ["A", "B", "C"]
            let Some(inner) = rhs.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
                return HashSet::new();
            };
            return inner
                .split(',')
                .map(str::trim)
                .filter_map(|s| s.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
    }
    HashSet::new()
}

fn save_to_disk(apps: &HashSet<String>) {
    state::store(SUBSYSTEM, &MutedAppsState { apps: apps.clone() });
}

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct NotificationsMuteHandles {
    pub(crate) apps: Mutable<HashSet<String>>,
}

impl Default for NotificationsMuteHandles {
    fn default() -> Self {
        Self {
            apps: Mutable::new(load_from_disk()),
        }
    }
}

/// Marker type for the per-app notification mute service.
pub struct NotificationsMuteService;

impl Service for NotificationsMuteService {
    type Handles = NotificationsMuteHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        NotificationsMuteHandles::default()
    }
}

#[must_use]
pub fn service() -> NotificationsMuteService {
    NotificationsMuteService
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Signal of the muted-apps set. The toast widget filters non-Critical
/// notifications by `app_name ∈ this set` (Critical urgency always shows).
pub fn muted_apps() -> impl Signal<Item = HashSet<String>> {
    registry::with(|r| {
        r.get::<NotificationsMuteHandles>()
            .expect("notifications_mute::service() not registered")
            .apps
            .signal_cloned()
    })
}

/// Add or remove `app_name` from the muted set and persist. Idempotent — a
/// no-op when the desired state already matches.
pub fn set_app_muted(app_name: &str, muted: bool) {
    let snapshot = registry::with(|r| {
        r.get::<NotificationsMuteHandles>().map(|h| {
            let mut apps = h.apps.lock_mut();
            let changed = if muted {
                apps.insert(app_name.to_string())
            } else {
                apps.remove(app_name)
            };
            (changed, apps.clone())
        })
    });
    if let Some((changed, apps)) = snapshot
        && changed
    {
        // File I/O off the GTK main thread.
        runtime::handle().spawn_blocking(move || save_to_disk(&apps));
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_returns_empty() {
        assert!(parse_apps_line("").is_empty());
        assert!(parse_apps_line("# only a comment").is_empty());
    }

    #[test]
    fn parse_single_app() {
        let s = parse_apps_line("apps = [\"Discord\"]\n");
        assert!(s.contains("Discord"));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn parse_multi_app() {
        let s = parse_apps_line("apps = [\"Discord\", \"Slack\", \"Telegram\"]\n");
        assert_eq!(s.len(), 3);
        assert!(s.contains("Discord"));
        assert!(s.contains("Slack"));
        assert!(s.contains("Telegram"));
    }

    #[test]
    fn parse_handles_extra_whitespace() {
        let s = parse_apps_line("apps   =   [ \"A\" ,   \"B\" ]");
        assert_eq!(s.len(), 2);
        assert!(s.contains("A"));
        assert!(s.contains("B"));
    }

    #[test]
    fn parse_ignores_unrelated_lines() {
        let body = "# header\nother = 5\napps = [\"X\"]\n";
        let s = parse_apps_line(body);
        assert_eq!(s.len(), 1);
        assert!(s.contains("X"));
    }

    // ── State migration (#1226) ─────────────────────────────────────────────

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
        home.join(".config/trollshell/muted-apps.toml")
    }

    #[test]
    fn migrates_the_legacy_file_once_and_leaves_it_untouched() {
        with_scratch_home(|home| {
            let legacy = legacy_path(home);
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, "apps = [\"Discord\"]\n").unwrap();
            let before = std::fs::metadata(&legacy).unwrap();

            let apps = load_from_disk();
            assert_eq!(apps, HashSet::from(["Discord".to_string()]));

            let state_path = state::path(SUBSYSTEM).unwrap();
            assert!(
                state_path.exists(),
                "state must now hold the migrated value"
            );

            let after = std::fs::metadata(&legacy).unwrap();
            assert_eq!(
                std::fs::read_to_string(&legacy).unwrap(),
                "apps = [\"Discord\"]\n",
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
            state::store(
                SUBSYSTEM,
                &MutedAppsState {
                    apps: HashSet::from(["Slack".to_string()]),
                },
            );

            let legacy = legacy_path(home);
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, "not even valid toml {{{").unwrap();

            assert_eq!(load_from_disk(), HashSet::from(["Slack".to_string()]));
        });
    }

    #[test]
    fn neither_file_present_defaults_to_empty() {
        with_scratch_home(|_home| {
            assert!(load_from_disk().is_empty());
        });
    }
}
