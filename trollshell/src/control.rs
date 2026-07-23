//! `mov.vibec0re.trollshell.Control` — the external control-center transport
//! (#390, the walking-skeleton foundation for the epic #381).
//!
//! The shell exposes a session-bus object implementing the
//! `mov.vibec0re.trollshell.Control` interface so the launch-on-demand
//! companion app (`trollshell-control-center`) — and its per-tab consumers
//! (#391 Place · #348 Plugins · #392 Display · #393 AI keys) — have something
//! to bind to. Beyond the foundation's `Ping`/`Version`, the first real tab
//! (#391) adds the **place** methods: `GetPlace` / `SetManualCity` /
//! `SetAutoLocation`, which round-trip the shell's runtime location override
//! (see [`hytte::services::geoclue::PlaceOverride`]). The **Plugins** tab (#348)
//! adds `ListPlugins` / `StartPlugin` / `StopPlugin` / `SetPluginEnabled`, which
//! manage the `trollshell-plugin-<id>` **user** units through the declarative
//! launcher ([`crate::plugin_launcher`], #419): declared plugins run as
//! *transient* units the host launches via `systemd-run --user`, with a
//! `StartUnit`/unit-file fallback for legacy static units. Each further tab
//! adds its own methods here, following the same shape.
//!
//! ## Why a *dedicated* bus name, not the app's primary name
//!
//! The `adw::Application` already owns the well-known name
//! `mov.vibec0re.trollshell` on the session bus (single-instance
//! `GApplication`, see `main.rs`) and auto-exports its own action group over
//! `org.gtk.Actions` at `/mov/vibec0re/trollshell` (see [`crate::commands`]).
//! `hytte_bus::own_name` therefore **cannot** own the primary name — it's
//! taken, and racing `GApplication` for it would break single-instance.
//!
//! So this endpoint owns a distinct, unambiguous well-known name
//! [`CONTROL_NAME`] (`mov.vibec0re.trollshell.Control`) and mounts the object
//! at [`CONTROL_PATH`]. The two coexist cleanly on the bus: `GApplication`'s
//! `GActions` under the primary name for niri keybinds, this typed interface
//! under `.Control` for the companion app. Both live on the same session bus,
//! owned by the same process.
//!
//! ## Pattern
//!
//! Same shape as `hytte_services::screensaver`: a [`Service`] whose `start`
//! calls [`hytte::bus::own_name`] + mounts a `#[zbus::interface]` object, and
//! whose `Handles` hold the `OwnNameSignal` so the ownership task lives for the
//! process lifetime. No raw zbus connection is constructed here (that's
//! clippy-banned) — everything goes through `hytte_bus`.

use hytte::reactive::Service;
use hytte::services::{geoclue, places};

use crate::plugin_launcher;

/// Dedicated well-known bus name for the control endpoint. Distinct from the
/// app's primary name `mov.vibec0re.trollshell` (owned by `GApplication`) — see
/// the module docs for why.
const CONTROL_NAME: &str = "mov.vibec0re.trollshell.Control";
/// Object path the [`ControlIface`] is mounted at.
const CONTROL_PATH: &str = "/mov/vibec0re/trollshell/Control";

/// Service marker registered via `App::with(control::service())`.
pub struct ControlService;

#[doc(hidden)]
pub struct ControlHandles {
    /// Keeps the name-ownership task alive for the process lifetime (dropping
    /// it would tear down the owned name). Held, not read.
    _ownership: hytte::bus::OwnNameSignal,
}

impl Service for ControlService {
    type Handles = ControlHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let ownership = hytte::bus::own_name(CONTROL_NAME)
            .at_path(CONTROL_PATH, ControlIface)
            .start();
        ControlHandles {
            _ownership: ownership,
        }
    }
}

/// Returns the control service to register with the hytte runtime.
#[must_use]
pub fn service() -> ControlService {
    ControlService
}

// ── D-Bus interface ───────────────────────────────────────────────────────────

/// Server implementation of `mov.vibec0re.trollshell.Control`. Unit struct —
/// it holds no state of its own; the place methods read/write the shell's
/// runtime location state through the [`geoclue`]/[`places`] services'
/// cross-thread accessors (the handlers run on the D-Bus task, not the GTK
/// main thread, so they must use those rather than the thread-local registry).
/// `Clone` is required by `OwnNameBuilder::at_path` (the object server re-mounts
/// a clone on reconnect).
#[derive(Clone)]
struct ControlIface;

// zbus's `#[interface]` macro requires every handler to be `async fn` even when
// the body doesn't await, and several of these read process-global state
// without touching `&self`. Allow both at the impl block to keep the noise out
// of each method.
#[allow(clippy::unused_async, clippy::unused_self)]
#[zbus::interface(name = "mov.vibec0re.trollshell.Control")]
impl ControlIface {
    /// Liveness probe — returns `"pong"`. The companion app calls this on
    /// startup to decide whether the shell is running.
    async fn ping(&self) -> String {
        "pong".to_owned()
    }

    /// The running shell's package version (`CARGO_PKG_VERSION`), so the
    /// companion app can surface it and, later, gate on feature availability.
    async fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    // ── Place / location (#391) ─────────────────────────────────────────────

    /// The resolved place — `(label, auto)`. `label` is the effective place
    /// name the weather widget shows (or the requested city / `"Resolving…"`
    /// before the first resolution); `auto` is `true` for `GeoClue2`
    /// auto-location, `false` when a manual city is in force. The companion
    /// app's Place tab populates itself from this.
    async fn get_place(&self) -> (String, bool) {
        let ov = geoclue::current_override();
        let resolved_name = places::shared_place()
            .and_then(|m| m.get_cloned())
            .map(|p| p.name);
        (place_label(&ov, resolved_name), ov.auto)
    }

    /// Switch to manual location and forward-geocode `city` (via the shell's
    /// existing Open-Meteo geocoding path), skipping `GeoClue2`. Takes effect
    /// live; a city that fails to geocode leaves the last good location in
    /// place. Re-query [`get_place`](Self::get_place) shortly after to read
    /// back the resolved label.
    async fn set_manual_city(&self, city: String) {
        geoclue::set_manual_city(city);
    }

    /// Toggle auto (`GeoClue2`) vs. manual location. `true` restores
    /// auto-location; `false` re-applies the last manual city (if any).
    async fn set_auto_location(&self, auto: bool) {
        geoclue::set_auto_location(auto);
    }

    // ── Plugins (#348, launch model #419) ───────────────────────────────────
    //
    // Tier 1 (status list) + tier 2 (on/off) of #348, backed by the declarative
    // launcher ([`plugin_launcher`] — plain async fns over `hytte-bus` +
    // `systemd-run`, so these handlers stay cross-thread-clean off the D-Bus
    // task). Declared plugins (the nix-written `plugins.json`) run as
    // *transient* units the host launches; legacy static units fall back to
    // `StartUnit`/unit files. The live "connected / rendering" overlay from the
    // host's in-process plugin registry (`plugins.rs`, GTK-thread-local) is a
    // deferred follow-up (#423) — it needs a cross-thread bridge out of the
    // registry.

    /// The known plugins — `(id, active_state, enabled)` for the union of the
    /// *declared* set (the nix-written state file, #419) and the
    /// `trollshell-plugin-<id>` **user** units systemd knows (transient runs +
    /// legacy static units). `active_state` is systemd's (`active` /
    /// `inactive` / `failed` / …; a declared-but-stopped plugin reports
    /// `inactive`); `enabled` is the declarative flag for declared plugins,
    /// unit-file enablement for legacy ones. The companion app's Plugins tab
    /// renders one switch row per entry.
    async fn list_plugins(&self) -> Vec<(String, String, bool)> {
        plugin_launcher::list()
            .await
            .into_iter()
            .map(|u| (u.id, u.active_state, u.enabled))
            .collect()
    }

    /// Start plugin `id` now: a declared plugin is launched as a transient
    /// unit via `systemd-run --user` (#419); a legacy static unit gets
    /// `StartUnit`. Does not change its enabled state (see
    /// [`set_plugin_enabled`](Self::set_plugin_enabled)). Re-query
    /// [`list_plugins`](Self::list_plugins) shortly after to read back the state.
    async fn start_plugin(&self, id: String) {
        if let Err(err) = plugin_launcher::start(&id).await {
            tracing::warn!(%err, plugin = %id, "StartPlugin failed");
        }
    }

    /// Stop plugin `id`'s user unit now (transient or static alike). Does not
    /// change its enabled state.
    async fn stop_plugin(&self, id: String) {
        if let Err(err) = plugin_launcher::stop(&id).await {
            tracing::warn!(%err, plugin = %id, "StopPlugin failed");
        }
    }

    /// Enable or disable plugin `id` for persistence across logins. For a
    /// *declared* plugin enablement is declarative — nix owns it (#419), so
    /// this is a logged no-op (persist by flipping
    /// `programs.trollshell.plugins.<id>.enable`); runtime start/stop still
    /// applies live. Legacy static units keep unit-file enable/disable.
    async fn set_plugin_enabled(&self, id: String, enabled: bool) {
        if let Err(err) = plugin_launcher::set_enabled(&id, enabled).await {
            tracing::warn!(%err, plugin = %id, enabled, "SetPluginEnabled failed");
        }
    }
}

/// The label to surface for the current place. Prefers the effective resolved
/// place name (what weather shows); before the first resolution it echoes the
/// requested manual city, else `"Resolving…"`. Pure, so the fallback logic is
/// unit-testable without a running shell.
fn place_label(ov: &geoclue::PlaceOverride, resolved_name: Option<String>) -> String {
    if let Some(name) = resolved_name {
        return name;
    }
    match (&ov.manual_city, ov.auto) {
        (Some(city), false) => city.clone(),
        _ => "Resolving…".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hytte::services::geoclue::PlaceOverride;

    #[test]
    fn label_prefers_resolved_name() {
        let ov = PlaceOverride {
            auto: false,
            manual_city: Some("Paris".to_owned()),
        };
        // A resolved name always wins over the requested city.
        assert_eq!(place_label(&ov, Some("Berlin".to_owned())), "Berlin");
    }

    #[test]
    fn label_echoes_manual_city_before_resolution() {
        let ov = PlaceOverride {
            auto: false,
            manual_city: Some("Paris".to_owned()),
        };
        assert_eq!(place_label(&ov, None), "Paris");
    }

    #[test]
    fn label_resolving_when_auto_and_unresolved() {
        assert_eq!(place_label(&PlaceOverride::default(), None), "Resolving…");
    }
}
