# Power-menu actions — extraction to `hytte-services`

**Status:** design
**Date:** 2026-04-29
**Author:** Claude (with annika)

## Goal

Move the power-menu actions (`suspend`, `reboot`, `poweroff`, niri quit) out of the `trollshell` binary and into reusable `hytte-services` modules, so any hytte-based shell inherits them. Replace ad-hoc `systemctl` / `niri msg` process spawns with the same D-Bus / niri-socket transports the rest of the codebase already uses.

This is a structural relocation, not a UX change — every existing power-menu row keeps the same label, icon, and effect.

## Motivation

Two problems with the status quo in `trollshell/src/widgets/pages.rs`:

1. **Inconsistent transport.** The existing `niri` service in `hytte-services` already uses `niri_ipc::Socket` for actions (`focus_workspace`, `focus_window`). The shell binary's `spawn_detached("niri", &["msg", "action", "quit"])` is a regression — same daemon, different transport, no socket-error visibility.
2. **System actions belong in the library.** Every other system interaction in this workspace lives in `hytte-services` and goes through `hytte-bus` for D-Bus or a daemon-specific protocol. Spawning `systemctl` from the shell binary breaks that pattern, leaves polkit error handling implicit (the user sees nothing if a permission check fails), and means a future hytte-based shell would have to reimplement the same handful of lines.

The fan-out is small (4 call sites), but doing it right is the difference between "extracted" and "relocated."

## Scope

### In scope

- New module `crates/hytte-services/src/logind.rs` exposing `suspend()`, `reboot()`, `poweroff()` as fire-and-forget free functions that call `org.freedesktop.login1.Manager` over the system bus via `hytte-bus`.
- Extension to `crates/hytte-services/src/niri.rs`: add `pub fn quit(skip_confirmation: bool)` wrapping `niri_ipc::Action::Quit`, using the existing `send_action` plumbing.
- Re-wire the four power-menu rows in `trollshell/src/widgets/pages.rs::page_power_menu` to call the new APIs directly.
- Delete the `spawn_detached` helper from `pages.rs` once it has no remaining callers.
- Module registration in `crates/hytte-services/src/lib.rs` (`pub mod logind`).

### Out of scope

- **Inhibitor checks.** `org.freedesktop.login1.Manager.ListInhibitors` and `Inhibit` would let the menu warn the user if e.g. a download is preventing suspend. The current code does no such check; matching parity here means the new module doesn't either. Future work, separately specced.
- **Lock-on-suspend coordination.** logind already raises a `PrepareForSleep` signal that downstream services can hook (the existing `screensaver` service does this for `SetLockedHint`). No change to that wiring.
- **Other logind verbs.** `Hibernate`, `HybridSleep`, `SuspendThenHibernate`, `Halt`, `KExec` are all on the same Manager interface and trivially addable, but the trollshell power menu does not surface them. YAGNI.
- **Service struct + signals.** logind verbs are pure actions: there is no state to publish. No `LogindService`, no `Handles`, no `service()` registration. Free functions only, matching the `screensaver::lock` / `screensaver::inhibit` style for action-only entry points.
- **Fallback to `systemctl` if logind D-Bus call fails.** If the D-Bus call fails the `tracing::warn!` is the diagnostic; the user can retry. Adding a fallback path doubles the failure modes to reason about for a marginal improvement.

## API

```rust
// crates/hytte-services/src/logind.rs

/// Suspend the system via `org.freedesktop.login1.Manager.Suspend`.
/// Fire-and-forget; errors are logged at warn level. Polkit auth (if
/// required) routes through the active session's auth agent — for
/// trollshell sessions, that's the in-shell polkit dialog.
pub fn suspend();

/// Reboot the system. Same call shape as `suspend`.
pub fn reboot();

/// Power off the system. Same call shape as `suspend`.
pub fn poweroff();
```

```rust
// crates/hytte-services/src/niri.rs (added)

/// Ask niri to exit the session (fire-and-forget).
///
/// `skip_confirmation = false` lets niri's built-in confirmation overlay
/// fire — the right UX when this is invoked from a power menu where the
/// menu itself is the only confirmation. Pass `true` if you have already
/// confirmed externally.
pub fn quit(skip_confirmation: bool);
```

The three logind functions all use the canonical `hytte-bus` shape, parallel to how `screensaver::call_login1_unlock` works today:

```rust
hytte_bus::call("org.freedesktop.login1")
    .bus(hytte_bus::BusKind::System)
    .at_path("/org/freedesktop/login1")
    .iface("org.freedesktop.login1.Manager")
    .method("Suspend")     // or "Reboot", "PowerOff"
    .args((false,))        // interactive: false → use active session's polkit agent
    .send::<()>()
    .await
```

The synchronous fire-and-forget wrapper is the same pattern as `niri::send_action`: `runtime::handle().spawn(async move { … })` with the error logged inside the future.

## Migration in `pages.rs`

The four call sites in `page_power_menu` (lines 2660-2706 in current `pages.rs`):

```rust
// Lock — already uses hytte::services::screensaver::lock(); unchanged.

// Logout
- spawn_detached("niri", &["msg", "action", "quit"]);
+ hytte::services::niri::quit(false);

// Suspend
- spawn_detached("systemctl", &["suspend"]);
+ hytte::services::logind::suspend();

// Reboot
- spawn_detached("systemctl", &["reboot"]);
+ hytte::services::logind::reboot();

// Shutdown
- spawn_detached("systemctl", &["poweroff"]);
+ hytte::services::logind::poweroff();
```

After the migration, `spawn_detached` has zero callers and is deleted. The function-level docstring referencing "task #27 polkit" stays accurate — polkit auth now flows over D-Bus directly, which is what that task plumbed.

## Behavior

- **Polkit auth.** Calling `Manager.Suspend(false)` (interactive=false) makes logind ask polkit to authorize the action against the *active session*. trollshell's polkit agent service answers that prompt with the standard auth dialog (already wired via `polkit::service()` and `widgets::polkit_dialog`). No new auth UI.
- **Failure modes.** D-Bus errors (logind not reachable, polkit denial, no active session) are logged at `tracing::warn!` and silently consumed — same observability ceiling as the current `spawn_detached` (which loses systemctl exit codes today). The drawer dismisses regardless, matching current UX.
- **Ordering.** Fire-and-forget. The drawer's `dismiss_all()` and the system action race; logind's actual suspend takes ~tens of milliseconds to begin, by which time the drawer animation is well underway. No user-visible difference from today.

## Testing

D-Bus calls against logind are integration-tested by running the shell on a real session, the same way the existing `screensaver`, `upower`, `polkit`, etc. services are tested. The wrapper functions are thin enough (a couple of lines each, all mechanical) that unit tests would only verify the constants — not worthwhile.

Manual integration test plan:
1. `cargo run -p trollshell` on a niri session.
2. Open power menu; click Logout — niri shows its built-in confirmation overlay; cancel.
3. Click Suspend — logind authenticates via polkit (auth dialog appears if pkla requires it; otherwise no dialog), system suspends.
4. Resume; confirm `screensaver::is_locked()` raised the lock screen via the existing `PrepareForSleep` hook.
5. Reboot and Shutdown — covered by the same path; only verified once in normal QA.

## Risks

- **No active session.** If trollshell is launched outside an XDG session (rare; only really happens in dev when running under `cargo run` from a shell that wasn't started by logind/PAM), logind has no session to authenticate against and the call fails with `org.freedesktop.DBus.Error.UnknownMethod` / `Error.AccessDenied`. The `tracing::warn!` line is the only diagnostic. Same risk exists today with `systemctl`, just with a less helpful failure mode.
- **niri `Action::Quit` payload churn.** `niri_ipc::Action::Quit { skip_confirmation: bool }` is the current shape. If a future niri-ipc version renames or restructures this, the wrapper updates in one place. Already a constraint we accept for `FocusWorkspace` / `FocusWindow`.

## File touch summary

| File                                              | Change                                                       |
| ------------------------------------------------- | ------------------------------------------------------------ |
| `crates/hytte-services/src/logind.rs`             | new — ~80 LOC                                                |
| `crates/hytte-services/src/niri.rs`               | extend — add `pub fn quit(skip_confirmation: bool)` (~10 LOC) |
| `crates/hytte-services/src/lib.rs`                | `pub mod logind;`                                            |
| `trollshell/src/widgets/pages.rs`                 | rewire 4 call sites; delete `spawn_detached` (~10 LOC net)   |

Net: roughly +90 / −10 LOC. Library gains capability; binary loses an unwanted detour.
