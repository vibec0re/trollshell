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

    // Let the task actually get scheduled at least once before we drop. This
    // is a courtesy, not a correctness requirement: `HandleTracker`'s count is
    // decremented independently of task scheduling (see hytte-bus's
    // handle.rs), so the task will observe `all_dropped()` the moment it
    // first checks, even if we dropped before it ever ran. A couple of
    // scheduler yields exercise the "already running and parked in select!"
    // path too, without guessing a wall-clock delay.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

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

    // Rather than guessing a single delay by which the subscription task has
    // registered its match rule with the daemon, keep re-emitting (with a
    // fresh, uniquely-tagged value each attempt) while polling for delivery.
    // Once the task's AddMatch has landed, the next attempt is delivered and
    // decoded here — this early-exits as soon as that's observed instead of
    // paying a fixed tax on every run.
    let emitter = iface_ref.signal_emitter();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut attempt = 0u32;
    let mut received = None;
    while tokio::time::Instant::now() < deadline && received.is_none() {
        Pinger::pinged(emitter, attempt).await.unwrap();

        let poll_deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        while tokio::time::Instant::now() < poll_deadline {
            if let Ok(Some(evt)) =
                tokio::time::timeout(Duration::from_millis(20), events.next()).await
            {
                let body: u32 = evt.body.body().deserialize().expect("decode body");
                if body == attempt {
                    received = Some(body);
                    break;
                }
            }
        }
        attempt += 1;
    }

    assert!(received.is_some(), "timeout waiting for signal delivery");
}
