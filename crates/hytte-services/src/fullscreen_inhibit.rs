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
//! persisted to `~/.config/trollshell/fullscreen-inhibit.toml` (flat
//! `enabled = true|false`, mirroring `dnd`), so turning the policy off sticks
//! across restarts.

use crate::config_file;
use futures_signals::signal::{Mutable, Signal};
use hytte_bus::FdLease;
use hytte_reactive::{Service, registry, runtime};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

// ── Persistence ────────────────────────────────────────────────────────────

/// Config file under `~/.config/trollshell/`.
const CONFIG_FILE: &str = "fullscreen-inhibit.toml";

/// Load the policy flag. **Default `true`** (unlike `dnd`) — the whole point is
/// to keep the box awake during fullscreen out of the box; a missing or
/// malformed file leaves the policy on.
fn load_enabled_from_disk() -> bool {
    let Some(text) = config_file::read(CONFIG_FILE) else {
        return true;
    };
    parse_enabled(&text)
}

/// Parse the flat `enabled = true|false` config body. Permissive: an explicit
/// `enabled = false` turns the policy off; anything else — a missing key, a
/// malformed value, an empty file — leaves the **default-on** policy. Split out
/// as a pure fn so it's unit-testable without touching `$HOME`.
fn parse_enabled(text: &str) -> bool {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rhs) = trimmed.strip_prefix("enabled") {
            let rhs = rhs.trim_start_matches([' ', '=', '\t']).trim();
            if rhs.eq_ignore_ascii_case("false") {
                return false;
            }
            if rhs.eq_ignore_ascii_case("true") {
                return true;
            }
        }
    }
    true
}

fn save_enabled_to_disk(enabled: bool) {
    config_file::write(
        "fullscreen-inhibit",
        CONFIG_FILE,
        &format!("enabled = {enabled}\n"),
    );
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

/// Spawn the async logind fd acquire. On success, re-checks [`desired`] under
/// the `hold` lock (so a fullscreen-ended / policy-off flip that happened while
/// the fd was in flight releases it instead of leaking), then registers the
/// visibility inhibitor and stores the hold.
fn acquire(shared: &'static Shared) {
    runtime::handle().spawn(async move {
        let lease = match crate::logind::inhibit_idle().await {
            Ok(lease) => lease,
            Err(e) => {
                tracing::warn!(error = %e, "fullscreen-inhibit: logind Inhibit(idle) failed");
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
    fn parse_enabled_defaults_on() {
        // Empty / keyless / malformed bodies all keep the default-on policy —
        // the papercut this feature fixes is only worth having on by default.
        assert!(parse_enabled(""));
        assert!(parse_enabled("# just a comment\n"));
        assert!(parse_enabled("something = else\n"));
        assert!(parse_enabled("enabled = maybe\n"));
    }

    #[test]
    fn parse_enabled_explicit_off_and_on() {
        assert!(!parse_enabled("enabled = false\n"));
        assert!(parse_enabled("enabled = true\n"));
        // Tolerant of spacing / case, like the dnd parser it mirrors.
        assert!(!parse_enabled("enabled=FALSE"));
        assert!(parse_enabled("  enabled  =  True  "));
    }

    // ── Disk round-trip (#769) ──────────────────────────────────────────────
    //
    // These drive `save_enabled_to_disk`/`load_enabled_from_disk` — i.e. that
    // this module's calls land on / read back the right file — not
    // `config_file::write`'s atomicity mechanics (temp file + fsync +
    // rename), which are already exhaustively covered where that mechanism
    // actually lives (`config_file::tests::{a_reader_never_observes_a_partial_file,
    // concurrent_writers_do_not_corrupt_each_other, overwrites_an_existing_file_exactly}`,
    // all exercised against the same `write_path` core `config_file::write`
    // delegates to). A single-line payload written synchronously by one
    // writer with no crash can't demonstrate a tear either way — see the
    // note on `save_replaces_a_longer_pre_existing_file_exactly` below for
    // the falsification that confirms this.

    #[test]
    fn save_and_load_round_trip() {
        let root = std::env::temp_dir().join(format!(
            "hytte-fullscreen-inhibit-roundtrip-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // `temp_env` serializes $HOME mutation across tests and restores it
        // after (mirrors `places`'s config-watcher tests).
        temp_env::with_var("HOME", Some(root.as_os_str()), || {
            save_enabled_to_disk(false);
            assert!(!load_enabled_from_disk(), "false must round-trip as false");

            save_enabled_to_disk(true);
            assert!(load_enabled_from_disk(), "true must round-trip as true");
        });

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn save_replaces_a_longer_pre_existing_file_exactly() {
        // Seed a stale file bigger than any real payload, then confirm the
        // replacement is exact, not just "starts with the right bytes".
        //
        // NOTE this does NOT falsify the non-atomic defect #769 fixes: verified
        // by hand (reverted `save_enabled_to_disk` to the old bare
        // `std::fs::write(&path, body)`, reran this test, restored) that it
        // passes unchanged either way. `std::fs::write` opens with `O_TRUNC`,
        // so in a synchronous, single-writer, no-crash run the file is already
        // zero-length before the new bytes land — no tail survives regardless
        // of which implementation writes them. The actual defect (a reader or
        // a crash observing a torn/zero-length file mid-write) is only
        // observable via a concurrent reader or an injected crash, and is
        // already covered where the atomicity mechanism lives:
        // `config_file::tests::{a_reader_never_observes_a_partial_file,
        // concurrent_writers_do_not_corrupt_each_other}`. This test is kept as
        // a plain correctness regression guard, not an atomicity proof.
        let root = std::env::temp_dir().join(format!(
            "hytte-fullscreen-inhibit-replace-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".config/trollshell")).unwrap();
        let cfg = root.join(".config/trollshell/fullscreen-inhibit.toml");
        std::fs::write(&cfg, "x".repeat(4096)).unwrap();

        temp_env::with_var("HOME", Some(root.as_os_str()), || {
            save_enabled_to_disk(false);
            assert_eq!(
                std::fs::read_to_string(&cfg).unwrap(),
                "enabled = false\n",
                "no tail of the old, longer content may survive the replace"
            );
            assert!(!load_enabled_from_disk());
        });

        std::fs::remove_dir_all(&root).unwrap();
    }
}
