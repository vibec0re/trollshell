//! Pins the comparison inside `SharedConnection::invalidate_if_generation`
//! (#1551, bullet 8): a failed call clears the pooled connection only if the
//! connection is still the generation the call's cached proxy was built on.
//!
//! The two race windows that rule closes cannot be held open by any seam, but
//! they both reduce to this comparison, so it is pinned directly. Deleting the
//! `Generation(guard.generation) == issued_on` check makes
//! `a_stale_generation_keeps_the_fresh_connection` fail.

#![cfg(feature = "system-tests")]

mod common;

use common::{connect, ephemeral_bus};
use hytte_bus::test_support::SharedConnection;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Whether the supervisor was woken: `notify_one` leaves a permit, so a
/// wake-up that happened earlier is still seen here.
async fn woken(notify: &Arc<Notify>) -> bool {
    tokio::time::timeout(Duration::from_millis(200), notify.notified())
        .await
        .is_ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_generation_keeps_the_fresh_connection() {
    let (conn, guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    let notify = shared.supervisor_notify_for_test();
    let stale = shared.generation_for_test().await;

    // A reconnect installs a newer connection; the failed call still carries
    // the generation of the one it was built on.
    shared
        .install_fresh_connection_for_test(connect(guard.address.as_str()).await)
        .await;
    assert_eq!(shared.generation_for_test().await, stale + 1);

    shared.invalidate_if_generation_for_test(stale).await;

    assert!(
        shared.is_connected_for_test().await,
        "a failure on generation {stale} must not clear the connection installed at generation {}",
        stale + 1
    );
    assert!(
        !woken(&notify).await,
        "a stale-generation failure must not wake the supervisor"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_current_generation_clears_the_connection_and_wakes_the_supervisor_once() {
    let (conn, guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    let notify = shared.supervisor_notify_for_test();
    shared
        .install_fresh_connection_for_test(connect(guard.address.as_str()).await)
        .await;
    let current = shared.generation_for_test().await;

    shared.invalidate_if_generation_for_test(current).await;

    assert!(
        !shared.is_connected_for_test().await,
        "a failure on the current generation must clear the connection"
    );
    assert!(woken(&notify).await, "the supervisor must be woken");

    // A second failure from the same generation finds nothing to clear, so
    // there is no second wake-up (the first `notified()` consumed the permit).
    shared.invalidate_if_generation_for_test(current).await;
    assert!(
        !woken(&notify).await,
        "the supervisor must be woken once, not once per failed call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_connection_it_is_a_no_op() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    let notify = shared.supervisor_notify_for_test();
    let generation = shared.generation_for_test().await;
    shared.drop_connection_for_test().await;

    shared.invalidate_if_generation_for_test(generation).await;

    assert!(!shared.is_connected_for_test().await);
    assert!(
        !woken(&notify).await,
        "with nothing installed there is nothing to retract and nobody to wake"
    );
}
