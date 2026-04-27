mod common;

use common::ephemeral_bus;

#[tokio::test(flavor = "multi_thread")]
async fn ephemeral_bus_round_trip() {
    let (conn, _guard) = ephemeral_bus().await;
    // The DBus daemon's own ListNames method works on any healthy bus.
    let dbus = zbus::fdo::DBusProxy::new(&conn)
        .await
        .expect("DBusProxy on ephemeral bus");
    let names = dbus.list_names().await.expect("ListNames");
    // org.freedesktop.DBus is always present.
    assert!(
        names.iter().any(|n| n.as_str() == "org.freedesktop.DBus"),
        "expected org.freedesktop.DBus in ListNames, got: {names:?}"
    );
}
