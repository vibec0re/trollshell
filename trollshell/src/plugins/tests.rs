//! Unit + per-connection integration tests for the plugin host transport.
//! Pulls the tested items from their respective submodules (all `pub(super)`,
//! so visible to this descendant module) and drives `handle_conn` end to end
//! over a `UnixStream::pair` socketpair.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::ui::{Dir as UiDir, EventKind as UiEventKind, Node as UiNode};
use hytte_plugin_proto::{
    Capability, ClockState, DatasourceError, DatasourceOutcome, Effect, EffectOutcome, HostMsg,
    Manifest, Mount, NiriAction, NowPlaying, Page, PluginMsg, ProvidedDatasource, StateKey, VOCAB,
    preem as vocab, read_frame, wire, write_frame,
};
use hytte_preem as kit;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use super::datasource::DatasourceRouter;
use super::effects::broker_effect;
#[cfg(feature = "system-tests")]
use super::effects::start_detached;
use super::effects::{
    AuditDecision, FORWARDED_ENV, FallbackReason, LaunchFailure, LaunchReport, PageAction,
    allocate_launch_unit, classify_systemd_run_failure, execute_command, format_audit_line,
    launch_argv, launch_outcome, launch_unit_name, map_page, map_page_for_layout,
    resolve_open_page,
};
use super::listener::{
    ACCEPT_BACKOFF, SocketClaim, accept_backoff, acquire_listen_lock, lock_path, socket_in_use,
    take_socket,
};
use super::preem_render::{self, Scope};
use super::pump::{
    any_sidebar_open, apply_forget, apply_open, request_remap, request_remap_holding,
    tick_decision, tint_in_process_surfaces, to_now_playing, to_upcoming_events,
};
use super::region::{clear_region_if_owned, upsert_region};
use super::session::{
    EFFECT_BURST, EffectRateLimiter, HiddenOnViolation, IdGuard, MAX_HIDDEN_ON_ENTRIES,
    MAX_HIDDEN_ON_NAME_BYTES, OUTBOUND_CAPACITY, REGISTER_TIMEOUT, capped_hidden_on,
    enforce_capabilities, handle_conn, push_gate, state_key_capability,
};
use super::shader_map::{self, Grants};
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
///
/// The tree deliberately sets a **non-`None` `tooltip`** (#957) on the root
/// `Box` and on both leaf kinds that declare one: an optional field is exactly
/// the kind a mapping arm forgets, and `None` everywhere would let a dropped
/// `tooltip: tooltip.clone()` pass unnoticed.
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
                tooltip: Some("a greeting".into()),
            },
            wire::Node::Icon {
                id: Some("i".into()),
                name: "battery-symbolic".into(),
                classes: vec![],
                tooltip: Some("87%, 3h left".into()),
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
                    tooltip: None,
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
                tooltip: None,
            },
            wire::Node::Spacer,
        ],
        // Carried across the mapping, not dropped (#957) — set on the root
        // box and on the two leaf kinds that declare the field.
        tooltip: Some("the whole card".into()),
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
                tooltip: Some("a greeting".into()),
            },
            UiNode::Icon {
                id: Some("i".into()),
                name: "battery-symbolic".into(),
                classes: vec![],
                tooltip: Some("87%, 3h left".into()),
            },
            UiNode::Pixels {
                id: Some("px".into()),
                width: 1,
                height: 1,
                data: Arc::from([10, 20, 30, 255].as_slice()),
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
                    tooltip: None,
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
                tooltip: None,
            },
            UiNode::Spacer,
        ],
        tooltip: Some("the whole card".into()),
    };
    assert_eq!(
        to_ui_node(&Scope::detached("map"), Grants::none(), &tree),
        expected
    );
}

/// The list nodes map field-for-field: `Row`/`ListBox` recurse their
/// children like `Box`, and `Text` carries `max_width_chars` **and** the
/// #297 `ellipsize` flag. A `Spacer` between the cluster and the value maps
/// 1:1 (the justification primitive).
///
/// Since #966 it also pins the two additive list props across the map:
/// `ListBox::dense` carries as-is, and `Row::spacing` **widens** from the
/// wire's `u16` to the reconciler's `i32` — the one arithmetic step in this
/// otherwise 1:1 file, and the one thing a field-for-field `assert_eq!` on a
/// zero would not have caught.
///
/// Since #961 the `tooltip`s ride along, and two of the three cases here are
/// deliberate. The `Row`'s and the second `Text`'s are **non-`None`**, so a
/// dropped `tooltip: tooltip.clone()` in the map fails rather than passing on
/// `None == None` — the mistake an optional field invites. The *ellipsizing*
/// `Text`'s stays `None` on **both** sides, which pins that the derived
/// "an ellipsized text hovers itself" default is the **reconciler's**
/// (`hytte_ui::widget_tree::node_tooltip`) and not something this map bakes
/// into the tree it hands over.
#[test]
fn wire_row_listbox_text_map_to_ui() {
    let tree = wire::Node::ListBox {
        dense: true,
        id: Some("list".into()),
        classes: vec!["ts-list".into()],
        children: vec![wire::Node::Row {
            spacing: 6,
            id: Some("r0".into()),
            classes: vec!["ts-row".into()],
            children: vec![
                wire::Node::Text {
                    id: None,
                    text: "an ellipsized destination".into(),
                    max_width_chars: Some(20),
                    ellipsize: true,
                    classes: vec!["ts-dest".into()],
                    tooltip: None,
                },
                wire::Node::Text {
                    id: None,
                    text: "spor 2".into(),
                    max_width_chars: None,
                    ellipsize: false,
                    classes: vec![],
                    tooltip: Some("platform 2".into()),
                },
                wire::Node::Spacer,
                wire::Node::Label {
                    id: None,
                    text: "12:30".into(),
                    classes: vec!["ts-time".into()],
                    tooltip: None,
                },
            ],
            tooltip: Some("Oslo S → Lillestrøm".into()),
        }],
    };
    let expected = UiNode::ListBox {
        id: Some("list".into()),
        classes: vec!["ts-list".into()],
        dense: true,
        children: vec![UiNode::Row {
            id: Some("r0".into()),
            classes: vec!["ts-row".into()],
            spacing: 6,
            children: vec![
                UiNode::Text {
                    id: None,
                    text: "an ellipsized destination".into(),
                    max_width_chars: Some(20),
                    ellipsize: true,
                    classes: vec!["ts-dest".into()],
                    tooltip: None,
                },
                UiNode::Text {
                    id: None,
                    text: "spor 2".into(),
                    max_width_chars: None,
                    ellipsize: false,
                    classes: vec![],
                    tooltip: Some("platform 2".into()),
                },
                UiNode::Spacer,
                UiNode::Label {
                    id: None,
                    text: "12:30".into(),
                    classes: vec!["ts-time".into()],
                    tooltip: None,
                },
            ],
            tooltip: Some("Oslo S → Lillestrøm".into()),
        }],
    };
    assert_eq!(
        to_ui_node(&Scope::detached("map"), Grants::none(), &tree),
        expected
    );
}

/// #966's bounded viewport maps 1:1, `max_height` widening `u16` → `i32` the
/// way `Row::spacing` does, and its **mandatory** child recursing through the
/// same `?` path `Button`/`Revealer` use — so a viewport whose only child fell
/// past a cap is dropped with it rather than mapping to an empty scroller.
#[test]
fn wire_scrolled_maps_to_ui() {
    let tree = wire::Node::Scrolled {
        id: Some("card-body".into()),
        max_height: 240,
        classes: vec!["ts-card-scroll".into()],
        child: Box::new(wire::Node::Label {
            id: None,
            text: "row".into(),
            classes: vec![],
            tooltip: None,
        }),
    };
    let expected = UiNode::Scrolled {
        id: Some("card-body".into()),
        max_height: 240,
        classes: vec!["ts-card-scroll".into()],
        child: Box::new(UiNode::Label {
            id: None,
            text: "row".into(),
            classes: vec![],
            tooltip: None,
        }),
    };
    assert_eq!(
        to_ui_node(&Scope::detached("map"), Grants::none(), &tree),
        expected
    );
}

/// The #333 `Expander` maps 1:1: the boxed `header` and the body `children`
/// recurse, and the `expanded` mutable prop carries across — as does #961's
/// `tooltip`, set non-`None` here so a dropped clone can't pass.
#[test]
fn wire_expander_maps_to_ui() {
    let tree = wire::Node::Expander {
        id: "room".into(),
        header: Box::new(wire::Node::Label {
            id: None,
            text: "Living Room".into(),
            classes: vec!["heading".into()],
            tooltip: None,
        }),
        children: vec![wire::Node::Label {
            id: Some("d".into()),
            text: "Lamp".into(),
            classes: vec![],
            tooltip: None,
        }],
        expanded: true,
        classes: vec!["boxed-list".into()],
        tooltip: Some("3 devices, 1 on".into()),
    };
    let expected = UiNode::Expander {
        id: "room".into(),
        header: Box::new(UiNode::Label {
            id: None,
            text: "Living Room".into(),
            classes: vec!["heading".into()],
            tooltip: None,
        }),
        children: vec![UiNode::Label {
            id: Some("d".into()),
            text: "Lamp".into(),
            classes: vec![],
            tooltip: None,
        }],
        expanded: true,
        classes: vec!["boxed-list".into()],
        tooltip: Some("3 devices, 1 on".into()),
    };
    assert_eq!(
        to_ui_node(&Scope::detached("map"), Grants::none(), &tree),
        expected
    );
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
    assert_eq!(
        to_ui_node(&Scope::detached("map"), Grants::none(), &tree),
        expected
    );
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
            tooltip: None,
        },
        panel: None,
        grants: Grants::none(),
        outbound: tx.clone(),
        hidden_on: Vec::new(),
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
            tooltip: None,
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
        to_ui_node(&Scope::detached("pixels"), Grants::none(), &bad),
        UiNode::Pixels {
            id: Some("lcd".into()),
            width: 0,
            height: 0,
            data: Arc::from(&[][..]),
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
        to_ui_node(&Scope::detached("pixels"), Grants::none(), &good),
        UiNode::Pixels {
            id: None,
            width: 1,
            height: 2,
            data: Arc::from([1, 2, 3, 4, 5, 6, 7, 8].as_slice()),
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
    let ui_scale = |n: &wire::Node| match to_ui_node(&Scope::detached("scale"), Grants::none(), n) {
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
    let ui_fraction = |n: &wire::Node| match to_ui_node(&scope, Grants::none(), n) {
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
    let mapped = to_ui_node(&scope, Grants::none(), &slider(10.0, 5.0, f64::NAN, 0.0));
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
        to_ui_node(&scope, Grants::none(), &legal),
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
                tooltip: None,
            },
            // A panel-less render: the chip lands in its bar region, and the
            // dedicated panels mailbox (#349 PR2) must stay empty.
            panel: None,
            effects: vec![],
            hidden_on: Vec::new(),
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

/// #1050: a frame's `hidden_on` must survive the wire → [`SlotRender`] hop the
/// reader task performs, unaltered and in order.
///
/// Everything else that tests per-screen visibility (`region.rs`'s `gtk_tests`)
/// builds its `SlotRender`s **by hand**, so all of it stays green against a host
/// that decodes the field and then quietly drops it on the way to the mailbox —
/// which is the whole feature failing, silently, on real glass. Nothing else in
/// the suite covers this hop; the mutation campaign for the host arm found it
/// exactly this way (M17 was green before this test existed).
///
/// Order and duplicates are asserted as-received rather than as a set: the wire
/// contract is an exact list, and a host that sorted or de-duplicated it would
/// be changing what the plugin said.
#[tokio::test]
async fn hidden_on_survives_the_wire_into_the_render_mailbox() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);
    let bar_center = ctx.bar_center.clone();

    let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host_end, &ctx).await });

    let (_prd, mut pwr) = plugin_end.into_split();
    write_frame(
        &mut pwr,
        &PluginMsg::Register {
            manifest: Manifest::new("perscreen", Mount::BarCenter),
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
                tooltip: None,
            },
            panel: None,
            hidden_on: vec!["HDMI-A-1".into(), "DP-2".into()],
            effects: vec![],
        },
    )
    .await
    .expect("send Render");

    let cards = wait_for_region(&bar_center).await;
    assert_eq!(
        cards[0].hidden_on,
        vec!["HDMI-A-1".to_owned(), "DP-2".to_owned()],
        "the per-screen verdict must reach the mailbox verbatim — the per-monitor \
         reconcilers read it from here and nowhere else",
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
                tooltip: None,
            },
            panel: Some(Box::new(wire::Node::Label {
                id: Some("panel".into()),
                text: "panel body".into(),
                classes: vec![],
                tooltip: None,
            })),
            effects: vec![],
            hidden_on: Vec::new(),
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
                tooltip: None,
            },
            panel: None,
            effects: vec![],
            hidden_on: Vec::new(),
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

/// How long a socket/lock test waits for a kernel-side teardown to land before
/// it gives up and fails loudly: 500 × 1 ms.
///
/// Dropping a `UnixListener`, or releasing an `flock`, is **not** synchronous
/// with the `drop` that asks for it. Under load the #1004 review measured a
/// dropped listener still carrying `SO_ACCEPTCON` in `/proc/net/unix` (so
/// [`socket_in_use`] answered "live" for a socket the test had just dropped),
/// and a released flock still answering `WouldBlock` while `/proc/locks`
/// listed nothing for the inode — ≈2 % of full-suite runs, in the **default**
/// hermetic bucket that `nix build .#trollshell`'s `doCheck` runs on every
/// build. Production never observes its own `drop`; only these tests do. So a
/// test that needs "the previous owner is really gone" **establishes** that
/// precondition rather than assuming the preceding statement already achieved
/// it.
const SETTLE_ATTEMPTS: usize = 500;

/// Bounded 1 ms spin until `ready()` answers true. Panics naming `what` if the
/// precondition never lands — the cap is what keeps a genuine regression (a
/// lock that is never released, a listener that never dies) a **failure**
/// rather than a hang. See [`SETTLE_ATTEMPTS`].
fn settle_blocking(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..SETTLE_ATTEMPTS {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("precondition never settled after {SETTLE_ATTEMPTS}ms: {what}");
}

/// [`settle_blocking`]'s async twin for the one probe that is a future: wait
/// until nothing answers on `path`, i.e. the socket file there really is stale.
async fn settle_until_socket_is_stale(path: &std::path::Path) {
    for _ in 0..SETTLE_ATTEMPTS {
        if !socket_in_use(path).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!(
        "precondition never settled after {SETTLE_ATTEMPTS}ms: a dropped listener still answers on {}",
        path.display()
    );
}

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
    // answers it — a stale socket, which is reclaimable. The teardown is not
    // synchronous with `drop` (see `SETTLE_ATTEMPTS`), so wait for it to land
    // instead of asserting into the race.
    drop(listener);
    settle_until_socket_is_stale(&path).await;
    assert!(
        path.exists(),
        "the stale socket file is still on disk — it is the *listener* that is gone",
    );
    assert!(
        !socket_in_use(&path).await,
        "a stale socket (no listener) is not in use, so it can be reclaimed",
    );
}

// ── Single-instance lock (#996) ───────────────────────────────────────────

/// #996 mechanism: the single-instance lock is **exclusive within one
/// process** and released by dropping the handle. `File::try_lock` is
/// `flock(LOCK_EX | LOCK_NB)`, whose lock lives on the open file description,
/// so two separate opens of the same path conflict even here — which is what
/// makes the racing test below possible without forking, and what makes the
/// kernel release the lock when a shell dies rather than leaving stale state.
/// Hermetic: a scratch dir, no sockets, no daemons.
#[test]
fn listen_lock_is_exclusive_within_one_process() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = dir.path().join("plugin.sock.lock");

    let held = acquire_listen_lock(&lock)
        .expect("lock file opens")
        .expect("the first instance takes the lock");
    assert!(
        acquire_listen_lock(&lock)
            .expect("lock file opens for the second instance too")
            .is_none(),
        "a second instance must NOT get the lock while the first holds it",
    );

    // Releasing (process exit, in production) makes it reclaimable — a restart
    // must not be wedged by its predecessor's lock file, which is never
    // unlinked. The release is not synchronous with `drop` under load (see
    // `SETTLE_ATTEMPTS`); a bounded retry still fails loudly if the lock is
    // never handed back, which is the property under test.
    drop(held);
    settle_blocking("the lock is reclaimable once the holder drops it", || {
        acquire_listen_lock(&lock)
            .expect("lock file opens")
            .is_some()
    });
}

/// #996: the lock is taken **before** the probe, so a second instance is
/// turned away by the lock (`Locked`) and never reaches the probe/unlink/bind
/// sequence at all. This is the ordering assertion: with the pre-#996 code
/// there was no lock and the loser fell through to `remove_file` + `bind`.
/// The winner keeps a listener that actually answers on the path — the exact
/// property the TOCTOU destroyed (the first binder stayed valid on an inode
/// nothing could name, and accepted nothing for the rest of the process).
#[tokio::test]
async fn a_second_take_is_refused_by_the_lock_before_the_probe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("plugin.sock");

    let SocketClaim::Bound(socket) = take_socket(&path).await.expect("the first instance binds")
    else {
        panic!("nothing holds the lock and no socket exists: the first take must bind");
    };
    let winner_inode = std::fs::metadata(&path).expect("socket exists").ino();

    // The loser is refused by the LOCK, not by the liveness probe. Asserting
    // the discriminant (not just "did not bind") is what pins the ordering:
    // without the lock this would come back `AlreadyLive` at best, and on the
    // real race — where both probe before either binds — a rebind.
    assert!(
        matches!(
            take_socket(&path).await.expect("the second take completes"),
            SocketClaim::Locked
        ),
        "a second instance is refused by the single-instance lock",
    );
    assert_eq!(
        std::fs::metadata(&path).expect("socket still exists").ino(),
        winner_inode,
        "the loser never unlinks or rebinds the winner's socket",
    );

    // ...and the winner is not deaf: the path still routes to its listener.
    let (client, accepted) = tokio::join!(UnixStream::connect(&path), socket.accept());
    client.expect("a client can still dial the path");
    accepted.expect("the connection lands on the winner's listener");
}

/// #436, pinned. A live listener that never took the lock — a trollshell older
/// than #996, or any other process that bound the path — must be refused as
/// [`SocketClaim::AlreadyLive`]: its socket is left exactly where it is, it
/// keeps answering, and the refused newcomer hands the lock back rather than
/// wedging the next start.
///
/// This guard is the reason `take_socket` still probes at all, and until this
/// test existed **deleting the probe left the whole suite green**: the racing
/// test deliberately uses a *stale* socket so the probe cannot decide it, and
/// the ordering test is decided by the lock before the probe is reached. So
/// the one outcome #436 is actually about was pinned by nothing, and a dev
/// `cargo run` beside a pre-#996 deployed shell would have silently unlinked
/// the live socket again.
#[tokio::test]
async fn a_live_listener_without_the_lock_is_refused_as_already_live() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("plugin.sock");
    let lock = lock_path(&path);

    // An instance older than #996: it owns the socket and never takes the lock.
    let incumbent = tokio::net::UnixListener::bind(&path).expect("the incumbent binds");
    let incumbent_inode = std::fs::metadata(&path).expect("socket exists").ino();
    assert!(
        !lock.exists(),
        "the incumbent predates the lock, so nothing has created the lock file yet",
    );

    assert!(
        matches!(
            take_socket(&path).await.expect("the take completes"),
            SocketClaim::AlreadyLive
        ),
        "a live listener the lock cannot see still turns the newcomer away (#436)",
    );
    assert_eq!(
        std::fs::metadata(&path).expect("socket still exists").ino(),
        incumbent_inode,
        "the refused instance never unlinks or rebinds the live socket",
    );

    // The refused take drops its lock handle on the way out, so it does not
    // wedge the lock for the next start (bounded — the release is not
    // synchronous with the drop; see `SETTLE_ATTEMPTS`).
    settle_blocking("an AlreadyLive refusal releases the lock it took", || {
        acquire_listen_lock(&lock)
            .expect("lock file opens")
            .is_some()
    });

    // ...and the incumbent is still serving: the whole point of standing down.
    let (client, accepted) = tokio::join!(UnixStream::connect(&path), incumbent.accept());
    client.expect("a client can still dial the incumbent");
    accepted.expect("the dial lands on the incumbent's listener");
}

/// #996: the single-instance lock is held for exactly as long as the bound
/// socket can accept, because it *is* part of the bound socket. Held across an
/// accept, and released when — and only when — the socket drops.
///
/// The lock and the listener used to be two bindings, and the invariant rested
/// on a `_lock` binding in `listen` that nothing observed: changing it to `_`
/// dropped the flock the instant the socket was bound and left the suite green
/// (146 passed). Making them one value is what makes that mutation impossible
/// to express; this test is what makes deleting the field from the value red.
#[tokio::test]
async fn the_bound_socket_holds_the_single_instance_lock_until_it_drops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("plugin.sock");
    let lock = lock_path(&path);

    let SocketClaim::Bound(socket) = take_socket(&path).await.expect("the first instance binds")
    else {
        panic!("nothing holds the lock and no socket exists: the take must bind");
    };
    assert!(
        acquire_listen_lock(&lock)
            .expect("lock file opens")
            .is_none(),
        "the bound socket holds the lock: a second instance cannot take it",
    );

    // Serving does not release it — the accept loop runs for the host's whole
    // life, and the gate has to stay shut for all of it.
    let (client, accepted) = tokio::join!(UnixStream::connect(&path), socket.accept());
    client.expect("a client can dial the bound socket");
    accepted.expect("the dial lands on the bound socket");
    assert!(
        acquire_listen_lock(&lock)
            .expect("lock file opens")
            .is_none(),
        "the lock is still held after the socket has accepted a connection",
    );

    // Dropping the socket (process going away) releases the lock with it, so
    // the next start is not wedged.
    drop(socket);
    settle_blocking("dropping the bound socket releases its lock", || {
        acquire_listen_lock(&lock)
            .expect("lock file opens")
            .is_some()
    });
}

/// What one racer got out of [`take_socket`] in
/// [`racing_takes_never_both_bind_the_socket`]. Named rather than reduced to a
/// `bool`, because "did not bind" is not the property under test: a loser that
/// *errored* (its `bind` losing to the winner's with `EADDRINUSE`) also fails
/// to bind, and that is a pre-fix outcome, not a fixed one. `Bound` carries the
/// path's inode as the winner saw it.
#[derive(Debug, PartialEq, Eq)]
enum RaceOutcome {
    Bound(u64),
    Locked,
    AlreadyLive,
    Failed(String),
}

/// #996 regression, in the shape the issue proves it: two instances released
/// from one barrier, over a **stale** socket file so the liveness probe cannot
/// save them (it answers "reclaimable" for both). Exactly one binds and the
/// other is refused by the **lock** — not by the probe, and not by a losing
/// `bind`. Before the fix both unlinked and both bound, and the loser of the
/// bind race owned the path while the winner logged "plugin host listening"
/// and accepted nothing.
///
/// Every assertion here is deterministic *given* the lock (the loser can never
/// reach the probe), which is what makes the whole set falsifiable: with the
/// lock removed, the loser lands in `AlreadyLive` or `Failed` depending on how
/// the two threads interleave, and both are red. Repeated because a race that
/// resolves differently per run must be sampled. Hermetic: two current-thread
/// runtimes on two OS threads (the dev-dependency tokio has no
/// `rt-multi-thread`), a scratch dir.
#[test]
fn racing_takes_never_both_bind_the_socket() {
    for round in 0..16 {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin.sock");

        // Leave a STALE socket file behind: `socket_in_use` must say
        // "reclaimable" to both racers, so only the lock can order them.
        //
        // "Stale" is a precondition this round has to **establish**, not
        // assume: the listener's teardown is not synchronous with `drop`, so
        // under load the racers could still find it live and both stand down
        // as `AlreadyLive`/`Locked` with nobody bound — which is exactly how
        // this test flaked at ≈2 % (see `SETTLE_ATTEMPTS`). Probe until it is
        // really dead before releasing them.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let stale = tokio::net::UnixListener::bind(&path).expect("bind stale socket");
            drop(stale);
            settle_until_socket_is_stale(&path).await;
        });
        drop(rt);
        assert!(
            path.exists(),
            "round {round}: the stale socket file is there"
        );

        // Two barriers: `start` releases both racers into probe→bind together,
        // `settled` keeps each claim (and so its lock) alive until both have
        // been through — otherwise the first racer could finish, drop its lock,
        // and let the second bind legitimately, which is not the race.
        let start = Arc::new(std::sync::Barrier::new(2));
        let settled = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let start = Arc::clone(&start);
                let settled = Arc::clone(&settled);
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("runtime");
                    start.wait();
                    let claim = rt.block_on(take_socket(&path));
                    let outcome = match &claim {
                        Ok(SocketClaim::Bound(..)) => RaceOutcome::Bound(
                            std::fs::metadata(&path)
                                .expect("the winner's socket exists")
                                .ino(),
                        ),
                        Ok(SocketClaim::Locked) => RaceOutcome::Locked,
                        Ok(SocketClaim::AlreadyLive) => RaceOutcome::AlreadyLive,
                        Err(e) => RaceOutcome::Failed(e.to_string()),
                    };
                    // Hold the claim (and its lock) past the other racer's turn.
                    settled.wait();
                    drop(claim);
                    outcome
                })
            })
            .collect();

        let outcomes: Vec<RaceOutcome> = handles
            .into_iter()
            .map(|h| h.join().expect("racer thread joins"))
            .collect();

        let winner_inode = outcomes.iter().find_map(|o| match o {
            RaceOutcome::Bound(inode) => Some(*inode),
            _ => None,
        });
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| matches!(o, RaceOutcome::Bound(_)))
                .count(),
            1,
            "round {round}: exactly one racer takes the socket, got {outcomes:?}",
        );
        assert!(
            outcomes.contains(&RaceOutcome::Locked),
            "round {round}: the loser is refused by the lock — never by the probe, \
             and never by a losing bind: {outcomes:?}",
        );
        assert_eq!(
            Some(std::fs::metadata(&path).expect("socket exists").ino()),
            winner_inode,
            "round {round}: the path still names the winner's socket, not a rebind",
        );
    }
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

/// #1045: the enforcement seam treats `OpenUri` like every other effect — an
/// un-capped one is dropped in the reader, exactly as an un-capped
/// `RunCommand` is — and, the half that matters for the trust argument, the two
/// capabilities do **not** substitute for one another in either direction.
///
/// **Falsified** by deleting the `Effect::OpenUri` arm from
/// [`Effect::required_capability`](hytte_plugin_proto::Effect::required_capability)
/// (the match is exhaustive, so it cannot be deleted — it can only be
/// *mis-mapped*, e.g. to `Capability::RunCommand`, which turns the second and
/// third assertions red; that mapping's own exhaustive test lives in
/// `hytte-plugin-proto`, #1058).
#[test]
fn enforce_capabilities_drops_an_uncapped_open_uri() {
    let open = Effect::open_uri(3, "https://pr1ma.darkest.space/agents/argus");

    assert!(
        enforce_capabilities(&[], "p", vec![open.clone()]).is_empty(),
        "a plugin that declared no caps cannot open a link",
    );
    // Holding the highest-trust cap does not imply the narrow one…
    assert!(
        enforce_capabilities(&[Capability::RunCommand], "p", vec![open.clone()]).is_empty(),
        "RunCommand does not stand in for OpenUri",
    );
    // …and holding the narrow one emphatically does not imply the other.
    assert!(
        enforce_capabilities(
            &[Capability::OpenUri],
            "p",
            vec![Effect::run_command(4, vec!["true".into()])],
        )
        .is_empty(),
        "OpenUri does not stand in for RunCommand",
    );

    let kept = enforce_capabilities(&[Capability::Notify, Capability::OpenUri], "p", vec![open]);
    assert_eq!(kept.len(), 1, "the declared cap lets it through");
    assert!(matches!(kept[0], Effect::OpenUri { id: 3, .. }));
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

// ── #1058: the `Render.hidden_on` shape cap ──────────────────────────────────
//
// PR #1068 shipped `hidden_on: Vec<String>` with no length cap of its own; a
// misbehaving plugin could otherwise make `SlotRender` retain (and every
// monitor's reconciler re-compare and `clone_from`) a multi-megabyte connector
// list on every frame — the decode itself is separately bounded by the 16 MiB
// frame limit (review LOW-2). `capped_hidden_on` is the decode/route-time
// guard: at most `MAX_HIDDEN_ON_ENTRIES` names, each at most
// `MAX_HIDDEN_ON_NAME_BYTES`. A violation is not fatal — it degrades to the
// empty set (the card shows on every screen, #1050's own safe default).
//
// `capped_hidden_on` is pure (review MEDIUM-2/MEDIUM-4, #1058 fix round): it
// returns the violation instead of calling `tracing::warn!` itself, so the
// "one warning per connection per violation kind" property is a plain
// return-value assertion — no tracing subscriber, hand-rolled or otherwise,
// needed to pin it.

/// One entry over [`MAX_HIDDEN_ON_ENTRIES`] degrades the whole set to empty
/// and reports the violation exactly once, even across repeated frames from
/// the same connection.
///
/// **Falsified** by dropping the entry-count check from `capped_hidden_on`
/// (the first assertion reds — 65 names come straight through), or by
/// dropping the `violated.insert(..)` gate so every call reports (the third
/// assertion reds — the second frame reports again).
#[test]
fn hidden_on_over_the_entry_cap_becomes_empty_and_warns_once() {
    let entries: Vec<String> = (0..=MAX_HIDDEN_ON_ENTRIES)
        .map(|i| format!("out{i}"))
        .collect();
    assert_eq!(entries.len(), MAX_HIDDEN_ON_ENTRIES + 1);

    let mut violated = HashSet::new();
    let (kept, violation) = capped_hidden_on(entries.clone(), &mut violated);
    assert!(
        kept.is_empty(),
        "one entry over the cap empties the whole set"
    );
    assert!(
        matches!(violation, Some((HiddenOnViolation::TooManyEntries, _))),
        "the first violating frame must report it: {violation:?}",
    );

    // Same connection, same violation, next frame: the cap still applies, but
    // the report does not repeat.
    let (kept_again, violation_again) = capped_hidden_on(entries, &mut violated);
    assert!(kept_again.is_empty(), "the cap keeps applying every frame");
    assert!(
        violation_again.is_none(),
        "a second frame from the same violation kind must not report again: {violation_again:?}",
    );
}

/// One connector name over [`MAX_HIDDEN_ON_NAME_BYTES`] degrades the whole set
/// to empty and reports the violation, even when the entry count itself is
/// fine — a distinct latch slot from the entry-count cap, so tripping this one
/// after already tripping that one still reports.
///
/// **Falsified** by dropping the per-name length check from
/// `capped_hidden_on` (the first assertion reds — the oversized name comes
/// through unchanged).
#[test]
fn hidden_on_with_a_name_over_the_byte_cap_becomes_empty_and_warns() {
    let over_name = "x".repeat(MAX_HIDDEN_ON_NAME_BYTES + 1);
    let entries = vec!["DP-1".to_owned(), over_name];

    let mut violated = HashSet::new();
    // Trip the OTHER cap first, to prove the two slots are independent.
    let many: Vec<String> = (0..=MAX_HIDDEN_ON_ENTRIES)
        .map(|i| format!("o{i}"))
        .collect();
    let (_, first) = capped_hidden_on(many, &mut violated);
    assert!(matches!(
        first,
        Some((HiddenOnViolation::TooManyEntries, _))
    ));

    let (kept, violation) = capped_hidden_on(entries, &mut violated);
    assert!(
        kept.is_empty(),
        "one oversized name empties the whole set, not just that entry",
    );
    assert!(
        matches!(violation, Some((HiddenOnViolation::NameTooLong, _))),
        "a different violation kind must still report, even with the other kind \
         already latched: {violation:?}",
    );
}

/// Right at both caps — [`MAX_HIDDEN_ON_ENTRIES`] entries, each exactly
/// [`MAX_HIDDEN_ON_NAME_BYTES`] bytes — passes through byte-for-byte, in
/// order, with no violation reported. The positive control for the two tests
/// above: it proves the cap is inclusive (`>`, not `>=`) rather than
/// accidentally rejecting a legitimately-sized frame.
#[test]
fn hidden_on_at_the_cap_passes_through_unchanged() {
    let entries: Vec<String> = std::iter::repeat_with(|| "x".repeat(MAX_HIDDEN_ON_NAME_BYTES))
        .take(MAX_HIDDEN_ON_ENTRIES)
        .collect();
    assert_eq!(entries.len(), MAX_HIDDEN_ON_ENTRIES);
    assert!(entries.iter().all(|e| e.len() == MAX_HIDDEN_ON_NAME_BYTES));

    let mut violated = HashSet::new();
    let (kept, violation) = capped_hidden_on(entries.clone(), &mut violated);
    assert_eq!(kept, entries, "at-cap input passes through unchanged");
    assert!(
        violation.is_none(),
        "no violation for an in-bounds frame: {violation:?}",
    );
}

/// MEDIUM-1 (#1058 fix round): the cap's only production call site
/// (`handle_conn`'s reader loop) is exercised end to end, not just the pure
/// function — a 65-entry `hidden_on` sent over a real socketpair connection
/// must reach the mounted card's `SlotRender.hidden_on` **empty**, mirroring
/// `hidden_on_survives_the_wire_into_the_render_mailbox`'s in-bounds
/// counterpart above.
///
/// **Falsified** by replacing `capped_hidden_on(hidden_on, &mut
/// hidden_on_warned)` with a bare `hidden_on` at the route call site — this
/// test reds (the mounted card keeps all 65 entries) while the rest of
/// `cargo test -p trollshell` stays green, which is exactly the gap the
/// review measured.
#[tokio::test]
async fn an_over_cap_hidden_on_reaches_the_mailbox_empty() {
    let (_clock_tx, clock_rx) = watch::channel(None);
    let (_vis_tx, vis_rx) = watch::channel(false);
    let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);
    let bar_center = ctx.bar_center.clone();

    let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
    tokio::spawn(async move { handle_conn(host_end, &ctx).await });

    let (_prd, mut pwr) = plugin_end.into_split();
    write_frame(
        &mut pwr,
        &PluginMsg::Register {
            manifest: Manifest::new("hidden-on-flood", Mount::BarCenter),
        },
    )
    .await
    .expect("send Register");
    let over_cap: Vec<String> = (0..=MAX_HIDDEN_ON_ENTRIES)
        .map(|i| format!("o{i}"))
        .collect();
    write_frame(
        &mut pwr,
        &PluginMsg::Render {
            tree: wire::Node::Label {
                id: Some("t".into()),
                text: "chip".into(),
                classes: vec![],
                tooltip: None,
            },
            panel: None,
            hidden_on: over_cap,
            effects: vec![],
        },
    )
    .await
    .expect("send Render");

    let cards = wait_for_region(&bar_center).await;
    assert!(
        cards[0].hidden_on.is_empty(),
        "an over-cap hidden_on must reach the mailbox empty, not the raw 65 entries",
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
                tooltip: None,
            },
            panel: None,
            effects: vec![],
            hidden_on: Vec::new(),
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
                tooltip: None,
            },
            panel: None,
            effects: vec![],
            hidden_on: Vec::new(),
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
                tooltip: None,
            },
            panel: None,
            effects: vec![Effect::OpenPage(Page::PowerMenu)],
            hidden_on: Vec::new(),
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
                tooltip: None,
            },
            panel: None,
            effects: vec![Effect::OpenPage(Page::PowerMenu)],
            hidden_on: Vec::new(),
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

// The requester/provider capability each datasource effect requires is
// covered by `hytte-plugin-proto`'s own exhaustive
// `required_capability_maps_every_effect` test (#1058); no host-side
// duplicate here any more.

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
                tooltip: None,
            },
            panel: None,
            effects: vec![],
            hidden_on: Vec::new(),
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
                tooltip: None,
            },
            panel: None,
            effects: vec![],
            hidden_on: Vec::new(),
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

/// Serialises every test that renders through `hytte-preem` — **or reads a
/// value derived from the kit's palette** — against the ones that *move* the
/// kit's accent.
///
/// `hytte_preem`'s accent is a process-global `AtomicU32` that **every** widget
/// reads at render time (`style.rs`'s `palette()`), and the harness runs test
/// functions concurrently in one process. A parity test compares two renders
/// taken moments apart; an accent flip landing between them would make them
/// differ for a reason that has nothing to do with the code under test. Every
/// preem test below takes this lock, and so do the two that write the global —
/// [`the_accent_reaches_the_shells_own_preem_surfaces`] and
/// [`an_accent_change_re_tints_a_gl_scope_without_rebuilding_it`].
///
/// # Not only rendering tests (#1005)
///
/// The rule above said "renders through `hytte-preem`", and that wording missed
/// a whole file. `shader_map`'s state cache keys on `theme_values()`, which is
/// `palette_snapshot` — the accent, once removed — so an accent flip landing
/// mid-test is a **cache miss** there rather than a wrong picture, and it
/// surfaced as an extra data-texture upload roughly one run in four under CPU
/// competition. Those tests take this lock now, which is why it is `pub(super)`
/// rather than private to this file. The rule is therefore: *anything whose
/// assertion depends on the kit's palette staying still* takes it.
static PREEM_INK_LOCK: Mutex<()> = Mutex::new(());

/// Take [`PREEM_INK_LOCK`], surviving a poisoning by an unrelated test's panic
/// (the data is `()`, so there is nothing to be inconsistent about).
pub(super) fn preem_ink_lock() -> std::sync::MutexGuard<'static, ()> {
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
///
/// Returns owned bytes so the parity assertions below read against a kit frame
/// unchanged; [`mapped_frame`] is the one that keeps the shared buffer's
/// identity, which is what the #911 sharing tests are about.
fn mapped_pixels(scope: &Scope, node: &wire::Node) -> (u32, u32, Vec<u8>) {
    let (width, height, data) = mapped_frame(scope, node);
    (width, height, data.to_vec())
}

/// [`mapped_pixels`] without flattening the buffer: the `Arc<[u8]>` the host
/// actually handed out, so a test can ask whether two mapping passes shared one
/// allocation (#911).
fn mapped_frame(scope: &Scope, node: &wire::Node) -> (u32, u32, Arc<[u8]>) {
    match to_ui_node(scope, Grants::none(), node) {
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
        tooltip: None,
    }
}

/// The `(width, height, data)` of each `Pixels` child of a mapped row, in order
/// — how the sibling-keying tests read one node's frame out of a multi-node
/// render.
fn mapped_row_pixels(scope: &Scope, node: &wire::Node) -> Vec<(u32, u32, Vec<u8>)> {
    match to_ui_node(scope, Grants::none(), node) {
        UiNode::Box { children, .. } => children
            .into_iter()
            .map(|child| match child {
                UiNode::Pixels {
                    width,
                    height,
                    data,
                    ..
                } => (width, height, data.to_vec()),
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
                    ..vocab::DotMatrixConfig::default()
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

/// #1091: the wire's `dot_px` reaches the kit on **both** dot surfaces, and
/// the resulting chip is the height the issue asked for — 18 px at pitch 2,
/// which is what fits the 32 px bar.
///
/// Parity against a kit oracle built at the same pitch, not just a height
/// assertion: a host that ignored `dot_px` and rendered at the default would
/// still produce *a* frame, so the height is the tell and the bytes are the
/// proof.
///
/// **Falsified** by dropping either pass-through — `Renderer::DotMatrix`'s
/// `dot_px` field or `marquee_strip`'s `.dot_px(…)` — in which case the shell
/// renders 36 px where the plugin asked for 18.
#[test]
fn a_moved_dot_pitch_reaches_the_kit_on_both_dot_surfaces() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("parity-dot-pitch");
    for (px, height) in [(2_u32, 18_u32), (3, 27), (4, 36), (8, 72)] {
        let dm = preem_node(
            Some("dm"),
            vocab::PreemWidget::DotMatrix {
                config: vocab::DotMatrixConfig {
                    style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                    dot_px: px,
                },
                state: vocab::DotMatrixState {
                    text: "12:34".into(),
                },
            },
        );
        let oracle = kit::DotMatrix::new(kit::DisplayStyle::Vfd)
            .dot_px(usize::try_from(px).expect("pitch fits usize"))
            .render("12:34");
        let mapped = mapped_pixels(&scope, &dm);
        assert_eq!(mapped.1, height, "dot matrix at dot_px {px} is {height} px");
        assert_eq!(mapped, kit_pixels(&oracle), "dot-matrix parity at {px}");

        let text = "SCROLLING MARQUEE TEST";
        let mq = preem_node(
            Some("mq"),
            vocab::PreemWidget::Marquee {
                config: vocab::MarqueeConfig {
                    style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                    window_px: 192,
                    gap_dots: 6,
                    dot_px: px,
                    speed_dots_per_sec: 0.0,
                },
                state: vocab::MarqueeState { text: text.into() },
            },
        );
        let oracle = kit::Marquee::new(kit::DisplayStyle::Vfd)
            .window_px(192)
            .gap_dots(6)
            .dot_px(usize::try_from(px).expect("pitch fits usize"))
            .render(text);
        let mapped = mapped_pixels(&scope, &mq);
        assert_eq!(mapped.1, height, "marquee at dot_px {px} is {height} px");
        assert_eq!(mapped.0, 192, "…and still the window width it asked for");
        assert_eq!(
            mapped,
            kit_pixels(&oracle.window(0)),
            "marquee parity at {px}"
        );
    }
}

/// A pitch change is a **config** change, so it rebuilds the renderer rather
/// than being folded in as new state — which is what the vocabulary's per-config
/// doc promises and what keeps a 36 px instance from drawing an 18 px frame.
///
/// **Falsified** by hand-writing `config_eq`'s dot arms field by field and
/// forgetting `dot_px`: the shell would keep the old renderer and the chip would
/// never change size. (It compares whole structs precisely so it cannot.)
#[test]
fn a_pitch_change_rebuilds_the_instance() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("rebuild-dot-pitch");
    let at = |px: u32| {
        preem_node(
            Some("dm"),
            vocab::PreemWidget::DotMatrix {
                config: vocab::DotMatrixConfig {
                    style: vocab::StyleRef::new(vocab::StyleName::Lcd),
                    dot_px: px,
                },
                state: vocab::DotMatrixState { text: "88".into() },
            },
        )
    };
    let wide = mapped_pixels(&scope, &at(4));
    let narrow = mapped_pixels(&scope, &at(2));
    assert_eq!(wide.1, 36);
    assert_eq!(
        narrow.1, 18,
        "the same node id at a new pitch must re-render, not reuse the 36 px instance",
    );
    // …and back up again, so the rebuild is not one-way.
    assert_eq!(mapped_pixels(&scope, &at(4)), wide);
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
                ..vocab::MarqueeConfig::default()
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

// ── the GL arm (#893 stage B) ────────────────────────────────────────────────
//
// **CI has no OpenGL.** `nix flake check`'s system-tests bucket runs
// `xvfb-run` in a sandbox with no `/dev/dri` and no mesa in the closure, so
// nothing here draws a pixel: what these hold is everything up to the draw —
// which arm a `Scope` takes, what node it emits, the uniforms in it, and the
// animation state machine that decides when the frame clock parks. The pixels,
// the parity delta and the `--areas 8` budget are live-verify
// (`docs/live-verify.md`).

/// Map `node` and take the GL surface out of it, asserting the invariants every
/// `Node::GlSurface` must satisfy — the mirror of [`mapped_frame`] for the GPU
/// arm.
fn mapped_gl(
    scope: &Scope,
    node: &wire::Node,
) -> (u32, u32, Arc<hytte::ui::gl_surface::GlUniforms>) {
    mapped_gl_for(scope, node, super::preem_gl::SCOPE)
}

/// [`mapped_gl`] for a named pipeline — the gauge is the second (#1143), and
/// *which* program a node names is part of what these tests hold: `hytte-ui`
/// keys its compiled resources on it (#979), so a kind emitting another kind's
/// program name would draw the wrong pipeline's shaders and never say so.
fn mapped_gl_for(
    scope: &Scope,
    node: &wire::Node,
    expected: hytte::ui::gl_surface::GlProgram,
) -> (u32, u32, Arc<hytte::ui::gl_surface::GlUniforms>) {
    match to_ui_node(scope, Grants::none(), node) {
        UiNode::GlSurface {
            width,
            height,
            program,
            state,
            classes,
            ..
        } => {
            assert_eq!(
                program, expected,
                "a preem widget's GL arm names its own registered pipeline",
            );
            assert_eq!(
                classes,
                vec!["ts-preem".to_owned()],
                "the GL arm keeps the node's classes, like every other arm",
            );
            (width, height, state)
        }
        other => panic!("the GL arm must map to a GlSurface node, got {other:?}"),
    }
}

/// A `Scope` widget at a known geometry, for the tests below.
fn gl_scope_widget(samples: Vec<f32>) -> vocab::PreemWidget {
    vocab::PreemWidget::Scope {
        config: vocab::ScopeConfig {
            style: vocab::StyleRef::new(vocab::StyleName::Crt),
            cols: 48,
            rows: 24,
            scale: 2,
            persistence: 184,
        },
        state: vocab::ScopeState { samples },
    }
}

/// **The kill switch restores today's bytes exactly.**
///
/// GL is the default (#893, Annika's call), so this is the contract that keeps
/// `TROLLSHELL_PREEM_RENDERER=cpu` a real escape hatch rather than a
/// nearly-the-same second renderer: under the CPU arm the host emits a
/// `Node::Pixels` whose buffer is byte-identical to the kit's own frame — the
/// same assertion `scope_renders_at_parity_with_the_kit_before_and_after_a_decay`
/// makes, restated here as the *switch's* promise and with the node kind
/// pinned too.
///
/// The whole preem test suite runs on the CPU arm by default (see
/// `preem_gl`'s `TEST_ARM`), precisely so every byte-parity assertion in this
/// file keeps measuring the kit rather than a shader CI cannot run.
#[test]
fn the_cpu_arm_still_emits_the_kits_own_bytes_as_a_pixels_node() {
    let _ink = preem_ink_lock();
    let key = Scope::detached("kill-switch-cpu");
    let samples: Vec<f32> = (0..64_u8).map(|i| f32::from(i % 9) / 4.0 - 1.0).collect();
    let node = preem_node(Some("sc"), gl_scope_widget(samples.clone()));

    assert!(
        matches!(
            to_ui_node(&key, Grants::none(), &node),
            UiNode::Pixels { .. }
        ),
        "with the kill switch on, a Scope is a raster surface",
    );
    let mut oracle = kit::Scope::with_size(48, 24).scale(2).persistence(184);
    oracle.advance(&samples);
    assert_eq!(
        mapped_pixels(&key, &node),
        kit_pixels(&oracle.render(kit::DisplayStyle::Crt)),
        "the CPU arm is the kit, byte for byte",
    );
}

/// The GL arm emits a `GlSurface` node the CPU arm would have sized
/// **identically** — `cols * scale` × `rows * scale`, exactly what
/// `Frame::upscale` produces — so flipping the kill switch changes no layout.
///
/// That is not cosmetic: the two nodes are different `NodeKind`s under the same
/// id, so a flip rebuilds the widget, and a rebuild that also resized would
/// reflow the whole card.
#[test]
fn the_gl_arm_emits_a_gl_surface_the_cpu_arm_would_have_sized_identically() {
    let _ink = preem_ink_lock();
    let samples: Vec<f32> = vec![0.0, 0.5, -0.5, 1.0];
    let node = preem_node(Some("sc"), gl_scope_widget(samples.clone()));

    let cpu = Scope::detached("gl-size-cpu");
    let (cpu_w, cpu_h, _) = mapped_pixels(&cpu, &node);

    super::preem_gl::with_gl_arm(|| {
        let gl = Scope::detached("gl-size-gl");
        let (gl_w, gl_h, uniforms) = mapped_gl(&gl, &node);
        assert_eq!(
            (gl_w, gl_h),
            (cpu_w, cpu_h),
            "same natural size on both arms"
        );
        assert_eq!((gl_w, gl_h), (96, 48), "cols * scale by rows * scale");
        assert_eq!(
            uniforms.grid,
            (48, 24),
            "the shader's offscreen passes run at the pre-upscale grid",
        );
        assert_eq!(
            uniforms.step_seq, 1,
            "the debut batch is stamped as step 0, so the counter starts at 1",
        );
        assert!(
            uniforms
                .data
                .as_ref()
                .is_some_and(|held| held.as_ref() == samples.as_slice()),
            "the batch travels as the data strip",
        );
    });
}

/// **`animates()` is the same expression on both arms**, so #926's frame-clock
/// park and unpark behave identically and `pump.rs` needed no change at all.
///
/// Driven through the whole settle sequence rather than spot-checked: a scope
/// animates while a batch is pending, keeps animating while the trail fades,
/// and goes quiet after exactly `scope_settle_steps(persistence)` idle steps.
/// The two arms are stepped in lockstep and compared at every step, so a
/// divergence anywhere in the sequence — not just at the ends — fails.
///
/// **Falsified** by changing either arm's `animates()` expression, or by
/// dropping the `idle` bookkeeping from `ScopeGl`'s `advance`.
#[test]
fn both_scope_arms_animate_and_park_in_lockstep() {
    let _ink = preem_ink_lock();
    let node = preem_node(Some("sc"), gl_scope_widget(vec![0.0, 1.0, -1.0]));
    let cpu = Scope::detached("park-cpu");
    let gl = Scope::detached("park-gl");

    let _ = to_ui_node(&cpu, Grants::none(), &node);
    super::preem_gl::with_gl_arm(|| {
        let _ = to_ui_node(&gl, Grants::none(), &node);
    });

    // `persistence: 184` settles in 17 steps; walk past that so the parked tail
    // is compared too.
    for step in 0..40 {
        let cpu_animates = preem_render::any_animating_in(std::slice::from_ref(&cpu));
        let gl_animates = preem_render::any_animating_in(std::slice::from_ref(&gl));
        assert_eq!(
            cpu_animates, gl_animates,
            "step {step}: the two arms disagree about whether the scope animates",
        );
        let moved = preem_render::advance_all(preem_render::ANIM_STEP_SECS);
        assert_eq!(
            moved.contains(&cpu),
            moved.contains(&gl),
            "step {step}: the two arms disagree about whether the scope moved",
        );
    }
    assert!(
        !preem_render::any_animating_in(std::slice::from_ref(&gl)),
        "a fully faded GL trail stops asking for repaints",
    );
}

/// **A second wire frame re-arms a parked GL scope.**
///
/// This is the arm the design spec singled out as the PR's review checklist:
/// `Renderer::update` ends in a `_ => {}` catch-all, so a missing `ScopeGl`
/// pattern there is not a compile error — it is a scope that queues nothing,
/// never wakes up, and shows its debut batch for the rest of the session.
///
/// It needed its own test, and the discovery is worth writing down: deleting
/// that arm left **every other GL test on this branch green**, including the
/// one whose name says it walks the animation state machine. The reason is that
/// the others map one wire frame and then only advance the clock — and
/// `apply`'s `same_widget` short-circuit means `update` is never reached on a
/// re-map of an unchanged frame. Only a *different* batch goes through it, and
/// only after the trail has parked is the difference observable.
///
/// **Falsified** by dropping `Self::ScopeGl` from `Renderer::update`'s `Scope`
/// arm: the scope stays parked and the assertion below goes red.
#[test]
fn a_new_batch_re_arms_a_parked_gl_scope() {
    let _ink = preem_ink_lock();
    super::preem_gl::with_gl_arm(|| {
        let key = Scope::detached("gl-requeue");
        let node = preem_node(Some("sc"), gl_scope_widget(vec![0.5, -0.5]));
        let (_, _, debut) = mapped_gl(&key, &node);
        assert_eq!(debut.step_seq, 1, "the debut batch");

        // Past the 17-step settle at persistence 184, so the trail is black and
        // the renderer has stopped asking for repaints.
        for _ in 0..40 {
            let _ = preem_render::advance_all(preem_render::ANIM_STEP_SECS);
        }
        assert!(
            !preem_render::any_animating_in(std::slice::from_ref(&key)),
            "the premise: a faded trail parks",
        );
        let (_, _, parked) = mapped_gl(&key, &node);

        // A *different* batch — the only thing that reaches `Renderer::update`.
        let second = vec![-0.25f32, 0.75, 0.25];
        let next = preem_node(Some("sc"), gl_scope_widget(second.clone()));
        let _ = mapped_gl(&key, &next);
        assert!(
            preem_render::any_animating_in(std::slice::from_ref(&key)),
            "a new batch wakes the scope back up",
        );

        assert!(
            advanced(preem_render::ANIM_STEP_SECS),
            "the queued batch is stamped by the next step",
        );
        let (_, _, after) = mapped_gl(&key, &next);
        assert_eq!(
            after.step_seq,
            parked.step_seq + 1,
            "one step ran, and the counter did not restart",
        );
        assert!(
            after
                .data
                .as_ref()
                .is_some_and(|held| held.as_ref() == second.as_slice()),
            "the new batch reaches the shader as the data strip",
        );
    });
}

/// `step_seq` is **monotonic** and advances one per animation step — the
/// idempotence key the surface replays against. A mapping pass on its own
/// advances nothing (that is the multi-monitor rule), and a repeat pass hands
/// back the *same* `Arc`, which is what makes `GlSurface::set_state`'s guard a
/// pointer compare.
#[test]
fn step_seq_advances_once_per_step_and_never_on_a_mapping_pass() {
    let _ink = preem_ink_lock();
    super::preem_gl::with_gl_arm(|| {
        let key = Scope::detached("step-seq");
        let node = preem_node(Some("sc"), gl_scope_widget(vec![0.25, -0.25]));

        let (_, _, first) = mapped_gl(&key, &node);
        assert_eq!(first.step_seq, 1, "the debut batch is one step");

        // The second monitor's pass over the same wire frame: no advance, and
        // the very same allocation.
        let (_, _, again) = mapped_gl(&key, &node);
        assert_eq!(again.step_seq, 1, "a mapping pass advances nothing");
        assert!(
            Arc::ptr_eq(&first, &again),
            "a re-map shares the cached uniforms, so the surface settles on a pointer compare",
        );

        let mut previous = 1;
        for step in 0..5 {
            assert!(advanced(preem_render::ANIM_STEP_SECS), "step {step} moved");
            let (_, _, now) = mapped_gl(&key, &node);
            assert_eq!(
                now.step_seq,
                previous + 1,
                "step {step}: one animation step is one step_seq",
            );
            previous = now.step_seq;
        }
    });
}

/// One tick carrying a long stall advances at most `MAX_CATCHUP_STEPS` — the
/// same clamp the CPU arm takes, and the reason a resume from suspend costs a
/// bounded hop rather than the interval it spanned.
///
/// The GL arm has a *second* clamp downstream (`hytte-ui`'s
/// `MAX_STEPS_PER_RENDER`, for a surface that was unmapped while its state kept
/// running); this is the upstream one, which is the live guard on the
/// production path.
#[test]
fn a_stalled_tick_advances_the_gl_arm_by_exactly_the_catch_up_clamp() {
    let _ink = preem_ink_lock();
    super::preem_gl::with_gl_arm(|| {
        let key = Scope::detached("catch-up");
        // A fading persistence with a 17-step settle, so none of the eight
        // steps the clamp allows is cut short by the trail going quiet — the
        // clamp is the only thing that bounds this, which is the point.
        let node = preem_node(Some("sc"), gl_scope_widget(vec![0.5, -0.5]));
        let (_, _, debut) = mapped_gl(&key, &node);
        assert_eq!(debut.step_seq, 1, "the debut batch");

        // Ten seconds of `dt` in one tick — two hundred steps' worth, which is
        // the shape of a resume from suspend.
        assert!(advanced(10.0), "a stalled tick still advances the trail");
        let (_, _, after) = mapped_gl(&key, &node);
        assert_eq!(
            after.step_seq,
            1 + u64::from(preem_render::MAX_CATCHUP_STEPS),
            "a stalled tick replays the clamp, never the stall's length",
        );
    });
}

/// The same clamp holds the **CPU** arm, so a stall costs the two arms the same
/// amount of animation. Stated as a comparison rather than a second constant:
/// the two arms diverging here would make a kill-switch flip change how a
/// resume from suspend looks.
#[test]
fn both_scope_arms_take_the_same_catch_up_clamp() {
    let _ink = preem_ink_lock();
    let node = preem_node(Some("sc"), gl_scope_widget(vec![0.5, -0.5]));
    let cpu = Scope::detached("catch-up-cpu");
    let gl = Scope::detached("catch-up-gl");
    let _ = to_ui_node(&cpu, Grants::none(), &node);
    super::preem_gl::with_gl_arm(|| {
        let _ = to_ui_node(&gl, Grants::none(), &node);
    });

    let moved = preem_render::advance_all(10.0);
    assert!(moved.contains(&cpu) && moved.contains(&gl));
    // The CPU arm has no counter to read, so the shared property is asserted
    // through the one both arms expose: how much of the trail is left. Eight
    // steps of `184/256` decay from a full-intensity beam, and the two arms
    // agree about *that* because they run the same loop.
    let (_, _, gl_state) = super::preem_gl::with_gl_arm(|| mapped_gl(&gl, &node));
    assert_eq!(
        gl_state.step_seq,
        1 + u64::from(preem_render::MAX_CATCHUP_STEPS),
    );
    let mut oracle = kit::Scope::with_size(48, 24).scale(2).persistence(184);
    oracle.advance(&[0.5, -0.5]);
    for _ in 0..preem_render::MAX_CATCHUP_STEPS {
        oracle.advance(&[]);
    }
    assert_eq!(
        mapped_pixels(&cpu, &node),
        kit_pixels(&oracle.render(kit::DisplayStyle::Crt)),
        "the CPU arm replayed the same eight steps the GL arm counted",
    );
}

/// **A failed GL context drops the instance to the CPU kit** — per the spec's
/// third fallback case, and the reason falling back is free for a kit widget:
/// it *has* a CPU implementation, and that implementation is the reference the
/// GL arm is measured against, so a blank chip would be strictly worse.
///
/// Driven through `hytte-ui`'s `abandon_gl`, which is the seam a host tests
/// this through without a display server — "a GL-less display" is not something
/// a hermetic test can arrange, but "behave as if the context had failed" is
/// one call. The latch is thread-local and never cleared, so this test's own
/// thread is the blast radius.
///
/// **Falsified** by dropping the `gl_lost` clause from `apply`: the instance
/// keeps its `ScopeGl` renderer, `to_ui_node` keeps emitting a `GlSurface`
/// node that can never draw, and the chip stays blank for the session.
#[test]
fn a_failed_gl_context_rebuilds_the_scope_onto_the_cpu_kit() {
    let _ink = preem_ink_lock();
    super::preem_gl::with_gl_arm(|| {
        let key = Scope::detached("context-lost");
        let samples: Vec<f32> = vec![0.0, 0.75, -0.75];
        let node = preem_node(Some("sc"), gl_scope_widget(samples.clone()));

        assert!(
            matches!(
                to_ui_node(&key, Grants::none(), &node),
                UiNode::GlSurface { .. }
            ),
            "the GL arm is chosen while a context is still possible",
        );
        let before = preem_render::probe(&key, Some("sc")).expect("the instance exists");

        hytte::ui::gl_surface::abandon_gl("no GL in this test");

        assert!(
            matches!(
                to_ui_node(&key, Grants::none(), &node),
                UiNode::Pixels { .. }
            ),
            "a lost context drops the scope to the raster arm",
        );
        let after = preem_render::probe(&key, Some("sc")).expect("the instance survives");
        assert_eq!(
            after.0,
            before.0 + 1,
            "the fallback is a rebuild, not a silent no-op",
        );

        // …and the raster it produces is the kit's, from a fresh phosphor —
        // the GL arm never drew a trail there is anything to inherit.
        let mut oracle = kit::Scope::with_size(48, 24).scale(2).persistence(184);
        oracle.advance(&samples);
        assert_eq!(
            mapped_pixels(&key, &node),
            kit_pixels(&oracle.render(kit::DisplayStyle::Crt)),
            "the fallback is the kit, byte for byte",
        );
    });
}

/// **A scope that was never going to animate again still falls back.**
///
/// The animating case is covered above; this is the one the failure hook's
/// original comment asserted away ("a settled one has nothing on screen to
/// correct", which is backwards — a settled *GL* scope whose context failed has
/// nothing on screen at all, and that is exactly what the fallback exists for).
///
/// `persistence: 256` is the kit's own infinite-persistence value, legal on the
/// wire, and it makes `fades` false: the renderer answers `animates()` with
/// `false` from the moment it is built, #926's clock parks, and no mapping pass
/// is ever coming on its own. So `apply`'s `gl_lost` rebuild — which handles
/// the animating case — is never re-entered, and the chip would stay blank
/// until the plugin sent another frame, which a settled widget may never do.
///
/// **Falsified** by dropping the `rebuild_gl_renderers_on_cpu()` call from
/// `preem_gl::install`'s hook: the instance keeps its `ScopeGl` renderer with
/// no rebuild, and the `builds` assertion goes red.
#[test]
fn a_settled_gl_scope_falls_back_without_waiting_for_a_frame_that_never_comes() {
    let _ink = preem_ink_lock();
    // The hook under test, installed the way `plugins::install` installs it.
    super::preem_gl::install();
    super::preem_gl::with_gl_arm(|| {
        let key = Scope::detached("context-lost-settled");
        let samples: Vec<f32> = vec![0.0, 0.6, -0.6];
        let node = preem_node(
            Some("sc"),
            vocab::PreemWidget::Scope {
                config: vocab::ScopeConfig {
                    style: vocab::StyleRef::new(vocab::StyleName::Crt),
                    cols: 48,
                    rows: 24,
                    scale: 2,
                    // The kit's ceiling: an infinite-persistence phosphor.
                    persistence: 256,
                },
                state: vocab::ScopeState {
                    samples: samples.clone(),
                },
            },
        );

        assert!(
            matches!(
                to_ui_node(&key, Grants::none(), &node),
                UiNode::GlSurface { .. }
            ),
            "the GL arm is chosen while a context is still possible",
        );
        // The premise, and the reason the animating path cannot save this one.
        assert!(
            !preem_render::any_animating_in(std::slice::from_ref(&key)),
            "an infinite-persistence scope never animates, so nothing will \
             re-map it on its own",
        );
        let before = preem_render::probe(&key, Some("sc")).expect("the instance exists");

        hytte::ui::gl_surface::abandon_gl("no GL in this test");

        // The hook rebuilt it **without** a mapping pass — that is the half a
        // parked clock would otherwise have withheld for ever.
        let after = preem_render::probe(&key, Some("sc")).expect("the instance survives");
        assert_eq!(
            after.0,
            before.0 + 1,
            "the failure hook rebuilt the renderer itself, not the next re-map",
        );
        assert_eq!(
            after.1, before.1,
            "…and did it without an apply, so no widget state was touched",
        );

        // …and what it now produces is the kit's own frame, from a fresh
        // phosphor: the GL arm never drew a trail there was anything to inherit.
        assert!(
            matches!(
                to_ui_node(&key, Grants::none(), &node),
                UiNode::Pixels { .. }
            ),
            "a lost context drops even a settled scope to the raster arm",
        );
        let mut oracle = kit::Scope::with_size(48, 24).scale(2).persistence(256);
        oracle.advance(&samples);
        assert_eq!(
            mapped_pixels(&key, &node),
            kit_pixels(&oracle.render(kit::DisplayStyle::Crt)),
            "the fallback is the kit, byte for byte",
        );
    });
}

/// An accent change re-tints a GL scope through the **uniforms**, with no
/// renderer rebuild — the same live-re-tint contract (#396/#862) the CPU arm
/// has, reached by the same `invalidate_cached_frames` call.
///
/// `ScopeGl` resolves its palette at mapping time exactly as the CPU `Scope`
/// does, so it needs no entry in `invalidate_cached_frames`' `TextBox`
/// special case — this is what says so.
///
/// **Falsified** two ways, one per assertion. Baking the palette into
/// `Renderer::ScopeGl` at build time stops the ink moving; adding `ScopeGl` to
/// `invalidate_cached_frames`' `TextBox` rebuild branch resets `step_seq` to
/// `1` and wipes the trail.
///
/// The `step_seq` assertion is load-bearing and the rebuild **counter alone is
/// not**: `invalidate_cached_frames` assigns `instance.renderer` directly and
/// never touches `instance.builds`, so a rebuild taken on that path is
/// completely invisible to `probe`. A mutation run against an earlier version
/// of this test — which checked only the counter — stayed green with the
/// rebuild added, which is exactly the failure it was written to catch.
#[test]
fn an_accent_change_re_tints_a_gl_scope_without_rebuilding_it() {
    let _ink = preem_ink_lock();
    super::preem_gl::with_gl_arm(|| {
        let key = Scope::detached("gl-accent");
        // A role-less style takes the session accent, which is what moves here.
        let node = preem_node(
            Some("sc"),
            vocab::PreemWidget::Scope {
                config: vocab::ScopeConfig {
                    style: vocab::StyleRef::default(),
                    cols: 16,
                    rows: 8,
                    scale: 1,
                    persistence: 184,
                },
                state: vocab::ScopeState { samples: vec![0.5] },
            },
        );

        tint_in_process_surfaces(Some([0x00, 0xff, 0x00, 0xff]));
        let (_, _, debut) = mapped_gl(&key, &node);
        // Run the trail on, so `step_seq` is somewhere a rebuild could not
        // land on by accident: a fresh `ScopeGl` starts at exactly 1.
        for _ in 0..3 {
            assert!(advanced(preem_render::ANIM_STEP_SECS));
        }
        let (_, _, green) = mapped_gl(&key, &node);
        assert_eq!(green.step_seq, debut.step_seq + 3, "three steps ran");
        let builds = preem_render::probe(&key, Some("sc"))
            .expect("the instance exists")
            .0;

        tint_in_process_surfaces(Some([0xff, 0x00, 0xff, 0xff]));
        let (_, _, magenta) = mapped_gl(&key, &node);

        assert_ne!(
            green.values, magenta.values,
            "the accent reaches the shader as a uniform",
        );
        // The observable consequence of a rebuild, and the reason this is the
        // assertion rather than the counter: a rebuilt `ScopeGl` restarts its
        // step count, which restarts the phosphor — a visible trail reset every
        // time the desktop accent moves.
        assert_eq!(
            magenta.step_seq, green.step_seq,
            "a re-tint keeps the animation state; a rebuild would restart it",
        );
        assert_eq!(
            preem_render::probe(&key, Some("sc"))
                .expect("the instance exists")
                .0,
            builds,
            "a re-tint is a cache drop, never a renderer rebuild",
        );
        tint_in_process_surfaces(None);
    });
}

// ── the `Gauge` GL arm (#1143) ───────────────────────────────────────────────
//
// The same three shell-side contracts the `Scope` arm has above, on the same
// seam: which arm a build takes, what node it emits, and what a failed context
// does about it. The pixels are the parity harness's and live-verify's.

/// One of **every** `PreemWidget` variant, at its vocabulary defaults.
///
/// The `match` below is the forcing function and is deliberately exhaustive
/// with no catch-all: a widget kind added to the vocabulary does not compile
/// until it is named here, and the length assertion then stays red until it has
/// an actual sample in the list.
fn every_preem_widget() -> Vec<vocab::PreemWidget> {
    use vocab::PreemWidget as W;
    let all = vec![
        W::DotMatrix {
            config: vocab::DotMatrixConfig::default(),
            state: vocab::DotMatrixState::default(),
        },
        W::SevenSeg {
            config: vocab::SevenSegConfig::default(),
            state: vocab::SevenSegState::default(),
        },
        W::TextBox {
            config: vocab::TextBoxConfig::default(),
            state: vocab::TextBoxState::default(),
        },
        W::LedStrip {
            config: vocab::LedStripConfig::default(),
            state: vocab::LedStripState::default(),
        },
        W::Marquee {
            config: vocab::MarqueeConfig::default(),
            state: vocab::MarqueeState::default(),
        },
        W::Scope {
            config: vocab::ScopeConfig::default(),
            state: vocab::ScopeState::default(),
        },
        W::Gauge {
            config: vocab::GaugeConfig::default(),
            state: vocab::GaugeState::default(),
        },
        W::FlipBoard {
            config: vocab::FlipBoardConfig::default(),
            state: vocab::FlipBoardState::default(),
        },
    ];
    let mut kinds = 0;
    for widget in &all {
        kinds += match widget {
            W::DotMatrix { .. }
            | W::SevenSeg { .. }
            | W::TextBox { .. }
            | W::LedStrip { .. }
            | W::Marquee { .. }
            | W::Scope { .. }
            | W::Gauge { .. }
            | W::FlipBoard { .. } => 1,
        };
    }
    assert_eq!(
        kinds, 8,
        "a widget kind was added to the vocabulary without a sample here",
    );
    all
}

/// **Every renderer answers both halves of the GL seam the same way** —
/// `Renderer::is_gl()` and `Renderer::gl_surface()` agree, on every widget kind,
/// under both arms.
///
/// `Renderer::is_gl`'s doc has promised this test since #1143 and #1148's review
/// found the promise was prose: `grep` returned the comment and nothing else.
/// For the two GL kinds that exist today other tests happen to cover both
/// directions, so nothing was broken — but #1144's dot matrix stacks on this
/// branch and its author reads that sentence as a guarantee before adding a
/// third arm.
///
/// The two halves are asked at different call sites and neither fails loudly on
/// its own. An arm answering `true` here and `None` there hands the reconciler
/// no node at all and is never rebuilt onto the kit by the context-failure hook:
/// a permanently blank chip. The reverse — `false` here, `Some` there — draws on
/// the GPU while `apply`'s `gl_lost` check believes it is a raster arm, so a
/// lost context leaves it frozen on its last frame.
///
/// Driven off the *widget* vocabulary rather than off a hand list of `Renderer`
/// variants, which is what makes it cover a new GL arm the day `build` starts
/// returning one: there is nothing here for #1144 to remember to update.
///
/// **Falsified** three ways, each a one-line edit to `preem_render`: drop
/// `GaugeGl` from `is_gl` (the first assertion goes red), drop the `GaugeGl` arm
/// from `gl_surface` so it falls into the `_ => None` catch-all (the same
/// assertion, the other way round), or make `build` never take the GL arm (the
/// last assertion goes red, which is what stops this test from passing
/// vacuously).
#[test]
fn every_gl_renderer_answers_both_halves_of_the_gl_seam() {
    let _ink = preem_ink_lock();
    let widgets = every_preem_widget();

    for widget in &widgets {
        let (is_gl, has_surface) =
            preem_render::gl_seam_for(widget).expect("every vocabulary widget builds");
        assert!(
            !is_gl && !has_surface,
            "{}: the CPU arm draws on neither half of the GL seam, got is_gl={is_gl} \
             gl_surface={has_surface}",
            widget.kind(),
        );
    }

    let mut on_the_gpu = Vec::new();
    super::preem_gl::with_gl_arm(|| {
        for widget in &widgets {
            let (is_gl, has_surface) =
                preem_render::gl_seam_for(widget).expect("every vocabulary widget builds");
            assert_eq!(
                is_gl,
                has_surface,
                "{}: is_gl() says {is_gl} and gl_surface() says {has_surface} — an arm that \
                 answers the two halves differently either draws nothing at all or is never \
                 rebuilt onto the kit when the context goes",
                widget.kind(),
            );
            if is_gl {
                on_the_gpu.push(widget.kind());
            }
        }
    });

    assert!(
        on_the_gpu.contains(&"scope")
            && on_the_gpu.contains(&"gauge")
            && on_the_gpu.contains(&"dot-matrix"),
        "the premise: under the GL arm the three kinds that have one draw on the GPU, got \
         {on_the_gpu:?}",
    );
}

/// A `Gauge` widget at a known geometry, for the tests below. `scale = 1`, so
/// the GL arm's native grid and the kit's logical one are the same number and
/// a size assertion says something about the *mapping* rather than about the
/// upscale.
fn gl_gauge_widget(target: f32) -> vocab::PreemWidget {
    vocab::PreemWidget::Gauge {
        config: vocab::GaugeConfig {
            style: vocab::StyleRef::new(vocab::StyleName::Crt),
            cols: 144,
            rows: 64,
            scale: 1,
            ..vocab::GaugeConfig::default()
        },
        state: vocab::GaugeState { target },
    }
}

/// **The kill switch reaches the gauge too**, and it restores the kit's own
/// bytes exactly — the mirror of
/// `the_cpu_arm_still_emits_the_kits_own_bytes_as_a_pixels_node`, which is the
/// `Scope`'s version of this contract.
///
/// The whole preem suite runs on the CPU arm by default (`preem_gl`'s
/// `TEST_ARM`), which is why every other gauge parity assertion in this file
/// keeps measuring the kit; this one pins the *node kind* as well, so a gauge
/// that silently took the GL arm under the switch would be caught here rather
/// than by a blank chip.
///
/// **Falsified** by dropping the `preem_gl::arm() == Arm::Gl` guard from
/// `build`'s gauge arm: the first assertion reports a `GlSurface`.
#[test]
fn the_cpu_arm_still_emits_the_kits_own_gauge_bytes_as_a_pixels_node() {
    let _ink = preem_ink_lock();
    let key = Scope::detached("gauge-kill-switch-cpu");
    let node = preem_node(Some("gg"), gl_gauge_widget(0.7));

    assert!(
        matches!(
            to_ui_node(&key, Grants::none(), &node),
            UiNode::Pixels { .. }
        ),
        "with the kill switch on, a Gauge is a raster surface",
    );
    let mut oracle = kit::Gauge::with_size(144, 64).scale(1);
    oracle.set_target(0.7);
    assert_eq!(
        mapped_pixels(&key, &node),
        kit_pixels(&oracle.render(kit::DisplayStyle::Crt)),
        "the CPU arm is the kit, byte for byte",
    );
}

/// The GL arm emits a `GlSurface` the CPU arm would have sized **identically**,
/// naming the gauge's own pipeline, with the **native** buffer as its grid.
///
/// The size agreement is the same layout argument the scope's version makes: a
/// kill-switch flip is a node-kind change, so it rebuilds the widget, and a
/// rebuild that also resized would reflow the whole card.
///
/// The *grid* is where the two kinds deliberately differ, and it is the whole of
/// #1090: a `Scope`'s offscreen passes run at the pre-upscale grid, while a
/// `Gauge`'s run at `cols * scale` — the resolution the dial is actually shown
/// at, instead of a quarter of it replicated.
///
/// **Falsified** by pointing `gauge_surface`'s `grid` at `(cols, rows)`: the
/// last assertion goes red while every `scale = 1` parity case stays green.
#[test]
fn the_gl_gauge_emits_its_own_pipeline_at_the_native_grid() {
    let _ink = preem_ink_lock();
    let node = preem_node(Some("gg"), gl_gauge_widget(0.7));

    let cpu = Scope::detached("gauge-gl-size-cpu");
    let (cpu_w, cpu_h, _) = mapped_pixels(&cpu, &node);

    super::preem_gl::with_gl_arm(|| {
        let gl = Scope::detached("gauge-gl-size-gl");
        let (gl_w, gl_h, uniforms) = mapped_gl_for(&gl, &node, super::preem_gl::GAUGE);
        assert_eq!(
            (gl_w, gl_h),
            (cpu_w, cpu_h),
            "same natural size on both arms",
        );
        assert_eq!((gl_w, gl_h), (144, 64), "cols * scale by rows * scale");
        assert_eq!(
            uniforms.grid,
            (144, 64),
            "a gauge's offscreen passes run at the **native** buffer, not the \
             pre-upscale grid the scope uses",
        );
        assert_eq!(
            uniforms.step_seq, 0,
            "a gauge carries no cross-frame GPU state, so nothing counts steps",
        );
        assert!(
            uniforms.data.is_none(),
            "and no data strip: every shape is analytic",
        );
    });
}

/// **`animates()` is the same expression on both gauge arms** — and here that
/// is not a re-derivation but the same field: both hold the one `kit::Gauge`,
/// so #926's frame-clock park cannot depend on which renderer drew the dial.
///
/// Stepped in lockstep through a whole swing and compared at every step, so a
/// divergence anywhere in the sequence fails rather than only at the ends.
///
/// **Falsified** by giving `GaugeGl` its own `animates()` arm that answers
/// `true` (or `false`) unconditionally.
#[test]
fn both_gauge_arms_animate_and_park_in_lockstep() {
    let _ink = preem_ink_lock();
    let node = preem_node(Some("gg"), gl_gauge_widget(0.9));

    let cpu = Scope::detached("gauge-lockstep-cpu");
    let _ = to_ui_node(&cpu, Grants::none(), &node);
    let gl = Scope::detached("gauge-lockstep-gl");
    super::preem_gl::with_gl_arm(|| {
        let _ = to_ui_node(&gl, Grants::none(), &node);
    });

    let cpu_only = std::slice::from_ref(&cpu);
    let gl_only = std::slice::from_ref(&gl);
    assert!(
        preem_render::any_animating_in(cpu_only),
        "the premise: a needle pointed at 0.9 has somewhere to go",
    );
    // Long enough for a 2 Hz spring at 0.5 damping to arrive and stop, plus a
    // tail past the park so the *parked* state is compared too.
    let mut parked_at = None;
    for step in 0..240 {
        let cpu_animates = preem_render::any_animating_in(cpu_only);
        let gl_animates = preem_render::any_animating_in(gl_only);
        assert_eq!(
            cpu_animates, gl_animates,
            "step {step}: the two arms disagree about whether the gauge animates",
        );
        if !cpu_animates && parked_at.is_none() {
            parked_at = Some(step);
        }
        let moved = preem_render::advance_all(preem_render::ANIM_STEP_SECS);
        assert_eq!(
            moved.contains(&cpu),
            moved.contains(&gl),
            "step {step}: the two arms disagree about whether the gauge moved",
        );
    }
    assert!(
        parked_at.is_some(),
        "the needle never settled, so the park was never actually observed",
    );
}

/// A `Gauge` whose GL context fails is rebuilt onto the kit **by the hook**,
/// without waiting for a mapping pass — the `Scope`'s
/// `a_settled_gl_scope_falls_back_without_waiting_for_a_frame_that_never_comes`
/// contract, and the reason `rebuild_gl_renderers_on_cpu` had to stop naming
/// one renderer variant (#1143).
///
/// A **settled** gauge is the case that needs it: a needle already on its
/// target answers `animates()` with `false`, #926's clock parks, `apply`'s
/// `gl_lost` rebuild is never re-entered, and the chip would stay blank until
/// the plugin sent a new target — which a gauge showing a steady reading may
/// never do.
///
/// **Falsified** by making `Renderer::is_gl` answer `false` for `GaugeGl` (the
/// `builds` assertion goes red — the hook walks past the instance), or by
/// dropping the `rebuild_gl_renderers_on_cpu()` call from `preem_gl::install`'s
/// hook.
#[test]
fn a_settled_gl_gauge_falls_back_without_waiting_for_a_frame_that_never_comes() {
    let _ink = preem_ink_lock();
    super::preem_gl::install();
    super::preem_gl::with_gl_arm(|| {
        let key = Scope::detached("gauge-context-lost-settled");
        let node = preem_node(Some("gg"), gl_gauge_widget(0.0));

        assert!(
            matches!(
                to_ui_node(&key, Grants::none(), &node),
                UiNode::GlSurface { .. }
            ),
            "the GL arm is chosen while a context is still possible",
        );
        // The premise, and the reason the animating path cannot save this one:
        // a needle built pointing at its own resting value never moves.
        assert!(
            !preem_render::any_animating_in(std::slice::from_ref(&key)),
            "a settled gauge never animates, so nothing will re-map it",
        );
        let before = preem_render::probe(&key, Some("gg")).expect("the instance exists");

        hytte::ui::gl_surface::abandon_gl("no GL in this test");

        let after = preem_render::probe(&key, Some("gg")).expect("the instance survives");
        assert_eq!(
            after.0,
            before.0 + 1,
            "the failure hook rebuilt the renderer itself, not the next re-map",
        );
        assert_eq!(
            after.1, before.1,
            "…and did it without an apply, so no widget state was touched",
        );

        assert!(
            matches!(
                to_ui_node(&key, Grants::none(), &node),
                UiNode::Pixels { .. }
            ),
            "a lost context drops even a settled gauge to the raster arm",
        );
        let mut oracle = kit::Gauge::with_size(144, 64).scale(1);
        oracle.set_target(0.0);
        assert_eq!(
            mapped_pixels(&key, &node),
            kit_pixels(&oracle.render(kit::DisplayStyle::Crt)),
            "and what it draws is the kit's own frame, byte for byte",
        );
    });
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

// ── the `DotMatrix` GL arm (#1144) ───────────────────────────────────────────
//
// The third kind on the seam, and the last until Annika says otherwise (#865:
// "lets pause after those"). The same shell-side contracts the `Scope` and the
// `Gauge` have above: which arm a build takes, what node it emits, what a
// failed context does about it — plus one of its own, since it is the first GL
// arm whose per-instance GPU state is a *buffer* rather than a number.

/// A `DotMatrix` widget at a known geometry, for the tests below.
fn gl_dot_matrix_widget(text: &str) -> vocab::PreemWidget {
    vocab::PreemWidget::DotMatrix {
        config: vocab::DotMatrixConfig {
            style: vocab::StyleRef::new(vocab::StyleName::Crt),
            dot_px: 4,
        },
        state: vocab::DotMatrixState {
            text: text.to_owned(),
        },
    }
}

/// **The kill switch reaches the dot matrix too**, and it restores the kit's
/// own bytes exactly — the mirror of
/// `the_cpu_arm_still_emits_the_kits_own_gauge_bytes_as_a_pixels_node`.
///
/// The whole preem suite runs on the CPU arm by default (`preem_gl`'s
/// `TEST_ARM`), which is why every other dot-matrix parity assertion in this
/// file keeps measuring the kit; this one pins the *node kind* as well.
///
/// **Falsified** by dropping the `preem_gl::arm() == Arm::Gl` guard from
/// `build`'s dot-matrix arm: the first assertion reports a `GlSurface`.
#[test]
fn the_cpu_arm_still_emits_the_kits_own_dot_matrix_bytes_as_a_pixels_node() {
    let _ink = preem_ink_lock();
    let key = Scope::detached("dot-matrix-kill-switch-cpu");
    let node = preem_node(Some("dm"), gl_dot_matrix_widget("PREEM"));

    assert!(
        matches!(
            to_ui_node(&key, Grants::none(), &node),
            UiNode::Pixels { .. }
        ),
        "with the kill switch on, a DotMatrix is a raster surface",
    );
    assert_eq!(
        mapped_pixels(&key, &node),
        kit_pixels(
            &kit::DotMatrix::new(kit::DisplayStyle::Crt)
                .dot_px(4)
                .render("PREEM")
        ),
        "the CPU arm is the kit, byte for byte",
    );
}

/// The GL arm emits a `GlSurface` the CPU arm would have sized **identically**,
/// naming the dot matrix's own pipeline, carrying the glyph strip, and counting
/// no steps.
///
/// The size agreement is the layout argument the other two kinds make: a
/// kill-switch flip is a node-kind change, so it rebuilds the widget, and a
/// rebuild that also resized would reflow the whole card.
///
/// Unlike the gauge, the grid here is trivially native — there is no `scale` on
/// this widget, the dot pitch is its size knob (#1091) — so what this pins
/// instead is the **strip**: five texels per character, which is what the
/// shader indexes by, and `None` would draw an empty display.
///
/// **Falsified** by handing `dot_matrix_surface` an unencoded line (the strip
/// length assertion), or by pointing the grid at anything but the buffer.
#[test]
fn the_gl_dot_matrix_emits_its_own_pipeline_with_the_glyph_strip() {
    let _ink = preem_ink_lock();
    let node = preem_node(Some("dm"), gl_dot_matrix_widget("PREEM"));

    let cpu = Scope::detached("dot-matrix-gl-size-cpu");
    let (cpu_w, cpu_h, _) = mapped_pixels(&cpu, &node);

    super::preem_gl::with_gl_arm(|| {
        let gl = Scope::detached("dot-matrix-gl-size-gl");
        let (gl_w, gl_h, uniforms) = mapped_gl_for(&gl, &node, super::preem_gl::DOT_MATRIX);
        assert_eq!(
            (gl_w, gl_h),
            (cpu_w, cpu_h),
            "same natural size on both arms",
        );
        // `2*pad + n*6*dot - dot` by `9*dot`, at five characters and pitch 4.
        assert_eq!((gl_w, gl_h), (124, 36));
        assert_eq!(
            uniforms.grid,
            (124, 36),
            "the offscreen passes run at the buffer the kit would have filled",
        );
        assert_eq!(
            uniforms.step_seq, 0,
            "a dot matrix carries no cross-frame GPU state at all",
        );
        assert_eq!(
            uniforms.data.as_ref().map(|strip| strip.len()),
            Some(5 * 5),
            "one texel per glyph column of every character",
        );
    });
}

/// **The glyph strip survives a cache drop** — a re-tint re-runs the mapping,
/// and the line is not walked again.
///
/// This is the property the `Glyphs` field exists for and the only one that can
/// distinguish it from encoding inside `gl_surface`: `invalidate_cached_frames`
/// drops every cached surface (that is what #885's live re-tint does), so the
/// uniform bag is genuinely rebuilt — and the strip inside it must be the same
/// allocation, not an equal one.
///
/// **Falsified** by calling `preem_gl::encode_glyphs` from
/// `Renderer::gl_surface` instead of cloning the field: the `ptr_eq` goes red
/// while every other assertion in this file stays green.
#[test]
fn the_glyph_strip_is_shared_across_mapping_passes() {
    let _ink = preem_ink_lock();
    let node = preem_node(Some("dm"), gl_dot_matrix_widget("88:88"));
    super::preem_gl::with_gl_arm(|| {
        let key = Scope::detached("dot-matrix-strip-sharing");
        let (_, _, first) = mapped_gl_for(&key, &node, super::preem_gl::DOT_MATRIX);
        preem_render::invalidate_cached_frames();
        let (_, _, second) = mapped_gl_for(&key, &node, super::preem_gl::DOT_MATRIX);

        assert!(
            !Arc::ptr_eq(&first, &second),
            "the premise: the cache really was dropped, so this is a fresh bag",
        );
        let (Some(before), Some(after)) = (first.data.as_ref(), second.data.as_ref()) else {
            panic!("both passes carry a strip");
        };
        assert!(
            Arc::ptr_eq(before, after),
            "the line is encoded on a state change, not on a mapping pass",
        );
    });
}

/// …and a **state change** does re-encode it, to the new line.
///
/// The other half of the contract above: sharing that outlived a text change
/// would freeze the display on its first message.
///
/// **Falsified** by dropping the `DotMatrixGl` arm from `Renderer::update`,
/// which the catch-all would then swallow silently.
#[test]
fn a_new_line_re_encodes_the_glyph_strip() {
    let _ink = preem_ink_lock();
    super::preem_gl::with_gl_arm(|| {
        let key = Scope::detached("dot-matrix-strip-update");
        let (_, _, first) = mapped_gl_for(
            &key,
            &preem_node(Some("dm"), gl_dot_matrix_widget("AA")),
            super::preem_gl::DOT_MATRIX,
        );
        let (_, _, second) = mapped_gl_for(
            &key,
            &preem_node(Some("dm"), gl_dot_matrix_widget("AB")),
            super::preem_gl::DOT_MATRIX,
        );
        let (Some(before), Some(after)) = (first.data.as_ref(), second.data.as_ref()) else {
            panic!("both passes carry a strip");
        };
        assert_ne!(
            before.as_ref(),
            after.as_ref(),
            "a new message reaches the shader",
        );
        assert_eq!(
            after.as_ref(),
            super::preem_gl::encode_glyphs("AB")
                .strip
                .expect("a non-empty line carries a strip")
                .as_ref(),
            "…and it is the new line, encoded the one way",
        );
        // Both builds went through `update`, not a rebuild: the config never
        // moved, so the instance is the same one.
        assert_eq!(
            preem_render::probe(&key, Some("dm")).expect("the instance exists"),
            (1, 2),
            "one build, two applies",
        );
    });
}

/// A `DotMatrix` whose GL context fails is rebuilt onto the kit **by the hook**,
/// without waiting for a mapping pass.
///
/// This kind is the strongest case for the hook of the three: a gauge at least
/// animates while its needle swings, and a scope while its trail fades, but a
/// dot matrix **never** animates — `animates()` is a constant `false` — so the
/// clock is parked from the moment it is built and `apply`'s `gl_lost` rebuild
/// is never re-entered. Without the hook the chip stays blank until the plugin
/// sends a new line, which a static readout may never do.
///
/// **Falsified** by making `Renderer::is_gl` answer `false` for `DotMatrixGl`
/// (the `builds` assertion goes red — the hook walks past the instance), or by
/// dropping the `rebuild_gl_renderers_on_cpu()` call from `preem_gl::install`'s
/// hook.
#[test]
fn a_gl_dot_matrix_falls_back_without_waiting_for_a_frame_that_never_comes() {
    let _ink = preem_ink_lock();
    super::preem_gl::install();
    super::preem_gl::with_gl_arm(|| {
        let key = Scope::detached("dot-matrix-context-lost");
        let node = preem_node(Some("dm"), gl_dot_matrix_widget("PREEM"));

        assert!(
            matches!(
                to_ui_node(&key, Grants::none(), &node),
                UiNode::GlSurface { .. }
            ),
            "the GL arm is chosen while a context is still possible",
        );
        // The premise, and it is stronger here than on either other kind.
        assert!(
            !preem_render::any_animating_in(std::slice::from_ref(&key)),
            "a dot matrix never animates, so nothing will ever re-map it",
        );
        let before = preem_render::probe(&key, Some("dm")).expect("the instance exists");

        hytte::ui::gl_surface::abandon_gl("no GL in this test");

        let after = preem_render::probe(&key, Some("dm")).expect("the instance survives");
        assert_eq!(
            after.0,
            before.0 + 1,
            "the failure hook rebuilt the renderer itself, not the next re-map",
        );
        assert_eq!(
            after.1, before.1,
            "…and did it without an apply, so no widget state was touched",
        );

        assert!(
            matches!(
                to_ui_node(&key, Grants::none(), &node),
                UiNode::Pixels { .. }
            ),
            "a lost context drops the display to the raster arm",
        );
        assert_eq!(
            mapped_pixels(&key, &node),
            kit_pixels(
                &kit::DotMatrix::new(kit::DisplayStyle::Crt)
                    .dot_px(4)
                    .render("PREEM")
            ),
            "and what it draws is the kit's own frame, byte for byte",
        );
    });
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

    let _ = to_ui_node(&scope, Grants::none(), &at(0.25));
    assert_eq!(preem_render::probe(&scope, Some("gg")), Some((1, 1)));

    let _ = to_ui_node(&scope, Grants::none(), &at(0.75));
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

/// **#911.** The second monitor's mapping pass takes a *handle* on the frame the
/// first one rasterised, never a copy of it.
///
/// The instance table is keyed by scope and shared across mounts, so a
/// two-output session maps every preem node twice per frame, and a blanket
/// repaint maps every *unchanged* node again on top of that. Returning owned
/// bytes from [`preem_render::Instance::frame`] made each of those a full RGBA
/// clone out of the store — which the reconciler then copied a second time into
/// the surface's texture. Now `UiNode::Pixels.data` is an `Arc<[u8]>`: the extra
/// pass costs a refcount, and `PixelSurface::set_pixels_shared` settles it with
/// an `Arc::ptr_eq` (#907) instead of scanning the buffer.
///
/// Pointer identity is the assertion because it is the one thing a byte-for-byte
/// copy cannot fake — `assert_eq!` on the bytes passes either way, which is
/// exactly why the test above it could not catch this.
#[test]
fn every_monitors_pass_shares_one_frame_allocation() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("shared-frame-911");
    let node = preem_node(
        Some("dm"),
        vocab::PreemWidget::DotMatrix {
            config: vocab::DotMatrixConfig::default(),
            state: vocab::DotMatrixState {
                text: "SHARED".into(),
            },
        },
    );

    let (width, height, first) = mapped_frame(&scope, &node);
    let (again_w, again_h, second) = mapped_frame(&scope, &node);
    assert!(
        !first.is_empty(),
        "the fixture must really rasterise something, or the sharing claim is vacuous",
    );
    assert_eq!(
        (width, height),
        (again_w, again_h),
        "both monitors map one frame",
    );
    assert!(
        Arc::ptr_eq(&first, &second),
        "two monitors mapping one frame must share one buffer, not hold two copies of it",
    );
    // Three monitors, a drawer, a blanket repaint — every further pass is the
    // same allocation until something invalidates the cache.
    let (_, _, third) = mapped_frame(&scope, &node);
    assert!(Arc::ptr_eq(&first, &third));
    assert_eq!(
        Arc::strong_count(&first),
        4,
        "the cache's own handle plus one per pass — no copy anywhere on the path",
    );

    preem_render::forget_scope(&scope);
}

/// The other half of #911: a frame that really did change is a **new**
/// allocation, so the surface's pointer compare correctly misses it and the new
/// pixels reach the screen. A shared buffer that outlived its content would be
/// a frozen widget.
#[test]
fn a_moved_frame_is_a_new_allocation() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("moved-frame-911");
    let node = preem_node(
        Some("mq"),
        vocab::PreemWidget::Marquee {
            config: vocab::MarqueeConfig {
                style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                window_px: 192,
                gap_dots: 6,
                speed_dots_per_sec: 20.0,
                ..vocab::MarqueeConfig::default()
            },
            state: vocab::MarqueeState {
                text: "A LONG SCROLLING MESSAGE".into(),
            },
        },
    );

    let (_, _, before) = mapped_frame(&scope, &node);
    assert!(advanced(0.5), "the marquee must actually have moved");
    let (_, _, after) = mapped_frame(&scope, &node);
    assert!(
        !Arc::ptr_eq(&before, &after),
        "a tick that moved the widget must produce a new buffer, not mutate the shared one",
    );
    assert_ne!(*before, *after, "…and it must really be different pixels");
    // The pre-tick handle is still valid — the surfaces still holding it see
    // the frame they were given, not a half-written one.
    assert_eq!(before.len(), after.len());

    preem_render::forget_scope(&scope);
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
                    ..vocab::DotMatrixConfig::default()
                },
                state: vocab::DotMatrixState { text: "A".into() },
            },
        )
    };

    let _ = to_ui_node(&scope, Grants::none(), &dots(vocab::StyleName::Vfd));
    assert_eq!(preem_render::probe(&scope, Some("w")), Some((1, 1)));

    let _ = to_ui_node(&scope, Grants::none(), &dots(vocab::StyleName::Crt));
    assert_eq!(
        preem_render::probe(&scope, Some("w")),
        Some((2, 2)),
        "a config change must rebuild, not update",
    );

    let _ = to_ui_node(
        &scope,
        Grants::none(),
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

/// Lifecycle, the #931 case: **re-sizing a gauge rebuilds its instance**, and
/// the rebuilt one is the square face the new size asks for.
///
/// The general "a config change rebuilds" rule above already covers the
/// mechanism, so what this pins is the *consequence* — that the shell's gauge
/// really is sized from `cols`/`rows` on every rebuild rather than from a
/// dimension latched at first build. A gauge that kept its original buffer
/// would keep rendering 288×128 here and the surface would be the wrong shape
/// on screen with nothing in CI to say so.
///
/// Both dimensions are asserted, because a square dial is not merely a smaller
/// wide one: at 48×48 the kit centres the face, drops to two subdivisions,
/// drops the counterweight and caps the bloom, and every one of those follows
/// from the buffer these two fields carry.
#[test]
fn a_gauge_resize_rebuilds_the_instance_at_the_new_size() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("gauge-resize");

    let dial = |cols: u32, rows: u32| {
        preem_node(
            Some("g"),
            vocab::PreemWidget::Gauge {
                config: vocab::GaugeConfig {
                    cols,
                    rows,
                    ..vocab::GaugeConfig::default()
                },
                state: vocab::GaugeState { target: 0.5 },
            },
        )
    };
    let pixels = |node: &UiNode| match node {
        UiNode::Pixels { width, height, .. } => (*width, *height),
        other => panic!("a gauge maps to a Pixels node, got {other:?}"),
    };

    let wide = to_ui_node(&scope, Grants::none(), &dial(144, 64));
    assert_eq!(preem_render::probe(&scope, Some("g")), Some((1, 1)));
    assert_eq!(pixels(&wide), (288, 128), "the default face, at ×2");

    let small = to_ui_node(&scope, Grants::none(), &dial(48, 48));
    assert_eq!(
        preem_render::probe(&scope, Some("g")),
        Some((2, 2)),
        "a size change is a config change, so it rebuilds rather than updating",
    );
    assert_eq!(pixels(&small), (96, 96), "…at the square size it asked for");

    // And back again: nothing latches.
    let wide_again = to_ui_node(&scope, Grants::none(), &dial(144, 64));
    assert_eq!(preem_render::probe(&scope, Some("g")), Some((3, 3)));
    assert_eq!(pixels(&wide_again), (288, 128));

    preem_render::forget_scope(&scope);
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
        spacing: 0,
        id: Some("row".into()),
        classes: vec![],
        children,
        tooltip: None,
    };

    let _ = to_ui_node(&scope, Grants::none(), &row(vec![leaf("a"), leaf("b")]));
    assert_eq!(preem_render::instance_count(&scope), 2);

    let _ = to_ui_node(&scope, Grants::none(), &row(vec![leaf("a")]));
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

    let _ = to_ui_node(&scope, Grants::none(), &anon("one"));
    assert_eq!(preem_render::probe(&scope, None), Some((1, 1)));

    let _ = to_ui_node(&scope, Grants::none(), &anon("two"));
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
    let _ = to_ui_node(&scope, Grants::none(), &three);

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
    let _ = to_ui_node(&scope, Grants::none(), &three);
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
    let _ = to_ui_node(&card, Grants::none(), &anon);
    assert_eq!(
        preem_render::anonymous_warnings() - base,
        1,
        "the first anonymous node in a scope warns",
    );
    let _ = to_ui_node(&card, Grants::none(), &anon);
    let _ = to_ui_node(&card, Grants::none(), &anon);
    assert_eq!(
        preem_render::anonymous_warnings() - base,
        1,
        "three frames of the same tree are still one warning",
    );

    // A plugin's two trees are two scopes, and each deserves to hear about its
    // own: the latch is keyed by `Scope`, not by a process-wide flag.
    let panel = Scope::detached("anon-warn-panel");
    let _ = to_ui_node(&panel, Grants::none(), &anon);
    assert_eq!(
        preem_render::anonymous_warnings() - base,
        2,
        "a different tree gets its own line",
    );

    // And the contract-honoring spelling is silent.
    let id_d = Scope::detached("anon-warn-id-d");
    let _ = to_ui_node(&id_d, Grants::none(), &preem_node(Some("dm"), widget()));
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
        tooltip: None,
    };

    let _ = to_ui_node(&scope, Grants::none(), &anon);
    assert_eq!(
        preem_render::anonymous_warnings() - base,
        1,
        "the node's first appearance warns",
    );

    let _ = to_ui_node(&scope, Grants::none(), &nothing);
    assert_eq!(
        preem_render::instance_count(&scope),
        0,
        "the blank frame must really empty the scope, or this test proves nothing",
    );

    // Present → absent → present → absent → present: still one line.
    let _ = to_ui_node(&scope, Grants::none(), &anon);
    let _ = to_ui_node(&scope, Grants::none(), &nothing);
    let _ = to_ui_node(&scope, Grants::none(), &anon);
    assert_eq!(
        preem_render::anonymous_warnings() - base,
        1,
        "three appearances across two preem-less frames are ONE warning, not one per appearance",
    );

    // And an explicit teardown — a card leaving its region, or the drawer
    // closing on a panel — does not re-arm it either.
    preem_render::forget_scope(&scope);
    let _ = to_ui_node(&scope, Grants::none(), &anon);
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
                tooltip: None,
            })
            .collect(),
        tooltip: None,
    }
}

/// The ids of a mapped [`label_tree`]'s children, in order.
fn mapped_label_ids(scope: &Scope, node: &wire::Node) -> Vec<String> {
    match to_ui_node(scope, Grants::none(), node) {
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
    let _ = to_ui_node(&scope, Grants::none(), &over);
    let _ = to_ui_node(&scope, Grants::none(), &over);
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

    let _ = to_ui_node(&scope, Grants::none(), &over);
    let _ = to_ui_node(&scope, Grants::none(), &over);
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
    let _ = to_ui_node(&scope, Grants::none(), &full);
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
        tooltip: None,
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
        tooltip: None,
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
    let mapped = to_ui_node(&scope, Grants::none(), &over);
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

    let _ = to_ui_node(&scope, Grants::none(), &over);
    let _ = to_ui_node(&scope, Grants::none(), &over);
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
    let mapped = to_ui_node(
        &buttons,
        Grants::none(),
        &button_chain(wire::MAX_TREE_DEPTH + 1),
    );
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
    let mapped = to_ui_node(&scope, Grants::none(), &exact);
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
    let _ = to_ui_node(&shared, Grants::none(), &clash);
    let _ = to_ui_node(&distinct, Grants::none(), &control);
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

    let _ = to_ui_node(&shared, Grants::none(), &clash);
    let _ = to_ui_node(&shared, Grants::none(), &clash);
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
        tooltip: None,
    };

    let _ = to_ui_node(&card, Grants::none(), &mixed);
    // A second pass over the same tree — what a second monitor does.
    let _ = to_ui_node(&card, Grants::none(), &mixed);
    // The same ids in the plugin's *other* tree.
    let _ = to_ui_node(&panel, Grants::none(), &mixed);

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
    let _ = to_ui_node(&scope, Grants::none(), &tree);
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
        let _ = to_ui_node(&scope, Grants::none(), &tree);
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

    let degraded =
        preem_render::with_unsupported_widgets(|| to_ui_node(&scope, Grants::none(), &node));
    assert_eq!(
        degraded,
        UiNode::Pixels {
            id: Some("x".into()),
            width: 0,
            height: 0,
            data: Arc::from(&[][..]),
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
                    ..vocab::MarqueeConfig::default()
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
    let _ = to_ui_node(&scope, Grants::none(), &pure);
    assert!(
        !preem_render::any_animating(),
        "a static dot matrix must not keep the animation clock awake",
    );

    let _ = to_ui_node(&scope, Grants::none(), &marquee(20.0));
    assert!(
        preem_render::any_animating(),
        "a scrolling marquee is what the clock exists for",
    );

    // `0.0` (and, per the vocabulary, a non-finite value) parks the message.
    let _ = to_ui_node(&scope, Grants::none(), &marquee(0.0));
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
    let _ = to_ui_node(&three, Grants::none(), &node);
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
    let _ = to_ui_node(&stalled, Grants::none(), &node);
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
        let _ = to_ui_node(scope, Grants::none(), node);
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
        let _ = to_ui_node(&control, Grants::none(), &gauge(0.7, target));
    }
    assert_eq!(
        preem_render::probe(&control, Some("w")),
        Some((1, targets.len().try_into().expect("fits u32"))),
        "the control must build once and update per target, or this test proves nothing",
    );

    // The same, with a NaN in the config: it must behave identically.
    let scope = Scope::detached("nan-config");
    for target in targets {
        let _ = to_ui_node(&scope, Grants::none(), &gauge(f32::NAN, target));
    }
    assert_eq!(
        preem_render::probe(&scope, Some("w")).map(|(builds, _)| builds),
        Some(1),
        "a moving gauge must build its renderer exactly once whatever its config's floats — \
         a rebuild per pass rests the needle every frame, so it never animates",
    );

    // A genuine config change must still rebuild — the tolerance above must not
    // have been bought by making every config compare equal.
    let _ = to_ui_node(&scope, Grants::none(), &gauge(0.9, 0.5));
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
    let _ = to_ui_node(&scope, Grants::none(), &strip(None));
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
    let _ = to_ui_node(&slow, Grants::none(), &slow_node);
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
    let _ = to_ui_node(&quick, Grants::none(), &quick_node);
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
/// every wire node re-mapped, every preem instance's cached frame `Arc`-cloned
/// out of the store — for every plugin, on every monitor, 20× a second, legacy
/// self-rasterising plugins included. Since #907 the *upload* at the end of that
/// is no longer part of the bill: `hytte-ui`'s `PixelSurface::set_pixels_shared`
/// keeps the last accepted buffer and returns without touching GTK when the
/// frame is identical. This narrowing is the second guard, and it is the one
/// that skips the walk rather than paying for it and discarding the result.
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
                    ..vocab::MarqueeConfig::default()
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
    let _ = to_ui_node(&forward, Grants::none(), &node(20.0));
    assert!(advanced(0.5));
    assert_eq!(
        mapped_pixels(&forward, &node(20.0)),
        kit_pixels(&oracle.window(10)),
        "a positive speed raises the offset, which is the kit's own \
         monotonically-increasing-counter direction",
    );

    let backward = Scope::detached("marquee-backward");
    let _ = to_ui_node(&backward, Grants::none(), &node(-20.0));
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
        Grants::none(),
        &preem_node(
            Some("mq"),
            vocab::PreemWidget::Marquee {
                config: vocab::MarqueeConfig {
                    style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                    window_px: 192,
                    gap_dots: 6,
                    speed_dots_per_sec: 20.0,
                    ..vocab::MarqueeConfig::default()
                },
                state: vocab::MarqueeState {
                    text: "A LONG SCROLLING MESSAGE".into(),
                },
            },
        ),
    );
    let _ = to_ui_node(
        &still,
        Grants::none(),
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
                ..vocab::MarqueeConfig::default()
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

    let _ = to_ui_node(&scope, Grants::none(), &gauge(0.9));
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
    let _ = to_ui_node(&scope, Grants::none(), &gauge(0.1));
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
    let _ = to_ui_node(&mine, Grants::none(), &node);
    let _ = to_ui_node(&theirs, Grants::none(), &node);

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
    let _ = to_ui_node(&scope, Grants::none(), &node);
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
    let _ = to_ui_node(&scope, Grants::none(), &node);
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
    let _ = to_ui_node(&scope, Grants::none(), &node);
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
    let _ = to_ui_node(&scope, Grants::none(), &gauge);
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
            config: vocab::DotMatrixConfig {
                style,
                ..vocab::DotMatrixConfig::default()
            },
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

// ── #935: a resolved role is offered to the skin, an author's pin is not ─────

/// libadwaita's six status colors — `@success_color`, `@warning_color` and
/// `@error_color` under each scheme, as the installed 1.9.3 resolves them
/// (the table on #935).
///
/// Hard-coded for the same reason `set_role_inks` exists at all: the hermetic
/// test binary has no display, so `resolve_role_inks` returns every color unset
/// and each role would — correctly, by its documented fallback — degrade to the
/// session accent, proving nothing about admission.
const LIBADWAITA_ROLES: [(&str, vocab::AccentRole, kit::Rgba); 6] = [
    (
        "dark @success_color",
        vocab::AccentRole::Success,
        [0x64, 0xec, 0xa5, 0xff],
    ),
    (
        "dark @warning_color",
        vocab::AccentRole::Warning,
        [0xff, 0xc1, 0x3f, 0xff],
    ),
    (
        "dark @error_color",
        vocab::AccentRole::Error,
        [0xff, 0x87, 0x7b, 0xff],
    ),
    (
        "light @success_color",
        vocab::AccentRole::Success,
        [0x00, 0x7c, 0x3d, 0xff],
    ),
    (
        "light @warning_color",
        vocab::AccentRole::Warning,
        [0x90, 0x54, 0x00, 0xff],
    ),
    (
        "light @error_color",
        vocab::AccentRole::Error,
        [0xc3, 0x00, 0x00, 0xff],
    ),
];

/// What each of [`LIBADWAITA_ROLES`] becomes on the `Lcd`, byte for byte, in the
/// same order — the ramp's answer against the skin's own olive field.
///
/// Pinned rather than recomputed on purpose: **the stop count changes what the
/// user sees**, and every other assertion here compares against `AA_TEXT` or
/// against the seam and so follows the kit anywhere it goes. This one does not
/// (the kit's own `an_accented_lcd_render_is_pinned_to_its_bytes` makes the same
/// argument for the accent path). Per-platform, like every admitted ink: the
/// ramp is integer-exact but the stop *selection* runs through `f32::powf`.
const LCD_ADMITTED_ROLE_INKS: [kit::Rgba; 6] = [
    [0x2d, 0x47, 0x30, 0xff],
    [0x49, 0x42, 0x20, 0xff],
    [0x53, 0x3d, 0x2f, 0xff],
    [0x14, 0x4c, 0x29, 0xff],
    [0x58, 0x3d, 0x0d, 0xff],
    [0x87, 0x0f, 0x0a, 0xff],
];

/// Drops the memoized role colors (and the kit accent) when it falls out of
/// scope — see [`role_ink_reset`].
pub(super) struct RoleInkReset;

impl Drop for RoleInkReset {
    fn drop(&mut self) {
        // `tint_in_process_surfaces(None)` is the shell's own theme-moved seam:
        // it clears the kit accent *and* calls `invalidate_cached_frames`,
        // which does the `ROLE_INKS.set(None)`. Resetting through it rather
        // than through `set_role_inks(default)` means the teardown cannot
        // reach a state the shell itself has no path to.
        tint_in_process_surfaces(None);
    }
}

/// Clear the role memo now, and again when the test ends.
///
/// `ROLE_INKS` is a `thread_local!`, so the default harness hands every test a
/// fresh cell and the module is green either way — including under
/// `--test-threads=1`, where all 121 share one. This is hygiene the file already
/// keeps (`a_status_role_resolves_to_the_theme_color_not_the_accent` ends on a
/// `tint_in_process_surfaces(None)`), made order-proof: a guard restores on the
/// unwind out of a failed assertion, where a trailing call would not, so one red
/// test cannot cascade into the next one on the same thread.
///
/// `pub(super)`, matching [`preem_ink_lock`] (PR #1031 review N1): the same
/// restore-on-unwind argument applies to `shader_map::tests`'s own accent
/// test, which was using a bare trailing `tint_in_process_surfaces(None)`
/// (no guard) until this fix round adopted this one there instead.
pub(super) fn role_ink_reset() -> RoleInkReset {
    tint_in_process_surfaces(None);
    RoleInkReset
}

/// Install exactly one role color, leaving the other two unset — so a case names
/// a single `(role, color)` pair and no second color can leak into it.
fn inject_role_ink(role: vocab::AccentRole, ink: kit::Rgba) {
    let mut inks = preem_render::RoleInks::default();
    match role {
        vocab::AccentRole::Success => inks.success = Some(ink),
        vocab::AccentRole::Warning => inks.warning = Some(ink),
        vocab::AccentRole::Error => inks.error = Some(ink),
        vocab::AccentRole::Accent | vocab::AccentRole::Neutral => {
            unreachable!("only the three status roles carry a theme color")
        }
    }
    preem_render::set_role_inks(inks);
}

/// The color a `StyleRef` resolves its ink to through the render path, or a
/// panic naming the variant that came back instead.
fn resolved_ink(style: vocab::StyleRef) -> kit::Rgba {
    match preem_render::resolved_pins(style).ink {
        kit::Ink::Fixed(rgba) => rgba,
        other => panic!("a resolved role must arrive as a pinned color, got {other:?}"),
    }
}

/// **#935's property, widened to #940's.** Every libadwaita status color, on
/// **every** skin, clears WCAG AA against the field it is actually painted on.
///
/// #939 could only claim that for the `Lcd`: it asked the skin through
/// [`kit::DisplayStyle::admit_ink`], which runs the per-skin `AccentPolicy`, and
/// that policy is `AsGiven` on `Vfd`/`Oled`/`Crt` — so the light-theme trio was
/// arriving there verbatim at **3.15–3.95:1**, nine of the eighteen
/// role×dark-skin pairs below the bar. #940 chose option B: the shell asks
/// through [`kit::DisplayStyle::admit_role_ink`] instead, the seam that holds AA
/// on all four skins because a status role is a *signal*, not the desktop's
/// identity.
///
/// Three claims, and the split between them is the design:
///
/// 1. **AA everywhere**, for all six colors on all four skins — the acceptance
///    #935 asked for.
/// 2. The equality against the kit seam is the **anti-drift** one: the shell
///    must route through the kit, not re-implement a ramp beside it (#933's
///    `the_public_seam_answers_what_the_render_path_does` is its mirror on the
///    kit side).
/// 3. The **accent** seam is asserted unmoved on the dark three in the same
///    loop, on the same colors: `admit_ink` still hands them back verbatim. That
///    is what "accent as given, roles to legible" means as an inequality, and it
///    is the half a mutation that routed everything through one seam would
///    break.
///
/// Resolution rather than pixels because three of the four skins bloom or mask
/// their lit layer, which would make a "find the fully-lit pixel" assertion a
/// claim about the post-passes. The rendered half is
/// [`a_role_renders_like_its_pin_exactly_where_the_theme_color_was_already_legible`]
/// and [`the_admitted_role_inks_on_the_lcd_are_pinned_to_their_bytes`].
///
/// **Deletion check:** reverting the role arm to `admit_ink` (#939's call) turns
/// this red on the light trio at `vfd`/`oled`/`crt`, `[0, 124, 61, 255]` at
/// 3.745:1 where AA was owed. Restoring the pre-#935 arm (`.map_or(Ink::Default,
/// Ink::Fixed)` — the role color pinned unadmitted) reds the seam equality on
/// the `Lcd`, `[100, 236, 165, 255]` against `[45, 71, 48, 255]`. Hard-coding
/// `DisplayStyle::Lcd` instead of `display_style(style)` reds the same equality
/// from the other side, on `vfd`.
#[test]
fn every_status_role_color_is_admitted_by_the_skin_it_renders_on() {
    let _ink = preem_ink_lock();
    let _roles = role_ink_reset();

    for (label, role, color) in LIBADWAITA_ROLES {
        for (name, skin) in vocab::StyleName::ALL
            .into_iter()
            .zip(kit::DisplayStyle::ALL)
        {
            assert_eq!(
                name.name(),
                skin.name(),
                "the wire's style order is the kit's — the zip above depends on it",
            );
            inject_role_ink(role, color);
            let resolved = resolved_ink(vocab::StyleRef::new(name).with_accent(role));
            let raw = kit::contrast_ratio(color, skin.field());
            let got = kit::contrast_ratio(resolved, skin.field());

            assert_eq!(
                resolved,
                skin.admit_role_ink(color, None),
                "{label} on {}: the shell must hand the color to the skin's own role seam",
                skin.name(),
            );
            assert!(
                got >= raw,
                "{label} on {}: admission may not make a color harder to read \
                 ({raw:.3}:1 became {got:.3}:1)",
                skin.name(),
            );
            assert!(
                got >= kit::AA_TEXT,
                "{label} on the {} field: {color:?} read at {raw:.3}:1 and the skin \
                 handed back {resolved:?} at {got:.3}:1, still below AA — this is #940",
                skin.name(),
            );

            if skin != kit::DisplayStyle::Lcd {
                assert_eq!(
                    skin.admit_ink(color, None),
                    color,
                    "{label} on {}: …while the *accent* seam is untouched — a dark panel is \
                     still `AsGiven`, which is what #940 option B deliberately kept",
                    skin.name(),
                );
            }
        }
    }
}

/// The admission measures against the ground the widget will **actually flood** —
/// the widget's own [`field`](vocab::StyleRef::field) pin when it has one, the
/// skin's otherwise. The same rule, and the same reason, as `palette_with`'s
/// ordering for the accent (#933's HIGH-1).
///
/// Both directions are asserted, because they fail differently. On `preem-demo`'s
/// dark lilac `FLD` ground the dark `@success_color` already reads at 9.24:1, so
/// the skin must pass it through — resolving against the olive field instead
/// would darken it to `#2d4730` and drop it onto the lilac at **1.35:1**, making
/// a widget strictly worse than it was before roles were admitted at all. On a
/// *light* pinned ground the role still has to be darkened, and to a different
/// stop than the skin's own field would have chosen.
///
/// **Deletion check:** passing `None` instead of `style.field` to
/// `admit_role_ink` turns the lilac assertion red — `[45, 71, 48, 255]` where
/// the role color belonged, which is 1.348:1 on that ground — and reds **only**
/// this test out of the module's 121, so it is an alarm on its own wire rather
/// than a second bell on the admission itself.
#[test]
fn a_pinned_field_moves_the_ground_a_role_is_admitted_against() {
    let _ink = preem_ink_lock();
    let _roles = role_ink_reset();
    let success = [0x64, 0xec, 0xa5, 0xff];
    inject_role_ink(vocab::AccentRole::Success, success);
    let lcd = vocab::StyleRef::new(vocab::StyleName::Lcd).with_accent(vocab::AccentRole::Success);

    // `preem-demo`'s `FLD` cell pins exactly this ground and leaves its ink on
    // the theme (`main.rs`'s `FIELD_PIN`).
    let lilac = [0x3a, 0x22, 0x50, 0xff];
    let on_lilac = resolved_ink(lcd.with_field(lilac));
    assert_eq!(
        on_lilac, success,
        "the role already reads on a dark pinned ground, so the skin has nothing to admit",
    );
    assert!(
        kit::contrast_ratio(on_lilac, lilac) >= kit::AA_TEXT,
        "…and it is legible there, which is what makes the passthrough right",
    );

    // #928's mirror: a host that pins a *light* ground reopens the same bug
    // through a second door, so the ink still has to be darkened — just to a
    // different stop than the olive field asks for.
    let paper = [0xf5, 0xf5, 0xf5, 0xff];
    let on_paper = resolved_ink(lcd.with_field(paper));
    assert_eq!(
        on_paper,
        [0x3f, 0x7b, 0x55, 0xff],
        "a light pinned ground gets its own admitted ink, not the skin field's",
    );
    assert!(
        kit::contrast_ratio(on_paper, paper) >= kit::AA_TEXT,
        "…and that ink clears AA on the ground it is drawn on",
    );
    assert_ne!(
        on_paper,
        resolved_ink(lcd),
        "…and it is a different stop from the one the skin's own field selects, \
         or this test cannot see the ground at all",
    );
}

/// The six admitted `Lcd` inks, byte for byte, and one of them all the way
/// through the real render path.
///
/// The literal quads pin the ramp: `ADMIT_STOPS` is what decides how far along
/// the accent → skin-ink line the answer sits, and every ratio-shaped assertion
/// in this file would follow it silently to a coarser ramp. The render half is
/// what ties the resolution to a pixel — `Lcd` is the kit's one skin with no
/// bloom and no mask, so a fully-lit dot *is* the ink, which is why the byte
/// probe lives on this skin and not on `Vfd`.
///
/// **Deletion check:** the pre-#935 arm turns every quad red (the raw theme
/// color comes back), and the rendered assertion red at the raw `#64eca5` being
/// on screen.
#[test]
fn the_admitted_role_inks_on_the_lcd_are_pinned_to_their_bytes() {
    let _ink = preem_ink_lock();
    let _roles = role_ink_reset();
    let lcd = kit::DisplayStyle::Lcd;

    for ((label, role, color), admitted) in LIBADWAITA_ROLES.into_iter().zip(LCD_ADMITTED_ROLE_INKS)
    {
        inject_role_ink(role, color);
        let resolved = resolved_ink(vocab::StyleRef::new(vocab::StyleName::Lcd).with_accent(role));
        assert_eq!(resolved, admitted, "{label}: the admitted ink moved");
        assert!(
            kit::contrast_ratio(admitted, lcd.field()) >= kit::AA_TEXT,
            "{label}: …and the pinned byte is the legible one",
        );
    }

    // The same resolution, rasterised: the admitted ink reaches a lit pixel and
    // the raw theme color reaches none.
    let (label, role, color) = LIBADWAITA_ROLES[0];
    let admitted = LCD_ADMITTED_ROLE_INKS[0];
    inject_role_ink(role, color);
    let scope = Scope::detached("935-lcd-bytes");
    let pixels = mapped_pixels(
        &scope,
        &ink_probe(
            "role",
            vocab::StyleRef::new(vocab::StyleName::Lcd).with_accent(role),
        ),
    );
    preem_render::forget_scope(&scope);

    assert!(
        pixels.2.chunks_exact(4).any(|px| px == admitted),
        "{label}: a fully-lit lcd dot must carry the admitted ink",
    );
    assert!(
        !pixels.2.chunks_exact(4).any(|px| px == color),
        "{label}: …and the raw theme color must reach no pixel at all — it is the invisible one",
    );
}

/// **The #912 contract, unmoved.** An author's explicit `ink` pin is returned
/// verbatim even when the skin would never have admitted that color — pins win
/// unconditionally, and #935 narrows that to *stated* colors only.
///
/// The pin used is the very color the role path darkens two tests up, so the two
/// halves of the distinction are measured against one another rather than
/// against different inputs, and the pin is asserted to be illegible so the
/// claim is about a color the skin really would have rejected.
///
/// **Deletion check:** admitting in the `Some(ink)` early-return arm turns this
/// red at "a pinned ink is a stated color and wins outright", `[45, 71, 48,
/// 255]` for `[100, 236, 165, 255]` — and reds #912's
/// `a_pinned_text_box_survives_a_theme_change_though_it_bakes_at_construction`
/// beside it, which is the same contract on the widget that bakes.
#[test]
fn an_explicit_ink_pin_is_never_admitted_even_where_the_lcd_cannot_carry_it() {
    let _ink = preem_ink_lock();
    let _roles = role_ink_reset();
    let illegible = [0x64, 0xec, 0xa5, 0xff];
    let lcd = kit::DisplayStyle::Lcd;

    assert!(
        kit::contrast_ratio(illegible, lcd.field()) < kit::AA_TEXT,
        "the pin has to be a color the skin would have refused, or this proves nothing",
    );

    let pinned = vocab::StyleRef::new(vocab::StyleName::Lcd).with_ink(illegible);
    assert_eq!(
        resolved_ink(pinned),
        illegible,
        "a pinned ink is a stated color and wins outright — legible or not",
    );

    inject_role_ink(vocab::AccentRole::Success, illegible);
    assert_ne!(
        resolved_ink(
            vocab::StyleRef::new(vocab::StyleName::Lcd).with_accent(vocab::AccentRole::Success)
        ),
        illegible,
        "…while the same color arriving as a *role* is the shell's answer, and is admitted",
    );

    let scope = Scope::detached("935-pin-verbatim");
    let pixels = mapped_pixels(&scope, &ink_probe("pin", pinned));
    preem_render::forget_scope(&scope);
    assert!(
        pixels.2.chunks_exact(4).any(|px| px == illegible),
        "…and the pinned color reaches the glass exactly as pinned",
    );
}

/// **A role renders exactly like its pin iff the theme color was already
/// legible on that skin's field** — the rendered half of #940, and now one rule
/// for all four skins instead of a per-skin table.
///
/// Before #940 the split was by *skin*: `AsGiven` meant a role on
/// `Vfd`/`Oled`/`Crt` was never touched, whatever it was, and every role on the
/// `Lcd` was. Option B replaces that with a split by *measurement* — a color
/// that clears `AA_TEXT` on the ground it will be drawn on is stop 0 of the ramp
/// and comes back byte-for-byte; one that does not is tinted. That partitions
/// exactly this table's 24 pairs into the 9 that pass through (libadwaita's dark
/// trio, on the three dark panels) and the 15 that move (everything on the
/// `Lcd`, plus the light trio everywhere).
///
/// The predicate is computed from the color, not written out as a list, which is
/// what makes it a rule rather than a recording. Comparing against the *pin*
/// rather than against a recorded baseline is what makes it non-vacuous without
/// a golden: the pin path provably never touches the admission
/// ([`an_explicit_ink_pin_is_never_admitted_even_where_the_lcd_cannot_carry_it`]),
/// so an equal frame is a frame the admission left alone, all the way through
/// bloom, ghost and the CRT mask.
///
/// **Deletion check:** reverting the role arm to `admit_ink` reds the light
/// trio's three dark-skin rows — the frames come back equal where the rule says
/// they must differ. Asking the *wrong* skin (hard-coding `DisplayStyle::Lcd` in
/// `ink_for` instead of `display_style(style)`) reds "dark `@success_color` on
/// vfd", which the rule says is a passthrough. The pre-#935 arm reds every
/// tinted row at once.
#[test]
fn a_role_renders_like_its_pin_exactly_where_the_theme_color_was_already_legible() {
    let _ink = preem_ink_lock();
    let _roles = role_ink_reset();
    let scope = Scope::detached("940-role-parity");
    let (mut same, mut moved) = (0_u32, 0_u32);

    for (index, (label, role, color)) in LIBADWAITA_ROLES.into_iter().enumerate() {
        for (name, skin) in vocab::StyleName::ALL
            .into_iter()
            .zip(kit::DisplayStyle::ALL)
        {
            inject_role_ink(role, color);
            let by_role = mapped_pixels(
                &scope,
                &ink_probe(
                    &format!("role-{index}-{}", skin.name()),
                    vocab::StyleRef::new(name).with_accent(role),
                ),
            );
            let by_pin = mapped_pixels(
                &scope,
                &ink_probe(
                    &format!("pin-{index}-{}", skin.name()),
                    vocab::StyleRef::new(name).with_ink(color),
                ),
            );
            let legible = kit::contrast_ratio(color, skin.field()) >= kit::AA_TEXT;
            if legible {
                same += 1;
                assert_eq!(
                    by_role,
                    by_pin,
                    "{label} on {}: it already read at {:.3}:1, so the skin has nothing to \
                     admit and not one byte may move",
                    skin.name(),
                    kit::contrast_ratio(color, skin.field()),
                );
            } else {
                moved += 1;
                assert_ne!(
                    by_role,
                    by_pin,
                    "{label} on {}: it read at {:.3}:1, below AA, so the role is admitted \
                     and the pin is not — the two frames must differ",
                    skin.name(),
                    kit::contrast_ratio(color, skin.field()),
                );
            }
        }
    }

    // Neither branch may be vacuous: the point of the rule is that both happen,
    // and #940's whole change is that `moved` grew from 6 to 15.
    assert_eq!(
        (same, moved),
        (9, 15),
        "9 of the 24 role×skin pairs are passthroughs and 15 are tinted",
    );

    preem_render::forget_scope(&scope);
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
                    ..vocab::MarqueeConfig::default()
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
            config: vocab::DotMatrixConfig {
                style,
                ..vocab::DotMatrixConfig::default()
            },
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

// ── Detached RunCommand launch (#953) ────────────────────────────────────────

/// A plugin/effect-id pair unique to this test process (#953 M3).
///
/// The first cut hardcoded `("ts-detached-953", 953)`, which meant two
/// concurrent `cargo test --features system-tests` runs against one user manager
/// — this repo's normal parallel-worktree workflow — raced for a single unit
/// name, and the loser got systemd's `Unit … was already loaded` as a test
/// failure. An interrupted run wedged the name for the length of the sleep.
/// pid + nanoseconds separates concurrent runs *and* successive ones.
#[cfg(feature = "system-tests")]
fn unique_launch_id() -> (String, u64) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    (format!("ts953-{}", std::process::id()), nanos)
}

/// #953: the detached spawn mode wraps the granted argv in a `systemd-run
/// --user` transient **service** unit.
///
/// The two negative assertions carry the design decision. `--scope` is the
/// obvious-looking alternative and is wrong here: with it, systemd-run "runs the
/// command by systemd-run itself as parent process" (systemd-run(1)), staying in
/// the foreground for the program's whole life — measured at **3030 ms** for a
/// `sleep 3` where the transient service returns in **7 ms**. Reading its exit
/// status would be reading the program's, which is precisely the awaited
/// `output()` #953's correction says to bypass; `--wait` would do the same to a
/// service unit. Without either, systemd-run returns as soon as the manager has
/// taken the start job, so its status is a *launch* verdict and the program is a
/// child of `systemd --user`, in neither `trollshell.service`'s process tree nor
/// its cgroup.
#[test]
fn detached_launch_wraps_the_argv_in_a_systemd_run_service_unit() {
    let argv = vec!["foot".to_owned(), "-e".to_owned(), "claude".to_owned()];
    let unit = launch_unit_name("caw", 7, 4242, 3);
    let env = vec![("WAYLAND_DISPLAY".to_owned(), "wayland-1".to_owned())];
    let run_args = launch_argv("caw", 7, &unit, &env, &argv);
    let expected: Vec<String> = [
        "--user",
        "--quiet",
        "--collect",
        "--slice=trollshell-launch.slice",
        "--unit=trollshell-launch-caw-7-4242-3.service",
        "--description=trollshell plugin launch: caw #7",
        "--setenv=WAYLAND_DISPLAY=wayland-1",
        "--",
        "foot",
        "-e",
        "claude",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    assert_eq!(run_args, expected, "the exact systemd-run invocation");
    assert!(
        !run_args.iter().any(|a| a == "--scope"),
        "--scope would keep systemd-run in the foreground as the program's parent",
    );
    assert!(
        !run_args.iter().any(|a| a == "--wait"),
        "--wait would make the launch call await the program's exit",
    );
    assert_eq!(
        unit, "trollshell-launch-caw-7-4242-3.service",
        "#953 wants the program named in `systemctl --user`",
    );
}

/// #953 M2: the unit name carries **host-allocated** uniquifiers, so a plugin
/// that restarts and reuses its own correlation `id` cannot collide with a unit
/// that is still running.
///
/// The pid half is not decoration: detached units outlive a shell restart by
/// design, so a fresh shell's `seq` starts at 0 again while the previous shell's
/// `…-0.service` may still be running. Both halves have to be in the name.
#[test]
fn detached_launch_unit_names_are_unique_per_host_allocation() {
    // Same plugin, same effect id — the exact shape a plugin restart produces.
    // Only the host-allocated fields differ, and that is enough.
    assert_ne!(
        launch_unit_name("caw", 1, 100, 0),
        launch_unit_name("caw", 1, 100, 1),
        "a second launch in one shell process must not reuse the name",
    );
    assert_ne!(
        launch_unit_name("caw", 1, 100, 0),
        launch_unit_name("caw", 1, 200, 0),
        "a second shell process must not reuse the previous one's name",
    );
    // The allocator hands out a fresh name every call, and keeps the plugin id
    // and effect id in it so a unit stays traceable to who asked for it.
    let first = allocate_launch_unit("caw", 1);
    let second = allocate_launch_unit("caw", 1);
    assert_ne!(first, second, "allocate_launch_unit must never repeat");
    for name in [&first, &second] {
        assert!(name.starts_with("trollshell-launch-caw-1-"), "{name}");
        assert!(name.ends_with(".service"), "{name}");
        assert!(
            name.contains(&std::process::id().to_string()),
            "the name carries this process's pid: {name}",
        );
        // systemd's unit-name limit is 255; the `is_valid_plugin_id` cap (64)
        // is what bounds the plugin half.
        assert!(
            name.len() < 255,
            "unit name must fit systemd's limit: {name}"
        );
    }
}

/// `--` must terminate systemd-run's option parsing before the plugin-supplied
/// argv, so a program name that looks like a flag is passed through as the
/// program rather than eaten as a `systemd-run` option.
#[test]
fn detached_launch_argv_is_separated_from_systemd_run_options() {
    let requested = vec!["--scope".to_owned(), "--wait".to_owned()];
    let unit = launch_unit_name("hostile", 1, 5, 0);
    let run_args = launch_argv("hostile", 1, &unit, &[], &requested);
    let sep = run_args
        .iter()
        .position(|a| a == "--")
        .expect("the argv is separated by --");
    assert_eq!(
        &run_args[sep + 1..],
        &["--scope".to_owned(), "--wait".to_owned()],
        "everything after -- is the plugin's argv, never a systemd-run option",
    );
    assert!(
        run_args[..sep]
            .iter()
            .all(|a| a != "--scope" && a != "--wait"),
        "and nothing before -- was contributed by the plugin",
    );
}

/// #953 L5: the shell forwards its own display/IPC variables, and forwards
/// **only** what it actually has — an empty or unset variable must not be
/// asserted over the user manager's real value.
#[test]
fn detached_launch_forwards_only_the_env_the_shell_has() {
    let unit = launch_unit_name("caw", 1, 5, 0);
    let argv = vec!["foot".to_owned()];

    let none = launch_argv("caw", 1, &unit, &[], &argv);
    assert!(
        !none.iter().any(|a| a.starts_with("--setenv=")),
        "with nothing to forward the argv carries no --setenv at all",
    );

    let some = vec![
        ("WAYLAND_DISPLAY".to_owned(), "wayland-1".to_owned()),
        (
            "NIRI_SOCKET".to_owned(),
            "/run/user/1001/niri.sock".to_owned(),
        ),
    ];
    let with = launch_argv("caw", 1, &unit, &some, &argv);
    let sep = with.iter().position(|a| a == "--").expect("separated");
    assert!(
        with[..sep].contains(&"--setenv=WAYLAND_DISPLAY=wayland-1".to_owned())
            && with[..sep].contains(&"--setenv=NIRI_SOCKET=/run/user/1001/niri.sock".to_owned()),
        "forwarded variables are passed before the argv separator: {with:?}",
    );
    // NIRI_SOCKET is the one the session's `import-environment` does NOT carry,
    // which is why forwarding exists at all.
    assert!(
        FORWARDED_ENV.contains(&"NIRI_SOCKET"),
        "a launched program must be able to drive niri IPC",
    );
}

/// #953 H1: a `systemd-run` that ran but could not reach a user manager started
/// **nothing**, so it must classify as `NothingStarted` and let the direct-spawn
/// fallback run — while a manager that answered and refused must not, because a
/// second route could start a second copy.
///
/// This is the pure half of the H1 fix. It matters that it is a unit test: the
/// bug it guards was invisible on any host that *has* a user manager, which is
/// every host this repo's gates run on.
#[test]
fn systemd_run_bus_failure_classifies_as_nothing_started() {
    // systemd 260.2's wording, verbatim.
    let nobus = "Failed to connect to user scope bus via local transport: \
                 $DBUS_SESSION_BUS_ADDRESS and $XDG_RUNTIME_DIR not defined \
                 (consider using --machine=<user>@.host --user to connect to bus of other user)";
    assert!(
        matches!(
            classify_systemd_run_failure("exit status: 1", nobus),
            LaunchFailure::NothingStarted(FallbackReason::NoUserManager, _),
        ),
        "systemd 260's bus-connect wording must reach the fallback",
    );
    // Older/other wordings in the same family — matching only 260's literal
    // sentence is exactly how the fallback became unreachable the first time.
    for older in [
        "Failed to connect to bus: No medium found",
        "Failed to connect to bus: Connection refused",
        "Failed to connect to system scope bus via local transport: Permission denied",
    ] {
        assert!(
            matches!(
                classify_systemd_run_failure("exit status: 1", older),
                LaunchFailure::NothingStarted(FallbackReason::NoUserManager, _),
            ),
            "the predicate must match the family, not one sentence: {older}",
        );
    }
    // A manager that answered and refused is NOT a fallback case: the unit-name
    // collision below exits 1 exactly like the bus failure above, so the exit
    // status carries no information and only the text can discriminate.
    let collision = "Failed to start transient service unit: Unit x.service was \
                     already loaded or has a fragment file.";
    assert!(
        matches!(
            classify_systemd_run_failure("exit status: 1", collision),
            LaunchFailure::Refused(_),
        ),
        "a refusal must be reported, never worked around into a second copy",
    );
}

/// #953 L3: the fallback outcome names the reason it was taken. Claiming "no
/// systemd-run" for a launch that never consulted systemd-run is a false
/// diagnosis, and the rejected-plugin-id path is reachable by any plugin that
/// declares an id like `"my plugin"` (registration does not validate it).
#[test]
fn detached_launch_outcome_reports_the_launch_never_an_exit_status() {
    assert_eq!(
        launch_outcome(&Ok(LaunchReport::Unit(
            "trollshell-launch-caw-7-9-0.service".to_owned()
        ))),
        EffectOutcome {
            ok: true,
            output: Some("launched unit trollshell-launch-caw-7-9-0.service".to_owned()),
        },
    );
    let cases = [
        (FallbackReason::NoSystemdRun, "no systemd-run"),
        (FallbackReason::NoUserManager, "no systemd user manager"),
        (
            FallbackReason::UnsafePluginId,
            "plugin id is not unit-name safe",
        ),
    ];
    for (reason, phrase) in cases {
        let out = launch_outcome(&Ok(LaunchReport::Process { pid: 4242, reason }));
        assert!(out.ok);
        let text = out.output.expect("the fallback names the pid");
        assert!(text.contains("pid 4242"), "{text}");
        assert!(text.contains(phrase), "expected {phrase:?} in {text:?}");
    }
    // A rejected plugin id must not be reported as a missing systemd-run.
    let unsafe_id = launch_outcome(&Ok(LaunchReport::Process {
        pid: 1,
        reason: FallbackReason::UnsafePluginId,
    }))
    .output
    .expect("output");
    assert!(
        !unsafe_id.contains("no systemd-run"),
        "systemd-run was present and simply never consulted: {unsafe_id}",
    );
    // A refused launch is the only `ok: false` there is here.
    let failed = launch_outcome(&Err("systemd-run --user failed (exit status: 1)".to_owned()));
    assert!(!failed.ok);
    assert!(
        failed
            .output
            .expect("a failure explains itself")
            .contains("systemd-run --user failed"),
    );
}

/// #953 M1: the audit line names the effect id and, for a detached launch, the
/// unit and slice — which is what makes `RunCommand(detached)` reconcilable
/// against `systemctl --user list-units 'trollshell-launch-*'`. Without them ten
/// launches from one plugin produce ten byte-identical lines.
#[test]
fn audit_line_names_the_effect_id_and_the_launched_unit() {
    // Fire-and-forget effects are unchanged: no id, no unit.
    assert_eq!(
        format_audit_line(
            "2026-07-24T00:00:00Z",
            "timer",
            "Notify",
            AuditDecision::Allowed,
            None,
            None,
            None,
        ),
        "2026-07-24T00:00:00Z plugin=timer effect=Notify decision=allowed",
    );
    // The attached mode gains the correlation id.
    assert_eq!(
        format_audit_line(
            "2026-07-24T00:00:00Z",
            "timer",
            "RunCommand",
            AuditDecision::Allowed,
            Some(7),
            None,
            None,
        ),
        "2026-07-24T00:00:00Z plugin=timer effect=RunCommand decision=allowed id=7",
    );
    // The detached mode names the unit it started, and the slice that collects
    // every such unit — the two things an operator needs to reconcile or clean up.
    assert_eq!(
        format_audit_line(
            "2026-07-24T00:00:00Z",
            "caw",
            "RunCommand(detached)",
            AuditDecision::Allowed,
            Some(7),
            None,
            Some("trollshell-launch-caw-7-4242-0.service"),
        ),
        "2026-07-24T00:00:00Z plugin=caw effect=RunCommand(detached) decision=allowed \
         id=7 unit=trollshell-launch-caw-7-4242-0.service slice=trollshell-launch.slice",
    );
    // A unit name embeds the plugin's own id, so it is sanitized too — a hostile
    // id must not be able to forge a second record through the unit field.
    let line = format_audit_line(
        "T",
        "bad id",
        "RunCommand(detached)",
        AuditDecision::Allowed,
        Some(1),
        None,
        Some("trollshell-launch-bad id\n-1-2-3.service"),
    );
    assert!(!line.contains('\n'), "no newline may reach the log: {line}");
    assert!(
        line.contains("unit=trollshell-launch-bad_id_-1-2-3.service"),
        "{line}"
    );
}

/// #953 regression guard for the *other* mode: the attached `RunCommand` still
/// runs the program to completion and routes its **exit status** back, which is
/// the behaviour the detached mode had to be added beside rather than replace.
/// (`command_outcome_maps_success_and_stdout` and
/// `command_outcome_truncates_long_output` in `effects.rs` pin the stdout
/// mapping; this pins that a real child is still awaited.)
#[tokio::test]
async fn attached_run_command_still_awaits_the_program_and_routes_its_exit_status() {
    let ok = execute_command("t", 1, &["true".to_owned()]).await;
    assert!(ok.ok, "a zero exit is reported as ok");
    let bad = execute_command("t", 2, &["false".to_owned()]).await;
    assert!(!bad.ok, "a non-zero exit is reported as not-ok");
    let missing = execute_command("t", 3, &["trollshell-no-such-binary-953".to_owned()]).await;
    assert!(!missing.ok, "a spawn failure is reported, never a hang");
    let empty = execute_command("t", 4, &[]).await;
    assert!(!empty.ok, "an empty argv is reported, never a panic");
}

/// #953, the behavioural half: a detached launch of a **30 s** program returns a
/// launch verdict essentially immediately — nowhere near the attached mode's
/// 10 s `RUN_COMMAND_TIMEOUT` — and the program is still running afterwards.
///
/// Gated to `system-tests` because it starts a real process (and, where a user
/// manager answers, a real transient unit). Both launch paths are legitimate
/// outcomes and the test says which one ran. There is **no skip branch**: after
/// the H1 fix every "no user manager" shape reaches the fallback, so a launch
/// that fails here is a real failure. (The previous cut skipped on a systemd 259
/// wording that systemd 260 does not emit, which meant it could not fire and the
/// test panicked instead — see `detached_launch_falls_back_without_a_user_manager`.)
///
/// This is the test the falsification deletes into: making the detached path
/// await the program — `--wait` on the systemd path, `kill_on_drop` on the
/// fallback — turns the assertions red.
///
/// **In `checks.system-tests` (#1082):** this test neither gates nor asserts
/// which [`LaunchReport`] shape it gets — `assert_launched_then_clean_up`
/// accepts either — so it needs no gate of its own, but a passing run here
/// only proves *some* launch path worked, not which one; cargo's captured
/// output for a passing test can't distinguish that.
/// `detached_launch_falls_back_without_a_user_manager_inner`'s hard
/// `assert_eq!(reason, FallbackReason::NoUserManager)` is the test that
/// actually proves the sandbox has no `systemd --user` manager — see its doc
/// comment for that proof.
#[cfg(feature = "system-tests")]
#[tokio::test]
async fn detached_launch_returns_at_once_and_the_program_outlives_the_call() {
    let (plugin, id) = unique_launch_id();
    let unit = allocate_launch_unit(&plugin, id);
    let argv = vec!["sleep".to_owned(), "30".to_owned()];
    let started = Instant::now();
    let report = start_detached(&plugin, id, &unit, &argv).await;
    let elapsed = started.elapsed();

    let report = report.unwrap_or_else(|e| panic!("detached launch failed: {e}"));
    assert!(
        elapsed < Duration::from_secs(3),
        "a detached launch must not await the program (it sleeps 30s); took {elapsed:?}",
    );

    // Give the program a beat to actually be running, then check liveness and
    // clean up. Cleanup happens *before* the assertion so a failure can't leak
    // a 30 s process or a transient unit.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_launched_then_clean_up(&report).await;
}

/// #953 M2, on a live user manager: two launches carrying the **same** plugin
/// id and the **same** effect id — the exact shape a plugin restart produces —
/// must both start, because the host, not the plugin, owns the unit name's
/// uniqueness. Before the fix the second got systemd's `Unit … was already
/// loaded or has a fragment file` while the first was still running.
///
/// **In `checks.system-tests` (#1082):** as with its sibling above, this test
/// accepts whichever [`LaunchReport`] fallback each launch takes and doesn't
/// itself prove which one the sandbox produced —
/// `detached_launch_falls_back_without_a_user_manager_inner`'s hard-asserted
/// `NoUserManager` reason is that proof. The uniqueness assertions here
/// (distinct unit names, distinct `LaunchReport`s) hold regardless of which
/// fallback fires.
#[cfg(feature = "system-tests")]
#[tokio::test]
async fn two_launches_with_one_effect_id_both_start() {
    let (plugin, id) = unique_launch_id();
    let argv = vec!["sleep".to_owned(), "30".to_owned()];

    // Two "connections" from the same plugin, both choosing the same id.
    let first_unit = allocate_launch_unit(&plugin, id);
    let first = start_detached(&plugin, id, &first_unit, &argv)
        .await
        .unwrap_or_else(|e| panic!("first launch failed: {e}"));
    let second_unit = allocate_launch_unit(&plugin, id);
    let second = start_detached(&plugin, id, &second_unit, &argv)
        .await
        .unwrap_or_else(|e| panic!("second launch with the same effect id failed: {e}"));

    assert_ne!(
        first_unit, second_unit,
        "the host must allocate a distinct unit name for each launch",
    );
    assert_ne!(first, second, "two live launches must be distinguishable");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_launched_then_clean_up(&first).await;
    assert_launched_then_clean_up(&second).await;
}

/// Whether `systemd-run` resolves on `$PATH`, honouring
/// `TROLLSHELL_REQUIRE_SYSTEMD_RUN` the way `hytte-ui`'s `gl_surface.rs`
/// honours `TROLLSHELL_REQUIRE_GL` via its `real_gl_or_skip` (#1077's
/// precedent, itself modelled on `TROLLSHELL_REQUIRE_ICON_THEME`): a skip is
/// indistinguishable from a pass in captured output, so the build that means
/// this to gate (CI's `system-tests` check, since #1082 puts `pkgs.systemd` in
/// its `nativeCheckInputs`) sets `TROLLSHELL_REQUIRE_SYSTEMD_RUN=1`, and a
/// missing `systemd-run` there is a real regression — the sandbox lost a
/// dependency it is supposed to have — so it **fails** naming the reason,
/// rather than skipping quietly. Without the variable (a bare
/// `cargo test --features system-tests` on a box that genuinely has no
/// systemd) it still skips, because failing a run that could never have
/// answered the question helps nobody.
///
/// Only `detached_launch_falls_back_without_a_user_manager` calls this: the
/// other gated launch tests (`detached_launch_returns_at_once_…`,
/// `two_launches_with_one_effect_id_both_start`) accept *either*
/// [`LaunchReport`] shape via `assert_launched_then_clean_up`, so a missing
/// `systemd-run` doesn't stop them from exercising the direct-spawn fallback —
/// they need no gate of their own.
#[cfg(feature = "system-tests")]
async fn systemd_run_on_path_or_skip(test_name: &str) -> bool {
    let found = tokio::process::Command::new("systemd-run")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok();
    if found {
        return true;
    }
    let required =
        std::env::var_os("TROLLSHELL_REQUIRE_SYSTEMD_RUN").is_some_and(|want| want == "1");
    assert!(
        !required,
        "TROLLSHELL_REQUIRE_SYSTEMD_RUN=1, but systemd-run is not on PATH for {test_name}",
    );
    eprintln!(
        "SKIPPED {test_name}: no systemd-run on PATH, so the NoUserManager shape cannot arise"
    );
    false
}

/// #953 H1, the behavioural half: with no user manager reachable, a detached
/// launch must take the **direct-spawn fallback** — the path the docs promise
/// and the code could not reach before this fix.
///
/// `std::env::remove_var` is `unsafe` in edition 2024 (and process-global, so
/// unsound under a parallel test harness), so the scrubbed environment is
/// applied by re-executing *this test binary* on the inner test below. That is
/// also exactly the shape the reviewer used to falsify the first cut.
///
/// **In `checks.system-tests` (#1082):** `systemd-run` is on `$PATH` there
/// (`pkgs.systemd`), but the sandbox already has no `systemd --user` manager
/// and no session bus, so the scrubbed re-exec below (which forces exactly
/// that shape) has nothing left to remove — the ambient environment already
/// is the `NoUserManager` case. It runs unconditionally, and its inner test's
/// hard `assert_eq!(reason, FallbackReason::NoUserManager)` is what actually
/// proves the sandbox's shape: a bare passing `cargo test` line can't show
/// which branch a test took, but this one hard-asserts the specific reason.
#[cfg(feature = "system-tests")]
#[tokio::test]
async fn detached_launch_falls_back_without_a_user_manager() {
    // Only meaningful where `systemd-run` exists but the bus does not; with no
    // `systemd-run` at all the fallback is already the trivial path.
    if !systemd_run_on_path_or_skip("detached_launch_falls_back_without_a_user_manager").await {
        return;
    }
    let exe = std::env::current_exe().expect("the test binary's own path");
    let out = tokio::process::Command::new(exe)
        .args([
            "--exact",
            "--nocapture",
            "--test-threads=1",
            "plugins::tests::detached_launch_falls_back_without_a_user_manager_inner",
        ])
        // The scrubbed environment: exactly what systemd-run needs to reach a
        // user manager, and nothing else.
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("XDG_RUNTIME_DIR")
        .env("TS_953_NO_USER_MANAGER", "1")
        .output()
        .await
        .expect("re-exec the test binary with a scrubbed environment");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the no-user-manager child must pass, not panic.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    );
    assert!(
        stdout.contains("NO_USER_MANAGER_FALLBACK") || stderr.contains("NO_USER_MANAGER_FALLBACK"),
        "the child must report that it took the fallback.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    );
}

/// The child half of [`detached_launch_falls_back_without_a_user_manager`]. Runs
/// its assertions only when that test re-executed it with the scrubbed
/// environment; in an ordinary run there *is* a user manager, so there would be
/// nothing to assert.
#[cfg(feature = "system-tests")]
#[tokio::test]
async fn detached_launch_falls_back_without_a_user_manager_inner() {
    if std::env::var_os("TS_953_NO_USER_MANAGER").is_none() {
        return;
    }
    let (plugin, id) = unique_launch_id();
    let unit = allocate_launch_unit(&plugin, id);
    let argv = vec!["sleep".to_owned(), "30".to_owned()];
    let started = Instant::now();
    let report = start_detached(&plugin, id, &unit, &argv).await;
    let elapsed = started.elapsed();

    let report = report.unwrap_or_else(|e| {
        panic!("with no user manager the launch must fall back, not fail: {e}")
    });
    let LaunchReport::Process { pid, reason } = report else {
        panic!("expected the direct-spawn fallback, got {report:?}");
    };
    assert_eq!(
        reason,
        FallbackReason::NoUserManager,
        "systemd-run was on PATH and ran; it simply could not reach a manager",
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the fallback must not await the program either; took {elapsed:?}",
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let still_running = process_alive(pid);
    let _ = tokio::process::Command::new("kill")
        .arg(pid.to_string())
        .output()
        .await;
    assert!(
        still_running,
        "the fallback-launched program must still be running"
    );
    println!("NO_USER_MANAGER_FALLBACK pid={pid}");
}

/// `true` while `pid` is a live (non-zombie) process. A `/proc/<pid>` existence
/// check alone would pass for a zombie, which is exactly what an exited-but-
/// unreaped child looks like.
#[cfg(feature = "system-tests")]
fn process_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // "<pid> (<comm>) <state> …" — comm can contain spaces and parens, so split
    // at the LAST ')'.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_owned))
        .is_some_and(|state| state != "Z")
}

/// Assert the launch named by `report` is still running, then stop it. Cleanup
/// runs **before** the assertion so a failure cannot leak a 30 s process or a
/// transient unit into the user manager (#953 M3).
#[cfg(feature = "system-tests")]
async fn assert_launched_then_clean_up(report: &LaunchReport) {
    match report {
        LaunchReport::Unit(unit) => {
            eprintln!("detached launch took the systemd-run path: {unit}");
            // Bounded at the same 10s as the launch call itself
            // (`LAUNCH_CALL_TIMEOUT`, effects.rs:1093): this arm only runs
            // where a real `systemd --user` manager answered `systemd-run`
            // (#1082 puts `systemd-run` on the `system-tests` sandbox's
            // `$PATH`, but that sandbox has no manager, so today this is a
            // developer-box path — CI could still reach it if a runner ever
            // exposes one). An unbounded `systemctl` here would turn a stuck
            // call into the outer CI timeout (#1011's shape, ~75 minutes)
            // instead of a fast, named red.
            let is_active = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::process::Command::new("systemctl")
                    .args(["--user", "is-active", unit])
                    .output(),
            )
            .await
            .unwrap_or_else(|_| panic!("systemctl --user is-active {unit} timed out after 10s"))
            .expect("systemctl --user is-active");
            let state = String::from_utf8_lossy(&is_active.stdout).trim().to_owned();
            tokio::time::timeout(
                Duration::from_secs(10),
                tokio::process::Command::new("systemctl")
                    .args(["--user", "stop", unit])
                    .output(),
            )
            .await
            .unwrap_or_else(|_| panic!("systemctl --user stop {unit} timed out after 10s"))
            .ok();
            assert_eq!(
                state, "active",
                "the launched program must still be running after the effect returned",
            );
        }
        LaunchReport::Process { pid, reason } => {
            eprintln!(
                "detached launch took the direct-spawn fallback: pid {pid} ({})",
                match reason {
                    FallbackReason::NoSystemdRun => "no systemd-run",
                    FallbackReason::NoUserManager => "no systemd user manager",
                    FallbackReason::UnsafePluginId => "plugin id is not unit-name safe",
                },
            );
            let still_running = process_alive(*pid);
            let _ = tokio::process::Command::new("kill")
                .arg(pid.to_string())
                .output()
                .await;
            assert!(
                still_running,
                "the launched program must still be running after the effect returned",
            );
        }
    }
}

/// **`wire_map`'s own `Shader` arm** (#893): the field plumbing from
/// `wire::Node::Shader` into the `ShaderNode` view `shader_map` reads.
///
/// `shader_map`'s tests cover the policy and the mapping; this covers the ten
/// lines between them, which are exactly the kind that transpose a pair. A
/// swapped `width`/`height`, a `data_width` fed from `width`, or a dropped
/// `scale` would leave every other test in the tree green — the reconciler node
/// would simply be the wrong shape, and no assertion anywhere else looks at it.
///
/// Deliberately asymmetric numbers on every axis (144 ≠ 48, 8 ≠ 2, scale 3) so
/// a transposition cannot coincide.
///
/// **Falsified** by swapping `width`/`height` or `data_width`/`data_height` in
/// `wire_map`'s arm, or by passing `node.scale` where `1` is expected.
#[test]
fn a_wire_shader_node_maps_its_fields_across_intact() {
    let data: Vec<u8> = (0u8..16).collect();
    let tree = wire::Node::Shader {
        id: Some("spectrum".into()),
        width: 144,
        height: 48,
        scale: 3,
        fragment: "void main() { fragColor = u_accent; }".into(),
        data: data.clone(),
        format: wire::ShaderData::Rgba8,
        data_width: 2,
        data_height: 2,
        classes: vec!["ts-shader".into()],
        tooltip: Some("dropped on purpose — the reconciler node carries none".into()),
    };

    let scope = Scope::detached("wire-shader");
    match to_ui_node(&scope, Grants::all(), &tree) {
        UiNode::Shader {
            id,
            width,
            height,
            state,
            classes,
            tooltip,
        } => {
            assert_eq!(id.as_deref(), Some("spectrum"));
            assert_eq!(
                tooltip.as_deref(),
                Some("dropped on purpose — the reconciler node carries none"),
                "…and since #968 review M3 it is carried rather than dropped",
            );
            assert_eq!((width, height), (144 * 3, 48 * 3), "size × the scale hint");
            assert_eq!(state.scale, 3, "…and the hint itself reaches the shader");
            assert_eq!(&*state.fragment, "void main() { fragColor = u_accent; }");
            assert_eq!(&*state.data, &data[..]);
            assert_eq!(state.data_size, (2, 2), "the data grid, not the surface");
            assert_eq!(state.format, hytte::ui::ShaderFormat::Rgba8);
            assert_eq!(classes, vec!["ts-shader".to_owned()]);
        }
        other => panic!("mapped to {other:?}"),
    }
}

/// The same node from a plugin that never declared `Capability::Shader` maps to
/// the broken-widget placeholder, **and its siblings still render** — the
/// property that makes this a degradation rather than a dropped frame.
///
/// **Falsified** by returning `None` from `map_node`'s `Shader` arm on a
/// refusal: the placeholder disappears and the `Box` comes back one child
/// short, which is the failure mode this file's whole posture rejects.
#[test]
fn an_ungranted_shader_degrades_without_taking_its_siblings() {
    let tree = wire::Node::Box {
        id: Some("root".into()),
        dir: wire::Dir::Vertical,
        spacing: 0,
        scroll: false,
        classes: vec![],
        children: vec![
            wire::Node::Label {
                id: Some("before".into()),
                text: "before".into(),
                classes: vec![],
                tooltip: None,
            },
            wire::Node::Shader {
                id: Some("denied".into()),
                width: 32,
                height: 32,
                scale: 1,
                fragment: "void main() { fragColor = u_fg; }".into(),
                data: vec![0, 0, 0, 0],
                format: wire::ShaderData::Rgba8,
                data_width: 1,
                data_height: 1,
                classes: vec!["ts-shader".into()],
                tooltip: None,
            },
            wire::Node::Label {
                id: Some("after".into()),
                text: "after".into(),
                classes: vec![],
                tooltip: None,
            },
        ],
        tooltip: None,
    };

    let scope = Scope::detached("wire-shader-denied");
    match to_ui_node(&scope, Grants::none(), &tree) {
        UiNode::Box { children, .. } => {
            assert_eq!(children.len(), 3, "the tree keeps its shape");
            assert!(matches!(&children[0], UiNode::Label { text, .. } if text == "before"));
            assert!(matches!(&children[2], UiNode::Label { text, .. } if text == "after"));
            match &children[1] {
                UiNode::Pixels {
                    id,
                    width,
                    height,
                    data,
                    classes,
                    ..
                } => {
                    assert_eq!(id.as_deref(), Some("denied"), "the key survives");
                    assert_eq!((*width, *height), (0, 0), "an empty surface");
                    assert!(data.is_empty());
                    assert_eq!(classes, &vec!["ts-shader".to_owned()], "CSS chrome stays");
                }
                other => panic!("the refused node mapped to {other:?}"),
            }
        }
        other => panic!("mapped to {other:?}"),
    }
}

/// **#968 review M1, the invalidation half.** A desktop-accent change rebuilds
/// a shader node's shared state even though the node's own bytes did not move.
///
/// This is the #396 live re-tint reaching the shader arm: the theme bag is
/// resolved per mapping pass, so a cache keyed on the wire node alone would keep
/// handing back the *old* accent until the plugin's data happened to change —
/// which for a static shader is never.
///
/// Lives here rather than beside the other `shader_map` tests because it writes
/// `hytte_preem`'s process-global accent, and [`PREEM_INK_LOCK`] is what stops
/// that racing every other preem test in this binary.
///
/// **Falsified** by dropping `held.values == values` from `shared_state`'s
/// comparison: the second state is pointer-equal to the first and the assertion
/// goes red.
#[test]
fn an_accent_change_rebuilds_a_shader_nodes_shared_state() {
    let _ink = preem_ink_lock();
    let scope = Scope::detached("shader-retint");
    shader_map::forget_scope(&scope);

    let tree = shader_tree_node();
    let before = shader_state_of(&scope, &tree);

    // The same write `pump::tint_in_process_surfaces` performs on a theme move.
    kit::set_accent(Some([0xff, 0x00, 0x99, 0xff]));
    let after = shader_state_of(&scope, &tree);
    kit::set_accent(None);
    let restored = shader_state_of(&scope, &tree);

    assert_ne!(
        before.values, after.values,
        "the accent must reach the shader's theme bag at all",
    );
    assert!(
        !std::sync::Arc::ptr_eq(&before, &after),
        "a re-tint must rebuild the shared state, not reuse the old accent",
    );
    assert_eq!(
        restored.values, before.values,
        "…and putting the accent back restores the original bag",
    );
    shader_map::forget_scope(&scope);
}

/// A `wire::Node::Shader` for the re-tint test — reads `u_accent`, so the
/// colour it is drawn in is the thing under test.
fn shader_tree_node() -> wire::Node {
    wire::Node::Shader {
        id: Some("retint".into()),
        width: 32,
        height: 32,
        scale: 1,
        fragment: "void main() { fragColor = u_accent; }".into(),
        data: vec![0, 0, 0, 0],
        format: wire::ShaderData::Rgba8,
        data_width: 1,
        data_height: 1,
        classes: vec![],
        tooltip: None,
    }
}

/// One mapping pass over `tree`, returning the shader state it produced.
fn shader_state_of(scope: &Scope, tree: &wire::Node) -> std::sync::Arc<hytte::ui::ShaderState> {
    match to_ui_node(scope, Grants::all(), tree) {
        UiNode::Shader { state, .. } => state,
        other => panic!("mapped to {other:?}"),
    }
}

// ── #1152: the two text kinds on the GL seam ─────────────────────────────────

/// The marquee and the text box on the GPU (#1152) — the host-side half of the
/// seam, against the kit as the oracle.
///
/// Appended as a module rather than woven in so the two kinds' assertions read
/// together: they are the fourth and fifth arms on a seam whose first three
/// each have a run of tests above, and what is *new* here is not a fourth copy
/// of that run but the two things neither the gauge nor the dot matrix has — a
/// ticker whose GPU state is re-uploaded on a clock, and a widget whose palette
/// is baked at construction.
mod text_kinds_gl {
    use std::sync::Arc;

    use hytte::ui::Node as UiNode;
    use hytte_plugin_proto::preem as vocab;
    use hytte_preem as kit;

    use super::super::preem_render::{self, Scope};
    use super::super::shader_map::Grants;
    use super::super::wire_map::to_ui_node;
    use super::{kit_pixels, mapped_gl_for, mapped_pixels, preem_ink_lock, preem_node};

    /// The message every ticker case shows: long enough to overflow a 96 px
    /// window at the default pitch, so it actually scrolls.
    const LONG: &str = "PREEM RASTER KIT ~ SCROLLING TICKER ~ ";

    /// A `Marquee` widget at a known geometry, scrolling at a rate that moves a
    /// whole dot in well under a second.
    fn marquee_widget(text: &str, speed: f32) -> vocab::PreemWidget {
        vocab::PreemWidget::Marquee {
            config: vocab::MarqueeConfig {
                style: vocab::StyleRef::new(vocab::StyleName::Crt),
                window_px: 96,
                gap_dots: 6,
                dot_px: 4,
                speed_dots_per_sec: speed,
            },
            state: vocab::MarqueeState {
                text: text.to_owned(),
            },
        }
    }

    /// The kit strip `marquee_widget` describes — the oracle for every byte
    /// assertion below.
    fn kit_strip(text: &str) -> kit::MarqueeStrip {
        kit::Marquee::new(kit::DisplayStyle::Crt)
            .window_px(96)
            .gap_dots(6)
            .dot_px(4)
            .render(text)
    }

    /// A `TextBox` widget at a known geometry.
    fn textbox_widget(text: &str) -> vocab::PreemWidget {
        vocab::PreemWidget::TextBox {
            config: vocab::TextBoxConfig {
                style: vocab::StyleRef::new(vocab::StyleName::Crt),
                width: vocab::TextBoxWidth::Cols(11),
                max_lines: 3,
                pad: 3,
                corner: 2,
                scale: 2,
                fixed_width: true,
                notdef: None,
            },
            state: vocab::TextBoxState {
                text: text.to_owned(),
            },
        }
    }

    /// The kit box `textbox_widget` describes.
    fn kit_box() -> kit::TextBox {
        kit::TextBox::styled(kit::DisplayStyle::Crt)
            .cols(11)
            .max_lines(3)
            .pad(3)
            .corner(2)
            .scale(2)
            .fixed_width(true)
    }

    /// **Both new arms answer the two halves of the GL seam**, and both are on
    /// the GPU under the GL arm.
    ///
    /// The sibling above (`every_gl_renderer_answers_both_halves_of_the_gl_seam`)
    /// already loops every vocabulary widget and asserts
    /// `is_gl() == gl_surface().is_some()`, so a new arm that answered one half
    /// is caught there whether or not anyone edits it. What it does *not*
    /// assert is that these two kinds specifically reached the GPU at all — its
    /// premise names the first three — so that is what this adds.
    ///
    /// **Falsified** by dropping either kind's `build` arm, or by forgetting it
    /// in `Renderer::is_gl`.
    #[test]
    fn the_two_text_kinds_draw_on_the_gpu_under_the_gl_arm() {
        let _ink = preem_ink_lock();
        let widgets = [marquee_widget(LONG, 12.0), textbox_widget("mrrp!")];
        for widget in &widgets {
            let (is_gl, has_surface) =
                preem_render::gl_seam_for(widget).expect("every vocabulary widget builds");
            assert!(
                !is_gl && !has_surface,
                "{}: the CPU arm draws on neither half",
                widget.kind(),
            );
        }
        super::super::preem_gl::with_gl_arm(|| {
            for widget in &widgets {
                let (is_gl, has_surface) =
                    preem_render::gl_seam_for(widget).expect("every vocabulary widget builds");
                assert!(
                    is_gl && has_surface,
                    "{}: #1152's arm answers both halves",
                    widget.kind(),
                );
            }
        });
    }

    /// **The kill switch restores the kit's own marquee bytes**, at the offset
    /// the renderer is sitting at.
    ///
    /// The mirror of `the_cpu_arm_still_emits_the_kits_own_dot_matrix_bytes…`
    /// for this kind. The whole preem suite runs on the CPU arm by default
    /// (`preem_gl`'s `TEST_ARM`), so this also pins the *node kind*: a ticker
    /// that silently took the GL arm under the switch would be caught here
    /// rather than by a blank chip.
    #[test]
    fn the_cpu_arm_still_emits_the_kits_own_marquee_bytes_as_a_pixels_node() {
        let _ink = preem_ink_lock();
        let key = Scope::detached("marquee-kill-switch-cpu");
        let node = preem_node(Some("mq"), marquee_widget(LONG, 12.0));

        assert!(
            matches!(
                to_ui_node(&key, Grants::none(), &node),
                UiNode::Pixels { .. }
            ),
            "with the kill switch on, a Marquee is a raster surface",
        );
        assert_eq!(
            mapped_pixels(&key, &node),
            kit_pixels(&kit_strip(LONG).window(0)),
            "the CPU arm is the kit, byte for byte",
        );
    }

    /// …and the same for the text box, whose builder bakes its palette.
    #[test]
    fn the_cpu_arm_still_emits_the_kits_own_textbox_bytes_as_a_pixels_node() {
        let _ink = preem_ink_lock();
        let key = Scope::detached("textbox-kill-switch-cpu");
        let node = preem_node(Some("tb"), textbox_widget("mrrp mrrp"));

        assert!(
            matches!(
                to_ui_node(&key, Grants::none(), &node),
                UiNode::Pixels { .. }
            ),
            "with the kill switch on, a TextBox is a raster surface",
        );
        assert_eq!(
            mapped_pixels(&key, &node),
            kit_pixels(&kit_box().render("mrrp mrrp")),
            "the CPU arm is the kit, byte for byte",
        );
    }

    /// **The GL ticker emits its own pipeline at the kit's own window**, with
    /// one texel per grid column.
    ///
    /// The size agreement is the layout argument every kind on this seam makes:
    /// a kill-switch flip is a node-kind change, so it rebuilds the widget, and
    /// a rebuild that also resized would reflow the whole card.
    ///
    /// The strip length is what separates this arm from the dot matrix's: a
    /// ticker uploads the **visible grid**, `cols` texels, not `5 × chars` —
    /// the message can be arbitrarily longer than the window and the upload
    /// must not grow with it.
    ///
    /// **Falsified** by encoding the bitmap instead of the window (the strip
    /// length), or by pointing the grid at anything but the kit's window.
    #[test]
    fn the_gl_marquee_emits_its_own_pipeline_with_the_window_grid() {
        let _ink = preem_ink_lock();
        let node = preem_node(Some("mq"), marquee_widget(LONG, 12.0));

        let cpu = Scope::detached("marquee-gl-size-cpu");
        let (cpu_w, cpu_h, _) = mapped_pixels(&cpu, &node);

        super::super::preem_gl::with_gl_arm(|| {
            let gl = Scope::detached("marquee-gl-size-gl");
            let (gl_w, gl_h, uniforms) = mapped_gl_for(&gl, &node, super::super::preem_gl::MARQUEE);
            assert_eq!(
                (gl_w, gl_h),
                (cpu_w, cpu_h),
                "same natural size on both arms",
            );
            // The window as asked for, by `9*dot`.
            assert_eq!((gl_w, gl_h), (96, 36));
            assert_eq!(uniforms.grid, (96, 36));
            assert_eq!(
                uniforms.step_seq, 0,
                "the scroll is a texture upload, not GPU state",
            );
            let strip = kit_strip(LONG);
            assert_eq!(
                uniforms.data.as_ref().map(|grid| grid.len()),
                Some(strip.cols()),
                "one texel per grid column — not per character of the message",
            );
            assert!(
                strip.cols() < LONG.chars().count() * kit::font::GLYPH_W,
                "the premise: the message is much longer than the grid ({} \
                 columns against {} texels for a per-character strip), so \
                 encoding the bitmap instead would be visibly bigger",
                strip.cols(),
                LONG.chars().count() * kit::font::GLYPH_W,
            );
        });
    }

    /// **The GL box emits its own pipeline at the kit's own final buffer**,
    /// with the block's glyph columns.
    ///
    /// `scale = 2` here on purpose: the kit bakes its upscale into the bytes,
    /// so the grid the reconciler is handed is already `logical × 2` and the
    /// shader divides back down. Handing it the pre-scale size would draw the
    /// box at a quarter of its allocation.
    ///
    /// **Falsified** by passing `layout.width()` as the grid, or by encoding
    /// only the characters each line actually has (the block is a rectangle).
    #[test]
    fn the_gl_textbox_emits_its_own_pipeline_with_the_glyph_block() {
        let _ink = preem_ink_lock();
        let node = preem_node(Some("tb"), textbox_widget("mrrp mrrp"));

        let cpu = Scope::detached("textbox-gl-size-cpu");
        let (cpu_w, cpu_h, _) = mapped_pixels(&cpu, &node);

        super::super::preem_gl::with_gl_arm(|| {
            let gl = Scope::detached("textbox-gl-size-gl");
            let (gl_w, gl_h, uniforms) = mapped_gl_for(&gl, &node, super::super::preem_gl::TEXTBOX);
            assert_eq!(
                (gl_w, gl_h),
                (cpu_w, cpu_h),
                "same natural size on both arms",
            );
            assert_eq!(uniforms.grid, (gl_w, gl_h));
            let layout = kit_box().layout("mrrp mrrp");
            assert_eq!(
                (gl_w as usize, gl_h as usize),
                layout.buffer(),
                "the **final** buffer, upscale included",
            );
            assert_eq!(
                uniforms.data.as_ref().map(|block| block.len()),
                Some(layout.lines().len() * layout.content_cols() * kit::font::GLYPH_W),
                "a rectangle of cells, five texels each",
            );
            assert_eq!(uniforms.step_seq, 0, "no cross-frame GPU state");
        });
    }

    /// **A scroll step re-uploads the grid, and a mapping pass never does** —
    /// the #911 rule for a widget whose GPU state moves on a clock.
    ///
    /// This is the property `Renderer::MarqueeGl`'s `window` field exists for
    /// and the only one that can distinguish it from encoding inside
    /// `gl_surface`: two mapping passes between two steps must hand out the
    /// *same allocation*, and one `advance` past a whole dot must mint a new
    /// one. `invalidate_cached_frames` in the middle is what makes the first
    /// half non-vacuous — the uniform bag is genuinely rebuilt, so an `Arc`
    /// that survives it was shared rather than re-encoded.
    ///
    /// **Falsified** by calling `preem_gl::encode_window` from
    /// `Renderer::gl_surface` (the `ptr_eq` goes red), or by dropping the
    /// re-encode from `advance`'s `MarqueeGl` arm (the `assert_ne` does).
    #[test]
    fn the_window_grid_is_shared_until_the_scroll_moves_a_whole_dot() {
        let _ink = preem_ink_lock();
        let node = preem_node(Some("mq"), marquee_widget(LONG, 12.0));
        super::super::preem_gl::with_gl_arm(|| {
            let key = Scope::detached("marquee-window-sharing");
            let (_, _, first) = mapped_gl_for(&key, &node, super::super::preem_gl::MARQUEE);
            preem_render::invalidate_cached_frames();
            let (_, _, second) = mapped_gl_for(&key, &node, super::super::preem_gl::MARQUEE);
            assert!(
                !Arc::ptr_eq(&first, &second),
                "the premise: the cache really was dropped, so this is a fresh bag",
            );
            let before = first.data.clone().expect("a grid");
            let after = second.data.clone().expect("a grid");
            assert!(
                Arc::ptr_eq(&before, &after),
                "a repeat mapping pass costs a refcount, not a re-encode",
            );

            // 12 dots/s for 0.2 s is 2.4 dots: past the first whole one.
            assert!(
                preem_render::advance_all(0.2).contains(&key),
                "the premise: the ticker reported movement",
            );
            let (_, _, stepped) = mapped_gl_for(&key, &node, super::super::preem_gl::MARQUEE);
            let moved = stepped.data.clone().expect("a grid");
            assert!(
                !Arc::ptr_eq(&before, &moved),
                "a whole-dot step mints a new grid",
            );
            assert_ne!(&before[..], &moved[..], "…and it is a different picture");
            let want: Vec<f32> = kit_strip(LONG)
                .window_columns(2)
                .into_iter()
                .map(f32::from)
                .collect();
            assert_eq!(
                &moved[..],
                &want[..],
                "…and it is the kit's own window at the phase the offset reached",
            );
        });
    }

    /// **A new message re-encodes the block**, and a re-tint reproduces the
    /// same one — the text box's half of the rule above.
    ///
    /// **Falsified** by dropping the `TextBoxGl` arm from `update`, which
    /// `update`'s catch-all would otherwise swallow, freezing the bubble's text
    /// for ever.
    #[test]
    fn a_new_message_re_encodes_the_glyph_block() {
        let _ink = preem_ink_lock();
        super::super::preem_gl::with_gl_arm(|| {
            let key = Scope::detached("textbox-block-sharing");
            let first_node = preem_node(Some("tb"), textbox_widget("mrrp"));
            let (_, _, first) = mapped_gl_for(&key, &first_node, super::super::preem_gl::TEXTBOX);
            preem_render::invalidate_cached_frames();
            let (_, _, again) = mapped_gl_for(&key, &first_node, super::super::preem_gl::TEXTBOX);
            // `invalidate_cached_frames` **rebuilds** a TextBox renderer rather
            // than only dropping its bytes — the builder bakes its palette — so
            // the block is legitimately a fresh allocation here. What must hold
            // is that it is the same *picture*.
            assert_eq!(
                first.data.as_deref(),
                again.data.as_deref(),
                "a re-tint reproduces the same block byte for byte",
            );

            let second_node = preem_node(Some("tb"), textbox_widget("purr purr"));
            let (_, _, second) = mapped_gl_for(&key, &second_node, super::super::preem_gl::TEXTBOX);
            assert_ne!(
                first.data.as_deref(),
                second.data.as_deref(),
                "a new message is a new block",
            );
            let layout = kit_box().layout("purr purr");
            assert_eq!(
                second.data.as_ref().map(|b| b.len()),
                Some(layout.lines().len() * layout.content_cols() * kit::font::GLYPH_W),
                "…laid out on the new text's own wrap",
            );
        });
    }

    /// **An accent change re-tints a GL text box** — the live-re-tint contract
    /// (#396/#862), on the one kind whose colors are resolved at
    /// **construction** rather than at mapping time.
    ///
    /// This is the mirror image of
    /// `an_accent_change_re_tints_a_gl_scope_without_rebuilding_it`: a
    /// `ScopeGl` resolves its palette per mapping pass and so must **not** be
    /// rebuilt, while a `TextBoxGl` bakes bg/ink/notdef into its builder and its
    /// `layout`, and so **must** be — exactly as the CPU `TextBox` is. Dropping
    /// its bytes is not enough, because the uniforms are mapped from the layout
    /// and the layout is where the old ink lives.
    ///
    /// **Falsified** by removing `Renderer::TextBoxGl` from
    /// `invalidate_cached_frames`' rebuild branch, which was measured to ship
    /// green against every other test in this module: the block's glyph bits do
    /// not move on a re-tint, so a test that only compared those saw nothing.
    /// A bubble stuck on the previous accent until its plugin next sent a
    /// message is what that leaves on the glass.
    #[test]
    fn an_accent_change_re_tints_a_gl_textbox() {
        let _ink = preem_ink_lock();
        super::super::preem_gl::with_gl_arm(|| {
            let key = Scope::detached("textbox-accent");
            // A role-less style takes the session accent, which is what moves
            // here — `kit_box`'s `Crt` skin tints its ink from it.
            let node = preem_node(Some("tb"), textbox_widget("mrrp mrrp"));

            super::tint_in_process_surfaces(Some([0x00, 0xff, 0x00, 0xff]));
            let (_, _, green) = mapped_gl_for(&key, &node, super::super::preem_gl::TEXTBOX);
            super::tint_in_process_surfaces(Some([0xff, 0x00, 0xff, 0xff]));
            let (_, _, magenta) = mapped_gl_for(&key, &node, super::super::preem_gl::TEXTBOX);

            assert_ne!(
                green.values, magenta.values,
                "the accent reaches the shader as a uniform",
            );
            assert_eq!(
                green.data.as_deref(),
                magenta.data.as_deref(),
                "…and only the colors moved: the glyph block is the same bits",
            );
            // The bytes it re-tints *to* are the kit's own, which is the half a
            // uniform comparison alone cannot say.
            super::tint_in_process_surfaces(None);
            let (_, _, plain) = mapped_gl_for(&key, &node, super::super::preem_gl::TEXTBOX);
            let want = kit_box().layout("mrrp mrrp").colors();
            let ink = plain
                .values
                .iter()
                .find(|(name, _)| *name == "u_ink")
                .expect("u_ink is mapped")
                .1;
            assert_eq!(
                ink,
                hytte::ui::gl_surface::GlValue::Vec4([
                    f32::from(want.1[0]),
                    f32::from(want.1[1]),
                    f32::from(want.1[2]),
                    f32::from(want.1[3]),
                ]),
                "the re-tinted ink is the one the kit's own builder baked",
            );
        });
    }

    /// **A GL text box falls back without waiting for a frame that never
    /// comes** — the context-failure hook, on the more exposed of the two.
    ///
    /// A ticker would recover on its next scroll step even without the hook; a
    /// text box never animates at all, so no mapping pass is ever coming on its
    /// own and the chip would stay blank until the plugin sent another message.
    /// The premise assertion says so out loud rather than leaving it implied.
    ///
    /// **Falsified** by dropping `TextBoxGl` from `Renderer::is_gl`, which
    /// leaves `rebuild_gl_renderers_on_cpu` walking past it.
    #[test]
    fn a_gl_textbox_falls_back_without_waiting_for_a_frame_that_never_comes() {
        let _ink = preem_ink_lock();
        super::super::preem_gl::install();
        super::super::preem_gl::with_gl_arm(|| {
            let key = Scope::detached("textbox-context-lost");
            let node = preem_node(Some("tb"), textbox_widget("mrrp mrrp"));

            assert!(
                matches!(
                    to_ui_node(&key, Grants::none(), &node),
                    UiNode::GlSurface { .. }
                ),
                "the GL arm is chosen while a context is still possible",
            );
            assert!(
                !preem_render::any_animating_in(std::slice::from_ref(&key)),
                "a text box never animates, so nothing will ever re-map it",
            );
            let before = preem_render::probe(&key, Some("tb")).expect("the instance exists");

            hytte::ui::gl_surface::abandon_gl("no GL in this test");

            let after = preem_render::probe(&key, Some("tb")).expect("the instance survives");
            assert_eq!(
                after.0,
                before.0 + 1,
                "the failure hook rebuilt the renderer itself, not the next re-map",
            );
            assert_eq!(
                after.1, before.1,
                "…and did it without an apply, so no widget state was touched",
            );
            assert_eq!(
                mapped_pixels(&key, &node),
                kit_pixels(&kit_box().render("mrrp mrrp")),
                "a lost context drops the bubble to the kit, byte for byte",
            );
        });
    }

    /// **Both marquee arms park the animation clock on the same predicate** —
    /// the property #926's frame clock rests on.
    ///
    /// `animates()` is one shared expression for the two arms, so this cannot
    /// drift by construction; what it *can* do is stop being shared, and then a
    /// kill-switch flip would change when the shell parks. Driven through the
    /// real renderers rather than by reading the source.
    ///
    /// **Falsified** by giving `MarqueeGl` its own `animates` arm.
    #[test]
    fn both_marquee_arms_park_the_clock_on_the_same_predicate() {
        let _ink = preem_ink_lock();
        // A message that fits the grid holds static; a parked or non-finite
        // speed never moves. None must keep the clock awake, on either arm.
        for (widget, animates) in [
            (marquee_widget(LONG, 12.0), true),
            (marquee_widget(LONG, 0.0), false),
            (marquee_widget("HI", 12.0), false),
            (marquee_widget(LONG, f32::NAN), false),
        ] {
            let node = preem_node(Some("mq"), widget);
            let cpu = Scope::detached("marquee-clock-cpu");
            let _ = to_ui_node(&cpu, Grants::none(), &node);
            assert_eq!(
                preem_render::any_animating_in(std::slice::from_ref(&cpu)),
                animates,
                "the CPU arm",
            );
            super::super::preem_gl::with_gl_arm(|| {
                let gl = Scope::detached("marquee-clock-gl");
                let _ = to_ui_node(&gl, Grants::none(), &node);
                assert_eq!(
                    preem_render::any_animating_in(std::slice::from_ref(&gl)),
                    animates,
                    "…and the GL arm, on the same predicate",
                );
            });
        }
    }
}
