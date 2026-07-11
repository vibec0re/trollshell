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
//!   liveness, or `Shutdown`.
//! - plugin → host: [`PluginMsg`] — a one-time [`Register`](PluginMsg::Register),
//!   then a [`Render { tree, effects }`](PluginMsg::Render) pushed on the
//!   plugin's own schedule (host state change, timer, external fetch), plus logs
//!   and liveness. The host reconciles `tree` into GTK and brokers `effects`.
//!
//! # Transport & topology
//!
//! The host **listens** on one same-user-only socket,
//! `$XDG_RUNTIME_DIR/trollshell/plugin.sock`; plugins are systemd user units
//! that **dial in** and self-identify with [`Register`](PluginMsg::Register).
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
//!   their meaning).
//!
//! What **requires a [`PROTO_VERSION`] bump**:
//!
//! - **Renaming** a field or variant, **removing** one, or **changing a field's
//!   type / meaning** — anything that changes what an existing name decodes to.
//! - **Reordering** a tuple variant's elements (positional, so order is
//!   meaning).

pub mod codec;
pub mod effect;
pub mod manifest;
pub mod msg;
pub mod state;
pub mod wire;

/// The wire protocol version. Exact-matched on [`Register`](msg::PluginMsg::Register)
/// (see [`Manifest::check_proto`]); bump it per the crate-level compat rules.
pub const PROTO_VERSION: u16 = 1;

pub use codec::{MAX_FRAME_LEN, ProtoError, decode, decode_body, encode, encode_body};
pub use effect::{AudioAction, Effect, EffectOutcome, MediaAction, NiriAction, Page};
pub use manifest::{Capability, Manifest, Mount, StateKey};
pub use msg::{HostMsg, LogLevel, PluginMsg};
pub use state::{ClockState, StateSnapshot};
pub use wire::{Cls, Dir, EventKind, Node, NodeId};

#[cfg(feature = "tokio")]
pub use codec::{read_frame, write_frame};
