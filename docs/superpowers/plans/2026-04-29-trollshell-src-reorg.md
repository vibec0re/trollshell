# Trollshell src Reorg Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reorganize `trollshell/src/` from one flat `widgets/` dir (mixing bar chips, overlays, and the 3870-line `pages.rs`) into four semantic top-level dirs: `widgets/` (bar chips only), `panels/` (drawer pages), `components/` (cross-cutting helpers), `overlays/` (layer-shell floats).

**Architecture:** Mechanical refactor preserving all behavior. Phase 1 extracts cross-cutting helpers from `pages.rs` and `widgets/util.rs` into `components/`. Phase 2 moves overlay files from `widgets/` to `overlays/` via `git mv`. Phase 3 moves each drawer page (15 of them) out of `pages.rs` into its own `panels/<name>.rs` file, renaming `page_<name>` → `panel_<name>` per the spec. Phase 4 deletes the now-empty `pages.rs`. The build is kept green at every commit boundary.

**Tech Stack:** Rust 1.94, GTK4 + libadwaita. No new deps. Pure structural reorg of existing code.

---

## Phases

- **Phase 1 (Task 1):** Components extraction + scaffolding for `panels/` and `overlays/`. ~1 commit.
- **Phase 2 (Task 2):** Overlays move (5 `git mv`). ~1 commit.
- **Phase 3 (Tasks 3–17):** Per-panel move (15 panels, alphabetical). ~15 commits.
- **Phase 4 (Task 18):** Delete `widgets/pages.rs`. ~1 commit.

After every task, the build is clean and tests pass — the binary still runs.

Spec reference: `/home/choom/src/trollshell/docs/superpowers/specs/2026-04-29-trollshell-src-reorg-design.md`.

---

## File Structure

End state (after Task 18):

```
trollshell/src/
├── main.rs                        (declares mod widgets/panels/components/overlays)
├── modal.rs                       (calls panels::panel_<name>)
├── widgets/                       (BAR CHIPS ONLY)
│   ├── mod.rs                     (no pages, no util, no overlays)
│   ├── battery.rs / bluetooth.rs / brightness.rs / clock.rs /
│   ├── cpu.rs / disk.rs / gpu.rs / memory.rs / microphone.rs /
│   ├── mpris.rs / network.rs / notif_indicator.rs / power_chip.rs /
│   ├── settings_chip.rs / tray.rs / volume.rs / vpn.rs /
│   ├── window_list.rs / workspaces.rs
├── panels/                        (DRAWER PAGES)
│   ├── mod.rs                     (re-exports panel_<name>)
│   ├── appearance.rs / audio.rs / bluetooth.rs / calendar.rs /
│   ├── clipboard.rs / connections.rs / displays.rs / media.rs /
│   ├── network.rs / notifications.rs / power.rs / power_menu.rs /
│   ├── settings.rs / stats.rs / vpn.rs
├── components/                    (CROSS-CUTTING HELPERS)
│   ├── mod.rs
│   ├── layout.rs                  (page_box, finish_page, page_grid, section)
│   ├── history_row.rs             (build_history_row)
│   ├── deep_link_row.rs           (deep_link_row)
│   ├── connection_row.rs          (build_connection_row, CONN_BUCKET_CAP)
│   └── format.rs                  (fmt_bytes, fmt_rate, fmt_us, humanize_since)
└── overlays/                      (LAYER-SHELL OVERLAYS)
    ├── mod.rs
    ├── lock_screen.rs / notifications.rs / osd.rs /
    └── polkit_dialog.rs / prompt.rs
```

---

## Task 1: Extract `components/`, scaffold `panels/` and `overlays/`

**Files:**

- Create: `trollshell/src/components/mod.rs`
- Create: `trollshell/src/components/layout.rs`
- Create: `trollshell/src/components/format.rs`
- Create: `trollshell/src/components/history_row.rs`
- Create: `trollshell/src/components/deep_link_row.rs`
- Create: `trollshell/src/components/connection_row.rs`
- Create: `trollshell/src/panels/mod.rs` (initially empty stub)
- Create: `trollshell/src/overlays/mod.rs` (initially empty stub)
- Modify: `trollshell/src/widgets/pages.rs` — remove the helpers being moved; rename `panel(title)` → `section(title)` at every callsite; replace `use super::util::{fmt_bytes, fmt_rate};` with `use crate::components::format::{fmt_bytes, fmt_rate};` plus other `use crate::components::*` imports as needed.
- Modify: `trollshell/src/widgets/mod.rs` — remove `pub mod util;`.
- Delete: `trollshell/src/widgets/util.rs`.
- Modify: `trollshell/src/main.rs` — add three lines: `mod components;`, `mod panels;`, `mod overlays;` near the top (after `mod widgets;`).

- [ ] **Step 1: Create the `components/` directory and helper files**

```bash
cd /home/choom/src/trollshell/trollshell/src
mkdir -p components panels overlays
```

Write `components/mod.rs`:

```rust
//! Cross-cutting building blocks reused across panels and (sometimes)
//! widgets. Each submodule owns one focused helper or a tight family of
//! helpers. Visibility is `pub(crate)` throughout — these are
//! implementation details of the trollshell binary.

pub mod connection_row;
pub mod deep_link_row;
pub mod format;
pub mod history_row;
pub mod layout;
```

Write `components/layout.rs` (lifts `page_box`, `finish_page`, `page_grid`, and `panel`→renamed `section` from `pages.rs:46-92`):

```rust
//! Layout primitives every drawer panel uses.
//!
//! `page_box` and `finish_page` wrap the outer column with the standard
//! drawer styling and an `AdwClamp` width cap. `page_grid` is the
//! two-column grid container. `section` (renamed from `panel` to avoid
//! clashing with the `panels/` module name) is the titled card you
//! attach into a grid cell.

use hytte::adw;
use hytte::gtk::{self, prelude::*};

pub(crate) fn page_box() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 4);
    b.add_css_class("ts-modal-page");
    b
}

/// Wrap a finished page widget (Box or Grid) in an `AdwClamp` so a child
/// reporting a pathological natural width (e.g. an `AdwActionRow` subtitle
/// that ends up holding a long single-line list) can't push the
/// layer-shell modal surface to full-screen width. Belt-and-suspenders
/// against the same class of bug — individual rows should still constrain
/// themselves (multi-line subtitles, `subtitle_lines(0)`, etc.) but this
/// catches the ones that don't.
pub(crate) fn finish_page(content: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let clamp = adw::Clamp::builder()
        .maximum_size(680)
        .tightening_threshold(560)
        .child(content)
        .build();
    clamp.upcast()
}

/// Two-column (or more) grid for rich modal pages. Sections attach via
/// `grid.attach(&section, col, row, 1, 1)`.
pub(crate) fn page_grid() -> gtk::Grid {
    let g = gtk::Grid::new();
    g.add_css_class("ts-modal-page");
    g.add_css_class("ts-page-grid");
    g.set_row_spacing(12);
    g.set_column_spacing(12);
    g.set_column_homogeneous(true);
    g
}

/// Card-style section with a title header. Caller appends content by
/// calling `outer.append(&child)` on the returned Box.
///
/// Renamed from `panel` to avoid the module-name clash with `panels/`.
pub(crate) fn section(title: &str) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 4);
    outer.add_css_class("ts-panel");
    outer.set_hexpand(true);
    outer.set_vexpand(true);
    let title_label = gtk::Label::new(Some(title));
    title_label.add_css_class("ts-panel-title");
    title_label.set_xalign(0.0);
    outer.append(&title_label);
    outer
}
```

Write `components/format.rs` (combines `widgets/util.rs` content with `fmt_us` from `pages.rs:304` and `humanize_since` from `pages.rs:3599`):

```rust
//! Formatters used across panels and widgets. Pure functions; no side
//! effects, no allocation beyond the returned `String`.

use std::time::SystemTime;

/// Format a byte count as a human-readable string (e.g. `"7.4 GiB"`).
pub(crate) fn fmt_bytes(b: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let f = b as f64;
    if f >= 1_073_741_824.0 {
        format!("{:.1} GiB", f / 1_073_741_824.0)
    } else if f >= 1_048_576.0 {
        format!("{:.1} MiB", f / 1_048_576.0)
    } else if f >= 1024.0 {
        format!("{:.1} KiB", f / 1024.0)
    } else {
        format!("{f:.0} B")
    }
}

/// Format a byte-per-second rate as a human-readable string (e.g. `"7.4 GiB/s"`).
pub(crate) fn fmt_rate(bps: f64) -> String {
    if bps >= 1_073_741_824.0 {
        format!("{:.1} GiB/s", bps / 1_073_741_824.0)
    } else if bps >= 1_048_576.0 {
        format!("{:.1} MiB/s", bps / 1_048_576.0)
    } else if bps >= 1024.0 {
        format!("{:.1} KiB/s", bps / 1024.0)
    } else {
        format!("{bps:.0} B/s")
    }
}

/// Format a duration in microseconds as `M:SS` (used by the media panel
/// for player position / track length).
pub(crate) fn fmt_us(us: u64) -> String {
    let secs = us / 1_000_000;
    let m = secs / 60;
    let s = secs % 60;
    format!("{m}:{s:02}")
}

/// Render a `SystemTime` as a relative `Xs/m/h/d ago`, or
/// `"moments from now"` for a future timestamp. Used by the VPN panel
/// for tunnel `since` and per-peer last-handshake.
pub(crate) fn humanize_since(t: SystemTime) -> String {
    let now = SystemTime::now();
    match now.duration_since(t) {
        Ok(d) => {
            let secs = d.as_secs();
            if secs < 60 {
                format!("{secs}s ago")
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86400)
            }
        }
        Err(_) => "moments from now".to_string(),
    }
}
```

Write `components/history_row.rs` (lifts `build_history_row` from `pages.rs:1887-1908`):

```rust
//! `build_history_row` — the row primitive used by `panel_stats` and by
//! the per-interface rows in `panel_network`. Returns
//! `[name 80px | sparkline hexpand | value 80px]` as a plain `gtk::Box`
//! (not an `AdwActionRow`) so the sparkline takes the row's full width.

use hytte::gtk::{self, prelude::*};
use hytte::ui::Sparkline;

/// Build a `[name | Sparkline | value]` row styled `.ts-history-row`.
/// Returns the box, the Sparkline (caller pushes samples), and the
/// value label (caller binds text on it).
pub(crate) fn build_history_row(name: &str) -> (gtk::Box, Sparkline, gtk::Label) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("ts-history-row");

    let name_label = gtk::Label::new(Some(name));
    name_label.add_css_class("ts-stat-name");
    name_label.set_xalign(0.0);
    name_label.set_size_request(80, -1);
    row.append(&name_label);

    let spark = Sparkline::new(60);
    spark.widget().set_hexpand(true);
    row.append(spark.widget());

    let value_label = gtk::Label::new(None);
    value_label.add_css_class("ts-stat-value");
    value_label.set_xalign(1.0);
    value_label.set_size_request(80, -1);
    row.append(&value_label);

    (row, spark, value_label)
}
```

Write `components/deep_link_row.rs` (lifts `deep_link_row` from `pages.rs:3306-3327`):

```rust
//! `deep_link_row` — an `AdwActionRow` that opens a different drawer
//! page on activation. Used by the Settings panel's "More" group and
//! by the Network panel's "Active connections" drill-down.

use hytte::adw::{self, prelude::*};
use hytte::gtk;

/// Build an `AdwActionRow` that, on activation, swaps every open drawer
/// to `target` via `crate::modal::switch_active`. Used by drawer panels
/// to surface other pages that don't have a dedicated bar chip.
pub(crate) fn deep_link_row(
    title: &str,
    subtitle: Option<&str>,
    icon_name: &str,
    target: crate::modal::Page,
) -> adw::ActionRow {
    let mut builder = adw::ActionRow::builder().title(title).activatable(true);
    if let Some(s) = subtitle {
        builder = builder.subtitle(s);
    }
    let row = builder.build();
    let icon = gtk::Image::from_icon_name(icon_name);
    row.add_prefix(&icon);
    let go_next = gtk::Image::from_icon_name("go-next-symbolic");
    row.add_suffix(&go_next);
    row.connect_activated(move |_| {
        crate::modal::switch_active(target);
    });
    row
}
```

Write `components/connection_row.rs` (lifts `CONN_BUCKET_CAP` from `pages.rs:996` and `build_connection_row` from `pages.rs:1001-…`):

```rust
//! Per-process connection row — used by the Connections panel to render
//! one socket from `hytte::services::netconn::connections()`.

use hytte::adw::{self, prelude::*};
use hytte::services::netconn::{ConnState, Connection, Proto};

/// Top-N cap for each bucket of the active-connections section.
pub(crate) const CONN_BUCKET_CAP: usize = 30;

/// Single-line render of an active connection: program (or "(unknown)")
/// + monospace `proto local→remote (state)` subtitle.
pub(crate) fn build_connection_row(c: &Connection) -> adw::ActionRow {
    let title = match c.program.as_deref() {
        Some(p) => match c.pid {
            Some(pid) => format!("{p} · pid {pid}"),
            None => p.to_string(),
        },
        None => "(unknown)".to_string(),
    };
    let row = adw::ActionRow::builder().title(&title).build();
    let proto = match c.proto {
        Proto::Tcp => "tcp",
        Proto::Tcp6 => "tcp6",
        Proto::Udp => "udp",
        Proto::Udp6 => "udp6",
    };
    let state = match c.state {
        ConnState::Established => "ESTAB",
        ConnState::Listen => "LISTEN",
        ConnState::TimeWait => "TIME-WAIT",
        ConnState::Close => "CLOSE",
        ConnState::Other => "·",
    };
    let remote = c
        .remote
        .map(|a| format!(" → {a}"))
        .unwrap_or_default();
    row.set_subtitle(&format!("{proto} {}{remote} ({state})", c.local));
    row.add_css_class("ts-mono");
    row
}
```

- [ ] **Step 2: Create empty `panels/mod.rs` and `overlays/mod.rs` stubs**

Write `panels/mod.rs`:

```rust
//! Drawer pages mounted into `modal.rs`'s per-monitor `gtk::Stack`.
//! Each page is one `pub fn panel_<name>() -> gtk::Widget` re-exported
//! at the module root. Per-panel private helpers stay in their owning
//! file. Phase 3 of the reorg fills this out one panel at a time;
//! after the first move, `panel::mod.rs` will list `pub mod <name>;`
//! lines and re-exports.
```

Write `overlays/mod.rs`:

```rust
//! Per-monitor layer-shell overlays — lock screen, OSD, dialogs, toast.
//! Each module exposes a `pub fn install(...)` that wires the overlay
//! to the relevant signal source. Phase 2 of the reorg populates this
//! by `git mv`-ing the existing overlay files out of `widgets/`.
```

- [ ] **Step 3: Declare the new modules in `main.rs`**

Open `trollshell/src/main.rs`. Find:

```rust
mod modal;
mod widgets;
```

Replace with:

```rust
mod components;
mod modal;
mod overlays;
mod panels;
mod widgets;
```

- [ ] **Step 4: Strip moved helpers from `pages.rs`**

Open `trollshell/src/widgets/pages.rs`. Delete these sections (line numbers approximate; grep by function name for safety):

- Lines ~46-92: `fn page_box`, `fn finish_page`, `fn page_grid`, `fn panel(title: &str)` (all four functions).
- Line ~304: `fn fmt_us(us: u64) -> String { ... }`.
- Line ~996: `const CONN_BUCKET_CAP: usize = 30;`.
- Lines ~1001-1036 (the body of `fn build_connection_row(c: &Connection) -> adw::ActionRow`).
- Lines ~1887-1908: `fn build_history_row(name: &str) -> (gtk::Box, Sparkline, gtk::Label)`.
- Lines ~3306-3327: `fn deep_link_row(...)`.
- Lines ~3599-3618: `fn humanize_since(t: SystemTime) -> String`.

Use `grep -n 'fn page_box\|fn finish_page\|fn page_grid\|fn panel(\|fn fmt_us\|fn build_history_row\|fn deep_link_row\|fn build_connection_row\|fn humanize_since\|^const CONN_BUCKET_CAP' trollshell/src/widgets/pages.rs` to confirm zero hits remain after deletion.

- [ ] **Step 5: Rewire `pages.rs` imports**

At the top of `pages.rs`, replace:

```rust
use super::util::{fmt_bytes, fmt_rate};
```

with:

```rust
use crate::components::connection_row::{build_connection_row, CONN_BUCKET_CAP};
use crate::components::deep_link_row::deep_link_row;
use crate::components::format::{fmt_bytes, fmt_rate, fmt_us, humanize_since};
use crate::components::history_row::build_history_row;
use crate::components::layout::{finish_page, page_box, page_grid, section};
```

- [ ] **Step 6: Rename `panel(title)` → `section(title)` callsites in `pages.rs`**

Search and replace inside `pages.rs` only (other files don't call this helper):

```bash
grep -n '\bpanel("' trollshell/src/widgets/pages.rs
```

Each call site looks like `let foo = panel("Configuration");`. Substitute `panel("…")` → `section("…")`. Two-step approach — find all matches first, then edit each. The Edit tool's `replace_all` flag works here since `panel("` is unambiguous as a substring.

- [ ] **Step 7: Drop `widgets/util.rs` and update `widgets/mod.rs`**

Delete the file:

```bash
rm trollshell/src/widgets/util.rs
```

Open `trollshell/src/widgets/mod.rs`. Find and remove:

```rust
pub mod util;
```

- [ ] **Step 8: Build the workspace**

Run: `cargo build --workspace --message-format=short 2>&1 | tail -10`
Expected: `Finished` cleanly. Common compile errors at this step:

- `unresolved import super::util` — leftover; grep `super::util` in pages.rs to confirm none remain.
- `cannot find function panel` — leftover `panel("…")` callsite; grep `\bpanel(` in pages.rs.
- `cannot find function build_history_row` etc. — missing `use crate::components::*` line.
- Visibility errors — the helper might still be `fn` in `components/` instead of `pub(crate) fn`.

- [ ] **Step 9: Run clippy**

Run: `cargo clippy --workspace --message-format=short 2>&1 | grep -E '(pages.rs|components/)'`
Expected: no new warnings. (The pre-existing `mpris.rs:23` doc-backticks warning may still appear; unrelated.)

- [ ] **Step 10: Run workspace tests**

Run: `cargo test --workspace --message-format=short 2>&1 | grep -E '(test result|FAILED)' | head -20`
Expected: every line `test result: ok.`. No `FAILED`.

- [ ] **Step 11: Commit**

```bash
git add trollshell/src/components/ trollshell/src/panels/mod.rs trollshell/src/overlays/mod.rs \
    trollshell/src/main.rs trollshell/src/widgets/mod.rs trollshell/src/widgets/pages.rs
git rm trollshell/src/widgets/util.rs
git commit -m "$(cat <<'EOF'
refactor(trollshell): extract components/ and scaffold panels/ + overlays/

Pulls cross-cutting helpers out of the 3870-line widgets/pages.rs into
a new components/ tree:

  components/layout.rs        page_box, finish_page, page_grid, section
                              (renamed from panel to free up panels/)
  components/format.rs        fmt_bytes, fmt_rate (was widgets/util.rs)
                              fmt_us, humanize_since (were in pages.rs)
  components/history_row.rs   build_history_row
  components/deep_link_row.rs deep_link_row
  components/connection_row.rs build_connection_row + CONN_BUCKET_CAP

widgets/util.rs deleted (its content moved to components/format.rs).
panels/mod.rs and overlays/mod.rs created as empty stubs; subsequent
phases populate them.

main.rs gains mod components/panels/overlays declarations.

No behavior change. pages.rs now imports the helpers from components/
instead of defining them inline.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Move overlays out of `widgets/`

**Files:**

- Create: `trollshell/src/overlays/mod.rs` (replace stub from Task 1).
- Move (`git mv`): `trollshell/src/widgets/lock_screen.rs` → `trollshell/src/overlays/lock_screen.rs`.
- Move (`git mv`): `trollshell/src/widgets/notifications.rs` → `trollshell/src/overlays/notifications.rs`.
- Move (`git mv`): `trollshell/src/widgets/osd.rs` → `trollshell/src/overlays/osd.rs`.
- Move (`git mv`): `trollshell/src/widgets/polkit_dialog.rs` → `trollshell/src/overlays/polkit_dialog.rs`.
- Move (`git mv`): `trollshell/src/widgets/prompt.rs` → `trollshell/src/overlays/prompt.rs`.
- Modify: `trollshell/src/widgets/mod.rs` — remove the five overlay `pub mod` lines.
- Modify: `trollshell/src/main.rs` — change five `widgets::<overlay>::install(...)` callsites to `overlays::<overlay>::install(...)`.

- [ ] **Step 1: `git mv` the five files**

```bash
cd /home/choom/src/trollshell
git mv trollshell/src/widgets/lock_screen.rs   trollshell/src/overlays/lock_screen.rs
git mv trollshell/src/widgets/notifications.rs trollshell/src/overlays/notifications.rs
git mv trollshell/src/widgets/osd.rs           trollshell/src/overlays/osd.rs
git mv trollshell/src/widgets/polkit_dialog.rs trollshell/src/overlays/polkit_dialog.rs
git mv trollshell/src/widgets/prompt.rs        trollshell/src/overlays/prompt.rs
```

- [ ] **Step 2: Replace `overlays/mod.rs` stub with `pub mod` declarations**

Write `trollshell/src/overlays/mod.rs`:

```rust
//! Per-monitor layer-shell overlays — lock screen, OSD, dialogs, toast.
//! Each module exposes a `pub fn install(...)` that wires the overlay
//! to the relevant signal source. Moved out of `widgets/` so that
//! `widgets/` reads strictly as bar chips.

pub mod lock_screen;
pub mod notifications;
pub mod osd;
pub mod polkit_dialog;
pub mod prompt;
```

- [ ] **Step 3: Drop the overlay declarations from `widgets/mod.rs`**

Open `trollshell/src/widgets/mod.rs`. Remove these five lines:

```rust
pub mod lock_screen;
pub mod notifications;
pub mod osd;
pub mod polkit_dialog;
pub mod prompt;
```

The remaining content should list only the ~19 bar-chip widget modules (battery, bluetooth, brightness, …).

- [ ] **Step 4: Update `main.rs` callsites**

Open `trollshell/src/main.rs`. Find these five calls (all inside the `App::new(...).run(|app| { … })` closure):

```rust
                widgets::prompt::install(primary);
                widgets::polkit_dialog::install(primary);
…
            widgets::lock_screen::install(&app.monitors());
…
                widgets::notifications::install(monitor);
                widgets::osd::install(monitor);
```

Substitute `widgets::` with `overlays::` for each:

```rust
                overlays::prompt::install(primary);
                overlays::polkit_dialog::install(primary);
…
            overlays::lock_screen::install(&app.monitors());
…
                overlays::notifications::install(monitor);
                overlays::osd::install(monitor);
```

- [ ] **Step 5: Build**

Run: `cargo build --workspace --message-format=short 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 6: Workspace tests**

Run: `cargo test --workspace --message-format=short 2>&1 | grep -E '(test result|FAILED)' | head -20`
Expected: every `test result: ok.`, no `FAILED`.

- [ ] **Step 7: Commit**

```bash
git add trollshell/src/overlays/mod.rs trollshell/src/widgets/mod.rs trollshell/src/main.rs
git commit -m "$(cat <<'EOF'
refactor(trollshell): move overlays out of widgets/ into overlays/

Five files moved via git mv: lock_screen, notifications (toast), osd,
polkit_dialog, prompt. widgets/ is now strictly bar chips. main.rs's
five overlay::install callsites updated.

No behavior change.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 3: Per-panel moves (Tasks 3–17)

Each task in this phase follows the same pattern. **Execute in alphabetical order** (appearance → audio → bluetooth → calendar → clipboard → connections → displays → media → network → notifications → power → power_menu → settings → stats → vpn) so that `panels/mod.rs` accumulates entries in a deterministic order and conflicts during review are minimized.

Each task uses this template (see Task 3 below for the full first instance; subsequent tasks are condensed since the procedure is identical).

**Per-panel template:**

1. Identify the panel's source range in `pages.rs` by grepping for `pub fn page_<name>`. Note the function plus any private helpers immediately above or below that the panel uses (e.g., `power_action_row` for `power_menu`, `theme_from_index`/`theme_to_index` for `settings`, `IfaceRow` + `build_iface_traffic_row` + `build_traffic_group_v2` + per-pill helpers for `network`).
2. Create `panels/<name>.rs` with:
   - A module docstring explaining the panel's purpose (lift the existing rustdoc on `pub fn page_<name>` if present).
   - `use` declarations for `gtk`, `adw`, `prelude`, the panel's service modules, and the relevant `crate::components::*` helpers.
   - The panel function, renamed `pub fn panel_<name>() -> gtk::Widget`.
   - Any private helpers that move with the panel.
3. Delete the same lines from `pages.rs`.
4. Add `pub mod <name>;` to `panels/mod.rs` and (alongside the first panel-move task) a re-export `pub use <name>::panel_<name>;`.
5. Update `modal.rs`'s `add_named` call from `pages::page_<name>()` to `panel_<name>()` (the function is in scope via `panels::*`).
6. Build clean.
7. Commit.

`crate::modal::Page::*` references inside panels (e.g., the Settings panel's `deep_link_row(..., crate::modal::Page::Appearance)`) keep their fully-qualified paths — no change needed.

The overall import block for a typical panel looks like:

```rust
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::gtk::{self, gdk, gio, glib};
use hytte::prelude::*;
use hytte::services::<service_imports>;

use crate::components::layout::{finish_page, page_box, page_grid, section};
use crate::components::format::{fmt_bytes, fmt_rate};
// + any other components this panel uses
```

Each panel imports only what it actually uses; the above is the maximum.

The 15 panel-move tasks below differ only in:

- The panel name and the source-range to move.
- Which `hytte::services::*` modules to import.
- Which `crate::components::*` items to import.
- The `add_named` line in `modal.rs` to update.

---

### Task 3: Move `panel_appearance`

**Files:**

- Create: `trollshell/src/panels/appearance.rs`
- Modify: `trollshell/src/widgets/pages.rs` (delete `pub fn page_appearance` body and its private helpers)
- Modify: `trollshell/src/panels/mod.rs` (add `pub mod appearance;` + `pub use appearance::panel_appearance;`)
- Modify: `trollshell/src/modal.rs` (update one `add_named` line)

- [ ] **Step 1: Locate the panel source**

Run: `grep -n 'pub fn page_appearance\|fn build_appearance_row\|fn wallpaper_basename' trollshell/src/widgets/pages.rs`
Note the start line of `pub fn page_appearance` and the end of the function block (the `}` matching its opening brace). The appearance panel uses helpers like `wallpaper_basename`, image-picker dialog code; copy any private fns that are referenced ONLY by this panel.

- [ ] **Step 2: Write `panels/appearance.rs`**

Open `trollshell/src/widgets/pages.rs`, copy the full text of `pub fn page_appearance` and any panel-private helpers it depends on. Paste into `trollshell/src/panels/appearance.rs` with:

- Module docstring: copy the existing rustdoc on `pub fn page_appearance` (lift verbatim).
- Imports (use only what this panel actually references):

  ```rust
  use std::cell::RefCell;
  use std::rc::Rc;

  use hytte::adw::{self, prelude::*};
  use hytte::gtk::{self, gio};
  use hytte::prelude::*;
  use hytte::services::wallpaper;

  use crate::components::layout::{finish_page, page_box};
  ```

  (Add or remove imports based on what the source body references.)

- Function rename: `pub fn page_appearance(...)` → `pub fn panel_appearance(...)`.

- [ ] **Step 3: Update `panels/mod.rs`**

Open `trollshell/src/panels/mod.rs`. Replace the empty stub body with:

```rust
//! Drawer pages mounted into `modal.rs`'s per-monitor `gtk::Stack`.
//! Each page is one `pub fn panel_<name>() -> gtk::Widget` re-exported
//! at the module root.

pub mod appearance;

pub use appearance::panel_appearance;
```

- [ ] **Step 4: Strip the panel from `pages.rs`**

Delete the lifted lines (the `pub fn page_appearance` body and any private helpers moved in Step 2) from `trollshell/src/widgets/pages.rs`.

- [ ] **Step 5: Update `modal.rs`**

Open `trollshell/src/modal.rs`. Find:

```rust
    stack.add_named(&pages::page_appearance(), Some(Page::Appearance.stack_name()));
```

Replace with:

```rust
    stack.add_named(&panels::panel_appearance(), Some(Page::Appearance.stack_name()));
```

If `modal.rs` doesn't yet have a `use crate::panels` import, add it near the existing `use crate::widgets::pages;` (which we'll keep until all panels migrate).

- [ ] **Step 6: Build**

Run: `cargo build --workspace --message-format=short 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 7: Run workspace tests**

Run: `cargo test --workspace --message-format=short 2>&1 | grep -E '(test result|FAILED)' | head`
Expected: every `test result: ok.`, no `FAILED`.

- [ ] **Step 8: Commit**

```bash
git add trollshell/src/panels/appearance.rs trollshell/src/panels/mod.rs \
    trollshell/src/widgets/pages.rs trollshell/src/modal.rs
git commit -m "$(cat <<'EOF'
refactor(trollshell): move panel_appearance into panels/

First panel migration. page_appearance → panel_appearance. modal.rs
calls into the new panels:: module; widgets/pages.rs no longer hosts
the appearance panel.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Move `panel_audio`

**Files:**

- Create: `trollshell/src/panels/audio.rs`
- Modify: `trollshell/src/widgets/pages.rs` (delete `pub fn page_audio` body + private helpers)
- Modify: `trollshell/src/panels/mod.rs` (append `pub mod audio;` + `pub use audio::panel_audio;`)
- Modify: `trollshell/src/modal.rs` (update one `add_named` line)

- [ ] **Step 1: Locate the panel source**

Run: `grep -n 'pub fn page_audio' trollshell/src/widgets/pages.rs` and find the closing `}` of the function. Identify any panel-private helpers (build\_\*\_row helpers for streams/sinks if any) that are referenced only by this panel.

- [ ] **Step 2: Write `panels/audio.rs`**

Lift `pub fn page_audio` and its panel-private helpers verbatim from `pages.rs` into `panels/audio.rs`. Rename `page_audio` → `panel_audio`. Imports likely needed:

```rust
use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::{bluetooth_audio, pipewire};

use crate::components::layout::{finish_page, page_box, page_grid, section};
```

(Confirm and adjust based on which symbols the lifted code actually references — particularly for `pipewire::PlaybackStream`, `pipewire::Sink`, etc., add type imports as needed.)

- [ ] **Step 3: Update `panels/mod.rs`**

Add lines so `panels/mod.rs` reads:

```rust
//! Drawer pages mounted into `modal.rs`'s per-monitor `gtk::Stack`.

pub mod appearance;
pub mod audio;

pub use appearance::panel_appearance;
pub use audio::panel_audio;
```

- [ ] **Step 4: Strip from `pages.rs`**

Delete the lifted lines from `trollshell/src/widgets/pages.rs`.

- [ ] **Step 5: Update `modal.rs`**

Find:

```rust
    stack.add_named(&pages::page_audio(), Some(Page::Audio.stack_name()));
```

Replace with:

```rust
    stack.add_named(&panels::panel_audio(), Some(Page::Audio.stack_name()));
```

- [ ] **Step 6: Build, test, commit**

```bash
cargo build --workspace --message-format=short 2>&1 | tail -5
cargo test --workspace --message-format=short 2>&1 | grep FAILED | head
git add trollshell/src/panels/audio.rs trollshell/src/panels/mod.rs \
    trollshell/src/widgets/pages.rs trollshell/src/modal.rs
git commit -m "$(cat <<'EOF'
refactor(trollshell): move panel_audio into panels/

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Move `panel_bluetooth`

**Files:**

- Create: `trollshell/src/panels/bluetooth.rs`
- Modify: `pages.rs`, `panels/mod.rs`, `modal.rs`

- [ ] **Step 1:** `grep -n 'pub fn page_bluetooth\|fn build_pair_prompt\|fn build_device_row\|fn populate_pair_prompt\|fn submit_entry\|fn build_yes_no_row\|fn build_text_entry_row\|fn build_device_menu' trollshell/src/widgets/pages.rs` — `panel_bluetooth` has many private helpers; lift them all together (~440 LOC).
- [ ] **Step 2:** Write `panels/bluetooth.rs` lifting `pub fn page_bluetooth` + all bluetooth-private helpers (`build_bluetooth_header`, `build_bluetooth_controls`, `build_bluetooth_device_groups`, `build_pair_prompt_banner`, `populate_pair_prompt`, `build_yes_no_row`, `build_text_entry_row`, `submit_entry`, `build_device_row`, `build_device_menu`). Rename `page_bluetooth` → `panel_bluetooth`. Imports:

```rust
use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::bluetooth::{self, Device, PairPrompt, PromptKind};

use crate::components::layout::{finish_page, page_box};
```

- [ ] **Step 3:** Append to `panels/mod.rs`:

```rust
pub mod bluetooth;
…
pub use bluetooth::panel_bluetooth;
```

- [ ] **Step 4:** Strip from `pages.rs`.
- [ ] **Step 5:** Update `modal.rs`: `pages::page_bluetooth()` → `panels::panel_bluetooth()`.
- [ ] **Step 6:** Build / test / commit:

```bash
cargo build --workspace --message-format=short 2>&1 | tail -5
cargo test --workspace --message-format=short 2>&1 | grep FAILED | head
git add trollshell/src/panels/bluetooth.rs trollshell/src/panels/mod.rs \
    trollshell/src/widgets/pages.rs trollshell/src/modal.rs
git commit -m "refactor(trollshell): move panel_bluetooth into panels/

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Move `panel_calendar`

- [ ] **Step 1:** `grep -n 'pub fn page_calendar' trollshell/src/widgets/pages.rs`. Find the closing `}`. Calendar is small (~250 LOC).
- [ ] **Step 2:** Lift `pub fn page_calendar` + private helpers into `trollshell/src/panels/calendar.rs`. Rename to `panel_calendar`. Imports:

```rust
use chrono::{DateTime, Datelike, Local};

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::calendar::{self, CalendarEvent};

use crate::components::format::humanize_since;
use crate::components::layout::{finish_page, page_box};
```

- [ ] **Step 3:** Append to `panels/mod.rs`: `pub mod calendar;` and `pub use calendar::panel_calendar;`.
- [ ] **Step 4:** Strip from `pages.rs`.
- [ ] **Step 5:** Update `modal.rs`: `pages::page_calendar()` → `panels::panel_calendar()`.
- [ ] **Step 6:** Build / test / commit (same shape as Task 5).

---

### Task 7: Move `panel_clipboard`

- [ ] **Step 1:** `grep -n 'pub fn page_clipboard' trollshell/src/widgets/pages.rs`.
- [ ] **Step 2:** Lift into `panels/clipboard.rs` + rename. Imports:

```rust
use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::clipboard::{self, ClipEntry, ClipKind};

use crate::components::layout::{finish_page, page_box};
```

- [ ] **Step 3:** Append `pub mod clipboard;` + `pub use clipboard::panel_clipboard;` to `panels/mod.rs`.
- [ ] **Step 4-6:** Strip, update modal.rs, build/test/commit.

---

### Task 8: Move `panel_connections`

- [ ] **Step 1:** `grep -n 'pub fn page_connections' trollshell/src/widgets/pages.rs`.
- [ ] **Step 2:** Lift into `panels/connections.rs` + rename. Imports:

```rust
use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::netconn;

use crate::components::connection_row::{build_connection_row, CONN_BUCKET_CAP};
use crate::components::layout::{finish_page, page_box};
```

- [ ] **Step 3:** Append entry to `panels/mod.rs`.
- [ ] **Step 4-6:** Strip / modal.rs update / build / commit.

---

### Task 9: Move `panel_displays`

- [ ] **Step 1:** `grep -n 'pub fn page_displays' trollshell/src/widgets/pages.rs`.
- [ ] **Step 2:** Lift into `panels/displays.rs` + private helpers. Rename. Imports:

```rust
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::displays::{self, Output};

use crate::components::layout::{finish_page, page_box};
```

- [ ] **Step 3:** Append entry to `panels/mod.rs`.
- [ ] **Step 4-6:** Strip / modal.rs update / build / commit.

---

### Task 10: Move `panel_media`

- [ ] **Step 1:** `grep -n 'pub fn page_media' trollshell/src/widgets/pages.rs`.
- [ ] **Step 2:** Lift into `panels/media.rs` + rename. Imports:

```rust
use std::cell::Cell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::mpris::{self, PlaybackStatus};

use crate::components::format::fmt_us;
use crate::components::layout::{finish_page, page_grid, section};
```

(`page_media` calls `panel("")` for the art panel — confirm `section("")` works the same way; the helper just builds an empty section box.)

- [ ] **Step 3:** Append entry to `panels/mod.rs`.
- [ ] **Step 4-6:** Strip / modal.rs update / build / commit.

---

### Task 11: Move `panel_network`

- [ ] **Step 1:** `grep -n 'pub fn page_network\|fn build_connection_group_v2\|fn build_primary_expander\|fn build_no_connection_placeholder_row\|fn build_all_links_expander\|fn build_link_state_pill\|fn state_pill_text\|fn state_pill_class\|fn build_dns_expander\|fn build_traffic_group_v2\|fn build_iface_traffic_row\|^struct IfaceRow\|fn build_wifi_group_v2\|fn build_wifi_header_suffix\|fn wifi_description_text\|fn dbm_label\|fn build_network_row_v2\|fn build_network_row_menu\|fn pill_label\|fn security_label\|fn signal_icon\|fn describe_state' trollshell/src/widgets/pages.rs` — `panel_network` is the largest (~700 LOC) with many private helpers (network sub-builders, pill helpers, wifi sub-builders).
- [ ] **Step 2:** Lift `pub fn page_network` + ALL its private helpers (the entire network panel block including the `IfaceRow` struct) into `trollshell/src/panels/network.rs`. Rename `page_network` → `panel_network`. Imports:

```rust
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::{netconn, networkd::{self, OperationalState}, sensors, wifi};
use hytte::ui::Sparkline;

use crate::components::deep_link_row::deep_link_row;
use crate::components::format::{fmt_bytes, fmt_rate};
use crate::components::history_row::build_history_row;
use crate::components::layout::{finish_page, page_box, page_grid, section};
```

- [ ] **Step 3:** Append entry to `panels/mod.rs`.
- [ ] **Step 4-6:** Strip / modal.rs update / build / commit.

---

### Task 12: Move `panel_notifications`

- [ ] **Step 1:** `grep -n 'pub fn page_notifications\|fn build_history_app_row\|fn build_history_action_row' trollshell/src/widgets/pages.rs`. Lift the panel and the two history helpers (`build_history_app_row`, `build_history_action_row`) into the new file.
- [ ] **Step 2:** Lift into `panels/notifications.rs` + rename. Imports:

```rust
use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::{dnd, notifications, notifications_mute};

use crate::components::layout::{finish_page, page_box};
```

- [ ] **Step 3:** Append entry to `panels/mod.rs`.
- [ ] **Step 4-6:** Strip / modal.rs update / build / commit.

---

### Task 13: Move `panel_power`

- [ ] **Step 1:** `grep -n 'pub fn page_power' trollshell/src/widgets/pages.rs`. Skip the match for `pub fn page_power_menu` — that's the next task.
- [ ] **Step 2:** Lift `pub fn page_power` + private helpers into `panels/power.rs`. Rename. Imports:

```rust
use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::{brightness, power_profiles::{self, humanize_profile}, screensaver, upower::{self, Battery, BatteryState}};

use crate::components::layout::{finish_page, page_box};
```

- [ ] **Step 3:** Append entry to `panels/mod.rs`.
- [ ] **Step 4-6:** Strip / modal.rs update / build / commit.

---

### Task 14: Move `panel_power_menu`

- [ ] **Step 1:** `grep -n 'pub fn page_power_menu\|fn power_action_row' trollshell/src/widgets/pages.rs`. Lift both — `power_action_row` is private to power_menu.
- [ ] **Step 2:** Lift into `panels/power_menu.rs` + rename `page_power_menu` → `panel_power_menu`. Imports:

```rust
use hytte::adw::{self, prelude::*};
use hytte::gtk::prelude::*;
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::{logind, niri, screensaver};

use crate::components::layout::{finish_page, page_box};
```

- [ ] **Step 3:** Append entry to `panels/mod.rs`.
- [ ] **Step 4-6:** Strip / modal.rs update / build / commit.

---

### Task 15: Move `panel_settings`

- [ ] **Step 1:** `grep -n 'pub fn page_settings\|fn theme_from_index\|fn theme_to_index' trollshell/src/widgets/pages.rs`. Lift the panel and the two theme helpers.
- [ ] **Step 2:** Lift into `panels/settings.rs` + rename. Imports:

```rust
use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::{dnd, theme};

use crate::components::deep_link_row::deep_link_row;
use crate::components::layout::{finish_page, page_box};
```

(The theme dropdown helpers reference `hytte::services::theme::Theme`; confirm import.)

- [ ] **Step 3:** Append entry to `panels/mod.rs`.
- [ ] **Step 4-6:** Strip / modal.rs update / build / commit.

---

### Task 16: Move `panel_stats`

- [ ] **Step 1:** `grep -n 'pub fn page_stats\|fn build_stats_history_group\|fn build_history_cpu_row\|fn build_history_memory_row\|fn build_history_network_row\|fn build_history_gpu_temp_row\|fn build_stats_live_group_v2\|fn build_live_cpu_row\|fn build_live_per_core_row\|fn build_live_memory_row\|fn build_live_swap_row\|fn build_live_processes_row\|fn build_live_gpu_row\|fn build_live_disk_expander' trollshell/src/widgets/pages.rs` — stats has many private helpers.
- [ ] **Step 2:** Lift `pub fn page_stats` + all stats-private helpers into `panels/stats.rs`. Rename. Imports:

```rust
use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::{sensors::{self, CpuLoad}, systemd};

use crate::components::format::fmt_bytes;
use crate::components::history_row::build_history_row;
use crate::components::layout::{finish_page, page_box};
```

- [ ] **Step 3:** Append entry to `panels/mod.rs`.
- [ ] **Step 4-6:** Strip / modal.rs update / build / commit.

---

### Task 17: Move `panel_vpn`

- [ ] **Step 1:** `grep -n 'pub fn page_vpn\|fn build_tunnel_group\|fn build_peer_row' trollshell/src/widgets/pages.rs`. Lift the panel + two private helpers (`build_tunnel_group`, `build_peer_row`).
- [ ] **Step 2:** Lift into `panels/vpn.rs` + rename. Imports:

```rust
use std::cell::RefCell;
use std::rc::Rc;
use std::time::SystemTime;

use hytte::adw::{self, prelude::*};
use hytte::gtk::prelude::*;
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::vpn;

use crate::components::format::{fmt_bytes, humanize_since};
use crate::components::layout::{finish_page, page_box};
```

- [ ] **Step 3:** Append entry to `panels/mod.rs`.
- [ ] **Step 4-6:** Strip / modal.rs update / build / commit.

---

## Task 18: Delete the now-empty `widgets/pages.rs`

**Files:**

- Delete: `trollshell/src/widgets/pages.rs`
- Modify: `trollshell/src/widgets/mod.rs` — drop `pub mod pages;`
- Modify: `trollshell/src/modal.rs` — drop `use crate::widgets::pages;` if any remains.

- [ ] **Step 1: Confirm `pages.rs` is empty (modulo imports / module separators)**

Run: `grep -n 'pub fn page_\|pub fn panel_' trollshell/src/widgets/pages.rs`
Expected: zero hits. The file may still have a few orphan imports or a module-level docstring.

- [ ] **Step 2: Delete the file**

```bash
git rm trollshell/src/widgets/pages.rs
```

- [ ] **Step 3: Update `widgets/mod.rs`**

Open `trollshell/src/widgets/mod.rs`. Remove the line `pub mod pages;`.

- [ ] **Step 4: Update `modal.rs`**

Open `trollshell/src/modal.rs`. Remove any `use crate::widgets::pages;` import (if present). All `pages::*` references should already be gone after Tasks 3–17; if any remain, those tasks were incomplete — go back.

- [ ] **Step 5: Build**

Run: `cargo build --workspace --message-format=short 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 6: Run clippy**

Run: `cargo clippy --workspace --message-format=short 2>&1 | tail -20`
Expected: only the pre-existing `mpris.rs:23` doc-backticks warning. Nothing new.

- [ ] **Step 7: Run workspace tests**

Run: `cargo test --workspace --message-format=short 2>&1 | grep -E '(test result|FAILED)' | head`
Expected: every `test result: ok.`, no `FAILED`.

- [ ] **Step 8: Commit**

```bash
git add trollshell/src/widgets/mod.rs trollshell/src/modal.rs
git commit -m "$(cat <<'EOF'
refactor(trollshell): delete the empty widgets/pages.rs

All 15 panels have moved into panels/. The 3870-line file is gone;
widgets/ is strictly bar chips, panels/ owns the drawer pages,
components/ owns cross-cutting helpers, overlays/ owns the layer-shell
floats. End of the reorg.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

Spec coverage check (against `docs/superpowers/specs/2026-04-29-trollshell-src-reorg-design.md`, "Scope > In scope"):

- ✅ New `panels/` dir with 15 panel files + `mod.rs` re-exports — Tasks 3–17 + scaffold in Task 1.
- ✅ New `components/` dir with 5 helper files + `mod.rs` — Task 1.
- ✅ New `overlays/` dir with 5 overlay files moved via `git mv` + `mod.rs` — Task 2 + scaffold in Task 1.
- ✅ `widgets/pages.rs` deleted — Task 18.
- ✅ `widgets/util.rs` absorbed into `components/format.rs` — Task 1.
- ✅ Per-page private helpers move with their panel — every Task 3–17 explicitly identifies private helpers in Step 1 grep.
- ✅ `page_*` → `panel_*` rename for all 15 fns — every Task 3–17 Step 2.
- ✅ `panel(title)` → `section(title)` rename — Task 1 Step 6.
- ✅ `widgets/mod.rs` cleanup (drops `pub mod pages;`, `mod util;`, 5 overlay mods) — Task 1 Step 7 (util), Task 2 Step 3 (overlays), Task 18 Step 3 (pages).
- ✅ `main.rs` declares `mod components/panels/overlays` — Task 1 Step 3.
- ✅ `main.rs` updates 5 overlay::install callsites — Task 2 Step 4.
- ✅ `modal.rs` updates 15 `add_named` calls — Tasks 3–17 each Step 5.
- ✅ Build green at every commit boundary — every task ends with build/test/commit.

No placeholders. Type and helper names consistent (`section` lowercase, `panel_<name>` lowercase, `CONN_BUCKET_CAP` upper-snake). Function references (`build_history_row`, `deep_link_row`, etc.) cross-checked against the current `pages.rs` line numbers (run grep at the start of every task as a guard against drift).
