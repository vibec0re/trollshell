//! Taking the bridge's listening socket: **a same-uid Unix socket**, not a
//! loopback port (#993).
//!
//! # The boundary
//!
//! This daemon answers an unauthenticated route that spends somebody's Claude
//! subscription — real money in `CLAUDE_BRIDGE_MODE=api`. It cannot demand a
//! bearer token (see the crate docs: the consuming plugin's key resolution
//! makes that structurally impossible), so **whoever can reach the socket is
//! authorized**, and the whole question is who that is.
//!
//! `127.0.0.1:8787` answered that badly. TCP loopback carries no file mode: any
//! second local account, and any container sharing the host network namespace
//! (the #947/#949 hive agents, directly), could spend the owner's subscription
//! and — with [`crate::bridge`]'s two permits — starve pet and caw with two
//! held connections. Every artefact that stated the boundary tested only the
//! LAN direction; none of them said that.
//!
//! A Unix socket does carry a mode. `0600` inside a `0700` directory under
//! `$XDG_RUNTIME_DIR` is exactly the boundary #956 settled for the plugin
//! socket, and this module runs `trollshell::plugins::listener`'s sequence:
//! **lock → probe → unlink a *stale* socket → bind → tighten to `0600`**.
//!
//! # …but strictly stricter than that sibling
//!
//! It is the same *order*, not the same *strength*, and the difference is
//! deliberate rather than accidental drift. `listener.rs` treats both chmods as
//! best-effort (`let _ = fs::set_permissions(…)`) and never tightens a
//! directory it did not create; here **both propagate**, so:
//!
//! - a parent directory this process cannot chmod to `0700` — i.e. one it does
//!   not own — is a refusal to start, not a warning nobody reads;
//! - a socket that binds but cannot be tightened to `0600` is unbound **and
//!   unlinked**, rather than served at whatever the inherited umask produced.
//!
//! The reason for the asymmetry is what is behind each socket. Reaching the
//! plugin host socket gets you a widget on somebody's bar; reaching this one
//! spends their Claude subscription, metered credits included. A mode that
//! silently failed to apply is the *whole* defect #993 is about, so it is not
//! something to log and continue through. `listener.rs` is now the weaker
//! sibling of the pair and arguably wants the same treatment — that is a
//! separate change to a file this PR deliberately does not touch.
//!
//! Each step is there because something went wrong without it:
//!
//! - the **lock** (#996) orders two starts through probe→bind. Without it both
//!   probe an absent socket, both bind, and the loser keeps a valid listener on
//!   an inode nothing can name — logging success while accepting nothing.
//! - the **probe** (#436/#995) is what makes "never unlink a live socket" true.
//!   A stale socket file left by a previous run is reclaimable; a socket a live
//!   sibling is answering on is not, and this one refuses to start rather than
//!   seize it (the honest analogue of the `EADDRINUSE` the TCP bind used to
//!   fail with).
//! - the **mode** is the boundary itself. It is applied after `bind(2)` rather
//!   than through a umask because the umask is inherited from whoever launched
//!   us, and a `0002` umask would leave the socket group-writable.
//!
//! There is **no TCP fallback anywhere in this module, on any path.** With no
//! `$XDG_RUNTIME_DIR` there is no socket and the daemon refuses to serve; a
//! quiet fall back to a port is the hole itself.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

/// Suffix appended to the socket file name to name the single-instance lock
/// file. It sits beside the socket in the same `0700` directory —
/// `…/claude-bridge.sock.lock` — appended to the whole file name rather than
/// swapped in as an extension, so it can never collide with the socket itself.
const LOCK_SUFFIX: &str = ".lock";

/// The single-instance lock path for `socket`.
pub fn lock_path(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_os_string();
    name.push(LOCK_SUFFIX);
    PathBuf::from(name)
}

/// Where this bridge listens, given a runtime dir — `None` when
/// `$XDG_RUNTIME_DIR` is unset.
///
/// The path is **not configurable**, exactly as the old listen address's IP was
/// not: it is `hytte_ai_providers::bridge_socket_path`, the one definition the
/// client crate dials and this daemon binds, so no environment mistake can move
/// the endpoint somewhere a second uid can reach. Taken from the *client* crate
/// rather than restated here for the reason
/// `hytte_plugin_proto::topology` gives: a path two ends must agree on
/// byte-for-byte is a wire contract, and a second copy of it is a drift waiting
/// to happen.
pub fn socket_path(runtime_dir: Option<&Path>) -> Option<PathBuf> {
    runtime_dir.map(hytte_ai_providers::bridge_socket_path_in)
}

/// Probe whether a live listener already owns the socket (#436/#995). A
/// successful connect means another `hytte-claude-bridge` is serving the path,
/// so this one must stand down rather than unlink a working socket. A
/// refused/failed connect means a stale socket file (a previous run left it) or
/// no file at all — safe to reclaim. The probe connection is dropped
/// immediately without sending a byte, so the incumbent reads EOF and answers
/// it with the 400 its parser gives any empty request.
pub async fn socket_in_use(path: &Path) -> bool {
    UnixStream::connect(path).await.is_ok()
}

/// Take the exclusive single-instance lock beside the socket (#996).
/// `Ok(Some(file))` = this process owns the socket; `Ok(None)` = another
/// process holds the lock.
///
/// `File::try_lock` is `flock(LOCK_EX | LOCK_NB)` on Unix — stable std, so no
/// new dependency and no `unsafe`. The lock lives on the **open file
/// description**, so the kernel drops it when the process dies (no stale state
/// to detect, no crash path that wedges the next start) and two separate opens
/// conflict even within one process (which is what lets the tests race two
/// takes without forking).
///
/// The lock file is created once and **never unlinked**: unlinking a flocked
/// path lets a racing instance create a fresh inode and lock *that* instead,
/// which is the race this closes. It is an empty file in `$XDG_RUNTIME_DIR`,
/// cleaned up by logind with the rest of the runtime dir.
pub fn acquire_listen_lock(lock: &Path) -> std::io::Result<Option<File>> {
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

/// The socket this process owns: the bound listener **and** the
/// single-instance lock, as one value.
///
/// One value on purpose. The lock must be held for exactly as long as the
/// listener — an accept loop that outlives its lock lets the next process past
/// the gate while this one is still serving — and as two bindings that
/// invariant rests on a `_lock` binding whose deletion nothing can observe.
/// Here it is structural: dropping the socket releases the lock, and there is
/// no way to keep accepting without keeping the lock.
#[derive(Debug)]
pub struct BridgeSocket {
    listener: UnixListener,
    /// The single-instance flock, held for this socket's whole life and
    /// released by the kernel when the process dies. Never read — its `Drop` is
    /// the whole contract.
    _lock: File,
}

impl BridgeSocket {
    /// Accept one client connection.
    pub async fn accept(&self) -> std::io::Result<(UnixStream, tokio::net::unix::SocketAddr)> {
        self.listener.accept().await
    }
}

/// The outcome of trying to take the socket.
#[derive(Debug)]
pub enum SocketClaim {
    /// This process owns the socket: the bound listener and the lock it holds
    /// for its whole life.
    Bound(BridgeSocket),
    /// Another process holds the single-instance lock: it either already owns
    /// the socket or is between its own probe and bind.
    Locked,
    /// Nobody holds the lock, yet a live listener still answers on the path.
    /// Stand down anyway rather than unlink a working socket (#436).
    AlreadyLive,
}

/// How the sequence sets a mode. A function pointer rather than a direct
/// `fs::set_permissions` call **so the fail-closed paths can be tested at all**.
///
/// Both chmods below are load-bearing and both propagate their error, which is
/// the difference between this module and the listener it is modelled on. That
/// is only worth claiming if it is pinned, and it cannot be pinned against the
/// real syscall: a same-uid process owns everything it creates, so `chmod(2)`
/// on its own directory and its own socket does not fail — making it fail
/// needs a second uid, which a hermetic `cargo test` does not have. Injecting
/// the one call is the seam that makes "refuses to start rather than serve an
/// open endpoint" a measurable claim instead of a comment.
type Chmod = fn(&Path, u32) -> std::io::Result<()>;

/// The real one.
fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Create the socket's parent directory, `0700`.
///
/// Normally `$XDG_RUNTIME_DIR/trollshell`, shared with the plugin host socket
/// and created by whichever of the two starts first. The runtime dir itself is
/// already `0700`; this is belt and braces for the nested directory, and the
/// chmod is unconditional so a directory a *previous, looser* version created
/// is tightened rather than trusted.
///
/// The error propagates: a directory this process cannot tighten is one it does
/// not own, and binding an endpoint that spends the owner's subscription inside
/// somebody else's directory is exactly what must not happen quietly.
fn prepare_dir(path: &Path, chmod: Chmod) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        chmod(parent, 0o700)?;
    }
    Ok(())
}

/// Take the socket: lock, probe, reclaim a stale socket, bind, tighten to
/// `0600`. See the module docs for why each step is there.
pub async fn take_socket(path: &Path) -> std::io::Result<SocketClaim> {
    take_socket_with(path, chmod).await
}

/// [`take_socket`] with the mode-setting call injected — see [`Chmod`].
async fn take_socket_with(path: &Path, chmod: Chmod) -> std::io::Result<SocketClaim> {
    prepare_dir(path, chmod)?;
    // 1. The gate. Held for the listener's life inside `SocketClaim::Bound`.
    let Some(lock) = acquire_listen_lock(&lock_path(path))? else {
        return Ok(SocketClaim::Locked);
    };
    // 2. Never unlink a live sibling's socket (#436/#995).
    if socket_in_use(path).await {
        return Ok(SocketClaim::AlreadyLive);
    }
    // 3. A *stale* socket left by a previous run makes `bind` fail EADDRINUSE.
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    // 4. THE BOUNDARY. Not best-effort: a socket that stayed at the umask's
    // mode is reachable by other uids, which is the whole defect this closes,
    // so a failure here unbinds and unlinks rather than serving an open
    // endpoint. Leaving the file would be worse than never binding: the next
    // start's probe would find it dead, reclaim it, and nobody would learn the
    // mode could not be set.
    if let Err(e) = chmod(path, 0o600) {
        drop(listener);
        let _ = fs::remove_file(path);
        return Err(e);
    }
    Ok(SocketClaim::Bound(BridgeSocket {
        listener,
        _lock: lock,
    }))
}

/// Bind the listening socket, or log why there is none.
///
/// `None` covers all three refusals — no runtime dir, a live sibling, another
/// process mid-start — and `main` turns it into a failure exit so the transient
/// unit's `Restart=` means something, exactly as a failed `bind` on the old
/// loopback port did.
///
/// **The return type is where "no TCP fallback" is enforced**: a
/// [`BridgeSocket`] holds a [`UnixListener`], so this function cannot hand back
/// a TCP listener even if somebody wanted one to exist.
pub async fn bind_listener(path: Option<&Path>) -> Option<BridgeSocket> {
    let Some(path) = path else {
        tracing::error!(
            "XDG_RUNTIME_DIR is unset, so there is no same-uid socket to bind — \
             refusing to serve (there is deliberately no loopback fallback: a \
             TCP port is reachable by every local uid, which is what #993 closed)",
        );
        return None;
    };
    match take_socket(path).await {
        Ok(SocketClaim::Bound(socket)) => Some(socket),
        Ok(SocketClaim::Locked) => {
            tracing::error!(
                lock = %lock_path(path).display(),
                "another hytte-claude-bridge holds the single-instance lock; not taking the socket over",
            );
            None
        }
        Ok(SocketClaim::AlreadyLive) => {
            tracing::error!(
                socket = %path.display(),
                "the socket already has a live listener (another hytte-claude-bridge?); \
                 not taking it over",
            );
            None
        }
        Err(e) => {
            tracing::error!(socket = %path.display(), error = %e, "could not bind");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BridgeSocket, SocketClaim, acquire_listen_lock, bind_listener, lock_path, socket_in_use,
        socket_path, take_socket, take_socket_with,
    };
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::time::Duration;

    /// Bound wait for a precondition that is not synchronous with the call that
    /// causes it (a dropped listener's socket going stale, an flock being
    /// released on drop).
    const SETTLE_ATTEMPTS: usize = 2000;

    async fn settle_until_stale(path: &Path) {
        for _ in 0..SETTLE_ATTEMPTS {
            if !socket_in_use(path).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("a dropped listener still answers on {}", path.display());
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("exists")
            .permissions()
            .mode()
            & 0o7777
    }

    /// **The boundary, measured.** The socket is `0600` inside a `0700`
    /// directory — the #956 shape — so no second uid can connect to it at all.
    ///
    /// Mutation: drop the `set_permissions` after `bind`, and the socket
    /// inherits the umask (`0755`/`0775` on a default login), which is the
    /// entire defect. This is the test that goes red.
    #[tokio::test]
    async fn the_socket_is_0600_inside_a_0700_directory() {
        let runtime = tempfile::tempdir().expect("tempdir");
        let path = socket_path(Some(runtime.path())).expect("a runtime dir yields a path");
        let SocketClaim::Bound(socket) = take_socket(&path).await.expect("binds") else {
            panic!("the first take must bind");
        };

        assert_eq!(mode_of(&path), 0o600, "the socket must be same-uid only");
        assert_eq!(
            mode_of(path.parent().expect("the socket has a parent")),
            0o700,
            "…inside a directory only this uid may even traverse",
        );
        // The lock beside it is not a way in either.
        assert_eq!(mode_of(&lock_path(&path)), 0o600);
        drop(socket);
    }

    /// The path is the client crate's, not a second copy: the two ends of the
    /// wire contract cannot drift.
    #[test]
    fn the_socket_path_is_the_one_the_client_dials() {
        assert_eq!(
            socket_path(Some(Path::new("/run/user/1000"))),
            Some(std::path::PathBuf::from(
                "/run/user/1000/trollshell/claude-bridge.sock"
            )),
        );
        assert_eq!(socket_path(None), None, "no runtime dir, no socket");
    }

    /// **No TCP fallback.** With no `$XDG_RUNTIME_DIR` the daemon binds
    /// *nothing* — it does not quietly fall back to the loopback port it used
    /// to serve, which every other local uid could reach.
    ///
    /// The second assertion is the one that would catch a re-introduced
    /// fallback by observation rather than by type: after the refusal,
    /// `127.0.0.1:8787` is still free. (If this fails on a developer box,
    /// check for an old bridge still running before suspecting the code.)
    #[tokio::test]
    async fn no_runtime_dir_binds_nothing_at_all() {
        assert!(
            bind_listener(None).await.is_none(),
            "no runtime dir must mean no listener",
        );
        std::net::TcpListener::bind(("127.0.0.1", 8787))
            .expect("8787 is free — a TCP fallback would be holding it");
    }

    /// A **stale** socket — a file left behind by a process that is gone — is
    /// reclaimed, so an ordinary restart rebinds cleanly instead of failing
    /// `EADDRINUSE` forever.
    ///
    /// "Reclaimed" is asserted by *serving a connection on it*, not by the
    /// inode changing. The first version of this test compared inodes and went
    /// red in CI: unlinking a file frees its inode number, and the very next
    /// `bind` in the same directory is exactly the allocation most likely to be
    /// handed it back — tmpfs did so (`left: 15466553, right: 15466553`). An
    /// inode number is not an identity across an unlink. The bind succeeding at
    /// all already proves the stale file was removed (it would be `EADDRINUSE`
    /// otherwise), and the accept proves *this* process owns what is there now.
    #[tokio::test]
    async fn a_stale_socket_is_reclaimed() {
        let runtime = tempfile::tempdir().expect("tempdir");
        let path = socket_path(Some(runtime.path())).expect("path");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");

        let dead = tokio::net::UnixListener::bind(&path).expect("a previous run's socket");
        drop(dead);
        settle_until_stale(&path).await;
        assert!(path.exists(), "the stale socket file is still on disk");

        let SocketClaim::Bound(socket) = take_socket(&path).await.expect("reclaims") else {
            panic!("a stale socket must be reclaimed, not stood down from");
        };
        let (client, accepted) =
            tokio::join!(tokio::net::UnixStream::connect(&path), socket.accept());
        client.expect("a client dials the reclaimed path");
        accepted.expect("…and lands on THIS process's listener");
        assert_eq!(mode_of(&path), 0o600);
        drop(socket);
    }

    /// **Fail closed on a directory this process cannot tighten.** A parent it
    /// cannot chmod to `0700` is one it does not own, and binding an endpoint
    /// that spends the owner's subscription inside somebody else's directory is
    /// what must not happen quietly.
    ///
    /// Driven through the injected [`Chmod`] (see its docs): a same-uid process
    /// owns everything it makes, so the real `chmod(2)` cannot be made to fail
    /// here, and the claim would otherwise be a comment. Mutation: turn
    /// `prepare_dir`'s `chmod(parent, 0o700)?` back into `let _ = …` and this
    /// is the test that goes red — the take succeeds and binds.
    #[tokio::test]
    async fn a_directory_that_cannot_be_tightened_refuses_to_start() {
        let runtime = tempfile::tempdir().expect("tempdir");
        let path = socket_path(Some(runtime.path())).expect("path");

        let err = take_socket_with(&path, chmod_denied_on_dir)
            .await
            .expect_err("an untightenable directory is a refusal");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            !path.exists() && !socket_in_use(&path).await,
            "…and nothing was bound on the way out",
        );
    }

    /// **Fail closed on a socket that cannot be tightened.** This is the
    /// boundary itself: a socket left at the inherited umask's mode
    /// (`0755`/`0775` on a default login) is reachable by every other uid,
    /// which is the entire defect. So the bind is undone and the file unlinked
    /// rather than served.
    ///
    /// Mutation: revert the `if let Err(e) = chmod(path, 0o600)` arm to
    /// `let _ = …` and this goes red twice over — the take returns `Ok`, and a
    /// live listener is left on the path.
    #[tokio::test]
    async fn a_socket_that_cannot_be_tightened_is_unbound_rather_than_served() {
        let runtime = tempfile::tempdir().expect("tempdir");
        let path = socket_path(Some(runtime.path())).expect("path");

        let err = take_socket_with(&path, chmod_denied_on_socket)
            .await
            .expect_err("an untightenable socket is a refusal");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            !socket_in_use(&path).await,
            "the endpoint must NOT be answering at the umask's mode",
        );
        assert!(
            !path.exists(),
            "…and the file is gone, so the next start's probe cannot mistake it \
             for a stale socket worth reclaiming",
        );
        // The directory it would have lived in was still tightened.
        assert_eq!(mode_of(path.parent().expect("parent")), 0o700);
    }

    /// A [`Chmod`] that refuses the directory's `0700` and does the real thing
    /// otherwise.
    fn chmod_denied_on_dir(path: &Path, mode: u32) -> std::io::Result<()> {
        if mode == 0o700 {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        super::chmod(path, mode)
    }

    /// A [`Chmod`] that refuses the socket's `0600` and does the real thing
    /// otherwise — so the directory really is tightened before the socket's
    /// chmod fails, which is the ordering the test above asserts.
    fn chmod_denied_on_socket(path: &Path, mode: u32) -> std::io::Result<()> {
        if mode == 0o600 {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        super::chmod(path, mode)
    }

    /// …and a **live** socket is not. The refused newcomer leaves the
    /// incumbent serving and refuses to start itself, rather than unlinking a
    /// working endpoint and logging success onto a socket nobody can reach
    /// (#995's failure, and #436's before it).
    ///
    /// Mutation: delete the `socket_in_use` probe from `take_socket`. Then the
    /// live socket is unlinked and rebound and this is the test that goes red —
    /// the other socket tests all stay green, because a stale socket cannot
    /// tell the two behaviours apart.
    ///
    /// Two assertions, neither of them about an inode (see
    /// `a_stale_socket_is_reclaimed`): the **outcome** is `AlreadyLive`, and
    /// the connection a client makes to the path afterwards is accepted by the
    /// **incumbent's** listener. The second is the property that matters — the
    /// path still leads to the process that was serving it — and the first is
    /// what makes the mutation fail fast rather than hang waiting on an accept
    /// that will never come.
    #[tokio::test]
    async fn a_live_socket_is_never_unlinked() {
        let runtime = tempfile::tempdir().expect("tempdir");
        let path = socket_path(Some(runtime.path())).expect("path");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");

        // An incumbent that never took the lock (an older build, or any other
        // process that bound the path) — so only the probe can see it.
        let incumbent = tokio::net::UnixListener::bind(&path).expect("the incumbent binds");

        assert!(
            matches!(
                take_socket(&path).await.expect("the take completes"),
                SocketClaim::AlreadyLive
            ),
            "a live listener turns the newcomer away",
        );
        // The incumbent is still the one behind the path: the refused instance
        // never unlinked or rebound it.
        let (client, accepted) =
            tokio::join!(tokio::net::UnixStream::connect(&path), incumbent.accept());
        client.expect("a client can still dial the incumbent");
        accepted.expect("the dial lands on the incumbent's listener");
    }

    /// The lock orders two starts through probe→bind: the second stands down
    /// on the lock, before it can unlink anything (#996).
    ///
    /// "Did not touch the first one's socket" is asserted by the first socket
    /// still accepting a dial afterwards — not by an inode, which says nothing
    /// across an unlink.
    #[tokio::test]
    async fn a_second_start_stands_down_on_the_lock() {
        let runtime = tempfile::tempdir().expect("tempdir");
        let path = socket_path(Some(runtime.path())).expect("path");
        let SocketClaim::Bound(first) = take_socket(&path).await.expect("binds") else {
            panic!("the first take must bind");
        };

        assert!(
            matches!(
                take_socket(&path).await.expect("the take completes"),
                SocketClaim::Locked
            ),
            "the second start must not get past the gate",
        );
        let (client, accepted) =
            tokio::join!(tokio::net::UnixStream::connect(&path), first.accept());
        client.expect("a client can still dial the first instance");
        accepted.expect("…and lands on ITS listener, untouched by the refused start");
        drop(first);
    }

    /// **The whole path, end to end.** The real client
    /// (`hytte_ai_providers::chat`, over its `unix://` transport) reaches the
    /// real request parser over the real `0600` socket this module binds, and
    /// gets the real answer back — here a 400 the parser produces without
    /// spawning anything, so the test stays hermetic.
    ///
    /// This is what "the HTTP protocol on top is byte-identical" means in
    /// practice: the two ends were written against `ureq` on TCP and neither
    /// was touched above the socket, so if the framing had drifted (a `Host:`
    /// the parser chokes on, a body length off by one) this is where it would
    /// show. `serve_connection` is inlined rather than called so the test does
    /// not bump the process-global status board that `crate::status`'s own
    /// tests read.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_real_client_reaches_the_real_parser_over_the_bound_socket() {
        use crate::backend::{Backend, Reprompt};
        use crate::bridge::Bridge;

        let runtime = tempfile::tempdir().expect("tempdir");
        let path = socket_path(Some(runtime.path())).expect("path");
        let SocketClaim::Bound(socket) = take_socket(&path).await.expect("binds") else {
            panic!("the take must bind");
        };
        let bridge = Bridge::new(
            Backend::Reprompt(Reprompt::new(hive_claude::Config::default())),
            Duration::from_secs(8),
            None,
        );
        let server = tokio::spawn(async move {
            let (mut stream, _addr) = socket.accept().await.expect("accept");
            let (head, body) = crate::http::read_request(&mut stream)
                .await
                .expect("the client's request parses");
            assert_eq!(head.path, crate::http::ROUTE, "the route survived");
            let (status, out) = bridge.handle(&head, &body).await;
            crate::http::write_response(&mut stream, status, &out).await;
            status
        });

        let url = format!("unix://{}", path.display());
        let err = tokio::task::spawn_blocking(move || {
            hytte_ai_providers::chat(
                &hytte_ai_providers::Provider::llama(url),
                // An empty transcript: refused by the parser before any backend
                // is reached, so nothing spawns `claude`.
                &[],
                &hytte_ai_providers::ChatOpts::default(),
            )
        })
        .await
        .expect("the blocking client joins")
        .expect_err("an empty `messages` is a 400");

        assert!(err.contains("400"), "the status reached the client: {err}");
        assert!(
            err.contains("must not be empty"),
            "…and so did the bridge's own reason: {err}",
        );
        assert_eq!(server.await.expect("the server task"), 400);
    }

    /// The lock lives as long as the socket **because it is part of it**: held
    /// across accepts, released only when the socket value drops. Making them
    /// one value is what stops a `_lock` binding being silently dropped early.
    #[tokio::test]
    async fn the_bound_socket_holds_the_lock_until_it_drops() {
        let runtime = tempfile::tempdir().expect("tempdir");
        let path = socket_path(Some(runtime.path())).expect("path");
        let lock = lock_path(&path);
        let SocketClaim::Bound(socket) = take_socket(&path).await.expect("binds") else {
            panic!("the first take must bind");
        };

        assert!(
            acquire_listen_lock(&lock)
                .expect("the lock file opens")
                .is_none(),
            "the lock is held while the socket lives",
        );
        // Held across a real accept, too.
        let (client, accepted) = tokio::join!(tokio::net::UnixStream::connect(&path), async {
            let socket: &BridgeSocket = &socket;
            socket.accept().await
        });
        client.expect("dial");
        accepted.expect("accept");
        assert!(
            acquire_listen_lock(&lock)
                .expect("the lock file opens")
                .is_none(),
            "…and still held after one",
        );

        drop(socket);
        for attempt in 0..SETTLE_ATTEMPTS {
            if acquire_listen_lock(&lock).expect("opens").is_some() {
                return;
            }
            assert!(attempt + 1 < SETTLE_ATTEMPTS, "the lock was never released");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
