//! `org.freedesktop.ScreenSaver` D-Bus service — idle inhibition.
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
//! **Locking is delegated, not implemented here.** trollshell deliberately
//! does NOT ship its own lock surface — a PAM-backed password screen is
//! exactly the security-critical wheel not worth reinventing. Both the D-Bus
//! `Lock()` method and [`lock`] (used by the power menu) run
//! `loginctl lock-session`, which makes `systemd-logind` emit its `Lock`
//! signal; the session's configured locker (e.g. swayidle's `lock` handler
//! running swaylock) takes it from there.
//!
//! ## swayidle pause / resume
//!
//! While at least one inhibitor is active, swayidle is paused via `SIGSTOP`;
//! when the last inhibitor releases, we send `SIGCONT`. swayidle upstream has
//! no built-in pause/resume signal contract — `SIGUSR1` in recent versions
//! actually *triggers* an idle event, which is the opposite of what we want.
//! STOP/CONT works regardless of swayidle's internal state machine but is
//! necessarily a process-level halt: any pending `before-sleep` callback in
//! flight when STOP arrives will not complete until CONT. In practice
//! swayidle is usually parked on its event loop, so this is fine.
//!
//! The integration assumes swayidle is the systemd `swayidle.service` user
//! unit; PID discovery goes through
//! `systemctl --user show -p MainPID --value swayidle.service`. If the user
//! runs swayidle outside the unit, pause/resume becomes a no-op and
//! inhibitors are tracked but not enforced.
//!
//! # Public API
//!
//! ```ignore
//! // Register once at startup:
//! .with(screensaver::service())
//!
//! // From a power-menu / keybind — asks the session to lock:
//! screensaver::lock();
//!
//! // Programmatic inhibit (rare — apps usually do this themselves over D-Bus):
//! let cookie = screensaver::inhibit("trollshell", "presentation mode");
//! // … later …
//! screensaver::uninhibit(cookie);
//!
//! // Subscribe in widgets (e.g. a "what's keeping me awake" drawer page):
//! screensaver::inhibitors() -> impl Signal<Item = Vec<Inhibitor>>
//!
//! // Manual "Keep awake" (caffeine) toggle — hybrid logind fd + SIGSTOP (#270):
//! screensaver::set_keep_awake(true);                       // engage / release
//! screensaver::keep_awake() -> impl Signal<Item = bool>    // authoritative on/off
//! screensaver::other_inhibitors() -> impl Signal<…>        // "Also awake: …" apps
//! ```

use anyhow::{Context, Result, anyhow};
use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_bus::FdLease;
use hytte_reactive::{Service, registry, runtime};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

// ── Cross-thread shared handle ────────────────────────────────────────────────
//
// `hytte_reactive::registry` is a thread-local — initialised on the GTK main
// thread, empty on hytte-tokio worker threads. Public mutators below
// (`inhibit`, `uninhibit`) are called from BOTH threads:
//   - GTK: widget button handlers, keybinds, power-menu.
//   - hytte-tokio: `ScreenSaverIface::inhibit` / `un_inhibit` D-Bus methods,
//     which run on the bus connection's tokio worker.
//
// Using `registry::with` from a hytte-tokio thread silently no-ops (no
// handles → early return), so external inhibit/uninhibit (Firefox, Chromium,
// mpv) would be dropped. A static `OnceLock` populated by `Service::start`
// is the cross-thread-safe alternative — `Mutable<T>`, `Arc<Mutex<…>>`, and
// `Arc<AtomicU32>` are all `Send + Sync`.
struct ScreenSaverShared {
    state: Arc<Mutex<HashMap<u32, Inhibitor>>>,
    inhibitors: Mutable<Vec<Inhibitor>>,
    next_cookie: Arc<AtomicU32>,
    /// Manual "Keep awake" (caffeine) coordination — the logind idle-inhibitor
    /// lease plus the screensaver cookie it's mirrored to. A separate mutex
    /// from `state`; lock ordering is always `manual` → `state`, never the
    /// reverse, so the two can't deadlock.
    manual: Arc<Mutex<ManualCaffeine>>,
}

static SHARED: OnceLock<ScreenSaverShared> = OnceLock::new();

// ── Manual "Keep awake" (caffeine) ─────────────────────────────────────────────
//
// The Power drawer's "Keep awake" toggle is a *hybrid* (issue #270):
//   - State: a real logind idle-inhibitor fd (`logind::inhibit_idle`) — honest,
//     inspectable via `systemd-inhibit --list`, survives a shell restart.
//   - Enforcement: a matching screensaver `Inhibitor` registered here, so the
//     existing swayidle `SIGSTOP` bridge actually keeps the screen awake — the
//     same path that already enforces external `org.freedesktop.ScreenSaver`
//     apps (Firefox/mpv/screen-share) a pure-logind `BlockInhibited` watch
//     would miss.
// The toggle's authoritative on/off state is read back from `inhibitors()`
// (via [`keep_awake`]), never from the acquire call's return — so any monitor's
// drawer can flip it and a drawer rebuild re-derives it.

/// Sentinel identity of the manual caffeine inhibitor in the shared inhibitor
/// map, so it can be told apart from external app inhibitors when deriving the
/// switch state ([`keep_awake`]) and the "Also awake" list ([`other_inhibitors`]).
const CAFFEINE_APP: &str = "trollshell";
const CAFFEINE_REASON: &str = "Keep awake";

/// State of the manual caffeine toggle. Held behind `ScreenSaverShared::manual`.
#[derive(Default)]
struct ManualCaffeine {
    /// The state the user last asked for. The async acquire task reconciles
    /// against this so a fast on→off (before the logind fd lands) can't leave a
    /// dangling inhibitor.
    desired: bool,
    /// An acquire task is in flight; suppresses spawning a second one.
    acquiring: bool,
    /// The live hold, present iff caffeine is currently engaged.
    hold: Option<ManualHold>,
}

/// A live manual caffeine hold: the logind fd (drop = release) plus the
/// screensaver cookie registered for `SIGSTOP` enforcement.
struct ManualHold {
    cookie: u32,
    /// Dropping this fd closes it, releasing the logind idle inhibitor. Held,
    /// not read — its lifetime is the whole point.
    _lease: FdLease,
}

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
    /// Live inhibitor list, keyed by cookie. Mutators go through SHARED;
    /// kept here so the Arc isn't dropped (its backing Mutex is shared).
    pub(crate) _state: Arc<Mutex<HashMap<u32, Inhibitor>>>,
    /// Reactive view of `state` for UI subscribers. Kept in sync after
    /// every mutation by [`publish_inhibitors`].
    pub(crate) inhibitors: Mutable<Vec<Inhibitor>>,
    /// Monotonic cookie counter. Mutators go through SHARED; kept here so
    /// the Arc is not dropped prematurely.
    pub(crate) _next_cookie: Arc<AtomicU32>,
    /// Keeps the name-ownership task alive for the process lifetime.
    _ownership: hytte_bus::OwnNameSignal,
}

// ── Service marker ────────────────────────────────────────────────────────────

pub struct ScreenSaverService;

impl Service for ScreenSaverService {
    type Handles = ScreenSaverHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let state = Arc::new(Mutex::new(HashMap::new()));
        let inhibitors = Mutable::new(Vec::new());
        // Start at 1 so a leaked default-zero cookie never matches.
        let next_cookie = Arc::new(AtomicU32::new(1));

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

        let _ = SHARED.set(ScreenSaverShared {
            state: state.clone(),
            inhibitors: inhibitors.clone(),
            next_cookie: next_cookie.clone(),
            manual: Arc::new(Mutex::new(ManualCaffeine::default())),
        });

        ScreenSaverHandles {
            _state: state,
            inhibitors,
            _next_cookie: next_cookie,
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

/// Whether the manual "Keep awake" (caffeine) toggle is engaged, derived from
/// the **authoritative** inhibitor list rather than any local widget state — so
/// every monitor's Power drawer agrees and a drawer rebuild re-reads it. Bind a
/// `Keep awake` switch's `active` to this. Multi-monitor safe (issue #270).
pub fn keep_awake() -> impl Signal<Item = bool> {
    inhibitors().map(|v| v.iter().any(is_caffeine))
}

/// Everything keeping the system awake **other than** the manual caffeine
/// toggle — external `org.freedesktop.ScreenSaver` inhibitors (Firefox, mpv,
/// screen-share, …). Feeds the "Also awake: …" subtitle so the user sees that
/// turning the toggle off won't let the screen sleep while an app still holds
/// it.
pub fn other_inhibitors() -> impl Signal<Item = Vec<Inhibitor>> {
    inhibitors().map(|v| v.into_iter().filter(|i| !is_caffeine(i)).collect())
}

/// Turn the manual "Keep awake" caffeine inhibitor on or off.
///
/// `on = true`: acquire a logind idle-inhibitor fd (honest, inspectable state)
/// **and** register a matching screensaver inhibitor so the existing swayidle
/// `SIGSTOP` bridge actually enforces keep-awake. `on = false`: drop the fd
/// (releasing the logind inhibitor) and remove the screensaver inhibitor
/// (`SIGCONT` if it was the last).
///
/// Idempotent — calling it with the state it's already in is a no-op, so the
/// GTK switch's programmatic `set_active` (from the authoritative-state
/// binding) can never thrash the logind fd. Safe to call from any thread; the
/// async fd acquire runs on the shared runtime.
pub fn set_keep_awake(on: bool) {
    let Some(shared) = SHARED.get() else {
        // Service not registered (test harness?) — nothing to hold.
        return;
    };
    if on {
        acquire_manual(shared);
    } else {
        release_manual(shared);
    }
}

/// Does this inhibitor represent the manual caffeine toggle (vs. an external
/// app)? Matched on the sentinel `(application, reason)` we register it with.
fn is_caffeine(i: &Inhibitor) -> bool {
    i.application == CAFFEINE_APP && i.reason == CAFFEINE_REASON
}

/// Engage manual caffeine: mark it desired and, unless a hold or an in-flight
/// acquire already exists, spawn the async logind fd acquire. On success the
/// task registers the screensaver inhibitor (which drives `SIGSTOP`) and stores
/// the hold; if the user toggled back off while the fd was in flight it drops
/// the fd instead, so a fast on→off never leaks an inhibitor.
fn acquire_manual(shared: &'static ScreenSaverShared) {
    {
        let mut m = shared.manual.lock().expect("caffeine state poisoned");
        m.desired = true;
        if m.acquiring || m.hold.is_some() {
            return; // already engaged or coming online
        }
        m.acquiring = true;
    }
    runtime::handle().spawn(async move {
        let lease = match crate::logind::inhibit_idle().await {
            Ok(lease) => lease,
            Err(e) => {
                tracing::warn!(error = %e, "keep-awake: logind Inhibit(idle) failed");
                {
                    let mut m = shared.manual.lock().expect("caffeine state poisoned");
                    m.acquiring = false;
                    m.desired = false;
                }
                // Re-publish so `keep_awake()` re-emits `false`: a switch the
                // user optimistically flipped on snaps back, since the acquire
                // didn't take (no inhibitor was registered).
                publish_inhibitors(&shared.state, &shared.inhibitors);
                return;
            }
        };
        let mut m = shared.manual.lock().expect("caffeine state poisoned");
        m.acquiring = false;
        if !m.desired {
            // Toggled back off before the fd landed — release it (drop closes
            // the fd) and leave no inhibitor behind.
            drop(lease);
            return;
        }
        // Register the enforcement half: a screensaver inhibitor. This pauses
        // swayidle (SIGSTOP) on the empty→non-empty transition and surfaces the
        // hold in `inhibitors()` (which drives `keep_awake()`).
        let cookie = inhibit(CAFFEINE_APP, CAFFEINE_REASON);
        m.hold = Some(ManualHold {
            cookie,
            _lease: lease,
        });
    });
}

/// Release manual caffeine: clear the desired flag (so any in-flight acquire
/// self-releases on completion) and, if a hold exists, drop the logind fd and
/// remove the screensaver inhibitor (`SIGCONT` if it was the last).
fn release_manual(shared: &ScreenSaverShared) {
    let mut m = shared.manual.lock().expect("caffeine state poisoned");
    m.desired = false;
    if let Some(hold) = m.hold.take() {
        // uninhibit removes the inhibitor + resumes swayidle if last; dropping
        // `hold` (at end of scope) closes the logind fd, releasing that lock.
        uninhibit(hold.cookie);
    }
}

/// Ask the session to lock. trollshell no longer draws its own lock surface;
/// this runs `loginctl lock-session`, which makes `systemd-logind` emit its
/// `Lock` signal — the session's configured locker (swayidle → swaylock, or
/// whatever you've wired) handles it. Safe to call from any thread: it spawns
/// onto the shared runtime.
pub fn lock() {
    runtime::handle().spawn(async {
        match tokio::process::Command::new("loginctl")
            .arg("lock-session")
            .status()
            .await
        {
            Ok(status) if status.success() => {}
            Ok(status) => {
                tracing::warn!(code = ?status.code(), "loginctl lock-session exited non-zero");
            }
            Err(e) => tracing::warn!(error = %e, "failed to run loginctl lock-session"),
        }
    });
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
        .args([
            "--user",
            "show",
            "-p",
            "MainPID",
            "--value",
            "swayidle.service",
        ])
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
    /// Lock the screen now — delegates to `loginctl lock-session` (see
    /// [`lock`]); the session's configured locker takes it from there. Apps
    /// and `gnome-screensaver-command --lock` use this.
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
    /// value being >0 (it's a hint, not load-bearing).
    async fn get_active_time(&self) -> u32 {
        0
    }

    /// "Wake the screen up." — best-effort no-op. Some apps call this to
    /// reset the idle timer when entering full-screen, which is precisely
    /// the case Inhibit/UnInhibit handles for us.
    async fn simulate_user_activity(&self) {
        tracing::debug!("SimulateUserActivity (ignored)");
    }
}
