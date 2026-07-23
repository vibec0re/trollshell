//! Screen-recording service — drives an external `wf-recorder` process and
//! exposes its state reactively (#403).
//!
//! Daemon-as-state-store, the sibling of the screenshot flow (#220) and the
//! screen-cast privacy indicator (#221): the shell does **not** encode video.
//! It spawns `wf-recorder` (wlr-screencopy, works under niri), tracks whether
//! a recording is running plus its elapsed seconds and output path, and — when
//! the recording stops cleanly — surfaces the saved file once via [`saved`] so
//! the shell can post a "Recording saved" toast (mirroring
//! [`niri::CapturedShot`](crate::niri::CapturedShot) → the #220 toast).
//!
//! ## Lifecycle
//!
//! [`start`] picks a region with `slurp` (mirroring how the screenshot flow
//! picks a region) and spawns `wf-recorder -f <file> -g <geometry>`. [`stop`]
//! sends **SIGINT** — not SIGKILL — so `wf-recorder` finalizes the container
//! (a hard kill would truncate/corrupt the `.mp4`); the same SIGINT idiom the
//! `screensaver` service uses on swayidle. A driver task owns the child and a
//! 1 Hz ticker; both the spawn/kill and the elapsed clock live on the tokio
//! runtime, never on the GTK thread.
//!
//! ## v1 decisions (#403)
//!
//! - `wf-recorder` over niri's portal-screencast path — simpler, dep-light.
//! - Audio (`--audio`) is **off by default**; `TROLLSHELL_RECORD_AUDIO=1` sets
//!   the initial default and the Settings drawer page (`panels/settings.rs`,
//!   #421) exposes an in-shell toggle ([`audio_enabled`]/[`set_audio_enabled`])
//!   that overrides it for the session — neither persists across restarts.
//! - `wf-recorder` and `slurp` are external tools that must be on `PATH`; the
//!   nix module now provisions both (#421) — the screenshot flow itself
//!   provisions nothing (niri captures its own screenshots), so there was
//!   nothing to mirror there.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// How long to wait for `wf-recorder` to flush and exit after SIGINT before
/// escalating to SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(5);

// ── Public data shapes ──────────────────────────────────────────────────────

/// Reactive state of the screen recorder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RecordingState {
    /// No recording in progress.
    #[default]
    Idle,
    /// A recording is running: `path` is where `wf-recorder` is writing and
    /// `elapsed_secs` ticks up once per second.
    Recording {
        /// Absolute path of the file `wf-recorder` is writing.
        path: String,
        /// Whole seconds since the recording started.
        elapsed_secs: u64,
    },
}

impl RecordingState {
    /// Whether a recording is currently running.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        matches!(self, Self::Recording { .. })
    }

    /// The output file path while recording, `None` when idle.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Recording { path, .. } => Some(path),
            Self::Idle => None,
        }
    }

    /// Elapsed whole seconds while recording, `None` when idle.
    #[must_use]
    pub fn elapsed_secs(&self) -> Option<u64> {
        match self {
            Self::Recording { elapsed_secs, .. } => Some(*elapsed_secs),
            Self::Idle => None,
        }
    }

    /// The chip's timer label: the formatted elapsed time while recording,
    /// `None` when idle.
    #[must_use]
    pub fn label(&self) -> Option<String> {
        self.elapsed_secs().map(format_elapsed)
    }

    /// Fresh `Recording` state at zero elapsed for `path`.
    fn recording(path: impl Into<String>) -> Self {
        Self::Recording {
            path: path.into(),
            elapsed_secs: 0,
        }
    }

    /// Same variant with the elapsed clock advanced; `Idle` stays `Idle`.
    fn with_elapsed(&self, elapsed_secs: u64) -> Self {
        match self {
            Self::Recording { path, .. } => Self::Recording {
                path: path.clone(),
                elapsed_secs,
            },
            Self::Idle => Self::Idle,
        }
    }
}

/// A completed recording, surfaced once via [`saved`] when a recording stops
/// so the shell can post a "Recording saved" toast — the recording sibling of
/// [`niri::CapturedShot`](crate::niri::CapturedShot).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedRecording {
    /// Absolute path of the saved video file.
    pub path: String,
}

// ── Service ─────────────────────────────────────────────────────────────────

/// Commands sent from the GTK thread to the driver task.
enum Cmd {
    Start,
    Stop,
    Toggle,
}

/// The screen-recording service handle.
pub struct RecorderService;

/// Internal handles holding the reactive state and the driver's command
/// channel.
#[doc(hidden)]
pub struct RecorderHandles {
    state: Mutable<RecordingState>,
    saved: Mutable<Option<SavedRecording>>,
    audio: Mutable<bool>,
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Service for RecorderService {
    type Handles = RecorderHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let state = Mutable::new(RecordingState::Idle);
        let saved = Mutable::new(None);
        let audio = Mutable::new(audio_enabled_default());
        let (tx, rx) = mpsc::unbounded_channel();
        rt.spawn(drive(state.clone(), saved.clone(), audio.clone(), rx));
        RecorderHandles {
            state,
            saved,
            audio,
            tx,
        }
    }
}

#[must_use]
pub fn service() -> RecorderService {
    RecorderService
}

/// Subscribe to the recorder's state (idle vs recording + elapsed + path).
pub fn state() -> impl Signal<Item = RecordingState> {
    registry::with(|r| {
        r.get::<RecorderHandles>()
            .expect("recorder::service() not registered")
            .state
            .signal_cloned()
    })
}

/// Fires once (a `Some`) each time a recording stops with a file on disk, so a
/// single global subscription can post the saved toast. Mirrors
/// [`niri::screenshot_captured`](crate::niri::screenshot_captured).
pub fn saved() -> impl Signal<Item = Option<SavedRecording>> {
    registry::with(|r| {
        r.get::<RecorderHandles>()
            .expect("recorder::service() not registered")
            .saved
            .signal_cloned()
    })
}

/// Subscribe to whether the *next* recording will capture audio
/// (`wf-recorder --audio`). Seeded from `TROLLSHELL_RECORD_AUDIO`; the
/// Settings drawer page toggles it at runtime via [`set_audio_enabled`].
/// Changing it never affects a recording already in progress.
pub fn audio_enabled() -> impl Signal<Item = bool> {
    registry::with(|r| {
        r.get::<RecorderHandles>()
            .expect("recorder::service() not registered")
            .audio
            .signal_cloned()
    })
}

/// Set whether audio is captured on the next recording (fire-and-forget).
/// Session-only — not persisted to disk, mirroring how `RecordingState`
/// itself resets to `Idle` on restart.
pub fn set_audio_enabled(on: bool) {
    registry::with(|r| {
        if let Some(handles) = r.get::<RecorderHandles>() {
            handles.audio.set(on);
        } else {
            tracing::warn!("recorder::service() not registered; ignoring set_audio_enabled");
        }
    });
}

/// Start a screen recording — region picked via `slurp` (fire-and-forget).
pub fn start() {
    send(Cmd::Start);
}

/// Stop the current recording, finalize the file, and fire the saved toast
/// (fire-and-forget).
pub fn stop() {
    send(Cmd::Stop);
}

/// Toggle recording: start if idle, stop if recording (fire-and-forget).
pub fn toggle() {
    send(Cmd::Toggle);
}

fn send(cmd: Cmd) {
    registry::with(|r| {
        if let Some(handles) = r.get::<RecorderHandles>() {
            if handles.tx.send(cmd).is_err() {
                tracing::warn!("recorder: command channel closed, driver gone");
            }
        } else {
            tracing::warn!("recorder::service() not registered; ignoring command");
        }
    });
}

// ── Driver ──────────────────────────────────────────────────────────────────

/// Owns the `wf-recorder` child and the 1 Hz elapsed ticker; serializes
/// Start/Stop/Toggle commands and reacts to the child exiting on its own.
async fn drive(
    state: Mutable<RecordingState>,
    saved: Mutable<Option<SavedRecording>>,
    audio: Mutable<bool>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut child: Option<Child> = None;
    let mut started: Option<Instant> = None;
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break }; // all senders dropped
                match cmd {
                    Cmd::Start => start_recording(&mut child, &mut started, &state, &audio).await,
                    Cmd::Stop => {
                        stop_recording(&mut child, &mut started, &state, &saved).await;
                    }
                    Cmd::Toggle => {
                        if child.is_some() {
                            stop_recording(&mut child, &mut started, &state, &saved).await;
                        } else {
                            start_recording(&mut child, &mut started, &state, &audio).await;
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                if let Some(started_at) = started {
                    let next = state.get_cloned().with_elapsed(started_at.elapsed().as_secs());
                    state.set(next);
                }
            }
            status = wait_opt(&mut child) => {
                // wf-recorder exited without us asking (finished, or crashed).
                child = None;
                started = None;
                match status {
                    Ok(s) => tracing::info!(status = ?s, "recorder: wf-recorder exited on its own"),
                    Err(e) => tracing::warn!(error = %e, "recorder: waiting on wf-recorder failed"),
                }
                finalize(&state, &saved).await;
            }
        }
    }
}

/// Await the child's exit, or pend forever when there is no child — so the
/// `select!` arm is inert while idle.
async fn wait_opt(child: &mut Option<Child>) -> std::io::Result<std::process::ExitStatus> {
    match child {
        Some(c) => c.wait().await,
        None => std::future::pending().await,
    }
}

async fn start_recording(
    child: &mut Option<Child>,
    started: &mut Option<Instant>,
    state: &Mutable<RecordingState>,
    audio: &Mutable<bool>,
) {
    if child.is_some() {
        return; // already recording
    }

    let Some(geometry) = pick_region().await else {
        tracing::info!("recorder: region selection cancelled or slurp unavailable; not recording");
        return;
    };

    let dir = videos_dir();
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!(error = %e, dir = %dir.display(), "recorder: could not create output dir");
        return;
    }
    let path = output_path(&dir, Local::now())
        .to_string_lossy()
        .into_owned();

    let args = recorder_args(&path, Some(&geometry), audio.get());
    match Command::new("wf-recorder").args(&args).spawn() {
        Ok(c) => {
            *child = Some(c);
            *started = Some(Instant::now());
            state.set(RecordingState::recording(path.clone()));
            tracing::info!(path, geometry, "recorder: started wf-recorder");
        }
        Err(e) => {
            tracing::warn!(error = %e, "recorder: failed to spawn wf-recorder (is it installed?)");
        }
    }
}

async fn stop_recording(
    child: &mut Option<Child>,
    started: &mut Option<Instant>,
    state: &Mutable<RecordingState>,
    saved: &Mutable<Option<SavedRecording>>,
) {
    let Some(mut c) = child.take() else {
        return; // not recording
    };
    interrupt(&c);
    match tokio::time::timeout(STOP_GRACE, c.wait()).await {
        Ok(Ok(status)) => tracing::info!(status = ?status, "recorder: wf-recorder finalized"),
        Ok(Err(e)) => tracing::warn!(error = %e, "recorder: waiting on wf-recorder failed"),
        Err(_) => {
            tracing::warn!("recorder: wf-recorder did not exit within grace after SIGINT; killing");
            let _ = c.kill().await;
        }
    }
    *started = None;
    finalize(state, saved).await;
}

/// Send SIGINT so `wf-recorder` flushes and closes the container cleanly
/// (a SIGKILL would truncate the file). Same idiom as `screensaver`'s swayidle
/// signalling.
fn interrupt(child: &Child) {
    let Some(pid) = child.id() else {
        return;
    };
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    let pid = nix::unistd::Pid::from_raw(pid);
    if let Err(e) = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT) {
        tracing::warn!(error = %e, "recorder: SIGINT to wf-recorder failed");
    }
}

/// Return to `Idle` and, if the recording produced a file, surface it once via
/// `saved` (the toast trigger). Reads the path off the current `Recording`
/// state before clearing it.
async fn finalize(state: &Mutable<RecordingState>, saved: &Mutable<Option<SavedRecording>>) {
    let path = match state.get_cloned() {
        RecordingState::Recording { path, .. } => Some(path),
        RecordingState::Idle => None,
    };
    state.set(RecordingState::Idle);
    if let Some(path) = path {
        match tokio::fs::try_exists(&path).await {
            Ok(true) => saved.set(Some(SavedRecording { path })),
            Ok(false) => tracing::warn!(path, "recorder: output file missing after stop; no toast"),
            Err(e) => tracing::warn!(error = %e, path, "recorder: could not stat output file"),
        }
    }
}

/// Run `slurp` to pick a region. `None` when the user cancelled (non-zero
/// exit / empty output) or `slurp` isn't installed.
async fn pick_region() -> Option<String> {
    let output = match Command::new("slurp").output().await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "recorder: failed to run slurp (is it installed?)");
            return None;
        }
    };
    if !output.status.success() {
        return None; // user pressed Esc
    }
    let geometry = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if geometry.is_empty() {
        None
    } else {
        Some(geometry)
    }
}

// ── Pure helpers (unit-tested) ───────────────────────────────────────────────

/// Build the `wf-recorder` argument vector: `-f <path>`, an optional
/// `-g <geometry>` region, and `--audio` when enabled.
fn recorder_args(path: &str, geometry: Option<&str>, audio: bool) -> Vec<String> {
    let mut args = vec!["-f".to_string(), path.to_string()];
    if let Some(geometry) = geometry {
        args.push("-g".to_string());
        args.push(geometry.to_string());
    }
    if audio {
        args.push("--audio".to_string());
    }
    args
}

/// The initial value of the in-shell audio toggle ([`audio_enabled`]), read
/// once at service start. Off by default (#403 v1); `TROLLSHELL_RECORD_AUDIO=1`
/// opts in as the starting point, overridable at runtime from Settings.
fn audio_enabled_default() -> bool {
    matches!(
        std::env::var("TROLLSHELL_RECORD_AUDIO").ok().as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Directory recordings are written to: `$XDG_VIDEOS_DIR`, else `$HOME/Videos`,
/// else the system temp dir.
fn videos_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_VIDEOS_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Videos");
    }
    std::env::temp_dir()
}

/// Timestamped output file name, e.g. `trollshell-recording-20260722-153000.mp4`.
fn output_file_name(now: DateTime<Local>) -> String {
    format!("trollshell-recording-{}.mp4", now.format("%Y%m%d-%H%M%S"))
}

fn output_path(dir: &Path, now: DateTime<Local>) -> PathBuf {
    dir.join(output_file_name(now))
}

/// Format an elapsed second count as `MM:SS`, or `H:MM:SS` past an hour.
fn format_elapsed(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // ── format_elapsed ────────────────────────────────────────────────────────

    #[test]
    fn format_elapsed_under_a_minute() {
        assert_eq!(format_elapsed(0), "00:00");
        assert_eq!(format_elapsed(5), "00:05");
        assert_eq!(format_elapsed(59), "00:59");
    }

    #[test]
    fn format_elapsed_minutes() {
        assert_eq!(format_elapsed(60), "01:00");
        assert_eq!(format_elapsed(65), "01:05");
        assert_eq!(format_elapsed(599), "09:59");
        assert_eq!(format_elapsed(3599), "59:59");
    }

    #[test]
    fn format_elapsed_hours() {
        assert_eq!(format_elapsed(3600), "1:00:00");
        assert_eq!(format_elapsed(3661), "1:01:01");
        assert_eq!(format_elapsed(36_000), "10:00:00");
    }

    // ── RecordingState machine: idle → recording → idle ──────────────────────

    #[test]
    fn idle_has_no_recording_fields() {
        let s = RecordingState::Idle;
        assert!(!s.is_recording());
        assert_eq!(s.path(), None);
        assert_eq!(s.elapsed_secs(), None);
        assert_eq!(s.label(), None);
    }

    #[test]
    fn default_state_is_idle() {
        assert_eq!(RecordingState::default(), RecordingState::Idle);
    }

    #[test]
    fn recording_starts_at_zero_elapsed() {
        let s = RecordingState::recording("/tmp/out.mp4");
        assert!(s.is_recording());
        assert_eq!(s.path(), Some("/tmp/out.mp4"));
        assert_eq!(s.elapsed_secs(), Some(0));
        assert_eq!(s.label().as_deref(), Some("00:00"));
    }

    #[test]
    fn with_elapsed_advances_clock_and_keeps_path() {
        let s = RecordingState::recording("/tmp/out.mp4").with_elapsed(65);
        assert!(s.is_recording());
        assert_eq!(s.path(), Some("/tmp/out.mp4"));
        assert_eq!(s.elapsed_secs(), Some(65));
        assert_eq!(s.label().as_deref(), Some("01:05"));
    }

    #[test]
    fn with_elapsed_is_a_noop_on_idle() {
        // Stop → Idle: advancing the clock on idle must not resurrect a recording.
        assert_eq!(RecordingState::Idle.with_elapsed(42), RecordingState::Idle);
    }

    #[test]
    fn full_cycle_idle_recording_idle() {
        let idle = RecordingState::Idle;
        assert!(!idle.is_recording());

        let recording = RecordingState::recording("/tmp/clip.mp4");
        assert!(recording.is_recording());
        let ticked = recording.with_elapsed(3);
        assert_eq!(ticked.label().as_deref(), Some("00:03"));

        // Stop resets to Idle regardless of prior elapsed.
        let stopped = RecordingState::Idle;
        assert!(!stopped.is_recording());
        assert_eq!(stopped.label(), None);
    }

    // ── recorder_args ─────────────────────────────────────────────────────────

    #[test]
    fn recorder_args_region_no_audio() {
        assert_eq!(
            recorder_args("/tmp/a.mp4", Some("0,0 640x480"), false),
            vec!["-f", "/tmp/a.mp4", "-g", "0,0 640x480"],
        );
    }

    #[test]
    fn recorder_args_region_with_audio() {
        assert_eq!(
            recorder_args("/tmp/a.mp4", Some("0,0 640x480"), true),
            vec!["-f", "/tmp/a.mp4", "-g", "0,0 640x480", "--audio"],
        );
    }

    #[test]
    fn recorder_args_no_region() {
        assert_eq!(
            recorder_args("/tmp/a.mp4", None, false),
            vec!["-f", "/tmp/a.mp4"],
        );
    }

    // ── output file name ──────────────────────────────────────────────────────

    #[test]
    fn output_file_name_is_timestamped_mp4() {
        let now = Local.with_ymd_and_hms(2026, 7, 22, 15, 30, 0).unwrap();
        assert_eq!(
            output_file_name(now),
            "trollshell-recording-20260722-153000.mp4",
        );
    }

    #[test]
    fn output_path_joins_dir_and_name() {
        let now = Local.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let path = output_path(Path::new("/videos"), now);
        assert_eq!(
            path,
            Path::new("/videos/trollshell-recording-20260102-030405.mp4"),
        );
    }
}
