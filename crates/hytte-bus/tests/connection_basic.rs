#![cfg(feature = "system-tests")]

mod common;

use common::ephemeral_bus;
use hytte_bus::BusKind;
use hytte_bus::test_support::SharedConnection;

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

// Verify the public API surface: BusKind is accessible from outside the crate.
const _: fn() = || {
    let _ = BusKind::Session;
};
