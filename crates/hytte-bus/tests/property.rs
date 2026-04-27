mod common;

use common::ephemeral_bus;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{property_with, PropState};
use std::time::Duration;

struct Counter {
    value: u32,
}

#[zbus::interface(name = "cc.hannig.test.Counter")]
impl Counter {
    #[zbus(property)]
    fn value(&self) -> u32 {
        self.value
    }

    #[zbus(property)]
    fn set_value(&mut self, v: u32) {
        self.value = v;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cold_start_emits_loading_then_loaded() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();

    // Stand up a server that exposes the Counter interface.
    let _server = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("cc.hannig.test.Counter")
        .unwrap()
        .serve_at("/cc/hannig/test/Counter", Counter { value: 7 })
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let signal = property_with::<u32>(&shared, "cc.hannig.test.Counter")
        .at_path("/cc/hannig/test/Counter")
        .iface("cc.hannig.test.Counter")
        .name("Value")
        .start();

    let mut stream = signal.to_stream();
    let mut saw_loading = false;
    let mut saw_loaded = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && saw_loaded.is_none() {
        let next = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        if let Ok(Some(state)) = next {
            match state {
                PropState::Loading => saw_loading = true,
                PropState::Loaded(v) => saw_loaded = Some(v),
                PropState::Stale(_) => {}
            }
        }
    }

    assert!(saw_loading, "expected at least one Loading emission");
    assert_eq!(saw_loaded, Some(7));
}

#[tokio::test(flavor = "multi_thread")]
async fn properties_changed_emits_loaded_with_new_value() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();

    let server = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("cc.hannig.test.Counter")
        .unwrap()
        .serve_at("/cc/hannig/test/Counter", Counter { value: 1 })
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let signal = property_with::<u32>(&shared, "cc.hannig.test.Counter")
        .at_path("/cc/hannig/test/Counter")
        .iface("cc.hannig.test.Counter")
        .name("Value")
        .start();
    let mut stream = signal.to_stream();

    // Drain initial Loading + Loaded(1).
    let mut current = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && !matches!(current, Some(1)) {
        if let Ok(Some(PropState::Loaded(v))) =
            tokio::time::timeout(Duration::from_millis(50), stream.next()).await
        {
            current = Some(v);
        }
    }
    assert_eq!(current, Some(1));

    // Give the tracking task time to advance past Loaded(1) and into the
    // PropertiesChanged subscription loop before we emit the signal.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Mutate the server-side property and emit PropertiesChanged.
    let iface_ref = server
        .object_server()
        .interface::<_, Counter>("/cc/hannig/test/Counter")
        .await
        .unwrap();
    {
        let mut iface = iface_ref.get_mut().await;
        iface.set_value(99);
        iface
            .value_changed(iface_ref.signal_emitter())
            .await
            .unwrap();
    }

    // Expect the consumer signal to update to Loaded(99).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut updated = None;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(PropState::Loaded(v))) =
            tokio::time::timeout(Duration::from_millis(50), stream.next()).await
        {
            updated = Some(v);
            break;
        }
    }
    assert_eq!(updated, Some(99));
}
