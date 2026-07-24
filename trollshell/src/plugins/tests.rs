//! Unit + per-connection integration tests for the plugin host transport.
//! Pulls the tested items from their respective submodules (all `pub(super)`,
//! so visible to this descendant module) and drives `handle_conn` end to end
//! over a `UnixStream::pair` socketpair.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hytte::futures_signals::signal::Mutable;
use hytte::ui::{Dir as UiDir, EventKind as UiEventKind, Node as UiNode};
use hytte_plugin_proto::{
    AudioAction, Capability, ClockState, Effect, HostMsg, Manifest, MediaAction, Mount, NiriAction,
    Page, PluginMsg, StateKey, read_frame, wire, write_frame,
};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use super::effects::{PageAction, map_page, resolve_open_page};
use super::listener::{ACCEPT_BACKOFF, accept_backoff, socket_in_use};
use super::pump::{any_sidebar_open, apply_forget, apply_open};
use super::region::{clear_region_if_owned, upsert_region};
use super::session::{
    EFFECT_BURST, EffectRateLimiter, IdGuard, OUTBOUND_CAPACITY, REGISTER_TIMEOUT,
    effect_capability, enforce_capabilities, handle_conn,
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
    assert_eq!(to_ui_node(&tree), expected);
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
    assert_eq!(to_ui_node(&tree), expected);
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
    assert_eq!(to_ui_node(&tree), expected);
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
    assert_eq!(to_ui_node(&tree), expected);
}

/// Every wire `Page` maps to the identically-named `modal::Page` (#508: an
/// exact 1:1 match again, now that the host's `Stats` page is combined).
#[test]
fn wire_page_maps_to_modal_page() {
    use crate::modal::Page as M;
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
        assert_eq!(map_page(wire_page), modal_page);
    }
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
        to_ui_node(&bad),
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
        to_ui_node(&good),
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
    let ui_scale = |n: &wire::Node| match to_ui_node(n) {
        UiNode::Pixels { scale, .. } => scale,
        other => panic!("expected Pixels, got {other:?}"),
    };
    assert_eq!(ui_scale(&node(2)), 2);
    assert_eq!(ui_scale(&node(0)), 1);
    assert_eq!(ui_scale(&node(u32::MAX)), 16_384);
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
async fn recv<R>(rd: &mut R) -> HostMsg
where
    R: tokio::io::AsyncRead + Unpin,
{
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_frame::<HostMsg, _>(rd),
    )
    .await
    .expect("a host frame within 5s")
    .expect("decode HostMsg")
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
        live_ids: Arc::new(Mutex::new(HashSet::new())),
        // Host-scoped runtime mirror (#423); like `live_ids`, kept per-ctx so the
        // per-connection tests stay isolated and never publish `PLUGIN_RUNTIME`.
        runtime: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
        effects_tx,
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
