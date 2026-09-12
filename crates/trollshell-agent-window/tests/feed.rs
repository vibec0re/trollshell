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
    tokio::time::timeout(Duration::from_mins(1), rx.recv())
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

/// The seed poll's `Update::State`, **plus** the `Update::Approvals` it is
/// always paired with (#1141): the first successful poll has no prior
/// answer to compare against on either `last` or `last_approvals`, so it
/// emits both, in that order, with no await between the two sends
/// (`poll_once`'s own doc). A test that goes on to assert "no more messages"
/// or "the very next message is X" has to drain this pair first, or it sees
/// the seed's own approvals answer instead of what it is actually waiting
/// for — this is that drain, named so a reader sees why it is there.
async fn seed_state(rx: &mut mpsc::UnboundedReceiver<Update>) -> AgentState {
    let state = next_state(rx, "the seed poll").await;
    assert!(
        matches!(
            next(rx, "the seed's paired approvals").await,
            Update::Approvals(_)
        ),
        "the seed poll must pair exactly one Approvals answer with its State"
    );
    state
}

/// Whether the hive was ever asked for the approval queue.
fn asked_for_the_queue(hive: &FakeHive) -> bool {
    hive.seen().iter().any(|l| l.contains("\"pending\""))
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

/// How many times the hive has been asked for its `Urls`.
fn urls_asks(hive: &FakeHive) -> usize {
    hive.seen()
        .iter()
        .filter(|l| l.contains("\"urls\""))
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
        tokio::sync::watch::channel(true).1,
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
        tokio::sync::watch::channel(true).1,
        out_tx,
    ));

    let _seed = seed_state(&mut out_rx).await;
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
        Duration::from_hours(1),
        cmd_rx,
        tokio::sync::watch::channel(true).1,
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
    until(
        "the loop drained the command lane and re-polled after each",
        || hive.writes().len() >= 4 && polls(&hive) >= after_seed + 4,
    )
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

/// **A refused pause puts the toggle back where the hive has it.** The
/// reviewer's test (#1130 M2), taken as supplied.
///
/// The hole it closes: `Update::Refused` set a banner and nothing else, and
/// the dedup in `poll_once` then swallowed the reconciling poll — because the
/// reconciling state *is* the state we last sent. So the operator clicked
/// pause, the hive said "agent busy", the banner said so, and the toggle
/// **stayed down**, with the window claiming a pause the daemon never made and
/// nothing to move it back until some unrelated field changed.
///
/// The fix clears `last` on a failed write, so the next poll re-emits.
///
/// Mutation (re-run this round, red): the reviewer's **M8** — replace the
/// refusal arm in `feed::run` with `let _ = write(&socket, &req).await;` — and
/// the `Update::Refused` assertion reds. Dropping only the `last = None` line
/// reds the reconciling-state half, which is the one that was actually broken.
#[tokio::test(start_paused = true)]
async fn a_refused_pause_puts_the_toggle_back_where_the_hive_has_it() {
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)])
        .refusing("set_paused", "agent busy");
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        // An hour: nothing here may be a scheduled tick that happened to land.
        Duration::from_hours(1),
        cmd_rx,
        tokio::sync::watch::channel(true).1,
        out_tx,
    ));

    let seed = seed_state(&mut out_rx).await;
    assert!(!seed.agent().expect("on the roster").paused());

    cmd_tx
        .send(feed::set_paused(&name("stray"), true))
        .expect("the loop is listening");

    let refused = next(&mut out_rx, "the refusal").await;
    match &refused {
        Update::Refused { reason, .. } => assert_eq!(reason, "agent busy"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // The fix: a refusal forces the next poll to re-emit, so the GTK side
    // re-applies and `Header::apply` gets its chance to put the toggle back.
    let state = next_state(&mut out_rx, "the reconciling state after a refusal").await;
    assert!(
        !state.agent().expect("still on the roster").paused(),
        "the hive never paused it, so the window must stop claiming it did"
    );
}

/// A refusal that is **not** followed by a reconciling state would be the
/// whole bug, so this pins the ordering too: refusal first, then the state.
///
/// Separate from the test above because that one asserts *what* the state
/// says and this asserts *that one arrives at all* — the two fail for
/// different reasons and a reader should be able to tell which.
#[tokio::test(start_paused = true)]
async fn every_refused_verb_is_followed_by_a_reconciling_state() {
    for (verb, req) in [
        ("start", feed::start(&name("stray"))),
        ("stop", feed::stop(&name("stray"))),
        ("set_paused", feed::set_paused(&name("stray"), true)),
    ] {
        let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)])
            .refusing(verb, "the hive said no");
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        tokio::spawn(feed::run(
            hive.path().to_path_buf(),
            name("stray"),
            Duration::from_hours(1),
            cmd_rx,
            tokio::sync::watch::channel(true).1,
            out_tx,
        ));
        let _seed = seed_state(&mut out_rx).await;

        cmd_tx.send(req).expect("the loop is listening");
        assert!(
            matches!(next(&mut out_rx, verb).await, Update::Refused { .. }),
            "{verb} must report its refusal"
        );
        let _reconciled = next_state(&mut out_rx, "the reconciling state").await;
    }
}

/// An **accepted** verb still dedups — the refusal path widens nothing.
///
/// Without this, "clear `last` on failure" could just as well have been
/// "clear `last` on every command", which would repaint the header on every
/// click for ever.
#[tokio::test(start_paused = true)]
async fn an_accepted_verb_does_not_force_a_repaint() {
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)]);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        Duration::from_hours(1),
        cmd_rx,
        tokio::sync::watch::channel(true).1,
        out_tx,
    ));
    let _seed = seed_state(&mut out_rx).await;
    let before = polls(&hive);

    cmd_tx
        .send(feed::set_paused(&name("stray"), true))
        .expect("the loop is listening");
    until("the re-poll after an accepted verb", || {
        polls(&hive) > before
    })
    .await;

    assert!(
        out_rx.try_recv().is_err(),
        "the hive accepted it and said nothing changed, so the header must not repaint"
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
        tokio::sync::watch::channel(true).1,
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
/// The `Urls` answer reaches the settings page — asked **once** when the hive
/// answers it, not once per poll.
///
/// Mutation (verified red, #1130 review M20): move the `Urls` request inside
/// the loop unconditionally and the count assertion reds.
#[tokio::test(start_paused = true)]
async fn the_hives_urls_are_fetched_once() {
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)])
        .with_urls("hive.local", "https://hive.local/");
    let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        CADENCE,
        cmd_rx,
        tokio::sync::watch::channel(true).1,
        out_tx,
    ));

    let first = next(&mut out_rx, "the first update").await;
    assert!(
        matches!(first, Update::Urls(_)),
        "the urls answer comes before the seed poll, so the Settings tab is never blank \
         after the first paint: {first:?}"
    );
    // Drain the seed before advancing: the ticker does not exist until the
    // loop has got past it, and a cadence advanced before that is a cadence
    // the ticker never sees.
    let _seed = next_state(&mut out_rx, "the seed poll").await;
    for _ in 0..4 {
        one_cadence(&hive, CADENCE).await;
    }
    assert!(polls(&hive) >= 5, "{:?}", hive.seen());
    assert_eq!(urls_asks(&hive), 1, "{:?}", hive.seen());
}

/// **A hive that cannot answer `Urls` yet is retried** — with backoff, not on
/// every poll.
///
/// The hole (#1130 L6): the request was a single attempt before the loop, so a
/// window opened while the hive was down showed `—` for Domain and Dashboard
/// on the Settings tab for the rest of the session, **even after the header
/// went green**. The old test pinned the "once" and not the "retry if it
/// failed", which is why the bug was invisible.
///
/// Mutation (re-run this round, red): make `UrlsFetch::attempt` give up after
/// its first failure (set `done` in the failure arm) and the "more than once"
/// assertion reds; drop the backoff (never set `skip`) and the "fewer than one
/// per poll" assertion does.
#[tokio::test(start_paused = true)]
async fn urls_are_retried_with_backoff_until_the_hive_answers() {
    /// Enough polls that a per-poll ask and a backed-off one are far apart.
    const POLLS: usize = 40;

    // No `with_urls`: the fake answers the verb with a bare success carrying
    // no urls, which is what a hive that cannot answer looks like.
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)]);
    let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        CADENCE,
        cmd_rx,
        tokio::sync::watch::channel(true).1,
        out_tx,
    ));
    let _seed = next_state(&mut out_rx, "the seed poll").await;

    for _ in 0..POLLS {
        one_cadence(&hive, CADENCE).await;
    }

    let asks = urls_asks(&hive);
    assert!(
        asks > 1,
        "a hive that could not answer must be asked again — otherwise the Settings tab shows \
         a dash for the life of the window ({asks} asks over {POLLS} polls)"
    );
    assert!(
        asks < POLLS,
        "…but not on every poll: that is a request per cadence for a value that never \
         changes ({asks} asks over {POLLS} polls)"
    );
}

/// **A hive that refuses the status poll is not also asked for the queue**
/// (#1146's review, M3).
///
/// `poll_once`'s own doc claims this — "a dead hive should cost one failed
/// connect, not two", the order `hytte_plugin_agents::poll::poll_once` uses —
/// and nothing pinned it: replacing the `if status_ok` gate with `if true`
/// left all 69 tests green.
///
/// Mutation (verified red): `if status_ok` → `if true`.
#[tokio::test(start_paused = true)]
async fn a_refused_status_poll_does_not_also_ask_for_the_queue() {
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)])
        .refusing("agent_status", "the hive said no");
    let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        CADENCE,
        cmd_rx,
        tokio::sync::watch::channel(true).1,
        out_tx,
    ));

    // Not `seed_state`: a refused status has no paired approvals answer,
    // which is exactly what this test is about.
    let state = next_state(&mut out_rx, "the seed poll").await;
    assert!(
        matches!(state, AgentState::Unreachable { .. }),
        "a refused status poll is an unreachable hive, not a roster: {state:?}"
    );
    // A few more cadences, so this cannot pass merely by looking too early.
    for _ in 0..3 {
        one_cadence(&hive, CADENCE).await;
    }
    assert!(
        !asked_for_the_queue(&hive),
        "a hive that refuses the status poll must not also be asked for the queue: {:?}",
        hive.seen()
    );
}

/// **A refused `Pending` clears this window's rows** rather than freezing on
/// the last good answer, and says which it is (#1146's review, M3/M1).
///
/// `FakeHive::refusing("pending", …)` existed since #1130's review and no
/// test used it, so the documented behaviour change was unpinned. The
/// refusal rides the update as an `Err` carrying the hive's own sentence —
/// an empty `Vec` would be indistinguishable from a healthy empty queue.
///
/// Mutation (verified red): map the `Pending` error to `Ok(Vec::new())` in
/// `poll_once` and the `Err` assertion reds.
#[tokio::test(start_paused = true)]
async fn a_refused_pending_clears_the_rows_and_names_the_reason() {
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)])
        .refusing("pending", "the approval queue is not available");
    let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        CADENCE,
        cmd_rx,
        tokio::sync::watch::channel(true).1,
        out_tx,
    ));

    let _state = next_state(&mut out_rx, "the seed poll").await;
    let approvals = loop {
        if let Update::Approvals(a) = next(&mut out_rx, "the seed's paired approvals").await {
            break a;
        }
    };
    let reason = approvals.expect_err("a refused queue must not arrive as an answered one");
    assert!(
        reason.contains("the approval queue is not available"),
        "the hive's own sentence has to survive to the group that shows it: {reason}"
    );
    assert!(
        asked_for_the_queue(&hive),
        "…and the window did ask, since the status poll answered: {:?}",
        hive.seen()
    );
}

/// **One poll's two answers reach the window as one batch** (#1146's review,
/// L3).
///
/// `poll_once` sends `Update::State` and `Update::Approvals` back to back
/// with no `.await` between them, so the header and the rows a window paints
/// come from the same observation. That invariant used to be a comment whose
/// only teeth were an accident of where `run` built its ticker: adding an
/// await between the sends reddened the two *urls* tests, with a message
/// naming nothing about the cause.
///
/// This is the direct pin — the `Approvals` has to be sitting in the channel
/// the moment the `State` comes off it, which is exactly false if anything
/// awaits in between.
///
/// Mutation (verified red): await the `Pending` request after sending
/// `Update::State` instead of before it.
#[tokio::test(start_paused = true)]
async fn the_polls_two_answers_arrive_with_no_await_between_them() {
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)]);
    let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        CADENCE,
        cmd_rx,
        tokio::sync::watch::channel(true).1,
        out_tx,
    ));

    let _state = next_state(&mut out_rx, "the seed poll").await;
    match out_rx.try_recv() {
        Ok(Update::Approvals(_)) => {}
        other => panic!(
            "the queue must already be in the channel when the state comes off it — anything \
             awaiting between the two sends splits one observation across two paints: {other:?}"
        ),
    }
}

/// **A window that was never mapped never polls, and mapping it costs exactly
/// one poll — not the cadences it missed while parked** (#1149 L4).
///
/// `feed::run`'s `visible` parameter is driven directly here rather than
/// through a real GTK map signal, the same way `each_button_sends_its_verb_once_and_repolls`
/// drives `cmd_tx` directly rather than a real button: `Window::build` is
/// what wires the receiver to `connect_map`/`connect_unmap`, and that wiring
/// has no logic of its own left to test once this pins what it delivers.
///
/// Mutation (verified red): drop the `if mapped` guard on the ticker arm and
/// the first assertion reds — the hive is polled while nothing could
/// possibly be looking at the window.
#[tokio::test(start_paused = true)]
async fn a_window_that_was_never_mapped_never_polls_and_maps_to_exactly_one() {
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)]);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    // The updates are never read — the assertions below are all on
    // `polls(&hive)`, and the channel only has to stay open (a dropped
    // receiver would end `feed::run` on its first send).
    let (out_tx, _out_rx) = mpsc::unbounded_channel();
    let (visible_tx, visible_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        CADENCE,
        cmd_rx,
        visible_rx,
        out_tx,
    ));

    // Several cadences pass with the window never mapped.
    for _ in 0..5 {
        tokio::time::advance(CADENCE).await;
    }

    // A probe, forced through the loop regardless of visibility
    // (`cmds.recv()` carries no `if mapped` guard): `until`, not a bare
    // assertion straight after the blind `advance` loop above, is what
    // makes "zero polls" a property this test actually exercised rather
    // than ticks the runtime simply never got around to delivering —
    // `unmapping_again_re_parks_the_poll_and_remapping_resumes_it`'s doc
    // has the longer version of why a raw `advance` loop alone cannot be
    // trusted here. The extra yields after `until` returns give a would-be
    // stray tick — one whose deadline the `advance` loop already passed —
    // room to fire on its own before the assertion below locks in the
    // count.
    cmd_tx
        .send(feed::set_paused(&name("stray"), false))
        .expect("the loop is listening");
    until("the probe's reconciling poll", || polls(&hive) > 0).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        polls(&hive),
        1,
        "only the probe's own reconciling poll — a window that was never mapped must never \
         poll on its own, however many cadences pass: {:?}",
        hive.seen()
    );
    let after_probe = polls(&hive);

    // Map it: exactly one more poll, immediately — not a burst of the five
    // cadences missed while parked.
    visible_tx.send(true).expect("the loop is listening");
    until("the map-edge poll", || polls(&hive) > after_probe).await;
    assert_eq!(
        polls(&hive),
        after_probe + 1,
        "one poll on the map edge, not a burst of the missed cadences: {:?}",
        hive.seen()
    );

    // The cadence resumes normally from here — a single ordinary tick, not
    // zero and not several.
    one_cadence(&hive, CADENCE).await;
    assert_eq!(polls(&hive), after_probe + 2);
}

/// **Unmapping again re-parks the poll**, and remapping resumes cleanly — not
/// just the first map/unmap edge the test above covers.
///
/// The unmap is confirmed with a **probe command** before any cadence is
/// advanced, rather than a bare `advance` after a few `yield_now`s: under
/// `start_paused`, `tokio::time::advance` does not guarantee that a task
/// parked on a plain (non-timer) wakeup — here, the just-sent `false` — has
/// actually been polled before the clock jumps, so asserting an absence right
/// after a blind advance can pass for the wrong reason (it did, once, in this
/// exact test). `cmds.recv()` carries no `if mapped` guard, so sending a
/// harmless command always reaches the loop and forces a reconciling poll;
/// biased selection checks the visibility branch ahead of it every iteration,
/// so by the time that poll lands, `mapped` is provably `false` — and with it
/// false, `ticker.tick()` is never even polled, so nothing short of the
/// production code being wrong could make the cadences below fire.
///
/// Mutation (verified red): make the unmap arm a no-op (drop the `mapped =
/// now` assignment on the `false` branch) and the "parked" assertion reds —
/// the ticker keeps firing after the window is hidden.
#[tokio::test(start_paused = true)]
async fn unmapping_again_re_parks_the_poll_and_remapping_resumes_it() {
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)]);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let (visible_tx, visible_rx) = tokio::sync::watch::channel(true);
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        CADENCE,
        cmd_rx,
        visible_rx,
        out_tx,
    ));
    let _seed = seed_state(&mut out_rx).await;
    let after_seed = polls(&hive);

    visible_tx.send(false).expect("the loop is listening");
    cmd_tx
        .send(feed::set_paused(&name("stray"), false))
        .expect("the loop is listening");
    until("the probe's reconciling poll", || {
        polls(&hive) > after_seed
    })
    .await;
    let after_probe = polls(&hive);

    for _ in 0..5 {
        tokio::time::advance(CADENCE).await;
    }
    assert_eq!(
        polls(&hive),
        after_probe,
        "parked: no polls while unmapped, however many cadences pass: {:?}",
        hive.seen()
    );

    visible_tx.send(true).expect("the loop is listening");
    until("the resume poll after remapping", || {
        polls(&hive) > after_probe
    })
    .await;
    assert_eq!(
        polls(&hive),
        after_probe + 1,
        "exactly one resume poll, not a burst: {:?}",
        hive.seen()
    );
}

/// A dropped visibility sender (the window torn down mid-poll) must not spin
/// this task: `changed()` errors instantly and forever once the sender side
/// is gone, so without latching `visible_open` to `false` this becomes a
/// tight loop that never yields to the ticker or the command lane again.
///
/// Asserted by the command lane still working *after* the sender drops —
/// a spun loop would starve `cmds.recv()` and this would time out instead of
/// failing an assertion, which is why `until` bounds it rather than awaiting
/// forever.
#[tokio::test(start_paused = true)]
async fn a_dropped_visibility_sender_does_not_spin_the_poller() {
    let hive = FakeHive::script(&[&roster(r#"{"name":"stray","running":true}"#)]);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let (visible_tx, visible_rx) = tokio::sync::watch::channel(true);
    tokio::spawn(feed::run(
        hive.path().to_path_buf(),
        name("stray"),
        Duration::from_hours(1),
        cmd_rx,
        visible_rx,
        out_tx,
    ));
    let _seed = seed_state(&mut out_rx).await;
    drop(visible_tx);

    cmd_tx
        .send(feed::set_paused(&name("stray"), true))
        .expect("the loop is listening");
    until("the command lane still drains after the sender drops", || {
        !hive.writes().is_empty()
    })
    .await;
}
