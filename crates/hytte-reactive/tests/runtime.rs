use hytte_reactive::runtime;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[test]
fn handle_spawns_tasks_on_a_background_thread() {
    let ran = Arc::new(AtomicBool::new(false));
    let ran_writer = ran.clone();

    runtime::handle().spawn(async move {
        ran_writer.store(true, Ordering::SeqCst);
    });

    // Give the runtime a moment to schedule and run the task.
    for _ in 0..100 {
        if ran.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("background task did not run within 1s");
}

#[test]
fn handle_is_stable_across_calls() {
    let h1 = runtime::handle();
    let h2 = runtime::handle();
    assert!(
        std::ptr::eq(h1, h2),
        "handle() should return the same Handle"
    );
}
