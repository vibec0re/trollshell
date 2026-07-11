//! The socket topology both ends share.
//!
//! Per the stamped connection model (#195): the host **listens** on one
//! same-user-only Unix socket under `$XDG_RUNTIME_DIR`, and plugins **dial
//! in**. The path is part of the wire contract — host and plugins must agree
//! on it byte-for-byte — so it lives here in the shared protocol crate rather
//! than being re-derived (and drifting) on each side.

use std::path::PathBuf;

/// Directory under `$XDG_RUNTIME_DIR` holding the host socket. The host
/// creates it `0700`; the runtime dir itself is already same-user-only.
pub const SOCKET_DIR: &str = "trollshell";

/// The host socket's file name inside [`SOCKET_DIR`].
pub const SOCKET_FILE: &str = "plugin.sock";

/// The well-known host socket path,
/// `$XDG_RUNTIME_DIR/`[`SOCKET_DIR`]`/`[`SOCKET_FILE`] — or `None` if
/// `XDG_RUNTIME_DIR` is unset (same-user-only by spec: neither side falls
/// back anywhere else).
#[must_use]
pub fn socket_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")?;
    let mut path = PathBuf::from(base);
    path.push(SOCKET_DIR);
    path.push(SOCKET_FILE);
    Some(path)
}
