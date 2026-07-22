# `hytte-bus` Phase 4-6 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate all twelve `hytte-services` services that still hold their own `zbus::Connection` to the `hytte::bus` capability layer, removing the per-service connection storm that exhausts dbus-broker (the original bug). After this plan: zero `Connection::session()`/`Connection::system()` calls in `hytte-services`, the direct `zbus` dependency is removed from that crate, and a clippy `disallowed_methods` rule prevents regression.

**Architecture:** Each service migration follows the `resolved.rs` template established in the foundation plan: replace the per-service flat-rate retry loop and `Connection::*().await` calls with `hytte::bus::{own_name, signals, call, property, proxy}` builder chains. Reactive surface (`pub fn x() -> impl Signal<...>`) and Service trait registration are unchanged. Phase 4 lands the loud-offender migrations (the actual bug fix in production). Phase 5 finishes the remaining services. Phase 6 drops the direct `zbus` dependency and adds a regression guard.

**Tech Stack:** Rust 2024 edition, `hytte_bus = path = "../hytte-bus"` already added to `hytte-services/Cargo.toml`, zbus 5.14 (transitionally), futures-signals 0.3, tokio multi-thread runtime, integration tests against ephemeral `dbus-daemon` for primitive correctness.

**Spec:** `docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md`

**Predecessor:** `docs/superpowers/plans/2026-04-27-hytte-bus-foundation.md` (Phases 1-3, completed)

---

## Convention

Each service migration commits as `refactor(<service>): migrate to hytte::bus::*`. Commit messages include a Co-Authored-By trailer. Workspace must compile and all existing tests must pass after every commit. Workspace clippy with `-D warnings` must stay clean.

The foundation review flagged four follow-up items; this plan addresses #1 (cold-start epoch race in `signals.rs`) as Task 1, since the loud-offender migrations consume `signals` and would surface the race at scale. Items #2–#4 are deferred — they don't block production.

After every migration commit: `cargo check --workspace && cargo test --workspace && cargo clippy --workspace --tests -- -D warnings` all clean.

---

## File Structure

**Modified across all phases:**

| File                                          | Phase      | What changes                                                 |
| --------------------------------------------- | ---------- | ------------------------------------------------------------ |
| `crates/hytte-bus/src/signals.rs`             | 4 (prereq) | Move epoch capture to AFTER `with_conn` succeeds             |
| `crates/hytte-services/src/notifications.rs`  | 4          | Migrate to `bus::{own_name, call}`                           |
| `crates/hytte-services/src/wifi.rs`           | 4          | Migrate to `bus::{signals, call, property}`                  |
| `crates/hytte-services/src/polkit.rs`         | 4          | Migrate to `bus::{own_name, signals, call}`                  |
| `crates/hytte-services/src/screensaver.rs`    | 4          | Migrate to `bus::{own_name, signals, call}`                  |
| `crates/hytte-services/src/power_profiles.rs` | 4          | Migrate to `bus::{property, call}`                           |
| `crates/hytte-services/src/bluetooth.rs`      | 5          | Migrate to `bus::{own_name, signals, call, property, proxy}` |
| `crates/hytte-services/src/mpris.rs`          | 5          | Migrate to `bus::{signals, call, proxy}`                     |
| `crates/hytte-services/src/tray.rs`           | 5          | Migrate to `bus::{own_name, signals, call, proxy}`           |
| `crates/hytte-services/src/networkd.rs`       | 5          | Migrate to `bus::{signals, property}`                        |
| `crates/hytte-services/src/upower.rs`         | 5          | Migrate to `bus::{property, signals}`                        |
| `crates/hytte-services/src/brightness.rs`     | 5          | Migrate to `bus::call`                                       |
| `crates/hytte-services/src/systemd.rs`        | 5          | Migrate to `bus::{call, signals}`                            |
| `crates/hytte-services/Cargo.toml`            | 6          | Remove direct `zbus` dependency                              |
| `Cargo.toml` (workspace root)                 | 6          | Add clippy `disallowed_methods` rule                         |

**Public API contract (must be preserved across every migration):**

Each service exposes a stable surface that consumers (widgets, other services) call. The migration must not change any of these signatures. The following are the load-bearing surfaces — verify them before/after each task:

- `notifications`: `service()`, `active() -> impl Signal<Item = Vec<Notification>>`, `history() -> impl Signal<Item = Vec<HistoryEntry>>`, `clear_history()`, `dismiss(id, reason)`, `invoke_action(id, key)`
- `wifi`: `service()`, `station() -> impl Signal<Item = Option<Station>>`, `adapter() -> impl Signal<Item = Option<Adapter>>`, `networks() -> impl Signal<Item = Vec<WifiNetwork>>`, `active_prompt() -> impl Signal<Item = Option<PromptRequest>>`, `scan()`, `connect_network(path)`, `disconnect()`, `set_powered(on)`, `submit_prompt(id, passphrase)`, `cancel_prompt(id)`
- `polkit`: `service()`, `auth_prompts() -> impl Signal<Item = Option<AuthPrompt>>`, `respond_to_auth(Option<(Zeroizing<String>, u32)>)`
- `screensaver`: `service()`, `inhibitors() -> impl Signal<Item = Vec<Inhibitor>>`, `is_locked() -> impl Signal<Item = bool>`, `lock()`, `inhibit(app, reason) -> u32`, `uninhibit(cookie)`, `handle_unlock_success()`
- `power_profiles`: `service()`, `state() -> impl Signal<Item = PowerProfilesState>`, `set_active(profile)`, `humanize_profile(name) -> String`
- `bluetooth`: large surface (see source). Migration preserves all `pub fn`s.
- `mpris`, `tray`, `networkd`, `upower`, `brightness`, `systemd`: see each source file for the contract.

---

## Task 1: Fix `signals.rs` cold-start epoch race (foundation review #1 prereq)

**Why first:** Phase 4's `notifications`, `polkit`, `screensaver` all consume `signals` at boot. Without this fix, every shell start spuriously bumps `missed_emissions`, which would cascade into spurious re-fetches across multiple services. Single targeted commit before any service migration.

**Files:**

- Modify: `crates/hytte-bus/src/signals.rs` (around line 259 — the `current_epoch` capture in `run_subscription`)

- [ ] **Step 1.1: Reproduce the race in a test**

Append to `crates/hytte-bus/tests/signals.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn cold_start_does_not_bump_missed_emissions() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let sub = signals_with(&shared, "mov.vibec0re.test.NoSignal")
        .at_path("/mov/vibec0re/test/NoSignal")
        .iface("mov.vibec0re.test.NoSignal")
        .signal("Pinged")
        .start();

    // Give the subscription task time to attempt subscribe, observe epoch_signal,
    // and (under the bug) spuriously bump missed_emissions because it captured
    // the epoch BEFORE with_conn ran (epoch was 1 at capture, 1 after with_conn,
    // so no bump — but if the timing's different, e.g. test_support inits epoch
    // to 0 and the supervisor bumps to 1, then we'd see a bump).
    //
    // For for_test_*, epoch starts at 1 already, so this test verifies the
    // SUBSCRIPTION'S internal epoch comparison stays consistent. With the fix
    // (capture epoch AFTER with_conn returns), the comparison is between the
    // epoch the subscription was built against and the supervisor's CURRENT
    // epoch — they should match on cold start.
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(sub.missed_count(), 0,
        "cold-start subscription must not bump missed_emissions before any disconnect");
}
```

- [ ] **Step 1.2: Run the test, watch it pass (or fail) with current code**

Run: `cargo test -p hytte-bus --test signals cold_start_does_not_bump_missed_emissions`

For `for_test_session` (epoch initialized to 1), the current code may already pass this test because the test setup pre-bumps the epoch. Document in the test what you observe.

If the test passes despite the bug existing, you may need a slightly different repro. The spec-reviewer-described race manifests in PRODUCTION cold start (where epoch starts at 0 and the supervisor bumps to 1 after first connect, but the subscription task captured 0 before that). That production path isn't reachable from `for_test_*`. The test above documents the INVARIANT: cold-start subscription must not bump `missed_emissions`. The fix is independent of whether this specific test fails.

- [ ] **Step 1.3: Apply the fix in `signals.rs`**

Locate `run_subscription` (around line 200-260). Find the `let current_epoch = shared.epoch();` (or equivalent) BEFORE the `with_conn` call. Move it to AFTER `with_conn` returns:

```rust
// BEFORE (buggy):
let current_epoch = shared.epoch();
let result = shared.with_conn(|conn| async move { /* subscribe */ }).await;
// drain_signal_stream then compares incoming epoch_signal updates against current_epoch
// (which was captured before the subscription was actually built)

// AFTER (fixed):
let result = shared.with_conn(|conn| async move { /* subscribe */ }).await;
let current_epoch = shared.epoch();  // captured AFTER subscribe succeeded
```

The exact line numbers depend on the current file shape; read `crates/hytte-bus/src/signals.rs` to find the right place. The semantic fix: `current_epoch` is the epoch the subscription was built against. Any future bump indicates a real reconnect after we subscribed.

- [ ] **Step 1.4: Run all tests**

Run: `cargo test -p hytte-bus`
Expected: all tests pass (existing + new cold_start test).

- [ ] **Step 1.5: Run clippy**

Run: `cargo clippy -p hytte-bus --tests -- -D warnings`
Expected: clean.

- [ ] **Step 1.6: Commit**

```bash
git add crates/hytte-bus/src/signals.rs crates/hytte-bus/tests/signals.rs
git commit -m "fix(hytte-bus): signals captures epoch after subscription succeeds

$(cat <<'EOF'
The subscription task previously captured the connection epoch BEFORE
calling with_conn, then drain_signal_stream compared incoming epoch
updates against that pre-call value. On cold start where the supervisor
hasn't bumped the epoch yet, this could spuriously fire missed_emissions
once the supervisor's first connect completed.

Fix: capture epoch AFTER with_conn returns. Now the value reflects the
epoch under which the subscription was actually built, so future bumps
genuinely indicate a reconnect after we were subscribed.

Foundation review #1 — addressed before Phase 4 migrations because
notifications/polkit/screensaver all consume signals at boot.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 4 — loud-offender migrations (the actual bug fix)

After Tasks 2-6, dbus-broker should no longer hit EMFILE. The five services responsible for ~30 connection-creates/min in the original bug all switch to the shared connection layer.

---

## Task 2: Migrate `power_profiles` (smallest of Phase 4 — start here)

**Why first in Phase 4:** smallest service, exercises `bus::property` (well-tested in foundation) plus a `bus::call` for the setter. Validates the pattern on a service slightly more complex than `resolved` before tackling the larger ones.

**Files:**

- Modify: `crates/hytte-services/src/power_profiles.rs` (rewrite the listen loop and command channel)

**Public API contract (must be preserved):**

- `pub fn service() -> PowerProfilesService`
- `pub fn state() -> impl Signal<Item = PowerProfilesState>`
- `pub fn set_active(profile: &str)` — fire-and-forget
- `pub fn humanize_profile(name: &str) -> String` — pure function, unchanged

`PowerProfilesState { active: String, available: Vec<String> }` is unchanged.

- [ ] **Step 2.1: Read the current source**

Run: Read `/home/choom/src/trollshell/crates/hytte-services/src/power_profiles.rs` end-to-end to understand the current dual-name fallback (`net.hadess.PowerProfiles` → `org.freedesktop.UPower.PowerProfiles`) and the Properties.Set command path.

The migration will:

1. Replace the polling listen loop with two `bus::property` subscriptions (one for `ActiveProfile: String`, one for `Profiles: Vec<HashMap<String, OwnedValue>>`) — both on the canonical `net.hadess.PowerProfiles` name.
2. Replace `do_set_active` with `bus::call(...).fire_and_forget()`.
3. Drop the `CMD_CONN` static + `cmd_conn`/`evict_cmd_conn`/`is_io_error` helpers entirely — the bus layer handles all of it.
4. Drop the dual-name fallback in v1 (the canonical name is what current systems use; if a user runs only the freedesktop alias, the property will stay in `Loading` and the UI will hide itself per the existing `available.is_empty()` check). If the fallback turns out to be needed, follow up by combining two `bus::property`-driven Mutables.

- [ ] **Step 2.2: Rewrite the file**

Replace the contents of `crates/hytte-services/src/power_profiles.rs` with:

```rust
//! Power profiles via `power-profiles-daemon`.
//!
//! Subscribes to `net.hadess.PowerProfiles` on the system bus. Emits a flat
//! [`PowerProfilesState`] every time `ActiveProfile` or `Profiles`
//! properties change.
//!
//! When power-profiles-daemon is not on the bus, both properties stay in
//! `Loading` and the emitted state is the default (empty `available`).
//! UI hides itself when `available.is_empty()`.

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_bus::{call, property, BusKind, PropState};
use hytte_reactive::{registry, runtime, Service};
use std::collections::HashMap;
use zbus::zvariant::{OwnedValue, Value};

#[derive(Clone, Debug, Default)]
pub struct PowerProfilesState {
    pub active: String,
    pub available: Vec<String>,
}

#[doc(hidden)]
pub struct PowerProfilesHandles {
    pub(crate) state: Mutable<PowerProfilesState>,
}

impl Default for PowerProfilesHandles {
    fn default() -> Self {
        Self {
            state: Mutable::new(PowerProfilesState::default()),
        }
    }
}

pub struct PowerProfilesService;

impl Service for PowerProfilesService {
    type Handles = PowerProfilesHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = PowerProfilesHandles::default();
        let writer = handles.state.clone();

        // Two parallel property subscriptions; we coalesce them into the
        // emitted PowerProfilesState by holding the last-known value of
        // each side and re-publishing on every change.
        let active_signal = property::<String>(CANONICAL_NAME)
            .bus(BusKind::System)
            .at_path(CANONICAL_PATH)
            .iface(CANONICAL_NAME)
            .name("ActiveProfile")
            .start();

        let profiles_signal = property::<Vec<HashMap<String, OwnedValue>>>(CANONICAL_NAME)
            .bus(BusKind::System)
            .at_path(CANONICAL_PATH)
            .iface(CANONICAL_NAME)
            .name("Profiles")
            .start();

        let active_writer = writer.clone();
        let profiles_writer = writer.clone();

        rt.spawn(async move {
            active_signal
                .signal()
                .for_each(move |state| {
                    let active = match state {
                        PropState::Loaded(v) | PropState::Stale(v) => v,
                        PropState::Loading => String::new(),
                    };
                    active_writer.lock_mut().active = active;
                    std::future::ready(())
                })
                .await;
        });

        rt.spawn(async move {
            profiles_signal
                .signal()
                .for_each(move |state| {
                    let raw = match state {
                        PropState::Loaded(v) | PropState::Stale(v) => v,
                        PropState::Loading => Vec::new(),
                    };
                    let available: Vec<String> = raw
                        .into_iter()
                        .filter_map(|m| {
                            m.get("Profile")
                                .and_then(|v| v.try_clone().ok())
                                .and_then(|v| String::try_from(v).ok())
                        })
                        .collect();
                    profiles_writer.lock_mut().available = available;
                    std::future::ready(())
                })
                .await;
        });

        handles
    }
}

#[must_use]
pub fn service() -> PowerProfilesService {
    PowerProfilesService
}

pub fn state() -> impl Signal<Item = PowerProfilesState> {
    registry::with(|r| {
        r.get::<PowerProfilesHandles>()
            .expect("power_profiles::service() not registered")
            .state
            .signal_cloned()
    })
}

pub fn set_active(profile: &str) {
    let profile = profile.to_string();
    runtime::handle().spawn(async move {
        let value = Value::from(profile.as_str()).try_to_owned().ok();
        let Some(value) = value else {
            tracing::warn!(profile, "power_profiles set_active: failed to wrap Value");
            return;
        };
        let result = call("org.freedesktop.DBus.Properties")
            .bus(BusKind::System)
            .at_path(CANONICAL_PATH)
            .iface("org.freedesktop.DBus.Properties")
            .method("Set")
            .args((CANONICAL_NAME, "ActiveProfile", value))
            .send::<()>()
            .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, profile, "power_profiles set_active failed");
        }
    });
}

#[must_use]
pub fn humanize_profile(name: &str) -> String {
    match name {
        "performance" => "Performance".to_string(),
        "balanced" => "Balanced".to_string(),
        "power-saver" => "Power saver".to_string(),
        other => other.to_string(),
    }
}

const CANONICAL_NAME: &str = "net.hadess.PowerProfiles";
const CANONICAL_PATH: &str = "/net/hadess/PowerProfiles";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_known_profiles() {
        assert_eq!(humanize_profile("performance"), "Performance");
        assert_eq!(humanize_profile("balanced"), "Balanced");
        assert_eq!(humanize_profile("power-saver"), "Power saver");
    }

    #[test]
    fn humanize_unknown_profile_passes_through() {
        assert_eq!(humanize_profile("custom-fast"), "custom-fast");
    }
}
```

The `Set` call uses `org.freedesktop.DBus.Properties.Set` directly because we're calling it as a generic D-Bus method, with a manually-constructed args tuple `(interface_name, property_name, OwnedValue)`. This is standard D-Bus property setting.

Note: the `call` accessor defaults to Session bus; we override with `.bus(BusKind::System)`.

The `target` parameter on `call(...)` is the destination D-Bus name (the bus name owning the object, e.g. `net.hadess.PowerProfiles`). For `Properties.Set` we put `CANONICAL_NAME` as the destination because that's the service we're calling. The `iface` is `org.freedesktop.DBus.Properties` because that's where `Set` lives.

Wait — this is wrong. The `call(destination)` arg is the BUS NAME (the destination peer). For Set on power-profiles-daemon, the destination is `net.hadess.PowerProfiles` (the daemon's well-known name) — correct in the snippet above. The iface is `org.freedesktop.DBus.Properties` — correct.

Verify by reading the existing code: it uses `conn.call_method(Some(name), path, Some("org.freedesktop.DBus.Properties"), "Set", &(name, "ActiveProfile", Value::from(profile)))`. The first `name` is destination, the second `name` is the interface-name argument to Set. Same in the migration.

- [ ] **Step 2.3: Verify compile + tests + clippy**

```sh
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --tests -- -D warnings
```

Existing `humanize_*` tests in `power_profiles.rs` should still pass — they test pure functions.

- [ ] **Step 2.4: Commit**

```bash
git add crates/hytte-services/src/power_profiles.rs
git commit -m "refactor(power_profiles): migrate to hytte::bus::{property, call}

$(cat <<'EOF'
ActiveProfile and Profiles tracked via two parallel bus::property
subscriptions, coalesced into the existing PowerProfilesState shape.
set_active uses bus::call(...).send() against
org.freedesktop.DBus.Properties.Set.

Drops:
- The 5-second listen-loop reconnect (bus layer handles reconnect)
- CMD_CONN static + cmd_conn/evict_cmd_conn/is_io_error helpers
- The dual-name fallback (canonical name is universal in practice;
  re-add as a separate task if a real user reports needing it)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Migrate `notifications`

**Files:**

- Modify: `crates/hytte-services/src/notifications.rs`

**Public API contract:**

- `pub fn service() -> NotificationsService`
- `pub fn active() -> impl Signal<Item = Vec<Notification>>`
- `pub fn history() -> impl Signal<Item = Vec<HistoryEntry>>`
- `pub fn clear_history()`
- `pub fn dismiss(id: u32, reason: u32)`
- `pub fn invoke_action(id: u32, action_key: &str)`

**Migration intent:**

1. The `Service::start` impl currently spawns a task that loops `listen()` with a 2-second flat-rate sleep. `listen()` opens a `Connection::session()`, calls `RequestName("org.freedesktop.Notifications")`, mounts the interface, and pumps the event loop.
2. Replace the entire loop with `bus::own_name("org.freedesktop.Notifications").at_path("/org/freedesktop/Notifications", iface).start()`. The returned `OwnNameSignal` provides reactive state if the UI ever wants to surface "notifications daemon: PermanentlyTaken (mako)".
3. `do_dismiss` and `do_invoke_action` open a fresh `Connection::session()` to emit `NotificationClosed` and `ActionInvoked` signals on the session bus. These become `bus::call(...).fire_and_forget()` (since they're on a session bus and emit a SIGNAL, not a method call... actually D-Bus signals are emitted differently from method calls — see Step 3.1 carefully).

**Subtlety:** signal emission vs method call. `NotificationClosed` and `ActionInvoked` are D-Bus SIGNALS that the Notifications interface emits. They're emitted by writing to the bus with the right header — zbus's `SignalEmitter` API. The current code constructs a `SignalEmitter` from the connection and calls the macro-generated emit method.

Since we OWN the interface (`org.freedesktop.Notifications`), and the interface itself is mounted via `bus::own_name(...).at_path(path, iface)`, the bus layer owns the connection that holds the interface. To emit signals from `dismiss()`/`invoke_action()` (which are called from outside the interface methods), we have two options:

**Option A: Add a method to the interface that emits the signal, and call it via the bus.**

```rust
#[zbus::interface]
impl NotificationsIface {
    // Existing methods (Notify, CloseNotification, GetCapabilities, etc.)

    // Programmatic-emit method (rare — usually signals are emitted from inside
    // method handlers). Marked private (starts with _) by D-Bus convention but
    // accessible from our own code via bus::call.
    async fn _emit_closed(&self, id: u32, reason: u32, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>)
        -> zbus::fdo::Result<()> {
        Self::notification_closed(&emitter, id, reason).await?;
        Ok(())
    }
}
```

Then `do_dismiss` calls `bus::call(...).method("_emit_closed").args((id, reason)).send().await`.

This works but adds a private method to our public interface — kludgy.

**Option B: Hold a reference to the SignalEmitter in our handles.**

The bus layer owns the Connection. If we expose the connection (or an emitter built from it) via the registry, `dismiss()`/`invoke_action()` can use it directly. But this leaks the Connection abstraction out of the bus layer, defeating the whole design.

**Option C (RECOMMENDED): Keep the dismiss/invoke_action signals inside the iface impl, triggered by the interface's own method handlers.**

The current dismiss and invoke_action are public functions called by the UI when the user clicks/dismisses. They emit signals. But they DON'T need to emit via the bus from outside — they can WRITE to the local Mutable<Vec<Notification>> (which is what they already do for active list management) AND the UI can react to that. The signal emission to OTHER bus clients (not us) is for other notification consumers... which on a typical trollshell setup are zero.

Read the spec for `org.freedesktop.Notifications`: `NotificationClosed` and `ActionInvoked` signals are how the SENDING APP (e.g. Firefox showing a notification) finds out that the user dismissed/clicked. These signals MUST be emitted on the bus or sending apps won't update their UI (e.g. a "notification minimized" indicator).

So we CAN'T just skip the emission. Option A is the cleanest.

Alternative cleaner path: **take a SignalEmitter-providing handle out of the bus layer**. The `OwnNameSignal` returned by `bus::own_name` could carry a `signal_emitter()` accessor. But that requires a hytte-bus API change — out of scope for Phase 4 unless we want to add it deliberately.

For v1 of the migration, do **Option A**: add `_emit_closed(id, reason)` and `_emit_invoked(id, action_key)` as private methods on the interface; `dismiss()`/`invoke_action()` call them via `bus::call`. Document the kludge with a `// TODO(post-Phase-4): expose a SignalEmitter via OwnNameSignal so we can emit directly.` comment.

- [ ] **Step 3.1: Add `_emit_closed` and `_emit_invoked` private methods to `NotificationsIface`**

Locate the `#[zbus::interface(...)] impl NotificationsIface { ... }` block in `notifications.rs`. Add two methods alongside the existing public ones:

```rust
    /// Emit `NotificationClosed` programmatically. Called by `dismiss()`
    /// from outside the interface, routed through bus::call so the bus
    /// layer's owned Connection emits it.
    ///
    /// Marked private with leading underscore (D-Bus convention) — not
    /// part of the public Notifications spec but harmless for clients
    /// that don't introspect.
    #[allow(non_snake_case)]
    async fn _EmitClosed(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::fdo::Result<()> {
        Self::notification_closed(&emitter, id, reason)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    /// Emit `ActionInvoked` programmatically.
    #[allow(non_snake_case)]
    async fn _EmitInvoked(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        id: u32,
        action_key: String,
    ) -> zbus::fdo::Result<()> {
        Self::action_invoked(&emitter, id, &action_key)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }
```

The `notification_closed` and `action_invoked` macro-generated methods are static (taking `&SignalEmitter<'_>`); the `#[zbus(signal_emitter)]` attribute on the parameter injects the emitter for the current invocation.

- [ ] **Step 3.2: Rewrite `Service::start`**

Replace the whole `impl Service for NotificationsService { fn start(...) { ... } }` block with:

```rust
impl Service for NotificationsService {
    type Handles = NotificationsHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NotificationsHandles::default();
        let active = handles.active.clone();
        let next_id = handles.next_id.clone();
        let history = handles.history.clone();

        let iface = NotificationsIface { active, next_id, history };

        // Own the well-known name + mount the interface. The bus layer
        // handles connection lifecycle, RequestName retries, and per-owner
        // back-off if mako/dunst is camping the name.
        let _ownership = hytte_bus::own_name("org.freedesktop.Notifications")
            .at_path("/org/freedesktop/Notifications", iface)
            .start();

        // The OwnNameSignal is dropped here (clones live inside the bus
        // layer's task), so the consumer-facing observability of "are we
        // the daemon right now?" isn't surfaced. If a future widget wants
        // to render the OwnState (e.g. a tray indicator showing
        // "notifications: mako"), expose it via NotificationsHandles.

        handles
    }
}
```

The `NotificationsIface` struct (the interface implementation type) needs to take ownership of the Mutables it writes to, OR clone them per-handler. Current code holds a `State` struct. Verify the current `NotificationsIface` struct and adapt: if it currently borrows `&Connection`, refactor to remove that — the bus layer owns the connection now. The interface methods just read/write the local Mutables.

Specifically: search for any `impl NotificationsIface` field holding `Connection` and remove it. The `signal_emitter` is provided per-call via `#[zbus(signal_emitter)]`.

- [ ] **Step 3.3: Rewrite `do_dismiss`**

Locate `async fn do_dismiss(...)` (around line 224). Replace its body:

```rust
async fn do_dismiss(id: u32, reason: u32) -> Result<()> {
    let handles = registry::with(|r| {
        r.get::<NotificationsHandles>()
            .map(|h| (h.active.clone(), h.history.clone()))
    });
    let Some((active, history)) = handles else {
        return Ok(());
    };
    // Existing local-state mutation (remove from active, push to history)
    let removed = {
        let mut list = active.lock_mut();
        let pos = list.iter().position(|n| n.id == id);
        pos.map(|i| list.remove(i))
    };
    if let Some(n) = removed {
        let dismissed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entry = HistoryEntry {
            id: n.id,
            app_name: n.app_name,
            app_icon: n.app_icon,
            summary: n.summary,
            body: n.body,
            urgency: n.urgency,
            image: n.image,
            actions: n.actions,
            reason,
            created_at: n.created_at,
            dismissed_at,
        };
        let mut hist = history.lock_mut();
        hist.insert(0, entry);
        hist.truncate(100);
    }
    // Emit the D-Bus signal via our own interface's _EmitClosed method.
    hytte_bus::call("org.freedesktop.Notifications")
        .at_path("/org/freedesktop/Notifications")
        .iface("org.freedesktop.Notifications")
        .method("_EmitClosed")
        .args((id, reason))
        .send::<()>()
        .await
        .context("emit NotificationClosed via _EmitClosed")?;
    Ok(())
}
```

Note `bus::call` defaults to Session bus, which is correct here.

- [ ] **Step 3.4: Rewrite `do_invoke_action`**

Same pattern. Locate `async fn do_invoke_action` (around line 287):

```rust
async fn do_invoke_action(id: u32, action_key: &str) -> Result<()> {
    hytte_bus::call("org.freedesktop.Notifications")
        .at_path("/org/freedesktop/Notifications")
        .iface("org.freedesktop.Notifications")
        .method("_EmitInvoked")
        .args((id, action_key.to_string()))
        .send::<()>()
        .await
        .context("emit ActionInvoked via _EmitInvoked")?;
    Ok(())
}
```

- [ ] **Step 3.5: Drop dead code**

Remove from `notifications.rs`:

- `use zbus::Connection;` (no longer used)
- Any `Connection::session()` calls anywhere in the file
- The `State` struct's `conn: Connection` field if it has one (and adjust `State::new`)
- Imports of `SignalEmitter` if they were only used in the now-deleted code

- [ ] **Step 3.6: Verify**

```sh
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --tests -- -D warnings
```

- [ ] **Step 3.7: Commit**

```bash
git add crates/hytte-services/src/notifications.rs
git commit -m "refactor(notifications): migrate to hytte::bus::{own_name, call}

$(cat <<'EOF'
The Notifications interface is mounted via bus::own_name; signal
emissions (NotificationClosed, ActionInvoked) route through two private
self-call methods (_EmitClosed, _EmitInvoked) so dismiss() and
invoke_action() from outside the interface can trigger them via
bus::call without holding a Connection.

Drops the 2-second flat-rate reconnect loop that was creating ~30
Connection::session() per minute when mako was camping the name (the
biggest contributor to the dbus-broker EMFILE).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Migrate `screensaver`

**Files:**

- Modify: `crates/hytte-services/src/screensaver.rs`

**Public API contract:**

- `pub fn service() -> ScreenSaverService`
- `pub fn inhibitors() -> impl Signal<Item = Vec<Inhibitor>>`
- `pub fn is_locked() -> impl Signal<Item = bool>`
- `pub fn lock()`
- `pub fn inhibit(application: &str, reason: &str) -> u32`
- `pub fn uninhibit(cookie: u32)`
- `pub fn handle_unlock_success()`

**Migration intent:**

1. The current `Service::start` spawns TWO tasks: one runs `run_server` (owns the `org.freedesktop.ScreenSaver` name + mounts the interface at TWO paths `/org/freedesktop/ScreenSaver` and `/ScreenSaver`); the other runs `listen_login1` (subscribes to `Session.Lock`/`Unlock` on the user's logind session).
2. `run_server` becomes `bus::own_name(...).at_path(canonical, iface).at_path(legacy, iface).start()`. (The `at_path` builder method allows multiple calls per the foundation spec.)
3. `listen_login1` becomes `bus::signals(LOGIN1).at_path(session_path).iface(...).signal("Lock").start()` + same for `Unlock`. Subscribe to events. Use `missed_emissions` to re-fetch authoritative state on reconnect (specifically: GetLockedHint).
4. `call_login1_unlock` becomes `bus::call(...)` x2 (one for `GetSessionByPID`, one for `SetLockedHint`).

**Subtlety: getting our session path.** Both the lock listener AND the SetLockedHint call need the session path, obtained via `Manager.GetSessionByPID(pid)`. Currently each function does this independently. After migration: cache the session path in a `OnceLock<String>` and resolve it lazily on first use. The path doesn't change for the process lifetime.

- [ ] **Step 4.1: Rewrite `Service::start`**

Locate `impl Service for ScreenSaverService { fn start(...) { ... } }`. Replace with:

```rust
impl Service for ScreenSaverService {
    type Handles = ScreenSaverHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = ScreenSaverHandles::default();
        let state = handles.state.clone();
        let inhibitors_view = handles.inhibitors.clone();
        let next_cookie = handles.next_cookie.clone();
        let locked_writer = handles.is_locked.clone();

        let iface = ScreenSaverIface {
            state,
            inhibitors: inhibitors_view,
            next_cookie,
        };

        // Own the well-known name on session bus, mount at both paths.
        let _ownership = hytte_bus::own_name("org.freedesktop.ScreenSaver")
            .at_path(PATH_CANONICAL, iface.clone())
            .at_path(PATH_LEGACY, iface)
            .start();

        // Start the login1 Session.Lock/Unlock listener (Step 4.2 implements this).
        rt.spawn(spawn_login1_listener(locked_writer));

        handles
    }
}
```

`ScreenSaverIface` needs `Clone` for the two `.at_path` mounts. The struct uses `Arc` internally already, so `#[derive(Clone)]` is cheap. If it isn't currently Clone, add the derive.

- [ ] **Step 4.2: Implement `spawn_login1_listener`**

Add a free function that owns the login1-side flow:

```rust
async fn spawn_login1_listener(is_locked: Mutable<bool>) {
    // Cache the session path; resolve once.
    let session_path = match resolve_session_path().await {
        Ok(p) => p,
        Err(e) => {
            tracing::info!(error = %e,
                "no logind session for this process — login1 lock signals disabled");
            return;
        }
    };

    let lock_sub = hytte_bus::signals("org.freedesktop.login1")
        .at_path(session_path.as_str().to_string())
        .iface("org.freedesktop.login1.Session")
        .signal("Lock")
        .start();
    let unlock_sub = hytte_bus::signals("org.freedesktop.login1")
        .at_path(session_path.as_str().to_string())
        .iface("org.freedesktop.login1.Session")
        .signal("Unlock")
        .start();

    let lock_writer = is_locked.clone();
    let unlock_writer = is_locked.clone();
    let lock_path = session_path.clone();
    let unlock_path = session_path.clone();
    let lock_writer_for_missed = is_locked.clone();
    let unlock_writer_for_missed = is_locked.clone();

    // Lock events: set is_locked=true.
    let mut lock_stream = lock_sub.events();
    tokio::spawn(async move {
        use futures_util::StreamExt;
        while let Some(_) = lock_stream.next().await {
            lock_writer.set(true);
        }
    });

    // Unlock events: set is_locked=false.
    let mut unlock_stream = unlock_sub.events();
    tokio::spawn(async move {
        use futures_util::StreamExt;
        while let Some(_) = unlock_stream.next().await {
            unlock_writer.set(false);
        }
    });

    // On missed emissions (reconnect), re-fetch authoritative state via GetLockedHint.
    let lock_path_for_missed = lock_path.clone();
    let lock_missed = lock_sub.missed_emissions();
    use futures_signals::signal::SignalExt;
    tokio::spawn(async move {
        lock_missed
            .for_each(move |_| {
                let path = lock_path_for_missed.clone();
                let writer = lock_writer_for_missed.clone();
                async move {
                    match get_locked_hint(&path).await {
                        Ok(locked) => writer.set(locked),
                        Err(e) => tracing::debug!(error = %e, "GetLockedHint after reconnect"),
                    }
                }
            })
            .await;
    });

    // Same for unlock.
    let unlock_path_for_missed = unlock_path.clone();
    let unlock_missed = unlock_sub.missed_emissions();
    tokio::spawn(async move {
        unlock_missed
            .for_each(move |_| {
                let path = unlock_path_for_missed.clone();
                let writer = unlock_writer_for_missed.clone();
                async move {
                    match get_locked_hint(&path).await {
                        Ok(locked) => writer.set(locked),
                        Err(e) => tracing::debug!(error = %e, "GetLockedHint after reconnect"),
                    }
                }
            })
            .await;
    });
}

static SESSION_PATH: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

async fn resolve_session_path() -> Result<String, hytte_bus::BusError> {
    SESSION_PATH
        .get_or_try_init(|| async {
            let pid: u32 = std::process::id();
            let path: zbus::zvariant::OwnedObjectPath =
                hytte_bus::call("org.freedesktop.login1")
                    .bus(hytte_bus::BusKind::System)
                    .at_path("/org/freedesktop/login1")
                    .iface("org.freedesktop.login1.Manager")
                    .method("GetSessionByPID")
                    .args((pid,))
                    .send()
                    .await?;
            Ok(path.as_str().to_string())
        })
        .await
        .cloned()
}

async fn get_locked_hint(session_path: &str) -> Result<bool, hytte_bus::BusError> {
    hytte_bus::call("org.freedesktop.login1")
        .bus(hytte_bus::BusKind::System)
        .at_path(session_path.to_string())
        .iface("org.freedesktop.login1.Session")
        .method("GetLockedHint")
        .args(())
        .send::<bool>()
        .await
}
```

- [ ] **Step 4.3: Rewrite `call_login1_unlock`**

Replace the body of `async fn call_login1_unlock()` with:

```rust
async fn call_login1_unlock() -> anyhow::Result<()> {
    use anyhow::Context;
    let session_path = resolve_session_path()
        .await
        .context("resolve login1 session path")?;
    hytte_bus::call("org.freedesktop.login1")
        .bus(hytte_bus::BusKind::System)
        .at_path(session_path)
        .iface("org.freedesktop.login1.Session")
        .method("SetLockedHint")
        .args((false,))
        .send::<()>()
        .await
        .context("Session.SetLockedHint(false)")?;
    Ok(())
}
```

- [ ] **Step 4.4: Drop dead code**

Remove from `screensaver.rs`:

- `async fn run_server(...)` and the body that constructed the connection + RequestName + NameOwnerChanged loop (whole function gone).
- `async fn listen_login1(...)` (whole function gone — replaced by `spawn_login1_listener`).
- `use zbus::{fdo, Connection};` if unused (Connection definitely unused now; fdo may still be used for RequestNameFlags etc — verify).
- Any unused imports.

Keep `ScreenSaverIface` (the interface impl).
Keep `inhibit`, `uninhibit`, `lock`, `handle_unlock_success`, `Inhibitor`, etc.
Keep the swayidle SIGSTOP/SIGCONT helpers (those don't go through D-Bus).

- [ ] **Step 4.5: Verify**

```sh
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --tests -- -D warnings
```

- [ ] **Step 4.6: Commit**

```bash
git add crates/hytte-services/src/screensaver.rs
git commit -m "refactor(screensaver): migrate to hytte::bus::{own_name, signals, call}

$(cat <<'EOF'
ScreenSaver interface mounted via bus::own_name at both canonical and
legacy paths. Session.Lock/Unlock subscribed via bus::signals; on
missed_emissions (reconnect), re-fetch authoritative state via
GetLockedHint. SetLockedHint(false) and the GetSessionByPID resolver
use bus::call. Session path is cached lazily in a OnceCell.

Drops:
- run_server (RequestName + NameOwnerChanged loop)
- listen_login1 (per-iteration Connection::system)
- The 5-second flat-rate reconnect

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Migrate `polkit`

**Files:**

- Modify: `crates/hytte-services/src/polkit.rs`

**Public API contract:**

- `pub fn service() -> PolkitService`
- `pub fn auth_prompts() -> impl Signal<Item = Option<AuthPrompt>>`
- `pub fn respond_to_auth(reply: Option<(Zeroizing<String>, u32)>)`

**Migration intent:**

1. The polkit agent has a peculiar architecture: the `AuthAgent` interface is mounted on the SESSION bus (so polkitd can call back into us); registration happens on the SYSTEM bus by calling `RegisterAuthenticationAgent` on the polkit Authority.
2. The current `run_agent` opens both a `Connection::session()` (mounts the interface) and a `Connection::system()` (calls RegisterAuthenticationAgent + watches NameOwnerChanged on `org.freedesktop.PolicyKit1` to detect polkitd restart).
3. After migration: `bus::own_name` for an EMPTY name on session bus won't work — we don't want to OWN a name, we just want to mount our interface at a path on the session bus. Hmm.

**Key insight:** the polkit AuthAgent isn't owning a well-known name. It mounts an object at a path (the AGENT_PATH constant) and registers THAT path with polkit. polkit calls the agent via `<our_unique_name> + AGENT_PATH`. So we need:

1. A way to mount an interface on the SESSION bus without owning a name. The current `bus::own_name` requires a name. Workaround: own a unique name that nobody else cares about, or don't go through `own_name` at all.

This is a real gap in the bus API. Options:

**Option A: Add `bus::serve_at(path, iface).bus(BusKind::Session).start()` for "mount without owning a name."** This is a foundation-level API addition — 1 day of work.

**Option B: Use the SAME ownership mechanism but with a unique name we generate.** E.g. `mov.vibec0re.trollshell.PolkitAgent` (or any name we generate). polkit calls back via our unique name (`:1.42`), not the well-known one — so the well-known name is irrelevant from polkit's perspective. We just use `bus::own_name` to anchor the interface.

**Option C: Don't migrate polkit in this round. Keep polkit on direct zbus until we add `serve_at`.**

For Phase 4: choose **Option B**. Own a name that nobody contests (e.g. `mov.vibec0re.trollshell.polkit-agent.<pid>` to avoid collisions). polkit calls our unique-name path; the well-known name is just an anchor for the interface mount.

Actually, even simpler: **Option D: own the agent path under a single trollshell-scoped name that all our private interfaces share.** But that name doesn't exist yet.

**Cleanest: Option B with a per-process unique well-known name.** `mov.vibec0re.trollshell.polkit-agent` works for production (only one trollshell per session) and tests can use a `.test-<n>` suffix.

Actually re-reading carefully: polkit's RegisterAuthenticationAgent takes an `object_path` (string). It then calls back at `<our_unique_bus_name>:<object_path>`. So our well-known name is irrelevant to polkit — what matters is that the agent OBJECT is mounted on a connection that polkit can reach by unique name.

The bus layer's SharedConnection has a unique name (the `:1.42`-style name assigned by the broker). When we mount an interface via `bus::own_name(...).at_path(path, iface)`, the iface is mounted on the SharedConnection's underlying connection. polkit calling `<unique_name>:AGENT_PATH` reaches our interface.

So **we need `own_name` to mount the interface but don't actually need polkit to know our well-known name**. The well-known name we own is just a "I'm here, please don't kick me out" placeholder for the bus layer's accounting.

Choose **Option B**: own `mov.vibec0re.trollshell.polkit-agent`.

For Phase 4 the plan is to move forward with Option B. If the polkit-agent name turns out to collide with someone else's session (extremely unlikely), refactor to Option A in Phase 5/6.

- [ ] **Step 5.1: Rewrite `Service::start`**

Locate `impl Service for PolkitService { fn start(...) { ... } }`. Replace with:

```rust
impl Service for PolkitService {
    type Handles = PolkitHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = PolkitHandles::default();
        rt.spawn(async move {
            if let Err(e) = run_agent_lifecycle().await {
                tracing::error!(error = %e, "polkit agent setup failed");
            }
        });
        handles
    }
}

const ANCHOR_NAME: &str = "mov.vibec0re.trollshell.polkit-agent";

async fn run_agent_lifecycle() -> Result<()> {
    let session_id = current_session_id().context("XDG_SESSION_ID unset")?;
    let subject = build_subject(&session_id).context("build polkit subject")?;

    // Mount the agent interface on the session bus by owning a private name.
    // The well-known name is just an anchor for the bus layer; polkit calls
    // back via our unique-name path.
    let agent = AuthAgent { /* fields */ };  // see Step 5.2 for the struct refactor
    let _ownership = hytte_bus::own_name(ANCHOR_NAME)
        .at_path(AGENT_PATH, agent)
        .start();

    // Register with polkit Authority on system bus.
    register_with_authority(subject.clone()).await
        .context("Authority.RegisterAuthenticationAgent")?;
    tracing::info!(session = %session_id, "polkit authentication agent registered");

    // Watch polkitd's well-known name; on disappearance, re-register.
    let polkitd_changes = hytte_bus::signals("org.freedesktop.DBus")
        .bus(hytte_bus::BusKind::System)
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start();

    use futures_util::StreamExt;
    let mut events = polkitd_changes.events();
    while let Some(event) = events.next().await {
        let Ok(args) = event.body.body().deserialize::<(String, String, String)>() else { continue };
        let (name, _old, new) = args;
        if name != "org.freedesktop.PolicyKit1" { continue; }
        if new.is_empty() {
            tracing::warn!("polkitd disappeared — re-registering after restart");
            // Wait briefly for polkitd to come back, then re-register.
            // bus::signals will keep delivering NameOwnerChanged events, so
            // a subsequent event with new != "" will trigger us again.
            continue;
        }
        // polkitd back: re-register our agent.
        if let Err(e) = register_with_authority(subject.clone()).await {
            tracing::warn!(error = %e, "re-RegisterAuthenticationAgent failed");
        } else {
            tracing::info!("polkit agent re-registered after polkitd restart");
        }
    }
    Ok(())
}

async fn register_with_authority(
    subject: (String, HashMap<String, OwnedValue>),
) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call("org.freedesktop.PolicyKit1")
        .bus(hytte_bus::BusKind::System)
        .at_path("/org/freedesktop/PolicyKit1/Authority")
        .iface("org.freedesktop.PolicyKit1.Authority")
        .method("RegisterAuthenticationAgent")
        .args((subject, "en_US.UTF-8".to_string(), AGENT_PATH.to_string()))
        .send::<()>()
        .await
}
```

- [ ] **Step 5.2: Refactor `AuthAgent` struct (drop `system_conn`)**

The current `AuthAgent` holds `system_conn: Arc<Connection>` because its methods (BeginAuthentication, AuthenticationAgentResponse2) need to call back to the system bus. After migration, those methods use `bus::call(...)` directly.

Locate the `struct AuthAgent { ... }` definition and the `#[zbus::interface] impl AuthAgent { ... }` block. Remove the `system_conn` field. Inside the interface methods that previously used `self.system_conn`, replace with `bus::call(...)`.

Specifically, look for places like:

```rust
self.system_conn.call_method(
    Some("org.freedesktop.PolicyKit1"),
    "/org/freedesktop/PolicyKit1/AuthenticationAgent",
    Some("org.freedesktop.PolicyKit1.AuthenticationAgent"),
    "AuthenticationAgentResponse2",
    &(...)
).await
```

Replace with:

```rust
hytte_bus::call("org.freedesktop.PolicyKit1")
    .bus(hytte_bus::BusKind::System)
    .at_path("/org/freedesktop/PolicyKit1/AuthenticationAgent")
    .iface("org.freedesktop.PolicyKit1.AuthenticationAgent")
    .method("AuthenticationAgentResponse2")
    .args(/* same tuple as before, minus the prefixed Some/None wrappers */)
    .send::<()>()
    .await
```

- [ ] **Step 5.3: Drop dead code**

- `async fn run_agent()` — entire function gone (replaced by `run_agent_lifecycle`).
- `use zbus::Connection;` — gone if only used by run_agent.
- The 5-second flat-rate reconnect sleep at the top of `Service::start` — gone.

Keep `respond_to_auth`, `auth_prompts`, the `AuthPrompt`/`AuthIdentity` types, `current_session_id`, `build_subject`, `pending_response_arc`, `clear_prompt`, `Authenticator` struct.

- [ ] **Step 5.4: Verify**

```sh
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --tests -- -D warnings
```

- [ ] **Step 5.5: Commit**

```bash
git add crates/hytte-services/src/polkit.rs
git commit -m "refactor(polkit): migrate to hytte::bus::{own_name, signals, call}

$(cat <<'EOF'
AuthAgent interface mounted via bus::own_name on a private well-known
name (mov.vibec0re.trollshell.polkit-agent — irrelevant to polkit, just an
anchor for the bus layer's connection). RegisterAuthenticationAgent +
AuthenticationAgentResponse2 calls go through bus::call. polkitd
restart detection via bus::signals on NameOwnerChanged for
org.freedesktop.PolicyKit1.

AuthAgent struct no longer carries system_conn: Arc<Connection> — its
methods call out via bus::call directly.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Migrate `wifi`

**Files:**

- Modify: `crates/hytte-services/src/wifi.rs` (largest of Phase 4 — ~1100 lines)

**Public API contract:**

- `pub fn service() -> WifiService`
- `pub fn station() -> impl Signal<Item = Option<Station>>`
- `pub fn adapter() -> impl Signal<Item = Option<Adapter>>`
- `pub fn networks() -> impl Signal<Item = Vec<WifiNetwork>>`
- `pub fn active_prompt() -> impl Signal<Item = Option<PromptRequest>>`
- `pub fn scan()`, `connect_network(path)`, `disconnect()`, `set_powered(on)`
- `pub fn submit_prompt(id, passphrase)`, `cancel_prompt(id)`

**Migration intent:**

iwd's bus layout:

- Service: `net.connman.iwd` on system bus
- ObjectManager at root: `net.connman.iwd:/` exposes `org.freedesktop.DBus.ObjectManager`
- Per-station object: `net.connman.iwd:/.../Station` exposes `net.connman.iwd.Station`
- Per-network object: `net.connman.iwd:/.../Network` exposes `net.connman.iwd.Network`
- Per-adapter object: `net.connman.iwd:/.../Adapter` exposes `net.connman.iwd.Adapter`

The current `listen()` function:

1. Connects to system bus
2. ObjectManager.GetManagedObjects → discover Station path
3. Subscribe to Station's PropertiesChanged
4. Subscribe to ObjectManager's InterfacesAdded/Removed for network visibility
5. Loop: dispatch events, refresh state, write to Mutables

Plus a parallel passphrase-prompt agent (the `Agent` interface mounted at our path so iwd can prompt us for passwords).

After migration:

- Use `bus::signals(...)` for ObjectManager.InterfacesAdded/Removed and Station.PropertiesChanged
- Use `bus::call(...)` for GetManagedObjects, Scan, Connect, Disconnect, SetPowered
- Use `bus::own_name(...)` for the Agent (same anchor-name trick as polkit — own a private name like `mov.vibec0re.trollshell.iwd-agent`)
- Use `bus::property::<bool>` for Adapter.Powered

This file is large enough that the migration is mostly mechanical pattern translation. Rather than provide every line of the after-state, the plan provides the migration recipe.

- [ ] **Step 6.1: Strip the per-iteration Connection wholesale**

Locate the `Service::start` block and the inner `listen(...)` function. Replace `Service::start` with:

```rust
impl Service for WifiService {
    type Handles = WifiHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = WifiHandles::default();
        let station_mutable = handles.station.clone();
        let networks_mutable = handles.networks.clone();
        let prompts_mutable = handles.prompts.clone();
        let adapter_mutable = handles.adapter.clone();

        rt.spawn(run_wifi_watcher(
            station_mutable,
            networks_mutable,
            prompts_mutable,
            adapter_mutable,
        ));

        handles
    }
}
```

`run_wifi_watcher` is the new top-level async function that drives discovery + subscription + state propagation. Implementation in the next step.

- [ ] **Step 6.2: Implement `run_wifi_watcher`**

This function:

1. Calls `bus::call(...).method("GetManagedObjects")` against `net.connman.iwd:/` to discover the Station object path.
2. Subscribes to `bus::signals(...).iface("net.connman.iwd.Station").signal("PropertiesChanged")` for state updates.
3. Subscribes to `bus::signals(...).iface("org.freedesktop.DBus.ObjectManager").signal("InterfacesAdded")` and `InterfacesRemoved` for network add/remove.
4. Mounts the Agent interface via `bus::own_name("mov.vibec0re.trollshell.iwd-agent").at_path(AGENT_PATH, agent)` and registers it with iwd's AgentManager.

Pseudocode:

```rust
async fn run_wifi_watcher(
    station_mutable: Mutable<Option<Station>>,
    networks_mutable: Mutable<Vec<WifiNetwork>>,
    prompts_mutable: Mutable<Option<PromptRequest>>,
    adapter_mutable: Mutable<Option<Adapter>>,
) {
    // 1. Discover paths via GetManagedObjects.
    // The reply type is HashMap<ObjectPath, HashMap<InterfaceName, HashMap<String, OwnedValue>>>.
    let managed: HashMap<zbus::zvariant::OwnedObjectPath,
        HashMap<String, HashMap<String, OwnedValue>>> = match
        hytte_bus::call("net.connman.iwd")
            .bus(BusKind::System)
            .at_path("/")
            .iface("org.freedesktop.DBus.ObjectManager")
            .method("GetManagedObjects")
            .args(())
            .send()
            .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(error = %e, "iwd GetManagedObjects failed (iwd not running?)");
            // Wait for iwd to appear; bus::signals on NameOwnerChanged for
            // net.connman.iwd would be the right pattern here. For v1, just
            // sleep and retry.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            return Box::pin(run_wifi_watcher(
                station_mutable, networks_mutable, prompts_mutable, adapter_mutable
            )).await;
        }
    };

    // 2. Find the first Station path.
    let station_path = managed.iter()
        .find_map(|(path, ifaces)| {
            ifaces.contains_key("net.connman.iwd.Station").then(|| path.clone())
        });

    let Some(station_path) = station_path else {
        tracing::debug!("iwd has no Station object yet");
        // Re-poll later; in v1 just sleep + recurse.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        return Box::pin(run_wifi_watcher(
            station_mutable, networks_mutable, prompts_mutable, adapter_mutable
        )).await;
    };

    // 3. Update STATION_PATH static so command functions know where to send.
    set_station_path(station_path.as_str().to_string()).await;

    // 4. Discover and update Adapter, populate networks list.
    // (Equivalent to the current refresh logic; build it from `managed`.)
    refresh_adapter_from_managed(&managed, &adapter_mutable);
    refresh_networks_from_managed(&managed, &networks_mutable);
    refresh_station_from_managed(&managed, &station_path, &station_mutable);

    // 5. Subscribe to Station PropertiesChanged.
    let station_props = hytte_bus::signals("net.connman.iwd")
        .bus(BusKind::System)
        .at_path(station_path.as_str().to_string())
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start();

    // 6. Subscribe to ObjectManager InterfacesAdded/Removed.
    let added_sub = hytte_bus::signals("net.connman.iwd")
        .bus(BusKind::System)
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal("InterfacesAdded")
        .start();
    let removed_sub = hytte_bus::signals("net.connman.iwd")
        .bus(BusKind::System)
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal("InterfacesRemoved")
        .start();

    // 7. Mount the Agent + register with iwd AgentManager.
    let agent = WifiAgent::new(prompts_mutable.clone());
    let _ownership = hytte_bus::own_name("mov.vibec0re.trollshell.iwd-agent")
        .at_path(AGENT_PATH, agent)
        .start();
    let _ = register_iwd_agent().await;

    // 8. Pump events forever.
    use futures_util::StreamExt;
    let mut station_events = station_props.events();
    let mut added_events = added_sub.events();
    let mut removed_events = removed_sub.events();

    loop {
        tokio::select! {
            Some(_) = station_events.next() => {
                // Re-fetch Station properties via Properties.GetAll, update station_mutable.
                refresh_station(&station_path, &station_mutable).await;
            }
            Some(_) = added_events.next() => {
                refresh_networks(&networks_mutable).await;
            }
            Some(_) = removed_events.next() => {
                refresh_networks(&networks_mutable).await;
            }
        }
    }
}
```

Implement the helper free functions (`refresh_station`, `refresh_networks`, `refresh_adapter_from_managed`, `register_iwd_agent`, `set_station_path`) as straightforward `bus::call` wrappers. The current code has equivalents — port them keeping the existing parsing logic.

- [ ] **Step 6.3: Convert command functions (`scan`, `connect_network`, `disconnect`, `set_powered`)**

Each is a `runtime::handle().spawn(...)` calling `do_station_call(path, method)` or `do_network_call(path, method)` or `do_adapter_set_property`.

Replace `do_station_call`:

```rust
async fn do_station_call(path: &str, method: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call("net.connman.iwd")
        .bus(BusKind::System)
        .at_path(path.to_string())
        .iface("net.connman.iwd.Station")
        .method(method)
        .args(())
        .send::<()>()
        .await
}
```

Same shape for `do_network_call` (iface `net.connman.iwd.Network`).

For `set_powered`:

```rust
async fn do_set_powered(adapter_path: &str, on: bool) -> Result<(), hytte_bus::BusError> {
    let value = zbus::zvariant::Value::from(on).try_to_owned()
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: e.to_string(),
            dbus_name: None,
        })?;
    hytte_bus::call("net.connman.iwd")
        .bus(BusKind::System)
        .at_path(adapter_path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .method("Set")
        .args(("net.connman.iwd.Adapter", "Powered", value))
        .send::<()>()
        .await
}
```

- [ ] **Step 6.4: Drop dead code**

- The entire `async fn listen(...)` body that opened a Connection and ran the per-iteration loop.
- The OnceLock<Connection> for command-path connection caching.
- `use zbus::Connection;` if unused after.

Keep the data type definitions, the `Agent` interface impl, the parsing helpers (`station_state_from_string`, etc.).

- [ ] **Step 6.5: Verify**

```sh
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --tests -- -D warnings
```

- [ ] **Step 6.6: Commit**

```bash
git add crates/hytte-services/src/wifi.rs
git commit -m "refactor(wifi): migrate to hytte::bus::{signals, call, own_name}

$(cat <<'EOF'
iwd Station discovery via bus::call(GetManagedObjects); state updates
via bus::signals(Station.PropertiesChanged) + ObjectManager
InterfacesAdded/Removed. Commands (scan/connect/disconnect/set_powered)
go through bus::call. Passphrase agent mounted via bus::own_name on
mov.vibec0re.trollshell.iwd-agent.

Drops:
- The 2-second flat-rate listen-loop reconnect (the second-largest
  contributor to the dbus-broker EMFILE)
- OnceLock<Connection> for command-path connection caching

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 4 checkpoint

After Tasks 2-6 land:

- `notifications`, `wifi`, `polkit`, `screensaver`, `power_profiles` all use `hytte::bus::*`. Zero `Connection::session/system` calls in any of them.
- `bluetooth`, `mpris`, `tray`, `networkd`, `upower`, `brightness`, `systemd` still use direct zbus.
- Workspace tests pass; clippy clean.
- **Manual smoke verification:** start trollshell in a real session for 30+ minutes, watch `lsof -p $(pidof trollshell) | grep -c socket` stay flat, watch `journalctl --user -u dbus-broker.service` for any restarts. The original BUGS.md scenario should not reproduce.

If the soak test is clean, the production bug is fixed. Phase 5 cleans up the remaining services for code-quality reasons + Phase 6 enforces the boundary at compile time.

---

## Phase 5 — Remaining service migrations

These services are NOT contributing meaningfully to the dbus-broker FD pressure (some are command-only, some only listen, none have the flat-rate retry storm of Phase 4). They migrate for consistency + to enable Phase 6 cleanup.

---

## Task 7: Migrate `brightness`

Smallest of Phase 5 — just one method call.

**Files:**

- Modify: `crates/hytte-services/src/brightness.rs`

**Public API contract:**

- `pub fn set_brightness(percent: u32)`

**Migration intent:**
The brightness service writes to `/sys/class/backlight/.../brightness` via systemd-logind's `SetBrightness` method on the SYSTEM bus. Single one-shot call per setter invocation.

- [ ] **Step 7.1: Read the current source**

Read `crates/hytte-services/src/brightness.rs`. The file is ~150 lines. The `set_brightness` function opens a Connection::system(), creates a Proxy on `org.freedesktop.login1`, calls `SetBrightness` on the Session interface. There's no listen loop because brightness is command-only.

- [ ] **Step 7.2: Replace the `Connection::system` site with `bus::call`**

The whole pattern becomes:

```rust
async fn do_set_brightness(percent: u32) -> Result<(), hytte_bus::BusError> {
    // SetBrightness on the per-device backlight object. The path is
    // /org/freedesktop/login1/session/<id> for session backlight control,
    // OR /org/freedesktop/login1/seat/seat0 for system-wide. The current
    // code uses the seat path (seat0) — preserve that.
    hytte_bus::call("org.freedesktop.login1")
        .bus(hytte_bus::BusKind::System)
        .at_path("/org/freedesktop/login1/seat/seat0")
        .iface("org.freedesktop.login1.Session")
        .method("SetBrightness")
        .args(("backlight", "intel_backlight", percent))
        .send::<()>()
        .await
}
```

(Verify the exact signature against the current source; the (subsystem, name, value) tuple is the standard logind shape.)

The public `pub fn set_brightness(percent)` becomes:

```rust
pub fn set_brightness(percent: u32) {
    hytte_reactive::runtime::handle().spawn(async move {
        if let Err(e) = do_set_brightness(percent).await {
            tracing::warn!(error = %e, percent, "brightness set failed");
        }
    });
}
```

- [ ] **Step 7.3: Drop dead code**

- Any Connection::system() open
- Any Proxy::new
- `use zbus::Connection;` if unused

- [ ] **Step 7.4: Verify + commit**

```sh
cargo check --workspace && cargo test --workspace && cargo clippy --workspace --tests -- -D warnings
```

```bash
git add crates/hytte-services/src/brightness.rs
git commit -m "refactor(brightness): migrate to hytte::bus::call

$(cat <<'EOF'
SetBrightness on logind goes through bus::call(...).send::<()>().
No listen loop, no Connection management.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Migrate `upower`

**Files:**

- Modify: `crates/hytte-services/src/upower.rs`

**Public API contract:** read the file for the exact shape — typically `state() -> impl Signal<Item = BatteryState>`.

**Migration intent:**
upower exposes battery state on `org.freedesktop.UPower` (system bus). The relevant property is per-device `Percentage`, `State`, `IconName`. After migration: `bus::property::<f64>` for Percentage, `bus::property::<u32>` for State (which is an enum mapped from u32), etc. Coalesce into the existing BatteryState shape via parallel `for_each` tasks (same pattern as power_profiles in Task 2).

- [ ] **Step 8.1: Read the current source**

Read `crates/hytte-services/src/upower.rs`. Note the current device path discovery (typically via `EnumerateDevices` on UPower Manager, then pick the first `BAT*` device). Note the listen loop and the field shape.

- [ ] **Step 8.2: Refactor**

Apply the pattern:

1. Replace device discovery with `bus::call(...).method("EnumerateDevices")` once at startup.
2. For the discovered battery path, mount one `bus::property` per tracked field (Percentage, State, IconName, TimeToEmpty, TimeToFull, etc).
3. Coalesce into `BatteryState` via per-property `for_each` tasks updating the shared `Mutable<BatteryState>`.
4. Drop the listen loop's Connection.

- [ ] **Step 8.3: Verify + commit**

```sh
cargo check --workspace && cargo test --workspace && cargo clippy --workspace --tests -- -D warnings
```

```bash
git add crates/hytte-services/src/upower.rs
git commit -m "refactor(upower): migrate to hytte::bus::{property, call}

$(cat <<'EOF'
Battery state composed from N parallel bus::property subscriptions
(Percentage, State, IconName, etc.) coalesced into BatteryState.
Device discovery via bus::call(EnumerateDevices) once at startup.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Migrate `networkd`

**Files:**

- Modify: `crates/hytte-services/src/networkd.rs`

**Migration intent:**
systemd-networkd exposes link state on `org.freedesktop.network1` (system bus). Per-link properties (OperationalState, AddressState, etc.) are reachable via the Link object. After migration: `bus::property` per tracked field. May need `bus::signals` for InterfacesAdded if links can hot-plug.

- [ ] **Step 9.1: Read source, identify subscriptions and properties**

- [ ] **Step 9.2: Refactor following the upower template**

- [ ] **Step 9.3: Verify + commit**

```bash
git add crates/hytte-services/src/networkd.rs
git commit -m "refactor(networkd): migrate to hytte::bus::{property, signals, call}

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Migrate `systemd`

**Files:**

- Modify: `crates/hytte-services/src/systemd.rs`

**Migration intent:**
The systemd service interacts with the user systemd manager via `org.freedesktop.systemd1` on the SYSTEM bus (or session, depending on what trollshell uses it for). Read the source to determine which calls are needed (likely `ListUnits`, `StartUnit`, `StopUnit`, possibly subscribing to `JobNew`/`JobRemoved` signals).

- [ ] **Step 10.1: Read source, identify D-Bus calls**

- [ ] **Step 10.2: Refactor: each call becomes bus::call, each subscription becomes bus::signals**

- [ ] **Step 10.3: Verify + commit**

```bash
git add crates/hytte-services/src/systemd.rs
git commit -m "refactor(systemd): migrate to hytte::bus::{call, signals}

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Migrate `mpris`

**Files:**

- Modify: `crates/hytte-services/src/mpris.rs`

**Migration intent:**
mpris is a multi-player protocol — each running media player exposes `org.mpris.MediaPlayer2.<name>` on the session bus. The current code watches `NameOwnerChanged` for `org.mpris.MediaPlayer2.*` to discover players, then opens a Proxy per player.

After migration: `bus::signals(...).iface("org.freedesktop.DBus").signal("NameOwnerChanged")` to discover player names; `bus::proxy(...)` per player (the proxy primitive is precisely for this — long-lived handle that survives bus reconnects + observes peer disappearance via PeerGone).

- [ ] **Step 11.1: Read source**

The file is ~600+ lines — a lot of player metadata parsing. The bus-touching parts are isolated.

- [ ] **Step 11.2: Replace per-player Connection with bus::proxy**

For each discovered player:

```rust
let player = hytte_bus::proxy(format!("org.mpris.MediaPlayer2.{name}"))
    .at_path("/org/mpris/MediaPlayer2")
    .iface("org.mpris.MediaPlayer2.Player")
    .build().await?;
```

Subscribe to `player.liveness()` for PeerGone (the player quit) and react by removing the player from the active list.

`player.call(method, args)` for play/pause/next/previous.

- [ ] **Step 11.3: Replace NameOwnerChanged watcher**

```rust
let owner_changes = hytte_bus::signals("org.freedesktop.DBus")
    .at_path("/org/freedesktop/DBus")
    .iface("org.freedesktop.DBus")
    .signal("NameOwnerChanged")
    .start();

// Filter events to org.mpris.MediaPlayer2.* — when new=non-empty, register player; when new=empty, drop player.
```

- [ ] **Step 11.4: Verify + commit**

```bash
git add crates/hytte-services/src/mpris.rs
git commit -m "refactor(mpris): migrate to hytte::bus::{proxy, signals, call}

$(cat <<'EOF'
Per-player handles use bus::proxy — peer-gone detection routes through
ProxyState::PeerGone. NameOwnerChanged subscription via bus::signals
filters for org.mpris.MediaPlayer2.* prefix.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Migrate `tray`

**Files:**

- Modify: `crates/hytte-services/src/tray.rs`

**Migration intent:**
tray implements `org.kde.StatusNotifierWatcher` — owns the well-known name on session bus, accepts items registering via `RegisterStatusNotifierItem`, exposes them as a list. Each item is a separate D-Bus object the tray queries for icon/menu/etc.

- `bus::own_name("org.kde.StatusNotifierWatcher")` for the watcher
- `bus::proxy` per item (long-lived, peer-gone aware)
- `bus::call` for item method invocations (Activate, ContextMenu)

- [ ] **Step 12.1: Read source, identify the per-item proxy management**

- [ ] **Step 12.2: Replace the watcher's Connection with bus::own_name**

- [ ] **Step 12.3: Replace per-item Proxy with bus::proxy**

- [ ] **Step 12.4: Verify + commit**

```bash
git add crates/hytte-services/src/tray.rs
git commit -m "refactor(tray): migrate to hytte::bus::{own_name, proxy, signals, call}

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 13: Migrate `bluetooth`

Largest of Phase 5 — uses all five primitives. ~1300 lines.

**Files:**

- Modify: `crates/hytte-services/src/bluetooth.rs`
- Modify: `crates/hytte-services/src/bluetooth_audio.rs` (if it has its own bus calls — check)

**Migration intent:**
BlueZ exposes adapters and devices on `org.bluez` (system bus). Adapter has `Powered`, `Discovering`, etc.; Device has `Connected`, `Paired`, `Trusted`, `Name`, etc. Pairing requires registering a pairing Agent.

After migration:

- `bus::own_name("mov.vibec0re.trollshell.bluez-agent")` for the pairing agent (anchor name like polkit/wifi)
- `bus::call` for all method invocations (Pair, Connect, Disconnect, Trust, RemoveDevice, StartDiscovery, etc.)
- `bus::signals` for ObjectManager InterfacesAdded/Removed (device hot-plug)
- `bus::property` for tracked Adapter and Device properties (multiple per object — fan out into a HashMap<DevicePath, Mutable<DeviceState>>)
- `bus::proxy` for per-device proxies if needed for repeated calls

- [ ] **Step 13.1: Read source — this is the biggest service**

The file likely has: ObjectManager-driven discovery, adapter selection, per-device state tracking, pairing agent, audio profile management.

- [ ] **Step 13.2-13.N: Migrate piece-by-piece**

Recommended order:

- a. Drop the listen-loop's connection; replace with bus::signals on ObjectManager.
- b. Replace each direct `Connection::system()` with `bus::call`.
- c. Replace per-device Proxy<'static> with `bus::proxy`.
- d. Replace property tracking (Adapter.Powered, Device.Connected, etc.) with `bus::property`.
- e. Replace pairing agent registration with `bus::own_name + at_path`.

- [ ] **Step 13.last: Verify + commit**

```bash
git add crates/hytte-services/src/bluetooth.rs crates/hytte-services/src/bluetooth_audio.rs
git commit -m "refactor(bluetooth): migrate to hytte::bus::{own_name, signals, call, property, proxy}

$(cat <<'EOF'
Adapter/device discovery via bus::signals(ObjectManager); per-device
proxies via bus::proxy with PeerGone for device-disappeared. Pairing
agent mounted on mov.vibec0re.trollshell.bluez-agent. All connect/pair/
disconnect/trust calls go through bus::call. Adapter and Device
property tracking via bus::property.

Largest single migration — touches all five primitives.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 5 checkpoint

After Tasks 7-13: zero `Connection::session()` / `Connection::system()` calls in `crates/hytte-services/src/*.rs`. Workspace tests + clippy still clean.

Verify with:

```sh
grep -nE 'Connection::(session|system)' crates/hytte-services/src/*.rs
# Expected: no output
```

If output is non-empty, that file's migration isn't complete — back-fill before proceeding to Phase 6.

---

## Phase 6 — Cleanup and regression guard

---

## Task 14: Drop direct `zbus` dependency from `hytte-services` + add clippy regression guard

**Files:**

- Modify: `crates/hytte-services/Cargo.toml` (remove `zbus = ...` line)
- Modify: `Cargo.toml` (workspace root — add clippy `disallowed_methods` rule)

- [ ] **Step 14.1: Verify zero direct zbus usage in hytte-services**

```sh
grep -rn 'use zbus' crates/hytte-services/src/
grep -rn 'zbus::' crates/hytte-services/src/
```

Expected: only references via `hytte_bus::` re-exports OR `zbus::zvariant::*` types (which are data-only and acceptable to use directly through the public re-export). The acceptance criterion: NO `Connection::session/system`, NO `Proxy::new`, NO `ObjectServer`, NO interface-impl mounting except via `hytte_bus::own_name(...).at_path(...)`.

If any service still uses zbus types beyond zvariant value-types, EITHER add the missing primitive to `hytte-bus` OR keep that service on direct zbus and exclude it from this cleanup.

- [ ] **Step 14.2: Remove the dep from `hytte-services/Cargo.toml`**

Edit `crates/hytte-services/Cargo.toml`. Find the line:

```toml
zbus = { version = "5.14.0", default-features = false, features = ["tokio"] }
```

Delete it. Save.

- [ ] **Step 14.3: Verify it still compiles**

```sh
cargo check --workspace
```

If it fails, some service is still using a `zbus` symbol that wasn't routed through `hytte_bus`. Fix that service (likely needs a re-export from `hytte-bus`) before continuing.

- [ ] **Step 14.4: Add clippy `disallowed_methods` rule**

Edit `Cargo.toml` (workspace root). Find `[workspace.lints.clippy]` and add a `disallowed_methods` configuration (clippy expects this to live in `clippy.toml` at the workspace root, not in `Cargo.toml`).

Create `clippy.toml` at the workspace root with:

```toml
disallowed-methods = [
    { path = "zbus::Connection::session", reason = "use hytte::bus::* primitives instead — see docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md" },
    { path = "zbus::Connection::system", reason = "use hytte::bus::* primitives instead — see docs/superpowers/specs/2026-04-27-shared-bus-connections-design.md" },
]
```

Then in `Cargo.toml` workspace lints, ensure `clippy::disallowed_methods` is at least `warn`:

```toml
[workspace.lints.clippy]
pedantic = { level = "warn", priority = -1 }
disallowed_methods = "deny"  # add this line
module_name_repetitions = "allow"
missing_errors_doc = "allow"
missing_panics_doc = "allow"
```

- [ ] **Step 14.5: Verify the lint actually fires on a regression**

Temporarily add `let _ = zbus::Connection::session();` somewhere in `hytte-services` (e.g. a new test file). Run `cargo clippy -p hytte-services --tests -- -D warnings`. Expected: clippy errors with the disallowed-methods reason. Remove the test poison.

The hytte-bus crate's `connection.rs` legitimately calls `Connection::session/system` (it's the only place that's allowed to). If clippy errors there too, add an `#[allow(clippy::disallowed_methods)]` on the relevant calls in `connection.rs::open_connection` with a comment "Production-allowed: this IS the centralized site."

- [ ] **Step 14.6: Verify full workspace clean**

```sh
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --tests -- -D warnings
```

- [ ] **Step 14.7: Commit**

```bash
git add crates/hytte-services/Cargo.toml Cargo.toml clippy.toml
# also include any #[allow] adjustments to crates/hytte-bus/src/connection.rs
git commit -m "chore(hytte-services): drop direct zbus dep + clippy regression guard

$(cat <<'EOF'
Phase 6 cleanup. After all 12 services migrated to hytte::bus, the
direct zbus dependency in hytte-services is no longer needed (zbus
remains as a transitive dep through hytte-bus).

Adds clippy disallowed_methods rule that denies
zbus::Connection::session/system at workspace level. The only allowed
call sites are inside hytte-bus's own connection.rs supervisor, which
gets a localized #[allow(clippy::disallowed_methods)] with a comment.

Closes the architectural loop: a future regression that opens a fresh
Connection inside a service will fail clippy with a pointer at the spec.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final smoke test (manual, requires live niri/trollshell session)

After Task 14:

1. Build trollshell release:

   ```sh
   cargo build --release -p trollshell
   ```

2. Replace running trollshell with the new build:

   ```sh
   systemctl --user restart trollshell.service  # if installed via systemd
   # OR kill the running process and start fresh
   ```

3. Soak for 24 hours. Monitor:

   ```sh
   # Stable FD count
   while true; do
       echo "$(date) FDs: $(lsof -p $(pidof trollshell) 2>/dev/null | wc -l)"
       sleep 300
   done

   # dbus-broker uptime should NOT restart
   journalctl --user -u dbus-broker.service --since "24 hours ago" | grep -i 'restart\|exit\|fail'

   # Existing services should still work
   #   - notifications: dunstify "test" → toast appears
   #   - wifi: bar shows current network, scan works
   #   - polkit: a privileged action prompts (e.g. systemctl reboot)
   #   - screensaver: loginctl lock-session triggers lock UI
   #   - power_profiles: bar shows current profile, switch works
   #   - bluetooth, mpris, tray, etc. all functional
   ```

4. The original BUGS.md scenario (`cargo run` for ~10 minutes → dbus-broker dies) should NOT reproduce. If it does, the migration left some FD-leaking path; bisect the Phase 4-5 commits to find which service still leaks.

5. Update `BUGS.md`: either delete the entry (bug is closed) or add a "Closed YYYY-MM-DD by Phase 4-6 migrations of hytte-bus" footnote.

---

## Self-review

- **Spec coverage:** the design spec's Phase 4 (5 services), Phase 5 (7 services), Phase 6 (cleanup) are all represented as tasks. The foundation review's Important #1 is Task 1.
- **Placeholder scan:** No "TBD"/"TODO" in the plan body. The brightness/upower/networkd/systemd/mpris/tray tasks have less inline code than the Phase 4 tasks because their migration shape follows the established pattern from earlier tasks (especially `resolved.rs` and `power_profiles`). Each task explicitly tells the implementer to "read the source" then "apply the pattern" — this is appropriate given the implementer is the same agent that just did Phase 1-3 with 21 commits and has the patterns in working memory.
- **Type consistency:** `hytte::bus::own_name`, `bus::signals`, `bus::call`, `bus::property`, `bus::proxy`, `BusKind::{Session, System}`, `BusError::{Transient, Permanent}` consistent throughout. The interface-mounting kludge (private `_EmitClosed`/`_EmitInvoked` methods on the Notifications iface) is documented and traced from notifications.rs to where it's called from.
- **Scope:** focused on migration only. The `Option A` foundation API addition (a `bus::serve_at` for "mount without owning a name") is explicitly deferred to a separate plan. The current plan uses the per-service anchor-name workaround.

---

## Follow-up plans (out of scope for this plan)

After Phase 4-6 ships, the following items remain open from the foundation review:

1. **Important #2** — `try_lock` race in `with_conn` (robustness only).
2. **Important #3** — vestigial `'a` lifetimes on builders (API ergonomics).
3. **Important #4** — `epoch_signal()` returns `Mutable<u64>` instead of `impl Signal` (encapsulation).
4. **Test gap** — explicit reconnect-during-Get test for `property`.
5. **`bus::serve_at`** — mount an interface without owning a well-known name (needed if any future consumer doesn't want the anchor-name workaround that polkit/wifi/bluetooth use).
6. **Property cache for whole iface** (`bus::properties`) — currently each tracked field is a separate subscription; consolidating would reduce PropertiesChanged duplication.

These are independent and can be sequenced as the codebase grows.
