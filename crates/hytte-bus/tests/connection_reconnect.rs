mod common;

use common::ephemeral_bus;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
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
