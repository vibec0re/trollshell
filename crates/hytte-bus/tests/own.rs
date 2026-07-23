#![cfg(feature = "system-tests")]

mod common;

use common::ephemeral_bus;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{OwnState, own_name_with};
use std::time::Duration;
use zbus::connection::Builder;
use zbus::fdo::RequestNameFlags;

#[tokio::test(flavor = "multi_thread")]
async fn acquires_unowned_name() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let state = own_name_with(&shared, "mov.vibec0re.test.unique").start();
    let final_state = wait_for_state(state.signal_cloned(), Duration::from_secs(2), |s| {
        matches!(s, OwnState::Owned)
    })
    .await;

    assert!(
        matches!(final_state, OwnState::Owned),
        "expected Owned, got {final_state:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lost_then_reacquired() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let state = own_name_with(&shared, "mov.vibec0re.test.contested").start();
    let _ = wait_for_state(state.signal_cloned(), Duration::from_secs(2), |s| {
        matches!(s, OwnState::Owned)
    })
    .await;

    // Steal the name with a second connection to the same ephemeral bus.
    let conn2 = Builder::address(address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("second connection to ephemeral bus");

    let dbus = zbus::fdo::DBusProxy::new(&conn2).await.unwrap();
    let _ = dbus
        .request_name(
            "mov.vibec0re.test.contested".try_into().unwrap(),
            RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue,
        )
        .await
        .unwrap();

    let lost_state = wait_for_state(state.signal_cloned(), Duration::from_secs(3), |s| {
        matches!(s, OwnState::Lost { .. })
    })
    .await;
    assert!(
        matches!(lost_state, OwnState::Lost { .. }),
        "expected Lost, got {lost_state:?}"
    );

    // Release the name explicitly so our primitive can re-acquire.
    // We can't rely on drop() to flush the async shutdown in time.
    let _ = dbus
        .release_name("mov.vibec0re.test.contested".try_into().unwrap())
        .await;
    drop(dbus);
    drop(conn2);

    let reacquired = wait_for_state(state.signal_cloned(), Duration::from_secs(5), |s| {
        matches!(s, OwnState::Owned)
    })
    .await;
    assert!(
        matches!(reacquired, OwnState::Owned),
        "expected re-acquired Owned, got {reacquired:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn permanently_taken_after_three_losses() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    // Use a short cooldown so the test doesn't block on the 5-minute default.
    // We only assert the *first* PermanentlyTaken transition; the cooldown
    // fires in the background and is torn down when the test process exits
    // (per the documented test-leak contract).
    let state = own_name_with(&shared, "mov.vibec0re.test.camped")
        .permanent_after(3)
        .cooldown_after_permanent(Duration::from_millis(100))
        .start();

    // Use ONE long-lived camper connection so every loss carries the same
    // unique_name in `prev_owner`, letting the consecutive-counter reach 3.
    // The camper subscribes to NameOwnerChanged so it can steal EXACTLY when
    // our primitive re-acquires, giving one clean steal per cycle.
    let address_clone = address.clone();
    let state_for_camper = state.clone();
    tokio::spawn(async move {
        let camper = Builder::address(address_clone.as_str())
            .expect("camper addr")
            .build()
            .await
            .expect("camper conn");
        let dbus = zbus::fdo::DBusProxy::new(&camper).await.unwrap();

        // Subscribe to NameOwnerChanged for our target name so we know when
        // the primitive re-acquires after each theft.
        let mut changes = dbus
            .receive_name_owner_changed_with_args(&[(0, "mov.vibec0re.test.camped")])
            .await
            .unwrap();

        // Steal the name 5 times from the same connection. 3 consecutive losses
        // to the same unique-name triggers PermanentlyTaken.
        let camper_unique = camper.unique_name().map(|u| u.as_str().to_string());
        for _ in 0..5u32 {
            // Grab the name (ReplaceExisting works because our primitive uses AllowReplacement).
            let _ = dbus
                .request_name(
                    "mov.vibec0re.test.camped".try_into().unwrap(),
                    RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue,
                )
                .await;

            // Wait for the primitive to positively observe the theft — its
            // state moving off `Owned` — before releasing, rather than
            // guessing a fixed hold is long enough for NameOwnerChanged
            // delivery. The primitive subscribes to NameOwnerChanged once per
            // connection, before any RequestName attempt, so this is a
            // bounded poll on an already-live subscription, not a race to
            // establish one. We check "not Owned" rather than the exact
            // `Lost` variant because the primitive sets `Lost` and then
            // immediately (synchronously, no `.await` between the two)
            // `Acquiring` on a non-permanent loss — a poller could otherwise
            // race past the transient `Lost` value entirely.
            let _ = wait_for_state(
                state_for_camper.signal_cloned(),
                Duration::from_secs(2),
                |s| !matches!(s, OwnState::Owned),
            )
            .await;

            // Release the name so our primitive can re-acquire.
            let _ = dbus
                .release_name("mov.vibec0re.test.camped".try_into().unwrap())
                .await;

            // Wait until the primitive re-acquires before stealing again, so each
            // theft is cleanly attributed to this camper's unique name.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                if tokio::time::Instant::now() > deadline {
                    break;
                }
                if let Ok(Some(sig)) =
                    tokio::time::timeout(Duration::from_millis(100), changes.next()).await
                    && let Ok(args) = sig.args()
                {
                    let new_owner = args.new_owner().as_ref().map(|n| n.as_str().to_string());
                    // The primitive re-acquired when new_owner is neither empty
                    // nor the camper's unique name.
                    if new_owner.as_deref() != camper_unique.as_deref() && new_owner.is_some() {
                        break;
                    }
                }
            }
        }
        // Camper connection kept alive until task ends.
        drop(camper);
    });

    let final_state = wait_for_state(state.signal_cloned(), Duration::from_secs(15), |s| {
        matches!(s, OwnState::PermanentlyTaken { .. })
    })
    .await;
    assert!(
        matches!(final_state, OwnState::PermanentlyTaken { .. }),
        "expected PermanentlyTaken, got {final_state:?}"
    );
}

// Trivial D-Bus interface used by `at_path_mounts_iface_callable`.
#[derive(Clone)]
struct Greeter;

#[zbus::interface(name = "mov.vibec0re.test.Greeter")]
impl Greeter {
    #[allow(clippy::unused_self)]
    fn hello(&self) -> String {
        "world".to_string()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn at_path_mounts_iface_callable() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let state = own_name_with(&shared, "mov.vibec0re.test.greeter")
        .at_path("/mov/vibec0re/test/Greeter", Greeter)
        .start();

    // Wait for Owned.
    let final_state = wait_for_state(state.signal_cloned(), Duration::from_secs(2), |s| {
        matches!(s, OwnState::Owned)
    })
    .await;
    assert!(
        matches!(final_state, OwnState::Owned),
        "expected Owned, got {final_state:?}"
    );

    // Call the mounted method from a separate connection.
    let client = Builder::address(address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("connect to ephemeral bus for client");
    let proxy = zbus::Proxy::new(
        &client,
        "mov.vibec0re.test.greeter",
        "/mov/vibec0re/test/Greeter",
        "mov.vibec0re.test.Greeter",
    )
    .await
    .expect("create proxy");
    // zbus maps snake_case fn names to PascalCase D-Bus member names.
    let result: String = proxy.call("Hello", &()).await.expect("Hello call");
    assert_eq!(result, "world");
}

async fn wait_for_state<S>(
    signal: S,
    deadline: Duration,
    pred: impl Fn(&OwnState) -> bool,
) -> OwnState
where
    S: futures_signals::signal::Signal<Item = OwnState> + Unpin,
{
    let mut stream = signal.to_stream();
    let mut last = OwnState::Acquiring;
    let end = tokio::time::Instant::now() + deadline;
    while tokio::time::Instant::now() < end {
        if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(50), stream.next()).await {
            last = s.clone();
            if pred(&s) {
                return s;
            }
        }
    }
    last
}
