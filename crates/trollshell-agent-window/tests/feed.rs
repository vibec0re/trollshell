//! The window's `host.sock` loop, driven against a **scripted** fake daemon.
//!
//! What the display tests in `src/ui.rs` cannot cover is the half that makes
//! the header live: that the window keeps asking, and that a change on the
//! socket becomes a new state. That is this file, and it needs no display —
//! only a real `UnixListener` and the real client.
//!
//! Every test here is `start_paused`, so a two-second cadence costs no
//! wall-clock time and an assertion about *when* something arrives is exact.

mod fake;

use std::time::Duration;

use fake::{FakeHive, roster};
use hytte_plugin_agents::model::{AgentName, Status};
use tokio::sync::mpsc;
use trollshell_agent_window::chrome::HeaderModel;
use trollshell_agent_window::feed::{self, AgentState, Update};

const CADENCE: Duration = Duration::from_secs(2);

fn name(s: &str) -> AgentName {
    AgentName::parse(s).expect("a legal test name")
}

/// Wait for the next update, with a generous bound so a broken loop fails by
/// name instead of hanging the suite.
async fn next(rx: &mut mpsc::UnboundedReceiver<Update>, what: &str) -> Update {
    tokio::time::timeout(Duration::from_secs(60), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|| panic!("the feed ended before {what}"))
}

/// Wait for the next **state** update, skipping the `Urls` answer.
async fn next_state(rx: &mut mpsc::UnboundedReceiver<Update>, what: &str) -> AgentState {
    loop {
        if let Update::State(s) = next(rx, what).await {
            return s;
        }
    }
}

/// Yield until `pred` holds, without advancing the (paused) clock.
///
/// `tokio::time::sleep` is the wrong tool for waiting on **socket** progress
/// under `start_paused`: auto-advance jumps the clock to the next deadline as
/// soon as every task is parked, including parked on I/O, so a sleep can
/// return before a round trip the test is waiting for has happened. Yielding
/// hands the scheduler its turn instead, which is what actually drives the
/// reactor.
async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if pred() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("{what} never happened");
}

/// How many status polls the hive has answered.
fn polls(hive: &FakeHive) -> usize {
    hive.seen()
        .iter()
        .filter(|l| l.contains("agent_status"))
        .count()
}

/// Advance one cadence and wait for the poll it is supposed to cause.
///
/// One step at a time, deliberately: `MissedTickBehavior::Delay` collapses
/// every deadline crossed by a single `advance` into **one** tick, so
/// advancing five cadences at once buys one poll, not five.
async fn one_cadence(hive: &FakeHive, cadence: Duration) {
    let before = polls(hive);
    tokio::time::advance(cadence).await;
    until("the next scheduled poll", || polls(hive) > before).await;
}

/// **A status change on the socket becomes a new header.**
///
/// The scripted hive answers `running` first and `paused` after, and the loop
/// turns that into two states whose rendered header text differs. This is the
/// socket half of "the header follows the hive"; the widget half is
/// `ui.rs`'s `applying_a_new_state_rewrites_the_header`.
///
/// Mutation (verified red): delete the ticker arm from `feed::run` — i.e. stop
/// polling after the seed — and the second `next_state` times out. Deleting
/// only the dedup makes it pass for the wrong reason, which is why the
/// assertion is on the *content* of the second state rather than on its
/// arrival alone.
#[tokio::test(start_paused = true)]
async fn a_status_change_on_the_socket_becomes_a_new_header() {
    let hive = FakeHive::script(&[
        &roster(r#"{"name":"stray","running":true,"status_text":"reviewing PR #963"}"#),
        &roster(r#"{"name":"stray","running":true,"paused":true}"#),
    ]);
    let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        CADENCE,
        cmd_rx,
        out_tx,
    ));

    let cfg = hytte_plugin_agents::config::AgentsConfig::default();
    let first = next_state(&mut out_rx, "the seed poll").await;
    assert_eq!(
        HeaderModel::of(&name("stray"), &cfg, &first).status,
        "reviewing PR #963"
    );

    let second = next_state(&mut out_rx, "the poll after the hive changed its mind").await;
    assert_eq!(
        second.agent().expect("still on the roster").status(),
        Status::Paused
    );
    assert_eq!(
        HeaderModel::of(&name("stray"), &cfg, &second).status,
        "paused"
    );
}

/// An unchanged hive sends **nothing** after the seed, however many times it
/// is polled.
///
/// The header is rewritten on every `Update::State`, so a feed that repeated
/// itself would repaint twice a second forever.
///
/// Mutation (verified red): drop the `last` comparison in `poll_once` and the
/// second poll arrives.
#[tokio::test(start_paused = true)]
async fn an_unchanged_hive_sends_one_state_not_one_per_poll() {
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)]);
    let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        CADENCE,
        cmd_rx,
        out_tx,
    ));

    let _seed = next_state(&mut out_rx, "the seed poll").await;
    for _ in 0..5 {
        one_cadence(&hive, CADENCE).await;
    }
    assert!(polls(&hive) >= 6, "it kept asking: {:?}", hive.seen());
    assert!(
        out_rx.try_recv().is_err(),
        "…and an idle hive must not repaint the header"
    );
}

/// **Each button sends exactly the verb the card sends, once**, and the window
/// re-polls immediately rather than waiting out the cadence.
///
/// The bytes are asserted on the socket, not on the `Request` value, because
/// the daemon reads bytes — this is the same reason `hive::wire` pins its
/// lines.
///
/// Mutation (verified red): change any verb (`graceful: false`, a scope built
/// another way, `paused` inverted) and the recorded line differs; send it
/// twice and the count does.
#[tokio::test(start_paused = true)]
async fn each_button_sends_its_verb_once_and_repolls() {
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)]);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        // An hour, so nothing here can be a tick that happened to land.
        Duration::from_secs(3600),
        cmd_rx,
        out_tx,
    ));
    let _seed = next_state(&mut out_rx, "the seed poll").await;
    let after_seed = polls(&hive);

    let n = name("stray");
    for req in [
        feed::start(&n),
        feed::stop(&n),
        feed::set_paused(&n, true),
        feed::set_paused(&n, false),
    ] {
        cmd_tx.send(req).expect("the loop is listening");
    }
    until("the loop drained the command lane and re-polled after each", || {
        hive.writes().len() >= 4 && polls(&hive) >= after_seed + 4
    })
    .await;

    assert_eq!(
        hive.writes(),
        vec![
            r#"{"cmd":"start","scope":{"agent_names":["stray"]}}"#.to_owned(),
            r#"{"cmd":"stop","scope":{"agent_names":["stray"]},"graceful":true}"#.to_owned(),
            r#"{"cmd":"set_paused","name":"stray","paused":true}"#.to_owned(),
            r#"{"cmd":"set_paused","name":"stray","paused":false}"#.to_owned(),
        ],
        "each button writes its own verb, exactly once"
    );
    assert!(
        polls(&hive) >= after_seed + 4,
        "every accepted verb is followed by a re-poll, so the header does not lag the click \
         by a whole cadence: {} → {}",
        after_seed,
        polls(&hive)
    );
}

/// A hive that has never answered leaves the window **unreachable**, with the
/// client's own sentence, and the loop keeps trying instead of exiting.
#[tokio::test(start_paused = true)]
async fn an_absent_socket_parks_and_keeps_trying() {
    let dir = tempfile::tempdir().expect("a tempdir");
    let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    tokio::spawn(feed::run(
        dir.path().join("nothing-here.sock"),
        name("stray"),
        CADENCE,
        cmd_rx,
        out_tx,
    ));

    let state = next_state(&mut out_rx, "the first failed poll").await;
    match state {
        AgentState::Unreachable { reason } => {
            assert!(reason.contains("not running"), "{reason}");
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }

    // Still alive: the lane is open, so a later answer would still land.
    tokio::time::advance(CADENCE * 3).await;
    tokio::task::yield_now().await;
    assert!(!out_rx.is_closed(), "the loop must not exit on a dead hive");
}

/// The `Urls` answer reaches the settings page — fetched **once**, not per
/// poll.
///
/// Mutation (verified red): move the `Urls` request inside the loop and the
/// count assertion reds.
#[tokio::test(start_paused = true)]
async fn the_hives_urls_are_fetched_once() {
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)]);
    let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        CADENCE,
        cmd_rx,
        out_tx,
    ));

    let _first = next(&mut out_rx, "the first update").await;
    for _ in 0..4 {
        one_cadence(&hive, CADENCE).await;
    }
    assert!(polls(&hive) >= 5, "{:?}", hive.seen());
    assert_eq!(
        hive.seen().iter().filter(|l| l.contains("\"urls\"")).count(),
        1,
        "{:?}",
        hive.seen()
    );
}
