//! `org.freedesktop.login1.Manager` action wrappers.
//!
//! Exposes `suspend`, `reboot`, `poweroff` as fire-and-forget free
//! functions that route through the system bus via `hytte-bus`, plus
//! [`inhibit_idle`], which leases a logind idle-inhibitor fd. Polkit
//! authorization (when required by pkla) flows through the active
//! session's auth agent — the standalone polkit-gnome agent run as a user
//! service alongside the session (see the flake's nixosModule / `etc/`).
//!
//! The `suspend`/`reboot`/`poweroff` actions publish no reactive state
//! (pure fire-and-forget). Errors are logged at `tracing::warn!` and
//! otherwise consumed; the caller's UI (drawer, menu) dismisses regardless,
//! mirroring the pre-extraction `spawn_detached("systemctl", …)` behavior.
//! [`inhibit_idle`] instead returns an [`FdLease`] the caller holds — the
//! open fd *is* the inhibition.
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

use hytte_bus::{BusError, FdLease};
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

/// Acquire a logind **idle inhibitor**. The returned [`FdLease`] wraps the
/// leased fd whose open-ness *is* the lock: while the lease is alive the
/// system won't run its idle actions (screen blank/dim, auto-lock on idle);
/// dropping the lease releases the inhibition. This is honest, inspectable
/// state — `systemd-inhibit --list` shows it. (The fd, and therefore the
/// inhibitor, is owned by this process; see below on surviving a shell
/// restart.)
///
/// Calls `Inhibit(what="idle", who="trollshell", why="Keep awake",
/// mode="block")` on `org.freedesktop.login1.Manager`, which lives on the
/// **system** bus.
///
/// This is the enforcement mechanism for the "Keep awake" caffeine toggle
/// (issue #270): the native idle manager ([`crate::idle_notify`], #204) reads
/// logind's `BlockInhibited` before dimming/locking, so holding this `idle`
/// lease makes it skip those actions. The caller pairs the lease with a
/// matching screensaver inhibitor purely for visibility in
/// [`crate::screensaver::inhibitors`]. Hold the lease in service-side state
/// (never a widget) for as long as the inhibition should last.
///
/// **The fd is owned by *this* process.** It closes when trollshell exits,
/// which releases the inhibitor — so the hold does *not* by itself survive a
/// shell restart (the `BlockInhibited` entry vanishes the moment the process
/// dies). A caller that wants "Keep awake" to outlast a restart must persist
/// its own desired-state and re-acquire on the next start; that is exactly what
/// [`crate::screensaver`] does (#534).
///
/// # Errors
/// Returns a [`BusError`] if the `Inhibit` call fails: a transient bus error
/// (subject to the default retry), a timeout, or a D-Bus error reply.
pub async fn inhibit_idle() -> Result<FdLease, BusError> {
    hytte_bus::call(LOGIN1_DEST)
        .bus(hytte_bus::BusKind::System)
        .at_path(LOGIN1_PATH)
        .iface(MANAGER_IFACE)
        .method("Inhibit")
        .args(("idle", "trollshell", "Keep awake", "block"))
        .call_fd()
        .await
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
