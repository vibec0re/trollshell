//! Native `ext-idle-notify-v1` idle manager (#204).
//!
//! trollshell owns the idle → dim → lock → suspend timeline in-process — it is
//! the idle daemon, replacing swayidle entirely. It binds the compositor's
//! `ext_idle_notifier_v1` global and creates one `ext_idle_notification_v1` per
//! threshold (240 dim / 300 lock / 600 suspend), exposes "idle since / resumed"
//! reactively via [`state`], and fires the three actions natively at their
//! thresholds:
//!
//! - **dim** — `brightnessctl -s set 10%` (saved level restored on resume —
//!   including a resume that lands while the dim is still in flight, which
//!   the dim then undoes itself; see `DimLatch`),
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
//! error. The observer thread reruns `run` with capped exponential backoff
//! whenever it exits with an error (connect failure, protocol/dispatch
//! error), resetting the published state to `Active` and restoring a pending
//! native dim before each retry. The `PrepareForSleep` relock arm is
//! supervised against panics (`spawn_supervised`) *and* re-subscribes if its
//! signal stream ever ends.

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_reactive::{
    Service, registry, runtime, spawn_supervised, spawn_supervised_blocking_bounded,
};
use std::collections::BTreeSet;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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

        // Wayland objects are `!Send`; the whole client lives on its own
        // blocking thread and only writes back the `Send + Sync` `Mutable`.
        // The loop inside reconnects with capped backoff on any error exit —
        // one dispatch error must not silently end dim/lock/suspend for the
        // rest of the session (#431).
        //
        // Supervised since #1170: #431 covered the *error* exit, not a panic,
        // and a panic under `run` — `wayland-client` dispatch over
        // compositor-supplied events — killed the thread outright and with it
        // the whole idle → dim → lock → suspend pipeline, silently, for the
        // session.
        //
        // **What a restart re-does:** a fresh Wayland connection, a re-bound
        // `ext_idle_notifier_v1`, and three fresh `ext_idle_notification_v1`
        // objects (dim/lock/suspend). Dropping the old ones cancels them, so
        // there is no double-arming, and the compositor — not this process —
        // owns the idle timer being re-observed. It also re-runs
        // `reset_after_observer_error`, which is why the dim latch is created
        // out here: see `spawn_observer`.
        spawn_observer(worker_state);

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
    /// Whether a native dim is in effect (backlight saved + lowered), so
    /// resume restores exactly what it dimmed and a *skipped* dim (inhibitor
    /// held) leaves nothing to restore — plus the resume count that lets a dim
    /// still in flight see its cycle already ended ([`DimLatch`]). Shared with
    /// the spawned dim tasks — and owned by the observer's reconnect loop, so
    /// it outlives any one client incarnation (#431).
    dim: Arc<DimLatch>,
}

impl IdleClient {
    fn new(state: Mutable<IdleState>, dim: Arc<DimLatch>) -> Self {
        Self {
            state,
            notifications: Vec::new(),
            idled: BTreeSet::new(),
            since: None,
            dim,
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
    ///
    /// Only the dim arm checks for a `resumed` that lands while it awaits
    /// ([`dim_unless_resumed`]), because only the dim has a resume action that
    /// such a resume could skip. Lock and suspend keep the plain gate-then-act
    /// shape: a resume inside their gate read costs a lock the user then
    /// dismisses (the security-safe direction) or a suspend issued just after
    /// activity resumed — never state left stuck behind an active user.
    fn fire_action(&self, secs: u32) {
        let dim = Arc::clone(&self.dim);
        // The dim's cycle is read **here**, on the observer thread, in event
        // order. Read inside the task instead, a `resumed` dispatched before
        // the task's first poll would already be counted, and the task would
        // take an ended cycle for its own (see `DimLatch::generation`).
        let generation = dim.generation();
        runtime::handle().spawn(async move {
            match secs {
                DIM_SECS => {
                    dim_unless_resumed(&dim, generation, inhibitor_blocks(&["idle"]), |args| {
                        run_command("brightnessctl", args)
                    })
                    .await;
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
    /// restore never clobbers the saved level — and ends the dim's cycle, so a
    /// dim still in flight undoes itself rather than landing after this.
    fn restore_dim(&self) {
        restore_dim_if_dimmed(&self.dim);
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
        let sub = hytte_bus::signals(hytte_bus::BusKind::System, LOGIN1_NAME)
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
    let v: OwnedValue = hytte_bus::call(hytte_bus::BusKind::System, LOGIN1_NAME)
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

// ── The dim against its own resume ──────────────────────────────────────────

/// `brightnessctl` arguments for the dim: `-s` saves the current level first,
/// which is what [`RESTORE_ARGS`] puts back.
const DIM_ARGS: &[&str] = &["-s", "set", "10%"];

/// `brightnessctl` arguments for the undim: restore the level `-s` saved.
const RESTORE_ARGS: &[&str] = &["-r"];

/// The native dim's state, shared by the two sides that race over it: the
/// observer thread, which dispatches `resumed`, and the dim task
/// [`IdleClient::fire_action`] spawns, which awaits the logind gate and then
/// `brightnessctl -s set 10%`.
///
/// Before this the shared state was a bare `dimmed` flag the task set only
/// *after* both awaits. A `resumed` dispatched inside either one read the flag
/// clear and restored nothing; the dim then landed, set the flag, and nothing
/// restored it — the screen stayed at 10% under an active user, and the next
/// cycle's `-s` saved that 10% as the level to restore, so it stayed there
/// for good. The window is the gate's D-Bus round trip plus the
/// `brightnessctl` exec: milliseconds normally, and up to `hytte_bus::call`'s
/// 25 s timeout and its one retry when logind is slow to answer.
///
/// Two fields under one lock close it:
///
/// - `resumes` counts the dim threshold's resumes. A dim task carries the
///   count from the moment its `idled` was dispatched
///   ([`DimLatch::generation`]), so after each await it can tell whether its
///   own idle cycle already ended.
/// - `dimmed` is set only by a dim whose cycle is still current
///   ([`DimLatch::applied`]), and taken — in the same critical section as the
///   bump — by [`DimLatch::resume`].
///
/// Why that is enough: the dim's last step and a resume are each one critical
/// section on the same lock, so exactly one of them comes first. If the resume
/// does, the dim sees the count moved, leaves `dimmed` clear and undoes itself
/// ([`Applied::RestoreNow`]); if the dim does, `dimmed` is set and the resume
/// takes it and restores. Either way a dim that lands gets exactly one
/// `brightnessctl -r` and a dim that does not gets none. A resume during the
/// gate read is caught one step earlier, by [`DimLatch::may_dim`], so the
/// screen never dips at all. It is a `std` `Mutex` held for a field update and
/// never across an await, which is what lets the observer thread take it from
/// inside Wayland dispatch.
///
/// The saved-original half: [`DimLatch::may_dim`] also refuses while a dim is
/// in effect, so `-s` never runs over this module's own 10%, and a dimmed
/// level cannot become the one `-r` restores.
///
/// An observer reset ([`reset_after_observer_error`]) ends the cycle through
/// [`DimLatch::resume`] too — a reset stands in for a resume nobody saw — so a
/// dim still in flight from a dead incarnation is undone the same way instead
/// of landing on a seat the next incarnation's fresh notifications will never
/// report resumed.
#[derive(Default)]
struct DimLatch {
    state: Mutex<DimState>,
}

/// What [`DimLatch`] guards.
#[derive(Default)]
struct DimState {
    /// How many times the dim threshold's idle cycle has ended.
    resumes: u64,
    /// `true` while a native dim is in effect (backlight saved + lowered).
    dimmed: bool,
}

/// What [`DimLatch::applied`] tells the dim task to do once
/// `brightnessctl -s set 10%` has succeeded.
#[derive(Debug, PartialEq, Eq)]
enum Applied {
    /// The cycle is still current: the dim is now in effect, and the cycle's
    /// resume restores it.
    InEffect,
    /// The cycle resumed while `brightnessctl` ran. That resume found no dim
    /// in effect and restored nothing, so the dim task owes the `-r` itself.
    RestoreNow,
}

impl DimLatch {
    fn lock(&self) -> MutexGuard<'_, DimState> {
        // Nothing panics while this is held (field updates only), but a
        // poisoned latch must not take the dim/restore path down with it.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The idle cycle a dim fired now belongs to. Read on the observer thread
    /// while `idled` is dispatched, never inside the spawned task: a `resumed`
    /// dispatched before the task's first poll would otherwise already be
    /// counted, and the task would take an ended cycle for its own.
    fn generation(&self) -> u64 {
        self.lock().resumes
    }

    /// May the dim fired in `generation` run `brightnessctl -s set 10%` now?
    /// Not if its cycle already resumed (the gate read outlasted the idle), and
    /// not while a dim is in effect — a second `-s` would save the 10% as the
    /// level `-r` restores.
    fn may_dim(&self, generation: u64) -> bool {
        let state = self.lock();
        state.resumes == generation && !state.dimmed
    }

    /// Record that the dim fired in `generation` has lowered the backlight, and
    /// say whether it stays down ([`Applied::InEffect`]) or must be undone now
    /// because its cycle resumed while `brightnessctl` ran.
    fn applied(&self, generation: u64) -> Applied {
        let mut state = self.lock();
        if state.resumes == generation {
            state.dimmed = true;
            Applied::InEffect
        } else {
            Applied::RestoreNow
        }
    }

    /// The dim threshold resumed (or an observer reset stands in for it): end
    /// the current cycle and take the dim in effect, if any. `true` means the
    /// caller owes the backlight one `brightnessctl -r`.
    fn resume(&self) -> bool {
        let mut state = self.lock();
        state.resumes += 1;
        std::mem::take(&mut state.dimmed)
    }

    /// Whether a native dim is in effect — what the tests assert on.
    #[cfg(test)]
    fn is_dimmed(&self) -> bool {
        self.lock().dimmed
    }
}

/// The dim arm of [`IdleClient::fire_action`]: read the gate, dim, and — if
/// the cycle resumed while either was awaited — skip the dim or undo it (see
/// [`DimLatch`] for why each check sits where it does).
///
/// `gate` is the logind read ([`inhibitor_blocks`]; `true` = skip) and `run`
/// one `brightnessctl` invocation (`true` = it exited 0). Both are injected so
/// the tests can land a resume inside either await with no bus and no
/// backlight. The `-r` owed by [`Applied::RestoreNow`] runs here, in sequence
/// after the `-s set`, so the two can never reach `brightnessctl` out of order.
async fn dim_unless_resumed<F>(
    dim: &DimLatch,
    generation: u64,
    gate: impl Future<Output = bool>,
    run: impl Fn(&'static [&'static str]) -> F,
) where
    F: Future<Output = bool>,
{
    if gate.await {
        return;
    }
    if !dim.may_dim(generation) {
        tracing::info!(
            target: LOG_TARGET,
            "native dim dropped — the seat resumed while the inhibitor gate was read (or a dim is already in effect)"
        );
        return;
    }
    if !run(DIM_ARGS).await {
        return;
    }
    if dim.applied(generation) == Applied::RestoreNow {
        tracing::info!(
            target: LOG_TARGET,
            "the seat resumed while the native dim ran; restoring the backlight now"
        );
        run(RESTORE_ARGS).await;
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

/// End the dim threshold's idle cycle and restore the saved backlight level
/// iff a native dim is currently in effect ([`DimLatch::resume`]). The latch
/// is updated **synchronously** (so callers observe it cleared on return);
/// only the `brightnessctl -r` itself is fire-and-forget on the shared
/// runtime. The flag guard is what keeps a stale restore from clobbering a
/// manually-set brightness; the cycle bump is what makes a dim still in flight
/// undo itself instead of landing after this returns.
fn restore_dim_if_dimmed(dim: &DimLatch) {
    if dim.resume() {
        runtime::handle().spawn(async {
            run_command("brightnessctl", RESTORE_ARGS).await;
        });
    }
}

/// Recover the observable side effects of a dead observer incarnation before
/// retrying: reset the published state to `Active` (whatever `Idle { .. }` it
/// last set may otherwise stay frozen forever) and restore a still-in-effect
/// native dim (the seat may resume while the observer is down, in which case
/// no `resumed` event ever reaches the *next* incarnation's fresh
/// notification objects — the backlight would stay stuck at 10%). It ends the
/// dim's cycle as a resume would, so a dim still in flight from the dead
/// incarnation undoes itself rather than landing after the reset.
fn reset_after_observer_error(state: &Mutable<IdleState>, dim: &DimLatch) {
    state.set(IdleState::Active);
    restore_dim_if_dimmed(dim);
}

/// Start the observer on a supervised blocking thread (#1170).
///
/// The dim latch is created **here**, outside the supervised closure, so it
/// survives a restart. That is the one piece of state a panic could otherwise
/// corrupt in a way a restart would not repair: `run` unwinding between
/// "backlight saved and lowered" and "resumed event restores it" would leave
/// the screen at 10% with the next incarnation's fresh latch having nothing to
/// restore — exactly the stuck-backlight failure `reset_after_observer_error`
/// was added for on the error path (#431). Every run therefore begins by
/// calling it, so a panic-restart cleans up precisely as an error-reconnect
/// does. On the first run it is a no-op (nothing published, nothing dimmed).
///
/// Split out from `Service::start` so the test below can supervise a body that
/// panics without needing a compositor.
fn spawn_observer(state: Mutable<IdleState>) {
    let dim = Arc::new(DimLatch::default());
    supervise_observer("idle-notify-observer", state, dim, |state, dim| {
        run_observer_with_reconnect(state, dim);
    });
}

/// [`spawn_observer`] with the name and the loop body injected, so the restart
/// contract — the same dim latch across runs, and the reset on every entry —
/// can be asserted against a body the test controls.
///
/// Supervised via [`spawn_supervised_blocking_bounded`] (#1196), not the plain
/// `spawn_supervised_blocking`: `run_observer_with_reconnect`'s doc comment
/// spells out its one designed return (no `ext_idle_notifier_v1` on this
/// compositor, which retrying cannot fix), and that return is exactly the case
/// the bounded variant exists for — `debug!` and the health row released,
/// instead of a `warn!` and a permanent `Returned` row on every host without
/// the protocol.
fn supervise_observer<F>(name: &'static str, state: Mutable<IdleState>, dim: Arc<DimLatch>, body: F)
where
    F: Fn(&Mutable<IdleState>, &Arc<DimLatch>) + Send + Sync + 'static,
{
    spawn_supervised_blocking_bounded(name, move || {
        reset_after_observer_error(&state, &dim);
        body(&state, &dim);
    });
}

/// Drive [`run`] forever on the dedicated observer thread, reconnecting with
/// capped exponential backoff whenever it exits with an error (compositor
/// restart, protocol/dispatch error, connect failure) instead of dying on the
/// first one — this thread is the only thing standing between "idle" and
/// "never dims/locks/suspends" (#431). Each error exit first runs
/// [`reset_after_observer_error`].
///
/// **A clean return is a real outcome here**, unlike the other three supervised
/// bodies #1170 touched: it means the compositor advertises no
/// `ext_idle_notifier_v1` at all, which retrying cannot change, so the manager
/// stays off (already logged inside [`run`]). On such a host the supervisor
/// stops the task and its health row goes away — a correct absence from the
/// Stats drawer, not a missing subsystem.
fn run_observer_with_reconnect(state: &Mutable<IdleState>, dim: &Arc<DimLatch>) {
    let mut backoff = RetryBackoff::default();
    loop {
        let started = Instant::now();
        match run(state.clone(), Arc::clone(dim)) {
            Ok(()) => return,
            Err(err) => {
                reset_after_observer_error(state, dim);
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
fn run(state: Mutable<IdleState>, dim: Arc<DimLatch>) -> Result<()> {
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

    let mut client = IdleClient::new(state, dim);
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
    use futures_executor::block_on;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// #1170's item 2 for the observer thread: panic a run and the next one
    /// starts — *and* starts clean.
    ///
    /// The second assertion is the one worth having. #431 established that a
    /// dead observer incarnation must have its side effects undone before the
    /// next one runs (published state back to `Active`, a native dim restored),
    /// because the seat may resume while nobody is listening and the fresh
    /// notification objects will never deliver the `resumed` that would restore
    /// the backlight. A panic is exactly that situation, so the restart has to
    /// run the same cleanup the error path does.
    ///
    /// Falsify either half: drop `reset_after_observer_error` from
    /// `supervise_observer` and the second run sees a dimmed, still-`Idle`
    /// world (the screen stays at 10% with nothing left to restore it); swap
    /// `spawn_supervised_blocking_bounded` back to `std::thread::spawn` and
    /// there is no second run to look at.
    #[test]
    fn a_panicking_observer_restarts_with_the_dim_flag_reset() {
        const NAME: &str = "test-idle-observer-restart";

        let state = Mutable::new(IdleState::default());
        let dim = Arc::new(DimLatch::default());
        let runs = Arc::new(AtomicUsize::new(0));
        // What run 2 found on entry, so the assertions read a snapshot rather
        // than racing the live handles.
        let second: Arc<Mutex<Option<(bool, IdleState)>>> = Arc::new(Mutex::new(None));

        supervise_observer(NAME, state.clone(), Arc::clone(&dim), {
            let runs = Arc::clone(&runs);
            let second = Arc::clone(&second);
            move |state, dim| {
                if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                    // Mid-dim, mid-idle — the exact moment #431 cares about.
                    let generation = dim.generation();
                    assert!(dim.may_dim(generation));
                    assert_eq!(dim.applied(generation), Applied::InEffect);
                    state.set(IdleState::Idle {
                        deepest_secs: DIM_SECS,
                        since: Local::now(),
                    });
                    panic!("{NAME}: first run panics, deliberately");
                }
                *second
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some((dim.is_dimmed(), state.get_cloned()));
                // Park so the supervisor stays live and keeps its health row.
                std::thread::sleep(Duration::from_secs(30));
            }
        });

        let deadline = Instant::now() + Duration::from_secs(15);
        let observed = loop {
            let seen = second
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if seen.is_some() {
                break seen;
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        let (was_dimmed, seen_state) =
            observed.expect("the observer never restarted after its panic");
        assert!(
            !was_dimmed,
            "the restarted observer inherited a set dim flag, so the backlight stays at 10% \
             with nothing left to restore it (#431)"
        );
        assert_eq!(
            seen_state,
            IdleState::Active,
            "the restarted observer inherited a frozen Idle state"
        );

        let health = hytte_reactive::health::snapshot()
            .into_iter()
            .find(|h| h.name == NAME)
            .expect("the supervisor publishes a live health row");
        assert_eq!(health.panics, 1, "the restart is not on the health record");
    }

    /// #1196: the observer's clean return — no `ext_idle_notifier_v1` on this
    /// compositor — is designed, not a bug that fell out of a loop, so
    /// `supervise_observer` must go through `spawn_supervised_blocking_bounded`
    /// rather than the plain `spawn_supervised_blocking`. A body that just
    /// returns (the shape `run_observer_with_reconnect` takes on such a host)
    /// must cost a `debug!` and a released health row, not a `warn!` and a
    /// permanent `Returned` one on every boot without the protocol.
    ///
    /// Falsify by swapping `supervise_observer`'s
    /// `spawn_supervised_blocking_bounded` back to `spawn_supervised_blocking`:
    /// the row then survives forever in `Returned` and the second wait below
    /// times out.
    ///
    /// Waits for the row to **appear** before waiting for it to disappear.
    /// Without that first wait, a body that returns as fast as this one does
    /// can race the tokio scheduler: checking only for absence would read "no
    /// row" while supervision simply hadn't started yet, and pass even against
    /// the plain `spawn_supervised_blocking` this test exists to catch —
    /// measured, not hypothetical (it did, until this fix).
    #[test]
    fn a_returning_observer_releases_its_row_instead_of_sticking() {
        const NAME: &str = "test-idle-observer-bounded-return";

        let state = Mutable::new(IdleState::default());
        let dim = Arc::new(DimLatch::default());

        supervise_observer(NAME, state, dim, |_state, _dim| {
            // A brief pause before returning widens the window in which the
            // row is observably present, so the first wait below is not
            // itself a race against an instant return.
            std::thread::sleep(Duration::from_millis(50));
        });

        assert!(
            wait_for(Duration::from_secs(5), || health_row_exists(NAME)),
            "the supervisor never published a health row for {NAME}"
        );
        assert!(
            wait_for(Duration::from_secs(5), || !health_row_exists(NAME)),
            "a designed return from the observer must release its health row, not leave it \
             Returned forever"
        );
    }

    /// Whether `hytte_reactive::health` still carries a row for `name`.
    fn health_row_exists(name: &str) -> bool {
        hytte_reactive::health::snapshot()
            .iter()
            .any(|h| h.name == name)
    }

    /// Poll `cond` until it holds or `within` elapses; returns whether it held.
    fn wait_for(within: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if cond() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

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
            Arc::new(DimLatch::default()),
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
        // (nothing dimmed here so the no-dim path spawns no restore command —
        // a dim in effect would exec a real `brightnessctl -r`.)
        let state = Mutable::new(IdleState::Idle {
            deepest_secs: LOCK_SECS,
            since: Local::now(),
        });
        let dim = Arc::new(DimLatch::default());
        reset_after_observer_error(&state, &dim);
        assert_eq!(state.get_cloned(), IdleState::Active);
        assert!(!dim.is_dimmed());
    }

    // ── The dim against its own resume ──────────────────────────────────────

    /// The backlight `brightnessctl` drives, as far as the dim can tell: a
    /// level, the level `-s` saved for `-r`, and every invocation in order.
    struct FakeBacklight {
        level: u32,
        saved: Option<u32>,
        ran: Vec<&'static [&'static str]>,
    }

    impl FakeBacklight {
        fn at(level: u32) -> RefCell<Self> {
            RefCell::new(Self {
                level,
                saved: None,
                ran: Vec::new(),
            })
        }

        /// What `brightnessctl` does with the two argument lists the dim uses.
        fn run(&mut self, args: &'static [&'static str]) {
            self.ran.push(args);
            if args == DIM_ARGS {
                self.saved = Some(self.level);
                self.level = 10;
            } else if args == RESTORE_ARGS {
                if let Some(saved) = self.saved {
                    self.level = saved;
                }
            } else {
                panic!("the dim ran an unexpected brightnessctl {args:?}");
            }
        }
    }

    /// Where an idle cycle's `resumed` lands relative to its dim task.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ResumeLands {
        /// While the task awaits logind's `BlockInhibited`.
        DuringGate,
        /// While `brightnessctl -s set 10%` runs — the window the bare
        /// `dimmed` flag lost a resume in.
        DuringDim,
        /// After the task finished — the ordinary cycle.
        AfterDim,
    }

    /// One idle cycle of the dim threshold, with its `resumed` landing at
    /// `when`. `idled` reads the generation on the "observer thread" (here: up
    /// front, before the task runs) exactly as [`IdleClient::fire_action`]
    /// does, and the resume is [`restore_dim_if_dimmed`]'s decision —
    /// [`DimLatch::resume`] — with the restore it owes run against the fake
    /// instead of spawned. The task itself is the production
    /// [`dim_unless_resumed`], with no inhibitor held.
    fn idle_cycle(dim: &DimLatch, backlight: &RefCell<FakeBacklight>, when: ResumeLands) {
        let resume = || {
            if dim.resume() {
                backlight.borrow_mut().run(RESTORE_ARGS);
            }
        };
        let generation = dim.generation();
        let gate = async {
            if when == ResumeLands::DuringGate {
                resume();
            }
            false
        };
        block_on(dim_unless_resumed(dim, generation, gate, |args| {
            let resume = &resume;
            async move {
                backlight.borrow_mut().run(args);
                if args == DIM_ARGS && when == ResumeLands::DuringDim {
                    resume();
                }
                true
            }
        }));
        if when == ResumeLands::AfterDim {
            resume();
        }
    }

    /// The bug: a resume dispatched while `brightnessctl -s set 10%` runs used
    /// to read `dimmed` still clear and restore nothing, and the dim then set
    /// it — the screen stayed at 10% under an active user. Now the dim sees
    /// its cycle ended and undoes itself, exactly once.
    ///
    /// Falsify by making [`DimLatch::applied`] set `dimmed` unconditionally
    /// (the old "set it after the await" order): the backlight ends at 10%
    /// and the latch still thinks a dim is in effect.
    #[test]
    fn a_resume_while_the_dim_runs_restores_the_backlight() {
        let dim = DimLatch::default();
        let backlight = FakeBacklight::at(80);

        idle_cycle(&dim, &backlight, ResumeLands::DuringDim);

        let backlight = backlight.borrow();
        assert_eq!(backlight.level, 80, "the resume was lost: still dimmed");
        assert_eq!(
            backlight.ran,
            [DIM_ARGS, RESTORE_ARGS],
            "one dim, one restore"
        );
        assert!(
            !dim.is_dimmed(),
            "a dim nobody will restore is still marked in effect"
        );
    }

    /// A resume that lands while the gate is read ends the cycle before the
    /// dim starts: `brightnessctl` never runs, so the screen does not even
    /// dip. (An observer reset lands here the same way — it is a resume.)
    ///
    /// Falsify by dropping the generation check from [`DimLatch::may_dim`]:
    /// the dim runs after the resume (and, with `applied` intact, undoes
    /// itself — so the assertion that trips is the empty invocation list).
    #[test]
    fn a_resume_while_the_gate_is_read_never_dims() {
        let dim = DimLatch::default();
        let backlight = FakeBacklight::at(80);

        idle_cycle(&dim, &backlight, ResumeLands::DuringGate);

        let backlight = backlight.borrow();
        assert_eq!(backlight.level, 80);
        assert!(backlight.ran.is_empty(), "dimmed after the cycle resumed");
        assert!(!dim.is_dimmed());
    }

    /// The ordinary cycle is unchanged: dim, then the resume restores it.
    #[test]
    fn a_resume_after_the_dim_restores_it() {
        let dim = DimLatch::default();
        let backlight = FakeBacklight::at(80);

        idle_cycle(&dim, &backlight, ResumeLands::AfterDim);

        let backlight = backlight.borrow();
        assert_eq!(backlight.level, 80);
        assert_eq!(backlight.ran, [DIM_ARGS, RESTORE_ARGS]);
        assert!(!dim.is_dimmed());
    }

    /// The escalation: once one resume was lost, the next cycle's `-s` saved
    /// the stuck 10% as the original, so every later restore put the screen
    /// back to 10%. Run the racing cycle and then ordinary ones: the saved
    /// level is the user's every time, and the screen ends where it started.
    #[test]
    fn a_racing_resume_never_leaves_ten_percent_as_the_saved_level() {
        let dim = DimLatch::default();
        let backlight = FakeBacklight::at(80);

        for when in [
            ResumeLands::DuringDim,
            ResumeLands::AfterDim,
            ResumeLands::DuringGate,
            ResumeLands::DuringDim,
            ResumeLands::AfterDim,
        ] {
            idle_cycle(&dim, &backlight, when);
            let backlight = backlight.borrow();
            assert_eq!(backlight.level, 80, "a cycle ended dimmed");
            assert_ne!(
                backlight.saved,
                Some(10),
                "10% became the level -r restores"
            );
        }
        assert!(!dim.is_dimmed());
    }

    /// The saved-original guard on its own: a second dim fired while one is in
    /// effect must not `-s` over the 10%. Two `idled` with no `resumed` between
    /// them is not something the protocol sends (and a reset ends the cycle),
    /// so this is the backstop, not the fix.
    ///
    /// Falsify by dropping `!state.dimmed` from [`DimLatch::may_dim`]: the
    /// second `-s` runs, saving 10 as the level the resume then "restores".
    #[test]
    fn a_dim_never_saves_over_a_dim_in_effect() {
        let dim = DimLatch::default();
        let backlight = FakeBacklight::at(80);
        let run = |args: &'static [&'static str]| {
            backlight.borrow_mut().run(args);
            std::future::ready(true)
        };

        let generation = dim.generation();
        block_on(dim_unless_resumed(
            &dim,
            generation,
            std::future::ready(false),
            run,
        ));
        block_on(dim_unless_resumed(
            &dim,
            generation,
            std::future::ready(false),
            run,
        ));
        assert_eq!(
            backlight.borrow().ran,
            [DIM_ARGS],
            "a second -s ran over the dim"
        );

        if dim.resume() {
            backlight.borrow_mut().run(RESTORE_ARGS);
        }
        assert_eq!(backlight.borrow().level, 80);
    }

    /// An inhibitor held at the gate still skips the dim entirely, and leaves
    /// nothing for the resume to restore (#377's "a skipped dim leaves nothing
    /// to restore").
    #[test]
    fn a_held_inhibitor_skips_the_dim_and_its_restore() {
        let dim = DimLatch::default();
        let backlight = FakeBacklight::at(80);

        let generation = dim.generation();
        block_on(dim_unless_resumed(
            &dim,
            generation,
            std::future::ready(true),
            |args| {
                backlight.borrow_mut().run(args);
                std::future::ready(true)
            },
        ));
        assert!(!dim.resume(), "a skipped dim owes a restore");
        assert!(backlight.borrow().ran.is_empty());
    }
}
