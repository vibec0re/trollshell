//! CPU package temperature via `/sys/class/hwmon`.

use std::path::{Path, PathBuf};

use crate::cast::millicelsius_to_celsius;

use super::CpuTemp;

/// hwmon chip `name`s that carry the CPU package temperature.
const PREFERRED_NAMES: &[&str] = &["coretemp", "k10temp", "zenpower", "asusec"];

/// True when `dir/name` still names a CPU sensor chip. The `hwmonN` index is
/// assigned at probe time, so after a module reload or a suspend/resume the
/// same path can be a different chip (an `nvme`, a `nouveau`) that happily
/// has `temp*_input` files too.
fn is_preferred_chip(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("name")).is_ok_and(|n| PREFERRED_NAMES.contains(&n.trim()))
}

/// Read the package CPU temperature. The `/sys/class/hwmon` chip directory is
/// resolved once (a `read_dir` walk + a `name` read per hwmon entry) and
/// cached in `chip`; subsequent ticks re-read the cached chip's `name` (one
/// tiny read, to prove the index was not reassigned) and its `temp*_input`
/// files. If the cached chip stops yielding a reading or is no longer a CPU
/// sensor (module reload / hotplug / resume) the cache is dropped and
/// re-resolved in the same call.
pub fn read_cpu_temp(chip: &mut Option<PathBuf>) -> CpuTemp {
    read_cpu_temp_in(chip, Path::new("/sys/class/hwmon"))
}

fn read_cpu_temp_in(chip: &mut Option<PathBuf>, hwmon_root: &Path) -> CpuTemp {
    // Fast path: the chip directory is already known.
    if let Some(dir) = chip.as_deref() {
        if is_preferred_chip(dir)
            && let Some(celsius) = read_chip_temp(dir)
        {
            return CpuTemp {
                package_celsius: Some(celsius),
            };
        }
        *chip = None; // chip vanished or was renumbered — re-resolve
    }

    // Slow path: find a preferred CPU sensor chip and cache its directory.
    let Ok(hwmon) = std::fs::read_dir(hwmon_root) else {
        return CpuTemp::default();
    };
    for entry in hwmon.flatten() {
        let dir = entry.path();
        if !is_preferred_chip(&dir) {
            continue;
        }
        if let Some(celsius) = read_chip_temp(&dir) {
            *chip = Some(dir);
            return CpuTemp {
                package_celsius: Some(celsius),
            };
        }
    }
    CpuTemp::default()
}

/// Highest `temp*_input` value (milli-°C) under a resolved hwmon chip
/// directory, converted to °C. `None` if the directory is unreadable or has
/// no `temp*_input` sensors.
pub(super) fn read_chip_temp(dir: &Path) -> Option<f64> {
    let mut max_milli: Option<u64> = None;
    for f in std::fs::read_dir(dir).ok()?.flatten() {
        let fname = f.file_name();
        let Some(name) = fname.to_str() else {
            continue;
        };
        if !name.starts_with("temp") || !name.ends_with("_input") {
            continue;
        }
        if let Ok(s) = std::fs::read_to_string(f.path())
            && let Ok(v) = s.trim().parse::<u64>()
        {
            max_milli = Some(max_milli.map_or(v, |cur| cur.max(v)));
        }
    }
    max_milli.map(millicelsius_to_celsius)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    #[allow(clippy::float_cmp)]
    fn read_chip_temp_takes_max_input_in_celsius() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("temp1_input"), "45000\n").unwrap();
        std::fs::write(dir.path().join("temp2_input"), "52000\n").unwrap();
        std::fs::write(dir.path().join("temp1_label"), "Package\n").unwrap(); // not *_input
        std::fs::write(dir.path().join("name"), "coretemp\n").unwrap(); // ignored here
        assert_eq!(read_chip_temp(dir.path()), Some(52.0));
    }

    fn chip(root: &Path, dir: &str, name: &str, milli: u64) {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("name"), format!("{name}\n")).unwrap();
        std::fs::write(d.join("temp1_input"), format!("{milli}\n")).unwrap();
    }

    /// After a module reload the cached `hwmon1` is an `NVMe` drive and the CPU
    /// sensor moved to `hwmon3`: the cached path must not be trusted.
    fn assert_celsius(t: CpuTemp, want: f64) {
        let got = t.package_celsius.expect("a reading");
        assert!((got - want).abs() < 1e-9, "got {got}, want {want}");
    }

    #[test]
    fn a_renumbered_hwmon_index_is_re_resolved() {
        let root = tempfile::tempdir().expect("tempdir");
        chip(root.path(), "hwmon1", "k10temp", 50_000);
        let mut cached = None;
        assert_celsius(read_cpu_temp_in(&mut cached, root.path()), 50.0);
        assert_eq!(
            cached.as_deref(),
            Some(root.path().join("hwmon1").as_path())
        );

        // Indices reassigned: hwmon1 is now the NVMe (hotter), CPU is hwmon3.
        chip(root.path(), "hwmon1", "nvme", 70_000);
        chip(root.path(), "hwmon3", "k10temp", 55_000);
        // Reading the NVMe's 70 C as the CPU's is the bug this pins.
        assert_celsius(read_cpu_temp_in(&mut cached, root.path()), 55.0);
        assert_eq!(
            cached.as_deref(),
            Some(root.path().join("hwmon3").as_path())
        );
    }

    #[test]
    fn a_stable_cached_chip_stays_on_the_fast_path() {
        let root = tempfile::tempdir().expect("tempdir");
        chip(root.path(), "hwmon0", "coretemp", 41_000);
        let mut cached = None;
        let _ = read_cpu_temp_in(&mut cached, root.path());
        chip(root.path(), "hwmon0", "coretemp", 43_000);
        assert_celsius(read_cpu_temp_in(&mut cached, root.path()), 43.0);
        assert_eq!(
            cached.as_deref(),
            Some(root.path().join("hwmon0").as_path())
        );
    }

    #[test]
    fn read_chip_temp_missing_or_sensorless_is_none() {
        assert_eq!(read_chip_temp(Path::new("/nonexistent/hwmon-x")), None);
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_chip_temp(empty.path()), None);
    }
}
