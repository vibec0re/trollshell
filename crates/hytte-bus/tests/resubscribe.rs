#![cfg(feature = "system-tests")]
//! Reconnect **detection**, and what a consumer can see of it (#1173).
//!
//! Every other reconnect test in this crate drives
//! `simulate_disconnect_for_test`, which clears the cached connection and wakes
//! the supervisor *itself*. That makes it useless for testing detection: it
//! performs by hand exactly the step the code under test is supposed to
//! perform, so deleting `with_conn`'s invalidate-and-notify tail leaves the
//! whole suite green. Nothing in this file calls it (grep: zero hits). Each
//! test here kills a real `dbus-daemon` and requires a primitive to notice on
//! its own.

use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{PropState, property_with};
use std::time::Duration;

// ── An ephemeral broker that can be restarted on the same socket path ────────
//
// A near-copy of `tests/common/mod.rs`, plus `restart_on_same_address`. It is
// duplicated rather than shared because PR #1189 is adding exactly that helper
// to `common/mod.rs` for `connection_reconnect.rs`; this module collapses onto
// it the moment #1189 lands. The doc comments there are the canonical ones —
// in particular the reasoning behind `DBUS_DAEMON_STARTUP_BUDGET`, which is
// sized for worst-case CI contention rather than typical-case latency
// (#676/#678).
mod daemon {
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::{Child, Command};
    use zbus::Connection;
    use zbus::connection::Builder;

    const DBUS_DAEMON_STARTUP_BUDGET: Duration = Duration::from_secs(30);

    pub struct BusGuard {
        child: Option<Child>,
        tmp: TempDir,
        pub address: String,
    }

    impl Drop for BusGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.start_kill();
                tokio::task::block_in_place(|| {
                    let handle = tokio::runtime::Handle::current();
                    let _ = handle.block_on(child.wait());
                });
            }
        }
    }

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

    async fn connect(address: &str) -> Connection {
        Builder::address(address)
            .expect("parse bus address")
            .build()
            .await
            .expect("connect to ephemeral bus")
    }

    /// Spawn a fresh `dbus-daemon` and connect to it.
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

    /// Kill the daemon behind `guard` and boot a fresh one on the **identical**
    /// socket path — a real system/session bus restarting under its supervisor
    /// while every consumer's configured address stays put. Connections opened
    /// against the old daemon are talking to a dead peer and stay that way;
    /// the `Connection` returned here is a brand new one. The returned guard
    /// replaces the caller's, which must not be used again.
    pub async fn restart_on_same_address(mut guard: BusGuard) -> (Connection, BusGuard) {
        if let Some(mut child) = guard.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }

        // dbus-daemon does not reliably unlink its socket on SIGKILL; remove a
        // stale one so the fresh daemon can bind the identical path rather than
        // failing with EADDRINUSE.
        if let Some(socket_path) = guard.address.strip_prefix("unix:path=") {
            let _ = std::fs::remove_file(socket_path);
        }

        let config = guard.tmp.path().join("session.conf");
        let child = spawn_daemon(&config).await;
        let conn = connect(&guard.address).await;

        guard.child = Some(child);
        (conn, guard)
    }
}

// ── A trivial peer to subscribe to ───────────────────────────────────────────

const COUNTER_NAME: &str = "mov.vibec0re.test.Counter";
const COUNTER_PATH: &str = "/mov/vibec0re/test/Counter";

struct Counter {
    value: u32,
}

#[zbus::interface(name = "mov.vibec0re.test.Counter")]
impl Counter {
    #[zbus(property)]
    fn value(&self) -> u32 {
        self.value
    }
}

/// Serve [`Counter`] at [`COUNTER_PATH`] on `address`. Keep the returned
/// connection alive for as long as the interface must stay reachable.
async fn serve_counter(address: &str, value: u32) -> zbus::Connection {
    zbus::connection::Builder::address(address)
        .expect("parse bus address")
        .name(COUNTER_NAME)
        .expect("request Counter well-known name")
        .serve_at(COUNTER_PATH, Counter { value })
        .expect("serve Counter interface")
        .build()
        .await
        .expect("build Counter server connection")
}

fn counter_property(shared: &SharedConnection) -> hytte_bus::PropertySignal<u32> {
    property_with::<u32>(shared, COUNTER_NAME)
        .at_path(COUNTER_PATH)
        .iface(COUNTER_NAME)
        .name("Value")
        .start()
}

/// Poll `stream` until `want` matches, or fail with `what`.
///
/// Deadline-polled with a generous budget rather than a single `timeout`: these
/// tests restart a subprocess and wait on a supervisor, under a CI run that is
/// also building two nixosTest VMs. The budget is a liveness guard, not a
/// latency assertion — same reasoning as `common/mod.rs`'s startup budget
/// (#676/#678).
async fn wait_for<S, F>(stream: &mut S, budget: Duration, what: &str, mut want: F) -> S::Item
where
    S: futures_util::Stream + Unpin,
    F: FnMut(&S::Item) -> bool,
{
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(50), stream.next()).await
            && want(&item)
        {
            return item;
        }
    }
    panic!("timed out waiting for {what}");
}

// ── Item 1: a lone property subscription is enough to drive detection ────────

/// **The headline.** A `property()` subscription, and nothing else, must notice
/// that the bus died and come back when it returns.
///
/// This is the test #1189's design note says cannot pass today, and why: until
/// #1173 `subscribe_properties_changed` pulled the connection out of
/// `with_conn` through an infallible `Ok(conn)` and issued the real `AddMatch`
/// *outside* the closure, so a dead connection produced a retry that failed
/// forever without ever reaching `with_conn`'s error arm. Nothing invalidated
/// the cached connection, the supervisor was never woken, and recovery was
/// parasitic on some *other* primitive on the same bus happening to make a
/// fallible call — which a process whose only use of a bus is `property()`
/// never makes.
///
/// So: no `call()` probe (grep this file), no `simulate_disconnect_for_test`.
/// The only thing this test does by hand is *arm* the replacement connection
/// the supervisor would otherwise open by reading `$DBUS_SESSION_BUS_ADDRESS`
/// — a variable no test here can safely repoint, since mutating it needs
/// `unsafe`, forbidden workspace-wide. Clearing the dead connection and waking
/// the supervisor are left entirely to the code under test.
///
/// The restarted daemon serves a **different** value, so the recovery
/// assertion cannot be satisfied by a replayed `Loaded(42)`: reaching
/// `Loaded(7)` requires a fresh `Get` on a fresh connection.
#[tokio::test(flavor = "multi_thread")]
async fn a_lone_property_subscription_detects_a_dead_daemon_and_recovers() {
    let (conn_a, guard_a) = daemon::ephemeral_bus().await;
    let address = guard_a.address.clone();
    let _server_a = serve_counter(&address, 42).await;

    let shared = SharedConnection::for_test_session(conn_a);
    shared.spawn_supervisor_for_test();

    let prop = counter_property(&shared);
    let mut stream = prop.signal().to_stream();
    wait_for(
        &mut stream,
        Duration::from_secs(20),
        "the initial Loaded(42)",
        |s| matches!(s, PropState::Loaded(42)),
    )
    .await;

    // Kill daemon A and boot a fresh one on the identical socket path. The
    // cached connection now points at a dead peer and nothing has said so.
    let (conn_b, _guard_b) = daemon::restart_on_same_address(guard_a).await;
    let _server_b = serve_counter(&address, 7).await;

    // Arm recovery — and only recovery.
    shared.arm_reconnect_for_test(conn_b);

    wait_for(
        &mut stream,
        Duration::from_secs(60),
        "Loaded(7) after a real detect-and-reconnect",
        |s| matches!(s, PropState::Loaded(7)),
    )
    .await;

    assert!(
        shared.epoch() > 1,
        "the supervisor never installed the armed connection, so nothing \
         detected the dead one"
    );
}
