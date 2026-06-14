# Trollshell src reorg — `widgets/` + `panels/` + `components/` + `overlays/`

**Status:** design
**Date:** 2026-04-29
**Author:** Claude (with annika)

## Goal

Reorganize `trollshell/src/` from the current flat `widgets/` (which mixes bar chips, overlays, and the 3700-line `pages.rs`) into four semantic top-level dirs: `widgets/` (bar chips only), `panels/` (drawer pages), `components/` (reusable building blocks), `overlays/` (lock screen + OSD + dialogs + toast). Rename the drawer page-builder functions from `page_*` → `panel_*` so the call shape matches the new module path.

## Motivation

`trollshell/src/widgets/pages.rs` is ~3750 LOC after the recent network/VPN/connections work. Per-commit reviewers have flagged the file size on every PR for the last two weeks; "future contributors have to scroll past 3.5k lines to find the right helper" is a recurring note. The existing `widgets/` flat namespace also conflates four distinct things — bar items (what mounts in the bar), drawer-page builders (what mounts in the modal stack), overlays (lock screen, OSD, prompt, polkit dialog, notification toast), and the cross-cutting helpers buried inside `pages.rs`. A semantic split clarifies each kind's boundary and gives helpers shared across the binary (`build_history_row`, `deep_link_row`, layout primitives, formatters) a natural home.

Refactor only — no behavior change, no service layer touches.

## Scope

### In scope

- New top-level dir `trollshell/src/panels/` containing one file per drawer page (15 files), with a `mod.rs` re-exporting the `panel_*` builders.
- New top-level dir `trollshell/src/components/` containing the cross-cutting helpers extracted from `pages.rs` (layout primitives, `build_history_row`, `deep_link_row`, `build_connection_row` + `CONN_BUCKET_CAP`, formatters).
- New top-level dir `trollshell/src/overlays/` containing the per-monitor layer-shell overlays moved out of `widgets/`: `lock_screen`, `osd`, `polkit_dialog`, `prompt`, `notifications`. Each becomes `overlays/<name>.rs`.
- `trollshell/src/widgets/pages.rs` deleted; its contents distributed across `panels/` and `components/`.
- Per-page private helpers (`build_traffic_group_v2`, `build_iface_traffic_row`, `IfaceRow`, `build_tunnel_group`, `build_peer_row`, `power_action_row`, theme-dropdown helpers, etc.) move with their owning page into the corresponding `panels/<page>.rs`.
- Drawer page-builder rename: `page_<name>()` → `panel_<name>()` for all 15 functions. Callsites in `trollshell/src/modal.rs` updated.
- `trollshell/src/widgets/util.rs` (formatters `fmt_bytes`, `fmt_rate`) absorbed into `components/format.rs`.
- `trollshell/src/widgets/mod.rs` updated to remove the `pub mod pages;`, `mod util;`, and the five overlay-module declarations.
- `trollshell/src/main.rs` updated to (a) declare the four new top-level dirs (`mod panels; mod components; mod overlays;` — `mod widgets;` already exists), (b) change `widgets::lock_screen::install`, `widgets::osd::install`, `widgets::prompt::install`, `widgets::polkit_dialog::install`, `widgets::notifications::install` to `overlays::*::install` (5 callsites in `main.rs::run`).

### Out of scope

- **Hytte-level changes.** `hytte-services::*` and `hytte-ui::*` are unchanged. This refactor lives entirely inside `trollshell/`.
- **API surface changes.** No public types or signatures change beyond the `page_*` → `panel_*` rename. Behavior is unchanged.
- **Testing changes.** No new tests; existing workspace tests remain green.
- **CSS or visual changes.** Pure code reorganization.
- **`build_traffic_group_v2` / `build_connection_group_v2` "v2" suffixes.** They date from a prior redesign pass; renaming to drop the suffix is a separate cleanup.

## Target layout

```
trollshell/src/
├── main.rs                        (declares mod widgets/panels/components/overlays; updates 5 overlay::install callsites)
├── modal.rs                       (drawer infrastructure; updates: imports panels::*, calls panel_<name>)
├── widgets/                       (BAR CHIPS ONLY — what mounts in the bar)
│   ├── mod.rs                     (drops pub mod pages, mod util, and the 5 overlay mods)
│   ├── battery.rs / bluetooth.rs / brightness.rs / clock.rs /
│   ├── cpu.rs / disk.rs / gpu.rs / memory.rs / microphone.rs /
│   ├── mpris.rs / network.rs / notif_indicator.rs / power_chip.rs /
│   ├── settings_chip.rs / tray.rs / volume.rs / vpn.rs /
│   ├── window_list.rs / workspaces.rs                  (~19 files)
├── panels/                        (DRAWER PAGES — modal stack children)
│   ├── mod.rs                     (re-exports panel_<name> from each module)
│   ├── network.rs                 (panel_network + traffic group + iface row + connection group + DNS expander + …)
│   ├── vpn.rs                     (panel_vpn + tunnel group + peer row)
│   ├── connections.rs             (panel_connections + bucket cap stays here)
│   ├── stats.rs                   (panel_stats + history rows for CPU/memory/network/GPU + live rows)
│   ├── audio.rs                   (panel_audio + per-stream / per-sink helpers)
│   ├── bluetooth.rs               (panel_bluetooth + pair prompt + device row helpers)
│   ├── media.rs                   (panel_media)
│   ├── power.rs                   (panel_power)
│   ├── power_menu.rs              (panel_power_menu + power_action_row)
│   ├── notifications.rs           (panel_notifications + history app rows)
│   ├── appearance.rs              (panel_appearance / wallpaper)
│   ├── displays.rs                (panel_displays)
│   ├── clipboard.rs               (panel_clipboard)
│   ├── settings.rs                (panel_settings + theme dropdown helpers)
│   └── calendar.rs                (panel_calendar)
├── components/                    (REUSABLE BITS — cross-cutting building blocks)
│   ├── mod.rs                     (re-exports each helper at component-root)
│   ├── layout.rs                  (page_box, finish_page, page_grid, section)
│   ├── history_row.rs             (build_history_row)
│   ├── deep_link_row.rs           (deep_link_row)
│   ├── connection_row.rs          (build_connection_row + CONN_BUCKET_CAP)
│   └── format.rs                  (fmt_bytes, fmt_rate — moved from widgets/util.rs;
│                                   fmt_us, humanize_since)
└── overlays/                      (LAYER-SHELL OVERLAYS — what floats on top)
    ├── mod.rs
    ├── lock_screen.rs             (PAM auth surfaces, was widgets/lock_screen.rs)
    ├── osd.rs                     (volume / brightness OSD, was widgets/osd.rs)
    ├── polkit_dialog.rs           (auth dialog, was widgets/polkit_dialog.rs)
    ├── prompt.rs                  (wifi password prompt, was widgets/prompt.rs)
    └── notifications.rs           (toast notifications, was widgets/notifications.rs)
```

Note the panel-vs-overlay-name disambiguation: `panels/notifications.rs` (drawer history list) and `overlays/notifications.rs` (transient toasts) are distinct concerns — the drawer panel reads notification history, the overlay renders incoming toasts. Same domain, different surfaces, different files.

**Naming notes**

- `panels/<name>.rs` exposes one `pub fn panel_<name>() -> gtk::Widget`. Per-page private helpers stay in the same file.
- `components/<name>.rs` exposes one or two related helpers. `layout.rs` keeps `page_box`/`finish_page`/`page_grid`/`panel` together because they're a cohesive set used by every panel.
- `components::layout::panel(title)` — the function name `panel` clashes with the directory name `panels/`. Rename `fn panel(title)` → `fn section(title)` to avoid confusion. The function builds a vertically-stacked section inside `page_grid`, "section" describes it accurately.
- `overlays/<name>.rs` keeps the existing `pub fn install(monitor: &Monitor)` (or `install(monitors: &[Monitor])` for `lock_screen`) entry-point names — no rename. Callers in `main.rs::run` change from `widgets::<name>::install(...)` to `overlays::<name>::install(...)`.

## Function rename summary

| Old (in `pages.rs`)                    | New (in `panels/<file>.rs`)                                                                                 |
| -------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `pub fn page_media()`                  | `pub fn panel_media()`                                                                                      |
| `pub fn page_network()`                | `pub fn panel_network()`                                                                                    |
| `pub fn page_bluetooth()`              | `pub fn panel_bluetooth()`                                                                                  |
| `pub fn page_stats()`                  | `pub fn panel_stats()`                                                                                      |
| `pub fn page_audio()`                  | `pub fn panel_audio()`                                                                                      |
| `pub fn page_power()`                  | `pub fn panel_power()`                                                                                      |
| `pub fn page_notifications()`          | `pub fn panel_notifications()`                                                                              |
| `pub fn page_power_menu()`             | `pub fn panel_power_menu()`                                                                                 |
| `pub fn page_appearance()`             | `pub fn panel_appearance()`                                                                                 |
| `pub fn page_displays()`               | `pub fn panel_displays()`                                                                                   |
| `pub fn page_clipboard()`              | `pub fn panel_clipboard()`                                                                                  |
| `pub fn page_settings()`               | `pub fn panel_settings()`                                                                                   |
| `pub fn page_calendar()`               | `pub fn panel_calendar()`                                                                                   |
| `pub fn page_vpn()`                    | `pub fn panel_vpn()`                                                                                        |
| `pub fn page_connections()`            | `pub fn panel_connections()`                                                                                |
| `fn page_box() -> gtk::Box`            | `pub(crate) fn page_box() -> gtk::Box` (in `components/layout.rs`)                                          |
| `fn finish_page(...)` → `gtk::Widget`  | `pub(crate) fn finish_page(...)` (in `components/layout.rs`)                                                |
| `fn page_grid() -> gtk::Grid`          | `pub(crate) fn page_grid() -> gtk::Grid` (in `components/layout.rs`)                                        |
| `fn panel(title) -> gtk::Box`          | `pub(crate) fn section(title) -> gtk::Box` (in `components/layout.rs` — renamed to avoid module-name clash) |
| `fn build_history_row(name)`           | `pub(crate) fn build_history_row(name)` (in `components/history_row.rs`)                                    |
| `fn deep_link_row(...)`                | `pub(crate) fn deep_link_row(...)` (in `components/deep_link_row.rs`)                                       |
| `fn build_connection_row(...)`         | `pub(crate) fn build_connection_row(...)` (in `components/connection_row.rs`)                               |
| `const CONN_BUCKET_CAP`                | `pub(crate) const CONN_BUCKET_CAP` (in `components/connection_row.rs`)                                      |
| `fn humanize_since(t)`                 | `pub(crate) fn humanize_since(t)` (in `components/format.rs`)                                               |
| `fn fmt_us(us)`                        | `pub(crate) fn fmt_us(us)` (in `components/format.rs`)                                                      |
| `widgets::util::{fmt_bytes, fmt_rate}` | `components::format::{fmt_bytes, fmt_rate}` (file moves wholesale)                                          |

Per-panel private helpers (`build_traffic_group_v2`, `build_iface_traffic_row`, `IfaceRow`, `build_tunnel_group`, `build_peer_row`, `build_history_*_row`, theme-dropdown helpers, …) keep their names; visibility stays `fn` (file-private).

`power_action_row` is currently used by `page_power_menu` only, so it stays inside `panels/power_menu.rs` as a private fn. Cross-page reuse is rare; we don't pre-emptively move things to `components/`.

## Migration mechanics

The refactor is mechanical. Process:

1. **Create the new dirs and skeleton files.** `mkdir -p panels components overlays`. Add `mod.rs` for each.
2. **Move overlays.** `git mv` each of `widgets/{lock_screen,osd,polkit_dialog,prompt,notifications}.rs` to `overlays/`. Update each file's internal `crate::widgets::*` references (rare — overlays mostly self-contained) and any `tests/` cross-refs (none today).
3. **Move shared helpers into `components/`.** Copy `page_box`/`finish_page`/`page_grid`/`panel` out of `pages.rs` into `components/layout.rs` (renaming `panel` → `section`). Same for `build_history_row`, `deep_link_row`, `build_connection_row`, `humanize_since`, `fmt_us`. Move `widgets/util.rs` content into `components/format.rs`.
4. **Move each panel's content into `panels/<name>.rs`.** Each panel block in `pages.rs` becomes its own file. Add `use crate::components::*` imports at the top.
5. **Update `mod.rs` for `panels/`, `components/`, and `overlays/`.** Re-export the public entry points (`panel_<name>`, helpers).
6. **Update `widgets/mod.rs`.** Remove `pub mod pages;`, `mod util;`, and the five overlay `pub mod` lines.
7. **Update `trollshell/src/main.rs`.** Add `mod panels; mod components; mod overlays;` (currently only `mod modal; mod widgets;`). Change the five `widgets::<overlay>::install(...)` calls in `run` to `overlays::<overlay>::install(...)`.
8. **Update `modal.rs`.** Change every `pages::page_<name>()` call to `crate::panels::panel_<name>()`.
9. **Update internal `crate::modal::Page` references inside panels** — e.g. the Settings panel's deep-link rows refer to `crate::modal::Page::Appearance` etc. Those keep their fully-qualified path; nothing changes.
10. **Delete `widgets/pages.rs` and `widgets/util.rs`.**

Single commit. The build cannot be split mid-refactor without leaving a broken state for several intermediate commits, and the final state is small enough (one big mechanical PR) to review as a unit.

## Visibility model

- `components::*` — `pub(crate)` on all helpers. The components dir is a private implementation detail of the trollshell binary; nothing outside the crate consumes it.
- `panels::*` — public re-exports of `panel_<name>` at `panels/mod.rs`. Per-panel private helpers stay file-private.
- `widgets::*` — unchanged from current (per-widget `pub fn widget(monitor) -> gtk::Widget`).
- `overlays::*` — per-overlay `pub fn install(...)` (no rename); `mod.rs` re-exports each module.

## Imports inside panels

Each `panels/<name>.rs` will start with a similar import block:

```rust
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::SystemTime;       // only where used (vpn, calendar)

use chrono::{DateTime, Local};   // only where used (calendar, clock)

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::gtk::{self, gdk, gio, glib};
use hytte::prelude::*;
use hytte::services::<service>;  // service-specific imports

use crate::components::layout::{page_box, finish_page, page_grid, section};
use crate::components::format::{fmt_bytes, fmt_rate, fmt_us, humanize_since};
use crate::components::history_row::build_history_row;
use crate::components::deep_link_row::deep_link_row;
use crate::components::connection_row::{build_connection_row, CONN_BUCKET_CAP};
```

Each panel imports only what it uses; the full block above is the maximum a single panel needs. `network.rs` and `connections.rs` use `connection_row`; `stats.rs` and `network.rs` use `history_row`; `vpn.rs` and `calendar.rs` use `humanize_since`; etc.

## Risks

- **One-shot mechanical PR with ~35 files touched.** A single missed `pub(crate)` or a stale `crate::widgets::pages::*` import means a broken build. Mitigation: run `cargo build --workspace` and `cargo clippy --workspace` after each tranche during dev, even though the final commit lands as one. Rust's compiler errors are precise enough that any miss is immediately surfaced.
- **`panels` enum-name clash.** The `Page` enum in `modal.rs` keeps its name (renaming to `Panel` is tempting but adds another batch of touch points and the variants — `Network`, `Vpn`, `Connections` — read fine as `Page::Network`). Stack-name strings (`"network"`, etc.) are unchanged; they're a dictionary key, not a user-visible label. **Decision: keep `Page` enum.**
- **`fn panel` → `fn section` rename.** `section` is descriptive and avoids the module-name clash. `card`, `block`, `box_section` also considered; `section` is the clearest.
- **The `_v2` legacy.** `build_traffic_group_v2` / `build_connection_group_v2` retain their name during the move. Renaming away from `_v2` is a follow-up patch, not bundled here.
- **Workspace tests.** All currently-passing tests are in `crates/hytte-services/src/*.rs` (parser tests for vpn, netconn, theme). None live inside `pages.rs`. The refactor doesn't touch tests.
- **Doc-comment integrity.** Each `pub fn panel_<name>()` keeps its existing module-level rustdoc; module-level docs at the top of each `panels/<name>.rs` give the page's purpose at a glance.

## File touch summary

| Operation                 | Count | Notes                                                                                 |
| ------------------------- | ----- | ------------------------------------------------------------------------------------- |
| New files (`panels/`)     | 16    | `mod.rs` + 15 panels                                                                  |
| New files (`components/`) | 6     | `mod.rs` + 5 helper files                                                             |
| New files (`overlays/`)   | 6     | `mod.rs` + 5 overlay modules (lock_screen, osd, polkit_dialog, prompt, notifications) |
| Renamed files (git mv)    | 5     | `widgets/{lock_screen,osd,polkit_dialog,prompt,notifications}.rs` → `overlays/`       |
| Deleted files             | 2     | `widgets/pages.rs`, `widgets/util.rs`                                                 |
| Modified files            | ~3    | `widgets/mod.rs`, `modal.rs`, `main.rs`                                               |
| Renamed fns               | 16    | 15 `page_*` → `panel_*`, 1 `panel(title)` → `section(title)`                          |

Net: ~28 new/moved files, 2 deleted, 3 modified. About 3750 LOC of `pages.rs` redistributed across 21 small/medium panel+component files (largest expected: `panels/network.rs` at ~700 LOC; smallest: `panels/media.rs` at ~210 LOC). Five overlay files move ~1950 LOC out of `widgets/` unchanged.
