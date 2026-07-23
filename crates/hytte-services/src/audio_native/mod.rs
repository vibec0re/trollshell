//! Native `PipeWire` audio backend — the production audio service for
//! `trollshell`. Re-exported from [`super::pipewire`] as
//! `pipewire::{PipewireService, service, set_*, toggle_mute}` so callers
//! continue using the historical `services::pipewire::*` path.
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
mod spectrum;
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
// All eight functions are fire-and-forget and silently drop the command if
// the audio service hasn't started yet (loop hasn't installed `COMMAND_TX`)
// or the receiver has gone away (process tearing down). Real failures land
// in tracing.

pub fn set_sink_volume(name: &str, linear: f64) {
    send_command(Command::SetSinkVolume {
        name: name.to_string(),
        linear,
    });
}

pub fn set_source_volume(name: &str, linear: f64) {
    send_command(Command::SetSourceVolume {
        name: name.to_string(),
        linear,
    });
}

pub fn set_stream_volume(id: u32, linear: f64) {
    send_command(Command::SetStreamVolume { id, linear });
}

pub fn set_sink_mute(name: &str, mute: bool) {
    send_command(Command::SetSinkMute {
        name: name.to_string(),
        mute,
    });
}

pub fn set_source_mute(name: &str, mute: bool) {
    send_command(Command::SetSourceMute {
        name: name.to_string(),
        mute,
    });
}

pub fn set_stream_mute(id: u32, mute: bool) {
    send_command(Command::SetStreamMute { id, mute });
}

pub fn set_default_sink(name: &str) {
    send_command(Command::SetDefaultSink {
        name: name.to_string(),
    });
}

pub fn set_default_source(name: &str) {
    send_command(Command::SetDefaultSource {
        name: name.to_string(),
    });
}

/// Activate or deactivate the audio spectrum capture tap (#405). The plugin host
/// calls this with `true` when the first `StateKey::AudioSpectrum` subscriber
/// connects and `false` when the last one leaves, so the monitor is only tapped
/// while something is listening. Fire-and-forget like the other setters.
pub fn set_spectrum_active(active: bool) {
    send_command(Command::SetSpectrumActive { active });
}

/// Set volume on whichever sink is currently the default. Reads the
/// default-sink name from the cached `sinks()` snapshot via `is_default`.
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

/// Toggle mute on the current default sink.
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
