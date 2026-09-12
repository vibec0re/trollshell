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
//! signal; the session's configured locker (`swaylock`, driven by logind)
//! takes it from there.
//!
//! ## Inhibitors and idle-pipeline enforcement
//!
//! Inhibitors registered here are tracked and surfaced via [`inhibitors`] (the
//! Power drawer's "what's keeping me awake" list). The native idle manager
//! ([`crate::idle_notify`], #204) drives dim/lock/suspend and gates each action
//! on **logind's `BlockInhibited`** — so the enforcement contract is a *logind*
//! inhibitor, not this session-bus list. The manual "Keep awake" toggle holds a
//! real logind idle inhibitor (below), so it suppresses dim/lock through that
//! gate. Modern Wayland apps suppress idle via the compositor's
//! `zwp_idle_inhibit` protocol (niri pauses `ext-idle-notify` directly for
//! those), so this freedesktop `org.freedesktop.ScreenSaver` interface is a
//! compatibility surface for apps that speak only the D-Bus API: their
//! inhibitors are tracked and shown, but with swayidle (and its `SIGSTOP`
//! bridge) retired they no longer force-pause the idle pipeline on their own.
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
//! // Manual "Keep awake" (caffeine) toggle — logind idle inhibitor (#270):
//! screensaver::set_keep_awake(true);                       // engage / release
//! screensaver::keep_awake() -> impl Signal<Item = bool>    // authoritative on/off
//! screensaver::other_inhibitors() -> impl Signal<…>        // "Also awake: …" apps
//! ```

use crate::config_file;
use futures_signals::signal::{Mutable, Signal, SignalExt};
use futures_util::StreamExt;
use hytte_bus::FdLease;
use hytte_reactive::{Service, registry, runtime, shared, spawn_supervised};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use zbus::message::Header;

// ── Keep-awake persistence (#534) ─────────────────────────────────────────────
//
// The logind idle-inhibitor fd is owned by *this* process, so it closes on
// exit and the hold vanishes on a shell restart (`crate::logind::inhibit_idle`
// docs). To make "Keep awake" actually survive a restart — as a user reasonably
// expects and the mechanism claimed — the desired flag is persisted to
// `~/.config/trollshell/keep-awake.toml` and re-acquired on the next
// `Service::start`. Flat `enabled = true|false`, **default OFF** (a fresh
// install has caffeine off), mirroring `dnd`.

/// Config file under `~/.config/trollshell/` holding the persisted keep-awake
/// desire.
const KEEP_AWAKE_CONFIG_FILE: &str = "keep-awake.toml";

/// Load the persisted "Keep awake" desire. **Default OFF**: only an explicit
/// `enabled = true` re-engages caffeine on start; a missing/malformed/empty
/// file leaves it off.
fn load_keep_awake_from_disk() -> bool {
    let Some(text) = config_file::read(KEEP_AWAKE_CONFIG_FILE) else {
        return false;
    };
    parse_keep_awake(&text)
}

/// Parse the flat `enabled = true|false` body. Default OFF — a missing key, a
/// malformed value, or an empty file all leave caffeine off. Split out as a
/// pure fn so it's unit-testable without touching `$HOME`; mirrors `dnd`'s
/// parser.
fn parse_keep_awake(text: &str) -> bool {
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

/// Persist the "Keep awake" desire. Best-effort; failure is logged and the live
/// hold remains the source of truth for this process.
fn save_keep_awake_to_disk(on: bool) {
    config_file::write(
        "keep-awake",
        KEEP_AWAKE_CONFIG_FILE,
        &format!("enabled = {on}\n"),
    );
}

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

// Cookie → caller's unique D-Bus name (#1171), for cookies registered through
// the `Inhibit` D-Bus method (not the internal [`inhibit`] used by the manual
// caffeine toggle, which has no remote caller to track). A client that dies
// without calling `UnInhibit` used to keep its cookie — and the screen awake
// — until this process restarted; the `NameOwnerChanged` watcher spawned in
// `Service::start` drops a cookie the moment its owner's entry names a bus
// name that just vanished (`drop_cookies_for_vanished_owner`).
//
// Not part of `ScreenSaverShared`: unlike `state`/`inhibitors`/`manual`, no
// free function needs to reach this through `shared_state()` — only
// `ScreenSaverIface` (in-process D-Bus method handlers) and
// `watch_owner_changes` touch it, and `Service::start` hands each its own
// `Arc` clone directly. Locked independently of `state`/`manual` — never
// nested under either.
type OwnersMap = Arc<Mutex<HashMap<u32, String>>>;

// ── Manual "Keep awake" (caffeine) ─────────────────────────────────────────────
//
// The Power drawer's "Keep awake" toggle (issue #270) holds a real logind
// idle-inhibitor fd (`logind::inhibit_idle`) — honest, inspectable via
// `systemd-inhibit --list`. That fd is what *enforces* keep-awake: the native
// idle manager (`idle_notify`, #204) gates dim/lock on logind's `BlockInhibited`
// containing `idle`, so the held inhibitor makes it skip them. A matching
// screensaver `Inhibitor` is also registered here purely for *visibility* — so
// the toggle shows up in `inhibitors()` and the "Also awake" list. (Before #204
// this screensaver inhibitor drove a swayidle `SIGSTOP` bridge for enforcement;
// that bridge is retired now that the idle manager reads the logind inhibitor
// directly.)
// The toggle's authoritative on/off state is read back from `inhibitors()`
// (via [`keep_awake`]), never from the acquire call's return — so any monitor's
// drawer can flip it and a drawer rebuild re-derives it.
//
// The held fd is owned by *this* process, so it closes on exit — the hold does
// not survive a shell restart on its own. The desire is therefore persisted to
// `~/.config/trollshell/keep-awake.toml` and re-acquired on the next
// `Service::start` (#534), so "Keep awake" stays on across a restart/upgrade
// instead of silently lapsing while the box quietly goes back to idle-locking.

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
/// screensaver cookie registered for visibility in [`inhibitors`].
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
        let owners: OwnersMap = Arc::new(Mutex::new(HashMap::new()));

        let iface = ScreenSaverIface {
            state: state.clone(),
            inhibitors: inhibitors.clone(),
            next_cookie: next_cookie.clone(),
            owners: owners.clone(),
        };

        // Drop a D-Bus-registered Inhibit cookie the moment its caller's
        // unique bus name vanishes without an `UnInhibit` (#1171) — reuses
        // `hytte_bus::signals`, the same `NameOwnerChanged` subscription
        // primitive `mpris`/`tray` already watch their own peers with,
        // rather than adding a bespoke second watcher. Cloned here (rather
        // than moving `state`/`inhibitors`/`owners` themselves) so those
        // bindings are still available below for `ScreenSaverHandles`.
        //
        // **Before `own_name`, deliberately** (#1192 review, MEDIUM-3):
        // owning the name is what makes `Inhibit` reachable, so a cookie
        // taken before this watcher exists could be orphaned with nobody
        // listening for its owner's departure. Both `.start()`s spawn rather
        // than await, so this ordering narrows the window without closing
        // it — `run_owner_watch`'s startup + periodic reconcile is what
        // bounds the remainder.
        let watch_state = state.clone();
        let watch_inhibitors = inhibitors.clone();
        let watch_owners = owners.clone();
        spawn_supervised("screensaver-owner-watch", move || {
            let state = watch_state.clone();
            let inhibitors = watch_inhibitors.clone();
            let owners = watch_owners.clone();
            async move {
                watch_owner_changes(&state, &inhibitors, &owners).await;
            }
        });

        // Own the well-known name on session bus, mount at both paths.
        let ownership =
            hytte_bus::own_name(hytte_bus::BusKind::Session, "org.freedesktop.ScreenSaver")
                .at_path(PATH_CANONICAL, iface.clone())
                .at_path(PATH_LEGACY, iface)
                .start();

        shared::insert(ScreenSaverShared {
            state: state.clone(),
            inhibitors: inhibitors.clone(),
            next_cookie: next_cookie.clone(),
            manual: Arc::new(Mutex::new(ManualCaffeine::default())),
        });

        // Re-engage a persisted keep-awake hold (#534). The logind idle fd is
        // owned by *this* process — the previous session's fd closed when that
        // process exited, so nothing is holding the box awake right now.
        // Persisting the desire (in `set_keep_awake`) and re-acquiring here is
        // what makes "Keep awake" actually survive a shell restart. We call
        // `acquire_manual` directly (not `set_keep_awake`) so this re-acquire
        // doesn't rewrite the file it just read.
        if load_keep_awake_from_disk()
            && let Some(shared) = shared::get::<ScreenSaverShared>()
        {
            acquire_manual(shared);
        }

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
/// `on = true`: acquire a logind idle-inhibitor fd (honest, inspectable state
/// that the native idle manager honors via `BlockInhibited`) **and** register a
/// matching screensaver inhibitor for visibility in [`inhibitors`]. `on =
/// false`: drop the fd (releasing the logind inhibitor) and remove the
/// screensaver inhibitor.
///
/// Idempotent — calling it with the state it's already in is a no-op, so the
/// GTK switch's programmatic `set_active` (from the authoritative-state
/// binding) can never thrash the logind fd. Safe to call from any thread; the
/// async fd acquire runs on the shared runtime.
///
/// The desire is persisted to `~/.config/trollshell/keep-awake.toml` so the
/// hold is re-acquired on the next shell start (#534) — the logind fd is
/// process-owned and would otherwise be silently dropped on restart. Persisting
/// the user's *intent* (rather than only a confirmed hold) means a transiently
/// failed acquire is simply retried next launch.
pub fn set_keep_awake(on: bool) {
    let Some(shared) = shared::get::<ScreenSaverShared>() else {
        // Service not registered (test harness?) — nothing to hold.
        return;
    };
    // Persist the desire off the GTK main thread. The two-way switch binding
    // blocks its programmatic `set_active` from re-entering the handler, so
    // this fires on genuine user flips, not on authoritative-state sync; the
    // startup re-acquire bypasses this path (calls `acquire_manual` directly),
    // so it never rewrites the file.
    runtime::handle().spawn_blocking(move || save_keep_awake_to_disk(on));
    if on {
        acquire_manual(shared);
    } else {
        release_manual(&shared);
    }
}

/// Does this inhibitor represent the manual caffeine toggle (vs. an external
/// app)? Matched on the sentinel `(application, reason)` we register it with.
fn is_caffeine(i: &Inhibitor) -> bool {
    i.application == CAFFEINE_APP && i.reason == CAFFEINE_REASON
}

/// Engage manual caffeine: mark it desired and, unless a hold or an in-flight
/// acquire already exists, spawn the async logind fd acquire. On success the
/// task registers the screensaver inhibitor (for visibility) and stores the
/// hold; if the user toggled back off while the fd was in flight it drops the
/// fd instead, so a fast on→off never leaks an inhibitor.
fn acquire_manual(shared: Arc<ScreenSaverShared>) {
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
        // Register the visibility half: a screensaver inhibitor that surfaces
        // the hold in `inhibitors()` (which drives `keep_awake()`). Enforcement
        // is the logind idle fd above, honored by the native idle manager.
        let cookie = inhibit(CAFFEINE_APP, CAFFEINE_REASON);
        m.hold = Some(ManualHold {
            cookie,
            _lease: lease,
        });
    });
}

/// Release manual caffeine: clear the desired flag (so any in-flight acquire
/// self-releases on completion) and, if a hold exists, drop the logind fd and
/// remove the screensaver inhibitor.
fn release_manual(shared: &ScreenSaverShared) {
    let mut m = shared.manual.lock().expect("caffeine state poisoned");
    m.desired = false;
    if let Some(hold) = m.hold.take() {
        // uninhibit removes the screensaver inhibitor; dropping `hold` (at end
        // of scope) closes the logind fd, releasing that inhibitor.
        uninhibit(hold.cookie);
    }
}

/// Ask the session to lock. trollshell no longer draws its own lock surface;
/// this runs `loginctl lock-session`, which makes `systemd-logind` emit its
/// `Lock` signal — the session's configured locker (`swaylock`, or whatever
/// you've wired) handles it. Safe to call from any thread: it spawns onto the
/// shared runtime.
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
    let Some(shared) = shared::get::<ScreenSaverShared>() else {
        // Service not registered (test harness?): return a sentinel so the
        // caller can still "release" without panicking.
        return 0;
    };
    let cookie = shared.next_cookie.fetch_add(1, Ordering::Relaxed);
    insert_inhibitor(
        &shared.state,
        Inhibitor {
            cookie,
            application: application.to_string(),
            reason: reason.to_string(),
        },
    );
    publish_inhibitors(&shared.state, &shared.inhibitors);
    cookie
}

/// Release a cookie returned from [`inhibit`]. Unknown cookies are
/// silently ignored — apps regularly double-call `UnInhibit` on shutdown.
pub fn uninhibit(cookie: u32) {
    let Some(shared) = shared::get::<ScreenSaverShared>() else {
        return;
    };
    remove_inhibitor(&shared.state, cookie);
    publish_inhibitors(&shared.state, &shared.inhibitors);
}

// ── Internal: inhibitor map mutation ──────────────────────────────────────────

fn insert_inhibitor(state: &Mutex<HashMap<u32, Inhibitor>>, inh: Inhibitor) {
    let mut map = state.lock().expect("screensaver state poisoned");
    map.insert(inh.cookie, inh);
}

/// Remove the inhibitor for `cookie`. Unknown cookies are silently ignored —
/// apps regularly double-call `UnInhibit` on shutdown.
fn remove_inhibitor(state: &Mutex<HashMap<u32, Inhibitor>>, cookie: u32) {
    let mut map = state.lock().expect("screensaver state poisoned");
    map.remove(&cookie);
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

// ── Internal: dead-caller cleanup (#1171) ──────────────────────────────────────

/// Remove every cookie owned by `vanished_sender` from `owners`, returning
/// the removed cookies. Pulled out of [`watch_owner_changes`] so the
/// bookkeeping is testable without a real bus connection: the async loop is
/// just "deserialize the signal, call this, remove the returned cookies
/// from the inhibitor map too, republish".
///
/// Locks only `owners`, and never while `state` or `manual` is held — see
/// `ScreenSaverShared::owners`'s doc for the lock-ordering note.
fn drop_cookies_for_vanished_owner(
    owners: &Mutex<HashMap<u32, String>>,
    vanished_sender: &str,
) -> Vec<u32> {
    let mut map = owners.lock().expect("screensaver owners poisoned");
    let dead: Vec<u32> = map
        .iter()
        .filter(|(_, sender)| sender.as_str() == vanished_sender)
        .map(|(cookie, _)| *cookie)
        .collect();
    for cookie in &dead {
        map.remove(cookie);
    }
    dead
}

/// Watch session-bus `NameOwnerChanged` and drop any Inhibit cookie whose
/// registering caller just vanished (#1171) — a client that dies without
/// calling `UnInhibit` would otherwise keep the screen awake until this
/// process restarts. Reuses `hytte_bus::signals`, the same subscription
/// primitive `mpris`/`tray` already use to watch their own peers, rather
/// than a bespoke second watcher; runs for the process lifetime under
/// `spawn_supervised`.
///
/// The subscription is deliberately **unfiltered** (no `arg0` match rule),
/// so this task wakes on every name change on the session bus. That is the
/// standard shape for this — a per-owner `arg0=':1.NN'` rule added at
/// `Inhibit` time and removed at `UnInhibit` would be one broker round trip
/// per cookie to save a body that exits in two string comparisons (#1192
/// review, NIT-1) — but it is a choice, not an oversight: revisit it if
/// this ever shows up in a profile.
async fn watch_owner_changes(
    state: &Mutex<HashMap<u32, Inhibitor>>,
    inhibitors: &Mutable<Vec<Inhibitor>>,
    owners: &Mutex<HashMap<u32, String>>,
) {
    let subscription = hytte_bus::signals(hytte_bus::BusKind::Session, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start();
    let messages = subscription.events().map(|event| event.body);
    run_owner_watch(
        messages,
        list_bus_names,
        RECONCILE_INTERVAL,
        state,
        inhibitors,
        owners,
    )
    .await;
    // `subscription` is the live handle — dropping the last one tears the
    // subscription down — so keep it alive across the whole loop, explicitly.
    drop(subscription);
}

/// How long the watcher waits for a `NameOwnerChanged` before sweeping the
/// bus for owners that left without one. See [`run_owner_watch`] for the two
/// windows this covers; a minute is far below the "until the shell restarts"
/// the bug it closes actually lasted, and the sweep is a single `ListNames`
/// round trip that does nothing at all when no cookie is registered.
const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Every name currently owned on the session bus, or `None` if the broker
/// couldn't be asked. `None` is **not** an empty list: a failed probe must
/// never be read as "nobody is on the bus", which would drop every live
/// cookie (see [`reconcile_owners`]).
async fn list_bus_names() -> Option<Vec<String>> {
    match hytte_bus::call(hytte_bus::BusKind::Session, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("ListNames")
        .send::<Vec<String>>()
        .await
    {
        Ok(names) => Some(names),
        Err(e) => {
            tracing::debug!(error = %e, "screensaver: ListNames failed, skipping reconcile");
            None
        }
    }
}

/// The watcher loop, taking its event source as a plain stream of D-Bus
/// messages.
///
/// Split from [`watch_owner_changes`] for one reason: `hytte_bus::signals`
/// subscribes on the **process-global pooled** session connection, which a
/// test cannot point at the ephemeral `dbus-daemon` it spawned (that would
/// need `DBUS_SESSION_BUS_ADDRESS` set before the pool initialises, i.e.
/// `std::env::set_var`, i.e. `unsafe`, which this crate forbids). With the
/// stream as a parameter, the gated `system_tests` module below drives this
/// exact loop — decode, departure check, cookie drop, republish — against a
/// real broker, over a real `Inhibit` call made by a real second connection
/// that then really dies. Before that the only tests were on
/// [`drop_cookies_for_vanished_owner`], a pure `HashMap` filter: inverting
/// the departure check below left all three of them green (#1192 review,
/// MEDIUM-1).
///
/// # Why there is a sweep and not only a stream (#1192 review, MEDIUM-3)
///
/// A pure event loop misses a departure in two windows, and a cookie
/// orphaned in either keeps the screen awake until the shell restarts —
/// exactly the symptom #1171 exists to remove:
///
/// 1. **Startup.** `own_name` makes `Inhibit` reachable as soon as it lands,
///    while the `NameOwnerChanged` subscription is spawned separately and has
///    its own connect latency (`SignalsBuilder::start` spawns, it does not
///    await). `Service::start` now spawns this watcher *before* it owns the
///    name, which narrows the window, but ordering alone cannot close it.
/// 2. **Reconnect.** `hytte_bus::signals` re-subscribes internally on a bus
///    reconnect; anything emitted while it was disconnected is simply gone,
///    and the re-subscribe is not observable from out here (the epoch signal
///    lives on `SharedConnection`, which `hytte-bus` does not hand out).
///
/// So: sweep once at startup, and again after every `RECONCILE_INTERVAL` of
/// quiet, asking the broker who is actually on the bus and dropping cookies
/// whose recorded owner isn't. The sweep is `ListNames` — one round trip for
/// all owners, rather than a `NameHasOwner` per recorded name — and it uses
/// the same drop-and-republish path the event does. The startup sweep runs
/// after `messages` exists but, in production, not necessarily after the
/// subscription is *live*; the periodic sweep is what makes that
/// remaining sliver bounded by a minute instead of by a restart.
///
/// `tokio::time::timeout` around `messages.next()` rather than a `select!`:
/// `tokio`'s `macros` feature is a dev-dependency here, and `next()` is
/// cancel-safe, so a timed-out poll loses nothing.
///
/// `reconcile_interval` is a parameter rather than a read of
/// [`RECONCILE_INTERVAL`] purely so the periodic arm is testable in
/// milliseconds instead of a minute; production always passes that constant.
async fn run_owner_watch<S, P, Fut>(
    mut messages: S,
    live_names: P,
    reconcile_interval: std::time::Duration,
    state: &Mutex<HashMap<u32, Inhibitor>>,
    inhibitors: &Mutable<Vec<Inhibitor>>,
    owners: &Mutex<HashMap<u32, String>>,
) where
    S: futures_util::Stream<Item = zbus::Message> + Unpin,
    P: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<Vec<String>>>,
{
    reconcile_owners(&live_names, state, inhibitors, owners).await;
    loop {
        match tokio::time::timeout(reconcile_interval, messages.next()).await {
            Ok(Some(message)) => handle_name_owner_changed(&message, state, inhibitors, owners),
            // The stream ended: every `SignalSubscription` handle was
            // dropped. Nothing left to watch.
            Ok(None) => break,
            Err(_elapsed) => reconcile_owners(&live_names, state, inhibitors, owners).await,
        }
    }
}

/// Ask the broker who is on the bus and drop every cookie whose recorded
/// owner isn't — the sweep half of [`run_owner_watch`], for departures no
/// `NameOwnerChanged` reached us for.
///
/// A probe that fails returns early and changes nothing: "couldn't ask" must
/// never be read as "nobody is there", which would drop every live cookie on
/// a transient bus hiccup.
async fn reconcile_owners<P, Fut>(
    live_names: &P,
    state: &Mutex<HashMap<u32, Inhibitor>>,
    inhibitors: &Mutable<Vec<Inhibitor>>,
    owners: &Mutex<HashMap<u32, String>>,
) where
    P: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<Vec<String>>>,
{
    if owners.lock().expect("screensaver owners poisoned").is_empty() {
        return; // nothing recorded — don't even ask the broker
    }
    let Some(names) = live_names().await else {
        return;
    };
    let live: HashSet<&str> = names.iter().map(String::as_str).collect();
    let dead = drop_cookies_with_absent_owners(owners, &live);
    if dead.is_empty() {
        return;
    }
    for cookie in &dead {
        tracing::debug!(
            cookie,
            "screensaver: owner absent at reconcile, dropping Inhibit cookie",
        );
        remove_inhibitor(state, *cookie);
    }
    publish_inhibitors(state, inhibitors);
}

/// Remove every cookie whose recorded owner is **not** in `live`, returning
/// the removed cookies. The sweep counterpart of
/// [`drop_cookies_for_vanished_owner`], and split out for the same reason:
/// the bookkeeping is testable without a bus. Same lock discipline — only
/// `owners`, never while `state` or `manual` is held.
fn drop_cookies_with_absent_owners(
    owners: &Mutex<HashMap<u32, String>>,
    live: &HashSet<&str>,
) -> Vec<u32> {
    let mut map = owners.lock().expect("screensaver owners poisoned");
    let dead: Vec<u32> = map
        .iter()
        .filter(|(_, sender)| !live.contains(sender.as_str()))
        .map(|(cookie, _)| *cookie)
        .collect();
    for cookie in &dead {
        map.remove(cookie);
    }
    dead
}

/// Apply one `NameOwnerChanged` message: ignore anything that isn't a
/// departure, otherwise drop every cookie the departed name owned and
/// republish the inhibitor list.
fn handle_name_owner_changed(
    message: &zbus::Message,
    state: &Mutex<HashMap<u32, Inhibitor>>,
    inhibitors: &Mutable<Vec<Inhibitor>>,
    owners: &Mutex<HashMap<u32, String>>,
) {
    let Ok((name, _old_owner, new_owner)) =
        message.body().deserialize::<(String, String, String)>()
    else {
        return;
    };
    if !new_owner.is_empty() {
        return; // a name gaining an owner is not a departure
    }
    // For a direct connection dropping, `name` here IS the vanished
    // unique bus name (":1.NN") — the same value `header.sender()`
    // captured in `ScreenSaverIface::inhibit`.
    let dead = drop_cookies_for_vanished_owner(owners, &name);
    if dead.is_empty() {
        return;
    }
    for cookie in &dead {
        tracing::debug!(
            cookie,
            sender = %name,
            "screensaver: caller vanished, dropping Inhibit cookie",
        );
        remove_inhibitor(state, *cookie);
    }
    publish_inhibitors(state, inhibitors);
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
    owners: Arc<Mutex<HashMap<u32, String>>>,
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
    /// back to `UnInhibit`. The inhibitor is tracked and surfaced via
    /// [`inhibitors`] (the idle manager enforces on logind inhibitors, not this
    /// list — see the module docs).
    ///
    /// Also remembers the caller's unique bus name against the cookie
    /// (#1171): if that name's connection drops without a matching
    /// `UnInhibit`, the `NameOwnerChanged` watcher spawned in
    /// `Service::start` drops the cookie for us — see
    /// `drop_cookies_for_vanished_owner`.
    async fn inhibit(
        &self,
        application_name: String,
        reason_for_inhibit: String,
        #[zbus(header)] header: Header<'_>,
    ) -> u32 {
        let cookie = self.next_cookie.fetch_add(1, Ordering::Relaxed);
        let inh = Inhibitor {
            cookie,
            application: application_name,
            reason: reason_for_inhibit,
        };
        let sender = header
            .sender()
            .map(|s| zbus::names::UniqueName::as_str(s).to_string());
        tracing::debug!(cookie, app = %inh.application, reason = %inh.reason, sender = ?sender, "Inhibit");
        insert_inhibitor(&self.state, inh);
        if let Some(sender) = sender {
            self.owners
                .lock()
                .expect("screensaver owners poisoned")
                .insert(cookie, sender);
        }
        publish_inhibitors(&self.state, &self.inhibitors);
        cookie
    }

    /// Release an inhibitor. Unknown cookies are silently ignored (apps
    /// double-call `UnInhibit` on shutdown).
    async fn un_inhibit(&self, cookie: u32) {
        tracing::debug!(cookie, "UnInhibit");
        remove_inhibitor(&self.state, cookie);
        self.owners
            .lock()
            .expect("screensaver owners poisoned")
            .remove(&cookie);
        publish_inhibitors(&self.state, &self.inhibitors);
    }

    /// "Is the screensaver inactive right now (i.e. is the user
    /// interacting)?" — always returns `false`. The native idle manager
    /// ([`crate::idle_notify`]) now tracks idle state, but wiring it into this
    /// legacy stub isn't worth it: apps that rely on this for video-pause
    /// heuristics already fall back to querying input device events.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keep_awake_defaults_off() {
        // A fresh install has caffeine OFF; only an explicit `enabled = true`
        // re-engages it on start. Missing key / malformed value / empty / comment
        // bodies all leave it off (unlike fullscreen-inhibit, which defaults on).
        assert!(!parse_keep_awake(""));
        assert!(!parse_keep_awake("# just a comment\n"));
        assert!(!parse_keep_awake("something = else\n"));
        assert!(!parse_keep_awake("enabled = maybe\n"));
    }

    #[test]
    fn parse_keep_awake_explicit_on_off() {
        // The round-trip the #534 restart-survival hinges on: an explicit
        // `enabled = true` re-engages the hold on the next start.
        assert!(parse_keep_awake("enabled = true\n"));
        assert!(!parse_keep_awake("enabled = false\n"));
        // Tolerant of spacing / case, like the `dnd` parser it mirrors.
        assert!(parse_keep_awake("enabled=TRUE"));
        assert!(!parse_keep_awake("  enabled  =  False  "));
    }

    #[test]
    fn keep_awake_save_body_round_trips_through_parse() {
        // What `save_keep_awake_to_disk` writes must parse back to the same
        // value — otherwise a persisted "on" wouldn't re-engage on restart.
        assert!(parse_keep_awake(&format!("enabled = {}\n", true)));
        assert!(!parse_keep_awake(&format!("enabled = {}\n", false)));
    }

    #[test]
    fn is_caffeine_matches_only_the_sentinel() {
        // The identity that `keep_awake()` / `other_inhibitors()` (and thus the
        // switch state) hinge on — the manual toggle vs. an external app.
        assert!(is_caffeine(&Inhibitor {
            cookie: 1,
            application: CAFFEINE_APP.to_string(),
            reason: CAFFEINE_REASON.to_string(),
        }));
        assert!(!is_caffeine(&Inhibitor {
            cookie: 2,
            application: "Firefox".to_string(),
            reason: "Playing video".to_string(),
        }));
        // A partial match (right app, wrong reason) is NOT the caffeine toggle.
        assert!(!is_caffeine(&Inhibitor {
            cookie: 3,
            application: CAFFEINE_APP.to_string(),
            reason: "Screen sharing".to_string(),
        }));
    }

    #[test]
    fn owner_vanished_drops_only_that_owners_cookies() {
        // #1171: three cookies, two owned by the same (about-to-vanish)
        // sender, one owned by a still-live sender.
        let owners = Mutex::new(HashMap::from([
            (1, ":1.10".to_string()),
            (2, ":1.20".to_string()),
            (3, ":1.10".to_string()),
        ]));

        let mut dead = drop_cookies_for_vanished_owner(&owners, ":1.10");
        dead.sort_unstable();
        assert_eq!(dead, vec![1, 3]);

        let remaining = owners.lock().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining.get(&2).map(String::as_str), Some(":1.20"));
    }

    #[test]
    fn owner_with_no_cookies_drops_nothing() {
        let owners = Mutex::new(HashMap::from([(1, ":1.10".to_string())]));
        let dead = drop_cookies_for_vanished_owner(&owners, ":1.99");
        assert!(dead.is_empty());
        assert_eq!(owners.lock().unwrap().len(), 1);
    }

    #[test]
    fn reconcile_drops_exactly_the_owners_not_on_the_bus() {
        // #1192 review, MEDIUM-3: the sweep half. Two cookies whose owners
        // are still on the bus, two whose owners are not (a departure that
        // arrived while nothing was subscribed, or across a reconnect).
        let owners = Mutex::new(HashMap::from([
            (1, ":1.10".to_string()),
            (2, ":1.20".to_string()),
            (3, ":1.10".to_string()),
            (4, ":1.30".to_string()),
        ]));
        let live: HashSet<&str> =
            [":1.20", ":1.30", "org.freedesktop.DBus"].into_iter().collect();

        let mut dead = drop_cookies_with_absent_owners(&owners, &live);
        dead.sort_unstable();
        assert_eq!(dead, vec![1, 3]);

        let remaining = owners.lock().unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains_key(&2) && remaining.contains_key(&4));
    }

    /// The **periodic** arm of `run_owner_watch` (#1192 review, MEDIUM-3),
    /// hermetically: the message stream never yields anything, so only the
    /// timeout branch can act, and the probe reports the owner as live on the
    /// startup sweep and gone afterwards — so a passing run means a *second*,
    /// timer-driven sweep really happened. That is the arm covering a bus
    /// reconnect, where `hytte_bus::signals` re-subscribes internally and any
    /// `NameOwnerChanged` emitted while it was disconnected is simply lost.
    ///
    /// **Falsification:** drop the `Err(_elapsed) => reconcile_owners(...)`
    /// arm (or make it a no-op) and this test times out red, while the
    /// startup-sweep tests stay green.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_periodic_sweep_drops_an_owner_that_left_with_no_signal() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        let state: Mutex<HashMap<u32, Inhibitor>> = Mutex::new(HashMap::new());
        let owners = Mutex::new(HashMap::new());
        let inhibitors = Mutable::new(Vec::new());
        insert_inhibitor(
            &state,
            Inhibitor {
                cookie: 1,
                application: "Firefox".to_string(),
                reason: "Playing video".to_string(),
            },
        );
        owners.lock().unwrap().insert(1, ":1.42".to_string());
        publish_inhibitors(&state, &inhibitors);

        // Live on the first probe (the startup sweep must NOT drop it), gone
        // on every one after.
        let probes = AtomicUsize::new(0);
        let live_names = || {
            let nth = probes.fetch_add(1, Ordering::Relaxed);
            async move {
                if nth == 0 {
                    Some(vec![":1.42".to_string()])
                } else {
                    Some(Vec::new())
                }
            }
        };

        let watcher = std::pin::pin!(run_owner_watch(
            futures_util::stream::pending::<zbus::Message>(),
            live_names,
            Duration::from_millis(20),
            &state,
            &inhibitors,
            &owners,
        ));
        let until_empty = std::pin::pin!(async {
            while !inhibitors.get_cloned().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        // The watcher never returns; finishing means `until_empty` won.
        let raced = tokio::time::timeout(
            Duration::from_secs(5),
            futures_util::future::select(watcher, until_empty),
        )
        .await;

        assert!(
            raced.is_ok(),
            "the periodic sweep never dropped the departed owner's cookie",
        );
        assert!(owners.lock().unwrap().is_empty());
        assert!(state.lock().unwrap().is_empty());
        assert!(
            probes.load(Ordering::Relaxed) >= 2,
            "the startup sweep alone is not evidence of a periodic one",
        );
    }

    #[test]
    fn reconcile_with_every_owner_live_drops_nothing() {
        let owners = Mutex::new(HashMap::from([
            (1, ":1.10".to_string()),
            (2, ":1.20".to_string()),
        ]));
        let live: HashSet<&str> = [":1.10", ":1.20"].into_iter().collect();
        assert!(drop_cookies_with_absent_owners(&owners, &live).is_empty());
        assert_eq!(owners.lock().unwrap().len(), 2);
    }

    /// End-to-end bookkeeping (#1171), still fully hermetic: register a
    /// cookie the way `Inhibit` does (insert into `state` + remember its
    /// caller in `owners`), then simulate its caller vanishing the way
    /// `watch_owner_changes` does on a `NameOwnerChanged` event, and assert
    /// the cookie — and its `inhibitors()` entry — are gone.
    #[test]
    fn inhibit_cookie_is_dropped_when_its_owner_vanishes() {
        let state: Mutex<HashMap<u32, Inhibitor>> = Mutex::new(HashMap::new());
        let owners = Mutex::new(HashMap::new());
        let inhibitors = Mutable::new(Vec::new());

        insert_inhibitor(
            &state,
            Inhibitor {
                cookie: 1,
                application: "Firefox".to_string(),
                reason: "Playing video".to_string(),
            },
        );
        owners.lock().unwrap().insert(1, ":1.42".to_string());
        publish_inhibitors(&state, &inhibitors);
        assert_eq!(inhibitors.get_cloned().len(), 1, "cookie should be live");

        let dead = drop_cookies_for_vanished_owner(&owners, ":1.42");
        for cookie in &dead {
            remove_inhibitor(&state, *cookie);
        }
        publish_inhibitors(&state, &inhibitors);

        assert!(
            inhibitors.get_cloned().is_empty(),
            "the cookie should be gone after its owner vanished",
        );
        assert!(owners.lock().unwrap().is_empty());
    }
}

// ── Real-bus coverage for the dead-caller cleanup (#1171) ─────────────────────
//
// Spawn an ephemeral `dbus-daemon`, mount the real `ScreenSaverIface` on it,
// subscribe to `NameOwnerChanged` the way `watch_owner_changes` does, then
// have a *second connection* call `Inhibit` over the wire and die. Everything
// between the wire and the inhibitor list runs for real: zbus's method
// dispatch, `#[zbus(header)]`'s sender capture, the `(s, s, s)` signal decode,
// the departure check, the cookie drop, the republish.
//
// This exists because the three hermetic tests above all exercise
// `drop_cookies_for_vanished_owner` — a ten-line pure `HashMap` filter — and
// nothing else. Measured in #1192's review: inverting the departure check in
// `handle_name_owner_changed` (`if !new_owner.is_empty()` → `if
// new_owner.is_empty()`), which restores exactly the bug #1171 filed, left all
// three green.
//
// Gated behind the `system-tests` cargo feature (whole-module `cfg`) so the
// default `cargo test` doesn't even compile it, keeping the default run
// hermetic. `wifi/nm_agent.rs`'s `system_tests` module is the template — the
// `BusGuard`/`ephemeral_bus` harness below is that one, same shape — including
// why the interface is staged on the connection *builder* rather than mounted
// on an already-built connection (a zbus object-server startup race that
// silently swallows the first method call, #756). Run with:
//   cargo test -p hytte-services --features system-tests --lib screensaver
#[cfg(all(test, feature = "system-tests"))]
mod system_tests {
    use super::{
        Arc, AtomicU32, HashMap, Inhibitor, Mutable, Mutex, OwnersMap, PATH_CANONICAL,
        RECONCILE_INTERVAL, ScreenSaverIface, StreamExt, run_owner_watch,
    };

    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::{Child, Command};
    use zbus::Connection;
    use zbus::connection::Builder;

    /// Upper bound on any single round trip (or on waiting for the watcher to
    /// react to a departure). A liveness guard, not a latency assertion — the
    /// same reasoning, and the same number, as `wifi::nm_agent`'s budget: this
    /// runs inside `nix flake check` next to two nixosTest VMs and the whole
    /// workspace clippy, where CPU contention is the normal condition.
    const DBUS_REPLY_BUDGET: Duration = Duration::from_secs(30);

    const SCREENSAVER_IFACE: &str = "org.freedesktop.ScreenSaver";
    const INTROSPECTABLE_IFACE: &str = "org.freedesktop.DBus.Introspectable";

    /// Kills the spawned `dbus-daemon` on drop. Mirrors `hytte-bus`'s
    /// `BusGuard`: SIGKILL + a `block_in_place` wait so the socket `TempDir`
    /// outlives the process (hence the multi-thread runtime requirement on
    /// every test here).
    struct BusGuard {
        child: Option<Child>,
        _tmp: TempDir,
        address: String,
    }

    impl Drop for BusGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.start_kill();
                tokio::task::block_in_place(|| {
                    let handle = tokio::runtime::Handle::current();
                    let _ = handle.block_on(child.wait());
                });
            }
        }
    }

    /// Spawn a fresh `dbus-daemon` and return a guard that kills it on drop.
    async fn ephemeral_bus() -> BusGuard {
        let tmp = TempDir::new().expect("create tempdir for dbus-daemon");
        let socket: PathBuf = tmp.path().join("bus");
        let address = format!("unix:path={}", socket.display());

        let config = tmp.path().join("session.conf");
        std::fs::write(
            &config,
            format!(
                r#"<?xml version="1.0"?>
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>{address}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
            ),
        )
        .expect("write dbus-daemon config");

        let mut child = Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&config)
            .arg("--print-address=1")
            .arg("--nofork")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn dbus-daemon — install package `dbus` if missing");

        let stdout = child.stdout.take().expect("dbus-daemon stdout");
        let mut lines = BufReader::new(stdout).lines();
        let printed = tokio::time::timeout(DBUS_REPLY_BUDGET, lines.next_line())
            .await
            .expect("dbus-daemon address timeout")
            .expect("dbus-daemon read address")
            .expect("dbus-daemon closed stdout");
        assert!(
            printed.contains("unix:path="),
            "unexpected dbus-daemon address: {printed}"
        );

        BusGuard {
            child: Some(child),
            _tmp: tmp,
            address,
        }
    }

    /// The handles the iface and the watcher share, plus the iface itself —
    /// built exactly the way `Service::start` builds them.
    struct Fixture {
        iface: ScreenSaverIface,
        state: Arc<Mutex<HashMap<u32, Inhibitor>>>,
        inhibitors: Mutable<Vec<Inhibitor>>,
        owners: OwnersMap,
    }

    fn fixture() -> Fixture {
        let state = Arc::new(Mutex::new(HashMap::new()));
        let inhibitors = Mutable::new(Vec::new());
        let next_cookie = Arc::new(AtomicU32::new(1));
        let owners: OwnersMap = Arc::new(Mutex::new(HashMap::new()));
        let iface = ScreenSaverIface {
            state: state.clone(),
            inhibitors: inhibitors.clone(),
            next_cookie,
            owners: owners.clone(),
        };
        Fixture {
            iface,
            state,
            inhibitors,
            owners,
        }
    }

    /// Host `iface` on a fresh connection to `guard`'s bus, returning the
    /// connection (bind it — it is the only thing keeping the object mounted)
    /// and its unique name, which is what a client addresses it by.
    ///
    /// `.serve_at(...)` on the **builder**, never `object_server().at(...)`
    /// on a built connection: see `wifi/nm_agent.rs`'s `mount_and_proxy` for
    /// the zbus startup race that makes the difference (#756).
    async fn mount(guard: &BusGuard, iface: ScreenSaverIface) -> (Connection, String) {
        let server = Builder::address(guard.address.as_str())
            .expect("parse ephemeral bus address")
            .serve_at(PATH_CANONICAL, iface)
            .expect("stage ScreenSaverIface at PATH_CANONICAL")
            .build()
            .await
            .expect("connect screensaver host to ephemeral bus");
        let dest = server
            .unique_name()
            .expect("server has a unique name")
            .to_string();
        (server, dest)
    }

    /// Subscribe to the broker's `NameOwnerChanged` and hand back the stream
    /// `run_owner_watch` consumes. `MessageStream::for_match_rule` awaits the
    /// `AddMatch` round trip, so **by the time this returns the subscription
    /// is live** — which is what lets the departure test below be
    /// deterministic rather than racing the broker.
    async fn name_owner_changed(
        conn: &Connection,
    ) -> impl futures_util::Stream<Item = zbus::Message> + Unpin + use<> {
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")
            .expect("valid sender")
            .path("/org/freedesktop/DBus")
            .expect("valid path")
            .interface("org.freedesktop.DBus")
            .expect("valid interface")
            .member("NameOwnerChanged")
            .expect("valid member")
            .build();
        let stream = zbus::MessageStream::for_match_rule(rule, conn, None)
            .await
            .expect("subscribe to NameOwnerChanged on the ephemeral bus");
        Box::pin(stream.filter_map(|m| std::future::ready(m.ok())))
    }

    /// Open a second connection and return it with its unique bus name — the
    /// value `#[zbus(header)]`'s `header.sender()` records against a cookie.
    async fn client(guard: &BusGuard) -> (Connection, String) {
        let conn = Builder::address(guard.address.as_str())
            .expect("parse ephemeral bus address")
            .build()
            .await
            .expect("connect client to ephemeral bus");
        let name = conn
            .unique_name()
            .expect("client has a unique name")
            .to_string();
        (conn, name)
    }

    /// Call `Inhibit(application, reason) -> u32` over the wire.
    ///
    /// The typing here is the point: the argument tuple is `(&str, &str)`
    /// (`ss`) and the reply is bound to `u32` (`u`), so this call *is* a wire
    /// signature assertion — a handler that grew or lost an argument would
    /// fail to dispatch rather than silently keep working. #1171 added a
    /// `#[zbus(header)] header: Header<'_>` parameter to that handler, and
    /// zbus header parameters are out-of-band rather than wire arguments;
    /// this is part of what proves it stayed that way (see also
    /// `inhibit_is_still_two_strings_in_one_uint_out`).
    async fn inhibit(conn: &Connection, dest: &str, application: &str, reason: &str) -> u32 {
        let proxy = zbus::Proxy::new(conn, dest.to_string(), PATH_CANONICAL, SCREENSAVER_IFACE)
            .await
            .expect("build ScreenSaver proxy");
        tokio::time::timeout(
            DBUS_REPLY_BUDGET,
            proxy.call("Inhibit", &(application, reason)),
        )
        .await
        .expect("Inhibit timed out — wire signature regression?")
        .expect("Inhibit call failed")
    }

    /// Every name currently owned on `conn`'s bus — the test's stand-in for
    /// production's `list_bus_names`, which asks the pooled session
    /// connection instead of this ephemeral one.
    async fn list_names_on(conn: &Connection) -> Option<Vec<String>> {
        let proxy = zbus::fdo::DBusProxy::new(conn).await.ok()?;
        let names = proxy.list_names().await.ok()?;
        Some(names.into_iter().map(|n| n.to_string()).collect())
    }

    /// Spawn the production watcher loop over `messages`, with `conn` as the
    /// broker its reconcile sweep asks. Returns the join handle; every test
    /// aborts it at the end.
    fn spawn_watcher(
        messages: impl futures_util::Stream<Item = zbus::Message> + Unpin + Send + 'static,
        conn: &Connection,
        fx: &Fixture,
    ) -> tokio::task::JoinHandle<()> {
        let (state, inhibitors, owners) =
            (fx.state.clone(), fx.inhibitors.clone(), fx.owners.clone());
        let probe_conn = conn.clone();
        tokio::spawn(async move {
            let live_names = move || {
                let conn = probe_conn.clone();
                async move { list_names_on(&conn).await }
            };
            run_owner_watch(
                messages,
                live_names,
                RECONCILE_INTERVAL,
                &state,
                &inhibitors,
                &owners,
            )
            .await;
        })
    }

    /// Poll the broker until `name` no longer has an owner, so a test that
    /// wants "the caller is definitely gone" isn't racing its teardown.
    async fn wait_until_name_gone(conn: &Connection, name: &str) {
        let proxy = zbus::fdo::DBusProxy::new(conn)
            .await
            .expect("build DBus proxy");
        let polled = tokio::time::timeout(DBUS_REPLY_BUDGET, async {
            loop {
                let owned = proxy
                    .name_has_owner(
                        name.try_into().expect("the client's unique name is valid"),
                    )
                    .await
                    .unwrap_or(false);
                if !owned {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(polled.is_ok(), "{name} never left the bus");
    }

    /// Poll `inhibitors` until it is empty, or fail on the budget.
    async fn wait_until_no_inhibitors(inhibitors: &Mutable<Vec<Inhibitor>>, why: &str) {
        let polled = tokio::time::timeout(DBUS_REPLY_BUDGET, async {
            while !inhibitors.get_cloned().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            polled.is_ok(),
            "{why}; still holding {:?}",
            inhibitors.get_cloned(),
        );
    }

    /// #1171's whole point, end to end on a real broker: an app takes an
    /// `Inhibit` cookie and dies without calling `UnInhibit`; the cookie must
    /// go with it.
    ///
    /// **Falsification (measured):** invert the departure check in
    /// `handle_name_owner_changed` — `if !new_owner.is_empty()` → `if
    /// new_owner.is_empty()` — and this test fails on
    /// `wait_until_no_inhibitors`. The three hermetic tests above stay green,
    /// which is why this file needed a real bus at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cookie_dies_with_the_connection_that_took_it() {
        let guard = ephemeral_bus().await;
        let fx = fixture();
        let (server, dest) = mount(&guard, fx.iface.clone()).await;

        // Subscribe BEFORE anyone can call Inhibit: `for_match_rule` awaits
        // AddMatch, so there is no window where a departure could be missed,
        // and the startup reconcile has nothing to find — this test is about
        // the event path, not the sweep.
        let watcher = spawn_watcher(name_owner_changed(&server).await, &server, &fx);

        let (client_conn, client_name) = client(&guard).await;
        let cookie = inhibit(&client_conn, &dest, "Firefox", "Playing video").await;

        assert_eq!(
            fx.inhibitors.get_cloned().len(),
            1,
            "the Inhibit call should have registered exactly one inhibitor",
        );
        assert_eq!(
            fx.owners
                .lock()
                .expect("owners poisoned")
                .get(&cookie)
                .map(String::as_str),
            Some(client_name.as_str()),
            "the cookie must be recorded against the *caller's* unique name",
        );

        // The app dies without calling UnInhibit.
        drop(client_conn);

        wait_until_no_inhibitors(
            &fx.inhibitors,
            "the dead caller's cookie should have been dropped",
        )
        .await;
        assert!(
            fx.owners.lock().expect("owners poisoned").is_empty(),
            "the owners map must not leak the dead caller's entry either",
        );
        assert!(
            fx.state.lock().expect("state poisoned").is_empty(),
            "the inhibitor map must be empty too, not just its published view",
        );

        watcher.abort();
    }

    /// #1192 review, MEDIUM-3 — the caller died in the window *before* the
    /// watcher was listening, so no `NameOwnerChanged` will ever arrive for
    /// it: only the startup reconcile can drop this cookie. That window is
    /// real in production (owning the name makes `Inhibit` reachable, and
    /// `SignalsBuilder::start` spawns rather than awaits) and it is the same
    /// shape a bus reconnect leaves behind.
    ///
    /// **Falsification:** delete the `reconcile_owners(...)` call before
    /// `run_owner_watch`'s loop and this test fails on
    /// `wait_until_no_inhibitors` — `RECONCILE_INTERVAL` (60 s) is
    /// deliberately longer than `DBUS_REPLY_BUDGET` (30 s), so the periodic
    /// sweep cannot quietly stand in for the startup one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cookie_orphaned_before_the_watcher_existed_is_reconciled_away() {
        let guard = ephemeral_bus().await;
        let fx = fixture();
        let (server, dest) = mount(&guard, fx.iface.clone()).await;

        // A caller takes a cookie and dies while nothing is watching.
        let (client_conn, client_name) = client(&guard).await;
        let cookie = inhibit(&client_conn, &dest, "Firefox", "Playing video").await;
        assert_eq!(
            fx.owners
                .lock()
                .expect("owners poisoned")
                .get(&cookie)
                .map(String::as_str),
            Some(client_name.as_str()),
        );
        drop(client_conn);
        wait_until_name_gone(&server, &client_name).await;

        // Only now does the watcher exist. Its `NameOwnerChanged`
        // subscription can never see a departure that already happened.
        let watcher = spawn_watcher(name_owner_changed(&server).await, &server, &fx);

        wait_until_no_inhibitors(
            &fx.inhibitors,
            "the startup reconcile should have dropped a cookie whose owner \
             left before the watcher subscribed",
        )
        .await;
        assert!(fx.owners.lock().expect("owners poisoned").is_empty());
        assert!(fx.state.lock().expect("state poisoned").is_empty());

        watcher.abort();
    }

    /// A live caller's cookies survive an *unrelated* connection's death —
    /// the other half of the departure check, and what would catch a watcher
    /// that drops everything on any name change. It is also the assertion
    /// that the reconcile sweep does not mistake a live owner for a departed
    /// one: the sweep runs at startup here, with the keeper already on the
    /// bus by the time its cookie is taken.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unrelated_departure_leaves_a_live_cookie_alone() {
        let guard = ephemeral_bus().await;
        let fx = fixture();
        let (server, dest) = mount(&guard, fx.iface.clone()).await;

        let watcher = spawn_watcher(name_owner_changed(&server).await, &server, &fx);

        let (keeper, _keeper_name) = client(&guard).await;
        let first = inhibit(&keeper, &dest, "mpv", "Playing video").await;

        // A different connection comes and goes.
        let (bystander, bystander_name) = client(&guard).await;
        drop(bystander);

        // A fresh call from the still-live keeper is a round trip through the
        // same broker the departure went through, so by the time it returns
        // the watcher has had the `NameOwnerChanged` dispatched to it; the
        // short sleep covers the watcher task's own scheduling.
        let second = inhibit(&keeper, &dest, "mpv", "Still playing").await;
        assert_ne!(first, second, "cookies must be unique");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let live = fx.inhibitors.get_cloned();
        assert_eq!(
            live.len(),
            2,
            "a bystander ({bystander_name}) leaving must not drop a live caller's cookies: {live:?}",
        );

        watcher.abort();
    }

    /// `Inhibit`'s published wire signature is `ss` → `u`, and the
    /// `#[zbus(header)]` parameter #1171 added did not become a third
    /// argument. `a_cookie_dies_with_the_connection_that_took_it` already
    /// makes the typed call; this asserts the same thing where a real client
    /// looks it up, so a regression reads as "the signature changed" rather
    /// than "the call hangs".
    #[tokio::test(flavor = "multi_thread")]
    async fn inhibit_is_still_two_strings_in_one_uint_out() {
        let guard = ephemeral_bus().await;
        let fx = fixture();
        let (server, dest) = mount(&guard, fx.iface.clone()).await;
        let _ = &server;
        let (client_conn, _name) = client(&guard).await;

        let proxy = zbus::Proxy::new(&client_conn, dest, PATH_CANONICAL, INTROSPECTABLE_IFACE)
            .await
            .expect("build Introspectable proxy");
        let xml: String = tokio::time::timeout(DBUS_REPLY_BUDGET, proxy.call("Introspect", &()))
            .await
            .expect("Introspect timed out")
            .expect("Introspect call failed");

        let method = xml
            .split_once(r#"<method name="Inhibit">"#)
            .and_then(|(_, rest)| rest.split_once("</method>"))
            .map(|(body, _)| body.to_string())
            .unwrap_or_else(|| panic!("no Inhibit method in introspection XML:\n{xml}"));

        let in_args = method.matches(r#"direction="in""#).count();
        let string_in_args = method.matches(r#"type="s" direction="in""#).count();
        let uint_out_args = method.matches(r#"type="u" direction="out""#).count();
        assert_eq!(
            (in_args, string_in_args, uint_out_args),
            (2, 2, 1),
            "Inhibit must stay `ss` -> `u`; a #[zbus(header)] parameter is \
             out-of-band and must not appear as an argument:\n{method}",
        );
    }
}
