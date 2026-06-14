# Power-Menu Actions Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move power-menu actions (suspend/reboot/poweroff and niri quit) out of the `trollshell` binary into reusable `hytte-services` modules, replacing process spawns with the same D-Bus and niri-socket transports the rest of the codebase already uses.

**Architecture:** Three-step extraction. (1) Extend the existing `niri` service with `quit(skip_confirmation: bool)` reusing its `send_action` helper. (2) Add a new `logind` module exposing free fns `suspend()`, `reboot()`, `poweroff()` that fire `org.freedesktop.login1.Manager` D-Bus calls via `hytte-bus`. (3) Re-wire the four call sites in `trollshell/src/widgets/pages.rs::page_power_menu` and delete the now-unused `spawn_detached` helper. No behavior change; structural relocation only.

**Tech Stack:** Rust 1.94, `hytte-bus` (workspace D-Bus capability layer over `zbus`), `hytte-reactive` (`runtime::handle()` for tokio spawn), `niri-ipc` 25.11.0 (compositor IPC), `tracing` (warn-level error logs).

---

## File Structure

| File                                  | Responsibility                                                                                                                                                                                                                             |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `crates/hytte-services/src/niri.rs`   | Compositor IPC client. **Add** `pub fn quit(skip_confirmation: bool)` reusing `send_action`.                                                                                                                                               |
| `crates/hytte-services/src/logind.rs` | **New.** Free fns wrapping `org.freedesktop.login1.Manager.{Suspend,Reboot,PowerOff}` over the system bus. No reactive state, no service struct — action-only.                                                                             |
| `crates/hytte-services/src/lib.rs`    | **Add** `pub mod logind;` line.                                                                                                                                                                                                            |
| `trollshell/src/widgets/pages.rs`     | **Modify** `page_power_menu` rows (lines ~2660-2706 in current file): replace four `spawn_detached(...)` calls with calls into `hytte::services::logind` / `hytte::services::niri`. **Delete** `fn spawn_detached` once it has no callers. |

Spec reference: `docs/superpowers/specs/2026-04-29-power-menu-extraction-design.md`.

---

## Task 1: Extend `niri` service with `quit`

**Files:**

- Modify: `crates/hytte-services/src/niri.rs` (add fn after `focus_window`, around line 242)

- [ ] **Step 1: Verify the existing send_action plumbing is still intact**

Run: `grep -n 'fn send_action\|fn focus_window\|fn focus_workspace' crates/hytte-services/src/niri.rs`
Expected: three matches, with `send_action` being the private helper that `focus_workspace` and `focus_window` delegate to.

- [ ] **Step 2: Add `quit` after `focus_window`**

Open `crates/hytte-services/src/niri.rs`. Find this block (around line 239-242):

```rust
/// Focus the window with the given id (fire-and-forget).
pub fn focus_window(id: u64) {
    send_action(Action::FocusWindow { id });
}
```

Insert immediately after that `}`:

```rust

/// Ask niri to exit the session (fire-and-forget).
///
/// `skip_confirmation = false` lets niri's built-in confirmation overlay
/// fire, which is the right UX when this is invoked from a power menu
/// where the menu itself is the only confirmation. Pass `true` if the
/// caller has already confirmed externally.
pub fn quit(skip_confirmation: bool) {
    send_action(Action::Quit { skip_confirmation });
}
```

- [ ] **Step 3: Build the crate to verify the Action::Quit variant matches**

Run: `cargo build -p hytte-services --message-format=short 2>&1 | tail -10`
Expected: `Finished \`dev\` profile`with no errors. If`Action::Quit { skip_confirmation }`is not the right variant shape, the compiler will reject it —`niri-ipc` 25.11.0 has been verified (`grep -E 'Quit|skip_confirmation' ~/.cargo/registry/src/\*/niri-ipc-25.11.0/src/lib.rs`) to expose exactly this shape.

- [ ] **Step 4: Run clippy to confirm no new warnings on the touched file**

Run: `cargo clippy -p hytte-services --message-format=short 2>&1 | grep niri.rs | tail -5`
Expected: no output (no clippy warnings on `niri.rs`).

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-services/src/niri.rs
git commit -m "$(cat <<'EOF'
feat(services/niri): add quit(skip_confirmation: bool)

Wraps niri_ipc::Action::Quit through the existing send_action plumbing,
matching the focus_workspace / focus_window shape. Replaces the binary's
spawn_detached("niri", &["msg", "action", "quit"]) detour.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: New `logind` module

**Files:**

- Create: `crates/hytte-services/src/logind.rs`
- Modify: `crates/hytte-services/src/lib.rs` (add `pub mod logind;`)

- [ ] **Step 1: Create the new module**

Write `crates/hytte-services/src/logind.rs`:

```rust
//! `org.freedesktop.login1.Manager` action wrappers.
//!
//! Exposes `suspend`, `reboot`, `poweroff` as fire-and-forget free
//! functions that route through the system bus via `hytte-bus`. Polkit
//! authorization (when required by pkla) flows through the active
//! session's auth agent — for trollshell sessions, that's the in-shell
//! polkit dialog wired by `widgets::polkit_dialog`.
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
```

- [ ] **Step 2: Register the module in lib.rs**

Open `crates/hytte-services/src/lib.rs`. Find this block:

```rust
pub mod displays;
pub mod dnd;
```

Locate the alphabetical insertion point — `logind` goes between `displays`/`dnd` and `mpris`. The current file lists modules alphabetically; insert `pub mod logind;` after `pub mod dnd;`:

```rust
pub mod displays;
pub mod dnd;
pub mod logind;
pub mod mpris;
```

- [ ] **Step 3: Build the crate**

Run: `cargo build -p hytte-services --message-format=short 2>&1 | tail -10`
Expected: `Finished \`dev\` profile` with no errors.

If `hytte_bus::call(...).send::<()>()` rejects on the unit type signature, the canonical pattern from `crates/hytte-services/src/screensaver.rs:223-235` (`call_login1_unlock`) confirms `send::<()>()` is correct for fire-and-forget logind calls.

- [ ] **Step 4: Run clippy on the new module**

Run: `cargo clippy -p hytte-services --message-format=short 2>&1 | grep logind.rs`
Expected: no output (no clippy warnings).

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-services/src/logind.rs crates/hytte-services/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(services): add logind module for suspend/reboot/poweroff

New `hytte-services::logind` exposes free fns calling
org.freedesktop.login1.Manager.{Suspend,Reboot,PowerOff} over the system
bus via hytte-bus. Action-only (no reactive state, no service struct).
Polkit auth routes through the active session's agent.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Rewire `page_power_menu` and delete `spawn_detached`

**Files:**

- Modify: `trollshell/src/widgets/pages.rs` (4 call sites in `page_power_menu`, around lines 2664-2706; delete `spawn_detached` around lines 2755-2768)

- [ ] **Step 1: Confirm the four call sites and the helper are still where the spec expects**

Run: `grep -n 'spawn_detached' trollshell/src/widgets/pages.rs`
Expected: 5 matches — 4 in `page_power_menu` (one each for `niri msg action quit`, `systemctl suspend`, `systemctl reboot`, `systemctl poweroff`) and 1 at the helper definition.

- [ ] **Step 2: Rewire the Logout row**

Open `trollshell/src/widgets/pages.rs`. Find this block (around lines 2664-2676):

```rust
    group.add(&power_action_row(
        "Logout",
        "End the niri session",
        "system-log-out-symbolic",
        None,
        || {
            // niri's `quit` shows its own confirmation overlay, which is the
            // right UX for a destructive session-end action. Pass
            // `--skip-confirmation` to suppress it if you want this row to
            // be the single point of confirmation.
            spawn_detached("niri", &["msg", "action", "quit"]);
        },
    ));
```

Replace with:

```rust
    group.add(&power_action_row(
        "Logout",
        "End the niri session",
        "system-log-out-symbolic",
        None,
        || {
            // niri's `quit` shows its own confirmation overlay, which is the
            // right UX for a destructive session-end action. Pass `true` to
            // suppress it if this row should be the single point of
            // confirmation.
            hytte::services::niri::quit(false);
        },
    ));
```

- [ ] **Step 3: Rewire the Suspend row**

Find:

```rust
        || {
            spawn_detached("systemctl", &["suspend"]);
        },
```

Replace with:

```rust
        || {
            hytte::services::logind::suspend();
        },
```

(There are three identical `spawn_detached("systemctl", …)` patterns; this Edit must use enough context to match only the Suspend row's closure. The surrounding `power_action_row` call lists `"Suspend"` as the first arg.)

- [ ] **Step 4: Rewire the Reboot row**

Same pattern: locate the closure inside the `"Reboot"` row and replace `spawn_detached("systemctl", &["reboot"]);` with `hytte::services::logind::reboot();`.

- [ ] **Step 5: Rewire the Shutdown row**

Same pattern: locate the closure inside the `"Shutdown"` row and replace `spawn_detached("systemctl", &["poweroff"]);` with `hytte::services::logind::poweroff();`.

- [ ] **Step 6: Verify zero remaining `spawn_detached` callers**

Run: `grep -n 'spawn_detached' trollshell/src/widgets/pages.rs`
Expected: only one match — the function definition itself (around line 2759).

- [ ] **Step 7: Delete the `spawn_detached` helper**

Find the block (around lines 2755-2768):

```rust
/// Fire-and-forget process spawn for power-menu actions. systemctl calls
/// hit polkit (wired up in task #27); auth flows through the trollshell
/// polkit dialog. Errors are logged at warn level — the user already sees
/// the drawer close, so a silent failure would be confusing.
fn spawn_detached(program: &str, args: &[&str]) {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    if let Err(e) = cmd.spawn() {
        tracing::warn!(program, ?args, error = %e, "power-menu: spawn failed");
    }
}

```

Delete the entire block (including the trailing blank line so the file doesn't accumulate one).

- [ ] **Step 8: Build the binary**

Run: `cargo build -p trollshell --message-format=short 2>&1 | tail -10`
Expected: `Finished \`dev\` profile`with no errors and no warnings about an unused`spawn_detached` symbol.

- [ ] **Step 9: Run workspace clippy to catch anything stale**

Run: `cargo clippy --workspace --message-format=short 2>&1 | tail -20`
Expected: no new warnings on `pages.rs`. (The pre-existing `mpris.rs` doc-backticks warning may still appear; that's unrelated.)

- [ ] **Step 10: Run workspace tests to confirm no regressions**

Run: `cargo test --workspace --message-format=short 2>&1 | grep -E '(test result|FAILED)' | head -30`
Expected: every line is `test result: ok.`. No `FAILED`.

- [ ] **Step 11: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
refactor(power-menu): use hytte-services for all actions

Replaces the four spawn_detached(...) calls in page_power_menu with
direct calls into the new hytte::services::logind module and the
extended hytte::services::niri::quit. Drops the spawn_detached helper
since it has no remaining callers.

No UX change. Polkit auth for suspend/reboot/poweroff now flows over
D-Bus directly instead of through systemctl, matching every other
system service in this workspace.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Manual integration check on a running session

**Files:** none (runtime verification only)

These steps must be performed by a human operator on a niri session — D-Bus calls against logind cannot be unit-tested without a real session bus and active session.

- [ ] **Step 1: Launch trollshell**

Run: `cargo run -p trollshell` from a niri session terminal.
Expected: bar appears on every monitor; no panics in the foreground tracing output; no warnings of the form `logind: Manager.* failed` (none should fire until a power-menu row is clicked).

- [ ] **Step 2: Test Logout (cancel)**

Open the power-menu drawer. Click **Logout**. Expected: niri's built-in confirmation overlay appears. Click cancel. The session continues; no warnings logged.

- [ ] **Step 3: Test Suspend**

Click **Suspend**. Expected: the system suspends within ~1 s. If pkla requires interactive auth, the trollshell polkit dialog appears first; entering the password completes the suspend. On resume, confirm the lock screen appeared (this is owned by the existing `screensaver` service's `PrepareForSleep` hook, not changed by this work; checking it covers the integration).

- [ ] **Step 4: Test Reboot and Shutdown only if you actually want to reboot/shutdown**

These are destructive; QA them once during normal development cycles. The same code path (D-Bus call to `Manager.Reboot` / `Manager.PowerOff`) is exercised by Suspend in step 3, so the marginal coverage is small.

- [ ] **Step 5: Capture any warnings to BUGS.md if observed**

If steps 2-4 produce any `logind: Manager.* failed` or unexpected polkit denials, append a line to `BUGS.md` describing the observed error. Do not commit any code changes for this step.

---

## Self-Review

Spec coverage check (against `docs/superpowers/specs/2026-04-29-power-menu-extraction-design.md`):

- ✅ "New module `crates/hytte-services/src/logind.rs`" — Task 2.
- ✅ "`pub fn quit(skip_confirmation: bool)` in `niri.rs`" — Task 1.
- ✅ "Re-wire the four power-menu rows" — Task 3 steps 2-5.
- ✅ "Delete the `spawn_detached` helper" — Task 3 step 7.
- ✅ "Module registration in `lib.rs`" — Task 2 step 2.
- ✅ Spec API constants (`LOGIN1_DEST`, `MANAGER_IFACE`, `interactive=false`) — Task 2 code block.
- ✅ Spec testing section — Task 4 manual integration plan; unit tests skipped per spec rationale.

No placeholders, no "implement later", every code step shows the actual code, every command shows expected output. Type names (`hytte_bus::call`, `hytte_bus::BusKind::System`, `runtime::handle()`, `niri_ipc::Action::Quit`) are consistent across tasks and verified against existing call sites in `screensaver.rs:227` and `niri.rs:245`.
