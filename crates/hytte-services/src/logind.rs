//! `org.freedesktop.login1.Manager` action wrappers.
//!
//! Exposes `suspend`, `reboot`, `poweroff` as fire-and-forget free
//! functions that route through the system bus via `hytte-bus`. Polkit
//! authorization (when required by pkla) flows through the active
//! session's auth agent — the standalone polkit-gnome agent run as a user
//! service alongside the session (see the flake's nixosModule / `etc/`).
//!
//! No reactive state is published from this module: these are pure
//! actions, so there is no `Service` struct or `service()` registration.
//! Errors are logged at `tracing::warn!` and otherwise consumed; the
//! caller's UI (drawer, menu) dismisses regardless, mirroring the
//! pre-extraction `spawn_detached("systemctl", …)` behavior.
//!
//! # Bus details
//!
//! - Destination: `org.freedesktop.login1` (system bus)
//! - Object path: `/org/freedesktop/login1`
//! - Interface: `org.freedesktop.login1.Manager`
//! - Methods: `Suspend(b)`, `Reboot(b)`, `PowerOff(b)` — each takes
//!   `interactive: bool`; we pass `false`, which delegates polkit auth
//!   to the active session's agent (the canonical behavior of
//!   `systemctl suspend` etc.).

use hytte_reactive::runtime;

const LOGIN1_DEST: &str = "org.freedesktop.login1";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";
const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";

/// Suspend the system. Fire-and-forget; errors logged at warn level.
pub fn suspend() {
    spawn_manager_call("Suspend");
}

/// Reboot the system. Fire-and-forget; errors logged at warn level.
pub fn reboot() {
    spawn_manager_call("Reboot");
}

/// Power off the system. Fire-and-forget; errors logged at warn level.
pub fn poweroff() {
    spawn_manager_call("PowerOff");
}

fn spawn_manager_call(method: &'static str) {
    runtime::handle().spawn(async move {
        let result = hytte_bus::call(LOGIN1_DEST)
            .bus(hytte_bus::BusKind::System)
            .at_path(LOGIN1_PATH)
            .iface(MANAGER_IFACE)
            .method(method)
            .args((false,))
            .send::<()>()
            .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, method, "logind: Manager.{method} failed");
        }
    });
}
