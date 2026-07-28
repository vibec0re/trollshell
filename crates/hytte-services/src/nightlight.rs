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
//! # Coordinates (#577)
//!
//! `wlsunset`'s geo mode needs a latitude and a longitude. Those used to be
//! baked into the unit's `ExecStart` at nix-eval time, which meant the Night
//! light switch was a silent no-op for anyone who had not hand-written their
//! coordinates into `programs.trollshell.nightlight.{latitude,longitude}` —
//! flipping it started a unit that printed a hint and exited 0.
//!
//! Nix-eval time is the wrong layer to consult a runtime daemon, so resolution
//! moved here, to the same place the toggle happens. [`set_enabled`] resolves
//! coordinates *before* starting the unit, in priority order:
//!
//! 1. **The live location fix** — `places::shared_location()`, the same
//!    tokio-side handle `weather.rs` reads (the registry accessor
//!    `places::current()` is thread-local to the GTK main thread and so is
//!    unreachable from here). That handle is the *effective* location, so the
//!    `TROLLSHELL_WEATHER_CITY` fallback and the control-center's manual place
//!    override feed night light too, for free.
//! 2. **The configured static coordinates** — `$TROLLSHELL_NIGHTLIGHT_LATITUDE`
//!    / `$TROLLSHELL_NIGHTLIGHT_LONGITUDE`, rendered from the nix options by
//!    `nix/hm-module.nix`. These are the override for anyone who does not want
//!    `GeoClue` running at all: with no location service there is no fix, so
//!    step 1 finds nothing and these win.
//! 3. **Nothing** — the unit is *not* started, a `warn!` says why, and
//!    `enabled` is re-published as `false` so the switch snaps back instead of
//!    sitting on over a dead daemon.
//!
//! The recorded reason this was deferred — "it resolves asynchronously, so
//! seeding at unit-start races the first fix" — is handled by resolving on the
//! async side: a [`LocationState::Resolving`] state means the first fix is
//! still in flight, and we simply *await* it (bounded by [`FIX_WAIT`], so a
//! wedged `GeoClue` degrades to step 2 rather than hanging the toggle).
//!
//! The resolved coordinates reach `wlsunset` through
//! `~/.config/trollshell/wlsunset.args` — one argument per line, read back by
//! the unit's `ExecStart` — which is precisely how the Appearance picker hands
//! `swaybg` its per-output argument vector. Temperatures stay in the unit
//! (`-t`/`-T` from the nix options): they have no runtime source.
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
//! - **Coordinates are resolved at toggle time, not tracked.** A fix that moves
//!   while night light is already on does not restart the daemon; the new
//!   coordinates land on the next toggle. Sunrise/sunset barely move over the
//!   distances a laptop covers in a session, so a re-exec would cost a visible
//!   gamma flicker for nothing.

use crate::geoclue::LocationState;
use crate::{config_file, places};
use futures_signals::signal::{Mutable, Signal, SignalExt as _};
use futures_util::StreamExt as _;
use hytte_reactive::{Service, registry, runtime};
use std::process::Stdio;
use std::time::Duration;

/// The user unit the shell toggles. Its `ExecStart` (declared in the
/// home-manager module) reads the coordinate arguments this module writes to
/// [`ARGS_FILE`] and execs `wlsunset` with them plus the configured
/// temperatures.
const UNIT: &str = "wlsunset.service";

/// Coordinate argument vector handed to the unit, one argument per line, under
/// `~/.config/trollshell/`. Mirrors `wallpaper.rs`'s `swaybg.args`.
const ARGS_FILE: &str = "wlsunset.args";

/// Static-coordinate override env vars, rendered from
/// `programs.trollshell.nightlight.{latitude,longitude}` by `nix/hm-module.nix`.
const LAT_ENV: &str = "TROLLSHELL_NIGHTLIGHT_LATITUDE";
const LON_ENV: &str = "TROLLSHELL_NIGHTLIGHT_LONGITUDE";

/// How long a toggle waits for `GeoClue`'s first fix when location resolution
/// is still in flight ([`LocationState::Resolving`]).
///
/// Chosen for an *interactive* wait: the user has just flipped a switch and is
/// watching it. A warm fix lands in well under a second and a cold one
/// (network/Wi-Fi trilateration on a slow link) in a few, so 10 s clears the
/// realistic cases with room to spare while still bounding a wedged
/// `GeoClue2` to roughly one "…did that work?" beat before falling through to
/// the static coordinates.
const FIX_WAIT: Duration = Duration::from_secs(10);

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
/// startup from `systemctl --user show -p ActiveState`; updated by
/// [`set_enabled`] on every toggle attempt.
pub fn enabled() -> impl Signal<Item = bool> {
    registry::with(|r| {
        r.get::<NightlightHandles>()
            .expect("nightlight::service() not registered")
            .enabled
            .signal()
    })
}

/// Toggle the night-light unit. Fire-and-forget.
///
/// Turning it **on** first resolves coordinates (see the module docs: live fix
/// → configured static override → nothing), writes them to [`ARGS_FILE`], and
/// only then runs `systemctl --user start wlsunset.service`. With no
/// coordinates the unit is deliberately *not* started — starting it would just
/// print a hint and exit 0, which is the silent no-op this whole path exists to
/// remove.
///
/// Turning it **off** is an unconditional `systemctl --user stop`.
///
/// The `enabled` signal is always re-published afterwards, even when the value
/// does not change (`Mutable::set` notifies unconditionally): on success with
/// the requested state, otherwise with the state the daemon is *actually* in,
/// re-read from `ActiveState`. That re-publish is what makes the Appearance
/// drawer's switch snap back when a toggle could not be honoured, instead of
/// leaving the widget showing "on" over a stopped daemon.
pub fn set_enabled(on: bool) {
    let enabled = registry::with(|r| r.get::<NightlightHandles>().map(|h| h.enabled.clone()));
    let Some(enabled) = enabled else {
        // Service not registered (test harness?) — nothing to toggle.
        tracing::warn!("nightlight: service not registered");
        return;
    };

    if !on {
        runtime::handle().spawn_blocking(move || apply(&enabled, false));
        return;
    }

    runtime::handle().spawn(async move {
        let Some(coords) = resolve_coords().await else {
            tracing::warn!(
                "nightlight: no coordinates — {UNIT} not started. Enable GeoClue2 (or set \
                 $TROLLSHELL_WEATHER_CITY), or set \
                 programs.trollshell.nightlight.{{latitude,longitude}}"
            );
            enabled.set(false);
            return;
        };
        tracing::debug!(
            lat = coords.lat,
            lon = coords.lon,
            source = coords.source,
            "nightlight: resolved coordinates"
        );
        let join = tokio::task::spawn_blocking(move || {
            if config_file::write("nightlight", ARGS_FILE, &args_file_body(coords)) {
                apply(&enabled, true);
            } else {
                // No args file means the unit would start inert; don't lie.
                enabled.set(read_active_state());
            }
        })
        .await;
        if let Err(e) = join {
            tracing::warn!(error = %e, "nightlight: start task failed to join");
        }
    });
}

// ── Coordinate resolution ────────────────────────────────────────────────────

/// A resolved latitude/longitude pair plus where it came from (logging only).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Coords {
    lat: f64,
    lon: f64,
    source: &'static str,
}

impl Coords {
    /// Coordinates sourced from the live location fix.
    fn live(lat: f64, lon: f64) -> Self {
        Self {
            lat,
            lon,
            source: "location",
        }
    }
}

/// Resolve the coordinates to start `wlsunset` with: the live location fix
/// first, then the configured static override, then nothing. See the module
/// docs for why that order.
async fn resolve_coords() -> Option<Coords> {
    if let Some(c) = live_coords().await {
        return Some(c);
    }
    static_coords()
}

/// Coordinates from the live location fix, via the tokio-side `places` handle
/// (`weather.rs`'s bridge). `None` when `places` isn't registered, the fix is
/// unavailable, or the first fix hasn't landed within [`FIX_WAIT`].
async fn live_coords() -> Option<Coords> {
    let Some(location) = places::shared_location() else {
        tracing::debug!("nightlight: places not registered; no live fix");
        return None;
    };
    match location.get_cloned() {
        LocationState::Resolved(loc) => return Some(Coords::live(loc.lat, loc.lon)),
        LocationState::Unavailable => return None,
        // First attempt still in flight — this is the race the original
        // deferral cited. Await it rather than guessing.
        LocationState::Resolving => {}
    }
    if let Ok(coords) = tokio::time::timeout(FIX_WAIT, first_fix(&location)).await {
        coords
    } else {
        tracing::warn!(
            secs = FIX_WAIT.as_secs(),
            "nightlight: location still resolving; falling back to configured coordinates"
        );
        None
    }
}

/// Await the first non-[`LocationState::Resolving`] state on `location`.
/// `None` when resolution lands on `Unavailable` (or the handle is dropped).
/// Signal-driven, so it parks rather than polls.
async fn first_fix(location: &Mutable<LocationState>) -> Option<Coords> {
    let mut states = location.signal_cloned().to_stream();
    while let Some(state) = states.next().await {
        match state {
            LocationState::Resolving => {}
            LocationState::Resolved(loc) => return Some(Coords::live(loc.lat, loc.lon)),
            LocationState::Unavailable => return None,
        }
    }
    None
}

/// Coordinates from the nix-configured static override. Both env vars must
/// parse to an in-range coordinate; a half-configured or nonsense pair is
/// ignored with a warning rather than handed to `wlsunset`.
fn static_coords() -> Option<Coords> {
    let lat = env_coord(LAT_ENV, 90.0);
    let lon = env_coord(LON_ENV, 180.0);
    match (lat, lon) {
        (Some(lat), Some(lon)) => Some(Coords {
            lat,
            lon,
            source: "config",
        }),
        (None, None) => None,
        _ => {
            tracing::warn!(
                "nightlight: only one of {LAT_ENV}/{LON_ENV} is usable; both are required"
            );
            None
        }
    }
}

/// Read one coordinate env var. `None` for unset/blank/unparseable/out-of-range
/// (`|value| > limit`); the latter two also warn, since they're a config bug.
fn env_coord(var: &str, limit: f64) -> Option<f64> {
    let raw = std::env::var(var).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let parsed = parse_coord(&raw, limit);
    if parsed.is_none() {
        tracing::warn!(var, raw, "nightlight: ignoring unusable coordinate");
    }
    parsed
}

/// Parse a decimal-degrees coordinate, rejecting blanks, non-numbers,
/// non-finite values, and anything outside `±limit`. Split out from
/// [`env_coord`] so it is testable without touching the process environment.
fn parse_coord(raw: &str, limit: f64) -> Option<f64> {
    let value: f64 = raw.trim().parse().ok()?;
    (value.is_finite() && value.abs() <= limit).then_some(value)
}

/// Serialize the coordinate arguments to the newline-delimited [`ARGS_FILE`]
/// body (one argument per line, trailing newline), read back a line at a time
/// by the unit's `ExecStart` — the `swaybg.args` shape. Rust's `f64` `Display`
/// is locale-independent and round-trips, so no trailing-zero padding reaches
/// `wlsunset`.
fn args_file_body(coords: Coords) -> String {
    format!("-l\n{}\n-L\n{}\n", coords.lat, coords.lon)
}

// ── Unit control ─────────────────────────────────────────────────────────────

/// Start or stop the unit, then publish the resulting state: the requested one
/// on success, else whatever `ActiveState` actually reports. Blocking — call
/// from `spawn_blocking`.
fn apply(enabled: &Mutable<bool>, on: bool) {
    let verb = if on { "start" } else { "stop" };
    let status = std::process::Command::new("systemctl")
        .args(["--user", verb, UNIT])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {
            enabled.set(on);
            return;
        }
        Ok(s) => tracing::warn!(
            ?s,
            verb,
            "nightlight: systemctl --user {verb} {UNIT} exited non-zero"
        ),
        Err(e) => tracing::warn!(error = %e, verb, "nightlight: failed to spawn systemctl"),
    }
    // The toggle didn't take — re-sync the signal to the daemon's truth so the
    // switch reflects reality instead of the request.
    enabled.set(read_active_state());
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

#[cfg(test)]
mod tests {
    use super::{Coords, args_file_body, parse_coord};

    #[test]
    fn parses_plain_and_padded_coordinates() {
        assert_eq!(parse_coord("52.52", 90.0), Some(52.52));
        assert_eq!(parse_coord("  13.405  ", 180.0), Some(13.405));
        assert_eq!(parse_coord("-33.87", 90.0), Some(-33.87));
        // nix renders floats with trailing zeros; they must still parse.
        assert_eq!(parse_coord("52.520000", 90.0), Some(52.52));
        assert_eq!(parse_coord("0", 90.0), Some(0.0));
    }

    #[test]
    fn rejects_blank_nonsense_and_out_of_range() {
        assert_eq!(parse_coord("", 90.0), None);
        assert_eq!(parse_coord("   ", 90.0), None);
        assert_eq!(parse_coord("north", 90.0), None);
        assert_eq!(parse_coord("nan", 90.0), None);
        assert_eq!(parse_coord("inf", 90.0), None);
        assert_eq!(parse_coord("90.1", 90.0), None);
        assert_eq!(parse_coord("-181", 180.0), None);
        // The poles and the antimeridian are legal.
        assert_eq!(parse_coord("90", 90.0), Some(90.0));
        assert_eq!(parse_coord("-180", 180.0), Some(-180.0));
    }

    #[test]
    fn args_file_is_one_argument_per_line() {
        let body = args_file_body(Coords::live(52.52, 13.405));
        assert_eq!(body, "-l\n52.52\n-L\n13.405\n");
        assert_eq!(
            body.lines().collect::<Vec<_>>(),
            ["-l", "52.52", "-L", "13.405"]
        );
    }

    #[test]
    fn args_file_keeps_negative_coordinates_intact() {
        let body = args_file_body(Coords::live(-33.87, -151.21));
        assert_eq!(
            body.lines().collect::<Vec<_>>(),
            ["-l", "-33.87", "-L", "-151.21"]
        );
    }
}
