//! Fullscreen auto-inhibit (#404) — hold a logind **idle inhibitor** while any
//! visible window is fullscreen, so the native idle pipeline
//! ([`crate::idle_notify`]) doesn't dim/lock/suspend mid-movie.
//!
//! # Why this is nearly free
//!
//! Both halves already ship as tested code — this module only wires them:
//!
//! - **The signal:** [`crate::niri::fullscreen_window_on`] computes per-output
//!   "is the active workspace showing a fullscreen window" from niri's
//!   `WindowLayoutsChanged` (the one event that fires on a fullscreen toggle).
//!   The GTK side (in `trollshell`) subscribes it per monitor and pushes the
//!   result here via [`set_output_fullscreen`] — feeding the live monitor size
//!   the predicate needs.
//! - **The mechanism:** the logind idle-inhibitor fd lease
//!   ([`crate::logind::inhibit_idle`], #205/#211), which the native idle
//!   actions already honor — every dim/lock/suspend is gated on logind's
//!   `BlockInhibited` containing `idle` (#204 Phase 3a). Holding this lease
//!   makes them skip.
//!
//! So the whole feature is: aggregate the per-output fullscreen bits, and on
//! the `false → true` edge (gated on the "Keep awake when fullscreen" policy
//! toggle) take the same `idle` inhibitor caffeine takes; drop it on
//! `true → false`. The hold logic mirrors `screensaver`'s manual caffeine
//! (`ManualCaffeine`): a `desired`/`acquiring` handshake so a fast on→off while
//! the fd is in flight never leaks an inhibitor.
//!
//! A failed `Inhibit(idle)` is retried on `INHIBIT_RETRY`'s ramp for as long
//! as the hold is still wanted. Nothing else would ask again: `reconcile` runs
//! on an *edge* (a fullscreen toggle, the policy switch, a hot-plug), and a
//! movie that was already fullscreen when the call failed produces none, so a
//! single transient logind or system-bus failure used to leave the machine
//! un-inhibited for the rest of the movie.
//!
//! # Visibility
//!
//! While the lease is held, a matching `org.freedesktop.ScreenSaver` inhibitor
//! is registered (via [`crate::screensaver::inhibit`]) purely so the hold shows
//! up in the Power drawer's "what's keeping me awake" list and in
//! `systemd-inhibit --list` — exactly as the caffeine toggle does. Enforcement
//! is the logind fd, not this list.
//!
//! # Policy toggle & persistence
//!
//! [`enabled`] / [`set_enabled`] back the "Keep awake when fullscreen" switch
//! next to caffeine in the Power panel. **On by default.** The choice is
//! persisted to `$XDG_STATE_HOME/trollshell/fullscreen-inhibit.toml` (#1226,
//! flat `enabled = true|false`, mirroring `dnd`), so turning the policy off
//! sticks across restarts. One-time read-migration from the legacy
//! `~/.config/trollshell/fullscreen-inhibit.toml`: if state is absent and
//! that file exists, its value is adopted into state and the old file is
//! left untouched; once state exists it is authoritative and the old file is
//! never read again.

use crate::config_file;
use crate::retry;
use futures_signals::signal::{Mutable, Signal};
use hytte_bus::FdLease;
use hytte_config::state;
use hytte_reactive::{Service, registry, runtime};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

// ── Persistence ────────────────────────────────────────────────────────────

/// The state subsystem name —
/// `$XDG_STATE_HOME/trollshell/fullscreen-inhibit.toml`.
const SUBSYSTEM: &str = "fullscreen-inhibit";

/// Legacy config file under `~/.config/trollshell/`, migrated once (#1226).
const LEGACY_CONFIG_FILE: &str = "fullscreen-inhibit.toml";

/// `#[serde(default)]` sits on the **container**, not the field: on a field it
/// resolves to the *field type*'s `Default` (`bool::default()` — `false`),
/// which is the opposite of this struct's documented default and the opposite
/// of what the unparsable path gives, so a keyless-but-valid state file and a
/// corrupt one would disagree (#1233 F4). It also future-proofs the shape: the
/// day a second field is added, every existing state file lacks it, and the
/// container form is what makes those files inherit this `Default` impl rather
/// than the new field type's zero value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct FullscreenInhibitState {
    enabled: bool,
}

/// Default ON — the whole point is to keep the box awake during fullscreen
/// out of the box.
impl Default for FullscreenInhibitState {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Load the policy flag. **Default `true`** (unlike `dnd`) — the whole point is
/// to keep the box awake during fullscreen out of the box; a missing or
/// malformed file leaves the policy on.
fn load_enabled_from_disk() -> bool {
    let old = config_file::path(LEGACY_CONFIG_FILE);
    let loaded: FullscreenInhibitState =
        state::load_or_migrate_from(SUBSYSTEM, old.as_deref(), |text| Some(parse_legacy(text)));
    loaded.enabled
}

/// Parse the flat `enabled = true|false` legacy config body. Permissive: an
/// explicit `enabled = false` turns the policy off; anything else — a
/// missing key, a malformed value, an empty file — leaves the
/// **default-on** policy. Only used for the one-time migration — always
/// succeeds, so [`load_enabled_from_disk`] wraps it in `Some` for
/// [`state::load_or_migrate_from`]'s `parse_old`; split out as a pure fn so
/// it's unit-testable without touching `$HOME`.
fn parse_legacy(text: &str) -> FullscreenInhibitState {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rhs) = trimmed.strip_prefix("enabled") {
            let rhs = rhs.trim_start_matches([' ', '=', '\t']).trim();
            if rhs.eq_ignore_ascii_case("false") {
                return FullscreenInhibitState { enabled: false };
            }
            if rhs.eq_ignore_ascii_case("true") {
                return FullscreenInhibitState { enabled: true };
            }
        }
    }
    FullscreenInhibitState::default()
}

fn save_enabled_to_disk(enabled: bool) {
    state::store(SUBSYSTEM, &FullscreenInhibitState { enabled });
}

// ── Screensaver visibility inhibitor identity ──────────────────────────────

/// App name registered as an `org.freedesktop.ScreenSaver` inhibitor while a
/// fullscreen hold is active, so it surfaces in the Power drawer's "Also awake"
/// list. Distinct from caffeine's `("trollshell", "Keep awake")` sentinel so
/// the two never collide.
const VIS_APP: &str = "Fullscreen";
const VIS_REASON: &str = "A window is fullscreen";

// ── Cross-thread shared state ──────────────────────────────────────────────
//
// Like `screensaver`, mutators run from BOTH the GTK main thread
// (`set_enabled` from the toggle; `set_output_fullscreen`/`retain_outputs`
// from the per-monitor subscriptions) and hytte-tokio worker threads (the
// async acquire task's `desired` re-check). `registry::with` is a GTK-thread
// thread-local, so a `static OnceLock` of `Send + Sync` handles is the
// cross-thread-safe home for the aggregation + the live hold.

struct Shared {
    /// Policy flag ("Keep awake when fullscreen"). Same `Mutable` clone the
    /// registry [`FullscreenInhibitHandles`] exposes via [`enabled`], so a
    /// `set` here re-emits on that signal.
    enabled: Mutable<bool>,
    /// Per-output "is the visible workspace fullscreen" bits, keyed by
    /// connector. The aggregate `any(true)` drives the hold.
    outputs: Arc<Mutex<HashMap<String, bool>>>,
    /// The live logind hold + its handshake state.
    hold: Arc<Mutex<HoldState>>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

/// Handshake state for the single logind hold. Lock ordering is always
/// **`hold` → `outputs`** (reconcile locks `hold`, then reads `outputs` via
/// [`desired`]); no path takes them in the other order, so they can't deadlock.
#[derive(Default)]
struct HoldState {
    /// An acquire task is in flight; suppresses spawning a second one.
    acquiring: bool,
    /// The live hold, present iff the inhibitor is currently engaged.
    hold: Option<Hold>,
}

/// A live fullscreen hold: the logind fd (drop = release) plus the screensaver
/// cookie registered for visibility.
struct Hold {
    cookie: u32,
    /// Dropping this fd closes it, releasing the logind idle inhibitor. Held,
    /// not read — its lifetime is the whole point.
    _lease: FdLease,
}

// ── Service ────────────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct FullscreenInhibitHandles {
    /// Registry-side handle for the [`enabled`] accessor. The aggregation
    /// (`outputs`) and the live hold live in [`SHARED`] (a `static`), so they
    /// don't need a home here.
    pub(crate) enabled: Mutable<bool>,
}

/// Marker type for the fullscreen auto-inhibit service.
pub struct FullscreenInhibitService;

impl Service for FullscreenInhibitService {
    type Handles = FullscreenInhibitHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let enabled = Mutable::new(load_enabled_from_disk());
        let _ = SHARED.set(Shared {
            enabled: enabled.clone(),
            outputs: Arc::new(Mutex::new(HashMap::new())),
            hold: Arc::new(Mutex::new(HoldState::default())),
        });
        FullscreenInhibitHandles { enabled }
    }
}

/// Returns the fullscreen auto-inhibit service to register with the runtime.
#[must_use]
pub fn service() -> FullscreenInhibitService {
    FullscreenInhibitService
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Signal of the "Keep awake when fullscreen" policy flag. Bind a switch's
/// `active` to this; authoritative, so every monitor's Power drawer agrees.
pub fn enabled() -> impl Signal<Item = bool> {
    registry::with(|r| {
        r.get::<FullscreenInhibitHandles>()
            .expect("fullscreen_inhibit::service() not registered")
            .enabled
            .signal()
    })
}

/// Turn the "Keep awake when fullscreen" policy on or off, persisting the
/// choice. Idempotent — a redundant call is a no-op for the fd and the file
/// (so the switch's programmatic `set_active` from the authoritative binding
/// can never thrash either). Reconciles the hold: turning the policy off while
/// a fullscreen window is up drops the inhibitor immediately.
pub fn set_enabled(on: bool) {
    let Some(shared) = SHARED.get() else {
        return;
    };
    if shared.enabled.get() != on {
        shared.enabled.set(on);
        // File I/O off the GTK main thread.
        runtime::handle().spawn_blocking(move || save_enabled_to_disk(on));
    }
    reconcile(shared);
}

/// Report whether `connector`'s currently-visible workspace shows a fullscreen
/// window. Called from the per-monitor niri subscription on the GTK thread.
/// Idempotent per connector; reconciles the aggregate hold.
pub fn set_output_fullscreen(connector: &str, fullscreen: bool) {
    let Some(shared) = SHARED.get() else {
        return;
    };
    {
        let mut map = shared.outputs.lock().expect("fullscreen outputs poisoned");
        map.insert(connector.to_string(), fullscreen);
    }
    reconcile(shared);
}

/// Drop the fullscreen bits for outputs no longer in `connectors` (a monitor
/// hot-unplug). Without this, a vanished output that was fullscreen would pin
/// the inhibitor forever. Reconciles afterwards.
pub fn retain_outputs(connectors: &[String]) {
    let Some(shared) = SHARED.get() else {
        return;
    };
    {
        let mut map = shared.outputs.lock().expect("fullscreen outputs poisoned");
        map.retain(|k, _| connectors.contains(k));
    }
    reconcile(shared);
}

// ── Hold reconciliation ────────────────────────────────────────────────────

/// Pure policy: hold the inhibitor iff the policy is enabled **and** some
/// output shows a fullscreen window. Split out so the edge logic is unit
/// testable without the `static` state.
fn should_hold(enabled: bool, any_fullscreen: bool) -> bool {
    enabled && any_fullscreen
}

/// Pure: does any output currently show a fullscreen window? Split from the
/// `Mutex` so it can be tested against a plain map.
fn any_true(outputs: &HashMap<String, bool>) -> bool {
    outputs.values().copied().any(|x| x)
}

/// Whether the inhibitor *should* be held right now, reading live `SHARED`
/// state. Locks `outputs` (briefly) — callers must not already hold that lock;
/// callers that hold `hold` may call this (order is `hold → outputs`).
fn desired(shared: &Shared) -> bool {
    let any = {
        let map = shared.outputs.lock().expect("fullscreen outputs poisoned");
        any_true(&map)
    };
    should_hold(shared.enabled.get(), any)
}

/// Bring the live hold in line with [`desired`]. Locks `hold` first, then reads
/// `desired` (which locks `outputs`) — the fixed `hold → outputs` order shared
/// with [`acquire`], so the two can't deadlock. Safe to call from any thread.
fn reconcile(shared: &'static Shared) {
    let mut h = shared.hold.lock().expect("fullscreen hold poisoned");
    if desired(shared) {
        if h.hold.is_some() || h.acquiring {
            return; // already engaged or coming online
        }
        h.acquiring = true;
        drop(h);
        acquire(shared);
    } else if let Some(hold) = h.hold.take() {
        // Remove the visibility inhibitor; dropping `hold` at end of scope
        // closes the logind fd, releasing that inhibitor.
        crate::screensaver::uninhibit(hold.cookie);
    }
}

/// The ramp a failed logind `Inhibit(idle)` is retried on while the hold is
/// still wanted.
///
/// Unbounded, because the condition that ends the retrying is not a budget but
/// the hold no longer being wanted — a movie that is still fullscreen still
/// wants it on the hundredth attempt. The numbers are `geoclue`'s
/// `RESOLVE_RETRY`'s, for the same reasons: a 1 s first retry catches a
/// system bus that blipped, and a 30 s ceiling keeps a logind that is really
/// down from becoming a journal flood. Each attempt is one `Inhibit` call on
/// the system bus.
pub(crate) const INHIBIT_RETRY: retry::Policy = retry::Policy {
    max_attempts: None,
    initial: Duration::from_secs(1),
    max_backoff: Duration::from_secs(30),
};

/// How [`acquire_while_wanted`] ended.
#[derive(Debug, PartialEq, Eq)]
enum Acquired<T> {
    /// The lease was taken.
    Held(T),
    /// The hold stopped being wanted between attempts. `still_wanted` has
    /// already released the in-flight claim.
    NotWanted,
    /// A bounded policy ran out. Unreachable with [`INHIBIT_RETRY`]; kept so a
    /// bounded ramp stays one field away, as `retry::Step::GiveUp` is.
    GaveUp,
}

/// Try `attempt` until it succeeds, sleeping on `policy`'s ramp between
/// failures, for as long as `still_wanted` says the result is wanted.
///
/// `still_wanted` is asked after every failure and again after every sleep, so
/// a hold that ends mid-backoff costs no further call. The production closure
/// (see [`acquire`]) clears `HoldState::acquiring` itself when it answers
/// `false`, under the same lock it read [`desired`] under: a `reconcile` that
/// lands after that answer then sees no acquire in flight and starts one,
/// rather than trusting a loop that has just decided to stop.
///
/// The logging is [`retry::FailureLatch`]'s: one `warn!` when a streak opens,
/// `debug!` while it repeats, and a `warn!` retraction if it recovers.
///
/// Everything is injected — the acquire, the check and the sleep — so the loop
/// is testable without logind or a clock.
async fn acquire_while_wanted<T, E, Attempt, AFut, Wanted, Sleep, SFut>(
    mut attempt: Attempt,
    mut still_wanted: Wanted,
    mut sleep: Sleep,
    policy: retry::Policy,
) -> Acquired<T>
where
    Attempt: FnMut() -> AFut,
    AFut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
    Wanted: FnMut() -> bool,
    Sleep: FnMut(Duration) -> SFut,
    SFut: Future<Output = ()>,
{
    let mut latch = retry::FailureLatch::new();
    let mut n: u32 = 1;
    loop {
        let outcome = attempt().await;
        let step = policy.step(&outcome, n);
        let error = match outcome {
            Ok(lease) => {
                if latch.record(true) == retry::Report::Recovered {
                    tracing::warn!(
                        attempts = n,
                        "fullscreen-inhibit: logind Inhibit(idle) succeeded after retrying"
                    );
                }
                return Acquired::Held(lease);
            }
            Err(e) => e,
        };
        let report = latch.record(false);
        if !still_wanted() {
            tracing::debug!(error = %error, "fullscreen-inhibit: Inhibit(idle) failed; hold no longer wanted");
            return Acquired::NotWanted;
        }
        let retry::Step::Retry { after } = step else {
            tracing::error!(error = %error, attempts = n, "fullscreen-inhibit: giving up on Inhibit(idle)");
            return Acquired::GaveUp;
        };
        if report == retry::Report::Opened {
            tracing::warn!(
                error = %error,
                retry_in = ?after,
                "fullscreen-inhibit: logind Inhibit(idle) failed; retrying while a window stays \
                 fullscreen (further failures log at debug)"
            );
        } else {
            tracing::debug!(error = %error, attempt = n, retry_in = ?after, "fullscreen-inhibit: Inhibit(idle) still failing");
        }
        sleep(after).await;
        if !still_wanted() {
            return Acquired::NotWanted;
        }
        n = n.saturating_add(1);
    }
}

/// After a failed attempt: is the hold still wanted? When it is not, clear
/// `acquiring` under the same lock [`desired`] was read under — see
/// [`acquire_while_wanted`] for why that has to be one step.
fn still_wanted(shared: &Shared) -> bool {
    let mut h = shared.hold.lock().expect("fullscreen hold poisoned");
    let wanted = desired(shared);
    if !wanted {
        h.acquiring = false;
    }
    wanted
}

/// Spawn the async logind fd acquire, retried on [`INHIBIT_RETRY`] while the
/// hold is still wanted. On success, re-checks [`desired`] under the `hold`
/// lock (so a fullscreen-ended / policy-off flip that happened while the fd
/// was in flight releases it instead of leaking), then registers the
/// visibility inhibitor and stores the hold.
fn acquire(shared: &'static Shared) {
    runtime::handle().spawn(async move {
        let lease = match acquire_while_wanted(
            crate::logind::inhibit_idle,
            || still_wanted(shared),
            tokio::time::sleep,
            INHIBIT_RETRY,
        )
        .await
        {
            Acquired::Held(lease) => lease,
            // `still_wanted` already cleared `acquiring`.
            Acquired::NotWanted => return,
            Acquired::GaveUp => {
                shared
                    .hold
                    .lock()
                    .expect("fullscreen hold poisoned")
                    .acquiring = false;
                return;
            }
        };
        let mut h = shared.hold.lock().expect("fullscreen hold poisoned");
        h.acquiring = false;
        // Re-check under the lock: state may have changed while awaiting. A
        // concurrent reconcile is serialized behind this same lock, so this is
        // atomic w.r.t. release.
        if !desired(shared) || h.hold.is_some() {
            drop(lease); // closes the fd → no dangling inhibitor
            return;
        }
        let cookie = crate::screensaver::inhibit(VIS_APP, VIS_REASON);
        h.hold = Some(Hold {
            cookie,
            _lease: lease,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use hytte_config::test_support::scratch_home;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    // ── Retrying a failed Inhibit(idle) ─────────────────────────────────────

    /// The test ramp: `INHIBIT_RETRY`'s shape, with tiny numbers. The real
    /// sleeps are injected away, so only the recorded durations matter.
    const TEST_RETRY: retry::Policy = retry::Policy {
        max_attempts: None,
        initial: Duration::from_millis(1),
        max_backoff: Duration::from_millis(4),
    };

    /// A scripted run: `outcomes` are the acquire results in order, `wanted`
    /// the `still_wanted` answers in order (an exhausted script answers
    /// `true`). Returns the result, how many acquires ran, and the sleeps.
    fn run(
        outcomes: Vec<Result<u32, &'static str>>,
        wanted: Vec<bool>,
        policy: retry::Policy,
    ) -> (Acquired<u32>, usize, Vec<Duration>) {
        let outcomes = RefCell::new(VecDeque::from(outcomes));
        let wanted = RefCell::new(VecDeque::from(wanted));
        let attempts = RefCell::new(0_usize);
        let sleeps = RefCell::new(Vec::new());
        let result = futures_executor::block_on(acquire_while_wanted(
            || {
                *attempts.borrow_mut() += 1;
                let next = outcomes
                    .borrow_mut()
                    .pop_front()
                    .expect("the script ran out of acquire outcomes");
                std::future::ready(next)
            },
            || wanted.borrow_mut().pop_front().unwrap_or(true),
            |d| {
                sleeps.borrow_mut().push(d);
                std::future::ready(())
            },
            policy,
        ));
        (result, attempts.into_inner(), sleeps.into_inner())
    }

    /// A transient failure during an already-fullscreen movie is retried
    /// until it takes, on the ramp — the case that used to leave the machine
    /// un-inhibited for the whole movie.
    ///
    /// Falsifiable: the old `acquire` gave up after the first `Err`, which is
    /// one attempt and no `Held`.
    #[test]
    fn a_failed_inhibit_is_retried_on_the_ramp_while_wanted() {
        let (result, attempts, sleeps) = run(
            vec![Err("bus blip"), Err("bus blip"), Ok(7)],
            vec![],
            TEST_RETRY,
        );
        assert_eq!(result, Acquired::Held(7));
        assert_eq!(attempts, 3);
        assert_eq!(
            sleeps,
            vec![Duration::from_millis(1), Duration::from_millis(2)]
        );
    }

    #[test]
    fn a_hold_no_longer_wanted_after_a_failure_stops_without_sleeping() {
        let (result, attempts, sleeps) = run(vec![Err("denied")], vec![false], TEST_RETRY);
        assert_eq!(result, Acquired::NotWanted);
        assert_eq!(attempts, 1);
        assert!(sleeps.is_empty());
    }

    /// The fullscreen window closed during the backoff: no further `Inhibit`
    /// call is made just to be dropped.
    #[test]
    fn a_hold_that_ends_mid_backoff_makes_no_further_call() {
        let (result, attempts, sleeps) = run(vec![Err("bus blip")], vec![true, false], TEST_RETRY);
        assert_eq!(result, Acquired::NotWanted);
        assert_eq!(attempts, 1);
        assert_eq!(sleeps.len(), 1);
    }

    #[test]
    fn the_first_success_needs_no_retry() {
        let (result, attempts, sleeps) = run(vec![Ok(1)], vec![], TEST_RETRY);
        assert_eq!(result, Acquired::Held(1));
        assert_eq!(attempts, 1);
        assert!(sleeps.is_empty());
    }

    /// Unreachable with the shipped (unbounded) ramp, but a bounded policy
    /// must stop rather than spin.
    #[test]
    fn a_bounded_policy_gives_up() {
        let bounded = retry::Policy {
            max_attempts: Some(2),
            ..TEST_RETRY
        };
        let (result, attempts, _) = run(vec![Err("down"), Err("down")], vec![], bounded);
        assert_eq!(result, Acquired::GaveUp);
        assert_eq!(attempts, 2);
    }

    #[test]
    fn should_hold_requires_both() {
        assert!(should_hold(true, true));
        assert!(!should_hold(true, false)); // nothing fullscreen
        assert!(!should_hold(false, true)); // policy off
        assert!(!should_hold(false, false));
    }

    #[test]
    fn any_true_empty_is_false() {
        let map: HashMap<String, bool> = HashMap::new();
        assert!(!any_true(&map));
    }

    #[test]
    fn any_true_all_false_is_false() {
        let mut map = HashMap::new();
        map.insert("DP-1".to_string(), false);
        map.insert("HDMI-A-1".to_string(), false);
        assert!(!any_true(&map));
    }

    #[test]
    fn any_true_one_true_is_true() {
        // A fullscreen window on the second monitor still keeps the box awake.
        let mut map = HashMap::new();
        map.insert("DP-1".to_string(), false);
        map.insert("HDMI-A-1".to_string(), true);
        assert!(any_true(&map));
    }

    #[test]
    fn parse_legacy_defaults_on() {
        // Empty / keyless / malformed bodies all keep the default-on policy —
        // the papercut this feature fixes is only worth having on by default.
        assert!(parse_legacy("").enabled);
        assert!(parse_legacy("# just a comment\n").enabled);
        assert!(parse_legacy("something = else\n").enabled);
        assert!(parse_legacy("enabled = maybe\n").enabled);
    }

    #[test]
    fn parse_legacy_explicit_off_and_on() {
        assert!(!parse_legacy("enabled = false\n").enabled);
        assert!(parse_legacy("enabled = true\n").enabled);
        // Tolerant of spacing / case, like the dnd parser it mirrors.
        assert!(!parse_legacy("enabled=FALSE").enabled);
        assert!(parse_legacy("  enabled  =  True  ").enabled);
    }

    fn legacy_path(home: &std::path::Path) -> std::path::PathBuf {
        home.join(".config/trollshell/fullscreen-inhibit.toml")
    }

    // ── Disk round-trip (#769, moved onto state by #1226) ───────────────────
    //
    // These drive `save_enabled_to_disk`/`load_enabled_from_disk` — i.e. that
    // this module's calls land on / read back the right subsystem — not
    // `state::store`'s atomicity mechanics (temp file + fsync + rename),
    // which are already exhaustively covered where that mechanism actually
    // lives (`hytte_config::state::tests` and `hytte_config::file::tests`,
    // which `state::store_at` shares the atomic writer with).

    #[test]
    fn save_and_load_round_trip() {
        scratch_home(|_home| {
            save_enabled_to_disk(false);
            assert!(!load_enabled_from_disk(), "false must round-trip as false");

            save_enabled_to_disk(true);
            assert!(load_enabled_from_disk(), "true must round-trip as true");
        });
    }

    #[test]
    fn save_replaces_a_longer_pre_existing_state_file_exactly() {
        // Seed a stale state file bigger than any real payload, then confirm
        // the replacement is exact, not just "starts with the right bytes".
        scratch_home(|_home| {
            let state_path = state::path(SUBSYSTEM).unwrap();
            std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
            std::fs::write(&state_path, "x".repeat(4096)).unwrap();

            save_enabled_to_disk(false);
            assert_eq!(
                std::fs::read_to_string(&state_path).unwrap(),
                "enabled = false\n",
                "no tail of the old, longer content may survive the replace"
            );
            assert!(!load_enabled_from_disk());
        });
    }

    // ── State migration (#1226) ─────────────────────────────────────────────

    #[test]
    fn migrates_the_legacy_file_once_and_leaves_it_untouched() {
        scratch_home(|home| {
            let legacy = legacy_path(home);
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, "enabled = false\n").unwrap();
            let before = std::fs::metadata(&legacy).unwrap();

            assert!(
                !load_enabled_from_disk(),
                "the legacy off-value must be adopted into state"
            );

            let state_path = state::path(SUBSYSTEM).unwrap();
            assert!(
                state_path.exists(),
                "state must now hold the migrated value"
            );

            let after = std::fs::metadata(&legacy).unwrap();
            assert_eq!(
                std::fs::read_to_string(&legacy).unwrap(),
                "enabled = false\n",
                "the legacy file's bytes must survive the migration untouched"
            );
            assert_eq!(
                before.modified().unwrap(),
                after.modified().unwrap(),
                "the legacy file's mtime must survive the migration untouched"
            );
        });
    }

    #[test]
    fn state_wins_once_it_exists_and_the_legacy_file_is_never_read_again() {
        scratch_home(|home| {
            state::store(SUBSYSTEM, &FullscreenInhibitState { enabled: false });

            let legacy = legacy_path(home);
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, "not even valid toml {{{").unwrap();

            assert!(
                !load_enabled_from_disk(),
                "state's off-value must win over the legacy file (which would default ON)"
            );
        });
    }

    /// State wins **even when it does not parse** — the half of #1226's
    /// contract with a user-visible cost, and the one the `state_wins_*` test
    /// above cannot reach because it seeds a valid state file (#1233 F1).
    #[test]
    fn a_corrupt_state_file_still_wins_over_the_legacy_file() {
        scratch_home(|home| {
            let state_path = state::path(SUBSYSTEM).unwrap();
            std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
            std::fs::write(&state_path, "not valid toml {{{").unwrap();

            let legacy = legacy_path(home);
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, "enabled = false\n").unwrap();

            assert!(
                load_enabled_from_disk(),
                "a corrupt state file falls to the documented default (ON), \
                 never back to the legacy file's OFF"
            );
            assert_eq!(
                std::fs::read_to_string(&state_path).unwrap(),
                "not valid toml {{{",
                "the read path must not rewrite the corrupt state file"
            );
        });
    }

    /// A state file that exists and parses as a TOML table but carries no
    /// `enabled` key must read the **documented** default (ON), which is what
    /// `#[serde(default)]` on the container buys and what the same attribute
    /// on the field silently inverts to `bool::default()` (#1233 F4). Pinned
    /// alongside the unparsable case above so the two "this file is unusable"
    /// paths are asserted to agree — before this they did not, and the one
    /// that disagreed was the silent one.
    #[test]
    fn a_keyless_state_file_reads_the_documented_default() {
        scratch_home(|_home| {
            let state_path = state::path(SUBSYSTEM).unwrap();
            std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
            std::fs::write(&state_path, "# nothing\n").unwrap();

            assert!(
                load_enabled_from_disk(),
                "a state file with no `enabled` key must read the struct's documented \
                 default (ON), not `bool::default()`"
            );
        });
    }

    #[test]
    fn neither_file_present_defaults_to_on() {
        scratch_home(|_home| {
            assert!(load_enabled_from_disk(), "fullscreen-inhibit defaults ON");
        });
    }
}
