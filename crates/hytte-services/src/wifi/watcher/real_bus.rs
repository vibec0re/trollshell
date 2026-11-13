//! The production `run_wifi_watcher`, through the process-global system
//! connection, against a real `dbus-daemon` and a scripted `net.connman.iwd`
//! (#1647 review: W1 and W2, and W3 for the network read a re-read can add).
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

use super::{IwdPaths, Station, StationState, run_wifi_watcher};
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

/// What the scripted iwd's station does around the watcher's reads. Each
/// change updates the station first and then emits its `PropertiesChanged`,
/// after the reply it follows and before the fake reads the next call.
struct Script {
    /// The station's properties when the snapshot is taken.
    station: Props,
    /// Changed right after the (first) snapshot is answered.
    after_snapshot: Props,
    /// Changed right after the first station `GetAll` is answered.
    after_first_read: Props,
    /// The station's networks, as `(path, name)`.
    networks: Vec<(&'static str, &'static str)>,
}

/// iwd's `GetManagedObjects` answer: the adapter, and the station with
/// `station`'s properties.
fn managed_objects(station: &Props) -> HashMap<OwnedObjectPath, Ifaces> {
    HashMap::from([
        (
            OwnedObjectPath::try_from(ADAPTER).expect("a path"),
            HashMap::from([(
                "net.connman.iwd.Adapter1".to_owned(),
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
async fn change_station(conn: &zbus::Connection, station: &Mutex<Props>, changed: Props) {
    if changed.is_empty() {
        return;
    }
    {
        let mut station = station.lock().expect("station");
        for (k, x) in &changed {
            station.insert(k.clone(), x.try_clone().expect("a cloneable value"));
        }
    }
    let invalidated: Vec<String> = Vec::new();
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
            networks,
        } = script;
        let station = Mutex::new(station);
        let mut after_snapshot = Some(after_snapshot);
        let mut after_first_read = Some(after_first_read);
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
                        change_station(&task_conn, &station, changed).await;
                    }
                }
                "GetAll" if at == STATION => {
                    let props = clone_props(&station.lock().expect("station"));
                    let _ = task_conn.reply(&hdr, &props).await;
                    if let Some(changed) = after_first_read.take() {
                        change_station(&task_conn, &station, changed).await;
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

/// The watcher's station, and the task running it.
struct Watcher {
    station: Mutable<Option<Station>>,
    task: tokio::task::JoinHandle<()>,
}

fn spawn_watcher() -> Watcher {
    let station = Mutable::new(None);
    let task = tokio::spawn(run_wifi_watcher(
        station.clone(),
        Mutable::new(Vec::new()),
        Mutable::new(None),
        Mutable::new(None),
        IwdPaths::default(),
    ));
    Watcher { station, task }
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
/// **Falsification:** skip the network read after a re-read that found a
/// change and the panel names the old network; skip the re-read and the
/// panel shows the old network too.
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
    let (_iwd, calls) = fake_iwd(
        &address,
        Script {
            station,
            after_snapshot: HashMap::from([("ConnectedNetwork".to_owned(), path(WORK))]),
            after_first_read: Props::new(),
            networks: vec![(HOME, "home"), (WORK, "work")],
        },
    )
    .await;
    let watcher = spawn_watcher();

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
