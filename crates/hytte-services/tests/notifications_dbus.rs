#![cfg(feature = "system-tests")]
//! The shell's `org.freedesktop.Notifications` daemon (`notifications.rs`),
//! called over D-Bus the way a client calls it (#1552, bullet 3): `Notify`,
//! `CloseNotification`, `GetCapabilities` and `GetServerInformation` on the
//! bus, with the replies and the `NotificationClosed` / `ActionInvoked`
//! signals read back from a client's side.
//!
//! # Why each test runs in a re-exec'd child
//!
//! Every test here is a pair: a parent that spawns one ephemeral
//! `dbus-daemon` and re-execs this test binary, filtered with `--exact` to the
//! test's `_inner` twin, and that inner test, which does the work in the
//! child. The parent sets `DBUS_SESSION_BUS_ADDRESS` on the **child** with
//! `Command::env`, which needs no `unsafe` (`std::env::set_var` does, and the
//! workspace forbids `unsafe_code`). It is `hytte-bus`'s `tests/ready.rs`
//! shape. The broker harness is the one `notifications.rs`'s own
//! `system_tests` and `screensaver.rs` use.
//!
//! The child installs the daemon exactly as `App::with` does
//! (`registry::install(notifications::service())`). So what runs is the
//! production `Service::start`: `hytte_bus::own_name(BusKind::Session,
//! "org.freedesktop.Notifications")` on the real pooled session connection,
//! which the supervisor opens from `DBUS_SESSION_BUS_ADDRESS`. Two things
//! need that, and the in-module `system_tests` can reach neither:
//!
//! * **The production call site.** Those tests hand `start_on` an ephemeral
//!   bus's builder, so `Service::start`'s own builder expression is never
//!   run there. A wrong bus kind or a misspelt name in it fails every test
//!   here, because a client on the session bus then reaches no daemon.
//! * **The shared registry across a bus round trip.** `CloseNotification`,
//!   the expiry timer and `invoke_action` close and emit through the
//!   process-global `shared` registry. An in-process test may only touch
//!   that map under `TEST_LOCK`, and clippy's `await_holding_lock` keeps the
//!   lock from spanning an `.await`. So the in-module test unregisters its
//!   set before its first round trip, and a `CloseNotification` sent after
//!   that would close nothing. A child process runs one test, has the map to
//!   itself, and keeps the set registered for as long as the test runs.
//!
//! Nothing here can reach the host's buses. The child's session bus is the
//! parent's broker. Its system bus is a socket path nothing listens on, so a
//! stray system-bus call fails instead of reaching the host. Its
//! `XDG_CONFIG_HOME` and `XDG_STATE_HOME` point into the broker's tempdir.
//! The inner tests return at once unless the parent's marker names them, so
//! an ordinary run, which discovers them like any other test, does nothing in
//! them.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p hytte-services --features system-tests --test notifications_dbus
//! ```

use futures_signals::signal::{Signal, SignalExt as _};
use futures_util::StreamExt as _;
use hytte_bus::OwnState;
use hytte_bus::test_support;
use hytte_reactive::{registry, runtime};
use hytte_services::notifications::{self, LocalAction, Urgency};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use zbus::message::Type;
use zbus::zvariant::{DynamicType, OwnedValue};
use zbus::{Connection, MatchRule, Message, MessageStream};

/// Upper bound on any single round trip, on owning the name, or on a signal
/// arriving. A liveness guard, not a latency assertion: the same number and
/// reasoning as `notifications.rs`'s `system_tests` budget.
const DBUS_REPLY_BUDGET: Duration = Duration::from_secs(30);

/// Upper bound on a whole child process. A child that hangs fails its parent
/// test by name instead of parking the binary.
const CHILD_BUDGET: Duration = Duration::from_mins(2);

/// How long a test listens, after its last expected signal, for one that
/// should not come. Each such test also orders a barrier signal after the
/// step that must stay silent (see the test docs), so this window only widens
/// what the barrier already catches.
const QUIET: Duration = Duration::from_millis(200);

/// Set on the re-exec'd child, to the name of the one inner test it runs.
const CHILD_ENV: &str = "HYTTE_SERVICES_NOTIFICATIONS_DBUS_CHILD";

/// Printed by the child, with the inner test's name, only once its whole body
/// has run. Without it, a stale `--exact` filter that matches no test would
/// still exit 0, and libtest would report `0 passed`.
const CHILD_OK: &str = "notifications-dbus-child-reached-the-end";

/// Spelled as literals, not taken from `notifications.rs`: this is the bus
/// surface a client sees, and a test built from the daemon's own constants
/// would agree with a typo in them.
const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// `expire_timeout = 0`: stays until closed. The tests that are not about
/// expiry post with it, so no 5 s default expiry can close a notification
/// under a slow runner before the test closes it.
const STICKY: i32 = 0;

// ── Parent side: one broker and one child per test ──────────────────────────

/// Kills the spawned `dbus-daemon` on drop. `screensaver.rs`'s `BusGuard`:
/// SIGKILL, then a `block_in_place` wait so the socket's `TempDir` outlives
/// the process (hence the multi-thread runtime on every parent test).
struct BusGuard {
    child: Option<Child>,
    tmp: TempDir,
    address: String,
}

impl Drop for BusGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                let _ = handle.block_on(child.wait());
            });
        }
    }
}

/// Spawn a fresh `dbus-daemon` and return a guard that kills it on drop.
async fn ephemeral_bus() -> BusGuard {
    let tmp = TempDir::new().expect("create tempdir for dbus-daemon");
    let socket: PathBuf = tmp.path().join("bus");
    let address = format!("unix:path={}", socket.display());

    let config = tmp.path().join("session.conf");
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
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
        ),
    )
    .expect("write dbus-daemon config");

    let mut child = Command::new("dbus-daemon")
        .arg("--config-file")
        .arg(&config)
        .arg("--print-address=1")
        .arg("--nofork")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn dbus-daemon — install package `dbus` if missing");

    let stdout = child.stdout.take().expect("dbus-daemon stdout");
    let mut lines = BufReader::new(stdout).lines();
    let printed = tokio::time::timeout(DBUS_REPLY_BUDGET, lines.next_line())
        .await
        .expect("dbus-daemon address timeout")
        .expect("dbus-daemon read address")
        .expect("dbus-daemon closed stdout");
    assert!(
        printed.contains("unix:path="),
        "unexpected dbus-daemon address: {printed}"
    );

    BusGuard {
        child: Some(child),
        tmp,
        address,
    }
}

/// Run `inner` in a re-exec'd child of this binary, on a broker of its own,
/// and fail with the child's output unless it ran to the end.
async fn run_in_child(inner: &str) {
    let bus = ephemeral_bus().await;
    let scratch = bus.tmp.path();
    let no_system_bus = format!("unix:path={}", scratch.join("no-system-bus").display());

    let args = ["--exact", "--nocapture", "--test-threads=1", inner];
    let exe = std::env::current_exe().expect("this test binary's own path");
    let child = Command::new(exe)
        .args(args)
        .env(CHILD_ENV, inner)
        // Spelled as literals: these are the variables zbus reads, not ours.
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env("DBUS_SYSTEM_BUS_ADDRESS", &no_system_bus)
        .env("XDG_CONFIG_HOME", scratch.join("config"))
        .env("XDG_STATE_HOME", scratch.join("state"))
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(CHILD_BUDGET, child)
        .await
        .unwrap_or_else(|_| panic!("{inner}: the child did not finish within {CHILD_BUDGET:?}"))
        .expect("re-exec this test binary with the broker as its session bus");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{inner} failed in the child ({:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        out.status,
    );
    assert!(
        stdout.contains(&format!("{CHILD_OK} {inner}")),
        "the child exited 0 without reaching the end of {inner}: a stale filter \
         matches no test and libtest still reports success\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    );
}

// ── Child side: the daemon, and a client of it ──────────────────────────────

/// In the child that runs `inner`: install the notifications service the way
/// `App::with` does, wait until it owns `org.freedesktop.Notifications`, and
/// return a client connection on the same bus. `None` everywhere else, which
/// the inner test takes as "return now".
async fn daemon_in_child(inner: &str) -> Option<Connection> {
    if std::env::var(CHILD_ENV).ok().as_deref() != Some(inner) {
        return None;
    }
    // Never fall back to zbus's own session-bus discovery, which would find
    // the host's bus under `XDG_RUNTIME_DIR`.
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        .expect("the parent sets the broker as the child's session bus");
    assert!(
        address.starts_with("unix:path="),
        "unexpected session bus address in the child: {address}"
    );

    registry::install(Box::new(notifications::service()), runtime::handle());
    wait_owned().await;

    Some(
        test_support::connect(&address)
            .await
            .expect("a client connection to the broker"),
    )
}

/// Wait until `notifications::ownership()` reports the name owned, i.e. the
/// object is served.
async fn wait_owned() {
    let mut states = Box::pin(notifications::ownership().to_stream());
    let mut seen = Vec::new();
    let owned = tokio::time::timeout(DBUS_REPLY_BUDGET, async {
        while let Some(state) = states.next().await {
            if state == OwnState::Owned {
                return true;
            }
            seen.push(state);
        }
        false
    })
    .await;
    assert!(
        matches!(owned, Ok(true)),
        "the daemon never owned {NAME} on the session bus within {DBUS_REPLY_BUDGET:?}; \
         states seen: {seen:?}"
    );
}

/// The child has run `inner` to the end: tell the parent.
fn reached_the_end(inner: &str) {
    println!("{CHILD_OK} {inner}");
}

/// The value `signal` holds now: a `Mutable`'s signal yields its current value
/// on the first poll.
async fn current<S: Signal>(signal: S) -> S::Item {
    let mut stream = Box::pin(signal.to_stream());
    tokio::time::timeout(DBUS_REPLY_BUDGET, stream.next())
        .await
        .expect("a Mutable's signal yields at once")
        .expect("a Mutable's signal does not end")
}

/// Call `member` on the daemon's object, bounded by [`DBUS_REPLY_BUDGET`].
async fn call<B>(client: &Connection, member: &str, body: &B) -> zbus::Result<Message>
where
    B: serde::Serialize + DynamicType,
{
    tokio::time::timeout(
        DBUS_REPLY_BUDGET,
        client.call_method(Some(NAME), PATH, Some(NAME), member, body),
    )
    .await
    .unwrap_or_else(|_| panic!("{member} got no reply within {DBUS_REPLY_BUDGET:?}"))
}

/// A client's `Notify`, with no icon, body or hints; the id it returns.
/// `actions` is the spec's flat `[key, label, …]` list.
async fn notify(
    client: &Connection,
    replaces_id: u32,
    summary: &str,
    actions: &[&str],
    expire_timeout: i32,
) -> u32 {
    let body = (
        "notifications-dbus-test",
        replaces_id,
        "",
        summary,
        "",
        actions.to_vec(),
        HashMap::<String, OwnedValue>::new(),
        expire_timeout,
    );
    call(client, "Notify", &body)
        .await
        .unwrap_or_else(|e| panic!("Notify({summary:?}) failed: {e}"))
        .body()
        .deserialize()
        .expect("Notify returns a u32")
}

/// A client's `CloseNotification(id)`.
async fn close_notification(client: &Connection, id: u32) -> zbus::Result<Message> {
    call(client, "CloseNotification", &(id,)).await
}

/// Subscribe `client` to the daemon's `member` signal, the way a client such
/// as libnotify does: by the name's owner, the object path and the interface.
/// The broker resolves the well-known `sender` to its current owner, so only
/// the daemon's signals match. The match is in place when this returns.
async fn subscribe(client: &Connection, member: &'static str) -> MessageStream {
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(NAME)
        .expect("a valid sender")
        .path(PATH)
        .expect("a valid path")
        .interface(NAME)
        .expect("a valid interface")
        .member(member)
        .expect("a valid member")
        .build();
    tokio::time::timeout(
        DBUS_REPLY_BUDGET,
        MessageStream::for_match_rule(rule, client, None),
    )
    .await
    .unwrap_or_else(|_| panic!("AddMatch for {member} got no reply"))
    .unwrap_or_else(|e| panic!("subscribe to {member}: {e}"))
}

/// The next signal on `stream`, failing if none arrives within the budget.
async fn next_signal(stream: &mut MessageStream, what: &str) -> Message {
    tokio::time::timeout(DBUS_REPLY_BUDGET, stream.next())
        .await
        .unwrap_or_else(|_| panic!("no {what} on the bus within {DBUS_REPLY_BUDGET:?}"))
        .expect("the signal stream does not end")
        .expect("a well-formed signal")
}

/// The next `NotificationClosed`, as `(id, reason)`.
async fn next_closed(stream: &mut MessageStream) -> (u32, u32) {
    next_signal(stream, "NotificationClosed")
        .await
        .body()
        .deserialize()
        .expect("NotificationClosed carries (uu)")
}

/// The next `ActionInvoked`, as `(id, action_key)`.
async fn next_invoked(stream: &mut MessageStream) -> (u32, String) {
    next_signal(stream, "ActionInvoked")
        .await
        .body()
        .deserialize()
        .expect("ActionInvoked carries (us)")
}

/// Nothing more arrives on `stream` for [`QUIET`].
async fn quiet(stream: &mut MessageStream, what: &str) {
    if let Ok(Some(extra)) = tokio::time::timeout(QUIET, stream.next()).await {
        panic!("an unexpected {what} arrived: {extra:?}");
    }
}

/// The id of the live notification whose summary is `summary`.
async fn live_id(summary: &str) -> u32 {
    current(notifications::active())
        .await
        .iter()
        .find(|n| n.summary == summary)
        .map(|n| n.id)
        .unwrap_or_else(|| panic!("no live notification titled {summary:?}"))
}

// ── The tests ───────────────────────────────────────────────────────────────

/// `Notify` returns a nonzero id, and a second `Notify` whose `replaces_id`
/// is that id returns the same id and replaces the notification in place:
/// one card, carrying the new summary.
///
/// Through the bus this pins the `replaces_id` argument's position in the
/// wire signature and the reply the client adopts. libnotify keeps updating
/// a notification under whatever id `Notify` returned, so a replace that
/// came back with a fresh id would leave the old card on screen.
#[tokio::test(flavor = "multi_thread")]
async fn notify_returns_an_id_and_a_replace_keeps_it() {
    run_in_child("notify_returns_an_id_and_a_replace_keeps_it_inner").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn notify_returns_an_id_and_a_replace_keeps_it_inner() {
    let inner = "notify_returns_an_id_and_a_replace_keeps_it_inner";
    let Some(client) = daemon_in_child(inner).await else {
        return;
    };

    let first = notify(&client, 0, "first", &[], STICKY).await;
    assert_ne!(first, 0, "Notify returns a nonzero id");

    let replaced = notify(&client, first, "replaced", &[], STICKY).await;
    assert_eq!(
        replaced, first,
        "a Notify whose replaces_id names a live notification returns that id"
    );

    let live: Vec<(u32, String)> = current(notifications::active())
        .await
        .into_iter()
        .map(|n| (n.id, n.summary))
        .collect();
    assert_eq!(
        live,
        [(first, "replaced".to_string())],
        "the replace lands in place: one live notification, with the new summary"
    );

    reached_the_end(inner);
}

/// `CloseNotification(id)` emits `NotificationClosed(id, 3)`, reason 3 being
/// the spec's "closed by a call to `CloseNotification`", from the name's
/// owner at the spec's path. The notification leaves the live list and is
/// filed in the history with reason 3.
///
/// The handler runs on a bus worker thread, not the GTK thread. The
/// history entry and the signal both show that its close found the daemon
/// through the process-global `shared` registry. A close that looked in the
/// GTK thread-local registry from there would find nothing and do nothing,
/// which is the "history is empty" bug `notifications.rs` describes.
#[tokio::test(flavor = "multi_thread")]
async fn close_notification_emits_notification_closed_with_reason_3() {
    run_in_child("close_notification_emits_notification_closed_with_reason_3_inner").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn close_notification_emits_notification_closed_with_reason_3_inner() {
    let inner = "close_notification_emits_notification_closed_with_reason_3_inner";
    let Some(client) = daemon_in_child(inner).await else {
        return;
    };
    let mut closed = subscribe(&client, "NotificationClosed").await;

    let id = notify(&client, 0, "closed by a call", &[], STICKY).await;
    close_notification(&client, id)
        .await
        .expect("CloseNotification on a live id replies success");

    assert_eq!(
        next_closed(&mut closed).await,
        (id, 3),
        "CloseNotification emits NotificationClosed(id, 3)"
    );
    assert!(
        current(notifications::active())
            .await
            .iter()
            .all(|n| n.id != id),
        "the closed notification leaves the live list"
    );
    assert!(
        current(notifications::history())
            .await
            .iter()
            .any(|h| h.id == id && h.reason == 3 && h.summary == "closed by a call"),
        "and is filed in the history with reason 3"
    );

    reached_the_end(inner);
}

/// `GetCapabilities` and `GetServerInformation` answer, over the bus, what the
/// daemon declares.
///
/// The capability list is pinned exactly. Clients branch on it (libnotify's
/// `notify_get_server_caps`), so adding or dropping one is a client-visible
/// change that should come with a change here. `actions` is backed by the
/// `ActionInvoked` tests below. `body-markup` is absent because the daemon
/// strips markup. Of the server information, the name and the spec version
/// are the contract: a client reads the spec version to know which spec it
/// is talking to. The vendor and the version string only have to be there.
#[tokio::test(flavor = "multi_thread")]
async fn capabilities_and_server_information_answer_over_the_bus() {
    run_in_child("capabilities_and_server_information_answer_over_the_bus_inner").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn capabilities_and_server_information_answer_over_the_bus_inner() {
    let inner = "capabilities_and_server_information_answer_over_the_bus_inner";
    let Some(client) = daemon_in_child(inner).await else {
        return;
    };

    let capabilities: Vec<String> = call(&client, "GetCapabilities", &())
        .await
        .expect("GetCapabilities replies")
        .body()
        .deserialize()
        .expect("GetCapabilities returns as");
    assert_eq!(capabilities, ["body", "icon-static", "actions"]);

    let (name, vendor, version, spec_version): (String, String, String, String) =
        call(&client, "GetServerInformation", &())
            .await
            .expect("GetServerInformation replies")
            .body()
            .deserialize()
            .expect("GetServerInformation returns (ssss)");
    assert_eq!(name, "trollshell");
    assert_eq!(spec_version, "1.2", "the spec version the daemon implements");
    assert!(!vendor.is_empty(), "a vendor");
    assert!(!version.is_empty(), "a version");

    reached_the_end(inner);
}

/// A client's `Notify` carrying actions shows them as the toast's buttons,
/// and invoking one through the daemon's own invoke path
/// (`notifications::invoke_action`, what the toast's and the history page's
/// buttons call) emits `ActionInvoked(id, key)` from the name's owner. Before
/// this test, that emit was verified only by hand on a live session.
#[tokio::test(flavor = "multi_thread")]
async fn invoking_a_client_action_emits_action_invoked() {
    run_in_child("invoking_a_client_action_emits_action_invoked_inner").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invoking_a_client_action_emits_action_invoked_inner() {
    let inner = "invoking_a_client_action_emits_action_invoked_inner";
    let Some(client) = daemon_in_child(inner).await else {
        return;
    };
    let mut invoked = subscribe(&client, "ActionInvoked").await;

    let id = notify(
        &client,
        0,
        "with actions",
        &["default", "Open", "reply", "Reply"],
        STICKY,
    )
    .await;
    let buttons: Vec<(String, String)> = current(notifications::active())
        .await
        .into_iter()
        .find(|n| n.id == id)
        .expect("the notification is live")
        .actions
        .into_iter()
        .map(|a| (a.key, a.label))
        .collect();
    assert_eq!(
        buttons,
        [
            ("default".to_string(), "Open".to_string()),
            ("reply".to_string(), "Reply".to_string()),
        ],
        "the flat [key, label, …] list becomes the toast's buttons, in order"
    );

    notifications::invoke_action(id, "reply");
    assert_eq!(
        next_invoked(&mut invoked).await,
        (id, "reply".to_string()),
        "invoking the action emits ActionInvoked(id, key)"
    );

    reached_the_end(inner);
}

/// #1572's local-action routing, seen from the bus. A click on one of the
/// shell's own toasts runs the shell's callback **instead of** broadcasting
/// `ActionInvoked`. Once a client's `Notify` replaces that toast, a click on
/// a key the shell had also registered must broadcast `ActionInvoked` for the
/// client and must not run the shell's callback.
///
/// The toast registers two keys. `open` is clicked before the replace and
/// `copy` after it, because a callback runs once and is gone: a `copy` that
/// was never clicked is still registered at the replace, so only
/// `forget_replaced_local_actions` can stop it from running. The hermetic
/// `notify_replacing_a_live_local_toast_forgets_its_callbacks` pins that the
/// `Notify` handler empties the map. This test pins what a client sees: the
/// broadcast happens, it comes from the daemon's owned connection, and the
/// served object and `invoke_action`'s registry share one callback map. The
/// broadcast for `copy` is the first `ActionInvoked` on the bus, so the `open`
/// click before it broadcast nothing.
///
/// `post_local_with_actions` always arms the 5 s default expiry, so the toast
/// is held with `pause_expiry`, as a hover holds it. Otherwise a slow runner
/// could expire it before the client's replace lands, and the replace would
/// get a fresh id.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_replace_of_a_shell_toast_turns_its_clicks_into_broadcasts() {
    run_in_child("a_client_replace_of_a_shell_toast_turns_its_clicks_into_broadcasts_inner").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_replace_of_a_shell_toast_turns_its_clicks_into_broadcasts_inner() {
    let inner = "a_client_replace_of_a_shell_toast_turns_its_clicks_into_broadcasts_inner";
    let Some(client) = daemon_in_child(inner).await else {
        return;
    };
    let mut invoked = subscribe(&client, "ActionInvoked").await;

    let opened = Arc::new(AtomicBool::new(false));
    let copied = Arc::new(AtomicBool::new(false));
    let (on_open, on_copy) = (Arc::clone(&opened), Arc::clone(&copied));
    notifications::post_local_with_actions(
        "trollshell",
        "the shell's own toast",
        "",
        Urgency::Normal,
        vec![
            LocalAction::new("open", "Open", move || on_open.store(true, Ordering::SeqCst)),
            LocalAction::new("copy", "Copy", move || on_copy.store(true, Ordering::SeqCst)),
        ],
    );
    let toast = live_id("the shell's own toast").await;
    notifications::pause_expiry(toast);

    notifications::invoke_action(toast, "open");
    assert!(
        opened.load(Ordering::SeqCst),
        "a click on the shell's own toast runs its callback"
    );

    let replaced = notify(&client, toast, "a client's replacement", &["copy", "Copy"], STICKY).await;
    assert_eq!(replaced, toast, "the client's Notify replaces the live toast");

    notifications::invoke_action(toast, "copy");
    assert!(
        !copied.load(Ordering::SeqCst),
        "the replaced toast's callback must not run on the client's click"
    );
    assert_eq!(
        next_invoked(&mut invoked).await,
        (toast, "copy".to_string()),
        "the click broadcasts ActionInvoked for the client, and it is the first \
         ActionInvoked on the bus: the shell's own `open` click broadcast nothing"
    );
    quiet(&mut invoked, "ActionInvoked").await;

    reached_the_end(inner);
}

/// `CloseNotification` on an id that is not live replies **success** and
/// emits nothing, both for an id that has already closed and for one that
/// was never issued. A close event for a notification that does not exist
/// could be taken by a client filtering the broadcast by id (libnotify does)
/// for one of its own (#1480).
///
/// The success reply is the current behaviour, pinned as it stands. Whether it
/// should be the spec's empty D-Bus error instead is an open owner question
/// on #1542 (the #1480 `CloseNotification` bullet). Its written default is
/// "it replies success, as mako does". If that question is answered the other
/// way, this test changes with it.
///
/// The second close and the never-issued id are followed by a barrier, a
/// close of a fresh notification. The next `NotificationClosed` must be the
/// barrier's, and nothing may follow it within [`QUIET`]. Neither close may
/// file a history entry either.
#[tokio::test(flavor = "multi_thread")]
async fn close_notification_on_a_non_live_id_replies_success_and_emits_nothing() {
    run_in_child("close_notification_on_a_non_live_id_replies_success_and_emits_nothing_inner")
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn close_notification_on_a_non_live_id_replies_success_and_emits_nothing_inner() {
    let inner = "close_notification_on_a_non_live_id_replies_success_and_emits_nothing_inner";
    let Some(client) = daemon_in_child(inner).await else {
        return;
    };
    let mut closed = subscribe(&client, "NotificationClosed").await;

    let id = notify(&client, 0, "closed twice", &[], STICKY).await;
    close_notification(&client, id)
        .await
        .expect("the first close replies success");
    assert_eq!(next_closed(&mut closed).await, (id, 3));

    close_notification(&client, id).await.expect(
        "a second CloseNotification for a closed id replies success, not an error (#1542)",
    );
    close_notification(&client, u32::MAX).await.expect(
        "CloseNotification for an id never issued replies success, not an error (#1542)",
    );

    let barrier = notify(&client, 0, "barrier", &[], STICKY).await;
    close_notification(&client, barrier)
        .await
        .expect("the barrier's close replies success");
    assert_eq!(
        next_closed(&mut closed).await,
        (barrier, 3),
        "the next NotificationClosed is the barrier's: neither the second close \
         nor the never-issued id emitted one"
    );
    quiet(&mut closed, "NotificationClosed").await;

    let history = current(notifications::history()).await;
    let filed: Vec<u32> = history
        .iter()
        .filter(|h| h.id == id || h.id == u32::MAX)
        .map(|h| h.id)
        .collect();
    assert_eq!(filed, [id], "only the real close files a history entry");

    reached_the_end(inner);
}

/// A notification posted with a finite `expire_timeout` emits
/// `NotificationClosed(id, 1)` when it expires, reason 1 being the spec's
/// "expired". This is the expiry timer's whole path seen from a client: the
/// handler arms the timer, the sleep fires on the hytte runtime, and its close
/// finds the daemon through the shared registry and emits.
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_notification_emits_notification_closed_with_reason_1() {
    run_in_child("an_expired_notification_emits_notification_closed_with_reason_1_inner").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_notification_emits_notification_closed_with_reason_1_inner() {
    let inner = "an_expired_notification_emits_notification_closed_with_reason_1_inner";
    let Some(client) = daemon_in_child(inner).await else {
        return;
    };
    let mut closed = subscribe(&client, "NotificationClosed").await;

    let id = notify(&client, 0, "expires", &[], 50).await;
    assert_eq!(
        next_closed(&mut closed).await,
        (id, 1),
        "a 50 ms notification expires with NotificationClosed(id, 1)"
    );
    assert!(
        current(notifications::active())
            .await
            .iter()
            .all(|n| n.id != id),
        "the expired notification leaves the live list"
    );
    assert!(
        current(notifications::history())
            .await
            .iter()
            .any(|h| h.id == id && h.reason == 1),
        "and is filed in the history with reason 1"
    );

    reached_the_end(inner);
}
