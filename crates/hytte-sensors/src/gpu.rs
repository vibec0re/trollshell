//! GPU state reading — AMD via sysfs, Intel via sysfs (i915/xe), Nvidia via `nvidia-smi`.

use crate::cast::{millicelsius_to_celsius, percent_u64_to_ratio, u64_to_f64_count};

use super::{GpuState, GpuVendor};
use std::time::{Duration, Instant};

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

/// How long a successful Nvidia reading stays valid before
/// [`read_nvidia_with_cache_at`] forks `nvidia-smi` again.
///
/// The samplers that drive [`read_gpu_with_cache`] tick at 1 Hz (#1249), so
/// this is picked against that tick, not against how fast the GPU's own
/// numbers actually move:
///
/// - It must be **under one tick** (1000 ms): a TTL at or above the tick
///   period would mean some ticks return the fork's normal cost while the
///   next tick's answer is silently the same age — worse, it would let two
///   callers sharing one [`GpuCache`] value (#1295's bar + sidebar overlap)
///   fall out of step by up to the TTL, one paying for a fork the other just
///   made. Staying under the tick means the constraint holds continuously,
///   not "on average".
/// - It must be **long enough to matter**: several concurrent callers on the
///   same tick (a fresh cache handed to more than one poller in the same
///   process, or two ticks that land within the same wall-clock window) all
///   land on the one cached reading instead of each forking their own.
///
/// 500 ms sits at half a tick — one `nvidia-smi` fork is guaranteed every
/// tick (the cache can never coast for two consecutive 1 Hz ticks), while
/// still collapsing any callers that land within the same half-second to
/// that one fork. This is the issue's own suggestion (#1297); a shorter TTL
/// buys nothing further since the tick, not the TTL, is now the fork rate's
/// floor, and a longer one risks a 1 Hz sampler occasionally showing a
/// reading from the *previous* tick.
///
/// This bounds forks **within one process sharing one cache value** — three
/// processes (the native sensors service, a bar `hytte-plugin-stats`
/// instance, a sidebar instance, #1248) each carry their own [`GpuCache`]
/// and still fork independently. Collapsing that is #1252 (retire the native
/// sampler) plus the sidebar instance's own parking, which already stops a
/// closed sidebar from polling at all.
pub const NVIDIA_READING_TTL: Duration = Duration::from_millis(500);

/// Per-tick GPU cache state threaded through the poll loop.
///
/// Carried alongside each GPU tick so readers never need `Mutex` or `Arc`.
/// Not [`Copy`] — [`nvidia_last`](Self::nvidia_last) carries a [`GpuState`],
/// which owns a `String`; callers that previously relied on `Copy` move the
/// value out with [`std::mem::take`] instead (see
/// `hytte-services::sensors::mod::poll_loop`, the existing precedent this
/// followed, and `hytte-plugin-stats::sample::Sampler::tick`, the caller
/// #1297 updated to match it).
#[derive(Clone, Debug, Default)]
pub struct GpuCache {
    /// Whether `nvidia-smi` is available.
    ///
    /// - `None`        — not yet probed; probe on the next GPU tick.
    /// - `Some(false)` — previously absent; skip `nvidia-smi`.
    /// - `Some(true)`  — previously present; call `nvidia-smi` directly, or
    ///   serve [`nvidia_last`](Self::nvidia_last) if it is still within
    ///   [`NVIDIA_READING_TTL`].
    pub(super) nvidia_available: Option<bool>,
    /// Previous RC6 residency sample for Intel GPU usage computation.
    ///
    /// `None` means either no Intel GPU detected yet, or this is the first
    /// tick (no delta available).
    pub(super) intel_rc6_prev: Option<(u64, Instant)>,
    /// The last successful Nvidia reading and when it was taken.
    ///
    /// `None` until the first successful `nvidia-smi` fork, and reset to
    /// `None` whenever a fork fails (mirrors `nvidia_available` flipping to
    /// `Some(false)` — a failed reading is never served stale). See
    /// [`NVIDIA_READING_TTL`] for how long a reading here stays valid.
    pub(super) nvidia_last: Option<(GpuState, Instant)>,
}

/// The Nvidia arm of [`read_gpu_with_cache`], with the [`NVIDIA_READING_TTL`]
/// throttle applied.
///
/// Isolated from the composed function (and from `Instant::now()`/
/// `nvidia-smi` directly) purely for testability: the AMD and Intel arms are
/// sysfs reads a test can't hermetically fake without touching `/sys`, but
/// this arm's only external calls are the injected `now` and `read` — see
/// this module's tests for the counting-fake-reader cases the issue asked
/// for.
///
/// Availability semantics of `cache.nvidia_available` are unchanged from
/// before #1297 — `None` probes once, `Some(false)` never forks — the new
/// behaviour is entirely in the `Some(true)` arm, which now forks only when
/// [`NVIDIA_READING_TTL`] has elapsed since the last successful reading, and
/// otherwise returns a clone of it.
fn read_nvidia_with_cache_at(
    cache: GpuCache,
    now: Instant,
    read: impl FnOnce() -> Option<GpuState>,
) -> (Option<GpuState>, GpuCache) {
    let nv_available = cache.nvidia_available.unwrap_or(true); // unknown → optimistically try once
    if !nv_available {
        return (
            None,
            GpuCache {
                nvidia_available: Some(false),
                intel_rc6_prev: None,
                nvidia_last: None,
            },
        );
    }

    // A reading inside the TTL is served without forking — this is the fix:
    // previously every call with `nvidia_available == Some(true)` forked.
    if let Some((state, sampled_at)) = &cache.nvidia_last
        && now.saturating_duration_since(*sampled_at) < NVIDIA_READING_TTL
    {
        let reading = state.clone();
        return (
            Some(reading),
            GpuCache {
                nvidia_available: Some(true),
                intel_rc6_prev: None,
                nvidia_last: cache.nvidia_last,
            },
        );
    }

    match read() {
        Some(state) => (
            Some(state.clone()),
            GpuCache {
                nvidia_available: Some(true),
                intel_rc6_prev: None,
                nvidia_last: Some((state, now)),
            },
        ),
        None => (
            None,
            GpuCache {
                nvidia_available: Some(false),
                intel_rc6_prev: None,
                nvidia_last: None,
            },
        ),
    }
}

/// Read GPU state, caching probe results across ticks.
///
/// Probe order: AMD → Intel → Nvidia.
///
/// `cache` carries the mutable per-tick state:
/// - `nvidia_available`: whether `nvidia-smi` is usable (see [`GpuCache`]).
/// - `intel_rc6_prev`: previous `(rc6_ms, Instant)` for RC6 delta computation.
/// - `nvidia_last`: the last successful Nvidia reading, served instead of
///   forking again while still within [`NVIDIA_READING_TTL`] (#1297).
///
/// Returns `(gpu_state, updated_cache)`. The caller stores the returned cache
/// back into `PollState`.
#[must_use]
pub fn read_gpu_with_cache(cache: GpuCache) -> (Option<GpuState>, GpuCache) {
    // AMD sysfs reads don't need caching — `read_amd_gpu` only walks
    // `/sys/class/drm` and exits on the first AMD card it finds.
    if let Some(state) = read_amd_gpu() {
        // AMD present — preserve whatever nvidia_available/nvidia_last were
        // (no need to probe or throttle Nvidia on an AMD box).
        return (
            Some(state),
            GpuCache {
                nvidia_available: cache.nvidia_available.or(Some(false)),
                intel_rc6_prev: None,
                nvidia_last: cache.nvidia_last,
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
                nvidia_last: cache.nvidia_last,
            },
        );
    }

    // No AMD or Intel GPU. Nvidia arm carries its own reading-TTL throttle.
    read_nvidia_with_cache_at(cache, Instant::now(), read_nvidia_gpu)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
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
        let t1 = t0 + Duration::from_secs(1);
        // rc6 counter went backwards (reset/wrap) — saturating_sub floors the
        // delta at 0, meaning "no idle time observed" → 100% busy.
        let (_new_prev, load) = compute_intel_usage(100, Some((5_000, t0)), t1);
        assert_eq!(load, Some(1.0));
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_intel_usage_residency_exceeding_elapsed_clamps_to_100_idle() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        // 5000ms of RC6 residency reported over a 1000ms tick — idle% would
        // be 500% uncapped; must clamp to 100% idle (0% usage).
        let (_new_prev, load) = compute_intel_usage(5_000, Some((0, t0)), t1);
        assert_eq!(load, Some(0.0));
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_intel_usage_normal_delta_computes_partial_load() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        // 500ms idle (RC6) out of a 1000ms tick → 50% idle → 50% usage.
        let (new_prev, load) = compute_intel_usage(500, Some((0, t0)), t1);
        assert_eq!(new_prev, Some((500, t1)));
        assert_eq!(load, Some(0.5));
    }

    // ── read_nvidia_with_cache_at (#1297) ────────────────────────────────────

    fn fake_reading(name: &str) -> GpuState {
        GpuState {
            vendor: GpuVendor::Nvidia,
            name: name.to_string(),
            ..GpuState::default()
        }
    }

    #[test]
    fn nvidia_reading_ttl_collapses_repeated_calls_to_one_fork() {
        let fork_count = Cell::new(0u32);
        let t0 = Instant::now();

        let (first, cache) = read_nvidia_with_cache_at(GpuCache::default(), t0, || {
            fork_count.set(fork_count.get() + 1);
            Some(fake_reading("fake nvidia"))
        });
        assert_eq!(
            fork_count.get(),
            1,
            "the first call has no cached reading yet and must fork"
        );
        assert_eq!(first.as_ref().map(|s| s.name.as_str()), Some("fake nvidia"));

        // Several more calls, at different instants strictly inside the TTL
        // window, must all be served the cached reading with no further fork.
        for step in 1..=5u64 {
            let now = t0 + Duration::from_millis(step * 80); // up to 400ms, under the 500ms TTL
            let (state, _) = read_nvidia_with_cache_at(cache.clone(), now, || {
                fork_count.set(fork_count.get() + 1);
                Some(fake_reading("should not be forked"))
            });
            assert_eq!(
                fork_count.get(),
                1,
                "call {step} inside the TTL forked nvidia-smi again (fork count moved past 1)"
            );
            assert_eq!(
                state.as_ref().map(|s| s.name.as_str()),
                Some("fake nvidia"),
                "call {step} inside the TTL must return the cached reading, not a fresh one"
            );
        }
    }

    #[test]
    fn nvidia_reading_ttl_elapsed_forks_again() {
        let fork_count = Cell::new(0u32);
        let t0 = Instant::now();

        let (_, cache) = read_nvidia_with_cache_at(GpuCache::default(), t0, || {
            fork_count.set(fork_count.get() + 1);
            Some(fake_reading("first"))
        });
        assert_eq!(fork_count.get(), 1);

        // Landing exactly on the TTL boundary must no longer be "inside" it.
        let (state, _) = read_nvidia_with_cache_at(cache, t0 + NVIDIA_READING_TTL, || {
            fork_count.set(fork_count.get() + 1);
            Some(fake_reading("second"))
        });
        assert_eq!(
            fork_count.get(),
            2,
            "a call at the TTL boundary must fork again instead of reusing the stale reading"
        );
        assert_eq!(state.as_ref().map(|s| s.name.as_str()), Some("second"));
    }

    #[test]
    fn nvidia_failed_fork_marks_unavailable_and_clears_cached_reading() {
        let t0 = Instant::now();
        // Seed a cache as if a reading had just been taken, then land past
        // the TTL so the call must actually attempt a fresh fork — and have
        // that fork fail. A failure must never leave the stale reading
        // behind for a later call to serve.
        let seeded = GpuCache {
            nvidia_available: Some(true),
            intel_rc6_prev: None,
            nvidia_last: Some((fake_reading("stale"), t0)),
        };
        let (state, cache) = read_nvidia_with_cache_at(seeded, t0 + NVIDIA_READING_TTL, || None);
        assert!(state.is_none(), "a failed fork must not surface any reading");
        assert_eq!(cache.nvidia_available, Some(false));
        assert!(
            cache.nvidia_last.is_none(),
            "a failed fork must clear the previously cached reading, not leave it stale"
        );
    }

    #[test]
    fn nvidia_unavailable_never_consults_cached_reading_or_forks() {
        let t0 = Instant::now();
        let cache = GpuCache {
            nvidia_available: Some(false),
            intel_rc6_prev: None,
            // Even a (hypothetically) still-present cached reading must not
            // surface once nvidia_available says "previously absent".
            nvidia_last: Some((fake_reading("must not surface"), t0)),
        };
        let called = Cell::new(false);
        let (state, new_cache) = read_nvidia_with_cache_at(cache, t0, || {
            called.set(true);
            None
        });
        assert!(
            state.is_none(),
            "nvidia_available == Some(false) must never surface a cached reading"
        );
        assert!(
            !called.get(),
            "nvidia_available == Some(false) must never fork nvidia-smi"
        );
        assert_eq!(new_cache.nvidia_available, Some(false));
    }
}
