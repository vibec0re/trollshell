//! Shell effects and the pages/actions they drive.
//!
//! An [`Effect`] is something a plugin *requests* on its render frame; the host
//! matches each to a real `do_thing` command and gates it on the plugin's
//! granted [`Capability`](crate::manifest::Capability) set. The plugin never
//! touches D-Bus / niri directly. These are wire-side mirrors of the host's
//! command surfaces; the host maps them (PR 2) — this crate stays GTK-free and
//! host-free.

use serde::{Deserialize, Serialize};

/// A drawer page the host can open. Wire-side mirror of the host's
/// `modal::Page`; the host maps `wire::Page -> modal::Page` (PR 2).
///
/// Every variant names a built-in host page **except** [`Page::PluginSelf`],
/// which has no `modal::Page` counterpart: it resolves to the *requesting*
/// plugin's own drawer panel, keyed by the effect's plugin id (the host broker
/// already carries it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    Media,
    Network,
    Vpn,
    Connections,
    Bluetooth,
    Stats,
    Audio,
    Power,
    PowerMenu,
    Notifications,
    Appearance,
    Displays,
    Clipboard,
    Calendar,
    Settings,
    /// The plugin's *own* drawer panel (#349 PR2). Unlike the other variants,
    /// this does not name a built-in host page: the host resolves it to the
    /// requesting plugin's panel tree, keyed by the effect's plugin id (the
    /// broker already carries it). A plugin emits
    /// `Effect::OpenPage(Page::PluginSelf)` from `update` to open its panel; it
    /// needs the [`OpenPage`](crate::manifest::Capability::OpenPage) capability
    /// like any other page-open.
    PluginSelf,
}

/// A niri compositor action (maps to `hytte_services::niri::*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NiriAction {
    FocusWorkspace { id: u64 },
    FocusWindow { id: u64 },
}

/// A media-player transport action (maps to `hytte_services::mpris::*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaAction {
    PlayPause,
    Next,
    Previous,
}

/// An audio-sink action (maps to the `pipewire` default-sink setters).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum AudioAction {
    /// Set the default sink volume, `0.0..=1.0`.
    SetVolume(f64),
    ToggleMute,
}

/// A shell effect a plugin returns on its [`Render`](crate::msg::PluginMsg::Render)
/// frame (bundled with the tree so a frame is atomic). Each maps to a real host
/// command, gated on the matching capability.
///
/// All variants are fire-and-forget **except** [`Effect::RunCommand`] and
/// [`Effect::OpenUri`], whose outcomes come back as a
/// [`HostMsg::EffectResult`](crate::msg::HostMsg::EffectResult) keyed by their
/// `id`.
///
/// Appending a variant here ⇒ **bump [`VOCAB`](crate::VOCAB)** (#437): a plugin
/// emits these on its render frame, so an older host must be able to detect and
/// refuse a plugin built against the newer vocabulary at the handshake.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    /// Open a drawer page (cap: [`OpenPage`](crate::manifest::Capability::OpenPage)).
    OpenPage(Page),
    /// A niri action (cap: [`Niri`](crate::manifest::Capability::Niri)).
    Niri(NiriAction),
    /// A media transport action (cap: [`Media`](crate::manifest::Capability::Media)).
    Media(MediaAction),
    /// An audio action (cap: [`Audio`](crate::manifest::Capability::Audio)).
    Audio(AudioAction),
    /// Spawn a command (cap: [`RunCommand`](crate::manifest::Capability::RunCommand),
    /// the highest-trust capability in the vocabulary). `id` correlates the
    /// resulting [`HostMsg::EffectResult`](crate::msg::HostMsg::EffectResult).
    ///
    /// # Two spawn modes (#953)
    ///
    /// `detached` picks between them; both are gated on the *same*
    /// [`RunCommand`](crate::manifest::Capability::RunCommand) capability,
    /// because a plugin that may run an arbitrary `argv` at all can already
    /// launch a detacher of its own — a second capability would be paperwork,
    /// not a boundary.
    ///
    /// - **`detached: false` (the default) — run and report.** The host runs
    ///   the command *to completion* under a bound (10 s today), captures
    ///   stdout, and returns the program's exit status in
    ///   [`EffectOutcome`]. The child lives and dies with the shell: it is in
    ///   the shell's own cgroup, and the host kills it if the bound expires or
    ///   the connection drops. Right for a short query whose answer the plugin
    ///   wants back.
    /// - **`detached: true` — launch and forget.** The host hands the program
    ///   to the systemd **user manager** as a transient unit
    ///   (`systemd-run --user --unit=trollshell-launch-<plugin>-<id>`), so it is
    ///   *not* in the shell's cgroup, is never awaited, and has no timeout: it
    ///   outlives a `trollshell.service` restart. [`EffectOutcome::ok`] then
    ///   reports **whether the launch succeeded**, never the program's exit
    ///   status (which the host never learns), and [`EffectOutcome::output`]
    ///   names the unit (or, with no user manager, the pid). Right for a
    ///   terminal, an editor, a companion window — anything meant to keep
    ///   running while the user works.
    ///
    /// Use [`Effect::run_command`] / [`Effect::launch`] rather than a struct
    /// literal, so a later field addition stays source-compatible.
    ///
    /// Additive on the wire: `detached` is a `#[serde(default)]` field, so a
    /// frame built before #953 decodes to `false` — the pre-#953 behaviour. It
    /// also carries `skip_serializing_if`, so a non-detached `RunCommand`
    /// serializes to the *exact same bytes* it did before the field existed
    /// (which is what keeps `tests/fixtures/plugin_render_v1.hex` unchanged).
    /// That diverges from [`Node::Text`](crate::wire::Node::Text)'s `ellipsize`,
    /// which pays a couple of bytes rather than name a predicate; here the
    /// byte-identity is itself the compat evidence, and `std::ops::Not::not`
    /// supplies the `&bool` predicate serde needs without a helper `fn`.
    /// An **older host** that predates the field skips the unknown key and runs
    /// the command in the attached mode — the launched program then dies with
    /// the shell (the status quo), which is a degradation, never a decode
    /// failure, so no [`VOCAB`](crate::VOCAB) bump is involved.
    RunCommand {
        id: u64,
        argv: Vec<String>,
        /// Launch independently of the shell instead of running to completion.
        /// See the variant docs for the two modes.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        detached: bool,
    },
    /// Raise a transient on-screen-display nudge (cap:
    /// [`RaiseOsd`](crate::manifest::Capability::RaiseOsd)). A **generic,
    /// reusable** surface: the *plugin* computes the display strings and the host
    /// just shows them, so any plugin can pop a "get up and go" style alert
    /// without the host learning its domain. `title` is the bold kind line,
    /// `body` the value readout, and `icon` an optional named symbolic icon (the
    /// host picks a sensible default when `None`). Fire-and-forget.
    RaiseOsd {
        title: String,
        body: String,
        icon: Option<String>,
    },
    /// Post a **notification toast** through the shell's own notification daemon
    /// (cap: [`Notify`](crate::manifest::Capability::Notify)). trollshell *is* the
    /// `org.freedesktop.Notifications` daemon, so a plugin that needs to make
    /// noise at a moment nobody is watching — a timer hitting zero, a threshold
    /// crossed (#320), an approval request (#344) — asks the host to inject a
    /// local toast rather than owning a D-Bus connection of its own. A **generic,
    /// reusable** surface like [`RaiseOsd`](Effect::RaiseOsd): the *plugin*
    /// computes the strings and the host renders them through the same path as an
    /// externally-posted `Notify` (rate-limited, DND-gated), so any plugin can
    /// alert without the host learning its domain. `summary` is the bold headline,
    /// `body` the detail line. Fire-and-forget.
    Notify { summary: String, body: String },
    /// Ask the shell to raise an **interactive consent prompt** on the focused
    /// output (cap: [`Consent`](crate::manifest::Capability::Consent), #487 phase
    /// 1b). The motivating consumer is the `infobroker` data broker: when a local
    /// AI agent asks for data it has no standing grant for, the broker emits this
    /// to get a human yes/no rather than silently denying.
    ///
    /// The host shows *"⟨agent⟩ wants: ⟨scope⟩ from ⟨datasource⟩"* (with `detail`
    /// as a secondary line) and four choices — Allow once / this session / always /
    /// Deny — then routes the answer back to *this* plugin as
    /// [`HostMsg::ConsentDecision`](crate::msg::HostMsg::ConsentDecision) keyed by
    /// the same `request_id`. **Not** fire-and-forget: it is a request/response
    /// pair, mirroring [`RunCommand`](Effect::RunCommand)→
    /// [`EffectResult`](crate::msg::HostMsg::EffectResult). An unanswered prompt
    /// times out to [`ConsentDecision::Deny`] after 60 s, so a wedged UI can never
    /// leave the requester hanging. `request_id` is the plugin's own correlation
    /// token (a fresh one per prompt); the other fields are the human-facing
    /// strings the plugin computes (the host learns no domain).
    RequestConsent {
        request_id: u64,
        agent: String,
        datasource: String,
        scope: String,
        detail: String,
    },
    /// Query a datasource served by **another** plugin (#509), gated on
    /// [`DatasourceQuery`](crate::manifest::Capability::DatasourceQuery). The
    /// requester side of the generic datasource protocol; the calendar case is
    /// already served by the host directly
    /// ([`StateKey::CalendarUpcoming`](crate::manifest::StateKey::CalendarUpcoming)),
    /// so this exists for **third-party** datasources.
    ///
    /// **Host-routed** — the requester never dials the provider. The host is the
    /// single policy chokepoint: it validates a provider for `provider` is connected
    /// and declared `scope`, forwards the query to it as
    /// [`HostMsg::DatasourceQuery`](crate::msg::HostMsg::DatasourceQuery), then
    /// routes the answer back to *this* plugin as
    /// [`HostMsg::DatasourceResult`](crate::msg::HostMsg::DatasourceResult) keyed by
    /// the same `request_id`. **Not** fire-and-forget: it is a request/response pair,
    /// mirroring [`RunCommand`](Effect::RunCommand)→
    /// [`EffectResult`](crate::msg::HostMsg::EffectResult). A missing provider, a
    /// denied scope, or an unanswered query (10 s host timeout) resolves to a
    /// synthesized [`DatasourceOutcome::Failed`], so a requester never hangs.
    ///
    /// `request_id` is the requester's own correlation token (a fresh one per
    /// query); `provider` names the datasource id (matched against providers'
    /// [`Manifest::provides`](crate::manifest::Manifest::provides)); `scope` selects
    /// a sub-view the provider declared; and `params` is an **opaque JSON** request
    /// string — the provider↔requester contract, documented per-datasource, that the
    /// host never interprets (keeping the vocab stable as datasources multiply).
    DatasourceQuery {
        request_id: u64,
        provider: String,
        scope: String,
        params: String,
    },
    /// A provider's answer to a host-forwarded datasource query (#509), gated on
    /// [`DatasourceProvider`](crate::manifest::Capability::DatasourceProvider). Sent
    /// by a provider plugin in reply to the
    /// [`HostMsg::DatasourceQuery`](crate::msg::HostMsg::DatasourceQuery) the host
    /// pushed it. The provider echoes the host's `request_id` verbatim — an **opaque
    /// host correlation**, *not* the requester's token (the host translates it on
    /// both legs so provider and requester id-spaces can never collide) — and returns
    /// the [`DatasourceOutcome`]. The host routes it on to the original requester.
    DatasourceResult {
        request_id: u64,
        outcome: DatasourceOutcome,
    },
    /// Open `uri` with the **desktop's default handler** (cap:
    /// [`OpenUri`](crate::manifest::Capability::OpenUri), #1045). `id`
    /// correlates the resulting
    /// [`HostMsg::EffectResult`](crate::msg::HostMsg::EffectResult), like
    /// [`RunCommand`](Effect::RunCommand) — so a plugin can toast a refusal
    /// rather than watch a click do nothing.
    ///
    /// The host resolves it with `gio::AppInfo::launch_default_for_uri`, the
    /// same call the shell's own screenshot / recording toasts use for their
    /// **Open** action. Not a subprocess, not a shell: the plugin names a
    /// destination and the *desktop* decides which program opens it.
    ///
    /// # Why this is not `RunCommand`
    ///
    /// Opening a link was already expressible — `Effect::launch(id, ["xdg-open",
    /// url])` — but only for a plugin holding
    /// [`RunCommand`](crate::manifest::Capability::RunCommand), which is
    /// arbitrary argv execution as the user (the highest-trust capability in the
    /// vocabulary). That is the wrong trust shape for a card whose other needs
    /// are its own panel and a toast: #963's agents card renders an
    /// `agent page https://…` row nobody can follow, and making the row a button
    /// should not cost the plugin a general exec grant. `OpenUri` is a **narrow
    /// intent**: one string, host-validated, resolved by the desktop. In trust
    /// order it sits above [`Notify`](crate::manifest::Capability::Notify) (it
    /// starts a program of the user's choosing) and well below
    /// [`RunCommand`](crate::manifest::Capability::RunCommand) (it cannot name
    /// one).
    ///
    /// # The host validates the scheme
    ///
    /// Only `http`, `https` and `file` are brokered. Everything else — a
    /// `mailto:`, a `javascript:`, an `ssh://`, an empty or scheme-less string —
    /// is refused with a warn and an [`EffectOutcome`] of `ok: false` whose
    /// `output` names the refused scheme; nothing is launched. The allow-list is
    /// host policy, not wire vocabulary, so widening it later (Annika flagged
    /// `mailto:` as an open question on #1045) is a host change alone and needs
    /// no new variant here.
    ///
    /// Note what the allow-list does and does not buy. It is **not** a sandbox:
    /// `file:///…` reaches the user's own default handler for that file type,
    /// and a same-user process on the plugin socket could always do more than
    /// this (see [`super::manifest::Capability`]'s route-0 note). It is a
    /// legibility guard — the effect does what its name says and cannot be
    /// smuggled into launching a handler for an unrelated protocol.
    ///
    /// # Use [`Effect::open_uri`], and know the older-host behaviour
    ///
    /// An appended **variant**, not a field: a host that predates #1045 cannot
    /// decode it at all — `rmp-serde` fails the whole frame
    /// ([`ProtoError::Decode`](crate::codec::ProtoError::Decode)) rather than
    /// skipping it the way it skips an unknown *field*, which is why appending a
    /// variant bumps [`VOCAB`](crate::VOCAB) where adding a field does not.
    ///
    /// That never happens in practice, and the reason is the capability, not the
    /// counter: an effect only rides the wire from a plugin whose manifest
    /// declared its gating capability (the host drops the rest — see
    /// [`Capability`](crate::manifest::Capability)), and `Capability::OpenUri`
    /// is *itself* a variant a pre-#1045 host cannot decode, so such a plugin is
    /// already dropped at `Register` with a handshake-read warn. See
    /// [`OPEN_URI_VOCAB`] for why that makes generation 5 a census-only bump.
    OpenUri {
        /// The plugin's correlation token, echoed on the
        /// [`EffectResult`](crate::msg::HostMsg::EffectResult).
        id: u64,
        /// The destination. `http`/`https`/`file` only; anything else is
        /// refused by the host.
        uri: String,
    },
}

impl Effect {
    /// Run `argv` to completion and route its exit status + stdout back as an
    /// [`EffectOutcome`] — the attached mode ([`Effect::RunCommand`] with
    /// `detached: false`). The host bounds it with a timeout and the child dies
    /// with the shell.
    ///
    /// Prefer this over a struct literal: a later optional field then stays
    /// source-compatible for out-of-tree plugins.
    #[must_use]
    pub fn run_command(id: u64, argv: Vec<String>) -> Self {
        Effect::RunCommand {
            id,
            argv,
            detached: false,
        }
    }

    /// Launch `argv` **independently of the shell** — the detached mode
    /// ([`Effect::RunCommand`] with `detached: true`, #953). The host hands it
    /// to the systemd user manager as a transient unit, never awaits it, and
    /// reports only whether the *launch* succeeded; the program outlives a shell
    /// restart. For a terminal, an editor, a companion window.
    ///
    /// # On a shell older than #953, this silently degrades
    ///
    /// `detached` is an additive field, not a new variant, so a pre-#953 host
    /// does not reject the frame — it skips the key it doesn't know and runs the
    /// command in the **attached** mode. The program is then a child of the
    /// shell in the shell's cgroup, is **killed after 10 s** by the attached
    /// mode's timeout, and the plugin gets `ok: false` with no output — which is
    /// indistinguishable from a program that simply failed.
    ///
    /// That is the deliberate price of not bumping
    /// [`VOCAB`](crate::VOCAB): a bump would make an older shell refuse *every*
    /// plugin rebuilt on this SDK at the handshake, including plugins that never
    /// launch anything, which is strictly worse than one effect degrading. A
    /// [`HostMsg::Hello`](crate::msg::HostMsg::Hello) negotiation cannot rescue
    /// it either — an old host advertises nothing, and the vocabulary counter
    /// correctly did not move. If a plugin must tell the two apart, the
    /// distinguishing signal is [`EffectOutcome::output`]: a #953 host always
    /// returns a non-empty `output` naming the unit or pid on a successful
    /// launch.
    #[must_use]
    pub fn launch(id: u64, argv: Vec<String>) -> Self {
        Effect::RunCommand {
            id,
            argv,
            detached: true,
        }
    }

    /// Open `uri` with the desktop's default handler ([`Effect::OpenUri`],
    /// #1045), routing the outcome back as an [`EffectOutcome`] keyed by `id`.
    /// `http`/`https`/`file` only — the host refuses any other scheme with
    /// `ok: false` rather than launching anything.
    ///
    /// Prefer this over a struct literal, for the same reason
    /// [`Effect::run_command`] exists: a later optional field then stays
    /// source-compatible for out-of-tree plugins.
    #[must_use]
    pub fn open_uri(id: u64, uri: impl Into<String>) -> Self {
        Effect::OpenUri {
            id,
            uri: uri.into(),
        }
    }
}

/// The [`VOCAB`](crate::VOCAB) generation that carries the open-a-link intent
/// ([`Effect::OpenUri`] + [`Capability::OpenUri`](crate::manifest::Capability::OpenUri))
/// — #1045.
///
/// **Census-only**, like [`SHADER_VOCAB`](crate::wire::SHADER_VOCAB),
/// [`SCROLLED_VOCAB`](crate::wire::SCROLLED_VOCAB) and
/// [`PREEM_VOCAB`](crate::preem::PREEM_VOCAB): generation 5 bumps
/// [`VOCAB`](crate::VOCAB) and leaves
/// [`VOCAB_UNCONDITIONAL`](crate::VOCAB_UNCONDITIONAL) alone, so a plugin
/// rebuilt on this SDK still stamps generation 1 and still clears an older
/// host's [`check_vocab`](crate::manifest::Manifest::check_vocab).
///
/// # Why it is safe to leave the unconditional ceiling alone
///
/// The three generations before this one are gated on the host having
/// advertised them in [`HostMsg::Hello`](crate::msg::HostMsg::Hello). This one
/// is gated by something stronger and earlier: **its capability**. An
/// [`Effect`] only reaches a host from a plugin that declared the gating
/// [`Capability`](crate::manifest::Capability) (the host drops every other
/// effect before brokering it), and `Capability::OpenUri` is itself a variant a
/// pre-#1045 host cannot decode — so that plugin's `Register` frame fails to
/// decode and the connection is dropped at the handshake, before any render
/// frame carrying an `OpenUri` could be sent. The #437 hazard the unconditional
/// counter exists to catch — an old host silently failing to decode a *render*
/// frame, redialing, and crash-looping — therefore cannot arise here.
///
/// The price is the one every appended capability has paid since the first
/// (stated on [`Capability::Shader`](crate::manifest::Capability::Shader)):
/// declaring `OpenUri` costs compatibility with a pre-#1045 host, which drops
/// the connection with a `plugin handshake read failed` warn naming the
/// undecodable variant. Bumping `VOCAB_UNCONDITIONAL` would not improve that
/// plugin's fate one bit — it would only add a refusal for every *other* plugin
/// rebuilt on this SDK, including the ones that never open a link.
///
/// A plugin that wants to branch rather than rely on that can compare this
/// against [`negotiated_vocab`](crate::manifest::Manifest::negotiated_vocab).
pub const OPEN_URI_VOCAB: u16 = 5;

/// The outcome of a datasource query (#509). Travels twice: from a provider back
/// to the host in [`Effect::DatasourceResult`], and from the host on to the
/// requester in [`HostMsg::DatasourceResult`](crate::msg::HostMsg::DatasourceResult).
/// A provider returns [`Ready`](DatasourceOutcome::Ready) with an opaque JSON
/// payload (the provider↔requester contract, never interpreted by the host) or
/// [`Failed`](DatasourceOutcome::Failed) for a failure of its own; the host itself
/// synthesizes a [`Failed`](DatasourceOutcome::Failed) for a routing failure — no
/// connected provider, a scope the provider never declared, or an unanswered query.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DatasourceOutcome {
    /// The query succeeded; `String` is the provider's opaque JSON payload.
    Ready(String),
    /// The query failed. `error` classifies it (host- or provider-sourced);
    /// `message` is a human-readable detail line.
    Failed {
        error: DatasourceError,
        message: String,
    },
}

/// Why a datasource query failed (#509), carried in
/// [`DatasourceOutcome::Failed`]. The first three are **host-synthesized** routing
/// failures the requester sees without the provider ever running; [`Provider`](DatasourceError::Provider)
/// is a failure the provider itself reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DatasourceError {
    /// No connected plugin provides the named datasource (host-synthesized).
    NotFound,
    /// A provider is connected but did not declare the requested scope
    /// (host-synthesized).
    ScopeDenied,
    /// The provider did not answer within the host's query timeout
    /// (host-synthesized).
    Timeout,
    /// The provider answered with a failure of its own (provider-sourced).
    Provider,
}

/// The human's answer to an [`Effect::RequestConsent`] knock (#487 phase 1b),
/// delivered back to the requesting plugin inside
/// [`HostMsg::ConsentDecision`](crate::msg::HostMsg::ConsentDecision) and
/// surfaced to the SDK as `Input::ConsentDecision`. The four choices the shell's
/// consent overlay offers; an unanswered prompt (60 s) resolves to
/// [`Deny`](ConsentDecision::Deny).
///
/// The meanings are the requester's to honor — the host only relays the choice —
/// but the intended semantics (as implemented by `infobroker`) are:
/// - [`AllowOnce`](ConsentDecision::AllowOnce) — allow exactly this one request.
/// - [`AllowSession`](ConsentDecision::AllowSession) — allow for the rest of this
///   session (until the requester restarts).
/// - [`AllowAlways`](ConsentDecision::AllowAlways) — allow, and persist a standing
///   grant so future asks are silent.
/// - [`Deny`](ConsentDecision::Deny) — refuse (and, for a deliberate click,
///   persist a standing "no").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsentDecision {
    /// Allow this one request only.
    AllowOnce,
    /// Allow for the rest of this session.
    AllowSession,
    /// Allow always (persist a standing grant).
    AllowAlways,
    /// Deny.
    Deny,
}

/// The outcome of a brokered [`Effect::RunCommand`], returned to the plugin.
///
/// Both fields mean something different in the two spawn modes (#953) — see the
/// [`RunCommand`](Effect::RunCommand) docs:
///
/// | | attached (`detached: false`) | detached (`detached: true`) |
/// |---|---|---|
/// | [`ok`](EffectOutcome::ok) | the program exited `0` | the **launch** succeeded (the exit status is never learned) |
/// | [`output`](EffectOutcome::output) | captured stdout | the transient unit name, or the pid on the no-user-manager fallback |
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectOutcome {
    /// Whether the command exited successfully — or, for a detached launch,
    /// whether the *launch* succeeded.
    pub ok: bool,
    /// Captured stdout (host may truncate), if any — or, for a detached launch,
    /// what was started (unit name / pid) or why it couldn't be.
    pub output: Option<String>,
}
