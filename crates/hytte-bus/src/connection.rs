//! Process-wide shared D-Bus connections, one per `BusKind`, with a
//! supervisor that owns reconnect with bounded exponential backoff.
//!
//! All five capability primitives sit on top of `SharedConnection`. No
//! other code in the workspace should call `zbus::Connection::session()`
//! or `system()`.

// Production-only accessors (session/system/start) are forward-declared for
// Task 6; they are wired up in Task 12.

use crate::BusError;
use crate::backoff::{FailureStreak, RetryStep};
use crate::error::is_transient_zbus_error;
use futures_signals::signal::Mutable;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex, OnceLock};
use tokio::sync::Mutex;
use zbus::Connection;

/// Which D-Bus to connect to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BusKind {
    /// The user session bus (`$DBUS_SESSION_BUS_ADDRESS`). Default for
    /// most consumer-facing services (notifications, screensaver, mpris).
    Session,
    /// The system bus (`/run/dbus/system_bus_socket`). Default for daemon
    /// integrations (login1, networkd, upower, iwd, polkit).
    System,
}

/// The internal mutable state of a `SharedConnection`.
///
/// `conn = None` means "currently reconnecting" — `with_conn` will return
/// a transient error until the supervisor re-establishes it.
struct Inner {
    conn: Option<Connection>,
    /// Monotonic counter, bumped every time a fresh `conn` is installed.
    /// `with_conn` captures it before running an op and, on a transient
    /// failure, only clears the cached connection if it is unchanged — so a
    /// late failure from a superseded connection attempt cannot clobber a
    /// newer, already-established connection.
    generation: u64,
}

/// Process-wide shared connection to one bus. Cloned freely (cheap, Arc).
///
/// Outside this crate, access is gated through the `test_support` re-export
/// so test code can construct instances; production code uses the supervisor
/// accessors added in Task 6.
#[derive(Clone)]
pub struct SharedConnection {
    kind: BusKind,
    inner: Arc<Mutex<Inner>>,
    epoch: Arc<AtomicU64>,
    epoch_signal: Mutable<u64>,
}

// ── Supervisor notify side-table ──────────────────────────────────────────────

/// Side-table mapping `SharedConnection` instances (keyed by Arc pointer
/// identity) to their supervisor's `Notify` channel. Used by
/// `simulate_disconnect_for_test` and by `with_conn`'s transient-error path
/// to wake the supervisor without storing the notifier inside `Inner`.
struct SupervisorNotifyTable {
    inner: StdMutex<HashMap<usize, Arc<tokio::sync::Notify>>>,
}

impl SupervisorNotifyTable {
    fn register(&self, owner: &SharedConnection, notify: Arc<tokio::sync::Notify>) {
        let key = Arc::as_ptr(&owner.inner) as usize;
        self.inner.lock().unwrap().insert(key, notify);
    }

    fn lookup(&self, owner: &SharedConnection) -> Option<Arc<tokio::sync::Notify>> {
        let key = Arc::as_ptr(&owner.inner) as usize;
        self.inner.lock().unwrap().get(&key).cloned()
    }
}

static SUPERVISOR_NOTIFY: LazyLock<SupervisorNotifyTable> =
    LazyLock::new(|| SupervisorNotifyTable {
        inner: StdMutex::new(HashMap::new()),
    });

/// Test-only side-table: pre-injected connections the supervisor should use
/// instead of calling `Connection::session/system`. Keyed by the same Arc
/// pointer identity as `SUPERVISOR_NOTIFY`. The value is consumed on first use.
struct InjectedConnTable {
    inner: StdMutex<HashMap<usize, Connection>>,
}

impl InjectedConnTable {
    fn inject(&self, owner: &SharedConnection, conn: Connection) {
        let key = Arc::as_ptr(&owner.inner) as usize;
        self.inner.lock().unwrap().insert(key, conn);
    }

    fn take(&self, owner_key: usize) -> Option<Connection> {
        self.inner.lock().unwrap().remove(&owner_key)
    }
}

static INJECTED_CONN: LazyLock<InjectedConnTable> = LazyLock::new(|| InjectedConnTable {
    inner: StdMutex::new(HashMap::new()),
});

// ── Process-wide singletons ───────────────────────────────────────────────────

static SESSION: OnceLock<SharedConnection> = OnceLock::new();
static SYSTEM: OnceLock<SharedConnection> = OnceLock::new();

/// Lazy global accessor for the session-bus shared connection. First call
/// constructs the singleton, opens the connection, and spawns the supervisor
/// on the hytte tokio runtime.
pub(crate) fn session() -> &'static SharedConnection {
    SESSION.get_or_init(|| SharedConnection::start(BusKind::Session))
}

/// Lazy global accessor for the system-bus shared connection.
pub(crate) fn system() -> &'static SharedConnection {
    SYSTEM.get_or_init(|| SharedConnection::start(BusKind::System))
}

/// Resolve a [`BusKind`] to its process-wide shared connection singleton. The
/// single place the builder constructors map their explicit bus argument onto a
/// connection.
pub(crate) fn for_kind(kind: BusKind) -> &'static SharedConnection {
    match kind {
        BusKind::Session => session(),
        BusKind::System => system(),
    }
}

// ── SharedConnection public API ───────────────────────────────────────────────

impl SharedConnection {
    /// The kind of bus this connection talks to.
    #[must_use]
    pub fn kind(&self) -> BusKind {
        self.kind
    }

    /// Current epoch — bumped each time the supervisor successfully
    /// re-establishes the connection. Primitives subscribe to
    /// `epoch_signal()` to know when to re-establish their state.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Reactive view of the epoch. Returns an `impl Signal` (not `Mutable`)
    /// so consumers cannot call `.set()` on it — only the supervisor does that
    /// through the struct field directly.
    pub fn epoch_signal(&self) -> impl futures_signals::signal::Signal<Item = u64> + use<> {
        self.epoch_signal.signal_cloned()
    }

    /// Run `f` against the current connection. On transient zbus errors
    /// (`InputOutput`, FDO `Disconnected`), maps to `BusError::Transient`,
    /// clears the cached connection, and notifies the supervisor to reconnect.
    /// Returns `BusError::Transient` immediately when no cached connection is
    /// available (mid-reconnect).
    pub async fn with_conn<F, R, Fut>(&self, f: F) -> Result<R, BusError>
    where
        F: FnOnce(Connection) -> Fut,
        Fut: std::future::Future<Output = Result<R, zbus::Error>>,
    {
        let (conn, generation) = {
            let guard = self.inner.lock().await;
            if let Some(c) = guard.conn.as_ref() {
                (c.clone(), guard.generation)
            } else {
                // No cached connection — mid-reconnect.
                let sentinel = zbus::Error::FDO(Box::new(zbus::fdo::Error::Disconnected(
                    "no cached connection (mid-reconnect)".into(),
                )));
                return Err(BusError::Transient { source: sentinel });
            }
        };

        let result = f(conn).await;
        if let Err(ref e) = result
            && is_transient_zbus_error(e)
        {
            // Was try_lock; replaced with lock().await so a concurrent with_conn doesn't
            // race us into leaving a known-broken connection cached. The lock is held briefly
            // — only long enough to write None — so the contention window is small.
            let mut guard = self.inner.lock().await;
            // Only clear if the connection we used is still the current one.
            // At disconnect several ops are in flight on the old connection;
            // without this guard each late failure would null the *fresh*
            // connection the supervisor already re-established and force yet
            // another reconnect + epoch bump (re-Get / re-subscribe across
            // every service).
            //
            // `take().is_some()` rather than `= None`, so "did I clear it" is
            // the same test as the write. The generation guard alone does not
            // make this once-per-disconnect: every in-flight op on the *same*
            // generation passes it, and each of them would re-notify and —
            // since #798 — re-log a loss that has already been reported.
            // Losing the redundant notifies costs nothing, because the first
            // one already left a permit on the `Notify` and the supervisor
            // re-reads `conn.is_none()` at the top of every iteration anyway.
            if guard.generation == generation && guard.conn.take().is_some() {
                drop(guard);
                log_connection_lost(self.kind, "with_conn", e);
                if let Some(notify) = SUPERVISOR_NOTIFY.lookup(self) {
                    notify.notify_one();
                }
            }
        }
        result.map_err(BusError::from_zbus)
    }

    /// Invalidate the cached connection and wake the supervisor to reconnect,
    /// but only if the epoch is still `expected` (i.e. no reconnect has
    /// happened since the caller captured it).
    ///
    /// This mirrors the epoch/generation-guarded invalidate + notify tail of
    /// [`with_conn`](Self::with_conn), for primitives that issue calls on a
    /// cached `zbus::Proxy` (notably [`BusProxy`](crate::BusProxy)) and thus
    /// bypass `with_conn`. Routing their transient failures here keeps them on
    /// the same shared reconnect path as everything else — a wedged peer's
    /// connection-level error still kicks the supervisor rather than being
    /// silently swallowed. Idempotent: a no-op once the connection was already
    /// replaced or cleared.
    pub(crate) async fn invalidate_if_epoch(&self, expected: u64) {
        let mut guard = self.inner.lock().await;
        if self.epoch() == expected && guard.conn.take().is_some() {
            drop(guard);
            log_connection_lost(
                self.kind,
                "proxy",
                &"a call on a cached proxy failed at the transport level",
            );
            if let Some(notify) = SUPERVISOR_NOTIFY.lookup(self) {
                notify.notify_one();
            }
        }
    }
}

// ── Production constructor ────────────────────────────────────────────────────

impl SharedConnection {
    /// Production constructor: creates the `SharedConnection` with `conn = None`
    /// and spawns the supervisor on the hytte tokio runtime. The supervisor's
    /// first iteration opens the real connection asynchronously.
    fn start(kind: BusKind) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            conn: None,
            generation: 0,
        }));
        let epoch = Arc::new(AtomicU64::new(0));
        let epoch_signal = Mutable::new(0u64);
        let notify = Arc::new(tokio::sync::Notify::new());

        let me = Self {
            kind,
            inner: inner.clone(),
            epoch: epoch.clone(),
            epoch_signal: epoch_signal.clone(),
        };

        SUPERVISOR_NOTIFY.register(&me, notify.clone());

        let inner_key = Arc::as_ptr(&me.inner) as usize;
        let task_notify = notify;
        hytte_reactive::runtime::handle().spawn(async move {
            supervisor_loop(kind, inner_key, inner, epoch, epoch_signal, task_notify).await;
        });

        me
    }
}

// ── Test-only constructors and accessors ──────────────────────────────────────

/// Test-only constructors and accessors. Production code uses
/// `connection::session()` / `connection::system()` (Task 6).
#[doc(hidden)]
pub mod test_support {
    use super::{
        Arc, AtomicU64, BusKind, Connection, INJECTED_CONN, Inner, Mutable, Mutex, Ordering,
        SUPERVISOR_NOTIFY,
    };

    pub use super::SharedConnection;

    impl SharedConnection {
        /// Construct a `SharedConnection` wrapping an existing test
        /// `Connection`. Bypasses the supervisor — for unit tests of
        /// individual primitives that want full control over reconnect.
        #[must_use]
        pub fn for_test_session(conn: Connection) -> Self {
            Self::for_test(BusKind::Session, conn)
        }

        /// Like `for_test_session` but for the system bus.
        #[must_use]
        pub fn for_test_system(conn: Connection) -> Self {
            Self::for_test(BusKind::System, conn)
        }

        fn for_test(kind: BusKind, conn: Connection) -> Self {
            Self {
                kind,
                inner: Arc::new(Mutex::new(Inner {
                    conn: Some(conn),
                    generation: 1,
                })),
                epoch: Arc::new(AtomicU64::new(1)),
                epoch_signal: Mutable::new(1),
            }
        }

        /// Test-only: install a fresh connection exactly as the supervisor does
        /// on a successful reconnect (bump generation + epoch). Lets a test
        /// deterministically reproduce "a fresh connection was installed while
        /// an old op was still in flight" without racing a real supervisor.
        #[doc(hidden)]
        pub async fn install_fresh_connection_for_test(&self, conn: Connection) {
            let mut g = self.inner.lock().await;
            g.conn = Some(conn);
            g.generation += 1;
            drop(g);
            let new_epoch = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
            self.epoch_signal.set(new_epoch);
        }

        /// Test-only: spawn a supervisor loop for a `for_test_*`
        /// `SharedConnection`. Production code never calls this — it is
        /// invoked from `start()` and from integration tests that need to
        /// exercise the reconnect path.
        ///
        /// Call this once per test, not once per reconnect cycle. Each call
        /// registers one `SUPERVISOR_NOTIFY` entry and spawns one orphaned
        /// tokio task that is never removed; this is harmless because each
        /// integration test binary runs in its own process and the runtime is
        /// torn down at process exit. Production code uses `start()` instead,
        /// which lives in an `OnceLock` for the lifetime of the process.
        #[doc(hidden)]
        pub fn spawn_supervisor_for_test(&self) {
            let inner = self.inner.clone();
            let inner_key = Arc::as_ptr(&self.inner) as usize;
            let epoch = self.epoch.clone();
            let signal = self.epoch_signal.clone();
            let notify = Arc::new(tokio::sync::Notify::new());
            SUPERVISOR_NOTIFY.register(self, notify.clone());
            let kind = self.kind;
            tokio::spawn(async move {
                super::supervisor_loop(kind, inner_key, inner, epoch, signal, notify).await;
            });
        }

        /// Test-only: pre-inject the connection the supervisor should use on
        /// its next reconnect, then drop the cached connection and wake the
        /// supervisor. This lets tests exercise the full supervisor reconnect
        /// path without needing to mutate `DBUS_SESSION_BUS_ADDRESS` (which
        /// is not allowed under `unsafe_code = "forbid"`).
        #[doc(hidden)]
        pub async fn simulate_disconnect_for_test(&self, replacement: Connection) {
            INJECTED_CONN.inject(self, replacement);
            {
                let mut guard = self.inner.lock().await;
                guard.conn = None;
                // Release the lock before notifying so the supervisor can
                // immediately acquire it.
                drop(guard);
            }
            if let Some(notify) = SUPERVISOR_NOTIFY.lookup(self) {
                notify.notify_one();
            }
        }
    }
}

// ── Supervisor logging ────────────────────────────────────────────────────────
//
// The three lines this file emits, kept together so their levels and their
// cadences can be read against one another rather than drifting apart at three
// call sites — the same reason `own.rs` funnels its four acquisition arms
// through one `log_acquire_failure`.

/// Report that the shared connection dropped out from under us: something ran
/// an operation on it, got a transport-level error back, and cleared the cache
/// so the supervisor will rebuild it.
///
/// **Level: `warn!`**, paired with [`log_connected`]'s reconnect arm — see
/// there for why the retraction takes the same level.
///
/// Before #798 this line did not exist at all. A blip that reconnected on the
/// first attempt therefore logged nothing but `bus connected`: a recovery with
/// no death, the mirror image of the asymmetry #765 objects to, and a reader of
/// the journal had no way to tell an ordinary startup from a bus that had just
/// come back. It also made a claim elsewhere in the crate untrue —
/// [`STREAK_IS_NO_LONGER_A_BLIP`](crate::backoff::STREAK_IS_NO_LONGER_A_BLIP)
/// keeps `own.rs`'s first four acquisition failures at `debug!` on the grounds
/// that `connection.rs` "already warns about the disconnect itself", when the
/// only `warn!` here was about a failed *reconnect*, which a single-attempt
/// recovery never reaches.
///
/// **Cadence: once per disconnect, not once per failed operation.** At a
/// disconnect several calls are in flight on the dead connection and every one
/// of them comes back transient. Both callers gate this on having been the one
/// that actually cleared the cache (`conn.take().is_some()`, under the
/// generation or epoch guard), so the losers stay silent and a disconnect costs
/// exactly one line however many operations noticed it.
fn log_connection_lost(kind: BusKind, via: &'static str, reason: &dyn std::fmt::Display) {
    tracing::warn!(
        ?kind,
        %via,
        %reason,
        "bus connection lost; every primitive on this bus is inert until the supervisor re-establishes it"
    );
}

/// Whether a successful connect *closes* an incident already in the journal,
/// rather than simply starting this process's bus life.
///
/// Epoch 1 is the first connection the process ever made: nothing was lost, no
/// [`log_connection_lost`] or [`log_connect_failure`] line precedes it, and
/// there is nothing to retract. Every later epoch was reached by way of a
/// cleared connection, and clearing it is what emits the loss.
///
/// Split out as a pure function for the same reason `own.rs`'s
/// `closes_a_give_up` is: it is the whole condition for the recovery line, and
/// the only part of it worth testing without a bus.
const fn closes_a_connection_loss(epoch: u64) -> bool {
    epoch > 1
}

/// Log that the shared connection is live.
///
/// **Level: `info!` for the first connection of the process, `warn!` for every
/// re-establishment after one.** That split is #765's both-halves invariant,
/// which `own.rs`'s `log_recovered` states in full: a filter that shows "the
/// bus is gone" must also show the line retracting it, or a `RUST_LOG=warn`
/// journal reports every death and no recovery. A retraction is not good news
/// logged at `warn!`; it is the second half of a `warn!`, and a level is a
/// filter salience rather than a sentiment. The first connect retracts nothing,
/// so it stays routine.
///
/// **Cadence:** one line per real transition, of which there is exactly one per
/// connection actually established — the supervisor cannot reach this arm twice
/// without a disconnect in between.
fn log_connected(kind: BusKind, epoch: u64) {
    if closes_a_connection_loss(epoch) {
        tracing::warn!(
            ?kind,
            epoch,
            "bus connection re-established; the earlier report that it was lost is now closed"
        );
    } else {
        tracing::info!(?kind, epoch, "bus connected");
    }
}

/// Emit the supervisor's failed-reconnect line at the cadence the ramp selects,
/// and report whether it spoke.
///
/// **Cadence (#798): only on the attempts
/// [`logs_at`](crate::backoff::logs_at) picks** — 1, 2, 4, 8, … — so a bus that
/// is unreachable for a day costs ~11 lines rather than one per attempt. Until
/// #797 this arm warned on every attempt, which is the flat uncapped retry
/// logging #646 objects to, and it could not be fixed: the private duration
/// cursor this loop carried had no attempt number for a cadence to be a
/// function of. #766 is what made it matter — the shell now defaults to `INFO`
/// instead of inheriting `EnvFilter`'s `ERROR`-only fallback, so these lines
/// reach a deployed journal for the first time.
///
/// The first failure of every streak is always loud, because `logs_at(1)` is
/// true and [`FailureStreak`] is cleared by any successful connect: a bus that
/// has just gone away says so immediately, and it is only the repeats that thin
/// out. Nothing is emitted on the thinned attempts, at any level — the same
/// choice `log_acquire_failure` makes in `own.rs`, and what actually bounds the
/// volume rather than moving it down one filter.
///
/// **Level: `warn!` on every line it emits**, deliberately *not* escalating
/// through [`RetryStep::is_serious`] the way `own.rs` does. That predicate
/// exists to lift `own.rs`'s acquisition failures out of `debug!` once they
/// have outlived the explanation "the bus blipped, and `connection.rs` already
/// warned about it" — an explanation only available to a caller downstream of
/// this loop. Here every line *is* the primary report of the condition, `warn!`
/// is already its honest level, and #765 is explicit that reaching for `error!`
/// to buy salience is the move to avoid.
///
/// The return value exists for `mod tests`, which walks a whole outage and
/// counts the lines it would emit without needing a bus or a subscriber.
fn log_connect_failure(kind: BusKind, error: &zbus::Error, step: RetryStep) -> bool {
    if !step.log {
        return false;
    }
    tracing::warn!(
        ?kind,
        error = %error,
        attempt = step.attempt,
        retry_in_ms = step.delay.as_millis(),
        "bus connect failed; retrying"
    );
    true
}

// ── Supervisor loop ───────────────────────────────────────────────────────────

// `inner_key` is the stable pointer identity of `inner` (Arc::as_ptr cast to
// usize). It is pre-computed by the caller so the supervisor can look up the
// injection table without borrowing the Arc.
async fn supervisor_loop(
    kind: BusKind,
    inner_key: usize,
    inner: Arc<Mutex<Inner>>,
    epoch: Arc<AtomicU64>,
    signal: Mutable<u64>,
    notify: Arc<tokio::sync::Notify>,
) {
    // The crate's shared retry ramp (`backoff.rs`), which this loop used to
    // carry a private duration-cursor copy of until #797. The walk is
    // unchanged (250 ms → 30 s, cleared on a successful connect); what the
    // shared type added is the attempt number, and #798 is where this loop
    // spends it — `log_connect_failure` gates on `RetryStep::log`, so a
    // permanently-unreachable bus costs O(log n) lines instead of one per
    // attempt.
    let mut backoff = FailureStreak::default();
    loop {
        // 1. If inner.conn is None, open a fresh connection.
        let needs_connect = {
            let g = inner.lock().await;
            g.conn.is_none()
        };

        if needs_connect {
            // Check for a test-injected replacement before hitting the real bus.
            let result = if let Some(conn) = INJECTED_CONN.take(inner_key) {
                Ok(conn)
            } else {
                open_connection(kind).await
            };

            match result {
                Ok(conn) => {
                    let mut g = inner.lock().await;
                    g.conn = Some(conn);
                    g.generation += 1;
                    drop(g);
                    let new_epoch = epoch.fetch_add(1, Ordering::AcqRel) + 1;
                    signal.set(new_epoch);
                    backoff.reset();
                    log_connected(kind, new_epoch);
                }
                Err(e) => {
                    let step = backoff.record();
                    log_connect_failure(kind, &e, step);
                    tokio::time::sleep(step.delay).await;
                    continue;
                }
            }
        }

        // 2. Wait for someone to notify us that the conn is broken.
        notify.notified().await;
    }
}

// Production-allowed: this IS the single centralized site that opens D-Bus
// connections. All other crates must use hytte::bus::* primitives instead.
#[allow(clippy::disallowed_methods)]
async fn open_connection(kind: BusKind) -> Result<Connection, zbus::Error> {
    match kind {
        BusKind::Session => Connection::session().await,
        BusKind::System => Connection::system().await,
    }
}

#[cfg(test)]
mod tests {
    use super::{BusKind, FailureStreak, closes_a_connection_loss, log_connect_failure};
    use std::time::Duration;

    /// The failure the supervisor is reacting to. Its content is irrelevant to
    /// the cadence — the ramp is indexed by attempt number, not by what broke.
    fn unreachable_bus() -> zbus::Error {
        zbus::Error::Failure("no bus at $DBUS_SESSION_BUS_ADDRESS".into())
    }

    /// Walk `supervisor_loop`'s failure arm `attempts` times against a bus that
    /// never comes back, and collect the attempt numbers that actually emit a
    /// line. This calls the real emitter, so deleting its `if !step.log` gate
    /// reddens every assertion below rather than quietly restoring the
    /// per-attempt `warn!` #798 removed.
    fn attempts_that_speak(attempts: u32) -> Vec<u32> {
        let error = unreachable_bus();
        let mut streak = FailureStreak::default();
        (0..attempts)
            .filter_map(|_| {
                let step = streak.record();
                log_connect_failure(BusKind::Session, &error, step).then_some(step.attempt)
            })
            .collect()
    }

    /// A bus that has just gone away must say so at once. The cadence is
    /// allowed to thin the repeats; it is not allowed to swallow the report.
    #[test]
    fn the_first_failure_of_a_streak_is_always_loud() {
        assert_eq!(
            attempts_that_speak(1),
            vec![1],
            "the first connect failure must emit its line immediately"
        );
    }

    /// The whole point of #798: the line fires at every doubling and nowhere
    /// else, so its volume is logarithmic in the length of the outage. The
    /// unconditional `warn!` this replaces would return all 1024 attempts.
    #[test]
    fn a_long_outage_costs_a_line_per_doubling_not_per_attempt() {
        assert_eq!(
            attempts_that_speak(1024),
            vec![1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024],
            "the supervisor must log at the ramp's doublings only"
        );
    }

    /// The claim stated as the budget it buys, on the supervisor's own walk
    /// rather than on `backoff.rs`'s: an hour with the bus unreachable is a
    /// handful of lines. At the flat pre-#798 cadence the same hour is ~120.
    #[test]
    fn an_hour_of_unreachable_bus_is_a_handful_of_lines() {
        let error = unreachable_bus();
        let mut streak = FailureStreak::default();
        let mut elapsed = Duration::ZERO;
        let mut attempts = 0u32;
        let mut lines = 0u32;
        while elapsed < Duration::from_hours(1) {
            let step = streak.record();
            attempts += 1;
            if log_connect_failure(BusKind::Session, &error, step) {
                lines += 1;
            }
            elapsed += step.delay;
        }
        assert!(
            attempts >= 100,
            "the 30 s ceiling implies ~120 attempts an hour; got {attempts}"
        );
        assert!(
            lines <= 8,
            "an hour of failure must not cost more than a handful of lines; got {lines}"
        );
    }

    /// The other half of the cadence: a successful connect clears the streak,
    /// so the *next* outage is reported the moment it starts rather than
    /// inheriting the previous one's thinning. Without the `backoff.reset()`
    /// the success arm performs, a flapping bus would go quiet.
    #[test]
    fn a_successful_connect_makes_the_next_failure_loud_again() {
        let error = unreachable_bus();
        let mut streak = FailureStreak::default();
        for _ in 0..12 {
            let _ = streak.record();
        }
        assert!(
            !log_connect_failure(BusKind::Session, &error, streak.record()),
            "a long streak must be thinned before the reset means anything"
        );

        streak.reset();

        assert!(
            log_connect_failure(BusKind::Session, &error, streak.record()),
            "the first failure after a successful connect must be loud again"
        );
    }

    /// #765's both-halves invariant, as the predicate that implements it: the
    /// process's first connection retracts nothing and stays at `info!`, while
    /// every re-establishment closes a `warn!` that is already in the journal
    /// and must be visible at the same filter.
    #[test]
    fn only_a_reconnect_closes_an_incident() {
        assert!(
            !closes_a_connection_loss(1),
            "the first connection of the process opens no incident to close"
        );
        for epoch in [2u64, 3, 100, u64::MAX] {
            assert!(
                closes_a_connection_loss(epoch),
                "epoch {epoch} was reached through a cleared connection, which was reported lost"
            );
        }
    }
}
