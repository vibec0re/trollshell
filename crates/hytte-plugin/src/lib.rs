//! `hytte-plugin` — the Rust runtime SDK for hytte's out-of-process widget
//! plugins ("frontend B"; issues #35 / #195 / #275, on the #266 wire protocol
//! and the #272 host transport).
//!
//! [`hytte_plugin_proto`] is the language-neutral *schema* (any language can
//! speak the wire format); this crate is the opinionated *Rust runtime* over
//! it, so a plugin author writes only The Elm Architecture core — a
//! [`Plugin`]: `manifest` / `init` / `update` / `view` — and [`run`] owns
//! everything else: dialing the host socket with bounded backoff, the
//! `Register` handshake, the read→update→render session loop, liveness, and
//! reconnection. A plugin binary depends on **this crate alone** (the proto
//! vocabulary is re-exported; no GTK, no `hytte` umbrella, not even a direct
//! tokio dependency) and its `main` is one line.
//!
//! `hytte-plugin-clock-demo` is the reference plugin built on this runtime.
//!
//! # What the runtime absorbs (and the author never sees)
//!
//! - **Liveness**: a host [`Ping`](proto::HostMsg::Ping) is answered with
//!   [`Pong`](proto::PluginMsg::Pong) internally — it is not an [`Input`].
//! - **The greeting**: right after `Register`, the runtime sends one
//!   `Log { Info, "<id> connected" }` frame. That is the wire `Log` path's
//!   only SDK surface in v1 — plugin diagnostics go to stderr, which systemd
//!   routes to the journal.
//! - **Shutdown ≡ disconnect**: on [`Shutdown`](proto::HostMsg::Shutdown) (or
//!   socket EOF) the session ends and the runtime redials with backoff. It
//!   deliberately does **not** exit: plugin units run `Restart=on-failure`, so
//!   a clean exit would leave the plugin dead across a host restart, while
//!   redialing rides it out (the dial just fails until the host is back).
//! - **Render dedup**: after every [`update`](Plugin::update) the runtime
//!   computes [`view`](Plugin::view) and sends a
//!   [`Render`](proto::PluginMsg::Render) frame iff the tree changed since the
//!   last sent one **or** the update returned effects (effects ride the render
//!   frame, so a non-empty batch forces a send even for an identical tree).
//!   The author never decides "should I re-render".
//!
//! # State is per-session
//!
//! [`init`](Plugin::init) runs on every (re)connect: a disconnect drops the
//! model and the next session re-derives it from the host's initial
//! [`StateSnapshot`](proto::StateSnapshot). That is the design's crash stance
//! (#195: the host persists nothing, the plugin's transient UI state is
//! re-derivable) applied symmetrically to the plugin side.
//!
//! # Self-driven re-renders
//!
//! A plugin that re-renders on its own schedule (a timer, an external fetch)
//! returns a message stream from [`sources`](Plugin::sources); each item
//! arrives in [`update`](Plugin::update) as [`Input::App`]. Sources are
//! created per session and dropped on disconnect — spawn nothing global.

use hytte_plugin_proto::{Effect, EffectOutcome, EventKind, Manifest, Node, NodeId, StateSnapshot};

mod runtime;

pub use runtime::run;

/// The full wire vocabulary, re-exported so a plugin depends on this crate
/// alone. (`Manifest`, `Node`, `Effect`, … are what a plugin actually names;
/// the codec/framing helpers matter only if you bypass [`run`].)
pub use hytte_plugin_proto as proto;

/// Stream constructors/combinators for building [`Plugin::sources`] values
/// (`iter`, `wrappers::*`, `StreamExt`, …) — re-exported wholesale so a
/// source-driven plugin still needs no dependency beyond this crate.
pub use tokio_stream;

/// A boxed message stream returned by [`Plugin::sources`]. Any well-behaved
/// [`Stream`](tokio_stream::Stream) qualifies (build one from the
/// re-exported [`tokio_stream`]: its wrappers, `iter`, channel receivers, …);
/// the runtime polls it inside a `select!`, so it must tolerate being polled
/// incrementally, as all standard combinators do.
pub type MsgStream<M> = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = M>>>;

/// One app-level input folded into the plugin's model by
/// [`update`](Plugin::update). This is the [`HostMsg`](proto::HostMsg) surface
/// *minus* the protocol plumbing (`Ping`/`Shutdown`, which the runtime
/// absorbs), *plus* the plugin's own [`sources`](Plugin::sources) messages.
#[derive(Debug)]
pub enum Input<M> {
    /// The full subscribed-state subset, re-sent by the host on any change
    /// (latest-wins, no deltas).
    Snapshot(StateSnapshot),
    /// A user interaction on one of the plugin's rendered nodes.
    Event {
        /// The interacted node, by the id the plugin assigned in its view.
        node: NodeId,
        /// What happened (click / scroll).
        kind: EventKind,
    },
    /// The outcome of a brokered
    /// [`Effect::RunCommand`](proto::Effect::RunCommand), keyed by the
    /// command's `id`.
    EffectResult {
        /// The `id` the plugin chose on the originating `RunCommand`.
        id: u64,
        /// Whether it succeeded, and any captured output.
        outcome: EffectOutcome,
    },
    /// A message from the plugin's own [`sources`](Plugin::sources) stream.
    App(M),
}

/// The Elm Architecture core of a plugin: pure state + `update` + `view`.
/// Implement this and hand the type to [`run`] — the trait has no transport
/// surface at all, which is what keeps every method unit-testable without a
/// socket or a host.
pub trait Plugin: Sized {
    /// Messages produced by this plugin's own [`sources`](Plugin::sources)
    /// (timer ticks, fetch results). Use [`std::convert::Infallible`] for a
    /// purely host-driven plugin.
    type Msg;

    /// The plugin's self-description: id, subscriptions, capabilities, mount.
    /// Sent as the `Register` handshake frame on every (re)connect.
    fn manifest() -> Manifest;

    /// The initial model, built fresh on every session (see the crate docs on
    /// per-session state). Its [`view`](Plugin::view) is the seed render, sent
    /// immediately so the slot mounts before the first snapshot lands.
    fn init() -> Self;

    /// The plugin's own message stream (timers, fetches), or `None` for a
    /// purely host-driven plugin. Called once per session; the stream is
    /// dropped on disconnect.
    #[must_use]
    fn sources() -> Option<MsgStream<Self::Msg>> {
        None
    }

    /// Fold one [`Input`] into the model and return the shell effects to
    /// bundle on the next render frame (usually none; a one-shot action like
    /// "open a page on click" is exactly one). The runtime re-renders after
    /// every call and dedups identical trees — return effects, not render
    /// decisions.
    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect>;

    /// Project the model into the declarative widget tree the host reconciles
    /// into GTK.
    fn view(&self) -> Node;
}
