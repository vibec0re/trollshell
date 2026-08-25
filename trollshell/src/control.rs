//! `mov.vibec0re.trollshell.Control` — the external control-center transport
//! (#390, the walking-skeleton foundation for the epic #381).
//!
//! The shell exposes a session-bus object implementing the
//! `mov.vibec0re.trollshell.Control` interface so the launch-on-demand
//! companion app (`trollshell-control-center`) — and its per-tab consumers
//! (#391 Place · #348 Plugins · #392 AI keys · #393 Display) — have something
//! to bind to. Alongside the foundation's `Ping`/`Version` sits `Revision`
//! (#601), the build's git hash — the deployment identity `Version` (a frozen
//! `0.1.0`) cannot provide. The first real tab
//! (#391) adds the **place** methods: `GetPlace` / `SetManualCity` /
//! `SetAutoLocation`, which round-trip the shell's runtime location override
//! (see [`hytte::services::geoclue::PlaceOverride`]). The **Plugins** tab (#348)
//! adds `ListPlugins` / `StartPlugin` / `StopPlugin` / `SetPluginEnabled` (plus
//! `ListPluginStates`, #423 — the live connected/rendering overlay read from the
//! host's in-process plugin registry), which
//! manage the `trollshell-plugin-<id>` **user** units through the declarative
//! launcher ([`crate::plugin_launcher`], #419): declared plugins run as
//! *transient* units the host launches via `systemd-run --user`, with a
//! `StartUnit`/unit-file fallback for legacy static units. Those three **report
//! failure** (`zbus::fdo::Result<()>`, #707) rather than swallowing it into a
//! log line, so the tab can tell a start that worked from one whose unit never
//! came up. That is wire-compatible: a `Result<()>` handler introspects with the
//! same (empty) out-args as a void one and replies identically on success — only
//! the failure path changes, from an empty reply to an error reply. `ReloadPlugins`
//! (#695) is the same launcher's convergence entry point, called from the
//! home-manager module's activation script so a switch applies to the running
//! session — no tab of its own, it is machine-facing. The **AI Keys** tab
//! (#392) adds `ListAiKeys` / `SetAiKey` / `ClearAiKey`, which store the
//! LLM-backed plugins' API keys in the login keyring ([`crate::secrets`]) and
//! relaunch the plugins that use them so a rotated key takes effect. Each
//! further tab adds its own methods here, following the same shape.
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

use crate::{plugin_launcher, secrets};

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
        let ownership = hytte::bus::own_name(hytte::bus::BusKind::Session, CONTROL_NAME)
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
    ///
    /// Note this is **not** a deployment identity: it has been `0.1.0` since the
    /// first commit and never changes. Use [`revision`](Self::revision) to tell
    /// one build from another.
    async fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    /// The source revision the running shell was **built** from (#601) — a short
    /// git hash (`34e3d96`), a dirty-tree hash (`34e3d96-dirty`), `"unknown"`
    /// for a non-git source, or `"dev"` for an unstamped local `cargo build`.
    ///
    /// This is the "which commit am I?" answer [`version`](Self::version) cannot
    /// give, and it exists because two bug reports (#375, #566) turned out to be
    /// already-fixed-but-not-deployed with no way to check that from the running
    /// shell. Answerable with no UI at all:
    ///
    /// ```text
    /// busctl --user call mov.vibec0re.trollshell.Control \
    ///   /mov/vibec0re/trollshell/Control mov.vibec0re.trollshell.Control Revision
    /// ```
    ///
    /// Transport only — where (or whether) this surfaces for a human is still
    /// open on #601. See [`crate::revision`] for the resolution order.
    async fn revision(&self) -> String {
        crate::revision::revision()
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
    // `StartUnit`/unit files. Tier 3 (#423) — the live "connected / rendering"
    // overlay — rides `ListPluginStates`, which reads the host's cross-thread
    // runtime mirror ([`crate::plugins::plugin_states`]) rather than the
    // GTK-thread-local registry, so it too stays clean off the D-Bus task.

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
    ///
    /// # Errors
    /// Unknown/invalid id, a unit that is already running, or an unreachable
    /// user manager — see [`plugin_launcher::start`].
    async fn start_plugin(&self, id: String) -> zbus::fdo::Result<()> {
        plugin_launcher::start(&id)
            .await
            .map_err(|err| fail(&id, &err, "StartPlugin"))
    }

    /// Stop plugin `id`'s user unit now (transient or static alike). Does not
    /// change its enabled state.
    ///
    /// # Errors
    /// Invalid id, no such unit, or an unreachable user manager.
    async fn stop_plugin(&self, id: String) -> zbus::fdo::Result<()> {
        plugin_launcher::stop(&id)
            .await
            .map_err(|err| fail(&id, &err, "StopPlugin"))
    }

    /// Enable or disable plugin `id` for persistence across logins. For a
    /// *declared* plugin enablement is declarative — nix owns it (#419), so
    /// this is a logged no-op (persist by flipping
    /// `programs.trollshell.plugins.<id>.enable`); runtime start/stop still
    /// applies live. Legacy static units keep unit-file enable/disable.
    ///
    /// # Errors
    /// Invalid id, or an unreachable user manager (legacy static-unit path
    /// only — the declarative path can't fail).
    async fn set_plugin_enabled(&self, id: String, enabled: bool) -> zbus::fdo::Result<()> {
        plugin_launcher::set_enabled(&id, enabled)
            .await
            .map_err(|err| fail(&id, &err, "SetPluginEnabled"))
    }

    /// Re-read `plugins.json` and converge the running plugins onto it (#695):
    /// launch newly enabled/added plugins, stop newly disabled/removed ones, and
    /// restart any whose declared spec changed (`env`, `package`, `secrets`).
    ///
    /// This is the hook the home-manager module's activation script pokes after
    /// rewriting the state file, so a `home-manager switch` actually applies to
    /// the *running* session — the transient `trollshell-plugin-<id>` units are
    /// created by the shell at runtime, so activation has no unit file of its own
    /// to diff or restart. Idempotent, argument-free, and safe to call when
    /// nothing changed (it then does nothing at all).
    ///
    /// Fire-and-forget: the reconcile is spawned onto the shell's runtime and
    /// this returns immediately, so a caller (`busctl` from an activation script)
    /// never blocks on a plugin's stop→relaunch wait. Failures are logged
    /// shell-side; re-query [`list_plugins`](Self::list_plugins) to observe the
    /// result.
    async fn reload_plugins(&self) {
        tracing::info!("ReloadPlugins: reconciling plugins against plugins.json");
        hytte::reactive::runtime::handle().spawn(plugin_launcher::reconcile());
    }

    /// Live per-plugin **runtime** state from the host's in-process plugin
    /// registry (#423) — the connected/rendering overlay the Plugins tab draws on
    /// top of the systemd-unit list [`list_plugins`](Self::list_plugins) reports.
    /// Each tuple is `(id, rendering, mount, last_seen_secs, violations)` for a
    /// plugin with a **live** host connection (it dialed the socket and completed
    /// the `Register` handshake):
    /// - `rendering` — `true` once the plugin has parked at least one frame (its
    ///   card/chip is live in a mount region); `false` for a connection that
    ///   registered but hasn't rendered yet (never crashed after start).
    /// - `mount` — the region it registered for (`"SidebarTop"`, `"BarCenter"`,
    ///   …), or `""` if unknown.
    /// - `last_seen_secs` — seconds since its newest frame (or the `Register`
    ///   before its first frame).
    /// - `violations` — effects the containment guards dropped over the
    ///   connection's life (#435 rate cap / #436 capability enforcement); a
    ///   nonzero count flags a misbehaving plugin.
    ///
    /// **Presence is the "connected" signal:** a `trollshell-plugin-<id>` unit
    /// that [`list_plugins`](Self::list_plugins) reports `active` but that is
    /// *absent* here started but never connected (crashed after launch / never
    /// registered) — exactly the case the unit list alone can't tell apart from a
    /// healthy plugin. Reads the host's cross-thread runtime mirror
    /// ([`crate::plugins::plugin_states`]); empty before the host is up.
    async fn list_plugin_states(&self) -> Vec<(String, bool, String, u64, u32)> {
        crate::plugins::plugin_states()
            .into_iter()
            .map(|s| (s.id, s.rendering, s.mount, s.last_seen_secs, s.violations))
            .collect()
    }

    // ── AI keys (#392) ──────────────────────────────────────────────────────
    //
    // Store the LLM-backed plugins' API keys in the login keyring
    // ([`crate::secrets`], Secret Service / gnome-keyring) — never on disk or in
    // config. A "slot" is a provider name (e.g. "openrouter"), and a plugin opts
    // in via its `plugins.json` `secrets` allowlist; the launcher injects the
    // stored key as `<SLOT>_API_KEY` at spawn, which is exactly the override
    // `hytte_ai_providers::load_key` reads. **Values only ever cross this
    // interface inbound (Set); they are never returned and never logged.**

    /// The provider slots that currently have a stored key — e.g.
    /// `["openrouter"]`. Values are **not** returned, so the control-center's
    /// AI Keys tab can show "key set / not set" per provider without ever
    /// handling the secret. An unreadable keyring yields an empty list.
    async fn list_ai_keys(&self) -> Vec<String> {
        secrets::list().await.unwrap_or_else(|err| {
            tracing::warn!(%err, "ListAiKeys: reading the keyring failed");
            Vec::new()
        })
    }

    /// Store `value` as the API key for `slot` (e.g. `"openrouter"`) in the
    /// login keyring, replacing any existing key, then relaunch the running
    /// plugins that declare the slot so the new key takes effect (rotation =
    /// relaunch). The relaunch is fired off the D-Bus task so the call returns
    /// promptly; the value is never written to disk/config and never logged.
    async fn set_ai_key(&self, slot: String, value: String) {
        if !secrets::is_valid_slot(&slot) {
            tracing::warn!(%slot, "SetAiKey: invalid slot name; ignored");
            return;
        }
        if let Err(err) = secrets::set(&slot, &value).await {
            tracing::warn!(%slot, %err, "SetAiKey: storing the key failed");
            return;
        }
        tracing::info!(%slot, "stored AI key; relaunching affected plugins");
        hytte::reactive::runtime::handle()
            .spawn(async move { plugin_launcher::relaunch_for_secret(&slot).await });
    }

    /// Delete the stored API key for `slot`, then relaunch the running plugins
    /// that declare it (so they drop back to keyless/fallback). Idempotent —
    /// clearing an unset slot is a no-op.
    async fn clear_ai_key(&self, slot: String) {
        if !secrets::is_valid_slot(&slot) {
            tracing::warn!(%slot, "ClearAiKey: invalid slot name; ignored");
            return;
        }
        if let Err(err) = secrets::clear(&slot).await {
            tracing::warn!(%slot, %err, "ClearAiKey: clearing the key failed");
            return;
        }
        tracing::info!(%slot, "cleared AI key; relaunching affected plugins");
        hytte::reactive::runtime::handle()
            .spawn(async move { plugin_launcher::relaunch_for_secret(&slot).await });
    }
}

/// Log a failed plugin control call and map it to the D-Bus error the caller
/// gets back (#707).
///
/// Before this the three plugin methods were **void**: a failed start was logged
/// shell-side and the caller got an ordinary empty reply, so the control-center's
/// Plugins tab (#348) showed a start as having worked when the unit never came
/// up, and a scripted caller had no signal at all. The log line stays — the
/// journal is where the *cause* lives — and the error now travels too.
///
/// `fdo::Error::Failed` rather than a bespoke `#[zbus::DBusError]` enum: the
/// launcher's failures are already flattened into one `anyhow` chain (bad id,
/// unit already up, no user manager), so there is nothing for a caller to
/// usefully switch on that the message doesn't say better. `{err:#}` renders the
/// whole `anyhow` context chain, not just the outermost frame.
fn fail(id: &str, err: &anyhow::Error, method: &str) -> zbus::fdo::Error {
    tracing::warn!(%err, plugin = %id, "{method} failed");
    zbus::fdo::Error::Failed(format!("{method} for plugin {id} failed: {err:#}"))
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
