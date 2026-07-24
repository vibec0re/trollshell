//! End-to-end smoke test against whatever EDS sources this machine has
//! configured. Run with `cargo run -p hytte-ecal --example probe`.
//!
//! Three halves:
//! - **Tasks:** list every task source, then create / query / remove one
//!   VTODO on the first task source found.
//! - **Tasks (live view push):** open a [`CalClient::watch`] over the first
//!   task list, then — from a *separate* client connection, standing in for
//!   Endeavour — create and modify a task, pumping the [`MainContext`] and
//!   asserting EDS pushes the `objects-added`/`-modified` notifications to the
//!   view (issue #33). Prints `live view push count: N`.
//! - **Calendar (RRULE expansion + EXDATE):** list every calendar source,
//!   then on the first one, create a `FREQ=DAILY;COUNT=5` VEVENT and expand it
//!   via [`CalClient::generate_instances`] over a one-month window — verifying
//!   the recurrence-expansion path (issue #29). Prints
//!   `recurring instance count: N`. It also seeds a second `COUNT=5` series
//!   with an `EXDATE` cancelling one day and prints `exdate instance count: N`
//!   plus `exdate cancelled occurrence present: <bool>`, so the nixosTest can
//!   assert the cancelled occurrence is excluded (the #29 follow-up). Finally it
//!   seeds a `DTSTART;TZID=Europe/Berlin:…123000` event and prints
//!   `tzid instance start_unix: N`, so the nixosTest can assert the zoned time
//!   resolves to its absolute instant (10:30 UTC), not the +2h double-shift
//!   (issue #522).

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use hytte_ecal::{CalClient, MainContext, Registry, sys::ECalClientSourceType};

fn main() -> anyhow::Result<()> {
    let registry = Registry::new()?;
    probe_tasks(&registry)?;
    probe_tasks_live_view(&registry)?;
    probe_calendar_recurrence(&registry)?;
    Ok(())
}

/// The push-refresh path (issue #33). Open a live view on the first task list,
/// then mutate it from a second client connection — simulating an external app
/// like Endeavour — and assert the view's callback fires for each change. This
/// exercises the whole FFI chain end-to-end against a real EDS: `get_view_sync`
/// → `view_start` → the GObject signal trampoline → the boxed Rust callback,
/// pumped via a private [`MainContext`].
fn probe_tasks_live_view(registry: &Registry) -> anyhow::Result<()> {
    println!("\n-- live view push (#33) --");
    let lists = registry.task_lists();
    let Some(first) = lists.first() else {
        eprintln!("no task lists configured — skipping live-view probe");
        return Ok(());
    };

    // The worker-thread model: a private context, pushed thread-default, that
    // the view's signals dispatch onto. Created before the view, as in the
    // tasks service.
    let ctx = MainContext::new().ok_or_else(|| anyhow::anyhow!("GMainContext alloc failed"))?;

    // Client A holds the view. The callback just counts pushes (coalesced —
    // exactly what the service does). `Rc<Cell>` is fine: the callback only
    // ever runs on this thread, inside `ctx.iterate`.
    let watcher = CalClient::connect(first, ECalClientSourceType::Tasks, 5)?;
    let pushes = Rc::new(Cell::new(0_usize));
    let pushes_cb = Rc::clone(&pushes);
    let _view = watcher.watch("#t", move || pushes_cb.set(pushes_cb.get() + 1))?;
    println!("watching '{}'", first.display_name());

    // `view-start` replays current contents via one `objects-added`; drain it
    // so the count below reflects only our subsequent writes.
    pump_until(&ctx, Duration::from_secs(5), &|| pushes.get() >= 1);
    let after_initial = pushes.get();
    println!("initial population pushes: {after_initial}");

    // Client B = the "external editor". A distinct connection, so EDS routes
    // its writes back to A's view over the real notification path.
    let editor = CalClient::connect(first, ECalClientSourceType::Tasks, 5)?;
    let ical = "\
        BEGIN:VCALENDAR\r\n\
        VERSION:2.0\r\n\
        PRODID:-//hytte-ecal-probe//\r\n\
        BEGIN:VTODO\r\n\
        UID:hytte-ecal-live-1\r\n\
        DTSTAMP:20260521T160000Z\r\n\
        SUMMARY:Live push probe\r\n\
        STATUS:NEEDS-ACTION\r\n\
        END:VTODO\r\n\
        END:VCALENDAR\r\n";
    let uid = editor.create_from_ical(ical)?;
    println!("editor created uid: {uid}");

    let target = after_initial + 1;
    let got_add = pump_until(&ctx, Duration::from_secs(10), &|| pushes.get() >= target);
    println!("after external create, push count: {}", pushes.get());
    if !got_add {
        anyhow::bail!("view did not receive a push after external create");
    }

    // Now modify it (the exact #33 scenario: an external app un/checks a todo).
    let modified = "\
        BEGIN:VCALENDAR\r\n\
        VERSION:2.0\r\n\
        PRODID:-//hytte-ecal-probe//\r\n\
        BEGIN:VTODO\r\n\
        UID:hytte-ecal-live-1\r\n\
        DTSTAMP:20260521T160500Z\r\n\
        LAST-MODIFIED:20260521T160500Z\r\n\
        SUMMARY:Live push probe (edited externally)\r\n\
        STATUS:COMPLETED\r\n\
        PERCENT-COMPLETE:100\r\n\
        END:VTODO\r\n\
        END:VCALENDAR\r\n";
    editor.modify_from_ical(modified)?;
    println!("editor modified uid: {uid}");

    let target = pushes.get() + 1;
    let got_mod = pump_until(&ctx, Duration::from_secs(10), &|| pushes.get() >= target);
    println!("live view push count: {}", pushes.get());
    if !got_mod {
        anyhow::bail!("view did not receive a push after external modify");
    }

    editor.remove(&uid, None)?;
    // Drain the removal push too (best-effort — not asserted).
    pump_until(&ctx, Duration::from_secs(3), &|| false);
    println!("removed {uid}");
    Ok(())
}

/// Pump `ctx` until `done()` is true or `budget` elapses; returns whether
/// `done()` became true. Blocks per-iteration with a short safety timeout so a
/// missing push can't hang the probe forever — we just rely on EDS waking the
/// context when a notification arrives.
fn pump_until(ctx: &MainContext, budget: Duration, done: &dyn Fn() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        // A non-blocking iteration dispatches any ready signal; sleep briefly
        // between turns so we don't spin while waiting on the D-Bus round-trip.
        if !ctx.iterate(false) {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    done()
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

/// Seed daily-recurring VEVENTs on the first calendar source and expand them
/// into instances over a one-month window, exercising the RRULE-expansion
/// binding — the fix for #29 — plus the EXDATE recurrence-set modifier (the
/// #29 follow-up): a second series cancels one occurrence via EXDATE, and the
/// expansion must surface it as one fewer instance.
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

    // The EXDATE companion: the same 5-occurrence daily shape, but with Jun 3
    // cancelled via EXDATE. Correct expansion drops that one occurrence ⇒ 4.
    let ical_exdate = "\
        BEGIN:VCALENDAR\r\n\
        VERSION:2.0\r\n\
        PRODID:-//hytte-ecal-probe//\r\n\
        BEGIN:VEVENT\r\n\
        UID:hytte-ecal-exdate-1\r\n\
        DTSTAMP:20260601T090000Z\r\n\
        DTSTART:20260601T090000Z\r\n\
        DTEND:20260601T093000Z\r\n\
        SUMMARY:Daily recurring probe with a cancelled day\r\n\
        RRULE:FREQ=DAILY;COUNT=5\r\n\
        EXDATE:20260603T090000Z\r\n\
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

    // Seed the EXDATE series on the same (writable) calendar.
    let exdate_uid = client.create_from_ical(ical_exdate)?;
    println!("created exdate uid: {exdate_uid}");

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

    // The EXDATE series: Jun 1,2,4,5 — the cancelled Jun 3 (09:00Z) absent.
    let cancelled_unix = 1_780_304_400 + 2 * 86_400; // 2026-06-03T09:00:00Z
    let exdate_recurring: Vec<_> = instances
        .iter()
        .filter(|i| i.ical.contains("hytte-ecal-exdate-1"))
        .collect();
    println!("exdate instance count: {}", exdate_recurring.len());
    let cancelled_present = exdate_recurring
        .iter()
        .any(|i| i.start_unix == cancelled_unix);
    println!("exdate cancelled occurrence present: {cancelled_present}");
    for inst in &exdate_recurring {
        println!(
            "  exdate instance start_unix={} end_unix={}",
            inst.start_unix, inst.end_unix
        );
    }

    // Zoned-time expansion (#522): its own end-to-end round-trip on the same
    // writable calendar.
    probe_calendar_zoned_time(&client)?;

    client.remove(&uid, None)?;
    println!("removed recurring {uid}");
    client.remove(&exdate_uid, None)?;
    println!("removed exdate {exdate_uid}");
    Ok(())
}

/// Seed a single-occurrence event with an explicit `TZID=Europe/Berlin`
/// (carrying its `VTIMEZONE` inline, the shape a synced CalDAV/Google calendar
/// delivers) on `client`, expand it, and report its absolute start_unix — the
/// #522 regression guard.
///
/// 12:30 in CEST (UTC+2) is the absolute instant 10:30 UTC (start_unix
/// 1784889000), regardless of the viewer's zone. The pre-fix conversion read
/// the Berlin wall-clock as UTC, yielding 12:30 UTC (1784896200) which the
/// display side then shifted to 14:30 CEST — a 12:30 event surfacing as 14:30.
/// This drives the whole end-to-end path (create → EDS store →
/// get_object_list → generate_instances → ical_time_to_unix) through a real
/// backend, not just the hermetic string parser.
fn probe_calendar_zoned_time(client: &CalClient) -> anyhow::Result<()> {
    let ical_tzid = "\
        BEGIN:VCALENDAR\r\n\
        VERSION:2.0\r\n\
        PRODID:-//hytte-ecal-probe//\r\n\
        BEGIN:VTIMEZONE\r\n\
        TZID:Europe/Berlin\r\n\
        BEGIN:DAYLIGHT\r\n\
        TZOFFSETFROM:+0100\r\n\
        TZOFFSETTO:+0200\r\n\
        TZNAME:CEST\r\n\
        DTSTART:19700329T020000\r\n\
        RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\n\
        END:DAYLIGHT\r\n\
        BEGIN:STANDARD\r\n\
        TZOFFSETFROM:+0200\r\n\
        TZOFFSETTO:+0100\r\n\
        TZNAME:CET\r\n\
        DTSTART:19701025T030000\r\n\
        RRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\n\
        END:STANDARD\r\n\
        END:VTIMEZONE\r\n\
        BEGIN:VEVENT\r\n\
        UID:hytte-ecal-tzid-1\r\n\
        DTSTAMP:20260724T090000Z\r\n\
        DTSTART;TZID=Europe/Berlin:20260724T123000\r\n\
        DTEND;TZID=Europe/Berlin:20260724T133000\r\n\
        SUMMARY:Zoned lunch (Europe/Berlin)\r\n\
        END:VEVENT\r\n\
        END:VCALENDAR\r\n";

    let tzid_uid = client.create_from_ical(ical_tzid)?;
    println!("created tzid uid: {tzid_uid}");

    // A window bracketing 2026-07-24.
    let jul_start = 1_782_864_000; // 2026-07-01T00:00:00Z
    let jul_end = 1_785_542_400; // 2026-08-01T00:00:00Z
    let instances = client.generate_instances(jul_start, jul_end)?;
    let tzid: Vec<_> = instances
        .iter()
        .filter(|i| i.ical.contains("hytte-ecal-tzid-1"))
        .collect();
    println!("tzid instance count: {}", tzid.len());
    for inst in &tzid {
        println!("tzid instance start_unix: {}", inst.start_unix);
    }

    client.remove(&tzid_uid, None)?;
    println!("removed tzid {tzid_uid}");
    Ok(())
}
