//! Smoke: build an `App`, register no services, and assert that the body
//! closure runs and that we can enumerate at least one monitor.
//!
//! Needs a display server, so it lives in the `system-tests` bucket.
#![cfg(feature = "system-tests")]

use hytte_ui::App;
use hytte_ui::gtk::gdk;
use hytte_ui::gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// (what `App` published, the display's own ready monitors), read inside the
/// body and asserted after `run` returns — a panic inside the activate
/// handler would abort across the FFI boundary, not fail a test.
type Seen = (Vec<gdk::Monitor>, Vec<gdk::Monitor>);

#[test]
fn body_runs_on_activate() {
    let ran = Rc::new(Cell::new(false));
    let ran_writer = ran.clone();
    let seen: Rc<RefCell<Option<Seen>>> = Rc::new(RefCell::new(None));
    let seen_writer = seen.clone();

    App::new("mov.vibec0re.hytte.test")
        .run(move |app| {
            ran_writer.set(true);
            // Don't crash even if there are no monitors (CI/headless edge).
            let published = app.monitors().iter().map(|m| m.gdk().clone()).collect();
            let ready = gdk::Display::default()
                .map(|display| {
                    display
                        .monitors()
                        .iter::<gdk::Monitor>()
                        .filter_map(Result::ok)
                        .filter(|m| m.geometry().width() > 0 && m.geometry().height() > 0)
                        .collect()
                })
                .unwrap_or_default();
            *seen_writer.borrow_mut() = Some((published, ready));
            // Stop the app loop immediately so the test exits.
            app.quit();
        })
        .expect("run");

    assert!(ran.get(), "body closure did not run");

    // #1368: what `App` publishes is exactly the display's monitors GTK has
    // applied a `done` to (non-zero geometry) — under Xvfb, every one of
    // them, since X11 sets a monitor's geometry in the same call that lists
    // it. Pins the wiring the hermetic `watch_ready` tests in `app.rs` cannot
    // reach: an `App` that never fed its list through `watch_ready` publishes
    // nothing here while the display lists its screen.
    let (published, ready) = seen.borrow_mut().take().expect("body recorded");
    assert_eq!(
        published, ready,
        "App::monitors must list the display's ready monitors, in order"
    );
}
