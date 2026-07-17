//! Night-light (color-temperature) service.
//!
//! A blue-light / gamma toggle backed entirely by a `wlsunset` **user** unit —
//! the shell keeps zero state of its own, exactly like the wallpaper picker
//! (`wallpaper.rs`). The daemon owns everything; the shell only
//!
//! 1. **writes** — toggles the unit on/off with `systemctl --user start|stop
//!    wlsunset.service` off the GTK thread (a near-verbatim copy of
//!    `wallpaper.rs`'s `restart_swaybg_unit` helper, which shells
//!    `systemctl --user restart swaybg.service` via `spawn_blocking`), and
//! 2. **reads** — seeds `enabled` once at startup from the unit's `ActiveState`
//!    via `systemctl --user show -p ActiveState --value wlsunset.service`
//!    (mirroring `screensaver.rs`, which reads a user unit's `MainPID` the same
//!    way).
//!
//! Because `wlsunset` is a persistent user daemon, restarting `trollshell`
//! during development reconnects to whatever state the unit is already in —
//! no state to lose.
//!
//! # Scope (v1)
//!
//! - **Point-in-time read.** The `ActiveState` seed is a one-shot CLI read, not
//!   a subscription, matching both existing precedents (wallpaper write +
//!   screensaver read). If the unit dies or is toggled outside the shell,
//!   `enabled()` won't update until the next process start. Live fidelity would
//!   mean subscribing to the *session*-bus user manager's `JobRemoved` /
//!   unit `PropertiesChanged` (the `systemd.rs` shape re-pointed at
//!   `BusKind::Session`) — deliberately out of scope for v1.
//! - **Static coordinates.** The unit runs `wlsunset` in geo mode from lat/lon
//!   configured in the nix module. Seeding lat/lon from the `geoclue` service is
//!   a follow-up (it resolves asynchronously, so seeding at unit-start races the
//!   first fix).

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry, runtime};
use std::process::Stdio;

/// The user unit the shell toggles. Its `ExecStart` (declared in the
/// home-manager module) runs `wlsunset` in geo mode from static coordinates.
const UNIT: &str = "wlsunset.service";

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct NightlightHandles {
    pub(crate) enabled: Mutable<bool>,
}

/// Night-light service marker. Pass to `App::with` to register the service.
///
/// `start()` seeds `enabled` from the unit's `ActiveState` on a blocking
/// thread; thereafter `set_enabled` updates it. No polling loop — see the
/// module docs on the point-in-time read.
pub struct NightlightService;

impl Service for NightlightService {
    type Handles = NightlightHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NightlightHandles {
            enabled: Mutable::new(false),
        };
        // Seed the initial value off the GTK thread: a point-in-time read of the
        // unit's ActiveState. `Mutable` is `Send + Sync`, so the blocking task
        // writes back the result directly.
        let writer = handles.enabled.clone();
        rt.spawn_blocking(move || {
            writer.set(read_active_state());
        });
        handles
    }
}

#[must_use]
pub fn service() -> NightlightService {
    NightlightService
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Signal of whether the night-light unit is enabled (active). Seeded once at
/// startup from `systemctl --user show -p ActiveState`; updated optimistically
/// by [`set_enabled`] on a successful toggle.
pub fn enabled() -> impl Signal<Item = bool> {
    registry::with(|r| {
        r.get::<NightlightHandles>()
            .expect("nightlight::service() not registered")
            .enabled
            .signal()
    })
}

/// Toggle the night-light unit. Fire-and-forget: runs
/// `systemctl --user start|stop wlsunset.service` on a blocking thread and, on
/// success, optimistically updates the `enabled` signal so the UI (and any
/// other monitor's drawer) reflects the new state. On failure the signal is
/// left untouched (mirroring `wallpaper.rs`, which never reverts on a failed
/// reload) and the error is logged.
pub fn set_enabled(on: bool) {
    let enabled = registry::with(|r| r.get::<NightlightHandles>().map(|h| h.enabled.clone()));
    let Some(enabled) = enabled else {
        // Service not registered (test harness?) — nothing to toggle.
        tracing::warn!("nightlight: service not registered");
        return;
    };

    runtime::handle().spawn_blocking(move || {
        let verb = if on { "start" } else { "stop" };
        let status = std::process::Command::new("systemctl")
            .args(["--user", verb, UNIT])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => enabled.set(on),
            Ok(s) => tracing::warn!(
                ?s,
                verb,
                "nightlight: systemctl --user {verb} {UNIT} exited non-zero"
            ),
            Err(e) => tracing::warn!(error = %e, verb, "nightlight: failed to spawn systemctl"),
        }
    });
}

/// Read the unit's `ActiveState` via `systemctl --user show`. Returns `true`
/// only when the value is exactly `active`; any other state (`inactive`,
/// `failed`, `activating`, …), a non-zero exit, or a missing `systemctl` all
/// map to `false`. A point-in-time read — see the module docs.
fn read_active_state() -> bool {
    let out = std::process::Command::new("systemctl")
        .args(["--user", "show", "-p", "ActiveState", "--value", UNIT])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(out) if out.status.success() => {
            let state = String::from_utf8_lossy(&out.stdout);
            state.trim() == "active"
        }
        Ok(_) => false,
        Err(e) => {
            tracing::debug!(error = %e, "nightlight: could not read ActiveState (assuming off)");
            false
        }
    }
}
