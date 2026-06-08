//! Display backlight level, read from `/sys/class/backlight/*` and written
//! via logind's `SetBrightness` (so no root / polkit prompt for the user).
//!
//! The service polls the active backlight device's `brightness` sysfs file
//! every 1 s so external changes (brightnessctl, laptop Fn keys, etc.) stay
//! in sync. Writes go through `org.freedesktop.login1.Session.SetBrightness`.

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry, runtime};
use std::sync::OnceLock;
use std::time::Duration;

/// Active backlight state.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Brightness {
    /// Current backlight level, `0.0..=1.0`.
    pub level: f64,
}

/// Sysfs info needed for `SetBrightness` calls: the subsystem (`"backlight"`)
/// and the device name (`"intel_backlight"`, `"amdgpu_bl1"`, …).
#[derive(Clone, Debug)]
struct Device {
    name: String,
    max: u32,
}

static DEVICE: OnceLock<Device> = OnceLock::new();

pub struct BrightnessService;

#[doc(hidden)]
pub struct BrightnessHandles {
    pub(crate) current: Mutable<Option<Brightness>>,
}

impl Default for BrightnessHandles {
    fn default() -> Self {
        Self {
            current: Mutable::new(None),
        }
    }
}

impl Service for BrightnessService {
    type Handles = BrightnessHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = BrightnessHandles::default();
        let writer = handles.current.clone();

        rt.spawn(async move {
            poll_loop(writer).await;
        });

        handles
    }
}

#[must_use]
pub fn service() -> BrightnessService {
    BrightnessService
}

pub fn current() -> impl Signal<Item = Option<Brightness>> {
    registry::with(|r| {
        r.get::<BrightnessHandles>()
            .expect("brightness::service() not registered")
            .current
            .signal()
    })
}

/// Set the backlight level (`0.0..=1.0`). Fire-and-forget.
pub fn set(level: f64) {
    let level = level.clamp(0.0, 1.0);
    runtime::handle().spawn(async move {
        if let Err(e) = do_set(level).await {
            tracing::warn!(error = %e, "brightness set failed");
        }
    });
}

async fn do_set(level: f64) -> Result<()> {
    let device = DEVICE
        .get()
        .context("no backlight device discovered yet")?
        .clone();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let value = (level * f64::from(device.max)).round() as u32;

    hytte_bus::call("org.freedesktop.login1")
        .bus(hytte_bus::BusKind::System)
        .at_path("/org/freedesktop/login1/session/auto")
        .iface("org.freedesktop.login1.Session")
        .method("SetBrightness")
        .args(("backlight", device.name, value))
        .send::<()>()
        .await
        .context("logind SetBrightness")?;
    Ok(())
}

async fn poll_loop(writer: Mutable<Option<Brightness>>) {
    // Last-snapshot dedupe so identical readings don't re-emit at 1 Hz to
    // every consumer (OSD, power-page slider, brightness chip). Mirrors the
    // pipewire service's gated-emit pattern.
    let mut last: Option<Brightness> = None;
    loop {
        let cur = read_state();
        if cur != last {
            writer.set(cur);
            last = cur;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn read_state() -> Option<Brightness> {
    let (name, current_raw, max) = pick_device()?;

    // Cache device identity for future writes.
    let _ = DEVICE.set(Device { name, max });

    if max == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let level = f64::from(current_raw) / f64::from(max);
    Some(Brightness {
        level: level.clamp(0.0, 1.0),
    })
}

/// Walks `/sys/class/backlight/` and returns the first device with a readable
/// `brightness` and `max_brightness`. Returns `(device_name, current, max)`.
fn pick_device() -> Option<(String, u32, u32)> {
    let dir = std::fs::read_dir("/sys/class/backlight").ok()?;
    for entry in dir.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(current) = std::fs::read_to_string(path.join("brightness"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        else {
            continue;
        };
        let Some(max) = std::fs::read_to_string(path.join("max_brightness"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        else {
            continue;
        };
        return Some((name, current, max));
    }
    None
}
