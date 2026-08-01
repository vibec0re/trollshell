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
//! 1. **The configured static coordinates** — `$TROLLSHELL_NIGHTLIGHT_LATITUDE`
//!    / `$TROLLSHELL_NIGHTLIGHT_LONGITUDE`, rendered from the nix options by
//!    `nix/hm-module.nix`. Configuring these is an explicit statement of where
//!    you are, so they win outright: someone who writes coordinates into their
//!    config and *also* runs `GeoClue` gets the coordinates they wrote. They
//!    also short-circuit step 2 entirely, so a configured user never waits on
//!    a location fix they didn't ask for.
//! 2. **The live location fix** — `places::shared_location()`, the same
//!    tokio-side handle `weather.rs` reads (the registry accessor
//!    `places::current()` is thread-local to the GTK main thread and so is
//!    unreachable from here). That handle is the *effective* location, so the
//!    `TROLLSHELL_WEATHER_CITY` fallback and the control-center's manual place
//!    override feed night light too, for free. This is the zero-config path,
//!    and the one that makes the switch work out of the box.
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
//! That await can outlast the user's patience, so every toggle is
//! generation-stamped ([`Generation`]): a request that arrives while an earlier
//! one is still parked supersedes it, and the superseded task drops its result
//! instead of applying it over the newer state (#594).
//!
//! Correct is not the same as bearable, though, and up to [`FIX_WAIT`] of
//! nothing-happening is what provokes that second toggle in the first place. So
//! the wait is also *announced*: the toggle is a tri-state
//! ([`NightlightState`]) rather than a bool — `Off`, `On`, and `Resolving`,
//! "you asked for this and we are parked on a location fix" — and the Appearance
//! row spins and says so instead of sitting silent (#597). Only the branch that
//! actually parks publishes `Resolving`, so a toggle with configured
//! coordinates (or a warm fix) never flickers through it.
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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

// ── Toggle supersession ──────────────────────────────────────────────────────

/// Monotonic counter naming the most recent [`set_enabled`] request.
///
/// Turning night light **on** is not instantaneous: with no configured
/// coordinates it awaits a location fix for up to [`FIX_WAIT`] before doing
/// anything observable — which is exactly long enough for a user who sees
/// nothing happen to flip the switch back off. Unguarded, that off would
/// complete and then be silently undone: the still-parked task would start the
/// unit and re-publish `enabled = true`, warming the screen and moving the
/// switch by itself (#594).
///
/// So every `set_enabled` claims the next generation, and a task that had to
/// await re-checks its [`Ticket`] before touching anything. A newer request
/// owns both the unit and the `enabled` signal from the moment it claims, so a
/// superseded task's only correct move is to drop its result *entirely* —
/// including the failure re-publish, which would otherwise stamp a stale
/// reading over the newer state and turn one race into another.
///
/// A counter rather than a cancellation token: the only thing a late task needs
/// to know is "am I still the latest?", and that is one atomic load.
#[derive(Clone, Debug, Default)]
struct Generation(Arc<AtomicU64>);

impl Generation {
    /// Supersede every outstanding [`Ticket`] and return one for this request.
    ///
    /// Starts at 1, so a fresh counter (0) matches no ticket. `u64` at
    /// human toggle rates never wraps.
    fn claim(&self) -> Ticket {
        Ticket {
            at: self.0.fetch_add(1, Ordering::SeqCst) + 1,
            generation: self.clone(),
        }
    }
}

/// One `set_enabled` request's claim on the toggle. [`Ticket::is_current`] goes
/// false as soon as a later request claims its own generation.
///
/// `Clone` hands out another handle to the **same** claim, not a new one —
/// [`Generation::claim`] is the only thing that supersedes. That is what lets a
/// single request keep one ticket for its own guards while lending another to
/// the `on_wait` callback [`resolve_coords`] fires from deep inside the await.
#[derive(Clone, Debug)]
struct Ticket {
    generation: Generation,
    at: u64,
}

impl Ticket {
    /// Whether this request is still the latest — i.e. whether it may act.
    /// Cheap and non-consuming, so a task can re-check as late as it likes.
    fn is_current(&self) -> bool {
        self.generation.0.load(Ordering::SeqCst) == self.at
    }
}

// ── Toggle state ─────────────────────────────────────────────────────────────

/// What the night-light toggle is doing right now.
///
/// Three states rather than a bool because the toggle genuinely has three: with
/// no configured coordinates, turning it **on** parks on a location fix for up
/// to [`FIX_WAIT`], and during that window the unit is not running but the user
/// has already asked for it. Publishing `false` there would snap the switch
/// back out from under their hand — #594's symptom, arrived at from the other
/// direction — and publishing `true` would be a claim the UI has no way to
/// qualify. So the wait gets a state of its own, and the row can show it (#597).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NightlightState {
    /// The unit is stopped.
    #[default]
    Off,
    /// A toggle-on is in flight, parked on a location fix. The unit is *not*
    /// running yet; the switch reads on because that is what was requested.
    Resolving,
    /// The unit is active.
    On,
}

impl NightlightState {
    /// Whether a switch bound to this state should read "on".
    ///
    /// `Resolving` is **on**. The user has just flipped the switch and the
    /// request is being honoured, so moving it back under them would be exactly
    /// the "switch moves by itself" bug #594 was about. Callers that need "is
    /// the daemon actually running" want to match the state instead.
    #[must_use]
    pub fn is_on(self) -> bool {
        matches!(self, Self::On | Self::Resolving)
    }

    /// Whether a toggle-on is parked on a location fix — the cue the Appearance
    /// row's spinner and "waiting" subtitle hang off.
    #[must_use]
    pub fn is_resolving(self) -> bool {
        matches!(self, Self::Resolving)
    }

    /// Map a `systemctl` `ActiveState == active` reading onto a state. Never
    /// yields [`Self::Resolving`]: that is shell-side intent, not something the
    /// unit can report.
    fn from_unit_active(active: bool) -> Self {
        if active { Self::On } else { Self::Off }
    }
}

/// What an incoming toggle-**off** should publish immediately, before its
/// `systemctl stop` has even been spawned.
///
/// Only a pending [`NightlightState::Resolving`] notice is retracted, and only
/// because the shell put that notice up itself: the toggle-off has already
/// superseded the parked start (it claimed a newer [`Ticket`]), so "waiting for
/// a location fix" is false the instant it lands, and leaving it on screen for
/// the length of a `systemctl` round-trip would be precisely the dishonest
/// feedback #597 exists to remove. `On`/`Off` are the daemon's readings and stay
/// its to publish — [`apply`] re-publishes the truth right behind this either
/// way, so nothing here needs to guess at the outcome of the stop.
fn retraction_for(current: NightlightState) -> Option<NightlightState> {
    (current == NightlightState::Resolving).then_some(NightlightState::Off)
}

/// Publish `next` only if `ticket` is still the latest request; returns whether
/// it did, so the caller can log the drop.
///
/// The rule from #594: a superseded task publishes *nothing*. The request that
/// displaced it owns the signal, so a late write — even an accurate one — only
/// trades one race for another.
fn publish_if_current(
    state: &Mutable<NightlightState>,
    ticket: &Ticket,
    next: NightlightState,
) -> bool {
    if !ticket.is_current() {
        return false;
    }
    state.set(next);
    true
}

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct NightlightHandles {
    pub(crate) state: Mutable<NightlightState>,
    /// Supersession counter for in-flight [`set_enabled`] tasks — see
    /// [`Generation`]. Cloned out on the GTK thread with `state` and moved
    /// into tokio, like every other handle here.
    generation: Generation,
}

/// Night-light service marker. Pass to `App::with` to register the service.
///
/// `start()` seeds `state` from the unit's `ActiveState` on a blocking
/// thread; thereafter `set_enabled` updates it. No polling loop — see the
/// module docs on the point-in-time read.
pub struct NightlightService;

impl Service for NightlightService {
    type Handles = NightlightHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NightlightHandles {
            state: Mutable::new(NightlightState::Off),
            generation: Generation::default(),
        };
        // Seed the initial value off the GTK thread: a point-in-time read of the
        // unit's ActiveState. `Mutable` is `Send + Sync`, so the blocking task
        // writes back the result directly.
        //
        // Deliberately a bare `spawn_blocking` and *not*
        // `hytte_reactive::spawn_supervised_blocking`, which #654 proposed and
        // #690/#691 re-examined. Recording the answer here so the question stops
        // being re-asked from the outside: supervision means "re-run the closure
        // when it panics", and for this closure that is a no-op at best and a
        // regression at worst.
        //
        // - There is no panic to recover from. `read_active_state` is total:
        //   every outcome of the `systemctl` call — spawn failure, non-zero
        //   exit, non-UTF-8 output — is mapped to a `NightlightState`, with no
        //   unwrap on any path. A supervisor would never fire. Keep it that
        //   way; adding one would make the next point load-bearing.
        // - A *retried* seed would land late, and a late unconditional write is
        //   exactly the #594 bug. This `set` skips the `Generation` guard the
        //   rest of the module uses precisely because it runs in the
        //   milliseconds before the bar is drawn, so no toggle can have raced
        //   it. Retrying on the supervisor's backoff (1s, 2s, …) moves the
        //   write into a window where a user toggle *can* precede it, and the
        //   stale seed would then stamp itself over the newer state. Supervising
        //   this would mean giving it a `Ticket` first.
        // - It is not silent either way: `install_panic_hook` (#690) routes any
        //   panic on this thread through `tracing`, which is the visibility half
        //   of what supervision would have bought — and the half that mattered.
        let writer = handles.state.clone();
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

/// Signal of the night-light toggle's full state — see [`NightlightState`].
///
/// Seeded once at startup from `systemctl --user show -p ActiveState`; updated
/// by [`set_enabled`] on every toggle attempt, including the intermediate
/// [`NightlightState::Resolving`] a start publishes while it waits on a location
/// fix. This is the accessor a widget that wants to *show* the wait binds to.
pub fn state() -> impl Signal<Item = NightlightState> {
    registry::with(|r| {
        r.get::<NightlightHandles>()
            .expect("nightlight::service() not registered")
            .state
            .signal()
    })
}

/// Signal of whether the night-light toggle reads "on".
///
/// A projection of [`state`] through [`NightlightState::is_on`], so a start that
/// is still resolving coordinates reports `true` — see that method for why.
/// Kept for callers that only want the switch position; anything that needs to
/// *distinguish* the pending case wants [`state`].
pub fn enabled() -> impl Signal<Item = bool> {
    state().map(NightlightState::is_on)
}

/// Toggle the night-light unit. Fire-and-forget.
///
/// Turning it **on** first resolves coordinates (see the module docs:
/// configured static coordinates → live fix → nothing), writes them to
/// [`ARGS_FILE`], and
/// only then runs `systemctl --user start wlsunset.service`. With no
/// coordinates the unit is deliberately *not* started — starting it would just
/// print a hint and exit 0, which is the silent no-op this whole path exists to
/// remove.
///
/// Turning it **off** is an unconditional `systemctl --user stop`.
///
/// Every call claims a [`Generation`], so a toggle-on that is still parked on a
/// location fix when the next toggle arrives is superseded and drops out
/// without starting the unit or touching `state` (#594). A toggle-on that has
/// to park first publishes [`NightlightState::Resolving`], so the row can say
/// what it is waiting for instead of going silent (#597).
///
/// The state signal is re-published afterwards even when the value does not
/// change (`Mutable::set` notifies unconditionally): on success with the
/// requested state, otherwise with the state the daemon is *actually* in,
/// re-read from `ActiveState`. That re-publish is what makes the Appearance
/// drawer's switch snap back when a toggle could not be honoured, instead of
/// leaving the widget showing "on" over a stopped daemon. A *superseded* task
/// publishes nothing at all: the request that displaced it is authoritative.
pub fn set_enabled(on: bool) {
    let handles = registry::with(|r| {
        r.get::<NightlightHandles>()
            .map(|h| (h.state.clone(), h.generation.clone()))
    });
    let Some((state, generation)) = handles else {
        // Service not registered (test harness?) — nothing to toggle.
        tracing::warn!("nightlight: service not registered");
        return;
    };
    // Claim before branching, on the calling (GTK) thread: from here on this
    // request owns the unit and the signal, and any task still parked in
    // `resolve_coords` is superseded.
    let ticket = generation.claim();

    if !on {
        // Retract a pending notice the shell put up itself — the claim above
        // has already superseded whatever was parked, so the "waiting for a
        // location fix" row is false as of this line. See `retraction_for` for
        // why only that state, and never an optimistic `Off` over `On`.
        if let Some(retracted) = retraction_for(state.get()) {
            tracing::debug!("nightlight: toggle-off retracts the pending coordinate wait");
            state.set(retracted);
        }
        // The stop itself is deliberately unguarded: it is immediate, so it
        // cannot act on stale intent the way a parked start can. Skipping it
        // when superseded would be worse — a later toggle-on that fails to
        // resolve coordinates would leave the unit running under a switch
        // reading "off". Its *publish* is guarded all the same: the stop takes a
        // systemctl round-trip, and a toggle-on landing inside that window has
        // already put a `Resolving` up that this task must not overwrite.
        runtime::handle().spawn_blocking(move || apply(&state, &ticket, false));
        return;
    }

    runtime::handle().spawn(async move {
        // Fired by `resolve_coords` only from the branch that actually parks on
        // a location fix, so the configured-coordinates and warm-fix paths never
        // flicker through `Resolving` on their way to `On`. Guarded like every
        // other publish: a request the user has already overruled announces
        // nothing.
        let announce_wait = {
            let state = state.clone();
            let ticket = ticket.clone();
            move || {
                if publish_if_current(&state, &ticket, NightlightState::Resolving) {
                    tracing::debug!(
                        secs = FIX_WAIT.as_secs(),
                        "nightlight: parked on a location fix; publishing Resolving"
                    );
                }
            }
        };
        let Some(coords) = resolve_coords(announce_wait).await else {
            if !ticket.is_current() {
                tracing::debug!("nightlight: coordinate wait superseded; publishing nothing");
                return;
            }
            tracing::warn!(
                "nightlight: no coordinates — {UNIT} not started. Enable GeoClue2 (or set \
                 $TROLLSHELL_WEATHER_CITY), or set \
                 programs.trollshell.nightlight.{{latitude,longitude}}"
            );
            // Also clears any `Resolving` this request published: the switch
            // snaps back, and the row stops claiming it is still waiting.
            state.set(NightlightState::Off);
            return;
        };
        tracing::debug!(
            lat = coords.lat,
            lon = coords.lon,
            source = coords.source,
            "nightlight: resolved coordinates"
        );
        let join = tokio::task::spawn_blocking(move || {
            // Re-check on the blocking thread, not before the spawn: the later
            // the check, the narrower the window. Dropping out here also skips
            // an args-file write nobody will read.
            if !ticket.is_current() {
                tracing::debug!("nightlight: toggle-on superseded before the args write; dropping");
                return;
            }
            if !config_file::write("nightlight", ARGS_FILE, &args_file_body(coords)) {
                // No args file means the unit would start inert; don't lie —
                // unless a newer request has taken over the signal meanwhile.
                publish_if_current(&state, &ticket, read_active_state());
                return;
            }
            // Only that write separates this from the check above, but the unit
            // start is the irreversible half, so guard it in its own right.
            if ticket.is_current() {
                apply(&state, &ticket, true);
            } else {
                tracing::debug!("nightlight: toggle-on superseded before the unit start; dropping");
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

/// Resolve the coordinates to start `wlsunset` with: the configured static
/// coordinates first, then the live location fix, then nothing. See the module
/// docs for why that order.
///
/// Configured-first is also why a user who set coordinates never pays
/// [`FIX_WAIT`]: `static_coords()` is a synchronous env read, so it returns
/// before [`live_coords`] is ever awaited.
///
/// `on_wait` is called at most once, from the one branch that is about to block
/// for real (see [`live_coords`]). Threading it down here rather than firing it
/// at the call site is the whole point: only this code knows whether a given
/// toggle will actually wait, and announcing a wait that never happens would put
/// a spinner on screen for the few milliseconds a configured toggle takes.
async fn resolve_coords<F: FnOnce()>(on_wait: F) -> Option<Coords> {
    if let Some(c) = static_coords() {
        return Some(c);
    }
    live_coords(on_wait).await
}

/// Coordinates from the live location fix, via the tokio-side `places` handle
/// (`weather.rs`'s bridge). `None` when `places` isn't registered, the fix is
/// unavailable, or the first fix hasn't landed within [`FIX_WAIT`].
///
/// `on_wait` fires immediately before the bounded await and nowhere else — a
/// handle that is already `Resolved` or `Unavailable` answers synchronously, so
/// there is nothing to announce.
async fn live_coords<F: FnOnce()>(on_wait: F) -> Option<Coords> {
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
    // The only path that can cost the user real time. Say so before parking.
    on_wait();
    if let Ok(coords) = tokio::time::timeout(FIX_WAIT, first_fix(&location)).await {
        coords
    } else {
        tracing::warn!(
            secs = FIX_WAIT.as_secs(),
            "nightlight: location still resolving and no coordinates configured; not starting"
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
/// on success, else whatever `ActiveState` actually reports. Either way the
/// published value is terminal (`On`/`Off`), so this is also what clears a
/// [`NightlightState::Resolving`] notice off the row. Blocking — call from
/// `spawn_blocking`.
///
/// The `systemctl` call is unconditional — see the toggle-off branch of
/// [`set_enabled`] for why a stop must run even when superseded — but the
/// **publish** is ticket-guarded, because those are two different questions.
/// "Should this stop happen?" is always yes; "does this task still own the
/// signal?" is not. A `systemctl` round-trip is long enough for the next toggle
/// to land inside it, and once a wait is *visible* (#597) there is finally
/// something for a late write to clobber: without the guard, a superseded stop
/// publishing `Off` over the `Resolving` a newer toggle-on just put up snaps the
/// switch back under the user's hand — #594's exact symptom, on the one path
/// #595 deliberately left unguarded when `Resolving` did not yet exist.
fn apply(state: &Mutable<NightlightState>, ticket: &Ticket, on: bool) {
    let verb = if on { "start" } else { "stop" };
    let status = std::process::Command::new("systemctl")
        .args(["--user", verb, UNIT])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let outcome = match status {
        Ok(s) if s.success() => NightlightState::from_unit_active(on),
        Ok(s) => {
            tracing::warn!(
                ?s,
                verb,
                "nightlight: systemctl --user {verb} {UNIT} exited non-zero"
            );
            // The toggle didn't take — re-sync the signal to the daemon's truth
            // so the switch reflects reality instead of the request.
            read_active_state()
        }
        Err(e) => {
            tracing::warn!(error = %e, verb, "nightlight: failed to spawn systemctl");
            read_active_state()
        }
    };
    if !publish_if_current(state, ticket, outcome) {
        tracing::debug!(
            verb,
            "nightlight: unit {verb} superseded before its result landed; publishing nothing"
        );
    }
}

/// Read the unit's `ActiveState` via `systemctl --user show`. Yields
/// [`NightlightState::On`] only when the value is exactly `active`; any other
/// state (`inactive`, `failed`, `activating`, …), a non-zero exit, or a missing
/// `systemctl` all map to [`NightlightState::Off`]. A point-in-time read — see
/// the module docs.
fn read_active_state() -> NightlightState {
    let out = std::process::Command::new("systemctl")
        .args(["--user", "show", "-p", "ActiveState", "--value", UNIT])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(out) if out.status.success() => {
            let active = String::from_utf8_lossy(&out.stdout);
            NightlightState::from_unit_active(active.trim() == "active")
        }
        Ok(_) => NightlightState::Off,
        Err(e) => {
            tracing::debug!(error = %e, "nightlight: could not read ActiveState (assuming off)");
            NightlightState::Off
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Coords, Generation, NightlightState, args_file_body, parse_coord, publish_if_current,
        retraction_for,
    };
    use futures_signals::signal::Mutable;

    // ── Pending state (#597) ─────────────────────────────────────────────────

    #[test]
    fn resolving_reads_as_on_so_the_switch_never_moves_under_the_user() {
        // The whole tri-state hinges on this mapping. `Resolving` reporting
        // `false` would snap the switch back the instant the user flipped it —
        // #594's symptom, reintroduced from the other direction.
        assert!(NightlightState::Resolving.is_on());
        assert!(NightlightState::On.is_on());
        assert!(!NightlightState::Off.is_on());
    }

    #[test]
    fn only_resolving_asks_the_row_for_a_pending_affordance() {
        assert!(NightlightState::Resolving.is_resolving());
        assert!(!NightlightState::On.is_resolving());
        assert!(!NightlightState::Off.is_resolving());
    }

    #[test]
    fn the_unit_can_never_report_the_pending_state() {
        // `Resolving` is shell-side intent; `ActiveState` only ever means on or
        // off, so the seed and every failure re-sync land on a terminal state.
        assert_eq!(NightlightState::from_unit_active(true), NightlightState::On);
        assert_eq!(
            NightlightState::from_unit_active(false),
            NightlightState::Off
        );
    }

    #[test]
    fn a_toggle_off_retracts_only_a_pending_notice() {
        assert_eq!(
            retraction_for(NightlightState::Resolving),
            Some(NightlightState::Off),
            "a pending wait the user just cancelled must come off the row at once"
        );
        assert_eq!(
            retraction_for(NightlightState::On),
            None,
            "the daemon's reading is its own to publish; `apply` re-publishes it"
        );
        assert_eq!(retraction_for(NightlightState::Off), None);
    }

    #[test]
    fn a_toggle_off_during_the_wait_still_supersedes_the_parked_start() {
        // The #594 sequence, replayed over the #597 state shape through the
        // real helpers: the pending affordance must not cost the user their
        // ability to bail out, which is also why the row keeps the switch
        // sensitive while `Resolving`.
        let state = Mutable::new(NightlightState::Off);
        let generation = Generation::default();

        // Toggle on with no configured coordinates: claims, parks on a fix, and
        // announces the wait.
        let parked = generation.claim();
        assert!(publish_if_current(
            &state,
            &parked,
            NightlightState::Resolving
        ));
        assert_eq!(state.get(), NightlightState::Resolving);
        assert!(
            state.get().is_on(),
            "the switch stays where the user put it while the fix resolves"
        );

        // User gives up and flips it back off. The claim supersedes the parked
        // start; the retraction takes the "waiting" row down immediately.
        let off = generation.claim();
        assert_eq!(
            retraction_for(state.get()),
            Some(NightlightState::Off),
            "the pending notice is retracted, not left to the systemctl round-trip"
        );
        state.set(NightlightState::Off);

        // The fix finally lands. The parked start must publish nothing at all —
        // not `On`, and not a `Resolving` it is no longer entitled to.
        assert!(!publish_if_current(&state, &parked, NightlightState::On));
        assert!(!publish_if_current(
            &state,
            &parked,
            NightlightState::Resolving
        ));
        assert_eq!(
            state.get(),
            NightlightState::Off,
            "the off must stand; this is exactly the #594 regression"
        );
        assert!(off.is_current());
    }

    #[test]
    fn a_superseded_stop_cannot_publish_over_a_newer_pending_notice() {
        // The window the tri-state opened up, and the reason `apply` publishes
        // through `publish_if_current` rather than writing straight to the
        // signal. A toggle-off spawns its `systemctl stop` and *then* blocks for
        // the round-trip; a toggle-on landing inside that window puts up a
        // `Resolving` the stop knows nothing about. Before the guard, the stop's
        // `Off` landed last and snapped the switch back mid-wait — #594's
        // symptom on the one path #595 could safely leave unguarded when `Off`
        // over `Off` was the worst that could happen.
        let state = Mutable::new(NightlightState::On);
        let generation = Generation::default();

        // Toggle off: claims, retracts nothing (state is `On`), spawns the stop.
        let stopping = generation.claim();
        assert_eq!(retraction_for(state.get()), None);

        // Toggle on lands while that stop is still inside systemctl, and parks.
        let restarting = generation.claim();
        assert!(publish_if_current(
            &state,
            &restarting,
            NightlightState::Resolving
        ));

        // The stop now returns. Its result is accurate — the unit really is
        // stopped — but it no longer owns the signal, so it must stay quiet.
        assert!(
            !publish_if_current(&state, &stopping, NightlightState::Off),
            "a superseded stop publishes nothing, however true its reading"
        );
        assert_eq!(
            state.get(),
            NightlightState::Resolving,
            "the newer toggle-on keeps its pending notice; the switch does not move"
        );
        assert!(state.get().is_on());
        assert!(restarting.is_current());
    }

    #[test]
    fn a_second_toggle_on_takes_over_the_pending_notice() {
        // Off is not the only thing that can displace a parked start: a rapid
        // on/off/on leaves the *newest* start owning the signal.
        let state = Mutable::new(NightlightState::Off);
        let generation = Generation::default();

        let first = generation.claim();
        assert!(publish_if_current(
            &state,
            &first,
            NightlightState::Resolving
        ));
        let second = generation.claim();

        assert!(!publish_if_current(&state, &first, NightlightState::On));
        assert!(publish_if_current(&state, &second, NightlightState::On));
        assert_eq!(state.get(), NightlightState::On);
    }

    // ── Toggle supersession (#594) ───────────────────────────────────────────
    //
    // These cover the guard's *decision* only. The two things that make the bug
    // reachable — the up-to-FIX_WAIT real-time park on a location fix, and the
    // `systemctl --user start|stop` it races — are untestable in CI (one is a
    // 10 s wall-clock wait, the other needs a live user manager), so they are
    // verified by hand; see the PR's live-verify notes.

    #[test]
    fn a_ticket_with_no_intervening_request_is_current() {
        let counter = Generation::default();
        let ticket = counter.claim();
        assert!(ticket.is_current());
        // Non-consuming: the real path re-checks more than once per request.
        assert!(ticket.is_current());
    }

    #[test]
    fn one_intervening_request_supersedes_the_pending_ticket() {
        let counter = Generation::default();
        let parked = counter.claim(); // toggle-on, awaiting a location fix
        let off = counter.claim(); // user gives up and flips it back off
        assert!(
            !parked.is_current(),
            "the parked toggle-on must drop its result instead of applying it"
        );
        assert!(off.is_current(), "the newest request stays authoritative");
    }

    #[test]
    fn repeated_toggles_leave_only_the_last_ticket_current() {
        let counter = Generation::default();
        let tickets: Vec<_> = (0..5).map(|_| counter.claim()).collect();
        let (last, earlier) = tickets.split_last().expect("five tickets claimed");
        assert!(last.is_current());
        assert!(
            earlier.iter().all(|t| !t.is_current()),
            "every superseded toggle must stay dropped, not just the first"
        );
        // And the winner loses the moment a sixth request lands.
        let newest = counter.claim();
        assert!(!last.is_current());
        assert!(newest.is_current());
    }

    #[test]
    fn a_ticket_is_only_compared_against_its_own_counter() {
        let a = Generation::default();
        let b = Generation::default();
        let ta = a.claim();
        let tb = b.claim();
        assert!(ta.is_current());
        assert!(tb.is_current());
    }

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
