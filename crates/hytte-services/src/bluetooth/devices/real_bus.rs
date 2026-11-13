//! The production `listen` → `event_loop` → `fold` → `SystemBluez`, through
//! the process-global system connection, against a real `dbus-daemon` and a
//! scripted `org.bluez` (#1647 review, R1–R3, and from its second round R4d
//! and R5).
//!
//! # Why each test runs in a re-exec'd child
//!
//! `listen` reaches D-Bus only through `hytte-bus`'s pooled system
//! connection, which the supervisor opens from `DBUS_SYSTEM_BUS_ADDRESS`. So
//! each test is its own parent: it spawns one ephemeral `dbus-daemon` and
//! re-execs this test binary, filtered with `--exact` to itself, with that
//! variable (and a marker) set on the **child** through `Command::env`, which
//! needs no `unsafe`. The child sees the marker and does the work. It is
//! `tests/notifications_dbus.rs`'s shape, for the system bus, and it needs a
//! child module because `listen` is private.
//!
//! Nothing here reaches the host's buses: the child's system bus is the
//! parent's broker, and its session bus is a socket path nothing listens on.
//! The broker is configured as a session bus (no policy beyond "allow"), so
//! the scripted `org.bluez` can own its name without root. A test that
//! needs the broker to refuse match rules sets its per-connection limit
//! ([`BrokerConfig`]). A test that needs to see whether anything starts
//! bluetoothd gives the broker an `org.bluez` activation file that counts
//! its starts ([`activations`]): what a real system bus does with a call to
//! `org.bluez` that has no owner.
//!
//! The scripted bluez answers calls off a raw `zbus` connection's
//! `MessageStream` with `reply`, `reply_error` and `emit_signal`, so the
//! order of replies and signals, and the errors, are the test's. A change it
//! makes updates its own state first and then emits the signal, as bluez
//! does.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p hytte-services --features system-tests --lib bluetooth::devices::real_bus
//! ```

use super::{Device, FoldBus, ListenEnd, listen};
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
const CHILD_ENV: &str = "HYTTE_BLUEZ_REAL_BUS_CHILD";

/// Set on a child to the file its broker's `org.bluez` activation appends a
/// line to ([`activations`]).
const ACTIVATIONS_ENV: &str = "HYTTE_BLUEZ_REAL_BUS_ACTIVATIONS";

/// Printed by a child that got to the end of its test, so the parent can
/// tell a pass from a filter that matched nothing.
const CHILD_OK: &str = "HYTTE_BLUEZ_REAL_BUS_OK";

/// Upper bound on any one wait. A liveness guard, not a latency assertion:
/// the same reasoning and number as this crate's other broker tests, which
/// run inside `nix flake check` next to heavier derivations.
const BUDGET: Duration = Duration::from_secs(30);

/// Upper bound on a whole child process, so a hung child fails its parent
/// test by name instead of parking the binary.
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

fn clone_ifaces(ifaces: &Ifaces) -> Ifaces {
    ifaces
        .iter()
        .map(|(k, props)| (k.clone(), clone_props(props)))
        .collect()
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

/// How [`run_in_child`] sets up the child's broker.
#[derive(Clone, Copy, Default)]
struct BrokerConfig {
    /// Refuse match rules past this many per connection.
    match_limit: Option<u32>,
    /// Give `org.bluez` an activation file whose `Exec` appends a line to the
    /// child's [`activations`] file and exits 1: a start the bus counts, and
    /// one that fails, so the name stays unowned. The broker answers the call
    /// that caused it only once that process has exited, so the line is
    /// written by the time the call's answer is in.
    activatable: bool,
}

/// The file a broker's `org.bluez` activation appends to, in its tempdir.
fn activations_file(tmp: &TempDir) -> std::path::PathBuf {
    tmp.path().join("activations")
}

/// A fresh `dbus-daemon`, set up as `config` says.
async fn broker(config: BrokerConfig) -> Broker {
    let tmp = TempDir::new().expect("a tempdir for dbus-daemon");
    let address = format!("unix:path={}", tmp.path().join("bus").display());
    let limit = config.match_limit.map_or_else(String::new, |n| {
        format!(r#"<limit name="max_match_rules_per_connection">{n}</limit>"#)
    });
    let servicedir = if config.activatable {
        let services = tmp.path().join("services");
        std::fs::create_dir(&services).expect("create the service directory");
        std::fs::write(
            services.join("org.bluez.service"),
            format!(
                "[D-BUS Service]\nName=org.bluez\nExec=/bin/sh -c \"echo activated >> {}; exit 1\"\n",
                activations_file(&tmp).display()
            ),
        )
        .expect("write the org.bluez activation file");
        format!("<servicedir>{}</servicedir>", services.display())
    } else {
        String::new()
    };
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
  {servicedir}
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
async fn run_in_child(test: &str, config: BrokerConfig) {
    let name = test_name(test);
    let bus = broker(config).await;
    let no_session = format!("unix:path={}", bus.tmp.path().join("no-session").display());
    let out = tokio::time::timeout(
        CHILD_BUDGET,
        Command::new(std::env::current_exe().expect("this test binary's path"))
            .args(["--exact", "--nocapture", "--test-threads=1", &name])
            .env(CHILD_ENV, &name)
            .env(ACTIVATIONS_ENV, activations_file(&bus.tmp))
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

/// In a child: how many times its broker has started `org.bluez` so far
/// ([`BrokerConfig::activatable`]).
fn activations() -> usize {
    let file = std::env::var(ACTIVATIONS_ENV).expect("the activation file's path");
    std::fs::read_to_string(file).map_or(0, |s| s.lines().count())
}

// ── A scripted org.bluez ────────────────────────────────────────────────────

/// What the scripted bluez holds and has been asked.
#[derive(Default)]
struct FakeState {
    /// Its objects, by path.
    objects: HashMap<String, Ifaces>,
    /// Paths whose `GetAll` is answered with this error instead.
    errors: HashMap<String, &'static str>,
    /// `GetManagedObjects` calls answered.
    managed_served: u32,
    /// `GetAll` calls answered, as `(path, interface)`.
    get_all: Vec<(String, String)>,
    /// Give up `org.bluez` right after answering the `GetAll` that brings
    /// `get_all` to this length, and clear this: bluetoothd answering what
    /// it has queued and then exiting.
    leave_after_get_all: Option<usize>,
}

struct FakeBluez {
    conn: zbus::Connection,
    state: Arc<Mutex<FakeState>>,
    /// Bumped after every answer, so a test can wait on what was asked.
    answered: tokio::sync::watch::Receiver<u32>,
}

async fn fake_bluez(address: &str, objects: HashMap<String, Ifaces>) -> FakeBluez {
    let conn = zbus::connection::Builder::address(address)
        .expect("the broker's address")
        .build()
        .await
        .expect("the scripted bluez connects");
    let mut calls = zbus::MessageStream::from(&conn);
    let state = Arc::new(Mutex::new(FakeState {
        objects,
        ..FakeState::default()
    }));
    let (answered_tx, answered) = tokio::sync::watch::channel(0_u32);
    let (task_conn, task_state) = (conn.clone(), state.clone());
    tokio::spawn(async move {
        while let Some(Ok(msg)) = calls.next().await {
            if msg.message_type() != zbus::message::Type::MethodCall {
                continue;
            }
            let hdr = msg.header();
            let path = hdr.path().map(ToString::to_string).unwrap_or_default();
            let iface = hdr.interface().map(ToString::to_string).unwrap_or_default();
            let member = hdr.member().map(ToString::to_string).unwrap_or_default();
            match (iface.as_str(), member.as_str()) {
                ("org.freedesktop.DBus.ObjectManager", "GetManagedObjects") => {
                    let reply: HashMap<OwnedObjectPath, Ifaces> = {
                        let mut st = task_state.lock().expect("fake state");
                        st.managed_served += 1;
                        st.objects
                            .iter()
                            .map(|(p, i)| {
                                (
                                    OwnedObjectPath::try_from(p.as_str()).expect("a path"),
                                    clone_ifaces(i),
                                )
                            })
                            .collect()
                    };
                    let _ = task_conn.reply(&hdr, &reply).await;
                }
                ("org.freedesktop.DBus.Properties", "GetAll") => {
                    let wanted: String = msg.body().deserialize().unwrap_or_default();
                    let (answer, leave) = {
                        let mut st = task_state.lock().expect("fake state");
                        st.get_all.push((path.clone(), wanted.clone()));
                        let leave = st.leave_after_get_all == Some(st.get_all.len());
                        if leave {
                            st.leave_after_get_all = None;
                        }
                        let answer = if let Some(error) = st.errors.get(&path) {
                            Err((*error, "scripted".to_owned()))
                        } else {
                            match st.objects.get(&path) {
                                // libdbus's answer for a path with no object.
                                None => Err((
                                    "org.freedesktop.DBus.Error.UnknownObject",
                                    format!("no object at {path}"),
                                )),
                                // gdbus's answer for an interface the object
                                // lacks.
                                Some(ifaces) => ifaces.get(&wanted).map(clone_props).ok_or((
                                    "org.freedesktop.DBus.Error.InvalidArgs",
                                    format!("No such interface '{wanted}'"),
                                )),
                            }
                        };
                        (answer, leave)
                    };
                    let _ = match answer {
                        Ok(props) => task_conn.reply(&hdr, &props).await,
                        Err((name, detail)) => task_conn.reply_error(&hdr, name, &detail).await,
                    };
                    if leave {
                        let _ = task_conn.release_name("org.bluez").await;
                    }
                }
                _ => {
                    let _ = task_conn
                        .reply_error(&hdr, "org.freedesktop.DBus.Error.UnknownMethod", &"no")
                        .await;
                }
            }
            answered_tx.send_modify(|n| *n += 1);
        }
    });
    conn.request_name("org.bluez").await.expect("own org.bluez");
    FakeBluez {
        conn,
        state,
        answered,
    }
}

impl FakeBluez {
    /// Wait until what bluez has answered satisfies `done`.
    async fn wait_for(&self, what: &str, done: impl Fn(&FakeState) -> bool) {
        let mut answered = self.answered.clone();
        let waited = tokio::time::timeout(BUDGET, async {
            loop {
                if done(&self.state.lock().expect("fake state")) {
                    return;
                }
                answered
                    .changed()
                    .await
                    .expect("the scripted bluez is alive");
            }
        })
        .await;
        let st = self.state.lock().expect("fake state");
        assert!(
            waited.is_ok(),
            "bluez never saw {what}: GetManagedObjects answered {}×, GetAll calls {:?}",
            st.managed_served,
            st.get_all
        );
    }

    fn get_all_served(&self, path: &str) -> bool {
        self.state
            .lock()
            .expect("fake state")
            .get_all
            .iter()
            .any(|(p, _)| p == path)
    }

    async fn emit_added(&self, path: &str, ifaces: &Ifaces) {
        let path = zbus::zvariant::ObjectPath::try_from(path).expect("a path");
        self.conn
            .emit_signal(
                None::<zbus::names::BusName<'_>>,
                "/",
                "org.freedesktop.DBus.ObjectManager",
                "InterfacesAdded",
                &(path, ifaces),
            )
            .await
            .expect("emit InterfacesAdded");
    }

    /// A change bluez makes to a device: its state first, then the signal.
    async fn change_device(&self, path: &str, changed: &Props) {
        if let Some(device) = self
            .state
            .lock()
            .expect("fake state")
            .objects
            .get_mut(path)
            .and_then(|ifaces| ifaces.get_mut("org.bluez.Device1"))
        {
            for (k, x) in changed {
                device.insert(k.clone(), x.try_clone().expect("a cloneable value"));
            }
        }
        let invalidated: Vec<String> = Vec::new();
        self.conn
            .emit_signal(
                None::<zbus::names::BusName<'_>>,
                path,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &("org.bluez.Device1", changed, invalidated),
            )
            .await
            .expect("emit PropertiesChanged");
    }
}

const ADAPTER: &str = "/org/bluez/hci0";

fn adapter() -> Ifaces {
    HashMap::from([(
        "org.bluez.Adapter1".to_owned(),
        HashMap::from([
            ("Address".to_owned(), v("00:00:00:00:00:01")),
            ("Name".to_owned(), v("hci0")),
            ("Powered".to_owned(), v(true)),
            ("Discoverable".to_owned(), v(false)),
            ("Discovering".to_owned(), v(true)),
        ]),
    )])
}

fn device(address: &str, alias: &str, connected: bool, battery: Option<u8>) -> Ifaces {
    let mut ifaces = HashMap::from([(
        "org.bluez.Device1".to_owned(),
        HashMap::from([
            ("Address".to_owned(), v(address.to_owned())),
            ("Alias".to_owned(), v(alias.to_owned())),
            ("Paired".to_owned(), v(true)),
            ("Connected".to_owned(), v(connected)),
            ("Trusted".to_owned(), v(true)),
        ]),
    )]);
    if let Some(percentage) = battery {
        ifaces.insert(
            "org.bluez.Battery1".to_owned(),
            HashMap::from([("Percentage".to_owned(), v(percentage))]),
        );
    }
    ifaces
}

/// `n` scanned devices under the adapter, with the adapter.
fn adapter_with_devices(n: u8) -> HashMap<String, Ifaces> {
    let mut objects = HashMap::from([(ADAPTER.to_owned(), adapter())]);
    for i in 0..n {
        let address = format!("10:00:00:00:00:{i:02X}");
        objects.insert(
            format!("{ADAPTER}/dev_{}", address.replace(':', "_")),
            device(&address, &format!("Device {i:02}"), false, None),
        );
    }
    objects
}

type Row = (String, bool, Option<u8>);

fn rows(list: &[Device]) -> Vec<Row> {
    let mut rows: Vec<Row> = list
        .iter()
        .map(|dev| (dev.alias.clone(), dev.connected, dev.battery))
        .collect();
    rows.sort();
    rows
}

/// Wait until the published list is `want`; the last list published if it
/// never is.
async fn wait_rows(
    devices: &Mutable<Vec<Device>>,
    want: &[(&str, bool, Option<u8>)],
) -> Result<(), Vec<Row>> {
    let want: Vec<Row> = want
        .iter()
        .map(|(alias, connected, battery)| ((*alias).to_owned(), *connected, *battery))
        .collect();
    let mut published = devices.signal_cloned().to_stream();
    let mut last = Vec::new();
    tokio::time::timeout(BUDGET, async {
        while let Some(list) = published.next().await {
            last = rows(&list);
            if last == want {
                return;
            }
        }
    })
    .await
    .map_err(|_| last)
}

/// Wait until `len` devices are published (`listen`'s snapshot).
async fn wait_published(devices: &Mutable<Vec<Device>>, len: usize) {
    let mut published = devices.signal_cloned().to_stream();
    tokio::time::timeout(BUDGET, async {
        while let Some(list) = published.next().await {
            if list.len() == len {
                return;
            }
        }
    })
    .await
    .expect("listen published its snapshot");
}

/// Run the production `listen` on its own task.
fn spawn_listen(
    adapter: &Mutable<Option<super::Adapter>>,
    devices: &Mutable<Vec<Device>>,
) -> tokio::task::JoinHandle<Result<ListenEnd, anyhow::Error>> {
    let (adapter, devices) = (adapter.clone(), devices.clone());
    tokio::spawn(async move {
        let store = Arc::new(tokio::sync::RwLock::new(String::new()));
        listen(&adapter, &devices, &store).await
    })
}

// ── R1: the fold, through the production FoldBus ───────────────────────────

/// Through the production `SystemBluez`, once the handshake's resync has
/// been served (#1551, #1647 review R1):
///
/// - `KEYBOARD`, in `listen`'s snapshot, is subscribed by that resync and
///   read again once its rule is in place; a `Connected=false` bluez makes
///   **after** that re-read reaches its row, so only the device's live
///   subscription can have carried it.
/// - `GHOST`: an `InterfacesAdded` for a device bluez does not have (a
///   replayed add). Its re-read answers `UnknownObject`: dropped.
/// - `HEADSET`: added as its address with a battery of 50, when bluez by
///   then has its name and 40 %. The re-read wins, battery included.
/// - `SLOW`: added, and its re-read answers `NoReply`: kept as added.
///
/// This is the first test to send a real `UnknownObject` or `NoReply` reply
/// through `hytte_bus::call` into `device_is_gone`; the hermetic suite builds
/// the `BusError` by hand.
///
/// **Falsification:** hand `forward_device`'s forwarder no handshake (no
/// device is ever re-read), make `SystemBluez::get_all` ignore its
/// interface, count every error as gone, hand `arm_once_subscribed` another
/// channel's wake (no resync), or subscribe a device's `PropertiesChanged` at
/// `/org/bluez`, and this fails.
#[tokio::test(flavor = "multi_thread")]
async fn r1_the_fold_settles_on_bluez_through_the_production_bus() {
    const TEST: &str = "r1_the_fold_settles_on_bluez_through_the_production_bus";
    const KEYBOARD: &str = "/org/bluez/hci0/dev_11_11_11_11_11_11";
    const HEADSET: &str = "/org/bluez/hci0/dev_22_22_22_22_22_22";
    const GHOST: &str = "/org/bluez/hci0/dev_33_33_33_33_33_33";
    const SLOW: &str = "/org/bluez/hci0/dev_44_44_44_44_44_44";
    let Some(address) = in_child(TEST) else {
        return run_in_child(TEST, BrokerConfig::default()).await;
    };

    let fake = fake_bluez(
        &address,
        HashMap::from([
            (ADAPTER.to_owned(), adapter()),
            (
                KEYBOARD.to_owned(),
                device("11:11:11:11:11:11", "Keyboard", true, None),
            ),
        ]),
    )
    .await;
    let adapter_m = Mutable::new(None);
    let devices_m = Mutable::new(Vec::new());
    let watch = spawn_listen(&adapter_m, &devices_m);

    // `listen`'s snapshot, the handshake's resync, and the snapshot
    // device's re-read once its rule is in place.
    fake.wait_for("the handshake's resync", |st| st.managed_served >= 2)
        .await;
    fake.wait_for("the keyboard's re-read", |st| {
        st.get_all.iter().any(|(p, _)| p == KEYBOARD)
    })
    .await;
    wait_rows(&devices_m, &[("Keyboard", true, None)])
        .await
        .expect("the snapshot");

    fake.change_device(
        KEYBOARD,
        &HashMap::from([("Connected".to_owned(), v(false))]),
    )
    .await;
    wait_rows(&devices_m, &[("Keyboard", false, None)])
        .await
        .expect("a change after the keyboard's re-read reaches its row live");

    {
        let mut st = fake.state.lock().expect("fake state");
        st.objects.insert(
            HEADSET.to_owned(),
            device("22:22:22:22:22:22", "Headset", true, Some(40)),
        );
        st.objects.insert(
            SLOW.to_owned(),
            device("44:44:44:44:44:44", "Slow", false, None),
        );
        st.errors
            .insert(SLOW.to_owned(), "org.freedesktop.DBus.Error.NoReply");
    }
    fake.emit_added(GHOST, &device("33:33:33:33:33:33", "Ghost", false, None))
        .await;
    fake.emit_added(SLOW, &device("44:44:44:44:44:44", "Slow", false, None))
        .await;
    fake.emit_added(
        HEADSET,
        &device("22:22:22:22:22:22", "22-22-22-22-22-22", false, Some(50)),
    )
    .await;

    let settled = wait_rows(
        &devices_m,
        &[
            ("Headset", true, Some(40)),
            ("Keyboard", false, None),
            ("Slow", false, None),
        ],
    )
    .await;
    let reads = fake.state.lock().expect("fake state").get_all.clone();
    assert!(
        settled.is_ok(),
        "the fold never settled on bluez's state: last published {settled:?}; GetAll calls {reads:?}"
    );
    assert!(
        fake.get_all_served(GHOST),
        "the replayed add was read again"
    );
    assert!(!watch.is_finished(), "the watch ended: {:?}", watch.await);
    watch.abort();
    println!("{CHILD_OK} {}", test_name(TEST));
}

// ── R2: a refused device rule ───────────────────────────────────────────────

/// A system bus that refuses some of the fold's device `AddMatch`es (here
/// dbus-daemon's `max_match_rules_per_connection` at 16, against 24 known
/// devices; the system bus default is 512) does not hold up the fold, so
/// bluetoothd exiting ends the watch (#1647 review R2). At the #1647 head
/// the fold waited for every snapshot device's rule before it read
/// anything, owner stream included, so it never started: the old daemon's
/// 24 devices stayed on screen and the exit went unnoticed. On `main` this
/// passes too: the fold started with the refused subscriptions missing.
///
/// Any end will do here; which one is R3's business.
///
/// **Falsification:** make the fold wait for the snapshot devices' rules
/// before it starts (the #1647 head) and the watch never ends.
#[tokio::test(flavor = "multi_thread")]
async fn r2_a_refused_device_rule_does_not_hold_up_the_fold() {
    const TEST: &str = "r2_a_refused_device_rule_does_not_hold_up_the_fold";
    let Some(address) = in_child(TEST) else {
        return run_in_child(
            TEST,
            BrokerConfig {
                match_limit: Some(16),
                ..BrokerConfig::default()
            },
        )
        .await;
    };
    let fake = fake_bluez(&address, adapter_with_devices(24)).await;
    let adapter_m = Mutable::new(None);
    let devices_m = Mutable::new(Vec::new());
    let watch = spawn_listen(&adapter_m, &devices_m);
    wait_published(&devices_m, 24).await;

    // bluetoothd exits.
    fake.conn
        .release_name("org.bluez")
        .await
        .expect("release org.bluez");
    let ended = tokio::time::timeout(BUDGET, watch).await;
    let served = fake.state.lock().expect("fake state").managed_served;
    assert!(
        matches!(ended, Ok(Ok(_))),
        "bluetoothd left but the watch did not end ({ended:?}); GetManagedObjects answered \
         {served}×; still published: adapter {:?}, {} devices",
        adapter_m.get_cloned().map(|a: super::Adapter| a.powered),
        devices_m.get_cloned().len(),
    );
    println!("{CHILD_OK} {}", test_name(TEST));
}

// ── R3: the handshake's resync and a queued `Lost` ──────────────────────────

/// bluetoothd leaving while the fold's rules are being installed: the
/// owner's `Lost` and the handshake's resync can both be queued when the
/// fold next turns. The `Lost` must win, every time: the resync is a call to
/// `org.bluez`, which, sent plain, D-Bus-activates the bluetoothd the user
/// just stopped on a real system bus. Here the resync is refused instead
/// (it is sent `no_auto_start()`, so the broker answers `NameHasNoOwner`)
/// and `listen` ends with that error instead of `DaemonExited`. #1647
/// review R3: 14 of 20 rounds lost it unbiased.
///
/// Probabilistic by nature: each round is one draw of a race, which is why
/// there are twenty.
///
/// **Falsification:** drop `biased;` from the fold's `select!` and most
/// rounds call `org.bluez` with no owner.
#[tokio::test(flavor = "multi_thread")]
async fn r3_a_queued_lost_wins_over_the_handshakes_resync() {
    const TEST: &str = "r3_a_queued_lost_wins_over_the_handshakes_resync";
    const ROUNDS: u32 = 20;
    let Some(address) = in_child(TEST) else {
        return run_in_child(TEST, BrokerConfig::default()).await;
    };
    let fake = fake_bluez(&address, adapter_with_devices(4)).await;
    fake.conn
        .release_name("org.bluez")
        .await
        .expect("release org.bluez");

    let mut lost = Vec::new();
    for round in 0..ROUNDS {
        fake.conn
            .request_name("org.bluez")
            .await
            .expect("own org.bluez");
        let served = fake.state.lock().expect("fake state").managed_served;
        let devices_m = Mutable::new(Vec::new());
        let watch = spawn_listen(&Mutable::new(None), &devices_m);
        fake.wait_for("listen's snapshot", |st| st.managed_served > served)
            .await;
        wait_published(&devices_m, 4).await;
        // bluetoothd exits while the fold's rules are being installed.
        fake.conn
            .release_name("org.bluez")
            .await
            .expect("release org.bluez");
        let ended = tokio::time::timeout(BUDGET, watch)
            .await
            .expect("the watch ended")
            .expect("listen did not panic");
        if !matches!(ended, Ok(ListenEnd::DaemonExited)) {
            lost.push(format!("round {round}: {ended:?}"));
        }
    }
    assert!(
        lost.is_empty(),
        "{}/{ROUNDS} watches called org.bluez after it had no owner: {lost:?}",
        lost.len()
    );
    println!("{CHILD_OK} {}", test_name(TEST));
}

// ── R4d: bluetoothd answers a re-read and exits ─────────────────────────────

/// bluetoothd answering a device re-read and then exiting, while more
/// re-reads are queued behind it, is not started again (#1647 second
/// review, R4d; #1551).
///
/// Each `listen` re-reads every device its handshake's resync subscribes,
/// back to back. When bluetoothd answers one and exits, the answer reaches
/// the fold before the `NameOwnerChanged` does, so the fold's bias cannot
/// help: the next queued re-read goes out to an `org.bluez` with no owner.
/// Sent plain, that call D-Bus-activates bluetoothd, which the review
/// measured in 78 of 120 rounds. The fold's reads are sent `no_auto_start()`
/// so the bus refuses them instead; the `Lost` then ends every watch.
///
/// The broker counts every start of `org.bluez`
/// ([`BrokerConfig::activatable`]). Each round, the scripted bluez owns the
/// name, holds 20 devices and gives the name up right after answering its
/// third `GetAll`. The last step is the positive control: a plain call to
/// `org.bluez` with no owner, which the count has to see.
///
/// Probabilistic by nature: each round is one draw of a race, which is why
/// there are thirty.
///
/// **Falsification:** send the device re-reads plain (drop `no_auto_start()`
/// from `get_all_props`) and rounds start bluetoothd.
#[tokio::test(flavor = "multi_thread")]
async fn r4d_bluetoothd_exiting_after_a_re_read_is_not_started_again() {
    const TEST: &str = "r4d_bluetoothd_exiting_after_a_re_read_is_not_started_again";
    const ROUNDS: usize = 30;
    const LEAVE_AFTER: usize = 3;
    let Some(address) = in_child(TEST) else {
        return run_in_child(
            TEST,
            BrokerConfig {
                activatable: true,
                ..BrokerConfig::default()
            },
        )
        .await;
    };
    let fake = fake_bluez(&address, adapter_with_devices(20)).await;
    fake.conn
        .release_name("org.bluez")
        .await
        .expect("release org.bluez");

    let mut started = Vec::new();
    let mut ends = Vec::new();
    for round in 0..ROUNDS {
        let before = activations();
        {
            let mut st = fake.state.lock().expect("fake state");
            st.leave_after_get_all = Some(st.get_all.len() + LEAVE_AFTER);
        }
        fake.conn
            .request_name("org.bluez")
            .await
            .expect("own org.bluez");
        let watch = spawn_listen(&Mutable::new(None), &Mutable::new(Vec::new()));
        // Every call the watch made has its answer by the time it ends, and
        // a call that started bluetoothd is answered only once that start
        // has finished: no wait is needed for the count.
        let ended = tokio::time::timeout(BUDGET, watch)
            .await
            .expect("the watch ended")
            .expect("listen did not panic");
        assert!(
            fake.state
                .lock()
                .expect("fake state")
                .leave_after_get_all
                .is_none(),
            "round {round}: bluez never answered {LEAVE_AFTER} re-reads, so it never left"
        );
        let after = activations();
        if after > before {
            started.push(format!(
                "round {round}: {} start(s), {ended:?}",
                after - before
            ));
        }
        if !matches!(ended, Ok(ListenEnd::DaemonExited)) {
            ends.push(format!("round {round}: {ended:?}"));
        }
    }
    assert!(
        started.is_empty(),
        "{}/{ROUNDS} watches started bluetoothd after it left: {started:?}",
        started.len()
    );
    assert!(
        ends.is_empty(),
        "{}/{ROUNDS} watches did not end on bluetoothd's exit: {ends:?}",
        ends.len()
    );

    // The control: `listen`'s own snapshot, sent plain, to `org.bluez` with
    // no owner. The count sees it, so a zero above is a zero.
    let before = activations();
    let answer = super::get_managed_objects().await;
    assert_eq!(
        activations(),
        before + 1,
        "the broker did not count a plain call's start ({:?})",
        answer.err()
    );
    println!("{CHILD_OK} {}", test_name(TEST));
}

/// Each read the fold makes, sent through the production `SystemBluez` to an
/// `org.bluez` with no owner, is refused by the bus and starts nothing
/// (#1551). R4d reaches the re-read by a race; this reaches both reads, the
/// resync's `GetManagedObjects` and a device's `GetAll`, every time. The
/// refusal is also what `device_is_gone` is handed in that case, so it is
/// checked there with the broker's own error: a device whose re-read meets
/// it is kept.
///
/// The positive control is `listen`'s own snapshot, sent plain: the broker
/// counts its start.
///
/// **Falsification:** drop `no_auto_start()` from either read
/// (`SystemBluez::managed_objects`, `get_all_props`) and its call starts
/// bluetoothd; count `NameHasNoOwner` as gone and the device read fails the
/// `device_is_gone` check.
#[tokio::test(flavor = "multi_thread")]
async fn r4_the_folds_reads_never_start_bluetoothd() {
    const TEST: &str = "r4_the_folds_reads_never_start_bluetoothd";
    const DEVICE: &str = "/org/bluez/hci0/dev_11_11_11_11_11_11";
    if in_child(TEST).is_none() {
        return run_in_child(
            TEST,
            BrokerConfig {
                activatable: true,
                ..BrokerConfig::default()
            },
        )
        .await;
    }
    let (props_tx, _props_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resync_tx, _resync_rx) = tokio::sync::mpsc::channel(super::RESYNC_WAKE_QUEUE);
    let bus = super::SystemBluez {
        props_tx,
        resync_tx,
    };

    let no_owner = |error: Option<&hytte_bus::BusError>| {
        matches!(
            error,
            Some(hytte_bus::BusError::Permanent { dbus_name: Some(name), .. })
                if name == "org.freedesktop.DBus.Error.NameHasNoOwner"
        )
    };

    let resync = bus.managed_objects().await.err();
    assert_eq!(
        activations(),
        0,
        "the resync's read started bluetoothd ({resync:?})"
    );
    assert!(no_owner(resync.as_ref()), "the resync's read: {resync:?}");

    let reread = bus.get_all(DEVICE, "org.bluez.Device1").await.err();
    assert_eq!(
        activations(),
        0,
        "a device's re-read started bluetoothd ({reread:?})"
    );
    assert!(no_owner(reread.as_ref()), "the re-read: {reread:?}");
    assert!(
        reread.as_ref().is_some_and(|e| !super::device_is_gone(e)),
        "the bus refusing a re-read for want of an owner was read as the device gone: {reread:?}"
    );

    let before = activations();
    let answer = super::get_managed_objects().await;
    assert_eq!(
        activations(),
        before + 1,
        "the broker did not count a plain call's start ({:?})",
        answer.err()
    );
    println!("{CHILD_OK} {}", test_name(TEST));
}

// ── R5: a refused top-level rule ────────────────────────────────────────────

/// A system bus that refuses one of the fold's three top-level `AddMatch`es
/// does not keep bluetoothd's exit from ending the watch (#1647 second
/// review, R5).
///
/// dbus-daemon's `max_match_rules_per_connection` is 4 here: the shell's
/// connection's own rule fits, and the owner's, and two of the three
/// top-level rules; the third is refused, so the wait for them never ends.
/// Until the wait ran beside the fold, the fold started only once it was
/// over: the watch never ended, and the old device list and `Powered: true`
/// stayed on screen. Now the fold reads the owner stream meanwhile.
///
/// That a rule was refused is checked too. The wait never ending is what
/// keeps its resync from being sent, so `GetManagedObjects` is answered
/// once, for `listen`'s snapshot, and no device is read.
///
/// **Falsification:** await `arm_once_subscribed` before the fold in
/// `event_loop` (the #1647 head) and the watch never ends.
#[tokio::test(flavor = "multi_thread")]
async fn r5_a_refused_top_level_rule_does_not_hide_bluetoothds_exit() {
    const TEST: &str = "r5_a_refused_top_level_rule_does_not_hide_bluetoothds_exit";
    let Some(address) = in_child(TEST) else {
        return run_in_child(
            TEST,
            BrokerConfig {
                match_limit: Some(4),
                ..BrokerConfig::default()
            },
        )
        .await;
    };
    let fake = fake_bluez(&address, adapter_with_devices(4)).await;
    let adapter_m = Mutable::new(None);
    let devices_m = Mutable::new(Vec::new());
    let watch = spawn_listen(&adapter_m, &devices_m);
    wait_published(&devices_m, 4).await;

    // bluetoothd exits.
    fake.conn
        .release_name("org.bluez")
        .await
        .expect("release org.bluez");
    let ended = tokio::time::timeout(BUDGET, watch).await;
    let (served, read) = {
        let st = fake.state.lock().expect("fake state");
        (st.managed_served, st.get_all.len())
    };
    assert!(
        matches!(ended, Ok(Ok(Ok(ListenEnd::DaemonExited)))),
        "bluetoothd left but the watch did not end on it ({ended:?}); still published: \
         adapter {:?}, {} devices",
        adapter_m.get_cloned().map(|a: super::Adapter| a.powered),
        devices_m.get_cloned().len(),
    );
    assert_eq!(
        (served, read),
        (1, 0),
        "the handshake's resync ran, so no top-level rule was refused and this proves nothing"
    );
    println!("{CHILD_OK} {}", test_name(TEST));
}
