//! Wallpaper picker service.
//!
//! Desktop background for the trollshell DE, rendered by `swaybg` under a
//! systemd user unit. Three dimensions on top of the original single-image v1
//! (#546):
//!
//! - **Per-output** — a *default* image applied to every output, plus optional
//!   per-connector overrides (`output name → image path`). swaybg takes the
//!   default as a leading `-i … -m fill` and each override as a later
//!   `-o <name> -i … -m fill`.
//! - **Time-of-day rotation** — a whole-screen mode that switches the wallpaper
//!   on a fixed morning/day/evening/night schedule (see [`Slot`]). Driven by a
//!   cheap 60 s main-loop tick; when the active slot's image changes the render
//!   is re-applied. While rotation is on it wins over the static per-output
//!   selection (it's a whole-screen mode).
//! - **Clear** — the render collapses to *no wallpaper* when nothing is set,
//!   which stops the swaybg unit rather than restarting it on an empty arg list.
//!
//! # Rendering
//!
//! The structured state ([`WallpaperState`]) is the source of truth, persisted
//! to `~/.config/trollshell/wallpaper.json`. From it the service derives the
//! swaybg argument vector and writes it, one arg per line, to
//! `~/.config/trollshell/swaybg.args`; the bundled swaybg unit's `ExecStart`
//! reads that file (see `etc/systemd/user/swaybg.service`). A representative
//! single image (the "primary") is also written to the legacy
//! `~/.config/trollshell/wallpaper.path` — for any not-yet-redeployed old unit
//! and for the custom-reload-command path — and handed to a configured reload
//! command.
//!
//! The reload command is configurable via the `TROLLSHELL_WALLPAPER_RELOAD_CMD`
//! env var, run through `sh -c`. A `{}` in it is replaced with the primary path
//! (shell-quoted); the path is also exported as `TROLLSHELL_WALLPAPER_PATH`.
//! When it's unset we fall back to restarting (or stopping, on clear) the
//! bundled `swaybg` user unit — the historical default. A custom reload command
//! is single-image only: per-output can't be expressed through the `{}`
//! placeholder, so it receives the primary image.
//!
//! # Persistence & backward compat
//!
//! - `wallpaper.json` — structured [`WallpaperState`], the source of truth.
//! - `swaybg.args` — derived render spec (one swaybg arg per line); the unit's
//!   `ExecStart` reads it. Absent ⇒ no wallpaper (the unit stays inactive).
//! - `wallpaper.path` — legacy single-line path, kept written for
//!   graceful-degradation of an old unit and read once on first launch to
//!   **migrate** a pre-#546 install (its single path becomes the new default).
//!
//! On init `wallpaper.json` is preferred; if it's absent (or unparseable) and a
//! legacy `wallpaper.path` exists, that single path is adopted as the default
//! image for all outputs.
//!
//! # Validation
//!
//! `set_*` reject empty/whitespace-only paths with a `tracing::warn!` (no-op).
//! They do NOT verify that the file exists or is a valid image — a bogus path
//! makes swaybg fail at start, surfaced via `systemctl --user status swaybg`.

use crate::config_file;
use chrono::{Local, Timelike};
use futures_signals::signal::{Mutable, Signal, SignalExt};
use gtk::glib;
use hytte_reactive::{Service, registry, runtime};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

// ── Constants ────────────────────────────────────────────────────────────────

/// Structured state file under `~/.config/trollshell/` — the source of truth.
const STATE_FILE: &str = "wallpaper.json";

/// Legacy single-line path file. Kept written (primary image) for old units and
/// read once on init to migrate a pre-#546 install.
const LEGACY_PATH_FILE: &str = "wallpaper.path";

/// Derived swaybg argument file (one arg per line) read by the swaybg unit's
/// `ExecStart`. Absent ⇒ no wallpaper.
const ARGS_FILE: &str = "swaybg.args";

/// The bundled swaybg user unit restarted/stopped to apply a change.
const SWAYBG_UNIT: &str = "swaybg.service";

/// How often the time-of-day rotation is re-evaluated. A minute is plenty for
/// hour-granular slots and the render dedups so an unchanged slot costs nothing.
// `Duration::from_mins` would read cleaner but was stabilized after the crate's
// MSRV (1.91), so `from_secs` avoids a `clippy::incompatible_msrv` bump.
#[allow(clippy::duration_suboptimal_units)]
const ROTATION_TICK: Duration = Duration::from_secs(60);

/// Env var naming a shell command run after the render files are written, to
/// tell the wallpaper daemon to pick up the new image. Run via `sh -c`.
/// Unset/empty falls back to restarting the bundled swaybg unit. Single-image
/// only — per-output isn't expressible through the placeholder.
const RELOAD_CMD_ENV: &str = "TROLLSHELL_WALLPAPER_RELOAD_CMD";

/// Substring of the reload command replaced with the primary path (shell-quoted)
/// before the command runs. A literal token rather than a `$VAR` reference, so
/// it survives the shell expansion that delivery via NixOS / home-manager
/// `sessionVariables` performs on the value.
const PATH_PLACEHOLDER: &str = "{}";

/// Env var also exported to the reload command, holding the primary absolute
/// path. Handy when [`RELOAD_CMD_ENV`] is delivered through a channel that keeps
/// `$` literal (e.g. a systemd `Environment=`).
const WALLPAPER_PATH_ENV: &str = "TROLLSHELL_WALLPAPER_PATH";

// ── Public data types ────────────────────────────────────────────────────────

/// A time-of-day slot. Boundaries are fixed (local wall-clock hours):
/// morning `06:00–11:00`, day `11:00–17:00`, evening `17:00–21:00`, night
/// otherwise (`21:00–06:00`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Morning,
    Day,
    Evening,
    Night,
}

impl Slot {
    /// All slots in schedule order, for building the rotation UI.
    pub const ALL: [Slot; 4] = [Slot::Morning, Slot::Day, Slot::Evening, Slot::Night];

    /// The slot covering wall-clock `hour` (0–23; values ≥ 24 wrap).
    #[must_use]
    pub fn for_hour(hour: u32) -> Slot {
        match hour % 24 {
            6..=10 => Slot::Morning,
            11..=16 => Slot::Day,
            17..=20 => Slot::Evening,
            _ => Slot::Night,
        }
    }

    /// Human label for the rotation UI (e.g. `"Morning"`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Slot::Morning => "Morning",
            Slot::Day => "Day",
            Slot::Evening => "Evening",
            Slot::Night => "Night",
        }
    }

    /// The slot's clock range as a subtitle hint (e.g. `"06:00 – 11:00"`).
    #[must_use]
    pub fn range_label(self) -> &'static str {
        match self {
            Slot::Morning => "06:00 \u{2013} 11:00",
            Slot::Day => "11:00 \u{2013} 17:00",
            Slot::Evening => "17:00 \u{2013} 21:00",
            Slot::Night => "21:00 \u{2013} 06:00",
        }
    }
}

/// Time-of-day rotation config: an enable flag plus a per-slot image path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rotation {
    /// Whether rotation is active. When on it overrides the static per-output
    /// selection (rotation is a whole-screen mode).
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub morning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evening: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub night: Option<String>,
}

impl Rotation {
    /// The image configured for `slot`, if any.
    #[must_use]
    pub fn image(&self, slot: Slot) -> Option<&str> {
        match slot {
            Slot::Morning => self.morning.as_deref(),
            Slot::Day => self.day.as_deref(),
            Slot::Evening => self.evening.as_deref(),
            Slot::Night => self.night.as_deref(),
        }
    }

    fn set(&mut self, slot: Slot, image: Option<String>) {
        let field = match slot {
            Slot::Morning => &mut self.morning,
            Slot::Day => &mut self.day,
            Slot::Evening => &mut self.evening,
            Slot::Night => &mut self.night,
        };
        *field = image;
    }

    /// `true` when nothing is configured — off and every slot empty. Used to
    /// omit the whole block from the serialized state.
    fn is_default(&self) -> bool {
        !self.enabled
            && self.morning.is_none()
            && self.day.is_none()
            && self.evening.is_none()
            && self.night.is_none()
    }
}

/// The complete wallpaper selection. Serialized to `wallpaper.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallpaperState {
    /// Image applied to every output without a specific override. This is the
    /// migration target for a pre-#546 single-path file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Per-connector overrides, keyed by output name (e.g. `"DP-1"`). Sorted
    /// (`BTreeMap`) so the derived render is deterministic.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, String>,
    /// Time-of-day rotation.
    #[serde(default, skip_serializing_if = "Rotation::is_default")]
    pub rotation: Rotation,
}

// ── Pure render / migration logic ────────────────────────────────────────────

/// The whole-screen image active for `state` at wall-clock `hour`: the current
/// rotation slot's image (falling back to the default) when rotation is on, else
/// the plain default.
fn active_global(state: &WallpaperState, hour: u32) -> Option<&str> {
    if state.rotation.enabled {
        state
            .rotation
            .image(Slot::for_hour(hour))
            .or(state.default.as_deref())
    } else {
        state.default.as_deref()
    }
}

/// The swaybg argument vector for `state` at wall-clock `hour`. An empty vector
/// means *no wallpaper* — the caller stops the unit rather than launching
/// swaybg with no image.
fn swaybg_args(state: &WallpaperState, hour: u32) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(image) = active_global(state, hour) {
        args.extend([
            "-i".to_string(),
            image.to_string(),
            "-m".to_string(),
            "fill".to_string(),
        ]);
    }
    // Per-output overrides apply only in static mode — rotation is whole-screen.
    if !state.rotation.enabled {
        for (name, image) in &state.outputs {
            args.extend([
                "-o".to_string(),
                name.clone(),
                "-i".to_string(),
                image.clone(),
                "-m".to_string(),
                "fill".to_string(),
            ]);
        }
    }
    args
}

/// One representative image for single-image consumers (the legacy path file and
/// a custom reload command): the whole-screen image, else the first per-output
/// override. `None` ⇒ nothing is set.
fn primary_image(state: &WallpaperState, hour: u32) -> Option<String> {
    active_global(state, hour)
        .map(str::to_string)
        .or_else(|| state.outputs.values().next().cloned())
}

/// Build the initial state from the on-disk file contents. Prefers the
/// structured `wallpaper.json`; on its absence (or a parse error) migrates a
/// legacy single-line `wallpaper.path` into [`WallpaperState::default`].
fn state_from_disk(json: Option<&str>, legacy: Option<&str>) -> WallpaperState {
    if let Some(text) = json {
        match serde_json::from_str::<WallpaperState>(text) {
            Ok(state) => return state,
            Err(e) => {
                tracing::warn!(error = %e, "wallpaper: state file parse failed; falling back");
            }
        }
    }
    if let Some(text) = legacy {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return WallpaperState {
                default: Some(trimmed.to_string()),
                ..WallpaperState::default()
            };
        }
    }
    WallpaperState::default()
}

/// Serialize the swaybg argument vector to the newline-delimited `swaybg.args`
/// body (one arg per line, trailing newline). The unit's `ExecStart` reads it
/// back a line at a time, so spaces in paths survive.
fn args_file_body(args: &[String]) -> String {
    let mut body = args.join("\n");
    body.push('\n');
    body
}

// ── Disk I/O (off the GTK main thread) ───────────────────────────────────────

fn load_state() -> WallpaperState {
    let json = config_file::read(STATE_FILE);
    let legacy = config_file::read(LEGACY_PATH_FILE);
    state_from_disk(json.as_deref(), legacy.as_deref())
}

/// The swaybg args already on disk (one per line), used to seed the render dedup
/// cache so a shell restart whose state hasn't changed doesn't needlessly
/// restart swaybg (and flicker). `None` when no render file exists yet.
fn read_existing_args() -> Option<Vec<String>> {
    config_file::read(ARGS_FILE).map(|text| text.lines().map(String::from).collect())
}

fn persist_state(state: &WallpaperState) {
    let state = state.clone();
    runtime::handle().spawn_blocking(move || match serde_json::to_string_pretty(&state) {
        Ok(json) => {
            config_file::write("wallpaper", STATE_FILE, &format!("{json}\n"));
        }
        Err(e) => tracing::warn!(error = %e, "wallpaper: failed to serialize state"),
    });
}

/// Write (or, when empty, remove) the render files the swaybg unit and legacy
/// consumers read.
fn write_render_files(args: &[String], primary: Option<&str>) {
    if args.is_empty() {
        config_file::remove("wallpaper", ARGS_FILE);
        config_file::remove("wallpaper", LEGACY_PATH_FILE);
        return;
    }
    config_file::write("wallpaper", ARGS_FILE, &args_file_body(args));
    match primary {
        Some(path) => {
            config_file::write("wallpaper", LEGACY_PATH_FILE, &format!("{path}\n"));
        }
        None => config_file::remove("wallpaper", LEGACY_PATH_FILE),
    }
}

/// Apply a render: write the files, then reload the daemon. Runs on a blocking
/// pool thread.
fn apply_render(args: &[String], primary: Option<&str>) {
    write_render_files(args, primary);
    reload(args, primary);
}

/// Tell the wallpaper daemon to pick up the freshly written render. Runs the
/// command named by [`RELOAD_CMD_ENV`] when set; otherwise restarts the bundled
/// swaybg unit — or *stops* it when the render is empty (a cleared wallpaper).
fn reload(args: &[String], primary: Option<&str>) {
    if let Ok(cmd) = std::env::var(RELOAD_CMD_ENV)
        && !cmd.trim().is_empty()
    {
        match primary {
            Some(path) => run_reload_command(&cmd, path),
            None => {
                tracing::debug!("wallpaper: cleared; custom reload command has no clear action");
            }
        }
        return;
    }
    if args.is_empty() {
        systemctl(&["--user", "stop", SWAYBG_UNIT]);
    } else {
        systemctl(&["--user", "restart", SWAYBG_UNIT]);
    }
}

/// Run a user-configured reload command via `sh -c`. Any [`PATH_PLACEHOLDER`] in
/// the command is replaced with the primary path, shell-quoted; the path is also
/// exported as [`WALLPAPER_PATH_ENV`].
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

fn systemctl(args: &[&str]) {
    let status = std::process::Command::new("systemctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(?s, ?args, "wallpaper: systemctl exited non-zero"),
        Err(e) => tracing::warn!(error = %e, ?args, "wallpaper: failed to spawn systemctl"),
    }
}

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct WallpaperHandles {
    pub(crate) state: Mutable<WallpaperState>,
    /// The swaybg args we last applied. Render dedup on the main thread — keeps
    /// the 60 s rotation tick (and repeated no-op picks) from churning the unit
    /// when the effective render is unchanged.
    last_args: RefCell<Option<Vec<String>>>,
}

impl Default for WallpaperHandles {
    fn default() -> Self {
        Self {
            state: Mutable::new(load_state()),
            // Seed from disk so the startup render only restarts swaybg when the
            // persisted state actually implies different args than are already
            // applied — an unchanged shell restart is then a no-op.
            last_args: RefCell::new(read_existing_args()),
        }
    }
}

/// Render the current `state`, deduping against `last_args`. Persists nothing —
/// callers that mutate state persist separately; this is the render side only,
/// shared by mutations, the initial apply, and the rotation tick.
fn render(state: &WallpaperState, last_args: &RefCell<Option<Vec<String>>>) {
    let hour = Local::now().hour();
    let args = swaybg_args(state, hour);
    if last_args.borrow().as_deref() == Some(args.as_slice()) {
        return;
    }
    *last_args.borrow_mut() = Some(args.clone());
    let primary = primary_image(state, hour);
    runtime::handle().spawn_blocking(move || apply_render(&args, primary.as_deref()));
}

/// Wallpaper service marker. Pass to `App::with` to register the service.
pub struct WallpaperService;

impl Service for WallpaperService {
    type Handles = WallpaperHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = WallpaperHandles::default();

        // Apply the persisted state so a fresh session renders it (and a
        // rotation slot that changed while we were off gets picked up).
        render(&handles.state.get_cloned(), &handles.last_args);

        // Time-of-day rotation tick. Cheap: it early-returns when rotation is
        // off, and `render` dedups so an unchanged slot never touches the unit.
        glib::timeout_add_local(ROTATION_TICK, || {
            tick_rotation();
            glib::ControlFlow::Continue
        });

        handles
    }
}

#[must_use]
pub fn service() -> WallpaperService {
    WallpaperService
}

fn tick_rotation() {
    registry::with(|r| {
        if let Some(h) = r.get::<WallpaperHandles>() {
            let state = h.state.get_cloned();
            if state.rotation.enabled {
                render(&state, &h.last_args);
            }
        }
    });
}

// ── Public API — signals ─────────────────────────────────────────────────────

/// Signal of the full wallpaper selection. The appearance panel combines this
/// with the connected-output list to build the per-output rows.
pub fn state() -> impl Signal<Item = WallpaperState> {
    registry::with(|r| {
        r.get::<WallpaperHandles>()
            .expect("wallpaper::service() not registered")
            .state
            .signal_cloned()
    })
}

/// Signal of the default (all-outputs) image path. `None` when unset.
pub fn default_path() -> impl Signal<Item = Option<String>> {
    state().map(|s| s.default)
}

// ── Public API — commands ────────────────────────────────────────────────────

/// Read-modify-write the state on the GTK main thread, then persist + render if
/// it changed. All mutation commands route through here.
fn mutate(edit: impl FnOnce(&mut WallpaperState)) {
    let ok = registry::with(|r| {
        let Some(h) = r.get::<WallpaperHandles>() else {
            return false;
        };
        let before = h.state.get_cloned();
        let mut next = before.clone();
        edit(&mut next);
        if next == before {
            return true; // no-op: don't churn disk or the unit
        }
        h.state.set(next.clone());
        persist_state(&next);
        render(&next, &h.last_args);
        true
    });
    if !ok {
        tracing::warn!("wallpaper::service() not registered");
    }
}

/// Non-empty trim of a user-supplied path, or `None` (with a warning) if blank.
fn clean(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        tracing::warn!("wallpaper: refusing empty path");
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Set the default image applied to every output without a specific override.
pub fn set_default(path: &str) {
    if let Some(value) = clean(path) {
        mutate(|s| s.default = Some(value));
    }
}

/// Clear the default image (outputs with an override keep it).
pub fn clear_default() {
    mutate(|s| s.default = None);
}

/// Set a per-output override for connector `name`.
pub fn set_output(name: &str, path: &str) {
    if let Some(value) = clean(path) {
        let name = name.to_string();
        mutate(move |s| {
            s.outputs.insert(name, value);
        });
    }
}

/// Remove the per-output override for connector `name` (falls back to default).
pub fn clear_output(name: &str) {
    let name = name.to_string();
    mutate(move |s| {
        s.outputs.remove(&name);
    });
}

/// Enable or disable time-of-day rotation.
pub fn set_rotation_enabled(on: bool) {
    mutate(move |s| s.rotation.enabled = on);
}

/// Set the image for a rotation `slot`.
pub fn set_slot_image(slot: Slot, path: &str) {
    if let Some(value) = clean(path) {
        mutate(move |s| s.rotation.set(slot, Some(value)));
    }
}

/// Clear the image for a rotation `slot`.
pub fn clear_slot(slot: Slot) {
    mutate(move |s| s.rotation.set(slot, None));
}

/// Clear everything — default, all per-output overrides, and rotation — back to
/// no wallpaper. Backs the panel's explicit "Clear wallpaper" button (#546).
pub fn clear() {
    mutate(|s| *s = WallpaperState::default());
}

#[cfg(test)]
mod tests {
    use super::{
        PATH_PLACEHOLDER, Rotation, Slot, WallpaperState, args_file_body, primary_image,
        shell_single_quote, state_from_disk, swaybg_args,
    };
    use std::collections::BTreeMap;

    fn expand(cmd: &str, path: &str) -> String {
        cmd.replace(PATH_PLACEHOLDER, &shell_single_quote(path))
    }

    // ── shell quoting (unchanged from v1) ───────────────────────────────────

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
        assert_eq!(shell_single_quote("/a/o'brien.png"), r"'/a/o'\''brien.png'");
    }

    #[test]
    fn command_without_placeholder_is_unchanged() {
        assert_eq!(
            expand("systemctl --user restart swww.service", "/x.png"),
            "systemctl --user restart swww.service"
        );
    }

    // ── slot schedule ───────────────────────────────────────────────────────

    #[test]
    fn slot_boundaries() {
        assert_eq!(Slot::for_hour(0), Slot::Night);
        assert_eq!(Slot::for_hour(5), Slot::Night);
        assert_eq!(Slot::for_hour(6), Slot::Morning);
        assert_eq!(Slot::for_hour(10), Slot::Morning);
        assert_eq!(Slot::for_hour(11), Slot::Day);
        assert_eq!(Slot::for_hour(16), Slot::Day);
        assert_eq!(Slot::for_hour(17), Slot::Evening);
        assert_eq!(Slot::for_hour(20), Slot::Evening);
        assert_eq!(Slot::for_hour(21), Slot::Night);
        assert_eq!(Slot::for_hour(23), Slot::Night);
    }

    #[test]
    fn slot_hour_wraps() {
        // Values ≥ 24 wrap so a caller can't panic us with a bogus hour.
        assert_eq!(Slot::for_hour(30), Slot::Morning);
    }

    // ── migration / parse ───────────────────────────────────────────────────

    #[test]
    fn migrates_legacy_single_path() {
        let state = state_from_disk(None, Some("  /home/a/wall.png\n"));
        assert_eq!(state.default.as_deref(), Some("/home/a/wall.png"));
        assert!(state.outputs.is_empty());
        assert!(!state.rotation.enabled);
    }

    #[test]
    fn legacy_blank_is_no_wallpaper() {
        assert_eq!(
            state_from_disk(None, Some("   \n")),
            WallpaperState::default()
        );
        assert_eq!(state_from_disk(None, None), WallpaperState::default());
    }

    #[test]
    fn json_wins_over_legacy() {
        let json = r#"{"default":"/j.png"}"#;
        let state = state_from_disk(Some(json), Some("/legacy.png"));
        assert_eq!(state.default.as_deref(), Some("/j.png"));
    }

    #[test]
    fn unparseable_json_falls_back_to_legacy() {
        let state = state_from_disk(Some("{ not json"), Some("/legacy.png"));
        assert_eq!(state.default.as_deref(), Some("/legacy.png"));
    }

    #[test]
    fn json_round_trip() {
        let mut state = WallpaperState {
            default: Some("/d.png".into()),
            outputs: BTreeMap::new(),
            rotation: Rotation {
                enabled: true,
                morning: Some("/m.png".into()),
                night: Some("/n.png".into()),
                ..Rotation::default()
            },
        };
        state.outputs.insert("DP-1".into(), "/dp1.png".into());
        state.outputs.insert("eDP-1".into(), "/edp1.png".into());

        let text = serde_json::to_string(&state).unwrap();
        let back: WallpaperState = serde_json::from_str(&text).unwrap();
        assert_eq!(state, back);
    }

    // ── render ──────────────────────────────────────────────────────────────

    fn args(pairs: &[&str]) -> Vec<String> {
        pairs.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn empty_state_renders_nothing() {
        assert!(swaybg_args(&WallpaperState::default(), 8).is_empty());
        assert_eq!(primary_image(&WallpaperState::default(), 8), None);
    }

    #[test]
    fn default_only_is_global_image() {
        let state = WallpaperState {
            default: Some("/d.png".into()),
            ..WallpaperState::default()
        };
        assert_eq!(
            swaybg_args(&state, 8),
            args(&["-i", "/d.png", "-m", "fill"])
        );
        assert_eq!(primary_image(&state, 8).as_deref(), Some("/d.png"));
    }

    #[test]
    fn per_output_overrides_follow_default() {
        let mut state = WallpaperState {
            default: Some("/d.png".into()),
            ..WallpaperState::default()
        };
        state.outputs.insert("DP-1".into(), "/dp1.png".into());
        assert_eq!(
            swaybg_args(&state, 8),
            args(&[
                "-i", "/d.png", "-m", "fill", // global first
                "-o", "DP-1", "-i", "/dp1.png", "-m", "fill",
            ])
        );
    }

    #[test]
    fn per_output_only_has_no_global() {
        let mut state = WallpaperState::default();
        state.outputs.insert("eDP-1".into(), "/e.png".into());
        assert_eq!(
            swaybg_args(&state, 8),
            args(&["-o", "eDP-1", "-i", "/e.png", "-m", "fill"])
        );
        // Primary falls through to the first per-output override.
        assert_eq!(primary_image(&state, 8).as_deref(), Some("/e.png"));
    }

    #[test]
    fn rotation_active_slot_wins_over_static() {
        let mut state = WallpaperState {
            default: Some("/d.png".into()),
            rotation: Rotation {
                enabled: true,
                morning: Some("/m.png".into()),
                ..Rotation::default()
            },
            ..WallpaperState::default()
        };
        state.outputs.insert("DP-1".into(), "/dp1.png".into());
        // Hour 8 ⇒ Morning slot. Global = morning image, per-output suppressed.
        assert_eq!(
            swaybg_args(&state, 8),
            args(&["-i", "/m.png", "-m", "fill"])
        );
        assert_eq!(primary_image(&state, 8).as_deref(), Some("/m.png"));
    }

    #[test]
    fn rotation_empty_slot_falls_back_to_default() {
        let state = WallpaperState {
            default: Some("/d.png".into()),
            rotation: Rotation {
                enabled: true,
                morning: Some("/m.png".into()),
                ..Rotation::default()
            },
            ..WallpaperState::default()
        };
        // Hour 14 ⇒ Day slot, which is unset ⇒ fall back to the default.
        assert_eq!(
            swaybg_args(&state, 14),
            args(&["-i", "/d.png", "-m", "fill"])
        );
    }

    #[test]
    fn args_file_round_trips_through_lines() {
        // `read_existing_args` parses the file back with `str::lines`; that must
        // reproduce exactly what `args_file_body` wrote (spaces in paths and all)
        // or the startup dedup seed silently mis-fires.
        let original = args(&[
            "-i",
            "/home/a/My Wall.png",
            "-m",
            "fill",
            "-o",
            "DP-1",
            "-i",
            "/x.png",
        ]);
        let body = args_file_body(&original);
        let parsed: Vec<String> = body.lines().map(ToString::to_string).collect();
        assert_eq!(parsed, original);
    }

    #[test]
    fn disabled_rotation_ignores_slots() {
        let state = WallpaperState {
            default: Some("/d.png".into()),
            rotation: Rotation {
                enabled: false,
                morning: Some("/m.png".into()),
                ..Rotation::default()
            },
            ..WallpaperState::default()
        };
        // Rotation off ⇒ slots ignored, default renders.
        assert_eq!(
            swaybg_args(&state, 8),
            args(&["-i", "/d.png", "-m", "fill"])
        );
    }
}
