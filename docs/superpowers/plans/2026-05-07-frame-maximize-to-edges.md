# Frame: hide on maximize-to-edges — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Hide the frame overlay whenever the active workspace contains a window that's been maximized-to-edges, in addition to the existing hide-on-fullscreen behavior.

**Architecture:** Lift the visibility predicate out of `bind_fullscreen_visibility`'s reactive closure into a pure helper, then switch from the existing dual-axis tolerance check (`tile_size ≈ output_size`) to a single width-only check (`tile_size.0 >= mon_w − EDGE_TOL`). Width alone suffices because niri's maximize-to-edges always covers the full available width AND height — there is no horizontal-only maximize state. Width-matching also catches fullscreen and edge-stretched floating windows in one rule. Rename the constant and binding function from `*FULLSCREEN*` to `*EDGE*` to reflect the broader semantics. All changes are confined to `trollshell/src/overlays/frame.rs`.

**Tech Stack:** Rust 1.94 stable, `niri-ipc` 25.11, `futures-signals`, GTK4. Tests use stock `#[test]` with a small private fixture-builder helper.

**Spec:** `docs/superpowers/specs/2026-05-07-frame-maximize-to-edges-design.md`

---

## File Structure

Single file changes:

- **Modify:** `trollshell/src/overlays/frame.rs` — extract pure helper `has_edge_window`, switch detection to width-only, rename `FULLSCREEN_TOL` → `EDGE_TOL` and `bind_fullscreen_visibility` → `bind_edge_visibility`, drop the now-unused `mon_h` parameter, add unit tests.

No other files. CSS, niri config, and other crates are unchanged.

---

## Task 1: Extract pure helper with baseline tests (no behavior change)

Refactor only. The helper `has_edge_window` is introduced with the _current_ dual-axis fullscreen logic. The existing reactive closure in `bind_fullscreen_visibility` becomes a thin wrapper that calls the helper. Baseline tests cover the existing behavior so Task 2 can change the predicate with confidence.

**Files:**

- Modify: `trollshell/src/overlays/frame.rs:92-118` (replace closure body) and add a `#[cfg(test)] mod tests` block (extending the existing one at line 213).

- [ ] **Step 1: Add baseline tests against a not-yet-existing helper**

Append to the existing `mod tests` block in `trollshell/src/overlays/frame.rs`. The block currently ends after `cutout_rect_tiny_monitor_clamps_to_zero` at roughly line 234. Add — inside the same `mod tests`, before the closing `}` — these test fixture builders and tests:

```rust
    use hytte::services::niri::{Window, Workspace};
    use niri_ipc::WindowLayout;

    const MON_W: f64 = 1920.0;
    const MON_H: f64 = 1080.0;
    const BAR_H: f64 = BAR_HEIGHT;
    const CONNECTOR: &str = "DP-1";

    fn mk_workspace(id: u64, output: &str, is_active: bool) -> Workspace {
        Workspace {
            id,
            idx: 1,
            name: None,
            output: Some(output.to_string()),
            is_urgent: false,
            is_active,
            is_focused: is_active,
            active_window_id: None,
        }
    }

    fn mk_window(id: u64, workspace_id: u64, tile: (f64, f64)) -> Window {
        Window {
            id,
            title: None,
            app_id: None,
            pid: None,
            workspace_id: Some(workspace_id),
            is_focused: false,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: Some((1, 1)),
                tile_size: tile,
                window_size: (tile.0 as i32, tile.1 as i32),
                tile_pos_in_workspace_view: Some((0.0, 0.0)),
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    #[test]
    fn has_edge_window_normal_tiled() {
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        // Tiled with struts: width = MON_W - 16, height = MON_H - BAR - 8.
        let w = vec![mk_window(10, 1, (MON_W - 16.0, MON_H - BAR_H - 8.0))];
        assert!(!has_edge_window(&ws, &w, CONNECTOR, MON_W, MON_H));
    }

    #[test]
    fn has_edge_window_fullscreen() {
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        let w = vec![mk_window(10, 1, (MON_W, MON_H))];
        assert!(has_edge_window(&ws, &w, CONNECTOR, MON_W, MON_H));
    }

    #[test]
    fn has_edge_window_other_workspace_ignored() {
        // Active workspace has only a normal-tiled window; another (inactive)
        // workspace on the same output has a fullscreen window. Frame should
        // NOT hide — the fullscreen window isn't on the visible workspace.
        let ws = vec![
            mk_workspace(1, CONNECTOR, true),
            mk_workspace(2, CONNECTOR, false),
        ];
        let w = vec![
            mk_window(10, 1, (MON_W - 16.0, MON_H - BAR_H - 8.0)),
            mk_window(20, 2, (MON_W, MON_H)),
        ];
        assert!(!has_edge_window(&ws, &w, CONNECTOR, MON_W, MON_H));
    }

    #[test]
    fn has_edge_window_other_output_ignored() {
        // Active workspace is on a DIFFERENT output (HDMI-A-1); the only
        // fullscreen window lives there. Our connector (DP-1) has no active
        // workspace. Helper returns false.
        let ws = vec![mk_workspace(1, "HDMI-A-1", true)];
        let w = vec![mk_window(10, 1, (MON_W, MON_H))];
        assert!(!has_edge_window(&ws, &w, CONNECTOR, MON_W, MON_H));
    }

    #[test]
    fn has_edge_window_no_active_workspace() {
        // Workspace exists on this output but is_active is false (e.g.,
        // no outputs connected yet, or transient state during hot-plug).
        let ws = vec![mk_workspace(1, CONNECTOR, false)];
        let w = vec![mk_window(10, 1, (MON_W, MON_H))];
        assert!(!has_edge_window(&ws, &w, CONNECTOR, MON_W, MON_H));
    }
```

- [ ] **Step 2: Run tests to verify they fail (compile error: helper missing)**

Run: `cargo test -p trollshell --lib overlays::frame`
Expected: compile error — `cannot find function 'has_edge_window' in this scope` or similar. The helper hasn't been added yet.

- [ ] **Step 3: Add the `has_edge_window` helper with current (dual-axis) logic**

Insert this function in `trollshell/src/overlays/frame.rs` immediately above `bind_fullscreen_visibility` (i.e., after the doc-comment block ending at line 91, before `fn bind_fullscreen_visibility`):

```rust
/// Return `true` when the active workspace on `connector` contains a
/// window whose tile size matches the monitor's logical size on both
/// axes (within `FULLSCREEN_TOL`). niri reserves struts for non-fullscreen
/// tiling, so a normally-maximized window has tile size strictly smaller
/// than the output — no false positives for non-fullscreen states.
fn has_edge_window(
    workspaces: &[Workspace],
    windows: &[Window],
    connector: &str,
    mon_w: f64,
    mon_h: f64,
) -> bool {
    let active_id = workspaces
        .iter()
        .find(|ws| ws.output.as_deref() == Some(connector) && ws.is_active)
        .map(|ws| ws.id);
    active_id.is_some_and(|id| {
        windows.iter().any(|w| {
            w.workspace_id == Some(id)
                && (w.layout.tile_size.0 - mon_w).abs() < FULLSCREEN_TOL
                && (w.layout.tile_size.1 - mon_h).abs() < FULLSCREEN_TOL
        })
    })
}
```

Add the import for `Window` next to the existing `Workspace` import at the top of the file. The existing line is:

```rust
use hytte::services::niri;
```

…and `pub use niri_ipc::{Window, Workspace};` is re-exported from `hytte::services::niri` (verified at `crates/hytte-services/src/niri.rs:30`). Adjust the use to:

```rust
use hytte::services::niri::{self, Window, Workspace};
```

- [ ] **Step 4: Replace the inline closure body with a call to `has_edge_window`**

In `bind_fullscreen_visibility` (currently `frame.rs:92-118`), replace the entire `map_ref!` block body with a delegating call. The full new body of the function:

```rust
fn bind_fullscreen_visibility(
    window: &gtk::Window,
    connector: String,
    mon_w: f64,
    mon_h: f64,
) {
    let workspaces = niri::workspaces();
    let windows = niri::windows();
    let visible = map_ref! {
        let workspaces = workspaces,
        let windows = windows =>
        !has_edge_window(workspaces, windows, &connector, mon_w, mon_h)
    };
    bind_visible(visible, window);
}
```

The `map_ref!` macro binds `workspaces` and `windows` as references (`&Vec<Workspace>`, `&Vec<Window>`); these deref-coerce to `&[Workspace]` / `&[Window]` automatically when passed to `has_edge_window` (verified pattern: same coercion is used in `widgets/window_list.rs:55-79`).

- [ ] **Step 5: Run tests + build, verify green**

Run: `cargo test -p trollshell --lib overlays::frame`
Expected: 7 tests pass — 2 existing (`cutout_rect_*`) + 5 new (`has_edge_window_*`).

Run: `cargo build -p trollshell`
Expected: clean build, no warnings about unused `mon_h` (still used).

- [ ] **Step 6: Commit**

```bash
git add trollshell/src/overlays/frame.rs
git commit -m "refactor(frame): extract has_edge_window helper, add baseline tests

Lift the visibility predicate out of bind_fullscreen_visibility's
reactive closure into a pure function so it's unit-testable. Behavior
unchanged — same dual-axis tolerance check against the monitor's
logical size. Adds five baseline tests covering the current states:
normal-tiled, fullscreen, other-workspace, other-output, no-active.
Sets up the next change to swap the predicate for width-only edge
detection."
```

---

## Task 2: Switch to width-only detection, drop mon_h, rename

Now change the predicate to width-only, drop the now-redundant `mon_h` parameter, and rename the constant + binding function to reflect that the trigger is "any edge-spanning window" (covers fullscreen, maximize-to-edges, and edge-stretched floating).

**Files:**

- Modify: `trollshell/src/overlays/frame.rs` — predicate body, signature changes, renames, doc-comment rewrite, two more tests.

- [ ] **Step 1: Write the failing test for maximize-to-edges**

Add to the same `mod tests` block, after `has_edge_window_no_active_workspace`:

```rust
    #[test]
    fn has_edge_window_maximize_to_edges() {
        // niri's MaximizeWindowToEdges: window covers full output width
        // AND full height-minus-bar (bar's exclusive zone still applies).
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        let w = vec![mk_window(10, 1, (MON_W, MON_H - BAR_H))];
        assert!(has_edge_window(&ws, &w, CONNECTOR, MON_W));
    }
```

Note: this test calls `has_edge_window` with **four** arguments (no `mon_h`). The signature change happens in Step 3.

- [ ] **Step 2: Run tests to verify the new test fails**

Run: `cargo test -p trollshell --lib overlays::frame`
Expected: compile error — `has_edge_window` takes 5 arguments, not 4 (the new test passes 4). Also: even if you reverted the test to 5 args including `MON_H`, the helper would return `false` for tile_size `(MON_W, MON_H - BAR_H)` because the height delta of `BAR_H = 44.0` exceeds `FULLSCREEN_TOL = 4.0` — assertion `assert!(...)` would fail.

- [ ] **Step 3: Switch helper to width-only, drop `mon_h` parameter**

In `trollshell/src/overlays/frame.rs`, replace the `has_edge_window` function (added in Task 1, Step 3) with the width-only version:

```rust
/// Return `true` when the active workspace on `connector` contains any
/// window whose tile width spans the full output (within `EDGE_TOL`).
/// Such a window covers the L/R edges where the frame paints its
/// gradient strips and corner masks, so the frame must hide.
///
/// Width alone suffices: niri's maximize-to-edges always covers the
/// full available width AND height (there is no horizontal-only
/// maximize state), and fullscreen does the same. An edge-stretched
/// floating window is treated identically — also the correct visual
/// response, since the frame would overlap its L/R edges.
///
/// The `>=` comparison (vs. two-sided `abs`) is robust against
/// fractional-scale rounding: tile width can never exceed `mon_w` in
/// practice, so a one-sided check covers all real cases.
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
            w.workspace_id == Some(id) && w.layout.tile_size.0 >= mon_w - EDGE_TOL
        })
    })
}
```

Note this references `EDGE_TOL` which is renamed in Step 5.

- [ ] **Step 4: Update `bind_fullscreen_visibility` to drop `mon_h` and use the new signature**

Replace the function (at the location it is after Task 1's Step 4 edit):

```rust
fn bind_fullscreen_visibility(
    window: &gtk::Window,
    connector: String,
    mon_w: f64,
) {
    let workspaces = niri::workspaces();
    let windows = niri::windows();
    let visible = map_ref! {
        let workspaces = workspaces,
        let windows = windows =>
        !has_edge_window(workspaces, windows, &connector, mon_w)
    };
    bind_visible(visible, window);
}
```

Update the call site in `install` (at `frame.rs:81`):

```rust
    bind_fullscreen_visibility(&window, connector, mon_w);
```

(Drops the trailing `, mon_h` argument.) The local `mon_h` binding earlier in `install` (`frame.rs:45`) is still used elsewhere? Verify by reading the surrounding `install` body — `mon_h` is only used as the argument to `bind_fullscreen_visibility`. Once the call drops it, the binding becomes dead. Leave the line `let mon_h = f64::from(mon_h);` in place for one more step — Step 7 removes it after the rename and final test.

Actually: drop the now-unused `mon_h` binding in `install` immediately. The variable shadowing (`let (mon_w, mon_h) = monitor.size(); let mon_w = ...; let mon_h = ...`) is no longer needed for `mon_h`. Edit `install` so it reads:

```rust
    let connector = monitor.connector().unwrap_or_default();
    let (mon_w, _mon_h) = monitor.size();
    let mon_w = f64::from(mon_w);
```

Discard the `mon_h` cast entirely.

- [ ] **Step 5: Rename `FULLSCREEN_TOL` → `EDGE_TOL` and rewrite its doc-comment**

Replace `frame.rs:34-38`:

```rust
/// Tolerance (logical pixels) when comparing a window's tile width to
/// the monitor width to detect an edge-spanning window (fullscreen,
/// maximize-to-edges, or edge-stretched floating). niri reports tile
/// sizes in logical pixels; a few pixels of slack covers fractional-
/// scale rounding.
const EDGE_TOL: f64 = 4.0;
```

The body of `has_edge_window` already references `EDGE_TOL` (Step 3) — once this rename lands, both sides match.

- [ ] **Step 6: Rename `bind_fullscreen_visibility` → `bind_edge_visibility` and rewrite its doc-comment**

Rename the function definition and its call site in `install`. The new function header + doc:

```rust
/// Hide the frame on `window` whenever the active workspace on the
/// output named `connector` contains an edge-spanning window. See
/// `has_edge_window` for the predicate. The trigger covers fullscreen
/// (existing behavior) and maximize-to-edges (new in this change), as
/// well as floating windows manually sized to span the output width.
///
/// `Layer::Overlay` is always above niri's apps by spec — including
/// fullscreen and maximize-to-edges ones — so without this toggle the
/// frame would paint over those windows.
fn bind_edge_visibility(window: &gtk::Window, connector: String, mon_w: f64) {
    let workspaces = niri::workspaces();
    let windows = niri::windows();
    let visible = map_ref! {
        let workspaces = workspaces,
        let windows = windows =>
        !has_edge_window(workspaces, windows, &connector, mon_w)
    };
    bind_visible(visible, window);
}
```

Update the call in `install` (at `frame.rs:81` after Task 1):

```rust
    bind_edge_visibility(&window, connector, mon_w);
```

Also update the existing `// Reactively hide the frame whenever ...` block-comment at `frame.rs:78-80` to mention both states:

```rust
    // Reactively hide the frame whenever this monitor's active workspace
    // has an edge-spanning window — fullscreen or maximize-to-edges.
    // Layer::Overlay is always above niri's apps by spec, so without this
    // toggle the frame would paint over those windows.
    bind_edge_visibility(&window, connector, mon_w);
```

- [ ] **Step 7: Add the within-tolerance test**

Append to `mod tests`, after `has_edge_window_maximize_to_edges`:

```rust
    #[test]
    fn has_edge_window_within_tolerance() {
        // Fractional-scale rounding can put tile width a hair under
        // mon_w. EDGE_TOL = 4.0, so mon_w - 2.0 should still trigger.
        let ws = vec![mk_workspace(1, CONNECTOR, true)];
        let w = vec![mk_window(10, 1, (MON_W - 2.0, MON_H - BAR_H))];
        assert!(has_edge_window(&ws, &w, CONNECTOR, MON_W));
    }
```

- [ ] **Step 8: Update existing baseline tests to drop the `mon_h` argument**

The five tests added in Task 1 (`has_edge_window_normal_tiled`, `_fullscreen`, `_other_workspace_ignored`, `_other_output_ignored`, `_no_active_workspace`) all currently pass `MON_H` as the fifth argument. Strip that argument from each call. Each test's `assert!` line becomes one of:

```rust
        assert!(!has_edge_window(&ws, &w, CONNECTOR, MON_W));
        assert!(has_edge_window(&ws, &w, CONNECTOR, MON_W));
```

The `MON_H` and `BAR_H` constants stay (still used inside `mk_window` tile-size literals).

- [ ] **Step 9: Run tests + build, verify all green**

Run: `cargo test -p trollshell --lib overlays::frame`
Expected: 9 tests pass — 2 existing `cutout_rect_*`, 5 baseline `has_edge_window_*`, plus `has_edge_window_maximize_to_edges` and `has_edge_window_within_tolerance`.

Run: `cargo build -p trollshell`
Expected: clean build, no warnings.

Run: `cargo clippy -p trollshell -- -D warnings`
Expected: clean — no clippy lints. The repo has a `clippy.toml`; the existing module passes lint cleanly today, the new code mirrors its style.

- [ ] **Step 10: Manual verification on the running system**

Build and run trollshell on niri:

```bash
cargo run --release -p trollshell
```

Then in a niri session:

1. Open a normal tiled window (e.g., a terminal). Frame is visible — you should see the dark gradient L/R/bottom strips and rounded inner corners around the workspace cutout.
2. Press `Mod+M` (default niri binding for `MaximizeWindowToEdges`). Frame disappears; the window now reaches L/R/bottom edges of the output (the bar at the top stays visible).
3. Press `Mod+M` again to unmaximize. Frame reappears as the window returns to its strut-inset tile.
4. Toggle fullscreen on a window via your usual fullscreen binding (regression check). Frame still hides.
5. Multi-monitor: maximize-to-edges on monitor A. Frame on A hides; frame on B stays visible.

If any of (1)–(5) fails, do NOT mark this task complete — diagnose with `RUST_LOG=trollshell=debug,hytte_services::niri=debug` and re-check the predicate fixtures vs. what niri actually reports.

- [ ] **Step 11: Commit**

```bash
git add trollshell/src/overlays/frame.rs
git commit -m "feat(frame): hide on maximize-to-edges via width-only check

niri 25.x's MaximizeWindowToEdges expands a window to the screen edges
(bypassing struts) but still respects the bar's exclusive zone. The
existing dual-axis fullscreen check missed this state because the
height delta (= BAR_HEIGHT) far exceeds FULLSCREEN_TOL.

Switch to a single width-only check: tile_size.0 >= mon_w - EDGE_TOL.
That covers fullscreen, maximize-to-edges, and edge-stretched floating
in one rule — every case where the frame's L/R strips would overlap a
window's L/R edges. Drop the now-unused mon_h plumbing and rename the
constant + binding function to *EDGE* for accuracy."
```

---

## Self-review notes (for the implementer)

- The plan introduces `has_edge_window` in Task 1 with the _current_ dual-axis logic so the refactor is behavior-preserving. Task 2 then changes the predicate, drops one parameter, and renames everything. Splitting it this way keeps each commit small and reviewable.
- `niri_ipc::{Window, Workspace}` are re-exported via `hytte::services::niri` (`crates/hytte-services/src/niri.rs:30`). The plan imports them through that re-export to match the rest of the codebase.
- `niri_ipc::WindowLayout` is needed in tests only for fixture construction; import it directly from `niri_ipc` inside `mod tests`.
- `mk_window`/`mk_workspace` use `is_focused: false` and `pos_in_scrolling_layout: Some((1, 1))` because the predicate doesn't read those fields — any valid value works. Keep the builders minimal so that future additions to `Window`/`Workspace` only break compilation in one place.
- The `let mon_h = f64::from(mon_h);` removal in Step 4 is the only edit to `install` outside the call-site rename. It's safe because `mon_h` had only one user.
