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
            while started.elapsed() < Duration::from_millis(200) {
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
            let drift = (Local::now() - got).num_seconds().abs();
            assert!(drift <= 1, "drift {drift}s");
        })
        .unwrap();
}
