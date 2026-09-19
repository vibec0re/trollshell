//! Spawn an isolated `dbus-daemon` for one test.
//!
//! Each test gets a fresh broker so tests don't interfere with each other
//! and don't depend on the host's session bus. The daemon is killed when
//! the returned `BusGuard` is dropped.
//!
//! Skips with a clear `panic!("dbus-daemon not on PATH")` if the binary
//! is missing — surface the dependency loudly rather than silently.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use zbus::Connection;
use zbus::connection::Builder;

/// Upper bound on how long the ephemeral `dbus-daemon` is allowed to take to
/// print its listen address before a test gives up on it.
///
/// **This is a liveness guard, not a latency assertion.** No test in this
/// crate claims that `dbus-daemon` *should* start and print its address
/// within any particular time — that would be a meaningless thing to assert
/// about a spawned subprocess under load. The only job of this budget is to
/// stop a broker that never starts from wedging the suite (and therefore
/// `nix flake check`) forever; a genuinely broken daemon still fails the
/// test, just later rather than sooner.
///
/// This lives in the shared `common` module, so it is the single gate that
/// every one of `hytte-bus`'s ten `tests/*.rs` files passes through — each
/// spawns its own broker via [`ephemeral_bus`]. These tests run inside
/// `nix flake check`, which in the same invocation also builds two
/// `nixosTest` VMs plus the full workspace clippy and package builds — CPU
/// contention is the normal condition, not an edge case. A tight budget here
/// buys nothing (nobody is measuring startup latency) and costs false reds
/// (#676/#678: a markdown-only PR tripped a 3-second sibling budget in
/// `hytte-services` under load). If you're looking at this thinking "30
/// seconds seems excessive for a local subprocess to print a line" — it is,
/// for the happy path, and that's the point: this number is sized against
/// worst-case CI contention, not typical-case latency. Tightening it does
/// not strengthen any assertion in this module; it only makes the suite
/// flake more often under load. If you want faster failure signal for a
/// real hang, run the test locally — CI's job is to not lie.
const DBUS_DAEMON_STARTUP_BUDGET: Duration = Duration::from_secs(30);

/// Upper bound on a **single** raw `zbus::Proxy::call` issued by a test in this
/// crate, and on the retry loop such a call sits in. (#1011)
///
/// ## Why a raw call must never be awaited unbounded here
///
/// A zbus method call carries no reply timeout unless the connection was built
/// with one, and `Builder`'s `method_timeout` defaults to `None`. So a call
/// whose reply never arrives does not fail — it parks the awaiting task
/// **forever**. And a reply can genuinely never arrive: zbus creates its object
/// server lazily, its dispatch task is the only consumer of inbound
/// `MethodCall` messages, and until that task has registered its
/// `msg_type=MethodCall, destination=<unique name>` match rule an arriving call
/// matches no receiver and is **dropped with no reply at all**. There is no arm
/// in `zbus::Connection` that synthesises an `UnknownObject`/`UnknownMethod`
/// error for an unhandled call.
///
/// That is what #1011 was, and it cost five `nix flake check` runs ~51 minutes
/// of silence apiece before the job timeout killed them: `tests/export.rs`
/// captured a unique name, called `Hello` on it, lost the race, and the
/// surrounding liveness loop — which re-checks its own deadline only *between*
/// iterations — could never reach that deadline. The last thing CI printed was
/// libtest's `test export_unmounts_on_handle_drop has been running for over 60
/// seconds`.
///
/// ## Why these two numbers, and why a timeout is retried rather than fatal
///
/// `connection.rs`'s `begin_dispatching` starts the dispatch task as soon as a
/// `SharedConnection` is built rather than at the first `export`/`own` mount,
/// which is where the race came from. It cannot *close* the window — zbus
/// exposes the `started_event` that would prove readiness only through
/// `connection::Builder`, and only for a connection built with an
/// already-served interface — so a first call can still be swallowed, and
/// **this bound, not that fix, is what stops #1011 recurring.** Measured over
/// `tests/export.rs` on 12 concurrent 4-core shards each contending with 8 busy
/// loops: the tree without these bounds hung 10 of 300 runs, and the same tree
/// with them hung 0 of 300 — while the first `Hello` was still lost in 35 of
/// 900 runs without `begin_dispatching` and 94 of 900 with it (its doc has the
/// table and why the test constructor is the worse of its two call sites).
///
/// Across all 2,400 instrumented runs, **every** lost call was answered on the
/// very next attempt — the retry count was never above 1.
///
/// So a call that gets no reply is treated as "not ready yet" and **retried**,
/// not failed. That is what lets [`CALL_BUDGET`] be short: a swallowed call
/// costs one second, not the test, and a call that is merely slow under load is
/// re-issued rather than declared broken. It is emphatically **not** a latency
/// assertion — nothing here claims a D-Bus call *should* answer within a second
/// (a healthy run answers in ~1 ms). It is the guard that stops an unanswerable
/// call from wedging the suite, and [`PROBE_BUDGET`] — 20 attempts' worth — is
/// what decides that something is actually broken. Tightening `PROBE_BUDGET` is
/// what would buy false reds under CI contention; tightening `CALL_BUDGET`
/// only costs extra retries. See [`DBUS_DAEMON_STARTUP_BUDGET`] above for the
/// same distinction stated at length.
#[allow(dead_code)] // not every test binary that pulls in `common` makes raw calls
pub const CALL_BUDGET: Duration = Duration::from_secs(1);

/// Upper bound on a whole liveness loop built out of [`CALL_BUDGET`] calls —
/// the deadline that decides a peer is really not answering. See
/// [`CALL_BUDGET`] for why the two are sized against each other.
#[allow(dead_code)] // not every test binary that pulls in `common` makes raw calls
pub const PROBE_BUDGET: Duration = Duration::from_secs(20);

/// Upper bound on reaping the ephemeral `dbus-daemon` in [`BusGuard`]'s `Drop`.
///
/// #1011's diagnosis named **two** things in this crate's tests that can park a
/// test forever, and the raw `zbus::Proxy::call` that [`CALL_BUDGET`] covers was
/// only one of them. The other is this drop: `block_in_place` plus a nested
/// `block_on` on [`tokio::process::Child::wait`], with nothing bounding it.
///
/// Nothing reached it in the 2,400 instrumented runs the PR for #1011 measured,
/// which is why it is a backstop rather than a fix. But an unbounded wait here
/// would be silent in exactly the way the unbounded call was — the test body has
/// already finished, libtest has not yet printed a result, and CI shows nothing
/// at all — so leaving the suite's *other* forever-park unbounded would leave
/// #1011's signature reachable by a second door. Bounded, a reap that wedges
/// costs one line on stderr and an abandoned child (already `SIGKILL`ed, and
/// spawned with `kill_on_drop(true)`) instead of the whole job's timeout.
///
/// Sized like [`DBUS_DAEMON_STARTUP_BUDGET`] and for the same reason: it is a
/// liveness guard, not a latency assertion. Reaping a `SIGKILL`ed local process
/// takes microseconds; 30 s is headroom for a starved runner, not for a slow
/// path.
const DAEMON_REAP_BUDGET: Duration = Duration::from_secs(30);

pub struct BusGuard {
    child: Option<Child>,
    tmp: TempDir,
    // Used by connection_reconnect.rs to open a replacement connection against
    // the same ephemeral bus; not all test binaries need it.
    #[allow(dead_code)]
    pub address: String,
}

impl Drop for BusGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Send SIGKILL then block until the daemon is fully reaped, so the
            // TempDir (socket directory) is not removed before the process exits.
            // block_in_place suspends async scheduling on this thread, allowing
            // a nested block_on without the "cannot block inside async" panic.
            //
            // Bounded by DAEMON_REAP_BUDGET — see its doc: this is the second of
            // the two forever-parks #1011's diagnosis named, and an unbounded
            // wait here is silent in exactly the way the unbounded call was.
            let _ = child.start_kill();
            tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                if handle
                    .block_on(tokio::time::timeout(DAEMON_REAP_BUDGET, child.wait()))
                    .is_err()
                {
                    eprintln!(
                        "common::BusGuard: the ephemeral dbus-daemon did not exit within \
                         {DAEMON_REAP_BUDGET:?} of SIGKILL; abandoning the reap rather than \
                         hanging the test (#1011)"
                    );
                }
            });
        }
    }
}

/// Write the `dbus-daemon` session config listening on `address` into `tmp`,
/// returning the config file's path. Split out of [`ephemeral_bus`] so
/// [`restart_on_same_address`] (#1175) can reuse the identical config — same
/// address, same policy — for a *second* daemon boot in the same `TempDir`.
fn write_session_conf(tmp: &TempDir, address: &str) -> PathBuf {
    let config = tmp.path().join("session.conf");
    std::fs::write(
        &config,
        format!(
            r#"<?xml version="1.0"?>
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>{address}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
        ),
    )
    .expect("write dbus-daemon config");
    config
}

/// Spawn `dbus-daemon` against `config` and block until it has printed its
/// listen address (proof it is up and accepting connections). Shared by
/// [`ephemeral_bus`] (first boot) and [`restart_on_same_address`] (#1175: a
/// second boot on the identical socket path, standing in for a real
/// system/session `dbus-daemon` restarting under its supervisor while the
/// address it's configured to listen on stays fixed).
async fn spawn_daemon(config: &Path) -> Child {
    let mut child = Command::new("dbus-daemon")
        .arg("--config-file")
        .arg(config)
        .arg("--print-address=1")
        .arg("--nofork")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn dbus-daemon — install package `dbus` if missing");

    // Read the printed address from stdout to confirm the daemon is up.
    let stdout = child.stdout.take().expect("dbus-daemon stdout");
    let mut lines = BufReader::new(stdout).lines();
    let printed = tokio::time::timeout(DBUS_DAEMON_STARTUP_BUDGET, lines.next_line())
        .await
        .expect("dbus-daemon address timeout")
        .expect("dbus-daemon read address")
        .expect("dbus-daemon closed stdout");
    assert!(
        printed.contains("unix:path="),
        "unexpected dbus-daemon address: {printed}"
    );

    child
}

/// Connect to `address`, confirming a daemon is actually listening there.
async fn connect(address: &str) -> Connection {
    Builder::address(address)
        .expect("parse bus address")
        .build()
        .await
        .expect("connect to ephemeral bus")
}

/// Spawn a fresh dbus-daemon, return a connection to it plus a guard
/// that kills the daemon on drop.
pub async fn ephemeral_bus() -> (Connection, BusGuard) {
    let tmp = TempDir::new().expect("create tempdir for dbus-daemon");
    let socket: PathBuf = tmp.path().join("bus");
    let address = format!("unix:path={}", socket.display());

    let config = write_session_conf(&tmp, &address);
    let child = spawn_daemon(&config).await;
    let conn = connect(&address).await;

    (
        conn,
        BusGuard {
            child: Some(child),
            tmp,
            address,
        },
    )
}

/// Kill the `dbus-daemon` behind `guard` and boot a fresh one listening on
/// the **identical** socket path (#1175) — standing in for a real
/// system/session bus restarting under its supervisor while every consumer's
/// configured address stays the same. Any `zbus::Connection` opened against
/// the old daemon (including the one returned by the original
/// [`ephemeral_bus`] call) is now talking to a dead peer and stays that way —
/// this does not, and cannot, reach into an existing `Connection` and fix it
/// up. The `Connection` this returns is a brand new one, freshly dialled
/// against the new daemon; the returned `BusGuard` (same `TempDir`, new
/// child) replaces the caller's old guard, which must not be used again.
#[allow(dead_code)] // not every test binary that pulls in `common` needs a restart
pub async fn restart_on_same_address(mut guard: BusGuard) -> (Connection, BusGuard) {
    if let Some(mut child) = guard.child.take() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    // dbus-daemon doesn't reliably unlink its own socket file on a SIGKILL;
    // remove any stale one so the fresh daemon can bind the identical path
    // instead of failing with EADDRINUSE.
    if let Some(socket_path) = guard.address.strip_prefix("unix:path=") {
        let _ = std::fs::remove_file(socket_path);
    }

    let config = guard.tmp.path().join("session.conf");
    let child = spawn_daemon(&config).await;
    let conn = connect(&guard.address).await;

    guard.child = Some(child);
    (conn, guard)
}
