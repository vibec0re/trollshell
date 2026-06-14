# Stats page redesign — v0.2.5

**Status:** design
**Date:** 2026-04-26
**Author:** Claude (with annika)
**Predecessors:** `2026-04-25-network-panel-redesign-design.md`, `2026-04-26-osd-redesign-design.md`.

## Goal

Replace the current bespoke `panel("CPU") / panel("Memory") / panel("GPU")` grid layout in `page_stats` with a vertical stack of Adwaita `AdwPreferencesGroup`s, fix the per-core CPU bars' broken hardcoded hex gradient (now uses `@accent_color`), add a History group with `Sparkline` widget rows showing the last 60 seconds of CPU / memory / network / GPU temp, and add a Services group listing failed systemd units via signal-driven dbus subscription.

## Scope

### In scope

**New widget primitive (`crates/hytte-ui/src/sparkline.rs`):**

- `Sparkline` struct backed by a `gtk::DrawingArea` with a fixed-capacity ring buffer of `f64` samples. Renders a single-stroke line + 15%-alpha fill via cairo. Theme accent color via `widget.color()`. Public API: `new(capacity)`, `widget()`, `push(f64)`, `set_domain_max(Option<f64>)`, `clear()`.
- Re-exported from `hytte_ui::Sparkline` and `hytte::ui::Sparkline`.

**Service extensions (`crates/hytte-services/src/sensors.rs`):**

- `Memory` struct gains `swap_used: u64`, `swap_total: u64` (parsed from existing `/proc/meminfo` read).
- New `pub fn process_count() -> impl Signal<Item = u32>` (count of `/proc/<num>/` entries; piggybacks on existing 1Hz polling tick).

**New service (`crates/hytte-services/src/systemd.rs`):**

- Subscribe to `org.freedesktop.systemd1.Manager.JobRemoved` signal. Seed initial state via `ListUnitsFiltered(["failed"])`. Re-fetch on each `JobRemoved`. Required: call `Manager.Subscribe()` on the connection (systemd doesn't emit signals to non-subscribed clients).
- Public `FailedUnit { name, description, sub_state }` + `failed_units() -> impl Signal<Item = Vec<FailedUnit>>`.
- Reconnect-on-error via the existing service-loop pattern (mirror bluetooth/networkd listen loops).
- Registered in `main.rs::App::with(systemd::service())`.

**UI restructure (`trollshell/src/widgets/pages.rs::page_stats`):**

- Drop `page_grid` + `panel("…")` shape.
- Three vertically-stacked `AdwPreferencesGroup`s inside `finish_page` Clamp:
  - **Live** group: CPU row (overall % + temp suffix), Per-core expander (with the migrated bars), Memory row (with progress suffix), Swap row (when present), Processes row, GPU row (when present), Disk expander (one row per mount).
  - **History** group: four custom rows (`gtk::Box` with `[name | sparkline | value]`) — CPU%, Memory%, Network throughput summed, GPU temp (when GPU present).
  - **Services** group: live description ("All services running" / "{N} failed unit(s)"), single AdwExpanderRow listing failed units (or single non-activatable "All units running" placeholder when empty). Failed units carry an `.ts-pill-error` suffix using `@error_color`.

**CSS additions (`trollshell/style.css`):**

- Per-core bar rules switched from hex-gradient to `@accent_color` (drops `linear-gradient(180deg, #ff006e, #8338ec 60%, #3a86ff)`).
- `.ts-stat-progress` (memory/swap suffix bars), `.ts-history-row`, `.ts-stat-name`, `.ts-stat-value` (with `tabular-nums`), `.ts-sparkline { color: @accent_color }`, `.ts-pill-error` using `@error_color` (or fallback `@destructive_color` if absent).

### Out of scope

- Per-disk I/O rates (would need `/proc/diskstats` parser).
- Fan speeds (hwmon).
- SMART / disk health.
- GPU memory % time-series (only raw bytes; would need a denominator — vendor-specific).
- Per-iface network sparklines (one summed line for v0.2.5).
- Configurable history length / poll rate / ring-buffer capacity.
- Click-to-zoom on sparklines / time-axis labels / multiple lines per chart.
- Persistent ring buffer across page-rebuild cycles (rings reset on each Stats drawer open).
- Live `Manager.UnitNew` / `UnitRemoved` per-unit subscriptions for systemd (the JobRemoved signal is the chosen primary trigger; per-unit PropertiesChanged isn't needed at v0.2.5 fidelity).

### Success criteria

- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green.
- New unit tests:
  - Sparkline ring buffer caps at capacity, clear empties.
  - sensors swap parse from synthetic /proc/meminfo input.
  - systemd parser extracts (name, description, sub_state) from a synthetic ListUnitsFiltered tuple reply, sorts by name.
- Manual:
  - Open Stats drawer → three native `AdwPreferencesGroup`s render. Per-core bars show accent color in both light and dark mode (no pink/purple/blue gradient).
  - Sparklines populate within ~10 seconds of opening; show ~60 seconds of history; CPU% sparkline tracks load (try `stress -c 4` briefly).
  - Memory row shows used/total + percent suffix bar; swap row only appears when swap is configured.
  - Process count visible in Live group.
  - Services group shows "All services running" by default. Trigger a synthetic failure (e.g. `systemctl --user start nonexistent.service`); the failed unit appears in the expander within seconds (signal-driven).

## §1 — Sparkline widget (`hytte-ui::sparkline`)

### Public API

```rust
#[derive(Clone)]
pub struct Sparkline {
    inner: gtk::DrawingArea,
    samples: Rc<RefCell<VecDeque<f64>>>,
    capacity: usize,
    domain_max: Cell<Option<f64>>,
}

impl Sparkline {
    pub fn new(capacity: usize) -> Self;
    pub fn widget(&self) -> &gtk::DrawingArea;
    pub fn push(&self, sample: f64);
    pub fn set_domain_max(&self, max: Option<f64>);
    pub fn clear(&self);
}
```

### Drawing implementation

`set_draw_func` closure captures the `samples` Rc and `domain_max` Cell. Per redraw:

1. Acquire `(width, height)` from drawing-area allocation. If `width <= 0 || height <= 0`, return.
2. Read the widget's color (`widget.color()`); fall back to a soft accent-alike when the color resolves to default. The CSS rule `.ts-sparkline { color: @accent_color; }` drives this.
3. If the ring is empty, return — no axis, no placeholder.
4. Compute `step_x = width / max(1, samples.len() - 1)` (one less so first/last sample sit at the left/right edges; if only one sample, render a single dot or shortest possible line).
5. Compute `denom`:
   - If `domain_max` is `Some(m)` and `m > 0.0`: use `m`.
   - Else: `samples.iter().copied().fold(0.0_f64, f64::max).max(epsilon)`.
6. Build a cairo path: for each sample `i`, `x = i as f64 * step_x`, `y = height - (sample_i / denom).clamp(0.0, 1.0) * height`. The line goes from leftmost to rightmost; most recent sample sits on the right edge.
7. Stroke at line-width 1.5 with the resolved color.
8. Fill the same path closed via `(width, height) → (0, height) → close`, using the resolved color at 0.15 alpha.

Anti-aliasing: default cairo behavior; explicit `set_antialias(Antialias::Default)`.

### Sample push

`push()` writes to the ring (drops oldest if at capacity), then `inner.queue_draw()`. Safe to call from any handler running on the GTK main context.

### Tests

- `push_caps_at_capacity` — push N+5 samples to a capacity-N sparkline; deque length == N; oldest sample dropped.
- `clear_empties` — push 3 samples, call `clear()`, deque empty.
- `set_domain_max_changes_normalization_target` — push samples up to 1.0 with `set_domain_max(Some(2.0))`; helper extracted from the draw-fn (e.g. `fn normalize(sample: f64, domain_max: Option<f64>, samples: &VecDeque<f64>) -> f64`) verifies normalization halves vs auto-scale.

The cairo drawing itself isn't unit-testable (needs a surface). Manual verification via the Stats page.

### Cargo

`cairo-rs` is already a transitive dep through gtk4-rs; no new deps. If, during implementation, the cairo API isn't reachable from `hytte-ui`'s current `Cargo.toml`, add `cairo-rs = "0.21"` (matching gtk4-rs's compatible version).

## §2 — Service extensions (`sensors.rs`)

### `Memory` swap fields

Extend the existing struct:

```rust
#[derive(Clone, Debug, Default)]
pub struct Memory {
    pub total: u64,
    pub available: u64,
    pub used: u64,
    pub swap_used: u64,
    pub swap_total: u64,
}
```

Parser: extend the existing `/proc/meminfo` reader (find by grepping for `MemTotal` or similar). On each parsed line, also capture:

- `SwapTotal:` → `swap_total = kB * 1024`
- `SwapFree:` → derived `swap_free` local; compute `swap_used = swap_total.saturating_sub(swap_free)`

Backward-compat: existing consumers reading `total`/`available`/`used` are unaffected. `Default` initializes the new fields to 0.

Test: a synthetic `/proc/meminfo` snippet (committed as a string fixture) is fed through the parser; assert the swap fields. Reuse whatever existing test-helper shape the file has (or add a small `#[cfg(test)] mod tests` if none exists).

### `process_count` signal

```rust
pub(crate) process_count: Mutable<u32>,
```

added to `SensorsHandles`, initialized to 0 in `Default`.

In the existing 1Hz polling tick (the listen loop that drives `cpu()`/`memory()`/etc.), read once:

```rust
fn read_process_count() -> u32 {
    std::fs::read_dir("/proc")
        .map(|iter| {
            iter.filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.parse::<u32>().is_ok())
                })
                .count() as u32
        })
        .unwrap_or(0)
}
```

Public:

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

## §3 — `systemd` service (new)

### Public API

```rust
pub struct SystemdService;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedUnit {
    pub name: String,
    pub description: String,
    pub sub_state: String,
}

impl Service for SystemdService { /* ... */ }

#[must_use]
pub fn service() -> SystemdService { SystemdService }

pub fn failed_units() -> impl Signal<Item = Vec<FailedUnit>>;
```

### Implementation

Mirror the bluetooth/networkd listen-loop pattern: outer `start()` retries forever on error; inner `listen()` opens the connection, registers the subscription, and stays in the receive loop.

```rust
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

    // Required: systemd only emits signals to clients that have called
    // Subscribe(). Without this, JobRemoved never fires.
    manager.call::<_, _, ()>("Subscribe", &()).await
        .context("Manager.Subscribe")?;

    // Seed initial state.
    refresh_failed(&manager, writer).await?;

    // Subscribe to JobRemoved — fires whenever a unit job (start/stop/
    // restart) finishes. Catches every transition that could affect the
    // failed-unit set.
    let mut signals = manager.receive_signal("JobRemoved").await
        .context("subscribe JobRemoved")?;
    while signals.next().await.is_some() {
        if let Err(e) = refresh_failed(&manager, writer).await {
            tracing::warn!(error = %e, "systemd refresh_failed after JobRemoved failed");
        }
    }
    Ok(())
}

async fn refresh_failed(
    manager: &zbus::Proxy<'_>,
    writer: &Mutable<Vec<FailedUnit>>,
) -> Result<()> {
    type UnitTuple = (
        String, // 0: name
        String, // 1: description
        String, // 2: load_state
        String, // 3: active_state
        String, // 4: sub_state
        String, // 5: follower
        zbus::zvariant::OwnedObjectPath, // 6: object_path
        u32,    // 7: job_id
        String, // 8: job_type
        zbus::zvariant::OwnedObjectPath, // 9: job_object_path
    );
    let units: Vec<UnitTuple> = manager
        .call("ListUnitsFiltered", &(vec!["failed".to_string()],))
        .await
        .context("ListUnitsFiltered failed")?;

    let mut out: Vec<FailedUnit> = units
        .into_iter()
        .map(|(name, description, _load, _active, sub_state, ..)| FailedUnit {
            name,
            description,
            sub_state,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    writer.set(out);
    Ok(())
}
```

The `Service::start` impl spawns the outer retry loop (5-second backoff on error, mirroring polkit/bluetooth).

### Tests

- Parser test: feed a synthetic `Vec<UnitTuple>`-shaped input through a small helper (extract the `.into_iter().map(...).collect()` + `.sort_by` logic into `pub(crate) fn parse_units(units: Vec<UnitTuple>) -> Vec<FailedUnit>`); assert sort + extract.
- Real zbus round-trip: not unit-testable (needs systemd running).

### Registration

`trollshell/src/main.rs` adds `.with(systemd::service())` alongside the existing service registrations.

## §4 — Stats page wiring

`page_stats()` returns a vertical Box (16px spacing) inside `finish_page` containing three `AdwPreferencesGroup`s.

### Live group

Title: `"Live"`.

| Row       | Type                                                | Title         | Subtitle (live)                                                    | Suffix (live)                                                                  |
| --------- | --------------------------------------------------- | ------------- | ------------------------------------------------------------------ | ------------------------------------------------------------------------------ |
| CPU       | `AdwActionRow`                                      | `"CPU"`       | `format!("{:.0}%", c.overall * 100.0)`                             | `gtk::Label` `"{temp} °C"` from `cpu_temp().package_celsius`; hidden when None |
| Per-core  | `AdwExpanderRow`                                    | `"Per-core"`  | `format!("{} cores", c.per_core.len())`                            | —                                                                              |
| (nested)  | `gtk::Box` row                                      | —             | —                                                                  | per-core mini-bars (existing build_core_bars logic, accent color)              |
| Memory    | `AdwActionRow`                                      | `"Memory"`    | `format!("{} / {} ({}%)", fmt_bytes(used), fmt_bytes(total), pct)` | `.ts-stat-progress` `gtk::ProgressBar` at `used/total`                         |
| Swap      | `AdwActionRow` (visible only when `swap_total > 0`) | `"Swap"`      | `format!("{} / {} ({}%)", swap_used, swap_total, pct)`             | `.ts-stat-progress` ProgressBar                                                |
| Processes | `AdwActionRow`                                      | `"Processes"` | —                                                                  | `gtk::Label` count                                                             |
| GPU       | `AdwActionRow` (only when `gpu()` is `Some`)        | `"GPU"`       | `gpu.name`                                                         | `gtk::Label` `"{temp} °C"` (or `"{load}%"` if no temp)                         |
| Disk      | `AdwExpanderRow`                                    | `"Disk"`      | `format!("{} mount(s)", count)`                                    | —                                                                              |
| (nested)  | `AdwActionRow` per mount                            | mount path    | —                                                                  | `format!("{}/{} ({}%)", used, total, pct)`                                     |

### History group

Title: `"History"`.

Each row is a custom `gtk::Box` styled `.ts-history-row` containing three children (left-to-right):

1. `gtk::Label` styled `.ts-stat-name`, `set_size_request(80, -1)`, `set_xalign(0.0)` — kind name.
2. `Sparkline::widget()` (the inner `gtk::DrawingArea`), `set_hexpand(true)`, `min-height: 24` via CSS.
3. `gtk::Label` styled `.ts-stat-value`, `set_size_request(80, -1)`, `set_xalign(1.0)` — current value.

The custom row gets added to the group via `group.add(&row)` (which accepts any `IsA<gtk::Widget>`).

Sparkline subscriptions:

- **CPU usage** — capacity 60, `set_domain_max(Some(1.0))`. Subscription: `sensors::cpu()` → push `c.overall`. Value label: `format!("{:.0}%", c.overall * 100.0)`.
- **Memory** — capacity 60, `set_domain_max(Some(1.0))`. Subscription: `sensors::memory()` → push `m.used as f64 / m.total as f64`. Value label: `format!("{:.0}%", pct)`.
- **Network** — capacity 60, `set_domain_max(None)`. Subscription: `sensors::network()` → push `interfaces.iter().filter(|i| i.name != "lo").map(|i| i.rx_rate_bps + i.tx_rate_bps).sum()`. Value label: `format!("↓ {} ↑ {}", fmt_rate(rx), fmt_rate(tx))` (sums per-iface separately for the label).
- **GPU temperature** — capacity 60, `set_domain_max(None)`. Only added when `sensors::gpu()` resolves to `Some(g)` AND `g.temperature_celsius.is_some()` at page-build time. (Subsequent absence-of-temp is simply pushing the last-known value, or skipping pushes — pick: skip pushes when None to keep the line stable.) Value label: `format!("{:.0} °C", temp)`.

### Services group

Title: `"Services"`. Description bound to `failed_units()`:

- `0` → `"All services running"`
- `n > 0` → `format!("{n} failed unit(s)")`

Single child:

`AdwExpanderRow`:

- Title: `"Failed units"`.
- Subtitle: bound to `failed_units().len()` count or `"None"` when empty.
- Drain+rebuild on emission:
  - When empty: single non-activatable `AdwActionRow` with title `"All units running"`, no subtitle.
  - When non-empty: one `AdwActionRow` per unit; title=`unit.name`, subtitle=`unit.description.clone()` (or `unit.sub_state` when description is empty), suffix=`gtk::Label` styled `.ts-pill-error` with text `"failed"`.

### Lifecycle

The four sparkline subscriptions are wired during `page_stats()` execution. Drawer pages rebuild on each open per existing modal.rs flow → ring buffers reset on each open.

## §5 — Stylesheet

`trollshell/style.css` modifications (existing rule replaced; new rules appended):

```css
/* Replace existing .ts-core-bar > trough rules (~line 199-212) */

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

/* Append: stats live progress (memory/swap suffix bars) */

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

/* Append: stats history rows */

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

/* Append: failed-unit pill */

.ts-pill-error {
  padding: 2px 10px;
  border-radius: 9999px;
  font-size: 0.8em;
  font-weight: 600;
  background: alpha(@error_color, 0.2);
  color: @error_color;
}
```

If `@error_color` isn't available in the libadwaita-version-in-use, fall back to `@destructive_color`. If neither is defined, hard-code `#e01b24` ONLY as last resort and flag in the commit message for a follow-up token-discovery pass. Implementer verifies via grep at task time.

## §6 — Implementation hand-off

After approval, the writing-plans skill produces a step-by-step plan. Suggested decomposition:

1. **Service:** sensors swap fields (`Memory.swap_used` / `Memory.swap_total` + parser) + 1 unit test.
2. **Service:** sensors `process_count` signal + reader + piggyback on existing 1Hz tick.
3. **Service:** new `systemd` service module (struct, listen loop with `Subscribe()`, `JobRemoved` signal handler, `refresh_failed` parser) + parser unit test + `main.rs` registration.
4. **Library:** new `hytte-ui::sparkline::Sparkline` widget (struct, drawing fn, ring-buffer push, public API) + 3 unit tests.
5. **UI:** `page_stats` skeleton — drop `page_grid`/`panel("…")`; vertical stack of three `AdwPreferencesGroup`s with stub bodies.
6. **UI:** Live group rows (CPU, Per-core expander with migrated bars, Memory, Swap, Processes, GPU, Disk).
7. **UI:** History group (4 sparkline rows wired to sensors signals).
8. **UI:** Services group (failed-unit expander wired to `systemd::failed_units()`).
9. **CSS:** Replace per-core gradient + append stats rules + pill-error.
