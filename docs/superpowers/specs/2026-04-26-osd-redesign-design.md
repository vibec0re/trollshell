# OSD redesign — v0.2.3

**Status:** design
**Date:** 2026-04-26
**Author:** Claude (with annika)
**Predecessors:** `2026-04-24-hytte-trollshell-design.md`, `2026-04-25-trollshell-v0.2.1-polish-design.md`, `2026-04-25-network-panel-redesign-design.md`.

## Goal

Replace the unstyled, primary-monitor-only OSD widget with an Adwaita-card design that mounts on every monitor and shows on the focused monitor only, with fade + slide-in animations. Adds a single derived signal `niri::focused_output()` to route events.

## Scope

### In scope

**Service extension:**
- `crates/hytte-services/src/niri.rs` — add `pub fn focused_output() -> impl Signal<Item = Option<String>>`. Derived from existing `workspaces()` by finding the workspace where `is_focused == true` and reading its `output`.

**UI work in `trollshell/src/widgets/osd.rs`:**
- Restructure the widget tree (Adwaita card; horizontal header [icon | label+value]; progress bar below). Keep the `Kind { Volume, Mic, Brightness }` enum.
- Add `OsdView.value: gtk::Label` for the percent / "Muted" readout.
- Per-monitor mount: replace the single `OSD_VIEW` thread-local with a `OSDS: HashMap<String, Rc<OsdView>>` keyed by `Monitor.connector()`. Public `install(&Monitor)` now appends to that map; `main.rs` calls it once per monitor.
- Module-level subscriptions (set up on first `install` call): the three existing signals (volume, mic, brightness) plus a new subscription to `niri::focused_output()` that updates a `FOCUSED_OUTPUT` thread-local. On signal emission, route the event to the OSD for the focused output; fall back to the first mounted OSD if focused output is unknown / not in the map.
- Fade + slide-in animations driven by CSS transitions on `opacity` and `margin-top`. Rust adds/removes a `.shown` modifier class. A second `glib::timeout` (`fade_out_timeout`) defers `window.set_visible(false)` until the fade completes.
- Bootstrap suppression (existing `Cell<bool>` per signal) is preserved at module level.
- Latest-wins debounce stays per-view (each OsdView has its own `timeout` + `fade_out_timeout`).

**CSS additions in `trollshell/style.css`:**
- ~70 lines of `.ts-osd*` rules. Card backdrop, accent progress, sized icon, muted-state dim, transitions on opacity + margin-top. All using existing `@accent_color` + `@window_bg_color` (or closest available shell-wide tokens, confirmed during implementation by grepping the existing stylesheet).

**`main.rs` change:**
- Replace the single `osd::install(&primary_monitor)` call with a loop over all monitors.

### Out of scope

- New OSD kinds (media play/pause, lock indicator, DND toggle, etc.). The `Kind` enum stays at three variants.
- Per-monitor focus-target overrides (e.g. "always show on primary").
- OSD positioning customization (top-center, 80px from top, fixed).
- Click-dismiss / hover-pause.
- Audio cues.
- Disconnect-while-showing handling beyond a silent no-op (the OsdView is reaped on next bar rebuild).

### Success criteria

- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green.
- Manual:
  - Press volume up/down on a 1-monitor setup → OSD card appears on that monitor with fade + 8px slide-in, hides after ~1.5s with fade-out + slide-out.
  - 2-monitor setup, focus monitor A → poke volume → only A shows. Focus B → poke volume → only B shows.
  - Toggle mic mute → mic OSD with `.muted` modifier (icon dims, progress fades).
  - Brightness keys → brightness OSD.
  - Bootstrap suppression intact: opening the laptop / starting the bar does NOT flash three OSDs in a row.
  - Latest-wins debounce intact: changing volume then brightness within 200ms switches kind in-place; the still-mid-fade-out OSD doesn't disappear early.

## §1 — UI structure (Adwaita card)

The widget tree replaces the current vertical `Box(Image + ProgressBar + Label)` with:

```
gtk::Window (.ts-osd, layer-shell Top, top-center, 80px top margin)
└─ gtk::Box (.ts-osd-card, vertical, 12px spacing)
   ├─ gtk::Box (.ts-osd-header, horizontal, 12px spacing)
   │  ├─ gtk::Image (.ts-osd-icon, set_pixel_size(32))
   │  └─ gtk::Box (vertical, 2px spacing)
   │     ├─ gtk::Label (.ts-osd-label, xalign=0)
   │     └─ gtk::Label (.ts-osd-value, xalign=0)
   └─ gtk::ProgressBar (.ts-osd-progress)
```

`OsdView` field changes:

```rust
struct OsdView {
    window: gtk::Window,
    card: gtk::Box,
    icon: gtk::Image,
    label: gtk::Label,        // was `text`; renamed for clarity
    value: gtk::Label,        // NEW: % / "Muted" readout
    progress: gtk::ProgressBar,
    timeout: Cell<Option<glib::SourceId>>,
    fade_out_timeout: Cell<Option<glib::SourceId>>,  // NEW: defers set_visible(false) until fade completes
    current_kind: Cell<Option<&'static str>>,
    current_muted: Cell<bool>,
}
```

Per-kind content matrix:

| Kind        | Icon                                                                  | Label name     | Value (when not muted)            | Value (muted) | Progress fraction        |
|-------------|-----------------------------------------------------------------------|----------------|-----------------------------------|---------------|--------------------------|
| Volume      | `audio-volume-{muted,low,medium,high}-symbolic`                       | `Volume`       | `{n}%` (rounded from linear×100)  | `Muted`       | `volume.linear` (0..1)   |
| Mic         | `microphone-sensitivity-{muted,medium,high}-symbolic`                 | `Microphone`   | `{n}%`                            | `Muted`       | `source.volume.linear`   |
| Brightness  | `display-brightness-symbolic` (single icon)                           | `Brightness`   | `{n}%` (rounded from level×100)   | (n/a)         | `brightness.level` (0..1)|

Helper functions added in `osd.rs`:

```rust
fn volume_icon(vol: &Volume) -> &'static str {
    if vol.muted { return "audio-volume-muted-symbolic"; }
    let pct = (vol.linear * 100.0).round() as u32;
    match pct {
        0..=33 => "audio-volume-low-symbolic",
        34..=66 => "audio-volume-medium-symbolic",
        _ => "audio-volume-high-symbolic",
    }
}

fn mic_icon(src: &Source) -> &'static str {
    if src.muted { return "microphone-sensitivity-muted-symbolic"; }
    let pct = (src.volume.linear * 100.0).round() as u32;
    if pct >= 50 { "microphone-sensitivity-high-symbolic" }
    else { "microphone-sensitivity-medium-symbolic" }
}
```

The brightness icon is a single name; no helper needed.

Sizing: card `min-width: 280px; max-width: 320px;` via CSS. Icon 32px via `set_pixel_size`. Progress bar `height: 6px;` via CSS.

## §2 — CSS + animations

Append to `trollshell/style.css` (existing `@accent_color` + closest-available shell-wide background token; confirmed during implementation):

```css
.ts-osd {
    background: transparent;
}

.ts-osd-card {
    min-width: 280px;
    max-width: 320px;
    padding: 16px;
    border-radius: 14px;
    background: alpha(@window_bg_color, 0.92);
    box-shadow: 0 4px 16px alpha(black, 0.25);

    opacity: 0;
    margin-top: 0px;
    transition: opacity 200ms ease-out,
                margin-top 200ms ease-out;
}

.ts-osd-card.shown {
    opacity: 1;
    margin-top: 8px;
}

.ts-osd-icon {
    color: @accent_color;
}

.ts-osd-label {
    font-weight: 600;
}

.ts-osd-value {
    font-size: 0.85em;
    opacity: 0.7;
}

.ts-osd-progress trough {
    min-height: 6px;
    background: alpha(@accent_color, 0.15);
    border-radius: 9999px;
}

.ts-osd-progress progress {
    background: @accent_color;
    border-radius: 9999px;
}

.ts-osd-card.muted .ts-osd-icon {
    color: alpha(@accent_color, 0.5);
}

.ts-osd-card.muted .ts-osd-progress progress,
.ts-osd-card.muted .ts-osd-progress trough {
    opacity: 0.3;
}
```

Per-kind tints (`.ts-osd-card.volume`, `.mic`, `.brightness`) are reserved. Defaults are shared accent treatment for v0.2.3.

**Animation mechanism (load-bearing):**

- On show:
  ```rust
  window.set_visible(true);
  let card_for_idle = card.clone();
  glib::idle_add_local_once(move || {
      card_for_idle.add_css_class("shown");
  });
  ```
  The `idle_add_local_once` defers the class flip by one frame so GTK4's CSS engine sees the transition's "from" state (opacity 0 / margin-top 0) before the "to" state (opacity 1 / margin-top 8). Without the defer, the same-frame visibility-and-class flip skips the transition.

- On hide (latest-wins timer fires):
  ```rust
  card.remove_css_class("shown");
  let window_for_fade = window.clone();
  let fade_id = glib::timeout_add_local_once(Duration::from_millis(220), move || {
      window_for_fade.set_visible(false);
  });
  view.fade_out_timeout.set(Some(fade_id));
  ```
  The 220ms = 200ms transition + 20ms safety buffer.

- New event arrives mid-fade-out: cancel `fade_out_timeout`, re-add `.shown`, set `window.set_visible(true)` (idempotent), arm a fresh `timeout` for the 1500ms hide.

`OsdView::show(...)` and `OsdView::hide(...)` become small helpers on a method or free fn for clarity.

## §3 — Multi-monitor mount + focused-output routing

### Service-side (`crates/hytte-services/src/niri.rs`)

Append:

```rust
/// Connector name of the currently focused monitor (e.g. `"DP-1"`).
/// Derived from `workspaces()` — finds the workspace whose
/// `is_focused == true` and reads its `output`. `None` when no
/// workspace is focused or the focused workspace has no output (rare
/// during reconnect).
pub fn focused_output() -> impl Signal<Item = Option<String>> {
    workspaces().map(|ws| {
        ws.iter()
            .find(|w| w.is_focused)
            .and_then(|w| w.output.clone())
    })
}
```

Signal source is the existing `workspaces()` Mutable. Niri publishes a fresh `WorkspacesChanged` whenever focus moves between outputs. No new listen-loop work.

### UI-side (`trollshell/src/widgets/osd.rs`)

Replace the existing single-instance thread-local:

```rust
thread_local! {
    static OSD_VIEW: RefCell<Option<Rc<OsdView>>> = const { RefCell::new(None) };
}
```

with the multi-monitor variants:

```rust
thread_local! {
    /// Mounted OSD instances keyed by `gtk::Monitor.connector()`.
    static OSDS: RefCell<HashMap<String, Rc<OsdView>>> = RefCell::new(HashMap::new());

    /// Most recent focused-output name. Updated by the niri
    /// subscription. Used to route OSD show events to the right
    /// monitor.
    static FOCUSED_OUTPUT: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Set after the first `install()` call to ensure module-level
    /// signal subscriptions are wired exactly once.
    static SUBS_INSTALLED: Cell<bool> = const { Cell::new(false) };
}
```

`pub fn install(monitor: &Monitor)`:

1. If `monitor.connector()` is `None`, log debug and return (no usable key).
2. Build the OsdView (window, card, header, icon, label, value, progress; class-tagged per §1).
3. Insert into `OSDS` keyed by `connector()`.
4. If `SUBS_INSTALLED` is `false`, set it to `true` and wire the four module-level subscriptions (volume, mic, brightness, focused_output). Otherwise return — subscriptions already running.

Routing helper:

```rust
fn route_show(make_state: impl FnOnce(&OsdView)) {
    let target_name: Option<String> = FOCUSED_OUTPUT.with(|c| c.borrow().clone());

    OSDS.with(|map| {
        let map = map.borrow();
        if map.is_empty() {
            return;
        }
        // First try the focused output; fall back to "any mounted OSD"
        // so the user always sees acknowledgment of their input.
        let view = target_name
            .as_ref()
            .and_then(|name| map.get(name))
            .or_else(|| map.values().next());
        if let Some(view) = view {
            make_state(view);
        }
    });
}
```

Each signal subscription's handler becomes:

```rust
// Inside the volume signal subscription:
route_show(|view| view.show(Kind::Volume, /* state extraction */));
```

`OsdView::show` is the existing per-view logic (icon name swap, label/value text, progress fraction, kind/muted CSS class management, timer arm). Bootstrap suppression's `Cell<bool>` flag is checked at the OUTER subscription level, before `route_show` is called.

`focused_output()` subscription is one line:

```rust
glib::MainContext::default().spawn_local(
    niri::focused_output().for_each(|out| {
        FOCUSED_OUTPUT.with(|c| *c.borrow_mut() = out);
        std::future::ready(())
    }),
);
```

### `main.rs` change

Replace the single `osd::install(&primary_monitor)` (or wherever it currently lives — verify by grep) with a loop:

```rust
for monitor in &monitors {
    osd::install(monitor);
}
```

(The bar already iterates `monitors` for per-monitor windows; same iteration carries here. Locate the existing osd::install call site and adjust.)

### Edge cases (handled, called out)

- **Focused output changes mid-OSD-display.** Existing OSD on monitor A continues its hide timer (correct). New event routes to B. Two OSDs visible briefly on different monitors. Acceptable; matches GNOME.
- **Monitor disconnect while OSD showing.** OsdView for the disconnected monitor stays in `OSDS` until next bar rebuild. Attempts to route to the dropped key silently no-op via the focused-output match. Not catastrophic.
- **`Monitor.connector()` returns `None`.** Skip mounting on that monitor. `tracing::debug!` for visibility.
- **`focused_output()` is `None`.** Fallback to first mounted OSD (in arbitrary HashMap order). Rare; only during niri startup before workspaces have been published.

## §4 — Tests

Unit-testable surface is small (the routing helper and the per-kind icon helpers). Most behavior is GTK + animation, manually verified.

- `osd::tests::volume_icon_thresholds` — pure helper test:
  - `Volume { muted: true, .. }` → `audio-volume-muted-symbolic`.
  - `linear: 0.0, muted: false` → low.
  - `linear: 0.5, muted: false` → medium.
  - `linear: 0.9, muted: false` → high.
- `osd::tests::mic_icon_thresholds` — analogous for `Source`.

Routing helper isn't easily unit-tested without mocking the thread-locals; rely on manual smoke testing via the §Success-criteria checklist.

## §5 — Implementation hand-off

After approval, the writing-plans skill produces a step-by-step plan. Suggested decomposition:

1. **Service:** add `niri::focused_output()` derived signal.
2. **UI structure:** rewrite `osd.rs` widget tree (Adwaita card with header / progress) and add `OsdView.value` field.
3. **UI multi-monitor:** swap the single thread-local for `OSDS` HashMap + `FOCUSED_OUTPUT` cell; install per-monitor; route via `route_show`.
4. **UI animations:** Rust-side `.shown` class flip + idle-defer + fade-out timer; CSS transitions.
5. **CSS additions:** the ~70-line block from §2.
6. **Helpers + tests:** `volume_icon` / `mic_icon` per-kind helpers + their unit tests.
7. **`main.rs` integration:** loop `osd::install` over all monitors.
