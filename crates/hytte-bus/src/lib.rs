//! Shared D-Bus capability layer for hytte services.
//!
//! See `docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md`
//! for the design.

#![doc(html_no_source)]

mod call;
mod connection;
mod error;
mod export;
mod handle;
mod own;
mod property;
mod proxy;
mod signals;

pub use call::{CallBuilder, FdLease, RetryPolicy, call_with};
pub use connection::BusKind;
pub use error::BusError;
pub use export::{ExportBuilder, ExportHandle, export_object_with};
pub use own::{OwnNameBuilder, OwnNameSignal, OwnState, own_name_with};
pub use property::{PropState, PropertyBuilder, PropertySignal, property_with};
pub use proxy::{BusProxy, ProxyBuilder, ProxyState, proxy_with};
pub use signals::{SignalEvent, SignalSubscription, SignalsBuilder, signals_with};

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

/// Build an [`ExportBuilder`] against the **system** bus by default — exports a
/// D-Bus interface at an object path *without* owning a well-known name. Use
/// `.bus(BusKind::Session)` to switch.
///
/// For agents whose host daemon calls back on the registering connection's
/// unique name (e.g. `NetworkManager`'s secret agent), this avoids needing a
/// well-known name (and the system-bus policy that would require).
#[must_use]
pub fn export_object(path: impl Into<String>) -> ExportBuilder {
    export::export_object_with(connection::system(), path)
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
