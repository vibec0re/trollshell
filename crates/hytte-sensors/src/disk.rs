//! `/proc/self/mountinfo` parsing, disk usage via `statvfs`, and process
//! counting via `/proc`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::cast::{octal_byte_from_u32, u64_to_f64_count};

use super::{DiskMount, DiskUsage, MountSpec};

// ── /proc/self/mountinfo parsing ─────────────────────────────────────────────

/// Filesystems considered "pseudo" — kernel synthetic filesystems we never
/// want to show as a "disk". Matches the spirit of `findmnt --real`.
const PSEUDO_FSTYPES: &[&str] = &[
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "mqueue",
    "securityfs",
    "pstore",
    "bpf",
    "tracefs",
    "debugfs",
    "hugetlbfs",
    "configfs",
    "fusectl",
    "binfmt_misc",
    "autofs",
    "efivarfs",
    "ramfs",
    "rpc_pipefs",
    "nsfs",
    "selinuxfs",
    "overlay",
    "squashfs",
    // Userspace pseudo-fuse mounts: gvfs auto-mounts and Flatpak portals.
    // Real user fuse storage (sshfs, gocryptfs, etc.) uses other fuse.*
    // subtypes and stays visible.
    "fuse.gvfsd-fuse",
    "fuse.portal",
];

/// Decode `\NNN` octal escapes used by `/proc/self/mountinfo` for special
/// characters in mount-point paths (e.g. `\040` for space, `\134` for `\`,
/// `\011` for tab, `\012` for newline).
///
/// A backslash not followed by exactly three octal digits is preserved
/// verbatim — mountinfo only uses the `\NNN` form, so anything else is
/// either a literal backslash in a path or malformed input we leave alone.
pub(super) fn decode_octal_escapes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let is_octal = |c: u8| (b'0'..=b'7').contains(&c);
        if b == b'\\'
            && i + 3 < bytes.len()
            && is_octal(bytes[i + 1])
            && is_octal(bytes[i + 2])
            && is_octal(bytes[i + 3])
        {
            let v = u32::from(bytes[i + 1] - b'0') * 64
                + u32::from(bytes[i + 2] - b'0') * 8
                + u32::from(bytes[i + 3] - b'0');
            out.push(octal_byte_from_u32(v)); // mountinfo only emits \000–\377
            i += 4;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse one line of `/proc/self/mountinfo`.
///
/// Format (man `proc(5)` §5):
/// ```text
/// 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw
///  ^  ^  ^    ^     ^                          ^
///  1  2  3    4     5                          fstype (after " - ")
/// ```
///
/// Fields after position 6 and before the literal `" - "` separator are
/// optional tags (variable count) — we ignore them.
pub(super) fn parse_mountinfo_line(line: &str) -> Option<MountSpec> {
    let (left, right) = line.split_once(" - ")?;

    let mut left_fields = left.split_ascii_whitespace();
    // Skip fields 1 (mount ID) and 2 (parent ID).
    let _ = left_fields.next()?;
    let _ = left_fields.next()?;
    // Field 3: major:minor.
    let dev = left_fields.next()?;
    let (maj_s, min_s) = dev.split_once(':')?;
    let major: u32 = maj_s.parse().ok()?;
    let minor: u32 = min_s.parse().ok()?;
    // Skip field 4 (root inside fs).
    let _ = left_fields.next()?;
    // Field 5: mount point.
    let mount_point = left_fields.next()?;

    // Right half: fstype is the first whitespace-separated token.
    let fstype = right.split_ascii_whitespace().next()?.to_string();

    Some(MountSpec {
        path: decode_octal_escapes(mount_point),
        dev_id: (major, minor),
        fstype,
    })
}

/// Parse `/proc/self/mountinfo` text into a deduplicated, filtered list.
///
/// 1. Drops lines whose fstype is in [`PSEUDO_FSTYPES`].
/// 2. Dedups by `dev_id`, keeping the entry with the shortest path; ties
///    broken by mountinfo order.
/// 3. Preserves the original mountinfo order of the surviving entries.
pub(super) fn parse_mountinfo(text: &str) -> Vec<MountSpec> {
    let all: Vec<MountSpec> = text
        .lines()
        .filter_map(parse_mountinfo_line)
        .filter(|s| !PSEUDO_FSTYPES.contains(&s.fstype.as_str()))
        .collect();

    // Pick the winning index per dev_id (shortest path; first-seen wins ties).
    let mut winner_idx: HashMap<(u32, u32), usize> = HashMap::new();
    for (i, spec) in all.iter().enumerate() {
        winner_idx
            .entry(spec.dev_id)
            .and_modify(|j| {
                if spec.path.len() < all[*j].path.len() {
                    *j = i;
                }
            })
            .or_insert(i);
    }
    let winners: std::collections::HashSet<usize> = winner_idx.values().copied().collect();

    all.into_iter()
        .enumerate()
        .filter_map(|(i, s)| if winners.contains(&i) { Some(s) } else { None })
        .collect()
}

/// Read and parse the live `/proc/self/mountinfo`.
///
/// Returns an empty list on read failure (e.g. sandboxed runtime); the
/// caller's only failure mode in that case is reporting zero mounts.
#[must_use]
pub fn read_mountlist() -> Vec<MountSpec> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|t| parse_mountinfo(&t))
        .unwrap_or_default()
}

// ── Disk usage ────────────────────────────────────────────────────────────────

/// Filesystems whose `statvfs` can block **indefinitely**: network filesystems
/// whose server stopped answering (a stale NFS/CIFS handle sits in
/// uninterruptible sleep until the server returns). Any `fuse.*` subtype is
/// treated the same way, since a FUSE `statfs` is answered by a userspace
/// daemon that may itself be wedged (sshfs, rclone, ...).
///
/// These mounts are **kept in the list** (a user with a NAS mounted still
/// wants it on the Stats page) but their `statvfs` runs on a side thread
/// under [`STATVFS_DEADLINE`] instead of inline, so one dead mount costs that
/// mount its row for a tick and not the sampler its life. Local filesystems
/// are called inline: they do not hang, and a thread per mount per tick would
/// be pure overhead.
const BLOCKING_FSTYPES: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "9p",
    "afs",
    "ceph",
    "glusterfs",
    "lustre",
    "ncpfs",
];

/// How long one tick waits, in total, for the mounts that may block. Short
/// enough that a wedged mount is a one-second hiccup on a 1 Hz sampler, long
/// enough that a healthy NAS answering in tens of milliseconds never misses.
const STATVFS_DEADLINE: Duration = Duration::from_secs(1);

/// True for a filesystem whose `statvfs` is run under [`STATVFS_DEADLINE`].
fn may_block(fstype: &str) -> bool {
    // `fuse`, `fuse.<subtype>` and `fuseblk` (ntfs-3g, FUSE exfat).
    fstype.starts_with("fuse") || BLOCKING_FSTYPES.contains(&fstype)
}

/// How long the last good row of a side-thread mount keeps being served after
/// it was measured. A slow-but-alive NAS (say 1.5 s per `statvfs`) misses
/// [`STATVFS_DEADLINE`] every tick, so its row is always the previous call's
/// result - one or two ticks old. Disk usage moves slowly, so that is
/// harmless; 60 s is long enough to ride out a server stalling for a few
/// ticks without the row flickering away, and short enough that a mount that
/// has really died stops showing numbers within a minute.
const STALE_AFTER: Duration = Duration::from_mins(1);

/// Per-mount state of the side-thread `statvfs`, shared across ticks.
#[derive(Default)]
struct MountSlot {
    /// A call on this mount is still running. A stuck call never returns, so
    /// without this a new thread would be spawned every tick and pile up;
    /// instead the running call is reused and the last row served meanwhile.
    in_flight: bool,
    /// The most recent successful row and when it landed - written by the
    /// thread itself, so a result that arrives after its tick's deadline is
    /// still picked up by the next tick.
    last: Option<(DiskMount, Instant)>,
}

static SLOTS: Mutex<BTreeMap<String, MountSlot>> = Mutex::new(BTreeMap::new());

fn slots() -> std::sync::MutexGuard<'static, BTreeMap<String, MountSlot>> {
    SLOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Clears a mount's `in_flight` flag when dropped - including when `stat`
/// panics and unwinds - so a panicking call cannot skip the mount forever.
struct InFlight(String);

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(slot) = slots().get_mut(&self.0) {
            slot.in_flight = false;
        }
    }
}

/// `statvfs` one mount point into a row; `None` when the call fails.
fn stat_mount(path: &str) -> Option<DiskMount> {
    use nix::sys::statvfs::statvfs;
    let s = statvfs(path).ok()?;
    let block_size = s.fragment_size();
    let total = s.blocks() * block_size;
    let free = s.blocks_available() * block_size;
    let used = total.saturating_sub(free);
    let usage = if total == 0 {
        0.0
    } else {
        u64_to_f64_count(used) / u64_to_f64_count(total)
    };
    Some(DiskMount {
        path: path.to_string(),
        total_bytes: total,
        used_bytes: used,
        free_bytes: free,
        usage,
    })
}

#[must_use]
pub fn read_disk_for_specs(specs: &[MountSpec]) -> DiskUsage {
    read_disk_with(specs, STATVFS_DEADLINE, STALE_AFTER, stat_mount)
}

/// [`read_disk_for_specs`] over an injected `statvfs`, deadline and staleness
/// bound, so the slow and hung mount paths are testable with probes that sleep.
fn read_disk_with(
    specs: &[MountSpec],
    deadline: Duration,
    stale_after: Duration,
    stat: fn(&str) -> Option<DiskMount>,
) -> DiskUsage {
    // One slot per spec, so the original mount order survives the split into
    // inline and side-thread work.
    let mut rows: Vec<Option<DiskMount>> = vec![None; specs.len()];
    let (tx, rx) = mpsc::channel::<()>();
    let mut pending = 0usize;

    // Forget slots that can no longer serve a row (mount gone, or dead for
    // longer than the staleness bound) and are not mid-call.
    let t = Instant::now();
    slots().retain(|_, s| {
        s.in_flight
            || s.last
                .as_ref()
                .is_some_and(|(_, at)| t.saturating_duration_since(*at) <= STALE_AFTER)
    });

    for (i, spec) in specs.iter().enumerate() {
        if !may_block(&spec.fstype) {
            rows[i] = stat(&spec.path);
            continue;
        }
        // Reuse a call that is still running instead of spawning another; the
        // last good row is served for it below.
        let spawn_new = {
            let mut all = slots();
            let slot = all.entry(spec.path.clone()).or_default();
            !std::mem::replace(&mut slot.in_flight, true)
        };
        if !spawn_new {
            continue;
        }
        let path = spec.path.clone();
        let tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("statvfs".into())
            .spawn(move || {
                let _clear = InFlight(path.clone());
                let row = stat(&path);
                if let Some(slot) = slots().get_mut(&path) {
                    slot.last = row.map(|r| (r, Instant::now()));
                }
                // The receiver is gone if the deadline passed; the result is
                // already in the slot for the next tick.
                let _ = tx.send(());
            });
        if spawned.is_ok() {
            pending += 1;
        } else if let Some(slot) = slots().get_mut(&spec.path) {
            slot.in_flight = false;
        }
    }
    drop(tx);

    let end = Instant::now() + deadline;
    while pending > 0 {
        let Some(left) = end.checked_duration_since(Instant::now()) else {
            break;
        };
        if rx.recv_timeout(left).is_err() {
            break;
        }
        pending -= 1;
    }

    // Side-thread mounts: whatever each slot holds now - this tick's result if
    // it made the deadline, otherwise the previous call's, while it is fresh.
    let now = Instant::now();
    {
        let all = slots();
        for (i, spec) in specs.iter().enumerate() {
            if may_block(&spec.fstype)
                && let Some((row, at)) = all.get(&spec.path).and_then(|s| s.last.as_ref())
                && now.saturating_duration_since(*at) <= stale_after
            {
                rows[i] = Some(row.clone());
            }
        }
    }

    DiskUsage {
        mounts: rows.into_iter().flatten().collect(),
    }
}

// ── Process count ─────────────────────────────────────────────────────────────

#[must_use]
pub fn read_process_count() -> u32 {
    std::fs::read_dir("/proc").map_or(0, |iter| {
        let count: usize = iter
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.parse::<u32>().is_ok())
            })
            .count();
        // `count` is a usize entry count; TryFrom catches the usize > u32::MAX
        // edge case (> 4 billion processes) and saturates to u32::MAX — safe.
        count.try_into().unwrap_or(u32::MAX)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_octal_escapes_passthrough() {
        assert_eq!(decode_octal_escapes("/home/choom"), "/home/choom");
        assert_eq!(decode_octal_escapes(""), "");
    }

    #[test]
    fn decode_octal_escapes_decodes_space() {
        // \040 = octal 40 = decimal 32 = ASCII space
        assert_eq!(decode_octal_escapes("/mnt/My\\040Drive"), "/mnt/My Drive");
    }

    #[test]
    fn decode_octal_escapes_decodes_tab_and_backslash() {
        // \011 = tab, \134 = backslash
        assert_eq!(decode_octal_escapes("a\\011b"), "a\tb");
        assert_eq!(decode_octal_escapes("a\\134b"), "a\\b");
    }

    #[test]
    fn decode_octal_escapes_preserves_lone_or_invalid_backslash() {
        // Backslash not followed by 3 octal digits is preserved verbatim.
        assert_eq!(decode_octal_escapes("/foo\\bar"), "/foo\\bar");
        assert_eq!(decode_octal_escapes("/foo\\12"), "/foo\\12");
        assert_eq!(decode_octal_escapes("/foo\\99x"), "/foo\\99x");
    }

    #[test]
    fn parse_mountinfo_line_basic() {
        let line = "36 35 98:0 / /mnt rw,noatime - ext3 /dev/root rw";
        let spec = parse_mountinfo_line(line).expect("parse");
        assert_eq!(spec.dev_id, (98, 0));
        assert_eq!(spec.path, "/mnt");
        assert_eq!(spec.fstype, "ext3");
    }

    #[test]
    fn parse_mountinfo_line_with_optional_tags() {
        // mountinfo lines may carry zero or more optional tag fields between
        // field 6 and the literal " - " separator.
        let line = "26 1 8:1 / / rw,relatime shared:1 master:2 - btrfs /dev/sda1 rw";
        let spec = parse_mountinfo_line(line).expect("parse");
        assert_eq!(spec.dev_id, (8, 1));
        assert_eq!(spec.path, "/");
        assert_eq!(spec.fstype, "btrfs");
    }

    #[test]
    fn parse_mountinfo_line_octal_path() {
        let line = "1 1 8:1 / /mnt/My\\040Drive rw - ext4 /dev/sda1 rw";
        let spec = parse_mountinfo_line(line).expect("parse");
        assert_eq!(spec.path, "/mnt/My Drive");
    }

    #[test]
    fn parse_mountinfo_line_malformed_returns_none() {
        assert!(parse_mountinfo_line("not a real line").is_none());
        assert!(
            parse_mountinfo_line("36 35 noslash / /mnt rw - ext3 /dev/root rw").is_none(),
            "missing colon in field 3 should fail",
        );
        assert!(
            parse_mountinfo_line("36 35 98:0 / /mnt rw").is_none(),
            "missing ' - ' separator should fail",
        );
    }

    #[test]
    fn parse_mountinfo_filters_pseudo() {
        let text = "\
1 0 0:1 / /proc rw - proc proc rw
2 0 0:2 / /sys rw - sysfs sys rw
3 0 0:3 / /tmp rw - tmpfs none rw
4 0 8:1 / /home rw - ext4 /dev/sda1 rw
5 0 8:2 / /data rw - btrfs /dev/sdb1 rw
";
        let v = parse_mountinfo(text);
        assert_eq!(v.len(), 2, "only ext4 and btrfs should survive");
        assert_eq!(v[0].path, "/home");
        assert_eq!(v[0].fstype, "ext4");
        assert_eq!(v[1].path, "/data");
        assert_eq!(v[1].fstype, "btrfs");
    }

    #[test]
    fn parse_mountinfo_dedups_by_dev_id_keeping_shortest_path() {
        // Two ext4 entries on the same major:minor — should collapse into
        // one, with the shorter path winning. A separate btrfs on a
        // different major:minor survives independently.
        let text = "\
1 0 8:1 /a /run/host/os-release rw - ext4 /dev/sda1 rw
2 0 8:1 / / rw - ext4 /dev/sda1 rw
3 0 8:2 / /home rw - btrfs /dev/sdb1 rw
";
        let v = parse_mountinfo(text);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].path, "/", "shortest path wins for dev (8,1)");
        assert_eq!(v[1].path, "/home");
    }

    #[test]
    fn parse_mountinfo_skips_malformed_lines() {
        let text = "\
1 0 8:1 / / rw - ext4 /dev/sda1 rw
not a real line at all
2 0 8:2 / /home rw - btrfs /dev/sdb1 rw
";
        let v = parse_mountinfo(text);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].path, "/");
        assert_eq!(v[1].path, "/home");
    }

    #[test]
    fn parse_mountinfo_filters_pseudo_fuse_mounts() {
        // gvfs and Flatpak portal fuse mounts are pseudo and should be
        // filtered. Real user fuse storage (e.g. fuse.sshfs) survives.
        let text = "\
1 0 0:50 / /run/user/1000/gvfs rw - fuse.gvfsd-fuse gvfsd-fuse rw
2 0 0:51 / /run/user/1000/doc rw - fuse.portal portal rw
3 0 0:52 / /mnt/server rw - fuse.sshfs user@host:/ rw
4 0 8:1 / / rw - ext4 /dev/sda1 rw
";
        let v = parse_mountinfo(text);
        assert_eq!(
            v.len(),
            2,
            "fuse.sshfs + ext4 survive; gvfs + portal filtered"
        );
        assert_eq!(v[0].path, "/mnt/server");
        assert_eq!(v[0].fstype, "fuse.sshfs");
        assert_eq!(v[1].path, "/");
        assert_eq!(v[1].fstype, "ext4");
    }

    fn spec(path: &str, fstype: &str) -> MountSpec {
        MountSpec {
            path: path.to_string(),
            dev_id: (0, 0),
            fstype: fstype.to_string(),
        }
    }

    fn fake_stat(path: &str) -> Option<DiskMount> {
        if path == "/test-hung-mount" {
            std::thread::sleep(Duration::from_secs(4));
        }
        if path == "/test-slow-mount" {
            // 1.5x the 300 ms deadline the tests use: slow but alive.
            std::thread::sleep(Duration::from_millis(450));
        }
        assert!(path != "/test-panic-mount", "stat blew up");
        if path == "/test-unreadable" {
            return None;
        }
        Some(DiskMount {
            path: path.to_string(),
            total_bytes: 100,
            used_bytes: 40,
            free_bytes: 60,
            usage: 0.4,
        })
    }

    #[test]
    fn network_and_fuse_filesystems_are_the_ones_that_may_block() {
        for t in [
            "nfs",
            "nfs4",
            "cifs",
            "smb3",
            "9p",
            "fuse.sshfs",
            "fuse",
            "fuseblk",
        ] {
            assert!(may_block(t), "{t}");
        }
        for t in ["ext4", "btrfs", "xfs", "vfat", "zfs", "f2fs"] {
            assert!(!may_block(t), "{t}");
        }
    }

    #[test]
    fn a_hung_remote_mount_loses_its_row_not_the_whole_tick() {
        let specs = [
            spec("/", "ext4"),
            spec("/test-hung-mount", "nfs4"),
            spec("/home", "btrfs"),
            spec("/test-unreadable", "ext4"),
            spec("/mnt/healthy-nas", "cifs"),
        ];
        let started = Instant::now();
        let usage = read_disk_with(&specs, DL, STALE, fake_stat);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the deadline did not bound the read"
        );
        let paths: Vec<&str> = usage.mounts.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(
            paths,
            ["/", "/home", "/mnt/healthy-nas"],
            "healthy mounts (remote included) keep their rows, in mountinfo order"
        );
        // While the stuck call is outstanding the mount is skipped, not
        // re-spawned onto yet another thread.
        assert!(slots()["/test-hung-mount"].in_flight);
        let again = read_disk_with(&specs[1..2], Duration::from_millis(50), STALE, fake_stat);
        assert!(again.mounts.is_empty());
    }

    const DL: Duration = Duration::from_millis(300);
    const STALE: Duration = Duration::from_mins(1);

    /// The regression this pins: a NAS that answers in 1.5x the deadline used
    /// to miss it every tick, send its result to a dropped receiver and never
    /// show. The late result now lands in the slot and the next tick shows it.
    #[test]
    fn a_slow_but_alive_mount_shows_on_the_next_tick() {
        let specs = [spec("/test-slow-mount", "nfs4")];
        let first = read_disk_with(&specs, DL, STALE, fake_stat);
        assert!(first.mounts.is_empty(), "nothing measured yet on tick 1");
        // Tick 2 (just after the slow call completes): its result is served
        // and, the call being done, a fresh one is started.
        std::thread::sleep(Duration::from_millis(400));
        let second = read_disk_with(&specs, DL, STALE, fake_stat);
        assert_eq!(second.mounts.len(), 1, "the late result never landed");
        assert_eq!(second.mounts[0].path, "/test-slow-mount");
        // Tick 3 lands inside the second call: no new thread, last row kept.
        let third = read_disk_with(&specs, Duration::from_millis(10), STALE, fake_stat);
        assert_eq!(third.mounts.len(), 1, "row dropped while a call is running");
    }

    #[test]
    fn a_row_that_stops_refreshing_goes_stale() {
        let specs = [spec("/test-stale-mount", "cifs")];
        let _ = read_disk_with(&specs, DL, Duration::from_millis(100), fake_stat);
        assert_eq!(
            read_disk_with(&specs, DL, Duration::from_millis(100), fake_stat)
                .mounts
                .len(),
            1
        );
        // The mount dies: its last row ages past the bound and is dropped.
        let slot_age = Duration::from_millis(150);
        std::thread::sleep(slot_age);
        let gone = read_disk_with(
            &specs,
            Duration::from_millis(1),
            Duration::from_millis(100),
            |p| {
                std::thread::sleep(Duration::from_secs(2));
                fake_stat(p)
            },
        );
        assert!(gone.mounts.is_empty(), "served a row past the bound");
    }

    #[test]
    fn a_panicking_stat_does_not_skip_the_mount_forever() {
        let specs = [spec("/test-panic-mount", "nfs4")];
        let _ = read_disk_with(&specs, DL, STALE, fake_stat);
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !slots()
                .get("/test-panic-mount")
                .is_some_and(|s| s.in_flight),
            "the in-flight flag survived a panic"
        );
    }
}
