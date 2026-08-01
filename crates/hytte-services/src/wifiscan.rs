//! Visible Wi-Fi networks from `NetworkManager` (system D-Bus).
//!
//! Enumerates the SSIDs `NetworkManager` currently sees (with signal strength)
//! purely for location fingerprinting (see [`crate::places`]). This is **not**
//! a Wi-Fi manager (that's [`crate::wifi`], an iwd client); it only reads the
//! scan list, so it stays a thin read-only sensor.
//!
//! We key on **SSID, not BSSID**: a place is recognised by the *set* of
//! network names visible there (your own plus the neighbours). That survives
//! router swaps — a replacement AP keeps the SSID even though its BSSID
//! changes — and the neighbours discriminate between places even when your own
//! SSID is deployed everywhere.
//!
//! `NetworkManager` scans on its own cadence; we re-read every
//! [`SCAN_INTERVAL`] on AC power, stretched to [`BATTERY_SCAN_INTERVAL`] (3x)
//! on battery (#505) — the visible set only changes when you physically move,
//! so a leisurely poll is plenty either way. The wait is checked in
//! [`RECHECK`]-sized steps rather than one fixed `tokio::time::interval`, so a
//! power-state flip mid-wait is honoured within a few seconds instead of only
//! on the next cycle.
//!
//! Published via [`current`] (registry signal, GTK thread) and [`shared_aps`]
//! (a process-global clone) so the `places` resolver's tokio task can read it
//! without touching the thread-local registry — mirroring geoclue.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Duration;

use futures_signals::signal::{Mutable, Signal};
use hytte_bus::{BusKind, call};
use hytte_reactive::{Service, registry, runtime, shared, spawn_supervised};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const NM_NAME: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const WIRELESS_IFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const AP_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Re-read cadence on AC power. Location changes at building scale, so this
/// is leisurely.
const SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// Re-read cadence on battery power: 3x AC (#505).
const BATTERY_SCAN_INTERVAL: Duration = Duration::from_secs(90);

/// How often [`wait_cadence`] re-checks the target cadence against elapsed
/// wait time. Small relative to [`SCAN_INTERVAL`] so a battery-state flip
/// mid-wait is honoured promptly, without turning this into a busy poll (each
/// recheck is a single in-memory read, no D-Bus I/O).
const RECHECK: Duration = Duration::from_secs(5);

/// Battery-aware re-scan cadence: [`BATTERY_SCAN_INTERVAL`] while on battery
/// power, else [`SCAN_INTERVAL`]. Pure so the on-battery → interval mapping is
/// unit-testable.
fn cadence(on_battery: bool) -> Duration {
    if on_battery {
        BATTERY_SCAN_INTERVAL
    } else {
        SCAN_INTERVAL
    }
}

/// Best-effort on-battery snapshot — see
/// [`crate::upower::on_battery_snapshot`] for the full rationale: it reads
/// upower's cross-thread `shared` bag (this scan loop runs on a tokio worker,
/// where the thread-local registry is empty), degrades to "assume AC" until
/// upower registers — `main.rs` starts `wifiscan` first — and never panics
/// the way `upower::on_battery()` would (#505).
fn on_battery() -> bool {
    crate::upower::on_battery_snapshot()
}

/// Wait out the current battery-aware cadence, re-checking every [`RECHECK`]
/// so a mid-wait power-state flip shortens or lengthens the remaining wait
/// instead of only taking effect on the next cycle.
async fn wait_cadence() {
    let mut waited = Duration::ZERO;
    loop {
        let target = cadence(on_battery());
        if waited >= target {
            return;
        }
        let step = RECHECK.min(target.saturating_sub(waited));
        tokio::time::sleep(step).await;
        waited += step;
    }
}

/// One visible network: its SSID and the strongest signal it's been seen at. A
/// mesh broadcasting one SSID from several APs collapses to a single entry —
/// we key on the name, not the per-radio BSSID, so the fingerprint survives
/// hardware swaps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessPoint {
    pub ssid: String,
    /// Signal strength, 0..=100 (strongest sighting of this SSID).
    pub strength: u8,
}

#[doc(hidden)]
#[derive(Default)]
pub struct WifiScanHandles {
    pub(crate) aps: Mutable<Vec<AccessPoint>>,
}

// Cross-thread shared handle: `registry` is GTK-thread-only, so the `places`
// resolver's tokio task reads visible networks from here instead.
struct Shared {
    aps: Mutable<Vec<AccessPoint>>,
}

pub struct WifiScanService;

impl Service for WifiScanService {
    type Handles = WifiScanHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = WifiScanHandles::default();
        let aps = handles.aps.clone();
        shared::insert(Shared { aps: aps.clone() });
        spawn_supervised("wifiscan", move || scan_loop(aps.clone()));
        handles
    }
}

#[must_use]
pub fn service() -> WifiScanService {
    WifiScanService
}

/// Signal of the currently-visible networks. First emission is the empty list
/// (before the first scan read lands).
pub fn current() -> impl Signal<Item = Vec<AccessPoint>> {
    registry::with(|r| {
        r.get::<WifiScanHandles>()
            .expect("wifiscan::service() not registered")
            .aps
            .signal_cloned()
    })
}

/// Process-global clone of the visible-network handle, for tokio-side readers
/// (the `places` resolver). `None` until [`service`] has started.
#[must_use]
pub fn shared_aps() -> Option<Mutable<Vec<AccessPoint>>> {
    shared::get::<Shared>().map(|s| s.aps.clone())
}

/// One-shot blocking scan for the `--scan-aps` CLI: forces a fresh scan, waits
/// briefly, and returns the visible networks. Drives the process runtime, so it
/// must be called from a non-async context (e.g. `main` before the App).
#[must_use]
pub fn scan_aps_blocking() -> Vec<AccessPoint> {
    runtime::handle().block_on(async { collect_aps(true).await.unwrap_or_default() })
}

/// Render visible networks as a paste-ready `ssids = [...]` TOML block for the
/// `--scan-aps` capture tool. Strongest first; signal as a comment.
#[must_use]
pub fn format_scan_block(aps: &[AccessPoint]) -> String {
    let mut sorted: Vec<&AccessPoint> = aps.iter().collect();
    sorted.sort_by(|a, b| {
        b.strength
            .cmp(&a.strength)
            .then_with(|| a.ssid.cmp(&b.ssid))
    });

    let mut out = String::new();
    out.push_str("# Visible networks (strongest first). Paste the ones you reliably see\n");
    out.push_str("# HERE but not at your other places (often neighbours) into a [[place]]\n");
    out.push_str("# in ~/.config/trollshell/places.toml:\n");
    out.push_str("ssids = [\n");
    for ap in sorted {
        // {:?} quotes + escapes, so names with spaces/quotes paste as valid TOML.
        let _ = writeln!(out, "  {:?},  # {}%", ap.ssid, ap.strength);
    }
    out.push_str("]\n");
    out
}

async fn scan_loop(aps: Mutable<Vec<AccessPoint>>) {
    // First iteration scans immediately (mirrors `tokio::time::interval`'s
    // instant first tick — the loop used before #505), then waits the
    // battery-aware cadence between reads. See `wait_cadence`.
    loop {
        match collect_aps(false).await {
            Ok(list) => {
                if aps.get_cloned() != list {
                    aps.set(list);
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "wifiscan: read failed (NetworkManager absent?)");
            }
        }
        wait_cadence().await;
    }
}

/// Read every visible network across all wireless devices, de-duplicated by
/// SSID (keeping the strongest signal). When `force_scan`, nudges a fresh
/// `RequestScan` first and waits briefly for it to land (the one-shot CLI; the
/// poll loop relies on NM's own periodic scans).
async fn collect_aps(force_scan: bool) -> Result<Vec<AccessPoint>, hytte_bus::BusError> {
    let devices = get_devices().await?;

    if force_scan {
        for dev in &devices {
            // Best-effort: errors on non-wifi devices or when rate-limited.
            let _ = request_scan(dev).await;
        }
        // RequestScan is async and a full multi-band scan takes several
        // seconds; wait long enough that the one-shot --scan-aps capture sees a
        // mostly-complete list (latency is irrelevant for a CLI).
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // SSID → strongest strength. A mesh (one SSID, many APs) collapses here.
    let mut by_ssid: HashMap<String, u8> = HashMap::new();
    for dev in &devices {
        // GetAllAccessPoints only exists on wireless devices; a non-wifi device
        // errors here and is skipped — no DeviceType check needed.
        let Ok(ap_paths) = get_all_access_points(dev).await else {
            continue;
        };
        for ap in ap_paths {
            if let Some(sighting) = read_ap(&ap).await {
                let entry = by_ssid.entry(sighting.ssid).or_insert(0);
                *entry = (*entry).max(sighting.strength);
            }
        }
    }
    Ok(by_ssid
        .into_iter()
        .map(|(ssid, strength)| AccessPoint { ssid, strength })
        .collect())
}

async fn get_devices() -> Result<Vec<OwnedObjectPath>, hytte_bus::BusError> {
    call(BusKind::System, NM_NAME)
        .at_path(NM_PATH)
        .iface(NM_IFACE)
        .method("GetDevices")
        .args(())
        .send::<Vec<OwnedObjectPath>>()
        .await
}

async fn get_all_access_points(
    device: &OwnedObjectPath,
) -> Result<Vec<OwnedObjectPath>, hytte_bus::BusError> {
    call(BusKind::System, NM_NAME)
        .at_path(device.as_str().to_string())
        .iface(WIRELESS_IFACE)
        .method("GetAllAccessPoints")
        .args(())
        .send::<Vec<OwnedObjectPath>>()
        .await
}

async fn request_scan(device: &OwnedObjectPath) -> Result<(), hytte_bus::BusError> {
    let options: HashMap<String, zbus::zvariant::Value<'static>> = HashMap::new();
    call(BusKind::System, NM_NAME)
        .at_path(device.as_str().to_string())
        .iface(WIRELESS_IFACE)
        .method("RequestScan")
        .args((options,))
        .send::<()>()
        .await
}

async fn read_ap(ap_path: &OwnedObjectPath) -> Option<AccessPoint> {
    let props: HashMap<String, OwnedValue> = call(BusKind::System, NM_NAME)
        .at_path(ap_path.as_str().to_string())
        .iface(PROPS_IFACE)
        .method("GetAll")
        .args((AP_IFACE,))
        .send::<HashMap<String, OwnedValue>>()
        .await
        .ok()?;
    ap_from_props(&props)
}

/// Pure extraction of an [`AccessPoint`] from NM's `AccessPoint` property map.
/// `Ssid` (`ay`) is decoded lossily and is required: a hidden network (empty
/// SSID) can't anchor a fingerprint, so it's dropped.
fn ap_from_props(props: &HashMap<String, OwnedValue>) -> Option<AccessPoint> {
    let ssid = prop_bytes(props, "Ssid")
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let ssid = ssid.trim().to_string();
    if ssid.is_empty() {
        return None;
    }
    let strength = property::<u8>(props, "Strength").unwrap_or(0);
    Some(AccessPoint { ssid, strength })
}

fn property<T>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| T::try_from(v).ok())
}

fn prop_bytes(props: &HashMap<String, OwnedValue>, key: &str) -> Option<Vec<u8>> {
    let v = props.get(key)?.try_clone().ok()?;
    <Vec<u8>>::try_from(v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn owned(v: Value<'_>) -> OwnedValue {
        OwnedValue::try_from(v).expect("ownable")
    }

    fn props(ssid: &[u8], strength: u8) -> HashMap<String, OwnedValue> {
        let mut p = HashMap::new();
        p.insert("Ssid".to_string(), owned(Value::from(ssid.to_vec())));
        p.insert("Strength".to_string(), owned(Value::from(strength)));
        p
    }

    // ── Battery-aware cadence (#505) ─────────────────────────────────────────

    #[test]
    fn cadence_is_scan_interval_on_ac() {
        assert_eq!(cadence(false), SCAN_INTERVAL);
    }

    #[test]
    fn cadence_stretches_on_battery() {
        assert_eq!(cadence(true), BATTERY_SCAN_INTERVAL);
        assert!(BATTERY_SCAN_INTERVAL > SCAN_INTERVAL);
    }

    #[test]
    fn ap_from_props_extracts_ssid_and_strength() {
        let ap = ap_from_props(&props(b"FRITZ!Box Annika", 87)).expect("parses");
        assert_eq!(ap.ssid, "FRITZ!Box Annika");
        assert_eq!(ap.strength, 87);
    }

    #[test]
    fn ap_from_props_hidden_or_blank_ssid_is_dropped() {
        assert!(ap_from_props(&props(b"", 40)).is_none());
        assert!(ap_from_props(&props(b"   ", 40)).is_none());
    }

    #[test]
    fn ap_from_props_non_utf8_ssid_is_lossy_not_fatal() {
        // 0xFF 0xFE isn't valid UTF-8 — must not panic, decodes lossily.
        let ap = ap_from_props(&props(&[0xff, 0xfe], 10)).expect("parses");
        assert!(!ap.ssid.is_empty());
    }

    #[test]
    fn format_scan_block_sorts_by_strength_desc() {
        let aps = vec![
            AccessPoint {
                ssid: "Weak".into(),
                strength: 20,
            },
            AccessPoint {
                ssid: "Strong".into(),
                strength: 90,
            },
        ];
        let block = format_scan_block(&aps);
        let strong_at = block.find("Strong").unwrap();
        let weak_at = block.find("Weak").unwrap();
        assert!(strong_at < weak_at, "strongest first:\n{block}");
        assert!(block.contains("ssids = ["));
    }

    #[test]
    fn format_scan_block_quotes_ssids_for_toml() {
        let aps = vec![AccessPoint {
            ssid: "My Net".into(),
            strength: 55,
        }];
        // {:?} quotes + escapes, so SSIDs with spaces paste as valid TOML.
        assert!(format_scan_block(&aps).contains("\"My Net\""));
    }
}
