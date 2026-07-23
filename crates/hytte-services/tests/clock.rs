//! Sanity check: register the clock service, read its `now()` signal once,
//! assert we get a non-default `DateTime<Local>` (i.e. roughly "now").
//!
//! No display required — clock service relies only on glib timers, but
//! we still drive the main context manually here.

use chrono::Local;
use futures_signals::signal::SignalExt;
use gtk::glib;
use hytte_reactive::{registry, runtime};
use hytte_services::clock;
use std::time::Duration;

#[test]
fn now_emits_a_recent_timestamp() {
    glib::MainContext::default()
        .with_thread_default(|| {
            registry::reset_for_tests();
            registry::install(Box::new(clock::ClockService), runtime::handle());

            let signal = clock::now();
            let mut stream = signal.to_stream();

            let ctx = glib::MainContext::default();
            let started = std::time::Instant::now();
            let mut got: Option<chrono::DateTime<Local>> = None;
            let _rt_guard = runtime::handle().enter();
            // The clock's `Mutable` is seeded with `Local::now()` synchronously
            // at registration time, so under normal conditions the very first
            // poll already yields it — this deadline is a ceiling for a
            // slow/cold CI runner, not an expected wait. It costs nothing when
            // healthy since the loop exits the instant a value arrives.
            while started.elapsed() < Duration::from_secs(5) {
                ctx.iteration(false);
                if let Some(v) = futures_executor::block_on(async {
                    use futures_util::StreamExt as _;
                    tokio::time::timeout(Duration::from_millis(10), stream.next())
                        .await
                        .ok()
                        .flatten()
                }) {
                    got = Some(v);
                    break;
                }
            }
            let got = got.expect("clock signal never emitted");
            // Widened from a sub-second bound: the intent is "roughly now", not
            // sub-second accuracy — a slow/cold runner can burn several seconds
            // of the deadline above just getting scheduled, which would show up
            // here as drift despite the clock service itself being correct.
            let drift = (Local::now() - got).num_seconds().abs();
            assert!(drift <= 10, "drift {drift}s");
        })
        .unwrap();
}
