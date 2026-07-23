//! Input sources: list of audio inputs with default-radio, volume slider, and mute.
//!
//! Thin instantiation of the shared [`super::endpoint`] keyed-diff row list
//! (#443) — see there for the row/list implementation shared with `sinks.rs`.

use hytte::gtk;
use hytte::services::pipewire::{self, Source};

use super::endpoint::{Endpoint, build_endpoint_list};

impl Endpoint for Source {
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

pub(super) fn build_source_list() -> gtk::ListBox {
    build_endpoint_list(
        pipewire::sources(),
        pipewire::set_default_source,
        pipewire::set_source_volume,
        pipewire::set_source_mute,
    )
}
