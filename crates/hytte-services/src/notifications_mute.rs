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
//! Persisted to `~/.config/trollshell/muted-apps.toml` as a single-line
//! `apps = ["Discord", "Slack"]` array. Mirrors the parser shape used by
//! `dnd` / `bluetooth_audio` — a flat key=value parser, fallback empty on
//! missing file or malformed contents. Writes are best-effort; failure is
//! logged and the in-memory state is the source of truth for the running
//! process.

use crate::config_file;
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry, runtime};
use std::collections::HashSet;

// ── Persistence ──────────────────────────────────────────────────────────────

/// Config file under `~/.config/trollshell/`.
const CONFIG_FILE: &str = "muted-apps.toml";

fn load_from_disk() -> HashSet<String> {
    config_file::read(CONFIG_FILE)
        .map(|text| parse_apps_line(&text))
        .unwrap_or_default()
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
    let mut sorted: Vec<&String> = apps.iter().collect();
    sorted.sort();
    let parts: Vec<String> = sorted
        .iter()
        .filter(|s| !s.contains('"'))
        .map(|s| format!("\"{s}\""))
        .collect();
    let body = format!("apps = [{}]\n", parts.join(", "));
    config_file::write("notifications_mute", CONFIG_FILE, &body);
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
}
