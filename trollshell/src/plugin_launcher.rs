//! Declarative widget-plugin **launcher** (#419): the host launches enabled
//! plugins as **transient systemd user units** via `systemd-run --user`.
//!
//! ## Model
//!
//! - **nix declares.** The `programs.trollshell.plugins` option (#350) no
//!   longer emits one static `trollshell-plugin-<id>` unit per entry — the
//!   home-manager / NixOS modules render it to a small JSON state file
//!   ([`STATE_FILE_REL`] under `$XDG_CONFIG_HOME`, then each entry of
//!   `$XDG_CONFIG_DIRS`) saying which plugins exist, how to exec them, and
//!   whether they're enabled.
//! - **the host launches.** At startup ([`launch_at_startup`]) every enabled,
//!   not-already-running plugin is spawned as a *transient* user unit
//!   (`systemd-run --user --unit=trollshell-plugin-<id>.service … <exec>`); the
//!   control-center's Plugins tab (#348) start goes through the same path
//!   ([`start`]).
//! - **systemd owns runtime state** (the system-daemon-as-state-store rule):
//!   crash supervision (`Restart=on-failure`), lifetime
//!   (`PartOf=graphical-session.target` — a plugin survives a *shell* restart
//!   but dies with the session), and stop. The shell keeps no runtime plugin
//!   state; a restarted shell simply finds the previous run's units still
//!   active and skips them.
//!
//! ## Secret-injection hook (#392)
//!
//! [`launch`] takes `extra_env`, appended after the spec's declared `env` as
//! additional `--setenv=<VAR>=<value>` arguments. That is the hook #392 (AI
//! API-key management) rides on: the control-center writes a key to
//! gnome-keyring/libsecret, and the launcher will read the slot and pass it
//! here at spawn — the secret never lands in the state file, a unit file, or
//! the plugin's own config, and rotating a key is just stop + relaunch. Today
//! every caller passes `&[]`; the key *management* itself is out of scope here.
//!
//! ## Legacy static units
//!
//! Hand-installed static units (`etc/systemd/user/trollshell-plugin-*.service`,
//! the pre-#419 path) keep working unchanged: the transport (`plugins.rs`)
//! doesn't care who spawned a plugin, and the control-surface fns here fall
//! back to plain `StartUnit` / unit-file enablement for any id that isn't in
//! the declarative state file.
//!
//! ## Why the `systemd-run` CLI, not D-Bus `StartTransientUnit`
//!
//! Per the #419/#392 thread's letter. The CLI does the transient-unit property
//! marshaling (`ExecStart=a(sasb)`, env, restart props) and `$PATH` resolution
//! for us, at the cost of one short-lived subprocess per launch — a handful per
//! session. `StartTransientUnit` over `hytte-bus` would avoid the subprocess
//! but re-implement that marshaling by hand; revisit only if the subprocess
//! ever becomes a problem.
//!
//! Everything here is plain `async fn` off the tokio side (no GTK, no
//! registry): the `Control` D-Bus handlers (`control.rs`) `.await` these
//! directly on the D-Bus task, and the startup launch runs on the shared
//! runtime.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use hytte::services::systemd;
use serde::Deserialize;

/// Relative path of the declarative state file under each XDG config root.
/// Written by the nix modules (`nix/hm-module.nix` → `$XDG_CONFIG_HOME`,
/// `nix/nixos-module.nix` → `/etc/xdg`); absent = no declared plugins (the
/// launcher stays inert and any static units keep working).
const STATE_FILE_REL: &str = "trollshell/plugins.json";

// ── State file model ─────────────────────────────────────────────────────────

/// One declared plugin's launch spec, as read from the state file.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct PluginSpec {
    /// The plugin binary to exec (an absolute nix store path in practice;
    /// `systemd-run` resolves a bare name against `$PATH`).
    exec: String,
    /// Declared environment for the plugin process (the config idiom the
    /// bundled plugins use — `PET_NAME`, …). Passed as `--setenv=K=V`.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// AI-key **slots** this plugin opts into (#392) — provider names (e.g.
    /// `"openrouter"`). At launch each slot's key is read from the login keyring
    /// ([`crate::secrets`]) and injected as `--setenv=<SLOT>_API_KEY=<value>`
    /// (see [`crate::secrets::env_var_for`]); a slot with no stored key is
    /// skipped (the plugin runs keyless). A plugin that doesn't list a slot
    /// never gets that key in its environment.
    #[serde(default)]
    secrets: Vec<String>,
    /// Whether the host launches this plugin at startup. A disabled plugin is
    /// still *declared* — it lists in the control-center and can be started
    /// manually — it just doesn't auto-launch.
    #[serde(default = "default_enabled")]
    enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// The state file's top level. `version` is written (`1`) but deliberately not
/// interpreted yet; unknown fields are ignored, so additive evolution doesn't
/// break older shells.
#[derive(Debug, Default, Deserialize)]
struct PluginState {
    #[serde(default)]
    plugins: BTreeMap<String, PluginSpec>,
}

/// Parse the state file's JSON. Pure; sanitization (id/charset checks) is
/// [`sanitize`]'s job so both are unit-testable apart.
fn parse_state(json: &str) -> Result<BTreeMap<String, PluginSpec>, serde_json::Error> {
    serde_json::from_str::<PluginState>(json).map(|s| s.plugins)
}

/// Drop entries the launcher must not act on: an id that fails the
/// `trollshell-plugin-<id>.service` template's charset guard, an empty `exec`,
/// or an env key that would corrupt a `--setenv=K=V` argument. Each drop is
/// logged loudly — a nix-written file should never trip these, so a trip means
/// the file was edited by hand.
fn sanitize(plugins: BTreeMap<String, PluginSpec>) -> BTreeMap<String, PluginSpec> {
    plugins
        .into_iter()
        .filter(|(id, spec)| {
            if !systemd::is_valid_plugin_id(id) {
                tracing::warn!(plugin = %id, "plugins.json: invalid plugin id; entry ignored");
                return false;
            }
            if spec.exec.is_empty() {
                tracing::warn!(plugin = %id, "plugins.json: empty exec; entry ignored");
                return false;
            }
            true
        })
        .map(|(id, mut spec)| {
            spec.env.retain(|k, _| {
                let ok = !k.is_empty() && !k.contains('=');
                if !ok {
                    tracing::warn!(plugin = %id, key = %k, "plugins.json: invalid env key; dropped");
                }
                ok
            });
            // A secret slot must map to a valid `<SLOT>_API_KEY` env-var name
            // (#392); drop any that wouldn't (a nix-written file shouldn't).
            spec.secrets.retain(|slot| {
                let ok = crate::secrets::is_valid_slot(slot);
                if !ok {
                    tracing::warn!(plugin = %id, %slot, "plugins.json: invalid secret slot; dropped");
                }
                ok
            });
            (id, spec)
        })
        .collect()
}

/// The candidate state-file paths in XDG precedence order:
/// `$XDG_CONFIG_HOME` (defaulting to `~/.config`), then each entry of
/// `$XDG_CONFIG_DIRS` (defaulting to `/etc/xdg`). First *existing* file wins
/// whole — per-file, not merged — so a home-manager per-user file fully
/// shadows a NixOS system one. Pure (env passed in) for testability.
fn candidate_paths(
    config_home: Option<&str>,
    home: Option<&str>,
    config_dirs: Option<&str>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    match config_home.filter(|v| !v.is_empty()) {
        Some(dir) => out.push(PathBuf::from(dir).join(STATE_FILE_REL)),
        None => {
            if let Some(home) = home.filter(|v| !v.is_empty()) {
                out.push(PathBuf::from(home).join(".config").join(STATE_FILE_REL));
            }
        }
    }
    let dirs = config_dirs.filter(|v| !v.is_empty()).unwrap_or("/etc/xdg");
    for dir in dirs.split(':').filter(|d| !d.is_empty()) {
        out.push(PathBuf::from(dir).join(STATE_FILE_REL));
    }
    out
}

/// Load + parse + sanitize the declarative plugin state. Missing file = no
/// declared plugins (inert, not an error). A present-but-broken file logs a
/// warning and yields empty rather than falling through to a lower-precedence
/// file — masking a broken user file with a system one would be quiet drift.
async fn load_declared() -> BTreeMap<String, PluginSpec> {
    let paths = candidate_paths(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        std::env::var("XDG_CONFIG_DIRS").ok().as_deref(),
    );
    for path in paths {
        match tokio::fs::read_to_string(&path).await {
            Ok(json) => match parse_state(&json) {
                Ok(plugins) => return sanitize(plugins),
                Err(err) => {
                    tracing::warn!(path = %path.display(), %err, "plugins.json unparsable; treating as empty");
                    return BTreeMap::new();
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "plugins.json unreadable; treating as empty");
                return BTreeMap::new();
            }
        }
    }
    BTreeMap::new()
}

// ── systemd-run launch ───────────────────────────────────────────────────────

/// The full `systemd-run` argv (sans the program itself) for one plugin
/// launch. Pure, so the exact invocation is pinned by unit tests:
///
/// - `--collect`: release the unit even if it ends failed, so a crash-looped
///   plugin doesn't wedge its unit name (a relaunch would otherwise need a
///   `reset-failed` first).
/// - `Restart=on-failure` / `RestartSec=2`: same supervision the static units
///   carried — supervision stays systemd's job.
/// - `PartOf=graphical-session.target`: stop propagates from session teardown,
///   so plugins die with the session but survive a shell restart.
/// - `extra_env` comes **after** the spec's env, so an injected secret (#392)
///   wins over a stale value declared in the spec.
/// - `--` terminates option parsing before the config-supplied exec path.
fn systemd_run_args(id: &str, spec: &PluginSpec, extra_env: &[(String, String)]) -> Vec<String> {
    let mut args = vec![
        "--user".to_owned(),
        "--quiet".to_owned(),
        "--collect".to_owned(),
        format!("--unit={}", systemd::plugin_unit_name(id)),
        format!("--description=trollshell plugin: {id}"),
        "--property=Restart=on-failure".to_owned(),
        "--property=RestartSec=2".to_owned(),
        "--property=PartOf=graphical-session.target".to_owned(),
    ];
    for (k, v) in &spec.env {
        args.push(format!("--setenv={k}={v}"));
    }
    for (k, v) in extra_env {
        args.push(format!("--setenv={k}={v}"));
    }
    args.push("--".to_owned());
    args.push(spec.exec.clone());
    args
}

/// Launch one declared plugin as a transient `trollshell-plugin-<id>.service`
/// user unit. `extra_env` is the #392 secret-injection hook (see the module
/// docs); every current caller passes `&[]`.
///
/// Fails if the unit already exists (the plugin is running — `systemd-run`
/// refuses to replace a live unit) or if there's no reachable user manager;
/// both surface as one logged warning at the call sites, never a crash.
async fn launch(id: &str, spec: &PluginSpec, extra_env: &[(String, String)]) -> anyhow::Result<()> {
    anyhow::ensure!(systemd::is_valid_plugin_id(id), "invalid plugin id: {id:?}");
    let output = tokio::process::Command::new("systemd-run")
        .args(systemd_run_args(id, spec, extra_env))
        .output()
        .await
        .context("spawning systemd-run --user")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "systemd-run --user failed for plugin {id} ({}): {}",
            output.status,
            stderr.trim()
        );
    }
    tracing::info!(plugin = %id, exec = %spec.exec, "launched plugin as transient user unit");
    Ok(())
}

/// Read every AI-key slot the plugin opted into ([`PluginSpec::secrets`], #392)
/// from the login keyring and map each to its injected `(<SLOT>_API_KEY, value)`
/// env pair. A slot with no stored key is skipped — the plugin launches keyless
/// and its own fallback (e.g. the pet's canned lines) applies. Secret values are
/// never logged (only the slot, and only on the skip path).
async fn resolve_secret_env(spec: &PluginSpec) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(spec.secrets.len());
    for slot in &spec.secrets {
        if let Some(value) = crate::secrets::get(slot).await {
            out.push((crate::secrets::env_var_for(slot), value));
        } else {
            tracing::debug!(slot = %slot, "no stored AI key for slot; launching plugin without it");
        }
    }
    out
}

/// Whether a systemd `ActiveState` means the unit is already running (or on
/// its way), i.e. a startup launch should skip it.
fn is_running(active_state: &str) -> bool {
    matches!(active_state, "active" | "activating" | "reloading")
}

/// Launch every enabled declared plugin that isn't already running. Idempotent
/// across shell restarts by construction: the transient units of a previous
/// run are systemd's state, so they show as running and are skipped.
async fn launch_enabled() {
    let declared = load_declared().await;
    if declared.is_empty() {
        tracing::debug!("no declared plugins (no plugins.json); launcher inert");
        return;
    }
    // One list call up front beats racing systemd-run's "unit already exists"
    // error per plugin — and keeps the skip loggable as the normal case it is.
    let running: HashSet<String> = match systemd::list_plugin_units().await {
        Ok(units) => units
            .into_iter()
            .filter(|u| is_running(&u.active_state))
            .map(|u| u.id)
            .collect(),
        Err(err) => {
            tracing::warn!(%err, "listing plugin units failed; launching blind");
            HashSet::new()
        }
    };
    for (id, spec) in &declared {
        if !spec.enabled {
            tracing::debug!(plugin = %id, "declared disabled; not launching");
            continue;
        }
        if running.contains(id) {
            tracing::info!(plugin = %id, "already running (systemd owns it); skipping launch");
            continue;
        }
        let extra_env = resolve_secret_env(spec).await;
        if let Err(err) = launch(id, spec, &extra_env).await {
            tracing::warn!(plugin = %id, %err, "plugin launch failed");
        }
    }
}

/// Kick off the startup launch on the shared tokio runtime. Called once from
/// `main.rs`'s run body; guarded so a re-fired `activate` (a second `trollshell`
/// invocation remote-activating the primary instance) can't double-launch.
pub fn launch_at_startup() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    hytte::reactive::runtime::handle().spawn(launch_enabled());
}

// ── Control surface (the #348 Plugins tab, via control.rs) ───────────────────

/// The plugins the control-center lists: the *declared* set (state file) ∪ the
/// `trollshell-plugin-*` units systemd knows (transient runs + legacy static
/// units). For a declared plugin the declarative `enabled` flag wins — a
/// transient unit has no unit file, so systemd would report it `disabled` —
/// and a declared-but-stopped plugin still lists as `inactive` (a stopped
/// `--collect` transient unit vanishes from systemd entirely). Pure merge in
/// [`merge_declared`].
pub async fn list() -> Vec<systemd::PluginUnit> {
    let declared = load_declared().await;
    let units = match systemd::list_plugin_units().await {
        Ok(units) => units,
        Err(err) => {
            tracing::warn!(%err, "listing plugin units failed");
            Vec::new()
        }
    };
    merge_declared(units, &declared)
}

/// Overlay the declared set onto systemd's unit list — see [`list`]. Pure.
fn merge_declared(
    mut units: Vec<systemd::PluginUnit>,
    declared: &BTreeMap<String, PluginSpec>,
) -> Vec<systemd::PluginUnit> {
    for (id, spec) in declared {
        if let Some(unit) = units.iter_mut().find(|u| &u.id == id) {
            unit.enabled = spec.enabled;
        } else {
            units.push(systemd::PluginUnit {
                id: id.clone(),
                active_state: "inactive".to_owned(),
                enabled: spec.enabled,
            });
        }
    }
    units.sort_by(|a, b| a.id.cmp(&b.id));
    units
}

/// Start plugin `id` now: a declared plugin is (re)launched as a transient
/// unit ([`launch`] — `--collect` already released any failed previous run);
/// an undeclared id falls back to `StartUnit` for a legacy static unit.
///
/// # Errors
/// Unknown/invalid id, a still-running unit, or an unreachable user manager.
pub async fn start(id: &str) -> anyhow::Result<()> {
    let declared = load_declared().await;
    match declared.get(id) {
        Some(spec) => {
            let extra_env = resolve_secret_env(spec).await;
            launch(id, spec, &extra_env).await
        }
        None => systemd::start_plugin(id).await,
    }
}

/// Stop plugin `id`'s unit now (`StopUnit` — works for transient and static
/// units alike; a stopped `--collect` transient unit is then released).
///
/// # Errors
/// Invalid id, no such unit, or an unreachable user manager.
pub async fn stop(id: &str) -> anyhow::Result<()> {
    systemd::stop_plugin(id).await
}

/// Persist plugin `id`'s auto-start state. For a **declared** plugin
/// enablement is *declarative* — nix owns it (#419), so a runtime toggle is a
/// logged no-op (flip `programs.trollshell.plugins.<id>.enable` to persist;
/// the tab's live start/stop still applies the runtime half). An undeclared id
/// falls back to unit-file `Enable/DisableUnitFiles` for legacy static units.
///
/// # Errors
/// Invalid id or an unreachable user manager (legacy path only).
pub async fn set_enabled(id: &str, enabled: bool) -> anyhow::Result<()> {
    let declared = load_declared().await;
    if declared.contains_key(id) {
        tracing::info!(
            plugin = %id,
            enabled,
            "enablement is declarative (nix-managed, #419); runtime toggle not persisted"
        );
        return Ok(());
    }
    systemd::set_plugin_enabled(id, enabled).await
}

// ── Secret rotation (#392): relaunch to re-inject a changed key ───────────────

/// Relaunch every **running** declared plugin whose `secrets` allowlist
/// includes `slot`, so a just-changed key (set or cleared in the control-center)
/// takes effect — rotation is stop + relaunch, re-reading the slot from the
/// keyring. Called from the `SetAiKey`/`ClearAiKey` control handlers after the
/// keyring write.
///
/// Stopped plugins and legacy static units are left alone: a stopped plugin
/// re-reads the key on its next start, and a static unit gets no injection at
/// all. Best-effort — each plugin's failure is logged, never propagated.
pub async fn relaunch_for_secret(slot: &str) {
    let declared = load_declared().await;
    let affected: Vec<(&String, &PluginSpec)> = declared
        .iter()
        .filter(|(_, spec)| spec.secrets.iter().any(|s| s == slot))
        .collect();
    if affected.is_empty() {
        tracing::debug!(%slot, "no declared plugin uses this secret slot; nothing to relaunch");
        return;
    }
    let running: HashSet<String> = match systemd::list_plugin_units().await {
        Ok(units) => units
            .into_iter()
            .filter(|u| is_running(&u.active_state))
            .map(|u| u.id)
            .collect(),
        Err(err) => {
            tracing::warn!(%err, %slot, "listing plugin units for relaunch failed; skipping");
            return;
        }
    };
    for (id, spec) in affected {
        if !running.contains(id) {
            tracing::debug!(plugin = %id, %slot, "not running; new key applies on next start");
            continue;
        }
        if let Err(err) = restart(id, spec).await {
            tracing::warn!(plugin = %id, %slot, %err, "relaunch after key change failed");
        } else {
            tracing::info!(plugin = %id, %slot, "relaunched to apply the changed AI key");
        }
    }
}

/// Stop a declared plugin's transient unit, wait for it to actually go down (so
/// its `--collect` unit name frees up), then relaunch it with freshly resolved
/// secret env. `systemd-run` refuses to replace a live unit, hence the
/// wait-until-stopped rather than a bare stop→launch.
async fn restart(id: &str, spec: &PluginSpec) -> anyhow::Result<()> {
    stop(id).await?;
    wait_until_stopped(id).await;
    let extra_env = resolve_secret_env(spec).await;
    launch(id, spec, &extra_env).await
}

/// Poll the plugin's unit until it is no longer running (inactive/failed, or
/// gone — a collected transient unit vanishes), bounded to ~5s so a stuck stop
/// can't wedge the relaunch. On a list error we return early and let the launch
/// attempt surface any "still exists" error itself.
async fn wait_until_stopped(id: &str) {
    for _ in 0..25 {
        match systemd::list_plugin_units().await {
            Ok(units) => {
                if !units
                    .iter()
                    .any(|u| u.id == id && is_running(&u.active_state))
                {
                    return;
                }
            }
            Err(_) => return,
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    tracing::warn!(plugin = %id, "unit still running after stop; relaunch may fail");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(exec: &str, enabled: bool) -> PluginSpec {
        PluginSpec {
            exec: exec.to_owned(),
            env: BTreeMap::new(),
            secrets: Vec::new(),
            enabled,
        }
    }

    // ── state file parsing ───────────────────────────────────────────────────

    #[test]
    fn parse_full_state_file() {
        let json = r#"{
            "version": 1,
            "plugins": {
                "pet": {
                    "exec": "/nix/store/abc/bin/hytte-plugin-pet",
                    "env": { "PET_NAME": "nisse" },
                    "enabled": true
                },
                "weather": { "exec": "/bin/weather", "enabled": false }
            }
        }"#;
        let plugins = parse_state(json).unwrap();
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins["pet"].exec, "/nix/store/abc/bin/hytte-plugin-pet");
        assert_eq!(plugins["pet"].env["PET_NAME"], "nisse");
        assert!(plugins["pet"].enabled);
        // env defaults empty; explicit enabled=false honored.
        assert!(plugins["weather"].env.is_empty());
        assert!(!plugins["weather"].enabled);
    }

    #[test]
    fn parse_reads_secret_slots_defaulting_empty() {
        let json = r#"{
            "plugins": {
                "pet": {
                    "exec": "/bin/pet",
                    "secrets": ["openrouter"]
                },
                "timer": { "exec": "/bin/timer" }
            }
        }"#;
        let plugins = parse_state(json).unwrap();
        assert_eq!(plugins["pet"].secrets, vec!["openrouter".to_owned()]);
        // A plugin that declares no secrets defaults to none injected.
        assert!(plugins["timer"].secrets.is_empty());
    }

    #[test]
    fn sanitize_drops_invalid_secret_slots() {
        let mut plugins = BTreeMap::new();
        let mut s = spec("/bin/ok", true);
        s.secrets = vec![
            "openrouter".to_owned(),
            "Bad Slot".to_owned(), // space / uppercase
            "bad=slot".to_owned(), // '=' would corrupt --setenv
            "9live".to_owned(),    // leading digit
        ];
        plugins.insert("p".to_owned(), s);
        let out = sanitize(plugins);
        assert_eq!(out["p"].secrets, vec!["openrouter".to_owned()]);
    }

    #[test]
    fn parse_defaults_enabled_true_and_ignores_unknown_fields() {
        // A minimal entry: only exec. `enabled` defaults true, unknown fields
        // (future schema additions) are ignored rather than erroring.
        let json = r#"{ "plugins": { "demo": { "exec": "/bin/demo", "future": 42 } } }"#;
        let plugins = parse_state(json).unwrap();
        assert!(plugins["demo"].enabled);
    }

    #[test]
    fn parse_empty_or_missing_plugins_key() {
        assert!(parse_state("{}").unwrap().is_empty());
        assert!(parse_state(r#"{ "version": 1 }"#).unwrap().is_empty());
        // Garbage is an error (the caller logs + treats as empty).
        assert!(parse_state("not json").is_err());
    }

    #[test]
    fn sanitize_drops_invalid_ids_and_empty_exec() {
        let mut plugins = BTreeMap::new();
        plugins.insert("ok-plugin".to_owned(), spec("/bin/ok", true));
        // An id that would escape the unit-name template.
        plugins.insert("../evil".to_owned(), spec("/bin/evil", true));
        plugins.insert("noexec".to_owned(), spec("", true));
        let out = sanitize(plugins);
        assert_eq!(out.keys().collect::<Vec<_>>(), vec!["ok-plugin"]);
    }

    #[test]
    fn sanitize_drops_env_keys_that_break_setenv() {
        let mut plugins = BTreeMap::new();
        let mut s = spec("/bin/ok", true);
        s.env.insert("GOOD".to_owned(), "v".to_owned());
        s.env.insert("BAD=KEY".to_owned(), "v".to_owned());
        s.env.insert(String::new(), "v".to_owned());
        plugins.insert("p".to_owned(), s);
        let out = sanitize(plugins);
        assert_eq!(out["p"].env.keys().collect::<Vec<_>>(), vec!["GOOD"]);
    }

    // ── XDG path resolution ──────────────────────────────────────────────────

    #[test]
    fn candidate_paths_prefer_config_home_then_config_dirs() {
        let paths = candidate_paths(Some("/home/a/.config"), Some("/home/a"), Some("/etc/xdg"));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/a/.config/trollshell/plugins.json"),
                PathBuf::from("/etc/xdg/trollshell/plugins.json"),
            ]
        );
    }

    #[test]
    fn candidate_paths_fall_back_to_home_and_default_dirs() {
        // No XDG_CONFIG_HOME → ~/.config; no XDG_CONFIG_DIRS → /etc/xdg.
        let paths = candidate_paths(None, Some("/home/a"), None);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/a/.config/trollshell/plugins.json"),
                PathBuf::from("/etc/xdg/trollshell/plugins.json"),
            ]
        );
        // Empty strings count as unset; multiple config dirs split on ':'.
        let paths = candidate_paths(Some(""), None, Some("/a::/b"));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/a/trollshell/plugins.json"),
                PathBuf::from("/b/trollshell/plugins.json"),
            ]
        );
    }

    // ── systemd-run argv ─────────────────────────────────────────────────────

    #[test]
    fn systemd_run_args_pin_the_invocation() {
        let mut s = spec("/nix/store/abc/bin/demo", true);
        s.env.insert("A".to_owned(), "1".to_owned());
        s.env.insert("B".to_owned(), "x=y".to_owned());
        let args = systemd_run_args(
            "demo",
            &s,
            &[("PLUGIN_API_KEY".to_owned(), "s3cret".to_owned())],
        );
        assert_eq!(
            args,
            vec![
                "--user",
                "--quiet",
                "--collect",
                "--unit=trollshell-plugin-demo.service",
                "--description=trollshell plugin: demo",
                "--property=Restart=on-failure",
                "--property=RestartSec=2",
                "--property=PartOf=graphical-session.target",
                "--setenv=A=1",
                // '=' in a *value* is fine (systemd splits on the first '=').
                "--setenv=B=x=y",
                // #392 hook: extra_env rides after the declared env.
                "--setenv=PLUGIN_API_KEY=s3cret",
                "--",
                "/nix/store/abc/bin/demo",
            ]
        );
    }

    // ── merge + running states ───────────────────────────────────────────────

    fn unit(id: &str, active: &str, enabled: bool) -> systemd::PluginUnit {
        systemd::PluginUnit {
            id: id.to_owned(),
            active_state: active.to_owned(),
            enabled,
        }
    }

    #[test]
    fn merge_declared_overlays_enablement_and_adds_stopped_plugins() {
        let mut declared = BTreeMap::new();
        // Running transient unit: systemd sees no unit file → enabled=false;
        // the declarative flag must win.
        declared.insert("pet".to_owned(), spec("/bin/pet", true));
        // Declared but stopped: a collected transient unit is gone from
        // systemd entirely — it must still list, inactive.
        declared.insert("weather".to_owned(), spec("/bin/weather", false));
        let units = vec![
            unit("pet", "active", false),
            // Legacy static unit, not declared: passes through untouched.
            unit("timer", "inactive", true),
        ];
        let out = merge_declared(units, &declared);
        assert_eq!(
            out,
            vec![
                unit("pet", "active", true),
                unit("timer", "inactive", true),
                unit("weather", "inactive", false),
            ]
        );
    }

    #[test]
    fn is_running_matches_live_states_only() {
        assert!(is_running("active"));
        assert!(is_running("activating"));
        assert!(is_running("reloading"));
        assert!(!is_running("inactive"));
        assert!(!is_running("failed"));
        assert!(!is_running("deactivating"));
    }
}
