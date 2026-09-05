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
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use hytte::futures_signals::map_ref;
use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, glib, prelude::*};
use hytte::reactive::registry;
use hytte::ui::{Dir as UiDir, EventKind as UiEventKind, Node as UiNode, NodeId, Reconciler};
use hytte_plugin_proto::HostMsg;
use tokio::sync::mpsc;

use super::preem_render::{self, Scope};
use super::pump::Animator;
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

    // This region's animation driver (#897): one frame-clock tick callback on
    // `container`, armed from the mapping pass below and broken by its own tick
    // once every card here has settled.
    //
    // The scopes closure captures the card list and nothing else — in particular
    // **not** `container`. A strong clone of the mount inside a closure the mount
    // itself (transitively) owns is the self-pin #909 measured on this very
    // function, and the tick callback GTK installs on `container` holds this
    // `Rc<Animator>`. The card roots the list does reach are *children* of the
    // container, and in GTK4 a parent refs its children and never the reverse,
    // so nothing here points back up.
    let animator = Animator::new(move || {
        cards
            .borrow()
            .iter()
            .map(|card| card.preem_scope.clone())
            .collect()
    });
    // The second re-arm point. The tick breaks on an unmapped mount, and a
    // sidebar region's mount/unmap is a `GtkRevealer` flipping `child_visible`
    // (`overlays/sidebar.rs`) — no render, so no mapping pass to re-arm from.
    // The bar's own regions also unmap when they go empty and re-map when a card
    // arrives, and *that* one does come with a mapping pass; this handler is
    // idempotent either way.
    animator.arm_on_map(&container);
    // Bound with [`hytte::reactive::bind`] rather than a hand-rolled apply-loop,
    // and *that* is what makes the region destroyable (#909 — the twin of the pin
    // #903 fixed on the drawer panel child).
    //
    // What shipped before was a `spawn_local`'d `for_each` capturing
    // `container.clone()`, aborted from the container's own `connect_destroy`.
    // The task owned the container, and the only thing that could abort the task
    // was that container's `destroy` handler — which GTK4 emits from
    // `gtk_widget_dispose`, i.e. at refcount zero. So the container pinned itself
    // against its own teardown: the handler never fired, and a hot-plug stranded
    // one fully-live region per monitor per mount, still reconciling plugin cards
    // into a detached widget tree on every render for the rest of the session.
    // Measured before this change: after the surface was destroyed the container
    // still upgraded from a `glib::WeakRef` **and** still mounted a renderer
    // instance for a plugin id it had never seen.
    //
    // `bind` is exactly the contract that was missing
    // (`crates/hytte-reactive/src/bind.rs:16-46`): it holds the widget only
    // through a `glib::WeakRef`, `break`s its loop the first time the upgrade
    // fails — the *guarantee*, covering a widget freed without emitting `destroy`
    // — and installs the same eager `abort_on_destroy` as residual trimming.
    // Reaching for the helper instead of re-typing its loop is not only taste:
    // `nix/lint-bind-pins.py` finds its work by scanning `bind*(` **call sites**
    // (`nix/lint-bind-pins.py:211`), so a hand-rolled copy is invisible to the
    // one guard that exists for this exact contract, and the scan's `0 pin(s)`
    // would say nothing about this file in either direction.
    //
    // #903's inner-`canvas` split is **not** needed here, because the strong refs
    // the apply closure holds already point *down* the tree rather than back at
    // the container. `Reconciler` keeps a strong clone of the box it was built
    // over (`crates/hytte-ui/src/widget_tree.rs:355`), but [`reconcile_region`]
    // builds one per card over that card's own `root` — a *child* of this
    // container — and in GTK4 a parent refs its children, never the reverse. A
    // canvas here would buy nothing and cost a level: [`reconcile_region`] also
    // `set_visible`s the container itself (the empty-region hide, which has to
    // stay on the widget the parent bar group lays out) and the row's inter-chip
    // `spacing` lives on it too.
    //
    // Nothing is released on the teardown path, deliberately: a card's scope
    // (`Scope::card(plugin_id)`) is shared by every monitor's copy of that card,
    // so forgetting it from one region's teardown would drop renderer instances a
    // surviving monitor is still painting. [`reconcile_region`]'s retain loop is
    // the release site, driven by the shared render list every live region
    // observes. The corner that leaves — a plugin leaving while *no* region is
    // alive to observe it, where a stranded-but-still-subscribed region used to
    // reclaim the scope by accident — is closed by **#921**'s
    // `pump::drive_scope_releaser`, a subscriber to the same render mailboxes
    // that needs no region (and no monitor) to exist. Deliberately *not* closed
    // here: mutation (b) of the #920 review showed a per-card `forget_scope` on
    // this teardown path breaks the cross-monitor invariant `gtk_tests` pins.
    hytte::reactive::bind(signal, &container, move |container, renders| {
        reconcile_region(container, &cards_for_signal, &renders, card_class);
        // **The re-arm point** (#897), and the reason it is here rather than
        // inside `reconcile_region`: a mapping pass is the only thing that can
        // give a settled widget somewhere to go, and `reconcile_region` holds
        // `cards.borrow_mut()` for its whole body while the scopes closure
        // needs a shared borrow of the same cell. Arming after it returns is
        // both correct and the only order that does not panic.
        //
        // `container` is `bind`'s own closure parameter, not a captured clone —
        // the contract `nix/lint-bind-pins.py` scans this call site for.
        animator.ensure_armed(container);
    });

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
    // Reveal the region exactly when it holds at least one card, so an empty
    // region (no plugin mounted here yet) adds no spacing to its parent group.
    //
    // **Before the borrow, deliberately.** `set_visible` maps the container and
    // GTK emits `map` synchronously from inside this call, so since #897 it
    // re-enters this module: `Animator::arm_on_map`'s handler reads the very
    // `cards` cell this function is about to take a `borrow_mut()` on. Ordered
    // the other way it is a `RefCell already mutably borrowed` panic on the
    // GTK main thread — the same class of re-entrancy #627/#630/#631/#632/#638/
    // #643 fixed across the shell, and one the tests caught here.
    container.set_visible(!renders.is_empty());

    let mut cards = cards.borrow_mut();

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
    let reconciler = Reconciler::new(&canvas, move |id: NodeId, kind: UiEventKind| {
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

    // This drawer child's animation driver (#897): one frame-clock tick callback
    // on `root`, armed after each render in [`drive_panel_child`]. Its scope set
    // is at most one — whichever panel is on screen — which is why a closed
    // drawer costs nothing twice over: the set is empty *and* the widget is
    // unmapped, so the clock is neither armed nor ticking.
    //
    // Captures the shown-scope cell only. A strong `root` here would re-pin
    // exactly what the `canvas` split above unpinned (#903), and the tick
    // callback GTK installs on `root` owns this `Rc<Animator>`.
    let shown_for_scopes = shown_scope.clone();
    let animator = Animator::new(move || shown_for_scopes.borrow().iter().cloned().collect());
    // The drawer's own re-arm on becoming visible. Belt and braces here rather
    // than load-bearing: `modal.rs` hides the drawer *toplevel*, so this child's
    // ticks stop for GTK's own reason too, and opening the drawer republishes
    // `active_panel_id`, which is a mapping pass. It costs one signal handler and
    // removes the need to reason about which of the two arrives first.
    animator.arm_on_map(&root);

    // Hand-rolled rather than routed through [`hytte::reactive::bind`], which is
    // what [`build_region`] above uses — and this is the one of the two mounts
    // that genuinely cannot. `bind`'s apply closure is called only *while* the
    // widget is alive; it has no hook on the emission where the upgrade fails,
    // and this mount has to do work there: release the panel scope it was
    // showing. So the loop is written out, with `bind`'s own two legs
    // (`crates/hytte-reactive/src/bind.rs:16-46`) — the `WeakRef` upgrade as the
    // guarantee, the `destroy`-driven abort below as residual trimming — plus the
    // release the helper cannot express. Being outside `bind` also puts this site
    // outside `nix/lint-bind-pins.py`'s reach, so the pin question here is
    // answered by `the_panel_loop_exits_once_its_root_is_gone` in `gtk_tests`
    // instead of by the scan.
    let handle = glib::MainContext::default().spawn_local(drive_panel_child(
        active,
        root.downgrade(),
        reconciler,
        outbound,
        shown_scope,
        animator,
    ));

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
    // *eager* leg of that contract; the `WeakRef` upgrade in [`drive_panel_child`]
    // is the guarantee, and it releases the same scope on the path this handler
    // cannot see (#909's first nit).
    //
    // No sibling-monitor hazard, and since #921 that is a property of *this*
    // release rather than of its callers. It used to be the latter:
    // `active_panel_id` is one global selection and the only two things that
    // destroy a drawer window are `modal::close_all` (`modal.rs:1161`) and
    // `overlays::sidebar::close_all` (`overlays/sidebar.rs:772`), both driven
    // from the same `monitors_changed` emission (`main.rs:240-241`), which tears
    // every monitor's surfaces down together — so no partial teardown existed to
    // drop a scope another monitor was still showing. But `Scope::panel` is
    // cross-monitor for exactly the reason `Scope::card` is, and this handler
    // forgot it unconditionally from *one* child's teardown, so a future
    // per-monitor drawer rebuild would have blanked a sibling monitor's panel
    // (the #920 review measured it as probe P5: 1 → 0 on the first destroy).
    // [`forget_previous_panel_scope`] now refcounts the scope across children,
    // so this fires the release and only the **last** holder's release reaches
    // `preem_render`. And it stays idempotent regardless:
    // `forget_previous_panel_scope` no-ops on an already-cleared cell (so the
    // count is never double-decremented), and `preem_render::forget_scope` is a
    // `HashMap::remove`.
    root.connect_destroy(move |_| {
        handle.abort();
        forget_previous_panel_scope(&shown_at_destroy, None);
    });
    root.upcast()
}

/// The drawer plugin child's render apply-loop: [`hytte::reactive::bind`]'s two
/// legs plus the panel-scope release `bind` has no hook for (see
/// [`build_panel_child`]).
///
/// Extracted from the `spawn_local` call so `gtk_tests` can drive the `WeakRef`
/// leg **directly**, which is the only way to reach it: in production the
/// `connect_destroy` abort always wins, because GTK4 emits `destroy` from
/// `gtk_widget_dispose` and there is no way to free a widget without it. Left
/// inline, `break` → `continue` here is a silently green mutation — the residual
/// this leg exists to trim would be re-introduced with the whole suite passing.
/// `the_panel_loop_exits_once_its_root_is_gone` is what that buys, and it is the
/// same argument that put `render_active_panel` and [`build_panel_child`] behind
/// their own seams.
async fn drive_panel_child(
    active: impl Signal<Item = Option<SlotRender>>,
    root: glib::WeakRef<gtk::Box>,
    mut reconciler: Reconciler,
    outbound: Rc<RefCell<Option<mpsc::Sender<HostMsg>>>>,
    shown_scope: Rc<RefCell<Option<Scope>>>,
    animator: Rc<Animator>,
) {
    let mut active = std::pin::pin!(active);
    while let Some(slot) = std::future::poll_fn(|cx| active.as_mut().poll_change(cx)).await {
        let Some(mounted) = root.upgrade() else {
            // The mount is gone and nobody aborted us: release the panel scope
            // that was on screen and stop, rather than parking on this signal for
            // the session holding the reconciler, the canvas and the outbound.
            forget_previous_panel_scope(&shown_scope, None);
            break;
        };
        render_active_panel(&mut reconciler, &outbound, &shown_scope, slot.as_ref());
        // **The re-arm point** for this mount (#897): the render above is the
        // panel's whole mapping pass, so it is the only place a settled panel
        // can be given something to animate — a plugin going active, or a state
        // change on the one already showing. `mounted` is the upgrade this loop
        // already performs; the animator holds no widget of its own.
        animator.ensure_armed(&mounted);
    }
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

thread_local! {
    /// How many live drawer panel children are currently showing each panel
    /// scope — the refcount that makes the panel-scope release **monitor
    /// independent** (#921).
    ///
    /// `Scope::panel(plugin_id)` is keyed without a connector, exactly like
    /// `Scope::card`, because every monitor's drawer child mirrors the one
    /// globally-active panel and they all share one set of renderer instances.
    /// The release, though, was per child: whichever child was destroyed first
    /// called `preem_render::forget_scope` unconditionally and dropped the
    /// instances a sibling monitor was still painting (the #920 review measured
    /// it as probe P5 — two children showing, one destroyed, `instance_count`
    /// 1 → 0). Unreachable through today's `modal::close_all` / sidebar
    /// `close_all`, which tear every monitor's surfaces down in one
    /// `monitors_changed` dispatch, but that is a property of the *callers*, and
    /// the region side got an invariant test for the identical hazard while this
    /// side got none.
    ///
    /// One count per scope, incremented when a child starts showing it and
    /// decremented when it stops (a switch, a drawer close, or the child's
    /// teardown); the instances are forgotten on the 1 → 0 edge only. Entries
    /// are removed at zero, so the map holds at most one key per plugin whose
    /// panel is on screen right now. GTK-main-thread-only, like every other
    /// piece of drawer-child state.
    static PANEL_SCOPE_HOLDERS: RefCell<HashMap<Scope, usize>> = RefCell::new(HashMap::new());
}

/// Record that one more drawer panel child is showing `scope`.
fn retain_panel_scope(scope: &Scope) {
    PANEL_SCOPE_HOLDERS.with_borrow_mut(|holders| {
        *holders.entry(scope.clone()).or_insert(0) += 1;
    });
}

/// Drop one drawer panel child's hold on `scope`, returning `true` when that was
/// the **last** one — i.e. when the caller should actually forget the renderer
/// instances.
///
/// An unknown scope reads as "nobody is holding it", which is the safe answer
/// for both ways it can happen: a `shown` cell built by hand in a test, and a
/// scope whose count already fell to zero. Either way the caller's
/// [`preem_render::forget_scope`] is a `HashMap::remove` miss, so answering
/// `true` there costs nothing and never silently keeps a live scope pinned.
fn release_panel_scope(scope: &Scope) -> bool {
    PANEL_SCOPE_HOLDERS.with_borrow_mut(|holders| match holders.get_mut(scope) {
        Some(holders_left) if *holders_left > 1 => {
            *holders_left -= 1;
            false
        }
        _ => {
            holders.remove(scope);
            true
        }
    })
}

/// Release a **departed** plugin's panel scope: drop its renderer instances and
/// its [`PANEL_SCOPE_HOLDERS`] entry together, so the two cannot diverge.
///
/// This is what `pump::drive_scope_releaser` calls instead of reaching for
/// `preem_render::forget_scope` itself (`pump` cannot see this module's private
/// map anyway). The release is deliberately **unconditional** — it does not go
/// through [`release_panel_scope`] — because routing it through the refcount
/// would re-open #921 exactly: a plugin that exits while a drawer child is
/// still holding its panel would keep its instances resident until that child
/// happened to blank, which with no monitors left never happens. "The plugin is
/// gone" outranks "someone is still showing it"; every live child is about to
/// be told `None` by the same mailbox emission and will render blank.
///
/// Dropping the map entry alongside is what keeps the two bookkeepings from
/// disagreeing (the #921 review's MEDIUM-2, probe R2B): a count left at 1 over
/// an empty store is a hold nothing will ever pair a release with, and it makes
/// every later release of that scope answer "not the last holder" — a leak that
/// survives the plugin it came from.
///
/// The residual, stated: a `shown` cell that outlived the departure *and* is
/// released only after a **later** session of the same plugin id took a hold
/// would decrement that new session's count. Not reachable through today's
/// wiring — every live child is woken by the departure emission and blanks
/// through [`forget_previous_panel_scope`] (the review enumerated the paths),
/// and `install_scope_releaser` is spawned from `plugins::install` before any
/// drawer child exists, so on that shared emission the releaser runs first and
/// the children release into an already-empty map.
pub(super) fn forget_departed_panel_scope(scope: &Scope) {
    PANEL_SCOPE_HOLDERS.with_borrow_mut(|holders| {
        holders.remove(scope);
    });
    preem_render::forget_scope(scope);
}

/// How many drawer panel children are currently holding `scope` — the refcount
/// [`forget_previous_panel_scope`] maintains, exposed so a test can assert the
/// *bookkeeping* rather than only its visible effect on `instance_count`.
///
/// Without this, `forget_departed_panel_scope`'s map drop has no deletion
/// check: an instance count of zero is satisfied by both the honest spelling
/// and the divergent one.
///
/// Gated on `system-tests` rather than plain `test` because its only callers
/// are in [`gtk_tests`], which carries that gate — a bare `#[cfg(test)]` here
/// is `dead_code` in the hermetic bucket that `cargo test --workspace` and the
/// package build's `doCheck` run.
#[cfg(all(test, feature = "system-tests"))]
pub(super) fn panel_scope_holders(scope: &Scope) -> usize {
    PANEL_SCOPE_HOLDERS.with_borrow(|holders| holders.get(scope).copied().unwrap_or(0))
}

/// Record which panel scope the drawer child is about to show, dropping the
/// preem renderer instances of the one it was showing before (#883) — unless it
/// is the same scope, in which case a re-render must *keep* the animation state
/// it is mid-way through, or unless **another monitor's** drawer child is still
/// showing it (#921).
///
/// `active_panel_id` is a single shared handle, so every monitor's drawer child
/// switches together — but "together" is a property of the selection, not of
/// this function, and a child can also stop showing a panel on its own
/// (`build_panel_child`'s `connect_destroy`, and [`drive_panel_child`]'s weak
/// leg). So the release is refcounted through [`PANEL_SCOPE_HOLDERS`]: this
/// child's hold is dropped every time, and the instances go only when it was the
/// last hold. Destroying one monitor's drawer child while a sibling paints the
/// same panel therefore leaves the instance count untouched — the panel-side
/// mirror of the invariant
/// `a_destroyed_regions_card_scope_is_released_by_a_surviving_region` pins for
/// cards.
///
/// **Closing the drawer counts as leaving**, not only switching plugins:
/// `modal.rs` clears `active_panel_id` on close, which lands here as `None`. So
/// a close/reopen cycle starts the panel's animations over — needles at rest,
/// phosphor dark, flip boards blank — rather than resuming mid-swing. That is
/// the deliberate trade (a closed drawer should not hold a phosphor buffer per
/// plugin for the session), but it is a visible behaviour on glass, not just a
/// teardown detail. Unchanged by the refcount: every monitor's child is told
/// `None` by the same broadcast, so the count still reaches zero.
///
/// Still idempotent, which the double-release paths rely on: a second call with
/// the same `next` early-returns on the equality check above the refcount, so a
/// child cannot take two holds on one scope, and a destroy handler firing after
/// [`drive_panel_child`] already cleared the cell sees `None == None` and returns
/// before touching the count.
fn forget_previous_panel_scope(shown: &Rc<RefCell<Option<Scope>>>, next: Option<&Scope>) {
    let mut shown = shown.borrow_mut();
    if shown.as_ref() == next {
        return;
    }
    if let Some(previous) = shown.take()
        && release_panel_scope(&previous)
    {
        preem_render::forget_scope(&previous);
    }
    if let Some(next) = next {
        retain_panel_scope(next);
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
        Animator, MountedCard, Scope, SlotRender, build_panel_child, build_region,
        drive_panel_child, forget_previous_panel_scope, preem_render, reconcile_region,
        render_active_panel,
    };
    // The #921 releaser lives in `pump` (beside the animation driver whose
    // "still animating" predicate a leaked scope corrupts), but the mounts it has
    // to outlive are built here — so its production-shaped coverage is here too.
    // The #897 animation probes are there for the same reason, read here because
    // the mounts that arm them are these.
    use crate::plugins::pump::{
        animation_arms, drive_scope_releaser, live_animators, live_plugin_ids_signal,
        reset_animation_probes,
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

    /// A tree of one **animating** preem node, at `speed` dots per second — `0.0`
    /// parks the message, which is the vocabulary's own way of spelling
    /// "settled" without changing the widget kind.
    fn marquee_tree(speed: f32) -> wire::Node {
        wire::Node::Preem {
            id: Some("mq".to_owned()),
            classes: vec![],
            widget: Box::new(vocab::PreemWidget::Marquee {
                config: vocab::MarqueeConfig {
                    style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                    window_px: 192,
                    gap_dots: 6,
                    speed_dots_per_sec: speed,
                },
                state: vocab::MarqueeState {
                    text: "A LONG SCROLLING MESSAGE".into(),
                },
            }),
        }
    }

    /// [`render_of`], but the card tree is a marquee at `speed` — so the mount
    /// has something to animate (or, at `0.0`, deliberately does not).
    fn marquee_render_of(plugin_id: &str, tx: &mpsc::Sender<HostMsg>, speed: f32) -> SlotRender {
        SlotRender {
            tree: marquee_tree(speed),
            ..render_of(plugin_id, tx)
        }
    }

    /// [`marquee_render_of`]'s twin for the **panel** tree — the drawer child's
    /// mount reads `panel`, not `tree`.
    fn marquee_panel_of(plugin_id: &str, tx: &mpsc::Sender<HostMsg>, speed: f32) -> SlotRender {
        SlotRender {
            panel: Some(marquee_tree(speed)),
            ..render_of(plugin_id, tx)
        }
    }

    /// One animation step in microseconds. The twin of `plugins::tests::step_us`
    /// — that module is a sibling `#[cfg(test)]` mod with no path here, and both
    /// derive from the same two constants rather than spelling 50 000.
    fn step_us() -> i64 {
        preem_render::MAX_TICK_DT_US / i64::from(preem_render::MAX_CATCHUP_STEPS)
    }

    /// Drive the GTK main loop for `ms` of wall clock, so a **real**
    /// `GdkFrameClock` gets to deliver ticks.
    ///
    /// `iteration(true)` blocks until a source is ready, which is what makes the
    /// counting honest — a spin on `iteration(false)` would starve the frame
    /// clock and measure the test's own loop instead. The timeout is what
    /// guarantees it terminates even in the case the test is *hoping* for, where
    /// no frame ever arrives.
    fn pump_for(ms: u64) {
        let done = Rc::new(std::cell::Cell::new(false));
        let flag = done.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || {
            flag.set(true);
        });
        while !done.get() {
            glib::MainContext::default().iteration(true);
        }
    }

    /// A **real mount** arms one frame-clock tick callback when it has something
    /// to animate, breaks it when everything settles, and arms a *new* one when
    /// a later mapping pass brings motion back (#897).
    ///
    /// The whole arm → park → re-arm cycle, driven through the production
    /// `build_region` — its `bind`, its `reconcile_region`, its `Animator` — and
    /// not a replica of it. The arm *count* is what tells the two failure modes
    /// apart: a mount that never disarms and a mount that correctly re-armed
    /// both end up animating, and only the count says which.
    ///
    /// The tick itself is driven by calling `Animator::tick` at chosen frame
    /// times rather than by waiting on a real `GdkFrameClock`: an unrealized
    /// window in a headless `#[gtk::test]` has no clock ticking, and a test that
    /// waited on wall-clock frames would be a flake. What that leaves untested
    /// here is the one line GTK owns — `add_tick_callback` delivering the tick —
    /// which is the on-glass half of #897's acceptance.
    ///
    /// **Deletion check (a):** dropping `animator.ensure_armed(container)` from
    /// `build_region`'s bind closure turns the *first* assertion red (`left: 0,
    /// right: 1`) — nothing ever arms, and every preem animation in the shell is
    /// frozen. **(b):** removing the `self.armed.set(false)` from
    /// `Animator::tick` leaves the mount believing it is still armed, so the
    /// re-arm is skipped and the last assertion reads `left: 1, right: 2` — the
    /// silently-dead mount a mutation to the break edge alone would otherwise
    /// leave green.
    #[gtk::test]
    fn a_mount_arms_parks_and_re_arms_its_frame_clock() {
        adw::init().expect("libadwaita init");
        reset_animation_probes();
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let renders = Mutable::new(vec![marquee_render_of("anim-mount", &tx, 20.0)]);

        let _window = mount_region(&renders);
        pump();

        assert_eq!(
            animation_arms(),
            1,
            "a mount holding a scrolling marquee must arm exactly one tick callback from its \
             mapping pass — this is the only place an instance can start animating, and a \
             mount that never arms freezes every preem animation for the session",
        );
        let animators = live_animators();
        assert_eq!(animators.len(), 1, "one mount, one animation driver");
        let animator = &animators[0];
        assert!(animator.is_armed(), "…and it must believe it is armed");

        // The mount's very first tick has no baseline to measure from: it stamps
        // one, advances nothing, and still asks for the next frame.
        let baseline = animator.tick(1_000_000);
        assert!(
            baseline.moved.is_empty(),
            "the first tick of a scope only stamps its frame-time baseline",
        );
        assert!(
            baseline.keep_going,
            "…and must not read 'settled' just because it moved nothing, or every animation \
             would park on its own first frame",
        );

        // A tick while it is still scrolling: it moves the card's scope (so the
        // mount's scopes closure really does reach `MountedCard::preem_scope`)
        // and asks for another frame.
        let running = animator.tick(1_000_000 + step_us());
        assert_eq!(
            running.moved,
            vec![Scope::card("anim-mount")],
            "the tick must advance the card this mount is showing, by name",
        );
        assert!(running.keep_going, "…and ask for the next frame");

        // Idempotence, asserted while the mount is **still animating**. The
        // first cut of this test ran the pass after parking the marquee, so
        // `armed == true` and `any_animating_in(..) == false` held at once and
        // either guard alone satisfied it — the `armed` one was never exercised
        // (#926 review M-4). It matters: `request_preem_repaint` runs a mapping
        // pass on every frame that moves, so without the guard an armed,
        // animating mount would gain a new tick callback *per frame*, unbounded,
        // each holding an `Rc<Animator>` and each running a full advance and
        // repaint fan-out.
        renders.set(vec![marquee_render_of("anim-mount", &tx, 20.0)]);
        pump();
        assert_eq!(
            animation_arms(),
            1,
            "a mapping pass over an already-armed, still-animating mount must not stack a \
             second callback",
        );

        // The plugin parks the message: same widget kind, nothing left to scroll.
        renders.set(vec![marquee_render_of("anim-mount", &tx, 0.0)]);
        pump();
        assert_eq!(
            animation_arms(),
            1,
            "…and neither must a pass over a mount with nothing left to animate",
        );

        let settled = animator.tick(2_000_000);
        assert!(
            !settled.keep_going,
            "a settled mount must break its callback"
        );
        assert!(
            !animator.is_armed(),
            "…and record that it did, or the mapping pass that could revive it will see an \
             armed mount and skip the re-arm",
        );

        // Motion comes back.
        renders.set(vec![marquee_render_of("anim-mount", &tx, 20.0)]);
        pump();
        assert_eq!(
            animation_arms(),
            2,
            "the mapping pass must arm a *new* callback for the revived marquee — the parked \
             one is gone, and nothing else in the shell can start it again",
        );
        assert!(animator.is_armed(), "…and the mount knows it");
    }

    /// The **drawer panel** mount arms, parks and re-arms exactly as a region
    /// does (#897).
    ///
    /// The twin of `a_mount_arms_parks_and_re_arms_its_frame_clock`, and it
    /// exists because deleting the panel's re-arm point left the whole suite
    /// green (#926 review M-3) — the region got a real-mount test and the panel,
    /// which #897's body calls the highest-consequence line in the change, got
    /// nothing. With that line gone a drawer panel whose gauge settles or whose
    /// phosphor decays out never animates again for the life of that drawer
    /// child: every later state change renders frozen, silently.
    ///
    /// **Deletion check:** commenting out `animator.ensure_armed(&mounted)` in
    /// `drive_panel_child` turns the first assertion red (`left: 0, right: 1`).
    #[gtk::test]
    fn the_drawer_panel_mount_arms_parks_and_re_arms_its_frame_clock() {
        adw::init().expect("libadwaita init");
        reset_animation_probes();
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let panels = Mutable::new(vec![marquee_panel_of("anim-panel", &tx, 20.0)]);
        let active = Mutable::new(Some("anim-panel".to_owned()));

        let _window = mount_panel_child(&panels, &active);
        pump();

        assert_eq!(
            animation_arms(),
            1,
            "a drawer panel showing a scrolling marquee must arm exactly one tick callback \
             from its render — `drive_panel_child` is the panel's whole mapping pass, and \
             nothing else can start its animation",
        );
        let animators = live_animators();
        assert_eq!(animators.len(), 1, "one mount, one animation driver");
        let animator = &animators[0];
        assert!(animator.is_armed(), "…and it must believe it is armed");

        let _ = animator.tick(1_000_000);
        let running = animator.tick(1_000_000 + step_us());
        assert_eq!(
            running.moved,
            vec![Scope::panel("anim-panel")],
            "the tick must advance the **panel** scope — the panel tree gets its own scope, \
             never the card's",
        );
        assert!(running.keep_going, "…and ask for the next frame");

        // The panel parks its message.
        panels.set(vec![marquee_panel_of("anim-panel", &tx, 0.0)]);
        pump();
        let settled = animator.tick(2_000_000);
        assert!(
            !settled.keep_going,
            "a settled panel must break its callback"
        );
        assert!(!animator.is_armed(), "…and record that it did");

        // Motion comes back on the panel that is already showing.
        panels.set(vec![marquee_panel_of("anim-panel", &tx, 20.0)]);
        pump();
        assert_eq!(
            animation_arms(),
            2,
            "the panel render must arm a *new* callback for the revived marquee",
        );
        assert!(animator.is_armed(), "…and the mount knows it");
    }

    /// An armed mount re-reads its scope set **every tick**, so a card joining
    /// the region afterwards animates too.
    ///
    /// `Animator`'s scopes closure is documented as "re-read every tick and every
    /// arm, never cached", and nothing pinned it: snapshotting the set at arm
    /// time left the whole suite green (#926 review L-1), because every other
    /// test changes a widget's *state* and never the region's *membership*.
    ///
    /// The consequence is a real freeze rather than a style point. A region that
    /// is already armed does not re-arm (that is the idempotence guard), so a
    /// second plugin's chip joining it would never be advanced at all — for as
    /// long as the first one keeps the mount awake.
    ///
    /// **Deletion check:** give `Animator` a `snapshot: RefCell<Vec<Scope>>`
    /// filled in `ensure_armed` and read by `tick` instead of `(self.scopes)()`,
    /// and the final assertion reads `left: ["anim-first"], right: ["anim-first",
    /// "anim-second"]`.
    #[gtk::test]
    fn an_armed_mount_re_reads_its_scopes_so_a_late_card_animates_too() {
        adw::init().expect("libadwaita init");
        reset_animation_probes();
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let renders = Mutable::new(vec![marquee_render_of("anim-first", &tx, 20.0)]);

        let _window = mount_region(&renders);
        pump();
        let animators = live_animators();
        let animator = &animators[0];
        assert_eq!(animation_arms(), 1, "the first card arms the mount");

        let _ = animator.tick(1_000_000);

        // A second plugin joins the same region, also animating. The mount is
        // already armed, so nothing re-arms — the tick has to notice on its own.
        renders.set(vec![
            marquee_render_of("anim-first", &tx, 20.0),
            marquee_render_of("anim-second", &tx, 20.0),
        ]);
        pump();
        assert_eq!(
            animation_arms(),
            1,
            "the mount was already armed, so the join must not arm a second callback — which \
             is exactly why the tick cannot rely on a set captured at arm time",
        );

        // The joiner's scope has no frame-time baseline yet, so its first tick
        // stamps one and advances nothing — the same rule every scope follows.
        // It is the tick *after* that which has to name both.
        let _ = animator.tick(1_000_000 + step_us());
        let mut moved: Vec<String> = animator
            .tick(1_000_000 + 2 * step_us())
            .moved
            .iter()
            .map(|scope| scope.plugin_id().to_owned())
            .collect();
        moved.sort();
        assert_eq!(
            moved,
            vec!["anim-first".to_owned(), "anim-second".to_owned()],
            "the tick must advance the cards the region holds **now**, not the ones it held \
             when it armed: a snapshot leaves the late joiner frozen for as long as its \
             neighbour keeps the mount awake",
        );
    }

    /// A **hidden** mount stops receiving ticks, and starts again when it is
    /// shown — driven by a real `GdkFrameClock`.
    ///
    /// The one line of #897 that only GTK can answer, and the one the first cut
    /// got wrong. GTK gates tick-callback delivery on **realized**, not mapped:
    /// `gtk_widget_add_tick_callback` begins updating under `if (priv->realized
    /// …)`, `gtk_widget_real_unrealize` is what disconnects, and neither
    /// `gtk_widget_unmap` nor `gtk_widget_set_child_visible` touches either. So
    /// the sidebar — a layer surface presented **once for the process lifetime**,
    /// whose toggle is a `GtkRevealer` flipping `child_visible` — kept ticking at
    /// the full display refresh while "closed". The #926 review measured 30
    /// deliveries per 500 ms either way, and a marquee's `animates()` is
    /// config-driven so it never settles: one scrolling card in a closed sidebar
    /// cost 60–144 wakeups a second per monitor against #883's 20 process-wide.
    ///
    /// The fixture is that exact shape: a presented window, a revealer with no
    /// transition, the real region mount inside it. No `Animator::tick` is called
    /// by hand anywhere in this test — every tick counted is one GTK delivered.
    ///
    /// The marquee crawls at 0.01 dots/s on purpose. `Renderer::animates` for a
    /// marquee is config-driven, so it keeps the callback armed indefinitely,
    /// while `advance` compares whole dots and so reports **no** movement across
    /// the couple of seconds this test spans — which keeps the tick closure out
    /// of `request_preem_repaint`, and therefore out of the registry a
    /// `#[gtk::test]` has no booted `App` to provide. Installing a fixture
    /// `PluginHandles` instead would leave process-global registry state behind
    /// on the shared `gtk::test_synced` thread. What this test measures is tick
    /// *delivery*; what a delivered tick then does is
    /// `a_mount_arms_parks_and_re_arms_its_frame_clock`'s and `pump_tests`'s job.
    ///
    /// **Deletion check:** remove the `if !widget.is_mapped()` break from
    /// `Animator::ensure_armed`'s tick closure and the middle assertion goes red
    /// with a full window's worth of ticks (~18 at 60 Hz over 300 ms) where at
    /// most one is allowed.
    #[gtk::test]
    fn a_hidden_mount_stops_ticking_and_resumes_when_shown() {
        adw::init().expect("libadwaita init");
        reset_animation_probes();
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let renders = Mutable::new(vec![marquee_render_of("anim-hidden", &tx, 0.01)]);

        let region = build_region(
            renders.signal_cloned(),
            gtk::Orientation::Horizontal,
            "ts-plugin-chip",
        );
        // A revealer with no transition, so `set_reveal_child` unmaps the child
        // immediately rather than over an animation — the same `child_visible`
        // flip `overlays/sidebar.rs` does, without the timing.
        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::None);
        revealer.set_reveal_child(true);
        revealer.set_child(Some(&region));
        drop(region);
        let window = gtk::Window::new();
        window.set_child(Some(&revealer));
        window.present();
        pump_for(200);

        let animators = live_animators();
        assert_eq!(animators.len(), 1, "one mount, one animation driver");
        let animator = &animators[0];
        assert!(
            revealer
                .child()
                .expect("the region is the revealer's child")
                .is_mapped(),
            "the fixture must really be mapped, or the 'hidden' half below is vacuous",
        );

        animator.reset_ticks();
        pump_for(300);
        let shown = animator.ticks();
        assert!(
            shown > 0,
            "a real frame clock must be delivering ticks to a shown mount, or this test \
             measures nothing at all (got {shown})",
        );

        // "Close the sidebar": the child is unmapped but stays **realized**, and
        // its toplevel is never hidden — the case GTK does not stop for.
        revealer.set_reveal_child(false);
        pump();
        assert!(
            !revealer
                .child()
                .expect("still the revealer's child")
                .is_mapped(),
            "the revealer must have unmapped its child",
        );
        animator.reset_ticks();
        pump_for(300);
        let hidden = animator.ticks();
        assert!(
            hidden <= 1,
            "a hidden mount must stop ticking after at most the one frame already in flight — \
             GTK will keep delivering to an unmapped-but-realized widget forever, so the break \
             has to be ours (got {hidden} against {shown} while shown)",
        );
        assert!(
            !animator.is_armed(),
            "…and the mount must record that it broke, or nothing will re-arm it",
        );

        // Reopen. Nothing re-renders, so only `connect_map` can revive it.
        revealer.set_reveal_child(true);
        pump();
        animator.reset_ticks();
        pump_for(300);
        let reshown = animator.ticks();
        assert!(
            reshown > 0,
            "showing the mount again must re-arm it — a sidebar opening is not a mapping \
             pass, so `Animator::arm_on_map` is the only thing that can (got {reshown})",
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
    /// — note it holds **no** reference to the `card` box the regions live in)
    /// **and** while `wire_open_subscription`'s parked task still holds strong
    /// `window`/`revealer`/`card` clones through an aborted-but-not-yet-dropped
    /// `JoinHandle`. Only the end of the drain loop drops that record. So the
    /// region's `destroy` fires two refcount steps *after* `window.destroy()`
    /// returns, not during it — this models both holders and asserts at the point
    /// production actually reaches, not the earliest point it could.
    ///
    /// **Deletion checks:** re-pinning the subscription with `container.clone()`
    /// turns it red, the same as
    /// [`a_destroyed_region_is_freed_and_stops_rendering`]. Moving the
    /// `drop(subscription)` before the assertion's other drops — the "handle
    /// outlives the drain" regression — turns it red too, which the bare-window
    /// version of this test could not see.
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
        let revealer = gtk::Revealer::new();
        revealer.set_child(Some(&clamp));
        drop(clamp);
        let window = gtk::Window::new();
        window.set_child(Some(&revealer));

        // The only other strong reference on this path, and the one that decides
        // *when* the cascade completes: `wire_open_subscription` parks a
        // `spawn_local`'d loop capturing `window`, `revealer` **and `card`**
        // clones (`sidebar.rs:486-497`), and `close_all` only **aborts** it
        // (`sidebar.rs:754`). Abort is not release — `JoinHandle::abort` is
        // `self.source.destroy()` while the handle keeps its own `Source` ref
        // (glib-0.22.5 `src/main_context_futures.rs:328`, `:294-298`), so the
        // future and its `card` clone die at `TaskSource::finalize` (`:84`),
        // i.e. when the handle drops. In production that handle is
        // `SidebarPanel::subscription` and it falls out of `close_all`'s drain
        // iteration, one step after `window.destroy()` returns. Without this
        // holder the test would assert freedom a refcount step earlier than
        // production ever reaches, and would stay green under a change that let
        // the handle outlive the drain (moved into a longer-lived map, or a
        // `SidebarPanel` field reorder putting the widget clones after it).
        let subscription = {
            let window = window.clone();
            let revealer = revealer.clone();
            let card = card.clone();
            glib::MainContext::default().spawn_local(async move {
                let _holds = (window, revealer, card);
                std::future::pending::<()>().await;
            })
        };
        drop(card);
        pump();

        // `close_all`'s order, and `SidebarPanel`'s field order after it
        // (`sidebar.rs:118-133`): abort the subscription, destroy the toplevel,
        // then let the drained record drop — `window`, `revealer`, …,
        // `subscription` last.
        subscription.abort();
        window.destroy();
        drop(window);
        pump();
        drop(revealer);
        pump();
        drop(subscription);
        pump();

        assert!(
            region_weak.upgrade().is_none(),
            "a hot-unplugged sidebar must free the plugin regions inside its \
             card: nothing on the sidebar's own teardown path holds them past \
             the drain (its two subscriptions and its settle timer are \
             aborted/cancelled explicitly, `sidebar.rs:754`/`:758`/`:768-770`, \
             and the records that hold the widget clones drop with the \
             iteration), so the only thing that can strand them is the region \
             pinning itself (#909)",
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

    /// The panel child's apply-loop must **exit** once its root is gone — the
    /// `WeakRef` leg of the `bind` contract (`crates/hytte-reactive/src/bind.rs:16-30`),
    /// which is otherwise untestable: in production the `connect_destroy` abort
    /// always wins the race, because GTK4 emits `destroy` from
    /// `gtk_widget_dispose` and there is no way to free a widget without it.
    /// Driving [`drive_panel_child`] with an already-dead `WeakRef` is the only
    /// path to that branch.
    ///
    /// [`build_region`]'s side of the same question needs no test here: it routes
    /// through `hytte::reactive::bind`, so its loop is the library's, checked by
    /// `nix/lint-bind-pins.py` at the call site.
    ///
    /// **Deletion checks**, all three measured (#920's, plus #921 review
    /// MEDIUM-1's — the sibling hold below is what makes the last two possible;
    /// before it, the only thing asserted after the break was
    /// `instance_count == 0`, which every spelling of the release satisfies):
    ///
    /// | mutation on the weak-break leg | red assertion | measured |
    /// | --- | --- | --- |
    /// | `break` → `continue` (the task parks forever) | `finished` | `false` |
    /// | the release deleted outright | the holders one | `left: 2, right: 1` |
    /// | `forget_previous_panel_scope` → a bare `preem_render::forget_scope` on the taken cell — the pre-#921 spelling, unconditional and never decrementing | the instance one | `left: 0, right: 1` |
    ///
    /// That third row is the point of the sibling: it was **fully green** here
    /// before (17 passed, exit 0), and the mutant it admitted is precisely probe
    /// P5 — a weak break blanking a sibling monitor's panel, plus a holder stuck
    /// at 1 that makes every later release of that scope answer "not the last
    /// holder" forever.
    #[gtk::test]
    fn the_panel_loop_exits_once_its_root_is_gone() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let scope = Scope::panel("weak-leg");

        // A panel already on screen, so the teardown leg has a scope to release.
        let canvas = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let mut reconciler = Reconciler::new(&canvas, |_: NodeId, _: UiEventKind| {});
        let outbound: Rc<RefCell<Option<mpsc::Sender<HostMsg>>>> = Rc::new(RefCell::new(None));
        let shown: Rc<RefCell<Option<Scope>>> = Rc::new(RefCell::new(None));
        render_active_panel(
            &mut reconciler,
            &outbound,
            &shown,
            Some(&render_of("weak-leg", &tx)),
        );
        assert_eq!(
            preem_render::instance_count(&scope),
            1,
            "the panel must have a live renderer instance for the leg to release",
        );

        // A **sibling monitor's** drawer child showing the same panel: a second
        // hold on the scope, taken through the same helper production takes it
        // through. Without it the weak leg's release is indistinguishable from
        // an unconditional `forget_scope`, which is the mutant this pins.
        let sibling: Rc<RefCell<Option<Scope>>> = Rc::new(RefCell::new(None));
        forget_previous_panel_scope(&sibling, Some(&scope));
        assert_eq!(
            super::panel_scope_holders(&scope),
            2,
            "two children must be holding the scope before the weak break",
        );

        // A root that is already gone: nothing parents it, so dropping the only
        // reference disposes it — the state the loop's upgrade has to detect.
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let root_weak = root.downgrade();
        drop(root);
        assert!(
            root_weak.upgrade().is_none(),
            "the root must really be gone"
        );

        let active = Mutable::new(Some(render_of("weak-leg", &tx)));
        let finished = Rc::new(std::cell::Cell::new(false));
        let finished_in_task = finished.clone();
        let handle = glib::MainContext::default().spawn_local(async move {
            drive_panel_child(
                active.signal_cloned(),
                root_weak,
                reconciler,
                outbound,
                shown,
                // Never reached: the weak leg breaks before the render, so this
                // loop arms nothing. An empty scope set would refuse to arm in
                // any case.
                Animator::new(Vec::new),
            )
            .await;
            finished_in_task.set(true);
        });
        pump();

        assert!(
            finished.get(),
            "the apply-loop must exit once its root is gone: a `continue` there \
             parks the task on its signal for the session, holding the \
             reconciler, its canvas and the outbound cell — exactly the residual \
             the `WeakRef` leg exists to trim",
        );
        assert_eq!(
            preem_render::instance_count(&scope),
            1,
            "…dropping its own hold on the way out, not the instances: this leg \
             is a monitor's drawer child going away like any other, so it must \
             not blank the sibling monitor still painting the same panel (#921)",
        );
        assert_eq!(
            super::panel_scope_holders(&scope),
            1,
            "…and the sibling's hold must be the one that is left, not a count \
             stuck at 2 that never releases",
        );

        // The sibling goes too: now it really is the last holder.
        forget_previous_panel_scope(&sibling, None);
        assert_eq!(
            preem_render::instance_count(&scope),
            0,
            "…and the panel scope is still released once nothing shows it, on \
             the one path the `connect_destroy` abort cannot see",
        );
        assert_eq!(super::panel_scope_holders(&scope), 0);
        handle.abort();
    }

    /// A **departed** plugin's panel release drops its refcount entry as well as
    /// its renderer instances, so the store and [`super::PANEL_SCOPE_HOLDERS`]
    /// cannot diverge (#921 review MEDIUM-2).
    ///
    /// The releaser's panel release is deliberately unconditional — gating it on
    /// the refcount would re-open #921 (a plugin exiting with a drawer child
    /// still holding its panel would keep its instances until that child
    /// blanked, which with no monitors left never happens). Unconditional means
    /// it writes *past* the refcount, so it has to maintain it: a count left at
    /// 1 over an empty store is a hold nothing will ever pair a release with,
    /// and it makes every later release of that scope answer "not the last
    /// holder" — a leak that outlives the plugin it came from.
    ///
    /// **Deletion check:** dropping the `PANEL_SCOPE_HOLDERS.remove` from
    /// [`super::forget_departed_panel_scope`] (leaving the bare
    /// `preem_render::forget_scope`) turns the holders assertion red with
    /// `left: 1, right: 0` — probe R2B's stale state, made observable. The run
    /// stops there; the new-session assertion below is what that stale count
    /// would go on to cost (a first child arriving as the scope's *second*
    /// holder, behind a ghost), asserted in the same test so no narrower repair
    /// can satisfy one and leave the other.
    #[gtk::test]
    fn a_departed_plugins_panel_release_drops_its_refcount_entry_too() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let scope = Scope::panel("panel-departed");

        // A monitor's drawer child showing the panel: one instance, one hold.
        let canvas = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let mut reconciler = Reconciler::new(&canvas, |_: NodeId, _: UiEventKind| {});
        let outbound: Rc<RefCell<Option<mpsc::Sender<HostMsg>>>> = Rc::new(RefCell::new(None));
        let shown: Rc<RefCell<Option<Scope>>> = Rc::new(RefCell::new(None));
        render_active_panel(
            &mut reconciler,
            &outbound,
            &shown,
            Some(&render_of("panel-departed", &tx)),
        );
        assert_eq!(preem_render::instance_count(&scope), 1);
        assert_eq!(super::panel_scope_holders(&scope), 1);

        // The plugin leaves every mailbox while that child still holds it.
        let panels = Mutable::new(vec![render_of("panel-departed", &tx)]);
        let releaser = spawn_releaser_over_panels(&panels);
        pump();
        panels.set(Vec::new());
        pump();

        assert_eq!(
            preem_render::instance_count(&scope),
            0,
            "a departed plugin's panel scope must be released whoever is \
             holding it — gating this on the refcount would re-open #921",
        );
        assert_eq!(
            super::panel_scope_holders(&scope),
            0,
            "…and its refcount entry must go with it: a count over an empty \
             store is a hold nothing will ever release",
        );

        // A later session of the same plugin id starts from one hold, not two.
        let next_session: Rc<RefCell<Option<Scope>>> = Rc::new(RefCell::new(None));
        forget_previous_panel_scope(&next_session, Some(&scope));
        assert_eq!(
            super::panel_scope_holders(&scope),
            1,
            "a new session's first drawer child must be the scope's only \
             holder, not the second one behind a ghost",
        );
        forget_previous_panel_scope(&next_session, None);
        assert_eq!(super::panel_scope_holders(&scope), 0);

        releaser.abort();
    }

    /// Spawn the monitor-independent scope releaser (#921) over seven mailboxes
    /// of which only the one at `slot` carries anything — the shape
    /// `plugins::install` wires up, minus the registry a `#[gtk::test]` has no
    /// booted `App` to provide.
    ///
    /// `slot` indexes `live_plugin_ids_signal`'s array in `PluginHandles` field
    /// order: 0-2 the sidebar regions, 3-5 the bar regions, 6 the shared panel
    /// list.
    ///
    /// Returns the task handle so the test can abort it: `#[gtk::test]` funnels
    /// every test in this binary onto one main context, and a parked
    /// subscription would otherwise keep polling through later tests' `pump()`s.
    fn spawn_releaser_over(
        slot: usize,
        mailbox: &Mutable<Vec<SlotRender>>,
    ) -> glib::JoinHandle<()> {
        let mut mailboxes: [Mutable<Vec<SlotRender>>; 7] =
            std::array::from_fn(|_| Mutable::new(Vec::new()));
        mailboxes[slot] = mailbox.clone();
        glib::MainContext::default()
            .spawn_local(drive_scope_releaser(live_plugin_ids_signal(mailboxes)))
    }

    /// [`spawn_releaser_over`] the `bar_left` region — where the chip mounts
    /// these tests build would land.
    fn spawn_releaser_over_bar_left(renders: &Mutable<Vec<SlotRender>>) -> glib::JoinHandle<()> {
        spawn_releaser_over(3, renders)
    }

    /// [`spawn_releaser_over`] the shared `panels` mailbox — the one a drawer
    /// child reads.
    fn spawn_releaser_over_panels(panels: &Mutable<Vec<SlotRender>>) -> glib::JoinHandle<()> {
        spawn_releaser_over(6, panels)
    }

    /// A plugin leaving while **no region is alive** must still release its card
    /// scope (#921).
    ///
    /// This is the #920 review's probe P2 with the releaser wired in. Every
    /// output unplugged is a `monitors_changed` carrying an empty list
    /// (`main.rs:236-252`): every bar is destroyed and none rebuilt, so when the
    /// plugin exits a moment later there is not one region left to run
    /// [`reconcile_region`]'s retain loop — the only thing that released a
    /// `Scope::card` before. Measured on `main`: `instance_count == 1` after the
    /// leave, and a region rebuilt afterwards never had that card so it never
    /// reclaims it either. The plugin's kit buffers stay resident for the
    /// session and, for an animating widget, its `Renderer::animates()` stays
    /// true — which is exactly the predicate every mount's frame clock parks on
    /// since #897.
    ///
    /// Before #920 this corner was masked: the region pinned itself, so a
    /// stranded-but-still-subscribed region observed the leave and released the
    /// shared scope by accident. Unpinning the region is what exposed it, which
    /// is why the fix is a subscriber that needs no region at all.
    ///
    /// **Deletion check:** not spawning the releaser (or dropping the
    /// `forget_scope` loop from [`drive_scope_releaser`]) turns the final
    /// assertion red with `left: 1, right: 0` — probe P2's measured number.
    #[gtk::test]
    fn a_plugin_leaving_with_no_live_region_still_releases_its_card_scope() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let renders = Mutable::new(vec![render_of("region-outputless", &tx)]);
        let scope = Scope::card("region-outputless");
        let releaser = spawn_releaser_over_bar_left(&renders);

        let window = mount_region(&renders);
        pump();
        assert_eq!(
            preem_render::instance_count(&scope),
            1,
            "the mounted chip must have a live renderer instance before teardown",
        );

        // Every output unplugged: the bar — and with it the only region
        // subscribed to this mailbox — is destroyed, and none is rebuilt.
        window.destroy();
        drop(window);
        pump();

        // …and *then* the plugin exits. Nothing monitor-shaped is left to
        // notice.
        renders.set(Vec::new());
        pump();

        assert_eq!(
            preem_render::instance_count(&scope),
            0,
            "a plugin leaving with no live region must still release its \
             renderer instances: scope release was monitor-shaped and scope \
             lifetime is not, so with every output unplugged the instances (and \
             an animating widget's `any_animating()`) stayed resident for the \
             session (#921)",
        );
        releaser.abort();
    }

    /// The releaser and a **live** region's retain loop both forget the same
    /// `Scope::card` when a plugin leaves, and that double release must be a
    /// no-op rather than a corruption (#921).
    ///
    /// The two release paths deliberately overlap: the retain loop is per
    /// region (and re-runs on every monitor's copy), the releaser is one per
    /// process, and nothing sequences them. `preem_render::forget_scope` is a
    /// `HashMap::remove`, so the second is a miss — asserted here rather than
    /// assumed, together with the consequence that matters on glass: the plugin
    /// reconnecting must get a fresh instance, not a poisoned table.
    #[gtk::test]
    fn the_releaser_and_a_live_regions_retain_loop_may_both_release_one_scope() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let renders = Mutable::new(vec![render_of("region-doubled", &tx)]);
        let scope = Scope::card("region-doubled");
        let releaser = spawn_releaser_over_bar_left(&renders);

        let window = mount_region(&renders);
        pump();
        assert_eq!(preem_render::instance_count(&scope), 1);

        // The plugin leaves with the region still mounted: `reconcile_region`'s
        // retain loop forgets the scope, and so does the releaser.
        renders.set(Vec::new());
        pump();
        assert_eq!(
            preem_render::instance_count(&scope),
            0,
            "both release paths running must leave the scope released, not \
             half-released",
        );

        // A fast reconnect: the same id comes back and must map fresh
        // instances.
        renders.set(vec![render_of("region-doubled", &tx)]);
        pump();
        assert_eq!(
            preem_render::instance_count(&scope),
            1,
            "a plugin reconnecting after a double release must map fresh \
             renderer instances",
        );

        // And the raw call, twice in a row, on a scope that is already gone.
        preem_render::forget_scope(&scope);
        assert_eq!(preem_render::instance_count(&scope), 0);
        preem_render::forget_scope(&scope);
        assert_eq!(
            preem_render::instance_count(&scope),
            0,
            "`forget_scope` must be idempotent — the overlapping release paths \
             above depend on it",
        );

        releaser.abort();
        window.destroy();
        drop(window);
        pump();
    }

    /// Destroying **one** monitor's drawer child must not drop the panel
    /// renderer instances a sibling monitor is still painting (#921) — the
    /// panel-side mirror of
    /// [`a_destroyed_regions_card_scope_is_released_by_a_surviving_region`].
    ///
    /// `Scope::panel(plugin_id)` is cross-monitor for the same reason
    /// `Scope::card` is: every monitor's drawer child mirrors the one globally
    /// active panel, so they share one set of instances. The release, though,
    /// was per child — whichever child's `connect_destroy` fired first called
    /// `preem_render::forget_scope` unconditionally. The #920 review measured it
    /// as probe P5: two children showing, `instance_count` 1; one destroyed,
    /// `instance_count` 0, with the survivor still on screen painting a panel
    /// whose instances had been dropped.
    ///
    /// Unreachable through today's `modal::close_all` / sidebar `close_all`
    /// (both tear every monitor down in one `monitors_changed` dispatch), which
    /// is why it shipped — but that is a property of the callers, and this pins
    /// the property of the release itself, so a future per-monitor drawer
    /// rebuild goes red here instead of silently blanking a sibling.
    ///
    /// **Deletion check:** removing the [`release_panel_scope`] refcount from
    /// [`forget_previous_panel_scope`] (forgetting unconditionally, as before)
    /// turns the middle assertion red with `left: 0, right: 1` — probe P5's
    /// measured number. The final assertion is what stops the refcount from
    /// being "never release at all": deleting the release entirely turns it red.
    #[gtk::test]
    fn one_monitors_drawer_child_going_away_leaves_a_siblings_panel_painting() {
        adw::init().expect("libadwaita init");
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let panels = Mutable::new(vec![render_of("panel-two-monitors", &tx)]);
        let active = Mutable::new(Some("panel-two-monitors".to_owned()));
        let scope = Scope::panel("panel-two-monitors");

        // Two monitors' drawer children, mirroring the one global selection.
        let hot_unplugged = mount_panel_child(&panels, &active);
        let survivor = mount_panel_child(&panels, &active);
        pump();
        assert_eq!(
            preem_render::instance_count(&scope),
            1,
            "both monitors' drawer children share one set of renderer instances",
        );

        hot_unplugged.destroy();
        drop(hot_unplugged);
        pump();
        assert_eq!(
            preem_render::instance_count(&scope),
            1,
            "one monitor's drawer child going away must not drop the renderer \
             instances the surviving monitor's child is still painting",
        );

        // …and the survivor really is still driving them: a fresh frame of the
        // same panel keeps the instance it is mid-animation on rather than
        // rebuilding it.
        panels.set(vec![render_of("panel-two-monitors", &tx)]);
        pump();
        assert!(
            preem_render::probe(&scope, Some("panel")).is_some(),
            "the surviving child must still be rendering the panel's preem node",
        );

        // The last holder going away is what releases it.
        survivor.destroy();
        drop(survivor);
        pump();
        assert_eq!(
            preem_render::instance_count(&scope),
            0,
            "the last drawer child showing a panel must still release its \
             renderer instances — refcounting the scope must not turn the \
             release off",
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
