//! Display / output enumeration via niri's IPC.
//!
//! Polls `Request::Outputs` every 2 s and emits a deduped `Vec<Output>` on
//! topology change (plug/unplug, mode switch, scale change, etc.). Persistent
//! per-profile layout lives in `kanshi` (see `etc/kanshi/`); this service
//! only reflects niri's runtime state.
//!
//! v1 is read-only with one exception: `set_output_enabled` toggles a single
//! connector on/off via `Request::Output { OutputAction::On | Off }`,
//! fire-and-forget. The next poll picks up the resulting state.
//!
//! Polling (vs subscribing to a niri event stream) is the v1 choice for
//! simplicity; the lag is fine for a passive page. Swap the inner loop body
//! to upgrade.
//!
//! Kept separate from `niri.rs` because that module owns the long-lived
//! event-stream socket; outputs aren't pushed on that stream so the polling
//! shape lives here.

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, runtime, Service};
use niri_ipc::socket::Socket;
use niri_ipc::{OutputAction, Request, Response, Transform as NiriTransform};
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

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = DisplaysHandles::default();
        let writer = handles.outputs.clone();

        rt.spawn(async move {
            poll_loop(writer).await;
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
        let action = if on { OutputAction::On } else { OutputAction::Off };
        if let Err(e) = send_output_action(&name, action) {
            tracing::warn!(output = %name, on, error = %e, "displays: output toggle failed");
        }
    });
}

fn send_output_action(name: &str, action: OutputAction) -> anyhow::Result<()> {
    let mut sock = Socket::connect().map_err(|e| anyhow::anyhow!("connect: {e}"))?;
    let req = Request::Output { output: name.to_string(), action };
    match sock.send(req).map_err(|e| anyhow::anyhow!("send: {e}"))? {
        Ok(_) => Ok(()),
        Err(msg) => Err(anyhow::anyhow!("niri: {msg}")),
    }
}

// ── Polling loop ─────────────────────────────────────────────────────────────

const POLL_INTERVAL: Duration = Duration::from_secs(2);

async fn poll_loop(writer: Mutable<Vec<Output>>) {
    loop {
        // niri-ipc's Socket is a blocking unix-socket client; run it on a
        // blocking pool so we don't park a tokio worker for the round-trip.
        let snapshot = tokio::task::spawn_blocking(query_outputs)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "displays: poll task panicked");
                Vec::new()
            });

        // PartialEq dedup: only write when the snapshot actually differs
        // from the cached one, otherwise downstream signals re-fire for no
        // reason every 2 s.
        let changed = {
            let cur = writer.lock_ref();
            *cur != snapshot
        };
        if changed {
            writer.set(snapshot);
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn query_outputs() -> Vec<Output> {
    let mut socket = match Socket::connect() {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "displays: niri socket connect failed");
            return Vec::new();
        }
    };

    let reply = match socket.send(Request::Outputs) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "displays: niri send Outputs failed");
            return Vec::new();
        }
    };

    let map = match reply {
        Ok(Response::Outputs(map)) => map,
        Ok(other) => {
            tracing::warn!(?other, "displays: unexpected reply for Outputs");
            return Vec::new();
        }
        Err(msg) => {
            tracing::warn!(error = %msg, "displays: niri returned error for Outputs");
            return Vec::new();
        }
    };

    let mut list: Vec<Output> = map.into_values().map(convert).collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

fn convert(o: niri_ipc::Output) -> Output {
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
        name: o.name,
        make: o.make,
        model: o.model,
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
