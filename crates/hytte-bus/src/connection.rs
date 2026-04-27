//! Process-wide shared D-Bus connections, one per `BusKind`, with a
//! supervisor that owns reconnect with bounded exponential backoff.
//!
//! All five capability primitives sit on top of `SharedConnection`. No
//! other code in the workspace should call `zbus::Connection::session()`
//! or `system()`.

// These accessors are all used by Task 6 supervisor and production callers;
// they are intentionally forward-declared here before any caller exists.
#![allow(dead_code)]

use crate::BusError;
use futures_signals::signal::Mutable;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
/// a transient error until the supervisor (Task 6) re-establishes it.
struct Inner {
    conn: Option<Connection>,
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

/// Returns true for zbus errors that indicate a lost or unavailable
/// connection (transient; the supervisor should reconnect and callers retry).
///
/// In zbus 5.x the top-level `Disconnected` variant was removed; the
/// equivalent is `zbus::Error::FDO(Box<fdo::Error::Disconnected>)`.
fn is_transient_zbus_error(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::InputOutput(_) => true,
        zbus::Error::FDO(fdo_err) => matches!(**fdo_err, zbus::fdo::Error::Disconnected(_)),
        _ => false,
    }
}

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

    /// Reactive view of the epoch.
    #[must_use]
    pub fn epoch_signal(&self) -> Mutable<u64> {
        self.epoch_signal.clone()
    }

    /// Run `f` against the current connection. On `zbus::Error::InputOutput`
    /// or FDO `Disconnected` (transient variants), maps to
    /// `BusError::Transient` and clears the cached connection so the next
    /// caller waits for the supervisor to reconnect (supervisor wiring in
    /// Task 6). Returns `BusError::Transient` immediately when no cached
    /// connection is available (mid-reconnect).
    pub async fn with_conn<F, R, Fut>(&self, f: F) -> Result<R, BusError>
    where
        F: FnOnce(Connection) -> Fut,
        Fut: std::future::Future<Output = Result<R, zbus::Error>>,
    {
        let conn = {
            let guard = self.inner.lock().await;
            if let Some(c) = guard.conn.as_ref() {
                c.clone()
            } else {
                // No cached connection — mid-reconnect. Signal transient
                // failure; Task 6 supervisor will fill the slot again.
                let sentinel = zbus::Error::FDO(Box::new(zbus::fdo::Error::Disconnected(
                    "no cached connection (mid-reconnect)".into(),
                )));
                return Err(BusError::Transient { source: sentinel });
            }
        };

        f(conn).await.map_err(|e| {
            if is_transient_zbus_error(&e) {
                // Mark the cached conn as broken so the next call returns
                // Transient immediately while the supervisor (Task 6) reconnects.
                if let Ok(mut guard) = self.inner.try_lock() {
                    guard.conn = None;
                }
            }
            BusError::from_zbus(e)
        })
    }
}

/// Test-only constructors and accessors. Production code uses
/// `connection::session()` / `connection::system()` (Task 6).
#[doc(hidden)]
pub mod test_support {
    use super::{Arc, AtomicU64, BusKind, Connection, Inner, Mutable, Mutex};

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
                inner: Arc::new(Mutex::new(Inner { conn: Some(conn) })),
                epoch: Arc::new(AtomicU64::new(1)),
                epoch_signal: Mutable::new(1),
            }
        }
    }
}
