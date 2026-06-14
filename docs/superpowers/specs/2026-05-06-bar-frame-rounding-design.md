# Integrated bar + rounded workspace frame

**Status:** design approved 2026-05-06
**Scope:** restyle `trollshell/style.css` `.hytte-bar-content`; new module `trollshell/src/overlays/frame.rs` and per-monitor mount in `trollshell/src/main.rs`; new niri config snippet `etc/niri/frame.kdl` with README entry.
**Reference mock:** `design/mock01.png`

## Motivation

The bar today is a floating rounded pill (5px side margins, 10px bottom margin, 12px border-radius) over whatever wallpaper niri renders behind it. The mock changes the visual model: the bar becomes a flush strip across the top, and the bar's dark gradient extends downward and around the workspace as a continuous "frame" with rounded inner corners. Apps tile inside that rounded cutout. The result reads as one card-like surface — bar + border — with the wallpaper visible only inside the cutout.

This is purely a presentation change. No new services, no new state. swaybg keeps painting the wallpaper for now (replacing it with a native trollshell renderer is a separate phase 2 spec).

## Design

### Architecture

Three layers, top to bottom:

| layer        | window                                | role                                                                                                                                                                                                                                            |
| ------------ | ------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `OVERLAY`    | trollshell `frame` overlay (new)      | full-screen, click-through; paints the dark gradient in L/R/bottom borders and carves rounded inner corners around the workspace cutout. Sits above the bar so the cutout's top corners can shape the bar's bottom-L/R as concave indentations. |
| `TOP`        | trollshell `Bar` (existing, restyled) | same widgets as today, `margin: 0; border-radius: 0;` — flush strip across full width. Bar's existing dark gradient bg unchanged; exclusive zone reserves the top inset.                                                                        |
| `BACKGROUND` | swaybg (unchanged)                    | wallpaper                                                                                                                                                                                                                                       |

niri tiles apps in the rectangle bounded by the bar's exclusive zone (top) and the new struts (`left 12; right 12; bottom 12`). The visible rounded corners come from the frame overlay painting the dark gradient OVER niri's app surfaces in the corner regions — no per-window `geometry-corner-radius` is required for the look to land. (We can add one later if app corners poke through visibly under specific apps; not in scope here.)

### Visual spec

- Frame thickness: `N = 12px` on left, right, and bottom. Top inset is the bar's exclusive zone.
- Cutout corner radius: `R = 16px` on all four corners.
- Color: the bar's existing gradient,
  ```css
  linear-gradient(90deg, rgba(15,15,35,1) 0%, rgba(25,15,45,1) 50%, rgba(15,15,35,1) 100%)
  ```
  Both bar and frame paint with this same 3-stop gradient, aligned to screen width (90deg from `x=0` to `x=W`), so the bar's L/R edges and the frame's L/R borders blend without seams.
- Bar height shrinks from ~59px (with the old margins) to ~44px (current `padding: 6px 12px` + `min-height: 32px`, no margin).

### Bar restyle

`trollshell/style.css` — change `.hytte-bar-content`:

```css
.hytte-bar-content {
  padding: 6px 12px;
  min-height: 32px;
  margin: 0; /* was: 5px 5px; margin-bottom: 10px */
  border-radius: 0; /* was: 12px */
  background: linear-gradient(
    90deg,
    rgba(15, 15, 35, 1) 0%,
    rgba(25, 15, 45, 1) 50%,
    rgba(15, 15, 35, 1) 100%
  );
  /* drop box-shadow — bar no longer floats */
}
```

The existing `.hytte-bar.drawer-open .hytte-bar-content { border-bottom-right-radius: 0 }` rule is preserved as-is. With the new flat bar it is a no-op (bottom-right radius is already 0), but keeping it costs nothing and means the drawer-seam logic stays in one place if we later round the bar's corners again.

### Frame overlay (new)

File: `trollshell/src/overlays/frame.rs`

Per-monitor, mounted by `frame::install(monitor)` in `main.rs`:

- Layer-shell window:
  - layer `OVERLAY`
  - anchored to all 4 edges (the layer-shell window covers the full output)
  - `exclusive_zone = 0` — frame must not reserve any compositor space; the bar reserves its top zone, and niri reserves L/R/bottom via struts
  - `keyboard_interactivity = None`
  - **input region empty** — every click passes through. The bar (on `TOP`, below the frame) and niri's apps (below `TOP`) keep receiving input as before. Implementation: call `gtk4_layer_shell` API to set an empty input region on the surface after realize.
- Content: a single `gtk::DrawingArea` filling the whole window, with a `set_draw_func` that paints the frame using cairo:
  1. Draw nothing for `y < bar_height` (bar area; bar paints itself on TOP).
  2. For `y >= bar_height`, paint the dark 3-stop gradient (90deg, screen-width-aligned) into a path that is `(L strip) ∪ (R strip) ∪ (bottom strip) ∪ (corner masks)`, where the cutout (a rounded rect with radius `R`) is excluded.
  3. Implement as one cairo path: trace the outer rect from `(0, bar_height)` to `(W, H)`, then trace the cutout (rounded rect), and fill with `FillRule::EvenOdd`. The four `cairo_arc` calls handle the rounded corners. Set the source to a `cairo::LinearGradient` from `(0, 0)` to `(W, 0)` with the three stops, then `fill()`.
- The `bar_height` value is needed at draw time. Two options, pick at implementation:
  - (a) Hardcode the post-restyle bar height (44px) as a const in `frame.rs`, with a comment pointing to the CSS.
  - (b) Query the bar window's size on the same monitor and signal it through. More dynamic but adds plumbing for v1; recommend (a) and revisit if the bar height becomes themable.
- Re-mount on monitor hot-plug — same lifecycle as existing overlays. The frame is rebuilt when `monitors_changed` fires.

**Layer-order note.** OVERLAY is above TOP, so the frame is drawn ABOVE the bar. The bar's top region is left transparent in the frame, so bar widgets remain visible. The bar's L/R bottom corners get covered by the frame's corner-mask painting — this is intentional and is what produces the concave indentations the mock shows. The frame's empty input region guarantees no click-eating regardless of where bar widgets sit relative to the painted corner area.

### niri config snippet (new)

File: `etc/niri/frame.kdl`

```kdl
layout {
    struts {
        left 12
        right 12
        bottom 12
    }
}
```

The user merges this into their `~/.config/niri/config.kdl` by hand, the same way `etc/niri/binds.kdl` is merged today. niri does not support `include`. The README at `etc/niri/README.md` gets a new section:

- "Frame struts" — what the snippet does (reserves L/R/bottom inset matching the trollshell frame), how to merge (add to existing `layout { }` block, or paste at the top if no `layout` block exists), and how to verify (a window should not tile into the frame border; the workspace's rounded corners should align with the frame's painted edges).

The snippet keeps `gaps` untouched. If the user already runs gaps, those compose with struts naturally.

### Mounting in main.rs

In the `run` callback, alongside the other per-monitor overlay installs:

```rust
for monitor in &app.monitors() {
    overlays::frame::install(monitor);          // new
    overlays::notifications::install(monitor);
    overlays::osd::install(monitor);
}
```

Order: install `frame` first so its window exists in z-order before notifications/OSD bind to the same monitor — although since notifications and OSD pop on demand and the frame is always-on, ordering here is cosmetic. Document the choice with a one-line comment.

### Module wiring

`trollshell/src/overlays/mod.rs` gets:

```rust
pub mod frame;
```

next to the existing `notifications`, `osd`, `lock_screen`, `prompt`, `polkit_dialog`.

## Touched files

- `trollshell/style.css` — `.hytte-bar-content` margin / border-radius / box-shadow
- `trollshell/src/overlays/frame.rs` — new module
- `trollshell/src/overlays/mod.rs` — `pub mod frame;`
- `trollshell/src/main.rs` — `overlays::frame::install(monitor)` in the per-monitor loop
- `etc/niri/frame.kdl` — new snippet
- `etc/niri/README.md` — new "Frame struts" section

## Out of scope (phase 2 / later)

- Replacing swaybg with a native trollshell wallpaper renderer (`crates/hytte-services/src/wallpaper.rs` currently shells out to swaybg via a systemd unit). A separate spec will rebuild the rendering side, drop the systemd unit, and tighten coupling between the wallpaper image and the cutout shape.
- Configurable `N` and `R` (hardcoded for v1).
- Per-monitor or themed frame colors (uses the bar's existing palette).
- niri `geometry-corner-radius` window-rule for app corners. The frame's overlay clipping handles the visible rounding for free; we can add a window-rule later if a specific app's corners poke visibly through the cutout's rounding.

## Verification

After landing:

1. Build and run trollshell on niri. The bar should sit flush against the top edge, full-width, no rounded corners on the bar itself.
2. With no apps open: the workspace area shows the wallpaper inside a rounded-corner cutout; the L/R/bottom borders are filled with the same dark gradient as the bar.
3. With an app open: the app tiles inside the strut-inset rectangle. The frame overlay paints over the four corners of the app, producing visibly rounded edges where the app meets the frame.
4. Click on the bar widgets — they react. Click in the L/R/bottom border (not inside any app) — the click does not get eaten by the frame (it falls through to whatever is below, typically nothing).
5. Open the drawer — it still pops down from the bar's bottom-right; the seam stays clean.
6. Hot-plug a monitor — the frame remounts on the new monitor along with the other overlays.
