//! The **Plugins** tab (#348, live runtime overlay #423) — one entry per
//! `trollshell-plugin-<id>` systemd **user** unit, round-tripped over the
//! shell's `Control` endpoint (`ListPlugins` / `ListPluginStates` /
//! `StartPlugin` / `StopPlugin` / `SetPluginEnabled`).
//!
//! Split out of `main.rs` verbatim, mirroring [`crate::places_tab`]: a tab with
//! its own state struct, its own poll timer and its own `Control` calls is a
//! module, not four hundred lines in the middle of the app shell.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use hytte_bus::RetryPolicy;

use crate::{CONTROL_IFACE, CONTROL_NAME, CONTROL_PATH, spawn_on_runtime};

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
pub(crate) fn build_page() -> (adw::PreferencesPage, glib::SourceId) {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Plugins")
        // `&lt;id&gt;`, not `<id>`: a group description is parsed as Pango
        // markup, and the raw form made the *whole* description fail to render
        // (`Element "markup" was closed, but the currently open element is
        // "id"`) — so this group has shipped with no description at all since
        // #348. Drive-by fix, spotted running the app for #640's Places tab.
        .description(
            "Widget plugins run as trollshell-plugin-&lt;id&gt; systemd user units. \
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
    use super::{
        PluginRuntime, is_running, mount_or_unknown, plugin_subtitle, runtime_overlay, seen_suffix,
        violations_suffix,
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
}
