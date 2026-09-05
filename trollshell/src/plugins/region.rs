//! GTK-side mount: reconciler-backed regions (sidebar cards + bar chips, #349)
//! and the plugin drawer panel (#349 PR2).
//!
//! Each mount region is a container of N plugin cards, one reconciler per card
//! keyed by plugin id; the drawer panel is a single reconciler showing the
//! **active** plugin's `panel` tree. The region + panel mailboxes are the
//! [`super::PluginHandles`] `Mutable<Vec<SlotRender>>` fields; [`upsert_region`]
//! and [`clear_region_if_owned`] mutate them (shared with the reader task in
//! [`super::session`]).

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use hytte::futures_signals::map_ref;
use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, glib, prelude::*};
use hytte::reactive::registry;
use hytte::ui::{Dir as UiDir, EventKind as UiEventKind, Node as UiNode, NodeId, Reconciler};
use hytte_plugin_proto::HostMsg;
use tokio::sync::mpsc;

use super::preem_render::{self, Scope};
use super::wire_map::{to_ui_node, to_wire_event};
use super::{PluginHandles, SlotRender};

fn lead_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .sidebar_lead
            .signal_cloned()
    })
}

fn top_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .sidebar_top
            .signal_cloned()
    })
}

fn bottom_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .sidebar_bottom
            .signal_cloned()
    })
}

fn bar_left_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .bar_left
            .signal_cloned()
    })
}

fn bar_center_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .bar_center
            .signal_cloned()
    })
}

fn bar_right_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .bar_right
            .signal_cloned()
    })
}

/// The [`Mount::SidebarLead`](hytte_plugin_proto::Mount::SidebarLead) **region** —
/// a vertical container of N plugin cards. Built per monitor from
/// `overlays::sidebar::build_card` and mounted at the very **top** of the sidebar,
/// above the built-in weather/calendar/tasks cards, so a plugin here leads the
/// sidebar (#301).
#[must_use]
pub fn sidebar_lead_slot() -> gtk::Widget {
    build_region(
        lead_render_signal(),
        gtk::Orientation::Vertical,
        "ts-plugin-card",
    )
}

/// The [`Mount::SidebarTop`](hytte_plugin_proto::Mount::SidebarTop) **region** — a
/// vertical container of N plugin cards. Built per monitor from
/// `overlays::sidebar::build_card` and appended above the built-in widgets.
#[must_use]
pub fn sidebar_top_slot() -> gtk::Widget {
    build_region(
        top_render_signal(),
        gtk::Orientation::Vertical,
        "ts-plugin-card",
    )
}

/// The [`Mount::SidebarBottom`](hytte_plugin_proto::Mount::SidebarBottom)
/// **region**, appended below the built-in sidebar widgets.
#[must_use]
pub fn sidebar_bottom_slot() -> gtk::Widget {
    build_region(
        bottom_render_signal(),
        gtk::Orientation::Vertical,
        "ts-plugin-card",
    )
}

/// The [`Mount::BarLeft`](hytte_plugin_proto::Mount::BarLeft) **region** — a
/// horizontal row of N plugin **chips** (#349). Built per monitor from `main.rs`'s
/// `build_bar` and appended into the bar's left group. Each plugin's `view()` tree
/// renders inside a `.ts-plugin-chip` pill, mirroring the sidebar card path but
/// laid out horizontally so co-mounted chips sit side by side.
#[must_use]
pub fn bar_left_slot() -> gtk::Widget {
    build_region(
        bar_left_render_signal(),
        gtk::Orientation::Horizontal,
        "ts-plugin-chip",
    )
}

/// The [`Mount::BarCenter`](hytte_plugin_proto::Mount::BarCenter) **region** — a
/// horizontal row of N plugin chips, appended into the bar's center group (#349).
#[must_use]
pub fn bar_center_slot() -> gtk::Widget {
    build_region(
        bar_center_render_signal(),
        gtk::Orientation::Horizontal,
        "ts-plugin-chip",
    )
}

/// The [`Mount::BarRight`](hytte_plugin_proto::Mount::BarRight) **region** — a
/// horizontal row of N plugin chips, appended into the bar's right group (#349).
#[must_use]
pub fn bar_right_slot() -> gtk::Widget {
    build_region(
        bar_right_render_signal(),
        gtk::Orientation::Horizontal,
        "ts-plugin-chip",
    )
}

/// One plugin's mounted card within a region: its dedicated reconciler root (a
/// child of the region container), the [`Reconciler`] driving it, and the
/// outbound sender its `on_event` routes user interactions to. Keyed per plugin
/// id so a card can be updated, reordered, or removed without disturbing its
/// siblings. GTK-main-thread-only.
struct MountedCard {
    plugin_id: String,
    /// The namespace this card's preem renderer instances live in (#883).
    /// Cached rather than re-derived per render because it is also what the
    /// removal path below hands [`preem_render::forget_scope`].
    preem_scope: Scope,
    root: gtk::Box,
    reconciler: Reconciler,
    /// Outbound of the connection currently owning this plugin's card, swapped
    /// on each render so events always reach the live connection.
    outbound: Rc<RefCell<Option<mpsc::Sender<HostMsg>>>>,
}

/// Build the `gtk::Box` region driven by `signal` (a mount's sorted render
/// list). Each connected plugin gets its own reconciler-backed card; the region
/// reconciles cards in on join / update / reorder / leave, keyed by plugin id.
///
/// `orientation` lays the cards out — `Vertical` for sidebar card stacks,
/// `Horizontal` for bar chip rows (#349). `card_class` is the CSS class stamped
/// on each card root: `ts-plugin-card` for a sidebar card, `ts-plugin-chip` for
/// a bar chip. The region hides itself while empty so an unused bar region never
/// introduces a phantom inter-widget gap in the bar group it sits in.
fn build_region(
    signal: impl Signal<Item = Vec<SlotRender>> + 'static,
    orientation: gtk::Orientation,
    card_class: &'static str,
) -> gtk::Widget {
    // Chips in a horizontal bar row want a small gap between co-mounted plugins;
    // sidebar cards stack tight (each card owns its own bottom margin in CSS).
    let spacing = match orientation {
        gtk::Orientation::Horizontal => 6,
        _ => 0,
    };
    let container = gtk::Box::new(orientation, spacing);
    container.add_css_class("ts-plugin-region");
    // Empty until a plugin dials in; a later reconcile reveals it once a card
    // exists (so an empty region contributes no spacing to its parent group).
    container.set_visible(false);

    // GTK-thread-only per-plugin card state. Order here is just a lookup table;
    // widget order is enforced against the region container directly.
    let cards: Rc<RefCell<Vec<MountedCard>>> = Rc::new(RefCell::new(Vec::new()));

    let cards_for_signal = cards.clone();
    // The render subscription holds this container **weakly**, and that is what
    // makes the region destroyable (#909 — the twin of the pin #903 fixed on the
    // drawer panel child).
    //
    // Held strongly (`container.clone()` moved into the closure, which is what
    // shipped before), the task owns the container and the only thing that
    // aborts the task is the container's own `destroy` handler below. The
    // container can then never reach refcount zero, `destroy` never fires, and a
    // hot-plug strands one fully-live region per monitor per mount — still
    // reconciling plugin cards into a detached widget tree on every render, for
    // the session. Measured before this change: after the surface was destroyed,
    // the container still upgraded from a `glib::WeakRef` **and** still mounted a
    // renderer instance for a plugin id it had never seen.
    //
    // This is the shape `hytte-reactive`'s `bind` has always had
    // (`crates/hytte-reactive/src/bind.rs:16-30`): the weak upgrade is the
    // *guarantee* — the loop `break`s the first time the widget is gone, which
    // covers a widget freed without emitting `destroy` — and the eager
    // `handle.abort()` below only trims the residual between the destroy and the
    // next emission. Written as a `while let` over `poll_change` rather than
    // `for_each` for exactly that reason: `for_each` cannot `break`.
    //
    // #903's inner-`canvas` split is **not** needed here, because the strong refs
    // this closure does hold already point *down* the tree, not back at the
    // container. `Reconciler` keeps a strong clone of the box it was built over
    // (`crates/hytte-ui/src/widget_tree.rs:355`), but [`reconcile_region`] builds
    // one per card over that card's own `root` — a *child* of this container —
    // and in GTK4 a parent refs its children, never the reverse. So the cards and
    // their reconcilers are freed with the subscription, and none of them can
    // hold the container up. Splitting an inner canvas out here would buy nothing
    // and cost a level: [`reconcile_region`] also `set_visible`s the *outer*
    // container (the empty-region hide, which has to stay on the widget the
    // parent bar group lays out) and the row's inter-chip `spacing` lives on it
    // too, so the weak handle would still be required.
    let container_weak = container.downgrade();
    let handle = glib::MainContext::default().spawn_local(async move {
        let mut signal = std::pin::pin!(signal);
        while let Some(renders) = std::future::poll_fn(|cx| signal.as_mut().poll_change(cx)).await {
            let Some(container) = container_weak.upgrade() else {
                // The surface is gone. Nothing to release: a card's scope
                // (`Scope::card(plugin_id)`) is shared by every monitor's copy of
                // that card, so forgetting it here would drop the renderer
                // instances a surviving monitor is still painting. It is
                // [`reconcile_region`]'s retain loop that releases a card scope,
                // driven by the shared render list every live region observes.
                break;
            };
            reconcile_region(&container, &cards_for_signal, &renders, card_class);
        }
    });

    // Teardown: abort the render subscription the moment the region widget is
    // destroyed (a bar rebuild or a sidebar `close_all` on hot-plug), so it stops
    // rendering into a detached container and drops its captured handles —
    // cards, reconcilers and all — immediately rather than lingering until the
    // next emission wakes it to break on the dead weak ref.
    //
    // Captures no widget, only the `JoinHandle`, matching the contract
    // `hytte-reactive`'s `abort_on_destroy` spells out
    // (`crates/hytte-reactive/src/bind.rs:49-56`). A strong clone here would
    // re-pin precisely what the weak handle above unpins — which is the bug this
    // handler silently had until #909.
    container.connect_destroy(move |_| handle.abort());

    container.upcast()
}

/// Reconcile a mount region's child cards against the latest sorted plugin
/// render list. Adds a card + reconciler for a newly-joined plugin, updates &
/// reorders existing cards, and removes cards whose plugin left — each keyed by
/// plugin id, so one plugin's join/leave never disturbs a sibling's widget
/// (the per-plugin removal semantics of #274).
fn reconcile_region(
    container: &gtk::Box,
    cards: &Rc<RefCell<Vec<MountedCard>>>,
    renders: &[SlotRender],
    card_class: &str,
) {
    let mut cards = cards.borrow_mut();

    // Reveal the region exactly when it holds at least one card, so an empty
    // region (no plugin mounted here yet) adds no spacing to its parent group.
    container.set_visible(!renders.is_empty());

    // 1. Drop cards whose plugin vanished from the region (left / disconnected).
    let present: HashSet<&str> = renders.iter().map(|r| r.plugin_id.as_str()).collect();
    cards.retain(|card| {
        let keep = present.contains(card.plugin_id.as_str());
        if !keep {
            container.remove(&card.root);
            // Release the plugin's preem renderer instances — its phosphor
            // buffers, needles and flip boards (#883) — rather than parking them
            // for the session. The render list is shared across monitors, so a
            // plugin absent from it has left every region: another monitor's
            // reconcile forgetting the same scope is a harmless no-op, and no
            // monitor still wants it.
            preem_render::forget_scope(&card.preem_scope);
        }
        keep
    });

    // 2. Upsert each plugin's card in sorted (region) order, laying the roots out
    //    to match. `prev` walks the intended sibling order.
    let mut prev: Option<gtk::Widget> = None;
    for render in renders {
        let preem_scope = Scope::card(&render.plugin_id);
        let ui_tree = to_ui_node(&preem_scope, &render.tree);
        if let Some(idx) = cards.iter().position(|c| c.plugin_id == render.plugin_id) {
            let card = &mut cards[idx];
            // Swap in the live connection's outbound, then re-render its tree.
            *card.outbound.borrow_mut() = Some(render.outbound.clone());
            card.reconciler.render(&ui_tree);
            container.reorder_child_after(&card.root, prev.as_ref());
            prev = Some(card.root.clone().upcast());
        } else {
            // New plugin joined: its own root + reconciler, wired to its own
            // outbound cell (so events reach the right connection).
            let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
            root.add_css_class(card_class);
            let outbound: Rc<RefCell<Option<mpsc::Sender<HostMsg>>>> =
                Rc::new(RefCell::new(Some(render.outbound.clone())));
            let ev_outbound = outbound.clone();
            let mut reconciler = Reconciler::new(&root, move |id: NodeId, kind: UiEventKind| {
                if let Some(tx) = ev_outbound.borrow().as_ref() {
                    // Non-blocking: a stuck plugin's full outbound queue drops the
                    // event rather than blocking the GTK thread (#435). It's about
                    // to be reaped by the liveness ping anyway.
                    let _ = tx.try_send(HostMsg::Event {
                        node: id,
                        kind: to_wire_event(kind),
                    });
                }
            });
            reconciler.render(&ui_tree);
            container.insert_child_after(&root, prev.as_ref());
            prev = Some(root.clone().upcast());
            cards.push(MountedCard {
                plugin_id: render.plugin_id.clone(),
                preem_scope,
                root,
                reconciler,
                outbound,
            });
        }
    }
}

// ── Plugin drawer panel (#349 PR2) ───────────────────────────────────────────

fn panels_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .panels
            .signal_cloned()
    })
}

fn active_panel_signal() -> impl Signal<Item = Option<String>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .active_panel_id
            .signal_cloned()
    })
}

/// Select which plugin's panel the per-monitor drawer children show (#349 PR2).
/// GTK thread. `Some(id)` on open/switch to a plugin panel; `None` on close.
/// `modal.rs` calls this from its plugin-open entry points and on drawer close /
/// monitor teardown.
pub fn set_active_panel(plugin_id: Option<&str>) {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .active_panel_id
            .set(plugin_id.map(str::to_owned));
    });
}

/// An empty panel tree — the blank page a drawer plugin child shows when no
/// plugin is active (or the active plugin left / has no panel).
fn empty_panel() -> UiNode {
    UiNode::Box {
        id: None,
        dir: UiDir::Vertical,
        spacing: 0,
        scroll: false,
        classes: Vec::new(),
        children: Vec::new(),
    }
}

/// The per-monitor plugin drawer child (#349 PR2): a `.ts-plugin-panel` root
/// `gtk::Box` over an inner `.ts-plugin-canvas` box — *two* boxes, since #903 —
/// whose content is the **active** plugin's `panel` tree. The reconciler mounts
/// into the inner one so the root stays freeable; see [`build_panel_child`] for
/// why that level of indirection is load-bearing. One instance
/// lives in each monitor's drawer stack under the fixed `PLUGIN_STACK_CHILD`
/// name (see `modal.rs`); all mirror the same active panel — exactly how sidebar
/// plugin cards mirror onto every monitor's sidebar region. When no plugin is
/// active — or the active plugin left, or it has no panel — the child renders an
/// empty tree (a blank page); the user then closes the drawer.
///
/// Panel events (button / slider / entry) route to the **live** connection of
/// whichever plugin is active, via the same swapped-`outbound` cell as
/// [`MountedCard`], so a fast plugin reconnect redirects panel events without a
/// dangling send.
#[must_use]
pub fn plugin_panel_slot() -> gtk::Widget {
    build_panel_child(panels_render_signal(), active_panel_signal())
}

/// [`plugin_panel_slot`]'s body, with the two registry-backed signals taken as
/// parameters.
///
/// The split exists for the same reason `panels/connections.rs` splits
/// `rebuild_connections` out of its bind: the production accessors
/// [`panels_render_signal`] / [`active_panel_signal`] `.expect()` a registered
/// `PluginHandles` out of the thread-local registry, which a `#[gtk::test]` has
/// no booted `App` to provide. Handing the child two plain `Mutable` signals
/// instead lets `gtk_tests` drive the **real** mount — its subscription, its
/// `connect_destroy`, its scope bookkeeping — rather than a hand-rolled replica
/// of it, which is what the #903 teardown ordering needed to be reproducible.
fn build_panel_child(
    panels: impl Signal<Item = Vec<SlotRender>> + 'static,
    active_id: impl Signal<Item = Option<String>> + 'static,
) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("ts-plugin-panel");

    // The reconciler mounts into an inner box rather than into `root` itself,
    // and that one level of indirection is what makes this child **destroyable**
    // (#903).
    //
    // `Reconciler` keeps a *strong* `gtk::Box` clone of whatever it is built over
    // (`crates/hytte-ui/src/widget_tree.rs:355`), and the reconciler is moved
    // into the render subscription below. Built over `root`, that closes a loop:
    // the task owns a strong ref to `root`, and the only thing that aborts the
    // task is `root`'s own `destroy` handler — so `root` can never reach refcount
    // zero, `destroy` never fires, and nothing about this child is ever torn
    // down. Measured before the split: a child whose window had been destroyed
    // still upgraded from a `WeakRef`, and still mapped preem nodes for the next
    // panel that went active — one stranded, fully-live drawer child per monitor
    // per hot-plug, reconciling into a detached widget tree for the session.
    //
    // Over an inner `canvas` the strong ref points *down* the tree instead of
    // back at `root`, so `root`'s only holder is its parent (the drawer stack).
    // It disposes with the drawer, `destroy` fires, the subscription is aborted,
    // and the closure — with the reconciler and `canvas` inside it — drops.
    //
    // Invisible to CSS: `.ts-plugin-panel` styles `root` and reaches the plugin's
    // tree through a *descendant* selector (`assets/trollshell/style.css:570`),
    // never a child one. The box is plain (vertical, spacing 0, no margins) and
    // propagates its child's expand flags, so it adds no geometry either.
    //
    // It carries a class of its own so the seam is *addressable* — a future
    // `.ts-plugin-panel > …` rule would otherwise silently miss the plugin tree
    // (#909's third nit). Nothing styles `.ts-plugin-canvas` today, and nothing
    // in either stylesheet can start matching because of it: the sheet has no
    // `[class*=…]` selector, and every child-combinator rule that touches the
    // `.ts-plugin-*` family is rooted at a widget *inside* the plugin tree
    // (`.ts-plugin-card scale > trough` and its two variants,
    // `assets/trollshell/style.css:542`/`:549`/`:557`), never at the panel root.
    let canvas = gtk::Box::new(gtk::Orientation::Vertical, 0);
    canvas.add_css_class("ts-plugin-canvas");
    root.append(&canvas);

    // The active connection's outbound, swapped on each render so panel events
    // reach whichever plugin is active now (mirrors the region card pattern).
    let outbound: Rc<RefCell<Option<mpsc::Sender<HostMsg>>>> = Rc::new(RefCell::new(None));
    let ev_outbound = outbound.clone();
    let mut reconciler = Reconciler::new(&canvas, move |id: NodeId, kind: UiEventKind| {
        if let Some(tx) = ev_outbound.borrow().as_ref() {
            // Non-blocking (#435): drop the panel event if the plugin's outbound
            // queue is full rather than block the GTK thread.
            let _ = tx.try_send(HostMsg::Event {
                node: id,
                kind: to_wire_event(kind),
            });
        }
    });

    // Derived signal: the active plugin's current panel `SlotRender` (or `None`
    // when nothing is active or the active plugin has no panel entry).
    let active = map_ref! {
        let panels = panels,
        let active_id = active_id => {
            active_id
                .as_ref()
                .and_then(|id| panels.iter().find(|r| &r.plugin_id == id).cloned())
        }
    };

    // The panel scope currently on screen (#883), so a switch away from a plugin
    // — or the drawer closing — releases its panel's preem renderer instances
    // instead of parking them for the session. A panel tree gets its own scope
    // (never the card's): the two trees are independent and may reuse node ids.
    let shown_scope: Rc<RefCell<Option<Scope>>> = Rc::new(RefCell::new(None));
    // Cloned out before the subscription takes ownership, so the destroy handler
    // below reads and clears the *same* cell the renders write.
    let shown_at_destroy = shown_scope.clone();

    // Holds `root` **weakly**, the same shape [`build_region`] above now has and
    // the one `hytte-reactive`'s `bind` has always had
    // (`crates/hytte-reactive/src/bind.rs:16-30`): the weak upgrade is the
    // *guarantee* and the `destroy`-driven abort below only trims the residual.
    // The `canvas` split is what lets `root` be freed at all; this is what makes
    // the subscription notice, and release the panel scope, on the one path the
    // abort cannot cover — a widget freed without emitting `destroy`. Unreachable
    // in GTK4 (dispose always emits it), which is why the abort carries the
    // common case; both legs are cheap and neither is load-bearing alone.
    let root_weak = root.downgrade();
    let handle = glib::MainContext::default().spawn_local(async move {
        let mut active = std::pin::pin!(active);
        while let Some(slot) = std::future::poll_fn(|cx| active.as_mut().poll_change(cx)).await {
            if root_weak.upgrade().is_none() {
                forget_previous_panel_scope(&shown_scope, None);
                break;
            }
            render_active_panel(&mut reconciler, &outbound, &shown_scope, slot.as_ref());
        }
    });

    // Teardown: abort the render subscription when the drawer child is destroyed
    // (a per-monitor drawer rebuild on hot-plug), and release the panel scope it
    // was showing on the way out.
    //
    // Releasing it *here* rather than leaving it to `modal::close_all`'s
    // `set_active_panel(None)` is the #903 fix. `close_all` destroys every drawer
    // window before it broadcasts (`modal.rs`'s `close_all`), so by the time the
    // `None` is published this handler has already aborted the only subscription
    // that could have acted on it. Reordering the broadcast ahead of the destroys
    // would not have saved it either: `Mutable::set` only *wakes* this task, and
    // it is polled on the next `glib::MainContext` iteration — reached long after
    // `close_all` has returned and the abort has happened. Teardown that depends
    // on someone still being subscribed is not teardown.
    //
    // Captures no widget — only the `JoinHandle` and an `Rc` of the scope cell —
    // matching the contract `hytte-reactive`'s `abort_on_destroy` spells out
    // (`crates/hytte-reactive/src/bind.rs:49-56`). A strong widget clone here
    // would re-pin precisely what the `canvas` split above unpins. It is the
    // *eager* leg of that contract; the weak upgrade in the loop above is the
    // guarantee, and it releases the same scope on the path this handler cannot
    // see (#909's first nit).
    //
    // No sibling-monitor hazard. `active_panel_id` is one global selection, and
    // the only two things that destroy a drawer window are `modal::close_all`
    // (`modal.rs:1161`) and `overlays::sidebar::close_all`
    // (`overlays/sidebar.rs:772`) — both driven from the same `monitors_changed`
    // emission (`main.rs:240-241`), which tears every monitor's surfaces down
    // together. There is no partial teardown that could drop a scope another
    // monitor is still showing. And this is idempotent regardless:
    // `forget_previous_panel_scope` no-ops on an already-cleared cell, and
    // `preem_render::forget_scope` is a `HashMap::remove`.
    root.connect_destroy(move |_| {
        handle.abort();
        forget_previous_panel_scope(&shown_at_destroy, None);
    });
    root.upcast()
}

/// Render whatever the drawer's plugin child should be showing now: `slot`'s
/// panel tree, or the blank page when nothing is active (or the active plugin
/// left / has no panel).
///
/// Extracted from [`plugin_panel_slot`]'s subscription closure so the preem
/// scope lifecycle it drives is reachable from a test — a `Reconciler` and two
/// `Rc`s are constructible; a `spawn_local`'d apply-loop over a `map_ref!` of
/// two registry-backed signals is not. `gtk_tests` at the bottom of this file is
/// what that buys.
fn render_active_panel(
    reconciler: &mut Reconciler,
    outbound: &Rc<RefCell<Option<mpsc::Sender<HostMsg>>>>,
    shown_scope: &Rc<RefCell<Option<Scope>>>,
    slot: Option<&SlotRender>,
) {
    if let Some(render) = slot.filter(|r| r.panel.is_some()) {
        // Swap in the active connection's outbound, then render its panel.
        *outbound.borrow_mut() = Some(render.outbound.clone());
        let scope = Scope::panel(&render.plugin_id);
        forget_previous_panel_scope(shown_scope, Some(&scope));
        reconciler.render(&to_ui_node(
            &scope,
            render.panel.as_ref().expect("filtered Some"),
        ));
    } else {
        // No active plugin (or it left / has no panel): blank the page and drop
        // any stale outbound so no event can reach a gone connection.
        *outbound.borrow_mut() = None;
        forget_previous_panel_scope(shown_scope, None);
        reconciler.render(&empty_panel());
    }
}

/// Record which panel scope the drawer child is about to show, dropping the
/// preem renderer instances of the one it was showing before (#883) — unless it
/// is the same scope, in which case a re-render must *keep* the animation state
/// it is mid-way through.
///
/// `active_panel_id` is a single shared handle, so every monitor's drawer child
/// switches together and a second child forgetting the same scope is a no-op.
///
/// **Closing the drawer counts as leaving**, not only switching plugins:
/// `modal.rs` clears `active_panel_id` on close, which lands here as `None`. So
/// a close/reopen cycle starts the panel's animations over — needles at rest,
/// phosphor dark, flip boards blank — rather than resuming mid-swing. That is
/// the deliberate trade (a closed drawer should not hold a phosphor buffer per
/// plugin for the session), but it is a visible behaviour on glass, not just a
/// teardown detail.
fn forget_previous_panel_scope(shown: &Rc<RefCell<Option<Scope>>>, next: Option<&Scope>) {
    let mut shown = shown.borrow_mut();
    if shown.as_ref() == next {
        return;
    }
    if let Some(previous) = shown.take() {
        preem_render::forget_scope(&previous);
    }
    *shown = next.cloned();
}

/// Insert-or-replace `render`'s plugin card in its mount region, latest-wins per
/// plugin id, keeping the region sorted by `(order, plugin_id)` ascending. A
/// plugin's repeated renders overwrite its own card (coalescing — superseded
/// trees dropped); distinct plugins get distinct cards, so they never fight
/// (#274). Called from a connection's reader task on every `Render`.
pub(super) fn upsert_region(region: &Mutable<Vec<SlotRender>>, render: SlotRender) {
    let mut cards = region.lock_mut();
    if let Some(existing) = cards.iter_mut().find(|c| c.plugin_id == render.plugin_id) {
        *existing = render;
    } else {
        cards.push(render);
    }
    cards.sort_by(|a, b| (a.order, &a.plugin_id).cmp(&(b.order, &b.plugin_id)));
}

/// Remove a plugin's card from a mount region on connection teardown — but only
/// if THIS connection still owns it (its `generation` matches the parked card's).
/// A fast-reconnect successor (same plugin id, higher generation) has already
/// replaced the card, so a stale teardown leaves it (the #278 guarantee, now
/// applied per plugin-id entry). A different plugin's card is keyed by a
/// different id and never matched, so siblings are undisturbed.
///
/// Probes with the read lock first so a teardown never spuriously notifies a
/// region this plugin isn't even in (each teardown checks *all six* regions —
/// three sidebar + three bar, #349); it re-finds under the write lock to stay
/// correct against a concurrent mutation.
pub(super) fn clear_region_if_owned(
    region: &Mutable<Vec<SlotRender>>,
    plugin_id: &str,
    generation: u64,
) {
    let owns = region
        .lock_ref()
        .iter()
        .any(|c| c.plugin_id == plugin_id && c.generation == generation);
    if !owns {
        return;
    }
    let mut cards = region.lock_mut();
    if let Some(pos) = cards
        .iter()
        .position(|c| c.plugin_id == plugin_id && c.generation == generation)
    {
        cards.remove(pos);
    }
}

// ── GTK integration tests (need a display → gated to `system-tests`) ─────────

/// The preem **scope lifecycle** this module drives (#883): a plugin card
/// leaving its region, and the drawer panel switching away or closing, must each
/// release that tree's renderer instances.
///
/// These live here rather than in `plugins::tests` because the functions they
/// drive — [`reconcile_region`] and [`render_active_panel`] — are private to this
/// module and need a real GTK container and `Reconciler`. They are the tests the
/// review found missing: `instances_are_swept_when_their_node_leaves_the_tree`
/// calls `preem_render::forget_scope` **directly**, which proves the function and
/// not that this file calls it — all three call sites could be deleted with the
/// whole binary suite still green.
#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use super::{
        MountedCard, Scope, SlotRender, build_panel_child, build_region,
        forget_previous_panel_scope, preem_render, reconcile_region, render_active_panel,
    };
    use hytte::adw;
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk::{self, glib, prelude::*};
    use hytte::ui::{EventKind as UiEventKind, NodeId, Reconciler};
    use hytte_plugin_proto::{HostMsg, preem as vocab, wire};
    use std::cell::RefCell;
    use std::rc::Rc;
    use tokio::sync::mpsc;

    /// Run the GTK main loop until it has nothing left to dispatch, so the panel
    /// child's `spawn_local`'d subscription is actually polled.
    fn pump() {
        while glib::MainContext::default().iteration(false) {}
    }

    /// Mount a real drawer plugin child on `panels`/`active`, inside a window,
    /// dropping every *local* strong reference to the child.
    ///
    /// That ownership mirrors `modal.rs` exactly: `build_stack` passes
    /// `plugin_panel_slot()` straight into `stack.add_named` as a temporary, so
    /// the drawer's widget tree holds the only reference and destroying the
    /// window really does dispose the child. A test that kept a handle would
    /// keep the child alive and never see its `connect_destroy` run at all.
    fn mount_panel_child(
        panels: &Mutable<Vec<SlotRender>>,
        active: &Mutable<Option<String>>,
    ) -> gtk::Window {
        let child = build_panel_child(panels.signal_cloned(), active.signal_cloned());
        let window = gtk::Window::new();
        window.set_child(Some(&child));
        drop(child);
        window
    }

    /// Mount a real bar-chip region on `renders`, inside a window, dropping every
    /// *local* strong reference to the region container.
    ///
    /// That ownership mirrors production exactly: `main.rs`'s `build_bar` passes
    /// `plugins::bar_left_slot()` straight into the bar group as a temporary
    /// (`main.rs:412-416`), and `overlays::sidebar::build_card` does the same with
    /// `card.append(&crate::plugins::sidebar_lead_slot())` (`sidebar.rs:452`), so
    /// the surface's widget tree holds the only reference. A test that kept a
    /// handle would keep the container alive and never see its `connect_destroy`
    /// run at all.
    fn mount_region(renders: &Mutable<Vec<SlotRender>>) -> gtk::Window {
        let region = build_region(
            renders.signal_cloned(),
            gtk::Orientation::Horizontal,
            "ts-plugin-chip",
        );
        let window = gtk::Window::new();
        window.set_child(Some(&region));
        drop(region);
        window
    }

    /// A tree of one preem node, so a scope's instance count is a non-zero
    /// number to watch fall to zero.
    fn preem_tree(id: &str) -> wire::Node {
        wire::Node::Preem {
            id: Some(id.to_owned()),
            classes: vec![],
            widget: Box::new(vocab::PreemWidget::DotMatrix {
                config: vocab::DotMatrixConfig::default(),
                state: vocab::DotMatrixState { text: id.into() },
            }),
        }
    }

    fn render_of(plugin_id: &str, tx: &mpsc::Sender<HostMsg>) -> SlotRender {
        SlotRender {
            plugin_id: plugin_id.to_owned(),
            order: 0,
            generation: 1,
            tree: preem_tree("chip"),
            panel: Some(preem_tree("panel")),
            outbound: tx.clone(),
        }
    }

    /// A card leaving its region releases its preem renderer instances.
    ///
    /// **Deletion check:** removing the `preem_render::forget_scope` call from
    /// `reconcile_region`'s retain loop turns the final assertion red
    /// (`left: 1, right: 0`).
    #[gtk::test]
    fn card_leaving_its_region_releases_its_preem_scope() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let cards: Rc<RefCell<Vec<MountedCard>>> = Rc::new(RefCell::new(Vec::new()));
        let scope = Scope::card("leaver");

        reconcile_region(
            &container,
            &cards,
            &[render_of("leaver", &tx)],
            "ts-plugin-chip",
        );
        assert_eq!(
            preem_render::instance_count(&scope),
            1,
            "the mounted card's preem node must have a live renderer instance",
        );

        // The plugin disconnects: its render leaves the region's mailbox.
        reconcile_region(&container, &cards, &[], "ts-plugin-chip");
        assert!(cards.borrow().is_empty(), "the card itself must be gone");
        assert_eq!(
            preem_render::instance_count(&scope),
            0,
            "a card leaving its region must release the tree's renderer instances, \
             not park them for the session",
        );
    }

    /// A mount **region** must be freeable — nothing [`build_region`] spawns may
    /// hold the container it renders into alive (#909, the twin of #903's pin on
    /// the drawer panel child).
    ///
    /// **Deletion check:** capturing `container.clone()` in the render
    /// subscription instead of `container.downgrade()` turns both assertions red
    /// at once — the container stays upgradeable, its `connect_destroy` never
    /// fires, and the stranded region goes on reconciling a plugin it never had
    /// into a detached widget tree, building renderer instances for it. That is
    /// the state `main` shipped: one live region per monitor per mount per
    /// hot-plug.
    #[gtk::test]
    fn a_destroyed_region_is_freed_and_stops_rendering() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let renders = Mutable::new(vec![render_of("region-stranded", &tx)]);

        let region = build_region(
            renders.signal_cloned(),
            gtk::Orientation::Horizontal,
            "ts-plugin-chip",
        );
        let region_weak = region.downgrade();
        let window = gtk::Window::new();
        window.set_child(Some(&region));
        // The bar group / sidebar card holds the only reference in production, so
        // the test must too — see [`mount_region`].
        drop(region);
        pump();
        assert_eq!(
            preem_render::instance_count(&Scope::card("region-stranded")),
            1,
            "the mounted chip must have a live renderer instance before teardown",
        );

        window.destroy();
        drop(window);
        pump();

        assert!(
            region_weak.upgrade().is_none(),
            "the region container must be freed with its surface: a subscription \
             holding a strong clone of the very container whose `destroy` handler \
             is supposed to abort it pins the container against its own teardown, \
             so hot-plug strands one live region per monitor per mount forever",
        );

        // A stranded region would still be subscribed, and would mount this
        // brand-new plugin's card — reconciler, widgets and renderer instances —
        // into a detached widget tree nobody can see.
        renders.set(vec![render_of("region-newcomer", &tx)]);
        pump();
        assert_eq!(
            preem_render::instance_count(&Scope::card("region-newcomer")),
            0,
            "a destroyed region must stop rendering: an orphan still building \
             renderer instances is what keeps their kit buffers resident and \
             `any_animating()` true (#897)",
        );
    }

    /// The **sidebar's** teardown path frees its plugin regions too — through
    /// the deeper production chain the bare-window tests above skip, and in
    /// `overlays::sidebar::close_all`'s real order.
    ///
    /// `close_all` (`overlays/sidebar.rs:744-773`) destroys the toplevel while
    /// `SidebarPanel` still holds its own `revealer` clone (`sidebar.rs:118-133`
    /// — note it holds **no** reference to the `card` box the regions live in),
    /// and only the end of the drain loop drops that record. So the region's
    /// `destroy` fires one refcount step *after* `window.destroy()` returns, not
    /// during it. This models that nesting and that order.
    ///
    /// **Deletion check:** the same one as
    /// [`a_destroyed_region_is_freed_and_stops_rendering`] — re-pinning the
    /// subscription with `container.clone()` turns it red.
    #[gtk::test]
    fn a_destroyed_sidebar_surface_frees_its_plugin_regions() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let renders = Mutable::new(vec![render_of("region-sidebar", &tx)]);

        // window → revealer → AdwClamp → card → region: `sidebar::install`'s
        // nesting (`sidebar.rs:290-303`), with the region appended into the card
        // as a temporary exactly as `build_card` does (`sidebar.rs:452`).
        let region = build_region(
            renders.signal_cloned(),
            gtk::Orientation::Vertical,
            "ts-plugin-card",
        );
        let region_weak = region.downgrade();
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.append(&region);
        drop(region);
        let clamp = adw::Clamp::builder().child(&card).build();
        drop(card);
        let revealer = gtk::Revealer::new();
        revealer.set_child(Some(&clamp));
        drop(clamp);
        let window = gtk::Window::new();
        window.set_child(Some(&revealer));
        pump();

        // `close_all`'s order: destroy the toplevel first, with the panel record
        // (here: the local `revealer`) still holding its clone…
        window.destroy();
        drop(window);
        pump();
        // …then let the drained record drop.
        drop(revealer);
        pump();

        assert!(
            region_weak.upgrade().is_none(),
            "a hot-unplugged sidebar must free the plugin regions inside its \
             card: nothing on the sidebar's own teardown path holds them (its \
             two subscriptions and its settle timer are aborted/cancelled \
             explicitly, `sidebar.rs:754`/`:758`/`:768-770`), so the only thing \
             that can strand them is the region pinning itself (#909)",
        );
    }

    /// A card's preem scope is **cross-monitor** — `Scope::card(plugin_id)` is
    /// shared by every monitor's copy of that card — so releasing it is
    /// [`reconcile_region`]'s retain loop's job, driven by the shared render
    /// list, and *not* the region's teardown's. Destroying one monitor's region
    /// must therefore neither drop a scope a surviving monitor is still showing,
    /// nor stop the surviving region from releasing it when the plugin leaves.
    ///
    /// This is the invariant #909's fix is built on rather than a regression
    /// guard for it: it is green on `main` too. **Deletion check:** adding a
    /// `preem_render::forget_scope` for each mounted card to [`build_region`]'s
    /// destroy handler turns the mid-test assertion red (`left: 0, right: 1`) —
    /// which is exactly why that release is not part of the fix.
    #[gtk::test]
    fn a_destroyed_regions_card_scope_is_released_by_a_surviving_region() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let renders = Mutable::new(vec![render_of("region-shared", &tx)]);
        let scope = Scope::card("region-shared");

        // Two monitors' regions, mirroring the one shared mailbox.
        let hot_unplugged = mount_region(&renders);
        let survivor = mount_region(&renders);
        pump();
        assert_eq!(
            preem_render::instance_count(&scope),
            1,
            "both monitors' copies of a card share one renderer instance",
        );

        hot_unplugged.destroy();
        drop(hot_unplugged);
        pump();
        assert_eq!(
            preem_render::instance_count(&scope),
            1,
            "one monitor's region going away must not drop the renderer instances \
             the surviving monitor's copy of the same card is still painting",
        );

        // The plugin disconnects: the shared render list empties, and the
        // surviving region's retain loop is what releases the scope.
        renders.set(Vec::new());
        pump();
        assert_eq!(
            preem_render::instance_count(&scope),
            0,
            "a card leaving the shared render list must still release its \
             renderer instances through the surviving region's retain loop",
        );
        drop(survivor);
    }

    /// Switching the drawer to another plugin's panel — and closing the drawer —
    /// each release the panel scope that was on screen.
    ///
    /// **Deletion check:** removing either `forget_previous_panel_scope` call
    /// from `render_active_panel` turns one of the two assertions red.
    #[gtk::test]
    fn panel_switch_and_close_release_the_previous_panels_preem_scope() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let mut reconciler = Reconciler::new(&root, |_: NodeId, _: UiEventKind| {});
        let outbound: Rc<RefCell<Option<mpsc::Sender<HostMsg>>>> = Rc::new(RefCell::new(None));
        let shown: Rc<RefCell<Option<Scope>>> = Rc::new(RefCell::new(None));
        let (first, second) = (Scope::panel("first"), Scope::panel("second"));

        render_active_panel(
            &mut reconciler,
            &outbound,
            &shown,
            Some(&render_of("first", &tx)),
        );
        assert_eq!(preem_render::instance_count(&first), 1);

        // Switch to another plugin's panel: the first one's instances go.
        render_active_panel(
            &mut reconciler,
            &outbound,
            &shown,
            Some(&render_of("second", &tx)),
        );
        assert_eq!(
            preem_render::instance_count(&first),
            0,
            "switching panels must release the outgoing panel's renderer instances",
        );
        assert_eq!(preem_render::instance_count(&second), 1);

        // Close the drawer: `modal.rs` clears the selection, which arrives here
        // as `None`.
        render_active_panel(&mut reconciler, &outbound, &shown, None);
        assert_eq!(
            preem_render::instance_count(&second),
            0,
            "closing the drawer must release the shown panel's renderer instances",
        );
    }

    /// Re-rendering the **same** panel keeps its instances — a plugin pushing a
    /// new frame must not restart the animation it is mid-way through.
    #[gtk::test]
    fn re_rendering_the_same_panel_keeps_its_preem_scope() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let mut reconciler = Reconciler::new(&root, |_: NodeId, _: UiEventKind| {});
        let outbound: Rc<RefCell<Option<mpsc::Sender<HostMsg>>>> = Rc::new(RefCell::new(None));
        let shown: Rc<RefCell<Option<Scope>>> = Rc::new(RefCell::new(None));
        let scope = Scope::panel("steady");

        let render = render_of("steady", &tx);
        render_active_panel(&mut reconciler, &outbound, &shown, Some(&render));
        let built = preem_render::probe(&scope, Some("panel"));
        assert!(built.is_some(), "the panel's preem node must have mapped");
        render_active_panel(&mut reconciler, &outbound, &shown, Some(&render));
        assert_eq!(
            preem_render::probe(&scope, Some("panel")),
            built,
            "the same panel re-rendering must neither rebuild nor re-apply",
        );
    }

    /// The drawer plugin child must be **freeable** — nothing the mount spawns
    /// may hold its own root alive (#903, the `hytte-reactive` `bind` contract at
    /// `crates/hytte-reactive/src/bind.rs:16-30` applied to a hand-rolled
    /// subscription).
    ///
    /// **Deletion check:** building the `Reconciler` over `root` instead of the
    /// inner `canvas` turns every assertion below red at once — the widget stays
    /// upgradeable, `destroy` never fires, and the stranded child goes on mapping
    /// preem nodes for whatever panel is activated next. That is the state `main`
    /// shipped, and it is why the destroy-time release this file now performs
    /// could not have worked as a fix on its own.
    #[gtk::test]
    fn a_destroyed_drawer_child_is_freed_and_stops_rendering() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let panels = Mutable::new(vec![render_of("stranded", &tx)]);
        let active = Mutable::new(Some("stranded".to_owned()));

        let child = build_panel_child(panels.signal_cloned(), active.signal_cloned());
        let child_weak = child.downgrade();
        let window = gtk::Window::new();
        window.set_child(Some(&child));
        // The drawer stack holds the only reference in production, so the test
        // must too — see [`mount_panel_child`].
        drop(child);
        pump();

        window.destroy();
        drop(window);
        pump();

        assert!(
            child_weak.upgrade().is_none(),
            "the drawer child must be freed with its window: a `Reconciler` built \
             over `root` keeps a strong clone of it inside the very subscription \
             `root`'s `destroy` handler is supposed to abort, so the widget pins \
             itself and hot-plug strands one live child per monitor forever",
        );

        // A stranded child would still be subscribed, and would map this newly
        // active panel's preem nodes into a widget tree nobody can see.
        panels.set(vec![render_of("next", &tx)]);
        active.set(Some("next".to_owned()));
        pump();
        assert_eq!(
            preem_render::instance_count(&Scope::panel("next")),
            0,
            "a destroyed drawer child must stop rendering: an orphan still \
             building renderer instances is what keeps their kit buffers \
             resident and `any_animating()` true (#897)",
        );
    }

    /// A monitor hot-plug — `modal::close_all` destroying every drawer window
    /// and *then* broadcasting `set_active_panel(None)` — must still release the
    /// open plugin panel's preem renderer instances (#903).
    ///
    /// **Deletion check:** dropping `forget_previous_panel_scope` from
    /// [`build_panel_child`]'s `connect_destroy` handler turns the final
    /// assertion red (`left: 1, right: 0`) — which is exactly the state `main`
    /// shipped: the child's `handle.abort()` kills the only subscription that
    /// could have acted on the `None`, and the `None` is delivered a
    /// main-context iteration *later* than `close_all` returns, so reordering
    /// the broadcast ahead of the destroys would not have saved it either.
    #[gtk::test]
    fn close_all_ordering_releases_the_open_panels_preem_scope() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let panels = Mutable::new(vec![render_of("hotplug", &tx)]);
        let active = Mutable::new(Some("hotplug".to_owned()));
        let scope = Scope::panel("hotplug");

        let window = mount_panel_child(&panels, &active);
        pump();
        assert_eq!(
            preem_render::instance_count(&scope),
            1,
            "the open plugin panel must have a live renderer instance to leak",
        );

        // `modal::close_all`, in its real order: every drawer window destroyed
        // first (the child's `connect_destroy` runs inside `destroy()`), then
        // the selection cleared.
        window.destroy();
        drop(window);
        active.set(None);
        // The `None` reaches a subscriber only on the *next* main-context
        // iteration — later than `close_all` returns — so pumping here is more
        // generous than production ever is.
        pump();

        assert_eq!(
            preem_render::instance_count(&scope),
            0,
            "a hot-plug teardown must release the open panel's renderer instances \
             rather than park them for the session: they hold their kit buffers, \
             and a mid-animation one keeps `any_animating()` true forever (#903)",
        );
    }

    /// The **class** the fix closes, not just `close_all`'s instance: a drawer
    /// child that is destroyed releases the panel scope it was showing even when
    /// no selection change ever arrives. Teardown does not depend on anyone
    /// still being subscribed.
    #[gtk::test]
    fn a_destroyed_drawer_child_releases_its_panel_scope() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let panels = Mutable::new(vec![render_of("orphan", &tx)]);
        let active = Mutable::new(Some("orphan".to_owned()));
        let scope = Scope::panel("orphan");

        let window = mount_panel_child(&panels, &active);
        pump();
        assert_eq!(preem_render::instance_count(&scope), 1);

        // No broadcast at all — just the mount going away.
        window.destroy();
        drop(window);
        pump();

        assert_eq!(
            preem_render::instance_count(&scope),
            0,
            "a destroyed drawer child must drop its panel scope on its own, with \
             no `set_active_panel(None)` to prompt it",
        );
    }

    /// The helper itself is idempotent: a second drawer child re-showing the
    /// scope it is already on must not drop a live panel's instances.
    #[gtk::test]
    fn forgetting_the_scope_already_shown_is_a_no_op() {
        adw::init().expect("libadwaita init");
        let scope = Scope::panel("shared");
        let shown: Rc<RefCell<Option<Scope>>> = Rc::new(RefCell::new(Some(scope.clone())));
        forget_previous_panel_scope(&shown, Some(&scope));
        assert_eq!(
            shown.borrow().as_ref(),
            Some(&scope),
            "showing the same scope again must keep it, so the animation continues",
        );
    }
}
