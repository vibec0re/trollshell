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
//! [`read_frame`] / [`write_frame`].

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Width of the length prefix, in bytes.
pub const LEN_PREFIX: usize = 4;

/// Hard cap on a single frame's body, to bound memory against a hostile or
/// buggy peer that declares a huge length. 16 MiB is far above any real view
/// tree; enforced only on the **read** side ([`decode`] / [`read_frame`]).
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

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
pub fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, ProtoError> {
    rmp_serde::from_slice(body).map_err(ProtoError::Decode)
}

/// Decode exactly one whole length-prefixed frame into `T`.
///
/// Strict: `frame` must be exactly the prefix plus the declared body (use
/// [`read_frame`] for streaming, where boundaries are handled for you). A short
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
