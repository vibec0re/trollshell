#![cfg(feature = "system-tests")]

mod common;

use common::{CALL_BUDGET, PROBE_BUDGET, ephemeral_bus};
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
///
/// Both liveness loops bound every call with [`CALL_BUDGET`] and treat "no
/// reply at all" as *retry*, never as an answer. This test is where #1011 was
/// found: an unbounded raw call here parked forever on a `Hello` that zbus had
/// dropped, and because a loop re-checks its deadline only between iterations,
/// the 2 s budget it used to carry was unreachable. See `common`'s
/// `CALL_BUDGET` for the mechanism and the measurements.
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
    let mut unanswered = 0u32;
    let deadline = tokio::time::Instant::now() + PROBE_BUDGET;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(CALL_BUDGET, proxy.call::<_, _, String>("Hello", &())).await {
            Ok(Ok(reply)) => {
                assert_eq!(reply, "world");
                mounted = true;
                break;
            }
            // An error reply — the object is not mounted *yet*. Retry.
            Ok(Err(_)) => {}
            // No reply at all within the budget: the connection was not yet
            // dispatching when this call landed, so zbus dropped it. Not a pass
            // and not a fail — count it, retry, and let the assertion below
            // report it if the whole budget goes this way.
            Err(_elapsed) => unanswered += 1,
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        mounted,
        "exported object was never reachable within {PROBE_BUDGET:?} \
         ({unanswered} Hello calls got no reply at all within {CALL_BUDGET:?} \
         each — see common::CALL_BUDGET, #1011)"
    );

    // Drop the handle — the supervisor must unmount the interface.
    drop(handle);

    // The Hello call must start failing once the object is unmounted. Silence
    // is deliberately NOT accepted as proof of unmounting: the leak this test
    // exists to catch is "a daemon can still reach the object", and only an
    // error *reply* shows the connection answered and refused. Treating an
    // unanswered call as success would make the assertion pass on exactly the
    // pathology #1011 was.
    let mut unmounted = false;
    let mut unanswered = 0u32;
    let deadline = tokio::time::Instant::now() + PROBE_BUDGET;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(CALL_BUDGET, proxy.call::<_, _, String>("Hello", &())).await {
            Ok(Err(_)) => {
                unmounted = true;
                break;
            }
            Ok(Ok(_)) => {}
            Err(_elapsed) => unanswered += 1,
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        unmounted,
        "exported object stayed reachable after the handle was dropped (leak): \
         no error reply within {PROBE_BUDGET:?} ({unanswered} Hello calls got \
         no reply at all within {CALL_BUDGET:?} each)"
    );
}
