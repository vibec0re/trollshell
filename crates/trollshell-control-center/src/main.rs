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
//! **user** unit with a switch that starts/enables or stops/disables it. The
//! **AI Keys** tab (#392) stores the LLM-backed plugins' API keys in the login
//! keyring (gnome-keyring/libsecret) — never on disk — and rotates them. All
//! round-trip over `Control`. There is deliberately **no Display tab**: #393
//! re-scoped display management away from a bespoke control-center page and
//! onto `org.gnome.Mutter.DisplayConfig`, a shim over niri-ipc
//! (`crates/hytte-services/src/display_config.rs`) that lets
//! **gnome-control-center's own Display panel** drive niri outputs directly —
//! compatmaxx: reuse the existing GNOME client, provide the backend. When the
//! shell isn't running the app degrades gracefully rather than panicking.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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
    let (plugins_page, plugins_poll) = build_plugins_page();
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

    // The Plugins tab's 2 s poll timer is scoped to this window: drop it on
    // close so a dismissed window stops polling `Control`, and a re-launch while
    // another window is still resident can't leave the first window's timer
    // double-polling behind it (#542). Wrapped in a cell + `.take()` so the
    // one-shot removal is clean under the `Fn` close handler.
    let plugins_poll = RefCell::new(Some(plugins_poll));
    window.connect_close_request(move |_| {
        if let Some(source) = plugins_poll.borrow_mut().take() {
            source.remove();
        }
        glib::Propagation::Proceed
    });

    check_shell_connection(&banner);
    window.present();
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

// ── Plugins tab (#348) · live connected/rendering overlay (#423) ─────────────

/// The poll cadence for the Plugins tab's live runtime overlay (#423). The tab
/// re-reads `ListPlugins` + `ListPluginStates` on this interval and refreshes
/// each row's connected/rendering badge **in place** (a changed plugin set
/// triggers a rebuild instead), so the badges track the host without the user
/// reopening the tab.
const PLUGIN_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// One connected plugin's host-side runtime state, as `ListPluginStates` (#423)
/// reports it. Absent for a plugin id the host doesn't list — i.e. one with no
/// live host connection (started but never registered, or stopped).
struct PluginRuntime {
    /// Whether the plugin has parked at least one render frame (its card/chip is
    /// live), vs. connected-but-not-yet-drawing.
    rendering: bool,
    /// The mount region it registered for (wire name), or `""` if unknown.
    mount: String,
    /// Seconds since the host last saw a frame (or its `Register`).
    last_seen_secs: u64,
    /// Effects the host's containment guards dropped (#435 rate cap / #436
    /// capability enforcement) over the connection's life.
    violations: u32,
}

/// One built plugin row's live widgets, kept keyed by id so the periodic refresh
/// can update the runtime overlay **in place** (no rebuild → no flicker) while
/// the plugin set is unchanged.
#[derive(Clone)]
struct PluginRow {
    row: adw::SwitchRow,
    badge: gtk::Image,
}

/// What the Plugins group is currently showing, so a poll only rebuilds on a
/// real transition (list ⇄ empty ⇄ unavailable) and otherwise updates in place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PluginsView {
    Uninit,
    List,
    Empty,
    Unavailable,
}

/// Shared, mutable state threaded through the Plugins tab's refresh path so the
/// build, the toggle handlers, and the poll timer all drive the same group.
#[derive(Clone)]
struct PluginsState {
    group: adw::PreferencesGroup,
    /// Every child currently added to the group (plugin rows or a single
    /// placeholder), for teardown before a rebuild.
    rows: Rc<RefCell<Vec<gtk::Widget>>>,
    /// The plugin rows keyed by id, for the in-place overlay update.
    by_id: Rc<RefCell<HashMap<String, PluginRow>>>,
    /// What's currently shown, gating rebuild vs. in-place update.
    view: Rc<Cell<PluginsView>>,
    /// Guard so programmatically setting a switch from a fetched state doesn't
    /// loop back into a Start/Stop call (mirrors the Place tab's `syncing`).
    syncing: Rc<Cell<bool>>,
}

/// Build the real **Plugins** tab: one switch row per `trollshell-plugin-<id>`
/// systemd **user** unit, round-tripping over `Control`
/// (`ListPlugins` / `StartPlugin` / `StopPlugin` / `SetPluginEnabled`). Each
/// switch reflects whether the plugin's unit is running; toggling it
/// starts+enables or stops+disables the unit, so the choice both applies now and
/// persists across logins. When the shell isn't running the list call fails and
/// the group shows an "unavailable" row — no panic.
///
/// On top of that unit state (tier 1 + 2 of #348), each row carries a **live
/// runtime badge** (#423) sourced from `ListPluginStates` — the host's
/// in-process view of which plugins actually dialed the socket and are
/// rendering. A prefix icon + subtitle distinguish connected-and-rendering,
/// connected-but-idle, and the diagnostic case a unit list alone can't:
/// active-but-never-connected (crashed after start). A poll timer keeps the
/// badges live.
fn build_plugins_page() -> (adw::PreferencesPage, glib::SourceId) {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Plugins")
        .description(
            "Widget plugins run as trollshell-plugin-<id> systemd user units. \
             Toggle one to start and enable it, or stop and disable it. The badge \
             shows the host's live view — connected and rendering, connected but \
             not yet drawing, or a unit that's active yet never connected.",
        )
        .build();
    page.add(&group);

    let state = PluginsState {
        group,
        rows: Rc::new(RefCell::new(Vec::new())),
        by_id: Rc::new(RefCell::new(HashMap::new())),
        view: Rc::new(Cell::new(PluginsView::Uninit)),
        syncing: Rc::new(Cell::new(false)),
    };

    refresh_plugins(&state);

    // Live overlay (#423): poll on an interval so the badges track reality
    // without the user reopening the tab. `refresh_plugins` updates in place
    // while the plugin set is unchanged, so a steady set never flickers. The
    // caller ties the returned `SourceId` to the window so the timer dies with
    // it (#542) rather than polling `Control` forever after the window closes.
    let poll = {
        let state = state.clone();
        glib::timeout_add_local(PLUGIN_POLL_INTERVAL, move || {
            refresh_plugins(&state);
            glib::ControlFlow::Continue
        })
    };
    (page, poll)
}

/// Re-read the unit list (`ListPlugins`) plus the runtime overlay
/// (`ListPluginStates`) over `Control` and reflect them into the group —
/// updating badges in place while the plugin set is unchanged, rebuilding the
/// rows on any structural change, and showing a single placeholder when there
/// are no plugins (informational) or the shell is unreachable ("unavailable").
fn refresh_plugins(state: &PluginsState) {
    let state = state.clone();
    spawn_on_runtime(list_plugins_and_states(), move |res| match res {
        Ok((units, states)) if !units.is_empty() => {
            let rt: HashMap<String, PluginRuntime> = states
                .into_iter()
                .map(|(id, rendering, mount, last_seen_secs, violations)| {
                    (
                        id,
                        PluginRuntime {
                            rendering,
                            mount,
                            last_seen_secs,
                            violations,
                        },
                    )
                })
                .collect();
            apply_plugins(&state, &units, &rt);
        }
        Ok(_) => set_placeholder(
            &state,
            PluginsView::Empty,
            "No plugins installed",
            "Install a trollshell-plugin-<id> user unit to manage it here.",
        ),
        Err(err) => {
            tracing::info!(%err, "ListPlugins failed");
            set_placeholder(
                &state,
                PluginsView::Unavailable,
                "Unavailable",
                "Is trollshell running?",
            );
        }
    });
}

/// Apply a non-empty unit list + runtime overlay: update the existing rows in
/// place when the plugin set already matches (no flicker), else rebuild them.
fn apply_plugins(
    state: &PluginsState,
    units: &[(String, String, bool)],
    rt: &HashMap<String, PluginRuntime>,
) {
    let same_set = state.view.get() == PluginsView::List && {
        let map = state.by_id.borrow();
        map.len() == units.len() && units.iter().all(|(id, ..)| map.contains_key(id))
    };
    if same_set {
        for (id, active_state, enabled) in units {
            // Clone the row handle out and let the borrow end at this `let`.
            // `update_plugin_row` finishes with `set_active`, and GObject
            // property notification is synchronous — the row's own
            // `connect_active_notify` handler runs inside that call. Holding
            // `by_id` borrowed across it means any path from that handler back
            // into `by_id` (`clear_rows`, a nested `apply_plugins`) panics with
            // a `BorrowMutError`, from inside a glib callback, which aborts the
            // process rather than failing gracefully (#643).
            let prow = state.by_id.borrow().get(id).cloned();
            if let Some(prow) = prow {
                update_plugin_row(&prow, &state.syncing, active_state, *enabled, rt.get(id));
            }
        }
        return;
    }
    // Structural change (or first load): rebuild the rows.
    clear_rows(state);
    for (id, active_state, enabled) in units {
        let prow = build_plugin_row(state, id, active_state, *enabled, rt.get(id));
        state.group.add(&prow.row);
        state.rows.borrow_mut().push(prow.row.clone().upcast());
        // The semicolon rule: `insert` returns the displaced entry, and as a
        // bare statement that `Option<PluginRow>` is a temporary dropped
        // *before* the `RefMut` (temporaries drop in reverse creation order),
        // i.e. it would drop two GTK widgets while `by_id` is borrowed. Binding
        // it moves the drop past the borrow. (The same call is safe as a
        // closure tail expression, where the value is moved out to the caller —
        // it is the trailing semicolon that creates the hazard.) `clear_rows`
        // ran just above, so the displaced value is `None` today.
        let displaced = state.by_id.borrow_mut().insert(id.clone(), prow);
        drop(displaced);
    }
    state.view.set(PluginsView::List);
}

/// Remove every currently-added child (plugin rows or a placeholder) from the
/// group and forget the keyed rows, before a rebuild.
fn clear_rows(state: &PluginsState) {
    // `take()`, not `borrow_mut().drain(..)`: the chained `RefMut` would stay
    // live across every `group.remove()`, which can emit synchronously into a
    // handler that re-enters these cells — a `BorrowMutError` inside a glib
    // callback aborts the process (#643).
    for row in state.rows.take() {
        state.group.remove(&row);
    }
    // Same reason for `by_id`: `clear()` drops each `PluginRow`'s two GTK
    // widgets *inside* the borrow, whereas `take()`'s borrow is over before the
    // returned map (and so its widgets) drops.
    drop(state.by_id.take());
}

/// Show a single informational/placeholder row (no plugins, or shell
/// unavailable), rebuilding only on a *transition* into `view` so a steady poll
/// doesn't flicker it.
fn set_placeholder(state: &PluginsState, view: PluginsView, title: &str, subtitle: &str) {
    if state.view.get() == view {
        return;
    }
    clear_rows(state);
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    state.group.add(&row);
    state.rows.borrow_mut().push(row.upcast());
    state.view.set(view);
}

/// Build one plugin's switch row with a runtime badge (#423) and its initial
/// state. Toggling the switch starts+enables (on) or stops+disables (off) the
/// unit, then re-reads so the row catches up to the real state.
fn build_plugin_row(
    state: &PluginsState,
    id: &str,
    active_state: &str,
    enabled: bool,
    rt: Option<&PluginRuntime>,
) -> PluginRow {
    let row = adw::SwitchRow::builder().title(id).build();
    let badge = gtk::Image::new();
    badge.set_valign(gtk::Align::Center);
    row.add_prefix(&badge);
    let prow = PluginRow {
        row: row.clone(),
        badge,
    };
    update_plugin_row(&prow, &state.syncing, active_state, enabled, rt);

    let (state, id) = (state.clone(), id.to_owned());
    row.connect_active_notify(move |sw| {
        if state.syncing.get() {
            return;
        }
        let want_on = sw.is_active();
        let (state, id) = (state.clone(), id.clone());
        spawn_on_runtime(set_plugin_state(id, want_on), move |res| {
            if let Err(err) = res {
                tracing::info!(%err, "plugin start/stop failed");
            }
            refresh_plugins_soon(&state);
        });
    });
    prow
}

/// Reflect a unit's state + runtime overlay into an existing row: the subtitle
/// (unit status + runtime line), the prefix badge, and the switch (set under
/// `syncing`, so the programmatic set doesn't fire a Start/Stop).
fn update_plugin_row(
    prow: &PluginRow,
    syncing: &Rc<Cell<bool>>,
    active_state: &str,
    enabled: bool,
    rt: Option<&PluginRuntime>,
) {
    let (icon, css, status) = runtime_overlay(active_state, rt);
    let mut subtitle = plugin_subtitle(active_state, enabled);
    if !status.is_empty() {
        subtitle.push_str(" · ");
        subtitle.push_str(&status);
    }
    prow.row.set_subtitle(&subtitle);
    apply_badge(&prow.badge, icon, css, &status);

    syncing.set(true);
    prow.row.set_active(is_running(active_state));
    syncing.set(false);
}

/// Set (or hide) a row's prefix runtime badge: a recolored symbolic icon whose
/// tooltip echoes the runtime status line. An empty `icon` hides it (an inactive
/// unit with no connection has nothing to overlay).
fn apply_badge(badge: &gtk::Image, icon: &str, css: &str, tooltip: &str) {
    for class in ["success", "accent", "warning"] {
        badge.remove_css_class(class);
    }
    if icon.is_empty() {
        badge.set_visible(false);
        return;
    }
    badge.set_icon_name(Some(icon));
    if !css.is_empty() {
        badge.add_css_class(css);
    }
    badge.set_tooltip_text(Some(tooltip));
    badge.set_visible(true);
}

/// Re-read now and again after systemd settles the transition, so a just-toggled
/// row catches up without waiting for the next poll tick.
fn refresh_plugins_soon(state: &PluginsState) {
    refresh_plugins(state);
    let state = state.clone();
    glib::timeout_add_local_once(Duration::from_millis(1200), move || {
        refresh_plugins(&state);
    });
}

/// Whether a systemd `ActiveState` means the plugin is currently running — the
/// switch's on-state. `activating` / `reloading` count as on (it's coming up).
fn is_running(active_state: &str) -> bool {
    matches!(active_state, "active" | "activating" | "reloading")
}

/// The base row subtitle: a human status from the unit's `ActiveState` plus its
/// persisted enabled/disabled state. The runtime overlay (#423) appends to this.
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

/// The connected/rendering overlay for a plugin row (#423), merging the systemd
/// `active_state` with the host's live runtime state (`None` = no live host
/// connection). Returns `(icon, css_class, status)`: `icon` is a symbolic name
/// for the prefix badge (`""` hides it), `css_class` recolors it
/// (`"success"`/`"accent"`/`"warning"`/`""`), and `status` is the human-readable
/// runtime line appended to the subtitle. Pure → unit-tested.
fn runtime_overlay(
    active_state: &str,
    rt: Option<&PluginRuntime>,
) -> (&'static str, &'static str, String) {
    match rt {
        // Connected and drawing: the healthy case.
        Some(rt) if rt.rendering => (
            "emblem-ok-symbolic",
            "success",
            format!(
                "Connected · rendering in {}{}{}",
                mount_or_unknown(&rt.mount),
                violations_suffix(rt.violations),
                seen_suffix(rt.last_seen_secs),
            ),
        ),
        // Connected but hasn't rendered yet (coming up, or a silent plugin).
        Some(rt) => (
            "content-loading-symbolic",
            "accent",
            format!(
                "Connected · not yet rendering{}{}",
                violations_suffix(rt.violations),
                seen_suffix(rt.last_seen_secs),
            ),
        ),
        // Unit is active but no host connection — crashed after start / never
        // registered. This is the diagnostic case the unit list can't show.
        None if is_running(active_state) => (
            "dialog-warning-symbolic",
            "warning",
            "Active but not connected".to_owned(),
        ),
        // Inactive and unconnected: nothing to overlay.
        None => ("", "", String::new()),
    }
}

/// The mount name for display, or a stand-in when the host didn't report one.
fn mount_or_unknown(mount: &str) -> &str {
    if mount.is_empty() {
        "an unknown region"
    } else {
        mount
    }
}

/// A " · N dropped" suffix when the plugin has tripped the containment guards
/// (#435/#436), else empty.
fn violations_suffix(violations: u32) -> String {
    if violations == 0 {
        String::new()
    } else {
        format!(" · {violations} dropped")
    }
}

/// A " · seen …" suffix for a nontrivial gap since the host last saw a frame,
/// humanized to s/m/h; empty for a fresh (<5s) plugin so an actively-drawing
/// card's subtitle stays tidy.
fn seen_suffix(secs: u64) -> String {
    match secs {
        0..=4 => String::new(),
        5..=59 => format!(" · seen {secs}s ago"),
        60..=3599 => format!(" · seen {}m ago", secs / 60),
        _ => format!(" · seen {}h ago", secs / 3600),
    }
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
async fn set_manual_city(city: String) -> Result<(), hytte_bus::BusError> {
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
async fn set_auto_location(auto: bool) -> Result<(), hytte_bus::BusError> {
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

// ── Plugins tab Control calls (#348) ─────────────────────────────────────────

/// `ListPlugins` → `[(id, active_state, enabled)]` for each plugin user unit.
async fn list_plugins() -> Result<Vec<(String, String, bool)>, hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("ListPlugins")
        .timeout(Duration::from_secs(3))
        .retry(RetryPolicy::Never)
        .send::<Vec<(String, String, bool)>>()
        .await
}

/// `ListPluginStates` → `[(id, rendering, mount, last_seen_secs, violations)]`
/// for each plugin with a live host connection (#423). The runtime overlay the
/// Plugins tab draws on top of the unit list.
async fn list_plugin_states() -> Result<Vec<(String, bool, String, u64, u32)>, hytte_bus::BusError>
{
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("ListPluginStates")
        .timeout(Duration::from_secs(3))
        .retry(RetryPolicy::Never)
        .send::<Vec<(String, bool, String, u64, u32)>>()
        .await
}

/// Fetch the unit list and the runtime overlay for the Plugins tab in one shot
/// (#423). The overlay is **best-effort**: a `ListPluginStates` error (e.g. an
/// older shell that predates it) degrades to no overlay rather than blanking the
/// unit list, so the tab still works against a shell without the method.
async fn list_plugins_and_states() -> Result<
    (
        Vec<(String, String, bool)>,
        Vec<(String, bool, String, u64, u32)>,
    ),
    hytte_bus::BusError,
> {
    let units = list_plugins().await?;
    let states = list_plugin_states().await.unwrap_or_default();
    Ok((units, states))
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
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
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
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
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
    use tracing_subscriber::filter::LevelFilter;

    use super::{
        DEFAULT_LOG_LEVEL, PluginRuntime, build_env_filter, is_running, mount_or_unknown,
        plugin_subtitle, runtime_overlay, seen_suffix, violations_suffix,
    };

    /// A connected plugin's runtime state, for the overlay tests.
    fn rt(rendering: bool, mount: &str, last_seen_secs: u64, violations: u32) -> PluginRuntime {
        PluginRuntime {
            rendering,
            mount: mount.to_owned(),
            last_seen_secs,
            violations,
        }
    }

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

    // ── Runtime overlay (#423) ───────────────────────────────────────────────

    #[test]
    fn overlay_reports_connected_and_rendering() {
        let (icon, css, status) = runtime_overlay("active", Some(&rt(true, "SidebarTop", 1, 0)));
        assert_eq!(icon, "emblem-ok-symbolic");
        assert_eq!(css, "success");
        assert_eq!(status, "Connected · rendering in SidebarTop");
    }

    #[test]
    fn overlay_reports_connected_not_yet_rendering() {
        let (icon, css, status) = runtime_overlay("activating", Some(&rt(false, "", 0, 0)));
        assert_eq!(icon, "content-loading-symbolic");
        assert_eq!(css, "accent");
        assert_eq!(status, "Connected · not yet rendering");
    }

    #[test]
    fn overlay_flags_active_but_not_connected() {
        // The diagnostic case: the unit is up, but nothing dialed the socket.
        let (icon, css, status) = runtime_overlay("active", None);
        assert_eq!(icon, "dialog-warning-symbolic");
        assert_eq!(css, "warning");
        assert_eq!(status, "Active but not connected");
    }

    #[test]
    fn overlay_blank_for_inactive_and_unconnected() {
        let (icon, css, status) = runtime_overlay("inactive", None);
        assert_eq!(icon, "");
        assert_eq!(css, "");
        assert!(status.is_empty());
    }

    #[test]
    fn overlay_surfaces_violations_and_age() {
        let (_, _, status) = runtime_overlay("active", Some(&rt(true, "BarCenter", 90, 3)));
        assert_eq!(
            status,
            "Connected · rendering in BarCenter · 3 dropped · seen 1m ago"
        );
    }

    #[test]
    fn mount_falls_back_when_unknown() {
        assert_eq!(mount_or_unknown("SidebarTop"), "SidebarTop");
        assert_eq!(mount_or_unknown(""), "an unknown region");
    }

    #[test]
    fn violations_suffix_only_when_nonzero() {
        assert_eq!(violations_suffix(0), "");
        assert_eq!(violations_suffix(1), " · 1 dropped");
        assert_eq!(violations_suffix(7), " · 7 dropped");
    }

    #[test]
    fn seen_suffix_humanizes_the_gap() {
        assert_eq!(seen_suffix(2), "");
        assert_eq!(seen_suffix(30), " · seen 30s ago");
        assert_eq!(seen_suffix(600), " · seen 10m ago");
        assert_eq!(seen_suffix(7200), " · seen 2h ago");
    }

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
}
