# Network panel tightening — connections drill-down + traffic-row layout fix

**Status:** design
**Date:** 2026-04-29
**Author:** Claude (with annika)
**Predecessors:** `2026-04-29-network-panel-and-vpn-design.md`.

## Goal

Two follow-up adjustments to the network drawer page that landed earlier today:

1. **Move "Active connections" to a dedicated drill-down page.** The inline full-width section (top-N=30 own-user + 30 other-user rows + truncation hint) makes the network drawer "way too much" — too dense, too tall, dominates the panel. Replace it with a single drill-down row that navigates to a new `Page::Connections`.

2. **Fix the per-interface traffic-row layout.** The current rows use `adw::ActionRow` with the sparkline jammed into the suffix slot at a hard-coded 120 px width, next to a value label. The right pattern is the existing `build_history_row(name)` helper in `page_stats`: a plain `gtk::Box` row of `[name 80px | sparkline hexpand | value 80px]` added directly to the `PreferencesGroup`. The sparkline gets the full row width.

UI/service separation stays absolute: no service touches change, both fixes are pure UI / layout.

## Motivation

- The previous spec (`2026-04-29-network-panel-and-vpn-design.md`) put "Active connections" inline below the two-column grid because the design intent was "one network drawer, see everything at a glance". In practice top-N=30 + 30 others + a truncation hint is ~60 rows of monospace socket text, which scrolls past the visible drawer height and pushes Wi-Fi / DNS off-screen on a typical monitor. A drill-down keeps the network drawer focused on configuration + live stats, and gives connections their own page-sized canvas.
- The traffic-row layout was a copy-from-spec deviation that hurt — `adw::ActionRow` is the right primitive for rows of [title | controls], wrong primitive for [name | sparkline | value]. `build_history_row` already solves this pattern in `page_stats` (CPU, memory, network, GPU temp rows). Reusing it is cheaper than fighting AdwActionRow's slot model.

## Scope

### In scope

- New `Page::Connections` enum variant in `trollshell/src/modal.rs`.
- New `pub fn page_connections() -> gtk::Widget` in `trollshell/src/widgets/pages.rs`.
- Removal of the inline active-connections section from `page_network`.
- New drill-down row in `page_network`'s outer column: `adw::ActionRow` titled "Active connections" with subtitle bound to `netconn::connections()` ("{total} sockets, {with_pid} with PID"), `system-search-symbolic` (or similar) prefix icon, chevron suffix, click navigates via `crate::modal::switch_active(Page::Connections)`.
- Refactor of the per-interface rows in `build_traffic_group_v2` to use `build_history_row(name)` instead of `adw::ActionRow + suffix box`. Keyed-by-name cache stays (Sparkline persistence is the load-bearing invariant from the prior spec's Task-4 fix). Truncation hint and "Other users" expander move along with the connections content.
- The truncation-hint mechanism (Task 11's `ceef26e` commit) follows to the new page unchanged.

### Out of scope

- **Connections search/filter UI.** Still in `FUTURE.md`.
- **Connections page sub-grouping** (e.g. group by program, collapsing per-program). Top-N sorted-by-program is still the v1 layout — moved verbatim.
- **Wi-Fi UX, DNS panel restructure, mobile-data, accent color, etc.** Out of this pass.
- **Sparkline rx/tx split.** Combined trace with rx/tx in the value label stays as-is (matches the existing pattern; `build_history_network_row` in `page_stats` does the same).
- **Service changes.** `vpn`, `netconn`, `sensors` all unchanged.
- **Removing the active-connections content entirely.** It's relocated, not deleted.

## Architecture

```
trollshell/src/modal.rs
  Page::Connections (new variant, after Vpn)
  stack_name(Connections) → "connections"
  install: stack.add_named(&pages::page_connections(), Some("connections"))

trollshell/src/widgets/pages.rs
  page_network() — strip the inline active-connections block; add a
                   drill-down ActionRow that navigates to Page::Connections.

  page_connections() — new full-page builder hosting the relocated
                       active-connections content (PreferencesGroup,
                       own-user rows, other-user expander, truncation
                       hint row, drain-and-rebuild bind on
                       netconn::connections()).

  build_traffic_group_v2() — per-interface row construction switches
                             from AdwActionRow + suffix to
                             build_history_row(name); keyed cache type
                             changes accordingly.
```

No service-layer changes. No new bar widget. No CSS additions (the existing `.ts-history-row`, `.ts-stat-name`, `.ts-stat-value` cover the traffic-row visuals; `.ts-mono` already covers connection-row subtitles).

## Detailed changes

### `Page::Connections` and modal stack mount

In `trollshell/src/modal.rs`, add `Connections` to the `Page` enum (placed after `Vpn`, near its semantic siblings) and add a `Self::Connections => "connections"` arm to `stack_name`. In `install`, mount the page in the stack:

```rust
stack.add_named(&pages::page_connections(), Some(Page::Connections.stack_name()));
```

### `page_connections()`

A new full-page builder. Layout shape mirrors `page_vpn`:

```rust
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

    // ── existing active-connections binding lifted from page_network verbatim ──
    // Tracks: owned_track, owned_overflow_track, other_expander, other_rows_track.
    // Uses CONN_BUCKET_CAP. Set-on-change semantics, sort, truncation hint, etc.
    // (Body is identical to the block currently in page_network::ceef26e.)

    column.append(&conn_group);

    finish_page(&column)
}
```

The bind closure body, the `CONN_BUCKET_CAP` constant, the `build_connection_row` helper, and the `Rc<RefCell<...>>` trackers are the existing logic moved verbatim — no semantic change to how connections render. Only the host page changes.

### `page_network()` — drill-down replacement

Remove from `page_network()` everything from `// Active connections — full-width section below the grid.` through (and including) `outer.append(&conn_group);`. Also remove the `Rc<RefCell<...>>` track variables, the `build_connection_row` reference, the `CONN_BUCKET_CAP` reference — all those move to the new page.

Replace with a single drill-down `adw::ActionRow` appended to `outer`:

```rust
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

The `crate::modal::switch_active` helper already exists (`pages.rs` line 3198 and surrounding shows existing usage from the Settings page "More" pattern). Wraps the row in a `PreferencesGroup` to get the standard rounded-row chrome — pure cosmetic.

### `build_traffic_group_v2()` — sysstats-pattern rows

The keyed cache stays — Sparkline must persist across emissions for history. The change is what each cache entry looks like:

```rust
struct IfaceRow {
    container: gtk::Box,    // was: adw::ActionRow (the row itself was the container)
    spark: Sparkline,
    value: gtk::Label,
}

fn build_iface_traffic_row(name: &str) -> IfaceRow {
    let (container, spark, value) = build_history_row(name);
    IfaceRow { container, spark, value }
}
```

In the bind closure, the only delta is `cache_mut.insert` and `group_for_bind.add` / `remove` operate on `entry.container` (a `gtk::Box`) instead of `entry.row` (an `adw::ActionRow`). `PreferencesGroup::add` accepts both types — `page_stats::build_stats_history_group` already adds plain `gtk::Box` children of `build_history_row`.

Rename the field `row` → `container` so future readers don't expect `AdwActionRow` semantics. Other field names stay.

The drop of the `set_valign(Center)` line on the suffix box (added during the prior task) is fine — `build_history_row` handles its own internal alignment.

### Drilldown subtitle staleness

The drill-down ActionRow subtitle mirrors `netconn::connections()`. While the user is on the network page, both that subtitle and (when expanded) the new `Page::Connections` subscribe to the same signal. Both update on every emission. No staleness.

When navigating to `Page::Connections`, the modal stack's `switch_active` retains the same surface (the drawer) — it's the inner stack page that swaps. The page is ALWAYS mounted (`add_named` happens at install time, like every other page); navigation just brings it forward. No subscription-resume latency.

## Testing

- No new parser tests. The relocation is pure UI plumbing.
- `cargo build -p trollshell --message-format=short`: clean.
- `cargo test --workspace`: 17 existing passing tests still pass.
- `cargo clippy -p trollshell`: no new warnings on `pages.rs` or `modal.rs`.
- Manual integration check on a niri session:
  1. Open network drawer. Confirm two-column grid + "Active connections" drill-down row at the bottom (NOT the wall of socket rows).
  2. Click "Active connections" — modal swaps to the new page; rows render identically to before, including the "(+N more)" hint when capped.
  3. Per-interface traffic rows: confirm sparkline now spans the row's full width, with the interface name on the left and the rate on the right.
  4. Bring a VPN tunnel up — new `wg0`/`tailscale0` interface appears in the traffic group at the correct alphabetical position with a fresh sparkline.

## File touch summary

| File                              | Change                                                                                                                                                                                     |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `trollshell/src/modal.rs`         | `Page::Connections` variant + `stack_name` arm + `add_named` mount                                                                                                                         |
| `trollshell/src/widgets/pages.rs` | New `pub fn page_connections()`; remove inline active-connections block from `page_network` and replace with drill-down row; refactor `build_iface_traffic_row` to use `build_history_row` |

Net: roughly +60/−60 LOC inside one file (the connections block moves) plus ~20 LOC of new drill-down + page wiring. Modal.rs gets 3 new lines.

## Risks

- **`PreferencesGroup` style mismatch.** `adw::PreferencesGroup::add` accepts `gtk::Widget`, but the visual styling (separators, rounded corners, hover) is designed around `AdwActionRow` / `AdwExpanderRow`. The existing `build_stats_history_group` proves a `PreferencesGroup` of plain `gtk::Box` children renders correctly; risk verified low.
- **Drill-down icon naming.** `system-search-symbolic` / `go-next-symbolic` are standard freedesktop names available in `adwaita-icon-theme`. If a user's theme is missing them GTK falls back to a missing-image glyph (visible but not broken). Acceptable.
- **`switch_active` page existence.** Requires the page to already be mounted in the stack (it is — `install()` mounts every page once at startup). No new bar chip needed; navigation is page-internal.
