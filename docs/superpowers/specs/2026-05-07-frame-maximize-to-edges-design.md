# Frame: hide on maximize-to-edges

**Status:** design approved 2026-05-07
**Scope:** `trollshell/src/overlays/frame.rs` — extend the existing fullscreen-visibility binding to also catch niri's maximize-to-edges state.

## Motivation

The frame overlay paints L/R/bottom gradient strips and four rounded inner corners around the workspace cutout. niri's `struts { left 8; right 8; bottom 8 }` reserve the matching inset, so tiled apps stay inside the cutout and never touch the frame.

niri 25.x has a per-window `MaximizeWindowToEdges` action (`Mod+M` by default; also triggered by titlebar maximize-button or double-click). A maximized-to-edges window expands to "the edges of the available screen area" — bypassing struts, gaps, and borders, but still respecting the bar's exclusive zone. Its `tile_size` becomes `(mon_w, mon_h − bar_height)`.

The frame's L/R/bottom strips and corner masks now sit *over* the window. There is no way to paint the frame that doesn't overlap the window content, because every pixel the frame paints is, by definition, where the window now is. The only sensible response is to hide the frame, exactly the same way it's already hidden on fullscreen.

The existing detection (`tile_size ≈ (mon_w, mon_h)`) only catches true fullscreen — maximize-to-edges has a smaller height because the bar's exclusive zone still applies, so the existing two-axis check misses it.

## Design

### Detection: width-only check

When `tile_size.0 ≈ mon_w` (window spans full output width), the window touches the L and R edges of the output, which is precisely where the frame's L/R gradient strips live. That's a sufficient signal to hide the frame:

- **Maximize-to-edges**: `tile_size = (mon_w, mon_h − bar_height)` → width matches → hide.
- **Fullscreen**: `tile_size = (mon_w, mon_h)` → width matches → hide. (Already hidden today; new logic preserves this.)
- **Edge-stretched floating window**: a floating window manually sized to `tile_size.0 == mon_w` would also overlap the frame's L/R strips, so hiding is still the correct visual response. Treating this case the same as maximize-to-edges is a feature, not a bug.
- **Normal tiled**: `tile_size.0 ≤ mon_w − 16` (struts) → width does not match → frame stays visible.

niri's maximize-to-edges always covers the full available *width* AND *height*; there is no "horizontal-only maximize" state in niri. So checking width alone is sufficient — a window with full height but partial width does not exist as a niri state, and even if it did, it wouldn't overlap the frame's L/R strips.

`Window` in `niri-ipc` 25.11 exposes only `is_focused`, `is_floating`, `is_urgent` — there is no `is_maximized_to_edges` boolean. Detection via `tile_size` comparison is the only available path, and matches the pattern the existing fullscreen detection already uses.

### Code shape

In `trollshell/src/overlays/frame.rs`:

1. Rename `FULLSCREEN_TOL` → `EDGE_TOL` (same value: `4.0`). Doc-comment updated to describe edge-detection in general (covers both fullscreen and maximize-to-edges).
2. Rename `bind_fullscreen_visibility` → `bind_edge_visibility`. Drop the `mon_h` parameter (no longer used). Update the doc-comment to explain that the trigger is "any window on the active workspace whose tile width spans the full output", and why width alone suffices.
3. Replace the two-axis tolerance check with a single `>=` comparison:

   ```rust
   w.workspace_id == Some(id)
       && w.layout.tile_size.0 >= mon_w - EDGE_TOL
   ```

   `>=` is more robust than the two-sided `abs` form: tile width can never *exceed* `mon_w` in practice, so a one-sided check is symmetric in effect and tolerates fractional-scale rounding the same way.
4. Lift the predicate out of the reactive closure into a pure helper, so it's testable:

   ```rust
   fn has_edge_window(
       workspaces: &[Workspace],
       windows: &[Window],
       connector: &str,
       mon_w: f64,
   ) -> bool {
       let active_id = workspaces
           .iter()
           .find(|ws| ws.output.as_deref() == Some(connector) && ws.is_active)
           .map(|ws| ws.id);
       active_id.is_some_and(|id| {
           windows.iter().any(|w| {
               w.workspace_id == Some(id)
                   && w.layout.tile_size.0 >= mon_w - EDGE_TOL
           })
       })
   }
   ```

   `bind_edge_visibility` becomes a thin wrapper that calls `has_edge_window` inside the `map_ref!` and inverts to `visible`.
5. Update the `install` callsite — drop the `mon_h` argument, rename the function call.

### Tests

Add unit tests for `has_edge_window` in the existing `#[cfg(test)] mod tests` block. Each test constructs minimal `Workspace` and `Window` fixtures (only the fields the helper reads) and asserts the boolean.

| test | scenario | expected |
|---|---|---|
| `has_edge_window_normal_tiled` | tile_size = (mon_w − 16, mon_h − 52) on active workspace | `false` |
| `has_edge_window_maximize_to_edges` | tile_size = (mon_w, mon_h − 44) on active workspace | `true` |
| `has_edge_window_fullscreen` | tile_size = (mon_w, mon_h) on active workspace | `true` |
| `has_edge_window_within_tolerance` | tile_size = (mon_w − 2, _) on active workspace, tol=4 | `true` |
| `has_edge_window_other_workspace_ignored` | edge-sized window exists, but on a non-active workspace on this output | `false` |
| `has_edge_window_other_output_ignored` | edge-sized window exists, but the active workspace is on a different output (different connector) | `false` |
| `has_edge_window_no_active_workspace` | no workspace on this output is active | `false` |

Construction note: `niri_ipc::Window` and `niri_ipc::Workspace` are re-exported via `hytte_services::niri`. Tests build them with `..Default::default()` if available, else field-by-field. If `Default` isn't derived, add a small private `mk_window(...)`/`mk_workspace(...)` builder in the test module.

### Behavior

`bind_visible(!has_edge_window(..), window)` — same `bind_visible` helper as today. Hide is binary, no animation. Re-evaluates on every `niri::workspaces()` and `niri::windows()` change, so toggling maximize-to-edges via `Mod+M` flips the frame visibility live.

## Touched files

- `trollshell/src/overlays/frame.rs` — rename constant + function, switch to width-only check, lift to pure helper, add tests.

No CSS, no niri config, no other crates. The struts in `etc/niri/frame.kdl` stay at 8px — they govern non-maximized tiling, which is unchanged.

## Out of scope

- Per-window niri `geometry-corner-radius` rule. Maximize-to-edges windows already square their own corners (per niri docs: "Windows are aware of their maximized-to-edges status and generally respond by squaring their corners"). The frame is hidden in that state anyway, so corner-rounding is moot.
- Show/hide animation. The existing fullscreen path is binary; matching that.
- A niri-IPC enhancement request to expose `is_maximized_to_edges` as a boolean. `tile_size`-comparison is sufficient and consistent with the existing fullscreen path.
- Floating-window special-casing. Width-only detection naturally handles edge-stretched floating windows the same way as maximize-to-edges, which is the correct visual response.

## Verification

After landing:

1. Build and run trollshell on niri. Open a normal tiled window — frame visible, rounded corners painted around the cutout.
2. Press `Mod+M` to maximize-to-edges. Frame hides; the window now spans full output width and reaches L/R/bottom edges of the available area.
3. Press `Mod+M` again to unmaximize. Frame reappears as the window returns to normal tile size.
4. Toggle fullscreen on a window (existing behavior). Frame still hides — regression check for the existing fullscreen path.
5. Switch to an empty workspace on the same monitor while a maximized-to-edges window remains on another workspace. Frame becomes visible (active workspace has no edge window).
6. With multi-monitor: maximize-to-edges on monitor A. Frame on A hides; frame on B stays visible.
7. `cargo test -p trollshell` — new `has_edge_window_*` tests pass.
