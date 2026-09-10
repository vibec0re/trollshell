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
//! vocabulary, [`tokio`], and [`tokio_stream`] are all re-exported, and
//! [`tick_stream`] wraps the common periodic-`sources` incantation — so even a
//! source-driven plugin needs no direct async-runtime dependency; no GTK, no
//! `hytte` umbrella either) and its `main` is one line.
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
//! # Process shutdown (#1079)
//!
//! Don't confuse this with the per-session "Shutdown ≡ disconnect" above,
//! which the *host* sends and which the runtime answers by redialing —
//! [`run`] never exits over it. This section is the other lifecycle
//! entirely: the *process* exiting, which only happens when the launcher
//! (`systemd-run --user`, `trollshell/src/plugin_launcher.rs`) stops the
//! plugin's transient unit, sending `SIGTERM` (or `SIGINT` under a manual
//! `Ctrl-C`).
//!
//! [`run`] installs a listener for both signals. On the first one it flips a
//! shutdown flag; the session loop finishes whatever frame is already in
//! flight (it does not abort mid-write), then, instead of redialing, calls
//! [`Plugin::shutdown`] — a bounded chance, not a blocking one, to flush
//! whatever a plugin persists (a grant store's queued write, a timer's saved
//! state, a fetch cache): the hook runs under a 2 s inner grace, cut off with
//! a warning if it overruns, and the process exits 0 either way. A signal
//! arriving before any session ever connected (still dialing, or sleeping
//! out the reconnect backoff) skips straight to exit — no session, so no
//! model to flush and [`sources`](Plugin::sources) is never called. Systemd's
//! own `TimeoutStopSec` on the unit is the *outer* bound: if a stuck hook (or
//! a stuck in-flight frame) still hasn't let the process exit by then,
//! systemd escalates to `SIGKILL`, past anything this runtime can do about
//! it. A plugin with nothing to flush needs no code at all — the default
//! [`Plugin::shutdown`] is a no-op, so the process still exits promptly.
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
//! brokers (open a page, drive niri/media, run a command, post a notification
//! toast via [`Effect::Notify`](proto::Effect::Notify) once the manifest grants
//! [`Capability::Notify`](proto::Capability::Notify), #406). The whole effect
//! vocabulary is the re-exported [`proto::Effect`] enum — a plugin names its
//! variants directly, and the host is the one that brokers **and cap-checks**
//! each: an effect whose [`Capability`](proto::Capability) the manifest doesn't
//! list is dropped by the host before it is brokered (#436), so
//! [`Manifest::capabilities`](proto::Manifest::capabilities) has to name every
//! capability the plugin's effects need or those effects simply never happen.
//! **Since #1058 the runtime also checks this client-side**, using the same
//! [`Effect::required_capability`](proto::Effect::required_capability) mapping
//! the host enforces: an emitted effect the manifest didn't grant is dropped
//! before it ever reaches the wire, once per effect kind per session — named
//! with an `eprintln!` to the plugin's own stderr *and* a
//! [`PluginMsg::Log`](proto::PluginMsg::Log) frame to the host (which routes
//! it through its own journal), so an author sees their own mistake rather
//! than a click that silently does nothing. **Getting that pairing wrong still
//! costs more than the dropped effect** for a capability whose generation an
//! older shell predates — `Shader` (#893), `OpenUri` (#1045) — declaring it is
//! also what makes that shell refuse the plugin *loudly at the handshake*, so
//! an undeclared emit that somehow reaches the wire (a non-Rust plugin, or one
//! bypassing this runtime) sails through `Register` there and then fails to
//! decode on the first render frame carrying it, i.e. the #437 crash-loop.
//! Declare what you emit.
//! A plugin's **own** external I/O — send a frame on the WebSocket it holds,
//! fire an HTTP call — is not a shell effect: the design does that in-process and
//! never round-trips it through the host. But `update` is sync, so it cannot do
//! the I/O itself; the sanctioned lane is a typed **command** channel (issue #280).
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
//! # …and [`display`], the path that doesn't rasterise (#884)
//!
//! Reach for [`display`] rather than the raw kit for anything the preem
//! **state vocabulary** (#882) has a word for. Its wrappers wear the kit's
//! shape — [`display::Gauge`], [`display::Marquee`], [`display::FlipBoard`], …
//! — but decide **at render time**, from the generation the host advertised in
//! [`HostMsg::Hello`](proto::HostMsg::Hello), whether the widget goes out as a
//! typed [`Node::Preem`](proto::Node::Preem) the shell draws and animates, or
//! as the CPU-rasterised [`Node::Pixels`](proto::Node::Pixels) it is today.
//!
//! One `update`/`view` pair serves both hosts: state setters always take
//! effect, and `advance(dt)` — the plugin-side animation tick — is a no-op
//! while the host speaks preem, because the shell owns the needle spring, the
//! phosphor, the flip clocks and the scroll offset there. Against a host that
//! never advertises, every widget rasterises exactly as before; version skew
//! degrades to the status quo, never to a blank chip. Rasterising by hand stays
//! the escape hatch for drawing the vocabulary has no word for (the pet's face,
//! caw's speech bubble) — see [`display`]'s docs.
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
//!                 Node::Label { id: None, text: "Lamp".into(), classes: vec![], tooltip: None },
//!                 Node::Spacer,
//!                 Node::Label { id: None, text: "On".into(), classes: vec!["dim-label".into()], tooltip: None },
//!             ],
//!         },
//!         // …one Row per device…
//!     ],
//! }
//! ```
//!
//! ## Sizing a list card: `spacing`, `dense`, and a real viewport (#966)
//!
//! Mara's live test of a 12-agent list card found three things the vocabulary
//! could not express, all of them host-side. The [`nodes`] module is the
//! ergonomic front for the fixes (it also defaults the two new fields, so a
//! literal you already wrote keeps working):
//!
//! - **[`Node::Row`] now carries `spacing`** — before it, a row's children butted
//!   together (`⚙argus`) unless you padded with [`Node::Spacer`]s.
//!   `nodes::row(children).spacing(6)`.
//! - **[`Node::ListBox`] now carries `dense`.** The host wraps every list child
//!   in a `GtkListBoxRow` — that is what makes `.boxed-list` paint — and the
//!   wrapper carries libadwaita's row height floor, which was most of the ~700 px
//!   that card took. `nodes::list(rows).dense(true)` drops it, and the rows are
//!   as tall as their content. You cannot reach the wrapper any other way: it
//!   never appears in the tree you sent.
//! - **[`Node::Scrolled`] bounds a card.** GTK CSS has no `max-height` and
//!   [`Node::Box`]'s `scroll` is only an *event target* — it forwards wheel
//!   deltas and neither clips nor scrolls — so before #966 a long list simply
//!   grew and pushed everything below it off the surface.
//!   `nodes::scrolled(240, body).build()` wraps `body` in a vertical viewport
//!   that is as tall as its content up to 240 px and scrolls the rest, and
//!   **degrades to the bare child** against a shell too old to decode the
//!   variant (it is negotiated, like the preem and shader vocabularies).
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
//!         id: None, text: room.name.clone(), classes: vec!["heading".into()], tooltip: None,
//!     }),
//!     children: room.devices.iter().map(device_row).collect(),
//!     expanded: room.open,
//!     classes: vec![],
//! }
//! // In `update`, on Input::Event { node, kind: Click, .. } where node == "room:…":
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
//! [`BarRight`](proto::Mount::BarRight) render as **bar chips** (#367): the
//! host mounts your `view()` tree as a single chip in the requested bar
//! region. A bar chip carries the shell's `.ts-plugin-chip` wrapper (not the
//! `.ts-plugin-card` surface — a bar chip is a slim inline widget, not a
//! sidebar card), so the "don't add `.card`/`.ts-plugin-*` yourself" rule
//! above applies to bar mounts too.
//!
//! ## Opening your own panel
//!
//! Any plugin — bar chip or sidebar card — may ALSO define an optional drawer
//! **panel** (#349): a second, independent `Node` tree the host mounts as its
//! own dedicated drawer page. Publish it by returning it on the [`View`] —
//! `View::new(chip).panel(detail)` from [`Plugin::view`] (a plain `node.into()`
//! = no panel, chip/card only); the whole `View` is re-projected every update
//! and the runtime dedups + sends it as one render frame. To open it, emit
//! [`Effect::OpenPage(Page::PluginSelf)`](proto::Page::PluginSelf) from
//! `update` — typically in response to a click on your chip/card — which needs
//! the [`OpenPage`](proto::Capability::OpenPage) capability like any other
//! page-open. The host resolves `PluginSelf` to *your* panel using the effect's
//! plugin id; you never name a page. The panel gets the drawer's own dark card
//! chrome, so a plugin panel root should NOT add `.ts-plugin-card`/`.card`
//! (same "don't double-card" caution as the sidebar-mount section above) — it
//! owns only its inner content and spacing.
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
//! ## Scoped plugin stylesheets: a deliberate non-goal (issue #316, gap 3)
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
//!             tooltip: None,
//!         },
//!         Node::Label {
//!             id: None,
//!             text: "21°C".into(),
//!             classes: vec!["numeric".into(), "dim-label".into()],
//!             tooltip: Some("Measured 4 minutes ago".into()),
//!         },
//!     ],
//!     tooltip: None,
//! }
//! ```
//!
//! # Tooltips
//!
//! Seven node kinds carry an optional `tooltip: Option<String>`, which the host
//! applies with `gtk::Widget::set_tooltip_text` (#957, #893, #961):
//! [`Node::Box`], [`Node::Label`], [`Node::Icon`], [`Node::Shader`],
//! [`Node::Row`], [`Node::Text`] and [`Node::Expander`]. It is the one place a
//! plugin can put words that don't fit on screen, and a **bar chip is where it
//! earns its keep**: a chip is a handful of 16px glyphs, and the claude-bridge
//! chip's `sub 18/0` was legible only to someone who had read its source. Hang
//! one string on the chip's **root box** and hovering anywhere on the pill
//! answers the question — no drawer panel, no extra capability, no new node.
//!
//! It is **plain text, not Pango markup**: the host calls `set_tooltip_text`, so
//! `<b>` arrives as four literal characters. It is a mutable prop like `text` —
//! a same-id re-render retitles in place, and re-rendering it as `None` clears
//! the hover rather than leaving the last string armed — and it is *not* part of
//! a node's identity, so changing it never rebuilds the widget.
//!
//! Set `tooltip: None` when you have nothing to say; it costs no wire bytes
//! (`skip_serializing_if`), and an older host that predates the field skips the
//! key rather than failing the frame, so using it is safe against any shell.
//! [`nodes::row`](crate::nodes::row) defaults it for you —
//! `nodes::row(children).tooltip("argus · running").build()`.
//!
//! ## The list-card three (#961)
//!
//! - **[`Node::Row`]** is where a card's legend goes: one string per row, on
//!   the row. A child with its own tooltip still wins the hover where the
//!   pointer is over it, so a row tooltip is a fallback rather than a blanket.
//!   (Before the field, the only way to get hover text on a row was to spell it
//!   as a horizontal [`Node::Box`] instead.)
//! - **[`Node::Expander`]**'s lands on the **header button**, not on the whole
//!   expander — so an expanded body does not inherit the header's legend.
//! - **[`Node::Text`] has a default.** When `ellipsize` is `true` and you set
//!   no tooltip, the host uses the node's **full `text`** as the hover, which is
//!   the only way a reader can see what the `…` cut off. An explicit tooltip
//!   always wins; a `Text` that does not ellipsize gets none. The host does not
//!   check whether the label is *actually* truncated (that depends on the
//!   allocation), so a short ellipsizing string in a wide container hovers
//!   itself — a legend, not a bug. Where that is unwanted: set a tooltip of
//!   your own, set a **blank** one (`Some(" ")`) to keep `ellipsize` and get no
//!   hover at all, or leave `ellipsize` off.
//!
//! **The derived hover is a child tooltip, so it wins where the pointer is over
//! it.** The case to know about: an [`Node::Expander`] whose header *is* an
//! ellipsizing [`Node::Text`] hands the reader the title back instead of the
//! header's legend, since GTK answers a hover from the deepest widget upward.
//! Three ways out: put the legend on the header `Text` itself (explicit beats
//! derived), blank the header `Text`'s tooltip so only the expander's own
//! legend is left to answer, or don't ellipsize a header you want to explain.
//!
//! **A blank tooltip arms nothing**, derived or explicit: an empty or
//! whitespace-only string is dropped by the host rather than popping an empty
//! tooltip window. Because an explicit value is read *before* the derived one,
//! a blank explicit tooltip **suppresses** the `Text` default rather than
//! falling back to it — which is what makes it an opt-out rather than a no-op.

use std::future::Future;
use std::time::Duration;

use hytte_plugin_proto::{
    AudioSpectrum, ConsentDecision, DatasourceOutcome, Effect, EffectOutcome, EventKind, Manifest,
    Node, NodeId, NowPlaying, StateSnapshot, UpcomingEvent,
};

/// The retro raster kit (#356), its own GTK-free leaf crate since #859 so the
/// shell can rasterize with it without linking this client SDK. Re-exported
/// here under its historical module name: a plugin still depends on
/// `hytte-plugin` alone, and every `hytte_plugin::preem::…` path keeps
/// resolving to the same items.
pub use hytte_preem as preem;

pub mod display;
pub mod nodes;
pub mod shader;

mod runtime;

pub use runtime::run;

/// The full wire vocabulary, re-exported so a plugin depends on this crate
/// alone. (`Manifest`, `Node`, `Effect`, … are what a plugin actually names;
/// the codec/framing helpers matter only if you bypass [`run`].)
pub use hytte_plugin_proto as proto;

/// The async runtime the SDK itself runs on, re-exported so a plugin can spawn
/// tasks, hold channels, and name tokio types (e.g. from the I/O task its
/// [`sources`](Plugin::sources) own) without a direct `tokio` dependency —
/// mirroring the [`proto`] re-export. The SDK enables the `rt`, `time`, `sync`,
/// `net`, `io-util`, and `macros` features; a plugin needing one beyond those
/// still adds `tokio` itself.
pub use tokio;

/// Stream constructors/combinators for building [`Plugin::sources`] values
/// (`iter`, `wrappers::*`, `StreamExt`, …) — re-exported wholesale so a
/// source-driven plugin still needs no dependency beyond this crate. The SDK
/// enables the `time` (`wrappers::IntervalStream`, wrapped by [`tick_stream`])
/// and `sync` (`wrappers::UnboundedReceiverStream`) features, so both are usable
/// straight off this re-export.
pub use tokio_stream;

/// A boxed message stream returned by [`Plugin::sources`]. Any well-behaved
/// [`Stream`](tokio_stream::Stream) qualifies (build one from the
/// re-exported [`tokio_stream`]: its wrappers, `iter`, channel receivers, …);
/// the runtime polls it inside a `select!`, so it must tolerate being polled
/// incrementally, as all standard combinators do.
pub type MsgStream<M> = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = M>>>;

/// A periodic message stream for [`Plugin::sources`]: emit `msg` (cloned) every
/// `period`, forever. This wraps the copy-pasted
/// `IntervalStream::new(tokio::time::interval(dt)).map(|_| Msg::Tick)`
/// incantation so a plugin driving a fixed-cadence re-render needs neither a
/// direct `tokio`/`tokio_stream` dependency nor the `time`-feature wrapper.
///
/// Like [`tokio::time::interval`], the **first tick fires immediately**, then
/// one every `period` (a heartbeat plugin can treat that leading edge as a
/// harmless no-op). The result is a plain [`Stream`](tokio_stream::Stream): box
/// it into a [`MsgStream`] for a timer-only plugin, or
/// [`merge`](tokio_stream::StreamExt::merge) it with the plugin's other sources.
///
/// ```ignore
/// fn sources(_cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
///     Some(Box::pin(tick_stream(Duration::from_secs(1), Msg::Tick)))
/// }
/// ```
#[must_use = "a tick stream does nothing unless polled (box it into a MsgStream)"]
pub fn tick_stream<M: Clone>(period: Duration, msg: M) -> impl tokio_stream::Stream<Item = M> {
    use tokio_stream::StreamExt as _;
    tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(period))
        .map(move |_| msg.clone())
}

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
    ///
    /// # Match it with `..`; build it with [`event`](Input::event)
    ///
    /// The variant is `#[non_exhaustive]`, so outside `hytte-plugin` it can be
    /// neither matched without a trailing `..` — `Input::Event { node, kind,
    /// .. }` — nor constructed with struct syntax. Use
    /// [`Input::event`](Input::event) / [`Input::event_on`](Input::event_on)
    /// instead.
    ///
    /// That is not decoration. [`output`](Input::Event::output) landed in
    /// #1050 as a third field, and adding a field to a struct variant is a
    /// source break for every plugin in the tree: the arm appears in thirteen
    /// plugin crates, each of which had to be edited. The attribute is what
    /// makes that the **last** such sweep — a fourth field can be added without
    /// touching a single plugin, because every arm already ends in `..` and
    /// every construction already goes through a constructor that can default
    /// it. #1068 deferred the field precisely so this flag day would be paid
    /// once, in the round that has a consumer for the value; this is that
    /// round.
    ///
    /// # The attribute is load-bearing, and this pins it
    ///
    /// Within this crate a `#[non_exhaustive]` variant is an ordinary one, so
    /// no unit test here can see the attribute at all — and the plugin crates
    /// cannot either: they check that today's arms *compile*, which they would
    /// go on doing if the attribute vanished. A **doctest**, though, compiles
    /// as its own crate linked against `hytte-plugin`, which is exactly the
    /// vantage point where the attribute bites.
    ///
    /// Struct-literal construction from outside the crate must not compile:
    ///
    /// ```compile_fail,E0639
    /// use hytte_plugin::{Input, proto::EventKind};
    /// let _: Input<()> = Input::Event {
    ///     node: "btn".to_owned(),
    ///     kind: EventKind::Click,
    ///     output: None,
    /// };
    /// ```
    ///
    /// …and neither must an exhaustive match without `..`:
    ///
    /// ```compile_fail,E0638
    /// use hytte_plugin::{Input, proto::EventKind};
    /// let input: Input<()> = Input::event("btn", EventKind::Click);
    /// if let Input::Event { node, kind, output } = input {
    ///     let _ = (node, kind, output);
    /// }
    /// ```
    ///
    /// The constructors are the supported way in, and they compile:
    ///
    /// ```
    /// use hytte_plugin::{Input, proto::EventKind};
    /// let _: Input<()> = Input::event("btn", EventKind::Click);
    /// let _: Input<()> =
    ///     Input::event_on("btn", EventKind::Click, Some("DP-2".to_owned()));
    /// ```
    ///
    /// The `E0639`/`E0638` codes are pinned in the fences so neither block can
    /// pass for an unrelated compile error.
    #[non_exhaustive]
    Event {
        /// The interacted node, by the id the plugin assigned in its view.
        node: NodeId,
        /// What happened (click / scroll / slider move / entry submit).
        kind: EventKind,
        /// The **connector name** of the monitor whose copy of the mirrored
        /// card produced this interaction (#1050) — `"DP-1"`, `"eDP-1"`,
        /// `"HDMI-A-2"`, as niri and Wayland spell them. The plugin-side face
        /// of [`HostMsg::Event.output`](proto::HostMsg::Event::output).
        ///
        /// One tree is mirrored onto every screen, so a click used to be
        /// un-attributable: a plugin acting on "the screen the user clicked on"
        /// had to guess, and the only proxy available — whichever output holds
        /// keyboard focus — is a different thing (#1019's layouts chip laid out
        /// the focused workspace, not the workspace on the screen whose chip was
        /// pressed). `Some(connector)` names the screen for real.
        ///
        /// **`None` means the host could not attribute the event to a screen —
        /// never "the primary monitor".** Treat it as unknown and fall back to
        /// whatever the plugin did before #1050. The host sends it from exactly
        /// one place today: the drawer **panel**, whose page stack is built
        /// with no monitor in scope. Bar chips and sidebar cards always carry
        /// `Some`.
        ///
        /// Pair it with [`View::hidden_on`], the other half of the per-screen
        /// story: `hidden_on` decides *where the card is*, `output` decides
        /// *which screen a click on it meant*.
        output: Option<String>,
    },
    /// The outcome of a brokered
    /// [`Effect::RunCommand`](proto::Effect::RunCommand) or
    /// [`Effect::OpenUri`](proto::Effect::OpenUri), keyed by the effect's `id`.
    ///
    /// Three things reply here, and they mean different things:
    ///
    /// - an **attached** `RunCommand` ([`Effect::run_command`](proto::Effect::run_command))
    ///   — the program's own exit status plus its captured stdout;
    /// - a **detached launch** ([`Effect::launch`](proto::Effect::launch), #953)
    ///   — arrives immediately and reports only whether the *launch* succeeded,
    ///   since the host hands the program to the systemd user manager and never
    ///   waits for it;
    /// - an **`OpenUri`** ([`Effect::open_uri`](proto::Effect::open_uri), #1045)
    ///   — whether the desktop's default handler was started. A URI the host
    ///   refuses (a scheme outside `http`/`https`/`file`) comes back `ok: false`
    ///   with the reason in [`output`](proto::EffectOutcome::output), which is
    ///   there so a plugin can toast it instead of leaving a click that
    ///   silently does nothing. A refusal answers immediately; a *launch*
    ///   answers when the desktop has resolved the handler, which may be after
    ///   an application-chooser dialog, so do not assume it lands in the same
    ///   frame as the click.
    ///
    /// `RunCommand` and `OpenUri` both reply on this same `id` field, and
    /// [`RequestConsent`](proto::Effect::RequestConsent)'s
    /// [`ConsentDecision`](Input::ConsentDecision) round-trip carries the same
    /// allocation contract on its own `request_id` — so treat every
    /// reply-bearing effect kind as one shared id space and allocate from a
    /// single counter, not one per kind, or two effects in flight at once can
    /// collide on the same value (#1060).
    EffectResult {
        /// The `id` the plugin chose on the originating effect.
        id: u64,
        /// Whether it succeeded, and any captured output — or, for a detached
        /// launch, whether it started and what it was named; or, for an
        /// `OpenUri`, why it was refused.
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
    /// **Sidebar mounts only — a bar chip is always visible.** This models the
    /// **sidebar** opening and closing (#288/#422), not a bar chip's presence: a
    /// [`Mount::BarLeft`](proto::Mount::BarLeft)/`BarCenter`/`BarRight` chip is
    /// effectively always on-screen, so the host seeds it a constant `true` and
    /// sends no edges. A bar-mounted plugin must therefore **not** park its
    /// pollers on this signal (it would idle while fully visible) — the park
    /// pattern above is for sidebar cards.
    ///
    /// **Latest-wins delivery.** Visibility is state, not a one-shot event, so a
    /// burst of toggles may coalesce to the newest value; act on the value you
    /// receive, never assume you saw every intermediate edge.
    SlotVisible(bool),
    /// The latest audio-reactive spectrum off the default sink's monitor (#405):
    /// the host [`AudioSpectrum`](proto::AudioSpectrum) push, delivered only to a
    /// plugin that subscribes
    /// [`StateKey::AudioSpectrum`](proto::StateKey::AudioSpectrum). A `{peak,
    /// bins}` frame arrives ~20 Hz **latest-wins** — fold it into the model and
    /// render bars / a needle / beat-driven frames. A plugin rendering slower
    /// than 20 Hz just sees the freshest frame each time; there is no backlog to
    /// drain. Ignoring it costs nothing.
    AudioSpectrum(AudioSpectrum),
    /// The human's answer to an [`Effect::RequestConsent`](proto::Effect::RequestConsent)
    /// this plugin raised (#487 phase 1b), the host
    /// [`ConsentDecision`](proto::HostMsg::ConsentDecision) push. Keyed by the
    /// `request_id` the plugin chose on the originating `RequestConsent`, so a
    /// plugin with several prompts in flight can correlate the answer. Delivered
    /// only to a plugin that declared
    /// [`Capability::Consent`](proto::Capability::Consent) and actually requested
    /// a prompt; an unanswered prompt resolves to
    /// [`ConsentDecision::Deny`](proto::ConsentDecision::Deny) after the host's
    /// 60 s timeout, so this always eventually arrives.
    ConsentDecision {
        /// The `request_id` the plugin chose on the originating `RequestConsent`.
        request_id: u64,
        /// The human's choice (or `Deny` on the 60 s timeout).
        decision: ConsentDecision,
    },
    /// The next few upcoming calendar events (#484): the host
    /// [`CalendarUpcoming`](proto::HostMsg::CalendarUpcoming) push, delivered only
    /// to a plugin that subscribes
    /// [`StateKey::CalendarUpcoming`](proto::StateKey::CalendarUpcoming) **and**
    /// declares [`Capability::Calendar`](proto::Capability::Calendar). A capped
    /// digest (the next [`MAX_UPCOMING_EVENTS`](proto::MAX_UPCOMING_EVENTS) in the
    /// coming 24 h), pushed on change; fold it into the model like any other host
    /// input. Times are Unix seconds — format them in the consumer's local time.
    CalendarUpcoming(Vec<UpcomingEvent>),
    /// The session's logind lock state (#484): the host
    /// [`SessionLocked`](proto::HostMsg::SessionLocked) push, `true` while locked.
    /// Delivered only to a plugin that subscribes
    /// [`StateKey::SessionLocked`](proto::StateKey::SessionLocked) **and** declares
    /// [`Capability::SessionState`](proto::Capability::SessionState). Seeded at
    /// register, then pushed on every change — key a "first unlock" action off the
    /// `true`→`false` transition, or blank sensitive content while `true`.
    SessionLocked(bool),
    /// The current-track digest off the mpris active player (#528): the host
    /// [`NowPlaying`](proto::HostMsg::NowPlaying) push, delivered only to a plugin
    /// that subscribes [`StateKey::NowPlaying`](proto::StateKey::NowPlaying) **and**
    /// declares [`Capability::NowPlaying`](proto::Capability::NowPlaying). Pushed on
    /// change (latest-wins), like [`AudioSpectrum`](Input::AudioSpectrum).
    NowPlaying(NowPlaying),
    /// A datasource query forwarded to this **provider** plugin (#509): the host
    /// [`DatasourceQuery`](proto::HostMsg::DatasourceQuery) push, delivered only to a
    /// plugin that declares [`Capability::DatasourceProvider`](proto::Capability::DatasourceProvider)
    /// and lists `datasource` in [`Manifest::provides`](proto::Manifest::provides).
    /// Answer it by returning an
    /// [`Effect::DatasourceResult`](proto::Effect::DatasourceResult) from
    /// [`update`](Plugin::update), echoing `request_id` **verbatim** (it is an opaque
    /// host correlation the host maps back to the original requester — do not
    /// interpret or reuse it) and carrying the [`DatasourceOutcome`](proto::DatasourceOutcome).
    /// `params` is the requester's opaque JSON request (the provider↔requester
    /// contract); a provider that serves one scope can usually ignore `scope`.
    DatasourceQuery {
        /// The opaque host correlation to echo back in the result. Not the
        /// requester's token — the host translates it on both legs.
        request_id: u64,
        /// Which datasource is being queried (a provider serving several
        /// [`Manifest::provides`](proto::Manifest::provides) entries dispatches on it).
        datasource: String,
        /// The requested scope (a sub-view the provider declared).
        scope: String,
        /// The requester's opaque JSON request payload.
        params: String,
    },
    /// The result of a datasource query this **requester** plugin issued (#509): the
    /// host [`DatasourceResult`](proto::HostMsg::DatasourceResult) push, keyed by the
    /// `request_id` the plugin chose on the originating
    /// [`Effect::DatasourceQuery`](proto::Effect::DatasourceQuery). Delivered only to
    /// a plugin that declared [`Capability::DatasourceQuery`](proto::Capability::DatasourceQuery).
    /// Carries either the provider's answer or a host-synthesized error (no provider
    /// / denied scope / timeout), so a query always eventually resolves — the
    /// datasource mate of [`EffectResult`](Input::EffectResult).
    DatasourceResult {
        /// The `request_id` the plugin chose on the originating query.
        request_id: u64,
        /// The provider's payload, or a host-/provider-sourced failure.
        outcome: DatasourceOutcome,
    },
    /// A message from the plugin's own [`sources`](Plugin::sources) stream.
    App(M),
}

impl<M> Input<M> {
    /// An [`Input::Event`] that names no screen — the constructor a unit test
    /// reaches for.
    ///
    /// [`Input::Event`] is `#[non_exhaustive]`, so a plugin crate cannot write
    /// the struct literal; this and [`event_on`](Input::event_on) are how one
    /// is built from outside `hytte-plugin`. `output` is `None`, which is what
    /// a drawer-panel event carries on the wire and what every plugin saw
    /// before #1050 — so a test written against this constructor is asserting
    /// the *fallback* path. Reach for [`event_on`](Input::event_on) to pin the
    /// per-screen one.
    #[must_use]
    pub fn event(node: impl Into<String>, kind: EventKind) -> Self {
        Self::event_on(node, kind, None)
    }

    /// An [`Input::Event`] attributed to `output` — the monitor whose copy of
    /// the mirrored card produced it (#1050).
    ///
    /// `Some(connector)` names the screen; `None` means *not attributable*,
    /// never "the primary monitor" — see
    /// [`Input::Event::output`](Input::Event::output).
    #[must_use]
    pub fn event_on(node: impl Into<String>, kind: EventKind, output: Option<String>) -> Self {
        Self::Event {
            node: node.into(),
            kind,
            output,
        }
    }
}

/// One projection of the model — everything [`Plugin::view`] renders: the
/// mounted **tree** (a bar chip or a sidebar card, per the manifest's
/// [`Mount`](proto::Mount)) plus the optional drawer **panel** (#349). This is
/// the SDK-side mirror of the wire frame's `Render { tree, panel, .. }` pair —
/// the two always travel together in one frame, so they are produced together
/// by one function.
///
/// A chip/card-only plugin converts its root node with `.into()`; a paneled
/// plugin chains [`panel`](View::panel):
///
/// ```ignore
/// fn view(&self) -> View {
///     chip_node().into()                    // no panel
///     // or:
///     View::new(chip_node()).panel(detail_node())
///     // or, hidden on the screens where there is nothing to show (#1050):
///     View::new(chip_node()).hidden_on(["DP-2"])
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    /// The tree mounted at the manifest's [`Mount`](proto::Mount) region — the
    /// bar chip or sidebar card.
    pub tree: Node,
    /// The optional dedicated drawer page (#349), opened via
    /// [`Effect::OpenPage(Page::PluginSelf)`](proto::Page::PluginSelf).
    /// `None` = no panel; the chip/card is the whole surface.
    pub panel: Option<Node>,
    /// The connector names of the monitors this view's card is **hidden** on
    /// (#1050) — see [`Render.hidden_on`](proto::PluginMsg::Render::hidden_on)
    /// for the full contract. Empty (the default) = shown everywhere, which is
    /// what every plugin did before the field existed.
    ///
    /// One tree is mirrored onto every screen, so this is the *only* way a
    /// plugin can differ per monitor, and it differs in visibility alone. The
    /// shape a per-screen plugin wants is: fold your state into a
    /// `output → verdict` map, then list the outputs whose verdict is "nothing
    /// to show". Names that match no connected monitor are ignored.
    pub hidden_on: Vec<String>,
}

impl View {
    /// A panel-less view of `tree` (equivalent to `tree.into()`), shown on every
    /// monitor.
    #[must_use]
    pub fn new(tree: Node) -> Self {
        Self {
            tree,
            panel: None,
            hidden_on: Vec::new(),
        }
    }

    /// Attach the drawer-panel tree.
    #[must_use]
    pub fn panel(mut self, panel: Node) -> Self {
        self.panel = Some(panel);
        self
    }

    /// Hide this view's card on the named outputs (#1050) — connector names as
    /// niri/Wayland spell them (`"DP-1"`, `"eDP-1"`, `"HDMI-A-2"`), compared
    /// exactly.
    ///
    /// **Replaces** the list rather than appending to it, so a `view()` that
    /// recomputes its verdict every frame — the intended usage — cannot leak a
    /// stale screen into it.
    #[must_use]
    pub fn hidden_on<I, S>(mut self, outputs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.hidden_on = outputs.into_iter().map(Into::into).collect();
        self
    }
}

impl From<Node> for View {
    fn from(tree: Node) -> Self {
        Self::new(tree)
    }
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

    /// Project the model into everything rendered: the mounted widget tree the
    /// host reconciles into GTK, plus the optional drawer panel — one [`View`].
    /// Re-projected after every [`update`](Plugin::update); the runtime dedups
    /// the whole `View` (a panel change alone still forces a frame, so a plugin
    /// can refresh its open panel while its chip stays fixed) and sends it as
    /// one `Render` frame. A chip/card-only plugin returns `node.into()`; see
    /// the crate-level *Opening your own panel* section for the paneled shape.
    fn view(&self) -> View;

    /// Flush pending state before the process exits (#1079): called once,
    /// after the session loop finishes whatever frame was already in flight,
    /// when [`run`] notices `SIGTERM`/`SIGINT` — see the crate-level
    /// *Shutdown* section for the full lifecycle and the grace period this
    /// runs under. The default implementation does nothing, so a plugin with
    /// no durable state to flush needs no change at all; one that persists
    /// something (a grant store, a timer's saved state, a fetch cache) awaits
    /// its own writer here instead of racing the process exit against it.
    ///
    /// Written as `-> impl Future` rather than `async fn` so the signature
    /// carries no implicit `Send`/`Sync` leakage (`clippy`/rustc's
    /// `async_fn_in_trait` lint) — an ordinary `async fn shutdown(&mut self)
    /// { … }` body in an implementation still satisfies this.
    fn shutdown(&mut self) -> impl Future<Output = ()> {
        async {}
    }
}

#[cfg(test)]
mod tests {
    use super::tick_stream;
    use std::time::Duration;
    use tokio_stream::StreamExt as _;

    /// The tick helper leads with an immediate tick, then holds the requested
    /// cadence. Paused virtual time makes the schedule the assertion: the
    /// runtime auto-advances to the next interval fire whenever the task blocks,
    /// so the elapsed virtual clock at each tick pins the period.
    #[tokio::test(start_paused = true)]
    async fn tick_stream_leads_immediately_then_holds_the_cadence() {
        let period = Duration::from_secs(5);
        let mut ticks = std::pin::pin!(tick_stream(period, 7_u8));
        let start = tokio::time::Instant::now();

        assert_eq!(ticks.next().await, Some(7), "the message value is emitted…");
        assert!(start.elapsed() < period, "…and the first tick is immediate");

        assert_eq!(ticks.next().await, Some(7));
        assert!(
            start.elapsed() >= period,
            "the second tick waits one period"
        );

        assert_eq!(ticks.next().await, Some(7));
        assert!(
            start.elapsed() >= period * 2,
            "and it keeps the fixed cadence"
        );
    }
}
