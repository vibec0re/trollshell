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
//! An `adw::ViewStack` of tabs plus a banner that reports whether the shell
//! answered `Ping`/`Version`. The **Place** tab (#391) manages the location that
//! feeds the weather widget — automatic (`GeoClue`) vs. a manual, forward-geocoded
//! city. The **Plugins** tab (#348) lists each `trollshell-plugin-<id>` systemd
//! **user** unit with a switch that starts/enables or stops/disables it. Both
//! round-trip over `Control`. The remaining tabs (AI Keys · Display) are still
//! "coming soon" `StatusPage` stubs, each its own follow-up issue. When the shell
//! isn't running the app degrades gracefully rather than panicking.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use hytte_bus::RetryPolicy;

/// Distinct app-id — this is its own application, not the shell.
const APP_ID: &str = "mov.vibec0re.trollshell.ControlCenter";
/// The shell's dedicated control endpoint (owned by the shell's `control.rs`).
const CONTROL_NAME: &str = "mov.vibec0re.trollshell.Control";
const CONTROL_PATH: &str = "/mov/vibec0re/trollshell/Control";
const CONTROL_IFACE: &str = "mov.vibec0re.trollshell.Control";

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt::init();

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_window);
    app.run()
}

/// Build the main window: a view-switcher over the tabs plus a
/// connection-status banner, then kick off the async shell probe.
fn build_window(app: &adw::Application) {
    let stack = adw::ViewStack::new();
    // The Plugins tab (#348): start/stop/enable each plugin's systemd user unit.
    let plugins_page = build_plugins_page();
    stack.add_titled_with_icon(
        &plugins_page,
        Some("plugins"),
        "Plugins",
        "application-x-addon-symbolic",
    );
    // The Place tab (#391): location management, round-tripped over Control.
    let place_page = build_place_page();
    stack.add_titled_with_icon(
        &place_page,
        Some("place"),
        "Place",
        "mark-location-symbolic",
    );
    add_placeholder(
        &stack,
        "ai-keys",
        "AI Keys",
        "dialog-password-symbolic",
        "Store and rotate API keys for the LLM-backed plugins. Coming soon (#393).",
    );
    add_placeholder(
        &stack,
        "display",
        "Display",
        "video-display-symbolic",
        "Arrange monitors, resolution, and scaling. Coming soon (#392).",
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

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&stack));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("trollshell Control Center")
        .default_width(760)
        .default_height(560)
        .content(&toolbar)
        .build();

    check_shell_connection(&banner);
    window.present();
}

/// Add a "coming soon" placeholder tab to `stack`.
fn add_placeholder(stack: &adw::ViewStack, name: &str, title: &str, icon: &str, description: &str) {
    let page = adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .description(description)
        .build();
    stack.add_titled_with_icon(&page, Some(name), title, icon);
}

// ── Place tab (#391) ────────────────────────────────────────────────────────

/// Build the real **Place** tab: the resolved place, an auto(`GeoClue`)/manual
/// switch, and a manual-city entry, all round-tripping over `Control`
/// (`GetPlace` / `SetAutoLocation` / `SetManualCity`). When the shell isn't
/// running the calls fail and the row shows an "unavailable" hint — no panic.
fn build_place_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Location")
        .description(
            "The location that feeds the weather widget. Automatic uses GeoClue; \
             manual forward-geocodes a city you name.",
        )
        .build();

    let place_row = adw::ActionRow::builder()
        .title("Current place")
        .subtitle("Resolving…")
        .build();
    // Default to "auto" so the pre-connection state matches the shell default;
    // GetPlace corrects it once the shell answers.
    let auto_switch = adw::SwitchRow::builder()
        .title("Automatic location")
        .subtitle("Detect your location automatically (GeoClue)")
        .active(true)
        .build();
    let city_entry = adw::EntryRow::builder()
        .title("Set city manually")
        .show_apply_button(true)
        .build();

    group.add(&place_row);
    group.add(&auto_switch);
    group.add(&city_entry);
    page.add(&group);

    // Guard so programmatically syncing the switch from GetPlace (which fires
    // `active-notify`) doesn't loop back into a `SetAutoLocation` call.
    let syncing = Rc::new(Cell::new(false));

    refresh_place(&place_row, &auto_switch, &syncing);

    // Auto/manual toggle → SetAutoLocation, then re-read the resolved place.
    {
        let place_row = place_row.clone();
        let syncing = syncing.clone();
        auto_switch.connect_active_notify(move |sw| {
            if syncing.get() {
                return;
            }
            let (place_row, sw, syncing) = (place_row.clone(), sw.clone(), syncing.clone());
            spawn_on_runtime(set_auto_location(sw.is_active()), move |res| {
                if let Err(err) = res {
                    tracing::info!(%err, "SetAutoLocation failed");
                }
                refresh_place_soon(&place_row, &sw, &syncing);
            });
        });
    }

    // Manual city applied → SetManualCity (switch flips to manual on re-read).
    {
        let place_row = place_row.clone();
        let auto_switch = auto_switch.clone();
        let syncing = syncing.clone();
        city_entry.connect_apply(move |entry| {
            let city = entry.text().trim().to_owned();
            if city.is_empty() {
                return;
            }
            let (place_row, auto_switch, syncing) =
                (place_row.clone(), auto_switch.clone(), syncing.clone());
            spawn_on_runtime(set_manual_city(city), move |res| {
                if let Err(err) = res {
                    tracing::info!(%err, "SetManualCity failed");
                }
                refresh_place_soon(&place_row, &auto_switch, &syncing);
            });
        });
    }

    page
}

/// Read the current place over `Control` and reflect it into the widgets. On
/// failure (shell not running) the row shows an unavailable hint.
fn refresh_place(
    place_row: &adw::ActionRow,
    auto_switch: &adw::SwitchRow,
    syncing: &Rc<Cell<bool>>,
) {
    let (place_row, auto_switch, syncing) =
        (place_row.clone(), auto_switch.clone(), syncing.clone());
    spawn_on_runtime(get_place(), move |res| match res {
        Ok((label, auto)) => {
            // Suppress the switch's notify handler during the programmatic sync.
            syncing.set(true);
            place_row.set_subtitle(&label);
            auto_switch.set_active(auto);
            syncing.set(false);
        }
        Err(err) => {
            tracing::info!(%err, "GetPlace failed");
            place_row.set_subtitle("Unavailable — is trollshell running?");
        }
    });
}

/// Re-read the place now and once more after the shell's resolve lag (a
/// forward-geocode + re-resolve takes a beat), so the label catches up to a
/// just-applied change without the user refreshing.
fn refresh_place_soon(
    place_row: &adw::ActionRow,
    auto_switch: &adw::SwitchRow,
    syncing: &Rc<Cell<bool>>,
) {
    refresh_place(place_row, auto_switch, syncing);
    let (place_row, auto_switch, syncing) =
        (place_row.clone(), auto_switch.clone(), syncing.clone());
    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
        refresh_place(&place_row, &auto_switch, &syncing);
    });
}

// ── Plugins tab (#348) ────────────────────────────────────────────────────────

/// Build the real **Plugins** tab: one switch row per `trollshell-plugin-<id>`
/// systemd **user** unit, round-tripping over `Control`
/// (`ListPlugins` / `StartPlugin` / `StopPlugin` / `SetPluginEnabled`). Each
/// switch reflects whether the plugin is running; toggling it starts+enables or
/// stops+disables the unit, so the choice both applies now and persists across
/// logins. When the shell isn't running the list call fails and the group shows
/// an "unavailable" row — no panic.
///
/// Scope is the plugins' **systemd-unit state** (tier 1 + 2 of #348). The live
/// "connected / rendering" overlay from the host's in-process plugin registry
/// (`plugins.rs`, GTK-thread-local) is a deferred follow-up — it needs a
/// cross-thread bridge out of that registry.
fn build_plugins_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Plugins")
        .description(
            "Widget plugins run as trollshell-plugin-<id> systemd user units. \
             Toggle one to start and enable it, or stop and disable it.",
        )
        .build();
    page.add(&group);

    // The rows we've added, so a refresh can remove them before rebuilding.
    let rows: Rc<RefCell<Vec<gtk::Widget>>> = Rc::new(RefCell::new(Vec::new()));
    // Guard so programmatically setting a switch from a fetched state doesn't
    // loop back into a Start/Stop call (mirrors the Place tab's `syncing`).
    let syncing = Rc::new(Cell::new(false));

    refresh_plugins(&group, &rows, &syncing);
    page
}

/// Re-read the plugin list over `Control` and rebuild the group's rows. On
/// failure (shell not running) the group shows a single "unavailable" row; with
/// no plugins installed, an informational row.
fn refresh_plugins(
    group: &adw::PreferencesGroup,
    rows: &Rc<RefCell<Vec<gtk::Widget>>>,
    syncing: &Rc<Cell<bool>>,
) {
    let (group, rows, syncing) = (group.clone(), rows.clone(), syncing.clone());
    spawn_on_runtime(list_plugins(), move |res| {
        // Drop the previous rows before rebuilding.
        for row in rows.borrow_mut().drain(..) {
            group.remove(&row);
        }
        match res {
            Ok(plugins) if !plugins.is_empty() => {
                for (id, active_state, enabled) in plugins {
                    let row =
                        build_plugin_row(&group, &rows, &syncing, &id, &active_state, enabled);
                    group.add(&row);
                    rows.borrow_mut().push(row.upcast());
                }
            }
            Ok(_) => {
                let row = adw::ActionRow::builder()
                    .title("No plugins installed")
                    .subtitle("Install a trollshell-plugin-<id> user unit to manage it here.")
                    .build();
                group.add(&row);
                rows.borrow_mut().push(row.upcast());
            }
            Err(err) => {
                tracing::info!(%err, "ListPlugins failed");
                let row = adw::ActionRow::builder()
                    .title("Unavailable")
                    .subtitle("Is trollshell running?")
                    .build();
                group.add(&row);
                rows.borrow_mut().push(row.upcast());
            }
        }
    });
}

/// Build one plugin's switch row. The switch reflects whether the unit is
/// running; toggling it starts+enables (on) or stops+disables (off) the unit,
/// then re-reads the list so the row catches up to the real state.
fn build_plugin_row(
    group: &adw::PreferencesGroup,
    rows: &Rc<RefCell<Vec<gtk::Widget>>>,
    syncing: &Rc<Cell<bool>>,
    id: &str,
    active_state: &str,
    enabled: bool,
) -> adw::SwitchRow {
    let subtitle = plugin_subtitle(active_state, enabled);
    let row = adw::SwitchRow::builder()
        .title(id)
        .subtitle(&subtitle)
        .build();
    // Suppress the notify handler while we set the initial value from the fetch.
    syncing.set(true);
    row.set_active(is_running(active_state));
    syncing.set(false);

    let (group, rows, syncing, id) = (group.clone(), rows.clone(), syncing.clone(), id.to_owned());
    row.connect_active_notify(move |sw| {
        if syncing.get() {
            return;
        }
        let want_on = sw.is_active();
        let (group, rows, syncing, id) = (group.clone(), rows.clone(), syncing.clone(), id.clone());
        spawn_on_runtime(set_plugin_state(id, want_on), move |res| {
            if let Err(err) = res {
                tracing::info!(%err, "plugin start/stop failed");
            }
            refresh_plugins_soon(&group, &rows, &syncing);
        });
    });
    row
}

/// Re-read the plugin list now and again after systemd settles the transition,
/// so a just-toggled row catches up without the user refreshing.
fn refresh_plugins_soon(
    group: &adw::PreferencesGroup,
    rows: &Rc<RefCell<Vec<gtk::Widget>>>,
    syncing: &Rc<Cell<bool>>,
) {
    refresh_plugins(group, rows, syncing);
    let (group, rows, syncing) = (group.clone(), rows.clone(), syncing.clone());
    glib::timeout_add_local_once(Duration::from_millis(1200), move || {
        refresh_plugins(&group, &rows, &syncing);
    });
}

/// Whether a systemd `ActiveState` means the plugin is currently running — the
/// switch's on-state. `activating` / `reloading` count as on (it's coming up).
fn is_running(active_state: &str) -> bool {
    matches!(active_state, "active" | "activating" | "reloading")
}

/// The row subtitle: a human status from the unit's `ActiveState` plus its
/// persisted enabled/disabled state.
fn plugin_subtitle(active_state: &str, enabled: bool) -> String {
    let status = match active_state {
        "active" => "Running",
        "activating" | "reloading" => "Starting…",
        "deactivating" => "Stopping…",
        "failed" => "Failed",
        "inactive" => "Stopped",
        other => other,
    };
    let persist = if enabled { "enabled" } else { "disabled" };
    format!("{status} · {persist}")
}

/// Probe the running shell's control endpoint on the shared tokio runtime, then
/// update `banner` back on the GTK main thread with the result. Never blocks the
/// UI and never panics when the shell is absent.
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
        match rx.await {
            Ok(Ok((pong, version))) => {
                banner.set_title(&format!(
                    "Connected to trollshell {version} (Ping → {pong})"
                ));
            }
            Ok(Err(err)) => {
                tracing::info!(%err, "trollshell control endpoint unreachable");
                banner.set_title("trollshell is not running — start the shell to manage it");
            }
            Err(_) => {
                // Sender dropped without sending (task cancelled) — unreachable
                // in practice, but degrade to the disconnected message.
                banner.set_title("Could not reach trollshell");
            }
        }
        banner.set_revealed(true);
    });
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
    hytte_bus::call(CONTROL_NAME)
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
fn spawn_on_runtime<T, Fut, F>(fut: Fut, on_done: F)
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
async fn get_place() -> Result<(String, bool), hytte_bus::BusError> {
    hytte_bus::call(CONTROL_NAME)
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
async fn set_manual_city(city: String) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(CONTROL_NAME)
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
async fn set_auto_location(auto: bool) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("SetAutoLocation")
        .args((auto,))
        .timeout(Duration::from_secs(3))
        .retry(RetryPolicy::Never)
        .send::<()>()
        .await
}

// ── Plugins tab Control calls (#348) ─────────────────────────────────────────

/// `ListPlugins` → `[(id, active_state, enabled)]` for each plugin user unit.
async fn list_plugins() -> Result<Vec<(String, String, bool)>, hytte_bus::BusError> {
    hytte_bus::call(CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("ListPlugins")
        .timeout(Duration::from_secs(3))
        .retry(RetryPolicy::Never)
        .send::<Vec<(String, String, bool)>>()
        .await
}

/// Apply an on/off toggle for plugin `id`: `on` → start + enable, `off` → stop +
/// disable, so the change both takes effect now and persists across logins. Two
/// `Control` calls; the first error short-circuits.
async fn set_plugin_state(id: String, on: bool) -> Result<(), hytte_bus::BusError> {
    let start_stop = if on { "StartPlugin" } else { "StopPlugin" };
    plugin_id_call(start_stop, &id).await?;
    set_plugin_enabled(&id, on).await
}

/// One `StartPlugin`/`StopPlugin` call carrying a plugin id, returning `()`. A
/// slightly longer timeout — the shell drives a systemd job to apply it.
async fn plugin_id_call(method: &str, id: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method(method)
        .args((id.to_owned(),))
        .timeout(Duration::from_secs(5))
        .retry(RetryPolicy::Never)
        .send::<()>()
        .await
}

/// `SetPluginEnabled(id, enabled)`: persist the plugin's auto-start state.
async fn set_plugin_enabled(id: &str, enabled: bool) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("SetPluginEnabled")
        .args((id.to_owned(), enabled))
        .timeout(Duration::from_secs(5))
        .retry(RetryPolicy::Never)
        .send::<()>()
        .await
}

#[cfg(test)]
mod tests {
    use super::{is_running, plugin_subtitle};

    #[test]
    fn running_states_map_the_switch() {
        assert!(is_running("active"));
        assert!(is_running("activating"));
        assert!(is_running("reloading"));
        assert!(!is_running("inactive"));
        assert!(!is_running("failed"));
        assert!(!is_running("deactivating"));
    }

    #[test]
    fn subtitle_combines_status_and_persistence() {
        assert_eq!(plugin_subtitle("active", true), "Running · enabled");
        assert_eq!(plugin_subtitle("failed", false), "Failed · disabled");
        assert_eq!(plugin_subtitle("inactive", true), "Stopped · enabled");
        assert_eq!(plugin_subtitle("activating", false), "Starting… · disabled");
    }

    #[test]
    fn subtitle_passes_through_unknown_active_state() {
        assert_eq!(
            plugin_subtitle("maintenance", true),
            "maintenance · enabled"
        );
    }
}
