//! CPU clock speed via `/sys/devices/system/cpu/cpu*/cpufreq`.
//!
//! Each tick reads every core's `scaling_cur_freq` (current frequency, kHz) and
//! `cpuinfo_max_freq` (the static hardware ceiling, kHz), converts to Hz, and
//! folds them into a [`CpuFreq`] snapshot. The aggregate `max_hz` is the highest
//! current frequency across cores (the reporter chose max, not mean); the
//! `max_ceiling_hz` is the highest ceiling across cores, used by the
//! `trollshell` stats panel's clock graph for a fixed 0→max axis.
//!
//! Graceful degrade: on hosts without a cpufreq governor (many VMs), the
//! `/sys/devices/system/cpu/cpu0/cpufreq` directory is absent — we publish an
//! empty [`CpuFreq`] so a UI row can self-hide, matching the battery/GPU
//! convention.

use std::path::{Path, PathBuf};

use crate::cast::khz_to_hz;

use super::CpuFreq;

/// Base directory for per-CPU sysfs nodes.
const CPU_BASE: &str = "/sys/devices/system/cpu";

/// One core's cpufreq sample, in kHz as read from sysfs.
///
/// `cur_khz` comes from `scaling_cur_freq`; `max_khz` from `cpuinfo_max_freq`
/// (absent on kernels/cores that don't expose it).
pub(super) struct CoreFreqKhz {
    /// Current frequency (`scaling_cur_freq`), kHz.
    pub cur_khz: u64,
    /// Static hardware ceiling (`cpuinfo_max_freq`), kHz. `None` if unreadable.
    pub max_khz: Option<u64>,
}

/// Read the current CPU clock snapshot.
///
/// Enumerates `/sys/devices/system/cpu/cpu{N}` directories that carry a
/// `cpufreq` subdir, orders them by core index for a stable `per_core` layout,
/// and reads each core's current + ceiling frequency. A core whose
/// `scaling_cur_freq` is missing or unparseable is skipped.
///
/// Returns an empty [`CpuFreq`] (per-core empty, zeros) when the base cpufreq
/// directory is absent (no governor — typical of VMs) or unreadable.
pub(super) fn read_cpu_freq() -> CpuFreq {
    let base = Path::new(CPU_BASE);

    // Graceful degrade: no cpufreq on cpu0 means no governor is driving the
    // CPU (VMs, some containers). Publish the default so a UI row self-hides.
    if !base.join("cpu0").join("cpufreq").is_dir() {
        return CpuFreq::default();
    }

    let Ok(entries) = std::fs::read_dir(base) else {
        return CpuFreq::default();
    };

    // Collect (core_index, cpufreq_dir) for every `cpuN` with a cpufreq subdir.
    // `read_dir` yields arbitrary order, so sort by index for a stable layout.
    let mut cores: Vec<(u32, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Match `cpu<digits>` exactly — skips `cpufreq`, `cpuidle`, etc.
        let Some(idx) = name
            .strip_prefix("cpu")
            .and_then(|rest| rest.parse::<u32>().ok())
        else {
            continue;
        };
        let cpufreq = entry.path().join("cpufreq");
        if cpufreq.is_dir() {
            cores.push((idx, cpufreq));
        }
    }
    cores.sort_by_key(|(idx, _)| *idx);

    let samples: Vec<CoreFreqKhz> = cores
        .iter()
        .filter_map(|(_, dir)| read_core_freq(dir))
        .collect();

    compute_cpu_freq(&samples)
}

/// Read one core's `scaling_cur_freq` + `cpuinfo_max_freq` from its `cpufreq`
/// directory. Returns `None` (skip this core) when the current frequency is
/// missing or unparseable; a missing ceiling is tolerated (`max_khz = None`).
fn read_core_freq(cpufreq_dir: &Path) -> Option<CoreFreqKhz> {
    let cur_khz = read_khz(&cpufreq_dir.join("scaling_cur_freq"))?;
    let max_khz = read_khz(&cpufreq_dir.join("cpuinfo_max_freq"));
    Some(CoreFreqKhz { cur_khz, max_khz })
}

/// Read a single-integer kHz sysfs file. `None` if unreadable or non-numeric.
fn read_khz(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Fold per-core kHz samples into a [`CpuFreq`] (values in Hz).
///
/// - `per_core` — each core's current frequency, in Hz.
/// - `max_hz` — aggregate = the maximum current frequency across cores.
/// - `max_ceiling_hz` — the highest `cpuinfo_max_freq` across cores.
///
/// An empty input yields the default (empty per-core, zeros) so callers degrade
/// gracefully.
pub(super) fn compute_cpu_freq(cores: &[CoreFreqKhz]) -> CpuFreq {
    if cores.is_empty() {
        return CpuFreq::default();
    }

    let per_core: Vec<f64> = cores.iter().map(|c| khz_to_hz(c.cur_khz)).collect();
    // Aggregate is the highest current core frequency (reporter chose max).
    let max_hz = per_core.iter().copied().fold(0.0_f64, f64::max);
    // Normalization ceiling: the highest static core ceiling.
    let ceiling_khz = cores.iter().filter_map(|c| c.max_khz).max().unwrap_or(0);
    let max_ceiling_hz = khz_to_hz(ceiling_khz);

    CpuFreq {
        max_hz,
        per_core,
        max_ceiling_hz,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core(cur_khz: u64, max_khz: Option<u64>) -> CoreFreqKhz {
        CoreFreqKhz { cur_khz, max_khz }
    }

    /// Per-core kHz values convert to Hz, and the aggregate `max_hz` is the
    /// maximum current frequency across cores (not the mean).
    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_uses_max_across_cores_in_hz() {
        // 1.2 GHz, 3.4 GHz, 2.0 GHz (all in kHz).
        let cores = [
            core(1_200_000, Some(4_000_000)),
            core(3_400_000, Some(4_000_000)),
            core(2_000_000, Some(4_000_000)),
        ];
        let freq = compute_cpu_freq(&cores);

        assert_eq!(
            freq.per_core,
            vec![1.2e9, 3.4e9, 2.0e9],
            "per-core must be kHz→Hz in input order"
        );
        assert_eq!(
            freq.max_hz, 3.4e9,
            "aggregate must be the max current freq across cores, not the mean"
        );
        assert_eq!(
            freq.max_ceiling_hz, 4.0e9,
            "ceiling must be the max cpuinfo_max_freq across cores, in Hz"
        );
    }

    /// The ceiling is the highest `cpuinfo_max_freq` even when cores advertise
    /// different maxima (heterogeneous / big.LITTLE layouts).
    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_ceiling_is_max_across_heterogeneous_cores() {
        let cores = [
            core(1_000_000, Some(2_000_000)),
            core(1_500_000, Some(5_000_000)),
        ];
        let freq = compute_cpu_freq(&cores);
        assert_eq!(freq.max_ceiling_hz, 5.0e9);
        assert_eq!(freq.max_hz, 1.5e9);
    }

    /// A missing ceiling on every core yields a zero ceiling (not a panic),
    /// while current frequencies still aggregate normally.
    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_missing_ceilings_yield_zero_ceiling() {
        let cores = [core(2_500_000, None), core(2_600_000, None)];
        let freq = compute_cpu_freq(&cores);
        assert_eq!(freq.max_ceiling_hz, 0.0);
        assert_eq!(freq.max_hz, 2.6e9);
        assert_eq!(freq.per_core.len(), 2);
    }

    /// The empty-input degrade case: no cores → default `CpuFreq` (empty
    /// per-core, zeroed aggregates) so a UI row can self-hide.
    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_empty_input_is_default() {
        let freq = compute_cpu_freq(&[]);
        assert!(freq.per_core.is_empty(), "no cores → empty per_core");
        assert_eq!(freq.max_hz, 0.0);
        assert_eq!(freq.max_ceiling_hz, 0.0);
    }

    /// `read_core_freq` parses a well-formed cpufreq directory (tempfile-backed,
    /// mirroring `hwmon.rs`), converting kHz → Hz downstream.
    #[test]
    #[allow(clippy::float_cmp)]
    fn read_core_freq_parses_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("scaling_cur_freq"), "2400000\n").unwrap();
        std::fs::write(dir.path().join("cpuinfo_max_freq"), "3600000\n").unwrap();

        let sample = read_core_freq(dir.path()).expect("core parses");
        assert_eq!(sample.cur_khz, 2_400_000);
        assert_eq!(sample.max_khz, Some(3_600_000));

        let freq = compute_cpu_freq(&[sample]);
        assert_eq!(freq.per_core, vec![2.4e9]);
        assert_eq!(freq.max_hz, 2.4e9);
        assert_eq!(freq.max_ceiling_hz, 3.6e9);
    }

    /// A core missing `scaling_cur_freq` is skipped (returns `None`); a missing
    /// `cpuinfo_max_freq` is tolerated (ceiling `None`).
    #[test]
    fn read_core_freq_handles_missing_files() {
        // No scaling_cur_freq at all → skip the core.
        let empty = tempfile::tempdir().expect("tempdir");
        assert!(read_core_freq(empty.path()).is_none());

        // Current present, ceiling absent → parses with max_khz = None.
        let partial = tempfile::tempdir().expect("tempdir");
        std::fs::write(partial.path().join("scaling_cur_freq"), "1800000\n").unwrap();
        let sample = read_core_freq(partial.path()).expect("core parses without ceiling");
        assert_eq!(sample.cur_khz, 1_800_000);
        assert_eq!(sample.max_khz, None);
    }

    /// A non-numeric `scaling_cur_freq` is treated as unreadable (skip the core)
    /// rather than panicking.
    #[test]
    fn read_core_freq_non_numeric_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("scaling_cur_freq"), "garbage\n").unwrap();
        assert!(read_core_freq(dir.path()).is_none());
    }
}
