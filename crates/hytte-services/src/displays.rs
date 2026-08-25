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
//! # Toggle feedback (#599)
//!
//! "The next poll picks it up" is up to [`POLL_INTERVAL`] away, and the panel
//! rebuilds its rows from every emission — so without help the switch the user
//! just flipped gets rebuilt from a reading that predates their toggle and moves
//! back on its own. [`Output::enabled`] is therefore a
//! [`Pending<bool>`][hytte_reactive::Pending]: niri's reading, plus the user's
//! not-yet-echoed request while one is outstanding.
//!
//! The intent lives **here**, not in the panel. It used to live in the panel —
//! an `Rc<RefCell<HashMap<String, bool>>>` with a 3 s `glib` timeout — and that
//! shape could not do the one thing a pending marker has to: revert. Clearing
//! the map re-rendered nothing, and [`outputs`] is `PartialEq`-deduped, so a
//! toggle niri never honoured produced no emission and left the switch pinned in
//! the user's position indefinitely. #599 retired it for the model
//! `nightlight` already used.
//!
//! [`reconcile`] owns the whole lifecycle, on the poller's own tick, so the echo
//! and the give-up are one decision in one place:
//!
//! - niri's reading agrees with the intent ⇒ the write landed; retire it.
//! - the connector is gone ⇒ there is nothing left to wait for; retire it.
//! - neither, for longer than [`TOGGLE_GRACE`] ⇒ give up and retire it, so the
//!   switch falls back to niri's reading and the failure becomes visible.
//!
//! Polling (vs subscribing to a niri event stream) is the v1 choice for
//! simplicity — niri 26.4 pushes no output event on its IPC event stream — and
//! the lag is fine for a passive page. Swap [`poll_loop`]'s body to upgrade;
//! that one edit now covers both consumers.
//!
//! Kept separate from `niri.rs` because that module owns the long-lived
//! event-stream socket; outputs aren't pushed on that stream so the polling
//! shape lives here.

use futures_signals::map_ref;
use futures_signals::signal::{Mutable, Signal, SignalExt};
use futures_util::StreamExt;
use hytte_reactive::{Pending, Service, registry, runtime, shared, spawn_supervised};
use niri_ipc::socket::Socket;
use niri_ipc::{OutputAction, Request, Response, Transform as NiriTransform};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

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
    /// Whether the output is currently driving pixels, as niri reports it —
    /// plus any [`set_output_enabled`] the user has asked for that niri has not
    /// echoed yet (#599).
    ///
    /// `enabled.confirmed()` is niri's reading and nothing else;
    /// `enabled.displayed()` is what a switch should show;
    /// `enabled.is_pending()` is the cue for a "turning on/off…" affordance.
    pub enabled: Pending<bool>,
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

// ── Pending toggles (#599) ───────────────────────────────────────────────────

/// How long an un-echoed toggle keeps the switch in the position the user put
/// it before the shell gives up and falls back to niri's reading — this
/// module's parameterisation of [`Pending`]'s give-up contract (`nightlight`
/// passes its `FIX_WAIT` to the same argument).
///
/// Sized off [`POLL_INTERVAL`], not off a feel: a toggle niri honours normally
/// shows up on the *next* tick, and the snapshot for that tick may already have
/// been in flight when the user flipped, so two intervals is the first budget
/// that cannot expire a healthy toggle. Three gives the round-trip a spare tick
/// and still bounds a dropped write to a beat rather than forever — which is the
/// whole point, since sticking pending forever is what the widget-local model
/// this replaced actually did. (Deliberately **not** that model's 3 s: at one
/// and a half poll intervals it could expire a toggle niri was about to honour,
/// which only went unnoticed because clearing the widget-local map re-rendered
/// nothing.)
const TOGGLE_GRACE: Duration = Duration::from_secs(6);

/// Outstanding toggle requests, keyed by connector — at most one per output,
/// since [`Pending::request`] replaces any earlier intent for the same value.
///
/// Each entry is a whole [`Pending<bool>`] rather than a bare flag so the
/// deadline rides with the request that set it; the `confirmed` half is
/// bookkeeping, re-seated onto niri's latest reading by [`Pending::rebased_on`]
/// wherever it is read.
pub(crate) type Intents = BTreeMap<String, Pending<bool>>;

/// Fold outstanding intents into niri's reading, producing the view [`outputs`]
/// publishes.
///
/// Every entry is rebased onto the reading in `outputs`, so an intent niri has
/// already caught up with comes out settled even if [`reconcile`] has not run
/// yet — the merged view can never claim a wait that is already over.
fn merge(outputs: &[Output], intents: &Intents) -> Vec<Output> {
    outputs
        .iter()
        .map(|o| {
            let mut merged = o.clone();
            if let Some(pending) = intents.get(&o.name) {
                merged.enabled = pending.rebased_on(*o.enabled.confirmed());
            }
            merged
        })
        .collect()
}

/// Decide which intents are still outstanding, given niri's latest reading.
///
/// The single place a pending toggle is retired, on all three paths — see the
/// module docs. Pure and `now`-injected so every path is testable without a
/// wall clock or a live niri; the deadline itself lives in the [`Pending`], set
/// when the request was recorded.
fn reconcile(intents: &Intents, outputs: &[Output], now: Instant) -> Intents {
    intents
        .iter()
        .filter_map(|(name, pending)| {
            // The connector is gone (unplugged, or niri unreachable this tick).
            // There is no row left to hold the switch on.
            let output = outputs.iter().find(|o| &o.name == name)?;
            // Rebasing settles the request against niri's reading: an echo
            // retires it, a disagreement keeps it with its clock untouched.
            let mut next = pending.rebased_on(*output.enabled.confirmed());
            next.expire(now);
            next.is_pending().then(|| (name.clone(), next))
        })
        .collect()
}

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct DisplaysHandles {
    /// niri's reading, settled — [`derive_loop`] never writes an intent here.
    pub(crate) outputs: Mutable<Vec<Output>>,
    /// Toggles the user has asked for that niri has not echoed yet. Kept beside
    /// `outputs` rather than inside it so the poller's derive step stays a pure
    /// projection of the snapshot; [`outputs`] combines the two into the one
    /// signal consumers see.
    pub(crate) intents: Mutable<Intents>,
}

impl Default for DisplaysHandles {
    fn default() -> Self {
        Self {
            outputs: Mutable::new(Vec::new()),
            intents: Mutable::new(Intents::new()),
        }
    }
}

pub struct DisplayService;

impl Service for DisplayService {
    type Handles = DisplaysHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = DisplaysHandles::default();
        let writer = handles.outputs.clone();
        let intents = handles.intents.clone();
        let source = niri_outputs_source();

        spawn_supervised("displays", move || {
            let snapshots = source.snapshot.clone();
            let writer = writer.clone();
            let intents = intents.clone();
            async move {
                derive_loop(snapshots, writer, intents).await;
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
/// when the value actually changes (`PartialEq` dedup).
///
/// **One** signal carrying niri's reading and any outstanding toggle intent
/// together, rather than two a widget would observe at different times — a row
/// built from two emissions can render "off, and spinning" or "on, no spinner"
/// out of a single logical transition. `map_ref!` folds them, and the dedup
/// keeps a merge that changes nothing from rebuilding every row.
pub fn outputs() -> impl Signal<Item = Vec<Output>> {
    registry::with(|r| {
        let handles = r
            .get::<DisplaysHandles>()
            .expect("displays::service() not registered");
        let outputs = handles.outputs.clone();
        let intents = handles.intents.clone();
        map_ref! {
            let outputs = outputs.signal_cloned(),
            let intents = intents.signal_cloned() =>
            merge(outputs, intents)
        }
        .dedupe_cloned()
    })
}

/// Toggle one output on or off. Fire-and-forget — the change is reflected
/// in the next polled `outputs()` emission, typically within 2 s. Errors
/// (unknown connector, IPC failure) are logged at warn and dropped.
///
/// The request is *also* recorded as an intent on [`Output::enabled`] before the
/// IPC round-trip, so the switch stays where the user put it across the row
/// rebuilds that happen in between (#599). [`reconcile`] retires it — on niri's
/// echo, or on [`TOGGLE_GRACE`] if that echo never comes.
///
/// Call from the GTK main thread: the intent is recorded through the
/// thread-local registry. Off-thread (or unregistered) the toggle still fires,
/// just without the optimistic feedback.
pub fn set_output_enabled(name: &str, on: bool) {
    record_intent(name, on);
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

/// Record a toggle the user just asked for, so the row keeps showing it until
/// niri echoes it (or [`TOGGLE_GRACE`] runs out).
///
/// Split from [`set_output_enabled`] at the registry boundary so the decision
/// itself ([`record`]) is a pure function over the map.
fn record_intent(name: &str, on: bool) {
    registry::with(|r| {
        let Some(handles) = r.get::<DisplaysHandles>() else {
            return;
        };
        let confirmed = handles
            .outputs
            .lock_ref()
            .iter()
            .find(|o| o.name == name)
            .map(|o| *o.enabled.confirmed());
        // No row for this connector — nothing to hold a switch on.
        let Some(confirmed) = confirmed else {
            return;
        };
        record(
            &mut handles.intents.lock_mut(),
            name,
            on,
            confirmed,
            Instant::now() + TOGGLE_GRACE,
        );
    });
}

/// Fold one fresh request into the intents map, given niri's current reading.
///
/// A request for the state niri *already* reports leaves nothing outstanding —
/// there is nothing to wait for, and a "turning on…" row for a write that is
/// already true would be the same dishonest feedback from the other side. It
/// also **removes** any earlier entry, which is the on/off/on case: the user has
/// toggled back to where niri is, and a surviving intent would keep the switch
/// showing the request they just undid.
fn record(intents: &mut Intents, name: &str, on: bool, confirmed: bool, deadline: Instant) {
    let mut pending = Pending::settled(confirmed);
    pending.request_until(on, deadline);
    if pending.is_pending() {
        intents.insert(name.to_owned(), pending);
    } else {
        intents.remove(name);
    }
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

/// Derive niri's reading from the shared raw snapshot, and retire any toggle
/// intent the same tick has settled or outlived.
///
/// Split out of `Service::start` so the dedup and expiry semantics are testable
/// without a live niri (each intent carries its own deadline, so a test seeds a
/// past one instead of waiting on a clock).
///
/// `PartialEq` dedup on both writes: only publish when the value actually
/// differs from the cached one, otherwise downstream signals re-fire for no
/// reason every 2 s.
///
/// The reconcile runs on **every** tick, not just the ones that change the
/// derived list. A give-up has to fire on a snapshot that looks identical to the
/// last — that is exactly what a toggle niri never honoured produces — and so
/// does the echo of a toggle that asked for the state niri was already in.
async fn derive_loop(
    snapshots: Mutable<Arc<OutputMap>>,
    writer: Mutable<Vec<Output>>,
    intents: Mutable<Intents>,
) {
    let mut stream = snapshots.signal_cloned().to_stream();
    while let Some(raw) = stream.next().await {
        let next = convert_snapshot(&raw);

        let kept = reconcile(&intents.lock_ref(), &next, Instant::now());
        let intents_changed = *intents.lock_ref() != kept;
        if intents_changed {
            intents.set(kept);
        }

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
        // Settled: this projection is niri's reading and only niri's reading.
        // Any outstanding intent is folded in later, by `merge`.
        enabled: Pending::settled(enabled),
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
    use super::{
        Intents, Mode, Output, OutputMap, Pending, TOGGLE_GRACE, convert_snapshot, derive_loop,
        merge, reconcile, record,
    };
    use futures_signals::signal::{Mutable, SignalExt};
    use niri_ipc::{
        LogicalOutput, Mode as NiriMode, Output as NiriOutput, Transform as NiriTransform,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

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
        assert!(*o.enabled.confirmed());
        assert!(
            !o.enabled.is_pending(),
            "the projection is niri's reading alone; intent is folded in by merge"
        );
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
        assert!(!*o.enabled.confirmed());
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
            let derive = tokio::spawn(derive_loop(
                source.clone(),
                writer.clone(),
                Mutable::new(Intents::new()),
            ));
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
            assert!(!*published[0].enabled.confirmed());
        });
    }

    // ── Pending toggles (#599) ───────────────────────────────────────────────
    //
    // The three edges of the model that replaced the panel's local pending map:
    // the echo arrives, the echo never arrives, and two toggles land in a row.
    // All hermetic — every intent is pinned to a deadline the test chooses, and
    // `reconcile` takes its `now`, so none of this reads a wall clock or needs a
    // live niri.

    /// A recorded toggle for one connector, pinned to `deadline`.
    fn intent(confirmed: bool, on: bool, deadline: Instant) -> Pending<bool> {
        let mut pending = Pending::settled(confirmed);
        pending.request_until(on, deadline);
        assert!(
            pending.is_pending(),
            "fixture asked for the state it is already in — nothing would be recorded"
        );
        pending
    }

    fn intents(entries: &[(&str, Pending<bool>)]) -> Intents {
        entries
            .iter()
            .map(|&(name, pending)| (name.to_owned(), pending))
            .collect()
    }

    /// niri's reading for one connector, as `convert_snapshot` would produce it.
    fn reading(name: &str, enabled: bool) -> Vec<Output> {
        convert_snapshot(&snapshot(vec![mk_output(name, enabled, 1.0)]))
    }

    #[test]
    fn an_outstanding_intent_holds_the_switch_where_the_user_put_it() {
        // The bug the whole model exists for: the poll that lands between the
        // toggle and niri's ack still reports the old value, and merging must
        // not let that value reach the switch.
        let deadline = Instant::now() + TOGGLE_GRACE;
        let outputs = reading("eDP-1", true);
        let pending = intents(&[("eDP-1", intent(true, false, deadline))]);

        let merged = merge(&outputs, &pending);
        assert!(merged[0].enabled.is_pending());
        assert!(
            *merged[0].enabled.confirmed(),
            "niri's reading is reported as-is, never overwritten"
        );
        assert!(
            !*merged[0].enabled.displayed(),
            "the switch stays off, where the user just put it"
        );
    }

    #[test]
    fn the_echo_retires_the_intent_and_the_row_goes_back_to_niri() {
        // Confirm-arrives. niri now agrees, so there is nothing left to wait
        // for — well inside the grace, which must not be what decides this.
        let now = Instant::now();
        let pending = intents(&[("eDP-1", intent(true, false, now + TOGGLE_GRACE))]);
        let echoed = reading("eDP-1", false);

        let kept = reconcile(&pending, &echoed, now);
        assert!(kept.is_empty(), "an echoed toggle is not still in flight");

        let merged = merge(&echoed, &kept);
        assert!(!merged[0].enabled.is_pending());
        assert!(!*merged[0].enabled.displayed());
    }

    #[test]
    fn an_intent_that_is_never_echoed_expires_and_the_switch_reverts() {
        // Confirm-never-arrives — the case the panel's local map got wrong:
        // it cleared its entry on a timer that re-rendered nothing, so the
        // switch stayed where the user put it forever. Here the give-up is a
        // change to the published value, so the row rebuilds on niri's reading.
        let asked_at = Instant::now();
        let deadline = asked_at + TOGGLE_GRACE;
        let pending = intents(&[("eDP-1", intent(true, false, deadline))]);
        // niri never honoured the toggle: it still reports the output as on,
        // tick after tick.
        let unchanged = reading("eDP-1", true);

        let within = reconcile(&pending, &unchanged, asked_at + TOGGLE_GRACE / 2);
        assert_eq!(
            within.len(),
            1,
            "a healthy toggle must still be pending inside its grace"
        );
        assert!(!*merge(&unchanged, &within)[0].enabled.displayed());

        let after = reconcile(&pending, &unchanged, deadline);
        assert!(after.is_empty(), "the give-up fires once the grace is out");

        let merged = merge(&unchanged, &after);
        assert!(!merged[0].enabled.is_pending());
        assert!(
            *merged[0].enabled.displayed(),
            "the switch reverts to niri's reading, so the failed toggle is visible"
        );
    }

    #[test]
    fn a_poll_tick_does_not_extend_an_outstanding_intent() {
        // The deadline belongs to the request, not to the last time we looked.
        // Re-running reconcile every 2 s must not push the give-up out forever,
        // which is what a "seconds since the last tick" model would do.
        let asked_at = Instant::now();
        let deadline = asked_at + TOGGLE_GRACE;
        let mut pending = intents(&[("eDP-1", intent(true, false, deadline))]);
        let unchanged = reading("eDP-1", true);

        for step in 1..4 {
            pending = reconcile(&pending, &unchanged, asked_at + Duration::from_secs(step));
            assert_eq!(pending.len(), 1);
            assert_eq!(
                pending["eDP-1"].deadline(),
                Some(deadline),
                "the clock is the request's, and it does not restart"
            );
        }
        assert!(reconcile(&pending, &unchanged, deadline).is_empty());
    }

    #[test]
    fn the_grace_is_long_enough_for_a_healthy_toggle() {
        // A tripwire on the constant, not on the mechanism: the snapshot for
        // the tick after a toggle may already have been in flight when the user
        // flipped, so anything under two poll intervals could expire a toggle
        // niri is about to honour. This is why TOGGLE_GRACE is not the 3 s the
        // widget-local model used.
        assert!(
            TOGGLE_GRACE >= super::POLL_INTERVAL * 2,
            "TOGGLE_GRACE must outlast the poll round-trip it is waiting on"
        );
    }

    #[test]
    fn a_rapid_retoggle_cancels_rather_than_leaving_a_stale_request() {
        // off then straight back on, while niri has echoed neither. The second
        // flip lands on the state niri already reports, so *nothing* is
        // outstanding — and the first request must not survive it, or the row
        // would keep showing the toggle the user just undid.
        let t0 = Instant::now();
        let mut pending = Intents::new();

        record(&mut pending, "eDP-1", false, true, t0 + TOGGLE_GRACE);
        assert_eq!(pending.len(), 1);
        assert!(
            !*merge(&reading("eDP-1", true), &pending)[0]
                .enabled
                .displayed()
        );

        record(&mut pending, "eDP-1", true, true, t0 + TOGGLE_GRACE);
        assert!(
            pending.is_empty(),
            "the undone request is gone, not left behind"
        );
        assert!(
            *merge(&reading("eDP-1", true), &pending)[0]
                .enabled
                .displayed()
        );
    }

    #[test]
    fn a_later_toggle_carries_its_own_deadline_not_the_earlier_ones() {
        // Rapid double-toggle. The panel's map armed one 3 s timer per toggle
        // keyed only by connector, so the *first* timer disarmed the *second*
        // toggle's intent early. One entry per connector, with the deadline
        // inside the request that set it, removes the interaction entirely.
        let t0 = Instant::now();
        let t2 = t0 + Duration::from_secs(2);
        let mut pending = Intents::new();

        // off … back on (cancels) … off again, two seconds later.
        record(&mut pending, "eDP-1", false, true, t0 + TOGGLE_GRACE);
        record(&mut pending, "eDP-1", true, true, t0 + TOGGLE_GRACE);
        record(&mut pending, "eDP-1", false, true, t2 + TOGGLE_GRACE);
        assert_eq!(pending.len(), 1, "at most one intent per connector");
        assert_eq!(pending["eDP-1"].deadline(), Some(t2 + TOGGLE_GRACE));

        // niri is still reporting the pre-toggle state. The moment the *first*
        // toggle's grace would have run out is not the live one's.
        let unchanged = reading("eDP-1", true);
        let kept = reconcile(&pending, &unchanged, t0 + TOGGLE_GRACE);
        assert_eq!(
            kept.len(),
            1,
            "the newer toggle keeps its own deadline; the older one's clock left with it"
        );
        assert!(
            !*merge(&unchanged, &kept)[0].enabled.displayed(),
            "the switch shows the newest request"
        );

        assert!(reconcile(&pending, &unchanged, t2 + TOGGLE_GRACE).is_empty());
    }

    #[test]
    fn an_intent_for_a_vanished_connector_is_dropped() {
        // Unplugged mid-toggle, or niri unreachable for a tick (the poller
        // publishes an empty map on IPC failure). There is no row to hold.
        let now = Instant::now();
        let pending = intents(&[("HDMI-A-1", intent(false, true, now + TOGGLE_GRACE))]);
        assert!(reconcile(&pending, &[], now).is_empty());
        assert!(reconcile(&pending, &reading("eDP-1", true), now).is_empty());
    }

    #[test]
    fn merging_an_intent_niri_already_agrees_with_shows_no_wait() {
        // Belt and braces against a "turning on…" row for a write that is
        // already true: `merge` rebases onto the reading it is handed, so it is
        // safe on its own without `reconcile` having run first.
        let outputs = reading("eDP-1", true);
        let stale = intents(&[("eDP-1", intent(false, true, Instant::now() + TOGGLE_GRACE))]);
        let merged = merge(&outputs, &stale);
        assert!(!merged[0].enabled.is_pending());
        assert!(*merged[0].enabled.displayed());
    }

    #[test]
    fn the_derive_loop_expires_a_stuck_intent_on_its_own_tick() {
        // The give-up wired up for real: no timer, no separate task — the
        // poller's own tick is what retires it, so a snapshot identical to the
        // last one (exactly what a dropped toggle produces) still resolves.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let source = Mutable::new(Arc::new(OutputMap::new()));
            let writer: Mutable<Vec<Output>> = Mutable::new(Vec::new());
            // Deadline already reached: the never-echoed path, without a
            // wall-clock wait.
            let pending = Mutable::new(intents(&[("eDP-1", intent(true, false, Instant::now()))]));

            let derive = tokio::spawn(derive_loop(source.clone(), writer.clone(), pending.clone()));

            // niri keeps reporting the output as on — the toggle never landed.
            source.set(snapshot(vec![mk_output("eDP-1", true, 1.0)]));
            settle().await;
            derive.abort();

            assert!(
                pending.lock_ref().is_empty(),
                "the poller's tick must retire an intent it has outlived"
            );
            let published = writer.lock_ref().clone();
            assert!(
                *published[0].enabled.confirmed(),
                "and what is left is niri's reading, so the switch snaps back"
            );
        });
    }

    #[test]
    fn the_derive_loop_retires_an_intent_the_snapshot_confirms() {
        // The other half of the same tick: niri echoed, so the intent goes
        // however much grace is left.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let source = Mutable::new(Arc::new(OutputMap::new()));
            let writer: Mutable<Vec<Output>> = Mutable::new(Vec::new());
            // A deadline far enough out that only the echo can retire this.
            let deadline = Instant::now() + TOGGLE_GRACE * 1000;
            let pending = Mutable::new(intents(&[("eDP-1", intent(true, false, deadline))]));

            let derive = tokio::spawn(derive_loop(source.clone(), writer.clone(), pending.clone()));

            source.set(snapshot(vec![mk_output("eDP-1", false, 1.0)]));
            settle().await;
            derive.abort();

            assert!(
                pending.lock_ref().is_empty(),
                "niri agreed, so nothing is in flight any more"
            );
            let merged = merge(&writer.lock_ref(), &pending.lock_ref());
            assert!(!merged[0].enabled.is_pending());
            assert!(!*merged[0].enabled.displayed());
        });
    }
}
