//! Native `ext-idle-notify-v1` idle observer (#204 Phase 2 of 4).
//!
//! Binds the compositor's `ext_idle_notifier_v1` global and creates one
//! `ext_idle_notification_v1` per swayidle threshold (240 dim / 300 lock /
//! 600 suspend, from `etc/swayidle/config`), then exposes "idle since /
//! resumed" reactively via [`state`].
//!
//! **Observe-only.** This phase runs *alongside* swayidle and takes **no
//! action** — no dim, lock, suspend, brightness, logind, or swayidle poke. Its
//! sole job is to validate that the native notifier fires at parity with
//! swayidle's timings (watch the `hytte_services::idle_notify` parity logs).
//! Wiring the dim/lock/suspend actions is Phase 3 (gated on native logind
//! inhibitor checks); retiring swayidle plus the `screensaver.rs` `SIGSTOP`
//! bridge is Phase 4. See issue #204 for the full roadmap.
//!
//! ## Pure-safe Wayland path
//!
//! Like `hytte-blur`, this uses only the safe `wayland-client` /
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
use hytte_reactive::{Service, registry};
use std::collections::BTreeSet;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

/// `tracing` target for the parity logs — the deliverable of this phase.
const LOG_TARGET: &str = "hytte_services::idle_notify";

/// Idle thresholds (seconds) mirrored from `etc/swayidle/config`: dim @ 240,
/// lock @ 300, suspend @ 600. Phase 2 observes these to confirm the native
/// notifier fires at the same wall-clock points as swayidle before Phase 3
/// wires the actions.
const THRESHOLDS: [u32; 3] = [240, 300, 600];

// ── Public data shape ─────────────────────────────────────────────────────────

/// Reactive idle state derived from the compositor's `ext-idle-notify-v1`
/// notifications.
///
/// Phase 2 is **observe-only**: this reports whether the seat is idle and,
/// roughly, since when — but drives no dim/lock/suspend action (that lands in
/// Phase 3, gated on logind inhibitors; see #204).
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

        // Wayland objects are `!Send`; the whole client lives on this dedicated
        // thread and only writes back the `Send + Sync` `Mutable`. A `std::thread`
        // (rather than a tokio task) keeps the blocking dispatch loop off the
        // shared runtime's worker pool.
        std::thread::Builder::new()
            .name("hytte-idle-notify".into())
            .spawn(move || {
                if let Err(err) = run(worker_state) {
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
        240 => "dim",
        300 => "lock",
        600 => "suspend",
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
}

impl IdleClient {
    fn new(state: Mutable<IdleState>) -> Self {
        Self {
            state,
            notifications: Vec::new(),
            idled: BTreeSet::new(),
            since: None,
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
}

/// Open an independent Wayland connection, bind `ext_idle_notifier_v1`, arm a
/// notification per threshold, then dispatch events forever. Runs on the
/// dedicated observer thread; returns only on a fatal Wayland error (or `Ok`
/// when the compositor advertises no idle-notifier at all).
fn run(state: Mutable<IdleState>) -> Result<()> {
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

    let mut client = IdleClient::new(state);
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
        "native ext-idle-notify-v1 observer armed (#204 Phase 2, observe-only; alongside swayidle dim@240 lock@300 suspend@600)"
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
        let mut client = IdleClient::new(Mutable::new(IdleState::default()));
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
}
