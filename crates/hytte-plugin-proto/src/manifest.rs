//! The plugin manifest, the subscription keys, and the capability model.

use crate::codec::ProtoError;
use crate::{PROTO_VERSION, VOCAB, VOCAB_UNCONDITIONAL};
use serde::{Deserialize, Serialize};

/// A host-state key a plugin can subscribe to. The set is additive (appending a
/// name-tagged variant keeps the same proto version — see the crate root's
/// compat rules).
///
/// Subscribing is also how a plugin **opts in** to a host→plugin push: the host
/// serializes only subscribed state, so a plugin receives a
/// [`HostMsg`](crate::msg::HostMsg) push *only* for the keys it declares. This
/// is the rule that keeps a new push additive — an older binary that never asked
/// for it never receives (and never fails to decode) the new variant (#305).
///
/// Appending a variant here ⇒ **bump [`VOCAB`](crate::VOCAB)** (#437): a plugin
/// declares these in its manifest, so the counter keeps a faithful census of the
/// whole wire vocabulary (a subscription's push is separately #305-gated).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateKey {
    /// `hytte_services::clock::now()` → [`ClockState`](crate::state::ClockState),
    /// delivered as [`HostMsg::StateSnapshot`](crate::msg::HostMsg::StateSnapshot).
    Clock,
    /// Opt-in to the slot-visibility push
    /// ([`HostMsg::SlotVisibility`](crate::msg::HostMsg::SlotVisibility), #288):
    /// the host sends `true`/`false` as the plugin's mount surface shows/hides,
    /// seeded once at register, so a poller can park while its card is hidden.
    /// Unlike [`Clock`](StateKey::Clock) this keys a boolean push, not a service
    /// accessor. **Required** to receive the push — an unsubscribed plugin never
    /// gets the frame (#305), which is what stops a pre-#294 binary that can't
    /// decode the variant from crash-looping.
    ///
    /// **Sidebar semantics.** The bool tracks the **sidebar** opening/closing,
    /// not a bar chip's presence (#288/#422): a [`Mount::is_bar`] chip is always
    /// on-screen, so the host seeds it a constant `true` and sends no edges. Park
    /// pollers on this only for a sidebar mount; a bar-mounted poller must not
    /// (it would idle while fully visible).
    SlotVisible,
    /// Opt-in to the desktop-accent push
    /// ([`HostMsg::Accent`](crate::msg::HostMsg::Accent), #376): the host
    /// resolves libadwaita's `@accent_color` and hands it to the plugin so the
    /// `preem` raster kit can tint its widgets' **default** color to match the
    /// shell out of the box. Like [`SlotVisible`](StateKey::SlotVisible) this is
    /// the #305 gate: the host sends the (name-tagged, additive)
    /// [`HostMsg::Accent`](crate::msg::HostMsg::Accent) variant *only* to a
    /// plugin that declares this key, so a pre-#376 binary that can't decode it
    /// never receives it. The `hytte-plugin` SDK auto-declares this on every
    /// plugin it builds — it knows how to consume the accent — so accent tracking
    /// is out-of-the-box and a plugin author never writes this by hand.
    Accent,
    /// Opt-in to the audio-reactive spectrum push
    /// ([`HostMsg::AudioSpectrum`](crate::msg::HostMsg::AudioSpectrum), #405): the
    /// host taps the default sink's monitor through `PipeWire`, downsamples to a
    /// peak + [`SPECTRUM_BINS`](crate::state::SPECTRUM_BINS)-band
    /// [`AudioSpectrum`](crate::state::AudioSpectrum), and pushes it ~20 Hz
    /// (latest-wins) so a `preem` scope/VU tile or a beat-driven caw can react to
    /// what's playing. Like [`Accent`](StateKey::Accent) / [`SlotVisible`](StateKey::SlotVisible)
    /// this is the #305 opt-in gate: the host sends the (name-tagged, additive)
    /// variant *only* to a plugin that declares this key, so a pre-#405 binary
    /// that can't decode it never receives it. Unlike `Accent`, the SDK does
    /// **not** auto-declare it — the spectrum is app data a plugin's own `view`
    /// consumes, so the plugin subscribes it explicitly (like
    /// [`Clock`](StateKey::Clock)), and the capture is only run while at least one
    /// subscriber exists (an idle desktop pays nothing).
    AudioSpectrum,
    /// Opt-in to the upcoming-calendar push
    /// ([`HostMsg::CalendarUpcoming`](crate::msg::HostMsg::CalendarUpcoming), #484):
    /// the host projects the next
    /// [`MAX_UPCOMING_EVENTS`](crate::state::MAX_UPCOMING_EVENTS)
    /// [`UpcomingEvent`](crate::state::UpcomingEvent)s in the coming 24 h off its
    /// `hytte_services::calendar` handles and pushes them on change (EDS is
    /// signal-driven, not polled). Like [`AudioSpectrum`](StateKey::AudioSpectrum)
    /// this is the #305 opt-in gate, **and** — because a calendar is personal data
    /// — the push additionally requires
    /// [`Capability::Calendar`](Capability::Calendar): the host sends the
    /// (name-tagged, additive) variant only to a plugin that both subscribes this
    /// key *and* declares that capability, so a pre-#484 binary never meets it. The
    /// motivating consumers are caw's morning briefing and the infobroker's
    /// `get calendar` datasource.
    CalendarUpcoming,
    /// Opt-in to the session-locked push
    /// ([`HostMsg::SessionLocked`](crate::msg::HostMsg::SessionLocked), #484): the
    /// host mirrors logind's session `LockedHint` and pushes the boolean on change,
    /// so a plugin can fire a "first unlock" action (caw's briefing) or **blank
    /// sensitive content while locked** (the infobroker's privacy note). The #305
    /// opt-in gate, additionally gated on
    /// [`Capability::SessionState`](Capability::SessionState) — the same
    /// subscribe-**and**-capability rule as [`CalendarUpcoming`](StateKey::CalendarUpcoming).
    SessionLocked,
    /// Opt-in to the now-playing push
    /// ([`HostMsg::NowPlaying`](crate::msg::HostMsg::NowPlaying), #528): the host
    /// projects `hytte_services::mpris`'s active player onto a GTK-free
    /// [`NowPlaying`](crate::state::NowPlaying) (title / artist / playing) and
    /// pushes it on change (latest-wins), exactly the way #405 projected the
    /// spectrum. The #305 opt-in gate, additionally gated on
    /// [`Capability::NowPlaying`](Capability::NowPlaying). The motivating consumer
    /// is the audio widget's dot-matrix track marquee.
    NowPlaying,
}

/// A shell capability a plugin requests in its manifest. The host auto-grants
/// from the manifest and audit-logs every brokered effect with the plugin id;
/// [`RunCommand`](Capability::RunCommand) is a separately granted, higher-trust
/// cap. Each gates the matching [`Effect`](crate::effect::Effect).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    /// Open drawer pages ([`Effect::OpenPage`](crate::effect::Effect::OpenPage)).
    OpenPage,
    /// Niri actions ([`Effect::Niri`](crate::effect::Effect::Niri)).
    Niri,
    /// Media transport ([`Effect::Media`](crate::effect::Effect::Media)).
    Media,
    /// Audio control ([`Effect::Audio`](crate::effect::Effect::Audio)).
    Audio,
    /// Spawn commands ([`Effect::RunCommand`](crate::effect::Effect::RunCommand)).
    RunCommand,
    /// Raise a transient OSD nudge ([`Effect::RaiseOsd`](crate::effect::Effect::RaiseOsd)).
    RaiseOsd,
    /// Post a notification toast through the shell's own notification daemon
    /// ([`Effect::Notify`](crate::effect::Effect::Notify)).
    Notify,
    /// Raise an interactive consent prompt
    /// ([`Effect::RequestConsent`](crate::effect::Effect::RequestConsent), #487
    /// phase 1b). Declaring this cap is **also** the #305 opt-in gate for the
    /// paired host→plugin push: the host only sends
    /// [`HostMsg::ConsentDecision`](crate::msg::HostMsg::ConsentDecision) — the
    /// answer to a `RequestConsent` — back to a connection that requested one, and
    /// a `RequestConsent` from a plugin that never declared `Consent` is dropped
    /// by host cap-enforcement (so it never receives a decision it couldn't
    /// decode). A pre-1b plugin that never declares `Consent` therefore never
    /// meets the new variant.
    Consent,
    /// Receive the upcoming-calendar push (#484). Unlike the effect-gating caps
    /// above, this gates a **host→plugin push**: paired with a
    /// [`StateKey::CalendarUpcoming`](StateKey::CalendarUpcoming) subscription, it
    /// is what lets a plugin receive
    /// [`HostMsg::CalendarUpcoming`](crate::msg::HostMsg::CalendarUpcoming). A
    /// calendar is personal data, so the host requires the capability on top of the
    /// subscription (a subscribe-only plugin is refused the push and warned), which
    /// is also part of the #305 additive gate. The consumers are caw's morning
    /// briefing and the infobroker's `get calendar` datasource.
    Calendar,
    /// Receive the session-locked push (#484): paired with a
    /// [`StateKey::SessionLocked`](StateKey::SessionLocked) subscription, gates
    /// [`HostMsg::SessionLocked`](crate::msg::HostMsg::SessionLocked). The lock
    /// state doubles as a privacy signal, so it is capability-gated like
    /// [`Calendar`](Capability::Calendar).
    SessionState,
    /// Receive the now-playing push (#528): paired with a
    /// [`StateKey::NowPlaying`](StateKey::NowPlaying) subscription, gates
    /// [`HostMsg::NowPlaying`](crate::msg::HostMsg::NowPlaying).
    NowPlaying,
    /// Emit datasource queries ([`Effect::DatasourceQuery`](crate::effect::Effect::DatasourceQuery),
    /// #509) — the **requester/consumer** side of the generic datasource protocol.
    /// Declaring this cap is also the #305 opt-in gate for the paired
    /// [`HostMsg::DatasourceResult`](crate::msg::HostMsg::DatasourceResult) push: the
    /// host routes a result back only to a connection that emitted a query (which
    /// requires this cap, or host cap-enforcement drops the effect), so a plugin that
    /// never declared it never meets the variant. The motivating consumer is the
    /// `infobroker`, which sources departures/weather through their provider plugins.
    DatasourceQuery,
    /// Answer datasource queries ([`Effect::DatasourceResult`](crate::effect::Effect::DatasourceResult),
    /// #509) — the **provider** side. Paired with a non-empty
    /// [`provides`](Manifest::provides): the host registers a connection as routable
    /// for a datasource — and pushes it
    /// [`HostMsg::DatasourceQuery`](crate::msg::HostMsg::DatasourceQuery) — only when
    /// it BOTH declares this cap AND lists the datasource in `provides`, the same
    /// declared-**and**-enforced posture as the domain-state pushes (a plugin that
    /// lists a datasource but omits this cap is refused and warned). The motivating
    /// providers are `hytte-plugin-departures` and `hytte-plugin-weather`.
    DatasourceProvider,
}

/// Where a plugin's view mounts in the shell. Wire-side vocabulary the host
/// resolves to a real container (PR 2).
///
/// The three sidebar regions differ only in **where** they sit relative to the
/// shell's built-in cards:
/// - [`SidebarLead`](Mount::SidebarLead) — the **leading** region, above the
///   built-in weather/calendar/tasks cards. A plugin here renders *above* the
///   native cards; the other two regions (mounted after them) cannot.
/// - [`SidebarTop`](Mount::SidebarTop) — after the built-in cards but above the
///   flex gap (the pet & friends).
/// - [`SidebarBottom`](Mount::SidebarBottom) — below everything (the departures
///   board).
///
/// Appending [`SidebarLead`](Mount::SidebarLead) is additive under the crate's
/// compat rules (see the crate root): `Mount` is an externally-tagged **unit**
/// enum, so every variant rides the wire as its bare *name* (`"SidebarTop"`, …).
/// A new name-tagged variant leaves every existing variant's encoding untouched,
/// so [`PROTO_VERSION`](crate::PROTO_VERSION) stays the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mount {
    /// The leading sidebar region — the very top, above the built-in cards.
    SidebarLead,
    SidebarTop,
    SidebarBottom,
    BarLeft,
    BarCenter,
    BarRight,
}

impl Mount {
    /// Whether this is one of the three **bar** regions (a slim inline chip),
    /// as opposed to a sidebar card. A bar chip is effectively always on-screen,
    /// so the host reports a constant [`SlotVisible`](StateKey::SlotVisible) of
    /// `true` for one — that key models sidebar open/close, not bar-chip presence
    /// (#288/#422).
    #[must_use]
    pub fn is_bar(self) -> bool {
        matches!(self, Mount::BarLeft | Mount::BarCenter | Mount::BarRight)
    }
}

/// A datasource a plugin serves (#509), declared in
/// [`Manifest::provides`]. `id` is the datasource name a requester queries
/// (`provider` on [`Effect::DatasourceQuery`](crate::effect::Effect::DatasourceQuery));
/// `scopes` are the sub-views the provider answers for (the host refuses a query
/// naming a scope not listed here with
/// [`DatasourceError::ScopeDenied`](crate::effect::DatasourceError::ScopeDenied)).
/// A provider declaring this must ALSO hold
/// [`Capability::DatasourceProvider`](Capability::DatasourceProvider). The
/// request/response payloads are opaque JSON at the proto layer — their schema is
/// the provider↔requester contract, documented per-datasource (e.g. in a
/// `SKILL.md`), so the wire vocabulary stays stable as datasources multiply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvidedDatasource {
    /// The datasource id a requester names to query it (e.g. `"departures"`).
    pub id: String,
    /// The scopes this provider answers for (e.g. `["next"]`). A query naming a
    /// scope outside this list is refused by the host.
    pub scopes: Vec<String>,
}

impl ProvidedDatasource {
    /// A provided datasource with the given `id` and `scopes`.
    #[must_use]
    pub fn new(id: impl Into<String>, scopes: Vec<String>) -> Self {
        Self {
            id: id.into(),
            scopes,
        }
    }

    /// Whether this provider declared `scope` (the host's scope check).
    #[must_use]
    pub fn serves_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

/// A plugin's self-description, sent once in
/// [`PluginMsg::Register`](crate::msg::PluginMsg::Register) right after it dials
/// into the host socket. The host validates [`proto`](Manifest::proto) by exact
/// match (see [`Manifest::check_proto`]) before granting caps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Stable plugin id (also the audit-log subject and the mount slot key).
    pub id: String,
    /// The [`PROTO_VERSION`] the plugin was built against — exact-matched.
    pub proto: u16,
    /// The wire-vocabulary generation (#437) the plugin may put on the wire
    /// **unconditionally** — [`VOCAB_UNCONDITIONAL`], not [`VOCAB`]. The host
    /// refuses a `Register` whose `vocab` exceeds its own
    /// ([`Manifest::check_vocab`]), catching a plugin that can render a wire
    /// variant this host can't decode (the plugin→host counterpart to the #305
    /// opt-in that guards host→plugin pushes). Stamped automatically by
    /// [`Manifest::new`], never set by a plugin author.
    ///
    /// Since #882 this is deliberately the **unconditional** ceiling rather than
    /// the census counter: a negotiated variant (one a plugin emits only after
    /// the host advertised it — see [`vocab_max`](Manifest::vocab_max)) can never
    /// reach a host that can't decode it, so declaring it here would refuse
    /// handshakes the negotiation already makes safe. See
    /// [`VOCAB_UNCONDITIONAL`] for the rule on which of the two counters a new
    /// variant bumps.
    ///
    /// Additive under the crate's compat rules — same [`PROTO_VERSION`], and
    /// `#[serde(default)]` so an older, pre-`vocab` manifest that omits the key
    /// decodes to `0` (generation 0, the pre-counter default) and always passes.
    /// Unlike [`order`](Manifest::order)/[`provides`](Manifest::provides) it carries
    /// **no** `skip_serializing_if`: like [`proto`](Manifest::proto) it is always on
    /// the wire, so a host can always read the generation a plugin declares.
    #[serde(default)]
    pub vocab: u16,
    /// The **highest** wire-vocabulary generation this plugin can speak if the
    /// host advertises it (#882) — as opposed to [`vocab`](Manifest::vocab),
    /// which is what it will emit with no advertisement at all.
    ///
    /// This is the plugin's half of the vocabulary negotiation. Declaring it is
    /// also, structurally, the #305 opt-in for the host's half
    /// ([`HostMsg::Hello`](crate::msg::HostMsg::Hello)): a plugin can only *set*
    /// this field if it was built against a proto that carries it — which is the
    /// same proto that carries `Hello` — so a host sending `Hello` exactly when
    /// this field is present can never push a variant the receiver can't decode.
    /// The same "opt-in by vocabulary" argument as
    /// [`EventKind::ValueChanged`](crate::wire::EventKind::ValueChanged), and it
    /// needs no new [`StateKey`] or [`Capability`].
    ///
    /// `None` — the default, and what every pre-#882 manifest decodes to — means
    /// "does not negotiate": the host sends no `Hello` and the plugin sticks to
    /// its unconditional [`vocab`](Manifest::vocab). Stamped automatically by
    /// [`Manifest::new`], never set by a plugin author.
    ///
    /// Additive under the crate's compat rules — same [`PROTO_VERSION`],
    /// `#[serde(default)]` for backward decode, and `skip_serializing_if` so a
    /// non-negotiating manifest stays byte-identical on the wire to a pre-#882
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vocab_max: Option<u16>,
    /// The host-state subset the plugin wants pushed to it.
    pub subscribes: Vec<StateKey>,
    /// The shell capabilities the plugin requests.
    pub capabilities: Vec<Capability>,
    /// Where the plugin's view mounts.
    pub mount: Mount,
    /// The plugin's placement request **within** its [`mount`](Manifest::mount)
    /// region. The host holds each region as N plugin cards and sorts them by
    /// `(order, id)` ascending — a lower `order` renders earlier (higher in a
    /// sidebar); ties break on the stable `id`. Advisory only: the host owns
    /// final placement and may clamp or ignore it.
    ///
    /// `None` is the default and is the value an **older, pre-`order`** plugin's
    /// `Register` frame decodes to (it omits the field entirely); it sorts as if
    /// `0`. Additive under the crate's compat rules — same [`PROTO_VERSION`],
    /// `#[serde(default)]` for backward decode, `skip_serializing_if` so a `None`
    /// is byte-identical on the wire to an old field-less manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
    /// The datasources this plugin serves (#509) — empty for a non-provider (the
    /// default, and what a pre-#509 manifest decodes to). A provider must also
    /// declare [`Capability::DatasourceProvider`](Capability::DatasourceProvider);
    /// the host then registers each listed [`ProvidedDatasource::id`] as routable to
    /// this connection and pushes it
    /// [`HostMsg::DatasourceQuery`](crate::msg::HostMsg::DatasourceQuery) for matching
    /// queries. Additive under the crate's compat rules — same [`PROTO_VERSION`],
    /// `#[serde(default)]` for backward decode, and `skip_serializing_if` so a
    /// non-provider's manifest stays byte-identical on the wire to a pre-#509 one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<ProvidedDatasource>,
}

impl Manifest {
    /// A manifest stamped with the current [`PROTO_VERSION`] and no
    /// subscriptions/capabilities yet.
    ///
    /// The two vocabulary numbers are stamped for the plugin: `vocab` at
    /// [`VOCAB_UNCONDITIONAL`] (what it may emit with no advertisement) and
    /// [`vocab_max`](Manifest::vocab_max) at [`VOCAB`] (what it can speak if the
    /// host asks for it). That pairing is what lets a plugin built against the
    /// newest proto still clear an old host's [`check_vocab`](Manifest::check_vocab)
    /// and degrade, rather than being refused at the handshake.
    #[must_use]
    pub fn new(id: impl Into<String>, mount: Mount) -> Self {
        Self {
            id: id.into(),
            proto: PROTO_VERSION,
            vocab: VOCAB_UNCONDITIONAL,
            vocab_max: Some(VOCAB),
            subscribes: Vec::new(),
            capabilities: Vec::new(),
            mount,
            order: None,
            provides: Vec::new(),
        }
    }

    /// Request a placement [`order`](Manifest::order) within the mount region.
    /// Advisory — the host sorts co-mounted plugins by `(order, id)` but owns
    /// final placement. Chainable off [`Manifest::new`].
    #[must_use]
    pub fn with_order(mut self, order: i32) -> Self {
        self.order = Some(order);
        self
    }

    /// The exact-match proto rule the host applies on `Register`: a plugin
    /// built against a different [`PROTO_VERSION`] is rejected outright rather
    /// than best-effort decoded, so schema skew fails loud at the handshake.
    pub fn check_proto(&self) -> Result<(), ProtoError> {
        if self.proto == PROTO_VERSION {
            Ok(())
        } else {
            Err(ProtoError::ProtoMismatch {
                ours: PROTO_VERSION,
                theirs: self.proto,
            })
        }
    }

    /// The wire-vocabulary rule the host applies on `Register` (#437): a plugin
    /// built against a **newer** [`VOCAB`] than this host — one that can render a
    /// [`Node`](crate::wire::Node)/[`Effect`](crate::effect::Effect) variant the
    /// host can't decode — is rejected at the handshake, so the plugin→host skew
    /// fails loud here instead of becoming a silent redial crash-loop. A plugin at
    /// the same or an **older** vocabulary passes: it can only render variants this
    /// host already understands. An older, pre-`vocab` manifest decodes to `0` and
    /// always passes.
    pub fn check_vocab(&self) -> Result<(), ProtoError> {
        if self.vocab <= VOCAB {
            Ok(())
        } else {
            Err(ProtoError::VocabTooNew {
                ours: VOCAB,
                theirs: self.vocab,
            })
        }
    }

    /// Whether this plugin negotiates its vocabulary at all (#882) — i.e.
    /// whether it declared a [`vocab_max`](Manifest::vocab_max).
    ///
    /// This is the host's gate for sending
    /// [`HostMsg::Hello`](crate::msg::HostMsg::Hello): send it exactly when this
    /// is `true`, and a plugin too old to decode `Hello` never receives one.
    #[must_use]
    pub fn negotiates_vocab(&self) -> bool {
        self.vocab_max.is_some()
    }

    /// The vocabulary generation both ends actually agreed on, given the
    /// `host_vocab` the host advertised (its own [`VOCAB`]).
    ///
    /// The minimum of what the plugin can speak
    /// ([`vocab_max`](Manifest::vocab_max), falling back to its unconditional
    /// [`vocab`](Manifest::vocab) when it does not negotiate) and what the host
    /// offered. Both ends compute the same number from the same two inputs, so
    /// neither has to trust the other's arithmetic.
    ///
    /// Compare it against a feature's own generation marker — e.g.
    /// [`PREEM_VOCAB`](crate::preem::PREEM_VOCAB) — to decide whether to use
    /// that feature or fall back.
    #[must_use]
    pub fn negotiated_vocab(&self, host_vocab: u16) -> u16 {
        self.vocab_max.unwrap_or(self.vocab).min(host_vocab)
    }
}
