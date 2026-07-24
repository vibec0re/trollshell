use futures_signals::signal::{Mutable, SignalExt};
use hytte_reactive::registry::{self, Service};
use hytte_reactive::runtime;

struct ClockService;

#[derive(Default)]
struct ClockHandles {
    tick: Mutable<u32>,
}

impl Service for ClockService {
    type Handles = ClockHandles;
    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        ClockHandles::default()
    }
}

fn tick() -> impl futures_signals::signal::Signal<Item = u32> {
    registry::with(|r| {
        r.get::<ClockHandles>()
            .expect("ClockService not registered")
            .tick
            .signal()
    })
}

#[test]
fn registered_service_handles_round_trip() {
    registry::reset_for_tests();
    registry::install(Box::new(ClockService), runtime::handle());

    // Read the signal — should yield the default 0.
    let mut stream = tick().to_stream();
    futures_executor::block_on(async {
        let v = futures_util::StreamExt::next(&mut stream).await;
        assert_eq!(v, Some(0));
    });
}

#[test]
fn missing_service_panics_with_helpful_message() {
    registry::reset_for_tests();
    let panicked = std::panic::catch_unwind(|| {
        let _ = tick();
    });
    let msg = panicked
        .err()
        .and_then(|e| {
            e.downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
        })
        .unwrap_or_default();
    assert!(msg.contains("ClockService not registered"), "got: {msg}");
}

#[test]
fn registry_keys_by_type_with_absent() {
    use hytte_reactive::registry::Registry;

    let mut r = Registry::default();
    r.insert(7u32);
    r.insert(String::from("hi")); // a distinct type coexists under its own key

    assert_eq!(r.get::<u32>(), Some(&7));
    assert_eq!(r.get::<String>().map(String::as_str), Some("hi"));
    assert_eq!(r.get::<i64>(), None, "an unregistered type reads back None");
}

#[test]
#[should_panic(expected = "duplicate service registration")]
fn duplicate_insert_of_same_type_trips_the_tripwire() {
    use hytte_reactive::registry::Registry;

    // Re-inserting the same handle type is the `main.rs` double-`.with(…)`
    // mistake: it still overwrites (release), but now trips a `debug_assert!`
    // in debug/test builds so the stray registration surfaces loudly.
    let mut r = Registry::default();
    r.insert(7u32);
    r.insert(9u32);
}
