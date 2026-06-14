# trollshell v0.2.5 stats page redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Adwaita-conformant Stats drawer page (vertical `AdwPreferencesGroup` stack), per-core CPU bars switched from a hardcoded hex gradient to `@accent_color`, plus new `Sparkline` widget + History group, sensors swap+process_count extensions, and a new signal-driven `systemd` service exposing failed units.

**Architecture:** New reusable `hytte_ui::Sparkline` widget (cairo-drawn ring buffer) consumed by the History group's four time-series rows. Sensors gain swap fields and a process count piggybacking on the existing 1Hz polling tick. New `hytte-services::systemd` service uses `Manager.Subscribe()` + `JobRemoved` signal to push failed-unit list updates. UI restructures `page_stats` to three vertically-stacked groups (Live / History / Services) inside `finish_page` Clamp, dropping the legacy `page_grid` + `panel("…")` layout.

**Tech Stack:** Rust 1.94 stable, GTK4 + libadwaita via gtk4-rs / cairo (already a transitive dep), `futures-signals`, `zbus`. No new top-level deps.

**Conventions used in every task:**

- TDD where unit-testable (sparkline ring buffer, swap parser, systemd parser).
- Commits use existing project prefixes: `feat(ui):`, `feat(sensors):`, `feat(systemd):`, `feat(de):`, `style:`, `refactor(de):`.
- Co-author trailer on every commit:
  `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`

**Spec backing this plan:** `docs/superpowers/specs/2026-04-26-stats-page-redesign-design.md`

---

## File Structure

**Created:**

- `crates/hytte-ui/src/sparkline.rs` — new `Sparkline` widget primitive.
- `crates/hytte-services/src/systemd.rs` — new service module.

**Modified:**

- `crates/hytte-ui/src/lib.rs` — re-export `Sparkline`.
- `crates/hytte-ui/Cargo.toml` — verify cairo accessibility (already transitive; add `cairo-rs` only if needed).
- `crates/hytte-services/src/sensors.rs` — extend `Memory`, add `process_count()` signal + reader.
- `crates/hytte-services/src/lib.rs` — re-export `systemd` module.
- `crates/hytte-services/Cargo.toml` — no changes (zbus already present).
- `trollshell/src/main.rs` — register systemd service.
- `trollshell/src/widgets/pages.rs` — rewrite `page_stats` (Live/History/Services groups), wire sparklines.
- `trollshell/style.css` — replace per-core gradient + append stats CSS.

---

## Task 1: sensors `Memory` swap fields + parser test

**Files:**

- Modify: `crates/hytte-services/src/sensors.rs`

**Background:** `Memory` currently has `{ total, free, available, used }`. Add `swap_used` + `swap_total` parsed from `SwapTotal` and `SwapFree` lines in the existing `/proc/meminfo` reader. Backward-compat: `Default` initializes to 0, existing consumers ignore the new fields.

- [ ] **Step 1: Write the failing test**

In `crates/hytte-services/src/sensors.rs`, find or create a `#[cfg(test)] mod tests` block at the bottom of the file. Append:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_meminfo_extracts_swap_fields() {
        let text = "\
MemTotal:       16331836 kB
MemFree:         1234567 kB
MemAvailable:    8000000 kB
SwapTotal:       8388604 kB
SwapFree:        4194302 kB
";
        let m = parse_meminfo(text);
        assert_eq!(m.total, 16331836 * 1024);
        assert_eq!(m.swap_total, 8388604 * 1024);
        // SwapTotal - SwapFree = 4194302 kB used
        assert_eq!(m.swap_used, 4194302 * 1024);
    }

    #[test]
    fn parse_meminfo_zero_swap_when_missing() {
        let text = "\
MemTotal:       16331836 kB
MemFree:         1234567 kB
MemAvailable:    8000000 kB
";
        let m = parse_meminfo(text);
        assert_eq!(m.swap_total, 0);
        assert_eq!(m.swap_used, 0);
    }
}
```

The test calls a `parse_meminfo(text: &str) -> Memory` helper that doesn't exist yet — the existing `read_proc_meminfo` reads the file directly. Step 3 extracts the parser.

- [ ] **Step 2: Run tests — verify they fail**

Run: `cargo test -p hytte-services sensors::tests::parse_meminfo -- --nocapture`
Expected: compile error — `parse_meminfo` not defined and/or `Memory` lacks `swap_*` fields.

- [ ] **Step 3: Add `swap_*` fields + extract parser + populate**

Find `Memory` struct (around line 39):

```rust
#[derive(Clone, Debug, Default)]
pub struct Memory {
    pub total: u64,
    pub free: u64,
    pub available: u64,
    pub used: u64,
    pub swap_used: u64,
    pub swap_total: u64,
}
```

(Keep all 4 existing fields — `free` stays.)

Find `read_proc_meminfo()` (around line 494). Refactor body to delegate to `parse_meminfo`:

```rust
fn read_proc_meminfo() -> Result<Memory, std::io::Error> {
    let text = std::fs::read_to_string("/proc/meminfo")?;
    Ok(parse_meminfo(&text))
}

fn parse_meminfo(text: &str) -> Memory {
    let mut total_kb: u64 = 0;
    let mut free_kb: u64 = 0;
    let mut available_kb: u64 = 0;
    let mut swap_total_kb: u64 = 0;
    let mut swap_free_kb: u64 = 0;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemFree:") {
            free_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("SwapTotal:") {
            swap_total_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("SwapFree:") {
            swap_free_kb = parse_kb(rest);
        }
    }

    let total = total_kb * 1024;
    let free = free_kb * 1024;
    let available = available_kb * 1024;
    let used = total.saturating_sub(available);
    let swap_total = swap_total_kb * 1024;
    let swap_free = swap_free_kb * 1024;
    let swap_used = swap_total.saturating_sub(swap_free);

    Memory {
        total,
        free,
        available,
        used,
        swap_used,
        swap_total,
    }
}
```

- [ ] **Step 4: Run tests — verify they pass**

Run: `cargo test -p hytte-services sensors::tests::parse_meminfo -- --nocapture`
Expected: 2 passed.

- [ ] **Step 5: Build + clippy**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/sensors.rs
git commit -m "$(cat <<'EOF'
feat(sensors): expose swap_used + swap_total on Memory

Extends Memory with swap_used / swap_total parsed from SwapTotal +
SwapFree in /proc/meminfo. Refactors the parser into a pure
parse_meminfo(&str) helper for unit-testability.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: sensors `process_count()` signal

**Files:**

- Modify: `crates/hytte-services/src/sensors.rs`

**Background:** Counts entries in `/proc/<num>/` once per existing 1Hz tick. Storage on `SensorsHandles`; pollster piggybacks on the existing listen loop.

- [ ] **Step 1: Add the field to `SensorsHandles`**

Find `SensorsHandles` (around line 136). Add a new field:

```rust
pub(crate) process_count: Mutable<u32>,
```

Update `Default for SensorsHandles` to include `process_count: Mutable::new(0),`.

- [ ] **Step 2: Add the public signal**

Below the existing `pub fn cpu()` / `pub fn memory()` / etc. (around line 264), add:

```rust
pub fn process_count() -> impl Signal<Item = u32> {
    registry::with(|r| {
        r.get::<SensorsHandles>()
            .expect("sensors::service() not registered")
            .process_count
            .signal_cloned()
    })
}
```

- [ ] **Step 3: Add the reader function**

Near the bottom of the file, alongside other `read_*` helpers:

```rust
fn read_process_count() -> u32 {
    std::fs::read_dir("/proc")
        .map(|iter| {
            #[allow(clippy::cast_possible_truncation)]
            let count = iter
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.parse::<u32>().is_ok())
                })
                .count();
            count.try_into().unwrap_or(u32::MAX)
        })
        .unwrap_or(0)
}
```

- [ ] **Step 4: Wire into the polling loop**

Find the existing 1Hz polling block (search for `tokio::time::sleep` in the listen loop around the cpu/memory writers). Add after the existing `mem_writer.set(read_proc_meminfo()...)` call (or wherever memory is updated):

```rust
proc_count_writer.set(read_process_count());
```

To get `proc_count_writer` into scope, add to the cloning block at the start of the listen loop (mirroring how `mem_writer` is cloned):

```rust
let proc_count_writer = handles.process_count.clone();
```

(Match the existing pattern: each writer is `let writer = handles.field.clone();` near the spawn site, then captured into the polling closure.)

- [ ] **Step 5: Build + clippy + tests**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p hytte-services`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/sensors.rs
git commit -m "$(cat <<'EOF'
feat(sensors): process_count signal

Counts numeric entries in /proc/ on every 1Hz polling tick. Used
by the v0.2.5 stats page Live group's Processes row.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `systemd` service module

**Files:**

- Create: `crates/hytte-services/src/systemd.rs`
- Modify: `crates/hytte-services/src/lib.rs`
- Modify: `trollshell/src/main.rs`

**Background:** Subscribe to `org.freedesktop.systemd1.Manager.JobRemoved` signal; seed via `ListUnitsFiltered(["failed"])`; refresh on each signal. **Must call `Manager.Subscribe()`** — systemd doesn't emit signals to non-subscribed clients.

- [ ] **Step 1: Create the new module**

Create `crates/hytte-services/src/systemd.rs`:

````rust
//! systemd service — surfaces the current set of failed units via
//! `org.freedesktop.systemd1.Manager`. Signal-driven: subscribes to
//! `JobRemoved` and re-fetches `ListUnitsFiltered(["failed"])` on
//! each emission.
//!
//! Notes on systemd dbus:
//! - `Manager.Subscribe()` MUST be called for the daemon to start
//!   emitting signals to this client. Without it `JobRemoved` never
//!   fires.
//! - `JobRemoved` covers every unit transition (start/stop/restart
//!   complete) regardless of result, so it's a reasonable proxy for
//!   "the failed-unit set may have changed". Cheaper than per-unit
//!   PropertiesChanged subscriptions for the v0.2.5 fidelity.
//!
//! # Public API
//!
//! ```ignore
//! .with(systemd::service())
//!
//! systemd::failed_units() -> impl Signal<Item = Vec<FailedUnit>>
//! ```

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_reactive::{registry, Service};
use std::time::Duration;
use zbus::Connection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedUnit {
    pub name: String,
    pub description: String,
    pub sub_state: String,
}

#[doc(hidden)]
pub struct SystemdHandles {
    pub(crate) failed_units: Mutable<Vec<FailedUnit>>,
}

impl Default for SystemdHandles {
    fn default() -> Self {
        Self {
            failed_units: Mutable::new(Vec::new()),
        }
    }
}

pub struct SystemdService;

impl Service for SystemdService {
    type Handles = SystemdHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = SystemdHandles::default();
        let writer = handles.failed_units.clone();

        rt.spawn(async move {
            loop {
                match listen(&writer).await {
                    Ok(()) => tracing::warn!("systemd listen loop ended, retrying in 5s"),
                    Err(e) => tracing::warn!(error = %e, "systemd listen error, retrying in 5s"),
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        handles
    }
}

#[must_use]
pub fn service() -> SystemdService {
    SystemdService
}

pub fn failed_units() -> impl Signal<Item = Vec<FailedUnit>> {
    registry::with(|r| {
        r.get::<SystemdHandles>()
            .expect("systemd::service() not registered")
            .failed_units
            .signal_cloned()
    })
}

// ── Listen loop ───────────────────────────────────────────────────────────────

/// systemd `ListUnitsFiltered` reply tuple shape:
/// (name, description, load_state, active_state, sub_state, follower,
///  object_path, job_id, job_type, job_object_path).
type UnitTuple = (
    String,
    String,
    String,
    String,
    String,
    String,
    zbus::zvariant::OwnedObjectPath,
    u32,
    String,
    zbus::zvariant::OwnedObjectPath,
);

async fn listen(writer: &Mutable<Vec<FailedUnit>>) -> Result<()> {
    let conn = Connection::system().await.context("connect system bus")?;
    let manager = zbus::Proxy::new(
        &conn,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await
    .context("create systemd Manager proxy")?;

    // REQUIRED: systemd only emits signals to clients that have called
    // Subscribe(). Without this, JobRemoved never fires.
    manager
        .call::<_, _, ()>("Subscribe", &())
        .await
        .context("Manager.Subscribe")?;

    refresh_failed(&manager, writer).await?;

    let mut signals = manager
        .receive_signal("JobRemoved")
        .await
        .context("subscribe JobRemoved")?;

    while signals.next().await.is_some() {
        if let Err(e) = refresh_failed(&manager, writer).await {
            tracing::warn!(error = %e, "systemd refresh after JobRemoved failed");
        }
    }
    Ok(())
}

async fn refresh_failed(
    manager: &zbus::Proxy<'_>,
    writer: &Mutable<Vec<FailedUnit>>,
) -> Result<()> {
    let units: Vec<UnitTuple> = manager
        .call("ListUnitsFiltered", &(vec!["failed".to_string()],))
        .await
        .context("ListUnitsFiltered")?;

    writer.set(parse_units(units));
    Ok(())
}

pub(crate) fn parse_units(units: Vec<UnitTuple>) -> Vec<FailedUnit> {
    let mut out: Vec<FailedUnit> = units
        .into_iter()
        .map(|(name, description, _load, _active, sub_state, ..)| FailedUnit {
            name,
            description,
            sub_state,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(s: &str) -> zbus::zvariant::OwnedObjectPath {
        zbus::zvariant::ObjectPath::try_from(s).unwrap().into()
    }

    fn unit(name: &str, desc: &str, sub: &str) -> UnitTuple {
        (
            name.to_string(),
            desc.to_string(),
            "loaded".to_string(),
            "failed".to_string(),
            sub.to_string(),
            String::new(),
            op("/org/freedesktop/systemd1/unit/dummy"),
            0,
            String::new(),
            op("/"),
        )
    }

    #[test]
    fn parse_units_extracts_name_description_sub_state() {
        let input = vec![unit("polkit.service", "Authorization Manager", "failed")];
        let out = parse_units(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "polkit.service");
        assert_eq!(out[0].description, "Authorization Manager");
        assert_eq!(out[0].sub_state, "failed");
    }

    #[test]
    fn parse_units_sorts_by_name() {
        let input = vec![
            unit("zzz.service", "z", "failed"),
            unit("aaa.service", "a", "failed"),
            unit("mmm.service", "m", "failed"),
        ];
        let out = parse_units(input);
        let names: Vec<&str> = out.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, vec!["aaa.service", "mmm.service", "zzz.service"]);
    }

    #[test]
    fn parse_units_empty_input_yields_empty_output() {
        let out = parse_units(Vec::new());
        assert!(out.is_empty());
    }
}
````

- [ ] **Step 2: Re-export from `crates/hytte-services/src/lib.rs`**

Find the existing `pub mod` declarations. Add `pub mod systemd;` alphabetically (after `screensaver`, before `tray`):

```rust
pub mod systemd;
```

- [ ] **Step 3: Run new tests — verify they pass**

Run: `cargo test -p hytte-services systemd::tests -- --nocapture`
Expected: 3 passed.

- [ ] **Step 4: Register the service in `main.rs`**

Edit `trollshell/src/main.rs`. Find the existing `.with(...)` chain (around lines 20-30). Add `.with(systemd::service())` alphabetically (after `screensaver`, before `tray`). Also add `use hytte::services::systemd;` at the top of the file if `systemd` isn't already imported via the `services::*` re-export.

The chain block becomes (relevant portion):

```rust
    .with(screensaver::service())
    .with(systemd::service())
    .with(tray::service())
```

- [ ] **Step 5: Build + clippy + tests**

Run: `cargo build -p hytte-services -p trollshell && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p hytte-services`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/systemd.rs crates/hytte-services/src/lib.rs trollshell/src/main.rs
git commit -m "$(cat <<'EOF'
feat(systemd): failed-units service via JobRemoved signal

New crates/hytte-services/src/systemd.rs subscribes to
org.freedesktop.systemd1.Manager.JobRemoved (after the required
Manager.Subscribe() call) and re-fetches ListUnitsFiltered(["failed"])
on each emission. Public failed_units() signal is consumed by the
v0.2.5 stats page Services group.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `hytte-ui::Sparkline` widget

**Files:**

- Create: `crates/hytte-ui/src/sparkline.rs`
- Modify: `crates/hytte-ui/src/lib.rs`

**Background:** Self-contained `gtk::DrawingArea`-backed widget with a fixed-capacity ring buffer. Renders via cairo as a single-stroke line + 15%-alpha fill. Theme accent color via `widget.color()` driven by the `.ts-sparkline { color: @accent_color; }` CSS rule landing in Task 9.

- [ ] **Step 1: Create the new module**

Create `crates/hytte-ui/src/sparkline.rs`:

````rust
//! Minimal time-series visualization. Owns a fixed-capacity ring
//! buffer of `f64` samples and renders them as a single-stroke line
//! + 15%-alpha fill via cairo. Color resolves through the widget's
//! GTK4 theme color (`.ts-sparkline { color: @accent_color; }` in
//! the consumer's stylesheet drives this).
//!
//! Used by `trollshell`'s stats page History group; designed to be
//! reusable for any future per-metric history surface.
//!
//! # Example
//!
//! ```ignore
//! let s = Sparkline::new(60);
//! s.set_domain_max(Some(1.0));   // fraction in 0..=1
//! container.append(s.widget());
//!
//! // Each tick:
//! s.push(current_load);
//! ```

use gtk::cairo;
use gtk::glib;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

#[derive(Clone)]
pub struct Sparkline {
    inner: gtk::DrawingArea,
    samples: Rc<RefCell<VecDeque<f64>>>,
    capacity: usize,
    domain_max: Rc<Cell<Option<f64>>>,
}

impl Sparkline {
    /// Build a sparkline that retains the most recent `capacity` samples.
    /// `capacity` MUST be > 0 (panics otherwise — caller error).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Sparkline capacity must be > 0");

        let inner = gtk::DrawingArea::new();
        inner.add_css_class("ts-sparkline");

        let samples: Rc<RefCell<VecDeque<f64>>> =
            Rc::new(RefCell::new(VecDeque::with_capacity(capacity)));
        let domain_max: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));

        let samples_for_draw = samples.clone();
        let domain_max_for_draw = domain_max.clone();
        inner.set_draw_func(move |area, cr, width, height| {
            let samples = samples_for_draw.borrow();
            let dmax = domain_max_for_draw.get();
            draw_sparkline(area, cr, width, height, &samples, dmax);
        });

        Self {
            inner,
            samples,
            capacity,
            domain_max,
        }
    }

    /// The underlying widget. Cheap clone (GTK refcount).
    #[must_use]
    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.inner
    }

    /// Push one sample. Drops the oldest if at capacity. Queues a redraw.
    pub fn push(&self, sample: f64) {
        {
            let mut s = self.samples.borrow_mut();
            if s.len() == self.capacity {
                s.pop_front();
            }
            s.push_back(sample);
        }
        self.inner.queue_draw();
    }

    /// Set a fixed domain max (e.g. `Some(1.0)` for 0..=1 fractions).
    /// `None` enables auto-scaling to the max sample currently in the
    /// ring.
    pub fn set_domain_max(&self, max: Option<f64>) {
        self.domain_max.set(max);
        self.inner.queue_draw();
    }

    /// Drop all samples. Triggers a redraw.
    pub fn clear(&self) {
        self.samples.borrow_mut().clear();
        self.inner.queue_draw();
    }
}

// ── Drawing ───────────────────────────────────────────────────────────────────

fn draw_sparkline(
    area: &gtk::DrawingArea,
    cr: &cairo::Context,
    width: i32,
    height: i32,
    samples: &VecDeque<f64>,
    domain_max: Option<f64>,
) {
    if width <= 0 || height <= 0 || samples.is_empty() {
        return;
    }
    cr.set_antialias(cairo::Antialias::Default);

    #[allow(clippy::cast_precision_loss)]
    let w = width as f64;
    #[allow(clippy::cast_precision_loss)]
    let h = height as f64;

    let denom = match domain_max {
        Some(m) if m > 0.0 => m,
        _ => samples.iter().copied().fold(0.0_f64, f64::max).max(f64::EPSILON),
    };

    #[allow(clippy::cast_precision_loss)]
    let count = samples.len() as f64;
    let step_x = if count <= 1.0 { 0.0 } else { w / (count - 1.0) };

    // Resolve theme color via widget.color() — driven by
    // `.ts-sparkline { color: @accent_color; }` in CSS.
    let color = area.color();
    let r = f64::from(color.red());
    let g = f64::from(color.green());
    let b = f64::from(color.blue());
    let a = f64::from(color.alpha());

    // Build path through samples.
    cr.new_path();
    for (i, sample) in samples.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let x = (i as f64) * step_x;
        let norm = (*sample / denom).clamp(0.0, 1.0);
        let y = h - norm * h;
        if i == 0 {
            cr.move_to(x, y);
        } else {
            cr.line_to(x, y);
        }
    }
    // Stroke the line.
    cr.set_source_rgba(r, g, b, a);
    cr.set_line_width(1.5);
    let _ = cr.stroke_preserve();

    // Close path along the bottom edge for the fill.
    cr.line_to(w, h);
    cr.line_to(0.0, h);
    cr.close_path();
    cr.set_source_rgba(r, g, b, a * 0.15);
    let _ = cr.fill();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_gtk_init() {
        static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| {
            gtk::init().ok();
        });
    }

    #[test]
    #[ignore = "requires a display server"]
    fn push_caps_at_capacity() {
        ensure_gtk_init();
        let s = Sparkline::new(3);
        for i in 0..5 {
            #[allow(clippy::cast_precision_loss)]
            s.push(i as f64);
        }
        let v: Vec<f64> = s.samples.borrow().iter().copied().collect();
        assert_eq!(v.len(), 3);
        assert_eq!(v, vec![2.0, 3.0, 4.0]);
    }

    #[test]
    #[ignore = "requires a display server"]
    fn clear_empties() {
        ensure_gtk_init();
        let s = Sparkline::new(10);
        s.push(1.0);
        s.push(2.0);
        s.clear();
        assert!(s.samples.borrow().is_empty());
    }

    #[test]
    #[ignore = "requires a display server"]
    fn set_domain_max_round_trips() {
        ensure_gtk_init();
        let s = Sparkline::new(5);
        s.set_domain_max(Some(2.0));
        assert_eq!(s.domain_max.get(), Some(2.0));
        s.set_domain_max(None);
        assert_eq!(s.domain_max.get(), None);
    }
}
````

- [ ] **Step 2: Re-export from `lib.rs`**

Edit `crates/hytte-ui/src/lib.rs`. After existing `pub mod` lines (around line 1-9), add:

```rust
pub mod sparkline;
```

In the `pub use` block (around line 10-14), add:

```rust
pub use sparkline::Sparkline;
```

- [ ] **Step 3: Run tests — verify they pass under xvfb (or are correctly ignored without)**

Run: `cargo test -p hytte-ui sparkline::tests -- --nocapture`
Expected: 3 ignored when no display; 3 passed under `xvfb-run` or in a graphical session.

- [ ] **Step 4: Build + clippy**

Run: `cargo build -p hytte-ui && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-ui/src/sparkline.rs crates/hytte-ui/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(ui): Sparkline widget primitive

Self-contained gtk::DrawingArea-backed time-series widget with a
fixed-capacity ring buffer and cairo-rendered single-stroke line +
15%-alpha fill. Resolves color via widget.color() so theme accent
flows through the .ts-sparkline CSS rule. Public API: new(capacity),
widget(), push(f64), set_domain_max(Option<f64>), clear().

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `page_stats` skeleton — three-group vertical stack

**Files:**

- Modify: `trollshell/src/widgets/pages.rs`

**Background:** Drop `page_grid()` + `panel("CPU"|"Memory"|"GPU"|"Disk")`. Replace with three vertical `AdwPreferencesGroup`s (Live / History / Services) inside `finish_page`. Group bodies are stubs in this task — populated in Tasks 6/7/8.

- [ ] **Step 1: Replace `page_stats` body**

Find `pub fn page_stats() -> gtk::Widget` (around line 1396). Replace the entire function body with:

```rust
pub fn page_stats() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    column.append(build_stats_live_group_v2().upcast_ref::<gtk::Widget>());
    column.append(build_stats_history_group().upcast_ref::<gtk::Widget>());
    column.append(build_stats_services_group().upcast_ref::<gtk::Widget>());

    finish_page(&column)
}
```

- [ ] **Step 2: Add stub builders**

Below `page_stats` (before the next `pub fn page_*` builder), add three stub functions that return empty `AdwPreferencesGroup`s. Tasks 6/7/8 fill them in:

```rust
fn build_stats_live_group_v2() -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder().title("Live").build()
}

fn build_stats_history_group() -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder().title("History").build()
}

fn build_stats_services_group() -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder().title("Services").build()
}
```

- [ ] **Step 3: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. The Stats drawer will render three empty group titles — that's expected for this commit.

The legacy `panel("CPU")` / `panel("Memory")` / `panel("GPU")` / `panel("Disk")` blocks are now dead code inside the old `page_stats` body, which has been replaced. Clippy may or may not flag any helpers that became unused. If it does, mark with `#[allow(dead_code)]` and a `// removed in Task 6` comment, OR delete inline if clearly orphaned. The `panel()` helper itself stays — other pages still use it.

- [ ] **Step 4: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
refactor(de): stats — Adwaita-conformant skeleton (three groups)

Drops the page_grid + panel() layout for page_stats in favor of a
vertical stack of three AdwPreferencesGroups (Live / History /
Services). Group bodies are stubs in this commit; subsequent
commits fill them in (Live rows, History sparklines, failed-unit
expander).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Stats Live group — rows + per-core expander

**Files:**

- Modify: `trollshell/src/widgets/pages.rs`

**Background:** Populate `build_stats_live_group_v2` with the rows specified in spec §4 Live group. Per-core mini-bars (existing logic) move into an `AdwExpanderRow`. Memory and Swap rows get progress-bar suffixes. Rows for GPU and Swap render only when the underlying signal indicates presence.

- [ ] **Step 1: Replace `build_stats_live_group_v2` body**

Replace the stub from Task 5 with:

```rust
fn build_stats_live_group_v2() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Live").build();

    group.add(&build_live_cpu_row());
    group.add(&build_live_per_core_expander());
    group.add(&build_live_memory_row());
    group.add(&build_live_swap_row());
    group.add(&build_live_processes_row());
    group.add(&build_live_gpu_row());
    group.add(&build_live_disk_expander());

    group
}
```

- [ ] **Step 2: Add `build_live_cpu_row`**

```rust
fn build_live_cpu_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("CPU").build();
    bind(
        sensors::cpu().map(|c| format!("{:.0}%", c.overall * 100.0)),
        &row,
        |r, t| r.set_subtitle(&t),
    );
    let temp_label = gtk::Label::new(None);
    temp_label.set_valign(gtk::Align::Center);
    bind(
        sensors::cpu_temp().map(|t| match t.package_celsius {
            Some(c) => format!("{c:.0} \u{00b0}C"),
            None => String::new(),
        }),
        &temp_label,
        move |label, txt| {
            label.set_text(&txt);
            label.set_visible(!txt.is_empty());
        },
    );
    row.add_suffix(&temp_label);
    row
}
```

- [ ] **Step 3: Add `build_live_per_core_expander`**

```rust
fn build_live_per_core_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("Per-core").build();
    bind(
        sensors::cpu().map(|c| format!("{} cores", c.per_core.len())),
        &expander,
        |r, t| r.set_subtitle(&t),
    );

    // Single nested row containing the horizontal bars Box, mirroring
    // the legacy build but inside an expander.
    let nested = adw::ActionRow::new();
    nested.set_activatable(false);
    nested.set_selectable(false);
    let cores_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    cores_row.add_css_class("ts-cores-row");
    cores_row.set_margin_top(4);
    cores_row.set_margin_bottom(4);
    cores_row.set_hexpand(true);
    nested.set_child(Some(&cores_row));

    let core_bars: Rc<RefCell<Vec<gtk::ProgressBar>>> = Rc::new(RefCell::new(Vec::new()));
    let cores_row_for_bind = cores_row.clone();
    let bars_for_bind = core_bars.clone();
    bind(sensors::cpu(), &cores_row, move |_, c: CpuLoad| {
        let mut bars = bars_for_bind.borrow_mut();
        if bars.len() != c.per_core.len() {
            while let Some(child) = cores_row_for_bind.first_child() {
                cores_row_for_bind.remove(&child);
            }
            bars.clear();
            for _ in 0..c.per_core.len() {
                let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
                col.set_hexpand(true);
                col.set_halign(gtk::Align::Center);
                let bar = gtk::ProgressBar::new();
                bar.add_css_class("ts-core-bar");
                bar.set_orientation(gtk::Orientation::Vertical);
                bar.set_inverted(true);
                bar.set_valign(gtk::Align::End);
                col.append(&bar);
                cores_row_for_bind.append(&col);
                bars.push(bar);
            }
        }
        for (bar, load) in bars.iter().zip(c.per_core.iter()) {
            bar.set_fraction(load.clamp(0.0, 1.0));
            bar.set_tooltip_text(Some(&format!("{:.0}%", load * 100.0)));
        }
    });

    expander.add_row(&nested);
    expander
}
```

- [ ] **Step 4: Add `build_live_memory_row`**

```rust
fn build_live_memory_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Memory").build();
    bind(
        sensors::memory().map(|m| {
            if m.total == 0 {
                "—".to_string()
            } else {
                #[allow(clippy::cast_precision_loss)]
                let pct = (m.used as f64 / m.total as f64) * 100.0;
                format!("{} / {} ({pct:.0}%)", fmt_bytes(m.used), fmt_bytes(m.total))
            }
        }),
        &row,
        |r, t| r.set_subtitle(&t),
    );

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ts-stat-progress");
    bar.set_valign(gtk::Align::Center);
    bind(
        sensors::memory().map(|m| {
            if m.total == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                let frac = m.used as f64 / m.total as f64;
                frac.clamp(0.0, 1.0)
            }
        }),
        &bar,
        gtk::ProgressBar::set_fraction,
    );
    row.add_suffix(&bar);

    row
}
```

- [ ] **Step 5: Add `build_live_swap_row`**

```rust
fn build_live_swap_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Swap").build();

    // Hide entirely when no swap is configured.
    bind(
        sensors::memory().map(|m| m.swap_total > 0),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );

    bind(
        sensors::memory().map(|m| {
            if m.swap_total == 0 {
                String::new()
            } else {
                #[allow(clippy::cast_precision_loss)]
                let pct = (m.swap_used as f64 / m.swap_total as f64) * 100.0;
                format!(
                    "{} / {} ({pct:.0}%)",
                    fmt_bytes(m.swap_used),
                    fmt_bytes(m.swap_total)
                )
            }
        }),
        &row,
        |r, t| r.set_subtitle(&t),
    );

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ts-stat-progress");
    bar.set_valign(gtk::Align::Center);
    bind(
        sensors::memory().map(|m| {
            if m.swap_total == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                let frac = m.swap_used as f64 / m.swap_total as f64;
                frac.clamp(0.0, 1.0)
            }
        }),
        &bar,
        gtk::ProgressBar::set_fraction,
    );
    row.add_suffix(&bar);

    row
}
```

- [ ] **Step 6: Add `build_live_processes_row`**

```rust
fn build_live_processes_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Processes").build();
    let count_label = gtk::Label::new(None);
    count_label.set_valign(gtk::Align::Center);
    bind(
        sensors::process_count().map(|n| format!("{n}")),
        &count_label,
        gtk::Label::set_text,
    );
    row.add_suffix(&count_label);
    row
}
```

- [ ] **Step 7: Add `build_live_gpu_row`**

```rust
fn build_live_gpu_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("GPU").build();

    // Hide when no GPU detected.
    bind(
        sensors::gpu().map(|g| g.is_some()),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );

    bind(
        sensors::gpu().map(|g| match g {
            Some(state) => state.name.clone(),
            None => String::new(),
        }),
        &row,
        |r, t| r.set_subtitle(&t),
    );

    let suffix = gtk::Label::new(None);
    suffix.set_valign(gtk::Align::Center);
    bind(
        sensors::gpu().map(|g| match g {
            Some(state) => match state.temperature_celsius {
                Some(t) => format!("{t:.0} \u{00b0}C"),
                None => match state.load {
                    Some(l) => format!("{:.0}%", l * 100.0),
                    None => String::new(),
                },
            },
            None => String::new(),
        }),
        &suffix,
        |label, txt| {
            label.set_text(&txt);
            label.set_visible(!txt.is_empty());
        },
    );
    row.add_suffix(&suffix);

    row
}
```

- [ ] **Step 8: Add `build_live_disk_expander`**

```rust
fn build_live_disk_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("Disk").build();
    bind(
        sensors::disk().map(|d| format!("{} mount(s)", d.mounts.len())),
        &expander,
        |r, t| r.set_subtitle(&t),
    );

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(sensors::disk(), &expander, move |_, d| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        let mut new_rows = Vec::with_capacity(d.mounts.len());
        for m in &d.mounts {
            let row = adw::ActionRow::builder()
                .title(&m.path)
                .activatable(false)
                .build();
            #[allow(clippy::cast_precision_loss)]
            let pct = if m.total > 0 {
                (m.used as f64 / m.total as f64) * 100.0
            } else {
                0.0
            };
            let label = gtk::Label::new(Some(&format!(
                "{} / {} ({pct:.0}%)",
                fmt_bytes(m.used),
                fmt_bytes(m.total),
            )));
            label.set_valign(gtk::Align::Center);
            row.add_suffix(&label);
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
}
```

- [ ] **Step 9: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 10: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(de): stats Live group — Adwaita rows + per-core expander

Populates build_stats_live_group_v2 with seven rows: CPU
(overall % subtitle, temp suffix), Per-core expander wrapping
the migrated mini-bars, Memory + Swap (progress-bar suffix),
Processes (count suffix), GPU (name subtitle, temp/load suffix;
hidden when no GPU), Disk expander (one row per mount). Swap
row hides itself when swap_total == 0.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Stats History group — sparkline rows

**Files:**

- Modify: `trollshell/src/widgets/pages.rs`

**Background:** Four custom `gtk::Box` rows: `[name | Sparkline | value]`. CPU% / Memory% / Network throughput / GPU temp (conditional). Each pushes samples on every emission of its source signal; ring buffer resets on page rebuild (drawer reopens trigger a fresh `page_stats()` call).

- [ ] **Step 1: Add the `Sparkline` import**

In `trollshell/src/widgets/pages.rs`, near the existing `use hytte::ui::{...}` block (or wherever the `hytte::ui::` imports live), add:

```rust
use hytte::ui::Sparkline;
```

- [ ] **Step 2: Replace `build_stats_history_group` body**

Replace the stub with:

```rust
fn build_stats_history_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("History").build();

    group.add(&build_history_cpu_row());
    group.add(&build_history_memory_row());
    group.add(&build_history_network_row());
    group.add(&build_history_gpu_temp_row());

    group
}
```

- [ ] **Step 3: Add `build_history_row` shared helper**

```rust
/// Build a `[name | Sparkline | value]` row styled `.ts-history-row`.
/// Returns the box, the Sparkline (caller pushes samples), and the
/// value label (caller binds text on it).
fn build_history_row(name: &str) -> (gtk::Box, Sparkline, gtk::Label) {
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

- [ ] **Step 4: Add `build_history_cpu_row`**

```rust
fn build_history_cpu_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("CPU");
    spark.set_domain_max(Some(1.0));

    let spark_clone = spark.clone();
    let value_clone = value.clone();
    bind(sensors::cpu(), &row, move |_, c: CpuLoad| {
        spark_clone.push(c.overall);
        value_clone.set_text(&format!("{:.0}%", c.overall * 100.0));
    });

    row
}
```

- [ ] **Step 5: Add `build_history_memory_row`**

```rust
fn build_history_memory_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("Memory");
    spark.set_domain_max(Some(1.0));

    let spark_clone = spark.clone();
    let value_clone = value.clone();
    bind(sensors::memory(), &row, move |_, m| {
        if m.total == 0 {
            spark_clone.push(0.0);
            value_clone.set_text("—");
        } else {
            #[allow(clippy::cast_precision_loss)]
            let frac = (m.used as f64 / m.total as f64).clamp(0.0, 1.0);
            spark_clone.push(frac);
            value_clone.set_text(&format!("{:.0}%", frac * 100.0));
        }
    });

    row
}
```

- [ ] **Step 6: Add `build_history_network_row`**

```rust
fn build_history_network_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("Network");
    spark.set_domain_max(None); // auto-scale

    let spark_clone = spark.clone();
    let value_clone = value.clone();
    bind(sensors::network(), &row, move |_, net| {
        let (rx_total, tx_total) = net
            .interfaces
            .iter()
            .filter(|i| i.name != "lo")
            .fold((0.0_f64, 0.0_f64), |(rx, tx), i| {
                (rx + i.rx_rate_bps, tx + i.tx_rate_bps)
            });
        let combined = rx_total + tx_total;
        spark_clone.push(combined);
        value_clone.set_text(&format!(
            "\u{2193} {} \u{2191} {}",
            fmt_rate(rx_total),
            fmt_rate(tx_total)
        ));
    });

    row
}
```

- [ ] **Step 7: Add `build_history_gpu_temp_row`**

```rust
fn build_history_gpu_temp_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("GPU temp");
    spark.set_domain_max(None);

    // Hide unless GPU is present with a temperature reading.
    bind(
        sensors::gpu().map(|g| g.and_then(|s| s.temperature_celsius).is_some()),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );

    let spark_clone = spark.clone();
    let value_clone = value.clone();
    bind(sensors::gpu(), &row, move |_, g| {
        if let Some(state) = g {
            if let Some(t) = state.temperature_celsius {
                spark_clone.push(t);
                value_clone.set_text(&format!("{t:.0} \u{00b0}C"));
            }
        }
    });

    row
}
```

- [ ] **Step 8: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(de): stats History group — sparkline rows

Four custom rows ([name | Sparkline | value]) for CPU usage, memory
%, network throughput (sum of non-lo rx+tx), and GPU temperature.
GPU temp row hides when no GPU temp is available. Sparkline ring
buffer resets per page-rebuild (drawer reopen).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Stats Services group — failed-unit expander

**Files:**

- Modify: `trollshell/src/widgets/pages.rs`

**Background:** Single `AdwExpanderRow` listing `systemd::failed_units()`. Empty state: a single non-activatable "All units running" placeholder. Non-empty: one ActionRow per unit with `.ts-pill-error` suffix.

- [ ] **Step 1: Add the `systemd` service import**

Find the `use hytte::services::{...}` block in `pages.rs`. Add `systemd` to the imported services:

```rust
use hytte::services::systemd;
```

- [ ] **Step 2: Replace `build_stats_services_group` body**

```rust
fn build_stats_services_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Services").build();

    bind(
        systemd::failed_units().map(|units| {
            if units.is_empty() {
                "All services running".to_string()
            } else {
                format!("{} failed unit(s)", units.len())
            }
        }),
        &group,
        |g, txt| g.set_description(Some(&txt)),
    );

    group.add(&build_failed_units_expander());
    group
}

fn build_failed_units_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("Failed units").build();
    bind(
        systemd::failed_units().map(|u| {
            if u.is_empty() {
                "None".to_string()
            } else {
                format!("{} unit(s)", u.len())
            }
        }),
        &expander,
        |r, t| r.set_subtitle(&t),
    );

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let placeholder_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    let placeholder_for_bind = placeholder_track.clone();
    bind(systemd::failed_units(), &expander, move |_, units| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        if let Some(p) = placeholder_for_bind.borrow_mut().take() {
            expander_for_bind.remove(&p);
        }
        if units.is_empty() {
            let placeholder = adw::ActionRow::builder()
                .title("All units running")
                .activatable(false)
                .selectable(false)
                .build();
            expander_for_bind.add_row(&placeholder);
            *placeholder_for_bind.borrow_mut() = Some(placeholder);
            return;
        }
        let mut new_rows = Vec::with_capacity(units.len());
        for unit in &units {
            let row = adw::ActionRow::builder()
                .title(&unit.name)
                .activatable(false)
                .build();
            let subtitle = if unit.description.is_empty() {
                unit.sub_state.clone()
            } else {
                unit.description.clone()
            };
            row.set_subtitle(&subtitle);

            let pill = gtk::Label::new(Some("failed"));
            pill.set_valign(gtk::Align::Center);
            pill.add_css_class("ts-pill-error");
            row.add_suffix(&pill);

            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
}
```

- [ ] **Step 3: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(de): stats Services group — failed-unit expander

Group description binds to a live count ("All services running"
or "{N} failed unit(s)"). Single AdwExpanderRow lists the failed
units with .ts-pill-error suffix. Empty state shows a single
non-activatable "All units running" placeholder row.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Stats CSS — replace per-core gradient + new rules

**Files:**

- Modify: `trollshell/style.css`

**Background:** Drop the hex-gradient background (`linear-gradient(180deg, #ff006e, #8338ec 60%, #3a86ff)`) from the per-core bar. Replace with `@accent_color`. Append new rules for `.ts-stat-progress`, `.ts-history-row`, `.ts-stat-name`, `.ts-stat-value`, `.ts-sparkline`, `.ts-pill-error`.

- [ ] **Step 1: Verify token availability**

Run: `grep -nE '@accent_color|@error_color|@destructive_color' trollshell/style.css | head -10`

`@accent_color` is already used (confirmed). For `.ts-pill-error`'s color, check whether `@error_color` is referenced. If not, the libadwaita-version-in-use likely still exposes it via the named-color system — try `@error_color` first; if at runtime the rule doesn't theme correctly (a manual smoke test would surface it as black or transparent), fall back to `@destructive_color` in a follow-up commit.

- [ ] **Step 2: Replace per-core trough rules + append new stats rules**

Find the existing `.ts-core-bar > trough` and `.ts-core-bar > trough > progress` rules (around lines 199-212 of `trollshell/style.css`). Replace them with:

```css
.ts-core-bar > trough {
  min-width: 8px;
  min-height: 56px;
  border-radius: 3px;
  background: alpha(@accent_color, 0.1);
  border: none;
  padding: 0;
}

.ts-core-bar > trough > progress {
  min-width: 8px;
  border-radius: 3px;
  background: @accent_color;
  border: none;
}
```

(The hex gradient `#ff006e → #8338ec → #3a86ff` is gone.)

Then append at the bottom of the file:

```css
/* ── Stats: live progress (memory + swap suffix bars) ──────────────────── */

.ts-stat-progress {
  min-height: 6px;
  min-width: 100px;
}

.ts-stat-progress > trough {
  min-height: 6px;
  background: alpha(@accent_color, 0.15);
  border-radius: 9999px;
}

.ts-stat-progress > trough > progress {
  background: @accent_color;
  border-radius: 9999px;
}

/* ── Stats: history sparkline rows ─────────────────────────────────────── */

.ts-history-row {
  padding: 8px 12px;
}

.ts-stat-name {
  font-weight: 600;
}

.ts-stat-value {
  opacity: 0.7;
  font-variant-numeric: tabular-nums;
}

.ts-sparkline {
  color: @accent_color;
  min-height: 24px;
}

/* ── Stats: failed-unit pill ───────────────────────────────────────────── */

.ts-pill-error {
  padding: 2px 10px;
  border-radius: 9999px;
  font-size: 0.8em;
  font-weight: 600;
  background: alpha(@error_color, 0.2);
  color: @error_color;
}
```

- [ ] **Step 3: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean (CSS isn't compiled; this just confirms nothing else broke).

- [ ] **Step 4: Manual smoke test (deferred to user)**

In a Niri session: `cargo run --release -p trollshell`. Open the Stats drawer.

- Three group headers visible: Live, History, Services.
- Per-core mini-bars in the Per-core expander render with accent color (NOT pink-purple-blue). Verify in both light and dark mode if the system supports a toggle.
- Memory + Swap rows show progress-bar suffix in accent color.
- History group's four sparkline rows populate within ~10 seconds. Trigger CPU load (`stress -c 4 -t 30`) and watch the CPU% line climb.
- Services group description reads "All services running"; trigger a synthetic failure (`systemctl --user start nonexistent-test.service` then `systemctl --user reset-failed`) and watch the count update.
- `.ts-pill-error` renders red-ish on failed-unit rows. If it appears black/transparent, the `@error_color` token isn't defined — file a follow-up to swap to `@destructive_color`.

- [ ] **Step 5: Commit**

```bash
git add trollshell/style.css
git commit -m "$(cat <<'EOF'
style: stats — Adwaita-themed bars + sparklines + failed-unit pill

Replaces the hardcoded hex gradient (#ff006e → #8338ec → #3a86ff)
on per-core CPU bars with @accent_color. Adds .ts-stat-progress
(memory/swap suffix bars), .ts-history-row (sparkline-row layout),
.ts-stat-name / .ts-stat-value (tabular-nums for stable widths),
.ts-sparkline { color: @accent_color } (drives the cairo stroke),
and .ts-pill-error using @error_color. All rules use existing
Adwaita tokens; no new color tokens introduced.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review notes

**Spec coverage:**

- Spec §1 Sparkline → Task 4.
- Spec §2 sensors swap → Task 1.
- Spec §2 sensors process_count → Task 2.
- Spec §3 systemd service → Task 3.
- Spec §4 page_stats Live group → Tasks 5 + 6.
- Spec §4 page_stats History group → Tasks 5 + 7.
- Spec §4 page_stats Services group → Tasks 5 + 8.
- Spec §5 stylesheet → Task 9.

**Final verification:**

- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green (sparkline tests `#[ignore]`'d for headless; swap parser + systemd parse_units tests run unconditionally).
- Manual smoke test (deferred) covers the success criteria.
