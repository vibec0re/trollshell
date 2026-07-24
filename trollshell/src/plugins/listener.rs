//! tokio-side: the UDS listener that binds the host socket and accepts plugin
//! connections, handing each to [`super::session::handle_conn`].
//!
//! **No accept error is fatal** — the socket stays valid, so a live listener is
//! always worth another `accept()` (#426). The bind path itself refuses to steal
//! a live sibling's socket (#436) and reclaims a stale one.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use hytte_plugin_proto::socket_path;
use tokio::net::{UnixListener, UnixStream};

use super::ListenerCtx;
use super::session::handle_conn;

/// A short backoff applied after a resource-pressure `accept(2)` error, so a
/// *persistent* one (sustained fd/memory exhaustion) degrades gracefully
/// instead of spinning the accept loop hot.
pub(super) const ACCEPT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

/// Decide how the accept loop should react to an `accept(2)` error. **No accept
/// error is fatal** — the socket bound successfully and stays valid, so a live
/// listener is always worth another `accept()`. Terminating the loop here is
/// exactly what stranded every plugin against a dead socket for the rest of the
/// session (#426): `accept(2)` returns transient errors (`ECONNABORTED` when a
/// peer aborts before we take it, or `EMFILE`/`ENFILE`/`ENOBUFS`/`ENOMEM` under
/// momentary resource pressure), yet the plugin-side SDK redials forever, so the
/// asymmetry left the host permanently deaf. Mirrors the `Lagged → continue`
/// survival the bus signal loop got in #428.
///
/// A connection aborted/reset/refused before we accepted it is a pure per-peer
/// hiccup — the listener is untouched — so retry **immediately** (`None`).
/// Anything else gets a short [`ACCEPT_BACKOFF`] (`Some`) before the retry.
/// Total by construction: every error maps to "retry", never "give up".
pub(super) fn accept_backoff(err: &std::io::Error) -> Option<std::time::Duration> {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::ConnectionAborted
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionRefused => None,
        _ => Some(ACCEPT_BACKOFF),
    }
}

/// Probe whether a live listener already owns the host socket (#436). A
/// successful `UnixStream::connect` means another trollshell instance is
/// listening on the path, so a second instance (a dev `cargo run` beside the
/// deployed user service) must stand down rather than unlink the live socket out
/// from under it. A refused/failed connect means a stale socket file (a previous
/// run left it) or no file at all — safe to reclaim. The probe connection is
/// dropped immediately (it sends nothing), so the live host sees an instant EOF
/// and reaps it without waiting out the handshake timeout.
pub(super) async fn socket_in_use(path: &Path) -> bool {
    UnixStream::connect(path).await.is_ok()
}

/// Bind the host socket and accept plugin connections forever. The path comes
/// from [`hytte_plugin_proto::socket_path`] (shared with the plugin-side
/// runtime — the one definition both ends dial/bind; `None` = same-user-only
/// by spec, no fallback). Creates the parent dir (`0700`), refuses to steal a
/// live sibling's socket (#436), unlinks any stale socket before bind, and
/// tightens the socket to `0600`.
pub(super) async fn listen(ctx: &ListenerCtx) -> std::io::Result<()> {
    let Some(path) = socket_path() else {
        tracing::warn!("XDG_RUNTIME_DIR unset; plugin host socket not created");
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        // Same-user only. Best-effort — the runtime dir is already 0700.
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    // #436: refuse to steal a live sibling's socket. A second shell instance (a
    // dev `cargo run` beside the deployed user service) must NOT unlink the
    // running shell's socket and bind its own — that strands every plugin
    // against the dev instance and leaves a dead socket on the dev instance's
    // exit (ECONNREFUSED for every reconnect). Probe first: if a listener
    // already answers on the path, another trollshell owns it, so stand down.
    // Returning `Ok(())` means the supervisor takes this as a clean completion
    // and does not restart us, so the dev instance simply runs without a plugin
    // host. A stale socket (connect refused) falls through and is reclaimed
    // below, so a normal restart — which stops the old process before starting
    // the new — still rebinds cleanly.
    if socket_in_use(&path).await {
        tracing::warn!(
            socket = %path.display(),
            "plugin host socket already has a live listener (another trollshell instance?); \
             not taking it over",
        );
        return Ok(());
    }
    // A stale socket left by a previous run makes `bind` fail with EADDRINUSE.
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(&path)?;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    tracing::info!(socket = %path.display(), "plugin host listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    handle_conn(stream, &ctx).await;
                });
            }
            Err(e) => {
                // Keep the listener alive: a transient `accept(2)` error must
                // NOT kill the loop, or one syscall hiccup strands every plugin
                // against a dead socket until restart (#426). Warn and retry;
                // back off on resource-pressure errors so a persistent one
                // degrades gracefully instead of spinning hot.
                match accept_backoff(&e) {
                    Some(delay) => {
                        tracing::warn!(error = %e, "plugin host accept failed; backing off and retrying");
                        tokio::time::sleep(delay).await;
                    }
                    None => {
                        tracing::debug!(error = %e, "plugin host accept: peer aborted before accept; retrying");
                    }
                }
            }
        }
    }
}
