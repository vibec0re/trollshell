//! Keeping the `NetworkManager` secret agent **registered** for as long as it
//! is exported — the half of the agent's lifetime `nm_agent` does not cover.
//!
//! Exporting the agent object and registering it with NM's `AgentManager` are
//! two separate facts with two separate owners. The export lives on our side,
//! and `hytte_bus::export_object` already re-mounts it on every reconnect of
//! the pooled system connection. The registration lives in **NM's memory**,
//! keyed to the unique name the `RegisterWithCapabilities` call arrived from,
//! and it is lost three ways nothing on our side used to notice:
//!
//! 1. **NM restarts.** Its `AgentManager` comes back empty.
//! 2. **Our system connection reconnects.** The new connection has a new
//!    unique name, and NM drops the old name's agent the moment that name
//!    leaves the bus ("agent unregistered or disappeared" in NM's log).
//! 3. **The one attempt fails** — most often because the shell started before
//!    NM was answering.
//!
//! Until this module there was exactly one `RegisterWithCapabilities`, 500 ms
//! after the export, and a failure was only `warn!`ed. Any of the three left
//! the shell deaf: NM never called `GetSecrets` again, so joining a new
//! secured network or a VPN failed with "secrets required" and no prompt,
//! until the shell was restarted. `wifi::switch_needed` deliberately keeps the
//! backend alive across an NM restart, which made (1) the common case.
//!
//! The fix is one task per NM backend commit — [`run`] — that registers on
//! start-up, retries a failure on [`REGISTER_RETRY`], and registers again
//! whenever it is woken. The wakes come from the `wifi` service's existing
//! `NameOwnerChanged` watch (NM gaining a new owner, or the subscription
//! reporting a hole, which is how a reconnect shows up there); see
//! `wifi::DaemonChange`. It is a single task with a single call site, so two
//! registrations can never be in flight at once, and the backend teardown
//! stops it and waits for it before `Unregister` — see `wifi::tear_down`.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

use crate::retry;

/// Retry ramp for a registration NM refused or never answered.
///
/// **Unbounded**, for the reason `wifi::PROBE_RETRY` gives (#607/#613) and
/// `geoclue::RESOLVE_RETRY` repeats: a budget that runs out leaves the shell
/// deaf to NM for the rest of the session — the bug this module closes, on a
/// longer fuse. The *delay* is what is bounded, at 30 s.
///
/// Most failures never reach the ceiling, because the ramp is not the only
/// thing that ends a failure streak: a wake (NM gaining an owner, the bus
/// reconnecting) cuts any pending backoff short. What the ramp is for is the
/// failure no event announces — NM owning its name a moment before its
/// `AgentManager` answers, or the identifier still being held by a
/// registration NM has not dropped yet (see
/// `crate::wifi_nm::ensure_nm_agent_registered`). The floor matches
/// `wifi::PROBE_RETRY`'s, since the first of those is a start-up race of well
/// under a second.
///
/// A permanent refusal — policy denying this user an agent, or another
/// process holding our identifier — therefore costs one cheap call every 30 s
/// forever. It stays *quiet* rather than becoming a log flood via
/// [`retry::FailureLatch`]: one `warn!`, then `debug!` until it heals.
pub(crate) const REGISTER_RETRY: retry::Policy = retry::Policy {
    max_attempts: None,
    initial: Duration::from_millis(500),
    max_backoff: Duration::from_secs(30),
};

/// How long to wait before each registration attempt.
///
/// NM may call `GetSecrets` as soon as an agent registers — a request that was
/// waiting for an agent is handed to the new one — so the agent object has to
/// be mounted on the connection *before* the registration arrives from it.
/// After a reconnect the re-mount (`hytte_bus::export_object`'s epoch watch)
/// and the wake that leads here are both reactions to the same epoch bump,
/// with nothing ordering them. Waiting first is the start-up path's original
/// 500 ms, kept and applied to every attempt rather than only the first.
const REGISTER_SETTLE: Duration = Duration::from_millis(500);

/// The production registrar: [`run_registrar`] over the real `AgentManager`
/// on the pooled system connection.
///
/// Never returns. Spawned per NM backend commit, cancelled by the backend's
/// teardown; `wake` is how the `wifi` service asks it to register again.
pub(super) async fn run(wake: Arc<Notify>) {
    run_registrar(
        wake,
        crate::wifi_nm::ensure_nm_agent_registered,
        REGISTER_RETRY,
        REGISTER_SETTLE,
    )
    .await;
}

/// [`run`]'s body, with the registration call, the ramp and the settle
/// injected — the `geoclue::run_resolve_loop` seam, so the hermetic tests can
/// count registrations under paused time and the `nm_agent` system tests can
/// point it at a fake `AgentManager` on a throwaway bus.
///
/// Registers once without being asked (the start-up registration), then parks
/// until either the retry delay of a failed attempt elapses or `wake` fires,
/// and registers again. Never returns.
///
/// **One registration in flight at a time, by construction**: this is one
/// task, and the `await` on `register` is its only call site. A wake that lands
/// while a registration is in flight is not lost either — `Notify::notify_one`
/// stores a permit when nobody is waiting, so the loop registers once more
/// after the current attempt, which is right when the wake was "NM just
/// restarted" and that attempt may have reached the old NM. Any number of
/// wakes during one attempt cost exactly one more.
pub(super) async fn run_registrar<R, RFut, E>(
    wake: Arc<Notify>,
    register: R,
    policy: retry::Policy,
    settle: Duration,
) where
    R: Fn() -> RFut,
    RFut: Future<Output = Result<(), E>>,
    E: std::fmt::Display,
{
    let mut latch = retry::FailureLatch::new();
    // 1-based and counts the attempt that produced the outcome being weighed,
    // per `retry::Policy::step`. Reset by a success, so a later failure streak
    // is priced from the bottom of the ramp.
    let mut attempt: u32 = 1;

    loop {
        tokio::time::sleep(settle).await;
        // Scoped so the outcome — and the error inside it — is gone before the
        // park below awaits.
        let retry_in = {
            let outcome = register().await;
            let verdict = policy.step(&outcome, attempt);
            let report = latch.record(outcome.is_ok());
            log_attempt(&outcome, report, attempt, verdict);
            match verdict {
                retry::Step::Retry { after } => {
                    attempt = attempt.saturating_add(1);
                    Some(after)
                }
                // Registered: park until the next wake. `GiveUp` is
                // unreachable under the unbounded `REGISTER_RETRY`; a bounded
                // policy would park the same way, with a fresh budget.
                retry::Step::Proceed | retry::Step::GiveUp => {
                    attempt = 1;
                    None
                }
            }
        };

        match retry_in {
            Some(after) => tokio::select! {
                () = wake.notified() => {}
                () = tokio::time::sleep(after) => {}
            },
            None => wake.notified().await,
        }
    }
}

/// What one registration attempt is worth saying, given the ones before it.
///
/// A success is always an `info!` — it is rare (start-up, NM restarts,
/// reconnects) and it is the line a reader looks for after restarting NM. A
/// failure streak is latched the way `geoclue::log_no_location` latches its
/// own: one `warn!` naming what it costs and that it will not repeat, `debug!`
/// for as long as nothing changes, and an `info!` retraction when it heals.
fn log_attempt<E: std::fmt::Display>(
    outcome: &Result<(), E>,
    report: retry::Report,
    attempt: u32,
    verdict: retry::Step,
) {
    let error = outcome
        .as_ref()
        .err()
        .map_or_else(String::new, ToString::to_string);
    let retry_in_secs = match verdict {
        retry::Step::Retry { after } => after.as_secs_f64(),
        retry::Step::Proceed | retry::Step::GiveUp => 0.0,
    };
    match report {
        retry::Report::Quiet => tracing::info!("wifi_nm: secret agent registered with NM"),
        retry::Report::Recovered => tracing::info!(
            failed_attempts = attempt.saturating_sub(1),
            "wifi_nm: secret agent registered with NM after earlier attempts failed"
        ),
        retry::Report::Opened => tracing::warn!(
            attempt,
            retry_in_secs,
            %error,
            "wifi_nm: secret agent registration failed — NM will not ask this shell for Wi-Fi or \
             VPN secrets until it succeeds. Retrying with backoff, and again whenever NM restarts \
             or the system bus reconnects; this line does not repeat while the failure does"
        ),
        retry::Report::Repeating => tracing::debug!(
            attempt,
            retry_in_secs,
            %error,
            "wifi_nm: secret agent registration still failing"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{Arc, Duration, Notify, retry, run_registrar};
    use std::sync::atomic::{AtomicU32, Ordering};

    // Everything here drives `run_registrar` directly under paused time: no
    // NM, no bus, and the sleeps cost no wall clock. The `nm_agent` system
    // tests cover the same loop over a real `dbus-daemon` against a fake
    // `AgentManager`.

    /// The test ramp: the same *shape* as `REGISTER_RETRY` (unbounded,
    /// doubling, capped). Deliberately not the shipped constant — tuning that
    /// must not redden these, and `retry.rs`'s `every_shipped_policy_*` tests
    /// own the real numbers.
    const FAST_RETRY: retry::Policy = retry::Policy {
        max_attempts: None,
        initial: Duration::from_millis(100),
        max_backoff: Duration::from_millis(400),
    };

    /// A ramp whose first retry is an hour away, so any attempt that happens
    /// sooner was caused by a wake and not by the backoff.
    const GLACIAL_RETRY: retry::Policy = retry::Policy {
        max_attempts: None,
        initial: Duration::from_hours(1),
        max_backoff: Duration::from_hours(1),
    };

    const SETTLE: Duration = Duration::from_millis(10);

    /// A registration that fails for its first `failures` calls and succeeds
    /// after, counting every call.
    fn failing_then_ok(
        calls: &Arc<AtomicU32>,
        failures: u32,
    ) -> impl Fn() -> std::future::Ready<Result<(), &'static str>> + use<> {
        let calls = calls.clone();
        move || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(if n < failures {
                Err("NetworkManager is not answering")
            } else {
                Ok(())
            })
        }
    }

    /// Poll `calls` until it reaches `n`, under paused time (each sleep lets
    /// the registrar's own timers fire). Panics if it never does.
    async fn until_calls(calls: &AtomicU32, n: u32) {
        for _ in 0..10_000 {
            if calls.load(Ordering::SeqCst) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!(
            "the registrar made {} registrations, never reached {n}",
            calls.load(Ordering::SeqCst)
        );
    }

    /// The start-up registration needs no wake — and after it succeeds the
    /// loop parks, rather than polling NM.
    ///
    /// Falsify by making the loop wait on the wake *before* its first attempt:
    /// this reads 0.
    #[tokio::test(start_paused = true)]
    async fn registers_once_on_start_and_then_parks() {
        let calls = Arc::new(AtomicU32::new(0));
        let _ = tokio::time::timeout(
            Duration::from_hours(6),
            run_registrar(
                Arc::new(Notify::new()),
                failing_then_ok(&calls, 0),
                FAST_RETRY,
                SETTLE,
            ),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// The third way the agent went deaf: the one start-up attempt failed (NM
    /// not up yet) and nothing ever tried again. Now a failure is retried on
    /// the ramp until it succeeds, and the loop parks once it has.
    ///
    /// Falsify by parking on the wake whatever `step` said (the pre-fix
    /// single attempt): this reads 1.
    #[tokio::test(start_paused = true)]
    async fn a_failed_registration_is_retried_until_it_succeeds() {
        let calls = Arc::new(AtomicU32::new(0));
        let _ = tokio::time::timeout(
            Duration::from_hours(6),
            run_registrar(
                Arc::new(Notify::new()),
                failing_then_ok(&calls, 3),
                FAST_RETRY,
                SETTLE,
            ),
        )
        .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "three failures, one success, and then nothing more for six hours"
        );
    }

    /// A wake after a successful registration — NM restarted, the bus
    /// reconnected — registers again. This is the first two ways the agent
    /// went deaf.
    #[tokio::test(start_paused = true)]
    async fn a_wake_registers_again() {
        let calls = Arc::new(AtomicU32::new(0));
        let wake = Arc::new(Notify::new());
        let task = tokio::spawn(run_registrar(
            wake.clone(),
            failing_then_ok(&calls, 0),
            FAST_RETRY,
            SETTLE,
        ));

        until_calls(&calls, 1).await;
        // Long after the start-up registration, with nothing pending.
        tokio::time::sleep(Duration::from_hours(1)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        wake.notify_one();
        until_calls(&calls, 2).await;
        tokio::time::sleep(Duration::from_hours(1)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one wake, one renewal — then parked again"
        );
        task.abort();
    }

    /// A wake cuts a pending backoff short: NM appearing is exactly the moment
    /// a registration that failed because NM was absent can succeed, and
    /// waiting out the ramp would leave the agent deaf for no reason.
    ///
    /// Falsify by dropping the `wake.notified()` arm of the retry `select!`:
    /// the second attempt waits the full hour and this times out.
    #[tokio::test(start_paused = true)]
    async fn a_wake_cuts_a_backoff_short() {
        let calls = Arc::new(AtomicU32::new(0));
        let wake = Arc::new(Notify::new());
        let task = tokio::spawn(run_registrar(
            wake.clone(),
            failing_then_ok(&calls, 1),
            GLACIAL_RETRY,
            SETTLE,
        ));

        until_calls(&calls, 1).await;
        let woken_at = tokio::time::Instant::now();
        wake.notify_one();
        until_calls(&calls, 2).await;
        assert!(
            woken_at.elapsed() < Duration::from_secs(1),
            "the retry after a wake waited {:?} — the backoff, not the wake, drove it",
            woken_at.elapsed()
        );
        task.abort();
    }

    /// However many wakes land while a registration is in flight, they cost
    /// exactly one more registration, and never a concurrent one — the "do
    /// not register twice at once" half of the fix.
    ///
    /// Falsify by spawning each registration instead of awaiting it: the
    /// in-flight high-water mark reads above 1.
    #[tokio::test(start_paused = true)]
    async fn a_burst_of_wakes_during_a_registration_costs_one_more_and_never_overlaps() {
        let calls = Arc::new(AtomicU32::new(0));
        let in_flight = Arc::new(AtomicU32::new(0));
        let high_water = Arc::new(AtomicU32::new(0));
        let wake = Arc::new(Notify::new());

        let register = {
            let (calls, in_flight, high_water) =
                (calls.clone(), in_flight.clone(), high_water.clone());
            move || {
                let (calls, in_flight, high_water) =
                    (calls.clone(), in_flight.clone(), high_water.clone());
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    high_water.fetch_max(now, Ordering::SeqCst);
                    // A slow NM: the round trip is long enough for the burst
                    // below to land inside it.
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok::<(), &'static str>(())
                }
            }
        };
        let task = tokio::spawn(run_registrar(wake.clone(), register, FAST_RETRY, SETTLE));

        until_calls(&calls, 1).await;
        for _ in 0..5 {
            wake.notify_one();
        }
        tokio::time::sleep(Duration::from_hours(1)).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "five wakes during one registration must coalesce into one renewal"
        );
        assert_eq!(high_water.load(Ordering::SeqCst), 1);
        task.abort();
    }
}
