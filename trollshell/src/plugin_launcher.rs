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
//!   state.
//!
//! ## Reconcile, not skip-if-running (#695)
//!
//! The launch step is a **convergence** ([`reconcile`]), not a one-shot spawn.
//! #419 shipped "idempotent = skip any plugin whose unit is already active",
//! which is only correct while the declared spec never changes — and it does:
//! a `home-manager switch` that edits `env`, points `package` at a fresh build,
//! flips `enable`, or adds/removes a plugin rewrites `plugins.json` and cannot
//! touch the *transient* unit that baked the old values in at spawn (there is
//! no unit file for activation to diff, and on the NixOS side activation runs
//! as root with no user bus at all). The result was a plugin running a
//! configuration — and a store path — the user no longer had declared, silently,
//! for as long as the session lasted (#695).
//!
//! So each launched unit carries a **spec fingerprint** in its `Description=`
//! ([`unit_description`]), systemd hands it back in the unit list it already
//! fetches, and [`reconcile`] diffs the live units against the freshly read
//! state file ([`plan`], pure):
//!
//! | declared | live unit                    | action    |
//! |----------|------------------------------|-----------|
//! | enabled  | not running                  | launch    |
//! | enabled  | running, fingerprint matches  | leave     |
//! | enabled  | running, fingerprint differs  | restart   |
//! | disabled | running                      | stop      |
//! | absent   | running, launcher-stamped    | stop      |
//! | absent   | running, no fingerprint      | leave     |
//!
//! That last pair is the legacy-static-unit guard: a unit this launcher never
//! spawned carries no fingerprint, so reconcile never touches it.
//!
//! [`reconcile`] runs at shell startup (so a `systemctl --user restart
//! trollshell` applies current config — the only path that helps NixOS-module
//! users, whose activation can't reach the user bus) and on demand via the
//! `Control.ReloadPlugins` D-Bus method, which the home-manager module calls
//! from its activation script so a switch fully applies live.
//!
//! ## Secret injection (#392)
//!
//! [`launch`] takes `extra_env`, appended after the spec's declared `env` as
//! additional `--setenv=<VAR>=<value>` arguments. That is the hook #392 (AI
//! API-key management) rides on: [`resolve_secret_env`] reads each slot in
//! [`PluginSpec::secrets`] from the login keyring (via [`crate::secrets`])
//! and maps it to its injected `(<SLOT>_API_KEY, value)` pair; every
//! `launch` call site builds `extra_env` this way before calling in — the
//! secret never lands in the state file, a unit file, or the plugin's own
//! config, and rotating a key is just stop + relaunch. Key *management*
//! (writing/rotating/deleting the stored key itself) is [`crate::secrets`]
//! and the control-center's AI Keys tab, not this module.
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

/// Load + parse + sanitize the declarative plugin state. Missing file = **no
/// declared plugins** (inert, not an error) — `Some(empty)`, which is a real
/// answer: it says every declarative plugin was removed from the config.
///
/// `None` means "a state file exists but we couldn't read or parse it" —
/// deliberately *not* the same as "nothing is declared", because [`reconcile`]
/// stops plugins that are no longer declared and a typo'd file must never be
/// read as "stop everything". A broken file also stops the search rather than
/// falling through to a lower-precedence one: masking a broken user file with a
/// system one would be quiet drift.
async fn load_declared() -> Option<BTreeMap<String, PluginSpec>> {
    let paths = candidate_paths(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        std::env::var("XDG_CONFIG_DIRS").ok().as_deref(),
    );
    for path in paths {
        match tokio::fs::read_to_string(&path).await {
            Ok(json) => match parse_state(&json) {
                Ok(plugins) => return Some(sanitize(plugins)),
                Err(err) => {
                    tracing::warn!(path = %path.display(), %err, "plugins.json unparsable; leaving plugins as they are");
                    return None;
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "plugins.json unreadable; leaving plugins as they are");
                return None;
            }
        }
    }
    Some(BTreeMap::new())
}

// ── Spec fingerprint (#695) ──────────────────────────────────────────────────

/// Opening delimiter of the spec fingerprint inside a launched unit's
/// `Description=` — see [`unit_description`].
const FP_OPEN: &str = "[cfg:";
/// Closing delimiter of the spec fingerprint.
const FP_CLOSE: char = ']';

/// FNV-1a 64 offset basis / prime. A **pinned, hand-rolled** hash on purpose:
/// the digest is written into a unit's description by one shell process and read
/// back by another (possibly a different build), so it must be stable across
/// rustc versions — which `std`'s `DefaultHasher` explicitly does not promise.
/// Not a security primitive: it only has to change when the spec changes.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 64 over `bytes`, chained from `hash`.
fn fnv1a(bytes: &[u8], hash: u64) -> u64 {
    bytes
        .iter()
        .fold(hash, |h, b| (h ^ u64::from(*b)).wrapping_mul(FNV_PRIME))
}

/// A short stable digest of everything about a spec that a *running* unit baked
/// in at spawn: the `exec` path (so a rebuilt `package` shows up), the declared
/// `env` (`BTreeMap`, so iteration order is the sort order — a map with the same
/// pairs always digests the same), and the `secrets` slot list (adding or
/// dropping a slot changes which key is injected).
///
/// Deliberately **not** covered:
/// - `enabled` — flipping it stops or starts the plugin, it never restarts one.
/// - the secret *values* — those come from the keyring, not the state file;
///   rotation already has its own precise path ([`relaunch_for_secret`], #392),
///   and keeping values out means a reconcile costs zero keyring reads for
///   plugins it isn't going to touch. (A key changed *outside* the control-center
///   while the shell is down therefore doesn't trigger a restart; use the
///   control-center, or stop/start the plugin.)
///
/// Pure, so the fingerprint contract is unit-testable.
fn spec_fingerprint(spec: &PluginSpec) -> String {
    // ASCII record (0x1e) / unit (0x1f) / group (0x1d) separators between the
    // parts, so `{"AB": "C"}` can't digest the same as `{"A": "BC"}`. Only a
    // value that itself contains one of those control bytes could re-introduce
    // an ambiguity, and a nix-rendered exec path / env value never does.
    let mut h = fnv1a(spec.exec.as_bytes(), FNV_OFFSET);
    for (k, v) in &spec.env {
        h = fnv1a(b"\x1e", h);
        h = fnv1a(k.as_bytes(), h);
        h = fnv1a(b"\x1f", h);
        h = fnv1a(v.as_bytes(), h);
    }
    for slot in &spec.secrets {
        h = fnv1a(b"\x1d", h);
        h = fnv1a(slot.as_bytes(), h);
    }
    format!("{h:016x}")
}

/// The `Description=` a launched plugin unit carries: a human-readable label
/// plus the spec fingerprint, in a form [`parse_fingerprint`] reads back.
///
/// Riding in the description is what makes the reconcile diff free — systemd
/// returns it as the second field of the unit list [`systemd::list_plugin_units`]
/// already fetches, so no extra property get, per plugin, per reconcile.
fn unit_description(id: &str, fingerprint: &str) -> String {
    format!("trollshell plugin: {id} {FP_OPEN}{fingerprint}{FP_CLOSE}")
}

/// The spec fingerprint stamped into a unit's `Description=`, or `None` for a
/// description this launcher didn't write — a legacy static unit, or a
/// transient unit from a pre-#695 shell. Inverse of [`unit_description`]. Pure.
fn parse_fingerprint(description: &str) -> Option<&str> {
    let start = description.rfind(FP_OPEN)? + FP_OPEN.len();
    description[start..].strip_suffix(FP_CLOSE)
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
/// - `--description=` carries the spec fingerprint (#695) so a later
///   [`reconcile`] can tell this unit's spec from the currently declared one.
/// - `--` terminates option parsing before the config-supplied exec path.
fn systemd_run_args(id: &str, spec: &PluginSpec, extra_env: &[(String, String)]) -> Vec<String> {
    let mut args = vec![
        "--user".to_owned(),
        "--quiet".to_owned(),
        "--collect".to_owned(),
        format!("--unit={}", systemd::plugin_unit_name(id)),
        format!(
            "--description={}",
            unit_description(id, &spec_fingerprint(spec))
        ),
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
/// docs); every current caller builds it via [`resolve_secret_env`].
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

// ── Reconcile (#695) ─────────────────────────────────────────────────────────

/// What [`reconcile`] decided to do about one plugin. Ordered as executed —
/// stops first, so a disabled/removed plugin releases its unit name before
/// anything else runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Action {
    /// Running but no longer wanted (declared `enable = false`, or dropped from
    /// the config entirely) → stop it.
    Stop,
    /// Running from a *different* spec than the one now declared (`env`,
    /// `package`/`exec` or `secrets` changed — or the unit predates the
    /// fingerprint) → stop, wait for it to go down, relaunch from the new spec.
    Restart,
    /// Declared enabled and not running → launch it now.
    Launch,
}

/// Diff the declared state against the live units: the whole of the reconcile
/// decision, pure and systemd-free so every branch is unit-testable.
///
/// `declared` is the freshly read state file; `units` is
/// [`systemd::list_plugin_units`]'s answer. Output is sorted (stops before
/// restarts before launches, then by id) so execution order is deterministic.
///
/// Two deliberate asymmetries:
/// - A **running unit with no fingerprint** whose id *is* declared is restarted
///   (we can't prove it matches, and converging is the point) — this is the
///   one-time recycle when a pre-#695 shell's units meet a #695 shell. An id
///   that is both declared *and* hand-installed as a static unit lands here on
///   every reconcile; [`restart`] documents what that does.
/// - A **running unit with no fingerprint** whose id is *not* declared is left
///   strictly alone: that is a legacy static unit (or someone else's), and the
///   launcher has never owned it.
fn plan(
    declared: &BTreeMap<String, PluginSpec>,
    units: &[systemd::PluginUnit],
) -> Vec<(String, Action)> {
    let running: BTreeMap<&str, Option<&str>> = units
        .iter()
        .filter(|u| is_running(&u.active_state))
        .map(|u| (u.id.as_str(), parse_fingerprint(&u.description)))
        .collect();
    let mut out: Vec<(String, Action)> = Vec::new();
    for (id, spec) in declared {
        match (spec.enabled, running.get(id.as_str())) {
            (true, None) => out.push((id.clone(), Action::Launch)),
            (true, Some(live)) => {
                if *live != Some(spec_fingerprint(spec).as_str()) {
                    out.push((id.clone(), Action::Restart));
                }
            }
            (false, Some(_)) => out.push((id.clone(), Action::Stop)),
            (false, None) => {}
        }
    }
    // Orphans: units this launcher stamped (so it owns them) whose plugin is no
    // longer declared at all — the `plugins.<id>` entry was removed.
    for (id, fingerprint) in &running {
        if fingerprint.is_some() && !declared.contains_key(*id) {
            out.push(((*id).to_owned(), Action::Stop));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Converge the running plugins onto the declared state (#695): launch what
/// should be running and isn't, stop what shouldn't be, and restart anything
/// running from a superseded spec (changed `env` / `package` / `secrets`).
/// Called at shell startup ([`launch_at_startup`]) and on demand from the
/// `Control.ReloadPlugins` handler, which the home-manager activation script
/// pokes after rewriting `plugins.json`.
///
/// Best-effort throughout: every per-plugin failure is logged, never propagated
/// — one broken plugin must not stop the rest from converging. Serialized on a
/// process-wide lock, so a reconcile racing another (activation firing twice,
/// or landing while startup is still running) queues instead of interleaving a
/// stop with the other's launch; the second then re-reads the state file and
/// converges on whatever is current.
pub async fn reconcile() {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LOCK.lock().await;

    let Some(declared) = load_declared().await else {
        // Unreadable/unparsable state file — leave the running set alone.
        return;
    };
    // One list call up front beats racing systemd-run's "unit already exists"
    // error per plugin — and it carries the fingerprints the diff runs on.
    let units = match systemd::list_plugin_units().await {
        Ok(units) => units,
        Err(err) => {
            // No reachable user manager: fall through with an empty live set,
            // which can only ever plan launches (each of which surfaces its own
            // error) — never a stop of something we failed to see.
            tracing::warn!(%err, "listing plugin units failed; launching blind");
            Vec::new()
        }
    };
    let actions = plan(&declared, &units);
    if actions.is_empty() {
        tracing::debug!(
            declared = declared.len(),
            "plugins already match the declared state"
        );
        return;
    }
    for (id, action) in actions {
        match action {
            Action::Stop => {
                tracing::info!(plugin = %id, "no longer declared as enabled; stopping");
                if let Err(err) = stop(&id).await {
                    tracing::warn!(plugin = %id, %err, "stopping the plugin failed");
                }
            }
            Action::Restart => {
                let Some(spec) = declared.get(&id) else {
                    continue;
                };
                tracing::info!(plugin = %id, exec = %spec.exec, "declared spec changed; restarting");
                if let Err(err) = restart(&id, spec).await {
                    tracing::warn!(plugin = %id, %err, "restarting the plugin failed");
                }
            }
            Action::Launch => {
                let Some(spec) = declared.get(&id) else {
                    continue;
                };
                let extra_env = resolve_secret_env(spec).await;
                if let Err(err) = launch(&id, spec, &extra_env).await {
                    tracing::warn!(plugin = %id, %err, "plugin launch failed");
                }
            }
        }
    }
}

/// Kick off the startup reconcile on the shared tokio runtime. Called once from
/// `main.rs`'s run body; guarded so a re-fired `activate` (a second `trollshell`
/// invocation remote-activating the primary instance) can't double-launch.
pub fn launch_at_startup() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    hytte::reactive::runtime::handle().spawn(reconcile());
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
    // An unreadable state file (`None`) degrades to "nothing declared" here:
    // listing what systemd knows is still better than an empty tab. Only
    // `reconcile` treats the distinction as load-bearing.
    let declared = load_declared().await.unwrap_or_default();
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
                description: String::new(),
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
    let declared = load_declared().await.unwrap_or_default();
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
    let declared = load_declared().await.unwrap_or_default();
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
    let declared = load_declared().await.unwrap_or_default();
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
///
/// If the relaunch fails, the plugin is brought back up from its **unit file**
/// if it has one, so a restart never leaves a plugin simply gone. That is the
/// one configuration where the transient relaunch is structurally impossible:
/// systemd refuses to create a transient unit whose name "was already loaded or
/// has a fragment file", so an id that is *both* declared and hand-installed as
/// a static unit (`etc/systemd/user/trollshell-plugin-<id>.service`) can only
/// ever run from the static unit. Pick one or the other — with both, every
/// reconcile that decides to restart will bounce the plugin through this
/// fallback and log it.
async fn restart(id: &str, spec: &PluginSpec) -> anyhow::Result<()> {
    stop(id).await?;
    wait_until_stopped(id).await;
    let extra_env = resolve_secret_env(spec).await;
    let Err(err) = launch(id, spec, &extra_env).await else {
        return Ok(());
    };
    if systemd::start_plugin(id).await.is_ok() {
        tracing::warn!(
            plugin = %id,
            "transient relaunch failed; brought the plugin back from its unit file \
             instead (declared *and* hand-installed as a static unit?)"
        );
    }
    Err(err)
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

    /// A declared spec with env, so a fingerprint test has something to change.
    fn spec_env(exec: &str, env: &[(&str, &str)]) -> PluginSpec {
        let mut s = spec(exec, true);
        for (k, v) in env {
            s.env.insert((*k).to_owned(), (*v).to_owned());
        }
        s
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
        // The description carries the spec fingerprint (#695) so a later
        // reconcile can diff this unit against the declared spec.
        let description = format!(
            "--description={}",
            unit_description("demo", &spec_fingerprint(&s))
        );
        assert_eq!(
            args,
            vec![
                "--user",
                "--quiet",
                "--collect",
                "--unit=trollshell-plugin-demo.service",
                description.as_str(),
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

    // ── spec fingerprint (#695) ──────────────────────────────────────────────

    #[test]
    fn fingerprint_is_pinned_and_stable() {
        // The digest crosses process (and build) boundaries — one shell writes
        // it into a unit description, a later one reads it back — so the exact
        // value is pinned, not just its properties. Changing the algorithm here
        // means every running plugin restarts once on upgrade.
        assert_eq!(
            spec_fingerprint(&spec("/bin/demo", true)),
            "963338d3f7f67d63"
        );
        assert_eq!(
            spec_fingerprint(&spec_env("/bin/demo", &[("A", "1")])),
            "01eabc3f30afe012"
        );
    }

    #[test]
    fn fingerprint_covers_exec_env_and_secrets() {
        let base = spec_env("/nix/store/aaa/bin/pet", &[("PET_NAME", "nisse")]);
        let fp = spec_fingerprint(&base);

        // A rebuilt package (new store path) is the #695 `package` half.
        let rebuilt = spec_env("/nix/store/bbb/bin/pet", &[("PET_NAME", "nisse")]);
        assert_ne!(spec_fingerprint(&rebuilt), fp);

        // A changed env value, an added key, and a dropped key all differ.
        assert_ne!(
            spec_fingerprint(&spec_env("/nix/store/aaa/bin/pet", &[("PET_NAME", "kat")])),
            fp
        );
        assert_ne!(
            spec_fingerprint(&spec_env(
                "/nix/store/aaa/bin/pet",
                &[("PET_NAME", "nisse"), ("PET_LLM_URL", "http://x")]
            )),
            fp
        );
        assert_ne!(spec_fingerprint(&spec("/nix/store/aaa/bin/pet", true)), fp);

        // Opting into a secret slot changes which key gets injected at spawn.
        let mut with_slot = base.clone();
        with_slot.secrets = vec!["openrouter".to_owned()];
        assert_ne!(spec_fingerprint(&with_slot), fp);
    }

    #[test]
    fn fingerprint_separators_keep_env_pairs_unambiguous() {
        // Without field separators these two would digest identically.
        let a = spec_env("/bin/x", &[("AB", "C")]);
        let b = spec_env("/bin/x", &[("A", "BC")]);
        assert_ne!(spec_fingerprint(&a), spec_fingerprint(&b));
    }

    #[test]
    fn fingerprint_ignores_enablement_and_map_order() {
        // `enabled` drives stop/launch, never a restart.
        let mut disabled = spec_env("/bin/x", &[("A", "1")]);
        disabled.enabled = false;
        assert_eq!(
            spec_fingerprint(&disabled),
            spec_fingerprint(&spec_env("/bin/x", &[("A", "1")]))
        );
        // The env is a BTreeMap, so insertion order can't perturb the digest.
        assert_eq!(
            spec_fingerprint(&spec_env("/bin/x", &[("A", "1"), ("B", "2")])),
            spec_fingerprint(&spec_env("/bin/x", &[("B", "2"), ("A", "1")]))
        );
    }

    #[test]
    fn description_round_trips_the_fingerprint() {
        let s = spec_env("/bin/pet", &[("PET_NAME", "nisse")]);
        let fp = spec_fingerprint(&s);
        let desc = unit_description("pet", &fp);
        assert_eq!(desc, format!("trollshell plugin: pet [cfg:{fp}]"));
        assert_eq!(parse_fingerprint(&desc), Some(fp.as_str()));
    }

    #[test]
    fn parse_fingerprint_rejects_foreign_descriptions() {
        // A legacy static unit, or a transient unit from a pre-#695 shell.
        assert_eq!(parse_fingerprint("trollshell plugin: pet"), None);
        assert_eq!(parse_fingerprint("Kaomoji cat widget"), None);
        assert_eq!(parse_fingerprint(""), None);
        // Truncated / malformed stamps don't parse as a fingerprint either.
        assert_eq!(parse_fingerprint("trollshell plugin: pet [cfg:abc"), None);
    }

    // ── merge + running states ───────────────────────────────────────────────

    fn unit(id: &str, active: &str, enabled: bool) -> systemd::PluginUnit {
        systemd::PluginUnit {
            id: id.to_owned(),
            active_state: active.to_owned(),
            enabled,
            description: String::new(),
        }
    }

    /// A live unit as this launcher would have stamped it for `spec`.
    fn unit_for(id: &str, active: &str, spec: &PluginSpec) -> systemd::PluginUnit {
        systemd::PluginUnit {
            description: unit_description(id, &spec_fingerprint(spec)),
            ..unit(id, active, false)
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

    // ── reconcile diff (#695) ────────────────────────────────────────────────
    //
    // The whole convergence decision is `plan`, so every case below is the
    // reconcile behaviour itself — no systemd, no D-Bus, no `systemd-run`.

    /// `{id: spec}` from pairs, for the diff tests.
    fn declared(entries: &[(&str, PluginSpec)]) -> BTreeMap<String, PluginSpec> {
        entries
            .iter()
            .map(|(id, spec)| ((*id).to_owned(), spec.clone()))
            .collect()
    }

    #[test]
    fn plan_launches_enabled_plugins_that_are_not_running() {
        let pet = spec_env("/bin/pet", &[("PET_NAME", "nisse")]);
        let d = declared(&[("pet", pet)]);
        // Nothing running at all (fresh session).
        assert_eq!(plan(&d, &[]), vec![("pet".to_owned(), Action::Launch)]);
        // A unit systemd still knows but that isn't live — inactive, failed,
        // or on its way down — is launchable (`--collect` released the name).
        for state in ["inactive", "failed", "deactivating"] {
            assert_eq!(
                plan(&d, &[unit("pet", state, false)]),
                vec![("pet".to_owned(), Action::Launch)],
                "state {state}"
            );
        }
    }

    #[test]
    fn plan_leaves_a_unit_running_the_declared_spec_alone() {
        let pet = spec_env("/bin/pet", &[("PET_NAME", "nisse")]);
        let units = vec![unit_for("pet", "active", &pet)];
        assert!(plan(&declared(&[("pet", pet)]), &units).is_empty());
    }

    #[test]
    fn plan_restarts_when_env_changed() {
        // The reported #695 case: `env.PET_LLM_URL` added by a switch.
        let running = spec_env("/bin/pet", &[("PET_NAME", "nisse")]);
        let now = spec_env(
            "/bin/pet",
            &[
                ("PET_NAME", "nisse"),
                ("PET_LLM_URL", "http://127.0.0.1:8787"),
            ],
        );
        let units = vec![unit_for("pet", "active", &running)];
        assert_eq!(
            plan(&declared(&[("pet", now)]), &units),
            vec![("pet".to_owned(), Action::Restart)]
        );
    }

    #[test]
    fn plan_restarts_when_the_package_was_rebuilt() {
        // The worse #695 half: `nix flake update` rewrites every store path.
        let running = spec_env("/nix/store/old/bin/pet", &[("PET_NAME", "nisse")]);
        let now = spec_env("/nix/store/new/bin/pet", &[("PET_NAME", "nisse")]);
        let units = vec![unit_for("pet", "active", &running)];
        assert_eq!(
            plan(&declared(&[("pet", now)]), &units),
            vec![("pet".to_owned(), Action::Restart)]
        );
    }

    #[test]
    fn plan_restarts_a_unit_launched_before_the_fingerprint_existed() {
        // A pre-#695 shell's transient unit (or a legacy *static* unit for a
        // declared id): no fingerprint to compare, so converge rather than
        // guess — a one-time recycle on upgrade.
        let pet = spec("/bin/pet", true);
        let units = vec![unit("pet", "active", false)];
        assert_eq!(
            plan(&declared(&[("pet", pet)]), &units),
            vec![("pet".to_owned(), Action::Restart)]
        );
    }

    #[test]
    fn plan_stops_a_plugin_declared_disabled() {
        let mut pet = spec_env("/bin/pet", &[("PET_NAME", "nisse")]);
        pet.enabled = false;
        let units = vec![unit_for("pet", "active", &pet)];
        assert_eq!(
            plan(&declared(&[("pet", pet)]), &units),
            vec![("pet".to_owned(), Action::Stop)]
        );
    }

    #[test]
    fn plan_leaves_a_disabled_stopped_plugin_alone() {
        let mut pet = spec("/bin/pet", true);
        pet.enabled = false;
        assert!(plan(&declared(&[("pet", pet)]), &[]).is_empty());
    }

    #[test]
    fn plan_stops_a_launcher_stamped_unit_that_is_no_longer_declared() {
        // `plugins.pet` removed from the config entirely: the unit carries our
        // stamp, so the launcher owns it and shuts it down.
        let gone = spec("/bin/pet", true);
        let units = vec![unit_for("pet", "active", &gone)];
        assert_eq!(
            plan(&BTreeMap::new(), &units),
            vec![("pet".to_owned(), Action::Stop)]
        );
    }

    #[test]
    fn plan_never_touches_units_it_did_not_launch() {
        // Legacy static units (#419's manual path) carry no fingerprint and are
        // not declared — reconcile must not stop them, whatever their state.
        let units = vec![
            unit("timer", "active", true),
            systemd::PluginUnit {
                description: "Hand-written plugin unit".to_owned(),
                ..unit("terminal", "active", true)
            },
        ];
        assert!(plan(&BTreeMap::new(), &units).is_empty());
    }

    #[test]
    fn plan_orders_stops_before_restarts_before_launches() {
        let stale = spec_env("/bin/stale", &[("V", "old")]);
        let fresh = spec_env("/bin/stale", &[("V", "new")]);
        let mut off = spec("/bin/off", true);
        off.enabled = false;
        let d = declared(&[
            ("stale", fresh),
            ("off", off.clone()),
            ("new", spec("/bin/new", true)),
        ]);
        let units = vec![
            unit_for("stale", "active", &stale),
            unit_for("off", "active", &off),
            // An orphan (declared entry removed) stops too.
            unit_for("gone", "active", &spec("/bin/gone", true)),
        ];
        assert_eq!(
            plan(&d, &units),
            vec![
                ("gone".to_owned(), Action::Stop),
                ("off".to_owned(), Action::Stop),
                ("stale".to_owned(), Action::Restart),
                ("new".to_owned(), Action::Launch),
            ]
        );
    }

    #[test]
    fn plan_treats_a_stopped_orphan_as_nothing_to_do() {
        // Only *running* units are candidates; an inactive leftover is already
        // where reconcile wants it.
        let gone = spec("/bin/gone", true);
        let units = vec![systemd::PluginUnit {
            description: unit_description("gone", &spec_fingerprint(&gone)),
            ..unit("gone", "inactive", false)
        }];
        assert!(plan(&BTreeMap::new(), &units).is_empty());
    }
}
