mod common;

use common::ephemeral_bus;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{proxy_with, ProxyState};
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
        if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(50), stream.next()).await {
            if matches!(s, ProxyState::Live) {
                saw_live = true;
                break;
            }
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

    // Stand up a peer that owns "cc.hannig.test.Vanishable".
    let peer = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("cc.hannig.test.Vanishable")
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let proxy = proxy_with(&shared, "cc.hannig.test.Vanishable")
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
        if let Ok(Some(s)) =
            tokio::time::timeout(Duration::from_millis(50), liveness.next()).await
        {
            if matches!(s, ProxyState::PeerGone) {
                saw_peer_gone = true;
                break;
            }
        }
    }
    assert!(saw_peer_gone, "expected PeerGone after peer dropped");
}
