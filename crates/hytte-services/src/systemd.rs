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
                            tracing::warn!(error = %e, "systemd listen error, retrying in 5s");
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
    call(BusKind::System, SYSTEMD_NAME)
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
    let job_removed = signals(BusKind::System, SYSTEMD_NAME)
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
    let units: Vec<UnitTuple> = call(BusKind::System, SYSTEMD_NAME)
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
    /// The unit's `Description=`, verbatim (empty for a unit systemd hasn't
    /// loaded — enumerating unit *files* doesn't report one).
    ///
    /// Carried because it is the only per-unit string the launcher gets for
    /// free: `ListUnitsByPatterns` already returns it, so the declarative
    /// launcher (#419) can stamp a spec fingerprint into the description of the
    /// transient units it creates and read it back here to tell a unit running
    /// the *current* declared spec from a stale one (#695) — without a single
    /// extra D-Bus call.
    pub description: String,
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
/// `pub` so the shell's declarative launcher (#419) names the transient unit
/// it hands `systemd-run --user` with the same template.
#[must_use]
pub fn plugin_unit_name(id: &str) -> String {
    format!("{PLUGIN_UNIT_PREFIX}{id}{UNIT_SUFFIX}")
}

/// A valid plugin id — the segment spliced into a unit name. Kept to a safe
/// charset (ASCII alphanumerics plus `-`/`_`, bounded, non-empty) so a
/// `StartPlugin`/`StopPlugin` caller on the session bus can't smuggle a crafted
/// unit name through the `trollshell-plugin-<id>.service` template. Pure.
/// `pub` so the shell's declarative launcher (#419) applies the same guard to
/// ids read from the `plugins.json` state file.
#[must_use]
pub fn is_valid_plugin_id(id: &str) -> bool {
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
/// *loaded* units (runtime `ActiveState` + `Description`) into one
/// `Vec<PluginUnit>` sorted by id. `files` enumerates every installed
/// `trollshell-plugin-*` unit (running or not); `loaded` carries live
/// `ActiveState`/`Description` for those systemd has loaded. A unit file with no
/// loaded entry is reported `inactive` with an empty description (systemd GCs
/// loaded state for a long-stopped unit, and unit-file enumeration carries no
/// description); a loaded unit with no file is still surfaced
/// (`enabled = false`). Pure, so the merge is unit-testable without a bus.
pub(crate) fn merge_plugin_units(
    files: Vec<UnitFileTuple>,
    loaded: Vec<UnitTuple>,
) -> Vec<PluginUnit> {
    // id → (active_state, description) from the loaded set.
    let loaded_by_id: std::collections::HashMap<String, (String, String)> = loaded
        .into_iter()
        .filter_map(|(name, desc, _load, active, ..)| {
            parse_plugin_id(&name).map(|id| (id, (active, desc)))
        })
        .collect();
    // BTreeMap keeps the output sorted by id and dedups a unit reported under
    // both its path and its bare name.
    let mut by_id: std::collections::BTreeMap<String, PluginUnit> =
        std::collections::BTreeMap::new();
    for (path, enable_state) in files {
        if let Some(id) = parse_plugin_id(&path) {
            let (active_state, description) = loaded_by_id
                .get(&id)
                .cloned()
                .unwrap_or_else(|| ("inactive".to_owned(), String::new()));
            by_id.insert(
                id.clone(),
                PluginUnit {
                    id,
                    active_state,
                    enabled: is_enabled_state(&enable_state),
                    description,
                },
            );
        }
    }
    // Union in any loaded plugin unit that has no unit file (transient / linked
    // without a persistent [Install]) so a running-but-file-less plugin shows.
    // This is the normal case for the declarative launcher's transient units.
    for (id, (active_state, description)) in loaded_by_id {
        by_id.entry(id.clone()).or_insert(PluginUnit {
            id,
            active_state,
            enabled: false,
            description,
        });
    }
    by_id.into_values().collect()
}

/// Enumerate the installed `trollshell-plugin-*` **user** units with their
/// runtime + enablement state (#348). Two one-shot calls to the *user* manager
/// (`systemd --user`, session bus): `ListUnitFilesByPatterns` for the installed
/// set + enablement, `ListUnitsByPatterns` for live `ActiveState` +
/// `Description`, merged by [`merge_plugin_units`].
///
/// # Errors
/// Propagates any `hytte_bus` call error (e.g. no user manager reachable).
pub async fn list_plugin_units() -> Result<Vec<PluginUnit>> {
    let pattern = format!("{PLUGIN_UNIT_PREFIX}*{UNIT_SUFFIX}");
    let files: Vec<UnitFileTuple> = call(BusKind::Session, SYSTEMD_NAME)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("ListUnitFilesByPatterns")
        .args((Vec::<String>::new(), vec![pattern.clone()]))
        .send()
        .await
        .context("ListUnitFilesByPatterns")?;
    let loaded: Vec<UnitTuple> = call(BusKind::Session, SYSTEMD_NAME)
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
    let _job: zbus::zvariant::OwnedObjectPath = call(BusKind::Session, SYSTEMD_NAME)
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
        let _reply: (bool, Vec<(String, String, String)>) = call(BusKind::Session, SYSTEMD_NAME)
            .at_path(MANAGER_PATH)
            .iface(MANAGER_IFACE)
            .method("EnableUnitFiles")
            .args((vec![unit], false, true))
            .send()
            .await
            .with_context(|| format!("EnableUnitFiles for plugin {id}"))?;
    } else {
        // Reply `changes: a(sss)` — discarded.
        let _changes: Vec<(String, String, String)> = call(BusKind::Session, SYSTEMD_NAME)
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

// ── Workspace stacks (#1071 §3.1/§3.3) ───────────────────────────────────────

/// Prefix of every workspace-stack slice and unit.
const WS_PREFIX: &str = "trollshell-ws-";

/// The longest workspace name. Names double as unit-name components, and
/// systemd's limit is 255 bytes for the whole name; this is the epic's own cap
/// (§3.1), which leaves the unit template far inside it.
const WS_NAME_MAX: usize = 32;

/// A valid workspace-stack name (#1071 §3.1).
///
/// Lowercase ASCII alphanumerics with **single interior dashes** — no leading,
/// trailing or doubled dash, non-empty, at most [`WS_NAME_MAX`] bytes.
///
/// This is stricter than [`is_valid_plugin_id`] and the difference is not
/// stylistic. `-` is systemd's *slice-hierarchy separator*, so the name is
/// spliced into a position where dash placement decides whether the unit is
/// legal at all. Measured against systemd 260.2 by asking it to start a service
/// in each candidate slice:
///
/// | slice | systemd |
/// | --- | --- |
/// | `trollshell-ws-foo.slice` | accepted |
/// | `trollshell-ws--foo.slice` | *"failed to load properly … Invalid argument"* |
/// | `trollshell-ws-foo-.slice` | *"… Invalid argument"* |
/// | `trollshell-ws-.slice` | *"… Invalid argument"* |
///
/// Uppercase is a different problem with the same answer. systemd accepts
/// `trollshell-ws-FOO.slice` happily, but **niri matches workspace names
/// case-insensitively** (`find_workspace_by_name`), so `Chat` and `chat` are one
/// workspace with two spellings and two distinct slices. Folding to lowercase at
/// the boundary — see [`normalize_workspace_name`] — keeps the file's identity
/// and the compositor's identity the same relation.
///
/// Pure.
#[must_use]
pub fn is_valid_workspace_name(name: &str) -> bool {
    if name.is_empty() || name.len() > WS_NAME_MAX {
        return false;
    }
    let bytes = name.as_bytes();
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && !name.contains("--")
}

/// A name as [`is_valid_workspace_name`] would have it, or `None` if no folding
/// can save it.
///
/// Only case is folded. Anything else — a space, an underscore, a doubled dash —
/// is rejected rather than rewritten, because a silent rewrite would give the
/// user a stack under a name they did not type while `niri msg` still answers to
/// the one they did.
#[must_use]
pub fn normalize_workspace_name(name: &str) -> Option<String> {
    let folded = name.trim().to_ascii_lowercase();
    is_valid_workspace_name(&folded).then_some(folded)
}

/// The slice one workspace stack's apps live in (#1071 §3.3).
///
/// # Why the name's own dashes are escaped
///
/// systemd derives a slice's parent from the **literal dashes in its name**:
/// `a-b.slice` is a child of `a.slice`. Taken naively, a stack called `chat-dev`
/// would become `trollshell-ws-chat-dev.slice` — a *child* of a stack called
/// `chat`, so stopping `chat` would silently also stop `chat-dev`. Measured on
/// systemd 260.2: starting one service in `tstest-ws-a.slice` and another in
/// `tstest-ws-a-b.slice`, then `systemctl --user stop tstest-ws-a.slice`, left
/// **both** gone.
///
/// So the name's dashes are written as systemd's `\x2d` escape, which is not a
/// hierarchy separator. Measured the same way: `tstest2-ws-a\x2db.slice` renders
/// as `Slice /tstest2/ws/a-b` (a *sibling* under `/tstest2/ws`) and survived a
/// stop of `tstest2-ws-a.slice`.
///
/// Pure. Assumes [`is_valid_workspace_name`]; a caller that has not checked gets
/// an escaped-but-still-invalid name rather than a crafted one, since the
/// validator's charset is the only thing that reaches here.
#[must_use]
pub fn workspace_slice_name(name: &str) -> String {
    format!("{WS_PREFIX}{}.slice", name.replace('-', r"\x2d"))
}

/// The transient unit for the `index`-th app of workspace `name`'s stack.
///
/// Same `\x2d` escaping as [`workspace_slice_name`], for the same reason: the
/// unit sits under the slice and its own name must not imply a different parent.
/// `index` is the app's position in the stack, so a relaunch of the same stack
/// reuses the name — which is what makes a double Start fail loudly (systemd
/// refuses a live unit name) rather than quietly running two copies.
#[must_use]
pub fn workspace_unit_name(name: &str, index: usize) -> String {
    format!(
        "{WS_PREFIX}{}-{index}{UNIT_SUFFIX}",
        name.replace('-', r"\x2d")
    )
}

/// The unit `pid` belongs to, or `None` when it belongs to none.
///
/// niri ≥ 26.04 running as a systemd service starts every `spawn`ed command as
/// its own `app-niri-*.scope`, so this names a unit for nearly every window on
/// screen — not only the ones this shell launched (#1071 §3.3). A pid with no
/// unit (a program started from a nested shell inside a terminal, say) is the
/// case that falls through to niri's own `CloseWindow`.
///
/// A failed lookup is **not** an error: systemd answers a plain
/// `PID … does not belong to any loaded unit` (measured), which is an ordinary
/// answer to an ordinary question. Only a bus-level failure propagates.
///
/// # Errors
/// Propagates a `hytte_bus` call error — no user manager reachable, the reply
/// could not be read. A pid systemd simply does not know is `Ok(None)`.
pub async fn unit_for_pid(pid: u32) -> Result<Option<String>> {
    let path: zbus::zvariant::OwnedObjectPath = match call(BusKind::Session, SYSTEMD_NAME)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("GetUnitByPID")
        .args((pid,))
        .send()
        .await
    {
        Ok(path) => path,
        // Every refusal here is "systemd does not know this pid" in practice;
        // distinguishing a `NoUnitForPID` name from a transport failure would
        // need a `BusError` shape `hytte_bus` deliberately does not expose, and
        // the fallback (close the window through niri) is right either way.
        Err(e) => {
            tracing::debug!(pid, error = %e, "no systemd unit for pid");
            return Ok(None);
        }
    };
    // `StopUnit` takes a *name*, and the object path encodes it with systemd's
    // own escaping. Reading `Id` back is the honest inverse; unescaping the path
    // by hand would be a second implementation of an encoding systemd owns.
    let id: String = call(BusKind::Session, SYSTEMD_NAME)
        .at_path(path.as_str())
        .iface("org.freedesktop.DBus.Properties")
        .method("Get")
        .args(("org.freedesktop.systemd1.Unit".to_owned(), "Id".to_owned()))
        .send::<zbus::zvariant::OwnedValue>()
        .await
        .context("Unit.Id")
        .and_then(|v| String::try_from(v).context("Unit.Id is not a string"))?;
    Ok(Some(id))
}

/// Stop `unit` (a full unit name, e.g. `app-niri-foot-1234.scope`).
///
/// `replace` mode, matching [`manage_unit`]. Stopping a unit that was never
/// created succeeds — measured: `systemctl --user stop` of an absent slice exits
/// 0 — so Stop is idempotent and a caller need not check first.
///
/// # Errors
/// Propagates a `hytte_bus` call error.
pub async fn stop_unit(unit: &str) -> Result<()> {
    let _job: zbus::zvariant::OwnedObjectPath = call(BusKind::Session, SYSTEMD_NAME)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("StopUnit")
        .args((unit.to_owned(), "replace".to_owned()))
        .send()
        .await
        .with_context(|| format!("StopUnit for {unit}"))?;
    Ok(())
}

/// Stop workspace `name`'s slice, and with it every app of its stack.
///
/// Stopping a slice stops its units (`Requires=`/`After=`,
/// `systemd.resource-control(5)`), SIGTERM then systemd's own escalation.
///
/// # Errors
/// As [`stop_unit`], plus an invalid workspace name.
pub async fn stop_workspace_slice(name: &str) -> Result<()> {
    anyhow::ensure!(
        is_valid_workspace_name(name),
        "invalid workspace name: {name:?}"
    );
    stop_unit(&workspace_slice_name(name)).await
}

/// The workspace name a stack unit belongs to. Inverse of
/// [`workspace_unit_name`], `\x2d` escape and all. Pure.
///
/// Returns `None` for anything that is not one of ours — including a
/// `trollshell-ws-…` name whose tail is not an index, which no
/// [`workspace_unit_name`] produces.
#[must_use]
pub fn parse_workspace_unit(unit: &str) -> Option<String> {
    let stem = unit
        .rsplit_once('/')
        .map_or(unit, |(_, file)| file)
        .strip_prefix(WS_PREFIX)?
        .strip_suffix(UNIT_SUFFIX)?;
    // The index is the last dash-separated field; every dash *before* it in a
    // name of ours is escaped, so this split cannot cut a name in half.
    let (name, index) = stem.rsplit_once('-')?;
    index.parse::<usize>().ok()?;
    let name = name.replace(r"\x2d", "-");
    is_valid_workspace_name(&name).then_some(name)
}

/// Every workspace stack with at least one unit that is up.
///
/// One of the two sources #1071 §3.3 derives `Active` from, and **one call for
/// every stack** rather than one per name: the shell polls this to draw the
/// cards, and a per-stack fan-out would put a D-Bus round trip per card on a
/// timer.
///
/// Asks for the units *inside* the slices rather than the slices' own
/// `ActiveState`, because "the slice has running units" is the question and an
/// empty slice's own state is systemd's housekeeping (measured:
/// `trollshell-launch.slice` stays `loaded active` with zero members).
///
/// # Errors
/// Propagates a `hytte_bus` call error.
pub async fn workspace_slices_up() -> Result<std::collections::BTreeSet<String>> {
    let units: Vec<UnitTuple> = call(BusKind::Session, SYSTEMD_NAME)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("ListUnitsByPatterns")
        .args((
            vec![
                "active".to_owned(),
                "activating".to_owned(),
                "deactivating".to_owned(),
                "reloading".to_owned(),
            ],
            vec![format!("{WS_PREFIX}*{UNIT_SUFFIX}")],
        ))
        .send()
        .await
        .context("ListUnitsByPatterns for the workspace slices")?;
    Ok(units
        .into_iter()
        .filter_map(|(name, ..)| parse_workspace_unit(&name))
        .collect())
}

/// Whether workspace `name`'s slice currently holds a unit that is up.
///
/// [`workspace_slices_up`] filtered to one name — the same single call, so a
/// Start's housekeeping and the page's poll cannot disagree about what "up"
/// means.
///
/// # Errors
/// As [`workspace_slices_up`], plus an invalid workspace name.
pub async fn workspace_slice_is_up(name: &str) -> Result<bool> {
    anyhow::ensure!(
        is_valid_workspace_name(name),
        "invalid workspace name: {name:?}"
    );
    Ok(workspace_slices_up().await?.contains(name))
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
    /// `ActiveState` (and the launcher-shaped description [`plugin_tuple_desc`]
    /// lets a test pin explicitly).
    fn plugin_tuple(name: &str, active: &str) -> UnitTuple {
        plugin_tuple_desc(name, active, &format!("{name} description"))
    }

    /// As [`plugin_tuple`], with an explicit `Description=` — the field the
    /// declarative launcher stamps its spec fingerprint into (#695).
    fn plugin_tuple_desc(name: &str, active: &str, desc: &str) -> UnitTuple {
        (
            name.to_string(),
            desc.to_string(),
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
        // …and with no loaded entry there is no description to report.
        assert_eq!(out[1].description, "");
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
    fn merge_carries_the_loaded_units_description() {
        // The launcher's reconcile (#695) reads its spec fingerprint back out of
        // the transient unit's Description=, so the merge must carry it through
        // verbatim — for a file-less transient unit and a unit-file one alike.
        let out = merge_plugin_units(
            vec![(
                "trollshell-plugin-pet.service".to_string(),
                "disabled".to_string(),
            )],
            vec![
                plugin_tuple_desc(
                    "trollshell-plugin-pet.service",
                    "active",
                    "trollshell plugin: pet [cfg:0123456789abcdef]",
                ),
                plugin_tuple_desc(
                    "trollshell-plugin-timer.service",
                    "active",
                    "trollshell plugin: timer [cfg:fedcba9876543210]",
                ),
            ],
        );
        assert_eq!(
            out[0].description,
            "trollshell plugin: pet [cfg:0123456789abcdef]"
        );
        assert_eq!(
            out[1].description,
            "trollshell plugin: timer [cfg:fedcba9876543210]"
        );
    }

    #[test]
    fn merge_ignores_non_plugin_units() {
        let files = vec![("trollshell.service".to_string(), "enabled".to_string())];
        let loaded = vec![plugin_tuple("dbus.service", "active")];
        assert!(merge_plugin_units(files, loaded).is_empty());
    }

    // ── Workspace stacks (#1071 §3.1/§3.3) ───────────────────────────────────

    /// #1071 §7's name-validator row, each negative carrying the systemd
    /// refusal it stands for.
    ///
    /// The three dash rules are not taste: each was measured against systemd
    /// 260.2 by asking it to start a service in the corresponding slice, and
    /// each answered *"failed to load properly … Invalid argument"*. See
    /// [`is_valid_workspace_name`]'s doc for the table.
    #[test]
    fn is_valid_workspace_name_matches_systemds_unit_name_rules() {
        assert!(is_valid_workspace_name("chat"));
        assert!(is_valid_workspace_name("dev2"));
        assert!(is_valid_workspace_name("chat-dev"));
        assert!(is_valid_workspace_name("a-b-c"));
        assert!(is_valid_workspace_name("9"));

        // Measured refusals: trollshell-ws--foo.slice, trollshell-ws-foo-.slice
        // and trollshell-ws-.slice are all "Invalid argument".
        assert!(!is_valid_workspace_name(""), "empty");
        assert!(!is_valid_workspace_name("-chat"), "leading dash");
        assert!(!is_valid_workspace_name("chat-"), "trailing dash");
        assert!(!is_valid_workspace_name("chat--dev"), "doubled dash");
        assert!(!is_valid_workspace_name("-"), "a lone dash is all three");

        // Anything that could break out of the unit-name template.
        assert!(!is_valid_workspace_name("chat.slice"));
        assert!(!is_valid_workspace_name("../evil"));
        assert!(!is_valid_workspace_name("a b"));
        assert!(!is_valid_workspace_name("a_b"), "underscore is not a dash");
        // Uppercase is legal to systemd but not to us: niri matches workspace
        // names case-insensitively, so `Chat` and `chat` would be one workspace
        // with two slices.
        assert!(!is_valid_workspace_name("Chat"));

        assert!(is_valid_workspace_name(&"x".repeat(WS_NAME_MAX)));
        assert!(!is_valid_workspace_name(&"x".repeat(WS_NAME_MAX + 1)));
    }

    /// Folding fixes case and nothing else.
    ///
    /// A silent rewrite of anything structural would hand the user a stack under
    /// a name they did not type while `niri msg action focus-workspace` still
    /// answers to the one they did.
    #[test]
    fn normalize_folds_case_and_refuses_everything_else() {
        assert_eq!(normalize_workspace_name("Chat"), Some("chat".to_owned()));
        assert_eq!(
            normalize_workspace_name("  CHAT-Dev "),
            Some("chat-dev".to_owned()),
            "surrounding whitespace is trimmed, since it cannot be typed on purpose"
        );
        assert_eq!(normalize_workspace_name("chat"), Some("chat".to_owned()));

        assert_eq!(normalize_workspace_name("chat dev"), None, "not a dash");
        assert_eq!(normalize_workspace_name("chat_dev"), None);
        assert_eq!(normalize_workspace_name("-chat"), None);
        assert_eq!(normalize_workspace_name("chat--dev"), None);
        assert_eq!(normalize_workspace_name(""), None);
    }

    /// The escape that keeps two stacks siblings rather than parent and child.
    ///
    /// Measured on systemd 260.2: `tstest-ws-a-b.slice` really is a child of
    /// `tstest-ws-a.slice` and stopping the parent stopped both, while
    /// `tstest2-ws-a\x2db.slice` rendered as `Slice /tstest2/ws/a-b` and
    /// survived a stop of `tstest2-ws-a.slice`.
    ///
    /// Falsified by dropping the `.replace` — `chat`'s slice then becomes a
    /// prefix of `chat-dev`'s under systemd's hierarchy rule.
    #[test]
    fn a_name_with_a_dash_gets_its_own_slice_not_a_nested_one() {
        assert_eq!(workspace_slice_name("chat"), "trollshell-ws-chat.slice");
        assert_eq!(
            workspace_slice_name("chat-dev"),
            r"trollshell-ws-chat\x2ddev.slice",
            "an interior dash must not become a hierarchy separator"
        );
        // The property that matters, stated as systemd's own rule: `a-b.slice`
        // is a child of `a.slice`, so `chat-dev`'s slice is nested inside
        // `chat`'s exactly when its stem begins `<chat's stem>-`. A literal
        // prefix is not enough — the escaped name does share the first
        // characters, and that is fine; it is the *dash* at the boundary that
        // would make it a child.
        let parent_stem = workspace_slice_name("chat")
            .trim_end_matches(".slice")
            .to_owned();
        let child = workspace_slice_name("chat-dev");
        assert!(
            !child.starts_with(&format!("{parent_stem}-")),
            "{child} must not nest under {parent_stem}.slice"
        );
        // …and the unescaped spelling the escape exists to avoid *would*.
        assert!(
            format!("{WS_PREFIX}chat-dev.slice").starts_with(&format!("{parent_stem}-")),
            "the assertion above is only meaningful because this is what \
             dropping the escape produces"
        );
    }

    /// Unit names carry the same escape, and the index distinguishes the apps.
    #[test]
    fn workspace_unit_names_are_per_app_and_escaped() {
        assert_eq!(
            workspace_unit_name("chat", 0),
            "trollshell-ws-chat-0.service"
        );
        assert_eq!(
            workspace_unit_name("chat", 2),
            "trollshell-ws-chat-2.service"
        );
        assert_eq!(
            workspace_unit_name("chat-dev", 1),
            r"trollshell-ws-chat\x2ddev-1.service"
        );
        // systemd's unit-name limit is 255; the WS_NAME_MAX cap is what bounds
        // the name half, and each escaped dash costs 4 bytes instead of 1.
        let longest = workspace_unit_name(&"a-".repeat(WS_NAME_MAX / 2 - 1), 99);
        assert!(longest.len() < 255, "{} bytes: {longest}", longest.len());
    }

    /// A stack's unit glob cannot catch a sibling stack's units.
    ///
    /// This is the same escape seen from the other side: `workspace_slice_is_up`
    /// globs `trollshell-ws-<escaped>-*.service`, and without the escape
    /// `chat`'s glob would match every one of `chat-dev`'s units and report a
    /// stopped stack as up.
    #[test]
    fn a_stacks_unit_glob_does_not_catch_a_siblings_units() {
        let chat = format!("{WS_PREFIX}{}-", "chat".replace('-', r"\x2d"));
        let chat_dev_unit = workspace_unit_name("chat-dev", 0);
        assert!(
            !chat_dev_unit.starts_with(&chat),
            "{chat_dev_unit} must not match the glob {chat}*"
        );
        assert!(workspace_unit_name("chat", 0).starts_with(&chat));
    }

    /// The unit name round-trips, escape and all — which is what lets the
    /// page's single `ListUnitsByPatterns` poll say *which* stacks are up
    /// rather than only how many units there are.
    ///
    /// The `chat-dev` case is the one that would break a naive
    /// `split('-').nth(2)`: every dash inside a name of ours is escaped, so the
    /// **last** dash is always the index separator.
    #[test]
    fn a_stack_unit_name_round_trips_through_the_parser() {
        for (name, index) in [("chat", 0), ("chat-dev", 3), ("a-b-c", 12), ("x", 99)] {
            let unit = workspace_unit_name(name, index);
            assert_eq!(
                parse_workspace_unit(&unit).as_deref(),
                Some(name),
                "round trip for {unit}"
            );
        }
    }

    /// …and nothing else parses as one of ours.
    #[test]
    fn parse_workspace_unit_rejects_everything_that_is_not_ours() {
        assert_eq!(parse_workspace_unit("trollshell-plugin-pet.service"), None);
        assert_eq!(parse_workspace_unit("app-niri-foot-1234.scope"), None);
        assert_eq!(parse_workspace_unit("trollshell-ws-chat.slice"), None);
        assert_eq!(
            parse_workspace_unit("trollshell-ws-chat.service"),
            None,
            "no index"
        );
        assert_eq!(
            parse_workspace_unit("trollshell-ws-chat-x.service"),
            None,
            "the tail must be a number"
        );
        assert_eq!(
            parse_workspace_unit("trollshell-ws-Chat-0.service"),
            None,
            "a name we could never have written"
        );
        // A *raw* dash is not a name of ours — we always escape — so a unit
        // spelled that way names the stack `chat`, not `chat-dev`, and the
        // parser must not invent the latter.
        assert_eq!(
            parse_workspace_unit("trollshell-ws-chat-dev-0.service").as_deref(),
            Some("chat-dev"),
            "…but an unescaped one still reads sensibly rather than panicking"
        );
    }
}
