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
//! banner at all. The **Places** tab ([`places_tab`], #640/#703) is
//! a full editor for `~/.config/trollshell/places.toml` — the named places that
//! drive departures, Wi-Fi-fingerprint place detection and walk time — plus the
//! session-only weather-location override this tab used to be (#391). It is the
//! one tab that does **not** go through `Control`: it reads and writes the file
//! directly, so it keeps working while the shell is down. The **Plugins** tab
//! (#348) lists each `trollshell-plugin-<id>` systemd **user** unit with a
//! switch that starts/enables or stops/disables it. The **AI Keys** tab (#392)
//! stores the LLM-backed plugins' API keys in the login keyring
//! (gnome-keyring/libsecret) — never on disk — and rotates them. Those
//! round-trip over `Control`. There is deliberately **no Display tab**: #393
//! re-scoped display management away from a bespoke control-center page and
//! onto `org.gnome.Mutter.DisplayConfig`, a shim over niri-ipc
//! (`crates/hytte-services/src/display_config.rs`) that lets
//! **gnome-control-center's own Display panel** drive niri outputs directly —
//! compatmaxx: reuse the existing GNOME client, provide the backend. When the
//! shell isn't running the app degrades gracefully rather than panicking.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use hytte_bus::RetryPolicy;

mod places_tab;
mod plugins_tab;

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
    // the login keyring, round-tripped over Control.
    let ai_keys_page = build_ai_keys_page();
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
        .title("Connecting to trollshell…")
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

    // The tab poll timers are scoped to this window: drop them on close so a
    // dismissed window stops polling `Control` (Plugins) and stat'ing
    // `places.toml` (Places), and a re-launch while another window is still
    // resident can't leave the first window's timers double-polling behind it
    // (#542). Wrapped in a cell + `.take()` so the one-shot removal is clean
    // under the `Fn` close handler.
    let polls = RefCell::new(vec![plugins_poll, places_poll]);
    window.connect_close_request(move |_| {
        for source in polls.take() {
            source.remove();
        }
        glib::Propagation::Proceed
    });

    check_shell_connection(&banner);
    check_shell_revision(&revision_label);
    window.present();
}

// ── Revision footer (#601) ────────────────────────────────────────────────────

/// Build the footer bar: a single dim, end-aligned label reporting the running
/// shell's build revision. Deliberately small and unobtrusive — a footer, not a
/// tab or dialog — and refreshed once at startup alongside the connection
/// banner (see [`check_shell_revision`]).
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

/// Probe `Control.Version` and `Control.Revision` on the shared tokio runtime
/// and set the footer label from the combined result. Mirrors
/// [`check_shell_connection`]'s shape (spawn on the runtime, deliver back over
/// [`spawn_on_runtime`]'s oneshot).
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
fn check_shell_revision(label: &gtk::Label) {
    let label = label.clone();
    spawn_on_runtime(
        async {
            let version = version().await;
            let revision = revision().await;
            (version, revision)
        },
        move |(version, revision)| {
            label.set_text(&format_revision_footer(
                version.ok().as_deref(),
                revision.ok().as_deref(),
            ));
        },
    );
}

/// `Version` → the running shell's reported version (#959; the same call
/// [`probe_shell`] uses for the banner).
async fn version() -> Result<String, hytte_bus::BusError> {
    control_call("Version").await
}

/// `Revision` → the running shell's build git revision (#601). See
/// [`check_shell_revision`] for why this is a plain `Control` round trip and
/// not a local resolve.
async fn revision() -> Result<String, hytte_bus::BusError> {
    control_call("Revision").await
}

// ── AI Keys tab (#392) ─────────────────────────────────────────────────────

/// The LLM providers the AI Keys tab manages, `(slot, label, help)`. The `slot`
/// is the provider name the shell stores the key under and injects as
/// `<SLOT>_API_KEY` at plugin launch — for `openrouter` that's
/// `OPENROUTER_API_KEY`, exactly what the pet and caw plugins read. Add a row
/// here to surface a new provider.
const KNOWN_AI_PROVIDERS: &[(&str, &str, &str)] = &[(
    "openrouter",
    "OpenRouter",
    "Cloud LLM used by the pet and caw plugins. Create a key at openrouter.ai.",
)];

/// Build the **AI Keys** tab: one password-entry row per known provider. Each
/// row stores a key in the shell's keyring (`SetAiKey` over `Control`) and shows
/// whether a key is currently stored (`ListAiKeys`) — the value itself is never
/// read back. The apply button sets/updates the key (wiping the entry after, so
/// the plaintext isn't retained in the widget); the trash button clears it. When
/// the shell isn't running the calls fail and the rows show "Unavailable".
fn build_ai_keys_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("AI provider keys")
        .description(
            "API keys for the LLM-backed plugins, stored in your login keyring \
             (gnome-keyring/libsecret) — never on disk or in config. A key is \
             injected only into the plugins that declare it, and changing one \
             relaunches those plugins.",
        )
        .build();

    // (slot, entry, status label, clear button) per provider while building.
    let mut built = Vec::new();
    for (slot, label, help) in KNOWN_AI_PROVIDERS {
        let entry = adw::PasswordEntryRow::builder()
            .title(*label)
            .show_apply_button(true)
            .build();
        entry.set_tooltip_text(Some(help));

        let status_lbl = gtk::Label::new(Some("…"));
        status_lbl.add_css_class("dim-label");
        let clear_btn = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Clear the stored key")
            .valign(gtk::Align::Center)
            .sensitive(false)
            .build();
        clear_btn.add_css_class("flat");
        entry.add_suffix(&status_lbl);
        entry.add_suffix(&clear_btn);

        group.add(&entry);
        built.push((*slot, entry, status_lbl, clear_btn));
    }
    page.add(&group);

    // Immutable shared (slot, status label, clear button) list for the refresh.
    let status: Rc<Vec<(String, gtk::Label, gtk::Button)>> = Rc::new(
        built
            .iter()
            .map(|(slot, _entry, lbl, btn)| ((*slot).to_owned(), lbl.clone(), btn.clone()))
            .collect(),
    );

    for (slot, entry, _lbl, clear_btn) in built {
        // Apply → SetAiKey, then wipe the entry (don't keep the plaintext) and
        // re-read the stored-key status.
        {
            let (slot, status) = (slot.to_owned(), status.clone());
            entry.connect_apply(move |e| {
                let value = e.text().to_string();
                if value.is_empty() {
                    return;
                }
                let (e, slot, status) = (e.clone(), slot.clone(), status.clone());
                spawn_on_runtime(set_ai_key(slot, value), move |res| {
                    if let Err(err) = res {
                        tracing::info!(%err, "SetAiKey failed");
                    }
                    e.set_text("");
                    refresh_ai_status(&status);
                });
            });
        }
        // Clear → ClearAiKey, then re-read the status.
        {
            let (slot, status) = (slot.to_owned(), status.clone());
            clear_btn.connect_clicked(move |_| {
                let (slot, status) = (slot.clone(), status.clone());
                spawn_on_runtime(clear_ai_key(slot), move |res| {
                    if let Err(err) = res {
                        tracing::info!(%err, "ClearAiKey failed");
                    }
                    refresh_ai_status(&status);
                });
            });
        }
    }

    refresh_ai_status(&status);
    page
}

/// Re-read which providers have a stored key (`ListAiKeys`) and reflect it into
/// each row's status label + clear-button sensitivity. On failure (shell not
/// running) every row shows "Unavailable".
fn refresh_ai_status(rows: &Rc<Vec<(String, gtk::Label, gtk::Button)>>) {
    let rows = rows.clone();
    spawn_on_runtime(list_ai_keys(), move |res| match res {
        Ok(slots) => {
            let set: std::collections::HashSet<String> = slots.into_iter().collect();
            for (slot, lbl, btn) in rows.iter() {
                let has = set.contains(slot);
                lbl.set_text(if has { "Key stored" } else { "No key set" });
                btn.set_sensitive(has);
            }
        }
        Err(err) => {
            tracing::info!(%err, "ListAiKeys failed");
            for (_slot, lbl, btn) in rows.iter() {
                lbl.set_text("Unavailable");
                btn.set_sensitive(false);
            }
        }
    });
}

/// `ListAiKeys` → the provider slots that currently have a stored key. Values
/// are never returned.
async fn list_ai_keys() -> Result<Vec<String>, hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("ListAiKeys")
        .timeout(Duration::from_secs(3))
        .retry(RetryPolicy::Never)
        .send::<Vec<String>>()
        .await
}

/// `SetAiKey(slot, value)`: store `value` as the key for `slot` in the shell's
/// keyring (which then relaunches the plugins that use it).
async fn set_ai_key(slot: String, value: String) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("SetAiKey")
        .args((slot, value))
        .timeout(Duration::from_secs(5))
        .retry(RetryPolicy::Never)
        .send::<()>()
        .await
}

/// `ClearAiKey(slot)`: delete the stored key for `slot`.
async fn clear_ai_key(slot: String) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("ClearAiKey")
        .args((slot,))
        .timeout(Duration::from_secs(5))
        .retry(RetryPolicy::Never)
        .send::<()>()
        .await
}

/// Probe the running shell's control endpoint on the shared tokio runtime, then
/// update `banner` back on the GTK main thread with the result. Never blocks the
/// UI and never panics when the shell is absent.
///
/// #959: the banner only carries information when the shell is *not*
/// reachable — a successful probe hides it (`set_revealed(false)`) rather than
/// leaving a permanent "Connected to trollshell …" notice up for the entire
/// session. A later poll that succeeds again (there isn't one today, but
/// nothing here assumes there won't be) would hide it again the same way.
fn check_shell_connection(banner: &adw::Banner) {
    let (tx, rx) = tokio::sync::oneshot::channel();

    // The D-Bus call runs on the process-wide hytte tokio runtime; the reply is
    // carried back over a oneshot the GTK main loop awaits below. Awaiting a
    // tokio oneshot receiver needs no runtime context, so it polls cleanly on
    // glib's executor.
    hytte_reactive::runtime::handle().spawn(async move {
        // The receiver is dropped if the window closed first — ignore the send
        // error in that case.
        let _ = tx.send(probe_shell().await);
    });

    let banner = banner.clone();
    glib::spawn_future_local(async move {
        let message = match rx.await {
            Ok(probe) => {
                if let Err(err) = &probe {
                    tracing::info!(%err, "trollshell control endpoint unreachable");
                }
                format_banner_message(&probe)
            }
            Err(_) => {
                // Sender dropped without sending (task cancelled) — unreachable
                // in practice, but degrade to the disconnected message.
                Some("Could not reach trollshell".to_owned())
            }
        };
        match message {
            Some(text) => {
                banner.set_title(&text);
                banner.set_revealed(true);
            }
            None => banner.set_revealed(false),
        }
    });
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
        DEFAULT_LOG_LEVEL, build_env_filter, format_banner_message, format_revision_footer,
        is_shell_not_running,
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
}
