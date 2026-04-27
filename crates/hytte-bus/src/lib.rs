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
mod signals;

pub use call::{call_with, CallBuilder, RetryPolicy};
pub use connection::BusKind;
pub use error::BusError;
pub use own::{own_name_with, OwnNameSignal, OwnState};
pub use property::{property_with, PropState};
pub use signals::{signals_with, SignalEvent, SignalSubscription};

#[doc(hidden)]
pub use connection::test_support;
