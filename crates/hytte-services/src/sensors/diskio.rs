//! `/proc/diskstats` parsing — physical-disk read/write throughput.
//!
//! Mirrors the `/proc/net/dev` path (`net.rs` + `apply_network`): read the
//! cumulative byte counters, diff them against the previous sample over `dt`,
//! and sum across physical whole-disk block devices to produce an aggregate
//! read/write **rate** plus the **cumulative totals since boot**.

use std::collections::HashMap;
use std::time::Instant;

use crate::cast::u64_to_f64_bytes;

use super::DiskIo;

/// A disk sector is always 512 bytes for the purposes of `/proc/diskstats`,
/// regardless of the device's physical/logical sector size (see
/// `Documentation/admin-guide/iostats.rst`).
const SECTOR_BYTES: u64 = 512;

// ── /proc/diskstats parsing ───────────────────────────────────────────────────

/// Read `/proc/diskstats` and return `(name, read_bytes, write_bytes)` —
/// cumulative-since-boot byte counters — for every **physical whole-disk**
/// block device. Partitions, loop/ram/zram/device-mapper and other virtual
/// devices are filtered out (see [`is_physical_disk`]).
pub(super) fn read_proc_diskstats() -> Result<Vec<(String, u64, u64)>, std::io::Error> {
    let text = std::fs::read_to_string("/proc/diskstats")?;
    Ok(parse_diskstats(&text))
}

/// Parse `/proc/diskstats` text into physical-disk cumulative byte counters.
///
/// Field layout (1-indexed, per `Documentation/admin-guide/iostats.rst`):
/// `1` major, `2` minor, `3` name, `4` reads completed, `5` reads merged,
/// **`6` sectors read**, `7` ms reading, `8` writes completed, `9` writes
/// merged, **`10` sectors written**, … — i.e. 0-indexed `[5]` and `[9]`.
/// Sectors are ×512 = bytes.
fn parse_diskstats(text: &str) -> Vec<(String, u64, u64)> {
    let mut result = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        // Need at least through the sectors-written field (0-indexed 9).
        if fields.len() < 10 {
            continue;
        }
        let name = fields[2];
        if !is_physical_disk(name) {
            continue;
        }
        let sectors_read: u64 = fields[5].parse().unwrap_or(0);
        let sectors_written: u64 = fields[9].parse().unwrap_or(0);
        result.push((
            name.to_string(),
            sectors_read.saturating_mul(SECTOR_BYTES),
            sectors_written.saturating_mul(SECTOR_BYTES),
        ));
    }
    result
}

/// True only for **physical whole-disk** block device names — the real disks
/// whose throughput we want to graph. Partitions, loop/ram/zram/dm/md/optical
/// and every other synthetic device are rejected by returning `false`.
///
/// Classifying purely from the name (so the filter stays hermetically testable)
/// is only reliable per device *class*, because e.g. `sda1` and `mmcblk0` both
/// end in "letter-then-digit" yet one is a partition and the other a whole
/// disk. We therefore match the known whole-disk patterns explicitly:
///
/// - `sd*` / `hd*` / `vd*` / `xvd*` — SCSI/SATA/USB, IDE, virtio, Xen: prefix
///   followed by one-or-more letters and **no** trailing digits (`sda`, not
///   `sda1`).
/// - `nvme<ctrl>n<ns>` — `NVMe` namespace whole disk (`nvme0n1`); the partition
///   form `nvme0n1p1` has a trailing `p<digits>` and is rejected.
/// - `mmcblk<N>` — eMMC/SD whole disk (`mmcblk0`); `mmcblk0p1` is a partition.
fn is_physical_disk(name: &str) -> bool {
    // SCSI/SATA/USB, IDE, virtio, Xen: <prefix> + letters, no trailing digits.
    // `xvd` must be tried before `vd`/`hd`/`sd` so it isn't shadowed.
    for prefix in ["xvd", "sd", "hd", "vd"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_lowercase());
        }
    }
    // NVMe: nvme<digits>n<digits> whole disk; nvme<..>p<digits> is a partition.
    if let Some(rest) = name.strip_prefix("nvme") {
        return is_nvme_namespace(rest);
    }
    // eMMC/SD: mmcblk<digits> whole disk; mmcblk<digits>p<digits> a partition.
    if let Some(rest) = name.strip_prefix("mmcblk") {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit());
    }
    false
}

/// True if `rest` (the part after the `nvme` prefix) is a bare namespace
/// (`<digits>n<digits>`) rather than a partition (`<digits>n<digits>p<digits>`).
fn is_nvme_namespace(rest: &str) -> bool {
    let Some((ctrl, ns)) = rest.split_once('n') else {
        return false;
    };
    !ctrl.is_empty()
        && ctrl.bytes().all(|b| b.is_ascii_digit())
        && !ns.is_empty()
        && ns.bytes().all(|b| b.is_ascii_digit())
}

// ── Rate computation ──────────────────────────────────────────────────────────

/// Compute the aggregate [`DiskIo`] snapshot from the current cumulative
/// per-device byte counters and the previous sample map.
///
/// Mirrors `apply_network`: per device, diff the cumulative counters against
/// the previous sample over `dt` to get a byte/sec rate, then **sum** the rates
/// across physical disks (the aggregate default — one combined series, like the
/// network row's rx+tx). Totals-since-boot are the summed raw cumulative
/// counters. Returns the snapshot plus the next prev-map to store.
pub(super) fn compute_disk_io(
    prev: &HashMap<String, (u64, u64, Instant)>,
    devices: Vec<(String, u64, u64)>,
    now: Instant,
) -> (DiskIo, HashMap<String, (u64, u64, Instant)>) {
    let mut read_bps = 0.0;
    let mut write_bps = 0.0;
    let mut total_read_bytes: u64 = 0;
    let mut total_write_bytes: u64 = 0;
    let mut next = HashMap::with_capacity(devices.len());

    for (name, read_total, write_total) in devices {
        if let Some((prev_read, prev_write, prev_when)) = prev.get(&name) {
            let dt = now.duration_since(*prev_when).as_secs_f64().max(0.1);
            read_bps += u64_to_f64_bytes(read_total.saturating_sub(*prev_read)) / dt;
            write_bps += u64_to_f64_bytes(write_total.saturating_sub(*prev_write)) / dt;
        }
        total_read_bytes = total_read_bytes.saturating_add(read_total);
        total_write_bytes = total_write_bytes.saturating_add(write_total);
        next.insert(name, (read_total, write_total, now));
    }

    (
        DiskIo {
            read_bps,
            write_bps,
            total_read_bytes,
            total_write_bytes,
        },
        next,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── is_physical_disk ──────────────────────────────────────────────────────

    #[test]
    fn keeps_whole_disks() {
        for name in [
            "sda", "sdb", "sdaa", "hda", "vda", "xvda", "nvme0n1", "nvme1n1", "mmcblk0",
        ] {
            assert!(is_physical_disk(name), "{name} should be kept");
        }
    }

    #[test]
    fn drops_partitions() {
        for name in [
            "sda1",
            "sdb2",
            "hda1",
            "vda1",
            "xvda1",
            "nvme0n1p1",
            "nvme1n1p2",
            "mmcblk0p1",
        ] {
            assert!(!is_physical_disk(name), "{name} is a partition; must drop");
        }
    }

    #[test]
    fn drops_virtual_devices() {
        for name in [
            "loop0", "loop7", "ram0", "zram0", "dm-0", "md0", "sr0", "nbd0", "fd0",
        ] {
            assert!(!is_physical_disk(name), "{name} is virtual; must drop");
        }
    }

    // ── parse_diskstats ───────────────────────────────────────────────────────

    #[test]
    fn parse_extracts_read_write_bytes() {
        // sectors read = 1000 (idx 5), sectors written = 2000 (idx 9).
        // ×512 → 512_000 read bytes, 1_024_000 written bytes.
        let text = "   8       0 sda 50 0 1000 40 60 0 2000 30 0 100 200";
        let v = parse_diskstats(text);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "sda");
        assert_eq!(v[0].1, 1000 * 512);
        assert_eq!(v[0].2, 2000 * 512);
    }

    #[test]
    fn parse_filters_partitions_and_virtual() {
        // Mirrors a real /proc/diskstats: whole disks + partitions + loop/ram.
        let text = "\
 259       0 nvme0n1 10 0 100 5 0 0 200 0 0 1 2
 259       1 nvme0n1p1 5 0 40 2 0 0 10 0 0 1 2
 259       2 nvme0n1p2 5 0 60 3 0 0 190 0 0 1 2
   8       0 sda 20 0 300 8 0 0 400 0 0 1 2
   8       1 sda1 20 0 300 8 0 0 400 0 0 1 2
   7       0 loop0 0 0 0 0 0 0 0 0 0 0 0
   1       0 ram0 0 0 0 0 0 0 0 0 0 0 0
";
        let v = parse_diskstats(text);
        let names: Vec<&str> = v.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, vec!["nvme0n1", "sda"], "only whole disks survive");
    }

    #[test]
    fn parse_short_line_skipped_without_panic() {
        // Fewer than 10 fields (truncated line) must be skipped, not panic.
        let text = "   8       0 sda 50 0 1000";
        assert!(parse_diskstats(text).is_empty());
    }

    #[test]
    fn parse_non_numeric_sectors_fall_back_to_zero() {
        let text = "   8       0 sda 50 0 xxx 40 60 0 yyy 30 0 100 200";
        let v = parse_diskstats(text);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].1, 0, "unparseable sectors read → 0, not panic");
        assert_eq!(v[0].2, 0);
    }

    // ── compute_disk_io (rate diff) ───────────────────────────────────────────

    #[test]
    #[allow(clippy::float_cmp)]
    fn rate_is_bytes_delta_over_dt() {
        let now = Instant::now();
        let earlier = now
            .checked_sub(Duration::from_secs(1))
            .expect("test Instant underflow");
        let mut prev = HashMap::new();
        // prev cumulative: 0 read, 0 written a second ago.
        prev.insert("sda".to_string(), (0u64, 0u64, earlier));
        // now cumulative: 512_000 read, 1_024_000 written.
        let (io, next) = compute_disk_io(&prev, vec![("sda".into(), 512_000, 1_024_000)], now);
        // dt is exactly 1s (constructed), so rate == delta.
        assert_eq!(io.read_bps, 512_000.0);
        assert_eq!(io.write_bps, 1_024_000.0);
        assert_eq!(io.total_read_bytes, 512_000);
        assert_eq!(io.total_write_bytes, 1_024_000);
        assert!(next.contains_key("sda"));
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn rates_sum_across_disks() {
        let now = Instant::now();
        let earlier = now
            .checked_sub(Duration::from_secs(1))
            .expect("test Instant underflow");
        let mut prev = HashMap::new();
        prev.insert("sda".to_string(), (0u64, 0u64, earlier));
        prev.insert("sdb".to_string(), (0u64, 0u64, earlier));
        let (io, _) = compute_disk_io(
            &prev,
            vec![("sda".into(), 1000, 2000), ("sdb".into(), 3000, 4000)],
            now,
        );
        assert_eq!(io.read_bps, 4000.0, "read rate sums across disks");
        assert_eq!(io.write_bps, 6000.0, "write rate sums across disks");
        assert_eq!(io.total_read_bytes, 4000);
        assert_eq!(io.total_write_bytes, 6000);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn first_sample_has_zero_rate_but_counts_totals() {
        // No prev entry → the device contributes 0 to the rate but its
        // cumulative counters still count toward totals-since-boot.
        let now = Instant::now();
        let (io, _) = compute_disk_io(&HashMap::new(), vec![("sda".into(), 999, 888)], now);
        assert_eq!(io.read_bps, 0.0);
        assert_eq!(io.write_bps, 0.0);
        assert_eq!(io.total_read_bytes, 999);
        assert_eq!(io.total_write_bytes, 888);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn counter_reset_saturates_to_zero_rate() {
        // If cumulative counters go backwards (device hot-swap), the diff must
        // saturate to 0 rather than underflow.
        let now = Instant::now();
        let earlier = now
            .checked_sub(Duration::from_secs(1))
            .expect("test Instant underflow");
        let mut prev = HashMap::new();
        prev.insert("sda".to_string(), (10_000u64, 10_000u64, earlier));
        let (io, _) = compute_disk_io(&prev, vec![("sda".into(), 500, 500)], now);
        assert_eq!(io.read_bps, 0.0);
        assert_eq!(io.write_bps, 0.0);
    }
}
