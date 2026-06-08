mod common;

use common::ephemeral_bus;
use hytte_bus::BusKind;
use hytte_bus::test_support::SharedConnection;

#[tokio::test(flavor = "multi_thread")]
async fn with_conn_returns_connection_on_healthy_bus() {
    let (test_conn, _guard) = ephemeral_bus().await;
    // Inject the ephemeral bus address as DBUS_SESSION_BUS_ADDRESS so
    // SharedConnection::session() (which calls Connection::session()) hits it.
    // The address printed by dbus-daemon is captured inside ephemeral_bus;
    // for tests we'd ideally inject via a test-only constructor — but for
    // this first test we use the env-var path that production uses too.
    //
    // The harness sets DBUS_SESSION_BUS_ADDRESS via `for_test` injection
    // in Task 6; for now this test asserts the API shape only.

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
