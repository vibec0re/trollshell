//! Display backlight level, read from `/sys/class/backlight/*` and written
//! via logind's `SetBrightness` (so no root / polkit prompt for the user).
//!
//! The service polls the active backlight device's `brightness` sysfs file
//! every 1 s so external changes (brightnessctl, laptop Fn keys, etc.) stay
//! in sync. Writes go through `org.freedesktop.login1.Session.SetBrightness`.
//!
//! # Writes
//!
//! [`set`] is fire-and-forget, but it is not one task per call: every request
//! lands in one `watch` slot and a single writer task drains it, so a slider
//! drag that fires `value-changed` dozens of times a second becomes at most
//! one `SetBrightness` in flight plus the latest value waiting behind it.
//! Writes therefore land in the order they were asked for (independent
//! spawned tasks used to race each other to logind, so an older value could
//! land last), and nothing piles up behind a slow logind.
//!
//! Every write is floored at [`MIN_LEVEL`], in the service rather than in
//! each caller, because a raw `0` turns the panel off on some hardware.

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, gated_poll, registry, spawn_supervised};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::watch;

/// The lowest level [`set`] will write, as a fraction of `max_brightness`.
///
/// On some panels a raw `0` is "backlight off", not "dimmest", which makes a
/// chip scrolled one notch too far indistinguishable from a dead screen. The
/// power drawer's slider always stopped here (its range starts at 0.05); the
/// chip's scroll clamped to `0.0`. One floor, applied to every write, means no
/// caller can get it wrong — and the raw value is floored at `1` as well,
/// since on a coarse backlight (`max_brightness` 7, say) even this fraction
/// rounds to `0`.
pub const MIN_LEVEL: f64 = 0.05;

/// Clamp a requested level into what [`set`] writes: `MIN_LEVEL..=1.0`.
///
/// Public so a caller that keeps its own running target (the chip's scroll)
/// accumulates from the value that was actually written, not from one below
/// the floor. `NaN` comes back as `NaN`; [`set`] ignores it.
#[must_use]
pub fn clamp_level(level: f64) -> f64 {
    level.clamp(MIN_LEVEL, 1.0)
}

/// Active backlight state.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Brightness {
    /// Current backlight level, `0.0..=1.0`.
    pub level: f64,
}

/// Sysfs info needed for `SetBrightness` calls: the subsystem (`"backlight"`)
/// and the device name (`"intel_backlight"`, `"amdgpu_bl1"`, …).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Device {
    name: String,
    max: u32,
}

/// The device writes target, refreshed by the poll loop whenever the picked
/// backlight changes. Previously a write-once `OnceLock`, which pinned the very
/// first-seen device for the whole session — so a backlight reappearing under a
/// different name (driver reload, dock) silently got stale writes (#434). A
/// process-global `Mutable` (like `geoclue`'s shared handle) lets the 1 Hz poll
/// update it, and `do_set` read it, from the tokio runtime without a registry.
static DEVICE: OnceLock<Mutable<Option<Device>>> = OnceLock::new();

fn device_handle() -> &'static Mutable<Option<Device>> {
    DEVICE.get_or_init(|| Mutable::new(None))
}

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

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = BrightnessHandles::default();
        let writer = handles.current.clone();

        spawn_supervised("brightness", move || {
            let writer = writer.clone();
            async move {
                poll_loop(writer).await;
            }
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

/// The writer's inbox: the latest requested level, or `None` before the first
/// request. Created, with its writer task, by the first [`set`].
static WRITES: OnceLock<watch::Sender<Option<f64>>> = OnceLock::new();

/// Set the backlight level, clamped to [`MIN_LEVEL`]`..=1.0`. Fire-and-forget,
/// and coalescing: a request made while an earlier one is still on its way to
/// logind replaces whatever is waiting, so only the latest level is written
/// next (see the module doc). A `NaN` is dropped.
pub fn set(level: f64) {
    if level.is_nan() {
        return;
    }
    let level = clamp_level(level);
    let writes = WRITES.get_or_init(|| {
        let (tx, rx) = watch::channel(None);
        // Supervised, so a panic cannot leave every later `set` landing in a
        // slot nobody reads. A restart's fresh receiver clone has not seen the
        // current value, so it re-writes it once — harmless, it is the level
        // the user last asked for.
        spawn_supervised("brightness-writer", move || {
            let rx = rx.clone();
            async move {
                run_writer(rx, |level| async move {
                    if let Err(e) = do_set(level).await {
                        tracing::warn!(error = %e, "brightness set failed");
                    }
                })
                .await;
            }
        });
        tx
    });
    writes.send_replace(Some(level));
}

/// Drain `rx`, handing each value to `write` and awaiting it before reading
/// the next. Values sent while a write is in flight collapse into the latest
/// one (`watch` keeps only that), which is the coalescing [`set`] promises.
/// Returns when the sender is dropped — never, for the real `static` one.
///
/// `write` is injected so the coalescing is testable without logind.
async fn run_writer<W, Fut>(mut rx: watch::Receiver<Option<f64>>, mut write: W)
where
    W: FnMut(f64) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    while rx.changed().await.is_ok() {
        // Copy out and drop the borrow before awaiting: holding a `watch`
        // read guard across the write would block every `set` behind it.
        let next = *rx.borrow_and_update();
        if let Some(level) = next {
            write(level).await;
        }
    }
}

/// The raw `SetBrightness` value for `level` on a backlight whose
/// `max_brightness` is `max`, never below `1` — see [`MIN_LEVEL`] for why the
/// floor applies to the raw value too. `None` for a `max` of `0`, which has no
/// level to write.
fn raw_value(level: f64, max: u32) -> Option<u32> {
    if max == 0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let value = (clamp_level(level) * f64::from(max)).round() as u32;
    Some(value.clamp(1, max))
}

async fn do_set(level: f64) -> Result<()> {
    let device = device_handle()
        .get_cloned()
        .context("no backlight device discovered yet")?;
    let value = raw_value(level, device.max)
        .with_context(|| format!("backlight {} reports max_brightness 0", device.name))?;

    hytte_bus::call(hytte_bus::BusKind::System, "org.freedesktop.login1")
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
    // Dedupe so identical readings don't re-emit at 1 Hz to every consumer
    // (OSD, power-page slider, brightness chip) — now `gated_poll`'s
    // reference-compare against `writer` itself (#1172) rather than a
    // hand-kept `last` local, which tracked the exact same value. No
    // drawer-visibility concept to park on, so the gate is a permanently
    // `true` `Mutable<bool>` purely to reuse the scaffolding; the park
    // branch never engages.
    let always_active = Mutable::new(true);
    gated_poll(
        always_active,
        || Duration::from_secs(1),
        writer,
        || async {
            // `read_state` is synchronous `std::fs` I/O (a sysfs directory walk
            // plus two file reads per candidate device); run it on tokio's
            // blocking pool rather than the async worker thread it used to
            // block (#1171) — the same fix `app_usage`/`sensors` already apply
            // to their own sysfs/procfs walks (see #434's app_usage fix for the
            // precedent). Cadence and values are unchanged: still one read per
            // second. The outer `Some` is unconditional (mirrors the pre-#1172
            // loop, which always considered writing) — the *inner* `Option`,
            // Some(device) vs None(no backlight), is the real value `gated_poll`
            // dedupes against.
            Some(
                tokio::task::spawn_blocking(read_state)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "brightness: sysfs read task panicked");
                        None
                    }),
            )
        },
    )
    .await;
}

fn read_state() -> Option<Brightness> {
    let (name, current_raw, max) = pick_device()?;
    let device = Device { name, max };

    // Refresh the write-target device only when it actually changed — so a
    // backlight that reappears under a new name is picked up, without cloning
    // (and dropping) a fresh `Device` every 1 Hz tick.
    let handle = device_handle();
    let changed = {
        let guard = handle.lock_ref();
        guard.as_ref() != Some(&device)
    };
    if changed {
        handle.set(Some(device.clone()));
    }

    if device.max == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let level = f64::from(current_raw) / f64::from(device.max);
    Some(Brightness {
        level: level.clamp(0.0, 1.0),
    })
}

/// Where the kernel exposes backlight class devices.
const BACKLIGHT_ROOT: &str = "/sys/class/backlight";

/// [`pick_device_in`] over the real sysfs root.
fn pick_device() -> Option<(String, u32, u32)> {
    pick_device_in(Path::new(BACKLIGHT_ROOT))
}

/// Preference rank of a backlight's `type` attribute: lower wins.
///
/// The kernel's own ordering (`Documentation/ABI/stable/sysfs-class-backlight`,
/// the `type` entry): "when multiple backlight interfaces are available for a
/// single device, firmware control should be preferred to platform control
/// should be preferred to raw control", because a firmware interface is the one
/// least likely to fight the hardware over the level. systemd-backlight and
/// GNOME's backlight helper read it the same way. A missing or unrecognised
/// `type` sorts last.
fn type_rank(kind: Option<&str>) -> u8 {
    match kind {
        Some("firmware") => 0,
        Some("platform") => 1,
        Some("raw") => 2,
        _ => 3,
    }
}

/// Read a sysfs attribute as a trimmed `u32`.
fn read_u32(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Pick the backlight device under `root` (a `/sys/class/backlight`-shaped
/// directory): among the devices with a readable `brightness` and
/// `max_brightness`, the best [`type_rank`], ties broken by name. Returns
/// `(device_name, current, max)`.
///
/// Until this took a ranking it returned the first readable entry in
/// `read_dir` order — which the kernel does not specify — so a machine with
/// two backlights (an `acpi_video0` beside an `intel_backlight`, or a
/// hybrid-GPU laptop's pair) could poll and write a different device from one
/// boot to the next. `root` is a parameter so the choice is testable on a
/// scratch tree.
fn pick_device_in(root: &Path) -> Option<(String, u32, u32)> {
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let current = read_u32(&path.join("brightness"))?;
            let max = read_u32(&path.join("max_brightness"))?;
            let kind = std::fs::read_to_string(path.join("type")).ok();
            let rank = type_rank(kind.as_deref().map(str::trim));
            Some((rank, name, current, max))
        })
        .min_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)))
        .map(|(_, name, current, max)| (name, current, max))
}

#[cfg(test)]
mod tests {
    use super::{MIN_LEVEL, clamp_level, pick_device_in, raw_value, run_writer};
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use tokio::sync::{Notify, watch};

    // ── The floor ───────────────────────────────────────────────────────────

    #[test]
    fn every_level_is_floored_and_capped() {
        assert!((clamp_level(0.0) - MIN_LEVEL).abs() < f64::EPSILON);
        assert!((clamp_level(-3.0) - MIN_LEVEL).abs() < f64::EPSILON);
        assert!((clamp_level(0.4) - 0.4).abs() < f64::EPSILON);
        assert!((clamp_level(7.0) - 1.0).abs() < f64::EPSILON);
    }

    /// A raw `0` is "off" on some panels; no level may produce one, even on a
    /// backlight coarse enough that `MIN_LEVEL` itself rounds down to it.
    #[test]
    fn the_raw_value_never_reaches_zero() {
        assert_eq!(raw_value(0.0, 255), Some(13)); // 0.05 * 255 = 12.75
        assert_eq!(raw_value(0.0, 7), Some(1)); // 0.05 * 7 = 0.35 would round to 0
        assert_eq!(raw_value(1.0, 7), Some(7));
        assert_eq!(raw_value(0.5, 96_000), Some(48_000));
        assert_eq!(raw_value(0.5, 0), None);
    }

    // ── Coalescing ──────────────────────────────────────────────────────────

    /// Wait (bounded) until the writer has recorded `n` writes.
    async fn until_written(written: &Mutex<Vec<f64>>, n: usize) {
        for _ in 0..100 {
            if written.lock().unwrap().len() >= n {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "writer never reached {n} writes: {:?}",
            written.lock().unwrap()
        );
    }

    /// Three requests while a write is in flight become one write of the
    /// latest, issued only after the first one finished.
    ///
    /// Falsifiable: a writer that spawns per value (what `set` did before)
    /// writes all four, and one that does not await `write` before reading
    /// the next value writes the second before the first is done.
    #[tokio::test(flavor = "current_thread")]
    async fn requests_during_an_in_flight_write_collapse_to_the_latest() {
        let (tx, rx) = watch::channel(None);
        let written: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Notify::new());

        let task = {
            let written = written.clone();
            let gate = gate.clone();
            tokio::spawn(run_writer(rx, move |level| {
                written.lock().unwrap().push(level);
                let gate = gate.clone();
                async move { gate.notified().await }
            }))
        };

        tx.send_replace(Some(0.1));
        until_written(&written, 1).await;
        // The first write is now blocked on the gate (a slow logind).
        tx.send_replace(Some(0.2));
        tx.send_replace(Some(0.3));
        tx.send_replace(Some(0.4));
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            *written.lock().unwrap(),
            vec![0.1],
            "nothing may be written while the first write is in flight"
        );

        gate.notify_one();
        until_written(&written, 2).await;
        gate.notify_one();
        drop(tx);
        task.await.expect("writer panicked");

        assert_eq!(*written.lock().unwrap(), vec![0.1, 0.4]);
    }

    // ── Device choice ───────────────────────────────────────────────────────

    fn device(root: &Path, name: &str, kind: Option<&str>, current: u32, max: u32) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("brightness"), format!("{current}\n")).unwrap();
        std::fs::write(dir.join("max_brightness"), format!("{max}\n")).unwrap();
        if let Some(kind) = kind {
            std::fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
        }
    }

    #[test]
    fn firmware_beats_platform_beats_raw_whatever_the_names() {
        let root = tempfile::tempdir().unwrap();
        device(root.path(), "a_raw", Some("raw"), 1, 10);
        device(root.path(), "b_platform", Some("platform"), 2, 10);
        device(root.path(), "c_firmware", Some("firmware"), 3, 10);
        assert_eq!(
            pick_device_in(root.path()),
            Some(("c_firmware".to_string(), 3, 10))
        );

        std::fs::remove_dir_all(root.path().join("c_firmware")).unwrap();
        assert_eq!(
            pick_device_in(root.path()),
            Some(("b_platform".to_string(), 2, 10))
        );
    }

    /// Same rank: the name decides, so the pick no longer depends on
    /// `read_dir`'s unspecified order. A missing `type` ranks below `raw`.
    #[test]
    fn ties_break_by_name_and_an_untyped_device_ranks_last() {
        let root = tempfile::tempdir().unwrap();
        device(root.path(), "intel_backlight", Some("raw"), 5, 100);
        device(root.path(), "amdgpu_bl1", Some("raw"), 7, 255);
        device(root.path(), "aaa_untyped", None, 1, 1);
        assert_eq!(
            pick_device_in(root.path()),
            Some(("amdgpu_bl1".to_string(), 7, 255))
        );
    }

    /// An unreadable device is skipped rather than chosen, and an empty or
    /// absent root is no device at all.
    #[test]
    fn unreadable_devices_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        device(root.path(), "good", Some("raw"), 4, 8);
        let broken = root.path().join("broken_firmware");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("type"), "firmware\n").unwrap();
        std::fs::write(broken.join("brightness"), "not a number\n").unwrap();
        std::fs::write(broken.join("max_brightness"), "10\n").unwrap();
        assert_eq!(
            pick_device_in(root.path()),
            Some(("good".to_string(), 4, 8))
        );

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(pick_device_in(empty.path()), None);
        assert_eq!(pick_device_in(&empty.path().join("absent")), None);
    }
}
