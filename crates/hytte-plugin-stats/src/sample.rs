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

/// The command lane: the host's slot-visibility push, forwarded by the reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// The mount surface became visible / hidden (#288).
    SetVisible(bool),
}

/// The message lane: one sample per tick, back to the reducer.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// A fresh sample landed.
    Sampled(Box<Snapshot>),
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
/// the GPU cache remembers whether `nvidia-smi` exists at all.
#[derive(Debug, Default)]
pub struct Sampler {
    /// What this instance draws, and therefore what it reads.
    needs: Needs,
    /// Previous `/proc/stat` (busy, total) per core.
    prev_cpu: Vec<(u64, u64)>,
    /// The resolved `/sys/class/hwmon` chip directory, once found.
    hwmon: Option<PathBuf>,
    /// `nvidia-smi` availability and the Intel RC6 delta base.
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
            let (gpu, cache) = hytte_sensors::read_gpu_with_cache(self.gpu);
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
        let cpu_clock_hz = if self.needs.cpu {
            let freq = hytte_sensors::read_cpu_freq();
            // The native row's own hide rule: no `cpufreq` governor at all
            // (VMs, some ARM boards) is `max_ceiling_hz == 0.0`, and the row
            // disappears rather than showing a flat, meaningless `0 Hz`.
            (freq.max_ceiling_hz > 0.0).then(|| finite_or_zero(freq.max_hz))
        } else {
            None
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
    /// The hwmon path and the GPU cache are deliberately **kept**: the chip
    /// directory is a `read_dir` walk whose answer does not go stale, and
    /// `GpuCache` is mostly "does `nvidia-smi` exist", which is a `fork`/`exec`
    /// to re-learn on every sidebar open. Its `intel_rc6_prev` half *is* a
    /// cumulative base with the same staleness, but its fields are private to
    /// `hytte-sensors` and there is no seam to clear one without the other —
    /// so on an Intel box the GPU needle, not the CPU row, wears one stale
    /// frame per open. Worth a seam if it ever shows on glass.
    pub fn reset(&mut self) {
        self.prev_cpu.clear();
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

/// What [`sampler_task`] needs of the thing it drives: read the machine, and
/// forget the cumulative baselines.
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
/// `Send + 'static` because the implementor is moved into and back out of a
/// `spawn_blocking`.
pub trait Sample: Send + 'static {
    /// One reading of the machine. **Blocking.**
    fn tick(&mut self) -> Snapshot;
    /// Drop the cumulative baselines — see [`Sampler::reset`].
    fn reset(&mut self);
}

impl Sample for Sampler {
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

/// The sampler task: park while the card is off screen, sample on the cadence
/// while it is on, and post each sample to the reducer.
///
/// Drives [`Gate`] directly rather than calling
/// [`poll::gated`](hytte_plugin::poll::gated) because the [`Sampler`]'s caches
/// have to live **across** the awaits: `gated` takes a `FnMut() -> impl Future`
/// whose future cannot borrow the closure's captures, so the state would have
/// to be wrapped in an `Arc<Mutex<…>>` for no gain. The gate's own contract —
/// park on hidden, refresh on the hidden→visible **edge**, `MissedTickBehavior::Delay`,
/// a close beating a due tick, no cancellation of work in flight — is unchanged
/// and is what makes a closed sidebar genuinely free.
pub async fn sampler_task(
    cmds: CmdReceiver<Cmd>,
    msgs: CmdSender<Msg>,
    period: Duration,
    needs: Needs,
) {
    sampler_task_with(cmds, msgs, period, move || Sampler::new(needs)).await;
}

/// [`sampler_task`] over an arbitrary [`Sample`] — the seam the gate tests
/// drive, so none of them touches the host's `/proc`.
///
/// `make` is a **factory**, not a value, because the sampler is moved into the
/// `spawn_blocking` and is therefore gone if that task panics or is cancelled:
/// recovering from that without ending the whole task (see the `Err` arm) needs
/// a way to build a fresh one.
async fn sampler_task_with<S: Sample>(
    cmds: CmdReceiver<Cmd>,
    msgs: CmdSender<Msg>,
    period: Duration,
    mut make: impl FnMut() -> S + Send + 'static,
) {
    // The gate absorbs every `SetVisible` itself and answers only the *open*
    // edge, so the loop below cannot otherwise see a close — and a close is
    // exactly what invalidates the `/proc/stat` baseline (#1277 LOW 4). The
    // classifier is the one place that sees every visibility command, so it
    // raises the flag and the next refresh lowers it. An `AtomicBool` rather
    // than a `Cell` because the task is `tokio::spawn`ed and must be `Send`.
    let parked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen_close = std::sync::Arc::clone(&parked);
    let mut gate = Gate::new(cmds, period, move |cmd: &Cmd| match cmd {
        Cmd::SetVisible(visible) => {
            if !*visible {
                seen_close.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Some(*visible)
        }
    });
    let mut sampler = make();
    while let Some(wake) = gate.next().await {
        match wake {
            Wake::Refresh => {
                if parked.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    // Re-baseline: the first frame after an unpark must be a
                    // load measured over a fresh window, not the mean over
                    // however long the sidebar was shut.
                    sampler.reset();
                }
                // The sampler is moved into the blocking closure and handed
                // back with the snapshot, which is what keeps its caches warm
                // without an `Arc<Mutex<…>>` around a value only this task ever
                // touches.
                let joined = tokio::task::spawn_blocking(move || {
                    let snapshot = sampler.tick();
                    (sampler, snapshot)
                })
                .await;
                match joined {
                    Ok((back, snapshot)) => {
                        sampler = back;
                        if msgs.send(Msg::Sampled(Box::new(snapshot))).is_err() {
                            // The reducer is gone: the session is tearing down.
                            return;
                        }
                    }
                    Err(e) => {
                        // The blocking task was cancelled or panicked. Both are
                        // our bug rather than a machine state, so say so — and
                        // then carry on with cold caches rather than ending the
                        // task, because ending it freezes the card for the rest
                        // of the session while leaving the process looking
                        // healthy. One tick of stale CPU deltas is the cost.
                        tracing::warn!(error = %e, "stats sampler tick failed; restarting its caches");
                        sampler = make();
                    }
                }
            }
            // Unreachable: the classifier above answers `Some` for the lane's
            // only variant, so the gate absorbs the whole lane. Spelled as an
            // irrefutable pattern there on purpose — a second `Cmd` variant
            // becomes a compile error here rather than a misclassified command.
            Wake::Cmd(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cmd, Msg, Needs, Sample, Sampler, Snapshot, as_unit, cpu_half, sampler_task,
        sampler_task_with,
    };
    use crate::config::Card;
    use hytte_plugin::cmd_channel;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// What a [`Sample`] did, shared with the test that installed it.
    ///
    /// The counters are what make the gate observable: "nothing arrived on the
    /// message lane" and "the sampler was never called" are different claims,
    /// and only the second one is about the gate (#1277 MEDIUM 2).
    #[derive(Debug, Default)]
    struct Calls {
        ticks: AtomicUsize,
        resets: AtomicUsize,
    }

    impl Calls {
        fn ticks(&self) -> usize {
            self.ticks.load(Ordering::SeqCst)
        }

        fn resets(&self) -> usize {
            self.resets.load(Ordering::SeqCst)
        }
    }

    /// A sampler that reads nothing at all and counts what it was asked to do.
    struct FakeSampler(Arc<Calls>);

    impl Sample for FakeSampler {
        fn tick(&mut self) -> Snapshot {
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
        let Msg::Sampled(snapshot) = first;
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
    /// **Falsified** by deleting the `parked.swap(…)` branch: `resets` stays 0.
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
        pump_ten_periods(period).await;
        cmd_tx.send(Cmd::SetVisible(true)).expect("lane is live");

        assert!(pump_until(|| calls.ticks() > before).await);
        assert_eq!(
            calls.resets(),
            1,
            "the re-open must drop the stale /proc/stat baseline exactly once",
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

    /// …and a closed command lane ends the task, rather than polling on against
    /// a dropped reducer.
    #[tokio::test(start_paused = true)]
    async fn a_closed_lane_ends_the_task() {
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, _msg_rx) = cmd_channel::<Msg>();
        let task = tokio::spawn(sampler_task(
            cmd_rx,
            msg_tx,
            Duration::from_secs(1),
            Needs::default(),
        ));
        drop(cmd_tx);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the task must return when its lane closes")
            .expect("and not by panicking");
    }
}
