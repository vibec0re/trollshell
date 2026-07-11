//! `App` and `AppBuilder` — the entry point for a hytte-based shell.
//!
//! The builder collects registered services and a one-shot body closure.
//! `run` constructs an `adw::Application`, connects an `activate` handler
//! that starts each service, installs the default stylesheet, and calls
//! the body once with an `&App` view.

use crate::error::{Error, Result};
use crate::monitor::Monitor;
use adw::prelude::*;
use futures_signals::signal::{Mutable, Signal};
use gtk::gdk;
use gtk::gio;
use hytte_reactive::registry::{self, ServiceErased};
use hytte_reactive::runtime;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Builder for an `App`. Registers services and an optional user CSS file
/// before `run` is called.
pub struct AppBuilder {
    app_id: String,
    services: Vec<Box<dyn ServiceErased>>,
    user_style: Option<PathBuf>,
}

impl AppBuilder {
    #[must_use]
    pub fn with<S: hytte_reactive::Service>(mut self, service: S) -> Self {
        self.services.push(Box::new(service));
        self
    }

    #[must_use]
    pub fn with_user_style(mut self, path: impl AsRef<Path>) -> Self {
        self.user_style = Some(path.as_ref().to_path_buf());
        self
    }

    /// Run the application. The body closure is invoked once on first
    /// activate; subsequent activates are no-ops.
    ///
    /// # Errors
    /// Returns `Error::NonZeroExit` if the GTK application exits with a
    /// non-zero status.
    pub fn run<F>(self, body: F) -> Result<()>
    where
        F: FnOnce(&App) + 'static,
    {
        adw::init().map_err(Error::GtkInit)?;

        // Default to dark for shell UI so libadwaita's @window_bg_color /
        // @card_bg_color / @borders / @accent_color resolve to dark values.
        // Without this, adwaita defaults to Default (follow system portal)
        // which on many systems falls back to light.
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);

        let inner = adw::Application::builder()
            .application_id(&self.app_id)
            .flags(gio::ApplicationFlags::default())
            .build();

        // Wrap the move-once state in `Rc<RefCell<Option<…>>>` so the
        // activate handler can `.take()` it on first fire.
        #[allow(clippy::type_complexity)]
        let body_cell: Rc<RefCell<Option<Box<dyn FnOnce(&App)>>>> =
            Rc::new(RefCell::new(Some(Box::new(body))));
        #[allow(clippy::type_complexity)]
        let services_cell: Rc<RefCell<Option<Vec<Box<dyn ServiceErased>>>>> =
            Rc::new(RefCell::new(Some(self.services)));
        let user_style = self.user_style;

        inner.connect_activate(move |inner_app| {
            // Hold the application alive without a regular toplevel. `hold()`
            // returns an `ApplicationHoldGuard` whose `Drop` releases the
            // hold; we leak it so the hold lasts the process lifetime.
            std::mem::forget(inner_app.hold());

            let Some(body_fn) = body_cell.borrow_mut().take() else {
                return;
            };
            let services = services_cell.borrow_mut().take().unwrap_or_default();

            install_default_css();
            if let Some(path) = user_style.as_ref() {
                install_user_css(path);
            }

            for service in services {
                registry::install(service, runtime::handle());
            }

            // Set up the monitors Mutable + listener BEFORE handing the
            // body the App, so the initial body sees the current set and
            // any later body subscription receives hot-plug updates.
            let monitors = Mutable::new(read_monitors());
            if let Some(display) = gdk::Display::default() {
                let model = display.monitors();
                let writer = monitors.clone();
                model.connect_items_changed(move |_, _, _, _| {
                    writer.set(read_monitors());
                });
            }

            let app = App {
                inner: inner_app.clone(),
                monitors,
            };
            body_fn(&app);
        });

        // Pass only argv[0] so the GTK/GIO option parser never sees Rust test
        // flags (--ignored, --test-threads, …) and does not exit non-zero.
        let argv0 = std::env::args().next().unwrap_or_default();
        let exit_code = i32::from(inner.run_with_args(&[argv0]));
        if exit_code == 0 {
            Ok(())
        } else {
            Err(Error::NonZeroExit(exit_code))
        }
    }
}

/// Live view of the running `adw::Application`. Handed to the consumer
/// body closure.
pub struct App {
    inner: adw::Application,
    monitors: Mutable<Vec<Monitor>>,
}

impl App {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(app_id: &str) -> AppBuilder {
        AppBuilder {
            app_id: app_id.to_owned(),
            services: Vec::new(),
            user_style: None,
        }
    }

    /// Snapshot of the currently connected monitors.
    #[must_use]
    pub fn monitors(&self) -> Vec<Monitor> {
        self.monitors.lock_ref().clone()
    }

    /// Signal of the current monitor list. Emits the initial value on
    /// subscribe and again on every hot-plug (monitor connect/disconnect).
    ///
    /// The returned signal owns a reference to the internal state, so it
    /// stays alive past `App` being dropped — safe to move into a
    /// `glib::MainContext::spawn_local` future from the body closure.
    pub fn monitors_changed(&self) -> impl Signal<Item = Vec<Monitor>> + 'static {
        self.monitors.signal_cloned()
    }

    /// Underlying `adw::Application`, exposed for advanced use.
    #[must_use]
    pub fn inner(&self) -> &adw::Application {
        &self.inner
    }

    /// Register `gio::ActionEntry`s on the underlying `adw::Application` (a
    /// `gio::ActionMap`).
    ///
    /// A `GApplication` auto-exports its own action group over
    /// `org.gtk.Actions` at the app's object path once it owns its bus name,
    /// so this is how a hytte shell exposes a keyboard/D-Bus command surface
    /// (e.g. niri keybinds driving the shell) **without** claiming a second
    /// bus name and **without** a thread hop — the handlers fire on the GTK
    /// main thread. Call from the body closure (post-activate); the actions
    /// are live for the process lifetime.
    pub fn add_action_entries(
        &self,
        entries: impl IntoIterator<Item = gio::ActionEntry<adw::Application>>,
    ) {
        self.inner.add_action_entries(entries);
    }

    /// Quit the main loop. Useful from tests.
    pub fn quit(&self) {
        self.inner.quit();
    }
}

fn read_monitors() -> Vec<Monitor> {
    let Some(display) = gdk::Display::default() else {
        return Vec::new();
    };
    let model = display.monitors();
    let mut out = Vec::with_capacity(crate::cast::u32_to_usize(model.n_items()));
    for i in 0..model.n_items() {
        if let Some(obj) = model.item(i)
            && let Ok(monitor) = obj.downcast::<gdk::Monitor>()
        {
            out.push(Monitor::new(monitor));
        }
    }
    out
}

fn install_default_css() {
    let provider = gtk::CssProvider::new();
    // The default stylesheet is loaded from disk at runtime — never compiled
    // in — so editing it cannot recompile the binary. Resolution mirrors
    // trollshell's `assets.rs`: the runtime `HYTTE_UI_DATA_DIR` override (set
    // by the Nix wrapper → the assets derivation) wins; otherwise the
    // compile-time `CARGO_MANIFEST_DIR/../../assets/hytte-ui` path points at
    // the in-repo source (the dev `cargo run` case). Only the *path* is ever
    // baked, never the CSS.
    provider.load_from_path(default_stylesheet_path());
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Resolve the default stylesheet path: the runtime `HYTTE_UI_DATA_DIR`
/// override (the Nix wrapper points it at the assets derivation) if set, else
/// the compile-time `CARGO_MANIFEST_DIR/../../assets/hytte-ui` path — the
/// in-repo source under the top-level `assets/` dir, for the dev `cargo run`
/// case. Baking only the path (not the contents) keeps the file fully
/// decoupled from the build.
fn default_stylesheet_path() -> PathBuf {
    let base = std::env::var_os("HYTTE_UI_DATA_DIR")
        .unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/hytte-ui").into());
    PathBuf::from(base).join("style.css")
}

fn install_user_css(path: &Path) {
    let provider = gtk::CssProvider::new();
    provider.load_from_path(path);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER,
        );
    }
}
