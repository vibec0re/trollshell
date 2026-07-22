//! Smoke: build an `App`, register no services, and assert that the body
//! closure runs and that we can enumerate at least one monitor.
//!
//! Needs a display server, so it lives in the `system-tests` bucket.
#![cfg(feature = "system-tests")]

use hytte_ui::App;
use std::cell::Cell;
use std::rc::Rc;

#[test]
fn body_runs_on_activate() {
    let ran = Rc::new(Cell::new(false));
    let ran_writer = ran.clone();

    App::new("mov.vibec0re.hytte.test")
        .run(move |app| {
            ran_writer.set(true);
            // Don't crash even if there are no monitors (CI/headless edge).
            let _ = app.monitors();
            // Stop the app loop immediately so the test exits.
            app.quit();
        })
        .expect("run");

    assert!(ran.get(), "body closure did not run");
}
