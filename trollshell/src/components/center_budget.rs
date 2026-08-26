//! The bar's **centre-slot width budget** — "this is the space you can have,
//! max" (#838).
//!
//! ## Why this exists
//!
//! The mpris chip used to decide full-vs-narrow by reaching *up* its own parent
//! chain from inside its own layout reaction: walk to the `GtkCenterBox`, measure
//! the left window-title cluster and the right status cluster, subtract, compare
//! against its own natural width. That is a feedback topology — a widget
//! observing the layout it is itself part of — and it was guarded twice and bit
//! twice:
//!
//! - **Stuck.** The watchers hooked `notify::width` on plain widgets — a
//!   property GTK4's `GtkWidget` does not have, so none of them ever fired at
//!   all (#838; see the surface hook in [`install`] for the full autopsy). While
//!   a player was stopped nothing else re-ran the decision, so the chip froze in
//!   whatever mode it last picked.
//! - **Blinking.** #842 fixed the freeze by adding `niri::windows()` /
//!   `niri::workspaces()` as triggers. But `windows()` re-emits on every **window
//!   title change** — terminal and browser titles tick constantly — and the
//!   window list's *natural* width tracks those titles. Each tick moved the
//!   measurement by a few px; near the fit boundary the swing crossed the widget's
//!   24 px hysteresis band and the chip alternated expand → squeeze → collapse →
//!   release → expand.
//!
//! Annika's call (#838, "can't we just give like the widgets a hint typ 'this is
//! the space you can have max' and then render appropriately???") is this module.
//! Budget flows **one way, downhill**:
//!
//! ```text
//!   bar geometry ──► center_budget ──► Mutable<Option<i32>> ──► widget renders
//!        (inputs)      (damping)            (signal)            (self-measure)
//! ```
//!
//! Nothing downstream of the arrow can move anything upstream of it, so the loop
//! that produced both failures does not exist to be guarded. The consumer never
//! touches its parent chain or measures a neighbour again.
//!
//! ## What is measured
//!
//! ```text
//!   budget = center_box.width() − left_natural − right_natural − GAP   (clamped ≥ 0)
//! ```
//!
//! - `center_box.width()` is the bar's content allocation. It fills the bar, so
//!   it is independent of what any bar widget chooses to render. The sidebar
//!   needs no term of its own: it is a `Layer::Top` surface whose left exclusive
//!   zone shrinks this exclusive bar on open, so the allocation *already* dropped
//!   by the sidebar width (#324) — subtracting again would double-count. It has
//!   only dropped once the compositor's configure has landed, though, which is an
//!   async round-trip after the toggle: *when* to take this reading is the whole
//!   subject of [`install`]'s surface hook.
//! - The clusters are measured by **natural** width, never allocated width.
//!   `CenterBox` squeezes the start child toward its minimum when the end pair is
//!   wide, so the left cluster's *allocation* depends on what the centre slot
//!   renders; its natural does not. (The window-button labels are width-capped by
//!   `window_list`'s `max_width_chars`, so the left natural stays bounded.)
//!
//! `None` means the geometry is not realised yet (no allocation). It is published
//! only as the initial state — see [`damp`] for why an unrealised *reading* never
//! overwrites a real budget.
//!
//! ## Damping at the source — the anti-oscillation mechanism
//!
//! This is the load-bearing part, and it replaces the per-consumer hysteresis the
//! widget used to carry. A freshly measured budget is published **only if it
//! differs from the last published budget by more than [`JITTER_PX`]**. Title
//! noise moves the left cluster's natural width by a handful of px at a time;
//! those measurements are taken, compared, and dropped here. They never reach a
//! consumer, so no consumer needs a guard against them.
//!
//! Two properties worth stating, because they are what make one threshold enough:
//!
//! - The anchor is the last **published** value, not the last measured one. A
//!   slow genuine drift (a window list growing one button at a time) accumulates
//!   against a fixed anchor and does eventually publish, so damping suppresses
//!   noise without suppressing signal. Anchoring on the last *measured* value
//!   would let unlimited drift through in [`JITTER_PX`]-sized steps.
//! - The published budget is therefore accurate to within [`JITTER_PX`] px of the
//!   true one. That is a *static* error — a consumer may sit up to that far from
//!   the true fit boundary — not an oscillation. Trading a bounded static error
//!   for the absence of a feedback loop is the whole design.
//!
//! ## Scope
//!
//! One consumer today (`widgets::mpris`), so this stays in the binary. If a
//! second bar widget wants a budget it graduates into `hytte_ui::Bar` as API.
//!
//! Known residual: the budget is the whole centre *slot*'s, and the slot also
//! holds `plugins::bar_center_slot()`. A centre-mounted plugin chip eats into the
//! budget without being subtracted from it, so the single consumer can overrun by
//! that chip's width. The pre-#838 code had the identical blind spot; fixing it
//! means splitting a shared budget between consumers, which is a different design
//! and not one anything asks for yet.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::niri;

use crate::components::monitor_key::{is_fallback_key, monitor_key};

/// Horizontal breathing room (px) kept between the centre slot and the left
/// window-title cluster, so the two never butt right up against each other.
const GAP: i32 = 12;

/// The single anti-oscillation tunable (px): two widths this close together are
/// not meaningfully different.
///
/// Absorbs the 24 px `HYSTERESIS` the mpris widget used to carry. Here it is the
/// republish threshold — a measurement within this band of the last published
/// budget is dropped, so neighbour-side jitter dies at the source rather than in
/// every consumer. Consumers reuse the same number for their own expand headroom
/// (see `widgets::mpris::EXPAND_HEADROOM`), which is the same claim applied to
/// their own natural width.
///
/// Raising it damps harder at the cost of a larger static error against the true
/// fit boundary; lowering it tracks the true budget more closely at the cost of
/// letting more title noise through.
pub(crate) const JITTER_PX: i32 = 24;

thread_local! {
    /// Per-connector centre-slot budget. Entries are created lazily by
    /// [`signal`] so a bar widget can subscribe during its own construction —
    /// which happens *before* [`install`] runs, since the bar tree only exists
    /// once every widget in it has been built.
    static BUDGETS: RefCell<HashMap<String, Mutable<Option<i32>>>> =
        RefCell::new(HashMap::new());
}

fn budget_state(key: &str) -> Mutable<Option<i32>> {
    BUDGETS.with(|map| {
        map.borrow_mut()
            .entry(key.to_string())
            .or_insert_with(|| Mutable::new(None))
            .clone()
    })
}

/// The centre slot's max width budget on `monitor`, in px. `None` until the bar
/// geometry is realised.
///
/// Safe to subscribe to before [`install`] has run for this monitor (that is the
/// normal order: widgets are built, then the bar, then `install`). The first real
/// measurement arrives on the bar's first allocation.
pub(crate) fn signal(monitor: &Monitor) -> impl Signal<Item = Option<i32>> + 'static {
    budget_state(&monitor_key(monitor)).signal()
}

/// Observe `bar`'s geometry and publish the centre-slot budget for `monitor`.
///
/// Call once per bar, immediately after `Bar::show()`. Every geometry input the
/// mpris widget used to read for itself is hooked here instead — and since #838
/// every hook is one that can actually fire:
///
/// - the bar window's **`gdk::Surface` `width`/`height`**: the compositor
///   configure itself, so it fires exactly when a resize *lands*, sidebar
///   exclusive-zone push included. This is the settle path, and the only watcher
///   of bar geometry there is; the long comment at the hook is the autopsy of the
///   three widget-side watchers it replaces, none of which ever fired.
/// - the sidebar's open signal (#324): the earliest *notice* of a toggle, one
///   compositor round-trip before the bar has actually shrunk.
/// - `niri::windows()` / `niri::workspaces()` (#842): the two sources
///   `widgets::window_list` rebuilds from, so a change here is exactly a change
///   to the left cluster's *natural* width — which no geometry watcher can see,
///   because the bar itself does not resize when a window title changes.
///
/// The noisy input (`windows()`) is still hooked at full fidelity. What changed
/// in #847 is where the noise stops: [`damp`], once, here.
///
/// All emissions coalesce into a single idle-scheduled measurement, so a busy
/// window-open burst costs one publish, and no measurement is ever taken from
/// inside a layout pass.
///
/// `pub` rather than the module family's usual `pub(crate)` for the same reason
/// `components::mpris_controls` is: `main.rs` is the only caller, and `main.rs`
/// is not part of the shadow `lib.rs` target (#674), so a `pub(crate)` here
/// reads as dead code in that target's compilation.
pub fn install(monitor: &Monitor, bar: &BarHandle) {
    let Some(center_box) = bar.window().child().and_downcast::<gtk::CenterBox>() else {
        // `Bar::show()` always sets a `CenterBox` child; if that ever changes,
        // publish nothing rather than guess a budget (consumers hold their
        // current presentation on `None`).
        tracing::warn!("bar window has no CenterBox child; centre budget not installed");
        return;
    };

    let schedule = {
        let state = budget_state(&monitor_key(monitor));
        let last = Rc::new(Cell::new(None::<i32>));
        let pending = Rc::new(Cell::new(false));
        move |center_box: &gtk::CenterBox| {
            if pending.replace(true) {
                return;
            }
            let center_box = center_box.clone();
            let state = state.clone();
            let last = last.clone();
            let pending = pending.clone();
            glib::idle_add_local_once(move || {
                pending.set(false);
                publish(&state, &last, measure(&center_box));
            });
        }
    };

    // ── The compositor configure — the bar-geometry watcher (#838) ──────────
    //
    // Watch the bar window's `gdk::Surface`, not any widget. GTK4's `GtkWidget`
    // has **no `width` property** — only `width-request` — so
    // `connect_notify_local(Some("width"), …)` on a plain widget is a connection
    // to a detail of a property that does not exist: silently legal, and
    // permanently dead. Nothing emits it, ever. Verified against the gtk4-rs
    // 0.11.2 bindings: `gtk::Widget` has no `connect_width_notify`, while
    // `gdk::Surface` does (`gdk4-0.11.2/src/auto/surface.rs:563`, and
    // `connect_height_notify` at `:458`).
    //
    // Two such watchers used to sit here (`CenterBox` and the left cluster), and
    // the pre-#847 mpris widget carried three before that, so the "settle after
    // an async resize" path #324 describes has never once run — not in this
    // module, and not in the widget it grew out of. That is the bug Annika saw
    // as "calculation seems still slightly off when sidebar open": the sidebar's
    // open signal (below) recomputes at once, but the bar's shrink is a
    // compositor round-trip that has not landed yet, so it reads the *pre-shrink*
    // width; the correcting re-measure was supposed to come from the `CenterBox`
    // width watcher, and never came. The stale budget then stood until unrelated
    // niri traffic happened to remeasure. All three of this module's bar-geometry
    // watchers are deleted rather than fixed:
    //
    // - `CenterBox` `width` and the left cluster's `width` — dead, as above.
    // - the window's `default-width` — a *real* `GtkWindow` property, but it is
    //   the app's requested default size. Nothing in this shell ever sets it and
    //   a layer-shell configure never writes back to it, so it was dead in
    //   practice too. The surface hook covers the case it was there for (monitor
    //   resize) properly: a mode change reconfigures the layer surface.
    //
    // **Do not reintroduce a `notify::width` on a widget.** It compiles, it
    // connects, and it can never fire.
    //
    // Lifetimes: the window owns its surface, so a handler on the surface must
    // not hold the widget tree strongly — window → surface → handler → window
    // would outlive the bar. Walking back up (the shape the deleted handlers
    // used) is impossible here because a `GdkSurface` has no widget pointer, so
    // the handler carries a `WeakRef` to the `CenterBox` instead — the same #224
    // discipline as the `bind` call sites below.
    //
    // Hooked once immediately (`Bar::show()` has already called `present()`, so
    // the window is realised and `connect_realize` will not fire again) and on
    // every subsequent realise. A hide/show cycle destroys the surface — taking
    // its handlers with it — and realises a *new* one, so re-hooking is both
    // required and incapable of double-hooking the same surface.
    {
        let schedule = schedule.clone();
        let weak_center_box = center_box.downgrade();
        let hook_surface = move |win: &gtk::Window| {
            let Some(surface) = win.surface() else {
                return;
            };
            let schedule = schedule.clone();
            let weak_center_box = weak_center_box.clone();
            let remeasure = move |_: &gdk::Surface| {
                if let Some(center_box) = weak_center_box.upgrade() {
                    schedule(&center_box);
                }
            };
            surface.connect_width_notify(remeasure.clone());
            surface.connect_height_notify(remeasure);
        };
        hook_surface(bar.window());
        bar.window().connect_realize(hook_surface);
    }

    // Sidebar toggle (#324). This is the *request*, not the result: it fires a
    // round-trip before the bar has shrunk, so on its own it re-measures the
    // pre-shrink geometry. Harmless — that reading equals the standing budget,
    // so [`damp`] drops it — and the surface hook above publishes the real
    // change when the configure lands. Never *subtract* the sidebar width here;
    // the exclusive zone is already in the bar allocation (see the module doc).
    {
        let schedule = schedule.clone();
        bind(
            crate::overlays::sidebar::open_signal(monitor),
            &center_box,
            move |center_box, _open| schedule(center_box),
        );
    }

    // Window set / workspace focus (#842) — the left cluster's natural width,
    // which no geometry watcher sees: the bar does not resize when a window
    // opens or a title changes, only the cluster's own natural does.
    {
        let schedule = schedule.clone();
        bind(niri::windows(), &center_box, move |center_box, _windows| {
            schedule(center_box);
        });
    }
    {
        let schedule = schedule.clone();
        bind(
            niri::workspaces(),
            &center_box,
            move |center_box, _spaces| schedule(center_box),
        );
    }

    // Settle the first value. The signal hooks above go on immediately — we run
    // after `Bar::show()`, so the whole tree exists — and the only one that has
    // to be re-armed on realise is the surface hook, because the surface is what
    // realise creates. Realise is still a good moment to *measure*, and both
    // paths are kept because `present()` has usually realised the window
    // already, in which case `connect_realize` never fires again.
    center_box.connect_realize(schedule.clone());
    schedule(&center_box);
}

/// Drop budget entries that can never be looked up again.
///
/// Connector-named monitors keep their entry across a bar rebuild — the key is
/// stable, and the subscriber wired up before the rebuild is still reading it.
/// A connector-less monitor's fallback key is the old `GdkMonitor` pointer, and
/// the next rebuild mints a different one, so leaving it in place is a pure leak
/// of one `Mutable` per hot-plug cycle (see `components::monitor_key`).
///
/// `pub` for the same `main.rs`-is-the-only-caller reason as [`install`].
pub fn prune_stale() {
    BUDGETS.with(|map| map.borrow_mut().retain(|key, _| !is_fallback_key(key)));
}

/// Measure the centre slot's budget from the live bar geometry, or `None` if the
/// bar has no allocation yet.
///
/// The `CenterBox`'s end widget is the `end_pair` box `hytte_ui::Bar` builds as
/// `[centre group, right group]`, so its last child is the right cluster.
fn measure(center_box: &gtk::CenterBox) -> Option<i32> {
    let bar_width = center_box.width();
    if bar_width <= 0 {
        return None;
    }
    let left = center_box.start_widget().map_or(0, |w| natural_width(&w));
    let right = center_box
        .end_widget()
        .and_then(|end_pair| end_pair.last_child())
        .map_or(0, |w| natural_width(&w));
    Some((bar_width - left - right - GAP).max(0))
}

/// A widget's natural width — independent of what the centre slot renders,
/// unlike its allocated width (which `CenterBox` perturbs; see the module doc).
fn natural_width(w: &gtk::Widget) -> i32 {
    w.measure(gtk::Orientation::Horizontal, -1).1
}

/// Apply [`damp`] to `raw` and, if it survives, publish it and become the new
/// anchor. Split from the GTK reads so the whole damping path is testable.
fn publish(state: &Mutable<Option<i32>>, last: &Cell<Option<i32>>, raw: Option<i32>) {
    if let Some(value) = damp(last.get(), raw) {
        last.set(Some(value));
        state.set(Some(value));
    }
}

/// The pure damping rule: given the last published budget and a fresh raw
/// measurement, return `Some(v)` to publish `v` or `None` to suppress.
///
/// - An unrealised reading (`raw == None`) is **never** published over a real
///   budget. Zero-allocation readings happen transiently — mid-resize, during a
///   bar teardown — and a consumer that receives `None` freezes its presentation
///   on a value it cannot act on. Holding the last real budget is strictly
///   better, and the next allocation republishes. (The initial `None` a consumer
///   sees before the first measurement comes from the `Mutable`'s starting value,
///   not from here.)
/// - The first real budget always publishes: a consumer is sitting on a default
///   presentation and there is no anchor yet to compare against.
/// - Afterwards, publish only if the measurement has moved more than
///   [`JITTER_PX`] from the **last published** value. This is where title jitter
///   dies; see the module doc for why the published anchor is the right one.
fn damp(last_published: Option<i32>, raw: Option<i32>) -> Option<i32> {
    let raw = raw?;
    match last_published {
        None => Some(raw),
        Some(anchor) if raw.abs_diff(anchor) > JITTER_PX.unsigned_abs() => Some(raw),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Cell, JITTER_PX, Mutable, damp, publish};

    /// Feed a sequence of raw measurements through the real publish path and
    /// collect what actually reached the `Mutable`. This is the damping rule as
    /// a consumer experiences it.
    fn published(raws: &[Option<i32>]) -> Vec<i32> {
        let state = Mutable::new(None::<i32>);
        let last = Cell::new(None::<i32>);
        let mut seen = Vec::new();
        for raw in raws {
            publish(&state, &last, *raw);
            if let Some(v) = state.get()
                && seen.last() != Some(&v)
            {
                seen.push(v);
            }
        }
        seen
    }

    #[test]
    fn the_first_real_budget_always_publishes() {
        assert_eq!(damp(None, Some(800)), Some(800));
        assert_eq!(published(&[Some(800)]), vec![800]);
    }

    #[test]
    fn sub_threshold_jitter_is_suppressed() {
        // The title-noise case: a handful of px each way around a settled
        // budget. None of it may reach a consumer.
        assert_eq!(damp(Some(800), Some(800)), None, "no movement at all");
        assert_eq!(damp(Some(800), Some(800 + JITTER_PX)), None, "at the band");
        assert_eq!(damp(Some(800), Some(800 - JITTER_PX)), None, "and below it");
        assert_eq!(
            published(&[Some(800), Some(806), Some(797), Some(812), Some(800)]),
            vec![800],
            "one publish, then silence"
        );
    }

    #[test]
    fn super_threshold_movement_passes() {
        // A window actually opening or the sidebar actually pushing the bar.
        assert_eq!(damp(Some(800), Some(800 + JITTER_PX + 1)), Some(825));
        assert_eq!(damp(Some(800), Some(800 - JITTER_PX - 1)), Some(775));
        assert_eq!(
            published(&[Some(800), Some(400), Some(900)]),
            vec![800, 400, 900]
        );
    }

    #[test]
    fn direction_changes_are_symmetric() {
        // Damping is a band around the anchor, not a ratchet: the same magnitude
        // passes (or is dropped) whichever way the budget moves.
        let up = published(&[Some(500), Some(600), Some(500)]);
        assert_eq!(up, vec![500, 600, 500], "a big move back down republishes");
        let jitter = published(&[Some(500), Some(510), Some(490), Some(510)]);
        assert_eq!(jitter, vec![500], "small moves both ways stay suppressed");
    }

    #[test]
    fn slow_drift_accumulates_against_the_published_anchor() {
        // Anchoring on the last *published* value (not the last measured one) is
        // what keeps damping from swallowing a genuine slow change: the steps are
        // each below the band, but they add up against a fixed anchor.
        let raws: Vec<Option<i32>> = (0..=40).map(|i| Some(500 + i * 5)).collect();
        let seen = published(&raws);
        assert!(
            seen.len() > 1,
            "a 200 px drift in 5 px steps must reach the consumer, got {seen:?}"
        );
        assert_eq!(
            *seen.last().unwrap(),
            700,
            "and must end at the true budget"
        );
        // Had the anchor been the last *measured* value, nothing would ever have
        // exceeded the band and `seen` would be a single element.
    }

    #[test]
    fn an_unrealised_reading_never_overwrites_a_real_budget() {
        assert_eq!(damp(Some(800), None), None, "hold the last real budget");
        assert_eq!(damp(None, None), None, "and publish nothing before one");
        assert_eq!(
            published(&[Some(800), None, None, Some(805)]),
            vec![800],
            "a transient zero allocation is not a budget change"
        );
    }
}

/// The one fact about GTK that [`install`]'s watcher choice rests on, asserted
/// rather than believed (#838).
///
/// Gated behind `system-tests` because instantiating a widget needs a display.
/// The `gdk::Surface` half of the claim needs no test: [`install`] calls the
/// typed `connect_width_notify`/`connect_height_notify` bindings, so the
/// compiler checks it on every build. It is the widget half — where the dead
/// watchers lived, and where the stringly-typed API accepts anything — that has
/// nothing checking it.
#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use hytte::gtk::{self, prelude::*};

    /// A `GtkWidget` has no `width` property, so `notify::width` on one can
    /// never fire — the whole reason two watchers in this module (and three more
    /// in the pre-#847 mpris widget) were dead from the day they were written.
    /// `connect_notify_local` takes the detail as a string and does not validate
    /// it, so nothing else in the toolchain will ever say so.
    #[gtk::test]
    fn a_widget_has_no_width_property_to_watch() {
        let center_box = gtk::CenterBox::new();
        assert!(
            center_box.find_property("width").is_none(),
            "GtkWidget grew a `width` property: `connect_notify_local(Some(\"width\"), …)` \
             is no longer dead, and this module's surface hook deserves a re-think"
        );
        assert!(
            center_box.find_property("width-request").is_some(),
            "`width-request` is the property that does exist — the request, not the \
             result, and the one `width` gets mistaken for"
        );
    }
}
