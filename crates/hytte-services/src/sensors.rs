//! Sensors service — polls `/proc/stat`, `/proc/meminfo`, and
//! `/proc/net/dev` every second and exposes CPU load, memory usage, and
//! network I/O rates as `futures-signals` signals.
//!
//! # Public API
//!
//! ```ignore
//! // Register once at startup:
//! .with(sensors::service())
//!
//! // Subscribe in widgets:
//! sensors::cpu()     -> impl Signal<Item = CpuLoad>
//! sensors::memory()  -> impl Signal<Item = Memory>
//! sensors::network() -> impl Signal<Item = NetIo>
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

// ── Service handle ────────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct SensorsHandles {
    pub(crate) cpu: Mutable<CpuLoad>,
    pub(crate) memory: Mutable<Memory>,
    pub(crate) network: Mutable<NetIo>,
}

impl Default for SensorsHandles {
    fn default() -> Self {
        Self {
            cpu: Mutable::new(CpuLoad::default()),
            memory: Mutable::new(Memory::default()),
            network: Mutable::new(NetIo::default()),
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

        rt.spawn(async move {
            poll_loop(cpu_writer, mem_writer, net_writer).await;
        });

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

// ── Polling loop ──────────────────────────────────────────────────────────────

struct PollState {
    /// `(active_prev, total_prev)` per cpu line — index 0 = overall, 1+ = core N-1.
    cpu_prev: Vec<(u64, u64)>,
    /// name → `(rx_bytes, tx_bytes, sample_instant)`
    net_prev: HashMap<String, (u64, u64, Instant)>,
}

impl PollState {
    fn new() -> Self {
        Self {
            cpu_prev: Vec::new(),
            net_prev: HashMap::new(),
        }
    }
}

async fn poll_loop(
    cpu_writer: Mutable<CpuLoad>,
    mem_writer: Mutable<Memory>,
    net_writer: Mutable<NetIo>,
) {
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

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
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
    let mut total_kb: u64 = 0;
    let mut free_kb: u64 = 0;
    let mut available_kb: u64 = 0;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemFree:") {
            free_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available_kb = parse_kb(rest);
        }
    }

    let total = total_kb * 1024;
    let free = free_kb * 1024;
    let available = available_kb * 1024;
    let used = total.saturating_sub(available);

    Ok(Memory { total, free, available, used })
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
