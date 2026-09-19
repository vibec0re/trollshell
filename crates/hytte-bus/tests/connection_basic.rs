#![cfg(feature = "system-tests")]

mod common;

use common::ephemeral_bus;
use hytte_bus::BusKind;
use hytte_bus::test_support::SharedConnection;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn with_conn_returns_connection_on_healthy_bus() {
    let (test_conn, _guard) = ephemeral_bus().await;
    // Wraps the ephemeral connection directly via the test-only
    // `for_test_session` constructor — no `DBUS_SESSION_BUS_ADDRESS` mutation
    // (which `unsafe_code = "forbid"` rules out; see `connection.rs`'s note
    // by `simulate_disconnect_for_test`) — then asserts `with_conn` sees that
    // same connection (matching unique names).
    let shared = SharedConnection::for_test_session(test_conn.clone());
    let unique_name_via_shared: Option<String> = shared
        .with_conn(
            |c| async move { Ok::<_, zbus::Error>(c.unique_name().map(ToString::to_string)) },
        )
        .await
        .expect("with_conn returns Ok on healthy bus");

    let unique_name_direct = test_conn.unique_name().map(ToString::to_string);
    assert_eq!(unique_name_via_shared, unique_name_direct);
}

/// A `SharedConnection` must be able to answer an inbound method call from the
/// moment it exists — **before** anything is exported or owned on it (#1011).
///
/// zbus creates its object server lazily, and the dispatch task that object
/// server spawns is the only consumer of inbound `MethodCall` messages: it
/// subscribes by adding a `msg_type=MethodCall, destination=<unique name>`
/// match rule to the connection's internal routing table. Until that rule
/// exists an arriving call matches no receiver and zbus **drops it with no
/// reply at all** — there is no fallback arm in `zbus::Connection` that
/// synthesises an `UnknownObject`/`UnknownMethod` error for an unhandled call.
/// A zbus call has no reply timeout by default either, so the peer does not get
/// an error: it waits forever.
///
/// Before `connection.rs`'s `begin_dispatching`, the first thing to touch
/// `object_server()` on a hytte connection was whichever `export`/`own`
/// supervisor happened to mount first, on the hytte runtime, whenever it got
/// scheduled. This test asserts the window is not merely small but *not the
/// caller's problem*: nothing is exported here at all, so on a tree without
/// that call the object server is never created and the `Introspect` below is
/// answered by nobody — the assertion fails by name rather than hanging,
/// because the wait is bounded.
///
/// `Introspectable.Introspect` on `/` is the probe because zbus's object server
/// serves it from the root node with no interface mounted, so a reply proves
/// the dispatch task is live and proves nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn shared_connection_answers_method_calls_before_anything_is_exported() {
    let (test_conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();
    let unique = test_conn
        .unique_name()
        .expect("ephemeral connection has a unique name")
        .as_str()
        .to_string();

    // Nothing is ever exported or owned on this one.
    let _shared = SharedConnection::for_test_session(test_conn);

    let client = zbus::connection::Builder::address(address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("client connection");
    let proxy = zbus::Proxy::new(
        &client,
        unique.as_str(),
        "/",
        "org.freedesktop.DBus.Introspectable",
    )
    .await
    .expect("client proxy");

    // Bounded, and retried: the residual window is one `AddMatch` round-trip
    // that `begin_dispatching` starts but cannot await (zbus exposes the
    // `started_event` only through `connection::Builder`). A swallowed call
    // costs one retry; a connection that never dispatches costs the assertion.
    let overall = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut answered = false;
    let mut unanswered = 0u32;
    while tokio::time::Instant::now() < overall {
        match tokio::time::timeout(
            Duration::from_secs(5),
            proxy.call::<_, _, String>("Introspect", &()),
        )
        .await
        {
            Ok(Ok(xml)) => {
                assert!(
                    xml.contains("org.freedesktop.DBus.Introspectable"),
                    "unexpected introspection reply: {xml}"
                );
                answered = true;
                break;
            }
            // An *error* reply is still proof the dispatch task is live.
            Ok(Err(_)) => {
                answered = true;
                break;
            }
            Err(_elapsed) => unanswered += 1,
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        answered,
        "a SharedConnection with nothing exported on it answered no method \
         call at all ({unanswered} calls got no reply): zbus's object-server \
         dispatch task was never started, so every call addressed to this \
         connection is dropped with no reply and its caller hangs (#1011)"
    );
}

// Verify the public API surface: BusKind is accessible from outside the crate.
const _: fn() = || {
    let _ = BusKind::Session;
};
