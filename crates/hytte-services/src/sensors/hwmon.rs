//! CPU package temperature via `/sys/class/hwmon`.

use std::path::{Path, PathBuf};

use crate::cast::millicelsius_to_celsius;

use super::CpuTemp;

/// Read the package CPU temperature. The `/sys/class/hwmon` chip directory is
/// resolved once (a `read_dir` walk + a `name` read per hwmon entry) and
/// cached in `chip`; subsequent ticks re-read only the cached chip's
/// `temp*_input` files. If the cached chip stops yielding a reading (module
/// reload / hotplug) the cache is dropped and re-resolved next call.
pub(super) fn read_cpu_temp(chip: &mut Option<PathBuf>) -> CpuTemp {
    // Fast path: the chip directory is already known.
    if let Some(dir) = chip.as_deref() {
        if let Some(celsius) = read_chip_temp(dir) {
            return CpuTemp {
                package_celsius: Some(celsius),
            };
        }
        *chip = None; // chip vanished — fall through and re-resolve
    }

    // Slow path: find a preferred CPU sensor chip and cache its directory.
    let Ok(hwmon) = std::fs::read_dir("/sys/class/hwmon") else {
        return CpuTemp::default();
    };
    let preferred_names: &[&str] = &["coretemp", "k10temp", "zenpower", "asusec"];
    for entry in hwmon.flatten() {
        let dir = entry.path();
        let Ok(name) = std::fs::read_to_string(dir.join("name")) else {
            continue;
        };
        if !preferred_names.contains(&name.trim()) {
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

    #[test]
    fn read_chip_temp_missing_or_sensorless_is_none() {
        assert_eq!(read_chip_temp(Path::new("/nonexistent/hwmon-x")), None);
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_chip_temp(empty.path()), None);
    }
}
