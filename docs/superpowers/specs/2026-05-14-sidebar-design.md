# Sidebar: pushable left sidebar that extends the frame

**Status:** design approved 2026-05-14
**Scope:** new `trollshell/src/overlays/sidebar.rs`, new `trollshell/src/widgets/sidebar_toggle.rs`, edits to `trollshell/src/overlays/frame.rs`, new CSS in `trollshell/style.css`, wiring in `trollshell/src/main.rs`.

## Motivation

trollshell currently has two HUD surfaces: the bar (top, exclusive zone) and the modal drawer (slides out from under the bar, overlay-style). Everything user-facing lives in one of those two places. The workspace area is otherwise dedicated to niri's tiled apps.

We want a third surface — a **left sidebar** — to host quick-access content (future phases). For the MVP, the goal is to land the *mechanism*: an animated slide-out surface anchored to the left edge that **pushes** niri tiles aside (real reflow, not overlay), visually extends the existing frame's left strut, and is toggled by a chip in the bar.

The push behavior is the load-bearing requirement. The drawer overlays apps; the sidebar must reserve space so niri reflows tiles when it opens. That way the user can leave it open while working.

## Design

### Three pieces

| piece | file | role |
|---|---|---|
| sidebar surface | `trollshell/src/overlays/sidebar.rs` *(new)* | layer-shell window, one per monitor, owns the slide animation and the exclusive zone. |
| toggle chip | `trollshell/src/widgets/sidebar_toggle.rs` *(new)* | bar widget. mounts as the leftmost item in `.left([…])`, flips the open-state mutable. |
| frame integration | `trollshell/src/overlays/frame.rs` *(edit)* | reads the sidebar's current visible width and offsets the cutout's left edge so the cutout's left-side rounded corners land flush with the sidebar's right edge. |

Shared state lives in a thread-local `HashMap<connector, Mutable<bool>>` named `SIDEBAR_OPEN`, mirroring `modal::DRAWER_OPEN`. Subscribers (sidebar surface, frame draw) read it; the toggle chip writes it. Per-monitor key is the connector string.

### Sidebar surface (`sidebar.rs`)

Layer-shell window with these properties:

- `Layer::Top`, `Anchor::Left + Top + Bottom`, `namespace("hytte-sidebar-{key}")`.
- `keyboard_mode(KeyboardMode::OnDemand)` — sidebar takes focus when interactive content lands there (post-MVP); MVP placeholder doesn't need it but the surface is wired for it.
- `exclusive_zone` is dynamic: `0` when closed, `SIDEBAR_WIDTH - FRAME_THICKNESS` (= 212) when open. Stacks with niri's static 8px left strut → total reserved space = 220px when open. **No change** to `etc/niri/frame.kdl`.
- Surface visibility is dynamic: `set_visible(false)` when closed, `set_visible(true)` when open (mirrors the drawer pattern in `modal.rs`). When closed, niri sees no surface on the left → tiles fill the cutout area (modulo niri's own 8px strut). When open, surface is 220px wide and exclusive_zone reserves the matching space.
- `set_size_request(SIDEBAR_WIDTH, -1)` so the surface always negotiates as 220 wide while visible, regardless of revealer state.

Content tree inside the surface:

```
window (layer-shell)
└── revealer  (TransitionType::SlideRight, 180ms)
    └── card  (gtk::Box vertical, .ts-sidebar)
        └── label  (gtk::Label "sidebar", .ts-sidebar-placeholder)
```

Margins on the card:
- `margin_top = BAR_HEIGHT + 10` (44 + 10 = 54) — matches the drawer's float-below-bar offset (`f322ab3 / 6f8e853`).
- `margin_bottom = FRAME_THICKNESS` (8) — flush with the frame's bottom strut.
- `margin_start = FRAME_THICKNESS` (8) — flush with the frame's left strut.

Card visuals (CSS in `style.css`):
- Same dark gradient as the bar and frame, but oriented vertically (top → bottom uses the bar's 3-stop palette).
- Rounded **right** corners at `CUTOUT_RADIUS` (10) so the cutout's left-side rounding visually transfers onto the sidebar's right edge.
- Left side: flat (sidebar is flush with the screen's left edge).

Constants in `sidebar.rs`:

```rust
pub const SIDEBAR_WIDTH: i32 = 220;
```

Re-exported via `pub use` from `overlays/mod.rs` so `frame.rs` can read it. `BAR_HEIGHT` and `FRAME_THICKNESS` already live in `frame.rs` as module constants; `sidebar.rs` redeclares its own (kept in sync by code review — same pattern as `frame.rs` ↔ `etc/niri/frame.kdl`).

Public API of `sidebar.rs`:

```rust
pub fn install(monitor: &Monitor);
pub fn close_all();                              // mirrors modal::close_all
pub fn open_signal(monitor: &Monitor) -> impl Signal<Item = bool> + 'static;
pub fn toggle(monitor: &Monitor);                // flip Mutable<bool>
pub const SIDEBAR_WIDTH: i32 = 220;
```

`open_signal` is the entry point for frame.rs and any future bar-CSS bindings (e.g., squaring off the bar's bottom-left corner while sidebar is open, analogous to the drawer-open binding).

### Open / close mechanics

State is a `Mutable<bool>`. Both the chip and `toggle()` flip it. The sidebar surface subscribes once at `install` time and reacts to changes.

**Opening (false → true):**

1. `window.set_visible(true)`; `window.present()` so the surface is mapped.
2. `set_exclusive_zone(SIDEBAR_WIDTH - FRAME_THICKNESS)` (212). niri sees the zone bump and starts reflowing tiles right.
3. `revealer.set_reveal_child(true)` → SlideRight, 180 ms.
4. `frame` queues redraws each tick of the transition (see Frame integration below); its cutout's left x animates 8 → 220 in sync with the slide.

**Closing (true → false):**

1. `revealer.set_reveal_child(false)` → SlideRight collapses, 180 ms.
2. On `connect_child_revealed_notify` when `is_child_revealed()` becomes false: `set_exclusive_zone(0)`, then `window.set_visible(false)`. niri reflows tiles back left.

The ordering is asymmetric on purpose:
- On open, change the zone **first** so niri starts its own animation as early as possible; by the time the GTK slide finishes, niri's tile motion is close to settled. Brief overshoot is invisible because the sliding sidebar covers it.
- On close, change the zone **last** so niri doesn't reclaim the space while the sidebar is still visible (which would let tiles render through the sidebar for a frame).

### Toggle chip (`sidebar_toggle.rs`)

Mirrors `widgets/settings_chip.rs` in shape and CSS:

```rust
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-sidebar-toggle");

    let icon = gtk::Image::from_icon_name("view-sidebar-symbolic");
    btn.set_child(Some(&icon));

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::overlays::sidebar::toggle(&monitor_for_click);
    });

    btn.upcast()
}
```

If `view-sidebar-symbolic` is not present in the icon theme, fall back to `sidebar-show-symbolic` then `go-previous-symbolic`. Try-list at runtime via the `icons` lookup pattern used elsewhere; the exact ordering can be tightened in PR review.

In `main.rs`, mount it as the new leftmost item:

```rust
.left([
    widgets::sidebar_toggle::widget(monitor),  // NEW, leftmost
    widgets::workspaces::widget(monitor),
    widgets::window_list::widget(monitor),
])
```

Also add `mod sidebar_toggle;` and `pub use sidebar_toggle;` to `widgets/mod.rs`.

### Frame integration (`frame.rs` edits)

The frame's draw fn currently uses `cutout_rect(w, h)` which returns `(FRAME_THICKNESS, BAR_HEIGHT, w - 2*FRAME_THICKNESS, h - BAR_HEIGHT - FRAME_THICKNESS)`. The sidebar makes the left edge of that cutout dynamic.

**Approach: per-frame allocation read.** Subscribe once in `install` to `sidebar::open_signal(monitor)`. While the sidebar surface is animating (revealer in transition), the frame queues redraws via `add_tick_callback`. Each tick, the draw fn reads the sidebar revealer's currently allocated width on this monitor (sidebar surface for this monitor lookup-able by connector key) and uses `max(FRAME_THICKNESS, sidebar_visible_width)` as the cutout's left x. When the revealer settles (`child_revealed` notify), tick callback unregisters.

Concretely:

1. Add two tiny helpers to `sidebar.rs`:

   ```rust
   /// Currently visible width of the sidebar card on this monitor, in CSS px.
   /// Returns FRAME_THICKNESS when closed (no extra cutout offset).
   pub fn current_visible_width(monitor: &Monitor) -> i32 { /* read revealer allocation */ }

   /// True when the sidebar's revealer animation is at rest (fully open or fully closed).
   /// Used by the frame's tick callback to know when to stop redrawing.
   pub fn is_settled(monitor: &Monitor) -> bool { /* revealer.is_child_revealed() == open_state */ }
   ```

   `current_visible_width` implementation: look up the per-monitor `SidebarPanel` in the thread-local map, return `panel.revealer.allocation().width().max(FRAME_THICKNESS_I32)`. When closed, the revealer's allocated width is 0, so the `.max` gives `FRAME_THICKNESS` and the cutout stays as-is.

   `is_settled` implementation: returns true when `revealer.is_child_revealed() == panel.open_state.get()`. During a transition the two diverge (target reached at the end of animation); at rest they match.

2. In `frame.rs::install`, after the existing setup:

   ```rust
   // Sync cutout's left x with the sidebar's visible width.
   let monitor_for_tick = monitor.clone();
   let area_for_tick = area.clone();
   crate::overlays::sidebar::open_signal(monitor).for_each(move |_| {
       // Run a tick callback for the animation window.
       let area = area_for_tick.clone();
       let monitor = monitor_for_tick.clone();
       area.add_tick_callback(move |a, _| {
           a.queue_draw();
           // Stop ticking once the sidebar's revealer is fully settled.
           if crate::overlays::sidebar::is_settled(&monitor) {
               glib::ControlFlow::Break
           } else {
               glib::ControlFlow::Continue
           }
       });
       async {}
   }).await;
   ```

   Wire shape is illustrative — actual code uses the project's `for_each` / spawn convention (see how `bind_visible` and the existing `niri::edge_window_on` subscription are spawned in `frame.rs`).

3. Modify `cutout_rect` to take an extra parameter:

   ```rust
   fn cutout_rect(width: f64, height: f64, left_inset: f64) -> (f64, f64, f64, f64) {
       let x = left_inset;
       let y = BAR_HEIGHT;
       let w = (width - left_inset - FRAME_THICKNESS).max(0.0);
       let h = (height - BAR_HEIGHT - FRAME_THICKNESS).max(0.0);
       (x, y, w, h)
   }
   ```

   The draw fn passes `left_inset = max(FRAME_THICKNESS, sidebar::current_visible_width(monitor) as f64)`.

4. The frame's existing `bind_visible` (hides frame on edge-window) keeps working unchanged — when the sidebar is open AND the user maximizes-to-edges, the frame hides; the sidebar surface still has its exclusive zone, so the maximized window still tiles to the right of the sidebar. That's correct behavior (sidebar is a peer, not part of the frame).

### CSS (`style.css`)

New rules:

```css
.ts-sidebar {
    background: linear-gradient(
        180deg,
        rgb(15, 15, 35),
        rgb(25, 15, 45) 50%,
        rgb(15, 15, 35)
    );
    border-radius: 0 10px 10px 0;   /* round only the right side */
    padding: 12px;
}

.ts-sidebar-placeholder {
    color: alpha(white, 0.5);
    font-style: italic;
}
```

Light-mode override mirrors `.ts-drawer`'s light-mode rule (line 307) — palette swap, same gradient structure.

### Lifecycle

`main.rs` has two relevant call sites:

1. `build_bar(monitor)` calls `modal::install(monitor)` synchronously per monitor and is itself called on every `monitors_changed` emission (initial + hot-plug). Sidebar mirrors this: add `overlays::sidebar::install(monitor)` immediately after `modal::install(monitor)` in `build_bar`. The sidebar's per-bar lifecycle matches the modal's, so hot-plug works the same way.
2. The `monitors_changed` reactor calls `modal::close_all()` before rebuilding. Add `overlays::sidebar::close_all()` on the next line, so stale sidebar surfaces are torn down before `build_bar` reinstalls them.

The frame's `install` (in the separate init-once `for monitor in &app.monitors()` loop) subscribes to `sidebar::open_signal(monitor)`, which reads from the `SIDEBAR_OPEN` thread-local — that map is populated lazily on first read, so the frame can subscribe even before `sidebar::install` has run for that monitor. `current_visible_width` returns `FRAME_THICKNESS` in the interim, which is the correct closed-state default.

## Touched files

- `trollshell/src/overlays/sidebar.rs` — new.
- `trollshell/src/overlays/mod.rs` — `pub mod sidebar;`.
- `trollshell/src/widgets/sidebar_toggle.rs` — new.
- `trollshell/src/widgets/mod.rs` — `pub mod sidebar_toggle;`.
- `trollshell/src/overlays/frame.rs` — extend `cutout_rect` signature, add sidebar-aware tick callback, plumb monitor through to the draw closure.
- `trollshell/src/main.rs` — install sidebar per monitor; prepend `sidebar_toggle` chip to `.left([…])`.
- `trollshell/style.css` — `.ts-sidebar`, `.ts-sidebar-placeholder`, light-mode override.

No changes to `etc/niri/frame.kdl`. No new dependencies.

## Tests

`#[cfg(test)] mod tests` in `sidebar.rs`:

| test | scenario | expected |
|---|---|---|
| `width_constant` | `SIDEBAR_WIDTH` is 220 | exact equality (guard against accidental edits) |
| `closed_width_returns_frame_thickness` | helper returns `FRAME_THICKNESS_I32` when revealer width is 0 | `8` |

`#[cfg(test)] mod tests` in `frame.rs` (extend existing):

| test | scenario | expected |
|---|---|---|
| `cutout_rect_with_sidebar_open` | `cutout_rect(1920, 1080, 220.0)` | x=220, w=1920-220-8=1692 |
| `cutout_rect_with_sidebar_closed` | `cutout_rect(1920, 1080, 8.0)` | x=8, w=1920-16=1904 (matches existing `cutout_rect_normal_monitor`) |
| `cutout_rect_tiny_monitor_clamps_to_zero` | unchanged, but now passes `FRAME_THICKNESS` as the third arg | preserved |

Existing `cutout_rect_*` tests get their call sites updated to pass `FRAME_THICKNESS` as the new third argument.

No integration tests for the layer-shell animation — interactive verification covers it (see Verification).

## Out of scope

- **Real sidebar content.** Phase 2 decides what lives in there (launcher, widgets, etc.). The placeholder label exists only to confirm the surface and animation work end-to-end.
- **Click-catcher.** The drawer has a fullscreen click-catcher; the sidebar does not. Users close it by re-clicking the chip or pressing ESC while the sidebar has focus. Click-on-workspace-to-close can be added later if it's wanted.
- **Keybinding.** No niri-side keybind to toggle the sidebar in MVP. Add later if useful.
- **Right-side sidebar.** Only left for now. Symmetric mirror would be straightforward but is not designed here.
- **Per-page state (à la `Page` enum in modal).** Sidebar has one piece of content; no stack, no page enum.
- **Niri config update.** `etc/niri/frame.kdl` stays at 8px struts. Sidebar's exclusive zone stacks additively.
- **Animation easing tweaks.** GTK `Revealer` SlideRight default; can be customized later if it doesn't read right next to niri's reflow easing.

## Verification

After landing:

1. `cargo build -p trollshell` succeeds; `cargo test -p trollshell` passes (new and existing tests).
2. Launch trollshell on niri. With no sidebar open, frame looks identical to before — 8px left strut, rounded cutout corners at x=8.
3. Click the new leftmost bar chip. Sidebar slides out from the left, 220 px wide. niri tiles in the workspace reflow right within a frame or two. Frame's cutout left edge has moved to x=220; cutout's top-left and bottom-left rounded corners now sit flush with the sidebar's right edge.
4. Click the chip again. Sidebar slides back in. niri tiles reflow left. Frame's cutout returns to x=8.
5. With sidebar open, focus it and press `ESC` — sidebar closes the same way.
6. With sidebar open, press `Mod+M` to maximize-to-edges. Frame hides (existing behavior); sidebar stays. The maximized window tiles right of the sidebar (because of the exclusive zone). Unmaximize → frame returns, sidebar still open.
7. Multi-monitor: sidebar toggle is per-monitor. Open on monitor A; monitor B's frame is unaffected, monitor B's tiles don't reflow.
8. Hot-unplug a monitor while its sidebar is open. No panic; the surface is torn down with the bar.
9. Visual regression: the bar's drawer (any page, e.g. Settings) still opens and closes correctly. Frame still hides on fullscreen.
