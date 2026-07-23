//! systemd service — surfaces the current set of failed units via
//! `org.freedesktop.systemd1.Manager`. Signal-driven: subscribes to
//! `JobRemoved` and re-fetches `ListUnitsFiltered(["failed"])` on
//! each emission.
//!
//! Notes on systemd dbus:
//! - Uses the **system bus** (`org.freedesktop.systemd1` on the system bus
//!   is the system manager; `systemd --user` exposes the same name on the
//!   session bus but this service monitors the system manager).
//! - `Manager.Subscribe()` MUST be called for the daemon to start
//!   emitting signals to this client. Without it `JobRemoved` never
//!   fires.
//! - `JobRemoved` covers every unit transition (start/stop/restart
//!   complete) regardless of result, so it's a reasonable proxy for
//!   "the failed-unit set may have changed". Cheaper than per-unit
//!   `PropertiesChanged` subscriptions for the v0.2.5 fidelity.
//!
//! All D-Bus I/O goes through [`hytte_bus::call`] and [`hytte_bus::signals`]
//! so the shared connection supervisor handles reconnects automatically.
//!
//! # Public API
//!
//! ```ignore
//! .with(systemd::service())
//!
//! systemd::failed_units() -> impl Signal<Item = Vec<FailedUnit>>
//! ```

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_bus::{BusKind, call, signals};
use hytte_reactive::{Service, registry, spawn_supervised};
use std::time::Duration;

const SYSTEMD_NAME: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_IFACE: &str = "org.freedesktop.systemd1.Manager";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedUnit {
    pub name: String,
    pub description: String,
    pub sub_state: String,
}

#[doc(hidden)]
pub struct SystemdHandles {
    pub(crate) failed_units: Mutable<Vec<FailedUnit>>,
}

impl Default for SystemdHandles {
    fn default() -> Self {
        Self {
            failed_units: Mutable::new(Vec::new()),
        }
    }
}

pub struct SystemdService;

impl Service for SystemdService {
    type Handles = SystemdHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = SystemdHandles::default();
        let writer = handles.failed_units.clone();

        spawn_supervised("systemd", move || {
            let writer = writer.clone();
            async move {
                loop {
                    match listen(&writer).await {
                        Ok(()) => tracing::warn!("systemd listen loop ended, retrying in 5s"),
                        Err(e) => {
                            tracing::warn!(error = %e, "systemd listen error, retrying in 5s")
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        });

        handles
    }
}

#[must_use]
pub fn service() -> SystemdService {
    SystemdService
}

pub fn failed_units() -> impl Signal<Item = Vec<FailedUnit>> {
    registry::with(|r| {
        r.get::<SystemdHandles>()
            .expect("systemd::service() not registered")
            .failed_units
            .signal_cloned()
    })
}

// ── Listen loop ───────────────────────────────────────────────────────────────

/// systemd `ListUnitsFiltered` reply tuple shape:
/// (`name`, `description`, `load_state`, `active_state`, `sub_state`, `follower`,
///  `object_path`, `job_id`, `job_type`, `job_object_path`).
type UnitTuple = (
    String,
    String,
    String,
    String,
    String,
    String,
    zbus::zvariant::OwnedObjectPath,
    u32,
    String,
    zbus::zvariant::OwnedObjectPath,
);

async fn listen(writer: &Mutable<Vec<FailedUnit>>) -> Result<()> {
    // REQUIRED: systemd only emits signals to clients that have called
    // Subscribe(). Without this, JobRemoved never fires.
    call(SYSTEMD_NAME)
        .bus(BusKind::System)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("Subscribe")
        .args(())
        .send::<()>()
        .await
        .context("Manager.Subscribe")?;

    // Initial fetch of failed units.
    refresh_failed(writer).await?;

    // Subscribe to JobRemoved so we re-fetch whenever a job completes
    // (which may change the failed-unit set).
    let job_removed = signals(SYSTEMD_NAME)
        .bus(BusKind::System)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .signal("JobRemoved")
        .start();

    let mut events = job_removed.events();

    while events.next().await.is_some() {
        if let Err(e) = refresh_failed(writer).await {
            tracing::warn!(error = %e, "systemd refresh after JobRemoved failed");
        }
    }
    Ok(())
}

async fn refresh_failed(writer: &Mutable<Vec<FailedUnit>>) -> Result<()> {
    let units: Vec<UnitTuple> = call(SYSTEMD_NAME)
        .bus(BusKind::System)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("ListUnitsFiltered")
        .args((vec!["failed".to_string()],))
        .send()
        .await
        .context("ListUnitsFiltered")?;

    writer.set(parse_units(units));
    Ok(())
}

pub(crate) fn parse_units(units: Vec<UnitTuple>) -> Vec<FailedUnit> {
    let mut out: Vec<FailedUnit> = units
        .into_iter()
        .map(
            |(name, description, _load, _active, sub_state, ..)| FailedUnit {
                name,
                description,
                sub_state,
            },
        )
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// ── Plugin unit management (#348) ─────────────────────────────────────────────
//
// The control-center Plugins tab (#348) lists and starts/stops plugins, which
// run as `trollshell-plugin-<id>` **user** units — the host is transport-only
// (see `trollshell/src/plugins.rs`). Unlike [`failed_units`] above (which
// monitors the *system* manager), these talk to the **user** manager:
// `systemd --user` owns `org.freedesktop.systemd1` on the **session** bus, so
// every call here overrides to `BusKind::Session` (the system manager knows
// nothing of a user's plugin units). They are one-shot request/response calls —
// no `Manager.Subscribe()` (that's only needed to receive *signals*).
//
// Everything below is plain `async fn` (no registry, no `Mutable`): the shell's
// `control.rs` D-Bus handlers `.await` them straight off the D-Bus task, so they
// stay cross-thread-clean by construction. The pure name/state helpers are
// factored out so the parse + merge are unit-testable without a live bus.

const PLUGIN_UNIT_PREFIX: &str = "trollshell-plugin-";
const UNIT_SUFFIX: &str = ".service";

/// One `trollshell-plugin-<id>` **user** unit's state, as surfaced to the
/// control-center Plugins tab (#348).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginUnit {
    /// Plugin id parsed from the unit name
    /// (`trollshell-plugin-<id>.service` → `<id>`).
    pub id: String,
    /// systemd `ActiveState` — `active` / `inactive` / `failed` / `activating` /
    /// `deactivating` / …
    pub active_state: String,
    /// Whether the unit file is enabled (persisted to auto-start at login).
    pub enabled: bool,
}

/// `ListUnitFilesByPatterns` reply tuple: (`unit_file_path_or_name`, `state`),
/// where `state` is the *enablement* state (`enabled` / `disabled` / `static` /
/// …), not the runtime `ActiveState`.
type UnitFileTuple = (String, String);

/// `trollshell-plugin-<id>.service` (or a full unit-file path ending in it) →
/// `Some("<id>")`; anything else → `None`. Inverse of [`plugin_unit_name`].
/// Pure, so the parse is unit-testable.
pub(crate) fn parse_plugin_id(unit: &str) -> Option<String> {
    // `ListUnitFilesByPatterns` yields a full path on some systemd versions and a
    // bare unit name on others — take the basename so both parse.
    let base = unit.rsplit('/').next().unwrap_or(unit);
    let id = base
        .strip_prefix(PLUGIN_UNIT_PREFIX)?
        .strip_suffix(UNIT_SUFFIX)?;
    (!id.is_empty()).then(|| id.to_owned())
}

/// `<id>` → `trollshell-plugin-<id>.service`. Inverse of [`parse_plugin_id`].
fn plugin_unit_name(id: &str) -> String {
    format!("{PLUGIN_UNIT_PREFIX}{id}{UNIT_SUFFIX}")
}

/// A valid plugin id — the segment spliced into a unit name. Kept to a safe
/// charset (ASCII alphanumerics plus `-`/`_`, bounded, non-empty) so a
/// `StartPlugin`/`StopPlugin` caller on the session bus can't smuggle a crafted
/// unit name through the `trollshell-plugin-<id>.service` template. Pure.
pub(crate) fn is_valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A systemd unit-file *enablement* state string that means "auto-starts at
/// login". `disabled` / `static` / `masked` / `linked` / … don't. Pure.
pub(crate) fn is_enabled_state(state: &str) -> bool {
    matches!(state, "enabled" | "enabled-runtime")
}

/// Merge the *installed* plugin unit files (enablement) with the currently
/// *loaded* units (runtime `ActiveState`) into one `Vec<PluginUnit>` sorted by
/// id. `files` enumerates every installed `trollshell-plugin-*` unit (running or
/// not); `loaded` carries live `ActiveState` for those systemd has loaded. A
/// unit file with no loaded entry is reported `inactive` (systemd GCs loaded
/// state for a long-stopped unit); a loaded unit with no file is still surfaced
/// (`enabled = false`). Pure, so the merge is unit-testable without a bus.
pub(crate) fn merge_plugin_units(
    files: Vec<UnitFileTuple>,
    loaded: Vec<UnitTuple>,
) -> Vec<PluginUnit> {
    // id → active_state from the loaded set.
    let active_by_id: std::collections::HashMap<String, String> = loaded
        .into_iter()
        .filter_map(|(name, _desc, _load, active, ..)| {
            parse_plugin_id(&name).map(|id| (id, active))
        })
        .collect();
    // BTreeMap keeps the output sorted by id and dedups a unit reported under
    // both its path and its bare name.
    let mut by_id: std::collections::BTreeMap<String, PluginUnit> =
        std::collections::BTreeMap::new();
    for (path, enable_state) in files {
        if let Some(id) = parse_plugin_id(&path) {
            let active_state = active_by_id
                .get(&id)
                .cloned()
                .unwrap_or_else(|| "inactive".to_owned());
            by_id.insert(
                id.clone(),
                PluginUnit {
                    id,
                    active_state,
                    enabled: is_enabled_state(&enable_state),
                },
            );
        }
    }
    // Union in any loaded plugin unit that has no unit file (transient / linked
    // without a persistent [Install]) so a running-but-file-less plugin shows.
    for (id, active_state) in active_by_id {
        by_id.entry(id.clone()).or_insert(PluginUnit {
            id,
            active_state,
            enabled: false,
        });
    }
    by_id.into_values().collect()
}

/// Enumerate the installed `trollshell-plugin-*` **user** units with their
/// runtime + enablement state (#348). Two one-shot calls to the *user* manager
/// (`systemd --user`, session bus): `ListUnitFilesByPatterns` for the installed
/// set + enablement, `ListUnitsByPatterns` for live `ActiveState`, merged by
/// [`merge_plugin_units`].
///
/// # Errors
/// Propagates any `hytte_bus` call error (e.g. no user manager reachable).
pub async fn list_plugin_units() -> Result<Vec<PluginUnit>> {
    let pattern = format!("{PLUGIN_UNIT_PREFIX}*{UNIT_SUFFIX}");
    let files: Vec<UnitFileTuple> = call(SYSTEMD_NAME)
        .bus(BusKind::Session)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("ListUnitFilesByPatterns")
        .args((Vec::<String>::new(), vec![pattern.clone()]))
        .send()
        .await
        .context("ListUnitFilesByPatterns")?;
    let loaded: Vec<UnitTuple> = call(SYSTEMD_NAME)
        .bus(BusKind::Session)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("ListUnitsByPatterns")
        .args((Vec::<String>::new(), vec![pattern]))
        .send()
        .await
        .context("ListUnitsByPatterns")?;
    Ok(merge_plugin_units(files, loaded))
}

/// Start plugin `id`'s user unit now (`StartUnit(<unit>, "replace")`). Does not
/// change enablement — pair with [`set_plugin_enabled`] to also persist it.
///
/// # Errors
/// Invalid id, or any `hytte_bus` call error.
pub async fn start_plugin(id: &str) -> Result<()> {
    manage_unit(id, "StartUnit").await
}

/// Stop plugin `id`'s user unit now (`StopUnit(<unit>, "replace")`). Does not
/// change enablement — pair with [`set_plugin_enabled`] to also persist it.
///
/// # Errors
/// Invalid id, or any `hytte_bus` call error.
pub async fn stop_plugin(id: &str) -> Result<()> {
    manage_unit(id, "StopUnit").await
}

/// `StartUnit`/`StopUnit` share everything but the method name and the returned
/// job path (which we discard).
async fn manage_unit(id: &str, method: &'static str) -> Result<()> {
    anyhow::ensure!(is_valid_plugin_id(id), "invalid plugin id: {id:?}");
    let unit = plugin_unit_name(id);
    let _job: zbus::zvariant::OwnedObjectPath = call(SYSTEMD_NAME)
        .bus(BusKind::Session)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method(method)
        .args((unit, "replace".to_owned()))
        .send()
        .await
        .with_context(|| format!("{method} for plugin {id}"))?;
    Ok(())
}

/// Enable or disable plugin `id`'s user unit for persistence across logins
/// (`EnableUnitFiles` / `DisableUnitFiles`, non-runtime). Runtime state is
/// unaffected — pair with [`start_plugin`]/[`stop_plugin`] to also apply it now.
///
/// # Errors
/// Invalid id, or any `hytte_bus` call error.
pub async fn set_plugin_enabled(id: &str, enabled: bool) -> Result<()> {
    anyhow::ensure!(is_valid_plugin_id(id), "invalid plugin id: {id:?}");
    let unit = plugin_unit_name(id);
    if enabled {
        // (files, runtime = false → persistent, force = true → replace any
        //  conflicting symlink). Reply `(carries_install_info, changes)` — discarded.
        let _reply: (bool, Vec<(String, String, String)>) = call(SYSTEMD_NAME)
            .bus(BusKind::Session)
            .at_path(MANAGER_PATH)
            .iface(MANAGER_IFACE)
            .method("EnableUnitFiles")
            .args((vec![unit], false, true))
            .send()
            .await
            .with_context(|| format!("EnableUnitFiles for plugin {id}"))?;
    } else {
        // Reply `changes: a(sss)` — discarded.
        let _changes: Vec<(String, String, String)> = call(SYSTEMD_NAME)
            .bus(BusKind::Session)
            .at_path(MANAGER_PATH)
            .iface(MANAGER_IFACE)
            .method("DisableUnitFiles")
            .args((vec![unit], false))
            .send()
            .await
            .with_context(|| format!("DisableUnitFiles for plugin {id}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(s: &str) -> zbus::zvariant::OwnedObjectPath {
        zbus::zvariant::ObjectPath::try_from(s).unwrap().into()
    }

    fn unit(name: &str, desc: &str, sub: &str) -> UnitTuple {
        (
            name.to_string(),
            desc.to_string(),
            "loaded".to_string(),
            "failed".to_string(),
            sub.to_string(),
            String::new(),
            op("/org/freedesktop/systemd1/unit/dummy"),
            0,
            String::new(),
            op("/"),
        )
    }

    #[test]
    fn parse_units_extracts_name_description_sub_state() {
        let input = vec![unit("polkit.service", "Authorization Manager", "failed")];
        let out = parse_units(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "polkit.service");
        assert_eq!(out[0].description, "Authorization Manager");
        assert_eq!(out[0].sub_state, "failed");
    }

    #[test]
    fn parse_units_sorts_by_name() {
        let input = vec![
            unit("zzz.service", "z", "failed"),
            unit("aaa.service", "a", "failed"),
            unit("mmm.service", "m", "failed"),
        ];
        let out = parse_units(input);
        let names: Vec<&str> = out.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, vec!["aaa.service", "mmm.service", "zzz.service"]);
    }

    #[test]
    fn parse_units_empty_input_yields_empty_output() {
        let out = parse_units(Vec::new());
        assert!(out.is_empty());
    }

    // ── Plugin unit management (#348) ────────────────────────────────────────

    /// A loaded `ListUnitsByPatterns` tuple for a plugin unit with a given
    /// `ActiveState`.
    fn plugin_tuple(name: &str, active: &str) -> UnitTuple {
        (
            name.to_string(),
            format!("{name} description"),
            "loaded".to_string(),
            active.to_string(),
            "running".to_string(),
            String::new(),
            op("/org/freedesktop/systemd1/unit/dummy"),
            0,
            String::new(),
            op("/"),
        )
    }

    #[test]
    fn parse_plugin_id_strips_prefix_and_suffix() {
        assert_eq!(
            parse_plugin_id("trollshell-plugin-pet.service").as_deref(),
            Some("pet")
        );
        // A hyphenated id survives (only the fixed prefix/suffix are stripped).
        assert_eq!(
            parse_plugin_id("trollshell-plugin-clock-demo.service").as_deref(),
            Some("clock-demo")
        );
    }

    #[test]
    fn parse_plugin_id_accepts_full_unit_file_path() {
        assert_eq!(
            parse_plugin_id("/home/u/.config/systemd/user/trollshell-plugin-weather.service")
                .as_deref(),
            Some("weather")
        );
    }

    #[test]
    fn parse_plugin_id_rejects_non_plugin_and_empty() {
        assert_eq!(parse_plugin_id("trollshell.service"), None);
        assert_eq!(parse_plugin_id("plasma-plugin-foo.service"), None);
        assert_eq!(parse_plugin_id("trollshell-plugin-pet.timer"), None);
        // Prefix + suffix with nothing between must not yield an empty id.
        assert_eq!(parse_plugin_id("trollshell-plugin-.service"), None);
    }

    #[test]
    fn plugin_unit_name_round_trips_with_parse() {
        let name = plugin_unit_name("clock-demo");
        assert_eq!(name, "trollshell-plugin-clock-demo.service");
        assert_eq!(parse_plugin_id(&name).as_deref(), Some("clock-demo"));
    }

    #[test]
    fn is_valid_plugin_id_guards_the_charset() {
        assert!(is_valid_plugin_id("pet"));
        assert!(is_valid_plugin_id("clock-demo"));
        assert!(is_valid_plugin_id("preem_demo2"));
        assert!(!is_valid_plugin_id(""));
        // Anything that could break out of the unit-name template is rejected.
        assert!(!is_valid_plugin_id("pet.service"));
        assert!(!is_valid_plugin_id("../evil"));
        assert!(!is_valid_plugin_id("a b"));
        assert!(!is_valid_plugin_id(&"x".repeat(65)));
    }

    #[test]
    fn is_enabled_state_matches_only_enabled_variants() {
        assert!(is_enabled_state("enabled"));
        assert!(is_enabled_state("enabled-runtime"));
        assert!(!is_enabled_state("disabled"));
        assert!(!is_enabled_state("static"));
        assert!(!is_enabled_state("masked"));
    }

    #[test]
    fn merge_pairs_enablement_with_active_state_sorted_by_id() {
        let files = vec![
            (
                "/x/trollshell-plugin-weather.service".to_string(),
                "enabled".to_string(),
            ),
            (
                "trollshell-plugin-pet.service".to_string(),
                "disabled".to_string(),
            ),
        ];
        let loaded = vec![plugin_tuple("trollshell-plugin-pet.service", "active")];
        let out = merge_plugin_units(files, loaded);
        // Sorted by id: pet, weather.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "pet");
        assert_eq!(out[0].active_state, "active");
        assert!(!out[0].enabled);
        assert_eq!(out[1].id, "weather");
        // No loaded entry for weather → reported inactive.
        assert_eq!(out[1].active_state, "inactive");
        assert!(out[1].enabled);
    }

    #[test]
    fn merge_surfaces_loaded_unit_without_a_file() {
        // A running plugin with no persistent unit file still shows (disabled).
        let out = merge_plugin_units(
            Vec::new(),
            vec![plugin_tuple("trollshell-plugin-terminal.service", "active")],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "terminal");
        assert_eq!(out[0].active_state, "active");
        assert!(!out[0].enabled);
    }

    #[test]
    fn merge_ignores_non_plugin_units() {
        let files = vec![("trollshell.service".to_string(), "enabled".to_string())];
        let loaded = vec![plugin_tuple("dbus.service", "active")];
        assert!(merge_plugin_units(files, loaded).is_empty());
    }
}
