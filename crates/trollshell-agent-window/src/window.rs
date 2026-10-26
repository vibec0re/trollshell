//! One agent's window: the chrome, the page, and the lane its buttons write
//! to.
//!
//! In the **library** rather than in `main.rs` because nothing could reach it
//! there (#1130 L2): deleting the `page_loaded` latch — so the embedded view
//! is rebuilt on every poll, throwing away the scroll position and any
//! half-typed message twice a second, the exact thing the latch exists for —
//! changed no test. The same hole covered the `NO_PAGE` hint, the banner, the
//! tab forwarding and the refusal path. `main.rs` is now the command line, the
//! application and one call into here; `gtk_tests` drives [`Window::update`]
//! and asserts what it did.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

use hytte_plugin_agents::config::AgentsConfig;
use hytte_plugin_agents::hive::wire::{HiveUrls, Request};
use hytte_plugin_agents::model::{AgentName, agent_url};
use hytte_plugin_agents::plugin::Shown;

use crate::chrome::{self, Controls, Decision, Facts, HeaderModel, Queue, Verdict};
use crate::feed::{self, AgentState, Update};
use crate::{cli, page, tls, ui, webview};

/// What the window shows before the hive has a page URL for this agent.
pub const NO_PAGE: &str = "This hive publishes no page for this agent yet — its domain is unconfigured, or the agent is \
     not on its roster. The header above still follows the agent's live status.";

/// Whether the compositor is **presenting** this window's content — the poll's
/// visibility signal (#1149 L4, reworked after this round's review).
///
/// The first cut read GTK's `map`/`unmap` on the toplevel, and on the target
/// compositor that parks nothing. No Wayland compositor can unmap a client's
/// toplevel — only the client can — so `map`/`unmap` report this binary's own
/// `present()`/teardown and nothing about whether anyone is looking; niri has
/// no minimise at all, and a window on an inactive workspace stays mapped, it
/// just stops being handed frame callbacks.
///
/// `GdkToplevelState::SUSPENDED` is the state that *does* move there:
/// xdg-shell v6's "the compositor is not presenting this content", which is
/// exactly the question the poll wants answered. It landed in GTK 4.12 and the
/// devShell is 4.22, so reading it costs nothing beyond the `v4_14` feature
/// this crate already takes.
///
/// Deliberately **not** `is-active` (focus): a visible-but-unfocused window
/// must keep polling, since a live status header while you work in another
/// window is the entire point of this chrome.
#[must_use]
pub(crate) fn presenting(state: gdk::ToplevelState) -> bool {
    !state.contains(gdk::ToplevelState::SUSPENDED)
}

/// The poll's visibility source: `true` while `window` is mapped **and** the
/// compositor says it is presenting it.
///
/// Three signals feed one `watch`, because no single one of them covers every
/// case:
///
/// - `realize` is where the `GdkSurface` first exists, so it is the only place
///   the `GdkToplevel` can be reached and its `state` subscribed to. A window
///   can be unrealized and realized again (each time on a *new* surface),
///   which is why the subscription is made here rather than once at build.
/// - `notify::state` on that toplevel is the live signal — the suspend and
///   un-suspend edges [`presenting`] exists for.
/// - `map`/`unmap` stay, for the two edges the toplevel state cannot give: the
///   window's own first presentation, and its teardown. On niri the unmap edge
///   is only ever the latter (see [`presenting`]), at which point the poll
///   ends anyway; on a compositor that *can* hide a client's toplevel it is a
///   real park.
///
/// The state handler holds the window **weakly**: the surface is owned by the
/// widget, so a strong clone captured in a handler attached to that surface
/// would be a cycle outliving the window.
pub(crate) fn watch_presentation(
    window: &adw::ApplicationWindow,
) -> tokio::sync::watch::Receiver<bool> {
    // Not presenting until GTK says otherwise — a window is built and then
    // explicitly presented (`main.rs`), so `false` is the correct starting
    // snapshot for `feed::run` to read, not a guess.
    let (tx, rx) = tokio::sync::watch::channel(false);

    let sender = tx.clone();
    window.connect_realize(move |w| {
        let Some(toplevel) = w.surface().and_downcast::<gdk::Toplevel>() else {
            // Not a toplevel surface. No backend that ships here does this,
            // but the cast is fallible: the map/unmap edges below still drive
            // the poll, it simply stops parking on suspension.
            tracing::debug!("this window has no GdkToplevel — the poll cannot park on suspension");
            return;
        };
        let state_tx = sender.clone();
        let weak = w.downgrade();
        toplevel.connect_state_notify(move |t| {
            let mapped = weak
                .upgrade()
                .is_some_and(|w: adw::ApplicationWindow| w.is_mapped());
            let _ = state_tx.send(mapped && presenting(t.state()));
        });
        let _ = sender.send(w.is_mapped() && presenting(toplevel.state()));
    });

    let sender = tx.clone();
    window.connect_map(move |w| {
        let _ = sender.send(
            w.surface()
                .and_downcast::<gdk::Toplevel>()
                .is_none_or(|t| presenting(t.state())),
        );
    });
    window.connect_unmap(move |_| {
        let _ = tx.send(false);
    });

    rx
}

/// How a window turns its agent's URL into a trust decision — always
/// [`tls::resolve`] on a launch.
///
/// `Send + Sync` because the call is made on a worker thread (#1246): the
/// resolver is moved into the probe thread by value, so the window's copy has
/// to be shareable. It is a field rather than a hard-coded call for the reason
/// [`tls::resolve_route`] is a public seam — a display test drives the real
/// probe against a scripted gateway on a budget it can wait out, through the
/// same threading a launch uses. Nothing outside `cfg(test)` constructs one
/// that is not `tls::resolve` (see [`Window::assemble`]).
type TrustResolver = std::sync::Arc<dyn Fn(&str) -> tls::Resolved + Send + Sync>;

/// The longest the window waits between two attempts to reach a gateway that
/// is not answering — where [`Window::next_retry_delay`]'s doubling stops.
///
/// A minute: long enough that a hive down for the afternoon costs one refused
/// connect a minute (or one [`PROBE_DEADLINE`](crate::verify::PROBE_DEADLINE)
/// on a worker, for a blackholed one), short enough that the page is there
/// within a minute of the gateway coming back without anyone touching it.
const RETRY_CEILING: std::time::Duration = std::time::Duration::from_mins(1);

/// [`Window::next_retry_delay`]'s arithmetic, with nothing to own: `base`
/// (the poll interval) doubled `misses` times, stopped at [`RETRY_CEILING`] —
/// or at `base` itself when a configured poll interval is longer than that,
/// so a slow poll is never answered by a faster retry. Saturating throughout,
/// because a window left open on a dead hive counts misses for as long as it
/// likes.
fn retry_delay(base: std::time::Duration, misses: u32) -> std::time::Duration {
    let factor = 1_u32.checked_shl(misses).unwrap_or(u32::MAX);
    base.saturating_mul(factor).min(RETRY_CEILING.max(base))
}

/// What a retry does when it comes due — the two things a window asks a
/// gateway that did not answer again for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Attempt {
    /// Probe again through [`Window::load_page`]: no page is mounted yet,
    /// because the launch-time probe got no verdict (#1274's dead worker, or
    /// the October 2026 audit's M1, an unreachable gateway).
    Probe,
    /// Load this URI in the mounted view again: the page is mounted, and its
    /// own load got no answer — #1557 item 2, the routes with no probe
    /// (`PinLeaf`, the system store), where nothing used to ask again at all.
    /// The URI is the one that failed, which [`page::reload_after`] has
    /// already confirmed is on the hive's own origin.
    Reload(String),
}

/// A retry that is waiting: on its timer, or — when the timer came due while
/// the compositor was not presenting the window — parked until it is
/// (#1557 item 3).
#[derive(Debug)]
struct Queued {
    attempt: Attempt,
    /// The timer, while it runs. `None` once it has come due and the attempt
    /// is parked: the source is spent then, and holding its id would let
    /// `Window::cancel_retry` remove a source `GLib` has already destroyed
    /// (`SourceId::remove` panics on one).
    source: Option<glib::SourceId>,
}

/// How a waiting retry is waiting — what [`on_resume`] needs to know of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Waiting {
    /// Its timer is still running.
    Armed,
    /// Its timer came due while the window was not being presented.
    Parked,
}

/// What a presentation edge back does to the retry — #1557 item 3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnResume {
    /// Nothing is waiting, or the edge is a flap and the retry's own timer
    /// is still running: leave it be.
    Leave,
    /// Run the waiting attempt now.
    RunNow,
    /// The edge is a flap, and the attempt is parked with no timer at all:
    /// give it one again, due when the flap guard next lets an edge act.
    Rearm(std::time::Duration),
}

/// The retry's half of the poll's parking rule, with nothing to own.
///
/// `feed::run` parks its ticker while the compositor is not presenting the
/// window, and on the edge back polls **once**, at once, unless an
/// edge-driven poll already ran inside the last interval
/// ([`feed::resume_acts_now`], the flap guard). A waiting retry is treated
/// the same way, through the same predicate: an operator who switches back
/// to a window whose gateway was down is looking at it *now*, so that is
/// when to ask, not at the end of a backoff step that may be a minute long —
/// and a burst of workspace switches is still one attempt per interval, not
/// one per switch.
///
/// A flap leaves an armed timer alone (it is at most a backoff step away and
/// fires on its own), but a parked attempt has no timer left, so it is
/// re-armed for whatever remains of the interval since the last edge-driven
/// attempt — the moment the guard would have let this edge through.
fn on_resume(
    waiting: Option<Waiting>,
    since_last: Option<std::time::Duration>,
    interval: std::time::Duration,
) -> OnResume {
    let Some(waiting) = waiting else {
        return OnResume::Leave;
    };
    if feed::resume_acts_now(since_last, interval) {
        return OnResume::RunNow;
    }
    match waiting {
        Waiting::Armed => OnResume::Leave,
        Waiting::Parked => {
            OnResume::Rearm(interval.saturating_sub(since_last.unwrap_or_default()))
        }
    }
}

/// One agent's window.
pub struct Window {
    /// This window, weakly — the handle the probe's continuation upgrades
    /// through when it lands back on the main thread (#1246).
    ///
    /// Weak and not strong, on the `hytte-reactive` bind-pins convention
    /// (#224/#1244): a `spawn_future_local` that captured a strong self-clone
    /// instead would be one more thing keeping this window alive for the rest
    /// of the probe's budget after it was closed. **That consequence is not
    /// reachable today** (#1274 L2, measured): three other strong `Rc`
    /// cycles already keep every window alive for the process's whole life —
    /// `main.rs`'s `state: Rc<RefCell<Option<Rc<Window>>>>` caches the built
    /// window and is never cleared on close; [`Window::assemble_with_trust`]
    /// wires `self.header`/`self.settings` to strong self-clones of their
    /// own (`press`/`decide`, `window.rs:381`/`383`); and [`Window::build`]
    /// itself moves a third — `let pump = Rc::clone(&this)` — into a
    /// `spawn_future_local`, the exact shape this field exists to avoid.
    /// That future ends only when `out_rx` closes, which needs `feed::run`
    /// to return, which it does only once the GTK side is gone. So
    /// `me.upgrade()` can never answer `None` in production, and "a probe's
    /// answer for a window nobody is looking at" cannot happen yet:
    /// replacing this field with a strong self-clone in the
    /// `spawn_future_local` leaves the whole suite green, and
    /// `nix/lint-bind-pins.py` does not catch it either — its two rules key
    /// on `bind*`/`connect_*` call sites, and `spawn_future_local` is
    /// neither. Kept anyway: it is the correct shape for the day any of
    /// those three cycles is closed, and a probe crossing a worker thread
    /// should not become a fourth path that pins a window nothing else is
    /// keeping alive. `Rc::new_cyclic` is what fills it.
    me: std::rc::Weak<Self>,
    toplevel: adw::ApplicationWindow,
    stack: adw::ViewStack,
    header: ui::Header,
    settings: ui::Settings,
    banner: adw::Banner,
    /// Filled once, when the hive first hands over a URL for this agent — the
    /// row carries it (hyperhive#4073) and a fresh hive may not have one yet.
    page_slot: gtk::Box,
    page_loaded: RefCell<bool>,
    /// The URL a launch-time TLS probe is running for, while one is — #1246's
    /// **single in-flight** rule.
    ///
    /// An `Option`, not a queue and not a map, and for #963's reason on the
    /// consent window: there is exactly one page slot, so there is exactly one
    /// thing a probe's answer can be *for*. It is what stops a second probe
    /// from starting — and the second probe is not a hypothetical: the hive is
    /// polled every 2 s by default and [`Window::apply`] ends in
    /// [`Window::load_page`], so an 8 s probe would otherwise have four more
    /// started behind it, each opening its own connection to a gateway that is
    /// already not answering. A second activation (`--tab` on the running
    /// instance, `HANDLES_COMMAND_LINE`) is the same story through a different
    /// door.
    ///
    /// [`Window::page_loaded`] latches *after* the answer lands; between the
    /// two, this is the latch.
    probe: RefCell<Option<String>>,
    /// The retry [`Window::schedule_retry`] has queued, while one is — the
    /// **third** latch on [`Window::load_page`], beside `probe` and
    /// `page_loaded`.
    ///
    /// A probe that got no verdict (the gateway did not answer, or the worker
    /// died) leaves the page unlatched on purpose so that asking again can
    /// work (the October 2026 audit, M1). Unlatched alone, though, *every*
    /// poll would ask: [`Window::apply`] ends in `load_page` and the hive's
    /// status text moves every few seconds while an agent works, so the
    /// backoff would be whatever the agent's chattiness made it. While this
    /// holds a source, `load_page` waits for it — or for the card's button,
    /// which removes it ([`Window::retry_now`]). The id is taken out of here
    /// by the source itself when it fires, so a `remove` is only ever called
    /// on one that has not.
    ///
    /// Since #1557 it carries **what** to do as well as when ([`Attempt`]):
    /// the mounted page's own reload rides the same slot, the same backoff
    /// and the same "Try again now", and a retry that comes due while the
    /// compositor is not presenting the window stays here **parked** — still
    /// holding the latch, with no timer — until [`Window::presentation_changed`]
    /// sees the window again.
    retry: RefCell<Option<Queued>>,
    /// How many attempts in a row got no answer — what
    /// [`Window::next_retry_delay`] doubles on. Back to zero the moment one
    /// settles: a probe's verdict, or (#1557 item 2) the mounted page's
    /// load committing after a reload.
    unanswered: std::cell::Cell<u32>,
    /// Whether the compositor is presenting this window — the poll's own
    /// signal ([`watch_presentation`]), followed here for the retry
    /// (#1557 item 3). `true` until a source says otherwise, which is what a
    /// window built by [`Window::assemble`] with no source at all gets.
    presented: std::cell::Cell<bool>,
    /// When a presentation edge last ran a waiting retry — the flap guard's
    /// only state, exactly as `last_immediate` is `feed::run`'s. Only edges
    /// stamp it: a timer firing, or a click on "Try again now", is not a
    /// flap.
    last_resume: std::cell::Cell<Option<std::time::Instant>>,
    /// What the probe runs — see [`TrustResolver`].
    trust: TrustResolver,
    cfg: AgentsConfig,
    name: AgentName,
    urls: RefCell<Option<HiveUrls>>,
    last: RefCell<AgentState>,
    /// The agent's `Pending` queue as this window last read it — #1141 —
    /// narrowed to this agent and sorted oldest-first (`chrome::pending_for`),
    /// or the reason it cannot be read (`chrome::Queue`). What
    /// `Window::on_decision` checks a click against before it becomes a
    /// frame, and what `Window::apply` renders.
    ///
    /// Forgotten when the hive stops answering (`Update::State` carrying
    /// `AgentState::Unreachable`), not kept: the plugin's rule since #1509,
    /// and for its reason — rows with live buttons that nobody can vouch for
    /// are rows a click can send a bare id from, to a hive that may come back
    /// holding something else under it.
    queue: RefCell<Queue>,
    /// The hive's own reason the **last** decision for an approval was
    /// refused, keyed by id, with the request that decision was about.
    /// Pruned on every `Update::Approvals` to the entries whose id still
    /// names that same request — an approval that left the queue, or whose
    /// id the hive handed to another request, has nothing left to keep
    /// showing a reason for.
    approval_refusals: RefCell<BTreeMap<i64, (Shown, String)>>,
    /// Approvals this window has sent a decision for and not yet heard the
    /// end of — #1146's review, H1.
    ///
    /// `Approve`/`Deny` act immediately on the far side and there is nothing
    /// to flip optimistically, so the row survives until the *next* poll
    /// takes it out of `Pending` — a whole cadence (2 s by default) in which
    /// the buttons used to stay live. That is two frames for a double-click,
    /// and a `Deny` behind an `Approve` that already succeeded for an
    /// operator who saw no feedback and tried the other button. An id in here
    /// renders its row's buttons insensitive **and** is refused by
    /// [`Window::on_decision`], so a click already queued on the main loop
    /// cannot slip past the widget state.
    ///
    /// It empties two ways, both of them the end of the round trip: the
    /// request leaves the polled queue (pruned on `Update::Approvals`), or
    /// the hive refuses the write (`Update::Refused`) — which is precisely
    /// why the refusal path keeps the row, so a retry is possible.
    ///
    /// Each id carries the request the decision was sent for, so the prune
    /// is by request rather than by id: a re-numbered queue's new request
    /// under the same id does not inherit a latch it never earned, and the
    /// refusal that comes back for the old one has nothing to decorate.
    in_flight: RefCell<BTreeMap<i64, Shown>>,
    cmds: tokio::sync::mpsc::UnboundedSender<Request>,
}

impl Window {
    /// Build the window, start its `host.sock` client, and pump the client's
    /// updates onto the GTK main context.
    #[must_use]
    pub fn build(
        app: &adw::Application,
        name: &AgentName,
        runtime: &tokio::runtime::Handle,
    ) -> Rc<Self> {
        let cfg = hytte_plugin_agents::config::load();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

        let this = Self::assemble(app, name, cfg, cmd_tx);

        // The poll's only visibility source — mirrors
        // `hytte_plugin_agents::poll`'s `SlotVisible` gate, which this window
        // has no host to push. Every closure it installs holds its own clone
        // of the sender, so it stays alive for exactly as long as `toplevel`
        // does, and dropping the window is what ends `feed::run`'s parking
        // loop (`visible_open` latches `false` once they are all gone).
        let visible_rx = watch_presentation(&this.toplevel);
        // …and the retry parks on the very same signal (#1557 item 3): a
        // second receiver, not a second watch.
        this.follow_presentation(visible_rx.clone());

        runtime.spawn(feed::run(
            std::path::PathBuf::from(&this.cfg.socket),
            name.clone(),
            this.cfg.poll_interval(),
            cmd_rx,
            visible_rx,
            out_tx,
        ));

        // `tokio::sync::mpsc`'s `recv()` is executor-agnostic, so the GTK main
        // context can await it directly — no bridging channel, and the updates
        // land on the thread that owns the widgets by construction.
        let pump = Rc::clone(&this);
        glib::spawn_future_local(async move {
            while let Some(update) = out_rx.recv().await {
                pump.update(update);
            }
        });

        this
    }

    /// Everything [`Window::build`] does **except** the I/O: the widgets, the
    /// wiring and the first paint.
    ///
    /// The test seam. A display test drives this with a command lane it holds
    /// the other end of, so it can assert both what the window shows and what
    /// the buttons put on the wire, with no socket and no runtime.
    ///
    /// The trust decision is always [`tls::resolve`] here — the only spelling
    /// anything outside `cfg(test)` can produce, since
    /// `Window::assemble_with_trust` is private to this module.
    #[must_use]
    pub fn assemble(
        app: &adw::Application,
        name: &AgentName,
        cfg: AgentsConfig,
        cmds: tokio::sync::mpsc::UnboundedSender<Request>,
    ) -> Rc<Self> {
        Self::assemble_with_trust(app, name, cfg, cmds, std::sync::Arc::new(tls::resolve))
    }

    /// [`Window::assemble`] with the trust decision named.
    ///
    /// Private, and the reason [`TrustResolver`] is: a display test drives the
    /// real probe against a fixture gateway on a budget it can wait out, and
    /// through the same worker thread a launch uses, without needing to set a
    /// `TROLLSHELL_AGENT_WINDOW_*` variable — which this crate cannot do at
    /// all (`std::env::set_var` is `unsafe` in edition 2024 and the workspace
    /// `forbid`s `unsafe_code`, and it would be process-wide across a test
    /// binary that runs its GTK tests on one thread anyway).
    #[must_use]
    fn assemble_with_trust(
        app: &adw::Application,
        name: &AgentName,
        cfg: AgentsConfig,
        cmds: tokio::sync::mpsc::UnboundedSender<Request>,
        trust: TrustResolver,
    ) -> Rc<Self> {
        let header = ui::Header::new();
        let settings = ui::Settings::new();
        let banner = adw::Banner::new("");
        // The title is `ui::refusal` around the hive's own reason, which is
        // text, not markup — and `AdwBanner:use-markup` defaults to TRUE, so a
        // reason carrying a `<` or an `&` failed to parse and blanked the
        // banner (the October 2026 audit, LOW). `ui.rs`'s rows have said the
        // same thing with `.use_markup(false)` since #1130.
        banner.set_use_markup(false);
        banner.set_revealed(false);

        let page_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page_slot.set_hexpand(true);
        page_slot.set_vexpand(true);

        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(
            &page_slot,
            Some(cli::Tab::Agent.as_str()),
            "Agent",
            "utilities-terminal-symbolic",
        );
        stack.add_titled_with_icon(
            &settings.root,
            Some(cli::Tab::Settings.as_str()),
            "Settings",
            "emblem-system-symbolic",
        );

        let switcher = adw::ViewSwitcher::builder()
            .stack(&stack)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build();
        let bar = adw::HeaderBar::builder().title_widget(&switcher).build();

        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&bar);
        body.append(&header.root);
        body.append(&banner);
        body.append(&stack);

        let toplevel = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(960)
            .default_height(720)
            .title(format!("{} — agent", cfg.label_for(name.as_str())))
            .content(&body)
            .build();

        // `new_cyclic` only so the window can hold a `Weak` of itself: the
        // probe's continuation runs on the main context after the worker
        // answers, and the handle it upgrades through must not be what keeps
        // this window alive (see `Window::me`).
        let this = Rc::new_cyclic(|me| Self {
            me: me.clone(),
            toplevel,
            stack,
            header,
            settings,
            banner,
            page_slot,
            page_loaded: RefCell::new(false),
            probe: RefCell::new(None),
            retry: RefCell::new(None),
            unanswered: std::cell::Cell::new(0),
            presented: std::cell::Cell::new(true),
            last_resume: std::cell::Cell::new(None),
            trust,
            cfg,
            name: name.clone(),
            urls: RefCell::new(None),
            last: RefCell::new(AgentState::Connecting),
            queue: RefCell::new(Queue::default()),
            approval_refusals: RefCell::new(BTreeMap::new()),
            in_flight: RefCell::new(BTreeMap::new()),
            cmds,
        });

        let press = Rc::clone(&this);
        this.header.connect(move |p| press.on_press(p));
        let decide = Rc::clone(&this);
        this.settings
            .connect_decision(move |d| decide.on_decision(d));
        this.apply();
        this
    }

    /// Bring this window forward.
    pub fn present(&self) {
        self.toplevel.present();
    }

    /// Switch to one tab. Silently a no-op if the stack has no such child,
    /// which cannot happen — `Tab` and the child names are one enum.
    pub fn show_tab(&self, tab: cli::Tab) {
        self.stack.set_visible_child_name(tab.as_str());
    }

    /// Which tab is showing.
    #[must_use]
    pub fn visible_tab(&self) -> Option<String> {
        self.stack.visible_child_name().map(|s| s.to_string())
    }

    fn on_press(&self, press: ui::Press) {
        let req = match press {
            ui::Press::Start => feed::start(&self.name),
            ui::Press::Stop => feed::stop(&self.name),
            ui::Press::SetPaused(paused) => feed::set_paused(&self.name, paused),
        };
        self.banner.set_revealed(false);
        if self.cmds.send(req).is_err() {
            tracing::warn!("the hive client is gone; this window is no longer live");
        }
    }

    /// Route one Approve/Deny press (#1141, spec §6.5).
    ///
    /// Three guards now, all from the spec's own rule for the sidebar's
    /// identical decision path (`hytte_plugin_agents::plugin::Agents::decide`):
    ///
    /// 1. the row queued under the id must still be **the request the row
    ///    showed** ([`chrome::should_send`], bound to the plugin's own
    ///    `Shown` the way #1509 binds the consent card). Three ways to fail,
    ///    and they are not one outcome:
    ///    - nothing is queued under it any more — it raced a poll and was
    ///      resolved elsewhere — so it is dropped with a debug line, not sent;
    ///    - a **different** request is: the hive re-numbered its queue, and
    ///      an `Approve { id }` would decide something the operator never
    ///      saw. Dropped unsent, and the view repainted from the queue as it
    ///      now is;
    ///    - the queue cannot be read (the hive is unreachable or refused it):
    ///      not sent, and the banner says so ([`chrome::not_sent`]), because
    ///      the operator pressed a button and silence would read as success;
    /// 2. **no decision for it may already be in flight** (#1146's review,
    ///    H1). Unlike a lifecycle press there is nothing to flip
    ///    optimistically — `Approve`/`Deny` run immediately on the far side
    ///    and the row waits for the next poll — so without this the row sat
    ///    for a whole cadence with two live buttons and would happily send a
    ///    second frame, including a `Deny` behind an `Approve` that had
    ///    already succeeded. The row's buttons go insensitive for the same
    ///    reason; this is the authoritative half, because a click can already
    ///    be queued on the main loop when they do.
    /// 3. …and a retry starts clean: the previous attempt's refusal reason is
    ///    dropped before the send (#1146's review, L1), so a row cannot read
    ///    `couldn't answer: agent busy` about a decision that is on the wire
    ///    right now.
    fn on_decision(&self, decision: Decision) {
        let id = decision.id;
        let verdict = chrome::should_send(&self.queue.borrow(), &decision);
        match verdict {
            Verdict::Send => {}
            Verdict::Gone => {
                tracing::debug!(
                    approval = id,
                    "the approval left the queue before this window could send a decision; dropping it"
                );
                return;
            }
            Verdict::Replaced => {
                tracing::debug!(
                    approval = id,
                    "the queue holds a different request under this id than the row showed; \
                     dropping the decision unsent"
                );
                self.apply();
                return;
            }
            Verdict::Unread => {
                tracing::info!(
                    approval = id,
                    "a decision arrived while the approval queue is unreadable; not sent"
                );
                self.banner.set_title(&chrome::not_sent(&decision));
                self.banner.set_revealed(true);
                return;
            }
        }
        if self.in_flight.borrow().contains_key(&id) {
            tracing::debug!(
                approval = id,
                "a decision for this approval is already in flight; dropping the second one"
            );
            return;
        }
        let req = decision.request();
        self.in_flight.borrow_mut().insert(id, decision.shown);
        self.approval_refusals.borrow_mut().remove(&id);
        // A decision that does go out retires an earlier "not sent" banner,
        // the way a lifecycle press retires its own (`on_press`).
        self.banner.set_revealed(false);
        if self.cmds.send(req).is_err() {
            tracing::warn!("the hive client is gone; this window is no longer live");
        }
        // Repaint so the row the operator just answered stops looking
        // answerable.
        self.apply();
    }

    /// Fold one update from the `host.sock` client into the chrome.
    pub fn update(&self, update: Update) {
        match update {
            Update::State(state) => {
                // No status answer, so no queue either — the plugin's rule
                // since #1509. `feed::poll_once` re-sends the queue on the
                // first poll that answers again, even an unchanged one, so
                // forgetting it here cannot strand the rows.
                if matches!(state, AgentState::Unreachable { .. }) {
                    self.forget_queue(Queue::Unreachable);
                }
                *self.last.borrow_mut() = state;
                self.apply();
            }
            Update::Urls(urls) => {
                *self.urls.borrow_mut() = Some(*urls);
                self.apply();
            }
            // The hive answered the queue: rebuild the rows from it.
            Update::Approvals(Ok(queue)) => {
                let queue = Queue::Read(chrome::pending_for(&self.name, queue));
                // Prune refusal reasons **and in-flight ids** to the requests
                // the fresh queue still carries under the same id — a row
                // that left the queue has nothing left to explain and nothing
                // left to wait on, and neither has one whose id now names a
                // different request. That prune is also how a decision's
                // latch is released: the poll that takes the approval out of
                // `Pending` is the end of the round trip (#1146's review,
                // H1/L2).
                self.approval_refusals
                    .borrow_mut()
                    .retain(|id, (shown, _)| queue.holds(*id, shown));
                self.in_flight
                    .borrow_mut()
                    .retain(|id, shown| queue.holds(*id, shown));
                *self.queue.borrow_mut() = queue;
                self.apply();
            }
            // The hive refused the queue (#1146's review, M1). The rows go —
            // this window can no longer vouch for them — but the group stays,
            // saying so, instead of hiding and looking like an empty queue.
            Update::Approvals(Err(reason)) => {
                self.forget_queue(Queue::Refused(reason));
                self.apply();
            }
            Update::Refused { request, reason } => match &request {
                // #1141: an Approve/Deny refusal is shown **inline**, on the
                // row it names — a global banner would say "the hive refused
                // that" beside a row that still shows live buttons, which
                // does not say *which* decision failed once more than one is
                // queued.
                Request::Approve { id } | Request::Deny { id } => {
                    // The round trip ended in a no, so the row is answerable
                    // again — that is the whole reason a refusal keeps it
                    // (#1146's review, H1).
                    //
                    // Only record the reason on the row the decision was
                    // **for**, which is the one still in flight under this
                    // id: `in_flight` is pruned to the requests still queued
                    // under their ids on every `Update::Approvals` and
                    // cleared whenever the queue is forgotten, so an entry
                    // here *is* the current row. A refusal that arrives after
                    // that row left the queue (the poll pruned it first), was
                    // replaced by another request under the id, or was
                    // forgotten with an unreachable hive finds no entry and
                    // has nothing left to decorate — and inserting anyway
                    // would plant an orphan that a later poll bringing the id
                    // back would read as "the decision just sent for you
                    // failed" (#1146 re-verify, M-NEW-1), or print request
                    // A's refusal on request B.
                    if let Some(shown) = self.in_flight.borrow_mut().remove(id) {
                        self.approval_refusals
                            .borrow_mut()
                            .insert(*id, (shown, reason));
                    }
                    self.apply();
                }
                _ => {
                    self.banner.set_title(&ui::refusal(&request, &reason));
                    self.banner.set_revealed(true);
                }
            },
        }
    }

    /// Drop the approval queue this window can no longer vouch for, and
    /// everything it was tracking about decisions on it, leaving `why` in its
    /// place for the group to say.
    fn forget_queue(&self, why: Queue) {
        self.approval_refusals.borrow_mut().clear();
        self.in_flight.borrow_mut().clear();
        *self.queue.borrow_mut() = why;
    }

    /// Push the current state through the whole chrome.
    fn apply(&self) {
        let state = self.last.borrow();
        self.header.apply(
            &HeaderModel::of(&self.name, &self.cfg, &state),
            &Controls::of(&state),
        );
        let refusals = self.approval_refusals.borrow();
        let in_flight = self.in_flight.borrow();
        let queue = self.queue.borrow();
        let rows: Vec<chrome::ApprovalRow> = queue
            .rows()
            .iter()
            .map(|a| {
                chrome::ApprovalRow::of(
                    a,
                    refusals.get(&a.id).map(|(_, reason)| reason.clone()),
                    in_flight.contains_key(&a.id),
                )
            })
            .collect();
        drop(in_flight);
        drop(refusals);
        let notice = queue.notice();
        drop(queue);
        self.settings.apply(
            &Facts::agent(&self.name, &state),
            &Facts::hive(&self.cfg, self.urls.borrow().as_ref()),
            &rows,
            notice.as_ref(),
        );
        self.badge_settings(rows.len());
        self.load_page(&state);
    }

    /// Badge the Settings tab with how many approvals are queued (#1146's
    /// review, M5).
    ///
    /// The window opens on the **Agent** tab unless launched with
    /// `--tab settings`, the queue renders only on Settings, and the group
    /// hides itself when empty — so an operator watching the turn stream (the
    /// issue's own framing) had no way to learn an approval had queued. The
    /// sidebar's answer to exactly this is a badge; `adw::ViewSwitcher`
    /// already renders one for a `ViewStackPage`, so this is the same answer
    /// with no new widget. `set_needs_attention` is what makes it visible
    /// while the *other* tab is showing, which is the case that motivated it.
    fn badge_settings(&self, queued: usize) {
        let page = self.stack.page(&self.settings.root);
        page.set_badge_number(u32::try_from(queued).unwrap_or(u32::MAX));
        page.set_needs_attention(queued > 0);
    }

    /// Mount the embedded page the first time the hive gives this agent a URL.
    ///
    /// **Once only**: a reload on every poll would throw away the scroll
    /// position and any half-typed message on the page, twice a second. That
    /// latch is #1130's M13 and is now pinned by
    /// `gtk_tests::the_page_is_built_once_and_not_on_every_poll`.
    ///
    /// Since #1246 the latch is two things, because mounting the page is two
    /// steps with a worker thread between them: [`Window::probe`] holds the
    /// window from the moment a probe starts, [`Window::page_loaded`] from the
    /// moment its answer lands. Either one means "do not start another".
    ///
    /// Since the October 2026 audit's M1 there is a third, [`Window::retry`],
    /// and `page_loaded` no longer latches on every answer: a probe that could
    /// not reach the gateway mounts nothing ([`Window::finish_probe`]), so the
    /// page stays open to a later attempt, and `retry` is what makes that
    /// attempt wait for its backoff step instead of the next poll.
    fn load_page(&self, state: &AgentState) {
        if *self.page_loaded.borrow()
            || self.probe.borrow().is_some()
            || self.retry.borrow().is_some()
        {
            return;
        }
        let Some(url) = state.agent().and_then(agent_url) else {
            if self.page_slot.first_child().is_none() {
                let hint = gtk::Label::builder()
                    .label(NO_PAGE)
                    .wrap(true)
                    .justify(gtk::Justification::Center)
                    .margin_top(48)
                    .margin_start(24)
                    .margin_end(24)
                    .valign(gtk::Align::Start)
                    .build();
                hint.add_css_class("dim-label");
                self.page_slot.append(&hint);
            }
            return;
        };

        self.begin_probe(page::embed_url(url));
    }

    /// Retry [`Window::load_page`] once, against whatever state is current —
    /// #1274 item 1's other retry route.
    ///
    /// [`Window::schedule_retry`] queues a call to this after a probe that
    /// produced no verdict: a dead worker ([`Window::begin_probe`]'s `Err`
    /// arm, #1274) or, since the October 2026 audit's M1, a gateway that did
    /// not answer ([`Window::finish_probe`]). Neither latches the page, but
    /// an unlatched page only buys a retry if something calls `load_page`
    /// again, and on a steady hive nothing else does: `feed::poll_once`
    /// dedups an unchanged row's `Update::State` (`feed.rs:552`), and a
    /// second activation's route (`main.rs`) never reaches `load_page` at
    /// all. Reads `self.last` rather than the URL the last probe was given,
    /// so a row that changed in the meantime is retried against its
    /// *current* state, not a stale one.
    fn retry_load_page(&self) {
        let state = self.last.borrow();
        self.load_page(&state);
    }

    /// How long to wait before asking a gateway that did not answer again,
    /// counting this miss: the poll interval, doubled per consecutive miss,
    /// up to [`RETRY_CEILING`] (or the poll interval itself, if that is
    /// configured longer).
    ///
    /// Starting at the poll interval keeps the first retry on the cadence the
    /// header already moves at — a gateway that was restarting is back on the
    /// next step — and the doubling is what stops a gateway that stays down
    /// from being dialled every two seconds for as long as the window is open.
    /// There is deliberately no *count* bound: a count would bring M1's latch
    /// back after the last attempt, and a gateway that comes back after an
    /// hour is still one the operator wants the page from.
    fn next_retry_delay(&self) -> std::time::Duration {
        let misses = self.unanswered.get();
        self.unanswered.set(misses.saturating_add(1));
        retry_delay(self.cfg.poll_interval(), misses)
    }

    /// Run `attempt` after `wait`, and hold [`Window::load_page`] shut until
    /// then.
    ///
    /// The one retry path every no-answer arm takes — #1274's dead worker,
    /// M1's unreachable gateway, and since #1557 item 2 the mounted page's
    /// own load — so they share the backoff and the latch rather than each
    /// carrying a timer of its own. The source holds the window weakly, for
    /// [`Window::me`]'s reason.
    fn schedule_retry(&self, wait: std::time::Duration, attempt: Attempt) {
        self.cancel_retry();
        let me = std::rc::Weak::clone(&self.me);
        let source = glib::timeout_add_local_once(wait, move || {
            if let Some(window) = me.upgrade() {
                window.retry_due();
            }
        });
        *self.retry.borrow_mut() = Some(Queued {
            attempt,
            source: Some(source),
        });
    }

    /// The retry's timer fired: run its attempt — **unless the compositor is
    /// not presenting this window**, in which case it parks (#1557 item 3).
    ///
    /// Parked, the attempt stays in [`Window::retry`] with no timer, so it
    /// keeps `load_page` latched exactly as a queued one does, and
    /// [`Window::presentation_changed`] runs it on the edge back. That is
    /// `feed::run`'s rule for the poll, applied to the retry: a window nobody
    /// is being shown spends nothing — at the ceiling a down gateway used to
    /// cost one TLS connect a minute for as long as the window sat on a
    /// workspace nobody looked at.
    fn retry_due(&self) {
        // This source is spent the moment it fires, so its id is dropped
        // here rather than removed — see `Queued::source`.
        let queued = self.retry.borrow_mut().take();
        let Some(Queued { attempt, .. }) = queued else {
            return;
        };
        if self.presented.get() {
            self.run_attempt(attempt);
        } else {
            tracing::debug!(
                ?attempt,
                "a retry came due while the compositor is not presenting this window; parked \
                 until it is"
            );
            *self.retry.borrow_mut() = Some(Queued {
                attempt,
                source: None,
            });
        }
    }

    /// Do what a retry was waiting to do.
    fn run_attempt(&self, attempt: Attempt) {
        match attempt {
            Attempt::Probe => self.retry_load_page(),
            Attempt::Reload(uri) => self.reload_page(&uri),
        }
    }

    /// Take the waiting retry out of its slot, removing its timer if one is
    /// running, and hand back what it would have done.
    fn take_retry(&self) -> Option<Attempt> {
        let pending = self.retry.borrow_mut().take()?;
        if let Some(source) = pending.source {
            source.remove();
        }
        Some(pending.attempt)
    }

    /// Drop a waiting retry, if there is one — a running timer is removed, so
    /// it never fires, and a parked attempt is forgotten.
    fn cancel_retry(&self) {
        drop(self.take_retry());
    }

    /// The cards' "Try again now": skip the rest of the backoff step and ask
    /// the gateway at once.
    ///
    /// The backoff itself is left where it is — a click is an operator saying
    /// "I just fixed it", and if they were wrong the next wait is the one the
    /// misses so far have earned, not the first step again. With nothing
    /// waiting (the last attempt is still in flight) it falls back to
    /// [`Window::retry_load_page`], which every latch already makes harmless.
    fn retry_now(&self) {
        match self.take_retry() {
            Some(attempt) => self.run_attempt(attempt),
            None => self.retry_load_page(),
        }
    }

    /// Follow the compositor's presentation of this window — the poll's own
    /// [`watch_presentation`] signal, through a second receiver — so the
    /// retry parks while nobody is shown the window (#1557 item 3).
    ///
    /// `tokio::sync::watch` is executor-agnostic, the reason `Window::build`
    /// can already await `tokio` channels on the main context. The loop ends
    /// when the senders do — they live in the toplevel's own signal handlers
    /// — or when the window is gone.
    fn follow_presentation(&self, mut visible: tokio::sync::watch::Receiver<bool>) {
        self.presented.set(*visible.borrow_and_update());
        let me = std::rc::Weak::clone(&self.me);
        glib::spawn_future_local(async move {
            while visible.changed().await.is_ok() {
                let now = *visible.borrow_and_update();
                let Some(window) = me.upgrade() else {
                    break;
                };
                window.presentation_changed(now);
            }
        });
    }

    /// The compositor started or stopped presenting this window.
    ///
    /// Stopping changes nothing at once: a timer still running is left to
    /// run, and [`Window::retry_due`] parks it if it comes due before the
    /// window is shown again. Starting is the edge [`on_resume`] decides — a
    /// waiting retry runs now, at most once per poll interval, by the flap
    /// guard the poll itself uses.
    fn presentation_changed(&self, now: bool) {
        let was = self.presented.replace(now);
        if !now || was {
            return;
        }
        let waiting = self.retry.borrow().as_ref().map(|q| {
            if q.source.is_some() {
                Waiting::Armed
            } else {
                Waiting::Parked
            }
        });
        let since_last = self.last_resume.get().map(|at| at.elapsed());
        match on_resume(waiting, since_last, self.cfg.poll_interval()) {
            OnResume::Leave => {}
            OnResume::RunNow => {
                self.last_resume.set(Some(std::time::Instant::now()));
                if let Some(attempt) = self.take_retry() {
                    tracing::debug!(?attempt, "shown again: running the waiting retry now");
                    self.run_attempt(attempt);
                }
            }
            OnResume::Rearm(wait) => {
                if let Some(attempt) = self.take_retry() {
                    self.schedule_retry(wait, attempt);
                }
            }
        }
    }

    /// What the mounted page reports about its own loads — #1557 item 2.
    ///
    /// [`webview::Gateway::Unanswered`] is a load of the hive's own page that
    /// nothing answered (the gateway refused the connection, did not resolve,
    /// timed out): the page's stack puts up the same card M1's probe does,
    /// and a reload of the failed URI is queued on the same backoff. Before
    /// this, on the routes with no probe (`PinLeaf`, the system store),
    /// `WebKit`'s error page stayed up and nothing ever reloaded it — the
    /// gateway coming back changed nothing until the window was closed.
    ///
    /// [`webview::Gateway::Answered`] is a load committing while that card
    /// was up: the hive is back, so the backoff starts over and anything
    /// still waiting is dropped — a reload queued behind a page that already
    /// came back would throw away whatever the operator has typed into it.
    fn on_gateway(&self, event: webview::Gateway) {
        match event {
            webview::Gateway::Unanswered { uri, why } => {
                let wait = self.next_retry_delay();
                tracing::warn!(
                    %uri,
                    %why,
                    retry_in = ?wait,
                    "nothing answered the agent's page; reloading it on the backoff"
                );
                if let Some(page) = self.page_slot.first_child() {
                    let me = std::rc::Weak::clone(&self.me);
                    let tried = format!("asked {uri} for the agent's page, and nothing answered ({why})");
                    webview::show_unanswered(
                        &page,
                        &webview::unreachable(Some(&tried), wait, move || {
                            if let Some(window) = me.upgrade() {
                                window.retry_now();
                            }
                        }),
                    );
                }
                self.schedule_retry(wait, Attempt::Reload(uri));
            }
            webview::Gateway::Answered => {
                self.unanswered.set(0);
                self.cancel_retry();
            }
        }
    }

    /// Load `uri` in the mounted page again — [`Attempt::Reload`].
    fn reload_page(&self, uri: &str) {
        if let Some(page) = self.page_slot.first_child() {
            webview::reload(&page, uri);
        }
    }

    /// Paint the verifying state, then check the hive's certificate **on a
    /// worker thread** — #1246.
    ///
    /// # Who runs where
    ///
    /// Everything here except the [`tls::resolve`] call is on the GTK main
    /// thread. The resolve is the part that blocks: #1234 reads the hive's own
    /// TLS material and, on the bundle route, opens a TLS connection to the
    /// gateway to verify the chain it presents before pinning the leaf.
    /// #1242's [`PROBE_DEADLINE`](crate::verify::PROBE_DEADLINE) made that a
    /// real wall-clock bound (8 s, DNS included) and said so in those words —
    /// *it bounds the freeze, it does not remove it*. This is the removal: the
    /// window paints the verifying state and returns to the main loop
    /// immediately, and the answer crosses back over a `tokio::sync::oneshot`
    /// that `glib::spawn_future_local` awaits on the main context. Nothing the
    /// worker touches is a widget, and nothing the continuation touches is a
    /// socket.
    ///
    /// A `oneshot` and a plain `std::thread` rather than [`gio::spawn_blocking`]:
    /// `tokio::sync`'s channels are executor-agnostic, which is already how
    /// this window gets its `host.sock` updates onto the main context
    /// ([`Window::build`]), so the crossing costs no new dependency and no new
    /// idiom — while `gio`'s task pool is shared with GIO's own async I/O
    /// (including the threaded resolver this probe calls into) and is
    /// documented as rate-limiting what it is handed, which is not where a
    /// call that may hold a thread for the whole budget belongs.
    ///
    /// [`gio::spawn_blocking`]: gtk::gio::spawn_blocking
    ///
    /// # The deadline still bounds it
    ///
    /// It moves with the probe rather than being replaced by it: the watchdog
    /// is armed inside [`crate::verify::probe`], so it now cancels a worker
    /// instead of the main thread. On expiry the card says the probe was
    /// cancelled and names every route, exactly as before — what changed is
    /// that the window was usable the whole time it ran.
    fn begin_probe(&self, embedded: String) {
        let host = tls::host_of(&embedded).unwrap_or_else(|| "the hive".to_owned());
        self.fill_page_slot(&webview::verifying(&host));
        *self.probe.borrow_mut() = Some(embedded.clone());

        let (answer, wait) = tokio::sync::oneshot::channel();
        let resolver = std::sync::Arc::clone(&self.trust);
        let asked = embedded.clone();
        let worker = std::thread::Builder::new()
            .name("agent-window-tls-probe".to_owned())
            .spawn(move || {
                // The receiver is gone if the window closed while this ran —
                // there is nobody to tell, which is the whole of the cleanup.
                drop(answer.send(resolver(&asked)));
            });

        match worker {
            Ok(_detached) => {
                let me = std::rc::Weak::clone(&self.me);
                glib::spawn_future_local(async move {
                    // The upgrade has to happen after the await regardless of
                    // the verdict, because the `Err` arm below needs `window`
                    // too — #1274 L1. (Nothing pins the window across the
                    // `.await` either way, before or after this reorder: `me`
                    // was already a `Weak`, which is #1274 L2's own point,
                    // moot by construction rather than by this ordering.)
                    let answer = wait.await;
                    let Some(window) = me.upgrade() else {
                        // The window closed while this ran — there is nobody
                        // left to show a verdict to, and nothing left to
                        // clear.
                        return;
                    };
                    let Ok(trust) = answer else {
                        // The worker panicked, so no verdict exists —
                        // `tls::resolve` is total, so this is unreachable
                        // short of an abort. The old shape returned here
                        // without touching `probe` or the page slot, which
                        // left `load_page`'s early return
                        // (`*page_loaded || probe.is_some()`) latched
                        // forever: spinner up, no retry, for the life of the
                        // process (#1274 L1). This releases the latch,
                        // replaces the spinner with a card that says what
                        // happened, and schedules the one retry that is
                        // real: `feed::poll_once` dedups an unchanged row's
                        // `Update::State` (`feed.rs:552`, pinned by
                        // `an_unchanged_hive_sends_one_state_not_one_per_poll`),
                        // and a second activation's route (`main.rs`) never
                        // reaches `load_page` at all — so nothing else would
                        // ever call it again on a steady hive. Since the
                        // October 2026 audit's M1 that retry is
                        // `schedule_retry`'s, shared with the unreachable
                        // gateway's: the same backoff, and the same latch
                        // keeping a poll from jumping it.
                        tracing::error!(
                            "the TLS probe thread died without a verdict; the page was not loaded"
                        );
                        window.probe.borrow_mut().take();
                        window.fill_page_slot(&webview::probe_died(&host));
                        window.schedule_retry(window.next_retry_delay(), Attempt::Probe);
                        return;
                    };
                    window.finish_probe(&embedded, &trust);
                });
            }
            Err(e) => {
                // No thread to be had. The old shape — resolve here, on this
                // thread — is strictly better than never loading the page, so
                // it is what a machine out of threads gets, with the freeze
                // said out loud.
                tracing::warn!(
                    error = %e,
                    "no thread for the TLS probe; checking the hive's certificate on the GTK main \
                     thread instead, which blocks the window for up to the probe's deadline"
                );
                let trust = (self.trust)(&embedded);
                self.finish_probe(&embedded, &trust);
            }
        }
    }

    /// The probe's answer, back on the GTK main thread: mount the page under
    /// the policy it settled on and close the in-flight slot — **unless the
    /// probe never reached the gateway**.
    ///
    /// That answer ([`tls::Resolved::unreachable`]) used to mount the page
    /// too, under the system store, and latch `page_loaded` with it. The
    /// October 2026 audit (M1) is what that cost: with the gateway down at
    /// open, `WebKit`'s load failed on the connection into its own error page,
    /// and when the gateway came back nothing asked again — the view had no
    /// pin, `load_page` returned early for good, and only closing the window
    /// got the page. So that answer now puts up [`webview::unreachable`],
    /// leaves `page_loaded` alone and [schedules](Window::schedule_retry) the
    /// next attempt; every other answer is a verdict, mounts the page as
    /// before, and resets the backoff.
    fn finish_probe(&self, embedded: &str, trust: &tls::Resolved) {
        self.probe.borrow_mut().take();
        if trust.unreachable {
            let wait = self.next_retry_delay();
            tracing::warn!(
                url = %embedded,
                tls_tried = trust.tried.as_deref().unwrap_or("nothing"),
                retry_in = ?wait,
                "the hive's gateway did not answer the certificate check; not loading the agent's \
                 page yet, and asking again"
            );
            let me = std::rc::Weak::clone(&self.me);
            self.fill_page_slot(&webview::unreachable(
                trust.tried.as_deref(),
                wait,
                move || {
                    if let Some(window) = me.upgrade() {
                        window.retry_now();
                    }
                },
            ));
            self.schedule_retry(wait, Attempt::Probe);
            return;
        }
        self.unanswered.set(0);
        tracing::info!(
            url = %embedded,
            tls_host = ?trust.policy.scoped_host(),
            tls_tried = trust.tried.as_deref().unwrap_or("nothing — the system trust store"),
            "loading the agent's page"
        );
        let me = std::rc::Weak::clone(&self.me);
        self.fill_page_slot(&webview::page(embedded, trust, move |event| {
            if let Some(window) = me.upgrade() {
                window.on_gateway(event);
            }
        }));
        *self.page_loaded.borrow_mut() = true;
    }

    /// Put `child` in the page slot, replacing whatever was there.
    ///
    /// One function because the slot now holds three different things over a
    /// launch — the no-page hint, the verifying state, the page — and each
    /// swap has to remove the last one or the box stacks them.
    fn fill_page_slot(&self, child: &gtk::Widget) {
        while let Some(old) = self.page_slot.first_child() {
            self.page_slot.remove(&old);
        }
        self.page_slot.append(child);
    }

    /// Whether the banner is showing, and what it says — the display tests'
    /// read-back.
    #[cfg(all(test, feature = "system-tests"))]
    fn banner_text(&self) -> Option<String> {
        self.banner
            .is_revealed()
            .then(|| self.banner.title().to_string())
    }

    /// The widget currently filling the page slot.
    #[cfg(all(test, feature = "system-tests"))]
    fn page_child(&self) -> Option<gtk::Widget> {
        self.page_slot.first_child()
    }

    /// Whether a launch-time TLS probe is in flight — the display tests' way
    /// to know the worker has not answered yet (#1246).
    #[cfg(all(test, feature = "system-tests"))]
    fn probing(&self) -> bool {
        self.probe.borrow().is_some()
    }

    /// Whether a retry is queued — the display tests' way to see M1's third
    /// latch without waiting the backoff out.
    #[cfg(all(test, feature = "system-tests"))]
    fn retrying(&self) -> bool {
        self.retry.borrow().is_some()
    }

    /// What the waiting retry would do, and whether it is parked (its timer
    /// came due while the window was hidden) — #1557's read-back.
    #[cfg(all(test, feature = "system-tests"))]
    fn waiting(&self) -> Option<(Attempt, bool)> {
        self.retry
            .borrow()
            .as_ref()
            .map(|q| (q.attempt.clone(), q.source.is_none()))
    }

    /// The banner's own title label, as GTK renders it — not the `title`
    /// property, which reads back whatever was set whether it parsed or not.
    #[cfg(all(test, feature = "system-tests"))]
    fn banner_rendered(&self) -> Vec<String> {
        fn labels(widget: &gtk::Widget, out: &mut Vec<String>) {
            if let Some(label) = widget.downcast_ref::<gtk::Label>() {
                out.push(label.text().to_string());
            }
            let mut child = widget.first_child();
            while let Some(c) = child {
                labels(&c, out);
                child = c.next_sibling();
            }
        }
        let mut out = Vec::new();
        labels(self.banner.upcast_ref(), &mut out);
        out
    }

    /// The header, for the tests that assert what it shows.
    #[cfg(all(test, feature = "system-tests"))]
    fn header(&self) -> &ui::Header {
        &self.header
    }

    /// The window itself, for the test that drives the presentation watch.
    #[cfg(all(test, feature = "system-tests"))]
    fn toplevel(&self) -> &adw::ApplicationWindow {
        &self.toplevel
    }
}

/// [`presenting`]'s policy, with no display and no compositor: a `ToplevelState`
/// is a bitflags value, so the decision it drives can be pinned hermetically
/// even though nothing in CI can make a real compositor set the bit (see
/// [`gtk_tests::the_presentation_watch_follows_map_and_unmap`] for how far the
/// wiring itself is pinned, and `docs/live-verify.md` for the rest).
#[cfg(test)]
mod tests {
    use super::{OnResume, RETRY_CEILING, Waiting, on_resume, presenting, retry_delay};
    use gtk::gdk::ToplevelState;
    use std::time::Duration;

    /// **Shown again, a waiting retry runs — once per interval** — #1557
    /// item 3, the decision with no window and no clock.
    ///
    /// The poll's rule (`feed::run`'s "Parking"), applied to the retry
    /// through the same `feed::resume_acts_now`: the first edge back acts,
    /// whether the retry's timer is still running or it came due while the
    /// window was hidden and parked; an edge inside one interval of the last
    /// edge-driven attempt is a flap, which leaves a running timer alone and
    /// gives a parked attempt a timer for the rest of that interval; and an
    /// edge with nothing waiting does nothing at all.
    ///
    /// Mutations (run this round, red): answer `RunNow` without asking
    /// `resume_acts_now` (no flap guard) and the armed-flap row reds; answer
    /// `Leave` for a parked flap (no re-arm) and the parked-flap row reds
    /// — that attempt would have no timer and no edge to run it, which is
    /// the M1 latch again.
    #[test]
    fn a_waiting_retry_runs_when_shown_again_once_per_interval() {
        let interval = Duration::from_secs(2);
        for waiting in [Waiting::Armed, Waiting::Parked] {
            assert_eq!(
                on_resume(Some(waiting), None, interval),
                OnResume::RunNow,
                "{waiting:?}: the first edge back asks at once"
            );
            assert_eq!(
                on_resume(Some(waiting), Some(interval), interval),
                OnResume::RunNow,
                "{waiting:?}: a whole interval later is a genuine return"
            );
            assert_eq!(
                on_resume(None, Some(Duration::ZERO), interval),
                OnResume::Leave,
                "nothing waiting, nothing to run"
            );
        }
        assert_eq!(
            on_resume(Some(Waiting::Armed), Some(Duration::from_millis(500)), interval),
            OnResume::Leave,
            "a flap leaves a running timer to fire on its own"
        );
        assert_eq!(
            on_resume(Some(Waiting::Parked), Some(Duration::from_millis(500)), interval),
            OnResume::Rearm(Duration::from_millis(1500)),
            "a flap gives a parked attempt a timer for the rest of the interval — not nothing"
        );
    }

    /// **The retry backs off, and stops backing off at the ceiling** — the
    /// October 2026 audit's M1, the half of it that needs no display.
    ///
    /// Starts on the poll interval (a gateway that was mid-restart is back on
    /// the header's own cadence), doubles per miss, and never waits past
    /// [`RETRY_CEILING`] — unless the poll itself is configured slower, which
    /// the retry then matches rather than outrunning. The last default row is
    /// a window left open on a dead hive for a very long time: saturating, not
    /// overflowing.
    ///
    /// Mutations (run this round, red): drop the `.min(…)` and the five-miss
    /// row reds at 64 s; drop the shift (always `base`) and the one-miss row
    /// reds at 2 s; drop the `.max(base)` and the hour-long poll's rows red at
    /// 60 s.
    #[test]
    fn the_retry_doubles_from_the_poll_interval_up_to_the_ceiling() {
        let two = Duration::from_secs(2);
        for (misses, expected) in [
            (0, two),
            (1, Duration::from_secs(4)),
            (4, Duration::from_secs(32)),
            (5, RETRY_CEILING),
            (31, RETRY_CEILING),
            (u32::MAX, RETRY_CEILING),
        ] {
            assert_eq!(retry_delay(two, misses), expected, "{misses} misses");
        }
        let hour = Duration::from_hours(1);
        for misses in [0, 3, u32::MAX] {
            assert_eq!(
                retry_delay(hour, misses),
                hour,
                "a poll configured slower than the ceiling is not answered by a faster retry"
            );
        }
    }

    /// **Suspended is the only state that parks the poll.** Focus in
    /// particular must not: a visible-but-unfocused window keeps its header
    /// live while you work elsewhere, which is the whole point of this chrome.
    ///
    /// Mutation (verified red): swap `SUSPENDED` for `FOCUSED` in
    /// [`presenting`] and the unfocused cases below red.
    #[test]
    fn only_a_suspended_toplevel_parks_the_poll() {
        assert!(presenting(ToplevelState::empty()), "a plain window polls");
        assert!(
            presenting(ToplevelState::TILED | ToplevelState::MAXIMIZED),
            "geometry states are not presentation states — neither parks the poll"
        );
        assert!(presenting(ToplevelState::FOCUSED), "a focused window polls");
        assert!(
            !presenting(ToplevelState::SUSPENDED),
            "a suspended window parks"
        );
        assert!(
            !presenting(ToplevelState::SUSPENDED | ToplevelState::FOCUSED),
            "suspended wins over every other bit that may ride along"
        );
    }
}

#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use super::{NO_PAGE, TrustResolver, Window};
    use crate::chrome::{ApprovalRow, Decision, Verb};
    use crate::cli::Tab;
    use crate::feed::{AgentState, Update};
    use crate::verify::tls_tests::{
        anchors, glib_ready, serve, serve_counting_dribbler, serve_dribbling,
    };
    use crate::verify::{Route, Source};
    use crate::{tls, webview};
    use gtk::prelude::*;
    use hytte_plugin_agents::config::AgentsConfig;
    use hytte_plugin_agents::hive::wire::{
        AgentStatusRow, Approval, ApprovalStatus, HiveUrls, Request, Scope,
    };
    use hytte_plugin_agents::model::{Agent, AgentName};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    fn name(s: &str) -> AgentName {
        AgentName::parse(s).expect("a legal test name")
    }

    /// One `Pending` row for `agent`, with a description a test can look for
    /// in the rendered row.
    fn approval(id: i64, agent: &str) -> Approval {
        Approval {
            id,
            agent: agent.to_owned(),
            status: ApprovalStatus::Pending,
            description: Some(format!("test approval #{id}")),
            requested_at: "2026-09-12T00:00:00Z".to_owned(),
            ..Approval::default()
        }
    }

    /// The click a row rendered from `approval` raises — what a queued click
    /// carries when it arrives after the row has moved on, which is the only
    /// way a test can deliver one (nothing renders a row for a request the
    /// window no longer has).
    fn clicked(approval: &Approval, verb: Verb) -> Decision {
        Decision::on(&ApprovalRow::of(approval, None, false), verb)
    }

    /// An application object, never run — `adw::ApplicationWindow` wants one,
    /// and registering is what `run` would do, which these tests must not.
    fn app() -> adw::Application {
        adw::Application::builder()
            .application_id("mov.vibec0re.trollshell.AgentWindow.test")
            .build()
    }

    /// A window for the tests that are **not** about TLS, with a trust
    /// resolver that reads nothing and dials nothing.
    ///
    /// Not [`Window::assemble`]'s real [`tls::resolve`]: that one reads the
    /// machine's `/var/lib/hive-tls` (and the `TROLLSHELL_AGENT_WINDOW_*`
    /// variables), so on a developer's own box — the one machine where this
    /// crate's hive material exists — a test about a banner would open a TLS
    /// connection to their real gateway and take however long that took. The
    /// answer it stands in for is exactly what a machine with no hive material
    /// produces (`TlsPolicy::SystemStore`, nothing tried), so nothing below
    /// changes shape; it just stops depending on whose laptop it runs on.
    /// #1246's own tests pick their route through [`scripted_trust`].
    fn window() -> (Rc<Window>, mpsc::UnboundedReceiver<Request>) {
        window_with_trust(Arc::new(|_| tls::Resolved::default()))
    }

    fn up(row: AgentStatusRow) -> AgentState {
        AgentState::Up(Box::new(Agent {
            name: name(&row.name.clone()),
            row,
            pending_paused: None,
        }))
    }

    fn row() -> AgentStatusRow {
        AgentStatusRow {
            name: "stray".to_owned(),
            running: true,
            status_text: Some("reviewing PR #963".to_owned()),
            ..AgentStatusRow::default()
        }
    }

    /// [`row`] whose page URL points at a fixture gateway on `port` — what
    /// makes the window's probe dial something a test controls.
    fn row_at(port: u16) -> AgentStatusRow {
        AgentStatusRow {
            url: Some(format!("https://localhost:{port}/agent/stray/")),
            ..row()
        }
    }

    /// Every probe a window ran under [`scripted_trust`]: the verdict, and how
    /// long the worker took to reach it.
    type Runs = Arc<Mutex<Vec<(tls::Resolved, Duration)>>>;

    /// A [`TrustResolver`] that runs the **real** route-2 probe — the same
    /// `verify::probe`, the same fixture anchors, the same `Deadline` — on a
    /// budget a test can wait out, and records each run.
    ///
    /// Not a canned `Resolved`: the point of #1246's tests is the *threading*,
    /// and a resolver that returns instantly cannot show that the main loop
    /// kept running while a slow one did not. Not the process environment
    /// either — `std::env::set_var` is `unsafe` in edition 2024 and this
    /// workspace `forbid`s `unsafe_code`, so a window's route can only be
    /// chosen through this seam.
    fn scripted_trust(budget: Duration) -> (TrustResolver, Runs) {
        let runs: Runs = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&runs);
        let route = Route::VerifyAgainstBundle {
            bundle: anchors(),
            source: Source::HiveDir,
        };
        let resolver: TrustResolver = Arc::new(move |url: &str| {
            let started = Instant::now();
            let resolved = tls::resolve_route_within(&route, url, budget);
            log.lock()
                .expect("the run log is not poisoned")
                .push((resolved.clone(), started.elapsed()));
            resolved
        });
        (resolver, runs)
    }

    fn window_with_trust(trust: TrustResolver) -> (Rc<Window>, mpsc::UnboundedReceiver<Request>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Window::assemble_with_trust(&app(), &name("stray"), AgentsConfig::default(), tx, trust),
            rx,
        )
    }

    /// The first (and, in these tests, only) probe a window ran.
    fn first_run(runs: &Runs) -> (tls::Resolved, Duration) {
        runs.lock()
            .expect("the run log is not poisoned")
            .first()
            .cloned()
            .expect("exactly one probe ran")
    }

    fn runs_so_far(runs: &Runs) -> usize {
        runs.lock().expect("the run log is not poisoned").len()
    }

    /// Iterate the GTK main context until `done`, or give up after `limit`.
    ///
    /// **Non-blocking iterations.** A test about the main loop still running
    /// must not itself be the thing that parks it: `iteration(true)` would
    /// sleep in `poll()` until a source is ready, so a probe that answers
    /// through a `oneshot` — which wakes the context — would be measured
    /// through a loop that was asleep for most of the probe. This spins and
    /// yields instead, so the tick counter below is counting dispatches the
    /// window's own main loop performed.
    fn pump_until(limit: Duration, done: impl Fn() -> bool) -> bool {
        let ctx = gtk::glib::MainContext::default();
        let give_up = Instant::now() + limit;
        while !done() {
            if Instant::now() >= give_up {
                return false;
            }
            if !ctx.iteration(false) {
                std::thread::yield_now();
            }
        }
        true
    }

    /// Stops a [`heartbeat`] source when it drops (#1274 N2).
    ///
    /// A plain `Rc<Cell<bool>>` a test sets by hand only closes the source on
    /// the path that reaches the `set(true)` call — a failing assertion
    /// *before* that call unwinds straight past it, which is exactly what
    /// `a_slow_hive_paints_the_verifying_state_and_the_main_loop_keeps_running`
    /// used to risk: its first three assertions ran before the old manual
    /// stop, so any one of them failing would have left a 20 ms
    /// `ControlFlow::Continue` timeout running on the shared default
    /// `MainContext` for every test that runs after — the exact thing this
    /// helper's own doc says must not happen. `Drop` runs on every exit from
    /// the scope that holds the guard, panicking or not — but it only sets a
    /// flag; the source removes *itself* the next time it fires and reads
    /// that flag (`ControlFlow::Break`, see [`heartbeat`]), so it outlives
    /// the drop by at most one period (20 ms at this file's own call sites),
    /// not for the rest of the process.
    struct HeartbeatGuard(Rc<Cell<bool>>);

    impl Drop for HeartbeatGuard {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    /// Cancels a window's queued retry when it drops — [`HeartbeatGuard`]'s
    /// shape, for the source the October 2026 audit's M1 fix queues.
    ///
    /// A probe with no verdict leaves a `glib` timeout on the one
    /// `MainContext` every `#[gtk::test]` shares, and the windows here are
    /// never dropped (see [`Window::me`] for the cycles that keep them), so
    /// left alone that source fires inside whichever test runs next and its
    /// probe dials **this** test's port — closed by then, and so free for a
    /// sibling's `serve()` to be handed and its one accept stolen: #1458's
    /// race, through a different door. Every test whose probe can end
    /// without a verdict holds one. It cancels what is *queued*; a probe still
    /// in flight when a test fails part-way is past its reach, which costs
    /// nothing a passing run can meet.
    struct RetryGuard(Rc<Window>);

    impl Drop for RetryGuard {
        fn drop(&mut self) {
            self.0.cancel_retry();
        }
    }

    /// What a probe that could not reach the gateway answers — the shape
    /// `tls::verify_then_pin`'s `Unreachable` arm builds, canned, so the tests
    /// that use it are about what the window does with the answer rather than
    /// about loopback timing. That the real arm sets the flag is
    /// `verify::tls_tests::a_gateway_that_does_not_answer_is_a_reachability_failure_not_a_trust_one`'s,
    /// against a real closed port.
    fn unreachable_answer() -> tls::Resolved {
        tls::Resolved {
            tried: Some(
                "could not check hive.local against the anchors in /fixture/trust-bundle.pem \
                 (test): could not reach hive.local:443 (Connection refused)"
                    .to_owned(),
            ),
            unreachable: true,
            ..tls::Resolved::default()
        }
    }

    /// A [`TrustResolver`] that answers `script[n]` on its `n`-th call (and
    /// the last entry on every call after), counting the calls.
    fn scripted_answers(script: Vec<tls::Resolved>) -> (TrustResolver, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let resolver: TrustResolver = Arc::new(move |_url: &str| {
            let n = counted.fetch_add(1, Ordering::SeqCst);
            script
                .get(n)
                .or_else(|| script.last())
                .cloned()
                .expect("a script with at least one answer")
        });
        (resolver, calls)
    }

    /// [`row`] with a page URL — none of the tests using it dial anything.
    fn row_with_page() -> AgentStatusRow {
        AgentStatusRow {
            url: Some("https://hive.local/agent/stray/".to_owned()),
            ..row()
        }
    }

    /// A heartbeat on the main context, and a guard that stops it when it
    /// drops.
    ///
    /// It has to stop: `#[gtk::test]` bodies share one `MainContext`, so a
    /// source left `Continue`ing would keep firing inside every test that runs
    /// after this one.
    fn heartbeat(every: Duration) -> (Rc<Cell<u32>>, HeartbeatGuard) {
        let ticks = Rc::new(Cell::new(0_u32));
        let stop = Rc::new(Cell::new(false));
        let counter = Rc::clone(&ticks);
        let halt = Rc::clone(&stop);
        gtk::glib::timeout_add_local(every, move || {
            counter.set(counter.get() + 1);
            if halt.get() {
                gtk::glib::ControlFlow::Break
            } else {
                gtk::glib::ControlFlow::Continue
            }
        });
        (ticks, HeartbeatGuard(stop))
    }

    /// `HeartbeatGuard`'s whole point, proven rather than trusted (#1274 N2):
    /// move the guard into a closure that panics — standing in for a failing
    /// assertion between `heartbeat(...)` and the old manual `stop.set(true)`
    /// — and show the source really does stop even though nothing that ran
    /// ever called `stop.set` directly.
    ///
    /// One tick after the drain below is expected either way: the callback
    /// increments `ticks` *before* it checks `halt` (see [`heartbeat`]), so
    /// the timer that was already armed when the guard dropped still fires
    /// once and only then breaks. What distinguishes "stopped" from "leaked"
    /// is the **second** sleep-and-drain: with the guard's `Drop` doing its
    /// job the source is gone by the first drain, so nothing moves the count
    /// again; leaked, it is still `Continue`ing every 5 ms and the second
    /// window catches it climbing.
    ///
    /// Falsification (restored after, red while applied): delete
    /// `HeartbeatGuard`'s `Drop` impl above and this reds — the second
    /// snapshot climbs past the first because the 5 ms source is still
    /// `Continue`ing.
    #[gtk::test]
    fn heartbeat_guard_stops_the_source_even_when_its_scope_panics() {
        let (ticks, guard) = heartbeat(Duration::from_millis(5));

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = guard;
            panic!("#1274 N2: simulating a failing assertion before the manual stop");
        }));
        assert!(
            panicked.is_err(),
            "the panic must actually happen for this test to prove anything"
        );

        // First drain: lets the timer that was already armed when the guard
        // dropped fire its one permitted last tick and break.
        std::thread::sleep(Duration::from_millis(50));
        let ctx = gtk::glib::MainContext::default();
        while ctx.iteration(false) {}
        let after_first_drain = ticks.get();

        // Second drain: if the source is truly gone, nothing moves it again.
        std::thread::sleep(Duration::from_millis(50));
        while ctx.iteration(false) {}

        assert_eq!(
            ticks.get(),
            after_first_drain,
            "the heartbeat kept firing after its guard's scope had already panicked away — the \
             guard's Drop did not run, or did not stop the source"
        );
    }

    /// **The page is built once.** Two states carrying the same URL leave the
    /// *same widget* in the slot — not a fresh `WebView` that threw away the
    /// scroll position and the half-typed message.
    ///
    /// Since #1246 the mount is two steps with a worker between them, so this
    /// pumps the main context until the probe answers before it takes the
    /// widget to compare. The identity assertion is against the **view**, not
    /// merely "not the hint": without that, the verifying state would satisfy
    /// it and the test would compare that with itself — the #1130 N6 vacuity,
    /// one state later.
    ///
    /// Mutation (re-run this round, red): the reviewer's **M13** — delete the
    /// `page_loaded` early return in `load_page` — and the pointer comparison
    /// reds.
    #[gtk::test]
    fn the_page_is_built_once_and_not_on_every_poll() {
        let (w, _rx) = window();
        let with_url = AgentStatusRow {
            url: Some("https://hive.local/agent/stray/".to_owned()),
            ..row()
        };

        w.update(Update::State(up(with_url.clone())));
        assert!(
            pump_until(Duration::from_secs(10), || !w.probing()),
            "the probe never answered"
        );
        let first = w.page_child().expect("the page went into the slot");
        // **Not vacuously** (#1130 N6): `assemble`'s first `apply` puts the
        // `NO_PAGE` label in the slot and #1246's `begin_probe` puts the
        // verifying state there, so without this the test would compare one of
        // those with itself and stay green with `Window::update` mutated to a
        // no-op — measured by #1242's re-verification.
        assert!(
            webview::view_of(&first).is_some(),
            "the update must have replaced the no-page hint with the view, or what follows \
             compares the hint with itself"
        );

        // A second poll, and a third that changes something else entirely.
        w.update(Update::State(up(with_url.clone())));
        w.update(Update::State(up(AgentStatusRow {
            status_text: Some("something else".to_owned()),
            ..with_url
        })));

        let again = w.page_child().expect("the slot is still filled");
        assert_eq!(
            first, again,
            "the embedded page must survive a poll — rebuilding it throws away the scroll \
             position and any half-typed message"
        );
    }

    /// With no URL from the hive, the slot carries the explanation and **not**
    /// a view pointed at nothing — and it is still replaced once a URL lands.
    ///
    /// Since #1246 that replacement is in two steps, and both are asserted:
    /// the verifying state goes in synchronously, the view when the worker
    /// answers.
    #[gtk::test]
    fn no_url_shows_the_hint_until_one_arrives() {
        let (w, _rx) = window();
        w.update(Update::State(up(row())));

        let hint = w.page_child().expect("the hint went into the slot");
        let label = hint
            .downcast_ref::<gtk::Label>()
            .expect("the no-page state is a label, not a view");
        assert_eq!(label.label(), NO_PAGE);

        w.update(Update::State(up(AgentStatusRow {
            url: Some("https://hive.local/agent/stray/".to_owned()),
            ..row()
        })));
        let verifying = w
            .page_child()
            .expect("the verifying state replaced the hint");
        assert!(
            webview::is_verifying(&verifying),
            "the hint goes the moment a URL lands, and what takes its place says why there is no \
             page yet"
        );

        assert!(
            pump_until(Duration::from_secs(10), || !w.probing()),
            "the probe never answered"
        );
        let page = w
            .page_child()
            .expect("the page replaced the verifying state");
        assert!(
            webview::view_of(&page).is_some(),
            "once the hive names a URL and its certificate checks out, the slot holds the page"
        );
    }

    /// **A refusal raises the banner, and a click clears it.**
    ///
    /// The banner is the whole of what an operator sees when the hive says no
    /// (the toggle is put back by the reconciling state `feed::run` now
    /// forces — `tests/feed.rs` owns that half).
    ///
    /// Mutation (re-run this round, red): drop the `Update::Refused` arm in
    /// `Window::update` and the first assertion reds.
    #[gtk::test]
    fn a_refusal_raises_the_banner_and_the_next_click_clears_it() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        assert_eq!(w.banner_text(), None, "nothing to say yet");

        w.update(Update::Refused {
            request: Request::SetPaused {
                name: "stray".to_owned(),
                paused: true,
            },
            reason: "agent busy".to_owned(),
        });
        assert_eq!(
            w.banner_text().as_deref(),
            Some("couldn't pause this agent: agent busy")
        );

        // A press clears it — a stale refusal over a fresh click reads as if
        // the new click failed too.
        w.header().press_stop_for_test();
        assert_eq!(w.banner_text(), None);
        assert!(matches!(
            rx.try_recv(),
            Ok(Request::Stop { graceful: true, .. })
        ));
    }

    /// **The hive's refusal reason is shown as text, not parsed as markup** —
    /// the October 2026 audit's LOW.
    ///
    /// `AdwBanner:use-markup` defaults to TRUE, and the title is the hive's
    /// own sentence. A `<` that does not open a tag GTK knows, or a bare `&`,
    /// fails the parse, and `GtkLabel` keeps whatever it showed before — for
    /// a first refusal, nothing: a revealed banner with no words in it.
    ///
    /// Read off the banner's own title **label**, not its `title` property:
    /// the property reads back whatever was set, parsed or not, which is why
    /// `banner_text` alone never saw this.
    ///
    /// Mutation (run this round, red): drop `set_use_markup(false)` in
    /// `assemble_with_trust` and the rendered-text assertion reds with `[""]`
    /// where the sentence should be.
    #[gtk::test]
    fn a_refusal_reason_is_shown_as_text_not_parsed_as_markup() {
        let (w, _rx) = window();
        w.update(Update::State(up(row())));
        let reason = "policy <deny> says a < b & c";
        w.update(Update::Refused {
            request: Request::SetPaused {
                name: "stray".to_owned(),
                paused: true,
            },
            reason: reason.to_owned(),
        });
        let expected = format!("couldn't pause this agent: {reason}");
        assert_eq!(w.banner_text().as_deref(), Some(expected.as_str()));
        let rendered = w.banner_rendered();
        assert!(
            rendered.contains(&expected),
            "the banner must show the hive's sentence as written: {rendered:?}"
        );
    }

    /// A button press puts **that verb** on the command lane, scoped to this
    /// window's agent.
    ///
    /// The bytes are pinned in `feed.rs`; what this pins is the wiring between
    /// the widget and the lane, which nothing covered.
    #[gtk::test]
    fn a_button_press_reaches_the_command_lane() {
        let (w, mut rx) = window();
        w.update(Update::State(up(AgentStatusRow {
            running: false,
            ..row()
        })));

        w.header().press_start_for_test();
        match rx.try_recv() {
            Ok(Request::Start { scope }) => {
                assert_eq!(scope.agent_names(), [String::from("stray")]);
            }
            other => panic!("expected a scoped Start, got {other:?}"),
        }
        assert_eq!(
            Scope::agent(&name("stray")).agent_names(),
            [String::from("stray")]
        );
    }

    /// The hive's `Urls` answer reaches the Settings tab.
    #[gtk::test]
    fn the_urls_answer_reaches_the_settings_rows() {
        let (w, _rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Urls(Box::new(HiveUrls {
            domain: Some("hive.local".to_owned()),
            home: Some("https://hive.local/".to_owned()),
            forge: None,
        })));
        assert!(
            w.settings
                .row_text()
                .iter()
                .any(|r| r == "Dashboard: https://hive.local/"),
            "{:?}",
            w.settings.row_text()
        );
    }

    /// `--tab settings` lands on the settings page, and a later `--tab agent`
    /// (a second launch forwarded by `HANDLES_COMMAND_LINE`) moves it back.
    #[gtk::test]
    fn the_tab_argument_moves_the_stack() {
        let (w, _rx) = window();
        w.show_tab(Tab::Settings);
        assert_eq!(w.visible_tab().as_deref(), Some("settings"));
        w.show_tab(Tab::Agent);
        assert_eq!(w.visible_tab().as_deref(), Some("agent"));
    }

    /// **Rows render from `Pending`, and the group hides itself when the
    /// queue is empty.** The widget half of the pin `chrome`'s own tests
    /// already have for the pure filter — this is what proves the filtered
    /// rows actually reach the Settings page (#1141).
    #[gtk::test]
    fn approvals_render_from_pending_and_the_group_hides_when_empty() {
        let (w, _rx) = window();
        w.update(Update::State(up(row())));
        assert!(
            !w.settings.approvals_visible(),
            "no approval, no group — nothing to decide yet"
        );

        w.update(Update::Approvals(Ok(vec![
            approval(1, "stray"),
            approval(2, "other-agent"),
        ])));
        assert!(w.settings.approvals_visible());
        assert_eq!(
            w.settings.approval_row_text().len(),
            1,
            "the other agent's row must not leak into this window: {:?}",
            w.settings.approval_row_text()
        );

        w.update(Update::Approvals(Ok(Vec::new())));
        assert!(
            !w.settings.approvals_visible(),
            "an emptied queue hides the group again"
        );
    }

    /// **Approving a pending row sends the pinned bytes**, and the row's
    /// membership check is the guard [`Window::on_decision`] applies before
    /// anything reaches the command lane.
    ///
    /// Mutation (verified red): make `on_decision` build `Request::Deny`
    /// for `Verb::Approve` (`chrome::Decision::request`) and the byte
    /// assertion reds.
    #[gtk::test]
    fn approving_a_pending_row_sends_the_pinned_approve_bytes() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(42, "stray")])));

        w.settings.press_approve_for_test(42);
        match rx.try_recv() {
            Ok(Request::Approve { id }) => assert_eq!(id, 42),
            other => panic!("expected Approve {{ id: 42 }}, got {other:?}"),
        }
    }

    /// Denying does the same, on the other button.
    #[gtk::test]
    fn denying_a_pending_row_sends_the_pinned_deny_bytes() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(7, "stray")])));

        w.settings.press_deny_for_test(7);
        match rx.try_recv() {
            Ok(Request::Deny { id }) => assert_eq!(id, 7),
            other => panic!("expected Deny {{ id: 7 }}, got {other:?}"),
        }
    }

    /// **A decision for an id no longer pending is dropped, not sent** —
    /// spec §6.5's rule for the race between a poll and a click, exercised
    /// directly against [`Window::on_decision`] rather than through a stale
    /// button (nothing renders a row for an id the window no longer has, so
    /// the only way to construct the race in a test is to ask for the
    /// decision the way a queued click would arrive).
    ///
    /// Mutation (verified red): delete the `chrome::should_send` guard in
    /// `on_decision` and this reds.
    #[gtk::test]
    fn a_decision_for_an_id_no_longer_pending_is_dropped_not_sent() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(5, "stray")])));
        w.update(Update::Approvals(Ok(Vec::new()))); // resolved elsewhere

        w.on_decision(clicked(&approval(5, "stray"), Verb::Approve));
        assert!(
            rx.try_recv().is_err(),
            "an id the last poll no longer carries must not reach the hive"
        );
    }

    /// **A re-numbered id gets a fresh row, and a click raised from the old
    /// one sends nothing.** The hive's store was reset or restored between
    /// two polls, and id 7 now names a different request. The row the
    /// operator saw leaves the container (so a press that began on it cannot
    /// be released onto the new request), the new request gets its own row
    /// and its own buttons, and a click already queued for the old row —
    /// carrying the request it showed — is refused by the guard rather than
    /// sent as an `Approve { id: 7 }` that would decide the new one.
    ///
    /// Mutations (verified red): key `ui::Approvals::apply`'s survivor test
    /// on the id alone (`w.id == a.id`) and the "old row left" assertion
    /// reds; revert `chrome::should_send` to the id-only check and the
    /// "nothing sent" one does.
    #[gtk::test]
    fn a_renumbered_id_gets_a_fresh_row_and_the_old_rows_click_sends_nothing() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        let shown = approval(7, "stray");
        w.update(Update::Approvals(Ok(vec![shown.clone()])));
        let old = w.settings.tracked_approval_rows();
        assert_eq!(old.len(), 1);

        let renumbered = Approval {
            description: Some("merge PR #99 (force-push to main)".to_owned()),
            ..approval(7, "stray")
        };
        w.update(Update::Approvals(Ok(vec![renumbered])));
        assert!(
            old.iter().all(|r| r.parent().is_none()),
            "the row for the request that left must leave the group, not be relabelled"
        );
        let text = w.settings.approval_row_text();
        assert_eq!(text.len(), 1, "{text:?}");
        assert!(text[0].contains("force-push"), "{text:?}");

        w.on_decision(clicked(&shown, Verb::Approve));
        assert!(
            rx.try_recv().is_err(),
            "a click raised from the old row must not approve the request now under its id"
        );

        // The request now on screen is answerable on its own row.
        w.settings.press_approve_for_test(7);
        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 7 })));
    }

    /// **A re-numbered id inherits neither the old request's latch nor its
    /// refusal.** A decision was in flight for request A under id 7 when the
    /// hive re-numbered; request B under the same id was never decided, so
    /// its buttons are live, and the refusal that comes back for A's
    /// decision is not printed on B's row.
    ///
    /// Mutations (verified red): prune `in_flight` by id alone
    /// (`queue.rows().iter().any(|a| a.id == *id)`) and the latch assertion
    /// reds — B's row would sit insensitive until A's answer happened to
    /// come back; fall back to recording a refusal on whatever request is
    /// queued under its id when nothing is in flight for it (the guard before
    /// this change asked only whether the id was still queued) and the
    /// refusal assertion does.
    #[gtk::test]
    fn a_renumbered_id_inherits_neither_the_latch_nor_the_refusal() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(7, "stray")])));
        assert!(w.settings.try_press_approve_for_test(7));
        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 7 })));

        w.update(Update::Approvals(Ok(vec![Approval {
            description: Some("a different request".to_owned()),
            ..approval(7, "stray")
        }])));
        assert!(
            w.settings.approve_is_live_for_test(7),
            "request B never had a decision sent, so it must not hold A's latch"
        );

        w.update(Update::Refused {
            request: Request::Approve { id: 7 },
            reason: "no such approval".to_owned(),
        });
        assert!(
            !w.settings
                .approval_row_text()
                .iter()
                .any(|r| r.contains("no such approval")),
            "a refusal about request A must not decorate request B: {:?}",
            w.settings.approval_row_text()
        );
        assert!(
            w.settings.try_press_approve_for_test(7),
            "request B was never decided, so its row must be answerable"
        );
        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 7 })));
    }

    /// **An unreachable hive's queue is forgotten, and a click that arrives
    /// meanwhile is not sent — and says so.** The rows go (nobody can vouch
    /// for them), the group says why instead of hiding, a click raised from
    /// a row that was on screen sends nothing and reveals the banner, and
    /// the queue the hive answers once it is back is answerable again.
    ///
    /// Mutations (verified red): drop the `forget_queue(Queue::Unreachable)`
    /// in `Window::update`'s `State` arm and the notice assertion reds — the
    /// stale row is still there, buttons live; drop the banner in
    /// `on_decision`'s `Unread` arm and the banner assertion does; drop the
    /// `set_revealed(false)` before a decision is sent and the last one does.
    #[gtk::test]
    fn an_unreachable_hive_forgets_the_queue_and_an_unsent_click_says_so() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        let shown = approval(7, "stray");
        w.update(Update::Approvals(Ok(vec![shown.clone()])));
        assert_eq!(w.settings.approval_row_text().len(), 1);

        w.update(Update::State(AgentState::Unreachable {
            reason: "the hive is not running".to_owned(),
        }));
        let text = w.settings.approval_row_text();
        assert_eq!(text.len(), 1, "only the notice, no stale row: {text:?}");
        assert!(
            text[0].starts_with(crate::chrome::APPROVALS_UNREACHABLE),
            "{text:?}"
        );
        assert!(w.settings.approvals_visible());

        w.on_decision(clicked(&shown, Verb::Approve));
        assert!(
            rx.try_recv().is_err(),
            "nothing may be sent against a queue this window cannot read"
        );
        let banner = w.banner_text().expect("the operator is told");
        assert!(
            banner.contains("not sent") && banner.contains("#7"),
            "{banner}"
        );

        // The hive comes back with the same request: answerable again.
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![shown])));
        assert!(w.settings.try_press_approve_for_test(7));
        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 7 })));
        assert_eq!(
            w.banner_text(),
            None,
            "a decision that did go out retires the \"not sent\" banner"
        );
    }

    /// **A refused write keeps the row and shows why inline.** The row is
    /// still there, still bearing live buttons, and its subtitle now carries
    /// the hive's own reason — no global banner for this one (#1141).
    #[gtk::test]
    fn a_refused_approval_keeps_the_row_and_shows_the_reason_inline() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(9, "stray")])));
        w.settings.press_approve_for_test(9);
        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 9 })));

        w.update(Update::Refused {
            request: Request::Approve { id: 9 },
            reason: "agent busy".to_owned(),
        });

        assert_eq!(
            w.banner_text(),
            None,
            "an approval refusal is inline, not a banner"
        );
        assert!(
            w.settings
                .approval_row_text()
                .iter()
                .any(|r| r.contains("agent busy")),
            "{:?}",
            w.settings.approval_row_text()
        );
        // Still pressable — the operator can retry.
        w.settings.press_approve_for_test(9);
    }

    /// **A second click on a row whose decision is in flight sends nothing**
    /// — #1146's review, H1, both halves: the buttons go insensitive, *and*
    /// `on_decision` refuses a click that was already queued when they did.
    ///
    /// `Approve`/`Deny` act immediately on the far side and nothing is
    /// flipped optimistically here, so the row survives until the next poll —
    /// a whole `poll_interval` (2 s by default) in which a double-click used
    /// to put two identical frames on the socket, the second of which the
    /// hive refuses, annotating a row whose decision actually succeeded.
    ///
    /// Mutations (both verified red): drop the `set_sensitive(!a.in_flight)`
    /// pair in `ui::Approvals::apply` and the first assertion reds; drop the
    /// `in_flight.insert` guard in `on_decision` and the second reds.
    #[gtk::test]
    fn two_clicks_on_the_same_row_send_one_decision() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(42, "stray")])));

        assert!(
            w.settings.try_press_approve_for_test(42),
            "the first click must be possible"
        );
        assert!(
            !w.settings.try_press_approve_for_test(42),
            "a row with a decision in flight must have no live Approve button"
        );
        // …and the guard behind the widget state, for the click GTK had
        // already queued when the buttons went dead.
        w.on_decision(clicked(&approval(42, "stray"), Verb::Approve));

        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 42 })));
        assert!(
            rx.try_recv().is_err(),
            "a second click on an in-flight row must not send a second Approve"
        );
    }

    /// **Deny cannot follow an Approve the operator already sent** — the same
    /// latch, across the two buttons.
    ///
    /// This is the scenario spec §6.5's "silence decides nothing" argument is
    /// really about: `Deny` durably refuses the reviewed work on the far
    /// side, and an operator who clicked Approve, saw nothing happen for two
    /// seconds and tried the other button used to send both.
    #[gtk::test]
    fn deny_cannot_follow_an_approve_for_the_same_approval() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(42, "stray")])));

        assert!(w.settings.try_press_approve_for_test(42));
        assert!(
            !w.settings.try_press_deny_for_test(42),
            "the Deny button of an in-flight row must be dead too"
        );
        w.on_decision(clicked(&approval(42, "stray"), Verb::Deny));

        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 42 })));
        assert!(
            rx.try_recv().is_err(),
            "Deny must not follow an Approve the operator already sent for the same id"
        );
    }

    /// **A refusal re-opens the row**, which is the whole reason the refusal
    /// path keeps it (#1146's review, H1) — and **a retry clears the previous
    /// attempt's reason** (L1), because a row that still reads `couldn't
    /// answer: agent busy` about a decision now on the wire is simply false.
    ///
    /// Mutations (verified red): drop the `in_flight.remove(id)` in the
    /// `Update::Refused` arm and the retry assertion reds; drop the
    /// `approval_refusals.remove(&id)` in `on_decision` and the last one does.
    #[gtk::test]
    fn a_refusal_reopens_the_row_and_the_retry_clears_the_old_reason() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(9, "stray")])));

        assert!(w.settings.try_press_approve_for_test(9));
        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 9 })));
        assert!(!w.settings.try_press_approve_for_test(9), "in flight");

        w.update(Update::Refused {
            request: Request::Approve { id: 9 },
            reason: "agent busy".to_owned(),
        });
        assert!(
            w.settings
                .approval_row_text()
                .iter()
                .any(|r| r.contains("agent busy")),
            "{:?}",
            w.settings.approval_row_text()
        );

        assert!(
            w.settings.try_press_approve_for_test(9),
            "a refused decision must leave the row answerable again"
        );
        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 9 })));
        assert!(
            !w.settings
                .approval_row_text()
                .iter()
                .any(|r| r.contains("agent busy")),
            "a retry must not keep showing the previous attempt's refusal: {:?}",
            w.settings.approval_row_text()
        );
    }

    /// **A refusal reason does not outlive the row it is about.**
    ///
    /// `Window::update`'s prune (#1146's review, L2): an approval that leaves
    /// the queue and comes back — a hive that re-queues an id, or a window
    /// that watched one resolve and another take its number — must come back
    /// clean.
    ///
    /// Mutation (verified red): delete the `approval_refusals.retain(…)` line
    /// in the `Update::Approvals(Ok(_))` arm and the last assertion reds.
    #[gtk::test]
    fn a_refusal_is_pruned_when_its_row_leaves_the_queue() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(9, "stray")])));
        w.settings.press_approve_for_test(9);
        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 9 })));
        w.update(Update::Refused {
            request: Request::Approve { id: 9 },
            reason: "agent busy".to_owned(),
        });
        assert!(
            w.settings
                .approval_row_text()
                .iter()
                .any(|r| r.contains("agent busy"))
        );

        w.update(Update::Approvals(Ok(Vec::new()))); // resolved elsewhere
        w.update(Update::Approvals(Ok(vec![approval(9, "stray")]))); // and back

        assert!(
            !w.settings
                .approval_row_text()
                .iter()
                .any(|r| r.contains("agent busy")),
            "a reason must not survive the row it explained: {:?}",
            w.settings.approval_row_text()
        );
    }

    /// **The poll that takes the row out of the queue opens the latch.**
    ///
    /// `in_flight`'s doc says the latch empties two ways: the id leaves the
    /// polled queue, or a refusal arrives. Only the second used to be
    /// tested. #1146 re-verify, M-NEW-1.
    ///
    /// Mutation (verified red): delete the
    /// `in_flight.borrow_mut().retain(|id| live.contains(id))` line in the
    /// `Update::Approvals(Ok(_))` arm and the last `assert!` reds — the
    /// latch never reopens, so the row stays permanently unanswerable.
    #[gtk::test]
    fn the_latch_opens_when_the_poll_takes_the_row_out_of_the_queue() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(7, "stray")])));
        assert!(w.settings.try_press_approve_for_test(7));
        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 7 })));

        w.update(Update::Approvals(Ok(Vec::new()))); // it landed
        w.update(Update::Approvals(Ok(vec![approval(7, "stray")]))); // and comes back

        assert!(
            w.settings.try_press_approve_for_test(7),
            "the poll that took the row out of the queue must open the latch"
        );
        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 7 })));
    }

    /// **A refusal arriving after its row already left the queue leaves
    /// nothing behind.**
    ///
    /// The `Update::Refused` arm used to insert into `approval_refusals`
    /// unconditionally, never asking whether the id was still in `pending`.
    /// The prune at `Update::Approvals` only runs on that update and only
    /// keeps ids the fresh queue carries, so an orphan inserted after the
    /// row left was not pruned on the empty tick (it did not exist yet) and
    /// was kept by the tick that brought the id back — decorating a row
    /// with a reason about a decision that was never sent for it. #1146
    /// re-verify, M-NEW-1.
    ///
    /// **Fails on HEAD before the guard**; passes with the guard on the
    /// insert — first "the id is still pending", and since the decision was
    /// bound to its request, "a decision for this request is still in
    /// flight", which the prune that took the row away has already cleared.
    #[gtk::test]
    fn a_refusal_that_arrives_after_its_row_left_the_queue_leaves_nothing_behind() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(8, "stray")])));
        assert!(w.settings.try_press_approve_for_test(8));
        assert!(matches!(rx.try_recv(), Ok(Request::Approve { id: 8 })));

        w.update(Update::Approvals(Ok(Vec::new()))); // the row leaves first…
        w.update(Update::Refused {
            // …and the "no" arrives after
            request: Request::Approve { id: 8 },
            reason: "agent busy".to_owned(),
        });
        assert!(
            w.settings.approval_row_text().is_empty(),
            "a refusal cannot resurrect a row: {:?}",
            w.settings.approval_row_text()
        );

        w.update(Update::Approvals(Ok(vec![approval(8, "stray")])));
        assert!(
            !w.settings
                .approval_row_text()
                .iter()
                .any(|r| r.contains("agent busy")),
            "an orphaned reason must not decorate the row that comes back: {:?}",
            w.settings.approval_row_text()
        );
        assert!(
            w.settings.try_press_approve_for_test(8),
            "the row that comes back must be answerable"
        );
    }

    /// **An id that leaves the queue takes its row out of the group**, not
    /// just out of the bookkeeping — the reviewer's test (#1146's review,
    /// M2), taken as supplied. Since #1149 N2, `ui::Approvals::apply`
    /// retargets a *surviving* id's row rather than rebuilding it (the
    /// sibling test below pins that half); this one is the case with no
    /// surviving id at all, disjoint before and after, where retargeting and
    /// a full rebuild produce the identical outcome this asserts: the old
    /// widget leaves the container, and only the new one is in it.
    ///
    /// Without the detach, a rebuild-or-retarget that only drained the `Vec`
    /// would leak every predecessor into the group — with **live
    /// Approve/Deny buttons on approvals that already left the queue** — and
    /// `approval_row_text` could not see it, because it reads back from the
    /// same `Vec` the drain empties. #1130's M4, reintroduced forty lines
    /// below the doc written about it.
    ///
    /// Mutation (verified red): delete the `self.root.remove(&w.row)` loop in
    /// `ui::Approvals::apply`, keeping the rest.
    #[gtk::test]
    fn approval_rows_are_rebuilt_not_appended() {
        let (w, _rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(1, "stray")])));
        let first = w.settings.tracked_approval_rows();
        assert_eq!(first.len(), 1);
        assert!(
            first.iter().all(|r| r.parent().is_some()),
            "the first apply must actually put the row in the group"
        );

        w.update(Update::Approvals(Ok(vec![approval(2, "stray")])));
        assert!(
            first.iter().all(|r| r.parent().is_none()),
            "the old approval row must leave the group, not just the bookkeeping"
        );
        assert!(
            w.settings
                .tracked_approval_rows()
                .iter()
                .all(|r| r.parent().is_some()),
            "…and the new one must be in it"
        );
    }

    /// **A surviving approval id keeps its own widget across an apply**
    /// (#1149 N2) — the fix's own pin, alongside the sibling test above that
    /// covers the fully-disjoint case.
    ///
    /// `Settings::apply` used to rebuild every approval row on every call
    /// regardless of whether the id set had changed at all, so a plain
    /// status change elsewhere on the window (which repaints the whole
    /// chrome, `Window::apply`) tore down and rebuilt buttons for a queue
    /// that had not moved — cheap today, but H1's in-flight latch (#1146's
    /// review) gives a row state to lose, and a click racing that rebuild
    /// lands on a widget about to be replaced. `ui::Approvals::apply` now
    /// retargets an id it has already seen instead of rebuilding it, proven
    /// here by comparing the `AdwActionRow` `GObject` itself (`PartialEq` on
    /// a `glib::Object` is pointer identity) before and after a second apply
    /// that keeps id 1 and adds id 2.
    ///
    /// Mutation (verified red): revert `ui::Approvals::apply` to rebuild
    /// every row unconditionally (drain everything, rebuild every id in
    /// `approvals`) and the identity assertion reds — the row for `1` is a
    /// fresh `GObject` on the second apply, even though nothing about it
    /// changed.
    #[gtk::test]
    fn a_surviving_approval_id_keeps_its_own_widget_across_an_apply() {
        let (w, _rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(1, "stray")])));
        let before = w.settings.tracked_approval_rows();
        assert_eq!(before.len(), 1);
        let survivor = before[0].clone();

        // A second apply that keeps id 1 and adds id 2 — a plain queue
        // growth, not a replacement.
        w.update(Update::Approvals(Ok(vec![
            approval(1, "stray"),
            approval(2, "stray"),
        ])));
        let after = w.settings.tracked_approval_rows();
        assert_eq!(after.len(), 2, "{after:?}");
        assert_eq!(
            after[0], survivor,
            "id 1 survived the apply and must keep its own widget, not a rebuilt lookalike"
        );
        assert!(
            survivor.parent().is_some(),
            "the surviving row must still be mounted, not detached-and-forgotten"
        );
    }

    /// **Only the delta touches the container** (#1149 N2, this round's
    /// review LOW 2) — a surviving row is not unparented and re-added.
    ///
    /// The first cut of the retarget preserved the `GObject` but still
    /// detached *every* row up front and re-added the survivors in order, so
    /// the container churn N2 set out to remove was unchanged and the doc
    /// claiming "only the delta … touches the container" was false. `parent`
    /// is a widget property, so the churn is directly observable: a
    /// detach-and-re-add fires `notify::parent` twice, in place fires it not
    /// at all.
    ///
    /// This drives the **production** shape end to end: the queue only ever
    /// reaches the group id-sorted (`PendingApprovals::new` sorts by id, and
    /// the hive's ids only grow), so a departure comes out of the middle and
    /// an arrival goes on the end — never a reorder. The reorder *fallback*
    /// cannot be produced through `Window::update` at all and is exercised
    /// one level down, in `ui`'s own
    /// `a_reordered_queue_re_adds_the_same_rows_in_the_new_order`.
    ///
    /// Mutation (verified red): restore the up-front
    /// `for w in existing.iter() { self.root.remove(&w.row) }` in
    /// `ui::Approvals::apply` and this reds at four (two applies × detach and
    /// re-add).
    #[gtk::test]
    fn only_the_delta_touches_the_container_when_the_queue_grows_or_shrinks() {
        let (w, _rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![
            approval(1, "stray"),
            approval(2, "stray"),
        ])));
        let survivor = w.settings.tracked_approval_rows()[0].clone();
        let churn = Rc::new(std::cell::Cell::new(0u32));
        let counter = Rc::clone(&churn);
        survivor.connect_parent_notify(move |_| counter.set(counter.get() + 1));

        // An arrival on the end and a departure from the middle: neither
        // moves the row for id 1.
        w.update(Update::Approvals(Ok(vec![
            approval(1, "stray"),
            approval(2, "stray"),
            approval(3, "stray"),
        ])));
        w.update(Update::Approvals(Ok(vec![
            approval(1, "stray"),
            approval(3, "stray"),
        ])));
        assert_eq!(
            churn.get(),
            0,
            "a surviving row was unparented and re-added — only the delta may touch the container"
        );
        let rows = w.settings.tracked_approval_rows();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0], survivor, "id 1 kept its own widget and its place");
        assert!(
            rows.iter().all(|r| r.parent().is_some()),
            "every row must still be mounted"
        );
    }

    /// **A surviving row's buttons still send its own id after the queue
    /// moved around it** — the reviewer's added test (this round, "what I
    /// added"), in the shape the window can actually produce.
    ///
    /// Sound by construction, since the match key `w.id == a.id` is the same
    /// id the closure captured when the row was built — but the shipped
    /// identity test proves only that the widget survives, and never presses
    /// it. Approving the wrong request is the worst thing a retarget could
    /// do; this measures the frame on the wire after the row above it left
    /// the queue and every surviving row shifted up. (The reviewer's literal
    /// version swaps two ids, which `PendingApprovals`' id sort makes
    /// unreachable here; that path is pressed in `ui`'s own
    /// `a_reordered_queue_re_adds_the_same_rows_in_the_new_order`.)
    #[gtk::test]
    fn a_shifted_queue_still_sends_the_pressed_rows_own_id() {
        let (w, mut rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![
            approval(1, "stray"),
            approval(2, "stray"),
            approval(3, "stray"),
        ])));
        w.update(Update::Approvals(Ok(vec![
            approval(2, "stray"),
            approval(3, "stray"),
        ])));

        assert!(w.settings.try_press_approve_for_test(3));
        assert!(
            matches!(rx.try_recv(), Ok(Request::Approve { id: 3 })),
            "the button on id 3's row must still approve id 3 after the row above it left"
        );
    }

    /// **A refused queue says so, rather than looking like an empty one**
    /// (#1146's review, M1).
    ///
    /// Both clear the rows — this window cannot vouch for them either way —
    /// so with the group hiding itself when empty, an older daemon or a
    /// permissions change rendered *identically* to a healthy hive with
    /// nothing queued. It carries the hive's own sentence, and recovers.
    ///
    /// Mutation (verified red): drop the `refused` arm in
    /// `ui::Approvals::apply` and the row-count assertion reds with an empty
    /// group — which is precisely the indistinguishable state.
    #[gtk::test]
    fn a_refused_queue_renders_its_own_state_not_an_empty_group() {
        let (w, _rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(1, "stray")])));

        w.update(Update::Approvals(Err("permission denied".to_owned())));
        assert!(
            w.settings.approvals_visible(),
            "a hive that will not answer must not render as an empty queue"
        );
        let text = w.settings.approval_row_text();
        assert_eq!(text.len(), 1, "the rows themselves are gone: {text:?}");
        assert!(
            text[0].starts_with(crate::chrome::APPROVALS_REFUSED)
                && text[0].contains("permission denied"),
            "{text:?}"
        );

        // …and it recovers.
        w.update(Update::Approvals(Ok(vec![approval(1, "stray")])));
        let text = w.settings.approval_row_text();
        assert_eq!(text.len(), 1);
        assert!(
            !text[0].contains("permission denied"),
            "a queue that answers again drops the refusal state: {text:?}"
        );
    }

    /// **A flapping hive leaves exactly one warning row in the container**,
    /// not one per flap (#1146 re-verify, L-NEW-1).
    ///
    /// `Approvals::apply` removes the *previous* refusal row from `self.root`
    /// before this call's rows go in — asserted here against the container
    /// (`tracked_approval_rows`, which since L-NEW-1 chains the refusal row
    /// too), not the bookkeeping alone, the same #851 shape
    /// `Settings::tracked_rows`'s doc is written about: a snapshot taken
    /// *before* the rebuild still holds the old row's `GObject`, so its
    /// `parent()` answers whether the widget itself left, independent of
    /// whatever the bookkeeping was overwritten with.
    ///
    /// Mutation (verified red): drop the `self.root.remove(&row)` call on the
    /// old refusal row in `Approvals::apply` (keeping the `refusal.take()`)
    /// and the first assertion below reds — the old row is still parented.
    #[gtk::test]
    fn a_flapping_hive_leaves_exactly_one_warning_row_in_the_container() {
        let (w, _rx) = window();
        w.update(Update::State(up(row())));

        // refused…
        w.update(Update::Approvals(Err("permission denied".to_owned())));
        let first_refusal = w.settings.tracked_approval_rows();
        assert_eq!(first_refusal.len(), 1);

        // …ok…
        w.update(Update::Approvals(Ok(vec![approval(1, "stray")])));
        assert!(
            first_refusal.iter().all(|r| r.parent().is_none()),
            "the old warning row must leave the container when the hive answers again"
        );

        // …refused again.
        w.update(Update::Approvals(Err("permission denied again".to_owned())));
        let rows = w.settings.tracked_approval_rows();
        assert_eq!(
            rows.len(),
            1,
            "a flapping hive must leave exactly one warning row tracked, not one per flap"
        );
        assert!(
            rows.iter().all(|r| r.parent().is_some()),
            "the current warning row must be in the container"
        );
    }

    /// **The Settings tab is badged with the queue depth** (#1146's review,
    /// M5).
    ///
    /// The window opens on the Agent tab, the queue renders only on Settings,
    /// and the group hides when empty — so an operator watching the turn
    /// stream had no signal at all that an approval had queued. The sidebar's
    /// answer is a badge; `adw::ViewSwitcher` renders one for a
    /// `ViewStackPage`, so this is the same answer with no new widget.
    ///
    /// Mutation (verified red): delete the `badge_settings` call in
    /// `Window::apply`.
    #[gtk::test]
    fn the_settings_tab_is_badged_with_the_queue_depth() {
        let (w, _rx) = window();
        w.update(Update::State(up(row())));
        let page = w.stack.page(&w.settings.root);
        assert_eq!(page.badge_number(), 0);
        assert!(!page.needs_attention());

        w.update(Update::Approvals(Ok(vec![
            approval(1, "stray"),
            approval(2, "stray"),
            approval(3, "other-agent"),
        ])));
        assert_eq!(
            page.badge_number(),
            2,
            "this agent's queue only — the badge counts what this window can decide"
        );
        assert!(page.needs_attention());

        w.update(Update::Approvals(Ok(Vec::new())));
        assert_eq!(page.badge_number(), 0);
        assert!(!page.needs_attention());
    }

    /// **The presentation watch is wired, and its map/unmap half works** —
    /// the window realizes, resolves a `GdkToplevel`, seeds the watch from
    /// its state, and drops to `false` when the window goes away.
    ///
    /// What this cannot reach is the **SUSPENDED flip itself**: only a
    /// compositor sets that bit, `gdk` exposes no setter (the property is
    /// read-only, fed from the xdg-shell configure), and CI has no compositor
    /// — xvfb is a bare X server with not even a window manager. So the flip
    /// is a live-verify line (`docs/live-verify.md`, the #1149 entries), and
    /// what CI pins instead is split in two: the *policy* hermetically
    /// ([`super::tests::only_a_suspended_toplevel_parks_the_poll`]), and the
    /// *wiring up to the toplevel* here. The poll's own reaction to the bool
    /// is `tests/feed.rs`'s, driven through the same `watch::Receiver` this
    /// returns.
    ///
    /// Mutation (verified red): drop the `connect_unmap` arm in
    /// [`super::watch_presentation`] and the last assertion reds; drop the
    /// `connect_realize`/`connect_map` pair and the first two do.
    #[gtk::test]
    fn the_presentation_watch_follows_map_and_unmap() {
        let (w, _rx) = window();
        let rx = super::watch_presentation(w.toplevel());
        assert!(
            !*rx.borrow(),
            "an unmapped window must not be reported as presenting"
        );

        w.toplevel().present();
        while gtk::glib::MainContext::default().iteration(false) {}
        assert!(
            w.toplevel().is_mapped(),
            "the test window never mapped — nothing below would mean anything"
        );
        assert!(
            w.toplevel()
                .surface()
                .and_downcast::<gtk::gdk::Toplevel>()
                .is_some(),
            "a realized window must resolve a GdkToplevel — without one the poll \
             cannot park on suspension at all"
        );
        assert!(
            *rx.borrow(),
            "a mapped, unsuspended window must be reported as presenting"
        );

        w.toplevel().set_visible(false);
        while gtk::glib::MainContext::default().iteration(false) {}
        assert!(
            !*rx.borrow(),
            "the unmap edge (teardown, and a hide on a compositor that allows one) must park"
        );
    }

    /// **A worker that dies without a verdict must not strand the window on
    /// the spinner forever** — #1274 L1, the fix for the arm the #1273
    /// adversarial review found, plus the #1287 review's MED: clearing the
    /// latch alone painted nothing (the operator still saw
    /// [`webview::verifying`]) and named two retry routes — the next poll, a
    /// second activation — that do not exist for an unchanged hive.
    ///
    /// The resolver panics instead of answering, reached through
    /// [`TrustResolver`] — the seam this crate's tests always inject through
    /// (see [`window`]'s doc) — rather than by mutating `begin_probe`'s
    /// spawned closure directly: `resolver(&asked)` panics before
    /// `answer.send(...)` runs, so the sender drops and `wait.await` resolves
    /// to `Err` exactly the way the reviewer's own mutation (make the worker
    /// drop the sender and panic instead of sending) does.
    ///
    /// Before #1274's fix this hung out its full `pump_until` budget — the
    /// `Err` arm returned without upgrading `me` or touching `self.probe`, so
    /// `load_page`'s `*page_loaded || probe.is_some()` early return latched
    /// forever and nothing could ever start a second attempt. That fix
    /// cleared the latch but this test's own retry half was vacuous against
    /// production: it hand-fed a second, byte-identical `Update::State` —
    /// precisely the input `feed.rs:552` exists to suppress on a real hive
    /// (`an_unchanged_hive_sends_one_state_not_one_per_poll`). It now sends
    /// no second update at all: the only thing that can start a second probe
    /// is the retry the `Err` arm queues against [`Window::retry_load_page`]
    /// (since the October 2026 audit's M1, through
    /// [`Window::schedule_retry`], which the unreachable gateway shares), and
    /// the pump below waits on that directly.
    #[gtk::test]
    fn a_worker_that_panics_clears_the_latch_and_the_scheduled_retry_starts_a_fresh_probe() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let panics: TrustResolver = Arc::new(move |_url: &str| {
            counted.fetch_add(1, Ordering::SeqCst);
            panic!("#1274 L1 mutation: the worker dies without a verdict");
        });
        let (w, _rx) = window_with_trust(panics);
        // The second death queues a third attempt; it must not fire in the
        // next test.
        let _quiet = RetryGuard(Rc::clone(&w));

        w.update(Update::State(up(row_at(0))));
        assert!(
            w.probing(),
            "the probe must start the moment a URL is named"
        );

        assert!(
            pump_until(Duration::from_secs(10), || !w.probing()),
            "the probe latch was never cleared after the worker died — the window is stranded on \
             the spinner forever (#1274 L1)"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "exactly one attempt so far"
        );

        let shown = w.page_child().expect("the slot is filled");
        assert!(
            !webview::is_verifying(&shown),
            "the operator is still looking at \"Verifying the hive's certificate…\" with no \
             probe running — the #1287 review's MED, unfixed"
        );
        assert!(
            webview::is_probe_died(&shown),
            "the slot should show the probe-died card, not something else"
        );

        // No second `Update::State` here — that would be the vacuous half of
        // the pre-#1287 test, hand-feeding exactly the input
        // `feed::poll_once` dedups on a real hive (`feed.rs:552`). The only
        // route that can start a fresh probe from here is the scheduled
        // retry, so wait on it directly.
        assert!(
            pump_until(Duration::from_secs(10), || w.probing()),
            "the scheduled retry never started a fresh probe"
        );
        assert!(
            pump_until(Duration::from_secs(10), || !w.probing()),
            "the second probe's worker died the same way and must clear the latch too"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "…and the retry actually ran the resolver a second time, with no update fed in \
             between"
        );
    }

    /// **A gateway that did not answer is asked again, and the page mounts
    /// once it does** — the October 2026 audit's M1.
    ///
    /// Before this, the unreachable answer mounted a pinless view and latched
    /// `page_loaded`, so the second answer below never happened: the window
    /// was the engine's error page until someone closed it. Three things are
    /// asserted, in the order the operator meets them:
    ///
    /// 1. the answer puts up the unreachable card, not a view, and queues a
    ///    retry;
    /// 2. a poll arriving meanwhile — the agent's status moved, which happens
    ///    every few seconds while it works — does **not** jump the queue: the
    ///    backoff is the retry's, not the agent's chattiness;
    /// 3. the queued retry, with nothing fed in, asks again, and that answer
    ///    mounts the page and latches it exactly as before.
    ///
    /// Mutations (run this round, red): drop `finish_probe`'s
    /// `if trust.unreachable` arm and (1) reds on a mounted view; drop
    /// `|| self.retry.borrow().is_some()` from `load_page` and (2) reds with
    /// a probe started by the poll; drop the `schedule_retry` call in that
    /// arm and (1)'s "next attempt is queued" reds — the card would sit there
    /// with nothing coming, which is M1 with a better sentence.
    #[gtk::test]
    fn an_unreachable_gateway_is_asked_again_and_the_page_mounts_once_it_answers() {
        let (trust, calls) = scripted_answers(vec![unreachable_answer(), tls::Resolved::default()]);
        let (w, _rx) = window_with_trust(trust);
        let _quiet = RetryGuard(Rc::clone(&w));

        w.update(Update::State(up(row_with_page())));
        assert!(
            pump_until(Duration::from_secs(10), || !w.probing()),
            "the probe never answered"
        );
        let card = w.page_child().expect("the slot is filled");
        assert!(
            webview::is_unreachable(&card),
            "a gateway that did not answer gets the card that says so, not a view under the \
             system store"
        );
        assert!(w.retrying(), "…and the next attempt is queued");

        w.update(Update::State(up(AgentStatusRow {
            status_text: Some("still working".to_owned()),
            ..row_with_page()
        })));
        assert!(
            !w.probing(),
            "a poll must not start a probe while a retry is queued — the backoff would be \
             whatever the agent's status line made it"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        assert!(
            pump_until(Duration::from_secs(10), || w.probing()),
            "the queued retry never asked again — the window is latched on a gateway that is \
             down (M1)"
        );
        assert!(
            pump_until(Duration::from_secs(10), || !w.probing()),
            "the retry's probe never answered"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let mounted = w.page_child().expect("the slot is filled");
        assert!(
            webview::view_of(&mounted).is_some(),
            "the gateway answered, so the page mounts"
        );
        assert!(!w.retrying(), "nothing is queued behind a mounted page");

        // …and it latches as it always did.
        w.update(Update::State(up(row_with_page())));
        assert_eq!(
            mounted,
            w.page_child().expect("the slot is still filled"),
            "the page mounted after a retry survives a poll like any other"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2, "…and no third probe ran");
    }

    /// **"Try again now" asks at once, and takes the queued retry with it.**
    ///
    /// The card's button is for the operator who has just restarted the
    /// gateway: it must start the probe synchronously — not at the end of the
    /// backoff step — and the step it skipped must not fire as well. A second
    /// miss queues the next step again.
    ///
    /// Mutation (run this round, red): make `retry_now` skip
    /// `cancel_retry` — the queued source still latches `load_page`, so the
    /// click starts nothing and the `probing()` assertion reds.
    #[gtk::test]
    fn try_again_now_asks_at_once_and_drops_the_queued_retry() {
        let (trust, calls) = scripted_answers(vec![unreachable_answer()]);
        let (w, _rx) = window_with_trust(trust);
        let _quiet = RetryGuard(Rc::clone(&w));

        w.update(Update::State(up(row_with_page())));
        assert!(
            pump_until(Duration::from_secs(10), || !w.probing()),
            "the probe never answered"
        );
        assert!(w.retrying(), "a retry is queued behind the card");
        let button = w
            .page_child()
            .and_downcast::<adw::StatusPage>()
            .and_then(|card| card.child())
            .and_downcast::<gtk::Button>()
            .expect("the unreachable card carries its button");

        button.emit_clicked();
        assert!(
            w.probing(),
            "the click must ask the gateway now, not at the end of the backoff step"
        );
        assert!(
            !w.retrying(),
            "…and the step it skipped is gone, not still waiting to fire"
        );

        assert!(
            pump_until(Duration::from_secs(10), || !w.probing()),
            "the clicked probe never answered"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            webview::is_unreachable(&w.page_child().expect("the slot is filled")),
            "still down: the card stays up"
        );
        assert!(w.retrying(), "…and the next step is queued");
    }

    /// **A slow hive does not freeze the window** — #1246, the whole of it.
    ///
    /// The peer is #1242's review fixture: a legal TLS record header
    /// announcing 0x0400 bytes, then one byte every 1.5 s. Every byte resets
    /// GIO's per-I/O timeout, so nothing but the deadline ends it — which on
    /// the old shape meant the GTK main thread sat in `connect_to_host` +
    /// `handshake` for the whole budget, painting nothing.
    ///
    /// Two observables, and the second is the one that matters: the page slot
    /// carries the verifying state **synchronously**, before a single main-loop
    /// iteration; and a 20 ms heartbeat on that same main context advances
    /// while the probe is in flight.
    ///
    /// Mutation (run this round, red): put the probe back inline — replace
    /// `self.begin_probe(page::embed_url(url))` in `load_page` with
    /// `self.finish_probe(&embedded, &(self.trust)(&embedded))`. `update`
    /// then blocks for the whole budget with the loop stopped, `probing()` is
    /// already false when `pump_until` is reached, so **no iteration happens
    /// at all**: the tick count stays at 0 and the verifying assertion reds
    /// too (the slot holds the answer, never the state).
    ///
    /// The answer itself is the unreachable card, not the page: this peer
    /// never finishes a handshake, so the probe runs out its budget, and
    /// since the October 2026 audit's M1 that verdict waits for the gateway
    /// rather than mounting a view the gateway could never rescue.
    #[gtk::test]
    fn a_slow_hive_paints_the_verifying_state_and_the_main_loop_keeps_running() {
        // The probe's worker and the fixture's server thread are two threads
        // touching GIO types beside this one (#1461).
        glib_ready();
        let port = serve_dribbling(Duration::from_secs(12), Duration::from_millis(1500));
        let (trust, runs) = scripted_trust(Duration::from_millis(900));
        let (w, _rx) = window_with_trust(trust);
        let _quiet = RetryGuard(Rc::clone(&w));
        // `_heartbeat_guard` outlives every assertion below, including a
        // panicking one — it stops the source on drop, at the end of this
        // function's scope, whichever way that scope ends (#1274 N2).
        let (ticks, _heartbeat_guard) = heartbeat(Duration::from_millis(20));

        w.update(Update::State(up(row_at(port))));

        assert!(
            w.probing(),
            "the probe must be in flight the moment the hive names a URL"
        );
        let shown = w.page_child().expect("the slot is filled");
        assert!(
            webview::is_verifying(&shown),
            "…and the slot must explain itself rather than sit blank or hold a page that is not \
             verified yet"
        );
        assert_eq!(
            ticks.get(),
            0,
            "nothing has been dispatched yet — everything below is what the probe let through"
        );

        let answered = pump_until(Duration::from_secs(20), || !w.probing());
        assert!(answered, "the probe never answered");

        let (_, on_worker) = first_run(&runs);
        assert!(
            ticks.get() >= 10,
            "the main loop dispatched {} times during a {on_worker:.1?} probe — a window that \
             cannot paint is the freeze #1246 exists to remove",
            ticks.get()
        );
        assert!(
            webview::is_unreachable(&w.page_child().expect("the slot is filled")),
            "the worker's answer replaces the verifying state — here with the card that says the \
             gateway did not answer"
        );
    }

    /// **The deadline still bounds the probe, now on the worker** — it moved
    /// with it rather than being replaced by it.
    ///
    /// The peer dribbles for 30 s; the budget is 700 ms. The assertion is
    /// generous (5 s) so it measures the mechanism and not CI's scheduler, and
    /// it still sits far below what an un-deadlined probe against this peer
    /// produces. Both halves are asserted: what the *worker* took, and what
    /// the *window* waited — a deadline that bounded the worker while the main
    /// thread waited on the `oneshot` anyway would pass the first and fail the
    /// second.
    ///
    /// Mutation (run this round, red): make `Deadline::arm`'s watchdog never
    /// `cancel()`, and both elapsed assertions red at ~30 s.
    #[gtk::test]
    fn the_deadline_bounds_a_probe_that_never_finishes_on_the_worker() {
        // The probe's worker and the fixture's server thread are two threads
        // touching GIO types beside this one (#1461).
        glib_ready();
        let port = serve_dribbling(Duration::from_secs(30), Duration::from_millis(200));
        let budget = Duration::from_millis(700);
        let (trust, runs) = scripted_trust(budget);
        let (w, _rx) = window_with_trust(trust);
        let _quiet = RetryGuard(Rc::clone(&w));

        let started = Instant::now();
        w.update(Update::State(up(row_at(port))));
        assert!(
            pump_until(Duration::from_secs(20), || !w.probing()),
            "the probe never answered"
        );
        let waited = started.elapsed();
        let (resolved, on_worker) = first_run(&runs);

        assert!(
            on_worker < Duration::from_secs(5),
            "a peer that keeps talking must not hold the worker: {on_worker:.1?} against a \
             {budget:.1?} budget"
        );
        assert!(
            waited < Duration::from_secs(5),
            "…and the window must learn about it just as soon: {waited:.1?}"
        );
        assert_eq!(
            resolved.policy,
            tls::TlsPolicy::SystemStore,
            "a probe that ran out of time pins nothing"
        );
        let tried = resolved
            .tried
            .as_deref()
            .expect("the card says what happened");
        assert!(tried.contains("the probe was cancelled after"), "{tried}");
        assert!(
            resolved.unreachable,
            "a probe the deadline cut short never judged a chain — that is reachability, not trust"
        );
        // Until the October 2026 audit's M1 this asserted the opposite — "a
        // failed probe still mounts the view, so WebKit's own load can fail
        // into the card" — and that view, pinless and latched, was what kept
        // a gateway that came back from ever being asked again.
        assert!(
            webview::is_unreachable(&w.page_child().expect("the slot is filled")),
            "a probe that ran out of time says so and waits to ask again, rather than mounting a \
             view no later answer could rescue"
        );
        assert!(w.retrying(), "…and the next attempt is queued");
    }

    /// **One activation, one probe, one connection** — #1246's single
    /// in-flight rule, asked of the peer rather than of the window.
    ///
    /// The second activation is what `main.rs` does on a running instance
    /// (`HANDLES_COMMAND_LINE`: parse, `show_tab`, `present`) with the 2 s
    /// poll still arriving behind it. Both routes reach
    /// [`Window::load_page`], and without the in-flight slot each would open
    /// its own TLS connection to a gateway that is already not answering.
    ///
    /// The listener counts accepts, so the claim is about sockets and not
    /// about what the window believes it did. It keeps accepting and dribbles
    /// each connection on its own thread, so a second probe would hang exactly
    /// like the first rather than be answered quickly and hide the fault.
    ///
    /// # The count is read while the probe is the only thing dialling
    ///
    /// Worth recording because it corrects a standing note in this crate:
    /// once the view is mounted the accept count reaches **2**. That second
    /// connection is not a second probe — the resolver ran exactly once,
    /// asserted below — it is the embedded view's own `load_uri` reaching
    /// `WebKitGTK`'s **network** process, which is a different process from
    /// the web process that dies under xvfb (see `webview.rs`'s `gtk_tests`
    /// module doc, which now carries this same correction).
    ///
    /// Previously documented as "within ~300 ms", measured under a mutation
    /// at 50–75 ms — a real cross-process race with the read as the
    /// literally-next statement after `pump_until`, not a defined instant
    /// (#1274 N1). `seen` below removes the clock instead of re-measuring it:
    /// `pump_until`'s own predicate snapshots the accept count on every
    /// iteration `probing()` is still `true`, which is strictly pre-mount by
    /// construction — `finish_probe` clears `probe` **before** it mounts the
    /// view, so the last snapshot taken while `probing()` still held is
    /// pinned before any `load_uri` the mount could trigger, with no
    /// wall-clock number to go stale under a slower or faster driver.
    ///
    /// Since the October 2026 audit's M1 this probe's answer (its budget runs
    /// out against the dribbler) mounts no view at all — it puts up the
    /// unreachable card and queues a retry one poll interval out — so the
    /// next thing to dial this peer is that retry, not `WebKit`. The snapshot
    /// is pre-answer either way, and the 300 ms wait below sits well inside
    /// the retry's 2 s.
    ///
    /// Mutation (run this round, red): drop `|| self.probe.borrow().is_some()`
    /// from `load_page`'s early return — the poll behind the activation starts
    /// a second probe, both are in flight when the pump waits on `probing()`,
    /// and the accept count is 2 before either mounts anything.
    #[gtk::test]
    fn a_second_activation_during_the_probe_opens_no_second_connection() {
        // The probe's worker and the fixture's server thread are two threads
        // touching GIO types beside this one (#1461).
        glib_ready();
        let (port, accepted) = serve_counting_dribbler(Duration::from_millis(200));
        let (trust, runs) = scripted_trust(Duration::from_millis(900));
        let (w, _rx) = window_with_trust(trust);
        let _quiet = RetryGuard(Rc::clone(&w));

        w.update(Update::State(up(row_at(port))));
        assert!(w.probing(), "the first probe is in flight");

        // The pen's `--tab settings` on the live instance, and the poll that
        // arrives while the probe runs.
        w.show_tab(Tab::Settings);
        w.update(Update::State(up(AgentStatusRow {
            status_text: Some("still working".to_owned()),
            ..row_at(port)
        })));

        // Snapshot the accept count on every iteration the probe is still in
        // flight, so the last write is strictly pre-mount — see "The count is
        // read while the probe is the only thing dialling" above (#1274 N1).
        let seen = Cell::new(0_usize);
        assert!(
            pump_until(Duration::from_secs(20), || {
                if w.probing() {
                    seen.set(accepted.load(Ordering::SeqCst));
                    false
                } else {
                    true
                }
            }),
            "the probe never answered"
        );

        assert_eq!(
            seen.get(),
            1,
            "one window, one launch-time probe, one connection — the activation and the poll \
             behind it must not each open their own"
        );
        // …and let anything a second probe would have queued actually run.
        assert!(
            !pump_until(Duration::from_millis(300), || runs_so_far(&runs) > 1),
            "a second probe was queued behind the first"
        );
        assert_eq!(runs_so_far(&runs), 1, "…and the resolver ran exactly once");
        assert_eq!(
            w.visible_tab().as_deref(),
            Some(Tab::Settings.as_str()),
            "the later activation's --tab survives the probe it did not restart"
        );
        assert!(
            webview::is_unreachable(&w.page_child().expect("the slot is filled")),
            "and the one probe's answer still reaches the slot"
        );
    }

    /// **The happy path is what it was** — route 2 verifies the chain the
    /// gateway presents, pins the leaf it accepted, and the page mounts under
    /// that policy. The only thing #1246 changed is which thread found out.
    ///
    /// It also re-pins #1130's M13 latch across the new two-step mount: a poll
    /// arriving after the answer must not rebuild the view, and must not
    /// re-probe. The fixture server accepts exactly **one** connection, so a
    /// second probe would hang rather than quietly succeed.
    #[gtk::test]
    fn the_verified_leaf_is_pinned_and_the_page_mounts_once() {
        // The probe's worker and the fixture's server thread are two threads
        // touching GIO types beside this one (#1461).
        glib_ready();
        let port = serve("server-leaf.pem", "server-leaf-key.pem");
        let (trust, runs) = scripted_trust(Duration::from_secs(30));
        let (w, _rx) = window_with_trust(trust);

        w.update(Update::State(up(row_at(port))));
        assert!(
            pump_until(Duration::from_secs(30), || !w.probing()),
            "the probe never answered"
        );

        let (resolved, _) = first_run(&runs);
        let tls::TlsPolicy::AllowCertificateForHost {
            cert: tls::Pinned::VerifiedPem(pem),
            host,
        } = &resolved.policy
        else {
            panic!("route 2 pins what it verified: {:?}", resolved.policy);
        };
        assert_eq!(host, "localhost");
        assert!(pem.contains("BEGIN CERTIFICATE"), "{pem}");

        let mounted = w.page_child().expect("the slot is filled");
        assert!(
            webview::view_of(&mounted).is_some(),
            "the page mounts under the policy the worker settled on"
        );

        w.update(Update::State(up(row_at(port))));
        assert_eq!(
            mounted,
            w.page_child().expect("the slot is still filled"),
            "the embedded page must survive a poll — the latch is the same one #1130 added"
        );
        assert_eq!(runs_so_far(&runs), 1, "…and no second probe ran");
    }

    /// **A retry that comes due while the window is hidden parks, and runs
    /// the moment the window is shown again** — #1557 item 3.
    ///
    /// The compositor's `SUSPENDED` flip cannot be produced here (see
    /// [`the_presentation_watch_follows_map_and_unmap`] for why), so the
    /// window is handed a `watch` it does not own, through
    /// [`Window::follow_presentation`] — the same call `Window::build` makes
    /// with [`super::watch_presentation`]'s receiver — and the test plays the
    /// compositor on the sending end.
    ///
    /// Mutations (run this round, red): make `retry_due` run the attempt
    /// whatever `presented` says — the retry as it shipped — and the second
    /// probe runs while hidden (`calls` reds at 2); drop the
    /// `presentation_changed` call from `follow_presentation`'s loop and the
    /// window never learns it was hidden, so the same assertion reds the same
    /// way.
    #[gtk::test]
    fn a_retry_due_while_hidden_parks_and_runs_when_shown_again() {
        let (trust, calls) = scripted_answers(vec![unreachable_answer()]);
        let (w, _rx) = window_with_trust(trust);
        let _quiet = RetryGuard(Rc::clone(&w));
        let (shown, visible) = tokio::sync::watch::channel(true);
        w.follow_presentation(visible);

        w.update(Update::State(up(row_with_page())));
        assert!(
            pump_until(Duration::from_secs(10), || !w.probing()),
            "the probe never answered"
        );
        assert_eq!(
            w.waiting(),
            Some((super::Attempt::Probe, false)),
            "the gateway did not answer, so a probe is queued on its timer"
        );

        shown.send(false).expect("the window follows the watch");
        // The timer coming due shows either way: parked, or — the bug —
        // a second probe.
        assert!(
            pump_until(Duration::from_secs(10), || {
                calls.load(Ordering::SeqCst) > 1 || w.waiting().is_some_and(|(_, parked)| parked)
            }),
            "the retry's timer never came due"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a hidden window's retry dials nothing — it parks"
        );
        assert_eq!(w.waiting(), Some((super::Attempt::Probe, true)), "parked");
        assert!(!w.probing());
        // Parked still holds `load_page`'s latch, so a poll arriving meanwhile
        // (the feed parks too, but a command's reconciling poll does not) does
        // not start the probe behind the retry's back.
        w.update(Update::State(up(AgentStatusRow {
            status_text: Some("still working".to_owned()),
            ..row_with_page()
        })));
        assert!(!w.probing(), "a parked retry still latches the page");

        shown.send(true).expect("the window follows the watch");
        assert!(
            pump_until(Duration::from_secs(2), || w.probing()),
            "shown again, the parked retry runs at once"
        );
        assert!(
            pump_until(Duration::from_secs(10), || !w.probing()),
            "the resumed probe never answered"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            w.waiting(),
            Some((super::Attempt::Probe, false)),
            "still down: the next step is queued on its timer again"
        );
    }

    /// **A burst of edges back runs one waiting retry, not one per edge** —
    /// #1557 item 3, the flap guard the poll uses, asked of the retry.
    ///
    /// Every workspace switch across the window is an edge; without the guard
    /// each one would be an immediate TLS probe against a gateway that is
    /// down. The first edge back runs the waiting retry at once; a second one
    /// inside the poll interval leaves the newly queued step on its timer.
    ///
    /// Mutation (run this round, red): make `on_resume` answer `RunNow`
    /// without asking `feed::resume_acts_now` and the second edge starts a
    /// third probe (`probing()` reds).
    #[gtk::test]
    fn a_burst_of_edges_back_runs_one_waiting_retry() {
        let (trust, calls) = scripted_answers(vec![unreachable_answer()]);
        let (w, _rx) = window_with_trust(trust);
        let _quiet = RetryGuard(Rc::clone(&w));
        let (shown, visible) = tokio::sync::watch::channel(true);
        w.follow_presentation(visible);
        let flip = |to: bool| {
            shown.send(to).expect("the window follows the watch");
            assert!(
                pump_until(Duration::from_secs(2), || w.presented.get() == to),
                "the edge to {to} never reached the window"
            );
        };

        w.update(Update::State(up(row_with_page())));
        assert!(pump_until(Duration::from_secs(10), || !w.probing()));
        assert!(w.retrying(), "the first step is queued");

        flip(false);
        flip(true);
        assert!(
            w.probing(),
            "the first edge back asks at once, ahead of the timer"
        );
        assert!(pump_until(Duration::from_secs(10), || !w.probing()));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(w.retrying(), "still down: the next step is queued");

        flip(false);
        flip(true);
        assert!(
            !w.probing(),
            "a second edge inside one interval is a flap: it rides the queued step"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(w.retrying(), "…which is still waiting on its timer");
    }

    /// **A mounted page whose load nothing answered is reloaded on the
    /// shared backoff, and the backoff starts over once it answers** —
    /// #1557 item 2, the window's half.
    ///
    /// The page is mounted under the system store — the route with no probe
    /// — and the page's report is delivered straight to
    /// [`Window::on_gateway`], because no web process runs where these tests
    /// do (`webview.rs`'s `gtk_tests` module doc); `webview.rs`'s own tests
    /// drive the signal half. The reload is observed through
    /// `WebViewExt::uri`, which `load_uri` sets before any decision is made.
    ///
    /// Mutations (run this round, red): drop `on_gateway`'s
    /// `schedule_retry` and nothing is queued (the `waiting` assertion reds);
    /// drop its `unanswered.set(0)` on `Answered` and the backoff reds at 2;
    /// drop its `cancel_retry` there and a reload stays queued behind a page
    /// that came back.
    #[gtk::test]
    fn an_unanswered_page_is_reloaded_on_the_shared_backoff() {
        let (w, _rx) = window();
        let _quiet = RetryGuard(Rc::clone(&w));
        w.update(Update::State(up(row_with_page())));
        assert!(pump_until(Duration::from_secs(10), || !w.probing()));
        let page = w.page_child().expect("the slot is filled");
        let view = webview::view_of(&page).expect("the system store mounts the page at once");

        let again = "https://hive.local/agent/stray/turn/3";
        w.on_gateway(webview::Gateway::Unanswered {
            uri: again.to_owned(),
            why: "Could not connect: Connection refused".to_owned(),
        });
        assert!(
            webview::shows_unanswered(&page),
            "the card stands in front of the view"
        );
        assert_eq!(
            w.waiting(),
            Some((super::Attempt::Reload(again.to_owned()), false)),
            "a reload of the URI that failed is queued"
        );
        assert_eq!(
            w.unanswered.get(),
            1,
            "…on the same backoff the probe retry counts on"
        );
        assert_eq!(
            page,
            w.page_child().expect("the slot is still filled"),
            "the view is kept, not rebuilt"
        );

        assert!(
            pump_until(Duration::from_secs(10), || !w.retrying()),
            "the queued reload never fired"
        );
        assert_eq!(
            webkit::prelude::WebViewExt::uri(&view).as_deref(),
            Some(again),
            "the reload loads the failed URI in the same view"
        );

        // Still down: the next failure waits a step longer.
        w.on_gateway(webview::Gateway::Unanswered {
            uri: again.to_owned(),
            why: "Could not connect: Connection refused".to_owned(),
        });
        assert_eq!(w.unanswered.get(), 2, "the backoff grows on a second miss");

        // …and the hive answers.
        w.on_gateway(webview::Gateway::Answered);
        assert!(
            !w.retrying(),
            "nothing stays queued behind a page that came back"
        );
        assert_eq!(w.unanswered.get(), 0, "the backoff starts over");
    }
}
