//! `trollshell-control-center` — the external, launch-on-demand settings &
//! management companion app for trollshell (#381; walking skeleton from #390).
//!
//! Modelled on gnome-control-center: the shell stays the lean always-on bar +
//! overlays, and heavier management UI lives here in a **separate windowed**
//! GTK4 + libadwaita app that talks to the running shell over D-Bus. It is
//! **never linked into the shell** — it only dials the shell's
//! `mov.vibec0re.trollshell.Control` session-bus endpoint (see the shell's
//! `control.rs`).
//!
//! An `adw::ViewStack` of tabs plus a banner that appears only when the shell
//! did *not* answer `Ping`/`Version` (#959) — a connected session shows no
//! banner at all. That probe, and the revision footer's, **re-run on the
//! Plugins tab's cadence** (#989): they used to be one-shot from
//! [`build_window`], which pinned "trollshell is not running" for the whole
//! session when the app was launched before the shell — the ordinary order
//! after login — and never brought the banner back when a running shell died.
//! The **Places** tab ([`places_tab`], #640/#703) is
//! a full editor for `~/.config/trollshell/places.toml` — the named places that
//! drive departures, Wi-Fi-fingerprint place detection and walk time — plus the
//! session-only weather-location override this tab used to be (#391). It is the
//! one tab that does **not** go through `Control`: it reads and writes the file
//! directly, so it keeps working while the shell is down. The **Plugins** tab
//! (#348) lists each `trollshell-plugin-<id>` systemd **user** unit with a
//! switch that starts/enables or stops/disables it. The **AI Keys** tab
//! ([`ai_keys_tab`], #392) stores the LLM-backed plugins' API keys in the
//! login keyring (gnome-keyring/libsecret) — never on disk — and rotates
//! them. Those round-trip over `Control`, and since #1003 its status also
//! re-reads on a shell-reachability transition rather than once at window
//! build — deliberately **not** a timer of its own, since `ListAiKeys` reaches
//! a prompt-capable keyring open on the shell side no periodic caller may
//! touch; see that module's doc for the full story. There is deliberately
//! **no Display tab**: #393
//! re-scoped display management away from a bespoke control-center page and
//! onto `org.gnome.Mutter.DisplayConfig`, a shim over niri-ipc
//! (`crates/hytte-services/src/display_config.rs`) that lets
//! **gnome-control-center's own Display panel** drive niri outputs directly —
//! compatmaxx: reuse the existing GNOME client, provide the backend. When the
//! shell isn't running the app degrades gracefully rather than panicking.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use hytte_bus::RetryPolicy;

mod ai_keys_tab;
mod places_tab;
mod plugins_tab;
#[cfg(all(test, feature = "system-tests"))]
mod test_support;

/// Distinct app-id — this is its own application, not the shell.
const APP_ID: &str = "mov.vibec0re.trollshell.ControlCenter";
/// The shell's dedicated control endpoint (owned by the shell's `control.rs`).
pub(crate) const CONTROL_NAME: &str = "mov.vibec0re.trollshell.Control";
pub(crate) const CONTROL_PATH: &str = "/mov/vibec0re/trollshell/Control";
pub(crate) const CONTROL_IFACE: &str = "mov.vibec0re.trollshell.Control";

/// Default `tracing` level when `RUST_LOG` is unset (#780, mirroring #746's
/// fix for the shell binary in `trollshell/src/main.rs`, #766).
///
/// `tracing_subscriber::fmt::init()`'s own env-unset fallback
/// (`EnvFilter::from_default_env`) is hard-coded to `ERROR`, and no
/// deployment path sets `RUST_LOG` for this companion app either, so a bare
/// `fmt::init()` silently discards every non-error log line on a normal
/// launch — currently 9 `info!` sites and no `warn!`/`debug!`/`trace!` (#780's
/// audit). `INFO` matches the shell binary's `DEFAULT_LOG_LEVEL` for
/// consistency between the two binaries.
const DEFAULT_LOG_LEVEL: tracing_subscriber::filter::LevelFilter =
    tracing_subscriber::filter::LevelFilter::INFO;

/// The banner's text before the first probe answers — what the window is
/// built with, and what [`ShellProbeUi`] therefore starts out believing is on
/// screen.
const CONNECTING_BANNER: &str = "Connecting to trollshell…";

/// How often the shell probes behind the connection banner and the revision
/// footer re-run (#989).
///
/// Deliberately *the Plugins tab's* cadence rather than a number of its own:
/// the banner and that tab answer the same question — is the shell there? —
/// against the same `Control` endpoint, and them disagreeing is exactly the
/// defect. Before #989 the probes were one-shot from [`build_window`], so a
/// control-center launched before the shell (the ordinary order after login
/// or a `home-manager switch`) pinned "trollshell is not running" and
/// "unavailable" for the rest of the session while the Plugins tab listed
/// live plugins underneath, and a shell that died mid-session never brought
/// the banner back at all.
///
/// Private again as of #1003: an earlier revision of that fix also read this
/// from [`ai_keys_tab`], giving that tab its own 2 s `ListAiKeys` timer. An
/// adversarial review found that timer reached a prompt-capable keyring open
/// on the shell side that must never be called periodically (see
/// `ai_keys_tab`'s module doc), so the tab now reacts to [`ShellProbeUi`]'s
/// reachability transitions instead of polling on any interval of its own —
/// it no longer needs this constant at all.
const SHELL_PROBE_INTERVAL: Duration = plugins_tab::PLUGIN_POLL_INTERVAL;

/// Builds the `EnvFilter` that gates the global `tracing` subscriber.
///
/// `rust_log`, when `Some`, is parsed directly as the filter's directive
/// string instead of reading the process's real `RUST_LOG` — this is what
/// lets a test exercise the default-directive and override paths in
/// isolation, without mutating process env (which `unsafe_code = "forbid"`
/// rules out here anyway: `std::env::set_var`/`remove_var` are `unsafe` fns).
/// `main` always passes `None`, so `RUST_LOG` still overrides
/// [`DEFAULT_LOG_LEVEL`] exactly as before — `EnvFilter::Builder::from_env_lossy`
/// is `parse_lossy(env::var("RUST_LOG").unwrap_or_default())` under the hood,
/// so passing the same string through `parse_lossy` directly runs the
/// identical code path for a given `RUST_LOG` value.
fn build_env_filter(rust_log: Option<&str>) -> tracing_subscriber::EnvFilter {
    let builder =
        tracing_subscriber::EnvFilter::builder().with_default_directive(DEFAULT_LOG_LEVEL.into());
    match rust_log {
        Some(dirs) => builder.parse_lossy(dirs),
        None => builder.from_env_lossy(),
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(build_env_filter(None))
        .init();

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_window);
    app.run()
}

/// Build the main window: a view-switcher over the tabs plus a
/// connection-status banner, then kick off the async shell probe.
fn build_window(app: &adw::Application) {
    let stack = adw::ViewStack::new();
    // The Plugins tab (#348): start/stop/enable each plugin's systemd user unit.
    let (plugins_page, plugins_poll) = plugins_tab::build_page();
    stack.add_titled_with_icon(
        &plugins_page,
        Some("plugins"),
        "Plugins",
        "application-x-addon-symbolic",
    );
    // The Places tab (#640/#703): the places.toml editor, plus the #391
    // weather-location override demoted into a group of its own.
    let (places_page, places_poll) = places_tab::build_page();
    stack.add_titled_with_icon(
        &places_page,
        Some("places"),
        "Places",
        "mark-location-symbolic",
    );
    // The AI Keys tab (#392): store/rotate the LLM-backed plugins' API keys in
    // the login keyring, round-tripped over Control. Since #1003 it re-reads
    // on a shell-reachability transition (wired below, once `probe` exists)
    // rather than once at window build, and installs no timer of its own —
    // see `ai_keys_tab`'s module doc for why.
    let (ai_keys_page, notify_shell_reachable_change) = ai_keys_tab::build_page();
    stack.add_titled_with_icon(
        &ai_keys_page,
        Some("ai-keys"),
        "AI Keys",
        "dialog-password-symbolic",
    );

    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    let header = adw::HeaderBar::builder().title_widget(&switcher).build();

    let banner = adw::Banner::builder()
        .title(CONNECTING_BANNER)
        .revealed(true)
        .build();

    // The revision footer (#601, extended by #959 to also carry the shell's
    // `Version`): the running shell's build git revision plus its reported
    // version, fetched over `Control.Revision`/`Control.Version` — see
    // `check_shell_revision` for why this must never resolve the companion
    // app's own compiled-in `TROLLSHELL_REV`.
    let (footer, revision_label) = build_revision_footer();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&stack));
    toolbar.add_bottom_bar(&footer);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("trollshell Control Center")
        .default_width(760)
        .default_height(560)
        .content(&toolbar)
        .build();

    // The shell probe behind the banner and the footer (#989): once now, then
    // on `SHELL_PROBE_INTERVAL` for as long as the window lives, so a shell
    // that starts after the control-center clears the banner and one that dies
    // mid-session brings it back — both directions out of
    // `format_banner_message`'s existing `None`/`Some` contract. Since #1003
    // the AI Keys tab rides the same probe rather than polling on its own: the
    // listener is registered *before* the first `poll()` so that first
    // outcome reaches it too — `build_page` does NOT also read at build time
    // (a second-round review finding: `reachable` starts `None`, so this
    // first probe is unconditionally a transition and a second, build-time
    // read would just double the keyring traffic at every window open; see
    // `ai_keys_tab`'s module doc and `build_page`'s own comment).
    let probe = ShellProbeUi::new(&banner, &revision_label);
    probe.set_reachable_listener(notify_shell_reachable_change);
    probe.poll();
    let shell_poll = install_shell_probe(&probe, SHELL_PROBE_INTERVAL);

    // The poll timers are scoped to this window: drop them on close so a
    // dismissed window stops polling `Control` (Plugins, and since #989 the
    // banner/footer probe — which, since #1003, is also the AI Keys tab's only
    // source of re-reads) and stat'ing `places.toml` (Places), and a re-launch
    // while another window is still resident can't leave the first window's
    // timers double-polling behind it (#542). Wrapped in a cell + `.take()` so
    // the one-shot removal is clean under the `Fn` close handler.
    let polls = RefCell::new(vec![plugins_poll, places_poll, shell_poll]);
    window.connect_close_request(move |_| {
        for source in polls.take() {
            source.remove();
        }
        glib::Propagation::Proceed
    });

    window.present();
}

// ── Revision footer (#601) ────────────────────────────────────────────────────

/// Build the footer bar: a single dim, end-aligned label reporting the running
/// shell's build revision. Deliberately small and unobtrusive — a footer, not a
/// tab or dialog — and refreshed on the same probe as the connection banner
/// (see [`ShellProbeUi`]), which since #989 re-runs on a timer rather than
/// once at startup.
fn build_revision_footer() -> (gtk::Box, gtk::Label) {
    let label = gtk::Label::builder()
        .label("Shell revision: checking…")
        .halign(gtk::Align::End)
        .build();
    label.add_css_class("dim-label");
    label.add_css_class("caption");

    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .halign(gtk::Align::End)
        .margin_top(2)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    bar.append(&label);
    (bar, label)
}

/// Format the footer label's text from a `Control.Version`/`Control.Revision`
/// call outcome, each `None` exactly when that call failed.
///
/// Pure so the shell-not-running fallback, the `"dev"` passthrough and the
/// partial-availability cases are unit-tested without a live D-Bus call — see
/// the `revision_footer_tests` module below. Four shapes, in order of how
/// informative they are:
///
/// - both present: `"trollshell {version} · revision {revision}"` — a `"dev"`
///   or `-dirty`-suffixed revision (`revision.rs`'s documented fallback /
///   marker) is rendered as-is rather than special-cased, per #601: a
///   developer seeing either is correct information, not a state to hide.
/// - only one present: render that one (`"trollshell {version} · revision
///   unknown"` / `"revision {revision}"`) rather than discarding it because
///   its sibling call happened to fail.
/// - neither present: the shell isn't running (or didn't answer in time) —
///   the pre-#959 fallback text, kept byte-for-byte so `docs/live-verify.md`'s
///   #601/#836 entry still matches.
fn format_revision_footer(version: Option<&str>, revision: Option<&str>) -> String {
    match (version, revision) {
        (None, None) => "Shell revision: unavailable (trollshell not running)".to_owned(),
        (Some(version), Some(revision)) => format!("trollshell {version} · revision {revision}"),
        (Some(version), None) => format!("trollshell {version} · revision unknown"),
        (None, Some(revision)) => format!("revision {revision}"),
    }
}

// ── The shell probe behind the banner and the footer (#959/#601, #989) ───────

/// One round of "is the shell there, and which build is it?" — the single
/// source both the connection banner and the revision footer are drawn from.
///
/// One probe rather than two (#989) because the two questions are the same
/// question asked twice: the banner needs `Ping` + `Version`, the footer needs
/// `Version` + `Revision`, and running them as separate periodic probes would
/// double the traffic only to let the two surfaces disagree about the same
/// endpoint at the same instant.
struct ShellProbe {
    /// `Ping` then `Version` — [`format_banner_message`]'s input, and the
    /// footer's version half.
    connection: Result<(String, String), hytte_bus::BusError>,
    /// `Revision`, or `None` when that call failed (or was never made because
    /// the connection probe already proved the endpoint isn't answering).
    revision: Option<String>,
}

/// Run one [`ShellProbe`] on the shared tokio runtime.
///
/// The `Revision` call is skipped outright when the connection probe failed:
/// a `Control` that did not answer `Ping` will not answer `Revision` either,
/// and `(None, None)` is already exactly the footer's shell-not-running text.
/// That keeps a *disconnected* tick down to one fast `ServiceUnknown` instead
/// of stacking a second timeout on top of it — which matters now that this
/// runs every [`SHELL_PROBE_INTERVAL`] rather than once.
///
/// # Why this calls `Control.Version`/`Control.Revision` and not a local resolver
///
/// `trollshell-control-center` is a separate binary from the shell, wrapped by
/// its own nix slice (`nix/control-center.nix`) with its **own**
/// `TROLLSHELL_REV` baked in. That value is the *companion app's* build
/// revision, not the running shell's — normally identical, but they diverge
/// exactly when it matters (a rebuild that updates one and not the other, a
/// stale store path, a dev companion against a deployed shell). Reporting the
/// companion's own revision here would look authoritative while silently
/// answering the wrong question — the exact failure #601 exists to prevent
/// (four bug reports — #375, #566, #375 again, #810 — turned on "which commit
/// is the running shell", not "which commit is the control center"). So this
/// crate has no `revision` module of its own; the only source of truth is the
/// D-Bus round trip below. The same reasoning applies to `Version` (#959):
/// the banner already round-trips it per-connection-check, so the footer
/// reuses that same source rather than a compiled-in `CARGO_PKG_VERSION`.
async fn probe_shell_status() -> ShellProbe {
    let connection = probe_shell().await;
    let revision = if should_probe_revision(&connection) {
        revision().await.ok()
    } else {
        None
    };
    ShellProbe {
        connection,
        revision,
    }
}

/// Whether a probe whose connection half came back as `connection` should go
/// on to ask for `Revision`.
///
/// A named predicate rather than an inline `if` so the one property
/// [`probe_shell_status`] buys — a *disconnected* tick costs one fast
/// `ServiceUnknown` instead of stacking a second 3 s timeout behind it — is
/// reachable from a test. `probe_shell_status` itself is a bare async fn over
/// `hytte_bus::call` and every #989 test hand-builds its [`ShellProbe`], so
/// nothing could otherwise reach this branch (the review of `32bf073`, LOW 4).
fn should_probe_revision(connection: &Result<(String, String), hytte_bus::BusError>) -> bool {
    connection.is_ok()
}

/// `Revision` → the running shell's build git revision (#601). See
/// [`probe_shell_status`] for why this is a plain `Control` round trip and
/// not a local resolve.
async fn revision() -> Result<String, hytte_bus::BusError> {
    control_call("Revision").await
}

/// Install the periodic shell probe, returning its `SourceId` so the window
/// can drop it on close (#542).
///
/// `interval` is a parameter rather than [`SHELL_PROBE_INTERVAL`] read inside,
/// so a test can install a fast one and assert the timer genuinely re-fires.
/// Deleting this timer is exactly the mutation that reintroduces #989, and
/// every other test here drives [`ShellProbeUi::apply`] by hand — none of them
/// would notice.
fn install_shell_probe(probe: &ShellProbeUi, interval: Duration) -> glib::SourceId {
    let probe = probe.clone();
    glib::timeout_add_local(interval, move || {
        probe.poll();
        glib::ControlFlow::Continue
    })
}

/// Holds [`ShellProbeUi`]'s single in-flight slot for as long as it lives, and
/// releases it on `Drop` — including the drop that happens when a completion
/// callback is discarded without ever running. See [`ShellProbeUi::poll`].
struct InFlightSlot(Rc<Cell<bool>>);

impl Drop for InFlightSlot {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// A single `main.rs`-owned subscriber to [`ShellProbeUi`]'s reachability
/// transitions (#1003) — see `reachable_listener`'s field doc.
type ReachableListener = Rc<RefCell<Option<Box<dyn Fn(bool)>>>>;

/// The banner + footer pair, and the bookkeeping that lets them be re-probed
/// on a timer instead of once at window build (#989).
///
/// Holds its two widgets strongly: the timer that owns this closure is removed
/// on `close-request` (#542), so the whole thing is dropped with the window,
/// and neither widget holds the timer back.
#[derive(Clone)]
struct ShellProbeUi {
    banner: adw::Banner,
    revision: gtk::Label,
    /// Set while a probe is outstanding, so a tick landing on top of a slow
    /// round trip skips rather than stacking a second one. Released by
    /// [`InFlightSlot`]'s `Drop`, never by hand.
    ///
    /// This is the whole ordering story for these probes, and why they need no
    /// generation counter the way the Plugins tab's polls do (#983): only ever
    /// one is in flight, so completions cannot arrive out of order in the
    /// first place. The tab cannot use the same discipline — skipping ticks
    /// there would also drop its post-toggle settle re-poll — which is why the
    /// two mechanisms differ.
    in_flight: Rc<Cell<bool>>,
    /// The banner message currently on screen (`None` = hidden), so a probe
    /// that says the same thing as the last one doesn't add a log line. At a
    /// 2 s cadence the unconditional `info!` this replaced would be one line
    /// every two seconds for as long as the shell is down — and `INFO` is the
    /// default level for this binary (#780).
    shown: Rc<RefCell<Option<String>>>,
    /// How many times [`poll`](Self::poll) has been called, skipped ticks
    /// included.
    ///
    /// Carried in production rather than behind `cfg(test)` because the timer
    /// is the whole of #989 and nothing else makes it falsifiable: every other
    /// test here drives [`apply`](Self::apply) by hand, so deleting the timer
    /// would leave them all green. `the_probe_re_runs_on_its_interval` watches
    /// this instead. One `Cell` increment every [`SHELL_PROBE_INTERVAL`].
    ticks: Rc<Cell<u32>>,
    /// The shell's reachability (`probe.connection.is_ok()`) as of the last
    /// *applied* probe, `None` before the first (#1003).
    ///
    /// Deliberately coarser than `shown` above: `shown` tracks the rendered
    /// banner *text*, so two different failures (`ServiceUnknown` then a
    /// plain `Timeout`, say) are two transitions — the banner's wording
    /// changed and that is worth a log line. `reachable` only asks Ok-vs-Err,
    /// so that same pair is *one* state as far as `reachable_listener` below
    /// is concerned — a subscriber like `ai_keys_tab` cares whether the
    /// endpoint answers at all, not which sentence explains why it doesn't.
    reachable: Rc<Cell<Option<bool>>>,
    /// Notified with the shell's new reachability whenever `reachable`
    /// changes (#1003) — see [`set_reachable_listener`](Self::set_reachable_listener).
    /// `ai_keys_tab::build_page`'s callback is the one subscriber today,
    /// replacing that tab's own periodic `ListAiKeys` clock: see its module
    /// doc for why a poll cadence of its own reached a prompt-capable keyring
    /// path nothing periodic may touch.
    reachable_listener: ReachableListener,
}

impl ShellProbeUi {
    /// Seed from what is actually on screen: the banner is built revealed,
    /// carrying [`CONNECTING_BANNER`], so the first probe to answer is a
    /// genuine transition either way and gets logged.
    fn new(banner: &adw::Banner, revision: &gtk::Label) -> Self {
        let shown = banner.is_revealed().then(|| banner.title().to_string());
        Self {
            banner: banner.clone(),
            revision: revision.clone(),
            in_flight: Rc::new(Cell::new(false)),
            shown: Rc::new(RefCell::new(shown)),
            ticks: Rc::new(Cell::new(0)),
            reachable: Rc::new(Cell::new(None)),
            reachable_listener: Rc::new(RefCell::new(None)),
        }
    }

    /// Register `listener` to be called with the shell's reachability
    /// whenever it changes, the very first applied probe's outcome included
    /// (#1003). Call this before the first [`poll`](Self::poll) if that first
    /// delivery matters to the caller — `build_window` does.
    ///
    /// Replaces any previously registered listener; today there is only ever
    /// one caller (`build_window`, for `ai_keys_tab`), so this is a plain
    /// setter rather than a list of subscribers.
    fn set_reachable_listener(&self, listener: impl Fn(bool) + 'static) {
        *self.reachable_listener.borrow_mut() = Some(Box::new(listener));
    }

    /// Claim the single in-flight slot for this tick, or `None` because a
    /// probe is still outstanding. The returned guard releases the slot when
    /// it drops.
    fn claim(&self) -> Option<InFlightSlot> {
        if self.in_flight.replace(true) {
            return None;
        }
        Some(InFlightSlot(self.in_flight.clone()))
    }

    /// Probe the shell and apply the outcome — unless one is already in
    /// flight, in which case this tick is skipped.
    ///
    /// The slot guard is **moved into the completion closure**, so the slot is
    /// released whether that closure runs or is dropped unrun. That second
    /// case is real: [`spawn_on_runtime`] calls its callback only `if let
    /// Ok(v) = rx.await`, so a sender dropped without sending (a panic inside
    /// [`probe_shell_status`], runtime teardown) would otherwise leave
    /// `in_flight` stuck at `true` and every later tick taking the skip path
    /// forever — freezing the banner and footer in whatever state they were
    /// last in, which is #989's own symptom reintroduced by the mechanism that
    /// fixes it (the review of `32bf073`, LOW 3).
    fn poll(&self) {
        self.ticks.set(self.ticks.get().saturating_add(1));
        let Some(slot) = self.claim() else {
            tracing::debug!("a shell probe is still in flight — skipping this tick");
            return;
        };
        let ui = self.clone();
        spawn_on_runtime(probe_shell_status(), move |probe| {
            drop(slot);
            ui.apply(&probe);
        });
    }

    /// Reflect one probe outcome into the banner and the footer.
    ///
    /// [`format_banner_message`]'s `None`/`Some` contract drives the banner in
    /// **both** directions (#989): `Some` reveals it with the text, `None`
    /// hides it. That is what makes a shell started after the control-center
    /// clear the banner, and a shell that dies mid-session bring it back —
    /// from the same call, with no separate "has it been shown yet?" state.
    ///
    /// Split out from [`poll`](Self::poll) so a `gtk_test` can drive a
    /// sequence of outcomes through it with no session bus to answer `Ping`.
    fn apply(&self, probe: &ShellProbe) {
        let message = format_banner_message(&probe.connection);
        if *self.shown.borrow() != message {
            self.shown.replace(message.clone());
            match &probe.connection {
                Err(err) => tracing::info!(%err, "trollshell control endpoint unreachable"),
                Ok((_pong, version)) => {
                    tracing::info!(%version, "trollshell control endpoint answered");
                }
            }
        }
        // #1003: a coarser transition than `shown` above — see `reachable`'s
        // field doc for why a failure-reason change alone must not fire this.
        // Checked unconditionally, not folded into the `shown` branch above:
        // `shown`'s very first value is seeded from what's already on screen
        // (`new`), so it can start non-`None` and suppress that first log
        // line, but `reachable` always starts at `None` and so always
        // delivers the first probe's outcome to a fresh listener.
        let reachable = probe.connection.is_ok();
        if self.reachable.replace(Some(reachable)) != Some(reachable)
            && let Some(listener) = self.reachable_listener.borrow().as_ref()
        {
            listener(reachable);
        }
        match &message {
            Some(text) => {
                self.banner.set_title(text);
                self.banner.set_revealed(true);
            }
            None => self.banner.set_revealed(false),
        }
        let version = probe
            .connection
            .as_ref()
            .ok()
            .map(|(_pong, version)| version.as_str());
        self.revision
            .set_text(&format_revision_footer(version, probe.revision.as_deref()));
    }
}

/// What (if anything) a transitions-only poller should log when its latest
/// outcome (`is_err`) is compared against `previous` — the last **applied**
/// outcome, `None` before the first completion (#1017).
///
/// Lifted out of `ai_keys_tab`'s original private copy (#1015) so a third
/// poller — the Plugins tab's own `ListPlugins` poll (#1017) — imports the
/// actual helper instead of hand-copying the same ten lines a second time.
/// The #1017 review (LOW 3) caught that first cut leaving `ai_keys_tab` on
/// its own byte-identical copy — a consolidation that named itself as one but
/// wasn't — so `ai_keys_tab` now imports this definition too; both tabs'
/// `gtk_tests` keep their own journal-capturing integration test (the "second
/// caller" and "third caller" each still get their own falsifiable wiring
/// check, just against the one shared rule rather than a hand-copy of it).
///
/// Pure so the rule is unit-tested with no display server and no `tracing`
/// subscriber (see `banner_message_tests` below). `ShellProbeUi::apply`
/// above predates this and keeps its own inline, message-*text*-keyed
/// version (`shown`): this is deliberately coarser, asking only Ok-vs-Err —
/// the same simplification `ShellProbeUi::reachable`'s own field doc makes
/// for why a failure-*reason* change alone isn't worth a second log line to
/// a consumer that only cares whether the endpoint answers at all.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LogTransition {
    /// Same outcome as last time (or the very first poll succeeded) —
    /// nothing to say.
    None,
    /// A run of successes (or the very first poll) just started failing.
    Failed,
    /// A run of failures just started succeeding again.
    Recovered,
}

/// See [`LogTransition`]'s doc for the shape and why this is shared.
pub(crate) fn log_transition(previous: Option<bool>, is_err: bool) -> LogTransition {
    if previous == Some(is_err) {
        return LogTransition::None;
    }
    if is_err {
        LogTransition::Failed
    } else if previous == Some(true) {
        LogTransition::Recovered
    } else {
        // The very first poll ever, and it succeeded: matches the pre-#1017
        // behaviour of never logging a bare success.
        LogTransition::None
    }
}

/// Format the connection banner's text from a [`probe_shell`] outcome. `None`
/// means "hide the banner" (the shell answered); `Some` carries the text to
/// show.
///
/// Pure so the three cases are unit-tested without a live D-Bus call — see
/// the `banner_message_tests` module below. Distinguishes the shell simply
/// not running (`ServiceUnknown` — nothing owns `CONTROL_NAME`, see
/// [`is_shell_not_running`]) from any other bus failure, which gets a literal
/// `Connection error: {err}` rather than being folded into the same "not
/// running" text — per #959, only a state the bus layer actually reports.
///
/// #989 leans on the `None`/`Some` split as a *two-way* contract rather than a
/// one-shot: [`ShellProbeUi::apply`] runs it on every probe, so the same
/// function that first revealed the banner is what later hides it, and what
/// reveals it again if the shell goes away.
fn format_banner_message(probe: &Result<(String, String), hytte_bus::BusError>) -> Option<String> {
    match probe {
        Ok(_) => None,
        Err(err) if is_shell_not_running(err) => {
            Some("trollshell is not running — start the shell to manage it".to_owned())
        }
        Err(err) => Some(format!("Connection error: {err}")),
    }
}

/// True when `err` is the D-Bus daemon's reply for "nothing owns this bus
/// name" (`org.freedesktop.DBus.Error.ServiceUnknown`) — i.e. the shell
/// process isn't running, as distinct from a real connection problem (a
/// transient bus hiccup, `AccessDenied`, a misbehaving `Control` endpoint's own
/// method failure). Confirmed empirically against a real `dbus-daemon`: a call
/// to a destination with no owner surfaces as exactly this
/// `BusError::Permanent` shape, not `Transient`.
fn is_shell_not_running(err: &hytte_bus::BusError) -> bool {
    matches!(
        err,
        hytte_bus::BusError::Permanent { dbus_name, .. }
            if dbus_name.as_deref() == Some("org.freedesktop.DBus.Error.ServiceUnknown")
    )
}

/// Call `Ping` then `Version` on the shell's control interface. Returns the
/// `(pong, version)` pair, or the first `BusError` (e.g. the shell isn't
/// running, so the name has no owner).
async fn probe_shell() -> Result<(String, String), hytte_bus::BusError> {
    let pong = control_call("Ping").await?;
    let version = control_call("Version").await?;
    Ok((pong, version))
}

/// One typed String-returning method call against the control interface. Short
/// timeout + no retry: the companion is interactive, so a missing shell should
/// resolve to "not running" quickly rather than hang the banner.
async fn control_call(method: &str) -> Result<String, hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method(method)
        .timeout(Duration::from_secs(3))
        .retry(RetryPolicy::Never)
        .send::<String>()
        .await
}

/// Run `fut` on the shared hytte tokio runtime and deliver its result to
/// `on_done` back on the GTK main thread. The D-Bus work stays off the UI
/// thread; the reply crosses back over a oneshot glib's executor awaits. If the
/// receiver is dropped first (window closed), `on_done` simply never runs.
pub(crate) fn spawn_on_runtime<T, Fut, F>(fut: Fut, on_done: F)
where
    T: Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    F: FnOnce(T) + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    hytte_reactive::runtime::handle().spawn(async move {
        let _ = tx.send(fut.await);
    });
    glib::spawn_future_local(async move {
        if let Ok(v) = rx.await {
            on_done(v);
        }
    });
}

/// `GetPlace` → `(label, auto)`: the resolved place label and whether
/// auto-location is in force.
pub(crate) async fn get_place() -> Result<(String, bool), hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("GetPlace")
        .timeout(Duration::from_secs(3))
        .retry(RetryPolicy::Never)
        .send::<(String, bool)>()
        .await
}

/// `SetManualCity(city)`: switch to manual location and forward-geocode `city`
/// shell-side. A slightly longer timeout than the others — the shell does a
/// network geocode as part of applying it.
pub(crate) async fn set_manual_city(city: String) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("SetManualCity")
        .args((city,))
        .timeout(Duration::from_secs(5))
        .retry(RetryPolicy::Never)
        .send::<()>()
        .await
}

/// `SetAutoLocation(auto)`: toggle auto (`GeoClue`) vs. manual location.
pub(crate) async fn set_auto_location(auto: bool) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("SetAutoLocation")
        .args((auto,))
        .timeout(Duration::from_secs(3))
        .retry(RetryPolicy::Never)
        .send::<()>()
        .await
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::filter::LevelFilter;

    use super::{
        DEFAULT_LOG_LEVEL, LogTransition, build_env_filter, format_banner_message,
        format_revision_footer, is_shell_not_running, log_transition, should_probe_revision,
    };

    // #780: with `RUST_LOG` unset, the effective filter must default to
    // `DEFAULT_LOG_LEVEL` (currently `INFO`), not `tracing-subscriber`'s own
    // hard-coded `ERROR` fallback (what a bare `fmt::init()` /
    // `EnvFilter::from_default_env()` produces).
    //
    // Unlike #766's shell-binary tests (`trollshell/src/main.rs`), which
    // both drove `build_env_filter` through its `Some(_)` arm and left the
    // `None` arm — the one `main` actually calls — unexercised, this test
    // calls `build_env_filter(None)` directly, which reads the *real*
    // process `RUST_LOG` (there's no way around that for the `None` arm
    // specifically — that's the whole point of exercising it). `cargo test`
    // inherits the parent shell's environment, and this repo's own
    // `CLAUDE.md` documents exporting `RUST_LOG` for local debugging
    // (`RUST_LOG=hytte_services=debug,trollshell=debug cargo run`), so a
    // developer with it exported would otherwise see this test assert a
    // default that is correctly *not* in effect. Skip rather than assert in
    // that case — `rust_log_override_still_wins` below already covers "an
    // ambient/explicit `RUST_LOG` wins over the default".
    #[test]
    fn default_log_level_is_not_error_for_the_none_arm() {
        if std::env::var_os("RUST_LOG").is_some() {
            return;
        }
        let filter = build_env_filter(None);
        assert_eq!(filter.max_level_hint(), Some(DEFAULT_LOG_LEVEL));
    }

    // `RUST_LOG` must still win over the default when set — mirrors #766's
    // override-path test for the shell binary.
    #[test]
    fn rust_log_override_still_wins() {
        let filter = build_env_filter(Some("trollshell_control_center=trace"));
        assert_eq!(filter.max_level_hint(), Some(LevelFilter::TRACE));
    }

    // ── Revision footer (#601, extended by #959 to add Version) ───────────

    #[test]
    fn footer_renders_version_and_revision() {
        assert_eq!(
            format_revision_footer(Some("0.1.0"), Some("34e3d96")),
            "trollshell 0.1.0 · revision 34e3d96"
        );
    }

    #[test]
    fn footer_renders_a_dirty_tree_hash_unmodified() {
        assert_eq!(
            format_revision_footer(Some("0.1.0"), Some("34e3d96-dirty")),
            "trollshell 0.1.0 · revision 34e3d96-dirty"
        );
    }

    // #601: `"dev"` is `trollshell/src/revision.rs`'s documented fallback for
    // an unstamped local `cargo run`/`cargo build`. It must render as-is, not
    // get hidden or swapped for a friendlier placeholder — seeing `dev` from a
    // deployed shell is itself the useful signal ("this wasn't built by nix").
    #[test]
    fn footer_renders_the_dev_fallback_honestly() {
        assert_eq!(
            format_revision_footer(Some("0.1.0"), Some("dev")),
            "trollshell 0.1.0 · revision dev"
        );
    }

    // A revision value that is literally the string "unknown" (not to be
    // confused with the `revision: None` case below, which synthesizes that
    // same word) must still pass through unmodified.
    #[test]
    fn footer_renders_unknown_passthrough() {
        assert_eq!(
            format_revision_footer(Some("0.1.0"), Some("unknown")),
            "trollshell 0.1.0 · revision unknown"
        );
    }

    // #959: `Version` succeeded but `Revision` didn't (or vice versa) — render
    // what's available rather than discarding it because its sibling call
    // failed.
    #[test]
    fn footer_renders_version_only() {
        assert_eq!(
            format_revision_footer(Some("0.1.0"), None),
            "trollshell 0.1.0 · revision unknown"
        );
    }

    #[test]
    fn footer_renders_revision_only() {
        assert_eq!(
            format_revision_footer(None, Some("34e3d96")),
            "revision 34e3d96"
        );
    }

    // The shell-not-running case: both calls failed, so both are `None`. Must
    // render an honest "unavailable" state, not a blank label or a leftover
    // stale value from a previous connection — and must match
    // `docs/live-verify.md`'s #601/#836 entry byte-for-byte, since #959
    // deliberately kept this string unchanged.
    #[test]
    fn footer_falls_back_when_shell_is_not_running() {
        assert_eq!(
            format_revision_footer(None, None),
            "Shell revision: unavailable (trollshell not running)"
        );
    }

    // ── Connection banner (#959) ────────────────────────────────────────────

    #[test]
    fn banner_hides_on_a_successful_probe() {
        let probe: Result<(String, String), hytte_bus::BusError> =
            Ok(("pong".to_owned(), "0.1.0".to_owned()));
        assert_eq!(format_banner_message(&probe), None);
    }

    #[test]
    fn banner_shows_not_running_for_service_unknown() {
        let probe: Result<(String, String), hytte_bus::BusError> =
            Err(hytte_bus::BusError::Permanent {
                reason: "The name mov.vibec0re.trollshell.Control was not provided by any \
                         .service files"
                    .to_owned(),
                dbus_name: Some("org.freedesktop.DBus.Error.ServiceUnknown".to_owned()),
            });
        assert_eq!(
            format_banner_message(&probe).as_deref(),
            Some("trollshell is not running — start the shell to manage it")
        );
    }

    // A permanent bus failure that is NOT the shell simply being absent (e.g.
    // `AccessDenied`, or a `Control` method itself failing) must not be folded
    // into the "not running" text — it gets its own, honest message instead.
    #[test]
    fn banner_shows_a_distinct_message_for_other_bus_errors() {
        let probe: Result<(String, String), hytte_bus::BusError> =
            Err(hytte_bus::BusError::Permanent {
                reason: "not authorised".to_owned(),
                dbus_name: Some("org.freedesktop.DBus.Error.AccessDenied".to_owned()),
            });
        let message = format_banner_message(&probe).expect("must show a message");
        assert!(
            message.starts_with("Connection error:"),
            "expected a distinct connection-error message, got {message:?}"
        );
        assert!(
            !message.contains("not running"),
            "must not be folded into the not-running text: {message:?}"
        );
    }

    #[test]
    fn is_shell_not_running_is_true_only_for_service_unknown() {
        let service_unknown = hytte_bus::BusError::Permanent {
            reason: "unused".to_owned(),
            dbus_name: Some("org.freedesktop.DBus.Error.ServiceUnknown".to_owned()),
        };
        assert!(is_shell_not_running(&service_unknown));

        let access_denied = hytte_bus::BusError::Permanent {
            reason: "unused".to_owned(),
            dbus_name: Some("org.freedesktop.DBus.Error.AccessDenied".to_owned()),
        };
        assert!(!is_shell_not_running(&access_denied));

        let no_name = hytte_bus::BusError::Permanent {
            reason: "unused".to_owned(),
            dbus_name: None,
        };
        assert!(!is_shell_not_running(&no_name));
    }

    // ── The probe's second call (#989) ──────────────────────────────────────

    // The one performance property `probe_shell_status` buys: a tick against a
    // shell that isn't there costs one fast `ServiceUnknown`, not that plus a
    // second 3 s timeout stacked behind it. The predicate is named precisely
    // so this is reachable — the async fn around it is a bare `hytte_bus::call`
    // chain no hermetic test can drive (the review of `32bf073`, LOW 4).
    #[test]
    fn a_failed_connection_probe_skips_the_revision_call() {
        let down: Result<(String, String), hytte_bus::BusError> =
            Err(hytte_bus::BusError::Permanent {
                reason: "unused".to_owned(),
                dbus_name: Some("org.freedesktop.DBus.Error.ServiceUnknown".to_owned()),
            });
        assert!(
            !should_probe_revision(&down),
            "a Control that did not answer Ping will not answer Revision either"
        );
    }

    // …and the other direction, so the skip is not simply "never ask".
    #[test]
    fn a_reachable_shell_is_still_asked_for_its_revision() {
        let up: Result<(String, String), hytte_bus::BusError> =
            Ok(("pong".to_owned(), "0.1.0".to_owned()));
        assert!(should_probe_revision(&up));
    }

    // ── Transitions-only logging (#1017, shared with `plugins_tab`) ─────────

    #[test]
    fn a_fresh_poller_failing_for_the_first_time_is_logged() {
        assert_eq!(log_transition(None, true), LogTransition::Failed);
    }

    #[test]
    fn a_fresh_poller_succeeding_for_the_first_time_is_quiet() {
        // Matches the pre-#1017 behaviour: a bare success was never logged.
        assert_eq!(log_transition(None, false), LogTransition::None);
    }

    #[test]
    fn a_repeated_failure_is_not_logged_again() {
        assert_eq!(log_transition(Some(true), true), LogTransition::None);
    }

    #[test]
    fn a_repeated_success_is_not_logged_again() {
        assert_eq!(log_transition(Some(false), false), LogTransition::None);
    }

    #[test]
    fn recovering_from_a_failure_is_logged() {
        assert_eq!(log_transition(Some(true), false), LogTransition::Recovered);
    }

    #[test]
    fn a_success_going_to_failure_is_logged() {
        assert_eq!(log_transition(Some(false), true), LogTransition::Failed);
    }
}

/// The banner/footer probe's widget-level behaviour (#989).
///
/// Gated on `system-tests` for the same reason [`plugins_tab`]'s module is:
/// `AdwBanner`'s revealed state is a real widget property, so "the banner
/// hides, then comes back" cannot be asserted without a display server. The
/// *text* for each individual outcome is already pinned hermetically by
/// `tests` above — what these add is the **sequence**, which is the whole of
/// what #989 broke.
#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use gtk::glib;

    use super::{
        CONNECTING_BANNER, ShellProbe, ShellProbeUi, build_revision_footer, install_shell_probe,
    };
    use crate::test_support::captured_logs;

    const NOT_RUNNING: &str = "trollshell is not running — start the shell to manage it";
    const UNAVAILABLE_FOOTER: &str = "Shell revision: unavailable (trollshell not running)";

    /// A probe that reached the shell.
    fn connected() -> ShellProbe {
        ShellProbe {
            connection: Ok(("pong".to_owned(), "0.1.0".to_owned())),
            revision: Some("34e3d96".to_owned()),
        }
    }

    /// A probe against a `Control` name nobody owns — the shell is not
    /// running. Exactly the `BusError` shape a real `dbus-daemon` produces for
    /// a destination with no owner (#959 confirmed it empirically).
    fn not_running() -> ShellProbe {
        ShellProbe {
            connection: Err(hytte_bus::BusError::Permanent {
                reason: "The name mov.vibec0re.trollshell.Control was not provided by any \
                         .service files"
                    .to_owned(),
                dbus_name: Some("org.freedesktop.DBus.Error.ServiceUnknown".to_owned()),
            }),
            revision: None,
        }
    }

    /// The banner and footer as `build_window` wires them, plus the probe
    /// state that drives both.
    fn probe_ui() -> (adw::Banner, gtk::Label, ShellProbeUi) {
        let banner = adw::Banner::builder()
            .title(CONNECTING_BANNER)
            .revealed(true)
            .build();
        let (_bar, label) = build_revision_footer();
        let ui = ShellProbeUi::new(&banner, &label);
        (banner, label, ui)
    }

    /// #989 in one sequence: launched before the shell, then the shell starts,
    /// then it dies, then it comes back. The banner has to follow all four
    /// steps, and the footer with it.
    ///
    /// Before this fix only the first step ever ran — the probes were one-shot
    /// from `build_window` — so steps 2 and 3 are precisely the two
    /// divergences the issue reports: a "not running" banner sitting over a
    /// Plugins tab full of live plugins, and a banner that never reappears
    /// after the shell dies.
    ///
    /// Falsified by removing the re-probe (dropping the
    /// `glib::timeout_add_local(SHELL_PROBE_INTERVAL, …)` in `build_window`)
    /// — but *that* mutation is invisible to a test driving `apply` directly,
    /// so the honest falsification is at this level: no-op `apply`'s
    /// `set_revealed` calls and this fails at step 2.
    #[gtk::test]
    fn the_banner_reveals_and_hides_in_both_directions() {
        adw::init().expect("libadwaita init");
        let (banner, label, ui) = probe_ui();

        // 1. Launched before the shell.
        ui.apply(&not_running());
        assert!(
            banner.is_revealed(),
            "a failed probe must reveal the banner"
        );
        assert_eq!(banner.title().as_str(), NOT_RUNNING);
        assert_eq!(label.text().as_str(), UNAVAILABLE_FOOTER);

        // 2. `systemctl --user start trollshell` — the next probe must HIDE
        //    the banner, not leave it pinned for the session.
        ui.apply(&connected());
        assert!(
            !banner.is_revealed(),
            "#989: a later successful probe must hide a banner an earlier failure revealed"
        );
        assert_eq!(
            label.text().as_str(),
            "trollshell 0.1.0 · revision 34e3d96",
            "the footer must follow the same probe"
        );

        // 3. The shell dies mid-session — the banner must come BACK.
        ui.apply(&not_running());
        assert!(
            banner.is_revealed(),
            "#989: a failure after a success must re-reveal the banner"
        );
        assert_eq!(banner.title().as_str(), NOT_RUNNING);
        assert_eq!(label.text().as_str(), UNAVAILABLE_FOOTER);

        // 4. …and back again, as many times as the shell restarts.
        ui.apply(&connected());
        assert!(!banner.is_revealed());
        assert_eq!(label.text().as_str(), "trollshell 0.1.0 · revision 34e3d96");
    }

    /// The startup case on its own: the window is built with the banner
    /// revealed and reading "Connecting to trollshell…", so a first probe that
    /// succeeds has to clear it rather than leave that placeholder up.
    #[gtk::test]
    fn a_first_successful_probe_clears_the_connecting_placeholder() {
        adw::init().expect("libadwaita init");
        let (banner, label, ui) = probe_ui();
        assert!(banner.is_revealed(), "sanity: built revealed");
        assert_eq!(banner.title().as_str(), CONNECTING_BANNER);

        ui.apply(&connected());

        assert!(!banner.is_revealed());
        assert_eq!(label.text().as_str(), "trollshell 0.1.0 · revision 34e3d96");
    }

    /// A bus error that is *not* "nothing owns this name" keeps its own text
    /// across the transition, and still clears on the next success — #959's
    /// distinction has to survive being applied repeatedly.
    #[gtk::test]
    fn a_connection_error_banner_also_clears_on_the_next_success() {
        adw::init().expect("libadwaita init");
        let (banner, _label, ui) = probe_ui();

        ui.apply(&ShellProbe {
            connection: Err(hytte_bus::BusError::Permanent {
                reason: "not authorised".to_owned(),
                dbus_name: Some("org.freedesktop.DBus.Error.AccessDenied".to_owned()),
            }),
            revision: None,
        });
        assert!(banner.is_revealed());
        let title = banner.title();
        assert!(
            title.as_str().starts_with("Connection error:"),
            "expected the distinct connection-error text, got {title:?}"
        );

        ui.apply(&connected());
        assert!(!banner.is_revealed());
    }

    /// The probes must not stack: a tick that lands while a slow round trip is
    /// still out has to skip, and the slot has to be released when that round
    /// trip completes.
    ///
    /// Falsified by making `claim` return `Some` unconditionally: the second
    /// assertion fails, and in production each tick would then pile another
    /// three-call probe (with 3 s timeouts) on top of the last.
    #[gtk::test]
    fn only_one_probe_is_in_flight_at_a_time() {
        adw::init().expect("libadwaita init");
        let (_banner, _label, ui) = probe_ui();

        let slot = ui.claim().expect("the first tick must probe");
        assert!(
            ui.claim().is_none(),
            "a tick landing on a still-outstanding probe must skip rather than stack a second"
        );

        // What the completion does — by dropping the guard, not by hand.
        drop(slot);
        assert!(
            ui.claim().is_some(),
            "the slot must be free again once the probe completes"
        );
    }

    /// The slot is released by [`super::InFlightSlot`]'s `Drop`, so a
    /// completion that is *discarded rather than run* frees it too.
    ///
    /// `spawn_on_runtime` invokes its callback only `if let Ok(v) = rx.await`,
    /// so a sender dropped without sending (a panic inside the probe, runtime
    /// teardown) drops the closure — and with it the guard it owns — without
    /// ever calling it. Before this guard existed the release lived *inside*
    /// that callback, so such a probe wedged `in_flight` at `true` and every
    /// later tick skipped forever, freezing the banner in whatever state it
    /// was last in: #989's own symptom, reintroduced by #989's fix (the review
    /// of `32bf073`, LOW 3).
    ///
    /// Falsified by emptying `InFlightSlot::drop`'s body: the last assertion
    /// fails.
    #[gtk::test]
    fn a_discarded_completion_still_releases_the_slot() {
        adw::init().expect("libadwaita init");
        let (_banner, _label, ui) = probe_ui();

        // Exactly what `poll` builds: a guard owned by the completion closure.
        let slot = ui.claim().expect("the first tick must probe");
        let completion: Box<dyn FnOnce(ShellProbe)> = Box::new(move |_probe| {
            drop(slot);
        });
        assert!(
            ui.claim().is_none(),
            "sanity: the slot is held while the closure is alive"
        );

        // The oneshot's sender went away, so `spawn_on_runtime` drops the
        // callback instead of calling it.
        drop(completion);

        assert!(
            ui.claim().is_some(),
            "a completion dropped without running must not wedge the probe forever"
        );
    }

    /// The #780 property this PR explicitly buys: at a 2 s cadence the probe
    /// logs on **transitions**, not on every tick.
    ///
    /// Without the guard, a shell left down overnight writes one `INFO` line
    /// every two seconds — 1 800 an hour — into the user's journal. Counting
    /// real emitted lines rather than inspecting `shown`, because a mutation
    /// that drops the guard while keeping `shown` correct would sail past a
    /// state assertion (the review of `32bf073`, LOW 5).
    ///
    /// Falsified by removing `apply`'s `if *self.shown.borrow() != message`
    /// guard: four probes then emit four lines and the first assertion fails.
    #[gtk::test]
    fn the_probe_logs_transitions_not_every_tick() {
        adw::init().expect("libadwaita init");
        let (_banner, _label, ui) = probe_ui();

        let lines = captured_logs(|| {
            // Down, and staying down: one line, not four.
            ui.apply(&not_running());
            ui.apply(&not_running());
            ui.apply(&not_running());
            ui.apply(&not_running());
        });
        assert_eq!(
            lines.len(),
            1,
            "a shell that stays down must log once, not once per tick: {lines:#?}"
        );
        assert!(
            lines[0].contains("unreachable"),
            "the one line must be the outage: {:?}",
            lines[0]
        );

        // …and a real change still speaks up, so the guard is not "never log".
        let lines = captured_logs(|| {
            ui.apply(&connected());
            ui.apply(&connected());
        });
        assert_eq!(
            lines.len(),
            1,
            "the shell coming back is a transition and must log exactly once: {lines:#?}"
        );
        assert!(
            lines[0].contains("answered"),
            "the one line must be the recovery: {:?}",
            lines[0]
        );
    }

    /// The timer itself: the probe has to keep firing, not run once. This is
    /// the mechanism #989 was missing, and the one every other test in this
    /// module is blind to — they all drive `apply` by hand.
    ///
    /// The in-flight slot is held for the whole test so each tick takes the
    /// skip path: there is no session bus here to answer `Ping`, and this is
    /// about the timer re-firing, not about what a probe returns.
    ///
    /// Falsified by making `install_shell_probe`'s closure return
    /// `glib::ControlFlow::Break` (a one-shot timer, i.e. the pre-#989
    /// behaviour): the tick count stops at 1 and this fails at the deadline.
    ///
    /// The loop drains the context **non-blocking** and sleeps between passes
    /// rather than calling `iteration(true)`: a broken timer leaves nothing to
    /// dispatch, and a blocking iteration would then park forever — hanging
    /// not just this test but every `#[gtk::test]` sharing the main context,
    /// which is a hang rather than a red line and so no falsification at all.
    #[gtk::test]
    fn the_probe_re_runs_on_its_interval() {
        adw::init().expect("libadwaita init");
        let (_banner, _label, ui) = probe_ui();

        let _slot = ui
            .claim()
            .expect("hold the in-flight slot for the whole test");
        let source = install_shell_probe(&ui, Duration::from_millis(5));
        let deadline = Instant::now() + Duration::from_secs(2);
        while ui.ticks.get() < 3 && Instant::now() < deadline {
            while glib::MainContext::default().iteration(false) {}
            std::thread::sleep(Duration::from_millis(1));
        }
        let ticks = ui.ticks.get();
        // Look the source up rather than `SourceId::remove()`, which unwraps:
        // under the falsifying mutation the source has already destroyed
        // itself, and a panic there would pre-empt the assertion below and
        // report the wrong reason for the red.
        if let Some(source) = glib::MainContext::default().find_source_by_id(&source) {
            source.destroy();
        }

        assert!(
            ticks >= 3,
            "the probe must keep re-running on its interval; it fired {ticks} time(s)"
        );
    }

    // ── The reachability listener (#1003) ────────────────────────────────────

    /// The mechanism that replaced `ai_keys_tab`'s own 2 s timer: a listener
    /// registered via `set_reachable_listener` must fire exactly on a genuine
    /// Ok↔Err transition — the first applied probe's outcome included — and
    /// stay quiet both on a repeat of the same outcome and on an error whose
    /// *text* changes but whose reachability doesn't (the coarsening
    /// `reachable`'s field doc calls out).
    ///
    /// Falsified by commenting out the `listener(reachable)` call in `apply`:
    /// every assertion below fails on an empty `events` vec — with no
    /// periodic timer left in `ai_keys_tab`, this is now the *only* path that
    /// can ever re-read `ListAiKeys`, so a silent listener silently
    /// reintroduces #1003 in full.
    #[gtk::test]
    fn the_reachable_listener_fires_only_on_a_reachability_transition() {
        adw::init().expect("libadwaita init");
        let (_banner, _label, ui) = probe_ui();
        let events: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let events = events.clone();
            ui.set_reachable_listener(move |reachable| events.borrow_mut().push(reachable));
        }

        ui.apply(&not_running());
        assert_eq!(
            *events.borrow(),
            vec![false],
            "the first applied probe's outcome must reach a listener registered before it"
        );

        ui.apply(&not_running());
        assert_eq!(
            *events.borrow(),
            vec![false],
            "an unchanged outcome must not fire the listener again"
        );

        // A different failure *reason* — the banner's `shown` text would
        // change here, but reachability itself does not, and this must stay
        // quiet (the coarsening is deliberate, not a gap).
        ui.apply(&ShellProbe {
            connection: Err(hytte_bus::BusError::Permanent {
                reason: "not authorised".to_owned(),
                dbus_name: Some("org.freedesktop.DBus.Error.AccessDenied".to_owned()),
            }),
            revision: None,
        });
        assert_eq!(
            *events.borrow(),
            vec![false],
            "an error-text change alone must not fire the reachability listener"
        );

        ui.apply(&connected());
        assert_eq!(
            *events.borrow(),
            vec![false, true],
            "the shell coming up must fire the listener"
        );

        ui.apply(&connected());
        assert_eq!(
            *events.borrow(),
            vec![false, true],
            "an unchanged reachable outcome must not fire the listener again"
        );

        ui.apply(&not_running());
        assert_eq!(
            *events.borrow(),
            vec![false, true, false],
            "the shell going down must fire the listener too"
        );
    }
}
