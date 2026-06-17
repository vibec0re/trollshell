//! Native `PipeWire` audio backend (work in progress).
//!
//! This module is being built phase by phase to replace the `pactl`/`wpctl`
//! shell-out in [`super::pipewire`]. Until Phase 6 lands, it is NOT wired
//! into the service registry — callers still go through `services::pipewire`
//! which keeps shelling out. Only `cargo check` exercises this code today.
//!
//! # Threading
//!
//! `libpipewire` is not `Send` / `Sync`. The mainloop, context, core, and
//! registry are all `Rc`-typed in the Rust bindings and must live on one
//! thread. We follow the canonical pipewire-rs pattern: spawn a dedicated
//! `std::thread`, build the mainloop there, run it indefinitely. Commands
//! from the tokio side cross the thread boundary via
//! [`pipewire::channel`], which the mainloop polls as a loop source.
//! Outbound state (sink/source/stream snapshots) lands in `Mutable`s from
//! `futures_signals`, which are `Send + Sync`, so subscribers on the GTK
//! main thread read them without further glue.
//!
//! # Phase 1 (done)
//!
//! Scaffold: mainloop owner thread, registry walker, Node classification
//! by `media.class`, monitor-source filtering, Metadata + Link logging.
//!
//! # Phase 2
//!
//! For each classified Audio Node, `registry.bind` a `Node` proxy, attach
//! a `.param` listener, and `subscribe_params(&[ParamType::Props])`.
//! Decode the `spa_pod` payload: `SPA_PROP_channelVolumes` (array of f32
//! linear gains per channel) and `SPA_PROP_mute` (bool). Walk the cache
//! after every change and `Mutable::set` fresh `Vec<Sink>`, `Vec<Source>`,
//! `Vec<PlaybackStream>`, `Vec<RecordStream>`, and `Volume` snapshots into
//! the existing `super::pipewire::PipewireHandles` so cutover in Phase 6
//! is a one-line swap.
//!
//! # Phase 3
//!
//! Mutation. A [`pipewire::channel`] bridges tokio-side callers into the
//! pw-loop thread. The eight `set_*` / `toggle_*` functions enqueue a
//! [`Command`], the receiver attached to the loop resolves the target node
//! by name (sinks/sources) or by id (streams), builds a `SPA_TYPE_OBJECT_\
//! Props` pod via [`libspa::pod`], and calls `node.set_param(Props, 0, pod)`.
//! For volume, the builder preserves the live channel count from the cache
//! so a stereo sink stays stereo after `set_sink_volume`.
//!
//! # Phase 4
//!
//! Default sink/source resolution via the `default` Metadata global. We
//! bind it, listen for property events with keys `default.audio.sink` and
//! `default.audio.source` (values are JSON: `{"name":"<node.name>"}`), and
//! cache the resolved names. After every metadata change `emit_snapshots`
//! flags the matching Sink/Source with `is_default = true` and the
//! `Volume` signal at the bar reflects the current default sink's level.
//! Writes go the other way via `Metadata::set_property`.
//!
//! # Phase 5 (this commit)
//!
//! Stream→sink routing via `PipeWire` Link globals. Each Link's props dict
//! carries `link.output.node` and `link.input.node` (pipewire node ids).
//! We index links by their output node id; on snapshot emission, every
//! `PlaybackStream` reads its `sink_id` from the link whose output is the
//! stream itself. `RecordStreams` mirror: `source_id` comes from the link's
//! `output` node (the source) when the stream is the `input`. With this
//! the audio modal can group streams under their target sink.
//!
//! # Module layout
//!
//! The implementation is split across four submodules:
//!
//! - [`types`] — all internal data types (`AudioRole`, `NodeEntry`,
//!   `AudioState`, `Command`, etc.)
//! - [`pod`] — pure `SPA` pod serialization/parsing helpers
//!   (`build_props_pod`, `decode_props`, `avg_volume`,
//!   `parse_default_name`, `pick_app_name`)
//! - [`graph`] — link-edge routing helpers (`resolve_link_dest`,
//!   `resolve_link_source`)
//! - [`loop_`] — the `PipeWire` mainloop thread, registry walker, command
//!   dispatcher, and `emit_snapshots`

mod graph;
mod loop_;
mod pod;
mod types;

use hytte_reactive::{Service, registry};

use super::pipewire::PipewireHandles;
use loop_::{send_command, spawn_mainloop};
use types::{Command, clone_handles};

/// Drives the dedicated pipewire-loop thread. Re-exported from
/// [`super::pipewire`] as `pipewire::PipewireService` so callers keep
/// using the historical `services::pipewire::service()` path.
pub struct PipewireService;

impl Service for PipewireService {
    type Handles = PipewireHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = PipewireHandles::default();
        let handles_for_thread = clone_handles(&handles);
        spawn_mainloop(handles_for_thread);
        handles
    }
}

/// Returns the audio service to register with the hytte runtime.
#[must_use]
pub fn service() -> PipewireService {
    PipewireService
}

// ── Mutation surface ──────────────────────────────────────────────────────
//
// Mirrors `super::pipewire`'s public mutation API so Phase 6 cutover is a
// `pub use audio_native::*` swap. All eight functions are fire-and-forget
// and silently drop the command if the audio service hasn't started yet
// (loop hasn't installed `COMMAND_TX`) or the receiver has gone away
// (process tearing down). Real failures land in tracing.

#[allow(dead_code)] // Phase 6 swaps this in for super::pipewire's version
pub fn set_sink_volume(name: &str, linear: f64) {
    send_command(Command::SetSinkVolume {
        name: name.to_string(),
        linear,
    });
}

#[allow(dead_code)]
pub fn set_source_volume(name: &str, linear: f64) {
    send_command(Command::SetSourceVolume {
        name: name.to_string(),
        linear,
    });
}

#[allow(dead_code)]
pub fn set_stream_volume(id: u32, linear: f64) {
    send_command(Command::SetStreamVolume { id, linear });
}

#[allow(dead_code)]
pub fn set_sink_mute(name: &str, mute: bool) {
    send_command(Command::SetSinkMute {
        name: name.to_string(),
        mute,
    });
}

#[allow(dead_code)]
pub fn set_source_mute(name: &str, mute: bool) {
    send_command(Command::SetSourceMute {
        name: name.to_string(),
        mute,
    });
}

#[allow(dead_code)]
pub fn set_stream_mute(id: u32, mute: bool) {
    send_command(Command::SetStreamMute { id, mute });
}

#[allow(dead_code)]
pub fn set_default_sink(name: &str) {
    send_command(Command::SetDefaultSink {
        name: name.to_string(),
    });
}

#[allow(dead_code)]
pub fn set_default_source(name: &str) {
    send_command(Command::SetDefaultSource {
        name: name.to_string(),
    });
}

/// Set volume on whichever sink is currently the default. The default-sink
/// resolution still lives in Phase 4's Metadata path, so for Phase 3 this
/// reads the last default-sink name out of the cached `sinks()` snapshot.
/// Once Phase 4 lands, `is_default` actually gets set and this becomes
/// reliable.
#[allow(dead_code)]
pub fn set_volume(linear: f64) {
    let name = registry::with(|r| {
        r.get::<PipewireHandles>().and_then(|h| {
            h.sinks
                .lock_ref()
                .iter()
                .find(|s| s.is_default)
                .map(|s| s.name.clone())
        })
    });
    if let Some(name) = name {
        set_sink_volume(&name, linear);
    } else {
        tracing::debug!("audio_native: set_volume without default sink");
    }
}

/// Toggle mute on the current default sink. Same default-resolution caveat
/// as [`set_volume`] until Phase 4 lands.
#[allow(dead_code)]
pub fn toggle_mute() {
    let target = registry::with(|r| {
        r.get::<PipewireHandles>().and_then(|h| {
            h.sinks
                .lock_ref()
                .iter()
                .find(|s| s.is_default)
                .map(|s| (s.name.clone(), s.muted))
        })
    });
    if let Some((name, muted)) = target {
        set_sink_mute(&name, !muted);
    } else {
        tracing::debug!("audio_native: toggle_mute without default sink");
    }
}

#[cfg(test)]
mod tests {
    use super::types::AudioRole;

    #[test]
    fn audio_role_classifies_the_four_relevant_classes() {
        assert_eq!(
            AudioRole::from_media_class("Audio/Sink"),
            Some(AudioRole::Sink),
        );
        assert_eq!(
            AudioRole::from_media_class("Audio/Source"),
            Some(AudioRole::Source),
        );
        assert_eq!(
            AudioRole::from_media_class("Stream/Output/Audio"),
            Some(AudioRole::OutputStream),
        );
        assert_eq!(
            AudioRole::from_media_class("Stream/Input/Audio"),
            Some(AudioRole::InputStream),
        );
    }

    #[test]
    fn audio_role_ignores_non_audio_classes() {
        // Cameras, MIDI, virtual surface nodes — anything that isn't one
        // of the four explicit classes must return None so the registry
        // walker skips it instead of mis-classifying as a sink.
        assert_eq!(AudioRole::from_media_class("Video/Source"), None);
        assert_eq!(AudioRole::from_media_class("Midi/Bridge"), None);
        assert_eq!(AudioRole::from_media_class(""), None);
        assert_eq!(AudioRole::from_media_class("Audio/Duplex"), None);
    }
}
