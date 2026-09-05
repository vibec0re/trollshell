//! Unit + per-connection integration tests for the plugin host transport.
//! Pulls the tested items from their respective submodules (all `pub(super)`,
//! so visible to this descendant module) and drives `handle_conn` end to end
//! over a `UnixStream::pair` socketpair.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::ui::{Dir as UiDir, EventKind as UiEventKind, Node as UiNode};
use hytte_plugin_proto::{
    AudioAction, Capability, ClockState, DatasourceError, DatasourceOutcome, Effect, HostMsg,
    Manifest, MediaAction, Mount, NiriAction, NowPlaying, Page, PluginMsg, ProvidedDatasource,
    StateKey, VOCAB, preem as vocab, read_frame, wire, write_frame,
};
use hytte_preem as kit;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use super::datasource::DatasourceRouter;
use super::effects::broker_effect;
use super::effects::{PageAction, map_page, map_page_for_layout, resolve_open_page};
use super::listener::{ACCEPT_BACKOFF, accept_backoff, socket_in_use};
use super::preem_render::{self, Scope};
use super::pump::{
    any_sidebar_open, apply_forget, apply_open, request_remap, request_remap_holding,
    tick_decision, tint_in_process_surfaces, to_now_playing, to_upcoming_events,
};
use super::region::{clear_region_if_owned, upsert_region};
use super::session::{
    EFFECT_BURST, EffectRateLimiter, IdGuard, OUTBOUND_CAPACITY, REGISTER_TIMEOUT,
    effect_capability, enforce_capabilities, handle_conn, push_gate, state_key_capability,
};
use super::wire_map::{clamp_pixels_scale, pixels_len_ok, to_ui_node, to_wire_event};
use super::{BrokeredEffect, ListenerCtx, SlotRender};

/// Regression for #426: the accept loop's error policy must be **total** —
/// every `accept(2)` error maps to a retry, never to loop termination.
/// Before the fix the `Err` arm did `return Err(e)`, so one transient
/// syscall error permanently killed the listener and stranded every plugin
/// against a dead socket until the shell restarted. A per-peer abort/reset
/// retries immediately (`None`); resource-pressure errors back off (`Some`).
#[test]
fn accept_error_never_terminates_the_loop() {
    use std::io::{Error, ErrorKind};

    // Per-peer hiccups: the listener is untouched, so retry immediately.
    for kind in [
        ErrorKind::ConnectionAborted,
        ErrorKind::ConnectionReset,
        ErrorKind::ConnectionRefused,
    ] {
        assert_eq!(
            accept_backoff(&Error::from(kind)),
            None,
            "{kind:?} should retry immediately, not terminate the loop",
        );
    }

    // Resource pressure (EMFILE/ENFILE/ENOBUFS/ENOMEM surface as `Other`,
    // OutOfMemory, etc.): still retryable, but after a short backoff so a
    // persistent error doesn't spin the loop hot.
    for kind in [
        ErrorKind::Other,
        ErrorKind::OutOfMemory,
        ErrorKind::PermissionDenied,
    ] {
        assert_eq!(
            accept_backoff(&Error::from(kind)),
            Some(ACCEPT_BACKOFF),
            "{kind:?} should back off and retry, not terminate the loop",
        );
    }
}

/// The `wire`→`hytte_ui` mapping is exhaustive over every node variant
/// (incl. `Box { scroll }` and nesting) and produces a field-for-field
/// mirror.
#[test]
#[allow(clippy::too_many_lines)] // one big paired tree literal; splitting hurts readability
fn wire_node_maps_to_ui_node_exhaustively() {
    let tree = wire::Node::Box {
        id: Some("root".into()),
        dir: wire::Dir::Vertical,
        spacing: 4,
        scroll: true,
        classes: vec!["ts-a".into()],
        children: vec![
            wire::Node::Label {
                id: None,
                text: "hi".into(),
                classes: vec!["ts-l".into()],
            },
            wire::Node::Icon {
                id: Some("i".into()),
                name: "battery-symbolic".into(),
                classes: vec![],
            },
            wire::Node::Pixels {
                id: Some("px".into()),
                width: 1,
                height: 1,
                data: vec![10, 20, 30, 255],
                scale: 2,
                classes: vec!["ts-lcd".into()],
            },
            wire::Node::Button {
                id: "b".into(),
                classes: vec!["ts-btn".into()],
                child: Box::new(wire::Node::Label {
                    id: None,
                    text: "go".into(),
                    classes: vec![],
                }),
            },
            wire::Node::Progress {
                id: None,
                fraction: 0.5,
                classes: vec![],
            },
            wire::Node::Slider {
                id: "sld".into(),
                min: 0.0,
                max: 1.0,
                value: 0.3,
                step: 0.1,
                enabled: false,
                classes: vec!["ts-slider".into()],
            },
            wire::Node::Revealer {
                id: Some("r".into()),
                open: true,
                child: Box::new(wire::Node::Separator {
                    classes: vec!["ts-sep".into()],
                }),
            },
            wire::Node::Box {
                id: None,
                dir: wire::Dir::Horizontal,
                spacing: 0,
                scroll: false,
                classes: vec![],
                children: vec![],
            },
            wire::Node::Spacer,
        ],
    };
    let expected = UiNode::Box {
        id: Some("root".into()),
        dir: UiDir::Vertical,
        spacing: 4,
        scroll: true,
        classes: vec!["ts-a".into()],
        children: vec![
            UiNode::Label {
                id: None,
                text: "hi".into(),
                classes: vec!["ts-l".into()],
            },
            UiNode::Icon {
                id: Some("i".into()),
                name: "battery-symbolic".into(),
                classes: vec![],
            },
            UiNode::Pixels {
                id: Some("px".into()),
                width: 1,
                height: 1,
                data: vec![10, 20, 30, 255],
                scale: 2,
                classes: vec!["ts-lcd".into()],
            },
            UiNode::Button {
                id: "b".into(),
                classes: vec!["ts-btn".into()],
                child: Box::new(UiNode::Label {
                    id: None,
                    text: "go".into(),
                    classes: vec![],
                }),
            },
            UiNode::Progress {
                id: None,
                fraction: 0.5,
                classes: vec![],
            },
            UiNode::Slider {
                id: "sld".into(),
                min: 0.0,
                max: 1.0,
                value: 0.3,
                step: 0.1,
                enabled: false,
                classes: vec!["ts-slider".into()],
            },
            UiNode::Revealer {
                id: Some("r".into()),
                open: true,
                child: Box::new(UiNode::Separator {
                    classes: vec!["ts-sep".into()],
                }),
            },
            UiNode::Box {
                id: None,
                dir: UiDir::Horizontal,
                spacing: 0,
                scroll: false,
                classes: vec![],
                children: vec![],
            },
            UiNode::Spacer,
        ],
    };
    assert_eq!(to_ui_node(&Scope::detached("map"), &tree), expected);
}

/// The list nodes map field-for-field: `Row`/`ListBox` recurse their
/// children like `Box`, and `Text` carries `max_width_chars` **and** the
/// #297 `ellipsize` flag. A `Spacer` between the cluster and the value maps
/// 1:1 (the justification primitive).
#[test]
fn wire_row_listbox_text_map_to_ui() {
    let tree = wire::Node::ListBox {
        id: Some("list".into()),
        classes: vec!["ts-list".into()],
        children: vec![wire::Node::Row {
            id: Some("r0".into()),
            classes: vec!["ts-row".into()],
            children: vec![
                wire::Node::Text {
                    id: None,
                    text: "an ellipsized destination".into(),
                    max_width_chars: Some(20),
                    ellipsize: true,
                    classes: vec!["ts-dest".into()],
                },
                wire::Node::Spacer,
                wire::Node::Label {
                    id: None,
                    text: "12:30".into(),
                    classes: vec!["ts-time".into()],
                },
            ],
        }],
    };
    let expected = UiNode::ListBox {
        id: Some("list".into()),
        classes: vec!["ts-list".into()],
        children: vec![UiNode::Row {
            id: Some("r0".into()),
            classes: vec!["ts-row".into()],
            children: vec![
                UiNode::Text {
                    id: None,
                    text: "an ellipsized destination".into(),
                    max_width_chars: Some(20),
                    ellipsize: true,
                    classes: vec!["ts-dest".into()],
                },
                UiNode::Spacer,
                UiNode::Label {
                    id: None,
                    text: "12:30".into(),
                    classes: vec!["ts-time".into()],
                },
            ],
        }],
    };
    assert_eq!(to_ui_node(&Scope::detached("map"), &tree), expected);
}

/// The #333 `Expander` maps 1:1: the boxed `header` and the body `children`
/// recurse, and the `expanded` mutable prop carries across.
#[test]
fn wire_expander_maps_to_ui() {
    let tree = wire::Node::Expander {
        id: "room".into(),
        header: Box::new(wire::Node::Label {
            id: None,
            text: "Living Room".into(),
            classes: vec!["heading".into()],
        }),
        children: vec![wire::Node::Label {
            id: Some("d".into()),
            text: "Lamp".into(),
            classes: vec![],
        }],
        expanded: true,
        classes: vec!["boxed-list".into()],
    };
    let expected = UiNode::Expander {
        id: "room".into(),
        header: Box::new(UiNode::Label {
            id: None,
            text: "Living Room".into(),
            classes: vec!["heading".into()],
        }),
        children: vec![UiNode::Label {
            id: Some("d".into()),
            text: "Lamp".into(),
            classes: vec![],
        }],
        expanded: true,
        classes: vec!["boxed-list".into()],
    };
    assert_eq!(to_ui_node(&Scope::detached("map"), &tree), expected);
}

#[test]
fn ui_event_maps_to_wire_event() {
    assert_eq!(to_wire_event(UiEventKind::Click), wire::EventKind::Click);
    assert_eq!(
        to_wire_event(UiEventKind::Scroll { dx: 1.5, dy: -2.0 }),
        wire::EventKind::Scroll { dx: 1.5, dy: -2.0 }
    );
    assert_eq!(
        to_wire_event(UiEventKind::ValueChanged { value: 0.42 }),
        wire::EventKind::ValueChanged { value: 0.42 }
    );
    assert_eq!(
        to_wire_event(UiEventKind::Submitted {
            text: "help".into()
        }),
        wire::EventKind::Submitted {
            text: "help".into()
        }
    );
}

/// The #357 `Entry` maps 1:1: the required id (the `Submitted` event
/// target), the `text` echo prop, and the placeholder all carry across.
#[test]
fn wire_entry_maps_to_ui() {
    let tree = wire::Node::Entry {
        id: "term-input".into(),
        text: String::new(),
        placeholder: "type a command…".into(),
        classes: vec!["monospace".into()],
    };
    let expected = UiNode::Entry {
        id: "term-input".into(),
        text: String::new(),
        placeholder: "type a command…".into(),
        classes: vec!["monospace".into()],
    };
    assert_eq!(to_ui_node(&Scope::detached("map"), &tree), expected);
}

/// Every wire `Page` maps to the identically-named `modal::Page` in the
/// combined/multicolumn layouts (#508: an exact 1:1 match, since those layouts
/// keep the single `Stats` page). Uses the pure `map_page_for_layout` so the
/// assertion doesn't depend on `TROLLSHELL_STATS_LAYOUT` in the test env.
#[test]
fn wire_page_maps_to_modal_page() {
    use crate::modal::Page as M;
    use crate::panels::stats::StatsLayout;
    let cases = [
        (Page::Media, M::Media),
        (Page::Network, M::Network),
        (Page::Vpn, M::Vpn),
        (Page::Connections, M::Connections),
        (Page::Bluetooth, M::Bluetooth),
        (Page::Stats, M::Stats),
        (Page::Audio, M::Audio),
        (Page::Power, M::Power),
        (Page::PowerMenu, M::PowerMenu),
        (Page::Notifications, M::Notifications),
        (Page::Appearance, M::Appearance),
        (Page::Displays, M::Displays),
        (Page::Clipboard, M::Clipboard),
        (Page::Calendar, M::Calendar),
        (Page::Settings, M::Settings),
    ];
    for (wire_page, modal_page) in cases {
        assert_eq!(
            map_page_for_layout(wire_page, StatsLayout::Combined),
            modal_page
        );
        assert_eq!(
            map_page_for_layout(wire_page, StatsLayout::Multicolumn),
            modal_page
        );
    }
}

/// In the `split` layout (#508), the wire protocol's single `Stats` page lands
/// on the host's CPU flyout (`StatsCpu`) — the #307 approximation — while every
/// other page stays a 1:1 match. `map_page` itself (env-read) resolves to the
/// combined `Stats` in the hermetic test env (no env var set), guarding the
/// default path.
#[test]
fn split_layout_maps_stats_to_cpu() {
    use crate::modal::Page as M;
    use crate::panels::stats::StatsLayout;
    assert_eq!(
        map_page_for_layout(Page::Stats, StatsLayout::Split),
        M::StatsCpu
    );
    // A non-Stats page is layout-independent.
    assert_eq!(
        map_page_for_layout(Page::Media, StatsLayout::Split),
        M::Media
    );
    // Default (env unset in tests) is the combined Stats.
    assert_eq!(map_page(Page::Stats), M::Stats);
}

/// #349 PR2: `resolve_open_page` is the pure seam the broker uses to split a
/// built-in page-open from the `PluginSelf` self-panel open. A built-in page
/// resolves to its `modal::Page`; `PluginSelf` resolves to the plugin-self
/// action (which the broker dispatches with the effect's plugin id) and never
/// reaches `map_page`'s `unreachable!` arm.
#[test]
fn resolve_open_page_splits_pluginself_from_builtin() {
    assert!(
        matches!(
            resolve_open_page(Page::Media),
            PageAction::OpenBuiltin(crate::modal::Page::Media)
        ),
        "a built-in page resolves to its modal::Page",
    );
    assert!(
        matches!(
            resolve_open_page(Page::Settings),
            PageAction::OpenBuiltin(crate::modal::Page::Settings)
        ),
        "another built-in page resolves to its modal::Page",
    );
    assert!(
        matches!(
            resolve_open_page(Page::PluginSelf),
            PageAction::OpenPluginSelf
        ),
        "PluginSelf resolves to the plugin-self action, not a builtin",
    );
}

/// One plugin card carrying an id/order/generation + a label tree, for the
/// region tests below.
fn render_of(
    plugin_id: &str,
    order: i32,
    generation: u64,
    text: &str,
    tx: &mpsc::Sender<HostMsg>,
) -> SlotRender {
    SlotRender {
        plugin_id: plugin_id.to_owned(),
        order,
        generation,
        tree: wire::Node::Label {
            id: None,
            text: text.to_owned(),
            classes: vec![],
        },
        panel: None,
        outbound: tx.clone(),
    }
}

/// Like [`render_of`], but the card also carries a distinct drawer `panel`
/// tree (a `Label` with the given panel text) — for the panels-mailbox tests.
fn render_with_panel(
    plugin_id: &str,
    order: i32,
    generation: u64,
    chip: &str,
    panel: &str,
    tx: &mpsc::Sender<HostMsg>,
) -> SlotRender {
    SlotRender {
        panel: Some(wire::Node::Label {
            id: Some("panel".into()),
            text: panel.to_owned(),
            classes: vec![],
        }),
        ..render_of(plugin_id, order, generation, chip, tx)
    }
}

fn label_text(render: &SlotRender) -> &str {
    match &render.tree {
        wire::Node::Label { text, .. } => text,
        other => panic!("expected a Label, got {other:?}"),
    }
}

/// A region keeps **one card per plugin id** (a plugin's re-render coalesces
/// its own card, latest-wins) and stays sorted by `(order, plugin_id)`.
#[test]
fn upsert_region_coalesces_per_plugin_and_sorts() {
    let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    let region: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
    // Arrive out of order; "alpha" renders twice.
    upsert_region(&region, render_of("bravo", 0, 0, "b1", &tx));
    upsert_region(&region, render_of("alpha", 0, 0, "a1", &tx));
    upsert_region(&region, render_of("alpha", 0, 0, "a2", &tx));

    let cards = region.lock_ref();
    assert_eq!(cards.len(), 2, "one card per plugin id (alpha coalesced)");
    assert_eq!(cards[0].plugin_id, "alpha", "sorted by (order, id)");
    assert_eq!(cards[1].plugin_id, "bravo");
    assert_eq!(label_text(&cards[0]), "a2", "alpha's latest tree wins");
}

/// `(order, id)` ordering: lower `order` first; `None` (mapped to `0` by the
/// reader) ties with `order: 0` and breaks on the stable id.
#[test]
fn region_orders_by_order_then_id() {
    let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    let region: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
    // pet requests order 5 (renders later); clock had no order → 0; aaa → 0.
    upsert_region(&region, render_of("pet", 5, 0, "", &tx));
    upsert_region(&region, render_of("clock", 0, 1, "", &tx)); // None → 0
    upsert_region(&region, render_of("aaa", 0, 2, "", &tx)); // ties clock on order

    let ids: Vec<String> = region
        .lock_ref()
        .iter()
        .map(|c| c.plugin_id.clone())
        .collect();
    // (0,"aaa") < (0,"clock") < (5,"pet")
    assert_eq!(ids, vec!["aaa", "clock", "pet"]);
}

/// #349 PR2: the dedicated panels mailbox reuses `upsert_region`/
/// `clear_region_if_owned`, so it inherits their guarantees for free — a
/// plugin's re-render coalesces its own panel latest-wins, and a stale
/// (lower-generation) teardown never evicts a fast-reconnect successor's
/// panel (the #278 generation guard, now covering panels).
#[test]
fn panel_upsert_coalesces_and_teardown_is_generation_scoped() {
    let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    let panels: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());

    // The plugin renders its panel twice on generation 0: coalesced in place.
    upsert_region(&panels, render_with_panel("pet", 0, 0, "chip", "p1", &tx));
    upsert_region(&panels, render_with_panel("pet", 0, 0, "chip", "p2", &tx));
    assert_eq!(panels.lock_ref().len(), 1, "one panel entry per plugin id");
    assert!(
        matches!(
            &panels.lock_ref()[0].panel,
            Some(wire::Node::Label { text, .. }) if text == "p2"
        ),
        "the plugin's latest panel wins",
    );

    // A fast reconnect (generation 1) replaces the entry; the OLD
    // connection's teardown (generation 0) must NOT evict the successor.
    upsert_region(&panels, render_with_panel("pet", 0, 1, "chip", "p3", &tx));
    clear_region_if_owned(&panels, "pet", 0);
    assert_eq!(
        panels.lock_ref().len(),
        1,
        "a stale-generation teardown leaves the successor's panel",
    );

    // The owning teardown (generation 1) clears it.
    clear_region_if_owned(&panels, "pet", 1);
    assert!(
        panels.lock_ref().is_empty(),
        "the owning connection's teardown clears the panel",
    );
}

/// A `Pixels` node whose buffer size violates `width*height*4` must not
/// reach the widget: the host validation seam degrades it to an empty
/// (0×0, no data) surface, preserving id + classes so a later valid frame
/// updates in place. A well-formed buffer passes through 1:1.
#[test]
fn pixels_bad_len_degrades_to_empty_surface() {
    assert!(pixels_len_ok(2, 2, 16));
    assert!(!pixels_len_ok(2, 2, 15));
    // Overflow-safe: absurd dims against a small buffer just report false.
    assert!(!pixels_len_ok(u32::MAX, u32::MAX, 4));

    let bad = wire::Node::Pixels {
        id: Some("lcd".into()),
        width: 2,
        height: 2,
        data: vec![0, 1, 2], // 3 bytes, needs 16
        scale: 2,
        classes: vec!["ts-lcd".into()],
    };
    assert_eq!(
        to_ui_node(&Scope::detached("pixels"), &bad),
        UiNode::Pixels {
            id: Some("lcd".into()),
            width: 0,
            height: 0,
            data: vec![],
            // The degraded (empty) surface renders nothing; scale is inert
            // there, so it normalizes to 1.
            scale: 1,
            classes: vec!["ts-lcd".into()],
        },
        "malformed Pixels degrades to a nothing-rendered surface",
    );

    let good = wire::Node::Pixels {
        id: None,
        width: 1,
        height: 2,
        data: vec![1, 2, 3, 4, 5, 6, 7, 8], // 1*2*4
        scale: 1,
        classes: vec![],
    };
    assert_eq!(
        to_ui_node(&Scope::detached("pixels"), &good),
        UiNode::Pixels {
            id: None,
            width: 1,
            height: 2,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
            scale: 1,
            classes: vec![],
        },
        "well-formed Pixels passes through 1:1",
    );
}

/// The #358 `scale` hint crosses the same trust boundary as the buffer:
/// sane integer scales pass through, `0` aliases to `1`, and an absurd
/// scale is clamped so the scaled natural dimension can never exceed the
/// host's cap.
#[test]
fn pixels_scale_is_clamped_at_the_host_seam() {
    // Pure clamp behavior.
    assert_eq!(clamp_pixels_scale(128, 128, 2), 2, "sane scale passes");
    assert_eq!(clamp_pixels_scale(128, 128, 0), 1, "0 aliases to 1");
    assert_eq!(
        clamp_pixels_scale(128, 128, u32::MAX),
        16_384 / 128,
        "absurd scale clamps to the scaled-dimension cap"
    );
    assert_eq!(
        clamp_pixels_scale(20_000, 1, 3),
        1,
        "an already-over-cap buffer keeps scale 1"
    );
    assert_eq!(clamp_pixels_scale(0, 0, 7), 1, "empty surface: inert 1");

    // Through the mapping arm: the caw case — a 1×1 stand-in at 2× passes
    // untouched; a hostile scale on the same node is capped.
    let node = |scale: u32| wire::Node::Pixels {
        id: Some("lcd".into()),
        width: 1,
        height: 1,
        data: vec![9, 9, 9, 255],
        scale,
        classes: vec![],
    };
    let ui_scale = |n: &wire::Node| match to_ui_node(&Scope::detached("scale"), n) {
        UiNode::Pixels { scale, .. } => scale,
        other => panic!("expected Pixels, got {other:?}"),
    };
    assert_eq!(ui_scale(&node(2)), 2);
    assert_eq!(ui_scale(&node(0)), 1);
    assert_eq!(ui_scale(&node(u32::MAX)), 16_384);
}

/// #904: the `Progress`/`Slider` arms cross the same trust boundary as
/// `Pixels`, and a non-finite `f64` on either is worse than a bad buffer —
/// `gtk_progress_bar_set_fraction` stores a `NaN` verbatim and
/// `gtk::Adjustment::new` returns NULL for a degenerate range, which the gtk4
/// binding turns into a debug-build panic. The arms therefore run the proto's
/// sanitiser (`wire::sane_fraction` / `wire::sane_slider_floats`), and a
/// sanitised node compares equal to itself so the reconciler's diff can
/// short-circuit again.
#[test]
fn progress_and_slider_floats_are_sanitised_at_the_host_seam() {
    let scope = Scope::detached("floats");
    let progress = |fraction: f64| wire::Node::Progress {
        id: Some("bar".into()),
        fraction,
        classes: vec![],
    };
    let ui_fraction = |n: &wire::Node| match to_ui_node(&scope, n) {
        UiNode::Progress { fraction, .. } => fraction,
        other => panic!("expected Progress, got {other:?}"),
    };
    assert_eq!(
        ui_fraction(&progress(f64::NAN)).to_bits(),
        0.0_f64.to_bits(),
        "a NaN fraction must never reach gtk::ProgressBar"
    );
    assert_eq!(
        ui_fraction(&progress(f64::INFINITY)).to_bits(),
        1.0_f64.to_bits()
    );
    assert_eq!(ui_fraction(&progress(2.5)).to_bits(), 1.0_f64.to_bits());
    assert_eq!(
        ui_fraction(&progress(0.42)).to_bits(),
        0.42_f64.to_bits(),
        "a legal fraction passes through untouched"
    );

    // The inverted range is the crashing case, not merely the churning one.
    let slider = |min: f64, max: f64, value: f64, step: f64| wire::Node::Slider {
        id: "sld".into(),
        min,
        max,
        value,
        step,
        enabled: true,
        classes: vec![],
    };
    let mapped = to_ui_node(&scope, &slider(10.0, 5.0, f64::NAN, 0.0));
    let UiNode::Slider {
        min,
        max,
        value,
        step,
        ..
    } = mapped
    else {
        panic!("expected Slider")
    };
    assert_eq!(min.to_bits(), 0.0_f64.to_bits(), "min: got {min}");
    assert_eq!(max.to_bits(), 1.0_f64.to_bits(), "max: got {max}");
    assert_eq!(value.to_bits(), 0.0_f64.to_bits(), "value: got {value}");
    assert_eq!(step.to_bits(), 0.01_f64.to_bits(), "step: got {step}");

    // A legal slider is left exactly alone, so the seam costs nothing normal.
    let legal = slider(0.0, 1.0, 0.3, 0.1);
    assert_eq!(
        to_ui_node(&scope, &legal),
        UiNode::Slider {
            id: "sld".into(),
            min: 0.0,
            max: 1.0,
            value: 0.3,
            step: 0.1,
            enabled: true,
            classes: vec![],
        }
    );
}

/// #277 (preserved under the region model): a plugin's back-to-back frames
/// coalesce its region card latest-wins, but a one-shot effect bundled on the
/// superseded frame rides the dedicated **global** non-lossy channel and is
/// delivered exactly once — not dropped, not duplicated.
#[test]
fn effects_survive_region_coalescing() {
    let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    let (eff_tx, mut eff_rx) = mpsc::unbounded_channel::<BrokeredEffect>();
    let region: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());

    // Frame A: a click's effect goes on the effect channel, then the tree is
    // parked. Frame B (a tick microseconds later) coalesces A's tree away.
    eff_tx
        .send(BrokeredEffect {
            plugin_id: "p".into(),
            effect: Effect::OpenPage(Page::PowerMenu),
            outbound: tx.clone(),
        })
        .expect("effect queued");
    upsert_region(&region, render_of("p", 0, 0, "A", &tx));
    upsert_region(&region, render_of("p", 0, 0, "B", &tx));

    // The region observes only B for plugin p (load-shedding by design)…
    {
        let cards = region.lock_ref();
        assert_eq!(cards.len(), 1);
        assert_eq!(label_text(&cards[0]), "B");
    }
    // …but the effect survived, exactly once, in order.
    let got = eff_rx.try_recv().expect("effect not dropped by coalescing");
    assert_eq!(got.plugin_id, "p");
    assert!(matches!(got.effect, Effect::OpenPage(Page::PowerMenu)));
    assert!(eff_rx.try_recv().is_err(), "effect must not be duplicated");
}

/// #274 removal semantics: a plugin's teardown removes only *its own* card;
/// a sibling plugin's card is keyed by a different id and stays put.
#[test]
fn per_plugin_teardown_leaves_siblings() {
    let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    let region: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
    upsert_region(&region, render_of("a", 0, 10, "", &tx));
    upsert_region(&region, render_of("b", 0, 11, "", &tx));

    clear_region_if_owned(&region, "a", 10);

    let cards = region.lock_ref();
    assert_eq!(cards.len(), 1, "only plugin a's card removed");
    assert_eq!(cards[0].plugin_id, "b", "sibling b undisturbed");
}

/// #278 (preserved, now per plugin-id entry): a stale teardown (older
/// generation) must never evict a fast-reconnect successor of the SAME
/// plugin id; only the owning generation's own teardown clears the card.
#[test]
fn stale_teardown_never_evicts_same_id_successor() {
    let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    let region: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
    // conn gen 1 parks plugin p; a fast reconnect (gen 2) replaces its card.
    upsert_region(&region, render_of("p", 0, 1, "v1", &tx));
    upsert_region(&region, render_of("p", 0, 2, "v2", &tx));

    // The old connection's teardown (gen 1) must NOT evict the gen-2 card.
    clear_region_if_owned(&region, "p", 1);
    {
        let cards = region.lock_ref();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].generation, 2, "successor survives stale teardown");
        assert_eq!(label_text(&cards[0]), "v2");
    }

    // The owning connection's own teardown (gen 2) does clear it.
    clear_region_if_owned(&region, "p", 2);
    assert!(
        region.lock_ref().is_empty(),
        "owning teardown clears the card"
    );
}

/// #288: slot visibility is the **OR across monitors** — a plugin's card
/// mirrors onto every monitor's sidebar, so it's visible while any one is
/// open. Walks the multi-monitor open/close lifecycle through the pure
/// aggregation helpers (`apply_open` returns the recomputed aggregate).
#[test]
fn slot_visibility_is_or_across_monitors() {
    let mut map = HashMap::new();
    // No monitors tracked yet → not visible.
    assert!(!any_sidebar_open(&map));

    // Two monitors install, both closed → not visible.
    assert!(!apply_open(&mut map, "DP-1", false));
    assert!(!apply_open(&mut map, "HDMI-A-1", false));

    // Open one → visible (OR); the other opening too stays visible.
    assert!(apply_open(&mut map, "DP-1", true));
    assert!(apply_open(&mut map, "HDMI-A-1", true));

    // Close one while the other stays open → still visible.
    assert!(apply_open(&mut map, "DP-1", false));
    // Close the last open sidebar → not visible.
    assert!(!apply_open(&mut map, "HDMI-A-1", false));
}

/// #288: hot-unplug of the monitor holding the **only** open sidebar must
/// drop visibility to `false` — its flag leaves the OR entirely, it isn't
/// merely set closed.
#[test]
fn hot_unplug_of_only_open_monitor_drops_visibility() {
    let mut map = HashMap::new();
    apply_open(&mut map, "DP-1", true);
    apply_open(&mut map, "HDMI-A-1", false);
    assert!(any_sidebar_open(&map), "one open sidebar → visible");

    // The monitor with the only open sidebar disappears → visibility drops.
    assert!(!apply_forget(&mut map, "DP-1"));
    // Forgetting the remaining (closed) monitor leaves it not visible + empty.
    assert!(!apply_forget(&mut map, "HDMI-A-1"));
    assert!(map.is_empty(), "forgotten monitors leave no stale entries");
}

/// A bar mount is a real wire variant the reader routes to its own region
/// (#349); assert the sidebar and bar mounts are all distinct from each other
/// so the `handle_conn` match can't confuse two.
#[test]
fn sidebar_and_bar_mounts_are_distinct() {
    assert_ne!(Mount::SidebarLead, Mount::SidebarTop);
    assert_ne!(Mount::SidebarLead, Mount::SidebarBottom);
    assert_ne!(Mount::SidebarTop, Mount::BarLeft);
    assert_ne!(Mount::SidebarBottom, Mount::BarCenter);
    // Effect + StateKey are exercised elsewhere; touch them here so the test
    // module's imports stay honest if the broker/pump code is refactored.
    assert_ne!(StateKey::Clock, StateKey::SlotVisible);
    assert!(matches!(Effect::OpenPage(Page::Media), Effect::OpenPage(_)));
}

/// #301 teardown isolation across the **three** sidebar regions: a plugin's
/// teardown probes all three (`handle_conn` calls `clear_region_if_owned` on
/// each), and clearing the region it actually lives in leaves the other two
/// regions' cards untouched. Mirrors the two-region isolation guarantees on
/// the new lead region.
#[test]
fn teardown_is_isolated_across_the_three_regions() {
    let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    let lead: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
    let top: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
    let bottom: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
    upsert_region(&lead, render_of("weather", -10, 0, "", &tx));
    upsert_region(&top, render_of("pet", 0, 1, "", &tx));
    upsert_region(&bottom, render_of("departures", 0, 2, "", &tx));

    // Weather (lead region) tears down: `handle_conn` probes every region.
    clear_region_if_owned(&lead, "weather", 0);
    clear_region_if_owned(&top, "weather", 0);
    clear_region_if_owned(&bottom, "weather", 0);

    assert!(lead.lock_ref().is_empty(), "weather's lead card removed");
    assert_eq!(top.lock_ref()[0].plugin_id, "pet", "pet (top) untouched");
    assert_eq!(
        bottom.lock_ref()[0].plugin_id,
        "departures",
        "departures (bottom) untouched"
    );
}

// ── Host session gating (#305): the SlotVisibility push is opt-in ─────────

/// Read one host→plugin frame, failing (not hanging) if none arrives.
///
/// [`HostMsg::Hello`] is skipped: since #883 the host advertises its wire
/// vocabulary as the first frame after an accepted `Register`, and every test
/// below builds its fixture from `Manifest::new` (which stamps `vocab_max`), so
/// they would all see it before the state frame they are actually about. The
/// advertisement's own *presence and absence* are asserted directly, off raw
/// `read_frame`, by `a_negotiating_plugin_is_told_the_hosts_vocabulary_first`
/// and `a_legacy_plugin_is_never_sent_the_vocabulary_advertisement` — so
/// skipping it here loses no coverage of the send-gate.
async fn recv<R>(rd: &mut R) -> HostMsg
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_frame::<HostMsg, _>(rd),
        )
        .await
        .expect("a host frame within 5s")
        .expect("decode HostMsg");
        if !matches!(msg, HostMsg::Hello { .. }) {
            return msg;
        }
    }
}

fn ctx_with(
    clock_rx: watch::Receiver<Option<ClockState>>,
    visibility_rx: watch::Receiver<bool>,
) -> (ListenerCtx, mpsc::UnboundedReceiver<BrokeredEffect>) {
    let (effects_tx, effects_rx) = mpsc::unbounded_channel();
    // Accent is unresolved in the host-session tests (they exercise the
    // clock/visibility gates), and none of them subscribes `StateKey::Accent`,
    // so no accent task ever reads this; seed `None` and let the sender drop.
    let (_accent_tx, accent_rx) = watch::channel(None);
    // Likewise for the audio spectrum (#405): these tests subscribe neither
    // `StateKey::AudioSpectrum`, so no spectrum task reads this.
    let (_spectrum_tx, spectrum_rx) = watch::channel(None);
    // The #484/#528 domain digests: seeded to their defaults; these tests don't
    // subscribe the domain keys, so no calendar/locked/now-playing task reads them.
    let (_calendar_tx, calendar_rx) = watch::channel(Vec::new());
    let (_now_playing_tx, now_playing_rx) = watch::channel(NowPlaying::default());
    let (_locked_tx, locked_rx) = watch::channel(false);
    let ctx = ListenerCtx {
        sidebar_lead: Mutable::new(Vec::new()),
        sidebar_top: Mutable::new(Vec::new()),
        sidebar_bottom: Mutable::new(Vec::new()),
        bar_left: Mutable::new(Vec::new()),
        bar_center: Mutable::new(Vec::new()),
        bar_right: Mutable::new(Vec::new()),
        panels: Mutable::new(Vec::new()),
        clock_rx,
        visibility_rx,
        accent_rx,
        spectrum_rx,
        calendar_rx,
        now_playing_rx,
        locked_rx,
        live_ids: Arc::new(Mutex::new(HashSet::new())),
        // Host-scoped runtime mirror (#423); like `live_ids`, kept per-ctx so the
        // per-connection tests stay isolated and never publish `PLUGIN_RUNTIME`.
        runtime: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
        effects_tx,
        // Host-scoped datasource router (#509); like `live_ids`/`runtime`, kept
        // per-ctx so the per-connection tests stay isolated.
        datasource: DatasourceRouter::default(),
    };
    (ctx, effects_rx)
}

/// Poll a region mailbox until it holds at least one card — the reader task
/// in `handle_conn` fills it asynchronously — failing (not hanging) if it
/// never populates. The `lock_ref` guard is dropped before each `await`, so
/// it never crosses a yield point.
async fn wait_for_region(region: &Mutable<Vec<SlotRender>>) -> Vec<SlotRender> {
    for _ in 0..200 {
        {
            let cards = region.lock_ref();
            if !cards.is_empty() {
                return cards.clone();
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("region never populated within timeout");
}

/// #349: a `Bar*`-mounted plugin's render must now reach the matching bar
/// region mailbox instead of being dropped (the v1 behavior this PR replaces).
/// Registers a `BarCenter` plugin, sends one `Render`, and asserts the card
/// lands in `bar_center` — and *only* there (no leak into the sibling bar
/// regions or a sidebar). Proves the un-defer end to end through `handle_conn`,
/// the same socketpair harness the visibility-gating tests use.
#[tokio::test]
async fn bar_mount_render_reaches_bar_region() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);
    // Clone the region handles (Mutable shares its state) before `ctx` moves
    // into the connection task, so the test can inspect the mailboxes after.
    let bar_center = ctx.bar_center.clone();
    let bar_left = ctx.bar_left.clone();
    let bar_right = ctx.bar_right.clone();
    let sidebar_top = ctx.sidebar_top.clone();
    let panels = ctx.panels.clone();

    let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host_end, &ctx).await });

    let (_prd, mut pwr) = plugin_end.into_split();
    write_frame(
        &mut pwr,
        &PluginMsg::Register {
            manifest: Manifest::new("barchip", Mount::BarCenter),
        },
    )
    .await
    .expect("send Register");
    write_frame(
        &mut pwr,
        &PluginMsg::Render {
            tree: wire::Node::Label {
                id: Some("t".into()),
                text: "chip".into(),
                classes: vec![],
            },
            // A panel-less render: the chip lands in its bar region, and the
            // dedicated panels mailbox (#349 PR2) must stay empty.
            panel: None,
            effects: vec![],
        },
    )
    .await
    .expect("send Render");

    let cards = wait_for_region(&bar_center).await;
    assert_eq!(
        cards.len(),
        1,
        "the BarCenter render reached bar_center (not dropped)"
    );
    assert_eq!(cards[0].plugin_id, "barchip");
    assert!(
        matches!(&cards[0].tree, wire::Node::Label { text, .. } if text == "chip"),
        "the plugin's view tree survived intact into the bar region",
    );

    // Routed to exactly one region: the sibling bar regions and the sidebar
    // stay empty (a BarCenter mount must not fan out or fall back).
    assert!(
        bar_left.lock_ref().is_empty(),
        "BarCenter didn't leak into BarLeft"
    );
    assert!(
        bar_right.lock_ref().is_empty(),
        "BarCenter didn't leak into BarRight"
    );
    assert!(
        sidebar_top.lock_ref().is_empty(),
        "a bar mount didn't leak into a sidebar region"
    );
    assert!(
        panels.lock_ref().is_empty(),
        "a panel-less render never touches the panels mailbox (#349 PR2)"
    );
}

/// #349 PR2: a render carrying a `panel` must reach BOTH the plugin's chip
/// region AND the dedicated panels mailbox — the chip renders inline while
/// the panel is available for the drawer child. A subsequent panel-less
/// render (the plugin dropping its panel) clears the panels entry but leaves
/// the chip. Drives it end to end through `handle_conn` on the socketpair
/// harness the other host tests use.
#[tokio::test]
async fn panel_render_populates_panels_mailbox() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);
    let bar_center = ctx.bar_center.clone();
    let panels = ctx.panels.clone();

    let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host_end, &ctx).await });

    let (_prd, mut pwr) = plugin_end.into_split();
    write_frame(
        &mut pwr,
        &PluginMsg::Register {
            manifest: Manifest::new("panelplug", Mount::BarCenter),
        },
    )
    .await
    .expect("send Register");
    // A panel-bearing render: a chip tree PLUS a distinct panel tree.
    write_frame(
        &mut pwr,
        &PluginMsg::Render {
            tree: wire::Node::Label {
                id: Some("chip".into()),
                text: "chip".into(),
                classes: vec![],
            },
            panel: Some(wire::Node::Label {
                id: Some("panel".into()),
                text: "panel body".into(),
                classes: vec![],
            }),
            effects: vec![],
        },
    )
    .await
    .expect("send panel Render");

    // The chip reaches its bar region…
    let chips = wait_for_region(&bar_center).await;
    assert_eq!(chips.len(), 1);
    assert!(
        matches!(&chips[0].tree, wire::Node::Label { text, .. } if text == "chip"),
        "the chip tree reached the bar region",
    );
    // …and the panel reaches the dedicated panels mailbox, tree intact.
    let panel_cards = wait_for_region(&panels).await;
    assert_eq!(panel_cards.len(), 1, "the panel reached the panels mailbox");
    assert_eq!(panel_cards[0].plugin_id, "panelplug");
    assert!(
        matches!(
            &panel_cards[0].panel,
            Some(wire::Node::Label { text, .. }) if text == "panel body"
        ),
        "the panel tree survived intact into the panels mailbox",
    );

    // Now the plugin drops its panel (Some→None): the panels entry clears,
    // but its chip stays in the bar region.
    write_frame(
        &mut pwr,
        &PluginMsg::Render {
            tree: wire::Node::Label {
                id: Some("chip".into()),
                text: "chip2".into(),
                classes: vec![],
            },
            panel: None,
            effects: vec![],
        },
    )
    .await
    .expect("send panel-less Render");

    // Poll until the panels mailbox drains (the reader clears it async).
    let mut cleared = false;
    for _ in 0..200 {
        if panels.lock_ref().is_empty() {
            cleared = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        cleared,
        "dropping the panel (Some→None) clears the panels entry"
    );
    assert_eq!(
        bar_center.lock_ref().len(),
        1,
        "the chip stays in the bar region after the panel is dropped",
    );
}

/// #305: a connection that does **not** subscribe `SlotVisible` must never
/// receive a `SlotVisibility` frame — not the register seed, not an edge. The
/// `Clock` snapshot is the ordered control channel: the plugin subscribes
/// Clock only, so its only frames are clock snapshots; a visibility edge
/// driven mid-stream produces nothing, and the very next frame the plugin
/// sees is the following clock snapshot — proving the edge was *filtered*, not
/// merely late. (This is the vibectl crash-loop, prevented.)
#[tokio::test]
async fn visibility_push_gated_off_when_not_subscribed() {
    let (clock_tx, clock_rx) = watch::channel(Some(ClockState {
        iso: "t0".into(),
        unix: 0,
    }));
    let (vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);

    let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host_end, &ctx).await });

    let (mut prd, mut pwr) = plugin_end.into_split();
    // A legacy-shaped plugin: subscribes Clock, NOT SlotVisible.
    let mut manifest = Manifest::new("legacy", Mount::SidebarTop);
    manifest.subscribes = vec![StateKey::Clock];
    write_frame(&mut pwr, &PluginMsg::Register { manifest })
        .await
        .expect("send Register");

    // Seed frame is the clock snapshot (the only subscription) — not visibility.
    assert!(
        matches!(recv(&mut prd).await, HostMsg::StateSnapshot { .. }),
        "register seed is a clock snapshot, never SlotVisibility",
    );

    // Drive a visibility edge (must be filtered) then a clock edge (passes).
    vis_tx.send_replace(true);
    clock_tx.send_replace(Some(ClockState {
        iso: "t1".into(),
        unix: 1,
    }));

    match recv(&mut prd).await {
        HostMsg::StateSnapshot { snapshot } => assert_eq!(
            snapshot.clock.map(|c| c.unix),
            Some(1),
            "the clock edge came through; the visibility edge produced no frame",
        ),
        HostMsg::SlotVisibility { .. } => {
            panic!("unsubscribed plugin received a SlotVisibility frame (#305 regression)")
        }
        other => panic!("unexpected frame: {other:?}"),
    }
}

/// #305: a connection that **does** subscribe `SlotVisible` gets the
/// register-time seed and every subsequent edge — the departures poller's
/// visibility gate keeps working. Subscribes `SlotVisible` only, so the only
/// proactive frames are the visibility pushes (deterministic ordering).
#[tokio::test]
async fn visibility_push_delivered_when_subscribed() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);

    let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host_end, &ctx).await });

    let (mut prd, mut pwr) = plugin_end.into_split();
    let mut manifest = Manifest::new("board", Mount::SidebarBottom);
    manifest.subscribes = vec![StateKey::SlotVisible];
    write_frame(&mut pwr, &PluginMsg::Register { manifest })
        .await
        .expect("send Register");

    // Register seed: the current aggregate (false, nothing open at boot).
    assert!(
        matches!(
            recv(&mut prd).await,
            HostMsg::SlotVisibility { visible: false }
        ),
        "register seed carries the current visibility",
    );

    // An open edge is forwarded.
    vis_tx.send_replace(true);
    assert!(
        matches!(
            recv(&mut prd).await,
            HostMsg::SlotVisibility { visible: true }
        ),
        "the open edge reaches the subscriber",
    );
}

/// #438: a **bar**-mounted plugin that subscribes `SlotVisible` is on-screen
/// whenever its chip is (a bar chip has no sidebar-style hide), so the host
/// seeds a constant `visible: true` and never feeds it the sidebar-open
/// aggregate — otherwise a bar plugin parking pollers on `SlotVisible` (#288)
/// would idle while fully visible. The sidebar aggregate starts `false` (a
/// sidebar mount would be seeded `false` here), and sidebar edges must not
/// reach the bar mount at all.
#[tokio::test]
async fn visibility_is_constant_true_for_bar_mounts() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);

    let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host_end, &ctx).await });

    let (mut prd, mut pwr) = plugin_end.into_split();
    let mut manifest = Manifest::new("barchip", Mount::BarCenter);
    manifest.subscribes = vec![StateKey::SlotVisible];
    write_frame(&mut pwr, &PluginMsg::Register { manifest })
        .await
        .expect("send Register");

    // The register seed is a constant `true` for a bar chip, despite the
    // sidebar aggregate being `false`.
    assert!(
        matches!(
            recv(&mut prd).await,
            HostMsg::SlotVisibility { visible: true }
        ),
        "a bar mount is seeded visible=true regardless of sidebar state",
    );

    // Sidebar edges must not reach a bar mount — its chip visibility is
    // independent of every sidebar. Drive a couple; the constant task already
    // sent its one seed and returned, so nothing more may arrive.
    vis_tx.send_replace(true);
    vis_tx.send_replace(false);
    let quiet = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        read_frame::<HostMsg, _>(&mut prd),
    )
    .await;
    assert!(
        quiet.is_err(),
        "a bar mount receives no sidebar-driven visibility edges (only the seed)",
    );
}

// ── Containment (#435) ────────────────────────────────────────────────────

/// The effect rate cap is a token bucket: a plugin may fire a full
/// [`EFFECT_BURST`] back-to-back, then is limited to the sustained refill;
/// a long idle refills back up to (but never beyond) the burst cap. Driven
/// with synthetic instants so it's deterministic.
#[test]
fn effect_rate_limiter_caps_sustained_but_allows_burst() {
    let t0 = Instant::now();
    let mut rl = EffectRateLimiter::new_at(t0);

    // The whole burst is available up front, then the bucket is empty.
    for _ in 0..EFFECT_BURST {
        assert!(rl.allow(t0), "burst tokens available immediately");
    }
    assert!(!rl.allow(t0), "burst exhausted at the same instant");

    // One refill interval later, exactly one more effect is allowed.
    let t1 = t0 + Duration::from_secs(1);
    assert!(rl.allow(t1), "one token refilled after 1s");
    assert!(!rl.allow(t1), "only one token per refill interval");

    // A long idle refills to the burst cap — and saturates there, so idle
    // time can't bank unbounded budget for a later flood.
    let t2 = t1 + Duration::from_secs(100);
    for _ in 0..EFFECT_BURST {
        assert!(
            rl.allow(t2),
            "bucket refills up to the burst cap after idle"
        );
    }
    assert!(!rl.allow(t2), "refill saturates at the burst cap");
}

/// #435: a peer that dials the socket but never sends `Register` must be
/// dropped after `REGISTER_TIMEOUT`, not park the connection task forever.
/// Paused-time so the 10s wall-clock timeout resolves instantly; the plugin
/// end is held open (never written) so the handshake read stays pending and
/// the *timeout* — not an EOF — is what ends the connection.
#[tokio::test(start_paused = true)]
async fn handshake_timeout_drops_a_silent_connection() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);

    let (host_end, _plugin_end) = UnixStream::pair().expect("socketpair");
    let conn = tokio::spawn(async move { handle_conn(host_end, &ctx).await });

    // Let the task arm its handshake timeout, then jump past it.
    tokio::task::yield_now().await;
    tokio::time::advance(REGISTER_TIMEOUT + Duration::from_secs(1)).await;

    tokio::time::timeout(Duration::from_secs(5), conn)
        .await
        .expect("handle_conn returns after the handshake timeout")
        .expect("conn task joined cleanly");
}

// ── Registration hygiene (#436) ───────────────────────────────────────────

/// #436 item 1: `socket_in_use` reports whether a live listener already owns
/// the path — absent socket → false (safe to bind); live listener → true
/// (another instance owns it, stand down); a stale socket file left after the
/// listener drops → false (reclaimable). Hermetic: a real `UnixListener` in a
/// scratch dir, no system daemons.
#[tokio::test]
async fn socket_in_use_detects_a_live_listener() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("probe.sock");

    assert!(
        !socket_in_use(&path).await,
        "an absent socket is not in use (safe to bind)",
    );

    let listener = tokio::net::UnixListener::bind(&path).expect("bind probe socket");
    assert!(
        socket_in_use(&path).await,
        "a live listener is detected as in use (stand down)",
    );

    // Dropping the listener leaves the socket file on disk but nothing
    // answers it — a stale socket, which is reclaimable.
    drop(listener);
    assert!(
        !socket_in_use(&path).await,
        "a stale socket (no listener) is not in use, so it can be reclaimed",
    );
}

/// #436 item 2: an [`IdGuard`] claim is exclusive per id within one host's
/// set — a second claim of a held id is rejected — and releasing the guard
/// (connection teardown) makes the id reclaimable. Distinct ids never
/// contend. Hermetic: a local set, no process-global state.
#[test]
fn duplicate_id_claim_rejected_until_released() {
    let ids = Arc::new(Mutex::new(HashSet::new()));

    let guard = IdGuard::claim(&ids, "pet").expect("first claim of an id succeeds");
    assert!(
        IdGuard::claim(&ids, "pet").is_none(),
        "a second claim of a live id is rejected",
    );
    assert!(
        IdGuard::claim(&ids, "clock").is_some(),
        "a distinct id claims independently",
    );

    drop(guard);
    assert!(
        IdGuard::claim(&ids, "pet").is_some(),
        "the id is reclaimable once the owning guard is dropped",
    );
}

/// #436 item 3: every [`Effect`] maps to exactly the [`Capability`] its wire
/// docs name, so enforcement gates each effect on the cap a plugin must
/// declare to use it. Exhaustive over the effect vocabulary.
#[test]
fn effect_capability_maps_each_effect() {
    assert_eq!(
        effect_capability(&Effect::OpenPage(Page::Media)),
        Capability::OpenPage,
    );
    assert_eq!(
        effect_capability(&Effect::Niri(NiriAction::FocusWorkspace { id: 1 })),
        Capability::Niri,
    );
    assert_eq!(
        effect_capability(&Effect::Media(MediaAction::PlayPause)),
        Capability::Media,
    );
    assert_eq!(
        effect_capability(&Effect::Audio(AudioAction::ToggleMute)),
        Capability::Audio,
    );
    assert_eq!(
        effect_capability(&Effect::RunCommand {
            id: 0,
            argv: vec![],
        }),
        Capability::RunCommand,
    );
    assert_eq!(
        effect_capability(&Effect::RaiseOsd {
            title: String::new(),
            body: String::new(),
            icon: None,
        }),
        Capability::RaiseOsd,
    );
    assert_eq!(
        effect_capability(&Effect::Notify {
            summary: String::new(),
            body: String::new(),
        }),
        Capability::Notify,
    );
    assert_eq!(
        effect_capability(&Effect::RequestConsent {
            request_id: 1,
            agent: String::new(),
            datasource: String::new(),
            scope: String::new(),
            detail: String::new(),
        }),
        Capability::Consent,
    );
}

/// #436 item 3: `enforce_capabilities` keeps only effects whose capability
/// the plugin declared, dropping the rest (source order preserved). The
/// manifest's grant set is authoritative — a plugin can *request* any effect,
/// but an ungranted one never reaches the broker.
#[test]
fn enforce_capabilities_drops_ungranted_effects() {
    let granted = vec![Capability::OpenPage, Capability::Notify];
    let effects = vec![
        Effect::OpenPage(Page::Media), // granted
        Effect::RaiseOsd {
            title: "t".into(),
            body: "b".into(),
            icon: None,
        }, // NOT granted
        Effect::Notify {
            summary: "s".into(),
            body: "b".into(),
        }, // granted
        Effect::Niri(NiriAction::FocusWindow { id: 7 }), // NOT granted
    ];

    let kept = enforce_capabilities(&granted, "p", effects);
    assert_eq!(kept.len(), 2, "only the two granted effects survive");
    assert!(matches!(kept[0], Effect::OpenPage(Page::Media)));
    assert!(matches!(kept[1], Effect::Notify { .. }));

    // A plugin that declared no caps has every effect dropped.
    assert!(
        enforce_capabilities(&[], "p", vec![Effect::OpenPage(Page::Power)]).is_empty(),
        "a plugin that declared no caps gets every effect dropped",
    );
}

// ── #484/#528 domain pushes: gating + projections ────────────────────────────

/// The state-key → capability map is exhaustive and gates exactly the domain
/// keys; the ambient keys (subscription is their whole opt-in) map to `None`.
#[test]
fn state_key_capability_gates_only_the_domain_keys() {
    assert_eq!(state_key_capability(StateKey::Clock), None);
    assert_eq!(state_key_capability(StateKey::SlotVisible), None);
    assert_eq!(state_key_capability(StateKey::Accent), None);
    assert_eq!(state_key_capability(StateKey::AudioSpectrum), None);
    assert_eq!(
        state_key_capability(StateKey::CalendarUpcoming),
        Some(Capability::Calendar)
    );
    assert_eq!(
        state_key_capability(StateKey::SessionLocked),
        Some(Capability::SessionState)
    );
    assert_eq!(
        state_key_capability(StateKey::NowPlaying),
        Some(Capability::NowPlaying)
    );
}

/// `push_gate`: an ambient key needs only the subscription; a domain key needs
/// the subscription **and** its gating capability. Missing either → refused.
#[test]
fn push_gate_requires_subscription_and_capability_for_domain_keys() {
    let mut m = Manifest::new("p", Mount::SidebarTop);
    // Ambient: subscription alone suffices; an unsubscribed key is refused.
    m.subscribes = vec![StateKey::Clock];
    assert!(push_gate(&m, StateKey::Clock));
    assert!(!push_gate(&m, StateKey::CalendarUpcoming), "not subscribed");

    // Domain: subscribed but missing the cap → refused (declared *and* enforced).
    m.subscribes = vec![StateKey::CalendarUpcoming];
    assert!(
        !push_gate(&m, StateKey::CalendarUpcoming),
        "a subscribe-only domain key is refused"
    );
    // Subscribed + cap → allowed.
    m.capabilities = vec![Capability::Calendar];
    assert!(push_gate(&m, StateKey::CalendarUpcoming));

    // The capability without the subscription is not enough either.
    let mut m2 = Manifest::new("p", Mount::SidebarTop);
    m2.capabilities = vec![Capability::SessionState];
    assert!(
        !push_gate(&m2, StateKey::SessionLocked),
        "cap without subscription"
    );
    m2.subscribes = vec![StateKey::SessionLocked];
    assert!(push_gate(&m2, StateKey::SessionLocked));
}

/// `to_upcoming_events` keeps events overlapping the next 24 h (not already
/// ended, starting inside the window), caps at `MAX_UPCOMING_EVENTS`, and maps
/// each field.
#[test]
fn to_upcoming_events_windows_caps_and_maps() {
    use chrono::{DateTime, Local};
    use hytte::services::calendar::CalendarEvent;

    let now = 1_700_000_000_i64;
    let day = 24 * 3600;
    let ev = |start: i64, end: i64, summary: &str, cal: &str| CalendarEvent {
        uid: String::new(),
        summary: summary.to_owned(),
        start: DateTime::from_timestamp(start, 0)
            .expect("ts")
            .with_timezone(&Local),
        end: DateTime::from_timestamp(end, 0)
            .expect("ts")
            .with_timezone(&Local),
        location: None,
        all_day: false,
        calendar_name: cal.to_owned(),
    };
    // Sorted ascending by start, as the calendar service guarantees.
    let events = vec![
        ev(now - 200, now - 100, "past", "A"), // already ended → out
        ev(now - 60, now + 60, "ongoing", "Work"), // ends in future → in
        ev(now + 100, now + 160, "e2", "A"),
        ev(now + 200, now + 260, "e3", "A"),
        ev(now + 300, now + 360, "e4", "A"),
        ev(now + 400, now + 460, "e5", "A"),
        ev(now + 500, now + 560, "e6", "A"), // 6th survivor → capped
        ev(now + day + 100, now + day + 200, "later", "A"), // starts past 24 h → out
    ];
    let out = to_upcoming_events(&events, now);
    assert_eq!(out.len(), 5, "capped at MAX_UPCOMING_EVENTS");
    let titles: Vec<&str> = out.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(titles, ["ongoing", "e2", "e3", "e4", "e5"]);
    assert!(!titles.contains(&"past") && !titles.contains(&"later"));
    // Field mapping on the first survivor.
    assert_eq!(out[0].start_unix, now - 60);
    assert_eq!(out[0].end_unix, now + 60);
    assert_eq!(out[0].calendar, "Work");
}

/// `to_now_playing` projects the active player (title/artist/playing, plus the
/// #840 timing) and maps `None` to the empty, not-playing default.
#[test]
fn to_now_playing_projects_the_active_player() {
    use hytte::services::mpris::{PlaybackStatus, Player};

    assert_eq!(to_now_playing(None), NowPlaying::default());
    let playing = Player {
        title: "Chrome Rain".to_owned(),
        artists: "Choom".to_owned(),
        status: PlaybackStatus::Playing,
        position_us: 83_000_000,
        length_us: 296_000_000,
        ..Player::default()
    };
    let np = to_now_playing(Some(&playing));
    assert_eq!(np.title, "Chrome Rain");
    assert_eq!(np.artist, "Choom");
    assert!(np.playing);
    // The timing rides across verbatim — both already microseconds (#840).
    assert_eq!(np.position_us, 83_000_000);
    assert_eq!(np.length_us, 296_000_000);
    // Paused / stopped read as not playing.
    let paused = Player {
        status: PlaybackStatus::Paused,
        ..playing.clone()
    };
    assert!(!to_now_playing(Some(&paused)).playing);
    // A player reporting no `mpris:length` projects `0` — the digest's own
    // "unknown", not a zero-length track (live streams, most web players).
    let untimed = Player {
        length_us: 0,
        position_us: 0,
        ..playing.clone()
    };
    let np = to_now_playing(Some(&untimed));
    assert_eq!(np.length_us, 0, "an untimed player stays unknown");
    assert_eq!(np.position_us, 0);
}

/// #436 item 2, end to end through `handle_conn`: a second connection that
/// Registers an id already held by a live connection is rejected (dropped),
/// and the incumbent's region card is left untouched — no flapping. Both
/// connections share the same [`ListenerCtx`] (its region mailboxes **and**
/// its live-id set), the real production shape.
#[tokio::test]
async fn duplicate_id_connection_is_rejected_end_to_end() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);
    let ctx_b = ctx.clone(); // shares the region mailboxes AND the live-id set
    let bar_center = ctx.bar_center.clone();

    // Connection A: registers "twin" and renders a card, claiming the id.
    let (a_host, a_plugin) = UnixStream::pair().expect("socketpair A");
    tokio::spawn(async move { handle_conn(a_host, &ctx).await });
    let (_ard, mut awr) = a_plugin.into_split();
    write_frame(
        &mut awr,
        &PluginMsg::Register {
            manifest: Manifest::new("twin", Mount::BarCenter),
        },
    )
    .await
    .expect("A Register");
    write_frame(
        &mut awr,
        &PluginMsg::Render {
            tree: wire::Node::Label {
                id: Some("t".into()),
                text: "A".into(),
                classes: vec![],
            },
            panel: None,
            effects: vec![],
        },
    )
    .await
    .expect("A Render");
    // A's card landing proves A is connected and holds the "twin" id.
    wait_for_region(&bar_center).await;

    // Connection B: same id. Must be rejected — the host drops it.
    let (b_host, b_plugin) = UnixStream::pair().expect("socketpair B");
    tokio::spawn(async move { handle_conn(b_host, &ctx_b).await });
    let (mut brd, mut bwr) = b_plugin.into_split();
    write_frame(
        &mut bwr,
        &PluginMsg::Register {
            manifest: Manifest::new("twin", Mount::BarCenter),
        },
    )
    .await
    .expect("B Register");

    // The host rejects B by dropping the connection: B's next read hits EOF.
    let dropped = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_frame::<HostMsg, _>(&mut brd),
    )
    .await
    .expect("B is dropped within 5s (not left hanging)");
    assert!(
        dropped.is_err(),
        "the duplicate-id connection is dropped (EOF), not accepted",
    );

    // A's card is intact — exactly one "twin" card, no flapping.
    let cards = bar_center.lock_ref();
    assert_eq!(cards.len(), 1, "the incumbent's card is untouched");
    assert_eq!(cards[0].plugin_id, "twin");
    assert!(
        matches!(&cards[0].tree, wire::Node::Label { text, .. } if text == "A"),
        "the incumbent (A) still owns the card; B never overwrote it",
    );
}

/// #436 item 2 (empty id): an **empty** plugin id is rejected outright — it
/// can't key a region card. The connection is dropped and no card is parked.
#[tokio::test]
async fn empty_plugin_id_is_rejected() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);
    let bar_center = ctx.bar_center.clone();

    let (host, plugin) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host, &ctx).await });
    let (mut prd, mut pwr) = plugin.into_split();
    write_frame(
        &mut pwr,
        &PluginMsg::Register {
            manifest: Manifest::new("", Mount::BarCenter),
        },
    )
    .await
    .expect("Register with empty id");

    // Dropped: the next read hits EOF.
    let dropped = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_frame::<HostMsg, _>(&mut prd),
    )
    .await
    .expect("the empty-id connection is dropped within 5s");
    assert!(dropped.is_err(), "an empty-id Register is rejected (EOF)");
    assert!(
        bar_center.lock_ref().is_empty(),
        "no card is parked for an empty id",
    );
}

/// #437, end to end through `handle_conn`: a Register whose `vocab` is **newer**
/// than this host's [`VOCAB`] is rejected at the handshake (the connection is
/// dropped, no card parked) — the plugin→host skew that used to be a silent 5 s
/// redial crash-loop now fails loud. A Register at the host's own vocab is
/// accepted (its card lands). Mirrors the `empty_plugin_id_is_rejected` /
/// duplicate-id reject harness.
#[tokio::test]
async fn newer_vocab_register_is_rejected_and_equal_vocab_is_accepted() {
    // ── Reject: a plugin built against a newer wire vocabulary. ──
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);
    let bar_center = ctx.bar_center.clone();

    let (host, plugin) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host, &ctx).await });
    let (mut prd, mut pwr) = plugin.into_split();
    let mut too_new = Manifest::new("from-the-future", Mount::BarCenter);
    too_new.vocab = VOCAB + 1; // one wire generation ahead of this host
    write_frame(&mut pwr, &PluginMsg::Register { manifest: too_new })
        .await
        .expect("send too-new Register");

    // Dropped at the handshake: the next read hits EOF.
    let dropped = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_frame::<HostMsg, _>(&mut prd),
    )
    .await
    .expect("the too-new-vocab connection is dropped within 5s");
    assert!(
        dropped.is_err(),
        "a Register with vocab > host's is rejected (EOF)",
    );
    assert!(
        bar_center.lock_ref().is_empty(),
        "no card is parked for a rejected too-new plugin",
    );

    // ── Accept: a plugin at the host's own vocabulary renders normally. ──
    let (_clock_tx2, clock_rx2) = watch::channel(None);
    let (_vis_tx2, vis_rx2) = watch::channel(false);
    let (ctx2, _effects_rx2) = ctx_with(clock_rx2, vis_rx2);
    let bar_center2 = ctx2.bar_center.clone();

    let (host2, plugin2) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host2, &ctx2).await });
    let (_prd2, mut pwr2) = plugin2.into_split();
    let mut ok = Manifest::new("current", Mount::BarCenter);
    ok.vocab = VOCAB; // equal to the host — accepted
    write_frame(&mut pwr2, &PluginMsg::Register { manifest: ok })
        .await
        .expect("send equal-vocab Register");
    write_frame(
        &mut pwr2,
        &PluginMsg::Render {
            tree: wire::Node::Label {
                id: Some("t".into()),
                text: "chip".into(),
                classes: vec![],
            },
            panel: None,
            effects: vec![],
        },
    )
    .await
    .expect("send Render");

    let cards = wait_for_region(&bar_center2).await;
    assert_eq!(
        cards.len(),
        1,
        "an equal-vocab plugin is accepted and renders"
    );
    assert_eq!(cards[0].plugin_id, "current");
}

/// #436 item 3, end to end: an effect whose capability the plugin didn't
/// declare is dropped in the reader and never reaches the effect broker
/// channel, while the render tree still lands. `route_render` sends effects
/// **before** parking the card, so the card's arrival means the effect
/// decision already happened — an empty channel then proves the effect was
/// dropped upstream, not merely delayed.
#[tokio::test]
async fn ungranted_effect_never_reaches_the_broker() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, mut effects_rx) = ctx_with(clock_rx, vis_rx);
    let bar_center = ctx.bar_center.clone();

    let (host, plugin) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host, &ctx).await });
    let (_prd, mut pwr) = plugin.into_split();
    // A plugin that declares NO capabilities.
    write_frame(
        &mut pwr,
        &PluginMsg::Register {
            manifest: Manifest::new("nocaps", Mount::BarCenter),
        },
    )
    .await
    .expect("Register (no caps)");
    // …nonetheless emits an OpenPage effect (ungranted).
    write_frame(
        &mut pwr,
        &PluginMsg::Render {
            tree: wire::Node::Label {
                id: Some("t".into()),
                text: "hi".into(),
                classes: vec![],
            },
            panel: None,
            effects: vec![Effect::OpenPage(Page::PowerMenu)],
        },
    )
    .await
    .expect("Render with an ungranted effect");

    wait_for_region(&bar_center).await;
    assert!(
        effects_rx.try_recv().is_err(),
        "an ungranted effect is dropped before the broker (#436)",
    );
}

/// #436 item 3, positive case end to end: a plugin that declared the cap sees
/// its effect brokered (reaches the effect channel), proving the enforcement
/// gate passes granted effects through unchanged.
#[tokio::test]
async fn granted_effect_reaches_the_broker() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, mut effects_rx) = ctx_with(clock_rx, vis_rx);
    let bar_center = ctx.bar_center.clone();

    let (host, plugin) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host, &ctx).await });
    let (_prd, mut pwr) = plugin.into_split();
    let mut manifest = Manifest::new("withcap", Mount::BarCenter);
    manifest.capabilities = vec![Capability::OpenPage];
    write_frame(&mut pwr, &PluginMsg::Register { manifest })
        .await
        .expect("Register (OpenPage granted)");
    write_frame(
        &mut pwr,
        &PluginMsg::Render {
            tree: wire::Node::Label {
                id: Some("t".into()),
                text: "hi".into(),
                classes: vec![],
            },
            panel: None,
            effects: vec![Effect::OpenPage(Page::PowerMenu)],
        },
    )
    .await
    .expect("Render with a granted effect");

    wait_for_region(&bar_center).await;
    let got = effects_rx
        .try_recv()
        .expect("the granted effect reached the broker");
    assert_eq!(got.plugin_id, "withcap");
    assert!(matches!(got.effect, Effect::OpenPage(Page::PowerMenu)));
}

// ── Datasource query routing (#509) ──────────────────────────────────────────

/// Read one host→plugin frame off an outbound mpsc queue (the requester/provider
/// side of a routed query), failing rather than hanging.
async fn recv_queue(rx: &mut mpsc::Receiver<HostMsg>) -> HostMsg {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a routed frame within 5s")
        .expect("the queue is open")
}

/// The requester/provider capability each datasource effect requires — the
/// exhaustive `effect_capability` mapping for the two #509 variants.
#[test]
fn effect_capability_maps_datasource_effects() {
    assert_eq!(
        effect_capability(&Effect::DatasourceQuery {
            request_id: 0,
            provider: String::new(),
            scope: String::new(),
            params: String::new(),
        }),
        Capability::DatasourceQuery,
    );
    assert_eq!(
        effect_capability(&Effect::DatasourceResult {
            request_id: 0,
            outcome: DatasourceOutcome::Ready(String::new()),
        }),
        Capability::DatasourceProvider,
    );
}

/// #509 capability enforcement: a `DatasourceQuery` needs `DatasourceQuery`, a
/// `DatasourceResult` needs `DatasourceProvider` — each is dropped without its own
/// cap, and one cap never smuggles the other effect through.
#[test]
fn enforce_capabilities_gates_datasource_effects() {
    let query = Effect::DatasourceQuery {
        request_id: 1,
        provider: "departures".into(),
        scope: "next".into(),
        params: "{}".into(),
    };
    let result = Effect::DatasourceResult {
        request_id: 1,
        outcome: DatasourceOutcome::Ready("x".into()),
    };
    // No caps → both dropped.
    assert!(
        enforce_capabilities(&[], "p", vec![query.clone(), result.clone()]).is_empty(),
        "ungranted datasource effects are dropped",
    );
    // The requester cap keeps only the query.
    let kept = enforce_capabilities(
        &[Capability::DatasourceQuery],
        "p",
        vec![query.clone(), result.clone()],
    );
    assert_eq!(kept.len(), 1);
    assert!(matches!(kept[0], Effect::DatasourceQuery { .. }));
    // The provider cap keeps only the result.
    let kept = enforce_capabilities(&[Capability::DatasourceProvider], "p", vec![query, result]);
    assert_eq!(kept.len(), 1);
    assert!(matches!(kept[0], Effect::DatasourceResult { .. }));
}

/// The end-to-end broker round-trip: a requester's `DatasourceQuery` is routed to
/// the registered provider under an **opaque host correlation** (not the
/// requester's token), and the provider's `DatasourceResult` comes back to the
/// requester keyed by its **own** `request_id`. Exercises `broker_effect` on both
/// legs, so it covers the broker dispatch + the router's correlation translation.
#[tokio::test]
async fn datasource_query_routes_to_provider_and_result_back() {
    let router = DatasourceRouter::default();
    let (prov_tx, mut prov_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    let (req_tx, mut req_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    router.register_provider(
        "departures",
        "departures",
        vec!["next".into()],
        prov_tx.clone(),
        1,
    );

    broker_effect(
        "infobroker",
        &Effect::DatasourceQuery {
            request_id: 42,
            provider: "departures".into(),
            scope: "next".into(),
            params: r#"{"limit":3}"#.into(),
        },
        &req_tx,
        &router,
    );

    // The provider sees the query under a host correlation, never the requester's 42.
    let HostMsg::DatasourceQuery {
        request_id: corr,
        datasource,
        scope,
        params,
    } = recv_queue(&mut prov_rx).await
    else {
        panic!("provider must receive a DatasourceQuery");
    };
    assert_eq!(datasource, "departures");
    assert_eq!(scope, "next");
    assert_eq!(params, r#"{"limit":3}"#);
    assert_ne!(
        corr, 42,
        "the provider sees a host correlation, not the requester token"
    );

    // The provider answers under that correlation; the requester gets it back keyed
    // by its own request_id.
    broker_effect(
        "departures",
        &Effect::DatasourceResult {
            request_id: corr,
            outcome: DatasourceOutcome::Ready("rows".into()),
        },
        &prov_tx,
        &router,
    );
    assert_eq!(
        recv_queue(&mut req_rx).await,
        HostMsg::DatasourceResult {
            request_id: 42,
            outcome: DatasourceOutcome::Ready("rows".into()),
        },
    );
}

/// #553: only the provider a query was **routed to** may resolve its correlation.
/// A second provider-capable plugin that echoes another plugin's in-flight host
/// correlation with a forged answer must NOT resolve the parked query; the genuine
/// provider's later answer still must. Guards the "host is the single policy
/// chokepoint" story against cross-provider result forgery.
#[tokio::test]
async fn datasource_result_from_a_non_routed_provider_is_dropped() {
    let router = DatasourceRouter::default();
    let (prov_tx, mut prov_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    let (req_tx, mut req_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    // The genuine provider of `departures`.
    router.register_provider(
        "departures",
        "departures",
        vec!["next".into()],
        prov_tx.clone(),
        1,
    );

    // Route a query; capture the opaque host correlation the provider is asked under.
    broker_effect(
        "infobroker",
        &Effect::DatasourceQuery {
            request_id: 42,
            provider: "departures".into(),
            scope: "next".into(),
            params: "{}".into(),
        },
        &req_tx,
        &router,
    );
    let HostMsg::DatasourceQuery {
        request_id: corr, ..
    } = recv_queue(&mut prov_rx).await
    else {
        panic!("provider must receive a DatasourceQuery");
    };

    // A DIFFERENT provider-capable plugin echoes that correlation with a forged
    // answer. It is not the routed-to provider, so the parked query must NOT
    // resolve. (`outbound` is unused by `DatasourceResult`; any sender is fine.)
    broker_effect(
        "impostor",
        &Effect::DatasourceResult {
            request_id: corr,
            outcome: DatasourceOutcome::Ready("forged".into()),
        },
        &prov_tx,
        &router,
    );

    // The genuine provider's answer resolves it — and because the forgery left the
    // entry parked (not removed), this still finds it. In a correct build the
    // requester deterministically receives "real": the impostor is the only other
    // writer and it never removes the correlation.
    broker_effect(
        "departures",
        &Effect::DatasourceResult {
            request_id: corr,
            outcome: DatasourceOutcome::Ready("real".into()),
        },
        &prov_tx,
        &router,
    );
    assert_eq!(
        recv_queue(&mut req_rx).await,
        HostMsg::DatasourceResult {
            request_id: 42,
            outcome: DatasourceOutcome::Ready("real".into()),
        },
        "the genuine provider's answer must resolve the query, not the forgery",
    );
    // And the forged answer must never also reach the requester (a bounded wait so a
    // leaked forgery would arrive within the window rather than escape detection).
    assert!(
        tokio::time::timeout(Duration::from_millis(200), req_rx.recv())
            .await
            .is_err(),
        "a forged result from a non-routed provider must never reach the requester",
    );
}

/// A query for a datasource no connected plugin provides resolves to a
/// host-synthesized `NotFound` — the requester never hangs.
#[tokio::test]
async fn datasource_query_for_unknown_provider_is_not_found() {
    let router = DatasourceRouter::default();
    let (req_tx, mut req_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    router.route_query(
        "infobroker".into(),
        7,
        "nope".into(),
        "x".into(),
        "{}".into(),
        req_tx,
    );
    let HostMsg::DatasourceResult {
        request_id,
        outcome,
    } = recv_queue(&mut req_rx).await
    else {
        panic!("a result must come back");
    };
    assert_eq!(request_id, 7);
    assert!(matches!(
        outcome,
        DatasourceOutcome::Failed {
            error: DatasourceError::NotFound,
            ..
        }
    ));
}

/// A query naming a scope the provider never declared resolves to `ScopeDenied` —
/// the host enforces the provider's declared `scopes`, not the provider itself.
#[tokio::test]
async fn datasource_query_for_undeclared_scope_is_scope_denied() {
    let router = DatasourceRouter::default();
    let (prov_tx, _prov_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    let (req_tx, mut req_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    router.register_provider("departures", "departures", vec!["next".into()], prov_tx, 1);
    router.route_query(
        "infobroker".into(),
        8,
        "departures".into(),
        "history".into(), // not a declared scope
        "{}".into(),
        req_tx,
    );
    let HostMsg::DatasourceResult {
        request_id,
        outcome,
    } = recv_queue(&mut req_rx).await
    else {
        panic!("a result must come back");
    };
    assert_eq!(request_id, 8);
    assert!(matches!(
        outcome,
        DatasourceOutcome::Failed {
            error: DatasourceError::ScopeDenied,
            ..
        }
    ));
}

/// A provider that accepts a forwarded query but never answers is reaped by the
/// host timeout, which synthesizes `Timeout` to the requester (`QUERY_TIMEOUT` is
/// shortened under test).
#[tokio::test]
async fn datasource_query_times_out_when_provider_never_answers() {
    let router = DatasourceRouter::default();
    // A wide provider queue so the forward succeeds; the test simply never answers.
    let (prov_tx, _prov_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    let (req_tx, mut req_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    router.register_provider("weather", "weather", vec!["current".into()], prov_tx, 1);
    router.route_query(
        "infobroker".into(),
        99,
        "weather".into(),
        "current".into(),
        "{}".into(),
        req_tx,
    );
    let HostMsg::DatasourceResult {
        request_id,
        outcome,
    } = recv_queue(&mut req_rx).await
    else {
        panic!("the timeout must synthesize a result");
    };
    assert_eq!(request_id, 99);
    assert!(matches!(
        outcome,
        DatasourceOutcome::Failed {
            error: DatasourceError::Timeout,
            ..
        }
    ));
}

/// End to end through `handle_conn`: a plugin whose manifest declares `provides` +
/// `Capability::DatasourceProvider` becomes routable — a query for its datasource
/// is forwarded to its connection.
#[tokio::test]
async fn provider_manifest_registers_a_routable_datasource() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);
    let bottom = ctx.sidebar_bottom.clone();
    let router = ctx.datasource.clone();

    let (host, plugin) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host, &ctx).await });
    let (mut prd, mut pwr) = plugin.into_split();
    let mut manifest = Manifest::new("departures", Mount::SidebarBottom);
    manifest.capabilities = vec![Capability::DatasourceProvider];
    manifest.provides = vec![ProvidedDatasource::new("departures", vec!["next".into()])];
    write_frame(&mut pwr, &PluginMsg::Register { manifest })
        .await
        .expect("Register (provider)");
    // A render frame lets us wait for registration to complete: provider
    // registration runs in the handshake, before the reader loop parks any card.
    write_frame(
        &mut pwr,
        &PluginMsg::Render {
            tree: wire::Node::Label {
                id: Some("t".into()),
                text: "board".into(),
                classes: vec![],
            },
            panel: None,
            effects: vec![],
        },
    )
    .await
    .expect("Render");
    wait_for_region(&bottom).await;

    // Route a query; the provider connection receives the forwarded frame.
    let (req_tx, _req_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    router.route_query(
        "infobroker".into(),
        3,
        "departures".into(),
        "next".into(),
        r#"{"limit":2}"#.into(),
        req_tx,
    );
    let HostMsg::DatasourceQuery {
        datasource, scope, ..
    } = recv(&mut prd).await
    else {
        panic!("the provider connection must receive the forwarded query");
    };
    assert_eq!(datasource, "departures");
    assert_eq!(scope, "next");
}

/// The provider gate: a plugin that lists `provides` but omits
/// `Capability::DatasourceProvider` is **not** registered, so a query for its
/// datasource resolves to `NotFound`.
#[tokio::test]
async fn provides_without_capability_is_not_registered() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);
    let bottom = ctx.sidebar_bottom.clone();
    let router = ctx.datasource.clone();

    let (host, plugin) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host, &ctx).await });
    let (_prd, mut pwr) = plugin.into_split();
    let mut manifest = Manifest::new("departures", Mount::SidebarBottom);
    // Lists a datasource but omits the gating capability.
    manifest.provides = vec![ProvidedDatasource::new("departures", vec!["next".into()])];
    write_frame(&mut pwr, &PluginMsg::Register { manifest })
        .await
        .expect("Register (no provider cap)");
    write_frame(
        &mut pwr,
        &PluginMsg::Render {
            tree: wire::Node::Label {
                id: Some("t".into()),
                text: "board".into(),
                classes: vec![],
            },
            panel: None,
            effects: vec![],
        },
    )
    .await
    .expect("Render");
    wait_for_region(&bottom).await;

    // The datasource was never registered → the query fails NotFound.
    let (req_tx, mut req_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    router.route_query(
        "infobroker".into(),
        4,
        "departures".into(),
        "next".into(),
        "{}".into(),
        req_tx,
    );
    let HostMsg::DatasourceResult { outcome, .. } = recv_queue(&mut req_rx).await else {
        panic!("a result must come back");
    };
    assert!(matches!(
        outcome,
        DatasourceOutcome::Failed {
            error: DatasourceError::NotFound,
            ..
        }
    ));
}

// ── Session-lane wave: spectrum demand-gate (#559) + now-playing re-seed (#542) ─

/// #559: the per-connection spectrum-tap demand state machine
/// ([`super::session::SpectrumGate`]). A connection contributes at most one unit
/// to the (here local) refcount; the count's 0↔1 crossing is the
/// `set_spectrum_active` edge. Pins the invariants the visibility gating relies
/// on: no double count, no missed re-arm, idempotent transitions.
#[test]
fn spectrum_gate_counts_and_edges_are_exact() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let count = AtomicUsize::new(0);
    let mut g = super::session::SpectrumGate::new();

    // First demand (subscribed + visible): 0→1, activate.
    assert_eq!(g.apply(true, &count), Some(true), "0→1 activates the tap");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    // A repeated demand is idempotent — no double count, no spurious edge.
    assert_eq!(g.apply(true, &count), None, "a repeated demand is a no-op");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    // Goes off-screen: 1→0, deactivate.
    assert_eq!(
        g.apply(false, &count),
        Some(false),
        "1→0 deactivates the tap"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    // A repeated release is idempotent — no double decrement.
    assert_eq!(
        g.apply(false, &count),
        None,
        "a repeated release is a no-op"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    // Back on-screen: 0→1, re-arm the tap (no missed re-arm).
    assert_eq!(
        g.apply(true, &count),
        Some(true),
        "invisible→visible re-arms"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

/// #559: the invisible-then-disconnect path releases EXACTLY once. Going
/// invisible drops the unit; the teardown release (the `SpectrumDemand` guard's
/// `Drop`, modeled here as a final `apply(false)`) must then be a no-op —
/// otherwise the refcount would underflow and stop the tap out from under a live
/// sibling subscriber.
#[test]
fn spectrum_gate_invisible_then_disconnect_releases_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let count = AtomicUsize::new(0);
    let mut g = super::session::SpectrumGate::new();

    g.apply(true, &count); // visible: +1
    assert_eq!(
        g.apply(false, &count),
        Some(false),
        "going invisible is the sole release"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    // Teardown while already invisible must not decrement again.
    assert_eq!(
        g.apply(false, &count),
        None,
        "teardown after invisibility never double-decrements"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

/// #559: multiple on-screen subscribers share the tap through the refcount — it
/// activates on the first and deactivates only on the last. A bar-mounted
/// subscriber is always on-screen (modeled as a gate held constantly `true`), so
/// it keeps its unit for the connection's life and a sidebar card opening/closing
/// beneath it never toggles the tap.
#[test]
fn spectrum_gate_refcount_shares_the_tap_across_connections() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let count = AtomicUsize::new(0);
    let mut sidebar = super::session::SpectrumGate::new();
    let mut bar = super::session::SpectrumGate::new();

    // The bar chip (always on-screen) starts the tap.
    assert_eq!(
        bar.apply(true, &count),
        Some(true),
        "the first on-screen subscriber starts the tap"
    );
    // A sidebar card opens: a second unit, tap already running → no edge.
    assert_eq!(
        sidebar.apply(true, &count),
        None,
        "a second on-screen subscriber adds no edge"
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    // The sidebar closes: back to one (the bar) — still no 1→0 edge.
    assert_eq!(
        sidebar.apply(false, &count),
        None,
        "the bar subscriber keeps the tap alive"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    // The bar subscriber finally leaves: 1→0, deactivate.
    assert_eq!(
        bar.apply(false, &count),
        Some(false),
        "the last subscriber stops the tap"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

/// Build a full `ListenerCtx` for the session-lane end-to-end tests, returning
/// the visibility and now-playing senders the test drives. Unlike [`ctx_with`]
/// (which surfaces only clock + visibility), this exposes the now-playing channel
/// so the #542 unpark re-seed can be exercised through `handle_conn`.
fn ctx_now_playing_lane() -> (ListenerCtx, watch::Sender<bool>, watch::Sender<NowPlaying>) {
    let (effects_tx, _effects_rx) = mpsc::unbounded_channel();
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (visibility_tx, visibility_rx) = watch::channel(false);
    let (_accent_tx, accent_rx) = watch::channel(None);
    let (_spectrum_tx, spectrum_rx) = watch::channel(None);
    let (_calendar_tx, calendar_rx) = watch::channel(Vec::new());
    let (now_playing_tx, now_playing_rx) = watch::channel(NowPlaying::default());
    let (_locked_tx, locked_rx) = watch::channel(false);
    let ctx = ListenerCtx {
        sidebar_lead: Mutable::new(Vec::new()),
        sidebar_top: Mutable::new(Vec::new()),
        sidebar_bottom: Mutable::new(Vec::new()),
        bar_left: Mutable::new(Vec::new()),
        bar_center: Mutable::new(Vec::new()),
        bar_right: Mutable::new(Vec::new()),
        panels: Mutable::new(Vec::new()),
        clock_rx,
        visibility_rx,
        accent_rx,
        spectrum_rx,
        calendar_rx,
        now_playing_rx,
        locked_rx,
        live_ids: Arc::new(Mutex::new(HashSet::new())),
        runtime: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
        effects_tx,
        datasource: DatasourceRouter::default(),
    };
    (ctx, visibility_tx, now_playing_tx)
}

/// #542, end to end through `handle_conn`: when a parked sidebar card unparks
/// (`SlotVisible` false→true), the host re-seeds the current now-playing **after**
/// the `SlotVisibility(true)` frame — so a marquee that dropped now-playing
/// pushes while hidden resumes with the live track, not a stale one. Ordering is
/// the crux (the widget only adopts now-playing while visible), which is why the
/// re-seed rides the visibility task rather than a racing separate task.
#[tokio::test]
async fn now_playing_is_reseeded_on_the_unpark_edge() {
    let (ctx, vis_tx, np_tx) = ctx_now_playing_lane();
    // Seed a live track BEFORE connect, so the now-playing task seeds it and no
    // later change fires — keeping the post-edge frames deterministic.
    let track = NowPlaying {
        title: "Chrome Rain".to_owned(),
        artist: "Choom".to_owned(),
        playing: true,
        position_us: 83_000_000,
        length_us: 296_000_000,
    };
    np_tx.send_replace(track.clone());

    let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host_end, &ctx).await });

    let (mut prd, mut pwr) = plugin_end.into_split();
    // A sidebar card that parks on visibility and shows the live track (the
    // audio-widget's shape): subscribes SlotVisible + NowPlaying, holds the
    // NowPlaying capability the push is gated on.
    let mut manifest = Manifest::new("marquee", Mount::SidebarTop);
    manifest.subscribes = vec![StateKey::SlotVisible, StateKey::NowPlaying];
    manifest.capabilities = vec![Capability::NowPlaying];
    write_frame(&mut pwr, &PluginMsg::Register { manifest })
        .await
        .expect("send Register");

    // The two register seeds arrive from two tasks in an unspecified order:
    // SlotVisibility(false) and NowPlaying(track). Collect both.
    let mut got_vis_seed = false;
    let mut got_np_seed = false;
    for _ in 0..2 {
        match recv(&mut prd).await {
            HostMsg::SlotVisibility { visible: false } => got_vis_seed = true,
            HostMsg::NowPlaying { now_playing } => {
                assert_eq!(now_playing, track, "the now-playing seed carries the track");
                got_np_seed = true;
            }
            other => panic!("unexpected register seed: {other:?}"),
        }
    }
    assert!(
        got_vis_seed && got_np_seed,
        "both the visibility and now-playing register seeds arrive",
    );

    // Unpark: open the sidebar. The visibility task sends SlotVisibility(true)…
    vis_tx.send_replace(true);
    assert!(
        matches!(
            recv(&mut prd).await,
            HostMsg::SlotVisibility { visible: true }
        ),
        "the unpark edge delivers SlotVisibility(true) first",
    );
    // …then re-seeds the current now-playing, ordered right behind it, so the
    // just-unparked marquee adopts the live track instead of resuming stale.
    match recv(&mut prd).await {
        HostMsg::NowPlaying { now_playing } => assert_eq!(
            now_playing, track,
            "the unpark re-seed carries the current track"
        ),
        other => {
            panic!("expected the now-playing re-seed after SlotVisibility(true), got {other:?}")
        }
    }
}

/// #542: the unpark re-seed is scoped to the RISING edge. Closing the sidebar
/// (true→false) delivers `SlotVisibility(false)` but must NOT re-seed now-playing
/// — only a parked card *reopening* needs the refresh, not one going away. Proven
/// by using a real now-playing change after the close as a sync barrier: the very
/// next frame must be that change, so no spurious re-seed slipped in between.
#[tokio::test]
async fn now_playing_reseed_only_fires_on_the_rising_edge() {
    let (ctx, vis_tx, np_tx) = ctx_now_playing_lane();
    let track = NowPlaying {
        title: "Neon".to_owned(),
        artist: "Choom".to_owned(),
        playing: true,
        position_us: 0,
        length_us: 0,
    };
    np_tx.send_replace(track.clone());

    let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host_end, &ctx).await });

    let (mut prd, mut pwr) = plugin_end.into_split();
    let mut manifest = Manifest::new("marquee2", Mount::SidebarTop);
    manifest.subscribes = vec![StateKey::SlotVisible, StateKey::NowPlaying];
    manifest.capabilities = vec![Capability::NowPlaying];
    write_frame(&mut pwr, &PluginMsg::Register { manifest })
        .await
        .expect("send Register");

    // Consume the two register seeds (SlotVisibility(false) + NowPlaying(track)),
    // unspecified order.
    for _ in 0..2 {
        match recv(&mut prd).await {
            HostMsg::SlotVisibility { visible: false } | HostMsg::NowPlaying { .. } => {}
            other => panic!("unexpected register seed: {other:?}"),
        }
    }

    // Open (rising edge): SlotVisibility(true) then the now-playing re-seed.
    vis_tx.send_replace(true);
    assert!(matches!(
        recv(&mut prd).await,
        HostMsg::SlotVisibility { visible: true }
    ));
    assert!(
        matches!(recv(&mut prd).await, HostMsg::NowPlaying { .. }),
        "the rising edge re-seeds now-playing",
    );

    // Close (falling edge): SlotVisibility(false) and nothing else. Drive a real
    // now-playing change as the sync barrier — the frame right after the close
    // must be that change, not a spurious re-seed of the old track.
    vis_tx.send_replace(false);
    assert!(
        matches!(
            recv(&mut prd).await,
            HostMsg::SlotVisibility { visible: false }
        ),
        "closing delivers the visibility edge",
    );
    let next = NowPlaying {
        title: "Rain".to_owned(),
        artist: "Choom".to_owned(),
        playing: true,
        position_us: 0,
        length_us: 0,
    };
    np_tx.send_replace(next.clone());
    match recv(&mut prd).await {
        HostMsg::NowPlaying { now_playing } => assert_eq!(
            now_playing, next,
            "no re-seed on the falling edge — the next frame is the real change",
        ),
        other => panic!("the falling edge must not re-seed now-playing: {other:?}"),
    }
}

/// The shell's own preem surfaces follow the desktop accent (#862).
///
/// #857 made the shell rasterise `hytte-preem` in-process (the stats drawer's
/// per-core LED panel), and the kit resolves palette ink from a **process
/// global** that only `hytte_preem::set_accent` writes. Nothing in the shell
/// wrote it, so a shell-side surface asking for palette ink rendered the kit
/// default while every out-of-process plugin correctly followed the session
/// accent.
///
/// Rendering is the only way to observe this: the kit exposes no accent
/// getter, deliberately — the accent is an input to `palette()`, not state a
/// caller reads back. So this drives a real widget and looks at its pixels.
///
/// **Deletion check:** removing the `set_accent` call from
/// [`tint_in_process_surfaces`] turns all three assertions red. The remaining
/// seam — `publish_accent` calling it — needs a registered `PluginHandles` and
/// is not covered here.
///
/// Touches process-global state and restores `None` before returning; if a
/// second test ever reads the accent, the two must not run concurrently.
#[test]
fn the_accent_reaches_the_shells_own_preem_surfaces() {
    // Held for the whole test: this one *moves* the process-global the every
    // preem render reads (see `PREEM_INK_LOCK`).
    let _ink = preem_ink_lock();
    let lit_pixels = |accent: Option<[u8; 4]>| {
        tint_in_process_surfaces(accent);
        hytte_preem::dot_matrix("8", hytte_preem::DisplayStyle::Vfd)
            .data()
            .to_vec()
    };

    let plain = lit_pixels(None);
    let teal = lit_pixels(Some([0x11, 0x99, 0xaa, 0xff]));
    let rose = lit_pixels(Some([0xdd, 0x22, 0x66, 0xff]));
    tint_in_process_surfaces(None);

    assert_ne!(
        plain, teal,
        "setting an accent must change what a palette-ink widget renders (#862)"
    );
    assert_ne!(
        teal, rose,
        "two different accents must render differently, so the first assertion cannot pass \
         merely because any call at all perturbs the output"
    );
    assert!(
        teal.chunks_exact(4)
            .any(|px| px == [0x11, 0x99, 0xaa, 0xff]),
        "a fully-lit dot should carry the accent exactly, not merely something derived from it"
    );
}

// ── #883: shell-side preem renderers ─────────────────────────────────────────

/// Serialises every test that renders through `hytte-preem` against the one
/// that *moves* the kit's accent.
///
/// `hytte_preem`'s accent is a process-global `AtomicU32` that **every** widget
/// reads at render time (`style.rs`'s `palette()`), and the harness runs test
/// functions concurrently in one process. A parity test compares two renders
/// taken moments apart; an accent flip landing between them would make them
/// differ for a reason that has nothing to do with the code under test. Every
/// preem test below takes this lock, and so does
/// [`the_accent_reaches_the_shells_own_preem_surfaces`], which is the only test
/// that writes the global.
static PREEM_INK_LOCK: Mutex<()> = Mutex::new(());

/// Take [`PREEM_INK_LOCK`], surviving a poisoning by an unrelated test's panic
/// (the data is `()`, so there is nothing to be inconsistent about).
fn preem_ink_lock() -> std::sync::MutexGuard<'static, ()> {
    PREEM_INK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Advance every live renderer by `dt` and answer "did anything move".
///
/// [`preem_render::advance_all`] names the scopes that moved (so the repaint
/// fan-out can nudge only the mailboxes holding them); the tests below only care
/// whether the tick did anything, and say so once here.
fn advanced(dt: f32) -> bool {
    !preem_render::advance_all(dt).is_empty()
}

/// A `Node::Preem` carrying `widget`, with one CSS class so the mapping's
/// class passthrough is asserted on every parity case rather than once.
fn preem_node(id: Option<&str>, widget: vocab::PreemWidget) -> wire::Node {
    wire::Node::Preem {
        id: id.map(str::to_owned),
        classes: vec!["ts-preem".into()],
        widget: Box::new(widget),
    }
}

/// Map `node` through the real host path and take the RGBA8 surface out of it,
/// asserting the invariants every preem node must satisfy on the way.
fn mapped_pixels(scope: &Scope, node: &wire::Node) -> (u32, u32, Vec<u8>) {
    match to_ui_node(scope, node) {
        UiNode::Pixels {
            width,
            height,
            data,
            scale,
            classes,
            ..
        } => {
            assert_eq!(
                scale, 1,
                "the kit bakes its own upscale into the buffer, so the host must not scale again",
            );
            assert_eq!(
                classes,
                vec!["ts-preem".to_owned()],
                "the preem arm keeps the node's classes, like every other arm",
            );
            assert_eq!(
                data.len(),
                usize::try_from(width).expect("width fits usize")
                    * usize::try_from(height).expect("height fits usize")
                    * 4,
                "a preem surface must honor the same RGBA8 size invariant as Node::Pixels",
            );
            (width, height, data)
        }
        other => panic!("a Node::Preem must map to Pixels, got {other:?}"),
    }
}

/// A horizontal row of **same-config** gauges — the interchangeable-sibling
/// shape #900 is about, where nothing but the node key can tell two widgets
/// apart. `None` for an id spells the anonymous fallback.
fn gauge_row<'a>(gauges: impl IntoIterator<Item = (Option<&'a str>, f32)>) -> wire::Node {
    wire::Node::Box {
        id: Some("row".into()),
        dir: wire::Dir::Horizontal,
        spacing: 0,
        scroll: false,
        classes: vec![],
        children: gauges
            .into_iter()
            .map(|(id, target)| {
                preem_node(
                    id,
                    vocab::PreemWidget::Gauge {
                        config: vocab::GaugeConfig::default(),
                        state: vocab::GaugeState { target },
                    },
                )
            })
            .collect(),
    }
}

/// The `(width, height, data)` of each `Pixels` child of a mapped row, in order
/// — how the sibling-keying tests read one node's frame out of a multi-node
/// render.
fn mapped_row_pixels(scope: &Scope, node: &wire::Node) -> Vec<(u32, u32, Vec<u8>)> {
    match to_ui_node(scope, node) {
        UiNode::Box { children, .. } => children
            .into_iter()
            .map(|child| match child {
                UiNode::Pixels {
                    width,
                    height,
                    data,
                    ..
                } => (width, height, data),
                other => panic!("a preem child must map to Pixels, got {other:?}"),
            })
            .collect(),
        other => panic!("expected the row's Box, got {other:?}"),
    }
}

/// A kit frame in the same `(w, h, bytes)` shape [`mapped_pixels`] returns — the
/// parity oracle's side of every comparison below.
fn kit_pixels(frame: &kit::Frame) -> (u32, u32, Vec<u8>) {
    (
        u32::try_from(frame.width()).expect("kit width fits u32"),
        u32::try_from(frame.height()).expect("kit height fits u32"),
        frame.data().to_vec(),
    )
}

/// The kit skin a wire [`vocab::StyleName`] names. Spelled out here rather than
/// imported from the module under test: an oracle that borrows the code's own
/// resolver agrees with it by construction.
fn kit_style(style: vocab::StyleName) -> kit::DisplayStyle {
    match style {
        vocab::StyleName::Vfd => kit::DisplayStyle::Vfd,
        vocab::StyleName::Lcd => kit::DisplayStyle::Lcd,
        vocab::StyleName::Oled => kit::DisplayStyle::Oled,
        vocab::StyleName::Crt => kit::DisplayStyle::Crt,
    }
}

/// Visual parity, `DotMatrix`: the shell's renderer must produce byte-identical
/// pixels to the kit call a plugin would have made itself — in **every** skin,
/// which also exercises the by-name `StyleName` → `DisplayStyle` resolution and
/// (because the node id is reused) the config-change rebuild.
#[test]
fn dot_matrix_renders_at_parity_with_the_kit() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("parity-dot-matrix");
    for style in vocab::StyleName::ALL {
        let node = preem_node(
            Some("dm"),
            vocab::PreemWidget::DotMatrix {
                config: vocab::DotMatrixConfig {
                    style: vocab::StyleRef::new(style),
                },
                state: vocab::DotMatrixState {
                    text: "PREEM 42".into(),
                },
            },
        );
        assert_eq!(
            mapped_pixels(&scope, &node),
            kit_pixels(&kit::dot_matrix("PREEM 42", kit_style(style))),
            "dot-matrix parity in the {} skin",
            style.name(),
        );
    }
}

/// Visual parity, `SevenSeg`, in every skin.
#[test]
fn seven_seg_renders_at_parity_with_the_kit() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("parity-seven-seg");
    for style in vocab::StyleName::ALL {
        let node = preem_node(
            Some("ss"),
            vocab::PreemWidget::SevenSeg {
                config: vocab::SevenSegConfig {
                    style: vocab::StyleRef::new(style),
                },
                state: vocab::SevenSegState {
                    text: "12:34".into(),
                },
            },
        );
        assert_eq!(
            mapped_pixels(&scope, &node),
            kit_pixels(&kit::seven_seg("12:34", kit_style(style))),
            "seven-segment parity in the {} skin",
            style.name(),
        );
    }
}

/// Visual parity, `TextBox` — the widget with the most config, so the oracle
/// spells the whole builder chain out and a mis-ordered or dropped knob shows
/// up as different bytes.
#[test]
fn text_box_renders_at_parity_with_the_kit() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("parity-text-box");
    let config = vocab::TextBoxConfig {
        style: vocab::StyleRef::new(vocab::StyleName::Lcd),
        width: vocab::TextBoxWidth::Cols(12),
        max_lines: 2,
        pad: 4,
        corner: 3,
        scale: 2,
        fixed_width: true,
        notdef: None,
    };
    let text = "the quick brown fox jumps";
    let node = preem_node(
        Some("tb"),
        vocab::PreemWidget::TextBox {
            config,
            state: vocab::TextBoxState { text: text.into() },
        },
    );
    let oracle = kit::TextBox::styled(kit::DisplayStyle::Lcd)
        .cols(12)
        .max_lines(2)
        .pad(4)
        .corner(3)
        .scale(2)
        .fixed_width(true);
    assert_eq!(
        mapped_pixels(&scope, &node),
        kit_pixels(&oracle.render(text)),
        "text-box parity across the whole builder chain",
    );

    // The other width spec is a different kit method, so it gets its own case.
    let fit = vocab::TextBoxConfig {
        width: vocab::TextBoxWidth::FitPx(160),
        ..config
    };
    let fit_node = preem_node(
        Some("tb"),
        vocab::PreemWidget::TextBox {
            config: fit,
            state: vocab::TextBoxState { text: text.into() },
        },
    );
    let fit_oracle = kit::TextBox::styled(kit::DisplayStyle::Lcd)
        .fit_px(160)
        .max_lines(2)
        .pad(4)
        .corner(3)
        .scale(2)
        .fixed_width(true);
    assert_eq!(
        mapped_pixels(&scope, &fit_node),
        kit_pixels(&fit_oracle.render(text)),
        "text-box parity with a FitPx width",
    );
}

/// Visual parity, `LedStrip`, in the three peak configurations the vocabulary
/// distinguishes: no peak at all, a shell-held one, and the plugin's own
/// explicit override.
#[test]
fn led_strip_renders_at_parity_with_the_kit() {
    let _ink = preem_ink_lock();
    let style = vocab::StyleRef::new(vocab::StyleName::Oled);
    let strip = kit::LedStrip::new(kit::DisplayStyle::Oled).leds(32);

    // 1. No peak-hold and no explicit peak: the kit's "no peak dot" reading.
    let plain = preem_node(
        Some("vu"),
        vocab::PreemWidget::LedStrip {
            config: vocab::LedStripConfig {
                style,
                leds: 32,
                peak_hold: None,
            },
            state: vocab::LedStripState {
                level: 0.6,
                peak: None,
            },
        },
    );
    assert_eq!(
        mapped_pixels(&Scope::detached("parity-led-plain"), &plain),
        kit_pixels(&strip.render(0.6, 0.0)),
        "a strip with neither peak source renders with no peak dot",
    );

    // 2. Shell-held peak: the level is folded into a `PeakHold` at build time.
    let held = preem_node(
        Some("vu"),
        vocab::PreemWidget::LedStrip {
            config: vocab::LedStripConfig {
                style,
                leds: 32,
                peak_hold: Some(vocab::PeakHoldConfig { rate: 0.1 }),
            },
            state: vocab::LedStripState {
                level: 0.6,
                peak: None,
            },
        },
    );
    let mut oracle_hold = kit::PeakHold::new(0.1);
    oracle_hold.push(0.6);
    assert_eq!(
        mapped_pixels(&Scope::detached("parity-led-held"), &held),
        kit_pixels(&strip.render(0.6, oracle_hold.value())),
        "a declared peak-hold rides the level the shell was given",
    );

    // 3. An explicit peak wins for the render it arrives on.
    let explicit = preem_node(
        Some("vu"),
        vocab::PreemWidget::LedStrip {
            config: vocab::LedStripConfig {
                style,
                leds: 32,
                peak_hold: Some(vocab::PeakHoldConfig { rate: 0.1 }),
            },
            state: vocab::LedStripState {
                level: 0.6,
                peak: Some(0.95),
            },
        },
    );
    assert_eq!(
        mapped_pixels(&Scope::detached("parity-led-explicit"), &explicit),
        kit_pixels(&strip.render(0.6, 0.95)),
        "an explicit peak overrides the held one for that render",
    );
}

/// Visual parity, `Marquee`, at rest **and** after one advance — the pair that
/// proves the shell's dots-per-second integration lands on the same whole-dot
/// window the kit would have been asked for.
#[test]
fn marquee_renders_at_parity_with_the_kit_before_and_after_a_scroll() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("parity-marquee");
    let text = "SCROLLING MARQUEE TEST";
    let node = preem_node(
        Some("mq"),
        vocab::PreemWidget::Marquee {
            config: vocab::MarqueeConfig {
                style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                window_px: 192,
                gap_dots: 6,
                speed_dots_per_sec: 20.0,
            },
            state: vocab::MarqueeState { text: text.into() },
        },
    );
    let oracle = kit::Marquee::new(kit::DisplayStyle::Vfd)
        .window_px(192)
        .gap_dots(6)
        .render(text);
    assert!(
        oracle.scrolls(),
        "the fixture must be long enough to scroll, or the advance below proves nothing",
    );

    assert_eq!(
        mapped_pixels(&scope, &node),
        kit_pixels(&oracle.window(0)),
        "a fresh marquee starts at the left edge",
    );

    // Half a second at 20 dots/s is exactly ten whole dots — the offset the kit
    // would have been handed by a plugin stepping one dot per 20 Hz beat.
    assert!(
        advanced(0.5),
        "advancing a scrolling marquee must report that it moved",
    );
    assert_eq!(
        mapped_pixels(&scope, &node),
        kit_pixels(&oracle.window(10)),
        "0.5 s at 20 dots/s is a ten-dot window",
    );
}

/// Visual parity, `Scope`, at the debut batch and after one identical advance —
/// the phosphor decay is a per-*call* step in the kit, so this also pins that
/// one animation tick issues exactly one of them.
#[test]
fn scope_renders_at_parity_with_the_kit_before_and_after_a_decay() {
    let _ink = preem_ink_lock();
    let scope_key = Scope::detached("parity-scope");
    let samples: Vec<f32> = (0..64_u8).map(|i| f32::from(i % 9) / 4.0 - 1.0).collect();
    let node = preem_node(
        Some("sc"),
        vocab::PreemWidget::Scope {
            config: vocab::ScopeConfig {
                style: vocab::StyleRef::new(vocab::StyleName::Crt),
                cols: 48,
                rows: 24,
                scale: 1,
                persistence: 184,
            },
            state: vocab::ScopeState {
                samples: samples.clone(),
            },
        },
    );
    let mut oracle = kit::Scope::with_size(48, 24).scale(1).persistence(184);
    oracle.advance(&samples);
    assert_eq!(
        mapped_pixels(&scope_key, &node),
        kit_pixels(&oracle.render(kit::DisplayStyle::Crt)),
        "the debut sample batch is stamped before the first frame reaches the screen",
    );

    // One animation step with nothing new to stamp: the trail decays, exactly
    // as an empty batch does in the kit.
    assert!(
        advanced(preem_render::ANIM_STEP_SECS),
        "a fading phosphor trail must report that it moved",
    );
    oracle.advance(&[]);
    assert_eq!(
        mapped_pixels(&scope_key, &node),
        kit_pixels(&oracle.render(kit::DisplayStyle::Crt)),
        "one animation step is exactly one phosphor decay",
    );
}

/// Visual parity, `Gauge`, at the target's arrival and after one advance — the
/// needle physics is closed-form, so the shell integrating it with the real
/// frame `dt` must land on the same `f32` the kit would have.
#[test]
fn gauge_renders_at_parity_with_the_kit_before_and_after_a_swing() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("parity-gauge");
    let node = preem_node(
        Some("gg"),
        vocab::PreemWidget::Gauge {
            config: vocab::GaugeConfig {
                style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                cols: 64,
                rows: 40,
                scale: 1,
                sweep_deg: 150.0,
                divisions: 4,
                subdivisions: 5,
                range: vocab::GaugeRange {
                    low: 0.0,
                    high: 100.0,
                },
                frequency_hz: 2.0,
                damping: 0.5,
            },
            state: vocab::GaugeState { target: 75.0 },
        },
    );
    let mut oracle = kit::Gauge::with_size(64, 40)
        .scale(1)
        .sweep_deg(150.0)
        .ticks(4, 5)
        .range(0.0, 100.0)
        .frequency(2.0)
        .damping(0.5);
    oracle.set_target(75.0);
    assert_eq!(
        mapped_pixels(&scope, &node),
        kit_pixels(&oracle.render(kit::DisplayStyle::Vfd)),
        "a fresh gauge rests at the low end with its target set",
    );

    assert!(
        advanced(preem_render::ANIM_STEP_SECS),
        "an un-settled needle must report that it moved",
    );
    oracle.advance(preem_render::ANIM_STEP_SECS);
    assert_eq!(
        mapped_pixels(&scope, &node),
        kit_pixels(&oracle.render(kit::DisplayStyle::Vfd)),
        "the shell integrates the needle with the same dt the kit would have",
    );
}

/// Visual parity, `FlipBoard`, at the text's arrival and after one advance.
#[test]
fn flip_board_renders_at_parity_with_the_kit_before_and_after_a_flip() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("parity-flip-board");
    let node = preem_node(
        Some("fb"),
        vocab::PreemWidget::FlipBoard {
            config: vocab::FlipBoardConfig {
                style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                mechanism: vocab::Mechanism::SplitFlap,
                cells: 8,
                glyph_px: 2,
                scale: 1,
                // `None` on both means "the mechanism's own default", which the
                // oracle reproduces by *not* calling the two builder methods.
                duration_secs: None,
                stagger_secs: None,
            },
            state: vocab::FlipBoardState {
                text: "12:34:56".into(),
            },
        },
    );
    let mut oracle = kit::FlipBoard::new(kit::Mechanism::SplitFlap)
        .cells(8)
        .glyph_px(2)
        .scale(1);
    oracle.set_text("12:34:56");
    assert_eq!(
        mapped_pixels(&scope, &node),
        kit_pixels(&oracle.render(kit::DisplayStyle::Vfd)),
        "a board that has just been given its text is mid-flip at t=0",
    );

    assert!(
        advanced(0.1),
        "cards still in motion must report that they moved",
    );
    oracle.advance(0.1);
    assert_eq!(
        mapped_pixels(&scope, &node),
        kit_pixels(&oracle.render(kit::DisplayStyle::Vfd)),
        "the shell drives the flip clock with the same dt the kit would have",
    );
}

/// Acceptance criterion 1 (#895's B2 handoff): the renderer rasterises the
/// **clamped** widget, never the raw one.
///
/// The config below asks for a 5000-column, 8× upscaled scope — a buffer the
/// wire caps are there to refuse. The assertion is not merely "it didn't
/// explode": the surface must be byte-identical to the kit rendering
/// `PreemWidget::clamped()`'s output, which is what proves the clamp is on the
/// path rather than merely available.
#[test]
fn an_absurd_preem_config_is_clamped_before_the_renderer_sees_it() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("clamp-seam");
    let raw = vocab::PreemWidget::Scope {
        config: vocab::ScopeConfig {
            style: vocab::StyleRef::new(vocab::StyleName::Vfd),
            cols: 5_000,
            rows: 40,
            scale: 8,
            persistence: 184,
        },
        state: vocab::ScopeState {
            samples: vec![0.5; 128],
        },
    };
    let (width, height, data) = mapped_pixels(&scope, &preem_node(Some("sc"), raw.clone()));
    assert!(
        width <= vocab::MAX_BUFFER_DIM && height <= vocab::MAX_BUFFER_DIM,
        "the rasterised surface must respect the wire's buffer cap, got {width}x{height}",
    );

    let vocab::PreemWidget::Scope { config, state } = raw.clamped() else {
        panic!("clamping a Scope yields a Scope");
    };
    let mut oracle = kit::Scope::with_size(
        usize::try_from(config.cols).expect("clamped cols fit usize"),
        usize::try_from(config.rows).expect("clamped rows fit usize"),
    )
    .scale(usize::try_from(config.scale).expect("clamped scale fits usize"))
    .persistence(config.persistence);
    oracle.advance(&state.samples);
    assert_eq!(
        (width, height, data),
        kit_pixels(&oracle.render(kit::DisplayStyle::Vfd)),
        "the renderer must rasterise the clamped widget, not the raw one",
    );
}

/// Lifecycle: a **state** change updates the instance in place — the renderer is
/// built once and the animation it is running is not restarted.
#[test]
fn a_state_change_updates_the_instance_in_place() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("lifecycle-state");
    let config = vocab::GaugeConfig::default();
    let at = |target: f32| {
        preem_node(
            Some("gg"),
            vocab::PreemWidget::Gauge {
                config,
                state: vocab::GaugeState { target },
            },
        )
    };

    let _ = to_ui_node(&scope, &at(0.25));
    assert_eq!(preem_render::probe(&scope, Some("gg")), Some((1, 1)));

    let _ = to_ui_node(&scope, &at(0.75));
    assert_eq!(
        preem_render::probe(&scope, Some("gg")),
        Some((1, 2)),
        "a new target must be applied to the SAME renderer — one build, two applies",
    );
}

/// Lifecycle: re-mapping an unchanged tree — which is what a second monitor's
/// reconcile does on every render frame — must not re-apply anything.
///
/// Without this, a two-output session would stamp every scope sample batch twice
/// and decay its phosphor twice per frame, i.e. animate at 2× speed.
#[test]
fn re_mapping_an_unchanged_widget_is_a_no_op() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("lifecycle-idempotent");
    let node = preem_node(
        Some("dm"),
        vocab::PreemWidget::DotMatrix {
            config: vocab::DotMatrixConfig::default(),
            state: vocab::DotMatrixState {
                text: "STEADY".into(),
            },
        },
    );

    let first = mapped_pixels(&scope, &node);
    assert_eq!(preem_render::probe(&scope, Some("dm")), Some((1, 1)));

    let second = mapped_pixels(&scope, &node);
    assert_eq!(
        preem_render::probe(&scope, Some("dm")),
        Some((1, 1)),
        "a second monitor mapping the same tree must neither rebuild nor re-apply",
    );
    assert_eq!(first, second, "and it must produce the same surface");
}

/// Lifecycle: a **config** change rebuilds the instance, and so does swapping
/// the widget **kind** under the same node id.
#[test]
fn a_config_or_kind_change_rebuilds_the_instance() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("lifecycle-rebuild");

    let dots = |style: vocab::StyleName| {
        preem_node(
            Some("w"),
            vocab::PreemWidget::DotMatrix {
                config: vocab::DotMatrixConfig {
                    style: vocab::StyleRef::new(style),
                },
                state: vocab::DotMatrixState { text: "A".into() },
            },
        )
    };

    let _ = to_ui_node(&scope, &dots(vocab::StyleName::Vfd));
    assert_eq!(preem_render::probe(&scope, Some("w")), Some((1, 1)));

    let _ = to_ui_node(&scope, &dots(vocab::StyleName::Crt));
    assert_eq!(
        preem_render::probe(&scope, Some("w")),
        Some((2, 2)),
        "a config change must rebuild, not update",
    );

    let _ = to_ui_node(
        &scope,
        &preem_node(
            Some("w"),
            vocab::PreemWidget::SevenSeg {
                config: vocab::SevenSegConfig::default(),
                state: vocab::SevenSegState { text: "1".into() },
            },
        ),
    );
    assert_eq!(
        preem_render::probe(&scope, Some("w")),
        Some((3, 3)),
        "a kind change under the same node id must rebuild too",
    );
}

/// Lifecycle: an instance whose node stops appearing in the tree is dropped at
/// the end of the mapping pass, and `forget_scope` drops the whole tree's worth
/// (what a plugin card leaving its region does).
#[test]
fn instances_are_swept_when_their_node_leaves_the_tree() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("lifecycle-sweep");
    let leaf = |id: &str| {
        preem_node(
            Some(id),
            vocab::PreemWidget::DotMatrix {
                config: vocab::DotMatrixConfig::default(),
                state: vocab::DotMatrixState { text: id.into() },
            },
        )
    };
    let row = |children: Vec<wire::Node>| wire::Node::Row {
        id: Some("row".into()),
        classes: vec![],
        children,
    };

    let _ = to_ui_node(&scope, &row(vec![leaf("a"), leaf("b")]));
    assert_eq!(preem_render::instance_count(&scope), 2);

    let _ = to_ui_node(&scope, &row(vec![leaf("a")]));
    assert_eq!(
        preem_render::instance_count(&scope),
        1,
        "the node the plugin stopped rendering releases its renderer",
    );
    assert!(preem_render::probe(&scope, Some("a")).is_some());
    assert!(preem_render::probe(&scope, Some("b")).is_none());

    preem_render::forget_scope(&scope);
    assert_eq!(
        preem_render::instance_count(&scope),
        0,
        "forgetting a scope drops everything in it",
    );
}

/// Lifecycle: a preem node with no `id` still **renders** — #900 requires the id
/// but the host degrades rather than dropping the widget, so a hand-rolled
/// plugin keeps working. It is keyed by its ordinal among the tree's un-id'd
/// preem nodes, so it animates across frames, and the ordinal is reset per
/// mapping pass rather than climbing forever.
#[test]
fn an_un_idd_preem_node_is_keyed_by_its_ordinal() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("lifecycle-ordinal");
    let anon = |text: &str| {
        preem_node(
            None,
            vocab::PreemWidget::DotMatrix {
                config: vocab::DotMatrixConfig::default(),
                state: vocab::DotMatrixState { text: text.into() },
            },
        )
    };

    let _ = to_ui_node(&scope, &anon("one"));
    assert_eq!(preem_render::probe(&scope, None), Some((1, 1)));

    let _ = to_ui_node(&scope, &anon("two"));
    assert_eq!(
        preem_render::probe(&scope, None),
        Some((1, 2)),
        "the same ordinal slot is reused across passes, so the instance survives",
    );
    assert_eq!(
        preem_render::instance_count(&scope),
        1,
        "the ordinal resets per pass instead of minting a new instance each frame",
    );
    preem_render::forget_scope(&scope);
}

/// **#900's acceptance test.** Three same-config gauges in a row; remove the
/// *first*; the two survivors keep their own needles.
///
/// This is the shape the vocabulary makes hazardous: interchangeable widgets
/// whose configs are identical by construction, so `same_config` agrees and
/// nothing downstream can tell two of them apart — the node key is the *only*
/// thing that can. Written against id'd nodes because #900 settled the policy at
/// "a preem node requires an `id`", and the SDK's `display` wrappers stamp one
/// from the widget key they already take, so this is what a real plugin emits.
///
/// The anonymous spelling of the very same tree still transplants, which is what
/// [`anonymous_gauges_transplant_a_needle_and_warn_once`] pins — the issue's
/// original wording ("today it fails by construction") is that test.
#[test]
fn id_d_gauges_keep_their_own_needles_when_a_sibling_is_removed() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("keying-id-row");
    let three = gauge_row([(Some("g0"), 0.15), (Some("g1"), 0.5), (Some("g2"), 0.85)]);
    let _ = to_ui_node(&scope, &three);

    // A fresh needle rests at the low end whatever its target, so the three are
    // pixel-identical until they have swung apart. Advance first, or every
    // assertion below is vacuous.
    for _ in 0..4 {
        assert!(
            advanced(preem_render::ANIM_STEP_SECS),
            "three un-settled needles must report that they moved",
        );
    }
    let before = mapped_row_pixels(&scope, &three);
    assert_eq!(before.len(), 3, "three gauges, three surfaces");
    assert_ne!(
        before[0], before[1],
        "the fixture must actually separate the needles",
    );
    assert_ne!(before[1], before[2], "…all three of them");

    // Remove the FIRST gauge and re-map with no animation step in between: a
    // survivor that kept its own renderer instance renders byte-identically,
    // and one that inherited a sibling's cannot.
    let two = gauge_row([(Some("g1"), 0.5), (Some("g2"), 0.85)]);
    let after = mapped_row_pixels(&scope, &two);
    assert_eq!(after.len(), 2, "two gauges left");
    assert_eq!(
        after[0], before[1],
        "g1 keeps its OWN needle when g0 is removed — not g0's",
    );
    assert_eq!(after[1], before[2], "and g2 keeps its own");
    assert_eq!(
        preem_render::probe(&scope, Some("g1")).map(|(builds, _)| builds),
        Some(1),
        "…without being rebuilt either, which would have reset it to the low end",
    );
    assert_eq!(
        preem_render::probe(&scope, Some("g2")).map(|(builds, _)| builds),
        Some(1),
    );
    assert_eq!(
        preem_render::instance_count(&scope),
        2,
        "the removed node's instance is swept at the end of the pass",
    );
    preem_render::forget_scope(&scope);
}

/// The same row, spelled **anonymously**: the transplant #900 is about, pinned
/// as the documented cost of the fallback rather than fixed — plus the one
/// warning that makes it diagnosable.
///
/// Every assertion here is "documented, not desired". The host renders the node
/// instead of refusing it (a hand-rolled client degrades to the pre-#900
/// behaviour rather than losing its widget), and says once per tree why that
/// widget may misbehave.
///
/// The warning is counted at its emitting call site
/// (`preem_render::anonymous_warnings`) rather than captured from `tracing`:
/// nothing in this file installs a subscriber, so there is no capture harness to
/// read, and the counter is bumped inside the same `if` that logs.
#[test]
fn anonymous_gauges_transplant_a_needle_and_warn_once() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("keying-anonymous-row");
    let warned = preem_render::anonymous_warnings();
    let three = gauge_row([(None, 0.15), (None, 0.5), (None, 0.85)]);
    let _ = to_ui_node(&scope, &three);
    assert_eq!(
        preem_render::anonymous_warnings() - warned,
        1,
        "three anonymous nodes in one tree are ONE warning, not three",
    );

    for _ in 0..4 {
        assert!(advanced(preem_render::ANIM_STEP_SECS));
    }
    let before = mapped_row_pixels(&scope, &three);
    assert_ne!(
        before[0], before[1],
        "the fixture must actually separate the needles",
    );
    assert_ne!(before[1], before[2], "…all three of them");

    let two = gauge_row([(None, 0.5), (None, 0.85)]);
    let after = mapped_row_pixels(&scope, &two);
    assert_eq!(
        after[0], before[0],
        "the transplant: the second gauge lands on the first's ordinal slot and renders the \
         REMOVED node's needle. An id avoids this — see \
         `id_d_gauges_keep_their_own_needles_when_a_sibling_is_removed`",
    );
    assert_eq!(
        after[1], before[1],
        "and the third inherits the second's, all the way down the row",
    );
    assert_eq!(
        preem_render::anonymous_warnings() - warned,
        1,
        "and the warning stays latched for the scope's lifetime — three mapping passes, \
         one journal line",
    );
    preem_render::forget_scope(&scope);
}

/// The anonymous-node warning is latched **per scope**, not per frame and not
/// per process: one line for a tree however many frames it renders, a separate
/// line for the plugin's other tree, and nothing at all for an id'd node.
///
/// At 20 Hz a per-frame warning would be twenty identical journal lines a
/// second, which is the `UNSUPPORTED_WARNED` lesson applied one scope down.
#[test]
fn the_anonymous_preem_warning_is_once_per_scope_not_once_per_frame() {
    let _ink = preem_ink_lock();
    let base = preem_render::anonymous_warnings();
    let widget = || vocab::PreemWidget::DotMatrix {
        config: vocab::DotMatrixConfig::default(),
        state: vocab::DotMatrixState {
            text: "ANON".into(),
        },
    };
    let anon = preem_node(None, widget());

    let card = Scope::detached("anon-warn-card");
    let _ = to_ui_node(&card, &anon);
    assert_eq!(
        preem_render::anonymous_warnings() - base,
        1,
        "the first anonymous node in a scope warns",
    );
    let _ = to_ui_node(&card, &anon);
    let _ = to_ui_node(&card, &anon);
    assert_eq!(
        preem_render::anonymous_warnings() - base,
        1,
        "three frames of the same tree are still one warning",
    );

    // A plugin's two trees are two scopes, and each deserves to hear about its
    // own: the latch is keyed by `Scope`, not by a process-wide flag.
    let panel = Scope::detached("anon-warn-panel");
    let _ = to_ui_node(&panel, &anon);
    assert_eq!(
        preem_render::anonymous_warnings() - base,
        2,
        "a different tree gets its own line",
    );

    // And the contract-honoring spelling is silent.
    let id_d = Scope::detached("anon-warn-id-d");
    let _ = to_ui_node(&id_d, &preem_node(Some("dm"), widget()));
    assert_eq!(
        preem_render::anonymous_warnings() - base,
        2,
        "an id'd node is the contract being met, not something to warn about",
    );

    preem_render::forget_scope(&card);
    preem_render::forget_scope(&panel);
    preem_render::forget_scope(&id_d);
}

/// The warning survives a frame in which the plugin renders **no preem node at
/// all** — and survives an explicit scope teardown.
///
/// A `ScopeState` is not a plugin session, and this is the regression that
/// proves the latch does not live on one. `end_pass` drops the whole entry the
/// moment a mapping pass leaves the scope with zero instances, and
/// `forget_scope` runs on a drawer *close* as well as on a plugin leaving
/// (`region.rs`'s `forget_previous_panel_scope` — its own rustdoc says so). With
/// the latch on the `ScopeState`, both re-armed it:
///
/// - a **conditionally-rendered** preem node (a gauge shown only while something
///   runs) warned again on every appearance — worst case one line every other
///   render, ~10 a second at a 20 Hz plugin, which is precisely the stream the
///   latch exists to prevent;
/// - a **drawer panel** holding an anonymous node warned once per drawer *open*,
///   deterministically, with no toggling at all.
///
/// So the contract is at most once per plugin tree for the shell's run. See
/// `preem_render`'s `WARNED`.
#[test]
fn the_anonymous_preem_warning_survives_an_emptied_scope() {
    let _ink = preem_ink_lock();
    let base = preem_render::anonymous_warnings();
    let scope = Scope::detached("anon-warn-emptied");
    let anon = preem_node(
        None,
        vocab::PreemWidget::DotMatrix {
            config: vocab::DotMatrixConfig::default(),
            state: vocab::DotMatrixState {
                text: "BLINK".into(),
            },
        },
    );
    // A frame of the same tree carrying no preem node at all — what a plugin
    // renders while its gauge has nothing to show.
    let nothing = wire::Node::Label {
        id: None,
        text: "idle".into(),
        classes: vec![],
    };

    let _ = to_ui_node(&scope, &anon);
    assert_eq!(
        preem_render::anonymous_warnings() - base,
        1,
        "the node's first appearance warns",
    );

    let _ = to_ui_node(&scope, &nothing);
    assert_eq!(
        preem_render::instance_count(&scope),
        0,
        "the blank frame must really empty the scope, or this test proves nothing",
    );

    // Present → absent → present → absent → present: still one line.
    let _ = to_ui_node(&scope, &anon);
    let _ = to_ui_node(&scope, &nothing);
    let _ = to_ui_node(&scope, &anon);
    assert_eq!(
        preem_render::anonymous_warnings() - base,
        1,
        "three appearances across two preem-less frames are ONE warning, not one per appearance",
    );

    // And an explicit teardown — a card leaving its region, or the drawer
    // closing on a panel — does not re-arm it either.
    preem_render::forget_scope(&scope);
    let _ = to_ui_node(&scope, &anon);
    assert_eq!(
        preem_render::anonymous_warnings() - base,
        1,
        "nor does forget_scope, which fires on a drawer close and not only on a plugin leaving",
    );
    preem_render::forget_scope(&scope);
}

// ── #901: bounds on a render tree ────────────────────────────────────────────

/// A `Box` root carrying `children` id'd `Label`s — a tree of exactly
/// `1 + children` nodes whose mapped prefix can be read straight off the child
/// ids, so a truncation test can assert *which* nodes survived and not merely
/// how many.
fn label_tree(children: usize) -> wire::Node {
    wire::Node::Box {
        id: Some("root".into()),
        dir: wire::Dir::Vertical,
        spacing: 0,
        scroll: false,
        classes: vec![],
        children: (0..children)
            .map(|i| wire::Node::Label {
                id: Some(format!("n{i}")),
                text: String::new(),
                classes: vec![],
            })
            .collect(),
    }
}

/// The ids of a mapped [`label_tree`]'s children, in order.
fn mapped_label_ids(scope: &Scope, node: &wire::Node) -> Vec<String> {
    match to_ui_node(scope, node) {
        UiNode::Box { children, .. } => children
            .into_iter()
            .map(|child| match child {
                UiNode::Label { id, .. } => id.expect("the fixture ids every label"),
                other => panic!("expected a Label child, got {other:?}"),
            })
            .collect(),
        other => panic!("expected the root Box, got {other:?}"),
    }
}

/// **#901's acceptance test for the preem cap.** One more preem node than
/// [`wire::MAX_PREEM_NODES_PER_TREE`]: the nodes inside the cap get renderer
/// instances and render, the one past it gets the unknown-widget placeholder,
/// and the tree hears about it exactly once however many frames it renders.
///
/// The cap is on **live instances**, not on nodes seen this pass, so the
/// surviving prefix is stable: the same 64 nodes keep their instances (and their
/// animation state) frame after frame rather than being rebuilt as the table
/// churns. That is what the `builds == 1` and the repeated `instance_count`
/// assertions below are for.
///
/// Serialised on the ink lock like every other preem test — here not for the
/// accent but for the process-global warning counters, which are read as deltas.
#[test]
fn preem_nodes_past_the_instance_cap_render_the_placeholder_and_warn_once() {
    let _ink = preem_ink_lock();
    let base = preem_render::instance_cap_warnings();
    let scope = Scope::detached("preem-instance-cap");

    let ids: Vec<String> = (0..=wire::MAX_PREEM_NODES_PER_TREE)
        .map(|i| format!("g{i}"))
        .collect();
    let row = |n: usize| gauge_row(ids.iter().take(n).map(|id| (Some(id.as_str()), 0.5)));
    let over = row(wire::MAX_PREEM_NODES_PER_TREE + 1);

    let mapped = mapped_row_pixels(&scope, &over);
    assert_eq!(
        mapped.len(),
        wire::MAX_PREEM_NODES_PER_TREE + 1,
        "every node still maps to a surface — the cap withholds a renderer, not a widget",
    );
    for (i, (width, height, data)) in mapped
        .iter()
        .take(wire::MAX_PREEM_NODES_PER_TREE)
        .enumerate()
    {
        assert!(
            *width > 0 && *height > 0 && !data.is_empty(),
            "node {i} is inside the cap and must render for real",
        );
    }
    assert_eq!(
        mapped[wire::MAX_PREEM_NODES_PER_TREE],
        (0, 0, Vec::new()),
        "the node past the cap renders the unknown-widget placeholder — the same empty \
         surface an unrenderable kind degrades to, keeping its id and classes",
    );
    assert_eq!(
        preem_render::instance_count(&scope),
        wire::MAX_PREEM_NODES_PER_TREE,
        "…and no instance was created for it: the cap is on renderer instances",
    );
    assert_eq!(
        preem_render::instance_cap_warnings() - base,
        1,
        "one node past the cap is one journal line",
    );

    // Two more frames of the same tree: the prefix keeps its instances (an
    // over-cap tree must not churn the table), and the line does not repeat.
    let _ = to_ui_node(&scope, &over);
    let _ = to_ui_node(&scope, &over);
    assert_eq!(
        preem_render::instance_cap_warnings() - base,
        1,
        "three frames of a tree that is over the cap on every one of them are ONE warning",
    );
    assert_eq!(
        preem_render::instance_count(&scope),
        wire::MAX_PREEM_NODES_PER_TREE,
        "and the same prefix holds the instances frame after frame",
    );
    assert_eq!(
        preem_render::probe(&scope, Some("g0")).map(|(builds, _)| builds),
        Some(1),
        "an in-cap node is never rebuilt because a sibling fell past the cap",
    );

    // The off-by-one control, in its own scope so the latch above cannot mask
    // it: a tree of *exactly* the cap is not over it.
    let at_cap = Scope::detached("preem-instance-cap-exact");
    let exact = row(wire::MAX_PREEM_NODES_PER_TREE);
    let mapped = mapped_row_pixels(&at_cap, &exact);
    assert!(
        mapped
            .iter()
            .all(|(width, _, data)| *width > 0 && !data.is_empty()),
        "every node of a tree exactly at the cap renders",
    );
    assert_eq!(
        preem_render::instance_count(&at_cap),
        wire::MAX_PREEM_NODES_PER_TREE,
        "…with a full set of instances",
    );
    assert_eq!(
        preem_render::instance_cap_warnings() - base,
        1,
        "exactly at the cap is not over it — no second line",
    );

    preem_render::forget_scope(&scope);
    preem_render::forget_scope(&at_cap);
}

/// **#901's acceptance test for the general node cap.** A tree one node past
/// [`wire::MAX_NODES_PER_TREE`] maps its prefix and drops the rest, with one
/// warning per tree.
///
/// Truncate rather than reject: `wire_map`'s posture is *degrade, don't blank*
/// (the malformed-`Pixels` arm sets it), and a rejected frame would leave the
/// previous one on screen, which looks exactly like a hung plugin. The prefix is
/// asserted by **id**, in order, so this measures "kept the prefix" and not just
/// "kept some nodes".
#[test]
fn a_tree_over_the_node_cap_keeps_its_prefix_and_warns_once() {
    let _ink = preem_ink_lock();
    let base = preem_render::node_cap_warnings();
    let scope = Scope::detached("node-cap-over");

    // Root + MAX children = MAX + 1 nodes: exactly one past the cap.
    let over = label_tree(wire::MAX_NODES_PER_TREE);
    let ids = mapped_label_ids(&scope, &over);
    assert_eq!(
        ids.len(),
        wire::MAX_NODES_PER_TREE - 1,
        "the root spends one node of the budget, so a full tree is the root plus MAX-1 children",
    );
    assert_eq!(
        ids[0], "n0",
        "what survives is the PREFIX, in traversal order"
    );
    assert_eq!(
        ids[ids.len() - 1],
        format!("n{}", wire::MAX_NODES_PER_TREE - 2),
        "…up to the last node the budget paid for",
    );
    let dropped = format!("n{}", wire::MAX_NODES_PER_TREE - 1);
    assert!(
        !ids.contains(&dropped),
        "…and the node past the cap is gone, not renumbered or substituted",
    );
    assert_eq!(
        preem_render::node_cap_warnings() - base,
        1,
        "an over-cap tree is one journal line",
    );

    let _ = to_ui_node(&scope, &over);
    let _ = to_ui_node(&scope, &over);
    assert_eq!(
        preem_render::node_cap_warnings() - base,
        1,
        "three frames of a tree that is over the cap on every one of them are ONE warning — \
         at 20 Hz a per-frame line would be twenty a second",
    );
}

/// The off-by-one guard for [`wire::MAX_NODES_PER_TREE`]: a tree of *exactly*
/// the cap maps whole and says nothing.
///
/// A cap that fires one node early would truncate a legal tree and log a
/// diagnostic about a plugin that did nothing wrong — and the truncation would
/// be invisible in the over-cap test above, which cannot tell "dropped the node
/// past the cap" from "dropped the last two".
#[test]
fn a_tree_exactly_at_the_node_cap_is_not_truncated() {
    let _ink = preem_ink_lock();
    let base = preem_render::node_cap_warnings();
    let scope = Scope::detached("node-cap-exact");

    // Root + (MAX - 1) children = exactly MAX nodes.
    let exact = label_tree(wire::MAX_NODES_PER_TREE - 1);
    let ids = mapped_label_ids(&scope, &exact);
    assert_eq!(
        ids.len(),
        wire::MAX_NODES_PER_TREE - 1,
        "every child of a tree exactly at the cap is mapped",
    );
    assert_eq!(
        ids[ids.len() - 1],
        format!("n{}", wire::MAX_NODES_PER_TREE - 2),
        "…including the very last one, which is the node the off-by-one would eat",
    );
    assert_eq!(
        preem_render::node_cap_warnings() - base,
        0,
        "exactly at the cap is not over it, so there is nothing to say",
    );
}

/// **#919 review F1's P4.** One wire frame, mapped once per monitor, must map
/// the same — including at the instance cap.
///
/// A scope sitting at exactly `MAX_PREEM_NODES_PER_TREE` instances is handed a
/// frame that swaps one node out for a newcomer, still exactly the cap's worth
/// of nodes. `region.rs` maps that frame once per monitor, and `end_pass` sweeps
/// between the two passes, so a cap charged against the *carried-over* instance
/// count answers "refused" for the first monitor and "admitted" for the second:
/// two screens, one frame, different pixels — the thing this module's
/// idempotence rule exists to forbid.
///
/// Charging against the nodes the pass has admitted makes the verdict a
/// function of the tree, so both passes agree and neither blanks anything.
#[test]
fn the_instance_cap_answers_the_same_for_every_monitor_pass() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("cap-two-monitors");

    let ids: Vec<String> = (0..wire::MAX_PREEM_NODES_PER_TREE)
        .map(|i| format!("g{i}"))
        .collect();
    let full = gauge_row(ids.iter().map(|id| (Some(id.as_str()), 0.5)));
    let _ = to_ui_node(&scope, &full);
    assert_eq!(
        preem_render::instance_count(&scope),
        wire::MAX_PREEM_NODES_PER_TREE,
        "the fixture must really start with the scope full, or it proves nothing",
    );

    // Drop the first node, append a newcomer: still exactly the cap's worth.
    let swapped: Vec<String> = ids[1..]
        .iter()
        .cloned()
        .chain(std::iter::once("gNEW".to_owned()))
        .collect();
    let frame = gauge_row(swapped.iter().map(|id| (Some(id.as_str()), 0.5)));

    let monitor1 = mapped_row_pixels(&scope, &frame);
    let monitor2 = mapped_row_pixels(&scope, &frame);
    assert_eq!(
        monitor1, monitor2,
        "two monitors map ONE wire frame, so they must map it identically — the cap may not \
         answer differently on the second pass because the first pass's sweep freed a slot",
    );
    assert_ne!(
        monitor1[wire::MAX_PREEM_NODES_PER_TREE - 1],
        (0, 0, Vec::new()),
        "…and neither pass may blank a node in a tree that is AT the cap and never over it",
    );

    preem_render::forget_scope(&scope);
}

/// **#919 review F1's P5.** A tree pinned at exactly the cap whose last node's
/// id changes every frame renders every node, every frame, for ever.
///
/// This is the shape a plugin reaches by keying a node on something that moves —
/// a track id, a unit name, a timestamp. Against the carried-over instance count
/// it blanked that node on every other frame indefinitely, at a 50 % duty cycle,
/// without the tree ever being over the cap: the newcomer was refused while the
/// departing node still held its slot, `end_pass` then freed it, and the cycle
/// repeated. Four frames is two full cycles of that.
#[test]
fn a_tree_at_the_instance_cap_that_rotates_one_id_never_blanks() {
    let _ink = preem_ink_lock();
    let base = preem_render::instance_cap_warnings();
    let scope = Scope::detached("cap-rotating-id");

    let stable: Vec<String> = (0..wire::MAX_PREEM_NODES_PER_TREE - 1)
        .map(|i| format!("g{i}"))
        .collect();
    for frame in 0..4 {
        let rotating = format!("r{frame}");
        let ids: Vec<&str> = stable
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(rotating.as_str()))
            .collect();
        let row = gauge_row(ids.into_iter().map(|id| (Some(id), 0.5)));
        let mapped = mapped_row_pixels(&scope, &row);
        assert_eq!(
            mapped.len(),
            wire::MAX_PREEM_NODES_PER_TREE,
            "frame {frame}: every node still maps to a surface",
        );
        assert_ne!(
            mapped[wire::MAX_PREEM_NODES_PER_TREE - 1],
            (0, 0, Vec::new()),
            "frame {frame}: a tree of exactly the cap's worth of nodes is never over the cap, \
             however often its last id changes",
        );
    }
    assert_eq!(
        preem_render::instance_cap_warnings() - base,
        0,
        "…and a tree that is never over the cap has nothing said about it",
    );

    preem_render::forget_scope(&scope);
}

/// A chain of `depth` nested single-child `Box`es, ids `d0` (outermost) …
/// `d{depth-1}` (innermost) — the shape that turns nesting straight into
/// `map_node` stack frames.
fn box_chain(depth: usize) -> wire::Node {
    let level = |id: usize, children: Vec<wire::Node>| wire::Node::Box {
        id: Some(format!("d{id}")),
        dir: wire::Dir::Vertical,
        spacing: 0,
        scroll: false,
        classes: vec![],
        children,
    };
    let mut node = level(depth - 1, vec![]);
    for id in (0..depth - 1).rev() {
        node = level(id, vec![node]);
    }
    node
}

/// The same chain built from `Button`s, whose child is **mandatory** — so a
/// level the walk refuses takes every ancestor down with it.
fn button_chain(depth: usize) -> wire::Node {
    let mut node = wire::Node::Label {
        id: Some("leaf".into()),
        text: String::new(),
        classes: vec![],
    };
    for id in (0..depth).rev() {
        node = wire::Node::Button {
            id: format!("b{id}"),
            classes: vec![],
            child: Box::new(node),
        };
    }
    node
}

/// How many nested `Box`es a mapped [`box_chain`] actually has.
fn mapped_chain_depth(node: &UiNode) -> usize {
    let mut depth = 0;
    let mut cursor = node;
    loop {
        let UiNode::Box { children, .. } = cursor else {
            return depth;
        };
        depth += 1;
        match children.first() {
            Some(child) => cursor = child,
            None => return depth,
        }
    }
}

/// **#919 review F2's acceptance test.** A tree nested one level past
/// [`wire::MAX_TREE_DEPTH`] is walked to the cap and no further, with one
/// warning per tree.
///
/// The node cap does not stand in for this: 65 nodes is four thousand under it,
/// and a chain that *is* at the node cap is 4096 `map_node` frames — measured at
/// ~6.5 KiB each in a debug build, which overflows the main thread's 8 MiB at
/// roughly a third of the node cap. So this is the cap that makes the walk's
/// stack use bounded rather than merely finite.
#[test]
fn a_tree_deeper_than_the_depth_cap_is_walked_to_the_cap_and_warns_once() {
    let _ink = preem_ink_lock();
    let depth_base = preem_render::depth_cap_warnings();
    let node_base = preem_render::node_cap_warnings();
    let scope = Scope::detached("depth-cap-over");

    let over = box_chain(wire::MAX_TREE_DEPTH + 1);
    let mapped = to_ui_node(&scope, &over);
    assert_eq!(
        mapped_chain_depth(&mapped),
        wire::MAX_TREE_DEPTH,
        "the walk descends exactly to the cap and stops",
    );
    assert_eq!(
        preem_render::depth_cap_warnings() - depth_base,
        1,
        "an over-deep tree is one journal line",
    );
    assert_eq!(
        preem_render::node_cap_warnings() - node_base,
        0,
        "…and it is the DEPTH line: 65 nodes is nowhere near the node cap, so a single \
         merged diagnostic would have told the author to send fewer nodes",
    );

    let _ = to_ui_node(&scope, &over);
    let _ = to_ui_node(&scope, &over);
    assert_eq!(
        preem_render::depth_cap_warnings() - depth_base,
        1,
        "three frames of a tree that is over the cap on every one of them are ONE warning",
    );

    // A chain of *mandatory* children collapses whole rather than truncating,
    // and takes the root with it — which is the only way `to_ui_node`'s
    // `unwrap_or(Spacer)` is reachable at all. Its own scope, so the latch above
    // does not hide its line.
    let buttons = Scope::detached("depth-cap-buttons");
    let mapped = to_ui_node(&buttons, &button_chain(wire::MAX_TREE_DEPTH + 1));
    assert!(
        matches!(mapped, UiNode::Spacer),
        "a Button's child is not optional, so the refused level takes every ancestor down \
         with it and the root itself comes back empty, got {mapped:?}",
    );
    assert_eq!(
        preem_render::depth_cap_warnings() - depth_base,
        2,
        "…and that tree gets told too",
    );

    preem_render::forget_scope(&scope);
    preem_render::forget_scope(&buttons);
}

/// The off-by-one guard for [`wire::MAX_TREE_DEPTH`]: a tree nested to *exactly*
/// the cap is walked whole and says nothing.
///
/// A cap that fired one level early would silently drop the innermost widget of
/// a legal layout and log about a plugin that did nothing wrong — and the
/// over-cap test above cannot tell "stopped at the cap" from "stopped one short".
#[test]
fn a_tree_exactly_at_the_depth_cap_is_walked_whole() {
    let _ink = preem_ink_lock();
    let base = preem_render::depth_cap_warnings();
    let scope = Scope::detached("depth-cap-exact");

    let exact = box_chain(wire::MAX_TREE_DEPTH);
    let mapped = to_ui_node(&scope, &exact);
    assert_eq!(
        mapped_chain_depth(&mapped),
        wire::MAX_TREE_DEPTH,
        "every level of a tree exactly at the cap is mapped, innermost included",
    );
    assert_eq!(
        preem_render::depth_cap_warnings() - base,
        0,
        "exactly at the cap is not past it, so there is nothing to say",
    );
}

// ── #918: two preem nodes sharing an id ──────────────────────────────────────

/// **#918's acceptance test.** Two gauges in one tree claiming the same `id`
/// collapse onto one renderer instance — and now say so, once per tree.
///
/// The collapse itself is *pinned, not fixed*: the last node rendered wins, so
/// nothing disappears. Refusing the second node would trade a widget that
/// jitters for a widget that is missing, which is the worse failure and not what
/// the issue asks for.
///
/// The control pair in a second scope is what makes the equality assertion mean
/// something: two *distinct* ids with the same two targets, advanced the same
/// four ticks, render **differently**. Without it "the two frames are equal"
/// would also pass for two separate instances whose needles simply had not moved
/// yet.
#[test]
fn two_preem_nodes_sharing_an_id_collapse_onto_one_instance_and_warn_once() {
    let _ink = preem_ink_lock();
    let base = preem_render::duplicate_id_warnings();
    let shared = Scope::detached("duplicate-id");
    let distinct = Scope::detached("duplicate-id-control");

    let clash = gauge_row([(Some("g"), 0.15), (Some("g"), 0.85)]);
    let control = gauge_row([(Some("a"), 0.15), (Some("b"), 0.85)]);
    let _ = to_ui_node(&shared, &clash);
    let _ = to_ui_node(&distinct, &control);
    assert_eq!(
        preem_render::duplicate_id_warnings() - base,
        1,
        "the pair sharing an id warns once; the control pair does not warn at all",
    );
    assert_eq!(
        preem_render::instance_count(&shared),
        1,
        "two nodes, ONE renderer instance — the hazard being diagnosed",
    );
    assert_eq!(
        preem_render::instance_count(&distinct),
        2,
        "…where two distinct ids get one each",
    );

    // Let the needles move, or "the two frames are equal" is vacuous.
    for _ in 0..4 {
        assert!(advanced(preem_render::ANIM_STEP_SECS));
    }
    let both = mapped_row_pixels(&shared, &clash);
    let apart = mapped_row_pixels(&distinct, &control);
    assert_ne!(
        apart[0], apart[1],
        "the fixture must actually separate two gauges heading for 0.15 and 0.85",
    );
    assert_eq!(
        both[0], both[1],
        "…so two nodes rendering the SAME frame is the collapse: one instance, one needle, \
         dragged between both targets every pass",
    );

    let _ = to_ui_node(&shared, &clash);
    let _ = to_ui_node(&shared, &clash);
    assert_eq!(
        preem_render::duplicate_id_warnings() - base,
        1,
        "and however many frames the tree renders, it is one journal line",
    );

    preem_render::forget_scope(&shared);
    preem_render::forget_scope(&distinct);
}

/// A tree whose preem ids are all distinct never trips #918 — including across
/// **frames** (a second mapping pass re-touches every key, which is the
/// multi-monitor path and must not read as a duplicate) and across **trees** (a
/// plugin's chip and its drawer panel are two scopes, so the same `"cpu"` in
/// both is fine — the namespace to be unique in is the tree).
#[test]
fn a_tree_of_distinct_preem_ids_never_warns() {
    let _ink = preem_ink_lock();
    let dup_base = preem_render::duplicate_id_warnings();
    let anon_base = preem_render::anonymous_warnings();
    let card = Scope::detached("distinct-ids-card");
    let panel = Scope::detached("distinct-ids-panel");

    let mixed = wire::Node::Box {
        id: Some("row".into()),
        dir: wire::Dir::Horizontal,
        spacing: 0,
        scroll: false,
        classes: vec![],
        children: vec![
            preem_node(
                Some("cpu"),
                vocab::PreemWidget::Gauge {
                    config: vocab::GaugeConfig::default(),
                    state: vocab::GaugeState { target: 0.4 },
                },
            ),
            preem_node(
                Some("net"),
                vocab::PreemWidget::DotMatrix {
                    config: vocab::DotMatrixConfig::default(),
                    state: vocab::DotMatrixState { text: "NET".into() },
                },
            ),
            preem_node(
                Some("clock"),
                vocab::PreemWidget::SevenSeg {
                    config: vocab::SevenSegConfig::default(),
                    state: vocab::SevenSegState {
                        text: "12:34".into(),
                    },
                },
            ),
        ],
    };

    let _ = to_ui_node(&card, &mixed);
    // A second pass over the same tree — what a second monitor does.
    let _ = to_ui_node(&card, &mixed);
    // The same ids in the plugin's *other* tree.
    let _ = to_ui_node(&panel, &mixed);

    assert_eq!(
        preem_render::instance_count(&card),
        3,
        "three distinct ids, three instances",
    );
    assert_eq!(
        preem_render::duplicate_id_warnings() - dup_base,
        0,
        "no id is claimed twice in any one pass, so nothing to warn about — not across \
         frames, and not across the plugin's two trees",
    );
    assert_eq!(
        preem_render::anonymous_warnings() - anon_base,
        0,
        "…and every node is id'd, so #900's latch stays untouched too",
    );

    preem_render::forget_scope(&card);
    preem_render::forget_scope(&panel);
}

/// The one-shot diagnostics are keyed by `(Scope, Warned)`, so claiming one does
/// not silence another in the same tree: an anonymous node beside a duplicate
/// pair produces **both** lines, each once.
///
/// A single per-scope flag would pass every other test in this file and lose one
/// of the two diagnostics here — the reason [`preem_render`]'s latch is a set of
/// `(Scope, Warned)` and not a `bool`.
#[test]
fn the_anonymous_and_duplicate_preem_warnings_are_independent() {
    let _ink = preem_ink_lock();
    let anon_base = preem_render::anonymous_warnings();
    let dup_base = preem_render::duplicate_id_warnings();
    let scope = Scope::detached("anon-beside-duplicate");

    // One anonymous node, then a pair sharing "g": both defects, one tree.
    let tree = gauge_row([(None, 0.2), (Some("g"), 0.4), (Some("g"), 0.6)]);
    let _ = to_ui_node(&scope, &tree);
    assert_eq!(
        preem_render::anonymous_warnings() - anon_base,
        1,
        "the anonymous node warns…",
    );
    assert_eq!(
        preem_render::duplicate_id_warnings() - dup_base,
        1,
        "…and so does the duplicate pair, in the very same pass",
    );
    assert_eq!(
        preem_render::instance_count(&scope),
        2,
        "three nodes, two instances: the anonymous one at its ordinal slot, and the shared \"g\"",
    );

    for _ in 0..2 {
        let _ = to_ui_node(&scope, &tree);
    }
    assert_eq!(
        preem_render::anonymous_warnings() - anon_base,
        1,
        "each stays latched on its own key across frames…",
    );
    assert_eq!(
        preem_render::duplicate_id_warnings() - dup_base,
        1,
        "…independently of the other",
    );

    preem_render::forget_scope(&scope);
}

/// A widget kind this build cannot render degrades to a nothing-rendered
/// surface that keeps its id and classes — the same posture the malformed-
/// `Pixels` seam takes — and recovers in place once it becomes renderable.
///
/// `build`'s match is exhaustive over today's vocabulary, so this path is
/// unreachable as the code stands; the test forces it through the seam that
/// stands in for a future `PreemWidget` variant this build predates.
#[test]
fn an_unrenderable_preem_widget_degrades_to_an_empty_surface() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("unsupported");
    let node = preem_node(
        Some("x"),
        vocab::PreemWidget::DotMatrix {
            config: vocab::DotMatrixConfig::default(),
            state: vocab::DotMatrixState { text: "hi".into() },
        },
    );

    let degraded = preem_render::with_unsupported_widgets(|| to_ui_node(&scope, &node));
    assert_eq!(
        degraded,
        UiNode::Pixels {
            id: Some("x".into()),
            width: 0,
            height: 0,
            data: vec![],
            scale: 1,
            classes: vec!["ts-preem".into()],
        },
        "an unrenderable widget keeps its id and classes so a later frame updates in place",
    );

    // The instance is kept (so the warn stays latched at one) but rebuilds the
    // moment the widget becomes renderable again.
    let (_, _, data) = mapped_pixels(&scope, &node);
    assert!(
        !data.is_empty(),
        "the same node recovers in place once its kind is renderable",
    );
}

/// A pure widget never asks the animation clock for anything; a scrolling
/// marquee does, and stops once its speed is parked.
///
/// This is the gate that keeps the 20 Hz timer free for every session that has
/// no animated preem widget on screen.
#[test]
fn only_animated_widgets_keep_the_clock_awake() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("animates");
    let marquee = |speed: f32| {
        preem_node(
            Some("mq"),
            vocab::PreemWidget::Marquee {
                config: vocab::MarqueeConfig {
                    style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                    window_px: 192,
                    gap_dots: 6,
                    speed_dots_per_sec: speed,
                },
                state: vocab::MarqueeState {
                    text: "A LONG SCROLLING MESSAGE".into(),
                },
            },
        )
    };

    let pure = preem_node(
        Some("dm"),
        vocab::PreemWidget::DotMatrix {
            config: vocab::DotMatrixConfig::default(),
            state: vocab::DotMatrixState {
                text: "STATIC".into(),
            },
        },
    );
    let _ = to_ui_node(&scope, &pure);
    assert!(
        !preem_render::any_animating(),
        "a static dot matrix must not keep the animation clock awake",
    );

    let _ = to_ui_node(&scope, &marquee(20.0));
    assert!(
        preem_render::any_animating(),
        "a scrolling marquee is what the clock exists for",
    );

    // `0.0` (and, per the vocabulary, a non-finite value) parks the message.
    let _ = to_ui_node(&scope, &marquee(0.0));
    assert!(
        !preem_render::any_animating(),
        "a parked speed stops asking for ticks",
    );
    assert!(
        !advanced(1.0),
        "and advancing a parked marquee reports no movement",
    );
}

/// The step-based kit primitives are driven off **elapsed time**, and a stall
/// can't make one replay the whole gap.
///
/// `PeakHold::decay` takes no `dt` — it is one fixed fall per call — so the
/// shell converts real seconds into whole steps. Three steps' worth of `dt`
/// must be exactly three decays, and a `dt` worth hundreds must be capped.
#[test]
fn step_based_animation_is_anchored_to_elapsed_time_and_capped() {
    let _ink = preem_ink_lock();
    let strip = kit::LedStrip::new(kit::DisplayStyle::Vfd).leds(16);
    let node = preem_node(
        Some("vu"),
        vocab::PreemWidget::LedStrip {
            config: vocab::LedStripConfig {
                style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                leds: 16,
                peak_hold: Some(vocab::PeakHoldConfig { rate: 0.05 }),
            },
            state: vocab::LedStripState {
                level: 1.0,
                peak: None,
            },
        },
    );

    let three = Scope::detached("steps-three");
    let _ = to_ui_node(&three, &node);
    assert!(advanced(preem_render::ANIM_STEP_SECS * 3.0));
    let mut oracle = kit::PeakHold::new(0.05);
    oracle.push(1.0);
    for _ in 0..3 {
        oracle.decay();
    }
    assert_eq!(
        mapped_pixels(&three, &node),
        kit_pixels(&strip.render(1.0, oracle.value())),
        "three animation steps' worth of dt is exactly three decays",
    );

    let stalled = Scope::detached("steps-stalled");
    let _ = to_ui_node(&stalled, &node);
    // A resume-from-suspend sized `dt`: hundreds of steps' worth.
    assert!(advanced(30.0));
    let mut capped = kit::PeakHold::new(0.05);
    capped.push(1.0);
    for _ in 0..preem_render::MAX_CATCHUP_STEPS {
        capped.decay();
    }
    assert!(
        capped.value() > 0.0,
        "the fixture must not decay to zero at the cap, or the assertion below is vacuous",
    );
    assert_eq!(
        mapped_pixels(&stalled, &node),
        kit_pixels(&strip.render(1.0, capped.value())),
        "a stall's worth of dt is capped instead of replayed step by step",
    );
}

/// A repaint request must actually wake a mount region's subscribers.
///
/// `Mutable`'s write guard only arms its wake-on-drop once something has gone
/// through `DerefMut`, so a `lock_mut()` that is merely taken and dropped
/// notifies nobody and the animation would advance invisibly. An empty mailbox
/// is skipped (there is nothing on screen to repaint).
#[test]
fn a_repaint_request_wakes_the_regions_subscribers() {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    let (tx, _rx) = mpsc::channel::<HostMsg>(4);
    let mailbox: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
    let mut cx = Context::from_waker(Waker::noop());
    let mut signal = pin!(mailbox.signal_cloned());

    // Drain the replayed initial value, then confirm the signal is quiet.
    assert!(matches!(
        signal.as_mut().poll_change(&mut cx),
        Poll::Ready(Some(_))
    ));
    assert!(matches!(
        signal.as_mut().poll_change(&mut cx),
        Poll::Pending
    ));

    request_remap(&mailbox);
    assert!(
        matches!(signal.as_mut().poll_change(&mut cx), Poll::Pending),
        "an empty mailbox has nothing on screen, so it is not woken",
    );

    mailbox.lock_mut().push(render_of("p", 0, 1, "chip", &tx));
    assert!(matches!(
        signal.as_mut().poll_change(&mut cx),
        Poll::Ready(Some(_))
    ));
    assert!(matches!(
        signal.as_mut().poll_change(&mut cx),
        Poll::Pending
    ));

    request_remap(&mailbox);
    assert!(
        matches!(signal.as_mut().poll_change(&mut cx), Poll::Ready(Some(_))),
        "a repaint request must wake the region even though the trees are unchanged",
    );
}

/// Acceptance criterion 2: `HostMsg::Hello` is sent **iff** the manifest
/// declares `vocab_max`.
///
/// A negotiating plugin gets the advertisement as the very first host frame —
/// which is what lets it emit `Node::Preem` at all.
#[tokio::test]
async fn a_negotiating_plugin_is_told_the_hosts_vocabulary_first() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);

    let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host_end, &ctx).await });
    let (mut prd, mut pwr) = plugin_end.into_split();

    let mut manifest = Manifest::new("negotiator", Mount::BarCenter);
    manifest.subscribes.push(StateKey::SlotVisible);
    assert!(
        manifest.negotiates_vocab(),
        "`Manifest::new` stamps `vocab_max`, so this fixture must negotiate",
    );
    write_frame(&mut pwr, &PluginMsg::Register { manifest })
        .await
        .expect("send Register");

    let first = read_frame::<HostMsg, _>(&mut prd)
        .await
        .expect("the host sends a frame");
    assert!(
        matches!(first, HostMsg::Hello { vocab } if vocab == VOCAB),
        "the advertisement must be the FIRST frame after an accepted Register, got {first:?}",
    );
}

/// Acceptance criterion 2, the half that matters: a **pre-#882** plugin — one
/// whose manifest carries no `vocab_max` — must receive **no** `Hello` at all.
///
/// Its `rmp-serde` cannot decode the variant, so a `Hello` would fail the
/// decode, close the session, and let `Restart=on-failure` redial into the #437
/// crash-loop, on every deployed plugin at once. The fixture is byte-for-byte a
/// negotiating manifest except for that one field, and it subscribes
/// `SlotVisible` on a bar mount so the host is guaranteed to send *something* —
/// making "no Hello" an assertion about the frame that did arrive rather than
/// about silence.
///
/// **What it does not prove**, despite the name: the fixture is a *current*
/// `Manifest` with the flag off, not one decoded from a pre-#882 encoder's
/// frame. It pins the gate (deleting it turns this red — see the PR's
/// falsification record) but not that a real old binary's `Register` decodes to
/// `vocab_max: None` rather than failing outright. That is live-verify.
#[tokio::test]
async fn a_legacy_plugin_is_never_sent_the_vocabulary_advertisement() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);

    let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host_end, &ctx).await });
    let (mut prd, mut pwr) = plugin_end.into_split();

    let mut manifest = Manifest::new("legacy", Mount::BarCenter);
    manifest.subscribes.push(StateKey::SlotVisible);
    // What a binary built before #882 sends: the field simply isn't on the wire.
    manifest.vocab_max = None;
    assert!(
        !manifest.negotiates_vocab(),
        "the fixture must be a non-negotiating manifest",
    );
    write_frame(&mut pwr, &PluginMsg::Register { manifest })
        .await
        .expect("send Register");

    let first = read_frame::<HostMsg, _>(&mut prd)
        .await
        .expect("the host sends a frame");
    assert!(
        matches!(first, HostMsg::SlotVisibility { visible: true }),
        "a legacy plugin's first frame must be the state it subscribed to, never a Hello — got \
         {first:?}",
    );
}

// ── #883 review round: the repaint economy the epic exists to buy ────────────

/// How many mapping passes a widget takes to stop doing work, simulating the
/// real feedback loop the animation clock closes: advance, then re-map the
/// **same** wire node (which is what a repaint request makes every monitor's
/// reconciler do).
///
/// Returns the instance's `(builds, applies)` after `ticks` rounds. A widget
/// whose idempotence gate works settles — the counts stop climbing. A widget
/// that defeats it climbs one per tick, forever.
fn pump_rounds(scope: &Scope, node: &wire::Node, ticks: u32) -> (u32, u32) {
    for _ in 0..ticks {
        let _ = advanced(preem_render::ANIM_STEP_SECS);
        let _ = to_ui_node(scope, node);
    }
    preem_render::probe(scope, Some("w")).expect("the node keeps its instance")
}

/// A non-finite float in a widget's **state** must not defeat the idempotence
/// gate.
///
/// `PreemWidget` derives `PartialEq` and IEEE `NaN != NaN`, so a widget carrying
/// one is never equal to itself. With a bare `==` in `apply` the short-circuit
/// never fires: a `Scope` re-arms `pending` and zeroes `idle` on every mapping
/// pass, `animates()` never goes false, and the clock re-maps and re-rasterises
/// a six-figure buffer at 20 Hz for as long as the plugin keeps sending that
/// frame. One `sum / count` with `count == 0` — the shape of every meter — is
/// enough. It also makes #897's "park the clock when nothing animates"
/// unreachable.
///
/// The control is the same widget with finite samples: it must settle, or the
/// assertion below could pass for the wrong reason.
///
/// The boundary scrub for this lands proto-side (`clamp_in_place`, on
/// `fix/preem-clamp-non-finite`) so every consumer of the vocabulary gets it.
/// This test feeds the renderer **directly**, bypassing the clamp, because the
/// host must stay stable even if an unsanitised widget ever reaches it.
#[test]
fn a_non_finite_sample_does_not_pin_the_animation_clock() {
    const TICKS: u32 = 200;

    let _ink = preem_ink_lock();
    let scoped = |samples: Vec<f32>| {
        preem_node(
            Some("w"),
            vocab::PreemWidget::Scope {
                config: vocab::ScopeConfig::default(),
                state: vocab::ScopeState { samples },
            },
        )
    };

    let control = Scope::detached("nan-state-control");
    let (_, finite_applies) = pump_rounds(&control, &scoped(vec![0.25; 32]), TICKS);
    assert!(
        finite_applies < TICKS,
        "the control must settle, or this test proves nothing — got {finite_applies} applies \
         over {TICKS} ticks",
    );

    let poisoned = Scope::detached("nan-state");
    let mut samples = vec![0.25; 32];
    samples[7] = f32::NAN;
    let (_, applies) = pump_rounds(&poisoned, &scoped(samples), TICKS);
    assert!(
        applies < TICKS,
        "a NaN sample must still settle like the finite control ({finite_applies} applies); \
         got {applies} over {TICKS} ticks, i.e. one per tick forever",
    );
    assert!(
        !preem_render::any_animating(),
        "and it must stop asking the animation clock for ticks",
    );
}

/// A non-finite float in a widget's **config** must not rebuild the renderer on
/// every pass.
///
/// `same_config` is the "update in place vs rebuild" predicate; a `NaN` in one
/// of `GaugeConfig`'s four floats makes a config unequal to itself, so the
/// needle returns to rest and a fresh kit object is allocated on every pass —
/// 20× a second, per monitor, and the widget can never animate at all.
///
/// The **state has to move** for this to bite, which is what a real plugin does:
/// with an unchanging widget `apply`'s own short-circuit answers first and
/// `same_config` is never consulted. So each pass here carries a new target, the
/// way a live gauge would.
#[test]
fn a_non_finite_config_float_does_not_rebuild_every_pass() {
    let _ink = preem_ink_lock();
    let gauge = |damping: f32, target: f32| {
        preem_node(
            Some("w"),
            vocab::PreemWidget::Gauge {
                config: vocab::GaugeConfig {
                    damping,
                    ..vocab::GaugeConfig::default()
                },
                state: vocab::GaugeState { target },
            },
        )
    };
    let targets = [0.1_f32, 0.2, 0.3, 0.4, 0.5];

    // The control: a finite config, a moving target. One build, N applies.
    let control = Scope::detached("nan-config-control");
    for target in targets {
        let _ = to_ui_node(&control, &gauge(0.7, target));
    }
    assert_eq!(
        preem_render::probe(&control, Some("w")),
        Some((1, targets.len().try_into().expect("fits u32"))),
        "the control must build once and update per target, or this test proves nothing",
    );

    // The same, with a NaN in the config: it must behave identically.
    let scope = Scope::detached("nan-config");
    for target in targets {
        let _ = to_ui_node(&scope, &gauge(f32::NAN, target));
    }
    assert_eq!(
        preem_render::probe(&scope, Some("w")).map(|(builds, _)| builds),
        Some(1),
        "a moving gauge must build its renderer exactly once whatever its config's floats — \
         a rebuild per pass rests the needle every frame, so it never animates",
    );

    // A genuine config change must still rebuild — the tolerance above must not
    // have been bought by making every config compare equal.
    let _ = to_ui_node(&scope, &gauge(0.9, 0.5));
    assert_eq!(
        preem_render::probe(&scope, Some("w")).map(|(builds, _)| builds),
        Some(2),
        "a real config change must still rebuild",
    );
}

/// An explicit peak masks the shell-held one at render time, so a decaying hold
/// must not report movement while one is set.
///
/// The vocabulary blesses sending both ("the explicit peak wins for the render
/// it arrives on and never disturbs `hold`"), so this is a supported
/// configuration — and before the fix it fanned a **pixel-identical** repaint
/// out to every bar mailbox on every monitor, 20× a second, for as long as the
/// plugin sent both.
#[test]
fn a_masked_peak_hold_does_not_ask_for_pixel_identical_repaints() {
    let _ink = preem_ink_lock();
    let strip = |peak: Option<f32>| {
        preem_node(
            Some("vu"),
            vocab::PreemWidget::LedStrip {
                config: vocab::LedStripConfig {
                    style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                    leds: 16,
                    peak_hold: Some(vocab::PeakHoldConfig { rate: 0.05 }),
                },
                state: vocab::LedStripState { level: 1.0, peak },
            },
        )
    };

    let scope = Scope::detached("masked-peak");
    let masked = strip(Some(0.9));
    let before = mapped_pixels(&scope, &masked);
    assert!(
        !preem_render::any_animating(),
        "a hold nothing draws must not keep the animation clock awake",
    );
    assert!(
        !advanced(preem_render::ANIM_STEP_SECS * 4.0),
        "a hold masked by an explicit peak must report no movement",
    );
    assert_eq!(
        mapped_pixels(&scope, &masked),
        before,
        "the fixture must be pixel-identical across the advance, or the assertion above is \
         asserting the wrong thing",
    );

    // Drop the explicit peak and the hold is what gets drawn again: it must
    // resume reporting movement, and it must have kept decaying meanwhile.
    let _ = to_ui_node(&scope, &strip(None));
    assert!(
        preem_render::any_animating(),
        "with no explicit peak the hold is the drawn value, so it animates again",
    );
    assert!(
        advanced(preem_render::ANIM_STEP_SECS),
        "and advancing it now reports movement",
    );
}

/// The phosphor settle bound is derived from the configured persistence, in both
/// directions.
///
/// The kit's decay is `(v * retained) >> 8`, so the steps a full-intensity trail
/// needs to reach black is a function of `retained` — and the constant `64` this
/// replaced was wrong twice over: a `persistence >= 240` trail froze part-way
/// down **permanently** (the bound ran out before the fade did, and `animates()`
/// then went false), while a default `184` trail was long gone after ~17 steps
/// but kept asking for repaints for 64.
#[test]
fn the_phosphor_settle_bound_follows_the_configured_persistence() {
    let _ink = preem_ink_lock();
    let traced = |persistence: u16| {
        preem_node(
            Some("w"),
            vocab::PreemWidget::Scope {
                config: vocab::ScopeConfig {
                    persistence,
                    ..vocab::ScopeConfig::default()
                },
                state: vocab::ScopeState {
                    samples: vec![0.9; 32],
                },
            },
        )
    };

    // The all-off frame: the same tile with no signal ever traced into it —
    // graticule on the field plus the flat axis every empty advance re-stamps,
    // and nothing else lit. Built from the kit directly, so "blank" is an
    // independent notion rather than the code under test grading its own work
    // (#906 R8: `faded != lit` only proved the trail had *moved*, which a trail
    // frozen half-way down also satisfies).
    //
    // Geometry read off the same `ScopeConfig::default()` the fixture builds
    // from, never spelled out: a hardcoded `144 × 48 @ 2` would silently become
    // a size mismatch the day those defaults move, and this assertion would then
    // fail reading like a phosphor regression.
    let all_off = {
        let defaults = vocab::ScopeConfig::default();
        let dim = |value: u32| usize::try_from(value).expect("a wire-capped dimension fits usize");
        let mut off = kit::Scope::with_size(dim(defaults.cols), dim(defaults.rows))
            .scale(dim(defaults.scale))
            .persistence(255);
        off.advance(&[]);
        kit_pixels(&off.render(kit_style(defaults.style.style)))
    };

    // 1. A long phosphor must fade all the way to black rather than freezing.
    let slow = Scope::detached("settle-slow");
    let slow_node = traced(255);
    let _ = to_ui_node(&slow, &slow_node);
    let lit = mapped_pixels(&slow, &slow_node);
    assert_ne!(
        lit, all_off,
        "the debut batch must actually light the tile, or the blankness assertion below is \
         vacuous",
    );
    // 64 steps: where the old constant stopped. The trail must still be moving.
    for _ in 0..64 {
        let _ = advanced(preem_render::ANIM_STEP_SECS);
    }
    assert!(
        preem_render::any_animating(),
        "a persistence-255 trail needs ~255 steps to reach black, so it must still be fading \
         after 64 — the old constant froze it here, permanently",
    );
    // Run it out to exactly one step short of the bound. At `v -> (v*255)>>8`,
    // i.e. `v - 1`, a full-intensity trail needs exactly 255 decays — which is
    // what `scope_settle_steps(255)` computes and therefore what the renderer
    // spends. 64 + 190 = 254 of them.
    for _ in 0..190 {
        let _ = advanced(preem_render::ANIM_STEP_SECS);
    }
    assert_ne!(
        mapped_pixels(&slow, &slow_node),
        all_off,
        "254 decays is one short: the trail must still be on screen, so the step below is \
         doing the work rather than the bound being loose",
    );
    // The 255th decay. Deliberately *not* asserted to have moved: `advanced` is
    // global, and the assertion that matters is where this lands the tile, not
    // that something somewhere reported motion.
    let _ = advanced(preem_render::ANIM_STEP_SECS);
    let faded = mapped_pixels(&slow, &slow_node);
    assert_ne!(
        faded, lit,
        "it must actually have faded, not merely stopped being asked to",
    );
    // …and the strong form, deliberately asserted *after* the weak one so a
    // mutation shows which of the two catches it (#906 R8): a trail frozen
    // part-way down satisfies `faded != lit` perfectly well. Blankness is the
    // property `scope_settle_steps` exists to guarantee.
    assert_eq!(
        faded, all_off,
        "after the bound the tile IS the all-off frame — blank, not merely different from lit",
    );
    assert!(
        !preem_render::any_animating(),
        "once black it must stop asking for ticks",
    );

    // 2. The default fades in ~17 steps and must stop asking soon after — well
    //    inside the 64 the old constant spent on pixel-identical repaints.
    let quick = Scope::detached("settle-quick");
    let quick_node = traced(184);
    let _ = to_ui_node(&quick, &quick_node);
    let mut spent = 0;
    while preem_render::any_animating() && spent < 64 {
        let _ = advanced(preem_render::ANIM_STEP_SECS);
        spent += 1;
    }
    assert!(
        spent < 32,
        "a default-persistence trail is gone in ~17 steps, so it must stop asking well before \
         the old constant's 64 — took {spent}",
    );
}

/// The animation clock's fan-out only wakes the mailboxes that actually hold an
/// advanced plugin's render.
///
/// A blanket nudge re-runs `reconcile_region` over every plugin's whole tree —
/// every wire node re-mapped, every preem instance's cached RGBA frame cloned
/// out of the store — for every plugin, on every monitor, 20× a second, legacy
/// self-rasterising plugins included. Since #907 the *upload* at the end of that
/// is no longer part of the bill: `hytte-ui`'s `PixelSurface::set_pixels` keeps
/// the last accepted buffer and returns without touching GTK when the frame is
/// identical. This narrowing is the second guard, and it is the one that skips
/// the walk rather than paying for it and discarding the result.
#[test]
fn a_repaint_request_skips_mailboxes_holding_no_mover() {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    let (tx, _rx) = mpsc::channel::<HostMsg>(4);
    let mailbox: Mutable<Vec<SlotRender>> =
        Mutable::new(vec![render_of("resident", 0, 1, "c", &tx)]);
    let mut cx = Context::from_waker(Waker::noop());
    let mut signal = pin!(mailbox.signal_cloned());
    assert!(matches!(
        signal.as_mut().poll_change(&mut cx),
        Poll::Ready(Some(_))
    ));
    assert!(matches!(
        signal.as_mut().poll_change(&mut cx),
        Poll::Pending
    ));

    let elsewhere: HashSet<&str> = ["mover"].into_iter().collect();
    request_remap_holding(&mailbox, &elsewhere);
    assert!(
        matches!(signal.as_mut().poll_change(&mut cx), Poll::Pending),
        "a mailbox holding none of the movers must not be woken",
    );

    let here: HashSet<&str> = ["resident"].into_iter().collect();
    request_remap_holding(&mailbox, &here);
    assert!(
        matches!(signal.as_mut().poll_change(&mut cx), Poll::Ready(Some(_))),
        "a mailbox holding a mover must be woken",
    );
}

/// A negative `speed_dots_per_sec` scrolls the **opposite** way to a positive
/// one of the same magnitude, and the positive direction is the kit's.
///
/// This is a two-ended contract with no single owner: `MarqueeStrip::window`
/// takes an *unsigned* offset, so the kit has no signed semantics for the shell
/// to inherit, and the proto documents only that `0.0` and non-finite park the
/// message. The direction lives in whoever integrates the offset — this
/// renderer, and the SDK's raster path (#884/#898) — so if the two ends disagree
/// a plugin's ticker reverses the day the host flips from raster to state.
///
/// Both sides are asserted against the kit rather than against each other:
/// `window` reads source column `(offset + col) % period`, so a rising offset
/// walks the message leftwards ("any monotonically increasing frame counter
/// loops seamlessly"). Half a second at ±20 dots/s is ±10 whole dots, so the
/// negative case must land on `period - 10`.
#[test]
fn marquee_scroll_direction_follows_the_speeds_sign() {
    let _ink = preem_ink_lock();
    let text = "SCROLLING MARQUEE TEST";
    let node = |speed: f32| {
        preem_node(
            Some("mq"),
            vocab::PreemWidget::Marquee {
                config: vocab::MarqueeConfig {
                    style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                    window_px: 192,
                    gap_dots: 6,
                    speed_dots_per_sec: speed,
                },
                state: vocab::MarqueeState { text: text.into() },
            },
        )
    };
    let oracle = kit::Marquee::new(kit::DisplayStyle::Vfd)
        .window_px(192)
        .gap_dots(6)
        .render(text);
    let period = oracle.period();
    assert!(
        oracle.scrolls() && period > 10,
        "the fixture must scroll and be longer than the step, or the two directions coincide",
    );

    let forward = Scope::detached("marquee-forward");
    let _ = to_ui_node(&forward, &node(20.0));
    assert!(advanced(0.5));
    assert_eq!(
        mapped_pixels(&forward, &node(20.0)),
        kit_pixels(&oracle.window(10)),
        "a positive speed raises the offset, which is the kit's own \
         monotonically-increasing-counter direction",
    );

    let backward = Scope::detached("marquee-backward");
    let _ = to_ui_node(&backward, &node(-20.0));
    assert!(advanced(0.5));
    assert_eq!(
        mapped_pixels(&backward, &node(-20.0)),
        kit_pixels(&oracle.window(period - 10)),
        "a negative speed of the same magnitude must scroll the other way, wrapping to \
         `period - 10` rather than parking at zero",
    );
}

/// `advance_all` names the scopes that moved, and only those — the input the
/// targeting above runs on.
#[test]
fn advance_all_names_only_the_scopes_that_moved() {
    let _ink = preem_ink_lock();
    let animated = Scope::card("scroller");
    let still = Scope::card("static");
    let _ = to_ui_node(
        &animated,
        &preem_node(
            Some("mq"),
            vocab::PreemWidget::Marquee {
                config: vocab::MarqueeConfig {
                    style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                    window_px: 192,
                    gap_dots: 6,
                    speed_dots_per_sec: 20.0,
                },
                state: vocab::MarqueeState {
                    text: "A LONG SCROLLING MESSAGE".into(),
                },
            },
        ),
    );
    let _ = to_ui_node(
        &still,
        &preem_node(
            Some("dm"),
            vocab::PreemWidget::DotMatrix {
                config: vocab::DotMatrixConfig::default(),
                state: vocab::DotMatrixState {
                    text: "STATIC".into(),
                },
            },
        ),
    );

    let moved = preem_render::advance_all(0.5);
    assert_eq!(
        moved,
        vec![animated],
        "only the scrolling marquee's scope moved, so only it may be named",
    );
    assert_eq!(
        moved[0].plugin_id(),
        "scroller",
        "and the fan-out must be able to read the plugin id back off it",
    );
}

// ── #897: the per-mount frame-clock tick decision ────────────────────────────

/// A scrolling marquee node in the fixture the tick tests share, at `speed`
/// dots per second.
///
/// One shape for all of them so the kit oracle below describes every case: the
/// only thing that varies between the tests is *when* the ticks arrive.
fn tick_marquee(speed: f32) -> wire::Node {
    preem_node(
        Some("mq"),
        vocab::PreemWidget::Marquee {
            config: vocab::MarqueeConfig {
                style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                window_px: 192,
                gap_dots: 6,
                speed_dots_per_sec: speed,
            },
            state: vocab::MarqueeState {
                text: "A LONG SCROLLING MESSAGE".into(),
            },
        },
    )
}

/// The kit's own strip for [`tick_marquee`], so an offset assertion reads
/// "the strip windowed at N dots" rather than "some pixels".
fn tick_marquee_oracle() -> kit::MarqueeStrip {
    kit::Marquee::new(kit::DisplayStyle::Vfd)
        .window_px(192)
        .gap_dots(6)
        .render("A LONG SCROLLING MESSAGE")
}

/// One animation step in microseconds — the unit the frame-time tests count in.
///
/// Derived from the clamp rather than spelled again, and
/// `the_tick_dt_clamp_is_the_resume_cap` pins the clamp itself against
/// `ANIM_STEP_SECS`, so a change to either constant reaches every test below.
fn step_us() -> i64 {
    preem_render::MAX_TICK_DT_US / i64::from(preem_render::MAX_CATCHUP_STEPS)
}

/// The dt clamp is **`ANIM_STEP_SECS` × `MAX_CATCHUP_STEPS`** — the resume cap,
/// not one step.
///
/// Its own test because the three constants live apart and are spelled in
/// different units (`MAX_TICK_DT_US` is an `i64` of microseconds,
/// `ANIM_STEP_SECS` an `f32` of seconds, `MAX_CATCHUP_STEPS` a `u32` of steps),
/// and because #897's park-resume argument is stated in *steps* while the code
/// clamps in *microseconds*.
///
/// The first cut of #897 clamped at one step, which is why this is worth a test
/// rather than a comment: at one step the clamp stops being a catch-up bound and
/// becomes a **rate** bound, silently slowing every animation under any clock
/// below 20 Hz — the regression
/// `a_frame_clock_slower_than_the_step_rate_still_runs_at_the_right_speed`
/// exists to catch, and which this pins the constant for.
#[test]
fn the_tick_dt_clamp_is_the_resume_cap() {
    assert_eq!(
        preem_render::MAX_CATCHUP_STEPS,
        8,
        "the resume cap must still be the eight steps #897's body asked to keep",
    );
    // With the step count pinned above, this is the cap expressed from the other
    // two constants, in the unit the clamp is spelled in.
    let cap_us = f64::from(preem_render::ANIM_STEP_SECS) * 8.0 * 1_000_000.0;
    assert!(
        (cap_us - 400_000.0).abs() < 1.0,
        "`ANIM_STEP_SECS` × 8 must be 400 ms — it is {cap_us} µs",
    );
    assert_eq!(
        preem_render::MAX_TICK_DT_US,
        400_000,
        "…and the frame-clock dt clamp must be exactly that, not one step: at one step the \
         clamp stops bounding catch-up and starts bounding the *rate*",
    );
    assert_eq!(step_us(), 50_000, "…so one step is 50 ms");
}

/// A tick that finds every widget in its mount settled says **stop**, and a
/// state change on a settled instance says **go** again.
///
/// The two halves of #897's park, and the pair the old timer could not be
/// tested on at all: `install_preem_clock` was armed once and never broke, so
/// there was no decision to assert. `tick_decision` is that decision, lifted out
/// of the GTK closure so it can be driven with no main loop, no display and no
/// registered `PluginHandles`.
///
/// The re-animating edge is deliberately a **state** change on a *live* gauge
/// (a new target for the same config), not a config change that would rebuild
/// the instance: "a state change that gives a settled widget somewhere to go" is
/// the case #897 names as the only way an instance can start animating, and a
/// rebuild would pass even if `any_animating_in` were asking about the wrong
/// scope set.
#[test]
fn a_settled_mount_stops_ticking_and_a_state_change_starts_it_again() {
    let _ink = preem_ink_lock();
    let scope = Scope::card("tick-park");
    let gauge = |target: f32| {
        preem_node(
            Some("g"),
            vocab::PreemWidget::Gauge {
                config: vocab::GaugeConfig::default(),
                state: vocab::GaugeState { target },
            },
        )
    };
    let mine = [scope.clone()];

    let _ = to_ui_node(&scope, &gauge(0.9));
    assert!(
        tick_decision(&mine, 1_000_000).keep_going,
        "a needle heading for a new target must keep its mount's clock armed",
    );

    // Run the spring out. `advance_all` rather than a tick, so the settling is
    // not itself an assertion about the clamp being tested next door.
    let mut spent = 0;
    while preem_render::any_animating() && spent < 4096 {
        let _ = advanced(preem_render::ANIM_STEP_SECS);
        spent += 1;
    }
    assert!(spent < 4096, "the needle must actually settle");

    let settled = tick_decision(&mine, 2_000_000);
    assert!(
        settled.moved.is_empty(),
        "a settled mount's tick must move nothing",
    );
    assert!(
        !settled.keep_going,
        "…and must break the tick callback rather than ask for another frame — this is the \
         park, and there is nothing else in #897 that stops the wakeups",
    );

    // The state change. Same config, new target: `apply`, not `build`.
    let _ = to_ui_node(&scope, &gauge(0.1));
    assert!(
        tick_decision(&mine, 3_000_000).keep_going,
        "a new target on a settled needle must re-arm the mount — this is what the mapping \
         pass's `ensure_armed` is reading, and a mount that never re-arms freezes every preem \
         animation for the session with CI still green",
    );
}

/// A tick only advances the scopes **its own mount** names.
///
/// The narrowing the old global `advance_all` did not do, and the reason a
/// settled bar region can park while an open drawer's gauge still swings.
#[test]
fn a_tick_leaves_scopes_its_mount_does_not_name_alone() {
    let _ink = preem_ink_lock();
    let mine = Scope::card("tick-mine");
    let theirs = Scope::card("tick-theirs");
    let node = tick_marquee(20.0);
    let _ = to_ui_node(&mine, &node);
    let _ = to_ui_node(&theirs, &node);

    let only_mine = [mine.clone()];
    // Baseline tick, then a full step.
    let _ = tick_decision(&only_mine, 0);
    let moved = tick_decision(&only_mine, step_us()).moved;
    assert_eq!(
        moved,
        vec![mine.clone()],
        "only the mount's own scope may be advanced or named",
    );

    let oracle = tick_marquee_oracle();
    assert_eq!(
        mapped_pixels(&mine, &node),
        kit_pixels(&oracle.window(1)),
        "the named scope scrolled its one dot",
    );
    assert_eq!(
        mapped_pixels(&theirs, &node),
        kit_pixels(&oracle.window(0)),
        "…and the other mount's scope did not move at all",
    );
}

/// A tick arriving after a long gap — a resume from suspend, or the first frame
/// after a park — advances at most the **resume cap**, not the gap.
///
/// The `dt` clamp, and the reason `ScopeState::last_advance_us` is deliberately
/// *not* reset when a mount re-arms: the clamp already bounds the parked
/// interval, and not resetting is what keeps a second mount from stomping the
/// baseline of a scope the first one is already driving.
///
/// Five seconds at 20 dots/s is 100 whole dots; the cap is 400 ms, so 8. The
/// fixture's period is asserted to be longer than both so the two cannot alias
/// onto the same window.
#[test]
fn a_tick_after_a_long_gap_advances_the_resume_cap_not_the_gap() {
    let _ink = preem_ink_lock();
    let scope = Scope::card("tick-stall");
    let node = tick_marquee(20.0);
    let _ = to_ui_node(&scope, &node);
    let oracle = tick_marquee_oracle();
    assert!(
        oracle.scrolls() && oracle.period() > 100,
        "the fixture must be longer than the unclamped 100-dot answer, or the clamp is \
         asserted against a wrapped-around alias of itself",
    );
    let mine = [scope.clone()];

    // The first tick of a scope has no baseline: it stamps and advances nothing,
    // which is one dropped frame at the start of a motion and no jump.
    let first = tick_decision(&mine, 10_000_000);
    assert!(
        first.moved.is_empty(),
        "the first tick must only stamp the baseline",
    );
    assert_eq!(
        mapped_pixels(&scope, &node),
        kit_pixels(&oracle.window(0)),
        "…so nothing has scrolled yet",
    );

    // Five seconds later.
    let moved = tick_decision(&mine, 15_000_000).moved;
    assert_eq!(
        moved,
        vec![scope.clone()],
        "the stalled tick still moves it"
    );
    assert_eq!(
        mapped_pixels(&scope, &node),
        kit_pixels(&oracle.window(8)),
        "a five-second gap must be worth the 400 ms resume cap — 8 dots — not the 100 it \
         really spanned: a resume from suspend (or a re-arm minutes after a park) catches up \
         a bounded amount and moves on",
    );
}

/// A frame clock **slower than one animation step** still runs animation at the
/// right speed.
///
/// The regression the #926 review found in #897's first cut, and the reason the
/// clamp is the resume cap rather than one step. A clamp at 50 ms does not only
/// bound catch-up — it truncates *every* frame interval longer than itself, so a
/// sustained sub-20-Hz clock runs everything slow with no diagnostic. Measured
/// then: a 15 Hz clock advanced a 20 dots/s marquee 15 dots in a second instead
/// of 20, a silent 25 % rate error.
///
/// It is not a corner. #897's own cost note says the shell rasterises on the CPU
/// per tick per animating widget, so enough widgets push the frame clock below
/// 20 Hz — and then phosphor fade length, needle settle time and marquee speed
/// all drift together.
///
/// Ten frames of 100 000 µs is **exactly** one second, and every interval is
/// well under the 400 ms cap, so nothing may be truncated. 10 Hz rather than the
/// 15 Hz the review measured only because 15 divides a second into a repeating
/// fraction: 15 × 66 666 µs is 999 990 µs, which floors to 19 dots and would
/// make the assertion argue with `dots()`'s rounding instead of with the clamp.
/// A compositor throttling an occluded surface to 10 Hz is the same case, harder.
#[test]
fn a_frame_clock_slower_than_the_step_rate_still_runs_at_the_right_speed() {
    let _ink = preem_ink_lock();
    let scope = Scope::card("tick-slow-clock");
    let node = tick_marquee(20.0);
    let _ = to_ui_node(&scope, &node);
    let oracle = tick_marquee_oracle();
    assert!(
        oracle.period() > 20,
        "the fixture must be longer than a second's worth of scroll",
    );
    let mine = [scope.clone()];

    // The baseline tick, then ten frames at 10 Hz — each one twice the step.
    let frame_us = 100_000;
    assert!(
        frame_us > step_us() && frame_us < preem_render::MAX_TICK_DT_US,
        "the fixture's frame must be longer than one step (or the old clamp would not have \
         truncated it) and shorter than the resume cap (or the new one would)",
    );
    let _ = tick_decision(&mine, 0);
    for frame in 1..=10 {
        let _ = tick_decision(&mine, frame * frame_us);
    }

    assert_eq!(
        mapped_pixels(&scope, &node),
        kit_pixels(&oracle.window(20)),
        "one second at 20 dots/s is 20 dots at ANY refresh rate: a clamp of one step would \
         have truncated each 100 ms frame to 50 ms and landed on 10 — a sustained slowdown \
         no other test would see, because every frame here is a perfectly ordinary one",
    );
}

/// One scope shown by **two mounts** advances once per unit of real time, not
/// once per mount per frame.
///
/// The double-mount rule. `Scope::card` is keyed by plugin and tree, never by
/// output, so the same chip on two monitors' bars is one set of renderer
/// instances driven by two frame clocks — and a `dt` measured per *mount* would
/// run every animation on a two-monitor desk at 2× speed, which is the exact
/// hazard the old single global timer existed to avoid. `advance_scopes` takes a
/// frame *timestamp* and each scope measures from its own last advance, so the
/// second mount's tick is worth only the real time since the first one's.
///
/// Driven at the nastiest phase rather than the easiest: the two clocks are
/// interleaved half a frame apart, so neither "same `frame_time` twice" nor
/// "whole steps each time" could carry the test.
#[test]
fn two_mounts_showing_one_scope_advance_it_once_per_frame() {
    let _ink = preem_ink_lock();
    let scope = Scope::card("tick-shared");
    let node = tick_marquee(20.0);
    let _ = to_ui_node(&scope, &node);
    let oracle = tick_marquee_oracle();
    // Two mounts — say the same chip in two monitors' bar-left regions — each
    // naming the one shared scope, exactly as `Animator`'s scopes closure would.
    let mount_a = [scope.clone()];
    let mount_b = [scope.clone()];

    // 100 ms of wall clock, five ticks shared between them, alternating 25 ms
    // apart: two 40 Hz-equivalent clocks half a frame out of phase, so no single
    // tick is a whole step and no two ticks share a `frame_time`.
    let step = step_us();
    for frame in 0..5 {
        let mount = if frame % 2 == 0 { &mount_a } else { &mount_b };
        let _ = tick_decision(mount, frame * step / 2);
    }

    assert_eq!(
        mapped_pixels(&scope, &node),
        kit_pixels(&oracle.window(2)),
        "100 ms at 20 dots/s is 2 dots however many mounts are watching: a per-mount `dt` \
         would have advanced it 4 (each tick a full 25 ms from its own mount's last), and a \
         `frame_time`-equality dedup would have advanced it 0 for the second mount and 4 for \
         the first, because two monitors' clocks share a time base but not a phase",
    );

    // …and it is not that motion stopped: the same scope keeps moving on the
    // next pair of ticks.
    let _ = tick_decision(&mount_b, 5 * step / 2);
    let _ = tick_decision(&mount_a, 3 * step);
    assert_eq!(
        mapped_pixels(&scope, &node),
        kit_pixels(&oracle.window(3)),
        "150 ms is 3 dots — the rate is right, not merely slow",
    );
}

/// A tick worth **no elapsed time** reports no motion, for a gauge as well as
/// for a marquee.
///
/// Two ticks carry `dt == 0` on the production path, and both are ordinary:
/// every scope's **first** tick (the `None` branch of `last_advance_us`, which
/// stamps a baseline and nothing else) and the **second mount** of a pair whose
/// clocks happen to be in phase, handing the same `frame_time` twice.
///
/// `LedStrip` and `Marquee` compare before against after and `Scope` counts
/// whole steps, so all three answered correctly already. `Gauge` and `FlipBoard`
/// took the kit's `advance(dt)` — which itself early-returns on a `dt` that
/// cannot move anything — and then reported `true` regardless (#926 review L-2,
/// probes P2/P4). That drops the instance's cached frame and fans a
/// `request_preem_repaint` out for a **byte-identical** frame, on every mount, on
/// every such tick: L-3 measured 6 fan-outs where 3 were owed.
#[test]
fn a_tick_worth_no_elapsed_time_reports_no_motion() {
    let _ink = preem_ink_lock();
    let scope = Scope::card("tick-zero-dt");
    let gauge = preem_node(
        Some("g"),
        vocab::PreemWidget::Gauge {
            config: vocab::GaugeConfig::default(),
            state: vocab::GaugeState { target: 0.9 },
        },
    );
    let _ = to_ui_node(&scope, &gauge);
    let mine = [scope.clone()];
    assert!(
        preem_render::any_animating(),
        "the fixture must be a needle actually heading somewhere, or every assertion below \
         passes for the settled reason instead of the dt one",
    );

    let first = tick_decision(&mine, 7_000_000);
    assert!(
        first.moved.is_empty(),
        "a scope's first tick has no baseline and so advances nothing — it must not report a \
         repaint for a frame it did not change",
    );
    assert!(
        first.keep_going,
        "…and must still ask for the next frame, or a gauge would park on its own first tick",
    );

    // Two mounts, one in-phase `frame_time`: the first advances, the second is
    // worth nothing.
    let moved = tick_decision(&mine, 7_000_000 + step_us()).moved;
    assert_eq!(
        moved,
        vec![scope.clone()],
        "the frame that really elapsed must move the needle",
    );
    assert!(
        tick_decision(&mine, 7_000_000 + step_us()).moved.is_empty(),
        "a second mount handing the same `frame_time` must not fan a second repaint out for \
         the identical frame",
    );
}

// ── #885: per-widget ink — roles, pins, and the live re-tint ─────────────────

/// A dot-matrix node in `style` — the smallest widget whose ink is visible.
fn ink_probe(id: &str, style: vocab::StyleRef) -> wire::Node {
    preem_node(
        Some(id),
        vocab::PreemWidget::DotMatrix {
            config: vocab::DotMatrixConfig { style },
            state: vocab::DotMatrixState { text: "88".into() },
        },
    )
}

/// #396, end to end through the shell's real accent path: changing the desktop
/// accent re-renders every **role-tinted** preem widget on screen, and leaves
/// every **pinned** one exactly as it was.
///
/// `tint_in_process_surfaces` is the shell's own accent seam (`pump`), so this
/// drives the same call the `StyleManager` listener makes — installing the kit
/// accent and dropping the cached frames — rather than a test-only shortcut.
///
/// **Deletion check:** making `ink_for` ignore `StyleRef::ink` (returning
/// `Ink::Default` for a pin) turns the two pinned assertions red while the
/// re-tinting one stays green — so the pin is what they measure, not the
/// invalidation.
#[test]
fn an_accent_change_re_tints_a_role_widget_and_leaves_a_pinned_one_alone() {
    let _ink = preem_ink_lock();
    let violet = [0x9b, 0x59, 0xb6, 0xff];
    let scope = Scope::detached("885-re-tint");
    let role = ink_probe("role", vocab::StyleRef::new(vocab::StyleName::Vfd));
    let pinned = ink_probe(
        "pinned",
        vocab::StyleRef::new(vocab::StyleName::Vfd).with_ink(violet),
    );

    tint_in_process_surfaces(Some([0x11, 0x99, 0xaa, 0xff]));
    let role_teal = mapped_pixels(&scope, &role);
    let pinned_teal = mapped_pixels(&scope, &pinned);

    tint_in_process_surfaces(Some([0xdd, 0x22, 0x66, 0xff]));
    let role_rose = mapped_pixels(&scope, &role);
    let pinned_rose = mapped_pixels(&scope, &pinned);

    tint_in_process_surfaces(None);
    preem_render::forget_scope(&scope);

    assert_ne!(
        role_teal, role_rose,
        "a role-tinted widget must re-render in the new accent with no plugin involvement (#396)",
    );
    assert_eq!(
        pinned_teal, pinned_rose,
        "a pinned widget is deliberately excluded from the re-tint — that is what pinning means",
    );
    assert!(
        pinned_teal.2.chunks_exact(4).any(|px| px == violet),
        "…and it is excluded *at its pinned color*, not merely frozen at whatever it first drew",
    );
    assert_ne!(
        role_teal, pinned_teal,
        "the two must differ under one accent, or the equality above is vacuous",
    );
}

/// A semantic role other than `Accent` resolves to the **theme's** color for
/// that role, not to the accent — the per-widget resolution the kit's one
/// process-global could not express.
///
/// The role colors are injected rather than looked up: the hermetic test binary
/// has no GTK display, so `resolve_role_inks` returns every color unset and each
/// role would (correctly, by its documented fallback) degrade to the accent —
/// proving nothing about resolution.
///
/// **Deletion check:** collapsing every role onto `Ink::Default` in `ink_for`
/// turns the first three assertions red.
#[test]
fn a_status_role_resolves_to_the_theme_color_not_the_accent() {
    let _ink = preem_ink_lock();
    let green = [0x2e, 0xc2, 0x7e, 0xff];
    let amber = [0xe5, 0xa5, 0x0a, 0xff];
    let scope = Scope::detached("885-roles");
    let vfd = vocab::StyleRef::new(vocab::StyleName::Vfd);
    let with = |role| vfd.with_accent(role);

    // Order matters: the tint call clears the memo, so the injection goes last.
    tint_in_process_surfaces(Some([0x11, 0x99, 0xaa, 0xff]));
    preem_render::set_role_inks(preem_render::RoleInks {
        success: Some(green),
        warning: Some(amber),
        error: None,
    });

    let accent = mapped_pixels(
        &scope,
        &ink_probe("accent", with(vocab::AccentRole::Accent)),
    );
    let success = mapped_pixels(&scope, &ink_probe("ok", with(vocab::AccentRole::Success)));
    let warning = mapped_pixels(&scope, &ink_probe("warn", with(vocab::AccentRole::Warning)));
    let error = mapped_pixels(&scope, &ink_probe("err", with(vocab::AccentRole::Error)));
    let neutral = mapped_pixels(
        &scope,
        &ink_probe("plain", with(vocab::AccentRole::Neutral)),
    );

    // The same accent-role widget again, with no accent installed at all: the
    // kit's own hard-coded ink, which is where `Neutral` should have landed.
    tint_in_process_surfaces(None);
    let unaccented = mapped_pixels(
        &scope,
        &ink_probe("accent", with(vocab::AccentRole::Accent)),
    );
    preem_render::forget_scope(&scope);

    assert_ne!(
        success, accent,
        "Success must resolve to @success_color, not to the desktop accent",
    );
    assert_ne!(warning, success, "…and each role to its own color");
    assert!(
        success.2.chunks_exact(4).any(|px| px == green),
        "a fully-lit dot carries the role's color exactly, not something derived from it",
    );
    assert_eq!(
        error, accent,
        "a role this theme does not define falls back to the accent rather than inventing a color",
    );
    assert_ne!(
        neutral, accent,
        "Neutral refuses the accent — the opt-out the wire documents",
    );
    assert_eq!(
        neutral, unaccented,
        "…and lands exactly where the same skin lands with no accent installed at all",
    );
}

/// The pin survives a theme change on the one widget that resolves its palette
/// at **construction** rather than at render — which is the widget
/// [`invalidate_cached_frames`](preem_render) *rebuilds* on that change, and so
/// the one where the ink scope has to cover `build()` and not just
/// `Instance::frame`.
///
/// `an_accent_change_re_tints_a_role_widget_and_leaves_a_pinned_one_alone` above
/// cannot see this: its `DotMatrix` resolves per render, so the frame-side scope
/// alone keeps it green. `TextBox` (and `Marquee`'s strip) is the case that needs
/// the build-side one.
///
/// **Deletion check:** dropping the `kit::with_ink` wrapper in `build()` leaves
/// the whole rest of the shell suite green and turns *this* red at "a pinned
/// `TextBox` must draw its pinned color" — the review probe that found the gap.
#[test]
fn a_pinned_text_box_survives_a_theme_change_though_it_bakes_at_construction() {
    let _ink = preem_ink_lock();
    let violet = [0x9b, 0x59, 0xb6, 0xff];
    let scope = Scope::detached("885-pinned-textbox");
    let node = preem_node(
        Some("tb"),
        vocab::PreemWidget::TextBox {
            config: vocab::TextBoxConfig {
                style: vocab::StyleRef::new(vocab::StyleName::Lcd).with_ink(violet),
                ..vocab::TextBoxConfig::default()
            },
            state: vocab::TextBoxState { text: "pin".into() },
        },
    );

    tint_in_process_surfaces(Some([0x11, 0x99, 0xaa, 0xff]));
    let teal = mapped_pixels(&scope, &node);
    tint_in_process_surfaces(Some([0xdd, 0x22, 0x66, 0xff]));
    let rose = mapped_pixels(&scope, &node);
    tint_in_process_surfaces(None);
    preem_render::forget_scope(&scope);

    assert!(
        teal.2.chunks_exact(4).any(|px| px == violet),
        "a pinned TextBox must draw its pinned color",
    );
    assert_eq!(teal, rose, "…and survive a theme change byte-identically");
}

// ── #885's palette widening: field + notdef (#884's two speech bubbles) ──────

/// A pinned **field** floods the ground the widget draws on, and — like a pinned
/// ink — is deliberately excluded from the live re-tint.
///
/// The ground is where most of a widget's pixels are, so this is the pin that
/// actually made `pet`'s and `caw`'s bubbles migratable: #912's ink-only
/// override would have left both boxes standing on the skin's own field.
///
/// The two pins are **independent**, and the middle assertion says so out loud:
/// a widget that pins only its field still re-tints its *ink* with the desktop,
/// so it is not byte-frozen. Only when both slots are spoken for — here by
/// `Neutral`, which is the ink saying "not even the accent" — is the whole
/// widget still. Getting that wrong in the obvious direction (asserting the
/// field-only widget is frozen) is what this test caught while it was written.
///
/// The unpinned control is what keeps the flooding claim from being vacuous.
///
/// **Deletion check:** making `pins_for` drop `StyleRef::field` (returning
/// `field: None`) leaves the rest of the shell suite green and turns "a pinned
/// field must flood the widget's ground" red.
#[test]
fn a_pinned_field_floods_the_ground_and_survives_a_theme_change() {
    let _ink = preem_ink_lock();
    let lilac = [0x3a, 0x22, 0x50, 0xff];
    let scope = Scope::detached("885-field");
    let vfd = vocab::StyleRef::new(vocab::StyleName::Vfd);
    let pinned = ink_probe("field", vfd.with_field(lilac));
    let plain = ink_probe("plain", vfd);
    let still = ink_probe(
        "still",
        vfd.with_field(lilac)
            .with_accent(vocab::AccentRole::Neutral),
    );

    tint_in_process_surfaces(Some([0x11, 0x99, 0xaa, 0xff]));
    let pinned_teal = mapped_pixels(&scope, &pinned);
    let plain_teal = mapped_pixels(&scope, &plain);
    let still_teal = mapped_pixels(&scope, &still);

    tint_in_process_surfaces(Some([0xdd, 0x22, 0x66, 0xff]));
    let pinned_rose = mapped_pixels(&scope, &pinned);
    let still_rose = mapped_pixels(&scope, &still);

    tint_in_process_surfaces(None);
    preem_render::forget_scope(&scope);

    let floods = |px: &(u32, u32, Vec<u8>)| px.2.chunks_exact(4).any(|c| c == lilac);
    assert!(
        floods(&pinned_teal),
        "a pinned field must flood the widget's ground, exactly, not something derived from it",
    );
    assert!(
        !floods(&plain_teal),
        "…and the same widget without the pin must not, or the check above is vacuous",
    );
    assert!(
        floods(&pinned_rose),
        "the ground is excluded from the re-tint — the accent moved and the field did not",
    );
    assert_ne!(
        pinned_teal, pinned_rose,
        "…while its *ink* still follows the desktop: the two pins are independent",
    );
    assert_eq!(
        still_teal, still_rose,
        "and with both slots spoken for the whole widget is byte-identical across the change",
    );
}

/// The whole point of the widening, end to end on the state path: a `TextBox`
/// carrying all three pins renders **byte-identically** to the kit call `pet`
/// made before it migrated (`TextBox::new().…​.colors(field, ink, notdef)`).
///
/// This is the shell's half of #884's compat promise. The plugin's half — that
/// its *raster* arm still produces those same bytes against an old shell — is
/// pinned in `hytte-plugin-pet` and `hytte-plugin-caw`; together they say the
/// bubbles look the same on both shells and on both arms.
///
/// The config is `pet`'s real one, down to the emoji: an uncovered char is the
/// only input that reaches the `notdef` slot, which is the color the palette
/// scope structurally *cannot* carry (no kit palette has one) and so the one a
/// wrong wiring drops silently.
///
/// **Deletion check:** making `text_box` ignore `config.notdef` leaves the rest
/// of the shell suite green and turns the oracle comparison red; so does making
/// `pins_for` drop the field. The three "…must move pixels" controls below say
/// which pin each failure is about.
#[test]
fn a_fully_pinned_text_box_reproduces_the_plugins_own_palette() {
    let _ink = preem_ink_lock();
    let field = [0x3a, 0x22, 0x50, 0xff];
    let ink = [0xf0, 0xe0, 0xf8, 0xff];
    let notdef = [0x6c, 0x4e, 0x86, 0xff];
    let text = "mrrp 💕";
    let scope = Scope::detached("885-palette");

    let config = |field, ink, notdef| vocab::TextBoxConfig {
        style: {
            let base = vocab::StyleRef::new(vocab::StyleName::Lcd);
            let base = match field {
                Some(f) => base.with_field(f),
                None => base,
            };
            match ink {
                Some(i) => base.with_ink(i),
                None => base,
            }
        },
        width: vocab::TextBoxWidth::FitPx(126),
        max_lines: 3,
        pad: 3,
        corner: 2,
        scale: 2,
        fixed_width: true,
        notdef,
    };
    let node = |id: &'static str, config| {
        preem_node(
            Some(id),
            vocab::PreemWidget::TextBox {
                config,
                state: vocab::TextBoxState { text: text.into() },
            },
        )
    };

    // An accent is installed throughout: a pin that quietly fell through to the
    // session tint would then differ from the oracle rather than coincide with it.
    tint_in_process_surfaces(Some([0x11, 0x99, 0xaa, 0xff]));
    let all = mapped_pixels(
        &scope,
        &node("all", config(Some(field), Some(ink), Some(notdef))),
    );
    let no_field = mapped_pixels(&scope, &node("nf", config(None, Some(ink), Some(notdef))));
    let no_ink = mapped_pixels(&scope, &node("ni", config(Some(field), None, Some(notdef))));
    let no_notdef = mapped_pixels(&scope, &node("nn", config(Some(field), Some(ink), None)));

    // …and it survives the theme moving, like every pin.
    tint_in_process_surfaces(Some([0xdd, 0x22, 0x66, 0xff]));
    let all_again = mapped_pixels(
        &scope,
        &node("all", config(Some(field), Some(ink), Some(notdef))),
    );
    tint_in_process_surfaces(None);
    preem_render::forget_scope(&scope);

    // The oracle is the pre-#884 kit call, written out rather than derived from
    // `text_box`: an oracle built from the code under test agrees by construction.
    let oracle = kit::TextBox::new()
        .fit_px(126)
        .max_lines(3)
        .pad(3)
        .corner(2)
        .scale(2)
        .fixed_width(true)
        .colors(field, ink, notdef);
    assert_eq!(
        all,
        kit_pixels(&oracle.render(text)),
        "a fully pinned TextBox must reproduce the plugin's own `colors()` bytes",
    );
    assert_eq!(
        all, all_again,
        "…and survive a theme change byte-identically"
    );
    assert_ne!(all, no_field, "the field pin must move pixels");
    assert_ne!(all, no_ink, "the ink pin must move pixels");
    assert_ne!(
        all, no_notdef,
        "the notdef pin must move pixels — the emoji is what reaches it",
    );
}

/// The **second** palette scope: `Renderer::update`'s marquee arm, which
/// re-rasterises the strip in place on a *text* change.
///
/// This is the one path where a state change re-runs a constructor that **bakes**
/// its palette. `Marquee::render` floods the strip's backdrop from
/// `palette().bg` (`marquee.rs:179`) and only re-resolves the lit ink per
/// `window()` call, so a strip built outside the widget's pins keeps the skin's
/// ground for the rest of the session. `build()` never runs here: a new message
/// leaves `same_config` agreeing, which is exactly why the scope has to be
/// repeated on this line rather than inherited from construction.
///
/// The `builds == 1` assertion is load-bearing — without it a rebuild would
/// satisfy the flood assertion and this would silently be a second test of
/// `build()`'s scope instead of `update`'s.
///
/// Found by review at `3b13ce32`: narrowing this one scope back to
/// `kit::with_ink(ink_for(…))` left the whole 339-test shell suite green, while
/// the same narrowing in `build()` reds
/// `a_fully_pinned_text_box_reproduces_the_plugins_own_palette`. The code was
/// already right; nothing measured it.
///
/// **Deletion check:** narrow `Renderer::update`'s marquee arm to `with_ink`
/// and this goes red at *"…and the strip `update` re-rasterises must keep it"*,
/// with `builds == 1` still holding.
#[test]
fn a_pinned_field_survives_a_marquee_text_change() {
    let _ink = preem_ink_lock();
    let lilac = [0x3a, 0x22, 0x50, 0xff];
    let scope = Scope::detached("885-marquee-field");
    let node = |text: &str| {
        preem_node(
            Some("mq"),
            vocab::PreemWidget::Marquee {
                config: vocab::MarqueeConfig {
                    style: vocab::StyleRef::new(vocab::StyleName::Vfd).with_field(lilac),
                    window_px: 192,
                    gap_dots: 6,
                    speed_dots_per_sec: 20.0,
                },
                state: vocab::MarqueeState { text: text.into() },
            },
        )
    };
    let floods = |px: &(u32, u32, Vec<u8>)| px.2.chunks_exact(4).any(|c| c == lilac);

    let first = mapped_pixels(&scope, &node("ONE LONG SCROLLING MESSAGE"));
    // A text change only: `same_config` still agrees, so this takes the
    // in-place `update` path and re-rasterises the strip there.
    let after = mapped_pixels(&scope, &node("ANOTHER LONG SCROLLING MESSAGE"));
    let builds = preem_render::probe(&scope, Some("mq"));
    preem_render::forget_scope(&scope);

    assert_eq!(
        builds.map(|(b, _)| b),
        Some(1),
        "the text change must be an in-place update, not a rebuild — or this measures `build`",
    );
    assert!(floods(&first), "the pin reaches the strip built by `build`");
    assert!(
        floods(&after),
        "…and the strip `update` re-rasterises must keep it",
    );
}

/// One `PreemWidget` of `kind`, its style reference carried in, in state
/// variant `b` or `a` — the two states
/// [`a_pinned_field_survives_a_state_change_on_every_widget`] drives through the
/// in-place `Renderer::update` path.
///
/// Split out of the test purely so neither function is 160 lines; the pairs
/// differ in **state only**, which is the property that makes `same_config`
/// agree and `apply` take `update` instead of rebuilding.
fn state_pair_of(kind: &str, style: vocab::StyleRef, b: bool) -> vocab::PreemWidget {
    let text = if b { "BBBB" } else { "AAAA" }.to_owned();
    match kind {
        "dm" => vocab::PreemWidget::DotMatrix {
            config: vocab::DotMatrixConfig { style },
            state: vocab::DotMatrixState { text },
        },
        "seg" => vocab::PreemWidget::SevenSeg {
            config: vocab::SevenSegConfig { style },
            state: vocab::SevenSegState { text },
        },
        "tb" => vocab::PreemWidget::TextBox {
            config: vocab::TextBoxConfig {
                style,
                ..vocab::TextBoxConfig::default()
            },
            state: vocab::TextBoxState { text },
        },
        "led" => vocab::PreemWidget::LedStrip {
            config: vocab::LedStripConfig {
                style,
                ..vocab::LedStripConfig::default()
            },
            state: vocab::LedStripState {
                level: if b { 0.8 } else { 0.2 },
                peak: b.then_some(0.9),
            },
        },
        "mq" => vocab::PreemWidget::Marquee {
            config: vocab::MarqueeConfig {
                style,
                ..vocab::MarqueeConfig::default()
            },
            state: vocab::MarqueeState {
                text: format!("{text} LONG SCROLLING MESSAGE"),
            },
        },
        "sc" => vocab::PreemWidget::Scope {
            config: vocab::ScopeConfig {
                style,
                ..vocab::ScopeConfig::default()
            },
            state: vocab::ScopeState {
                samples: if b {
                    vec![1.0, -1.0, 0.25]
                } else {
                    vec![0.0, 0.5, -0.5]
                },
            },
        },
        "ga" => vocab::PreemWidget::Gauge {
            config: vocab::GaugeConfig {
                style,
                ..vocab::GaugeConfig::default()
            },
            state: vocab::GaugeState {
                target: if b { 0.8 } else { 0.2 },
            },
        },
        "fb" => vocab::PreemWidget::FlipBoard {
            config: vocab::FlipBoardConfig {
                style,
                ..vocab::FlipBoardConfig::default()
            },
            state: vocab::FlipBoardState { text },
        },
        other => panic!("no such widget kind: {other}"),
    }
}

/// …and the same claim for **every** widget kind, over a state change that
/// takes the in-place `Renderer::update` path.
///
/// The marquee test above closes the one arm that re-rasterises. This one is the
/// enumeration behind "and no other arm can": rather than arguing it in prose,
/// it drives a state change through all eight and asserts the pinned ground is
/// still flooded afterwards, with `builds == 1` proving none of them rebuilt.
///
/// The kit side of the argument, re-derived here rather than taken from #912's
/// list: exactly two non-test functions in `hytte-preem` read `palette()`
/// outside a `render`/`window` call — `TextBox::styled` (`textbox.rs:74`) and
/// `Marquee::render` (`marquee.rs:179`). Those are the only two that can bake.
/// `TextBox`'s update arm copies text and does not rebuild the box; `Scope`
/// stores a sample batch; `Gauge::set_target`, `FlipBoard::set_text` and the LED
/// strip's level/peak/hold all touch state a later `render(style)` reads inside
/// `Instance::frame`'s scope. So the marquee arm is the whole exposure, and this
/// test is what will notice if a future arm joins it.
///
/// **Deletion check:** narrowing `Renderer::update`'s marquee scope to
/// `with_ink` reds the `mq` row; making `pins_for` drop the field reds every
/// row.
#[test]
fn a_pinned_field_survives_a_state_change_on_every_widget() {
    let _ink = preem_ink_lock();
    let lilac = [0x3a, 0x22, 0x50, 0xff];
    let vfd = vocab::StyleRef::new(vocab::StyleName::Vfd).with_field(lilac);
    let scope = Scope::detached("885-field-every-widget");
    let floods = |px: &(u32, u32, Vec<u8>)| px.2.chunks_exact(4).any(|c| c == lilac);

    for id in ["dm", "seg", "tb", "led", "mq", "sc", "ga", "fb"] {
        let before = mapped_pixels(&scope, &preem_node(Some(id), state_pair_of(id, vfd, false)));
        let after = mapped_pixels(&scope, &preem_node(Some(id), state_pair_of(id, vfd, true)));
        assert_eq!(
            preem_render::probe(&scope, Some(id)).map(|(b, _)| b),
            Some(1),
            "{id}: a state change must not rebuild, or this proves nothing about `update`",
        );
        assert!(floods(&before), "{id}: the pin reaches the first render");
        assert!(
            floods(&after),
            "{id}: …and survives the in-place state change",
        );
    }
    preem_render::forget_scope(&scope);
}

/// The memoized role colors are dropped when the theme moves — which is what
/// makes #396 true for `Success`/`Warning`/`Error` and not just for the accent.
///
/// A color-scheme flip moves `@success_color` exactly as it can move
/// `@accent_color`. Without the drop, a status widget would keep the previous
/// scheme's green for the rest of the session, since nothing else invalidates a
/// memo that is only read on a cold cache.
///
/// The observation is indirect by necessity — the hermetic binary has no theme
/// to re-resolve against — but it is exact: after the second theme change the
/// memo is cold, `resolve_role_inks` returns the "no GTK" answer, and the role
/// degrades to the accent. A *surviving* memo would still be carrying `green`.
///
/// **Deletion check:** removing `ROLE_INKS.set(None)` from
/// `invalidate_cached_frames` leaves the rest of the shell suite green and turns
/// this red at "the memo must be dropped on a theme change…".
#[test]
fn a_theme_change_drops_the_memoized_role_colors() {
    let _ink = preem_ink_lock();
    let green = [0x2e, 0xc2, 0x7e, 0xff];
    let scope = Scope::detached("885-role-memo");
    let ok = ink_probe(
        "ok",
        vocab::StyleRef::new(vocab::StyleName::Vfd).with_accent(vocab::AccentRole::Success),
    );

    tint_in_process_surfaces(Some([0x11, 0x99, 0xaa, 0xff]));
    preem_render::set_role_inks(preem_render::RoleInks {
        success: Some(green),
        warning: None,
        error: None,
    });
    let first = mapped_pixels(&scope, &ok);

    // The theme moved and nothing re-injects, so a dropped memo re-resolves to
    // the hermetic "no theme" answer and the role degrades to the accent.
    tint_in_process_surfaces(Some([0xdd, 0x22, 0x66, 0xff]));
    let second = mapped_pixels(&scope, &ok);
    tint_in_process_surfaces(None);
    preem_render::forget_scope(&scope);

    assert!(
        first.2.chunks_exact(4).any(|px| px == green),
        "the injected role color must reach the first render",
    );
    assert!(
        !second.2.chunks_exact(4).any(|px| px == green),
        "the memo must be dropped on a theme change, so the stale role color cannot survive it",
    );
}
