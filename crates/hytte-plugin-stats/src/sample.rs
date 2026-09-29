//! Sampling — the `/proc` and `/sys` reads, and the task that drives them.
//!
//! # The same code the shell runs, in another process
//!
//! Every read here goes through [`hytte_sensors`], the GTK-free leaf P0
//! (#1249) moved the shell's own samplers into byte-for-byte. That is the whole
//! reason P0 existed: a plugin cannot subscribe to `hytte-services` (there is
//! no `StateKey::Sensors`, and a 1 Hz push of a twenty-field struct to every
//! plugin is the wrong wire), so the *sampler* moves rather than the state. The
//! numbers on this card are therefore the numbers on the native Stats page, not
//! a second implementation that agrees by coincidence.
//!
//! # Nothing here reads `/proc` in a test
//!
//! [`Snapshot`] is a plain struct of public fields, constructed directly by the
//! view tests; [`Sampler`] is the only thing that touches the filesystem and it
//! is never built in one. The split is deliberate — a test that read the real
//! `/proc/stat` would assert about the machine it happened to run on.
//!
//! That is enforced structurally rather than by convention: the task under test
//! is [`sampler_task_with`], which takes a [`Sample`] factory, and every test
//! hands it a counting fake. Before #1277 the claim rested entirely on the poll
//! gate never opening — so the one test that was supposed to establish it would
//! have read the host's `/proc` on the very run where it failed, and the
//! sampler's own happy path was executed by nothing at all. The one read that
//! *is* tested against real kernel data is [`cpu_half`], and it is handed a
//! `/proc/stat`-shaped fixture rather than the file.
//!
//! # Why the reads are `spawn_blocking`ed
//!
//! They are cheap on this machine and not cheap on every machine:
//! `hytte_sensors::read_gpu_with_cache` probes AMD and Intel through sysfs but
//! falls through to **spawning `nvidia-smi`** on an Nvidia box, which is a
//! `fork`/`exec` per tick. `hytte_plugin::run` drives the whole session on a
//! **current-thread** runtime, so a blocking read here is a blocking read of
//! the socket loop — the card would stop answering the host for as long as the
//! probe took. `hytte-services`' own `sensors` service `spawn_blocking`s the
//! same reads for the same reason.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hytte_plugin::poll::{Gate, Wake};
use hytte_plugin::{CmdReceiver, CmdSender};
use hytte_sensors::GpuCache;

use crate::top_apps::{self, TopApps, Walker};

/// One tick's worth of the machine, as the card consumes it.
///
/// Deliberately **not** `hytte_sensors`' own shapes re-exported: this is the
/// subset the card draws, already normalised (loads as `0.0..=1.0` `f32`s, one
/// `Option` per reading that can be absent), so `view` has no unit conversion
/// in it and a test can write one down in four lines.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// Overall CPU load, `0.0..=1.0` — or `None` when no delta is available
    /// yet, which the card draws as a dash.
    ///
    /// An `Option` rather than an `f32` resting at zero because the two are
    /// different claims and the card makes both: "the machine is idle" and "we
    /// have not measured it yet" must not render as the same `0%` beside a
    /// `----` lamp row (#1277 LOW 5). `None` is [`per_core`](Self::per_core)
    /// being empty, said for the headline.
    pub cpu: Option<f32>,
    /// Per-logical-core load, `0.0..=1.0`, in the kernel's core order. Empty
    /// before the first delta is available (the first tick has no previous
    /// sample to subtract) — see [`cpu_half`].
    pub per_core: Vec<f32>,
    /// CPU package temperature in °C, or `None` when no hwmon chip answers.
    pub cpu_temp_c: Option<f32>,
    /// The GPU, or `None` when there is none to read — which is what makes the
    /// GPU half of the card hide itself.
    pub gpu: Option<Gpu>,
    /// Memory and swap, or `None` when `/proc/meminfo` could not be read or
    /// this instance does not draw memory (see [`Needs`]).
    pub memory: Option<Memory>,
    /// One entry per mounted filesystem, in `/proc/self/mountinfo` order —
    /// which is the order the native Disks card and disk chip render, and the
    /// only ordering `hytte_sensors` defines. Empty when this instance does not
    /// draw disks.
    pub disks: Vec<Disk>,
    /// Running process count (the native CPU card's "Processes" row,
    /// `trollshell/src/panels/stats.rs:576` → `:1182`), or `None` when this
    /// instance does not draw the CPU half (see [`Needs`]).
    ///
    /// Added by the #1295 review's MED 1: `hytte_sensors::read_process_count`
    /// was already in the P0 leaf this crate samples with, so the row was a
    /// gap in the page rather than a gap in what is reachable.
    pub processes: Option<u64>,
    /// Aggregate CPU clock in Hz — the native CPU card's "Clock" row
    /// (`stats.rs:578` → `:1557`, `f.max_hz`) — or `None` when this instance
    /// does not draw the CPU half, or when the machine has no `cpufreq`
    /// governor (`max_ceiling_hz == 0.0`: VMs and some ARM boards), which is
    /// the native row's own hide rule.
    pub cpu_clock_hz: Option<f64>,
    /// The highest `cpuinfo_max_freq` across cores in Hz — the fixed top the
    /// native Clock row's sparkline is normalised against
    /// (`CpuFreq::max_ceiling_hz`, "a fixed 0→max-clock domain that shows
    /// headroom rather than auto-scaling"), so the drawer page's line can
    /// draw on the same axis (#1252). `Some` exactly when
    /// [`cpu_clock_hz`](Self::cpu_clock_hz) is.
    pub cpu_clock_ceiling_hz: Option<f64>,
    /// Per-core clock, each core's current frequency over
    /// [`cpu_clock_ceiling_hz`](Self::cpu_clock_ceiling_hz), `0.0..=1.0`, in
    /// the kernel's `cpufreq` order — the native expanded Clock row's series
    /// (`hytte-services`' `cpu_freq_per_core_history`: `hz / max_ceiling_hz`
    /// per core), for the drawer page's per-core clock graph (#1419 item 2).
    ///
    /// One entry per core **that exposes a `cpufreq` node**, which is not
    /// always every core [`per_core`](Self::per_core) counts. Empty exactly
    /// when [`cpu_clock_hz`](Self::cpu_clock_hz) is `None`: no governor, or an
    /// instance that does not draw the CPU half.
    pub per_core_clock: Vec<f32>,
    /// Aggregate disk-throughput history — the native Disks card's I/O row
    /// (`stats.rs:610` → `:1293`) — or `None` when this instance does not draw
    /// disks (see [`Needs`]). Unlike [`cpu`](Self::cpu) this is never withheld
    /// for a cold start: `hytte_sensors::compute_disk_io` answers an empty
    /// `prev` map with a valid zero rate rather than nothing, because a byte
    /// counter (unlike a `/proc/stat` jiffy count) is meaningful from the very
    /// first read.
    pub disk_io: Option<DiskIo>,
}

/// Aggregate disk-throughput history, mirroring the native Disks card's I/O
/// row: the current combined read/write rate, and the cumulative totals since
/// boot.
///
/// A narrowing of `hytte_sensors::DiskIo`, restated locally so `Snapshot` can
/// derive `PartialEq` — the upstream type does not, the same reason
/// [`Memory`] and [`Disk`] are local copies rather than the upstream shapes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DiskIo {
    /// Aggregate read rate across physical disks, bytes/sec. Sanitised to a
    /// finite value the same way [`as_unit`] sanitises a load — see
    /// [`finite_or_zero`].
    pub read_bps: f64,
    /// Aggregate write rate across physical disks, bytes/sec.
    pub write_bps: f64,
    /// Cumulative bytes read since boot, summed across physical disks.
    pub total_read_bytes: u64,
    /// Cumulative bytes written since boot, summed across physical disks.
    pub total_write_bytes: u64,
}

/// The memory half of a [`Snapshot`] — the four counters the native Memory row
/// and swap row read, and nothing else `/proc/meminfo` carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    /// Bytes in use — `hytte_sensors`' own definition, `total - available`.
    pub used: u64,
    /// Bytes of RAM. Zero is the machine the native row renders as an em dash.
    pub total: u64,
    /// Swap bytes in use.
    pub swap_used: u64,
    /// Swap bytes configured. Zero hides the swap row, exactly as the native
    /// page does.
    pub swap_total: u64,
}

/// One mounted filesystem.
///
/// A narrowing of [`hytte_sensors::DiskMount`]: `free_bytes` is dropped (it is
/// `total - used` and nothing on either surface prints it) and `usage` is
/// re-expressed as a sanitised `f32` the way every other load on this card is,
/// so `view` has no conversion in it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Disk {
    /// The mount point, as the kernel spells it.
    pub path: String,
    /// Bytes in use.
    pub used_bytes: u64,
    /// Bytes on the filesystem.
    pub total_bytes: u64,
    /// Fullness, `0.0..=1.0`.
    pub usage: f32,
}

/// The GPU half of a [`Snapshot`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gpu {
    /// Free-form adapter name, as the vendor reports it.
    pub name: String,
    /// Load, `0.0..=1.0`, or `None` when the vendor exposes no busy counter.
    pub load: Option<f32>,
    /// Adapter temperature in °C, or `None` when the vendor exposes none.
    ///
    /// Not clamped or sanitised, for [`celsius`]'s reason — there is no
    /// defensible range for a temperature, and a non-finite reading is drawn as
    /// nothing rather than as a number nobody measured. Added by P2 (#1251):
    /// the native GPU chip shows a `{c:.0}°` label beside its bar and P1's card
    /// had no place to put one, so nothing read it until the chips existed.
    pub temperature_c: Option<f32>,
    /// VRAM in use, or `None` when the vendor exposes no memory counter (some
    /// vendors report load/temperature but not memory). The native GPU card's
    /// "GPU VRAM" history row (`trollshell/src/panels/stats.rs:632`) hides
    /// itself unless both this and [`memory_total_bytes`](Self) are `Some` —
    /// the same rule this page follows (#1295 review MED 1).
    pub memory_used_bytes: Option<u64>,
    /// VRAM installed, or `None` for the same reason as
    /// [`memory_used_bytes`](Self).
    pub memory_total_bytes: Option<u64>,
}

/// The command lane: what the reducer tells the two sampling tasks.
///
/// One lane, because the SDK hands [`Plugin::sources`](hytte_plugin::Plugin)
/// exactly one receiver; [`route`] splits it so each task's
/// [`Gate`] sees only its own switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// The mount surface became visible / hidden (#288) — the sensors
    /// sampler's gate.
    SetVisible(bool),
    /// The plugin's own page opened or closed (#1427, the host's
    /// `PageVisibility` push through [`Plugin::page_visible`](hytte_plugin::Plugin::page_visible))
    /// — the `/proc` walker's gate. The page is the only thing that draws the
    /// Top apps lists, so it is the only thing that runs the walk; see
    /// `crate::top_apps` (a crate-private module) for why that matches native.
    PageVisible(bool),
}

impl Cmd {
    /// The sensors sampler's classifier: the surface's visibility, and nothing
    /// about the page.
    ///
    /// Spelled without a wildcard on purpose, like its sibling: a third
    /// variant is a compile error here, which is the place to decide which
    /// gate it opens.
    ///
    /// **Not** the page's visibility either: the bar chips are on screen
    /// whether or not their drawer page is, so a page close must never park
    /// the sampler behind them.
    #[must_use]
    pub const fn surface(self) -> Option<bool> {
        match self {
            Self::SetVisible(visible) => Some(visible),
            Self::PageVisible(_) => None,
        }
    }

    /// The walker's classifier: whether the page is on screen, and nothing
    /// about the surface.
    ///
    /// **Not** the surface's visibility, and that is the whole point: a bar
    /// instance opens its own sensors gate at `init` and never closes it, so a
    /// walker that answered `SetVisible` would walk `/proc` for as long as the
    /// bar chips are on screen — i.e. forever.
    #[must_use]
    pub const fn page(self) -> Option<bool> {
        match self {
            Self::PageVisible(open) => Some(open),
            Self::SetVisible(_) => None,
        }
    }
}

/// The message lane: one reading per tick, back to the reducer.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// A fresh sample landed.
    Sampled(Box<Snapshot>),
    /// A fresh walk of `/proc` landed — the page's Top apps lists.
    TopApps(TopApps),
}

/// Which sensors this instance actually draws — so it does not pay for the
/// ones it does not.
///
/// Derived from the resolved [`Card`](crate::config::Card) once at launch,
/// because the reads it gates are not all cheap and several of them are
/// genuinely expensive on the wrong machine:
///
/// - **`gpu`** falls through to spawning `nvidia-smi` on an Nvidia box — a
///   `fork`/`exec` per tick, which is the reason the whole read is
///   `spawn_blocking`ed.
/// - **`disk`** walks `/proc/self/mountinfo` and then `statvfs`es every
///   surviving mount, and (since the #1295 review's MED 1) re-reads
///   `/proc/diskstats` for the Disks card's I/O history. On a box with a dozen
///   filesystems that is a dozen syscalls that can each block on a slow or
///   unresponsive filesystem.
/// - **`temperature`** resolves an hwmon chip by `read_dir` the first time.
/// - **`memory`** is one small `/proc/meminfo` read, gated for symmetry rather
///   than for cost.
/// - **`cpu`** gates the CPU card's two newer rows (#1295 review MED 1):
///   `read_process_count` walks every entry of `/proc`, and `read_cpu_freq`
///   walks every core's `cpufreq` sysfs node. Both are cheap on most boxes but
///   scale with process/core count, unlike `/proc/stat` below — and both are
///   pointless work on an instance with no CPU half to show them on.
///
/// `/proc/stat` is deliberately **not** gated: it is a single small read, it is
/// the baseline [`Sampler::reset`] exists to invalidate, and an instance that
/// draws no CPU at all is not a shape worth a second code path.
/// Five **independent** switches over "which sensors does this instance read",
/// which is the same argument `crate::config::Card` makes: collapsing them into
/// a bitflag or an enum would make "this surface draws memory but not disk"
/// unspellable, and each one gates a different syscall.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    /// Read the process count and the CPU clock (#1295 review MED 1).
    pub cpu: bool,
    /// Read the package temperature.
    pub temperature: bool,
    /// Read the GPU.
    pub gpu: bool,
    /// Read `/proc/meminfo`.
    pub memory: bool,
    /// Walk the mount table and `statvfs` each mount, and read the disk I/O
    /// counters (#1295 review MED 1).
    pub disk: bool,
}

impl Needs {
    /// What one resolved table asks the sampler for.
    ///
    /// The temperature and the GPU ride their own switches; on a surface whose
    /// `cpu` is off there is no CPU chip or CPU half for the temperature to sit
    /// beside, so it is not read either — which is the one place a *pair* of
    /// keys decides a read.
    #[must_use]
    pub const fn of(card: crate::config::Card) -> Self {
        Self {
            cpu: card.cpu,
            temperature: card.temperature && (card.cpu || card.gpu),
            gpu: card.gpu,
            memory: card.memory,
            disk: card.disk,
        }
    }
}

/// The stateful half of sampling: the per-tick caches `hytte_sensors` asks the
/// caller to carry.
///
/// All three are "remember the last answer so the next one is cheap or
/// possible at all": `/proc/stat` is cumulative so a *load* needs the previous
/// reading, the hwmon chip directory costs a `read_dir` walk to resolve, and
/// the GPU cache remembers whether `nvidia-smi` exists at all, plus (#1297)
/// its last reading and Intel's RC6 delta base.
#[derive(Debug, Default)]
pub struct Sampler {
    /// What this instance draws, and therefore what it reads.
    needs: Needs,
    /// Previous `/proc/stat` (busy, total) per core.
    prev_cpu: Vec<(u64, u64)>,
    /// The resolved `/sys/class/hwmon` chip directory, once found.
    hwmon: Option<PathBuf>,
    /// `nvidia-smi` availability, its last successful reading (#1297), and
    /// the Intel RC6 delta base.
    gpu: GpuCache,
    /// Previous per-device `/proc/diskstats` byte counters, keyed by device
    /// name — `hytte_sensors::compute_disk_io`'s own cache shape.
    ///
    /// **Not** cleared by [`reset`](Self::reset): unlike the CPU baseline,
    /// leaving this stale means the first disk-I/O reading after an unpark
    /// averages over however long the surface was closed rather than over a
    /// fresh window (the same failure #1277 LOW 4 fixed for CPU load) — a
    /// known simplification, named rather than silently carried, since the
    /// rate is one line on a page and not the headline this card leads with.
    prev_diskio: HashMap<String, (u64, u64, Instant)>,
}

impl Sampler {
    /// A sampler with cold caches, reading only what `needs` asks for. The
    /// first [`Self::tick`] has no previous `/proc/stat` to subtract, so it
    /// publishes **no CPU reading at all** — see [`cpu_half`], which is where
    /// that decision lives and is tested.
    #[must_use]
    pub fn new(needs: Needs) -> Self {
        Self {
            needs,
            ..Self::default()
        }
    }

    /// Read the machine once. **Blocking** — see the module doc for why every
    /// caller runs it under `spawn_blocking`.
    ///
    /// Panic-free over any state of `/proc` and `/sys`: every read in
    /// `hytte_sensors` is a `Result` or an `Option` that degrades to a default,
    /// and the two casts below are saturating.
    #[must_use]
    pub fn tick(&mut self) -> Snapshot {
        let now = hytte_sensors::read_proc_stat().unwrap_or_default();
        let (cpu, per_core) = cpu_half(&self.prev_cpu, &now);
        // Only replace the baseline with a reading we actually got: an
        // `Err` from `/proc/stat` (which should not happen on Linux, but is a
        // plain `io::Error` away) would otherwise wipe the baseline and cost
        // the *next* tick its delta too.
        if !now.is_empty() {
            self.prev_cpu = now;
        }

        let cpu_temp_c = if self.needs.temperature {
            hytte_sensors::read_cpu_temp(&mut self.hwmon)
                .package_celsius
                .map(celsius)
        } else {
            None
        };

        let gpu = if self.needs.gpu {
            // `GpuCache` is not `Copy` (#1297 — it carries the last Nvidia
            // reading, which owns a `String`), so take it out of `self`
            // rather than copy it, mirroring the sensors service's own
            // `poll_loop` doing the same with `std::mem::take`.
            let (gpu, cache) = hytte_sensors::read_gpu_with_cache(std::mem::take(&mut self.gpu));
            self.gpu = cache;
            gpu.map(|g| Gpu {
                name: g.name,
                load: g.load.map(as_unit),
                temperature_c: g.temperature_celsius.map(celsius),
                memory_used_bytes: g.memory_used_bytes,
                memory_total_bytes: g.memory_total_bytes,
            })
        } else {
            None
        };

        // The CPU card's two newer rows (#1295 review MED 1): a process count
        // and the aggregate clock. Both ride the `cpu` need rather than the
        // ungated `/proc/stat` read above — a `read_dir("/proc")` walk and a
        // per-core `cpufreq` sysfs walk are not "one small read" the way
        // `/proc/stat` is, and are pointless work on a surface with no CPU
        // half to show them on.
        let processes = if self.needs.cpu {
            Some(u64::from(hytte_sensors::read_process_count()))
        } else {
            None
        };
        // One `cpufreq` walk feeds the aggregate and the per-core series alike,
        // so the page's collapsed and expanded Clock rows are one observation.
        let (cpu_clock_hz, cpu_clock_ceiling_hz, per_core_clock) = if self.needs.cpu {
            let freq = hytte_sensors::read_cpu_freq();
            let (hz, ceiling) = clock_of(&freq);
            (hz, ceiling, per_core_clock_of(&freq))
        } else {
            (None, None, Vec::new())
        };

        let memory = if self.needs.memory {
            hytte_sensors::read_proc_meminfo().ok().map(|m| Memory {
                used: m.used,
                total: m.total,
                swap_used: m.swap_used,
                swap_total: m.swap_total,
            })
        } else {
            None
        };

        // The mount table is re-read every tick rather than cached behind a
        // `POLLPRI` watcher the way `hytte-services`' own sensors service does
        // it (#1249 left that half in the shell, since it needs an `AsyncFd`
        // and a tokio reactor). One small `/proc` read per poll buys a plugin
        // that notices a `mount` immediately and owns no watcher — and the
        // `statvfs` calls beside it dominate the cost either way.
        let disks = if self.needs.disk {
            hytte_sensors::read_disk_for_specs(&hytte_sensors::read_mountlist())
                .mounts
                .into_iter()
                .map(|m| Disk {
                    path: m.path,
                    used_bytes: m.used_bytes,
                    total_bytes: m.total_bytes,
                    usage: as_unit(m.usage),
                })
                .collect()
        } else {
            Vec::new()
        };

        // The Disks card's I/O history row (#1295 review MED 1) — gated on
        // the same `disk` need as the mount walk above, and read from
        // `/proc/diskstats` rather than `statvfs`: a different file, but the
        // same "this instance does not draw disks" question decides both.
        let disk_io = if self.needs.disk {
            let devices = hytte_sensors::read_proc_diskstats().unwrap_or_default();
            let now = Instant::now();
            let (io, next) = hytte_sensors::compute_disk_io(&self.prev_diskio, devices, now);
            self.prev_diskio = next;
            Some(DiskIo {
                read_bps: finite_or_zero(io.read_bps),
                write_bps: finite_or_zero(io.write_bps),
                total_read_bytes: io.total_read_bytes,
                total_write_bytes: io.total_write_bytes,
            })
        } else {
            None
        };

        Snapshot {
            cpu,
            per_core,
            cpu_temp_c,
            gpu,
            memory,
            disks,
            processes,
            cpu_clock_hz,
            cpu_clock_ceiling_hz,
            per_core_clock,
            disk_io,
        }
    }

    /// Forget the cumulative `/proc/stat` baseline, so the next
    /// [`tick`](Self::tick) publishes nothing and the one after it is a load
    /// measured over a *fresh* window.
    ///
    /// Called on the gate's open edge (#1277 LOW 4). `/proc/stat` is
    /// cumulative, so a baseline taken before the sidebar was closed makes the
    /// first frame on reopen the mean load **over the whole time it was
    /// closed** — close the sidebar, build for an hour, reopen, and the card
    /// reads the hour's average before snapping to reality a period later. The
    /// cost of re-baselining is one poll period of dashes, which is the honest
    /// answer and is exactly what a cold start already shows.
    ///
    /// The hwmon path is deliberately **kept**: the chip directory is a
    /// `read_dir` walk whose answer does not go stale. The GPU cache's
    /// *availability* memo is kept too — whether `nvidia-smi` exists is a
    /// `fork`/`exec` to re-learn — but everything else in it is a
    /// measurement anchored to a moment before the park: the last Nvidia
    /// reading (#1297) and the Intel RC6 delta base both predate the close,
    /// so serving either on the first frame after an unpark would be exactly
    /// the stale-frame failure the `/proc/stat` baseline above exists to
    /// avoid — just on the GPU needle instead of the CPU row.
    /// [`GpuCache::forget_readings`](hytte_sensors::GpuCache::forget_readings)
    /// is that seam (#1297 review MEDIUM 1; before it, only the Intel half
    /// had this problem — the Nvidia reading didn't exist yet to go stale).
    pub fn reset(&mut self) {
        self.prev_cpu.clear();
        self.gpu.forget_readings();
    }
}

/// The CPU half of a tick: the overall load and the per-core loads, **or
/// nothing at all** when `/proc/stat` has not been read twice yet.
///
/// This is the branch that makes "the first tick draws dashes" true, and it is
/// split out of [`Sampler::tick`] so it can be driven from a `/proc/stat`-shaped
/// fixture rather than from the machine a test happens to run on.
///
/// Without it the claim is false in a way that is invisible on glass:
/// `hytte_sensors::compute_cpu_load` answers an empty `prev` with
/// `overall: 0.0` **and a full-length `per_core` of zeroes**, not an empty one
/// — so a cold card would draw one *idle* lamp per core and a `CPU 0%`
/// headline, which is a measurement it never made (#1277 MEDIUM 3). `None` and
/// an empty `per_core` are what [`crate::card`] renders as `—` and `----`.
fn cpu_half(prev: &[(u64, u64)], now: &[(u64, u64)]) -> (Option<f32>, Vec<f32>) {
    if prev.is_empty() || now.is_empty() {
        return (None, Vec::new());
    }
    let load = hytte_sensors::compute_cpu_load(prev, now);
    (
        Some(as_unit(load.overall)),
        load.per_core.iter().copied().map(as_unit).collect(),
    )
}

/// A `f64` load as a finite `0.0..=1.0` `f32`.
///
/// Clamped **here**, at the one seam between the sampler and everything else,
/// rather than at each of the four places a load is drawn. A `NaN` reads as
/// `0.0`: `f32::clamp` panics on a `NaN` bound but merely *returns* the `NaN`
/// for a `NaN` input, which would then defeat the runtime's render dedup
/// forever (#896/#898 — `Node` derives `PartialEq`, and a view holding a `NaN`
/// is unequal to an identical copy of itself). The `as f32` cast is lossy in
/// precision only and cannot overflow from a value already inside `0..=1`.
#[allow(clippy::cast_possible_truncation)]
fn as_unit(load: f64) -> f32 {
    let v = load as f32;
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// A `f64` temperature as an `f32`. Unlike [`as_unit`] this one is **not**
/// clamped or sanitised: there is no defensible range for a temperature, and a
/// non-finite reading is rendered as a dash by `card::temp_text` rather than
/// substituted with a number nothing measured. The cast loses precision the
/// sensor never had (hwmon reports millidegrees, and the readout is whole
/// degrees).
#[allow(clippy::cast_possible_truncation)]
fn celsius(c: f64) -> f32 {
    c as f32
}

/// The CPU clock half of a tick: `(aggregate clock, its normalisation
/// ceiling)`, both in Hz — or `(None, None)` on a machine with no `cpufreq`
/// governor (`max_ceiling_hz == 0.0`: VMs and some ARM boards), the native
/// Clock row's own hide rule, so the row disappears rather than showing a flat,
/// meaningless `0 Hz`.
///
/// The aggregate is `max_hz` (the fastest core right now) and the ceiling is
/// `max_ceiling_hz` (the highest `cpuinfo_max_freq`) — the native row's fixed
/// "0→max-clock" axis. Split out of [`Sampler::tick`] so the mapping is pinned
/// without reading sysfs: swapping the ceiling for the current clock would pin
/// the page's Clock line to its top edge forever, and nothing else would see it
/// (#1414 review, MEDIUM 5).
fn clock_of(freq: &hytte_sensors::CpuFreq) -> (Option<f64>, Option<f64>) {
    if freq.max_ceiling_hz > 0.0 {
        (
            Some(finite_or_zero(freq.max_hz)),
            Some(finite_or_zero(freq.max_ceiling_hz)),
        )
    } else {
        (None, None)
    }
}

/// The per-core half of the CPU clock: each core's current frequency over the
/// shared `max_ceiling_hz`, as a `0.0..=1.0` fraction — or nothing on a
/// machine with no `cpufreq` governor, [`clock_of`]'s hide rule, so the two
/// halves of the Clock row appear and disappear together.
///
/// The native accumulator (`hytte-services`' `cpu_freq_per_core_history`)
/// divides the same two numbers. The fraction goes through [`as_unit`] here,
/// so it is clamped to the fixed axis the graph draws (a core reporting a
/// boost above `cpuinfo_max_freq` sits on the top rail, as the aggregate line
/// does — `panel::History::push` clamps that one) and a `NaN` reads as `0.0`
/// rather than defeating the render dedup (#896/#898).
fn per_core_clock_of(freq: &hytte_sensors::CpuFreq) -> Vec<f32> {
    if freq.max_ceiling_hz > 0.0 {
        freq.per_core
            .iter()
            .map(|&hz| as_unit(hz / freq.max_ceiling_hz))
            .collect()
    } else {
        Vec::new()
    }
}

/// An `f64` reading with no bounded range (a byte rate, a clock frequency) as
/// a finite value — `0.0` for anything non-finite.
///
/// Unlike [`as_unit`] this does **not** clamp to `0.0..=1.0`: a rate or a
/// clock has no unit ceiling, only a "must not be `NaN`/`±inf`" floor, which
/// is the one property that would otherwise defeat the runtime's render dedup
/// forever (#896/#898).
fn finite_or_zero(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

/// What [`drive`] needs of the thing it drives: read the machine, and forget
/// the cumulative baselines.
///
/// A trait rather than a hard-wired [`Sampler`] for one reason: the gate test.
/// `a_hidden_card_never_samples` is the *only* evidence for the module doc's
/// claim that nothing here reads the host's `/proc` — and with a hard-wired
/// sampler, the moment that test fails it is also the moment it reads the real
/// machine, and the moment it passes it proves nothing about *why* nothing
/// arrived (#1277 MEDIUM 2). A fake that counts its calls makes both halves
/// observable, and lets the positive control — the gate opens and a sample
/// reaches the reducer — exist at all.
///
/// Generic over its [`Reading`](Sample::Reading) since #1419 item 3, so the
/// Top apps walker ([`Walker`], whose reading is a [`TopApps`]) runs through
/// the same loop as the sensors [`Sampler`] rather than through a second copy
/// of it — the loop's ordering invariant (#1313) then exists once.
///
/// `Send + 'static` because the implementor is moved into and back out of a
/// `spawn_blocking`.
pub trait Sample: Send + 'static {
    /// What one tick produces.
    type Reading: Send + 'static;
    /// One reading of the machine. **Blocking.**
    fn tick(&mut self) -> Self::Reading;
    /// Drop the cumulative baselines — see [`Sampler::reset`].
    fn reset(&mut self);
}

impl Sample for Sampler {
    type Reading = Snapshot;

    // Not recursive: an inherent method shadows a trait method of the same
    // name in path resolution, so both of these are the `impl Sampler` bodies
    // above.
    fn tick(&mut self) -> Snapshot {
        Sampler::tick(self)
    }

    fn reset(&mut self) {
        Sampler::reset(self);
    }
}

/// Start everything this plugin samples with: the sensors [`Sampler`] on
/// `period` behind the surface's visibility, the Top apps [`Walker`] on
/// [`top_apps::POLL`] behind the page's, and the [`route`] that splits the
/// reducer's one command lane between them.
///
/// Returns the three tasks' handles, which production drops (the tasks end on
/// their own when the session's lane closes) and a test awaits.
pub fn spawn(
    cmds: CmdReceiver<Cmd>,
    msgs: CmdSender<Msg>,
    period: Duration,
    needs: Needs,
) -> [tokio::task::JoinHandle<()>; 3] {
    spawn_with(
        cmds,
        msgs,
        period,
        move || Sampler::new(needs),
        <Walker>::default,
    )
}

/// [`spawn`] over arbitrary samplers — the seam the composition tests drive
/// with counting fakes, so none of them reads the host's `/proc`.
pub(crate) fn spawn_with<S, W>(
    cmds: CmdReceiver<Cmd>,
    msgs: CmdSender<Msg>,
    period: Duration,
    make_sensors: impl FnMut() -> S + Send + 'static,
    make_walker: impl FnMut() -> W + Send + 'static,
) -> [tokio::task::JoinHandle<()>; 3]
where
    S: Sample<Reading = Snapshot>,
    W: Sample<Reading = TopApps>,
{
    let (sensors_tx, sensors_rx) = hytte_plugin::cmd_channel();
    let (walker_tx, walker_rx) = hytte_plugin::cmd_channel();
    [
        tokio::spawn(route(cmds, sensors_tx, walker_tx)),
        tokio::spawn(sampler_task_with(
            sensors_rx,
            msgs.clone(),
            period,
            make_sensors,
        )),
        tokio::spawn(drive(
            walker_rx,
            msgs,
            top_apps::POLL,
            |cmd: &Cmd| cmd.page(),
            Msg::TopApps,
            Keep {
                under: top_apps::KEEP_BASELINE,
                settle: top_apps::MIN_BASELINE_AGE,
            },
            make_walker,
        )),
    ]
}

/// What a reopen does with the last read's baseline — see [`drive`]'s
/// *A young baseline survives a quick reopen*.
#[derive(Clone, Copy, Debug)]
struct Keep {
    /// A baseline whose read started less than this before the reopen is
    /// kept; an older one is dropped.
    under: Duration,
    /// A kept baseline is read against no sooner than this after its read
    /// started: the reopen's first read is held until then.
    settle: Duration,
}

impl Keep {
    /// Keep nothing: every reopen re-baselines, however quick.
    const NOTHING: Self = Self {
        under: Duration::ZERO,
        settle: Duration::ZERO,
    };
}

/// Split the reducer's one command lane into the two tasks' lanes: the
/// surface's visibility to the sensors sampler, the page's to the walker.
///
/// Each task's classifier ([`Cmd::surface`], [`Cmd::page`]) would ignore the
/// other's command anyway; routing means neither lane carries a command its
/// task has no use for, and it is the one place the split is spelled out.
/// Ends — dropping both lanes, which ends both tasks — when the reducer's lane
/// closes, i.e. when the session tears down.
async fn route(mut cmds: CmdReceiver<Cmd>, sensors: CmdSender<Cmd>, walker: CmdSender<Cmd>) {
    while let Some(cmd) = cmds.recv().await {
        let lane = match cmd {
            Cmd::SetVisible(_) => &sensors,
            Cmd::PageVisible(_) => &walker,
        };
        // A task that has ended only ever did so because its lane closed,
        // which is this function's own teardown — nothing is left to gate.
        let _ = lane.send(cmd);
    }
}

/// The sensors sampler task: park while the card is off screen, sample on the
/// cadence while it is on, and post each sample to the reducer — [`drive`]
/// over the surface's visibility.
///
/// Drives [`Gate`] directly rather than calling
/// [`poll::gated`](hytte_plugin::poll::gated) because the [`Sampler`]'s caches
/// have to live **across** the awaits: `gated` takes a `FnMut() -> impl Future`
/// whose future cannot borrow the closure's captures, so the state would have
/// to be wrapped in an `Arc<Mutex<…>>` for no gain. The gate's own contract —
/// park on hidden, refresh on the hidden→visible **edge**, `MissedTickBehavior::Delay`,
/// a close beating a due tick, no cancellation of work in flight — is unchanged
/// and is what makes a closed sidebar genuinely free.
///
/// `make` is a **factory**, not a value, because the sampler is moved into the
/// `spawn_blocking` and is therefore gone if that task panics or is cancelled:
/// recovering from that without ending the whole task (see [`drive`]'s `Err`
/// arm) needs a way to build a fresh one. It is also the seam the gate tests
/// drive, so none of them touches the host's `/proc`.
async fn sampler_task_with<S: Sample<Reading = Snapshot>>(
    cmds: CmdReceiver<Cmd>,
    msgs: CmdSender<Msg>,
    period: Duration,
    make: impl FnMut() -> S + Send + 'static,
) {
    drive(
        cmds,
        msgs,
        period,
        |cmd: &Cmd| cmd.surface(),
        |snapshot| Msg::Sampled(Box::new(snapshot)),
        // Every reopen re-baselines, however quick: the sensors' reopen rule
        // is #1277 LOW 4's as it stands, and #1427 does not touch it.
        Keep::NOTHING,
        make,
    )
    .await;
}

/// The gated sampling loop both tasks run: park while `visibility` says
/// closed, read on the open edge and then on the cadence, re-baseline on a
/// reopen, and post each reading to the reducer through `wrap`.
///
/// # Invariant: the reset happens-before the read it guards (#1313)
///
/// On the hidden→visible edge, a due tick can never be handed back ahead of
/// the edge that owes the re-baseline — this is a property of the code, not
/// of scheduler luck. Two things make it so, and both live one level down in
/// [`hytte_plugin::poll::Gate::next`]: the cadence's `, if open` guard means
/// `interval.tick()` is not even polled while the gate is closed (so nothing
/// can be "due ahead of" an edge that hasn't happened yet), and `biased;`
/// means that once open, a simultaneously-ready visibility command always
/// wins a tie against a due tick. Either way, [`Wake::Refresh`] for the open
/// edge is returned only *after* `Gate` has already reset its own interval —
/// so by the time this loop's `Wake::Refresh` arm runs, the cadence is
/// already a full period out. This task's own `parked.swap(…)` re-baseline
/// (below) then runs synchronously, strictly before the `spawn_blocking` read
/// it gates, on the same thread — so the first frame after a reopen is always
/// measured over a fresh window (#1277 LOW 4), never over the mean of however
/// long the sidebar was shut.
///
/// What this invariant does **not** cover: a *regular* cadence tick's own
/// `spawn_blocking` read runs on a real blocking-pool thread and can complete
/// at any wall-clock moment, independent of this task's own progress. A test
/// that infers "the reopen happened" from a rising tick *count* rather than
/// from the reset itself can be fooled by a late-finishing, unrelated tick
/// from before the close — which is a test-observation hazard, not a
/// violation of this invariant. See
/// `reopening_the_sidebar_re_baselines_before_it_reads`'s own notes and
/// `a_tick_due_while_hidden_never_reads_before_the_unpark_reset`, which pins
/// the ordering directly instead.
///
/// # A young baseline survives a quick reopen (`keep`)
///
/// The reopen re-baseline exists so the first reading after a reopen is not
/// the mean over however long the gate was shut. A baseline read less than
/// `keep.under` before the reopen is not that: a delta from it is a real
/// reading over a short window. So a reopen re-baselines only when the last
/// read started `keep.under` or longer ago (or there was none). The walker
/// passes [`top_apps::KEEP_BASELINE`]: a page closed and reopened within a
/// couple of walks gets a CPU list on its first walk instead of `—` for a
/// whole cadence (the #1426 review's NIT 8). The sensors pass
/// [`Keep::NOTHING`]: `elapsed() < ZERO` is never true.
///
/// A kept baseline also has a floor, `keep.settle`: the reopen's first read
/// is **held** until the baseline's read started at least that long ago, so
/// it starts at `max(reopen, last read's start + settle)`. Without it a close
/// just after a read started and a reopen a moment later would measure the
/// first CPU list over a few hundred milliseconds, where whole-tick jiffy
/// counts are noisiest (the #1437 review's LOW 1). The walker passes
/// [`top_apps::MIN_BASELINE_AGE`].
///
/// While a read is held the loop still reads the lane through the gate, lane
/// first (`biased;`, as the gate itself does). `Gate::next` is cancel-safe:
/// it awaits only a channel `recv` and an `Interval::tick`, and handles a
/// command it has taken before it awaits again, so dropping it when the hold
/// ends loses nothing. A close during the hold is absorbed by the gate, so
/// when the hold ends the gate is closed and the read is dropped. A close and
/// a reopen during the hold hand back a fresh open edge, which is decided
/// afresh. The held read stands in for the open edge's, so the cadence
/// restarts from it (`Gate::reset`), as the gate restarts it from an edge.
///
/// The age is measured on tokio's clock from the moment a read is started,
/// which is no later than the moment its baseline was taken — so a kept
/// baseline is at most `keep.under` old, never older.
async fn drive<S: Sample>(
    cmds: CmdReceiver<Cmd>,
    msgs: CmdSender<Msg>,
    period: Duration,
    visibility: fn(&Cmd) -> Option<bool>,
    wrap: fn(S::Reading) -> Msg,
    keep: Keep,
    mut make: impl FnMut() -> S + Send + 'static,
) {
    /// What woke the loop: the gate, or the end of a held read.
    enum Due<C> {
        Gate(Option<Wake<C>>),
        Held,
    }

    // The gate absorbs every visibility command itself and answers only the
    // *open* edge, so the loop below cannot otherwise see a close — and a
    // close is exactly what invalidates the cumulative baseline (#1277 LOW 4).
    // The classifier is the one place that sees every visibility command, so
    // it raises the flag and the next refresh lowers it. An `AtomicBool`
    // rather than a `Cell` because the task is `tokio::spawn`ed and must be
    // `Send`.
    let parked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen_close = std::sync::Arc::clone(&parked);
    let mut gate = Gate::new(cmds, period, move |cmd: &Cmd| {
        let want = visibility(cmd);
        if want == Some(false) {
            seen_close.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        want
    });
    let mut sampler = make();
    // When the last read started — the age of the baseline it left, for
    // `keep`. `None` before the first read and after a sampler is rebuilt.
    let mut last_read: Option<tokio::time::Instant> = None;
    // A reopen's first read, held until its kept baseline is `keep.settle`
    // old: the instant it may start.
    let mut held: Option<tokio::time::Instant> = None;
    loop {
        let due = match held {
            None => Due::Gate(gate.next().await),
            Some(at) => tokio::select! {
                // The lane first, as in the gate: a close already queued when
                // the hold ends is taken before the read, and cancels it.
                biased;
                wake = gate.next() => Due::Gate(wake),
                () = tokio::time::sleep_until(at) => Due::Held,
            },
        };
        match due {
            // The lane closed: the session is tearing down.
            Due::Gate(None) => return,
            // The other task's command: `route` never sends one down this
            // lane, and if it did, it would not be this task's business. The
            // classifiers (`Cmd::surface`, `Cmd::page`) are exhaustive
            // matches without a wildcard, so a new `Cmd` variant is a compile
            // error there — the place to decide which gate it opens — rather
            // than a command silently dropped here.
            Due::Gate(Some(Wake::Cmd(_))) => continue,
            Due::Gate(Some(Wake::Refresh)) => {
                // A fresh edge (or a tick) decides for itself: whatever was
                // held is superseded.
                held = None;
                let kept = last_read.filter(|at| at.elapsed() < keep.under);
                // `swap` first, always: the flag must come down on this
                // refresh whether or not the baseline is kept.
                let reopened = parked.swap(false, std::sync::atomic::Ordering::Relaxed);
                if reopened {
                    match kept {
                        // Re-baseline: the first frame after an unpark must
                        // be a load measured over a fresh window, not the mean
                        // over however long the sidebar was shut.
                        None => sampler.reset(),
                        // Kept, but too young to read against yet: hold the
                        // read until it is `keep.settle` old.
                        Some(at) => {
                            let ready = at + keep.settle;
                            if ready > tokio::time::Instant::now() {
                                held = Some(ready);
                                continue;
                            }
                        }
                    }
                }
            }
            Due::Held => {
                held = None;
                // A close during the hold was absorbed by the gate: the page
                // is gone, and so is the read it was owed.
                if !gate.is_visible() {
                    continue;
                }
                // This read stands in for the open edge's: restart the cadence
                // from it, as the gate restarts it from an edge.
                gate.reset();
            }
        }
        last_read = Some(tokio::time::Instant::now());
        // The sampler is moved into the blocking closure and handed back with
        // the snapshot, which is what keeps its caches warm without an
        // `Arc<Mutex<…>>` around a value only this task ever touches.
        let joined = tokio::task::spawn_blocking(move || {
            let reading = sampler.tick();
            (sampler, reading)
        })
        .await;
        match joined {
            Ok((back, reading)) => {
                sampler = back;
                if msgs.send(wrap(reading)).is_err() {
                    // The reducer is gone: the session is tearing down.
                    return;
                }
            }
            Err(e) => {
                // The blocking task was cancelled or panicked. Both are our
                // bug rather than a machine state, so say so — and then carry
                // on with cold caches rather than ending the task, because
                // ending it freezes the card for the rest of the session while
                // leaving the process looking healthy. One tick of stale CPU
                // deltas is the cost.
                tracing::warn!(error = %e, "stats sampler tick failed; restarting its caches");
                sampler = make();
                // A fresh sampler has no baseline to keep.
                last_read = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cmd, Msg, Needs, Sample, Sampler, Snapshot, as_unit, clock_of, cpu_half, per_core_clock_of,
        route, sampler_task_with, spawn, spawn_with,
    };
    use crate::config::Card;
    use crate::top_apps::{TopApps, Walker};
    use hytte_plugin::cmd_channel;
    use hytte_sensors::app_usage::ProcSample;

    /// The Clock row's two numbers are the **fastest core now** over the
    /// **highest `cpuinfo_max_freq`** — the native row's fixed 0→max-clock
    /// axis — and a machine with no `cpufreq` governor has neither.
    ///
    /// **Falsified** by taking the ceiling from `max_hz` (the page's Clock line
    /// then sits on its top edge forever), or by dropping the zero-ceiling hide.
    #[test]
    fn the_clock_is_the_fastest_core_over_the_highest_ceiling() {
        let freq = hytte_sensors::CpuFreq {
            max_hz: 3_800_000_000.0,
            per_core: vec![1_200_000_000.0, 3_800_000_000.0],
            max_ceiling_hz: 5_000_000_000.0,
        };
        assert_eq!(
            clock_of(&freq),
            (Some(3_800_000_000.0), Some(5_000_000_000.0))
        );
        assert_eq!(
            clock_of(&hytte_sensors::CpuFreq::default()),
            (None, None),
            "no governor, no Clock row",
        );
        let poisoned = hytte_sensors::CpuFreq {
            max_hz: f64::NAN,
            ..freq
        };
        assert_eq!(clock_of(&poisoned), (Some(0.0), Some(5_000_000_000.0)));
    }

    /// **The per-core clock is each core over the shared ceiling** (#1419
    /// item 2) — the native expanded Clock row's `hz / max_ceiling_hz` per
    /// core, in core order, on the fixed `0..=1` axis — and a machine with no
    /// `cpufreq` governor has no series at all, the aggregate's hide rule.
    ///
    /// **Falsified** by dividing by the fastest core (`max_hz`) instead of the
    /// ceiling (every series then peaks at the top rail), by dropping the
    /// zero-ceiling hide, or by skipping `as_unit` (the boosted core leaves the
    /// axis and the `NaN` one reaches the tree).
    #[test]
    fn the_per_core_clock_is_each_core_over_the_shared_ceiling() {
        let freq = hytte_sensors::CpuFreq {
            max_hz: 4_000_000_000.0,
            per_core: vec![1_250_000_000.0, 4_000_000_000.0, 5_500_000_000.0, f64::NAN],
            max_ceiling_hz: 5_000_000_000.0,
        };
        assert_eq!(per_core_clock_of(&freq), vec![0.25, 0.8, 1.0, 0.0]);
        assert_eq!(
            per_core_clock_of(&hytte_sensors::CpuFreq {
                max_ceiling_hz: 0.0,
                ..freq
            }),
            Vec::<f32>::new(),
            "no governor, no per-core clock",
        );
    }
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// What a [`Sample`] did, shared with the test that installed it.
    ///
    /// The counters are what make the gate observable: "nothing arrived on the
    /// message lane" and "the sampler was never called" are different claims,
    /// and only the second one is about the gate (#1277 MEDIUM 2).
    ///
    /// `sequence` exists because the counters alone cannot tell "the reopen's
    /// own read" apart from an unrelated one that happens to land afterwards
    /// (#1313): `tick()` runs on a real blocking-pool thread and can complete
    /// — and bump `ticks` — at any wall-clock moment relative to the sampler
    /// task's own progress, so a regular cadence tick from *before* a close
    /// can finish late and push `ticks()` past whatever a test captured as
    /// "before the reopen", with no reset anywhere near it. A recorded order
    /// is the only thing that survives that: it is what
    /// [`a_tick_due_while_hidden_never_reads_before_the_unpark_reset`] checks
    /// instead of a count.
    #[derive(Debug, Default)]
    struct Calls {
        ticks: AtomicUsize,
        resets: AtomicUsize,
        sequence: std::sync::Mutex<Vec<&'static str>>,
    }

    impl Calls {
        fn ticks(&self) -> usize {
            self.ticks.load(Ordering::SeqCst)
        }

        fn resets(&self) -> usize {
            self.resets.load(Ordering::SeqCst)
        }

        fn record(&self, what: &'static str) {
            self.sequence.lock().expect("not poisoned").push(what);
        }

        fn sequence(&self) -> Vec<&'static str> {
            self.sequence.lock().expect("not poisoned").clone()
        }
    }

    /// A sampler that reads nothing at all and counts what it was asked to do.
    struct FakeSampler(Arc<Calls>);

    impl Sample for FakeSampler {
        type Reading = Snapshot;

        fn tick(&mut self) -> Snapshot {
            // Recorded *before* the counter: a test polls `ticks()` in a busy
            // loop (`pump_until`), and the two writes are not one atomic
            // operation — recording first guarantees the sequence log already
            // has this entry by the moment any observer sees the counter
            // move, rather than leaving a window where `ticks() >= 1` is true
            // but `sequence()` has not caught up yet (measured: 3/200 idle
            // runs of `a_tick_due_while_hidden_never_reads_before_the_unpark_reset`
            // saw exactly that gap before this ordering was fixed).
            self.0.record("tick");
            let n = self.0.ticks.fetch_add(1, Ordering::SeqCst);
            // A different reading each tick (from a literal table, so there is
            // no cast the pedantic lints would refuse), so a test can tell one
            // sample from the next on the wire.
            let cpu = [0.1_f32, 0.2, 0.3, 0.4, 0.5][n % 5];
            Snapshot {
                cpu: Some(cpu),
                per_core: vec![0.25, 0.5],
                cpu_temp_c: Some(42.0),
                ..Snapshot::default()
            }
        }

        fn reset(&mut self) {
            // Same ordering reason as `tick` above, even though nothing here
            // currently busy-polls `resets()` — keeping both consistent means
            // nobody has to rediscover this the same way twice.
            self.0.record("reset");
            self.0.resets.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Turn the runtime over until `done` answers true, or give up after a
    /// **wall-clock** budget.
    ///
    /// The sample path crosses a `spawn_blocking` — a real blocking-pool thread
    /// — so a single `yield_now` measures scheduling luck rather than the gate
    /// (#1277 MEDIUM 2: with one yield, deleting the `, if open` guard from
    /// `hytte_plugin::poll::Gate` left `a_hidden_card_never_samples` green).
    /// The budget is wall-clock because a blocking thread is on no other
    /// clock: the test's virtual time says nothing about when it finishes.
    async fn pump_until(mut done: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            for _ in 0..50 {
                if done() {
                    return true;
                }
                tokio::task::yield_now().await;
            }
            if std::time::Instant::now() >= deadline {
                return done();
            }
            // Virtual time does not auto-advance while this loop keeps the
            // runtime busy, so the cadence would never fire: nudge it by a
            // fraction of the shortest period any test here uses.
            tokio::time::advance(Duration::from_millis(200)).await;
        }
    }

    /// Pump until one message lands on the reducer's lane.
    ///
    /// Separate from a counter assertion on purpose: the counter is bumped
    /// *inside* the blocking closure, so "the sampler was called" is true a
    /// scheduler turn before "the sample reached the reducer". The two claims
    /// are different and the tests below make both.
    async fn next_sample(rx: &mut hytte_plugin::CmdReceiver<Msg>) -> Option<Msg> {
        let mut got = None;
        pump_until(|| {
            if got.is_none() {
                got = rx.try_recv().ok();
            }
            got.is_some()
        })
        .await;
        got
    }

    /// Turn the runtime over a fixed number of times — the negative half, where
    /// there is no event to wait *for*. Ten virtual periods, each followed by
    /// enough scheduler turns that a sample would comfortably have landed;
    /// `a_hidden_card_never_samples` proves that budget is enough by using the
    /// same one for its own positive control.
    async fn pump_ten_periods(period: Duration) {
        for _ in 0..10 {
            tokio::time::advance(period).await;
            for _ in 0..200 {
                tokio::task::yield_now().await;
            }
        }
    }

    /// The one normalisation the sampler does, including the `NaN` case that
    /// would otherwise defeat the runtime's render dedup forever.
    ///
    /// `float_cmp` is allowed here because every value asserted is an **exact**
    /// output of a clamp or a literal round-trip — `0.0`, `1.0`, and the `0.5`
    /// that is exactly representable — not the result of arithmetic. An
    /// epsilon comparison would weaken the `NaN` row in particular, which is
    /// the whole point of this test.
    #[allow(clippy::float_cmp)]
    #[test]
    fn a_load_is_clamped_into_the_unit_range_and_a_nan_reads_as_rest() {
        assert!((as_unit(0.5) - 0.5).abs() < 1e-6);
        assert_eq!(as_unit(0.0), 0.0);
        assert_eq!(as_unit(1.0), 1.0);
        assert_eq!(as_unit(-3.0), 0.0);
        assert_eq!(as_unit(7.5), 1.0);
        assert_eq!(as_unit(f64::INFINITY), 1.0);
        assert_eq!(as_unit(f64::NEG_INFINITY), 0.0);
        let nan = as_unit(f64::NAN);
        assert!(!nan.is_nan(), "a NaN reading must not reach the view");
        assert_eq!(nan, 0.0);
    }

    /// A default [`Snapshot`] is the "nothing measured yet" state the card
    /// renders dashes for — pinned because the seed render goes out before the
    /// first sample can possibly have landed.
    ///
    /// `float_cmp`: `0.0` here is `f32::default()`, an exact literal.
    #[test]
    fn the_default_snapshot_is_the_nothing_yet_state() {
        let s = Snapshot::default();
        assert_eq!(s.cpu, None, "the headline dashes rather than reading 0%");
        assert!(s.per_core.is_empty());
        assert!(s.cpu_temp_c.is_none());
        assert!(s.gpu.is_none());
        assert!(s.processes.is_none());
        assert!(s.cpu_clock_hz.is_none());
        assert!(s.cpu_clock_ceiling_hz.is_none());
        assert!(s.disk_io.is_none());
    }

    /// **The first tick draws dashes** — the claim four doc comments make, made
    /// true and then pinned against kernel-shaped data (#1277 MEDIUM 3).
    ///
    /// Driven off a `/proc/stat`-shaped fixture, never the file: the aggregate
    /// line plus four cores, as `(active, total)` jiffy counters. What makes
    /// this worth a test is the row below it — `hytte_sensors::compute_cpu_load`
    /// answers a cold `prev` with a **full-length** `per_core` of zeroes, so
    /// without [`cpu_half`]'s branch the card's first frame is four idle lamps
    /// and a `CPU 0%` headline: a measurement it never made, rendered as
    /// confidently as a real one.
    #[test]
    fn the_first_tick_withholds_its_reading_instead_of_publishing_an_idle_one() {
        let first = [(400, 1_000), (100, 250), (100, 250), (100, 250), (100, 250)];

        // The upstream behaviour this branch exists to correct, pinned so a
        // change to `hytte-sensors` that made the branch redundant shows up
        // here rather than being silently carried forever.
        let raw = hytte_sensors::compute_cpu_load(&[], &first);
        assert_eq!(
            raw.per_core.len(),
            4,
            "compute_cpu_load answers a cold prev with one zero per core, not nothing",
        );

        let (cpu, per_core) = cpu_half(&[], &first);
        assert_eq!(cpu, None, "the headline has nothing to report yet");
        assert!(per_core.is_empty(), "…and neither has the lamp row");
        assert_eq!(crate::card::percent_text(cpu), "—");
        assert_eq!(crate::card::lamp_rows(&per_core), vec!["----".to_owned()]);

        // The *second* tick has a delta, and it is a real one: core 1 spent the
        // whole window busy and core 2 none of it.
        let second = [
            (700, 2_000),
            (350, 1_250),
            (100, 250),
            (100, 250),
            (100, 250),
        ];
        let (cpu2, per_core2) = cpu_half(&first, &second);
        assert!(cpu2.is_some(), "the second tick reports");
        assert_eq!(per_core2.len(), 4);
        assert!(
            (per_core2[0] - 0.25).abs() < 1e-6,
            "core 0 was busy a quarter of the window: {per_core2:?}",
        );
        assert!(
            per_core2[1..].iter().all(|l| l.abs() < 1e-6),
            "the idle cores are idle, not absent: {per_core2:?}",
        );
        assert_ne!(
            crate::card::lamp_rows(&per_core2),
            vec!["----".to_owned()],
            "…and the row is lamps now, not dashes",
        );

        // An unreadable `/proc/stat` is the same "nothing measured" state, not
        // a zero: a `now` of nothing cannot be subtracted from either.
        assert_eq!(cpu_half(&first, &[]), (None, Vec::new()));
    }

    /// A [`Sampler`] can be constructed with cold caches and costs nothing
    /// until it is ticked — which is what lets the task own one before the
    /// gate has ever opened.
    #[test]
    fn a_fresh_sampler_has_cold_caches() {
        let s = Sampler::new(Needs::default());
        assert!(format!("{s:?}").contains("prev_cpu: []"));
    }

    /// **The needs table**: what a resolved `stats.toml` table asks the sampler
    /// to read, and — the point of the type — what it asks it *not* to.
    ///
    /// Pure, so the expensive reads it gates (`nvidia-smi` on an Nvidia box, a
    /// mount walk plus one `statvfs` per filesystem) are decided by a function
    /// a test can drive rather than by a branch only a live machine reaches.
    ///
    /// **Falsified** by having `Needs::of` answer `Self { cpu: true,
    /// temperature: true, gpu: true, memory: true, disk: true }`
    /// unconditionally.
    #[test]
    fn the_needs_follow_the_table() {
        // The two shipped tables, spelled out rather than compared to
        // `Needs::of` of themselves.
        assert_eq!(
            Needs::of(Card::sidebar_default()),
            Needs {
                cpu: true,
                temperature: true,
                gpu: true,
                memory: false,
                disk: false,
            },
            "the compact sidebar card reads no memory and walks no mount table",
        );
        assert_eq!(
            Needs::of(Card::bar_default()),
            Needs {
                cpu: true,
                temperature: true,
                gpu: true,
                memory: true,
                disk: true,
            },
        );

        // Each switch gates its own read.
        let off = |f: fn(&mut Card)| {
            let mut cfg = Card::bar_default();
            f(&mut cfg);
            Needs::of(cfg)
        };
        assert!(!off(|c| c.cpu = false).cpu);
        assert!(!off(|c| c.gpu = false).gpu);
        assert!(!off(|c| c.memory = false).memory);
        assert!(!off(|c| c.disk = false).disk);
        assert!(!off(|c| c.temperature = false).temperature);

        // …and the one pair: with neither half of the card drawn there is
        // nowhere to put a temperature, so the hwmon chip is not resolved.
        assert!(
            !off(|c| {
                c.cpu = false;
                c.gpu = false;
            })
            .temperature,
        );
        assert!(
            off(|c| c.cpu = false).temperature,
            "…but a GPU-only surface still shows the adapter's",
        );
    }

    /// **The four (five, since #1295's MED 1) expensive reads are gated in
    /// `tick`, not merely derived by `Needs::of`**: with every need off, the
    /// snapshot carries no temperature, no GPU, no memory, no mounts, no
    /// process count and no CPU clock. This is the only thing that makes
    /// `DEFAULT_TOML`'s "turning either off also stops this instance from
    /// sampling the corresponding sensor" true rather than merely written
    /// down.
    ///
    /// It builds a real [`Sampler`], which reads `/proc/stat` (the one
    /// ungated read) and nothing else — it asserts about the gates, never
    /// about the machine. Lifted from the #1295 review's MED 2, whose two
    /// assertions (`cpu_temp_c`, `gpu`, `memory`, `disks`) are unchanged; the
    /// `processes` / `cpu_clock_hz` / `disk_io` assertions are this round's
    /// extension for the fifth gate MED 1 added.
    ///
    /// **Falsified** by deleting any one of `tick`'s five `if self.needs.*`.
    #[test]
    fn a_sampler_that_needs_nothing_reads_nothing_but_proc_stat() {
        let snap = Sampler::new(Needs::default()).tick();
        assert_eq!(snap.cpu_temp_c, None, "the hwmon read_dir is gated");
        assert_eq!(snap.gpu, None, "the nvidia-smi fork is gated");
        assert_eq!(snap.memory, None, "/proc/meminfo is gated");
        assert!(snap.disks.is_empty(), "the mount walk + statvfs are gated");
        assert_eq!(snap.processes, None, "the /proc read_dir walk is gated");
        assert_eq!(snap.cpu_clock_hz, None, "the cpufreq sysfs walk is gated");
        assert_eq!(snap.cpu_clock_ceiling_hz, None, "…and so is its ceiling");
        assert!(
            snap.per_core_clock.is_empty(),
            "…and the per-core clock (#1419 item 2)"
        );
        assert_eq!(snap.disk_io, None, "the /proc/diskstats read is gated");
    }

    /// **The gate**: while the surface is hidden, nothing is sampled at all —
    /// and the same pump, with the gate open, *does* see a sample.
    ///
    /// The control is not decoration. Without it "nothing arrived" is
    /// indistinguishable from "nothing had a chance to arrive": the sample path
    /// crosses a `spawn_blocking`, so an impatient test passes with the gate
    /// deleted (#1277 MEDIUM 2 — exactly what happened, at 47 passed / 0
    /// failed). Running the control *after* the negative half, through the same
    /// [`pump_ten_periods`] budget, is what makes the negative half evidence.
    ///
    /// The assertion is on the **sampler's call count**, not on the message
    /// lane: the claim is that the machine was never read, and a task that
    /// sampled and then failed to send would satisfy the weaker one.
    ///
    /// **Falsified** by dropping the `, if open` guard inside
    /// `hytte_plugin::poll::Gate` — the hidden half then counts ticks.
    #[tokio::test(start_paused = true)]
    async fn a_hidden_card_never_samples() {
        let period = Duration::from_secs(1);
        let calls = Arc::new(Calls::default());
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, mut msg_rx) = cmd_channel::<Msg>();
        let made = Arc::clone(&calls);
        let task = tokio::spawn(sampler_task_with(cmd_rx, msg_tx, period, move || {
            FakeSampler(Arc::clone(&made))
        }));

        cmd_tx.send(Cmd::SetVisible(false)).expect("lane is live");
        pump_ten_periods(period).await;

        assert_eq!(calls.ticks(), 0, "a hidden card must not sample at all");
        assert!(
            msg_rx.try_recv().is_err(),
            "…and nothing can have reached the reducer either",
        );

        // The control: the same budget, with the gate open.
        cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");
        assert!(
            pump_until(|| calls.ticks() > 0).await,
            "the pump above must be long enough to see a sample when there is one \
             — otherwise the negative half proves nothing",
        );

        drop(cmd_tx);
        let _ = task.await;
    }

    /// **The happy path**: the gate opens, the machine is read, and the sample
    /// reaches the reducer as a `Msg::Sampled` carrying what the sampler
    /// produced.
    ///
    /// Nothing executed this before #1277 — no test ever sent
    /// `SetVisible(true)` — so a `Cmd` classifier that answered `None` would
    /// have left the gate shut forever, the card frozen on its seed render, and
    /// the whole suite green.
    ///
    /// **Falsified** by having the classifier answer `None`: the gate never
    /// opens and no sample arrives.
    #[tokio::test(start_paused = true)]
    async fn an_open_card_samples_on_the_edge_and_again_on_the_cadence() {
        let period = Duration::from_secs(1);
        let calls = Arc::new(Calls::default());
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, mut msg_rx) = cmd_channel::<Msg>();
        let made = Arc::clone(&calls);
        let task = tokio::spawn(sampler_task_with(cmd_rx, msg_tx, period, move || {
            FakeSampler(Arc::clone(&made))
        }));

        // The hidden→visible edge owes an immediate refresh.
        cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");
        let first = next_sample(&mut msg_rx)
            .await
            .expect("the open edge must sample immediately and the sample must reach the reducer");
        let Msg::Sampled(snapshot) = first else {
            panic!("the sensors lane carries sensor samples, not a Top apps walk");
        };
        assert_eq!(snapshot.per_core.len(), 2);
        assert!(
            (snapshot.per_core[0] - 0.25).abs() < 1e-6 && (snapshot.per_core[1] - 0.5).abs() < 1e-6,
            "…carrying what the sampler produced, not a default: {:?}",
            snapshot.per_core,
        );
        assert!(snapshot.cpu.is_some());

        // …and the cadence keeps going while it stays open: a *second* sample
        // reaches the reducer, and it is a second read of the machine rather
        // than a replay of the first.
        assert!(
            next_sample(&mut msg_rx).await.is_some(),
            "the cadence must keep sampling while the card is on screen",
        );
        assert!(calls.ticks() >= 2, "…by reading the machine again");

        drop(cmd_tx);
        let _ = task.await;
    }

    /// **The park invalidates the baseline** (#1277 LOW 4): a hidden→visible
    /// edge resets the sampler before it reads, so the first frame on reopen is
    /// a load measured over a fresh window rather than the mean over however
    /// long the sidebar was shut.
    ///
    /// The gate absorbs every `SetVisible` itself, so the only place that sees
    /// a *close* is the classifier — which is why this is a flag and not a
    /// `Wake` arm, and why it needs a test of its own.
    ///
    /// **#1313:** the assertion used to be `assert!(pump_until(|| ticks() >
    /// before)); assert_eq!(resets(), 1)` — waiting for *any* tick past
    /// `before`, then checking the reset. That is order-dependent for a
    /// reason that has nothing to do with `Gate`'s own ordering: `tick()`
    /// runs on a real blocking-pool thread, so a *regular* cadence tick from
    /// the first open window (queued the instant virtual time crossed its
    /// period, independently of anything below) can finish late — after the
    /// close, and (while the wait below was the hand-stepped
    /// `pump_ten_periods`, before #1427) even after the reopen is sent — and
    /// bump `ticks()` past `before` with no reset anywhere near it, because it
    /// was never the reopen's own read. Measured: 25/200 runs (12.5%) red
    /// under `taskset -c 0-3` plus four pinned burners on those cores, 0/200
    /// idle — see the PR body. The fix checks the direct signal (`resets()`)
    /// right after the edge, before ever asking about a tick, so a stale
    /// tick landing late cannot be mistaken for the reopen's own.
    ///
    /// **Falsified** by:
    /// - deleting the `parked.swap(…)` branch: `resets` stays 0, every run;
    /// - deleting `hytte_plugin::poll::Gate::next`'s `biased;`: 10/200 red
    ///   under the same contention (0/200 idle) — the mechanism this test was
    ///   originally fooled by is exactly what `biased` prevents: without it,
    ///   a close racing a simultaneously-due tick from the still-open first
    ///   window can lose the race and get deferred behind an extra,
    ///   unrelated tick, delaying `parked`'s own reset past this test's
    ///   direct check.
    #[tokio::test(start_paused = true)]
    async fn reopening_the_sidebar_re_baselines_before_it_reads() {
        let period = Duration::from_secs(1);
        let calls = Arc::new(Calls::default());
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, _msg_rx) = cmd_channel::<Msg>();
        let made = Arc::clone(&calls);
        let task = tokio::spawn(sampler_task_with(cmd_rx, msg_tx, period, move || {
            FakeSampler(Arc::clone(&made))
        }));

        cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");
        assert!(pump_until(|| calls.ticks() >= 1).await);
        assert_eq!(
            calls.resets(),
            0,
            "the first open is a cold sampler already — nothing to re-baseline",
        );
        let before = calls.ticks();

        cmd_tx.send(Cmd::SetVisible(false)).expect("lane is live");
        // Ten periods by the runtime's own auto-advance, not
        // `pump_ten_periods`: the paused clock cannot move while a read is
        // still on the blocking pool (tokio inhibits auto-advance for a
        // pending `spawn_blocking` on a current-thread runtime), so this
        // returns only after any read in flight at the close has landed and
        // the task has taken the close. The hand-stepped pump never waited in
        // real time, and with a read starved on one loaded core the reopen
        // below reached a task still inside that read: `resets()` read 0
        // (measured on #1427: 2/30 release runs pinned to one core beside
        // four burners).
        tokio::time::sleep(period * 10).await;
        cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");

        // Check the reset *directly*, right after the edge, before ever
        // asking about a tick — one yield is enough, because processing an
        // already-queued visibility command needs no real work (no
        // `spawn_blocking`) before the reset runs; a `pump_until` here would
        // be exactly the proxy that let a stale tick fool this test before.
        tokio::task::yield_now().await;
        assert_eq!(
            calls.resets(),
            1,
            "the re-open must drop the stale /proc/stat baseline exactly once, \
             and it must have done so before anything else observable happens",
        );

        assert!(pump_until(|| calls.ticks() > before).await);
        assert_eq!(
            calls.resets(),
            1,
            "…and still exactly once by the time the reopen's own read lands",
        );

        drop(cmd_tx);
        let _ = task.await;
    }

    /// **A quick sidebar reopen still re-baselines** — the sensors keep no
    /// baseline across a reopen, however young (#1427 gave only the Top apps
    /// walker a `KEEP_BASELINE` window; the sensors' reopen rule is #1277 LOW
    /// 4's as it stands). Closed and reopened with no time passing at all,
    /// the sampler is reset before its next read.
    ///
    /// Waits with [`settle_until`], so no virtual time passes between the
    /// close and the reopen: a young-baseline window of any length would keep
    /// this baseline.
    ///
    /// **Falsified** by handing the sensors' `drive` the walker's
    /// `top_apps::KEEP_BASELINE` in place of `Duration::ZERO`.
    #[tokio::test(start_paused = true)]
    async fn a_quick_sidebar_reopen_still_re_baselines() {
        let period = Duration::from_secs(1);
        let calls = Arc::new(Calls::default());
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, mut msg_rx) = cmd_channel::<Msg>();
        let made = Arc::clone(&calls);
        let task = tokio::spawn(sampler_task_with(cmd_rx, msg_tx, period, move || {
            FakeSampler(Arc::clone(&made))
        }));

        cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");
        // The open edge's sample has reached the reducer, so the task is back
        // at its gate with a baseline zero seconds old.
        assert!(
            settle_until(|| msg_rx.try_recv().is_ok()).await,
            "the open edge samples",
        );
        assert_eq!(calls.resets(), 0, "the first open has nothing to drop");

        cmd_tx.send(Cmd::SetVisible(false)).expect("lane is live");
        cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");
        assert!(
            settle_until(|| calls.resets() >= 1).await,
            "a zero-second close must still drop the sensors' baseline",
        );
        assert_eq!(calls.resets(), 1, "exactly once");

        drop(cmd_tx);
        let _ = task.await;
    }

    /// **The ordering, pinned directly rather than inferred from a count**
    /// (#1313): a tick that was "due" while the gate was hidden — by the
    /// wall the interval would have crossed, had it been polled — must not
    /// produce a read before the unpark reset. `Calls::sequence()` records
    /// `"tick"`/`"reset"` in the exact order they happened, which is what
    /// lets this test tell the true order apart from a count that a stale
    /// event could satisfy by coincidence (see the sibling test's #1313
    /// note).
    ///
    /// The gate starts closed, so a single close→ten-periods→open drives
    /// exactly one edge with no earlier open to leave a stray tick in
    /// flight — the one thing that made the sibling test's `ticks() >
    /// before` proxy foolable is structurally absent here.
    ///
    /// This task's own `parked.swap(…)` reset runs synchronously, strictly
    /// before the `spawn_blocking` read it gates — that half is exercised and
    /// falsified directly below. The other half of the invariant — that
    /// [`hytte_plugin::poll::Gate::next`]'s `, if open` guard and `biased;`
    /// ordering are what keep a due tick from ever being handed back ahead of
    /// the edge in the first place — lives one crate over and is exercised
    /// there and by the sibling test, not by this one: this test starts
    /// hidden and drives exactly one edge, with no earlier open to leave a
    /// stray tick racing the close the way the sibling test's history shows
    /// (see its own #1313 note) — so `biased` has nothing to arbitrate here.
    /// Measured (200 runs, `taskset -c 0-3` plus four pinned burners): both
    /// deleting `biased;` and deleting `Gate`'s `self.reset()` leave this
    /// test green throughout; the sibling test is what catches the former,
    /// and `hytte_plugin::poll`'s own suite catches the latter (below).
    ///
    /// **Falsified** by deleting the `parked.swap(…)` branch here:
    /// `calls.sequence()` never contains `"reset"` at all.
    #[tokio::test(start_paused = true)]
    async fn a_tick_due_while_hidden_never_reads_before_the_unpark_reset() {
        let period = Duration::from_secs(1);
        let calls = Arc::new(Calls::default());
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, _msg_rx) = cmd_channel::<Msg>();
        let made = Arc::clone(&calls);
        let task = tokio::spawn(sampler_task_with(cmd_rx, msg_tx, period, move || {
            FakeSampler(Arc::clone(&made))
        }));

        // The gate starts closed; a redundant close is a level, not an edge,
        // but the classifier flags it regardless — any close invalidates the
        // next open, not only a close that followed an open.
        cmd_tx.send(Cmd::SetVisible(false)).expect("lane is live");

        // Ten periods of virtual time with the gate never open. Were the
        // interval polled at all here, `MissedTickBehavior::Delay` would make
        // its very next `.tick()` resolve immediately on the next poll — this
        // is exactly the "due tick queued behind the edge" shape #1313 is
        // about, and `Gate::next`'s `, if open` guard is what is supposed to
        // keep it from ever being polled while hidden.
        pump_ten_periods(period).await;
        assert_eq!(
            calls.sequence(),
            Vec::<&str>::new(),
            "the closed gate must not have sampled or reset at all yet",
        );

        // One yield does not bound the *blocking-pool* thread the way it
        // bounds the sampler task: under real contention a `spawn_blocking`
        // closure can occasionally still complete before this yield returns,
        // so the log may already hold `["reset", "tick"]` here rather than
        // just `["reset"]` (measured: 1/200 runs under `taskset -c 0-3` plus
        // four pinned burners). What must never happen is a tick recorded
        // *first* — that is the one thing checked here; the pair's own order
        // is re-checked, unconditionally, right below.
        cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");
        tokio::task::yield_now().await;
        assert_eq!(
            calls.sequence().first(),
            Some(&"reset"),
            "the first thing that may have happened by now must be the edge's \
             own reset — never a tick ahead of it: {:?}",
            calls.sequence(),
        );

        // A *prefix* check, not equality of the whole log: under real
        // contention `pump_until` can itself take long enough (real time) for
        // the cadence to legitimately fire again while it waits, so more than
        // one `"tick"` landing is fine and expected — measured, dropping this
        // to a prefix is what makes the shape (never a tick before its reset)
        // survive the same contention campaign the sibling test's fix does.
        // What must never appear is a `"tick"` ahead of the one `"reset"`.
        assert!(pump_until(|| calls.ticks() >= 1).await);
        let seq = calls.sequence();
        assert_eq!(
            seq.first(),
            Some(&"reset"),
            "the very first thing in the log must be the edge's own reset: {seq:?}",
        );
        assert_eq!(
            seq.get(1),
            Some(&"tick"),
            "…and the read that follows must come strictly after it: {seq:?}",
        );

        drop(cmd_tx);
        let _ = task.await;
    }

    /// **The bar instance's tick**: one open edge, then nothing on the lane
    /// ever again, and the sampler keeps reading on the cadence.
    ///
    /// This is the sampler-side half of "a bar chip samples on its own tick
    /// regardless of the sidebar gate" (#1251). The other half is in
    /// `crate::plugin`: a bar instance puts exactly one `SetVisible(true)` on
    /// its own lane at `init` and forwards no host visibility push at all, so
    /// this — an open gate with a silent lane — is precisely the state a bar
    /// instance runs in for the whole session.
    ///
    /// Note what it does **not** rely on: the host's constant
    /// `SlotVisibility { visible: true }` seed for bar mounts. That seed exists
    /// (`trollshell/src/plugins/session.rs`) and would open the gate too, but a
    /// bar chip that samples only because the host happened to send one frame
    /// is a chip one `try_send` away from being frozen.
    ///
    /// **Falsified** by making the gate answer the open edge and then re-close
    /// (`self.visible = false` after handing back `Wake::Refresh`): exactly one
    /// sample lands instead of one per period, which is the failure mode a bar
    /// chip would show as a frozen reading rather than as an error.
    #[tokio::test(start_paused = true)]
    async fn one_open_edge_then_silence_keeps_the_cadence_running() {
        let period = Duration::from_secs(1);
        let calls = Arc::new(Calls::default());
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, _msg_rx) = cmd_channel::<Msg>();
        let made = Arc::clone(&calls);
        let task = tokio::spawn(sampler_task_with(cmd_rx, msg_tx, period, move || {
            FakeSampler(Arc::clone(&made))
        }));

        // Exactly what `Stats::with_config` puts on the lane for a bar family,
        // and then nothing.
        cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");
        assert!(
            pump_until(|| calls.ticks() >= 5).await,
            "five periods of silence must still produce five samples, got {}",
            calls.ticks(),
        );
        assert_eq!(
            calls.resets(),
            0,
            "…and it never re-baselines: the LOW 4 re-baseline fires on an \
             unpark, and a chip that is always on screen never parks, so its \
             /proc/stat window stays continuous for the whole session",
        );

        drop(cmd_tx);
        let _ = task.await;
    }

    /// …and a closed command lane ends **every** task — the router, the
    /// sensors sampler and the walker — rather than leaving one polling on
    /// against a dropped reducer.
    ///
    /// Through [`spawn`], the production wrapper, with the real samplers: the
    /// lane closes before either gate has opened, so nothing reads `/proc`.
    ///
    /// **Falsified** by `route` holding on to its two senders after its own
    /// lane closes (e.g. looping on a `sleep` instead of returning): the two
    /// sampling tasks then never see their lanes close.
    #[tokio::test(start_paused = true)]
    async fn a_closed_lane_ends_every_task() {
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, _msg_rx) = cmd_channel::<Msg>();
        let tasks = spawn(cmd_rx, msg_tx, Duration::from_secs(1), Needs::default());
        drop(cmd_tx);
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("every task must return when the lane closes")
                .expect("and not by panicking");
        }
    }

    /// Wait for [`spawn_with`]'s three tasks to end after the test dropped the
    /// lane — **bounded**, so a teardown that stops working fails the test
    /// instead of hanging the whole binary (measured: a router that kept its
    /// senders alive left three tests running forever rather than red).
    async fn join_all(tasks: [tokio::task::JoinHandle<()>; 3]) {
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("every task ends once the lane closes")
                .expect("and not by panicking");
        }
    }

    /// A walker that reads nothing and counts what it was asked to do — the
    /// Top apps twin of [`FakeSampler`].
    struct FakeWalker(Arc<Calls>);

    impl Sample for FakeWalker {
        type Reading = TopApps;

        fn tick(&mut self) -> TopApps {
            self.0.record("walk");
            self.0.ticks.fetch_add(1, Ordering::SeqCst);
            TopApps {
                by_cpu: Vec::new(),
                by_mem: vec![ProcSample {
                    name: "firefox".to_owned(),
                    app_id: Some("firefox".to_owned()),
                    cpu_frac: 0.0,
                    mem_bytes: 1 << 30,
                    procs: 3,
                }],
            }
        }

        fn reset(&mut self) {
            self.0.record("reset");
            self.0.resets.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// **Each command goes to its own task, and only there**: the surface's
    /// visibility to the sensors sampler, the page's to the walker.
    ///
    /// **Falsified** by routing either variant to the other lane, or by
    /// fanning every command out to both (the walker lane then carries the
    /// bar's own `SetVisible(true)` seed).
    #[tokio::test]
    async fn the_router_sends_each_command_to_its_own_task() {
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (sensors_tx, mut sensors_rx) = cmd_channel::<Cmd>();
        let (walker_tx, mut walker_rx) = cmd_channel::<Cmd>();
        let router = tokio::spawn(route(cmd_rx, sensors_tx, walker_tx));
        for cmd in [
            Cmd::SetVisible(true),
            Cmd::PageVisible(true),
            Cmd::SetVisible(false),
            Cmd::PageVisible(false),
        ] {
            cmd_tx.send(cmd).expect("lane is live");
        }
        drop(cmd_tx);
        tokio::time::timeout(Duration::from_secs(5), router)
            .await
            .expect("the router ends when its lane closes")
            .expect("and not by panicking");

        let drain = |rx: &mut hytte_plugin::CmdReceiver<Cmd>| {
            std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>()
        };
        assert_eq!(
            drain(&mut sensors_rx),
            vec![Cmd::SetVisible(true), Cmd::SetVisible(false)],
        );
        assert_eq!(
            drain(&mut walker_rx),
            vec![Cmd::PageVisible(true), Cmd::PageVisible(false)],
        );
    }

    /// Each task's classifier answers **only its own switch** — the second
    /// line of defence behind the router. One direction keeps a bar
    /// instance's permanently-open surface from opening the walker; the other
    /// keeps a page close from parking the bar chips' sampler.
    ///
    /// **Falsified** by `Cmd::page` answering `SetVisible`, or
    /// `Cmd::surface` answering `PageVisible`.
    #[test]
    fn each_classifier_answers_only_its_own_switch() {
        for on in [false, true] {
            assert_eq!(Cmd::SetVisible(on).surface(), Some(on));
            assert_eq!(Cmd::SetVisible(on).page(), None);
            assert_eq!(Cmd::PageVisible(on).page(), Some(on));
            assert_eq!(Cmd::PageVisible(on).surface(), None);
        }
    }

    /// **The walker runs only while the page is open — never for the bar
    /// chips — and the chips keep sampling across a page close.** End to end
    /// through [`spawn_with`]'s router and both gates, with counting fakes.
    ///
    /// A bar instance opens its sensors gate at `init` and never closes it,
    /// so this drives exactly that — `SetVisible(true)` and nothing else — and
    /// shows the sensors sampler ticking away (the control: the pump did move
    /// time) while the walker is never called: a hidden page walks nothing.
    /// Opening the page then starts the walker and delivers its lists; closing
    /// it parks the walker again while the sensors sampler keeps its cadence;
    /// and a reopen after a long close drops the baseline first (#1277 LOW 4's
    /// rule, which the walker inherits by running through the same loop).
    ///
    /// **Falsified** by the router swapping its two lanes (the walker never
    /// hears its switch), by the walker's classifier ignoring
    /// `PageVisible(false)` (it keeps walking after the close), by
    /// `Cmd::surface` answering `PageVisible` together with the router
    /// fanning out (the page close parks the chips), and by `drive`'s `parked`
    /// re-baseline being deleted. **Not** by letting `SetVisible` through at
    /// only one of its two stops — the router, or the walker's classifier —
    /// since the other still holds it back (measured: both stay green here);
    /// those are what `the_router_sends_each_command_to_its_own_task` and
    /// `each_classifier_answers_only_its_own_switch` are for.
    #[tokio::test(start_paused = true)]
    async fn the_walker_runs_only_while_the_page_is_open_never_for_the_chips() {
        let sensors = Arc::new(Calls::default());
        let walker = Arc::new(Calls::default());
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, mut msg_rx) = cmd_channel::<Msg>();
        let (made_s, made_w) = (Arc::clone(&sensors), Arc::clone(&walker));
        let tasks = spawn_with(
            cmd_rx,
            msg_tx,
            Duration::from_secs(1),
            move || FakeSampler(Arc::clone(&made_s)),
            move || FakeWalker(Arc::clone(&made_w)),
        );

        // What a bar instance's `init` puts on the lane, and then nothing.
        cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");
        assert!(
            pump_until(|| sensors.ticks() >= 5).await,
            "the chips' own sampler runs — the control that time moved",
        );
        pump_ten_periods(crate::top_apps::POLL).await;
        assert_eq!(
            walker.ticks(),
            0,
            "an open surface is not an open page: the walker never ran",
        );

        // The page opens: the walker walks, and its lists reach the reducer.
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        let mut landed = None;
        assert!(
            pump_until(|| {
                while let Ok(msg) = msg_rx.try_recv() {
                    if let Msg::TopApps(apps) = msg {
                        landed = Some(apps);
                    }
                }
                landed.is_some()
            })
            .await,
            "an open page walks, and the walk reaches the reducer",
        );
        assert_eq!(landed.expect("landed").by_mem[0].procs, 3);

        // The page closes: the walker parks. One walk may already have been
        // in flight on the blocking pool — the gate never cancels one — so
        // the bound is one more, not none; ten periods of an open gate would
        // be ten.
        cmd_tx.send(Cmd::PageVisible(false)).expect("lane is live");
        tokio::task::yield_now().await;
        let parked_at = walker.ticks();
        let chips_at = sensors.ticks();
        pump_ten_periods(crate::top_apps::POLL).await;
        assert!(
            walker.ticks() <= parked_at + 1,
            "a closed page must park the walker: {} walks after the close",
            walker.ticks() - parked_at,
        );
        // …while the chips, which are still on screen, keep their own
        // cadence. Pumped until rather than counted over the ten periods
        // above: a blocking-pool read lags virtual time under load, so a
        // count there would measure the machine, not the gate.
        assert!(
            pump_until(|| sensors.ticks() >= chips_at + 3).await,
            "a page close must not park the chips' sampler: {} samples since",
            sensors.ticks() - chips_at,
        );

        // Reopening re-baselines before it walks.
        assert_eq!(walker.resets(), 0);
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        assert!(pump_until(|| walker.resets() >= 1).await);
        assert_eq!(walker.resets(), 1, "exactly one re-baseline per reopen");
        assert_eq!(
            sensors.resets(),
            0,
            "…and the chips' sampler, never parked, never re-baselined",
        );

        drop(cmd_tx);
        join_all(tasks).await;
    }

    /// **The walker keeps its baseline across the task's ticks.** `drive`
    /// moves the walker into a `spawn_blocking` and back out every tick; this
    /// runs a real [`Walker`] over a `/proc` stand-in whose shares are computed
    /// from the baseline it is handed, and checks the CPU list the reducer
    /// receives is a real share from the second walk on.
    ///
    /// **Falsified** by `drive` keeping a fresh sampler each tick
    /// (`sampler = make()` in place of `sampler = back`): every walk is then
    /// cold and every CPU list is withheld. The walker's own threading is
    /// pinned by `crate::top_apps`' tests; this is the task's half.
    #[allow(clippy::float_cmp)]
    #[tokio::test(start_paused = true)]
    async fn the_walker_keeps_its_baseline_across_the_tasks_ticks() {
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, mut msg_rx) = cmd_channel::<Msg>();
        let sensors = Arc::new(Calls::default());
        let made = Arc::clone(&sensors);
        let tasks = spawn_with(
            cmd_rx,
            msg_tx,
            Duration::from_secs(1),
            move || FakeSampler(Arc::clone(&made)),
            || Walker::over(crate::top_apps::fake_proc().0),
        );

        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        let mut walks: Vec<TopApps> = Vec::new();
        assert!(
            pump_until(|| {
                while let Ok(msg) = msg_rx.try_recv() {
                    if let Msg::TopApps(apps) = msg {
                        walks.push(apps);
                    }
                }
                walks.len() >= 3
            })
            .await,
            "three walks reach the reducer",
        );
        assert!(walks[0].by_cpu.is_empty(), "the first walk is cold");
        for walk in &walks[1..3] {
            assert_eq!(walk.by_cpu.len(), 1, "{walks:?}");
            assert_eq!(walk.by_cpu[0].cpu_frac, 0.25, "{walks:?}");
        }
        assert_eq!(sensors.ticks(), 0, "the sensors gate was never opened");

        drop(cmd_tx);
        join_all(tasks).await;
    }

    /// **The real walker re-baselines on a reopen after a long close, through
    /// the shared loop.** A real [`Walker`]'s page is closed for ten walks'
    /// worth of time and reopened; the first walk after the reopen must be
    /// cold (no CPU list) and handed no baseline, so its CPU share is never
    /// the mean over the closed gap (#1277 LOW 4). From the #1426 review, LOW
    /// 2: `Walker::reset` was tested directly and `drive`'s reset only with a
    /// fake, so the one line joining them was not.
    ///
    /// **Falsified** by making `impl Sample for Walker`'s `reset` a no-op, and
    /// by `drive` keeping every baseline (`keep` read as unbounded).
    #[tokio::test(start_paused = true)]
    async fn a_page_reopened_after_a_long_close_measures_cpu_over_a_fresh_window() {
        async fn next_walk(rx: &mut hytte_plugin::CmdReceiver<Msg>) -> TopApps {
            let mut got = None;
            pump_until(|| {
                while got.is_none() {
                    match rx.try_recv() {
                        Ok(Msg::TopApps(apps)) => got = Some(apps),
                        Ok(Msg::Sampled(_)) => {}
                        Err(_) => break,
                    }
                }
                got.is_some()
            })
            .await;
            got.expect("a walk lands")
        }

        // The gap below is ten walks; it must be past the young-baseline
        // window, or this would be the quick-reopen test.
        assert!(crate::top_apps::POLL * 10 > crate::top_apps::KEEP_BASELINE);

        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, mut msg_rx) = cmd_channel::<Msg>();
        let (walk, handed) = crate::top_apps::fake_proc();
        let mut walk = Some(walk);
        let tasks = spawn_with(
            cmd_rx,
            msg_tx,
            Duration::from_secs(1),
            || FakeSampler(Arc::new(Calls::default())),
            move || Walker::over(walk.take().expect("the walker is built once")),
        );

        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        assert!(
            next_walk(&mut msg_rx).await.by_cpu.is_empty(),
            "cold first walk"
        );
        assert_eq!(
            next_walk(&mut msg_rx).await.by_cpu.len(),
            1,
            "warm second walk"
        );

        cmd_tx.send(Cmd::PageVisible(false)).expect("lane is live");
        // Ten walks' worth of closed page by the runtime's own auto-advance,
        // which tokio holds while a `spawn_blocking` walk is pending: this
        // returns only once a walk in flight at the close has landed on the
        // lane, so the drain below takes it. The #1426 version stepped the
        // clock by hand (`pump_ten_periods`), and a walk starved on a loaded
        // core landed *after* the drain, where `next_walk` read it as the
        // reopen's — a warm share, and a red test (measured on #1427, release
        // build pinned to one core beside four burners: 29 of 30 runs on
        // `main`, 27 of 30 on this branch before the fix).
        tokio::time::sleep(crate::top_apps::POLL * 10).await;
        while msg_rx.try_recv().is_ok() {}
        let before = handed.lock().expect("not poisoned").len();

        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        let reopened = next_walk(&mut msg_rx).await;
        assert!(
            reopened.by_cpu.is_empty(),
            "the reopen's walk must be cold, not a share over the gap: {reopened:?}",
        );
        assert_eq!(
            handed.lock().expect("not poisoned")[before],
            (None, 0),
            "the reopen's walk must be handed no baseline",
        );

        drop(cmd_tx);
        join_all(tasks).await;
    }

    /// Turn the runtime over until `done` answers true, **without moving
    /// virtual time**, or give up after a wall-clock budget.
    ///
    /// [`pump_until`] nudges the clock forward while it waits, and a walk on
    /// the blocking pool can take long enough in wall-clock terms for that to
    /// add up to many virtual seconds. That is harmless where a test waits for
    /// "a walk, whenever" and fatal where it measures a baseline's *age*: the
    /// young-baseline tests below advance the clock themselves, exactly, and
    /// wait with this instead.
    async fn settle_until(mut done: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if done() {
                return true;
            }
            tokio::task::yield_now().await;
        }
        done()
    }

    /// Turn the runtime over a fixed number of times without moving virtual
    /// time — the negative half of [`settle_until`], where there is nothing
    /// to wait *for*. The same order of budget [`pump_ten_periods`] gives each
    /// period, ten times over.
    async fn quiesce() {
        for _ in 0..2_000 {
            tokio::task::yield_now().await;
        }
    }

    /// Wait (without moving virtual time) until the reducer's lane has
    /// carried `n` walks in all, collecting them into `walks`, and assert it
    /// is exactly `n` — no walk the test did not ask for.
    async fn walks_landed(
        rx: &mut hytte_plugin::CmdReceiver<Msg>,
        walks: &mut Vec<TopApps>,
        n: usize,
    ) {
        let landed = settle_until(|| {
            while let Ok(msg) = rx.try_recv() {
                if let Msg::TopApps(apps) = msg {
                    walks.push(apps);
                }
            }
            walks.len() >= n
        })
        .await;
        assert!(landed, "walk {n} lands");
        assert_eq!(walks.len(), n, "exactly {n} walks: {walks:?}");
    }

    /// **A quick close and reopen keeps a young baseline** (the #1426
    /// review's NIT 8, now that a page close parks the walker): a page
    /// reopened less than `KEEP_BASELINE` after the last walk started gets a
    /// **warm** first walk — handed the previous walk's baseline, so its CPU
    /// list is a real share rather than `—` for a whole cadence. At exactly
    /// `KEEP_BASELINE` the baseline is old enough to drop, and the reopen's
    /// walk is cold again. (How soon the warm walk may start is
    /// [`a_kept_baseline_is_read_no_sooner_than_half_a_walk_after_it_was_taken`]'s
    /// subject; here it is simply waited for.)
    ///
    /// Virtual time moves only where this test moves it (see
    /// [`settle_until`]), so each baseline's age is exact.
    ///
    /// **Falsified** by `drive` re-baselining on every reopen (`keep`
    /// ignored: the quick reopen's walk is handed no baseline), by keeping
    /// every baseline (the `KEEP_BASELINE` reopen is handed one), by `<=` for
    /// `<` in the age check (the boundary reopen keeps it), and by stamping
    /// `last_read` only once (the just-under-the-window reopen then measures
    /// from the first walk and drops it).
    #[allow(clippy::float_cmp)]
    #[tokio::test(start_paused = true)]
    async fn a_quick_reopen_keeps_a_young_baseline_and_a_slow_one_drops_it() {
        use crate::top_apps::{KEEP_BASELINE, MIN_BASELINE_AGE, POLL};

        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, mut msg_rx) = cmd_channel::<Msg>();
        let (walk, handed) = crate::top_apps::fake_proc();
        let mut walk = Some(walk);
        let tasks = spawn_with(
            cmd_rx,
            msg_tx,
            Duration::from_secs(1),
            || FakeSampler(Arc::new(Calls::default())),
            move || Walker::over(walk.take().expect("the walker is built once")),
        );
        let handed_at = |i: usize| handed.lock().expect("not poisoned")[i];
        let mut walks: Vec<TopApps> = Vec::new();

        // Open: the edge walks at once (cold), and one cadence later again.
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        walks_landed(&mut msg_rx, &mut walks, 1).await;
        assert!(walks[0].by_cpu.is_empty(), "the first walk is cold");
        tokio::time::advance(POLL).await;
        walks_landed(&mut msg_rx, &mut walks, 2).await;
        assert_eq!(walks[1].by_cpu.len(), 1, "the second walk is warm");

        // A close and an immediate reopen: the baseline is 0 s old, so the
        // walk is held until it is MIN_BASELINE_AGE old.
        cmd_tx.send(Cmd::PageVisible(false)).expect("lane is live");
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        tokio::time::advance(MIN_BASELINE_AGE).await;
        walks_landed(&mut msg_rx, &mut walks, 3).await;
        assert_eq!(
            handed_at(2),
            (Some(500), 2_000),
            "the quick reopen's walk is handed the last walk's baseline",
        );
        assert_eq!(walks[2].by_cpu.len(), 1, "…so its CPU list is there");
        assert_eq!(walks[2].by_cpu[0].cpu_frac, 0.25, "…and a real share");

        // Closed for just under the window: still kept.
        cmd_tx.send(Cmd::PageVisible(false)).expect("lane is live");
        tokio::task::yield_now().await;
        tokio::time::advance(KEEP_BASELINE.saturating_sub(Duration::from_millis(1))).await;
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        walks_landed(&mut msg_rx, &mut walks, 4).await;
        assert_eq!(
            handed_at(3),
            (Some(750), 3_000),
            "a baseline younger than KEEP_BASELINE survives the reopen",
        );

        // Closed for exactly the window: dropped, and the walk is cold.
        cmd_tx.send(Cmd::PageVisible(false)).expect("lane is live");
        tokio::task::yield_now().await;
        tokio::time::advance(KEEP_BASELINE).await;
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        walks_landed(&mut msg_rx, &mut walks, 5).await;
        assert_eq!(
            handed_at(4),
            (None, 0),
            "a KEEP_BASELINE-old baseline is dropped"
        );
        assert!(walks[4].by_cpu.is_empty(), "…so the walk is cold again");

        drop(cmd_tx);
        join_all(tasks).await;
    }

    /// Everything the reducer's lane carries by now, added to `walks`.
    fn drain_walks(rx: &mut hytte_plugin::CmdReceiver<Msg>, walks: &mut Vec<TopApps>) {
        while let Ok(msg) = rx.try_recv() {
            if let Msg::TopApps(apps) = msg {
                walks.push(apps);
            }
        }
    }

    /// **A kept baseline is read against no sooner than `MIN_BASELINE_AGE`
    /// after its read started** (the #1437 review's LOW 1): a quick reopen's
    /// first walk is held until `max(reopen, last walk's start +
    /// MIN_BASELINE_AGE)`, so its CPU list is never a share over a window of a
    /// few hundred milliseconds — and it still comes well inside a cadence.
    /// Then the two things the hold must not break:
    ///
    /// - **The cadence restarts from the held walk**, a full `POLL` after it,
    ///   as it does from an open edge.
    /// - **A close during the hold cancels the walk**: no walk with the page
    ///   shut, and a later reopen is decided afresh.
    ///
    /// The negatives wait with `tokio::time::sleep`, not `advance`: the paused
    /// runtime will not move its clock while a walk is on the blocking pool,
    /// so by the time a sleep returns, any walk started before its deadline
    /// has landed. A walk that should not have started cannot hide.
    ///
    /// **Falsified** by not holding the walk (it lands at the reopen), by a
    /// shorter hold (`settle / 2`: it lands before the floor), by dropping the
    /// `gate.reset()` after a held walk (the next walk comes 700 ms early), and
    /// by dropping the `gate.is_visible()` check (the held walk runs with the
    /// page shut).
    #[allow(clippy::float_cmp)]
    #[tokio::test(start_paused = true)]
    async fn a_kept_baseline_is_read_no_sooner_than_half_a_walk_after_it_was_taken() {
        use crate::top_apps::{MIN_BASELINE_AGE, POLL};
        // How long after walk 2 started the page closes and reopens.
        const EARLY: Duration = Duration::from_millis(300);
        const MS: Duration = Duration::from_millis(1);

        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, mut msg_rx) = cmd_channel::<Msg>();
        let (walk, handed) = crate::top_apps::fake_proc();
        let mut walk = Some(walk);
        let tasks = spawn_with(
            cmd_rx,
            msg_tx,
            Duration::from_secs(1),
            || FakeSampler(Arc::new(Calls::default())),
            move || Walker::over(walk.take().expect("the walker is built once")),
        );
        let handed_at = |i: usize| handed.lock().expect("not poisoned")[i];
        let started = || handed.lock().expect("not poisoned").len();
        let mut walks: Vec<TopApps> = Vec::new();

        // Walk 1 on the open edge (0 s, cold), walk 2 a cadence later (2 s).
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        walks_landed(&mut msg_rx, &mut walks, 1).await;
        tokio::time::advance(POLL).await;
        walks_landed(&mut msg_rx, &mut walks, 2).await;

        // 300 ms into walk 2's window the page closes and reopens at once.
        tokio::time::advance(EARLY).await;
        cmd_tx.send(Cmd::PageVisible(false)).expect("lane is live");
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");

        // Up to a millisecond short of the floor: nothing starts.
        tokio::time::sleep(MIN_BASELINE_AGE.saturating_sub(EARLY + MS)).await;
        drain_walks(&mut msg_rx, &mut walks);
        assert_eq!(
            (walks.len(), started()),
            (2, 2),
            "no walk before the kept baseline is MIN_BASELINE_AGE old",
        );

        // At the floor: the held walk, warm, over a window of exactly it.
        tokio::time::sleep(MS).await;
        walks_landed(&mut msg_rx, &mut walks, 3).await;
        assert_eq!(handed_at(2), (Some(500), 2_000), "handed walk 2's baseline");
        assert_eq!(walks[2].by_cpu.len(), 1, "its CPU list is there");
        assert_eq!(walks[2].by_cpu[0].cpu_frac, 0.25, "…a real share");

        // The cadence restarts from the held walk: nothing a millisecond
        // short of a full POLL after it, then the next walk.
        tokio::time::sleep(POLL.saturating_sub(MS)).await;
        drain_walks(&mut msg_rx, &mut walks);
        assert_eq!(
            (walks.len(), started()),
            (3, 3),
            "the next walk is a full POLL after the held one",
        );
        tokio::time::sleep(MS).await;
        walks_landed(&mut msg_rx, &mut walks, 4).await;

        // A close and a reopen hold a walk again; a second close during the
        // hold cancels it, and nothing walks with the page shut.
        cmd_tx.send(Cmd::PageVisible(false)).expect("lane is live");
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        cmd_tx.send(Cmd::PageVisible(false)).expect("lane is live");
        tokio::time::sleep(POLL * 5).await;
        drain_walks(&mut msg_rx, &mut walks);
        assert_eq!(
            (walks.len(), started()),
            (4, 4),
            "a close during the hold cancels the held walk",
        );

        // Ten seconds later the baseline is too old to keep: the reopen walks
        // at once, cold.
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        walks_landed(&mut msg_rx, &mut walks, 5).await;
        assert_eq!(handed_at(4), (None, 0), "a stale baseline is dropped");

        drop(cmd_tx);
        join_all(tasks).await;
    }

    /// **A quick reopen lowers the reopen flag even though it keeps the
    /// baseline**, so a later cadence tick is never taken for a reopen,
    /// however late it comes (`drive`'s "`swap` first, always").
    ///
    /// **Falsified** by swapping `parked` only when the baseline is dropped:
    /// the late tick then re-baselines and walks cold.
    #[tokio::test(start_paused = true)]
    async fn a_late_tick_after_a_quick_reopen_is_not_a_reopen() {
        use crate::top_apps::{KEEP_BASELINE, MIN_BASELINE_AGE, POLL};

        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, mut msg_rx) = cmd_channel::<Msg>();
        let (walk, handed) = crate::top_apps::fake_proc();
        let mut walk = Some(walk);
        let tasks = spawn_with(
            cmd_rx,
            msg_tx,
            Duration::from_secs(1),
            || FakeSampler(Arc::new(Calls::default())),
            move || Walker::over(walk.take().expect("the walker is built once")),
        );
        let mut walks: Vec<TopApps> = Vec::new();

        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        walks_landed(&mut msg_rx, &mut walks, 1).await;
        // A close and an immediate reopen: young, so the baseline is kept.
        // (Its walk is held until the baseline is MIN_BASELINE_AGE old.)
        cmd_tx.send(Cmd::PageVisible(false)).expect("lane is live");
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        tokio::time::advance(MIN_BASELINE_AGE).await;
        walks_landed(&mut msg_rx, &mut walks, 2).await;
        assert_eq!(walks[1].by_cpu.len(), 1, "the quick reopen's walk is warm");

        // The page stays open and the next cadence tick comes late (a stalled
        // runtime, say), past KEEP_BASELINE. It is a tick, not a reopen.
        tokio::time::advance(KEEP_BASELINE + POLL).await;
        walks_landed(&mut msg_rx, &mut walks, 3).await;
        assert_eq!(
            handed.lock().expect("not poisoned")[2],
            (Some(500), 2_000),
            "a cadence tick is handed the previous walk's baseline, however late",
        );
        assert_eq!(walks[2].by_cpu.len(), 1, "…so its CPU list is there");

        drop(cmd_tx);
        join_all(tasks).await;
    }

    /// **The page's register seed opens the walker whichever side of the
    /// first tick it lands on**, and a `false` seed opens nothing.
    ///
    /// In a live session a bar instance's `init` puts `SetVisible(true)` on
    /// the lane before `sources` spawns the tasks, and the host's
    /// `PageVisibility` seed arrives only after, through the session loop — so
    /// the chips' sampler may already have ticked any number of times, and
    /// the walker's own construction-time cadence tick may be long overdue.
    /// Three launches:
    ///
    /// - **Before**: the page seed is queued with the chips' seed, before any
    ///   task has run. The walker walks at once.
    /// - **After**: the chips have sampled for ten seconds first. The walker
    ///   still walks exactly once on the seed and then waits a full cadence —
    ///   an overdue construction-time tick is not a second walk.
    /// - **Shut**: the seed says the page is closed. The chips sample; the
    ///   walker never does, however long it waits.
    ///
    /// In both open launches the next walk is one whole cadence after the
    /// seed's, and then it does come.
    ///
    /// **Falsified** by the walker's gate starting open (the shut launch
    /// walks), by the router or the walker's classifier dropping
    /// `PageVisible` (no launch walks), and by the gate's open-edge `reset`
    /// being skipped (the late seed walks twice back to back).
    #[tokio::test(start_paused = true)]
    async fn the_page_seed_opens_the_walker_before_or_after_the_first_tick() {
        /// What one launch saw: the walker's count once the seed settled, a
        /// hair under one cadence later, and just after it; and the chips'.
        #[derive(Debug, PartialEq, Eq)]
        struct Seen {
            settled: usize,
            early: usize,
            cadence: usize,
            chips: usize,
        }

        /// One launch: the bar's own seed, then (after `chips_first` of
        /// virtual time) the host's page seed.
        async fn launch(chips_first: Duration, seed: bool) -> Seen {
            let sensors = Arc::new(Calls::default());
            let walker = Arc::new(Calls::default());
            let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
            let (msg_tx, _msg_rx) = cmd_channel::<Msg>();
            let (made_s, made_w) = (Arc::clone(&sensors), Arc::clone(&walker));
            let tasks = spawn_with(
                cmd_rx,
                msg_tx,
                Duration::from_secs(1),
                move || FakeSampler(Arc::clone(&made_s)),
                move || FakeWalker(Arc::clone(&made_w)),
            );
            cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");
            if !chips_first.is_zero() {
                assert!(
                    settle_until(|| sensors.ticks() >= 1).await,
                    "the chips' open edge samples"
                );
                tokio::time::advance(chips_first).await;
                assert!(
                    settle_until(|| sensors.ticks() >= 2).await,
                    "…and the cadence runs before the page seed"
                );
            }
            cmd_tx.send(Cmd::PageVisible(seed)).expect("lane is live");
            // Wait for what should come, then give what should not a fair
            // chance to come too — all without moving the clock.
            let want = usize::from(seed);
            let _ = settle_until(|| walker.ticks() >= want).await;
            quiesce().await;
            let settled = walker.ticks();
            tokio::time::advance(crate::top_apps::POLL.saturating_sub(Duration::from_millis(1)))
                .await;
            quiesce().await;
            let early = walker.ticks();
            tokio::time::advance(Duration::from_millis(1)).await;
            if seed {
                let _ = settle_until(|| walker.ticks() > early).await;
            } else {
                quiesce().await;
            }
            let cadence = walker.ticks();
            let chips = sensors.ticks();
            drop(cmd_tx);
            join_all(tasks).await;
            Seen {
                settled,
                early,
                cadence,
                chips,
            }
        }

        let before = launch(Duration::ZERO, true).await;
        assert_eq!(
            (before.settled, before.early, before.cadence),
            (1, 1, 2),
            "seed before the first tick: {before:?}",
        );

        let after = launch(Duration::from_secs(10), true).await;
        assert!(after.chips >= 2, "the chips ticked first: {after:?}");
        assert_eq!(
            (after.settled, after.early, after.cadence),
            (1, 1, 2),
            "seed after the first ticks: {after:?}",
        );

        let shut = launch(Duration::from_secs(10), false).await;
        assert!(shut.chips >= 2, "the chips ticked: {shut:?}");
        assert_eq!(
            (shut.settled, shut.early, shut.cadence),
            (0, 0, 0),
            "a shut page walks nothing: {shut:?}",
        );
    }

    /// **The walker keeps native's 2 s cadence** even when the sensors
    /// sampler runs faster: 9.5 s of an open page is **exactly five walks** —
    /// the open edge at 0 s, then 2, 4, 6 and 8 s — never one a second. From
    /// the #1426 review, LOW 4 — a faster walker silently multiplies the cost
    /// of an open page.
    ///
    /// # Why the count is exact
    ///
    /// The test does not move the clock itself. It sleeps, and the paused
    /// runtime **auto-advances** to the next timer only once every task is
    /// idle — and tokio holds that auto-advance for as long as any
    /// `spawn_blocking` task is still pending on a current-thread runtime
    /// (`BlockingSchedule::new` → `inhibit_auto_advance`, released when the
    /// task ends). So virtual time never passes a walk that is still on the
    /// blocking pool, however slow the machine is.
    ///
    /// The #1426 version stepped the clock by hand (`pump_ten_periods`: an
    /// `advance` plus 200 `yield_now`s per second) and never waited in real
    /// time for the blocking pool. On a loaded machine the pool could miss
    /// every step, so it asserted only `1 ≤ walks ≤ 6`, and even the lower
    /// bound flaked: zero walks in CI on #1435 (run 36476735746). With the
    /// clock held by the runtime, neither bound needs slack.
    ///
    /// **Falsified** by handing the walker the sensors' `period` instead of
    /// `top_apps::POLL` in `spawn_with` (ten walks), or any other cadence.
    #[tokio::test(start_paused = true)]
    async fn the_walker_walks_on_its_own_two_second_cadence() {
        let walker = Arc::new(Calls::default());
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, _msg_rx) = cmd_channel::<Msg>();
        let made = Arc::clone(&walker);
        let tasks = spawn_with(
            cmd_rx,
            msg_tx,
            Duration::from_secs(1),
            || FakeSampler(Arc::new(Calls::default())),
            move || FakeWalker(Arc::clone(&made)),
        );
        cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        // Half a second short of the walk due at 10 s, so no walk is due at
        // the instant this wakes: the count cannot depend on which of two
        // simultaneous timers the runtime polls first.
        tokio::time::sleep(Duration::from_millis(9_500)).await;
        let walks = walker.ticks();
        assert_eq!(
            walks, 5,
            "9.5 s of an open page at a 2 s cadence is the edge plus four walks",
        );
        drop(cmd_tx);
        join_all(tasks).await;
    }

    /// **A slow walk never stalls the chips.** The walk runs off the
    /// session's one thread (`spawn_blocking`), so the sensors sampler keeps
    /// its cadence *while a walk is still in progress*. Walked inline, a
    /// 12–50 ms `/proc` walk would hold the current-thread runtime, and the
    /// chips and the socket loop with it. From the #1426 review, LOW 3.
    ///
    /// **Falsified** by dropping `drive`'s `spawn_blocking` and calling
    /// `sampler.tick()` inline.
    #[tokio::test(start_paused = true)]
    async fn a_slow_walk_never_stalls_the_chips() {
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc;

        struct SlowWalker {
            walking: Arc<AtomicBool>,
            release: mpsc::Receiver<()>,
        }
        impl Sample for SlowWalker {
            type Reading = TopApps;
            fn tick(&mut self) -> TopApps {
                self.walking.store(true, Ordering::SeqCst);
                let _ = self.release.recv_timeout(Duration::from_secs(5));
                self.walking.store(false, Ordering::SeqCst);
                TopApps::default()
            }
            fn reset(&mut self) {}
        }

        let sensors = Arc::new(Calls::default());
        let walking = Arc::new(AtomicBool::new(false));
        let (release_tx, release_rx) = mpsc::channel();
        let mut release_rx = Some(release_rx);
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, _msg_rx) = cmd_channel::<Msg>();
        let (made_s, made_w) = (Arc::clone(&sensors), Arc::clone(&walking));
        let tasks = spawn_with(
            cmd_rx,
            msg_tx,
            Duration::from_secs(1),
            move || FakeSampler(Arc::clone(&made_s)),
            move || SlowWalker {
                walking: Arc::clone(&made_w),
                release: release_rx.take().expect("the walker is built once"),
            },
        );

        cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");
        cmd_tx.send(Cmd::PageVisible(true)).expect("lane is live");
        assert!(
            pump_until(|| walking.load(Ordering::SeqCst)).await,
            "a walk must be observable in progress from the session's thread",
        );
        let before = sensors.ticks();
        assert!(
            pump_until(|| sensors.ticks() >= before + 3).await,
            "the chips keep sampling while a walk is in flight",
        );
        assert!(
            walking.load(Ordering::SeqCst),
            "…and the walk is still running"
        );

        release_tx.send(()).expect("the walker is waiting");
        drop(cmd_tx);
        join_all(tasks).await;
    }
}
