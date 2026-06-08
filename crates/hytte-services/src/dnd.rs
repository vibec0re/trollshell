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
//! User toggle persisted to `~/.config/trollshell/dnd.toml` as a single-line
//! `enabled = true|false` flag. Default OFF (toasts on). Mirrors the parser
//! shape used by `bluetooth_audio` — flat key=value, fallback off on missing
//! file or malformed contents. Writes are best-effort; failure is logged and
//! the in-memory state is the source of truth for the running process.

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry, runtime};
use std::path::PathBuf;

// ── Persistence ──────────────────────────────────────────────────────────────

const CONFIG_REL_PATH: &str = ".config/trollshell/dnd.toml";

fn config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(CONFIG_REL_PATH))
}

fn load_enabled_from_disk() -> bool {
    let Some(path) = config_path() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    // Permissive: look for `enabled = true` anywhere; otherwise default OFF.
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rhs) = trimmed.strip_prefix("enabled") {
            let rhs = rhs.trim_start_matches([' ', '=', '\t']).trim();
            if rhs.eq_ignore_ascii_case("true") {
                return true;
            }
            if rhs.eq_ignore_ascii_case("false") {
                return false;
            }
        }
    }
    false
}

fn save_enabled_to_disk(enabled: bool) {
    let Some(path) = config_path() else {
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(error = %e, path = %parent.display(), "dnd: mkdir failed");
        return;
    }
    let body = format!("enabled = {enabled}\n");
    if let Err(e) = std::fs::write(&path, body) {
        tracing::warn!(error = %e, path = %path.display(), "dnd: write failed");
    }
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
    let prev = registry::with(|r| {
        r.get::<DndHandles>().map(|h| {
            let cur = h.enabled.get();
            if cur != on {
                h.enabled.set(on);
            }
            cur
        })
    });
    if prev != Some(on) {
        // File I/O off the GTK main thread.
        runtime::handle().spawn_blocking(move || save_enabled_to_disk(on));
    }
}
