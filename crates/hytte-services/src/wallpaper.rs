//! Wallpaper picker service.
//!
//! Single-image, single-output wallpaper for the trollshell DE. The chosen
//! image path is persisted to `~/.config/trollshell/wallpaper.path` (one
//! line, plain text) and rendered by `swaybg` running under a systemd user
//! unit (`etc/systemd/user/swaybg.service`). On `set_path`, this service
//! rewrites the path file and triggers
//! `systemctl --user restart swaybg.service` so the new wallpaper is
//! applied immediately.
//!
//! The systemd unit's `ExecStart` is a `sh -c` wrapper that reads the path
//! file at swaybg startup, so we don't have to copy or symlink images —
//! the user's chosen path is the path swaybg sees.
//!
//! # Persistence
//!
//! - File: `$HOME/.config/trollshell/wallpaper.path`
//! - Format: a single line containing the absolute path of the wallpaper
//!   image. Trailing whitespace/newlines tolerated on read; one trailing
//!   newline written on save.
//! - Missing file or empty file ⇒ `current_path()` emits `None`.
//!
//! # Validation
//!
//! `set_path` rejects empty/whitespace-only paths with a `tracing::warn!`
//! (no-op, no file write, no unit restart). It does NOT verify that the
//! file exists or is readable — if the user picked a missing path, swaybg
//! will fail at start and surface via `systemctl --user status swaybg`.
//!
//! # Scope (v1)
//!
//! Single image, single monitor. Multi-output and per-output wallpaper
//! (with kanshi-style profile awareness) is a follow-up. There is no
//! "clear" / "unset" path — the user re-picks via the drawer.

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry, runtime};
use std::path::PathBuf;
use std::process::Stdio;

// ── Persistence ──────────────────────────────────────────────────────────────

const CONFIG_REL_PATH: &str = ".config/trollshell/wallpaper.path";

fn config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(CONFIG_REL_PATH))
}

fn load_path_from_disk() -> Option<String> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn save_path_to_disk(value: &str) {
    let Some(path) = config_path() else {
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(error = %e, path = %parent.display(), "wallpaper: mkdir failed");
        return;
    }
    let body = format!("{value}\n");
    if let Err(e) = std::fs::write(&path, body) {
        tracing::warn!(error = %e, path = %path.display(), "wallpaper: write failed");
    }
}

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct WallpaperHandles {
    pub(crate) current: Mutable<Option<String>>,
}

impl Default for WallpaperHandles {
    fn default() -> Self {
        Self {
            current: Mutable::new(load_path_from_disk()),
        }
    }
}

/// Wallpaper service marker. Pass to `App::with` to register the service.
/// No background task is needed — `current_path()` is read on init from
/// disk, then updated synchronously from `set_path()`.
pub struct WallpaperService;

impl Service for WallpaperService {
    type Handles = WallpaperHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        WallpaperHandles::default()
    }
}

#[must_use]
pub fn service() -> WallpaperService {
    WallpaperService
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Signal of the current wallpaper path. Emits `None` when the persistence
/// file is missing or empty.
pub fn current_path() -> impl Signal<Item = Option<String>> {
    registry::with(|r| {
        r.get::<WallpaperHandles>()
            .expect("wallpaper::service() not registered")
            .current
            .signal_cloned()
    })
}

/// Set the wallpaper path. Persists to disk and restarts the swaybg
/// systemd user unit so the new image is applied immediately.
///
/// Empty / whitespace-only paths are rejected with a warning — the path
/// file is left untouched and the unit is not restarted.
///
/// Filesystem-level validation (file existence, image format) is not
/// performed here; if the path is bogus, swaybg will fail to render and
/// the failure surfaces via `systemctl --user status swaybg.service`.
pub fn set_path(path: &str) {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        tracing::warn!("wallpaper: refusing empty path");
        return;
    }

    let value = trimmed.to_string();

    // Update the in-memory signal synchronously so the UI reflects the
    // new selection immediately. The `prev != next` guard keeps repeated
    // picks of the same path from spawning needless restart commands.
    let prev = registry::with(|r| {
        r.get::<WallpaperHandles>().map(|h| {
            let cur = h.current.get_cloned();
            if cur.as_deref() != Some(value.as_str()) {
                h.current.set(Some(value.clone()));
            }
            cur
        })
    });

    // `prev` is `Option<Option<String>>`: outer = service registered?,
    // inner = previously-stored path. Flatten and compare.
    if prev.flatten().as_deref() == Some(value.as_str()) {
        // No-op: same path as before; don't churn the unit.
        return;
    }

    // File I/O + systemctl off the GTK main thread.
    let value_for_io = value.clone();
    runtime::handle().spawn_blocking(move || {
        save_path_to_disk(&value_for_io);
        restart_swaybg_unit();
    });
}

fn restart_swaybg_unit() {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "restart", "swaybg.service"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(
            ?s,
            "wallpaper: systemctl --user restart swaybg.service exited non-zero"
        ),
        Err(e) => tracing::warn!(error = %e, "wallpaper: failed to spawn systemctl"),
    }
}
