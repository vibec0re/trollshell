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
//! `screensaver::lock()` flips an `is_locked: Mutable<bool>` signal;
//! `widgets::lock_screen` subscribes and drives an
//! `ext-session-lock-v1` client (per-monitor lock surfaces with
//! PAM-backed unlock, compositor-enforced input isolation, and
//! crash-safe — see `overlays/lock_screen.rs` for the lifecycle).
//! External triggers (`loginctl lock-session`, `systemd-logind` Lock
//! signal, swayidle before-sleep) flow through the same signal via
//! the login1 listen loop in this module.
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
use hytte_reactive::{registry, runtime, Service};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

// ── Cross-thread shared handle ────────────────────────────────────────────────
//
// `hytte_reactive::registry` is a thread-local — initialised on the GTK main
// thread, empty on hytte-tokio worker threads. Public mutators below
// (`lock`, `inhibit`, `uninhibit`, `handle_unlock_success`) are called from
// BOTH threads:
//   - GTK: widget button handlers, keybinds, power-menu.
//   - hytte-tokio: `ScreenSaverIface::lock`, `ScreenSaverIface::inhibit`,
//     `ScreenSaverIface::un_inhibit` D-Bus methods, which run on the bus
//     connection's tokio worker.
//
// Using `registry::with` from a hytte-tokio thread silently no-ops (no
// handles → early return), causing `Lock()` D-Bus calls and inhibit/uninhibit
// from external apps (Firefox, Chromium, mpv) to be silently dropped. A
// static `OnceLock` populated by `Service::start` is the cross-thread-safe
// alternative — `Mutable<T>`, `Arc<Mutex<…>>`, and `Arc<AtomicU32>` are all
// `Send + Sync`, so storing them in a static is safe.
struct ScreenSaverShared {
    is_locked: Mutable<bool>,
    state: Arc<Mutex<HashMap<u32, Inhibitor>>>,
    inhibitors: Mutable<Vec<Inhibitor>>,
    next_cookie: Arc<AtomicU32>,
}

static SHARED: OnceLock<ScreenSaverShared> = OnceLock::new();

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
    /// Live inhibitor list, keyed by cookie. Mutators now go through SHARED;
    /// kept here so the Arc isn't dropped (its backing Mutex is shared).
    pub(crate) _state: Arc<Mutex<HashMap<u32, Inhibitor>>>,
    /// Reactive view of `state` for UI subscribers. Kept in sync after
    /// every mutation by [`publish_inhibitors`].
    pub(crate) inhibitors: Mutable<Vec<Inhibitor>>,
    /// Monotonic cookie counter. Mutators now go through SHARED; kept here
    /// so the Arc is not dropped prematurely.
    pub(crate) _next_cookie: Arc<AtomicU32>,
    /// `true` while the ext-session-lock-v1 client owns the screen,
    /// `false` otherwise. Set by [`lock`] (and the login1 Lock
    /// signal); cleared by [`handle_unlock_success`] after PAM auth.
    /// Subscribers: `overlays::lock_screen`.
    pub(crate) is_locked: Mutable<bool>,
    /// Keeps the name-ownership task alive for the process lifetime.
    _ownership: hytte_bus::OwnNameSignal,
}

// ── Service marker ────────────────────────────────────────────────────────────

pub struct ScreenSaverService;

impl Service for ScreenSaverService {
    type Handles = ScreenSaverHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let state = Arc::new(Mutex::new(HashMap::new()));
        let inhibitors = Mutable::new(Vec::new());
        // Start at 1 so a leaked default-zero cookie never matches.
        let next_cookie = Arc::new(AtomicU32::new(1));
        let is_locked = Mutable::new(false);

        let iface = ScreenSaverIface {
            state: state.clone(),
            inhibitors: inhibitors.clone(),
            next_cookie: next_cookie.clone(),
        };

        // Own the well-known name on session bus, mount at both paths.
        let ownership = hytte_bus::own_name("org.freedesktop.ScreenSaver")
            .at_path(PATH_CANONICAL, iface.clone())
            .at_path(PATH_LEGACY, iface)
            .start();

        // Start the login1 Session.Lock/Unlock listener.
        let locked_writer = is_locked.clone();
        rt.spawn(spawn_login1_listener(locked_writer));

        let _ = SHARED.set(ScreenSaverShared {
            is_locked: is_locked.clone(),
            state: state.clone(),
            inhibitors: inhibitors.clone(),
            next_cookie: next_cookie.clone(),
        });

        ScreenSaverHandles {
            _state: state,
            inhibitors,
            _next_cookie: next_cookie,
            is_locked,
            _ownership: ownership,
        }
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

/// Signal emitting `true` while the session is locked, `false`
/// otherwise. Subscribed by `overlays::lock_screen` to create and
/// destroy the ext-session-lock-v1 client.
pub fn is_locked() -> impl Signal<Item = bool> {
    registry::with(|r| {
        r.get::<ScreenSaverHandles>()
            .expect("screensaver::service() not registered")
            .is_locked
            .signal_cloned()
    })
}

/// Called by the lock UI after a successful PAM authentication.
/// Flips `is_locked` to false (the `overlays::lock_screen`
/// subscription then calls `Instance::unlock()` on the active
/// session-lock client) and tells logind to release its
/// session-level lock state via `Session.SetLockedHint(false)`.
pub fn handle_unlock_success() {
    let Some(shared) = SHARED.get() else {
        tracing::warn!("handle_unlock_success called before service registered");
        return;
    };
    shared.is_locked.set(false);
    runtime::handle().spawn(async move {
        if let Err(e) = call_login1_unlock().await {
            tracing::warn!(error = %e, "login1 SetLockedHint(false) failed");
        }
    });
}

async fn call_login1_unlock() -> anyhow::Result<()> {
    let session_path = resolve_session_path()
        .await
        .context("resolve login1 session path")?;
    hytte_bus::call("org.freedesktop.login1")
        .bus(hytte_bus::BusKind::System)
        .at_path(session_path)
        .iface("org.freedesktop.login1.Session")
        .method("SetLockedHint")
        .args((false,))
        .send::<()>()
        .await
        .context("Session.SetLockedHint(false)")?;
    Ok(())
}

async fn spawn_login1_listener(is_locked: Mutable<bool>) {
    use futures_signals::signal::SignalExt;
    use futures_util::StreamExt;

    // Cache the session path; resolve once.
    let session_path = match resolve_session_path().await {
        Ok(p) => p,
        Err(e) => {
            tracing::info!(error = %e,
                "no logind session for this process — login1 lock signals disabled");
            return;
        }
    };

    let lock_sub = hytte_bus::signals("org.freedesktop.login1")
        .bus(hytte_bus::BusKind::System)
        .at_path(session_path.clone())
        .iface("org.freedesktop.login1.Session")
        .signal("Lock")
        .start();
    let unlock_sub = hytte_bus::signals("org.freedesktop.login1")
        .bus(hytte_bus::BusKind::System)
        .at_path(session_path.clone())
        .iface("org.freedesktop.login1.Session")
        .signal("Unlock")
        .start();

    let lock_writer = is_locked.clone();
    let unlock_writer = is_locked.clone();
    let lock_writer_for_missed = is_locked.clone();
    let unlock_writer_for_missed = is_locked.clone();

    // Lock events: set is_locked=true.
    // Clone the subscription so the stream is owned and 'static.
    let lock_sub_for_events = lock_sub.clone();
    tokio::spawn(async move {
        let mut stream = lock_sub_for_events.events();
        while stream.next().await.is_some() {
            lock_writer.set(true);
        }
    });

    // Unlock events: set is_locked=false.
    let unlock_sub_for_events = unlock_sub.clone();
    tokio::spawn(async move {
        let mut stream = unlock_sub_for_events.events();
        while stream.next().await.is_some() {
            unlock_writer.set(false);
        }
    });

    // On missed emissions (reconnect), re-fetch authoritative state via GetLockedHint.
    // Clone so the signal is owned and 'static in the spawned task.
    let lock_path_for_missed = session_path.clone();
    let lock_sub_for_missed = lock_sub.clone();
    tokio::spawn(async move {
        lock_sub_for_missed
            .missed_emissions()
            .for_each(move |_| {
                let path = lock_path_for_missed.clone();
                let writer = lock_writer_for_missed.clone();
                async move {
                    match get_locked_hint(&path).await {
                        Ok(locked) => writer.set(locked),
                        Err(e) => tracing::debug!(error = %e, "GetLockedHint after reconnect"),
                    }
                }
            })
            .await;
    });

    // Same for unlock.
    let unlock_path_for_missed = session_path.clone();
    let unlock_sub_for_missed = unlock_sub.clone();
    tokio::spawn(async move {
        unlock_sub_for_missed
            .missed_emissions()
            .for_each(move |_| {
                let path = unlock_path_for_missed.clone();
                let writer = unlock_writer_for_missed.clone();
                async move {
                    match get_locked_hint(&path).await {
                        Ok(locked) => writer.set(locked),
                        Err(e) => tracing::debug!(error = %e, "GetLockedHint after reconnect"),
                    }
                }
            })
            .await;
    });
}

static SESSION_PATH: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

async fn resolve_session_path() -> Result<String, hytte_bus::BusError> {
    SESSION_PATH
        .get_or_try_init(|| async {
            let pid: u32 = std::process::id();
            let path: zbus::zvariant::OwnedObjectPath =
                hytte_bus::call("org.freedesktop.login1")
                    .bus(hytte_bus::BusKind::System)
                    .at_path("/org/freedesktop/login1")
                    .iface("org.freedesktop.login1.Manager")
                    .method("GetSessionByPID")
                    .args((pid,))
                    .send()
                    .await?;
            Ok(path.as_str().to_string())
        })
        .await
        .cloned()
}

async fn get_locked_hint(session_path: &str) -> Result<bool, hytte_bus::BusError> {
    hytte_bus::call("org.freedesktop.login1")
        .bus(hytte_bus::BusKind::System)
        .at_path(session_path.to_string())
        .iface("org.freedesktop.login1.Session")
        .method("GetLockedHint")
        .args(())
        .send::<bool>()
        .await
}

/// Trigger the lock surface. Flips `is_locked` to `true`; the
/// `overlays::lock_screen` subscription instantiates an
/// ext-session-lock-v1 client and mounts the per-monitor lock
/// surfaces in response.
pub fn lock() {
    let Some(shared) = SHARED.get() else {
        tracing::warn!("screensaver::lock called before service registered");
        return;
    };
    shared.is_locked.set(true);
}

/// Programmatically register an inhibitor. Returns the cookie; the caller
/// must pass it back to [`uninhibit`] to release.
///
/// Internal helper for trollshell modules that want to inhibit (e.g. a
/// future "do-not-disturb while presenting" toggle). External apps go
/// through D-Bus.
#[must_use]
pub fn inhibit(application: &str, reason: &str) -> u32 {
    let Some(shared) = SHARED.get() else {
        // Service not registered (test harness?): return a sentinel so the
        // caller can still "release" without panicking.
        return 0;
    };
    let cookie = shared.next_cookie.fetch_add(1, Ordering::Relaxed);
    let was_empty = insert_inhibitor(
        &shared.state,
        Inhibitor {
            cookie,
            application: application.to_string(),
            reason: reason.to_string(),
        },
    );
    publish_inhibitors(&shared.state, &shared.inhibitors);
    if was_empty {
        spawn_pause_swayidle();
    }
    cookie
}

/// Release a cookie returned from [`inhibit`]. Unknown cookies are
/// silently ignored — apps regularly double-call `UnInhibit` on shutdown.
pub fn uninhibit(cookie: u32) {
    let Some(shared) = SHARED.get() else {
        return;
    };
    let became_empty = remove_inhibitor(&shared.state, cookie);
    publish_inhibitors(&shared.state, &shared.inhibitors);
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

// zbus's `#[interface]` macro requires every method to be `async fn` even
// when the body doesn't await; some handlers also don't need `&self` to
// reach state (they call free functions or trivially return constants).
// Allowing at the impl-block keeps the noise out of each method.
#[allow(clippy::unused_async, clippy::unused_self)]
#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaverIface {
    /// Lock the screen now. Flips `is_locked` to `true`. Apps and
    /// `gnome-screensaver-command --lock` use this.
    async fn lock(&self) {
        lock();
    }

    /// Register an inhibitor. Returns a cookie the app must keep + pass
    /// back to `UnInhibit`. Pauses swayidle on the empty → non-empty
    /// transition.
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
    async fn get_active(&self) -> bool {
        false
    }

    /// "Seconds since the user last did anything." — v1 returns 0. We
    /// could read `loginctl show-session $XDG_SESSION_ID -p IdleSinceHint`
    /// and compute, but no app on a sane configuration depends on the
    /// value being >0 (it's a hint, not load-bearing). Track for follow-up
    /// alongside a real `GetActive()` when we add `ext-idle-notify-v1`.
    async fn get_active_time(&self) -> u32 {
        0
    }

    /// "Wake the screen up." — best-effort no-op. Some apps call this to
    /// reset the idle timer when entering full-screen, which is precisely
    /// the case Inhibit/UnInhibit handles for us. Logging at debug so we
    /// can tell from a journal whether anyone in the wild is calling it.
    async fn simulate_user_activity(&self) {
        tracing::debug!("SimulateUserActivity (ignored)");
    }
}

