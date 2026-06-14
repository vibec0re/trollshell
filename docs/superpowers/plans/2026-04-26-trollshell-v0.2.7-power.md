# trollshell v0.2.7 power profiles + battery OSD Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface `power-profiles-daemon`'s active profile + selection in the Power drawer's Battery group with an active-profile icon indicator, and add a fourth `Kind::Battery` to the OSD that fires on plug/unplug edges plus 20% / 10% / 5% threshold crossings.

**Architecture:** New `crates/hytte-services/src/power_profiles.rs` service subscribes to `net.hadess.PowerProfiles` (canonical) on the system bus with fallback to `org.freedesktop.UPower.PowerProfiles` (freedesktop alias). Public `state()` signal + `set_active(profile)` command. UI lives in `page_power`'s existing Battery `AdwPreferencesGroup` as an `AdwExpanderRow` with dynamic active-profile prefix icon. Battery OSD plugs into the existing OSD subscriptions array (`install_subscriptions`); `detect_battery_event` is a pure helper unit-tested for plug/unplug + 3 threshold crossings.

**Tech Stack:** Rust 1.94 stable, GTK4 + libadwaita, `futures-signals`, `zbus`, `tokio`. No new top-level deps.

**Conventions:**

- TDD where unit-testable (`humanize_profile`, `detect_battery_event`).
- Commits use existing prefixes: `feat(power-profiles):`, `feat(de):`, `feat(osd):`, `style:`.
- Co-author trailer on every commit:
  `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`

**Spec backing this plan:** `docs/superpowers/specs/2026-04-26-v0.2.7-power-design.md`

---

## File Structure

**Created:**

- `crates/hytte-services/src/power_profiles.rs` — new service module.

**Modified:**

- `crates/hytte-services/src/lib.rs` — re-export `power_profiles` module.
- `trollshell/src/main.rs` — register `power_profiles::service()` in the `App::with(...)` chain.
- `trollshell/src/widgets/pages.rs` — `page_power` Battery group gains `build_power_profile_expander()`; `humanize_profile` + `profile_icon_name` helpers.
- `trollshell/src/widgets/osd.rs` — `Kind::Battery` variant; `BatteryEvent` enum; `detect_battery_event` + `render_battery` helpers; battery subscription wired in `install_subscriptions`.
- `trollshell/style.css` — append `.ts-osd-card.battery .ts-osd-icon` rule.

---

## Task 1: `power_profiles` service module

**Files:**

- Create: `crates/hytte-services/src/power_profiles.rs`
- Modify: `crates/hytte-services/src/lib.rs`
- Modify: `trollshell/src/main.rs`

**Background:** New service subscribes to `net.hadess.PowerProfiles` on the system bus with fallback to the freedesktop alias. Watches `Properties.PropertiesChanged` for live `ActiveProfile` updates. Lists `Profiles` (a{sv} dict array) into a flat `Vec<String>` of profile names.

- [ ] **Step 1: Create `crates/hytte-services/src/power_profiles.rs`**

```rust
//! Power profiles via `power-profiles-daemon`.
//!
//! Subscribes to `net.hadess.PowerProfiles` (canonical) on the system
//! bus with fallback to `org.freedesktop.UPower.PowerProfiles` (the
//! freedesktop alias newer builds also expose). Emits a flat
//! [`PowerProfilesState`] every time the daemon's `ActiveProfile` or
//! `Profiles` properties change.
//!
//! When neither name is on the bus the listen loop emits the default
//! (empty) state and re-probes every 30s. UI hides itself when
//! `available.is_empty()`.

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_reactive::{registry, runtime, Service};
use std::collections::HashMap;
use std::time::Duration;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

#[derive(Clone, Debug, Default)]
pub struct PowerProfilesState {
    pub active: String,
    pub available: Vec<String>,
}

#[doc(hidden)]
pub struct PowerProfilesHandles {
    pub(crate) state: Mutable<PowerProfilesState>,
}

impl Default for PowerProfilesHandles {
    fn default() -> Self {
        Self {
            state: Mutable::new(PowerProfilesState::default()),
        }
    }
}

pub struct PowerProfilesService;

impl Service for PowerProfilesService {
    type Handles = PowerProfilesHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = PowerProfilesHandles::default();
        let writer = handles.state.clone();
        rt.spawn(async move {
            loop {
                match listen(&writer).await {
                    Ok(()) => tracing::debug!("power_profiles listen ended, retrying in 5s"),
                    Err(e) => tracing::warn!(error = %e, "power_profiles error, retrying in 5s"),
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        handles
    }
}

#[must_use]
pub fn service() -> PowerProfilesService {
    PowerProfilesService
}

pub fn state() -> impl Signal<Item = PowerProfilesState> {
    registry::with(|r| {
        r.get::<PowerProfilesHandles>()
            .expect("power_profiles::service() not registered")
            .state
            .signal_cloned()
    })
}

pub fn set_active(profile: &str) {
    let profile = profile.to_string();
    runtime::handle().spawn(async move {
        if let Err(e) = do_set_active(&profile).await {
            tracing::warn!(error = %e, profile, "power_profiles set_active failed");
        }
    });
}

#[must_use]
pub fn humanize_profile(name: &str) -> String {
    match name {
        "performance" => "Performance".to_string(),
        "balanced" => "Balanced".to_string(),
        "power-saver" => "Power saver".to_string(),
        other => other.to_string(),
    }
}

const CANONICAL_NAME: &str = "net.hadess.PowerProfiles";
const CANONICAL_PATH: &str = "/net/hadess/PowerProfiles";
const FREEDESKTOP_NAME: &str = "org.freedesktop.UPower.PowerProfiles";
const FREEDESKTOP_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";

async fn listen(writer: &Mutable<PowerProfilesState>) -> Result<()> {
    let conn = Connection::system().await.context("connect system bus")?;
    let proxy = match build_proxy(&conn).await {
        Some(p) => p,
        None => {
            writer.set(PowerProfilesState::default());
            tokio::time::sleep(Duration::from_secs(30)).await;
            return Err(anyhow::anyhow!("power-profiles-daemon not on bus"));
        }
    };

    refresh_state(&proxy, writer).await?;

    let mut props = proxy
        .receive_signal("PropertiesChanged")
        .await
        .context("subscribe PropertiesChanged")?;
    while props.next().await.is_some() {
        if let Err(e) = refresh_state(&proxy, writer).await {
            tracing::warn!(error = %e, "power_profiles refresh failed");
        }
    }
    Ok(())
}

async fn build_proxy(conn: &Connection) -> Option<zbus::Proxy<'static>> {
    if let Ok(p) = zbus::Proxy::new(conn, CANONICAL_NAME, CANONICAL_PATH, CANONICAL_NAME).await
        && probe(&p).await
    {
        return Some(p);
    }
    if let Ok(p) =
        zbus::Proxy::new(conn, FREEDESKTOP_NAME, FREEDESKTOP_PATH, FREEDESKTOP_NAME).await
        && probe(&p).await
    {
        return Some(p);
    }
    None
}

async fn probe(p: &zbus::Proxy<'_>) -> bool {
    p.get_property::<String>("ActiveProfile").await.is_ok()
}

async fn refresh_state(
    proxy: &zbus::Proxy<'_>,
    writer: &Mutable<PowerProfilesState>,
) -> Result<()> {
    let active: String = proxy.get_property("ActiveProfile").await.unwrap_or_default();

    let raw: Vec<HashMap<String, OwnedValue>> = proxy
        .get_property("Profiles")
        .await
        .unwrap_or_default();
    let available: Vec<String> = raw
        .into_iter()
        .filter_map(|m| {
            m.get("Profile")
                .and_then(|v| v.try_clone().ok())
                .and_then(|v| String::try_from(v).ok())
        })
        .collect();

    writer.set(PowerProfilesState { active, available });
    Ok(())
}

// ── Command channel with reconnect-on-IO ─────────────────────────────────────

static CMD_CONN: tokio::sync::Mutex<Option<Connection>> = tokio::sync::Mutex::const_new(None);

async fn cmd_conn() -> Result<Connection> {
    let mut guard = CMD_CONN.lock().await;
    if guard.is_none() {
        let fresh = Connection::system()
            .await
            .context("open shared power_profiles command connection")?;
        *guard = Some(fresh);
    }
    Ok(guard
        .as_ref()
        .expect("just stored Some")
        .clone())
}

async fn evict_cmd_conn() {
    *CMD_CONN.lock().await = None;
}

fn is_io_error(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<zbus::Error>()
            .is_some_and(|ze| matches!(ze, zbus::Error::InputOutput(_)))
    })
}

async fn do_set_active(profile: &str) -> Result<()> {
    if try_set_active_at(CANONICAL_NAME, CANONICAL_PATH, profile)
        .await
        .is_ok()
    {
        return Ok(());
    }
    try_set_active_at(FREEDESKTOP_NAME, FREEDESKTOP_PATH, profile).await
}

async fn try_set_active_at(name: &str, path: &str, profile: &str) -> Result<()> {
    let conn = cmd_conn().await?;
    let r = conn
        .call_method(
            Some(name),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Set",
            &(name, "ActiveProfile", zbus::zvariant::Value::from(profile)),
        )
        .await
        .with_context(|| format!("call Properties.Set ActiveProfile on {name}"));
    if let Err(ref e) = r && is_io_error(e) {
        evict_cmd_conn().await;
    }
    r.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_known_profiles() {
        assert_eq!(humanize_profile("performance"), "Performance");
        assert_eq!(humanize_profile("balanced"), "Balanced");
        assert_eq!(humanize_profile("power-saver"), "Power saver");
    }

    #[test]
    fn humanize_unknown_profile_passes_through() {
        assert_eq!(humanize_profile("custom-fast"), "custom-fast");
    }
}
```

- [ ] **Step 2: Re-export from `lib.rs`**

In `crates/hytte-services/src/lib.rs`, add `pub mod power_profiles;` alphabetically (after `polkit` if present, before `resolved`):

```rust
pub mod power_profiles;
```

Run `grep -n 'pub mod' crates/hytte-services/src/lib.rs` to confirm the alphabetical position.

- [ ] **Step 3: Register in `main.rs`**

In `trollshell/src/main.rs`, find the existing `.with(...)` chain. Add `.with(power_profiles::service())` alphabetically (after `.with(polkit::service())` if present, before `.with(resolved::service())`):

```rust
.with(power_profiles::service())
```

Add `use hytte::services::power_profiles;` at the top of `main.rs` if `power_profiles` isn't already in scope via wildcard.

- [ ] **Step 4: Run tests**

Run: `cargo test -p hytte-services power_profiles::tests -- --nocapture`
Expected: 2 passed (`humanize_known_profiles`, `humanize_unknown_profile_passes_through`).

- [ ] **Step 5: Build + clippy**

Run: `cargo build -p hytte-services -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/power_profiles.rs crates/hytte-services/src/lib.rs trollshell/src/main.rs
git commit -m "$(cat <<'EOF'
feat(power-profiles): expose net.hadess.PowerProfiles state + setter

New crates/hytte-services/src/power_profiles.rs subscribes to
net.hadess.PowerProfiles (canonical) on the system bus, falling
back to org.freedesktop.UPower.PowerProfiles when the canonical
name has no owner. Public state() signal exposes ActiveProfile +
the daemon's Profiles list as a flat Vec<String>; set_active()
calls Properties.Set on the daemon. Shared CMD_CONN with
reconnect-on-IO mirrors the v0.2.6 BlueZ/wifi pattern. Hidden
gracefully when daemon absent (empty state, slow re-probe).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Power profile UI in `page_power`

**Files:**

- Modify: `trollshell/src/widgets/pages.rs`

**Background:** Add an `AdwExpanderRow` to the existing Battery group in `page_power`. Title "Power profile"; subtitle binds to humanized active profile. Prefix is a `gtk::Image` that swaps icon name per active profile. Nested rows drain-and-rebuild on each `state()` emission, with a checkmark suffix on the active profile's row.

- [ ] **Step 1: Add the `power_profiles` import**

In `trollshell/src/widgets/pages.rs`, near the existing service imports, add:

```rust
use hytte::services::power_profiles::{self, humanize_profile};
```

- [ ] **Step 2: Add `build_power_profile_expander` and `profile_icon_name`**

After the existing `page_power` function (or grouped with `describe_battery` and other power helpers), add:

```rust
fn build_power_profile_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder()
        .title("Power profile")
        .build();

    bind(
        power_profiles::state().map(|s| !s.available.is_empty()),
        &expander,
        gtk::prelude::WidgetExt::set_visible,
    );

    bind(
        power_profiles::state().map(|s| humanize_profile(&s.active)),
        &expander,
        |row, t| row.set_subtitle(&t),
    );

    let icon = gtk::Image::new();
    icon.set_valign(gtk::Align::Center);
    bind(
        power_profiles::state().map(|s| profile_icon_name(&s.active)),
        &icon,
        |w, name| w.set_icon_name(Some(name)),
    );
    expander.add_prefix(&icon);

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(power_profiles::state(), &expander, move |_, state| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        let mut new_rows = Vec::with_capacity(state.available.len());
        for profile in &state.available {
            let row = adw::ActionRow::builder()
                .title(humanize_profile(profile))
                .activatable(true)
                .build();
            if profile == &state.active {
                let check = gtk::Image::from_icon_name("object-select-symbolic");
                check.set_valign(gtk::Align::Center);
                row.add_suffix(&check);
            }
            let profile_owned = profile.clone();
            row.connect_activated(move |_| {
                power_profiles::set_active(&profile_owned);
            });
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
}

fn profile_icon_name(active: &str) -> &'static str {
    match active {
        "performance" => "power-profile-performance-symbolic",
        "balanced" => "power-profile-balanced-symbolic",
        "power-saver" => "power-profile-power-saver-symbolic",
        _ => "system-run-symbolic",
    }
}
```

- [ ] **Step 3: Wire into `page_power`'s Battery group**

Find the existing `page_power` function. After the existing `battery_group.add(&battery_row);` line, add:

```rust
battery_group.add(&build_power_profile_expander());
```

- [ ] **Step 4: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(de): power-profile expander in Battery group

Adds an AdwExpanderRow to the existing Battery PreferencesGroup in
page_power. Title "Power profile"; subtitle binds to the humanized
active profile name; prefix is a gtk::Image whose icon swaps per
active profile (power-profile-{active}-symbolic, fallback
system-run-symbolic). Nested rows drain-rebuild on each state()
emission with a checkmark suffix on the active profile. Whole
expander hides when power-profiles-daemon is absent (empty
available list).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: OSD `Kind::Battery` + `BatteryEvent` + `detect_battery_event`

**Files:**

- Modify: `trollshell/src/widgets/osd.rs`

**Background:** Extend the OSD `Kind` enum, add a `BatteryEvent` enum modeling the five fire conditions, and write `detect_battery_event` as a pure helper unit-tested for plug/unplug + 3 threshold crossings + steady-state suppression + Unknown-state suppression. No subscription wiring yet (Task 4 wires it).

- [ ] **Step 1: Extend `Kind`**

Find the existing `enum Kind` (around line 58). Add `Battery`:

```rust
enum Kind {
    Volume,
    Mic,
    Brightness,
    Battery,
}
```

Update the `css_class` impl:

```rust
impl Kind {
    fn css_class(self) -> &'static str {
        match self {
            Self::Volume => "volume",
            Self::Mic => "mic",
            Self::Brightness => "brightness",
            Self::Battery => "battery",
        }
    }
}
```

- [ ] **Step 2: Add `upower` import**

Near the top of `trollshell/src/widgets/osd.rs`, alongside the existing `use hytte::services::*` imports:

```rust
use hytte::services::upower::{self, Battery, BatteryState};
```

- [ ] **Step 3: Add the failing tests**

Append to the existing `#[cfg(test)] mod tests` block at the bottom of `osd.rs` (it exists from v0.2.3 Task 7):

```rust
fn batt(percentage: f64, state: BatteryState) -> Battery {
    Battery {
        percentage,
        state,
        ..Battery::default()
    }
}

#[test]
fn detect_plug_in() {
    let prev = batt(50.0, BatteryState::Discharging);
    let curr = batt(50.0, BatteryState::Charging);
    assert!(matches!(
        detect_battery_event(Some(&prev), &curr),
        Some(BatteryEvent::PluggedIn)
    ));
}

#[test]
fn detect_unplug() {
    let prev = batt(80.0, BatteryState::Charging);
    let curr = batt(80.0, BatteryState::Discharging);
    assert!(matches!(
        detect_battery_event(Some(&prev), &curr),
        Some(BatteryEvent::Unplugged)
    ));
}

#[test]
fn detect_low_threshold_cross() {
    let prev = batt(22.0, BatteryState::Discharging);
    let curr = batt(19.0, BatteryState::Discharging);
    assert!(matches!(
        detect_battery_event(Some(&prev), &curr),
        Some(BatteryEvent::LowBattery)
    ));
}

#[test]
fn detect_critical_threshold_cross() {
    let prev = batt(11.0, BatteryState::Discharging);
    let curr = batt(9.0, BatteryState::Discharging);
    assert!(matches!(
        detect_battery_event(Some(&prev), &curr),
        Some(BatteryEvent::CriticalBattery)
    ));
}

#[test]
fn detect_imminent_shutdown_threshold_cross() {
    let prev = batt(6.0, BatteryState::Discharging);
    let curr = batt(4.0, BatteryState::Discharging);
    assert!(matches!(
        detect_battery_event(Some(&prev), &curr),
        Some(BatteryEvent::ImminentShutdown)
    ));
}

#[test]
fn no_event_on_steady_discharge() {
    let prev = batt(50.0, BatteryState::Discharging);
    let curr = batt(49.0, BatteryState::Discharging);
    assert!(detect_battery_event(Some(&prev), &curr).is_none());
}

#[test]
fn no_event_on_unknown_state() {
    let prev = batt(50.0, BatteryState::Unknown);
    let curr = batt(50.0, BatteryState::Charging);
    assert!(detect_battery_event(Some(&prev), &curr).is_none());
}
```

- [ ] **Step 4: Run tests, verify they fail**

Run: `cargo test -p trollshell osd::tests -- --nocapture`
Expected: compile error — `BatteryEvent`, `detect_battery_event` not defined.

- [ ] **Step 5: Add `BatteryEvent` + `detect_battery_event` + threshold constants**

Below the `Kind` enum / `impl Kind` block (or alongside other helper definitions in `osd.rs`):

```rust
const LOW_THRESHOLD: f64 = 20.0;
const CRITICAL_THRESHOLD: f64 = 10.0;
const IMMINENT_THRESHOLD: f64 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatteryEvent {
    PluggedIn,
    Unplugged,
    LowBattery,
    CriticalBattery,
    ImminentShutdown,
}

fn detect_battery_event(prev: Option<&Battery>, curr: &Battery) -> Option<BatteryEvent> {
    let prev = prev?;

    if curr.state == BatteryState::Unknown || prev.state == BatteryState::Unknown {
        return None;
    }

    if prev.state == BatteryState::Discharging && curr.state == BatteryState::Charging {
        return Some(BatteryEvent::PluggedIn);
    }
    if prev.state == BatteryState::Charging && curr.state == BatteryState::Discharging {
        return Some(BatteryEvent::Unplugged);
    }

    if curr.state == BatteryState::Discharging && prev.state == BatteryState::Discharging {
        if prev.percentage > IMMINENT_THRESHOLD && curr.percentage <= IMMINENT_THRESHOLD {
            return Some(BatteryEvent::ImminentShutdown);
        }
        if prev.percentage > CRITICAL_THRESHOLD && curr.percentage <= CRITICAL_THRESHOLD {
            return Some(BatteryEvent::CriticalBattery);
        }
        if prev.percentage > LOW_THRESHOLD && curr.percentage <= LOW_THRESHOLD {
            return Some(BatteryEvent::LowBattery);
        }
    }

    None
}
```

- [ ] **Step 6: Run tests, verify all 7 pass**

Run: `cargo test -p trollshell osd::tests -- --nocapture`
Expected: 7 new tests passing (plus the 7 pre-existing v0.2.3 ignored tests stay ignored).

- [ ] **Step 7: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add trollshell/src/widgets/osd.rs
git commit -m "$(cat <<'EOF'
feat(osd): Kind::Battery + BatteryEvent + detect_battery_event helper

Adds a fourth Kind variant for battery OSDs and a pure
detect_battery_event(prev, curr) helper that returns one of:
PluggedIn (Discharging→Charging), Unplugged (Charging→Discharging),
LowBattery (crossed 20% from above while discharging),
CriticalBattery (crossed 10%), ImminentShutdown (crossed 5%).
Most-severe wins on a single tick. Unknown state on either side
suppresses. Seven unit tests cover plug/unplug + each threshold +
steady-state and unknown-state suppression.

Subscription wiring lands in the next commit.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: OSD battery subscription + `render_battery`

**Files:**

- Modify: `trollshell/src/widgets/osd.rs`

**Background:** Wire a fourth subscription in `install_subscriptions` that reads `upower::battery()`, maintains a `last_battery: RefCell<Option<Battery>>` baseline (seeded silently on first emission), runs `detect_battery_event`, and on `Some(event)` calls `render_battery(event, &batt)` then `route_show(&state)`.

- [ ] **Step 1: Add `render_battery`**

Below `render_brightness` (around line 285 — find it by searching for `fn render_brightness`):

```rust
fn render_battery(event: BatteryEvent, batt: &Battery) -> State {
    let (icon, label, value) = match event {
        BatteryEvent::PluggedIn => (
            "battery-charging-symbolic",
            "Charging",
            format!("{:.0}%", batt.percentage),
        ),
        BatteryEvent::Unplugged => (
            "battery-symbolic",
            "On battery",
            format!("{:.0}%", batt.percentage),
        ),
        BatteryEvent::LowBattery => (
            "battery-low-symbolic",
            "Low battery",
            format!("{:.0}%", batt.percentage),
        ),
        BatteryEvent::CriticalBattery => (
            "battery-caution-symbolic",
            "Critical battery",
            format!("{:.0}%", batt.percentage),
        ),
        BatteryEvent::ImminentShutdown => (
            "battery-caution-symbolic",
            "Battery very low",
            format!("{:.0}%", batt.percentage),
        ),
    };
    State {
        kind: Kind::Battery,
        icon,
        fraction: (batt.percentage / 100.0).clamp(0.0, 1.0),
        label,
        value,
        muted: false,
    }
}
```

- [ ] **Step 2: Add the battery subscription block**

In `install_subscriptions` (around line 209), after the existing `// ── Brightness ────` block but before the `// ── Focused output ────` block, add:

```rust
    // ── Battery ───────────────────────────────────────────────────────
    //
    // Edge + threshold detection. The first emission seeds a baseline
    // silently; subsequent emissions diff against it via
    // detect_battery_event.
    {
        let first = Cell::new(true);
        let last_battery: RefCell<Option<Battery>> = RefCell::new(None);
        glib::MainContext::default().spawn_local(upower::battery().for_each(
            move |batt: Battery| {
                if first.replace(false) {
                    *last_battery.borrow_mut() = Some(batt);
                    return std::future::ready(());
                }
                let prev_snapshot = last_battery.borrow().clone();
                if let Some(event) = detect_battery_event(prev_snapshot.as_ref(), &batt) {
                    let state = render_battery(event, &batt);
                    route_show(&state);
                }
                *last_battery.borrow_mut() = Some(batt);
                std::future::ready(())
            },
        ));
    }
```

- [ ] **Step 3: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add trollshell/src/widgets/osd.rs
git commit -m "$(cat <<'EOF'
feat(osd): battery subscription + render_battery

Adds a fourth subscription in install_subscriptions reading
upower::battery(). Maintains a last_battery baseline seeded
silently on the first emission (bootstrap suppression). On each
subsequent emission, detect_battery_event diffs against the baseline
and route_show fires the OSD on plug/unplug or threshold crossing.
Reuses existing OSD machinery: latest-wins debounce, fade+slide,
focused-output routing, drawer-open suppression.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: CSS — `.ts-osd-card.battery` tint hook

**Files:**

- Modify: `trollshell/style.css`

**Background:** Reserved per-kind tint hook. Same accent treatment as Volume / Brightness — the icon name (`battery-low-symbolic`, `battery-caution-symbolic`) carries the visual urgency on its own. Future polish could add an `.urgent` class with `@error_color`.

- [ ] **Step 1: Append the rule**

At the bottom of `trollshell/style.css`, after the existing OSD rules:

```css
.ts-osd-card.battery .ts-osd-icon {
  color: @accent_color;
}
```

- [ ] **Step 2: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Manual smoke test (deferred to user)**

In a Niri session: `cargo run --release -p trollshell`.

- Power profiles: open Power drawer → "Power profile" expander appears below Charge. Expand → list of profiles with checkmark on the active one. Click another → check `powerprofilesctl get`. External `powerprofilesctl set performance` → UI updates within ~1s.
- Battery OSD: plug AC → "Charging — N%" pops on focused monitor. Unplug → "On battery — N%". Drain past 20% → "Low battery". 10% → "Critical battery". 5% → "Battery very low". Bootstrap (first launch): no OSD on startup.

- [ ] **Step 4: Commit**

```bash
git add trollshell/style.css
git commit -m "$(cat <<'EOF'
style: OSD battery — accent-tinted icon

Reserved per-kind tint hook for Kind::Battery. The accent treatment
matches Volume / Brightness; battery-low-symbolic and
battery-caution-symbolic icon names carry the urgency visual on
their own. No new color tokens.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review notes

**Spec coverage:**

- Spec §1 power_profiles service → Task 1.
- Spec §2 power_profiles UI → Task 2.
- Spec §3 OSD Kind::Battery + BatteryEvent + detect_battery_event → Task 3.
- Spec §3 OSD render_battery + subscription → Task 4.
- Spec §3 CSS → Task 5.

**Final verification:**

- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green; new unit tests in `power_profiles::tests` (2) + `osd::tests` (7).
- Manual smoke test (deferred) covers each spec success criterion.
