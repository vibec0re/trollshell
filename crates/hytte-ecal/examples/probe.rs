//! End-to-end smoke test against whatever EDS task lists this machine
//! has configured. Run with `cargo run -p hytte-ecal --example probe`.
//! Lists every task source, then creates / queries / removes one task
//! on the first task source found.

use hytte_ecal::{CalClient, Registry, sys::ECalClientSourceType};

fn main() -> anyhow::Result<()> {
    let registry = Registry::new()?;
    let lists = registry.task_lists();
    println!("found {} task list(s)", lists.len());
    for src in &lists {
        println!("  - {} ({})", src.display_name(), src.uid());
    }
    let Some(first) = lists.first() else {
        eprintln!("no task lists configured — bailing");
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
