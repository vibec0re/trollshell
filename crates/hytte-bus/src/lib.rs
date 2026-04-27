//! Shared D-Bus capability layer for hytte services.
//!
//! See `docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md`
//! for the design.

#![doc(html_no_source)]

mod call;
mod connection;
mod error;
mod own;
mod property;
mod proxy;
mod signals;

pub use call::{call_with, CallBuilder, RetryPolicy};
pub use connection::BusKind;
pub use error::BusError;
pub use own::{own_name_with, OwnNameBuilder, OwnNameSignal, OwnState};
pub use property::{property_with, PropState, PropertyBuilder, PropertySignal};
pub use proxy::{proxy_with, BusProxy, ProxyBuilder, ProxyState};
pub use signals::{signals_with, SignalEvent, SignalSubscription, SignalsBuilder};

#[doc(hidden)]
pub use connection::test_support;

// ── Public global accessors ───────────────────────────────────────────────────
//
// These are the production API that `hytte-services` consumers call. They
// route to the lazily-initialised session-bus / system-bus `SharedConnection`
// singletons.

/// Build an [`OwnNameBuilder`] against the **session** bus by default. Use
/// `.bus(BusKind::System)` to switch.
#[must_use]
pub fn own_name(name: impl Into<String>) -> OwnNameBuilder {
    own::own_name_with(connection::session(), name)
}

/// Build a [`SignalsBuilder`] against the **system** bus by default. Use
/// `.bus(BusKind::Session)` to switch.
#[must_use]
pub fn signals(destination: impl Into<String>) -> SignalsBuilder {
    signals::signals_with(connection::system(), destination)
}

/// Build a [`CallBuilder`] against the **session** bus by default. Use
/// `.bus(BusKind::System)` to switch.
#[must_use]
pub fn call(destination: impl Into<String>) -> CallBuilder<()> {
    call::call_with(connection::session(), destination)
}

/// Build a [`PropertyBuilder`] against the **system** bus by default. Use
/// `.bus(BusKind::Session)` to switch.
#[must_use]
pub fn property<T>(destination: impl Into<String>) -> PropertyBuilder<T>
where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<zbus::zvariant::OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<zbus::zvariant::Value<'v>, Error = zbus::zvariant::Error>,
{
    property::property_with(connection::system(), destination)
}

/// Build a [`ProxyBuilder`] against the **system** bus by default. Use
/// `.bus(BusKind::Session)` to switch.
pub fn proxy(destination: impl Into<String>) -> ProxyBuilder {
    proxy::proxy_with(connection::system(), destination)
}
