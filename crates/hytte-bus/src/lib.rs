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
// These are the production API that `hytte-services` consumers call. Each takes
// the target [`BusKind`] as its first argument — there is no default. The bus
// is stated at the constructor so a call to a system daemon can never silently
// land on the session bus (or vice versa); it routes to the lazily-initialised
// session-bus / system-bus `SharedConnection` singleton.

/// Build an [`OwnNameBuilder`] against the given `bus`.
#[must_use]
pub fn own_name(bus: BusKind, name: impl Into<String>) -> OwnNameBuilder {
    own::own_name_with(connection::for_kind(bus), name)
}

/// Build an [`ExportBuilder`] against the given `bus` — exports a D-Bus
/// interface at an object path *without* owning a well-known name.
///
/// For agents whose host daemon calls back on the registering connection's
/// unique name (e.g. `NetworkManager`'s secret agent), this avoids needing a
/// well-known name (and the system-bus policy that would require).
#[must_use]
pub fn export_object(bus: BusKind, path: impl Into<String>) -> ExportBuilder {
    export::export_object_with(connection::for_kind(bus), path)
}

/// Build a [`SignalsBuilder`] against the given `bus`.
#[must_use]
pub fn signals(bus: BusKind, destination: impl Into<String>) -> SignalsBuilder {
    signals::signals_with(connection::for_kind(bus), destination)
}

/// Build a [`CallBuilder`] against the given `bus`.
#[must_use]
pub fn call(bus: BusKind, destination: impl Into<String>) -> CallBuilder<()> {
    call::call_with(connection::for_kind(bus), destination)
}

/// Build a [`PropertyBuilder`] against the given `bus`.
#[must_use]
pub fn property<T>(bus: BusKind, destination: impl Into<String>) -> PropertyBuilder<T>
where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<zbus::zvariant::OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<zbus::zvariant::Value<'v>, Error = zbus::zvariant::Error>,
{
    property::property_with(connection::for_kind(bus), destination)
}

/// Build a [`ProxyBuilder`] against the given `bus`.
pub fn proxy(bus: BusKind, destination: impl Into<String>) -> ProxyBuilder {
    proxy::proxy_with(connection::for_kind(bus), destination)
}
