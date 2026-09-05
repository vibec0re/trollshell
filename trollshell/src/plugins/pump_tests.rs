//! End-to-end coverage for [`request_preem_repaint`] (#906 item 1).
//!
//! `super::tests` (`plugins::tests`) already covers [`request_remap_holding`]
//! in isolation (`a_repaint_request_skips_mailboxes_holding_no_mover`), which
//! proves the predicate but not that `request_preem_repaint` actually reaches
//! it the way #896's R5 fix intends: only a mailbox whose scope holds a mover
//! gets nudged. Nothing drove the pump tick through a real region with more
//! than one scope and checked which mailbox came out woken.
//!
//! This module does that: two plugins are mapped into two different bar
//! mailboxes exactly as [`super::super::region::reconcile_region`] would (a
//! `to_ui_node` mapping pass per plugin, same as a real chip mount), one
//! carrying an always-scrolling marquee and the other a plain label that can
//! never animate. Advancing the real animation clock (`preem_render::advance_all`)
//! and feeding its output straight into `request_preem_repaint` — the actual
//! function `install_preem_clock` calls every tick — must wake only the
//! mailbox holding the mover.
//!
//! Everything here is GTK-free (`to_ui_node` only builds `hytte_ui::Node`
//! data, never a widget), so this runs as a plain hermetic `#[test]`, not a
//! `#[gtk::test]`.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use hytte::futures_signals::signal::Signal;
use hytte::reactive::Service;
use hytte_plugin_proto::{HostMsg, preem as vocab, wire};
use tokio::sync::mpsc;

use crate::plugins::datasource::DatasourceRouter;
use crate::plugins::wire_map::to_ui_node;

use super::*;

/// A minimal stand-in for `plugins::PluginsService` that installs an
/// already-built [`PluginHandles`] into the thread-local registry without
/// booting the real plugin host (socket listener, tokio tasks, the
/// process-global `PLUGIN_RUNTIME`) — `request_preem_repaint` only needs the
/// handle bag a real `Service::start` would have produced, not the rest of
/// what installing the plugin host does.
struct FixtureService(PluginHandles);

impl Service for FixtureService {
    type Handles = PluginHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        self.0
    }
}

/// A `Node::Preem` marquee long enough that it scrolls and fast enough that it
/// never settles: `Renderer::advance`/`animates` for `Marquee` report movement
/// for as long as `speed_dots_per_sec != 0.0` and the message doesn't fit the
/// window — unlike the kit's other primitives (gauge, flip board, phosphor
/// scope), which all eventually settle. Same config shape
/// `marquee_scroll_direction_follows_the_speeds_sign` (`plugins::tests`)
/// already proved scrolls against the kit.
fn marquee_node(id: &str) -> wire::Node {
    wire::Node::Preem {
        id: Some(id.to_owned()),
        classes: vec![],
        widget: Box::new(vocab::PreemWidget::Marquee {
            config: vocab::MarqueeConfig {
                style: vocab::StyleRef::new(vocab::StyleName::Vfd),
                window_px: 192,
                gap_dots: 6,
                speed_dots_per_sec: 20.0,
            },
            state: vocab::MarqueeState {
                text: "AN ANIMATING SCROLLING MARQUEE TICKER".into(),
            },
        }),
    }
}

/// A tree with no preem node in it at all — genuinely static: nothing in it
/// can ever appear in [`preem_render::advance_all`]'s moved list, regardless
/// of what the fan-out logic under test does with it.
fn static_node(id: &str) -> wire::Node {
    wire::Node::Label {
        id: Some(id.to_owned()),
        text: "static".into(),
        classes: vec![],
    }
}

fn slot(plugin_id: &str, tree: wire::Node, tx: &mpsc::Sender<HostMsg>) -> SlotRender {
    SlotRender {
        plugin_id: plugin_id.to_owned(),
        order: 0,
        generation: 1,
        tree,
        panel: None,
        outbound: tx.clone(),
    }
}

/// A `PluginHandles` fixture with `bar_left`/`bar_center` pre-seeded; every
/// other mailbox and channel starts at the same default `PluginsService::start`
/// gives it in production.
fn fixture_handles(
    bar_left: Mutable<Vec<SlotRender>>,
    bar_center: Mutable<Vec<SlotRender>>,
) -> PluginHandles {
    PluginHandles {
        sidebar_lead: Mutable::new(Vec::new()),
        sidebar_top: Mutable::new(Vec::new()),
        sidebar_bottom: Mutable::new(Vec::new()),
        bar_left,
        bar_center,
        bar_right: Mutable::new(Vec::new()),
        panels: Mutable::new(Vec::new()),
        active_panel_id: Mutable::new(None),
        clock_tx: tokio::sync::watch::channel(None).0,
        visibility_tx: tokio::sync::watch::channel(false).0,
        accent_tx: tokio::sync::watch::channel(None).0,
        spectrum_tx: tokio::sync::watch::channel(None).0,
        calendar_tx: tokio::sync::watch::channel(Vec::new()).0,
        now_playing_tx: tokio::sync::watch::channel(NowPlaying::default()).0,
        locked_tx: tokio::sync::watch::channel(false).0,
        effects_rx: RefCell::new(None),
        datasource: DatasourceRouter::default(),
    }
}

/// The reviewer's own probe shape: an animating panel/plugin must not wake a
/// static `bar_left` subscriber.
///
/// Two plugins, two different bar mailboxes — `resident` (a plain label, never
/// animates) mounted in `bar_left`, `mover` (a scrolling marquee) mounted in
/// `bar_center` — mapped through [`super::super::wire_map::to_ui_node`]
/// exactly as [`super::super::region::reconcile_region`] would map a real
/// chip's tree on join. Advancing the real animation clock reports only
/// `mover`'s scope as moved; feeding that straight into
/// [`request_preem_repaint`] — the function `install_preem_clock` calls every
/// tick — must wake `bar_center`'s subscribers and leave `bar_left`'s alone,
/// even though `request_preem_repaint` probes *both* mailboxes (it does not
/// know in advance which region a moved plugin's card lives in).
///
/// **Falsification:** remove the early return in [`request_remap_holding`]
/// (`if !holds { return; }`) so it nudges unconditionally — `bar_left`'s
/// assertion below goes red, because `resident`'s mailbox has never held a
/// mover.
#[test]
fn an_animating_plugin_does_not_wake_a_static_bar_left_subscriber() {
    registry::reset_for_tests();

    let (tx, _rx) = mpsc::channel::<HostMsg>(4);

    // Map both plugins' trees exactly as a real region reconcile would — this
    // is what actually registers `mover`'s preem instance in
    // `preem_render`'s scope table, not just the mailbox bookkeeping below.
    let resident_scope = Scope::card("t906-resident");
    let _ = to_ui_node(&resident_scope, &static_node("chip"));
    let mover_scope = Scope::card("t906-mover");
    let _ = to_ui_node(&mover_scope, &marquee_node("chip"));

    let bar_left: Mutable<Vec<SlotRender>> =
        Mutable::new(vec![slot("t906-resident", static_node("chip"), &tx)]);
    let bar_center: Mutable<Vec<SlotRender>> =
        Mutable::new(vec![slot("t906-mover", marquee_node("chip"), &tx)]);

    registry::install(
        Box::new(FixtureService(fixture_handles(
            bar_left.clone(),
            bar_center.clone(),
        ))),
        hytte::reactive::runtime::handle(),
    );

    // Subscribe both mailboxes before the tick, and drain each signal's
    // immediate first `Ready` (a freshly-subscribed signal always yields its
    // current value once) so only a *repaint request* — the wake this test is
    // about — shows up as the next poll.
    let mut left_sig = pin!(bar_left.signal_cloned());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(
        left_sig.as_mut().poll_change(&mut cx),
        Poll::Ready(Some(_))
    ));
    let mut center_sig = pin!(bar_center.signal_cloned());
    assert!(matches!(
        center_sig.as_mut().poll_change(&mut cx),
        Poll::Ready(Some(_))
    ));

    // The real pump tick: advance every live renderer, then fan the result out
    // exactly as `install_preem_clock`'s callback does.
    let moved = preem_render::advance_all(preem_render::ANIM_STEP_SECS);
    assert_eq!(
        moved,
        vec![mover_scope.clone()],
        "only the marquee's scope should report movement — the label has no \
         preem instance to animate at all",
    );
    request_preem_repaint(&moved);

    assert!(
        matches!(left_sig.as_mut().poll_change(&mut cx), Poll::Pending),
        "an animating plugin elsewhere must not wake a static bar_left subscriber",
    );
    assert!(
        matches!(
            center_sig.as_mut().poll_change(&mut cx),
            Poll::Ready(Some(_))
        ),
        "the mailbox actually holding the animating plugin must be woken",
    );

    preem_render::forget_scope(&resident_scope);
    preem_render::forget_scope(&mover_scope);
}

// ── #921: the monitor-independent scope releaser ─────────────────────────────

/// Drive `releaser` one step and assert it is still parked on its signal.
///
/// The releaser never completes (it is a `while let` over a `Mutable`-backed
/// signal that lives as long as the mailboxes), so `Pending` is the only
/// correct answer — a `Ready` here would mean the loop broke, which in
/// production is the whole subscription going away.
fn step(releaser: &mut std::pin::Pin<&mut impl Future<Output = ()>>) {
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        releaser.as_mut().poll(&mut cx).is_pending(),
        "the scope releaser must stay subscribed for the process lifetime",
    );
}

/// A plugin leaving **every** render mailbox releases *both* of its preem
/// scopes, with no region, no drawer child and no widget of any kind involved
/// (#921).
///
/// This is the mechanism the production wiring rests on, isolated from GTK: the
/// releaser's only input is the union of the seven mailboxes' plugin ids, and
/// its only output is a `forget_scope` per departed id. `region::gtk_tests`'
/// `a_plugin_leaving_with_no_live_region_still_releases_its_card_scope` covers
/// the same fix in its production shape (a real region mounted, then destroyed,
/// then the plugin exiting); this one covers the part that shape cannot show —
/// that the departure is read off *all seven* mailboxes, so the id has to leave
/// the panel list as well as its region before anything is forgotten, which is
/// exactly the order `session.rs:815-824`'s teardown writes them in.
///
/// **Deletion check:** dropping the `forget_scope` loop from
/// [`drive_scope_releaser`] turns the **final** card assertion red with
/// `left: 1, right: 0`. Narrowing the union to the six region mailboxes turns
/// the **middle** one red instead, with `left: 0, right: 1` — the scopes would
/// be released while the plugin is still in the panel list.
#[test]
fn a_departing_plugin_releases_both_its_scopes_with_no_region_alive() {
    let (tx, _rx) = mpsc::channel::<HostMsg>(4);
    let card = Scope::card("t921-leaver");
    let panel = Scope::panel("t921-leaver");

    // The renderer instances a real chip mount and a real drawer child would
    // have built, mapped the same way they map them.
    let _ = to_ui_node(&card, &marquee_node("chip"));
    let _ = to_ui_node(&panel, &marquee_node("panel"));
    assert_eq!(preem_render::instance_count(&card), 1);
    assert_eq!(preem_render::instance_count(&panel), 1);

    // The seven mailboxes, with this plugin in one region (`bar_left`) and in
    // the shared panel list — where a chip with a drawer panel really sits.
    let mailboxes: [Mutable<Vec<SlotRender>>; RENDER_MAILBOXES] = [
        Mutable::new(Vec::new()),
        Mutable::new(Vec::new()),
        Mutable::new(Vec::new()),
        Mutable::new(vec![slot("t921-leaver", marquee_node("chip"), &tx)]),
        Mutable::new(Vec::new()),
        Mutable::new(Vec::new()),
        Mutable::new(vec![slot("t921-leaver", marquee_node("panel"), &tx)]),
    ];
    let bar_left = mailboxes[3].clone();
    let panels = mailboxes[6].clone();

    let mut releaser = pin!(drive_scope_releaser(live_plugin_ids_signal(mailboxes)));
    // The first emission only seeds "who is here"; nothing has left yet.
    step(&mut releaser);
    assert_eq!(preem_render::instance_count(&card), 1);

    // Teardown's order (`session.rs:815-824`): the six regions first…
    bar_left.set(Vec::new());
    step(&mut releaser);
    assert_eq!(
        preem_render::instance_count(&card),
        1,
        "a plugin still in the panel list has not left the host yet",
    );
    assert_eq!(preem_render::instance_count(&panel), 1);

    // …then the panel list. Now the id is in no mailbox at all.
    panels.set(Vec::new());
    step(&mut releaser);
    assert_eq!(
        preem_render::instance_count(&card),
        0,
        "a departed plugin's card scope must be released with no region alive \
         to run a retain loop (#921)",
    );
    assert_eq!(
        preem_render::instance_count(&panel),
        0,
        "…and its panel scope too — both of a plugin's trees go when it does",
    );
}

/// The repaint nudges the animation clock fires at up to 20 Hz must **not**
/// reach the releaser's body (#921).
///
/// `request_remap` deliberately re-emits a mailbox whose contents are unchanged
/// — that is how an advanced preem renderer reaches the screen. The releaser
/// rides those same mailboxes, so without the membership projection and the
/// `dedupe_cloned` in [`live_plugin_ids_signal`] it would rebuild a set and
/// diff it 20 times a second forever, deep-cloning every plugin's `wire::Node`
/// tree on the way.
///
/// **Deletion check:** removing `.dedupe_cloned()` turns the second assertion
/// red — the nudge propagates and the combined signal yields a set that is
/// equal to the previous one.
#[test]
fn a_repaint_nudge_does_not_wake_the_scope_releaser() {
    let (tx, _rx) = mpsc::channel::<HostMsg>(4);
    let bar_left = Mutable::new(vec![slot("t921-steady", marquee_node("chip"), &tx)]);
    let mailboxes: [Mutable<Vec<SlotRender>>; RENDER_MAILBOXES] = [
        Mutable::new(Vec::new()),
        Mutable::new(Vec::new()),
        Mutable::new(Vec::new()),
        bar_left.clone(),
        Mutable::new(Vec::new()),
        Mutable::new(Vec::new()),
        Mutable::new(Vec::new()),
    ];

    let mut live = pin!(live_plugin_ids_signal(mailboxes));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(live.as_mut().poll_change(&mut cx), Poll::Ready(Some(_))),
        "the first poll must yield the live set",
    );

    // The animation clock's nudge: same contents, re-emitted.
    request_remap(&bar_left);
    assert!(
        matches!(live.as_mut().poll_change(&mut cx), Poll::Pending),
        "a repaint nudge changes no membership, so it must not reach the \
         releaser — it fires up to 20 times a second",
    );

    // A real join still does.
    bar_left.set(vec![
        slot("t921-steady", marquee_node("chip"), &tx),
        slot("t921-newcomer", marquee_node("chip"), &tx),
    ]);
    assert!(
        matches!(live.as_mut().poll_change(&mut cx), Poll::Ready(Some(_))),
        "a plugin actually joining must reach the releaser",
    );

    preem_render::forget_scope(&Scope::card("t921-steady"));
}
