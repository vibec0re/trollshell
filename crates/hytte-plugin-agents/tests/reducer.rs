//! The reducer, driven with no host and no hive.
//!
//! `Plugin` has no transport surface, so every one of these builds a model
//! with [`hytte_plugin::cmd_channel`] — the shape that crate's own docs name
//! for exactly this — folds inputs into it, and reads back the effects it
//! returned plus the frames it queued on the command lane.
//!
//! The command lane is where the interesting assertions live: a click's
//! *visible* result is a re-render, but its *consequential* result is the JSON
//! line that reaches `host.sock`, so these pin the bytes.

use hytte_plugin::proto::{
    ConsentChoices, ConsentDecision, Effect, EffectOutcome, EventKind, Page,
};
use hytte_plugin::{CmdReceiver, Input, Plugin, cmd_channel};
use hytte_plugin_agents::Agents;
use hytte_plugin_agents::hive::client::HiveError;
use hytte_plugin_agents::hive::wire::{
    AgentStatusRow, Approval, ApprovalKind, ApprovalStatus, HiveUrls, Request, Response, Scope,
    VersionMismatch,
};
use hytte_plugin_agents::model::{Hive, Status};
use hytte_plugin_agents::plugin::card_of;
use hytte_plugin_agents::poll::{Cmd, Msg};
use hytte_plugin_agents::window::Probe;

// ── harness ──────────────────────────────────────────────────────────────────

fn model() -> (Agents, CmdReceiver<Cmd>) {
    let (tx, rx) = cmd_channel();
    (Agents::with_cmds(tx), rx)
}

/// Decode a `tests/fixtures/*.json` line into the roster it carries — the
/// same bytes `fake_socket.rs` puts through the real client, so the reducer
/// and the wire tests cannot drift apart.
fn roster(fixture: &str) -> Vec<AgentStatusRow> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing fixture {} ({e})", path.display()));
    let resp: Response = serde_json::from_str(body.trim()).expect("the fixture decodes");
    resp.agent_statuses.expect("the fixture carries a roster")
}

fn status(rows: Vec<AgentStatusRow>) -> Input<Msg> {
    Input::App(Msg::Status(Ok(rows)))
}

/// A click the host could not attribute to a screen — `output: None`, which
/// is what the drawer panel sends and what every event carried before #1050.
/// The card reads no `output`, so this is the whole surface these tests need.
fn click(id: &str) -> Input<Msg> {
    Input::event(id, EventKind::Click)
}

/// Every frame queued on the command lane so far, as the JSON lines they will
/// become on the socket.
fn lines(rx: &mut CmdReceiver<Cmd>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        if let Cmd::Send(req) = cmd {
            out.push(serde_json::to_string(&req).expect("a Request serializes"));
        }
    }
    out
}

/// Every command queued, including the visibility ones.
fn cmds(rx: &mut CmdReceiver<Cmd>) -> Vec<Cmd> {
    let mut out = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        out.push(cmd);
    }
    out
}

// ── folding a poll ───────────────────────────────────────────────────────────

#[test]
fn a_poll_becomes_a_roster_and_a_failure_becomes_one_parked_row() {
    let (mut m, _rx) = model();
    assert_eq!(m.hive, Hive::Connecting);

    m.update(status(roster("agent_status_grouped.json")));
    assert_eq!(m.hive.agents().len(), 3);
    assert_eq!(m.hive.agents()[0].name.as_str(), "trollshell-choom");

    m.update(Input::App(Msg::Status(Err(HiveError::Unreachable {
        reason: "no socket — hive-c0re is not running".to_owned(),
    }))));
    assert_eq!(
        m.hive,
        Hive::Unreachable {
            reason: "no socket — hive-c0re is not running".to_owned()
        }
    );
    assert!(m.hive.agents().is_empty());
}

/// A version mismatch gets its **own** state, not the generic unreachable one:
/// "the hive is down" and "I refuse to read this hive" are different things
/// for an operator to see.
#[test]
fn a_version_mismatch_gets_its_own_state() {
    let (mut m, _rx) = model();
    m.update(Input::App(Msg::Status(Err(HiveError::Version(
        VersionMismatch {
            theirs: 99,
            ours: 1,
        },
    )))));
    assert_eq!(
        m.hive,
        Hive::Incompatible(VersionMismatch {
            theirs: 99,
            ours: 1
        })
    );
}

/// A hive that **answered** — with `ok: false`, or with a line this build
/// cannot parse — is up, and must not be labelled unreachable. The two send
/// the operator to different places: one is a `systemctl` problem, the other
/// is not.
///
/// Falsification: fold `Refused`/`Protocol` back into the `Err(e)` catch-all
/// and the first two assertions go red.
#[test]
fn a_reachable_hive_that_refuses_is_not_labelled_unreachable() {
    let (mut m, _rx) = model();

    m.update(Input::App(Msg::Status(Err(HiveError::Refused {
        reason: "agent \"ghost\" is not managed by this hive".to_owned(),
    }))));
    assert_eq!(
        m.hive,
        Hive::Error {
            reason: "agent \"ghost\" is not managed by this hive".to_owned()
        }
    );

    m.update(Input::App(Msg::Status(Err(HiveError::Protocol {
        reason: "unparseable answer: expected value".to_owned(),
    }))));
    assert!(
        matches!(m.hive, Hive::Error { .. }),
        "an unusable answer is still a reachable hive: {:?}",
        m.hive
    );

    // …and the genuinely unreachable case keeps its own state.
    m.update(Input::App(Msg::Status(Err(HiveError::Unreachable {
        reason: "no socket — hive-c0re is not running".to_owned(),
    }))));
    assert!(matches!(m.hive, Hive::Unreachable { .. }), "{:?}", m.hive);
}

/// A row whose name hyperhive's own `Ident` would refuse never becomes a row —
/// every button on it would address the daemon with a name it rejects at
/// deserialize time, so the row would render live controls that cannot work.
///
/// Falsification: widen `AgentName::parse` back to spec §11.2's
/// `[A-Za-z0-9_-]` and this goes red.
#[test]
fn a_row_named_outside_hyperhives_ident_charset_is_dropped() {
    let (mut m, _rx) = model();
    m.update(status(vec![
        row("good-agent", false, false),
        row("Agent_9", false, false),
        row("SHOUTING", false, false),
    ]));
    let names: Vec<&str> = m.hive.agents().iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["good-agent"],
        "only names hyperhive's Ident accepts may render"
    );
}

/// §11 rule two: a name that fails the whitelist never becomes a row, and the
/// legitimate rows around it survive.
///
/// Falsification: drop the `AgentName::parse` guard in `fold_status` and the
/// roster grows to two, putting `../etc/passwd` into a node id.
#[test]
fn a_row_whose_name_fails_the_whitelist_is_dropped_not_rendered() {
    let (mut m, _rx) = model();
    let rows = roster("agent_status_bad_name.json");
    assert_eq!(rows.len(), 2, "the fixture carries one good and one bad");
    m.update(status(rows));
    let agents = m.hive.agents();
    assert_eq!(agents.len(), 1, "the illegal name must not become a row");
    assert_eq!(agents[0].name.as_str(), "good");
}

/// The §6.2 precedence fixture, folded end to end: five agents, five states,
/// in the hive's own order.
#[test]
fn the_precedence_fixture_renders_one_state_per_row() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_precedence.json")));
    let got: Vec<(&str, Status)> = m
        .hive
        .agents()
        .iter()
        .map(|a| (a.name.as_str(), a.status()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("wedged", Status::Failed),
            ("locked-out", Status::NeedsLogin),
            ("parked", Status::Paused),
            ("off", Status::Stopped),
            ("busy", Status::Running),
        ]
    );
    // The harness text is the running row's second line, verbatim.
    assert_eq!(m.hive.agents()[4].status_line(), "running the P1 gate");
    // …and the badge is orthogonal: `off` is stopped AND needs an update.
    assert!(m.hive.agents()[3].needs_update());
}

// ── the pause button ─────────────────────────────────────────────────────────

/// A click on `pause:<name>` emits exactly one `SetPaused` with the flipped
/// bool, and the row flips optimistically while it is in flight.
#[test]
fn a_pause_click_emits_one_set_paused_with_the_flipped_bool() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    // `trollshell-choom` is running and unpaused → the click asks to pause.
    let fx = m.update(click("pause:trollshell-choom"));
    assert!(fx.is_empty(), "pause is a command, not a shell effect");
    assert_eq!(
        lines(&mut rx),
        vec![r#"{"cmd":"set_paused","name":"trollshell-choom","paused":true}"#.to_owned()]
    );
    assert!(
        m.hive.agent(&name("trollshell-choom")).unwrap().paused(),
        "the row flips optimistically"
    );

    // `nixos-choom` is already paused → the click asks to resume.
    m.update(click("pause:nixos-choom"));
    assert_eq!(
        lines(&mut rx),
        vec![r#"{"cmd":"set_paused","name":"nixos-choom","paused":false}"#.to_owned()]
    );
}

/// The optimistic flip lasts exactly until the next poll answers — **whatever
/// it says**. A write the hive refused must not leave a lie on screen.
///
/// Falsification: make `fold_status` preserve `pending_paused` when the poll
/// disagrees, and the second half of this test fails with the row still
/// claiming "paused" over a hive that says otherwise.
#[test]
fn an_optimistic_flip_is_cleared_by_the_next_poll_even_when_it_disagrees() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    m.update(click("pause:trollshell-choom"));
    let _ = lines(&mut rx);
    assert!(m.hive.agent(&name("trollshell-choom")).unwrap().paused());

    // The hive answers with the SAME roster — i.e. the write did not land.
    m.update(status(roster("agent_status_grouped.json")));
    assert!(
        !m.hive.agent(&name("trollshell-choom")).unwrap().paused(),
        "the row must reconcile to the hive, not to the click"
    );
}

/// A click on an agent the roster does not carry is inert — no frame, no
/// panic. (The id came back over a socket; it is not trusted.)
#[test]
fn a_click_on_an_unknown_or_illegal_agent_sends_nothing() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    for id in [
        "pause:ghost",
        "pause:../etc/passwd",
        "pause:has space",
        "start:ghost-but-illegal!",
        "nonsense",
        "",
    ] {
        m.update(click(id));
    }
    assert!(lines(&mut rx).is_empty());
}

// ── the card's two buttons ───────────────────────────────────────────────────

/// **The row itself is not a click target** (Annika, 2026-09-11): her spec is
/// "click on agent opens agent page in trollshell-webview", which is #950 and
/// does not exist yet, so nothing on the card answers to the row — only the two
/// buttons do.
///
/// This asserts the *absence* through the reducer rather than through the tree,
/// because the tree half is `view.rs`'s
/// `a_card_row_is_two_lines_with_exactly_the_mocks_two_buttons`: even if a name
/// button came back, no arm here would route it. The old `chat:` prefix is the
/// one that used to.
///
/// Falsification: re-add a `chat:`/row-activate arm pointing at `open_detail`
/// and the first assertion reds.
#[test]
fn the_row_itself_is_not_a_click_target_until_the_webview_exists() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    for retired in ["chat:stray", "details:stray", "stray"] {
        assert_eq!(
            m.update(click(retired)),
            vec![],
            "{retired} must route nowhere"
        );
    }
    assert_eq!(m.selected, None, "and must not select anything");
    assert!(lines(&mut rx).is_empty(), "nor ask the hive anything");
}

/// The **edit** button — Annika's `[optionsedit]` — opens the agent's
/// companion window **on its settings tab**, and asks the hive nothing.
///
/// Her call on #947, 2026-09-11 07:43Z: the pen opens that window's settings
/// tab, so an agent has one surface. The launch is **detached** (#953), so the
/// window outlives a `trollshell.service` restart instead of dying with it.
///
/// Falsification: swap the tab word, drop `--tab settings`, or reorder the
/// flags, and the argv assertion reds; point the arm back at `open_detail` and
/// the effect kind does.
#[test]
fn the_edit_button_opens_the_companion_window_on_its_settings_tab() {
    let (mut m, mut rx) = model();
    m.set_window_probe(Probe::fixed(true));
    m.update(status(roster("agent_status_grouped.json")));

    let fx = m.update(click("edit:stray"));
    assert_eq!(
        fx,
        vec![Effect::RunCommand {
            id: 0,
            argv: vec![
                "trollshell-agent-window".to_owned(),
                "--agent".to_owned(),
                "stray".to_owned(),
                "--tab".to_owned(),
                "settings".to_owned(),
            ],
            detached: true,
        }]
    );
    assert_eq!(
        m.selected, None,
        "the window IS the settings surface now — no drawer page opens behind it"
    );
    assert!(
        lines(&mut rx).is_empty(),
        "opening a window is not a hive request"
    );
}

/// On a desktop with **no** companion window installed, the pen keeps P1's
/// behaviour: this plugin's own drawer page, which was the placeholder for
/// that window and is now its fallback.
///
/// Which of the two routes a click takes is a property of the *desktop*, not
/// of the click, which is why every test in this section states which desktop
/// it describes (`Agents::set_window_probe`) instead of inheriting the
/// machine's `PATH`.
///
/// Falsification: drop the fallback (return `Vec::new()` when the window is
/// absent) and this reds — such a desktop would have a dead pen.
#[test]
fn without_the_window_the_edit_button_still_opens_this_agents_page() {
    let (mut m, mut rx) = model();
    m.set_window_probe(Probe::fixed(false));
    m.update(status(roster("agent_status_grouped.json")));

    let fx = m.update(click("edit:stray"));
    assert_eq!(fx, vec![Effect::OpenPage(Page::PluginSelf)]);
    assert_eq!(
        m.selected
            .as_ref()
            .map(hytte_plugin_agents::model::AgentName::as_str),
        Some("stray")
    );
    assert!(
        lines(&mut rx).is_empty(),
        "opening a page is not a hive request"
    );
}

/// The card's lifecycle button sends the hive's **own** `Start`/`Stop` verbs,
/// scoped to one agent — not `SetPaused`.
///
/// The two are different verbs on `host.sock`, and Annika's mock names
/// `[startstop]`. `SetPaused` is unchanged and still reachable, from the drawer
/// page's pause control.
///
/// This pins the **frames**, not which button a row draws: it clicks the ids
/// directly, so `lifecycle_affordance`'s state→verb choice is deliberately not
/// falsified here — `view.rs`'s
/// `a_card_row_is_two_lines_with_exactly_the_mocks_two_buttons` is where that
/// lives, and it does red on it (verified). Splitting it that way is the point:
/// the view decides which verb is offered, the reducer decides what the verb
/// sends, and a mutation to either has exactly one home.
///
/// Falsification: point either arm at `SetPaused`, or widen the scope past one
/// agent, and both frames red.
#[test]
fn the_lifecycle_button_starts_a_stopped_agent_and_stops_a_running_one() {
    let (mut m, mut rx) = model();
    // `stray` is the fixture's stopped row; `trollshell-choom` is running.
    m.update(status(roster("agent_status_grouped.json")));

    m.update(click("start:stray"));
    m.update(click("stop:trollshell-choom"));
    assert_eq!(
        lines(&mut rx),
        vec![
            serde_json::to_string(&Request::Start {
                scope: Scope::agent("stray")
            })
            .expect("serializes"),
            serde_json::to_string(&Request::Stop {
                scope: Scope::agent("trollshell-choom"),
                graceful: true,
            })
            .expect("serializes"),
        ]
    );
}

/// `agents-back` — the panel's "all agents" button — clears the selection.
///
/// It is the only way out of an agent page, and since #963's review it is also
/// what rescues a selection the roster cannot resolve (a hive that went down
/// with an agent page open), so it needs coverage of its own rather than
/// riding along inside another test's tail.
///
/// Falsification: drop the `node == view::BACK_ID` arm from `click` and this
/// goes red.
#[test]
fn the_all_agents_button_clears_the_selection() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    m.update(click("edit:stray"));
    assert!(m.selected.is_some());

    let fx = m.update(click("agents-back"));
    assert_eq!(fx, vec![], "going back opens nothing; it is already open");
    assert_eq!(m.selected, None);
    assert!(lines(&mut rx).is_empty(), "and asks the hive nothing");
}

/// The card's title row is the one thing that jumps to the drawer, and it lands
/// on the **hive overview**, not on whatever agent was last selected.
///
/// Falsification: drop the `view::OVERVIEW_ID` arm from `click` and this goes
/// red.
#[test]
fn the_title_row_button_opens_the_drawer_at_the_hive_overview() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    m.update(click("edit:stray"));
    assert!(m.selected.is_some());

    let fx = m.update(click("agents-overview"));
    assert_eq!(fx, vec![Effect::OpenPage(Page::PluginSelf)]);
    assert_eq!(m.selected, None, "the overview is not an agent's page");
    assert!(lines(&mut rx).is_empty());
}

/// A selection whose agent leaves the roster falls back to the overview rather
/// than pinning a page to something that no longer exists.
#[test]
fn a_selection_that_vanishes_falls_back_to_the_overview() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    m.update(click("edit:stray"));
    assert!(m.selected.is_some());

    m.update(status(roster("agent_status_precedence.json")));
    assert_eq!(m.selected, None);
}

// ── the panel's start / stop ─────────────────────────────────────────────────

/// Spec §11 rule one, through the reducer: the frames a panel click produces
/// scope exactly one agent.
#[test]
fn the_panel_start_and_stop_scope_exactly_one_agent() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    m.update(click("start:stray"));
    m.update(click("stop:stray"));
    assert_eq!(
        lines(&mut rx),
        vec![
            r#"{"cmd":"start","scope":{"agent_names":["stray"]}}"#.to_owned(),
            r#"{"cmd":"stop","scope":{"agent_names":["stray"]},"graceful":true}"#.to_owned(),
        ]
    );
}

// ── §8's edge-triggered Notify ───────────────────────────────────────────────

fn row(name: &str, failed: bool, needs_login: bool) -> AgentStatusRow {
    AgentStatusRow {
        name: name.to_owned(),
        running: !failed,
        failed,
        needs_login,
        ..AgentStatusRow::default()
    }
}

/// Two identical polls raise **one** toast, not two — §8's "edge only, never a
/// level: a hive with one wedged agent must not toast every 5 s".
///
/// Falsification: replace the `prev_alarms` diff with a level check
/// (`if now.failed { … }`) and the second poll starts toasting too.
#[test]
fn notify_fires_on_the_edge_and_two_identical_polls_toast_once() {
    let (mut m, _rx) = model();

    // First sighting: healthy, and no toast (nothing has transitioned).
    assert!(m.update(status(vec![row("a", false, false)])).is_empty());

    // The edge.
    let fx = m.update(status(vec![row("a", true, false)]));
    assert_eq!(fx.len(), 1, "one toast on the edge");
    match &fx[0] {
        Effect::Notify { summary, body } => {
            assert!(summary.contains('a'), "{summary}");
            assert!(summary.contains("failed"), "{summary}");
            assert!(!body.is_empty());
        }
        other => panic!("expected Notify, got {other:?}"),
    }

    // The level. Same flags, three more polls, zero toasts.
    for _ in 0..3 {
        assert!(
            m.update(status(vec![row("a", true, false)])).is_empty(),
            "a steady failed agent must not re-toast"
        );
    }

    // Recovering and failing again is a NEW edge, so it toasts again.
    assert!(m.update(status(vec![row("a", false, false)])).is_empty());
    assert_eq!(m.update(status(vec![row("a", true, false)])).len(), 1);
}

/// `needs_login` is its own edge, with its own wording — and both flags
/// flipping at once raise both toasts.
#[test]
fn needs_login_is_its_own_edge_and_both_can_fire_together() {
    let (mut m, _rx) = model();
    m.update(status(vec![row("a", false, false)]));

    let fx = m.update(status(vec![row("a", false, true)]));
    assert_eq!(fx.len(), 1);
    match &fx[0] {
        Effect::Notify { summary, .. } => assert!(summary.contains("needs login"), "{summary}"),
        other => panic!("expected Notify, got {other:?}"),
    }

    m.update(status(vec![row("a", false, false)]));
    let both = m.update(status(vec![row("a", true, true)]));
    assert_eq!(both.len(), 2, "two independent edges, two toasts");
}

/// A first sighting is not a transition — otherwise every reconnect would
/// toast the whole roster, which is the louder bug (§8's edge rule).
#[test]
fn a_first_sighting_of_an_already_failed_agent_is_silent() {
    let (mut m, _rx) = model();
    assert!(
        m.update(status(vec![row("a", true, true)])).is_empty(),
        "a wedged agent seen for the first time must not toast"
    );
}

/// The toast names the agent by its **display label** when one is configured,
/// because that is what the row shows.
#[test]
fn the_toast_uses_the_configured_display_label() {
    let (mut m, _rx) = model();
    m.update(Input::App(Msg::Config(Box::new(toml_config(
        "[display.a]\nlabel = \"choom\"\n",
    )))));
    m.update(status(vec![row("a", false, false)]));
    let fx = m.update(status(vec![row("a", true, false)]));
    match &fx[0] {
        Effect::Notify { summary, .. } => assert!(summary.starts_with("choom"), "{summary}"),
        other => panic!("expected Notify, got {other:?}"),
    }
}

fn toml_config(body: &str) -> hytte_plugin_agents::config::AgentsConfig {
    hytte_config::subsystem::assemble::<hytte_plugin_agents::config::AgentsConfig>(&[(
        std::path::PathBuf::from("overlay.toml"),
        body.to_owned(),
    )])
    .expect("the test config assembles")
    .config
}

// ── group expanders ──────────────────────────────────────────────────────────

/// A group's expander is **plugin-driven**: the host never self-toggles, so
/// the model is the single source of truth for what is open
/// (`crates/hytte-plugin-proto/src/wire.rs:365-405`). Clicking the header
/// flips it, and the flip survives the next poll — otherwise a group the
/// operator collapsed would spring open every two seconds.
///
/// Falsification: drop the `ids::GROUP` arm from `click` and the first
/// assertion goes red; clear `expanded` in `fold_status` and the last one does.
#[test]
fn a_group_header_click_toggles_it_and_the_choice_survives_a_poll() {
    let (mut m, mut rx) = model();
    m.update(Input::App(Msg::Config(Box::new(toml_config(
        "[display.trollshell-choom]\nproject = \"viberoot\"\n[display.nixos-choom]\nproject = \"nixos\"\n",
    )))));
    m.update(status(roster("agent_status_grouped.json")));

    // A group with a running agent defaults open; one click collapses it.
    m.update(click("group:viberoot"));
    assert_eq!(m.expanded.get("viberoot"), Some(&false));
    assert!(
        lines(&mut rx).is_empty(),
        "a header click asks the hive nothing"
    );

    // And it stays collapsed across polls.
    m.update(status(roster("agent_status_grouped.json")));
    assert_eq!(
        m.expanded.get("viberoot"),
        Some(&false),
        "a collapsed group must not spring open on the next poll"
    );

    // Clicking again re-opens it.
    m.update(click("group:viberoot"));
    assert_eq!(m.expanded.get("viberoot"), Some(&true));
}

// ── a refused write ──────────────────────────────────────────────────────────

/// A write the hive refused raises **exactly one** toast, naming what was
/// attempted and carrying the daemon's own reason.
///
/// Without it the operator sees a pause button that flips, snaps back on the
/// next poll, and explains nothing — which on the two failures most likely in
/// practice (a permission problem, or `agent "…" is not managed`) reads as a
/// broken button rather than as a refusal.
///
/// Falsification: drop the `Msg::WriteRefused` send from `poll.rs`'s `Err`
/// arm, or the `Input::App(Msg::WriteRefused)` arm from `update`, and this
/// goes red.
#[test]
fn a_refused_write_raises_exactly_one_explaining_toast() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    let fx = m.update(Input::App(Msg::WriteRefused {
        request: Request::SetPaused {
            name: "trollshell-choom".to_owned(),
            paused: true,
        },
        reason: "agent \"trollshell-choom\" is not managed by this hive".to_owned(),
    }));
    assert_eq!(fx.len(), 1, "one refusal, one toast");
    match &fx[0] {
        Effect::Notify { summary, body } => {
            assert!(summary.contains("refused"), "{summary}");
            assert!(
                summary.contains("pause"),
                "the toast must name the attempt: {summary}"
            );
            assert!(summary.contains("trollshell-choom"), "{summary}");
            assert!(
                body.contains("not managed"),
                "the hive's own words carry the why: {body}"
            );
        }
        other => panic!("expected Notify, got {other:?}"),
    }
}

/// The toast phrases each verb the way the row does — including `resume` for
/// an un-pause, and the configured **display label** rather than the hive's
/// raw name, so it matches the row the operator just clicked.
#[test]
fn the_refusal_toast_phrases_the_verb_and_uses_the_display_label() {
    let (mut m, _rx) = model();
    m.update(Input::App(Msg::Config(Box::new(toml_config(
        "[display.trollshell-choom]\nlabel = \"choom\"\n",
    )))));
    m.update(status(roster("agent_status_grouped.json")));

    let cases = [
        (
            Request::SetPaused {
                name: "trollshell-choom".to_owned(),
                paused: false,
            },
            "resume choom",
        ),
        (
            Request::Start {
                scope: Scope::agent("trollshell-choom"),
            },
            "start choom",
        ),
        (
            Request::Stop {
                scope: Scope::agent("trollshell-choom"),
                graceful: true,
            },
            "stop choom",
        ),
        (
            Request::Restart {
                name: "trollshell-choom".to_owned(),
            },
            "restart choom",
        ),
    ];
    for (request, want) in cases {
        let fx = m.update(Input::App(Msg::WriteRefused {
            request,
            reason: "nope".to_owned(),
        }));
        match &fx[0] {
            Effect::Notify { summary, .. } => {
                assert!(summary.contains(want), "expected {want:?} in {summary:?}");
            }
            other => panic!("expected Notify, got {other:?}"),
        }
    }
}

// ── visibility ───────────────────────────────────────────────────────────────

/// The visibility push is forwarded down the lane so the poll task can park —
/// the #305 gate that makes `StateKey::SlotVisible` worth subscribing.
///
/// Falsification: drop the `Input::SlotVisible` arm and the poller never
/// parks (and never wakes on open).
#[test]
fn the_visibility_edge_reaches_the_poll_task() {
    let (mut m, mut rx) = model();
    m.update(Input::SlotVisible(true));
    m.update(Input::SlotVisible(false));
    assert_eq!(
        cmds(&mut rx),
        vec![Cmd::SetVisible(true), Cmd::SetVisible(false)]
    );
}

/// The manifest is the plugin's whole trust declaration, so it is pinned:
/// `OpenPage` + `Notify` + `OpenUri` + `RunCommand` + `Consent` and nothing
/// else. `RunCommand` arrived with P2's companion window (#950) and is the
/// highest-trust capability in the vocabulary, which is why the one argv this
/// plugin can build is a constant binary plus a re-parsed `AgentName`
/// (`window::argv`). **`Consent` arrived with #947 P3** — withheld through P1
/// and P2 on purpose (spec §11 rules two and three, §13), so the row was
/// trustworthy before it could raise a modal that approves a config change.
///
/// Declaring it is load-bearing twice: without it the SDK drops every
/// `RequestConsent` this plugin emits (#1058), *and* the host never pushes the
/// `ConsentDecision` back (the #305 opt-in), so the prompt would neither appear
/// nor be answerable.
#[test]
fn the_manifest_declares_exactly_five_capabilities_and_no_secrets() {
    use hytte_plugin::proto::{Capability, Mount, StateKey};
    let m = Agents::manifest();
    assert_eq!(m.id, "agents");
    assert_eq!(m.mount, Mount::SidebarTop);
    assert_eq!(
        m.capabilities,
        vec![
            Capability::OpenPage,
            Capability::Notify,
            Capability::OpenUri,
            Capability::RunCommand,
            Capability::Consent,
        ]
    );
    // Still nothing that reads the operator's own data: an approval prompt is
    // the hive asking about itself.
    for denied in [
        Capability::Calendar,
        Capability::SessionState,
        Capability::NowPlaying,
        Capability::DatasourceProvider,
    ] {
        assert!(!m.capabilities.contains(&denied), "{denied:?}");
    }
    assert_eq!(
        m.subscribes,
        vec![StateKey::Clock, StateKey::SlotVisible],
        "SlotVisible is the #305 gate for the park; Clock drives the panel ages"
    );
    assert!(m.provides.is_empty(), "this plugin serves no datasource");
}

// ── the agent-page button: the companion window (#950), the browser behind it
//    (#1045) ──────────────────────────────────────────────────────────────────

/// The manifest grants the capability the agent-page click's effect
/// **requires** — asked of the one table that decides it, not restated as a
/// list, and asked on **both** desktops, because since #950 the click emits a
/// different effect depending on whether the companion window is installed.
///
/// `Effect::required_capability` (`hytte-plugin-proto/src/effect.rs`) is the
/// single mapping both enforcement points consult: the host's
/// `session::enforce_capabilities` and, since #1058, the SDK's own
/// `drop_ungranted_effects` (`hytte-plugin/src/runtime.rs`), whose predicate is
/// literally `granted.contains(&effect.required_capability())` — which is what
/// this asserts, over this plugin's real manifest and a real emitted effect.
///
/// So the assertion is about a **consequence**, not about a spelling: with the
/// capability declared the effect survives to the wire; without it the SDK
/// drops it before framing (#1084) and the click silently does nothing on a
/// current host, while on a host older than #1045 `Register` still decodes and
/// the first frame carrying the effect does not — the #437 crash-loop.
///
/// Mutation (verified red): delete `Capability::OpenUri` **or**
/// `Capability::RunCommand` from `manifest()` and one of the two rounds here
/// goes red, along with the manifest test above.
#[test]
fn the_manifest_grants_what_the_agent_page_click_emits() {
    for installed in [true, false] {
        let (mut m, _rx) = model();
        m.set_window_probe(Probe::fixed(installed));
        m.update(status(roster("agent_status_grouped.json")));

        let fx = m.update(click("open:trollshell-choom"));
        let [effect] = fx.as_slice() else {
            panic!("a click emits exactly one effect, got {fx:?} (window installed: {installed})");
        };
        let required = effect
            .required_capability()
            .expect("both routes are gated effects");

        let granted = Agents::manifest().capabilities;
        assert!(
            granted.contains(&required),
            "the SDK drops an effect whose capability the manifest omits (#1058): \
             {effect:?} needs {required:?}, manifest grants {granted:?}"
        );
    }
}

/// With the companion window installed, the agent-page click launches **it**,
/// detached, for that agent — not the browser.
///
/// Annika on #947, 2026-09-11 07:16Z: the agent page opens in "a dedicated,
/// shell-controlled webview, not the browser — the point is that we control it
/// so it can be integrated". So the effect is a detached `RunCommand` (#953)
/// naming the window binary and the agent, and **nothing else**: the URL is not
/// carried, because the window reads `host.sock` itself and would be wrong to
/// trust a string that made a round trip through the host.
///
/// Falsification: swap the argv order, emit `--tab agent` (the window's
/// default, which #950 spells by omission), or drop `detached: true` — each
/// reds on the exact effect.
#[test]
fn the_agent_page_click_launches_the_companion_window() {
    let (mut m, mut rx) = model();
    m.set_window_probe(Probe::fixed(true));
    m.update(status(roster("agent_status_grouped.json")));

    let fx = m.update(click("open:trollshell-choom"));
    assert_eq!(
        fx,
        vec![Effect::RunCommand {
            id: 0,
            argv: vec![
                "trollshell-agent-window".to_owned(),
                "--agent".to_owned(),
                "trollshell-choom".to_owned(),
            ],
            detached: true,
        }]
    );
    assert!(
        lines(&mut rx).is_empty(),
        "opening a window is not a hive request"
    );
    assert_eq!(m.selected, None, "and it neither selects nor opens a page");
}

/// Without the window, the click keeps P1's route: **exactly one** `OpenUri`,
/// carrying that row's own URL, and asks the hive nothing.
///
/// This is the fallback #950 asks for by name — "so a plugin without the window
/// still opens the browser" — and it is why the window is resolved on `PATH`
/// *before* the effect is chosen rather than after the host answers: a detached
/// launch reports only that the launch succeeded, and on the `systemd-run` path
/// a binary that does not exist still produces `ok: true`
/// (`trollshell/src/plugins/effects.rs`), so an `EffectResult`-driven fallback
/// would never fire.
///
/// The URL is the fixture's, read back out of the model — not a string the
/// node id carried. Falsification: have `open_agent_page` parse a URL out of
/// `node` instead of looking the agent up and the assertion still passes for
/// this id but the vanished-agent sibling below goes red.
#[test]
fn without_the_window_the_agent_page_click_opens_one_uri_with_the_rows_own_url() {
    let (mut m, mut rx) = model();
    m.set_window_probe(Probe::fixed(false));
    m.update(status(roster("agent_status_grouped.json")));

    let fx = m.update(click("open:trollshell-choom"));
    assert_eq!(
        fx,
        vec![Effect::OpenUri {
            id: 0,
            uri: "https://hive.local/agent/trollshell-choom/".to_owned(),
        }]
    );
    assert!(
        lines(&mut rx).is_empty(),
        "following a link is not a hive request"
    );
    assert_eq!(m.selected, None, "and it neither selects nor opens a page");
}

/// Two clicks take **distinct** correlation tokens — on either desktop, and
/// out of the *same* counter when the two routes are mixed.
///
/// `EffectResult`'s own docs call the reply-bearing effects one shared id space
/// and say to allocate from a single counter (#1060): two effects in flight on
/// the same id cannot be told apart by the host's audit or by the plugin's own
/// result arm. Since #950 that is no longer hypothetical — `RunCommand` and
/// `OpenUri` are both live in this plugin — so the mixed round is the one that
/// would catch a per-kind counter.
///
/// Falsification: make `take_effect_id` return a constant, reset it per click,
/// or give `open_window` a counter of its own, and one of the rounds reds.
#[test]
fn clicks_take_distinct_correlation_ids_across_both_routes() {
    for installed in [true, false] {
        let (mut m, _rx) = model();
        m.set_window_probe(Probe::fixed(installed));
        m.update(status(roster("agent_status_grouped.json")));

        let ids: Vec<u64> = ["open:trollshell-choom", "open:nixos-choom"]
            .into_iter()
            .map(|node| match m.update(click(node)).as_slice() {
                [Effect::OpenUri { id, .. } | Effect::RunCommand { id, .. }] => *id,
                other => panic!("expected one reply-bearing effect, got {other:?}"),
            })
            .collect();
        assert_eq!(ids, vec![0, 1], "window installed: {installed}");
    }

    // The mixed case: a window launch and a browser open out of one model.
    let (mut m, _rx) = model();
    m.set_window_probe(Probe::fixed(true));
    m.update(status(roster("agent_status_grouped.json")));
    let launch = match m.update(click("open:trollshell-choom")).as_slice() {
        [Effect::RunCommand { id, .. }] => *id,
        other => panic!("expected a launch, got {other:?}"),
    };
    m.set_window_probe(Probe::fixed(false));
    let opened = match m.update(click("open:nixos-choom")).as_slice() {
        [Effect::OpenUri { id, .. }] => *id,
        other => panic!("expected an open, got {other:?}"),
    };
    assert_ne!(
        launch, opened,
        "one id space, not one counter per effect kind (#1060)"
    );
}

/// A click whose agent the model no longer holds — or whose name is illegal —
/// opens **nothing**, on either desktop.
///
/// This is what makes "the destination comes from the model, never from the id"
/// a testable property rather than a comment: the id is well-formed and the
/// prefix matches, and the only reason nothing is emitted is that there is no
/// agent behind it. The window route is held to the same rule even though it
/// carries no URL — a window for an agent this roster does not have is a window
/// with nothing to show.
///
/// Falsification: carry the URL on the node id (`open:<url>`) and emit it
/// verbatim, or drop `open_window`'s `self.hive.agent(name).is_none()` guard,
/// and this goes red.
#[test]
fn a_click_on_a_vanished_or_illegal_agent_opens_nothing() {
    for installed in [true, false] {
        let (mut m, mut rx) = model();
        m.set_window_probe(Probe::fixed(installed));
        m.update(status(roster("agent_status_grouped.json")));

        assert_eq!(m.update(click("open:ghost")), vec![], "no such agent");
        assert_eq!(
            m.update(click("open:not a name")),
            vec![],
            "the name fails the §11 whitelist before anything else happens"
        );
        assert!(lines(&mut rx).is_empty(), "window installed: {installed}");
    }

    // …and an agent the hive reports with no `url` at all: nothing to open in
    // the browser. The window route does not need one, so it still launches.
    let (mut m, _rx) = model();
    m.set_window_probe(Probe::fixed(false));
    m.update(status(vec![AgentStatusRow {
        name: "trollshell-choom".to_owned(),
        running: true,
        ..AgentStatusRow::default()
    }]));
    assert_eq!(m.update(click("open:trollshell-choom")), vec![]);

    let (mut m, _rx) = model();
    m.set_window_probe(Probe::fixed(true));
    m.update(status(vec![AgentStatusRow {
        name: "trollshell-choom".to_owned(),
        running: true,
        ..AgentStatusRow::default()
    }]));
    assert!(
        matches!(
            m.update(click("open:trollshell-choom")).as_slice(),
            [Effect::RunCommand { .. }]
        ),
        "the window resolves the URL from host.sock itself, so a row without \
         one is still openable there"
    );
}

/// The panel's `dashboard` link opens the hive's own root, and only once the
/// `Urls` answer has landed.
///
/// Falsification: point the `OPEN_DASHBOARD` arm at the agent lookup (or drop
/// the `is_empty` filter) and one of the two halves reds.
#[test]
fn the_dashboard_link_opens_the_hives_root_once_urls_have_landed() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    assert_eq!(
        m.update(click("open-dashboard")),
        vec![],
        "nothing to open before the hive said where home is"
    );

    m.update(Input::App(Msg::Urls(Box::new(HiveUrls {
        domain: Some("hive.local".to_owned()),
        home: Some("  https://hive.local/  ".to_owned()),
    }))));
    assert_eq!(
        m.update(click("open-dashboard")),
        vec![Effect::OpenUri {
            id: 0,
            uri: "https://hive.local/".to_owned(),
        }],
        "and the stored value is trimmed, not passed through with its padding"
    );
}

/// A refused open **says so**; a successful one says nothing — and the same
/// arm covers a refused window launch, which is why its summary names neither
/// a link nor a window.
///
/// `EffectOutcome::output` exists for exactly this (`hytte-plugin`'s
/// `Input::EffectResult` docs: "so a plugin can toast it instead of leaving a
/// click that silently does nothing"). Two reply-bearing effects are live here
/// since #950 and there is still no correlation table: both mean "the thing you
/// clicked did not appear", and the host's own sentence is the useful half.
///
/// Falsification: drop the `!outcome.ok` guard and the success half reds; drop
/// the arm entirely and both refusal halves do.
#[test]
fn a_refused_open_or_launch_toasts_its_reason_and_a_successful_one_is_silent() {
    let (mut m, _rx) = model();

    let refused = m.update(Input::EffectResult {
        id: 0,
        outcome: EffectOutcome {
            ok: false,
            output: Some("refused scheme: ftp".to_owned()),
        },
    });
    assert_eq!(
        refused,
        vec![Effect::Notify {
            summary: "couldn't open that".to_owned(),
            body: "refused scheme: ftp".to_owned(),
        }]
    );

    // The same arm, a launch's own failure sentence (#953's `launch_outcome`).
    let launch_failed = m.update(Input::EffectResult {
        id: 1,
        outcome: EffectOutcome {
            ok: false,
            output: Some(
                "launch failed: spawning trollshell-agent-window: No such file or directory"
                    .to_owned(),
            ),
        },
    });
    assert_eq!(
        launch_failed,
        vec![Effect::Notify {
            summary: "couldn't open that".to_owned(),
            body: "launch failed: spawning trollshell-agent-window: No such file or directory"
                .to_owned(),
        }]
    );

    assert_eq!(
        m.update(Input::EffectResult {
            id: 2,
            outcome: EffectOutcome {
                ok: true,
                output: None,
            },
        }),
        vec![],
        "the window (or the browser) appearing is the feedback; a toast on top of it is noise"
    );
}

fn name(s: &str) -> hytte_plugin_agents::model::AgentName {
    hytte_plugin_agents::model::AgentName::parse(s).expect("a legal test name")
}

// ── #947 P3: approvals — prompt, decide, time out, badge ─────────────────────
//
// The whole of spec §6.5's behaviour, driven with no socket, no host and no
// overlay: `Msg::Pending` in, `Effect::RequestConsent` out,
// `Input::ConsentDecision` in, a JSON line on the command lane out. Each test
// below names the mechanism it reds when deleted.

/// One pending approval, `status: Pending`, with the id doubling as a clock —
/// the queue orders by id, so a lower id is an older request.
fn approval(id: i64, agent: &str) -> Approval {
    Approval {
        id,
        agent: agent.to_owned(),
        kind: ApprovalKind::MergeConfigPr,
        requested_at: format!("2026-09-12T09:{id:02}:00Z"),
        status: ApprovalStatus::Pending,
        description: None,
    }
}

fn pending(queue: Vec<Approval>) -> Input<Msg> {
    Input::App(Msg::Pending(Ok(queue)))
}

/// A `Pending` round trip the hive refused — the lane that clears badges
/// without disturbing the prompt bookkeeping (#1140's review, LOW-3).
fn pending_refused() -> Input<Msg> {
    Input::App(Msg::Pending(Err(HiveError::Refused {
        reason: "permission denied".to_owned(),
    })))
}

/// Every `Button` id in the rendered card, in render order.
///
/// The reducer suite usually asserts on effects and socket frames; this exists
/// for the one #1140 finding whose symptom is a *missing* affordance — an
/// approval that prompts with no badge behind it has no recovery path at all.
fn button_ids(node: &hytte_plugin::proto::Node) -> Vec<String> {
    use hytte_plugin::proto::Node;

    fn walk(node: &Node, out: &mut Vec<String>) {
        if let Node::Button { id, .. } = node {
            out.push(id.clone());
        }
        match node {
            Node::Box { children, .. }
            | Node::Row { children, .. }
            | Node::ListBox { children, .. } => {
                for c in children {
                    walk(c, out);
                }
            }
            Node::Expander {
                header, children, ..
            } => {
                walk(header, out);
                for c in children {
                    walk(c, out);
                }
            }
            Node::Button { child, .. } | Node::Scrolled { child, .. } => walk(child, out),
            _ => {}
        }
    }

    let mut out = Vec::new();
    walk(node, &mut out);
    out
}

/// Every `RequestConsent` in a batch of effects, as
/// `(request_id, agent, scope, detail)` — the four strings the card renders.
fn prompts(fx: &[Effect]) -> Vec<(u64, String, String, String)> {
    fx.iter()
        .filter_map(|e| match e {
            Effect::RequestConsent {
                request_id,
                agent,
                scope,
                detail,
                ..
            } => Some((*request_id, agent.clone(), scope.clone(), detail.clone())),
            _ => None,
        })
        .collect()
}

/// The `request_id` of the single prompt in `fx`.
fn one_prompt(fx: &[Effect]) -> u64 {
    let raised = prompts(fx);
    assert_eq!(raised.len(), 1, "expected exactly one prompt, got {fx:?}");
    raised[0].0
}

/// A pending approval raises exactly one prompt, and a second poll carrying the
/// **same** approval raises none — spec §6.5's dedup, and the difference
/// between a modal and a modal every two seconds.
///
/// # Two mechanisms, and the second half is what separates them
///
/// Two things could produce "no second prompt", and a test that only polls
/// twice cannot tell which one did it: the **gate** (one card at a time, so
/// nothing else is raised while the first is open) and the **`prompted` set**
/// (this approval has already been asked about). Measured: deleting the
/// `prompted` insert leaves a poll-twice test green, because the gate alone
/// covers it.
///
/// So the second half answers the card first — which opens the gate — and then
/// polls again with the approval **still queued**, which is the real sequence
/// (the write has not landed yet, or the hive refused it). Only `prompted`
/// stops a second card there.
///
/// Falsification: delete the `prompted` insert in `Agents::raise`, or the
/// `oldest_unprompted` filter, and the last assertion goes red.
#[test]
fn a_pending_approval_prompts_once_and_not_again_on_the_next_poll() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    let first = m.update(pending(vec![approval(7, "trollshell-choom")]));
    let raised = prompts(&first);
    assert_eq!(raised.len(), 1, "{first:?}");
    assert_eq!(raised[0].1, "trollshell-choom", "the agent's label");
    assert_eq!(
        raised[0].2, "merge a reviewed config PR",
        "the kind, in English"
    );
    assert!(
        raised[0].3.contains("request #7"),
        "the detail names the request: {}",
        raised[0].3
    );
    let id = raised[0].0;

    // While the card is open: nothing new — that is the gate.
    let second = m.update(pending(vec![approval(7, "trollshell-choom")]));
    assert_eq!(prompts(&second), Vec::new(), "{second:?}");

    // Answered, so the gate is open — and the approval is still queued,
    // because the hive has not processed the write yet.
    m.update(Input::ConsentDecision {
        request_id: id,
        decision: ConsentDecision::AllowOnce,
    });
    assert_eq!(
        lines(&mut rx),
        vec![r#"{"cmd":"approve","id":7}"#.to_owned()]
    );

    let third = m.update(pending(vec![approval(7, "trollshell-choom")]));
    assert_eq!(
        prompts(&third),
        Vec::new(),
        "an approval already asked about must not be asked again: {third:?}"
    );
}

/// The card asks with the **two-button** choice set, because "This session" and
/// "Always" are not answers to a one-shot config merge — and because that
/// variant is what makes an unanswered prompt send nothing.
///
/// Falsification: pass `ConsentChoices::Grant` and this goes red; the overlay
/// would then draw four buttons and deny the approval after 60 s of silence.
#[test]
fn the_prompt_asks_for_the_approve_deny_card() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    let fx = m.update(pending(vec![approval(7, "trollshell-choom")]));
    match fx.as_slice() {
        [
            Effect::RequestConsent {
                choices,
                datasource,
                ..
            },
        ] => {
            assert_eq!(*choices, ConsentChoices::Approval);
            assert!(
                datasource.is_empty(),
                "an approval names no datasource: {datasource:?}"
            );
        }
        other => panic!("expected one RequestConsent, got {other:?}"),
    }
}

/// A second approval gets its own prompt — but only once the first is answered,
/// because the host has one consent window and a second `RequestConsent` would
/// replace the first card inside its own 60 s.
///
/// Falsification: delete the `if self.prompt.is_some()` gate in `raise_next`
/// and the second fold raises a prompt while the first is still on screen.
#[test]
fn a_second_approval_waits_for_the_first_answer_then_prompts() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    let queue = vec![approval(7, "trollshell-choom"), approval(8, "nixos-choom")];
    let first = m.update(pending(queue.clone()));
    let id = one_prompt(&first);

    // The second approval does not steal the card.
    let while_open = m.update(pending(queue.clone()));
    assert_eq!(prompts(&while_open), Vec::new(), "{while_open:?}");

    // Answering the first frees the gate…
    m.update(Input::ConsentDecision {
        request_id: id,
        decision: ConsentDecision::AllowOnce,
    });
    assert_eq!(
        lines(&mut rx),
        vec![r#"{"cmd":"approve","id":7}"#.to_owned()]
    );

    // …and the next poll raises the one behind it.
    let next = m.update(pending(queue));
    let raised = prompts(&next);
    assert_eq!(raised.len(), 1, "{next:?}");
    assert!(
        raised[0].3.contains("request #8"),
        "the *next* approval, not the answered one: {}",
        raised[0].3
    );
    assert_ne!(raised[0].0, id, "a fresh correlation token");
}

/// An approval that is no longer pending raises nothing — whether it left the
/// queue before the plugin ever saw it, or between two polls.
#[test]
fn an_approval_that_is_not_pending_never_prompts() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    let mut resolved = approval(7, "trollshell-choom");
    resolved.status = ApprovalStatus::Approved;
    // A caller that hands the model a non-pending row (the poll filters these,
    // so this is the model refusing to trust that).
    let fx = m.update(pending(vec![resolved]));
    assert_eq!(prompts(&fx), Vec::new(), "{fx:?}");
    assert_eq!(m.pending.all().len(), 0);
}

/// Spec §6.5's mapping: **every** affirmative becomes exactly one
/// `Approve { id }` for that one id, and nothing standing is persisted.
///
/// The four-button card only reaches this plugin on a host older than #947, so
/// the `AllowSession` / `AllowAlways` arms are what make that host degrade to
/// "approved once" rather than to nothing.
///
/// Falsification: map any `Allow*` to `Request::Deny` and the line goes red;
/// send it twice and the `lines` assertion counts two.
#[test]
fn every_affirmative_becomes_exactly_one_approve() {
    for decision in [
        ConsentDecision::AllowOnce,
        ConsentDecision::AllowSession,
        ConsentDecision::AllowAlways,
    ] {
        let (mut m, mut rx) = model();
        m.update(status(roster("agent_status_grouped.json")));
        let id = one_prompt(&m.update(pending(vec![approval(7, "trollshell-choom")])));

        m.update(Input::ConsentDecision {
            request_id: id,
            decision,
        });
        assert_eq!(
            lines(&mut rx),
            vec![r#"{"cmd":"approve","id":7}"#.to_owned()],
            "{decision:?}"
        );

        // The same token again must send nothing: `Approve` runs the action on
        // the far side, so it is not idempotent the way `SetPaused` is.
        m.update(Input::ConsentDecision {
            request_id: id,
            decision,
        });
        assert_eq!(lines(&mut rx), Vec::<String>::new(), "{decision:?} twice");
    }
}

/// A deny click becomes exactly one `Deny { id }`.
#[test]
fn a_deny_click_becomes_exactly_one_deny() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    let id = one_prompt(&m.update(pending(vec![approval(7, "trollshell-choom")])));

    m.update(Input::ConsentDecision {
        request_id: id,
        decision: ConsentDecision::Deny,
    });
    assert_eq!(lines(&mut rx), vec![r#"{"cmd":"deny","id":7}"#.to_owned()]);
}

/// A decision for a token this model is not waiting on is dropped — a
/// superseded card's late answer must not be applied to whatever is in flight
/// now.
#[test]
fn a_decision_for_an_unknown_token_writes_nothing() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    let id = one_prompt(&m.update(pending(vec![approval(7, "trollshell-choom")])));

    m.update(Input::ConsentDecision {
        request_id: id.wrapping_add(99),
        decision: ConsentDecision::AllowOnce,
    });
    assert_eq!(lines(&mut rx), Vec::<String>::new());
}

/// §6.5: an approval that disappears from `Pending` between the prompt and the
/// answer "is dropped with a debug line, not an error" — so the answer writes
/// nothing, and the gate is free again.
#[test]
fn an_approval_resolved_elsewhere_is_dropped_not_re_decided() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    let id = one_prompt(&m.update(pending(vec![approval(7, "trollshell-choom")])));

    // Somebody answered it on the dashboard.
    m.update(pending(Vec::new()));
    m.update(Input::ConsentDecision {
        request_id: id,
        decision: ConsentDecision::AllowOnce,
    });
    assert_eq!(lines(&mut rx), Vec::<String>::new());
}

/// **The timeout (default 2 on #947).** The overlay sends nothing at all when
/// an approval prompt goes unanswered, so the reducer simply never hears from
/// it: the approval stays pending, the badge stays, and the plugin writes
/// nothing to the hive.
///
/// This test *is* the shape of that silence — there is no input to feed,
/// because "nothing arrives" is the whole mechanism. What it pins is that the
/// model does not answer on its own behalf either.
///
/// Falsification: make `ConsentChoices::Approval.unanswered()` return
/// `Some(Deny)` (the overlay would then send a deny on the 60 s timeout) and
/// `overlays::consent`'s `golden_card_approval_*` goes red; make this reducer
/// deny on a timer of its own and the `lines` assertion here goes red.
#[test]
fn a_prompt_nobody_answers_writes_nothing_and_keeps_its_badge() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    let queue = vec![approval(7, "trollshell-choom")];
    m.update(pending(queue.clone()));

    // Sixty seconds of polls go by with no decision.
    for _ in 0..30 {
        let fx = m.update(pending(queue.clone()));
        assert_eq!(prompts(&fx), Vec::new(), "no re-prompt while it is open");
    }
    assert_eq!(
        lines(&mut rx),
        Vec::<String>::new(),
        "silence must never become a decision"
    );
    assert_eq!(
        m.pending.count_for("trollshell-choom"),
        1,
        "the approval is still waiting, so the badge is still there"
    );
}

/// The badge click re-raises the prompt for the **oldest** approval that agent
/// is waiting on — the recovery path for a timed-out card.
///
/// Falsification: make `raise_for` pick `last()` instead of the oldest and the
/// "request #6" assertion goes red.
#[test]
fn the_badge_click_re_raises_the_oldest_approval() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    let queue = vec![
        approval(6, "trollshell-choom"),
        approval(9, "trollshell-choom"),
    ];
    let first = m.update(pending(queue.clone()));
    let id = one_prompt(&first);
    assert!(prompts(&first)[0].3.contains("request #6"));

    // Nobody answered. The operator clicks the badge.
    let again = m.update(click("approvals:trollshell-choom"));
    let raised = prompts(&again);
    assert_eq!(raised.len(), 1, "{again:?}");
    assert!(
        raised[0].3.contains("request #6"),
        "the oldest, not the newest: {}",
        raised[0].3
    );
    assert_ne!(raised[0].0, id, "a fresh token, so the stale one is inert");
}

/// A badge click for an agent with nothing queued raises nothing — the click
/// raced a poll that emptied the queue.
#[test]
fn a_badge_click_with_an_empty_queue_raises_nothing() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    let fx = m.update(click("approvals:trollshell-choom"));
    assert_eq!(prompts(&fx), Vec::new(), "{fx:?}");
}

/// **The refusal (§6.5's "no silent loss").** The hive rejects the `Approve`;
/// the operator gets exactly one toast naming what failed, and the approval is
/// still pending, so the row is still badged.
///
/// Falsification: drop the `Approve`/`Deny` arms from `describe` (the toast
/// then says "the request") or return no effect for a refused write, and the
/// assertions here go red.
#[test]
fn a_refused_approve_toasts_once_and_keeps_the_approval_badged() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    m.update(pending(vec![approval(7, "trollshell-choom")]));

    let fx = m.update(Input::App(Msg::WriteRefused {
        request: Request::Approve { id: 7 },
        reason: "approval 7 is not pending".to_owned(),
    }));
    match fx.as_slice() {
        [Effect::Notify { summary, body }] => {
            assert!(summary.contains("approve"), "{summary}");
            assert!(summary.contains("#7"), "{summary}");
            assert!(
                summary.contains("merge a reviewed config PR"),
                "the toast names the action, not just a rowid: {summary}"
            );
            assert_eq!(body, "approval 7 is not pending");
        }
        other => panic!("expected exactly one Notify, got {other:?}"),
    }
    assert_eq!(
        m.pending.count_for("trollshell-choom"),
        1,
        "a refused write must not clear the approval"
    );
}

/// A refused `Deny` reads as a deny in the toast — the two must not be
/// indistinguishable, since one of them is destructive on the far side.
#[test]
fn a_refused_deny_says_deny() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    m.update(pending(vec![approval(7, "trollshell-choom")]));

    let fx = m.update(Input::App(Msg::WriteRefused {
        request: Request::Deny { id: 7 },
        reason: "permission denied".to_owned(),
    }));
    match fx.as_slice() {
        [Effect::Notify { summary, .. }] => {
            assert!(summary.starts_with("hive refused: deny"), "{summary}");
        }
        other => panic!("expected exactly one Notify, got {other:?}"),
    }
}

/// The correlation token comes from the **shared** effect counter, not a second
/// one — `Input::EffectResult`'s allocation contract (#1060), which this plugin
/// now exercises with three reply-bearing effect kinds at once.
///
/// Falsification: give `raise` its own counter starting at 0 and this finds the
/// same value used twice.
#[test]
fn the_prompt_token_shares_the_one_effect_id_space() {
    let (mut m, _rx) = model();
    m.set_window_probe(Probe::fixed(false));
    m.update(status(roster("agent_status_grouped.json")));

    // An `OpenUri` takes a token…
    let opened = m.update(click("open:trollshell-choom"));
    let uri_id = match opened.as_slice() {
        [Effect::OpenUri { id, .. }] => *id,
        other => panic!("expected one OpenUri, got {other:?}"),
    };
    // …and the prompt raised next must not reuse it.
    let prompt_id = one_prompt(&m.update(pending(vec![approval(7, "trollshell-choom")])));
    assert_ne!(uri_id, prompt_id);
}

/// The description is the sentence worth showing, but it is free text another
/// process wrote, so it is bounded before it reaches a 480 px card — and
/// bounded on a **char** boundary, so a multi-byte description cannot panic
/// the plugin.
#[test]
fn a_long_description_is_clamped_on_a_char_boundary() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    let mut a = approval(7, "trollshell-choom");
    a.description = Some("🦀".repeat(400));

    let fx = m.update(pending(vec![a]));
    let detail = prompts(&fx)[0].3.clone();
    let crabs = detail.chars().filter(|c| *c == '🦀').count();
    assert!(crabs <= 240, "clamped to the cap, got {crabs}");
    assert!(detail.contains('…'), "and says it was cut: {detail}");
    assert!(detail.contains("request #7"), "{detail}");
}

// ── #1140 review: the fix round ──────────────────────────────────────────────

/// Advance the model's clock, the way the host's `StateKey::Clock` push does.
fn clock(unix: i64) -> Input<Msg> {
    Input::Snapshot(hytte_plugin::proto::StateSnapshot {
        clock: Some(hytte_plugin::proto::ClockState {
            unix,
            iso: String::new(),
        }),
    })
}

/// **MEDIUM-1.** The review's probe, inverted: an ignored card must not mute
/// the queue behind it for the rest of the session.
///
/// The `Approval` card sends nothing when it is ignored, so `decide` never
/// runs; and the approval stays queued, which is default 2, so `fold_pending`
/// never clears the prompt either. Before the `raised_unix` stamp, that shut
/// the one-card gate permanently — for **every** agent — on the most ordinary
/// user action there is, and default 3 ("the overlay is the notification")
/// made the failure silent.
///
/// Falsification: delete the ageing branch at the head of `raise_next` (or
/// stop stamping `raised_unix`) and the last assertion goes red — no approval,
/// for any agent, ever prompts again.
#[test]
fn an_ignored_card_stops_muting_the_queue_once_it_ages_out() {
    let (mut m, _rx) = model();
    m.update(clock(1_000));
    m.update(status(roster("agent_status_grouped.json")));

    // #7 prompts, and nobody answers it.
    let first = m.update(pending(vec![approval(7, "trollshell-choom")]));
    assert_eq!(prompts(&first).len(), 1, "{first:?}");

    // A second agent's approval queues up behind it. While the card could
    // still be on screen, the gate correctly holds.
    let queue = vec![approval(7, "trollshell-choom"), approval(8, "nixos-choom")];
    for tick in 0..25 {
        m.update(clock(1_000 + tick * 2));
        let fx = m.update(pending(queue.clone()));
        assert_eq!(prompts(&fx), Vec::new(), "tick {tick}: {fx:?}");
    }

    // Past the host's bound plus the race margin, the gate reopens and the
    // approval *behind* the ignored one prompts.
    m.update(clock(1_000 + 66));
    let woken = m.update(pending(queue.clone()));
    let raised = prompts(&woken);
    assert_eq!(raised.len(), 1, "{woken:?}");
    assert!(
        raised[0].3.contains("request #8"),
        "the queue behind the ignored card, not the card itself: {}",
        raised[0].3
    );

    // …and the ignored one is still pending and still badged — default 2. It
    // is *not* re-raised on its own; the badge click is its way back.
    assert_eq!(m.pending.count_for("trollshell-choom"), 1);
    let after = m.update(pending(queue));
    assert_eq!(prompts(&after), Vec::new(), "{after:?}");
}

/// The ageing is bounded by the **host's** number, not a guess: a prompt that
/// is merely old-ish still holds the gate, because the card may well still be
/// on screen and a second `RequestConsent` would replace it.
#[test]
fn the_gate_ages_out_only_past_the_hosts_own_bound() {
    let (mut m, _rx) = model();
    m.update(clock(1_000));
    m.update(status(roster("agent_status_grouped.json")));
    let queue = vec![approval(7, "trollshell-choom"), approval(8, "nixos-choom")];
    m.update(pending(vec![approval(7, "trollshell-choom")]));

    // One second before the host would even have torn the card down.
    m.update(clock(1_000 + 59));
    assert_eq!(prompts(&m.update(pending(queue.clone()))), Vec::new());

    // Exactly at the bound plus the grace.
    m.update(clock(1_000 + 65));
    assert_eq!(prompts(&m.update(pending(queue))).len(), 1);
}

/// With no clock yet, nothing ages: guessing at elapsed time without one would
/// abandon a card that is still on screen.
#[test]
fn a_prompt_raised_before_the_first_clock_never_ages_out() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    m.update(pending(vec![approval(7, "trollshell-choom")]));

    let queue = vec![approval(7, "trollshell-choom"), approval(8, "nixos-choom")];
    // A clock arrives *after* the prompt was raised, reading far past the
    // bound. `raised_unix` is 0, so the elapsed time is unknowable.
    m.update(clock(9_999_999));
    assert_eq!(prompts(&m.update(pending(queue))), Vec::new());
}

/// §6.5's "dropping it also reopens the gate" — the mechanism the review found
/// unpinned (mutation 5).
///
/// Falsification: delete the `self.prompt = None` in `fold_pending` and #8
/// never prompts, because the departed #7 holds the gate until the ageing
/// branch eventually lets go.
#[test]
fn an_approval_leaving_the_queue_reopens_the_gate_in_that_same_fold() {
    let (mut m, _rx) = model();
    m.update(clock(1_000));
    m.update(status(roster("agent_status_grouped.json")));
    assert_eq!(
        prompts(&m.update(pending(vec![approval(7, "trollshell-choom")]))).len(),
        1
    );

    // #7 was answered on the dashboard; #8 arrived. Same fold, no clock
    // movement at all — so only the departure can be what reopened the gate.
    let fx = m.update(pending(vec![approval(8, "nixos-choom")]));
    let raised = prompts(&fx);
    assert_eq!(raised.len(), 1, "{fx:?}");
    assert!(raised[0].3.contains("request #8"), "{}", raised[0].3);
}

/// **MEDIUM-2.** The review's probe, inverted: an approval naming an agent the
/// roster does not list gets **no prompt and no badge**, and does not hold the
/// gate against the agents that do exist.
///
/// A card with nothing on screen corresponding to it is unanswerable — the
/// badge is the only way back into a prompt, and the badge is drawn by walking
/// the roster.
///
/// Falsification: drop the roster filter in `fold_pending` and the first
/// assertion finds a prompt for `ghost-agent`.
#[test]
fn an_approval_for_an_agent_the_roster_does_not_list_is_dropped() {
    let (mut m, _rx) = model();
    m.update(clock(1_000));
    m.update(status(roster("agent_status_grouped.json")));

    let fx = m.update(pending(vec![approval(7, "ghost-agent")]));
    assert_eq!(prompts(&fx), Vec::new(), "no prompt: {fx:?}");
    assert_eq!(m.pending.count_for("ghost-agent"), 0, "no badge");
    assert!(
        !button_ids(&card_of(&m))
            .iter()
            .any(|id| id.starts_with("approvals:")),
        "no badge anywhere on the card"
    );

    // …and a real agent's approval behind it still prompts, in the same fold.
    let fx = m.update(pending(vec![
        approval(7, "ghost-agent"),
        approval(8, "trollshell-choom"),
    ]));
    let raised = prompts(&fx);
    assert_eq!(raised.len(), 1, "{fx:?}");
    assert!(raised[0].3.contains("request #8"), "{}", raised[0].3);
}

/// A name hyperhive's own `Ident` would refuse never reaches the card's
/// headline either — which is what `wire.rs`'s `Approval::agent` doc already
/// claimed happened, and did not.
#[test]
fn an_approval_whose_agent_fails_the_whitelist_is_dropped() {
    let (mut m, _rx) = model();
    m.update(clock(1_000));
    m.update(status(roster("agent_status_grouped.json")));
    for bad in ["Agent_9", "SHOUTING", "../etc/passwd", ""] {
        let fx = m.update(pending(vec![approval(7, bad)]));
        assert_eq!(prompts(&fx), Vec::new(), "{bad:?}: {fx:?}");
    }
}

/// With the hive not `Up` there is no roster to judge against, so the queue
/// empties — "the hive is down" is not "this agent does not exist", and the
/// card renders its error row rather than badges either way.
#[test]
fn a_queue_arriving_while_the_hive_is_down_badges_nothing() {
    let (mut m, _rx) = model();
    m.update(clock(1_000));
    m.update(Input::App(Msg::Status(Err(HiveError::Unreachable {
        reason: "no socket".to_owned(),
    }))));
    let fx = m.update(pending(vec![approval(7, "trollshell-choom")]));
    assert_eq!(prompts(&fx), Vec::new(), "{fx:?}");
    assert_eq!(m.pending.count_for("trollshell-choom"), 0);
}

/// **LOW-3.** A refused `Pending` clears the badges — they are claims about a
/// queue this build can no longer see — without disturbing `prompted` or the
/// in-flight prompt, so a one-tick blip cannot re-raise a card the operator
/// already has on screen.
///
/// Falsification: make the `Err` arm a no-op and the badge survives a refusal;
/// make it clear `prompted` too and the recovery fold re-prompts #7.
#[test]
fn a_refused_queue_clears_the_badges_but_never_re_prompts() {
    let (mut m, _rx) = model();
    m.update(clock(1_000));
    m.update(status(roster("agent_status_grouped.json")));
    assert_eq!(
        prompts(&m.update(pending(vec![approval(7, "trollshell-choom")]))).len(),
        1
    );
    assert_eq!(m.pending.count_for("trollshell-choom"), 1);

    let refused = m.update(pending_refused());
    assert_eq!(prompts(&refused), Vec::new(), "{refused:?}");
    assert_eq!(
        m.pending.count_for("trollshell-choom"),
        0,
        "a queue we cannot see must not be badged"
    );

    // The hive answers again with the same approval still queued: it is badged
    // again, and it does **not** prompt again.
    let back = m.update(pending(vec![approval(7, "trollshell-choom")]));
    assert_eq!(prompts(&back), Vec::new(), "{back:?}");
    assert_eq!(m.pending.count_for("trollshell-choom"), 1);
}

/// **MEDIUM-5.** All three of the card's free-text values are bounded, not one.
///
/// `agent` reaches the headline and `requested_at` the detail line, both raw
/// off the wire and both deliberately unparsed by the mirror. On a 480 px
/// wrapping label an unbounded one pushes the buttons off the only surface
/// that can answer the approval.
///
/// Falsification: drop either `clamp` and the matching assertion goes red.
#[test]
fn every_free_text_value_on_the_card_is_bounded() {
    let (mut m, _rx) = model();
    m.update(clock(1_000));
    // A roster row whose name is legal but long, so the *label* is what grows:
    // `AgentName` already caps the name at 63 bytes, and the config's label
    // for it is operator-supplied and uncapped.
    m.update(status(roster("agent_status_grouped.json")));

    let mut a = approval(7, "trollshell-choom");
    a.description = Some("🦀".repeat(5_000));
    a.requested_at = "9".repeat(5_000);

    let fx = m.update(pending(vec![a]));
    let (_, agent, _, detail) = prompts(&fx)[0].clone();
    assert!(
        agent.chars().count() <= 64,
        "the headline is bounded, got {} chars",
        agent.chars().count()
    );
    assert!(
        detail.chars().count() <= 320,
        "the detail line is bounded, got {} chars",
        detail.chars().count()
    );
    assert!(detail.contains('…'), "and says it was cut: {detail}");
}

/// The same, with a 5 000-char **label** — the value an operator writes into
/// `agents.toml`, which nothing else caps.
#[test]
fn a_runaway_display_label_cannot_grow_the_card() {
    let (mut m, _rx) = model();
    m.update(clock(1_000));
    m.update(status(roster("agent_status_grouped.json")));
    m.update(Input::App(Msg::Config(Box::new(
        hytte_config::subsystem::assemble::<hytte_plugin_agents::config::AgentsConfig>(&[(
            std::path::PathBuf::from("overlay.toml"),
            format!(
                "[display.trollshell-choom]\nlabel = \"{}\"\n",
                "x".repeat(5_000)
            ),
        )])
        .expect("the config assembles")
        .config,
    ))));

    let fx = m.update(pending(vec![approval(7, "trollshell-choom")]));
    let agent = prompts(&fx)[0].1.clone();
    assert!(
        agent.chars().count() <= 64,
        "got {} chars",
        agent.chars().count()
    );
}
