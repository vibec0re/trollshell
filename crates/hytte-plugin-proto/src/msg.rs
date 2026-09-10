//! The two message envelopes: plugin → host ([`PluginMsg`]) and host → plugin
//! ([`HostMsg`]).
//!
//! The channel is full-duplex: the plugin re-renders and pushes a
//! [`PluginMsg::Render`] on its **own** schedule (a host-state change, an
//! internal timer, an external fetch completing), not only in reply to a host
//! message. See the crate root for the framing/encoding.

use crate::effect::{ConsentDecision, DatasourceOutcome, Effect, EffectOutcome};
use crate::manifest::Manifest;
use crate::state::{AudioSpectrum, NowPlaying, StateSnapshot, UpcomingEvent};
use crate::wire::{EventKind, Node, NodeId};
use serde::{Deserialize, Serialize};

/// Plugin → host frames.
// `clippy::large_enum_variant`, tripped by #1050's 24-byte `hidden_on`:
// `Render` is 336 bytes (`tree` 144 + `panel` 144 + `hidden_on` 24 + `effects`
// 24) against `Register`'s 120, and the 216-byte gap just crosses the lint's
// 200-byte default — it was 192 before this field.
//
// Allowed rather than boxed, because the lint's cost model does not apply to
// this type. `PluginMsg` is a **per-frame envelope**: `codec::read_frame`
// deserializes exactly one, the reader loop destructures it immediately, and
// nothing in the workspace stores it — there is no `Vec<PluginMsg>`, no
// `mpsc` channel of them, no queue (the only `Vec<PluginMsg>` anywhere is a
// two-element golden-fixture table). So the "largest variant" cost is one
// stack move per frame, not per-element bloat across a collection.
//
// The fix the lint suggests is nevertheless a real (small) improvement, and is
// named here so it is a decision rather than an oversight: `panel:
// Option<Node>` reserves a full `Node` (144 B) on **every** frame although most
// plugins never render a panel, and `Option<Box<Node>>` would take `Render` to
// 200 B and the gap to 80 — serializing identically (`Box` is transparent to
// serde, so no fixture moves). It is not done here because it changes a field
// #1050 is not about, across the SDK, the host and seven plugin crates' test
// helpers; it wants its own PR.
#[allow(clippy::large_enum_variant)]
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
        /// The **connector names** (`"DP-1"`, `"eDP-1"`, `"HDMI-A-2"` — niri's
        /// and Wayland's own output names) of the monitors this frame's card
        /// must **not** be shown on (#1050).
        ///
        /// # Why the wire needs this at all
        ///
        /// A plugin renders **one** `tree` and the host mirrors it onto *every*
        /// monitor's bar/sidebar — one render mailbox, one reconciler per
        /// monitor over the same list. That is deliberate (a plugin should not
        /// have to know how many screens exist to draw a chip), but it means a
        /// plugin whose *verdict* is per-screen has no way to express it: before
        /// this field, a plugin could only fold its state off the **focused**
        /// output and every screen's chip followed that one answer. #1019's
        /// niri-layouts chip is the reported case — "hide unless the active
        /// workspace has ≥ 2 windows" was computed once, so screen B's chip
        /// showed screen A's count.
        ///
        /// `hidden_on` is the narrow fix: **same tree everywhere, visibility per
        /// screen.** It carries visibility only — a plugin that wants genuinely
        /// *different content* per monitor still cannot have it, and that is the
        /// intended limit (see the crate root's multi-monitor note).
        ///
        /// # Host semantics
        ///
        /// The host's per-monitor reconciler hides a card whose `hidden_on`
        /// contains **that monitor's** connector, and treats it as absent when
        /// deciding whether the region collapses on that monitor — the same
        /// treatment a tree that renders nothing already gets (#1042). The two
        /// rules are a disjunction: a card is hidden here if its tree renders
        /// nothing **or** this output is listed.
        ///
        /// A name matching no connected monitor is **ignored silently** — it
        /// hides nothing anywhere, and is never an error. There is no host-side
        /// validation of connector names (nor can there be a useful one: outputs
        /// come and go with hot-plug, and a name for a monitor that is currently
        /// off is a legitimate thing to carry). The host does log the ignored
        /// names at `debug` when the set changes, so a typo (`"DP1"` for
        /// `"DP-1"`) is diagnosable rather than merely silent.
        ///
        /// Names are compared **exactly** — no case folding, no normalisation.
        ///
        /// # Compat
        ///
        /// Additive, exactly like [`panel`](PluginMsg::Render::panel) and
        /// [`RunCommand.detached`](crate::effect::Effect::RunCommand): a
        /// defaulted field, not a new variant, so an empty list is
        /// `skip_serializing_if`-elided and a frame that does not use it is
        /// **byte-identical** to a pre-#1050 one. It therefore keeps
        /// [`PROTO_VERSION`](crate::PROTO_VERSION) *and* leaves
        /// [`VOCAB`](crate::VOCAB) alone — the vocabulary counter tracks
        /// appended *variants* (which an older peer cannot decode at all), while
        /// an unknown *field key* is skipped by `rmp-serde` on decode. An older
        /// host therefore ignores this field and mirrors the card everywhere,
        /// which is exactly the pre-#1050 behaviour.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        hidden_on: Vec<String>,
        effects: Vec<Effect>,
    },
    /// A diagnostic line surfaced in the host log, tagged with the plugin id.
    Log { level: LogLevel, msg: String },
    /// Liveness reply to a [`HostMsg::Ping`], echoing its `seq`.
    Pong { seq: u64 },
}

/// Host → plugin frames.
///
/// Appending a variant here ⇒ **bump [`VOCAB`](crate::VOCAB)** (#437) in addition
/// to the #305 opt-in gate: the push itself must be gated on a manifest opt-in so
/// an old plugin never receives it, and bumping the counter keeps it a faithful
/// census of the whole wire vocabulary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HostMsg {
    /// The full subscribed-state subset (sent initially and on every change,
    /// latest-wins — no per-key deltas).
    StateSnapshot { snapshot: StateSnapshot },
    /// A user interaction on a rendered node, addressed by its [`NodeId`].
    Event {
        node: NodeId,
        kind: EventKind,
        /// The **connector name** of the monitor whose copy of the card produced
        /// this event (#1050) — the other half of the multi-monitor story
        /// [`Render.hidden_on`](PluginMsg::Render::hidden_on) opens.
        ///
        /// One tree is mirrored onto every monitor, so before this field a click
        /// was un-attributable: a plugin acting on "the screen the user clicked
        /// on" had to guess, and the only available proxy — whatever output has
        /// keyboard focus — is not the same thing (#1019's layouts chip laid out
        /// the focused workspace, not the workspace on the screen whose chip was
        /// pressed). `output` makes it deterministic.
        ///
        /// `None` means **the host could not attribute the event to a screen**,
        /// not "the primary monitor" — treat it as unknown and fall back to
        /// whatever the plugin did before #1050. Today the host sends `None`
        /// from exactly one place, the plugin **drawer panel**: the drawer's
        /// page stack is built without a monitor in scope (`modal.rs`'s
        /// `build_pages_stack`), and the active-panel selection is a single
        /// process-wide value rather than a per-monitor one, so a panel event
        /// genuinely has no screen to name. Bar chips and sidebar cards always
        /// carry `Some`. A monitor with no connector name reported by GDK would
        /// also produce `None`.
        ///
        /// # Compat
        ///
        /// Additive: a defaulted, `skip_serializing_if`-elided field, so an
        /// `Event` without an output is **byte-identical** to a pre-#1050 one
        /// and a plugin built against the older proto skips the unknown key.
        /// Being a *field* and not a new [`HostMsg`] variant is also what keeps
        /// it outside the #305 opt-in rule — there is no undecodable variant tag
        /// for an old plugin to choke on — and outside the
        /// [`VOCAB`](crate::VOCAB) census for the same reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
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
    /// A datasource query forwarded to the **provider** plugin (#509),
    /// host→provider. The host routes a requester's
    /// [`Effect::DatasourceQuery`](crate::effect::Effect::DatasourceQuery) here after
    /// confirming this connection provides `datasource` (in
    /// [`Manifest::provides`](crate::manifest::Manifest::provides)) and declared
    /// `scope`. `request_id` is an **opaque host correlation**, not the requester's
    /// token — the host rewrites it on both legs so provider and requester id-spaces
    /// never collide; the provider echoes it verbatim in its
    /// [`Effect::DatasourceResult`](crate::effect::Effect::DatasourceResult). `params`
    /// is the requester's opaque JSON request (the provider↔requester contract).
    ///
    /// **Opt-in (#305) + capability:** sent only to a connection that declares
    /// [`Capability::DatasourceProvider`](crate::manifest::Capability::DatasourceProvider)
    /// **and** lists `datasource` in `provides`, so a plugin that isn't a registered
    /// provider never meets this name-tagged variant — the same additive gate as the
    /// domain-state pushes.
    DatasourceQuery {
        request_id: u64,
        datasource: String,
        scope: String,
        params: String,
    },
    /// The result of a datasource query the plugin issued (#509), host→requester —
    /// the answer to its
    /// [`Effect::DatasourceQuery`](crate::effect::Effect::DatasourceQuery), keyed by
    /// the same `request_id` the requester chose. Carries either the provider's
    /// answer or a host-synthesized error (no provider / denied scope / 10 s
    /// timeout). The request/response mate of `DatasourceQuery`, exactly as
    /// [`EffectResult`](HostMsg::EffectResult) is `RunCommand`'s. Surfaced to the SDK
    /// as `Input::DatasourceResult`.
    ///
    /// **Opt-in (#305):** the host routes this only to a connection that emitted a
    /// query — which requires
    /// [`Capability::DatasourceQuery`](crate::manifest::Capability::DatasourceQuery),
    /// or host cap-enforcement drops the effect — so a plugin that never declared
    /// that cap never receives this name-tagged variant it couldn't decode, the same
    /// additive rule as [`ConsentDecision`](HostMsg::ConsentDecision).
    DatasourceResult {
        request_id: u64,
        outcome: DatasourceOutcome,
    },
    /// A liveness probe; answer with [`PluginMsg::Pong`] carrying the same `seq`.
    Ping { seq: u64 },
    /// The host is going away; no further frames follow and the connection is
    /// about to close. Treat it as end-of-session — reconnect policy is the
    /// plugin's. (The `hytte-plugin` runtime redials with backoff rather than
    /// exiting: plugin units run `Restart=on-failure`, so a clean exit would
    /// strand the plugin across a host restart.)
    Shutdown,
    /// The host's own wire-vocabulary generation (#882) — the **advertisement**
    /// half of the vocabulary negotiation. Despite sitting last in this enum
    /// (appended, per the crate's compat rules — position is declaration order,
    /// not send order), it is the *first* frame the host sends after accepting a
    /// [`Register`](PluginMsg::Register), before any state snapshot.
    ///
    /// `vocab` is the host's [`VOCAB`](crate::VOCAB): the newest generation it
    /// can decode. The plugin resolves the agreed generation with
    /// [`Manifest::negotiated_vocab`](crate::manifest::Manifest::negotiated_vocab)
    /// and enables the negotiated features at or below it — today that means
    /// emitting [`Node::Preem`](crate::wire::Node::Preem) once the agreed
    /// generation reaches [`PREEM_VOCAB`](crate::preem::PREEM_VOCAB), and
    /// CPU-rasterising to [`Node::Pixels`](crate::wire::Node::Pixels) otherwise.
    ///
    /// **Opt-in (#305) — structural, by vocabulary.** The host sends this
    /// *only* to a plugin whose manifest carries a
    /// [`vocab_max`](crate::manifest::Manifest::vocab_max)
    /// ([`negotiates_vocab`](crate::manifest::Manifest::negotiates_vocab)). A
    /// plugin can only set that field if it was built against the proto that
    /// added it — the same proto that added this variant — so a pre-#882 binary
    /// never meets a `Hello` it couldn't decode. That is the
    /// [`EventKind::ValueChanged`](crate::wire::EventKind::ValueChanged)
    /// argument rather than a new [`StateKey`](crate::manifest::StateKey): the
    /// opt-in is the declaration itself, so nothing new is subscribable.
    ///
    /// **The send-gate is load-bearing, not a nicety.** A host that sends
    /// `Hello` unconditionally puts every pre-#882 plugin into the exact #437
    /// failure it was built to prevent: the plugin's `rmp-serde` cannot decode
    /// the unknown variant, its session dies, the SDK redials and meets the same
    /// frame again — a silent, permanent crash-loop that the `PROTO_VERSION`
    /// exact-match cannot catch, because both sides are the same version. Gate
    /// every send on
    /// [`Manifest::negotiates_vocab`](crate::manifest::Manifest::negotiates_vocab).
    ///
    /// **A plugin that receives no `Hello` must assume no negotiated features.**
    /// Silence is the legacy answer, not an error: an old host simply never
    /// sends one, and the plugin runs its fallback path — which is exactly what
    /// it did before #882.
    Hello { vocab: u16 },
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

    /// The #528 now-playing push round-trips (playing and idle), timing fields
    /// (#840) included — both a track the player timed and one it didn't.
    #[test]
    fn now_playing_push_round_trips() {
        use crate::state::NowPlaying;
        for now_playing in [
            NowPlaying {
                title: "Chrome Rain".into(),
                artist: "Choom".into(),
                playing: true,
                position_us: 83_000_000,
                length_us: 296_000_000,
            },
            NowPlaying {
                title: "Some Stream".into(),
                artist: String::new(),
                playing: true,
                position_us: 83_000_000,
                length_us: 0,
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
