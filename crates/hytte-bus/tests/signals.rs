#![cfg(feature = "system-tests")]

mod common;

use common::ephemeral_bus;
use futures_util::StreamExt;
use hytte_bus::signals_with;
use hytte_bus::test_support::SharedConnection;
use std::time::Duration;
use zbus::connection::Builder;
use zbus::object_server::SignalEmitter;

// ── Task-exit-on-drop test ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn task_exits_when_subscription_dropped() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);

    // Start a subscription and grab the exit probe *before* dropping.
    let sub = signals_with(&shared, "mov.vibec0re.test.Pinger")
        .at_path("/mov/vibec0re/test/Pinger")
        .iface("mov.vibec0re.test.Pinger")
        .signal("Pinged")
        .start();

    // Grab the task-done receiver while we still hold the subscription.
    let done_rx = sub
        .task_done_receiver()
        .await
        .expect("task_done_receiver should be Some on first call");

    // Give the task a moment to start up and reach its first select! wait.
    // The task parks in the inner loop on `stream.next()` / epoch_stream.
    // After drop, `receiver_count()` becomes 0 and the check fires on the
    // next select! iteration.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Drop all handles — this is the moment the task should detect the leak.
    drop(sub);

    // The task must exit within 1 second.
    tokio::time::timeout(Duration::from_secs(1), done_rx)
        .await
        .expect("timeout: subscription task did not exit within 1s after drop")
        .expect("task_done_tx was dropped without sending — task panicked?");
}

struct Pinger;

#[zbus::interface(name = "mov.vibec0re.test.Pinger")]
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
        .name("mov.vibec0re.test.Pinger")
        .unwrap()
        .serve_at("/mov/vibec0re/test/Pinger", Pinger)
        .unwrap()
        .build()
        .await
        .unwrap();
    let object_server = server.object_server();
    let iface_ref = object_server
        .interface::<_, Pinger>("/mov/vibec0re/test/Pinger")
        .await
        .unwrap();

    // Subscribe via the bus primitive.
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();
    let sub = signals_with(&shared, "mov.vibec0re.test.Pinger")
        .at_path("/mov/vibec0re/test/Pinger")
        .iface("mov.vibec0re.test.Pinger")
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
