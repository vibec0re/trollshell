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

use crate::config_file;
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry, runtime};

// ── Persistence ──────────────────────────────────────────────────────────────

/// Config file under `~/.config/trollshell/`.
const CONFIG_FILE: &str = "dnd.toml";

fn load_enabled_from_disk() -> bool {
    let Some(text) = config_file::read(CONFIG_FILE) else {
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
    config_file::write("dnd", CONFIG_FILE, &format!("enabled = {enabled}\n"));
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
