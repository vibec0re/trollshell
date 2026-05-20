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

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use std::collections::HashMap;
use std::time::{Duration, Instant};

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
    /// Tick counter for rate-limiting slower polls.
    tick: u64,
}

impl PollState {
    fn new() -> Self {
        Self {
            cpu_prev: Vec::new(),
            net_prev: HashMap::new(),
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

        // ── CPU ───────────────────────────────────────────────────────────────
        match read_proc_stat() {
            Ok(cpu_now) => {
                let load = compute_cpu_load(&state.cpu_prev, &cpu_now);
                state.cpu_prev = cpu_now;
                cpu_writer.set(load);
            }
            Err(e) => {
                tracing::warn!(error = %e, "sensors: failed to read /proc/stat");
            }
        }

        // ── Memory ────────────────────────────────────────────────────────────
        match read_proc_meminfo() {
            Ok(mem) => {
                mem_writer.set(mem);
            }
            Err(e) => {
                tracing::warn!(error = %e, "sensors: failed to read /proc/meminfo");
            }
        }

        // ── Network ───────────────────────────────────────────────────────────
        match read_proc_net_dev() {
            Ok(net_now) => {
                let mut interfaces = Vec::new();
                let mut next_net_prev = HashMap::new();

                for (name, rx, tx) in net_now {
                    let (rx_rate, tx_rate) = match state.net_prev.get(&name) {
                        Some((prev_rx, prev_tx, prev_when)) => {
                            let dt = now
                                .duration_since(*prev_when)
                                .as_secs_f64()
                                .max(0.1);
                            #[allow(clippy::cast_precision_loss)]
                            let rx_r = (rx.saturating_sub(*prev_rx) as f64) / dt;
                            #[allow(clippy::cast_precision_loss)]
                            let tx_r = (tx.saturating_sub(*prev_tx) as f64) / dt;
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
                net_writer.set(NetIo { interfaces });
            }
            Err(e) => {
                tracing::warn!(error = %e, "sensors: failed to read /proc/net/dev");
            }
        }

        // ── CPU temp (every tick — cheap sysfs reads) ─────────────────────
        cpu_temp_writer.set(read_cpu_temp());

        // ── GPU (every 2 ticks) ───────────────────────────────────────────
        if state.tick.is_multiple_of(2) {
            gpu_writer.set(read_gpu());
        }

        // ── Disk (every 5 ticks) ──────────────────────────────────────────
        if state.tick.is_multiple_of(5) {
            let specs = mount_list_reader.get_cloned();
            disk_writer.set(read_disk_for_specs(&specs));
        }

        // ── TCP socket counts (every 2 ticks) ─────────────────────────────
        if state.tick.is_multiple_of(2) {
            net_conn_writer.set(read_net_connections());
        }

        // ── Process count (every tick) ────────────────────────────────────
        proc_count_writer.set(read_process_count());

        state.tick = state.tick.wrapping_add(1);
        tokio::time::sleep(Duration::from_secs(1)).await;
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

// ── /proc/net/{tcp,tcp6} parsing ─────────────────────────────────────────────

fn read_net_connections() -> NetConnections {
    let v4 = count_tcp_states("/proc/net/tcp");
    let v6 = count_tcp_states("/proc/net/tcp6");
    NetConnections {
        tcp_established: v4.0,
        tcp_listen: v4.1,
        tcp6_established: v6.0,
        tcp6_listen: v6.1,
    }
}

/// Returns `(established, listen)` counts. /proc/net/tcp* state column is the
/// 4th whitespace-separated field, encoded as 2-char hex. 01 = ESTABLISHED,
/// 0A = LISTEN.
fn count_tcp_states(path: &str) -> (u32, u32) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (0, 0);
    };
    let mut established = 0u32;
    let mut listen = 0u32;
    for line in text.lines().skip(1) {
        let Some(state) = line.split_ascii_whitespace().nth(3) else { continue };
        match state {
            "01" => established += 1,
            "0A" => listen += 1,
            _ => {}
        }
    }
    (established, listen)
}

// ── /proc/stat parsing ────────────────────────────────────────────────────────

/// Returns one entry per `cpu*` line: `(active_jiffies, total_jiffies)`.
/// Index 0 = aggregate `cpu` line, 1+ = `cpu0`, `cpu1`, …
fn read_proc_stat() -> Result<Vec<(u64, u64)>, std::io::Error> {
    let text = std::fs::read_to_string("/proc/stat")?;
    let mut entries = Vec::new();

    for line in text.lines() {
        if !line.starts_with("cpu") {
            // cpu lines are at the top; once we see something else we're done.
            break;
        }
        let mut fields = line.split_ascii_whitespace();
        let _label = fields.next(); // "cpu" or "cpu0", etc.
        let nums: Vec<u64> = fields
            .map(|f| f.parse::<u64>().unwrap_or(0))
            .collect();
        if nums.is_empty() {
            continue;
        }
        // field layout after the label: user nice system idle iowait …
        // nums[0]=user, nums[1]=nice, nums[2]=system, nums[3]=idle, nums[4]=iowait
        let total: u64 = nums.iter().sum();
        let idle_jiffies = nums.get(3).copied().unwrap_or(0)
            + nums.get(4).copied().unwrap_or(0);
        let active = total.saturating_sub(idle_jiffies);
        entries.push((active, total));
    }

    Ok(entries)
}

fn compute_cpu_load(prev: &[(u64, u64)], now: &[(u64, u64)]) -> CpuLoad {
    if prev.is_empty() || now.is_empty() {
        // First sample — no delta yet.
        return CpuLoad {
            overall: 0.0,
            per_core: vec![0.0; now.len().saturating_sub(1)],
        };
    }

    let load_for = |i: usize| -> f64 {
        let Some(&(prev_active, prev_total)) = prev.get(i) else {
            return 0.0;
        };
        let Some(&(now_active, now_total)) = now.get(i) else {
            return 0.0;
        };
        let d_total = now_total.saturating_sub(prev_total);
        let d_active = now_active.saturating_sub(prev_active);
        if d_total == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let load = d_active as f64 / d_total as f64;
        load.clamp(0.0, 1.0)
    };

    let overall = load_for(0);
    let core_count = now.len().saturating_sub(1);
    let per_core = (0..core_count).map(|i| load_for(i + 1)).collect();

    CpuLoad { overall, per_core }
}

// ── /proc/meminfo parsing ─────────────────────────────────────────────────────

fn read_proc_meminfo() -> Result<Memory, std::io::Error> {
    let text = std::fs::read_to_string("/proc/meminfo")?;
    Ok(parse_meminfo(&text))
}

fn parse_meminfo(text: &str) -> Memory {
    let mut total_kb: u64 = 0;
    let mut free_kb: u64 = 0;
    let mut available_kb: u64 = 0;
    let mut swap_total_kb: u64 = 0;
    let mut swap_free_kb: u64 = 0;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemFree:") {
            free_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("SwapTotal:") {
            swap_total_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("SwapFree:") {
            swap_free_kb = parse_kb(rest);
        }
    }

    let total = total_kb * 1024;
    let free = free_kb * 1024;
    let available = available_kb * 1024;
    let used = total.saturating_sub(available);
    let swap_total = swap_total_kb * 1024;
    let swap_free = swap_free_kb * 1024;
    let swap_used = swap_total.saturating_sub(swap_free);

    Memory {
        total,
        free,
        available,
        used,
        swap_used,
        swap_total,
    }
}

/// Parse a `/proc/meminfo` value field like `"  16331836 kB"` → `16331836`.
fn parse_kb(s: &str) -> u64 {
    s.split_ascii_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

// ── /proc/net/dev parsing ─────────────────────────────────────────────────────

/// Returns `(name, rx_bytes, tx_bytes)` for every interface.
fn read_proc_net_dev() -> Result<Vec<(String, u64, u64)>, std::io::Error> {
    let text = std::fs::read_to_string("/proc/net/dev")?;
    let mut result = Vec::new();

    for line in text.lines().skip(2) {
        // Each line: "  eth0: 123 456 ..."
        // Split on '|' is unreliable; split on whitespace after stripping the
        // interface name (which may contain spaces in theory, but not on Linux).
        let line = line.trim();
        let Some(colon_pos) = line.find(':') else {
            continue;
        };
        let name = line[..colon_pos].trim().to_string();
        let rest = &line[colon_pos + 1..];

        let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
        // field layout (0-indexed after the colon):
        // rx: [0]=bytes [1]=packets [2]=errs [3]=drop [4]=fifo [5]=frame [6]=compressed [7]=multicast
        // tx: [8]=bytes [9]=packets ...
        let rx_bytes: u64 = fields.first().and_then(|v| v.parse().ok()).unwrap_or(0);
        let tx_bytes: u64 = fields.get(8).and_then(|v| v.parse().ok()).unwrap_or(0);

        result.push((name, rx_bytes, tx_bytes));
    }

    Ok(result)
}

// ── CPU temperature ───────────────────────────────────────────────────────────

fn read_cpu_temp() -> CpuTemp {
    use std::fs;
    let Ok(hwmon) = fs::read_dir("/sys/class/hwmon") else {
        return CpuTemp::default();
    };

    let preferred_names: &[&str] = &["coretemp", "k10temp", "zenpower", "asusec"];

    for entry in hwmon.flatten() {
        let name_path = entry.path().join("name");
        let Ok(name) = fs::read_to_string(&name_path) else {
            continue;
        };
        let name = name.trim();
        if !preferred_names.contains(&name) {
            continue;
        }
        // Found a CPU sensor chip. Read every temp*_input file, take the max.
        let mut max_milli: Option<u64> = None;
        if let Ok(rd) = fs::read_dir(entry.path()) {
            for f in rd.flatten() {
                let fname = f.file_name();
                let Some(name_str) = fname.to_str() else {
                    continue;
                };
                if !name_str.starts_with("temp") || !name_str.ends_with("_input") {
                    continue;
                }
                if let Ok(s) = fs::read_to_string(f.path())
                    && let Ok(v) = s.trim().parse::<u64>()
                {
                    max_milli = Some(max_milli.map_or(v, |cur| cur.max(v)));
                }
            }
        }
        if let Some(m) = max_milli {
            #[allow(clippy::cast_precision_loss)]
            return CpuTemp { package_celsius: Some(m as f64 / 1000.0) };
        }
    }
    CpuTemp::default()
}

// ── GPU ───────────────────────────────────────────────────────────────────────

fn read_amd_gpu() -> Option<GpuState> {
    use std::fs;
    let drm = fs::read_dir("/sys/class/drm").ok()?;
    for entry in drm.flatten() {
        let entry_name = entry.file_name();
        let Some(name) = entry_name.to_str() else {
            continue;
        };
        // Only top-level cards (cardN), not connectors (cardN-...)
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let device = entry.path().join("device");
        let Ok(vendor) = fs::read_to_string(device.join("vendor")) else {
            continue;
        };
        if vendor.trim() != "0x1002" {
            continue;
        }

        #[allow(clippy::cast_precision_loss)]
        let load = fs::read_to_string(device.join("gpu_busy_percent"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|v| v as f64 / 100.0);

        let memory_used_bytes = fs::read_to_string(device.join("mem_info_vram_used"))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        let memory_total_bytes = fs::read_to_string(device.join("mem_info_vram_total"))
            .ok()
            .and_then(|s| s.trim().parse().ok());

        // Temperature: walk device/hwmon/hwmonN/temp1_input
        let mut temperature_celsius = None;
        if let Ok(hwmon) = fs::read_dir(device.join("hwmon")) {
            for h in hwmon.flatten() {
                let temp = h.path().join("temp1_input");
                if let Ok(s) = fs::read_to_string(&temp)
                    && let Ok(v) = s.trim().parse::<u64>()
                {
                    #[allow(clippy::cast_precision_loss)]
                    let celsius = v as f64 / 1000.0;
                    temperature_celsius = Some(celsius);
                    break;
                }
            }
        }

        // Name from /sys/class/drm/cardN/device/uevent or just hardcode "AMD GPU"
        let gpu_name = fs::read_to_string(device.join("uevent"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("DRIVER=").map(|d| format!("AMD ({d})")))
            })
            .unwrap_or_else(|| "AMD GPU".to_string());

        return Some(GpuState {
            vendor: GpuVendor::Amd,
            name: gpu_name,
            temperature_celsius,
            load,
            memory_used_bytes,
            memory_total_bytes,
        });
    }
    None
}

fn read_nvidia_gpu() -> Option<GpuState> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,temperature.gpu,utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = std::str::from_utf8(&out.stdout).ok()?;
    let line = s.lines().next()?;
    let parts: Vec<&str> = line.split(',').map(str::trim).collect();
    if parts.len() < 5 {
        return None;
    }
    let gpu_name = parts[0].to_string();
    let temperature_celsius = parts[1].parse::<f64>().ok();
    #[allow(clippy::cast_precision_loss)]
    let load = parts[2].parse::<f64>().ok().map(|v| v / 100.0);
    let mem_used_mib: Option<u64> = parts[3].parse().ok();
    let mem_total_mib: Option<u64> = parts[4].parse().ok();
    let memory_used_bytes = mem_used_mib.map(|m| m * 1024 * 1024);
    let memory_total_bytes = mem_total_mib.map(|m| m * 1024 * 1024);
    Some(GpuState {
        vendor: GpuVendor::Nvidia,
        name: gpu_name,
        temperature_celsius,
        load,
        memory_used_bytes,
        memory_total_bytes,
    })
}

fn read_gpu() -> Option<GpuState> {
    read_amd_gpu().or_else(read_nvidia_gpu)
}

// ── /proc/self/mountinfo parsing ─────────────────────────────────────────────

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

/// Filesystems considered "pseudo" — kernel synthetic filesystems we never
/// want to show as a "disk". Matches the spirit of `findmnt --real`.
const PSEUDO_FSTYPES: &[&str] = &[
    "proc", "sysfs", "cgroup", "cgroup2", "devtmpfs", "devpts", "tmpfs",
    "mqueue", "securityfs", "pstore", "bpf", "tracefs", "debugfs",
    "hugetlbfs", "configfs", "fusectl", "binfmt_misc", "autofs",
    "efivarfs", "ramfs", "rpc_pipefs", "nsfs", "selinuxfs", "overlay",
    "squashfs",
    // Userspace pseudo-fuse mounts: gvfs auto-mounts and Flatpak portals.
    // Real user fuse storage (sshfs, gocryptfs, etc.) uses other fuse.*
    // subtypes and stays visible.
    "fuse.gvfsd-fuse", "fuse.portal",
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
            #[allow(clippy::cast_possible_truncation)]
            out.push(v as u8);  // safe: mountinfo only emits \000–\377
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

/// Read and parse the live `/proc/self/mountinfo`.
///
/// Returns an empty list on read failure (e.g. sandboxed runtime); the
/// caller's only failure mode in that case is reporting zero mounts.
fn read_mountlist() -> Vec<MountSpec> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|t| parse_mountinfo(&t))
        .unwrap_or_default()
}

// ── Disk usage ────────────────────────────────────────────────────────────────

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

// ── Process count ─────────────────────────────────────────────────────────────

fn read_process_count() -> u32 {
    std::fs::read_dir("/proc")
        .map_or(0, |iter| {
            #[allow(clippy::cast_possible_truncation)]
            let count = iter
                .filter_map(std::result::Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.parse::<u32>().is_ok())
                })
                .count();
            count.try_into().unwrap_or(u32::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_meminfo_extracts_swap_fields() {
        let text = "\
MemTotal:       16331836 kB
MemFree:         1234567 kB
MemAvailable:    8000000 kB
SwapTotal:       8388604 kB
SwapFree:        4194302 kB
";
        let m = parse_meminfo(text);
        assert_eq!(m.total, 16_331_836 * 1024);
        assert_eq!(m.swap_total, 8_388_604 * 1024);
        assert_eq!(m.swap_used, 4_194_302 * 1024);
    }

    #[test]
    fn parse_meminfo_zero_swap_when_missing() {
        let text = "\
MemTotal:       16331836 kB
MemFree:         1234567 kB
MemAvailable:    8000000 kB
";
        let m = parse_meminfo(text);
        assert_eq!(m.swap_total, 0);
        assert_eq!(m.swap_used, 0);
    }

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
        assert_eq!(v.len(), 2, "fuse.sshfs + ext4 survive; gvfs + portal filtered");
        assert_eq!(v[0].path, "/mnt/server");
        assert_eq!(v[0].fstype, "fuse.sshfs");
        assert_eq!(v[1].path, "/");
        assert_eq!(v[1].fstype, "ext4");
    }
}
