//! Process-wide shared D-Bus connections, one per `BusKind`, with a
//! supervisor that owns reconnect with bounded exponential backoff.
//!
//! All five capability primitives sit on top of `SharedConnection`. No
//! other code in the workspace should call `zbus::Connection::session()`
//! or `system()`.

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
