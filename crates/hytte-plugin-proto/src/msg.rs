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
    Render { tree: Node, effects: Vec<Effect> },
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
