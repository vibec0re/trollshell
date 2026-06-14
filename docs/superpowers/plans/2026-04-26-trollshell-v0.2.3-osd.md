# trollshell v0.2.3 OSD redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the unstyled, primary-monitor-only OSD with an Adwaita-card design mounted on every monitor, showing on the focused monitor only, with fade + 8px slide-in animations.

**Architecture:** One `niri::focused_output()` derived signal added to the service layer. UI restructured to a horizontal-header card (icon | label+value column) with an accent progress bar. Multi-monitor mount keyed by `Monitor.connector()`; routing picks the OSD whose connector matches niri's focused-output, with first-mounted as fallback. Animations are CSS-driven (`opacity` + `margin-top` transitions) toggled by Rust adding/removing a `.shown` class.

**Tech Stack:** Rust 1.94 stable, GTK4 + libadwaita, `futures-signals`, `glib::idle_add_local_once` + `glib::timeout_add_local_once`, no new deps.

**Conventions used in every task:**

- TDD where unit tests are practical (Task 1's signal helper, Task 7's icon thresholds). UI restructure tasks verify via `cargo build` + `cargo clippy --workspace --all-targets -- -D warnings` and a deferred manual smoke-test note.
- Commits use existing project prefixes: `feat(de):` for shell UI work, `feat(niri):` for service work, `style:` for CSS, `refactor(de):` for restructures.
- Co-author trailer on every commit:
  `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`

**Spec backing this plan:** `docs/superpowers/specs/2026-04-26-osd-redesign-design.md`

---

## File Structure

**Modified files (no new files):**

- `crates/hytte-services/src/niri.rs` — append `pub fn focused_output() -> impl Signal<Item = Option<String>>`.
- `trollshell/src/widgets/osd.rs` — rewrite widget tree, add multi-monitor map, add animations, add per-kind icon helpers.
- `trollshell/src/main.rs` — replace single-monitor `osd::install` call with iteration.
- `trollshell/style.css` — append ~70 lines of `.ts-osd*` rules.

---

## Task 1: `niri::focused_output()` derived signal

**Files:**

- Modify: `crates/hytte-services/src/niri.rs`

**Background:** `Workspace.output: Option<String>` and `Workspace.is_focused: bool` are already published by the existing `workspaces()` Mutable. A small derived signal exposes the currently-focused monitor's connector name without new listening logic.

- [ ] **Step 1: Add `focused_output()` after the existing `workspaces()` getter**

In `crates/hytte-services/src/niri.rs`, find `pub fn workspaces() -> impl Signal<Item = Vec<Workspace>>` (around line 189). Add immediately below:

```rust
/// Connector name of the currently focused monitor (e.g. `"DP-1"`).
/// Derived from [`workspaces()`] by finding the workspace whose
/// `is_focused == true` and reading its `output`. `None` when no
/// workspace is focused or the focused workspace has no output (rare
/// during reconnect / niri startup).
pub fn focused_output() -> impl Signal<Item = Option<String>> {
    use futures_signals::signal::SignalExt;
    workspaces().map(|ws| {
        ws.iter()
            .find(|w| w.is_focused)
            .and_then(|w| w.output.clone())
    })
}
```

(`SignalExt::map` is already in scope via the existing `use` block at the top of the file. The local `use` here is defensive — verify by running clippy. If `SignalExt` is already imported, the local `use` becomes dead code; clippy will flag, remove it.)

- [ ] **Step 2: Build + clippy**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/hytte-services/src/niri.rs
git commit -m "$(cat <<'EOF'
feat(niri): focused_output() derived signal

Returns the connector name of the currently focused monitor by
finding the workspace where is_focused is true and reading its
output. Used by the OSD widget for multi-monitor routing.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: OSD widget tree rewrite (Adwaita card structure)

**Files:**

- Modify: `trollshell/src/widgets/osd.rs`

**Background:** Replace the vertical `Box(Image + ProgressBar + Label)` with a horizontal-header layout: `[icon | label+value column]` above the progress bar. Add a `value: gtk::Label` field for the percent / "Muted" readout, and a `fade_out_timeout: Cell<Option<glib::SourceId>>` for the animation work in Task 3.

This task does NOT yet add the `.shown` class flip or multi-monitor map — `OSD_VIEW` stays as a single thread-local. After this commit, the OSD still works (instant show/hide, primary-monitor only) but with the new structure.

- [ ] **Step 1: Update the `OsdView` struct**

In `trollshell/src/widgets/osd.rs`, find the existing `struct OsdView` (around line 67). Replace with:

```rust
/// Mutable widgets owned by the OSD; rebuilt content swaps icon name,
/// progress fraction, label/value text, and kind/muted CSS classes
/// in place.
struct OsdView {
    window: gtk::Window,
    card: gtk::Box,
    icon: gtk::Image,
    label: gtk::Label,        // kind name: "Volume" / "Microphone" / "Brightness"
    value: gtk::Label,        // percent / "Muted"
    progress: gtk::ProgressBar,
    /// Pending hide timeout. Held so each new event can cancel and
    /// re-arm it (latest-wins debounce).
    timeout: Cell<Option<glib::SourceId>>,
    /// Pending fade-out → set_visible(false) timeout. Cancelled if a
    /// new event arrives mid-fade-out.
    fade_out_timeout: Cell<Option<glib::SourceId>>,
    /// CSS modifier classes currently set on the card so we can clean
    /// them up on each update without growing a leaky class list.
    current_kind: Cell<Option<&'static str>>,
    current_muted: Cell<bool>,
}
```

(The existing `text: gtk::Label` field is renamed to `label` and a new `value: gtk::Label` is added. The new `fade_out_timeout` field is initialized to `Cell::new(None)` in the constructor.)

- [ ] **Step 2: Update the `install` widget construction**

Locate the widget construction inside `pub fn install(monitor: &Monitor)` (around lines 101-128). Replace from `let card = gtk::Box::new(...)` through the `Rc::new(OsdView { ... })` block with the new shape:

```rust
let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
card.add_css_class("ts-osd-card");

let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
header.add_css_class("ts-osd-header");

let icon = gtk::Image::new();
icon.add_css_class("ts-osd-icon");
icon.set_pixel_size(32);
header.append(&icon);

let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
column.set_hexpand(true);
let label = gtk::Label::new(None);
label.add_css_class("ts-osd-label");
label.set_xalign(0.0);
column.append(&label);
let value = gtk::Label::new(None);
value.add_css_class("ts-osd-value");
value.set_xalign(0.0);
column.append(&value);
header.append(&column);

card.append(&header);

let progress = gtk::ProgressBar::new();
progress.add_css_class("ts-osd-progress");
card.append(&progress);

window.set_child(Some(&card));

let view = Rc::new(OsdView {
    window,
    card,
    icon,
    label,
    value,
    progress,
    timeout: Cell::new(None),
    fade_out_timeout: Cell::new(None),
    current_kind: Cell::new(None),
    current_muted: Cell::new(false),
});
```

- [ ] **Step 3: Update the `State` struct + `render_*` functions to populate label and value separately**

Find `struct State` (around line 209). Replace with:

```rust
/// Rendered OSD state — what to display once a signal fires.
struct State {
    kind: Kind,
    icon: &'static str,
    fraction: f64,
    /// Kind name shown in `.ts-osd-label` ("Volume" / "Microphone" /
    /// "Brightness").
    label: &'static str,
    /// Percent / "Muted" text shown in `.ts-osd-value`.
    value: String,
    muted: bool,
}
```

Update `render_volume` (around line 217):

```rust
fn render_volume(v: Volume) -> State {
    let icon = volume_icon(&v);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct = (v.linear * 100.0).round() as u32;
    let value = if v.muted {
        "Muted".to_string()
    } else {
        format!("{pct}%")
    };
    State {
        kind: Kind::Volume,
        icon,
        fraction: clamp01(v.linear),
        label: "Volume",
        value,
        muted: v.muted,
    }
}
```

Update `render_mic` (around line 241):

```rust
fn render_mic(source: Option<&Source>) -> Option<State> {
    let s = source?;
    let icon = mic_icon(s);
    let pct = pct(s.volume);
    let value = if s.muted {
        "Muted".to_string()
    } else {
        format!("{pct}%")
    };
    Some(State {
        kind: Kind::Mic,
        icon,
        fraction: clamp01(s.volume),
        label: "Microphone",
        value,
        muted: s.muted,
    })
}
```

(`volume_icon` and `mic_icon` are added in Task 7. For this commit, inline placeholders that match the existing single-icon behavior are fine — the helpers replace them then. To avoid churn, define them now as one-liners that match the current behavior:)

```rust
fn volume_icon(v: &Volume) -> &'static str {
    if v.muted { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" }
}

fn mic_icon(s: &Source) -> &'static str {
    if s.muted { "microphone-sensitivity-muted-symbolic" } else { "audio-input-microphone-symbolic" }
}
```

(Task 7 replaces these with level-banded variants and adds tests.)

Update `render_brightness` (around line 259):

```rust
fn render_brightness(b: Brightness) -> State {
    let pct = pct(b.level);
    State {
        kind: Kind::Brightness,
        icon: "display-brightness-symbolic",
        fraction: clamp01(b.level),
        label: "Brightness",
        value: format!("{pct}%"),
        muted: false,
    }
}
```

- [ ] **Step 4: Update the `show` function to set both label and value**

Find `fn show(view: &Rc<OsdView>, state: &State)` (around line 273). Replace the body's text-setting line:

Before:

```rust
view.text.set_text(&state.text);
```

After:

```rust
view.label.set_text(state.label);
view.value.set_text(&state.value);
```

The rest of `show()` (icon name, progress fraction, kind/muted class management, timeout handling) is unchanged in this task. Animations come in Task 3.

- [ ] **Step 5: Update the module docstring's CSS hooks list**

Around line 22-30 of the file (the `//! CSS hooks (intentionally bar-prefixed `ts-`):` block), update to reflect the new structure:

```rust
//! CSS hooks (intentionally bar-prefixed `ts-`):
//! - window root: `.ts-osd`
//! - inner card: `.ts-osd-card`
//! - header (icon + label/value column): `.ts-osd-header`
//! - icon: `.ts-osd-icon`
//! - kind label: `.ts-osd-label`
//! - value readout: `.ts-osd-value`
//! - progress bar: `.ts-osd-progress`
//! - kind modifier: `.volume`, `.mic`, `.brightness`
//! - state modifier: `.muted` (when applicable)
//! - shown modifier: `.shown` (toggled by Rust to drive CSS transitions)
```

- [ ] **Step 6: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. The OSD now renders with the new structure; pills/icons appear unstyled until Task 6 adds the CSS.

- [ ] **Step 7: Commit**

```bash
git add trollshell/src/widgets/osd.rs
git commit -m "$(cat <<'EOF'
refactor(de): OSD — Adwaita card widget tree

Replaces the vertical Box(Image + ProgressBar + Label) with a
horizontal header (icon | label+value column) above the progress
bar. Splits the existing `text` label into a kind name ("Volume"/
"Microphone"/"Brightness") and a value readout (percent / "Muted").
Adds OsdView.fade_out_timeout placeholder for the animation work
landing next. Single-monitor mount unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: OSD animations (fade + slide-in)

**Files:**

- Modify: `trollshell/src/widgets/osd.rs`

**Background:** Animations are CSS-driven (transitions on `opacity` and `margin-top` in Task 6's stylesheet). This task adds the Rust-side hooks: toggle `.shown` class with a one-frame delay so GTK4's CSS engine sees the from-state before the to-state, and a fade-out timer that defers `set_visible(false)` until after the transition completes.

- [ ] **Step 1: Update `show` to use the `.shown` class flip pattern**

Find the existing `show()` function in `osd.rs` (around line 273). Replace from `view.window.set_visible(true);` through the end of the function:

```rust
// Make the window visible, then defer the .shown class flip by one
// frame so GTK4's CSS engine sees the transition's "from" state
// (opacity 0, margin-top 0) before the "to" state. Without the
// idle defer, the same-frame visibility-and-class flip skips the
// transition entirely.
//
// Cancel any in-flight fade-out: a new event landed before the
// previous fade-out timer fired, so we keep the window visible
// and re-arm.
if let Some(prev) = view.fade_out_timeout.take() {
    prev.remove();
}
view.window.set_visible(true);
let card_for_idle = view.card.clone();
glib::idle_add_local_once(move || {
    card_for_idle.add_css_class("shown");
});

// Reset auto-hide. When the timer fires we DON'T set_visible(false)
// directly; instead remove the .shown class to start the CSS fade,
// then schedule a second timer to flip visibility once the
// transition has finished.
if let Some(prev) = view.timeout.take() {
    prev.remove();
}
let view_for_timeout = view.clone();
let id = glib::timeout_add_local_once(
    Duration::from_millis(u64::from(HIDE_AFTER_MS)),
    move || {
        view_for_timeout.timeout.set(None);
        view_for_timeout.card.remove_css_class("shown");
        // Wait for the 200ms CSS transition + 20ms safety buffer
        // before actually hiding the layer-shell window.
        let view_for_fade = view_for_timeout.clone();
        let fade_id = glib::timeout_add_local_once(
            Duration::from_millis(220),
            move || {
                view_for_fade.fade_out_timeout.set(None);
                view_for_fade.window.set_visible(false);
            },
        );
        view_for_timeout.fade_out_timeout.set(Some(fade_id));
    },
);
view.timeout.set(Some(id));
```

(The body before the visibility section — icon name swap, label/value text, progress fraction, kind/muted class management — is unchanged from Task 2.)

- [ ] **Step 2: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. The OSD now flips `.shown` correctly but visually still looks like Task 2 (CSS arrives in Task 6).

- [ ] **Step 3: Commit**

```bash
git add trollshell/src/widgets/osd.rs
git commit -m "$(cat <<'EOF'
feat(de): OSD — fade + slide-in animation hooks

Toggles a .shown CSS class on the card with a one-frame idle
defer so GTK4 sees the transition's "from" state before the "to"
state. The latest-wins hide timer removes the class to start the
fade-out, then a 220ms (200ms transition + 20ms safety) timer
defers set_visible(false) until the transition completes. New
events arriving mid-fade-out cancel the deferred-hide and re-arm.

CSS transitions land in a subsequent commit.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: OSD multi-monitor mount + focused-output routing

**Files:**

- Modify: `trollshell/src/widgets/osd.rs`

**Background:** Replace the single `OSD_VIEW` thread-local with `OSDS: HashMap<String, Rc<OsdView>>` keyed by `Monitor.connector()`. Subscriptions move to module-level (set up exactly once on first `install` call); each emission routes to the focused-output's OSD via a new `route_show` helper.

- [ ] **Step 1: Replace the thread-local declarations**

Find the existing `OSD_VIEW` thread-local (around line 202-206) and replace with:

```rust
thread_local! {
    /// Mounted OSD instances keyed by `gtk::Monitor.connector()`.
    /// `connector()` matches niri's `Workspace.output` (KMS connector
    /// names like `"DP-1"`, `"eDP-1"`).
    static OSDS: RefCell<HashMap<String, Rc<OsdView>>> =
        RefCell::new(HashMap::new());

    /// Most recent focused-output name from
    /// [`hytte::services::niri::focused_output`]. Updated by the
    /// module-level subscription.
    static FOCUSED_OUTPUT: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Set after the first `install()` call to ensure module-level
    /// signal subscriptions are wired exactly once across all
    /// per-monitor mounts.
    static SUBS_INSTALLED: Cell<bool> = const { Cell::new(false) };
}
```

Add `use std::collections::HashMap;` at the top if not already present.

Add to imports near the existing `hytte::services::*` block:

```rust
use hytte::services::niri;
```

- [ ] **Step 2: Restructure `install()` to mount per-monitor + route subscriptions**

Replace the entire body of `pub fn install(monitor: &Monitor)` (around lines 86-200) with:

```rust
pub fn install(monitor: &Monitor) {
    let connector = match monitor.connector() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => {
            tracing::debug!("osd::install: monitor has no connector name; skipping");
            return;
        }
    };

    let view = build_osd_view(monitor);
    OSDS.with(|map| {
        map.borrow_mut().insert(connector, view);
    });

    if !SUBS_INSTALLED.with(Cell::get) {
        SUBS_INSTALLED.with(|c| c.set(true));
        install_subscriptions();
    }
}

/// Construct one OsdView for `monitor`. Pure widget construction —
/// signal wiring lives in `install_subscriptions()` and runs once
/// regardless of monitor count.
fn build_osd_view(monitor: &Monitor) -> Rc<OsdView> {
    let window = layer_window(monitor)
        .layer(Layer::Overlay)
        .anchor(Anchor::Top)
        .margin(Margin {
            top: TOP_MARGIN,
            ..Margin::default()
        })
        .namespace("hytte-osd")
        .exclusive(false)
        .keyboard_mode(KeyboardMode::None)
        .build();
    window.add_css_class("ts-osd");
    window.set_visible(false);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.add_css_class("ts-osd-card");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    header.add_css_class("ts-osd-header");

    let icon = gtk::Image::new();
    icon.add_css_class("ts-osd-icon");
    icon.set_pixel_size(32);
    header.append(&icon);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
    column.set_hexpand(true);
    let label = gtk::Label::new(None);
    label.add_css_class("ts-osd-label");
    label.set_xalign(0.0);
    column.append(&label);
    let value = gtk::Label::new(None);
    value.add_css_class("ts-osd-value");
    value.set_xalign(0.0);
    column.append(&value);
    header.append(&column);

    card.append(&header);

    let progress = gtk::ProgressBar::new();
    progress.add_css_class("ts-osd-progress");
    card.append(&progress);

    window.set_child(Some(&card));

    Rc::new(OsdView {
        window,
        card,
        icon,
        label,
        value,
        progress,
        timeout: Cell::new(None),
        fade_out_timeout: Cell::new(None),
        current_kind: Cell::new(None),
        current_muted: Cell::new(false),
    })
}

/// Wire the four module-level signal subscriptions exactly once on
/// the first `install()` call, regardless of monitor count.
fn install_subscriptions() {
    // Volume.
    {
        let first = Cell::new(true);
        glib::MainContext::default().spawn_local(
            pipewire::default_sink().for_each(move |v: Volume| {
                if first.replace(false) {
                    return std::future::ready(());
                }
                let state = render_volume(v);
                route_show(&state);
                std::future::ready(())
            }),
        );
    }

    // Microphone (default source) — derive from sources() like before.
    {
        let first = Cell::new(true);
        let combined = map_ref! {
            let sources = pipewire::sources() => {
                sources.iter().find(|s| s.is_default).cloned()
            }
        }
        .dedupe_cloned();

        glib::MainContext::default().spawn_local(combined.for_each(
            move |source: Option<Source>| {
                if first.replace(false) {
                    return std::future::ready(());
                }
                if let Some(state) = render_mic(source.as_ref()) {
                    route_show(&state);
                }
                std::future::ready(())
            },
        ));
    }

    // Brightness.
    {
        let first = Cell::new(true);
        glib::MainContext::default().spawn_local(brightness::current().for_each(
            move |b: Option<Brightness>| {
                if first.replace(false) {
                    return std::future::ready(());
                }
                if let Some(b) = b {
                    let state = render_brightness(b);
                    route_show(&state);
                }
                std::future::ready(())
            },
        ));
    }

    // Focused output — updates FOCUSED_OUTPUT used by route_show.
    glib::MainContext::default().spawn_local(
        niri::focused_output().for_each(|out| {
            FOCUSED_OUTPUT.with(|c| *c.borrow_mut() = out);
            std::future::ready(())
        }),
    );
}

/// Route `state` to the OSD on the focused monitor. Falls back to
/// the first mounted OSD when the focused output is unknown or not
/// in the map (e.g. niri startup, monitor disconnect).
fn route_show(state: &State) {
    let target_name: Option<String> = FOCUSED_OUTPUT.with(|c| c.borrow().clone());
    OSDS.with(|map| {
        let map = map.borrow();
        if map.is_empty() {
            return;
        }
        let view = target_name
            .as_ref()
            .and_then(|name| map.get(name))
            .or_else(|| map.values().next());
        if let Some(view) = view {
            show(view, state);
        }
    });
}
```

(The previous in-line subscription wiring inside `install()` is now consolidated in `install_subscriptions()`. The widget construction is in `build_osd_view()`. The single-OSD `OSD_VIEW` thread-local is gone — replaced by `OSDS`.)

- [ ] **Step 3: Verify the old `OSD_VIEW` references are gone**

Run: `grep -n 'OSD_VIEW' trollshell/src/widgets/osd.rs`
Expected: 0 matches.

- [ ] **Step 4: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. The OSD still mounts on only one monitor for now (main.rs change comes in Task 5); routing to the focused output is wired but with a single-entry map, focused-output match always falls back to the only entry.

- [ ] **Step 5: Commit**

```bash
git add trollshell/src/widgets/osd.rs
git commit -m "$(cat <<'EOF'
feat(de): OSD — multi-monitor mount + focused-output routing

Replaces the single OSD_VIEW thread-local with OSDS keyed by
Monitor.connector(). Subscriptions (volume / mic / brightness /
focused_output) move to install_subscriptions() and run exactly
once across all per-monitor mounts. route_show() picks the OSD
on the focused output, falling back to the first mounted OSD
when the focused output is unknown or missing from the map.

main.rs continues to install on the primary monitor only; the
loop swap lands next.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `main.rs` — install OSD on every monitor

**Files:**

- Modify: `trollshell/src/main.rs`

**Background:** With Task 4's per-monitor mount in place, the OSD only needs main.rs to call `install()` for each monitor. The other primary-only widgets (notifications, prompt, polkit_dialog) stay primary-only — their semantics differ.

- [ ] **Step 1: Replace the single-monitor `osd::install` call with iteration**

In `trollshell/src/main.rs`, find the block (around line 73-78):

```rust
if let Some(primary) = app.monitors().first() {
    widgets::notifications::install(primary);
    widgets::prompt::install(primary);
    widgets::polkit_dialog::install(primary);
    widgets::osd::install(primary);
}
```

Replace with:

```rust
if let Some(primary) = app.monitors().first() {
    widgets::notifications::install(primary);
    widgets::prompt::install(primary);
    widgets::polkit_dialog::install(primary);
}

// OSD mounts on every monitor; routing picks the focused one.
for monitor in &app.monitors() {
    widgets::osd::install(monitor);
}
```

(Verify the iteration shape against the existing bar setup. If `app.monitors()` returns a `Vec<Monitor>` directly, `for monitor in &app.monitors()` works. If it's a `gio::ListModel`, use whatever conversion the bar code already uses for per-monitor windows. Read main.rs around line 80+ to find the existing per-monitor iteration pattern and match it.)

- [ ] **Step 2: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add trollshell/src/main.rs
git commit -m "$(cat <<'EOF'
feat(de): install OSD on every monitor

OSD mounts on each monitor; route_show inside the widget picks
the focused-output's instance. Notifications / prompt /
polkit_dialog stay primary-only — their semantics are different.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: CSS — `.ts-osd*` styling + transitions

**Files:**

- Modify: `trollshell/style.css`

**Background:** ~70 lines. All rules use existing tokens (`@accent_color`, `@window_bg_color`); confirm by grepping the file before adding. If `@window_bg_color` isn't present, fall back to whatever shell-wide background token is in use (e.g. `@theme_bg_color`, `@card_bg_color`).

- [ ] **Step 1: Verify available tokens**

Run: `grep -nE '@accent_color|@window_bg_color|@theme_bg_color|@card_bg_color' trollshell/style.css | head -20`
Note which background-style token the file already uses. The plan assumes `@window_bg_color`; if a different one is in use, substitute it consistently below.

- [ ] **Step 2: Append the rules to `trollshell/style.css`**

At the bottom of the file:

```css
/* ── OSD ────────────────────────────────────────────────────────────────── */

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
  transition:
    opacity 200ms ease-out,
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

(If Step 1 surfaced a different background token, substitute it for `@window_bg_color` in the `.ts-osd-card` rule.)

- [ ] **Step 3: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean (CSS isn't compiled; this just confirms nothing else broke).

- [ ] **Step 4: Commit**

```bash
git add trollshell/style.css
git commit -m "$(cat <<'EOF'
style: OSD card + transitions

Adwaita-styled card with rounded backdrop, accent-tinted icon,
accent progress fill, and 200ms opacity + margin-top transitions
driven by the .shown class. Muted state dims icon and progress.
All rules use existing @accent_color + @window_bg_color tokens;
no new color tokens introduced.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Per-kind icon helpers + unit tests

**Files:**

- Modify: `trollshell/src/widgets/osd.rs`

**Background:** Task 2 stubbed `volume_icon(&Volume)` and `mic_icon(&Source)` with single-icon placeholders. This task replaces them with level-banded variants (low/medium/high) and adds unit tests for the thresholds.

- [ ] **Step 1: Write the failing tests**

Append to `osd.rs` (or create) a `#[cfg(test)] mod tests` block at the bottom of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use hytte::services::pipewire::{Source, Volume};

    fn vol(linear: f64, muted: bool) -> Volume {
        Volume { linear, muted, ..Volume::default() }
    }

    fn src(volume: f64, muted: bool) -> Source {
        Source { volume, muted, ..Source::default() }
    }

    #[test]
    fn volume_icon_muted() {
        assert_eq!(volume_icon(&vol(0.5, true)), "audio-volume-muted-symbolic");
    }

    #[test]
    fn volume_icon_low_band() {
        assert_eq!(volume_icon(&vol(0.0, false)), "audio-volume-low-symbolic");
        assert_eq!(volume_icon(&vol(0.33, false)), "audio-volume-low-symbolic");
    }

    #[test]
    fn volume_icon_medium_band() {
        assert_eq!(volume_icon(&vol(0.5, false)), "audio-volume-medium-symbolic");
        assert_eq!(volume_icon(&vol(0.66, false)), "audio-volume-medium-symbolic");
    }

    #[test]
    fn volume_icon_high_band() {
        assert_eq!(volume_icon(&vol(0.67, false)), "audio-volume-high-symbolic");
        assert_eq!(volume_icon(&vol(1.0, false)), "audio-volume-high-symbolic");
    }

    #[test]
    fn mic_icon_muted() {
        assert_eq!(mic_icon(&src(0.5, true)), "microphone-sensitivity-muted-symbolic");
    }

    #[test]
    fn mic_icon_high_band() {
        assert_eq!(mic_icon(&src(0.5, false)), "microphone-sensitivity-high-symbolic");
        assert_eq!(mic_icon(&src(1.0, false)), "microphone-sensitivity-high-symbolic");
    }

    #[test]
    fn mic_icon_medium_band() {
        assert_eq!(mic_icon(&src(0.0, false)), "microphone-sensitivity-medium-symbolic");
        assert_eq!(mic_icon(&src(0.49, false)), "microphone-sensitivity-medium-symbolic");
    }
}
```

(`Volume` and `Source` derive `Default` per the existing service definitions — verify by reading `crates/hytte-services/src/pipewire.rs`. If they don't, replace `..Volume::default()` / `..Source::default()` with explicit zero-fills for the remaining fields.)

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p trollshell osd::tests -- --nocapture`
Expected: tests fail because the placeholder helpers from Task 2 return only "muted" / "high" / "medium" / "low" with no banding.

- [ ] **Step 3: Replace `volume_icon` and `mic_icon` with level-banded versions**

In `osd.rs`, find the placeholder helpers added in Task 2. Replace with:

```rust
fn volume_icon(v: &Volume) -> &'static str {
    if v.muted {
        return "audio-volume-muted-symbolic";
    }
    let pct = (clamp01(v.linear) * 100.0).round() as u32;
    match pct {
        0..=33 => "audio-volume-low-symbolic",
        34..=66 => "audio-volume-medium-symbolic",
        _ => "audio-volume-high-symbolic",
    }
}

fn mic_icon(s: &Source) -> &'static str {
    if s.muted {
        return "microphone-sensitivity-muted-symbolic";
    }
    let pct = (clamp01(s.volume) * 100.0).round() as u32;
    if pct >= 50 {
        "microphone-sensitivity-high-symbolic"
    } else {
        "microphone-sensitivity-medium-symbolic"
    }
}
```

(Casts may need `#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]` to satisfy the workspace lints. Add as needed; the existing `pct()` helper near the bottom of `osd.rs` already does this — match that style.)

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test -p trollshell osd::tests -- --nocapture`
Expected: all 8 tests pass.

- [ ] **Step 5: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add trollshell/src/widgets/osd.rs
git commit -m "$(cat <<'EOF'
feat(de): OSD — level-banded icons for volume + microphone

volume_icon picks audio-volume-{low,medium,high}-symbolic based
on percent bands (0-33 / 34-66 / 67+). mic_icon picks
microphone-sensitivity-{medium,high}-symbolic at the 50% threshold.
Both fall through to {muted,muted}-symbolic when muted is set.
Eight unit tests cover the threshold boundaries.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review notes

**Spec coverage:**

- Spec §1 UI structure → Task 2.
- Spec §2 CSS + animations → Task 3 (Rust hooks) + Task 6 (CSS).
- Spec §3 multi-monitor + focused-output routing → Task 1 (signal) + Task 4 (mount/route) + Task 5 (main.rs).
- Spec §4 tests → Task 7.
- Spec §5 implementation hand-off ordering → matches Task 1 → Task 2 → Task 3 → Task 4 → Task 5 → Task 6 → Task 7.

**Final verification:**

- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green (8 new icon-helper tests).
- Manual smoke test (deferred) covers each spec success criterion.
