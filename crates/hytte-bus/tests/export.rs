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

/// Upper bound on a **single** raw `zbus::Proxy::call` in this file.
///
/// A zbus method call carries no reply timeout unless the connection was built
/// with one, and `method_timeout` defaults to `None` — so a call whose reply
/// never comes does not fail, it parks the calling task **forever**. The
/// liveness loops below re-check their own deadline only *between* iterations,
/// so an unbounded call inside one makes that deadline unreachable.
///
/// That is exactly what #1011 was. `zbus` drops an inbound method call that
/// arrives before its object-server dispatch task has registered its match
/// rule — no reply, not even an error — so the first `Hello` here could be
/// answered by nobody. The test then hung instead of failing: five
/// `nix flake check` runs went silent for ~51 minutes apiece and were killed
/// by the job timeout, with `test export_unmounts_on_handle_drop has been
/// running for over 60 seconds` as the last thing CI ever printed.
/// `connection.rs`'s `begin_dispatching` is what removes the race; this bound
/// is what makes a *recurrence* — of that or of any other lost reply — a named
/// red assertion in seconds instead of a silent hang.
///
/// **This is a liveness guard, not a latency assertion**, in the same sense as
/// `common`'s `DBUS_DAEMON_STARTUP_BUDGET`: nothing here claims a D-Bus call
/// *should* complete within five seconds. These tests run inside
/// `nix flake check` next to two `nixosTest` VMs and a full workspace compile,
/// so CPU starvation is the normal condition. Five seconds is far past any
/// honest latency (a green run answers the first call in ~1 ms) and far short
/// of [`PROBE_BUDGET`], which is what leaves room to retry rather than fail.
const CALL_BUDGET: Duration = Duration::from_secs(5);

/// Upper bound on a whole mounted/unmounted liveness loop.
///
/// Was 2 s while each call inside could park forever, which made it decorative:
/// the loop could not reach its own deadline. It is deliberately several times
/// [`CALL_BUDGET`] so that a call which *is* swallowed costs one retry rather
/// than the test — trading a hang for a flake would be no fix at all. Only the
/// failing path spends any of this; a healthy run leaves both loops on the
/// first iteration.
const PROBE_BUDGET: Duration = Duration::from_secs(20);

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
            // No reply at all within the budget. Not a pass and not a fail:
            // count it, retry, and let the assertion below report it if the
            // whole budget goes this way.
            Err(_elapsed) => unanswered += 1,
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        mounted,
        "exported object was never reachable within {PROBE_BUDGET:?} \
         ({unanswered} of the Hello calls got no reply at all within \
         {CALL_BUDGET:?} — see this file's CALL_BUDGET note, #1011)"
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
         no error reply within {PROBE_BUDGET:?} ({unanswered} of the Hello \
         calls got no reply at all within {CALL_BUDGET:?})"
    );
}
