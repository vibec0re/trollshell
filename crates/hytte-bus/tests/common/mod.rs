//! Spawn an isolated `dbus-daemon` for one test.
//!
//! Each test gets a fresh broker so tests don't interfere with each other
//! and don't depend on the host's session bus. The daemon is killed when
//! the returned `BusGuard` is dropped.
//!
//! Skips with a clear `panic!("dbus-daemon not on PATH")` if the binary
//! is missing — surface the dependency loudly rather than silently.

use std::path::PathBuf;
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

pub struct BusGuard {
    child: Option<Child>,
    _tmp: TempDir,
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
            let _ = child.start_kill();
            tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                let _ = handle.block_on(child.wait());
            });
        }
    }
}

/// Spawn a fresh dbus-daemon, return a connection to it plus a guard
/// that kills the daemon on drop.
pub async fn ephemeral_bus() -> (Connection, BusGuard) {
    let tmp = TempDir::new().expect("create tempdir for dbus-daemon");
    let socket: PathBuf = tmp.path().join("bus");
    let address = format!("unix:path={}", socket.display());

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

    let mut child = Command::new("dbus-daemon")
        .arg("--config-file")
        .arg(&config)
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

    // Connect once via Builder::address to confirm the daemon is reachable.
    let conn = Builder::address(address.as_str())
        .expect("parse bus address")
        .build()
        .await
        .expect("connect to ephemeral bus");

    (
        conn,
        BusGuard {
            child: Some(child),
            _tmp: tmp,
            address,
        },
    )
}
