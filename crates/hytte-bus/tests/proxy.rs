#![cfg(feature = "system-tests")]

mod common;

use common::ephemeral_bus;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{ProxyState, proxy_with};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn live_when_peer_present() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let proxy = proxy_with(&shared, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .build()
        .await
        .expect("build proxy");

    let mut stream = proxy.liveness().to_stream();
    let mut saw_live = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(50), stream.next()).await
            && matches!(s, ProxyState::Live)
        {
            saw_live = true;
            break;
        }
    }
    assert!(saw_live, "proxy never reached Live state");

    // Verify a method call works through the proxy.
    let names: Vec<String> = proxy.call("ListNames", &()).await.expect("ListNames");
    assert!(names.iter().any(|n| n == "org.freedesktop.DBus"));
}

#[tokio::test(flavor = "multi_thread")]
async fn peer_gone_then_back() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();

    // Stand up a peer that owns "mov.vibec0re.test.Vanishable".
    let peer = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("mov.vibec0re.test.Vanishable")
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let proxy = proxy_with(&shared, "mov.vibec0re.test.Vanishable")
        .at_path("/")
        .iface("org.freedesktop.DBus.Peer")
        .build()
        .await
        .expect("build");

    let mut liveness = proxy.liveness().to_stream();
    // Wait for Live.
    while let Ok(Some(s)) = tokio::time::timeout(Duration::from_secs(1), liveness.next()).await {
        if matches!(s, ProxyState::Live) {
            break;
        }
    }

    // Drop the peer.
    drop(peer);

    // Expect PeerGone within 2s.
    let mut saw_peer_gone = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(50), liveness.next()).await
            && matches!(s, ProxyState::PeerGone)
        {
            saw_peer_gone = true;
            break;
        }
    }
    assert!(saw_peer_gone, "expected PeerGone after peer dropped");
}

#[derive(Clone)]
struct Slowpoke;

#[zbus::interface(name = "mov.vibec0re.test.Slowpoke")]
impl Slowpoke {
    /// Deliberately never replies within a normal timeout.
    #[allow(clippy::unused_self)]
    async fn slow(&self) -> String {
        tokio::time::sleep(Duration::from_secs(3)).await;
        "eventually".to_string()
    }

    #[allow(clippy::unused_self)]
    fn fast(&self) -> String {
        "quick".to_string()
    }
}

/// A `BusProxy::call` to a wedged peer must return a (permanent) timeout error
/// bounded by the proxy's timeout, not hang the caller's task forever.
#[tokio::test(flavor = "multi_thread")]
async fn call_times_out_on_slow_peer() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();

    let _peer = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("mov.vibec0re.test.Slowpoke")
        .unwrap()
        .serve_at("/", Slowpoke)
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let proxy = proxy_with(&shared, "mov.vibec0re.test.Slowpoke")
        .at_path("/")
        .iface("mov.vibec0re.test.Slowpoke")
        .timeout(Duration::from_millis(150))
        .build()
        .await
        .expect("build proxy");

    // Wait for Live.
    let mut liveness = proxy.liveness().to_stream();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(ProxyState::Live)) =
            tokio::time::timeout(Duration::from_millis(50), liveness.next()).await
        {
            break;
        }
    }

    // A fast method completes well within the 150 ms timeout.
    let fast: Result<String, _> = proxy.call("Fast", &()).await;
    assert_eq!(fast.expect("fast call should succeed"), "quick");

    // The slow method (3 s) exceeds the timeout: the call must RETURN a
    // permanent timeout error. Bound the whole thing so a regression (no
    // timeout) fails the test rather than hanging it.
    let slow: Result<String, hytte_bus::BusError> =
        tokio::time::timeout(Duration::from_secs(2), proxy.call("Slow", &()))
            .await
            .expect("BusProxy::call must return within its own timeout, not hang");
    let err = slow.expect_err("slow call should have timed out");
    assert!(
        !err.is_transient(),
        "a call timeout is a permanent error, got {err:?}"
    );
}
