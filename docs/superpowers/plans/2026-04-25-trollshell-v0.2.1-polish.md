# trollshell v0.2.1 polish & reactive-hygiene Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close `BUGS.md` (BT scan-stop), eliminate hand-rolled feedback-suppression flags by introducing a `bind_two_way` primitive in `hytte-reactive`, and knock down five tracked TODOs in `hytte-services` and `trollshell`.

**Architecture:** One reusable signal-handler-blocking primitive in the library. The BT scan bug is a separate fix (BlueZ owns discovery sessions per bus client; we currently open a fresh `zbus::Connection` per command call). Each remaining TODO is an isolated change in its own file. Tasks 1–10 are independent except where noted.

**Tech Stack:** Rust 1.94 stable, GTK4 + libadwaita via `gtk4-rs`, `futures-signals`, `zbus`, `tokio`. Workspace uses `cargo` with the project clippy lint set enforced workspace-wide.

**Conventions used in every task:**
- TDD where unit tests are practical. Refactor-only tasks (migrations) verify via `cargo build` + `cargo clippy --workspace --all-targets -- -D warnings` and call out manual checks.
- Commits use Conventional Commits (`feat:`, `fix:`, `refactor:`, `docs:`). The repo's existing prefixes are `feat(de):`, `fix(de):`, `polish:`, `style:`. Match those.
- Co-author trailer on every commit:
  `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`

**Spec backing this plan:** `docs/superpowers/specs/2026-04-25-trollshell-v0.2.1-polish-design.md`

---

## File Structure

**Modified files (no new files):**

- `crates/hytte-reactive/src/bind.rs` — add `bind_two_way` and unit tests.
- `crates/hytte-reactive/src/lib.rs` — re-export `bind_two_way`.
- `crates/hytte/src/lib.rs` — add `bind_two_way` to `prelude`.
- `crates/hytte-services/src/bluetooth.rs` — shared command connection, refactor `do_*` helpers.
- `crates/hytte-services/src/bluetooth_audio.rs` — case-insensitive MAC matching.
- `crates/hytte-services/src/clipboard.rs` — id-based delete API.
- `crates/hytte-services/src/polkit.rs` — multi-prompt PAM conversation loop.
- `trollshell/src/widgets/pages.rs` — Switch / Scale migrations, calendar click-day, clipboard ⋮ popover.
- `trollshell/src/widgets/notifications.rs` — toast overflow "+N more".
- `trollshell/src/widgets/polkit_dialog.rs` — follow-up prompt UI.
- `BUGS.md` — remove the BT entry once Task 5 is verified.

---

## Task 1: `bind_two_way` primitive + tests in `hytte-reactive`

**Files:**
- Modify: `crates/hytte-reactive/src/bind.rs`
- Modify: `crates/hytte-reactive/src/lib.rs`
- Modify: `crates/hytte/src/lib.rs`

**Background:** `bind` (existing helper) is one-way: a signal drives a widget property. When the widget is *also* writable by the user (Switch active, Scale value, ToggleButton active), the signal-driven `apply` can re-fire the user's `notify::active` / `value-changed` / `toggled` handler, which then writes the same value back to the service, which echoes back. Three sites in `trollshell` already invented `Rc<Cell<bool>> suppress` to break that loop. This task lifts the pattern into one reusable primitive that uses GTK signal-handler-block, the canonical fix.

**API target (already approved in spec §1):**

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

- [ ] **Step 1: Read the existing `bind` implementation for shape**

Read `crates/hytte-reactive/src/bind.rs` end-to-end so the new function matches: same `glib::MainContext::default().spawn_local`, same widget-clone-lifetime model, same docstring style.

- [ ] **Step 2: Write the failing tests**

Append to `crates/hytte-reactive/src/bind.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use futures_signals::signal::Mutable;
    use gtk::glib;
    use gtk::prelude::*;
    use std::rc::Rc;
    use std::cell::Cell;

    fn ensure_gtk_init() {
        // gtk::test_init() panics if called twice; gate with a once.
        static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| {
            gtk::init().expect("gtk init for tests");
        });
    }

    /// A signal emission applies the value, and the user-event handler is
    /// NOT re-fired while the apply runs.
    #[test]
    fn signal_apply_does_not_refire_user_handler() {
        ensure_gtk_init();
        let ctx = glib::MainContext::default();
        let _guard = ctx.acquire().expect("acquire gtk main ctx");

        let switch = gtk::Switch::new();
        let state = Mutable::new(false);
        let user_calls = Rc::new(Cell::new(0u32));

        let user_calls_for_handler = user_calls.clone();
        bind_two_way(
            state.signal(),
            &switch,
            |w, on| w.set_active(on),
            move |w| {
                let counter = user_calls_for_handler.clone();
                w.connect_active_notify(move |_| counter.set(counter.get() + 1))
            },
        );

        // Pump until the initial Mutable emission applies.
        while ctx.iteration(false) {}

        // Drive a state change. apply() will call set_active(true), which
        // would normally fire active-notify. The handler must stay blocked.
        state.set(true);
        while ctx.iteration(false) {}

        assert_eq!(user_calls.get(), 0,
            "user handler must not fire during signal-driven apply");
        assert!(switch.is_active(), "apply did set the property");
    }

    /// A genuine user action still fires the user handler — the block is
    /// released between applies.
    #[test]
    fn user_event_still_fires_after_apply() {
        ensure_gtk_init();
        let ctx = glib::MainContext::default();
        let _guard = ctx.acquire().expect("acquire gtk main ctx");

        let switch = gtk::Switch::new();
        let state = Mutable::new(false);
        let user_calls = Rc::new(Cell::new(0u32));

        let user_calls_for_handler = user_calls.clone();
        bind_two_way(
            state.signal(),
            &switch,
            |w, on| w.set_active(on),
            move |w| {
                let counter = user_calls_for_handler.clone();
                w.connect_active_notify(move |_| counter.set(counter.get() + 1))
            },
        );

        while ctx.iteration(false) {}

        // Simulate a user-driven flip by toggling active directly. Because
        // the signal hasn't emitted, the handler must NOT be blocked.
        switch.set_active(true);
        while ctx.iteration(false) {}

        assert_eq!(user_calls.get(), 1,
            "user-driven set_active must fire the user handler exactly once");
    }
}
```

- [ ] **Step 3: Run tests and verify they fail to compile**

Run: `cargo test -p hytte-reactive --lib bind::tests -- --nocapture`
Expected: compile error — `bind_two_way` not defined.

- [ ] **Step 4: Implement `bind_two_way`**

Add to `crates/hytte-reactive/src/bind.rs` directly above the `#[cfg(test)] mod tests` block:

```rust
/// Two-way bind: signal drives a writable widget property while the user
/// can still drive that property themselves. The user-event handler is
/// blocked across each signal-driven `apply`, so programmatic state
/// mirroring never re-enters the user handler.
///
/// `connect_user` is invoked once at bind time. It must wire a user-event
/// handler (e.g. `connect_active_notify`, `connect_value_changed`,
/// `connect_toggled`) and return its [`glib::SignalHandlerId`]. The bind
/// future blocks that handler around every `apply` call and unblocks it
/// after.
///
/// Lifetime is tied to the widget the same way `bind` is: a cheap clone
/// keeps the future alive for as long as the widget is referenced.
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
{
    let widget = widget.clone();
    let handler_id = connect_user(&widget);
    glib::MainContext::default().spawn_local(async move {
        signal
            .for_each(move |value| {
                widget.block_signal(&handler_id);
                apply(&widget, value);
                widget.unblock_signal(&handler_id);
                std::future::ready(())
            })
            .await;
    });
}
```

`block_signal` / `unblock_signal` are on `glib::ObjectExt`, brought in by `use gtk::prelude::*` already at the top of the file.

- [ ] **Step 5: Re-export from the crate root**

Edit `crates/hytte-reactive/src/lib.rs` line 8 from:

```rust
pub use bind::{bind, bind_class, bind_text, bind_visible};
```

to:

```rust
pub use bind::{bind, bind_class, bind_text, bind_two_way, bind_visible};
```

- [ ] **Step 6: Re-export from `hytte::prelude`**

Edit `crates/hytte/src/lib.rs` line 21 from:

```rust
pub use hytte_reactive::{bind, bind_class, bind_text, bind_visible, Service};
```

to:

```rust
pub use hytte_reactive::{bind, bind_class, bind_text, bind_two_way, bind_visible, Service};
```

- [ ] **Step 7: Run tests and verify they pass**

Run: `cargo test -p hytte-reactive --lib bind::tests -- --nocapture`
Expected: 2 passed.

- [ ] **Step 8: Run clippy on the workspace**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. If clippy flags `needless_pass_by_value` or similar on the new generic, fix inline.

- [ ] **Step 9: Commit**

```bash
git add crates/hytte-reactive/src/bind.rs crates/hytte-reactive/src/lib.rs crates/hytte/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(reactive): bind_two_way primitive

Wraps bind() with automatic GTK signal-handler-block around each
signal-driven apply, so user-event handlers can't re-enter from the
mirroring path. Replaces three hand-rolled Cell<bool> suppression
flags in trollshell (next commits).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Migrate the 6 Switch sites to `bind_two_way`

**Files:**
- Modify: `trollshell/src/widgets/pages.rs` (sites at approx lines 625, 675, 699, 1684, 1828, 2395)

**Background:** Six Switch widgets currently use a defensive `if w.is_active() != on { w.set_active(on) }` guard inside their bind. The guard is benign (GTK's `set_active` no-ops on equal values) but redundant once handler-blocking is structural. Migrating means: drop the guard, restructure the two calls (separate `bind` for state + `connect_active_notify` for user) into one `bind_two_way` call.

This task is a pure refactor with no behavior change for these widgets — they were never broken, just verbose. The behavior change for the BT scan toggle ships in Task 5 (paired with the bug fix in Task 4).

**Sites to migrate:**

1. BT power switch (~line 625) — `bluetooth::set_powered(active)`
2. BT discoverable switch (~line 675) — `bluetooth::set_discoverable(active)`
3. BT auto-switch audio (~line 699) — `bluetooth_audio::set_auto_switch_enabled(active)`
4. DND switch on notifications page (~line 1684) — `dnd::set_enabled(active)`
5. Per-app mute switch (~line 1828) — `notifications_mute::set_app_muted(&app, active)`
6. DND switch on settings page (~line 2395) — `dnd::set_enabled(active)`

The Displays drawer Switch at ~line 2235 is **out of scope** (uses an optimistic `pending_state` apply pipeline; spec §Out-of-scope). Leave it.

- [ ] **Step 1: Migrate BT power switch (~line 625)**

Replace:

```rust
let power_switch = gtk::Switch::new();
power_switch.set_valign(gtk::Align::Center);
bind(
    bluetooth::adapter().map(|a| a.is_some()),
    &power_switch,
    gtk::prelude::WidgetExt::set_sensitive,
);
bind(
    bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
    &power_switch,
    |w, on| {
        if w.is_active() != on {
            w.set_active(on);
        }
    },
);
power_switch.connect_active_notify(|sw| {
    bluetooth::set_powered(sw.is_active());
});
```

with:

```rust
let power_switch = gtk::Switch::new();
power_switch.set_valign(gtk::Align::Center);
bind(
    bluetooth::adapter().map(|a| a.is_some()),
    &power_switch,
    gtk::prelude::WidgetExt::set_sensitive,
);
bind_two_way(
    bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
    &power_switch,
    |w, on| w.set_active(on),
    |w| w.connect_active_notify(|sw| bluetooth::set_powered(sw.is_active())),
);
```

- [ ] **Step 2: Migrate BT discoverable switch (~line 675)**

Replace:

```rust
let disc_switch = gtk::Switch::new();
disc_switch.set_valign(gtk::Align::Center);
bind(
    bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discoverable)),
    &disc_switch,
    |w, on| {
        if w.is_active() != on {
            w.set_active(on);
        }
    },
);
disc_switch.connect_active_notify(|sw| {
    bluetooth::set_discoverable(sw.is_active());
});
```

with:

```rust
let disc_switch = gtk::Switch::new();
disc_switch.set_valign(gtk::Align::Center);
bind_two_way(
    bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discoverable)),
    &disc_switch,
    |w, on| w.set_active(on),
    |w| w.connect_active_notify(|sw| bluetooth::set_discoverable(sw.is_active())),
);
```

- [ ] **Step 3: Migrate BT auto-switch audio (~line 699)**

Replace:

```rust
let auto_switch = gtk::Switch::new();
auto_switch.set_valign(gtk::Align::Center);
bind(
    bluetooth_audio::auto_switch_enabled(),
    &auto_switch,
    |w, on| {
        if w.is_active() != on {
            w.set_active(on);
        }
    },
);
auto_switch.connect_active_notify(|sw| {
    bluetooth_audio::set_auto_switch_enabled(sw.is_active());
});
```

with:

```rust
let auto_switch = gtk::Switch::new();
auto_switch.set_valign(gtk::Align::Center);
bind_two_way(
    bluetooth_audio::auto_switch_enabled(),
    &auto_switch,
    |w, on| w.set_active(on),
    |w| w.connect_active_notify(|sw| bluetooth_audio::set_auto_switch_enabled(sw.is_active())),
);
```

- [ ] **Step 4: Migrate DND switch on notifications page (~line 1684)**

Find the existing block:

```rust
let dnd_switch = gtk::Switch::new();
// ... bind(...) with `if w.is_active() != on { w.set_active(on) }`
// ... dnd_switch.connect_active_notify(|sw| { dnd::set_enabled(sw.is_active()); });
```

Replace the bind+connect_active_notify pair with one `bind_two_way`:

```rust
let dnd_switch = gtk::Switch::new();
dnd_switch.set_valign(gtk::Align::Center);
bind_two_way(
    dnd::enabled(),
    &dnd_switch,
    |w, on| w.set_active(on),
    |w| w.connect_active_notify(|sw| dnd::set_enabled(sw.is_active())),
);
```

Preserve any sibling code in the block (subtitle binds, css class, row construction) — only the bind+connect pair changes.

- [ ] **Step 5: Migrate per-app mute switch (~line 1828)**

The per-app mute switch is built inside `build_app_mute_row` (or the inline equivalent in `pages.rs:~1828`). The existing code captures `app: &str` by clone for both the connect handler and the bind. Replace:

```rust
let mute_switch = gtk::Switch::new();
mute_switch.set_valign(gtk::Align::Center);
mute_switch.set_tooltip_text(Some("Mute toasts from this app"));
mute_switch.set_active(muted.contains(app));
let app_owned = app.to_string();
mute_switch.connect_active_notify(move |sw| {
    notifications_mute::set_app_muted(&app_owned, sw.is_active());
});
let app_for_bind = app.to_string();
bind(
    notifications_mute::muted_apps().map(move |m| m.contains(&app_for_bind)),
    &mute_switch,
    |w, on| {
        if w.is_active() != on {
            w.set_active(on);
        }
    },
);
```

with:

```rust
let mute_switch = gtk::Switch::new();
mute_switch.set_valign(gtk::Align::Center);
mute_switch.set_tooltip_text(Some("Mute toasts from this app"));
mute_switch.set_active(muted.contains(app));
let app_for_bind = app.to_string();
let app_for_handler = app.to_string();
bind_two_way(
    notifications_mute::muted_apps().map(move |m| m.contains(&app_for_bind)),
    &mute_switch,
    |w, on| w.set_active(on),
    move |w| {
        w.connect_active_notify(move |sw| {
            notifications_mute::set_app_muted(&app_for_handler, sw.is_active());
        })
    },
);
```

The doc comment on the original site mentions "The `is_active() != on` guard breaks the bind→active-notify→set_app_muted→bind feedback loop." — replace that comment with: `// bind_two_way blocks the user handler around the apply, so no feedback loop.`

- [ ] **Step 6: Migrate DND switch on settings page (~line 2395)**

Same shape as Step 4. Replace the bind+connect_active_notify pair with one `bind_two_way` against `dnd::enabled()` and `dnd::set_enabled`.

- [ ] **Step 7: Build + clippy**

Run: `cargo build -p trollshell`
Expected: clean build.

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Manual smoke test**

In a Niri session: `cargo run --release -p trollshell`. Open the Bluetooth drawer; toggle Power on/off; toggle Discoverable; toggle Auto-switch audio. Open the Notifications drawer; toggle DND. Open the Settings drawer; toggle DND — observe both DND switches sync (they share `dnd::enabled()`). Mute and unmute an app from the notifications group expander. Each toggle should round-trip without flicker.

- [ ] **Step 9: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
refactor(de): migrate Switch sites to bind_two_way

Six Switch sites (BT power, BT discoverable, BT auto-switch audio,
DND on notifications page, DND on settings page, per-app mute) drop
their defensive `is_active() != on` guards. Handler blocking is now
structural through bind_two_way.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Migrate the 2 Scale sites to `bind_two_way`

**Files:**
- Modify: `trollshell/src/widgets/pages.rs` (sites at approx lines 196 / 232 mpris seek bar, ~1601 brightness slider)

**Background:** Brightness slider and mpris seek bar each maintain a hand-rolled `Rc<Cell<bool>> suppress` flag to gate `connect_value_changed` while the bind reflects external state. With `bind_two_way`, the Cell goes away and the bind becomes a single call.

The mpris seek is **embedded inside a larger bind closure** (the bind on `mpris::active_player()` updates title, artist, art, and seek). The seek migration extracts only the seek↔fraction part into its own `bind_two_way` call; the rest of the player state stays on plain `bind`. Two binds against the same signal is fine — `futures-signals` allows multiple subscribers.

- [ ] **Step 1: Migrate brightness slider (~line 1594)**

Locate the slider construction (between `gtk::Scale::with_range(...)` and the trailing icon append). Replace:

```rust
let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.05, 1.0, 0.05);
slider.set_draw_value(false);
slider.set_hexpand(true);

// Avoid feedback: when bind() reflects external state into the slider,
// suppress the connect_value_changed → brightness::set() path. Without
// this the slider would echo every poll back into a brightness write.
let suppress = Rc::new(Cell::new(false));
let suppress_for_handler = suppress.clone();
slider.connect_value_changed(move |s| {
    if suppress_for_handler.get() {
        return;
    }
    brightness::set(s.value());
});
let suppress_for_bind = suppress.clone();
bind(brightness::current(), &slider, move |s, b| {
    if let Some(b) = b {
        suppress_for_bind.set(true);
        s.set_value(b.level);
        suppress_for_bind.set(false);
    }
});
```

with:

```rust
let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.05, 1.0, 0.05);
slider.set_draw_value(false);
slider.set_hexpand(true);

bind_two_way(
    brightness::current(),
    &slider,
    |s, b| {
        if let Some(b) = b {
            s.set_value(b.level);
        }
    },
    |s| s.connect_value_changed(|s| brightness::set(s.value())),
);
```

If the surrounding scope still has unused `use std::cell::Cell;` or `use std::rc::Rc;` imports, leave them — other call sites in the same file rely on them. Don't run an import sweep here.

- [ ] **Step 2: Migrate mpris seek bar — extract from the active_player bind**

The current code:
- Around line 162, declares `seek_suppress: Rc<Cell<bool>> = Rc::new(Cell::new(false))`.
- Around line 196, `seek.connect_value_changed(move |s| { if suppress.get() { return; } /* bus/track_id checks; mpris::set_position(...) */ });`
- Inside the big bind on `mpris::active_player()` (line 232), each `seek_for_bind.set_value(...)` is wrapped:

```rust
seek_suppress.set(true);
seek_for_bind.set_value(0.0);  // or frac.clamp(0.0, 1.0)
seek_suppress.set(false);
```

Migration steps:

1. Delete the `seek_suppress` declaration (line 162).
2. Delete the `connect_value_changed` block at line 196 entirely — it moves into a `bind_two_way` call below.
3. Inside the big bind on `active_player`, drop every `seek_suppress.set(true); ... seek_suppress.set(false);` triplet, leaving only the `seek_for_bind.set_value(...)` lines exposed. **Important:** the big bind subscription is now **plain `bind`** updating the *seek value* programmatically — but `bind_two_way` blocks the user handler around its own apply, NOT around third-party programmatic writes. Solution: move the seek-value updates OUT of the big bind and let the dedicated `bind_two_way` (added below) own the seek-value mirror.

After step 3, the big bind on `active_player()` no longer touches `seek_for_bind.set_value(...)`. The big bind keeps title/artist/album/buttons/labels/art-image only.

4. Add a new `bind_two_way` immediately after the existing big bind on `active_player`:

```rust
// Seek value mirror + user-driven SetPosition. Subscribes to active_player
// independently of the title/art bind above; futures-signals allows
// multiple subscribers and bind_two_way owns the user-handler block.
let bus_for_seek = current_bus.clone();
let tid_for_seek = current_track_id.clone();
let len_for_seek = current_length.clone();
bind_two_way(
    mpris::active_player().map(|maybe| {
        let Some(p) = maybe else { return 0.0; };
        if p.length_us == 0 { 0.0 } else {
            #[allow(clippy::cast_precision_loss)]
            ((p.position_us as f64) / (p.length_us as f64)).clamp(0.0, 1.0)
        }
    }),
    &seek,
    |s, frac| s.set_value(frac),
    move |s| s.connect_value_changed(move |s| {
        let bus_opt = bus_for_seek.borrow();
        let tid_opt = tid_for_seek.borrow();
        let (Some(b), Some(t)) = (bus_opt.as_ref(), tid_opt.as_ref()) else {
            return;
        };
        let pos_fraction = s.value().clamp(0.0, 1.0);
        let length = len_for_seek.get();
        if length == 0 { return; }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let pos_us = (pos_fraction * length as f64) as i64;
        mpris::set_position(b, t, pos_us);
    }),
);
```

The `current_bus` / `current_track_id` / `current_length` cells are still maintained by the big bind closure (which writes them inside the `Some(player) => { ... }` arm).

5. Verify all references to `seek_suppress` are gone:

Run: `grep -n seek_suppress trollshell/src/widgets/pages.rs`
Expected: no output.

- [ ] **Step 3: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Manual smoke test**

In Niri: `cargo run --release -p trollshell`.

- Brightness drawer: drag the slider; observe brightness applies. Wait through one or two backend polls (`hytte-services::brightness` polls actual backlight) — slider should not jitter back, even when the polled level rounds slightly differently.
- Media drawer with a player active (e.g. `mpv some.mp3`): drag the seek bar — track should jump. Let it play and watch the seek bar advance smoothly with no jitter while *not* being dragged.

- [ ] **Step 5: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
refactor(de): migrate brightness & mpris-seek to bind_two_way

Drops two hand-rolled Rc<Cell<bool>> suppression flags. The seek bar
splits out of the active_player bind into its own bind_two_way; title,
artist, art, and buttons stay on the original bind.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: BlueZ shared command connection (the `BUGS.md` fix, part 1)

**Files:**
- Modify: `crates/hytte-services/src/bluetooth.rs`

**Background:** Per BlueZ semantics, a discovery session is owned by the bus client that called `StartDiscovery`. The current `do_adapter_call` / `do_device_call` / `do_set_*_bool` helpers each open a fresh `Connection::system().await` and drop it after the call. `StartDiscovery` from connection-A → conn-A drops → BlueZ tears down its session. `StopDiscovery` from connection-B → BlueZ returns `org.bluez.Error.Failed: No discovery started` (silently logged at warn). Same identity problem affects Connect/Disconnect/Pair/Trust under contention.

Fix: hold a single shared `zbus::Connection` for command lifetime; route every `do_*` helper through it.

- [ ] **Step 1: Add a shared command connection accessor**

In `crates/hytte-services/src/bluetooth.rs`, in the imports section near line 34, ensure `tokio::sync::OnceCell` is imported (use the tokio one, not std, because initialization is async):

```rust
use tokio::sync::OnceCell;
```

(`tokio::sync::OnceCell` is already exported via the `sync` feature, which the workspace already enables — confirm by checking `crates/hytte-services/Cargo.toml`. If it isn't, add it to the existing `tokio` features.)

Add a private accessor right above the `// ── Command helpers` section (around line 510):

```rust
/// Shared command-channel connection. BlueZ owns sessions (e.g. for
/// `StartDiscovery`) per bus client; using a fresh connection per call
/// breaks Start/Stop pairing because BlueZ sees them as different
/// clients. Lazily initialized on first command call.
static CMD_CONN: OnceCell<Connection> = OnceCell::const_new();

async fn cmd_conn() -> Result<&'static Connection> {
    CMD_CONN
        .get_or_try_init(|| async {
            Connection::system()
                .await
                .context("open shared bluetooth command connection")
        })
        .await
}
```

- [ ] **Step 2: Refactor `do_set_adapter_bool` to take `&Connection`**

Change the signature and body to use the shared connection:

```rust
async fn do_set_adapter_bool(adapter_path: &str, prop: &str, on: bool) -> Result<()> {
    let conn = cmd_conn().await?;
    conn.call_method(
        Some("org.bluez"),
        adapter_path,
        Some("org.freedesktop.DBus.Properties"),
        "Set",
        &(
            "org.bluez.Adapter1",
            prop,
            zbus::zvariant::Value::from(on),
        ),
    )
    .await
    .with_context(|| format!("call Properties.Set Adapter1.{prop}"))?;
    Ok(())
}
```

(No callers change — same signature.)

- [ ] **Step 3: Refactor `do_set_device_bool` the same way**

Replace the `let conn = Connection::system().await...` body with `let conn = cmd_conn().await?;`. Keep everything else identical.

- [ ] **Step 4: Refactor `do_adapter_call` the same way**

Replace the `Connection::system()` body with `let conn = cmd_conn().await?;`. The rest of the function body is unchanged.

- [ ] **Step 5: Refactor `do_device_call` the same way**

Replace the `Connection::system()` body with `let conn = cmd_conn().await?;`.

- [ ] **Step 6: Refactor `do_remove_device` the same way**

Replace the `Connection::system()` body with `let conn = cmd_conn().await?;`.

- [ ] **Step 7: Audit any remaining `Connection::system()` call in this file**

Run: `grep -n 'Connection::system' crates/hytte-services/src/bluetooth.rs`
Expected: only two surviving call sites — the listen loop's `listen()` (line ~759) and the agent registration (line ~1204).

- [ ] **Step 8: Decide whether to migrate the agent registration**

The agent registration (`register_agent`, line ~1204) opens `Connection::system()` to call `org.bluez.AgentManager1.RegisterAgent`. The agent path it registers belongs to that specific connection — the agent service needs to receive callbacks on the same connection. Migrating *that* registration to `cmd_conn()` would change the agent's owning connection, which couples agent dispatch with command dispatch.

Decision for v0.2.1: **leave agent registration on its own dedicated `Connection::system()`**. This is intentional — keeping the agent's bus identity stable across command churn matters for pairing-prompt semantics. Add a one-line comment above the existing `let conn = Connection::system()...` in the agent registration path:

```rust
// Distinct connection from CMD_CONN: the agent path is owned by this
// connection, and BlueZ delivers Agent1 callbacks to the same one.
```

- [ ] **Step 9: Build + clippy**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 10: Run existing service tests**

Run: `cargo test -p hytte-services`
Expected: green. The bluetooth service has no unit tests against a real BlueZ; this just confirms nothing else broke.

- [ ] **Step 11: Commit**

```bash
git add crates/hytte-services/src/bluetooth.rs
git commit -m "$(cat <<'EOF'
fix(bluetooth): share a single command connection across calls

BlueZ tracks discovery (and other) sessions by bus client. The previous
implementation opened a fresh Connection::system() inside every command
helper, so StartDiscovery and StopDiscovery were issued from different
client identities — BlueZ would reject the Stop with "No discovery
started" and the scan would keep running.

Lift a shared `tokio::sync::OnceCell<Connection>` (CMD_CONN) and route
all do_adapter_call / do_device_call / do_set_* helpers through it. The
agent registration keeps its own Connection on purpose; agent callback
dispatch must arrive on the connection that owns the agent path.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Migrate BT scan toggle to `bind_two_way` + verify the bug is fixed

**Files:**
- Modify: `trollshell/src/widgets/pages.rs` (~line 732)
- Modify: `BUGS.md`

**Depends on:** Task 1 (`bind_two_way` available) and Task 4 (shared connection).

- [ ] **Step 1: Migrate the BT scan toggle**

Replace:

```rust
let scan_btn = gtk::ToggleButton::new();
scan_btn.set_valign(gtk::Align::Center);
bind(
    bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discovering)),
    &scan_btn,
    |w, discovering| {
        if w.is_active() != discovering {
            w.set_active(discovering);
        }
        w.set_label(if discovering { "Stop" } else { "Scan" });
    },
);
scan_btn.connect_toggled(|btn| {
    if btn.is_active() {
        bluetooth::start_discovery();
    } else {
        bluetooth::stop_discovery();
    }
});
```

with:

```rust
let scan_btn = gtk::ToggleButton::new();
scan_btn.set_valign(gtk::Align::Center);
bind_two_way(
    bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discovering)),
    &scan_btn,
    |w, discovering| {
        w.set_active(discovering);
        w.set_label(if discovering { "Stop" } else { "Scan" });
    },
    |w| w.connect_toggled(|btn| {
        if btn.is_active() {
            bluetooth::start_discovery();
        } else {
            bluetooth::stop_discovery();
        }
    }),
);
```

- [ ] **Step 2: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Manual verification of `BUGS.md`**

In one terminal:

```sh
bluetoothctl --monitor
```

Watch for `Discovering = yes` / `Discovering = no` lines.

In another terminal: `cargo run --release -p trollshell` (in a Niri session).

1. Open the Bluetooth drawer.
2. Click **Scan**. Observe `Discovering = yes` in the monitor; the button label flips to **Stop**; the spinner appears.
3. Click **Stop**. Observe `Discovering = no` in the monitor within a second or two; the button label flips back to **Scan**; the spinner disappears.
4. Repeat steps 2–3 twice more to confirm reliability. There should be no `org.bluez.Error.Failed` printed for `StopDiscovery` in `bluetoothctl` output.

If `Discovering = no` does not arrive after Stop, the fix is incomplete — re-investigate before continuing.

- [ ] **Step 4: Update `BUGS.md`**

Open `BUGS.md`. The contents are:

```
# Bluetooth 

Stop button not stops scanning
```

Replace with an empty file (so the next polish cycle has a place to record bugs without mistakenly carrying this one forward):

```
# Bugs

(none currently tracked)
```

- [ ] **Step 5: Commit**

```bash
git add trollshell/src/widgets/pages.rs BUGS.md
git commit -m "$(cat <<'EOF'
fix(de): bluetooth Stop button reliably ends discovery

Migrates the scan ToggleButton to bind_two_way (paired with the
shared CMD_CONN fix in the previous commit). BlueZ now sees Start
and Stop on the same client identity and the scan stops as expected.

Closes BUGS.md entry.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: BT-audio MAC casing

**Files:**
- Modify: `crates/hytte-services/src/bluetooth_audio.rs`

**Background:** `sink_belongs_to_device` substring-matches a pipewire sink name against `MAC_TO_PW_TOKEN(addr)`. BlueZ paths use uppercase MACs; pipewire sometimes uses lowercase node names (observed on `bluez_input.aa_bb_…`). Mismatch → auto-switch fails.

- [ ] **Step 1: Write the failing test**

In `crates/hytte-services/src/bluetooth_audio.rs`, add to the existing `#[cfg(test)] mod tests` block (or create one if missing):

```rust
#[test]
fn sink_belongs_to_device_matches_lowercase_pw_name() {
    // BlueZ paths use uppercase MACs; pipewire occasionally emits the
    // node name with the MAC lowercased (observed on bluez_input nodes).
    // The match must be case-insensitive on the MAC token.
    let dev = Device {
        path: "/org/bluez/hci0/dev_AC_C5_8B_11_22_33".to_string(),
        address: "AC:C5:8B:11:22:33".to_string(),
        ..Device::default()
    };
    assert!(sink_belongs_to_device(
        "bluez_input.ac_c5_8b_11_22_33.headset-head-unit",
        &dev,
    ));
}

#[test]
fn sink_belongs_to_device_still_matches_uppercase() {
    let dev = Device {
        path: "/org/bluez/hci0/dev_AC_C5_8B_11_22_33".to_string(),
        address: "AC:C5:8B:11:22:33".to_string(),
        ..Device::default()
    };
    assert!(sink_belongs_to_device(
        "bluez_output.AC_C5_8B_11_22_33.1",
        &dev,
    ));
}

#[test]
fn sink_belongs_to_device_rejects_other_mac() {
    let dev = Device {
        path: "/org/bluez/hci0/dev_AC_C5_8B_11_22_33".to_string(),
        address: "AC:C5:8B:11:22:33".to_string(),
        ..Device::default()
    };
    assert!(!sink_belongs_to_device(
        "bluez_output.DE_AD_BE_EF_00_00.1",
        &dev,
    ));
}
```

If `Device` is not in scope inside the tests block, add `use super::*;` at the top of the block (the `Device` type is re-exported from the bluetooth service into this module — verify with the existing imports in `bluetooth_audio.rs`).

- [ ] **Step 2: Run tests and verify the lowercase one fails**

Run: `cargo test -p hytte-services bluetooth_audio::tests::sink_belongs_to_device -- --nocapture`
Expected: `sink_belongs_to_device_matches_lowercase_pw_name` FAILS; the other two pass.

- [ ] **Step 3: Fix `sink_belongs_to_device`**

Locate the function (around line 210). Replace:

```rust
fn sink_belongs_to_device(sink_name: &str, device: &Device) -> bool {
    if device.address.is_empty() {
        return false;
    }
    let token = mac_to_pw_token(&device.address);
    // Belt-and-suspenders: substring + the conventional "bluez_output" prefix
    // is the realistic shape; we also allow bluez_input / bluez_sink variants
    // by leaving the prefix unchecked.
    // TODO(bt-audio-followup): consider case-insensitive MAC match if BlueZ
    // ever emits lowercase MACs. Today everything we've seen is upper-case.
    sink_name.contains(&token)
}
```

with:

```rust
fn sink_belongs_to_device(sink_name: &str, device: &Device) -> bool {
    if device.address.is_empty() {
        return false;
    }
    // Pipewire sometimes lowercases MAC tokens in node names
    // (`bluez_input.ac_c5_…`) while BlueZ paths use uppercase. Match
    // case-insensitively to cover both shapes.
    let token = mac_to_pw_token(&device.address).to_ascii_uppercase();
    sink_name.to_ascii_uppercase().contains(&token)
}
```

(Uppercasing both sides is allocation-free if either side is already ASCII; for the volumes this is called at, two short owned `String`s per call are negligible.)

- [ ] **Step 4: Run tests and verify all three pass**

Run: `cargo test -p hytte-services bluetooth_audio::tests::sink_belongs_to_device -- --nocapture`
Expected: 3 passed.

- [ ] **Step 5: Build + clippy**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/bluetooth_audio.rs
git commit -m "$(cat <<'EOF'
fix(bt-audio): match pipewire sink names case-insensitively on MAC

Pipewire occasionally lowercases MAC tokens in bluez node names
(bluez_input.ac_c5_…); BlueZ paths use uppercase. The previous
substring check missed lowercased pw names, so auto-switch failed
on those devices.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Cliphist id-based delete (service + UI)

**Files:**
- Modify: `crates/hytte-services/src/clipboard.rs`
- Modify: `trollshell/src/widgets/pages.rs` (around the `build_clipboard_row` function, ~line 2660)

**Background:** `cliphist delete` reads `<id>\t<preview>` lines from stdin (the same format `cliphist list` emits) and removes any matching entry. We don't store the original raw line on `ClipEntry`, so the safe approach is: re-run `cliphist list`, find the line whose id-prefix matches, and pipe THAT line into `cliphist delete`. Two subprocess calls per delete is acceptable — delete is rare and correctness matters.

The UI: add a ⋮ `gtk::MenuButton` suffix to each clipboard row; popover contains a single destructive "Delete entry" button. Per project memory, destructive actions go in popovers, not row click targets. The row's existing click target stays as paste-and-dismiss.

- [ ] **Step 1: Write the failing test for the delete path**

This test exercises the line-selection logic, not the subprocess call itself. We refactor the matching into a pure function so it's testable.

In `crates/hytte-services/src/clipboard.rs`, find the existing `#[cfg(test)] mod tests` block. Add:

```rust
#[test]
fn select_delete_line_finds_matching_id() {
    let raw = "1\thello\n42\ttarget\n3\tnope\n";
    assert_eq!(select_delete_line(raw, 42), Some("42\ttarget".to_string()));
}

#[test]
fn select_delete_line_returns_none_when_id_missing() {
    let raw = "1\thello\n3\tnope\n";
    assert_eq!(select_delete_line(raw, 42), None);
}

#[test]
fn select_delete_line_skips_garbage_rows() {
    let raw = "garbage-line\n42\ttarget\n";
    assert_eq!(select_delete_line(raw, 42), Some("42\ttarget".to_string()));
}
```

- [ ] **Step 2: Run tests and verify failure**

Run: `cargo test -p hytte-services clipboard::tests::select_delete_line -- --nocapture`
Expected: compile error — `select_delete_line` not defined.

- [ ] **Step 3: Implement `select_delete_line` and `delete`**

In `crates/hytte-services/src/clipboard.rs`, add a new public command in the "Public API" section (just below `paste_entry`):

```rust
/// Delete a history entry by id. Re-runs `cliphist list` to obtain the
/// exact line cliphist will recognize, then pipes that line into
/// `cliphist delete`. Refreshes [`history()`] afterwards.
///
/// Fire-and-forget; failures are logged at warn.
pub fn delete(id: u64) {
    runtime::handle().spawn_blocking(move || {
        if let Err(e) = run_delete_by_id(id) {
            tracing::warn!(id, error = %e, "clipboard: delete failed");
        }
    });
    refresh();
}
```

Add the helper functions in the "Subprocess helpers" section, below `run_decode_to_wlcopy`:

```rust
/// Find the `<id>\t<preview>` line in `cliphist list` output whose
/// integer prefix equals `id`. Returns the line trimmed of the trailing
/// newline (suitable for piping into `cliphist delete` with an explicit
/// `\n` appended).
fn select_delete_line(list_output: &str, id: u64) -> Option<String> {
    for line in list_output.lines() {
        let Some((id_part, _)) = line.split_once('\t') else {
            continue;
        };
        let Ok(parsed) = id_part.trim().parse::<u64>() else {
            continue;
        };
        if parsed == id {
            return Some(line.to_string());
        }
    }
    None
}

fn run_delete_by_id(id: u64) -> anyhow::Result<()> {
    use std::io::Write as _;

    let list = std::process::Command::new("cliphist")
        .arg("list")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("spawn cliphist list (for delete): {e}"))?;
    if !list.status.success() {
        return Err(anyhow::anyhow!("cliphist list (for delete) exited {:?}", list.status));
    }
    let stdout = String::from_utf8_lossy(&list.stdout);
    let Some(line) = select_delete_line(&stdout, id) else {
        // Entry already gone (concurrent delete, or id stale). Treat as
        // success so the caller's refresh still runs.
        return Ok(());
    };

    let mut delete = std::process::Command::new("cliphist")
        .arg("delete")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn cliphist delete: {e}"))?;
    {
        let mut stdin = delete.stdin.take()
            .ok_or_else(|| anyhow::anyhow!("cliphist delete: no stdin pipe"))?;
        stdin.write_all(line.as_bytes())
            .map_err(|e| anyhow::anyhow!("write cliphist delete stdin: {e}"))?;
        stdin.write_all(b"\n")
            .map_err(|e| anyhow::anyhow!("write cliphist delete newline: {e}"))?;
    }
    let status = delete.wait()
        .map_err(|e| anyhow::anyhow!("wait cliphist delete: {e}"))?;
    if !status.success() {
        return Err(anyhow::anyhow!("cliphist delete exited {status:?}"));
    }
    Ok(())
}
```

- [ ] **Step 4: Update the module docstring**

The module's TODO note at the top (around line 25) reads:

```rust
//! No delete API. cliphist's `cliphist delete` reads `<id>\t<preview>`
//! lines from stdin (it doesn't take an id argument), which means a clean
//! "delete by id" call would have to feed back the exact line we got from
//! `cliphist list`. Skipped until there's UI demand. TODO.
```

Replace with:

```rust
//! Delete by id is supported via [`delete`]. Implementation runs
//! `cliphist list` to get the exact line cliphist recognises, then
//! pipes that line into `cliphist delete`. Two subprocess calls per
//! delete; acceptable since deletion is user-initiated.
```

- [ ] **Step 5: Run tests and verify they pass**

Run: `cargo test -p hytte-services clipboard::tests -- --nocapture`
Expected: existing tests still pass + 3 new ones pass.

- [ ] **Step 6: Add a ⋮ MenuButton popover to clipboard rows in the UI**

In `trollshell/src/widgets/pages.rs`, find `build_clipboard_row` (around line 2660). Replace:

```rust
fn build_clipboard_row(entry: &ClipEntry) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&entry.preview)
        .activatable(true)
        .build();
    row.set_title_lines(1);

    let icon_name = match entry.kind {
        ClipKind::Image => "image-x-generic-symbolic",
        ClipKind::Text => "edit-paste-symbolic",
    };
    let icon = gtk::Image::from_icon_name(icon_name);
    row.add_prefix(&icon);

    let id = entry.id;
    row.connect_activated(move |_| {
        clipboard::paste_entry(id);
        crate::modal::dismiss_all();
    });

    row
}
```

with:

```rust
fn build_clipboard_row(entry: &ClipEntry) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&entry.preview)
        .activatable(true)
        .build();
    row.set_title_lines(1);

    let icon_name = match entry.kind {
        ClipKind::Image => "image-x-generic-symbolic",
        ClipKind::Text => "edit-paste-symbolic",
    };
    let icon = gtk::Image::from_icon_name(icon_name);
    row.add_prefix(&icon);

    // ⋮ menu button: destructive "Delete entry" lives here, not on the row's
    // primary click target — destructive actions belong in a popover so they
    // can't be misclicked while reaching for paste.
    let menu_btn = gtk::MenuButton::new();
    menu_btn.set_icon_name("view-more-symbolic");
    menu_btn.set_valign(gtk::Align::Center);
    menu_btn.add_css_class("flat");
    menu_btn.set_tooltip_text(Some("More actions"));

    let popover = gtk::Popover::new();
    let popover_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    popover_box.set_margin_top(4);
    popover_box.set_margin_bottom(4);
    popover_box.set_margin_start(4);
    popover_box.set_margin_end(4);

    let delete_btn = gtk::Button::with_label("Delete entry");
    delete_btn.add_css_class("flat");
    delete_btn.add_css_class("destructive-action");
    let id_for_delete = entry.id;
    let popover_for_delete = popover.clone();
    delete_btn.connect_clicked(move |_| {
        clipboard::delete(id_for_delete);
        popover_for_delete.popdown();
    });
    popover_box.append(&delete_btn);
    popover.set_child(Some(&popover_box));
    menu_btn.set_popover(Some(&popover));
    row.add_suffix(&menu_btn);

    let id = entry.id;
    row.connect_activated(move |_| {
        clipboard::paste_entry(id);
        crate::modal::dismiss_all();
    });

    row
}
```

- [ ] **Step 7: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Manual smoke test**

In Niri: copy a few text snippets so cliphist has entries. Open the Clipboard drawer. For an entry, click the ⋮ button → click **Delete entry**. The entry should disappear within a second (after `refresh()` re-emits). Click the row body itself for paste-and-dismiss to confirm that path still works.

- [ ] **Step 9: Commit**

```bash
git add crates/hytte-services/src/clipboard.rs trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(clipboard): id-based delete

Adds clipboard::delete(id) wired to a ⋮ MenuButton popover on each
clipboard row. cliphist delete reads `<id>\t<preview>` lines from
stdin; the implementation re-runs `cliphist list` to obtain the exact
line and pipes it through.

Destructive action lives in the popover (per project convention)
rather than on the row's primary click target.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Polkit second `PAM_PROMPT_ECHO_OFF` (confirm-new-password flow)

**Files:**
- Modify: `crates/hytte-services/src/polkit.rs` (around line 360)
- Modify: `trollshell/src/widgets/polkit_dialog.rs`

**Background:** The current PAM conversation in `run_helper` (`polkit.rs:324`) handles a single `PAM_PROMPT_ECHO_OFF` by writing the held password and dropping it. PAM flows like password-change can issue a second `PAM_PROMPT_ECHO_OFF` ("Retype new password"). Today the dialog has already torn down by then; the second prompt is never shown.

This is the most complex of the TODOs because it requires coordinating service state (the helper is mid-conversation) with UI state (the dialog needs to re-prompt). The simplest correct approach: extend `AuthPrompt` to carry an optional follow-up prompt text, and have the dialog stay mounted across the second exchange. The helper loop is rewritten to support multiple sequential password slots.

**Approach:**

1. Service-side: `run_helper` no longer takes a single `Zeroizing<String> password`. Instead, on every `PAM_PROMPT_ECHO_OFF`, it asks the dialog for *fresh input* via a new internal channel, mirroring how the first password is fetched today.
2. The first prompt is delivered by populating `AuthPrompt::message` from polkit's caller-supplied text; subsequent prompts are delivered through the same channel.
3. Dialog-side: keep the window mounted, swap the prompt label, clear the entry, await user input.

**Existing service shape (relevant pieces, from prior grep):**
- `AuthPrompt` carries `action_id`, `message`, `identities`. (Line 63.)
- `auth_prompts() -> impl Signal<Item = Option<AuthPrompt>>`. (Line 156.)
- `respond_to_auth(Option<(Zeroizing<String>, u32)>)` is how the dialog returns. (Line 173.)
- `await_reply` (line 214) is the one-shot await used today.
- `run_helper` (line 324) holds a single password value.

**Approach to coordinating multi-prompt state:**

Add a second one-shot-channel-cycle: after the first `respond_to_auth(Some((pw, uid)))`, if the helper issues another `PAM_PROMPT_ECHO_OFF`, the service emits a *new* `AuthPrompt` (same action_id/identities, new prompt text). The dialog widget treats this as an in-place re-prompt instead of a fresh dialog: it observes the existing `DIALOG_WINDOW` thread-local and updates the prompt label + clears the entry instead of building a new window.

To distinguish "first prompt" from "follow-up", add a field:

```rust
pub struct AuthPrompt {
    pub action_id: String,
    pub message: String,
    pub identities: Vec<Identity>,
    /// True when this prompt arrives mid-flight (e.g. a confirm-new-
    /// password follow-up). The dialog updates an existing window
    /// in-place rather than rebuilding.
    pub follow_up: bool,
}
```

That field is the contract.

- [ ] **Step 1: Add `follow_up` to `AuthPrompt`**

Locate the `AuthPrompt` definition in `crates/hytte-services/src/polkit.rs` (around line 63). Add the new field with a default of `false`. Update any constructor sites (`AuthPrompt { ... }`) within the file to include `follow_up: false`.

- [ ] **Step 2: Refactor `run_helper` to loop for additional `PAM_PROMPT_ECHO_OFF`**

Locate `run_helper` (line 324). The current flow holds `password_slot: Option<Zeroizing<String>>` initialized to the user-supplied first password and consumed on the first `PAM_PROMPT_ECHO_OFF`.

Rewrite so each `PAM_PROMPT_ECHO_OFF` after the first issues a fresh `AuthPrompt { follow_up: true, message: <prompt-text-from-helper> }` and awaits a response via the existing `set_prompt` / `await_reply` channel. The function signature changes to accept ONLY the *first* password; subsequent responses are obtained from the user via the dialog.

Replace the `match tag` arm:

```rust
"PAM_PROMPT_ECHO_OFF" => {
    // TODO(polkit-followup): I4 — handle a second PAM_PROMPT_ECHO_OFF (rare; e.g. confirm-new-password flows).
    let pw = password_slot.take().unwrap_or_default();
    stdin
        .write_all(pw.as_bytes())
        .await
        .context("write password")?;
    stdin
        .write_all(b"\n")
        .await
        .context("write password newline")?;
    drop(pw);
}
```

with:

```rust
"PAM_PROMPT_ECHO_OFF" => {
    // First prompt: serve from password_slot (the password the user
    // already typed in the initial dialog). Subsequent prompts come
    // from a follow-up dialog round-trip — common in password-change
    // flows like passwd's "Retype new password".
    let pw = if let Some(pw) = password_slot.take() {
        pw
    } else {
        let prompt = AuthPrompt {
            action_id: action_id.clone(),
            message: rest.to_string(),
            identities: identities.clone(),
            follow_up: true,
        };
        match await_followup_password(prompt).await {
            Some(pw) => pw,
            None => {
                // User cancelled the follow-up; tear the helper down.
                authenticated = false;
                break;
            }
        }
    };
    stdin
        .write_all(pw.as_bytes())
        .await
        .context("write password")?;
    stdin
        .write_all(b"\n")
        .await
        .context("write password newline")?;
    drop(pw);
}
```

This requires `action_id` and `identities` to be in scope inside `run_helper`. Find the existing call to `run_helper(...)` (around line ~478, inside the agent-callback arm) — add the two values to the call, and update `run_helper`'s signature to accept them. The call site already has `action_id` and `identities` (it just constructed the `AuthPrompt`); pipe them in.

- [ ] **Step 3: Implement `await_followup_password`**

Add to `crates/hytte-services/src/polkit.rs`, near `await_reply` (line 214):

```rust
/// Service-internal: emit a follow-up `AuthPrompt` and wait for the
/// dialog's reply. `Some(pw)` on submit, `None` on user cancel.
async fn await_followup_password(prompt: AuthPrompt) -> Option<Zeroizing<String>> {
    set_prompt(Some(prompt.clone()));
    let reply = await_reply(prompt).await;
    set_prompt(None);
    match reply {
        UserReply::Confirm { password, .. } => Some(password),
        UserReply::Cancel => None,
    }
}
```

(`UserReply` is already declared in this file; check its variants and adjust the destructuring above to match. The `..` pattern handles whatever extra fields may exist — adjust to specific names if `UserReply::Confirm` is a tuple variant.)

If `UserReply` is a tuple variant `Confirm(Zeroizing<String>, u32)`, write:

```rust
match reply {
    UserReply::Confirm(password, _uid) => Some(password),
    UserReply::Cancel => None,
}
```

- [ ] **Step 4: Update the dialog widget to handle `follow_up`**

In `trollshell/src/widgets/polkit_dialog.rs`, the `install` function subscribes to `auth_prompts()` and dispatches `Some(req)` → `show_dialog` and `None` → `close_dialog`. Modify `show_dialog` to take a different path when `prompt.follow_up == true`:

Find:

```rust
glib::MainContext::default().spawn_local(
    polkit::auth_prompts().for_each(move |prompt| {
        match prompt {
            Some(req) => show_dialog(&monitor, req),
            None => close_dialog(),
        }
        std::future::ready(())
    }),
);
```

Replace with:

```rust
glib::MainContext::default().spawn_local(
    polkit::auth_prompts().for_each(move |prompt| {
        match prompt {
            Some(req) if req.follow_up => update_dialog_for_followup(&req),
            Some(req) => show_dialog(&monitor, req),
            None => close_dialog(),
        }
        std::future::ready(())
    }),
);
```

Add a new function below `close_dialog` (above `show_dialog`):

```rust
/// Update the existing dialog in-place for a follow-up PAM prompt
/// (e.g. "Retype new password"). The window stays mounted and
/// keyboard-grabbed; only the prompt label and entry contents change.
/// If no dialog is currently mounted (shouldn't happen — follow-ups
/// only emit while the first dialog is up), fall back to building a
/// fresh window so the user isn't stranded.
fn update_dialog_for_followup(prompt: &AuthPrompt) {
    let updated = DIALOG_WINDOW.with(|slot| {
        let slot = slot.borrow();
        let Some(window) = slot.as_ref() else {
            return false;
        };
        // Walk the child tree to find the prompt label and the
        // PasswordEntry. The dialog is a fixed shape; the labels are
        // appended in show_dialog in a known order. Use widget names
        // to stay robust to layout tweaks: tag the relevant widgets in
        // show_dialog with set_widget_name(...) before this lands.
        let Some(root) = window.child() else { return false; };
        let mut walker = WidgetWalker::new(root);
        if let Some(label) = walker.find_named("ts-prompt-followup-label") {
            if let Ok(label) = label.downcast::<gtk::Label>() {
                label.set_text(&prompt.message);
                label.set_visible(!prompt.message.is_empty());
            }
        }
        if let Some(entry_w) = walker.find_named("ts-prompt-password-entry") {
            if let Ok(entry) = entry_w.downcast::<gtk::PasswordEntry>() {
                entry.set_text("");
                entry.grab_focus();
            }
        }
        true
    });
    if !updated {
        tracing::warn!("polkit follow-up prompt arrived without an existing dialog");
    }
}

/// Iterator over a widget's descendants for `find_named`.
struct WidgetWalker {
    queue: std::collections::VecDeque<gtk::Widget>,
}
impl WidgetWalker {
    fn new(root: gtk::Widget) -> Self {
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(root);
        Self { queue }
    }
    fn find_named(&mut self, name: &str) -> Option<gtk::Widget> {
        while let Some(w) = self.queue.pop_front() {
            if w.widget_name() == name {
                return Some(w);
            }
            let mut child = w.first_child();
            while let Some(c) = child {
                self.queue.push_back(c.clone());
                child = c.next_sibling();
            }
        }
        None
    }
}
```

Now tag the password entry and a follow-up label in `show_dialog`. Find the password entry construction:

```rust
let entry = gtk::PasswordEntry::new();
entry.set_show_peek_icon(true);
entry.set_margin_top(8);
vbox.append(&entry);
```

Replace with:

```rust
let entry = gtk::PasswordEntry::new();
entry.set_show_peek_icon(true);
entry.set_margin_top(8);
entry.set_widget_name("ts-prompt-password-entry");
vbox.append(&entry);

// Hidden by default; populated and shown when a follow-up PAM prompt
// arrives (e.g. "Retype new password" in a password-change flow).
let followup_label = gtk::Label::new(None);
followup_label.set_widget_name("ts-prompt-followup-label");
followup_label.set_xalign(0.0);
followup_label.set_wrap(true);
followup_label.add_css_class("ts-prompt-followup");
followup_label.set_visible(false);
vbox.append(&followup_label);
```

- [ ] **Step 5: Build + clippy**

Run: `cargo build -p hytte-services -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. If `await_reply`'s return shape differs from what Step 3 assumed, fix the destructuring; the rest stands.

- [ ] **Step 6: Manual smoke test (best-effort)**

A real confirm-new-password polkit flow is hard to trigger casually. The reproducible test path is to trigger any policy that doesn't issue a follow-up — confirm the existing single-prompt path still works (e.g. `pkexec` something benign in a Niri session, or wait for an organic polkit prompt). Mark this step DONE once the regular polkit dialog still authenticates correctly. The follow-up code path stays unverified but reachable; document this honestly in the commit message.

- [ ] **Step 7: Commit**

```bash
git add crates/hytte-services/src/polkit.rs trollshell/src/widgets/polkit_dialog.rs
git commit -m "$(cat <<'EOF'
feat(polkit): handle follow-up PAM_PROMPT_ECHO_OFF in-place

PAM password-change flows can issue a second PAM_PROMPT_ECHO_OFF
("Retype new password") after the first. The conversation now loops:
the first password comes from the initial dialog as before, and
subsequent prompts emit AuthPrompt { follow_up: true, .. } which the
dialog handles by clearing the entry and updating the prompt label
in-place rather than rebuilding the window.

The single-prompt path is unchanged. The follow-up path is reachable
but hard to validate end-to-end without a live password-change
policy; the implementation mirrors the established channel pattern.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Notification toast overflow ("+N more")

**Files:**
- Modify: `trollshell/src/widgets/notifications.rs`

**Background:** The toast surface currently renders one card per active notification. Under a fast burst of 5+ active notifications, the column grows tall and busy. Per the existing `TODO(notif-followup)` (line 27): cap visible toasts and add a synthetic "+N more" overflow card. The full history continues to live on the Notifications drawer page.

**Behavior:**
- Up to 4 individual toasts visible.
- When a 5th would appear, drop the oldest and ensure a single `+N more` overflow card exists. N counts how many active notifications are NOT individually toasted.
- Increment N on subsequent arrivals; decrement when an underlying notification leaves the active set (dismissed, expired, or invoked-action). When N drops to 0, remove the overflow card.
- Click on the overflow card opens the Notifications drawer page (use the existing `crate::modal::open(Page::Notifications)` or whatever the bar's notification chip uses — check `widgets/notif_indicator.rs`).
- Critical-urgency notifications bypass the overflow cap (always rendered as full toasts, per existing DND/mute parity).

- [ ] **Step 1: Locate the modal-open helper used by the notification chip**

Run: `grep -n 'Page::Notifications\|open.*Notifications' trollshell/src/widgets/notif_indicator.rs trollshell/src/modal.rs`

Note the function used. Likely something like `crate::modal::open(Page::Notifications, &monitor)` or a public `open_notifications` helper.

- [ ] **Step 2: Refactor the toast `for_each` to apply the cap**

In `trollshell/src/widgets/notifications.rs`, find the `toast_signal.for_each` block (line 105). The block computes a `visible: Vec<&Notification>` after applying DND + per-app-mute filters.

Modify the visible-set construction so that:

1. Critical visible notifications are kept unconditionally.
2. Non-critical visible notifications are partitioned into `head` (most recent ≤ `MAX_VISIBLE_NONCRITICAL`) and `tail` (the rest).
3. `head` cards render as before. `tail.len()` becomes the overflow count `N`.

Pick `const MAX_VISIBLE_NONCRITICAL: usize = 4;`.

The "most recent" ordering: `notifications::active()` is already ordered by arrival (verify by reading the notifications service if uncertain, but the existing widget treats arrival order implicitly). Take the *last* `MAX_VISIBLE_NONCRITICAL` non-critical entries as `head`; everything before becomes `tail`.

Add a new piece of state above the for-each (next to `card_map`):

```rust
let overflow_card: RefCell<Option<gtk::Widget>> = RefCell::new(None);
```

Inside the closure, after the existing `// Build id sets.` step, insert:

```rust
// Partition non-critical visible into head (rendered as cards) and
// tail (collapsed into a +N overflow card). Critical urgency always
// renders individually and never counts toward the cap.
let (critical_visible, noncritical_visible): (Vec<&Notification>, Vec<&Notification>) = visible
    .iter()
    .partition(|n| n.urgency == Urgency::Critical);
let nc_head_start = noncritical_visible
    .len()
    .saturating_sub(MAX_VISIBLE_NONCRITICAL);
let head_noncritical = &noncritical_visible[nc_head_start..];
let tail_noncritical_count = nc_head_start;

// Rebuild new_ids using head + critical_visible.
let new_ids: HashMap<u32, &Notification> = critical_visible
    .iter()
    .copied()
    .chain(head_noncritical.iter().copied())
    .map(|n| (n.id, n))
    .collect();
```

Replace the existing `let new_ids: HashMap<u32, &Notification> = visible.iter()...` line (~line 150) with the partition block above.

Then, after the existing card add/remove loop (~line 171), insert overflow management:

```rust
// Manage the overflow "+N more" card. Singleton, lives in
// `overflow_card`. Removed when tail is empty; re-built when tail
// count changes (so the label updates).
{
    let mut slot = overflow_card.borrow_mut();
    if tail_noncritical_count == 0 {
        if let Some(card) = slot.take() {
            vbox.remove(&card);
        }
    } else {
        // Drop any prior overflow card so the label reflects the
        // current count; cheap and avoids wiring a label binding.
        if let Some(card) = slot.take() {
            vbox.remove(&card);
        }
        let card = build_overflow_card(tail_noncritical_count);
        vbox.append(&card);
        *slot = Some(card);
    }
}
```

And update the window-visibility line (~line 174):

```rust
// Show/hide window based on whether any cards (individual or overflow) exist.
window.set_visible(!map.is_empty() || overflow_card.borrow().is_some());
```

- [ ] **Step 3: Add `MAX_VISIBLE_NONCRITICAL` and `build_overflow_card`**

Add to the constants section (or just below the imports) in `notifications.rs`:

```rust
const MAX_VISIBLE_NONCRITICAL: usize = 4;
```

Add a new builder function below `build_card`:

```rust
fn build_overflow_card(count: usize) -> gtk::Widget {
    let card = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    card.add_css_class("ts-toast");
    card.add_css_class("ts-toast-overflow");

    let icon = gtk::Image::from_icon_name("preferences-system-notifications-symbolic");
    icon.set_pixel_size(24);
    icon.add_css_class("ts-toast-image");
    card.append(&icon);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
    column.set_hexpand(true);

    let summary = gtk::Label::new(Some(&format!("+{count} more notifications")));
    summary.add_css_class("ts-toast-summary");
    summary.set_xalign(0.0);
    column.append(&summary);

    let body = gtk::Label::new(Some("Click to open Notifications"));
    body.add_css_class("ts-toast-body");
    body.set_xalign(0.0);
    column.append(&body);

    card.append(&column);

    let gesture = gtk::GestureClick::new();
    gesture.connect_pressed(move |gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        // Use the same modal-open helper the notif chip uses. The exact
        // call depends on how the rest of the bar opens drawer pages —
        // see widgets/notif_indicator.rs.
        crate::modal::open_notifications();
    });
    card.add_controller(gesture);

    card.upcast()
}
```

If `crate::modal::open_notifications` doesn't exist, use whatever helper Step 1 surfaced. If the existing call shape requires a `&Monitor` argument, capture one; the install fn already has `monitor: &Monitor`. Capture it in the for-each closure (`let monitor_for_overflow = monitor.clone();` near the top of `install`, then use it inside `build_overflow_card` via a closure parameter). Adjust `build_overflow_card` to accept `monitor: Monitor` if needed.

- [ ] **Step 4: Update the docstring**

Replace the `// ── Queue cap` comment block (line 25) from:

```rust
//! # Queue cap
//!
//! TODO(notif-followup): when bursts produce 5+ active toasts, show only the
//! latest 3 plus a synthetic "+N more" card that opens the drawer's
//! Notifications page on click. The current implementation renders one card
//! per active notification — fine for steady-state but visually noisy under
//! a fast burst. The notifications service itself does not queue; it tracks
//! the live set, so any cap is consumer-side.
```

to:

```rust
//! # Queue cap
//!
//! Up to [`MAX_VISIBLE_NONCRITICAL`] non-critical toasts render
//! individually. Additional non-critical toasts collapse into a
//! synthetic "+N more" card that opens the Notifications drawer on
//! click. Critical-urgency toasts always render individually and don't
//! count toward the cap. The notifications service itself does not
//! queue; it tracks the live set, so the cap is consumer-side only.
```

- [ ] **Step 5: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Manual smoke test**

In Niri:

```sh
for i in $(seq 1 7); do
  notify-send "Burst $i" "body $i"
  sleep 0.05
done
```

Expected: 4 individual toast cards + 1 "+3 more notifications" card visible. Click the overflow card → Notifications drawer opens. Dismiss individual toasts → overflow count stays steady (since dismissed individuals are gone from `active()`, the head/tail repartition rebalances). Dismiss enough that there's room for everyone individually → overflow card disappears.

- [ ] **Step 7: Commit**

```bash
git add trollshell/src/widgets/notifications.rs
git commit -m "$(cat <<'EOF'
feat(notifications): toast overflow with "+N more" card

Caps non-critical toasts at MAX_VISIBLE_NONCRITICAL (4) individual
cards. Additional non-critical notifications collapse into a single
"+N more" card whose click opens the Notifications drawer. Critical
toasts always render individually and don't count toward the cap.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Calendar click-day → highlight + scroll

**Files:**
- Modify: `trollshell/src/widgets/pages.rs` (`page_calendar`, ~line 2533, and `build_calendar_row`, ~line 2633)

**Background:** Calendar shows day-marks for events; clicking a day currently does nothing. The TODO at line 2553 wants click-day to scroll the events list to the first event of that date and visually highlight it.

**Approach:** Track event rows by date in a `Vec<(NaiveDate, gtk::Widget)>`. On `Calendar::day-selected`: find the first row matching the date, scroll to it, add a transient `.ts-cal-day-hit` CSS class for ~1.5s.

The events list is inside a `gtk::ScrolledWindow`? Check the calendar page structure first — the events `PreferencesGroup` is appended to `column`, which is a `page_box()`. Whether the column is itself wrapped in a scrolled window happens in `finish_page`. Confirm via:

`grep -n 'fn finish_page' trollshell/src/widgets/pages.rs`

If `finish_page` wraps in `gtk::ScrolledWindow`, scrolling the row into view uses the row's `compute_point` / `gtk_widget_translate_coordinates` to compute its y in the scroll viewport, then sets the scroll's `vadjustment` value.

If `finish_page` does not scroll, skip the scroll-step and just highlight in place.

- [ ] **Step 1: Inspect `finish_page`**

Read `pages.rs` around line 53 (`fn finish_page`). Note whether it wraps the content in a `gtk::ScrolledWindow`.

If it does: capture the inner `gtk::ScrolledWindow` reference for the calendar page (or, easier, set up a local scrolled window inside `page_calendar` for just the events `PreferencesGroup` so scrolling can be controlled directly).

The cleanest approach for v0.2.1 is: wrap only the `PreferencesGroup` inside `page_calendar` in its own bounded `gtk::ScrolledWindow`. The same pattern used for `wifi networks` (`pages.rs:499`).

- [ ] **Step 2: Wrap the events group in a `ScrolledWindow`**

In `page_calendar`, after `let group = adw::PreferencesGroup::builder().title("Upcoming").build();` (~line 2564), replace the line `column.append(&group);` with:

```rust
let scrolled = gtk::ScrolledWindow::new();
scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
scrolled.set_min_content_height(220);
scrolled.set_max_content_height(360);
scrolled.add_css_class("ts-calendar-list");
scrolled.set_child(Some(&group));
column.append(&scrolled);
```

- [ ] **Step 3: Track rows by date**

Change the existing `rows_track` declaration:

```rust
let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
```

to:

```rust
let rows_track: Rc<RefCell<Vec<(chrono::NaiveDate, adw::ActionRow)>>> = Rc::new(RefCell::new(Vec::new()));
```

In the bind closure on `calendar::events()`, where rows are pushed (~line 2602), replace:

```rust
let mut new_rows = Vec::with_capacity(evs.len());
for ev in &evs {
    let row = build_calendar_row(ev);
    group_for_bind.add(&row);
    new_rows.push(row);
}
*rows_for_bind.borrow_mut() = new_rows;
```

with:

```rust
let mut new_rows: Vec<(chrono::NaiveDate, adw::ActionRow)> = Vec::with_capacity(evs.len());
for ev in &evs {
    let row = build_calendar_row(ev);
    group_for_bind.add(&row);
    new_rows.push((ev.start.date_naive(), row));
}
*rows_for_bind.borrow_mut() = new_rows;
```

The drain at the top of the closure (~line 2579) needs the same shape change:

```rust
for (_date, row) in rows_for_bind.borrow_mut().drain(..) {
    group_for_bind.remove(&row);
}
```

- [ ] **Step 4: Wire `connect_day_selected`**

Below the existing `cal.connect_next_month` / `cal.connect_prev_month` blocks (~line 2561), add:

```rust
{
    let rows_for_select = rows_track.clone();
    let scrolled_for_select = scrolled.clone();
    cal.connect_day_selected(move |c| {
        let Some(d) = c.date().and_then(|gdt| {
            let y = gdt.year();
            let m = u32::try_from(gdt.month()).ok()?;
            let day = u32::try_from(gdt.day_of_month()).ok()?;
            chrono::NaiveDate::from_ymd_opt(y, m, day)
        }) else {
            return;
        };
        let rows = rows_for_select.borrow();
        let Some((_d, row)) = rows.iter().find(|(date, _)| *date == d) else {
            return;
        };
        scroll_row_into_view(&scrolled_for_select, row);
        flash_row_highlight(row);
    });
}
```

`gtk::Calendar::date()` returns `Option<glib::DateTime>` whose API exposes `year()`, `month()`, `day_of_month()`. (If your version of gtk-rs has slightly different accessors — verify by running `cargo doc -p gtk4 --open` and searching for `Calendar::date` — adjust accordingly. The intent is "convert the calendar's selected day to a `chrono::NaiveDate`".)

- [ ] **Step 5: Add `scroll_row_into_view` and `flash_row_highlight`**

Below `apply_event_marks` (~line 2631), add:

```rust
/// Scroll `scrolled` so that `row` is visible. Uses the row's allocated
/// position relative to the scrolled-window viewport.
fn scroll_row_into_view(scrolled: &gtk::ScrolledWindow, row: &adw::ActionRow) {
    let Some(child) = scrolled.child() else { return; };
    let Some((_, y)) = row.translate_coordinates(&child, 0.0, 0.0) else {
        return;
    };
    let adj = scrolled.vadjustment();
    let target = (y - 8.0).max(adj.lower());
    let max = (adj.upper() - adj.page_size()).max(adj.lower());
    adj.set_value(target.min(max));
}

/// Add `.ts-cal-day-hit` to `row` for ~1.5s, then remove it.
fn flash_row_highlight(row: &adw::ActionRow) {
    row.add_css_class("ts-cal-day-hit");
    let row_for_clear = row.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
        row_for_clear.remove_css_class("ts-cal-day-hit");
    });
}
```

- [ ] **Step 6: Add the `.ts-cal-day-hit` CSS class to the stylesheet**

The trollshell stylesheet lives in `etc/.../style.css` or similar — find it:

Run: `find . -name 'style.css' -path '*/etc/*' -o -name 'style.css' -path '*/hytte-ui/*' -o -name 'style.css' -path '*/trollshell/*'`

Locate the existing stylesheet (likely in `crates/hytte-ui/` or under `trollshell/`).

Per project memory: do not introduce new color tokens. Use an existing accent or selection token. If the stylesheet defines `@theme_selected_bg_color` or uses a libadwaita accent variable, reference it. The class only needs a brief background flash:

```css
.ts-cal-day-hit {
    background: alpha(@theme_selected_bg_color, 0.2);
    transition: background 600ms ease-out;
}
```

Append this rule near the bottom of the stylesheet, in the calendar-related section if one exists.

- [ ] **Step 7: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Manual smoke test**

In Niri, with the evolution-data-server cache populated (existing calendar setup): open the Calendar drawer. Click a day with an event-mark. The events list should scroll to that date's first event and the row should briefly flash a highlight. Click a day without a mark — nothing visible should happen (no row matches; safe no-op).

- [ ] **Step 9: Commit**

```bash
git add trollshell/src/widgets/pages.rs <stylesheet-path>
git commit -m "$(cat <<'EOF'
feat(calendar): click a marked day to scroll & flash matching event

Tracks event rows by NaiveDate. Calendar's day-selected scrolls the
events list to the first matching row and adds a transient
.ts-cal-day-hit class for ~1.5s. The events list is wrapped in a
bounded ScrolledWindow so scrolling can be driven directly.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review notes

After completing all tasks, verify:

- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green.
- `BUGS.md` no longer carries the bluetooth entry.
- `grep -n 'TODO(notif-followup)\|TODO(polkit-followup): I4\|TODO(bt-audio-followup)\|TODO: click on a marked day\|cliphist\b.*Skipped' .` returns nothing inside `crates/` and `trollshell/src/`.
- The five remaining TODOs in source (calendar-followup miscellaneous, niri v0.3+, polkit-followup other items, etc.) are NOT touched by this milestone — they were never in scope.

Done. Ready for execution.
