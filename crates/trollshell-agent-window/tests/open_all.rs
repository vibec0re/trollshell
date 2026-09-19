//! `--open-all`'s roster read, driven against the **scripted `host.sock`**
//! the window's other integration tests already use
//! ([#1306](https://github.com/vibec0re/trollshell/issues/1306)).
//!
//! What `src/open_all.rs`'s unit tests cannot cover is the half that crosses a
//! socket: that the fan-out asks the hive the same verb the card asks, reads
//! the same bytes back, and turns a real multi-agent answer into the launch
//! list. That needs no display and no compositor — a real `UnixListener` and
//! the real client are the whole harness.

mod fake;

use fake::FakeHive;
use hytte_plugin_agents::model::AgentName;
use trollshell_agent_window::open_all::{roster_from, running_agents};

/// A five-agent roster covering every one of the five collapsed states, in
/// the hive's own JSON — the same shape (and the same five rows) as
/// `hytte-plugin-agents/tests/fixtures/agent_status_precedence.json`, so the
/// two crates read one wire.
const PRECEDENCE: &str = r#"{"version":1,"ok":true,"agent_statuses":[
  {"name":"wedged","running":false,"failed":true,"needs_login":true,"paused":true},
  {"name":"locked-out","running":true,"failed":false,"needs_login":true,"paused":true},
  {"name":"parked","running":true,"failed":false,"needs_login":false,"paused":true},
  {"name":"off","running":false,"failed":false,"needs_login":false,"paused":false},
  {"name":"busy","running":true,"failed":false,"needs_login":false,"paused":false},
  {"name":"argus","running":true,"failed":false,"needs_login":false,"paused":false}
]}"#;

/// The whole membership decision, end to end over a real socket: six agents
/// on the wire, **two** windows.
///
/// Three of the four excluded rows have `running: true` on the wire —
/// `locked-out` needs a login, `parked` is paused, `wedged` has failed — which
/// is the point: the rule is the *collapsed* `Status`, not the raw flag, and
/// only a roster carrying that distinction can tell the two apart.
///
/// Falsification (verified red): filter on `row.running` in `running_agents`
/// and this answers five names instead of two.
#[tokio::test]
async fn a_real_roster_becomes_the_running_agents_in_the_hives_own_order() {
    // `host.sock` speaks JSON **lines**, one object per line, and the fixture
    // above is indented for a human — so the newlines come back out before it
    // goes on the wire.
    let script = PRECEDENCE.replace('\n', "");
    let hive = FakeHive::script(&[script.as_str()]);

    let rows = roster_from(hive.path()).await.expect("the fake answers");
    assert_eq!(rows.len(), 6, "every row is carried off the wire");

    assert_eq!(
        running_agents(&rows)
            .iter()
            .map(AgentName::as_str)
            .collect::<Vec<_>>(),
        vec!["busy", "argus"],
        "only the running ones, in the order the hive listed them"
    );

    // …and it asked the one verb the card asks, nothing else.
    let asked = hive.seen();
    assert_eq!(asked.len(), 1, "one round trip: {asked:?}");
    assert!(asked[0].contains("agent_status"), "{asked:?}");
}

/// A hive that answered and listed nothing is **not** an error: the fan-out
/// says so and exits 0, because "nothing to open" is an answer. Here that is
/// the empty list reaching `running_agents`; `open_all::run` turns it into
/// the one stderr line.
#[tokio::test]
async fn an_empty_roster_is_an_answer_and_not_a_failure() {
    let hive = FakeHive::script(&[r#"{"version":1,"ok":true,"agent_statuses":[]}"#]);
    let rows = roster_from(hive.path()).await.expect("the fake answers");
    assert!(rows.is_empty());
    assert!(running_agents(&rows).is_empty());
}

/// A hive that is not there is an error with the operator's own fix in it —
/// the client's `connect_reason`, unchanged, because this mode adds no second
/// diagnosis of its own.
#[tokio::test]
async fn an_absent_socket_reports_the_clients_own_reason() {
    let dir = tempfile::tempdir().expect("a tempdir");
    let err = roster_from(&dir.path().join("nothing-here.sock"))
        .await
        .expect_err("there is no socket");
    assert!(
        err.to_string().contains("hive-c0re is not running"),
        "{err}"
    );
}

/// A hive that answers `ok: false` is a refusal, not an unreachable socket:
/// the fan-out must not send the operator to `systemctl` for a daemon that is
/// running and saying no.
#[tokio::test]
async fn a_refusing_hive_carries_its_own_sentence() {
    let hive = FakeHive::script(&[r#"{"version":1,"ok":true,"agent_statuses":[]}"#])
        .refusing("agent_status", "the roster is locked");
    let err = roster_from(hive.path()).await.expect_err("the hive says no");
    assert!(err.to_string().contains("the roster is locked"), "{err}");
}
