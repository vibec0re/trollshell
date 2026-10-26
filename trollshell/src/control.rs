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
//! host's in-process plugin registry, and `ListPluginVersions`, #887 — each
//! connected plugin's self-declared release version), which
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
//! relaunch the plugins that use them so a rotated key takes effect. Those
//! three report a keyring failure the same way, as an error reply, so a key the
//! keyring refused is never shown as stored. Each further tab adds its own
//! methods here, following the same shape.
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
    ///
    /// # Errors
    /// A `city` [`check_city`] refuses — blank, or longer than
    /// [`MAX_CITY_BYTES`] — as a `Failed` reply, with the location left as it
    /// was. The control-center's Places tab already shows that reply as a
    /// toast (`places_tab::override_failure`).
    async fn set_manual_city(&self, city: String) -> zbus::fdo::Result<()> {
        check_city(&city)?;
        geoclue::set_manual_city(city);
        Ok(())
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
    /// `inactive`); `enabled` is the effective flag for declared plugins —
    /// nix's, or the Plugins tab's persisted switch where nix leaves it free
    /// (#1400) — and unit-file enablement for legacy ones. The companion
    /// app's Plugins tab renders one switch row per entry.
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
    /// A declared plugin whose unit is still stopping (`deactivating`, e.g. a
    /// `StopPlugin` a moment ago) is launched once the unit is gone, so the
    /// answer can take up to the launcher's 12 s stop wait (#1417 follow-up).
    ///
    /// # Errors
    /// Unknown/invalid id, a unit that is already running, a unit still
    /// stopping when that wait ran out, or an unreachable user manager — see
    /// [`plugin_launcher::start`].
    async fn start_plugin(&self, id: String) -> zbus::fdo::Result<()> {
        plugin_launcher::start(&id)
            .await
            .map_err(|err| fail(&id, &err, "StartPlugin"))
    }

    /// Stop plugin `id`'s user unit now (transient or static alike). Does not
    /// change its enabled state.
    ///
    /// Queues behind any start, relaunch or reconcile the launcher already
    /// has under way or queued (#1417 item 1b), so a stop sent right after a
    /// `StartPlugin` stops what that start launched instead of landing before
    /// the unit exists. It therefore waits for everything queued on the
    /// launcher's lock ahead of it: a reconcile's restarts, keyring reads,
    /// queued Saves and switches. #1417 item 1 leaves that unbounded, so the
    /// caller's timeout decides what it is told. The control-center's switch
    /// gives its whole round trip, `SetPluginEnabled` and then this, one
    /// deadline (#1421), so a long queue reads there as unanswered, and the
    /// stop still runs when the lock frees. See [`plugin_launcher::stop`].
    ///
    /// # Errors
    /// Invalid id, no such unit, or an unreachable user manager.
    async fn stop_plugin(&self, id: String) -> zbus::fdo::Result<()> {
        plugin_launcher::stop(&id)
            .await
            .map_err(|err| fail(&id, &err, "StopPlugin"))
    }

    /// Enable or disable plugin `id` for persistence across logins. For a
    /// *declared* plugin nix leaves free, the choice is kept in the shell's
    /// own `$XDG_STATE_HOME/trollshell/plugins.toml` and applies from the
    /// next reconcile on (#1400); for one whose
    /// `programs.trollshell.plugins.<id>.enable` nix pins, this refuses and
    /// says so. The start/stop itself is the caller's `StartPlugin` /
    /// `StopPlugin`, which the control-center's switch sends only after this
    /// succeeded. Legacy static units keep unit-file enable/disable.
    ///
    /// # Errors
    /// A pinned plugin, an unreadable `plugins.json`, or a failed state
    /// write; on the legacy static-unit path, an invalid id or an
    /// unreachable user manager — see [`plugin_launcher::set_enabled`].
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
    /// nothing changed (it then does nothing at all). Since #1399 the shell
    /// also notices a changed `plugins.json` by itself within a few seconds,
    /// so this call is what makes a switch apply immediately, not what makes
    /// it apply at all.
    ///
    /// Fire-and-forget: the reconcile is spawned onto the shell's runtime and
    /// this returns immediately, so a caller (`busctl` from an activation script)
    /// never blocks on a plugin's stop→relaunch wait. Failures are logged
    /// shell-side; re-query [`list_plugins`](Self::list_plugins) to observe the
    /// result. A pass that could not list the plugin units launches blind and
    /// leaves the rest to the `plugins.json` watch, which reconciles again on
    /// its next tick (#1404).
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

    /// The release version each **connected** plugin declared in its manifest
    /// (#887), as `id → version` (`a{ss}`).
    ///
    /// Additive beside [`list_plugin_states`](Self::list_plugin_states) rather
    /// than a sixth tuple element there, so neither that method's nor
    /// [`list_plugins`](Self::list_plugins)' signature changes under an older
    /// control-center. A plugin that is not connected, or connected but declared
    /// no version (built before #887), is simply absent — the Plugins tab shows
    /// "—" for both. Values are display text the host has already sanitised and
    /// capped (`plugins::version`); nothing here parses them.
    async fn list_plugin_versions(&self) -> std::collections::HashMap<String, String> {
        crate::plugins::plugin_states()
            .into_iter()
            .filter_map(|s| Some((s.id, s.version?)))
            .collect()
    }

    /// The settings each plugin declared in its manifest (#1410), as
    /// `id → JSON` (`a{ss}`): the value is a JSON array of
    /// `hytte_plugin_proto::manifest::Setting`, exactly as serde spells that
    /// type (`{"env":…,"label":…,"doc":…,"kind":{"Path":{"directory":false}},
    /// "default":…}`, a unit kind as the bare string `"Text"`).
    ///
    /// JSON rather than a D-Bus struct because a setting's `kind` is a tagged
    /// union with per-variant fields, which a D-Bus signature can only carry
    /// as a variant the control-center would then unpick by hand; serde's own
    /// spelling lets it decode straight into the proto's types, so the two ends
    /// share one definition. Additive beside
    /// [`list_plugin_versions`](Self::list_plugin_versions), so no existing
    /// signature changes.
    ///
    /// Not only the connected plugins are here: each id's last declaration is
    /// remembered under `$XDG_STATE_HOME`, so a plugin that is switched off,
    /// or cannot start without its setting, still has a form. An id is
    /// forgotten once `plugins.json` stops declaring it and it is not
    /// connected (#1415 review L3), and only declared ids are remembered
    /// across sessions — so a hand-installed static unit has a form only while
    /// it is connected. A plugin that declares none is absent. Every list is
    /// already sanitised (`plugins::settings`).
    async fn list_plugin_settings(&self) -> std::collections::HashMap<String, String> {
        let declared = plugin_launcher::declared_ids().await;
        let snapshot = tokio::task::spawn_blocking(move || {
            crate::plugins::settings::snapshot(declared.as_ref())
        });
        let schemas = match snapshot.await {
            Ok(schemas) => schemas,
            Err(err) => {
                tracing::warn!(%err, "ListPluginSettings: reading the settings store failed");
                return std::collections::HashMap::new();
            }
        };
        schemas
            .into_iter()
            .filter_map(|(id, list)| match serde_json::to_string(&list) {
                Ok(json) => Some((id, json)),
                Err(err) => {
                    tracing::warn!(plugin = %id, %err, "ListPluginSettings: could not encode a plugin's settings");
                    None
                }
            })
            .collect()
    }

    /// Restart plugin `id` so it reads `plugin-settings.toml` again (#1410) —
    /// what the Plugins tab's Settings group calls once after a Save.
    ///
    /// One call, where the tab used to send `StopPlugin` then `StartPlugin`
    /// itself (#1415 review H2): the launcher runs the stop, the wait for the
    /// unit to be really down (through `deactivating`) and the relaunch under
    /// its own convergence lock, so no reconcile or second Save lands in
    /// between. Answers what it did, as a word
    /// ([`plugin_launcher::SettingsRestart::wire_name`]):
    ///
    /// - `"relaunched"` — it was running, and now runs with the saved values;
    /// - `"not-running"` — declared but stopped: nothing was started, and the
    ///   values apply at its next start;
    /// - `"not-declared"` — a hand-installed static unit, which the launcher
    ///   does not launch and which never reads the file: left alone;
    /// - `"switched-off"` — declared and running, but switched off (#1417
    ///   item 1b): nothing was relaunched, a unit the launcher started was
    ///   stopped, and the values apply when the plugin is next started. A
    ///   switch flipped off while this call waited is what reaches it.
    /// - `"still-stopping"` — declared, running and switched on, but its unit,
    ///   which the launcher started, did not stop within 12 s (#1417 item 1):
    ///   nothing was relaunched and nothing started the old unit again, so the
    ///   plugin is down once its stuck process exits, and the values apply
    ///   when it is next started.
    ///
    /// The tab sends this after **every** Save, whatever its own last poll
    /// said: the answer is decided here, after any restart already under way,
    /// so a Save made while an earlier one is still restarting the plugin is
    /// applied too (#1415 second review).
    ///
    /// # Errors
    /// A `plugins.json` that exists but cannot be read (never answered as
    /// `not-declared`), or a failed unit listing, stop or relaunch.
    async fn restart_plugin(&self, id: String) -> zbus::fdo::Result<String> {
        plugin_launcher::restart_for_settings(&id)
            .await
            .map(|outcome| outcome.wire_name().to_owned())
            .map_err(|err| fail(&id, &err, "RestartPlugin"))
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
    /// handling the secret.
    ///
    /// # Errors
    /// A keyring that cannot be opened, unlocked or searched. That used to
    /// be an empty list, which says no key is stored: the tab then showed
    /// every provider as "No key set" while their keys sat in a keyring the
    /// shell could not read. It shows "Unavailable" for an error.
    async fn list_ai_keys(&self) -> zbus::fdo::Result<Vec<String>> {
        list_keys(&LoginKeyring).await
    }

    /// Store `value` as the API key for `slot` (e.g. `"openrouter"`) in the
    /// login keyring, replacing any existing key, then relaunch the running
    /// plugins that declare the slot so the new key takes effect (rotation =
    /// relaunch). The relaunch is fired off the D-Bus task so the call returns
    /// promptly; the value is never written to disk/config and never logged.
    ///
    /// # Errors
    /// A slot name [`secrets::is_valid_slot`] refuses, a `value`
    /// [`check_key_value`] refuses, or a keyring that cannot be opened,
    /// unlocked or written — then nothing is stored and nothing is relaunched.
    /// The first and last used to be an ordinary empty reply, so the
    /// control-center showed a key as stored that the keyring had refused (the
    /// shape #707 fixed for the plugin methods), and the value had no check at
    /// all. Neither the reply nor the log line carries the value.
    async fn set_ai_key(&self, slot: String, value: String) -> zbus::fdo::Result<()> {
        set_key(&LoginKeyring, slot, &value).await
    }

    /// Delete the stored API key for `slot`, then relaunch the running plugins
    /// that declare it (so they drop back to keyless/fallback). Idempotent —
    /// clearing an unset slot is a no-op.
    ///
    /// # Errors
    /// A slot name [`secrets::is_valid_slot`] refuses, or a keyring that
    /// cannot be opened, unlocked or written — then nothing is relaunched,
    /// and the key is still stored.
    async fn clear_ai_key(&self, slot: String) -> zbus::fdo::Result<()> {
        clear_key(&LoginKeyring, slot).await
    }
}

/// What the AI-key methods need from the login keyring and the launcher: the
/// seam a test replaces so a keyring refusal can be driven without a Secret
/// Service (#1558). Production is [`LoginKeyring`], which makes exactly the
/// calls the handlers made before the seam existed.
trait KeyStore {
    /// [`secrets::list`].
    async fn list(&self) -> anyhow::Result<Vec<String>>;
    /// [`secrets::set`].
    async fn set(&self, slot: &str, value: &str) -> anyhow::Result<()>;
    /// [`secrets::clear`].
    async fn clear(&self, slot: &str) -> anyhow::Result<()>;
    /// Relaunch the running plugins that declare `slot`, off the caller's task.
    fn relaunch(&self, slot: String);
}

/// The real keyring ([`crate::secrets`]) and the real launcher.
struct LoginKeyring;

impl KeyStore for LoginKeyring {
    async fn list(&self) -> anyhow::Result<Vec<String>> {
        secrets::list().await
    }
    async fn set(&self, slot: &str, value: &str) -> anyhow::Result<()> {
        secrets::set(slot, value).await
    }
    async fn clear(&self, slot: &str) -> anyhow::Result<()> {
        secrets::clear(slot).await
    }
    fn relaunch(&self, slot: String) {
        hytte::reactive::runtime::handle()
            .spawn(async move { plugin_launcher::relaunch_for_secret(&slot).await });
    }
}

/// `ListAiKeys` over `store`: a keyring failure is a `Failed` reply.
async fn list_keys(store: &impl KeyStore) -> zbus::fdo::Result<Vec<String>> {
    store.list().await.map_err(|err| {
        tracing::warn!(%err, "ListAiKeys failed");
        zbus::fdo::Error::Failed(format!("ListAiKeys failed: {err:#}"))
    })
}

/// `SetAiKey` over `store`: validate, store, then relaunch. A refusal returns
/// before the relaunch.
async fn set_key(store: &impl KeyStore, slot: String, value: &str) -> zbus::fdo::Result<()> {
    check_slot(&slot, "SetAiKey")?;
    check_key_value(value, "SetAiKey")?;
    store
        .set(&slot, value)
        .await
        .map_err(|err| key_fail(&slot, &err, "SetAiKey"))?;
    tracing::info!(%slot, "stored AI key; relaunching affected plugins");
    store.relaunch(slot);
    Ok(())
}

/// `ClearAiKey` over `store`: validate, clear, then relaunch. A refusal
/// returns before the relaunch.
async fn clear_key(store: &impl KeyStore, slot: String) -> zbus::fdo::Result<()> {
    check_slot(&slot, "ClearAiKey")?;
    store
        .clear(&slot)
        .await
        .map_err(|err| key_fail(&slot, &err, "ClearAiKey"))?;
    tracing::info!(%slot, "cleared AI key; relaunching affected plugins");
    store.relaunch(slot);
    Ok(())
}

/// Refuse a slot name [`secrets::is_valid_slot`] rejects, before the keyring
/// is touched, as the same `Failed` reply [`key_fail`] gives.
///
/// The name is not echoed into the reply (the caller knows what it sent),
/// and it is `Debug`-escaped in the log line: it is whatever the caller put
/// in the message, and failing the check means it is not a bounded
/// identifier.
fn check_slot(slot: &str, method: &str) -> zbus::fdo::Result<()> {
    if secrets::is_valid_slot(slot) {
        return Ok(());
    }
    tracing::warn!(?slot, "{method}: invalid slot name; refused");
    Err(zbus::fdo::Error::Failed(format!(
        "{method} failed: not a valid slot name (a lowercase ASCII letter, then lowercase \
         letters, digits, '-' or '_', at most 64 bytes)"
    )))
}

/// The longest API key [`ControlIface::set_ai_key`] stores, in bytes.
///
/// Every provider key in use is well under 256 bytes (an `OpenRouter` or
/// Anthropic key is ~100); 4 KiB leaves an order of magnitude for a token
/// format nobody ships yet while refusing what is plainly not a key — a pasted
/// file, or a session-bus client filling the keyring with a megabyte that the
/// launcher then copies into every relaunch's environment.
const MAX_KEY_BYTES: usize = 4096;

/// Refuse an AI-key `value` that cannot work as one, before the keyring is
/// touched, as the same `Failed` shape [`check_slot`] gives:
///
/// - **blank** (empty or all whitespace): `hytte_ai_providers::load_key`
///   trims the injected `<SLOT>_API_KEY` and treats an empty result as no key,
///   so storing one listed the slot as "key set" while every plugin ran
///   keyless.
/// - **a NUL byte**: the launcher hands the value to `systemd-run` as an
///   environment variable (`Command::env`), and an interior NUL makes `spawn`
///   fail with `InvalidInput` — every plugin declaring the slot would stop
///   launching, with nothing pointing at the key. A conformant bus already
///   refuses a NUL inside a D-Bus string, so over the session bus this is
///   defence in depth; the check costs one scan and does not depend on which
///   broker is running.
/// - **longer than [`MAX_KEY_BYTES`]**.
///
/// Neither the reply nor the log line carries the value — not even its
/// length: the reason names which rule failed, and the caller knows what it
/// sent.
fn check_key_value(value: &str, method: &str) -> zbus::fdo::Result<()> {
    let refused = if value.trim().is_empty() {
        "the key is empty"
    } else if value.contains('\0') {
        "the key contains a NUL byte"
    } else if value.len() > MAX_KEY_BYTES {
        "the key is longer than 4096 bytes"
    } else {
        return Ok(());
    };
    tracing::warn!("{method}: {refused}; refused");
    Err(zbus::fdo::Error::Failed(format!(
        "{method} failed: {refused}"
    )))
}

/// The longest city name [`ControlIface::set_manual_city`] accepts, in bytes.
///
/// Real place names are far shorter — the longest in common use run to ~85
/// Latin characters, and a non-Latin name at three UTF-8 bytes a character
/// still fits 85 of them. The bound exists because nothing else did: the
/// string is held in the place override, echoed by `GetPlace` until the first
/// resolution, and URL-encoded into the Open-Meteo query, and the session bus
/// passes messages far larger than any city.
const MAX_CITY_BYTES: usize = 256;

/// Refuse a `SetManualCity` argument that is blank or longer than
/// [`MAX_CITY_BYTES`], as a `Failed` reply, before the override is touched.
///
/// **Refused, not truncated.** Cutting at a char boundary would forward-
/// geocode a *prefix* of what was asked for, and a geocoder's search matches
/// prefixes — so a truncated name can resolve, silently, to a different place,
/// and the weather would be wrong with nothing saying so. A refusal is a toast
/// in the control-center. A blank name is refused too: it can never geocode,
/// so all it did was replace the label with an empty string until the lookup
/// failed (the control-center already filters it; `busctl` does not).
fn check_city(city: &str) -> zbus::fdo::Result<()> {
    let refused = if city.trim().is_empty() {
        "the city is empty"
    } else if city.len() > MAX_CITY_BYTES {
        "the city name is longer than 256 bytes"
    } else {
        return Ok(());
    };
    tracing::warn!("SetManualCity: {refused}; refused");
    Err(zbus::fdo::Error::Failed(format!(
        "SetManualCity failed: {refused}"
    )))
}

/// [`fail`] for the AI-key methods: the same log line and the same `Failed`
/// reply carrying the whole `anyhow` chain, naming the keyring slot rather
/// than a plugin. Only ever called with a slot [`check_slot`] has passed,
/// and with the keyring's own error, so the value being stored reaches
/// neither.
fn key_fail(slot: &str, err: &anyhow::Error, method: &str) -> zbus::fdo::Error {
    tracing::warn!(%err, %slot, "{method} failed");
    zbus::fdo::Error::Failed(format!("{method} for slot {slot} failed: {err:#}"))
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

    /// The message of a `Failed` reply, or a panic naming what came back.
    fn failed_message(reply: zbus::fdo::Result<()>) -> String {
        match reply {
            Err(zbus::fdo::Error::Failed(message)) => message,
            other => panic!("expected a Failed reply, got {other:?}"),
        }
    }

    /// An invalid slot is an error reply from both writing methods, not the
    /// empty reply that told the control-center the key was stored. The
    /// check runs before the keyring is touched, which is what makes this
    /// hermetic; the value is in neither the reply nor its log line.
    #[tokio::test]
    async fn an_invalid_slot_is_refused_with_an_error_reply() {
        let (captured, guard) = hytte_config::test_support::capture();
        let set = ControlIface
            .set_ai_key("Not A Slot".to_owned(), "sk-do-not-echo".to_owned())
            .await;
        let clear = ControlIface.clear_ai_key("../etc".to_owned()).await;
        drop(guard);
        let set = failed_message(set);
        assert!(
            set.starts_with("SetAiKey failed: not a valid slot name"),
            "{set}"
        );
        assert!(!set.contains("sk-do-not-echo"), "{set}");
        let clear = failed_message(clear);
        assert!(
            clear.starts_with("ClearAiKey failed: not a valid slot name"),
            "{clear}"
        );
        for event in captured.events() {
            assert!(
                !format!("{event:?}").contains("sk-do-not-echo"),
                "the value reached the journal: {event:?}"
            );
        }
    }

    /// A key value that cannot work is refused before the keyring is touched —
    /// which is what makes this hermetic: with a valid slot, a value past the
    /// check would reach `secrets::set` — and neither the reply nor any log
    /// line carries it.
    ///
    /// The slot is a valid name no provider uses, so a regression that lets a
    /// value through on a developer's machine with an unlocked keyring writes
    /// a junk item under `test-never-stored`, not over their real
    /// `openrouter` key.
    ///
    /// **Falsification:** drop the `check_key_value` call from `set_ai_key`
    /// → each case reaches the keyring and the reply is the keyring's error
    /// (measured: "The name is not activatable" with no Secret Service on the
    /// test bus), or `Ok` with one running.
    #[tokio::test]
    async fn an_unusable_key_value_is_refused_without_echoing_it() {
        let oversized = format!("sk-do-not-echo{}", "x".repeat(MAX_KEY_BYTES));
        let cases = [
            (String::new(), "the key is empty"),
            (" \t\n".to_owned(), "the key is empty"),
            (
                "sk-do-not-echo\0tail".to_owned(),
                "the key contains a NUL byte",
            ),
            (oversized, "the key is longer than 4096 bytes"),
        ];
        let (captured, guard) = hytte_config::test_support::capture();
        let mut replies = Vec::new();
        for (value, _) in &cases {
            replies.push(
                ControlIface
                    .set_ai_key("test-never-stored".to_owned(), value.clone())
                    .await,
            );
        }
        drop(guard);
        for ((_, why), reply) in cases.iter().zip(replies) {
            assert_eq!(failed_message(reply), format!("SetAiKey failed: {why}"));
        }
        for event in captured.events() {
            assert!(
                !format!("{event:?}").contains("sk-do-not-echo"),
                "the value reached the journal: {event:?}"
            );
        }
    }

    /// The bound is inclusive: a key of exactly [`MAX_KEY_BYTES`] and a
    /// real-looking one pass, so the check refuses only what it says.
    #[test]
    fn a_plausible_key_value_passes() {
        assert!(check_key_value("sk-or-v1-0123456789abcdef", "SetAiKey").is_ok());
        assert!(check_key_value(&"k".repeat(MAX_KEY_BYTES), "SetAiKey").is_ok());
    }

    /// `SetManualCity` refuses a blank or over-long name as a `Failed` reply.
    /// (`geoclue::set_manual_city` is a no-op without the service, so the
    /// reply is what this pins.)
    ///
    /// **Falsification:** drop the `check_city` call → both replies are `Ok`.
    #[tokio::test]
    async fn an_unusable_city_is_refused() {
        let blank = failed_message(ControlIface.set_manual_city("   ".to_owned()).await);
        assert_eq!(blank, "SetManualCity failed: the city is empty");
        let long = failed_message(
            ControlIface
                .set_manual_city("a".repeat(MAX_CITY_BYTES + 1))
                .await,
        );
        assert_eq!(
            long,
            "SetManualCity failed: the city name is longer than 256 bytes"
        );
    }

    /// The city bound counts **bytes** and is inclusive: 85 three-byte
    /// characters (255 bytes) pass, 86 (258 bytes) do not.
    #[test]
    fn the_city_bound_is_bytes_and_inclusive() {
        assert!(check_city("Llanfairpwllgwyngyll").is_ok());
        assert!(check_city(&"a".repeat(MAX_CITY_BYTES)).is_ok());
        assert!(check_city(&"\u{0e01}".repeat(85)).is_ok());
        assert!(check_city(&"\u{0e01}".repeat(86)).is_err());
    }

    /// A keyring failure travels as a `Failed` reply carrying the method, the
    /// slot and the whole `anyhow` chain — #707's `fail`, for a slot.
    #[test]
    fn a_keyring_failure_names_the_method_the_slot_and_the_cause() {
        let err = anyhow::anyhow!("the secret service is not running").context("storing the key");
        let zbus::fdo::Error::Failed(message) = key_fail("openrouter", &err, "SetAiKey") else {
            panic!("a keyring failure is a Failed reply");
        };
        assert_eq!(
            message,
            "SetAiKey for slot openrouter failed: storing the key: the secret service is not running"
        );
    }

    /// A [`KeyStore`] that answers from a script and records what it was asked.
    struct FakeKeys {
        refuse: bool,
        relaunched: std::sync::Mutex<Vec<String>>,
    }

    impl FakeKeys {
        fn new(refuse: bool) -> Self {
            Self {
                refuse,
                relaunched: std::sync::Mutex::default(),
            }
        }
        fn answer<T>(&self, ok: T) -> anyhow::Result<T> {
            if self.refuse {
                Err(anyhow::anyhow!("the keyring is locked"))
            } else {
                Ok(ok)
            }
        }
    }

    impl KeyStore for FakeKeys {
        async fn list(&self) -> anyhow::Result<Vec<String>> {
            self.answer(vec!["openrouter".to_owned()])
        }
        async fn set(&self, _slot: &str, _value: &str) -> anyhow::Result<()> {
            self.answer(())
        }
        async fn clear(&self, _slot: &str) -> anyhow::Result<()> {
            self.answer(())
        }
        fn relaunch(&self, slot: String) {
            self.relaunched.lock().unwrap().push(slot);
        }
    }

    /// A keyring that refuses a set reaches the caller as a `Failed` reply
    /// naming the cause, nothing is relaunched, and the value is in neither
    /// the reply nor the log.
    ///
    /// **Falsification:** make `set_key` swallow the refusal (`.ok()` in place
    /// of the `map_err(..)?`) -> the reply is `Ok` and this goes red.
    #[tokio::test]
    async fn a_keyring_that_refuses_a_set_fails_the_caller() {
        let keys = FakeKeys::new(true);
        let (captured, guard) = hytte_config::test_support::capture();
        let reply = set_key(&keys, "openrouter".to_owned(), "sk-do-not-echo").await;
        drop(guard);
        let message = failed_message(reply);
        assert!(
            message.starts_with("SetAiKey for slot openrouter failed:"),
            "{message}"
        );
        assert!(message.contains("the keyring is locked"), "{message}");
        assert!(!message.contains("sk-do-not-echo"), "{message}");
        assert!(keys.relaunched.lock().unwrap().is_empty());
        for event in captured.events() {
            assert!(
                !format!("{event:?}").contains("sk-do-not-echo"),
                "the value reached the journal: {event:?}"
            );
        }
    }

    /// A keyring that refuses a clear is a `Failed` reply and no relaunch.
    ///
    /// **Falsification:** swallow the refusal in `clear_key` -> red.
    #[tokio::test]
    async fn a_keyring_that_refuses_a_clear_fails_the_caller() {
        let keys = FakeKeys::new(true);
        let message = failed_message(clear_key(&keys, "openrouter".to_owned()).await);
        assert!(
            message.starts_with("ClearAiKey for slot openrouter failed:"),
            "{message}"
        );
        assert!(message.contains("the keyring is locked"), "{message}");
        assert!(keys.relaunched.lock().unwrap().is_empty());
    }

    /// A keyring that cannot be listed is a `Failed` reply, not an empty list
    /// (which says no key is stored).
    ///
    /// **Falsification:** make `list_keys` return `Ok(vec![])` on error -> red.
    #[tokio::test]
    async fn a_keyring_that_fails_a_list_fails_the_caller() {
        let keys = FakeKeys::new(true);
        let reply = list_keys(&keys).await;
        let Err(zbus::fdo::Error::Failed(message)) = reply else {
            panic!("expected a Failed reply, got {reply:?}");
        };
        assert!(message.starts_with("ListAiKeys failed:"), "{message}");
        assert!(message.contains("the keyring is locked"), "{message}");
    }

    /// The success path: set and clear reply `Ok` and relaunch the slot once
    /// each; list returns the keyring's slots.
    #[tokio::test]
    async fn a_keyring_that_accepts_replies_ok_and_relaunches() {
        let keys = FakeKeys::new(false);
        set_key(&keys, "openrouter".to_owned(), "sk-or-v1-0123456789abcdef")
            .await
            .unwrap();
        clear_key(&keys, "openrouter".to_owned()).await.unwrap();
        assert_eq!(
            *keys.relaunched.lock().unwrap(),
            ["openrouter", "openrouter"]
        );
        assert_eq!(list_keys(&keys).await.unwrap(), ["openrouter"]);
    }
}
