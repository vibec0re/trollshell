//! Sensors service — wraps the `hytte-sensors` leaf crate (#1249) with a
//! `futures-signals`-backed `Service`: a 1 Hz tick loop polls `/proc/stat`,
//! `/sys/.../cpufreq`, `/proc/meminfo`, `/proc/net/dev`, `/proc/diskstats`,
//! `/sys/class/hwmon`, `/sys/class/drm`, and optional `nvidia-smi` through
//! that crate's `read_*`/`compute_*` functions, and exposes CPU load/clock/
//! temp, memory usage, network I/O rates, disk I/O throughput, GPU stats, and
//! disk usage as signals. The actual procfs/sysfs reads, their pure data
//! shapes, and the unit tests that pin them all live in `hytte-sensors` —
//! this module owns only what needs `Mutable`/a tokio runtime: the
//! `SensorsHandles` wrapper, the tick loop, the sparkline history
//! accumulators, and the `AsyncFd` mount-table watcher.
//!
//! # Public API
//!
//! ```ignore
//! // Register once at startup:
//! .with(sensors::service())
//!
//! // Subscribe in widgets:
//! sensors::cpu()      -> impl Signal<Item = CpuLoad>
//! sensors::cpu_freq() -> impl Signal<Item = CpuFreq>
//! sensors::memory()   -> impl Signal<Item = Memory>
//! sensors::network()  -> impl Signal<Item = NetIo>
//! sensors::cpu_temp() -> impl Signal<Item = CpuTemp>
//! sensors::gpu()      -> impl Signal<Item = Option<GpuState>>
//! sensors::disk()     -> impl Signal<Item = DiskUsage>
//! sensors::disk_io()  -> impl Signal<Item = DiskIo>
//! ```

mod warn_latch;

use futures_signals::signal::{Mutable, Signal, SignalExt};
use futures_util::StreamExt;
use hytte_reactive::{Service, registry, spawn_supervised};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cast::u64_to_f64_bytes;

// The data shapes moved to `hytte-sensors` byte-for-byte (#1249) as plain
// `pub struct`/`pub enum` items directly in that crate's root, exactly where
// they used to live in this module — so re-exporting them with `pub use`
// here keeps `hytte_services::sensors::CpuLoad` (etc.) resolving to the same
// path every existing caller (this crate's own tests, `trollshell`'s
// widgets/panels) already uses. `MountSpec` stays a plain (non-`pub`) import:
// it was `pub(crate)` before the move and nothing outside this module ever
// names it.
pub use hytte_sensors::{
    CpuFreq, CpuLoad, CpuTemp, DiskIo, DiskMount, DiskUsage, GpuState, GpuVendor, Memory,
    NetConnections, NetInterface, NetIo,
};
use hytte_sensors::{
    GpuCache, MountSpec, compute_cpu_load, compute_disk_io, read_cpu_freq, read_cpu_temp,
    read_disk_for_specs, read_gpu_with_cache, read_mountlist, read_net_connections,
    read_proc_diskstats, read_proc_meminfo, read_proc_net_dev, read_proc_stat, read_process_count,
};
use warn_latch::{WARN_COOLDOWN, WarnLatch};

// ── Blocking-read bundle ───────────────────────────────────────────────────────

/// All data collected by a single blocking-I/O sweep.
///
/// Constructed inside `tokio::task::spawn_blocking` and returned to the async
/// poll loop so that no blocking syscall runs directly on a tokio worker thread.
///
/// `Default` (#1327 review MEDIUM 2) is for tests only — a fake injected
/// sampler that only cares about one or two fields can start from
/// `TickData::default()` rather than naming all twelve.
#[derive(Default)]
struct TickData {
    /// Parsed `/proc/stat` entries, or `None` on read error.
    cpu_stat: Option<Vec<(u64, u64)>>,
    /// Per-core CPU clock snapshot from `/sys/.../cpufreq`. Empty/default when
    /// no cpufreq governor is present (e.g. VMs).
    cpu_freq: CpuFreq,
    /// Parsed `/proc/meminfo`, or `None` on read error.
    mem: Option<Memory>,
    /// Parsed `/proc/net/dev`, or `None` on read error.
    net_dev: Option<Vec<(String, u64, u64)>>,
    /// Cumulative `(name, read_bytes, write_bytes)` per physical disk from
    /// `/proc/diskstats`, or `None` on read error.
    disk_io: Option<Vec<(String, u64, u64)>>,
    /// CPU package temp from the cached hwmon chip dir (fast path) or a fresh
    /// scan (slow path on first call / after chip disappears).
    cpu_temp: CpuTemp,
    /// Updated chip-dir cache to thread back into `PollState`.
    cpu_temp_chip: Option<PathBuf>,
    /// GPU state snapshot when this is a GPU tick.
    ///
    /// `None` means "not a GPU tick" (the GPU field should not be updated).
    /// Use the `gpu_tick` flag to distinguish a GPU tick that found no hardware
    /// from a non-GPU tick.
    gpu_state: Option<GpuState>,
    /// `true` if the GPU was polled this tick (regardless of whether hardware
    /// was found). When `false`, `gpu_state` should be ignored.
    gpu_tick: bool,
    /// TCP socket counts (read every 2 ticks; `None` on non-TCP ticks).
    net_conn: Option<NetConnections>,
    /// Process count from `/proc`.
    proc_count: u32,
    /// Disk usage (read every 5 ticks; `None` on non-disk ticks).
    disk: Option<DiskUsage>,
}

// ── Service handle ────────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct SensorsHandles {
    pub(crate) cpu: Mutable<CpuLoad>,
    pub(crate) cpu_freq: Mutable<CpuFreq>,
    pub(crate) memory: Mutable<Memory>,
    pub(crate) network: Mutable<NetIo>,
    pub(crate) disk_io: Mutable<DiskIo>,
    pub(crate) cpu_temp: Mutable<CpuTemp>,
    pub(crate) gpu: Mutable<Option<GpuState>>,
    pub(crate) disk: Mutable<DiskUsage>,
    pub(crate) net_connections: Mutable<NetConnections>,
    pub(crate) process_count: Mutable<u32>,
    /// Live list of real mounts from `/proc/self/mountinfo`. Updated by
    /// `mount_watch_loop`; consumed by `poll_tick`'s disk branch.
    pub(crate) mount_list: Mutable<Vec<MountSpec>>,
    // ── Sparkline history (#231) ──────────────────────────────────────────────
    // Process-wide ring buffers (last `HISTORY_CAP` samples) for the Stats-panel
    // sparklines, so the history lives in the service (one buffer, not one per
    // monitor) and outlives any widget — a lazily-built Stats page opens
    // pre-populated. Filled by per-metric accumulator tasks (`spawn_history`).
    pub(crate) cpu_hist: Mutable<Arc<VecDeque<f64>>>,
    pub(crate) mem_hist: Mutable<Arc<VecDeque<f64>>>,
    pub(crate) disk_io_hist: Mutable<Arc<VecDeque<f64>>>,
    pub(crate) gpu_load_hist: Mutable<Arc<VecDeque<f64>>>,
    pub(crate) gpu_vram_hist: Mutable<Arc<VecDeque<f64>>>,
    pub(crate) gpu_temp_hist: Mutable<Arc<VecDeque<f64>>>,
    // ── Clock + per-core history (#338) ───────────────────────────────────────
    // The CPU card's clock-aggregate row and its two per-core `MultiSparkline`
    // rows used to accumulate history in-widget, which is what kept the StatsCpu
    // page force-eager-built (#231/#336 hoisted only the *overall* CPU-load
    // line). Hoisting these here — a scalar ring for the clock aggregate, and
    // 2-D rings (one per core) for the per-core load/clock series — lets that
    // page build lazily and open pre-populated via `Sparkline::set_samples` /
    // `MultiSparkline::set_frames`.
    /// Aggregate clock ring: `max_hz / max_ceiling_hz` (0..=1) per tick.
    pub(crate) cpu_freq_hist: Mutable<Arc<VecDeque<f64>>>,
    /// Per-core load rings: one `HISTORY_CAP` ring per logical core (0..=1).
    pub(crate) cpu_per_core_hist: Mutable<Arc<Vec<VecDeque<f64>>>>,
    /// Per-core clock rings: one ring per core, each `hz / max_ceiling_hz` (0..=1).
    pub(crate) cpu_freq_per_core_hist: Mutable<Arc<Vec<VecDeque<f64>>>>,
}

/// Sample count each sparkline history keeps — matches `Sparkline::new(60)` in
/// the Stats panel so a `set_samples` snapshot fills the whole graph.
const HISTORY_CAP: usize = 60;

impl Default for SensorsHandles {
    fn default() -> Self {
        Self {
            cpu: Mutable::new(CpuLoad::default()),
            cpu_freq: Mutable::new(CpuFreq::default()),
            memory: Mutable::new(Memory::default()),
            network: Mutable::new(NetIo::default()),
            disk_io: Mutable::new(DiskIo::default()),
            cpu_temp: Mutable::new(CpuTemp::default()),
            gpu: Mutable::new(None),
            disk: Mutable::new(DiskUsage::default()),
            net_connections: Mutable::new(NetConnections::default()),
            process_count: Mutable::new(0),
            mount_list: Mutable::new(Vec::new()),
            cpu_hist: Mutable::new(Arc::new(VecDeque::new())),
            mem_hist: Mutable::new(Arc::new(VecDeque::new())),
            disk_io_hist: Mutable::new(Arc::new(VecDeque::new())),
            gpu_load_hist: Mutable::new(Arc::new(VecDeque::new())),
            gpu_vram_hist: Mutable::new(Arc::new(VecDeque::new())),
            gpu_temp_hist: Mutable::new(Arc::new(VecDeque::new())),
            cpu_freq_hist: Mutable::new(Arc::new(VecDeque::new())),
            cpu_per_core_hist: Mutable::new(Arc::new(Vec::new())),
            cpu_freq_per_core_hist: Mutable::new(Arc::new(Vec::new())),
        }
    }
}

/// Spawn a task that accumulates a `HISTORY_CAP`-sample ring for one sparkline:
/// it subscribes to `source`, and for each emit where `extract` yields a sample
/// pushes it and republishes the whole window into `sink`. `extract` returning
/// `None` (e.g. a GPU field that's absent this tick) leaves the ring unchanged —
/// mirroring the old per-widget "only push when present" behaviour.
fn spawn_history<T, F>(source: Mutable<T>, sink: Mutable<Arc<VecDeque<f64>>>, extract: F)
where
    T: Clone + Send + Sync + 'static,
    F: Fn(&T) -> Option<f64> + Clone + Send + 'static,
{
    spawn_supervised("sensors", move || {
        let source = source.clone();
        let sink = sink.clone();
        let extract = extract.clone();
        async move {
            let mut ring: VecDeque<f64> = VecDeque::with_capacity(HISTORY_CAP);
            let mut stream = source.signal_cloned().to_stream();
            while let Some(value) = stream.next().await {
                if let Some(sample) = extract(&value) {
                    if ring.len() == HISTORY_CAP {
                        ring.pop_front();
                    }
                    ring.push_back(sample);
                    sink.set(Arc::new(ring.clone()));
                }
            }
        }
    });
}

/// Spawn a task that accumulates a **per-core** history: one `HISTORY_CAP`-sample
/// ring per series, mirroring `hytte_ui`'s `MultiSparkline::push_frame` width-reset
/// semantics. For each `source` emit, `extract` yields one frame (one sample per
/// core); when the frame width changes (a CPU hot-plug, or the first real frame
/// after the empty default) the rings are cleared and re-sized to the new width
/// so history restarts cleanly, then the whole 2-D window is republished into
/// `sink`. Consumed by a lazily-built per-core `MultiSparkline` via `set_frames`.
fn spawn_per_core_history<T, F>(
    source: Mutable<T>,
    sink: Mutable<Arc<Vec<VecDeque<f64>>>>,
    extract: F,
) where
    T: Clone + Send + Sync + 'static,
    F: Fn(&T) -> Vec<f64> + Clone + Send + 'static,
{
    spawn_supervised("sensors", move || {
        let source = source.clone();
        let sink = sink.clone();
        let extract = extract.clone();
        async move {
            let mut series: Vec<VecDeque<f64>> = Vec::new();
            let mut stream = source.signal_cloned().to_stream();
            while let Some(value) = stream.next().await {
                let frame = extract(&value);
                if series.len() != frame.len() {
                    series.clear();
                    series.resize_with(frame.len(), || VecDeque::with_capacity(HISTORY_CAP));
                }
                for (buf, &sample) in series.iter_mut().zip(frame.iter()) {
                    if buf.len() == HISTORY_CAP {
                        buf.pop_front();
                    }
                    buf.push_back(sample);
                }
                sink.set(Arc::new(series.clone()));
            }
        }
    });
}

// ── Service marker ────────────────────────────────────────────────────────────

/// The sensors service marker type — pass to `App::with`.
pub struct SensorsService;

impl Service for SensorsService {
    type Handles = SensorsHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = SensorsHandles::default();
        let cpu_writer = handles.cpu.clone();
        let cpu_freq_writer = handles.cpu_freq.clone();
        let mem_writer = handles.memory.clone();
        let net_writer = handles.network.clone();
        let disk_io_writer = handles.disk_io.clone();
        let cpu_temp_writer = handles.cpu_temp.clone();
        let gpu_writer = handles.gpu.clone();
        let disk_writer = handles.disk.clone();
        let net_conn_writer = handles.net_connections.clone();
        let proc_count_writer = handles.process_count.clone();
        let mount_list_for_poll = handles.mount_list.clone();
        let mount_list_for_watch = handles.mount_list.clone();

        spawn_supervised("sensors", move || {
            let writers = PollWriters {
                cpu: cpu_writer.clone(),
                cpu_freq: cpu_freq_writer.clone(),
                mem: mem_writer.clone(),
                net: net_writer.clone(),
                disk_io: disk_io_writer.clone(),
                cpu_temp: cpu_temp_writer.clone(),
                gpu: gpu_writer.clone(),
                disk: disk_writer.clone(),
                net_conn: net_conn_writer.clone(),
                proc_count: proc_count_writer.clone(),
                mount_list: mount_list_for_poll.clone(),
            };
            poll_loop(writers)
        });
        spawn_supervised("sensors", move || {
            mount_watch_loop(mount_list_for_watch.clone())
        });

        // Sparkline history accumulators (#231): one ring per Stats graph, fed
        // off the live metric signals — extractors mirror each row's old
        // `spark.push(..)` scalar and its present/absent guard.
        spawn_history(handles.cpu.clone(), handles.cpu_hist.clone(), |c| {
            Some(c.overall)
        });
        spawn_history(handles.memory.clone(), handles.mem_hist.clone(), |m| {
            Some(if m.total == 0 {
                0.0
            } else {
                (u64_to_f64_bytes(m.used) / u64_to_f64_bytes(m.total)).clamp(0.0, 1.0)
            })
        });
        spawn_history(
            handles.disk_io.clone(),
            handles.disk_io_hist.clone(),
            |io| Some(io.read_bps + io.write_bps),
        );
        spawn_history(handles.gpu.clone(), handles.gpu_load_hist.clone(), |g| {
            g.as_ref().and_then(|s| s.load).map(|l| l * 100.0)
        });
        spawn_history(handles.gpu.clone(), handles.gpu_vram_hist.clone(), |g| {
            g.as_ref()
                .and_then(|s| s.memory_used_bytes.zip(s.memory_total_bytes))
                .map(|(used, total)| {
                    if total == 0 {
                        0.0
                    } else {
                        (u64_to_f64_bytes(used) / u64_to_f64_bytes(total) * 100.0).clamp(0.0, 100.0)
                    }
                })
        });
        spawn_history(handles.gpu.clone(), handles.gpu_temp_hist.clone(), |g| {
            g.as_ref().and_then(|s| s.temperature_celsius)
        });

        // CPU clock + per-core accumulators (#338): the clock aggregate and the
        // two per-core series that used to push history in-widget in the Stats
        // CPU card. Extractors mirror each row's old `push`/`push_frame` math and
        // its `max_ceiling_hz`-normalization (0.0 when no cpufreq governor).
        spawn_history(
            handles.cpu_freq.clone(),
            handles.cpu_freq_hist.clone(),
            |f| Some(normalized_clock(f.max_hz, f.max_ceiling_hz)),
        );
        spawn_per_core_history(
            handles.cpu.clone(),
            handles.cpu_per_core_hist.clone(),
            |c| c.per_core.clone(),
        );
        spawn_per_core_history(
            handles.cpu_freq.clone(),
            handles.cpu_freq_per_core_hist.clone(),
            |f| {
                let ceiling = f.max_ceiling_hz;
                f.per_core
                    .iter()
                    .map(|&hz| normalized_clock(hz, ceiling))
                    .collect()
            },
        );

        handles
    }
}

/// Normalize a clock frequency against the fixed `max_ceiling_hz` axis (the
/// highest `cpuinfo_max_freq` across cores), yielding a 0..=1 fraction. Returns
/// `0.0` when no ceiling is known (no cpufreq governor — VMs), matching the
/// Stats card's original in-widget normalization.
fn normalized_clock(hz: f64, ceiling_hz: f64) -> f64 {
    if ceiling_hz > 0.0 {
        hz / ceiling_hz
    } else {
        0.0
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the sensors service to register with the hytte runtime.
#[must_use]
pub fn service() -> SensorsService {
    SensorsService
}

/// Signal that emits the current CPU load.
pub fn cpu() -> impl Signal<Item = CpuLoad> {
    registry::with(|r| {
        r.get::<SensorsHandles>()
            .expect("sensors::service() not registered")
            .cpu
            .signal_cloned()
    })
}

/// Signal that emits the current per-core CPU clock (cpufreq) snapshot.
pub fn cpu_freq() -> impl Signal<Item = CpuFreq> {
    registry::with(|r| {
        r.get::<SensorsHandles>()
            .expect("sensors::service() not registered")
            .cpu_freq
            .signal_cloned()
    })
}

/// Signal that emits the current memory usage.
pub fn memory() -> impl Signal<Item = Memory> {
    registry::with(|r| {
        r.get::<SensorsHandles>()
            .expect("sensors::service() not registered")
            .memory
            .signal()
    })
}

/// Read one sparkline-history ring (`#231`) off the shared handles.
fn history_of(
    pick: impl FnOnce(&SensorsHandles) -> &Mutable<Arc<VecDeque<f64>>>,
) -> impl Signal<Item = Arc<VecDeque<f64>>> {
    registry::with(|r| {
        pick(
            r.get::<SensorsHandles>()
                .expect("sensors::service() not registered"),
        )
        .signal_cloned()
    })
}

/// Read one per-core history ring set (`#338`) off the shared handles. The
/// per-core twin of [`history_of`]: the item is a `Vec` of rings, one per core.
fn per_core_history_of(
    pick: impl FnOnce(&SensorsHandles) -> &Mutable<Arc<Vec<VecDeque<f64>>>>,
) -> impl Signal<Item = Arc<Vec<VecDeque<f64>>>> {
    registry::with(|r| {
        pick(
            r.get::<SensorsHandles>()
                .expect("sensors::service() not registered"),
        )
        .signal_cloned()
    })
}

/// CPU-load history (fraction 0..=1), `HISTORY_CAP` samples. See [`history_of`].
pub fn cpu_history() -> impl Signal<Item = Arc<VecDeque<f64>>> {
    history_of(|h| &h.cpu_hist)
}

/// Aggregate CPU-clock history (fraction 0..=1 of `max_ceiling_hz`),
/// `HISTORY_CAP` samples. Feeds the Stats CPU card's collapsed clock sparkline.
pub fn cpu_freq_history() -> impl Signal<Item = Arc<VecDeque<f64>>> {
    history_of(|h| &h.cpu_freq_hist)
}

/// Per-core CPU-load history (fraction 0..=1), one `HISTORY_CAP` ring per core.
/// Feeds the Stats CPU card's expanded per-core `MultiSparkline` via
/// `set_frames`. Tolerates a changing core count (rings reset on width change).
pub fn cpu_per_core_history() -> impl Signal<Item = Arc<Vec<VecDeque<f64>>>> {
    per_core_history_of(|h| &h.cpu_per_core_hist)
}

/// Per-core CPU-clock history (each core's `hz / max_ceiling_hz`, 0..=1), one
/// `HISTORY_CAP` ring per core. Feeds the expanded per-core clock
/// `MultiSparkline` via `set_frames`.
pub fn cpu_freq_per_core_history() -> impl Signal<Item = Arc<Vec<VecDeque<f64>>>> {
    per_core_history_of(|h| &h.cpu_freq_per_core_hist)
}

/// Memory-used-fraction history (0..=1).
pub fn memory_history() -> impl Signal<Item = Arc<VecDeque<f64>>> {
    history_of(|h| &h.mem_hist)
}

/// Combined disk read+write throughput history (bytes/s).
pub fn disk_io_history() -> impl Signal<Item = Arc<VecDeque<f64>>> {
    history_of(|h| &h.disk_io_hist)
}

/// GPU-load history (percent 0..=100); empty until a load reading appears.
pub fn gpu_load_history() -> impl Signal<Item = Arc<VecDeque<f64>>> {
    history_of(|h| &h.gpu_load_hist)
}

/// GPU-VRAM-used history (percent 0..=100).
pub fn gpu_vram_history() -> impl Signal<Item = Arc<VecDeque<f64>>> {
    history_of(|h| &h.gpu_vram_hist)
}

/// GPU-temperature history (°C).
pub fn gpu_temp_history() -> impl Signal<Item = Arc<VecDeque<f64>>> {
    history_of(|h| &h.gpu_temp_hist)
}

/// Signal that emits the current network I/O snapshot.
pub fn network() -> impl Signal<Item = NetIo> {
    registry::with(|r| {
        r.get::<SensorsHandles>()
            .expect("sensors::service() not registered")
            .network
            .signal_cloned()
    })
}

/// Signal that emits the current disk I/O throughput snapshot (aggregate
/// read/write rate across physical disks + cumulative totals since boot).
pub fn disk_io() -> impl Signal<Item = DiskIo> {
    registry::with(|r| {
        r.get::<SensorsHandles>()
            .expect("sensors::service() not registered")
            .disk_io
            .signal()
    })
}

/// Signal that emits the current CPU temperature.
pub fn cpu_temp() -> impl Signal<Item = CpuTemp> {
    registry::with(|r| {
        r.get::<SensorsHandles>()
            .expect("sensors::service() not registered")
            .cpu_temp
            .signal()
    })
}

/// Signal that emits the current TCP socket-state counts.
pub fn net_connections() -> impl Signal<Item = NetConnections> {
    registry::with(|r| {
        r.get::<SensorsHandles>()
            .expect("sensors::service() not registered")
            .net_connections
            .signal()
    })
}

/// Signal that emits the current GPU state, or `None` if no GPU detected.
pub fn gpu() -> impl Signal<Item = Option<GpuState>> {
    registry::with(|r| {
        r.get::<SensorsHandles>()
            .expect("sensors::service() not registered")
            .gpu
            .signal_cloned()
    })
}

/// Signal that emits the current disk usage for all tracked mount points.
pub fn disk() -> impl Signal<Item = DiskUsage> {
    registry::with(|r| {
        r.get::<SensorsHandles>()
            .expect("sensors::service() not registered")
            .disk
            .signal_cloned()
    })
}

/// Signal that emits the current number of running processes.
pub fn process_count() -> impl Signal<Item = u32> {
    registry::with(|r| {
        r.get::<SensorsHandles>()
            .expect("sensors::service() not registered")
            .process_count
            .signal_cloned()
    })
}

// ── Polling loop ──────────────────────────────────────────────────────────────

struct PollState {
    /// `(active_prev, total_prev)` per cpu line — index 0 = overall, 1+ = core N-1.
    cpu_prev: Vec<(u64, u64)>,
    /// name → `(rx_bytes, tx_bytes, sample_instant)`
    net_prev: HashMap<String, (u64, u64, Instant)>,
    /// disk name → `(read_bytes, write_bytes, sample_instant)` for the disk-I/O
    /// rate diff (mirrors `net_prev`).
    disk_io_prev: HashMap<String, (u64, u64, Instant)>,
    /// Resolved `/sys/class/hwmon/hwmonN` dir of the CPU sensor chip, cached
    /// after the first scan so each tick re-reads only its `temp*_input`
    /// instead of re-walking all of `/sys/class/hwmon`.
    cpu_temp_chip: Option<PathBuf>,
    /// Per-tick GPU probe cache: nvidia availability flag + Intel RC6 prev sample.
    gpu_cache: GpuCache,
    /// Tick counter for rate-limiting slower polls.
    tick: u64,
    /// Rate-caps for the four uncapped per-tick `warn!` sites (#770): a
    /// persistent failure logs at most once per [`WARN_COOLDOWN`] instead of
    /// once per second. See the `warn_latch` module docs.
    warn_blocking_io: WarnLatch,
    warn_cpu_stat: WarnLatch,
    warn_memory: WarnLatch,
    warn_network: WarnLatch,
}

impl PollState {
    fn new() -> Self {
        Self {
            cpu_prev: Vec::new(),
            net_prev: HashMap::new(),
            disk_io_prev: HashMap::new(),
            cpu_temp_chip: None,
            gpu_cache: GpuCache::default(),
            tick: 0,
            warn_blocking_io: WarnLatch::new(),
            warn_cpu_stat: WarnLatch::new(),
            warn_memory: WarnLatch::new(),
            warn_network: WarnLatch::new(),
        }
    }
}

/// Bundle of `Mutable` writers + the mount-list reader the poll loop needs.
/// Constructed in `SensorsService::start` from the `SensorsHandles` clones.
struct PollWriters {
    cpu: Mutable<CpuLoad>,
    cpu_freq: Mutable<CpuFreq>,
    mem: Mutable<Memory>,
    net: Mutable<NetIo>,
    disk_io: Mutable<DiskIo>,
    cpu_temp: Mutable<CpuTemp>,
    gpu: Mutable<Option<GpuState>>,
    disk: Mutable<DiskUsage>,
    net_conn: Mutable<NetConnections>,
    proc_count: Mutable<u32>,
    /// Read-only on this side; the watcher loop mutates.
    mount_list: Mutable<Vec<MountSpec>>,
}

/// Inputs to one tick's blocking body, bundled so the body itself can be
/// swapped out (see [`poll_tick`]) without a long parameter list.
struct TickInputs {
    /// Cached hwmon chip dir, taken out of `PollState` for the duration of
    /// the blocking call.
    chip: Option<PathBuf>,
    /// GPU probe cache, taken out of `PollState` for the duration of the
    /// blocking call. `poll_tick` keeps its own clone so a panicked call
    /// doesn't lose it (#1327).
    gpu_cache: GpuCache,
    do_gpu: bool,
    do_net_conn: bool,
    do_disk: bool,
    /// Cloned mount list, only non-empty on a disk tick.
    specs: Vec<MountSpec>,
}

/// The real per-tick blocking body: every blocking syscall for one tick,
/// bundled so it runs entirely inside `tokio::task::spawn_blocking` (never on
/// a tokio worker thread) and so a test can substitute a fake body — see
/// [`poll_tick`] — without touching a real `/proc`/`/sys`.
fn sample_tick(inputs: TickInputs) -> (TickData, GpuCache) {
    let TickInputs {
        chip,
        gpu_cache,
        do_gpu,
        do_net_conn,
        do_disk,
        specs,
    } = inputs;

    // CPU
    let cpu_stat = read_proc_stat().ok();
    // CPU clock (per-core cpufreq)
    let cpu_freq = read_cpu_freq();
    // Memory
    let mem = read_proc_meminfo().ok();
    // Network I/O
    let net_dev = read_proc_net_dev().ok();
    // Disk I/O (physical-disk read/write byte counters)
    let disk_io = read_proc_diskstats().ok();
    // CPU temp (with cached chip dir)
    let (cpu_temp, cpu_temp_chip) = {
        let mut ch = chip;
        let temp = read_cpu_temp(&mut ch);
        (temp, ch)
    };
    // GPU (every 2 ticks)
    let (gpu_state, new_gpu_cache) = if do_gpu {
        read_gpu_with_cache(gpu_cache)
    } else {
        (None, gpu_cache)
    };
    // TCP socket counts (every 2 ticks)
    let net_conn = if do_net_conn {
        Some(read_net_connections())
    } else {
        None
    };
    // Process count
    let proc_count = read_process_count();
    // Disk (every 5 ticks)
    let disk = if do_disk {
        Some(read_disk_for_specs(&specs))
    } else {
        None
    };
    (
        TickData {
            cpu_stat,
            cpu_freq,
            mem,
            net_dev,
            disk_io,
            cpu_temp,
            cpu_temp_chip,
            gpu_state,
            gpu_tick: do_gpu,
            net_conn,
            proc_count,
            disk,
        },
        new_gpu_cache,
    )
}

/// Run one poll tick: gather `TickInputs` from `state`, run `sample` on a
/// blocking thread, and fold the result back into `state`/`w`.
///
/// `sample` is injectable (rather than hardcoded to [`sample_tick`]) so a
/// test can make it panic on demand and assert what `poll_tick` does with
/// `state.gpu_cache` afterwards, without a real `/sys`/`/proc` failure to
/// provoke (#1327). Production always calls it with `sample_tick`, via
/// [`poll_loop`].
///
/// Does **not** sleep — [`poll_loop`] owns the real 1 Hz cadence so a test
/// can call this directly, back to back, with no wall-clock wait.
async fn poll_tick<F>(state: &mut PollState, w: &PollWriters, sample: F)
where
    F: FnOnce(TickInputs) -> (TickData, GpuCache) + Send + 'static,
{
    let now = Instant::now();

    // Snapshot tick-local flags before moving `state` fields into the closure.
    let tick = state.tick;
    let do_gpu = tick.is_multiple_of(2);
    let do_net_conn = tick.is_multiple_of(2);
    let do_disk = tick.is_multiple_of(5);

    // Take the chip-dir cache out of state so the blocking call can own it,
    // keeping a clone behind for the same reason the GPU cache does below: a
    // panicked call must not cost the *next* tick a slow `/sys/class/hwmon`
    // re-walk (#1327 review MEDIUM 1 — the same bug the issue named, one
    // `take()` over).
    let chip = state.cpu_temp_chip.take();
    let chip_before_tick = chip.clone();
    // Move the GPU cache out of state so the blocking call can own it, but
    // keep a clone behind: if the call panics, the moved-in cache is gone
    // with it, and without this clone `state.gpu_cache` would silently fall
    // back to `Default` — re-probing `nvidia-smi` availability from scratch
    // on the next tick (#1327).
    let gpu_cache = std::mem::take(&mut state.gpu_cache);
    let gpu_cache_before_tick = gpu_cache.clone();

    // Mount list is cloned here (cheap — it's rarely non-empty).
    let specs = if do_disk {
        w.mount_list.get_cloned()
    } else {
        Vec::new()
    };

    let inputs = TickInputs {
        chip,
        gpu_cache,
        do_gpu,
        do_net_conn,
        do_disk,
        specs,
    };

    // ── All blocking I/O runs on a dedicated blocking thread ──────────
    let data = tokio::task::spawn_blocking(move || sample(inputs)).await;

    let Ok((data, new_gpu_cache)) = data else {
        log_blocking_io_panic(&mut state.warn_blocking_io, now);
        // Restore both caches the panicked call took ownership of, rather
        // than leaving them at the `Default`/`None` the `take`s left behind
        // (#1327; the chip half is MEDIUM 1 of the review).
        state.gpu_cache = gpu_cache_before_tick;
        state.cpu_temp_chip = chip_before_tick;
        state.tick = state.tick.wrapping_add(1);
        return;
    };
    log_blocking_io_recovery(&mut state.warn_blocking_io);

    // Thread the chip cache back.
    state.cpu_temp_chip = data.cpu_temp_chip;
    // Thread the GPU cache back (always returned, even on non-GPU ticks).
    state.gpu_cache = new_gpu_cache;

    apply_cpu_load(state, data.cpu_stat, &w.cpu, now);
    apply_cpu_freq(data.cpu_freq, &w.cpu_freq);
    apply_memory(state, data.mem, &w.mem, now);
    apply_network(state, data.net_dev, now, &w.net);
    apply_disk_io(&mut state.disk_io_prev, data.disk_io, now, &w.disk_io);
    apply_cpu_temp(data.cpu_temp, &w.cpu_temp);
    apply_gpu(data.gpu_tick, data.gpu_state, &w.gpu);
    apply_disk(data.disk, &w.disk);
    apply_conn_counts(data.net_conn, &w.net_conn);
    w.proc_count.set(data.proc_count);

    state.tick = state.tick.wrapping_add(1);
}

async fn poll_loop(w: PollWriters) {
    poll_loop_with(w, sample_tick).await;
}

/// [`poll_loop`], generic over the per-tick sampler.
///
/// `poll_tick`'s own injectability (added for #1327's panic test) cannot, on
/// its own, catch a mutation confined to *this* function's body — nothing
/// above ever calls `poll_loop`/`poll_loop_with` at all, so a change here
/// that fed `poll_tick` a sampler wrapped to discard whatever `PollState`
/// persisted (e.g. resetting `TickInputs::gpu_cache` to `Default` every
/// tick) would be invisible to the whole suite (#1327 review MEDIUM 2,
/// second half — the seam-wrapper hole moved up one layer). Splitting this
/// out is what lets a test drive the *actual* production loop, real 1 Hz
/// sleep included, with a fake sampler standing in for `sample_tick`.
///
/// `sample` is `Fn` rather than `FnMut`: cloning the `Arc` once per tick
/// (rather than needing `Clone`/interior mutability plumbed through the
/// bound itself) is enough, since `sample_tick` and every test sampler here
/// use `Mutex`/atomics internally when they need to remember anything across
/// calls.
async fn poll_loop_with<F>(w: PollWriters, sample: F)
where
    F: Fn(TickInputs) -> (TickData, GpuCache) + Send + Sync + 'static,
{
    let sample = Arc::new(sample);
    let mut state = PollState::new();
    loop {
        let sample = Arc::clone(&sample);
        poll_tick(&mut state, &w, move |inputs| sample(inputs)).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ── Per-concern publish helpers ───────────────────────────────────────────────

/// Log the blocking-I/O-task-panicked failure, rate-capped (#770). Split out
/// of `poll_tick` to keep it under `clippy::too_many_lines` — see the
/// `warn_latch` module docs for the cadence.
fn log_blocking_io_panic(latch: &mut WarnLatch, now: Instant) {
    if let Some(suppressed) = latch.on_failure(now, WARN_COOLDOWN) {
        tracing::warn!(
            suppressed,
            "sensors: blocking I/O task panicked; skipping tick"
        );
    }
}

/// Log recovery from a blocking-I/O-task-panic streak, if anything was
/// suppressed while it was failing (#770). Counterpart to
/// [`log_blocking_io_panic`].
fn log_blocking_io_recovery(latch: &mut WarnLatch) {
    if let Some(suppressed) = latch.on_success() {
        tracing::warn!(
            suppressed,
            "sensors: blocking I/O task recovered; no longer panicking"
        );
    }
}

/// Compute and publish CPU load; update the rolling `cpu_prev` cache.
///
/// Takes `&mut PollState` (rather than just `cpu_prev`) because it also owns
/// the failure latch for #770's rate-capped `warn!` — see the `warn_latch`
/// module docs — and threading both `&mut` pieces separately through
/// `poll_tick`'s call site is what pushed that function over
/// `clippy::too_many_lines`.
fn apply_cpu_load(
    state: &mut PollState,
    cpu_stat: Option<Vec<(u64, u64)>>,
    writer: &Mutable<CpuLoad>,
    now: Instant,
) {
    match cpu_stat {
        Some(cpu_now) => {
            let load = compute_cpu_load(&state.cpu_prev, &cpu_now);
            state.cpu_prev = cpu_now;
            writer.set(load);
            if let Some(suppressed) = state.warn_cpu_stat.on_success() {
                tracing::warn!(suppressed, "sensors: /proc/stat reads recovered");
            }
        }
        None => {
            if let Some(suppressed) = state.warn_cpu_stat.on_failure(now, WARN_COOLDOWN) {
                tracing::warn!(suppressed, "sensors: failed to read /proc/stat");
            }
        }
    }
}

/// Publish the per-core CPU clock snapshot (every tick).
///
/// The reader already degrades to a default `CpuFreq` when no cpufreq governor
/// is present, so there is no error variant to warn about here.
fn apply_cpu_freq(cpu_freq: CpuFreq, writer: &Mutable<CpuFreq>) {
    writer.set(cpu_freq);
}

/// Publish memory usage, or warn on read failure.
///
/// Takes `&mut PollState` for the same reason as [`apply_cpu_load`]: it owns
/// the #770 failure latch, and this keeps `poll_tick`'s call site to one
/// line instead of threading the latch through separately.
fn apply_memory(
    state: &mut PollState,
    mem: Option<Memory>,
    writer: &Mutable<Memory>,
    now: Instant,
) {
    match mem {
        Some(mem) => {
            writer.set(mem);
            if let Some(suppressed) = state.warn_memory.on_success() {
                tracing::warn!(suppressed, "sensors: /proc/meminfo reads recovered");
            }
        }
        None => {
            if let Some(suppressed) = state.warn_memory.on_failure(now, WARN_COOLDOWN) {
                tracing::warn!(suppressed, "sensors: failed to read /proc/meminfo");
            }
        }
    }
}

/// Compute per-interface byte rates from the new `/proc/net/dev` snapshot,
/// update the rolling `net_prev` cache, and publish the `NetIo` snapshot.
///
/// Takes `&mut PollState` for the same reason as [`apply_cpu_load`]: it owns
/// the #770 failure latch alongside `net_prev`.
fn apply_network(
    state: &mut PollState,
    net_dev: Option<Vec<(String, u64, u64)>>,
    now: Instant,
    writer: &Mutable<NetIo>,
) {
    match net_dev {
        Some(net_now) => {
            let mut interfaces = Vec::new();
            let mut next_net_prev = HashMap::new();

            for (name, rx, tx) in net_now {
                let (rx_rate, tx_rate) = match state.net_prev.get(&name) {
                    Some((prev_rx, prev_tx, prev_when)) => {
                        let dt = now.duration_since(*prev_when).as_secs_f64().max(0.1);
                        let rx_r = u64_to_f64_bytes(rx.saturating_sub(*prev_rx)) / dt;
                        let tx_r = u64_to_f64_bytes(tx.saturating_sub(*prev_tx)) / dt;
                        (rx_r, tx_r)
                    }
                    None => (0.0, 0.0),
                };
                interfaces.push(NetInterface {
                    name: name.clone(),
                    rx_bytes_total: rx,
                    tx_bytes_total: tx,
                    rx_rate_bps: rx_rate,
                    tx_rate_bps: tx_rate,
                });
                next_net_prev.insert(name, (rx, tx, now));
            }

            state.net_prev = next_net_prev;
            writer.set(NetIo { interfaces });
            if let Some(suppressed) = state.warn_network.on_success() {
                tracing::warn!(suppressed, "sensors: /proc/net/dev reads recovered");
            }
        }
        None => {
            if let Some(suppressed) = state.warn_network.on_failure(now, WARN_COOLDOWN) {
                tracing::warn!(suppressed, "sensors: failed to read /proc/net/dev");
            }
        }
    }
}

/// Compute the aggregate disk I/O rate from the new `/proc/diskstats` snapshot,
/// update the rolling `disk_io_prev` cache, and publish the `DiskIo` snapshot.
/// Mirrors [`apply_network`], summed across physical disks.
fn apply_disk_io(
    disk_io_prev: &mut HashMap<String, (u64, u64, Instant)>,
    disk_io: Option<Vec<(String, u64, u64)>>,
    now: Instant,
    writer: &Mutable<DiskIo>,
) {
    match disk_io {
        Some(devices) => {
            let (snapshot, next_prev) = compute_disk_io(disk_io_prev, devices, now);
            *disk_io_prev = next_prev;
            writer.set(snapshot);
        }
        None => {
            tracing::warn!("sensors: failed to read /proc/diskstats");
        }
    }
}

/// Publish the CPU package temperature (every tick).
fn apply_cpu_temp(cpu_temp: CpuTemp, writer: &Mutable<CpuTemp>) {
    writer.set(cpu_temp);
}

/// Publish the GPU state snapshot (only on GPU ticks).
fn apply_gpu(gpu_tick: bool, gpu_state: Option<GpuState>, writer: &Mutable<Option<GpuState>>) {
    if gpu_tick {
        writer.set(gpu_state);
    }
}

/// Publish disk usage (only on disk ticks).
fn apply_disk(disk: Option<DiskUsage>, writer: &Mutable<DiskUsage>) {
    if let Some(disk) = disk {
        writer.set(disk);
    }
}

/// Publish TCP socket-state counts (only on net-conn ticks).
fn apply_conn_counts(net_conn: Option<NetConnections>, writer: &Mutable<NetConnections>) {
    if let Some(nc) = net_conn {
        writer.set(nc);
    }
}

// ── Mount table watcher ──────────────────────────────────────────────────────

/// Background task: keep `mount_list` in sync with `/proc/self/mountinfo`.
///
/// Seeds the Mutable once, then waits for `POLLPRI` events on the open file
/// — the kernel signals POLLPRI on `/proc/self/mountinfo` whenever the mount
/// table changes (mount, unmount, remount). On each event we re-parse the
/// file from scratch via [`read_mountlist`].
///
/// Failure modes (open error, `AsyncFd` registration error, poll error) all
/// log a warning and exit. The Mutable then either stays empty (if the
/// initial open failed) or holds whatever was last successfully read.
async fn mount_watch_loop(mount_list: Mutable<Vec<MountSpec>>) {
    use std::os::fd::OwnedFd;
    use tokio::io::Interest;
    use tokio::io::unix::AsyncFd;

    // Seed once before we even attempt to register for events. This way a
    // POLLPRI registration failure still leaves us with a correct list as
    // of startup.
    //
    // `read_mountlist` does blocking file I/O; run it on a blocking thread.
    mount_list.set(
        tokio::task::spawn_blocking(read_mountlist)
            .await
            .unwrap_or_default(),
    );

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
                // `read_mountlist` does blocking file I/O; run it on a blocking thread.
                let new_list = tokio::task::spawn_blocking(read_mountlist)
                    .await
                    .unwrap_or_default();
                mount_list.set(new_list);
            }
            Err(e) => {
                tracing::warn!(error = %e, "sensors: mountinfo poll error, exiting watcher");
                return;
            }
        }
    }
}

// ── Publisher tests (#1172) ──────────────────────────────────────────────────
//
// The leaf parsers in `hytte-sensors` (`compute_cpu_load`, `compute_disk_io`,
// …) are well covered by their own modules' tests (#1249). The `apply_*` functions
// that fold each tick's sample into a `Mutable` — the glue `poll_tick` calls —
// had none: whether a `None` sample is silently skipped vs. warned-once,
// whether a "tick-gated" publisher (`apply_gpu`/`apply_disk`/
// `apply_conn_counts`) actually leaves the writer untouched off-tick, and
// `apply_network`'s inline rate computation (the one rate calculation in this
// module that is *not* behind an already-tested leaf function) were all
// unexercised.
#[cfg(test)]
mod tests {
    use super::{
        CpuFreq, CpuLoad, CpuTemp, DiskIo, DiskMount, DiskUsage, GpuCache, GpuState, GpuVendor,
        NetConnections, NetIo, PollState, PollWriters, TickData, TickInputs, WARN_COOLDOWN,
        apply_conn_counts, apply_cpu_freq, apply_cpu_load, apply_cpu_temp, apply_disk,
        apply_disk_io, apply_gpu, apply_memory, apply_network, poll_loop_with, poll_tick,
        read_gpu_with_cache,
    };
    use futures_signals::signal::Mutable;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// A fresh set of `PollWriters` over brand-new `Mutable`s, for tests that
    /// need to drive [`poll_tick`] directly without a registered `Service`.
    fn fresh_poll_writers() -> PollWriters {
        PollWriters {
            cpu: Mutable::new(CpuLoad::default()),
            cpu_freq: Mutable::new(CpuFreq::default()),
            mem: Mutable::new(super::Memory::default()),
            net: Mutable::new(NetIo::default()),
            disk_io: Mutable::new(DiskIo::default()),
            cpu_temp: Mutable::new(CpuTemp::default()),
            gpu: Mutable::new(None),
            disk: Mutable::new(DiskUsage::default()),
            net_conn: Mutable::new(NetConnections::default()),
            proc_count: Mutable::new(0),
            mount_list: Mutable::new(Vec::new()),
        }
    }

    // ── poll_tick: BOTH caches must survive a panicked blocking tick ────────
    //
    // #1327: the tick moves `state.gpu_cache` (and, review MEDIUM 1: the
    // exact same bug, `state.cpu_temp_chip`) into the blocking closure and
    // only threads fresh values back on success; on the `Err` (panicked) arm
    // the moved-in values were lost, silently resetting `state.gpu_cache` to
    // `Default` and `state.cpu_temp_chip` to `None` — which costs the next
    // tick a re-probe of `nvidia-smi` availability (and, where present, a
    // fork) outside #1297's TTL, and a slow `/sys/class/hwmon` re-walk for
    // the chip dir.
    //
    // `GpuCache`'s fields are `pub(super)` inside `hytte-sensors`, so this
    // crate cannot build one by hand; running the real `read_gpu_with_cache`
    // once (fast, no real GPU or `nvidia-smi` required to terminate) is the
    // only way to get a concrete non-`Default` value to seed with — the
    // *panicked tick* itself never touches a real sampler, since the
    // injected one below is pure fake. `PathBuf` (the chip dir's type) needs
    // no such trick — it is a plain, publicly constructible `std` type.
    //
    // Do **not** add `PartialEq` to `GpuCache` to replace the `Debug`-string
    // compare below: `GpuState::load` is an `f64`, and this tree has already
    // been bitten by a derived `PartialEq` over `f64` used as an equality
    // gate (a `NaN` defeats it forever). The string compare is the better
    // choice here, not a workaround for `PartialEq`'s absence.
    //
    // Falsification (both quoted in the PR): delete
    // `state.gpu_cache = gpu_cache_before_tick;` on the `Err` arm in
    // `poll_tick` for the original #1327 bug, or delete
    // `state.cpu_temp_chip = chip_before_tick;` for review MEDIUM 1.
    #[tokio::test]
    async fn a_panicked_tick_restores_both_pre_tick_caches() {
        let (_gpu_state, seeded) = read_gpu_with_cache(GpuCache::default());
        let seeded_debug = format!("{seeded:?}");
        let chip = PathBuf::from("/sys/class/hwmon/hwmon3");

        let mut state = PollState::new();
        state.gpu_cache = seeded;
        state.cpu_temp_chip = Some(chip.clone());

        let w = fresh_poll_writers();

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in_closure = Arc::clone(&calls);
        poll_tick(
            &mut state,
            &w,
            move |_inputs: TickInputs| -> (TickData, GpuCache) {
                calls_in_closure.fetch_add(1, Ordering::SeqCst);
                panic!("a_panicked_tick_restores_both_pre_tick_caches: injected panic (#1327)");
            },
        )
        .await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the injected sampler must have run exactly once"
        );
        assert_eq!(
            format!("{:?}", state.gpu_cache),
            seeded_debug,
            "a panicked blocking tick must restore the pre-tick GpuCache rather than silently \
             resetting it to Default (#1327)"
        );
        assert_eq!(
            state.cpu_temp_chip,
            Some(chip),
            "a panicked blocking tick must restore the pre-tick hwmon chip dir too \
             (#1327 review MEDIUM 1)"
        );
        // LOW 2: the panic arm's other two post-conditions, pinned in the same
        // test rather than left free — `state.tick` must still advance (a
        // panic streak that recovers should come back on the same phase
        // parity it left), and the #770 rate-cap latch must have recorded a
        // failure (`log_blocking_io_panic`), not merely been left untouched.
        // `on_failure` is `pub(super)` inside `warn_latch`, visible here as a
        // descendant of `sensors` — a fresh call reading back `None` ("a
        // streak is already in progress") is the only way to observe that
        // without a tracing subscriber.
        assert_eq!(
            state.tick, 1,
            "the phase counter must still advance across a panicked tick"
        );
        assert_eq!(
            state
                .warn_blocking_io
                .on_failure(Instant::now(), WARN_COOLDOWN),
            None,
            "the panic must already have recorded a failure in the #770 rate-cap latch"
        );
    }

    /// **The `Ok` arm's own forward-threading** (#1327 review MEDIUM 2): a
    /// successful tick must *adopt* what the sampler handed back, not merely
    /// survive a panic. Without this, restoring the pre-tick cache
    /// unconditionally — on **both** arms, not just the panicked one — passes
    /// [`a_panicked_tick_restores_both_pre_tick_caches`] while #1297's
    /// `nvidia-smi` TTL memo (and the hwmon chip-dir cache) never advance at
    /// all: every tick would re-probe/re-walk, strictly worse than the bug
    /// #1327 names.
    ///
    /// `fresh` is real (via `read_gpu_with_cache`, for the same
    /// cross-crate-construction reason as above) and `PollState::new()`
    /// starts at `GpuCache::default()` (`nvidia_available: None`), so the two
    /// are always distinguishable regardless of the box this runs on.
    ///
    /// Falsification: replace the `Ok` arm's `state.gpu_cache = new_gpu_cache;`
    /// / `state.cpu_temp_chip = data.cpu_temp_chip;` with the pre-tick values
    /// — this reds with the fresh cache's `Debug` string on the right and the
    /// `Default` one (still sitting in `state` from `PollState::new()`) on
    /// the left.
    #[tokio::test]
    async fn a_returning_tick_adopts_what_the_sampler_handed_back() {
        let (_gpu_state, fresh) = read_gpu_with_cache(GpuCache::default());
        let fresh_debug = format!("{fresh:?}");
        let chip = PathBuf::from("/sys/class/hwmon/hwmon9");

        let mut state = PollState::new();
        let w = fresh_poll_writers();
        let chip_in_closure = chip.clone();
        poll_tick(&mut state, &w, move |_inputs: TickInputs| {
            (
                TickData {
                    cpu_temp_chip: Some(chip_in_closure),
                    ..TickData::default()
                },
                fresh,
            )
        })
        .await;

        assert_eq!(
            format!("{:?}", state.gpu_cache),
            fresh_debug,
            "a completed tick must adopt the cache the sampler returned"
        );
        assert_eq!(
            state.cpu_temp_chip,
            Some(chip),
            "…and the chip dir it resolved"
        );
        assert_eq!(state.tick, 1, "…and the phase counter advances");
    }

    /// **The production `poll_loop` entry point itself must thread the cache
    /// from one tick into the next** (#1327 review MEDIUM 2, second half): a
    /// mutation confined entirely to `poll_loop`/`poll_loop_with`'s own body —
    /// e.g. handing `poll_tick` a sampler wrapped to discard whatever
    /// `PollState` persisted and always report a fresh `GpuCache::default()`
    /// — is invisible to every test above, since none of them ever calls
    /// `poll_loop`/`poll_loop_with` at all; they all drive `poll_tick`
    /// directly. This is the same seam-wrapper shape item 2 of #1327 exists
    /// to close, reintroduced by this file's own #1327 refactor one layer out:
    /// `poll_tick(injected)` tested, `poll_loop` shipping untested.
    ///
    /// Drives two REAL ticks of `poll_loop_with` — production's own loop,
    /// real 1 s sleep between ticks included — with an injected `Fn` sampler
    /// that records the `GpuCache` it was handed on each call (via a
    /// `Mutex<Vec<_>>`, since the bound is `Fn`, not `FnMut`: interior
    /// mutability is what lets one `Arc`-shared sampler serve every tick
    /// without `poll_loop_with` itself needing a `Clone`/`FnMut` sampler) and
    /// hands back a distinct, real `GpuCache` on its first call. A short real
    /// wall-clock wait (a touch over one sleep interval) is deliberate here
    /// rather than a paused virtual clock: `spawn_blocking`'s completion is
    /// real-thread-scheduled regardless of tokio's time driver, and mixing
    /// that with `start_paused` is exactly the kind of interaction this
    /// tree's own notes warn is fragile.
    ///
    /// Falsification: wrap `poll_loop_with`'s call to `poll_tick` so the
    /// sampler it hands `poll_tick` always resets `TickInputs::gpu_cache` to
    /// `GpuCache::default()` first (mutation (b) from the review) — this reds
    /// on the second-tick assertion, `Default` on the left instead of the
    /// first tick's real answer.
    #[tokio::test]
    async fn poll_loop_threads_the_cache_from_one_tick_into_the_next() {
        let (_gpu_state, distinct) = read_gpu_with_cache(GpuCache::default());
        let distinct_debug = format!("{distinct:?}");
        let default_debug = format!("{:?}", GpuCache::default());

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_in_closure = Arc::clone(&seen);
        let handed_out: Arc<Mutex<Option<GpuCache>>> = Arc::new(Mutex::new(Some(distinct)));

        let w = fresh_poll_writers();
        let handle = tokio::spawn(poll_loop_with(w, move |inputs: TickInputs| {
            seen_in_closure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("{:?}", inputs.gpu_cache));
            let next = handed_out
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .unwrap_or_default();
            (TickData::default(), next)
        }));

        // The first tick runs immediately; the second follows the one real
        // second `poll_loop_with` sleeps between ticks. 1.3s leaves margin
        // without waiting for a third.
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        handle.abort();

        let seen = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            seen.len() >= 2,
            "expected at least two ticks of poll_loop_with to have run, saw {}",
            seen.len()
        );
        assert_eq!(
            seen[0], default_debug,
            "the first tick starts from PollState::new()'s Default cache"
        );
        assert_eq!(
            seen[1], distinct_debug,
            "the second tick must see the cache the FIRST tick returned — poll_loop's own \
             loop, not just poll_tick in isolation, must thread it (#1327 review MEDIUM 2)"
        );
    }

    // ── apply_cpu_load: table test (input sample → published value) ─────────

    #[allow(clippy::float_cmp)]
    #[test]
    fn apply_cpu_load_table() {
        struct Case {
            name: &'static str,
            prev: Vec<(u64, u64)>,
            sample: Option<Vec<(u64, u64)>>,
            want_overall: Option<f64>,
            want_prev_after: Vec<(u64, u64)>,
        }
        let cases = [
            Case {
                name: "first sample: no prior, load reads 0",
                prev: Vec::new(),
                sample: Some(vec![(50, 200)]),
                want_overall: Some(0.0),
                want_prev_after: vec![(50, 200)],
            },
            Case {
                name: "second sample: half the total delta was active",
                prev: vec![(50, 200)],
                sample: Some(vec![(150, 400)]),
                // d_active=100, d_total=200 → 0.5
                want_overall: Some(0.5),
                want_prev_after: vec![(150, 400)],
            },
            Case {
                // Two identical `/proc/stat` samples: the jiffy total did not
                // move, so there is no interval to divide by. The leaf guards
                // this (`proc_stat.rs`'s `d_total == 0 → 0.0`); the table is
                // the place that *says* a stalled counter reads 0 rather than
                // NaN or a panic.
                name: "identical samples: d_total == 0 reads 0, not NaN",
                prev: vec![(50, 200)],
                sample: Some(vec![(50, 200)]),
                want_overall: Some(0.0),
                want_prev_after: vec![(50, 200)],
            },
            Case {
                name: "read failure: writer untouched, prev untouched",
                prev: vec![(50, 200)],
                sample: None,
                want_overall: None,
                want_prev_after: vec![(50, 200)],
            },
        ];

        for c in cases {
            let mut state = PollState::new();
            state.cpu_prev = c.prev;
            let writer = Mutable::new(CpuLoad {
                overall: -1.0,
                per_core: Vec::new(),
            });
            apply_cpu_load(&mut state, c.sample, &writer, Instant::now());

            if let Some(want) = c.want_overall {
                assert_eq!(writer.get_cloned().overall, want, "{}", c.name);
            } else {
                assert_eq!(
                    writer.get_cloned().overall,
                    -1.0,
                    "{}: a failed sample must not publish",
                    c.name
                );
            }
            assert_eq!(state.cpu_prev, c.want_prev_after, "{}", c.name);
        }
    }

    // ── apply_cpu_freq: unconditional passthrough ────────────────────────────

    #[allow(clippy::float_cmp)]
    #[test]
    fn apply_cpu_freq_publishes_verbatim() {
        let writer = Mutable::new(CpuFreq::default());
        let sample = CpuFreq {
            max_hz: 3_200_000_000.0,
            per_core: vec![3_200_000_000.0, 1_800_000_000.0],
            max_ceiling_hz: 4_000_000_000.0,
        };
        apply_cpu_freq(sample.clone(), &writer);
        let got = writer.get_cloned();
        assert_eq!(got.max_hz, sample.max_hz);
        assert_eq!(got.per_core, sample.per_core);
        assert_eq!(got.max_ceiling_hz, sample.max_ceiling_hz);
    }

    // ── apply_memory: table test ──────────────────────────────────────────────

    #[test]
    fn apply_memory_table() {
        let mut state = PollState::new();
        let writer = Mutable::new(super::Memory {
            total: 999,
            ..Default::default()
        });

        // A successful read publishes verbatim.
        apply_memory(
            &mut state,
            Some(super::Memory {
                total: 16_000_000_000,
                free: 4_000_000_000,
                available: 8_000_000_000,
                used: 8_000_000_000,
                swap_used: 0,
                swap_total: 2_000_000_000,
            }),
            &writer,
            Instant::now(),
        );
        assert_eq!(writer.get_cloned().total, 16_000_000_000);

        // A failed read leaves the last published value in place.
        apply_memory(&mut state, None, &writer, Instant::now());
        assert_eq!(
            writer.get_cloned().total,
            16_000_000_000,
            "a failed read must not clobber the last-known value"
        );
    }

    // ── apply_network: table test + the net-rate cache across two ticks ──────

    #[allow(clippy::float_cmp)]
    #[test]
    fn apply_network_table() {
        let mut state = PollState::new();
        let writer = Mutable::new(NetIo::default());

        // First sample for an interface: no prior entry, so the rate reads 0
        // even though the counters are non-zero (nothing to diff against).
        apply_network(
            &mut state,
            Some(vec![("eth0".to_string(), 1_000, 2_000)]),
            Instant::now(),
            &writer,
        );
        let first = writer.get_cloned();
        assert_eq!(first.interfaces.len(), 1);
        assert_eq!(first.interfaces[0].rx_bytes_total, 1_000);
        assert_eq!(first.interfaces[0].tx_bytes_total, 2_000);
        assert_eq!(first.interfaces[0].rx_rate_bps, 0.0, "no prior sample yet");

        // A read failure leaves the last-known snapshot in place.
        apply_network(&mut state, None, Instant::now(), &writer);
        assert_eq!(
            writer.get_cloned().interfaces.len(),
            1,
            "unchanged on failure"
        );
    }

    /// **The net-rate cache across two ticks, with a counter wrap.** A second
    /// tick whose byte counter is *lower* than the first — an interface reset
    /// or a wrapped 32-bit counter surfaced through `/proc/net/dev` — must not
    /// underflow into a huge bogus rate (`u64::MAX`-scale). The rate
    /// computation saturates the delta to 0 instead.
    ///
    /// Falsification: swap `saturating_sub` for plain `-` in `apply_network`
    /// and this either panics (debug) or reds with an astronomical rate
    /// (release).
    #[allow(clippy::float_cmp)]
    #[test]
    fn apply_network_counter_wrap_saturates_the_rate_to_zero() {
        let mut state = PollState::new();
        let writer = Mutable::new(NetIo::default());
        let t0 = Instant::now();

        // Tick 1: establish a baseline.
        apply_network(
            &mut state,
            Some(vec![("eth0".to_string(), 10_000, 20_000)]),
            t0,
            &writer,
        );

        // Tick 2, 1 s later: the counter is now *lower* than tick 1's (a
        // reset/wrap), not higher.
        let t1 = t0 + Duration::from_secs(1);
        apply_network(
            &mut state,
            Some(vec![("eth0".to_string(), 500, 900)]),
            t1,
            &writer,
        );

        let got = writer.get_cloned();
        assert_eq!(got.interfaces.len(), 1);
        assert_eq!(
            got.interfaces[0].rx_rate_bps, 0.0,
            "a counter that went backwards must saturate to a 0 rate, not underflow"
        );
        assert_eq!(
            got.interfaces[0].tx_rate_bps, 0.0,
            "a counter that went backwards must saturate to a 0 rate, not underflow"
        );
        // The raw totals still reflect exactly what this tick read, wrap and
        // all — only the *rate* is protected, not the counter itself.
        assert_eq!(got.interfaces[0].rx_bytes_total, 500);
        assert_eq!(got.interfaces[0].tx_bytes_total, 900);
    }

    /// **The rate formula itself, pinned to literals.** The two tests above
    /// only ever assert a *zero* rate (no prior sample; a wrapped counter), so
    /// neither exercises the divisor, the unit, or which delta lands in which
    /// field. A rate whose unit is asserted nowhere is exactly the #1026
    /// shape: `rx_rate_bps`/`tx_rate_bps` are documented **bytes/sec** at the
    /// `NetInterface` declaration, and nothing else in the tree says so.
    ///
    /// 20 000 rx bytes and 40 000 tx bytes over a 2 s gap ⇒ 10 000 B/s and
    /// 20 000 B/s. The two numbers are deliberately different so a rx/tx swap
    /// cannot pass, and the gap is deliberately not 1 s so the `/ dt` divisor
    /// is load-bearing.
    ///
    /// Falsification: `… / dt * 8.0` (a bytes→bits unit error) reds both rate
    /// assertions; swapping the `rx_r`/`tx_r` assignment reds them too.
    #[allow(clippy::float_cmp)]
    #[test]
    fn apply_network_rate_is_bytes_per_second_over_the_elapsed_gap() {
        let mut state = PollState::new();
        let writer = Mutable::new(NetIo::default());
        let t0 = Instant::now();

        // Tick 1: the baseline. No prior sample, so no rate yet.
        apply_network(
            &mut state,
            Some(vec![("eth0".to_string(), 10_000, 20_000)]),
            t0,
            &writer,
        );

        // Tick 2, exactly 2 s later: +20 000 rx, +40 000 tx.
        let t1 = t0 + Duration::from_secs(2);
        apply_network(
            &mut state,
            Some(vec![("eth0".to_string(), 30_000, 60_000)]),
            t1,
            &writer,
        );

        let got = writer.get_cloned();
        assert_eq!(got.interfaces.len(), 1);
        assert_eq!(
            got.interfaces[0].rx_rate_bps, 10_000.0,
            "20000 rx bytes over 2 s is 10000 bytes/sec"
        );
        assert_eq!(
            got.interfaces[0].tx_rate_bps, 20_000.0,
            "40000 tx bytes over 2 s is 20000 bytes/sec"
        );
        // The totals are the raw counters, not the deltas.
        assert_eq!(got.interfaces[0].rx_bytes_total, 30_000);
        assert_eq!(got.interfaces[0].tx_bytes_total, 60_000);
    }

    /// **A vanished interface is pruned from the rate cache.** `apply_network`
    /// rebuilds `net_prev` from scratch every tick rather than `insert`ing into
    /// the existing map, which is the only thing that drops an interface that
    /// went away (a USB tether unplugged, a VPN `tun0` torn down). Nothing
    /// asserted that, so swapping the rebuild for an in-place `insert` — a
    /// plausible "avoid the allocation" optimisation — would leak an entry per
    /// vanished interface for the life of the process and red nothing.
    ///
    /// Falsification: replace `state.net_prev = next_net_prev;` with a loop
    /// that inserts into `state.net_prev` and this reds on `len() == 1`.
    #[test]
    fn apply_network_prunes_an_interface_that_disappeared() {
        let mut state = PollState::new();
        let writer = Mutable::new(NetIo::default());
        let t0 = Instant::now();

        // Tick 1: two interfaces.
        apply_network(
            &mut state,
            Some(vec![
                ("eth0".to_string(), 1_000, 2_000),
                ("tun0".to_string(), 10, 20),
            ]),
            t0,
            &writer,
        );
        assert_eq!(state.net_prev.len(), 2, "both interfaces cached");
        assert_eq!(writer.get_cloned().interfaces.len(), 2);

        // Tick 2: `tun0` is gone from /proc/net/dev.
        let t1 = t0 + Duration::from_secs(1);
        apply_network(
            &mut state,
            Some(vec![("eth0".to_string(), 2_000, 4_000)]),
            t1,
            &writer,
        );
        assert_eq!(
            state.net_prev.len(),
            1,
            "an interface absent from this tick must be pruned from the cache"
        );
        assert!(state.net_prev.contains_key("eth0"));
        assert!(!state.net_prev.contains_key("tun0"));
        assert_eq!(writer.get_cloned().interfaces.len(), 1);
    }

    // ── apply_disk_io: table test ─────────────────────────────────────────────

    #[allow(clippy::float_cmp)]
    #[test]
    fn apply_disk_io_table() {
        let mut prev: HashMap<String, (u64, u64, Instant)> = HashMap::new();
        let writer = Mutable::new(DiskIo::default());
        let t0 = Instant::now();

        apply_disk_io(
            &mut prev,
            Some(vec![("sda".to_string(), 1_000, 500)]),
            t0,
            &writer,
        );
        let got = writer.get_cloned();
        assert_eq!(got.total_read_bytes, 1_000);
        assert_eq!(got.total_write_bytes, 500);
        assert_eq!(got.read_bps, 0.0, "no prior sample yet");
        assert!(prev.contains_key("sda"));

        // A read failure warns (not asserted here) and leaves `prev` and the
        // writer untouched — both halves asserted, not just the writer.
        apply_disk_io(&mut prev, None, t0, &writer);
        assert_eq!(writer.get_cloned().total_read_bytes, 1_000);
        assert_eq!(
            prev.len(),
            1,
            "a failed read must not clear the rolling rate cache"
        );
        assert!(prev.contains_key("sda"), "the cached device survives");
    }

    // ── apply_cpu_temp: unconditional passthrough ────────────────────────────

    #[test]
    fn apply_cpu_temp_publishes_verbatim() {
        let writer = Mutable::new(CpuTemp::default());
        apply_cpu_temp(
            CpuTemp {
                package_celsius: Some(55.5),
            },
            &writer,
        );
        assert_eq!(writer.get_cloned().package_celsius, Some(55.5));
    }

    // ── Tick-gated publishers: apply_gpu / apply_disk / apply_conn_counts ────
    //
    // Each of these only publishes on its own tick cadence (`poll_tick` calls
    // them every tick, but with `None`/`gpu_tick: false` off-cadence) — the
    // one behaviour all three share and the one a table test alone would miss
    // if the "off-tick" row were left out.

    #[test]
    fn apply_gpu_table() {
        let writer = Mutable::new(Some(GpuState {
            vendor: GpuVendor::Nvidia,
            name: "sentinel".to_string(),
            ..Default::default()
        }));

        // Off-tick: the writer is left exactly as it was.
        apply_gpu(
            false,
            Some(GpuState {
                vendor: GpuVendor::Amd,
                ..Default::default()
            }),
            &writer,
        );
        assert_eq!(writer.get_cloned().unwrap().name, "sentinel");

        // On-tick with hardware found: publishes it.
        apply_gpu(
            true,
            Some(GpuState {
                vendor: GpuVendor::Amd,
                name: "found".to_string(),
                ..Default::default()
            }),
            &writer,
        );
        assert_eq!(writer.get_cloned().unwrap().name, "found");

        // On-tick with no hardware found: publishes `None` (clears it) —
        // distinct from "not this tick".
        apply_gpu(true, None, &writer);
        assert!(writer.get_cloned().is_none());
    }

    #[test]
    fn apply_disk_table() {
        let writer = Mutable::new(DiskUsage {
            mounts: vec![DiskMount {
                path: "/sentinel".to_string(),
                total_bytes: 1,
                used_bytes: 1,
                free_bytes: 0,
                usage: 1.0,
            }],
        });

        // Off-tick (`None`): untouched.
        apply_disk(None, &writer);
        assert_eq!(writer.get_cloned().mounts[0].path, "/sentinel");

        // On-tick: publishes the fresh snapshot.
        apply_disk(
            Some(DiskUsage {
                mounts: vec![DiskMount {
                    path: "/".to_string(),
                    total_bytes: 100,
                    used_bytes: 50,
                    free_bytes: 50,
                    usage: 0.5,
                }],
            }),
            &writer,
        );
        assert_eq!(writer.get_cloned().mounts[0].path, "/");
    }

    #[test]
    fn apply_conn_counts_table() {
        let writer = Mutable::new(NetConnections {
            tcp_established: 999,
            ..Default::default()
        });

        // Off-tick (`None`): untouched.
        apply_conn_counts(None, &writer);
        assert_eq!(writer.get_cloned().tcp_established, 999);

        // On-tick: publishes the fresh snapshot.
        apply_conn_counts(
            Some(NetConnections {
                tcp_established: 7,
                tcp_listen: 3,
                ..Default::default()
            }),
            &writer,
        );
        let got = writer.get_cloned();
        assert_eq!(got.tcp_established, 7);
        assert_eq!(got.tcp_listen, 3);
    }
}
