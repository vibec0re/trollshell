mod common;

use common::ephemeral_bus;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::signals_with;
use std::time::Duration;
use zbus::connection::Builder;
use zbus::object_server::SignalEmitter;

struct Pinger;

#[zbus::interface(name = "cc.hannig.test.Pinger")]
impl Pinger {
    #[zbus(signal)]
    async fn pinged(emitter: &SignalEmitter<'_>, value: u32) -> zbus::Result<()>;
}

#[tokio::test(flavor = "multi_thread")]
async fn delivers_emitted_signal() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();

    // Mount a server emitter on a separate connection.
    let server = Builder::address(address.as_str())
        .unwrap()
        .name("cc.hannig.test.Pinger")
        .unwrap()
        .serve_at("/cc/hannig/test/Pinger", Pinger)
        .unwrap()
        .build()
        .await
        .unwrap();
    let object_server = server.object_server();
    let iface_ref = object_server
        .interface::<_, Pinger>("/cc/hannig/test/Pinger")
        .await
        .unwrap();

    // Subscribe via the bus primitive.
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();
    let sub = signals_with(&shared, "cc.hannig.test.Pinger")
        .at_path("/cc/hannig/test/Pinger")
        .iface("cc.hannig.test.Pinger")
        .signal("Pinged")
        .start();
    let mut events = sub.events();

    // Give the subscription task time to register the match rule with the
    // daemon before we emit. The task runs on the hytte-reactive runtime so
    // it needs at least one scheduler yield plus one D-Bus round-trip.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Emit and expect to receive.
    let emitter = iface_ref.signal_emitter();
    Pinger::pinged(emitter, 42).await.unwrap();

    let evt = tokio::time::timeout(Duration::from_secs(2), events.next())
        .await
        .expect("timeout waiting for signal")
        .expect("stream ended");
    let body: u32 = evt.body.body().deserialize().expect("decode body");
    assert_eq!(body, 42);
}

#[tokio::test(flavor = "multi_thread")]
async fn missed_emissions_bumps_on_reconnect() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let sub = signals_with(&shared, "cc.hannig.test.Pinger")
        .at_path("/cc/hannig/test/Pinger")
        .iface("cc.hannig.test.Pinger")
        .signal("Pinged")
        .start();

    let initial = sub.missed_count();

    // Open a replacement connection to inject into the supervisor, then
    // simulate a disconnect/reconnect cycle.
    let replacement = Builder::address(address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("replacement connection");
    shared.simulate_disconnect_for_test(replacement).await;

    // Wait up to 2s for the missed_count counter to bump.
    let mut bumped = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        let now = sub.missed_count();
        if now > initial {
            bumped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(bumped, "missed_emissions did not bump after disconnect");
}
