# Sysinfo: dynamic mount discovery

**Status:** design approved 2026-05-05
**Scope:** `crates/hytte-services/src/sensors.rs` (+ `Cargo.toml` feature flag)

## Goal

The `sensors::disk()` signal currently reports usage for a hardcoded list
`["/", "/home"]`. Replace this with a dynamic list of real mounts read from
`/proc/self/mountinfo`, watched for changes via `POLLPRI`, with pseudo-fs
filtering and `major:minor` dedup. UI consumers (`widgets/disk.rs`,
`panels/stats.rs`) require no changes — they already iterate `disk.mounts`.

## Non-goals

- `/etc/fstab` parsing (the user's fstab is empty in practice; mountinfo is the
  authoritative live view).
- UI changes to the disk indicator chip or stats panel expander.
- Per-mount icons or labels.
- A public `sensors::mounts()` accessor — internal Mutable only.

## Architecture

Two cooperating pieces inside `sensors.rs`:

### (a) Mount list — event-driven

A new internal helper:

```rust
fn read_mountlist() -> Vec<MountSpec>
```

parses `/proc/self/mountinfo`, drops pseudo fstypes via a denylist, and dedups
by `major:minor` keeping the shortest path within each device group.

A new tokio task `mount_watch_loop` opens `/proc/self/mountinfo`, wraps the fd
in `tokio::io::unix::AsyncFd` with `Interest::PRIORITY`, seeds the shared
`Mutable<Vec<MountSpec>>` once, then loops on `ready_mut(Ready::PRIORITY)`,
re-reading the file and updating the Mutable on each event. The kernel
delivers POLLPRI on `/proc/self/mountinfo` whenever the mount table changes.

### (b) Disk usage — periodic

The existing 5-tick branch in `poll_loop` no longer calls
`read_disk(&["/", "/home"])`. Instead it reads the current
`Vec<MountSpec>` from the shared Mutable and runs `statvfs` on each
`spec.path`, emitting a `DiskUsage` with one `DiskMount` per spec. Cadence
unchanged (~5 s).

### Why split

Mount/unmount events are rare and bursty — POLLPRI is the right signal. Disk
usage drifts continuously and needs polling. One signal each.

### Failure modes

- If opening `/proc/self/mountinfo` fails: `tracing::warn!`, the watcher task
  exits before seeding. The `Mutable` stays at its `Default` (empty) so
  `disk()` reports an empty `mounts` vec — UI shows zero bars rather than
  crashing.
- If `AsyncFd` setup fails: warn and exit. The Mutable has already been seeded
  by step 2, so the mount list is correct as of startup but will not update on
  later mount/unmount events.
- If a single line of mountinfo fails to parse: skip that line, continue.
- No fallback periodic re-list. Watcher failure on Linux is unexpected; if it
  ever bites, revisit.

## Data shapes

New internal type, **not** in the public API:

```rust
struct MountSpec {
    path: String,        // mountinfo field 5 (mount point), octal-decoded
    dev_id: (u32, u32),  // mountinfo field 3 (major:minor)
    fstype: String,      // mountinfo right-half token 1, diagnostic only
}
```

Public types unchanged:

- `DiskUsage { mounts: Vec<DiskMount> }` — same.
- `DiskMount { path, total_bytes, used_bytes, free_bytes, usage }` — same.
- `sensors::disk() -> impl Signal<Item = DiskUsage>` — same.

`SensorsHandles` gains one private field:

```rust
pub(crate) mount_list: Mutable<Vec<MountSpec>>,
```

Initialised to `Mutable::new(Vec::new())` in `Default`.

## Pseudo-fs denylist

```rust
const PSEUDO_FSTYPES: &[&str] = &[
    "proc", "sysfs", "cgroup", "cgroup2", "devtmpfs", "devpts", "tmpfs",
    "mqueue", "securityfs", "pstore", "bpf", "tracefs", "debugfs",
    "hugetlbfs", "configfs", "fusectl", "binfmt_misc", "autofs",
    "efivarfs", "ramfs", "rpc_pipefs", "nsfs", "selinuxfs", "overlay",
    "squashfs",
];
```

Defined as a module-level `const` near `read_mountlist`. Anything not in this
list passes — inclusive toward exotic real fstypes (`zfs`, `xfs`, `ntfs3`,
`exfat`, `nfs4`, `cifs`, `f2fs`, …). Matches the spirit of `findmnt --real`.

## Dedup

After filtering, group by `dev_id`. Within each group keep the entry with the
**shortest path**; ties broken by mountinfo order. Final list preserves the
original mountinfo order of the surviving entries.

Rationale: on this system `/` and `/home/choom` are bind-style mounts on the
same device; `statvfs` returns identical numbers. Showing one bar for one
storage device is more honest than showing duplicates. A separate USB drive on
a different `major:minor` survives dedup and shows as its own bar.

## Mountinfo parsing

`/proc/self/mountinfo` line format (man `proc(5)` §5):

```
36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
 ^  ^  ^    ^     ^                          ^
 1  2  3    4     5                          9 (after the " - " separator)
```

Fields 7..N before the literal ` - ` token are optional tags (variable count).

Implementation:

1. Read whole file (typically <16 KB).
2. For each line, split once on `" - "` into `(left, right)`. Skip line if
   either half missing.
3. Parse `left` whitespace-separated fields:
   - field 3 → split on `:` → `(major, minor)` as `u32`.
   - field 5 → mount point (octal-decode).
4. Parse `right` first whitespace token → fstype.
5. If fstype in `PSEUDO_FSTYPES`, skip.
6. Otherwise emit `MountSpec { path, dev_id: (major, minor), fstype }`.

After collecting all specs, run dedup (above).

### Octal decoding

mountinfo encodes characters in field 5 (mount point) as `\NNN` octal escapes:
` ` → `\040`, `\` → `\134`, `\t` → `\011`, `\n` → `\012`. Implement a small
helper that walks the byte string and decodes `\` followed by exactly three
octal digits. Anything else (including `\` not followed by three octal digits)
is preserved verbatim. ~10 lines.

## Watcher task lifecycle

Spawned alongside `poll_loop` from `SensorsService::start`:

```rust
tokio::spawn(poll_loop(/* … existing args … */));
tokio::spawn(mount_watch_loop(handles.mount_list.clone()));
```

Both tasks live for the process lifetime; no shutdown plumbing (matches the
rest of `sensors.rs`).

`mount_watch_loop`:

1. Open `/proc/self/mountinfo` via `std::fs::File::open`. On error: warn, exit.
2. Seed: write `read_mountlist()` to the Mutable.
3. Convert to `std::os::fd::OwnedFd` and wrap:
   `AsyncFd::with_interest(fd, Interest::PRIORITY)`. On error: warn, exit.
4. Loop:
   - `let mut guard = async_fd.ready_mut(Interest::PRIORITY).await?;`
   - `guard.clear_ready();`
   - `mount_list.set(read_mountlist());`

`read_mountlist` is a free function that re-opens and re-reads
`/proc/self/mountinfo` from scratch on each call — simpler than threading the
`File` handle through, and avoids needing to `lseek` the watched fd. The
watched fd is used purely as a POLLPRI signal source; we never read its
contents.

## Poll loop change

In `poll_loop`, replace:

```rust
if state.tick.is_multiple_of(5) {
    disk_writer.set(read_disk(&["/", "/home"]));
}
```

with:

```rust
if state.tick.is_multiple_of(5) {
    let specs = mount_list_reader.get_cloned();
    disk_writer.set(read_disk_for_specs(&specs));
}
```

`mount_list_reader: Mutable<Vec<MountSpec>>` (a clone of `handles.mount_list`)
is added as a new argument to `poll_loop`, plumbed through
`SensorsService::start`. The poll loop only reads via `get_cloned`; the
watcher task is the sole writer.

`read_disk_for_specs(&[MountSpec]) -> DiskUsage` is `read_disk` adapted to
take specs and use `spec.path` for `statvfs`. The original
`read_disk(&[&str])` is removed (no other callers).

## Cargo.toml change

`crates/hytte-services/Cargo.toml`:

```diff
-tokio = { version = "1.52.1", features = ["rt", "io-util", "process", "sync", "time"] }
+tokio = { version = "1.52.1", features = ["net", "rt", "io-util", "process", "sync", "time"] }
```

`tokio::io::unix::AsyncFd` is gated behind the `net` feature.

## Testing

Unit tests in `sensors.rs` `#[cfg(test)] mod tests`:

- `parse_mountinfo_line_basic` — feeds a representative line, asserts
  `MountSpec` fields.
- `parse_mountinfo_line_with_optional_tags` — line with `master:1 shared:2`
  before `-`, asserts parser still finds field 5 and the fstype.
- `parse_mountinfo_line_octal_path` — path containing `\040`, asserts the
  decoded `path` contains a literal space.
- `parse_mountinfo_filters_pseudo` — fixture string with `tmpfs`, `proc`,
  `ext4`, `btrfs`; asserts only the latter two survive.
- `parse_mountinfo_dedups_by_dev_id` — fixture with two `ext4` entries on the
  same `major:minor` (e.g. `/` and `/run/host/os-release`); asserts the
  shortest path wins, dedup leaves one entry.

No integration test for the watcher — the parsing is the failure-prone part;
the AsyncFd plumbing is best validated by running.

To make `read_mountlist` testable without `/proc`, factor it as:

```rust
fn parse_mountinfo(text: &str) -> Vec<MountSpec> { … }       // pure, tested
fn read_mountlist() -> Vec<MountSpec> {                       // I/O wrapper
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|t| parse_mountinfo(&t))
        .unwrap_or_default()
}
```

## Files touched

- `crates/hytte-services/src/sensors.rs` — adds `MountSpec`, `parse_mountinfo`,
  `parse_mountinfo_line`, `read_mountlist`, `mount_watch_loop`, denylist const,
  octal decoder, `mount_list` field on `SensorsHandles`, `read_disk_for_specs`,
  poll-loop arg plumbing. Removes `read_disk(&[&str])`.
- `crates/hytte-services/Cargo.toml` — add `"net"` to `tokio` features.

No other files change. UI consumers remain untouched.

## Verification

After implementation:

- `cargo build -p hytte-services` clean.
- `cargo test -p hytte-services sensors::tests` — new tests pass.
- Run `trollshell` and observe: the disk chip shows N bars where N matches
  `findmnt --real | tail -n +2 | awk '{print $1, $2}' | sort -u | wc -l`
  (with the major:minor dedup applied).
- Plug in a USB stick or `mount` something new; chip updates within ≤5 s.
- Stats panel "Disk" expander reflects the same mount set.
