//! Audio device + stream state.
//!
//! Facade module: all I/O lives in [`super::audio_native`], which talks
//! the native `PipeWire` protocol via the `pipewire` crate. This file owns
//! only the public data types, the runtime-registered [`PipewireHandles`]
//! and the read-side `Signal` accessors. The eight mutation functions
//! (`set_*` / `toggle_*`) and the service constructor are re-exported
//! from [`super::audio_native`] so existing callers keep using the
//! historical `services::pipewire::` path unchanged.
//!
//! # Historical note
//!
//! Earlier revisions shelled out to `pactl` / `wpctl` for everything
//! (subscribe, list, set). That backend stopped working on hosts without
//! the pulseaudio compatibility binaries installed (e.g. NixOS with
//! `services.pipewire.enable = true` but no `pulseaudio` package). The
//! v0.2 rewrite to a native client lives in `audio_native` and is the
//! single source of truth for everything in this module.

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::registry;

pub use super::audio_native::{
    PipewireService, service, set_default_sink, set_default_source, set_sink_mute, set_sink_volume,
    set_source_mute, set_source_volume, set_spectrum_active, set_stream_mute, set_stream_volume,
    set_volume, toggle_mute,
};

/// Number of frequency bands in an [`AudioSpectrum`] frame — a fixed low→high
/// split of the default sink's monitor (#405). Matches the plugin proto's
/// `SPECTRUM_BINS` so the host maps one onto the other 1:1.
pub const SPECTRUM_BINS: usize = 16;

/// One audio-reactive frame off the **default sink's monitor** (#405): a peak
/// level plus a [`SPECTRUM_BINS`]-band magnitude split, low→high frequency, both
/// normalized to `0.0..=1.0`. Produced ~20 Hz by the capture tap in
/// [`super::audio_native`] and surfaced through [`audio_spectrum`]. GTK- and
/// wire-free: the plugin host projects it onto the plugin proto's own
/// `AudioSpectrum` before pushing it to subscribing plugins.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioSpectrum {
    /// Peak (max-abs) sample magnitude over the analysis window, `0.0..=1.0`.
    pub peak: f32,
    /// Per-band normalized magnitude, index `0` = lowest frequency, each
    /// `0.0..=1.0`. Exactly [`SPECTRUM_BINS`] long.
    pub bins: [f32; SPECTRUM_BINS],
}

/// Default-sink volume snapshot, surfaced to the bar chip.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Volume {
    /// Linear gain, `0.0..=1.0` (may exceed 1.0 if user boosts above 100%).
    /// `0.0` is the default until the audio backend reports the real value.
    pub linear: f64,
    pub muted: bool,
}

/// One audio output device.
#[derive(Clone, Debug, PartialEq)]
pub struct Sink {
    /// `PipeWire` global id of the sink Node.
    pub id: u32,
    /// `node.name` — canonical identifier across sessions, used by every
    /// mutation function in this module.
    pub name: String,
    /// `node.description` — human-readable, falls back to `name` if unset.
    pub description: String,
    /// Linear gain in `0.0..=1.0+`, averaged across channels.
    pub volume: f64,
    pub muted: bool,
    /// `true` if this sink matches the current `default.audio.sink`
    /// metadata.
    pub is_default: bool,
}

/// One audio input device. Same shape as [`Sink`]; `PipeWire` treats the
/// graph symmetrically and so do we. Monitor sources (loopback from sinks)
/// are filtered out at the backend.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub volume: f64,
    pub muted: bool,
    pub is_default: bool,
}

/// An application playback stream — Firefox tabs, Spotify, etc.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackStream {
    pub id: u32,
    /// Best-effort app name. Falls back to `node.name` / `media.name` if
    /// the application doesn't set `application.name` (Spotify).
    pub app_name: String,
    /// `PipeWire` id of the sink this stream is currently routed to, or `0`
    /// when no link has been seen yet (brief transitional state).
    pub sink_id: u32,
    pub volume: f64,
    pub muted: bool,
}

/// An application record stream — browser mic capture, conference apps.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordStream {
    pub id: u32,
    pub app_name: String,
    /// `PipeWire` id of the source this stream is currently reading from.
    pub source_id: u32,
    pub volume: f64,
    pub muted: bool,
}

/// Runtime-registered handle bag — every signal accessor in this module
/// reads its `Mutable` here. Populated by [`PipewireService::start`].
///
/// Crate-public fields so [`super::audio_native`] can write into them
/// from the pipewire-loop thread (the `Mutable` type is `Send + Sync`).
#[doc(hidden)]
pub struct PipewireHandles {
    pub(crate) sink: Mutable<Volume>,
    pub(crate) sinks: Mutable<Vec<Sink>>,
    pub(crate) sources: Mutable<Vec<Source>>,
    pub(crate) streams: Mutable<Vec<PlaybackStream>>,
    pub(crate) record_streams: Mutable<Vec<RecordStream>>,
    /// Latest audio-reactive frame off the default sink's monitor (#405), or
    /// `None` while the capture tap is inactive (no subscriber) or hasn't
    /// produced its first window yet. Written from the pipewire-loop thread's
    /// capture `process` callback; read by [`audio_spectrum`].
    pub(crate) spectrum: Mutable<Option<AudioSpectrum>>,
}

impl Default for PipewireHandles {
    fn default() -> Self {
        Self {
            sink: Mutable::new(Volume::default()),
            sinks: Mutable::new(Vec::new()),
            sources: Mutable::new(Vec::new()),
            streams: Mutable::new(Vec::new()),
            record_streams: Mutable::new(Vec::new()),
            spectrum: Mutable::new(None),
        }
    }
}

/// Default-sink volume + mute. The bar chip binds this for its icon and
/// the OSD overlay reads it on every wheel/key event.
pub fn default_sink() -> impl Signal<Item = Volume> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .sink
            .signal_cloned()
    })
}

/// Every Audio/Sink the daemon knows about. Used by the audio modal.
pub fn sinks() -> impl Signal<Item = Vec<Sink>> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .sinks
            .signal_cloned()
    })
}

/// Every non-monitor Audio/Source. The microphone widget binds this.
pub fn sources() -> impl Signal<Item = Vec<Source>> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .sources
            .signal_cloned()
    })
}

/// Every Stream/Output/Audio (per-app playback). Shown in the audio modal.
pub fn playback_streams() -> impl Signal<Item = Vec<PlaybackStream>> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .streams
            .signal_cloned()
    })
}

/// Every Stream/Input/Audio (per-app record). Drives the mic-in-use chip.
pub fn record_streams() -> impl Signal<Item = Vec<RecordStream>> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .record_streams
            .signal_cloned()
    })
}

/// The latest audio-reactive spectrum off the default sink's monitor (#405), or
/// `None` while the capture is inactive. The plugin host binds this and forwards
/// each frame to plugins subscribing `StateKey::AudioSpectrum`. The capture only
/// runs while [`set_spectrum_active(true)`](set_spectrum_active) has been called
/// (i.e. a subscriber exists), so this stays `None` on an idle desktop.
pub fn audio_spectrum() -> impl Signal<Item = Option<AudioSpectrum>> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .spectrum
            .signal_cloned()
    })
}
