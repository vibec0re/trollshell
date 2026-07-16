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
//!
//! # The `preem` raster kit
//!
//! [`preem`] is the SDK's GTK-free retro-display toolkit (issue #356): a
//! shared RGBA8 framebuffer ([`preem::Frame`]), the 5×7 pixel font
//! ([`preem::font`], promoted from the pet's speech bubble), and predefined
//! widgets — [`preem::dot_matrix`], [`preem::seven_seg`], and the
//! [`preem::TextBox`] 8bit textbox — all rendering into
//! [`Node::Pixels`](proto::Node::Pixels) buffers in the VFD / LCD / OLED
//! [`preem::DisplayStyle`] skins. See that module's docs; the
//! `hytte-plugin-preem-demo` crate is the reference consumer.
//!
//! # Styling
//!
//! A plugin's entire style surface is the `classes` field every [`Node`]
//! variant carries (a `Vec<`[`Cls`](proto::Cls)`>`, a plain CSS class token) —
//! there is no other hook, and a plugin cannot ship its own CSS (see
//! *Scoped plugin stylesheets* below). Classes flow to GTK **verbatim**: the
//! host's reconciler calls `add_css_class` once per token, with no
//! filtering, renaming, or validation (`hytte-ui`'s
//! `widget_tree::apply_classes`, which every [`Node`] kind's `classes` goes
//! through). Whatever rule matches that class name in the shell's
//! already-loaded stylesheets — libadwaita's own, or the shell's — paints;
//! there is nothing plugin-specific about the mechanism itself.
//!
//! ## The blessed set: standard libadwaita style classes
//!
//! These are libadwaita's own documented style classes, not a hytte
//! invention, so they theme identically in any libadwaita app — safe to put
//! in any [`Node`]'s `classes` today, with no host change and no proto
//! version bump:
//!
//! - **Typography** — `heading`, `caption-heading`, `title-1`..`title-4`
//!   (`title-1` largest), `numeric` (tabular figures — a clock or a
//!   temperature reads better with it), `monospace`, `dim-label` (dims text
//!   to the secondary/muted opacity).
//! - **State** — `success`, `warning`, `error` (recolor a `Label`/`Icon`'s
//!   foreground to the semantic color, not a background fill), `accent`.
//! - **Containers** — `flat` (drops a `Box`/`Button`'s frame — the standard
//!   "no chrome" hook), `card` (libadwaita's own rounded, shadowed surface —
//!   see the sidebar-mount caution below before reaching for this one),
//!   `boxed-list` on a [`Node::ListBox`] (the native carded, separated-rows list
//!   — see *Native card lists* below).
//!
//! `hytte-plugin-weather` (this workspace's reference weather card) sets
//! `flat` on its root today — proof the mechanism needs no shell change to
//! land. The issue #316 motivating consumer, the out-of-tree vibectl sidebar
//! widget, goes further: `heading` for titles, `dim-label` + `numeric` for
//! secondary readouts, alongside its own private hooks.
//!
//! ## Native card lists (`.boxed-list`) and collapsible rows (#333)
//!
//! A [`Node::ListBox`] materializes as a **real `gtk::ListBox`** (selection-less),
//! not a plain box, specifically so libadwaita's `.boxed-list` styling — which
//! selects `list.boxed-list` and its `> row`s — actually paints. Put
//! `"boxed-list"` in a `ListBox`'s `classes` and its [`Node::Row`] children
//! (auto-wrapped in list rows by GTK) get the carded surface, rounded corners,
//! and hairline row separators of a native Adwaita list — no shell change, no
//! proto bump. So the recipe for a native card list is just:
//!
//! ```ignore
//! Node::ListBox {
//!     id: Some("devices".into()),
//!     classes: vec!["boxed-list".into()],
//!     children: vec![
//!         Node::Row {
//!             id: Some("lamp".into()),
//!             classes: vec![],
//!             children: vec![
//!                 Node::Label { id: None, text: "Lamp".into(), classes: vec![] },
//!                 Node::Spacer,
//!                 Node::Label { id: None, text: "On".into(), classes: vec!["dim-label".into()] },
//!             ],
//!         },
//!         // …one Row per device…
//!     ],
//! }
//! ```
//!
//! For a **collapsible** section, reach for [`Node::Expander`] instead of
//! hand-rolling a button + chevron + revealer. It renders a flat, full-width
//! header (your `header` node, with a trailing disclosure chevron) over a
//! revealer holding `children`. Clicking the header fires an
//! [`EventKind::Click`](proto::EventKind::Click) addressed by the expander's `id`
//! — fold that into your model, flip `expanded`, and re-render; the host reveals
//! the body and rotates the chevron. Because the toggle round-trips as a plain
//! click a plugin already opts into by rendering the node, `Expander` needs no
//! new event kind and no manifest opt-in:
//!
//! ```ignore
//! // In `view`, driven by `self.rooms[i].open` in your own model:
//! Node::Expander {
//!     id: format!("room:{}", room.id),
//!     header: Box::new(Node::Label {
//!         id: None, text: room.name.clone(), classes: vec!["heading".into()],
//!     }),
//!     children: room.devices.iter().map(device_row).collect(),
//!     expanded: room.open,
//!     classes: vec![],
//! }
//! // In `update`, on Input::Event { node, kind: Click } where node == "room:…":
//! //     toggle that room's `open`, return the new model → the host re-renders.
//! ```
//!
//! ## Shell-provided guarantees for sidebar mounts
//!
//! A plugin mounted at [`Mount::SidebarLead`](proto::Mount::SidebarLead),
//! [`SidebarTop`](proto::Mount::SidebarTop), or
//! [`SidebarBottom`](proto::Mount::SidebarBottom) renders as one card inside
//! a host-managed region; the host wraps every plugin's card root in its own
//! `gtk::Box` carrying `.ts-plugin-card` **automatically** (issue #319,
//! `trollshell/src/plugins.rs`'s `reconcile_region`) — do not add that class
//! yourself, and avoid stacking libadwaita's `card` on your own root either
//! (two nested rounded/shadowed surfaces read as a card-in-a-card). That
//! wrapper gives every plugin card the same `@sidebar_card_background`
//! opaque fill, corner radius, and inter-card margin as the shell's own
//! weather/tasks/departures cards (`assets/trollshell/style.css`) — a solid
//! dark surface instead of the sidebar's frosted, semi-translucent panel
//! showing straight through, so plugin text stays legible with no per-plugin
//! contrast tuning. Text color (white) is inherited the same way, from the
//! sidebar's own ancestor rule — nothing to set. The host's card adds **no
//! padding** of its own: your root owns its inner spacing, same as the
//! built-ins, so nobody double-pads.
//!
//! [`Mount::BarLeft`](proto::Mount::BarLeft),
//! [`BarCenter`](proto::Mount::BarCenter), and
//! [`BarRight`](proto::Mount::BarRight) renders are not wired up yet (v1
//! drops them — see `trollshell/src/plugins.rs`), so this card guarantee is
//! sidebar-only in practice today.
//!
//! ## `Node::Pixels` paints no CSS background
//!
//! `classes` still attach to a [`Node::Pixels`]'s widget the same way as
//! every other kind, but the GTK widget behind it (`hytte-ui`'s
//! `pixels::PixelSurface`) overrides GTK's `snapshot` vfunc to paint only its
//! RGBA8 texture and never chains up to the default CSS background/border
//! paint — so a `card`, `error`, or any other background-painting class is a
//! silent no-op there. Wrap a raster node in a `Box` if it needs a themed
//! backdrop.
//!
//! **Size it with `scale`, not shell CSS** (#358): a `Pixels` surface's
//! natural size is its buffer size times its `scale` hint, so a `128×128`
//! buffer at `scale: 2` renders a crisp 256 px without any shell-side px
//! rule (integer factors are the sharp case for the nearest-neighbor
//! upscale). Host CSS can still override upward, but a plugin no longer
//! depends on it for a sane default.
//!
//! ## What NOT to rely on
//!
//! The shell's own sidebar and bar widgets carry internal classes —
//! `ts-sidebar-*`, `hytte-bar-*`, and friends — styled in the binary's or
//! library's own stylesheet for *their* layout, not offered as a plugin
//! contract; they can be renamed or restyled without notice. If a class
//! isn't in the blessed set above or the automatic `.ts-plugin-card`
//! wrapper, don't copy it off a native widget just because it looks right in
//! the shell's CSS today.
//!
//! Two of this workspace's reference plugins are the exception worth calling
//! out rather than imitating: `hytte-plugin-weather` and
//! `hytte-plugin-departures` are 1:1 ports of what used to be native sidebar
//! chips, and their views still set the shell's own `ts-weather*` /
//! `ts-departures*` classes to keep pixel parity with the pre-port look.
//! That's a historical artifact of the port, not a supported third-party
//! surface — a new plugin should reach for the blessed set and the
//! `.ts-plugin-card` guarantee above, not grep these two for class names.
//!
//! ## Scoped plugin stylesheets: deferred (issue #316, gap 3)
//!
//! A plugin cannot register CSS of its own, scoped to its mount — `classes`
//! only ever select into stylesheets the *host* loads. That is a deliberate
//! v1 non-goal, not an oversight: GTK CSS selectors are unscoped by default,
//! so letting an out-of-process, third-party plugin ship a stylesheet raises
//! a scoping/security question (its rules reaching outside its own subtree,
//! or clobbering shell chrome) that deserves its own design pass rather than
//! a speculative answer bolted onto this doc. Revisit on demand.
//!
//! ## Example
//!
//! ```ignore
//! Node::Box {
//!     id: None,
//!     dir: Dir::Vertical,
//!     spacing: 4,
//!     scroll: false,
//!     // No `.ts-plugin-card` and no `.card` here — the host's region
//!     // wrapper already supplies the card treatment (see above).
//!     classes: vec!["flat".into()],
//!     children: vec![
//!         Node::Label {
//!             id: None,
//!             text: "Living Room".into(),
//!             classes: vec!["heading".into()],
//!         },
//!         Node::Label {
//!             id: None,
//!             text: "21°C".into(),
//!             classes: vec!["numeric".into(), "dim-label".into()],
//!         },
//!     ],
//! }
//! ```

use hytte_plugin_proto::{Effect, EffectOutcome, EventKind, Manifest, Node, NodeId, StateSnapshot};

pub mod preem;

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
        /// What happened (click / scroll / slider move / entry submit).
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
