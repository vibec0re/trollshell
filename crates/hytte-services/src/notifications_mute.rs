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
use std::collections::{BTreeSet, HashSet};

// ── Persistence ──────────────────────────────────────────────────────────────

/// The state subsystem name — `$XDG_STATE_HOME/trollshell/muted-apps.toml`.
const SUBSYSTEM: &str = "muted-apps";

/// Legacy config file under `~/.config/trollshell/`, migrated once (#1226).
const LEGACY_CONFIG_FILE: &str = "muted-apps.toml";

/// `#[serde(default)]` on the **container** (#1233 F4) — see `dnd::DndState`
/// for why the rule is uniform rather than applied only where the two forms
/// differ.
///
/// `apps` is a `BTreeSet`, not the `HashSet` the in-memory handle carries, and
/// that is the whole of what keeps the rendered file sorted: serde walks the
/// set in iteration order, and a `HashSet`'s is `RandomState`-seeded per
/// instance — the review measured **172 distinct renders of one six-element
/// logical set across 200 builds** (#1233 F5). The pre-#1226 hand-rolled
/// writer sorted explicitly; losing that made the file churn byte-wise on
/// every unrelated toggle, stop being diffable or greppable by a human, and
/// pre-broke any future dedup-on-content. [`state_for`] is the one conversion
/// seam, so the test can render through the same path `save_to_disk` does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct MutedAppsState {
    apps: BTreeSet<String>,
}

/// The on-disk shape of `apps` — the one place the unordered in-memory set
/// becomes the ordered on-disk one. See [`MutedAppsState`].
fn state_for(apps: &HashSet<String>) -> MutedAppsState {
    MutedAppsState {
        apps: apps.iter().cloned().collect(),
    }
}

fn load_from_disk() -> HashSet<String> {
    let old = config_file::path(LEGACY_CONFIG_FILE);
    let loaded: MutedAppsState = state::load_or_migrate_from(SUBSYSTEM, old.as_deref(), |text| {
        Some(state_for(&parse_apps_line(text)))
    });
    loaded.apps.into_iter().collect()
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
    state::store(SUBSYSTEM, &state_for(apps));
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
    use hytte_config::test_support::scratch_home;

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

    fn legacy_path(home: &std::path::Path) -> std::path::PathBuf {
        home.join(".config/trollshell/muted-apps.toml")
    }

    #[test]
    fn migrates_the_legacy_file_once_and_leaves_it_untouched() {
        scratch_home(|home| {
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
        scratch_home(|home| {
            state::store(
                SUBSYSTEM,
                &state_for(&HashSet::from(["Slack".to_string()])),
            );

            let legacy = legacy_path(home);
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, "not even valid toml {{{").unwrap();

            assert_eq!(load_from_disk(), HashSet::from(["Slack".to_string()]));
        });
    }

    /// State wins **even when it does not parse** — the half of #1226's
    /// contract with a user-visible cost, and the one the `state_wins_*` test
    /// above cannot reach because it seeds a valid state file (#1233 F1).
    #[test]
    fn a_corrupt_state_file_still_wins_over_the_legacy_file() {
        scratch_home(|home| {
            let state_path = state::path(SUBSYSTEM).unwrap();
            std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
            std::fs::write(&state_path, "not valid toml {{{").unwrap();

            let legacy = legacy_path(home);
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, "apps = [\"Discord\"]\n").unwrap();

            assert!(
                load_from_disk().is_empty(),
                "a corrupt state file falls to the documented default (no muted apps), \
                 never back to the legacy file's list"
            );
            assert_eq!(
                std::fs::read_to_string(&state_path).unwrap(),
                "not valid toml {{{",
                "the read path must not rewrite the corrupt state file"
            );
        });
    }

    /// The rendered file is sorted, and two renders of the same logical set
    /// are byte-identical (#1233 F5).
    ///
    /// Six independently-seeded `HashSet`s — `RandomState` is per-instance —
    /// all render to the same sorted line. Under a `HashSet` serialized field
    /// the chance of all six landing in sorted order is (1/720)^6, so this
    /// falsifies the `BTreeSet` rather than merely coexisting with it.
    /// Rendered through `state::store_at` into a tempdir because this crate
    /// deliberately has no `toml` dependency of its own (see its Cargo.toml).
    #[test]
    fn the_rendered_file_is_sorted_and_independent_of_insertion_order() {
        const APPS: [&str; 6] = ["Zed", "Discord", "Element", "Firefox", "Slack", "Thunderbird"];
        const WANT: &str =
            "apps = [\"Discord\", \"Element\", \"Firefox\", \"Slack\", \"Thunderbird\", \"Zed\"]\n";

        let dir = tempfile::tempdir().expect("tempdir");
        for rotation in 0..APPS.len() {
            let apps: HashSet<String> = APPS
                .iter()
                .cycle()
                .skip(rotation)
                .take(APPS.len())
                .map(|s| (*s).to_string())
                .collect();
            let path = dir.path().join(format!("muted-apps-{rotation}.toml"));
            state::store_at(&path, &state_for(&apps)).expect("renders");
            assert_eq!(
                std::fs::read_to_string(&path).expect("read back"),
                WANT,
                "insertion order {rotation} must render the same sorted bytes"
            );
        }
    }

    /// The bug the new writer fixed, kept pinned: the pre-#1226 hand-rolled
    /// writer silently `filter`ed out any app name containing `\"`.
    #[test]
    fn an_app_name_with_a_quote_round_trips() {
        scratch_home(|_home| {
            let apps = HashSet::from(["He said \"hi\"".to_string()]);
            save_to_disk(&apps);
            assert_eq!(load_from_disk(), apps);
        });
    }

    #[test]
    fn neither_file_present_defaults_to_empty() {
        scratch_home(|_home| {
            assert!(load_from_disk().is_empty());
        });
    }
}
