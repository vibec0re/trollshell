//! `App` and `AppBuilder` — the entry point for a hytte-based shell.
//!
//! The builder collects registered services and a one-shot body closure.
//! `run` constructs an `adw::Application`, connects an `activate` handler
//! that starts each service, installs the default stylesheet, and calls
//! the body once with an `&App` view.

use crate::error::{Error, Result};
use crate::monitor::Monitor;
use adw::prelude::*;
use futures_signals::signal::{Mutable, Signal, SignalExt};
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use hytte_reactive::registry::{self, ServiceErased};
use hytte_reactive::runtime;
use std::cell::{Cell, RefCell};
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
    /// Register a service. Its handles are installed into the thread-local
    /// registry on first activate, before the body closure runs.
    #[must_use]
    pub fn with<S: hytte_reactive::Service>(mut self, service: S) -> Self {
        self.services.push(Box::new(service));
        self
    }

    /// Load a user stylesheet from `path` at `STYLE_PROVIDER_PRIORITY_USER`
    /// (above the default sheet), on top of the built-in one.
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
            let Some(body_fn) = body_cell.borrow_mut().take() else {
                // A redundant activate — e.g. a second launch's forwarded
                // activation reaching the already-running primary. The shell
                // is up; don't re-hold or re-run the body.
                return;
            };

            // First activate only: hold the application alive without a
            // regular toplevel. `hold()` returns an `ApplicationHoldGuard`
            // whose `Drop` releases the hold; we leak it so the hold lasts the
            // process lifetime. Doing this inside the first-activate guard
            // avoids leaking a fresh guard on every (possibly redundant)
            // activate.
            std::mem::forget(inner_app.hold());

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
            // `watch_ready` reads the display's list once now (synchronously)
            // and again, debounced, on every hot-plug — publishing only the
            // monitors GTK has applied a `done` to (#1368).
            let monitors = Mutable::new(Vec::new());
            if let Some(display) = gdk::Display::default() {
                let writer = monitors.clone();
                watch_ready(
                    &display.monitors(),
                    &glib::MainContext::default(),
                    monitor_is_ready,
                    MONITOR_READY_NOTIFY,
                    move |ready| {
                        publish_if_changed(&writer, ready);
                    },
                );
            }

            let app = App {
                inner: inner_app.clone(),
                monitors,
            };
            body_fn(&app);
        });

        // Register up front so a second launch can be reported *before* it
        // silently forwards. Default single-instance flags turn a second launch
        // into the "remote" instance: it registers, finds the primary already
        // running, forwards its activation, and exits 0. `is_remote()` is only
        // valid while registered (and after `run` returns the primary is torn
        // down), so check it here — right after an explicit `register` — rather
        // than post-run. Surfacing it turns a silent no-op that reads like a
        // crash into a clear message (a recurring dev head-scratcher next to the
        // deployed service).
        if let Err(err) = inner.register(gio::Cancellable::NONE) {
            tracing::warn!(%err, "failed to register the GApplication");
        } else if inner.is_remote() {
            tracing::error!(
                "another instance of this shell is already running; this launch will \
                 forward its activation to the primary and start no shell of its own"
            );
        }

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
    /// The published monitor list: only monitors [`monitor_is_ready`] passes,
    /// written only by [`publish_if_changed`] (#1368).
    monitors: Mutable<Vec<gdk::Monitor>>,
}

impl App {
    /// Start building an app with the given D-Bus/GTK application id (e.g.
    /// `"mov.vibec0re.trollshell"`). Chain `with`/`with_user_style`, then
    /// [`AppBuilder::run`].
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
    ///
    /// **Every monitor listed here has completed its first `done`** (#1368):
    /// its geometry is set and, on a compositor that names its outputs
    /// (`wl_output` v4 sends `name` before that `done`), so is its
    /// [`connector`](Monitor::connector). GTK's Wayland backend appends a
    /// hot-plugged monitor to the display's list the moment it binds the
    /// `wl_output` — before either has arrived — so a monitor still waiting
    /// on that round trip is left out and listed once it lands. A compositor
    /// that never names its outputs still gets them listed, nameless, once
    /// their geometry is known.
    #[must_use]
    pub fn monitors(&self) -> Vec<Monitor> {
        self.monitors
            .lock_ref()
            .iter()
            .cloned()
            .map(Monitor::new)
            .collect()
    }

    /// Signal of the current monitor list. Emits the initial value on
    /// subscribe and again on every hot-plug (monitor connect/disconnect).
    /// Carries the same guarantee as [`monitors`](Self::monitors): a
    /// hot-plugged output appears in the first emission after GTK applies
    /// its `done`, never in one before it — and an emission is skipped when
    /// the list it would carry is the one already published (the same
    /// `gdk::Monitor`s in the same order), so a hot-plug costs one rebuild.
    ///
    /// The returned signal owns a reference to the internal state, so it
    /// stays alive past `App` being dropped — safe to move into a
    /// `glib::MainContext::spawn_local` future from the body closure.
    pub fn monitors_changed(&self) -> impl Signal<Item = Vec<Monitor>> + 'static {
        self.monitors
            .signal_cloned()
            .map(|monitors| monitors.into_iter().map(Monitor::new).collect())
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

/// Whether GTK has applied this monitor's first `done` (#1368).
///
/// GTK 4.22's Wayland backend appends a hot-plugged monitor to
/// `display.monitors()` as soon as it binds the `wl_output`
/// (`gdk/wayland/gdkmonitor-wayland.c:376`); the connector name arrives later
/// with the `name` event (`:282-292`) and the geometry later still, set only
/// by `done` (`output_handle_done` → `apply_monitor_change`, `:237-245`). Both
/// need a compositor round trip, and the one-tick debounce in [`watch_ready`]
/// can win that race — which is how a returning output used to be published
/// with no connector and stay that way (every connector-keyed overlay skipped
/// it; nothing re-read the list when the name landed). A 0×0 geometry is the
/// tell: `wl_output` v4 sends `name` before `done`, so a non-zero geometry
/// means the name, if the compositor sends one, is in too.
fn monitor_is_ready(monitor: &gdk::Monitor) -> bool {
    let geometry = monitor.geometry();
    geometry.width() > 0 && geometry.height() > 0
}

/// The properties a not-yet-ready monitor is watched on. `geometry` is the
/// one that flips [`monitor_is_ready`]; `connector` rides along so a name that
/// lands in a dispatch of its own also re-reads — which on a v4 compositor
/// finds the monitor still waiting on `done` and publishes nothing new
/// ([`publish_if_changed`]), so it costs no rebuild.
const MONITOR_READY_NOTIFY: &[&str] = &["geometry", "connector"];

/// Set `out` to `ready` unless it already holds exactly those objects, in that
/// order (GObject equality is identity). Returns whether it published.
///
/// A re-read that finds nothing new — a hot-plugged monitor still held back,
/// or a watch firing before the `done` that makes it ready — would otherwise
/// re-emit the same list and drive a full per-monitor teardown/rebuild for
/// nothing; with it, a hot-plug costs one rebuild (#1368).
fn publish_if_changed<O: PartialEq + Clone>(out: &Mutable<Vec<O>>, ready: Vec<O>) -> bool {
    if *out.lock_ref() == ready {
        return false;
    }
    out.set(ready);
    true
}

/// Mirror the **ready** items of `model` into `publish` (#1368).
///
/// Reads `model` once now, synchronously, and again one idle tick after any
/// burst of `items_changed` — coalesced, so a replug (remove-then-add, two
/// signals) is one read, not two. Each read hands `publish` the items
/// `is_ready` passes, in model order, and leaves the rest out. An item left
/// out is **watched**: a `notify` handler on each property in `notify`
/// schedules the same debounced read, so the read that finds it ready is the
/// one that publishes it — with no other change to the model needed.
///
/// A watch is armed once per item however many reads find it unready, and is
/// disconnected by the first read that finds the item ready or gone from the
/// model. It holds the item only weakly: an item removed before it was ever
/// ready is not kept alive by its watch.
///
/// Generic over the item type, rather than tied to `gdk::Monitor`, so it can
/// be driven with no display: the tests feed it a `gio::ListStore` of
/// `gio::SimpleAction`s and step a private `glib::MainContext` (`ctx`, where
/// the debounced read is scheduled) by hand. `App` calls it with the display's
/// monitor list, [`monitor_is_ready`] and [`MONITOR_READY_NOTIFY`].
///
/// The returned feed is kept alive by `model`'s `items_changed` handler; the
/// caller may drop it.
fn watch_ready<O, P>(
    model: &impl IsA<gio::ListModel>,
    ctx: &glib::MainContext,
    is_ready: fn(&O) -> bool,
    notify: &'static [&'static str],
    publish: P,
) -> Rc<ReadyFeed<O>>
where
    O: IsA<glib::Object>,
    P: Fn(Vec<O>) + 'static,
{
    let model = model.upcast_ref::<gio::ListModel>();
    let feed = Rc::new(ReadyFeed {
        model: model.downgrade(),
        ctx: ctx.clone(),
        is_ready,
        notify,
        publish: Box::new(publish),
        scheduled: Cell::new(false),
        watching: RefCell::new(Vec::new()),
    });
    feed.read();
    let on_change = Rc::clone(&feed);
    model.connect_items_changed(move |_, _, _, _| on_change.schedule());
    feed
}

/// The state behind one [`watch_ready`] call.
struct ReadyFeed<O: IsA<glib::Object>> {
    /// Weak so the feed (held by the model's own `items_changed` handler) does
    /// not keep its model alive in a cycle.
    model: glib::WeakRef<gio::ListModel>,
    ctx: glib::MainContext,
    is_ready: fn(&O) -> bool,
    notify: &'static [&'static str],
    publish: Box<dyn Fn(Vec<O>)>,
    /// A read is already queued on `ctx`; further triggers ride it.
    scheduled: Cell<bool>,
    /// One entry per item the last read left out.
    watching: RefCell<Vec<Watch<O>>>,
}

/// The `notify` handlers armed on one not-yet-ready item.
struct Watch<O: IsA<glib::Object>> {
    item: glib::WeakRef<O>,
    handlers: Vec<glib::SignalHandlerId>,
}

impl<O: IsA<glib::Object>> ReadyFeed<O> {
    /// Queue one read on the next idle tick, unless one is already queued —
    /// the single scheduler both `items_changed` and every watch feed.
    fn schedule(self: &Rc<Self>) {
        if self.scheduled.replace(true) {
            return;
        }
        let feed = Rc::clone(self);
        self.ctx
            .spawn_local_with_priority(glib::Priority::DEFAULT_IDLE, async move {
                feed.scheduled.set(false);
                feed.read();
            });
    }

    /// Read the model, re-arm the watches, publish the ready items.
    fn read(self: &Rc<Self>) {
        let Some(model) = self.model.upgrade() else {
            return;
        };
        let n_items = model.n_items();
        let mut ready = Vec::with_capacity(crate::cast::u32_to_usize(n_items));
        let mut unready = Vec::new();
        for item in (0..n_items)
            .filter_map(|i| model.item(i))
            .filter_map(|obj| obj.downcast::<O>().ok())
        {
            if (self.is_ready)(&item) {
                ready.push(item);
            } else {
                unready.push(item);
            }
        }
        self.watch(&unready);
        (self.publish)(ready);
    }

    /// Make `watching` cover exactly `unready`: drop the watch of every item
    /// that is now ready or gone, and arm one on each item not yet watched.
    fn watch(self: &Rc<Self>, unready: &[O]) {
        let mut watching = self.watching.borrow_mut();
        watching.retain_mut(|watch| {
            // Finalized: its handlers went with it.
            let Some(item) = watch.item.upgrade() else {
                return false;
            };
            if unready.contains(&item) {
                return true;
            }
            for handler in watch.handlers.drain(..) {
                item.disconnect(handler);
            }
            false
        });
        for item in unready {
            if watching
                .iter()
                .any(|watch| watch.item.upgrade().as_ref() == Some(item))
            {
                continue;
            }
            tracing::debug!(
                item = item.type_().name(),
                notify = ?self.notify,
                "list item not ready yet; holding it back until it is"
            );
            let handlers = self
                .notify
                .iter()
                .map(|&property| {
                    let feed = Rc::downgrade(self);
                    item.connect_notify_local(Some(property), move |_, _| {
                        if let Some(feed) = feed.upgrade() {
                            feed.schedule();
                        }
                    })
                })
                .collect();
            watching.push(Watch {
                item: item.downgrade(),
                handlers,
            });
        }
    }

    /// How many `notify` handlers the feed holds — including a finalized
    /// item's, until a read prunes its watch.
    #[cfg(test)]
    fn armed(&self) -> usize {
        self.watching
            .borrow()
            .iter()
            .map(|watch| watch.handlers.len())
            .sum()
    }
}

/// The shipped default stylesheet, compiled into the binary as a last-resort
/// fallback for [`install_default_css`] when the on-disk copy is missing. The
/// on-disk path is still preferred so edits don't force a recompile; this only
/// backstops a deployment that shipped without its assets.
///
/// `pub(crate)` so `widget_tree`'s dense-row measurement (#966) can install the
/// **shipped** rule rather than a copy of it retyped in the test: a test that
/// declares its own CSS proves the reconciler adds a class and nothing about
/// whether `assets/hytte-ui/style.css` gives that class any meaning. This
/// constant is the only reachable copy in the sandboxed `system-tests` run —
/// crane's source filter strips `assets/` bar this one file, precisely because
/// it is `include_str!`'d here.
pub(crate) const DEFAULT_STYLESHEET: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/hytte-ui/style.css"
));

fn install_default_css() {
    let provider = gtk::CssProvider::new();
    // The default stylesheet is loaded from disk at runtime — never compiled
    // in — so editing it cannot recompile the binary. Resolution mirrors
    // trollshell's `assets.rs`: the runtime `HYTTE_UI_DATA_DIR` override (set
    // by the Nix wrapper → the assets derivation) wins; otherwise the
    // compile-time `CARGO_MANIFEST_DIR/../../assets/hytte-ui` path points at
    // the in-repo source (the dev `cargo run` case). Only the *path* is ever
    // baked, never the CSS.
    let path = default_stylesheet_path();
    if path.exists() {
        provider.load_from_path(&path);
    } else {
        // Neither `HYTTE_UI_DATA_DIR` nor the source tree is present (a
        // deployment shipped without its assets). `load_from_path` on a
        // missing file loads *nothing* — silently dropping load-bearing rules,
        // notably the transparent `.hytte-popup-catcher` (without which the
        // outside-click catcher paints opaque over the whole screen). Fall back
        // to the copy compiled into the binary so those rules still apply.
        tracing::warn!(
            path = %path.display(),
            "default stylesheet not found on disk; using the compiled-in fallback"
        );
        provider.load_from_string(DEFAULT_STYLESHEET);
    }
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

#[cfg(test)]
mod tests {
    //! #1368's readiness gate, driven with no display: `gio::SimpleAction`s
    //! stand in for `gdk::Monitor`s (`enabled` for "GTK has applied `done`"),
    //! a `gio::ListStore` for `display.monitors()`, and a private
    //! `glib::MainContext`, stepped by hand, for the main loop.

    use super::{ReadyFeed, publish_if_changed, watch_ready};
    use adw::prelude::*;
    use futures_signals::signal::{Mutable, Signal};
    use gtk::{gio, glib};
    use std::cell::RefCell;
    use std::pin::pin;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    const NOTIFY: &[&str] = &["enabled"];

    fn output(name: &str, ready: bool) -> gio::SimpleAction {
        let item = gio::SimpleAction::new(name, None);
        item.set_enabled(ready);
        item
    }

    fn is_ready(item: &gio::SimpleAction) -> bool {
        item.is_enabled()
    }

    fn names(reads: &[&[&str]]) -> Vec<Vec<String>> {
        reads
            .iter()
            .map(|read| read.iter().map(|&name| name.to_owned()).collect())
            .collect()
    }

    /// A [`watch_ready`] over a store of stand-ins, recording every read.
    struct Rig {
        ctx: glib::MainContext,
        store: gio::ListStore,
        /// Every read the feed made, as the names it published.
        reads: Rc<RefCell<Vec<Vec<String>>>>,
        feed: Rc<ReadyFeed<gio::SimpleAction>>,
    }

    impl Rig {
        fn new(items: &[&gio::SimpleAction]) -> Self {
            let ctx = glib::MainContext::new();
            let store = gio::ListStore::new::<gio::SimpleAction>();
            for &item in items {
                store.append(item);
            }
            let reads = Rc::new(RefCell::new(Vec::new()));
            let sink = Rc::clone(&reads);
            let feed = watch_ready(
                &store,
                &ctx,
                is_ready,
                NOTIFY,
                move |ready: Vec<gio::SimpleAction>| {
                    sink.borrow_mut()
                        .push(ready.iter().map(|item| item.name().to_string()).collect());
                },
            );
            Self {
                ctx,
                store,
                reads,
                feed,
            }
        }

        /// Dispatch everything queued on the rig's context, as the main loop
        /// would between two frames.
        fn settle(&self) {
            while self.ctx.iteration(false) {}
        }

        fn reads(&self) -> Vec<Vec<String>> {
            self.reads.borrow().clone()
        }
    }

    /// **#1368.** A listed item that is not ready is left out of what
    /// [`watch_ready`] publishes, and its becoming ready — with no change to
    /// the list itself — triggers exactly one re-read, which publishes it.
    /// That is the hot-plugged monitor GTK lists before its `name`/`done`
    /// arrive: before the fix it was published nameless and nothing re-read
    /// the list when the name landed.
    ///
    /// **Falsified** by treating every item as ready in `ReadyFeed::read`
    /// ("b" is in the first read) and by deleting the `notify` handler in
    /// `ReadyFeed::watch` (no second read).
    #[test]
    fn an_unready_item_is_held_back_until_it_is_ready() {
        let a = output("a", true);
        let b = output("b", false);
        let rig = Rig::new(&[&a, &b]);
        assert_eq!(
            rig.reads(),
            names(&[&["a"]]),
            "the initial read is synchronous and leaves the unready item out",
        );
        rig.settle();
        assert_eq!(rig.reads().len(), 1, "nothing re-reads while b waits");

        b.set_enabled(true);
        rig.settle();
        assert_eq!(
            rig.reads(),
            names(&[&["a"], &["a", "b"]]),
            "b turning ready re-reads exactly once, and that read publishes it",
        );
        assert_eq!(
            rig.feed.armed(),
            0,
            "b's watch is disarmed once it is ready"
        );
    }

    /// The hot-plug path proper: an item that arrives unready through
    /// `items_changed` is read (debounced, one read for the burst), held
    /// back, and published by the read its readiness schedules — the same
    /// scheduler, so two items turning ready in one turn share one read.
    ///
    /// **Falsified** with the other two: no readiness filter publishes "b"
    /// and "c" in the second read; no `notify` handler leaves them out for
    /// good.
    #[test]
    fn a_hot_plugged_item_is_published_by_the_read_that_finds_it_ready() {
        let a = output("a", true);
        let rig = Rig::new(&[&a]);
        let b = output("b", false);
        let c = output("c", false);
        rig.store.append(&b);
        rig.store.append(&c);
        rig.settle();
        assert_eq!(
            rig.reads(),
            names(&[&["a"], &["a"]]),
            "two appends in one turn are one read, and it publishes neither unready item",
        );
        assert_eq!(rig.feed.armed(), 2 * NOTIFY.len(), "one watch each");

        c.set_enabled(true);
        b.set_enabled(true);
        rig.settle();
        assert_eq!(
            rig.reads(),
            names(&[&["a"], &["a"], &["a", "b", "c"]]),
            "both turning ready in one turn is one read, in model order",
        );
        assert_eq!(rig.feed.armed(), 0);
    }

    /// A burst of reads before an item is ready arms **one** watch on it,
    /// not one per read — and once the item is published, its watch is gone:
    /// a notify on it re-reads nothing.
    ///
    /// **Falsified** by deleting the "already watched" check in
    /// `ReadyFeed::watch`: three reads leave three watches on "b".
    #[test]
    fn reads_before_readiness_arm_one_watch() {
        let a = output("a", true);
        let b = output("b", false);
        let rig = Rig::new(&[&a, &b]);
        // b's own watch fires while b is still unready: one re-read, which
        // must neither publish b nor arm a second watch on it.
        b.notify("enabled");
        rig.settle();
        // An unrelated change: a third read that finds b unready.
        rig.store.append(&output("c", true));
        rig.settle();
        assert_eq!(rig.reads(), names(&[&["a"], &["a"], &["a", "c"]]));
        assert_eq!(
            rig.feed.armed(),
            NOTIFY.len(),
            "three reads found b unready; it carries one watch, not three",
        );

        b.set_enabled(true);
        rig.settle();
        assert_eq!(
            rig.reads(),
            names(&[&["a"], &["a"], &["a", "c"], &["a", "b", "c"]]),
            "exactly one read for b turning ready",
        );
        assert_eq!(rig.feed.armed(), 0);

        b.notify("enabled");
        rig.settle();
        assert_eq!(
            rig.reads().len(),
            4,
            "a published item's watch is disconnected: its notify re-reads nothing",
        );
    }

    /// A watch holds its item weakly: an item removed from the model before
    /// it was ever ready — a monitor unplugged mid-round-trip — is freed, and
    /// the next read prunes the watch.
    #[test]
    fn an_item_removed_before_it_is_ready_is_not_kept_alive() {
        let a = output("a", true);
        let rig = Rig::new(&[&a]);
        let b = output("b", false);
        rig.store.append(&b);
        rig.settle();
        assert_eq!(rig.feed.armed(), NOTIFY.len());

        let gone = b.downgrade();
        drop(b);
        rig.store.remove(1);
        assert!(
            gone.upgrade().is_none(),
            "the watch must not keep the item it waits on alive",
        );
        rig.settle();
        assert_eq!(rig.reads(), names(&[&["a"], &["a"], &["a"]]));
        assert_eq!(rig.feed.armed(), 0, "the dead item's watch is pruned");
    }

    /// The optional half of #1368: a re-read that finds the same objects in
    /// the same order does not re-emit, so a hot-plug — whose first read
    /// finds the new monitor still held back — costs one rebuild, not two.
    /// Identity, not value: a replugged output is a new `GdkMonitor`, and
    /// order is part of the list (the first monitor hosts the password prompt).
    #[test]
    fn an_identical_list_is_not_republished() {
        let a = output("a", true);
        let b = output("b", true);
        let out = Mutable::new(vec![a.clone()]);
        let mut changes = pin!(out.signal_cloned());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            changes.as_mut().poll_change(&mut cx),
            Poll::Ready(Some(_))
        ));

        assert!(!publish_if_changed(&out, vec![a.clone()]));
        assert!(
            changes.as_mut().poll_change(&mut cx).is_pending(),
            "the same objects re-read are not an emission",
        );

        assert!(publish_if_changed(&out, vec![a.clone(), b.clone()]));
        assert!(matches!(
            changes.as_mut().poll_change(&mut cx),
            Poll::Ready(Some(list)) if list == [a.clone(), b.clone()]
        ));
        assert!(
            publish_if_changed(&out, vec![b.clone(), a.clone()]),
            "a reorder is a change",
        );
        assert!(
            publish_if_changed(&out, vec![b, output("a", true)]),
            "an equal-looking but different object is a change",
        );
    }
}
