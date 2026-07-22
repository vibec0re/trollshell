//! `mov.vibec0re.trollshell.Control` — the external control-center transport
//! (#390, the walking-skeleton foundation for the epic #381).
//!
//! The shell exposes a session-bus object implementing the
//! `mov.vibec0re.trollshell.Control` interface so the launch-on-demand
//! companion app (`trollshell-control-center`) — and, later, its per-tab
//! consumers (#391 Plugins · #392 Place/Location · #393 AI keys · #348
//! Display) — have something to bind to. The surface is deliberately trivial
//! for now: `Ping` and `Version`. Real capabilities land as the tabs do.
//!
//! ## Why a *dedicated* bus name, not the app's primary name
//!
//! The `adw::Application` already owns the well-known name
//! `mov.vibec0re.trollshell` on the session bus (single-instance
//! `GApplication`, see `main.rs`) and auto-exports its own action group over
//! `org.gtk.Actions` at `/mov/vibec0re/trollshell` (see [`crate::commands`]).
//! `hytte_bus::own_name` therefore **cannot** own the primary name — it's
//! taken, and racing `GApplication` for it would break single-instance.
//!
//! So this endpoint owns a distinct, unambiguous well-known name
//! [`CONTROL_NAME`] (`mov.vibec0re.trollshell.Control`) and mounts the object
//! at [`CONTROL_PATH`]. The two coexist cleanly on the bus: `GApplication`'s
//! `GActions` under the primary name for niri keybinds, this typed interface
//! under `.Control` for the companion app. Both live on the same session bus,
//! owned by the same process.
//!
//! ## Pattern
//!
//! Same shape as `hytte_services::screensaver`: a [`Service`] whose `start`
//! calls [`hytte::bus::own_name`] + mounts a `#[zbus::interface]` object, and
//! whose `Handles` hold the `OwnNameSignal` so the ownership task lives for the
//! process lifetime. No raw zbus connection is constructed here (that's
//! clippy-banned) — everything goes through `hytte_bus`.

use hytte::reactive::Service;

/// Dedicated well-known bus name for the control endpoint. Distinct from the
/// app's primary name `mov.vibec0re.trollshell` (owned by `GApplication`) — see
/// the module docs for why.
const CONTROL_NAME: &str = "mov.vibec0re.trollshell.Control";
/// Object path the [`ControlIface`] is mounted at.
const CONTROL_PATH: &str = "/mov/vibec0re/trollshell/Control";

/// Service marker registered via `App::with(control::service())`.
pub struct ControlService;

#[doc(hidden)]
pub struct ControlHandles {
    /// Keeps the name-ownership task alive for the process lifetime (dropping
    /// it would tear down the owned name). Held, not read.
    _ownership: hytte::bus::OwnNameSignal,
}

impl Service for ControlService {
    type Handles = ControlHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let ownership = hytte::bus::own_name(CONTROL_NAME)
            .at_path(CONTROL_PATH, ControlIface)
            .start();
        ControlHandles {
            _ownership: ownership,
        }
    }
}

/// Returns the control service to register with the hytte runtime.
#[must_use]
pub fn service() -> ControlService {
    ControlService
}

// ── D-Bus interface ───────────────────────────────────────────────────────────

/// Server implementation of `mov.vibec0re.trollshell.Control`. Unit struct —
/// the walking-skeleton surface is stateless. `Clone` is required by
/// `OwnNameBuilder::at_path` (the object server re-mounts a clone on reconnect).
#[derive(Clone)]
struct ControlIface;

// zbus's `#[interface]` macro requires every handler to be `async fn` even when
// the body doesn't await, and these trivially return constants without touching
// `&self`. Allow both at the impl block to keep the noise out of each method.
#[allow(clippy::unused_async, clippy::unused_self)]
#[zbus::interface(name = "mov.vibec0re.trollshell.Control")]
impl ControlIface {
    /// Liveness probe — returns `"pong"`. The companion app calls this on
    /// startup to decide whether the shell is running.
    async fn ping(&self) -> String {
        "pong".to_owned()
    }

    /// The running shell's package version (`CARGO_PKG_VERSION`), so the
    /// companion app can surface it and, later, gate on feature availability.
    async fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }
}
