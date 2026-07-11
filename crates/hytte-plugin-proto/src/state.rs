//! The host-state subset a plugin subscribes to.
//!
//! Per Annika's call on #195 there are **no per-key deltas**: the host sends
//! the plugin the full subscribed-state subset ([`StateSnapshot`]) on *any*
//! change, latest-wins. A plugin declares which keys it wants via
//! [`Manifest::subscribes`](crate::manifest::Manifest::subscribes); only those
//! fields are populated, keeping serialization/transfer small.

use serde::{Deserialize, Serialize};

/// Wall-clock state — the wire projection of `hytte_services::clock::now()`.
/// GTK- and chrono-free: the host fills these from its `DateTime<Local>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockState {
    /// ISO-8601 / RFC 3339 local timestamp, e.g. `2026-07-11T15:49:00+02:00`.
    pub iso: String,
    /// Unix time in whole seconds.
    pub unix: i64,
}

/// The full subscribed-state subset, re-sent to the plugin on **any** change
/// (no deltas, latest-wins). A field is `Some` iff the plugin subscribed to the
/// matching [`StateKey`](crate::manifest::StateKey); an unsubscribed key stays
/// `None` (and is omitted from the wire).
///
/// Additive by construction: a new key lands as a new `Option` field carrying
/// `#[serde(default)]`, so an old payload lacking it still decodes — same proto
/// version (see the crate root's compat rules). v1 carries `clock` only.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<ClockState>,
}
