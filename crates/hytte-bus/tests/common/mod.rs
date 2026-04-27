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
use zbus::connection::Builder;
use zbus::Connection;

pub struct BusGuard {
    child: Option<Child>,
    _tmp: TempDir,
}

impl Drop for BusGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Best-effort SIGKILL; this is test cleanup.
            let _ = child.start_kill();
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
        .spawn()
        .expect("spawn dbus-daemon — install package `dbus` if missing");

    // Read the printed address from stdout to confirm the daemon is up.
    let stdout = child.stdout.take().expect("dbus-daemon stdout");
    let mut lines = BufReader::new(stdout).lines();
    let printed = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
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
        },
    )
}
