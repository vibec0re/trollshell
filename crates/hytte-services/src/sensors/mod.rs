//! Sensors service — polls `/proc/stat`, `/sys/.../cpufreq`, `/proc/meminfo`,
//! `/proc/net/dev`, `/proc/diskstats`, `/sys/class/hwmon`, `/sys/class/drm`,
//! and optional `nvidia-smi` every second and exposes CPU load/clock/temp,
//! memory usage, network I/O rates, disk I/O throughput, GPU stats, and disk
//! usage as `futures-signals` signals.
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

mod cpufreq;
mod disk;
mod diskio;
mod gpu;
mod hwmon;
mod meminfo;
mod net;
mod proc_stat;

use futures_signals::signal::{Mutable, Signal, SignalExt};
use futures_util::StreamExt;
use hytte_reactive::{Service, registry};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cast::u64_to_f64_bytes;

use cpufreq::read_cpu_freq;
use disk::{read_disk_for_specs, read_mountlist, read_process_count};
use diskio::{compute_disk_io, read_proc_diskstats};
use gpu::{GpuCache, read_gpu_with_cache};
use hwmon::read_cpu_temp;
use meminfo::read_proc_meminfo;
use net::{read_net_connections, read_proc_net_dev};
use proc_stat::{compute_cpu_load, read_proc_stat};

// ── Blocking-read bundle ───────────────────────────────────────────────────────

/// All data collected by a single blocking-I/O sweep.
///
/// Constructed inside `tokio::task::spawn_blocking` and returned to the async
/// poll loop so that no blocking syscall runs directly on a tokio worker thread.
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

// ── Public data shapes ────────────────────────────────────────────────────────

/// Per-CPU load snapshot.
#[derive(Clone, Debug, Default)]
pub struct CpuLoad {
    /// Overall load, `0.0..=1.0`.
    pub overall: f64,
    /// Per-logical-core load. Length matches the kernel's CPU count.
    pub per_core: Vec<f64>,
}

/// Per-core CPU clock (cpufreq) snapshot, all frequencies in **Hz**.
///
/// Sourced from `/sys/devices/system/cpu/cpu*/cpufreq`. When no cpufreq
/// governor is present (many VMs), all fields are default (empty `per_core`,
/// zeroed frequencies) so a consumer can self-hide.
#[derive(Clone, Debug, Default)]
pub struct CpuFreq {
    /// Aggregate current frequency = the **maximum** current frequency across
    /// cores, in Hz.
    pub max_hz: f64,
    /// Per-logical-core current frequency, in Hz. Length matches the number of
    /// cores exposing a `cpufreq` node.
    pub per_core: Vec<f64>,
    /// Highest `cpuinfo_max_freq` across cores, in Hz — the fixed normalization
    /// ceiling for a 0→max axis.
    pub max_ceiling_hz: f64,
}

/// Memory usage snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct Memory {
    /// Bytes.
    pub total: u64,
    pub free: u64,
    pub available: u64,
    /// Convenience: `total - available`.
    pub used: u64,
    pub swap_used: u64,
    pub swap_total: u64,
}

/// Network I/O snapshot — all interfaces.
#[derive(Clone, Debug, Default)]
pub struct NetIo {
    pub interfaces: Vec<NetInterface>,
}

/// Per-interface network I/O snapshot.
#[derive(Clone, Debug)]
pub struct NetInterface {
    pub name: String,
    pub rx_bytes_total: u64,
    pub tx_bytes_total: u64,
    /// Rate since the previous sample (bytes/sec).
    pub rx_rate_bps: f64,
    pub tx_rate_bps: f64,
}

/// Disk I/O throughput snapshot — aggregate across all physical whole-disk
/// block devices (the soft default; mirrors the network row's rx+tx aggregate).
#[derive(Clone, Copy, Debug, Default)]
pub struct DiskIo {
    /// Aggregate read rate across physical disks (bytes/sec).
    pub read_bps: f64,
    /// Aggregate write rate across physical disks (bytes/sec).
    pub write_bps: f64,
    /// Cumulative bytes **read since boot**, summed across physical disks.
    pub total_read_bytes: u64,
    /// Cumulative bytes **written since boot**, summed across physical disks.
    pub total_write_bytes: u64,
}

/// CPU package temperature.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuTemp {
    /// Package temperature in degrees Celsius. None if no sensor found.
    pub package_celsius: Option<f64>,
}

/// GPU vendor.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GpuVendor {
    #[default]
    Unknown,
    Amd,
    Intel,
    Nvidia,
}

/// GPU state snapshot.
#[derive(Clone, Debug, Default)]
pub struct GpuState {
    pub vendor: GpuVendor,
    /// Free-form name (e.g. "NVIDIA GeForce RTX 3080" or "AMD Radeon RX 6800").
    #[allow(clippy::doc_markdown)]
    pub name: String,
    pub temperature_celsius: Option<f64>,
    /// 0.0..=1.0
    pub load: Option<f64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
}

/// Disk usage for all tracked mount points.
#[derive(Clone, Debug, Default)]
pub struct DiskUsage {
    pub mounts: Vec<DiskMount>,
}

/// Per-mount-point disk usage.
#[derive(Clone, Debug)]
pub struct DiskMount {
    pub path: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    /// 0.0..=1.0
    pub usage: f64,
}

/// TCP socket-state counts from `/proc/net/{tcp,tcp6}`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NetConnections {
    /// IPv4 connections in the ESTABLISHED state.
    pub tcp_established: u32,
    /// IPv4 sockets in the LISTEN state.
    pub tcp_listen: u32,
    /// IPv6 connections in the ESTABLISHED state.
    pub tcp6_established: u32,
    /// IPv6 sockets in the LISTEN state.
    pub tcp6_listen: u32,
}

impl NetConnections {
    /// Sum of IPv4 + IPv6 ESTABLISHED.
    #[must_use]
    pub fn established_total(&self) -> u32 {
        self.tcp_established + self.tcp6_established
    }
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
    /// `mount_watch_loop`; consumed by `poll_loop`'s disk branch.
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
        }
    }
}

/// Spawn a task that accumulates a `HISTORY_CAP`-sample ring for one sparkline:
/// it subscribes to `source`, and for each emit where `extract` yields a sample
/// pushes it and republishes the whole window into `sink`. `extract` returning
/// `None` (e.g. a GPU field that's absent this tick) leaves the ring unchanged —
/// mirroring the old per-widget "only push when present" behaviour.
fn spawn_history<T, F>(
    rt: &tokio::runtime::Handle,
    source: Mutable<T>,
    sink: Mutable<Arc<VecDeque<f64>>>,
    extract: F,
) where
    T: Clone + Send + Sync + 'static,
    F: Fn(&T) -> Option<f64> + Send + 'static,
{
    rt.spawn(async move {
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
    });
}

// ── Service marker ────────────────────────────────────────────────────────────

/// The sensors service marker type — pass to `App::with`.
pub struct SensorsService;

impl Service for SensorsService {
    type Handles = SensorsHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
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

        rt.spawn(async move {
            poll_loop(PollWriters {
                cpu: cpu_writer,
                cpu_freq: cpu_freq_writer,
                mem: mem_writer,
                net: net_writer,
                disk_io: disk_io_writer,
                cpu_temp: cpu_temp_writer,
                gpu: gpu_writer,
                disk: disk_writer,
                net_conn: net_conn_writer,
                proc_count: proc_count_writer,
                mount_list: mount_list_for_poll,
            })
            .await;
        });
        rt.spawn(mount_watch_loop(mount_list_for_watch));

        // Sparkline history accumulators (#231): one ring per Stats graph, fed
        // off the live metric signals — extractors mirror each row's old
        // `spark.push(..)` scalar and its present/absent guard.
        spawn_history(rt, handles.cpu.clone(), handles.cpu_hist.clone(), |c| {
            Some(c.overall)
        });
        spawn_history(rt, handles.memory.clone(), handles.mem_hist.clone(), |m| {
            Some(if m.total == 0 {
                0.0
            } else {
                (u64_to_f64_bytes(m.used) / u64_to_f64_bytes(m.total)).clamp(0.0, 1.0)
            })
        });
        spawn_history(
            rt,
            handles.disk_io.clone(),
            handles.disk_io_hist.clone(),
            |io| Some(io.read_bps + io.write_bps),
        );
        spawn_history(
            rt,
            handles.gpu.clone(),
            handles.gpu_load_hist.clone(),
            |g| g.as_ref().and_then(|s| s.load).map(|l| l * 100.0),
        );
        spawn_history(
            rt,
            handles.gpu.clone(),
            handles.gpu_vram_hist.clone(),
            |g| {
                g.as_ref()
                    .and_then(|s| s.memory_used_bytes.zip(s.memory_total_bytes))
                    .map(|(used, total)| {
                        if total == 0 {
                            0.0
                        } else {
                            (u64_to_f64_bytes(used) / u64_to_f64_bytes(total) * 100.0)
                                .clamp(0.0, 100.0)
                        }
                    })
            },
        );
        spawn_history(
            rt,
            handles.gpu.clone(),
            handles.gpu_temp_hist.clone(),
            |g| g.as_ref().and_then(|s| s.temperature_celsius),
        );

        handles
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

/// CPU-load history (fraction 0..=1), `HISTORY_CAP` samples. See [`history_of`].
pub fn cpu_history() -> impl Signal<Item = Arc<VecDeque<f64>>> {
    history_of(|h| &h.cpu_hist)
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

async fn poll_loop(w: PollWriters) {
    let cpu_writer = w.cpu;
    let cpu_freq_writer = w.cpu_freq;
    let mem_writer = w.mem;
    let net_writer = w.net;
    let disk_io_writer = w.disk_io;
    let cpu_temp_writer = w.cpu_temp;
    let gpu_writer = w.gpu;
    let disk_writer = w.disk;
    let net_conn_writer = w.net_conn;
    let proc_count_writer = w.proc_count;
    let mount_list_reader = w.mount_list;
    let mut state = PollState::new();

    loop {
        let now = Instant::now();

        // Snapshot tick-local flags before moving `state` fields into the closure.
        let tick = state.tick;
        let do_gpu = tick.is_multiple_of(2);
        let do_net_conn = tick.is_multiple_of(2);
        let do_disk = tick.is_multiple_of(5);

        // Take the chip-dir cache out of state so the closure can own it.
        let chip = state.cpu_temp_chip.take();
        // Move the GPU cache out of state so the closure can own it.
        // On non-GPU ticks we put it back unchanged; on GPU ticks we replace it
        // with the updated cache returned by `read_gpu_with_cache`.
        let gpu_cache = std::mem::take(&mut state.gpu_cache);

        // Mount list is cloned here (cheap — it's rarely non-empty).
        let specs = if do_disk {
            mount_list_reader.get_cloned()
        } else {
            Vec::new()
        };

        // ── All blocking I/O runs on a dedicated blocking thread ──────────
        let data = tokio::task::spawn_blocking(move || {
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
            let (gpu_state, gpu_tick, new_gpu_cache) = if do_gpu {
                let (state, cache) = read_gpu_with_cache(gpu_cache);
                (state, true, Some(cache))
            } else {
                (None, false, Some(gpu_cache))
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
                    gpu_tick,
                    net_conn,
                    proc_count,
                    disk,
                },
                new_gpu_cache,
            )
        })
        .await;

        let Ok((data, new_gpu_cache)) = data else {
            tracing::warn!("sensors: blocking I/O task panicked; skipping tick");
            state.tick = state.tick.wrapping_add(1);
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };

        // Thread the chip cache back.
        state.cpu_temp_chip = data.cpu_temp_chip;

        // Thread the GPU cache back (always returned, even on non-GPU ticks).
        if let Some(cache) = new_gpu_cache {
            state.gpu_cache = cache;
        }

        apply_cpu_load(&mut state.cpu_prev, data.cpu_stat, &cpu_writer);
        apply_cpu_freq(data.cpu_freq, &cpu_freq_writer);
        apply_memory(data.mem, &mem_writer);
        apply_network(&mut state.net_prev, data.net_dev, now, &net_writer);
        apply_disk_io(&mut state.disk_io_prev, data.disk_io, now, &disk_io_writer);
        apply_cpu_temp(data.cpu_temp, &cpu_temp_writer);
        apply_gpu(data.gpu_tick, data.gpu_state, &gpu_writer);
        apply_disk(data.disk, &disk_writer);
        apply_conn_counts(data.net_conn, &net_conn_writer);
        proc_count_writer.set(data.proc_count);

        state.tick = state.tick.wrapping_add(1);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ── Per-concern publish helpers ───────────────────────────────────────────────

/// Compute and publish CPU load; update the rolling `cpu_prev` cache.
fn apply_cpu_load(
    cpu_prev: &mut Vec<(u64, u64)>,
    cpu_stat: Option<Vec<(u64, u64)>>,
    writer: &Mutable<CpuLoad>,
) {
    match cpu_stat {
        Some(cpu_now) => {
            let load = compute_cpu_load(cpu_prev, &cpu_now);
            *cpu_prev = cpu_now;
            writer.set(load);
        }
        None => {
            tracing::warn!("sensors: failed to read /proc/stat");
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
fn apply_memory(mem: Option<Memory>, writer: &Mutable<Memory>) {
    match mem {
        Some(mem) => {
            writer.set(mem);
        }
        None => {
            tracing::warn!("sensors: failed to read /proc/meminfo");
        }
    }
}

/// Compute per-interface byte rates from the new `/proc/net/dev` snapshot,
/// update the rolling `net_prev` cache, and publish the `NetIo` snapshot.
fn apply_network(
    net_prev: &mut HashMap<String, (u64, u64, Instant)>,
    net_dev: Option<Vec<(String, u64, u64)>>,
    now: Instant,
    writer: &Mutable<NetIo>,
) {
    match net_dev {
        Some(net_now) => {
            let mut interfaces = Vec::new();
            let mut next_net_prev = HashMap::new();

            for (name, rx, tx) in net_now {
                let (rx_rate, tx_rate) = match net_prev.get(&name) {
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

            *net_prev = next_net_prev;
            writer.set(NetIo { interfaces });
        }
        None => {
            tracing::warn!("sensors: failed to read /proc/net/dev");
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

// ── Internal type ─────────────────────────────────────────────────────────────

/// Internal representation of one mounted filesystem.
///
/// Not part of the public sensors API; consumed only by the disk poller.
#[derive(Clone, Debug)]
pub(crate) struct MountSpec {
    /// Mount point (mountinfo field 5), with octal escapes decoded.
    pub(crate) path: String,
    /// `(major, minor)` from mountinfo field 3 — used for dedup.
    pub(crate) dev_id: (u32, u32),
    /// fstype (right-half token 1) — diagnostic only.
    pub(crate) fstype: String,
}
