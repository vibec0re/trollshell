//! Wallpaper picker service.
//!
//! Single-image, single-output wallpaper for the trollshell DE. The chosen
//! image path is persisted to `~/.config/trollshell/wallpaper.path` (one
//! line, plain text). On `set_path`, this service rewrites the path file and
//! then runs a *reload command* so the new wallpaper is applied immediately.
//!
//! The reload command is configurable via the `TROLLSHELL_WALLPAPER_RELOAD_CMD`
//! env var, run through `sh -c`. A `{}` in it is replaced with the chosen path
//! (shell-quoted); the path is also exported as `TROLLSHELL_WALLPAPER_PATH`.
//! When it's unset we fall back to restarting the bundled `swaybg` user unit
//! (`etc/systemd/user/swaybg.service`) — the historical default. This lets a
//! `swww`/`awww` (or `hyprpaper`, `wbg`, …) user route the picker at their own
//! daemon instead of swaybg, e.g. `awww img {}`.
//!
//! The bundled swaybg unit's `ExecStart` is a `sh -c` wrapper that reads the
//! path file at startup, so we don't have to copy or symlink images — the
//! user's chosen path is the path swaybg sees.
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

use crate::config_file;
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry, runtime};
use std::process::Stdio;

// ── Persistence ──────────────────────────────────────────────────────────────

/// Config file under `~/.config/trollshell/`.
const CONFIG_FILE: &str = "wallpaper.path";

/// Env var naming a shell command run after the path file is written, to tell
/// the wallpaper daemon to pick up the new image. Run via `sh -c`. Unset/empty
/// falls back to restarting the bundled swaybg user unit, so the default is
/// unchanged. Lets a `swww`/`awww` (or `hyprpaper`, `wbg`, …) user point the
/// picker at their own daemon — e.g. `awww img {}` (see [`PATH_PLACEHOLDER`]).
const RELOAD_CMD_ENV: &str = "TROLLSHELL_WALLPAPER_RELOAD_CMD";

/// Substring of the reload command replaced with the chosen path (shell-quoted)
/// before the command runs. A literal token rather than a `$VAR` reference, so
/// it survives the shell expansion that delivery via NixOS / home-manager
/// `sessionVariables` performs on the value: a `$TROLLSHELL_WALLPAPER_PATH`
/// reference there would be expanded (to empty) at login, long before the
/// picker ever sets the path.
const PATH_PLACEHOLDER: &str = "{}";

/// Env var also exported to the reload command, holding the chosen absolute
/// path. Handy when [`RELOAD_CMD_ENV`] is delivered through a channel that
/// keeps `$` literal (e.g. a systemd `Environment=`); prefer
/// [`PATH_PLACEHOLDER`] when setting the command through the nix module
/// options, where `$`-references get expanded away.
const WALLPAPER_PATH_ENV: &str = "TROLLSHELL_WALLPAPER_PATH";

fn load_path_from_disk() -> Option<String> {
    let text = config_file::read(CONFIG_FILE)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn save_path_to_disk(value: &str) {
    config_file::write("wallpaper", CONFIG_FILE, &format!("{value}\n"));
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

/// Set the wallpaper path. Persists to disk and runs the configured reload
/// command (default: restart the bundled swaybg user unit) so the new image
/// is applied immediately.
///
/// Empty / whitespace-only paths are rejected with a warning — the path
/// file is left untouched and no reload runs.
///
/// Filesystem-level validation (file existence, image format) is not
/// performed here; if the path is bogus, the wallpaper daemon will fail to
/// render and the failure surfaces in its logs.
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

    // File I/O + the reload command off the GTK main thread.
    let value_for_io = value.clone();
    runtime::handle().spawn_blocking(move || {
        save_path_to_disk(&value_for_io);
        reload_wallpaper(&value_for_io);
    });
}

/// Tell the wallpaper daemon to pick up the freshly written path. Runs the
/// command named by [`RELOAD_CMD_ENV`] when set; otherwise restarts the bundled
/// swaybg unit (the historical default).
fn reload_wallpaper(path: &str) {
    match std::env::var(RELOAD_CMD_ENV) {
        Ok(cmd) if !cmd.trim().is_empty() => run_reload_command(&cmd, path),
        _ => restart_swaybg_unit(),
    }
}

/// Run a user-configured reload command via `sh -c`. Any [`PATH_PLACEHOLDER`]
/// in the command is replaced with the chosen path, shell-quoted; the path is
/// also exported as [`WALLPAPER_PATH_ENV`].
fn run_reload_command(cmd: &str, path: &str) {
    let expanded = cmd.replace(PATH_PLACEHOLDER, &shell_single_quote(path));
    let status = std::process::Command::new("sh")
        .args(["-c", expanded.as_str()])
        .env(WALLPAPER_PATH_ENV, path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            tracing::warn!(?s, command = %expanded, "wallpaper: reload command exited non-zero");
        }
        Err(e) => {
            tracing::warn!(error = %e, command = %expanded, "wallpaper: failed to spawn reload command");
        }
    }
}

/// Wrap `s` in single quotes for safe substitution into a `sh -c` string,
/// escaping embedded single quotes the POSIX way (`'\''`).
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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

#[cfg(test)]
mod tests {
    use super::{PATH_PLACEHOLDER, shell_single_quote};

    fn expand(cmd: &str, path: &str) -> String {
        cmd.replace(PATH_PLACEHOLDER, &shell_single_quote(path))
    }

    #[test]
    fn quotes_plain_path() {
        assert_eq!(shell_single_quote("/home/a/wall.png"), "'/home/a/wall.png'");
    }

    #[test]
    fn quotes_path_with_spaces() {
        assert_eq!(
            expand("awww img {}", "/home/a/My Wall.png"),
            "awww img '/home/a/My Wall.png'"
        );
    }

    #[test]
    fn escapes_embedded_single_quote() {
        // POSIX: close quote, escaped quote, reopen — '\'' .
        assert_eq!(shell_single_quote("/a/o'brien.png"), r"'/a/o'\''brien.png'");
    }

    #[test]
    fn command_without_placeholder_is_unchanged() {
        assert_eq!(
            expand("systemctl --user restart swww.service", "/x.png"),
            "systemctl --user restart swww.service"
        );
    }
}
