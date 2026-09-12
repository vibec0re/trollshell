#![cfg(feature = "system-tests")]

mod common;

use common::{ephemeral_bus, restart_on_same_address};
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{BusError, PropState, RetryPolicy, call_with, property_with};
use std::time::Duration;
use zbus::connection::Builder;

#[tokio::test(flavor = "multi_thread")]
async fn epoch_bumps_after_supervised_reconnect() {
    let (conn, guard) = ephemeral_bus().await;

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let initial = shared.epoch();
    assert_eq!(initial, 1);

    // Open a second independent connection to the same ephemeral bus. We will
    // inject this as the "replacement" connection so the supervisor reconnects
    // without needing to open a real session bus. This avoids mutating
    // DBUS_SESSION_BUS_ADDRESS (which would require unsafe code).
    let replacement = Builder::address(guard.address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("open replacement connection to ephemeral bus");

    // Simulate a disconnect: inject the replacement, clear the cached conn,
    // and wake the supervisor. The supervisor will find the injected connection
    // and use it instead of calling Connection::session().
    shared.simulate_disconnect_for_test(replacement).await;

    let mut epoch_stream = shared.epoch_signal().to_stream();
    let mut saw_higher = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_millis(100), epoch_stream.next()).await;
        if let Ok(Some(v)) = next
            && v > initial
        {
            saw_higher = true;
            break;
        }
    }
    assert!(
        saw_higher,
        "epoch did not advance within 5s of simulated disconnect"
    );
}

// Regression test for issue #433 fix 1: a late transient failure from a
// superseded connection attempt must not clobber a connection the supervisor
// has already re-established.
#[tokio::test(flavor = "multi_thread")]
async fn late_transient_failure_does_not_clobber_fresh_connection() {
    let (conn, guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);

    // A replacement connection standing in for the one the supervisor would
    // install after a disconnect.
    let fresh = Builder::address(guard.address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("open replacement connection to ephemeral bus");

    // Run an op that, mid-flight, sees a *fresh* connection installed
    // (generation bump) and only then fails transiently — exactly the "late
    // failure from a superseded attempt" race. The generation guard in
    // `with_conn` must decline to clear the fresh connection.
    let shared_for_op = shared.clone();
    let shared_for_install = shared.clone();
    let result = shared_for_op
        .with_conn(move |_old_conn| async move {
            shared_for_install
                .install_fresh_connection_for_test(fresh)
                .await;
            Err::<(), zbus::Error>(zbus::Error::FDO(Box::new(zbus::fdo::Error::Disconnected(
                "late failure from superseded conn".to_owned(),
            ))))
        })
        .await;
    assert!(
        result.is_err(),
        "the in-flight op itself must still report its failure"
    );

    // The fresh connection must still be cached: a subsequent op finds a
    // connection rather than the mid-reconnect transient sentinel.
    let after = shared
        .with_conn(|_c| async move { Ok::<(), zbus::Error>(()) })
        .await;
    assert!(
        after.is_ok(),
        "late transient failure clobbered the freshly installed connection"
    );
}

// ── #1175: the *detection* half of reconnect, exercised for real ─────────────
//
// Every test above (and every other reconnect test in this crate) drives
// `simulate_disconnect_for_test`, which clears `Inner::conn` and wakes the
// supervisor *itself* — it never touches `with_conn`'s own
// invalidate-and-notify block (connection.rs:205-211). Deleting that block
// left the whole suite green. This test does not call
// `simulate_disconnect_for_test` at all (grep this file: zero hits): it kills
// the real ephemeral `dbus-daemon` out from under a live connection, restarts
// a fresh one on the identical socket path, and requires the block itself to
// notice and clear the dead connection before anything recovers.
//
// A lone `property()` subscription cannot drive this by itself:
// `subscribe_properties_changed` (property.rs) fetches the cached connection
// through a trivial, infallible `with_conn(|conn| async { Ok(conn) })` and
// then issues its real `PropertiesChanged` AddMatch call *outside* that
// wrapper, so a dead connection just makes it retry forever without ever
// exercising `with_conn`'s error arm. In production some other primitive
// sharing the same `SharedConnection` is always the one whose *fallible* call
// actually goes through `with_conn` end to end (property.rs's own `cold_get`
// is one such call, but it is never reached while subscribing keeps
// failing) — here that role is played explicitly by a one-shot `call()`
// against `org.freedesktop.DBus.GetId`, polled until it stops seeing the raw
// transport error and starts seeing `with_conn`'s own "no cached connection
// (mid-reconnect)" sentinel (connection.rs:176) instead. That transition can
// only happen if the invalidate-and-notify block actually ran and cleared
// `Inner::conn` — the exact thing #1175 asks to be pinned.
//
// Recovery then goes through `install_fresh_connection_for_test` — the same
// "hand it a freshly dialled connection" step `supervisor_loop`'s own
// successful-reconnect arm performs — deliberately *not*
// `spawn_supervisor_for_test`: a real supervisor's `open_connection` reads
// `$DBUS_SESSION_BUS_ADDRESS`, which this crate cannot safely repoint at the
// restarted daemon (mutating it needs `unsafe`, forbidden workspace-wide —
// see the `simulate_disconnect_for_test` comment above), and racing a real
// session bus (if one happens to be reachable in the environment running this
// test) against our own restarted daemon for who wins the reconnect is a trap
// this test has no reason to walk into.
struct Counter {
    value: u32,
}

#[zbus::interface(name = "mov.vibec0re.test.Counter")]
impl Counter {
    #[zbus(property)]
    fn value(&self) -> u32 {
        self.value
    }
}

/// Serve the `Counter` test interface at a fixed path on `address`. Returns
/// the server-side `Connection`; keep it alive for as long as the interface
/// must stay reachable.
async fn serve_counter(address: &str, value: u32) -> zbus::Connection {
    zbus::connection::Builder::address(address)
        .expect("parse bus address")
        .name("mov.vibec0re.test.Counter")
        .expect("request Counter well-known name")
        .serve_at("/mov/vibec0re/test/Counter", Counter { value })
        .expect("serve Counter interface")
        .build()
        .await
        .expect("build Counter server connection")
}

#[tokio::test(flavor = "multi_thread")]
async fn killed_and_restarted_daemon_is_detected_by_with_conn_itself() {
    let (conn_a, guard_a) = ephemeral_bus().await;
    let address = guard_a.address.clone();

    let _server_a = serve_counter(&address, 42).await;

    // Deliberately no `spawn_supervisor_for_test()` — see the module comment
    // above for why.
    let shared = SharedConnection::for_test_session(conn_a);

    let prop = property_with::<u32>(&shared, "mov.vibec0re.test.Counter")
        .at_path("/mov/vibec0re/test/Counter")
        .iface("mov.vibec0re.test.Counter")
        .name("Value")
        .start();
    let mut stream = prop.signal().to_stream();

    // Drain to the initial Loaded(42) before touching the daemon.
    let mut got_initial = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !got_initial {
        if let Ok(Some(PropState::Loaded(42))) =
            tokio::time::timeout(Duration::from_millis(100), stream.next()).await
        {
            got_initial = true;
        }
    }
    assert!(got_initial, "initial cold Get never reached Loaded(42)");

    // Kill daemon A and boot a fresh one on the identical socket path (#1175).
    // `shared`'s cached connection now points at a dead peer; nothing has
    // told `with_conn` that yet. Re-serve Counter on the new daemon so the
    // property tracker has something to recover *to*.
    let (conn_b, _guard_b) = restart_on_same_address(guard_a).await;
    let _server_b = serve_counter(&address, 42).await;

    // Poll for both: the property subscription observing the disruption
    // (Stale, since it already has a last-known value), and the real
    // detection signal on a *different* call through the same connection.
    let mut saw_stale = false;
    let mut detected = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline && !detected {
        if let Ok(Some(state)) =
            tokio::time::timeout(Duration::from_millis(20), stream.next()).await
            && matches!(state, PropState::Stale(42))
        {
            saw_stale = true;
        }

        let probe: Result<String, BusError> = call_with(&shared, "org.freedesktop.DBus")
            .at_path("/org/freedesktop/DBus")
            .iface("org.freedesktop.DBus")
            .method("GetId")
            .retry(RetryPolicy::Never)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        match &probe {
            Err(e) if e.is_transient() && e.to_string().contains("no cached connection") => {
                detected = true;
            }
            _ => tokio::time::sleep(Duration::from_millis(80)).await,
        }
    }
    assert!(
        detected,
        "with_conn never reported \"no cached connection\" for a call issued \
         after the daemon died — its invalidate-and-notify block \
         (connection.rs:205-211) never cleared the dead connection"
    );
    assert!(
        saw_stale,
        "property subscription never observed PropState::Stale after the daemon died"
    );

    // Recovery: hand the connection over exactly as a real supervisor's
    // successful-reconnect arm would — NOT simulate_disconnect_for_test.
    shared.install_fresh_connection_for_test(conn_b).await;

    let mut recovered = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !recovered {
        if let Ok(Some(PropState::Loaded(42))) =
            tokio::time::timeout(Duration::from_millis(100), stream.next()).await
        {
            recovered = true;
        }
    }
    assert!(
        recovered,
        "property subscription never recovered to Loaded(42) after real \
         detection + reconnect"
    );
}
