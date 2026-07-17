//! Native `ext-idle-notify-v1` idle client (#204 Phase 3a of 4).
//!
//! Binds the compositor's `ext_idle_notifier_v1` global and creates one
//! `ext_idle_notification_v1` per swayidle threshold (240 dim / 300 lock /
//! 600 suspend, from `etc/swayidle/config`), then exposes "idle since /
//! resumed" reactively via [`state`].
//!
//! **Two modes, selected once at startup by [`NATIVE_ACTIONS_ENV`]:**
//!
//! - **Observe-only (default).** Runs *alongside* swayidle and takes **no**
//!   action — its job is to emit parity logs proving the native notifier fires
//!   at the same wall-clock points as swayidle (Phase 2). This is the default,
//!   so merging Phase 3a changes nothing and cannot double-fire with swayidle.
//! - **Native actions (opt-in, Phase 3a).** When [`NATIVE_ACTIONS_ENV`] is
//!   truthy, the same three actions swayidle runs fire natively at their
//!   thresholds — dim (`brightnessctl -s set 10%`, restored on resume), lock
//!   (`screensaver::lock`), suspend (`systemctl suspend`) — each **gated on
//!   logind's `BlockInhibited`** (skip dim/lock while `idle` is inhibited, skip
//!   suspend while `sleep` is). Intended for the maintainer to live-verify with
//!   swayidle's handlers parked; see the opt-in's doc for the cutover roadmap.
//!
//! The parity logging is unconditional (both modes). Retiring swayidle's
//! handlers plus the `screensaver.rs` `SIGSTOP` bridge — and then this opt-in —
//! is Phase 3b/4. See issue #204 for the full roadmap.
//!
//! ## Pure-safe Wayland path
//!
//! This uses only the safe `wayland-client` /
//! `wayland-protocols` (`staging`) APIs — no `unsafe`, inheriting the
//! workspace `unsafe_code = "forbid"`. It opens its **own** `wayland-client`
//! connection via `Connection::connect_to_env()` (independent of GTK's own
//! backend — a plain idle observer needs no GTK surface) and drives the event
//! queue with `blocking_dispatch` on a dedicated `std::thread`. The Wayland
//! objects are `!Send`, so they live entirely on that thread; only the
//! `Mutable<IdleState>` (which is `Send + Sync`) is written from it, matching
//! the reactive core's handle/work split.

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry, runtime};
use std::collections::BTreeSet;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;
use zbus::zvariant::OwnedValue;

/// `tracing` target for the parity logs — the deliverable of this phase.
const LOG_TARGET: &str = "hytte_services::idle_notify";

/// Idle thresholds (seconds) mirrored from `etc/swayidle/config`: dim @ 240,
/// lock @ 300, suspend @ 600. Observed for parity in every mode; when the
/// native-action opt-in is on they also gate the corresponding action.
const DIM_SECS: u32 = 240;
const LOCK_SECS: u32 = 300;
const SUSPEND_SECS: u32 = 600;

/// Every idle threshold, in ascending order — one `ext_idle_notification_v1`
/// each.
const THRESHOLDS: [u32; 3] = [DIM_SECS, LOCK_SECS, SUSPEND_SECS];

/// Opt-in env var enabling the **native idle-action pipeline** (dim / lock /
/// suspend). Truthy (`1`/`true`/`yes`/`on`, case-insensitive) turns it on;
/// anything else — including unset — leaves it **off**.
///
/// **This is a temporary Phase-3a bridge (#204).** While off (the default),
/// `idle_notify` is exactly the Phase-2 observe-only client: it arms the
/// notifications and emits the parity logs but fires **no** action, so merging
/// this cannot double-fire with the still-running swayidle. Turning it on lets
/// the maintainer live-verify the native actions *after* parking swayidle's
/// handlers (`systemctl --user stop swayidle`; see the PR's live-verify
/// protocol). Phase 3b/4 then deletes swayidle's handlers and the
/// `screensaver.rs` `SIGSTOP` bridge and removes this gate, making the native
/// path the sole one.
const NATIVE_ACTIONS_ENV: &str = "TROLLSHELL_NATIVE_IDLE_ACTIONS";

/// Read the [`NATIVE_ACTIONS_ENV`] opt-in. Default **off**. Read once at
/// service start so the mode is fixed for the process lifetime.
fn native_actions_enabled() -> bool {
    std::env::var(NATIVE_ACTIONS_ENV)
        .ok()
        .as_deref()
        .is_some_and(is_truthy)
}

/// Truthy env-var values: `1`, `true`, `yes`, `on` (case-insensitive, trimmed).
fn is_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

// ── Public data shape ─────────────────────────────────────────────────────────

/// Reactive idle state derived from the compositor's `ext-idle-notify-v1`
/// notifications.
///
/// This reactive shape is independent of the native idle-action pipeline: it
/// reports whether the seat is idle and, roughly, since when — whether or not
/// the Phase-3a actions are armed (see [`NATIVE_ACTIONS_ENV`]).
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

        // Read the opt-in exactly once at startup (Phase-3a bridge; default off
        // → observe-only, byte-for-byte the Phase-2 behavior). See
        // [`NATIVE_ACTIONS_ENV`].
        let actions_enabled = native_actions_enabled();
        if actions_enabled {
            tracing::warn!(
                target: LOG_TARGET,
                env = NATIVE_ACTIONS_ENV,
                "native idle actions ENABLED — dim/lock/suspend fire natively (gated on logind inhibitors); park swayidle's handlers (systemctl --user stop swayidle) to avoid double-firing"
            );
        }

        // Wayland objects are `!Send`; the whole client lives on this dedicated
        // thread and only writes back the `Send + Sync` `Mutable`. A `std::thread`
        // (rather than a tokio task) keeps the blocking dispatch loop off the
        // shared runtime's worker pool.
        std::thread::Builder::new()
            .name("hytte-idle-notify".into())
            .spawn(move || {
                if let Err(err) = run(worker_state, actions_enabled) {
                    tracing::warn!(target: LOG_TARGET, error = %err, "native idle-notify observer stopped");
                }
            })
            .expect("spawn hytte-idle-notify thread");

        IdleNotifyHandles { state }
    }
}

/// Returns the idle-notify service to register with the hytte runtime.
#[must_use]
pub fn service() -> IdleNotifyService {
    IdleNotifyService
}

/// Reactive idle state from the native `ext-idle-notify-v1` observer. `Active`
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

/// Map a threshold to the swayidle action it mirrors, for the parity logs.
fn swayidle_action(secs: u32) -> &'static str {
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
    /// Whether the native idle-action pipeline is armed (the Phase-3a opt-in,
    /// [`NATIVE_ACTIONS_ENV`]). When `false` this is a pure observer.
    actions_enabled: bool,
    /// `true` while a native dim is in effect (backlight saved + lowered), so
    /// resume restores exactly what it dimmed and a *skipped* dim (inhibitor
    /// held) leaves nothing to restore. Shared with the spawned action tasks.
    dimmed: Arc<AtomicBool>,
}

impl IdleClient {
    fn new(state: Mutable<IdleState>, actions_enabled: bool) -> Self {
        Self {
            state,
            notifications: Vec::new(),
            idled: BTreeSet::new(),
            since: None,
            actions_enabled,
            dimmed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Handle an `idled` event for `secs`: record it, estimate the idle-since
    /// on the first firing of this cycle, log parity, publish.
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
            swayidle_action = swayidle_action(secs),
            "ext-idle-notify idled (observe-only parity check vs swayidle)"
        );
        self.publish();
        if self.actions_enabled {
            self.fire_action(secs);
        }
    }

    /// Handle a `resumed` event for `secs`: clear it, drop the idle-since when
    /// the last threshold resumes, log parity, publish.
    fn on_resumed(&mut self, secs: u32) {
        self.idled.remove(&secs);
        if self.idled.is_empty() {
            self.since = None;
        }
        tracing::info!(
            target: LOG_TARGET,
            threshold_secs = secs,
            swayidle_action = swayidle_action(secs),
            "ext-idle-notify resumed (observe-only parity check vs swayidle)"
        );
        self.publish();
        if self.actions_enabled && secs == DIM_SECS {
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
    /// the relevant logind inhibitor (`idle` for dim/lock, `sleep` for suspend).
    /// The commands mirror `etc/swayidle/config` exactly. Runs off the observer
    /// thread so the Wayland dispatch loop keeps servicing events. Only called
    /// when the Phase-3a opt-in is on.
    fn fire_action(&self, secs: u32) {
        let dimmed = self.dimmed.clone();
        runtime::handle().spawn(async move {
            match secs {
                DIM_SECS => {
                    if inhibitor_blocks("idle").await {
                        return;
                    }
                    // swayidle: `brightnessctl -s set 10%` (save current, then dim).
                    if run_command("brightnessctl", &["-s", "set", "10%"]).await {
                        dimmed.store(true, Ordering::SeqCst);
                    }
                }
                LOCK_SECS => {
                    if inhibitor_blocks("idle").await {
                        return;
                    }
                    // swayidle: `loginctl lock-session` — reuse the existing path
                    // (screensaver::lock runs exactly that; do not reimplement).
                    crate::screensaver::lock();
                }
                SUSPEND_SECS => {
                    if inhibitor_blocks("sleep").await {
                        return;
                    }
                    // swayidle: `systemctl suspend`.
                    run_command("systemctl", &["suspend"]).await;
                }
                _ => {}
            }
        });
    }

    /// Undim on resume, mirroring swayidle's `resume 'brightnessctl -r'`. Only
    /// restores when a native dim is actually in effect (a skipped dim leaves
    /// the flag clear), so a stale restore never clobbers the saved level.
    fn restore_dim(&self) {
        let dimmed = self.dimmed.clone();
        runtime::handle().spawn(async move {
            if dimmed.swap(false, Ordering::SeqCst) {
                run_command("brightnessctl", &["-r"]).await;
            }
        });
    }
}

// ── Native idle actions (Phase 3a, opt-in) ─────────────────────────────────────

/// `true` if the logind `BlockInhibited` set holds `what` (so the matching
/// native idle action must be **skipped**). On any error reading it we return
/// `true` (skip) — the safe choice: never fire dim/lock/suspend when we can't
/// confirm nothing is inhibiting.
async fn inhibitor_blocks(what: &str) -> bool {
    match read_block_inhibited().await {
        Ok(list) => {
            let blocked = block_list_contains(&list, what);
            if blocked {
                tracing::info!(
                    target: LOG_TARGET,
                    what,
                    block_inhibited = %list,
                    "native idle action skipped — logind inhibitor held"
                );
            }
            blocked
        }
        Err(err) => {
            tracing::warn!(
                target: LOG_TARGET,
                what,
                error = %err,
                "could not read logind BlockInhibited; skipping native idle action to be safe"
            );
            true
        }
    }
}

/// Read `org.freedesktop.login1.Manager.BlockInhibited` (a colon-separated list
/// like `"idle:sleep"`) via `hytte-bus` on the **system** bus. `Properties.Get`
/// always replies a `Variant` (`v`), so deserialize `OwnedValue` then unwrap to
/// `String`.
async fn read_block_inhibited() -> Result<String, hytte_bus::BusError> {
    let v: OwnedValue = hytte_bus::call("org.freedesktop.login1")
        .bus(hytte_bus::BusKind::System)
        .at_path("/org/freedesktop/login1")
        .iface("org.freedesktop.DBus.Properties")
        .method("Get")
        .args(("org.freedesktop.login1.Manager", "BlockInhibited"))
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

/// Open an independent Wayland connection, bind `ext_idle_notifier_v1`, arm a
/// notification per threshold, then dispatch events forever. Runs on the
/// dedicated observer thread; returns only on a fatal Wayland error (or `Ok`
/// when the compositor advertises no idle-notifier at all).
fn run(state: Mutable<IdleState>, actions_enabled: bool) -> Result<()> {
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
    // swayidle keeps driving the timeline; we simply observe nothing.
    let notifier: ExtIdleNotifierV1 = match globals.bind(&qh, 1..=1, ()) {
        Ok(notifier) => notifier,
        Err(err) => {
            tracing::info!(
                target: LOG_TARGET,
                error = %err,
                "compositor does not advertise ext_idle_notifier_v1; native idle observer disabled (swayidle still runs)"
            );
            return Ok(());
        }
    };

    let mut client = IdleClient::new(state, actions_enabled);
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
        native_actions = actions_enabled,
        "native ext-idle-notify-v1 client armed (#204 Phase 3a; observe-only unless native_actions=true, which fires dim@240/lock@300/suspend@600 gated on logind inhibitors)"
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
    fn swayidle_action_maps_known_thresholds() {
        assert_eq!(swayidle_action(240), "dim");
        assert_eq!(swayidle_action(300), "lock");
        assert_eq!(swayidle_action(600), "suspend");
        assert_eq!(swayidle_action(42), "none");
    }

    #[test]
    fn snapshot_tracks_deepest_active_threshold() {
        let mut client = IdleClient::new(Mutable::new(IdleState::default()), false);
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
        // The exact case the safety gate turns on: `idle` present → skip idle
        // actions (dim/lock); `sleep` present → skip suspend.
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
    fn is_truthy_recognises_common_on_values() {
        for on in ["1", "true", "TRUE", "Yes", " on ", "On"] {
            assert!(is_truthy(on), "{on:?} should be truthy");
        }
        for off in ["0", "false", "no", "off", "", "  ", "2", "enable"] {
            assert!(!is_truthy(off), "{off:?} should be falsy");
        }
    }
}
