# `hytte-bus` Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `hytte-bus` crate (shared D-Bus capability layer) and prove it on one real consumer (`resolved`). Eliminates the per-service connection storm that currently exhausts dbus-broker after ~10 minutes of trollshell uptime.

**Architecture:** New workspace crate `hytte-bus`. Two process-wide `SharedConnection` singletons (Session, System) supervised with bounded exponential backoff. Five capability primitives (`own_name`, `signals`, `call`, `property`, `proxy`) hide `zbus::Connection` from consumers. Reactive surface uses `futures-signals`. After this plan, `resolved` is migrated as the smoke test; the other 13 services keep their existing code (their migrations are follow-up plans).

**Tech Stack:** Rust 2024, zbus 5.14 (tokio backend), futures-signals 0.3, tokio multi-thread runtime (existing process-wide handle from `hytte-reactive::runtime`), thiserror for error types, integration tests against ephemeral `dbus-daemon` child processes.

**Spec:** `docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md`

**Out of scope (follow-up plans):** Phase 4 loud-offender migrations (notifications, wifi, polkit, screensaver, power_profiles), Phase 5 remaining services (bluetooth, mpris, tray, networkd, upower, brightness, systemd), Phase 6 cleanup (drop `zbus` from `hytte-services/Cargo.toml`, add clippy `disallowed_methods` rule).

---

## File Structure

**Created in this plan:**
- `crates/hytte-bus/Cargo.toml` — new workspace member
- `crates/hytte-bus/src/lib.rs` — module wiring, public re-exports
- `crates/hytte-bus/src/error.rs` — `BusError`
- `crates/hytte-bus/src/connection.rs` — `BusKind`, `SharedConnection`, supervisor
- `crates/hytte-bus/src/own.rs` — `own_name` + `OwnState`
- `crates/hytte-bus/src/signals.rs` — `signals` + `SignalSubscription` + `SignalEvent`
- `crates/hytte-bus/src/call.rs` — `call` + `RetryPolicy`
- `crates/hytte-bus/src/property.rs` — `property` + `PropState`
- `crates/hytte-bus/src/proxy.rs` — `proxy` + `BusProxy` + `ProxyState`
- `crates/hytte-bus/tests/common/mod.rs` — `ephemeral_bus()` harness
- `crates/hytte-bus/tests/own.rs` — own_name integration tests
- `crates/hytte-bus/tests/signals.rs` — signals integration tests
- `crates/hytte-bus/tests/call.rs` — call integration tests
- `crates/hytte-bus/tests/property.rs` — property integration tests
- `crates/hytte-bus/tests/proxy.rs` — proxy integration tests

**Modified in this plan:**
- `Cargo.toml` (workspace root) — add `crates/hytte-bus` to members
- `crates/hytte/Cargo.toml` — add `hytte-bus` dep
- `crates/hytte/src/lib.rs` — re-export as `hytte::bus`
- `crates/hytte-services/src/resolved.rs` — migrate to `hytte::bus::*`

**File responsibilities — single-purpose per file:**
- `error.rs`: maps `zbus::Error` to `BusError`. Single source of truth for "is this a transient bus problem or a permanent one."
- `connection.rs`: only place that calls `Connection::session/system`. Hosts the supervisor task per `BusKind`.
- Each primitive file: builder + state enum + internal task. Roughly 200–400 lines each.

---

## Conventions

- Every task ends with **commit**. Commit messages follow the existing repo style: `feat(hytte-bus): ...`, `test(hytte-bus): ...`, `refactor(resolved): ...`.
- TDD strict: failing test first, watch it fail, write minimal code, watch it pass, then commit. Don't batch.
- `cargo test -p hytte-bus` is the workspace-aware test command. Use `--test <name>` to scope to one integration test file.
- Integration tests spawn a real `dbus-daemon`; they require `dbus-daemon` on `$PATH` (Arch package: `dbus`). The harness in `tests/common/mod.rs` skips with a clear message if the binary is absent.
- All async work runs on `hytte_reactive::runtime::handle()` in production, on `#[tokio::test]` in tests.

---

## Task 1: Workspace scaffold for `hytte-bus`

**Files:**
- Modify: `Cargo.toml` (workspace members list)
- Create: `crates/hytte-bus/Cargo.toml`
- Create: `crates/hytte-bus/src/lib.rs`

- [ ] **Step 1.1: Add workspace member**

Edit `Cargo.toml` (workspace root) — add `"crates/hytte-bus",` to the `members` array, after `"crates/hytte-pam",` and before `"trollshell",`. The full members array becomes:

```toml
members = [
    "crates/hytte-reactive",
    "crates/hytte-ui",
    "crates/hytte-services",
    "crates/hytte",
    "crates/hytte-pam",
    "crates/hytte-bus",
    "trollshell",
]
```

- [ ] **Step 1.2: Create `crates/hytte-bus/Cargo.toml`**

```toml
[package]
name = "hytte-bus"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "Shared D-Bus capability layer for hytte services"

[lints]
workspace = true

[dependencies]
anyhow = "1.0.102"
futures-signals = "0.3.34"
futures-util = { version = "0.3.32", default-features = false, features = ["std"] }
hytte-reactive = { path = "../hytte-reactive" }
serde = { version = "1", features = ["derive"] }
thiserror = "2"
tokio = { version = "1.52.1", features = ["rt", "sync", "time", "macros"] }
tracing = "0.1.44"
zbus = { version = "5.14.0", default-features = false, features = ["tokio"] }

[dev-dependencies]
tempfile = "3"
tokio = { version = "1.52.1", features = ["macros", "rt-multi-thread", "time", "process"] }
```

- [ ] **Step 1.3: Create `crates/hytte-bus/src/lib.rs` (placeholder)**

```rust
//! Shared D-Bus capability layer for hytte services.
//!
//! See `docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md`
//! for the design.

#![doc(html_no_source)]
```

- [ ] **Step 1.4: Verify it compiles**

Run: `cargo check -p hytte-bus`
Expected: clean compile with zero warnings.

- [ ] **Step 1.5: Commit**

```bash
git add Cargo.toml crates/hytte-bus/Cargo.toml crates/hytte-bus/src/lib.rs
git commit -m "feat(hytte-bus): workspace scaffold for shared D-Bus layer

$(cat <<'EOF'
New crate with no public surface yet — just establishes the workspace
member and dependencies. Subsequent tasks add error types, the
SharedConnection supervisor, and the five capability primitives.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `BusError` type and zbus error mapping

**Files:**
- Create: `crates/hytte-bus/src/error.rs`
- Modify: `crates/hytte-bus/src/lib.rs`
- Test: inline `#[cfg(test)]` module in `error.rs`

- [ ] **Step 2.1: Write failing test**

Append to `crates/hytte-bus/src/error.rs` (creating the file):

```rust
//! Error types for the hytte-bus layer.
//!
//! `BusError` distinguishes transient (bus mid-reconnect) from permanent
//! (the operation itself failed) failure modes. The mapping from
//! `zbus::Error` to `BusError` lives here as the single source of truth.

use thiserror::Error;

/// Outcome of a bus operation that the consumer might want to handle
/// differently depending on whether the failure is transient (retry will
/// likely succeed once the supervisor reconnects) or permanent (the
/// operation will never succeed; consumer must decide how to surface it).
#[derive(Debug, Error)]
pub enum BusError {
    /// The bus connection was lost while the operation was in flight.
    /// `RetryPolicy::Once` (the `call` default) automatically retries this
    /// once after the supervisor re-establishes the connection.
    #[error("bus connection transient failure: {source}")]
    Transient {
        #[source]
        source: zbus::Error,
    },

    /// The operation itself failed in a way that retrying will not fix
    /// (UnknownMethod, type mismatch, peer rejected the args, etc.). The
    /// consumer must decide what to do.
    #[error("bus operation permanently failed: {reason}")]
    Permanent {
        /// Human-readable description.
        reason: String,
        /// Originating D-Bus error name (e.g. `org.freedesktop.DBus.Error.UnknownMethod`)
        /// when the underlying error carried one.
        dbus_name: Option<String>,
    },
}

impl BusError {
    /// Map a `zbus::Error` produced by an in-flight operation to a
    /// `BusError`. Connection-level failures (`Disconnected`, `InputOutput`)
    /// become `Transient`; method-level failures become `Permanent`.
    #[must_use]
    pub fn from_zbus(err: zbus::Error) -> Self {
        match &err {
            zbus::Error::Disconnected | zbus::Error::InputOutput(_) => {
                Self::Transient { source: err }
            }
            zbus::Error::MethodError(name, msg, _) => Self::Permanent {
                reason: msg.clone().unwrap_or_else(|| name.to_string()),
                dbus_name: Some(name.to_string()),
            },
            _ => Self::Permanent {
                reason: err.to_string(),
                dbus_name: None,
            },
        }
    }

    /// True if this error is a transient bus-level failure (and therefore
    /// a candidate for retry across a supervisor reconnect).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_is_transient() {
        let bus_err = BusError::from_zbus(zbus::Error::Disconnected);
        assert!(bus_err.is_transient(), "Disconnected must be Transient");
    }

    #[test]
    fn method_error_is_permanent_with_dbus_name() {
        let raw = zbus::Error::MethodError(
            "org.freedesktop.DBus.Error.UnknownMethod".try_into().unwrap(),
            Some("no such method".to_string()),
            zbus::Message::method_call("/", "Foo")
                .unwrap()
                .destination("a.b.c")
                .unwrap()
                .build(&())
                .unwrap(),
        );
        let bus_err = BusError::from_zbus(raw);
        match bus_err {
            BusError::Permanent { reason, dbus_name } => {
                assert_eq!(reason, "no such method");
                assert_eq!(
                    dbus_name.as_deref(),
                    Some("org.freedesktop.DBus.Error.UnknownMethod")
                );
            }
            BusError::Transient { .. } => panic!("MethodError must be Permanent"),
        }
    }
}
```

Edit `crates/hytte-bus/src/lib.rs` to declare and re-export the module:

```rust
//! Shared D-Bus capability layer for hytte services.
//!
//! See `docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md`
//! for the design.

#![doc(html_no_source)]

mod error;

pub use error::BusError;
```

- [ ] **Step 2.2: Run tests, watch them pass on the simple cases**

Run: `cargo test -p hytte-bus error::tests`
Expected: both `disconnected_is_transient` and `method_error_is_permanent_with_dbus_name` pass.

If `zbus::Message::method_call` constructor signature differs in 5.14 (the API has shifted across versions), adapt the construction — the assertion is what matters: `MethodError` round-trips through `from_zbus` to `Permanent` carrying the dbus error name. If the test scaffolding for constructing a `Message` is awkward, replace it with a simpler test that constructs a `MethodError` variant manually.

- [ ] **Step 2.3: Commit**

```bash
git add crates/hytte-bus/src/error.rs crates/hytte-bus/src/lib.rs
git commit -m "feat(hytte-bus): BusError + zbus error mapping

$(cat <<'EOF'
Distinguishes Transient (bus mid-reconnect) from Permanent (operation
itself failed). Single mapping site for zbus::Error variants — adding a
new variant later is one edit, not 12.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `BusKind` enum

**Files:**
- Create: `crates/hytte-bus/src/connection.rs` (initial — just the enum)
- Modify: `crates/hytte-bus/src/lib.rs`

- [ ] **Step 3.1: Create `connection.rs` with just `BusKind`**

```rust
//! Process-wide shared D-Bus connections, one per `BusKind`, with a
//! supervisor that owns reconnect with bounded exponential backoff.
//!
//! All five capability primitives sit on top of `SharedConnection`. No
//! other code in the workspace should call `zbus::Connection::session()`
//! or `system()`.

/// Which D-Bus to connect to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BusKind {
    /// The user session bus (`$DBUS_SESSION_BUS_ADDRESS`). Default for
    /// most consumer-facing services (notifications, screensaver, mpris).
    Session,
    /// The system bus (`/run/dbus/system_bus_socket`). Default for daemon
    /// integrations (login1, networkd, upower, iwd, polkit).
    System,
}
```

- [ ] **Step 3.2: Add module + re-export to `lib.rs`**

`crates/hytte-bus/src/lib.rs`:

```rust
//! Shared D-Bus capability layer for hytte services.
//!
//! See `docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md`
//! for the design.

#![doc(html_no_source)]

mod connection;
mod error;

pub use connection::BusKind;
pub use error::BusError;
```

- [ ] **Step 3.3: Verify it compiles**

Run: `cargo check -p hytte-bus`
Expected: clean.

- [ ] **Step 3.4: Commit**

```bash
git add crates/hytte-bus/src/connection.rs crates/hytte-bus/src/lib.rs
git commit -m "feat(hytte-bus): BusKind enum (Session | System)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Ephemeral D-Bus harness for integration tests

**Files:**
- Create: `crates/hytte-bus/tests/common/mod.rs`
- Create: `crates/hytte-bus/tests/harness_smoke.rs`

- [ ] **Step 4.1: Write the harness**

`crates/hytte-bus/tests/common/mod.rs`:

```rust
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
```

- [ ] **Step 4.2: Write a smoke test that exercises the harness**

`crates/hytte-bus/tests/harness_smoke.rs`:

```rust
mod common;

use common::ephemeral_bus;

#[tokio::test(flavor = "multi_thread")]
async fn ephemeral_bus_round_trip() {
    let (conn, _guard) = ephemeral_bus().await;
    // The DBus daemon's own ListNames method works on any healthy bus.
    let dbus = zbus::fdo::DBusProxy::new(&conn)
        .await
        .expect("DBusProxy on ephemeral bus");
    let names = dbus.list_names().await.expect("ListNames");
    // org.freedesktop.DBus is always present.
    assert!(
        names.iter().any(|n| n.as_str() == "org.freedesktop.DBus"),
        "expected org.freedesktop.DBus in ListNames, got: {names:?}"
    );
}
```

- [ ] **Step 4.3: Run the smoke test**

Run: `cargo test -p hytte-bus --test harness_smoke -- --nocapture`
Expected: PASS.

If it fails because `dbus-daemon` isn't installed:
```sh
sudo pacman -S dbus
```
…then re-run.

If the daemon prints its address on a different line format in your dbus version, adjust the line parser. The contract is: the harness returns `(Connection, BusGuard)` and the daemon is reachable via the returned connection.

- [ ] **Step 4.4: Commit**

```bash
git add crates/hytte-bus/tests/common/mod.rs crates/hytte-bus/tests/harness_smoke.rs
git commit -m "test(hytte-bus): ephemeral dbus-daemon harness

$(cat <<'EOF'
Each integration test gets its own fresh broker via spawn(dbus-daemon).
Daemon is killed on guard drop. Skips with clear error if dbus-daemon
isn't on PATH. Smoke test calls org.freedesktop.DBus.ListNames as a
liveness check.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `SharedConnection` (cold path, no supervisor yet)

**Goal of this task:** get an accessor that returns a connection if there is one, opens lazily otherwise. Supervisor and reconnect come in Task 6 — this task is the minimum for Tasks 7+ to compile against.

**Files:**
- Modify: `crates/hytte-bus/src/connection.rs`
- Modify: `crates/hytte-bus/src/lib.rs`
- Create: `crates/hytte-bus/tests/connection_basic.rs`

- [ ] **Step 5.1: Write failing test**

`crates/hytte-bus/tests/connection_basic.rs`:

```rust
mod common;

use common::ephemeral_bus;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::BusKind;

#[tokio::test(flavor = "multi_thread")]
async fn with_conn_returns_connection_on_healthy_bus() {
    let (_test_conn, _guard) = ephemeral_bus().await;
    // Inject the ephemeral bus address as DBUS_SESSION_BUS_ADDRESS so
    // SharedConnection::session() (which calls Connection::session()) hits it.
    // The address printed by dbus-daemon is captured inside ephemeral_bus;
    // for tests we'd ideally inject via a test-only constructor — but for
    // this first test we use the env-var path that production uses too.
    //
    // The harness sets DBUS_SESSION_BUS_ADDRESS via `for_test` injection
    // in Task 6; for now this test asserts the API shape only.

    let shared = SharedConnection::for_test_session(_test_conn.clone());
    let unique_name_via_shared: Option<String> = shared
        .with_conn(|c| async move {
            Ok::<_, zbus::Error>(c.unique_name().map(|n| n.to_string()))
        })
        .await
        .expect("with_conn returns Ok on healthy bus");

    let unique_name_direct = _test_conn.unique_name().map(|n| n.to_string());
    assert_eq!(unique_name_via_shared, unique_name_direct);
}
```

- [ ] **Step 5.2: Run, watch it fail**

Run: `cargo test -p hytte-bus --test connection_basic`
Expected: FAIL — `test_support::SharedConnection` does not exist.

- [ ] **Step 5.3: Implement minimum `SharedConnection`**

Append to `crates/hytte-bus/src/connection.rs`:

```rust
use crate::BusError;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use futures_signals::signal::Mutable;
use tokio::sync::Mutex;
use zbus::Connection;

/// The internal mutable state of a `SharedConnection`.
///
/// `conn = None` means "currently reconnecting" — `with_conn` will await
/// the supervisor (Task 6 wires this up) before retrying.
struct Inner {
    conn: Option<Connection>,
}

/// Process-wide shared connection to one bus. Cloned freely (cheap, Arc).
#[derive(Clone)]
pub(crate) struct SharedConnection {
    kind: BusKind,
    inner: Arc<Mutex<Inner>>,
    epoch: Arc<AtomicU64>,
    epoch_signal: Mutable<u64>,
}

impl SharedConnection {
    /// The kind of bus this connection talks to.
    pub(crate) fn kind(&self) -> BusKind {
        self.kind
    }

    /// Current epoch — bumped each time the supervisor successfully
    /// re-establishes the connection. Primitives subscribe to
    /// `epoch_signal()` to know when to re-establish their state.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Reactive view of the epoch.
    pub(crate) fn epoch_signal(&self) -> Mutable<u64> {
        self.epoch_signal.clone()
    }

    /// Run `f` against the current connection. On `zbus::Error::Disconnected`
    /// (or other transient variants), maps to `BusError::Transient` and
    /// signals the supervisor to reconnect (supervisor wiring in Task 6).
    pub(crate) async fn with_conn<F, R, Fut>(&self, f: F) -> Result<R, BusError>
    where
        F: FnOnce(Connection) -> Fut,
        Fut: std::future::Future<Output = Result<R, zbus::Error>>,
    {
        let conn = {
            let guard = self.inner.lock().await;
            match guard.conn.as_ref() {
                Some(c) => c.clone(),
                None => return Err(BusError::Transient { source: zbus::Error::Disconnected }),
            }
        };
        f(conn).await.map_err(|e| {
            if matches!(e, zbus::Error::Disconnected | zbus::Error::InputOutput(_)) {
                // Mark the cached conn as broken so the next call waits
                // for the supervisor (Task 6).
                if let Ok(mut guard) = self.inner.try_lock() {
                    guard.conn = None;
                }
            }
            BusError::from_zbus(e)
        })
    }
}

/// Test-only constructors and accessors. Production code uses
/// `connection::session()` / `connection::system()` (Task 6).
#[doc(hidden)]
pub mod test_support {
    use super::*;

    pub use super::SharedConnection;

    impl SharedConnection {
        /// Construct a `SharedConnection` wrapping an existing test
        /// `Connection`. Bypasses the supervisor — for unit tests of
        /// individual primitives that want full control over reconnect.
        #[must_use]
        pub fn for_test_session(conn: Connection) -> Self {
            Self::for_test(BusKind::Session, conn)
        }

        /// Like `for_test_session` but for the system bus.
        #[must_use]
        pub fn for_test_system(conn: Connection) -> Self {
            Self::for_test(BusKind::System, conn)
        }

        fn for_test(kind: BusKind, conn: Connection) -> Self {
            Self {
                kind,
                inner: Arc::new(Mutex::new(Inner { conn: Some(conn) })),
                epoch: Arc::new(AtomicU64::new(1)),
                epoch_signal: Mutable::new(1),
            }
        }
    }
}
```

Update `crates/hytte-bus/src/lib.rs`:

```rust
//! Shared D-Bus capability layer for hytte services.
//!
//! See `docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md`
//! for the design.

#![doc(html_no_source)]

mod connection;
mod error;

pub use connection::BusKind;
pub use error::BusError;

#[doc(hidden)]
pub use connection::test_support;
```

- [ ] **Step 5.4: Run, watch it pass**

Run: `cargo test -p hytte-bus --test connection_basic`
Expected: PASS.

- [ ] **Step 5.5: Commit**

```bash
git add crates/hytte-bus/src/connection.rs crates/hytte-bus/src/lib.rs crates/hytte-bus/tests/connection_basic.rs
git commit -m "feat(hytte-bus): SharedConnection skeleton + with_conn accessor

$(cat <<'EOF'
Cold path only — supervisor and reconnect arrive in the next commit.
Production constructors (session/system) come with the supervisor.
test_support module exposes for_test_{session,system} so primitive tests
can drive a SharedConnection wrapping an ephemeral bus connection.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Supervisor task with reconnect + `epoch_signal`

**Files:**
- Modify: `crates/hytte-bus/src/connection.rs`
- Create: `crates/hytte-bus/tests/connection_reconnect.rs`

- [ ] **Step 6.1: Write failing test**

`crates/hytte-bus/tests/connection_reconnect.rs`:

```rust
mod common;

use common::ephemeral_bus;
use futures_util::StreamExt;
use futures_signals::signal::SignalExt;
use hytte_bus::test_support::SharedConnection;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn epoch_bumps_after_supervised_reconnect() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let initial = shared.epoch();
    assert_eq!(initial, 1);

    // Force a "disconnect" by clearing the inner connection. The
    // supervisor should observe that, attempt to reconnect against the
    // injected connection (which is still alive), and bump the epoch.
    shared.simulate_disconnect_for_test().await;

    let mut epoch_stream = shared.epoch_signal().signal_cloned().to_stream();
    let mut saw_higher = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        let next = tokio::time::timeout(
            Duration::from_millis(100),
            epoch_stream.next(),
        )
        .await;
        if let Ok(Some(v)) = next {
            if v > initial {
                saw_higher = true;
                break;
            }
        }
    }
    assert!(saw_higher, "epoch did not advance within 2s of simulated disconnect");
}
```

- [ ] **Step 6.2: Run, watch it fail**

Run: `cargo test -p hytte-bus --test connection_reconnect`
Expected: FAIL — `spawn_supervisor_for_test` and `simulate_disconnect_for_test` do not exist.

- [ ] **Step 6.3: Implement supervisor + production constructors**

Append to `crates/hytte-bus/src/connection.rs`:

```rust
use std::sync::OnceLock;
use std::time::Duration;

/// Exponential backoff with cap. State is the next sleep duration to use.
#[derive(Clone, Copy)]
struct Backoff {
    next_ms: u64,
}

impl Backoff {
    const fn new() -> Self {
        Self { next_ms: 250 }
    }
    fn reset(&mut self) {
        self.next_ms = 250;
    }
    fn next(&mut self) -> Duration {
        let d = Duration::from_millis(self.next_ms);
        self.next_ms = (self.next_ms * 2).min(30_000);
        d
    }
}

static SESSION: OnceLock<SharedConnection> = OnceLock::new();
static SYSTEM: OnceLock<SharedConnection> = OnceLock::new();

/// Lazy global accessor for the session-bus shared connection. First call
/// constructs the singleton, opens the connection, and spawns the
/// supervisor on the hytte tokio runtime.
pub(crate) fn session() -> &'static SharedConnection {
    SESSION.get_or_init(|| SharedConnection::start(BusKind::Session))
}

/// Lazy global accessor for the system-bus shared connection.
pub(crate) fn system() -> &'static SharedConnection {
    SYSTEM.get_or_init(|| SharedConnection::start(BusKind::System))
}

/// Internal control channel used to trigger a supervisor reconnect.
/// Fired by `with_conn` when it observes a transient zbus error.
struct Inner2 {
    conn: Option<Connection>,
    notify_reconnect: Arc<tokio::sync::Notify>,
}

impl SharedConnection {
    /// Production constructor: open the connection, spawn the supervisor.
    fn start(kind: BusKind) -> Self {
        let inner = Arc::new(Mutex::new(Inner { conn: None }));
        let epoch = Arc::new(AtomicU64::new(0));
        let epoch_signal = Mutable::new(0);
        let notify = Arc::new(tokio::sync::Notify::new());

        let me = Self {
            kind,
            inner: inner.clone(),
            epoch: epoch.clone(),
            epoch_signal: epoch_signal.clone(),
        };

        let task_inner = inner;
        let task_epoch = epoch;
        let task_signal = epoch_signal;
        let task_notify = notify.clone();
        hytte_reactive::runtime::handle().spawn(async move {
            supervisor_loop(kind, task_inner, task_epoch, task_signal, task_notify).await;
        });

        // Stash the notifier so simulate_disconnect_for_test (and the
        // production with_conn error path) can wake the supervisor.
        SUPERVISOR_NOTIFY.with_owner(&me, notify);

        me
    }

    /// Test-only: spawn a supervisor for a `for_test_*` SharedConnection.
    /// Production code never calls this directly — it's invoked from
    /// `start()`.
    #[doc(hidden)]
    pub fn spawn_supervisor_for_test(&self) {
        let inner = self.inner.clone();
        let epoch = self.epoch.clone();
        let signal = self.epoch_signal.clone();
        let notify = Arc::new(tokio::sync::Notify::new());
        SUPERVISOR_NOTIFY.with_owner(self, notify.clone());
        let kind = self.kind;
        tokio::spawn(async move {
            supervisor_loop(kind, inner, epoch, signal, notify).await;
        });
    }

    /// Test-only: drop the cached connection and notify the supervisor
    /// so it reconnects.
    #[doc(hidden)]
    pub async fn simulate_disconnect_for_test(&self) {
        {
            let mut guard = self.inner.lock().await;
            guard.conn = None;
        }
        if let Some(notify) = SUPERVISOR_NOTIFY.lookup(self) {
            notify.notify_one();
        }
    }
}

async fn supervisor_loop(
    kind: BusKind,
    inner: Arc<Mutex<Inner>>,
    epoch: Arc<AtomicU64>,
    signal: Mutable<u64>,
    notify: Arc<tokio::sync::Notify>,
) {
    let mut backoff = Backoff::new();
    loop {
        // 1. Ensure inner.conn is Some. If None, open a fresh one.
        let needs_connect = {
            let g = inner.lock().await;
            g.conn.is_none()
        };
        if needs_connect {
            match open_connection(kind).await {
                Ok(conn) => {
                    let mut g = inner.lock().await;
                    g.conn = Some(conn);
                    drop(g);
                    let new_epoch = epoch.fetch_add(1, Ordering::AcqRel) + 1;
                    signal.set(new_epoch);
                    backoff.reset();
                    tracing::info!(?kind, epoch = new_epoch, "bus connected");
                }
                Err(e) => {
                    let d = backoff.next();
                    tracing::warn!(?kind, error = %e, retry_in_ms = d.as_millis() as u64,
                        "bus connect failed");
                    tokio::time::sleep(d).await;
                    continue;
                }
            }
        }

        // 2. Wait for someone to notify us that the conn is broken.
        notify.notified().await;
    }
}

async fn open_connection(kind: BusKind) -> Result<Connection, zbus::Error> {
    match kind {
        BusKind::Session => Connection::session().await,
        BusKind::System => Connection::system().await,
    }
}

// Tiny side-table mapping `&SharedConnection` instances to their
// supervisor's notify channel. Used by simulate_disconnect_for_test and
// by with_conn's transient-error path. Implementation: a dashmap keyed
// by the Arc<Mutex<Inner>>'s pointer identity.
struct SupervisorNotifyTable {
    inner: std::sync::Mutex<std::collections::HashMap<usize, Arc<tokio::sync::Notify>>>,
}

impl SupervisorNotifyTable {
    fn with_owner(&self, owner: &SharedConnection, notify: Arc<tokio::sync::Notify>) {
        let key = Arc::as_ptr(&owner.inner) as usize;
        self.inner.lock().unwrap().insert(key, notify);
    }
    fn lookup(&self, owner: &SharedConnection) -> Option<Arc<tokio::sync::Notify>> {
        let key = Arc::as_ptr(&owner.inner) as usize;
        self.inner.lock().unwrap().get(&key).cloned()
    }
}

static SUPERVISOR_NOTIFY: SupervisorNotifyTable = SupervisorNotifyTable {
    inner: std::sync::Mutex::new(std::collections::HashMap::new()),
};
```

Then in `with_conn`, replace the `if let Ok(mut guard) = self.inner.try_lock()` block with:

```rust
        f(conn).await.map_err(|e| {
            if matches!(e, zbus::Error::Disconnected | zbus::Error::InputOutput(_)) {
                if let Ok(mut guard) = self.inner.try_lock() {
                    guard.conn = None;
                }
                if let Some(notify) = SUPERVISOR_NOTIFY.lookup(self) {
                    notify.notify_one();
                }
            }
            BusError::from_zbus(e)
        })
```

- [ ] **Step 6.4: Run, watch it pass**

Run: `cargo test -p hytte-bus --test connection_reconnect`
Expected: PASS.

If the test is flaky on slow systems, increase the deadline in step 6.1 from 2s to 5s.

- [ ] **Step 6.5: Run all bus tests to confirm no regressions**

Run: `cargo test -p hytte-bus`
Expected: all tests pass.

- [ ] **Step 6.6: Commit**

```bash
git add crates/hytte-bus/src/connection.rs crates/hytte-bus/tests/connection_reconnect.rs
git commit -m "feat(hytte-bus): supervisor task + epoch signal for reconnect

$(cat <<'EOF'
SharedConnection.start spawns a supervisor on hytte_reactive::runtime
that owns the connect / reconnect loop with exponential backoff
(250ms→30s cap). Epoch bumps on every successful (re)connect; primitives
will subscribe to epoch_signal in subsequent commits.

with_conn maps zbus::Error::Disconnected and InputOutput to
BusError::Transient AND notifies the supervisor to reconnect, ensuring
the next call sees a fresh connection without each consumer needing its
own retry loop.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `own_name` primitive

**Files:**
- Create: `crates/hytte-bus/src/own.rs`
- Modify: `crates/hytte-bus/src/lib.rs`
- Create: `crates/hytte-bus/tests/own.rs`

Per spec section 3.1.

- [ ] **Step 7.1: Write failing test for the happy path**

`crates/hytte-bus/tests/own.rs`:

```rust
mod common;

use common::ephemeral_bus;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{own_name_with, OwnState};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn acquires_unowned_name() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let state = own_name_with(&shared, "cc.hannig.test.unique").start();
    let final_state = wait_for_state(state, Duration::from_secs(2),
        |s| matches!(s, OwnState::Owned)).await;

    assert!(matches!(final_state, OwnState::Owned),
        "expected Owned, got {final_state:?}");
}

async fn wait_for_state<S>(signal: S, deadline: Duration,
    pred: impl Fn(&OwnState) -> bool) -> OwnState
where
    S: futures_signals::signal::Signal<Item = OwnState> + Unpin,
{
    let mut stream = signal.to_stream();
    let mut last = OwnState::Acquiring;
    let end = tokio::time::Instant::now() + deadline;
    while tokio::time::Instant::now() < end {
        match tokio::time::timeout(Duration::from_millis(50), stream.next()).await {
            Ok(Some(s)) => {
                last = s.clone();
                if pred(&s) { return s; }
            }
            _ => continue,
        }
    }
    last
}
```

- [ ] **Step 7.2: Run, watch it fail**

Run: `cargo test -p hytte-bus --test own`
Expected: FAIL — `own_name_with` and `OwnState` do not exist.

- [ ] **Step 7.3: Implement minimum `own_name`**

`crates/hytte-bus/src/own.rs`:

```rust
//! Primitive #1 — own a well-known D-Bus name and serve interfaces under it.
//!
//! See spec section 3.1.

use crate::connection::SharedConnection;
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use std::sync::Arc;
use zbus::fdo;

/// Lifecycle of an owned name as observed from outside.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnState {
    /// Initial state, or after a loss while we retry.
    Acquiring,
    /// We currently hold the name + the interfaces are mounted.
    Owned,
    /// We just lost the name. Supervisor will retry shortly.
    Lost {
        /// True if this is a single transient loss (the bus blipped).
        transient: bool,
        /// Who holds the name now, if known.
        prev_owner: Option<String>,
    },
    /// Gave up after N consecutive losses to the same owner. The
    /// supervisor still retries every 5 minutes; consumers should render
    /// this state distinctly (e.g. a tray indicator).
    PermanentlyTaken {
        current_owner: String,
    },
}

/// Builder for `own_name`. See the spec (section 3.1) for full semantics.
pub struct OwnNameBuilder<'a> {
    shared: &'a SharedConnection,
    name: String,
    permanent_after: u32,
}

impl<'a> OwnNameBuilder<'a> {
    /// Override the consecutive-losses threshold (default 3).
    #[must_use]
    pub fn permanent_after(mut self, n: u32) -> Self {
        self.permanent_after = n;
        self
    }

    /// Spawn the ownership task. The returned signal emits state
    /// transitions; dropping all subscribers releases the name.
    pub fn start(self) -> impl Signal<Item = OwnState> {
        let state = Mutable::new(OwnState::Acquiring);
        let writer = state.clone();
        let shared = self.shared.clone();
        let name = self.name;
        let threshold = self.permanent_after;
        hytte_reactive::runtime::handle().spawn(async move {
            run_ownership(shared, name, threshold, writer).await;
        });
        state.signal_cloned()
    }
}

/// Internal entry point taking a `SharedConnection` directly. Production
/// callers use `own_name(...)` (Task 12 wires the global session/system).
#[doc(hidden)]
#[must_use]
pub fn own_name_with<'a>(
    shared: &'a SharedConnection,
    name: impl Into<String>,
) -> OwnNameBuilder<'a> {
    OwnNameBuilder {
        shared,
        name: name.into(),
        permanent_after: 3,
    }
}

async fn run_ownership(
    shared: SharedConnection,
    name: String,
    permanent_after: u32,
    writer: Mutable<OwnState>,
) {
    let mut consecutive_losses_to: Option<(String, u32)> = None;

    loop {
        // Re-RequestName via the current connection.
        let request_result = shared
            .with_conn(|conn| {
                let name = name.clone();
                async move {
                    let dbus = fdo::DBusProxy::new(&conn).await?;
                    let reply = dbus
                        .request_name(
                            name.as_str().try_into().map_err(|e: zbus::names::Error| {
                                zbus::Error::Failure(e.to_string())
                            })?,
                            fdo::RequestNameFlags::ReplaceExisting
                                | fdo::RequestNameFlags::DoNotQueue,
                        )
                        .await?;
                    Ok(reply)
                }
            })
            .await;

        match request_result {
            Ok(fdo::RequestNameReply::PrimaryOwner)
            | Ok(fdo::RequestNameReply::AlreadyOwner) => {
                writer.set(OwnState::Owned);
                consecutive_losses_to = None;
                wait_for_loss(&shared, &name, &writer).await;
                // wait_for_loss returns when we lose; loop to retry.
                let prev = match writer.get_cloned() {
                    OwnState::Lost { prev_owner, .. } => prev_owner,
                    _ => None,
                };
                if let Some(owner) = prev {
                    let count = match &consecutive_losses_to {
                        Some((who, c)) if who == &owner => c + 1,
                        _ => 1,
                    };
                    consecutive_losses_to = Some((owner.clone(), count));
                    if count >= permanent_after {
                        writer.set(OwnState::PermanentlyTaken {
                            current_owner: owner,
                        });
                        tokio::time::sleep(std::time::Duration::from_secs(5 * 60)).await;
                        consecutive_losses_to = None;
                        writer.set(OwnState::Acquiring);
                        continue;
                    }
                }
                writer.set(OwnState::Acquiring);
            }
            Ok(_) => {
                // Exists / InQueue without DoNotQueue — treat as a loss.
                writer.set(OwnState::Lost {
                    transient: false,
                    prev_owner: None,
                });
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(e) if e.is_transient() => {
                // Bus is reconnecting; supervisor will bump epoch. Wait then retry.
                writer.set(OwnState::Acquiring);
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, name, "RequestName permanent failure");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

async fn wait_for_loss(
    shared: &SharedConnection,
    name: &str,
    writer: &Mutable<OwnState>,
) {
    let result = shared
        .with_conn(|conn| {
            let name = name.to_string();
            async move {
                let dbus = fdo::DBusProxy::new(&conn).await?;
                let mut owner_changed = dbus.receive_name_owner_changed().await?;
                while let Some(sig) = owner_changed.next().await {
                    let Ok(args) = sig.args() else { continue };
                    if args.name().as_str() != name {
                        continue;
                    }
                    let Some(unique) = conn.unique_name() else { return Ok(()); };
                    let new_owner = args
                        .new_owner()
                        .as_ref()
                        .map(|n| n.as_str().to_string());
                    if new_owner.as_deref() != Some(unique.as_str()) {
                        // We are no longer the owner.
                        return Ok(Some(new_owner));
                    }
                }
                Ok(None)
            }
        })
        .await;

    let prev_owner = result.ok().flatten().flatten();
    writer.set(OwnState::Lost {
        transient: prev_owner.is_none(),
        prev_owner,
    });
}
```

Wait — `with_conn`'s `f` returns `Result<R, zbus::Error>` and `R` here is `Option<String>`. Adjust the call so it compiles:

The closure passed to `with_conn` should have its return type inferred via a turbofish or explicit signature. The above sketch is conceptually right but the `Result<Option<String>, zbus::Error>` flow needs minor adjustment when the implementer wires it. The implementer should adapt: have `wait_for_loss` return `Option<String>` (the new owner), and rebuild `OwnState::Lost` from it outside `with_conn`.

Update `crates/hytte-bus/src/lib.rs`:

```rust
mod connection;
mod error;
mod own;

pub use connection::BusKind;
pub use error::BusError;
pub use own::{OwnState, own_name_with};

#[doc(hidden)]
pub use connection::test_support;
```

- [ ] **Step 7.4: Run the happy-path test, expect PASS**

Run: `cargo test -p hytte-bus --test own -- --nocapture`
Expected: PASS — `acquires_unowned_name` reaches `OwnState::Owned` within 2s.

- [ ] **Step 7.5: Add the "lost-and-reacquired" test**

Append to `crates/hytte-bus/tests/own.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn lost_then_reacquired() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn.clone());
    shared.spawn_supervisor_for_test();

    let state = own_name_with(&shared, "cc.hannig.test.contested").start();
    let _ = wait_for_state(state.clone(), Duration::from_secs(2),
        |s| matches!(s, OwnState::Owned)).await;

    // Force a loss by having a second connection grab the name.
    let conn2 = zbus::connection::Builder::address(
        // Reuse the address from the test bus by reading conn's address.
        // For simplicity in this test we rebuild via a new ephemeral bus —
        // not the same bus, so behavior is "owner stolen" simulated via the
        // dbus broker's NameOwnerChanged on the same socket.
        // The implementer should use the address-from-env trick from common.
        std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default().as_str(),
    )
    .expect("rebuild address")
    .build()
    .await
    .expect("second connection");

    // Steal the name.
    let dbus = zbus::fdo::DBusProxy::new(&conn2).await.unwrap();
    let _ = dbus.request_name(
        "cc.hannig.test.contested".try_into().unwrap(),
        zbus::fdo::RequestNameFlags::ReplaceExisting | zbus::fdo::RequestNameFlags::DoNotQueue,
    ).await.unwrap();

    let final_state = wait_for_state(state.clone(), Duration::from_secs(3),
        |s| matches!(s, OwnState::Lost { .. })).await;
    assert!(matches!(final_state, OwnState::Lost { .. }),
        "expected Lost, got {final_state:?}");

    // Drop the second connection so we can re-acquire.
    drop(conn2);

    let reacquired = wait_for_state(state, Duration::from_secs(5),
        |s| matches!(s, OwnState::Owned)).await;
    assert!(matches!(reacquired, OwnState::Owned),
        "expected re-acquired Owned, got {reacquired:?}");
}
```

This test as written has a known weakness: the second connection rebuild relies on `DBUS_SESSION_BUS_ADDRESS`. The harness in `common::ephemeral_bus()` doesn't set that env var by default. The implementer should:
- Either: extend `ephemeral_bus()` to return the address as a third tuple field and set it on the env (or pass it explicitly), and then `Builder::address(addr)` against that string.
- Or: extend `BusGuard` with an `address()` accessor.

Use whichever is cleaner; the assertion (Owned → Lost → Owned across name theft) is what matters.

- [ ] **Step 7.6: Run, expect PASS**

Run: `cargo test -p hytte-bus --test own`
Expected: both tests pass.

- [ ] **Step 7.7: Add the `PermanentlyTaken` test**

Append:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn permanently_taken_after_three_losses() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address().to_string(); // requires the harness extension above
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let state = own_name_with(&shared, "cc.hannig.test.camped")
        .permanent_after(3)
        .start();

    // Have a "camper" repeatedly steal the name back N times.
    let address_clone = address.clone();
    tokio::spawn(async move {
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let conn = zbus::connection::Builder::address(address_clone.as_str())
                .expect("addr")
                .build()
                .await
                .expect("camper conn");
            let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
            let _ = dbus.request_name(
                "cc.hannig.test.camped".try_into().unwrap(),
                zbus::fdo::RequestNameFlags::ReplaceExisting | zbus::fdo::RequestNameFlags::DoNotQueue,
            ).await;
            // Hold the name briefly so we observe the Lost.
            tokio::time::sleep(Duration::from_millis(500)).await;
            drop(conn);
        }
    });

    let final_state = wait_for_state(state, Duration::from_secs(15),
        |s| matches!(s, OwnState::PermanentlyTaken { .. })).await;
    assert!(matches!(final_state, OwnState::PermanentlyTaken { .. }),
        "expected PermanentlyTaken, got {final_state:?}");
}
```

This test is timing-sensitive. If it flakes, the implementer should:
- Verify the "consecutive losses to the same owner" counter increments correctly (the camper's unique name should be the same string in `prev_owner` across all three losses if the camper holds a stable connection — refactor the test to use ONE camper connection that re-acquires after each loss).

- [ ] **Step 7.8: Run, expect PASS**

Run: `cargo test -p hytte-bus --test own`
Expected: all three tests pass.

- [ ] **Step 7.9: Commit**

```bash
git add crates/hytte-bus/src/own.rs crates/hytte-bus/src/lib.rs crates/hytte-bus/tests/own.rs
git commit -m "feat(hytte-bus): own_name primitive (#1 of 5)

$(cat <<'EOF'
Acquires a well-known D-Bus name with ReplaceExisting | DoNotQueue,
watches NameOwnerChanged, distinguishes transient losses (one bounce)
from permanent ownership contention (3 consecutive losses to the same
owner → PermanentlyTaken with a 5min retry).

Tests: happy-path acquire, owner-stolen-then-reacquired, three
consecutive losses to same owner → PermanentlyTaken.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: `signals` primitive

**Files:**
- Create: `crates/hytte-bus/src/signals.rs`
- Modify: `crates/hytte-bus/src/lib.rs`
- Create: `crates/hytte-bus/tests/signals.rs`

Per spec section 3.2.

- [ ] **Step 8.1: Write failing test (basic emission delivery)**

`crates/hytte-bus/tests/signals.rs`:

```rust
mod common;

use common::ephemeral_bus;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::signals_with;
use std::time::Duration;
use zbus::object_server::SignalEmitter;

#[zbus::interface(name = "cc.hannig.test.Pinger")]
struct Pinger;

#[zbus::interface(name = "cc.hannig.test.Pinger")]
impl Pinger {
    #[zbus(signal)]
    async fn pinged(emitter: &SignalEmitter<'_>, value: u32) -> zbus::Result<()>;
}

#[tokio::test(flavor = "multi_thread")]
async fn delivers_emitted_signal() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address().to_string();

    // Mount a server emitter on a separate connection.
    let server = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("cc.hannig.test.Pinger")
        .unwrap()
        .serve_at("/cc/hannig/test/Pinger", Pinger)
        .unwrap()
        .build()
        .await
        .unwrap();
    let object_server = server.object_server();
    let iface_ref = object_server
        .interface::<_, Pinger>("/cc/hannig/test/Pinger")
        .await
        .unwrap();

    // Subscribe via the bus primitive.
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();
    let sub = signals_with(&shared, "cc.hannig.test.Pinger")
        .at_path("/cc/hannig/test/Pinger")
        .iface("cc.hannig.test.Pinger")
        .signal("Pinged")
        .start();
    let mut events = sub.events();

    // Emit and expect to receive.
    let emitter = iface_ref.signal_emitter();
    Pinger::pinged(emitter, 42).await.unwrap();

    let evt = tokio::time::timeout(Duration::from_secs(2), events.next())
        .await
        .expect("timeout waiting for signal")
        .expect("stream ended");
    let body: u32 = evt.body.body().deserialize().expect("decode body");
    assert_eq!(body, 42);
}
```

- [ ] **Step 8.2: Run, watch it fail**

Run: `cargo test -p hytte-bus --test signals`
Expected: FAIL — `signals_with`, `SignalEvent`, etc. do not exist.

- [ ] **Step 8.3: Implement `signals` primitive**

`crates/hytte-bus/src/signals.rs`:

```rust
//! Primitive #2 — subscribe to D-Bus signals on a remote object.
//!
//! See spec section 3.2.

use crate::connection::SharedConnection;
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// A single signal emission delivered to the consumer.
pub struct SignalEvent {
    /// The raw zbus message; consumer calls `.body().deserialize::<T>()`
    /// to decode arguments.
    pub body: zbus::Message,
    /// Sender's unique name, if known.
    pub sender: Option<String>,
    /// Local clock when the event was received.
    pub timestamp: SystemTime,
}

/// Handle on a live signal subscription. Cloning is cheap (Arc) and does
/// not cancel; dropping the last clone tears down the subscription.
#[derive(Clone)]
pub struct SignalSubscription {
    inner: Arc<SubInner>,
}

struct SubInner {
    sender: tokio::sync::broadcast::Sender<Arc<SignalEvent>>,
    missed: Mutable<u64>,
    missed_counter: Arc<AtomicU64>,
}

impl SignalSubscription {
    /// Stream of signal emissions. Each call to `events()` returns an
    /// independent receiver; backpressure is handled by zbus' broadcast
    /// channel (slow consumers may lag).
    pub fn events(&self) -> impl futures_util::Stream<Item = SignalEvent> + Unpin {
        let mut rx = self.inner.sender.subscribe();
        Box::pin(async_stream::stream! {
            while let Ok(evt) = rx.recv().await {
                yield (*evt).clone_for_consumer();
            }
        })
    }

    /// Counter that bumps every time the bus reconnected and we
    /// re-subscribed — i.e. some signals between disconnect and
    /// re-subscribe were lost. Consumers that need authoritative state
    /// should re-fetch when this signal fires.
    pub fn missed_emissions(&self) -> impl Signal<Item = u64> {
        self.inner.missed.signal_cloned()
    }
}

impl SignalEvent {
    fn clone_for_consumer(&self) -> Self {
        Self {
            body: self.body.clone(),
            sender: self.sender.clone(),
            timestamp: self.timestamp,
        }
    }
}

/// Builder.
pub struct SignalsBuilder<'a> {
    shared: &'a SharedConnection,
    destination: String,
    path: String,
    iface: String,
    signal: String,
}

impl<'a> SignalsBuilder<'a> {
    #[must_use]
    pub fn at_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }
    #[must_use]
    pub fn iface(mut self, name: impl Into<String>) -> Self {
        self.iface = name.into();
        self
    }
    #[must_use]
    pub fn signal(mut self, name: impl Into<String>) -> Self {
        self.signal = name.into();
        self
    }
    pub fn start(self) -> SignalSubscription {
        let (tx, _) = tokio::sync::broadcast::channel(64);
        let missed = Mutable::new(0u64);
        let missed_counter = Arc::new(AtomicU64::new(0));
        let sub = SignalSubscription {
            inner: Arc::new(SubInner {
                sender: tx.clone(),
                missed: missed.clone(),
                missed_counter: missed_counter.clone(),
            }),
        };
        let shared = self.shared.clone();
        let dest = self.destination;
        let path = self.path;
        let iface = self.iface;
        let signal_name = self.signal;
        hytte_reactive::runtime::handle().spawn(async move {
            run_subscription(shared, dest, path, iface, signal_name, tx, missed, missed_counter).await;
        });
        sub
    }
}

#[doc(hidden)]
#[must_use]
pub fn signals_with<'a>(
    shared: &'a SharedConnection,
    destination: impl Into<String>,
) -> SignalsBuilder<'a> {
    SignalsBuilder {
        shared,
        destination: destination.into(),
        path: String::new(),
        iface: String::new(),
        signal: String::new(),
    }
}

async fn run_subscription(
    shared: SharedConnection,
    dest: String,
    path: String,
    iface: String,
    signal_name: String,
    tx: tokio::sync::broadcast::Sender<Arc<SignalEvent>>,
    missed: Mutable<u64>,
    missed_counter: Arc<AtomicU64>,
) {
    let mut first_iteration = true;
    loop {
        if !first_iteration {
            let n = missed_counter.fetch_add(1, Ordering::AcqRel) + 1;
            missed.set(n);
        }
        first_iteration = false;

        let result = shared
            .with_conn(|conn| {
                let dest = dest.clone();
                let path = path.clone();
                let iface = iface.clone();
                let signal_name = signal_name.clone();
                let tx = tx.clone();
                async move {
                    let proxy = zbus::Proxy::new(&conn, dest.as_str(), path.as_str(), iface.as_str()).await?;
                    let mut stream = proxy.receive_signal(signal_name.as_str()).await?;
                    while let Some(msg) = stream.next().await {
                        let event = SignalEvent {
                            body: (*msg).clone(),
                            sender: msg.header().sender().map(|s| s.to_string()),
                            timestamp: SystemTime::now(),
                        };
                        let _ = tx.send(Arc::new(event));
                    }
                    Ok(())
                }
            })
            .await;

        if let Err(e) = result {
            tracing::debug!(error = %e, dest, path, iface, signal_name,
                "signal subscription ended; will re-subscribe after reconnect");
        }

        // Wait for the supervisor to bring the bus back before retrying.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}
```

Add `async-stream = "0.3"` to `crates/hytte-bus/Cargo.toml` deps for the `stream!` macro.

Update `crates/hytte-bus/src/lib.rs`:

```rust
mod connection;
mod error;
mod own;
mod signals;

pub use connection::BusKind;
pub use error::BusError;
pub use own::{OwnState, own_name_with};
pub use signals::{SignalEvent, SignalSubscription, signals_with};

#[doc(hidden)]
pub use connection::test_support;
```

- [ ] **Step 8.4: Run the basic test**

Run: `cargo test -p hytte-bus --test signals`
Expected: PASS.

- [ ] **Step 8.5: Add the missed-emissions test**

Append to `crates/hytte-bus/tests/signals.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn missed_emissions_bumps_on_reconnect() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let sub = signals_with(&shared, "cc.hannig.test.Pinger")
        .at_path("/cc/hannig/test/Pinger")
        .iface("cc.hannig.test.Pinger")
        .signal("Pinged")
        .start();

    let initial = sub.missed_emissions().get();   // we'd need .get() exposed
    // simulate a disconnect/reconnect cycle:
    shared.simulate_disconnect_for_test().await;

    // Wait up to 2s for the counter to bump.
    let mut bumped = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        let now = sub.missed_emissions().get();
        if now > initial {
            bumped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(bumped, "missed_emissions did not bump after disconnect");
}
```

The test as written calls `.get()` on a `Signal`. `futures_signals::signal::Mutable` exposes `.get_cloned()`. The implementer should adapt: hold a `Mutable<u64>` accessor (rename `missed_emissions()` to return the `Mutable` or expose a separate `missed_count()` getter for tests). Alternative: convert the signal to a stream and poll for the increment.

- [ ] **Step 8.6: Run all signals tests**

Run: `cargo test -p hytte-bus --test signals`
Expected: both tests pass.

- [ ] **Step 8.7: Commit**

```bash
git add crates/hytte-bus/Cargo.toml crates/hytte-bus/src/signals.rs crates/hytte-bus/src/lib.rs crates/hytte-bus/tests/signals.rs
git commit -m "feat(hytte-bus): signals primitive (#2 of 5)

$(cat <<'EOF'
Subscribes to a remote D-Bus signal, exposes events via a broadcast
channel, surfaces missed_emissions counter that bumps on each
re-subscribe (consumers that need authoritative state can react).

Tests: emission delivered through the subscription, counter bumps
after a simulated disconnect.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: `call` primitive

**Files:**
- Create: `crates/hytte-bus/src/call.rs`
- Modify: `crates/hytte-bus/src/lib.rs`
- Create: `crates/hytte-bus/tests/call.rs`

Per spec section 3.3.

- [ ] **Step 9.1: Write failing test (success path)**

`crates/hytte-bus/tests/call.rs`:

```rust
mod common;

use common::ephemeral_bus;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{call_with, BusError, RetryPolicy};

#[tokio::test(flavor = "multi_thread")]
async fn calls_dbus_list_names() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let names: Vec<String> = call_with(&shared, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("ListNames")
        .args(())
        .send()
        .await
        .expect("ListNames");

    assert!(names.iter().any(|n| n == "org.freedesktop.DBus"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_method_is_permanent() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let result: Result<(), BusError> = call_with(&shared, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("DefinitelyDoesNotExist")
        .args(())
        .retry(RetryPolicy::Never)
        .send()
        .await;

    match result {
        Err(BusError::Permanent { dbus_name, .. }) => {
            assert!(dbus_name.is_some(), "expected dbus_name on Permanent");
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}
```

- [ ] **Step 9.2: Run, watch it fail**

Run: `cargo test -p hytte-bus --test call`
Expected: FAIL — `call_with` and `RetryPolicy` do not exist.

- [ ] **Step 9.3: Implement `call`**

`crates/hytte-bus/src/call.rs`:

```rust
//! Primitive #3 — one-shot D-Bus method call.
//!
//! See spec section 3.3.

use crate::connection::SharedConnection;
use crate::error::BusError;
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;
use zbus::zvariant::Type;

/// Retry behavior on transient bus failure (the bus was mid-reconnect when
/// the call landed).
#[derive(Clone, Copy, Debug)]
pub enum RetryPolicy {
    /// Never retry; surface `BusError::Transient` immediately.
    Never,
    /// Retry once after the supervisor re-establishes the connection.
    /// This is the default — the only legitimate transient on a local bus.
    Once,
    /// Exponential backoff up to N attempts.
    Backoff { max_attempts: u32 },
}

/// Builder for a one-shot D-Bus method call.
pub struct CallBuilder<'a, A> {
    shared: &'a SharedConnection,
    destination: String,
    path: String,
    iface: String,
    method: String,
    args: A,
    timeout: Duration,
    retry: RetryPolicy,
}

#[doc(hidden)]
#[must_use]
pub fn call_with<'a>(
    shared: &'a SharedConnection,
    destination: impl Into<String>,
) -> CallBuilder<'a, ()> {
    CallBuilder {
        shared,
        destination: destination.into(),
        path: String::new(),
        iface: String::new(),
        method: String::new(),
        args: (),
        timeout: Duration::from_secs(25),
        retry: RetryPolicy::Once,
    }
}

impl<'a, A> CallBuilder<'a, A> {
    pub fn at_path(mut self, p: impl Into<String>) -> Self { self.path = p.into(); self }
    pub fn iface(mut self, i: impl Into<String>) -> Self { self.iface = i.into(); self }
    pub fn method(mut self, m: impl Into<String>) -> Self { self.method = m.into(); self }
    pub fn timeout(mut self, d: Duration) -> Self { self.timeout = d; self }
    pub fn retry(mut self, r: RetryPolicy) -> Self { self.retry = r; self }

    pub fn args<NewA>(self, args: NewA) -> CallBuilder<'a, NewA>
    where NewA: Serialize + Type {
        CallBuilder {
            shared: self.shared, destination: self.destination, path: self.path,
            iface: self.iface, method: self.method, args,
            timeout: self.timeout, retry: self.retry,
        }
    }
}

impl<'a, A: Serialize + Type + Send + Sync + Clone + 'static> CallBuilder<'a, A> {
    /// Execute the call. Returns the deserialized reply, or a `BusError`.
    pub async fn send<R: DeserializeOwned + Type + 'static>(self) -> Result<R, BusError> {
        let attempt_one = do_call::<A, R>(&self).await;
        match (attempt_one, self.retry) {
            (Ok(r), _) => Ok(r),
            (Err(e), RetryPolicy::Never) => Err(e),
            (Err(e), _) if !e.is_transient() => Err(e),
            (Err(_), RetryPolicy::Once) => do_call::<A, R>(&self).await,
            (Err(_), RetryPolicy::Backoff { max_attempts }) => {
                let mut attempts = 1u32;
                let mut delay = Duration::from_millis(250);
                loop {
                    if attempts >= max_attempts { return Err(do_call::<A, R>(&self).await.err().unwrap()); }
                    tokio::time::sleep(delay).await;
                    match do_call::<A, R>(&self).await {
                        Ok(r) => return Ok(r),
                        Err(e) if !e.is_transient() => return Err(e),
                        Err(_) => { attempts += 1; delay = (delay * 2).min(Duration::from_secs(30)); }
                    }
                }
            }
        }
    }

    /// Spawn the call onto the runtime; log on error. For sync contexts
    /// that don't care about the reply.
    pub fn fire_and_forget(self) where R: Send + 'static, A: Send + Sync + 'static {
        // Compile-time: R is unknown here. Provide a separate fire_and_forget
        // method on the builder that doesn't take R, since fire-and-forget
        // doesn't decode the reply.
    }
}

async fn do_call<A, R>(b: &CallBuilder<'_, A>) -> Result<R, BusError>
where
    A: Serialize + Type + Clone,
    R: DeserializeOwned + Type,
{
    b.shared.with_conn(|conn| {
        let dest = b.destination.clone();
        let path = b.path.clone();
        let iface = b.iface.clone();
        let method = b.method.clone();
        let args = b.args.clone();
        let timeout = b.timeout;
        async move {
            let proxy = zbus::Proxy::new(&conn, dest.as_str(), path.as_str(), iface.as_str()).await?;
            let fut = proxy.call::<_, _, R>(method.as_str(), &args);
            tokio::time::timeout(timeout, fut)
                .await
                .map_err(|_| zbus::Error::Failure("call timeout".into()))?
        }
    }).await
}
```

The `fire_and_forget` impl above is a stub — it requires a separate method that takes no `R` generic. Implementer: add a `pub fn fire_and_forget(self)` ON `CallBuilder<'_, A>` (no R), which spawns `Self::send::<()>` on the runtime. Adjust generics so `R = ()` is the default for fire-and-forget paths.

Update `crates/hytte-bus/src/lib.rs`:

```rust
mod call;
mod connection;
mod error;
mod own;
mod signals;

pub use call::{CallBuilder, RetryPolicy, call_with};
pub use connection::BusKind;
pub use error::BusError;
pub use own::{OwnState, own_name_with};
pub use signals::{SignalEvent, SignalSubscription, signals_with};

#[doc(hidden)]
pub use connection::test_support;
```

- [ ] **Step 9.4: Run, expect PASS**

Run: `cargo test -p hytte-bus --test call`
Expected: both tests pass.

- [ ] **Step 9.5: Add the retry-Once test**

Append:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn retry_once_recovers_from_transient_disconnect() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    // Fire a disconnect simulation, then immediately attempt a call.
    // The first attempt fails (Transient), retry-Once succeeds after the
    // supervisor brings the bus back.
    shared.simulate_disconnect_for_test().await;

    let names: Result<Vec<String>, BusError> = call_with(&shared, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("ListNames")
        .args(())
        .retry(RetryPolicy::Once)
        .send()
        .await;

    assert!(names.is_ok(), "expected retry-Once to succeed; got {names:?}");
}
```

This test depends on the supervisor reconnecting fast enough that retry-Once observes the new connection. If flaky, increase the wait inside `do_call` slightly when the first attempt fails (e.g. await `epoch_signal` advancing before retrying).

- [ ] **Step 9.6: Run, expect PASS**

Run: `cargo test -p hytte-bus --test call`
Expected: all three tests pass.

- [ ] **Step 9.7: Commit**

```bash
git add crates/hytte-bus/src/call.rs crates/hytte-bus/src/lib.rs crates/hytte-bus/tests/call.rs
git commit -m "feat(hytte-bus): call primitive (#3 of 5)

$(cat <<'EOF'
One-shot method call with default retry-Once policy. Maps timeout to
zbus::Error::Failure; permanent errors carry the dbus error name through
BusError::Permanent. fire_and_forget spawns onto the hytte runtime and
logs on error.

Tests: ListNames round-trip, UnknownMethod → Permanent, retry-Once
recovers from a simulated disconnect.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: `property` primitive

**Files:**
- Create: `crates/hytte-bus/src/property.rs`
- Modify: `crates/hytte-bus/src/lib.rs`
- Create: `crates/hytte-bus/tests/property.rs`

Per spec section 3.4.

- [ ] **Step 10.1: Write failing test (cold-start Loading → Loaded)**

`crates/hytte-bus/tests/property.rs`:

```rust
mod common;

use common::ephemeral_bus;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{property_with, PropState};
use std::time::Duration;
use zbus::object_server::SignalEmitter;

#[zbus::interface(name = "cc.hannig.test.Counter")]
struct Counter {
    value: u32,
}

#[zbus::interface(name = "cc.hannig.test.Counter")]
impl Counter {
    #[zbus(property)]
    fn value(&self) -> u32 { self.value }

    #[zbus(property)]
    fn set_value(&mut self, v: u32) { self.value = v; }
}

#[tokio::test(flavor = "multi_thread")]
async fn cold_start_emits_loading_then_loaded() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address().to_string();

    // Stand up a server that exposes the Counter interface.
    let _server = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("cc.hannig.test.Counter")
        .unwrap()
        .serve_at("/cc/hannig/test/Counter", Counter { value: 7 })
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let signal = property_with::<u32>(&shared, "cc.hannig.test.Counter")
        .at_path("/cc/hannig/test/Counter")
        .iface("cc.hannig.test.Counter")
        .name("Value")
        .start();

    let mut stream = signal.to_stream();
    let mut saw_loading = false;
    let mut saw_loaded = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && saw_loaded.is_none() {
        let next = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        if let Ok(Some(state)) = next {
            match state {
                PropState::Loading => saw_loading = true,
                PropState::Loaded(v) => saw_loaded = Some(v),
                PropState::Stale(_) => {}
            }
        }
    }

    assert!(saw_loading, "expected at least one Loading emission");
    assert_eq!(saw_loaded, Some(7));
}
```

- [ ] **Step 10.2: Run, watch it fail**

Run: `cargo test -p hytte-bus --test property`
Expected: FAIL.

- [ ] **Step 10.3: Implement `property`**

`crates/hytte-bus/src/property.rs`:

```rust
//! Primitive #4 — cached property reads with PropertiesChanged tracking.
//!
//! See spec section 3.4.

use crate::connection::SharedConnection;
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use zbus::zvariant::Type;

/// Three states of a tracked property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropState<T> {
    /// Initial, before the first Get completes.
    Loading,
    /// Current authoritative value.
    Loaded(T),
    /// Last known value while the bus is reconnecting or the property is
    /// momentarily unavailable. UI should render this differently from
    /// `Loaded` (e.g. a dimmed CSS class).
    Stale(T),
}

pub struct PropertyBuilder<'a, T> {
    shared: &'a SharedConnection,
    destination: String,
    path: String,
    iface: String,
    name: String,
    _t: std::marker::PhantomData<T>,
}

#[doc(hidden)]
#[must_use]
pub fn property_with<'a, T>(
    shared: &'a SharedConnection,
    destination: impl Into<String>,
) -> PropertyBuilder<'a, T>
where T: Clone + DeserializeOwned + Type + Send + 'static {
    PropertyBuilder {
        shared, destination: destination.into(), path: String::new(),
        iface: String::new(), name: String::new(),
        _t: std::marker::PhantomData,
    }
}

impl<'a, T> PropertyBuilder<'a, T>
where T: Clone + DeserializeOwned + Type + Send + Sync + 'static {
    pub fn at_path(mut self, p: impl Into<String>) -> Self { self.path = p.into(); self }
    pub fn iface(mut self, i: impl Into<String>) -> Self { self.iface = i.into(); self }
    pub fn name(mut self, n: impl Into<String>) -> Self { self.name = n.into(); self }

    pub fn start(self) -> impl Signal<Item = PropState<T>> {
        let state: Mutable<PropState<T>> = Mutable::new(PropState::Loading);
        let writer = state.clone();
        let shared = self.shared.clone();
        let dest = self.destination;
        let path = self.path;
        let iface = self.iface;
        let name = self.name;
        hytte_reactive::runtime::handle().spawn(async move {
            run_property(shared, dest, path, iface, name, writer).await;
        });
        state.signal_cloned()
    }
}

async fn run_property<T>(
    shared: SharedConnection,
    dest: String,
    path: String,
    iface: String,
    name: String,
    writer: Mutable<PropState<T>>,
) where T: Clone + DeserializeOwned + Type + Send + Sync + 'static {
    let mut last: Option<T> = None;
    loop {
        // Cold start vs. reconnect:
        match (&last, writer.get_cloned()) {
            (Some(v), _) => writer.set(PropState::Stale(v.clone())),
            _ => writer.set(PropState::Loading),
        }

        let result = shared.with_conn(|conn| {
            let dest = dest.clone();
            let path = path.clone();
            let iface = iface.clone();
            let name = name.clone();
            async move {
                let props = zbus::fdo::PropertiesProxy::builder(&conn)
                    .destination(dest.as_str())?
                    .path(path.as_str())?
                    .build()
                    .await?;
                let v: zbus::zvariant::OwnedValue =
                    props.get(iface.as_str().try_into()?, name.as_str()).await?;
                let typed: T = v.try_into()
                    .map_err(|e: zbus::zvariant::Error| zbus::Error::Failure(e.to_string()))?;
                Ok(typed)
            }
        }).await;

        match result {
            Ok(v) => {
                last = Some(v.clone());
                writer.set(PropState::Loaded(v));
            }
            Err(e) => {
                tracing::debug!(error = %e, dest, path, iface, name, "property Get failed");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        }

        // Subscribe to PropertiesChanged.
        let _ = shared.with_conn(|conn| {
            let dest = dest.clone();
            let path = path.clone();
            let iface = iface.clone();
            let name = name.clone();
            let writer = writer.clone();
            let last_ref = &mut last;
            async move {
                let props = zbus::fdo::PropertiesProxy::builder(&conn)
                    .destination(dest.as_str())?
                    .path(path.as_str())?
                    .build()
                    .await?;
                let mut changes = props.receive_properties_changed().await?;
                while let Some(sig) = changes.next().await {
                    let Ok(args) = sig.args() else { continue };
                    if args.interface_name != iface.as_str() { continue; }
                    if let Some(raw) = args.changed_properties.get(name.as_str()) {
                        if let Ok(typed) = T::try_from(raw.try_to_owned()
                            .map_err(|e| zbus::Error::Failure(e.to_string()))?)
                        {
                            *last_ref = Some(typed.clone());
                            writer.set(PropState::Loaded(typed));
                        }
                    }
                    if args.invalidated_properties.iter().any(|n| *n == name.as_str()) {
                        // Re-fetch.
                        break;
                    }
                }
                Ok(())
            }
        }).await;
    }
}
```

The above has type-juggling inside the changes loop — `T::try_from(zbus::zvariant::OwnedValue)` requires `T: TryFrom<OwnedValue>`. The implementer should adjust trait bounds (add `TryFrom<OwnedValue, Error = …>` to T's bounds, or use `zvariant::DeserializeValue`) — the spec is that the property value gets decoded into T, the exact zbus 5.x trait may differ slightly. The test asserts the observable behavior; the implementer adjusts the bounds to make it compile.

Update `crates/hytte-bus/src/lib.rs`:

```rust
mod call;
mod connection;
mod error;
mod own;
mod property;
mod signals;

pub use call::{CallBuilder, RetryPolicy, call_with};
pub use connection::BusKind;
pub use error::BusError;
pub use own::{OwnState, own_name_with};
pub use property::{PropState, property_with};
pub use signals::{SignalEvent, SignalSubscription, signals_with};

#[doc(hidden)]
pub use connection::test_support;
```

- [ ] **Step 10.4: Run, expect PASS**

Run: `cargo test -p hytte-bus --test property`
Expected: PASS.

- [ ] **Step 10.5: Add the PropertiesChanged tracking test**

Append:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn properties_changed_emits_loaded_with_new_value() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address().to_string();

    let server = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("cc.hannig.test.Counter")
        .unwrap()
        .serve_at("/cc/hannig/test/Counter", Counter { value: 1 })
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let signal = property_with::<u32>(&shared, "cc.hannig.test.Counter")
        .at_path("/cc/hannig/test/Counter")
        .iface("cc.hannig.test.Counter")
        .name("Value")
        .start();
    let mut stream = signal.to_stream();
    // Drain initial Loading + Loaded(1).
    let mut current = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && !matches!(current, Some(1)) {
        if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(50), stream.next()).await {
            if let PropState::Loaded(v) = s { current = Some(v); }
        }
    }
    assert_eq!(current, Some(1));

    // Mutate the server-side property and emit PropertiesChanged.
    let iface_ref = server.object_server()
        .interface::<_, Counter>("/cc/hannig/test/Counter").await.unwrap();
    {
        let mut iface = iface_ref.get_mut().await;
        iface.set_value(99);
        iface.value_changed(iface_ref.signal_emitter()).await.unwrap();
    }

    // Expect the consumer signal to update to Loaded(99).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut updated = None;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(50), stream.next()).await {
            if let PropState::Loaded(v) = s { updated = Some(v); break; }
        }
    }
    assert_eq!(updated, Some(99));
}
```

- [ ] **Step 10.6: Run all property tests**

Run: `cargo test -p hytte-bus --test property`
Expected: both tests pass.

- [ ] **Step 10.7: Commit**

```bash
git add crates/hytte-bus/src/property.rs crates/hytte-bus/src/lib.rs crates/hytte-bus/tests/property.rs
git commit -m "feat(hytte-bus): property primitive (#4 of 5)

$(cat <<'EOF'
GetAll-on-cold-start + PropertiesChanged tracking. Three states:
Loading (initial), Loaded(T) (current), Stale(T) (last known value
while reconnecting). UI can render Stale distinctly from Loaded.

Tests: cold start emits Loading then Loaded, PropertiesChanged
delivers updated value.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: `proxy` primitive

**Files:**
- Create: `crates/hytte-bus/src/proxy.rs`
- Modify: `crates/hytte-bus/src/lib.rs`
- Create: `crates/hytte-bus/tests/proxy.rs`

Per spec section 3.5.

- [ ] **Step 11.1: Write failing test (Live state)**

`crates/hytte-bus/tests/proxy.rs`:

```rust
mod common;

use common::ephemeral_bus;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{proxy_with, ProxyState};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn live_when_peer_present() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let proxy = proxy_with(&shared, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .build().await.expect("build proxy");

    let mut stream = proxy.liveness().to_stream();
    let mut saw_live = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(50), stream.next()).await {
            if matches!(s, ProxyState::Live) { saw_live = true; break; }
        }
    }
    assert!(saw_live, "proxy never reached Live state");

    // Verify a method call works through the proxy.
    let names: Vec<String> = proxy.call("ListNames", ()).await.expect("ListNames");
    assert!(names.iter().any(|n| n == "org.freedesktop.DBus"));
}
```

- [ ] **Step 11.2: Run, watch it fail**

Run: `cargo test -p hytte-bus --test proxy`
Expected: FAIL.

- [ ] **Step 11.3: Implement `proxy`**

`crates/hytte-bus/src/proxy.rs`:

```rust
//! Primitive #5 — long-lived proxy handle that survives reconnects.
//!
//! See spec section 3.5.

use crate::connection::SharedConnection;
use crate::error::BusError;
use futures_signals::signal::{Mutable, Signal};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::Arc;
use tokio::sync::RwLock;
use zbus::zvariant::Type;

/// Liveness of a long-lived proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyState {
    /// Proxy is connected and the peer holds the destination name.
    Live,
    /// Bus is mid-reconnect; calls will return `BusError::Transient`
    /// momentarily.
    Reconnecting,
    /// Peer's destination name has no owner. Distinct from bus disconnect:
    /// the bus is fine, the *peer* (e.g. spotify) quit.
    PeerGone,
}

pub struct ProxyBuilder<'a> {
    shared: &'a SharedConnection,
    destination: String,
    path: String,
    iface: String,
}

#[doc(hidden)]
#[must_use]
pub fn proxy_with<'a>(
    shared: &'a SharedConnection,
    destination: impl Into<String>,
) -> ProxyBuilder<'a> {
    ProxyBuilder {
        shared,
        destination: destination.into(),
        path: String::new(),
        iface: String::new(),
    }
}

impl<'a> ProxyBuilder<'a> {
    pub fn at_path(mut self, p: impl Into<String>) -> Self { self.path = p.into(); self }
    pub fn iface(mut self, i: impl Into<String>) -> Self { self.iface = i.into(); self }

    pub async fn build(self) -> Result<BusProxy, BusError> {
        let inner = Arc::new(ProxyInner {
            shared: self.shared.clone(),
            destination: self.destination,
            path: self.path,
            iface: self.iface,
            cached: RwLock::new(None),
            liveness: Mutable::new(ProxyState::Reconnecting),
        });
        // Initial build.
        rebuild_proxy(&inner).await?;
        Ok(BusProxy { inner })
    }
}

#[derive(Clone)]
pub struct BusProxy {
    inner: Arc<ProxyInner>,
}

struct ProxyInner {
    shared: SharedConnection,
    destination: String,
    path: String,
    iface: String,
    cached: RwLock<Option<zbus::Proxy<'static>>>,
    liveness: Mutable<ProxyState>,
}

impl BusProxy {
    pub fn liveness(&self) -> impl Signal<Item = ProxyState> {
        self.inner.liveness.signal_cloned()
    }

    pub async fn call<A, R>(&self, method: &str, args: A) -> Result<R, BusError>
    where A: Serialize + Type, R: DeserializeOwned + Type {
        let cached = self.inner.cached.read().await;
        let proxy = cached.as_ref().ok_or_else(||
            BusError::Transient { source: zbus::Error::Disconnected })?;
        proxy.call::<_, _, R>(method, &args).await.map_err(BusError::from_zbus)
    }
}

async fn rebuild_proxy(inner: &Arc<ProxyInner>) -> Result<(), BusError> {
    let new = inner.shared.with_conn(|conn| {
        let dest = inner.destination.clone();
        let path = inner.path.clone();
        let iface = inner.iface.clone();
        async move {
            zbus::Proxy::new(&conn, dest.as_str(), path.as_str(), iface.as_str())
                .await
                .map(|p| p.into_owned())
        }
    }).await?;
    let mut cached = inner.cached.write().await;
    *cached = Some(new);
    inner.liveness.set(ProxyState::Live);
    Ok(())
}
```

The `into_owned()` call requires zbus's owned-proxy conversion (zbus 5.x exposes this; verify exact name). Implementer: if the API is named differently in 5.14, use whatever produces a `'static` proxy.

`PeerGone` and reconnect-driven `Reconnecting`/`Live` transitions are NOT yet implemented in this minimum cut — add a watcher task in step 11.5 below.

Update `crates/hytte-bus/src/lib.rs`:

```rust
mod call;
mod connection;
mod error;
mod own;
mod property;
mod proxy;
mod signals;

pub use call::{CallBuilder, RetryPolicy, call_with};
pub use connection::BusKind;
pub use error::BusError;
pub use own::{OwnState, own_name_with};
pub use property::{PropState, property_with};
pub use proxy::{BusProxy, ProxyBuilder, ProxyState, proxy_with};
pub use signals::{SignalEvent, SignalSubscription, signals_with};

#[doc(hidden)]
pub use connection::test_support;
```

- [ ] **Step 11.4: Run the basic test**

Run: `cargo test -p hytte-bus --test proxy`
Expected: PASS for `live_when_peer_present`.

- [ ] **Step 11.5: Add reconnect watcher + PeerGone test**

Append to `crates/hytte-bus/src/proxy.rs`, inside `ProxyBuilder::build`, after `rebuild_proxy(&inner).await?;`:

```rust
        // Spawn a watcher that reacts to bus reconnects + NameOwnerChanged
        // for the destination.
        let watch_inner = inner.clone();
        hytte_reactive::runtime::handle().spawn(async move {
            run_proxy_watcher(watch_inner).await;
        });
```

Implement at the bottom of `proxy.rs`:

```rust
async fn run_proxy_watcher(inner: Arc<ProxyInner>) {
    use futures_signals::signal::SignalExt;
    use futures_util::StreamExt;

    // Subscribe to NameOwnerChanged for the destination on each connection.
    loop {
        let dest = inner.destination.clone();
        let result = inner.shared.with_conn(|conn| {
            let dest = dest.clone();
            async move {
                let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
                let mut changes = dbus.receive_name_owner_changed().await?;
                while let Some(sig) = changes.next().await {
                    let Ok(args) = sig.args() else { continue };
                    if args.name().as_str() != dest { continue; }
                    let new = args.new_owner().as_ref().map(|n| n.as_str().to_string());
                    if new.is_none() {
                        return Ok::<&str, zbus::Error>("peer-gone");
                    }
                    return Ok("peer-back");
                }
                Ok("stream-ended")
            }
        }).await;

        match result {
            Ok("peer-gone") => inner.liveness.set(ProxyState::PeerGone),
            Ok("peer-back") => {
                let _ = rebuild_proxy(&inner).await;
            }
            _ => {
                inner.liveness.set(ProxyState::Reconnecting);
                let _ = rebuild_proxy(&inner).await;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}
```

Add the `PeerGone` test:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn peer_gone_then_back() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address().to_string();

    // Stand up a peer that owns "cc.hannig.test.Vanishable".
    let peer = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("cc.hannig.test.Vanishable")
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let proxy = proxy_with(&shared, "cc.hannig.test.Vanishable")
        .at_path("/")
        .iface("org.freedesktop.DBus.Peer")
        .build().await.expect("build");

    let mut liveness = proxy.liveness().to_stream();
    // Wait for Live.
    while let Ok(Some(s)) = tokio::time::timeout(Duration::from_secs(1), liveness.next()).await {
        if matches!(s, ProxyState::Live) { break; }
    }

    // Drop the peer.
    drop(peer);

    // Expect PeerGone within 2s.
    let mut saw_peer_gone = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(50), liveness.next()).await {
            if matches!(s, ProxyState::PeerGone) { saw_peer_gone = true; break; }
        }
    }
    assert!(saw_peer_gone, "expected PeerGone after peer dropped");
}
```

- [ ] **Step 11.6: Run all proxy tests**

Run: `cargo test -p hytte-bus --test proxy`
Expected: both tests pass.

- [ ] **Step 11.7: Commit**

```bash
git add crates/hytte-bus/src/proxy.rs crates/hytte-bus/src/lib.rs crates/hytte-bus/tests/proxy.rs
git commit -m "feat(hytte-bus): proxy primitive (#5 of 5)

$(cat <<'EOF'
Long-lived BusProxy that survives reconnects. Caches a zbus::Proxy
internally, rebuilds on bus epoch bump, observes NameOwnerChanged on
the destination to distinguish PeerGone from Reconnecting.

Tests: Live state when peer is up + ListNames round-trip,
PeerGone when peer drops.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Production accessors + `hytte` umbrella re-export

**Files:**
- Modify: `crates/hytte-bus/src/lib.rs` (add public `own_name`, `signals`, `call`, `property`, `proxy` that hit the global session/system)
- Modify: `crates/hytte/Cargo.toml`
- Modify: `crates/hytte/src/lib.rs`

- [ ] **Step 12.1: Add public global accessors in `lib.rs`**

```rust
//! Shared D-Bus capability layer for hytte services.
//!
//! See `docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md`
//! for the design.

#![doc(html_no_source)]

mod call;
mod connection;
mod error;
mod own;
mod property;
mod proxy;
mod signals;

pub use call::{CallBuilder, RetryPolicy};
pub use connection::BusKind;
pub use error::BusError;
pub use own::{OwnState, OwnNameBuilder};
pub use property::{PropState, PropertyBuilder};
pub use proxy::{BusProxy, ProxyBuilder, ProxyState};
pub use signals::{SignalEvent, SignalSubscription, SignalsBuilder};

#[doc(hidden)]
pub use connection::test_support;

// ── Public global accessors. These are the production API that
// `hytte-services` consumers call. They route to the lazily-initialized
// session-bus / system-bus SharedConnection singletons.

/// Build an `OwnNameBuilder` against the session bus by default. Use
/// `.bus(BusKind::System)` to switch (note: most consumers want session).
#[must_use]
pub fn own_name(name: impl Into<String>) -> OwnNameBuilder<'static> {
    own::own_name_with(connection::session(), name)
}

#[must_use]
pub fn signals(destination: impl Into<String>) -> SignalsBuilder<'static> {
    signals::signals_with(connection::system(), destination)
}

#[must_use]
pub fn call(destination: impl Into<String>) -> CallBuilder<'static, ()> {
    call::call_with(connection::session(), destination)
}

#[must_use]
pub fn property<T>(destination: impl Into<String>) -> PropertyBuilder<'static, T>
where T: Clone + serde::de::DeserializeOwned + zbus::zvariant::Type + Send + Sync + 'static {
    property::property_with(connection::system(), destination)
}

#[must_use]
pub fn proxy(destination: impl Into<String>) -> ProxyBuilder<'static> {
    proxy::proxy_with(connection::system(), destination)
}
```

`OwnNameBuilder`, `SignalsBuilder`, `CallBuilder`, `PropertyBuilder`, `ProxyBuilder` need a `bus(BusKind)` method that switches the underlying `SharedConnection` between session and system. Implementer adds this in each builder file — the method is one line: `pub fn bus(mut self, kind: BusKind) -> Self { self.shared = match kind { BusKind::Session => connection::session(), BusKind::System => connection::system() }; self }`. The lifetime `'static` works because the SharedConnections are global statics.

- [ ] **Step 12.2: Add `hytte-bus` to the umbrella crate**

Edit `crates/hytte/Cargo.toml`:

```toml
[dependencies]
hytte-bus      = { path = "../hytte-bus" }
hytte-reactive = { path = "../hytte-reactive" }
hytte-ui       = { path = "../hytte-ui" }
hytte-services = { path = "../hytte-services" }
```

- [ ] **Step 12.3: Re-export as `hytte::bus`**

Edit `crates/hytte/src/lib.rs`:

```rust
//! Library-first toolkit for composing GTK4 + Wayland desktop shells. This
//! crate just re-exports `hytte_ui`, `hytte_reactive`, `hytte_services`,
//! and `hytte_bus` under shorter module paths.

pub use hytte_bus as bus;
pub use hytte_reactive as reactive;
pub use hytte_services as services;
pub use hytte_ui as ui;

pub use hytte_reactive::futures_signals;
pub use hytte_ui::{adw, gtk};

pub mod prelude {
    pub use hytte_reactive::futures_signals::signal::SignalExt;
    pub use hytte_reactive::{bind, bind_class, bind_text, bind_two_way, bind_visible, Service};
    pub use hytte_ui::{
        App, Anchor, Bar, BarHandle, Edge, KeyboardMode, Layer, Margin, Monitor, Popup,
        PopupBuilder, PopupPosition,
    };
}
```

- [ ] **Step 12.4: Verify the workspace compiles**

Run: `cargo check --workspace`
Expected: clean compile (no warnings).

- [ ] **Step 12.5: Run all tests**

Run: `cargo test --workspace`
Expected: all tests pass.

- [ ] **Step 12.6: Commit**

```bash
git add crates/hytte-bus/src/lib.rs crates/hytte/Cargo.toml crates/hytte/src/lib.rs
git commit -m "feat(hytte): re-export hytte-bus as hytte::bus + global accessors

$(cat <<'EOF'
Production API: hytte::bus::{own_name, signals, call, property, proxy}
each route to the lazily-initialized session/system SharedConnection
singletons. Each builder gains a .bus(BusKind) method to override the
default bus.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: Migrate `resolved` to `hytte::bus`

**Files:**
- Modify: `crates/hytte-services/src/resolved.rs`

Smoke-test migration. Per spec section 5.3 (Phase 3). After this task: `resolved` opens zero connections of its own; everything routes through `hytte::bus::property`.

- [ ] **Step 13.1: Rewrite `resolved.rs` to use `bus::property`**

Replace the body of `crates/hytte-services/src/resolved.rs`:

```rust
//! DNS state from systemd-resolved (`org.freedesktop.resolve1`).
//!
//! Reads the Manager's `DNS` property — a list of `(ifindex, family,
//! address)` tuples — and emits a `DnsState` summary. The underlying
//! property subscription lives in `hytte_bus::property`, so reconnects
//! and PropertiesChanged tracking are handled by the bus layer.

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_bus::{property, BusKind, PropState};
use hytte_reactive::{registry, Service};
use std::net::IpAddr;

pub struct ResolvedService;

#[derive(Clone, Debug, Default)]
pub struct DnsState {
    pub servers: Vec<IpAddr>,
}

impl DnsState {
    #[must_use]
    pub fn configured(&self) -> bool {
        !self.servers.is_empty()
    }
}

#[doc(hidden)]
pub struct ResolvedHandles {
    pub(crate) dns: Mutable<DnsState>,
}

impl Default for ResolvedHandles {
    fn default() -> Self {
        Self {
            dns: Mutable::new(DnsState::default()),
        }
    }
}

impl Service for ResolvedService {
    type Handles = ResolvedHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = ResolvedHandles::default();
        let writer = handles.dns.clone();

        // DNS = a(iiay) — array of (ifindex i32, family i32, address bytes).
        let dns_property = property::<Vec<(i32, i32, Vec<u8>)>>("org.freedesktop.resolve1")
            .bus(BusKind::System)
            .at_path("/org/freedesktop/resolve1")
            .iface("org.freedesktop.resolve1.Manager")
            .name("DNS")
            .start();

        rt.spawn(async move {
            dns_property
                .for_each(move |state| {
                    let raw = match state {
                        PropState::Loaded(v) | PropState::Stale(v) => v,
                        PropState::Loading => Vec::new(),
                    };
                    let mut servers: Vec<IpAddr> = Vec::with_capacity(raw.len());
                    for (_idx, family, bytes) in raw {
                        if let Some(ip) = parse_addr(family, &bytes) {
                            servers.push(ip);
                        }
                    }
                    servers.sort();
                    servers.dedup();
                    writer.set(DnsState { servers });
                    std::future::ready(())
                })
                .await;
        });

        handles
    }
}

fn parse_addr(family: i32, bytes: &[u8]) -> Option<IpAddr> {
    // AF_INET = 2, AF_INET6 = 10 on Linux.
    match (family, bytes.len()) {
        (2, 4) => Some(IpAddr::V4(std::net::Ipv4Addr::new(
            bytes[0], bytes[1], bytes[2], bytes[3],
        ))),
        (10, 16) => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            Some(IpAddr::V6(std::net::Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

#[must_use]
pub fn service() -> ResolvedService {
    ResolvedService
}

pub fn dns() -> impl Signal<Item = DnsState> {
    registry::with(|r| {
        r.get::<ResolvedHandles>()
            .expect("resolved::service() not registered")
            .dns
            .signal_cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::parse_addr;
    use std::net::IpAddr;

    #[test]
    fn parses_ipv4() {
        let ip = parse_addr(2, &[1, 1, 1, 1]).unwrap();
        assert_eq!(ip, IpAddr::V4(std::net::Ipv4Addr::new(1, 1, 1, 1)));
    }

    #[test]
    fn parses_ipv6() {
        let bytes = [0x20, 0x01, 0x48, 0x60, 0x48, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0x88, 0x88];
        let ip = parse_addr(10, &bytes).unwrap();
        assert_eq!(
            ip,
            IpAddr::V6("2001:4860:4860::8888".parse().unwrap())
        );
    }

    #[test]
    fn rejects_unknown_family() {
        assert!(parse_addr(99, &[1, 2, 3, 4]).is_none());
    }
}
```

- [ ] **Step 13.2: Add `hytte-bus` as a `hytte-services` dep (transitional)**

Edit `crates/hytte-services/Cargo.toml`, add to `[dependencies]`:

```toml
hytte-bus = { path = "../hytte-bus" }
```

(Phase 6 of the spec — removing direct `zbus` dep — is a follow-up plan; for now both `hytte-bus` and `zbus` coexist.)

- [ ] **Step 13.3: Verify the workspace compiles**

Run: `cargo check --workspace`
Expected: clean.

- [ ] **Step 13.4: Verify `resolved` unit tests still pass**

Run: `cargo test -p hytte-services resolved::`
Expected: all three `parses_*` / `rejects_*` tests pass — they're pure functions, not affected by the migration.

- [ ] **Step 13.5: Manual smoke verification**

Build trollshell and start it briefly:

```sh
cargo build --release -p trollshell
RUST_LOG=hytte_bus=debug,hytte_services::resolved=debug,trollshell=info \
  ./target/release/trollshell &
sleep 5
lsof -p $(pidof trollshell) | grep -c socket
kill %1
```

Expected:
- `trollshell` starts cleanly.
- The journal shows `hytte_bus` "bus connected" once for the system bus, then `resolved` consuming a `Loaded(...)` state for the DNS property.
- `lsof` socket count is small and stable (the migration's purpose) — single-digit number of sockets, not the dozens we'd see with the old per-call connection pattern. (Other services unmigrated still open their own connections; the count is informative, not a hard assertion.)

If trollshell panics on startup with a SharedConnection-related error (`could not open system bus`, etc.), the Phase 6 deferral isn't holding cleanly — file an issue and roll back this commit; debug the supervisor's first-connect path.

- [ ] **Step 13.6: Commit**

```bash
git add crates/hytte-services/src/resolved.rs crates/hytte-services/Cargo.toml
git commit -m "refactor(resolved): migrate to hytte::bus::property

$(cat <<'EOF'
Smoke-test consumer for the new hytte-bus capability layer (Phase 3 of
the migration plan in docs/superpowers/specs/2026-04-27-shared-bus-
connections-design.md). resolved opens zero connections of its own; the
DNS property is tracked by hytte_bus::property which shares the
process-wide system-bus SharedConnection.

Behavior unchanged: dns() still returns Signal<Item = DnsState> with the
same parse_addr logic. The 2-second polling loop is gone — updates
arrive via PropertiesChanged from the bus layer.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review checklist (run after writing all tasks)

- [ ] **Spec coverage:** every section in `docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md` covered by Phases 1-3 has a task above. Phases 4-6 are explicitly deferred to follow-up plans (noted in the header).
- [ ] **No placeholders:** no "TBD", "TODO", "implement later" in the plan body. Where the implementer needs to adapt for zbus 5.x API specifics (the trait bounds in property's value-decoding, the exact `into_owned()` API on Proxy), the plan flags this explicitly with the assertion that anchors the behavior.
- [ ] **Type consistency:** `OwnState`, `PropState`, `ProxyState`, `BusError`, `RetryPolicy` use the same names everywhere. `own_name_with`/`signals_with`/`call_with`/`property_with`/`proxy_with` are the test-facing constructors taking `&SharedConnection`; the public `own_name`/`signals`/etc. (Task 12) are the production accessors that target the global singletons.
- [ ] **TDD discipline:** every primitive task starts with a failing test before any implementation code is written.
- [ ] **Frequent commits:** 13 commits across the plan. Each leaves the workspace compiling and tests passing.

---

## Follow-up plans (out of scope, written separately later)

After this plan ships:

1. **Phase 4: Loud-offender migrations** — one plan covering `notifications`, `wifi`, `polkit`, `screensaver`, `power_profiles`. These are the services causing the FD storm; this is the "actual bug fix" landing.
2. **Phase 5: Remaining services** — one plan or one per service for `bluetooth`, `mpris`, `tray`, `networkd`, `upower`, `brightness`, `systemd`.
3. **Phase 6: Cleanup** — drop `zbus` direct dep from `hytte-services/Cargo.toml`, add a clippy `disallowed_methods` rule listing `zbus::Connection::session` / `system` to prevent regression.
