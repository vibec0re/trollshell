//! End-to-end smoke test against whatever EDS sources this machine has
//! configured. Run with `cargo run -p hytte-ecal --example probe`.
//!
//! Two halves:
//! - **Tasks:** list every task source, then create / query / remove one
//!   VTODO on the first task source found.
//! - **Calendar (RRULE expansion):** list every calendar source, then on
//!   the first one, create a `FREQ=DAILY;COUNT=5` VEVENT and expand it via
//!   [`CalClient::generate_instances`] over a one-month window — verifying
//!   the recurrence-expansion path (issue #29). Prints
//!   `recurring instance count: N` so the nixosTest can assert it.

use hytte_ecal::{CalClient, Registry, sys::ECalClientSourceType};

fn main() -> anyhow::Result<()> {
    let registry = Registry::new()?;
    probe_tasks(&registry)?;
    probe_calendar_recurrence(&registry)?;
    Ok(())
}

/// The original task-list roundtrip (create → query → remove a VTODO).
fn probe_tasks(registry: &Registry) -> anyhow::Result<()> {
    let lists = registry.task_lists();
    println!("found {} task list(s)", lists.len());
    for src in &lists {
        println!("  - {} ({})", src.display_name(), src.uid());
    }
    let Some(first) = lists.first() else {
        eprintln!("no task lists configured — skipping task roundtrip");
        return Ok(());
    };

    let client = CalClient::connect(first, ECalClientSourceType::Tasks, 5)?;
    println!("\nconnected to '{}'", first.display_name());

    let ical = "\
        BEGIN:VCALENDAR\r\n\
        VERSION:2.0\r\n\
        PRODID:-//hytte-ecal-probe//\r\n\
        BEGIN:VTODO\r\n\
        UID:hytte-ecal-probe-1\r\n\
        DTSTAMP:20260521T160000Z\r\n\
        SUMMARY:Probe task (created via FFI)\r\n\
        STATUS:NEEDS-ACTION\r\n\
        END:VTODO\r\n\
        END:VCALENDAR\r\n";
    let uid = client.create_from_ical(ical)?;
    println!("created uid: {uid}");

    let all = client.get_object_strings("#t")?;
    println!("\nbackend has {} object(s):", all.len());
    for (i, s) in all.iter().enumerate() {
        let first_line = s.lines().nth(2).unwrap_or("???");
        println!("  [{i}] {first_line}");
    }

    client.remove(&uid, None)?;
    println!("\nremoved {uid}");
    Ok(())
}

/// Seed a daily-recurring VEVENT on the first calendar source and expand it
/// into instances over a one-month window, exercising the RRULE-expansion
/// binding (`e_cal_client_generate_instances_sync`) — the fix for #29.
fn probe_calendar_recurrence(registry: &Registry) -> anyhow::Result<()> {
    let cals = registry.calendars();
    println!("\nfound {} calendar(s)", cals.len());
    for src in &cals {
        println!("  - {} ({})", src.display_name(), src.uid());
    }
    if cals.is_empty() {
        eprintln!("no calendars configured — skipping recurrence probe");
        return Ok(());
    }

    // A 5-occurrence daily series. DTSTAMP is required by RFC 5545; the
    // anchor is inside the window we expand below so all 5 occurrences
    // (Jun 1–5 2026, UTC) land in range.
    let ical = "\
        BEGIN:VCALENDAR\r\n\
        VERSION:2.0\r\n\
        PRODID:-//hytte-ecal-probe//\r\n\
        BEGIN:VEVENT\r\n\
        UID:hytte-ecal-rrule-1\r\n\
        DTSTAMP:20260601T090000Z\r\n\
        DTSTART:20260601T090000Z\r\n\
        DTEND:20260601T093000Z\r\n\
        SUMMARY:Daily recurring probe\r\n\
        RRULE:FREQ=DAILY;COUNT=5\r\n\
        END:VEVENT\r\n\
        END:VCALENDAR\r\n";

    // Use the first calendar we can actually write to. Source ordering isn't
    // guaranteed and a read-only/remote calendar would fail the create —
    // skip past those rather than aborting the whole probe.
    let mut connected = None;
    for src in &cals {
        let client = match CalClient::connect(src, ECalClientSourceType::Events, 5) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("connect to '{}' failed: {e}", src.display_name());
                continue;
            }
        };
        match client.create_from_ical(ical) {
            Ok(uid) => {
                println!("\nconnected to calendar '{}'", src.display_name());
                connected = Some((client, uid));
                break;
            }
            Err(e) => {
                eprintln!("create on '{}' failed: {e}", src.display_name());
            }
        }
    }
    let Some((client, uid)) = connected else {
        anyhow::bail!("no writable calendar found to seed the recurring event");
    };
    println!("created recurring uid: {uid}");

    // Window: the whole of June 2026 (UTC). All 5 daily occurrences fall
    // inside it.
    let start_unix = 1_780_272_000; // 2026-06-01T00:00:00Z
    let end_unix = 1_782_864_000; // 2026-07-01T00:00:00Z
    let instances = client.generate_instances(start_unix, end_unix)?;
    let recurring: Vec<_> = instances
        .iter()
        .filter(|i| i.ical.contains("hytte-ecal-rrule-1"))
        .collect();
    println!("recurring instance count: {}", recurring.len());
    for inst in &recurring {
        println!(
            "  instance start_unix={} end_unix={}",
            inst.start_unix, inst.end_unix
        );
    }

    client.remove(&uid, None)?;
    println!("removed recurring {uid}");
    Ok(())
}
