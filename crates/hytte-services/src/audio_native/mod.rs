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
//! - `types` — all internal data types (`AudioRole`, `NodeEntry`,
//!   `AudioState`, `Command`, etc.)
//! - `pod` — pure `SPA` pod serialization/parsing helpers
//!   (`build_props_pod`, `decode_props`, `avg_volume`,
//!   `parse_default_name`, `pick_app_name`)
//! - `graph` — link-edge routing helpers (`resolve_link_dest`,
//!   `resolve_link_source`)
//! - `loop_` — the `PipeWire` mainloop thread, registry walker, command
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

/// Build or tear down the audio spectrum capture tap (#405/#581). The plugin host
/// calls this with `true` when the first on-screen `StateKey::AudioSpectrum`
/// subscriber appears and `false` when the last one goes away.
///
/// `true` **creates** the `trollshell-spectrum` capture node and `false`
/// **destroys** it — it is not an active/paused toggle over a permanently
/// connected stream. So with nothing subscribed there is no capture client in the
/// graph for `wpctl status` / pavucontrol / Helvum to list at all, which is what
/// "only records when needed" has to mean to be worth anything (#581).
/// Fire-and-forget like the other setters.
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
    use super::types::{AudioRole, DefaultKind, PendingDefaults, SpectrumAction};

    /// A pick made before the `default` metadata binds is held, not dropped —
    /// the last one per kind, since a later pick supersedes an earlier one —
    /// and handed over exactly once when it binds. Dropping it was what lost
    /// `bluetooth_audio`'s switch, which never re-sends.
    ///
    /// The effectful half — `write_default` parking it and the registry walker
    /// writing it on bind — needs a bound metadata proxy, i.e. a live daemon;
    /// it is on the PR's live-verify list.
    #[test]
    fn a_default_picked_before_the_metadata_binds_is_held_until_it_does() {
        let mut pending = PendingDefaults::default();
        assert!(
            pending.drain().is_empty(),
            "nothing parked, nothing written"
        );

        pending.park(DefaultKind::Sink, "alsa_output.analog".to_owned());
        pending.park(DefaultKind::Source, "alsa_input.analog".to_owned());
        pending.park(
            DefaultKind::Sink,
            "bluez_output.AC_C5_8B_11_22_33.1".to_owned(),
        );
        assert_eq!(
            pending.drain(),
            vec![
                (
                    DefaultKind::Sink,
                    "bluez_output.AC_C5_8B_11_22_33.1".to_owned()
                ),
                (DefaultKind::Source, "alsa_input.analog".to_owned()),
            ],
            "the last pick per kind is the one written",
        );
        assert_eq!(
            pending,
            PendingDefaults::default(),
            "a drained pick must not be written a second time on the next bind",
        );
    }

    /// A user's pick is **written** to the configured key and the default is
    /// **read** back off the effective one — the pair `WirePlumber` keeps (see
    /// `DefaultKind`'s doc for the scripts). Writing the effective key was the
    /// bug: the default flipped, and the next rescan (a device plugged, a
    /// Bluetooth sink connecting, a jack's `Route` change) wrote the old choice
    /// back, and nothing reached the state file.
    ///
    /// Falsify by pointing `configured_key` back at `default.audio.*`: the
    /// write and read keys collapse into one and this fails.
    #[test]
    fn a_default_pick_is_written_to_the_configured_key_and_read_off_the_effective_one() {
        assert_eq!(
            DefaultKind::Sink.configured_key(),
            "default.configured.audio.sink",
        );
        assert_eq!(
            DefaultKind::Source.configured_key(),
            "default.configured.audio.source",
        );
        assert_eq!(DefaultKind::Sink.effective_key(), "default.audio.sink");
        assert_eq!(DefaultKind::Source.effective_key(), "default.audio.source");
        for kind in [DefaultKind::Sink, DefaultKind::Source] {
            assert_ne!(
                kind.configured_key(),
                kind.effective_key(),
                "{kind:?}: a pick written where the default is read is overwritten on the next rescan",
            );
        }
    }

    /// The metadata listener follows the effective keys and nothing else — the
    /// configured key's own change event (which our write produces) must not
    /// flip `is_default` before the session manager has resolved it.
    #[test]
    fn the_listener_follows_only_the_effective_keys() {
        assert_eq!(
            DefaultKind::from_effective_key("default.audio.sink"),
            Some(DefaultKind::Sink),
        );
        assert_eq!(
            DefaultKind::from_effective_key("default.audio.source"),
            Some(DefaultKind::Source),
        );
        for other in [
            "default.configured.audio.sink",
            "default.configured.audio.source",
            "default.video.source",
            "",
        ] {
            assert_eq!(DefaultKind::from_effective_key(other), None, "{other:?}");
        }
    }

    /// #581: the tap is demand-built, so the (requested, currently-built) truth
    /// table is the whole contract. Pinned here because the effectful half needs a
    /// live `PipeWire` daemon and can't be exercised in CI.
    #[test]
    fn spectrum_action_truth_table() {
        // 0→1 with nothing built: create the node, then start it.
        assert_eq!(
            SpectrumAction::decide(true, false),
            SpectrumAction::BuildAndActivate,
        );
        // Demand re-asserted over a live stream must NOT build a second tap on
        // the same monitor — just make sure the existing one is running.
        assert_eq!(SpectrumAction::decide(true, true), SpectrumAction::Activate);
        // 1→0: a full teardown. Regression guard for the actual bug — pausing
        // instead would leave the node listed as a capture client all session.
        assert_eq!(
            SpectrumAction::decide(false, true),
            SpectrumAction::Teardown,
        );
        // `false` with nothing built (the last subscriber of a session that never
        // managed to build a tap): no work, and above all no build.
        assert_eq!(
            SpectrumAction::decide(false, false),
            SpectrumAction::Nothing,
        );
    }

    /// The two states that must never construct a stream, stated separately from
    /// the table so the intent survives a future refactor of the enum.
    #[test]
    fn spectrum_action_never_builds_when_demand_is_off() {
        for built in [true, false] {
            assert_ne!(
                SpectrumAction::decide(false, built),
                SpectrumAction::BuildAndActivate,
            );
        }
    }

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
