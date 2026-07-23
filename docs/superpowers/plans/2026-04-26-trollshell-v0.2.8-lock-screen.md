# trollshell v0.2.8 native lock screen Implementation Plan

> **Historical note (#204):** the swayidle idle pipeline this plan edits
> (`etc/swayidle/config`, the SIGSTOP/SIGCONT pause helpers) has been **retired**.
> trollshell now owns idle → dim → lock → suspend natively in-process — an
> `ext-idle-notify-v1` client gated on logind inhibitors, before-sleep relock via
> logind `PrepareForSleep`. See `crates/hytte-services/src/idle_notify.rs` and #204.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `screensaver::lock()`'s gtklock shell-out with a native trollshell lock surface (per-monitor, layer-shell, exclusive keyboard) that authenticates via in-process libpam and integrates with logind's `Session.Lock`/`Unlock` signals.

**Architecture:** New `hytte-pam` crate isolates the libpam C dep. `screensaver.rs` gains an `is_locked: Mutable<bool>` signal and a login1 listen loop that flips it on `Session.Lock`/`Unlock`. New `widgets/lock_screen.rs` mounts a `LockSurface` per monitor (primary holds password entry; secondaries are clock-only) and subscribes to `is_locked`. Authentication runs on `tokio::task::spawn_blocking`; on success the widget calls `screensaver::handle_unlock_success()` which clears the signal and tells logind via `Session.SetLockedHint(false)`. `etc/pam.d/trollshell` (`auth include login`) ships with the milestone.

**Tech Stack:** Rust 1.85+ stable (workspace edition 2024), GTK4 + libadwaita, `pam` 0.8 crate (system `libpam` headers required at build time), `zeroize`, `thiserror`, `zbus`, existing `nix` crate for username lookup.

**Conventions:**

- Workspace lints `pedantic = warn`, `module_name_repetitions = allow`, `missing_errors_doc = allow`, `missing_panics_doc = allow`. `unsafe_code = "forbid"` workspace-wide; the new code adds no `unsafe`.
- TDD where unit-testable (`hytte-pam` API smoke test only — real PAM needs a live stack).
- Commits use existing prefixes: `feat(pam):`, `feat(screensaver):`, `feat(de):`, `style:`.
- Co-author trailer on every commit:
  `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`

**Spec backing this plan:** `docs/superpowers/specs/2026-04-26-v0.2.8-lock-screen-design.md`

---

## File Structure

**Created:**

- `crates/hytte-pam/Cargo.toml`
- `crates/hytte-pam/src/lib.rs`
- `trollshell/src/widgets/lock_screen.rs`
- `etc/pam.d/trollshell` (one-line PAM service file)

**Modified:**

- `Cargo.toml` (workspace root) — add `crates/hytte-pam` to members.
- `trollshell/Cargo.toml` — add `hytte-pam` dep.
- `crates/hytte-services/src/screensaver.rs` — add `is_locked` signal, `handle_unlock_success`, `call_login1_unlock`, `listen_login1`, rewrite `lock()`, drop gtklock paths.
- `trollshell/src/widgets/mod.rs` — add `pub mod lock_screen;`.
- `trollshell/src/main.rs` — wire `widgets::lock_screen::install(&app.monitors())`.
- `trollshell/style.css` — append lock-screen rules.
- `etc/swayidle/config` — replace `gtklock` with `loginctl lock-session`.
- `etc/README.md` — add PAM install instructions.

---

## Task 1: `hytte-pam` crate

**Files:**

- Create: `crates/hytte-pam/Cargo.toml`
- Create: `crates/hytte-pam/src/lib.rs`
- Modify: `Cargo.toml` (workspace root)

**Background:** New crate isolates the libpam C dep. Single public function `authenticate(service, username, password) -> Result<(), PamError>`. Re-exports `Zeroizing<String>` from `zeroize` for password handling. Smoke test verifies the API surface compiles.

- [ ] **Step 1: Create `crates/hytte-pam/Cargo.toml`**

```toml
[package]
name = "hytte-pam"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "Synchronous PAM authentication for hytte-based shells"

[lints]
workspace = true

[dependencies]
pam = "0.8"
zeroize = "1.8"
thiserror = "1"
```

- [ ] **Step 2: Create `crates/hytte-pam/src/lib.rs`**

```rust
//! Synchronous PAM authentication for screen-unlock and similar
//! "verify the current user's password" flows.
//!
//! libpam itself is C and blocking. Authenticate from a
//! `tokio::task::spawn_blocking` so the GTK main loop isn't held up
//! by the PAM stack's I/O (notably `pam_unix` ↔ `unix_chkpwd`).

use thiserror::Error;
pub use zeroize::Zeroizing;

#[derive(Debug, Error)]
pub enum PamError {
    #[error("authentication failed")]
    AuthFailed,
    #[error("PAM service error: {0}")]
    Service(String),
    #[error("PAM session error: {0}")]
    Session(String),
}

/// Verify `password` against the PAM stack configured for `service`
/// as `username`. Returns `Ok(())` on success.
///
/// Blocks the calling thread. Always call from
/// `tokio::task::spawn_blocking` or a dedicated worker thread.
pub fn authenticate(
    service: &str,
    username: &str,
    password: Zeroizing<String>,
) -> Result<(), PamError> {
    use pam::Authenticator;

    let mut auth = Authenticator::with_password(service)
        .map_err(|e| PamError::Service(e.to_string()))?;
    auth.handler_mut()
        .set_credentials(username, password.as_str());
    auth.authenticate().map_err(|_| PamError::AuthFailed)?;
    auth.open_session()
        .map_err(|e| PamError::Session(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_surface_compiles() {
        let _: fn(&str, &str, Zeroizing<String>) -> Result<(), PamError> = authenticate;
    }
}
```

- [ ] **Step 3: Add to workspace members**

In `/home/choom/src/troll/trollshell-workspace/Cargo.toml`, find the `members = [...]` list. Add `"crates/hytte-pam",` alphabetically (after `"crates/hytte"` is fine — the order today is reactive, ui, services, hytte; insert between hytte and trollshell):

```toml
[workspace]
resolver = "3"
members = [
    "crates/hytte-reactive",
    "crates/hytte-ui",
    "crates/hytte-services",
    "crates/hytte",
    "crates/hytte-pam",
    "trollshell",
]
```

- [ ] **Step 4: Build + run the smoke test**

Run: `cargo build -p hytte-pam`
Expected: clean build. (`libpam-dev` / Arch `pam` headers must be present; they are on every system that ran `systemd setup`.)

Run: `cargo test -p hytte-pam`
Expected: 1 passed (`api_surface_compiles`).

- [ ] **Step 5: Workspace clippy clean**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/hytte-pam/
git commit -m "$(cat <<'EOF'
feat(pam): hytte-pam crate for synchronous PAM authentication

New crate isolates the libpam C dep. Public authenticate(service,
username, password) -> Result<(), PamError>. Re-exports
zeroize::Zeroizing for password handling. Smoke test verifies the
API surface compiles.

Consumed by the v0.2.8 lock screen.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `screensaver.rs` — `is_locked` signal + `handle_unlock_success` + `call_login1_unlock`

**Files:**

- Modify: `crates/hytte-services/src/screensaver.rs`

**Background:** Add a `Mutable<bool>` field to `ScreenSaverHandles` (note: existing struct name uses `ScreenSaver` capitalized in CamelCase, with lowercase `screensaver` in function paths). Expose via `is_locked()`. Add `handle_unlock_success()` that flips the mutable to false and asynchronously calls `Session.SetLockedHint(false)` on logind. `lock()` body rewrite + login1 listen loop come in Tasks 3 + 4.

- [ ] **Step 1: Add the `is_locked` field**

Find `ScreenSaverHandles` (around line 82). Add a new field at the end of the struct:

```rust
#[doc(hidden)]
pub struct ScreenSaverHandles {
    pub(crate) state: Arc<Mutex<HashMap<u32, Inhibitor>>>,
    pub(crate) inhibitors: Mutable<Vec<Inhibitor>>,
    pub(crate) next_cookie: Arc<AtomicU32>,
    pub(crate) is_locked: Mutable<bool>,
}
```

In `Default for ScreenSaverHandles` (around line 97), add the field initializer:

```rust
impl Default for ScreenSaverHandles {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
            inhibitors: Mutable::new(Vec::new()),
            next_cookie: Arc::new(AtomicU32::new(1)),
            is_locked: Mutable::new(false),
        }
    }
}
```

- [ ] **Step 2: Add the `is_locked()` public signal**

Below the existing `pub fn inhibitors()` (around line 146), add:

```rust
/// Signal emitting `true` while the lock UI is mounted, `false`
/// otherwise. Subscribed by `widgets::lock_screen` to drive the
/// per-monitor surfaces.
pub fn is_locked() -> impl Signal<Item = bool> {
    registry::with(|r| {
        r.get::<ScreenSaverHandles>()
            .expect("screensaver::service() not registered")
            .is_locked
            .signal_cloned()
    })
}
```

- [ ] **Step 3: Add `handle_unlock_success` + `call_login1_unlock`**

Below `is_locked()`:

```rust
/// Called by the lock UI after a successful PAM authentication.
/// Flips `is_locked` to false (which clears the lock surfaces) and
/// tells logind to release its session-level lock state via
/// `Session.SetLockedHint(false)`.
pub fn handle_unlock_success() {
    let handles = registry::with(|r| {
        r.get::<ScreenSaverHandles>().map(|h| h.is_locked.clone())
    });
    if let Some(locked) = handles {
        locked.set(false);
    }
    runtime::handle().spawn(async move {
        if let Err(e) = call_login1_unlock().await {
            tracing::warn!(error = %e, "login1 SetLockedHint(false) failed");
        }
    });
}

async fn call_login1_unlock() -> anyhow::Result<()> {
    use anyhow::Context;
    let conn = Connection::system().await.context("connect system bus")?;
    let manager = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    .context("login1 Manager proxy")?;
    let pid: u32 = std::process::id();
    let session_path: zbus::zvariant::OwnedObjectPath = manager
        .call("GetSessionByPID", &(pid,))
        .await
        .context("GetSessionByPID")?;
    let session = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        session_path.as_str(),
        "org.freedesktop.login1.Session",
    )
    .await
    .context("login1 Session proxy")?;
    session
        .call::<_, _, ()>("SetLockedHint", &(false,))
        .await
        .context("Session.SetLockedHint(false)")?;
    Ok(())
}
```

Verify `runtime::handle()` is in scope (it should be — used elsewhere in the file).

- [ ] **Step 4: Build + clippy**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-services/src/screensaver.rs
git commit -m "$(cat <<'EOF'
feat(screensaver): is_locked signal + handle_unlock_success

Adds is_locked: Mutable<bool> to ScreenSaverHandles, the public
is_locked() signal subscribers can listen to, and
handle_unlock_success() which the v0.2.8 lock UI calls after a
successful PAM authentication. handle_unlock_success flips the
signal to false and asynchronously calls
Session.SetLockedHint(false) on logind for in-session
correctness.

The lock() body rewrite and the login1 Lock/Unlock listen loop
land in subsequent commits.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `screensaver.rs` — rewrite `lock()` + drop gtklock paths

**Files:**

- Modify: `crates/hytte-services/src/screensaver.rs`

**Background:** Replace the body of `pub fn lock()` (which currently calls `spawn_locker`) with a flip of `is_locked` to true. Delete the now-unused gtklock spawn helpers, `TROLL_LOCK_CMD` env-var read, and update the module docstring.

- [ ] **Step 1: Rewrite `lock()` body**

Find `pub fn lock()` (around line 159). Replace the entire function body:

```rust
/// Trigger the lock surface. Flips `is_locked` to `true`; the
/// `widgets::lock_screen` subscription mounts the per-monitor lock
/// surfaces in response.
pub fn lock() {
    let handles = registry::with(|r| {
        r.get::<ScreenSaverHandles>().map(|h| h.is_locked.clone())
    });
    if let Some(locked) = handles {
        locked.set(true);
    } else {
        tracing::warn!("screensaver::lock called before service registered");
    }
}
```

- [ ] **Step 2: Delete `spawn_locker` and `lock_command` helpers**

Find and delete the `fn spawn_locker(...)` and `fn lock_command(...)` helpers further down in the file (around line 251 per the existing screensaver.rs scout). Drop any imports they relied on (e.g. `tokio::process::Command`, `std::process::Command`, `std::env::var`) IF they're not used elsewhere in the file. Run clippy after to surface unused-import warnings.

- [ ] **Step 3: Update the module docstring**

The top of `screensaver.rs` (lines 1-50ish) describes the gtklock binary + `TROLL_LOCK_CMD` env var. Replace those two paragraphs with:

```
//! `screensaver::lock()` flips an `is_locked: Mutable<bool>` signal;
//! `widgets::lock_screen` subscribes and mounts per-monitor layer-shell
//! lock surfaces with PAM-backed unlock. External triggers
//! (`loginctl lock-session`, `systemd-logind` Lock signal, swayidle
//! before-sleep) flow through the same signal via the login1 listen
//! loop in this module.
```

The `Inhibit` / `UnInhibit` / `Lock` D-Bus method docs and the swayidle SIGSTOP/SIGCONT pause section stay unchanged.

- [ ] **Step 4: Build + clippy**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. If clippy flags now-unused imports (e.g. `tokio::process::Command`), remove them.

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-services/src/screensaver.rs
git commit -m "$(cat <<'EOF'
feat(screensaver): lock() flips is_locked instead of spawning gtklock

Replaces the gtklock shell-out with a Mutable<bool> flip. The
v0.2.8 lock_screen widget subscribes to is_locked() and mounts
per-monitor lock surfaces in response. Drops spawn_locker,
lock_command, and the TROLL_LOCK_CMD env var read; updates the
module docstring.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `screensaver.rs` — login1 `Session.Lock`/`Unlock` listen loop

**Files:**

- Modify: `crates/hytte-services/src/screensaver.rs`

**Background:** Add a background task (alongside the existing inhibitor-server spawn in `Service::start`) that subscribes to the user's logind session and translates `Session.Lock` / `Session.Unlock` signals into flips of `is_locked`. Reconnect-on-error with 5-second backoff.

- [ ] **Step 1: Add the listen-loop spawn in `Service::start`**

Find `impl Service for ScreenSaverService` → `fn start` (around line 115). Currently it spawns one task running `run_server`. Add a second `rt.spawn(...)` block AFTER the existing one, before the `handles` return:

```rust
let locked_writer = handles.is_locked.clone();
rt.spawn(async move {
    loop {
        match listen_login1(&locked_writer).await {
            Ok(()) => tracing::warn!("login1 stream ended, retrying in 5s"),
            Err(e) => tracing::warn!(error = %e, "login1 error, retrying in 5s"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
});
```

- [ ] **Step 2: Add the `listen_login1` helper**

Below `call_login1_unlock` (added in Task 2), add:

```rust
async fn listen_login1(handles: &Mutable<bool>) -> anyhow::Result<()> {
    use anyhow::Context;
    use futures_util::StreamExt;

    let conn = Connection::system()
        .await
        .context("connect system bus for login1")?;

    let manager = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    .context("login1 Manager proxy")?;

    let pid: u32 = std::process::id();
    let session_path: zbus::zvariant::OwnedObjectPath = manager
        .call("GetSessionByPID", &(pid,))
        .await
        .context("GetSessionByPID")?;

    let session = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        session_path.as_str(),
        "org.freedesktop.login1.Session",
    )
    .await
    .context("login1 Session proxy")?;

    let mut lock_signals = session
        .receive_signal("Lock")
        .await
        .context("subscribe Session.Lock")?;
    let mut unlock_signals = session
        .receive_signal("Unlock")
        .await
        .context("subscribe Session.Unlock")?;

    loop {
        tokio::select! {
            Some(_) = lock_signals.next() => handles.set(true),
            Some(_) = unlock_signals.next() => handles.set(false),
            else => break,
        }
    }
    Ok(())
}
```

`futures_util::StreamExt` is already used elsewhere in `screensaver.rs` for the inhibitor server's `MessageStream`; the local `use` here is defensive in case the file's top-level imports don't include it. Verify with grep — if it's already imported at the top, drop the local `use`.

- [ ] **Step 3: Build + clippy**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/screensaver.rs
git commit -m "$(cat <<'EOF'
feat(screensaver): login1 Session.Lock/Unlock listen loop

Subscribes to org.freedesktop.login1.Session.Lock and Session.Unlock
signals on the user's own session (resolved via
Manager.GetSessionByPID). Flips is_locked accordingly. Reconnect-on-
error with 5s backoff matches the existing inhibitor-server task.

Now external triggers — `loginctl lock-session`, swayidle
before-sleep, `systemctl suspend` — all flow through the same
is_locked signal as the bar's Lock button.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `widgets/lock_screen.rs` — full module

**Files:**

- Create: `trollshell/src/widgets/lock_screen.rs`
- Modify: `trollshell/src/widgets/mod.rs`
- Modify: `trollshell/Cargo.toml`

**Background:** New widget module. Single big task because the surface, submit handler, and subscription hang together — splitting them leaves the file uncompilable mid-stream.

- [ ] **Step 1: Add `hytte-pam` dep to `trollshell/Cargo.toml`**

Find `[dependencies]` in `trollshell/Cargo.toml`. Add alphabetically (after `hytte` if present, before any non-hytte deps):

```toml
hytte-pam = { path = "../crates/hytte-pam" }
```

- [ ] **Step 2: Create `trollshell/src/widgets/lock_screen.rs`**

Full file contents:

```rust
//! Native trollshell lock screen.
//!
//! When `screensaver::is_locked()` emits `true`, mounts a layer-shell
//! window per monitor on `Layer::Overlay` with `KeyboardMode::Exclusive`.
//! The first installed monitor gets the password entry + clock; secondary
//! monitors get a clock-only black-cover. PAM authentication runs on a
//! `spawn_blocking` worker; on success `screensaver::handle_unlock_success()`
//! flips the signal and clears the surfaces.
//!
//! # Limitations
//!
//! - Hot-plug / monitor disconnect while locked is not handled. If the
//!   primary monitor is unplugged mid-lock, no other monitor inherits
//!   the entry. v0.3 polish.
//! - Wallpaper-blur is not implemented; the lock root has a 0.95-alpha
//!   `@window_bg_color` background which lets a small amount of
//!   wallpaper bleed through for visual continuity.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use hytte::futures_signals::signal::SignalExt;
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::{clock, screensaver};
use hytte::ui::{layer_window, Anchor, KeyboardMode, Layer, Monitor};

use hytte_pam::{authenticate, PamError, Zeroizing};

thread_local! {
    static LOCK_SURFACES: RefCell<HashMap<String, LockSurface>> =
        RefCell::new(HashMap::new());
    static SUBS_INSTALLED: Cell<bool> = const { Cell::new(false) };
}

struct LockSurface {
    window: gtk::Window,
    primary: Option<PrimaryUi>,
}

struct PrimaryUi {
    entry: gtk::PasswordEntry,
    error_label: gtk::Label,
    spinner: gtk::Spinner,
    submit_btn: gtk::Button,
    card: gtk::Box,
}

pub fn install(monitors: &[Monitor]) {
    if monitors.is_empty() {
        tracing::warn!("lock_screen::install called with no monitors");
        return;
    }

    for (idx, monitor) in monitors.iter().enumerate() {
        let connector = match monitor.connector() {
            Some(c) if !c.is_empty() => c,
            _ => continue,
        };
        let primary = idx == 0;
        let surface = build_lock_surface(monitor, primary);
        LOCK_SURFACES.with(|map| map.borrow_mut().insert(connector, surface));
    }

    if !SUBS_INSTALLED.with(Cell::get) {
        SUBS_INSTALLED.with(|c| c.set(true));
        install_lock_subscription();
    }
}

fn build_lock_surface(monitor: &Monitor, primary: bool) -> LockSurface {
    let window = layer_window(monitor)
        .layer(Layer::Overlay)
        .anchor(Anchor::Top)
        .anchor(Anchor::Bottom)
        .anchor(Anchor::Left)
        .anchor(Anchor::Right)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::Exclusive)
        .namespace("hytte-lock")
        .build();
    window.add_css_class("ts-lock-root");
    window.set_visible(false);

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.set_valign(gtk::Align::Center);
    outer.set_halign(gtk::Align::Center);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 16);
    card.add_css_class("ts-lock-card");
    card.set_halign(gtk::Align::Center);

    let clock_label = gtk::Label::new(None);
    clock_label.add_css_class("ts-lock-clock");
    clock_label.set_xalign(0.5);
    bind(
        clock::now().map(|dt| dt.format("%H:%M").to_string()),
        &clock_label,
        |w, t| w.set_text(&t),
    );
    card.append(&clock_label);

    let date_label = gtk::Label::new(None);
    date_label.add_css_class("ts-lock-date");
    date_label.set_xalign(0.5);
    bind(
        clock::now().map(|dt| dt.format("%A, %B %-d").to_string()),
        &date_label,
        |w, t| w.set_text(&t),
    );
    card.append(&date_label);

    let primary_ui = if primary {
        let user_label = gtk::Label::new(Some(&current_username()));
        user_label.add_css_class("ts-lock-user");
        user_label.set_xalign(0.5);
        card.append(&user_label);

        let entry = gtk::PasswordEntry::new();
        entry.set_show_peek_icon(false);
        entry.add_css_class("ts-lock-entry");
        entry.set_width_chars(28);
        card.append(&entry);

        let error_label = gtk::Label::new(None);
        error_label.add_css_class("ts-lock-error");
        error_label.set_xalign(0.5);
        error_label.set_visible(false);
        card.append(&error_label);

        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        card.append(&spinner);

        let submit_btn = gtk::Button::with_label("Authenticate");
        submit_btn.add_css_class("suggested-action");
        submit_btn.set_halign(gtk::Align::Center);
        card.append(&submit_btn);

        let entry_for_submit = entry.clone();
        let card_for_submit = card.clone();
        let error_for_submit = error_label.clone();
        let spinner_for_submit = spinner.clone();
        let submit_btn_for_submit = submit_btn.clone();
        let submit = move || {
            submit_password(
                &entry_for_submit,
                &card_for_submit,
                &error_for_submit,
                &spinner_for_submit,
                &submit_btn_for_submit,
            );
        };

        let submit_for_enter = submit.clone();
        entry.connect_activate(move |_| submit_for_enter());

        let submit_for_btn = submit;
        submit_btn.connect_clicked(move |_| submit_for_btn());

        Some(PrimaryUi {
            entry,
            error_label,
            spinner,
            submit_btn,
            card: card.clone(),
        })
    } else {
        None
    };

    outer.append(&card);
    window.set_child(Some(&outer));

    LockSurface {
        window,
        primary: primary_ui,
    }
}

fn submit_password(
    entry: &gtk::PasswordEntry,
    card: &gtk::Box,
    error: &gtk::Label,
    spinner: &gtk::Spinner,
    submit_btn: &gtk::Button,
) {
    let password = Zeroizing::new(entry.text().to_string());
    entry.set_text("");
    error.set_visible(false);
    spinner.set_visible(true);
    spinner.set_spinning(true);
    submit_btn.set_sensitive(false);
    entry.set_sensitive(false);

    let username = current_username();
    let entry_for_done = entry.clone();
    let card_for_done = card.clone();
    let error_for_done = error.clone();
    let spinner_for_done = spinner.clone();
    let submit_for_done = submit_btn.clone();

    glib::MainContext::default().spawn_local(async move {
        let result = tokio::task::spawn_blocking(move || {
            authenticate("trollshell", &username, password)
        })
        .await
        .unwrap_or_else(|_| Err(PamError::Service("blocking task panicked".into())));

        spinner_for_done.set_spinning(false);
        spinner_for_done.set_visible(false);
        submit_for_done.set_sensitive(true);
        entry_for_done.set_sensitive(true);
        entry_for_done.grab_focus();

        match result {
            Ok(()) => screensaver::handle_unlock_success(),
            Err(PamError::AuthFailed) => {
                show_auth_error(&error_for_done, "Incorrect password");
                shake(&card_for_done);
            }
            Err(PamError::Service(msg)) => {
                tracing::warn!(error = %msg, "PAM service error");
                show_auth_error(&error_for_done, "Authentication unavailable");
            }
        }
    });
}

fn show_auth_error(label: &gtk::Label, text: &str) {
    label.set_text(text);
    label.set_visible(true);
}

fn shake(card: &gtk::Box) {
    card.add_css_class("ts-lock-shake");
    let card_for_clear = card.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(450), move || {
        card_for_clear.remove_css_class("ts-lock-shake");
    });
}

fn current_username() -> String {
    nix::unistd::User::from_uid(nix::unistd::Uid::current())
        .ok()
        .flatten()
        .map(|u| u.name)
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "user".to_string())
}

fn install_lock_subscription() {
    glib::MainContext::default().spawn_local(
        screensaver::is_locked().for_each(|locked| {
            LOCK_SURFACES.with(|map| {
                let map = map.borrow();
                if locked {
                    for surface in map.values() {
                        surface.window.set_visible(true);
                        if let Some(p) = surface.primary.as_ref() {
                            p.error_label.set_visible(false);
                            p.entry.set_text("");
                            p.spinner.set_spinning(false);
                            p.spinner.set_visible(false);
                            p.submit_btn.set_sensitive(true);
                            p.entry.set_sensitive(true);
                            p.entry.grab_focus();
                        }
                    }
                } else {
                    for surface in map.values() {
                        if let Some(p) = surface.primary.as_ref() {
                            p.entry.set_text("");
                            p.error_label.set_visible(false);
                        }
                        surface.window.set_visible(false);
                    }
                }
            });
            std::future::ready(())
        }),
    );
}
```

- [ ] **Step 3: Wire into `widgets/mod.rs`**

In `trollshell/src/widgets/mod.rs`, add `pub mod lock_screen;` alphabetically. Looking at the existing module list in mod.rs (battery, bluetooth, brightness, clock, cpu, disk, gpu, memory, microphone, mpris, network, notif_indicator, notifications, osd, pages, polkit_dialog, power_chip, prompt, settings_chip, tray, util, volume, window_list, workspaces), `lock_screen` goes alphabetically between `gpu` and `memory`:

```rust
pub mod gpu;
pub mod lock_screen;
pub mod memory;
```

- [ ] **Step 4: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

If clippy flags `clippy::too_many_lines` on `build_lock_surface`, add `#[allow(clippy::too_many_lines)]` above it (matches existing precedent for the OSD `build_osd_view`).

- [ ] **Step 5: Commit**

```bash
git add trollshell/Cargo.toml trollshell/src/widgets/lock_screen.rs trollshell/src/widgets/mod.rs
git commit -m "$(cat <<'EOF'
feat(de): widgets/lock_screen.rs — native lock surface

Per-monitor LockSurface (Layer::Overlay, KeyboardMode::Exclusive,
all-edge anchored). Index-0 monitor is "primary" with the full UI:
clock + date (bound to clock::now()), username label, password
entry, error label, spinner, submit button. Secondary monitors are
clock-only.

Submit handler runs hytte_pam::authenticate("trollshell", ...) on
spawn_blocking. On success calls screensaver::handle_unlock_success;
on AuthFailed shakes the card and shows "Incorrect password";
other PAM errors warn-log and show generic messages.

install_lock_subscription mounts/dismisses surfaces on each
screensaver::is_locked() emission.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `main.rs` install order

**Files:**

- Modify: `trollshell/src/main.rs`

**Background:** Insert `widgets::lock_screen::install(&app.monitors())` between the primary-only widget block and the per-monitor (notifications + osd) loop. Lock surfaces should be mounted before bars / drawers so they're ready when the first lock signal arrives.

- [ ] **Step 1: Edit `main.rs`**

Find the existing block:

```rust
if let Some(primary) = app.monitors().first() {
    widgets::prompt::install(primary);
    widgets::polkit_dialog::install(primary);
}

for monitor in &app.monitors() {
    widgets::notifications::install(monitor);
    widgets::osd::install(monitor);
}
```

Insert the lock_screen install before the per-monitor loop:

```rust
if let Some(primary) = app.monitors().first() {
    widgets::prompt::install(primary);
    widgets::polkit_dialog::install(primary);
}

widgets::lock_screen::install(&app.monitors());

for monitor in &app.monitors() {
    widgets::notifications::install(monitor);
    widgets::osd::install(monitor);
}
```

- [ ] **Step 2: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add trollshell/src/main.rs
git commit -m "$(cat <<'EOF'
feat(de): install lock_screen on every monitor

widgets::lock_screen::install(&app.monitors()) lands between the
primary-only widget block and the per-monitor (notifications + osd)
loop. Lock surfaces are mounted before bars so they're ready when
the first is_locked signal arrives.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `etc/` configuration files

**Files:**

- Create: `etc/pam.d/trollshell`
- Modify: `etc/swayidle/config`
- Modify: `etc/README.md`

**Background:** Ship the PAM service file the lock UI's `authenticate("trollshell", ...)` call needs. Update swayidle to call `loginctl lock-session` instead of `gtklock`. Document the install steps.

- [ ] **Step 1: Create the PAM service file**

Create `etc/pam.d/trollshell`. File contents (one line):

```
auth include login
```

(No trailing newline-handling needed; PAM tolerates either.)

- [ ] **Step 2: Update `etc/swayidle/config`**

Find the existing config (small file). Replace `gtklock` with `loginctl lock-session` in both occurrences:

```
timeout 240 'brightnessctl -s set 10%' resume 'brightnessctl -r'
timeout 300 'loginctl lock-session'
timeout 600 'systemctl suspend'
before-sleep 'loginctl lock-session'
```

- [ ] **Step 3: Add a "PAM lock screen" section to `etc/README.md`**

Open `etc/README.md`. Find a logical insertion point (the file documents per-feature configs alphabetically). Add a new section. The section content:

````markdown
## PAM lock screen

Install the screen-unlock PAM service file:

```sh
sudo install -m 644 etc/pam.d/trollshell /etc/pam.d/trollshell
```

Without this file the lock UI mounts but authentication fails with
"Authentication unavailable" — there's no PAM service named
`trollshell` for libpam to consult.

Build-time deps: `libpam` headers (Arch `pam` package, Nix
`pkgs.pam`). Runtime deps: standard `pam_unix` stack (default on
every distro that has working login).
````

Place alphabetically among existing sections.

- [ ] **Step 4: Commit**

```bash
git add etc/pam.d/trollshell etc/swayidle/config etc/README.md
git commit -m "$(cat <<'EOF'
feat(etc): pam.d/trollshell + swayidle loginctl + README

New etc/pam.d/trollshell ships a one-line PAM service file (auth
include login) that the v0.2.8 lock screen authenticates against.

etc/swayidle/config replaces the gtklock invocations (timeout 300,
before-sleep) with `loginctl lock-session`, which fires logind's
Session.Lock signal, which the screensaver listen loop translates
into is_locked.set(true).

etc/README.md gains a "PAM lock screen" section documenting the
sudo install step and build deps.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Lock-screen CSS

**Files:**

- Modify: `trollshell/style.css`

**Background:** Append rules for the lock surfaces. All tokens already in use elsewhere in the file (`@window_bg_color`, `@card_bg_color`, `@error_color`). No new color tokens.

- [ ] **Step 1: Append rules to `trollshell/style.css`**

At the bottom of the file:

```css
/* ── Lock screen ────────────────────────────────────────────────────────── */

.ts-lock-root {
  background: alpha(@window_bg_color, 0.95);
}

.ts-lock-card {
  padding: 32px 48px;
  border-radius: 18px;
  background: alpha(@card_bg_color, 0.92);
  box-shadow: 0 8px 32px alpha(black, 0.4);
}

.ts-lock-clock {
  font-size: 4em;
  font-weight: 300;
  font-variant-numeric: tabular-nums;
  margin-bottom: -8px;
}

.ts-lock-date {
  font-size: 1.1em;
  opacity: 0.7;
  margin-bottom: 8px;
}

.ts-lock-user {
  font-size: 0.95em;
  opacity: 0.8;
  margin-top: 16px;
}

.ts-lock-entry {
  margin-top: 8px;
}

.ts-lock-error {
  color: @error_color;
  font-size: 0.9em;
  margin-top: 4px;
}

@keyframes ts-lock-shake-keyframes {
  0%,
  100% {
    margin-left: 0;
  }
  20% {
    margin-left: -8px;
  }
  40% {
    margin-left: 8px;
  }
  60% {
    margin-left: -6px;
  }
  80% {
    margin-left: 4px;
  }
}

.ts-lock-shake {
  animation: ts-lock-shake-keyframes 400ms ease-in-out;
}
```

- [ ] **Step 2: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean (CSS isn't compiled; this just confirms nothing else broke).

- [ ] **Step 3: Manual smoke test (deferred to user)**

In a Niri session: `cargo run --release -p trollshell`.

1. Press the Lock button in the Power drawer → all monitors black-cover; primary shows clock + date + username + entry.
2. Type wrong password → entry shakes, "Incorrect password" appears, field clears.
3. Type correct password → unlocks; `loginctl show-session $(loginctl --no-legend list-sessions | head -n1 | awk '{print $1}') | grep LockedHint` should show `LockedHint=no`.
4. From a terminal: `loginctl lock-session` → lock surfaces appear (login1 hook).
5. `systemctl suspend` → swayidle's before-sleep fires `loginctl lock-session` → locks before suspend; wake → lock visible.
6. 2-monitor: secondary shows clock-only; primary holds entry. Mouse can't move past either lock surface.
7. Without `/etc/pam.d/trollshell` installed: lock UI shows but auth gives "Authentication unavailable" + warn log.

- [ ] **Step 4: Commit**

```bash
git add trollshell/style.css
git commit -m "$(cat <<'EOF'
style: lock screen — Adwaita-tinted surface + shake keyframes

Appends lock-screen rules for .ts-lock-root (full-screen wallpaper-
bleed background), .ts-lock-card (rounded card with shadow),
.ts-lock-clock / .ts-lock-date / .ts-lock-user (typography),
.ts-lock-entry / .ts-lock-error, and a 400ms ts-lock-shake-keyframes
animation toggled by the Rust side on AuthFailed. All rules use
existing @window_bg_color / @card_bg_color / @error_color tokens;
no new color tokens introduced.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review notes

**Spec coverage:**

- Spec §1 hytte-pam crate → Task 1.
- Spec §2 screensaver `is_locked` + `handle_unlock_success` + `call_login1_unlock` → Task 2.
- Spec §2 `lock()` rewrite + drop gtklock paths → Task 3.
- Spec §2 login1 listen loop → Task 4.
- Spec §3 widgets/lock_screen.rs full module → Task 5.
- Spec §4 main.rs install order → Task 6.
- Spec §5 etc/pam.d/trollshell + swayidle config + README → Task 7.
- Spec §5 CSS additions → Task 8.

**Final verification:**

- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green; new smoke test in `hytte-pam::tests::api_surface_compiles`.
- Manual smoke (deferred): see Task 8 step 3.
