//! The on-disk + on-socket topology the broker and its CLI share.
//!
//! Three well-known paths, all keyed off the standard XDG base dirs so both the
//! plugin (server) and the [`crate`]'s CLI derive them identically:
//!
//! - **the broker socket** — `$XDG_RUNTIME_DIR/`[`SOCKET_FILE`], the boring
//!   JSON-lines endpoint the CLI dials (same-user-only, `0600`, unlinked before
//!   bind exactly like the host's own plugin socket);
//! - **the grant store** — `$XDG_STATE_HOME/`[`STATE_DIR`]`/`[`GRANTS_FILE`]
//!   (falling back to `~/.local/state/…`), the durable half;
//! - tokens are in-memory only (see [`crate::tokens`]) — no path.

use std::path::PathBuf;

/// The broker socket's file name under `$XDG_RUNTIME_DIR`. Deliberately *not*
/// under the host's `trollshell/` socket dir — this is the broker's own
/// endpoint, owned by the plugin process, not the shell.
pub const SOCKET_FILE: &str = "hytte-infobroker.sock";

/// State subdirectory holding the durable grant store, under `$XDG_STATE_HOME`
/// (or `~/.local/state`).
pub const STATE_DIR: &str = "hytte-infobroker";

/// The durable grant store file name inside [`STATE_DIR`].
pub const GRANTS_FILE: &str = "grants.toml";

/// The broker socket path, `$XDG_RUNTIME_DIR/`[`SOCKET_FILE`] — or `None` if
/// `XDG_RUNTIME_DIR` is unset (same-user-only by spec: no fallback anywhere
/// world-readable).
#[must_use]
pub fn socket_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(base).join(SOCKET_FILE))
}

/// The durable grant store path: `$XDG_STATE_HOME/`[`STATE_DIR`]`/`[`GRANTS_FILE`],
/// or `$HOME/.local/state/`[`STATE_DIR`]`/`[`GRANTS_FILE`] when `XDG_STATE_HOME`
/// is unset (the XDG default). `None` only when neither `XDG_STATE_HOME` nor
/// `HOME` is set.
#[must_use]
pub fn grants_path() -> Option<PathBuf> {
    let base = if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(state)
    } else {
        PathBuf::from(std::env::var_os("HOME")?)
            .join(".local")
            .join("state")
    };
    Some(base.join(STATE_DIR).join(GRANTS_FILE))
}
