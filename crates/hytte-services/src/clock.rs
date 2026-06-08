//! Wall-clock service. Ticks a `Mutable<DateTime<Local>>` once per second
//! on the GTK main loop.

use chrono::{DateTime, Local};
use futures_signals::signal::{Mutable, Signal};
use gtk::glib;
use hytte_reactive::{Service, registry};
use std::time::Duration;

pub struct ClockService;

#[derive(Clone)]
#[doc(hidden)]
pub struct ClockHandles {
    pub(crate) now: Mutable<DateTime<Local>>,
}

impl Service for ClockService {
    type Handles = ClockHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = ClockHandles {
            now: Mutable::new(Local::now()),
        };
        let writer = handles.now.clone();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            writer.set(Local::now());
            glib::ControlFlow::Continue
        });
        handles
    }
}

#[must_use]
pub fn service() -> ClockService {
    ClockService
}

pub fn now() -> impl Signal<Item = DateTime<Local>> {
    registry::with(|r| {
        r.get::<ClockHandles>()
            .expect("clock::service() not registered")
            .now
            .signal_cloned()
    })
}
