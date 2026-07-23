//! `org.gnome.Mutter.DisplayConfig` shim over niri-ipc (#393).
//!
//! Serves the D-Bus interface that GNOME's display tooling already speaks —
//! most importantly **gnome-control-center's own Display panel** — and drives
//! it onto niri outputs over niri-ipc. "Compatmaxx": reuse the unmodified
//! GNOME client, provide the backend. Decided on #381 (Annika 💯'd it): display
//! management is *not* a bespoke trollshell tab; it's g-c-c's panel talking to
//! this object.
//!
//! # What is implemented
//!
//! * **Owns `org.gnome.Mutter.DisplayConfig`** on the session bus (via
//!   [`hytte_bus::own_name`], same pattern as [`crate::screensaver`] /
//!   `trollshell::control`) and mounts the interface at
//!   `/org/gnome/Mutter/DisplayConfig`.
//! * **`GetCurrentState`** — enumerates niri's `Request::Outputs` into Mutter's
//!   monitors + logical-monitors model. This alone makes g-c-c's Display panel
//!   *show* every connected output, its modes, and its current arrangement.
//! * **`ApplyMonitorsConfig`** — translates the requested logical-monitor
//!   layout back into niri `OutputAction`s (on/off, mode, scale, transform,
//!   position). Honors the `serial` (rejects stale requests) and the `method`
//!   (0 = verify, 1 = temporary, 2 = persistent).
//! * **`MonitorsChanged`** — emitted when niri's output topology changes
//!   (hot-plug / mode / scale / position), detected by a background poll (niri
//!   26.4 has no output event on its IPC event stream — see the poll note).
//!
//! # What is deferred (documented, not silently wrong)
//!
//! * **True persistence (kanshi).** niri's `Request::Output` is *explicitly
//!   temporary* — the compositor forgets it as soon as its own output config
//!   reloads (niri-ipc docs). So *every* IPC apply here is non-persistent;
//!   `method = persistent` currently applies live and logs that a kanshi-profile
//!   write (`etc/kanshi/`) is the follow-up. GNOME's "keep these settings?"
//!   confirmation flow still works — only survival across a niri output-config
//!   reload is missing.
//! * **Mirroring.** niri-ipc exposes no mirror action, so an
//!   `ApplyMonitorsConfig` that assigns two monitors to one logical monitor is
//!   rejected (`NotSupported`) rather than faked.
//!
//! # niri ↔ Mutter model mapping (and where we clamp / reject, never lie)
//!
//! * niri maps one output to one logical output (no mirroring), so each enabled
//!   niri output becomes exactly one Mutter *logical monitor* carrying that
//!   output's single monitor-spec.
//! * **Modes** are reported verbatim from niri (`GetCurrentState` never invents
//!   a mode niri didn't list). Refresh is millihertz in niri, Hz in Mutter.
//! * **Scales.** Mutter wants a discrete `supported_scales` list per mode; niri
//!   does *true* fractional scaling. We offer `1.0` plus the 0.25-steps up to
//!   `3.0` whose resulting logical size stays above a sane floor
//!   ([`MIN_LOGICAL_PX`]) — a list niri can honor for real. We intentionally
//!   relax Mutter's integer-logical-size constraint (niri isn't bound by it)
//!   but never advertise a scale niri can't apply.
//! * **Transform** maps 1:1 by index: both niri's `Transform` and Mutter's `u`
//!   transform follow the `wl_output` counter-clockwise order.
//! * **Primary.** niri has no primary-output concept; we mark the top-left-most
//!   enabled output primary so g-c-c has something coherent to show. `primary`
//!   in an incoming apply is accepted and ignored.
//!
//! # Poll note
//!
//! niri-ipc 26.4 emits no output-change event on its event stream (only
//! workspace/window/cast events). To fire `MonitorsChanged` we poll
//! `Request::Outputs` every [`POLL_INTERVAL`] and diff a cheap fingerprint —
//! the same shape [`crate::displays`] uses. A transient empty read (niri socket
//! blip) is ignored so it can't flip-flop the serial.
//!
//! # Live-verify
//!
//! CI cannot exercise this: it needs a live niri session with real outputs plus
//! gnome-control-center. See the PR body's Live-verify section.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use hytte_bus::{OwnNameSignal, own_name};
use hytte_reactive::{Service, spawn_supervised};
use niri_ipc::socket::Socket;
use niri_ipc::{
    ConfiguredMode, ConfiguredPosition, Mode as NiriMode, ModeToSet, Output as NiriOutput,
    OutputAction, PositionToSet, Request, Response, ScaleToSet, Transform as NiriTransform,
};
use serde::{Deserialize, Serialize};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

/// Well-known bus name we own on the session bus — the exact name g-c-c binds.
const DISPLAY_CONFIG_NAME: &str = "org.gnome.Mutter.DisplayConfig";
/// Object path the interface is mounted at (also the `MonitorsChanged` sender).
const DISPLAY_CONFIG_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

/// How often the background loop re-reads niri outputs to detect topology
/// changes and emit `MonitorsChanged`. Mirrors [`crate::displays`].
const POLL_INTERVAL: Duration = Duration::from_secs(2);

// ── Mutter scale model ────────────────────────────────────────────────────────

/// Smallest fractional scale we offer above native.
const SCALE_MIN: f64 = 1.0;
/// Largest scale we offer.
const SCALE_MAX: f64 = 3.0;
/// Step between offered fractional scales (Mutter uses 0.25 too).
const SCALE_STEP: f64 = 0.25;
/// Number of `SCALE_STEP` increments spanning `SCALE_MIN..=SCALE_MAX`
/// (`(3.0 - 1.0) / 0.25 = 8`). A `u32` so the loop var maps to `f64` losslessly
/// (no float cast). Kept in sync with the three constants above by
/// [`tests::scale_steps_matches_range`].
const SCALE_STEPS: u32 = 8;
/// Floor on a mode's *logical* width/height (physical ÷ scale) below which we
/// stop offering a fractional scale. A heuristic bound on the offered list
/// only — niri itself accepts arbitrary scales.
const MIN_LOGICAL_PX: f64 = 480.0;

// ── Serialized (GetCurrentState) shapes ───────────────────────────────────────
//
// Field order below IS the wire signature — the zvariant `Type` derive emits a
// D-Bus struct `(...)` in declaration order. These must match
// `org.gnome.Mutter.DisplayConfig` exactly or g-c-c won't bind.

/// `(ssss)` — connector, vendor, product, serial.
#[derive(Debug, Clone, Serialize, zbus::zvariant::Type)]
struct MonitorSpecOut {
    connector: String,
    vendor: String,
    product: String,
    serial: String,
}

/// `(siiddada{sv})` — one available mode of a monitor.
#[derive(Debug, Clone, Serialize, zbus::zvariant::Type)]
struct MonitorModeOut {
    id: String,
    width: i32,
    height: i32,
    refresh_rate: f64,
    preferred_scale: f64,
    supported_scales: Vec<f64>,
    properties: HashMap<String, OwnedValue>,
}

/// `((ssss)a(siiddada{sv})a{sv})` — one connected monitor (enabled or not).
#[derive(Debug, Clone, Serialize, zbus::zvariant::Type)]
struct MonitorOut {
    spec: MonitorSpecOut,
    modes: Vec<MonitorModeOut>,
    properties: HashMap<String, OwnedValue>,
}

/// `(iiduba(ssss)a{sv})` — one logical monitor in the compositor layout.
#[derive(Debug, Clone, Serialize, zbus::zvariant::Type)]
struct LogicalMonitorOut {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    primary: bool,
    monitors: Vec<MonitorSpecOut>,
    properties: HashMap<String, OwnedValue>,
}

/// The full `GetCurrentState` reply: `(u a(...) a(...) a{sv})`.
type CurrentState = (
    u32,
    Vec<MonitorOut>,
    Vec<LogicalMonitorOut>,
    HashMap<String, OwnedValue>,
);

// ── Deserialized (ApplyMonitorsConfig) shapes ─────────────────────────────────

/// `(ssa{sv})` — a monitor assignment inside an apply request.
#[derive(Debug, Clone, Deserialize, zbus::zvariant::Type)]
struct MonitorConfig {
    connector: String,
    mode_id: String,
    #[allow(dead_code)]
    properties: HashMap<String, OwnedValue>,
}

/// `(iiduba(ssa{sv}))` — one requested logical monitor.
#[derive(Debug, Clone, Deserialize, zbus::zvariant::Type)]
struct LogicalMonitorConfig {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    #[allow(dead_code)]
    primary: bool,
    monitors: Vec<MonitorConfig>,
}

// ── Pure niri → Mutter mapping ────────────────────────────────────────────────

/// Map a niri transform onto Mutter's `u` transform (both are `wl_output`
/// counter-clockwise order, so the mapping is 1:1 by index).
fn transform_to_mutter(t: NiriTransform) -> u32 {
    match t {
        NiriTransform::Normal => 0,
        NiriTransform::_90 => 1,
        NiriTransform::_180 => 2,
        NiriTransform::_270 => 3,
        NiriTransform::Flipped => 4,
        NiriTransform::Flipped90 => 5,
        NiriTransform::Flipped180 => 6,
        NiriTransform::Flipped270 => 7,
    }
}

/// Inverse of [`transform_to_mutter`]. `None` for an out-of-range value.
fn transform_from_mutter(v: u32) -> Option<NiriTransform> {
    Some(match v {
        0 => NiriTransform::Normal,
        1 => NiriTransform::_90,
        2 => NiriTransform::_180,
        3 => NiriTransform::_270,
        4 => NiriTransform::Flipped,
        5 => NiriTransform::Flipped90,
        6 => NiriTransform::Flipped180,
        7 => NiriTransform::Flipped270,
        _ => return None,
    })
}

/// Stable, reversible mode id: `"{w}x{h}@{refresh_millihertz}"`. The client
/// echoes it back in `ApplyMonitorsConfig`, so it must round-trip exactly.
fn encode_mode_id(m: &NiriMode) -> String {
    format!("{}x{}@{}", m.width, m.height, m.refresh_rate)
}

/// Parse a mode id produced by [`encode_mode_id`] back into a niri
/// [`ConfiguredMode`] (refresh in Hz, as niri's setter expects).
fn decode_mode_id(id: &str) -> Option<ConfiguredMode> {
    let (res, refresh) = id.split_once('@')?;
    let (w, h) = res.split_once('x')?;
    let width: u16 = w.parse().ok()?;
    let height: u16 = h.parse().ok()?;
    let refresh_mhz: u32 = refresh.parse().ok()?;
    Some(ConfiguredMode {
        width,
        height,
        refresh: Some(f64::from(refresh_mhz) / 1000.0),
    })
}

/// Whether a fractional scale keeps the mode's logical size above the floor.
/// Native (`1.0`) is always allowed.
fn is_scale_supported(width: i32, height: i32, scale: f64) -> bool {
    if (scale - 1.0).abs() < f64::EPSILON {
        return true;
    }
    if !(SCALE_MIN..=SCALE_MAX).contains(&scale) {
        return false;
    }
    let logical_w = f64::from(width) / scale;
    let logical_h = f64::from(height) / scale;
    logical_w >= MIN_LOGICAL_PX && logical_h >= MIN_LOGICAL_PX
}

/// Discrete scales g-c-c may offer for a mode of this physical size.
fn supported_scales(width: i32, height: i32) -> Vec<f64> {
    let mut out = Vec::new();
    for i in 0..=SCALE_STEPS {
        let scale = SCALE_MIN + f64::from(i) * SCALE_STEP;
        if is_scale_supported(width, height, scale) {
            out.push(scale);
        }
    }
    if out.is_empty() {
        out.push(1.0);
    }
    out
}

/// Heuristic preferred scale for a mode: the offered scale whose logical width
/// lands closest to a comfortable target. Only a hint for g-c-c's default; the
/// authoritative current scale rides on the `is-current` mode + logical monitor.
fn preferred_scale(width: i32, height: i32) -> f64 {
    const TARGET_LOGICAL_W: f64 = 1920.0;
    supported_scales(width, height)
        .into_iter()
        .min_by(|a, b| {
            let da = (f64::from(width) / a - TARGET_LOGICAL_W).abs();
            let db = (f64::from(width) / b - TARGET_LOGICAL_W).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(1.0)
}

/// Is this connector an internal panel? Used for the `is-builtin` hint.
fn is_builtin(connector: &str) -> bool {
    let c = connector.to_ascii_lowercase();
    c.starts_with("edp") || c.starts_with("lvds") || c.starts_with("dsi")
}

/// Human-facing display name: `"make model"`, falling back to whichever half is
/// present, else the connector.
fn display_name(o: &NiriOutput) -> String {
    let make = o.make.trim();
    let model = o.model.trim();
    match (make.is_empty(), model.is_empty()) {
        (false, false) => format!("{make} {model}"),
        (false, true) => make.to_string(),
        (true, false) => model.to_string(),
        (true, true) => o.name.clone(),
    }
}

/// The connector we treat as primary: the top-left-most enabled output.
fn pick_primary(outputs: &BTreeMap<String, NiriOutput>) -> Option<String> {
    outputs
        .values()
        .filter_map(|o| o.logical.map(|l| (l.x, l.y, o.name.clone())))
        .min()
        .map(|(_, _, name)| name)
}

/// Owned-`Value` helpers for `a{sv}` entries. `try_to_owned` on a scalar is
/// infallible, hence `expect`.
fn v_bool(b: bool) -> OwnedValue {
    Value::Bool(b)
        .try_to_owned()
        .expect("bool → OwnedValue is infallible")
}
fn v_u32(n: u32) -> OwnedValue {
    Value::U32(n)
        .try_to_owned()
        .expect("u32 → OwnedValue is infallible")
}
fn v_i32(n: i32) -> OwnedValue {
    Value::I32(n)
        .try_to_owned()
        .expect("i32 → OwnedValue is infallible")
}
fn v_str(s: &str) -> OwnedValue {
    Value::from(s.to_owned())
        .try_to_owned()
        .expect("string → OwnedValue is infallible")
}

/// Build the Mutter monitor entry for one niri output (enabled or not).
fn build_monitor(o: &NiriOutput) -> MonitorOut {
    let modes = o
        .modes
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let width = i32::from(m.width);
            let height = i32::from(m.height);
            let mut props = HashMap::new();
            props.insert("is-current".to_owned(), v_bool(o.current_mode == Some(i)));
            props.insert("is-preferred".to_owned(), v_bool(m.is_preferred));
            MonitorModeOut {
                id: encode_mode_id(m),
                width,
                height,
                refresh_rate: f64::from(m.refresh_rate) / 1000.0,
                preferred_scale: preferred_scale(width, height),
                supported_scales: supported_scales(width, height),
                properties: props,
            }
        })
        .collect();

    let mut props = HashMap::new();
    props.insert("is-builtin".to_owned(), v_bool(is_builtin(&o.name)));
    props.insert("display-name".to_owned(), v_str(&display_name(o)));
    if let Some((w, h)) = o.physical_size {
        props.insert("width-mm".to_owned(), v_i32(i32::try_from(w).unwrap_or(0)));
        props.insert("height-mm".to_owned(), v_i32(i32::try_from(h).unwrap_or(0)));
    }

    MonitorOut {
        spec: spec_of(o),
        modes,
        properties: props,
    }
}

/// The `(ssss)` monitor-spec for an output.
fn spec_of(o: &NiriOutput) -> MonitorSpecOut {
    MonitorSpecOut {
        connector: o.name.clone(),
        vendor: o.make.clone(),
        product: o.model.clone(),
        serial: o.serial.clone().unwrap_or_default(),
    }
}

/// Build a logical monitor for an *enabled* output; `None` when disabled
/// (`logical: None`), which is exactly Mutter's "not in `logical_monitors`".
fn build_logical_monitor(o: &NiriOutput, primary: Option<&str>) -> Option<LogicalMonitorOut> {
    let l = o.logical?;
    Some(LogicalMonitorOut {
        x: l.x,
        y: l.y,
        scale: l.scale,
        transform: transform_to_mutter(l.transform),
        primary: primary == Some(o.name.as_str()),
        monitors: vec![spec_of(o)],
        properties: HashMap::new(),
    })
}

/// Global `a{sv}` for `GetCurrentState`. Logical layout mode (positions/sizes in
/// logical pixels, matching niri's `logical` fields), no mirroring, per-monitor
/// scale (not a single global scale).
fn global_properties() -> HashMap<String, OwnedValue> {
    let mut p = HashMap::new();
    p.insert("layout-mode".to_owned(), v_u32(1));
    p.insert("supports-changing-layout-mode".to_owned(), v_bool(false));
    p.insert("supports-mirroring".to_owned(), v_bool(false));
    p.insert("global-scale-required".to_owned(), v_bool(false));
    p
}

/// Assemble the whole `GetCurrentState` reply from a niri outputs snapshot.
fn build_current_state(serial: u32, outputs: &BTreeMap<String, NiriOutput>) -> CurrentState {
    let primary = pick_primary(outputs);
    let monitors = outputs.values().map(build_monitor).collect();
    let logical = outputs
        .values()
        .filter_map(|o| build_logical_monitor(o, primary.as_deref()))
        .collect();
    (serial, monitors, logical, global_properties())
}

// ── Apply planning (pure, testable) ───────────────────────────────────────────

/// One niri action to apply to a connector, in a form that's easy to assert on
/// in tests (niri's own `OutputAction` isn't `PartialEq`). All variants are
/// `Copy` (niri's `ConfiguredMode`/`ConfiguredPosition`/`Transform` are), so
/// the plan lowers by value without clones.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PlannedAction {
    Off,
    On,
    Mode(ConfiguredMode),
    Scale(f64),
    Transform(NiriTransform),
    Position(ConfiguredPosition),
}

/// Why an `ApplyMonitorsConfig` request can't be honored.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ApplyError {
    UnknownConnector(String),
    UnknownMode { connector: String, mode_id: String },
    BadTransform(u32),
    Mirroring,
}

/// Does `cm` name a mode this output actually has? (Guards against a client
/// echoing a stale/foreign mode id.)
fn mode_exists(o: &NiriOutput, cm: &ConfiguredMode) -> bool {
    o.modes.iter().any(|m| {
        m.width == cm.width
            && m.height == cm.height
            && cm
                .refresh
                .is_none_or(|r| (f64::from(m.refresh_rate) / 1000.0 - r).abs() < 0.01)
    })
}

/// Translate a requested logical-monitor layout into per-connector niri actions.
/// Enabled connectors get on/mode/scale/transform/position; any known connector
/// absent from the request is turned off. Rejects mirroring, unknown
/// connectors/modes, and bad transforms rather than applying a lie.
fn plan_apply(
    outputs: &BTreeMap<String, NiriOutput>,
    logical_monitors: &[LogicalMonitorConfig],
) -> Result<Vec<(String, Vec<PlannedAction>)>, ApplyError> {
    let mut plan: Vec<(String, Vec<PlannedAction>)> = Vec::new();
    let mut requested: BTreeSet<String> = BTreeSet::new();

    for lm in logical_monitors {
        if lm.monitors.len() != 1 {
            return Err(ApplyError::Mirroring);
        }
        let transform =
            transform_from_mutter(lm.transform).ok_or(ApplyError::BadTransform(lm.transform))?;
        for mc in &lm.monitors {
            let o = outputs
                .get(&mc.connector)
                .ok_or_else(|| ApplyError::UnknownConnector(mc.connector.clone()))?;
            let cm = decode_mode_id(&mc.mode_id)
                .filter(|cm| mode_exists(o, cm))
                .ok_or_else(|| ApplyError::UnknownMode {
                    connector: mc.connector.clone(),
                    mode_id: mc.mode_id.clone(),
                })?;
            requested.insert(mc.connector.clone());
            plan.push((
                mc.connector.clone(),
                vec![
                    PlannedAction::On,
                    PlannedAction::Mode(cm),
                    PlannedAction::Scale(lm.scale),
                    PlannedAction::Transform(transform),
                    PlannedAction::Position(ConfiguredPosition { x: lm.x, y: lm.y }),
                ],
            ));
        }
    }

    for name in outputs.keys() {
        if !requested.contains(name) {
            plan.push((name.clone(), vec![PlannedAction::Off]));
        }
    }

    Ok(plan)
}

/// Lower a [`PlannedAction`] into the niri-ipc action to send.
fn to_output_action(a: PlannedAction) -> OutputAction {
    match a {
        PlannedAction::Off => OutputAction::Off,
        PlannedAction::On => OutputAction::On,
        PlannedAction::Mode(cm) => OutputAction::Mode {
            mode: ModeToSet::Specific(cm),
        },
        PlannedAction::Scale(s) => OutputAction::Scale {
            scale: ScaleToSet::Specific(s),
        },
        PlannedAction::Transform(t) => OutputAction::Transform { transform: t },
        PlannedAction::Position(p) => OutputAction::Position {
            position: PositionToSet::Specific(p),
        },
    }
}

/// Map a planning failure onto the D-Bus error g-c-c will surface.
fn apply_error_to_fdo(e: &ApplyError) -> zbus::fdo::Error {
    match e {
        ApplyError::UnknownConnector(c) => {
            zbus::fdo::Error::InvalidArgs(format!("unknown connector {c}"))
        }
        ApplyError::UnknownMode { connector, mode_id } => {
            zbus::fdo::Error::InvalidArgs(format!("unknown mode {mode_id} for {connector}"))
        }
        ApplyError::BadTransform(v) => {
            zbus::fdo::Error::InvalidArgs(format!("unsupported transform {v}"))
        }
        ApplyError::Mirroring => {
            zbus::fdo::Error::NotSupported("mirroring is not supported under niri".to_owned())
        }
    }
}

// ── Blocking niri IPC (run on the blocking pool) ──────────────────────────────

/// Query niri for the current outputs, sorted by connector. Returns empty on
/// any IPC failure (caller treats empty as "transient, skip").
fn query_outputs_raw() -> BTreeMap<String, NiriOutput> {
    let mut socket = match Socket::connect() {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "display_config: niri socket connect failed");
            return BTreeMap::new();
        }
    };
    match socket.send(Request::Outputs) {
        Ok(Ok(Response::Outputs(map))) => map.into_iter().collect(),
        Ok(Ok(other)) => {
            tracing::warn!(?other, "display_config: unexpected reply for Outputs");
            BTreeMap::new()
        }
        Ok(Err(msg)) => {
            tracing::warn!(error = %msg, "display_config: niri returned error for Outputs");
            BTreeMap::new()
        }
        Err(e) => {
            tracing::debug!(error = %e, "display_config: send Outputs failed");
            BTreeMap::new()
        }
    }
}

/// Send a plan to niri, one fresh short-lived socket per action (niri's IPC is
/// one-request-per-connection). Stops at the first hard failure.
fn apply_plan_blocking(plan: Vec<(String, Vec<PlannedAction>)>) -> Result<(), String> {
    for (connector, actions) in plan {
        for action in actions {
            let mut socket = Socket::connect().map_err(|e| format!("connect: {e}"))?;
            let req = Request::Output {
                output: connector.clone(),
                action: to_output_action(action),
            };
            match socket.send(req).map_err(|e| format!("send: {e}"))? {
                Ok(_) => {}
                Err(msg) => return Err(format!("niri({connector}): {msg}")),
            }
        }
    }
    Ok(())
}

// ── Change detection / signal ─────────────────────────────────────────────────

/// Cheap change key: connector, enabled, current mode, position, scale,
/// transform. Any topology or config change flips it → serial bump + signal.
fn fingerprint(outputs: &BTreeMap<String, NiriOutput>) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for (name, o) in outputs {
        let (lx, ly, scale, tf) = o.logical.map_or((0, 0, 0.0, 0), |l| {
            (l.x, l.y, l.scale, transform_to_mutter(l.transform))
        });
        let mode = o
            .current_mode
            .and_then(|i| o.modes.get(i))
            .map_or_else(|| "off".to_owned(), encode_mode_id);
        let _ = write!(
            s,
            "{name}|{}|{mode}|{lx},{ly}|{scale:.4}|{tf};",
            o.logical.is_some()
        );
    }
    s
}

/// Emit `MonitorsChanged` on the owned connection.
async fn emit_monitors_changed(ownership: &OwnNameSignal) {
    let result = ownership
        .emit(DISPLAY_CONFIG_PATH, |emitter| async move {
            DisplayConfigIface::monitors_changed(&emitter).await
        })
        .await;
    if let Err(e) = result {
        tracing::warn!(error = %e, "MonitorsChanged emit failed");
    }
}

/// Background loop: poll niri outputs, bump the serial and emit
/// `MonitorsChanged` whenever the fingerprint changes.
async fn monitor_loop(serial: Arc<AtomicU32>, ownership: OwnNameSignal) {
    let mut last: Option<String> = None;
    loop {
        let outputs = tokio::task::spawn_blocking(query_outputs_raw)
            .await
            .unwrap_or_default();
        // Empty ⇒ niri unreachable this tick (a real session always has ≥1
        // output). Skip so a blip can't flip-flop the serial.
        if outputs.is_empty() {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }
        let fp = fingerprint(&outputs);
        match &last {
            Some(prev) if *prev == fp => {}
            None => last = Some(fp),
            Some(_) => {
                serial.fetch_add(1, Ordering::SeqCst);
                last = Some(fp);
                emit_monitors_changed(&ownership).await;
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// ── Service ───────────────────────────────────────────────────────────────────

/// Service marker registered via `App::with(display_config::service())`.
pub struct DisplayConfigService;

#[doc(hidden)]
pub struct DisplayConfigHandles {
    /// Keeps the name-ownership task (and thus the owned name + mounted object)
    /// alive for the process lifetime. Held, not read.
    _ownership: OwnNameSignal,
    /// Kept so the `Arc` shared with the poll loop + interface isn't dropped.
    _serial: Arc<AtomicU32>,
}

impl Service for DisplayConfigService {
    type Handles = DisplayConfigHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        // Start above 0 so a client that never called GetCurrentState (serial 0)
        // can't accidentally match.
        let serial = Arc::new(AtomicU32::new(1));
        let iface = DisplayConfigIface {
            serial: serial.clone(),
        };
        let ownership = own_name(DISPLAY_CONFIG_NAME)
            .at_path(DISPLAY_CONFIG_PATH, iface)
            .start();

        let serial_loop = serial.clone();
        let ownership_loop = ownership.clone();
        spawn_supervised("display_config", move || {
            let serial_loop = serial_loop.clone();
            let ownership_loop = ownership_loop.clone();
            async move {
                monitor_loop(serial_loop, ownership_loop).await;
            }
        });

        DisplayConfigHandles {
            _ownership: ownership,
            _serial: serial,
        }
    }
}

/// Returns the `DisplayConfig` service to register with the hytte runtime.
#[must_use]
pub fn service() -> DisplayConfigService {
    DisplayConfigService
}

// ── D-Bus interface ───────────────────────────────────────────────────────────

/// Server implementation of `org.gnome.Mutter.DisplayConfig`. `Clone` is
/// required by `OwnNameBuilder::at_path` (the object server re-mounts a clone on
/// reconnect); the shared serial rides in an `Arc` so all clones agree.
#[derive(Clone)]
struct DisplayConfigIface {
    serial: Arc<AtomicU32>,
}

// The property getters return constants and don't touch `&self`; allow at the
// impl block rather than per-method.
#[allow(clippy::unused_self)]
#[zbus::interface(name = "org.gnome.Mutter.DisplayConfig")]
impl DisplayConfigIface {
    /// Enumerate current niri outputs into the Mutter monitors +
    /// logical-monitors model. The returned `serial` must be echoed back in a
    /// later `ApplyMonitorsConfig`.
    async fn get_current_state(&self) -> CurrentState {
        let serial = self.serial.load(Ordering::SeqCst);
        let outputs = tokio::task::spawn_blocking(query_outputs_raw)
            .await
            .unwrap_or_default();
        build_current_state(serial, &outputs)
    }

    /// Apply a requested logical-monitor layout to niri.
    ///
    /// `method`: `0` = verify (validate only), `1` = temporary, `2` =
    /// persistent. niri-ipc applies are inherently temporary, so `2` currently
    /// applies live and logs that kanshi persistence is a follow-up (#393).
    async fn apply_monitors_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: Vec<LogicalMonitorConfig>,
        // Global apply properties (e.g. "layout-mode"); accepted, not acted on.
        properties: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        let _ = properties;
        let current = self.serial.load(Ordering::SeqCst);
        if serial != current {
            return Err(zbus::fdo::Error::AccessDenied(format!(
                "stale serial {serial} (current {current}); re-read GetCurrentState"
            )));
        }
        if method > 2 {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "unknown apply method {method}"
            )));
        }

        let outputs = tokio::task::spawn_blocking(query_outputs_raw)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("niri query join error: {e}")))?;
        let plan = plan_apply(&outputs, &logical_monitors).map_err(|e| apply_error_to_fdo(&e))?;

        // method 0 = verify: the plan built, so the config is applicable.
        if method == 0 {
            return Ok(());
        }
        if method == 2 {
            tracing::warn!(
                "ApplyMonitorsConfig(persistent): niri-ipc applies are temporary; kanshi-profile persistence is a #393 follow-up. Applying live."
            );
        }

        tokio::task::spawn_blocking(move || apply_plan_blocking(plan))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("niri apply join error: {e}")))?
            .map_err(zbus::fdo::Error::Failed)?;
        Ok(())
    }

    /// Whether clients may call `ApplyMonitorsConfig`. Always `true` here.
    #[zbus(property)]
    fn apply_monitors_config_allowed(&self) -> bool {
        true
    }

    /// Whether the compositor manages panel orientation itself. niri doesn't
    /// expose accelerometer-driven auto-rotation over IPC, so `false`.
    #[zbus(property)]
    fn panel_orientation_managed(&self) -> bool {
        false
    }

    /// Emitted when niri's output topology or configuration changes.
    #[zbus(signal)]
    async fn monitors_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Fixtures ──────────────────────────────────────────────────────────────

    fn mk_mode(width: u16, height: u16, refresh_mhz: u32, preferred: bool) -> NiriMode {
        NiriMode {
            width,
            height,
            refresh_rate: refresh_mhz,
            is_preferred: preferred,
        }
    }

    fn mk_logical(x: i32, y: i32, scale: f64) -> niri_ipc::LogicalOutput {
        niri_ipc::LogicalOutput {
            x,
            y,
            width: 0,
            height: 0,
            scale,
            transform: NiriTransform::Normal,
        }
    }

    /// An enabled output at `(x,y)` with a single 1080p60 mode as current.
    fn mk_output(name: &str, x: i32, y: i32, scale: f64, enabled: bool) -> NiriOutput {
        NiriOutput {
            name: name.to_owned(),
            make: "ACME".to_owned(),
            model: "Screen".to_owned(),
            serial: Some("S1".to_owned()),
            physical_size: Some((600, 340)),
            modes: vec![
                mk_mode(1920, 1080, 60_000, true),
                mk_mode(1280, 720, 60_000, false),
            ],
            current_mode: if enabled { Some(0) } else { None },
            is_custom_mode: false,
            vrr_supported: false,
            vrr_enabled: false,
            logical: enabled.then(|| mk_logical(x, y, scale)),
        }
    }

    fn outputs_map(list: Vec<NiriOutput>) -> BTreeMap<String, NiriOutput> {
        list.into_iter().map(|o| (o.name.clone(), o)).collect()
    }

    fn cfg(connector: &str, mode_id: &str, x: i32, y: i32, scale: f64) -> LogicalMonitorConfig {
        LogicalMonitorConfig {
            x,
            y,
            scale,
            transform: 0,
            primary: false,
            monitors: vec![MonitorConfig {
                connector: connector.to_owned(),
                mode_id: mode_id.to_owned(),
                properties: HashMap::new(),
            }],
        }
    }

    // ── Transform mapping ─────────────────────────────────────────────────────

    #[test]
    fn transform_round_trips_all_variants() {
        for t in [
            NiriTransform::Normal,
            NiriTransform::_90,
            NiriTransform::_180,
            NiriTransform::_270,
            NiriTransform::Flipped,
            NiriTransform::Flipped90,
            NiriTransform::Flipped180,
            NiriTransform::Flipped270,
        ] {
            assert_eq!(transform_from_mutter(transform_to_mutter(t)), Some(t));
        }
    }

    #[test]
    fn transform_from_mutter_rejects_out_of_range() {
        assert_eq!(transform_from_mutter(8), None);
        assert_eq!(transform_from_mutter(99), None);
    }

    // ── Mode id round-trip ────────────────────────────────────────────────────

    #[test]
    fn mode_id_round_trips() {
        let m = mk_mode(2560, 1440, 59_951, true);
        let id = encode_mode_id(&m);
        assert_eq!(id, "2560x1440@59951");
        let cm = decode_mode_id(&id).expect("decode");
        assert_eq!(cm.width, 2560);
        assert_eq!(cm.height, 1440);
        assert!((cm.refresh.expect("refresh") - 59.951).abs() < 1e-9);
    }

    #[test]
    fn decode_mode_id_rejects_garbage() {
        assert!(decode_mode_id("nonsense").is_none());
        assert!(decode_mode_id("1920x1080").is_none());
        assert!(decode_mode_id("1920@60000").is_none());
        assert!(decode_mode_id("axb@c").is_none());
    }

    // ── Scale model ───────────────────────────────────────────────────────────

    #[test]
    fn scale_steps_matches_range() {
        // Guards the hand-computed SCALE_STEPS against the three float consts.
        let expected = ((SCALE_MAX - SCALE_MIN) / SCALE_STEP).round();
        assert!((f64::from(SCALE_STEPS) - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn supported_scales_always_include_native() {
        for (w, h) in [(1920, 1080), (3840, 2160), (800, 600), (640, 480)] {
            assert!(
                supported_scales(w, h).contains(&1.0),
                "native scale missing for {w}x{h}"
            );
        }
    }

    #[test]
    fn supported_scales_respect_logical_floor() {
        // Every offered fractional scale keeps logical size above the floor.
        for &s in &supported_scales(3840, 2160) {
            if (s - 1.0).abs() < f64::EPSILON {
                continue;
            }
            assert!(f64::from(3840) / s >= MIN_LOGICAL_PX);
            assert!(f64::from(2160) / s >= MIN_LOGICAL_PX);
        }
        // 4K offers a rich list; a tiny mode collapses to native only.
        assert!(supported_scales(3840, 2160).len() >= 5);
        assert_eq!(supported_scales(640, 480), vec![1.0]);
    }

    #[test]
    fn preferred_scale_is_offered() {
        for (w, h) in [(1920, 1080), (3840, 2160), (2560, 1440)] {
            let p = preferred_scale(w, h);
            assert!(supported_scales(w, h).contains(&p));
        }
    }

    // ── Primary selection ─────────────────────────────────────────────────────

    #[test]
    fn primary_is_top_left_enabled_output() {
        let map = outputs_map(vec![
            mk_output("DP-1", 1920, 0, 1.0, true),
            mk_output("eDP-1", 0, 0, 1.0, true),
            mk_output("HDMI-A-1", 0, 0, 1.0, false), // disabled: ineligible
        ]);
        assert_eq!(pick_primary(&map).as_deref(), Some("eDP-1"));
    }

    #[test]
    fn primary_none_when_all_disabled() {
        let map = outputs_map(vec![mk_output("DP-1", 0, 0, 1.0, false)]);
        assert_eq!(pick_primary(&map), None);
    }

    // ── Logical monitor mapping ───────────────────────────────────────────────

    #[test]
    fn logical_monitor_reflects_niri_and_skips_disabled() {
        let enabled = mk_output("DP-1", 100, 50, 1.5, true);
        let lm = build_logical_monitor(&enabled, Some("DP-1")).expect("enabled → logical");
        assert_eq!((lm.x, lm.y), (100, 50));
        assert!((lm.scale - 1.5).abs() < f64::EPSILON);
        assert_eq!(lm.transform, 0);
        assert!(lm.primary);
        assert_eq!(lm.monitors.len(), 1);
        assert_eq!(lm.monitors[0].connector, "DP-1");

        let disabled = mk_output("DP-2", 0, 0, 1.0, false);
        assert!(build_logical_monitor(&disabled, None).is_none());
    }

    #[test]
    fn current_state_counts_monitors_and_logicals() {
        let map = outputs_map(vec![
            mk_output("eDP-1", 0, 0, 1.0, true),
            mk_output("DP-1", 1920, 0, 1.0, true),
            mk_output("HDMI-A-1", 0, 0, 1.0, false),
        ]);
        let (serial, monitors, logicals, props) = build_current_state(7, &map);
        assert_eq!(serial, 7);
        assert_eq!(monitors.len(), 3, "all connected outputs listed");
        assert_eq!(
            logicals.len(),
            2,
            "only enabled outputs are logical monitors"
        );
        assert!(props.contains_key("layout-mode"));
    }

    // ── Apply planning ────────────────────────────────────────────────────────

    #[test]
    fn plan_enables_requested_and_disables_absent() {
        let map = outputs_map(vec![
            mk_output("eDP-1", 0, 0, 1.0, true),
            mk_output("DP-1", 1920, 0, 1.0, true),
        ]);
        // Request only eDP-1; DP-1 must be turned off.
        let plan =
            plan_apply(&map, &[cfg("eDP-1", "1920x1080@60000", 0, 0, 2.0)]).expect("valid plan");

        let edp = &plan
            .iter()
            .find(|(c, _)| c == "eDP-1")
            .expect("eDP-1 present")
            .1;
        assert_eq!(
            edp,
            &vec![
                PlannedAction::On,
                PlannedAction::Mode(ConfiguredMode {
                    width: 1920,
                    height: 1080,
                    refresh: Some(60.0),
                }),
                PlannedAction::Scale(2.0),
                PlannedAction::Transform(NiriTransform::Normal),
                PlannedAction::Position(ConfiguredPosition { x: 0, y: 0 }),
            ]
        );

        let dp = &plan
            .iter()
            .find(|(c, _)| c == "DP-1")
            .expect("DP-1 present")
            .1;
        assert_eq!(dp, &vec![PlannedAction::Off]);
    }

    #[test]
    fn plan_rejects_unknown_connector() {
        let map = outputs_map(vec![mk_output("eDP-1", 0, 0, 1.0, true)]);
        let err = plan_apply(&map, &[cfg("DP-9", "1920x1080@60000", 0, 0, 1.0)]).unwrap_err();
        assert_eq!(err, ApplyError::UnknownConnector("DP-9".to_owned()));
    }

    #[test]
    fn plan_rejects_unknown_mode() {
        let map = outputs_map(vec![mk_output("eDP-1", 0, 0, 1.0, true)]);
        // 4K mode not in the output's mode list.
        let err = plan_apply(&map, &[cfg("eDP-1", "3840x2160@60000", 0, 0, 1.0)]).unwrap_err();
        assert_eq!(
            err,
            ApplyError::UnknownMode {
                connector: "eDP-1".to_owned(),
                mode_id: "3840x2160@60000".to_owned(),
            }
        );
    }

    #[test]
    fn plan_rejects_bad_transform() {
        let map = outputs_map(vec![mk_output("eDP-1", 0, 0, 1.0, true)]);
        let mut c = cfg("eDP-1", "1920x1080@60000", 0, 0, 1.0);
        c.transform = 42;
        let err = plan_apply(&map, &[c]).unwrap_err();
        assert_eq!(err, ApplyError::BadTransform(42));
    }

    #[test]
    fn plan_rejects_mirroring() {
        let map = outputs_map(vec![
            mk_output("eDP-1", 0, 0, 1.0, true),
            mk_output("DP-1", 0, 0, 1.0, true),
        ]);
        let mut c = cfg("eDP-1", "1920x1080@60000", 0, 0, 1.0);
        c.monitors.push(MonitorConfig {
            connector: "DP-1".to_owned(),
            mode_id: "1920x1080@60000".to_owned(),
            properties: HashMap::new(),
        });
        assert_eq!(plan_apply(&map, &[c]).unwrap_err(), ApplyError::Mirroring);
    }

    // ── Fingerprint ───────────────────────────────────────────────────────────

    #[test]
    fn fingerprint_changes_on_scale_and_position() {
        let base = outputs_map(vec![mk_output("eDP-1", 0, 0, 1.0, true)]);
        let scaled = outputs_map(vec![mk_output("eDP-1", 0, 0, 2.0, true)]);
        let moved = outputs_map(vec![mk_output("eDP-1", 100, 0, 1.0, true)]);
        let off = outputs_map(vec![mk_output("eDP-1", 0, 0, 1.0, false)]);
        let fp = fingerprint(&base);
        assert_ne!(fp, fingerprint(&scaled));
        assert_ne!(fp, fingerprint(&moved));
        assert_ne!(fp, fingerprint(&off));
        assert_eq!(fp, fingerprint(&base), "stable for identical state");
    }
}
