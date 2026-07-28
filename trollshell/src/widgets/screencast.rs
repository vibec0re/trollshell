//! Screen-share privacy indicator — visible only while niri reports at
//! least one live screencast session (`niri::active_casts()`).
//!
//! **Click stops the cast** (#578). A screencast session is a privacy
//! signal, not a navigation target — there's no drawer `Page` for it and it
//! doesn't want one; the useful action is "make it stop". So the chip is
//! built with `chip::action_indicator`, whose click runs a closure rather
//! than opening a page (`chip::indicator` always wires a page; the original
//! #221 slice used a click-less `chip::static_indicator` for want of that
//! middle shape — this chip was its only caller, so #578 replaced it).
//!
//! The click sends niri's `Action::StopCast` once per distinct
//! `Cast::session_id` — niri stops every stream in a session, so a session
//! that fanned out into several `Cast`s must not be asked several times.
//! Only `CastKind::PipeWire` sessions are asked: niri's IPC cannot stop a
//! `WlrScreencopy` capture (wf-recorder, xdg-desktop-portal-wlr), so those
//! are filtered out and the tooltip says so instead of the click quietly
//! doing nothing.
//!
//! The chip stays sensitive even when nothing is stoppable, rather than
//! being greyed out: an insensitive widget is skipped by GTK's event pick,
//! and the tooltip naming *what* is capturing your screen is the chip's
//! primary job — it must not be the thing that gets sacrificed.
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
use hytte::services::niri::{self, Cast as NiriCast, CastKind, CastTarget, Window};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

pub fn widget(_monitor: &Monitor) -> gtk::Widget {
    // Live cast list, read at click time rather than captured as a snapshot —
    // the same `Rc<RefCell<…>>` idiom `widgets/tray.rs` uses so a handler
    // wired once at construction always acts on current data.
    let casts: Rc<RefCell<Vec<NiriCast>>> = Rc::new(RefCell::new(Vec::new()));

    let casts_for_click = Rc::clone(&casts);
    let btn = crate::components::chip::action_indicator("ts-screencast", move || {
        stop_all(&casts_for_click.borrow());
    });

    let icon = gtk::Image::from_icon_name("screen-shared-symbolic");
    btn.set_child(Some(&icon));

    bind_visible(niri::active_casts().map(|casts| !casts.is_empty()), &btn);

    // Keep the click handler's view of the casts current. The button is the
    // bind target only so the apply-loop dies with the widget — the closure
    // touches the cell, not the button.
    bind(niri::active_casts(), &btn, move |_, next: Vec<NiriCast>| {
        *casts.borrow_mut() = next;
    });

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

/// Ask niri to stop every stoppable cast in `casts`.
///
/// The selection is [`stoppable_sessions`]; this is the effectful half.
fn stop_all(casts: &[NiriCast]) {
    let sessions = stoppable_sessions(casts);
    if sessions.is_empty() {
        tracing::debug!(
            casts = casts.len(),
            "screencast chip clicked, but no cast is stoppable over niri IPC"
        );
        return;
    }
    for session_id in sessions {
        tracing::debug!(session_id, "stopping screencast");
        niri::stop_cast(session_id);
    }
}

/// The distinct `session_id`s in `casts` that niri's `StopCast` can act on,
/// in first-seen order.
///
/// A session with several streams surfaces as several [`NiriCast`]s but is
/// stopped whole by one request, so the ids are deduped rather than sent per
/// cast. `WlrScreencopy` casts are dropped: niri's IPC has no stop for them.
///
/// Pure function — no niri socket; unit-testable.
fn stoppable_sessions(casts: &[NiriCast]) -> Vec<u64> {
    let mut seen: HashSet<u64> = HashSet::new();
    casts
        .iter()
        .filter(|c| is_stoppable(c))
        .filter(|c| seen.insert(c.session_id))
        .map(|c| c.session_id)
        .collect()
}

/// Whether niri's `StopCast` can stop this cast — true only for `PipeWire`
/// sessions; `WlrScreencopy` captures have no IPC stop.
fn is_stoppable(cast: &NiriCast) -> bool {
    matches!(cast.kind, CastKind::PipeWire)
}

/// Build the chip's tooltip: one line per active cast, then a line telling
/// the user what a click will do — "Click to stop" when at least one cast is
/// stoppable, otherwise why it isn't.
fn tooltip_text(casts: &[NiriCast], windows: &[Window]) -> String {
    if casts.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = casts.iter().map(|c| describe_cast(c, windows)).collect();
    lines.push(
        if casts.iter().any(is_stoppable) {
            "Click to stop"
        } else {
            "Cannot be stopped from here (wlr-screencopy)"
        }
        .to_string(),
    );
    lines.join("\n")
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

#[cfg(test)]
mod tests {
    use super::{NiriCast, is_stoppable, stoppable_sessions, tooltip_text};
    use hytte::services::niri::{CastKind, CastTarget};

    fn mk_cast(stream_id: u64, session_id: u64, kind: CastKind) -> NiriCast {
        NiriCast {
            stream_id,
            session_id,
            kind,
            target: CastTarget::Output {
                name: "DP-1".to_string(),
            },
            is_dynamic_target: false,
            is_active: true,
            pid: None,
            pw_node_id: None,
        }
    }

    #[test]
    fn only_pipewire_is_stoppable() {
        assert!(is_stoppable(&mk_cast(1, 1, CastKind::PipeWire)));
        assert!(!is_stoppable(&mk_cast(1, 1, CastKind::WlrScreencopy)));
    }

    #[test]
    fn sessions_are_deduped_across_streams() {
        // One session, two streams — niri stops the session whole, so it must
        // be asked exactly once.
        let casts = [
            mk_cast(10, 7, CastKind::PipeWire),
            mk_cast(11, 7, CastKind::PipeWire),
        ];
        assert_eq!(stoppable_sessions(&casts), [7]);
    }

    #[test]
    fn sessions_keep_first_seen_order() {
        let casts = [
            mk_cast(10, 9, CastKind::PipeWire),
            mk_cast(11, 4, CastKind::PipeWire),
            mk_cast(12, 9, CastKind::PipeWire),
        ];
        assert_eq!(stoppable_sessions(&casts), [9, 4]);
    }

    #[test]
    fn wlr_screencopy_sessions_are_dropped() {
        let casts = [
            mk_cast(10, 1, CastKind::WlrScreencopy),
            mk_cast(11, 2, CastKind::PipeWire),
        ];
        assert_eq!(stoppable_sessions(&casts), [2]);
    }

    #[test]
    fn nothing_stoppable_yields_no_sessions() {
        let casts = [mk_cast(10, 1, CastKind::WlrScreencopy)];
        assert!(stoppable_sessions(&casts).is_empty());
    }

    #[test]
    fn tooltip_is_empty_with_no_casts() {
        assert_eq!(tooltip_text(&[], &[]), "");
    }

    #[test]
    fn tooltip_offers_stop_when_stoppable() {
        let casts = [mk_cast(10, 1, CastKind::PipeWire)];
        assert_eq!(
            tooltip_text(&casts, &[]),
            "Sharing output: DP-1\nClick to stop"
        );
    }

    #[test]
    fn tooltip_explains_when_nothing_is_stoppable() {
        let casts = [mk_cast(10, 1, CastKind::WlrScreencopy)];
        let text = tooltip_text(&casts, &[]);
        assert!(
            text.ends_with("Cannot be stopped from here (wlr-screencopy)"),
            "unexpected tooltip: {text}"
        );
    }

    #[test]
    fn tooltip_offers_stop_when_only_some_are_stoppable() {
        let casts = [
            mk_cast(10, 1, CastKind::WlrScreencopy),
            mk_cast(11, 2, CastKind::PipeWire),
        ];
        assert!(tooltip_text(&casts, &[]).ends_with("Click to stop"));
    }
}
