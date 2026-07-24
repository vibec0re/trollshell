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
/// All variants are fire-and-forget **except** [`Effect::RunCommand`], whose
/// outcome comes back as a [`HostMsg::EffectResult`](crate::msg::HostMsg::EffectResult)
/// keyed by its `id`.
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
    /// a separately granted, higher-trust capability). `id` correlates the
    /// resulting [`HostMsg::EffectResult`](crate::msg::HostMsg::EffectResult).
    RunCommand { id: u64, argv: Vec<String> },
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectOutcome {
    /// Whether the command exited successfully.
    pub ok: bool,
    /// Captured stdout (host may truncate), if any.
    pub output: Option<String>,
}
