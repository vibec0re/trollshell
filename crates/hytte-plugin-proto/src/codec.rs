//! Length-prefixed `MessagePack` framing and the protocol error type.
//!
//! One frame on the wire:
//!
//! ```text
//! ┌────────────────────┬──────────────────────────────┐
//! │ u32 body length, BE │ MessagePack body (that many B) │
//! └────────────────────┴──────────────────────────────┘
//! ```
//!
//! The 4-byte big-endian prefix is the body length in bytes; the body is the
//! `rmp_serde::to_vec_named` (named-field map) encoding of a
//! [`PluginMsg`](crate::msg::PluginMsg) / [`HostMsg`](crate::msg::HostMsg) — or
//! any `Serialize` type. [`encode`] emits a whole frame; [`decode`] parses
//! exactly one. For streaming I/O, the optional `tokio` feature adds
//! `read_frame` / `write_frame`.
//!
//! # Bounded before decoding
//!
//! [`MAX_FRAME_LEN`] bounds a body's bytes. Before a body is deserialised,
//! [`decode_body`] (so [`decode`] and `read_frame` too, in both directions)
//! also walks it once by its `MessagePack` markers alone. The walk refuses a
//! body that is malformed, nests deeper than [`MAX_FRAME_DEPTH`], or holds
//! more than [`MAX_FRAME_VALUES`] values. It allocates nothing and is a loop,
//! not a recursion, so a body's decoded size is bounded before any of it is
//! decoded. [`MAX_FRAME_VALUES`] and [`MAX_FRAME_DEPTH`] bound what decoding a
//! frame costs. The tree caps in [`wire`](crate::wire) bound what the host
//! builds from it afterwards.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Width of the length prefix, in bytes.
pub const LEN_PREFIX: usize = 4;

/// Hard cap on a single frame's body, to bound memory against a hostile or
/// buggy peer that declares a huge length. 16 MiB is far above any real view
/// tree; enforced only on the **read** side ([`decode`] / `read_frame`).
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// How many `MessagePack` values one frame body may hold. [`decode_body`]
/// counts them before it deserialises anything.
///
/// # Why there is a second cap
///
/// [`MAX_FRAME_LEN`] bounds a frame's bytes. That does not bound what decoding
/// the frame costs, because a value's decoded size is unrelated to its encoded
/// size. A [`Node`](crate::wire::Node) is 144 B in memory, and a
/// [`Node::Spacer`](crate::wire::Node::Spacer) is **one byte** on the wire:
/// `rmp-serde` decodes a unit variant from its integer index as well as from
/// its name, and `0x0d` is `Spacer`'s index.
///
/// Measured on a debug build before this cap existed:
///
/// - One 16 MiB `Render` of index-encoded spacers decoded to 16.7 M nodes and
///   **2.3 GiB** resident.
/// - One of name-encoded spacers (7 B each) decoded to 2.4 M nodes and 330 MiB.
///
/// Both happened before the host's tree caps
/// ([`MAX_NODES_PER_TREE`](crate::wire::MAX_NODES_PER_TREE) and the rest) ran,
/// because those caps inspect the decoded value. A plugin could send that frame
/// in a loop. With this cap, both frames are refused at their `children`
/// header, before a single node exists.
///
/// # Why a count and not a smaller byte cap
///
/// The legitimately large frames are large because of single values: a
/// [`Node::Shader`](crate::wire::Node::Shader)'s data buffer (up to
/// [`MAX_SHADER_DATA_BYTES`](crate::wire::MAX_SHADER_DATA_BYTES)), or a
/// [`Node::Pixels`](crate::wire::Node::Pixels) RGBA buffer. Each of those is
/// one `bin`. A flood is large because it holds many values. A byte cap
/// cannot tell the two apart, and a count can: a `bin`, `str` or `ext` counts
/// once, however long it is.
///
/// # The derivation
///
/// The count rule:
///
/// - A scalar, `str`, `bin` or `ext` is one value.
/// - An array is one value plus its elements.
/// - A map is one value plus two per entry (the key and the value).
///
/// Under that rule, these are the widest legal nodes. Each has every optional
/// field set and [`MAX_NODE_CLASSES`](crate::wire::MAX_NODE_CLASSES) classes,
/// and this module's tests pin both numbers:
///
/// - **89 values:** a [`Node::Preem`](crate::wire::Node::Preem) gauge.
/// - **57 values:** a [`Node::Shader`](crate::wire::Node::Shader), the widest
///   node of any other kind.
///
/// One tree holds at most
/// [`MAX_NODES_PER_TREE`](crate::wire::MAX_NODES_PER_TREE) nodes, of which at
/// most [`MAX_PREEM_NODES_PER_TREE`](crate::wire::MAX_PREEM_NODES_PER_TREE)
/// are preem. That makes 64 × 89 + 4032 × 57 = 235,520 values per tree. A
/// `Render` carries two trees, `tree` and `panel`, and the host caps each one
/// separately, so two trees come to 471,040. The envelope adds a few hundred
/// more: `hidden_on` at the host's 64 entries and a burst of effects.
///
/// **2¹⁹ = 524,288** is the next power of two. It is 11 % above the widest
/// legal `Render`, which is itself an extreme: the largest tree a bundled
/// plugin renders is in the low hundreds of nodes, with one to three classes
/// each.
///
/// # What it does not admit
///
/// The derivation counts no samples, and samples are values too. The
/// per-node sample caps multiplied by the node caps come to more samples than
/// a 16 MiB frame can carry. For example, 4096
/// [`Node::Sparkline`](crate::wire::Node::Sparkline)s of
/// [`MAX_SPARKLINE_SAMPLES`](crate::wire::MAX_SPARKLINE_SAMPLES) each is 4 M
/// samples, 20 MiB as the `f32`s the SDK encodes. So a value budget cannot
/// bound memory and also admit every frame that passes every per-node cap.
///
/// This budget therefore also acts as a per-frame sample cap. A `Render` with
/// a realistic structure has about half a million values left for samples:
/// seven full [`Node::MultiSparkline`](crate::wire::Node::MultiSparkline)s,
/// or a hundred full scopes.
///
/// # The worst case it leaves
///
/// - **Values.** Every value can become at most one 144 B node, so a frame
///   within this cap decodes to at most 2¹⁹ × 144 B = **72 MiB** of nodes.
///   Measured: an index-encoded spacer frame one envelope short of the cap
///   decoded to 73 MiB resident.
/// - **One `bin` as samples.** `rmp-serde` hands a `bin` to a target that asks
///   for a sequence as a sequence of its bytes. One `bin` can therefore fill a
///   sample list ([`Node::Sparkline`](crate::wire::Node::Sparkline)'s `values`
///   and the other `Vec<f32>` fields) at 4 B per byte. This cap counts that
///   `bin` once, so it is [`MAX_FRAME_LEN`] that bounds this route: **64 MiB**
///   (65 MiB measured for a whole-frame `bin`). That is a 4× amplification,
///   where the flood above was 144×.
/// - **Both together.** The two routes share one 16 MiB frame, so a frame can
///   reach about 134 MiB at most, compared with 2.3 GiB before this cap.
///
/// # A refusal drops the frame, not part of it
///
/// A body over the cap is refused whole with
/// [`ProtoError::TooManyValues`]. It is never decoded in part, because serde
/// has no partial decode. The host's tree caps keep a prefix of the tree;
/// this cap refuses the frame the way [`MAX_FRAME_LEN`] does, and a peer sees
/// the same disconnect either way. Like [`MAX_FRAME_LEN`], it is enforced on
/// the **read** side only.
pub const MAX_FRAME_VALUES: usize = 1 << 19;

/// How deeply one frame body may nest `MessagePack` containers (arrays and
/// maps; a top-level map is level 1). [`decode_body`] checks this in the same
/// walk as [`MAX_FRAME_VALUES`], before it deserialises anything.
///
/// # Why the decoder's own limit is not enough
///
/// `rmp-serde` stops at 1024 levels on its own, and its decoder recurses once
/// per level. It does not count the single-entry map that wraps an enum
/// variant, though, so a [`Node::Button`](crate::wire::Node::Button) costs it
/// one level where the wire has two. Its limit therefore admits a chain of
/// about 1020 buttons, which is 2040 `MessagePack` levels. Measured on a debug
/// build:
///
/// - That chain decodes on an 8 MiB stack, which is what the host's runtime
///   threads and a plugin's main thread have.
/// - On a 2 MiB stack, tokio's default for a worker thread, a chain of 320
///   buttons **aborts the process** with a stack overflow. A chain of 256 fits.
///
/// So `rmp-serde`'s limit is safe only on the stacks it happens to run on
/// today. This cap allows at most 126 buttons, about half the chain that fit
/// in 2 MiB.
///
/// # It is not a tighter tree cap
///
/// A tree at the host's own
/// [`MAX_TREE_DEPTH`](crate::wire::MAX_TREE_DEPTH) is 198 levels deep in
/// `MessagePack`, and this module's tests pin that number. It breaks down as:
///
/// - the `Render` envelope's two levels;
/// - 63 containers of three levels each (the variant's map, its fields' map
///   and its `children` array);
/// - the seven levels of the deepest leaf, a preem gauge.
///
/// What this cap does change is chains past 84 `Box` levels or 126 `Button`
/// levels. The host used to map 64 levels of such a chain and drop the rest.
/// That frame is now refused, as one past [`MAX_FRAME_VALUES`] is.
pub const MAX_FRAME_DEPTH: usize = 256;

/// Everything that can go wrong framing/decoding a message.
#[derive(Debug)]
pub enum ProtoError {
    /// The body was not valid `MessagePack` for the target type.
    Decode(rmp_serde::decode::Error),
    /// A declared body length exceeds [`MAX_FRAME_LEN`].
    FrameTooLarge {
        /// The over-large declared length.
        len: usize,
    },
    /// A body holds more `MessagePack` values than [`MAX_FRAME_VALUES`]. This
    /// is the [`FrameTooLarge`](Self::FrameTooLarge) for a frame whose bytes
    /// fit but whose decoded size would not. It is raised before anything is
    /// deserialised, so nothing was decoded.
    ///
    /// A body that is malformed, or nests deeper than [`MAX_FRAME_DEPTH`], is a
    /// [`Decode`](Self::Decode) error instead, carrying `rmp-serde`'s own
    /// error kinds.
    TooManyValues {
        /// A lower bound on the body's value count. The walk stops at the
        /// first container header that takes it past the cap, and this
        /// includes that header's declared elements.
        at_least: usize,
    },
    /// The buffer was shorter (or longer) than the declared frame.
    FrameTruncated {
        /// Bytes the declared frame needs (`LEN_PREFIX + body`).
        need: usize,
        /// Bytes actually present.
        got: usize,
    },
    /// A `Register` manifest's `proto` didn't equal [`PROTO_VERSION`](crate::PROTO_VERSION).
    ProtoMismatch {
        /// This build's [`PROTO_VERSION`](crate::PROTO_VERSION).
        ours: u16,
        /// The peer's declared proto.
        theirs: u16,
    },
    /// A `Register` manifest's `vocab` was **newer** than this host's
    /// [`VOCAB`](crate::VOCAB) (#437): the plugin was built against a newer wire
    /// vocabulary and can render a variant this host can't decode, so it is
    /// refused at the handshake (see [`Manifest::check_vocab`](crate::Manifest::check_vocab)).
    VocabTooNew {
        /// This build's [`VOCAB`](crate::VOCAB).
        ours: u16,
        /// The peer's declared (newer) vocab.
        theirs: u16,
    },
    /// Underlying I/O error (only from the `tokio` framed helpers).
    #[cfg(feature = "tokio")]
    Io(std::io::Error),
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "MessagePack decode failed: {e}"),
            Self::FrameTooLarge { len } => {
                write!(
                    f,
                    "frame body {len} B exceeds MAX_FRAME_LEN {MAX_FRAME_LEN} B"
                )
            }
            Self::TooManyValues { at_least } => {
                write!(
                    f,
                    "frame body holds at least {at_least} MessagePack values, over \
                     MAX_FRAME_VALUES {MAX_FRAME_VALUES}; refused before decoding"
                )
            }
            Self::FrameTruncated { need, got } => {
                write!(f, "truncated frame: need {need} B, got {got} B")
            }
            Self::ProtoMismatch { ours, theirs } => {
                write!(f, "plugin proto {theirs} != host proto {ours}")
            }
            Self::VocabTooNew { ours, theirs } => {
                write!(
                    f,
                    "plugin wire vocabulary {theirs} is newer than host {ours} — \
                     the plugin was built against a newer wire vocabulary; update the shell"
                )
            }
            #[cfg(feature = "tokio")]
            Self::Io(e) => write!(f, "frame I/O failed: {e}"),
        }
    }
}

impl std::error::Error for ProtoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(e) => Some(e),
            #[cfg(feature = "tokio")]
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Serialize `msg` to a `MessagePack` body (named-field map mode, **no** length
/// prefix). Infallible for the closed wire types — see [`encode`].
#[must_use]
pub fn encode_body<T: Serialize>(msg: &T) -> Vec<u8> {
    // `to_vec_named` pins the named-field map representation (never positional
    // arrays), which is what makes unknown-field skipping — hence additive
    // schema evolution — work. Serializing plain data into an in-memory `Vec`
    // cannot fail for the wire types.
    rmp_serde::to_vec_named(msg).expect("MessagePack serialization of a wire message is infallible")
}

/// Encode `msg` into a whole length-prefixed frame.
///
/// Infallible: serialization of the closed wire types cannot fail, and the
/// [`MAX_FRAME_LEN`] cap is a read-side defense (a locally-built frame is
/// trusted). `decode(encode(&m)) == Ok(m)` round-trips.
#[must_use]
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = encode_body(msg);
    let mut frame = Vec::with_capacity(LEN_PREFIX + body.len());
    let len = u32::try_from(body.len()).expect("wire message body fits in u32");
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

/// Decode a `MessagePack` body (no length prefix) into `T`.
///
/// The body is walked first, without decoding it, and refused before serde
/// runs if it holds more than [`MAX_FRAME_VALUES`] values
/// ([`ProtoError::TooManyValues`]). It is also refused if it nests deeper than
/// [`MAX_FRAME_DEPTH`] or is malformed `MessagePack`: a header claiming more
/// elements than there are bytes left, a body that ends early, or the reserved
/// `0xc1` marker. Those are [`ProtoError::Decode`] errors carrying
/// `rmp-serde`'s own error kinds (an early end of input, a length mismatch, a
/// type mismatch, the depth limit). The walk reads exactly the one value serde
/// reads, so trailing bytes are ignored as before.
pub fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, ProtoError> {
    walk(body)?;
    rmp_serde::from_slice(body).map_err(ProtoError::Decode)
}

/// What [`walk`] measured of a body it accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
    /// `MessagePack` values in the body's top-level value, that value included.
    values: usize,
    /// The deepest container nesting reached; a top-level array or map is 1.
    depth: usize,
}

/// Walk the one `MessagePack` value at the head of `body` by its markers
/// alone, without decoding it. This is the pre-pass [`decode_body`] runs, and
/// it refuses the body if:
///
/// - it is malformed;
/// - it nests deeper than [`MAX_FRAME_DEPTH`]; or
/// - it holds more than [`MAX_FRAME_VALUES`] values.
///
/// Every marker is one value, so a container is one value plus its elements
/// (two per map entry). A `str`, `bin` or `ext` is one value, and its bytes
/// are skipped unread.
///
/// The walk is a loop over a fixed-size stack, so it allocates nothing and
/// cannot overflow. It checks every container header against two lower
/// bounds before stepping into it:
///
/// - Every value still owed is at least one more byte. A header that promises
///   more values than there are bytes left is refused at once, so a lying
///   `u32::MAX`-element header costs nothing to reject.
/// - Every value still owed adds at least one to the count. A frame that
///   cannot fit the budget is refused at the header that proves it, before
///   any of the elements that header introduces are read.
///
/// A body this accepts can still fail to decode, for example on a wrong type
/// or bad UTF-8. That is serde's to report.
fn walk(body: &[u8]) -> Result<Shape, ProtoError> {
    use rmp::Marker;
    use rmp_serde::decode::Error;

    // `owed_at[d]` holds what the container at nesting level `d + 1` still
    // owes while a deeper one is open.
    let mut owed_at = [0_usize; MAX_FRAME_DEPTH];
    let mut depth = 0;
    let mut deepest = 0;
    // Values still owed at the current level: the top level owes exactly one.
    let mut owed = 1_usize;
    let mut read = 0_usize;
    // `read` plus every value an open container still owes. It never
    // decreases, and it equals the body's count once nothing is owed, so it
    // is a lower bound on that count throughout.
    let mut promised = 1_usize;
    let mut pos = 0_usize;
    loop {
        while owed == 0 {
            if depth == 0 {
                return Ok(Shape {
                    values: read,
                    depth: deepest,
                });
            }
            depth -= 1;
            owed = owed_at[depth];
        }
        owed -= 1;
        read += 1;
        let byte = *body
            .get(pos)
            .ok_or_else(|| ProtoError::Decode(Error::InvalidMarkerRead(eof())))?;
        pos += 1;
        // The bytes to skip after the marker (and any length), and for a
        // container its declared length and the values per element.
        let (payload, header) = match Marker::from_u8(byte) {
            Marker::FixPos(_) | Marker::FixNeg(_) | Marker::Null | Marker::True | Marker::False => {
                (0, None)
            }
            // The fixed-size payloads. An ext is a type byte and then its
            // data, so a fixext's payload is one more than its data.
            Marker::U8 | Marker::I8 => (1, None),
            Marker::U16 | Marker::I16 | Marker::FixExt1 => (2, None),
            Marker::FixExt2 => (3, None),
            Marker::U32 | Marker::I32 | Marker::F32 => (4, None),
            Marker::FixExt4 => (5, None),
            Marker::U64 | Marker::I64 | Marker::F64 => (8, None),
            Marker::FixExt8 => (9, None),
            Marker::FixExt16 => (17, None),
            Marker::FixStr(len) => (usize::from(len), None),
            Marker::Str8 | Marker::Bin8 => (read_len(body, &mut pos, 1)?, None),
            Marker::Str16 | Marker::Bin16 => (read_len(body, &mut pos, 2)?, None),
            Marker::Str32 | Marker::Bin32 => (read_len(body, &mut pos, 4)?, None),
            Marker::Ext8 => (read_len(body, &mut pos, 1)?.saturating_add(1), None),
            Marker::Ext16 => (read_len(body, &mut pos, 2)?.saturating_add(1), None),
            Marker::Ext32 => (read_len(body, &mut pos, 4)?.saturating_add(1), None),
            Marker::FixArray(len) => (0, Some((usize::from(len), 1))),
            Marker::Array16 => (0, Some((read_len(body, &mut pos, 2)?, 1))),
            Marker::Array32 => (0, Some((read_len(body, &mut pos, 4)?, 1))),
            // A map entry is two values, its key and its value.
            Marker::FixMap(len) => (0, Some((usize::from(len), 2))),
            Marker::Map16 => (0, Some((read_len(body, &mut pos, 2)?, 2))),
            Marker::Map32 => (0, Some((read_len(body, &mut pos, 4)?, 2))),
            Marker::Reserved => {
                return Err(ProtoError::Decode(Error::TypeMismatch(Marker::Reserved)));
            }
        };
        pos = pos
            .checked_add(payload)
            .filter(|&end| end <= body.len())
            .ok_or_else(|| ProtoError::Decode(Error::InvalidDataRead(eof())))?;
        let Some((declared, per_element)) = header else {
            continue;
        };
        if depth == MAX_FRAME_DEPTH {
            return Err(ProtoError::Decode(Error::DepthLimitExceeded));
        }
        deepest = deepest.max(depth + 1);
        let items = declared.saturating_mul(per_element);
        promised = promised.saturating_add(items);
        if promised - read > body.len() - pos {
            return Err(ProtoError::Decode(Error::LengthMismatch(
                u32::try_from(declared).unwrap_or(u32::MAX),
            )));
        }
        if promised > MAX_FRAME_VALUES {
            return Err(ProtoError::TooManyValues { at_least: promised });
        }
        if items > 0 {
            owed_at[depth] = owed;
            depth += 1;
            owed = items;
        }
    }
}

/// Read a `width`-byte big-endian length at `*pos`, advancing past it.
fn read_len(body: &[u8], pos: &mut usize, width: usize) -> Result<usize, ProtoError> {
    let bytes = pos
        .checked_add(width)
        .and_then(|end| body.get(*pos..end))
        .ok_or_else(|| ProtoError::Decode(rmp_serde::decode::Error::InvalidDataRead(eof())))?;
    *pos += width;
    let len = bytes
        .iter()
        .fold(0_u64, |acc, &b| (acc << 8) | u64::from(b));
    // Every length is at most 32 bits wide. A target whose `usize` is narrower
    // saturates, and the bounds checks after the read then refuse it.
    Ok(usize::try_from(len).unwrap_or(usize::MAX))
}

/// The I/O error `rmp-serde` itself reports when a body ends early.
fn eof() -> std::io::Error {
    std::io::ErrorKind::UnexpectedEof.into()
}

/// Decode exactly one whole length-prefixed frame into `T`.
///
/// Strict: `frame` must be exactly the prefix plus the declared body (use
/// `read_frame` for streaming, where boundaries are handled for you). A short
/// or over-long buffer, or a declared length past [`MAX_FRAME_LEN`], is an
/// error rather than a partial/oversized decode.
pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, ProtoError> {
    if frame.len() < LEN_PREFIX {
        return Err(ProtoError::FrameTruncated {
            need: LEN_PREFIX,
            got: frame.len(),
        });
    }
    let mut prefix = [0u8; LEN_PREFIX];
    prefix.copy_from_slice(&frame[..LEN_PREFIX]);
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME_LEN {
        return Err(ProtoError::FrameTooLarge { len });
    }
    let body = &frame[LEN_PREFIX..];
    if body.len() != len {
        return Err(ProtoError::FrameTruncated {
            need: LEN_PREFIX + len,
            got: frame.len(),
        });
    }
    decode_body(body)
}

#[cfg(feature = "tokio")]
mod async_io {
    use super::{LEN_PREFIX, MAX_FRAME_LEN, ProtoError, decode_body, encode};
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    /// Read one length-prefixed frame from `reader` and decode it into `T`.
    ///
    /// Reads the 4-byte prefix, then exactly that many body bytes, then
    /// decodes. A closed/EOF stream surfaces as [`ProtoError::Io`] (that's how
    /// a plugin disconnect appears). Cancel-safe only at frame boundaries.
    pub async fn read_frame<T, R>(reader: &mut R) -> Result<T, ProtoError>
    where
        T: DeserializeOwned,
        R: AsyncRead + Unpin,
    {
        let mut prefix = [0u8; LEN_PREFIX];
        reader
            .read_exact(&mut prefix)
            .await
            .map_err(ProtoError::Io)?;
        let len = u32::from_be_bytes(prefix) as usize;
        if len > MAX_FRAME_LEN {
            return Err(ProtoError::FrameTooLarge { len });
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).await.map_err(ProtoError::Io)?;
        decode_body(&body)
    }

    /// Encode `msg` and write its whole frame to `writer`, then flush.
    pub async fn write_frame<T, W>(writer: &mut W, msg: &T) -> Result<(), ProtoError>
    where
        T: Serialize,
        W: AsyncWrite + Unpin,
    {
        let frame = encode(msg);
        writer.write_all(&frame).await.map_err(ProtoError::Io)?;
        writer.flush().await.map_err(ProtoError::Io)?;
        Ok(())
    }
}

#[cfg(feature = "tokio")]
pub use async_io::{read_frame, write_frame};

#[cfg(test)]
mod tests {
    //! The pre-decode walk ([`walk`]) against the vocabulary it guards: what a
    //! legal frame costs it, and what it refuses.

    use super::{
        LEN_PREFIX, MAX_FRAME_DEPTH, MAX_FRAME_LEN, MAX_FRAME_VALUES, ProtoError, Shape, decode,
        decode_body, encode, encode_body, walk,
    };
    use crate::effect::Effect;
    use crate::msg::PluginMsg;
    use crate::preem::{
        AccentRole, DEFAULT_DOT_PX, DotMatrixConfig, DotMatrixState, FlipBoardConfig,
        FlipBoardState, GaugeConfig, GaugeRange, GaugeState, LedStripConfig, LedStripState,
        MarqueeConfig, MarqueeState, Mechanism, PeakHoldConfig, PreemWidget, ScopeConfig,
        ScopeState, SevenSegConfig, SevenSegState, StyleName, StyleRef, TextBoxConfig,
        TextBoxState, TextBoxWidth,
    };
    use crate::wire::{
        Dir, MAX_NODE_CLASSES, MAX_NODES_PER_TREE, MAX_PREEM_NODES_PER_TREE, MAX_SHADER_DATA_BYTES,
        MAX_TREE_DEPTH, Node, ShaderData,
    };
    use serde::Serialize;

    fn shape(msg: &impl Serialize) -> Shape {
        walk(&encode_body(msg)).expect("a locally-encoded body walks")
    }

    fn classes() -> Vec<String> {
        vec!["c".to_owned(); MAX_NODE_CLASSES]
    }

    fn style() -> StyleRef {
        StyleRef {
            style: StyleName::Crt,
            accent: Some(AccentRole::Warning),
            ink: Some([1, 2, 3, 4]),
            field: Some([5, 6, 7, 8]),
        }
    }

    /// Every [`PreemWidget`] kind with every optional field set and every
    /// default-elided field off its default, so each one is on the wire.
    fn widest_preem() -> Vec<PreemWidget> {
        vec![
            PreemWidget::DotMatrix {
                config: DotMatrixConfig {
                    style: style(),
                    dot_px: DEFAULT_DOT_PX + 1,
                },
                state: DotMatrixState { text: "t".into() },
            },
            PreemWidget::SevenSeg {
                config: SevenSegConfig { style: style() },
                state: SevenSegState { text: "t".into() },
            },
            PreemWidget::TextBox {
                config: TextBoxConfig {
                    style: style(),
                    width: TextBoxWidth::Cols(9),
                    max_lines: 2,
                    pad: 3,
                    corner: 4,
                    scale: 2,
                    fixed_width: true,
                    notdef: Some([9, 9, 9, 9]),
                },
                state: TextBoxState { text: "t".into() },
            },
            PreemWidget::LedStrip {
                config: LedStripConfig {
                    style: style(),
                    leds: 12,
                    peak_hold: Some(PeakHoldConfig { rate: 0.5 }),
                },
                state: LedStripState {
                    level: 0.5,
                    peak: Some(0.75),
                },
            },
            PreemWidget::Marquee {
                config: MarqueeConfig {
                    style: style(),
                    window_px: 64,
                    gap_dots: 3,
                    dot_px: DEFAULT_DOT_PX + 1,
                    speed_dots_per_sec: 10.0,
                },
                state: MarqueeState { text: "t".into() },
            },
            PreemWidget::Scope {
                config: ScopeConfig {
                    style: style(),
                    cols: 10,
                    rows: 10,
                    scale: 2,
                    persistence: 9,
                },
                state: ScopeState {
                    samples: Vec::new(),
                },
            },
            PreemWidget::Gauge {
                config: GaugeConfig {
                    style: style(),
                    cols: 10,
                    rows: 10,
                    scale: 2,
                    sweep_deg: 90.0,
                    divisions: 4,
                    subdivisions: 2,
                    range: GaugeRange {
                        low: 0.0,
                        high: 2.0,
                    },
                    frequency_hz: 3.0,
                    damping: 0.7,
                },
                state: GaugeState { target: 0.5 },
            },
            PreemWidget::FlipBoard {
                config: FlipBoardConfig {
                    style: style(),
                    mechanism: Mechanism::Nixie,
                    cells: 4,
                    glyph_px: 8,
                    scale: 2,
                    duration_secs: Some(0.5),
                    stagger_secs: Some(0.1),
                },
                state: FlipBoardState { text: "t".into() },
            },
        ]
    }

    fn spacer() -> Box<Node> {
        Box::new(Node::Spacer)
    }

    /// Every [`Node`] kind at its widest, with no children and no samples:
    /// every optional field set and `classes` at [`MAX_NODE_CLASSES`]. A
    /// mandatory child is a [`Node::Spacer`], one value, which
    /// [`own_values`] subtracts. One [`Node::Preem`] per [`PreemWidget`] kind.
    /// The first is a [`Node::Box`].
    fn widest_nodes() -> Vec<Node> {
        let mut nodes = widest_containers();
        nodes.extend(widest_leaves());
        nodes.extend(widest_preem().into_iter().map(|widget| Node::Preem {
            id: Some("i".into()),
            classes: classes(),
            widget: Box::new(widget),
        }));
        nodes
    }

    /// The container half of [`widest_nodes`].
    fn widest_containers() -> Vec<Node> {
        vec![
            Node::Box {
                id: Some("i".into()),
                dir: Dir::Vertical,
                spacing: 4,
                scroll: true,
                classes: classes(),
                children: Vec::new(),
                tooltip: Some("t".into()),
            },
            Node::Row {
                id: Some("i".into()),
                classes: classes(),
                spacing: 4,
                children: Vec::new(),
                tooltip: Some("t".into()),
            },
            Node::ListBox {
                id: Some("i".into()),
                classes: classes(),
                dense: true,
                children: Vec::new(),
            },
            Node::Scrolled {
                id: Some("i".into()),
                max_height: 100,
                classes: classes(),
                child: spacer(),
            },
            Node::Button {
                id: "b".into(),
                classes: classes(),
                child: spacer(),
            },
            Node::Revealer {
                id: Some("i".into()),
                open: true,
                child: spacer(),
            },
            Node::Expander {
                id: "e".into(),
                header: spacer(),
                children: Vec::new(),
                expanded: true,
                classes: classes(),
                tooltip: Some("t".into()),
            },
        ]
    }

    /// The leaf half of [`widest_nodes`], less the preem kinds.
    fn widest_leaves() -> Vec<Node> {
        vec![
            Node::Label {
                id: Some("i".into()),
                text: "t".into(),
                classes: classes(),
                tooltip: Some("t".into()),
            },
            Node::Text {
                id: Some("i".into()),
                text: "t".into(),
                max_width_chars: Some(20),
                ellipsize: true,
                classes: classes(),
                tooltip: Some("t".into()),
            },
            Node::Icon {
                id: Some("i".into()),
                name: "n".into(),
                classes: classes(),
                tooltip: Some("t".into()),
            },
            Node::Pixels {
                id: Some("i".into()),
                width: 1,
                height: 1,
                data: vec![0; 4],
                scale: 2,
                classes: classes(),
            },
            Node::Progress {
                id: Some("i".into()),
                fraction: 0.5,
                classes: classes(),
            },
            Node::Slider {
                id: "s".into(),
                min: 0.0,
                max: 1.0,
                value: 0.5,
                step: 0.1,
                enabled: false,
                classes: classes(),
            },
            Node::Separator { classes: classes() },
            Node::Spacer,
            Node::Entry {
                id: "e".into(),
                text: "t".into(),
                placeholder: "p".into(),
                classes: classes(),
            },
            Node::Shader {
                id: Some("i".into()),
                width: 1,
                height: 1,
                scale: 2,
                fragment: "f".into(),
                data: vec![0; 4],
                format: ShaderData::R8,
                data_width: 4,
                data_height: 1,
                classes: classes(),
                tooltip: Some("t".into()),
            },
            Node::Sparkline {
                id: Some("i".into()),
                values: Vec::new(),
                max: Some(1.0),
                classes: classes(),
            },
            Node::MultiSparkline {
                id: Some("i".into()),
                series: Vec::new(),
                max: Some(1.0),
                classes: classes(),
            },
        ]
    }

    /// The values a node costs on its own: its encoding less one value per
    /// mandatory [`Node::Spacer`] child [`widest_nodes`] gave it.
    fn own_values(node: &Node) -> usize {
        let spacers = match node {
            Node::Scrolled { .. }
            | Node::Button { .. }
            | Node::Revealer { .. }
            | Node::Expander { .. } => 1,
            _ => 0,
        };
        shape(node).values - spacers
    }

    /// What [`MAX_FRAME_VALUES`]' derivation says the widest preem node costs.
    const WIDEST_PREEM_NODE: usize = 89;
    /// What [`MAX_FRAME_VALUES`]' derivation says the widest other node costs.
    const WIDEST_OTHER_NODE: usize = 57;
    /// What [`MAX_FRAME_DEPTH`]'s doc says a tree at the host's depth cap is.
    const DEEPEST_LEGAL_RENDER: usize = 198;
    const _: () = assert!(DEEPEST_LEGAL_RENDER <= MAX_FRAME_DEPTH);

    /// A `Render` envelope at its widest: `hidden_on` at the host's 64-entry
    /// cap and a burst of eight effects (the host's `EFFECT_BURST`), around
    /// the two trees.
    fn render(tree: Node, panel: Node) -> PluginMsg {
        PluginMsg::Render {
            tree,
            panel: Some(Box::new(panel)),
            hidden_on: vec!["HDMI-A-1".into(); 64],
            effects: vec![Effect::run_command(1, vec!["argv".into(); 8]); 8],
        }
    }

    /// The numbers [`MAX_FRAME_VALUES`]' doc derives the budget from are the
    /// ones the walk measures, for every node kind there is.
    ///
    /// **Falsified** by a new field on a node kind, which moves its widest
    /// cost: update the two constants and the derivation in the cap's doc
    /// together.
    #[test]
    fn the_widest_nodes_cost_what_the_derivation_says() {
        let nodes = widest_nodes();
        let kinds: std::collections::HashSet<_> =
            nodes.iter().map(std::mem::discriminant).collect();
        assert_eq!(kinds.len(), 20, "every `Node` kind the match below names");
        let (mut preem, mut other) = (0, 0);
        for node in &nodes {
            let values = own_values(node);
            // Exhaustive on purpose: a new `Node` kind stops this compiling
            // until it is sorted here and its widest form is in `widest_nodes`.
            match node {
                Node::Preem { .. } => preem = preem.max(values),
                Node::Box { .. }
                | Node::Row { .. }
                | Node::ListBox { .. }
                | Node::Scrolled { .. }
                | Node::Label { .. }
                | Node::Text { .. }
                | Node::Icon { .. }
                | Node::Pixels { .. }
                | Node::Button { .. }
                | Node::Progress { .. }
                | Node::Slider { .. }
                | Node::Revealer { .. }
                | Node::Separator { .. }
                | Node::Spacer
                | Node::Expander { .. }
                | Node::Entry { .. }
                | Node::Shader { .. }
                | Node::Sparkline { .. }
                | Node::MultiSparkline { .. } => other = other.max(values),
            }
        }
        let widgets: std::collections::HashSet<_> = widest_preem()
            .iter()
            .map(|widget| match widget {
                // Exhaustive for the same reason, over the preem kinds.
                PreemWidget::DotMatrix { .. } => 0,
                PreemWidget::SevenSeg { .. } => 1,
                PreemWidget::TextBox { .. } => 2,
                PreemWidget::LedStrip { .. } => 3,
                PreemWidget::Marquee { .. } => 4,
                PreemWidget::Scope { .. } => 5,
                PreemWidget::Gauge { .. } => 6,
                PreemWidget::FlipBoard { .. } => 7,
            })
            .collect();
        assert_eq!(widgets.len(), 8, "every `PreemWidget` kind");
        assert_eq!(preem, WIDEST_PREEM_NODE, "the widest preem node");
        assert_eq!(other, WIDEST_OTHER_NODE, "the widest other node");
        let per_tree = MAX_PREEM_NODES_PER_TREE * preem
            + (MAX_NODES_PER_TREE - MAX_PREEM_NODES_PER_TREE) * other;
        assert_eq!(per_tree, 235_520, "the derivation's per-tree figure");
    }

    /// The widest frame the host would accept and render whole decodes: a
    /// `Render` whose `tree` and `panel` each hold [`MAX_NODES_PER_TREE`]
    /// nodes of the widest kinds (the preem ones up to
    /// [`MAX_PREEM_NODES_PER_TREE`]), carrying a shader data buffer at
    /// [`MAX_SHADER_DATA_BYTES`] and an 8 MiB pixel buffer, in the widest
    /// envelope.
    #[test]
    fn the_widest_legal_render_decodes() {
        let widest = widest_nodes();
        let pick = |want: &dyn Fn(&Node) -> bool| {
            widest
                .iter()
                .filter(|node| want(node))
                .max_by_key(|node| own_values(node))
                .cloned()
                .expect("a node of that kind")
        };
        let preem = pick(&|node| matches!(node, Node::Preem { .. }));
        let other = pick(&|node| !matches!(node, Node::Preem { .. }));
        assert!(matches!(other, Node::Shader { .. }), "{other:?}");
        let tree = |extra: Node| {
            let Some(Node::Box {
                id,
                dir,
                spacing,
                scroll,
                classes,
                tooltip,
                ..
            }) = widest.first().cloned()
            else {
                unreachable!("widest_nodes starts with a Box");
            };
            let mut children = vec![preem.clone(); MAX_PREEM_NODES_PER_TREE];
            children.resize(MAX_NODES_PER_TREE - 2, other.clone());
            children.push(extra);
            Node::Box {
                id,
                dir,
                spacing,
                scroll,
                classes,
                children,
                tooltip,
            }
        };
        let pixels = Node::Pixels {
            id: Some("p".into()),
            width: 2048,
            height: 1024,
            data: vec![0xa5; 2048 * 1024 * 4],
            scale: 1,
            classes: classes(),
        };
        let Node::Shader {
            id,
            width,
            height,
            scale,
            fragment,
            format,
            classes,
            tooltip,
            ..
        } = other.clone()
        else {
            unreachable!("asserted above");
        };
        // 2048 × 2048 `R8` cells: exactly `MAX_SHADER_DATA_BYTES`.
        let shader = Node::Shader {
            id,
            width,
            height,
            scale,
            fragment,
            data: vec![0x5a; MAX_SHADER_DATA_BYTES],
            format,
            data_width: 2048,
            data_height: 2048,
            classes,
            tooltip,
        };
        let msg = render(tree(shader), tree(pixels));
        let frame = encode(&msg);
        assert!(
            frame.len() <= LEN_PREFIX + MAX_FRAME_LEN,
            "{} B",
            frame.len()
        );
        let measured = walk(&frame[LEN_PREFIX..]).expect("the widest legal render walks");
        // The derivation's 471,040, less what the root and the two buffer
        // nodes save over the widest kinds, plus the envelope.
        assert!(
            measured.values > 470_000 && measured.values <= MAX_FRAME_VALUES,
            "{measured:?} against MAX_FRAME_VALUES {MAX_FRAME_VALUES}",
        );
        let back: PluginMsg = decode(&frame).expect("the widest legal render decodes");
        assert_eq!(back, msg);
    }

    /// A tree at the host's [`MAX_TREE_DEPTH`] (63 containers above a leaf,
    /// each container the deepest-nesting kind, the leaf the deepest preem
    /// one) is the depth [`MAX_FRAME_DEPTH`]'s doc states, inside the cap.
    #[test]
    fn a_tree_at_the_hosts_depth_cap_fits() {
        let leaf = Node::Preem {
            id: None,
            classes: Vec::new(),
            widget: Box::new(widest_preem().swap_remove(6)),
        };
        let chain = (1..MAX_TREE_DEPTH).fold(leaf, |child, _| Node::Box {
            id: None,
            dir: Dir::Horizontal,
            spacing: 0,
            scroll: false,
            classes: Vec::new(),
            children: vec![child],
            tooltip: None,
        });
        let msg = render(chain.clone(), chain);
        let measured = shape(&msg);
        assert_eq!(measured.depth, DEEPEST_LEGAL_RENDER, "{measured:?}");
        let back: PluginMsg = decode(&encode(&msg)).expect("a legal-depth tree decodes");
        assert_eq!(back, msg);
    }

    /// An array of `n - 1` one-byte values: `n` values in all.
    fn values(n: usize) -> Vec<u8> {
        let mut body = vec![0xdd];
        body.extend_from_slice(&u32::try_from(n - 1).expect("fits").to_be_bytes());
        body.resize(5 + n - 1, 0x0d);
        body
    }

    /// The budget admits exactly [`MAX_FRAME_VALUES`] values, and one more is
    /// refused at the header that declares it, reporting that header's count.
    #[test]
    fn the_value_budget_is_exact() {
        assert_eq!(
            walk(&values(MAX_FRAME_VALUES)).map(|s| s.values).ok(),
            Some(MAX_FRAME_VALUES),
        );
        assert!(matches!(
            walk(&values(MAX_FRAME_VALUES + 1)),
            Err(ProtoError::TooManyValues { at_least }) if at_least == MAX_FRAME_VALUES + 1
        ));
        // Spread over many small containers, where no single header proves
        // the excess, it is refused at the header that tips the running count.
        let mut spread = vec![0xdd];
        let arrays = MAX_FRAME_VALUES / 8 + 1;
        spread.extend_from_slice(&u32::try_from(arrays).expect("fits").to_be_bytes());
        for _ in 0..arrays {
            spread.extend_from_slice(&[0x97, 1, 2, 3, 4, 5, 6, 7]);
        }
        assert!(matches!(
            walk(&spread),
            Err(ProtoError::TooManyValues { at_least }) if at_least > MAX_FRAME_VALUES
        ));
    }

    /// `depth` nested one-element arrays around a `nil`.
    fn nested(depth: usize) -> Vec<u8> {
        let mut body = vec![0x91; depth];
        body.push(0xc0);
        body
    }

    /// The depth cap is exact, and a body nested far past it, far past
    /// `rmp-serde`'s own 1024 too, is refused by a walk that does not
    /// recurse.
    #[test]
    fn the_depth_cap_is_exact() {
        assert_eq!(
            walk(&nested(MAX_FRAME_DEPTH)).ok(),
            Some(Shape {
                values: MAX_FRAME_DEPTH + 1,
                depth: MAX_FRAME_DEPTH,
            }),
        );
        for depth in [MAX_FRAME_DEPTH + 1, 1024, MAX_FRAME_LEN - 1] {
            assert!(
                matches!(
                    walk(&nested(depth)),
                    Err(ProtoError::Decode(
                        rmp_serde::decode::Error::DepthLimitExceeded
                    ))
                ),
                "{depth} levels",
            );
        }
        // An empty container still counts as a level.
        let mut empty = vec![0x91; MAX_FRAME_DEPTH];
        empty.push(0x90);
        assert!(matches!(
            walk(&empty),
            Err(ProtoError::Decode(
                rmp_serde::decode::Error::DepthLimitExceeded
            ))
        ));
    }

    /// A header that declares more than the body can hold is refused at the
    /// header, whatever kind it is and wherever it sits.
    #[test]
    fn a_lying_header_is_refused_at_the_header() {
        use rmp_serde::decode::Error;
        let tail = [0x0d_u8; 64];
        let lying = |head: &[u8]| {
            let mut body = vec![0x93, 0x01, 0xa1, b'x'];
            body.extend_from_slice(head);
            body.extend_from_slice(&tail);
            walk(&body)
        };
        let max = u32::MAX.to_be_bytes();
        for head in [
            [&[0xdd_u8][..], &max].concat(),
            [&[0xdf_u8][..], &max].concat(),
            vec![0xdc, 0x03, 0xe8],
            vec![0xde, 0x00, 0x21],
        ] {
            assert!(
                matches!(
                    lying(&head),
                    Err(ProtoError::Decode(Error::LengthMismatch(_)))
                ),
                "{head:02x?}",
            );
        }
        for head in [
            [&[0xdb_u8][..], &max].concat(),
            [&[0xc6_u8][..], &max].concat(),
            [&[0xc9_u8][..], &max].concat(),
            vec![0xd9, 0xff],
            vec![0xc4, 0xff],
        ] {
            assert!(
                matches!(
                    lying(&head),
                    Err(ProtoError::Decode(Error::InvalidDataRead(_)))
                ),
                "{head:02x?}",
            );
        }
    }

    /// The one marker byte `MessagePack` leaves unassigned is refused, as
    /// `rmp-serde` refuses it.
    #[test]
    fn the_reserved_marker_is_refused() {
        for body in [&[0xc1_u8][..], &[0x92, 0x01, 0xc1], &[0x81, 0xc1, 0x01]] {
            assert!(
                matches!(
                    walk(body),
                    Err(ProtoError::Decode(rmp_serde::decode::Error::TypeMismatch(
                        rmp::Marker::Reserved
                    )))
                ),
                "{body:02x?}",
            );
        }
    }

    /// The walk reads the one value serde reads, so trailing bytes are still
    /// ignored rather than refused: the walk adds no refusal to a body
    /// `rmp-serde` accepted, except past its two caps.
    #[test]
    fn trailing_bytes_are_ignored_as_before() {
        let mut body = encode_body(&PluginMsg::Pong { seq: 7 });
        let len = body.len();
        body.extend_from_slice(&[0xc1, 0xdd, 0xff]);
        assert_eq!(walk(&body).ok(), walk(&body[..len]).ok());
        assert_eq!(
            decode_body::<PluginMsg>(&body).ok(),
            Some(PluginMsg::Pong { seq: 7 })
        );
    }

    /// A small deterministic generator of arbitrary `MessagePack`, using every
    /// marker form there is, that knows what it generated.
    struct Gen(u64);

    impl Gen {
        fn next(&mut self) -> u64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next() % u64::try_from(n).expect("fits")).expect("fits")
        }

        fn bytes(&mut self, out: &mut Vec<u8>, n: usize) {
            for _ in 0..n {
                out.push(b'a' + u8::try_from(self.below(26)).expect("fits"));
            }
        }

        /// A length-prefixed form: the marker, then `len` in `width` bytes.
        fn header(out: &mut Vec<u8>, marker: u8, width: usize, len: usize) {
            out.push(marker);
            let len = u32::try_from(len).expect("fits").to_be_bytes();
            out.extend_from_slice(&len[4 - width..]);
        }

        /// Append one value, returning the values and depth it holds.
        fn value(&mut self, out: &mut Vec<u8>, room: usize) -> (usize, usize) {
            let form = self.below(if room == 0 { 9 } else { 14 });
            match form {
                0 => out.push(u8::try_from(self.below(0x80)).expect("fits")),
                1 => out.push(0xe0 | u8::try_from(self.below(0x20)).expect("fits")),
                2 => out.push([0xc0, 0xc2, 0xc3][self.below(3)]),
                3 => {
                    let (marker, width) = [
                        (0xcc, 1),
                        (0xcd, 2),
                        (0xce, 4),
                        (0xcf, 8),
                        (0xd0, 1),
                        (0xd1, 2),
                        (0xd2, 4),
                        (0xd3, 8),
                        (0xca, 4),
                        (0xcb, 8),
                    ][self.below(10)];
                    out.push(marker);
                    self.bytes(out, width);
                }
                4 | 5 => {
                    let len = self.below(40);
                    let bin = form == 5;
                    match self.below(4) {
                        0 if !bin && len < 32 => {
                            out.push(0xa0 | u8::try_from(len).expect("fits"));
                        }
                        1 | 0 => Self::header(out, if bin { 0xc4 } else { 0xd9 }, 1, len),
                        2 => Self::header(out, if bin { 0xc5 } else { 0xda }, 2, len),
                        _ => Self::header(out, if bin { 0xc6 } else { 0xdb }, 4, len),
                    }
                    self.bytes(out, len);
                }
                6 => {
                    let (marker, len) =
                        [(0xd4, 1), (0xd5, 2), (0xd6, 4), (0xd7, 8), (0xd8, 16)][self.below(5)];
                    out.push(marker);
                    self.bytes(out, 1 + len);
                }
                7 | 8 => {
                    let len = self.below(20);
                    let (marker, width) = [(0xc7, 1), (0xc8, 2), (0xc9, 4)][self.below(3)];
                    Self::header(out, marker, width, len);
                    self.bytes(out, 1 + len);
                }
                _ => {
                    let len = self.below(5);
                    let map = form >= 12;
                    match self.below(3) {
                        0 => {
                            let fix = if map { 0x80 } else { 0x90 };
                            out.push(fix | u8::try_from(len).expect("fits"));
                        }
                        1 => Self::header(out, if map { 0xde } else { 0xdc }, 2, len),
                        _ => Self::header(out, if map { 0xdf } else { 0xdd }, 4, len),
                    }
                    let (mut values, mut depth) = (1, 0);
                    for _ in 0..len * if map { 2 } else { 1 } {
                        let (v, d) = self.value(out, room - 1);
                        values += v;
                        depth = depth.max(d);
                    }
                    return (values, depth + 1);
                }
            }
            (1, 0)
        }
    }

    /// Against arbitrary well-formed `MessagePack` in every marker form:
    ///
    /// - the walk counts exactly the values and depth that were generated;
    /// - `rmp-serde` accepts the same body;
    /// - every truncation of it is refused, by the walk and by `rmp-serde`
    ///   alike, and never panics.
    #[test]
    fn the_walk_agrees_with_what_was_generated() {
        let mut generator = Gen(0x9e37_79b9_7f4a_7c15);
        let mut body = Vec::new();
        let (mut total, mut deepest, mut bytes) = (0, 0, 0);
        for _ in 0..1000 {
            body.clear();
            let want = generator.value(&mut body, 6);
            total += want.0;
            deepest = deepest.max(want.1);
            bytes += body.len();
            assert_eq!(
                walk(&body).map(|s| (s.values, s.depth)).ok(),
                Some(want),
                "{body:02x?}",
            );
            assert!(
                rmp_serde::from_slice::<serde::de::IgnoredAny>(&body).is_ok(),
                "{body:02x?}",
            );
            for end in 0..body.len() {
                assert!(
                    matches!(walk(&body[..end]), Err(ProtoError::Decode(_))),
                    "{:02x?}",
                    &body[..end],
                );
                assert!(rmp_serde::from_slice::<serde::de::IgnoredAny>(&body[..end]).is_err());
            }
        }
        // The generator is seeded, so these are fixed; they keep it from
        // quietly degenerating into a test of scalars.
        assert!(
            total > 5000 && deepest == 6 && bytes > 40_000,
            "{total} values, {deepest} deep, {bytes} B"
        );
    }
}
