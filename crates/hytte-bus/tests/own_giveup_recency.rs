#![cfg(feature = "system-tests")]
//! #776: a give-up's cooldown must not be short-circuited by a stale release.
//!
//! `own_release.rs` covers the *wanted* half of #669 — a squatter that exits
//! must be noticed through `NameOwnerChanged` rather than on the next cooldown
//! wake. This file covers the half that was never asserted: a
//! `NameOwnerChanged` that says "released" but describes a moment that has
//! since passed must **not** be treated as a wake.
//!
//! #776 was argued from reading the code path, with an explicit note that
//! nothing demonstrated it. This is that demonstration. Against `main` before
//! the fix it fails; the assertion is a count of real bus traffic, taken from a
//! monitor connection, so it cannot pass or fail for a reason other than the
//! one under test — in particular it does not depend on catching `Mutable`
//! state transitions that a fast-enough path can coalesce away.
//!
//! **What it does and does not isolate.** #776 shipped two changes: draining
//! the contention retry sleep, and folding the queued backlog in
//! `wait_for_release_or_cooldown` before acting on a release. Measured, this
//! test sees one extra `RequestName` with *neither* and the expected count
//! with *either*, so it demonstrates the defect end-to-end but does not
//! attribute the fix to one change or the other — each independently closes
//! this entrance, which is the point of applying both. The fold is isolated
//! instead by the hermetic `wait_for_release_or_cooldown` tests in `own.rs`'s
//! `mod tests`, which drive the predicate directly and fail without it.
//!
//! Deliberately a separate test binary, like `own_release.rs`: one ephemeral
//! broker per test, and the duplicated `wait_for_state` helper that each
//! integration binary needs its own copy of anyway.

mod common;

use common::ephemeral_bus;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{OwnState, own_name_with};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use zbus::connection::Builder;
use zbus::fdo::RequestNameFlags;

const NAME: &str = "mov.vibec0re.test.flapped";

/// Consecutive losses before the primitive gives up. Deliberately higher than
/// the two `own_release.rs` uses, and sized for slack rather than for speed:
/// the flap has to land *inside* a contention retry sleep, and the retry ramp
/// is `RETRY_AFTER_LOSS * consecutive`, so five gives 250 + 500 + 750 + 1000 ms
/// — about two and a half seconds — between the first observable `Lost` and
/// the give-up. Any of those four sleeps will do.
///
/// The slack is what keeps a loaded box from turning this red for the wrong
/// reason. A flap that arrives *after* the give-up is a genuine release, which
/// the fixed code is supposed to wake on, so it would look exactly like the
/// defect. The precondition assertion right after the flap catches that case
/// and says so, but the margin is what makes it not happen.
const PERMANENT_AFTER: u32 = 5;

/// Far longer than the test is willing to wait. This is the assertion's teeth:
/// on the fixed code the primitive sits here for the whole run, so the extra
/// `RequestName` the count is looking for can only come from the defect.
const COOLDOWN: Duration = Duration::from_mins(10);

/// How many `RequestName(NAME, …)` calls the broker should see over the whole
/// run: two from the squatter (its initial grab, then the re-take that makes
/// the buffered release stale) and exactly `PERMANENT_AFTER` from the
/// primitive, after which it is in the ten-minute cooldown and must ask no
/// more. The defect adds one more primitive attempt.
const EXPECTED_REQUEST_NAMES: usize = 2 + PERMANENT_AFTER as usize;

/// How long to let the bus settle before counting. A spurious wake fires on
/// the first poll of the wait — microseconds after `PermanentlyTaken` — so
/// this only has to outlast scheduling, not a timer; the alternative it is
/// being measured against is ten minutes away.
const SETTLE: Duration = Duration::from_secs(3);

/// Upper bound on how long each of the two state transitions this test waits
/// for is allowed to take. A liveness guard, not a latency assertion — the
/// same reasoning `common::DBUS_DAEMON_STARTUP_BUDGET` spells out at length.
/// The give-up is ~2.5 s of deliberate sleeping, so this is roughly a 12x
/// margin; tightening it would not strengthen any assertion here, only make
/// the suite flake under CI contention.
const STATE_BUDGET: Duration = Duration::from_secs(30);

/// A squatter holds the name without `AllowReplacement`, so every
/// `RequestName` the primitive makes comes back `Exists`. While the primitive
/// is between attempts — sleeping out a contention retry, which before #776
/// did not drain its subscription — the squatter releases the name and
/// immediately takes it back. That leaves a `NameOwnerChanged` saying
/// "released" buffered on the primitive's subscription, with the
/// re-acquisition right behind it.
///
/// By the time the primitive gives up and starts waiting for a release, that
/// buffered pair describes a name that is taken. The wait must therefore run
/// to its cooldown, not return `Released` on the first message it finds.
///
/// The observable is the count of `RequestName` calls the broker sees for this
/// name. On the fix that is `EXPECTED_REQUEST_NAMES`; the spurious wake shows
/// up as one more.
#[tokio::test(flavor = "multi_thread")]
async fn a_release_buffered_during_a_retry_does_not_short_circuit_the_cooldown() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();

    let (request_names, monitor_task) = count_request_names(&address).await;

    // ── The squatter takes the name and refuses replacement ───────────────
    let squatter = Builder::address(address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("squatter connection to ephemeral bus");
    let squatter_dbus = zbus::fdo::DBusProxy::new(&squatter)
        .await
        .expect("squatter DBusProxy");
    let reply = squatter_dbus
        .request_name(
            NAME.try_into().expect("well-known name"),
            RequestNameFlags::DoNotQueue.into(),
        )
        .await
        .expect("squatter RequestName");
    assert!(
        matches!(reply, zbus::fdo::RequestNameReply::PrimaryOwner),
        "squatter must own the name before the primitive starts, got {reply:?}"
    );

    // ── The primitive starts and begins losing ────────────────────────────
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();
    let state = own_name_with(&shared, NAME)
        .permanent_after(PERMANENT_AFTER)
        .cooldown_after_permanent(COOLDOWN)
        .start();

    // The first `Lost` is set before the first contention retry sleep, so
    // seeing it puts us inside the window the flap has to land in.
    let lost = wait_for_state(state.signal_cloned(), STATE_BUDGET, |s| {
        matches!(s, OwnState::Lost { .. })
    })
    .await;
    assert!(
        matches!(lost, OwnState::Lost { .. }),
        "the primitive must have been refused the name at least once, got {lost:?}"
    );

    // ── The flap: released, then immediately taken back ───────────────────
    squatter_dbus
        .release_name(NAME.try_into().expect("well-known name"))
        .await
        .expect("squatter ReleaseName");
    let retaken = squatter_dbus
        .request_name(
            NAME.try_into().expect("well-known name"),
            RequestNameFlags::DoNotQueue.into(),
        )
        .await
        .expect("squatter re-RequestName");
    assert!(
        matches!(retaken, zbus::fdo::RequestNameReply::PrimaryOwner),
        "the squatter must get the name straight back, or the buffered release \
         would not be stale; got {retaken:?}"
    );

    // The test's own precondition, asserted rather than assumed: the flap has
    // to have landed while the primitive was still retrying. Had it arrived
    // after the give-up, the release would be *genuine* — the fixed code is
    // supposed to wake on that — and the count below would read like the
    // defect. `PERMANENT_AFTER` is sized so this cannot realistically happen;
    // this turns "cannot realistically" into a diagnosable red if it ever does.
    let after_flap = state
        .signal_cloned()
        .to_stream()
        .next()
        .await
        .expect("the state signal always yields its current value");
    assert!(
        !matches!(after_flap, OwnState::PermanentlyTaken { .. }),
        "the flap must land inside a contention retry, not after the give-up — \
         this run measured nothing; got {after_flap:?}"
    );

    // ── The give-up, and then silence ─────────────────────────────────────
    let latched = wait_for_state(state.signal_cloned(), STATE_BUDGET, |s| {
        matches!(s, OwnState::PermanentlyTaken { .. })
    })
    .await;
    assert!(
        matches!(latched, OwnState::PermanentlyTaken { .. }),
        "{PERMANENT_AFTER} refusals by one holder must latch PermanentlyTaken, \
         got {latched:?}"
    );

    tokio::time::sleep(SETTLE).await;

    let seen = request_names.load(Ordering::SeqCst);
    assert_eq!(
        seen, EXPECTED_REQUEST_NAMES,
        "after giving up, the primitive must wait out its {COOLDOWN:?} cooldown \
         rather than acting on a release that was already superseded when it was \
         buffered; {seen} RequestName calls for this name reached the broker, \
         expected {EXPECTED_REQUEST_NAMES}"
    );

    // Keep the squatter alive until the assertions have run, so a dropped
    // connection is never what freed the name.
    drop(squatter_dbus);
    drop(squatter);
    monitor_task.abort();
}

/// Spawn a monitor connection on `address` that counts every `RequestName`
/// call for [`NAME`] the broker routes, and return the live counter plus the
/// task's handle.
///
/// A monitor sees the traffic itself, so unlike the `OwnState` signal it
/// cannot lose an event to `Mutable` coalescing: a spurious attempt is a real
/// message on the wire whether or not any state it produced was observable.
async fn count_request_names(address: &str) -> (Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let monitor = Builder::address(address)
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("monitor connection to ephemeral bus");
    // The match rule narrows what the broker *routes* here, but a monitor
    // connection still receives the traffic addressed to it in its own right
    // (`NameAcquired`, `NameLost`, its own `Hello` reply), so the counting side
    // re-checks rather than trusting the rule — see `is_request_name_for`.
    let mut stream = zbus::MessageStream::from(&monitor);
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::MethodCall)
        .interface("org.freedesktop.DBus")
        .expect("match rule interface")
        .member("RequestName")
        .expect("match rule member")
        .arg(0, NAME)
        .expect("match rule arg0")
        .build();
    zbus::fdo::MonitoringProxy::new(&monitor)
        .await
        .expect("MonitoringProxy")
        .become_monitor(&[rule], 0)
        .await
        .expect("BecomeMonitor — the ephemeral bus config allows it");

    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    let task = tokio::spawn(async move {
        // The monitor connection has to outlive the stream, or the broker
        // drops the peer and the count stops before the settle window.
        let _monitor = monitor;
        while let Some(msg) = stream.next().await {
            let Ok(msg) = msg else { continue };
            if is_request_name_for(&msg, NAME) {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    (seen, task)
}

/// Whether `msg` is a `RequestName` method call asking for `name`.
///
/// A `RequestName` body is `(name, flags)`; only the name is checked, so an
/// attempt counts however it was flagged.
fn is_request_name_for(msg: &zbus::Message, name: &str) -> bool {
    let header = msg.header();
    if msg.message_type() != zbus::message::Type::MethodCall
        || header.member().is_none_or(|m| m.as_str() != "RequestName")
    {
        return false;
    }
    msg.body()
        .deserialize::<(String, u32)>()
        .is_ok_and(|(requested, _flags)| requested == name)
}

async fn wait_for_state<S>(
    signal: S,
    deadline: Duration,
    pred: impl Fn(&OwnState) -> bool,
) -> OwnState
where
    S: futures_signals::signal::Signal<Item = OwnState> + Unpin,
{
    let mut stream = signal.to_stream();
    let mut last = OwnState::Acquiring;
    let end = tokio::time::Instant::now() + deadline;
    while tokio::time::Instant::now() < end {
        if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(50), stream.next()).await {
            last = s.clone();
            if pred(&s) {
                return s;
            }
        }
    }
    last
}
