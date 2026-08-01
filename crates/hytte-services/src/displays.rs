//! Display / output enumeration via niri's IPC.
//!
//! Owns the process's **single** `Request::Outputs` poller (#655): one socket
//! round-trip every 2 s, published as a raw snapshot that every consumer of
//! niri's output topology subscribes to. This module derives the public
//! `Vec<Output>` view from it; [`crate::display_config`] (the
//! `org.gnome.Mutter.DisplayConfig` shim) derives its `MonitorsChanged`
//! fingerprint from the same snapshot rather than running a second poller.
//!
//! `outputs()` emits a deduped `Vec<Output>` on topology change (plug/unplug,
//! mode switch, scale change, etc.). Persistent per-profile layout lives in
//! `kanshi` (see `etc/kanshi/`); this service only reflects niri's runtime
//! state.
//!
//! v1 is read-only with one exception: `set_output_enabled` toggles a single
//! connector on/off via `Request::Output { OutputAction::On | Off }`,
//! fire-and-forget. The next poll picks up the resulting state.
//!
//! Polling (vs subscribing to a niri event stream) is the v1 choice for
//! simplicity — niri 26.4 pushes no output event on its IPC event stream — and
//! the lag is fine for a passive page. Swap [`poll_loop`]'s body to upgrade;
//! that one edit now covers both consumers.
//!
//! Kept separate from `niri.rs` because that module owns the long-lived
//! event-stream socket; outputs aren't pushed on that stream so the polling
//! shape lives here.

use futures_signals::signal::{Mutable, Signal, SignalExt};
use futures_util::StreamExt;
use hytte_reactive::{Service, registry, runtime, shared, spawn_supervised};
use niri_ipc::socket::Socket;
use niri_ipc::{OutputAction, Request, Response, Transform as NiriTransform};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

// ── Public data types ────────────────────────────────────────────────────────

/// One connected output as exposed by niri.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    /// Connector name, e.g. `eDP-1`, `HDMI-A-1`. Stable identifier.
    pub name: String,
    /// Manufacturer string from EDID. May be empty for virtual outputs.
    pub make: String,
    /// Model string from EDID. May be empty.
    pub model: String,
    /// Active mode, or `None` when the output is disabled / no mode set.
    pub mode: Option<Mode>,
    /// Whether the output is currently driving pixels. False ⇒ disabled.
    pub enabled: bool,
    /// Logical scale factor (e.g. 1.0, 1.5, 2.0). 1.0 when disabled.
    pub scale: f64,
    /// Output transform as a stable string (`"normal"`, `"90"`, `"flipped"`).
    pub transform: String,
}

/// Active mode of an output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// Refresh rate in millihertz (matches niri-ipc; divide by 1000 for Hz).
    pub refresh_mhz: u32,
}

// ── The one shared niri `Outputs` source (#655) ──────────────────────────────

/// A raw niri outputs snapshot, keyed by connector name (`Output::name`, which
/// is also the key niri replies with). `BTreeMap` so iteration is
/// connector-sorted for every consumer.
pub(crate) type OutputMap = BTreeMap<String, niri_ipc::Output>;

/// Cross-thread handle bag for the single `Request::Outputs` poller.
///
/// Published through [`hytte_reactive::shared`] rather than the thread-local
/// registry because both subscribers are tokio tasks, and because it must be
/// reachable whether or not `displays::service()` itself was registered.
pub(crate) struct NiriOutputsShared {
    /// Latest raw snapshot. Republished on **every** tick — see [`poll_loop`]
    /// for why it isn't deduped here.
    pub(crate) snapshot: Mutable<Arc<OutputMap>>,
}

/// Serializes [`niri_outputs_source`]'s get-or-init so two callers can never
/// race into two pollers. In practice both callers are `Service::start` on the
/// GTK main thread, but the lock makes the function correct from anywhere.
static SOURCE_INIT: Mutex<()> = Mutex::new(());

/// Get the shared raw-outputs source, starting the single poller on first call.
///
/// Idempotent: the second caller reuses the running poller. Deliberately keyed
/// through [`hytte_reactive::shared`] rather than a `OnceLock` so
/// `registry::reset_for_tests` clears it — a `OnceLock` would hand a second
/// in-process `App` run the first run's dead handle (see `shared`'s docs).
pub(crate) fn niri_outputs_source() -> Arc<NiriOutputsShared> {
    let _guard = SOURCE_INIT.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(existing) = shared::get::<NiriOutputsShared>() {
        return existing;
    }

    let snapshot = Mutable::new(Arc::new(OutputMap::new()));
    let writer = snapshot.clone();
    shared::insert(NiriOutputsShared { snapshot });

    spawn_supervised("niri-outputs", move || {
        let writer = writer.clone();
        async move {
            poll_loop(writer).await;
        }
    });

    shared::get::<NiriOutputsShared>().expect("just inserted")
}

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct DisplaysHandles {
    pub(crate) outputs: Mutable<Vec<Output>>,
}

impl Default for DisplaysHandles {
    fn default() -> Self {
        Self {
            outputs: Mutable::new(Vec::new()),
        }
    }
}

pub struct DisplayService;

impl Service for DisplayService {
    type Handles = DisplaysHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = DisplaysHandles::default();
        let writer = handles.outputs.clone();
        let source = niri_outputs_source();

        spawn_supervised("displays", move || {
            let snapshots = source.snapshot.clone();
            let writer = writer.clone();
            async move {
                derive_loop(snapshots, writer).await;
            }
        });

        handles
    }
}

#[must_use]
pub fn service() -> DisplayService {
    DisplayService
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Signal of the connected outputs, sorted by connector name. Emits only
/// when the value actually changes (`PartialEq` dedup at write time).
pub fn outputs() -> impl Signal<Item = Vec<Output>> {
    registry::with(|r| {
        r.get::<DisplaysHandles>()
            .expect("displays::service() not registered")
            .outputs
            .signal_cloned()
    })
}

/// Toggle one output on or off. Fire-and-forget — the change is reflected
/// in the next polled `outputs()` emission, typically within 2 s. Errors
/// (unknown connector, IPC failure) are logged at warn and dropped.
pub fn set_output_enabled(name: &str, on: bool) {
    let name = name.to_string();
    runtime::handle().spawn_blocking(move || {
        let action = if on {
            OutputAction::On
        } else {
            OutputAction::Off
        };
        if let Err(e) = send_output_action(&name, action) {
            tracing::warn!(output = %name, on, error = %e, "displays: output toggle failed");
        }
    });
}

fn send_output_action(name: &str, action: OutputAction) -> anyhow::Result<()> {
    let mut sock = Socket::connect().map_err(|e| anyhow::anyhow!("connect: {e}"))?;
    let req = Request::Output {
        output: name.to_string(),
        action,
    };
    match sock.send(req).map_err(|e| anyhow::anyhow!("send: {e}"))? {
        Ok(_) => Ok(()),
        Err(msg) => Err(anyhow::anyhow!("niri: {msg}")),
    }
}

// ── Polling loop ─────────────────────────────────────────────────────────────

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The one niri `Outputs` poller. Publishes each raw snapshot to `writer`.
///
/// The snapshot is republished **unconditionally**, without a dedup compare:
/// niri's `Output` isn't `PartialEq`, and the two subscribers key their own
/// change detection off *different* projections of it (this module compares the
/// converted [`Output`] list, which carries make/model but not position;
/// `display_config` compares a fingerprint that carries position but not
/// make/model). Any single dedup key here would therefore be able to swallow a
/// change one of them cares about. Each subscriber already compared once per
/// tick before this consolidation, so the per-tick cost is unchanged — what
/// went away is the second socket connect.
async fn poll_loop(writer: Mutable<Arc<OutputMap>>) {
    loop {
        // niri-ipc's Socket is a blocking unix-socket client; run it on a
        // blocking pool so we don't park a tokio worker for the round-trip.
        let snapshot = tokio::task::spawn_blocking(query_outputs_raw)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "displays: niri Outputs poll task panicked");
                OutputMap::new()
            });

        writer.set(Arc::new(snapshot));

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Derive the public [`outputs`] view from the shared raw snapshot.
///
/// Split out of `Service::start` so the dedup semantics are testable without a
/// live niri. `PartialEq` dedup: only write when the derived list actually
/// differs from the cached one, otherwise downstream signals re-fire for no
/// reason every 2 s.
async fn derive_loop(snapshots: Mutable<Arc<OutputMap>>, writer: Mutable<Vec<Output>>) {
    let mut stream = snapshots.signal_cloned().to_stream();
    while let Some(raw) = stream.next().await {
        let next = convert_snapshot(&raw);
        let changed = {
            let cur = writer.lock_ref();
            *cur != next
        };
        if changed {
            writer.set(next);
        }
    }
}

/// One blocking `Request::Outputs` round-trip, keyed by connector.
///
/// Returns empty on any IPC failure — callers treat empty as "niri unreachable
/// this tick". Shared by [`poll_loop`] and by `display_config`'s request-driven
/// `GetCurrentState` / `ApplyMonitorsConfig` (which need a *fresh* read at
/// request time, not the up-to-2 s-old poll snapshot), so exactly one function
/// in the crate speaks `Outputs` to niri.
pub(crate) fn query_outputs_raw() -> OutputMap {
    let mut socket = match Socket::connect() {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "displays: niri socket connect failed");
            return OutputMap::new();
        }
    };

    let reply = match socket.send(Request::Outputs) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "displays: niri send Outputs failed");
            return OutputMap::new();
        }
    };

    match reply {
        Ok(Response::Outputs(map)) => map.into_iter().collect(),
        Ok(other) => {
            tracing::warn!(?other, "displays: unexpected reply for Outputs");
            OutputMap::new()
        }
        Err(msg) => {
            tracing::warn!(error = %msg, "displays: niri returned error for Outputs");
            OutputMap::new()
        }
    }
}

/// Project a raw snapshot onto the shell's connector-sorted [`Output`] list.
/// (`OutputMap` is already keyed by connector, so the sort is a no-op unless
/// niri ever keys a reply by something other than the output's own name.)
fn convert_snapshot(raw: &OutputMap) -> Vec<Output> {
    let mut list: Vec<Output> = raw.values().map(convert).collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

fn convert(o: &niri_ipc::Output) -> Output {
    let mode = o
        .current_mode
        .and_then(|idx| o.modes.get(idx).copied())
        .map(|m| Mode {
            width: u32::from(m.width),
            height: u32::from(m.height),
            refresh_mhz: m.refresh_rate,
        });

    // niri reports `logical: None` when the output is disabled; treat that
    // as the canonical disabled signal rather than relying on mode presence
    // (a disabled output can still expose its preferred modes list).
    let enabled = o.logical.is_some();
    let scale = o.logical.map_or(1.0, |l| l.scale);
    let transform = o
        .logical
        .map_or_else(|| "normal".to_string(), |l| transform_str(l.transform));

    Output {
        name: o.name.clone(),
        make: o.make.clone(),
        model: o.model.clone(),
        mode,
        enabled,
        scale,
        transform,
    }
}

fn transform_str(t: NiriTransform) -> String {
    match t {
        NiriTransform::Normal => "normal",
        NiriTransform::_90 => "90",
        NiriTransform::_180 => "180",
        NiriTransform::_270 => "270",
        NiriTransform::Flipped => "flipped",
        NiriTransform::Flipped90 => "flipped-90",
        NiriTransform::Flipped180 => "flipped-180",
        NiriTransform::Flipped270 => "flipped-270",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::{Mode, Output, OutputMap, convert_snapshot, derive_loop};
    use futures_signals::signal::{Mutable, SignalExt};
    use niri_ipc::{
        LogicalOutput, Mode as NiriMode, Output as NiriOutput, Transform as NiriTransform,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Fixtures ──────────────────────────────────────────────────────────────

    fn mk_output(name: &str, enabled: bool, scale: f64) -> NiriOutput {
        NiriOutput {
            name: name.to_owned(),
            make: "ACME".to_owned(),
            model: "Screen".to_owned(),
            serial: Some("S1".to_owned()),
            physical_size: Some((600, 340)),
            modes: vec![
                NiriMode {
                    width: 1920,
                    height: 1080,
                    refresh_rate: 60_000,
                    is_preferred: true,
                },
                NiriMode {
                    width: 1280,
                    height: 720,
                    refresh_rate: 60_000,
                    is_preferred: false,
                },
            ],
            current_mode: enabled.then_some(0),
            is_custom_mode: false,
            vrr_supported: false,
            vrr_enabled: false,
            logical: enabled.then_some(LogicalOutput {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale,
                transform: NiriTransform::_90,
            }),
        }
    }

    fn snapshot(list: Vec<NiriOutput>) -> Arc<OutputMap> {
        Arc::new(list.into_iter().map(|o| (o.name.clone(), o)).collect())
    }

    // ── Projection ────────────────────────────────────────────────────────────

    #[test]
    fn convert_snapshot_is_connector_sorted() {
        let raw = snapshot(vec![
            mk_output("HDMI-A-1", true, 1.0),
            mk_output("DP-1", true, 1.0),
            mk_output("eDP-1", true, 1.0),
        ]);
        let names: Vec<String> = convert_snapshot(&raw).into_iter().map(|o| o.name).collect();
        assert_eq!(names, ["DP-1", "HDMI-A-1", "eDP-1"]);
    }

    #[test]
    fn convert_reads_mode_scale_and_transform_from_niri() {
        let raw = snapshot(vec![mk_output("eDP-1", true, 1.5)]);
        let list = convert_snapshot(&raw);
        assert_eq!(list.len(), 1);
        let o = &list[0];
        assert!(o.enabled);
        assert_eq!(
            o.mode,
            Some(Mode {
                width: 1920,
                height: 1080,
                refresh_mhz: 60_000,
            })
        );
        assert!((o.scale - 1.5).abs() < f64::EPSILON);
        assert_eq!(o.transform, "90");
        assert_eq!(o.make, "ACME");
    }

    #[test]
    fn disabled_output_reports_defaults() {
        // `logical: None` is the canonical disabled tell; scale/transform fall
        // back to the neutral values rather than the last live ones.
        let raw = snapshot(vec![mk_output("DP-2", false, 2.0)]);
        let list = convert_snapshot(&raw);
        let o = &list[0];
        assert!(!o.enabled);
        assert_eq!(o.mode, None);
        assert!((o.scale - 1.0).abs() < f64::EPSILON);
        assert_eq!(o.transform, "normal");
    }

    #[test]
    fn empty_snapshot_projects_to_empty_list() {
        assert!(convert_snapshot(&OutputMap::new()).is_empty());
    }

    // ── Derive loop ───────────────────────────────────────────────────────────

    /// Hand the single-threaded runtime enough cooperative turns to drain the
    /// derive task and the subscriber it wakes. No wall-clock and no
    /// `tokio::time::pause` (that needs tokio's `test-util` feature), so this is
    /// deterministic rather than merely usually-long-enough.
    async fn settle() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    /// The consolidation must not change what `outputs()` observers see: a
    /// re-published but *identical* snapshot has to be swallowed (the shared
    /// source republishes every 2 s without deduping), while a real change
    /// still emits.
    #[test]
    fn derive_dedups_identical_snapshots_and_emits_on_change() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let source = Mutable::new(Arc::new(OutputMap::new()));
            let writer: Mutable<Vec<Output>> = Mutable::new(Vec::new());

            let emissions = Arc::new(AtomicUsize::new(0));
            let counter = emissions.clone();
            let sig = writer.signal_cloned();
            let sub = tokio::spawn(async move {
                sig.for_each(move |_| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(())
                })
                .await;
            });
            let derive = tokio::spawn(derive_loop(source.clone(), writer.clone()));
            settle().await; // initial replay of the empty list

            source.set(snapshot(vec![mk_output("eDP-1", true, 1.0)]));
            settle().await;
            assert_eq!(emissions.load(Ordering::SeqCst), 2, "first snapshot emits");

            // Same content, fresh Arc — exactly what an idle poll tick looks like.
            source.set(snapshot(vec![mk_output("eDP-1", true, 1.0)]));
            settle().await;
            assert_eq!(
                emissions.load(Ordering::SeqCst),
                2,
                "an unchanged snapshot must not re-fire outputs()"
            );

            source.set(snapshot(vec![mk_output("eDP-1", false, 1.0)]));
            settle().await;
            assert_eq!(emissions.load(Ordering::SeqCst), 3, "a real change emits");

            derive.abort();
            sub.abort();

            let published = writer.lock_ref().clone();
            assert_eq!(published.len(), 1);
            assert!(!published[0].enabled);
        });
    }
}
