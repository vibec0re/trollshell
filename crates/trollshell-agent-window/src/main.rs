//! The `trollshell-agent-window` binary: parse the command line, register the
//! per-agent application, and build the window when `GApplication` says to.
//!
//! The window itself and everything it shows live in the library crate — see
//! its module docs for the design, and `cli::app_id` for why the application
//! id carries the agent.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;
use gtk::glib;

use hytte_plugin_agents::config::AgentsConfig;
use hytte_plugin_agents::hive::wire::{HiveUrls, Request};
use hytte_plugin_agents::model::{AgentName, agent_url};

use trollshell_agent_window::chrome::{Controls, Facts, HeaderModel};
use trollshell_agent_window::feed::{self, AgentState, Update};
use trollshell_agent_window::{cli, page, tls, ui, webview};

/// Default `tracing` level when `RUST_LOG` is unset — `INFO`, matching the
/// shell (#746) and the control center (#780). `fmt::init()`'s own fallback is
/// `ERROR`, and nothing on the launch path sets `RUST_LOG`, so without this
/// every diagnostic here would be discarded on a normal launch.
const DEFAULT_LOG_LEVEL: tracing_subscriber::filter::LevelFilter =
    tracing_subscriber::filter::LevelFilter::INFO;

/// Exit code for a command line this window cannot use.
const EXIT_USAGE: u8 = 2;

/// What the window shows before the hive has a page URL for this agent.
const NO_PAGE: &str = "This hive publishes no page for this agent yet — its domain is unconfigured, or the agent is \
     not on its roster. The header above still follows the agent's live status.";

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(DEFAULT_LOG_LEVEL.into())
                .from_env_lossy(),
        )
        .init();

    let command_line: Vec<String> = std::env::args().collect();
    // Parsed **here**, before the application exists, because the app id is
    // derived from `--agent` and `Application::new` aborts on an invalid one.
    let args = match cli::parse(&command_line[1..]) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("trollshell-agent-window: {e}\n{}", cli::USAGE);
            return glib::ExitCode::from(EXIT_USAGE);
        }
    };

    // The window's own runtime: it links no `hytte-reactive`, so there is no
    // process-wide one to borrow. Two worker threads is one socket poll and
    // room for the write that interrupts it.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("trollshell-agent-window: could not start a runtime: {e}");
            return glib::ExitCode::FAILURE;
        }
    };

    let app = adw::Application::builder()
        .application_id(cli::app_id(&args.agent))
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let state: Rc<RefCell<Option<Rc<Window>>>> = Rc::new(RefCell::new(None));
    let handle = runtime.handle().clone();
    app.connect_command_line(move |app, cmd| {
        let args: Vec<String> = cmd
            .arguments()
            .iter()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let args = match cli::parse(&args) {
            Ok(args) => args,
            Err(e) => {
                // The *remote* process learns this through the exit code this
                // returns; `print_literal`, which would put the sentence on
                // its own stderr, is gated on glib's `v2_80` feature and
                // enabling that would widen the whole workspace's glib floor
                // for one diagnostic. The sentence goes to the journal
                // instead, where this window's other diagnostics already are.
                tracing::warn!(error = %e, "{}", cli::USAGE);
                return glib::ExitCode::from(EXIT_USAGE);
            }
        };

        let mut slot = state.borrow_mut();
        let window = slot.get_or_insert_with(|| Window::build(app, &args.agent, &handle));
        window.show_tab(args.tab);
        window.toplevel.present();
        glib::ExitCode::SUCCESS
    });

    app.run_with_args(&command_line)
}

/// One agent's window: the chrome, the page, and the lane its buttons write
/// to.
struct Window {
    toplevel: adw::ApplicationWindow,
    stack: adw::ViewStack,
    header: ui::Header,
    settings: ui::Settings,
    banner: adw::Banner,
    /// Rebuilt once, when the hive first hands over a URL for this agent —
    /// the row carries it (hyperhive#4073) and a fresh hive may not have one
    /// yet.
    page_slot: gtk::Box,
    page_loaded: RefCell<bool>,
    cfg: AgentsConfig,
    name: AgentName,
    urls: RefCell<Option<HiveUrls>>,
    last: RefCell<AgentState>,
    cmds: tokio::sync::mpsc::UnboundedSender<Request>,
}

impl Window {
    fn build(
        app: &adw::Application,
        name: &AgentName,
        runtime: &tokio::runtime::Handle,
    ) -> Rc<Self> {
        let cfg = hytte_plugin_agents::config::load();

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

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

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
            cmds: cmd_tx,
        });

        let press = Rc::clone(&this);
        this.header.connect(move |p| press.on_press(p));
        this.apply();

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
                pump.on_update(update);
            }
        });

        this
    }

    /// Switch to one tab. Silently a no-op if the stack has no such child,
    /// which cannot happen — `Tab` and the child names are one enum.
    fn show_tab(&self, tab: cli::Tab) {
        self.stack.set_visible_child_name(tab.as_str());
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

    fn on_update(&self, update: Update) {
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
    /// Once only: a reload on every poll would throw away the scroll position
    /// and any half-typed message on the page, twice a second.
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
        let policy = tls::policy(std::env::var(tls::CA_ENV).ok().as_deref(), &embedded);
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
}
