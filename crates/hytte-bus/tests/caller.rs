#![cfg(feature = "system-tests")]

//! `CallerCheck` against a real `dbus-daemon`: the owner of a name is
//! admitted, anyone else is refused, and the owner is looked up per call, so
//! a daemon that comes back under a new unique name is checked against that
//! one. The check asks on the connection it is handed, which in an interface
//! method is the one the call arrived on (`check_reads_the_sender_off_a_real_call`).

mod common;

use common::{PROBE_BUDGET, ephemeral_bus};
use hytte_bus::{BusError, CallerCheck, CallerRefused, caller_check};
use zbus::Connection;
use zbus::connection::Builder;

/// The well-known name the checks below are for, standing in for
/// `org.freedesktop.NetworkManager` or `org.bluez`.
const DAEMON: &str = "mov.vibec0re.test.Daemon";

/// A peer that owns [`DAEMON`].
async fn daemon(address: &str) -> Connection {
    Builder::address(address)
        .expect("parse ephemeral bus address")
        .name(DAEMON)
        .expect("a valid well-known name")
        .build()
        .await
        .expect("bring the daemon up")
}

/// A peer that owns nothing.
async fn peer(address: &str) -> Connection {
    Builder::address(address)
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("connect a peer")
}

fn unique(conn: &Connection) -> String {
    conn.unique_name()
        .expect("a bus connection has a unique name")
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_is_admitted_and_anyone_else_is_refused() {
    let (conn, guard) = ephemeral_bus().await;
    let check = caller_check(DAEMON);
    let owner = daemon(&guard.address).await;
    let intruder = peer(&guard.address).await;

    check
        .check_sender(&conn, Some(&unique(&owner)))
        .await
        .expect("the owner is admitted");

    match check.check_sender(&conn, Some(&unique(&intruder))).await {
        Err(CallerRefused::NotOwner {
            name,
            sender,
            owner: holder,
        }) => {
            assert_eq!(name, DAEMON);
            assert_eq!(sender, unique(&intruder));
            assert_eq!(holder, unique(&owner));
        }
        other => panic!("expected NotOwner, got {other:?}"),
    }
}

/// The owner is looked up on every call. When the daemon restarts, the old
/// unique name is refused and the new one admitted, with no reconnect or
/// refresh in between.
///
/// **Falsification:** resolve the owner once and keep it (a cached owner)
/// and the new daemon is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_daemon_is_checked_under_its_new_name() {
    let (conn, guard) = ephemeral_bus().await;
    let check = caller_check(DAEMON);

    let first = daemon(&guard.address).await;
    let first_name = unique(&first);
    check
        .check_sender(&conn, Some(&first_name))
        .await
        .expect("the first daemon is admitted");

    first.close().await.expect("the first daemon exits");
    let second = daemon(&guard.address).await;

    check
        .check_sender(&conn, Some(&unique(&second)))
        .await
        .expect("the restarted daemon is admitted under its new name");
    assert!(
        matches!(
            check.check_sender(&conn, Some(&first_name)).await,
            Err(CallerRefused::NotOwner { .. })
        ),
        "the old daemon's name is refused once it no longer owns the name"
    );
}

/// No owner, nobody admitted: the check fails closed, and says why.
#[tokio::test(flavor = "multi_thread")]
async fn a_name_with_no_owner_admits_no_one() {
    let (conn, guard) = ephemeral_bus().await;
    let check = caller_check(DAEMON);
    let someone = peer(&guard.address).await;

    match check.check_sender(&conn, Some(&unique(&someone))).await {
        Err(CallerRefused::OwnerUnknown {
            name,
            error: BusError::Permanent { dbus_name, .. },
        }) => {
            assert_eq!(name, DAEMON);
            assert_eq!(
                dbus_name.as_deref(),
                Some("org.freedesktop.DBus.Error.NameHasNoOwner")
            );
        }
        other => panic!("expected OwnerUnknown(NameHasNoOwner), got {other:?}"),
    }
}

/// An interface guarded the way the agents guard theirs: the connection and
/// the sender both come from the call itself.
#[derive(Clone)]
struct Gate {
    check: CallerCheck,
}

#[zbus::interface(name = "mov.vibec0re.test.Gate")]
impl Gate {
    async fn knock(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.check
            .check(conn, &header)
            .await
            .map_err(|refused| zbus::fdo::Error::AccessDenied(refused.to_string()))
    }
}

/// `check` reads the sender out of a real call's header and asks the bus the
/// call arrived on: the daemon's call is answered, an intruder's is refused.
#[tokio::test(flavor = "multi_thread")]
async fn check_reads_the_sender_off_a_real_call() {
    let (_conn, guard) = ephemeral_bus().await;
    let gate = Gate {
        check: caller_check(DAEMON),
    };
    // Staged on the builder, so the object server dispatches before anything
    // can be read off the socket (see `common::CALL_BUDGET`).
    let server = Builder::address(guard.address.as_str())
        .expect("parse ephemeral bus address")
        .serve_at("/mov/vibec0re/test/Gate", gate)
        .expect("stage the gate")
        .build()
        .await
        .expect("serve the gate");
    let server_name = unique(&server);

    let knock = async |from: &Connection| -> zbus::Result<()> {
        let proxy = zbus::Proxy::new(
            from,
            server_name.as_str(),
            "/mov/vibec0re/test/Gate",
            "mov.vibec0re.test.Gate",
        )
        .await?;
        tokio::time::timeout(PROBE_BUDGET, proxy.call::<_, _, ()>("Knock", &()))
            .await
            .expect("the gate answers")
    };

    let owner = daemon(&guard.address).await;
    knock(&owner).await.expect("the daemon's call is answered");

    let intruder = peer(&guard.address).await;
    match knock(&intruder).await {
        Err(zbus::Error::MethodError(name, _, _)) => {
            assert_eq!(name.as_str(), "org.freedesktop.DBus.Error.AccessDenied");
        }
        other => panic!("expected the intruder to be refused, got {other:?}"),
    }
}
