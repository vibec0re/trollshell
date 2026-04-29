# Network Panel Tightening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the inline "Active connections" wall to a dedicated `Page::Connections` drill-down so the network drawer stays focused, and switch the per-interface traffic rows from `adw::ActionRow + suffix` to the wider `build_history_row(name)` pattern used by `page_stats`.

**Architecture:** Pure UI relocation. No service-layer touches; both `vpn` and `netconn` keep their existing signal contracts. The active-connections logic is lifted verbatim from `page_network` into a new `page_connections()` builder; `page_network` gets a single drill-down `adw::ActionRow` that subscribes to the same `netconn::connections()` signal for its subtitle. Per-interface traffic rows replace their AdwActionRow-with-sparkline-in-suffix shape with the existing `build_history_row(name) -> (gtk::Box, Sparkline, gtk::Label)` helper, which gives the sparkline the row's full hexpand width.

**Tech Stack:** Rust 1.94, GTK4 + libadwaita, `futures-signals`, existing `hytte::services::netconn` and `hytte::services::sensors` signals.

---

## File Structure

| File                                   | Responsibility |
| -------------------------------------- | -------------- |
| `trollshell/src/modal.rs`              | Add `Page::Connections` variant + `stack_name` arm + `add_named` mount alongside the other pages. |
| `trollshell/src/widgets/pages.rs`      | Add `pub fn page_connections()` (active-connections content lifted verbatim). Strip the same content from `page_network()` and replace with a drill-down `ActionRow`. Refactor `build_iface_traffic_row` + `IfaceRow` struct to use `build_history_row`. |

Spec reference: `/home/choom/src/trollshell/docs/superpowers/specs/2026-04-29-network-panel-tightening-design.md`.

---

## Task 1: Move "Active connections" to `Page::Connections` drill-down

**Files:**
- Modify: `trollshell/src/modal.rs` (Page enum + stack_name + install stack mount)
- Modify: `trollshell/src/widgets/pages.rs` (new `page_connections()`; strip + replace in `page_network()`)

- [ ] **Step 1: Add `Page::Connections` variant**

Open `trollshell/src/modal.rs`. Find the `Page` enum (around line 14). Add `Connections` immediately after `Vpn`:

```rust
pub enum Page {
    Media,
    Network,
    Vpn,
    Connections,
    Bluetooth,
    Stats,
    Audio,
    Power,
    PowerMenu,
    Notifications,
    Appearance,
    Displays,
    Clipboard,
    Calendar,
    Settings,
}
```

- [ ] **Step 2: Add `stack_name` arm**

In the same file, find the `stack_name` impl. Add the `Connections => "connections"` arm after the `Vpn` arm:

```rust
impl Page {
    fn stack_name(self) -> &'static str {
        match self {
            Self::Media => "media",
            Self::Network => "network",
            Self::Vpn => "vpn",
            Self::Connections => "connections",
            Self::Bluetooth => "bluetooth",
            Self::Stats => "stats",
            Self::Audio => "audio",
            Self::Power => "power",
            Self::PowerMenu => "power-menu",
            Self::Notifications => "notifications",
            Self::Appearance => "appearance",
            Self::Displays => "displays",
            Self::Clipboard => "clipboard",
            Self::Calendar => "calendar",
            Self::Settings => "settings",
        }
    }
}
```

- [ ] **Step 3: Mount in modal stack**

In the same file, find `install()`. Around line 167 you'll see existing `add_named` lines. Insert after the `Vpn` line:

```rust
    stack.add_named(&pages::page_vpn(), Some(Page::Vpn.stack_name()));
    stack.add_named(
        &pages::page_connections(),
        Some(Page::Connections.stack_name()),
    );
    stack.add_named(&pages::page_bluetooth(), Some(Page::Bluetooth.stack_name()));
```

(`page_connections` doesn't exist yet — the build will fail until Step 4. That's fine; we land everything in one commit.)

- [ ] **Step 4: Add `pub fn page_connections()` to pages.rs**

Open `trollshell/src/widgets/pages.rs`. Find the `// ── VPN page ──` separator (above `pub fn page_vpn`). Insert the new page builder immediately above it (so the page-builder block is alphabetical-ish: connections, vpn, calendar, …). Use this body — it's the active-connections logic lifted verbatim from `page_network()`:

```rust
// ── Connections page ──────────────────────────────────────────────────────────

/// Drawer drill-down listing per-process active sockets.
///
/// Surfaces every entry of `hytte::services::netconn::connections()`. Own-user
/// sockets (PID present) appear at top sorted by program name; other-user
/// sockets (where ss can't see PID) collapse into a single "Other users"
/// expander at the bottom. Each bucket is capped at `CONN_BUCKET_CAP` rows;
/// truncation is surfaced via a "(+N more)" hint row in the own bucket and a
/// "{shown} of {total} sockets" subtitle on the other-users expander.
pub fn page_connections() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    let conn_group = adw::PreferencesGroup::builder()
        .title("Active connections")
        .build();
    bind(
        netconn::connections().map(|cs| {
            let total = cs.len();
            let with_pid = cs.iter().filter(|c| c.pid.is_some()).count();
            format!("{total} sockets, {with_pid} with PID")
        }),
        &conn_group,
        |g, txt| g.set_description(Some(&txt)),
    );

    // Top-level rows: own-user sockets sorted by program. Other users
    // (where ss can't see PID) collapse into a single expander at the
    // bottom so they don't dominate.
    let owned_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let owned_overflow_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));
    let other_expander = adw::ExpanderRow::builder().title("Other users").build();
    let other_rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    let group_for_bind = conn_group.clone();
    let owned_for_bind = owned_track.clone();
    let overflow_for_bind = owned_overflow_track.clone();
    let other_for_bind = other_expander.clone();
    let other_rows_for_bind = other_rows_track.clone();
    bind(
        netconn::connections(),
        &conn_group,
        move |_g, mut conns| {
            conns.sort_by(|a, b| match (a.pid.is_some(), b.pid.is_some()) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (true, true) => a
                    .program
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.program.as_deref().unwrap_or("")),
                (false, false) => a.local.to_string().cmp(&b.local.to_string()),
            });

            let total_owned = conns.iter().filter(|c| c.pid.is_some()).count();
            let total_other = conns.len() - total_owned;

            let mut owned = owned_for_bind.borrow_mut();
            for r in owned.drain(..) {
                group_for_bind.remove(&r);
            }
            let mut others = other_rows_for_bind.borrow_mut();
            for r in others.drain(..) {
                other_for_bind.remove(&r);
            }
            if let Some(prev) = overflow_for_bind.borrow_mut().take() {
                group_for_bind.remove(&prev);
            }

            let mut owned_count = 0usize;
            let mut other_count = 0usize;
            for c in &conns {
                if c.pid.is_some() {
                    if owned_count >= CONN_BUCKET_CAP {
                        continue;
                    }
                    let row = build_connection_row(c);
                    group_for_bind.add(&row);
                    owned.push(row);
                    owned_count += 1;
                } else {
                    if other_count >= CONN_BUCKET_CAP {
                        continue;
                    }
                    let row = build_connection_row(c);
                    other_for_bind.add_row(&row);
                    others.push(row);
                    other_count += 1;
                }
            }

            if total_owned > owned_count {
                let hint = adw::ActionRow::builder()
                    .title(format!("(+{} more)", total_owned - owned_count))
                    .activatable(false)
                    .selectable(false)
                    .build();
                hint.set_subtitle("Top sockets shown.");
                group_for_bind.add(&hint);
                *overflow_for_bind.borrow_mut() = Some(hint);
            }

            if total_other > other_count {
                other_for_bind.set_subtitle(&format!("{other_count} of {total_other} sockets"));
            } else {
                other_for_bind.set_subtitle(&format!("{other_count} sockets"));
            }
            other_for_bind.set_visible(total_other > 0);
        },
    );
    conn_group.add(&other_expander);
    column.append(&conn_group);

    finish_page(&column)
}
```

(`CONN_BUCKET_CAP` and `build_connection_row` already exist at file scope from the prior network panel work — they don't move.)

- [ ] **Step 5: Strip the active-connections block from `page_network()`**

In `trollshell/src/widgets/pages.rs`, find `pub fn page_network()` (around line 313). Locate this block (currently around lines 344-450, between `outer.append(&grid);` and `finish_page(&outer)`):

```rust
    // Active connections — full-width section below the grid.
    let conn_group = adw::PreferencesGroup::builder()
        .title("Active connections")
        .build();
    bind(
        netconn::connections().map(|cs| {
            let total = cs.len();
            let with_pid = cs.iter().filter(|c| c.pid.is_some()).count();
            format!("{total} sockets, {with_pid} with PID")
        }),
        &conn_group,
        |g, txt| g.set_description(Some(&txt)),
    );

    // Top-level rows: own-user sockets sorted by program. Other users
    // (where ss can't see PID) collapse into a single expander at the
    // bottom so they don't dominate.
    let owned_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let owned_overflow_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));
    let other_expander = adw::ExpanderRow::builder().title("Other users").build();
    let other_rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    let group_for_bind = conn_group.clone();
    let owned_for_bind = owned_track.clone();
    let overflow_for_bind = owned_overflow_track.clone();
    let other_for_bind = other_expander.clone();
    let other_rows_for_bind = other_rows_track.clone();
    bind(
        netconn::connections(),
        &conn_group,
        move |_g, mut conns| {
            // Sort: own-user (has PID) first by program, then no-PID by local addr.
            conns.sort_by(|a, b| match (a.pid.is_some(), b.pid.is_some()) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (true, true) => a
                    .program
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.program.as_deref().unwrap_or("")),
                (false, false) => a.local.to_string().cmp(&b.local.to_string()),
            });

            // Raw bucket totals, used so the truncation hint and expander
            // subtitle can reflect what's actually there (not just what's shown).
            let total_owned = conns.iter().filter(|c| c.pid.is_some()).count();
            let total_other = conns.len() - total_owned;

            // Drain previous rows.
            let mut owned = owned_for_bind.borrow_mut();
            for r in owned.drain(..) {
                group_for_bind.remove(&r);
            }
            let mut others = other_rows_for_bind.borrow_mut();
            for r in others.drain(..) {
                other_for_bind.remove(&r);
            }
            // Drain any previous overflow hint row.
            if let Some(prev) = overflow_for_bind.borrow_mut().take() {
                group_for_bind.remove(&prev);
            }

            let mut owned_count = 0usize;
            let mut other_count = 0usize;
            for c in &conns {
                if c.pid.is_some() {
                    if owned_count >= CONN_BUCKET_CAP {
                        continue;
                    }
                    let row = build_connection_row(c);
                    group_for_bind.add(&row);
                    owned.push(row);
                    owned_count += 1;
                } else {
                    if other_count >= CONN_BUCKET_CAP {
                        continue;
                    }
                    let row = build_connection_row(c);
                    other_for_bind.add_row(&row);
                    others.push(row);
                    other_count += 1;
                }
            }

            // Owner-bucket truncation hint.
            if total_owned > owned_count {
                let hint = adw::ActionRow::builder()
                    .title(format!("(+{} more)", total_owned - owned_count))
                    .activatable(false)
                    .selectable(false)
                    .build();
                hint.set_subtitle("Top sockets shown.");
                group_for_bind.add(&hint);
                *overflow_for_bind.borrow_mut() = Some(hint);
            }

            // Other-bucket subtitle: show capped vs total when truncated.
            if total_other > other_count {
                other_for_bind.set_subtitle(&format!("{other_count} of {total_other} sockets"));
            } else {
                other_for_bind.set_subtitle(&format!("{other_count} sockets"));
            }
            other_for_bind.set_visible(total_other > 0);
        },
    );
    conn_group.add(&other_expander);
    outer.append(&conn_group);
```

Replace it with this drill-down stub:

```rust
    // Active connections drill-down — opens Page::Connections for the full list.
    let drill = adw::ActionRow::builder()
        .title("Active connections")
        .activatable(true)
        .build();
    drill.add_prefix(&gtk::Image::from_icon_name("system-search-symbolic"));
    drill.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    bind(
        netconn::connections().map(|cs| {
            let total = cs.len();
            let with_pid = cs.iter().filter(|c| c.pid.is_some()).count();
            format!("{total} sockets, {with_pid} with PID")
        }),
        &drill,
        |row, txt| row.set_subtitle(&txt),
    );
    drill.connect_activated(|_| {
        crate::modal::switch_active(crate::modal::Page::Connections);
    });
    let drill_group = adw::PreferencesGroup::new();
    drill_group.add(&drill);
    outer.append(&drill_group);
```

The `outer.append(&grid);` line above it stays. The `finish_page(&outer)` line below it stays.

- [ ] **Step 6: Build the workspace**

Run: `cargo build --workspace --message-format=short 2>&1 | tail -10`
Expected: clean. The newly-added `page_connections()` is referenced from `modal.rs`; the strip from `page_network()` removes the inline content. No callers reference removed symbols (everything that was used — `CONN_BUCKET_CAP`, `build_connection_row` — still exists at file scope and is now used by `page_connections`).

- [ ] **Step 7: Run clippy on trollshell**

Run: `cargo clippy -p trollshell --message-format=short 2>&1 | grep -E '(pages.rs|modal.rs)'`
Expected: no new warnings on either file. (Pre-existing `mpris.rs` warning may still appear; unrelated.)

- [ ] **Step 8: Run workspace tests**

Run: `cargo test --workspace --message-format=short 2>&1 | grep -E '(test result|FAILED)' | head -20`
Expected: every line is `test result: ok.`. No `FAILED`.

- [ ] **Step 9: Commit**

```bash
git add trollshell/src/modal.rs trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
refactor(network-page): move Active connections to Page::Connections drilldown

Inline section was making the network drawer too dense (60+ rows of
socket text past the visible drawer height on a typical monitor).
New Page::Connections hosts the relocated content verbatim — same
sort, same per-bucket cap, same truncation hint. page_network()
keeps a single drill-down row whose subtitle stays bound to
netconn::connections() so the count stays live without expanding.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Per-interface traffic rows use `build_history_row`

**Files:**
- Modify: `trollshell/src/widgets/pages.rs` — `IfaceRow` struct and `build_iface_traffic_row()` (around lines 826-851), plus the bind closure callsites in `build_traffic_group_v2()` (around lines 741-784).

- [ ] **Step 1: Update `IfaceRow` struct**

Open `trollshell/src/widgets/pages.rs`. Find the `IfaceRow` struct (around line 829):

```rust
/// Per-interface traffic row holding the widgets the bind updates each
/// `sensors::network()` emission. Returned by `build_iface_traffic_row`
/// and stored in the network drawer's interface cache.
struct IfaceRow {
    row: adw::ActionRow,
    spark: Sparkline,
    value: gtk::Label,
}
```

Replace with:

```rust
/// Per-interface traffic row holding the widgets the bind updates each
/// `sensors::network()` emission. Returned by `build_iface_traffic_row`
/// and stored in the network drawer's interface cache.
///
/// `container` is a plain `gtk::Box` matching the `build_history_row`
/// shape used by `page_stats` — name on the left, sparkline taking the
/// row's full hexpand, value on the right. `adw::PreferencesGroup::add`
/// accepts the box as a child, same as `build_stats_history_group` does.
struct IfaceRow {
    container: gtk::Box,
    spark: Sparkline,
    value: gtk::Label,
}
```

- [ ] **Step 2: Refactor `build_iface_traffic_row`**

Find the function (around line 839):

```rust
/// One per-interface traffic row: name on the left, sparkline center,
/// current ↓rx ↑tx label on the right. Returned widgets are stored by
/// the caller so subsequent emissions can `spark.push(...)` and
/// `value.set_text(...)` instead of rebuilding the row.
fn build_iface_traffic_row(iface: &sensors::NetInterface) -> IfaceRow {
    let row = adw::ActionRow::builder().title(&iface.name).build();
    let suffix_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    suffix_box.set_valign(gtk::Align::Center);
    let spark = Sparkline::new(60);
    spark.widget().set_width_request(120);
    suffix_box.append(spark.widget());
    let value = gtk::Label::new(None);
    value.add_css_class("ts-mono");
    suffix_box.append(&value);
    row.add_suffix(&suffix_box);
    IfaceRow { row, spark, value }
}
```

Replace with:

```rust
/// One per-interface traffic row: name on the left, sparkline taking
/// the row's full hexpand, current ↓rx ↑tx label on the right.
///
/// Wraps the existing `build_history_row(name)` helper used by
/// `page_stats`. The returned widgets are stored by the caller so
/// subsequent emissions can `spark.push(...)` and `value.set_text(...)`
/// instead of rebuilding the row.
fn build_iface_traffic_row(iface: &sensors::NetInterface) -> IfaceRow {
    let (container, spark, value) = build_history_row(&iface.name);
    IfaceRow { container, spark, value }
}
```

- [ ] **Step 3: Update bind closure to use `entry.container`**

Find the bind closure inside `build_traffic_group_v2()` (around lines 741-784). It currently calls `group_for_bind.remove(&entry.row)` and `group_for_bind.add(&entry.row)`. Update both to use `entry.container`.

The current block:

```rust
            // Remove interfaces that disappeared.
            let live: HashSet<String> =
                interfaces.iter().map(|i| i.name.clone()).collect();
            cache_mut.retain(|name, entry| {
                let keep = live.contains(name);
                if !keep {
                    group_for_bind.remove(&entry.row);
                }
                keep
            });

            // Update existing rows; create new ones for unseen names.
            for iface in interfaces {
                let combined = iface.rx_rate_bps + iface.tx_rate_bps;
                let value_text = format!(
                    "\u{2193} {} \u{2191} {}",
                    fmt_rate(iface.rx_rate_bps),
                    fmt_rate(iface.tx_rate_bps),
                );
                if let Some(entry) = cache_mut.get(&iface.name) {
                    entry.spark.push(combined);
                    entry.value.set_text(&value_text);
                } else {
                    // New interface arrived mid-session. Remove every
                    // surviving iface row from the group, insert the new
                    // entry into the cache, then re-add all rows in
                    // sorted order so display order matches name order.
                    // Totals/TCP rows are unaffected: they were added to
                    // the group synchronously before any iface row, so
                    // they remain at the bottom of the visual stack.
                    for entry in cache_mut.values() {
                        group_for_bind.remove(&entry.row);
                    }
                    let entry = build_iface_traffic_row(iface);
                    entry.spark.push(combined);
                    entry.value.set_text(&value_text);
                    cache_mut.insert(iface.name.clone(), entry);
                    let mut sorted_names: Vec<&String> = cache_mut.keys().collect();
                    sorted_names.sort();
                    for name in sorted_names {
                        if let Some(entry) = cache_mut.get(name) {
                            group_for_bind.add(&entry.row);
                        }
                    }
                }
            }
```

Replace with:

```rust
            // Remove interfaces that disappeared.
            let live: HashSet<String> =
                interfaces.iter().map(|i| i.name.clone()).collect();
            cache_mut.retain(|name, entry| {
                let keep = live.contains(name);
                if !keep {
                    group_for_bind.remove(&entry.container);
                }
                keep
            });

            // Update existing rows; create new ones for unseen names.
            for iface in interfaces {
                let combined = iface.rx_rate_bps + iface.tx_rate_bps;
                let value_text = format!(
                    "\u{2193} {} \u{2191} {}",
                    fmt_rate(iface.rx_rate_bps),
                    fmt_rate(iface.tx_rate_bps),
                );
                if let Some(entry) = cache_mut.get(&iface.name) {
                    entry.spark.push(combined);
                    entry.value.set_text(&value_text);
                } else {
                    // New interface arrived mid-session. Remove every
                    // surviving iface row from the group, insert the new
                    // entry into the cache, then re-add all rows in
                    // sorted order so display order matches name order.
                    // Totals/TCP rows are unaffected: they were added to
                    // the group synchronously before any iface row, so
                    // they remain at the bottom of the visual stack.
                    for entry in cache_mut.values() {
                        group_for_bind.remove(&entry.container);
                    }
                    let entry = build_iface_traffic_row(iface);
                    entry.spark.push(combined);
                    entry.value.set_text(&value_text);
                    cache_mut.insert(iface.name.clone(), entry);
                    let mut sorted_names: Vec<&String> = cache_mut.keys().collect();
                    sorted_names.sort();
                    for name in sorted_names {
                        if let Some(entry) = cache_mut.get(name) {
                            group_for_bind.add(&entry.container);
                        }
                    }
                }
            }
```

(Three substitutions: `entry.row` → `entry.container` at three call sites in this block.)

- [ ] **Step 4: Build trollshell**

Run: `cargo build -p trollshell --message-format=short 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p trollshell --message-format=short 2>&1 | grep pages.rs`
Expected: no new warnings on `pages.rs`.

- [ ] **Step 6: Run workspace tests**

Run: `cargo test --workspace --message-format=short 2>&1 | grep -E '(test result|FAILED)' | head -20`
Expected: every line is `test result: ok.`. No `FAILED`.

- [ ] **Step 7: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
refactor(network-page): per-interface traffic rows use build_history_row

The AdwActionRow + suffix-box approach was squeezing the sparkline to
~120 px next to a value label. build_history_row (the page_stats
pattern) gives the sparkline the row's full hexpand, with name on the
left and value on the right. PreferencesGroup accepts the plain
gtk::Box returned by build_history_row directly — same shape as
build_stats_history_group does for CPU/memory/network/GPU rows.

Keyed cache stays (Sparkline persistence is load-bearing); only the
row primitive changes from adw::ActionRow to gtk::Box. Field renamed
row → container for accuracy.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Manual integration check on a running session

**Files:** none (runtime verification only)

- [ ] **Step 1: Launch trollshell**

Run: `cargo run -p trollshell` from a niri session terminal.
Expected: bar appears, network drawer opens.

- [ ] **Step 2: Verify network drawer is no longer dense**

Open the network drawer. Confirm:
- Two-column grid at top (Configuration left, Live right) renders as before.
- Per-interface traffic rows now show the sparkline filling the row width with name on the left and `↓ X ↑ Y` on the right (sysstats-style), not a small 120px sparkline jammed into a suffix.
- Below the grid: a single "Active connections" row with subtitle "{N} sockets, {M} with PID" and a chevron suffix. NO inline list of socket rows.

- [ ] **Step 3: Drill into Page::Connections**

Click the "Active connections" row. Confirm:
- Modal stack swaps to the new page.
- Layout matches the previous inline section: own-user sockets at top sorted by program, "Other users" expander with no-PID sockets, "(+N more)" hint when truncated.
- Subtitle on the group reflects the live total, same as before.

- [ ] **Step 4: Verify sparkline persistence**

With network active (e.g. open a few browser tabs), watch a per-interface row in the traffic group for ~30 seconds. Confirm the sparkline accumulates samples (you should see a moving graph, not a flat line — the persistence guarantee from the prior fix `66f22bc` is unchanged by this work).

- [ ] **Step 5: Capture anomalies in `BUGS.md`**

If anything misbehaves, append a line to `BUGS.md`. Do not commit code in this step.

---

## Self-Review

Spec coverage check (against `docs/superpowers/specs/2026-04-29-network-panel-tightening-design.md`, "Scope > In scope"):

- ✅ `Page::Connections` enum variant — Task 1 Step 1.
- ✅ `pub fn page_connections()` builder — Task 1 Step 4.
- ✅ Stack mount via `add_named` — Task 1 Step 3.
- ✅ Removal of inline active-connections from `page_network` — Task 1 Step 5.
- ✅ Drill-down `ActionRow` with bound subtitle and `switch_active` activation — Task 1 Step 5.
- ✅ Truncation hint mechanism follows verbatim — Task 1 Step 4 (the lifted body includes `owned_overflow_track` and the (+N more) hint).
- ✅ `build_iface_traffic_row` uses `build_history_row` — Task 2 Steps 1-2.
- ✅ `IfaceRow.row` renamed to `container`, type changes to `gtk::Box` — Task 2 Steps 1-2.
- ✅ Bind closure callsites updated — Task 2 Step 3.
- ✅ Manual integration plan — Task 3.

No placeholders. Type names (`IfaceRow.container`, `Page::Connections`, `page_connections()`) consistent across tasks. Function references (`build_history_row`, `build_connection_row`, `CONN_BUCKET_CAP`, `crate::modal::switch_active`) all verified to exist in the current tree before plan was written.
