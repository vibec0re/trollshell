//! Procfs/sysfs samplers for CPU/memory/network/disk/GPU stats (#1249, P0 of
//! the #1248 epic).
//!
//! GTK-free leaf crate on the `hytte-preem` extraction precedent (#859): the
//! actual `/proc` and `/sys` reads, the pure data shapes they produce, and
//! their unit tests moved here byte-for-byte out of
//! `hytte-services::sensors`. That crate's `sensors` module keeps the
//! `Service`/`SensorsHandles` wrapper, the 1 Hz tick loop, the sparkline
//! history accumulators, and the `AsyncFd` mount-table watcher — everything
//! that needs `futures-signals`' `Mutable` or a tokio runtime — and calls
//! into this crate's `read_*`/`compute_*` functions for the actual sampling.
//! A plugin cannot subscribe to `hytte-services` directly, so this is what
//! lets a future `hytte-plugin-stats` (#1250) reuse the same samplers without
//! linking GTK, D-Bus, or a reactive registry.
//!
//! # Public API
//!
//! ```ignore
//! hytte_sensors::read_proc_stat()       -> Result<Vec<(u64, u64)>, io::Error>
//! hytte_sensors::compute_cpu_load(..)   -> CpuLoad
//! hytte_sensors::read_cpu_freq()        -> CpuFreq
//! hytte_sensors::read_proc_meminfo()    -> Result<Memory, io::Error>
//! hytte_sensors::read_proc_net_dev()    -> Result<Vec<(String, u64, u64)>, io::Error>
//! hytte_sensors::read_net_connections() -> NetConnections
//! hytte_sensors::read_proc_diskstats()  -> Result<Vec<(String, u64, u64)>, io::Error>
//! hytte_sensors::compute_disk_io(..)    -> (DiskIo, HashMap<..>)
//! hytte_sensors::read_cpu_temp(..)      -> CpuTemp
//! hytte_sensors::read_gpu_with_cache(..) -> (Option<GpuState>, GpuCache)
//! hytte_sensors::read_mountlist()       -> Vec<MountSpec>
//! hytte_sensors::read_disk_for_specs(..) -> DiskUsage
//! hytte_sensors::read_process_count()   -> u32
//! ```

mod cast;
mod cpufreq;
mod disk;
mod diskio;
mod gpu;
mod hwmon;
mod meminfo;
mod net;
mod proc_stat;

pub use cpufreq::read_cpu_freq;
pub use disk::{read_disk_for_specs, read_mountlist, read_process_count};
pub use diskio::{compute_disk_io, read_proc_diskstats};
pub use gpu::{GpuCache, NVIDIA_READING_TTL, read_gpu_with_cache};
pub use hwmon::read_cpu_temp;
pub use meminfo::read_proc_meminfo;
pub use net::{read_net_connections, read_proc_net_dev};
pub use proc_stat::{compute_cpu_load, read_proc_stat};

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

// ── Internal type ─────────────────────────────────────────────────────────────

/// One mounted filesystem, parsed from `/proc/self/mountinfo`.
///
/// Consumed by the disk poller (`disk::read_disk_for_specs`) and by
/// `hytte-services`' mount-table watcher, which holds the live list in a
/// `Mutable<Vec<MountSpec>>` and refreshes it on `POLLPRI`. The struct itself
/// is `pub` so that `Mutable` can name it across the crate boundary; its
/// fields stay crate-private — nothing outside `hytte-sensors` reads them.
#[derive(Clone, Debug)]
pub struct MountSpec {
    /// Mount point (mountinfo field 5), with octal escapes decoded.
    pub(crate) path: String,
    /// `(major, minor)` from mountinfo field 3 — used for dedup.
    pub(crate) dev_id: (u32, u32),
    /// fstype (right-half token 1) — diagnostic only.
    pub(crate) fstype: String,
}
