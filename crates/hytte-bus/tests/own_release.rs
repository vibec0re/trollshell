#![cfg(feature = "system-tests")]
//! Recovery from a squatted name, against a real broker (#669).
//!
//! `own.rs` covers the *other* half of this story — that a name held by a peer
//! which refuses replacement escalates to `PermanentlyTaken` instead of
//! retrying at 4 Hz forever (#653/#668). This file covers what happens next:
//! when that peer exits, the name must come back within milliseconds, driven by
//! `NameOwnerChanged`, rather than on the next cooldown wake up to five minutes
//! later.
//!
//! Deliberately a separate test binary from `own.rs`, and deliberately not
//! extending `tests/common/mod.rs`: the harness there is being changed
//! concurrently for #678, and the two lanes are kept disjoint. The cost is one
//! duplicated `wait_for_state` helper, which each integration test binary needs
//! its own copy of anyway (they are separate crates).

mod common;

use common::ephemeral_bus;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{OwnState, own_name_with};
use std::time::Duration;
use zbus::connection::Builder;
use zbus::fdo::RequestNameFlags;

const NAME: &str = "mov.vibec0re.test.released";

/// Far longer than anything this test is willing to wait for. This is the
/// assertion's teeth: if recovery still needed the cooldown wake, the test
/// fails by timeout instead of passing ten minutes late.
const COOLDOWN: Duration = Duration::from_mins(10);

/// What "near-instant" is allowed to mean on a loaded CI box. The real path is
/// sub-millisecond; the margin against `COOLDOWN` is 60x, so this cannot flake
/// into a false pass.
const RECOVERY_BUDGET: Duration = Duration::from_secs(10);

/// A squatter takes the name without `AllowReplacement`, so every `RequestName`
/// we make comes back `Exists` and the primitive latches `PermanentlyTaken`
/// (the mako-owns-`org.freedesktop.Notifications` case). When the squatter then
/// releases the name, the primitive must acquire it on the `NameOwnerChanged`
/// wake — with the cooldown set to ten minutes, nothing else can explain a pass.
#[tokio::test(flavor = "multi_thread")]
async fn a_released_name_is_reacquired_without_waiting_out_the_cooldown() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let squatter = Builder::address(address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("squatter connection to ephemeral bus");
    let squatter_unique = squatter
        .unique_name()
        .map(|u| u.as_str().to_string())
        .expect("squatter unique name");
    let squatter_dbus = zbus::fdo::DBusProxy::new(&squatter)
        .await
        .expect("squatter DBusProxy");
    let reply = squatter_dbus
        .request_name(
            NAME.try_into().expect("well-known name"),
            RequestNameFlags::DoNotQueue.into(),
        )
        .await
        .expect("squatter RequestName");
    assert!(
        matches!(reply, zbus::fdo::RequestNameReply::PrimaryOwner),
        "squatter must own the name before the primitive starts, got {reply:?}"
    );

    let state = own_name_with(&shared, NAME)
        .permanent_after(2)
        .cooldown_after_permanent(COOLDOWN)
        .start();

    // Phase 1: the give-up. Two refused attempts (~0.75s of short retries)
    // latch PermanentlyTaken, and the primitive settles into the wait.
    let latched = wait_for_state(state.signal_cloned(), Duration::from_secs(10), |s| {
        matches!(s, OwnState::PermanentlyTaken { .. })
    })
    .await;
    match latched {
        OwnState::PermanentlyTaken { ref current_owner } => assert_eq!(
            current_owner, &squatter_unique,
            "PermanentlyTaken must name the connection actually holding it"
        ),
        ref other => panic!("expected PermanentlyTaken, got {other:?}"),
    }

    // Phase 2: the squatter leaves the field. The broker emits
    // NameOwnerChanged with an empty new_owner (nobody is queued — both sides
    // asked with DoNotQueue), which is exactly the wake under test.
    squatter_dbus
        .release_name(NAME.try_into().expect("well-known name"))
        .await
        .expect("squatter ReleaseName");

    let recovered = wait_for_state(state.signal_cloned(), RECOVERY_BUDGET, |s| {
        matches!(s, OwnState::Owned)
    })
    .await;
    assert!(
        matches!(recovered, OwnState::Owned),
        "the released name must be re-acquired on the NameOwnerChanged wake, \
         well inside the {COOLDOWN:?} cooldown; got {recovered:?}"
    );

    // Keep the squatter's connection alive until the assertions have run, so a
    // dropped connection is never what freed the name.
    drop(squatter_dbus);
    drop(squatter);
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
