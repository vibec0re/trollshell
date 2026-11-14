//! The production `run_wifi_watcher`, through the process-global system
//! connection, against a real `dbus-daemon` and a scripted `net.connman.iwd`
//! (#1647 review: W1 and W2, and W3 for the network read a re-read can add;
//! #1654: W5 for a disconnect, W6 for an iwd or station gone inside a hole).
//!
//! Each test re-execs this test binary with `DBUS_SYSTEM_BUS_ADDRESS` set to
//! a fresh broker, for the reason and in the shape `bluetooth::devices`'
//! `real_bus` module gives (the harness is duplicated per module, the house
//! pattern for broker tests in this crate). Nothing here reaches the host's
//! buses.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p hytte-services --features system-tests --lib wifi::watcher::real_bus
//! ```

use super::{Adapter, IwdPaths, Station, StationState, run_wifi_watcher};
use futures_signals::signal::{Mutable, SignalExt};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// Set on a child to the test it is to run.
const CHILD_ENV: &str = "HYTTE_IWD_REAL_BUS_CHILD";

/// Printed by a child that got to the end of its test.
const CHILD_OK: &str = "HYTTE_IWD_REAL_BUS_OK";

/// Upper bound on any one wait: a liveness guard, not a latency assertion.
const BUDGET: Duration = Duration::from_secs(30);

/// Upper bound on a whole child process.
const CHILD_BUDGET: Duration = Duration::from_mins(2);

type Props = HashMap<String, OwnedValue>;
type Ifaces = HashMap<String, Props>;

fn v<T: Into<zbus::zvariant::Value<'static>>>(x: T) -> OwnedValue {
    x.into().try_to_owned().expect("an owned value")
}

fn clone_props(props: &Props) -> Props {
    props
        .iter()
        .map(|(k, x)| (k.clone(), x.try_clone().expect("a cloneable value")))
        .collect()
}

fn path(p: &str) -> OwnedValue {
    v(zbus::zvariant::ObjectPath::try_from(p.to_owned()).expect("an object path"))
}

// ── The broker and the re-exec ──────────────────────────────────────────────

/// Kills the spawned `dbus-daemon` on drop; the `TempDir` holding its socket
/// outlives it.
struct Broker {
    child: Option<Child>,
    tmp: TempDir,
    address: String,
}

impl Drop for Broker {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            tokio::task::block_in_place(|| {
                let _ = tokio::runtime::Handle::current().block_on(child.wait());
            });
        }
    }
}

/// A fresh `dbus-daemon`, refusing match rules past `match_limit` per
/// connection when given.
async fn broker(match_limit: Option<u32>) -> Broker {
    let tmp = TempDir::new().expect("a tempdir for dbus-daemon");
    let address = format!("unix:path={}", tmp.path().join("bus").display());
    let limit = match_limit.map_or_else(String::new, |n| {
        format!(r#"<limit name="max_match_rules_per_connection">{n}</limit>"#)
    });
    let config = tmp.path().join("bus.conf");
    std::fs::write(
        &config,
        format!(
            r#"<?xml version="1.0"?>
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>{address}</listen>
  <auth>EXTERNAL</auth>
  {limit}
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
        ),
    )
    .expect("write the dbus-daemon config");
    let mut child = Command::new("dbus-daemon")
        .arg("--config-file")
        .arg(&config)
        .arg("--print-address=1")
        .arg("--nofork")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn dbus-daemon (package `dbus`)");
    let stdout = child.stdout.take().expect("dbus-daemon's stdout");
    let printed = tokio::time::timeout(BUDGET, BufReader::new(stdout).lines().next_line())
        .await
        .expect("dbus-daemon printed no address")
        .expect("read dbus-daemon's address")
        .expect("dbus-daemon closed its stdout");
    assert!(printed.contains("unix:path="), "{printed}");
    Broker {
        child: Some(child),
        tmp,
        address,
    }
}

/// This test's name as the harness filters it (`--exact`).
fn test_name(test: &str) -> String {
    let module = module_path!();
    let module = module.split_once("::").map_or(module, |(_, rest)| rest);
    format!("{module}::{test}")
}

/// The system bus address when this process is the child running `test`;
/// `None` in the parent.
fn in_child(test: &str) -> Option<String> {
    (std::env::var(CHILD_ENV).ok() == Some(test_name(test)))
        .then(|| std::env::var("DBUS_SYSTEM_BUS_ADDRESS").expect("the broker's address"))
}

/// Run `test` in a re-exec'd child whose system bus is a fresh broker.
async fn run_in_child(test: &str, match_limit: Option<u32>) {
    let name = test_name(test);
    let bus = broker(match_limit).await;
    let no_session = format!("unix:path={}", bus.tmp.path().join("no-session").display());
    let out = tokio::time::timeout(
        CHILD_BUDGET,
        Command::new(std::env::current_exe().expect("this test binary's path"))
            .args(["--exact", "--nocapture", "--test-threads=1", &name])
            .env(CHILD_ENV, &name)
            .env("DBUS_SYSTEM_BUS_ADDRESS", &bus.address)
            .env("DBUS_SESSION_BUS_ADDRESS", &no_session)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the child finished")
    .expect("spawn the child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains(&format!("{CHILD_OK} {name}")),
        "{name} failed in the child ({:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        out.status
    );
}

// ── A scripted iwd ──────────────────────────────────────────────────────────

const ADAPTER: &str = "/net/connman/iwd/0";
const STATION: &str = "/net/connman/iwd/0/4";
/// iwd's path for the `psk` network "home" on [`STATION`].
const HOME_NET: &str = "/net/connman/iwd/0/4/686f6d65_psk";

/// What the scripted iwd's station does around the watcher's reads. Each
/// change updates the station first and then emits its `PropertiesChanged`,
/// after the reply it follows and before the fake reads the next call.
#[derive(Default)]
struct Script {
    /// The station's properties when the snapshot is taken.
    station: Props,
    /// Changed right after the (first) snapshot is answered.
    after_snapshot: Props,
    /// Changed right after the first station `GetAll` is answered.
    after_first_read: Props,
    /// Dropped from the station in that same change, and listed in its
    /// `PropertiesChanged` as invalidated: how iwd reports a property its
    /// getter no longer answers, `ConnectedNetwork` after a disconnect.
    invalidated_after_first_read: &'static [&'static str],
    /// The station's networks, as `(path, name)`.
    networks: Vec<(&'static str, &'static str)>,
    /// A `GetOrderedNetworks` the fake holds.
    hold: Option<Hold>,
}

/// The `nth` `GetOrderedNetworks` (from 1): the fake sends on `held`, then
/// answers once `release` fires, so the test can read what the watcher
/// published while that read is out.
struct Hold {
    nth: usize,
    held: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

/// iwd's `GetManagedObjects` answer: the adapter, and the station with
/// `station`'s properties.
fn managed_objects(station: &Props) -> HashMap<OwnedObjectPath, Ifaces> {
    HashMap::from([
        (
            OwnedObjectPath::try_from(ADAPTER).expect("a path"),
            HashMap::from([(
                "net.connman.iwd.Adapter".to_owned(),
                HashMap::from([
                    ("Powered".to_owned(), v(true)),
                    ("Name".to_owned(), v("phy0")),
                ]),
            )]),
        ),
        (
            OwnedObjectPath::try_from(STATION).expect("a path"),
            HashMap::from([
                (
                    "net.connman.iwd.Device".to_owned(),
                    HashMap::from([("Name".to_owned(), v("wlan0"))]),
                ),
                ("net.connman.iwd.Station".to_owned(), clone_props(station)),
            ]),
        ),
    ])
}

/// A change iwd makes to its station: the station first, then the signal.
async fn change_station(
    conn: &zbus::Connection,
    station: &Mutex<Props>,
    changed: Props,
    invalidated: &[&str],
) {
    if changed.is_empty() && invalidated.is_empty() {
        return;
    }
    {
        let mut station = station.lock().expect("station");
        for (k, x) in &changed {
            station.insert(k.clone(), x.try_clone().expect("a cloneable value"));
        }
        for k in invalidated {
            station.remove(*k);
        }
    }
    conn.emit_signal(
        None::<zbus::names::BusName<'_>>,
        STATION,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
        &("net.connman.iwd.Station", changed, invalidated),
    )
    .await
    .expect("emit PropertiesChanged");
}

/// Start the scripted iwd. Returns its connection (dropping it is iwd
/// leaving) and the calls it answered, as `"<member> <path>"`.
async fn fake_iwd(address: &str, script: Script) -> (zbus::Connection, Arc<Mutex<Vec<String>>>) {
    let conn = zbus::connection::Builder::address(address)
        .expect("the broker's address")
        .build()
        .await
        .expect("the scripted iwd connects");
    let mut stream = zbus::MessageStream::from(&conn);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (task_conn, task_calls) = (conn.clone(), calls.clone());
    tokio::spawn(async move {
        let Script {
            station,
            after_snapshot,
            after_first_read,
            invalidated_after_first_read,
            networks,
            mut hold,
        } = script;
        let station = Mutex::new(station);
        let mut after_snapshot = Some(after_snapshot);
        let mut after_first_read = Some(after_first_read);
        let mut network_reads = 0_usize;
        while let Some(Ok(msg)) = stream.next().await {
            if msg.message_type() != zbus::message::Type::MethodCall {
                continue;
            }
            let hdr = msg.header();
            let at = hdr.path().map(ToString::to_string).unwrap_or_default();
            let member = hdr.member().map(ToString::to_string).unwrap_or_default();
            task_calls
                .lock()
                .expect("calls")
                .push(format!("{member} {at}"));
            match member.as_str() {
                "GetManagedObjects" => {
                    let reply = managed_objects(&station.lock().expect("station"));
                    let _ = task_conn.reply(&hdr, &reply).await;
                    if let Some(changed) = after_snapshot.take() {
                        change_station(&task_conn, &station, changed, &[]).await;
                    }
                }
                "GetAll" if at == STATION => {
                    let props = clone_props(&station.lock().expect("station"));
                    let _ = task_conn.reply(&hdr, &props).await;
                    if let Some(changed) = after_first_read.take() {
                        change_station(&task_conn, &station, changed, invalidated_after_first_read)
                            .await;
                    }
                }
                "GetAll" => {
                    let name = networks
                        .iter()
                        .find(|(p, _)| *p == at)
                        .map_or("", |(_, name)| *name);
                    let props: Props = HashMap::from([
                        ("Name".to_owned(), v(name.to_owned())),
                        ("Type".to_owned(), v("psk")),
                    ]);
                    let _ = task_conn.reply(&hdr, &props).await;
                }
                "GetOrderedNetworks" => {
                    network_reads += 1;
                    if hold.as_ref().is_some_and(|hold| hold.nth == network_reads)
                        && let Some(Hold { held, release, .. }) = hold.take()
                    {
                        let _ = held.send(());
                        let _ = release.await;
                    }
                    let ordered: Vec<(OwnedObjectPath, i16)> = networks
                        .iter()
                        .map(|(p, _)| (OwnedObjectPath::try_from(*p).expect("a path"), -5000))
                        .collect();
                    let _ = task_conn.reply(&hdr, &ordered).await;
                }
                "RegisterAgent" => {
                    let _ = task_conn.reply(&hdr, &()).await;
                }
                _ => {
                    let _ = task_conn
                        .reply_error(&hdr, "org.freedesktop.DBus.Error.UnknownMethod", &"no")
                        .await;
                }
            }
        }
    });
    conn.request_name("net.connman.iwd")
        .await
        .expect("own net.connman.iwd");
    (conn, calls)
}

fn station_props(state: &str) -> Props {
    HashMap::from([
        ("State".to_owned(), v(state.to_owned())),
        ("Scanning".to_owned(), v(false)),
    ])
}

fn state_change(state: &str) -> Props {
    HashMap::from([("State".to_owned(), v(state.to_owned()))])
}

/// The watcher's station and adapter, and the task running it.
struct Watcher {
    station: Mutable<Option<Station>>,
    adapter: Mutable<Option<Adapter>>,
    task: tokio::task::JoinHandle<()>,
}

fn spawn_watcher() -> Watcher {
    let station = Mutable::new(None);
    let adapter = Mutable::new(None);
    let task = tokio::spawn(run_wifi_watcher(
        station.clone(),
        Mutable::new(Vec::new()),
        Mutable::new(None),
        adapter.clone(),
        IwdPaths::default(),
    ));
    Watcher {
        station,
        adapter,
        task,
    }
}

/// Wait until the station satisfies `done`; the last station seen if it
/// never does.
async fn wait_station(
    station: &Mutable<Option<Station>>,
    done: impl Fn(&Station) -> bool,
) -> Result<(), Option<Station>> {
    let mut published = station.signal_cloned().to_stream();
    let mut last = None;
    tokio::time::timeout(BUDGET, async {
        while let Some(st) = published.next().await {
            if st.as_ref().is_some_and(&done) {
                return;
            }
            last = st;
        }
    })
    .await
    .map_err(|_| last)
}

/// Wait until the scripted iwd was called with every one of `members`.
async fn wait_calls(calls: &Mutex<Vec<String>>, members: &[&str]) -> Result<(), Vec<String>> {
    let seen = |calls: &Mutex<Vec<String>>| {
        let calls = calls.lock().expect("calls");
        members
            .iter()
            .all(|m| calls.iter().any(|c| c.starts_with(&format!("{m} "))))
    };
    tokio::time::timeout(BUDGET, async {
        // Polled: the fake has no notifier, and a call is a message away.
        while !seen(calls) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| calls.lock().expect("calls").clone())
}

// ── W1: a station change after the re-read ──────────────────────────────────

/// iwd's station moves on right after each read of it: the snapshot shows
/// `disconnected` and is followed by `connecting` (made before the
/// station's rule can be in place, so lost), and the first station `GetAll`
/// shows `connecting` and is followed by `connected`. The watcher must end
/// on `connected` (#1647 review W1): its re-read is sent once the station's
/// rule is in place, so the change after it is delivered. Before #1551's
/// follow-up nothing read the station after the snapshot, and the panel
/// stayed `disconnected`.
///
/// **Falsification:** read the station before subscribing to it, drop
/// `refresh_station` from the re-read, subscribe the station at the
/// adapter's path, or hand the pump a stream other than the station's, and
/// the panel never shows `connected`.
#[tokio::test(flavor = "multi_thread")]
async fn w1_a_station_change_after_the_re_read_reaches_the_panel() {
    const TEST: &str = "w1_a_station_change_after_the_re_read_reaches_the_panel";
    let Some(address) = in_child(TEST) else {
        return run_in_child(TEST, None).await;
    };
    let (_iwd, calls) = fake_iwd(
        &address,
        Script {
            station: station_props("disconnected"),
            after_snapshot: state_change("connecting"),
            after_first_read: state_change("connected"),
            networks: Vec::new(),
            ..Script::default()
        },
    )
    .await;
    let watcher = spawn_watcher();

    let reached = wait_station(&watcher.station, |st| st.state == StationState::Connected).await;
    if let Err(last) = reached {
        panic!(
            "the panel stayed at {:?}; iwd is connected. Calls: {:?}",
            last.map(|st| st.state),
            calls.lock().expect("calls")
        );
    }
    watcher.task.abort();
    println!("{CHILD_OK} {}", test_name(TEST));
}

// ── W2: a refused station rule ──────────────────────────────────────────────

/// A system bus that refuses the station's `AddMatch` (here dbus-daemon's
/// per-connection limit at 4: the watcher's three discovery rules and the
/// connection's own fit, the station's does not) costs the station's live
/// updates only. The network list is still read and the agent still
/// registered, so iwd can ask for a passphrase (#1647 review W2). Until the
/// review the station's rule was waited for before both, so the watcher sat
/// with neither, and iwd answered `NoAgent` for every network that needs a
/// passphrase.
///
/// The premise is checked too: with the station's rule refused, the station
/// is never read again, since that read waits for the rule. If it is read,
/// the limit no longer refuses the station's rule and the test says so.
///
/// **Falsification:** wait for the station's rule before the pump (the
/// #1647 head) and neither call is made.
#[tokio::test(flavor = "multi_thread")]
async fn w2_a_refused_station_rule_costs_only_the_stations_updates() {
    const TEST: &str = "w2_a_refused_station_rule_costs_only_the_stations_updates";
    let Some(address) = in_child(TEST) else {
        return run_in_child(TEST, Some(4)).await;
    };
    let (_iwd, calls) = fake_iwd(
        &address,
        Script {
            station: station_props("disconnected"),
            after_snapshot: Props::new(),
            after_first_read: Props::new(),
            networks: Vec::new(),
            ..Script::default()
        },
    )
    .await;
    let watcher = spawn_watcher();

    let made = wait_calls(&calls, &["GetOrderedNetworks", "RegisterAgent"]).await;
    assert!(
        made.is_ok(),
        "the networks were not read or the agent not registered. Calls: {made:?}"
    );
    // A bounded negative, for the premise: a station re-read would follow
    // its rule within a round trip or two.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let calls = calls.lock().expect("calls").clone();
    assert!(
        !calls.contains(&format!("GetAll {STATION}")),
        "the station was read again, so its rule was installed: this limit no longer \
         refuses it. Calls: {calls:?}"
    );
    watcher.task.abort();
    println!("{CHILD_OK} {}", test_name(TEST));
}

// ── W3: the networks follow a re-read that found a change ───────────────────

/// The station joins another network between the snapshot and the
/// station's rule (a change that is lost). Its re-read finds the new
/// network, and since the station changed, the network list is read again,
/// so the panel names the network the station is on now
/// (`reread_station`).
///
/// While that network read is out the panel names no network at all: the
/// re-read keeps the SSID only on the same network (#1654). It used to keep
/// "home" for `WORK` until the read landed.
///
/// **Falsification:** skip the network read after a re-read that found a
/// change and the fake never holds a second read; skip the re-read and the
/// panel shows the old network too; keep the SSID across the re-read's
/// network change (`refresh_station`'s pre-#1654 field update) and the held
/// read sees "home".
#[tokio::test(flavor = "multi_thread")]
async fn w3_a_re_read_that_finds_a_change_reads_the_networks() {
    const TEST: &str = "w3_a_re_read_that_finds_a_change_reads_the_networks";
    const HOME: &str = "/net/connman/iwd/0/4/686f6d65_psk";
    const WORK: &str = "/net/connman/iwd/0/4/776f726b_psk";
    let Some(address) = in_child(TEST) else {
        return run_in_child(TEST, None).await;
    };
    let mut station = station_props("connected");
    station.insert("ConnectedNetwork".to_owned(), path(HOME));
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let (_iwd, calls) = fake_iwd(
        &address,
        Script {
            station,
            after_snapshot: HashMap::from([("ConnectedNetwork".to_owned(), path(WORK))]),
            networks: vec![(HOME, "home"), (WORK, "work")],
            // The first read follows the snapshot; the second, the re-read.
            hold: Some(Hold {
                nth: 2,
                held: held_tx,
                release: release_rx,
            }),
            ..Script::default()
        },
    )
    .await;
    let watcher = spawn_watcher();

    tokio::time::timeout(BUDGET, held_rx)
        .await
        .expect("the re-read found the change and read the networks")
        .expect("the fake held that read");
    let st = watcher.station.get_cloned();
    let _ = release_tx.send(());
    assert_eq!(
        st.as_ref().map(|st| (
            st.connected_network.as_deref(),
            st.connected_ssid.as_deref()
        )),
        Some((Some(WORK), None)),
        "the re-read published {st:?} before its network read"
    );

    let reached = wait_station(&watcher.station, |st| {
        st.connected_network.as_deref() == Some(WORK)
            && st.connected_ssid.as_deref() == Some("work")
    })
    .await;
    if let Err(last) = reached {
        panic!(
            "the panel stayed at {:?}; iwd is on \"work\". Calls: {:?}",
            last.map(|st| (st.connected_network, st.connected_ssid)),
            calls.lock().expect("calls")
        );
    }
    watcher.task.abort();
    println!("{CHILD_OK} {}", test_name(TEST));
}

// ── W4: the SSID crosses the pass boundary ──────────────────────────────────

/// The network W4's station stays on.
const W4_HOME: &str = "/net/connman/iwd/0/4/686f6d65_psk";

/// More `InterfacesAdded` than hytte-bus's broadcast holds (64), so the
/// pump's receiver for them reports `Lagged`.
const W4_BURST: usize = 200;

/// Emit [`W4_BURST`] `InterfacesAdded` without reading a call in between, so
/// a pump waiting on this fake's reply cannot drain them.
async fn w4_burst(conn: &zbus::Connection) {
    let added = OwnedObjectPath::try_from("/net/connman/iwd/0/4/6e6577_psk").expect("a path");
    for _ in 0..W4_BURST {
        let ifaces: HashMap<String, Props> = HashMap::new();
        let _ = conn
            .emit_signal(
                None::<zbus::names::BusName<'_>>,
                "/",
                "org.freedesktop.DBus.ObjectManager",
                "InterfacesAdded",
                &(added.clone(), ifaces),
            )
            .await;
    }
}

/// W4's scripted iwd: connected on [`W4_HOME`] ("home") throughout. It
/// answers the first `RegisterAgent` with a burst ([`w4_burst`]), and it holds
/// the first `GetOrderedNetworks` after the second snapshot: it sends `held`,
/// then answers once `release` fires. `stream` is made before the fake owns
/// iwd's name, so no call can arrive before it.
async fn w4_fake(
    conn: zbus::Connection,
    mut stream: zbus::MessageStream,
    held: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
) {
    let mut station = station_props("connected");
    station.insert("ConnectedNetwork".to_owned(), path(W4_HOME));
    let mut snapshots = 0_u32;
    let mut burst_sent = false;
    let mut to_hold = Some((held, release));
    while let Some(Ok(msg)) = stream.next().await {
        if msg.message_type() != zbus::message::Type::MethodCall {
            continue;
        }
        let hdr = msg.header();
        let at = hdr.path().map(ToString::to_string).unwrap_or_default();
        let member = hdr.member().map(ToString::to_string).unwrap_or_default();
        match member.as_str() {
            "GetManagedObjects" => {
                snapshots += 1;
                let _ = conn.reply(&hdr, &managed_objects(&station)).await;
            }
            "GetAll" if at == STATION => {
                let _ = conn.reply(&hdr, &clone_props(&station)).await;
            }
            "GetAll" => {
                let props: Props = HashMap::from([
                    ("Name".to_owned(), v("home".to_owned())),
                    ("Type".to_owned(), v("psk")),
                ]);
                let _ = conn.reply(&hdr, &props).await;
            }
            "GetOrderedNetworks" => {
                if snapshots == 2
                    && let Some((held, release)) = to_hold.take()
                {
                    let _ = held.send(());
                    let _ = release.await;
                }
                let ordered: Vec<(OwnedObjectPath, i16)> =
                    vec![(OwnedObjectPath::try_from(W4_HOME).expect("a path"), -5000)];
                let _ = conn.reply(&hdr, &ordered).await;
            }
            "RegisterAgent" => {
                let _ = conn.reply(&hdr, &()).await;
                if !std::mem::replace(&mut burst_sent, true) {
                    w4_burst(&conn).await;
                }
            }
            _ => {
                let _ = conn
                    .reply_error(&hdr, "org.freedesktop.DBus.Error.UnknownMethod", &"no")
                    .await;
            }
        }
    }
}

/// The station stays on "home" while a hole sends the watcher back to
/// discovery. The second pass's snapshot names the same network, so the
/// station that pass publishes keeps "home" while its own network read is
/// still out (#1551). The hole is a `Lagged` on the `InterfacesAdded`
/// receiver ([`w4_burst`]), sent while the pump waits on the fake's reply.
///
/// **Falsification:** clear the station anywhere between the pump's return
/// and `refresh_station_from_managed`, or publish the parsed station without
/// `with_kept_ssid`, and the held read sees no SSID.
#[tokio::test(flavor = "multi_thread")]
async fn w4_a_rediscovery_keeps_the_ssid_while_its_network_read_is_out() {
    const TEST: &str = "w4_a_rediscovery_keeps_the_ssid_while_its_network_read_is_out";
    let Some(address) = in_child(TEST) else {
        return run_in_child(TEST, None).await;
    };
    let conn = zbus::connection::Builder::address(address.as_str())
        .expect("the broker's address")
        .build()
        .await
        .expect("the scripted iwd connects");
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let stream = zbus::MessageStream::from(&conn);
    let fake = tokio::spawn(w4_fake(conn.clone(), stream, held_tx, release_rx));
    conn.request_name("net.connman.iwd")
        .await
        .expect("own net.connman.iwd");
    let watcher = spawn_watcher();

    tokio::time::timeout(BUDGET, held_rx)
        .await
        .expect("a second pass read the networks: the burst made a hole")
        .expect("the fake held that read");
    let st = watcher.station.get_cloned();
    let _ = release_tx.send(());
    assert_eq!(
        st.as_ref().and_then(|st| st.connected_ssid.as_deref()),
        Some("home"),
        "the second pass published {st:?} before its network read"
    );
    watcher.task.abort();
    fake.abort();
    println!("{CHILD_OK} {}", test_name(TEST));
}

// ── W5: a disconnect clears the network ─────────────────────────────────────

/// iwd disconnects the station the way it does on a real disconnect: one
/// `PropertiesChanged` with `State` changed to `disconnected` and
/// `ConnectedNetwork` (and `ConnectedAccessPoint`) in its invalidated list,
/// since that getter answers nothing once the network is gone (iwd 3.12
/// `src/station.c:1988-1991`, ell `dbus-service.c:985-994`). The panel must
/// lose the network and its name (#1654). It used to keep both, and the old
/// network's row its "Connected" mark. The disconnect is sent right after the
/// station's re-read, so the station's rule is in place.
///
/// **Falsification:** read only the changed dict (drop the invalidated list
/// in `handle_station_props_event` or in `apply_station_props_delta`) and
/// the station keeps `HOME_NET` and "home".
#[tokio::test(flavor = "multi_thread")]
async fn w5_a_disconnect_clears_the_network_and_its_name() {
    const TEST: &str = "w5_a_disconnect_clears_the_network_and_its_name";
    let Some(address) = in_child(TEST) else {
        return run_in_child(TEST, None).await;
    };
    let mut station = station_props("connected");
    station.insert("ConnectedNetwork".to_owned(), path(HOME_NET));
    let (_iwd, calls) = fake_iwd(
        &address,
        Script {
            station,
            after_first_read: state_change("disconnected"),
            invalidated_after_first_read: &["ConnectedNetwork", "ConnectedAccessPoint"],
            networks: vec![(HOME_NET, "home")],
            ..Script::default()
        },
    )
    .await;
    let watcher = spawn_watcher();

    let reached = wait_station(&watcher.station, |st| {
        st.state == StationState::Disconnected
            && st.connected_network.is_none()
            && st.connected_ssid.is_none()
    })
    .await;
    if let Err(last) = reached {
        panic!(
            "the panel stayed at {:?}; iwd is disconnected. Calls: {:?}",
            last.map(|st| (st.state, st.connected_network, st.connected_ssid)),
            calls.lock().expect("calls")
        );
    }
    watcher.task.abort();
    println!("{CHILD_OK} {}", test_name(TEST));
}

// ── W6: iwd or its station gone inside a hole ───────────────────────────────

/// What W6's iwd does inside the hole that ends the watcher's first pass.
/// Each goes on while the fake holds that pass's network read, so no pump
/// is reading yet, and ends in more signals than hytte-bus's broadcast holds
/// (64), so the pump's first read of that receiver is a `Lagged` and it
/// never reads what went before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum W6Hole {
    /// iwd releases its name, then [`W4_BURST`] `NameOwnerChanged` for
    /// another name overrun the owner receiver.
    IwdLeaves,
    /// iwd removes its station, then [`W4_BURST`] `InterfacesRemoved` for a
    /// network overrun that receiver. iwd keeps its name.
    StationRemoved,
    /// Nothing goes: [`w4_burst`] overruns the `InterfacesAdded` receiver,
    /// and the next pass's snapshot fails.
    SnapshotFails,
}

/// [`W4_BURST`] `NameOwnerChanged` for a name of its own, from a connection
/// with no message stream, so nothing the bus sends it back can fill a queue
/// (zbus drops what no stream reads).
async fn w6_owner_flood(flood: &zbus::Connection) {
    const NAME: &str = "org.example.Flood";
    const BUS: Option<&str> = Some("org.freedesktop.DBus");
    for _ in 0..W4_BURST / 2 {
        flood
            .call_method(
                BUS,
                "/org/freedesktop/DBus",
                BUS,
                "RequestName",
                &(NAME, 0_u32),
            )
            .await
            .expect("RequestName");
        flood
            .call_method(BUS, "/org/freedesktop/DBus", BUS, "ReleaseName", &(NAME,))
            .await
            .expect("ReleaseName");
    }
}

/// The station's removal, then [`W4_BURST`] removals of a network.
async fn w6_remove_station(conn: &zbus::Connection) {
    let removed = |p: &str, iface: &str| {
        (
            OwnedObjectPath::try_from(p.to_owned()).expect("a path"),
            vec![iface.to_owned()],
        )
    };
    let station = removed(STATION, "net.connman.iwd.Station");
    let network = removed("/net/connman/iwd/0/4/6e6577_psk", "net.connman.iwd.Network");
    for body in std::iter::once(&station).chain(std::iter::repeat_n(&network, W4_BURST)) {
        let _ = conn
            .emit_signal(
                None::<zbus::names::BusName<'_>>,
                "/",
                "org.freedesktop.DBus.ObjectManager",
                "InterfacesRemoved",
                body,
            )
            .await;
    }
}

/// A one-shot hold: the fake sends on the first, then waits for the second.
type W6Hold = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

/// W6's scripted iwd: connected on [`HOME_NET`] ("home"). At the first
/// `GetOrderedNetworks` (the first pass's network read, before its pump
/// starts) it sends `hole_made` and makes `hole`, then answers. With
/// [`W6Hole::SnapshotFails`] it answers the second snapshot with an error
/// and holds the third (`third_snapshot`).
struct W6Fake {
    conn: zbus::Connection,
    stream: zbus::MessageStream,
    flood: zbus::Connection,
    hole: W6Hole,
    hole_made: tokio::sync::oneshot::Sender<()>,
    third_snapshot: Option<W6Hold>,
}

async fn w6_fake(fake: W6Fake) {
    let W6Fake {
        conn,
        mut stream,
        flood,
        hole,
        hole_made,
        mut third_snapshot,
    } = fake;
    let mut station = station_props("connected");
    station.insert("ConnectedNetwork".to_owned(), path(HOME_NET));
    let mut station_gone = false;
    let mut snapshots = 0_u32;
    let mut hole_made = Some(hole_made);
    while let Some(Ok(msg)) = stream.next().await {
        if msg.message_type() != zbus::message::Type::MethodCall {
            continue;
        }
        let hdr = msg.header();
        let at = hdr.path().map(ToString::to_string).unwrap_or_default();
        let member = hdr.member().map(ToString::to_string).unwrap_or_default();
        match member.as_str() {
            "GetManagedObjects" => {
                snapshots += 1;
                if hole == W6Hole::SnapshotFails && snapshots == 2 {
                    let _ = conn
                        .reply_error(&hdr, "org.freedesktop.DBus.Error.Failed", &"scripted")
                        .await;
                    continue;
                }
                if snapshots == 3
                    && let Some((held, release)) = third_snapshot.take()
                {
                    let _ = held.send(());
                    let _ = release.await;
                }
                let mut managed = managed_objects(&station);
                if station_gone {
                    managed.retain(|p, _| p.as_str() != STATION);
                }
                let _ = conn.reply(&hdr, &managed).await;
            }
            "GetAll" if at == STATION && station_gone => {
                let _ = conn
                    .reply_error(&hdr, "org.freedesktop.DBus.Error.UnknownObject", &"gone")
                    .await;
            }
            "GetAll" if at == STATION => {
                let _ = conn.reply(&hdr, &clone_props(&station)).await;
            }
            "GetAll" => {
                let props: Props = HashMap::from([
                    ("Name".to_owned(), v("home".to_owned())),
                    ("Type".to_owned(), v("psk")),
                ]);
                let _ = conn.reply(&hdr, &props).await;
            }
            "GetOrderedNetworks" => {
                if let Some(hole_made) = hole_made.take() {
                    let _ = hole_made.send(());
                    match hole {
                        W6Hole::IwdLeaves => {
                            conn.release_name("net.connman.iwd")
                                .await
                                .expect("release iwd's name");
                            w6_owner_flood(&flood).await;
                        }
                        W6Hole::StationRemoved => {
                            station_gone = true;
                            w6_remove_station(&conn).await;
                        }
                        W6Hole::SnapshotFails => w4_burst(&conn).await,
                    }
                }
                let ordered: Vec<(OwnedObjectPath, i16)> =
                    vec![(OwnedObjectPath::try_from(HOME_NET).expect("a path"), -5000)];
                let _ = conn.reply(&hdr, &ordered).await;
            }
            "RegisterAgent" => {
                let _ = conn.reply(&hdr, &()).await;
            }
            _ => {
                let _ = conn
                    .reply_error(&hdr, "org.freedesktop.DBus.Error.UnknownMethod", &"no")
                    .await;
            }
        }
    }
}

/// Start W6's iwd for `hole` and the watcher, and wait for the hole: the
/// station and adapter the first pass published are on the panel then.
async fn w6_start(
    address: &str,
    hole: W6Hole,
    third_snapshot: Option<W6Hold>,
) -> (Watcher, tokio::task::JoinHandle<()>) {
    let connect = || async {
        zbus::connection::Builder::address(address)
            .expect("the broker's address")
            .build()
            .await
            .expect("connect to the broker")
    };
    let conn = connect().await;
    let flood = connect().await;
    let (hole_tx, hole_rx) = tokio::sync::oneshot::channel::<()>();
    let stream = zbus::MessageStream::from(&conn);
    let fake = tokio::spawn(w6_fake(W6Fake {
        conn: conn.clone(),
        stream,
        flood,
        hole,
        hole_made: hole_tx,
        third_snapshot,
    }));
    conn.request_name("net.connman.iwd")
        .await
        .expect("own net.connman.iwd");
    let watcher = spawn_watcher();

    tokio::time::timeout(BUDGET, hole_rx)
        .await
        .expect("the first pass read the networks")
        .expect("the fake made the hole");
    assert!(
        watcher.station.lock_ref().is_some() && watcher.adapter.lock_ref().is_some(),
        "the first pass published its station and adapter before its network read"
    );
    (watcher, fake)
}

/// Wait until the watcher has cleared its station and adapter. Clear is
/// final in W6: nothing republishes them without iwd or its station.
async fn w6_wait_cleared(watcher: &Watcher) -> Result<(), (Option<Station>, Option<Adapter>)> {
    tokio::time::timeout(BUDGET, async {
        // Polled, as `wait_calls` is: the end state is final here.
        while watcher.station.lock_ref().is_some() || watcher.adapter.lock_ref().is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| (watcher.station.get_cloned(), watcher.adapter.get_cloned()))
}

/// iwd leaves inside the hole that ends a pass, so no pump reads its
/// `NameOwnerChanged`. The next pass finds iwd's name without an owner and
/// must clear the station and adapter before it waits (#1654). It used to
/// wait with both on the panel until iwd came back.
///
/// **Falsification:** never run `wait_for_owner`'s `gone`, or hand
/// `wait_for_iwd` a closure that clears nothing, and the panel keeps both.
#[tokio::test(flavor = "multi_thread")]
async fn w6_an_iwd_that_leaves_inside_a_hole_is_cleared() {
    const TEST: &str = "w6_an_iwd_that_leaves_inside_a_hole_is_cleared";
    let Some(address) = in_child(TEST) else {
        return run_in_child(TEST, None).await;
    };
    let (watcher, fake) = w6_start(&address, W6Hole::IwdLeaves, None).await;
    if let Err(kept) = w6_wait_cleared(&watcher).await {
        panic!("iwd is gone and the panel kept {kept:?}");
    }
    watcher.task.abort();
    fake.abort();
    println!("{CHILD_OK} {}", test_name(TEST));
}

/// iwd removes its station inside the hole that ends a pass, and keeps its
/// name. The next pass's snapshot has no station, and it must clear the
/// station and adapter before it retries (#1654). It used to retry with
/// both on the panel.
///
/// **Falsification:** do not clear on a snapshot with no station and the
/// panel keeps both.
#[tokio::test(flavor = "multi_thread")]
async fn w6_a_station_removed_inside_a_hole_is_cleared() {
    const TEST: &str = "w6_a_station_removed_inside_a_hole_is_cleared";
    let Some(address) = in_child(TEST) else {
        return run_in_child(TEST, None).await;
    };
    let (watcher, fake) = w6_start(&address, W6Hole::StationRemoved, None).await;
    if let Err(kept) = w6_wait_cleared(&watcher).await {
        panic!("the station is gone and the panel kept {kept:?}");
    }
    watcher.task.abort();
    fake.abort();
    println!("{CHILD_OK} {}", test_name(TEST));
}

/// A pass whose snapshot fails learns nothing about iwd, so it keeps what
/// the last pass published, as a hole does, and the pass after it still
/// finds the station there, SSID and all (#1654: only a pass that finds no
/// owner or no station clears).
///
/// **Falsification:** clear on a failed snapshot too and the third snapshot
/// finds the panel empty.
#[tokio::test(flavor = "multi_thread")]
async fn w6_a_failed_snapshot_keeps_the_station() {
    const TEST: &str = "w6_a_failed_snapshot_keeps_the_station";
    let Some(address) = in_child(TEST) else {
        return run_in_child(TEST, None).await;
    };
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let (watcher, fake) =
        w6_start(&address, W6Hole::SnapshotFails, Some((held_tx, release_rx))).await;

    tokio::time::timeout(BUDGET, held_rx)
        .await
        .expect("a third pass took a snapshot")
        .expect("the fake held it");
    let (st, adapter) = (watcher.station.get_cloned(), watcher.adapter.get_cloned());
    let _ = release_tx.send(());
    assert_eq!(
        st.as_ref().map(|st| (
            st.connected_network.as_deref(),
            st.connected_ssid.as_deref()
        )),
        Some((Some(HOME_NET), Some("home"))),
        "a failed snapshot dropped {st:?}"
    );
    assert!(adapter.is_some(), "a failed snapshot dropped the adapter");
    watcher.task.abort();
    fake.abort();
    println!("{CHILD_OK} {}", test_name(TEST));
}
