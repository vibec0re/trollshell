//! GPU state reading — AMD via sysfs, Intel via sysfs (i915/xe), Nvidia via `nvidia-smi`.

use crate::cast::{millicelsius_to_celsius, percent_u64_to_ratio, u64_to_f64_count};

use super::{GpuState, GpuVendor};
use std::time::Instant;

/// True when a sysfs `device/vendor` file's contents identify an AMD PCI
/// device (vendor ID `0x1002`). Factored out of [`read_amd_gpu`] so the
/// string-parsing rule is testable against fixture strings without touching
/// `/sys`.
fn is_amd_vendor(vendor_file_contents: &str) -> bool {
    vendor_file_contents.trim() == "0x1002"
}

/// Parse the GPU display name out of a sysfs `device/uevent` file's contents:
/// pulls the `DRIVER=` line (e.g. `amdgpu`) into `"AMD (amdgpu)"`. Falls back
/// to `"AMD GPU"` when no `DRIVER=` line is present. Factored out of
/// [`read_amd_gpu`] so the parsing is testable against fixture strings.
fn amd_gpu_name_from_uevent(uevent_file_contents: &str) -> String {
    uevent_file_contents
        .lines()
        .find_map(|l| l.strip_prefix("DRIVER=").map(|d| format!("AMD ({d})")))
        .unwrap_or_else(|| "AMD GPU".to_string())
}

fn read_amd_gpu() -> Option<GpuState> {
    use std::fs;
    let drm = fs::read_dir("/sys/class/drm").ok()?;
    for entry in drm.flatten() {
        let entry_name = entry.file_name();
        let Some(name) = entry_name.to_str() else {
            continue;
        };
        // Only top-level cards (cardN), not connectors (cardN-...)
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let device = entry.path().join("device");
        let Ok(vendor) = fs::read_to_string(device.join("vendor")) else {
            continue;
        };
        if !is_amd_vendor(&vendor) {
            continue;
        }

        let load = fs::read_to_string(device.join("gpu_busy_percent"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(percent_u64_to_ratio);

        let memory_used_bytes = fs::read_to_string(device.join("mem_info_vram_used"))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        let memory_total_bytes = fs::read_to_string(device.join("mem_info_vram_total"))
            .ok()
            .and_then(|s| s.trim().parse().ok());

        // Temperature: walk device/hwmon/hwmonN/temp1_input
        let mut temperature_celsius = None;
        if let Ok(hwmon) = fs::read_dir(device.join("hwmon")) {
            for h in hwmon.flatten() {
                let temp = h.path().join("temp1_input");
                if let Ok(s) = fs::read_to_string(&temp)
                    && let Ok(v) = s.trim().parse::<u64>()
                {
                    temperature_celsius = Some(millicelsius_to_celsius(v));
                    break;
                }
            }
        }

        // Name from /sys/class/drm/cardN/device/uevent or just hardcode "AMD GPU"
        let gpu_name = fs::read_to_string(device.join("uevent"))
            .ok()
            .map_or_else(|| "AMD GPU".to_string(), |s| amd_gpu_name_from_uevent(&s));

        return Some(GpuState {
            vendor: GpuVendor::Amd,
            name: gpu_name,
            temperature_celsius,
            load,
            memory_used_bytes,
            memory_total_bytes,
        });
    }
    None
}

/// Read RC6 residency in milliseconds from the Intel sysfs paths.
///
/// Tries the newer `gt/gt0/rc6_residency_ms` path first (xe driver / multi-tile),
/// then falls back to the older `power/rc6_residency_ms` (i915).
fn read_intel_rc6_ms(card_path: &std::path::Path) -> Option<u64> {
    use std::fs;
    // Newer path: xe driver or recent i915 with gt sub-directory
    let newer = card_path.join("gt/gt0/rc6_residency_ms");
    if let Ok(s) = fs::read_to_string(&newer)
        && let Ok(v) = s.trim().parse::<u64>()
    {
        return Some(v);
    }
    // Older i915 path
    let older = card_path.join("power/rc6_residency_ms");
    fs::read_to_string(older)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Compute Intel GPU usage from RC6 idle-residency delta.
///
/// RC6 is a power-saving idle state; when the GPU is busy it exits RC6.
/// `idle% = Δrc6_ms / dt_ms * 100`, `usage% = 100 − idle%`.
///
/// Returns `(new_rc6_ms, usage_ratio_0_to_1)`.  On the first call (no prev),
/// returns `(current_rc6_ms, 0.0)`.
#[allow(clippy::cast_precision_loss)] // dt_ms and delta are bounded well within f64 precision
fn compute_intel_usage(
    rc6_now: u64,
    prev: Option<(u64, Instant)>,
    now: Instant,
) -> (Option<(u64, Instant)>, Option<f64>) {
    let new_prev = Some((rc6_now, now));
    let Some((rc6_prev, prev_when)) = prev else {
        // First tick — no delta available yet.
        return (new_prev, Some(0.0));
    };
    // Duration between ticks is at most a few seconds; u128→u64 is always safe in practice.
    // Saturate at u64::MAX (584 million years) to avoid any theoretical overflow.
    let dt_ms = u64::try_from(now.duration_since(prev_when).as_millis()).unwrap_or(u64::MAX);
    if dt_ms == 0 {
        return (new_prev, Some(0.0));
    }
    let delta_rc6 = rc6_now.saturating_sub(rc6_prev);
    // idle% = delta_rc6_ms / dt_ms * 100; clamp to [0, 100]
    let idle_pct = (delta_rc6 * 100 / dt_ms).min(100);
    let usage_pct = 100u64.saturating_sub(idle_pct);
    let load = u64_to_f64_count(usage_pct) / 100.0;
    (new_prev, Some(load))
}

fn read_intel_gpu(rc6_prev: Option<(u64, Instant)>) -> Option<(GpuState, Option<(u64, Instant)>)> {
    use std::fs;
    let drm = fs::read_dir("/sys/class/drm").ok()?;
    for entry in drm.flatten() {
        let entry_name = entry.file_name();
        let Some(name) = entry_name.to_str() else {
            continue;
        };
        // Only top-level cards (cardN), not connectors (cardN-...)
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let card_path = entry.path();
        let device = card_path.join("device");

        // Detect Intel via driver symlink resolving to i915 or xe.
        let driver_path = device.join("driver");
        let Ok(resolved) = fs::read_link(&driver_path) else {
            continue;
        };
        let driver_name = resolved.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if driver_name != "i915" && driver_name != "xe" {
            continue;
        }

        // RC6-based usage delta
        let now = Instant::now();
        let (new_rc6_prev, load) = match read_intel_rc6_ms(&card_path) {
            Some(rc6_now) => compute_intel_usage(rc6_now, rc6_prev, now),
            None => (rc6_prev, None),
        };

        // Temperature: walk device/hwmon/hwmonN/temp1_input (same as AMD)
        let mut temperature_celsius = None;
        if let Ok(hwmon) = fs::read_dir(device.join("hwmon")) {
            for h in hwmon.flatten() {
                let temp = h.path().join("temp1_input");
                if let Ok(s) = fs::read_to_string(&temp)
                    && let Ok(v) = s.trim().parse::<u64>()
                {
                    temperature_celsius = Some(millicelsius_to_celsius(v));
                    break;
                }
            }
        }

        // VRAM: discrete Arc only; absent/0 on integrated iGPU — degrade gracefully.
        let memory_used_bytes = fs::read_to_string(device.join("mem_info_vram_used"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&b| b > 0);
        let memory_total_bytes = fs::read_to_string(device.join("mem_info_vram_total"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&b| b > 0);

        let gpu_name = format!("Intel GPU ({driver_name})");

        return Some((
            GpuState {
                vendor: GpuVendor::Intel,
                name: gpu_name,
                temperature_celsius,
                load,
                memory_used_bytes,
                memory_total_bytes,
            },
            new_rc6_prev,
        ));
    }
    None
}

fn read_nvidia_gpu() -> Option<GpuState> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,temperature.gpu,utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = std::str::from_utf8(&out.stdout).ok()?;
    let line = s.lines().next()?;
    let parts: Vec<&str> = line.split(',').map(str::trim).collect();
    if parts.len() < 5 {
        return None;
    }
    let gpu_name = parts[0].to_string();
    let temperature_celsius = parts[1].parse::<f64>().ok();
    let load = parts[2].parse::<f64>().ok().map(|v| v / 100.0);
    let mem_used_mib: Option<u64> = parts[3].parse().ok();
    let mem_total_mib: Option<u64> = parts[4].parse().ok();
    let memory_used_bytes = mem_used_mib.map(|m| m * 1024 * 1024);
    let memory_total_bytes = mem_total_mib.map(|m| m * 1024 * 1024);
    Some(GpuState {
        vendor: GpuVendor::Nvidia,
        name: gpu_name,
        temperature_celsius,
        load,
        memory_used_bytes,
        memory_total_bytes,
    })
}

/// Per-tick GPU cache state threaded through the poll loop.
///
/// Carried alongside each GPU tick so readers never need `Mutex` or `Arc`.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct GpuCache {
    /// Whether `nvidia-smi` is available.
    ///
    /// - `None`        — not yet probed; probe on the next GPU tick.
    /// - `Some(false)` — previously absent; skip `nvidia-smi`.
    /// - `Some(true)`  — previously present; call `nvidia-smi` directly.
    pub(super) nvidia_available: Option<bool>,
    /// Previous RC6 residency sample for Intel GPU usage computation.
    ///
    /// `None` means either no Intel GPU detected yet, or this is the first
    /// tick (no delta available).
    pub(super) intel_rc6_prev: Option<(u64, Instant)>,
}

/// Read GPU state, caching probe results across ticks.
///
/// Probe order: AMD → Intel → Nvidia.
///
/// `cache` carries the mutable per-tick state:
/// - `nvidia_available`: whether `nvidia-smi` is usable (see [`GpuCache`]).
/// - `intel_rc6_prev`: previous `(rc6_ms, Instant)` for RC6 delta computation.
///
/// Returns `(gpu_state, updated_cache)`. The caller stores the returned cache
/// back into `PollState`.
pub(super) fn read_gpu_with_cache(cache: GpuCache) -> (Option<GpuState>, GpuCache) {
    // AMD sysfs reads don't need caching — `read_amd_gpu` only walks
    // `/sys/class/drm` and exits on the first AMD card it finds.
    if let Some(state) = read_amd_gpu() {
        // AMD present — preserve whatever nvidia_available was (no need to probe).
        return (
            Some(state),
            GpuCache {
                nvidia_available: cache.nvidia_available.or(Some(false)),
                intel_rc6_prev: None,
            },
        );
    }

    // No AMD GPU. Try Intel.
    if let Some((state, new_rc6_prev)) = read_intel_gpu(cache.intel_rc6_prev) {
        return (
            Some(state),
            GpuCache {
                nvidia_available: cache.nvidia_available.or(Some(false)),
                intel_rc6_prev: new_rc6_prev,
            },
        );
    }

    // No AMD or Intel GPU. Try nvidia if we know (or suspect) it might be present.
    let nv_available = cache.nvidia_available.unwrap_or(true); // unknown → optimistically try once
    if !nv_available {
        return (
            None,
            GpuCache {
                nvidia_available: Some(false),
                intel_rc6_prev: None,
            },
        );
    }
    match read_nvidia_gpu() {
        Some(state) => (
            Some(state),
            GpuCache {
                nvidia_available: Some(true),
                intel_rc6_prev: None,
            },
        ),
        None => (
            None,
            GpuCache {
                nvidia_available: Some(false),
                intel_rc6_prev: None,
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── is_amd_vendor / amd_gpu_name_from_uevent ─────────────────────────────

    #[test]
    fn is_amd_vendor_matches_amd_pci_id() {
        assert!(is_amd_vendor("0x1002\n"));
        assert!(is_amd_vendor("0x1002"));
    }

    #[test]
    fn is_amd_vendor_rejects_other_vendors() {
        // 0x8086 is Intel, 0x10de is Nvidia.
        assert!(!is_amd_vendor("0x8086\n"));
        assert!(!is_amd_vendor("0x10de\n"));
        assert!(!is_amd_vendor(""));
    }

    #[test]
    fn amd_gpu_name_from_uevent_extracts_driver() {
        let uevent = "DRIVER=amdgpu\nPCI_CLASS=30000\nMODALIAS=pci:foo\n";
        assert_eq!(amd_gpu_name_from_uevent(uevent), "AMD (amdgpu)");
    }

    #[test]
    fn amd_gpu_name_from_uevent_falls_back_without_driver_line() {
        let uevent = "PCI_CLASS=30000\nMODALIAS=pci:foo\n";
        assert_eq!(amd_gpu_name_from_uevent(uevent), "AMD GPU");
    }

    #[test]
    fn amd_gpu_name_from_uevent_falls_back_on_empty_input() {
        assert_eq!(amd_gpu_name_from_uevent(""), "AMD GPU");
    }

    // ── compute_intel_usage ───────────────────────────────────────────────

    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_intel_usage_first_call_has_no_delta() {
        let now = Instant::now();
        let (new_prev, load) = compute_intel_usage(1_000, None, now);
        assert_eq!(new_prev, Some((1_000, now)));
        assert_eq!(load, Some(0.0), "first sample has nothing to diff against");
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_intel_usage_zero_elapsed_reports_zero_load() {
        let t0 = Instant::now();
        // Second call lands on the exact same Instant as the first (dt_ms == 0)
        // — must not divide by zero.
        let (new_prev, load) = compute_intel_usage(500, Some((100, t0)), t0);
        assert_eq!(new_prev, Some((500, t0)));
        assert_eq!(load, Some(0.0));
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_intel_usage_counter_reset_clamps_to_full_load() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1_000);
        // rc6 counter went backwards (reset/wrap) — saturating_sub floors the
        // delta at 0, meaning "no idle time observed" → 100% busy.
        let (_new_prev, load) = compute_intel_usage(100, Some((5_000, t0)), t1);
        assert_eq!(load, Some(1.0));
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_intel_usage_residency_exceeding_elapsed_clamps_to_100_idle() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1_000);
        // 5000ms of RC6 residency reported over a 1000ms tick — idle% would
        // be 500% uncapped; must clamp to 100% idle (0% usage).
        let (_new_prev, load) = compute_intel_usage(5_000, Some((0, t0)), t1);
        assert_eq!(load, Some(0.0));
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_intel_usage_normal_delta_computes_partial_load() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1_000);
        // 500ms idle (RC6) out of a 1000ms tick → 50% idle → 50% usage.
        let (new_prev, load) = compute_intel_usage(500, Some((0, t0)), t1);
        assert_eq!(new_prev, Some((500, t1)));
        assert_eq!(load, Some(0.5));
    }
}
