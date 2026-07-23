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

/// Number of frequency bands in an [`AudioSpectrum`] payload — a fixed,
/// low→high split of the default sink's monitor (#405). Sixteen is enough for a
/// legible bar-spectrum / scope tile while keeping the wire frame tiny (a peak
/// plus 16 floats, pushed ~20 Hz — well under what `Node::Pixels` animation
/// already moves the other way).
pub const SPECTRUM_BINS: usize = 16;

/// One audio-reactive frame off the **default sink's monitor** (#405): a peak
/// level plus a [`SPECTRUM_BINS`]-band magnitude split, low→high frequency.
///
/// The host taps the monitor through `PipeWire`, downsamples to this shape at
/// ~20 Hz, and pushes it to a plugin that subscribes
/// [`StateKey::AudioSpectrum`](crate::manifest::StateKey::AudioSpectrum) as a
/// [`HostMsg::AudioSpectrum`](crate::msg::HostMsg::AudioSpectrum) — **latest-wins**,
/// so a plugin that renders slower than 20 Hz simply skips frames. Both values
/// are already normalized to `0.0..=1.0` (a heuristic display gain, clamped), so
/// a consumer maps them straight onto bar heights / needle angles without its
/// own calibration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioSpectrum {
    /// Peak (max-abs) sample magnitude over the analysis window, `0.0..=1.0`.
    pub peak: f32,
    /// Per-band normalized magnitude, index `0` = lowest frequency band, each
    /// `0.0..=1.0`. Exactly [`SPECTRUM_BINS`] long.
    pub bins: [f32; SPECTRUM_BINS],
}
