//! `hytte-plugin-proto` — the GTK-free wire protocol for hytte's out-of-process
//! widget plugins ("frontend B"; issues #35 / #195, on the reconciler #199).
//!
//! A plugin is an autonomous process speaking The Elm Architecture across a
//! Unix socket: the host is a **stateless render target**, the plugin holds all
//! its own state. This crate is *only* the wire vocabulary the two exchange —
//! it links **no GTK, no `hytte-ui`, no tokio** (the tokio framing helpers are
//! behind an optional feature), so a plugin author — in any language — depends
//! on the schema, never the shell.
//!
//! # Model
//!
//! - host → plugin: [`HostMsg`] — a full subscribed-state snapshot on change, a
//!   user [`Event`](HostMsg::Event), an [`EffectResult`](HostMsg::EffectResult),
//!   a [`SlotVisibility`](HostMsg::SlotVisibility) push (park pollers while
//!   hidden), liveness, or `Shutdown`.
//! - plugin → host: [`PluginMsg`] — a one-time [`Register`](PluginMsg::Register),
//!   then a [`Render { tree, effects }`](PluginMsg::Render) pushed on the
//!   plugin's own schedule (host state change, timer, external fetch), plus logs
//!   and liveness. The host reconciles `tree` into GTK and brokers `effects`.
//!
//! # Transport & topology
//!
//! The host **listens** on one same-user-only socket,
//! `$XDG_RUNTIME_DIR/trollshell/plugin.sock` (construct it with
//! [`socket_path`] — both ends share the one definition); plugins are systemd
//! user units that **dial in** and self-identify with
//! [`Register`](PluginMsg::Register).
//! A crash is just a disconnect (`Restart=on-failure` reconnects). "Enable a
//! plugin" = enable its unit. Encoding is `MessagePack` (`rmp-serde`) in
//! length-prefixed frames — see [`codec`].
//!
//! # Schema-drift mitigations
//!
//! Schema skew across independently-built plugins is the standing risk, so the
//! guards are baked in at birth:
//!
//! - **[`PROTO_VERSION`] is exact-matched on `Register`** ([`Manifest::check_proto`]):
//!   a plugin on a different proto is rejected at the handshake, not
//!   best-effort decoded.
//! - **Named-field encoding is pinned** — bodies are always
//!   `rmp_serde::to_vec_named` (a map keyed by field name), never positional
//!   arrays. Unknown fields are then skipped on decode, which is what makes
//!   additive evolution safe.
//! - **The serde enum representation is pinned to the default external
//!   tagging** (a single-key map, `{ "Variant": … }`, keyed by the variant
//!   *name*). Do not switch to internal/adjacent/untagged: appending a variant
//!   is only invisible to older code paths because tagging is name-based, not
//!   positional.
//!
//! ## Compat rules
//!
//! What keeps the **same** [`PROTO_VERSION`]:
//!
//! - Adding an **optional field** to a struct (carry `#[serde(default)]`, and
//!   `#[serde(skip_serializing_if = …)]` where it should stay off the wire).
//! - Appending a **new enum variant** (name-tagged, so existing variants keep
//!   their meaning) — but this also **bumps [`VOCAB`]** (see the vocab-counter
//!   section below): a new wire variant grows the vocabulary, and the counter is
//!   how a host detects a plugin that renders one it can't decode.
//!
//! What **requires a [`PROTO_VERSION`] bump**:
//!
//! - **Renaming** a field or variant, **removing** one, or **changing a field's
//!   type / meaning** — anything that changes what an existing name decodes to.
//! - **Reordering** a tuple variant's elements (positional, so order is
//!   meaning).
//!
//! ### A new host→plugin push must be **opt-in**, never unconditional (#305)
//!
//! "Appending a name-tagged variant is additive" only holds while old code never
//! *receives* the new variant. A host that pushes a freshly-added
//! [`HostMsg`](msg::HostMsg) variant to **every** connection breaks exactly that:
//! a plugin built against the older proto can't decode the unknown variant, its
//! session dies, and (with an SDK that redials) it crash-loops — and the
//! [`PROTO_VERSION`] exact-match can't catch it, since both sides are the same
//! version. So a new push must be **gated on an opt-in the plugin declares in its
//! [`Manifest`](manifest::Manifest)** — a [`StateKey`](manifest::StateKey)
//! subscription or a [`Capability`](manifest::Capability). The host serializes
//! only what a connection subscribed to, so an old binary that never declared the
//! opt-in never receives the variable it can't decode. (Visibility, #288, was
//! retrofitted onto [`StateKey::SlotVisible`](manifest::StateKey::SlotVisible)
//! for this reason.)
//!
//! ### The wire-vocabulary counter must be bumped on every appended variant (#437)
//!
//! The #305 rule guards **host→plugin** pushes. The mirror hazard is
//! **plugin→host**: a plugin built against a *newer* proto (same
//! [`PROTO_VERSION`], one appended [`Node`](wire::Node) / [`EventKind`](wire::EventKind) /
//! [`Effect`](effect::Effect) variant) can *render* that variant in its
//! [`Render`](msg::PluginMsg::Render) tree. An older host's `rmp-serde` then fails
//! to decode the frame, the host treats it as an ordinary disconnect, the SDK
//! redials and re-sends the identical tree — a permanent, near-silent 5 s
//! crash-loop the [`PROTO_VERSION`] exact-match can't catch (both sides are the
//! same version).
//!
//! [`VOCAB`] closes that gap. It is a **monotonic generation counter of the wire
//! vocabulary**, independent of [`PROTO_VERSION`]. Every plugin stamps its
//! build-time [`VOCAB`] into its [`Manifest`](manifest::Manifest) automatically —
//! [`Manifest::new`](manifest::Manifest::new) does it, like `proto` — and the host
//! rejects a `Register` whose `vocab` exceeds its own at the handshake
//! ([`Manifest::check_vocab`](manifest::Manifest::check_vocab)), turning the silent
//! crash-loop into one loud, self-explanatory rejection. An older plugin that
//! predates the field decodes to `vocab = 0` (`#[serde(default)]`) and always
//! passes.
//!
//! **The rule: appending a wire variant ⇒ bump [`VOCAB`].** Any new
//! [`Node`](wire::Node), [`EventKind`](wire::EventKind), [`Effect`](effect::Effect),
//! [`StateKey`](manifest::StateKey), or [`HostMsg`](msg::HostMsg) variant a peer
//! can put on the wire grows the vocabulary the other side must understand;
//! bumping [`VOCAB`] is what lets an older host detect (and cleanly refuse) a
//! plugin that speaks the newer one. (A purely host→plugin addition — a new
//! `HostMsg` push — is already covered by the #305 opt-in, but bumping [`VOCAB`]
//! for it too keeps the counter a faithful census of the whole vocabulary and
//! costs nothing.)

pub mod codec;
pub mod effect;
pub mod manifest;
pub mod msg;
pub mod state;
pub mod topology;
pub mod wire;

/// The wire protocol version. Exact-matched on [`Register`](msg::PluginMsg::Register)
/// (see [`Manifest::check_proto`]); bump it per the crate-level compat rules.
pub const PROTO_VERSION: u16 = 1;

/// The **wire-vocabulary generation** — a monotonic counter of how many times the
/// on-the-wire vocabulary has grown, independent of [`PROTO_VERSION`] (#437).
///
/// Every plugin stamps this into its [`Manifest`](manifest::Manifest) at build
/// time (automatically, via [`Manifest::new`](manifest::Manifest::new)); the host
/// refuses a `Register` whose `vocab` exceeds its own
/// ([`Manifest::check_vocab`](manifest::Manifest::check_vocab)), so a plugin built
/// against a newer vocabulary — one that can render a [`Node`](wire::Node) /
/// [`Effect`](effect::Effect) variant this host can't decode — fails loud at the
/// handshake instead of silently crash-looping (see the crate root's
/// wire-vocabulary section).
///
/// **Bump this by 1 whenever you append a wire variant** — a [`Node`](wire::Node),
/// [`EventKind`](wire::EventKind), [`Effect`](effect::Effect),
/// [`StateKey`](manifest::StateKey), or [`HostMsg`](msg::HostMsg) case. The counter
/// starts at `1` (this PR); generation `0` is reserved for an older, pre-`vocab`
/// manifest, which decodes to `0` (`#[serde(default)]`) and so always clears a
/// host's check — a pre-counter plugin is treated as the oldest generation.
pub const VOCAB: u16 = 1;

pub use codec::{MAX_FRAME_LEN, ProtoError, decode, decode_body, encode, encode_body};
pub use effect::{
    AudioAction, ConsentDecision, DatasourceError, DatasourceOutcome, Effect, EffectOutcome,
    MediaAction, NiriAction, Page,
};
pub use manifest::{Capability, Manifest, Mount, ProvidedDatasource, StateKey};
pub use msg::{HostMsg, LogLevel, PluginMsg};
pub use state::{
    AudioSpectrum, ClockState, MAX_UPCOMING_EVENTS, NowPlaying, SPECTRUM_BINS, StateSnapshot,
    UpcomingEvent,
};
pub use topology::{SOCKET_DIR, SOCKET_FILE, socket_path};
pub use wire::{Cls, Dir, EventKind, Node, NodeId};

#[cfg(feature = "tokio")]
pub use codec::{read_frame, write_frame};
