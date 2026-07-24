//! Native `ext-idle-notify-v1` idle manager (#204).
//!
//! trollshell owns the idle → dim → lock → suspend timeline in-process — it is
//! the idle daemon, replacing swayidle entirely. It binds the compositor's
//! `ext_idle_notifier_v1` global and creates one `ext_idle_notification_v1` per
//! threshold (240 dim / 300 lock / 600 suspend), exposes "idle since / resumed"
//! reactively via [`state`], and fires the three actions natively at their
//! thresholds:
//!
//! - **dim** — `brightnessctl -s set 10%` (saved level restored on resume),
//! - **lock** — [`crate::screensaver::lock`] (`loginctl lock-session`),
//! - **suspend** — `systemctl suspend`,
//!
//! each **gated on logind's `BlockInhibited`**: dim/lock skip while `idle` is
//! inhibited, suspend skips while `idle` *or* `sleep` is — so "Keep awake" /
//! an idle-inhibiting app (mpv/Firefox/a playing video) holds off the whole
//! timeline, not just dim/lock (#420). Additionally it relocks the
//! session just before the system sleeps by handling logind's
//! `PrepareForSleep(true)` signal — the native replacement for swayidle's
//! `before-sleep 'loginctl lock-session'` (also reusing
//! [`crate::screensaver::lock`]).
//!
//! ## Pure-safe Wayland path
//!
//! This uses only the safe `wayland-client` /
//! `wayland-protocols` (`staging`) APIs — no `unsafe`, inheriting the
//! workspace `unsafe_code = "forbid"`. It opens its **own** `wayland-client`
//! connection via `Connection::connect_to_env()` (independent of GTK's own
//! backend — a plain idle client needs no GTK surface) and drives the event
//! queue with `blocking_dispatch` on a dedicated `std::thread`. The Wayland
//! objects are `!Send`, so they live entirely on that thread; only the
//! `Mutable<IdleState>` (which is `Send + Sync`) is written from it, matching
//! the reactive core's handle/work split.
//!
//! ## Resilience (#431)
//!
//! This module is the **only** dim/lock/suspend path in the system, and lock
//! is a security function — so no arm of it may die permanently on a single
//! error. The observer thread reruns [`run`] with capped exponential backoff
//! whenever it exits with an error (connect failure, protocol/dispatch
//! error), resetting the published state to `Active` and restoring a pending
//! native dim before each retry. The `PrepareForSleep` relock arm is
//! supervised against panics (`spawn_supervised`) *and* re-subscribes if its
//! signal stream ever ends.

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_reactive::{Service, registry, runtime, spawn_supervised};
use std::collections::BTreeSet;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;
use zbus::zvariant::OwnedValue;

/// `tracing` target for the idle manager's logs.
const LOG_TARGET: &str = "hytte_services::idle_notify";

/// Idle thresholds (seconds): dim @ 240, lock @ 300, suspend @ 600. Each fires
/// its native action, gated on the relevant logind inhibitor.
const DIM_SECS: u32 = 240;
const LOCK_SECS: u32 = 300;
const SUSPEND_SECS: u32 = 600;

/// Every idle threshold, in ascending order — one `ext_idle_notification_v1`
/// each.
const THRESHOLDS: [u32; 3] = [DIM_SECS, LOCK_SECS, SUSPEND_SECS];

/// logind `Manager` on the **system** bus — source of both `BlockInhibited`
/// (the inhibitor gate for dim/lock/suspend) and the `PrepareForSleep` signal
/// that drives the before-sleep relock.
const LOGIN1_NAME: &str = "org.freedesktop.login1";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";
const LOGIN1_MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";

// ── Public data shape ─────────────────────────────────────────────────────────

/// Reactive idle state derived from the compositor's `ext-idle-notify-v1`
/// notifications.
///
/// This reactive shape is independent of the native idle actions: it reports
/// whether the seat is idle and, roughly, since when.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum IdleState {
    /// The seat is active — no idle threshold is currently fired.
    #[default]
    Active,
    /// At least one idle threshold has fired.
    Idle {
        /// The largest threshold (seconds) currently in the idled state — how
        /// deep into idle the seat is (240 dim, 300 lock, 600 suspend).
        deepest_secs: u32,
        /// Rough wall-clock time this idle cycle began (when the first
        /// threshold fired, minus that threshold's duration).
        since: DateTime<Local>,
    },
}

impl IdleState {
    /// `true` while at least one idle threshold is currently fired.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        matches!(self, IdleState::Idle { .. })
    }
}

// ── Service ───────────────────────────────────────────────────────────────────

pub struct IdleNotifyService;

#[doc(hidden)]
pub struct IdleNotifyHandles {
    pub(crate) state: Mutable<IdleState>,
}

impl Service for IdleNotifyService {
    type Handles = IdleNotifyHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let state = Mutable::new(IdleState::default());
        let worker_state = state.clone();

        // Relock on logind `PrepareForSleep(true)`, mirroring swayidle's
        // `before-sleep 'loginctl lock-session'`. This D-Bus work runs as a
        // tokio task on the shared runtime (Wayland stays on its own `!Send`
        // thread). Supervised so a panic in the signal listener restarts the
        // relock arm instead of silently disabling before-sleep locking.
        spawn_supervised("idle_notify", run_prepare_for_sleep_relock);

        // Wayland objects are `!Send`; the whole client lives on this dedicated
        // thread and only writes back the `Send + Sync` `Mutable`. A `std::thread`
        // (rather than a tokio task) keeps the blocking dispatch loop off the
        // shared runtime's worker pool. The loop inside reconnects with capped
        // backoff on any error exit — one dispatch error must not silently end
        // dim/lock/suspend for the rest of the session (#431).
        std::thread::Builder::new()
            .name("hytte-idle-notify".into())
            .spawn(move || run_observer_with_reconnect(&worker_state))
            .expect("spawn hytte-idle-notify thread");

        IdleNotifyHandles { state }
    }
}

/// Returns the idle-notify service to register with the hytte runtime.
#[must_use]
pub fn service() -> IdleNotifyService {
    IdleNotifyService
}

/// Reactive idle state from the native `ext-idle-notify-v1` manager. `Active`
/// until the first threshold fires; `Idle { .. }` while any threshold is idled.
pub fn state() -> impl Signal<Item = IdleState> {
    registry::with(|r| {
        r.get::<IdleNotifyHandles>()
            .expect("idle_notify::service() not registered")
            .state
            .signal_cloned()
    })
}

// ── Wayland client ────────────────────────────────────────────────────────────

/// Map a threshold to the idle action it drives, for logging.
fn action_name(secs: u32) -> &'static str {
    match secs {
        DIM_SECS => "dim",
        LOCK_SECS => "lock",
        SUSPEND_SECS => "suspend",
        _ => "none",
    }
}

/// Dispatch state for the idle-notify event queue. Holds the live notification
/// objects (dropping one cancels it), the set of currently-idled thresholds,
/// and the reactive sink updated on every transition.
struct IdleClient {
    /// Reactive sink, published on every idled/resumed transition.
    state: Mutable<IdleState>,
    /// Live notification objects, one per threshold. Kept alive because
    /// dropping an `ext_idle_notification_v1` cancels the subscription.
    notifications: Vec<ExtIdleNotificationV1>,
    /// Thresholds (seconds) currently in the idled state.
    idled: BTreeSet<u32>,
    /// Rough wall-clock estimate of when this idle cycle began.
    since: Option<DateTime<Local>>,
    /// `true` while a native dim is in effect (backlight saved + lowered), so
    /// resume restores exactly what it dimmed and a *skipped* dim (inhibitor
    /// held) leaves nothing to restore. Shared with the spawned action tasks
    /// — and owned by the observer's reconnect loop, so it outlives any one
    /// client incarnation (#431).
    dimmed: Arc<AtomicBool>,
}

impl IdleClient {
    fn new(state: Mutable<IdleState>, dimmed: Arc<AtomicBool>) -> Self {
        Self {
            state,
            notifications: Vec::new(),
            idled: BTreeSet::new(),
            since: None,
            dimmed,
        }
    }

    /// Handle an `idled` event for `secs`: record it, estimate the idle-since
    /// on the first firing of this cycle, log, publish, then fire the action.
    fn on_idled(&mut self, secs: u32) {
        if self.idled.is_empty() {
            // First threshold of this cycle → the seat has already been idle
            // `secs` seconds, so the cycle began roughly `secs` ago.
            self.since = Some(Local::now() - chrono::Duration::seconds(i64::from(secs)));
        }
        self.idled.insert(secs);
        tracing::info!(
            target: LOG_TARGET,
            threshold_secs = secs,
            action = action_name(secs),
            "ext-idle-notify idled"
        );
        self.publish();
        self.fire_action(secs);
    }

    /// Handle a `resumed` event for `secs`: clear it, drop the idle-since when
    /// the last threshold resumes, log, publish, and undim at the dim threshold.
    fn on_resumed(&mut self, secs: u32) {
        self.idled.remove(&secs);
        if self.idled.is_empty() {
            self.since = None;
        }
        tracing::info!(
            target: LOG_TARGET,
            threshold_secs = secs,
            action = action_name(secs),
            "ext-idle-notify resumed"
        );
        self.publish();
        if secs == DIM_SECS {
            self.restore_dim();
        }
    }

    fn publish(&self) {
        self.state.set(self.snapshot());
    }

    /// Derive the reactive state from the currently-idled thresholds.
    fn snapshot(&self) -> IdleState {
        match self.idled.iter().next_back().copied() {
            Some(deepest_secs) => IdleState::Idle {
                deepest_secs,
                since: self.since.unwrap_or_else(Local::now),
            },
            None => IdleState::Active,
        }
    }

    /// Fire the native idle action for `secs` on the shared runtime, gated on
    /// the relevant logind inhibitor(s): `idle` for dim/lock; `idle` *or*
    /// `sleep` for suspend, so "Keep awake" / a playing video's `idle`
    /// inhibitor holds off suspend too, not just dim/lock (#420). Runs off the
    /// observer thread so the Wayland dispatch loop keeps servicing events.
    fn fire_action(&self, secs: u32) {
        let dimmed = self.dimmed.clone();
        runtime::handle().spawn(async move {
            match secs {
                DIM_SECS => {
                    if inhibitor_blocks(&["idle"]).await {
                        return;
                    }
                    // `brightnessctl -s set 10%` (save current, then dim).
                    if run_command("brightnessctl", &["-s", "set", "10%"]).await {
                        dimmed.store(true, Ordering::SeqCst);
                    }
                }
                LOCK_SECS => {
                    if inhibitor_blocks(&["idle"]).await {
                        return;
                    }
                    // `loginctl lock-session` — reuse the existing path
                    // (screensaver::lock runs exactly that; do not reimplement).
                    crate::screensaver::lock();
                }
                SUSPEND_SECS => {
                    // Skip suspend while EITHER `idle` or `sleep` is inhibited:
                    // caffeine ("Keep awake") and idle-inhibiting apps hold an
                    // `idle` inhibitor, and they expect to keep the box awake —
                    // gating suspend on `sleep` alone let it suspend mid-movie
                    // (#420).
                    if inhibitor_blocks(&["idle", "sleep"]).await {
                        return;
                    }
                    run_command("systemctl", &["suspend"]).await;
                }
                _ => {}
            }
        });
    }

    /// Undim on resume (`brightnessctl -r`). Only restores when a native dim is
    /// actually in effect (a skipped dim leaves the flag clear), so a stale
    /// restore never clobbers the saved level.
    fn restore_dim(&self) {
        restore_dim_if_dimmed(&self.dimmed);
    }
}

// ── Before-sleep relock ─────────────────────────────────────────────────────

/// Should a logind `PrepareForSleep` payload trigger a before-sleep relock?
///
/// The signal carries one boolean: `true` fires *just before* the system
/// suspends/hibernates, `false` fires *after* resume. Only the pre-sleep edge
/// (`true`) should relock — exactly swayidle's `before-sleep` action; `false`
/// is a no-op. Factored out as a pure mapping so the edge logic is unit-tested
/// without a live logind.
fn should_relock_on_prepare_for_sleep(about_to_sleep: bool) -> bool {
    about_to_sleep
}

/// Subscribe to logind's `PrepareForSleep` on the **system** bus and relock the
/// session on the pre-sleep edge (`PrepareForSleep(true)`), reusing
/// [`crate::screensaver::lock`] (`loginctl lock-session`) — the native
/// replacement for swayidle's `before-sleep 'loginctl lock-session'`.
///
/// Runs as a tokio task on the shared runtime. `hytte_bus` handles connection
/// pooling and reconnection, so one `events()` receiver survives bus
/// reconnects — but if the stream itself ever *ends* (the bus-layer
/// subscription task died), a fresh subscription is built after backoff
/// rather than letting the before-sleep relock silently disappear (#431).
async fn run_prepare_for_sleep_relock() {
    let mut backoff = RetryBackoff::default();
    loop {
        let started = Instant::now();
        let sub = hytte_bus::signals(LOGIN1_NAME)
            .bus(hytte_bus::BusKind::System)
            .at_path(LOGIN1_PATH)
            .iface(LOGIN1_MANAGER_IFACE)
            .signal("PrepareForSleep")
            .start();
        let mut events = sub.events();

        tracing::info!(
            target: LOG_TARGET,
            "native before-sleep relock armed (logind PrepareForSleep(true) → screensaver::lock)"
        );

        while let Some(event) = events.next().await {
            // PrepareForSleep carries a single boolean `start`.
            match event.body.body().deserialize::<bool>() {
                Ok(about_to_sleep) => {
                    if should_relock_on_prepare_for_sleep(about_to_sleep) {
                        tracing::info!(
                            target: LOG_TARGET,
                            "logind PrepareForSleep(true) — relocking session before sleep (native before-sleep)"
                        );
                        crate::screensaver::lock();
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        error = %err,
                        "could not decode logind PrepareForSleep payload; skipping before-sleep relock"
                    );
                }
            }
        }

        // The stream only ends when the subscription's broadcast channel
        // closes — hytte-bus re-subscribes across bus reconnects internally,
        // so reaching here means its subscription task died. Losing the
        // before-sleep relock is a silent security regression; rebuild.
        let delay = backoff.next_delay(started.elapsed());
        tracing::error!(
            target: LOG_TARGET,
            retry_in_secs = delay.as_secs_f64(),
            "logind PrepareForSleep stream ended; re-subscribing after backoff"
        );
        tokio::time::sleep(delay).await;
    }
}

// ── Native idle actions ─────────────────────────────────────────────────────

/// Fail-**closed** gate decision from a `BlockInhibited` read.
///
/// `None` means the read itself failed — we then **skip** the action (`true`),
/// never firing dim/lock/suspend when we cannot confirm that nothing is
/// inhibiting. This direction is load-bearing for "Keep awake" (#534): the
/// gate must fail *toward not acting*, not toward locking. `Some(list)` does
/// whole-token membership over the colon-separated list. Pure, so the
/// fail-closed direction is unit-tested without a live bus — a regression that
/// flipped the `None` arm to `false` (fail-open, "keep-awake still locked me")
/// would trip the test.
fn should_skip_action(block_inhibited: Option<&str>, whats: &[&str]) -> bool {
    match block_inhibited {
        Some(list) => block_list_contains_any(list, whats),
        // A failed read must never allow the action.
        None => true,
    }
}

/// `true` if the matching native idle action must be **skipped** — because a
/// logind inhibitor is held, *or* because we couldn't read the inhibitor set
/// (in which case we skip to be safe; see [`should_skip_action`]). dim/lock
/// pass `["idle"]`; suspend passes `["idle", "sleep"]`, so a held `idle`
/// inhibitor ("Keep awake" / a playing video) holds off suspend as well, not
/// just dim/lock (#420). The `BlockInhibited` is read **fresh** on every fire
/// (not from a cached subscription), so there is no arm-time/fire-time
/// staleness window: the decision reflects logind's live inhibitor set at the
/// instant the threshold fires.
async fn inhibitor_blocks(whats: &[&str]) -> bool {
    let read = read_block_inhibited().await;
    let skip = should_skip_action(read.as_deref().ok(), whats);
    match &read {
        Ok(list) if skip => {
            tracing::info!(
                target: LOG_TARGET,
                ?whats,
                block_inhibited = %list,
                "native idle action skipped — logind inhibitor held"
            );
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(
                target: LOG_TARGET,
                ?whats,
                error = %err,
                "could not read logind BlockInhibited; skipping native idle action to be safe"
            );
        }
    }
    skip
}

/// Read `org.freedesktop.login1.Manager.BlockInhibited` (a colon-separated list
/// like `"idle:sleep"`) via `hytte-bus` on the **system** bus. `Properties.Get`
/// always replies a `Variant` (`v`), so deserialize `OwnedValue` then unwrap to
/// `String`.
async fn read_block_inhibited() -> Result<String, hytte_bus::BusError> {
    let v: OwnedValue = hytte_bus::call(LOGIN1_NAME)
        .bus(hytte_bus::BusKind::System)
        .at_path(LOGIN1_PATH)
        .iface("org.freedesktop.DBus.Properties")
        .method("Get")
        .args((LOGIN1_MANAGER_IFACE, "BlockInhibited"))
        .send::<OwnedValue>()
        .await?;
    String::try_from(v).map_err(|_| hytte_bus::BusError::Permanent {
        reason: "login1 BlockInhibited was not a string".to_string(),
        dbus_name: None,
    })
}

/// Does the colon-separated logind `BlockInhibited` string contain `what`?
/// e.g. `block_list_contains("idle:sleep", "idle") == true`.
fn block_list_contains(block_inhibited: &str, what: &str) -> bool {
    block_inhibited.split(':').any(|part| part == what)
}

/// Does the colon-separated logind `BlockInhibited` string contain **any** of
/// `whats`? The suspend gate passes `["idle", "sleep"]`, so it skips when
/// *either* is inhibited — e.g. `block_list_contains_any("idle", &["idle",
/// "sleep"]) == true` (an `idle`-only inhibitor now holds off suspend, #420).
fn block_list_contains_any(block_inhibited: &str, whats: &[&str]) -> bool {
    whats
        .iter()
        .any(|w| block_list_contains(block_inhibited, w))
}

/// Run a fire-and-forget command off the async worker via `spawn_blocking`
/// (mirroring `wallpaper.rs`), returning whether it exited successfully. stdio
/// is silenced; failures are logged, never propagated.
async fn run_command(program: &'static str, args: &'static [&'static str]) -> bool {
    let status = runtime::handle()
        .spawn_blocking(move || {
            std::process::Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        })
        .await;
    match status {
        Ok(Ok(s)) if s.success() => true,
        Ok(Ok(s)) => {
            tracing::warn!(target: LOG_TARGET, program, ?args, code = ?s.code(), "native idle action command exited non-zero");
            false
        }
        Ok(Err(e)) => {
            tracing::warn!(target: LOG_TARGET, program, ?args, error = %e, "failed to spawn native idle action command");
            false
        }
        Err(e) => {
            tracing::warn!(target: LOG_TARGET, program, ?args, error = %e, "native idle action command task join failed");
            false
        }
    }
}

// ── Observer resilience (#431) ──────────────────────────────────────────────

/// Capped-exponential-backoff schedule for the observer's reconnects (and the
/// relock arm's re-subscribes): 1 s → 2 s → … → 30 s cap, resetting to 1 s
/// after a run that stayed healthy for ≥ 30 s. Deliberately the same schedule
/// as `hytte_reactive::spawn_supervised` — this is its `std::thread` /
/// error-return sibling (the supervisor only guards *panics* in tokio tasks).
struct RetryBackoff {
    /// Delay before the next restart; doubles per consecutive failure.
    delay: Duration,
    /// Delay after a healthy run (and the schedule's starting point).
    initial: Duration,
    /// Ceiling the delay is clamped to as it doubles.
    max: Duration,
    /// A run that lived at least this long resets the schedule to `initial`,
    /// so an isolated failure after a long healthy stretch retries promptly
    /// instead of inheriting an accumulated delay from an earlier flap.
    reset_after: Duration,
}

impl Default for RetryBackoff {
    fn default() -> Self {
        Self {
            delay: Duration::from_secs(1),
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            reset_after: Duration::from_secs(30),
        }
    }
}

impl RetryBackoff {
    /// The delay to wait before the next restart, given how long the run that
    /// just failed stayed up. Pure bookkeeping — the caller does the sleeping
    /// — so the schedule is unit-testable without clocks.
    fn next_delay(&mut self, ran: Duration) -> Duration {
        if ran >= self.reset_after {
            self.delay = self.initial;
        }
        let delay = self.delay;
        self.delay = self.delay.saturating_mul(2).min(self.max);
        delay
    }
}

/// Restore the saved backlight level iff a native dim is currently in effect.
/// The flag is swapped **synchronously** (so callers observe it cleared on
/// return); only the `brightnessctl -r` itself is fire-and-forget on the
/// shared runtime. The flag guard is what keeps a stale restore from
/// clobbering a manually-set brightness.
fn restore_dim_if_dimmed(dimmed: &Arc<AtomicBool>) {
    if dimmed.swap(false, Ordering::SeqCst) {
        runtime::handle().spawn(async {
            run_command("brightnessctl", &["-r"]).await;
        });
    }
}

/// Recover the observable side effects of a dead observer incarnation before
/// retrying: reset the published state to `Active` (whatever `Idle { .. }` it
/// last set may otherwise stay frozen forever) and restore a still-in-effect
/// native dim (the seat may resume while the observer is down, in which case
/// no `resumed` event ever reaches the *next* incarnation's fresh
/// notification objects — the backlight would stay stuck at 10%).
fn reset_after_observer_error(state: &Mutable<IdleState>, dimmed: &Arc<AtomicBool>) {
    state.set(IdleState::Active);
    restore_dim_if_dimmed(dimmed);
}

/// Drive [`run`] forever on the dedicated observer thread, reconnecting with
/// capped exponential backoff whenever it exits with an error (compositor
/// restart, protocol/dispatch error, connect failure) instead of dying on the
/// first one — this thread is the only thing standing between "idle" and
/// "never dims/locks/suspends" (#431). Each error exit first runs
/// [`reset_after_observer_error`]. A clean return means the compositor
/// advertises no `ext_idle_notifier_v1` at all — retrying cannot change that,
/// so the manager stays off (already logged inside [`run`]).
fn run_observer_with_reconnect(state: &Mutable<IdleState>) {
    let dimmed = Arc::new(AtomicBool::new(false));
    let mut backoff = RetryBackoff::default();
    loop {
        let started = Instant::now();
        match run(state.clone(), dimmed.clone()) {
            Ok(()) => return,
            Err(err) => {
                reset_after_observer_error(state, &dimmed);
                let delay = backoff.next_delay(started.elapsed());
                tracing::error!(
                    target: LOG_TARGET,
                    error = %err,
                    retry_in_secs = delay.as_secs_f64(),
                    "native idle-notify observer failed; reconnecting after backoff (dim/lock/suspend paused until then)"
                );
                std::thread::sleep(delay);
            }
        }
    }
}

/// Open an independent Wayland connection, bind `ext_idle_notifier_v1`, arm a
/// notification per threshold, then dispatch events forever. Runs on the
/// dedicated observer thread; returns only on a fatal Wayland error — which
/// [`run_observer_with_reconnect`] retries with backoff — or `Ok` when the
/// compositor advertises no idle-notifier at all.
fn run(state: Mutable<IdleState>, dimmed: Arc<AtomicBool>) -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("connect to the Wayland compositor (is WAYLAND_DISPLAY set?)")?;
    let (globals, mut queue) =
        registry_queue_init::<IdleClient>(&conn).context("initialise the Wayland registry")?;
    let qh = queue.handle();

    // A seat is required to create idle notifications; we only use it as an
    // opaque handle, so v1 is enough and its events are ignored.
    let seat: WlSeat = globals
        .bind(&qh, 1..=1, ())
        .context("bind wl_seat (no seat advertised?)")?;

    // Graceful no-op if the compositor lacks the global (non-niri / older niri):
    // the idle timeline simply doesn't run.
    let notifier: ExtIdleNotifierV1 = match globals.bind(&qh, 1..=1, ()) {
        Ok(notifier) => notifier,
        Err(err) => {
            tracing::warn!(
                target: LOG_TARGET,
                error = %err,
                "compositor does not advertise ext_idle_notifier_v1; native idle manager disabled (no dim/lock/suspend)"
            );
            return Ok(());
        }
    };

    let mut client = IdleClient::new(state, dimmed);
    for &secs in &THRESHOLDS {
        // Protocol timeout is in milliseconds; the notification's user-data is
        // the threshold in seconds so the event handler can identify it.
        let timeout_ms = secs * 1000;
        let notification = notifier.get_idle_notification(timeout_ms, &seat, &qh, secs);
        client.notifications.push(notification);
    }
    conn.flush().context("flush idle-notification requests")?;

    tracing::info!(
        target: LOG_TARGET,
        thresholds_secs = ?THRESHOLDS,
        "native ext-idle-notify-v1 idle manager armed (dim@240/lock@300/suspend@600, gated on logind inhibitors)"
    );

    loop {
        queue
            .blocking_dispatch(&mut client)
            .context("dispatch Wayland idle-notify events")?;
    }
}

// ── Dispatch impls ────────────────────────────────────────────────────────────

// `registry_queue_init` drives the registry globals through this impl; we only
// need it to exist.
impl Dispatch<WlRegistry, GlobalListContents> for IdleClient {
    fn event(
        _state: &mut Self,
        _registry: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// The seat is used only as a handle for `get_idle_notification`; ignore its
// `capabilities` / `name` events.
impl Dispatch<WlSeat, ()> for IdleClient {
    fn event(
        _state: &mut Self,
        _seat: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// `ext_idle_notifier_v1` emits no events; the impl is required to bind it.
impl Dispatch<ExtIdleNotifierV1, ()> for IdleClient {
    fn event(
        _state: &mut Self,
        _notifier: &ExtIdleNotifierV1,
        _event: <ExtIdleNotifierV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// The real work: `idled` / `resumed` per notification, keyed by its threshold
// (the `u32` user-data).
impl Dispatch<ExtIdleNotificationV1, u32> for IdleClient {
    fn event(
        state: &mut Self,
        _notification: &ExtIdleNotificationV1,
        event: <ExtIdleNotificationV1 as Proxy>::Event,
        threshold_secs: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let secs = *threshold_secs;
        match event {
            ext_idle_notification_v1::Event::Idled => state.on_idled(secs),
            ext_idle_notification_v1::Event::Resumed => state.on_resumed(secs),
            // Event enum is `#[non_exhaustive]`; ignore any future variants.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_name_maps_known_thresholds() {
        assert_eq!(action_name(240), "dim");
        assert_eq!(action_name(300), "lock");
        assert_eq!(action_name(600), "suspend");
        assert_eq!(action_name(42), "none");
    }

    #[test]
    fn snapshot_tracks_deepest_active_threshold() {
        let mut client = IdleClient::new(
            Mutable::new(IdleState::default()),
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(client.snapshot(), IdleState::Active);

        client.on_idled(240);
        assert!(client.snapshot().is_idle());
        match client.snapshot() {
            IdleState::Idle { deepest_secs, .. } => assert_eq!(deepest_secs, 240),
            IdleState::Active => panic!("expected idle after 240 fired"),
        }

        // A deeper threshold deepens the reported state.
        client.on_idled(300);
        match client.snapshot() {
            IdleState::Idle { deepest_secs, .. } => assert_eq!(deepest_secs, 300),
            IdleState::Active => panic!("expected idle after 300 fired"),
        }

        // Resuming the deepest falls back to the shallower one still idled.
        client.on_resumed(300);
        match client.snapshot() {
            IdleState::Idle { deepest_secs, .. } => assert_eq!(deepest_secs, 240),
            IdleState::Active => panic!("expected idle while 240 still fired"),
        }

        // Resuming the last active threshold returns to Active.
        client.on_resumed(240);
        assert_eq!(client.snapshot(), IdleState::Active);
    }

    #[test]
    fn block_list_contains_matches_whole_tokens() {
        // The primitive the safety gate turns on: whole-token membership in the
        // colon-separated `BlockInhibited` list.
        assert!(block_list_contains("idle:sleep", "idle"));
        assert!(block_list_contains("idle:sleep", "sleep"));
        assert!(block_list_contains("idle", "idle"));
        assert!(block_list_contains("handle-power-key:idle:sleep", "idle"));

        // Absent inhibitor → do not skip.
        assert!(!block_list_contains("sleep", "idle"));
        assert!(!block_list_contains("", "idle"));
        assert!(!block_list_contains("handle-lid-switch", "sleep"));
        // Whole-token match only: no substring false positives.
        assert!(!block_list_contains("idlehint", "idle"));
    }

    #[test]
    fn gate_fails_closed_on_read_error() {
        // #534: a failed `BlockInhibited` read must SKIP the action
        // (fail-closed) — never fire dim/lock/suspend when we can't confirm
        // nothing inhibits. This pins the exact direction a "keep-awake still
        // locked me" regression would flip: the `None` (read-failed) arm to
        // `false`. Fail-open here is the difference between an idle box that
        // stays awake and one that locks despite the hold.
        assert!(should_skip_action(None, &["idle"]));
        assert!(should_skip_action(None, &["idle", "sleep"]));
    }

    #[test]
    fn gate_decision_matches_live_inhibitor_set() {
        // Successful read, relevant inhibitor absent → do NOT skip (fire).
        assert!(!should_skip_action(Some(""), &["idle"]));
        assert!(!should_skip_action(Some("handle-power-key"), &["idle"]));
        // Relevant inhibitor present → skip (this is the keep-awake hold).
        assert!(should_skip_action(Some("idle"), &["idle"]));
        assert!(should_skip_action(
            Some("handle-power-key:idle:sleep"),
            &["idle"]
        ));
        // Suspend gate skips on either `idle` or `sleep`.
        assert!(should_skip_action(Some("idle"), &["idle", "sleep"]));
        assert!(should_skip_action(Some("sleep"), &["idle", "sleep"]));
        assert!(!should_skip_action(
            Some("handle-lid-switch"),
            &["idle", "sleep"]
        ));
    }

    #[test]
    fn suspend_gate_skips_on_idle_or_sleep() {
        // #420: the suspend action gates on `["idle", "sleep"]`, so an
        // `idle`-only inhibitor (caffeine "Keep awake" / a playing video) now
        // holds off the 600 s suspend too — it used to gate on `sleep` alone
        // and suspend mid-movie.
        let suspend_gate = &["idle", "sleep"];

        // Skipped when `idle` alone is inhibited — the papercut this fixes.
        assert!(block_list_contains_any("idle", suspend_gate));
        // Still skipped when `sleep` is inhibited (unchanged behavior).
        assert!(block_list_contains_any("sleep", suspend_gate));
        // Skipped when both are present, in any position.
        assert!(block_list_contains_any("idle:sleep", suspend_gate));
        assert!(block_list_contains_any(
            "handle-power-key:idle:sleep",
            suspend_gate
        ));

        // Fires (not skipped) when NEITHER `idle` nor `sleep` is inhibited.
        assert!(!block_list_contains_any("", suspend_gate));
        assert!(!block_list_contains_any("handle-power-key", suspend_gate));
        assert!(!block_list_contains_any(
            "handle-lid-switch:handle-power-key",
            suspend_gate
        ));
        // Whole-token match only: no substring false positives.
        assert!(!block_list_contains_any("idlehint", suspend_gate));
    }

    #[test]
    fn dim_lock_gate_ignores_sleep() {
        // dim/lock gate on `["idle"]` only: a `sleep`-only inhibitor must not
        // hold them off (only the suspend gate widened to include `idle`).
        let dim_lock_gate = &["idle"];
        assert!(block_list_contains_any("idle", dim_lock_gate));
        assert!(!block_list_contains_any("sleep", dim_lock_gate));
        assert!(!block_list_contains_any("", dim_lock_gate));
    }

    #[test]
    fn prepare_for_sleep_relocks_only_before_sleep() {
        // logind PrepareForSleep(true) fires just before suspend → relock
        // (mirrors swayidle's `before-sleep`); PrepareForSleep(false) fires on
        // resume → no-op.
        assert!(should_relock_on_prepare_for_sleep(true));
        assert!(!should_relock_on_prepare_for_sleep(false));
    }

    #[test]
    fn retry_backoff_doubles_then_caps() {
        // #431: consecutive fast failures (e.g. compositor down, connect
        // refused) double the delay up to the 30 s cap — never further, and
        // never a permanent stop.
        let mut backoff = RetryBackoff::default();
        let crashed_instantly = Duration::ZERO;
        assert_eq!(
            backoff.next_delay(crashed_instantly),
            Duration::from_secs(1)
        );
        assert_eq!(
            backoff.next_delay(crashed_instantly),
            Duration::from_secs(2)
        );
        assert_eq!(
            backoff.next_delay(crashed_instantly),
            Duration::from_secs(4)
        );
        assert_eq!(
            backoff.next_delay(crashed_instantly),
            Duration::from_secs(8)
        );
        assert_eq!(
            backoff.next_delay(crashed_instantly),
            Duration::from_secs(16)
        );
        assert_eq!(
            backoff.next_delay(crashed_instantly),
            Duration::from_secs(30)
        );
        // Capped: stays at 30 s, does not keep doubling.
        assert_eq!(
            backoff.next_delay(crashed_instantly),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn retry_backoff_resets_after_healthy_run() {
        // A run that stayed up ≥ reset_after (30 s) is healthy: the next
        // failure retries promptly at 1 s instead of inheriting the
        // accumulated delay from an earlier flap.
        let mut backoff = RetryBackoff::default();
        let crashed_instantly = Duration::ZERO;
        backoff.next_delay(crashed_instantly); // 1 s
        backoff.next_delay(crashed_instantly); // 2 s
        backoff.next_delay(crashed_instantly); // 4 s (next would be 8 s)
        assert_eq!(
            backoff.next_delay(Duration::from_secs(30)),
            Duration::from_secs(1)
        );
        // …and the doubling starts over from there.
        assert_eq!(
            backoff.next_delay(crashed_instantly),
            Duration::from_secs(2)
        );
        // Just under the healthy threshold does NOT reset.
        let mut backoff = RetryBackoff::default();
        backoff.next_delay(crashed_instantly); // 1 s
        assert_eq!(
            backoff.next_delay(Duration::from_secs(29)),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn observer_error_reset_publishes_active() {
        // #431: when the observer dies mid-idle-cycle, whatever `Idle { .. }`
        // it last published must not stay frozen across the reconnect gap.
        // (dimmed = false here so the no-dim path spawns no restore command —
        // the dimmed = true path would exec a real `brightnessctl -r`.)
        let state = Mutable::new(IdleState::Idle {
            deepest_secs: LOCK_SECS,
            since: Local::now(),
        });
        let dimmed = Arc::new(AtomicBool::new(false));
        reset_after_observer_error(&state, &dimmed);
        assert_eq!(state.get_cloned(), IdleState::Active);
        assert!(!dimmed.load(Ordering::SeqCst));
    }
}
