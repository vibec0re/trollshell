# Integrated bar + rounded workspace frame — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the trollshell bar a flush full-width strip and add a new `OVERLAY`-layer frame that paints the bar's dark gradient as L/R/bottom borders with rounded inner corners around the workspace cutout, producing the card-on-dark look from `design/mock01.png`.

**Architecture:** Three layers. `BACKGROUND` = swaybg (unchanged). `TOP` = existing `Bar`, restyled to `margin: 0; border-radius: 0;`. `OVERLAY` = new per-monitor `frame` overlay window — full-screen, click-through, paints the dark gradient in the L/R/bottom border regions plus the four rounded interior corners of the workspace cutout. niri reserves L/R/bottom via a static config snippet (`struts`); the bar's existing exclusive zone reserves the top.

**Tech Stack:** Rust 2024, GTK4 + libadwaita via `hytte` toolkit, `gtk4-layer-shell`, cairo via `gtk::DrawingArea`, niri compositor.

**Spec:** `docs/superpowers/specs/2026-05-06-bar-frame-rounding-design.md`

**Visual constants (used throughout):**
- `BAR_HEIGHT = 44` (post-restyle, computed from `padding: 6px 12px` + `min-height: 32px`, no margin)
- `FRAME_THICKNESS = 12` (left, right, bottom inset around the workspace cutout)
- `CUTOUT_RADIUS = 16` (corner radius on all four cutout corners)
- Frame gradient: same as bar background — `linear-gradient(90deg, rgba(15,15,35,1) 0%, rgba(25,15,45,1) 50%, rgba(15,15,35,1) 100%)`, aligned to screen width

---

## File structure

| File | Status | Responsibility |
|---|---|---|
| `trollshell/style.css` | Modify (lines 36-47) | flatten `.hytte-bar-content` (margin / border-radius / box-shadow) |
| `trollshell/src/overlays/frame.rs` | Create | per-monitor OVERLAY-layer window + cairo draw fn for the frame shape |
| `trollshell/src/overlays/mod.rs` | Modify | `pub mod frame;` |
| `trollshell/src/main.rs` | Modify (run-callback's per-monitor loop, around line 85) | `overlays::frame::install(monitor);` |
| `etc/niri/frame.kdl` | Create | `layout { struts { left 12; right 12; bottom 12 } }` snippet |
| `etc/niri/README.md` | Modify | new "Frame struts" section explaining how to merge the snippet |

No new crate dependencies. cairo and `gtk4-layer-shell` are already in the dependency tree (used by the bar and existing overlays).

---

## Tasks

### Task 1: Bar restyle (CSS only, build verification)

**Files:**
- Modify: `trollshell/style.css:36-47`

- [ ] **Step 1: Apply the CSS change**

In `trollshell/style.css`, replace the current `.hytte-bar-content` block (lines 36-47):

```css
.hytte-bar-content {
    padding: 6px 12px;
    margin: 5px 5px;
    margin-bottom: 10px;
    min-height: 32px;
    border-radius: 12px;
    background: linear-gradient(90deg,
        rgba(15, 15, 35, 1) 0%,
        rgba(25, 15, 45, 1) 50%,
        rgba(15, 15, 35, 1) 100%);
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.9);
}
```

with:

```css
.hytte-bar-content {
    padding: 6px 12px;
    margin: 0;
    min-height: 32px;
    border-radius: 0;
    background: linear-gradient(90deg,
        rgba(15, 15, 35, 1) 0%,
        rgba(25, 15, 45, 1) 50%,
        rgba(15, 15, 35, 1) 100%);
}
```

Leave `.hytte-bar.drawer-open .hytte-bar-content { border-bottom-right-radius: 0; }` untouched — it's a no-op now but stays for symmetry if rounding is reintroduced later.

- [ ] **Step 2: Build to confirm CSS still parses (Rust embeds it via `concat!(env!("CARGO_MANIFEST_DIR"), "/style.css")` in main.rs, so a build is the lightest verification gate)**

Run: `cargo build -p trollshell`
Expected: build succeeds with no errors.

- [ ] **Step 3: Commit**

```bash
git add trollshell/style.css
git commit -m "feat(bar): flatten bar — drop margin, border-radius, box-shadow"
```

---

### Task 2: Add `frame` module skeleton with empty `install` + wire it

**Files:**
- Create: `trollshell/src/overlays/frame.rs`
- Modify: `trollshell/src/overlays/mod.rs`
- Modify: `trollshell/src/main.rs` (per-monitor overlay install loop)

This task wires the module into the build with a no-op `install` so the rest of the work can iterate in isolation. No drawing yet.

- [ ] **Step 1: Create the new module file**

Create `trollshell/src/overlays/frame.rs`:

```rust
//! Per-monitor OVERLAY-layer window that paints the dark frame around
//! the workspace cutout. Full-screen, click-through, no exclusive zone.
//!
//! Layered above the bar (which is on `Layer::Top`). Bar widgets remain
//! interactive because the frame's input region is empty — every click
//! falls through to the layer below.
//!
//! The frame paints the bar's dark gradient (3-stop, 90deg, screen-width
//! aligned) into the L/R/bottom border regions and carves four rounded
//! inner corners around the workspace cutout. Top inset is the bar's
//! exclusive zone.
//!
//! Visual constants — match `etc/niri/frame.kdl` struts and the
//! post-restyle bar geometry from `style.css`. If any of these change,
//! update both sides.

use hytte::gtk;
use hytte::prelude::*;
use hytte::ui::layer_window;
use gtk4_layer_shell::{KeyboardMode, Layer};

/// Bar height after restyle: `padding: 6px 12px` (12 vertical) + `min-height: 32px` = 44.
/// Top inset of the frame (= top of the workspace cutout).
const BAR_HEIGHT: f64 = 44.0;

/// Frame thickness on left, right, and bottom. Must match the niri
/// `struts` values in `etc/niri/frame.kdl`.
const FRAME_THICKNESS: f64 = 12.0;

/// Corner radius for all four corners of the workspace cutout.
const CUTOUT_RADIUS: f64 = 16.0;

/// Mount one frame overlay on `monitor`.
pub fn install(_monitor: &Monitor) {
    // Skeleton — implementation lands in tasks 3 and 4.
}
```

- [ ] **Step 2: Register the module**

In `trollshell/src/overlays/mod.rs`, add `pub mod frame;` alongside the existing modules. The full file becomes:

```rust
//! Per-monitor layer-shell overlays — lock screen, OSD, dialogs, toast.
//! Each module exposes a `pub fn install(...)` that wires the overlay
//! to the relevant signal source. Moved out of `widgets/` so that
//! `widgets/` reads strictly as bar chips.

pub mod frame;
pub mod lock_screen;
pub mod notifications;
pub mod osd;
pub mod polkit_dialog;
pub mod prompt;
```

- [ ] **Step 3: Mount the overlay in main.rs**

In `trollshell/src/main.rs`, find the per-monitor install loop (around lines 85-88, where `notifications::install` and `osd::install` are called). Add `overlays::frame::install(monitor);` as the FIRST overlay installed in the loop (rationale: it is always-on and visually behind transient overlays; install order documents that intent even though z-order between separate layer-shell windows on the same layer is implementation-defined).

```rust
// Notifications + OSD mount on every monitor; routing picks the focused one.
for monitor in &app.monitors() {
    overlays::frame::install(monitor);
    overlays::notifications::install(monitor);
    overlays::osd::install(monitor);
}
```

- [ ] **Step 4: Build to confirm wiring**

Run: `cargo build -p trollshell`
Expected: build succeeds.

- [ ] **Step 5: Commit**

```bash
git add trollshell/src/overlays/frame.rs trollshell/src/overlays/mod.rs trollshell/src/main.rs
git commit -m "feat(overlays): scaffold frame overlay module + install hook"
```

---

### Task 3: Implement layer-shell window plumbing (no drawing yet)

**Files:**
- Modify: `trollshell/src/overlays/frame.rs`

Stand up the OVERLAY-layer window with the right anchors, layer, exclusive-zone-off, click-through input region. Render a fully transparent `gtk::DrawingArea` so we can confirm the window mounts without affecting anything visually.

- [ ] **Step 1: Replace the stub `install` with the layer-shell setup**

Replace the body of `install` in `trollshell/src/overlays/frame.rs`:

```rust
use hytte::gtk::{self, prelude::*};

pub fn install(monitor: &Monitor) {
    let window = layer_window(monitor)
        .layer(Layer::Overlay)
        .anchor(hytte::ui::Anchor::Top)
        .anchor(hytte::ui::Anchor::Bottom)
        .anchor(hytte::ui::Anchor::Left)
        .anchor(hytte::ui::Anchor::Right)
        .namespace("hytte-frame")
        .exclusive(false)
        .keyboard_mode(KeyboardMode::None)
        .build();
    window.add_css_class("ts-frame");

    // Transparent drawing area — fills the layer-shell surface. Drawing
    // body lands in Task 4.
    let area = gtk::DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    window.set_child(Some(&area));

    // Empty input region: clicks pass through to the bar (Layer::Top below)
    // and to niri's apps (normal layer below that). Set after realize so
    // the surface exists.
    install_click_through(&window);

    window.set_visible(true);
}

/// Set an empty input region on the window's surface so every pointer
/// event falls through to the layer below. Layer-shell does not give
/// us this directly; we go through the underlying `GdkSurface` once
/// it's realized.
fn install_click_through(window: &gtk::Window) {
    use hytte::gtk::cairo;
    use hytte::gtk::gdk;

    window.connect_realize(|w| {
        if let Some(surface) = w.surface() {
            // An empty cairo region == no pointer area == fully click-through.
            let empty = cairo::Region::create();
            surface.set_input_region(&empty);
        } else {
            tracing::warn!("frame: window has no surface at realize");
        }
    });
}
```

Add the missing imports at the top of the file:

```rust
use hytte::gtk::{self, prelude::*};
```

(Replace the existing `use hytte::gtk;` line.)

- [ ] **Step 2: Build**

Run: `cargo build -p trollshell`
Expected: build succeeds. If `cairo::Region::create` is not in scope, the import path may be `gtk::cairo::Region`; the test below catches that.

- [ ] **Step 3: Run trollshell and confirm nothing breaks visually**

Run: `RUST_LOG=trollshell=debug cargo run -p trollshell` from inside a niri session.
Expected:
- Bar appears as a flush full-width strip at the top (Task 1's CSS change is now visible).
- No new visual artifacts; no panics on startup.
- `niri msg layers` (or equivalent) shows a layer named `hytte-frame` on the OVERLAY layer with full-screen geometry.
- Clicks anywhere outside the bar still reach apps (the empty input region works).

Stop the process when satisfied.

- [ ] **Step 4: Commit**

```bash
git add trollshell/src/overlays/frame.rs
git commit -m "feat(frame): mount overlay-layer window with click-through input region"
```

---

### Task 4: Cairo draw — paint the dark frame with rounded cutout

**Files:**
- Modify: `trollshell/src/overlays/frame.rs`

This is the visual payload. Wire a `set_draw_func` on the `DrawingArea` that paints the dark gradient into the frame region (everything below `BAR_HEIGHT` minus the rounded cutout) using cairo's even-odd fill rule.

- [ ] **Step 1: Add a unit test for the cutout-rect helper (TDD)**

At the bottom of `trollshell/src/overlays/frame.rs`, add a pure helper that returns the cutout rectangle's bounds for a given monitor size, plus a test. Tests can live in the same file with a `#[cfg(test)]` module.

```rust
/// Cutout bounds for a monitor of size (`width`, `height`). The cutout
/// is the rounded transparent rectangle inside which apps tile. Returns
/// `(x, y, w, h)` of the cutout's bounding box (corner radius applied
/// at draw time, not in this rect).
fn cutout_rect(width: f64, height: f64) -> (f64, f64, f64, f64) {
    let x = FRAME_THICKNESS;
    let y = BAR_HEIGHT;
    let w = (width - 2.0 * FRAME_THICKNESS).max(0.0);
    let h = (height - BAR_HEIGHT - FRAME_THICKNESS).max(0.0);
    (x, y, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutout_rect_normal_monitor() {
        // 1920x1080: bar 44 + bottom inset 12 + L/R inset 12 each.
        let (x, y, w, h) = cutout_rect(1920.0, 1080.0);
        assert_eq!(x, 12.0);
        assert_eq!(y, 44.0);
        assert_eq!(w, 1920.0 - 24.0);
        assert_eq!(h, 1080.0 - 44.0 - 12.0);
    }

    #[test]
    fn cutout_rect_tiny_monitor_clamps_to_zero() {
        // Pathological tiny monitor: cutout would be negative; clamp to 0
        // to avoid passing negative dimensions into cairo.
        let (_x, _y, w, h) = cutout_rect(20.0, 30.0);
        assert_eq!(w, 0.0);
        assert_eq!(h, 0.0);
    }
}
```

- [ ] **Step 2: Run the test, confirm it passes**

Run: `cargo test -p trollshell --lib overlays::frame::tests`
Expected: 2 tests pass.

(If the test path differs because `trollshell` is a binary crate, run `cargo test -p trollshell -- frame` instead and confirm both `cutout_rect_*` tests appear in the output.)

- [ ] **Step 3: Implement the draw function**

After the existing `install` function and `install_click_through`, add the draw setup. Modify `install` to call it on the `DrawingArea`:

In `install`, after `window.set_child(Some(&area));`, insert:

```rust
    install_draw(&area);
```

Then add the function below:

```rust
fn install_draw(area: &gtk::DrawingArea) {
    use hytte::gtk::cairo;

    area.set_draw_func(move |_area, cr: &cairo::Context, width: i32, height: i32| {
        let w = f64::from(width);
        let h = f64::from(height);

        // Skip if the area is too small to contain the bar + bottom inset.
        if h <= BAR_HEIGHT + FRAME_THICKNESS || w <= 2.0 * FRAME_THICKNESS {
            return;
        }

        let (cx, cy, cw, ch) = cutout_rect(w, h);
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }

        // Build a path with two sub-paths: the outer "frame region" rect
        // (everything below the bar), and the rounded cutout. Fill with
        // EvenOdd so the cutout is excluded.
        cr.set_fill_rule(cairo::FillRule::EvenOdd);

        // Outer region: from (0, BAR_HEIGHT) to (w, h). Bar area above is
        // left untouched (transparent), so the bar paints its own gradient.
        cr.rectangle(0.0, BAR_HEIGHT, w, h - BAR_HEIGHT);

        // Inner cutout: rounded rect at (cx, cy) of size (cw, ch).
        rounded_rect(cr, cx, cy, cw, ch, CUTOUT_RADIUS);

        // Source: 3-stop horizontal gradient matching the bar's CSS,
        // aligned to the full screen width so the bar's gradient and the
        // frame's L/R borders are continuous at every x.
        let gradient = cairo::LinearGradient::new(0.0, 0.0, w, 0.0);
        gradient.add_color_stop_rgba(0.0, 15.0 / 255.0, 15.0 / 255.0, 35.0 / 255.0, 1.0);
        gradient.add_color_stop_rgba(0.5, 25.0 / 255.0, 15.0 / 255.0, 45.0 / 255.0, 1.0);
        gradient.add_color_stop_rgba(1.0, 15.0 / 255.0, 15.0 / 255.0, 35.0 / 255.0, 1.0);

        if let Err(e) = cr.set_source(&gradient) {
            tracing::warn!(error = %e, "frame: failed to set gradient source");
            return;
        }
        if let Err(e) = cr.fill() {
            tracing::warn!(error = %e, "frame: cairo fill failed");
        }
    });
}

/// Trace a closed rounded-rectangle sub-path of size (`w`, `h`) at (`x`, `y`)
/// with corner radius `r`, on the given cairo context. Does not stroke or fill.
fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r,        r, -PI / 2.0,        0.0); // top-right
    cr.arc(x + w - r, y + h - r,    r, 0.0,              PI / 2.0); // bottom-right
    cr.arc(x + r,     y + h - r,    r, PI / 2.0,         PI); // bottom-left
    cr.arc(x + r,     y + r,        r, PI,               1.5 * PI); // top-left
    cr.close_path();
}
```

Note: GTK4's drawing-area sets a transparent default surface; the area below `BAR_HEIGHT` and inside the cutout stays transparent automatically since we never paint there. The bar's own gradient (on `Layer::Top`) shows through the bar region; niri's apps + wallpaper show through the cutout.

- [ ] **Step 4: Build**

Run: `cargo build -p trollshell`
Expected: build succeeds. If a method name (e.g., `set_draw_func`, `rectangle`, `arc`, `set_fill_rule`) doesn't resolve, double-check the cairo and gtk-rs imports — `gtk::DrawingArea::set_draw_func`, `cairo::Context::rectangle/arc/set_fill_rule`, `cairo::LinearGradient::new`/`add_color_stop_rgba`, `cairo::Region::create` are all stable and present in the versions used elsewhere in the repo.

- [ ] **Step 5: Run trollshell and visually verify**

Run: `RUST_LOG=trollshell=debug cargo run -p trollshell` inside a niri session.

Expected:
- Bar is a flush strip at top (no margin, no rounded corners).
- Below the bar, a dark gradient strip runs along the left edge (12px wide), the right edge (12px wide), and the bottom (12px tall), all in the same dark color as the bar.
- At the four corners of the workspace area (just below the bar's L/R, and at the bottom L/R), there's visible rounded-corner shaping — the dark frame curves inward, leaving a rounded transparent cutout where the wallpaper / apps appear.
- No tracing warnings about gradient or fill failures.
- Resize / move a niri window into a corner: the corner appears clipped by the frame's rounded edge.

Stop the process when satisfied.

- [ ] **Step 6: Commit**

```bash
git add trollshell/src/overlays/frame.rs
git commit -m "feat(frame): paint dark gradient with rounded cutout via cairo"
```

---

### Task 5: niri config snippet + README

**Files:**
- Create: `etc/niri/frame.kdl`
- Modify: `etc/niri/README.md`

Until the user merges these struts, niri windows will tile under the frame's L/R/bottom borders and look clipped. The snippet documents the pairing.

- [ ] **Step 1: Create the niri snippet**

Create `etc/niri/frame.kdl`:

```kdl
// Inset the niri tiling area to match trollshell's frame overlay.
// Top inset is reserved by the bar's exclusive zone — only L/R/bottom
// need struts here. Numbers MUST match
// trollshell/src/overlays/frame.rs::FRAME_THICKNESS (12.0).

layout {
    struts {
        left 12
        right 12
        bottom 12
    }
}
```

- [ ] **Step 2: Add a "Frame struts" section to `etc/niri/README.md`**

Append to `etc/niri/README.md` (after the existing media-bind sections), keeping the same headings style:

```markdown
## Frame struts

trollshell's frame overlay (added in 2026-05-06) draws a dark gradient
border around the workspace and rounds the inner corners. For the
border to align with niri's tiling area, niri needs matching struts on
the left, right, and bottom — top is already reserved by the bar's
exclusive zone.

The snippet at `etc/niri/frame.kdl` defines those struts. niri does
**not** support `include`, so the snippet has to be merged into your
config by hand. Two cases:

### You don't have a `layout { }` block yet

Open `~/.config/niri/config.kdl` and paste the entire `layout { … }`
block from `etc/niri/frame.kdl` near the top of the file (or anywhere
at the top level). Reload niri (it picks up config changes
automatically; otherwise `niri msg action reload-config`).

### You already have a `layout { }` block

Copy only the `struts { … }` sub-block into your existing `layout { }`.
If you already have a `struts { }` block of your own, merge values: any
existing inset on left / right / bottom should be the larger of the two,
or 12 to match the frame.

### Verification

1. Restart trollshell (or wait for auto-reload). The bar should be a
   flush full-width strip at top, with a 12px dark border on the left,
   right, and bottom of the workspace and rounded corners on all four
   sides of the cutout.
2. Open a window and snap it into a corner. The window's edge should
   stop 12px inside the screen edge (struts working) and the visible
   corner should appear rounded (frame overlay working).
3. If the window touches the screen edge, the strut isn't in effect —
   re-check the merged `layout { }` block.

### Tuning

Both numbers (frame thickness in trollshell, struts in niri) MUST match.
If you change one, change the other:

- niri: `etc/niri/frame.kdl` → `struts { left N; right N; bottom N }`
- trollshell: `trollshell/src/overlays/frame.rs::FRAME_THICKNESS`
```

- [ ] **Step 3: Verify the snippet by merging it locally and reloading niri (manual)**

Manually merge the snippet into `~/.config/niri/config.kdl`, then run `niri msg action reload-config`. Open a test window. Confirm it tiles 12px inside each edge of the inner area (no longer reaches the screen borders below or beside). Trollshell's frame overlay should align cleanly with the strut edges.

- [ ] **Step 4: Commit**

```bash
git add etc/niri/frame.kdl etc/niri/README.md
git commit -m "docs(niri): ship frame-struts snippet + merge instructions"
```

---

### Task 6: Final integration check

**Files:** none modified

A consolidated visual + behavioral check before declaring done.

- [ ] **Step 1: Clean build**

Run: `cargo build -p trollshell --release`
Expected: succeeds.

- [ ] **Step 2: Clippy**

Run: `cargo clippy -p trollshell --all-targets -- -D warnings`
Expected: no warnings. Fix any that come up (likely candidates: unused imports if cairo wasn't already pulled in; `#[allow(dead_code)]` should not be needed).

- [ ] **Step 3: Tests**

Run: `cargo test -p trollshell`
Expected: existing tests still pass; the two `cutout_rect_*` tests added in Task 4 pass.

- [ ] **Step 4: Manual smoke test**

In a niri session with the frame.kdl snippet merged:

1. Launch trollshell. Bar is flat full-width.
2. Border is visible on L / R / bottom in the bar's dark gradient color.
3. All four corners of the workspace cutout are visibly rounded.
4. Open an app: it tiles inside the cutout, with corners visually clipped by the frame.
5. Click on bar widgets: they react. Click in the dark border area: the click does not get eaten (passes through, niri receives it).
6. Open the drawer (settings/power chip). It pops down from the bar's bottom-right; the seam stays clean. Close it.
7. Lock and unlock the screen. The frame stays consistent through lock screen mount/unmount.
8. If a second monitor is available, hot-plug it. The frame mounts on the new monitor; hot-unplug cleanly closes it.

- [ ] **Step 5: Final verification**

`git status` should be clean. Run `git log --oneline -10` to confirm five new commits with conventional-commit-style messages.

No commit at this step — verification only.

---

## Self-review notes

- All five spec components (bar restyle, frame overlay window, frame draw, niri snippet, README) are covered by tasks 1–5; task 6 is a final integration gate.
- All visual constants (`BAR_HEIGHT = 44`, `FRAME_THICKNESS = 12`, `CUTOUT_RADIUS = 16`) are defined once in `frame.rs` and referenced consistently across niri snippet + README.
- The cairo even-odd fill technique with one outer rect + one inner rounded rect is the standard idiom for "fill an annulus / rect-with-cutout" — no exotic compositing needed.
- Click-through is set on every realize via `gdk::Surface::set_input_region(&empty)`. If the layer-shell window is reparented (it shouldn't be on niri, but defensively), realize fires again and re-applies.
- No new crate dependencies: `cairo`, `gdk`, `gtk4-layer-shell` are already used by the bar and the OSD overlay.
