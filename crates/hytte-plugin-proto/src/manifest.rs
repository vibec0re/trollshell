//! The plugin manifest, the subscription keys, and the capability model.

use crate::PROTO_VERSION;
use crate::codec::ProtoError;
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
    SlotVisible,
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
}

impl Manifest {
    /// A manifest stamped with the current [`PROTO_VERSION`] and no
    /// subscriptions/capabilities yet.
    #[must_use]
    pub fn new(id: impl Into<String>, mount: Mount) -> Self {
        Self {
            id: id.into(),
            proto: PROTO_VERSION,
            subscribes: Vec::new(),
            capabilities: Vec::new(),
            mount,
            order: None,
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
}
