//! Smoke-test the tasks write path against the user's real EDS task
//! list. Run with `cargo run -p hytte-services --example tasks_smoke`.
//! Creates a task, marks it complete, edits the summary, and finally
//! deletes it — printing the .ics file after each step so a developer
//! can eyeball the diff.

use std::time::Duration;

use hytte_services::tasks;

fn main() {
    let ics = dirs_like_path();
    println!("tasks file: {}", ics.display());

    let uid = tasks::create_task("Smoke test task".into(), None);
    println!("\ncreated uid = {uid}");
    wait();
    dump(&ics);

    tasks::edit_task(&uid, "Smoke test task (edited)".into(), None);
    println!("\nedited:");
    wait();
    dump(&ics);

    tasks::set_completed(&uid, true);
    println!("\ncompleted:");
    wait();
    dump(&ics);

    tasks::delete_task(&uid);
    println!("\ndeleted:");
    wait();
    dump(&ics);
}

fn dirs_like_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    std::path::PathBuf::from(home)
        .join(".local/share/evolution/tasks/trollshell-tasks/calendar.ics")
}

fn dump(p: &std::path::Path) {
    println!("---");
    println!("{}", std::fs::read_to_string(p).unwrap_or_default());
    println!("---");
}

fn wait() {
    // Writes run on the tokio blocking pool; give them a beat to flush.
    std::thread::sleep(Duration::from_millis(200));
}
