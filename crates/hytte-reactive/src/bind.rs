//! Helpers that spawn a per-binding future on `glib::MainContext`, drive
//! a `Signal`, and apply each emitted value to a GTK widget on the main
//! thread — holding the widget only weakly so a torn-down widget frees.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures_signals::signal::Signal;
use gtk::glib;
use gtk::prelude::*;

/// Spawn a future on the GTK main loop that drives `signal` and applies
/// each emitted value to `widget` via `apply`.
///
/// The apply-loop holds only a [`glib::WeakRef`] to `widget`, so the binding
/// never keeps the widget alive by itself. While the widget is in use its
/// parent chain (window → boxes → chips) holds it strong, so every emission
/// upgrades the weak ref and applies exactly as before. Once the last strong
/// ref is dropped — e.g. a bar or drawer is torn down on monitor hot-plug —
/// the next emission upgrades to `None`, the loop `break`s, and both the task
/// and its underlying signal subscription are released.
///
/// The apply-loop is *also* aborted eagerly from the widget's `destroy` signal,
/// so a widget torn down while its signal is parked releases the task (and its
/// subscription) immediately rather than lingering until the next emission wakes
/// it to break. The weak upgrade stays as the guarantee — it covers the sliver
/// between destroy and abort, and any widget freed without emitting `destroy` —
/// so the #224 free-on-drop behaviour is unchanged; the abort just trims the
/// residual to zero in the common teardown path.
pub fn bind<S, W, F>(signal: S, widget: &W, apply: F)
where
    S: Signal + 'static,
    S::Item: 'static,
    W: IsA<gtk::Widget> + Clone + 'static,
    F: Fn(&W, S::Item) + 'static,
{
    let weak = widget.downgrade();
    let handle = glib::MainContext::default().spawn_local(async move {
        let mut signal = std::pin::pin!(signal);
        while let Some(value) = std::future::poll_fn(|cx| signal.as_mut().poll_change(cx)).await {
            let Some(widget) = weak.upgrade() else { break };
            apply(&widget, value);
        }
    });
    abort_on_destroy(widget, handle);
}

/// Abort a bind's parked apply-loop the moment `widget` is destroyed, rather
/// than leaving it alive until the next emission wakes it to break on a dead
/// weak ref (see [`bind`]). Because the closure captures only the
/// [`glib::JoinHandle`] — never the widget — it introduces no strong ref that
/// could keep the widget alive.
fn abort_on_destroy<W: IsA<gtk::Widget>>(widget: &W, handle: glib::JoinHandle<()>) {
    widget.connect_destroy(move |_| handle.abort());
}

/// Bind a string-producing signal to a `gtk::Label`'s text.
pub fn bind_text<S>(signal: S, label: &gtk::Label)
where
    S: Signal + 'static,
    S::Item: AsRef<str> + 'static,
{
    bind(signal, label, |w, v| w.set_text(v.as_ref()));
}

/// Bind a bool signal to a widget's `visible` property.
pub fn bind_visible<W, S>(signal: S, widget: &W)
where
    W: IsA<gtk::Widget> + Clone + 'static,
    S: Signal<Item = bool> + 'static,
{
    bind(signal, widget, W::set_visible);
}

/// Bind a bool signal to whether `class` is present on the widget.
pub fn bind_class<W, S>(signal: S, widget: &W, class: &str)
where
    W: IsA<gtk::Widget> + Clone + 'static,
    S: Signal<Item = bool> + 'static,
{
    let class = class.to_owned();
    bind(signal, widget, move |w, v| {
        if v {
            w.add_css_class(&class);
        } else {
            w.remove_css_class(&class);
        }
    });
}

/// Two-way bind: signal drives a writable widget property while the user
/// can still drive that property themselves. The user-event handler is
/// blocked across each signal-driven `apply`, so programmatic state
/// mirroring never re-enters the user handler.
///
/// `connect_user` is invoked once at bind time. It must wire a user-event
/// handler (e.g. `connect_active_notify`, `connect_value_changed`,
/// `connect_toggled`) and return its [`glib::SignalHandlerId`]. The bind
/// future blocks that handler around every `apply` call and unblocks it
/// after.
///
/// Lifetime is tied to the widget the same way [`bind`] is: the apply-loop
/// holds only a [`glib::WeakRef`], so it applies while the widget is alive and
/// frees itself (breaking the loop) once the widget's last strong ref drops.
pub fn bind_two_way<S, W, V, Apply, Connect>(
    signal: S,
    widget: &W,
    apply: Apply,
    connect_user: Connect,
) where
    S: Signal<Item = V> + 'static,
    V: 'static,
    W: IsA<gtk::Widget> + Clone + 'static,
    Apply: Fn(&W, V) + 'static,
    Connect: FnOnce(&W) -> glib::SignalHandlerId,
{
    let handler_id = connect_user(widget);
    let weak = widget.downgrade();
    let handle = glib::MainContext::default().spawn_local(async move {
        let mut signal = std::pin::pin!(signal);
        while let Some(value) = std::future::poll_fn(|cx| signal.as_mut().poll_change(cx)).await {
            let Some(widget) = weak.upgrade() else { break };
            widget.block_signal(&handler_id);
            apply(&widget, value);
            widget.unblock_signal(&handler_id);
        }
    });
    abort_on_destroy(widget, handle);
}

/// How long after the user releases a drag a signal-driven `apply` stays
/// suppressed. A continuous poller (e.g. the mpris ~250 ms position poll bound
/// to a seek slider) needs a cycle or two to reflect the value the user just
/// committed, so without this settle window the first post-release poll would
/// still report the *pre-drag* value and snap the thumb back under the finger.
/// 600 ms ≈ 2–3 mpris poll cycles.
const DRAG_SETTLE: Duration = Duration::from_millis(600);

/// Pure suppression predicate for [`bind_two_way_drag_safe`]: a signal-driven
/// apply is suppressed while a pointer/touch grab is active (`grabbed`), or
/// while a grab ended less than `settle` ago. Split out with no GTK so the
/// drag-suppression logic is unit-testable without a display server.
fn drag_suppresses(
    grabbed: bool,
    last_release: Option<Instant>,
    now: Instant,
    settle: Duration,
) -> bool {
    grabbed || matches!(last_release, Some(t) if now.saturating_duration_since(t) < settle)
}

/// One piece of input on a drag-safe widget, as [`DragGate`] sees it.
///
/// GTK-free so the gate is unit-testable without a display server: the
/// widget's capture-phase controller and its `unmap`/`state-flags-changed`
/// handlers translate what GTK delivers into these (see [`drag_input`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DragInput {
    /// A pointer button went down on the widget (GDK's button number).
    Press(u32),
    /// A pointer button came back up.
    Release(u32),
    /// A touch sequence began on the widget.
    TouchBegin,
    /// A touch sequence ended, or the compositor cancelled it.
    TouchEnd,
    /// Whatever was holding the widget is gone *without* a release: the widget
    /// was unmapped mid-drag (a drawer closed with Esc, `modal::close_all`, a
    /// stack page switched away), made insensitive, or its pointer grab was
    /// broken. GTK delivers no release to the widget in any of those cases, so
    /// without this the gate would believe the drag never ended.
    Abandon,
}

/// What [`DragGate::offer`] decided about one signal value.
#[derive(Debug, PartialEq)]
enum Offered<V> {
    /// Not suppressed: apply it now.
    Apply(V),
    /// Suppressed: the gate holds it (replacing any older held value) until
    /// the grab and its settle window are over.
    Held,
}

/// What a delivery timer does when it fires — [`DragGate::fire`].
#[derive(Debug, PartialEq)]
enum Fired<V> {
    /// The grab and its settle window are over: apply the held value.
    Apply(V),
    /// Still inside the settle window (a release moved it): fire again after
    /// this long.
    Rearm(Duration),
    /// Nothing to deliver — no value is held, or a new grab began, whose own
    /// end arms a fresh timer.
    Idle,
}

/// The suppress → hold → deliver state machine behind
/// [`bind_two_way_drag_safe`], with no GTK in it.
///
/// It tracks which inputs are holding the widget, when the last grab ended,
/// and the **newest** signal value that arrived while suppressed. A suppressed
/// value is held rather than dropped: once nothing holds the widget and
/// [`DRAG_SETTLE`] has elapsed since the grab ended, a one-shot timer delivers
/// it. That is what makes the bind correct for a source that changes once
/// mid-drag and then goes quiet — a `Mutable` set by an event, not a poller —
/// which a "drop it, the next emission reconciles" rule leaves stale for good.
struct DragGate<V> {
    /// Pointer buttons held on the widget, one bit per GDK button number
    /// (numbers past 63 share the top bit). A bitmask rather than a flag so a
    /// second button pressed and released mid-drag does not end the drag the
    /// first one is still holding.
    buttons: u64,
    /// Touch sequences in progress on the widget.
    touches: u32,
    /// When the last grab ended — the start of the settle window.
    released_at: Option<Instant>,
    /// The newest value suppressed since the last apply.
    held: Option<V>,
    /// A delivery timer is scheduled, so a burst of suppressed values (or a
    /// release while one is pending) schedules one timer, not one each.
    armed: bool,
    settle: Duration,
}

impl<V> DragGate<V> {
    fn new(settle: Duration) -> Self {
        Self {
            buttons: 0,
            touches: 0,
            released_at: None,
            held: None,
            armed: false,
            settle,
        }
    }

    /// Whether any button or touch is holding the widget.
    fn grabbed(&self) -> bool {
        self.buttons != 0 || self.touches != 0
    }

    fn suppresses(&self, now: Instant) -> bool {
        drag_suppresses(self.grabbed(), self.released_at, now, self.settle)
    }

    /// Record one piece of input. Returns how long to wait before delivering
    /// the held value when this input left the widget un-held, a value is
    /// held, and no timer is armed yet — the caller schedules that timer.
    fn input(&mut self, input: DragInput, now: Instant) -> Option<Duration> {
        let was_grabbed = self.grabbed();
        match input {
            DragInput::Press(button) => self.buttons |= button_bit(button),
            DragInput::Release(button) => self.buttons &= !button_bit(button),
            DragInput::TouchBegin => self.touches = self.touches.saturating_add(1),
            DragInput::TouchEnd => self.touches = self.touches.saturating_sub(1),
            DragInput::Abandon => {
                self.buttons = 0;
                self.touches = 0;
            }
        }
        // Only the transition out of a grab starts a settle window: a release
        // of a button the gate never saw go down (or an `Abandon` while idle)
        // changes nothing.
        if was_grabbed && !self.grabbed() {
            self.released_at = Some(now);
        }
        self.arm(now)
    }

    /// A value from the signal: apply it, or hold it if a grab (or its settle
    /// window) is suppressing applies.
    fn offer(&mut self, value: V, now: Instant) -> Offered<V> {
        if self.suppresses(now) {
            self.held = Some(value);
            Offered::Held
        } else {
            // A fresh apply supersedes anything older still held.
            self.held = None;
            Offered::Apply(value)
        }
    }

    /// Schedule delivery of the held value: the wait for a new timer, or
    /// `None` when nothing is held, a timer is already armed, or a grab is
    /// live — its end reaches here again through [`Self::input`].
    fn arm(&mut self, now: Instant) -> Option<Duration> {
        if self.armed || self.held.is_none() || self.grabbed() {
            return None;
        }
        self.armed = true;
        Some(self.settle_left(now))
    }

    /// The delivery timer fired.
    fn fire(&mut self, now: Instant) -> Fired<V> {
        self.armed = false;
        if self.held.is_none() || self.grabbed() {
            return Fired::Idle;
        }
        if self.suppresses(now) {
            self.armed = true;
            return Fired::Rearm(self.settle_left(now));
        }
        self.held.take().map_or(Fired::Idle, Fired::Apply)
    }

    /// Forget the held value for good — the widget was destroyed, so an armed
    /// timer must find nothing to apply.
    fn discard(&mut self) {
        self.held = None;
    }

    /// Time left in the settle window, rounded **up** to whole milliseconds:
    /// a glib timeout truncates its interval to milliseconds, and a timer that
    /// fired a fraction early would only have to re-arm.
    fn settle_left(&self, now: Instant) -> Duration {
        let elapsed = self
            .released_at
            .map_or(self.settle, |t| now.saturating_duration_since(t));
        let left = self.settle.saturating_sub(elapsed);
        let millis = left.as_nanos().div_ceil(1_000_000);
        Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
    }
}

/// [`DragGate::buttons`]' bit for GDK button number `button`.
fn button_bit(button: u32) -> u64 {
    1_u64 << button.min(63)
}

/// Translate one event the capture-phase controller saw into gate input.
/// Everything that is not a press, release or grab change — motion, crossing,
/// scroll, keys — is `None`.
fn drag_input(event: &gtk::gdk::Event) -> Option<DragInput> {
    use gtk::gdk::EventType;
    let button = || {
        event
            .downcast_ref::<gtk::gdk::ButtonEvent>()
            .map(gtk::gdk::ButtonEvent::button)
    };
    match event.event_type() {
        EventType::ButtonPress => button().map(DragInput::Press),
        EventType::ButtonRelease => button().map(DragInput::Release),
        EventType::TouchBegin => Some(DragInput::TouchBegin),
        EventType::TouchEnd | EventType::TouchCancel => Some(DragInput::TouchEnd),
        EventType::GrabBroken => Some(DragInput::Abandon),
        _ => None,
    }
}

/// The live half of one drag-safe binding: the [`DragGate`] plus everything an
/// apply needs — the widget (weakly, as every bind holds it), the user handler
/// to block across the apply, and the caller's `apply`.
///
/// Strong refs to it are held only by the bind's apply-loop (aborted when the
/// widget is destroyed) and by a delivery timer while one is scheduled. The
/// handlers wired onto the widget itself hold a [`std::rc::Weak`], so the
/// widget never owns its own `apply` closure — the same ownership the plain
/// binds have, where `apply` lives in the loop alone.
struct DragSafe<W: glib::object::ObjectType, V, A> {
    gate: RefCell<DragGate<V>>,
    widget: glib::WeakRef<W>,
    handler_id: glib::SignalHandlerId,
    apply: A,
}

impl<W, V, A> DragSafe<W, V, A>
where
    W: IsA<gtk::Widget> + 'static,
    V: 'static,
    A: Fn(&W, V) + 'static,
{
    /// Apply `value` with the user handler blocked. A no-op once the widget
    /// has been freed.
    fn apply_now(&self, value: V) {
        let Some(widget) = self.widget.upgrade() else {
            return;
        };
        widget.block_signal(&self.handler_id);
        (self.apply)(&widget, value);
        widget.unblock_signal(&self.handler_id);
    }

    /// A value from the signal.
    fn offer(self: &Rc<Self>, value: V) {
        let now = Instant::now();
        let offered = self.gate.borrow_mut().offer(value, now);
        match offered {
            Offered::Apply(value) => self.apply_now(value),
            Offered::Held => {
                let wait = self.gate.borrow_mut().arm(now);
                if let Some(wait) = wait {
                    self.schedule(wait);
                }
            }
        }
    }

    /// Input on the widget.
    fn input(self: &Rc<Self>, input: DragInput) {
        let wait = self.gate.borrow_mut().input(input, Instant::now());
        if let Some(wait) = wait {
            self.schedule(wait);
        }
    }

    /// A one-shot delivery timer: after `wait`, apply the held value if the
    /// grab and its settle window are over, re-arm if a later release moved
    /// the window, or stop.
    fn schedule(self: &Rc<Self>, wait: Duration) {
        let this = Rc::clone(self);
        glib::timeout_add_local_once(wait, move || {
            let fired = this.gate.borrow_mut().fire(Instant::now());
            match fired {
                Fired::Apply(value) => this.apply_now(value),
                Fired::Rearm(wait) => this.schedule(wait),
                Fired::Idle => {}
            }
        });
    }
}

/// Drag-safe two-way bind for **continuous** widgets — a `gtk::Scale`/`Range`
/// whose value a signal keeps writing while the user can also drag it.
///
/// Identical to [`bind_two_way`] but adds *grab suppression*: while the user is
/// actively dragging the widget — and for a short `DRAG_SETTLE` window after
/// they let go — signal-driven `apply` calls are held back. Without this, a
/// continuous poller (e.g. the mpris ~250 ms position poll bound to a seek
/// slider) writes the widget's value mid-drag and yanks the thumb back out from
/// under the user's finger; the poller and the user fight over the widget.
///
/// # Contract: eventually consistent
///
/// A held value is **deferred, not dropped**. Only the newest one is kept, and
/// once nothing holds the widget and the settle window has elapsed it is
/// applied exactly once, by a one-shot timer — whether or not the signal emits
/// again. So the widget always ends up showing the signal's latest value, at
/// the latest `DRAG_SETTLE` after the grab ends; it may lag the signal in
/// between, but never stays stale. That holds for a ticking poller and equally
/// for a `Mutable` that changes once mid-drag and never again. A value that
/// arrives while nothing suppresses applies immediately, and supersedes
/// anything still held.
///
/// # Grab tracking
///
/// Grab state is tracked with a capture-phase [`gtk::EventControllerLegacy`] —
/// raw button/touch press and release events. Unlike a `GestureClick`, a legacy
/// controller does not participate in gesture claiming, so it is *not* cancelled
/// mid-drag when the range's own drag gesture claims the sequence. Each held
/// button and touch is tracked on its own, so the drag lasts until the **last**
/// of them is released — pressing and releasing a second button mid-drag does
/// not end it. Because a release can fail to arrive at all, three more things
/// end a grab: the widget being unmapped (a drawer closed mid-drag), the widget
/// becoming insensitive, and a GDK grab-broken event. The controller is owned
/// by the widget, so it lives and dies with it, and it only observes (returns
/// [`glib::Propagation::Proceed`]) — the widget's own handlers still run.
///
/// Lifetime is tied to the widget the same way [`bind`] is: nothing wired onto
/// the widget holds `apply` or the widget strongly, and destroying the widget
/// aborts the apply-loop and discards any held value, so a pending delivery
/// timer applies nothing.
///
/// Prefer plain [`bind_two_way`] for **discrete** widgets (switches, toggles,
/// spin rows): they have no drag to protect and no fighting poller, so the extra
/// controller and suppression would only add overhead.
pub fn bind_two_way_drag_safe<S, W, V, Apply, Connect>(
    signal: S,
    widget: &W,
    apply: Apply,
    connect_user: Connect,
) where
    S: Signal<Item = V> + 'static,
    V: 'static,
    W: IsA<gtk::Widget> + Clone + 'static,
    Apply: Fn(&W, V) + 'static,
    Connect: FnOnce(&W) -> glib::SignalHandlerId,
{
    drop(drag_safe_bind(
        signal,
        widget,
        apply,
        connect_user,
        DRAG_SETTLE,
    ));
}

/// [`bind_two_way_drag_safe`] with the settle window as a parameter, returning
/// the binding's live half so a display test can feed it input directly and
/// wait out a short window instead of [`DRAG_SETTLE`].
fn drag_safe_bind<S, W, V, Apply, Connect>(
    signal: S,
    widget: &W,
    apply: Apply,
    connect_user: Connect,
    settle: Duration,
) -> Rc<DragSafe<W, V, Apply>>
where
    S: Signal<Item = V> + 'static,
    V: 'static,
    W: IsA<gtk::Widget> + Clone + 'static,
    Apply: Fn(&W, V) + 'static,
    Connect: FnOnce(&W) -> glib::SignalHandlerId,
{
    let binding = Rc::new(DragSafe {
        gate: RefCell::new(DragGate::new(settle)),
        widget: widget.downgrade(),
        handler_id: connect_user(widget),
        apply,
    });

    let controller = gtk::EventControllerLegacy::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let for_events = Rc::downgrade(&binding);
    controller.connect_event(move |_, event| {
        if let (Some(input), Some(binding)) = (drag_input(event), for_events.upgrade()) {
            binding.input(input);
        }
        // Observe only — never consume; the widget's own drag handling runs.
        glib::Propagation::Proceed
    });
    widget.add_controller(controller);

    // A release that never arrives must not hold the widget forever: an
    // unmapped or insensitive widget is not sent the release of a drag that
    // was live when it went away.
    let for_unmap = Rc::downgrade(&binding);
    widget.connect_unmap(move |_| {
        if let Some(binding) = for_unmap.upgrade() {
            binding.input(DragInput::Abandon);
        }
    });
    let for_sensitivity = Rc::downgrade(&binding);
    widget.connect_state_flags_changed(move |w, _| {
        if w.state_flags().contains(gtk::StateFlags::INSENSITIVE)
            && let Some(binding) = for_sensitivity.upgrade()
        {
            binding.input(DragInput::Abandon);
        }
    });

    let for_loop = Rc::clone(&binding);
    let handle = glib::MainContext::default().spawn_local(async move {
        let mut signal = std::pin::pin!(signal);
        while let Some(value) = std::future::poll_fn(|cx| signal.as_mut().poll_change(cx)).await {
            if for_loop.widget.upgrade().is_none() {
                break;
            }
            for_loop.offer(value);
        }
    });

    // As `abort_on_destroy`, plus: drop whatever is held, so a delivery timer
    // still scheduled finds nothing and never touches the destroyed widget
    // (whose user handler is gone by then, so blocking it would warn).
    let for_destroy = Rc::downgrade(&binding);
    widget.connect_destroy(move |_| {
        if let Some(binding) = for_destroy.upgrade() {
            binding.gate.borrow_mut().discard();
        }
        handle.abort();
    });

    binding
}

// Pure drag-suppression logic — no GTK, so it runs in the default (hermetic)
// test bucket. The actual pointer-grab tracking is display-dependent (a real
// `gtk::Scale` drag) and is covered by manual live-verify, not here.
#[cfg(test)]
mod drag_suppress_tests {
    use super::{DRAG_SETTLE, drag_suppresses};
    use std::time::{Duration, Instant};

    #[test]
    fn active_grab_always_suppresses() {
        let now = Instant::now();
        // No prior release, but the grab is live → suppress regardless.
        assert!(drag_suppresses(true, None, now, DRAG_SETTLE));
        assert!(drag_suppresses(true, Some(now), now, DRAG_SETTLE));
    }

    #[test]
    fn idle_with_no_release_applies() {
        let now = Instant::now();
        // Not grabbed and never released → the poller value flows through.
        assert!(!drag_suppresses(false, None, now, DRAG_SETTLE));
    }

    #[test]
    fn release_within_settle_suppresses() {
        let released = Instant::now();
        let now = released + Duration::from_millis(300);
        // 300 ms < 600 ms settle → still suppressed so the daemon can catch up.
        assert!(drag_suppresses(false, Some(released), now, DRAG_SETTLE));
    }

    #[test]
    fn release_past_settle_applies_again() {
        let released = Instant::now();
        let now = released + Duration::from_millis(900);
        // 900 ms ≥ 600 ms settle → the poller resumes and the widget reconciles.
        assert!(!drag_suppresses(false, Some(released), now, DRAG_SETTLE));
    }

    #[test]
    fn settle_boundary_is_exclusive() {
        let released = Instant::now();
        // Exactly at the settle window: `< settle` is false → not suppressed.
        assert!(!drag_suppresses(
            false,
            Some(released),
            released + DRAG_SETTLE,
            DRAG_SETTLE
        ));
    }
}

// The suppress → hold → deliver state machine, driven by hand: every input and
// timer firing is a method call at a chosen `Instant`, so the whole lifecycle —
// including the cases where GTK never sends a release — runs hermetically.
#[cfg(test)]
mod drag_gate_tests {
    use super::{DRAG_SETTLE, DragGate, DragInput, Fired, Offered};
    use std::time::{Duration, Instant};

    const PRIMARY: u32 = 1;
    const SECONDARY: u32 = 3;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn gate() -> DragGate<u32> {
        DragGate::new(DRAG_SETTLE)
    }

    #[test]
    fn a_value_with_nothing_held_applies_at_once() {
        let mut gate = gate();
        let now = Instant::now();
        assert_eq!(gate.offer(7, now), Offered::Apply(7));
        assert_eq!(
            gate.arm(now),
            None,
            "nothing is held, so nothing to schedule"
        );
    }

    /// The #1 regression: a value that arrives mid-drag and is never followed by
    /// another emission still reaches the widget once the drag is over. The old
    /// rule dropped it and waited for an emission that never came.
    #[test]
    fn a_value_from_mid_drag_is_delivered_after_the_settle_window() {
        let mut gate = gate();
        let t0 = Instant::now();
        assert_eq!(gate.input(DragInput::Press(PRIMARY), t0), None);
        assert_eq!(gate.offer(5, t0 + ms(100)), Offered::Held);
        assert_eq!(
            gate.arm(t0 + ms(100)),
            None,
            "no timer while the grab is live — its release schedules one",
        );

        let released = t0 + ms(400);
        assert_eq!(
            gate.input(DragInput::Release(PRIMARY), released),
            Some(DRAG_SETTLE),
            "ending the grab with a value held schedules its delivery",
        );
        assert_eq!(gate.fire(released + DRAG_SETTLE), Fired::Apply(5));
        assert_eq!(
            gate.fire(released + DRAG_SETTLE + ms(1)),
            Fired::Idle,
            "delivered exactly once",
        );
    }

    #[test]
    fn only_the_newest_held_value_is_delivered() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::Press(PRIMARY), t0);
        for v in [1, 2, 3] {
            assert_eq!(gate.offer(v, t0), Offered::Held);
        }
        let wait = gate.input(DragInput::Release(PRIMARY), t0);
        assert_eq!(wait, Some(DRAG_SETTLE));
        assert_eq!(gate.fire(t0 + DRAG_SETTLE), Fired::Apply(3));
    }

    #[test]
    fn a_value_held_inside_the_settle_window_waits_out_only_the_rest_of_it() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::Press(PRIMARY), t0);
        assert_eq!(
            gate.input(DragInput::Release(PRIMARY), t0),
            None,
            "nothing held at release time, so nothing to schedule",
        );
        assert_eq!(gate.offer(9, t0 + ms(200)), Offered::Held);
        assert_eq!(gate.arm(t0 + ms(200)), Some(ms(400)));
        assert_eq!(gate.fire(t0 + ms(600)), Fired::Apply(9));
    }

    #[test]
    fn a_burst_of_held_values_arms_one_timer() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::Press(PRIMARY), t0);
        gate.input(DragInput::Release(PRIMARY), t0);
        gate.offer(1, t0 + ms(10));
        assert!(gate.arm(t0 + ms(10)).is_some(), "the first held value arms");
        gate.offer(2, t0 + ms(20));
        assert_eq!(gate.arm(t0 + ms(20)), None, "a timer is already armed");
    }

    #[test]
    fn a_new_grab_inside_the_window_defers_delivery_to_its_own_release() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::Press(PRIMARY), t0);
        gate.offer(4, t0);
        assert_eq!(
            gate.input(DragInput::Release(PRIMARY), t0),
            Some(DRAG_SETTLE)
        );
        assert_eq!(
            gate.input(DragInput::Press(PRIMARY), t0 + ms(300)),
            None,
            "a timer is already armed",
        );
        assert_eq!(
            gate.fire(t0 + ms(600)),
            Fired::Idle,
            "the user grabbed again — keep holding, and let that release re-arm",
        );
        let second = t0 + ms(900);
        assert_eq!(
            gate.input(DragInput::Release(PRIMARY), second),
            Some(DRAG_SETTLE)
        );
        assert_eq!(gate.fire(second + DRAG_SETTLE), Fired::Apply(4));
    }

    #[test]
    fn a_quick_regrab_moves_an_armed_timer_by_re_arming() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::Press(PRIMARY), t0);
        gate.offer(8, t0);
        assert_eq!(
            gate.input(DragInput::Release(PRIMARY), t0),
            Some(DRAG_SETTLE)
        );
        // Pressed and released again before the armed timer fires: the window
        // now ends 200 ms later than that timer was scheduled for.
        gate.input(DragInput::Press(PRIMARY), t0 + ms(100));
        assert_eq!(gate.input(DragInput::Release(PRIMARY), t0 + ms(200)), None);
        assert_eq!(gate.fire(t0 + ms(600)), Fired::Rearm(ms(200)));
        assert_eq!(gate.fire(t0 + ms(800)), Fired::Apply(8));
    }

    #[test]
    fn a_fresh_apply_supersedes_a_held_value() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::Press(PRIMARY), t0);
        gate.offer(1, t0);
        gate.input(DragInput::Release(PRIMARY), t0);
        // The window closed, and a newer value beat the timer to it.
        assert_eq!(gate.offer(2, t0 + DRAG_SETTLE), Offered::Apply(2));
        assert_eq!(
            gate.fire(t0 + DRAG_SETTLE + ms(1)),
            Fired::Idle,
            "the older held value must not overwrite the newer one",
        );
    }

    #[test]
    fn a_second_button_released_mid_drag_does_not_end_it() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::Press(PRIMARY), t0);
        gate.input(DragInput::Press(SECONDARY), t0 + ms(50));
        assert_eq!(
            gate.input(DragInput::Release(SECONDARY), t0 + ms(100)),
            None
        );
        assert!(gate.grabbed(), "the primary button still holds the widget");
        assert_eq!(gate.offer(6, t0 + ms(2000)), Offered::Held);

        let released = t0 + ms(3000);
        assert_eq!(
            gate.input(DragInput::Release(PRIMARY), released),
            Some(DRAG_SETTLE)
        );
        assert_eq!(gate.fire(released + DRAG_SETTLE), Fired::Apply(6));
    }

    #[test]
    fn touches_hold_the_widget_until_the_last_one_ends() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::TouchBegin, t0);
        gate.input(DragInput::TouchBegin, t0);
        gate.input(DragInput::TouchEnd, t0);
        assert!(gate.grabbed());
        assert_eq!(gate.offer(2, t0), Offered::Held);
        assert_eq!(gate.input(DragInput::TouchEnd, t0), Some(DRAG_SETTLE));
        assert!(!gate.grabbed());
    }

    /// The #2 regression: a drag whose release never arrives — the drawer was
    /// closed mid-drag, so the widget was unmapped — used to leave the grab set
    /// for the rest of the session, suppressing every later value.
    #[test]
    fn abandon_ends_a_grab_whose_release_never_comes() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::Press(PRIMARY), t0);
        gate.offer(3, t0);
        assert_eq!(
            gate.input(DragInput::Abandon, t0 + ms(10)),
            Some(DRAG_SETTLE)
        );
        assert!(!gate.grabbed());
        assert_eq!(gate.fire(t0 + ms(10) + DRAG_SETTLE), Fired::Apply(3));

        // Later values flow, and the release that finally turns up (if it ever
        // does) for a button the gate already forgot opens no new window.
        let later = t0 + ms(5000);
        assert_eq!(gate.input(DragInput::Release(PRIMARY), later), None);
        assert_eq!(gate.offer(4, later), Offered::Apply(4));
    }

    #[test]
    fn abandon_while_idle_opens_no_settle_window() {
        let mut gate = gate();
        let t0 = Instant::now();
        assert_eq!(gate.input(DragInput::Abandon, t0), None);
        assert_eq!(
            gate.offer(1, t0),
            Offered::Apply(1),
            "an unmap with no drag in progress must not suppress anything",
        );
    }

    #[test]
    fn a_stray_release_or_touch_end_is_ignored() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::Release(PRIMARY), t0);
        gate.input(DragInput::TouchEnd, t0);
        assert!(!gate.grabbed());
        assert_eq!(gate.offer(1, t0), Offered::Apply(1));
    }

    #[test]
    fn button_numbers_past_the_mask_share_its_top_bit() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::Press(u32::MAX), t0);
        assert!(gate.grabbed());
        gate.input(DragInput::Release(64), t0);
        assert!(!gate.grabbed());
    }

    #[test]
    fn discard_leaves_an_armed_timer_nothing_to_apply() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::Press(PRIMARY), t0);
        gate.offer(1, t0);
        assert!(gate.input(DragInput::Release(PRIMARY), t0).is_some());
        gate.discard();
        assert_eq!(gate.fire(t0 + DRAG_SETTLE), Fired::Idle);
    }

    #[test]
    fn settle_left_rounds_up_to_whole_milliseconds() {
        let mut gate = gate();
        let t0 = Instant::now();
        gate.input(DragInput::Press(PRIMARY), t0);
        gate.input(DragInput::Release(PRIMARY), t0);
        assert_eq!(
            gate.settle_left(t0 + Duration::from_micros(100_500)),
            ms(500)
        );
        assert_eq!(gate.settle_left(t0 + DRAG_SETTLE), Duration::ZERO);
        assert_eq!(gate.settle_left(t0 + ms(5000)), Duration::ZERO);
    }
}

// These exercise live GTK widgets, so they need a display server — gated into
// the `system-tests` bucket rather than run by default.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::*;
    use futures_signals::signal::Mutable;
    use gtk::glib;
    use std::cell::Cell;
    use std::rc::Rc;

    /// A signal emission applies the value, and the user-event handler is
    /// NOT re-fired while the apply runs.
    #[gtk::test]
    fn signal_apply_does_not_refire_user_handler() {
        let ctx = glib::MainContext::default();

        let switch = gtk::Switch::new();
        let state = Mutable::new(false);
        let user_calls = Rc::new(Cell::new(0u32));

        let user_calls_for_handler = user_calls.clone();
        bind_two_way(state.signal(), &switch, gtk::Switch::set_active, move |w| {
            let counter = user_calls_for_handler.clone();
            w.connect_active_notify(move |_| counter.set(counter.get() + 1))
        });

        // Pump until the initial Mutable emission applies.
        while ctx.iteration(false) {}

        // Drive a state change. apply() will call set_active(true), which
        // would normally fire active-notify. The handler must stay blocked.
        state.set(true);
        while ctx.iteration(false) {}

        assert_eq!(
            user_calls.get(),
            0,
            "user handler must not fire during signal-driven apply"
        );
        assert!(switch.is_active(), "apply did set the property");
    }

    /// Dropping the last strong ref to a bound widget frees it: the next
    /// signal emission wakes the parked apply-loop, its `WeakRef` upgrade
    /// returns `None`, and the loop breaks. On the old strong-clone/`for_each`
    /// code the future pinned the widget forever, so `weak.upgrade()` would
    /// still be `Some` — this is the #224 regression test.
    #[gtk::test]
    fn dropping_bound_widget_frees_it_on_next_emission() {
        let ctx = glib::MainContext::default();

        let label = gtk::Label::new(None);
        let weak = label.downgrade();
        let state = Mutable::new(String::from("a"));

        bind_text(state.signal_cloned(), &label);
        while ctx.iteration(false) {}
        assert_eq!(
            weak.upgrade().map(|l| l.text().to_string()),
            Some(String::from("a")),
            "bound label applied the initial value"
        );

        // Drop the only strong ref we hold. A weakly-held bind future must not
        // keep the widget alive.
        drop(label);

        // Emit again: this wakes the parked apply-loop, which upgrades the
        // (now-dangling) weak ref, gets `None`, and breaks — releasing it.
        state.set(String::from("b"));
        while ctx.iteration(false) {}

        assert!(
            weak.upgrade().is_none(),
            "bound widget must be freed once its last strong ref is dropped"
        );
    }

    /// A genuine user action still fires the user handler — the block is
    /// released between applies.
    #[gtk::test]
    fn user_event_still_fires_after_apply() {
        let ctx = glib::MainContext::default();

        let switch = gtk::Switch::new();
        let state = Mutable::new(false);
        let user_calls = Rc::new(Cell::new(0u32));

        let user_calls_for_handler = user_calls.clone();
        bind_two_way(state.signal(), &switch, gtk::Switch::set_active, move |w| {
            let counter = user_calls_for_handler.clone();
            w.connect_active_notify(move |_| counter.set(counter.get() + 1))
        });

        while ctx.iteration(false) {}

        // Simulate a user-driven flip by toggling active directly. Because
        // the signal hasn't emitted, the handler must NOT be blocked.
        switch.set_active(true);
        while ctx.iteration(false) {}

        assert_eq!(
            user_calls.get(),
            1,
            "user-driven set_active must fire the user handler exactly once"
        );
    }

    /// With no active grab, `bind_two_way_drag_safe` applies signal values to
    /// the widget exactly like `bind_two_way` — the drag guard only kicks in
    /// while the user is dragging. (The suppression path is driven below by
    /// feeding the binding its input directly; that real pointer input reaches
    /// the controller is live-verified on a real `gtk::Scale` drag.)
    #[gtk::test]
    fn drag_safe_applies_when_not_grabbed() {
        let ctx = glib::MainContext::default();

        let scale = gtk::Scale::new(
            gtk::Orientation::Horizontal,
            Some(&gtk::Adjustment::new(0.0, 0.0, 1.0, 0.01, 0.1, 0.0)),
        );
        let state = Mutable::new(0.0_f64);
        let user_calls = Rc::new(Cell::new(0u32));

        let user_calls_for_handler = user_calls.clone();
        bind_two_way_drag_safe(
            state.signal(),
            &scale,
            gtk::prelude::RangeExt::set_value,
            move |s| {
                let counter = user_calls_for_handler.clone();
                s.connect_value_changed(move |_| counter.set(counter.get() + 1))
            },
        );

        while ctx.iteration(false) {}

        // Idle (no grab): a signal emission applies to the widget, and the user
        // handler stays blocked across the programmatic apply.
        state.set(0.75);
        while ctx.iteration(false) {}

        assert!(
            (gtk::prelude::RangeExt::value(&scale) - 0.75).abs() < f64::EPSILON,
            "idle drag-safe bind must apply the signal value"
        );
        assert_eq!(
            user_calls.get(),
            0,
            "user handler must not fire during signal-driven apply"
        );
    }

    // ── Held values reach the widget (the gate's GTK half) ───────────────────
    //
    // These feed `DragInput` straight into the binding — the press a real drag
    // would send through the capture controller — and then let the real glib
    // timer, the real `unmap`/`state-flags-changed` signals and the real
    // `destroy` do the rest. Pointer input itself (that the controller sees a
    // `gtk::Scale` drag's press and release) stays a live-verify item.

    /// A settle window short enough to wait out, long enough that a value
    /// applied inside it would be caught.
    const TEST_SETTLE: Duration = Duration::from_millis(50);

    /// Iterate the main context until `done`, failing after a bound that is a
    /// ceiling on a failure, never a delay on a pass.
    fn pump_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let ctx = glib::MainContext::default();
        while !done() {
            assert!(
                Instant::now() < deadline,
                "{what} did not happen within 5 s"
            );
            if !ctx.iteration(false) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Iterate the main context for `span` of wall time.
    fn pump_for(span: Duration) {
        let until = Instant::now() + span;
        let ctx = glib::MainContext::default();
        while Instant::now() < until {
            if !ctx.iteration(false) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn unit_scale() -> gtk::Scale {
        gtk::Scale::new(
            gtk::Orientation::Horizontal,
            Some(&gtk::Adjustment::new(0.0, 0.0, 1.0, 0.01, 0.1, 0.0)),
        )
    }

    fn value_is(scale: &gtk::Scale, want: f64) -> bool {
        (gtk::prelude::RangeExt::value(scale) - want).abs() < f64::EPSILON
    }

    type ScaleBinding = Rc<DragSafe<gtk::Scale, f64, fn(&gtk::Scale, f64)>>;

    /// A drag-safe scale over `state`, with its user handler counting into
    /// `user_calls`.
    fn bound_scale(state: &Mutable<f64>, user_calls: &Rc<Cell<u32>>) -> (gtk::Scale, ScaleBinding) {
        let scale = unit_scale();
        let counter = user_calls.clone();
        let binding = drag_safe_bind(
            state.signal(),
            &scale,
            gtk::prelude::RangeExt::set_value as fn(&gtk::Scale, f64),
            move |s| s.connect_value_changed(move |_| counter.set(counter.get() + 1)),
            TEST_SETTLE,
        );
        pump_until("the initial value", || value_is(&scale, 0.0));
        (scale, binding)
    }

    /// Set `state` to `value` while `binding` is grabbed, and wait until the
    /// apply-loop has seen it — and held it rather than applied it.
    fn set_mid_drag(binding: &ScaleBinding, state: &Mutable<f64>, value: f64) {
        assert!(binding.gate.borrow().grabbed(), "only meaningful mid-drag");
        state.set(value);
        pump_until("the value to be held", || {
            binding
                .gate
                .borrow()
                .held
                .is_some_and(|held| (held - value).abs() < f64::EPSILON)
        });
    }

    /// The #1 regression end to end: one `set` mid-drag, no emission after it,
    /// and the widget still ends up showing it — through the one-shot timer.
    #[gtk::test]
    fn drag_safe_delivers_a_value_set_once_mid_drag() {
        let state = Mutable::new(0.0_f64);
        let user_calls = Rc::new(Cell::new(0u32));
        let (scale, binding) = bound_scale(&state, &user_calls);

        binding.input(DragInput::Press(1));
        set_mid_drag(&binding, &state, 0.75);
        pump_for(TEST_SETTLE * 2);
        assert!(value_is(&scale, 0.0), "held while the grab is live");

        let released = Instant::now();
        binding.input(DragInput::Release(1));
        pump_until("the held value's delivery", || value_is(&scale, 0.75));
        assert!(
            released.elapsed() >= TEST_SETTLE,
            "held through the settle window, delivered after {:?}",
            released.elapsed(),
        );
        assert_eq!(
            user_calls.get(),
            0,
            "the deferred apply blocks the user handler like any other",
        );
    }

    /// The #2 regression end to end: the window holding a mid-drag widget goes
    /// away, no release ever arrives, and the held value still lands — and
    /// later values are not suppressed.
    #[gtk::test]
    fn drag_safe_unmap_mid_drag_ends_the_grab() {
        let state = Mutable::new(0.0_f64);
        let user_calls = Rc::new(Cell::new(0u32));
        let (scale, binding) = bound_scale(&state, &user_calls);
        let window = gtk::Window::new();
        window.set_child(Some(&scale));
        window.present();
        pump_until("the scale to map", || scale.is_mapped());

        binding.input(DragInput::Press(1));
        set_mid_drag(&binding, &state, 0.5);
        window.set_visible(false);
        assert!(!scale.is_mapped());
        pump_until("the held value's delivery", || value_is(&scale, 0.5));

        state.set(0.25);
        pump_until("a later value", || value_is(&scale, 0.25));
        window.destroy();
    }

    #[gtk::test]
    fn drag_safe_insensitive_mid_drag_ends_the_grab() {
        let state = Mutable::new(0.0_f64);
        let user_calls = Rc::new(Cell::new(0u32));
        let (scale, binding) = bound_scale(&state, &user_calls);

        binding.input(DragInput::Press(1));
        set_mid_drag(&binding, &state, 0.5);
        scale.set_sensitive(false);
        pump_until("the held value's delivery", || value_is(&scale, 0.5));
    }

    /// A value held when the widget is dropped neither keeps it alive nor gets
    /// applied to it afterwards: the timer finds nothing once `destroy` ran.
    #[gtk::test]
    fn drag_safe_held_value_does_not_pin_the_widget() {
        let state = Mutable::new(0.0_f64);
        let user_calls = Rc::new(Cell::new(0u32));
        let (scale, binding) = bound_scale(&state, &user_calls);
        let weak = scale.downgrade();

        binding.input(DragInput::Press(1));
        set_mid_drag(&binding, &state, 0.5);
        binding.input(DragInput::Release(1));
        assert!(binding.gate.borrow().armed, "a delivery timer is scheduled");
        drop(scale);
        assert!(
            weak.upgrade().is_none(),
            "neither the binding nor its timer holds the widget",
        );
        assert!(
            binding.gate.borrow().held.is_none(),
            "destroy discarded the held value",
        );
        pump_for(TEST_SETTLE * 3);
        assert!(!binding.gate.borrow().armed, "the timer fired and stopped");
    }
}
