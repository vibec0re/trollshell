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
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use hytte_plugin_agents::config::AgentsConfig;
use hytte_plugin_agents::hive::wire::{HiveUrls, Request};
use hytte_plugin_agents::model::{AgentName, agent_url};

use crate::chrome::{Controls, Facts, HeaderModel};
use crate::feed::{self, AgentState, Update};
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
            cmds,
        });

        let press = Rc::clone(&this);
        this.header.connect(move |p| press.on_press(p));
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
            Update::Refused { request, reason } => {
                self.banner.set_title(&ui::refusal(&request, &reason));
                self.banner.set_revealed(true);
            }
        }
    }

    /// Push the current state through the whole chrome.
    fn apply(&self) {
        let state = self.last.borrow();
        self.header.apply(
            &HeaderModel::of(&self.name, &self.cfg, &state),
            &Controls::of(&state),
        );
        self.settings.apply(
            &Facts::agent(&self.name, &state),
            &Facts::hive(&self.cfg, self.urls.borrow().as_ref()),
        );
        self.load_page(&state);
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
    use gtk::prelude::*;
    use hytte_plugin_agents::config::AgentsConfig;
    use hytte_plugin_agents::hive::wire::{AgentStatusRow, HiveUrls, Request, Scope};
    use hytte_plugin_agents::model::{Agent, AgentName};
    use std::rc::Rc;
    use tokio::sync::mpsc;

    fn name(s: &str) -> AgentName {
        AgentName::parse(s).expect("a legal test name")
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
}
