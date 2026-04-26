//! `org.freedesktop.ScreenSaver` D-Bus service + screen-lock dispatch.
//!
//! Implements the freedesktop `ScreenSaver` wire shape on the session bus so
//! third-party apps (Firefox, Chromium, mpv, video calls, …) can:
//!
//!   * `Inhibit` / `UnInhibit` the screensaver while playing fullscreen
//!     video, sharing the screen, or doing anything where an automatic lock
//!     would be ruinous.
//!   * `Lock()` programmatically (e.g. `gnome-screensaver-command --lock`).
//!   * Query active state (stubbed in v1 — see method docs).
//!
//! The lock binary is `gtklock` by default, overridable via the
//! `TROLL_LOCK_CMD` env var (set it to a single shell-style command, parsed
//! with naive whitespace splitting; quoting is not supported — use a wrapper
//! script if you need it).
//!
//! While at least one inhibitor is active, swayidle is paused via
//! `SIGSTOP`; when the last inhibitor releases, we send `SIGCONT`. swayidle
//! upstream has no built-in pause/resume signal contract — `SIGUSR1` in
//! recent versions actually *triggers* an idle event, which is exactly the
//! opposite of what we want. STOP/CONT works regardless of swayidle's
//! internal state machine but is necessarily a process-level halt: any
//! pending `before-sleep` callback in flight when STOP arrives will not
//! complete until CONT. In practice swayidle is usually parked on its
//! event loop, so this is fine.
//!
//! The integration assumes swayidle is the systemd `swayidle.service` user
//! unit shipped at `etc/systemd/user/swayidle.service`; PID discovery goes
//! through `systemctl --user show -p MainPID --value swayidle.service`. If
//! the user runs swayidle outside the unit (e.g. forked by a hand-written
//! niri spawn-at-startup), pause/resume becomes a no-op and inhibitors are
//! tracked but not enforced. A future task can add a fallback to `pidof
//! swayidle`.
//!
//! # Public API
//!
//! ```ignore
//! // Register once at startup:
//! .with(screensaver::service())
//!
//! // From a power-menu / keybind:
//! screensaver::lock();
//!
//! // Programmatic inhibit (rare — apps usually do this themselves over D-Bus):
//! let cookie = screensaver::inhibit("trollshell", "presentation mode");
//! // … later …
//! screensaver::uninhibit(cookie);
//!
//! // Subscribe in widgets (e.g. a "what's keeping me awake" drawer page):
//! screensaver::inhibitors() -> impl Signal<Item = Vec<Inhibitor>>
//! ```

use anyhow::{anyhow, Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_reactive::{registry, runtime, Service};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::{fdo, Connection};

// ── Public data shapes ────────────────────────────────────────────────────────

/// One live screensaver inhibitor — what app asked to stay awake and why.
/// Surfaced via [`inhibitors()`] so a future drawer page can show the user
/// what's preventing automatic lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inhibitor {
    /// Cookie returned to the caller from `Inhibit`. Apps pass it back to
    /// `UnInhibit` to release.
    pub cookie: u32,
    /// Caller-supplied app name, e.g. "Firefox" or "mpv".
    pub application: String,
    /// Caller-supplied reason, e.g. "Playing video" or "Screen sharing".
    pub reason: String,
}

// ── Service handle ────────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct ScreenSaverHandles {
    /// Live inhibitor list, keyed by cookie. Wrapped in a sync Mutex so
    /// both the public `inhibit/uninhibit` helpers and the D-Bus interface
    /// methods (which run on the tokio scheduler) can mutate without
    /// `.await`-ing across an async barrier.
    pub(crate) state: Arc<Mutex<HashMap<u32, Inhibitor>>>,
    /// Reactive view of `state` for UI subscribers. Kept in sync after
    /// every mutation by [`publish_inhibitors`].
    pub(crate) inhibitors: Mutable<Vec<Inhibitor>>,
    /// Monotonic cookie counter. u32 is plenty: even at one inhibit per
    /// second we'd take ~136 years to wrap, and apps that leak cookies
    /// will hit the `HashMap` memory limit long before.
    pub(crate) next_cookie: Arc<AtomicU32>,
    /// `true` while the native lock UI is mounted on all monitors, `false`
    /// otherwise. Driven by [`handle_unlock_success`] and by the Task 3
    /// `lock()` rewrite. Subscribers: `widgets::lock_screen`.
    pub(crate) is_locked: Mutable<bool>,
}

impl Default for ScreenSaverHandles {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
            inhibitors: Mutable::new(Vec::new()),
            // Start at 1 so a leaked default-zero cookie never matches.
            next_cookie: Arc::new(AtomicU32::new(1)),
            is_locked: Mutable::new(false),
        }
    }
}

// ── Service marker ────────────────────────────────────────────────────────────

pub struct ScreenSaverService;

impl Service for ScreenSaverService {
    type Handles = ScreenSaverHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = ScreenSaverHandles::default();
        let state = handles.state.clone();
        let inhibitors_view = handles.inhibitors.clone();
        let next_cookie = handles.next_cookie.clone();

        rt.spawn(async move {
            loop {
                if let Err(e) =
                    run_server(state.clone(), inhibitors_view.clone(), next_cookie.clone()).await
                {
                    tracing::warn!(error = %e, "ScreenSaver server failed, retrying in 5s");
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        handles
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the screensaver service to register with the hytte runtime.
#[must_use]
pub fn service() -> ScreenSaverService {
    ScreenSaverService
}

/// Live inhibitors, keyed by cookie. UI subscribers can render this as a
/// "what's keeping the system awake" list.
pub fn inhibitors() -> impl Signal<Item = Vec<Inhibitor>> {
    registry::with(|r| {
        r.get::<ScreenSaverHandles>()
            .expect("screensaver::service() not registered")
            .inhibitors
            .signal_cloned()
    })
}

/// Signal emitting `true` while the lock UI is mounted, `false`
/// otherwise. Subscribed by `widgets::lock_screen` to drive the
/// per-monitor surfaces.
pub fn is_locked() -> impl Signal<Item = bool> {
    registry::with(|r| {
        r.get::<ScreenSaverHandles>()
            .expect("screensaver::service() not registered")
            .is_locked
            .signal_cloned()
    })
}

/// Called by the lock UI after a successful PAM authentication.
/// Flips `is_locked` to false (which clears the lock surfaces) and
/// tells logind to release its session-level lock state via
/// `Session.SetLockedHint(false)`.
pub fn handle_unlock_success() {
    let handles = registry::with(|r| {
        r.get::<ScreenSaverHandles>().map(|h| h.is_locked.clone())
    });
    if let Some(locked) = handles {
        locked.set(false);
    }
    runtime::handle().spawn(async move {
        if let Err(e) = call_login1_unlock().await {
            tracing::warn!(error = %e, "login1 SetLockedHint(false) failed");
        }
    });
}

async fn call_login1_unlock() -> anyhow::Result<()> {
    use anyhow::Context;
    let conn = Connection::system().await.context("connect system bus")?;
    let manager = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    .context("login1 Manager proxy")?;
    let pid: u32 = std::process::id();
    let session_path: zbus::zvariant::OwnedObjectPath = manager
        .call("GetSessionByPID", &(pid,))
        .await
        .context("GetSessionByPID")?;
    let session = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        session_path.as_str(),
        "org.freedesktop.login1.Session",
    )
    .await
    .context("login1 Session proxy")?;
    session
        .call::<_, _, ()>("SetLockedHint", &(false,))
        .await
        .context("Session.SetLockedHint(false)")?;
    Ok(())
}

/// Spawn the lock binary (`$TROLL_LOCK_CMD` or `gtklock`). Idempotent: if
/// the locker is already running, the second instance typically detects
/// the existing one and exits cleanly (gtklock does this; waylock does
/// too). Best-effort — failures are logged, not surfaced.
pub fn lock() {
    if let Err(e) = spawn_locker() {
        tracing::warn!(error = %e, "screensaver::lock failed to spawn lock binary");
    }
}

/// Programmatically register an inhibitor. Returns the cookie; the caller
/// must pass it back to [`uninhibit`] to release.
///
/// Internal helper for trollshell modules that want to inhibit (e.g. a
/// future "do-not-disturb while presenting" toggle). External apps go
/// through D-Bus.
#[must_use]
pub fn inhibit(application: &str, reason: &str) -> u32 {
    let Some(handles) = registry::with(|r| {
        r.get::<ScreenSaverHandles>()
            .map(|h| (h.state.clone(), h.inhibitors.clone(), h.next_cookie.clone()))
    }) else {
        // Service not registered (test harness?): return a sentinel so the
        // caller can still "release" without panicking.
        return 0;
    };
    let (state, view, counter) = handles;
    let cookie = counter.fetch_add(1, Ordering::Relaxed);
    let was_empty = insert_inhibitor(
        &state,
        Inhibitor {
            cookie,
            application: application.to_string(),
            reason: reason.to_string(),
        },
    );
    publish_inhibitors(&state, &view);
    if was_empty {
        spawn_pause_swayidle();
    }
    cookie
}

/// Release a cookie returned from [`inhibit`]. Unknown cookies are
/// silently ignored — apps regularly double-call `UnInhibit` on shutdown.
pub fn uninhibit(cookie: u32) {
    let Some(handles) = registry::with(|r| {
        r.get::<ScreenSaverHandles>()
            .map(|h| (h.state.clone(), h.inhibitors.clone()))
    }) else {
        return;
    };
    let (state, view) = handles;
    let became_empty = remove_inhibitor(&state, cookie);
    publish_inhibitors(&state, &view);
    if became_empty {
        spawn_resume_swayidle();
    }
}

// ── Internal: inhibitor map mutation ──────────────────────────────────────────

/// Returns `true` if the map went from empty → non-empty (i.e. this is the
/// transition where we should pause swayidle).
fn insert_inhibitor(state: &Mutex<HashMap<u32, Inhibitor>>, inh: Inhibitor) -> bool {
    let mut map = state.lock().expect("screensaver state poisoned");
    let was_empty = map.is_empty();
    map.insert(inh.cookie, inh);
    was_empty
}

/// Returns `true` if the map went from non-empty → empty (i.e. this is
/// the transition where we should resume swayidle). If the cookie wasn't
/// present, returns `false`.
fn remove_inhibitor(state: &Mutex<HashMap<u32, Inhibitor>>, cookie: u32) -> bool {
    let mut map = state.lock().expect("screensaver state poisoned");
    if map.remove(&cookie).is_none() {
        return false;
    }
    map.is_empty()
}

/// Snapshot the inhibitor map into the reactive `Mutable<Vec<_>>` for UI
/// consumers. Sorted by cookie so re-renders are stable.
fn publish_inhibitors(state: &Mutex<HashMap<u32, Inhibitor>>, view: &Mutable<Vec<Inhibitor>>) {
    let snapshot: Vec<Inhibitor> = {
        let map = state.lock().expect("screensaver state poisoned");
        let mut v: Vec<Inhibitor> = map.values().cloned().collect();
        v.sort_by_key(|i| i.cookie);
        v
    };
    view.set(snapshot);
}

// ── Internal: lock-binary dispatch ────────────────────────────────────────────

/// Resolve the lock command from `$TROLL_LOCK_CMD`, falling back to
/// `gtklock`. Splits on ASCII whitespace; no shell quoting. Empty or
/// whitespace-only env values fall back to the default.
fn resolve_lock_cmd() -> Vec<String> {
    if let Ok(raw) = std::env::var("TROLL_LOCK_CMD") {
        let parts: Vec<String> = raw.split_whitespace().map(str::to_string).collect();
        if !parts.is_empty() {
            return parts;
        }
    }
    vec!["gtklock".to_string()]
}

fn spawn_locker() -> Result<()> {
    let cmd = resolve_lock_cmd();
    let (program, args) = cmd.split_first().context("empty lock command")?;
    tracing::info!(program = %program, args = ?args, "spawning lock binary");
    // Detach: synchronous std spawn, no wait. The OS reaps when the locker
    // exits via SIGCHLD into trollshell's default handler (zombies are
    // cheap and rare here — at most one outstanding per lock cycle).
    let mut command = std::process::Command::new(program);
    command.args(args);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    command
        .spawn()
        .with_context(|| format!("spawn {program}"))?;
    Ok(())
}

// ── Internal: swayidle pause / resume ─────────────────────────────────────────

/// Resolve the swayidle PID by asking systemd-user for the unit's `MainPID`.
///
/// Returns `None` if the unit isn't loaded, isn't running, or `systemctl`
/// itself isn't available. The caller logs and continues — pause/resume
/// is best-effort, not load-bearing.
async fn swayidle_pid() -> Option<i32> {
    let out = tokio::process::Command::new("systemctl")
        .args(["--user", "show", "-p", "MainPID", "--value", "swayidle.service"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let pid: i32 = s.trim().parse().ok()?;
    if pid <= 0 {
        return None;
    }
    Some(pid)
}

fn spawn_pause_swayidle() {
    runtime::handle().spawn(async {
        if let Err(e) = signal_swayidle(nix::sys::signal::Signal::SIGSTOP).await {
            tracing::debug!(error = %e, "could not pause swayidle (proceeding without)");
        }
    });
}

fn spawn_resume_swayidle() {
    runtime::handle().spawn(async {
        if let Err(e) = signal_swayidle(nix::sys::signal::Signal::SIGCONT).await {
            tracing::debug!(error = %e, "could not resume swayidle (proceeding without)");
        }
    });
}

async fn signal_swayidle(sig: nix::sys::signal::Signal) -> Result<()> {
    let pid = swayidle_pid()
        .await
        .ok_or_else(|| anyhow!("swayidle.service has no MainPID"))?;
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), sig)
        .with_context(|| format!("kill({pid}, {sig:?})"))?;
    tracing::debug!(pid, ?sig, "signalled swayidle");
    Ok(())
}

// ── D-Bus interface ───────────────────────────────────────────────────────────

const PATH_CANONICAL: &str = "/org/freedesktop/ScreenSaver";
/// Bare path some apps (notably Firefox / Chromium historically) query due
/// to a long-standing GNOME bug. Mounting both at the same server matches
/// what gnome-screensaver and xdg-screensaver do in the wild.
const PATH_LEGACY: &str = "/ScreenSaver";

/// Server interface implementation. Cloned (cheaply, via Arc) when mounted
/// at the second path so both objects share the same inhibitor map.
#[derive(Clone)]
struct ScreenSaverIface {
    state: Arc<Mutex<HashMap<u32, Inhibitor>>>,
    inhibitors: Mutable<Vec<Inhibitor>>,
    next_cookie: Arc<AtomicU32>,
}

#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaverIface {
    /// Lock the screen now. Spawns the configured lock binary; returns
    /// immediately. Apps and `gnome-screensaver-command --lock` use this.
    #[allow(clippy::unused_async, clippy::unused_self)]
    async fn lock(&self) {
        if let Err(e) = spawn_locker() {
            tracing::warn!(error = %e, "Lock(): spawn_locker failed");
        }
    }

    /// Register an inhibitor. Returns a cookie the app must keep + pass
    /// back to `UnInhibit`. Pauses swayidle on the empty → non-empty
    /// transition.
    #[allow(clippy::unused_async)]
    async fn inhibit(&self, application_name: String, reason_for_inhibit: String) -> u32 {
        let cookie = self.next_cookie.fetch_add(1, Ordering::Relaxed);
        let inh = Inhibitor {
            cookie,
            application: application_name,
            reason: reason_for_inhibit,
        };
        tracing::debug!(cookie, app = %inh.application, reason = %inh.reason, "Inhibit");
        let was_empty = insert_inhibitor(&self.state, inh);
        publish_inhibitors(&self.state, &self.inhibitors);
        if was_empty {
            spawn_pause_swayidle();
        }
        cookie
    }

    /// Release an inhibitor. Resumes swayidle on the non-empty → empty
    /// transition. Unknown cookies are silently ignored (apps double-call
    /// `UnInhibit` on shutdown).
    #[allow(clippy::unused_async)]
    async fn un_inhibit(&self, cookie: u32) {
        tracing::debug!(cookie, "UnInhibit");
        let became_empty = remove_inhibitor(&self.state, cookie);
        publish_inhibitors(&self.state, &self.inhibitors);
        if became_empty {
            spawn_resume_swayidle();
        }
    }

    /// "Is the screensaver inactive right now (i.e. is the user
    /// interacting)?" — v1 always returns `false`. niri doesn't expose an
    /// inverse-of-idle signal, and properly tracking this would mean
    /// shipping our own `ext-idle-notify-v1` client. Deprioritised; apps
    /// that rely on this for video-pause heuristics already fall back to
    /// querying input device events.
    #[allow(clippy::unused_self, clippy::unused_async)]
    async fn get_active(&self) -> bool {
        false
    }

    /// "Seconds since the user last did anything." — v1 returns 0. We
    /// could read `loginctl show-session $XDG_SESSION_ID -p IdleSinceHint`
    /// and compute, but no app on a sane configuration depends on the
    /// value being >0 (it's a hint, not load-bearing). Track for follow-up
    /// alongside a real `GetActive()` when we add `ext-idle-notify-v1`.
    #[allow(clippy::unused_self, clippy::unused_async)]
    async fn get_active_time(&self) -> u32 {
        0
    }

    /// "Wake the screen up." — best-effort no-op. Some apps call this to
    /// reset the idle timer when entering full-screen, which is precisely
    /// the case Inhibit/UnInhibit handles for us. Logging at debug so we
    /// can tell from a journal whether anyone in the wild is calling it.
    #[allow(clippy::unused_self, clippy::unused_async)]
    async fn simulate_user_activity(&self) {
        tracing::debug!("SimulateUserActivity (ignored)");
    }
}

// ── Server bootstrap + restart loop ───────────────────────────────────────────

async fn run_server(
    state: Arc<Mutex<HashMap<u32, Inhibitor>>>,
    inhibitors_view: Mutable<Vec<Inhibitor>>,
    next_cookie: Arc<AtomicU32>,
) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("connect session bus")?;

    let iface = ScreenSaverIface {
        state,
        inhibitors: inhibitors_view,
        next_cookie,
    };

    // Mount at both paths *before* claiming the well-known name so apps
    // that race the NameAcquired signal find both objects already present.
    conn.object_server()
        .at(PATH_CANONICAL, iface.clone())
        .await
        .context("register /org/freedesktop/ScreenSaver")?;
    conn.object_server()
        .at(PATH_LEGACY, iface)
        .await
        .context("register /ScreenSaver")?;

    let dbus = fdo::DBusProxy::new(&conn)
        .await
        .context("create DBusProxy")?;

    // Replace any other holder (gnome-screensaver, xfce4-screensaver,
    // mate-screensaver). They can re-grab us back if they outlive us; in
    // practice on a niri+trollshell session they aren't running. Document
    // this collision in the user-facing notes.
    let flags = fdo::RequestNameFlags::ReplaceExisting | fdo::RequestNameFlags::DoNotQueue;
    let reply = dbus
        .request_name("org.freedesktop.ScreenSaver".try_into().unwrap(), flags)
        .await
        .context("request_name org.freedesktop.ScreenSaver")?;

    if reply != fdo::RequestNameReply::PrimaryOwner && reply != fdo::RequestNameReply::AlreadyOwner
    {
        return Err(anyhow!(
            "could not acquire org.freedesktop.ScreenSaver: {reply:?}. \
             Disable gnome-screensaver / xfce4-screensaver / mate-screensaver first."
        ));
    }

    tracing::info!("org.freedesktop.ScreenSaver acquired");

    // Watch for someone else replacing us (or the bus dropping our name).
    // When that happens we tear down + retry from the outer loop, mirroring
    // the bluetooth / polkit Agent re-registration pattern.
    let mut owner_changed = dbus
        .receive_name_owner_changed()
        .await
        .context("subscribe NameOwnerChanged")?;

    while let Some(signal) = owner_changed.next().await {
        let Ok(args) = signal.args() else { continue };
        if args.name().as_str() != "org.freedesktop.ScreenSaver" {
            continue;
        }
        // Lost our slot. Bounce + try to reclaim with ReplaceExisting.
        let unique = conn.unique_name().map(|n| n.as_str().to_string());
        let new_owner = args
            .new_owner()
            .as_ref()
            .map(|n| n.as_str().to_string());
        if new_owner != unique {
            // Best-effort cleanup so the next iteration can re-`at()` fresh.
            let _ = conn
                .object_server()
                .remove::<ScreenSaverIface, _>(PATH_CANONICAL)
                .await;
            let _ = conn
                .object_server()
                .remove::<ScreenSaverIface, _>(PATH_LEGACY)
                .await;
            return Err(anyhow!(
                "org.freedesktop.ScreenSaver owner changed away — re-registering"
            ));
        }
    }

    Err(anyhow!("NameOwnerChanged stream ended"))
}
