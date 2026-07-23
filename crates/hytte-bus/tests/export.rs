#![cfg(feature = "system-tests")]

mod common;

use common::ephemeral_bus;
use hytte_bus::export_object_with;
use hytte_bus::test_support::SharedConnection;
use std::time::Duration;
use zbus::connection::Builder;

#[derive(Clone)]
struct Greeter;

#[zbus::interface(name = "mov.vibec0re.test.Greeter")]
impl Greeter {
    #[allow(clippy::unused_self)]
    fn hello(&self) -> String {
        "world".to_string()
    }
}

/// Dropping the last `ExportHandle` must unregister the interface from the
/// connection — otherwise a daemon that recorded our unique name keeps reaching
/// an object whose owner believes it retired (the NM secret-agent leak).
#[tokio::test(flavor = "multi_thread")]
async fn export_unmounts_on_handle_drop() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();
    // The export owns no well-known name, so a client addresses the object by
    // the shared connection's unique name. Capture it before `conn` is moved.
    let unique = conn
        .unique_name()
        .expect("shared connection has a unique name")
        .as_str()
        .to_string();
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let handle = export_object_with(&shared, "/mov/vibec0re/test/Exported").start(Greeter);

    let client = Builder::address(address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("client connection");
    let proxy = zbus::Proxy::new(
        &client,
        unique.as_str(),
        "/mov/vibec0re/test/Exported",
        "mov.vibec0re.test.Greeter",
    )
    .await
    .expect("client proxy");

    // Wait until the supervisor has mounted the object (it mounts on epoch 1).
    let mut mounted = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if let Ok(reply) = proxy.call::<_, _, String>("Hello", &()).await {
            assert_eq!(reply, "world");
            mounted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(mounted, "exported object was never reachable");

    // Drop the handle — the supervisor must unmount the interface.
    drop(handle);

    // The Hello call must start failing once the object is unmounted.
    let mut unmounted = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if proxy.call::<_, _, String>("Hello", &()).await.is_err() {
            unmounted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        unmounted,
        "exported object stayed reachable after the handle was dropped (leak)"
    );
}
