//! GPU state reading — AMD via sysfs, Intel via sysfs (i915/xe), Nvidia via `nvidia-smi`.

use crate::cast::{millicelsius_to_celsius, percent_u64_to_ratio, u64_to_f64_count};

use super::{GpuState, GpuVendor};
use std::time::{Duration, Instant};

/// True when a sysfs `device/vendor` file's contents identify an AMD PCI
/// device (vendor ID `0x1002`). Factored out of [`read_amd_gpu`] so the
/// string-parsing rule is testable against fixture strings without touching
/// `/sys`.
fn is_amd_vendor(vendor_file_contents: &str) -> bool {
    vendor_file_contents.trim() == "0x1002"
}

/// Parse the GPU display name out of a sysfs `device/uevent` file's contents:
/// pulls the `DRIVER=` line (e.g. `amdgpu`) into `"AMD (amdgpu)"`. Falls back
/// to `"AMD GPU"` when no `DRIVER=` line is present. Factored out of
/// [`read_amd_gpu`] so the parsing is testable against fixture strings.
fn amd_gpu_name_from_uevent(uevent_file_contents: &str) -> String {
    uevent_file_contents
        .lines()
        .find_map(|l| l.strip_prefix("DRIVER=").map(|d| format!("AMD ({d})")))
        .unwrap_or_else(|| "AMD GPU".to_string())
}

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
        if !is_amd_vendor(&vendor) {
            continue;
        }

        let load = fs::read_to_string(device.join("gpu_busy_percent"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(percent_u64_to_ratio);

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
                    temperature_celsius = Some(millicelsius_to_celsius(v));
                    break;
                }
            }
        }

        // Name from /sys/class/drm/cardN/device/uevent or just hardcode "AMD GPU"
        let gpu_name = fs::read_to_string(device.join("uevent"))
            .ok()
            .map_or_else(|| "AMD GPU".to_string(), |s| amd_gpu_name_from_uevent(&s));

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

/// Read RC6 residency in milliseconds from the Intel sysfs paths.
///
/// Tries the newer `gt/gt0/rc6_residency_ms` path first (xe driver / multi-tile),
/// then falls back to the older `power/rc6_residency_ms` (i915).
fn read_intel_rc6_ms(card_path: &std::path::Path) -> Option<u64> {
    use std::fs;
    // Newer path: xe driver or recent i915 with gt sub-directory
    let newer = card_path.join("gt/gt0/rc6_residency_ms");
    if let Ok(s) = fs::read_to_string(&newer)
        && let Ok(v) = s.trim().parse::<u64>()
    {
        return Some(v);
    }
    // Older i915 path
    let older = card_path.join("power/rc6_residency_ms");
    fs::read_to_string(older)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Compute Intel GPU usage from RC6 idle-residency delta.
///
/// RC6 is a power-saving idle state; when the GPU is busy it exits RC6.
/// `idle% = Δrc6_ms / dt_ms * 100`, `usage% = 100 − idle%`.
///
/// Returns `(new_rc6_ms, usage_ratio_0_to_1)`.  On the first call (no prev),
/// returns `(current_rc6_ms, 0.0)`.
#[allow(clippy::cast_precision_loss)] // dt_ms and delta are bounded well within f64 precision
fn compute_intel_usage(
    rc6_now: u64,
    prev: Option<(u64, Instant)>,
    now: Instant,
) -> (Option<(u64, Instant)>, Option<f64>) {
    let new_prev = Some((rc6_now, now));
    let Some((rc6_prev, prev_when)) = prev else {
        // First tick — no delta available yet.
        return (new_prev, Some(0.0));
    };
    // Duration between ticks is at most a few seconds; u128→u64 is always safe in practice.
    // Saturate at u64::MAX (584 million years) to avoid any theoretical overflow.
    let dt_ms = u64::try_from(now.duration_since(prev_when).as_millis()).unwrap_or(u64::MAX);
    if dt_ms == 0 {
        return (new_prev, Some(0.0));
    }
    let delta_rc6 = rc6_now.saturating_sub(rc6_prev);
    // idle% = delta_rc6_ms / dt_ms * 100; clamp to [0, 100]
    let idle_pct = (delta_rc6 * 100 / dt_ms).min(100);
    let usage_pct = 100u64.saturating_sub(idle_pct);
    let load = u64_to_f64_count(usage_pct) / 100.0;
    (new_prev, Some(load))
}

fn read_intel_gpu(rc6_prev: Option<(u64, Instant)>) -> Option<(GpuState, Option<(u64, Instant)>)> {
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
        let card_path = entry.path();
        let device = card_path.join("device");

        // Detect Intel via driver symlink resolving to i915 or xe.
        let driver_path = device.join("driver");
        let Ok(resolved) = fs::read_link(&driver_path) else {
            continue;
        };
        let driver_name = resolved.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if driver_name != "i915" && driver_name != "xe" {
            continue;
        }

        // RC6-based usage delta
        let now = Instant::now();
        let (new_rc6_prev, load) = match read_intel_rc6_ms(&card_path) {
            Some(rc6_now) => compute_intel_usage(rc6_now, rc6_prev, now),
            None => (rc6_prev, None),
        };

        // Temperature: walk device/hwmon/hwmonN/temp1_input (same as AMD)
        let mut temperature_celsius = None;
        if let Ok(hwmon) = fs::read_dir(device.join("hwmon")) {
            for h in hwmon.flatten() {
                let temp = h.path().join("temp1_input");
                if let Ok(s) = fs::read_to_string(&temp)
                    && let Ok(v) = s.trim().parse::<u64>()
                {
                    temperature_celsius = Some(millicelsius_to_celsius(v));
                    break;
                }
            }
        }

        // VRAM: discrete Arc only; absent/0 on integrated iGPU — degrade gracefully.
        let memory_used_bytes = fs::read_to_string(device.join("mem_info_vram_used"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&b| b > 0);
        let memory_total_bytes = fs::read_to_string(device.join("mem_info_vram_total"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&b| b > 0);

        let gpu_name = format!("Intel GPU ({driver_name})");

        return Some((
            GpuState {
                vendor: GpuVendor::Intel,
                name: gpu_name,
                temperature_celsius,
                load,
                memory_used_bytes,
                memory_total_bytes,
            },
            new_rc6_prev,
        ));
    }
    None
}

/// How long one `nvidia-smi` invocation may run before it is killed, once
/// one has answered in this process (until then it is
/// [`NVIDIA_SMI_FIRST_DEADLINE`]).
///
/// `nvidia-smi` talks to the kernel driver, and a wedged driver (a GPU that
/// fell off the bus, a hung suspend/resume) leaves it in uninterruptible
/// sleep **forever**. Every caller runs the sampler under `spawn_blocking`
/// with no timeout of its own, so without a deadline here one such call
/// freezes *all* sampling for the life of the process. A healthy cold start
/// (CUDA/NVML init) is 100-300 ms; 2 s is an order of magnitude over that
/// and still only two ticks of a 1 Hz sampler.
pub const NVIDIA_SMI_DEADLINE: Duration = Duration::from_secs(2);

/// How long `nvidia-smi` may run while none has answered yet in this process
/// (#1561). After the first reading, [`NVIDIA_SMI_DEADLINE`] applies.
///
/// On a host without persistence mode, the first `nvidia-smi` brings the
/// driver up from cold, which can take longer than 2 s. With only
/// [`NVIDIA_SMI_DEADLINE`], that call was killed every time, so the GPU row
/// never appeared on a host where it used to be slow but work. 10 s lets a
/// slow first call finish.
///
/// The cost: until the first reading, each attempt can block its caller this
/// long. On a driver that never answers, every attempt does. The retry
/// backoff ([`NVIDIA_BACKOFF_INITIAL`] up to [`NVIDIA_BACKOFF_MAX`]) is
/// counted from when such an attempt *returns*, so each stall is followed by
/// at least one full backoff of live readings rather than by a gate that
/// opened while the call ran (#1643 review). A call that runs out this
/// deadline is killed and reaped like any other.
pub const NVIDIA_SMI_FIRST_DEADLINE: Duration = Duration::from_secs(10);

/// Set once any `nvidia-smi` call in this process has returned a reading,
/// and never cleared. [`nvidia_smi_deadline`] reads it.
static NVIDIA_SMI_ANSWERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The deadline for the next `nvidia-smi` call: [`NVIDIA_SMI_FIRST_DEADLINE`]
/// until one has answered in this process, [`NVIDIA_SMI_DEADLINE`] after.
fn nvidia_smi_deadline(answered_before: bool) -> Duration {
    if answered_before {
        NVIDIA_SMI_DEADLINE
    } else {
        NVIDIA_SMI_FIRST_DEADLINE
    }
}

/// First retry delay after a transient `nvidia-smi` failure (timeout,
/// non-zero exit, unparseable output). Doubles per consecutive failure up to
/// [`NVIDIA_BACKOFF_MAX`], and resets on the first success.
///
/// Each delay is counted from the moment the failed call **returned**, not
/// from when it started: a call that ran out a 10 s
/// [`NVIDIA_SMI_FIRST_DEADLINE`] would otherwise come back to a 5 s gate
/// that had already opened, and retry at once.
pub const NVIDIA_BACKOFF_INITIAL: Duration = Duration::from_secs(5);

/// Ceiling of the `nvidia-smi` retry backoff: a GPU that stays broken costs
/// one fork (and at worst one [`NVIDIA_SMI_DEADLINE`] of blocking, or one
/// [`NVIDIA_SMI_FIRST_DEADLINE`] while none has answered yet) per five
/// minutes, not one per tick.
pub const NVIDIA_BACKOFF_MAX: Duration = Duration::from_mins(5);

/// Why one `nvidia-smi` call produced no reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NvidiaFailure {
    /// The program does not exist (`ErrorKind::NotFound`): this machine has
    /// no Nvidia driver userland. Permanent for the life of the process.
    NotInstalled,
    /// It exists but did not answer: spawn error, deadline, non-zero exit or
    /// garbage output. A driver that is still loading, or briefly wedged,
    /// recovers - so this is retried with backoff, never written off.
    Transient,
}

/// Parse one `--format=csv,noheader,nounits` line of
/// `name,temperature.gpu,utilization.gpu,memory.used,memory.total`.
fn parse_nvidia_csv(s: &str) -> Option<GpuState> {
    let line = s.lines().next()?;
    let parts: Vec<&str> = line.split(',').map(str::trim).collect();
    if parts.len() < 5 {
        return None;
    }
    let gpu_name = parts[0].to_string();
    let temperature_celsius = parts[1].parse::<f64>().ok();
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

/// Most killed children the reaper tracks at once.
///
/// A child stuck in uninterruptible driver sleep stays on the reaper's list
/// until the kernel lets it go. How fast new ones can arrive depends on the
/// caller:
///
/// - On the retry backoff alone, at most one per backoff plus the killed
///   call's own deadline ([`NVIDIA_BACKOFF_INITIAL`] doubling to
///   [`NVIDIA_BACKOFF_MAX`], counted from when the call returned), so 64
///   means at least 4.8 h of continuous failures (5 + 10 + ... + 160 s, then
///   57 x 300 s).
/// - [`GpuCache::forget_readings`] clears that backoff, and
///   `hytte-plugin-stats` calls it (`Sampler::reset`) when its sidebar
///   reopens after a while. So with a wedged driver, each such reopen can add
///   one more child (one fork each, killed at [`NVIDIA_SMI_FIRST_DEADLINE`]
///   when none has ever answered), and the cap is what bounds that path.
///
/// Past the cap a further child is dropped un-waited (a zombie until the
/// process exits, the pre-reaper behaviour) so the list, and the memory
/// behind it, stays bounded.
const REAPER_MAX_PENDING: usize = 64;

/// How often the reaper re-polls its list while any child is still alive.
const REAPER_POLL: Duration = Duration::from_millis(100);

/// The one long-lived reaper thread's inbox. `None` until first use, and
/// again after a failed spawn or a dead receiver, so the next kill retries.
static REAPER: std::sync::Mutex<Option<std::sync::mpsc::Sender<std::process::Child>>> =
    std::sync::Mutex::new(None);

/// True while the last reaper spawn attempt failed, so a streak of failures
/// logs once rather than once per kill.
static REAPER_SPAWN_FAILING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Add `child` to the reaper's `pending` list, unless the list is full.
///
/// Children that already exited are retired first (`try_wait` reaps them), so
/// `cap` counts only children still running: a list of finished children
/// never turns a new one away. An error from `try_wait` (ECHILD: already
/// reaped elsewhere) also retires the entry. Returns whether `child` was
/// kept; past the cap it is dropped un-waited (see [`REAPER_MAX_PENDING`]).
fn admit(pending: &mut Vec<std::process::Child>, child: std::process::Child, cap: usize) -> bool {
    pending.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
    if pending.len() < cap {
        pending.push(child);
        true
    } else {
        false
    }
}

/// Body of the reaper thread: collect killed children and `try_wait` them all
/// until each is gone. Polling the whole list (instead of blocking in `wait`
/// on one) means a child stuck in D-state cannot delay reaping the others.
fn reaper_loop(rx: &std::sync::mpsc::Receiver<std::process::Child>) {
    let mut pending: Vec<std::process::Child> = Vec::new();
    loop {
        // Idle: block for work. Busy: wait one poll interval for more.
        let next = if pending.is_empty() {
            rx.recv().map_err(|_| ())
        } else {
            match rx.recv_timeout(REAPER_POLL) {
                Ok(c) => Ok(c),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(()),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // No sender left: nothing new can arrive, but the
                    // children already held must still be waited on.
                    while !pending.is_empty() {
                        std::thread::sleep(REAPER_POLL);
                        pending.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
                    }
                    return;
                }
            }
        };
        match next {
            Ok(child) => {
                // Over the cap the child is dropped un-waited (see
                // REAPER_MAX_PENDING).
                admit(&mut pending, child, REAPER_MAX_PENDING);
            }
            Err(()) if pending.is_empty() => return,
            Err(()) => {}
        }
        // Reap whatever has exited; keep the rest.
        pending.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
    }
}

/// Spawn the reaper thread and return its inbox, or `None` if the thread
/// could not be started (a transient EAGAIN, say).
fn spawn_reaper() -> Option<std::sync::mpsc::Sender<std::process::Child>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("nvidia-smi-reaper".into())
        .spawn(move || reaper_loop(&rx))
        .ok()
        .map(|_| tx)
}

/// Hand a killed `nvidia-smi` to the reaper so it is always `wait`ed.
///
/// A failed spawn (or a reaper whose thread died) is retried on the next
/// kill: the inbox is only kept while it works. While spawning keeps failing,
/// each kill's child is dropped un-waited (a zombie until the process exits),
/// as before the reaper existed, and one `warn!` says so per failure streak.
fn reap_later(child: std::process::Child) {
    reap_later_via(&REAPER, child);
}

/// [`reap_later`] against a given inbox slot, so a test can use its own
/// instead of the process-wide one its siblings share.
fn reap_later_via(
    reaper: &std::sync::Mutex<Option<std::sync::mpsc::Sender<std::process::Child>>>,
    child: std::process::Child,
) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut slot = reaper
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let child = match slot.as_ref() {
        Some(tx) => match tx.send(child) {
            Ok(()) => return,
            // Dead receiver: the send hands the child back.
            Err(std::sync::mpsc::SendError(c)) => c,
        },
        None => child,
    };
    *slot = None;
    if let Some(tx) = spawn_reaper() {
        REAPER_SPAWN_FAILING.store(false, Relaxed);
        if tx.send(child).is_ok() {
            *slot = Some(tx);
        }
    } else if !REAPER_SPAWN_FAILING.swap(true, Relaxed) {
        tracing::warn!(
            "could not start the nvidia-smi reaper thread; children killed while it \
             cannot start are never waited on (zombies until the process exits)"
        );
    }
}

/// Run `program` (normally `nvidia-smi`) with a hard `deadline`.
///
/// The child is polled with `try_wait` rather than waited on, so a hung
/// driver costs at most `deadline`. On timeout it is `SIGKILL`ed and reaped
/// by the single reaper thread ([`reap_later`]): a process stuck in
/// uninterruptible driver sleep may not die promptly, and waiting on it must
/// not hold the sampler hostage either.
fn query_nvidia_smi(program: &str, deadline: Duration) -> Result<GpuState, NvidiaFailure> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = Command::new(program)
        .args([
            "--query-gpu=name,temperature.gpu,utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                NvidiaFailure::NotInstalled
            } else {
                NvidiaFailure::Transient
            }
        })?;

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                reap_later(child);
                return Err(NvidiaFailure::Transient);
            }
        }
    };
    if !status.success() {
        return Err(NvidiaFailure::Transient);
    }
    let mut out = String::new();
    child
        .stdout
        .take()
        .ok_or(NvidiaFailure::Transient)?
        .read_to_string(&mut out)
        .map_err(|_| NvidiaFailure::Transient)?;
    parse_nvidia_csv(&out).ok_or(NvidiaFailure::Transient)
}

fn read_nvidia_gpu() -> Result<GpuState, NvidiaFailure> {
    query_nvidia_smi_tracked("nvidia-smi", &NVIDIA_SMI_ANSWERED, nvidia_smi_deadline)
}

/// One [`query_nvidia_smi`] call whose deadline is `deadline_for(answered)`,
/// and which sets `answered` when it returns a reading.
///
/// Only a reading sets it. A failed spawn, a timeout, a non-zero exit or
/// output that does not parse all leave it as it was, and nothing clears it.
/// `deadline_for` is [`nvidia_smi_deadline`] in production; a test passes a
/// short one so a timeout does not cost it the full first-call deadline.
fn query_nvidia_smi_tracked(
    program: &str,
    answered: &std::sync::atomic::AtomicBool,
    deadline_for: impl FnOnce(bool) -> Duration,
) -> Result<GpuState, NvidiaFailure> {
    use std::sync::atomic::Ordering::Relaxed;
    let result = query_nvidia_smi(program, deadline_for(answered.load(Relaxed)));
    if result.is_ok() {
        answered.store(true, Relaxed);
    }
    result
}

/// How long a successful Nvidia reading stays valid before
/// `read_nvidia_with_cache_at` forks `nvidia-smi` again: **1.5 s**.
///
/// The samplers that drive [`read_gpu_with_cache`] tick at 1 Hz (#1249), and
/// `nvidia-smi` is a fork plus an NVML init, routinely 100-300 ms of CPU and
/// a wake of the GPU driver. The approved intent (#1542, item 7) is to sample
/// the GPU about every 2 s, less often than the CPU.
///
/// This **deliberately gives up** the old "never serve a previous tick's
/// reading" contract (500 ms, #1297). A served reading is under 1.5 s old
/// plus the fork's own time.
///
/// Why 1.5 s and not the literal 2 s: the TTL must stay away from every
/// caller's call period, for the same reason the old 500 ms stayed away from
/// the 1 s tick. `sampled_at` is the instant the fork *started*, inside a
/// tick whose own start jitters (a slow `statvfs` alone can add up to 1 s),
/// so a TTL equal to a 2-tick period would sometimes serve the reading from
/// two ticks ago and only fork two ticks later. 1.5 s sits half a tick from
/// both 1 s and 2 s, so +-0.5 s of jitter cannot change which tick forks:
///
/// - the native sampler reads the GPU every 2nd tick (`do_gpu`): its calls
///   land ~2 s apart, always past the TTL, so it forks every time, every
///   2 s, unchanged;
/// - the `hytte-plugin-stats` sampler reads every 1 Hz tick: a call ~1 s
///   after a fork is served, the one ~2 s after forks, so it now forks
///   every other tick (it forked every tick before).
///
/// It still collapses several callers that land inside the same window (a
/// fresh cache handed to more than one poller in the same process) onto the
/// one cached reading.
///
/// This bounds forks **within one process sharing one cache value** — three
/// processes (the native sensors service, a bar `hytte-plugin-stats`
/// instance, a sidebar instance, #1248) each carry their own [`GpuCache`]
/// and still fork independently. Collapsing that is #1252 (retire the native
/// sampler) plus the sidebar instance's own parking, which already stops a
/// closed sidebar from polling at all.
pub const NVIDIA_READING_TTL: Duration = Duration::from_millis(1500);

/// Per-tick GPU cache state threaded through the poll loop.
///
/// Carried alongside each GPU tick so readers never need `Mutex` or `Arc`.
/// Not [`Copy`] — `nvidia_last` carries a [`GpuState`],
/// which owns a `String`; callers that previously relied on `Copy` move the
/// value out with [`std::mem::take`] instead (see
/// `hytte-services::sensors::mod::poll_loop`, the existing precedent this
/// followed, and `hytte-plugin-stats::sample::Sampler::tick`, the caller
/// #1297 updated to match it).
#[derive(Clone, Debug, Default)]
pub struct GpuCache {
    /// Whether `nvidia-smi` is available.
    ///
    /// - `None`        — not yet probed; probe on the next GPU tick.
    /// - `Some(false)` — previously absent; skip `nvidia-smi`.
    /// - `Some(true)`  — previously present; call `nvidia-smi` directly, or
    ///   serve [`nvidia_last`](Self::nvidia_last) if it is still within
    ///   [`NVIDIA_READING_TTL`].
    pub(super) nvidia_available: Option<bool>,
    /// Previous RC6 residency sample for Intel GPU usage computation.
    ///
    /// `None` means either no Intel GPU detected yet, or this is the first
    /// tick (no delta available).
    pub(super) intel_rc6_prev: Option<(u64, Instant)>,
    /// The last successful Nvidia reading and the instant its fork was
    /// *started* (not when `nvidia-smi` returned — a cold CUDA/NVML init is
    /// routinely 100-300 ms, so a reading served at just under the TTL can
    /// be measurably older in wall-clock terms than the TTL says; at the 1.5 s
    /// TTL that is up to a few hundred ms on top of it).
    ///
    /// `None` until the first successful `nvidia-smi` fork, and reset to
    /// `None` whenever a fork fails (mirrors `nvidia_available` flipping to
    /// `Some(false)` — a failed reading is never served stale). See
    /// [`NVIDIA_READING_TTL`] for how long a reading here stays valid, and
    /// [`forget_readings`](Self::forget_readings) for dropping it early on a
    /// caller's own park/unpark edge.
    pub(super) nvidia_last: Option<(GpuState, Instant)>,
    /// Retry gate after a *transient* `nvidia-smi` failure: do not fork again
    /// before `.0`, which is `.1` after the instant the failed call
    /// *returned* (unlike [`nvidia_last`](Self::nvidia_last)'s instant, which
    /// is when its fork started); `.1` is the delay that produced it (the
    /// next consecutive failure doubles it, capped at
    /// [`NVIDIA_BACKOFF_MAX`]). `None` while calls succeed or none has failed
    /// yet.
    pub(super) nvidia_backoff: Option<(Instant, Duration)>,
}

impl GpuCache {
    /// Forget every cached *measurement*, keeping the `nvidia-smi`
    /// availability memo (a `fork`/`exec` to re-learn, and an answer that
    /// does not go stale).
    ///
    /// For a caller whose surface parked: both `nvidia_last` and
    /// `intel_rc6_prev` are anchored to a wall-clock moment before the park,
    /// so serving either after an unpark shows a frame measured before the
    /// surface was shut (#1297; the Intel half predates it — see
    /// `hytte-plugin-stats::sample::Sampler::reset`, the caller this seam
    /// was added for).
    pub fn forget_readings(&mut self) {
        self.nvidia_last = None;
        // The retry gate is an `Instant` (monotonic, paused across suspend):
        // a failure just before a suspend would otherwise keep gating
        // recovery for minutes of awake time after the resume.
        self.nvidia_backoff = None;
        self.intel_rc6_prev = None;
    }
}

/// The Nvidia arm of [`read_gpu_with_cache`], with the [`NVIDIA_READING_TTL`]
/// throttle applied.
///
/// Isolated from the composed function (and from `Instant::now()`/
/// `nvidia-smi` directly) purely for testability: the AMD and Intel arms are
/// sysfs reads a test can't hermetically fake without touching `/sys`, but
/// this arm's only external calls are the injected `now`, `read` and
/// `ended` — see this module's tests for the counting-fake-reader cases the
/// issue asked for.
///
/// Availability semantics of `cache.nvidia_available`: `None` probes,
/// `Some(false)` never forks again, and since this fix `Some(false)` is set
/// **only** when `nvidia-smi` does not exist ([`NvidiaFailure::NotInstalled`]).
/// Any other failure (a timeout, a non-zero exit while the driver loads, odd
/// output) is [`NvidiaFailure::Transient`]: the reading is dropped, the call
/// answers `None`, and the next fork is held back by
/// [`GpuCache::nvidia_backoff`] - [`NVIDIA_BACKOFF_INITIAL`] doubling up to
/// [`NVIDIA_BACKOFF_MAX`] - so a GPU that comes back is picked up again
/// without a bar instance (which cannot park) forking at 1 Hz meanwhile.
/// The `Some(true)` arm forks only when [`NVIDIA_READING_TTL`] has elapsed
/// since the last successful reading, and otherwise returns a clone of it.
///
/// Two clocks, because a call can take up to [`NVIDIA_SMI_FIRST_DEADLINE`]:
/// `now` is the instant the call *starts*. The TTL and the retry gate are
/// checked against it, and a reading's `sampled_at` is it (see
/// [`GpuCache::nvidia_last`]). `ended` is read once, after a transient
/// failure, and the next retry gate is counted from it, so a backoff never
/// runs out while the call it follows is still running (#1643 review).
fn read_nvidia_with_cache_at(
    cache: GpuCache,
    now: Instant,
    read: impl FnOnce() -> Result<GpuState, NvidiaFailure>,
    ended: impl FnOnce() -> Instant,
) -> (Option<GpuState>, GpuCache) {
    let nv_available = cache.nvidia_available.unwrap_or(true); // unknown → optimistically try once
    if !nv_available {
        return (
            None,
            GpuCache {
                nvidia_available: Some(false),
                intel_rc6_prev: None,
                nvidia_last: None,
                nvidia_backoff: None,
            },
        );
    }

    // A reading inside the TTL is served without forking — this is the fix:
    // previously every call with `nvidia_available == Some(true)` forked.
    if let Some((state, sampled_at)) = &cache.nvidia_last
        && now.saturating_duration_since(*sampled_at) < NVIDIA_READING_TTL
    {
        let reading = state.clone();
        return (
            Some(reading),
            GpuCache {
                nvidia_available: Some(true),
                intel_rc6_prev: None,
                nvidia_last: cache.nvidia_last,
                nvidia_backoff: None,
            },
        );
    }

    // Still inside the retry gate of an earlier transient failure: no fork.
    if let Some((not_before, _)) = cache.nvidia_backoff
        && now < not_before
    {
        return (
            None,
            GpuCache {
                nvidia_last: None,
                intel_rc6_prev: None,
                ..cache
            },
        );
    }

    match read() {
        Ok(state) => (
            Some(state.clone()),
            GpuCache {
                nvidia_available: Some(true),
                intel_rc6_prev: None,
                nvidia_last: Some((state, now)),
                nvidia_backoff: None,
            },
        ),
        Err(NvidiaFailure::NotInstalled) => (
            None,
            GpuCache {
                nvidia_available: Some(false),
                intel_rc6_prev: None,
                nvidia_last: None,
                nvidia_backoff: None,
            },
        ),
        Err(NvidiaFailure::Transient) => {
            let delay = cache
                .nvidia_backoff
                .map_or(NVIDIA_BACKOFF_INITIAL, |(_, d)| {
                    d.saturating_mul(2).min(NVIDIA_BACKOFF_MAX)
                });
            (
                None,
                GpuCache {
                    nvidia_available: cache.nvidia_available,
                    intel_rc6_prev: None,
                    nvidia_last: None,
                    nvidia_backoff: Some((ended() + delay, delay)),
                },
            )
        }
    }
}

/// Read GPU state, caching probe results across ticks.
///
/// Probe order: AMD → Intel → Nvidia.
///
/// `cache` carries the mutable per-tick state:
/// - `nvidia_available`: whether `nvidia-smi` is usable (see [`GpuCache`]).
/// - `intel_rc6_prev`: previous `(rc6_ms, Instant)` for RC6 delta computation.
/// - `nvidia_last`: the last successful Nvidia reading, served instead of
///   forking again while still within [`NVIDIA_READING_TTL`] (#1297).
///
/// Returns `(gpu_state, updated_cache)`. The caller stores the returned cache
/// back into `PollState`.
#[must_use]
pub fn read_gpu_with_cache(cache: GpuCache) -> (Option<GpuState>, GpuCache) {
    read_gpu_with_cache_at(
        cache,
        Instant::now(),
        read_amd_gpu,
        read_intel_gpu,
        read_nvidia_gpu,
        // Read after the call, not the `now` above: a backoff counts from
        // when the failed call returned.
        Instant::now,
    )
}

/// [`read_gpu_with_cache`] over injected clocks and injected probe arms —
/// the seam that makes the *composition* (which arm the cache is handed to,
/// in what order) testable, not just the Nvidia arm's TTL in isolation
/// (#1297 review MEDIUM 2). The public function above is the one-line real
/// wiring: `Instant::now()` as `now`, `Instant::now` itself as `ended`, and
/// the three real `read_*` functions. `now` and `ended` mean what they do in
/// [`read_nvidia_with_cache_at`], which is the only arm that reads `ended`.
fn read_gpu_with_cache_at(
    cache: GpuCache,
    now: Instant,
    amd: impl FnOnce() -> Option<GpuState>,
    intel: impl FnOnce(Option<(u64, Instant)>) -> Option<(GpuState, Option<(u64, Instant)>)>,
    nvidia: impl FnOnce() -> Result<GpuState, NvidiaFailure>,
    ended: impl FnOnce() -> Instant,
) -> (Option<GpuState>, GpuCache) {
    // AMD sysfs reads don't need caching — `read_amd_gpu` only walks
    // `/sys/class/drm` and exits on the first AMD card it finds.
    if let Some(state) = amd() {
        // AMD present — preserve whatever nvidia_available/nvidia_last were
        // (no need to probe or throttle Nvidia on an AMD box).
        return (
            Some(state),
            GpuCache {
                nvidia_available: cache.nvidia_available.or(Some(false)),
                intel_rc6_prev: None,
                nvidia_last: cache.nvidia_last,
                nvidia_backoff: cache.nvidia_backoff,
            },
        );
    }

    // No AMD GPU. Try Intel.
    if let Some((state, new_rc6_prev)) = intel(cache.intel_rc6_prev) {
        return (
            Some(state),
            GpuCache {
                nvidia_available: cache.nvidia_available.or(Some(false)),
                intel_rc6_prev: new_rc6_prev,
                nvidia_last: cache.nvidia_last,
                nvidia_backoff: cache.nvidia_backoff,
            },
        );
    }

    // No AMD or Intel GPU. Nvidia arm carries its own reading-TTL throttle —
    // and, critically, gets the CALLER's cache, not a fresh default one.
    read_nvidia_with_cache_at(cache, now, nvidia, ended)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;

    // ── is_amd_vendor / amd_gpu_name_from_uevent ─────────────────────────────

    #[test]
    fn is_amd_vendor_matches_amd_pci_id() {
        assert!(is_amd_vendor("0x1002\n"));
        assert!(is_amd_vendor("0x1002"));
    }

    #[test]
    fn is_amd_vendor_rejects_other_vendors() {
        // 0x8086 is Intel, 0x10de is Nvidia.
        assert!(!is_amd_vendor("0x8086\n"));
        assert!(!is_amd_vendor("0x10de\n"));
        assert!(!is_amd_vendor(""));
    }

    #[test]
    fn amd_gpu_name_from_uevent_extracts_driver() {
        let uevent = "DRIVER=amdgpu\nPCI_CLASS=30000\nMODALIAS=pci:foo\n";
        assert_eq!(amd_gpu_name_from_uevent(uevent), "AMD (amdgpu)");
    }

    #[test]
    fn amd_gpu_name_from_uevent_falls_back_without_driver_line() {
        let uevent = "PCI_CLASS=30000\nMODALIAS=pci:foo\n";
        assert_eq!(amd_gpu_name_from_uevent(uevent), "AMD GPU");
    }

    #[test]
    fn amd_gpu_name_from_uevent_falls_back_on_empty_input() {
        assert_eq!(amd_gpu_name_from_uevent(""), "AMD GPU");
    }

    // ── compute_intel_usage ───────────────────────────────────────────────

    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_intel_usage_first_call_has_no_delta() {
        let now = Instant::now();
        let (new_prev, load) = compute_intel_usage(1_000, None, now);
        assert_eq!(new_prev, Some((1_000, now)));
        assert_eq!(load, Some(0.0), "first sample has nothing to diff against");
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_intel_usage_zero_elapsed_reports_zero_load() {
        let t0 = Instant::now();
        // Second call lands on the exact same Instant as the first (dt_ms == 0)
        // — must not divide by zero.
        let (new_prev, load) = compute_intel_usage(500, Some((100, t0)), t0);
        assert_eq!(new_prev, Some((500, t0)));
        assert_eq!(load, Some(0.0));
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_intel_usage_counter_reset_clamps_to_full_load() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        // rc6 counter went backwards (reset/wrap) — saturating_sub floors the
        // delta at 0, meaning "no idle time observed" → 100% busy.
        let (_new_prev, load) = compute_intel_usage(100, Some((5_000, t0)), t1);
        assert_eq!(load, Some(1.0));
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_intel_usage_residency_exceeding_elapsed_clamps_to_100_idle() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        // 5000ms of RC6 residency reported over a 1000ms tick — idle% would
        // be 500% uncapped; must clamp to 100% idle (0% usage).
        let (_new_prev, load) = compute_intel_usage(5_000, Some((0, t0)), t1);
        assert_eq!(load, Some(0.0));
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn compute_intel_usage_normal_delta_computes_partial_load() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        // 500ms idle (RC6) out of a 1000ms tick → 50% idle → 50% usage.
        let (new_prev, load) = compute_intel_usage(500, Some((0, t0)), t1);
        assert_eq!(new_prev, Some((500, t1)));
        assert_eq!(load, Some(0.5));
    }

    // ── read_nvidia_with_cache_at (#1297) ────────────────────────────────────

    fn fake_reading(name: &str) -> GpuState {
        GpuState {
            vendor: GpuVendor::Nvidia,
            name: name.to_string(),
            ..GpuState::default()
        }
    }

    /// [`read_nvidia_with_cache_at`] for a `read` that takes no time: the
    /// call ends at the instant it starts. The tests of the TTL and of the
    /// gate's arithmetic use it; the ones about how long a call ran pass
    /// their own `ended`.
    fn read_nvidia_instantly(
        cache: GpuCache,
        now: Instant,
        read: impl FnOnce() -> Result<GpuState, NvidiaFailure>,
    ) -> (Option<GpuState>, GpuCache) {
        read_nvidia_with_cache_at(cache, now, read, || now)
    }

    #[test]
    fn nvidia_reading_ttl_collapses_repeated_calls_to_one_fork() {
        let fork_count = Cell::new(0u32);
        let t0 = Instant::now();

        let (first, cache) = read_nvidia_instantly(GpuCache::default(), t0, || {
            fork_count.set(fork_count.get() + 1);
            Ok(fake_reading("fake nvidia"))
        });
        assert_eq!(
            fork_count.get(),
            1,
            "the first call has no cached reading yet and must fork"
        );
        assert_eq!(first.as_ref().map(|s| s.name.as_str()), Some("fake nvidia"));

        // Several more calls, at different instants strictly inside the TTL
        // window, must all be served the cached reading with no further fork.
        for step in 1..=5u64 {
            let now = t0 + Duration::from_millis(step * 80); // up to 400ms, well under the 1.5s TTL
            let (state, _) = read_nvidia_instantly(cache.clone(), now, || {
                fork_count.set(fork_count.get() + 1);
                Ok(fake_reading("should not be forked"))
            });
            assert_eq!(
                fork_count.get(),
                1,
                "call {step} inside the TTL forked nvidia-smi again (fork count moved past 1)"
            );
            assert_eq!(
                state.as_ref().map(|s| s.name.as_str()),
                Some("fake nvidia"),
                "call {step} inside the TTL must return the cached reading, not a fresh one"
            );
        }
    }

    /// The value is a product decision (#1542, item 7), so it is pinned as a
    /// literal rather than through the constant: changing it must change this.
    #[test]
    fn nvidia_reading_ttl_is_one_and_a_half_seconds() {
        assert_eq!(NVIDIA_READING_TTL, Duration::from_millis(1500));
    }

    /// Run one scenario from a fresh fork at `t0`: a second call `after_ms`
    /// later, returning whether that call forked.
    fn forks_after(after_ms: u64) -> bool {
        let fork_count = Cell::new(0u32);
        let t0 = Instant::now();
        let (_, cache) = read_nvidia_instantly(GpuCache::default(), t0, || {
            fork_count.set(fork_count.get() + 1);
            Ok(fake_reading("first"))
        });
        assert_eq!(fork_count.get(), 1);
        let (state, _) = read_nvidia_instantly(cache, t0 + Duration::from_millis(after_ms), || {
            fork_count.set(fork_count.get() + 1);
            Ok(fake_reading("second"))
        });
        let forked = fork_count.get() == 2;
        let want = if forked { "second" } else { "first" };
        assert_eq!(state.as_ref().map(|s| s.name.as_str()), Some(want));
        forked
    }

    /// The 1 Hz caller (`hytte-plugin-stats`): a call ~1 s after a fork is
    /// served from cache and one ~2 s after forks, with +-0.4 s of tick
    /// jitter either way, so it forks every other tick.
    #[test]
    fn a_one_hz_caller_forks_every_other_tick_despite_jitter() {
        for ms in [600, 1000, 1400] {
            assert!(
                !forks_after(ms),
                "a call {ms} ms after a fork must be served"
            );
        }
        for ms in [1600, 2000, 2400] {
            assert!(forks_after(ms), "a call {ms} ms after a fork must fork");
        }
    }

    /// The 2-tick caller (the native sampler's `do_gpu`): its calls land
    /// ~2 s apart, so with jitter each one is past the TTL and forks.
    #[test]
    fn a_two_tick_caller_forks_every_call_despite_jitter() {
        for ms in [1600, 2400] {
            assert!(forks_after(ms), "a call {ms} ms after a fork must fork");
        }
    }

    #[test]
    fn nvidia_reading_ttl_elapsed_forks_again() {
        let fork_count = Cell::new(0u32);
        let t0 = Instant::now();

        let (_, cache) = read_nvidia_instantly(GpuCache::default(), t0, || {
            fork_count.set(fork_count.get() + 1);
            Ok(fake_reading("first"))
        });
        assert_eq!(fork_count.get(), 1);

        // Landing exactly on the TTL boundary must no longer be "inside" it.
        let (state, _) = read_nvidia_instantly(cache, t0 + NVIDIA_READING_TTL, || {
            fork_count.set(fork_count.get() + 1);
            Ok(fake_reading("second"))
        });
        assert_eq!(
            fork_count.get(),
            2,
            "a call at the TTL boundary must fork again instead of reusing the stale reading"
        );
        assert_eq!(state.as_ref().map(|s| s.name.as_str()), Some("second"));
    }

    #[test]
    fn nvidia_not_installed_marks_unavailable_and_clears_cached_reading() {
        let t0 = Instant::now();
        // Seed a cache as if a reading had just been taken, then land past
        // the TTL so the call must actually attempt a fresh fork - and have
        // that fork find no `nvidia-smi` at all. A failure must never leave
        // the stale reading behind for a later call to serve.
        let seeded = GpuCache {
            nvidia_available: Some(true),
            intel_rc6_prev: None,
            nvidia_last: Some((fake_reading("stale"), t0)),
            nvidia_backoff: None,
        };
        let (state, cache) = read_nvidia_instantly(seeded, t0 + NVIDIA_READING_TTL, || {
            Err(NvidiaFailure::NotInstalled)
        });
        assert!(
            state.is_none(),
            "a failed fork must not surface any reading"
        );
        assert_eq!(cache.nvidia_available, Some(false));
        assert!(
            cache.nvidia_last.is_none(),
            "a failed fork must clear the previously cached reading, not leave it stale"
        );
    }

    /// The old behaviour this replaces: any single failure wrote the GPU off
    /// for the life of the process. A transient one now only backs off.
    #[test]
    fn nvidia_transient_failure_backs_off_and_then_recovers() {
        let t0 = Instant::now();
        let forks = Cell::new(0u32);
        let fail = || {
            forks.set(forks.get() + 1);
            Err(NvidiaFailure::Transient)
        };

        let (state, cache) = read_nvidia_instantly(GpuCache::default(), t0, fail);
        assert!(state.is_none());
        assert_ne!(
            cache.nvidia_available,
            Some(false),
            "a transient failure must not write the GPU off"
        );
        assert_eq!(forks.get(), 1);

        // Inside the 5 s gate: no fork.
        let (state, cache) = read_nvidia_instantly(cache, t0 + Duration::from_millis(4_999), fail);
        assert!(state.is_none());
        assert_eq!(forks.get(), 1, "forked inside the backoff window");

        // At the gate: forks again, fails again, the delay doubles.
        let t1 = t0 + NVIDIA_BACKOFF_INITIAL;
        let (_, cache) = read_nvidia_instantly(cache, t1, fail);
        assert_eq!(forks.get(), 2);
        assert_eq!(
            cache.nvidia_backoff,
            Some((t1 + 2 * NVIDIA_BACKOFF_INITIAL, 2 * NVIDIA_BACKOFF_INITIAL))
        );

        // Recovery: the driver is back, the next due fork succeeds and the
        // gate clears.
        let t2 = t1 + 2 * NVIDIA_BACKOFF_INITIAL;
        let (state, cache) = read_nvidia_instantly(cache, t2, || Ok(fake_reading("recovered")));
        assert_eq!(state.as_ref().map(|s| s.name.as_str()), Some("recovered"));
        assert_eq!(cache.nvidia_available, Some(true));
        assert!(cache.nvidia_backoff.is_none());
    }

    #[test]
    fn forget_readings_clears_the_retry_gate() {
        let t0 = Instant::now();
        let (_, mut cache) =
            read_nvidia_instantly(GpuCache::default(), t0, || Err(NvidiaFailure::Transient));
        assert!(cache.nvidia_backoff.is_some());
        cache.forget_readings();
        assert!(cache.nvidia_backoff.is_none());
        let (state, _) = read_nvidia_instantly(cache, t0, || Ok(fake_reading("back")));
        assert!(state.is_some(), "the gate survived a park/unpark");
    }

    #[test]
    fn nvidia_backoff_is_capped() {
        let mut t = Instant::now();
        let mut cache = GpuCache::default();
        for _ in 0..20 {
            let (_, next) = read_nvidia_instantly(cache, t, || Err(NvidiaFailure::Transient));
            let (not_before, delay) = next.nvidia_backoff.expect("transient sets a gate");
            assert!(delay <= NVIDIA_BACKOFF_MAX);
            t = not_before;
            cache = next;
        }
        assert_eq!(cache.nvidia_backoff.map(|b| b.1), Some(NVIDIA_BACKOFF_MAX));
    }

    /// A retry's backoff is counted from the end of the failed call: a call
    /// that ran out the 10 s first deadline must not come back to a gate
    /// that opened while it ran (#1643 review, MEDIUM 1). Counted from the
    /// start, the 5 s gate had opened 5 s before such a call returned, so
    /// the caller's next tick forked again at once, and a wedged driver froze
    /// a 1 Hz card for 30 s without a break.
    #[test]
    fn a_backoff_is_counted_from_the_end_of_the_failed_call() {
        let forks = Cell::new(0u32);
        let wedged = || {
            forks.set(forks.get() + 1);
            Err(NvidiaFailure::Transient)
        };
        let t0 = Instant::now();
        let ended = t0 + NVIDIA_SMI_FIRST_DEADLINE;
        let (_, mut cache) = read_nvidia_with_cache_at(GpuCache::default(), t0, wedged, || ended);
        assert_eq!(
            cache.nvidia_backoff,
            Some((ended + NVIDIA_BACKOFF_INITIAL, NVIDIA_BACKOFF_INITIAL))
        );

        // The caller ticks again as soon as the call returns, and on through
        // the backoff: none of those ticks forks.
        for after_ms in [0, 1_000, 4_999] {
            let at = ended + Duration::from_millis(after_ms);
            let (_, next) = read_nvidia_with_cache_at(cache, at, wedged, || at);
            cache = next;
            assert_eq!(
                forks.get(),
                1,
                "a retry forked {after_ms} ms after the failed call returned, inside its backoff"
            );
        }

        // A full backoff after the return, the gate opens. That attempt runs
        // out its deadline too, and its doubled backoff counts from its own
        // end.
        let t1 = ended + NVIDIA_BACKOFF_INITIAL;
        let ended1 = t1 + NVIDIA_SMI_FIRST_DEADLINE;
        let (_, cache) = read_nvidia_with_cache_at(cache, t1, wedged, || ended1);
        assert_eq!(forks.get(), 2, "the gate never opened after the backoff");
        assert_eq!(
            cache.nvidia_backoff,
            Some((
                ended1 + 2 * NVIDIA_BACKOFF_INITIAL,
                2 * NVIDIA_BACKOFF_INITIAL
            ))
        );
    }

    /// The end clock is for the backoff only. A reading stays stamped with
    /// the instant its call *started* (see [`GpuCache::nvidia_last`]), even
    /// when the call took a while.
    #[test]
    fn a_reading_is_stamped_with_the_start_of_its_call() {
        let t0 = Instant::now();
        let (_, cache) = read_nvidia_with_cache_at(
            GpuCache::default(),
            t0,
            || Ok(fake_reading("slow")),
            || t0 + Duration::from_secs(3),
        );
        assert_eq!(cache.nvidia_last.map(|(_, at)| at), Some(t0));
    }

    // ── query_nvidia_smi (real child processes, hermetic scripts) ────────────

    /// Install a `#!/bin/sh` stand-in at `dir/name`, written by a `sh` child
    /// and **never by this process**.
    ///
    /// Not `std::fs::write` plus a chmod, because of `ETXTBSY`: a sibling
    /// test that forks (a `sleeper()`, another stand-in, `kill`) hands its
    /// child a copy of every fd this process has open at that instant -
    /// `O_CLOEXEC` closes them only at the child's `exec` - and `execve`
    /// refuses a file anyone holds open for writing. `query_nvidia_smi` maps
    /// that spawn error to `Transient`, so a stand-in that never ran looked
    /// like a timeout: measured on a 64-core box, 10 of 60 runs of this
    /// binary failed that way (3 of 60 on `main`), and 0 of 60 with the write
    /// in a child. Same fix as #1540's `stand_in::install` in
    /// `hytte-claude-bridge`.
    fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        let path = dir.join(name);
        let mut writer = Command::new("/bin/sh")
            .args(["-c", r#"cat > "$1" && chmod 755 "$1""#, "sh"])
            .arg(&path)
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn the script writer");
        writer
            .stdin
            .take()
            .expect("piped")
            .write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
            .expect("hand the script over");
        assert!(
            writer.wait().expect("reap the script writer").success(),
            "could not install {}",
            path.display()
        );
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn query_nvidia_smi_parses_a_fast_answer() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "smi", "echo 'NVIDIA Test, 55, 40, 1024, 8192'");
        let got = query_nvidia_smi(&p, Duration::from_secs(5)).expect("reading");
        assert_eq!(got.name, "NVIDIA Test");
        assert_eq!(got.memory_total_bytes, Some(8192 * 1024 * 1024));
    }

    #[test]
    fn query_nvidia_smi_kills_a_hung_child_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "smi", "exec sleep 30");
        let started = Instant::now();
        let got = query_nvidia_smi(&p, Duration::from_millis(200));
        assert_eq!(got.err(), Some(NvidiaFailure::Transient));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline did not bound the call"
        );
    }

    #[test]
    fn query_nvidia_smi_distinguishes_missing_from_failing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-nvidia-smi");
        assert_eq!(
            query_nvidia_smi(missing.to_str().unwrap(), Duration::from_secs(1)).err(),
            Some(NvidiaFailure::NotInstalled)
        );
        let p = script(dir.path(), "smi", "exit 9");
        assert_eq!(
            query_nvidia_smi(&p, Duration::from_secs(5)).err(),
            Some(NvidiaFailure::Transient)
        );
    }

    // ── the first-call deadline (#1561 bullet 2) ─────────────────────────────

    /// The two values are pinned as literals rather than through the
    /// constants, so changing either one has to change this test. 2 s is the
    /// existing deadline. 10 s is this PR's own choice (#1643), not a
    /// decision anyone made on #1561, and is waiting on the owner's read.
    #[test]
    fn the_deadline_is_generous_until_nvidia_smi_first_answers() {
        assert_eq!(nvidia_smi_deadline(false), Duration::from_secs(10));
        assert_eq!(nvidia_smi_deadline(true), Duration::from_secs(2));
    }

    /// One [`query_nvidia_smi_tracked`] call with a fixed `deadline`.
    /// Returns its result and the flag value its deadline was chosen from.
    fn tracked(
        program: &str,
        answered: &std::sync::atomic::AtomicBool,
        deadline: Duration,
    ) -> (Result<GpuState, NvidiaFailure>, bool) {
        let mut asked = None;
        let got = query_nvidia_smi_tracked(program, answered, |before| {
            asked = Some(before);
            deadline
        });
        (got, asked.expect("the call never chose a deadline"))
    }

    /// Only a reading counts as "has answered": a failed spawn, a program
    /// that ran but failed, one that printed garbage and one that ran out
    /// its deadline all leave the flag down. And once it is up, a later
    /// failure does not bring it back down. Each call's deadline is chosen
    /// from the flag as it stood before the call.
    #[test]
    fn only_a_reading_marks_nvidia_smi_as_answered() {
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-nvidia-smi");
        let missing = missing.to_str().unwrap();
        // Written by this process, but never executed: no write fd can race
        // an exec of it (see `script`).
        let not_executable = dir.path().join("not-executable");
        std::fs::write(
            &not_executable,
            "#!/bin/sh\necho 'NVIDIA Test, 1, 2, 3, 4'\n",
        )
        .unwrap();
        let not_executable = not_executable.to_str().unwrap();
        let fails = script(dir.path(), "fails", "exit 9");
        let garbage = script(dir.path(), "garbage", "echo 'not a csv line'");
        let hangs = script(dir.path(), "hangs", "exec sleep 30");
        let answers = script(
            dir.path(),
            "answers",
            "echo 'NVIDIA Test, 55, 40, 1024, 8192'",
        );
        let quick = Duration::from_secs(5);
        let short = Duration::from_millis(200);

        let answered = AtomicBool::new(false);
        let cases = [
            (
                missing,
                quick,
                NvidiaFailure::NotInstalled,
                "a program that is not there",
            ),
            (
                not_executable,
                quick,
                NvidiaFailure::Transient,
                "a failed spawn",
            ),
            (
                fails.as_str(),
                quick,
                NvidiaFailure::Transient,
                "a non-zero exit",
            ),
            (
                garbage.as_str(),
                quick,
                NvidiaFailure::Transient,
                "output that does not parse",
            ),
            (hangs.as_str(), short, NvidiaFailure::Transient, "a timeout"),
        ];
        for (program, deadline, want, what) in cases {
            let (got, asked) = tracked(program, &answered, deadline);
            assert_eq!(got.err(), Some(want), "{what}");
            assert!(
                !asked,
                "{what}: the deadline was chosen as if one had answered"
            );
            assert!(
                !answered.load(Relaxed),
                "{what} marked nvidia-smi as answered"
            );
        }

        let (got, asked) = tracked(&answers, &answered, quick);
        assert_eq!(got.expect("a reading").name, "NVIDIA Test");
        assert!(
            !asked,
            "the first reading's deadline was chosen as if one had answered"
        );
        assert!(
            answered.load(Relaxed),
            "a reading did not mark nvidia-smi as answered"
        );

        for (program, deadline, want, what) in cases {
            let (got, asked) = tracked(program, &answered, deadline);
            assert_eq!(got.err(), Some(want), "{what}, after a reading");
            assert!(asked, "{what}: the deadline forgot the earlier reading");
            assert!(answered.load(Relaxed), "{what} cleared the answered flag");
        }
    }

    /// Marks a re-exec'd child of one of this module's tests. Its value is
    /// the full path of the one `_inner` test that child runs; unset in an
    /// ordinary run, where every `_inner` test returns at once.
    const GPU_CHILD: &str = "HYTTE_SENSORS_GPU_TEST_CHILD";

    /// Printed by a child only after every assertion passed, so a stale
    /// `--exact` filter that runs nothing cannot pass for a success.
    const CHILD_OK: &str = "gpu-test-child-reached-the-end";

    /// True in the re-exec'd child that runs `inner`.
    fn is_child(inner: &str) -> bool {
        std::env::var_os(GPU_CHILD).is_some_and(|v| v == inner)
    }

    /// Re-exec this test binary to run just `inner`, with `PATH` replaced by
    /// `path` when given. Asserts that the child passed and returns its
    /// stdout and stderr.
    fn run_child(inner: &str, path: Option<&std::ffi::OsStr>) -> (String, String) {
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("this test binary"));
        child
            .args(["--exact", "--nocapture", "--test-threads=1", inner])
            .env(GPU_CHILD, inner);
        if let Some(path) = path {
            child.env("PATH", path);
        }
        let out = child.output().expect("re-exec this test binary");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "the child failed\n{stdout}\n{stderr}");
        (stdout, stderr)
    }

    const SLOW_FIRST_INNER: &str =
        "gpu::tests::the_real_reader_waits_for_a_slow_first_answer_only_inner";

    /// **`read_nvidia_gpu` itself** gives `nvidia-smi` the long deadline
    /// until it first answers, and the short one after. Pinned in a
    /// re-exec'd child, which has its own process-wide flag and a `PATH`
    /// whose `nvidia-smi` is a stand-in that takes 3 s to answer: between
    /// [`NVIDIA_SMI_DEADLINE`] and [`NVIDIA_SMI_FIRST_DEADLINE`].
    #[test]
    fn the_real_reader_waits_for_a_slow_first_answer_only() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "nvidia-smi",
            "sleep 3\necho 'NVIDIA Slow, 50, 10, 100, 1000'",
        );
        let path = path_with_stand_in(dir.path());
        let (stdout, stderr) = run_child(SLOW_FIRST_INNER, Some(&path));
        assert!(
            stdout.contains(CHILD_OK),
            "the child ran nothing\n{stdout}\n{stderr}"
        );
    }

    /// The body of [`the_real_reader_waits_for_a_slow_first_answer_only`]; a
    /// no-op outside its child.
    #[test]
    fn the_real_reader_waits_for_a_slow_first_answer_only_inner() {
        if !is_child(SLOW_FIRST_INNER) {
            return;
        }
        let started = Instant::now();
        let first = read_nvidia_gpu();
        let took = started.elapsed();
        assert_eq!(
            first.expect("the slow first call was killed").name,
            "NVIDIA Slow"
        );
        assert!(
            took > NVIDIA_SMI_DEADLINE,
            "the stand-in answered in {took:?}, so this proved nothing"
        );

        let started = Instant::now();
        let second = read_nvidia_gpu();
        let took = started.elapsed();
        assert_eq!(
            second.err(),
            Some(NvidiaFailure::Transient),
            "the slow call after a reading was not killed"
        );
        assert!(
            took < Duration::from_secs(3),
            "the call after a reading ran {took:?}: it did not get the short deadline"
        );
        println!("{CHILD_OK}");
    }

    /// `PATH` for a child whose `nvidia-smi` is the stand-in in `dir`. The
    /// stand-in's own `sleep` still has to resolve through the inherited one.
    fn path_with_stand_in(dir: &std::path::Path) -> std::ffi::OsString {
        let mut path = std::ffi::OsString::from(dir);
        if let Some(inherited) = std::env::var_os("PATH") {
            path.push(":");
            path.push(inherited);
        }
        path
    }

    /// Printed by the wrapper child when an AMD or Intel card answered, so
    /// the Nvidia arm, the only one that reads the end clock, never ran.
    const NVIDIA_ARM_NOT_REACHED: &str = "gpu-test-child-nvidia-arm-not-reached";

    const WRAPPER_END_CLOCK_INNER: &str =
        "gpu::tests::the_public_wrapper_counts_the_backoff_from_the_end_of_the_call_inner";

    /// **`read_gpu_with_cache` itself** counts a backoff from when the failed
    /// call returned. The seam tests above cannot see a wrapper that hands
    /// the composition its start instant as the end clock (#1643 review).
    /// Pinned in a re-exec'd child whose `nvidia-smi` is a stand-in that
    /// fails after 1 s, so the call's start and end are a second apart.
    ///
    /// The wrapper probes the real `/sys/class/drm` first. Where an AMD or
    /// Intel card answers, the Nvidia arm never runs (in production either),
    /// and this passes without checking anything, saying so on stderr. A
    /// host with no DRM card or only an Nvidia one, and a build sandbox with
    /// no `/sys`, reach the Nvidia arm.
    #[test]
    fn the_public_wrapper_counts_the_backoff_from_the_end_of_the_call() {
        let dir = tempfile::tempdir().unwrap();
        script(dir.path(), "nvidia-smi", "sleep 1\nexit 9");
        let path = path_with_stand_in(dir.path());
        let (stdout, stderr) = run_child(WRAPPER_END_CLOCK_INNER, Some(&path));
        if stdout.contains(NVIDIA_ARM_NOT_REACHED) {
            eprintln!("skipped: an AMD or Intel card answered first, so the Nvidia arm never ran");
            return;
        }
        assert!(
            stdout.contains(CHILD_OK),
            "the child ran nothing\n{stdout}\n{stderr}"
        );
    }

    /// The body of
    /// [`the_public_wrapper_counts_the_backoff_from_the_end_of_the_call`]; a
    /// no-op outside its child.
    #[test]
    fn the_public_wrapper_counts_the_backoff_from_the_end_of_the_call_inner() {
        if !is_child(WRAPPER_END_CLOCK_INNER) {
            return;
        }
        let before = Instant::now();
        let (state, cache) = read_gpu_with_cache(GpuCache::default());
        let after = Instant::now();
        if state.is_some() {
            println!("{NVIDIA_ARM_NOT_REACHED}");
            return;
        }
        let (not_before, delay) = cache
            .nvidia_backoff
            .expect("the failing stand-in left no retry gate");
        assert_eq!(delay, NVIDIA_BACKOFF_INITIAL);
        assert!(
            not_before >= before + Duration::from_secs(1) + NVIDIA_BACKOFF_INITIAL,
            "the gate opens {:?} after the call started: it was counted from before \
             the 1 s call returned",
            not_before.saturating_duration_since(before)
        );
        assert!(
            not_before <= after + NVIDIA_BACKOFF_INITIAL,
            "the gate was counted from after the call returned"
        );
        println!("{CHILD_OK}");
    }

    #[test]
    fn nvidia_unavailable_never_consults_cached_reading_or_forks() {
        let t0 = Instant::now();
        let cache = GpuCache {
            nvidia_available: Some(false),
            intel_rc6_prev: None,
            // Even a (hypothetically) still-present cached reading must not
            // surface once nvidia_available says "previously absent".
            nvidia_last: Some((fake_reading("must not surface"), t0)),
            nvidia_backoff: None,
        };
        let called = Cell::new(false);
        let (state, new_cache) = read_nvidia_instantly(cache, t0, || {
            called.set(true);
            Err(NvidiaFailure::NotInstalled)
        });
        assert!(
            state.is_none(),
            "nvidia_available == Some(false) must never surface a cached reading"
        );
        assert!(
            !called.get(),
            "nvidia_available == Some(false) must never fork nvidia-smi"
        );
        assert_eq!(new_cache.nvidia_available, Some(false));
    }

    /// #1297 review MEDIUM 3: the original collapse test above re-used
    /// `cache.clone()` (the cache the *first* call returned) on every
    /// iteration instead of threading each call's own returned cache into
    /// the next — so a hit branch that silently wiped the cache on the way
    /// out would still read as "answered from the cache" every time. Every
    /// real caller threads (`self.gpu = cache`, `state.gpu_cache = cache`),
    /// so this test's shape matches theirs: it feeds each call the cache the
    /// *previous* call handed back.
    #[test]
    fn a_cache_hit_threads_the_cache_forward() {
        let fork_count = Cell::new(0u32);
        let t0 = Instant::now();
        let (_, mut cache) = read_nvidia_instantly(GpuCache::default(), t0, || {
            fork_count.set(fork_count.get() + 1);
            Ok(fake_reading("fake nvidia"))
        });

        for step in 1..=5u64 {
            let now = t0 + Duration::from_millis(step * 80); // up to 400ms, well under the 1.5s TTL
            let (state, next) = read_nvidia_instantly(cache, now, || {
                fork_count.set(fork_count.get() + 1);
                Ok(fake_reading("should not be forked"))
            });
            cache = next;
            assert_eq!(fork_count.get(), 1, "call {step} forked again");
            assert_eq!(state.as_ref().map(|s| s.name.as_str()), Some("fake nvidia"));
            assert_eq!(
                cache.nvidia_available,
                Some(true),
                "call {step}: a hit must not forget the availability memo"
            );
            assert!(
                cache.nvidia_last.is_some(),
                "call {step}: a hit must keep the cached reading for the next caller"
            );
        }
    }

    // ── read_gpu_with_cache_at (#1297 review MEDIUM 2) ───────────────────────

    /// The *composed* entry point must hand the caller's cache to the Nvidia
    /// arm — a fresh `GpuCache::default()` there would defeat the whole TTL
    /// while every test above (which calls `read_nvidia_with_cache_at`
    /// directly) stayed green.
    #[test]
    fn composition_passes_the_callers_cache_to_the_nvidia_arm() {
        let t0 = Instant::now();
        let fork_count = Cell::new(0u32);
        let seeded = GpuCache {
            nvidia_available: Some(true),
            intel_rc6_prev: None,
            nvidia_last: Some((fake_reading("cached"), t0)),
            nvidia_backoff: None,
        };
        let (state, cache) = read_gpu_with_cache_at(
            seeded,
            t0 + Duration::from_millis(100),
            || None,
            |_| None,
            || {
                fork_count.set(fork_count.get() + 1);
                Ok(fake_reading("forked"))
            },
            || t0 + Duration::from_millis(100),
        );
        assert_eq!(
            fork_count.get(),
            0,
            "the composed fn refused the caller's cache"
        );
        assert_eq!(state.as_ref().map(|s| s.name.as_str()), Some("cached"));
        assert!(cache.nvidia_last.is_some());
    }

    /// The composed entry point hands the Nvidia arm the caller's end clock,
    /// not its start instant, so a backoff counts from when the failed call
    /// returned (#1643 review).
    #[test]
    fn composition_passes_the_end_clock_to_the_nvidia_arm() {
        let t0 = Instant::now();
        let ended = t0 + NVIDIA_SMI_FIRST_DEADLINE;
        let (_, cache) = read_gpu_with_cache_at(
            GpuCache::default(),
            t0,
            || None,
            |_| None,
            || Err(NvidiaFailure::Transient),
            || ended,
        );
        assert_eq!(
            cache.nvidia_backoff,
            Some((ended + NVIDIA_BACKOFF_INITIAL, NVIDIA_BACKOFF_INITIAL)),
            "the composed fn counted the backoff from the wrong instant"
        );
    }

    /// Probe order AMD → Intel → Nvidia, and an AMD hit never even calls the
    /// Intel or Nvidia arms.
    #[test]
    fn amd_wins_over_intel_and_nvidia() {
        let t0 = Instant::now();
        let forked = Cell::new(false);
        let (state, cache) = read_gpu_with_cache_at(
            GpuCache::default(),
            t0,
            || Some(fake_reading("amd")),
            |_| panic!("intel must not be consulted once AMD answered"),
            || {
                forked.set(true);
                Err(NvidiaFailure::Transient)
            },
            || t0,
        );
        assert_eq!(state.as_ref().map(|s| s.name.as_str()), Some("amd"));
        assert!(!forked.get(), "an AMD box must never fork nvidia-smi");
        assert_eq!(cache.nvidia_available, Some(false));
    }

    // ── GpuCache::forget_readings (#1297 review MEDIUM 1) ────────────────────

    #[test]
    fn forget_readings_forces_the_next_call_to_fork() {
        let t0 = Instant::now();
        let mut cache = GpuCache {
            nvidia_available: Some(true),
            intel_rc6_prev: Some((123, t0)),
            nvidia_last: Some((fake_reading("stale"), t0)),
            nvidia_backoff: None,
        };

        cache.forget_readings();
        assert!(
            cache.nvidia_last.is_none(),
            "forget_readings must drop the Nvidia reading"
        );
        assert!(
            cache.intel_rc6_prev.is_none(),
            "forget_readings must drop the Intel RC6 delta base"
        );
        assert_eq!(
            cache.nvidia_available,
            Some(true),
            "forget_readings must keep the availability memo — re-learning it is the fork it exists to avoid"
        );

        let fork_count = Cell::new(0u32);
        // Still well within what would have been the TTL window measured
        // from `t0` — a forgotten reading must fork anyway, which is the
        // whole point of the seam `Sampler::reset` calls on a park/unpark
        // edge (a stale pre-close reading must never survive the park).
        let now = t0 + Duration::from_millis(50);
        let (state, _) = read_nvidia_instantly(cache, now, || {
            fork_count.set(fork_count.get() + 1);
            Ok(fake_reading("fresh"))
        });
        assert_eq!(
            fork_count.get(),
            1,
            "a forgotten reading must force a fresh fork even inside the old TTL window"
        );
        assert_eq!(state.as_ref().map(|s| s.name.as_str()), Some("fresh"));
    }

    // ── read_gpu_with_cache, the PUBLIC wrapper (#1297 review, second round) ─

    /// The public wrapper hands ITS caller's cache to the composition — pinned
    /// against the real sysfs arms because a `GpuCache::default()` at
    /// `read_gpu_with_cache`'s own call site defeats the TTL in production while
    /// every seam-level test above stays green (#1297 review, second round).
    /// Reads the real `/sys/class/drm` and, only on a box with `nvidia-smi` on
    /// PATH, forks it once — no display, no bus, so it stays in the default bucket.
    #[test]
    fn the_public_wrapper_threads_the_callers_cache_through() {
        let now = Instant::now();
        let seeded = GpuCache {
            nvidia_available: Some(true),
            intel_rc6_prev: None,
            nvidia_last: Some((fake_reading("cached"), now)),
            nvidia_backoff: None,
        };
        let (state, cache) = read_gpu_with_cache(seeded);
        // Every arm preserves an existing `Some(true)`: AMD/Intel via `.or(...)`,
        // the Nvidia arm via a TTL hit. A default cache yields `Some(false)` on an
        // AMD box, an Intel box, and a box with no GPU and no nvidia-smi (CI).
        assert_eq!(
            cache.nvidia_available,
            Some(true),
            "the wrapper dropped the caller's availability memo"
        );
        // Every arm carries the reading through: AMD/Intel copy it, a hit keeps it.
        assert!(
            cache.nvidia_last.is_some(),
            "the wrapper dropped the caller's last reading"
        );
        // On a box where nvidia-smi actually answers, a hit inside the TTL must
        // serve the seeded reading, not a fresh fork's.
        if let Some(s) = &state
            && s.vendor == GpuVendor::Nvidia
        {
            assert_eq!(
                s.name, "cached",
                "the wrapper forked instead of serving the cached reading"
            );
        }
    }

    // ── the reaper (no killed child is ever left un-waited) ──────────────────

    /// True once `/proc/<pid>` no longer exists, i.e. the child was waited on.
    /// A killed-but-unreaped child lingers there in state `Z`.
    fn is_reaped(pid: u32) -> bool {
        !std::path::Path::new(&format!("/proc/{pid}/stat")).exists()
    }

    fn wait_reaped(pid: u32, within: Duration) -> bool {
        let end = Instant::now() + within;
        while Instant::now() < end {
            if is_reaped(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        is_reaped(pid)
    }

    fn sleeper() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_killed_child_handed_to_the_reaper_is_waited_on() {
        let mut child = sleeper();
        let pid = child.id();
        child.kill().unwrap();
        reap_later(child);
        assert!(
            wait_reaped(pid, Duration::from_secs(5)),
            "zombie left behind"
        );
    }

    /// The regression: with one reaper per outstanding child, a second killed
    /// child arriving while the first is stuck was dropped as a zombie. One
    /// stuck (still-running) child must not hold up reaping the next.
    #[test]
    fn a_stuck_child_does_not_hold_up_reaping_the_next() {
        let stuck = sleeper();
        let stuck_pid = stuck.id();
        reap_later(stuck);

        let mut killed = sleeper();
        let killed_pid = killed.id();
        killed.kill().unwrap();
        reap_later(killed);
        assert!(
            wait_reaped(killed_pid, Duration::from_secs(5)),
            "a later killed child stayed a zombie behind a stuck one"
        );
        assert!(!is_reaped(stuck_pid), "the live child was disturbed");

        // Let it go: the reaper must collect it too.
        let _ = std::process::Command::new("kill")
            .arg(stuck_pid.to_string())
            .status();
        assert!(
            wait_reaped(stuck_pid, Duration::from_secs(5)),
            "the released child was never reaped"
        );
    }

    /// True while `/proc/<pid>` shows state `Z`: killed, not yet waited on.
    fn is_zombie(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                s.rsplit_once(')')
                    .map(|(_, rest)| rest.trim_start().starts_with('Z'))
            })
            .unwrap_or(false)
    }

    fn wait_zombie(pid: u32) {
        let end = Instant::now() + Duration::from_secs(5);
        while !is_zombie(pid) {
            assert!(Instant::now() < end, "child {pid} never exited");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// `admit` enforces the cap, and only children still running count
    /// toward it: finished ones are retired first, so a full list of them
    /// never turns a new child away (#1561).
    #[test]
    fn admit_caps_running_children_and_retires_finished_ones_first() {
        let mut pending = Vec::new();
        assert!(admit(&mut pending, sleeper(), 2));
        assert!(admit(&mut pending, sleeper(), 2));
        let mut over = sleeper();
        let over_pid = over.id();
        over.kill().unwrap();
        wait_zombie(over_pid);
        assert!(
            !admit(&mut pending, over, 2),
            "a third live child was kept past cap 2"
        );
        assert_eq!(pending.len(), 2, "the cap did not bound the list");
        assert!(is_zombie(over_pid), "the over-cap child was waited on");

        // Both kept children finish: they must not count against the cap.
        let pids: Vec<u32> = pending.iter().map(std::process::Child::id).collect();
        for c in &mut pending {
            c.kill().unwrap();
        }
        for pid in pids {
            wait_zombie(pid);
        }
        assert!(
            admit(&mut pending, sleeper(), 2),
            "finished children counted toward the cap"
        );
        assert_eq!(pending.len(), 1, "finished children were not retired");
        for c in &mut pending {
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }

    /// The cap on the real loop: `REAPER_MAX_PENDING` live children fill the
    /// list, so one more (killed) child is dropped un-waited and stays a
    /// zombie, while a child sent after the list drained is reaped.
    #[test]
    fn the_reaper_loop_stops_tracking_past_the_cap() {
        // The test fills the real cap with live processes, so a cap raised
        // far past today's is a failure here, not a process flood.
        let cap = std::hint::black_box(REAPER_MAX_PENDING);
        assert!(
            cap <= 256,
            "REAPER_MAX_PENDING is {cap}: the list is no longer bounded tightly"
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let reaper = std::thread::spawn(move || reaper_loop(&rx));
        let live: Vec<u32> = (0..REAPER_MAX_PENDING)
            .map(|_| {
                let c = sleeper();
                let pid = c.id();
                tx.send(c).unwrap();
                pid
            })
            .collect();

        let mut over = sleeper();
        let over_pid = over.id();
        over.kill().unwrap();
        wait_zombie(over_pid);
        tx.send(over).unwrap();
        std::thread::sleep(REAPER_POLL * 6);
        assert!(
            is_zombie(over_pid),
            "a child past REAPER_MAX_PENDING was reaped: the cap is not enforced"
        );

        // Finish the tracked ones; the loop collects them all.
        let _ = std::process::Command::new("kill")
            .args(live.iter().map(u32::to_string))
            .status();
        for pid in &live {
            assert!(
                wait_reaped(*pid, Duration::from_secs(10)),
                "child {pid} never reaped"
            );
        }
        // The drained list has room again.
        let mut again = sleeper();
        let again_pid = again.id();
        again.kill().unwrap();
        tx.send(again).unwrap();
        assert!(
            wait_reaped(again_pid, Duration::from_secs(5)),
            "a child after the list drained was not reaped"
        );
        drop(tx);
        reaper.join().unwrap();
    }

    /// A reaper that is not running (spawn failed once, or its thread died)
    /// is replaced on the next kill instead of staying off for the process.
    #[test]
    fn a_dead_reaper_inbox_is_replaced_on_the_next_kill() {
        // A local slot: the process-wide `REAPER` is shared with sibling
        // tests, and replacing its sender would strand their children.
        let (dead_tx, dead_rx) = std::sync::mpsc::channel();
        drop(dead_rx);
        let slot = std::sync::Mutex::new(Some(dead_tx));

        let mut child = sleeper();
        let pid = child.id();
        child.kill().unwrap();
        reap_later_via(&slot, child);
        assert!(
            wait_reaped(pid, Duration::from_secs(5)),
            "a dead reaper inbox was never replaced"
        );
        assert!(
            slot.lock().unwrap().is_some(),
            "the replacement inbox was not kept"
        );
        // Dropping the slot drops the only sender; the new reaper exits.
    }

    /// With every sender gone the reaper still waits on the children it
    /// already holds, instead of dropping them un-waited.
    #[test]
    fn a_reaper_whose_senders_are_gone_still_reaps_what_it_holds() {
        let (tx, rx) = std::sync::mpsc::channel();
        let reaper = std::thread::spawn(move || reaper_loop(&rx));
        let pid = send_live(&tx);
        // Let the loop take it and enter its busy wait, then disconnect.
        std::thread::sleep(REAPER_POLL * 2);
        drop(tx);
        std::thread::sleep(REAPER_POLL * 2);
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
        assert!(
            wait_reaped(pid, Duration::from_secs(5)),
            "a held child was dropped un-waited when the senders went away"
        );
        reaper.join().unwrap();
    }

    /// Wait until `pid` is no longer running: a zombie, or already reaped.
    fn wait_exited(pid: u32) {
        let end = Instant::now() + Duration::from_secs(5);
        while !(is_zombie(pid) || is_reaped(pid)) {
            assert!(Instant::now() < end, "child {pid} never exited");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn send_live(tx: &std::sync::mpsc::Sender<std::process::Child>) -> u32 {
        let c = sleeper();
        let pid = c.id();
        tx.send(c).unwrap();
        pid
    }

    /// The loop's call site, not just `admit`: a tracked child that finished
    /// while the loop sat in its `REAPER_POLL` wait must be retired before the
    /// next arrival is counted against the cap (#1561). Each round refills the
    /// list to the cap with a live child (that message also restarts the wait),
    /// then one tracked child finishes and a killed child arrives well inside
    /// the same wait, so no timeout pass retires the finished one first.
    #[test]
    fn the_reaper_loop_retires_a_finished_child_before_counting_the_cap() {
        let cap = std::hint::black_box(REAPER_MAX_PENDING);
        assert!(cap <= 256, "REAPER_MAX_PENDING is {cap}");
        let (tx, rx) = std::sync::mpsc::channel();
        let reaper = std::thread::spawn(move || reaper_loop(&rx));
        let mut live: Vec<u32> = (0..cap - 1).map(|_| send_live(&tx)).collect();
        for round in 0..3 {
            let mut late = sleeper();
            let late_pid = late.id();
            late.kill().unwrap();
            wait_zombie(late_pid);
            // Back to the cap; this message starts a fresh REAPER_POLL wait.
            live.push(send_live(&tx));
            let started = Instant::now();
            let done = live.remove(0);
            let _ = std::process::Command::new("kill")
                .arg(done.to_string())
                .status();
            wait_exited(done);
            tx.send(late).unwrap();
            let took = started.elapsed();
            assert!(
                wait_reaped(late_pid, Duration::from_secs(2)),
                "round {round}: a child arriving just after a tracked one finished was \
                 refused, so finished children counted toward the cap (round took {took:?}, \
                 REAPER_POLL is {REAPER_POLL:?})"
            );
        }
        let _ = std::process::Command::new("kill")
            .args(live.iter().map(u32::to_string))
            .status();
        for pid in &live {
            assert!(
                wait_reaped(*pid, Duration::from_secs(10)),
                "child {pid} never reaped"
            );
        }
        drop(tx);
        reaper.join().unwrap();
    }

    /// End to end through the deadline path: the child that missed its
    /// deadline is killed and gone from `/proc`, not left in state `Z`.
    #[test]
    fn query_nvidia_smi_leaves_no_zombie_after_a_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let p = script(
            dir.path(),
            "smi",
            &format!("echo $$ > {}\nexec sleep 30", pidfile.display()),
        );
        let got = query_nvidia_smi(&p, Duration::from_millis(500));
        assert_eq!(got.err(), Some(NvidiaFailure::Transient));
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .expect("the script ran")
            .trim()
            .parse()
            .unwrap();
        assert!(
            wait_reaped(pid, Duration::from_secs(5)),
            "zombie left behind"
        );
    }

    /// Voluntary context switches of one thread, from `<task>/status`.
    fn voluntary_switches(task: &std::path::Path) -> u64 {
        std::fs::read_to_string(task.join("status"))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap()
    }

    /// An idle reaper sleeps in `recv()`: it must not wake every
    /// `REAPER_POLL` for the life of the process, on every machine.
    ///
    /// Counts the thread's `voluntary_ctxt_switches` (each block-and-wake is
    /// one) over a 2 s idle window. Measured: 0-1 for the shipped loop, and
    /// about 9 per second (~19 here) with the idle arm mutated into the
    /// 100 ms poll. The threshold of 5 leaves room for a stray wakeup on a
    /// loaded runner and is still far below the mutant. Runs `reaper_loop` on
    /// a private channel so sibling tests' children cannot keep it busy.
    #[test]
    fn an_idle_reaper_does_not_wake() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (task_tx, task_rx) = std::sync::mpsc::channel();
        let reaper = std::thread::spawn(move || {
            task_tx
                .send(std::fs::read_link("/proc/thread-self").unwrap())
                .unwrap();
            reaper_loop(&rx);
        });
        let task = std::path::Path::new("/proc").join(task_rx.recv().unwrap());
        // One busy spell first, so the loop has been through its polling arm
        // and must find its way back to the blocking one.
        let mut child = sleeper();
        let pid = child.id();
        child.kill().unwrap();
        tx.send(child).unwrap();
        assert!(
            wait_reaped(pid, Duration::from_secs(5)),
            "zombie left behind"
        );
        std::thread::sleep(REAPER_POLL * 3);
        let before = voluntary_switches(&task);
        std::thread::sleep(REAPER_POLL * 20);
        let woke = voluntary_switches(&task) - before;
        assert!(woke < 5, "an idle reaper woke {woke} times in 2 s");
        drop(tx);
        reaper.join().unwrap();
    }

    // ── spawn failures, induced with RLIMIT_NPROC (#1561) ────────────────────

    /// Every `WARN` event's message. A re-exec'd child installs it as its
    /// global default: the child runs one test, so nothing else logs there.
    struct Warnings(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl tracing::Subscriber for Warnings {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            if *event.metadata().level() != tracing::Level::WARN {
                return;
            }
            let mut message = String::new();
            event.record(
                &mut |field: &tracing::field::Field, value: &dyn std::fmt::Debug| {
                    if field.name() == "message" {
                        message = format!("{value:?}");
                    }
                },
            );
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(message);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Printed by the spawn-failure child when `RLIMIT_NPROC` does not bind
    /// this uid, so no spawn can be made to fail.
    const NPROC_NOT_ENFORCED: &str = "gpu-test-child-rlimit-nproc-does-not-bind";

    const SPAWN_FAILURE_INNER: &str =
        "gpu::tests::spawn_failures_warn_once_per_streak_and_leak_nothing_inner";

    /// What a failed spawn does, for both spawns on the GPU path: the
    /// `nvidia-smi` child and the reaper thread.
    ///
    /// The failures are real. A soft `RLIMIT_NPROC` of 1 makes every
    /// `fork`/`clone` fail with `EAGAIN` for any uid that already has a task:
    /// the kernel adds the task being created to the uid's count
    /// (`copy_creds`) before it checks `count > limit` against the calling
    /// process's limit. This child has at least two tasks of its own anyway
    /// (its main thread and libtest's test thread). The limit belongs to one
    /// process, though, and lowering it in this shared test binary would
    /// fail every sibling test that forks. So it is lowered in a re-exec'd
    /// child that runs only the `_inner` test below. Other processes of the
    /// same uid keep their own limits: more of them only puts the uid
    /// further over 1, which changes nothing here.
    ///
    /// The kernel exempts a task with `CAP_SYS_RESOURCE` or `CAP_SYS_ADMIN`
    /// in the initial user namespace (in practice: real root). There the
    /// child finds it can still start a thread and says so. Unset
    /// `TROLLSHELL_REQUIRE_NPROC`, this test then passes without checking
    /// anything; its `skipped:` line goes to stderr, which libtest discards
    /// for a passing test unless run with `--nocapture`, so the skip is
    /// silent. With `TROLLSHELL_REQUIRE_NPROC=1` it fails instead, on the
    /// `TROLLSHELL_REQUIRE_GL`/`TROLLSHELL_REQUIRE_TZDATA` precedent.
    /// `checks.workspace-tests` sets it: the nix build sandbox runs tests as
    /// an unprivileged build user, so the limit binds there.
    #[test]
    fn spawn_failures_warn_once_per_streak_and_leak_nothing() {
        let (stdout, stderr) = run_child(SPAWN_FAILURE_INNER, None);
        if stdout.contains(NPROC_NOT_ENFORCED) {
            let required =
                std::env::var_os("TROLLSHELL_REQUIRE_NPROC").is_some_and(|want| want == "1");
            assert!(
                !required,
                "TROLLSHELL_REQUIRE_NPROC=1, but RLIMIT_NPROC does not bind this uid (root, or \
                 CAP_SYS_RESOURCE/CAP_SYS_ADMIN in the initial user namespace), so no spawn \
                 could be made to fail"
            );
            eprintln!(
                "skipped: RLIMIT_NPROC does not bind this uid (root?), so no spawn can be \
                 made to fail"
            );
            return;
        }
        assert!(
            stdout.contains(CHILD_OK),
            "the child ran nothing\n{stdout}\n{stderr}"
        );
    }

    /// The body of [`spawn_failures_warn_once_per_streak_and_leak_nothing`];
    /// a no-op outside its child, which is the only process that ever
    /// lowers its `RLIMIT_NPROC`.
    #[test]
    fn spawn_failures_warn_once_per_streak_and_leak_nothing_inner() {
        use nix::sys::resource::{Resource, getrlimit, setrlimit};
        if !is_child(SPAWN_FAILURE_INNER) {
            return;
        }
        let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        tracing::subscriber::set_global_default(Warnings(std::sync::Arc::clone(&warnings)))
            .expect("the child's only subscriber");
        let warned = || warnings.lock().unwrap().clone();

        // Everything this test forks, it forks before the limit goes down.
        let dir = tempfile::tempdir().unwrap();
        let smi = script(dir.path(), "smi", "echo 'NVIDIA Test, 55, 40, 1024, 8192'");
        let killed = || {
            let mut c = sleeper();
            let pid = c.id();
            c.kill().unwrap();
            wait_zombie(pid);
            (c, pid)
        };
        let (first, first_pid) = killed();
        let (second, second_pid) = killed();
        let (after, after_pid) = killed();
        let (again, again_pid) = killed();

        let (soft, hard) = getrlimit(Resource::RLIMIT_NPROC).unwrap();
        let lower = || setrlimit(Resource::RLIMIT_NPROC, 1, hard).expect("lower RLIMIT_NPROC");
        let restore =
            || setrlimit(Resource::RLIMIT_NPROC, soft, hard).expect("restore RLIMIT_NPROC");

        lower();
        if let Ok(probe) = std::thread::Builder::new().spawn(|| {}) {
            restore();
            probe.join().unwrap();
            println!("{NPROC_NOT_ENFORCED}");
            return;
        }

        // 1. `nvidia-smi` cannot be spawned: a transient failure (retried
        // with backoff), no panic, no child, so nothing for the reaper and
        // no reaper started.
        assert_eq!(
            query_nvidia_smi(&smi, Duration::from_secs(5)).err(),
            Some(NvidiaFailure::Transient),
            "a failed nvidia-smi spawn"
        );
        assert!(
            REAPER.lock().unwrap().is_none(),
            "a failed nvidia-smi spawn started a reaper"
        );
        assert_eq!(
            warned(),
            Vec::<String>::new(),
            "a failed nvidia-smi spawn warned"
        );

        // 2. The reaper thread cannot start, for two kills in a row: one
        // warning for the streak, no inbox kept, and both children dropped
        // un-waited (the documented cost: zombies until the process exits).
        reap_later(first);
        reap_later(second);
        restore();
        let lines = warned();
        assert_eq!(lines.len(), 1, "want one warning per streak: {lines:?}");
        assert!(
            lines[0].contains("could not start the nvidia-smi reaper"),
            "{lines:?}"
        );
        assert!(
            REAPER.lock().unwrap().is_none(),
            "a failed reaper spawn left an inbox behind"
        );
        std::thread::sleep(REAPER_POLL * 3);
        assert!(
            is_zombie(first_pid) && is_zombie(second_pid),
            "a child whose reaper could not start was waited on after all"
        );

        // 3. The next kill once threads can start: a reaper starts and
        // reaps it, with no further warning.
        reap_later(after);
        assert!(
            wait_reaped(after_pid, Duration::from_secs(5)),
            "no reaper was started once spawning worked again"
        );
        assert!(
            REAPER.lock().unwrap().is_some(),
            "the new inbox was not kept"
        );
        assert_eq!(warned().len(), 1, "a working spawn warned");

        // 4. That success ended the streak, so the next failure warns again.
        // (A dead inbox forces the respawn: the live reaper would take it.)
        let (dead_tx, dead_rx) = std::sync::mpsc::channel();
        drop(dead_rx);
        *REAPER.lock().unwrap() = Some(dead_tx);
        lower();
        reap_later(again);
        restore();
        assert_eq!(warned().len(), 2, "a second streak did not warn");
        assert!(
            is_zombie(again_pid),
            "the second streak's child was waited on"
        );
        println!("{CHILD_OK}");
    }
}
