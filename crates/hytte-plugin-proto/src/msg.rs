//! The two message envelopes: plugin → host ([`PluginMsg`]) and host → plugin
//! ([`HostMsg`]).
//!
//! The channel is full-duplex: the plugin re-renders and pushes a
//! [`PluginMsg::Render`] on its **own** schedule (a host-state change, an
//! internal timer, an external fetch completing), not only in reply to a host
//! message. See the crate root for the framing/encoding.

use crate::effect::{Effect, EffectOutcome};
use crate::manifest::Manifest;
use crate::state::StateSnapshot;
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
    /// the plugin author. v1 sends it once at session start (latest-wins if
    /// re-sent); live re-tint on an accent change is a follow-up.
    Accent { color: Option<[u8; 4]> },
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
}
