#![cfg(feature = "system-tests")]

//! System tests for [`hytte_bus::CallBuilder::call_fd`] — the fd-leasing call.
//!
//! These spawn an ephemeral `dbus-daemon` (one per test) and register a tiny
//! mock interface that hands back one end of a `socketpair` as a `UNIX_FD`. We
//! never touch real `login1` — logind is not on the ephemeral broker — so the
//! mock stands in for `org.freedesktop.login1.Manager.Inhibit`. The
//! real-logind path is **live-verify only**.

mod common;

use common::ephemeral_bus;
use hytte_bus::call_with;
use hytte_bus::test_support::SharedConnection;
use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::connection::Builder;

/// Where the mock parks the *peer* (`a`) end of the socketpair it created, so
/// the test can drive/observe it. The mock returns the other end (`b`) as the
/// `UNIX_FD` reply.
type PeerSlot = Arc<Mutex<Option<UnixStream>>>;

/// Sentinel byte the peer writes so the client can prove the leased fd is the
/// live, independent read end.
const SENTINEL: u8 = 0x42;

struct FdVendor {
    peer: PeerSlot,
}

#[zbus::interface(name = "cc.hannig.test.FdVendor")]
impl FdVendor {
    /// Create a fresh `socketpair`, stash the peer end for the test to use, and
    /// hand back the other end as a `UNIX_FD` — the same shape as logind's
    /// `Inhibit` reply (a single `h`).
    fn get_fd(&self) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
        let (peer, give) =
            UnixStream::pair().map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        *self.peer.lock().expect("peer slot poisoned") = Some(peer);
        Ok(std::os::fd::OwnedFd::from(give).into())
    }
}

/// Build a second connection to the ephemeral bus that owns the well-known name
/// the client will call, with the mock interface mounted.
async fn serve_vendor(address: &str, peer: PeerSlot) -> zbus::Connection {
    Builder::address(address)
        .expect("parse ephemeral bus address")
        .name("cc.hannig.test.fdvendor")
        .expect("request well-known name")
        .serve_at("/cc/hannig/test/FdVendor", FdVendor { peer })
        .expect("mount FdVendor")
        .build()
        .await
        .expect("build vendor connection")
}

/// Invoke the mock through the new `call_fd()` helper.
async fn lease_fd(shared: &SharedConnection) -> hytte_bus::FdLease {
    call_with(shared, "cc.hannig.test.fdvendor")
        .at_path("/cc/hannig/test/FdVendor")
        .iface("cc.hannig.test.FdVendor")
        .method("GetFd")
        .args(())
        .call_fd()
        .await
        .expect("call_fd")
}

/// The leased fd must outlive the reply message it arrived on: if it were a mere
/// borrow of the message's fd it would be dead by the time `call_fd` returns.
/// We prove it's a live, independent fd by exchanging a byte with the peer end
/// the mock retained.
#[tokio::test(flavor = "multi_thread")]
async fn call_fd_returns_a_live_independent_fd() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let peer: PeerSlot = Arc::new(Mutex::new(None));
    let _server = serve_vendor(&address, peer.clone()).await;

    let lease = lease_fd(&shared).await;

    // The reply message (and the fd it carried) has been dropped inside zbus by
    // now. Write a sentinel from the retained peer end, then read it back
    // through the leased fd — proving the lease holds an independent, open dup.
    {
        let mut a = peer.lock().unwrap().take().expect("mock stashed peer end");
        a.write_all(&[SENTINEL]).expect("write sentinel to peer");
        // keep `a` alive until after we read
        let mut client = UnixStream::from(lease.into_inner());
        let mut buf = [0u8; 1];
        client
            .read_exact(&mut buf)
            .expect("read sentinel from leased fd");
        assert_eq!(buf[0], SENTINEL, "leased fd did not carry the peer's byte");
    }
}

/// Dropping the `FdLease` must close the fd — for a logind inhibitor that *is*
/// the release. We observe it from the peer: while the lease is alive the
/// socket peer is open (a non-blocking read yields `WouldBlock`); once the lease
/// drops, the last copy of the `b` end is gone and the peer sees EOF.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_lease_closes_the_fd() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();
    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let peer: PeerSlot = Arc::new(Mutex::new(None));
    let _server = serve_vendor(&address, peer.clone()).await;

    let lease = lease_fd(&shared).await;

    let mut a = peer.lock().unwrap().take().expect("mock stashed peer end");
    a.set_nonblocking(true).expect("set peer non-blocking");

    // Lease alive → peer open, no pending data → WouldBlock.
    let mut buf = [0u8; 1];
    match a.read(&mut buf) {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("expected WouldBlock while the lease is alive, got {other:?}"),
    }

    // Release the lease: the only surviving copy of the `b` end closes.
    drop(lease);

    // The peer should now observe EOF (read returns Ok(0)). Allow a brief window
    // for the close to propagate across the socketpair.
    let mut saw_eof = false;
    for _ in 0..100 {
        match a.read(&mut buf) {
            Ok(0) => {
                saw_eof = true;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            other => panic!("unexpected peer read after dropping the lease: {other:?}"),
        }
    }
    assert!(
        saw_eof,
        "peer never saw EOF — the leased fd was not closed when the FdLease dropped"
    );
}
