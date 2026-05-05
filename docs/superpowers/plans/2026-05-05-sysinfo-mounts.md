# Sysinfo Dynamic Mount Discovery — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the hardcoded `["/", "/home"]` mount list in `sensors::disk()` with a dynamic list from `/proc/self/mountinfo`, watched for changes via POLLPRI, with pseudo-fs filtering and `major:minor` dedup.

**Architecture:** Two cooperating pieces inside `crates/hytte-services/src/sensors.rs`. (a) An event-driven `mount_watch_loop` that opens `/proc/self/mountinfo`, wraps the fd in `tokio::io::unix::AsyncFd` with `Interest::PRIORITY`, seeds a shared `Mutable<Vec<MountSpec>>`, then re-reads on each POLLPRI event. (b) The existing `poll_loop`'s 5-tick disk branch reads from that Mutable and runs `statvfs` per mount. Public types (`DiskUsage`, `DiskMount`, `sensors::disk()`) are unchanged — UI consumers (`widgets/disk.rs`, `panels/stats.rs`) keep working untouched.

**Tech Stack:** Rust 2021, `tokio` 1.52 (`net` feature for `AsyncFd`), `nix::sys::statvfs`, `futures-signals::Mutable`, `tracing`.

**Spec:** [`docs/superpowers/specs/2026-05-05-sysinfo-mounts-design.md`](../specs/2026-05-05-sysinfo-mounts-design.md)

---

## File Structure

**Modified:**
- `crates/hytte-services/Cargo.toml` — add `"net"` to tokio features.
- `crates/hytte-services/src/sensors.rs` — single-file change. Adds: `MountSpec`, `PSEUDO_FSTYPES` const, `decode_octal_escapes`, `parse_mountinfo_line`, `parse_mountinfo`, `read_mountlist`, `mount_watch_loop`, `read_disk_for_specs`, `mount_list` field on `SensorsHandles`. Removes: `read_disk(&[&str])`.

**No other files change.**

---

## Task 1: Enable tokio `net` feature

`tokio::io::unix::AsyncFd` is gated behind the `net` cargo feature; the watcher task needs it. The crate currently has `["rt", "io-util", "process", "sync", "time"]`.

**Files:**
- Modify: `crates/hytte-services/Cargo.toml:23`

- [ ] **Step 1: Edit the tokio dep line**

Change line 23 from:

```toml
tokio = { version = "1.52.1", features = ["rt", "io-util", "process", "sync", "time"] }
```

to:

```toml
tokio = { version = "1.52.1", features = ["net", "rt", "io-util", "process", "sync", "time"] }
```

- [ ] **Step 2: Verify the crate still builds**

Run: `cargo build -p hytte-services`
Expected: clean build, no errors.

- [ ] **Step 3: Commit**

```bash
git add crates/hytte-services/Cargo.toml
git commit -m "chore(services): enable tokio net feature for AsyncFd"
```

---

## Task 2: Add `MountSpec`, denylist const, and octal-escape decoder (TDD)

These three pieces are pure helpers needed by the parser. Write tests first for `decode_octal_escapes` (the only one that has interesting behavior to test in isolation).

**Files:**
- Modify: `crates/hytte-services/src/sensors.rs` — add a new section "── /proc/self/mountinfo parsing ──" near the existing "── Disk usage ──" section (just *before* it, around line 753).
- Test: same file's existing `#[cfg(test)] mod tests` block (around line 798).

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `sensors.rs` (just before the closing `}` of `mod tests`):

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p hytte-services sensors::tests::decode_octal -- --nocapture`
Expected: FAIL with "cannot find function `decode_octal_escapes`".

- [ ] **Step 3: Add the new section, types, const, and decoder**

Insert the following block in `sensors.rs` immediately before the `// ── Disk usage ──` line (currently at line 753):

```rust
// ── /proc/self/mountinfo parsing ─────────────────────────────────────────────

/// Internal representation of one mounted filesystem.
///
/// Not part of the public sensors API; consumed only by the disk poller.
#[derive(Clone, Debug)]
struct MountSpec {
    /// Mount point (mountinfo field 5), with octal escapes decoded.
    path: String,
    /// `(major, minor)` from mountinfo field 3 — used for dedup.
    dev_id: (u32, u32),
    /// fstype (right-half token 1) — diagnostic only.
    #[allow(dead_code)]
    fstype: String,
}

/// Filesystems considered "pseudo" — kernel synthetic filesystems we never
/// want to show as a "disk". Matches the spirit of `findmnt --real`.
const PSEUDO_FSTYPES: &[&str] = &[
    "proc", "sysfs", "cgroup", "cgroup2", "devtmpfs", "devpts", "tmpfs",
    "mqueue", "securityfs", "pstore", "bpf", "tracefs", "debugfs",
    "hugetlbfs", "configfs", "fusectl", "binfmt_misc", "autofs",
    "efivarfs", "ramfs", "rpc_pipefs", "nsfs", "selinuxfs", "overlay",
    "squashfs",
];

/// Decode `\NNN` octal escapes used by `/proc/self/mountinfo` for special
/// characters in mount-point paths (e.g. `\040` for space, `\134` for `\`,
/// `\011` for tab, `\012` for newline).
///
/// A backslash not followed by exactly three octal digits is preserved
/// verbatim — mountinfo only uses the `\NNN` form, so anything else is
/// either a literal backslash in a path or malformed input we leave alone.
fn decode_octal_escapes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let is_octal = |c: u8| c >= b'0' && c <= b'7';
        if b == b'\\'
            && i + 3 < bytes.len()
            && is_octal(bytes[i + 1])
            && is_octal(bytes[i + 2])
            && is_octal(bytes[i + 3])
        {
            let v = (bytes[i + 1] - b'0') * 64
                + (bytes[i + 2] - b'0') * 8
                + (bytes[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p hytte-services sensors::tests::decode_octal`
Expected: 4 passed.

- [ ] **Step 5: Run a clippy check on the crate**

Run: `cargo clippy -p hytte-services --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/sensors.rs
git commit -m "feat(sensors): add MountSpec, pseudo-fs denylist, octal decoder"
```

---

## Task 3: `parse_mountinfo_line` (TDD)

A pure function that turns one mountinfo line into a `MountSpec` (or `None`).

**Files:**
- Modify: `crates/hytte-services/src/sensors.rs` (same parsing section)

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p hytte-services sensors::tests::parse_mountinfo_line`
Expected: FAIL with "cannot find function `parse_mountinfo_line`".

- [ ] **Step 3: Implement `parse_mountinfo_line`**

Add this function in the `── /proc/self/mountinfo parsing ──` section, immediately after `decode_octal_escapes`:

```rust
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
fn parse_mountinfo_line(line: &str) -> Option<MountSpec> {
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p hytte-services sensors::tests::parse_mountinfo_line`
Expected: 4 passed.

- [ ] **Step 5: Clippy**

Run: `cargo clippy -p hytte-services --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/sensors.rs
git commit -m "feat(sensors): parse single mountinfo line into MountSpec"
```

---

## Task 4: `parse_mountinfo` with filtering + dedup (TDD)

The full parser: walk all lines, filter pseudo fstypes, dedup by `dev_id` keeping the shortest path within each group while preserving original order of survivors.

**Files:**
- Modify: `crates/hytte-services/src/sensors.rs` (same section)

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p hytte-services sensors::tests::parse_mountinfo`
Expected: 3 new tests FAIL with "cannot find function `parse_mountinfo`".

- [ ] **Step 3: Implement `parse_mountinfo`**

Add this function immediately after `parse_mountinfo_line`:

```rust
/// Parse `/proc/self/mountinfo` text into a deduplicated, filtered list.
///
/// 1. Drops lines whose fstype is in [`PSEUDO_FSTYPES`].
/// 2. Dedups by `dev_id`, keeping the entry with the shortest path; ties
///    broken by mountinfo order.
/// 3. Preserves the original mountinfo order of the surviving entries.
fn parse_mountinfo(text: &str) -> Vec<MountSpec> {
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
    let winners: std::collections::HashSet<usize> =
        winner_idx.values().copied().collect();

    all.into_iter()
        .enumerate()
        .filter_map(|(i, s)| if winners.contains(&i) { Some(s) } else { None })
        .collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p hytte-services sensors::tests::parse_mountinfo`
Expected: 3 new tests passed (plus the 4 `parse_mountinfo_line_*` already passing).

- [ ] **Step 5: Clippy**

Run: `cargo clippy -p hytte-services --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/sensors.rs
git commit -m "feat(sensors): filter pseudo fs and dedup mounts by dev_id"
```

---

## Task 5: `read_mountlist` I/O wrapper, refactor disk poll to take `MountSpec`

Add the I/O wrapper. Replace `read_disk(&[&str])` with `read_disk_for_specs(&[MountSpec])`. Inside `poll_loop`, build a temporary `Vec<MountSpec>` from the still-hardcoded `["/", "/home"]` paths. End-state behavior is **unchanged** in this commit — same two mounts shown — but the plumbing is now ready for Task 6.

**Files:**
- Modify: `crates/hytte-services/src/sensors.rs`
  - mountinfo section: add `read_mountlist`.
  - disk-usage section (around line 755): replace `read_disk` with `read_disk_for_specs`.
  - `poll_loop` (around line 397): change `read_disk` call site.

- [ ] **Step 1: Add `read_mountlist`**

Append in the `── /proc/self/mountinfo parsing ──` section, after `parse_mountinfo`:

```rust
/// Read and parse the live `/proc/self/mountinfo`.
///
/// Returns an empty list on read failure (e.g. sandboxed runtime); the
/// caller's only failure mode in that case is reporting zero mounts.
fn read_mountlist() -> Vec<MountSpec> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|t| parse_mountinfo(&t))
        .unwrap_or_default()
}
```

- [ ] **Step 2: Replace `read_disk(&[&str])` with `read_disk_for_specs(&[MountSpec])`**

In the `// ── Disk usage ──` section (currently around line 753), replace:

```rust
fn read_disk(paths: &[&str]) -> DiskUsage {
    use nix::sys::statvfs::statvfs;
    let mut mounts = Vec::new();
    for p in paths {
        let Ok(s) = statvfs(*p) else {
            continue;
        };
        let block_size = s.fragment_size();
        let total = s.blocks() * block_size;
        let free = s.blocks_available() * block_size;
        let used = total.saturating_sub(free);
        #[allow(clippy::cast_precision_loss)]
        let usage = if total == 0 { 0.0 } else { used as f64 / total as f64 };
        mounts.push(DiskMount {
            path: (*p).to_string(),
            total_bytes: total,
            used_bytes: used,
            free_bytes: free,
            usage,
        });
    }
    DiskUsage { mounts }
}
```

with:

```rust
fn read_disk_for_specs(specs: &[MountSpec]) -> DiskUsage {
    use nix::sys::statvfs::statvfs;
    let mut mounts = Vec::with_capacity(specs.len());
    for spec in specs {
        let Ok(s) = statvfs(spec.path.as_str()) else {
            continue;
        };
        let block_size = s.fragment_size();
        let total = s.blocks() * block_size;
        let free = s.blocks_available() * block_size;
        let used = total.saturating_sub(free);
        #[allow(clippy::cast_precision_loss)]
        let usage = if total == 0 { 0.0 } else { used as f64 / total as f64 };
        mounts.push(DiskMount {
            path: spec.path.clone(),
            total_bytes: total,
            used_bytes: used,
            free_bytes: free,
            usage,
        });
    }
    DiskUsage { mounts }
}
```

- [ ] **Step 3: Update the poll_loop call site**

Inside `poll_loop` (currently around line 395-398), replace:

```rust
        // ── Disk (every 5 ticks) ──────────────────────────────────────────
        if state.tick.is_multiple_of(5) {
            disk_writer.set(read_disk(&["/", "/home"]));
        }
```

with:

```rust
        // ── Disk (every 5 ticks) ──────────────────────────────────────────
        if state.tick.is_multiple_of(5) {
            // Temporary: same hardcoded paths as before, expressed as
            // MountSpecs. Task 6 swaps this for a live mount list.
            let specs = vec![
                MountSpec {
                    path: "/".to_string(),
                    dev_id: (0, 0),
                    fstype: String::new(),
                },
                MountSpec {
                    path: "/home".to_string(),
                    dev_id: (0, 0),
                    fstype: String::new(),
                },
            ];
            disk_writer.set(read_disk_for_specs(&specs));
        }
```

- [ ] **Step 4: Build and test**

Run: `cargo build -p hytte-services && cargo test -p hytte-services sensors::`
Expected: clean build; all parsing tests still pass.

- [ ] **Step 5: Clippy**

Run: `cargo clippy -p hytte-services --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/sensors.rs
git commit -m "refactor(sensors): disk poll now consumes MountSpec list"
```

---

## Task 6: Mount-list Mutable, watcher task, live integration

The behavior change. Add the `mount_list: Mutable<Vec<MountSpec>>` to `SensorsHandles`. Add `mount_watch_loop`. Spawn it from `SensorsService::start` and pass a clone of the handle to `poll_loop`. Drop the temporary specs from Task 5; read from the Mutable instead.

**Files:**
- Modify: `crates/hytte-services/src/sensors.rs`
  - `SensorsHandles` struct (around line 138) and its `Default` impl (around line 149).
  - `SensorsService::start` (around line 172).
  - `poll_loop` signature (around line 311) and disk branch (around line 395).
  - Add `mount_watch_loop` in a new section after `poll_loop`.

- [ ] **Step 1: Add the `mount_list` field to `SensorsHandles`**

In the `SensorsHandles` struct (currently around line 138), add a new field after `process_count`:

```rust
#[doc(hidden)]
pub struct SensorsHandles {
    pub(crate) cpu: Mutable<CpuLoad>,
    pub(crate) memory: Mutable<Memory>,
    pub(crate) network: Mutable<NetIo>,
    pub(crate) cpu_temp: Mutable<CpuTemp>,
    pub(crate) gpu: Mutable<Option<GpuState>>,
    pub(crate) disk: Mutable<DiskUsage>,
    pub(crate) net_connections: Mutable<NetConnections>,
    pub(crate) process_count: Mutable<u32>,
    /// Live list of real mounts from `/proc/self/mountinfo`. Updated by
    /// `mount_watch_loop`; consumed by `poll_loop`'s disk branch.
    pub(crate) mount_list: Mutable<Vec<MountSpec>>,
}
```

In the `Default` impl (currently around line 149), add:

```rust
impl Default for SensorsHandles {
    fn default() -> Self {
        Self {
            cpu: Mutable::new(CpuLoad::default()),
            memory: Mutable::new(Memory::default()),
            network: Mutable::new(NetIo::default()),
            cpu_temp: Mutable::new(CpuTemp::default()),
            gpu: Mutable::new(None),
            disk: Mutable::new(DiskUsage::default()),
            net_connections: Mutable::new(NetConnections::default()),
            process_count: Mutable::new(0),
            mount_list: Mutable::new(Vec::new()),
        }
    }
}
```

- [ ] **Step 2: Add `mount_watch_loop`**

Add a new section after the existing `poll_loop` function (right before `// ── /proc/net/{tcp,tcp6} parsing ──` at line 413). Insert:

```rust
// ── Mount table watcher ──────────────────────────────────────────────────────

/// Background task: keep `mount_list` in sync with `/proc/self/mountinfo`.
///
/// Seeds the Mutable once, then waits for `POLLPRI` events on the open file
/// — the kernel signals POLLPRI on `/proc/self/mountinfo` whenever the mount
/// table changes (mount, unmount, remount). On each event we re-parse the
/// file from scratch via [`read_mountlist`].
///
/// Failure modes (open error, AsyncFd registration error, poll error) all
/// log a warning and exit. The Mutable then either stays empty (if the
/// initial open failed) or holds whatever was last successfully read.
async fn mount_watch_loop(mount_list: Mutable<Vec<MountSpec>>) {
    use std::os::fd::OwnedFd;
    use tokio::io::unix::AsyncFd;
    use tokio::io::Interest;

    // Seed once before we even attempt to register for events. This way a
    // POLLPRI registration failure still leaves us with a correct list as
    // of startup.
    mount_list.set(read_mountlist());

    let file = match std::fs::File::open("/proc/self/mountinfo") {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "sensors: failed to open mountinfo for watch");
            return;
        }
    };
    let fd: OwnedFd = file.into();
    let async_fd = match AsyncFd::with_interest(fd, Interest::PRIORITY) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "sensors: failed to register mountinfo AsyncFd");
            return;
        }
    };

    loop {
        match async_fd.ready(Interest::PRIORITY).await {
            Ok(mut guard) => {
                guard.clear_ready();
                mount_list.set(read_mountlist());
            }
            Err(e) => {
                tracing::warn!(error = %e, "sensors: mountinfo poll error, exiting watcher");
                return;
            }
        }
    }
}
```

- [ ] **Step 3: Add `mount_list_reader` parameter to `poll_loop`**

Change the `poll_loop` signature (currently at line 310-320):

```rust
#[allow(clippy::too_many_arguments)]
async fn poll_loop(
    cpu_writer: Mutable<CpuLoad>,
    mem_writer: Mutable<Memory>,
    net_writer: Mutable<NetIo>,
    cpu_temp_writer: Mutable<CpuTemp>,
    gpu_writer: Mutable<Option<GpuState>>,
    disk_writer: Mutable<DiskUsage>,
    net_conn_writer: Mutable<NetConnections>,
    proc_count_writer: Mutable<u32>,
    mount_list_reader: Mutable<Vec<MountSpec>>,
) {
```

(Add the new parameter as the last argument.)

- [ ] **Step 4: Replace the disk branch**

In `poll_loop`'s disk branch (replaced in Task 5 with hardcoded `MountSpec`s), replace:

```rust
        // ── Disk (every 5 ticks) ──────────────────────────────────────────
        if state.tick.is_multiple_of(5) {
            // Temporary: same hardcoded paths as before, expressed as
            // MountSpecs. Task 6 swaps this for a live mount list.
            let specs = vec![
                MountSpec {
                    path: "/".to_string(),
                    dev_id: (0, 0),
                    fstype: String::new(),
                },
                MountSpec {
                    path: "/home".to_string(),
                    dev_id: (0, 0),
                    fstype: String::new(),
                },
            ];
            disk_writer.set(read_disk_for_specs(&specs));
        }
```

with:

```rust
        // ── Disk (every 5 ticks) ──────────────────────────────────────────
        if state.tick.is_multiple_of(5) {
            let specs = mount_list_reader.get_cloned();
            disk_writer.set(read_disk_for_specs(&specs));
        }
```

- [ ] **Step 5: Wire up `SensorsService::start`**

In `SensorsService::start` (currently around line 172), add a `mount_list` clone for both the watcher and the poll loop, and spawn the watcher task. Replace the function body with:

```rust
    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = SensorsHandles::default();
        let cpu_writer = handles.cpu.clone();
        let mem_writer = handles.memory.clone();
        let net_writer = handles.network.clone();
        let cpu_temp_writer = handles.cpu_temp.clone();
        let gpu_writer = handles.gpu.clone();
        let disk_writer = handles.disk.clone();
        let net_conn_writer = handles.net_connections.clone();
        let proc_count_writer = handles.process_count.clone();
        let mount_list_for_poll = handles.mount_list.clone();
        let mount_list_for_watch = handles.mount_list.clone();

        rt.spawn(async move {
            poll_loop(
                cpu_writer,
                mem_writer,
                net_writer,
                cpu_temp_writer,
                gpu_writer,
                disk_writer,
                net_conn_writer,
                proc_count_writer,
                mount_list_for_poll,
            )
            .await;
        });
        rt.spawn(mount_watch_loop(mount_list_for_watch));

        handles
    }
```

- [ ] **Step 6: Build and run all tests**

Run: `cargo build -p hytte-services && cargo test -p hytte-services sensors::`
Expected: clean build; all 9 parsing tests pass.

- [ ] **Step 7: Clippy**

Run: `cargo clippy -p hytte-services --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add crates/hytte-services/src/sensors.rs
git commit -m "feat(sensors): live mount discovery via /proc/self/mountinfo"
```

---

## Task 7: Manual verification

Build the full workspace and run trollshell to confirm the chip and stats panel reflect real mounts.

**Files:** none modified.

- [ ] **Step 1: Workspace build**

Run: `cargo build` (from `/home/choom/src/trollshell`)
Expected: clean build.

- [ ] **Step 2: Compute the expected mount count**

Run:
```bash
findmnt --real --output TARGET,SOURCE --noheadings \
  | awk '{print $2}' \
  | sed 's/\[.*//' \
  | sort -u \
  | wc -l
```
Note the number — that's the upper bound (we dedup by `major:minor` of the kernel device, which is a slightly coarser grouping than `findmnt` source-uniqueness, but on most setups they match).

Also run plain `findmnt --real` and inspect the unique `major:minor` columns — the chip should show one bar per unique `major:minor`.

- [ ] **Step 3: Launch trollshell and inspect the disk chip**

Run: `cargo run --bin trollshell` (or however the user normally launches it; `./trollshell` is also a checked-in symlink).

Verify visually:
- The disk chip in the bar shows N tiny vertical bars where N matches the unique-`major:minor` count from Step 2.
- Hovering each bar shows a tooltip with that mount's path and percentage.
- Opening the Stats modal (click the chip) shows the same mount set inside the "Disk" expander, with `M used / N total (P%)` per row.

- [ ] **Step 4: Test live updates (optional, if user wants to verify the watcher)**

If the user has an extra device or can `mount --bind /tmp /mnt/test` (with sudo):
1. While trollshell is running, mount something new.
2. The chip should grow by one bar within ~5 s (next disk tick after the watcher updates the Mutable).
3. Unmount; the bar should disappear within ~5 s.

Skip this step if unwanted — the parser tests cover the logic; this is a smoke check on the AsyncFd plumbing.

- [ ] **Step 5: No commit needed for manual verification.**

---

## Self-Review

After writing this plan I checked it against the spec:

**Spec coverage:**
- Goal & non-goals: covered by Task 0 (file-structure section) and overall task scope.
- Architecture (a) mount list event-driven: Tasks 5, 6.
- Architecture (b) disk usage periodic: Task 5 (refactor) + Task 6 (live wiring).
- Failure modes: Task 6 Step 2 (mount_watch_loop) implements the warn+exit branches and the seed-before-register order.
- Data shapes: Task 2 (`MountSpec`), Task 6 (`mount_list` field). Public types untouched ✓.
- Pseudo-fs denylist: Task 2.
- Dedup (shortest path, dev_id, stable order): Task 4.
- Mountinfo parsing (octal escapes, optional tags, " - " split): Tasks 2, 3.
- Watcher lifecycle (open → seed → AsyncFd → loop): Task 6.
- Poll loop change: Task 5 (transitional) + Task 6 (final).
- Cargo.toml `net` feature: Task 1.
- Testing (octal, line parser, mountinfo filter & dedup, malformed): Tasks 2, 3, 4.
- Files-touched list (sensors.rs + Cargo.toml): matches.
- Verification: Task 7.

**Placeholder scan:** No `TODO`, `TBD`, `implement later`, or `add error handling` left in steps. All code blocks are complete.

**Type consistency:** `MountSpec { path, dev_id, fstype }` — used identically in Tasks 2, 3, 4, 5, 6. `read_disk_for_specs(&[MountSpec]) -> DiskUsage` — same signature in Tasks 5 and 6. `mount_list: Mutable<Vec<MountSpec>>` — same in struct definition (Task 6 Step 1), watcher fn arg (Task 6 Step 2), `poll_loop` arg (`mount_list_reader`) (Task 6 Step 3).
