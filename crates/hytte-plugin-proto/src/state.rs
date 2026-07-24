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
/// are **perceptual dBFS levels** in `0.0..=1.0` (#504): `1.0` at full scale,
/// falling to `0.0` ~48 dB below it, on the same logarithmic taper
/// `PipeWire`/`PulseAudio` show volume on. Human loudness is logarithmic, so a
/// consumer maps a level straight onto a bar height / needle angle and it fills
/// the range — a raw *linear* amplitude (the pre-#504 contract) crushed all but
/// the loudest content into the bottom of the bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioSpectrum {
    /// Peak window level on a perceptual dBFS scale, `0.0..=1.0` (#504).
    pub peak: f32,
    /// Per-band level on the same perceptual dBFS scale, index `0` = lowest
    /// frequency band, each `0.0..=1.0`. Exactly [`SPECTRUM_BINS`] long.
    pub bins: [f32; SPECTRUM_BINS],
}

/// Cap on the [`UpcomingEvent`] list the host pushes as
/// [`HostMsg::CalendarUpcoming`](crate::msg::HostMsg::CalendarUpcoming) (#484):
/// the **next five** events in the coming 24 hours. Small on purpose — this is a
/// briefing-shaped digest (caw's morning news, the infobroker's `get calendar`),
/// not the sidebar's full month view — so the wire frame stays tiny and the
/// host's projection is a cheap, push-on-change slice of the calendar service.
pub const MAX_UPCOMING_EVENTS: usize = 5;

/// One upcoming calendar event, projected GTK-/chrono-free onto the wire (#484):
/// the host fills these from its `hytte_services::calendar` handles (the EDS
/// backend), capped to the next [`MAX_UPCOMING_EVENTS`] in the coming 24 h and
/// pushed on change (EDS is signal-driven, not polled) to a plugin that opts into
/// [`StateKey::CalendarUpcoming`](crate::manifest::StateKey::CalendarUpcoming)
/// **and** holds [`Capability::Calendar`](crate::manifest::Capability::Calendar).
/// Times are Unix seconds (the consumer formats them in local time), so the wire
/// carries no timezone/`chrono` types.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpcomingEvent {
    /// Event start, Unix seconds.
    pub start_unix: i64,
    /// Event end, Unix seconds.
    pub end_unix: i64,
    /// The event's title (the calendar's `SUMMARY`; the host substitutes a
    /// placeholder for an empty one).
    pub title: String,
    /// The calendar source's human label (e.g. `"Personal"`, `"Work"`).
    pub calendar: String,
}

/// The current-track digest a plugin renders when it opts into
/// [`StateKey::NowPlaying`](crate::manifest::StateKey::NowPlaying) (#528, mirroring
/// the #405 spectrum projection): the host projects
/// `hytte_services::mpris`'s active player onto this GTK-free shape and pushes it
/// on change (latest-wins) as
/// [`HostMsg::NowPlaying`](crate::msg::HostMsg::NowPlaying). The motivating
/// consumer is the audio widget's dot-matrix marquee, which scrolls the live
/// title/artist while something plays and falls back to its own banner otherwise.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NowPlaying {
    /// The track title (`xesam:title`), or empty when nothing is playing.
    pub title: String,
    /// The track artist(s) (`xesam:artist`, comma-joined), or empty.
    pub artist: String,
    /// Whether the active player is currently *playing* (as opposed to paused /
    /// stopped / absent). A consumer keys its "show the track vs the fallback"
    /// choice off this.
    pub playing: bool,
}
