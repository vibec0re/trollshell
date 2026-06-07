//! Visible Wi-Fi access points from `NetworkManager` (system D-Bus).
//!
//! Enumerates every AP `NetworkManager` currently sees — `{bssid, ssid,
//! strength}` — purely for location fingerprinting (see [`crate::places`]).
//! This is **not** a Wi-Fi manager (that's [`crate::wifi`], an iwd client); it
//! only reads the scan list, so it stays a thin read-only sensor.
//!
//! `NetworkManager` scans on its own cadence; we re-read the list every
//! [`SCAN_INTERVAL`] (the AP set only changes when you physically move, so a
//! leisurely poll is plenty).
//!
//! Published via [`current`] (registry signal, GTK thread) and
//! [`shared_aps`] (a process-global clone) so the `places` resolver's tokio
//! task can read it without touching the thread-local registry — mirroring
//! the geoclue shared-handle pattern.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::Duration;

use futures_signals::signal::{Mutable, Signal};
use hytte_bus::{BusKind, call};
use hytte_reactive::{Service, registry, runtime};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const NM_NAME: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const WIRELESS_IFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const AP_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Re-read cadence. Location changes at building scale, so this is leisurely.
const SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// One visible access point. The `bssid` is the AP's MAC (`HwAddress`),
/// lowercased; it's globally unique per radio, which is what makes a
/// constellation of them a location fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessPoint {
    pub bssid: String,
    pub ssid: String,
    /// Signal strength, 0..=100.
    pub strength: u8,
}

#[doc(hidden)]
#[derive(Default)]
pub struct WifiScanHandles {
    pub(crate) aps: Mutable<Vec<AccessPoint>>,
}

// Cross-thread shared handle: `registry` is GTK-thread-only, so the `places`
// resolver's tokio task reads visible APs from here instead.
struct Shared {
    aps: Mutable<Vec<AccessPoint>>,
}
static SHARED: OnceLock<Shared> = OnceLock::new();

pub struct WifiScanService;

impl Service for WifiScanService {
    type Handles = WifiScanHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = WifiScanHandles::default();
        let aps = handles.aps.clone();
        let _ = SHARED.set(Shared { aps: aps.clone() });
        rt.spawn(scan_loop(aps));
        handles
    }
}

#[must_use]
pub fn service() -> WifiScanService {
    WifiScanService
}

/// Signal of the currently-visible access points. First emission is the empty
/// list (before the first scan read lands).
pub fn current() -> impl Signal<Item = Vec<AccessPoint>> {
    registry::with(|r| {
        r.get::<WifiScanHandles>()
            .expect("wifiscan::service() not registered")
            .aps
            .signal_cloned()
    })
}

/// Process-global clone of the visible-AP handle, for tokio-side readers
/// (the `places` resolver). `None` until [`service`] has started.
#[must_use]
pub fn shared_aps() -> Option<Mutable<Vec<AccessPoint>>> {
    SHARED.get().map(|s| s.aps.clone())
}

/// One-shot blocking scan for the `--scan-aps` CLI: forces a fresh scan,
/// waits briefly, and returns the visible APs. Drives the process runtime, so
/// it must be called from a non-async context (e.g. `main` before the App).
#[must_use]
pub fn scan_aps_blocking() -> Vec<AccessPoint> {
    runtime::handle().block_on(async { collect_aps(true).await.unwrap_or_default() })
}

/// Render visible APs as a paste-ready `bssids = [...]` TOML block for the
/// `--scan-aps` capture tool. Strongest first; SSID + strength as comments.
#[must_use]
pub fn format_scan_block(aps: &[AccessPoint]) -> String {
    let mut sorted: Vec<&AccessPoint> = aps.iter().collect();
    sorted.sort_by(|a, b| {
        b.strength
            .cmp(&a.strength)
            .then_with(|| a.bssid.cmp(&b.bssid))
    });

    let mut out = String::new();
    out.push_str("# Visible access points (strongest first). Paste the stable ones into\n");
    out.push_str("# a [[place]] in ~/.config/trollshell/places.toml:\n");
    out.push_str("bssids = [\n");
    for ap in sorted {
        let ssid = if ap.ssid.is_empty() {
            "<hidden>"
        } else {
            ap.ssid.as_str()
        };
        let _ = writeln!(out, "  \"{}\",  # {} ({}%)", ap.bssid, ssid, ap.strength);
    }
    out.push_str("]\n");
    out
}

async fn scan_loop(aps: Mutable<Vec<AccessPoint>>) {
    let mut tick = tokio::time::interval(SCAN_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        match collect_aps(false).await {
            Ok(list) => {
                if aps.get_cloned() != list {
                    aps.set(list);
                }
            }
            Err(e) => tracing::debug!(error = %e, "wifiscan: read failed (NetworkManager absent?)"),
        }
    }
}

/// Read every visible AP across all wireless devices, de-duplicated by BSSID.
/// When `force_scan`, nudges a fresh `RequestScan` first and waits briefly for
/// it to land (used by the one-shot CLI; the poll loop relies on NM's own
/// periodic scans).
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

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for dev in &devices {
        // GetAllAccessPoints only exists on wireless devices; a non-wifi
        // device errors here and is skipped — no DeviceType check needed.
        let Ok(ap_paths) = get_all_access_points(dev).await else {
            continue;
        };
        for ap in ap_paths {
            if let Some(sighting) = read_ap(&ap).await
                && seen.insert(sighting.bssid.clone())
            {
                out.push(sighting);
            }
        }
    }
    Ok(out)
}

async fn get_devices() -> Result<Vec<OwnedObjectPath>, hytte_bus::BusError> {
    call(NM_NAME)
        .bus(BusKind::System)
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
    call(NM_NAME)
        .bus(BusKind::System)
        .at_path(device.as_str().to_string())
        .iface(WIRELESS_IFACE)
        .method("GetAllAccessPoints")
        .args(())
        .send::<Vec<OwnedObjectPath>>()
        .await
}

async fn request_scan(device: &OwnedObjectPath) -> Result<(), hytte_bus::BusError> {
    let options: HashMap<String, zbus::zvariant::Value<'static>> = HashMap::new();
    call(NM_NAME)
        .bus(BusKind::System)
        .at_path(device.as_str().to_string())
        .iface(WIRELESS_IFACE)
        .method("RequestScan")
        .args((options,))
        .send::<()>()
        .await
}

async fn read_ap(ap_path: &OwnedObjectPath) -> Option<AccessPoint> {
    let props: HashMap<String, OwnedValue> = call(NM_NAME)
        .bus(BusKind::System)
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
/// `HwAddress` (the BSSID) is required and lowercased; `Ssid` (`ay`) is
/// decoded lossily and may be empty (hidden network).
fn ap_from_props(props: &HashMap<String, OwnedValue>) -> Option<AccessPoint> {
    let bssid = property::<String>(props, "HwAddress")?;
    if bssid.is_empty() {
        return None;
    }
    let ssid = prop_bytes(props, "Ssid")
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let strength = property::<u8>(props, "Strength").unwrap_or(0);
    Some(AccessPoint {
        bssid: bssid.to_ascii_lowercase(),
        ssid,
        strength,
    })
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

    fn props(bssid: &str, ssid: &[u8], strength: u8) -> HashMap<String, OwnedValue> {
        let mut p = HashMap::new();
        p.insert("HwAddress".to_string(), owned(Value::from(bssid)));
        p.insert("Ssid".to_string(), owned(Value::from(ssid.to_vec())));
        p.insert("Strength".to_string(), owned(Value::from(strength)));
        p
    }

    #[test]
    fn ap_from_props_extracts_and_lowercases_bssid() {
        let p = props("A4:2B:8C:11:22:33", b"FRITZ!Box Annika", 87);
        let ap = ap_from_props(&p).expect("parses");
        assert_eq!(ap.bssid, "a4:2b:8c:11:22:33");
        assert_eq!(ap.ssid, "FRITZ!Box Annika");
        assert_eq!(ap.strength, 87);
    }

    #[test]
    fn ap_from_props_hidden_ssid_is_empty_not_fatal() {
        let p = props("aa:bb:cc:dd:ee:ff", b"", 40);
        let ap = ap_from_props(&p).expect("parses");
        assert_eq!(ap.bssid, "aa:bb:cc:dd:ee:ff");
        assert!(ap.ssid.is_empty());
    }

    #[test]
    fn ap_from_props_missing_bssid_is_none() {
        let mut p = HashMap::new();
        p.insert("Strength".to_string(), owned(Value::from(50u8)));
        assert!(ap_from_props(&p).is_none());
    }

    #[test]
    fn ap_from_props_non_utf8_ssid_is_lossy() {
        // 0xFF 0xFE is not valid UTF-8 — must not panic, decodes lossily.
        let p = props("aa:bb:cc:dd:ee:ff", &[0xff, 0xfe], 10);
        let ap = ap_from_props(&p).expect("parses");
        assert!(!ap.ssid.is_empty()); // replacement chars, but present
    }

    #[test]
    fn format_scan_block_sorts_by_strength_desc() {
        let aps = vec![
            AccessPoint {
                bssid: "11:11:11:11:11:11".into(),
                ssid: "Weak".into(),
                strength: 20,
            },
            AccessPoint {
                bssid: "22:22:22:22:22:22".into(),
                ssid: "Strong".into(),
                strength: 90,
            },
        ];
        let block = format_scan_block(&aps);
        let strong_at = block.find("22:22:22:22:22:22").unwrap();
        let weak_at = block.find("11:11:11:11:11:11").unwrap();
        assert!(
            strong_at < weak_at,
            "strongest AP should come first:\n{block}"
        );
        assert!(block.starts_with("# Visible"));
        assert!(block.contains("bssids = ["));
    }

    #[test]
    fn format_scan_block_marks_hidden_ssid() {
        let aps = vec![AccessPoint {
            bssid: "aa:aa:aa:aa:aa:aa".into(),
            ssid: String::new(),
            strength: 55,
        }];
        assert!(format_scan_block(&aps).contains("<hidden>"));
    }
}
