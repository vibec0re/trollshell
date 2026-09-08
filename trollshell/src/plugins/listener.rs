//! tokio-side: the UDS listener that binds the host socket and accepts plugin
//! connections, handing each to [`super::session::handle_conn`].
//!
//! **No accept error is fatal** — the socket stays valid, so a live listener is
//! always worth another `accept()` (#426). The bind path itself takes an
//! exclusive lock beside the socket before it looks at anything (#996), refuses
//! to steal a live sibling's socket (#436) and reclaims a stale one.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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

/// Suffix appended to the socket file name to name the single-instance lock
/// file (#996). It sits beside the socket, in the same `0700` runtime dir.
const LOCK_SUFFIX: &str = ".lock";

/// The single-instance lock path for `socket`: the socket path with
/// [`LOCK_SUFFIX`] appended (`…/plugin.sock.lock`). Appended to the whole file
/// name rather than swapped in as an extension, so it can never collide with
/// the socket itself.
pub(super) fn lock_path(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_os_string();
    name.push(LOCK_SUFFIX);
    PathBuf::from(name)
}

/// Take the exclusive single-instance lock beside the host socket (#996).
/// `Ok(Some(file))` = this instance owns the socket; `Ok(None)` = another
/// instance holds the lock.
///
/// `File::try_lock` is `flock(LOCK_EX | LOCK_NB)` on Unix — stable std since
/// 1.89, so this needs no new dependency and no `unsafe`. Two properties make
/// it the right primitive here, and a pid file the wrong one:
///
/// - The lock lives on the **open file description**, so it is held for exactly
///   as long as the returned `File` and the *kernel* drops it when the process
///   dies. There is no stale state to detect, and no crash path that leaves the
///   next start wedged.
/// - Two separate `open`s conflict even **within one process** (unlike POSIX
///   `fcntl` record locks), which is what lets the hermetic tests race two
///   `take_socket` calls without forking.
///
/// The lock file is created once and **never unlinked**: unlinking a flocked
/// path lets a racing instance create a fresh inode and lock *that* instead,
/// which is exactly the race this closes. It is an empty 0-byte file in
/// `$XDG_RUNTIME_DIR`, cleaned up by logind with the rest of the runtime dir.
pub(super) fn acquire_listen_lock(lock: &Path) -> std::io::Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock)?;
    // Same-user only, like the socket itself. Best-effort — the parent is 0700.
    let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// The outcome of trying to take ownership of the host socket (#996).
pub(super) enum SocketClaim {
    /// This instance owns the socket. Carries the bound listener **and** the
    /// lock handle, which the caller must hold for the listener's whole life —
    /// dropping it would let a second instance in behind us.
    Bound(UnixListener, File),
    /// Another instance holds the single-instance lock: it either already owns
    /// the socket or is between its own probe and bind. Stand down.
    Locked,
    /// Nobody holds the lock, yet a live listener still answers on the path —
    /// an instance older than #996 that never took the lock. Stand down anyway
    /// rather than unlink a working socket (#436).
    AlreadyLive,
}

/// Take the host socket: lock, then probe, then bind (#996 + #436).
///
/// The three old steps — probe, unlink, bind — were unsynchronised, so two
/// shells released at the same moment both probed an absent/stale socket, both
/// unlinked and both bound: the first one to bind kept a valid listener on an
/// inode nothing could name any more, logged "plugin host listening", and
/// accepted nothing for the rest of the process's life (#996). The lock closes
/// that window by ordering the whole sequence: it is taken **before** the probe
/// and held past the bind, so at most one instance is ever inside probe→bind.
///
/// The probe is kept, and still decides an outcome rather than only a log line:
/// the lock only orders instances that *take* it, so a shell older than this
/// change — or any other process that binds the path — is still only visible by
/// connecting to it. With the lock held, the probe answering "live" means
/// exactly that, and standing down stays the #436 answer.
pub(super) async fn take_socket(path: &Path) -> std::io::Result<SocketClaim> {
    // 1. The gate. Held for the listener's life via `SocketClaim::Bound`.
    let Some(lock) = acquire_listen_lock(&lock_path(path))? else {
        return Ok(SocketClaim::Locked);
    };
    // 2. #436: refuse to steal a live sibling's socket.
    if socket_in_use(path).await {
        return Ok(SocketClaim::AlreadyLive);
    }
    // 3. A stale socket left by a previous run makes `bind` fail with EADDRINUSE.
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    Ok(SocketClaim::Bound(listener, lock))
}

/// Bind the host socket and accept plugin connections forever. The path comes
/// from [`hytte_plugin_proto::socket_path`] (shared with the plugin-side
/// runtime — the one definition both ends dial/bind; `None` = same-user-only
/// by spec, no fallback). Creates the parent dir (`0700`), then hands the whole
/// lock → probe → bind sequence to [`take_socket`]: one instance at a time
/// (#996), never stealing a live sibling's socket (#436), reclaiming a stale
/// one, and tightening the socket to `0600`.
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
    // #996/#436: a second shell instance (a dev `cargo run` beside the deployed
    // user service, or two starts racing under one session target) must NOT end
    // up owning this path — that strands every plugin against the dev instance
    // and leaves a dead socket on its exit (ECONNREFUSED for every reconnect).
    // Returning `Ok(())` on a stand-down means the supervisor takes this as a
    // clean completion and does not restart us, so the loser simply runs
    // without a plugin host. A stale socket (nobody holding the lock, connect
    // refused) is reclaimed, so a normal restart — which stops the old process
    // before starting the new — still rebinds cleanly.
    //
    // `_lock` is the single-instance flock, deliberately bound for the rest of
    // this function: it must outlive the accept loop, and `listen` only returns
    // when the host is going away.
    let (listener, _lock) = match take_socket(&path).await? {
        SocketClaim::Bound(listener, lock) => (listener, lock),
        SocketClaim::Locked => {
            tracing::warn!(
                lock = %lock_path(&path).display(),
                "another trollshell instance holds the plugin host lock; not taking the socket over",
            );
            return Ok(());
        }
        SocketClaim::AlreadyLive => {
            tracing::warn!(
                socket = %path.display(),
                "plugin host socket already has a live listener (another trollshell instance?); \
                 not taking it over",
            );
            return Ok(());
        }
    };
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
