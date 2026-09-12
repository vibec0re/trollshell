//! One agent's window: the chrome, the page, and the lane its buttons write
//! to.
//!
//! In the **library** rather than in `main.rs` because nothing could reach it
//! there (#1130 L2): deleting the `page_loaded` latch — so the embedded view
//! is rebuilt on every poll, throwing away the scroll position and any
//! half-typed message twice a second, the exact thing the latch exists for —
//! changed no test. The same hole covered the `NO_PAGE` hint, the banner, the
//! tab forwarding and the refusal path. `main.rs` is now the command line, the
//! application and one call into here; [`gtk_tests`] drives [`Window::update`]
//! and asserts what it did.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use hytte_plugin_agents::config::AgentsConfig;
use hytte_plugin_agents::hive::wire::{Approval, HiveUrls, Request};
use hytte_plugin_agents::model::{AgentName, agent_url};

use crate::chrome::{self, Controls, Facts, HeaderModel};
use crate::feed::{self, AgentState, Update};
use crate::ui::Decision;
use crate::{cli, page, tls, ui, webview};

/// What the window shows before the hive has a page URL for this agent.
pub const NO_PAGE: &str = "This hive publishes no page for this agent yet — its domain is unconfigured, or the agent is \
     not on its roster. The header above still follows the agent's live status.";

/// One agent's window.
pub struct Window {
    toplevel: adw::ApplicationWindow,
    stack: adw::ViewStack,
    header: ui::Header,
    settings: ui::Settings,
    banner: adw::Banner,
    /// Filled once, when the hive first hands over a URL for this agent — the
    /// row carries it (hyperhive#4073) and a fresh hive may not have one yet.
    page_slot: gtk::Box,
    page_loaded: RefCell<bool>,
    cfg: AgentsConfig,
    name: AgentName,
    urls: RefCell<Option<HiveUrls>>,
    last: RefCell<AgentState>,
    /// The agent's `Pending` queue, already narrowed to this agent and
    /// sorted oldest-first (`chrome::pending_for`) — #1141. What
    /// `Window::on_decision` checks a click's id against before it becomes a
    /// frame, and what `Window::apply` renders.
    pending: RefCell<Vec<Approval>>,
    /// The hive's own reason the **last** decision for an approval was
    /// refused, keyed by id. Pruned to the ids `pending` still carries on
    /// every `Update::Approvals` — an approval that left the queue has
    /// nothing left to keep showing a reason for.
    approval_refusals: RefCell<BTreeMap<i64, String>>,
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
    /// It empties two ways, both of them the end of the round trip: the id
    /// leaves the polled queue (pruned on `Update::Approvals`), or the hive
    /// refuses the write (`Update::Refused`) — which is precisely why the
    /// refusal path keeps the row, so a retry is possible.
    in_flight: RefCell<BTreeSet<i64>>,
    /// The hive's own reason the **queue itself** could not be read, when the
    /// last `Pending` came back refused (#1146's review, M1).
    ///
    /// `None` is "the hive answered" — which, with an empty queue, is the
    /// group hiding itself. A refusal has to look different: without this,
    /// an older daemon or a permissions change rendered exactly like a
    /// healthy hive with nothing to decide.
    approvals_refused: RefCell<Option<String>>,
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

        runtime.spawn(feed::run(
            std::path::PathBuf::from(&this.cfg.socket),
            name.clone(),
            this.cfg.poll_interval(),
            cmd_rx,
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
    #[must_use]
    pub fn assemble(
        app: &adw::Application,
        name: &AgentName,
        cfg: AgentsConfig,
        cmds: tokio::sync::mpsc::UnboundedSender<Request>,
    ) -> Rc<Self> {
        let header = ui::Header::new();
        let settings = ui::Settings::new();
        let banner = adw::Banner::new("");
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

        let this = Rc::new(Self {
            toplevel,
            stack,
            header,
            settings,
            banner,
            page_slot,
            page_loaded: RefCell::new(false),
            cfg,
            name: name.clone(),
            urls: RefCell::new(None),
            last: RefCell::new(AgentState::Connecting),
            pending: RefCell::new(Vec::new()),
            approval_refusals: RefCell::new(BTreeMap::new()),
            in_flight: RefCell::new(BTreeSet::new()),
            approvals_refused: RefCell::new(None),
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
    /// 1. the id must still be in the last-polled queue
    ///    ([`chrome::should_send`] — an approval that left `Pending` between
    ///    the render and the click raced a poll, and is dropped with a debug
    ///    line, not sent);
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
        let id = decision.id();
        if !chrome::should_send(&self.pending.borrow(), id) {
            tracing::debug!(
                approval = id,
                "the approval left the queue before this window could send a decision; dropping it"
            );
            return;
        }
        if !self.in_flight.borrow_mut().insert(id) {
            tracing::debug!(
                approval = id,
                "a decision for this approval is already in flight; dropping the second one"
            );
            return;
        }
        self.approval_refusals.borrow_mut().remove(&id);
        let req = match decision {
            Decision::Approve(id) => feed::approve(id),
            Decision::Deny(id) => feed::deny(id),
        };
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
                *self.last.borrow_mut() = state;
                self.apply();
            }
            Update::Urls(urls) => {
                *self.urls.borrow_mut() = Some(*urls);
                self.apply();
            }
            // The hive answered the queue: rebuild the rows from it.
            Update::Approvals(Ok(queue)) => {
                let pending = chrome::pending_for(&self.name, queue);
                // Prune refusal reasons **and in-flight ids** to the ones the
                // fresh queue still carries — a row that left the queue has
                // nothing left to explain and nothing left to wait on. That
                // prune is also how a decision's latch is released: the poll
                // that takes the approval out of `Pending` is the end of the
                // round trip (#1146's review, H1/L2).
                let live: BTreeSet<i64> = pending.iter().map(|a| a.id).collect();
                self.approval_refusals
                    .borrow_mut()
                    .retain(|id, _| live.contains(id));
                self.in_flight.borrow_mut().retain(|id| live.contains(id));
                *self.pending.borrow_mut() = pending;
                *self.approvals_refused.borrow_mut() = None;
                self.apply();
            }
            // The hive refused the queue (#1146's review, M1). The rows go —
            // this window can no longer vouch for them — but the group stays,
            // saying so, instead of hiding and looking like an empty queue.
            Update::Approvals(Err(reason)) => {
                self.approval_refusals.borrow_mut().clear();
                self.in_flight.borrow_mut().clear();
                self.pending.borrow_mut().clear();
                *self.approvals_refused.borrow_mut() = Some(reason);
                self.apply();
            }
            Update::Refused { request, reason } => match &request {
                // #1141: an Approve/Deny refusal is shown **inline**, on the
                // row it names — a global banner would say "the hive refused
                // that" beside a row that still shows live buttons, which
                // does not say *which* decision failed once more than one is
                // queued.
                Request::Approve { id } | Request::Deny { id } => {
                    // Only record the reason if the row is still pending —
                    // a refusal that arrives after the row already left the
                    // queue (the poll pruned it first) has nothing left to
                    // decorate, and inserting anyway would plant an orphan
                    // entry that a later poll bringing the same id back would
                    // read as "the decision just sent for you failed"
                    // (#1146 re-verify, M-NEW-1).
                    if self.pending.borrow().iter().any(|a| a.id == *id) {
                        self.approval_refusals.borrow_mut().insert(*id, reason);
                    }
                    // The round trip ended in a no, so the row is answerable
                    // again — that is the whole reason a refusal keeps it
                    // (#1146's review, H1).
                    self.in_flight.borrow_mut().remove(id);
                    self.apply();
                }
                _ => {
                    self.banner.set_title(&ui::refusal(&request, &reason));
                    self.banner.set_revealed(true);
                }
            },
        }
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
        let rows: Vec<chrome::ApprovalRow> = self
            .pending
            .borrow()
            .iter()
            .map(|a| {
                chrome::ApprovalRow::of(a, refusals.get(&a.id).cloned(), in_flight.contains(&a.id))
            })
            .collect();
        drop(in_flight);
        drop(refusals);
        let refused = self.approvals_refused.borrow();
        self.settings.apply(
            &Facts::agent(&self.name, &state),
            &Facts::hive(&self.cfg, self.urls.borrow().as_ref()),
            &rows,
            refused.as_deref(),
        );
        drop(refused);
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
    /// [`gtk_tests::the_page_is_built_once_and_not_on_every_poll`].
    fn load_page(&self, state: &AgentState) {
        if *self.page_loaded.borrow() {
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

        let embedded = page::embed_url(url);
        let policy = tls::policy(std::env::var(tls::CERT_ENV).ok().as_deref(), &embedded);
        tracing::info!(
            url = %embedded,
            tls_host = ?policy.scoped_host(),
            "loading the agent's page"
        );

        while let Some(child) = self.page_slot.first_child() {
            self.page_slot.remove(&child);
        }
        self.page_slot.append(&webview::page(&embedded, &policy));
        *self.page_loaded.borrow_mut() = true;
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

    /// The header, for the tests that assert what it shows.
    #[cfg(all(test, feature = "system-tests"))]
    fn header(&self) -> &ui::Header {
        &self.header
    }
}

#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use super::{NO_PAGE, Window};
    use crate::cli::Tab;
    use crate::feed::{AgentState, Update};
    use crate::ui::Decision;
    use gtk::prelude::*;
    use hytte_plugin_agents::config::AgentsConfig;
    use hytte_plugin_agents::hive::wire::{
        AgentStatusRow, Approval, ApprovalStatus, HiveUrls, Request, Scope,
    };
    use hytte_plugin_agents::model::{Agent, AgentName};
    use std::rc::Rc;
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

    /// An application object, never run — `adw::ApplicationWindow` wants one,
    /// and registering is what `run` would do, which these tests must not.
    fn app() -> adw::Application {
        adw::Application::builder()
            .application_id("mov.vibec0re.trollshell.AgentWindow.test")
            .build()
    }

    fn window() -> (Rc<Window>, mpsc::UnboundedReceiver<Request>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Window::assemble(&app(), &name("stray"), AgentsConfig::default(), tx),
            rx,
        )
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

    /// **The page is built once.** Two states carrying the same URL leave the
    /// *same widget* in the slot — not a fresh `WebView` that threw away the
    /// scroll position and the half-typed message.
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
        let first = w.page_child().expect("the page went into the slot");
        // **Not vacuously** (#1130 N6): `assemble`'s first `apply` puts the
        // `NO_PAGE` label in the slot, so without this the test would compare
        // that label with itself and stay green with `Window::update` mutated
        // to a no-op — measured by the re-verification.
        assert!(
            first.downcast_ref::<gtk::Label>().is_none(),
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
        let page = w.page_child().expect("the page replaced the hint");
        assert!(
            page.downcast_ref::<gtk::Label>().is_none(),
            "once the hive names a URL the slot holds the page, not the hint"
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
        assert_eq!(Scope::agent("stray").agent_names(), [String::from("stray")]);
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
    /// for `Decision::Approve` and the byte assertion reds.
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

        w.on_decision(Decision::Approve(5));
        assert!(
            rx.try_recv().is_err(),
            "an id the last poll no longer carries must not reach the hive"
        );
    }

    /// **A refused write keeps the row and shows why inline.** The row is
    /// still there, still bearing live buttons, and its subtitle now carries
    /// the hive's own reason — no global banner for this one (#1141).
    #[gtk::test]
    fn a_refused_approval_keeps_the_row_and_shows_the_reason_inline() {
        let (w, _rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(9, "stray")])));

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
        w.on_decision(Decision::Approve(42));

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
        w.on_decision(Decision::Deny(42));

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
        let (w, _rx) = window();
        w.update(Update::State(up(row())));
        w.update(Update::Approvals(Ok(vec![approval(9, "stray")])));
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
    /// **Fails on HEAD before the guard**; passes with the `pending`-still-
    /// contains-the-id guard on the insert.
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

    /// **A rebuild takes the old approval rows out of the group**, not just
    /// out of the bookkeeping — the reviewer's test (#1146's review, M2),
    /// taken as supplied.
    ///
    /// `Settings::apply` rebuilds the approvals on every state change, so a
    /// rebuild that only drained the `Vec` would leak every predecessor into
    /// the group — with **live Approve/Deny buttons on approvals that already
    /// left the queue** — and `approval_row_text` could not see it, because
    /// it reads back from the same `Vec` the drain empties. #1130's M4,
    /// reintroduced forty lines below the doc written about it.
    ///
    /// Mutation (verified red): delete the `self.root.remove(&w.row)` in
    /// `ui::Approvals::apply`, keeping the drain.
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
            text[0].starts_with(crate::ui::APPROVALS_REFUSED)
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
}
