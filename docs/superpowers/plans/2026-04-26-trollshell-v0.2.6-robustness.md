# trollshell v0.2.6 robustness & polish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Three independent robustness/polish sub-features. Notifications mount per-monitor with focused-output routing (mirrors v0.2.3 OSD pattern). BlueZ + iwd `CMD_CONN` survive daemon restarts via reopen-on-closed. Network panel surfaces IPv4/v6 prefix lengths and replaces "No connection" expander with a placeholder row.

**Architecture:** A — Refactor `notifications.rs` to a `ToastView`-per-monitor model with module-level `route_emission` selecting the focused-output's view. B — Replace `static CMD_CONN: OnceCell<Connection>` with `static CMD_CONN: tokio::sync::Mutex<Option<Connection>>`; `cmd_conn()` checks `is_closed()` and reopens. C — `Link.addresses` becomes `Vec<LinkAddress { addr, prefix_len }>`; UI renders `192.168.1.42/24`. New `build_no_connection_placeholder_row` is mutually-exclusive with the Primary expander.

**Tech Stack:** Rust 1.94 stable, GTK4 + libadwaita, `futures-signals`, `zbus`, `tokio`. No new top-level deps.

**Conventions:**
- TDD where unit-testable (`networkd::parse_describe`, no other practical surface).
- Commits use existing prefixes: `feat(de):`, `feat(bluetooth):`, `feat(wifi):`, `feat(networkd):`, `refactor(de):`.
- Co-author trailer on every commit:
  `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`

**Spec backing this plan:** `docs/superpowers/specs/2026-04-26-v0.2.6-robustness-design.md`

---

## File Structure

**Modified files (no new files):**

- `trollshell/src/widgets/notifications.rs` — refactor to per-monitor mount with focused-output routing.
- `trollshell/src/main.rs` — move `notifications::install` from primary-only block into the per-monitor loop.
- `crates/hytte-services/src/bluetooth.rs` — replace `CMD_CONN` shape, refactor `cmd_conn()`.
- `crates/hytte-services/src/wifi.rs` — same as bluetooth.rs.
- `crates/hytte-services/src/networkd.rs` — add `LinkAddress` struct, update `parse_describe`, update existing test.
- `trollshell/src/widgets/pages.rs` — consume `LinkAddress` shape (IPv4/v6 prefix display); add `build_no_connection_placeholder_row`; hide-bind on Primary expander.

---

## Task 1: Refactor `notifications.rs` — extract `build_toast_view`

**Files:**
- Modify: `trollshell/src/widgets/notifications.rs`

**Background:** Today `install(&Monitor)` builds the layer-shell window AND wires the signal subscription in one ~180-line function. Task 2 will split signal wiring into a separate module-level function. This task extracts pure widget construction into `build_toast_view(&Monitor) -> ToastView` while keeping the subscription wiring inline. Single-monitor mount is preserved — no behavior change visible to the user.

- [ ] **Step 1: Add the `ToastView` struct definition**

In `trollshell/src/widgets/notifications.rs`, near the top of the file (after imports, before `thread_local!`):

```rust
struct ToastView {
    window: gtk::Window,
    vbox: gtk::Box,
    card_map: RefCell<HashMap<u32, gtk::Widget>>,
    overflow_card: RefCell<Option<gtk::Widget>>,
    suppressed_during_dnd: RefCell<HashSet<u32>>,
}
```

- [ ] **Step 2: Add `build_toast_view`**

Below the existing `pub fn install(monitor: &Monitor)`, add:

```rust
fn build_toast_view(monitor: &Monitor) -> ToastView {
    let window = layer_window(monitor)
        .layer(Layer::Top)
        .anchor(Anchor::Top)
        .anchor(Anchor::Right)
        .margin(Margin {
            top: 8,
            right: 8,
            bottom: 0,
            left: 0,
        })
        .namespace("hytte-toasts")
        .exclusive(false)
        .keyboard_mode(KeyboardMode::None)
        .build();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    vbox.add_css_class("ts-toasts");
    window.set_child(Some(&vbox));

    ToastView {
        window,
        vbox,
        card_map: RefCell::new(HashMap::new()),
        overflow_card: RefCell::new(None),
        suppressed_during_dnd: RefCell::new(HashSet::new()),
    }
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p trollshell`
Expected: clean. The new function is dead-code (no callers yet) but compiles. Clippy may flag it as unused; that's expected — Task 2 wires it in.

- [ ] **Step 4: Add `#[allow(dead_code)]` to silence clippy until Task 2**

Above `fn build_toast_view`:

```rust
#[allow(dead_code)] // wired in Task 2
fn build_toast_view(monitor: &Monitor) -> ToastView {
```

(`ToastView` itself is used by no callers yet either — also `#[allow(dead_code)]`.)

```rust
#[allow(dead_code)] // wired in Task 2
struct ToastView {
```

- [ ] **Step 5: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add trollshell/src/widgets/notifications.rs
git commit -m "$(cat <<'EOF'
refactor(de): notifications — extract ToastView + build_toast_view

Pure widget construction moves from the install body into a new
helper. ToastView aggregates per-window state (card_map,
overflow_card, suppressed_during_dnd) that today lives as
closure-captured RefCells. Single-monitor install behavior is
preserved; the new symbols are #[allow(dead_code)] until the
multi-monitor wiring in the next commit consumes them.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Multi-monitor notifications — install_subscriptions, route_emission, apply_emission

**Files:**
- Modify: `trollshell/src/widgets/notifications.rs`

**Background:** Replace the single `TOAST_WINDOW` thread-local with `TOAST_WINDOWS: HashMap<String, ToastView>` keyed by `Monitor.connector()`. Add `FOCUSED_OUTPUT` cell + `SUBS_INSTALLED` first-call guard. Subscriptions wire once across all `install` calls and call `route_emission`, which picks the target view by focused output and runs the existing notification-management logic against THAT view's per-window state.

- [ ] **Step 1: Replace the thread-locals**

Find the existing `thread_local!` block (around line 55). Replace:

```rust
thread_local! {
    static TOAST_WINDOW: RefCell<Option<gtk::Window>> = const { RefCell::new(None) };
}
```

with:

```rust
thread_local! {
    /// Mounted toast surfaces keyed by `Monitor.connector()`. Each
    /// entry owns its layer-shell window and the per-window state.
    static TOAST_WINDOWS: RefCell<HashMap<String, ToastView>> =
        RefCell::new(HashMap::new());

    /// Most recent focused-output name from
    /// [`hytte::services::niri::focused_output`]. Routes incoming
    /// notification batches to the matching window.
    static FOCUSED_OUTPUT: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Set after the first `install()` call so module-level
    /// subscriptions wire exactly once across all per-monitor mounts.
    static SUBS_INSTALLED: Cell<bool> = const { Cell::new(false) };
}
```

Add to imports near the top of the file:

```rust
use std::cell::Cell;
use hytte::services::niri;
```

(`std::cell::RefCell` and `std::collections::{HashMap, HashSet}` should already be imported.)

- [ ] **Step 2: Remove the `#[allow(dead_code)]` attributes from Task 1**

`ToastView` and `build_toast_view` are now consumed below. Remove the two `#[allow(dead_code)]` lines added in Task 1.

- [ ] **Step 3: Replace the `install` body**

Replace the entire body of `pub fn install(monitor: &Monitor)`:

```rust
pub fn install(monitor: &Monitor) {
    let connector = match monitor.connector() {
        Some(c) if !c.is_empty() => c,
        _ => {
            tracing::debug!("notifications::install: monitor has no connector; skipping");
            return;
        }
    };

    let view = build_toast_view(monitor);
    TOAST_WINDOWS.with(|map| map.borrow_mut().insert(connector, view));

    if !SUBS_INSTALLED.with(Cell::get) {
        SUBS_INSTALLED.with(|c| c.set(true));
        install_subscriptions();
    }
}
```

The previous `install` body's `#[allow(clippy::too_many_lines)]` attribute can be removed (the body is now small). If clippy complains anywhere else, address as it surfaces.

- [ ] **Step 4: Add `install_subscriptions`**

Below `install`:

```rust
fn install_subscriptions() {
    // niri::focused_output() → FOCUSED_OUTPUT
    glib::MainContext::default().spawn_local(
        niri::focused_output().for_each(|out| {
            FOCUSED_OUTPUT.with(|c| *c.borrow_mut() = out);
            std::future::ready(())
        }),
    );

    // Combined (notifications, dnd, muted) signal → route_emission.
    let toast_signal = map_ref! {
        let notifs = notifications::active(),
        let dnd_on = dnd::enabled(),
        let muted = notifications_mute::muted_apps() => {
            (notifs.clone(), *dnd_on, muted.clone())
        }
    };
    glib::MainContext::default().spawn_local(
        toast_signal.for_each(|(notifs, dnd_on, muted): (Vec<Notification>, bool, HashSet<String>)| {
            route_emission(&notifs, dnd_on, &muted);
            std::future::ready(())
        }),
    );
}
```

- [ ] **Step 5: Add `route_emission`**

```rust
fn route_emission(
    notifs: &[Notification],
    dnd_on: bool,
    muted: &HashSet<String>,
) {
    let target_name = FOCUSED_OUTPUT.with(|c| c.borrow().clone());
    TOAST_WINDOWS.with(|map| {
        let map = map.borrow();
        if map.is_empty() {
            return;
        }
        let view = target_name
            .as_ref()
            .and_then(|n| map.get(n))
            .or_else(|| map.values().next());
        if let Some(view) = view {
            apply_emission(view, notifs, dnd_on, muted);
        }
    });
}
```

- [ ] **Step 6: Add `apply_emission`**

This carries the existing toast-management logic but operates on a `&ToastView` instead of closure-captured RefCells. The logic itself is unchanged (DND/mute filter + suppressed-set GC + critical/non-critical partition + head/tail split + card add/remove + overflow management + window visibility). The only delta is `card_map` etc. live on `view`.

The `monitor` argument needed by the overflow-card click handler is captured via `view.window` (which is anchored on a specific monitor) — but actually `build_overflow_card(&Monitor, count)` takes a `Monitor`. To avoid threading the monitor through, the cleanest fix is: when building each `ToastView` in Task 1, capture the monitor on the view, OR pass the monitor name through. Simpler: extend `ToastView` with a `monitor: Monitor` field, populated from `build_toast_view`.

First, update `ToastView` (the struct from Task 1) to include the monitor:

```rust
struct ToastView {
    window: gtk::Window,
    vbox: gtk::Box,
    monitor: Monitor,    // NEW — used by overflow-card click handler
    card_map: RefCell<HashMap<u32, gtk::Widget>>,
    overflow_card: RefCell<Option<gtk::Widget>>,
    suppressed_during_dnd: RefCell<HashSet<u32>>,
}
```

Update `build_toast_view` to populate the field:

```rust
ToastView {
    window,
    vbox,
    monitor: monitor.clone(),
    card_map: RefCell::new(HashMap::new()),
    overflow_card: RefCell::new(None),
    suppressed_during_dnd: RefCell::new(HashSet::new()),
}
```

(`Monitor` is `Clone` per `hytte_ui::monitor.rs`.)

Now `apply_emission`:

```rust
fn apply_emission(
    view: &ToastView,
    notifs: &[Notification],
    dnd_on: bool,
    muted: &HashSet<String>,
) {
    let mut map = view.card_map.borrow_mut();
    let mut suppressed = view.suppressed_during_dnd.borrow_mut();

    let visible: Vec<&Notification> = notifs
        .iter()
        .filter(|n| {
            if n.urgency == Urgency::Critical {
                return true;
            }
            if suppressed.contains(&n.id) {
                return false;
            }
            if dnd_on || muted.contains(&n.app_name) {
                suppressed.insert(n.id);
                return false;
            }
            true
        })
        .collect();

    let active_ids: HashSet<u32> = notifs.iter().map(|n| n.id).collect();
    suppressed.retain(|id| active_ids.contains(id));

    let (critical_visible, noncritical_visible): (
        Vec<&Notification>,
        Vec<&Notification>,
    ) = visible
        .iter()
        .copied()
        .partition(|n| n.urgency == Urgency::Critical);
    let nc_head_start = noncritical_visible
        .len()
        .saturating_sub(MAX_VISIBLE_NONCRITICAL);
    let head_noncritical = &noncritical_visible[nc_head_start..];
    let tail_noncritical_count = nc_head_start;

    let new_ids: HashMap<u32, &Notification> = critical_visible
        .iter()
        .copied()
        .chain(head_noncritical.iter().copied())
        .map(|n| (n.id, n))
        .collect();
    let old_ids: Vec<u32> = map.keys().copied().collect();

    for id in &old_ids {
        if !new_ids.contains_key(id) && let Some(card) = map.remove(id) {
            view.vbox.remove(&card);
        }
    }

    for (id, notif) in &new_ids {
        if let Some(old_card) = map.remove(id) {
            view.vbox.remove(&old_card);
        }
        let card = build_card(notif);
        view.vbox.append(&card);
        map.insert(*id, card);
    }

    {
        let mut slot = view.overflow_card.borrow_mut();
        if tail_noncritical_count == 0 {
            if let Some(card) = slot.take() {
                view.vbox.remove(&card);
            }
        } else {
            if let Some(card) = slot.take() {
                view.vbox.remove(&card);
            }
            let card = build_overflow_card(&view.monitor, tail_noncritical_count);
            view.vbox.append(&card);
            *slot = Some(card);
        }
    }

    view.window.set_visible(!map.is_empty() || view.overflow_card.borrow().is_some());
}
```

- [ ] **Step 7: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. If clippy flags anything (e.g. `manual_let_else`, `single_match_else`), fix as it surfaces.

- [ ] **Step 8: Commit**

```bash
git add trollshell/src/widgets/notifications.rs
git commit -m "$(cat <<'EOF'
feat(de): notifications — multi-monitor mount + focused-output routing

Replaces the single TOAST_WINDOW thread-local with TOAST_WINDOWS
keyed by Monitor.connector(). Subscriptions (notifications + dnd +
muted, niri::focused_output) move to install_subscriptions() and
run exactly once across all per-monitor mounts. route_emission()
picks the toast view on the focused output, falls back to the first
mounted view when the focused output is unknown or missing from
the map. apply_emission carries the existing card-management logic
(DND/mute filter, suppressed-set GC, critical/non-critical partition,
+N more overflow) but operates on a per-monitor ToastView.

main.rs continues to install on the primary monitor only; the loop
swap lands next.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `main.rs` — install notifications per monitor

**Files:**
- Modify: `trollshell/src/main.rs`

**Background:** Move `widgets::notifications::install` from the primary-only block into the existing per-monitor loop alongside `osd::install`. `prompt` and `polkit_dialog` stay primary-only.

- [ ] **Step 1: Edit `main.rs`**

Find the block (around lines 73-82):

```rust
if let Some(primary) = app.monitors().first() {
    widgets::notifications::install(primary);
    widgets::prompt::install(primary);
    widgets::polkit_dialog::install(primary);
}

// OSD mounts on every monitor; routing picks the focused one.
for monitor in &app.monitors() {
    widgets::osd::install(monitor);
}
```

Replace with:

```rust
if let Some(primary) = app.monitors().first() {
    widgets::prompt::install(primary);
    widgets::polkit_dialog::install(primary);
}

// Notifications + OSD mount on every monitor; routing picks the focused one.
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
feat(de): install notifications on every monitor

Notifications mount on each monitor; route_emission inside the
widget picks the focused-output's instance. Prompt and
polkit_dialog stay primary-only — security-gate semantics that
benefit from a fixed location.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: BlueZ `CMD_CONN` reconnect

**Files:**
- Modify: `crates/hytte-services/src/bluetooth.rs`

**Background:** Replace `static CMD_CONN: OnceCell<Connection>` (set-once) with `static CMD_CONN: tokio::sync::Mutex<Option<Connection>>`. `cmd_conn()` returns `Result<Connection>` (owned Arc-clone) instead of `&'static Connection`. On each call, lock + check `is_closed()`, reopen if dead.

- [ ] **Step 1: Update imports**

In `crates/hytte-services/src/bluetooth.rs`, find the existing `use tokio::sync::OnceCell;` import. Replace with:

```rust
use tokio::sync::Mutex;
```

(If `OnceCell` is used elsewhere in the file, leave that import; only remove if `cmd_conn` is its sole consumer. Verify with grep before removing.)

- [ ] **Step 2: Replace the static + accessor**

Find the existing `cmd_conn` block (around line 511-525):

```rust
/// Shared command-channel connection. `BlueZ` owns sessions (e.g. for
/// `StartDiscovery`) per bus client; using a fresh connection per call
/// breaks Start/Stop pairing because `BlueZ` sees them as different
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

Replace with:

```rust
/// Shared command-channel connection. `BlueZ` owns sessions (e.g. for
/// `StartDiscovery`) per bus client; using a fresh connection per call
/// breaks Start/Stop pairing because `BlueZ` sees them as different
/// clients. Lazily opened on first call; auto-reopens on
/// `is_closed()` so trollshell survives `systemctl restart bluetooth`
/// without itself restarting.
static CMD_CONN: Mutex<Option<Connection>> = Mutex::const_new(None);

async fn cmd_conn() -> Result<Connection> {
    let mut guard = CMD_CONN.lock().await;
    if guard.as_ref().is_none_or(zbus::Connection::is_closed) {
        let fresh = Connection::system()
            .await
            .context("open shared bluetooth command connection")?;
        *guard = Some(fresh);
    }
    Ok(guard
        .as_ref()
        .expect("just stored Some")
        .clone())
}
```

- [ ] **Step 3: Verify call sites compile unchanged**

The existing `do_*` helpers (around lines 529-600) all have the form:

```rust
async fn do_X(...) -> Result<()> {
    let conn = cmd_conn().await?;
    conn.call_method(...).await?;
    Ok(())
}
```

`conn` was `&'static Connection`; now it's `Connection` (owned). Both deref to the same `call_method` API. **No call-site changes needed** — Rust's auto-ref handles `conn.call_method(...)` either way.

Run: `cargo build -p hytte-services`
Expected: clean.

- [ ] **Step 4: Build + clippy + tests**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p hytte-services`
Expected: clean + green.

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-services/src/bluetooth.rs
git commit -m "$(cat <<'EOF'
fix(bluetooth): CMD_CONN auto-reopens on closed peer

Replaces the OnceCell<Connection> with Mutex<Option<Connection>>.
cmd_conn() now checks is_closed() on every call and reopens the
connection if the peer is gone — survives systemctl restart bluetooth
without trollshell itself restarting. Mid-call failures still error
once with ConnectionClosed (user retries the click); the next call
sees the closed cell and reopens.

Listen loops untouched (already retry with backoff).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Wi-Fi `CMD_CONN` reconnect

**Files:**
- Modify: `crates/hytte-services/src/wifi.rs`

**Background:** Mirror of Task 4 in `wifi.rs`. Same pattern, only the error-context string changes.

- [ ] **Step 1: Update imports**

In `crates/hytte-services/src/wifi.rs`, find `use tokio::sync::OnceCell;` and replace with `use tokio::sync::Mutex;` (or add Mutex alongside if OnceCell is used elsewhere — verify with grep).

- [ ] **Step 2: Replace the static + accessor**

Find the existing `cmd_conn` block (around line 414-426):

```rust
static CMD_CONN: OnceCell<Connection> = OnceCell::const_new();

async fn cmd_conn() -> Result<&'static Connection> {
    CMD_CONN
        .get_or_try_init(|| async {
            Connection::system()
                .await
                .context("open shared wifi command connection")
        })
        .await
}
```

Replace with:

```rust
/// Shared command-channel connection. Avoids opening a fresh system
/// bus connection on every iwd call. Lazily opened on first call;
/// auto-reopens on `is_closed()` so trollshell survives
/// `systemctl restart iwd` without itself restarting.
static CMD_CONN: Mutex<Option<Connection>> = Mutex::const_new(None);

async fn cmd_conn() -> Result<Connection> {
    let mut guard = CMD_CONN.lock().await;
    if guard.as_ref().is_none_or(zbus::Connection::is_closed) {
        let fresh = Connection::system()
            .await
            .context("open shared wifi command connection")?;
        *guard = Some(fresh);
    }
    Ok(guard
        .as_ref()
        .expect("just stored Some")
        .clone())
}
```

- [ ] **Step 3: Build + clippy + tests**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p hytte-services`
Expected: clean + green.

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/wifi.rs
git commit -m "$(cat <<'EOF'
fix(wifi): CMD_CONN auto-reopens on closed peer

Mirror of the bluetooth.rs reconnect fix. Replaces OnceCell<Connection>
with Mutex<Option<Connection>>. cmd_conn() checks is_closed() on
every call and reopens — survives systemctl restart iwd without
trollshell restarting. Listen loop untouched (already retries with
backoff).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `networkd::Link.addresses` → `Vec<LinkAddress>`

**Files:**
- Modify: `crates/hytte-services/src/networkd.rs`

**Background:** `Link.addresses: Vec<IpAddr>` discards prefix length. The `parse_describe` parser already extracts `prefix_length` from each `DescribeAddress` but throws it away. Add a `LinkAddress { addr, prefix_len }` struct, update the parser, update the existing `parses_describe_json_minimal` test.

- [ ] **Step 1: Add the `LinkAddress` struct**

In `crates/hytte-services/src/networkd.rs`, add near the existing `RouteSummary` struct definition:

```rust
#[derive(Clone, Debug)]
pub struct LinkAddress {
    pub addr: IpAddr,
    pub prefix_len: u8,
}
```

- [ ] **Step 2: Update `Link`**

Find the `Link` struct definition. Change the `addresses` field type:

```rust
#[derive(Clone, Debug, Default)]
pub struct Link {
    pub idx: i32,
    pub name: String,
    pub operational: OperationalState,
    pub addresses: Vec<LinkAddress>,        // was Vec<IpAddr>
    pub gateway_v4: Option<Ipv4Addr>,
    pub gateway_v6: Option<Ipv6Addr>,
    pub routes: Vec<RouteSummary>,
}
```

- [ ] **Step 3: Update `ParsedDescribe`**

Find `ParsedDescribe`:

```rust
#[derive(Debug, Default)]
pub(crate) struct ParsedDescribe {
    pub addresses: Vec<LinkAddress>,        // was Vec<IpAddr>
    pub gateway_v4: Option<Ipv4Addr>,
    pub gateway_v6: Option<Ipv6Addr>,
    pub routes: Vec<RouteSummary>,
}
```

- [ ] **Step 4: Update `parse_describe`**

Find the existing addresses-population loop in `parse_describe`. Replace:

```rust
for a in raw.addresses {
    if let Some(ip) = bytes_to_ip(a.family, &a.address) {
        out.addresses.push(ip);
    }
}
```

with:

```rust
for a in raw.addresses {
    if let Some(addr) = bytes_to_ip(a.family, &a.address) {
        out.addresses.push(LinkAddress {
            addr,
            prefix_len: a.prefix_length,
        });
    }
}
```

- [ ] **Step 5: Update `parses_describe_json_minimal` test**

Find the existing test:

```rust
#[test]
fn parses_describe_json_minimal() {
    let parsed = parse_describe(SAMPLE_DESCRIBE).expect("parse");
    assert_eq!(parsed.addresses, vec![IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 42))]);
    assert_eq!(parsed.gateway_v4, Some(std::net::Ipv4Addr::new(192, 168, 1, 1)));
    assert_eq!(parsed.gateway_v6, None);
    assert_eq!(parsed.routes.len(), 2);
}
```

Replace the `assert_eq!(parsed.addresses, ...)` line with:

```rust
assert_eq!(parsed.addresses.len(), 1);
assert_eq!(parsed.addresses[0].addr, IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 42)));
assert_eq!(parsed.addresses[0].prefix_len, 24);
```

(The other test assertions about gateway and routes are unchanged.)

- [ ] **Step 6: Run tests**

Run: `cargo test -p hytte-services networkd::tests -- --nocapture`
Expected: 3 passed (parses_describe_json_minimal, handles_unknown_fields, default_route_populates_gateway_v4).

- [ ] **Step 7: Wire `read_links` to populate the new shape**

Find `read_links` (around line 126) where the `Link { ... }` constructor populates fields. Verify the line `addresses: parsed.addresses,` now compiles (since `parsed.addresses: Vec<LinkAddress>`). No code change needed at this constructor — it just inherits the new shape. Run cargo build to confirm.

Run: `cargo build -p hytte-services`
Expected: clean. If `pages.rs` has consumers that break (it does — Task 7 fixes those), the workspace build will fail. Run only `cargo build -p hytte-services` for now to confirm the service-side change compiles.

- [ ] **Step 8: Commit (workspace build is intentionally still broken; Task 7 fixes pages.rs)**

```bash
git add crates/hytte-services/src/networkd.rs
git commit -m "$(cat <<'EOF'
feat(networkd): expose IPv4/IPv6 prefix length on Link.addresses

Link.addresses is now Vec<LinkAddress { addr, prefix_len }>; the
prefix_length field was already being parsed from networkd's
Describe() JSON but discarded. Surfacing it lets the network panel
render addresses as 192.168.1.42/24 instead of bare IPs.

This commit changes the public API of Link.addresses; pages.rs
consumers are updated in the next commit (workspace build is
intentionally broken between these two commits if reviewed in
isolation).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `pages.rs` — render IPv4/v6 prefix length

**Files:**
- Modify: `trollshell/src/widgets/pages.rs`

**Background:** Consume the new `LinkAddress` shape in the IPv4 and IPv6 address rows of `build_primary_expander`. Render `192.168.1.42/24` instead of `192.168.1.42`. The `(+N more)` collapse and link-local filter for IPv6 stay in place.

- [ ] **Step 1: Update IPv4 address row mapping**

In `trollshell/src/widgets/pages.rs::build_primary_expander`, find the IPv4 address row's signal map (the closure inside `bind(networkd::primary().map(...), &v4_addr_row, ...)`). The current shape:

```rust
networkd::primary().map(|p| match p {
    Some(link) => link
        .addresses
        .iter()
        .filter_map(|ip| match ip {
            std::net::IpAddr::V4(v) => Some(v.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(", "),
    None => String::new(),
})
```

Change `|ip|` to `|a|` and use `a.addr` + `a.prefix_len`:

```rust
networkd::primary().map(|p| match p {
    Some(link) => link
        .addresses
        .iter()
        .filter_map(|a| match a.addr {
            std::net::IpAddr::V4(v) => Some(format!("{v}/{}", a.prefix_len)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(", "),
    None => String::new(),
})
```

- [ ] **Step 2: Update IPv6 address row mapping**

Find the IPv6 row's signal map. Existing shape (with link-local filter + multi-address collapse):

```rust
networkd::primary().map(|p| match p {
    Some(link) => {
        let v6: Vec<String> = link
            .addresses
            .iter()
            .filter_map(|ip| match ip {
                std::net::IpAddr::V6(v) if !v.is_unicast_link_local() => Some(v.to_string()),
                _ => None,
            })
            .collect();
        if v6.is_empty() {
            String::new()
        } else if v6.len() == 1 {
            v6[0].clone()
        } else {
            format!("{} (+{} more)", v6[0], v6.len() - 1)
        }
    }
    None => String::new(),
})
```

Change to consume `LinkAddress`:

```rust
networkd::primary().map(|p| match p {
    Some(link) => {
        let v6: Vec<String> = link
            .addresses
            .iter()
            .filter_map(|a| match a.addr {
                std::net::IpAddr::V6(v) if !v.is_unicast_link_local() => {
                    Some(format!("{v}/{}", a.prefix_len))
                }
                _ => None,
            })
            .collect();
        if v6.is_empty() {
            String::new()
        } else if v6.len() == 1 {
            v6[0].clone()
        } else {
            format!("{} (+{} more)", v6[0], v6.len() - 1)
        }
    }
    None => String::new(),
})
```

- [ ] **Step 3: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. Workspace build is now whole again after Task 6's intentional break.

- [ ] **Step 4: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(de): network — IPv4/v6 address rows show prefix length

Consumes the v0.2.6 LinkAddress shape (addr + prefix_len). The
Primary expander's IPv4 row renders e.g. "192.168.1.42/24" and the
IPv6 row renders "2001:db8::1/64" (with the existing link-local
filter and multi-address (+N more) collapse).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: `pages.rs` — "No connection" hide-and-replace

**Files:**
- Modify: `trollshell/src/widgets/pages.rs`

**Background:** Today the Primary expander sets its title to "No connection" via the title bind's `None` arm. Reviewer flag (v0.2.2): hide the expander entirely and show a separate non-activatable placeholder row instead.

- [ ] **Step 1: Strip the None-fallback in the Primary expander's title bind**

In `build_primary_expander`, find the title bind:

```rust
bind(
    networkd::primary().map(|p| match p {
        Some(link) => link.name,
        None => "No connection".to_string(),
    }),
    &expander,
    |w, name| w.set_title(&name),
);
```

Replace with:

```rust
bind(
    networkd::primary().map(|p| p.map_or(String::new(), |link| link.name)),
    &expander,
    |w, name| w.set_title(&name),
);
```

(When `primary` is None the title is empty — which doesn't matter because the next step hides the expander.)

- [ ] **Step 2: Add the visibility bind to the Primary expander**

Below the existing title and subtitle binds inside `build_primary_expander`, add:

```rust
bind(
    networkd::primary().map(|p| p.is_some()),
    &expander,
    gtk::prelude::WidgetExt::set_visible,
);
```

- [ ] **Step 3: Add `build_no_connection_placeholder_row`**

Below `build_primary_expander` (or in any consistent location in `pages.rs`):

```rust
fn build_no_connection_placeholder_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("No connection")
        .activatable(false)
        .selectable(false)
        .build();
    row.set_subtitle("No primary network link");
    bind(
        networkd::primary().map(|p| p.is_none()),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );
    row
}
```

- [ ] **Step 4: Wire into `build_connection_group_v2`**

Find `build_connection_group_v2`. Currently:

```rust
group.add(&build_primary_expander());
group.add(&build_all_links_expander());
group.add(&build_dns_expander());
```

Insert the placeholder row between Primary and All-links:

```rust
group.add(&build_primary_expander());
group.add(&build_no_connection_placeholder_row());
group.add(&build_all_links_expander());
group.add(&build_dns_expander());
```

- [ ] **Step 5: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Manual smoke test (deferred to user)**

In a Niri session: `cargo run --release -p trollshell`. Open the Network drawer.

- Connected: Primary expander visible with link name; no placeholder row.
- Disconnect (e.g. `nmcli c down <name>` or `iwctl station <iface> disconnect`): Primary expander disappears; "No connection" row appears.
- Reconnect: expander reappears; placeholder hides.

- [ ] **Step 7: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
polish(de): network — hide Primary expander when no connection

Replaces the title-fallback "No connection" trick with an explicit
hide-and-replace: the Primary expander gains a set_visible bind on
primary().is_some(), and a new sibling AdwActionRow titled
"No connection" (subtitle "No primary network link") is mutually
visible only when primary is None. Closes the v0.2.2 reviewer flag.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review notes

**Spec coverage:**
- Spec §1 multi-monitor notifications → Tasks 1, 2, 3.
- Spec §2 service reconnect → Tasks 4, 5.
- Spec §3a IPv4/v6 prefix length → Tasks 6, 7.
- Spec §3b "No connection" hide-and-replace → Task 8.

**Final verification:**
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green (one updated test in `networkd::tests::parses_describe_json_minimal`).
- Manual smoke tests deferred per success criteria.
