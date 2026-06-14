# trollshell v0.2.1 — polish & reactive-hygiene pass

**Status:** design
**Date:** 2026-04-25
**Author:** Claude (with annika)
**Predecessors:** `2026-04-24-hytte-trollshell-design.md` (v0.1 architecture), v0.2.0 plan.

## Goal

Close the tracked correctness gaps in `trollshell` and lift the reactive-binding pattern that three call sites have hand-rolled into a reusable primitive in `hytte-reactive`. No new features.

## Scope

### In scope

1. **Library:** add `bind_two_way` to `hytte-reactive` and migrate the three hand-rolled feedback-suppression sites to it.
2. **Bug fix:** route BlueZ adapter/device commands through a long-lived shared `zbus::Connection` so `StartDiscovery`/`StopDiscovery` are paired by BlueZ. Closes `BUGS.md`.
3. **TODO sweep:**
   - Toast overflow ("+N more") in `widgets/notifications.rs`
   - Calendar click-day → scroll/highlight matching list rows in `widgets/pages.rs`
   - Second `PAM_PROMPT_ECHO_OFF` (confirm-new-password) in `services/polkit.rs` + `widgets/polkit_dialog.rs`
   - Cliphist id-based delete in `services/clipboard.rs` + clipboard drawer page
   - Case-insensitive MAC matching in `services/bluetooth_audio.rs`

### Out of scope

- `services/niri.rs:177` (`WorkspaceUrgencyChanged`, `KeyboardLayoutSwitched`) — explicitly deferred to v0.3 per existing TODO.
- The Displays drawer Switch (`widgets/pages.rs:2235`). It uses an optimistic `pending_state` apply pipeline with rollback that is structurally different from the other Switches; migrating it would require an `bind_optimistic` primitive, which we are not designing now.
- New features, new pages, visual/UX walkthrough, screenshot tests.

## Success criteria

- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green.
- BT scan toggle: clicking Stop reliably ends discovery (verified via `bluetoothctl --monitor`).
- Brightness slider does not echo writes (no observable jitter when service emits new readings during user drag).
- mpris seek does not echo writes (no observable jitter during user drag).
- Toast queue caps at 4 individual + 1 overflow toast; overflow click opens Notifications page.
- Calendar day-click scrolls the events list to the first event of that date and visually highlights it for ~1.5s.
- Polkit confirm-new-password flow: dialog accepts a second prompt without closing prematurely.
- Clipboard rows expose a Delete entry in a ⋮ MenuButton popover that removes the entry via `cliphist delete` (id-keyed, stdin-driven).
- BT audio auto-switch matches devices regardless of MAC case in either pipewire node names or BlueZ paths.

## §1 — `hytte-reactive::bind_two_way`

### Why

`bind()` is one-way (signal → widget property). When the widget is also user-writable (Switch, ToggleButton, Scale), every signal-driven `apply` of mirrored state can re-fire the widget's user-event handler, which calls a service writer, which can echo back.

Three sites in `trollshell` hand-roll a `Cell<bool> suppress` flag to gate the user handler:

- `widgets/pages.rs:1601` — brightness slider
- `widgets/pages.rs:162` — mpris seek bar
- (implicitly) the BT scan toggle does NOT, which is one of two factors behind `BUGS.md`. The other factor is the connection-identity bug fixed in §2.

Six other Switch sites use a defensive `if w.is_active() != on { w.set_active(on) }` guard that is benign (GTK no-ops `set_active` on equal values) but redundant.

### Mechanism

GTK signal-handler block via `glib::SignalHandlerId`. The standard fix; avoids hand-rolled re-entrancy flags.

### Public API

Added in `crates/hytte-reactive/src/bind.rs`. Re-exported from the crate's `lib.rs` and from `hytte::prelude::*` alongside `bind`/`bind_text`/`bind_visible`/`bind_class`.

```rust
pub fn bind_two_way<S, W, V, Apply, Connect>(
    signal: S,
    widget: &W,
    apply: Apply,
    connect_user: Connect,
) where
    S: Signal<Item = V> + 'static,
    V: 'static,
    W: IsA<gtk::Widget> + Clone + 'static,
    Apply: Fn(&W, V) + 'static,
    Connect: FnOnce(&W) -> glib::SignalHandlerId,
```

### Semantics

1. `connect_user` is invoked once at bind time. It wires the user-event handler to the widget and returns its `SignalHandlerId`.
2. The bind future runs on `glib::MainContext::default()` like `bind()`. For each emission of `signal`:
   - `widget.block_signal(&handler_id)`
   - `apply(&widget, value)`
   - `widget.unblock_signal(&handler_id)`
3. The handler stays registered across emissions; only the _delivery_ of `notify::active` / `value-changed` / `toggled` is suppressed during apply, so genuine user actions still fire.
4. Lifetime is tied to the widget (cheap clone, GTK refcount) — same model as `bind()`.

### Design choices not taken

- **No `bind_optimistic` / ack-window logic.** All three real call sites work fine with plain handler-blocking once §2 lands. Adding optimistic suppression now would over-design for zero current consumers. File a v0.3 follow-up if a future widget needs it.
- **No automatic `if would-set != current` short-circuit.** Removed from migrated sites; structurally unnecessary once handler blocking is correct.
- **No typed convenience wrappers** (`bind_two_way_active`, `bind_two_way_value`, etc.). One generic primitive is sufficient. Callers explicitly pick the connect function they need.

### Tests

In `crates/hytte-reactive/src/bind.rs` (or a sibling test file). Tests use `glib::test_init` to set up a main context.

- A signal emission applies the value, and the user-event handler is **not** re-fired during that apply.
- A genuine user action **does** fire the user handler (handler is not permanently blocked).
- Dropping the widget stops the bind future cleanly (no panics, no leaked tasks).

## §2 — BlueZ command connection identity

### Root cause

`crates/hytte-services/src/bluetooth.rs` opens a fresh `Connection::system()` inside every adapter/device call helper:

- `do_adapter_call` (line 552)
- `do_device_call` (line 568)
- `do_set_adapter_bool` (line ~513)
- `do_set_device_bool` (line ~533)
- agent registration (line 1204)

BlueZ owns discovery sessions per bus client. `StartDiscovery` from a transient connection-A starts a session owned by A; A drops; session is dropped (or remains briefly until BlueZ notices client-disconnect). `StopDiscovery` from a fresh connection-B returns `org.bluez.Error.Failed: No discovery started`, which the current code logs at `warn` and discards. Net symptom: clicking Stop does not stop scanning.

### Fix

Hold a single shared `zbus::Connection` for the lifetime of the service and use it for _every_ command (not only discovery — same identity issue affects Connect/Disconnect/Pair/Trust under contention).

### Implementation outline

1. Add a module-level `tokio::sync::OnceCell<Connection>` (e.g. `static CMD_CONN: OnceCell<Connection> = ...`) initialized lazily on first command call.
2. Refactor signatures: `do_adapter_call(conn: &Connection, path, method)`, `do_device_call(conn, path, method)`, `do_set_adapter_bool(conn, path, prop, on)`, `do_set_device_bool(conn, path, prop, on)`. Each public command (`start_discovery`, `stop_discovery`, `set_powered`, `set_discoverable`, `connect_device`, `disconnect_device`, `pair_device`, `set_trusted`, `remove_device`, …) fetches the shared conn first.
3. Agent registration also uses the shared conn.
4. The listen loop's existing connection (line 759) stays separate — it has signal subscriptions and can fail/reconnect independently. Command identity does not need to match the listen loop.
5. Reconnect handling: naive for v0.2.1. If the command conn ever errors with `zbus::Error::ConnectionClosed` or equivalent, log and let the next call re-init via OnceCell-replacement helper, OR simply panic and let the supervised service restart. v0.3 follow-up: proper auto-reconnect of the command channel.

### Verification

Manual:

```sh
bluetoothctl --monitor
# in another terminal: open BT drawer, click Scan, then click Stop.
# Expected: `Discovering = yes` then `Discovering = no` round-trip on the
# Stop click. No `org.bluez.Error.Failed` for `StopDiscovery`.
```

Automated tests against a real BlueZ are out of scope. A small zbus mock could be added later but is not required for this fix.

### Risk

A dead command connection blocks every command until reconnect or restart. The existing service is already supervised; mitigation deferred to v0.3.

## §3 — Tracked TODO sweep

Five independent units. PR order is flexible; nothing here blocks anything else, and §1 / §2 don't block these either.

### 3a — Toast overflow

**File:** `trollshell/src/widgets/notifications.rs`

Cap the visible-toast `Vec` at 4. When a 5th active toast would appear, drop the oldest visible toast and ensure a single synthetic "+N more" overflow toast is present (creating it if needed, incrementing N if not). Click on the overflow toast opens the Notifications drawer page; dismissal of the overflow clears all currently-tracked overflowed toasts. Counts decrement when an underlying notification is dismissed by other means; if N drops to 0, remove the overflow toast.

The full history continues to live in the Notifications drawer page — toasts remain a transient overflow indicator only.

No service-side changes.

### 3b — Calendar click-day highlight

**File:** `trollshell/src/widgets/pages.rs` (`page_calendar`, around line 2553)

Track event rows by date in an `Rc<RefCell<Vec<(NaiveDate, gtk::Widget)>>>` populated as the events list is built. Connect `gtk::Calendar::day-selected`. On select:

1. Compute the `NaiveDate` from the calendar's selected (year, month, day).
2. Find the first row whose date matches.
3. Scroll the events `ScrolledWindow` so the row's vertical position is visible (use `gtk::Adjustment` on the scrolled window's `vadjustment`, or the row's `compute_point`/`gtk_widget_translate_coordinates` if cleaner).
4. Add CSS class `ts-cal-day-hit` to the row; schedule a `glib::timeout_add_local_once` for ~1.5s that removes the class.

CSS class added in `etc/.../style.css` only if a token already governs accent/highlight color. Do not introduce new color tokens (per project memory).

### 3c — Polkit confirm-new-password

**Files:** `crates/hytte-services/src/polkit.rs` (around line 360), `trollshell/src/widgets/polkit_dialog.rs`

Currently the PAM conversation loop handles one `PAM_PROMPT_ECHO_OFF`, dispatches the response, and returns. PAM flows like password-change can issue a second `PAM_PROMPT_ECHO_OFF` ("Retype new password"). The dialog tears down before the second prompt arrives.

Change:

- In the polkit service, the conversation handler loops until PAM completes (success or final failure), forwarding each prompt to the dialog and awaiting a response.
- The dialog widget gains a state to receive a follow-up prompt while still mounted: clear the entry, swap the prompt label text to the new prompt string, await user input, send back. Auth completes (success or failure) closes the dialog.
- Existing single-prompt behavior is preserved as the common case.

Both prompts run through the same UI affordance; no new dialog layout.

### 3d — Cliphist id-based delete

**Files:** `crates/hytte-services/src/clipboard.rs` (line 28 TODO), `trollshell/src/widgets/pages.rs` (`page_clipboard`)

Add a service command `delete(id: &str)` (the id from `cliphist list`'s `\t`-separated lines) that shells out:

```sh
echo '{id}\t...' | cliphist delete
```

`cliphist delete` reads stdin, matching the line format used by `cliphist list`. Refresh the in-memory list afterwards (re-run `cliphist list` or remove the entry locally and re-emit). Errors are logged at `warn`, not surfaced as toasts (consistent with other clipboard error paths).

UI: each clipboard row gets a ⋮ `gtk::MenuButton` suffix with a popover containing a single "Delete entry" destructive action (per project memory: destructive actions go in a popover, not the row click target). Activating it calls `clipboard::delete(id)`.

### 3e — BT-audio MAC casing

**File:** `crates/hytte-services/src/bluetooth_audio.rs` (around line 218)

The name-pattern bookkeeping that maps a pipewire node name to a BlueZ device path is currently case-sensitive. BlueZ paths use uppercase MACs; pipewire node names sometimes use lowercase. Mismatch breaks auto-switch on those devices.

Change: normalize both sides to uppercase (or lowercase — choose one and apply consistently) before any substring/equality comparison. Add a unit test covering the previously broken mismatched-case scenario.

## Open follow-ups (v0.3 candidates, not part of this milestone)

- `bind_optimistic` primitive in `hytte-reactive` for widgets that need ack-window semantics (e.g. the Displays Switch).
- Auto-reconnect of the BlueZ command connection on `ConnectionClosed`.
- `services/niri.rs` `WorkspaceUrgencyChanged` / `KeyboardLayoutSwitched` (already tracked in source).

## Implementation hand-off

After approval, the writing-plans skill produces a step-by-step implementation plan that this spec backs. Each of `§1`, `§2`, and the five `§3` units can be a separate plan step or PR; they do not depend on one another.
