//! The seam over `hytte-bus`'s "subscribed" handshake (#1551).
//!
//! A service that subscribes and then reads a daemon's state has to wait for
//! [`hytte_bus::SignalSubscription::subscribed`] in between, or whatever the
//! daemon emits before the match rule lands is lost. The helpers that order
//! those steps (`bluetooth`'s `subscribe_then_snapshot`, `wifi`'s
//! `open_discovery`) take the subscription as a [`Handshake`] and do the
//! waiting **themselves**. A call site cannot leave the wait out, and a test
//! can hand the helper a recording stand-in and read the order back.

use hytte_bus::SignalSubscription;

/// Something whose match rule can be waited for.
pub(crate) trait Handshake {
    /// Resolves once the bus has installed this subscription's match rule.
    /// See [`SignalSubscription::subscribed`] for what that does and does not
    /// promise across a reconnect.
    fn subscribed(&self) -> impl Future<Output = ()>;
}

impl Handshake for SignalSubscription {
    fn subscribed(&self) -> impl Future<Output = ()> {
        SignalSubscription::subscribed(self)
    }
}
