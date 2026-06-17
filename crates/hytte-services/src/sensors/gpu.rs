//! GPU state reading — AMD via sysfs, Nvidia via `nvidia-smi`.

use crate::cast::{millicelsius_to_celsius, percent_u64_to_ratio};

use super::{GpuState, GpuVendor};

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
        if vendor.trim() != "0x1002" {
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
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("DRIVER=").map(|d| format!("AMD ({d})")))
            })
            .unwrap_or_else(|| "AMD GPU".to_string());

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

/// Read GPU state, caching whether `nvidia-smi` is available.
///
/// `nvidia_available` is the cached probe result from the previous GPU tick:
/// - `None`  — not yet probed; probe now and remember the outcome.
/// - `Some(false)` — previously found absent; skip `nvidia-smi` entirely.
/// - `Some(true)`  — previously found present; call `nvidia-smi` directly.
///
/// Returns `(gpu_state, updated_nvidia_available)`.  The caller stores the
/// returned `bool` back into `PollState::nvidia_available`.
pub(super) fn read_gpu_with_cache(nvidia_available: Option<bool>) -> (Option<GpuState>, bool) {
    // AMD sysfs reads don't need caching — `read_amd_gpu` only walks
    // `/sys/class/drm` and exits on the first AMD card it finds.
    if let Some(state) = read_amd_gpu() {
        // AMD present — preserve whatever nvidia_available was (no need to probe).
        return (Some(state), nvidia_available.unwrap_or(false));
    }

    // No AMD GPU. Try nvidia if we know (or suspect) it might be present.
    let nv_available = nvidia_available.unwrap_or(true); // unknown → optimistically try once
    if !nv_available {
        return (None, false);
    }
    match read_nvidia_gpu() {
        Some(state) => (Some(state), true),
        None => (None, false),
    }
}
