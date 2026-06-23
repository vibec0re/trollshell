//! Sensors service — polls `/proc/stat`, `/proc/meminfo`, `/proc/net/dev`,
//! `/sys/class/hwmon`, `/sys/class/drm`, and optional `nvidia-smi` every
//! second and exposes CPU load/temp, memory usage, network I/O rates, GPU
//! stats, and disk usage as `futures-signals` signals.
//!
//! # Public API
//!
//! ```ignore
//! // Register once at startup:
//! .with(sensors::service())
//!
//! // Subscribe in widgets:
//! sensors::cpu()      -> impl Signal<Item = CpuLoad>
//! sensors::memory()   -> impl Signal<Item = Memory>
//! sensors::network()  -> impl Signal<Item = NetIo>
//! sensors::cpu_temp() -> impl Signal<Item = CpuTemp>
//! sensors::gpu()      -> impl Signal<Item = Option<GpuState>>
//! sensors::disk()     -> impl Signal<Item = DiskUsage>
//! ```

mod disk;
mod gpu;
mod hwmon;
mod meminfo;
mod net;
mod proc_stat;

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::cast::u64_to_f64_bytes;

use disk::{read_disk_for_specs, read_mountlist, read_process_count};
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
    /// Parsed `/proc/meminfo`, or `None` on read error.
    mem: Option<Memory>,
    /// Parsed `/proc/net/dev`, or `None` on read error.
    net_dev: Option<Vec<(String, u64, u64)>>,
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

// ── Service marker ────────────────────────────────────────────────────────────

/// The sensors service marker type — pass to `App::with`.
pub struct SensorsService;

impl Service for SensorsService {
    type Handles = SensorsHandles;

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
            poll_loop(PollWriters {
                cpu: cpu_writer,
                mem: mem_writer,
                net: net_writer,
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

/// Signal that emits the current memory usage.
pub fn memory() -> impl Signal<Item = Memory> {
    registry::with(|r| {
        r.get::<SensorsHandles>()
            .expect("sensors::service() not registered")
            .memory
            .signal()
    })
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
    mem: Mutable<Memory>,
    net: Mutable<NetIo>,
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
    let mem_writer = w.mem;
    let net_writer = w.net;
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
            // Memory
            let mem = read_proc_meminfo().ok();
            // Network I/O
            let net_dev = read_proc_net_dev().ok();
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
                    mem,
                    net_dev,
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
        apply_memory(data.mem, &mem_writer);
        apply_network(&mut state.net_prev, data.net_dev, now, &net_writer);
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
