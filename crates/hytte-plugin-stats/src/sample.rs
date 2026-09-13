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

use std::path::PathBuf;
use std::time::Duration;

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
    /// Overall CPU load, `0.0..=1.0`.
    pub cpu: f32,
    /// Per-logical-core load, `0.0..=1.0`, in the kernel's core order. Empty
    /// before the first delta is available (the first tick has no previous
    /// sample to subtract).
    pub per_core: Vec<f32>,
    /// CPU package temperature in °C, or `None` when no hwmon chip answers.
    pub cpu_temp_c: Option<f32>,
    /// The GPU, or `None` when there is none to read — which is what makes the
    /// GPU half of the card hide itself.
    pub gpu: Option<Gpu>,
}

/// The GPU half of a [`Snapshot`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gpu {
    /// Free-form adapter name, as the vendor reports it.
    pub name: String,
    /// Load, `0.0..=1.0`, or `None` when the vendor exposes no busy counter.
    pub load: Option<f32>,
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

/// The stateful half of sampling: the per-tick caches `hytte_sensors` asks the
/// caller to carry.
///
/// All three are "remember the last answer so the next one is cheap or
/// possible at all": `/proc/stat` is cumulative so a *load* needs the previous
/// reading, the hwmon chip directory costs a `read_dir` walk to resolve, and
/// the GPU cache remembers whether `nvidia-smi` exists at all.
#[derive(Debug, Default)]
pub struct Sampler {
    /// Previous `/proc/stat` (busy, total) per core.
    prev_cpu: Vec<(u64, u64)>,
    /// The resolved `/sys/class/hwmon` chip directory, once found.
    hwmon: Option<PathBuf>,
    /// `nvidia-smi` availability and the Intel RC6 delta base.
    gpu: GpuCache,
}

impl Sampler {
    /// A sampler with cold caches. The first [`Self::tick`] has no previous
    /// `/proc/stat` to subtract, so its `per_core` is empty and its `cpu` is
    /// `0.0` — the card renders dashes for one tick rather than a wrong number.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
        let load = hytte_sensors::compute_cpu_load(&self.prev_cpu, &now);
        // Only replace the baseline with a reading we actually got: an
        // `Err` from `/proc/stat` (which should not happen on Linux, but is a
        // plain `io::Error` away) would otherwise wipe the baseline and cost
        // the *next* tick its delta too.
        if !now.is_empty() {
            self.prev_cpu = now;
        }

        let temp = hytte_sensors::read_cpu_temp(&mut self.hwmon);
        let (gpu, cache) = hytte_sensors::read_gpu_with_cache(self.gpu);
        self.gpu = cache;

        Snapshot {
            cpu: as_unit(load.overall),
            per_core: load.per_core.iter().copied().map(as_unit).collect(),
            cpu_temp_c: temp.package_celsius.map(celsius),
            gpu: gpu.map(|g| Gpu {
                name: g.name,
                load: g.load.map(as_unit),
            }),
        }
    }
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
pub async fn sampler_task(cmds: CmdReceiver<Cmd>, msgs: CmdSender<Msg>, period: Duration) {
    let mut gate = Gate::new(cmds, period, |cmd: &Cmd| match cmd {
        Cmd::SetVisible(visible) => Some(*visible),
    });
    let mut sampler = Sampler::new();
    while let Some(wake) = gate.next().await {
        match wake {
            Wake::Refresh => {
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
                        sampler = Sampler::new();
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
    use super::{Cmd, Msg, Sampler, Snapshot, as_unit, sampler_task};
    use hytte_plugin::cmd_channel;
    use std::time::Duration;

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
    #[allow(clippy::float_cmp)]
    #[test]
    fn the_default_snapshot_is_the_nothing_yet_state() {
        let s = Snapshot::default();
        assert_eq!(s.cpu, 0.0);
        assert!(s.per_core.is_empty());
        assert!(s.cpu_temp_c.is_none());
        assert!(s.gpu.is_none());
    }

    /// A [`Sampler`] can be constructed with cold caches and costs nothing
    /// until it is ticked — which is what lets the task own one before the
    /// gate has ever opened.
    #[test]
    fn a_fresh_sampler_has_cold_caches() {
        let s = Sampler::new();
        assert!(format!("{s:?}").contains("prev_cpu: []"));
    }

    /// **The gate**: while the surface is hidden, nothing is sampled at all.
    ///
    /// Virtual time, so "ten periods went by and nothing happened" is a
    /// statement about the gate rather than about how long the test slept. The
    /// task is never told the surface is visible, so it must never reach a
    /// `spawn_blocking` — which is also what keeps this test off `/proc`.
    ///
    /// **Falsified** by dropping the `, if visible` guard inside
    /// `hytte_plugin::poll::Gate` (this then samples ten times).
    #[tokio::test(start_paused = true)]
    async fn a_hidden_card_never_samples() {
        let (cmd_tx, cmd_rx) = cmd_channel::<Cmd>();
        let (msg_tx, mut msg_rx) = cmd_channel::<Msg>();
        let task = tokio::spawn(sampler_task(cmd_rx, msg_tx, Duration::from_secs(1)));

        cmd_tx.send(Cmd::SetVisible(false)).expect("lane is live");
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;

        assert!(
            msg_rx.try_recv().is_err(),
            "a hidden card must not sample at all",
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
        let task = tokio::spawn(sampler_task(cmd_rx, msg_tx, Duration::from_secs(1)));
        drop(cmd_tx);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the task must return when its lane closes")
            .expect("and not by panicking");
    }
}
