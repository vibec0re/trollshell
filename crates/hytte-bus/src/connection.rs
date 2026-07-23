//! Process-wide shared D-Bus connections, one per `BusKind`, with a
//! supervisor that owns reconnect with bounded exponential backoff.
//!
//! All five capability primitives sit on top of `SharedConnection`. No
//! other code in the workspace should call `zbus::Connection::session()`
//! or `system()`.

// Production-only accessors (session/system/start) are forward-declared for
// Task 6; they are wired up in Task 12.

use crate::BusError;
use crate::error::is_transient_zbus_error;
use futures_signals::signal::Mutable;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex, OnceLock};
use std::time::Duration;
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

// ── Exponential backoff ───────────────────────────────────────────────────────

/// Exponential backoff with cap. State is the next sleep duration to use.
#[derive(Clone, Copy)]
struct Backoff {
    next_ms: u64,
}

impl Backoff {
    const fn new() -> Self {
        Self { next_ms: 250 }
    }

    fn reset(&mut self) {
        self.next_ms = 250;
    }

    fn next(&mut self) -> Duration {
        let d = Duration::from_millis(self.next_ms);
        self.next_ms = (self.next_ms * 2).min(30_000);
        d
    }
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
            if guard.generation == generation {
                guard.conn = None;
                drop(guard);
                if let Some(notify) = SUPERVISOR_NOTIFY.lookup(self) {
                    notify.notify_one();
                }
            }
        }
        result.map_err(BusError::from_zbus)
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
    let mut backoff = Backoff::new();
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
                    tracing::info!(?kind, epoch = new_epoch, "bus connected");
                }
                Err(e) => {
                    let d = backoff.next();
                    tracing::warn!(
                        ?kind,
                        error = %e,
                        retry_in_ms = d.as_millis(),
                        "bus connect failed"
                    );
                    tokio::time::sleep(d).await;
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
