# hytte + trollshell v0.2.0 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship v0.2.0 — "systemet syns" — a `Popup` UI primitive plus four read-only system services (`upower`, `pipewire`, `networkd`, `resolved`) wired into a right-cluster of indicators in `trollshell` (battery, volume, network) where each indicator opens an anchored popup with detail.

**Architecture:** Build on v0.1. Add `hytte-ui::popup` (a thin wrapper around `gtk::Popover` parented to a trigger widget). Add four service modules to `hytte-services` — three pure zbus DBus clients (`upower`, `networkd`, `resolved`) and one shell-out poller (`pipewire`, polling `wpctl get-volume`). Each service exposes a typed `Mutable<State>` via the existing registry pattern. Bars get a new opt-in `keyboard_interactivity()` builder method so popovers receive input.

**Tech Stack:** Existing v0.1 stack + `zbus` (with `tokio` runtime feature), shell-out to `wpctl` for v0.2.0 volume tracking (replaceable with `pipewire-rs` in v0.3+).

**Out of scope (v0.2.1 / later):** `Panel` primitive, `bluetooth` service, write controls (volume slider, wifi join, brightness), notifications daemon. Read-only indicators only.

---

## File Structure

After this plan completes:

```
crates/hytte-ui/src/
├── popup.rs               # NEW: Popup primitive
├── bar.rs                 # MODIFIED: + keyboard_interactivity setter
├── layer_window.rs        # MODIFIED: + keyboard_mode setter on builder
└── lib.rs                 # MODIFIED: + popup module + KeyboardMode re-export

crates/hytte-services/src/
├── upower.rs              # NEW: battery via UPower DBus
├── pipewire.rs            # NEW: volume via wpctl polling
├── networkd.rs            # NEW: link state via systemd-networkd DBus
├── resolved.rs            # NEW: DNS state via systemd-resolved DBus
└── lib.rs                 # MODIFIED: + four module decls

trollshell/src/widgets/
├── battery.rs             # NEW: battery icon + popup
├── volume.rs              # NEW: volume icon + popup
├── network.rs             # NEW: network icon + popup
└── mod.rs                 # MODIFIED: + three new mods

trollshell/
├── src/main.rs            # MODIFIED: right cluster wiring
└── style.css              # MODIFIED: indicator styles + popup content styles
```

---

## Pre-flight

Verify required system packages:

```sh
pacman -Qi upower wireplumber pipewire systemd 2>&1 | grep '^Name'
```

Expected: lines for `upower`, `wireplumber`, `pipewire`, `systemd`. UPower and wireplumber may not be installed by default.

```sh
sudo pacman -S --needed upower wireplumber
```

---

## Task 1: `Bar::keyboard_interactivity()` builder method

`gtk::Popover` opened from a layer-shell window needs the surface to declare keyboard interactivity, otherwise the compositor won't route input to the popover. Plumb a setter through `LayerWindowBuilder` and `Bar`.

**Files:**
- Modify: `crates/hytte-ui/src/layer_window.rs`
- Modify: `crates/hytte-ui/src/bar.rs`
- Modify: `crates/hytte-ui/src/lib.rs`

- [ ] **Step 1: Add `keyboard_mode` to `LayerWindowBuilder`**

In `crates/hytte-ui/src/layer_window.rs`:

Add a `keyboard_mode: Option<KeyboardMode>` field to `LayerWindowBuilder` struct. Add the import at the top:

```rust
use gtk4_layer_shell::{Edge as LsEdge, KeyboardMode, Layer, LayerShell};
```

In the struct, add after `exclusive: bool,`:

```rust
    keyboard_mode: Option<KeyboardMode>,
```

In the `layer_window(monitor)` constructor, add to the struct literal:

```rust
        keyboard_mode: None,
```

Add a builder method on `LayerWindowBuilder` (with `#[must_use]`):

```rust
    #[must_use]
    pub fn keyboard_mode(mut self, mode: KeyboardMode) -> Self {
        self.keyboard_mode = Some(mode);
        self
    }
```

In `build()`, after `if self.exclusive { window.auto_exclusive_zone_enable(); }`, add:

```rust
        if let Some(mode) = self.keyboard_mode {
            window.set_keyboard_mode(mode);
        }
```

- [ ] **Step 2: Re-export `KeyboardMode` from `lib.rs`**

In `crates/hytte-ui/src/lib.rs`, change the line:

```rust
pub use gtk4_layer_shell::Layer;
```

to:

```rust
pub use gtk4_layer_shell::{KeyboardMode, Layer};
```

- [ ] **Step 3: Add `Bar::keyboard_interactivity()`**

In `crates/hytte-ui/src/bar.rs`:

Add to the imports:

```rust
use gtk4_layer_shell::KeyboardMode;
```

Add a field on the `Bar` struct, after `exclusive: bool,`:

```rust
    keyboard_mode: Option<KeyboardMode>,
```

In `Bar::new(monitor)`, add to the struct literal:

```rust
            keyboard_mode: None,
```

Add a builder method (place between `exclusive` and `left`):

```rust
    /// Enable keyboard input for popovers spawned from bar widgets.
    ///
    /// Layer-shell surfaces have `KeyboardMode::None` by default, which
    /// means popovers parented to a bar widget will not receive keyboard
    /// input. Pass `KeyboardMode::OnDemand` to allow popovers to grab
    /// focus when shown.
    #[must_use]
    pub fn keyboard_interactivity(mut self, mode: KeyboardMode) -> Self {
        self.keyboard_mode = Some(mode);
        self
    }
```

In `show()`, after `.namespace(...)`, before `.build()`, add a conditional:

```rust
        let mut builder = layer_window(&self.monitor)
            .anchor(anchor_main)
            .anchor(anchor_perp.0)
            .anchor(anchor_perp.1)
            .margin(self.margin)
            .exclusive(self.exclusive)
            .namespace(format!("hytte-bar-{:?}", self.edge).to_lowercase());
        if let Some(mode) = self.keyboard_mode {
            builder = builder.keyboard_mode(mode);
        }
        let window = builder.build();
```

(Replace the existing `let window = layer_window(...).build();` chain with the above. Verify with `cargo check -p hytte-ui` after the edit.)

Re-export `KeyboardMode` from `bar.rs` so consumers don't have to dig: not necessary since `lib.rs` re-exports it now.

- [ ] **Step 4: Build**

Run: `cargo check -p hytte-ui`
Expected: clean.

- [ ] **Step 5: Commit**

```sh
git add crates/hytte-ui
git commit -m "feat(ui): Bar::keyboard_interactivity() for popover support"
```

---

## Task 2: `hytte-ui::popup` — anchored popup primitive

Wraps `gtk::Popover`, parented to a trigger widget. Builder takes the trigger + content; `show()`/`hide()`/`toggle()` control visibility. Click-outside dismisses automatically (Popover default).

**Files:**
- Create: `crates/hytte-ui/src/popup.rs`
- Modify: `crates/hytte-ui/src/lib.rs`
- Modify: `crates/hytte-ui/src/style.css` (add popup content default)

- [ ] **Step 1: Implement `popup.rs`**

Create `crates/hytte-ui/src/popup.rs`:

```rust
//! `Popup` — an anchored popover hosted on a trigger widget.
//!
//! Wraps `gtk::Popover`. The popover is `set_parent(&trigger)`'d so it
//! positions automatically relative to the trigger's allocation. Click
//! outside dismisses (default Popover behaviour).
//!
//! For popups spawned from a `Bar`, the bar must be built with
//! `Bar::keyboard_interactivity(KeyboardMode::OnDemand)` so the layer
//! surface can grant keyboard focus to the popover.

use gtk::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Position {
    Top,
    Bottom,
    Left,
    Right,
}

pub struct PopupBuilder {
    anchor: gtk::Widget,
    child: Option<gtk::Widget>,
    position: Position,
    has_arrow: bool,
    css_class: Option<String>,
}

impl PopupBuilder {
    #[must_use]
    pub fn child(mut self, child: impl IsA<gtk::Widget>) -> Self {
        self.child = Some(child.upcast());
        self
    }

    #[must_use]
    pub fn position(mut self, position: Position) -> Self {
        self.position = position;
        self
    }

    #[must_use]
    pub fn has_arrow(mut self, on: bool) -> Self {
        self.has_arrow = on;
        self
    }

    #[must_use]
    pub fn css_class(mut self, class: impl Into<String>) -> Self {
        self.css_class = Some(class.into());
        self
    }

    /// Build the popover. The popover is parented to the anchor widget;
    /// dropping the returned handle does *not* close it (the parent owns
    /// it via GTK's reference counting).
    #[must_use]
    pub fn build(self) -> Popup {
        let popover = gtk::Popover::new();
        popover.set_parent(&self.anchor);
        popover.set_position(map_position(self.position));
        popover.set_has_arrow(self.has_arrow);
        popover.set_autohide(true);
        popover.add_css_class("hytte-popup");
        if let Some(class) = self.css_class {
            popover.add_css_class(&class);
        }
        if let Some(child) = self.child {
            popover.set_child(Some(&child));
        }
        Popup { popover }
    }
}

/// Handle to a built popover. Cheap to clone (refcounted GObject).
#[derive(Clone)]
pub struct Popup {
    popover: gtk::Popover,
}

impl Popup {
    #[must_use]
    pub fn new(anchor: &impl IsA<gtk::Widget>) -> PopupBuilder {
        PopupBuilder {
            anchor: anchor.clone().upcast(),
            child: None,
            position: Position::Bottom,
            has_arrow: false,
            css_class: None,
        }
    }

    pub fn show(&self) {
        self.popover.popup();
    }

    pub fn hide(&self) {
        self.popover.popdown();
    }

    pub fn toggle(&self) {
        if self.popover.is_visible() {
            self.popover.popdown();
        } else {
            self.popover.popup();
        }
    }

    /// Underlying `gtk::Popover` for advanced use.
    #[must_use]
    pub fn popover(&self) -> &gtk::Popover {
        &self.popover
    }
}

fn map_position(p: Position) -> gtk::PositionType {
    match p {
        Position::Top => gtk::PositionType::Top,
        Position::Bottom => gtk::PositionType::Bottom,
        Position::Left => gtk::PositionType::Left,
        Position::Right => gtk::PositionType::Right,
    }
}
```

- [ ] **Step 2: Re-export from `lib.rs`**

In `crates/hytte-ui/src/lib.rs`:

Add `mod popup;` to the module list (after `mod monitor;`):

```rust
mod popup;
```

Add to the public re-exports (after the bar re-export):

```rust
pub use popup::{Popup, PopupBuilder, Position as PopupPosition};
```

- [ ] **Step 3: Default popup styling**

Append to `crates/hytte-ui/src/style.css`:

```css
/* Popup defaults */
.hytte-popup > contents {
    background: rgba(28, 28, 32, 0.94);
    color: #f5f5f7;
    border: 1px solid rgba(255, 255, 255, 0.08);
    border-radius: 8px;
    padding: 8px 10px;
    box-shadow: 0 6px 20px rgba(0, 0, 0, 0.5);
}
```

- [ ] **Step 4: Build**

Run: `cargo check -p hytte-ui`
Expected: clean.

- [ ] **Step 5: Add `Popup` and `PopupBuilder` to the umbrella prelude**

In `crates/hytte/src/lib.rs`, in the `prelude` module, add to the `hytte_ui` use line:

```rust
    pub use hytte_ui::{
        App, Anchor, Bar, BarHandle, Edge, KeyboardMode, Layer, Margin, Monitor, Popup,
        PopupBuilder, PopupPosition,
    };
```

(Replace the existing `pub use hytte_ui::{App, Anchor, Bar, BarHandle, Edge, Layer, Margin, Monitor};` with the above expanded list.)

- [ ] **Step 6: Commit**

```sh
git add crates/hytte-ui crates/hytte
git commit -m "feat(ui): Popup primitive over gtk::Popover + prelude exports"
```

---

## Task 3: `hytte-services::upower` — battery service

Read battery state from UPower's `DisplayDevice` (the aggregated battery, suitable for laptops with one main battery). Subscribe to `PropertiesChanged` on the system bus.

**Files:**
- Modify: `crates/hytte-services/Cargo.toml` (add zbus)
- Create: `crates/hytte-services/src/upower.rs`
- Modify: `crates/hytte-services/src/lib.rs`

- [ ] **Step 1: Add zbus**

Run: `cargo add -p hytte-services zbus --features tokio --no-default-features`
Expected: `zbus` added with tokio runtime feature.

- [ ] **Step 2: Implement `upower.rs`**

Create `crates/hytte-services/src/upower.rs`:

```rust
//! Battery state via UPower.
//!
//! Subscribes to `org.freedesktop.UPower.Device.PropertiesChanged` on the
//! `/org/freedesktop/UPower/devices/DisplayDevice` path of the UPower
//! daemon (the aggregated battery — one entry covering all batteries on
//! the system).

use anyhow::{anyhow, Context, Result};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use std::time::Duration;
use zbus::Connection;

pub struct UpowerService;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BatteryState {
    Unknown,
    Charging,
    Discharging,
    Empty,
    FullyCharged,
    PendingCharge,
    PendingDischarge,
}

impl BatteryState {
    fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Charging,
            2 => Self::Discharging,
            3 => Self::Empty,
            4 => Self::FullyCharged,
            5 => Self::PendingCharge,
            6 => Self::PendingDischarge,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Battery {
    /// Charge percentage, `0.0..=100.0`.
    pub percentage: f64,
    /// Charge/discharge state.
    pub state: BatteryState,
    /// Approximate seconds until empty (when discharging).
    pub time_to_empty: Option<Duration>,
    /// Approximate seconds until full (when charging).
    pub time_to_full: Option<Duration>,
    /// Free-form icon name from UPower (e.g. `"battery-good-symbolic"`).
    pub icon_name: String,
}

impl Default for Battery {
    fn default() -> Self {
        Self {
            percentage: 0.0,
            state: BatteryState::Unknown,
            time_to_empty: None,
            time_to_full: None,
            icon_name: String::new(),
        }
    }
}

#[doc(hidden)]
pub struct UpowerHandles {
    pub(crate) battery: Mutable<Battery>,
}

impl Default for UpowerHandles {
    fn default() -> Self {
        Self {
            battery: Mutable::new(Battery::default()),
        }
    }
}

impl Service for UpowerService {
    type Handles = UpowerHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = UpowerHandles::default();
        let writer = handles.battery.clone();

        rt.spawn(async move {
            loop {
                match listen(&writer).await {
                    Ok(()) => tracing::warn!("upower stream closed, reconnecting in 1s"),
                    Err(e) => tracing::warn!(error = %e, "upower error, reconnecting in 1s"),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        handles
    }
}

async fn listen(battery: &Mutable<Battery>) -> Result<()> {
    let conn = Connection::system().await.context("connect system bus")?;

    // Read all properties of the DisplayDevice.
    let read = || async {
        let proxy = zbus::Proxy::new(
            &conn,
            "org.freedesktop.UPower",
            "/org/freedesktop/UPower/devices/DisplayDevice",
            "org.freedesktop.UPower.Device",
        )
        .await
        .context("create DisplayDevice proxy")?;

        let percentage: f64 = proxy.get_property("Percentage").await.unwrap_or(0.0);
        let state: u32 = proxy.get_property("State").await.unwrap_or(0);
        let time_to_empty: i64 = proxy.get_property("TimeToEmpty").await.unwrap_or(0);
        let time_to_full: i64 = proxy.get_property("TimeToFull").await.unwrap_or(0);
        let icon_name: String = proxy.get_property("IconName").await.unwrap_or_default();

        Ok::<Battery, anyhow::Error>(Battery {
            percentage,
            state: BatteryState::from_u32(state),
            time_to_empty: u64::try_from(time_to_empty).ok().map(Duration::from_secs),
            time_to_full: u64::try_from(time_to_full).ok().map(Duration::from_secs),
            icon_name,
        })
    };

    // Initial state.
    battery.set(read().await?);

    // Subscribe to PropertiesChanged.
    let proxy = zbus::fdo::PropertiesProxy::builder(&conn)
        .destination("org.freedesktop.UPower")
        .map_err(|e| anyhow!("set destination: {e}"))?
        .path("/org/freedesktop/UPower/devices/DisplayDevice")
        .map_err(|e| anyhow!("set path: {e}"))?
        .build()
        .await
        .context("build properties proxy")?;

    use futures_util::StreamExt;
    let mut changes = proxy.receive_properties_changed().await?;
    while changes.next().await.is_some() {
        battery.set(read().await?);
    }
    Ok(())
}

#[must_use]
pub fn service() -> UpowerService {
    UpowerService
}

pub fn battery() -> impl Signal<Item = Battery> {
    registry::with(|r| {
        r.get::<UpowerHandles>()
            .expect("upower::service() not registered")
            .battery
            .signal_cloned()
    })
}
```

- [ ] **Step 3: Update `lib.rs`**

In `crates/hytte-services/src/lib.rs`, append:

```rust
pub mod upower;
```

- [ ] **Step 4: Build**

Run: `cargo check -p hytte-services`
Expected: clean.

- [ ] **Step 5: Commit**

```sh
git add crates/hytte-services
git commit -m "feat(services): upower battery state via DBus"
```

---

## Task 4: `hytte-services::pipewire` — volume via `wpctl` polling

Pragmatic v0.2.0 path: poll `wpctl get-volume @DEFAULT_AUDIO_SINK@` every 250ms and parse output. Avoids the complexity of `pipewire-rs`. To be replaced in v0.3+ with proper PipeWire registry subscription.

`wpctl` output format:

```
$ wpctl get-volume @DEFAULT_AUDIO_SINK@
Volume: 0.65
$ wpctl get-volume @DEFAULT_AUDIO_SINK@
Volume: 0.65 [MUTED]
```

**Files:**
- Create: `crates/hytte-services/src/pipewire.rs`
- Modify: `crates/hytte-services/src/lib.rs`

- [ ] **Step 1: Implement `pipewire.rs`**

Create `crates/hytte-services/src/pipewire.rs`:

```rust
//! Default audio sink volume + mute state, polled via `wpctl`.
//!
//! v0.2.0 uses a 250 ms shell-out poll for simplicity. v0.3+ should
//! switch to a proper `pipewire-rs` registry subscription so updates
//! arrive event-driven.

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use std::process::Command;
use std::time::Duration;

pub struct PipewireService;

#[derive(Clone, Copy, Debug, Default)]
pub struct Volume {
    /// Linear volume, `0.0..=1.0` (may exceed 1.0 if user boosts above
    /// 100%). Untouched on parse failure.
    pub linear: f64,
    pub muted: bool,
}

#[doc(hidden)]
pub struct PipewireHandles {
    pub(crate) sink: Mutable<Volume>,
}

impl Default for PipewireHandles {
    fn default() -> Self {
        Self {
            sink: Mutable::new(Volume::default()),
        }
    }
}

impl Service for PipewireService {
    type Handles = PipewireHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = PipewireHandles::default();
        let writer = handles.sink.clone();

        rt.spawn(async move {
            let mut last = Volume::default();
            loop {
                if let Some(v) = poll() {
                    if v.linear != last.linear || v.muted != last.muted {
                        writer.set(v);
                        last = v;
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });

        handles
    }
}

fn poll() -> Option<Volume> {
    let out = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = std::str::from_utf8(&out.stdout).ok()?;
    parse(s)
}

fn parse(s: &str) -> Option<Volume> {
    // Expected: "Volume: 0.65 [MUTED]\n" or "Volume: 0.65\n"
    let trimmed = s.trim();
    let rest = trimmed.strip_prefix("Volume:")?.trim();
    let mut parts = rest.split_whitespace();
    let linear: f64 = parts.next()?.parse().ok()?;
    let muted = rest.contains("[MUTED]");
    Some(Volume { linear, muted })
}

#[must_use]
pub fn service() -> PipewireService {
    PipewireService
}

pub fn default_sink() -> impl Signal<Item = Volume> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .sink
            .signal_cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parse_unmuted() {
        let v = parse("Volume: 0.65\n").unwrap();
        assert!((v.linear - 0.65).abs() < 1e-9);
        assert!(!v.muted);
    }

    #[test]
    fn parse_muted() {
        let v = parse("Volume: 0.20 [MUTED]\n").unwrap();
        assert!((v.linear - 0.20).abs() < 1e-9);
        assert!(v.muted);
    }

    #[test]
    fn parse_garbage_returns_none() {
        assert!(parse("not wpctl output").is_none());
        assert!(parse("Volume: foo").is_none());
    }
}
```

- [ ] **Step 2: Update `lib.rs`**

Append:

```rust
pub mod pipewire;
```

- [ ] **Step 3: Build + test**

Run: `cargo test -p hytte-services pipewire`
Expected: 3 unit tests pass.

- [ ] **Step 4: Commit**

```sh
git add crates/hytte-services
git commit -m "feat(services): pipewire volume + mute via wpctl polling"
```

---

## Task 5: `hytte-services::networkd` — link state

DBus client to `org.freedesktop.network1.Manager`. Lists links, watches per-link `OperationalState`. Emits two signals: a primary-link state (best routable interface) and a full list.

**Files:**
- Create: `crates/hytte-services/src/networkd.rs`
- Modify: `crates/hytte-services/src/lib.rs`

- [ ] **Step 1: Implement `networkd.rs`**

Create `crates/hytte-services/src/networkd.rs`:

```rust
//! Link state from systemd-networkd (`org.freedesktop.network1`).
//!
//! Polls the Manager's `ListLinks` once at startup, then queries each
//! link's properties. Subscribes to `Manager.PropertiesChanged` for
//! refresh signals. (networkd does not emit per-link PropertiesChanged
//! universally; a periodic re-poll is the robust path.)

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use std::time::Duration;
use zbus::Connection;

pub struct NetworkdService;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationalState {
    Missing,
    Off,
    NoCarrier,
    Dormant,
    DegradedCarrier,
    Carrier,
    Degraded,
    EnslavedRouting,
    Routable,
    Unknown,
}

impl OperationalState {
    fn parse(s: &str) -> Self {
        match s {
            "missing" => Self::Missing,
            "off" => Self::Off,
            "no-carrier" => Self::NoCarrier,
            "dormant" => Self::Dormant,
            "degraded-carrier" => Self::DegradedCarrier,
            "carrier" => Self::Carrier,
            "degraded" => Self::Degraded,
            "enslaved" => Self::EnslavedRouting,
            "routable" => Self::Routable,
            _ => Self::Unknown,
        }
    }

    /// Coarse priority used to pick a "primary" link (highest wins).
    fn priority(self) -> u8 {
        match self {
            Self::Routable => 5,
            Self::Degraded => 4,
            Self::EnslavedRouting => 3,
            Self::Carrier | Self::DegradedCarrier => 2,
            Self::Dormant => 1,
            _ => 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Link {
    pub idx: i32,
    pub name: String,
    pub operational: OperationalState,
}

#[doc(hidden)]
pub struct NetworkdHandles {
    pub(crate) links: Mutable<Vec<Link>>,
    pub(crate) primary: Mutable<Option<Link>>,
}

impl Default for NetworkdHandles {
    fn default() -> Self {
        Self {
            links: Mutable::new(Vec::new()),
            primary: Mutable::new(None),
        }
    }
}

impl Service for NetworkdService {
    type Handles = NetworkdHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NetworkdHandles::default();
        let links_writer = handles.links.clone();
        let primary_writer = handles.primary.clone();

        rt.spawn(async move {
            loop {
                match listen(&links_writer, &primary_writer).await {
                    Ok(()) => tracing::warn!("networkd stream ended, retrying in 2s"),
                    Err(e) => tracing::warn!(error = %e, "networkd error, retrying in 2s"),
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        handles
    }
}

async fn listen(
    links_out: &Mutable<Vec<Link>>,
    primary_out: &Mutable<Option<Link>>,
) -> Result<()> {
    let conn = Connection::system().await.context("connect system bus")?;

    loop {
        let links = read_links(&conn).await?;
        let primary = links
            .iter()
            .max_by_key(|l| l.operational.priority())
            .filter(|l| l.operational.priority() > 0)
            .cloned();

        links_out.set(links);
        primary_out.set(primary);

        // Re-poll every 2 seconds. Cheap; networkd has no global property
        // change signal we can listen for portably.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn read_links(conn: &Connection) -> Result<Vec<Link>> {
    let manager = zbus::Proxy::new(
        conn,
        "org.freedesktop.network1",
        "/org/freedesktop/network1",
        "org.freedesktop.network1.Manager",
    )
    .await
    .context("create networkd Manager proxy")?;

    // ListLinks returns array of (idx: i32, name: String, path: ObjectPath).
    let list: Vec<(i32, String, zbus::zvariant::OwnedObjectPath)> =
        manager.call("ListLinks", &()).await.context("ListLinks")?;

    let mut out = Vec::with_capacity(list.len());
    for (idx, name, path) in list {
        let link_proxy = zbus::Proxy::new(
            conn,
            "org.freedesktop.network1",
            path.as_str(),
            "org.freedesktop.network1.Link",
        )
        .await
        .context("create Link proxy")?;

        let op_state: String = link_proxy
            .get_property("OperationalState")
            .await
            .unwrap_or_default();

        out.push(Link {
            idx,
            name,
            operational: OperationalState::parse(&op_state),
        });
    }
    Ok(out)
}

#[must_use]
pub fn service() -> NetworkdService {
    NetworkdService
}

pub fn links() -> impl Signal<Item = Vec<Link>> {
    registry::with(|r| {
        r.get::<NetworkdHandles>()
            .expect("networkd::service() not registered")
            .links
            .signal_cloned()
    })
}

pub fn primary() -> impl Signal<Item = Option<Link>> {
    registry::with(|r| {
        r.get::<NetworkdHandles>()
            .expect("networkd::service() not registered")
            .primary
            .signal_cloned()
    })
}
```

- [ ] **Step 2: Update `lib.rs`**

Append:

```rust
pub mod networkd;
```

- [ ] **Step 3: Build**

Run: `cargo check -p hytte-services`
Expected: clean.

- [ ] **Step 4: Commit**

```sh
git add crates/hytte-services
git commit -m "feat(services): networkd link state via DBus"
```

---

## Task 6: `hytte-services::resolved` — DNS state

Read the configured DNS servers from systemd-resolved. v0.2.0 just exposes whether resolution is configured + the server addresses; richer per-link state can come later.

**Files:**
- Create: `crates/hytte-services/src/resolved.rs`
- Modify: `crates/hytte-services/src/lib.rs`

- [ ] **Step 1: Implement `resolved.rs`**

Create `crates/hytte-services/src/resolved.rs`:

```rust
//! DNS state from systemd-resolved (`org.freedesktop.resolve1`).
//!
//! Reads the Manager's `DNS` property — a list of `(ifindex, family,
//! address)` tuples — every 2 seconds. Emits a `DnsState` summary.

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use std::net::IpAddr;
use std::time::Duration;
use zbus::Connection;

pub struct ResolvedService;

#[derive(Clone, Debug, Default)]
pub struct DnsState {
    pub servers: Vec<IpAddr>,
}

impl DnsState {
    #[must_use]
    pub fn configured(&self) -> bool {
        !self.servers.is_empty()
    }
}

#[doc(hidden)]
pub struct ResolvedHandles {
    pub(crate) dns: Mutable<DnsState>,
}

impl Default for ResolvedHandles {
    fn default() -> Self {
        Self {
            dns: Mutable::new(DnsState::default()),
        }
    }
}

impl Service for ResolvedService {
    type Handles = ResolvedHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = ResolvedHandles::default();
        let writer = handles.dns.clone();

        rt.spawn(async move {
            loop {
                match listen(&writer).await {
                    Ok(()) => tracing::warn!("resolved poll ended, retrying in 2s"),
                    Err(e) => tracing::warn!(error = %e, "resolved error, retrying in 2s"),
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        handles
    }
}

async fn listen(out: &Mutable<DnsState>) -> Result<()> {
    let conn = Connection::system().await.context("connect system bus")?;

    let proxy = zbus::Proxy::new(
        &conn,
        "org.freedesktop.resolve1",
        "/org/freedesktop/resolve1",
        "org.freedesktop.resolve1.Manager",
    )
    .await
    .context("create resolved Manager proxy")?;

    loop {
        // DNS = a(iiay) — array of (ifindex i32, family i32, address bytes).
        let raw: Vec<(i32, i32, Vec<u8>)> = proxy.get_property("DNS").await.unwrap_or_default();
        let mut servers: Vec<IpAddr> = Vec::with_capacity(raw.len());
        for (_idx, family, bytes) in raw {
            if let Some(ip) = parse_addr(family, &bytes) {
                servers.push(ip);
            }
        }
        servers.sort();
        servers.dedup();
        out.set(DnsState { servers });

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn parse_addr(family: i32, bytes: &[u8]) -> Option<IpAddr> {
    // AF_INET = 2, AF_INET6 = 10 on Linux.
    match (family, bytes.len()) {
        (2, 4) => Some(IpAddr::V4(std::net::Ipv4Addr::new(
            bytes[0], bytes[1], bytes[2], bytes[3],
        ))),
        (10, 16) => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            Some(IpAddr::V6(std::net::Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

#[must_use]
pub fn service() -> ResolvedService {
    ResolvedService
}

pub fn dns() -> impl Signal<Item = DnsState> {
    registry::with(|r| {
        r.get::<ResolvedHandles>()
            .expect("resolved::service() not registered")
            .dns
            .signal_cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::parse_addr;
    use std::net::IpAddr;

    #[test]
    fn parses_ipv4() {
        let ip = parse_addr(2, &[1, 1, 1, 1]).unwrap();
        assert_eq!(ip, IpAddr::V4(std::net::Ipv4Addr::new(1, 1, 1, 1)));
    }

    #[test]
    fn parses_ipv6() {
        let bytes = [0x20, 0x01, 0x48, 0x60, 0x48, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0x88, 0x88];
        let ip = parse_addr(10, &bytes).unwrap();
        assert_eq!(
            ip,
            IpAddr::V6("2001:4860:4860::8888".parse().unwrap())
        );
    }

    #[test]
    fn rejects_unknown_family() {
        assert!(parse_addr(99, &[1, 2, 3, 4]).is_none());
    }
}
```

- [ ] **Step 2: Update `lib.rs`**

Append:

```rust
pub mod resolved;
```

- [ ] **Step 3: Build + test**

Run: `cargo test -p hytte-services resolved`
Expected: 3 unit tests pass.

- [ ] **Step 4: Commit**

```sh
git add crates/hytte-services
git commit -m "feat(services): resolved DNS state via DBus"
```

---

## Task 7: trollshell battery widget + popup

Right-cluster icon showing the battery. On click, popup with percentage + time remaining + state.

**Files:**
- Create: `trollshell/src/widgets/battery.rs`
- Modify: `trollshell/src/widgets/mod.rs`

- [ ] **Step 1: Implement `battery.rs`**

Create `trollshell/src/widgets/battery.rs`:

```rust
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::upower::{self, Battery, BatteryState};

pub fn widget() -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-battery");

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    bind(
        upower::battery().map(|b| b.icon_name.clone()),
        &icon,
        |w, name| {
            if name.is_empty() {
                w.set_icon_name(Some("battery-missing-symbolic"));
            } else {
                w.set_icon_name(Some(&name));
            }
        },
    );

    let detail = detail_widget();
    let popup = Popup::new(&btn)
        .child(detail)
        .position(PopupPosition::Bottom)
        .css_class("ts-battery-popup")
        .build();

    btn.connect_clicked(move |_| popup.toggle());
    btn.upcast()
}

fn detail_widget() -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.add_css_class("ts-popup-column");

    let pct = gtk::Label::new(None);
    pct.set_xalign(0.0);
    pct.add_css_class("ts-popup-headline");
    bind_text(
        upower::battery().map(|b| format!("{:.0}%", b.percentage)),
        &pct,
    );
    column.append(&pct);

    let state_label = gtk::Label::new(None);
    state_label.set_xalign(0.0);
    bind_text(
        upower::battery().map(|b| describe(&b)),
        &state_label,
    );
    column.append(&state_label);

    column.upcast()
}

fn describe(b: &Battery) -> String {
    let state = match b.state {
        BatteryState::Charging => "Charging",
        BatteryState::Discharging => "Discharging",
        BatteryState::Empty => "Empty",
        BatteryState::FullyCharged => "Fully charged",
        BatteryState::PendingCharge => "Pending charge",
        BatteryState::PendingDischarge => "Pending discharge",
        BatteryState::Unknown => "Unknown",
    };
    let remaining = match b.state {
        BatteryState::Discharging => b.time_to_empty.map(|d| fmt_dur(d, "until empty")),
        BatteryState::Charging => b.time_to_full.map(|d| fmt_dur(d, "until full")),
        _ => None,
    };
    match remaining {
        Some(r) => format!("{state} — {r}"),
        None => state.to_string(),
    }
}

fn fmt_dur(d: std::time::Duration, suffix: &str) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    if h > 0 {
        format!("{h}h {m}m {suffix}")
    } else {
        format!("{m}m {suffix}")
    }
}
```

- [ ] **Step 2: Update `widgets/mod.rs`**

Replace `trollshell/src/widgets/mod.rs` with:

```rust
pub mod battery;
pub mod clock;
pub mod workspaces;
```

(`volume` and `network` modules are added in tasks 8 and 9.)

- [ ] **Step 3: Build**

Run: `cargo check -p trollshell`
Expected: clean.

- [ ] **Step 4: Commit**

```sh
git add trollshell
git commit -m "feat(trollshell): battery indicator + popup"
```

---

## Task 8: trollshell volume widget + popup

Icon shows volume level + mute. Popup shows percentage. (No slider yet — read-only.)

**Files:**
- Create: `trollshell/src/widgets/volume.rs`
- Modify: `trollshell/src/widgets/mod.rs`

- [ ] **Step 1: Implement `volume.rs`**

Create `trollshell/src/widgets/volume.rs`:

```rust
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::pipewire::{self, Volume};

pub fn widget() -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-volume");

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    bind(pipewire::default_sink(), &icon, |w, v| {
        w.set_icon_name(Some(icon_name(v)));
    });

    let detail = detail_widget();
    let popup = Popup::new(&btn)
        .child(detail)
        .position(PopupPosition::Bottom)
        .css_class("ts-volume-popup")
        .build();

    btn.connect_clicked(move |_| popup.toggle());
    btn.upcast()
}

fn icon_name(v: Volume) -> &'static str {
    if v.muted {
        "audio-volume-muted-symbolic"
    } else if v.linear < 0.34 {
        "audio-volume-low-symbolic"
    } else if v.linear < 0.67 {
        "audio-volume-medium-symbolic"
    } else {
        "audio-volume-high-symbolic"
    }
}

fn detail_widget() -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.add_css_class("ts-popup-column");

    let headline = gtk::Label::new(None);
    headline.set_xalign(0.0);
    headline.add_css_class("ts-popup-headline");
    bind_text(
        pipewire::default_sink().map(|v| {
            if v.muted {
                "Muted".to_string()
            } else {
                format!("{:.0}%", v.linear * 100.0)
            }
        }),
        &headline,
    );
    column.append(&headline);

    let device = gtk::Label::new(Some("Default sink"));
    device.set_xalign(0.0);
    column.append(&device);

    column.upcast()
}
```

- [ ] **Step 2: Update `widgets/mod.rs`**

Replace contents:

```rust
pub mod battery;
pub mod clock;
pub mod volume;
pub mod workspaces;
```

- [ ] **Step 3: Build**

Run: `cargo check -p trollshell`
Expected: clean.

- [ ] **Step 4: Commit**

```sh
git add trollshell
git commit -m "feat(trollshell): volume indicator + popup"
```

---

## Task 9: trollshell network widget + popup

Network indicator showing primary link's operational state. Popup combines per-link list (from networkd) with DNS server count (from resolved).

**Files:**
- Create: `trollshell/src/widgets/network.rs`
- Modify: `trollshell/src/widgets/mod.rs`

- [ ] **Step 1: Implement `network.rs`**

Create `trollshell/src/widgets/network.rs`:

```rust
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::networkd::{self, Link, OperationalState};
use hytte::services::resolved;

pub fn widget() -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-network");

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    bind(networkd::primary(), &icon, |w, primary| {
        w.set_icon_name(Some(icon_name(primary.as_ref())));
    });

    let detail = detail_widget();
    let popup = Popup::new(&btn)
        .child(detail)
        .position(PopupPosition::Bottom)
        .css_class("ts-network-popup")
        .build();

    btn.connect_clicked(move |_| popup.toggle());
    btn.upcast()
}

fn icon_name(primary: Option<&Link>) -> &'static str {
    match primary.map(|l| l.operational) {
        Some(OperationalState::Routable) => "network-wired-symbolic",
        Some(OperationalState::Degraded | OperationalState::DegradedCarrier) => {
            "network-wired-acquiring-symbolic"
        }
        Some(_) => "network-wired-no-route-symbolic",
        None => "network-wired-disconnected-symbolic",
    }
}

fn detail_widget() -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.add_css_class("ts-popup-column");

    let headline = gtk::Label::new(None);
    headline.set_xalign(0.0);
    headline.add_css_class("ts-popup-headline");
    bind_text(
        networkd::primary().map(|p| match p {
            Some(link) => format!("{}: {}", link.name, describe_state(link.operational)),
            None => "Disconnected".to_string(),
        }),
        &headline,
    );
    column.append(&headline);

    let links_label = gtk::Label::new(None);
    links_label.set_xalign(0.0);
    bind_text(
        networkd::links().map(|ls| {
            let lines: Vec<String> = ls
                .iter()
                .map(|l| format!("{} ({})", l.name, describe_state(l.operational)))
                .collect();
            lines.join("\n")
        }),
        &links_label,
    );
    column.append(&links_label);

    let dns = gtk::Label::new(None);
    dns.set_xalign(0.0);
    bind_text(
        resolved::dns().map(|state| {
            if state.configured() {
                format!("DNS: {} server(s)", state.servers.len())
            } else {
                "DNS: not configured".to_string()
            }
        }),
        &dns,
    );
    column.append(&dns);

    column.upcast()
}

fn describe_state(s: OperationalState) -> &'static str {
    match s {
        OperationalState::Routable => "routable",
        OperationalState::Degraded => "degraded",
        OperationalState::DegradedCarrier => "degraded carrier",
        OperationalState::Carrier => "carrier",
        OperationalState::EnslavedRouting => "enslaved",
        OperationalState::NoCarrier => "no carrier",
        OperationalState::Dormant => "dormant",
        OperationalState::Off => "off",
        OperationalState::Missing => "missing",
        OperationalState::Unknown => "unknown",
    }
}
```

- [ ] **Step 2: Update `widgets/mod.rs`**

Replace contents:

```rust
pub mod battery;
pub mod clock;
pub mod network;
pub mod volume;
pub mod workspaces;
```

- [ ] **Step 3: Build**

Run: `cargo check -p trollshell`
Expected: clean.

- [ ] **Step 4: Commit**

```sh
git add trollshell
git commit -m "feat(trollshell): network indicator + popup with links/DNS"
```

---

## Task 10: trollshell main.rs integration + style polish

Wire the four new services into `App`, enable keyboard interactivity on the bar so popovers work, add the right-cluster widgets, and ship some indicator/popup styling.

**Files:**
- Modify: `trollshell/src/main.rs`
- Modify: `trollshell/style.css`

- [ ] **Step 1: Update `main.rs`**

Replace `trollshell/src/main.rs` with:

```rust
mod widgets;

use hytte::prelude::*;
use hytte::services::{clock, networkd, niri, pipewire, resolved, upower};

fn main() -> hytte::ui::Result<()> {
    tracing_subscriber::fmt::init();

    App::new("cc.hannig.trollshell")
        .with(clock::service())
        .with(niri::service())
        .with(upower::service())
        .with(pipewire::service())
        .with(networkd::service())
        .with(resolved::service())
        .with_user_style(concat!(env!("CARGO_MANIFEST_DIR"), "/style.css"))
        .run(|app| {
            for monitor in app.monitors() {
                Bar::new(&monitor)
                    .edge(Edge::Top)
                    .exclusive(true)
                    .keyboard_interactivity(KeyboardMode::OnDemand)
                    .left([widgets::workspaces::widget(&monitor)])
                    .right([
                        widgets::network::widget(),
                        widgets::volume::widget(),
                        widgets::battery::widget(),
                        widgets::clock::widget(),
                    ])
                    .show()
                    .into_long_lived();
            }
        })
}
```

- [ ] **Step 2: Append indicator + popup styles**

Append to `trollshell/style.css`:

```css
/* Right-cluster indicator buttons */
.ts-indicator {
    padding: 0 6px;
    min-width: 22px;
}
.ts-indicator image {
    -gtk-icon-size: 16px;
}

/* Popup column layout */
.ts-popup-column {
    min-width: 180px;
}
.ts-popup-headline {
    font-weight: 600;
    font-size: 14px;
}
```

- [ ] **Step 3: Build release**

Run: `cargo build --release -p trollshell`
Expected: clean. Should show no clippy warnings either: `cargo clippy --workspace -- -D warnings`.

- [ ] **Step 4: Commit**

```sh
git add trollshell
git commit -m "feat(trollshell): right-cluster indicators + popup styling

Wires up upower/pipewire/networkd/resolved into the App and adds
network/volume/battery indicator widgets with click-to-popup details.
Bars now use KeyboardMode::OnDemand so popovers receive input."
```

- [ ] **Step 5: Manual smoke checklist on Niri**

(User performs these.)

- [ ] `cargo run --release -p trollshell` starts cleanly.
- [ ] Right cluster shows network → volume → battery → clock icons in that order.
- [ ] Battery icon reflects current charge / charging state; clicking pops a panel with `NN%` + state line.
- [ ] Volume icon changes when you `wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.3+`/`0.3-`/toggle mute; popup shows current `%` (or "Muted").
- [ ] Network icon shows wired-routable when online; popup lists each link with its operational state and a DNS server count.
- [ ] Clicking a popup-open indicator a second time closes the popup (`toggle`).
- [ ] Clicking outside a popup dismisses it (Popover autohide).
- [ ] No regressions in v0.1 functionality (clock still ticks, workspaces still update).

---

## Self-Review

**Spec coverage (vs design `2026-04-24-hytte-trollshell-design.md`, v0.2 row):**
- `Popup` primitive — Task 2.
- `Panel` primitive — **deliberately deferred** to v0.3+ (not needed for read-only indicators).
- `pipewire` (read) — Task 4 (wpctl polling; pipewire-rs in v0.3).
- `networkd` — Task 5.
- `resolved` — Task 6.
- `upower` — Task 3.
- `bluetooth` (read) — **deferred** to v0.2.1 (BlueZ is the largest API; deserves its own iteration).
- Trollshell right-cluster + popup-on-click — Tasks 7–10.

**Placeholder scan:** No "TBD"/"TODO"-as-acceptance. The pipewire-rs migration noted in Task 4's docstring is a forward-looking comment, not a deferred requirement.

**Type consistency:**
- `BatteryState`, `Battery`, `Volume`, `OperationalState`, `Link`, `DnsState` referenced consistently across services and widget tasks.
- `Popup`, `PopupPosition`, `KeyboardMode` exported through prelude (Task 2 step 5) and used in tasks 7-10.
- `service()` constructor + free signal-returning fn pattern matches v0.1 services.

**Risks / unknowns:**
- `wpctl` polling interval (250ms) is a feel choice — bump to 500ms if CPU-noisy.
- networkd has no global PropertiesChanged for link state — we re-poll every 2s. May feel sluggish; v0.3 should explore `PropertiesChanged` per-link signal subscription.
- UPower `DisplayDevice` may not exist on systems with no battery (desktops). The service falls back to default state silently; trollshell's battery widget will show `battery-missing-symbolic`. Document in v0.2.1 README.
- `gtk::Popover` with layer-shell + `KeyboardMode::OnDemand`: in some niri/wayland configs may not steal focus correctly. Surface as a v0.2.1 issue if it manifests.
