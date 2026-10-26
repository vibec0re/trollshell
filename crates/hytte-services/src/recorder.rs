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
//! picks a region) and launches `wf-recorder -f <file> -g <geometry>`. [`stop`]
//! sends **SIGINT** — not SIGKILL — so `wf-recorder` finalizes the container
//! (a hard kill would truncate/corrupt the `.mp4`); the same SIGINT idiom the
//! `screensaver` service uses on swayidle. A driver task owns the recorder and
//! a 1 Hz ticker; both the launch/kill and the elapsed clock live on the tokio
//! runtime, never on the GTK thread.
//!
//! ## Its own unit, so a shell restart keeps recording
//!
//! `wf-recorder` runs as the transient **user** unit `trollshell-recorder.service`
//! (`systemd-run --user`), not as a child of the shell. As a child it sat in
//! `trollshell.service`'s cgroup, so a shell crash (`Restart=on-failure`) or a
//! `systemctl --user restart trollshell` stopped the recording with it. (The
//! file itself survived: wf-recorder 0.6's `GRACEFUL_TERMINATION_SIGNALS` are
//! `SIGTERM`, `SIGINT` and `SIGHUP`, all through one handler. What was lost
//! was the rest of the recording and the toast.) In its own unit it outlives
//! the shell, and the restarted shell **adopts** it: the unit's
//! `Description=` carries the output path and its `ActiveEnterTimestamp` the
//! start, so the chip shows the right file and clock and Stop works.
//!
//! - `PartOf=graphical-session.target`, the plugin launcher's default target
//!   (`niri-session.target` is itself `PartOf=` it), so session teardown
//!   still stops the recorder. Going by wf-recorder 0.6's `main`, a recorder
//!   that outlived its compositor would not exit on its own: its capture loop
//!   ends on the signal flag, not when `wl_display_dispatch` fails.
//! - No `--collect`, unlike the plugin launcher: a unit that **fails** stays
//!   loaded as `failed`, and its `Result`/`ExecMainStatus` is how the driver
//!   learns that a recording failed. The driver resets it once read, and a
//!   unit that exits cleanly is collected anyway.
//! - The driver polls the unit's `ActiveState` on its 1 Hz tick while a
//!   recording runs, and every 100 ms while a Stop waits.
//! - With no `systemd-run` or no user manager, it spawns `wf-recorder` as a
//!   child exactly as before, which is what a detached `RunCommand` does too.
//!
//! A recording that ends unsuccessfully (a non-zero exit, a kill, or no
//! non-empty file) is toasted as a failure, never as "Recording saved".
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
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Local};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry};
use nix::sys::signal::Signal as Sig;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::systemd;

/// How long to wait for `wf-recorder` to flush and exit after SIGINT before
/// escalating to SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long to wait for the recorder unit to register a SIGKILL, so its
/// `failed` state is read and reset rather than left for the next start.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// How often a Stop re-reads the recorder unit while it waits for the exit.
const STOP_POLL: Duration = Duration::from_millis(100);

/// The fixed name of the transient user unit `wf-recorder` runs in. One name,
/// because there is at most one recording, and a fixed one is what lets a
/// restarted shell find the recording again.
const RECORDER_UNIT: &str = "trollshell-recorder.service";

/// The unit's `Description=` up to the output path, which follows it verbatim
/// (measured on systemd 260: a transient unit's description is stored as given,
/// with no `%` specifier expansion), so adoption reads the path back out.
const DESCRIPTION_PREFIX: &str = "trollshell screen recording: ";

/// The target whose stop also stops the recorder unit: session teardown. See
/// the module doc.
const SESSION_TARGET: &str = "graphical-session.target";

/// The shell's own variables the recorder unit is given, when the shell has
/// them. The unit otherwise gets the user manager's environment, which
/// `etc/niri/session.kdl`'s `import-environment` fills in for a real session.
/// In a dev loop inside a nested compositor, though, the manager holds the
/// outer session's values, and the recording would capture the wrong screen.
/// So these two (the display, and where its socket and the audio server live)
/// are passed explicitly, as the detached `RunCommand` launcher does.
const FORWARDED_ENV: [&str; 2] = ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR"];

/// Bounds the `systemd-run` call itself, which returns as soon as the user
/// manager has taken the start job (milliseconds).
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds each call to the user manager (see [`bounded`]). systemd answers in
/// milliseconds; this is for a session bus that is down.
const BUS_TIMEOUT: Duration = Duration::from_secs(1);

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

/// The running recorder.
enum Recorder {
    /// `wf-recorder` runs as [`RECORDER_UNIT`], and the driver polls the unit.
    Unit,
    /// The fallback where no user manager took it: a direct child, in the
    /// shell's own cgroup.
    Child(Child),
}

/// How `wf-recorder` ended, as far as the driver could tell.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Exit {
    /// It exited successfully.
    Clean,
    /// It did not: a non-zero status, a signal, or a kill after the grace.
    /// The text completes "wf-recorder …", e.g. `exited with status 1`.
    Failed(String),
    /// The driver could not learn how it ended (a failed `wait`).
    Unknown,
}

/// Owns the recorder and the 1 Hz elapsed ticker; serializes
/// Start/Stop/Toggle commands and reacts to the recorder exiting on its own.
async fn drive(
    state: Mutable<RecordingState>,
    saved: Mutable<Option<SavedRecording>>,
    audio: Mutable<bool>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut recorder: Option<Recorder> = None;
    let mut started: Option<Instant> = None;
    // A recording the previous shell started may still be running in its own
    // unit: pick it up before taking any command.
    adopt(&mut recorder, &mut started, &state, &saved).await;
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break }; // all senders dropped
                match cmd {
                    Cmd::Start => {
                        start_recording(&mut recorder, &mut started, &state, &audio, &saved).await;
                    }
                    Cmd::Stop => {
                        stop_recording(&mut recorder, &mut started, &state, &saved).await;
                    }
                    Cmd::Toggle => {
                        if recorder.is_some() {
                            stop_recording(&mut recorder, &mut started, &state, &saved).await;
                        } else {
                            start_recording(&mut recorder, &mut started, &state, &audio, &saved)
                                .await;
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                if matches!(recorder, Some(Recorder::Unit)) {
                    match poll_unit().await {
                        Ok(UnitPhase::Live) => {}
                        Ok(UnitPhase::Exited(exit)) => {
                            // wf-recorder exited without us asking.
                            tracing::info!(?exit, "recorder: the recorder unit exited on its own");
                            recorder = None;
                            started = None;
                            finalize(&state, &saved, &exit).await;
                        }
                        // A bus hiccup is not an exit: keep the recording and
                        // look again on the next tick.
                        Err(e) => {
                            tracing::debug!(error = %e, "recorder: could not read the recorder unit");
                        }
                    }
                }
                if let Some(started_at) = started {
                    let next = state.get_cloned().with_elapsed(started_at.elapsed().as_secs());
                    state.set(next);
                }
            }
            status = wait_child(&mut recorder) => {
                // wf-recorder exited without us asking (finished, or crashed).
                recorder = None;
                started = None;
                let exit = match status {
                    Ok(s) => {
                        tracing::info!(status = ?s, "recorder: wf-recorder exited on its own");
                        exit_of_status(s)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "recorder: waiting on wf-recorder failed");
                        Exit::Unknown
                    }
                };
                finalize(&state, &saved, &exit).await;
            }
        }
    }
}

/// Await the child's exit, or pend forever when there is none (idle, or a
/// recorder unit, which the ticker polls instead), so the `select!` arm is
/// inert then.
async fn wait_child(recorder: &mut Option<Recorder>) -> std::io::Result<std::process::ExitStatus> {
    match recorder {
        Some(Recorder::Child(c)) => c.wait().await,
        Some(Recorder::Unit) | None => std::future::pending().await,
    }
}

async fn start_recording(
    recorder: &mut Option<Recorder>,
    started: &mut Option<Instant>,
    state: &Mutable<RecordingState>,
    audio: &Mutable<bool>,
    saved: &Mutable<Option<SavedRecording>>,
) {
    if recorder.is_some() {
        return; // already recording
    }
    // A recorder unit this driver is not tracking is a recording that is
    // already running (adoption at startup can miss one on a bus hiccup):
    // show it rather than start a second one, which systemd would refuse. A
    // stale failed one is reported and reset here, which frees the name.
    if adopt(recorder, started, state, saved).await {
        return;
    }

    let Some(geometry) = pick_region().await else {
        tracing::info!("recorder: region selection cancelled or slurp unavailable; not recording");
        return;
    };

    let dir = videos_dir();
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!(error = %e, dir = %dir.display(), "recorder: could not create output dir");
        toast_start_failure(&format!(
            "Couldn't create the recordings folder {}: {e}",
            dir.display()
        ));
        return;
    }
    let path = output_path(&dir, Local::now())
        .to_string_lossy()
        .into_owned();

    let args = recorder_args(&path, Some(&geometry), audio.get());
    match launch_unit(&path, &args).await {
        Launch::Unit => {
            *recorder = Some(Recorder::Unit);
            *started = Some(Instant::now());
            state.set(RecordingState::recording(path.clone()));
            tracing::info!(
                path,
                geometry,
                unit = RECORDER_UNIT,
                "recorder: started wf-recorder"
            );
        }
        Launch::Refused(body) => {
            tracing::warn!(detail = %body, "recorder: systemd-run refused the recorder unit");
            toast_start_failure(&body);
        }
        Launch::NothingStarted(why) => {
            tracing::info!(
                detail = %why,
                "recorder: no user manager took the recorder; spawning wf-recorder directly",
            );
            match Command::new("wf-recorder").args(&args).spawn() {
                Ok(c) => {
                    *recorder = Some(Recorder::Child(c));
                    *started = Some(Instant::now());
                    state.set(RecordingState::recording(path.clone()));
                    tracing::info!(path, geometry, "recorder: started wf-recorder as a child");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "recorder: failed to spawn wf-recorder (is it installed?)");
                    toast_start_failure(&spawn_error_body("wf-recorder", &e));
                }
            }
        }
    }
}

async fn stop_recording(
    recorder: &mut Option<Recorder>,
    started: &mut Option<Instant>,
    state: &Mutable<RecordingState>,
    saved: &Mutable<Option<SavedRecording>>,
) {
    let Some(running) = recorder.take() else {
        return; // not recording
    };
    let exit = match running {
        Recorder::Child(c) => stop_child(c).await,
        Recorder::Unit => stop_unit().await,
    };
    *started = None;
    finalize(state, saved, &exit).await;
}

/// SIGINT the child, give it [`STOP_GRACE`] to finalize, then SIGKILL it.
async fn stop_child(mut c: Child) -> Exit {
    interrupt(&c);
    match tokio::time::timeout(STOP_GRACE, c.wait()).await {
        Ok(Ok(status)) => {
            tracing::info!(status = ?status, "recorder: wf-recorder finalized");
            exit_of_status(status)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "recorder: waiting on wf-recorder failed");
            Exit::Unknown
        }
        Err(_) => {
            tracing::warn!("recorder: wf-recorder did not exit within grace after SIGINT; killing");
            let _ = c.kill().await;
            Exit::Failed(killed_after_grace())
        }
    }
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
    if let Err(e) = nix::sys::signal::kill(pid, Sig::SIGINT) {
        tracing::warn!(error = %e, "recorder: SIGINT to wf-recorder failed");
    }
}

/// The unit's version of [`stop_child`]: SIGINT through the user manager
/// (`KillUnit`), [`STOP_GRACE`] to finalize, then SIGKILL.
async fn stop_unit() -> Exit {
    if let Err(e) = bounded(systemd::kill_user_unit(
        RECORDER_UNIT,
        "main",
        Sig::SIGINT as i32,
    ))
    .await
    {
        tracing::warn!(error = %e, "recorder: SIGINT to the recorder unit failed");
    }
    if let Some(exit) = wait_unit_exit(STOP_GRACE).await {
        tracing::info!(?exit, "recorder: wf-recorder finalized");
        return exit;
    }
    tracing::warn!("recorder: the recorder unit did not exit within grace after SIGINT; killing");
    if let Err(e) = bounded(systemd::kill_user_unit(
        RECORDER_UNIT,
        "all",
        Sig::SIGKILL as i32,
    ))
    .await
    {
        tracing::warn!(error = %e, "recorder: SIGKILL to the recorder unit failed");
    }
    // Waiting here lets the poll read and reset the unit's `failed` state now,
    // rather than leave it to be reported again at the next start. Its own
    // detail ("killed by SIGKILL") gives way to the reason for the kill.
    if wait_unit_exit(KILL_GRACE).await.is_some() {
        Exit::Failed(killed_after_grace())
    } else {
        Exit::Failed(format!(
            "didn't stop when asked; check `systemctl --user status {RECORDER_UNIT}`"
        ))
    }
}

/// Re-read the recorder unit every [`STOP_POLL`] until it has exited, or
/// `None` once `budget` runs out.
async fn wait_unit_exit(budget: Duration) -> Option<Exit> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match tokio::time::timeout_at(deadline, poll_unit()).await {
            Ok(Ok(UnitPhase::Exited(exit))) => return Some(exit),
            Ok(Ok(UnitPhase::Live)) => {}
            Ok(Err(e)) => tracing::debug!(error = %e, "recorder: could not read the recorder unit"),
            Err(_) => return None,
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(STOP_POLL).await;
    }
}

/// Run one user-manager call for at most [`BUS_TIMEOUT`]. While the session
/// bus is down, `hytte_bus` retries a call once after waiting up to 5 s for a
/// reconnect. That suits a service, but here it would hold a Start (before
/// `slurp` opens) or a Stop that long on every call. Every caller already
/// tolerates a miss: the tick polls again, and a Start that cannot see the
/// manager simply launches.
async fn bounded<T>(
    call: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(BUS_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "the user manager did not answer within {} s",
                BUS_TIMEOUT.as_secs()
            ))
        })
}

/// Where the recorder unit is.
#[derive(Debug, PartialEq, Eq)]
enum UnitPhase {
    /// Still running, or still stopping.
    Live,
    /// It has exited, this way.
    Exited(Exit),
}

/// Read the recorder unit's state off the user manager. A failed unit's
/// reason is read and its failure reset, which frees the fixed name.
async fn poll_unit() -> anyhow::Result<UnitPhase> {
    let Some(unit) = bounded(systemd::user_unit(RECORDER_UNIT)).await? else {
        // Not loaded: a transient unit that exits cleanly is collected at once.
        return Ok(UnitPhase::Exited(Exit::Clean));
    };
    Ok(match unit_state(&unit.active_state) {
        UnitState::Live => UnitPhase::Live,
        UnitState::Gone => UnitPhase::Exited(Exit::Clean),
        UnitState::Failed => UnitPhase::Exited(Exit::Failed(collect_failure(&unit).await)),
    })
}

/// Why the failed recorder unit failed, read off its `Result` and
/// `ExecMainStatus`, after which its failure is reset so the name is free.
async fn collect_failure(unit: &systemd::UserUnit) -> String {
    let result = service_property(unit, "Result")
        .await
        .and_then(|v| String::try_from(v).ok());
    let status = service_property(unit, "ExecMainStatus")
        .await
        .and_then(|v| i32::try_from(v).ok());
    if let Err(e) = bounded(systemd::reset_failed_user_unit(RECORDER_UNIT)).await {
        tracing::warn!(error = %e, "recorder: could not reset the failed recorder unit");
    }
    unit_failure_detail(result.as_deref(), status)
}

/// One `org.freedesktop.systemd1.Service` property of `unit`, or `None` (with
/// a debug line) when it cannot be read.
async fn service_property(
    unit: &systemd::UserUnit,
    property: &str,
) -> Option<zbus::zvariant::OwnedValue> {
    bounded(systemd::user_unit_property(
        &unit.object_path,
        "org.freedesktop.systemd1.Service",
        property,
    ))
    .await
    .inspect_err(|e| tracing::debug!(error = %e, property, "recorder: unit property unreadable"))
    .ok()
}

/// Pick up a recorder unit this driver is not tracking, and say whether a
/// running recording was adopted. A unit that failed while no shell watched it
/// is reported and reset instead.
async fn adopt(
    recorder: &mut Option<Recorder>,
    started: &mut Option<Instant>,
    state: &Mutable<RecordingState>,
    saved: &Mutable<Option<SavedRecording>>,
) -> bool {
    let unit = match bounded(systemd::user_unit(RECORDER_UNIT)).await {
        Ok(Some(unit)) => unit,
        Ok(None) => return false,
        // No user manager (the direct-spawn fallback's world), or a bus
        // hiccup: there is no unit this driver could reach anyway.
        Err(e) => {
            tracing::debug!(error = %e, "recorder: could not look for a running recorder unit");
            return false;
        }
    };
    match adoption(&unit.active_state, &unit.description) {
        Adoption::Resume(path) => {
            let elapsed = active_for(&unit).await;
            *recorder = Some(Recorder::Unit);
            *started = Some(
                Instant::now()
                    .checked_sub(elapsed)
                    .unwrap_or_else(Instant::now),
            );
            state.set(RecordingState::recording(path.clone()).with_elapsed(elapsed.as_secs()));
            tracing::info!(
                path,
                elapsed_secs = elapsed.as_secs(),
                "recorder: adopted a running recording"
            );
            true
        }
        Adoption::Failed(path) => {
            let exit = Exit::Failed(collect_failure(&unit).await);
            report(path, &exit, saved).await;
            false
        }
        Adoption::Reset => {
            if let Err(e) = bounded(systemd::reset_failed_user_unit(RECORDER_UNIT)).await {
                tracing::warn!(error = %e, "recorder: could not reset the failed recorder unit");
            }
            false
        }
        Adoption::Foreign => {
            tracing::warn!(
                unit = RECORDER_UNIT,
                description = unit.description,
                "recorder: a unit by the recorder's name is running but names no recording; \
                 not adopting it",
            );
            false
        }
        Adoption::Nothing => false,
    }
}

/// How long `unit` has been active, from its `ActiveEnterTimestamp`; zero when
/// that cannot be read.
async fn active_for(unit: &systemd::UserUnit) -> Duration {
    let start_usec = bounded(systemd::user_unit_property(
        &unit.object_path,
        "org.freedesktop.systemd1.Unit",
        "ActiveEnterTimestamp",
    ))
    .await
    .ok()
    .and_then(|v| u64::try_from(v).ok())
    .unwrap_or(0);
    elapsed_since(SystemTime::now(), start_usec)
}

/// What `systemd-run` made of a recorder launch.
#[derive(Debug, PartialEq, Eq)]
enum Launch {
    /// The user manager took it: `wf-recorder` runs as [`RECORDER_UNIT`].
    Unit,
    /// Nothing was started (no `systemd-run`, or no user manager reachable),
    /// so a direct spawn starts the recorder exactly once. The text says why.
    NothingStarted(String),
    /// The manager answered and refused, or `wf-recorder` is not installed.
    /// Not worked around, since a second route could start a second recorder;
    /// the text is the start-failure toast's body.
    Refused(String),
}

/// Ask the user manager to run `wf-recorder <args>` as [`RECORDER_UNIT`],
/// writing `path`. Returns once the manager has taken the start job.
async fn launch_unit(path: &str, args: &[String]) -> Launch {
    let env = forwarded_env(|name| std::env::var(name).ok());
    let mut cmd = Command::new("systemd-run");
    cmd.args(unit_launch_args(path, args, &env))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        // Bounds only the short-lived helper. The unit it asked for is the
        // manager's, and is untouched by the helper dying.
        .kill_on_drop(true);
    match tokio::time::timeout(LAUNCH_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => Launch::Unit,
        Ok(Ok(out)) => classify_launch_failure(&String::from_utf8_lossy(&out.stderr)),
        Ok(Err(e)) => Launch::NothingStarted(format!("systemd-run could not be run: {e}")),
        // The manager may have taken the job by now, so a direct spawn could
        // start a second recorder. Track it as the unit: the next tick's poll
        // settles it, and a unit that never started reads as "exited, wrote
        // nothing", which is toasted as a failure.
        Err(_) => {
            tracing::warn!(
                "recorder: systemd-run did not answer within {}s; tracking {RECORDER_UNIT} anyway",
                LAUNCH_TIMEOUT.as_secs(),
            );
            Launch::Unit
        }
    }
}

/// Return to `Idle` and [`report`] how the recording ended. Reads the path
/// off the current `Recording` state before clearing it.
async fn finalize(
    state: &Mutable<RecordingState>,
    saved: &Mutable<Option<SavedRecording>>,
    exit: &Exit,
) {
    let path = match state.get_cloned() {
        RecordingState::Recording { path, .. } => Some(path),
        RecordingState::Idle => None,
    };
    state.set(RecordingState::Idle);
    if let Some(path) = path {
        report(path, exit, saved).await;
    }
}

/// Surface a finished recording once via `saved` (the "Recording saved"
/// toast) when it exited cleanly and left a non-empty file, and as a
/// "Recording failed" toast otherwise — see [`outcome`].
async fn report(path: String, exit: &Exit, saved: &Mutable<Option<SavedRecording>>) {
    match outcome(&path, exit, &file_state(&path).await) {
        Outcome::Saved => saved.set(Some(SavedRecording { path })),
        Outcome::Failed(body) => {
            tracing::warn!(path, ?exit, body, "recorder: recording failed");
            toast_recording_failure(&body);
        }
    }
}

/// What is at a recording's output path.
#[derive(Debug, PartialEq, Eq)]
enum FileState {
    /// A non-empty file.
    Written,
    /// A file with no bytes in it.
    Empty,
    /// Nothing.
    Missing,
    /// It could not be checked; the text is the error.
    Unreadable(String),
}

async fn file_state(path: &str) -> FileState {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() == 0 => FileState::Empty,
        Ok(_) => FileState::Written,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileState::Missing,
        Err(e) => FileState::Unreadable(e.to_string()),
    }
}

/// Run `slurp` to pick a region. `None` when the user cancelled (non-zero
/// exit / empty output) or `slurp` isn't installed.
async fn pick_region() -> Option<String> {
    let output = match Command::new("slurp").output().await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "recorder: failed to run slurp (is it installed?)");
            toast_start_failure(&spawn_error_body("slurp (the region picker)", &e));
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

/// Surface a recording-*start* failure as a **loud** critical toast on top of
/// the caller's `tracing::warn!` — the fix for #502.
///
/// Before this, a failed `slurp` / `wf-recorder` spawn (e.g. neither binary on
/// the `systemd --user` service's `PATH`) only logged, so the record chip read
/// to the user as "the button does nothing at all" — a silent dead end. Now
/// every start-path failure says out loud *why* it failed.
///
/// [`Urgency::Critical`](crate::notifications::Urgency::Critical) so the toast
/// bypasses Do-Not-Disturb and the own-app mute — a failure the user just
/// triggered by clicking the chip must not be swallowed (see
/// [`crate::notifications::post_local`]'s DND section). Safe to call from the
/// driver task on the hytte-tokio runtime: `post_local` only touches the
/// cross-thread notifications handle, and no-ops when the service is
/// unregistered (headless tests).
fn toast_start_failure(body: &str) {
    crate::notifications::post_local(
        "Screen recording",
        "Couldn't start recording",
        body,
        crate::notifications::Urgency::Critical,
    );
}

/// [`toast_start_failure`]'s sibling for a recording that started and then
/// ended badly: `wf-recorder` exited unsuccessfully, or left no file or an
/// empty one. Before this, every stop with *any* file on disk read "Recording
/// saved", a 0-byte file after a non-zero exit included. Critical for the same
/// reason: the user believes they have a recording.
fn toast_recording_failure(body: &str) {
    crate::notifications::post_local(
        "Screen recording",
        "Recording failed",
        body,
        crate::notifications::Urgency::Critical,
    );
}

// ── Pure helpers (unit-tested) ───────────────────────────────────────────────

/// How a finished recording is reported: whether it was saved, and if not,
/// the failure toast's body.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Clean exit, non-empty file: the "Recording saved" toast.
    Saved,
    /// Anything else: the "Recording failed" toast with this body.
    Failed(String),
}

/// Decide how a recording to `path` that ended with `exit` and left `file`
/// is reported. A recording counts as saved only when `wf-recorder` did not
/// report a failure **and** the file exists with at least one byte in it.
/// [`Exit::Unknown`] (a failed `wait`) is judged on the file alone, as before.
fn outcome(path: &str, exit: &Exit, file: &FileState) -> Outcome {
    match (exit, file) {
        (Exit::Clean | Exit::Unknown, FileState::Written) => Outcome::Saved,
        (Exit::Failed(why), FileState::Written) => {
            Outcome::Failed(format!("wf-recorder {why}, so {path} may be incomplete."))
        }
        (Exit::Failed(why), FileState::Empty | FileState::Missing) => {
            Outcome::Failed(format!("wf-recorder {why} and saved nothing."))
        }
        (Exit::Failed(why), FileState::Unreadable(e)) => Outcome::Failed(format!(
            "wf-recorder {why}, and {path} couldn't be checked: {e}"
        )),
        (Exit::Clean | Exit::Unknown, FileState::Empty) => {
            Outcome::Failed(format!("wf-recorder left {path} empty."))
        }
        (Exit::Clean | Exit::Unknown, FileState::Missing) => {
            Outcome::Failed(format!("wf-recorder didn't write {path}."))
        }
        (Exit::Clean | Exit::Unknown, FileState::Unreadable(e)) => {
            Outcome::Failed(format!("Couldn't check {path}: {e}"))
        }
    }
}

/// A child's exit status as an [`Exit`]: success, or what went wrong.
fn exit_of_status(status: std::process::ExitStatus) -> Exit {
    use std::os::unix::process::ExitStatusExt;
    if status.success() {
        Exit::Clean
    } else if let Some(code) = status.code() {
        Exit::Failed(exit_code_detail(code))
    } else if let Some(signo) = status.signal() {
        Exit::Failed(signal_detail(signo))
    } else {
        Exit::Failed(format!("ended unexpectedly ({status})"))
    }
}

/// A failed unit's reason, from its `Result` and `ExecMainStatus` (the exit
/// code for `exit-code`, the signal number for `signal`/`core-dump`).
fn unit_failure_detail(result: Option<&str>, status: Option<i32>) -> String {
    match (result, status) {
        (Some("exit-code"), Some(code)) => exit_code_detail(code),
        (Some("signal" | "core-dump"), Some(signo)) => signal_detail(signo),
        (Some(result), _) => format!("failed ({result})"),
        (None, _) => "failed".to_owned(),
    }
}

fn exit_code_detail(code: i32) -> String {
    format!("exited with status {code}")
}

fn signal_detail(signo: i32) -> String {
    match Sig::try_from(signo) {
        Ok(sig) => format!("was killed by {}", sig.as_str()),
        Err(_) => format!("was killed by signal {signo}"),
    }
}

/// The [`Exit::Failed`] reason for a recorder that ignored SIGINT for the
/// whole [`STOP_GRACE`] and was killed, which leaves an `.mp4` without its
/// index.
fn killed_after_grace() -> String {
    format!(
        "didn't finish within {} s of being stopped and was killed",
        STOP_GRACE.as_secs()
    )
}

/// The three states of a loaded recorder unit that the driver tells apart.
#[derive(Debug, PartialEq, Eq)]
enum UnitState {
    /// Running, starting or stopping.
    Live,
    /// `inactive`: exited cleanly, not yet collected.
    Gone,
    /// `failed`: exited unsuccessfully, kept loaded because the unit has no
    /// `--collect`.
    Failed,
}

/// Classify a systemd `ActiveState`. Anything that is neither `inactive` nor
/// `failed` (`active`, `activating`, `deactivating`, `reloading`, …) still has
/// a process to wait for.
fn unit_state(active_state: &str) -> UnitState {
    match active_state {
        "inactive" => UnitState::Gone,
        "failed" => UnitState::Failed,
        _ => UnitState::Live,
    }
}

/// What a recorder unit the driver is not tracking means.
#[derive(Debug, PartialEq, Eq)]
enum Adoption {
    /// A recording to this path is running: show it, and let Stop reach it.
    Resume(String),
    /// A recording to this path failed while no shell was watching: report
    /// it, then reset the unit.
    Failed(String),
    /// A failed unit that names no recording: reset it so the name is free.
    Reset,
    /// A live unit that names no recording: not ours, left alone.
    Foreign,
    /// It already exited cleanly.
    Nothing,
}

/// Decide what to do with a loaded [`RECORDER_UNIT`] from its `ActiveState`
/// and `Description=`.
fn adoption(active_state: &str, description: &str) -> Adoption {
    match (unit_state(active_state), recording_path(description)) {
        (UnitState::Live, Some(path)) => Adoption::Resume(path.to_owned()),
        (UnitState::Live, None) => Adoption::Foreign,
        (UnitState::Failed, Some(path)) => Adoption::Failed(path.to_owned()),
        (UnitState::Failed, None) => Adoption::Reset,
        (UnitState::Gone, _) => Adoption::Nothing,
    }
}

/// The recorder unit's `Description=` for a recording to `path`.
fn unit_description(path: &str) -> String {
    format!("{DESCRIPTION_PREFIX}{path}")
}

/// The output path a [`unit_description`] names, `None` for any other
/// description.
fn recording_path(description: &str) -> Option<&str> {
    description
        .strip_prefix(DESCRIPTION_PREFIX)
        .filter(|path| !path.is_empty())
}

/// The time from systemd's `ActiveEnterTimestamp` (microseconds since the
/// epoch, `0` for "never") to `now`. Zero when the timestamp is unset or
/// later than `now`.
fn elapsed_since(now: SystemTime, start_usec: u64) -> Duration {
    if start_usec == 0 {
        return Duration::ZERO;
    }
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_micros(start_usec))
        .and_then(|start| now.duration_since(start).ok())
        .unwrap_or_default()
}

/// [`FORWARDED_ENV`] read through `lookup`, skipping anything unset or empty
/// so a launch never puts an empty value over the user manager's own. In
/// [`FORWARDED_ENV`]'s order, so the argv is deterministic.
fn forwarded_env(lookup: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    FORWARDED_ENV
        .iter()
        .filter_map(|name| {
            let value = lookup(name)?;
            (!value.is_empty()).then(|| ((*name).to_owned(), value))
        })
        .collect()
}

/// `systemd-run`'s argv for a recording to `path`, where `args` is
/// `wf-recorder`'s own ([`recorder_args`]) and `env` the variables to forward.
///
/// The flags follow the plugin launcher's (`trollshell/src/launch.rs`), minus
/// `--collect` (see the module doc). The property name and the variable
/// names are constants. What varies sits only in value position: the path in
/// `--description=` and the values in `--setenv=`. `--` ends option parsing
/// before the program. `systemd-run` resolves `wf-recorder` against its own
/// `PATH`, which is the shell's.
fn unit_launch_args(path: &str, args: &[String], env: &[(String, String)]) -> Vec<String> {
    let mut cmdline = vec![
        "--user".to_owned(),
        "--quiet".to_owned(),
        format!("--unit={RECORDER_UNIT}"),
        format!("--description={}", unit_description(path)),
        format!("--property=PartOf={SESSION_TARGET}"),
    ];
    cmdline.extend(env.iter().map(|(k, v)| format!("--setenv={k}={v}")));
    cmdline.push("--".to_owned());
    cmdline.push("wf-recorder".to_owned());
    cmdline.extend(args.iter().cloned());
    cmdline
}

/// Classify a `systemd-run` that exited non-zero, by its stderr. Its exit
/// status is `1` for every failure (measured on systemd 260), so the text is
/// the only signal there is. That is the detached `RunCommand` launcher's
/// predicate (`classify_systemd_run_failure`), plus `wf-recorder` missing
/// from `PATH`, which `systemd-run` reports itself because it resolves the
/// program before asking the manager.
fn classify_launch_failure(stderr: &str) -> Launch {
    let stderr = stderr.trim();
    if stderr.contains("Failed to connect to") && stderr.contains("bus") {
        Launch::NothingStarted(format!(
            "systemd-run --user could not reach a user manager: {stderr}"
        ))
    } else if stderr.contains("Failed to find executable") {
        Launch::Refused(spawn_error_body(
            "wf-recorder",
            &std::io::Error::from(std::io::ErrorKind::NotFound),
        ))
    } else {
        Launch::Refused(format!("systemd-run couldn't start the recorder: {stderr}"))
    }
}

/// A human-readable reason an external tool failed to launch, for the #502
/// failure toast. `NotFound` (the overwhelmingly likely case — the tool isn't
/// installed or isn't on the service's `PATH`) gets a plain, actionable line
/// naming the tool; any other spawn error carries the raw `io::Error`.
fn spawn_error_body(tool: &str, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("{tool} isn't installed or isn't on PATH.")
    } else {
        format!("Couldn't launch {tool}: {e}")
    }
}

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

    // ── spawn_error_body (#502 loud failure) ─────────────────────────────────

    #[test]
    fn spawn_error_body_not_found_names_tool_and_path() {
        // The #502 case: the tool isn't installed / on PATH → a plain,
        // actionable line rather than a bare "No such file or directory".
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            spawn_error_body("slurp", &e),
            "slurp isn't installed or isn't on PATH.",
        );
    }

    #[test]
    fn spawn_error_body_other_error_carries_raw_message() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(
            spawn_error_body("wf-recorder", &e),
            "Couldn't launch wf-recorder: denied",
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

    // ── outcome: how a finished recording is reported ────────────────────────

    const OUT: &str = "/v/clip.mp4";

    fn failed(why: &str) -> Exit {
        Exit::Failed(why.to_owned())
    }

    #[test]
    fn only_a_clean_exit_with_a_non_empty_file_is_saved() {
        assert_eq!(
            outcome(OUT, &Exit::Clean, &FileState::Written),
            Outcome::Saved
        );
        // A failed `wait` says nothing about the recording, so the file decides,
        // as it did before.
        assert_eq!(
            outcome(OUT, &Exit::Unknown, &FileState::Written),
            Outcome::Saved
        );
    }

    #[test]
    fn a_non_zero_exit_with_an_empty_file_is_a_failure_not_saved() {
        // The reported case: before this, any file on disk read "saved".
        assert_eq!(
            outcome(OUT, &failed("exited with status 1"), &FileState::Empty),
            Outcome::Failed("wf-recorder exited with status 1 and saved nothing.".to_owned()),
        );
        assert_eq!(
            outcome(OUT, &failed("exited with status 1"), &FileState::Missing),
            Outcome::Failed("wf-recorder exited with status 1 and saved nothing.".to_owned()),
        );
    }

    #[test]
    fn a_failed_exit_that_left_bytes_is_still_a_failure() {
        assert_eq!(
            outcome(OUT, &failed("was killed by SIGKILL"), &FileState::Written),
            Outcome::Failed(
                "wf-recorder was killed by SIGKILL, so /v/clip.mp4 may be incomplete.".to_owned()
            ),
        );
        assert_eq!(
            outcome(
                OUT,
                &failed("failed (timeout)"),
                &FileState::Unreadable("EACCES".into())
            ),
            Outcome::Failed(
                "wf-recorder failed (timeout), and /v/clip.mp4 couldn't be checked: EACCES"
                    .to_owned()
            ),
        );
    }

    #[test]
    fn a_clean_exit_without_a_non_empty_file_is_a_failure() {
        assert_eq!(
            outcome(OUT, &Exit::Clean, &FileState::Empty),
            Outcome::Failed("wf-recorder left /v/clip.mp4 empty.".to_owned()),
        );
        assert_eq!(
            outcome(OUT, &Exit::Clean, &FileState::Missing),
            Outcome::Failed("wf-recorder didn't write /v/clip.mp4.".to_owned()),
        );
        assert_eq!(
            outcome(OUT, &Exit::Unknown, &FileState::Unreadable("EIO".into())),
            Outcome::Failed("Couldn't check /v/clip.mp4: EIO".to_owned()),
        );
    }

    #[tokio::test]
    async fn file_state_tells_missing_empty_and_written_apart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = |name: &str| dir.path().join(name).to_string_lossy().into_owned();

        assert_eq!(file_state(&path("absent.mp4")).await, FileState::Missing);
        std::fs::write(path("empty.mp4"), b"").expect("write");
        assert_eq!(file_state(&path("empty.mp4")).await, FileState::Empty);
        std::fs::write(path("clip.mp4"), b"ftyp").expect("write");
        assert_eq!(file_state(&path("clip.mp4")).await, FileState::Written);
    }

    // ── Exit: a child's status, a unit's Result ──────────────────────────────

    #[test]
    fn exit_of_status_reads_success_code_and_signal() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        assert_eq!(exit_of_status(ExitStatus::from_raw(0)), Exit::Clean);
        // wait(2) encoding: the exit code sits in the second byte.
        assert_eq!(
            exit_of_status(ExitStatus::from_raw(1 << 8)),
            failed("exited with status 1")
        );
        assert_eq!(
            exit_of_status(ExitStatus::from_raw(9)),
            failed("was killed by SIGKILL")
        );
    }

    #[test]
    fn unit_failure_detail_reads_result_and_status() {
        assert_eq!(
            unit_failure_detail(Some("exit-code"), Some(3)),
            "exited with status 3"
        );
        assert_eq!(
            unit_failure_detail(Some("signal"), Some(9)),
            "was killed by SIGKILL"
        );
        assert_eq!(
            unit_failure_detail(Some("core-dump"), Some(11)),
            "was killed by SIGSEGV"
        );
        assert_eq!(
            unit_failure_detail(Some("signal"), Some(4242)),
            "was killed by signal 4242"
        );
        assert_eq!(
            unit_failure_detail(Some("timeout"), Some(0)),
            "failed (timeout)"
        );
        // The properties could not be read at all.
        assert_eq!(unit_failure_detail(None, None), "failed");
        assert_eq!(
            unit_failure_detail(Some("exit-code"), None),
            "failed (exit-code)"
        );
    }

    #[test]
    fn killed_after_grace_names_the_grace() {
        assert_eq!(
            killed_after_grace(),
            "didn't finish within 5 s of being stopped and was killed"
        );
    }

    // ── The recorder unit: state, adoption ───────────────────────────────────

    #[test]
    fn unit_state_waits_on_everything_but_inactive_and_failed() {
        assert_eq!(unit_state("inactive"), UnitState::Gone);
        assert_eq!(unit_state("failed"), UnitState::Failed);
        for live in [
            "active",
            "activating",
            "deactivating",
            "reloading",
            "refreshing",
        ] {
            assert_eq!(unit_state(live), UnitState::Live, "{live}");
        }
    }

    #[test]
    fn a_running_recorder_unit_is_adopted_with_its_path() {
        let desc = unit_description("/home/u/Videos/trollshell-recording-1.mp4");
        assert_eq!(
            adoption("active", &desc),
            Adoption::Resume("/home/u/Videos/trollshell-recording-1.mp4".to_owned()),
        );
        // Stopping still has a process to wait for, so Stop must reach it.
        assert_eq!(
            adoption("deactivating", &desc),
            Adoption::Resume("/home/u/Videos/trollshell-recording-1.mp4".to_owned()),
        );
    }

    #[test]
    fn a_failed_recorder_unit_is_reported_then_reset() {
        assert_eq!(
            adoption("failed", &unit_description("/v/a.mp4")),
            Adoption::Failed("/v/a.mp4".to_owned()),
        );
        // No path to report: only free the name.
        assert_eq!(adoption("failed", "something else"), Adoption::Reset);
    }

    #[test]
    fn a_live_unit_that_names_no_recording_is_not_adopted() {
        assert_eq!(adoption("active", "something else"), Adoption::Foreign);
        assert_eq!(adoption("active", DESCRIPTION_PREFIX), Adoption::Foreign);
    }

    #[test]
    fn a_cleanly_exited_unit_is_left_alone() {
        assert_eq!(
            adoption("inactive", &unit_description("/v/a.mp4")),
            Adoption::Nothing
        );
    }

    #[test]
    fn the_description_round_trips_the_path_verbatim() {
        for path in [
            "/v/a.mp4",
            "/v/with space/a.mp4",
            "/v/%h-not-a-specifier/a.mp4",
            "/v/--unit=x.mp4",
        ] {
            assert_eq!(recording_path(&unit_description(path)), Some(path));
        }
        assert_eq!(recording_path("trollshell plugin launch: x #1"), None);
        assert_eq!(recording_path(""), None);
    }

    #[test]
    fn elapsed_since_counts_from_the_active_enter_timestamp() {
        let start_usec = 1_800_000_000_000_000_u64;
        let now = SystemTime::UNIX_EPOCH
            + Duration::from_micros(start_usec)
            + Duration::from_millis(65_500);
        assert_eq!(
            elapsed_since(now, start_usec),
            Duration::from_millis(65_500)
        );
        // systemd's "never" is 0, not the epoch.
        assert_eq!(elapsed_since(now, 0), Duration::ZERO);
        // A start after `now` (a clock step) is zero, not a panic.
        assert_eq!(elapsed_since(now, start_usec + 100_000_000), Duration::ZERO);
        assert_eq!(elapsed_since(now, u64::MAX), Duration::ZERO);
    }

    // ── The systemd-run invocation ───────────────────────────────────────────

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn unit_launch_args_pin_the_invocation() {
        let args = recorder_args("/v/a.mp4", Some("0,0 640x480"), true);
        assert_eq!(
            unit_launch_args(
                "/v/a.mp4",
                &args,
                &env(&[
                    ("WAYLAND_DISPLAY", "wayland-1"),
                    ("XDG_RUNTIME_DIR", "/run/user/1000")
                ]),
            ),
            vec![
                "--user",
                "--quiet",
                "--unit=trollshell-recorder.service",
                "--description=trollshell screen recording: /v/a.mp4",
                "--property=PartOf=graphical-session.target",
                "--setenv=WAYLAND_DISPLAY=wayland-1",
                "--setenv=XDG_RUNTIME_DIR=/run/user/1000",
                "--",
                "wf-recorder",
                "-f",
                "/v/a.mp4",
                "-g",
                "0,0 640x480",
                "--audio",
            ],
        );
    }

    #[test]
    fn unit_launch_args_never_collect_the_unit() {
        // A failed recorder unit must stay loaded so its `Result` can be read.
        let cmdline = unit_launch_args("/v/a.mp4", &[], &[]);
        assert!(!cmdline.iter().any(|a| a == "--collect"), "{cmdline:?}");
    }

    #[test]
    fn a_flag_shaped_path_stays_in_value_position() {
        let path = "/v/ --unit=evil.service";
        let args = recorder_args(path, None, false);
        let cmdline = unit_launch_args(path, &args, &[]);
        let sep = cmdline.iter().position(|a| a == "--").expect("separated");
        // Before `--`, the path only ever appears inside the description.
        assert_eq!(
            cmdline[..sep]
                .iter()
                .filter(|a| a.starts_with("--unit="))
                .collect::<Vec<_>>(),
            vec!["--unit=trollshell-recorder.service"],
        );
        // After it, the program and wf-recorder's own argv, unchanged.
        assert_eq!(cmdline[sep + 1], "wf-recorder");
        assert_eq!(cmdline[sep + 2..], args[..]);
    }

    #[test]
    fn forwarded_env_skips_unset_and_empty_in_a_fixed_order() {
        let lookup = |name: &str| match name {
            "XDG_RUNTIME_DIR" => Some("/run/user/1000".to_owned()),
            "WAYLAND_DISPLAY" => Some(String::new()),
            _ => Some("never asked".to_owned()),
        };
        assert_eq!(
            forwarded_env(lookup),
            env(&[("XDG_RUNTIME_DIR", "/run/user/1000")])
        );
        assert_eq!(
            forwarded_env(|name| Some(format!("{name}-v"))),
            env(&[
                ("WAYLAND_DISPLAY", "WAYLAND_DISPLAY-v"),
                ("XDG_RUNTIME_DIR", "XDG_RUNTIME_DIR-v")
            ]),
        );
        assert!(forwarded_env(|_| None).is_empty());
    }

    #[test]
    fn no_user_manager_falls_back_to_a_direct_spawn() {
        // systemd 260's wording, and an older one: the family is matched.
        for stderr in [
            "Failed to connect to user scope bus via local transport: \
             $DBUS_SESSION_BUS_ADDRESS and $XDG_RUNTIME_DIR not defined",
            "Failed to connect to bus: No medium found\n",
        ] {
            assert!(
                matches!(classify_launch_failure(stderr), Launch::NothingStarted(_)),
                "{stderr}"
            );
        }
    }

    #[test]
    fn a_missing_wf_recorder_is_refused_with_the_not_installed_line() {
        // Measured on systemd 260.5, which resolves the program client-side.
        assert_eq!(
            classify_launch_failure(
                "Failed to find executable wf-recorder: No such file or directory\n"
            ),
            Launch::Refused("wf-recorder isn't installed or isn't on PATH.".to_owned()),
        );
    }

    #[test]
    fn a_manager_refusal_is_not_worked_around() {
        // A taken name: a direct spawn here could start a second recorder.
        assert_eq!(
            classify_launch_failure(
                "Failed to start transient service unit: Unit trollshell-recorder.service \
                 was already loaded or has a fragment file.\n"
            ),
            Launch::Refused(
                "systemd-run couldn't start the recorder: Failed to start transient service \
                 unit: Unit trollshell-recorder.service was already loaded or has a fragment \
                 file."
                    .to_owned()
            ),
        );
    }
}
