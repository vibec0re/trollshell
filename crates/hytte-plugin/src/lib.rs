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
//!
//! # Commands: the outbound I/O lane
//!
//! [`update`](Plugin::update) returns shell [`Effect`]s — actions the *host*
//! brokers (open a page, drive niri/media, run a command). A plugin's **own**
//! external I/O — send a frame on the WebSocket it holds, fire an HTTP call —
//! is not a shell effect: the design does that in-process and never round-trips
//! it through the host. But `update` is sync, so it cannot do the I/O itself;
//! the sanctioned lane is a typed **command** channel (issue #280).
//!
//! The runtime creates one fresh channel per session and threads its two ends
//! for you: [`init`](Plugin::init) receives the
//! [`CmdSender<Self::Cmd>`](CmdSender) (store it in the model, then
//! [`send`](CmdSender::send) from `update` to dispatch a command), and
//! [`sources`](Plugin::sources) receives the matching
//! [`CmdReceiver<Self::Cmd>`](CmdReceiver) (drain it in the I/O task the
//! sources own — the same task that also feeds [`Input::App`] messages back
//! in). This mirrors [`Input`]'s inbound direction with an explicit outbound
//! one, so a plugin that *controls* something (not just displays it) needs no
//! ad-hoc channel smuggling. A purely host-driven plugin sets `type Cmd =`
//! [`Infallible`](std::convert::Infallible) and never touches either end.
//!
//! **Lifecycle.** The channel's life is exactly one session: created on
//! (re)connect, destroyed on disconnect together with the model (its sender)
//! and the sources task (its receiver). Commands therefore never cross a
//! reconnect — a command still queued when the socket drops dies with the
//! session, which is correct, since the very I/O task that would service it is
//! being torn down and re-established anyway. The next session starts from a
//! clean channel, just as it re-derives the model from the next snapshot.

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

/// The sending half of a plugin's per-session **command lane** — the
/// sanctioned outbound path from [`update`](Plugin::update) to the plugin's
/// own external I/O (issue #280; see the crate-level *Commands* section).
///
/// The runtime hands this to [`init`](Plugin::init); store it in the model and
/// call [`send`](CmdSender::send) from `update` to queue one command for the
/// I/O task your [`sources`](Plugin::sources) built around the matching
/// [`CmdReceiver`]. It is **unbounded**, so `update` (which is sync) never
/// blocks; [`send`](CmdSender::send) returns `Err` only once the receiver is
/// gone — i.e. the session is already tearing down — which callers can safely
/// ignore. `Clone` it if more than one place needs to enqueue commands.
///
/// (An alias for tokio's [`UnboundedSender`](tokio::sync::mpsc::UnboundedSender)
/// so a plugin needs no direct tokio dependency to name it.)
pub type CmdSender<C> = tokio::sync::mpsc::UnboundedSender<C>;

/// The receiving half of a plugin's per-session **command lane**, handed to
/// [`sources`](Plugin::sources). Drain it in the plugin's own I/O task — e.g.
/// `while let Some(cmd) = rx.recv().await { socket.send(cmd).await }` — which
/// is also where the inbound [`Input::App`] messages are produced, so a single
/// task owns both directions of the plugin's external connection.
///
/// Dropped on disconnect, so any command still queued when the session ends is
/// discarded rather than replayed against the next connection (see the
/// crate-level *Commands* section on lifecycle).
///
/// (An alias for tokio's
/// [`UnboundedReceiver`](tokio::sync::mpsc::UnboundedReceiver).)
pub type CmdReceiver<C> = tokio::sync::mpsc::UnboundedReceiver<C>;

/// Construct a command-lane [`CmdSender`]/[`CmdReceiver`] pair.
///
/// In normal operation you do **not** call this — [`run`] creates the
/// per-session channel and hands the ends to [`init`](Plugin::init) and
/// [`sources`](Plugin::sources) for you. It is exposed so unit tests can build
/// a plugin's model without a live runtime (e.g. `Model::init(cmd_channel().0)`),
/// and for the rare plugin that needs an auxiliary channel of its own.
#[must_use]
pub fn cmd_channel<C>() -> (CmdSender<C>, CmdReceiver<C>) {
    tokio::sync::mpsc::unbounded_channel()
}

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
    /// The plugin's mount surface became visible (`true`) or hidden (`false`) —
    /// its host [`SlotVisibility`](proto::HostMsg::SlotVisibility). The runtime
    /// delivers one **at register** (seeded from the host so a (re)connecting
    /// plugin starts in the right state), then one on every change.
    ///
    /// This is the hook for **parking your own pollers while nobody is looking**:
    /// gate a `sources()` fetch/tick loop on the latest value (fetch while
    /// visible, idle while hidden), the same energy behavior the shell already
    /// applies to its built-in pollers. Ignoring it keeps today's always-on
    /// behavior — nothing breaks.
    ///
    /// **Latest-wins delivery.** Visibility is state, not a one-shot event, so a
    /// burst of toggles may coalesce to the newest value; act on the value you
    /// receive, never assume you saw every intermediate edge.
    SlotVisible(bool),
    /// A message from the plugin's own [`sources`](Plugin::sources) stream.
    App(M),
}

/// The Elm Architecture core of a plugin: pure state + `update` + `view`.
/// Implement this and hand the type to [`run`] — the trait has no transport
/// surface at all, which is what keeps every method unit-testable without a
/// socket or a host.
pub trait Plugin: Sized {
    /// Messages produced by this plugin's own [`sources`](Plugin::sources)
    /// (timer ticks, fetch results) — the **inbound** side of its own I/O,
    /// folded in as [`Input::App`]. Use [`std::convert::Infallible`] for a
    /// purely host-driven plugin.
    type Msg;

    /// Commands this plugin dispatches from [`update`](Plugin::update) to its
    /// own I/O task — the **outbound** side, symmetric to [`Msg`](Plugin::Msg)
    /// (see the crate-level *Commands* section). The runtime carries them over
    /// a per-session [`CmdSender`]/[`CmdReceiver`] pair. Use
    /// [`std::convert::Infallible`] for a plugin that only *displays* state and
    /// issues no I/O of its own (it then ignores both channel ends).
    type Cmd;

    /// The plugin's self-description: id, subscriptions, capabilities, mount.
    /// Sent as the `Register` handshake frame on every (re)connect.
    fn manifest() -> Manifest;

    /// The initial model, built fresh on every session (see the crate docs on
    /// per-session state). Its [`view`](Plugin::view) is the seed render, sent
    /// immediately so the slot mounts before the first snapshot lands.
    ///
    /// `cmds` is this session's command sender (see the crate-level *Commands*
    /// section): a plugin that issues its own I/O stores it in the model and
    /// [`send`](CmdSender::send)s on it from [`update`](Plugin::update); a
    /// purely host-driven plugin (`Cmd = Infallible`) ignores it.
    fn init(cmds: CmdSender<Self::Cmd>) -> Self;

    /// The plugin's own message stream (timers, fetches), or `None` for a
    /// purely host-driven plugin. Called once per session; the stream is
    /// dropped on disconnect.
    ///
    /// `cmds` is this session's command receiver, paired with the sender given
    /// to [`init`](Plugin::init): a plugin that issues I/O drains it in the
    /// task that also emits its [`Msg`](Plugin::Msg)s (returning that task's
    /// stream here); the default drops it, which is what a source-less or
    /// command-less plugin wants.
    #[must_use]
    fn sources(cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        let _ = cmds;
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
