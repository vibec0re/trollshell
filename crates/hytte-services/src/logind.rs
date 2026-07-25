//! `org.freedesktop.login1.Manager` action wrappers.
//!
//! Exposes `suspend`, `reboot`, `poweroff` as fire-and-forget free
//! functions that route through the system bus via `hytte-bus`, plus
//! [`inhibit_idle`], which leases a logind idle-inhibitor fd. Polkit
//! authorization (when required by pkla) flows through the active
//! session's auth agent — the standalone polkit-gnome agent run as a user
//! service alongside the session (see the flake's nixosModule / `etc/`).
//!
//! The `suspend`/`reboot`/`poweroff` actions publish no reactive state
//! (pure fire-and-forget). Errors are logged at `tracing::warn!` and
//! otherwise consumed; the caller's UI (drawer, menu) dismisses regardless,
//! mirroring the pre-extraction `spawn_detached("systemctl", …)` behavior.
//! [`inhibit_idle`] instead returns an [`FdLease`] the caller holds — the
//! open fd *is* the inhibition.
//!
//! # Bus details
//!
//! - Destination: `org.freedesktop.login1` (system bus)
//! - Object path: `/org/freedesktop/login1`
//! - Interface: `org.freedesktop.login1.Manager`
//! - Methods: `Suspend(b)`, `Reboot(b)`, `PowerOff(b)` — each takes
//!   `interactive: bool`; we pass `false`, which delegates polkit auth
//!   to the active session's agent (the canonical behavior of
//!   `systemctl suspend` etc.).

use std::sync::OnceLock;
use std::time::Duration;

use futures_signals::signal::{Mutable, Signal, SignalExt as _};
use futures_util::StreamExt as _;
use hytte_bus::{BusError, BusKind, FdLease, PropState};
use hytte_reactive::runtime;
use zbus::zvariant::OwnedObjectPath;

const LOGIN1_DEST: &str = "org.freedesktop.login1";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";
const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";

/// Suspend the system. Fire-and-forget; errors logged at warn level.
pub fn suspend() {
    spawn_manager_call("Suspend");
}

/// Reboot the system. Fire-and-forget; errors logged at warn level.
pub fn reboot() {
    spawn_manager_call("Reboot");
}

/// Power off the system. Fire-and-forget; errors logged at warn level.
pub fn poweroff() {
    spawn_manager_call("PowerOff");
}

/// Acquire a logind **idle inhibitor**. The returned [`FdLease`] wraps the
/// leased fd whose open-ness *is* the lock: while the lease is alive the
/// system won't run its idle actions (screen blank/dim, auto-lock on idle);
/// dropping the lease releases the inhibition. This is honest, inspectable
/// state — `systemd-inhibit --list` shows it. (The fd, and therefore the
/// inhibitor, is owned by this process; see below on surviving a shell
/// restart.)
///
/// Calls `Inhibit(what="idle", who="trollshell", why="Keep awake",
/// mode="block")` on `org.freedesktop.login1.Manager`, which lives on the
/// **system** bus.
///
/// This is the enforcement mechanism for the "Keep awake" caffeine toggle
/// (issue #270): the native idle manager ([`crate::idle_notify`], #204) reads
/// logind's `BlockInhibited` before dimming/locking, so holding this `idle`
/// lease makes it skip those actions. The caller pairs the lease with a
/// matching screensaver inhibitor purely for visibility in
/// [`crate::screensaver::inhibitors`]. Hold the lease in service-side state
/// (never a widget) for as long as the inhibition should last.
///
/// **The fd is owned by *this* process.** It closes when trollshell exits,
/// which releases the inhibitor — so the hold does *not* by itself survive a
/// shell restart (the `BlockInhibited` entry vanishes the moment the process
/// dies). A caller that wants "Keep awake" to outlast a restart must persist
/// its own desired-state and re-acquire on the next start; that is exactly what
/// [`crate::screensaver`] does (#534).
///
/// # Errors
/// Returns a [`BusError`] if the `Inhibit` call fails: a transient bus error
/// (subject to the default retry), a timeout, or a D-Bus error reply.
pub async fn inhibit_idle() -> Result<FdLease, BusError> {
    hytte_bus::call(hytte_bus::BusKind::System, LOGIN1_DEST)
        .at_path(LOGIN1_PATH)
        .iface(MANAGER_IFACE)
        .method("Inhibit")
        .args(("idle", "trollshell", "Keep awake", "block"))
        .call_fd()
        .await
}

fn spawn_manager_call(method: &'static str) {
    runtime::handle().spawn(async move {
        let result = hytte_bus::call(BusKind::System, LOGIN1_DEST)
            .at_path(LOGIN1_PATH)
            .iface(MANAGER_IFACE)
            .method(method)
            .args((false,))
            .send::<()>()
            .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, method, "logind: Manager.{method} failed");
        }
    });
}

// ── Session lock state (#484) ─────────────────────────────────────────────────

/// Lazily-started tracker of the session's logind `LockedHint`. First call
/// spawns the subscription; the [`Mutable`] it writes lives here rather than in
/// a registered [`Service`](hytte_reactive::Service) so the plugin host can pull
/// the signal without a `main.rs` registration line.
static SESSION_LOCKED: OnceLock<Mutable<bool>> = OnceLock::new();

/// A signal of the session's logind `LockedHint` — `true` while the session is
/// locked (#484). Sourced from `org.freedesktop.login1.Session.LockedHint` on the
/// **system** bus, tracked live via [`hytte_bus::property`] so an unlock (a
/// swaylock `SetLockedHint(false)`, `loginctl unlock-session`) flips it back.
///
/// Starts `false` (unlocked) — the safe default for a shell in use (sensitive
/// content shows, "first unlock" actions still fire) — and holds there until the
/// session path resolves, which is retried with backoff rather than given up on
/// after one transient miss (#542). Lazily started on first call and shared across callers
/// (a single subscription, one `Mutable`), so pulling this repeatedly is cheap.
///
/// The trollshell plugin host projects this onto the `SessionLocked` wire push so
/// subscribing plugins (caw's briefing, the infobroker privacy blank) see it.
pub fn session_locked() -> impl Signal<Item = bool> {
    SESSION_LOCKED
        .get_or_init(|| {
            let locked = Mutable::new(false);
            let writer = locked.clone();
            runtime::handle().spawn(track_session_locked(writer));
            locked
        })
        .signal()
}

/// Project one [`PropState<bool>`] onto the tracked lock value, or `None` to keep
/// the last one (`Loading` — the pre-`Get` / reconnect gap). Pure, so the
/// keep-last policy is unit-testable without a live bus.
fn locked_from_prop(state: &PropState<bool>) -> Option<bool> {
    match state {
        PropState::Loaded(locked) | PropState::Stale(locked) => Some(*locked),
        PropState::Loading => None,
    }
}

/// Resolve the caller's concrete logind session path and track its `LockedHint`.
/// The concrete path (not the `session/auto` alias) is required so
/// `PropertiesChanged` — which logind emits on the real object path — actually
/// reaches the subscription.
async fn track_session_locked(writer: Mutable<bool>) {
    // Resolve with backoff rather than giving up on the first miss: at session
    // bring-up logind and the shell come up concurrently, so `GetSession` /
    // `GetSessionByPID` can transiently fail before the session is registered. A
    // one-shot resolve would then disable lock tracking for the rest of the
    // session (fail-open = unlocked); retrying keeps it eventually correct (#542).
    let path = resolve_session_path_with_retry().await;
    // Held for the loop's lifetime: dropping the last `PropertySignal` clone tears
    // the tracking task down, so this binding keeps it alive.
    let prop = hytte_bus::property::<bool>(BusKind::System, LOGIN1_DEST)
        .at_path(path)
        .iface(SESSION_IFACE)
        .name("LockedHint")
        .start();
    let mut states = prop.signal().to_stream();
    while let Some(state) = states.next().await {
        if let Some(locked) = locked_from_prop(&state) {
            writer.set_neq(locked);
        }
    }
}

/// The caller's concrete logind session object path: prefer `$XDG_SESSION_ID`
/// via `GetSession`, else `GetSessionByPID(0)` (0 = the calling process). `None`
/// when logind can't be reached or the caller isn't in a session.
async fn resolve_session_path() -> Option<String> {
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && !id.is_empty()
        && let Ok(path) = manager_object_path("GetSession", (id,)).await
    {
        return Some(path.as_str().to_owned());
    }
    match manager_object_path("GetSessionByPID", (0u32,)).await {
        Ok(path) => Some(path.as_str().to_owned()),
        Err(e) => {
            // The retry wrapper owns the user-facing cadence, so keep the
            // per-attempt detail at debug — a retry loop must not spam warn (#542).
            tracing::debug!(error = %e, "logind: GetSessionByPID failed");
            None
        }
    }
}

/// Resolve the concrete session path, retrying with capped exponential backoff
/// until it succeeds (#542). A transient startup miss (logind slow to answer, the
/// session not yet registered) must not permanently disable lock tracking, so
/// this keeps trying — 1 s → 2 s → … → 30 s cap — rather than falling back to
/// "unlocked forever". Mirrors the calendar/idle services' init-retry idiom.
async fn resolve_session_path_with_retry() -> String {
    const BACKOFF_START: Duration = Duration::from_secs(1);
    const BACKOFF_CAP: Duration = Duration::from_secs(30);
    let mut backoff = BACKOFF_START;
    let mut attempts = 0u32;
    loop {
        if let Some(path) = resolve_session_path().await {
            if attempts > 0 {
                tracing::info!(attempts, "logind: session path resolved after retries");
            }
            return path;
        }
        attempts = attempts.saturating_add(1);
        // First miss at warn for visibility; the rest at debug so a machine where
        // logind never answers doesn't fill the journal.
        if attempts == 1 {
            tracing::warn!(
                retry_in_s = backoff.as_secs(),
                "logind: could not resolve session path; retrying (lock tracking paused until then)"
            );
        } else {
            tracing::debug!(
                attempts,
                retry_in_s = backoff.as_secs(),
                "logind: session path still unresolved; retrying"
            );
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_CAP);
    }
}

/// Call a logind `Manager` method returning a single object path.
async fn manager_object_path<A>(method: &'static str, args: A) -> Result<OwnedObjectPath, BusError>
where
    A: serde::Serialize + zbus::zvariant::Type + Send + Sync + Clone + 'static,
{
    hytte_bus::call(BusKind::System, LOGIN1_DEST)
        .at_path(LOGIN1_PATH)
        .iface(MANAGER_IFACE)
        .method(method)
        .args(args)
        .send::<OwnedObjectPath>()
        .await
}

#[cfg(test)]
mod tests {
    use super::locked_from_prop;
    use hytte_bus::PropState;

    #[test]
    fn locked_maps_loaded_and_stale_and_keeps_last_on_loading() {
        assert_eq!(locked_from_prop(&PropState::Loaded(true)), Some(true));
        assert_eq!(locked_from_prop(&PropState::Loaded(false)), Some(false));
        // Stale (bus reconnecting) still carries the last known value…
        assert_eq!(locked_from_prop(&PropState::Stale(true)), Some(true));
        // …while Loading (pre-Get) keeps whatever we had (no clobber to a default).
        assert_eq!(locked_from_prop(&PropState::<bool>::Loading), None);
    }
}
