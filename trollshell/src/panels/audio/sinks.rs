//! Output sinks: list of audio outputs with default-radio, volume slider, and mute.
//!
//! Thin instantiation of the shared [`super::endpoint`] keyed-diff row list
//! (#443) — see there for the row/list implementation shared with `sources.rs`.

use hytte::gtk;
use hytte::services::pipewire::{self, Sink};

use super::endpoint::{Endpoint, build_endpoint_list};

impl Endpoint for Sink {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn volume(&self) -> f64 {
        self.volume
    }
    fn muted(&self) -> bool {
        self.muted
    }
    fn is_default(&self) -> bool {
        self.is_default
    }
}

pub(super) fn build_sink_list() -> gtk::ListBox {
    build_endpoint_list(
        pipewire::sinks(),
        pipewire::set_default_sink,
        pipewire::set_sink_volume,
        pipewire::set_sink_mute,
    )
}
