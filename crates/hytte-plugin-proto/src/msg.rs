//! The two message envelopes: plugin → host ([`PluginMsg`]) and host → plugin
//! ([`HostMsg`]).
//!
//! The channel is full-duplex: the plugin re-renders and pushes a
//! [`PluginMsg::Render`] on its **own** schedule (a host-state change, an
//! internal timer, an external fetch completing), not only in reply to a host
//! message. See the crate root for the framing/encoding.

use crate::effect::{ConsentDecision, Effect, EffectOutcome};
use crate::manifest::Manifest;
use crate::state::{AudioSpectrum, NowPlaying, StateSnapshot, UpcomingEvent};
use crate::wire::{EventKind, Node, NodeId};
use serde::{Deserialize, Serialize};

/// Plugin → host frames.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PluginMsg {
    /// First frame after dialing in: self-identify. The host validates
    /// `manifest.proto` (exact match) and grants caps, or drops the connection.
    Register { manifest: Manifest },
    /// A rendered view plus the shell effects to broker for it. Bundled so a
    /// (tree, effects) frame is applied atomically.
    Render {
        tree: Node,
        /// The plugin's optional drawer *panel* tree (#349 PR2) — a second,
        /// independent [`Node`] tree the host mounts as a dedicated drawer
        /// page, opened by
        /// [`Effect::OpenPage(Page::PluginSelf)`](crate::effect::Page::PluginSelf).
        /// `None` (the default, and what a pre-PR2 frame decodes to) = the
        /// plugin has no panel; its chip/card is display-only. Additive:
        /// `#[serde(default, skip_serializing_if = "Option::is_none")]` keeps a
        /// panel-less frame byte-identical on the wire and `PROTO_VERSION`
        /// unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        panel: Option<Node>,
        effects: Vec<Effect>,
    },
    /// A diagnostic line surfaced in the host log, tagged with the plugin id.
    Log { level: LogLevel, msg: String },
    /// Liveness reply to a [`HostMsg::Ping`], echoing its `seq`.
    Pong { seq: u64 },
}

/// Host → plugin frames.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HostMsg {
    /// The full subscribed-state subset (sent initially and on every change,
    /// latest-wins — no per-key deltas).
    StateSnapshot { snapshot: StateSnapshot },
    /// A user interaction on a rendered node, addressed by its [`NodeId`].
    Event { node: NodeId, kind: EventKind },
    /// The result of a brokered [`Effect::RunCommand`](crate::effect::Effect::RunCommand),
    /// keyed by the command's `id`.
    EffectResult { id: u64, outcome: EffectOutcome },
    /// The plugin's mount surface became visible or hidden — e.g. the sidebar
    /// its card lives in was opened / closed. Pushed on every open/close edge
    /// and **once at register** (so a reconnecting plugin starts in the right
    /// state), letting a plugin park its own pollers/timers while nobody is
    /// looking (the shell already gates its built-in pollers this way).
    ///
    /// **Delivery is latest-wins.** Unlike an [`Event`](HostMsg::Event) (a
    /// one-shot interaction), visibility is *state*: a burst of open/close
    /// toggles may coalesce to the newest `visible` value, and that is correct —
    /// the receiver only ever needs the current state, never the intermediate
    /// edges. This is explicitly **not** a #277-style lossiness concern (which
    /// is about dropping one-shot effects); dropping a superseded visibility
    /// value loses nothing.
    ///
    /// With multiple monitors a card mirrors onto every monitor's sidebar, so
    /// the host sends `visible: true` while **any** sidebar showing it is open
    /// (OR across monitors) and `false` only once they are all closed.
    SlotVisibility { visible: bool },
    /// The desktop accent color the host resolved from libadwaita's
    /// `@accent_color` (#376), delivered so an out-of-process plugin — which
    /// can't read GTK/adwaita itself — can tint its `preem` widgets' **default**
    /// color to match the shell. `color` is an opaque RGBA byte quad
    /// (`[r, g, b, a]`, matching `preem`'s pixel layout); `None` means the host
    /// couldn't resolve one, in which case the kit keeps its hard-coded per-style
    /// default. An explicit plugin palette always wins — accent is only the
    /// fallback default.
    ///
    /// **Opt-in (#305):** sent *only* to a plugin that subscribes
    /// [`StateKey::Accent`](crate::manifest::StateKey::Accent), so appending this
    /// name-tagged variant stays additive — a pre-#376 binary that never declared
    /// the key never receives (and never fails to decode) it. The `hytte-plugin`
    /// SDK auto-declares that subscription, so accent tracking is transparent to
    /// the plugin author. Sent once at session start and again on every
    /// accent/scheme change (#396) — always latest-wins on re-send.
    Accent { color: Option<[u8; 4]> },
    /// The latest audio-reactive spectrum off the default sink's monitor (#405),
    /// pushed ~20 Hz **latest-wins** so a slow plugin just skips frames. The
    /// payload is a peak level plus a fixed low→high band split — see
    /// [`AudioSpectrum`].
    ///
    /// **Opt-in (#305):** sent *only* to a plugin that subscribes
    /// [`StateKey::AudioSpectrum`](crate::manifest::StateKey::AudioSpectrum), so
    /// appending this name-tagged variant stays additive — a pre-#405 binary that
    /// never declared the key never receives (and never fails to decode) it.
    /// Unlike [`Accent`](HostMsg::Accent) the SDK does not auto-declare the
    /// subscription: the spectrum is data a plugin's own view renders, so the
    /// plugin opts in explicitly, and the host only runs the capture while a
    /// subscriber is present.
    AudioSpectrum { spectrum: AudioSpectrum },
    /// The human's answer to an [`Effect::RequestConsent`](crate::effect::Effect::RequestConsent)
    /// prompt (#487 phase 1b), keyed by the same `request_id` the plugin chose.
    /// The request/response mate of `RequestConsent`, exactly as
    /// [`EffectResult`](HostMsg::EffectResult) is `RunCommand`'s. Surfaced to the
    /// SDK as `Input::ConsentDecision`.
    ///
    /// **Opt-in (#305):** a new host→plugin push, so it must be gated on an opt-in
    /// the plugin declared — here [`Capability::Consent`](crate::manifest::Capability::Consent).
    /// The host only sends this to a connection that actually emitted a
    /// `RequestConsent` (which requires the `Consent` cap, or host cap-enforcement
    /// drops the effect), so a pre-1b plugin that never declared `Consent` never
    /// receives this name-tagged variant it couldn't decode — the same additive
    /// rule as [`Accent`](HostMsg::Accent)/[`AudioSpectrum`](HostMsg::AudioSpectrum).
    ConsentDecision {
        request_id: u64,
        decision: ConsentDecision,
    },
    /// The next few upcoming calendar events (#484), pushed on change (EDS is
    /// signal-driven) as a small digest — the next
    /// [`MAX_UPCOMING_EVENTS`](crate::state::MAX_UPCOMING_EVENTS)
    /// [`UpcomingEvent`]s in the coming 24 h — off the host's
    /// `hytte_services::calendar` handles.
    ///
    /// **Opt-in (#305) + capability:** sent only to a plugin that subscribes
    /// [`StateKey::CalendarUpcoming`](crate::manifest::StateKey::CalendarUpcoming)
    /// **and** declares [`Capability::Calendar`](crate::manifest::Capability::Calendar)
    /// — a calendar is personal data, so the host gates the push on the capability
    /// on top of the subscription (a subscribe-only plugin is refused it and
    /// warned). A pre-#484 binary that declares neither never meets this
    /// name-tagged variant, keeping the addition additive.
    CalendarUpcoming { events: Vec<UpcomingEvent> },
    /// The session's logind `LockedHint` (#484): `true` while the session is
    /// locked. Pushed on change so a plugin can fire a "first unlock" action or
    /// blank sensitive content while locked.
    ///
    /// **Opt-in (#305) + capability:** sent only to a plugin that subscribes
    /// [`StateKey::SessionLocked`](crate::manifest::StateKey::SessionLocked) **and**
    /// declares [`Capability::SessionState`](crate::manifest::Capability::SessionState)
    /// — the same subscribe-and-capability rule as [`CalendarUpcoming`](HostMsg::CalendarUpcoming).
    SessionLocked { locked: bool },
    /// The current-track digest off the mpris active player (#528), pushed on
    /// change (latest-wins), exactly the way [`AudioSpectrum`](HostMsg::AudioSpectrum)
    /// projected the spectrum. See [`NowPlaying`].
    ///
    /// **Opt-in (#305) + capability:** sent only to a plugin that subscribes
    /// [`StateKey::NowPlaying`](crate::manifest::StateKey::NowPlaying) **and**
    /// declares [`Capability::NowPlaying`](crate::manifest::Capability::NowPlaying).
    NowPlaying { now_playing: NowPlaying },
    /// A liveness probe; answer with [`PluginMsg::Pong`] carrying the same `seq`.
    Ping { seq: u64 },
    /// The host is going away; no further frames follow and the connection is
    /// about to close. Treat it as end-of-session — reconnect policy is the
    /// plugin's. (The `hytte-plugin` runtime redials with backoff rather than
    /// exiting: plugin units run `Restart=on-failure`, so a clean exit would
    /// strand the plugin across a host restart.)
    Shutdown,
}

/// Severity for [`PluginMsg::Log`]. Mirrors the host's `tracing` levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[cfg(test)]
mod tests {
    use super::HostMsg;
    use crate::codec::{decode, encode};
    use crate::manifest::{Manifest, Mount, StateKey};

    /// The #376 accent push round-trips, carrying both a resolved color and the
    /// unresolved (`None`) case byte-for-byte.
    #[test]
    fn accent_push_round_trips() {
        for color in [Some([0x35, 0x84, 0xe4, 0xff]), Some([0, 0, 0, 0]), None] {
            let msg = HostMsg::Accent { color };
            let back = decode::<HostMsg>(&encode(&msg)).expect("accent frame decodes");
            assert_eq!(back, msg);
        }
    }

    /// `StateKey::Accent` is a plain name-tagged variant, so a manifest carrying
    /// it round-trips — the additive opt-in a plugin declares to receive the
    /// accent push.
    #[test]
    fn accent_subscription_round_trips() {
        let mut manifest = Manifest::new("preem-plugin", Mount::SidebarTop);
        manifest.subscribes = vec![StateKey::Clock, StateKey::Accent];
        let back = decode::<Manifest>(&encode(&manifest)).expect("manifest decodes");
        assert_eq!(back, manifest);
    }

    /// The #405 audio-spectrum push round-trips, carrying its peak and all
    /// [`SPECTRUM_BINS`](crate::state::SPECTRUM_BINS) band values byte-for-byte —
    /// the `[f32; 16]` array encodes as a msgpack sequence and decodes back
    /// exactly.
    #[test]
    fn audio_spectrum_push_round_trips() {
        use crate::state::{AudioSpectrum, SPECTRUM_BINS};
        let mut bins = [0.0_f32; SPECTRUM_BINS];
        for (i, b) in bins.iter_mut().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            {
                *b = i as f32 / SPECTRUM_BINS as f32;
            }
        }
        let msg = HostMsg::AudioSpectrum {
            spectrum: AudioSpectrum { peak: 0.75, bins },
        };
        let back = decode::<HostMsg>(&encode(&msg)).expect("spectrum frame decodes");
        assert_eq!(back, msg);
    }

    /// `StateKey::AudioSpectrum` is a plain name-tagged variant, so a manifest
    /// declaring it round-trips — the additive opt-in a plugin uses to receive the
    /// spectrum push (#405).
    #[test]
    fn audio_spectrum_subscription_round_trips() {
        let mut manifest = Manifest::new("scope-plugin", Mount::SidebarTop);
        manifest.subscribes = vec![StateKey::Clock, StateKey::AudioSpectrum];
        let back = decode::<Manifest>(&encode(&manifest)).expect("manifest decodes");
        assert_eq!(back, manifest);
    }

    /// The #484 upcoming-calendar push round-trips, carrying the whole event list
    /// (and the empty "no upcoming events" case) byte-for-byte.
    #[test]
    fn calendar_upcoming_push_round_trips() {
        use crate::state::UpcomingEvent;
        for events in [
            Vec::new(),
            vec![
                UpcomingEvent {
                    start_unix: 1_752_248_940,
                    end_unix: 1_752_252_540,
                    title: "standup".into(),
                    calendar: "Work".into(),
                },
                UpcomingEvent {
                    start_unix: 1_752_260_000,
                    end_unix: 1_752_263_600,
                    title: "the thing".into(),
                    calendar: "Personal".into(),
                },
            ],
        ] {
            let msg = HostMsg::CalendarUpcoming { events };
            let back = decode::<HostMsg>(&encode(&msg)).expect("calendar frame decodes");
            assert_eq!(back, msg);
        }
    }

    /// The #484 session-locked push round-trips both boolean states.
    #[test]
    fn session_locked_push_round_trips() {
        for locked in [true, false] {
            let msg = HostMsg::SessionLocked { locked };
            let back = decode::<HostMsg>(&encode(&msg)).expect("locked frame decodes");
            assert_eq!(back, msg);
        }
    }

    /// The #528 now-playing push round-trips (playing and idle).
    #[test]
    fn now_playing_push_round_trips() {
        use crate::state::NowPlaying;
        for now_playing in [
            NowPlaying {
                title: "Chrome Rain".into(),
                artist: "Choom".into(),
                playing: true,
            },
            NowPlaying::default(),
        ] {
            let msg = HostMsg::NowPlaying { now_playing };
            let back = decode::<HostMsg>(&encode(&msg)).expect("now-playing frame decodes");
            assert_eq!(back, msg);
        }
    }

    /// The three #484/#528 domain subscriptions and their gating capabilities are
    /// plain name-tagged variants, so a manifest declaring them round-trips.
    #[test]
    fn domain_subscriptions_and_capabilities_round_trip() {
        use crate::manifest::Capability;
        let mut manifest = Manifest::new("domain-plugin", Mount::SidebarTop);
        manifest.subscribes = vec![
            StateKey::CalendarUpcoming,
            StateKey::SessionLocked,
            StateKey::NowPlaying,
        ];
        manifest.capabilities = vec![
            Capability::Calendar,
            Capability::SessionState,
            Capability::NowPlaying,
        ];
        let back = decode::<Manifest>(&encode(&manifest)).expect("manifest decodes");
        assert_eq!(back, manifest);
    }
}
