//! Screen-share privacy indicator — visible only while niri reports at
//! least one live screencast session (`niri::active_casts()`).
//!
//! Non-interactive by design (the #221 triage's chosen default): a
//! screencast session is a privacy signal, not a navigation target, and
//! there's no drawer `Page` for it today. Built with
//! `chip::static_indicator` rather than `chip::indicator`, which always
//! wires a click-through into some panel — a click-less chip needs its own
//! constructor. Wiring a click (e.g. niri's `Action::StopCast`, which only
//! covers `PipeWire` casts and stops by `session_id`) is a deliberate future
//! follow-up, not done here.
//!
//! Mirrors `widgets/vpn.rs`'s hide-when-inactive shape. The visibility gate
//! is "a cast session exists" (`active_casts()` non-empty), not "actively
//! streaming frames": a paused cast (`Cast::is_active == false`, e.g. an
//! OBS scene switch) still holds a live capture session open, so it stays
//! counted — the safer default for a privacy affordance (over-warn rather
//! than under-warn).

use hytte::futures_signals::map_ref;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
// `Cast` is aliased: `gtk::prelude::*` brings glib's own `Cast` trait (which
// supplies `.upcast()`) into scope, and importing niri's `Cast` struct under
// the same name would shadow it.
use hytte::services::niri::{self, Cast as NiriCast, CastTarget, Window};

pub fn widget(_monitor: &Monitor) -> gtk::Widget {
    let btn = crate::components::chip::static_indicator("ts-screencast");

    let icon = gtk::Image::from_icon_name("screen-shared-symbolic");
    btn.set_child(Some(&icon));

    bind_visible(niri::active_casts().map(|casts| !casts.is_empty()), &btn);

    // Resolve `CastTarget::Window { id }` against the live window list so
    // the tooltip reads as a title rather than a bare niri window id.
    let tooltip = map_ref! {
        let casts = niri::active_casts(),
        let windows = niri::windows() =>
        tooltip_text(casts, windows)
    };
    bind(tooltip, &btn, |b, text: String| {
        b.set_tooltip_text(Some(&text));
    });

    btn.upcast()
}

/// Build the chip's tooltip: one line per active cast.
fn tooltip_text(casts: &[NiriCast], windows: &[Window]) -> String {
    casts
        .iter()
        .map(|c| describe_cast(c, windows))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Human-readable description of a single cast's target, for the tooltip.
fn describe_cast(cast: &NiriCast, windows: &[Window]) -> String {
    match &cast.target {
        CastTarget::Nothing {} => "Screen sharing starting…".to_string(),
        CastTarget::Output { name } => format!("Sharing output: {name}"),
        CastTarget::Window { id } => {
            let title = windows
                .iter()
                .find(|w| w.id == *id)
                .and_then(|w| w.title.as_deref())
                .unwrap_or("a window");
            format!("Sharing window: {title}")
        }
    }
}
