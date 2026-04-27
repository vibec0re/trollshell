//! Shared D-Bus capability layer for hytte services.
//!
//! See `docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md`
//! for the design.

#![doc(html_no_source)]

mod connection;
mod error;
mod own;

pub use connection::BusKind;
pub use error::BusError;
pub use own::{own_name_with, OwnState};

#[doc(hidden)]
pub use connection::test_support;
