mod common;

use common::ephemeral_bus;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{call_with, BusError, RetryPolicy};
use zbus::connection::Builder;

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
    let replacement = Builder::address(guard.address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("replacement conn");
    shared.simulate_disconnect_for_test(replacement).await;

    let names: Result<Vec<String>, BusError> = call_with(&shared, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("ListNames")
        .args(())
        .retry(RetryPolicy::Once)
        .send()
        .await;

    assert!(names.is_ok(), "expected retry-Once to succeed; got {names:?}");
}
