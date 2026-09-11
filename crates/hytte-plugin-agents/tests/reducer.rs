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

use hytte_plugin::proto::{Effect, EffectOutcome, EventKind, Page};
use hytte_plugin::{CmdReceiver, Input, Plugin, cmd_channel};
use hytte_plugin_agents::Agents;
use hytte_plugin_agents::hive::client::HiveError;
use hytte_plugin_agents::hive::wire::{
    AgentStatusRow, HiveUrls, Request, Response, Scope, VersionMismatch,
};
use hytte_plugin_agents::model::{Hive, Status};
use hytte_plugin_agents::poll::{Cmd, Msg};

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

/// The **edit** button — Annika's `[optionsedit]` — opens this plugin's page on
/// that agent, and asks the hive nothing.
///
/// `OpenPage(PluginSelf)` names the *page*; #1010's modal dialog changes the
/// surface the host mounts it on, not this effect, which is why the button can
/// be wired now and the dialog can land later without touching this arm.
///
/// Falsification: point the `ids::EDIT` arm at anything else and the `Effect`
/// or the `selected` assertion reds.
#[test]
fn the_edit_button_opens_this_agents_page() {
    let (mut m, mut rx) = model();
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
/// `OpenPage` + `Notify` + `OpenUri` and nothing else. **`RunCommand` is P2 and
/// `Consent` is P3** — a P1 that quietly declared either would be granted an
/// authority the row has not earned yet (spec §11 rules two and three, §13).
#[test]
fn the_manifest_declares_exactly_three_capabilities_and_no_secrets() {
    use hytte_plugin::proto::{Capability, Mount, StateKey};
    let m = Agents::manifest();
    assert_eq!(m.id, "agents");
    assert_eq!(m.mount, Mount::SidebarTop);
    assert_eq!(
        m.capabilities,
        vec![
            Capability::OpenPage,
            Capability::Notify,
            Capability::OpenUri
        ]
    );
    assert!(!m.capabilities.contains(&Capability::RunCommand));
    assert!(!m.capabilities.contains(&Capability::Consent));
    assert_eq!(
        m.subscribes,
        vec![StateKey::Clock, StateKey::SlotVisible],
        "SlotVisible is the #305 gate for the park; Clock drives the panel ages"
    );
    assert!(m.provides.is_empty(), "this plugin serves no datasource");
}

// ── the link button (#1045) ─────────────────────────────────────────────────

/// The manifest grants the capability the link click's effect **requires** —
/// asked of the one table that decides it, not restated as a list.
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
/// Mutation (verified red): delete `Capability::OpenUri` from `manifest()` and
/// both the `granted` assertion and the manifest test above go red.
#[test]
fn the_manifest_grants_what_the_link_click_emits() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    let fx = m.update(click("open:trollshell-choom"));
    let [effect] = fx.as_slice() else {
        panic!("a link click emits exactly one effect, got {fx:?}");
    };
    let required = effect
        .required_capability()
        .expect("OpenUri is a gated effect");

    let granted = Agents::manifest().capabilities;
    assert!(
        granted.contains(&required),
        "the SDK drops an effect whose capability the manifest omits (#1058): \
         {effect:?} needs {required:?}, manifest grants {granted:?}"
    );
}

/// A click on the link emits **exactly one** `OpenUri`, carrying that row's own
/// URL, and asks the hive nothing.
///
/// The URL is the fixture's, read back out of the model — not a string the
/// node id carried. Falsification: have `open_agent_page` parse a URL out of
/// `node` instead of looking the agent up and the assertion still passes for
/// this id but the `a_link_click_on_a_vanished_agent_opens_nothing` sibling
/// below goes red.
#[test]
fn a_link_click_emits_one_open_uri_with_the_rows_own_url() {
    let (mut m, mut rx) = model();
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

/// Two link clicks take **distinct** correlation tokens.
///
/// `EffectResult`'s own docs call the reply-bearing effects one shared id space
/// and say to allocate from a single counter (#1060): two effects in flight on
/// the same id cannot be told apart by the host's audit or by the plugin's own
/// result arm.
///
/// Falsification: make `take_effect_id` return a constant (or reset it per
/// click) and this reds on the second id.
#[test]
fn two_link_clicks_take_distinct_correlation_ids() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    let ids: Vec<u64> = ["open:trollshell-choom", "open:nixos-choom"]
        .into_iter()
        .map(|node| match m.update(click(node)).as_slice() {
            [Effect::OpenUri { id, .. }] => *id,
            other => panic!("expected one OpenUri, got {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![0, 1]);
}

/// A link click whose agent the model no longer holds — or whose name is
/// illegal — opens **nothing**.
///
/// This is what makes "the URL comes from the model, never from the id" a
/// testable property rather than a comment: the id is well-formed and the
/// prefix matches, and the only reason nothing is emitted is that there is no
/// agent to read a URL off.
///
/// Falsification: carry the URL on the node id (`open:<url>`) and emit it
/// verbatim, and this goes red.
#[test]
fn a_link_click_on_a_vanished_or_illegal_agent_opens_nothing() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    assert_eq!(m.update(click("open:ghost")), vec![], "no such agent");
    assert_eq!(
        m.update(click("open:not a name")),
        vec![],
        "the name fails the §11 whitelist before anything else happens"
    );
    // …and an agent the hive reports with no `url` at all.
    m.update(status(vec![AgentStatusRow {
        name: "trollshell-choom".to_owned(),
        running: true,
        ..AgentStatusRow::default()
    }]));
    assert_eq!(m.update(click("open:trollshell-choom")), vec![]);
    assert!(lines(&mut rx).is_empty());
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

/// A refused open **says so**; a successful one says nothing.
///
/// `EffectOutcome::output` exists for exactly this (`hytte-plugin`'s
/// `Input::EffectResult` docs: "so a plugin can toast it instead of leaving a
/// click that silently does nothing"). No correlation table is needed because
/// `OpenUri` is the only reply-bearing effect P1 emits — P2's `choom` launch is
/// what will need one.
///
/// Falsification: drop the `!outcome.ok` guard and the success half reds; drop
/// the arm entirely and the refusal half does.
#[test]
fn a_refused_open_toasts_its_reason_and_a_successful_one_is_silent() {
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
            summary: "couldn't open the link".to_owned(),
            body: "refused scheme: ftp".to_owned(),
        }]
    );

    assert_eq!(
        m.update(Input::EffectResult {
            id: 1,
            outcome: EffectOutcome {
                ok: true,
                output: None,
            },
        }),
        vec![],
        "the browser appearing is the feedback; a toast on top of it is noise"
    );
}

fn name(s: &str) -> hytte_plugin_agents::model::AgentName {
    hytte_plugin_agents::model::AgentName::parse(s).expect("a legal test name")
}
