//! Roundtrip tests against a real, ephemeral evolution-data-server (#29/#33).
//!
//! Opt-in (needs EDS + a session bus): run in the nix devShell with
//! `cargo test -p hytte-ecal --features system-tests -- --test-threads=1`.
//! Single-threaded because GLib caches the session-bus connection process-wide
//! and the harness spawns one EDS per process.
#![cfg(feature = "system-tests")]

mod common;

use hytte_ecal::sys::ECalClientSourceType;
use hytte_ecal::{CalClient, Registry, Source};

const PLAIN_EVENT: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//hytte//test//EN\r\n\
BEGIN:VEVENT\r\nUID:hytte-plain-1\r\nSUMMARY:Plain Event\r\n\
DTSTART:20260620T100000Z\r\nDTEND:20260620T110000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

const A_TASK: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//hytte//test//EN\r\n\
BEGIN:VTODO\r\nUID:hytte-task-1\r\nSUMMARY:Test Task\r\nSTATUS:NEEDS-ACTION\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";

fn find(sources: Vec<Source>, name: &str) -> Source {
    sources
        .into_iter()
        .find(|s| s.display_name() == name)
        .unwrap_or_else(|| panic!("fixture source {name:?} not found"))
}

#[test]
fn registry_sees_fixtures_and_event_task_roundtrip() {
    let _eds = common::spawn();

    let registry = Registry::new().expect("open ephemeral source registry");

    // The harness's two fixture sources show up in the registry.
    let cal = find(registry.calendars(), common::CAL_NAME);
    let tasks = find(registry.task_lists(), common::TASKS_NAME);

    // Calendar: create → list → remove a plain event.
    let cal_client =
        CalClient::connect(&cal, ECalClientSourceType::Events, 10).expect("connect calendar");
    let uid = cal_client
        .create_from_ical(PLAIN_EVENT)
        .expect("create event");
    let objs = cal_client.get_object_strings("#t").expect("list events");
    assert_eq!(objs.len(), 1, "one event after create");
    assert!(
        objs[0].contains("SUMMARY:Plain Event"),
        "event body roundtrips: {}",
        objs[0]
    );
    cal_client.remove(&uid, None).expect("remove event");
    assert!(
        cal_client
            .get_object_strings("#t")
            .expect("list after remove")
            .is_empty(),
        "calendar empty after remove"
    );

    // Task list: create → list a VTODO (the #33 surface).
    let task_client =
        CalClient::connect(&tasks, ECalClientSourceType::Tasks, 10).expect("connect tasks");
    task_client.create_from_ical(A_TASK).expect("create task");
    let task_objs = task_client.get_object_strings("#t").expect("list tasks");
    assert_eq!(task_objs.len(), 1, "one task after create");
    assert!(
        task_objs[0].contains("SUMMARY:Test Task"),
        "task body roundtrips: {}",
        task_objs[0]
    );
}
