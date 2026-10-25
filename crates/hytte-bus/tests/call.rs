#![cfg(feature = "system-tests")]

mod common;

use common::ephemeral_bus;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{BusError, RetryPolicy, call_with};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::Instant;

#[tokio::test(flavor = "multi_thread")]
async fn calls_dbus_list_names() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let names: Vec<String> = call_with(&shared, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("ListNames")
        .args(())
        .send()
        .await
        .expect("ListNames");

    assert!(names.iter().any(|n| n == "org.freedesktop.DBus"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_method_is_permanent() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let result: Result<(), BusError> = call_with(&shared, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("DefinitelyDoesNotExist")
        .args(())
        .retry(RetryPolicy::Never)
        .send()
        .await;

    match result {
        Err(BusError::Permanent { dbus_name, .. }) => {
            assert!(dbus_name.is_some(), "expected dbus_name on Permanent");
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_once_recovers_from_transient_disconnect() {
    let (conn, guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    // Inject a replacement connection, then simulate a disconnect so the
    // supervisor immediately uses it on reconnect.
    let replacement = common::connect(guard.address.as_str()).await;
    shared.simulate_disconnect_for_test(replacement).await;

    let names: Result<Vec<String>, BusError> = call_with(&shared, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("ListNames")
        .args(())
        .retry(RetryPolicy::Once)
        .send()
        .await;

    assert!(
        names.is_ok(),
        "expected retry-Once to succeed; got {names:?}"
    );
}

// ── A reconnect that already happened is not waited for again ────────────────
//
// `RetryPolicy::Once` waits for the connection epoch to advance before it
// retries. Until this was fixed it read the epoch it waits *past* only once the
// first attempt had already failed, so a call whose connection was replaced
// while it was in flight — another operation's failure cleared the cached
// connection and the supervisor reconnected in milliseconds — started waiting
// for a second reconnect that nothing was ever going to perform, and slept the
// whole `timeout.min(5 s)` before a retry that then succeeded at once. A bus
// restart with several calls in flight is exactly that: whichever call notices
// first clears the connection, and every call whose failure surfaces after the
// supervisor has reinstalled one stalls.

/// The peer both buses serve. On the bus that is about to die it counts each
/// call as it arrives and never answers, so the calls are still in flight when
/// the broker goes; on the bus that replaces it, it answers at once, so a
/// retry's latency is the retry's wait and nothing else.
struct Stall {
    /// `Some` on the dying bus: incremented as each call arrives.
    arrived: Option<Arc<AtomicUsize>>,
}

#[zbus::interface(name = "mov.vibec0re.test.Stall")]
impl Stall {
    async fn ping(&self) -> u32 {
        if let Some(arrived) = &self.arrived {
            arrived.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
        7
    }
}

const STALL: &str = "mov.vibec0re.test.Stall";
const STALL_PATH: &str = "/mov/vibec0re/test/Stall";

/// Serve [`Stall`] on the bus at `address`, under its well-known name. Keep the
/// returned connection alive for as long as the peer must stay reachable.
async fn serve_stall(address: &str, arrived: Option<Arc<AtomicUsize>>) -> zbus::Connection {
    zbus::connection::Builder::address(address)
        .expect("parse bus address")
        .name(STALL)
        .expect("request the Stall well-known name")
        .serve_at(STALL_PATH, Stall { arrived })
        .expect("serve the Stall interface")
        .build()
        .await
        .expect("build the Stall server connection")
}

/// How many calls are in flight when the broker dies.
const IN_FLIGHT: usize = 8;

/// The bound on how long every in-flight call may take, from the broker's death
/// to its answer, to count as "did not wait for a reconnect that had already
/// happened". The stall it catches is `wait_for_reconnect`'s 5 s ceiling; a
/// retry that does not wait answers in milliseconds. Sized generously against
/// the first and far below the second, so CI contention cannot flake it.
const NO_STALL: Duration = Duration::from_secs(2);

/// Spawn [`IN_FLIGHT`] default-policy (`RetryPolicy::Once`) calls to [`Stall`]
/// on `shared`, and return once every one of them has reached the peer.
async fn calls_in_flight(
    shared: &SharedConnection,
    arrived: &AtomicUsize,
) -> Vec<tokio::task::JoinHandle<Result<u32, BusError>>> {
    let calls = (0..IN_FLIGHT)
        .map(|_| {
            let shared = shared.clone();
            tokio::spawn(async move {
                call_with(&shared, STALL)
                    .at_path(STALL_PATH)
                    .iface(STALL)
                    .method("Ping")
                    .send::<u32>()
                    .await
            })
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    while arrived.load(Ordering::SeqCst) < IN_FLIGHT {
        assert!(
            Instant::now() < deadline,
            "only {} of {IN_FLIGHT} calls reached the peer",
            arrived.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    calls
}

/// Await every call, requiring each to succeed (through its one retry), and
/// return how long the slowest took measured from `died`.
async fn all_answered(
    calls: Vec<tokio::task::JoinHandle<Result<u32, BusError>>>,
    died: Instant,
) -> Duration {
    for call in calls {
        // Well past the 5 s stall, so a regression reads as "took 5 s" in the
        // assertion below rather than as a join timeout here.
        let answer = tokio::time::timeout(Duration::from_secs(20), call)
            .await
            .expect("a call outlived its own retry budget")
            .expect("the call task panicked");
        assert_eq!(
            answer.expect("the retry on the replacement bus must succeed"),
            7
        );
    }
    died.elapsed()
}

/// Deterministic: the replacement connection is already installed — and the
/// epoch already bumped — before any of the in-flight calls fails, so every one
/// of their failures surfaces after the reconnect. That is the late half of a
/// bus restart pinned open: `simulate_disconnect_for_test` plays the operation
/// whose failure noticed first, and the replacement is a second broker because
/// a supervisor that reconnects to the *same* address would have to read
/// `$DBUS_SESSION_BUS_ADDRESS` (see `connection_reconnect.rs`).
#[tokio::test(flavor = "multi_thread")]
async fn a_call_whose_connection_was_replaced_in_flight_retries_without_waiting() {
    let (conn_a, guard_a) = ephemeral_bus().await;
    let (conn_b, guard_b) = ephemeral_bus().await;
    let arrived = Arc::new(AtomicUsize::new(0));
    let _peer_a = serve_stall(&guard_a.address, Some(arrived.clone())).await;
    let _peer_b = serve_stall(&guard_b.address, None).await;

    let shared = SharedConnection::for_test_session(conn_a);
    shared.spawn_supervisor_for_test();
    let calls = calls_in_flight(&shared, &arrived).await;

    let before = shared.epoch();
    shared.simulate_disconnect_for_test(conn_b).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while shared.epoch() == before {
        assert!(
            Instant::now() < deadline,
            "the supervisor never reconnected"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Only now does the broker under the in-flight calls die.
    let died = Instant::now();
    drop(guard_a);
    let slowest = all_answered(calls, died).await;
    assert!(
        slowest < NO_STALL,
        "the slowest of {IN_FLIGHT} in-flight calls took {slowest:?} to answer after a \
         reconnect that had already happened; RetryPolicy::Once waited for a second one"
    );
}

/// The same restart end to end: nothing is installed ahead of time, only armed,
/// so it is the in-flight calls' own failures that clear the dead connection
/// and wake the supervisor, and whether a given call's failure surfaces before
/// or after the reconnect is up to the scheduler. Every ordering must answer
/// promptly. In practice some failure always lands after the reconnect —
/// before the fix this failed 20 runs out of 20, every one at 5.00 s — but
/// nothing guarantees a schedule that does, which is what the deterministic
/// test above is for.
#[tokio::test(flavor = "multi_thread")]
async fn calls_in_flight_across_a_broker_restart_answer_promptly() {
    let (conn_a, guard_a) = ephemeral_bus().await;
    let (conn_b, guard_b) = ephemeral_bus().await;
    let arrived = Arc::new(AtomicUsize::new(0));
    let _peer_a = serve_stall(&guard_a.address, Some(arrived.clone())).await;
    let _peer_b = serve_stall(&guard_b.address, None).await;

    let shared = SharedConnection::for_test_session(conn_a);
    shared.spawn_supervisor_for_test();
    shared.arm_reconnect_for_test(conn_b);
    let calls = calls_in_flight(&shared, &arrived).await;

    let died = Instant::now();
    drop(guard_a);
    let slowest = all_answered(calls, died).await;
    assert!(
        slowest < NO_STALL,
        "the slowest of {IN_FLIGHT} calls in flight across a broker restart took {slowest:?}"
    );
}
