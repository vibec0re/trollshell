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
//!   whether they're enabled — which, unless nix pins it, the Plugins tab's
//!   switch may override (see "The Plugins tab's switch persists" below).
//! - **the host launches.** At startup ([`launch_at_startup`]) every enabled,
//!   not-already-running plugin is spawned as a *transient* user unit
//!   (`systemd-run --user --unit=trollshell-plugin-<id>.service … <exec>`); the
//!   control-center's Plugins tab (#348) start goes through the same path
//!   ([`start`]).
//! - **systemd owns runtime state** (the system-daemon-as-state-store rule):
//!   crash supervision (`Restart=on-failure`), lifetime (`PartOf=` the session
//!   target — a plugin survives a *shell* restart but dies with the session),
//!   and stop. The shell keeps no runtime plugin state.
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
//! | enabled  | running, no fingerprint      | restart   |
//! | disabled | running, launcher-stamped    | stop      |
//! | disabled | running, no fingerprint      | leave     |
//! | absent   | running, launcher-stamped    | stop      |
//! | absent   | running, no fingerprint      | leave     |
//!
//! The two "no fingerprint → leave" rows are the legacy-static-unit guard: a
//! unit this launcher never spawned carries no fingerprint, so reconcile never
//! **stops** it. That covers a declared-but-off id too (#1400 review, finding
//! 4): `availablePlugins` declares every bundled id, off, so without that row
//! a hand-installed static unit for one of them would be stopped on every
//! reconcile. A declared-**on** id with an unstamped running unit is still
//! restarted, since converging onto the declared spec is the point; for a
//! static unit that bounces it through [`restart`]'s unit-file fallback.
//!
//! [`reconcile`] runs at shell startup, again whenever `plugins.json` changes
//! on disk (#1399, see the next section), and on demand via
//! the `Control.ReloadPlugins` D-Bus method, which the home-manager module
//! still calls from its activation script so a switch applies at once instead
//! of on the next poll.
//!
//! ## Watching the state file (#1399)
//!
//! Until #1399 a shell restart was the only thing that helped NixOS-module
//! users: that module writes `/etc/xdg/trollshell/plugins.json` from *system*
//! activation, which runs as root with no user bus and so can never call
//! `ReloadPlugins`. A `nixos-rebuild switch` that added a plugin, bumped its
//! package or flipped `enable` succeeded, and nothing happened. So the shell
//! now polls the file itself ([`converge_then_watch`]), in the same task that
//! runs the startup reconcile, and reconciles when it moves. Five choices in
//! it are not obvious:
//!
//! - **The stamp is `hytte-config`'s content hash**
//!   ([`stamps_of`](hytte_config::subsystem::watch::stamps_of)), not `(mtime,
//!   len)`. Both modules deploy the file as a nix-store path (`/etc/xdg/…` →
//!   `/etc/static/…` → `/nix/store/…`, and home-manager's `xdg.configFile`
//!   symlink), every store file has the constant mtime `1970-01-01 00:00:01`,
//!   and the most common change, a package bump, swaps one store hash inside
//!   `exec` for another of the same length. An `(mtime, len)` stamp sees
//!   neither.
//! - **Every candidate path is stamped**, not only the file that won.
//!   Resolution is first-existing-file-wins ([`candidate_paths`]), so a
//!   home-manager file appearing over the `/etc/xdg` one changes the spec
//!   without either file's bytes changing (`hytte-config`'s #1040 R9, same
//!   shape). Stamping a shadowed file costs a spurious reconcile at worst,
//!   which also relaunches any effectively-enabled plugin that is not running
//!   (crashed, or stopped behind the launcher's back with `systemctl --user
//!   stop`) — the effective state wins, as it does on a home-manager poke.
//!   Since #1400 a plugin stopped from the Plugins tab is not one of them:
//!   the switch persisted the stop, so it is effectively off.
//! - **Stamp, then load** (#1040 V2): the baseline is taken before startup's
//!   reconcile reads the file, and every later tick re-stamps before it
//!   reconciles, so an edit landing in between is one tick late, never folded
//!   into the stamps and lost. The first observation therefore runs no
//!   reconcile of its own; startup's covers it. The stamps update
//!   **unconditionally** on a move, so a file that stops parsing is
//!   reconciled against once per save (which leaves the plugins alone, see
//!   [`load_declared_from`]) rather than once per tick.
//! - **A reconcile that could not list the units is retried** (#1402 review,
//!   finding 4). The stamps took the change before that reconcile ran, so a
//!   watch pass that cannot list the live units acts on nothing
//!   ([`Trigger::Watch`]) and leaves the change pending, and the next tick
//!   reconciles again until one pass settles. It warns once per outage.
//!   Startup and `ReloadPlugins` still launch blind ([`Trigger::OneShot`]),
//!   so the plugins come up while the user manager is briefly unreachable,
//!   but since #1404 the stops and restarts such a pass could not see are
//!   retried too: one blind pass, then a real one on the next tick. Startup's
//!   pass starts the watch with the retry already pending, and a
//!   `ReloadPlugins` pass hands its failure to the watch through
//!   [`RELOAD_UNLISTED`]. A *single plugin's* launch, stop or restart that
//!   fails is **not** retried by anyone: an unbounded relaunch retry is
//!   #880's bug.
//! - **Serialisation is [`CONVERGE_LOCK`]'s**, which [`reconcile`] already
//!   takes: a tick racing a home-manager poke or a Plugins-tab start queues
//!   behind it, and the second pass re-reads the file and finds nothing left
//!   to do.
//!
//! The cadence is `hytte-config`'s
//! [`POLL_INTERVAL`](hytte_config::subsystem::watch::POLL_INTERVAL) (3 s).
//! The task is supervised (`hytte::reactive::spawn_supervised`), so a panic
//! in it restarts it with a fresh baseline and a fresh reconcile rather than
//! leaving the watch dead for the session.
//!
//! **One transient the watch does see: moving plugins from the NixOS module
//! to home-manager in one `nixos-rebuild switch`** (#1402 review, finding 5).
//! Within either module a switch replaces its `plugins.json` symlink
//! atomically, so no tick ever finds the file missing. Across that one
//! migration, though, the `/etc/xdg` file is gone at the `/etc/static` swap,
//! inside system activation, and `~/.config/trollshell/plugins.json` only
//! appears seconds later when `home-manager-<user>.service` runs. A tick in
//! that gap sees every candidate absent, reads it as "nothing declared" and
//! stops every launched plugin. The next tick, or home-manager's own poke,
//! relaunches them from the new file. It is a one-off bounce, and nothing
//! here debounces it. The reverse move (home-manager → NixOS) has no gap,
//! because the `/etc` file appears before home-manager removes its own.
//!
//! ## The Plugins tab's switch persists (#1400)
//!
//! nix declares, but for most plugins it only declares a **default**. The
//! control-center's switch sends `SetPluginEnabled` and then
//! `StartPlugin`/`StopPlugin` (persist first, so a refused persist changes
//! nothing; #1400 review, finding 5), and [`set_enabled`] keeps that choice
//! for a declared plugin in `$XDG_STATE_HOME/trollshell/plugins.toml`
//! ([`Overrides`], through `hytte_config::state`, #866 decision 3: state is
//! what the shell writes when you flip a toggle). It stores only
//! **differences** from the declared value, in either direction, so switching
//! a plugin back to what nix says deletes its entry. [`load_declared_from`]
//! folds the file into every plugin's `enabled` ([`effective_enabled`]), which
//! is how [`reconcile`], [`list`], [`start`] and the #1399 watch all see one
//! effective value and a switched-on plugin survives a shell restart.
//!
//! Who wins is decided by **nix priority**, the rule #1227 set for config
//! keys (option C on the #1400 thread): an `enable` assigned plainly or with
//! `lib.mkForce` is **pinned**, and the modules render `"_locked":
//! ["enabled"]` on its entry ([`PluginSpec::enable_locked`]). For a pinned
//! plugin an override on disk is ignored and [`set_enabled`] refuses to
//! write one, with an error naming `programs.trollshell.plugins.<id>.enable`;
//! the control-center greys that switch, so only a stale tab or a hand-made
//! `busctl` call ever sees the error. An `enable` left unset or set with
//! `lib.mkDefault` pins nothing, and the switch decides.
//!
//! The state file is **not** watched: its only writer is [`set_enabled`],
//! behind a switch that starts or stops the plugin itself right after. An
//! override for an id no longer declared, or for a pinned one, is ignored
//! rather than deleted, so a pin relaxed back to `lib.mkDefault` finds the
//! switch's last choice again.
//!
//! ## The session target (#707)
//!
//! The transient unit's `PartOf=` used to be hardcoded to
//! `graphical-session.target`, while the *shell's own* unit binds to the
//! configurable `programs.trollshell.systemd.target` (`nix/hm-module.nix`,
//! exampled — and shipped in `etc/` — as `niri-session.target`). Under any
//! non-default target the shell and its plugins bound to *different* targets,
//! so session teardown reached them out of step.
//!
//! The target therefore rides the state file: a top-level `"target"` key
//! ([`PluginState::target`]) the home-manager module renders from
//! `systemd.target`. It is optional and backward-compatible — a `plugins.json`
//! written by a pre-#707 module (or by the NixOS module, which declares no
//! `systemd.target` option at all, having no shell unit of its own to bind)
//! carries no `"target"` and falls back to [`DEFAULT_TARGET`], i.e. exactly the
//! previous behavior. A value that could not be a unit name is rejected with a
//! warning and the default used, same as [`sanitize`]'s other guards.
//!
//! A *changed* target recycles the running plugins, because the target is part
//! of the spec fingerprint ([`spec_fingerprint`]) — but only when it differs
//! from [`DEFAULT_TARGET`], so a default-configured session digests exactly as
//! it did pre-#707 and upgrading doesn't bounce every plugin once for nothing.
//!
//! ## Secret injection (#392)
//!
//! [`launch()`] takes `extra_env`. That is the hook #392 (AI API-key management)
//! rides on: [`resolve_secret_env`] reads each slot in [`PluginSpec::secrets`]
//! from the login keyring (via [`crate::secrets`]) and maps it to its injected
//! `(<SLOT>_API_KEY, value)` pair; every `launch` call site builds `extra_env`
//! this way before calling in, and rotating a key is just stop + relaunch. Key
//! *management* (writing/rotating/deleting the stored key itself) is
//! [`crate::secrets`] and the control-center's AI Keys tab, not this module.
//!
//! ### Where the value does and does not go
//!
//! This list used to read "never lands in the state file, a unit file, or the
//! plugin's own config" — a claim that was both incomplete (it omitted the
//! argv, which is how #984's world-readable leak survived) and, on the "unit
//! file" clause, simply false. Spelled out, so the next reader can check it
//! rather than trust it:
//!
//! | channel | protection |
//! |---|---|
//! | `plugins.json` (the nix-written state file) | never written there. It is a nix-store symlink, `0444` — world-readable, which is why `secrets` exists as a separate option from `env` |
//! | the shipped unit files (nix store, `etc/systemd/user/`) | never written there; a launched plugin has no unit file of its own to write to |
//! | the plugin's own config | never written there — the plugin reads its key from the environment and never learns where it was stored |
//! | `systemd-run`'s argv, `/proc/<pid>/cmdline` | `0444`, **world-readable** — never carries the value since #984, only the bare `--setenv=<NAME>`; see below |
//! | `systemd-run`'s `/proc/<pid>/environ` | `0400`, owner-only — the intended channel |
//! | the **transient unit fragment** systemd writes for the launched unit, `/run/user/<uid>/systemd/transient/<unit>.service` | the file itself is `0644` and holds `Environment="<NAME>=<value>"` in plaintext; what contains it is `/run/user/<uid>` being `0700`. **Same-user**, in scope per #956 — and true before #984 as well as after |
//! | the unit's `Environment=` property (`systemctl --user show -p Environment`) | same-user, in scope per #956; this is what the live-verify step reads back to confirm the key actually arrived |
//! | the journal, and this module's own `tracing` output | never — every log line here carries the plugin id, exec, target or slot *name* only |
//!
//! The same-user rows are deliberate, not oversights: #956 settled that a
//! process which can already reach the plugin socket could launch its own
//! plugin unit anyway, so the boundary this module defends is the *user*, and
//! only the world-readable rows are defects.
//!
//! ### …and never on the argv (#984)
//!
//! The one channel that list left out was the one that crosses the same-user
//! boundary #956 settled on. `extra_env` used to render as
//! `--setenv=<VAR>=<value>` **argv elements**, and while `/proc/<pid>/environ`
//! is `0400` owner-only, `/proc/<pid>/cmdline` is `0444`: any local user could
//! read every plugin's API key off the `systemd-run` process, at every launch,
//! every reconcile relaunch and every key rotation.
//!
//! So the value travels over `systemd-run`'s **own environment** instead:
//! [`crate::launch::command`] sets each `extra_env` pair with `Command::env`
//! while the argv it builds emits the **bare**
//! `--setenv=<VAR>` form, which `systemd-run` documents as "when `=` and
//! *VALUE* are omitted, the value of the variable with the same name in the
//! program environment will be used" (`systemd-run(1)`, the option itself
//! since v211). Ordering semantics are unchanged — a bare `--setenv=K` after a
//! `--setenv=K=declared` still wins, so an injected secret still overrides a
//! stale value declared in the spec.
//!
//! The spec's declared [`env`](PluginSpec::env) deliberately **stays** on the
//! argv as `--setenv=K=V`: it is rendered by nix into the world-readable
//! `plugins.json`, so argv discloses nothing that a `cat` of the state file
//! doesn't, and keeping it there means a declared value is passed explicitly
//! rather than being read back out of whatever the shell happened to inherit
//! under the same name. Only secrets move.
//!
//! The same-user rows of the table above — the transient fragment and the
//! unit's `Environment=` property — are unchanged by #984 and stay in scope per
//! #956. Only the world-readable argv row moved.
//!
//! ## …and waiting for one that isn't there yet (#866)
//!
//! A slot with no readable key is *skipped*, not fatal — the plugin launches
//! keyless. That is the right call at launch time and the wrong place to stop,
//! because of one very ordinary session: the shell starts before gnome-keyring
//! is unlocked, every declared slot reads back empty, and when the ring unlocks
//! two seconds later nothing goes back to re-inject. The plugin stays keyless for
//! the rest of the session with nothing on screen to say why.
//!
//! So [`resolve_secret_env`] reports what it *couldn't* resolve, and
//! [`note_resolution`] records those `(slot, plugin)` pairs. While any are
//! outstanding one background task ([`watch_outstanding_secrets`]) re-probes them
//! every [`SECRET_POLL_INTERVAL`] with [`crate::secrets::probe`] — which, unlike
//! the launch-time [`get`](crate::secrets::get), tells "the ring is locked" apart
//! from "nobody stored one", **and unlike `get` never raises an unlock prompt**
//! (that distinction is the whole of `probe`'s docs; a poller that prompted every
//! 30s would be worse than the bug it fixes). **Both non-available states keep
//! waiting**: an external tool can write a key at any time, so the distinction is
//! logged, never acted on. A slot that comes back available goes to the existing
//! [`relaunch_for_secret_inner`], and is dropped only for the plugins that
//! actually came back up, or that failed to come back
//! [`MAX_RELAUNCH_FAILURES`] times in a row ([`settle_slot`], #880 — see its
//! doc comment for why an id can fail on *every* attempt); when nothing is
//! left outstanding the task stands down, so a session whose keys all
//! resolved at launch polls exactly zero times.
//!
//! **Polling is a choice, not a constraint.** oo7 0.5.0 surfaces no unlock
//! signal, but the underlying `org.freedesktop.Secret.Collection` does carry a
//! `Locked` property, so a `PropertiesChanged` subscription through `hytte-bus`
//! is buildable. It was not built here because it would mean a second, hand-rolled
//! Secret Service client living beside oo7 for one edge, and a 30s prompt-free
//! property read costs nothing. Revisit if the outstanding set ever gets large.
//!
//! One case that does poll indefinitely, by design: an `api`-mode claude bridge
//! configured with its key in `~/.config/trollshell/anthropic.key` and nothing in
//! the keyring declares the `anthropic` slot, never resolves it, and is therefore
//! watched for the session. The cost is one property read every 30s and the
//! payoff is that moving the key into the keyring takes effect without a
//! relogin; [`prune_undeclared`] still drops it the moment the plugin leaves the
//! config.
//!
//! ## Legacy static units
//!
//! Hand-installed static units (`etc/systemd/user/trollshell-plugin-*.service`,
//! the pre-#419 path) keep working: the transport (`plugins.rs`) doesn't care
//! who spawned a plugin, and the control-surface fns here fall back to plain
//! `StartUnit` / unit-file enablement for any id that isn't in the declarative
//! state file.
//!
//! An id that **is** declared is the launcher's, and since #1400 the nix
//! modules declare every bundled id by default (`availablePlugins`, off). A
//! static unit for such an id is left alone while the plugin is off: reconcile
//! stops only a unit it stamped ([`plan`]). What the launcher cannot do is
//! start it: switching the plugin on (the Plugins tab, or `enable = true`)
//! makes it launch its own transient unit, which systemd refuses while the
//! static unit's file exists. To hand an id to a static unit entirely, drop it
//! from `availablePlugins` (and `plugins`), so it is undeclared again.
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

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use hytte::services::systemd;
use hytte_config::subsystem::watch::{self, Stamp};
use serde::{Deserialize, Serialize};

use crate::launch::{self, Launch};
use crate::secrets::SecretProbe;

/// Relative path of the declarative state file under each XDG config root.
/// Written by the nix modules (`nix/hm-module.nix` → `$XDG_CONFIG_HOME`,
/// `nix/nixos-module.nix` → `/etc/xdg`); absent = no declared plugins (the
/// launcher stays inert and any static units keep working).
const STATE_FILE_REL: &str = "trollshell/plugins.json";

/// The systemd user target a launched plugin unit binds to (`PartOf=`) when the
/// state file names none — every `plugins.json` written before #707, and every
/// one the NixOS module writes. Also systemd's own session target, so this stays
/// the right answer for a session that never renamed it.
const DEFAULT_TARGET: &str = "graphical-session.target";

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
    /// ([`crate::secrets`]) and injected as `<SLOT>_API_KEY` (see
    /// [`crate::secrets::env_var_for`]) — over `systemd-run`'s inherited
    /// environment plus a bare `--setenv=<SLOT>_API_KEY`, never as an argv
    /// `=<value>` (#984). A slot with no stored key is skipped (the plugin runs
    /// keyless). A plugin that doesn't list a slot never gets that key in its
    /// environment.
    #[serde(default)]
    secrets: Vec<String>,
    /// Whether the host launches this plugin at startup. A disabled plugin is
    /// still *declared* — it lists in the control-center and can be started
    /// manually — it just doesn't auto-launch.
    ///
    /// As read from the file this is the **declared** value; [`load_declared_from`]
    /// replaces it with the effective one, the Plugins tab's persisted switch
    /// folded in (#1400, [`effective_enabled`]).
    #[serde(default = "default_enabled")]
    enabled: bool,
    /// The keys of this entry nix **pins** (#1400), in #1227's `_locked`
    /// spelling. Today only ever `["enabled"]`, rendered when
    /// `programs.trollshell.plugins.<id>.enable` was assigned at a priority
    /// stronger than `lib.mkDefault` — a plain `enable = true;` or a
    /// `lib.mkForce`. Absent (every unpinned entry, and every file written
    /// before #1400) pins nothing; a name this shell does not know is
    /// ignored, so a later module pinning another key cannot break it.
    #[serde(default, rename = "_locked")]
    locked: Vec<String>,
}

/// The `_locked` entry that pins [`PluginSpec::enabled`] (#1400) — the JSON
/// key it pins, exactly as #1227's markers name theirs.
const LOCKED_ENABLED: &str = "enabled";

impl PluginSpec {
    /// Whether nix pins this plugin's `enabled` (#1400): the Plugins tab's
    /// switch cannot persist over it, and any override already on disk for it
    /// is ignored. Pure.
    fn enable_locked(&self) -> bool {
        self.locked.iter().any(|key| key == LOCKED_ENABLED)
    }
}

/// `enabled`'s value when a hand-written `plugins.json` omits it. Still
/// `true`, unlike the nix option's `false` default since #1400: both modules
/// always write the field, so this only ever answers for a file a person
/// wrote, and such a file written before #1400 meant "launch it".
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
    /// Systemd user target the launched plugin units bind to (#707), rendered
    /// from `programs.trollshell.systemd.target` — the same value the *shell's*
    /// own unit binds to. Absent (a pre-#707 or NixOS-written file) means
    /// [`DEFAULT_TARGET`]; that is the whole of the backward compatibility.
    #[serde(default)]
    target: Option<String>,
}

/// The sanitized declaration the launcher acts on: which plugins are declared,
/// and the session target their units bind to. [`Default`] is the "nothing
/// declared" answer — an empty set on the default target.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Declared {
    plugins: BTreeMap<String, PluginSpec>,
    target: String,
}

impl Default for Declared {
    fn default() -> Self {
        Self {
            plugins: BTreeMap::new(),
            target: DEFAULT_TARGET.to_owned(),
        }
    }
}

/// Parse the state file's JSON. Pure; sanitization (id/charset checks, target
/// validation) is [`sanitize`]'s job so both are unit-testable apart.
fn parse_state(json: &str) -> Result<PluginState, serde_json::Error> {
    serde_json::from_str::<PluginState>(json)
}

/// Whether `target` can be used as the `PartOf=` value on a launched plugin
/// unit: non-empty, within systemd's unit-name length bound, and drawn from
/// systemd's unit-name charset (ASCII alphanumerics plus `:-_.\@`). A **charset**
/// guard, not a full unit-name validation — it doesn't insist on a `.target`
/// suffix, because that would silently swap in the default for a deliberate
/// `PartOf=` on some other unit type, which systemd itself permits.
///
/// The state file is nix-written, so this should never trip; it exists because
/// the value lands verbatim in a `systemd-run --property=PartOf=…` argument, and
/// a hand-edited file must degrade to the default rather than to an argument
/// systemd can't parse. Pure.
fn is_valid_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 255
        && target.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'_' | b'.' | b'\\' | b'@')
        })
}

/// Drop entries the launcher must not act on: an id that fails the
/// `trollshell-plugin-<id>.service` template's charset guard, an empty `exec`,
/// or an env key that would corrupt a `--setenv=K=V` argument — plus a
/// `"target"` that isn't a unit name (#707), which degrades to
/// [`DEFAULT_TARGET`] rather than dropping anything. Each drop is logged loudly
/// — a nix-written file should never trip these, so a trip means the file was
/// edited by hand.
fn sanitize(state: PluginState) -> Declared {
    let target = match state.target {
        Some(target) if is_valid_target(&target) => target,
        Some(target) => {
            tracing::warn!(%target, default = DEFAULT_TARGET, "plugins.json: invalid systemd target; using the default");
            DEFAULT_TARGET.to_owned()
        }
        None => DEFAULT_TARGET.to_owned(),
    };
    let plugins = state
        .plugins
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
        .collect();
    Declared { plugins, target }
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

/// [`candidate_paths`] for this process's own environment — the one place the
/// launcher reads `XDG_CONFIG_HOME`/`HOME`/`XDG_CONFIG_DIRS`.
fn state_file_paths() -> Vec<PathBuf> {
    candidate_paths(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        std::env::var("XDG_CONFIG_DIRS").ok().as_deref(),
    )
}

/// Everything the launcher reads to decide what is declared, as paths: the
/// `plugins.json` candidates nix renders (config), and the one
/// `plugins.toml` the Plugins tab's switch persists into (state, #1400).
///
/// A value rather than two environment reads at each use so the whole chain
/// — the #1399 watch, [`reconcile_from`], [`set_enabled_in`] — is drivable in
/// a test against scratch files, with neither the developer's real
/// `~/.config/trollshell` nor their real `$XDG_STATE_HOME/trollshell` in
/// reach.
#[derive(Clone, Debug)]
struct Sources {
    /// The `plugins.json` candidates, in XDG precedence order
    /// ([`candidate_paths`]).
    config: Vec<PathBuf>,
    /// `$XDG_STATE_HOME/trollshell/plugins.toml` ([`OVERRIDES_SUBSYSTEM`]),
    /// or `None` when neither `$XDG_STATE_HOME` nor `$HOME` is set — then
    /// nothing is overridden and the switch cannot persist.
    overrides: Option<PathBuf>,
}

impl Sources {
    /// This process's own sources — the one place the launcher resolves both
    /// halves from the environment.
    fn from_env() -> Self {
        Self {
            config: state_file_paths(),
            overrides: hytte_config::state::path(OVERRIDES_SUBSYSTEM),
        }
    }
}

/// [`load_declared_from`] over this process's own [`Sources`].
async fn load_declared() -> Option<Declared> {
    load_declared_from(&Sources::from_env()).await
}

/// The **effective** declaration: what nix declares ([`load_nix_declared`]),
/// with the Plugins tab's persisted switch folded into each plugin's
/// `enabled` ([`fold_overrides`], #1400).
///
/// `None` exactly when [`load_nix_declared`] says so — a `plugins.json` that
/// exists but cannot be read or parsed. The override file never makes this
/// `None`: it is the shell's own, and one that does not parse is read as "no
/// overrides" (`hytte_config::state`'s contract; the next switch rewrites
/// it).
///
/// This is the one place the effective spec is assembled; every reader goes
/// through it — [`reconcile`], [`list`], [`start`], the #1399 watch — so they
/// cannot disagree about whether a plugin is on. It takes its [`Sources`]
/// rather than reading the environment so the watch's production task is
/// drivable in a test against scratch files (see [`reconcile_then_watch`]).
async fn load_declared_from(sources: &Sources) -> Option<Declared> {
    let mut declared = load_nix_declared(&sources.config).await?;
    fold_overrides(&mut declared, &read_overrides(sources.overrides.as_deref()));
    Some(declared)
}

/// Load + parse + sanitize the declarative plugin state from the first of
/// `paths` that exists — nix's half of [`load_declared_from`], before the
/// switch's overrides are folded in. [`set_enabled_in`] reads this one
/// directly, because an override is stored as a difference from the
/// **declared** value, never from an effective one.
///
/// Missing file = **no declared plugins** (inert, not an error) —
/// `Some(Declared::default())`, which is a real answer: it says every
/// declarative plugin was removed from the config.
///
/// `None` means "a state file exists but we couldn't read or parse it" —
/// deliberately *not* the same as "nothing is declared", because [`reconcile`]
/// stops plugins that are no longer declared and a typo'd file must never be
/// read as "stop everything". A broken file also stops the search rather than
/// falling through to a lower-precedence one: masking a broken user file with a
/// system one would be quiet drift.
async fn load_nix_declared(paths: &[PathBuf]) -> Option<Declared> {
    for path in paths {
        match tokio::fs::read_to_string(path).await {
            Ok(json) => match parse_state(&json) {
                Ok(state) => return Some(sanitize(state)),
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
    Some(Declared::default())
}

// ── The Plugins tab's persisted switch (#1400) ───────────────────────────────

/// The `hytte_config::state` subsystem the switch persists into:
/// `$XDG_STATE_HOME/trollshell/plugins.toml`. State, not config — #866
/// decision 3: the shell writes it when you flip a toggle, you never edit it,
/// and nix never renders it.
const OVERRIDES_SUBSYSTEM: &str = "plugins";

/// `plugins.toml`: where the Plugins tab's switch disagrees with what nix
/// declares, per plugin id.
///
/// ```toml
/// [enabled]
/// timer = true    # declared off (the default); switched on
/// pet = false     # declared `lib.mkDefault true`; switched off
/// ```
///
/// Only **differences** are stored ([`record_override`]), in either
/// direction, so switching a plugin back to what nix says deletes its entry
/// and a file with no entries is deleted outright. The shell is its only
/// writer ([`set_enabled_in`]), and it is deliberately **not** watched: the
/// switch that writes it applies the change live with its own
/// `StartPlugin`/`StopPlugin` right after, so a watch would only ever
/// re-apply what the switch is already doing.
///
/// An entry is **ignored**, not deleted, when its plugin is pinned in nix or
/// no longer declared at all ([`fold_overrides`]): a pin that is later
/// relaxed back to `lib.mkDefault`, or a plugin that comes back, finds the
/// last choice the switch made.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
struct Overrides {
    /// Plugin id → the `enabled` the switch chose.
    #[serde(default)]
    enabled: BTreeMap<String, bool>,
}

/// A plugin's effective `enabled` (#1400): nix's value when nix pins it,
/// otherwise the switch's override if there is one, otherwise nix's value.
/// The whole of option C's rule, and pure so the truth table is a test.
fn effective_enabled(declared: bool, locked: bool, overridden: Option<bool>) -> bool {
    if locked {
        declared
    } else {
        overridden.unwrap_or(declared)
    }
}

/// Fold `overrides` into every declared plugin's `enabled`
/// ([`effective_enabled`]). Iterates the **declared** set, so an override for
/// an id `plugins.json` no longer declares is ignored by construction. Pure.
fn fold_overrides(declared: &mut Declared, overrides: &Overrides) {
    for (id, spec) in &mut declared.plugins {
        spec.enabled = effective_enabled(
            spec.enabled,
            spec.enable_locked(),
            overrides.enabled.get(id).copied(),
        );
    }
}

/// Record the switch's choice for `id` as a difference from its `declared`
/// value: store it when the two differ, drop the entry when they agree.
/// Returns whether the map changed, i.e. whether the file needs a write.
/// Pure.
fn record_override(overrides: &mut Overrides, id: &str, declared: bool, wanted: bool) -> bool {
    if wanted == declared {
        overrides.enabled.remove(id).is_some()
    } else {
        overrides.enabled.insert(id.to_owned(), wanted) != Some(wanted)
    }
}

/// The override file at `path`, or no overrides at all: no path, no file, or
/// a file that does not parse (logged by `hytte_config::state`).
fn read_overrides(path: Option<&Path>) -> Overrides {
    path.and_then(hytte_config::state::load_at)
        .unwrap_or_default()
}

/// Write `overrides` to `path`, or delete the file once nothing is
/// overridden — a switch put back to what nix says leaves no trace.
fn write_overrides(path: &Path, overrides: &Overrides) -> std::io::Result<()> {
    if overrides.enabled.is_empty() {
        hytte_config::state::remove_at(path)
    } else {
        hytte_config::state::store_at(path, overrides)
    }
}

/// What [`set_enabled_in`] does for one id, decided from nix's declaration
/// alone. Pure, so the three arms are testable without a user manager.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Persist {
    /// Not declared in `plugins.json`: a legacy static unit, whose
    /// enablement is its unit file's (`Enable/DisableUnitFiles`).
    UnitFile,
    /// Declared, and nix pins `enable`: refuse, and write nothing.
    Pinned,
    /// Declared and free: record the choice against this declared value.
    Override {
        /// What nix declares, which the override is stored relative to.
        declared: bool,
    },
}

/// Decide [`Persist`] for `id` against nix's own declaration (never an
/// effective one — see [`load_nix_declared`]). Pure.
fn persist_decision(nix: &Declared, id: &str) -> Persist {
    match nix.plugins.get(id) {
        None => Persist::UnitFile,
        Some(spec) if spec.enable_locked() => Persist::Pinned,
        Some(spec) => Persist::Override {
            declared: spec.enabled,
        },
    }
}

/// The error a pinned plugin's switch gets (#1400), naming the option that
/// pins it. The control-center greys a pinned switch, so this only reaches a
/// stale tab or a hand-made `busctl` call.
fn pinned_error(id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "plugin {id} is pinned in nix (programs.trollshell.plugins.{id}.enable); \
         change it there, or declare it with lib.mkDefault so the Plugins tab can switch it"
    )
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
/// pairs always digests the same), the `secrets` slot list (adding or dropping a
/// slot changes which key is injected), and the session `target` the unit's
/// `PartOf=` was set from (#707).
///
/// The target is folded in **only when it differs from [`DEFAULT_TARGET`]**, so
/// the default canonicalizes as "absent" and a default-configured session
/// digests byte-identically to a pre-#707 shell's — no one-time recycle of every
/// plugin on upgrade for a value that didn't change. A session that *does* name
/// a target (the `niri-session.target` `etc/` ships) gets the recycle it should:
/// its pre-#707 units digest without the target, so the first reconcile after
/// the upgrade sees a mismatch and relaunches them with the right `PartOf=`.
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
fn spec_fingerprint(spec: &PluginSpec, target: &str) -> String {
    // ASCII record (0x1e) / unit (0x1f) / group (0x1d) / file (0x1c) separators
    // between the parts, so `{"AB": "C"}` can't digest the same as
    // `{"A": "BC"}`. Only a value that itself contains one of those control
    // bytes could re-introduce an ambiguity, and a nix-rendered exec path / env
    // value / unit name never does.
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
    // The default target digests as absent — see the doc comment.
    if target != DEFAULT_TARGET {
        h = fnv1a(b"\x1c", h);
        h = fnv1a(target.as_bytes(), h);
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

/// This plugin's launch, as a [`crate::launch::Launch`].
///
/// The flag vocabulary itself lives in [`crate::launch`] since #1071 phase 2
/// (Annika: *"you should consider generalizing on this"*) — including #984's
/// argv/environment pairing, which that module keeps enforced the same way this
/// one used to: its argv builder is private and its `command` is the only way
/// out, so there is still no reachable path that produces an argv without the
/// matching environment. What stays here is only what is *about a plugin*:
///
/// - `--collect`, `--user`, `--quiet` and `--` are unconditional over there.
/// - `Restart=on-failure` / `RestartSec=2`: same supervision the static units
///   carried — supervision stays systemd's job.
/// - `PartOf=<target>`: stop propagates from session teardown, so plugins die
///   with the session but survive a shell restart. `target` is the state file's
///   (defaulting to [`DEFAULT_TARGET`]), so it is the *same* target the shell's
///   own unit binds to rather than a hardcoded guess at it (#707).
/// - `TimeoutStopSec=`[`crate::launch::PLUGIN_TIMEOUT_STOP`] (#1098, #1092
///   review M4): bounds a stuck `Plugin::shutdown` hook well below the user
///   manager's 90 s default — see that constant's doc for why 10 s and why
///   this launch only.
/// - the spec's declared `env` is passed value-inline as `--setenv=K=V`: it is
///   nix-rendered into the world-readable state file, so the argv discloses
///   nothing new, and an explicit value can't be shadowed by whatever the shell
///   inherited under the same name.
/// - `extra_env` (the #392 secret hook) goes in [`Launch::secret_env`], which is
///   rendered as the **bare** `--setenv=<NAME>` form with the value carried on
///   `systemd-run`'s own environment (#984), after the declared env so an
///   injected secret still overrides a stale declared value.
/// - `--description=` carries the spec fingerprint (#695) so a later
///   [`reconcile`] can tell this unit's spec from the currently declared one.
///
/// A plugin unit deliberately has **no slice**: it is supervised and bound to
/// the session target, which is a stronger relationship than the grouping a
/// slice gives, and adding one would change the pinned argv for no gain.
fn plugin_launch(
    id: &str,
    spec: &PluginSpec,
    extra_env: &[(String, String)],
    target: &str,
) -> Launch {
    Launch {
        unit: systemd::plugin_unit_name(id),
        description: unit_description(id, &spec_fingerprint(spec, target)),
        slice: None,
        properties: vec![
            "Restart=on-failure".to_owned(),
            "RestartSec=2".to_owned(),
            format!("PartOf={target}"),
            format!("TimeoutStopSec={}", launch::PLUGIN_TIMEOUT_STOP),
        ],
        env: spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        secret_env: extra_env.to_vec(),
        argv: vec![spec.exec.clone()],
    }
}

/// Launch one declared plugin as a transient `trollshell-plugin-<id>.service`
/// user unit. `extra_env` is the #392 secret-injection hook (see the module
/// docs); every current caller builds it via [`resolve_secret_env`], and its
/// values reach the unit over `systemd-run`'s inherited environment rather than
/// its argv ([`crate::launch::command`], #984 — which is the *only* way to build
/// the invocation, so a second launch path cannot bypass the pairing however it
/// is written).
///
/// Fails if the unit already exists (the plugin is running — `systemd-run`
/// refuses to replace a live unit) or if there's no reachable user manager;
/// both surface as one logged warning at the call sites, never a crash.
async fn launch(
    id: &str,
    spec: &PluginSpec,
    extra_env: &[(String, String)],
    target: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(systemd::is_valid_plugin_id(id), "invalid plugin id: {id:?}");
    let output = launch::command(
        launch::SYSTEMD_RUN,
        &plugin_launch(id, spec, extra_env, target),
    )
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
    tracing::info!(plugin = %id, exec = %spec.exec, %target, "launched plugin as transient user unit");
    Ok(())
}

/// Read every AI-key slot the plugin opted into ([`PluginSpec::secrets`], #392)
/// from the login keyring and map each to its injected `(<SLOT>_API_KEY, value)`
/// env pair. A slot with no stored key is skipped — the plugin launches keyless
/// and its own fallback (e.g. the pet's canned lines) applies. Secret values are
/// never logged (only the slot, and only on the skip path).
///
/// Every skipped slot is also recorded against `id` so the watcher can pick the
/// plugin up when the key appears (#866, see the module docs); the ones that
/// *did* resolve clear any earlier record, so a relaunch that finally got its key
/// stops being waited on.
async fn resolve_secret_env(id: &str, spec: &PluginSpec) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(spec.secrets.len());
    let mut missing = Vec::new();
    let mut present = Vec::new();
    for slot in &spec.secrets {
        if let Some(value) = crate::secrets::get(slot).await {
            out.push((crate::secrets::env_var_for(slot), value));
            present.push(slot.clone());
        } else {
            tracing::debug!(slot = %slot, "no stored AI key for slot; launching plugin without it");
            missing.push(slot.clone());
        }
    }
    note_resolution(id, &missing, &present);
    out
}

// ── Waiting for a missing secret to appear (#866) ────────────────────────────

/// How often [`watch_outstanding_secrets`] re-probes the slots a plugin launched
/// without.
///
/// What it is waiting for is a human unlocking a keyring or an external tool
/// writing a key — minutes-scale events — and each probe is a Secret Service
/// round trip, so this is deliberately gentle. A `const` and not an option: no
/// session makes 30s the wrong answer, and a knob here would be one nobody could
/// sensibly set.
const SECRET_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How many consecutive relaunch failures [`settle_slot`] tolerates for one
/// `(slot, plugin)` pair before giving up on it (#880).
///
/// #878's F7 fix re-inserts a slot whose relaunch failed, with no cap, so a
/// plugin id that can only ever fail bounces every [`SECRET_POLL_INTERVAL`]
/// for the life of the session, each pass under [`CONVERGE_LOCK`]. That
/// configuration is reachable: an id both declared in `plugins.json` *and*
/// hand-installed as a static unit under the same
/// `trollshell-plugin-<id>.service` name always fails its transient relaunch
/// (`systemd-run` refuses the taken name — see [`restart`]'s doc comment),
/// while the static-unit fallback keeps bringing the plugin back up, so
/// nothing about the failure is transient. A `const`, not an option: no
/// session makes some other retry count the right answer, same reasoning as
/// [`SECRET_POLL_INTERVAL`] above.
const MAX_RELAUNCH_FAILURES: usize = 3;

/// Which declared secret slots are still unresolved, and which plugins are
/// waiting on each: `slot → {plugin id}`. A slot leaves the map when it becomes
/// available *and* every plugin waiting on it relaunched, when every waiter has
/// since launched with it, when the declaration no longer justifies the wait
/// ([`prune_undeclared`]), or when a waiter's relaunch has failed
/// [`MAX_RELAUNCH_FAILURES`] times in a row ([`settle_slot`], #880).
type Outstanding = BTreeMap<String, BTreeSet<String>>;

/// Consecutive relaunch-failure count per `(slot, plugin id)` pair (#880) —
/// see [`MAX_RELAUNCH_FAILURES`]. A pair is absent until its first failure;
/// [`settle_slot`] removes the entry again on a success (or a pass where the
/// id wasn't attempted) and once the cap drops the id, so the map only ever
/// holds pairs currently mid-streak.
type FailureCounts = BTreeMap<(String, String), usize>;

/// The watcher's whole state, behind one **synchronous** mutex.
///
/// `std::sync::Mutex` rather than tokio's, for two reasons that reinforce each
/// other:
///
/// - Nothing here ever awaits while holding it — and because
///   [`watch_outstanding_secrets`] is boxed as `dyn Future + Send`, rustc
///   *enforces* that: a `MutexGuard` (which is `!Send`) held across an `.await`
///   fails to compile rather than deadlocking in production.
/// - [`WatchGuard`]'s `Drop` has to clear [`WatchState::watching`] under the
///   same lock the arm path checks it under, and `Drop` cannot `.await`.
struct WatchState {
    outstanding: Outstanding,
    /// Consecutive relaunch-failure streaks, keyed by `(slot, plugin id)`
    /// (#880). See [`FailureCounts`] and [`MAX_RELAUNCH_FAILURES`].
    failures: FailureCounts,
    /// Whether a [`watch_outstanding_secrets`] task is alive.
    ///
    /// Read and written **only while holding this mutex**, which is what makes
    /// "arm a watcher iff none is running" and "stand down when nothing is left"
    /// race-free against each other. Without that pairing a [`note_resolution`]
    /// landing between the watcher deciding to exit and clearing the flag would
    /// record a slot nobody is watching.
    watching: bool,
}

static WATCH: std::sync::Mutex<WatchState> = std::sync::Mutex::new(WatchState {
    outstanding: BTreeMap::new(),
    failures: BTreeMap::new(),
    watching: false,
});

/// Lock [`WATCH`], recovering from poisoning rather than propagating a panic.
///
/// Nothing in a critical section here can panic on its own, so poisoning would
/// only ever arrive from a panic elsewhere in the same task — and the correct
/// answer to that is still to keep the watcher's bookkeeping working, not to
/// take the shell's plugin launcher down with it.
fn watch_state() -> std::sync::MutexGuard<'static, WatchState> {
    WATCH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Clears [`WatchState::watching`] when the watcher task ends **however it
/// ends** — including a panic or the runtime dropping the future mid-poll.
///
/// Without this a task that died anywhere but its own `return` would leave the
/// flag set, and [`note_resolution`] would never arm a replacement: the feature
/// would be silently, permanently disarmed for the rest of the session.
struct WatchGuard {
    /// Whether this task still owns the flag. The normal-exit path clears the
    /// flag under the lock and unsets this, so the guard cannot then clear a
    /// *successor* watcher's flag; on a panic or cancellation it is still set,
    /// no successor can exist (nothing arms while the flag is true), and the
    /// guard is the only thing that disarms.
    owns_flag: bool,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        if self.owns_flag {
            watch_state().watching = false;
        }
    }
}

/// Fold one plugin's secret resolution into the outstanding set: record the
/// slots that came back empty against `id`, and drop `id` from the ones that
/// resolved (removing a slot entirely once nobody is waiting on it). Returns
/// whether anything is outstanding afterwards — i.e. whether a watcher is
/// wanted. Pure, so the whole bookkeeping is unit-testable.
fn fold_resolution(
    out: &mut Outstanding,
    id: &str,
    missing: &[String],
    present: &[String],
) -> bool {
    for slot in missing {
        out.entry(slot.clone()).or_default().insert(id.to_owned());
    }
    for slot in present {
        if let Some(waiting) = out.get_mut(slot) {
            waiting.remove(id);
            if waiting.is_empty() {
                out.remove(slot);
            }
        }
    }
    !out.is_empty()
}

/// Every slot still being waited on, in a stable order. Pure.
fn outstanding_slots(out: &Outstanding) -> Vec<String> {
    out.keys().cloned().collect()
}

/// The plugin ids currently waiting on `slot`, in a stable order — the log
/// line's subject, read **without** dropping them (see [`settle_slot`]). Pure.
fn waiters_on(out: &Outstanding, slot: &str) -> Vec<String> {
    out.get(slot)
        .map(|ids| ids.iter().cloned().collect())
        .unwrap_or_default()
}

/// Resolve one slot's watch after a relaunch attempt: everything that was
/// waiting stops waiting **except** the ids whose relaunch failed, which stay
/// so the next pass tries again — up to [`MAX_RELAUNCH_FAILURES`] consecutive
/// failures per `(slot, id)` pair (#880). Past the cap the id is dropped
/// instead of retried again, and the returned list says which ids that
/// happened to (with the attempt count and the triggering relaunch's error),
/// for the caller to log.
///
/// This is #866's F7 fix, plus #880's cap. The slot used to be removed
/// *before* the relaunch, so a transiently failing restart — the unit briefly
/// still up, a wedged user manager — lost the slot forever and left the
/// plugin keyless with nothing still watching for it. Writing the failures
/// back is also why this rebuilds the entry rather than retaining in place:
/// by the time it runs, a successful [`restart`] has already dropped its own
/// id via [`fold_resolution`], so a `retain` would have nothing left to keep.
///
/// The failure count resets for any `(slot, id)` pair that *isn't* reported
/// failed this pass — a relaunch that finally succeeded, or an id that was
/// waiting but wasn't attempted this time — read from `out[slot]`'s contents
/// **before** this call clears them, i.e. the same waiter set
/// [`watch_outstanding_secrets`] already relaunched from. Pure.
fn settle_slot(
    out: &mut Outstanding,
    failures: &mut FailureCounts,
    slot: &str,
    failed: &[(String, String)],
) -> Vec<(String, usize, String)> {
    let previously_waiting = out.get(slot).cloned().unwrap_or_default();
    for id in &previously_waiting {
        if !failed.iter().any(|(f, _)| f == id) {
            failures.remove(&(slot.to_owned(), id.clone()));
        }
    }
    out.remove(slot);
    let mut dropped = Vec::new();
    let mut retry = BTreeSet::new();
    for (id, reason) in failed {
        let key = (slot.to_owned(), id.clone());
        let count = {
            let c = failures.entry(key.clone()).or_insert(0);
            *c += 1;
            *c
        };
        if count >= MAX_RELAUNCH_FAILURES {
            failures.remove(&key);
            dropped.push((id.clone(), count, reason.clone()));
        } else {
            retry.insert(id.clone());
        }
    }
    if !retry.is_empty() {
        out.insert(slot.to_owned(), retry);
    }
    dropped
}

/// Drop watch entries the declaration no longer justifies: a plugin that was
/// removed from `plugins.json`, or that no longer lists the slot it was waiting
/// on. Removes any slot left with no waiters.
///
/// #866's F10. Without it a plugin deleted from the config keeps a slot — and
/// therefore the 30s poll — alive for the rest of the session, waiting on a key
/// nothing would consume. Pure.
fn prune_undeclared(out: &mut Outstanding, declared: &BTreeMap<String, PluginSpec>) {
    out.retain(|slot, waiting| {
        waiting.retain(|id| {
            declared
                .get(id)
                .is_some_and(|spec| spec.secrets.iter().any(|s| s == slot))
        });
        !waiting.is_empty()
    });
}

/// Whether a probe means the slot can be injected now — the transition the
/// watcher acts on.
///
/// [`SecretProbe::Locked`] and [`SecretProbe::Absent`] both mean *keep waiting*,
/// and that is the deliberate part: a locked ring may unlock, and an absent key
/// may be written by `secret-tool` or the control-center at any moment. The two
/// are held apart for the log, not for the decision. Pure.
fn is_now_available(probe: SecretProbe) -> bool {
    matches!(probe, SecretProbe::Available)
}

/// Record one plugin's just-resolved secrets, arming the watcher if anything is
/// still missing. The thin edge over [`fold_resolution`].
fn note_resolution(id: &str, missing: &[String], present: &[String]) {
    let mut w = watch_state();
    if !fold_resolution(&mut w.outstanding, id, missing, present) {
        return;
    }
    // Armed under the same lock the watcher stands down under.
    if !w.watching {
        w.watching = true;
        tracing::info!(
            slots = ?outstanding_slots(&w.outstanding),
            interval_s = SECRET_POLL_INTERVAL.as_secs(),
            "a declared secret was unavailable at launch; watching for it",
        );
        hytte::reactive::runtime::handle().spawn(watch_outstanding_secrets());
    }
}

/// A spawnable `Send` future — see [`watch_outstanding_secrets`] for why the
/// watcher needs its type written down rather than inferred.
type SpawnedTask = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Re-probe the outstanding slots until each becomes available, relaunching the
/// plugins that declared it as it does, then stand down. See the module docs.
///
/// # Why this returns a boxed future rather than being an `async fn`
///
/// The call graph is a **cycle**: this task calls [`relaunch_for_secret_inner`]
/// → [`restart`] → [`resolve_secret_env`] → [`note_resolution`], which spawns
/// *this* task. `Handle::spawn` requires `Send`, and rustc cannot *infer* `Send`
/// around such a cycle — it gives up with "cannot satisfy `impl Future: Send`".
/// Naming the type asserts `Send` instead of inferring it, which is what breaks
/// the cycle; the `Box::pin` is simply the price of being able to write the type
/// down. It also buys the [`WatchState`] invariant a compiler check: a `!Send`
/// `MutexGuard` held across an `.await` would fail to build.
fn watch_outstanding_secrets() -> SpawnedTask {
    Box::pin(async move {
        let mut guard = WatchGuard { owns_flag: true };
        loop {
            tokio::time::sleep(SECRET_POLL_INTERVAL).await;
            // Forget plugins the config no longer declares (F10) before deciding
            // whether there is anything left to watch. An unreadable state file
            // prunes nothing — same stance `reconcile` takes.
            let declared = load_declared().await;
            let slots = {
                let mut w = watch_state();
                if let Some(declared) = &declared {
                    prune_undeclared(&mut w.outstanding, &declared.plugins);
                }
                if w.outstanding.is_empty() {
                    // Cleared under the lock, so an arm racing this can't be
                    // lost; the guard then has nothing left to own.
                    w.watching = false;
                    guard.owns_flag = false;
                    tracing::debug!("no secret slots left outstanding; standing down the watcher");
                    return;
                }
                outstanding_slots(&w.outstanding)
            };
            for slot in slots {
                let probe = crate::secrets::probe(&slot).await;
                if !is_now_available(probe) {
                    tracing::debug!(%slot, ?probe, "declared secret still unavailable; still waiting");
                    continue;
                }
                // Read the waiters, do NOT drop them — a failed relaunch has to
                // stay outstanding (F7). `settle_slot` below decides.
                let waiting = { waiters_on(&watch_state().outstanding, &slot) };
                tracing::info!(
                    %slot,
                    plugins = ?waiting,
                    "declared secret became available; relaunching the plugins that declare it",
                );
                let failed = relaunch_for_secret_inner(&slot).await;
                let mut w = watch_state();
                let ws = &mut *w;
                let dropped = settle_slot(&mut ws.outstanding, &mut ws.failures, &slot, &failed);
                drop(w);
                if !failed.is_empty() {
                    let ids: Vec<&str> = failed.iter().map(|(id, _)| id.as_str()).collect();
                    tracing::warn!(
                        %slot,
                        plugins = ?ids,
                        "relaunch failed; keeping the slot under watch for the next pass",
                    );
                }
                // #880: a (slot, id) pair that just hit MAX_RELAUNCH_FAILURES
                // stops being retried — it would otherwise thrash the plugin
                // every SECRET_POLL_INTERVAL for the rest of the session.
                for (id, attempts, error) in dropped {
                    tracing::warn!(
                        plugin = %id,
                        %slot,
                        attempts,
                        %error,
                        "relaunch failed {attempts} times in a row for this secret slot; \
                         giving up on this plugin for the rest of the session — check for an \
                         id both declared in plugins.json and hand-installed as a static \
                         systemd unit (systemd-run then always refuses the name), or rotate \
                         the key from the control-center; either resolves it, and the next \
                         launch or reconcile re-arms watching for it",
                    );
                }
            }
        }
    })
}

/// Whether a systemd `ActiveState` means the unit is already running (or on
/// its way), i.e. a startup launch should skip it.
fn is_running(active_state: &str) -> bool {
    matches!(active_state, "active" | "activating" | "reloading")
}

// ── Reconcile (#695) ─────────────────────────────────────────────────────────

/// Serialises every path that drives a plugin's unit through a **multi-step**
/// transition — [`reconcile`], [`relaunch_for_secret`] and [`start`].
///
/// #866's F6. Before this only `reconcile` was serialised, and `relaunch_for_secret`
/// grew a second caller: the control-center's `SetAiKey`/`ClearAiKey` already
/// fired one, and the watcher now fires another. Those two collide on the
/// feature's *happy path* — saving a key in the control-center is exactly what
/// unlocks the ring, so a Save spawns a relaunch and the watcher's next pass
/// spawns another within 30s. Two concurrent `stop → wait-until-stopped →
/// launch` sequences interleave badly: one's `stop` lands between the other's
/// wait and launch (plugin left down), or both reach `launch` and the loser gets
/// systemd's "unit already exists" → the static-unit fallback in [`restart`] →
/// `Err`, i.e. the bridge simply gone for the session.
///
/// **Held only at the top-level entry points.** [`restart`] and [`stop`] are
/// reached from inside those, and a tokio `Mutex` is not reentrant, so taking it
/// there too would deadlock instantly. `stop` on its own (the Plugins tab's Stop
/// button) is a single call with no window to interleave and stays outside.
static CONVERGE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
/// - A **running unit with no fingerprint** whose id is declared **enabled**
///   is restarted (we can't prove it matches, and converging is the point) —
///   this is the one-time recycle when a pre-#695 shell's units meet a #695
///   shell. An id that is both declared enabled *and* hand-installed as a
///   static unit lands here on every reconcile; [`restart`] documents what
///   that does.
/// - A **running unit with no fingerprint** is otherwise left strictly alone,
///   whether its id is undeclared or declared **disabled**: that is a legacy
///   static unit (or someone else's), and the launcher has never owned it.
///   Only a launcher-stamped unit is ever stopped. The declared-disabled half
///   matters since #1400, whose `availablePlugins` declares every bundled id
///   off by default (#1400 review, finding 4).
fn plan(declared: &Declared, units: &[systemd::PluginUnit]) -> Vec<(String, Action)> {
    let running: BTreeMap<&str, Option<&str>> = units
        .iter()
        .filter(|u| is_running(&u.active_state))
        .map(|u| (u.id.as_str(), parse_fingerprint(&u.description)))
        .collect();
    let mut out: Vec<(String, Action)> = Vec::new();
    for (id, spec) in &declared.plugins {
        match (spec.enabled, running.get(id.as_str())) {
            (true, None) => out.push((id.clone(), Action::Launch)),
            (true, Some(live)) => {
                if *live != Some(spec_fingerprint(spec, &declared.target).as_str()) {
                    out.push((id.clone(), Action::Restart));
                }
            }
            // Stamped: the launcher's own unit, so a disabled plugin stops.
            (false, Some(Some(_))) => out.push((id.clone(), Action::Stop)),
            // Unstamped: a unit this launcher never spawned (a hand-installed
            // static unit for a declared-off id) — left alone, like an
            // undeclared one (#1400 review, finding 4).
            (false, Some(None) | None) => {}
        }
    }
    // Orphans: units this launcher stamped (so it owns them) whose plugin is no
    // longer declared at all — the `plugins.<id>` entry was removed.
    for (id, fingerprint) in &running {
        if fingerprint.is_some() && !declared.plugins.contains_key(*id) {
            out.push(((*id).to_owned(), Action::Stop));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Converge the running plugins onto the declared state (#695): launch what
/// should be running and isn't, stop what shouldn't be, and restart anything
/// running from a superseded spec (changed `env` / `package` / `secrets`).
/// Three callers: shell startup and the `plugins.json` watch after it (both
/// [`launch_at_startup`]'s one task, [`reconcile_then_watch`], #1399), and the
/// `Control.ReloadPlugins` handler, which the home-manager activation script
/// pokes after rewriting `plugins.json` so a switch does not wait for the
/// watch's next tick.
///
/// Best-effort throughout: every per-plugin failure is logged, never propagated
/// — one broken plugin must not stop the rest from converging. Serialized on
/// [`CONVERGE_LOCK`], so a reconcile racing another (activation firing twice, a
/// poke landing on the same change a watch tick saw, or either landing while
/// startup is still running) queues instead of interleaving a stop with the
/// other's launch; the second then re-reads the state file and converges on
/// whatever is current.
///
/// Nothing awaits a poke's result (the handler spawns this and returns), so
/// a pass that could not list the live units ([`Outcome::Unlisted`]: it
/// launched blind) raises [`RELOAD_UNLISTED`], and the watch's next tick
/// makes the real pass (#1404).
pub async fn reconcile() {
    if let Outcome::Unlisted { .. } = reconcile_from(Sources::from_env(), Trigger::OneShot).await {
        RELOAD_UNLISTED.store(true, Ordering::SeqCst);
    }
}

/// Raised by a `Control.ReloadPlugins` pass ([`reconcile`]) that could not
/// list the live units and so launched blind (#1404). The stops and restarts
/// it could not see are still owed, so the watch takes the flag on its next
/// tick and reconciles as though its own pass had failed
/// ([`converge_then_watch`]).
///
/// A flag the running watch reads, rather than a retry the poke runs itself:
/// - **One owner of retries.** The watch already retries its own failed
///   passes every tick and warns once per outage. A poke retrying on its own
///   would be a second loop beside it, retrying the same outage on its own
///   cadence with its own log lines.
/// - **Every retry is an ordinary watch pass**, under [`CONVERGE_LOCK`] like
///   any other. The flag only decides whether the next tick runs one, and it
///   folds into the watch's own pending state, so a tick that owes both a
///   failed watch pass and a failed poke still runs one reconcile, not two.
/// - **Taken before the pass, never cleared after one.** The watch swaps it
///   to `false` at the top of a tick and then reconciles, so a poke that
///   fails after the swap raises it again for the tick after: a failure is
///   never credited to a pass that started before it. Clearing it once a
///   pass settled would race a poke failing in between and lose that
///   failure, and a poke that settles leaves it alone for the same reason.
///
/// What that costs is at most one redundant pass, in one corner: a watch pass
/// already queued on the lock behind the failed poke lists the units and
/// applies everything, and the next tick still takes the flag and reconciles
/// once more. That pass finds nothing owed; like any spurious reconcile, all
/// it can do is relaunch an effectively-enabled plugin that is not running.
/// Closing the corner would take the flag raised and cleared under the lock,
/// inside [`reconcile_listing`], by whichever pass lists: more machinery than
/// one serialised extra pass is worth.
static RELOAD_UNLISTED: AtomicBool = AtomicBool::new(false);

/// Who asked for a reconcile, which decides what a failed unit listing means
/// (#1399 review, finding 4; #1404).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    /// Shell startup, or a `Control.ReloadPlugins` poke. A failed listing
    /// launches blind, as it always has: the empty live set can only plan
    /// launches, never a stop of something unseen, each launch surfaces its
    /// own error, and the plugins come up while the user manager is briefly
    /// unreachable. The pass still reports [`Outcome::Unlisted`], because the
    /// stops and restarts it could not see are owed, and the watch makes the
    /// real pass on its next tick (#1404): startup's outcome starts the watch
    /// with the retry pending, and a poke's raises [`RELOAD_UNLISTED`].
    OneShot,
    /// A `plugins.json` watch tick, or its retry. A failed listing acts on
    /// **nothing** and reports [`Outcome::Unlisted`], and the watch retries
    /// on its next tick. It does not launch blind: that retry comes every
    /// tick for as long as the outage lasts, and a blind pass on each would
    /// re-run `systemd-run` for every enabled plugin every few seconds (the
    /// running ones answering "unit already exists" each time). A one-shot
    /// pass launches blind once and leaves the rest to this retry.
    Watch,
}

impl Trigger {
    /// Whether a failed listing still goes ahead with an empty live set. Pure.
    fn launches_blind(self) -> bool {
        matches!(self, Self::OneShot)
    }
}

/// What one [`reconcile_from`] made of its attempt. The watch reads it for
/// startup's pass and its own, and [`reconcile`] reads it to hand a poke's
/// failed listing to the watch.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use]
enum Outcome {
    /// Nothing for a retry to do. It planned against the live units, or the
    /// file was unreadable or unparsable, which leaves the plugins alone and
    /// which a later save (a moved stamp), not a retry, is what fixes.
    Settled,
    /// The pass could not list the live units, so the stops and restarts it
    /// should have made are missing: a [`Trigger::Watch`] pass acted on
    /// nothing, and a [`Trigger::OneShot`] pass launched blind. Either way
    /// the watch reconciles again on its next tick.
    Unlisted {
        /// The listing error, for the watch's log line.
        error: String,
    },
}

/// [`reconcile`] over explicit [`Sources`] rather than the process
/// environment. Owned rather than borrowed so the watch can hand out one
/// `'static` future per change (see [`reconcile_then_watch`]).
///
/// Only a failed **listing** is ever reported back for a retry (see
/// [`Trigger`]). A single plugin's launch, stop or restart that fails is
/// logged and left alone, whoever triggered the pass. Retrying those
/// unboundedly is #880's bug: a plugin that can only ever fail (an id both
/// declared and hand-installed as a static unit) bouncing every tick for the
/// rest of the session. The next change, poke or shell start tries it again.
async fn reconcile_from(sources: Sources, trigger: Trigger) -> Outcome {
    reconcile_listing(sources, trigger, systemd::list_plugin_units).await
}

/// [`reconcile_from`] with the unit listing passed in. Production always
/// passes [`systemd::list_plugin_units`]; the seam exists because that call
/// fails only without a reachable user manager, and what a failed listing
/// returns for each [`Trigger`] is what the #1404 retry runs on.
async fn reconcile_listing<L, Fut>(sources: Sources, trigger: Trigger, list_units: L) -> Outcome
where
    L: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<Vec<systemd::PluginUnit>>>,
{
    let _guard = CONVERGE_LOCK.lock().await;

    let Some(declared) = load_declared_from(&sources).await else {
        // Unreadable/unparsable state file — leave the running set alone.
        return Outcome::Settled;
    };
    // One list call up front beats racing systemd-run's "unit already exists"
    // error per plugin — and it carries the fingerprints the diff runs on.
    let (units, outcome) = match list_units().await {
        Ok(units) => (units, Outcome::Settled),
        Err(err) if trigger.launches_blind() => {
            // No reachable user manager: fall through with an empty live set,
            // which can only ever plan launches (each of which surfaces its own
            // error) — never a stop of something we failed to see. What it
            // cannot see is still owed, so the pass reports it (#1404).
            tracing::warn!(
                %err,
                "listing plugin units failed; launching blind, and reconciling again on the watch's next tick",
            );
            (
                Vec::new(),
                Outcome::Unlisted {
                    error: err.to_string(),
                },
            )
        }
        // The watch logs this itself, once per outage rather than per tick.
        Err(err) => {
            return Outcome::Unlisted {
                error: err.to_string(),
            };
        }
    };
    let actions = plan(&declared, &units);
    // No early return: once the listing is answered this fn has one exit, so
    // the test that drives a failed listing (an empty plan) pins the same
    // `outcome` a blind pass with launches to make returns (#1407 review,
    // finding 1).
    if actions.is_empty() && outcome == Outcome::Settled {
        tracing::debug!(
            declared = declared.plugins.len(),
            target = %declared.target,
            "plugins already match the declared state"
        );
    }
    // Per-plugin failures below are logged, not reported back: see this
    // fn's doc for why a retry here would be #880's bounce loop.
    for (id, action) in actions {
        match action {
            Action::Stop => {
                tracing::info!(plugin = %id, "no longer declared as enabled; stopping");
                if let Err(err) = stop(&id).await {
                    tracing::warn!(plugin = %id, %err, "stopping the plugin failed");
                }
            }
            Action::Restart => {
                let Some(spec) = declared.plugins.get(&id) else {
                    continue;
                };
                tracing::info!(plugin = %id, exec = %spec.exec, "declared spec changed; restarting");
                if let Err(err) = restart(&id, spec, &declared.target).await {
                    tracing::warn!(plugin = %id, %err, "restarting the plugin failed");
                }
            }
            Action::Launch => {
                let Some(spec) = declared.plugins.get(&id) else {
                    continue;
                };
                let extra_env = resolve_secret_env(&id, spec).await;
                if let Err(err) = launch(&id, spec, &extra_env, &declared.target).await {
                    tracing::warn!(plugin = %id, %err, "plugin launch failed");
                }
            }
        }
    }
    outcome
}

/// Kick off the startup reconcile, and the `plugins.json` watch behind it
/// (#1399), as one supervised task on the shared tokio runtime. Called once
/// from `main.rs`'s run body; guarded so a re-fired `activate` (a second
/// `trollshell` invocation remote-activating the primary instance) can't
/// double-launch, and — because the watch is the same task — can't start a
/// second watch either.
pub fn launch_at_startup() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let sources = Sources::from_env();
    hytte::reactive::spawn_supervised("plugins-json", move || {
        reconcile_then_watch(sources.clone(), watch::POLL_INTERVAL)
    });
}

// ── Watching the state file (#1399) ──────────────────────────────────────────

/// The production task: [`converge_then_watch`] over the `plugins.json`
/// candidates in `sources`, with [`reconcile_from`] over all of `sources` as
/// the thing it runs and [`RELOAD_UNLISTED`] as the flag a failed
/// `ReloadPlugins` pass raises for it (#1404).
///
/// Its whole job is that hand-over, and it is a function of its own so a
/// test can drive exactly what [`launch_at_startup`] spawns: a generic loop
/// tested with a counting stand-in still ships inert if the real call site
/// hands it something else. The test,
/// `the_production_task_hands_reconcile_to_the_loop`, runs this against an
/// unparsable scratch file, which is the one input `reconcile_from` answers
/// without reaching a user manager, and counts the warnings it leaves: one
/// at startup, one after the file changes, and one after it raises the flag.
///
/// Only the config half is watched. The switch's `plugins.toml` is read on
/// every reconcile but never stamped: its only writer is [`set_enabled_in`],
/// behind a switch that starts or stops the plugin itself right after (see
/// [`Overrides`]).
fn reconcile_then_watch(
    sources: Sources,
    cadence: Duration,
) -> impl Future<Output = ()> + Send + 'static {
    let watched = sources.config.clone();
    converge_then_watch(watched, cadence, &RELOAD_UNLISTED, move |trigger| {
        reconcile_from(sources.clone(), trigger)
    })
}

/// Stamp every candidate path, run `converge` once — startup's reconcile —
/// then re-stamp every `cadence` and run `converge` again whenever a stamp
/// moved. Never returns. See the module doc's "Watching the state file".
///
/// The baseline is taken **here, before** the first `converge`, and not by the
/// caller: as two statements at a call site the order is a rule nothing
/// enforces (`hytte-config`'s #1040 V2, which is where a swap of exactly this
/// pair was measured losing an edit forever). Every later tick re-stamps
/// **before** it converges too, for the same reason. Generic over `converge`
/// so the loop's timing is testable on a paused clock with no systemd
/// anywhere; [`reconcile_then_watch`] is what pins the production argument.
///
/// A watch reconcile that could not list the live units
/// ([`Outcome::Unlisted`]) leaves the change **pending**, and the next tick
/// reconciles again whether or not a stamp moved, until one attempt settles.
/// That is a flag here, not a re-stamp delayed until after the converge,
/// which would fold an edit landing during the converge into the stamps and
/// lose it. The first failure of an outage warns and the repeats are
/// `debug`, so a dead user manager costs one journal line, not one every
/// tick.
///
/// The one-shot passes feed the same pending state (#1404). Startup's own
/// pass is [`Trigger::OneShot`]: it launches blind as it always has, and an
/// [`Outcome::Unlisted`] from it starts the loop with the retry already
/// pending, so the first tick makes the real pass. `reload_unlisted` is the
/// same hand-over for a `Control.ReloadPlugins` pass, whose outcome nobody
/// awaits ([`RELOAD_UNLISTED`] in production, which says why it is a flag).
/// Every tick takes it before its pass and folds it into `pending`, so a
/// poke's failure costs one pass on the next tick, shares the outage's one
/// warning (the blind pass already logged it), and never adds a second pass
/// to a tick that owes one anyway.
async fn converge_then_watch<C, F>(
    paths: Vec<PathBuf>,
    cadence: Duration,
    reload_unlisted: &'static AtomicBool,
    mut converge: C,
) where
    C: FnMut(Trigger) -> F,
    F: Future<Output = Outcome>,
{
    let mut seen = watch::stamps_of(&paths);
    let mut pending = matches!(converge(Trigger::OneShot).await, Outcome::Unlisted { .. });
    loop {
        tokio::time::sleep(cadence).await;
        let moved = moved_since(&paths, &mut seen);
        // Taken before the pass, never cleared after it: see RELOAD_UNLISTED.
        pending |= reload_unlisted.swap(false, Ordering::SeqCst);
        if moved.is_empty() {
            if !pending {
                continue;
            }
            tracing::debug!("retrying the reconcile a failed unit listing left pending");
        } else {
            tracing::info!(paths = ?moved, "plugins.json changed; reconciling the declared plugins");
        }
        match converge(Trigger::Watch).await {
            Outcome::Settled => {
                if pending {
                    tracing::info!(
                        "listing plugin units works again; the pending change is applied"
                    );
                }
                pending = false;
            }
            Outcome::Unlisted { error } => {
                if pending {
                    tracing::debug!(%error, "listing plugin units still fails; retrying next tick");
                } else {
                    tracing::warn!(
                        %error,
                        "listing plugin units failed; acted on nothing and will retry every tick until it works",
                    );
                }
                pending = true;
            }
        }
    }
}

/// Re-stamp `paths` and return the ones whose stamp differs from `seen`,
/// replacing `seen` with the fresh stamps **unconditionally** — so a change is
/// reported once, on the tick that first sees it, whatever the reconcile after
/// it makes of the file.
///
/// A path that appeared or vanished counts as moved (its stamp goes from or to
/// `None`); that is how a higher-precedence file showing up over a lower one
/// fires the watch although no existing file's bytes changed.
fn moved_since<'a>(paths: &'a [PathBuf], seen: &mut Vec<Stamp>) -> Vec<&'a Path> {
    let now = watch::stamps_of(paths);
    let moved = paths
        .iter()
        .zip(now.iter().zip(seen.iter()))
        .filter(|(_, (now, then))| now != then)
        .map(|(path, _)| path.as_path())
        .collect();
    *seen = now;
    moved
}

// ── Control surface (the #348 Plugins tab, via control.rs) ───────────────────

/// The plugins the control-center lists: the *declared* set (state file) ∪ the
/// `trollshell-plugin-*` units systemd knows (transient runs + legacy static
/// units). For a declared plugin the effective `enabled` flag wins — nix's,
/// or the switch's persisted override (#1400); a transient unit has no unit
/// file, so systemd would report it `disabled` —
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
    merge_declared(units, &declared.plugins)
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
/// unit ([`launch()`] — `--collect` already released any failed previous run);
/// an undeclared id falls back to `StartUnit` for a legacy static unit.
///
/// Takes [`CONVERGE_LOCK`] so a human clicking Start cannot land inside a
/// concurrent relaunch's stop→launch window and lose the race to systemd's "unit
/// already exists" (#866's F6). It calls neither `reconcile` nor `restart`, so
/// there is no reentrancy.
///
/// # Errors
/// Unknown/invalid id, a still-running unit, or an unreachable user manager.
pub async fn start(id: &str) -> anyhow::Result<()> {
    let _guard = CONVERGE_LOCK.lock().await;
    let declared = load_declared().await.unwrap_or_default();
    match declared.plugins.get(id) {
        Some(spec) => {
            let extra_env = resolve_secret_env(id, spec).await;
            launch(id, spec, &extra_env, &declared.target).await
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

/// Persist plugin `id`'s auto-start state — the first half of the Plugins
/// tab's switch, before its `StartPlugin`/`StopPlugin` (#1400), so a refusal
/// here starts or stops nothing.
///
/// - A **declared** plugin nix leaves free (`enable` unset, or
///   `lib.mkDefault`): the choice is kept in `$XDG_STATE_HOME/trollshell/
///   plugins.toml` as a difference from the declared value ([`Overrides`]),
///   and every reconcile, listing and watch tick after it reads the folded
///   value ([`load_declared_from`]), so it survives a shell restart and a
///   rebuild that leaves the declaration alone.
/// - A **declared** plugin nix pins (`enable` assigned plainly, or with
///   `lib.mkForce`): an error naming `programs.trollshell.plugins.<id>.enable`,
///   and nothing is written.
/// - An **undeclared** id: unit-file `Enable/DisableUnitFiles`, for legacy
///   static units.
///
/// # Errors
/// A pinned plugin; an unreadable `plugins.json`; no state directory to
/// persist into (neither `$XDG_STATE_HOME` nor `$HOME` set) or a failed write;
/// on the legacy path, an invalid id or an unreachable user manager.
pub async fn set_enabled(id: &str, enabled: bool) -> anyhow::Result<()> {
    set_enabled_in(&Sources::from_env(), id, enabled).await
}

/// [`set_enabled`] over explicit [`Sources`], so a test can drive it against
/// scratch files.
///
/// The declared-plugin arms run under [`CONVERGE_LOCK`]: the override file is
/// a read-modify-write, so two switches flipped in quick succession must not
/// lose one of the two, and a reconcile must not read the file between the
/// two halves of either. The legacy arm drops the lock before its D-Bus call;
/// it touches nothing a reconcile reads.
async fn set_enabled_in(sources: &Sources, id: &str, enabled: bool) -> anyhow::Result<()> {
    let guard = CONVERGE_LOCK.lock().await;
    // Nix's own declaration, not the effective one: an override is stored
    // relative to what nix declares.
    let Some(nix) = load_nix_declared(&sources.config).await else {
        anyhow::bail!(
            "plugins.json exists but cannot be read; not persisting plugin {id}'s switch \
             (the journal names the file)"
        );
    };
    let declared = match persist_decision(&nix, id) {
        Persist::UnitFile => {
            drop(guard);
            return systemd::set_plugin_enabled(id, enabled).await;
        }
        Persist::Pinned => return Err(pinned_error(id)),
        Persist::Override { declared } => declared,
    };
    let path = sources.overrides.as_deref().with_context(|| {
        format!("cannot persist plugin {id}'s switch: neither $XDG_STATE_HOME nor $HOME is set")
    })?;
    let mut overrides = read_overrides(Some(path));
    if record_override(&mut overrides, id, declared, enabled) {
        write_overrides(path, &overrides).with_context(|| format!("writing {}", path.display()))?;
        tracing::info!(
            plugin = %id,
            enabled,
            declared,
            path = %path.display(),
            "Plugins tab switch persisted (#1400)"
        );
    }
    Ok(())
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
///
/// Serialised on [`CONVERGE_LOCK`] (#866's F6): this now has two callers that
/// collide on the happy path, and two interleaved `stop → wait → launch`
/// sequences can leave a plugin down for the session.
pub async fn relaunch_for_secret(slot: &str) {
    let failed = relaunch_for_secret_inner(slot).await;
    if !failed.is_empty() {
        let ids: Vec<&str> = failed.iter().map(|(id, _)| id.as_str()).collect();
        tracing::warn!(%slot, plugins = ?ids, "some plugins did not relaunch after the key change");
    }
}

/// [`relaunch_for_secret`]'s body, reporting **which plugin ids failed to come
/// back**, each paired with its relaunch error.
///
/// The watcher needs the id list to keep those slots outstanding rather than
/// dropping them on a transient failure (#866's F7), and the error alongside
/// each one to log a reason when #880's [`MAX_RELAUNCH_FAILURES`] cap gives up
/// on a pair; the two control-center callers only want the log line, which the
/// public wrapper above writes. Ids that were simply *not running* are not
/// failures — they pick the key up on their next start — so they are not
/// reported.
///
/// A failure to even list the units reports **every** affected id against that
/// same listing error: nothing was attempted, so nothing should stop being
/// watched.
async fn relaunch_for_secret_inner(slot: &str) -> Vec<(String, String)> {
    let _guard = CONVERGE_LOCK.lock().await;

    let declared = load_declared().await.unwrap_or_default();
    let affected: Vec<(&String, &PluginSpec)> = declared
        .plugins
        .iter()
        .filter(|(_, spec)| spec.secrets.iter().any(|s| s == slot))
        .collect();
    if affected.is_empty() {
        tracing::debug!(%slot, "no declared plugin uses this secret slot; nothing to relaunch");
        return Vec::new();
    }
    let running: HashSet<String> = match systemd::list_plugin_units().await {
        Ok(units) => units
            .into_iter()
            .filter(|u| is_running(&u.active_state))
            .map(|u| u.id)
            .collect(),
        Err(err) => {
            tracing::warn!(%err, %slot, "listing plugin units for relaunch failed; skipping");
            let reason = err.to_string();
            return affected
                .into_iter()
                .map(|(id, _)| (id.clone(), reason.clone()))
                .collect();
        }
    };
    let mut failed = Vec::new();
    for (id, spec) in affected {
        if !running.contains(id) {
            tracing::debug!(plugin = %id, %slot, "not running; new key applies on next start");
            continue;
        }
        if let Err(err) = restart(id, spec, &declared.target).await {
            tracing::warn!(plugin = %id, %slot, %err, "relaunch after key change failed");
            failed.push((id.clone(), err.to_string()));
        } else {
            tracing::info!(plugin = %id, %slot, "relaunched to apply the changed AI key");
        }
    }
    failed
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
async fn restart(id: &str, spec: &PluginSpec, target: &str) -> anyhow::Result<()> {
    stop(id).await?;
    wait_until_stopped(id).await;
    let extra_env = resolve_secret_env(id, spec).await;
    let Err(err) = launch(id, spec, &extra_env, target).await else {
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
            locked: Vec::new(),
        }
    }

    /// [`spec`], pinned by nix (`_locked = ["enabled"]`, #1400).
    fn pinned(exec: &str, enabled: bool) -> PluginSpec {
        PluginSpec {
            locked: vec![LOCKED_ENABLED.to_owned()],
            ..spec(exec, enabled)
        }
    }

    /// [`Sources`] naming only `plugins.json` candidates — no override file,
    /// so nothing is folded in: what every test that is about nix's half
    /// alone wants.
    fn config_only(paths: &[PathBuf]) -> Sources {
        Sources {
            config: paths.to_vec(),
            overrides: None,
        }
    }

    /// A parsed state file with no `"target"` — what every pre-#707 module (and
    /// today's NixOS module) writes.
    fn state(plugins: BTreeMap<String, PluginSpec>) -> PluginState {
        PluginState {
            plugins,
            target: None,
        }
    }

    /// The fingerprint on the default session target, which is what every test
    /// that isn't specifically about the target (#707) wants.
    fn fp(spec: &PluginSpec) -> String {
        spec_fingerprint(spec, DEFAULT_TARGET)
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
        let plugins = parse_state(json).unwrap().plugins;
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
        let plugins = parse_state(json).unwrap().plugins;
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
        let out = sanitize(state(plugins));
        assert_eq!(out.plugins["p"].secrets, vec!["openrouter".to_owned()]);
    }

    #[test]
    fn parse_defaults_enabled_true_and_ignores_unknown_fields() {
        // A minimal entry: only exec. `enabled` defaults true, unknown fields
        // (future schema additions) are ignored rather than erroring.
        let json = r#"{ "plugins": { "demo": { "exec": "/bin/demo", "future": 42 } } }"#;
        let plugins = parse_state(json).unwrap().plugins;
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
        assert!(parse_state("{}").unwrap().plugins.is_empty());
        assert!(
            parse_state(r#"{ "version": 1 }"#)
                .unwrap()
                .plugins
                .is_empty()
        );
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
        let out = sanitize(state(plugins));
        assert_eq!(out.plugins.keys().collect::<Vec<_>>(), vec!["ok-plugin"]);
    }

    #[test]
    fn sanitize_drops_env_keys_that_break_setenv() {
        let mut plugins = BTreeMap::new();
        let mut s = spec("/bin/ok", true);
        s.env.insert("GOOD".to_owned(), "v".to_owned());
        s.env.insert("BAD=KEY".to_owned(), "v".to_owned());
        s.env.insert(String::new(), "v".to_owned());
        plugins.insert("p".to_owned(), s);
        let out = sanitize(state(plugins));
        assert_eq!(
            out.plugins["p"].env.keys().collect::<Vec<_>>(),
            vec!["GOOD"]
        );
    }

    // ── The session target (#707) ────────────────────────────────────────────

    #[test]
    fn parse_reads_the_session_target_and_defaults_it_when_absent() {
        // The whole of #707's backward compatibility: a state file written by a
        // pre-#707 module (or by the NixOS module, which has no such option)
        // carries no "target" and must still launch onto the old default.
        let old = r#"{ "version": 1, "plugins": { "demo": { "exec": "/bin/demo" } } }"#;
        assert_eq!(parse_state(old).unwrap().target, None);
        assert_eq!(sanitize(parse_state(old).unwrap()).target, DEFAULT_TARGET);

        // …and a #707 module renders `systemd.target` into it.
        let new = r#"{
            "version": 1,
            "target": "niri-session.target",
            "plugins": { "demo": { "exec": "/bin/demo" } }
        }"#;
        assert_eq!(
            sanitize(parse_state(new).unwrap()).target,
            "niri-session.target"
        );
    }

    #[test]
    fn sanitize_falls_back_to_the_default_target_for_a_value_that_is_not_a_unit_name() {
        // A hand-edited file must degrade to the default rather than to a unit
        // systemd refuses to load — the value lands verbatim in `--property=
        // PartOf=…`.
        for bad in [
            "",                         // empty
            "graphical session.target", // a space
            "target;rm -rf",            // shell-ish punctuation
            "tar\nget.target",          // a newline
        ] {
            let out = sanitize(PluginState {
                plugins: BTreeMap::new(),
                target: Some(bad.to_owned()),
            });
            assert_eq!(out.target, DEFAULT_TARGET, "{bad:?}");
        }
        // The names that do occur are accepted, including a templated one.
        for ok in [
            "graphical-session.target",
            "niri-session.target",
            "my_session@seat0.target",
        ] {
            let out = sanitize(PluginState {
                plugins: BTreeMap::new(),
                target: Some(ok.to_owned()),
            });
            assert_eq!(out.target, ok);
        }
    }

    #[test]
    fn systemd_run_args_bind_partof_to_the_declared_target() {
        // The defect (#707): the transient unit used to hardcode
        // `PartOf=graphical-session.target` while the shell's own unit bound to
        // the configurable target, so the two came down out of step.
        let s = spec("/bin/demo", true);
        let args = run_argv("demo", &s, &[], "niri-session.target");
        assert!(
            args.contains(&"--property=PartOf=niri-session.target".to_owned()),
            "{args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|a| a == "--property=PartOf=graphical-session.target"),
            "{args:?}"
        );
    }

    #[test]
    fn a_changed_target_recycles_the_running_plugins_but_the_default_does_not() {
        let s = spec("/bin/demo", true);
        // The default canonicalizes as "absent", so a default-configured
        // session's digest is byte-identical to the pre-#707 one — upgrading
        // must not bounce every plugin once for a value that didn't change.
        assert_eq!(spec_fingerprint(&s, DEFAULT_TARGET), "963338d3f7f67d63");
        // A real change does show up, which is what makes the reconcile relaunch
        // the unit with the right PartOf=.
        assert_ne!(
            spec_fingerprint(&s, "niri-session.target"),
            spec_fingerprint(&s, DEFAULT_TARGET)
        );

        // …and end to end through `plan`: same declared spec, different target,
        // against a unit stamped on the default → Restart.
        let d = Declared {
            plugins: [("demo".to_owned(), s.clone())].into_iter().collect(),
            target: "niri-session.target".to_owned(),
        };
        let units = vec![systemd::PluginUnit {
            id: "demo".to_owned(),
            active_state: "active".to_owned(),
            enabled: true,
            description: unit_description("demo", &fp(&s)),
        }];
        assert_eq!(
            plan(&d, &units),
            vec![("demo".to_owned(), Action::Restart)],
            "a changed session target must recycle the plugin"
        );
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

    // ── Watching the state file (#1399) ──────────────────────────────────────

    use hytte_config::test_support::{capture, scratch_home};
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// The one mtime every nix-store file carries, `1970-01-01 00:00:01` —
    /// what both nix modules' `plugins.json` resolves to through its symlinks.
    const STORE_MTIME: Duration = Duration::from_secs(1);

    /// Cadence for the paused-clock loop tests. Any value works, since the
    /// clock only moves when a test says so; a round one keeps the arithmetic
    /// readable.
    const TICK: Duration = Duration::from_secs(3);

    /// The `reload_unlisted` flag for every loop test that is not about a
    /// `ReloadPlugins` pass (#1404): nothing ever raises it. A test that
    /// raises one declares its own, so parallel tests never share one.
    static NO_RELOAD: AtomicBool = AtomicBool::new(false);

    /// Two `plugins.json` bodies differing only in the store hash inside
    /// `exec`, which is what a package bump changes: same length, different
    /// bytes.
    const SPEC_A: &str = r#"{"plugins":{"pet":{"exec":"/nix/store/aaaaaaaa-pet/bin/pet"}}}"#;
    const SPEC_B: &str = r#"{"plugins":{"pet":{"exec":"/nix/store/bbbbbbbb-pet/bin/pet"}}}"#;

    /// Write `body` to `path` the way a nix-store file looks: mtime pinned to
    /// [`STORE_MTIME`], whatever the wall clock says. Creates parent dirs.
    fn write_store_file(path: &Path, body: &str) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        std::fs::write(path, body).expect("write");
        std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open")
            .set_modified(std::time::SystemTime::UNIX_EPOCH + STORE_MTIME)
            .expect("set mtime");
    }

    /// A `converge` stand-in that only counts its calls, each of which settles.
    fn counting(
        runs: Arc<AtomicUsize>,
    ) -> impl FnMut(Trigger) -> std::future::Ready<Outcome> + Send {
        move |_| {
            runs.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Outcome::Settled)
        }
    }

    /// Move the paused clock by `by`, then let the woken loop run: `advance`
    /// only marks a sleep ready, and nothing polls it until the next `.await`.
    async fn tick(by: Duration) {
        tokio::time::advance(by).await;
        tokio::task::yield_now().await;
    }

    /// **A store-shaped rewrite moves the stamp**: same mtime, same length,
    /// different bytes — a package bump under either nix module.
    ///
    /// Red if the stamp the launcher uses goes back to `(mtime, len)`
    /// (`hytte-config`'s `watch::stamp`), or if the launcher grows its own
    /// stamp of that shape: both halves of it are identical here, by
    /// construction and by the precondition asserts.
    #[test]
    fn a_store_shaped_rewrite_moves_the_stamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        let paths = vec![path.clone()];
        write_store_file(&path, SPEC_A);
        let mut seen = watch::stamps_of(&paths);

        write_store_file(&path, SPEC_B);

        let meta = std::fs::metadata(&path).expect("stat");
        assert_eq!(
            meta.modified().expect("mtime"),
            std::time::SystemTime::UNIX_EPOCH + STORE_MTIME,
            "precondition: the rewrite kept the store mtime"
        );
        assert_eq!(
            usize::try_from(meta.len()).expect("small"),
            SPEC_A.len(),
            "precondition: the rewrite kept the length"
        );
        assert_eq!(
            moved_since(&paths, &mut seen),
            vec![path.as_path()],
            "a store-hash bump inside `exec` must move the stamp"
        );
        assert!(
            moved_since(&paths, &mut seen).is_empty(),
            "and it is reported once: `seen` took the new stamp"
        );
    }

    /// **The loop**: startup converges once, a store-shaped rewrite converges
    /// exactly once one interval later, and the quiet ticks after that
    /// converge nothing.
    ///
    /// Red if the loop's `converge().await` after a move is deleted (the
    /// second count never arrives), if the stamps stop updating on a move
    /// (every later tick fires again), if the stamp stops seeing a
    /// same-length same-mtime rewrite, or if the loop waits longer than one
    /// `cadence` (the rewrite comes straight after startup, so a doubled
    /// sleep has not woken by the first tick).
    #[tokio::test(start_paused = true)]
    async fn a_rewrite_converges_exactly_once_one_interval_later() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        write_store_file(&path, SPEC_A);
        let runs = Arc::new(AtomicUsize::new(0));

        let task = tokio::spawn(converge_then_watch(
            vec![path.clone()],
            TICK,
            &NO_RELOAD,
            counting(runs.clone()),
        ));
        tokio::task::yield_now().await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "startup's own converge, once"
        );

        write_store_file(&path, SPEC_B);
        tick(TICK).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "the rewrite converges one interval later"
        );

        tick(TICK).await;
        tick(TICK).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "and only once: later quiet ticks converge nothing"
        );
        task.abort();
    }

    /// **No reconcile on the first observation**: with nothing changed,
    /// startup's converge is the only one, however many ticks go by.
    ///
    /// Red if the baseline is not a real stamp of the files — e.g. every
    /// path starting as "absent", which makes the first tick see each
    /// existing file "appear" and reconcile a second time for nothing.
    #[tokio::test(start_paused = true)]
    async fn the_first_observation_is_not_a_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        write_store_file(&path, SPEC_A);
        let runs = Arc::new(AtomicUsize::new(0));

        let task = tokio::spawn(converge_then_watch(
            vec![path.clone()],
            TICK,
            &NO_RELOAD,
            counting(runs.clone()),
        ));
        tokio::task::yield_now().await;
        for _ in 0..3 {
            tick(TICK).await;
        }
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "startup's converge covers the first observation"
        );
        task.abort();
    }

    /// **Stamp, then load** (#1040 V2): an edit that lands after the baseline
    /// was taken but while startup's converge is still running is picked up by
    /// the next tick, not folded into the baseline and lost.
    ///
    /// The edit is made *by* the first converge, which is the only way to put
    /// it strictly between the two. Red if [`converge_then_watch`] takes its
    /// baseline after the first `converge().await` instead of before: the
    /// baseline then already holds the edit and no tick ever fires.
    #[tokio::test(start_paused = true)]
    async fn an_edit_landing_during_startups_reconcile_is_picked_up_next_tick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        write_store_file(&path, SPEC_A);
        let runs = Arc::new(AtomicUsize::new(0));

        let converge = {
            let runs = runs.clone();
            let path = path.clone();
            move |_| {
                if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                    // A `nixos-rebuild switch` finishing mid-startup.
                    write_store_file(&path, SPEC_B);
                }
                std::future::ready(Outcome::Settled)
            }
        };
        let task = tokio::spawn(converge_then_watch(
            vec![path.clone()],
            TICK,
            &NO_RELOAD,
            converge,
        ));
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "startup's own converge");

        tick(TICK).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "the edit made during startup's converge must reach the next tick"
        );
        task.abort();
    }

    /// **Every candidate path is watched**: a higher-precedence file appearing
    /// over a lower one fires the watch, though the lower one never changed.
    ///
    /// The new file carries the *same bytes* as the one it shadows, so the
    /// only thing that moved is which path exists. Red if the watch stamps
    /// only the path that won at startup (the lower one here), and red if it
    /// stamps the winner's *content* instead of every path, since the
    /// content, the length and the mtime of the winner are all unchanged.
    #[tokio::test(start_paused = true)]
    async fn a_higher_precedence_file_appearing_fires_the_watch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let high = dir.path().join("home/trollshell/plugins.json");
        let low = dir.path().join("etc/xdg/trollshell/plugins.json");
        write_store_file(&low, SPEC_A);
        let runs = Arc::new(AtomicUsize::new(0));

        let task = tokio::spawn(converge_then_watch(
            vec![high.clone(), low.clone()],
            TICK,
            &NO_RELOAD,
            counting(runs.clone()),
        ));
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "startup's own converge");

        write_store_file(&high, SPEC_A);
        tick(TICK).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "a home-manager file appearing over the /etc/xdg one must converge"
        );

        tick(TICK).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "once");
        task.abort();
    }

    // The next four tests are the #1402 adversarial review's (findings 2, 1,
    // 3 and 6), ported with the seam's `Trigger`/`Outcome` added.

    /// **A file vanishing fires the watch too**: removing the last plugin
    /// makes both modules delete `plugins.json`, and a home-manager file
    /// going away reveals the `/etc/xdg` one below it. Either way the only
    /// thing that moved is a path going from present to absent.
    ///
    /// Red if a disappearance stops counting as a move (a stamp filter that
    /// only looks at files that exist now), which no test above can see:
    /// every one of them only ever makes a file appear or change.
    #[tokio::test(start_paused = true)]
    async fn a_vanishing_file_fires_the_watch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let high = dir.path().join("home/trollshell/plugins.json");
        let low = dir.path().join("etc/xdg/trollshell/plugins.json");
        write_store_file(&high, SPEC_B);
        write_store_file(&low, SPEC_A);
        let runs = Arc::new(AtomicUsize::new(0));

        let task = tokio::spawn(converge_then_watch(
            vec![high.clone(), low.clone()],
            TICK,
            &NO_RELOAD,
            counting(runs.clone()),
        ));
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "startup's own converge");

        std::fs::remove_file(&high).expect("rm");
        tick(TICK).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "the shadowing file going away must converge onto the one below"
        );

        std::fs::remove_file(&low).expect("rm");
        tick(TICK).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            3,
            "and the last file going away must converge onto nothing declared"
        );

        tick(TICK).await;
        assert_eq!(runs.load(Ordering::SeqCst), 3, "once each");
        task.abort();
    }

    /// **Stamp, then load, on every change — not only at startup**: an edit
    /// that lands while a *watch-triggered* reconcile is running is picked up
    /// by the tick after it, not folded into the stamps and lost.
    ///
    /// Red if the loop re-stamps after its `converge(..).await` instead of
    /// before it (compare without updating `seen`, converge, then
    /// `seen = stamps_of(..)`): the post-converge stamp already holds the
    /// edit, so no later tick ever sees it move. The startup test above
    /// cannot see that swap, because it only pins where the *baseline* is
    /// taken. It is also the tempting way to retry a failed reconcile, which
    /// is why the retry is a pending flag instead.
    #[tokio::test(start_paused = true)]
    async fn an_edit_landing_during_a_watch_reconcile_is_picked_up_next_tick() {
        const SPEC_C: &str = r#"{"plugins":{"pet":{"exec":"/nix/store/cccccccc-pet/bin/pet"}}}"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        write_store_file(&path, SPEC_A);
        let runs = Arc::new(AtomicUsize::new(0));

        let converge = {
            let runs = runs.clone();
            let path = path.clone();
            move |_| {
                if runs.fetch_add(1, Ordering::SeqCst) == 1 {
                    // A second switch finishing while the first one's
                    // reconcile is still running.
                    write_store_file(&path, SPEC_C);
                }
                std::future::ready(Outcome::Settled)
            }
        };
        let task = tokio::spawn(converge_then_watch(
            vec![path.clone()],
            TICK,
            &NO_RELOAD,
            converge,
        ));
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "startup's own converge");

        write_store_file(&path, SPEC_B);
        tick(TICK).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the first change");

        tick(TICK).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            3,
            "the edit made during the watch's own reconcile must reach the next tick"
        );
        task.abort();
    }

    /// **What a save in progress reads as.** Since #1399 a tick can land in
    /// the middle of a truncate-then-write save (`>`, `cp`, an editor that
    /// writes in place), so the empty and the half-written file must both read
    /// as "unparsable, leave the plugins alone" (`None`) — never as "nothing
    /// declared", which would stop every launched plugin and relaunch them
    /// all on the next tick. Only a file *absent* from every candidate path
    /// means nothing is declared; an absent (or dangling) higher-precedence
    /// file falls through to the one below.
    ///
    /// Red if an empty or whitespace-only file is ever treated as "no
    /// plugins" (a tolerant `json.trim().is_empty()` arm), if a dangling
    /// symlink stops the search instead of falling through, or if the
    /// all-absent answer stops being `Some(empty)`.
    #[tokio::test]
    async fn a_save_in_progress_never_reads_as_nothing_declared() {
        let dir = tempfile::tempdir().expect("tempdir");
        let high = dir.path().join("home/trollshell/plugins.json");
        let low = dir.path().join("etc/xdg/trollshell/plugins.json");
        let paths = vec![high.clone(), low.clone()];
        write_store_file(&low, SPEC_A);

        for partial in [
            "",
            "\n",
            &SPEC_B[..SPEC_B.len() / 2],
            &SPEC_B[..SPEC_B.len() - 1],
        ] {
            write_store_file(&high, partial);
            assert!(
                load_declared_from(&config_only(&paths)).await.is_none(),
                "{partial:?} must read as unparsable, not as nothing declared"
            );
        }

        std::fs::remove_file(&high).expect("rm");
        std::os::unix::fs::symlink(dir.path().join("gc-collected"), &high).expect("symlink");
        let fell_through = load_declared_from(&config_only(&paths))
            .await
            .expect("a dangling link falls through to the file below");
        assert_eq!(
            fell_through.plugins["pet"].exec,
            "/nix/store/aaaaaaaa-pet/bin/pet"
        );

        std::fs::remove_file(&high).expect("rm");
        std::fs::remove_file(&low).expect("rm");
        let nothing = load_declared_from(&config_only(&paths))
            .await
            .expect("absent everywhere is an answer");
        assert!(
            nothing.plugins.is_empty(),
            "absent everywhere = nothing declared"
        );
    }

    /// **What `launch_at_startup` spawns.** Its two statements are the one
    /// place the watch is wired into the running shell, and nothing drives
    /// them (the global runtime, the real `XDG_*`, a real user manager), so
    /// this holds the body to its shape by source instead.
    ///
    /// Red if it goes back to spawning a bare startup `reconcile()` (every
    /// other test in this module stays green: they drive
    /// `reconcile_then_watch` directly), drops supervision, or polls at
    /// anything but `hytte-config`'s interval.
    #[test]
    fn launch_at_startup_spawns_the_supervised_watch() {
        let src = include_str!("plugin_launcher.rs");
        let start = src
            .find("pub fn launch_at_startup()")
            .expect("launch_at_startup is defined");
        let len = src[start..].find("\n}\n").expect("its body ends");
        let body = &src[start..start + len];
        for needle in [
            "spawn_supervised(",
            "reconcile_then_watch(",
            "watch::POLL_INTERVAL",
        ] {
            assert!(body.contains(needle), "{needle} missing from:\n{body}");
        }
    }

    // ── A reconcile that could not list the units (review finding 4) ─────────

    /// A `converge` stand-in that records the [`Trigger`] of every call and
    /// answers each from `outcomes` in turn, settling once they run out.
    fn scripted(
        calls: Arc<std::sync::Mutex<Vec<Trigger>>>,
        outcomes: Vec<Outcome>,
    ) -> impl FnMut(Trigger) -> std::future::Ready<Outcome> + Send {
        let mut outcomes = outcomes.into_iter();
        move |trigger| {
            calls.lock().expect("calls").push(trigger);
            std::future::ready(outcomes.next().unwrap_or(Outcome::Settled))
        }
    }

    fn unlisted() -> Outcome {
        Outcome::Unlisted {
            error: "no user manager".to_owned(),
        }
    }

    /// **A watch reconcile that could not list the units is retried on the
    /// next tick, and once it settles, never again.** The stamps already took
    /// the change before that reconcile ran, so without the pending flag no
    /// later tick would ever apply it: its stops and restarts would be gone
    /// until the file changed again.
    ///
    /// Also pins the triggers: startup is [`Trigger::OneShot`] (it keeps
    /// launching blind), and every watch pass is [`Trigger::Watch`].
    ///
    /// Red if the pending flag is dropped (no retry: two calls, not three),
    /// if a settled retry leaves it set (a retry every tick forever), or if
    /// the loop hands the wrong trigger to either pass.
    #[tokio::test(start_paused = true)]
    async fn a_watch_reconcile_that_could_not_list_is_retried_once_next_tick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        write_store_file(&path, SPEC_A);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        // Startup settles, the change's reconcile cannot list, the retry
        // settles.
        let converge = scripted(
            calls.clone(),
            vec![Outcome::Settled, unlisted(), Outcome::Settled],
        );
        let task = tokio::spawn(converge_then_watch(
            vec![path.clone()],
            TICK,
            &NO_RELOAD,
            converge,
        ));
        tokio::task::yield_now().await;

        write_store_file(&path, SPEC_B);
        tick(TICK).await;
        assert_eq!(
            *calls.lock().expect("calls"),
            [Trigger::OneShot, Trigger::Watch],
            "startup, then the change"
        );

        tick(TICK).await;
        assert_eq!(
            *calls.lock().expect("calls"),
            [Trigger::OneShot, Trigger::Watch, Trigger::Watch],
            "nothing moved, but the change is still pending: retried"
        );

        tick(TICK).await;
        tick(TICK).await;
        assert_eq!(
            calls.lock().expect("calls").len(),
            3,
            "the retry settled: nothing pending, nothing more"
        );
        task.abort();
    }

    /// **An outage costs one warning, however long it lasts**, while the
    /// retries go on every tick underneath it.
    ///
    /// Red if every failed retry warns (a dead user manager would put a line
    /// in the journal every 3 s), or if the retries stop.
    #[tokio::test(start_paused = true)]
    async fn a_listing_that_keeps_failing_warns_once_and_retries_every_tick() {
        let (captured, _guard) = capture();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        write_store_file(&path, SPEC_A);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let converge = scripted(
            calls.clone(),
            vec![
                Outcome::Settled,
                unlisted(),
                unlisted(),
                unlisted(),
                unlisted(),
            ],
        );
        let task = tokio::spawn(converge_then_watch(
            vec![path.clone()],
            TICK,
            &NO_RELOAD,
            converge,
        ));
        tokio::task::yield_now().await;

        write_store_file(&path, SPEC_B);
        for _ in 0..4 {
            tick(TICK).await;
        }
        assert_eq!(
            calls.lock().expect("calls").len(),
            5,
            "startup, the change, and a retry on each of the three ticks after"
        );
        let warned = captured
            .warnings()
            .iter()
            .filter(|m| m.contains("listing plugin units failed"))
            .count();
        assert_eq!(warned, 1, "one warning for the whole outage");
        task.abort();
    }

    // ── A one-shot pass that could not list the units (#1404) ────────────────

    /// The warnings a loop test's `converge` stand-ins could never have
    /// logged themselves: whatever the loop says about a failed listing.
    fn listing_warnings(captured: &hytte_config::test_support::Captured) -> usize {
        captured
            .warnings()
            .iter()
            .filter(|m| m.contains("listing plugin units"))
            .count()
    }

    /// **A startup reconcile that could not list the units gets its real
    /// pass on the first tick, and exactly one.** Startup launched blind,
    /// which can only plan launches, so a stop or restart it should have
    /// made is owed although no stamp will ever move for it.
    ///
    /// Red if the loop starts with nothing pending whatever startup returned
    /// (the retry never comes: one call, not two), or if a settled retry
    /// leaves it pending (a retry every tick).
    #[tokio::test(start_paused = true)]
    async fn a_startup_reconcile_that_could_not_list_is_retried_once_next_tick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        write_store_file(&path, SPEC_A);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        // Startup launches blind, the retry settles.
        let converge = scripted(calls.clone(), vec![unlisted(), Outcome::Settled]);
        let task = tokio::spawn(converge_then_watch(
            vec![path.clone()],
            TICK,
            &NO_RELOAD,
            converge,
        ));
        tokio::task::yield_now().await;
        assert_eq!(*calls.lock().expect("calls"), [Trigger::OneShot], "startup");

        tick(TICK).await;
        assert_eq!(
            *calls.lock().expect("calls"),
            [Trigger::OneShot, Trigger::Watch],
            "nothing moved, but startup's stops and restarts are owed: retried on the first tick"
        );

        for _ in 0..3 {
            tick(TICK).await;
        }
        assert_eq!(
            calls.lock().expect("calls").len(),
            2,
            "the retry settled: nothing pending, nothing more"
        );
        task.abort();
    }

    /// **A startup reconcile that settled owes nothing**: no retry, however
    /// many ticks go by.
    ///
    /// Red if the loop starts with the retry pending whatever startup
    /// returned.
    #[tokio::test(start_paused = true)]
    async fn a_startup_reconcile_that_settled_is_not_retried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        write_store_file(&path, SPEC_A);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let converge = scripted(calls.clone(), vec![Outcome::Settled]);
        let task = tokio::spawn(converge_then_watch(
            vec![path.clone()],
            TICK,
            &NO_RELOAD,
            converge,
        ));
        tokio::task::yield_now().await;
        for _ in 0..3 {
            tick(TICK).await;
        }
        assert_eq!(
            *calls.lock().expect("calls"),
            [Trigger::OneShot],
            "startup settled: nothing owed"
        );
        task.abort();
    }

    /// **A startup outage costs the loop no warning of its own**, while the
    /// retries go on every tick until one settles. The blind pass already
    /// logged the outage (`reconcile_listing`'s one-shot arm), so the loop's
    /// failed retries are repeats of it.
    ///
    /// Red if the retries stop before one settles, or if the loop treats the
    /// first failed retry of a startup outage as a new outage and warns.
    #[tokio::test(start_paused = true)]
    async fn a_startup_outage_is_retried_every_tick_without_a_second_warning() {
        let (captured, _guard) = capture();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        write_store_file(&path, SPEC_A);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let converge = scripted(
            calls.clone(),
            vec![unlisted(), unlisted(), unlisted(), Outcome::Settled],
        );
        let task = tokio::spawn(converge_then_watch(
            vec![path.clone()],
            TICK,
            &NO_RELOAD,
            converge,
        ));
        tokio::task::yield_now().await;
        for _ in 0..5 {
            tick(TICK).await;
        }
        assert_eq!(
            *calls.lock().expect("calls"),
            [
                Trigger::OneShot,
                Trigger::Watch,
                Trigger::Watch,
                Trigger::Watch
            ],
            "startup, then a retry every tick until one settles, then nothing"
        );
        assert_eq!(
            listing_warnings(&captured),
            0,
            "the blind pass logged this outage; the loop adds nothing"
        );
        task.abort();
    }

    /// **A `ReloadPlugins` pass that could not list the units is retried by
    /// the watch on its next tick.** The poke's own pass launched blind and
    /// nothing awaits its outcome, so [`reconcile`] raises the flag the
    /// loop was handed (`RELOAD_UNLISTED` in production, which
    /// `the_production_task_hands_reconcile_to_the_loop` pins). This raises
    /// it by hand, exactly as `reconcile` does with an `Unlisted`.
    ///
    /// The retry fails once before it settles, which pins that the flag
    /// joins the loop's pending state: the retry's own failure is the poke's
    /// outage again, so it warns nothing and is retried in turn.
    ///
    /// Red if the loop never reads the flag (no retry: one call, not three),
    /// if it reads the flag without taking it (a retry every tick forever),
    /// or if the flag runs a pass without marking it pending (the failed
    /// retry warns).
    #[tokio::test(start_paused = true)]
    async fn a_reload_that_could_not_list_is_retried_next_tick() {
        static POKED: AtomicBool = AtomicBool::new(false);
        let (captured, _guard) = capture();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        write_store_file(&path, SPEC_A);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        // Startup settles, the poke's retry cannot list, the next settles.
        let converge = scripted(
            calls.clone(),
            vec![Outcome::Settled, unlisted(), Outcome::Settled],
        );
        let task = tokio::spawn(converge_then_watch(
            vec![path.clone()],
            TICK,
            &POKED,
            converge,
        ));
        tokio::task::yield_now().await;
        tick(TICK).await;
        assert_eq!(
            *calls.lock().expect("calls"),
            [Trigger::OneShot],
            "control: nothing moved and nothing was poked"
        );

        // What `reconcile` does when its one-shot pass comes back Unlisted.
        POKED.store(true, Ordering::SeqCst);
        tick(TICK).await;
        assert_eq!(
            *calls.lock().expect("calls"),
            [Trigger::OneShot, Trigger::Watch],
            "the poke's stops and restarts are owed: retried on the next tick"
        );
        assert!(!POKED.load(Ordering::SeqCst), "and the tick took the flag");

        tick(TICK).await;
        assert_eq!(
            *calls.lock().expect("calls"),
            [Trigger::OneShot, Trigger::Watch, Trigger::Watch],
            "that retry could not list either: retried again"
        );

        for _ in 0..3 {
            tick(TICK).await;
        }
        assert_eq!(
            calls.lock().expect("calls").len(),
            3,
            "the retry settled: nothing pending, nothing more"
        );
        assert_eq!(
            listing_warnings(&captured),
            0,
            "the poke's blind pass logged this outage; the loop adds nothing"
        );
        task.abort();
    }

    /// **A flag raised while a watch pass runs belongs to the tick after**
    /// (#1407 review, finding 4). A `ReloadPlugins` pass that could not list
    /// raises the flag only once it has dropped [`CONVERGE_LOCK`], i.e. while
    /// the watch pass queued behind it may already be running. That tick took
    /// the flag *before* its pass, so whatever lands during the pass is the
    /// next tick's to take. The stand-in raises it from inside the pass,
    /// which is that interleaving.
    ///
    /// Red if the loop clears the flag once a pass settles, or reads it
    /// before the pass and clears it after (`load` at the top,
    /// `store(false)` at the bottom). [`RELOAD_UNLISTED`]'s doc rules both
    /// out ("taken before the pass, never cleared after one"), and no other
    /// test raises the flag while a pass is running.
    #[tokio::test(start_paused = true)]
    async fn a_reload_failing_during_a_watch_pass_is_retried_the_tick_after() {
        static POKED: AtomicBool = AtomicBool::new(false);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        write_store_file(&path, SPEC_A);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let converge = {
            let calls = calls.clone();
            move |trigger: Trigger| {
                let mut seen = calls.lock().expect("calls");
                seen.push(trigger);
                if seen.len() == 2 {
                    // The change's pass: a poke's failure lands while it runs.
                    POKED.store(true, Ordering::SeqCst);
                }
                std::future::ready(Outcome::Settled)
            }
        };
        let task = tokio::spawn(converge_then_watch(
            vec![path.clone()],
            TICK,
            &POKED,
            converge,
        ));
        tokio::task::yield_now().await;

        write_store_file(&path, SPEC_B);
        tick(TICK).await;
        assert_eq!(
            calls.lock().expect("calls").len(),
            2,
            "startup, then the change"
        );

        tick(TICK).await;
        assert_eq!(
            *calls.lock().expect("calls"),
            [Trigger::OneShot, Trigger::Watch, Trigger::Watch],
            "the failure raised during that pass is retried on the next tick"
        );
        tick(TICK).await;
        tick(TICK).await;
        assert_eq!(calls.lock().expect("calls").len(), 3, "once");
        assert!(!POKED.load(Ordering::SeqCst), "and the flag was taken");
        task.abort();
    }

    /// **What a failed listing returns, per trigger**, through the real
    /// reconcile with only the listing faked. Both triggers report
    /// [`Outcome::Unlisted`], since both leave the stops and restarts owed,
    /// and that answer is all the startup and `ReloadPlugins` retries run on
    /// (#1404): before it, a one-shot pass that launched blind reported
    /// `Settled`, and no test above could tell, because they all script the
    /// outcome. Only the one-shot pass launches blind first, and says so. A
    /// listing that works settles.
    ///
    /// Hermetic by construction: the scratch `plugins.json` declares one
    /// plugin, off, and there is no override file, so the plan against any
    /// live set here is empty and no pass reaches `systemd-run` or the
    /// keyring. The rail below checks that before anything runs. An empty
    /// plan is still the path a real outage takes, because
    /// `reconcile_listing` has one exit after the listing whatever the plan
    /// holds; a blind pass that launches something returns the same
    /// `outcome` this one does (#1407 review, finding 1).
    ///
    /// Red if a one-shot pass that launched blind reports `Settled` again
    /// (from its blind arm or from that one exit), if a watch pass launches
    /// blind, or if a watch pass that could not list reports `Settled`.
    #[tokio::test]
    async fn a_failed_listing_is_owed_for_every_trigger_and_only_a_one_shot_launches_blind() {
        const OFF: &str =
            r#"{"plugins":{"pet":{"exec":"/nix/store/aaaaaaaa-pet/bin/pet","enabled":false}}}"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugins.json");
        write_store_file(&path, OFF);
        let sources = config_only(&[path]);
        let declared = load_declared_from(&sources).await.expect("parses");
        assert!(
            declared.plugins.contains_key("pet") && plan(&declared, &[]).is_empty(),
            "rail: one plugin declared, and nothing here may launch it"
        );

        let (captured, _guard) = capture();
        let blind = || {
            captured
                .warnings()
                .iter()
                .filter(|m| m.contains("launching blind"))
                .count()
        };
        let failing = || async { Err(anyhow::anyhow!("no user manager")) };
        for (trigger, launches_blind) in [(Trigger::OneShot, 1), (Trigger::Watch, 0)] {
            let before = blind();
            assert_eq!(
                reconcile_listing(sources.clone(), trigger, failing).await,
                Outcome::Unlisted {
                    error: "no user manager".to_owned()
                },
                "{trigger:?}: what it could not see is owed"
            );
            assert_eq!(
                blind() - before,
                launches_blind,
                "{trigger:?}: blind passes"
            );
        }

        let listed = || async { Ok(Vec::new()) };
        for trigger in [Trigger::OneShot, Trigger::Watch] {
            assert_eq!(
                reconcile_listing(sources.clone(), trigger, listed).await,
                Outcome::Settled,
                "{trigger:?}: a pass that listed owes nothing"
            );
        }
    }

    /// **What production hands the seam** (#1407 review, finding 3).
    /// `reconcile_from` is the one place the real unit listing and the
    /// caller's trigger reach [`reconcile_listing`]. Every test that gets as
    /// far as the listing fakes it, and the production-task test stops at an
    /// unparsable file before any listing, so this is held by source, on
    /// `launch_at_startup_spawns_the_supervised_watch`'s precedent.
    ///
    /// Red if `reconcile_from` hands the seam a stub listing (every pass
    /// planning against nothing: launches only, never a stop or restart) or
    /// a fixed trigger, or if [`reconcile_then_watch`] stops forwarding the
    /// loop's trigger (every watch retry launching blind, or startup never
    /// doing so).
    #[test]
    fn production_hands_the_seam_the_real_listing_and_the_callers_trigger() {
        let src = include_str!("plugin_launcher.rs");
        let prod = &src[..src.find("#[cfg(test)]\nmod tests").expect("tests module")];
        let body = |sig: &str| {
            let start = prod.find(sig).unwrap_or_else(|| panic!("{sig} is defined"));
            let len = prod[start..].find("\n}\n").expect("its body ends");
            &prod[start..start + len]
        };
        let from = body("async fn reconcile_from(");
        assert!(
            from.contains("reconcile_listing(sources, trigger, systemd::list_plugin_units)"),
            "reconcile_from must hand the seam the real listing and its own trigger:\n{from}"
        );
        let task = body("fn reconcile_then_watch(");
        assert!(
            task.contains("move |trigger|")
                && task.contains("reconcile_from(sources.clone(), trigger)"),
            "reconcile_then_watch must forward the loop's trigger:\n{task}"
        );
    }

    /// **What [`reconcile`] does with its outcome.** It is the
    /// `ReloadPlugins` entry point and reads the real environment. Its
    /// `Unlisted` arm needs a failing listing, i.e. the real user manager, so
    /// that half is held by source, on
    /// `launch_at_startup_spawns_the_supervised_watch`'s precedent: it runs
    /// the one-shot pass and raises [`RELOAD_UNLISTED`] when that pass could
    /// not list. `a_reload_that_settled_raises_nothing` drives the other half,
    /// `a_reload_that_could_not_list_is_retried_next_tick` pins what the watch
    /// does with the flag, and `the_production_task_hands_reconcile_to_the_loop`
    /// that the production task watches this flag.
    ///
    /// Red if the poke goes back to discarding its outcome, or raises some
    /// other flag.
    #[test]
    fn reconcile_hands_a_failed_listing_to_the_watch() {
        let src = include_str!("plugin_launcher.rs");
        let start = src
            .find("pub async fn reconcile()")
            .expect("reconcile is defined");
        let len = src[start..].find("\n}\n").expect("its body ends");
        let body = &src[start..start + len];
        for needle in [
            "reconcile_from(",
            "Trigger::OneShot",
            "Outcome::Unlisted",
            "RELOAD_UNLISTED.store(true",
        ] {
            assert!(body.contains(needle), "{needle} missing from:\n{body}");
        }
    }

    /// **A poke that settled raises nothing** (#1407 review, finding 2). The
    /// driven half of `reconcile_hands_a_failed_listing_to_the_watch`:
    /// [`reconcile`] itself, against an unparsable `plugins.json` under
    /// [`scratch_home`], which is the one input it answers (`Settled`)
    /// without reaching a user manager. `scratch_home`'s `temp_env` lock
    /// serialises this against `the_production_task_hands_reconcile_to_the_loop`,
    /// the one other test that touches [`RELOAD_UNLISTED`].
    ///
    /// Red if `reconcile()` raises the flag whatever its pass returned, or on
    /// the wrong outcome (`!matches!(…, Outcome::Unlisted { .. })`). The
    /// source scan's four needles are still all there in both.
    #[test]
    fn a_reload_that_settled_raises_nothing() {
        scratch_home(|home| {
            let path = home.join(".config").join(STATE_FILE_REL);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, "{ this is not json").expect("write");
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async {
                assert!(
                    load_declared().await.is_none(),
                    "rail: the environment must resolve to the unparsable scratch file"
                );
                RELOAD_UNLISTED.store(false, Ordering::SeqCst);
                reconcile().await;
                assert!(
                    !RELOAD_UNLISTED.load(Ordering::SeqCst),
                    "a poke that settled owes the watch nothing"
                );
            });
        });
    }

    /// Which callers launch blind when the listing fails: the one-shot ones,
    /// which run once and leave the rest to the watch's retry (#1404), and
    /// never a watch tick, whose retry comes every tick.
    ///
    /// Red if the two answers are swapped or merged — a watch tick launching
    /// blind would re-run `systemd-run` for every enabled plugin on every
    /// tick of an outage.
    #[test]
    fn only_a_one_shot_reconcile_launches_blind() {
        assert!(Trigger::OneShot.launches_blind());
        assert!(!Trigger::Watch.launches_blind());
    }

    /// **The wiring**: what [`launch_at_startup`] spawns really runs
    /// [`reconcile_from`] — at startup, again when the file moves, and again
    /// when a failed `ReloadPlugins` pass raises [`RELOAD_UNLISTED`] (#1404).
    ///
    /// The loop tests above drive [`converge_then_watch`] with a counting
    /// stand-in, which says nothing about what production hands it. This
    /// drives [`reconcile_then_watch`] itself, against a scratch file that
    /// never parses: the one input `reconcile_from` answers by logging
    /// "unparsable" and returning *before* it lists or touches a single unit.
    /// Every body this test writes must stay unparsable, and the rail below
    /// checks that before anything runs — a parsable file here would send the
    /// reconcile on to the developer's real user manager.
    ///
    /// Belt and braces, the whole test runs under [`scratch_home`] with the
    /// file at the scratch `$HOME`'s own `plugins.json`, so even a refactor
    /// that swapped `reconcile_from(sources)` for the environment-reading
    /// `reconcile()` would read this file and stop in the same place.
    ///
    /// Red if [`reconcile_then_watch`] hands the loop anything but
    /// `reconcile_from` (a no-op: not one warning ever arrives), or any flag
    /// but `RELOAD_UNLISTED` (the third never arrives).
    #[test]
    fn the_production_task_hands_reconcile_to_the_loop() {
        scratch_home(|home| {
            let path = home.join(".config").join(STATE_FILE_REL);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, "{ this is not json").expect("write");
            let paths = vec![path.clone()];
            // The override half resolves under the scratch home too (#1400):
            // `scratch_home` clears `$XDG_STATE_HOME`, so this is
            // `<scratch>/.local/state/trollshell/plugins.toml`.
            let sources = Sources {
                config: paths.clone(),
                overrides: hytte_config::state::path(OVERRIDES_SUBSYSTEM),
            };
            assert!(
                sources
                    .overrides
                    .as_deref()
                    .is_some_and(|p| p.starts_with(home)),
                "rail: the override file must resolve under the scratch home, got {:?}",
                sources.overrides
            );

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let (captured, _guard) = capture();
                // The rail: both routes to the file must refuse it.
                assert!(
                    load_declared_from(&sources).await.is_none(),
                    "rail: the scratch file must not parse"
                );
                assert!(
                    load_declared().await.is_none(),
                    "rail: the environment must resolve to the scratch file too"
                );
                let shown = path.display().to_string();
                let unparsable = || {
                    captured
                        .events()
                        .iter()
                        .filter(|e| {
                            e.level == tracing::Level::WARN
                                && e.message.contains("unparsable")
                                && e.fields.get("path") == Some(&shown)
                        })
                        .count()
                };
                let before = unparsable();
                assert_eq!(before, 2, "the rail's own two reads, live control");

                let settles = |want: usize| async move {
                    for _ in 0..1000 {
                        if unparsable() >= want {
                            return true;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    false
                };
                let reached = tokio::select! {
                    () = reconcile_then_watch(sources.clone(), Duration::from_millis(10)) => false,
                    reached = async {
                        if !settles(before + 1).await {
                            return false;
                        }
                        // Different bytes, still not JSON.
                        std::fs::write(&path, "{ still not json, either").expect("rewrite");
                        if !settles(before + 2).await {
                            return false;
                        }
                        // What `reconcile` does when a poke's pass could not
                        // list (#1404). No other test touches this flag.
                        RELOAD_UNLISTED.store(true, Ordering::SeqCst);
                        if !settles(before + 3).await {
                            return false;
                        }
                        // Ten quiet ticks: the flag was taken, not left up.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        unparsable() == before + 3
                    } => reached,
                };
                assert!(
                    reached,
                    "startup, one change and one raised flag must each run the real reconcile \
                     once; saw {} (want 3)",
                    unparsable() - before
                );
                assert!(
                    !RELOAD_UNLISTED.load(Ordering::SeqCst),
                    "the production task took the flag"
                );
            });
        });
    }

    // ── The Plugins tab's persisted switch (#1400) ───────────────────────────

    /// Option C's rule, every row of it: nix's value when nix pins it, else
    /// the switch's override, else nix's value.
    ///
    /// Red if a pin stops winning (the two starred rows), if an override
    /// stops applying to a free plugin, or if "no override" stops meaning
    /// the declared value.
    #[test]
    fn effective_enabled_is_option_cs_truth_table() {
        // (declared, pinned, override, effective)
        let rows = [
            (false, false, None, false),
            (false, false, Some(false), false),
            (false, false, Some(true), true),
            (true, false, None, true),
            (true, false, Some(false), false),
            (true, false, Some(true), true),
            (false, true, None, false),
            (false, true, Some(false), false),
            (false, true, Some(true), false), // * a pin ignores the switch
            (true, true, None, true),
            (true, true, Some(false), true), // * a pin ignores the switch
            (true, true, Some(true), true),
        ];
        for (declared, pinned, overridden, want) in rows {
            assert_eq!(
                effective_enabled(declared, pinned, overridden),
                want,
                "declared {declared}, pinned {pinned}, override {overridden:?}"
            );
        }
    }

    /// The fold over a whole declaration: a free plugin takes its override in
    /// either direction, a pinned one keeps nix's value, a free one with no
    /// override keeps nix's value, and an override naming an id nix no longer
    /// declares brings nothing back.
    #[test]
    fn fold_overrides_skips_pinned_and_undeclared_ids() {
        let mut d = declared(&[
            ("timer", spec("/bin/timer", false)),
            ("pet", spec("/bin/pet", true)),
            ("niri-layouts", pinned("/bin/niri", true)),
            ("weather", spec("/bin/weather", false)),
        ]);
        let overrides = Overrides {
            enabled: [
                ("timer".to_owned(), true),
                ("pet".to_owned(), false),
                ("niri-layouts".to_owned(), false),
                ("ghost".to_owned(), true),
            ]
            .into_iter()
            .collect(),
        };
        fold_overrides(&mut d, &overrides);
        let enabled: BTreeMap<&str, bool> = d
            .plugins
            .iter()
            .map(|(id, s)| (id.as_str(), s.enabled))
            .collect();
        assert_eq!(
            enabled,
            [
                ("niri-layouts", true),
                ("pet", false),
                ("timer", true),
                ("weather", false),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
            "an undeclared id must not be declared by its override"
        );
    }

    /// Only differences are stored, in both directions, and agreeing with nix
    /// deletes the entry. The return value is whether a write is needed.
    #[test]
    fn record_override_stores_only_differences() {
        let mut o = Overrides::default();
        assert!(record_override(&mut o, "timer", false, true), "on over off");
        assert!(!record_override(&mut o, "timer", false, true), "same again");
        assert!(record_override(&mut o, "pet", true, false), "off over on");
        assert_eq!(o.enabled.get("timer"), Some(&true));
        assert_eq!(o.enabled.get("pet"), Some(&false));

        assert!(
            record_override(&mut o, "timer", false, false),
            "back to nix"
        );
        assert!(!o.enabled.contains_key("timer"), "the entry is deleted");
        assert!(
            !record_override(&mut o, "timer", false, false),
            "nothing left"
        );
        assert!(
            !record_override(&mut o, "clock", true, true),
            "agreeing with nix never writes an entry"
        );
        assert_eq!(o.enabled.len(), 1);
    }

    /// `_locked` is read off each entry, only `"enabled"` pins the switch, and
    /// an entry without it — every unpinned one, and every pre-#1400 file —
    /// pins nothing.
    #[test]
    fn parse_reads_the_locked_marker_and_defaults_it_absent() {
        let json = r#"{
            "version": 1,
            "plugins": {
                "niri-layouts": { "exec": "/bin/niri", "enabled": true, "_locked": ["enabled"] },
                "timer": { "exec": "/bin/timer", "enabled": false },
                "later": { "exec": "/bin/later", "enabled": false, "_locked": ["some-future-key"] }
            }
        }"#;
        let plugins = sanitize(parse_state(json).expect("parses")).plugins;
        assert!(plugins["niri-layouts"].enable_locked());
        assert!(!plugins["timer"].enable_locked());
        assert!(
            !plugins["later"].enable_locked(),
            "a marker naming another key does not pin enabled"
        );
    }

    /// Which way `set_enabled` goes, decided from nix's declaration alone.
    #[test]
    fn persist_decision_routes_each_kind_of_id() {
        let nix = declared(&[
            ("timer", spec("/bin/timer", false)),
            ("pet", spec("/bin/pet", true)),
            ("niri-layouts", pinned("/bin/niri", true)),
        ]);
        assert_eq!(persist_decision(&nix, "legacy"), Persist::UnitFile);
        assert_eq!(persist_decision(&nix, "niri-layouts"), Persist::Pinned);
        assert_eq!(
            persist_decision(&nix, "timer"),
            Persist::Override { declared: false }
        );
        assert_eq!(
            persist_decision(&nix, "pet"),
            Persist::Override { declared: true }
        );
    }

    /// A scratch `plugins.json` + `plugins.toml` pair: `(dir, sources, the
    /// override file's path)`. The state file lives under the tempdir, never
    /// under the real `$XDG_STATE_HOME`.
    fn scratch_sources(plugins_json: &str) -> (tempfile::TempDir, Sources, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = dir.path().join("config/trollshell/plugins.json");
        std::fs::create_dir_all(json.parent().expect("parent")).expect("mkdir");
        std::fs::write(&json, plugins_json).expect("write plugins.json");
        let toml = dir.path().join("state/trollshell/plugins.toml");
        let sources = Sources {
            config: vec![json],
            overrides: Some(toml.clone()),
        };
        (dir, sources, toml)
    }

    /// Two free plugins: `timer` declared off (the default), `pet` declared
    /// on (`lib.mkDefault true`).
    const TWO_FREE: &str = r#"{"version":1,"plugins":{
        "timer":{"exec":"/bin/timer","enabled":false},
        "pet":{"exec":"/bin/pet","enabled":true}
    }}"#;

    /// Every effective `enabled` the launcher would act on, by id.
    async fn effective(sources: &Sources) -> BTreeMap<String, bool> {
        load_declared_from(sources)
            .await
            .expect("plugins.json parses")
            .plugins
            .into_iter()
            .map(|(id, s)| (id, s.enabled))
            .collect()
    }

    /// **The switch persists** (#1400): what `set_enabled` writes is what the
    /// next `load_declared_from` — every reconcile, listing and watch tick,
    /// and the one after a shell restart — reads back, in both directions;
    /// and switching a plugin back to what nix declares deletes its entry,
    /// and the last one the file.
    ///
    /// Red if the fold is dropped from `load_declared_from` (the effective
    /// values stay nix's), if an agreeing switch is written rather than
    /// deleted, or if an emptied map leaves a file behind.
    #[tokio::test]
    async fn the_switch_persists_and_switching_back_deletes_the_entry() {
        let (_dir, sources, toml) = scratch_sources(TWO_FREE);
        let both = |timer: bool, pet: bool| -> BTreeMap<String, bool> {
            [("pet".to_owned(), pet), ("timer".to_owned(), timer)]
                .into_iter()
                .collect()
        };
        assert_eq!(effective(&sources).await, both(false, true), "as declared");

        set_enabled_in(&sources, "timer", true).await.expect("on");
        assert_eq!(
            std::fs::read_to_string(&toml).expect("the switch wrote state"),
            "[enabled]\ntimer = true\n"
        );
        assert_eq!(effective(&sources).await, both(true, true));

        set_enabled_in(&sources, "pet", false).await.expect("off");
        assert_eq!(effective(&sources).await, both(true, false));

        set_enabled_in(&sources, "timer", false)
            .await
            .expect("back");
        assert_eq!(
            read_overrides(Some(&toml)),
            Overrides {
                enabled: [("pet".to_owned(), false)].into_iter().collect()
            },
            "switching back to the declared value deletes the entry"
        );
        assert_eq!(effective(&sources).await, both(false, false));

        set_enabled_in(&sources, "pet", true).await.expect("back");
        assert!(!toml.exists(), "no override left, no file left");
        assert_eq!(effective(&sources).await, both(false, true));
    }

    /// A plugin nix pins refuses the switch with the option's name and writes
    /// nothing; a stale override already on disk for it stays on disk,
    /// untouched and ignored.
    ///
    /// Red if the pin check is dropped from `set_enabled_in` (the override is
    /// written) or the error stops naming the option.
    #[tokio::test]
    async fn a_pinned_plugins_switch_errors_and_writes_nothing() {
        let (_dir, sources, toml) = scratch_sources(
            r#"{"plugins":{"niri-layouts":{"exec":"/bin/niri","enabled":true,"_locked":["enabled"]}}}"#,
        );
        let err = set_enabled_in(&sources, "niri-layouts", false)
            .await
            .expect_err("a pinned plugin's switch must not persist");
        assert!(
            err.to_string()
                .contains("programs.trollshell.plugins.niri-layouts.enable"),
            "the error must name the option that pins it: {err}"
        );
        assert!(!toml.exists(), "nothing written");

        // An override left from before the pin: ignored, and left alone.
        std::fs::create_dir_all(toml.parent().expect("parent")).expect("mkdir");
        std::fs::write(&toml, "[enabled]\nniri-layouts = false\n").expect("seed");
        assert_eq!(
            effective(&sources).await.get("niri-layouts"),
            Some(&true),
            "a pin wins over an override on disk"
        );
        set_enabled_in(&sources, "niri-layouts", false)
            .await
            .expect_err("still pinned");
        assert_eq!(
            std::fs::read_to_string(&toml).expect("still there"),
            "[enabled]\nniri-layouts = false\n",
            "a refused switch leaves the file's bytes alone"
        );
    }

    /// A `plugins.json` that exists but does not parse cannot say whether an
    /// id is declared or pinned, so the switch refuses rather than guessing —
    /// and never falls through to the legacy unit-file path, which would
    /// reach the user manager.
    #[tokio::test]
    async fn an_unparsable_plugins_json_refuses_to_persist() {
        let (_dir, sources, toml) = scratch_sources("{ not json");
        set_enabled_in(&sources, "timer", true)
            .await
            .expect_err("nothing to decide against");
        assert!(!toml.exists());
    }

    /// The override file is the shell's own; one that no longer parses reads
    /// as "no overrides" — nix's values — rather than failing the load.
    #[tokio::test]
    async fn a_corrupt_override_file_reads_as_no_overrides() {
        let (_dir, sources, toml) = scratch_sources(TWO_FREE);
        std::fs::create_dir_all(toml.parent().expect("parent")).expect("mkdir");
        std::fs::write(&toml, "not toml {{{").expect("corrupt");
        assert_eq!(
            effective(&sources).await,
            [("pet".to_owned(), true), ("timer".to_owned(), false)]
                .into_iter()
                .collect::<BTreeMap<_, _>>()
        );
    }

    /// **The wiring**: `set_enabled` and `load_declared`, the two
    /// environment-reading wrappers the `Control` handlers and `list`/`start`
    /// call, really do resolve the override file under `$XDG_STATE_HOME` and
    /// agree with each other about it.
    ///
    /// Under [`scratch_home`], so both the `plugins.json` it declares and the
    /// `plugins.toml` it writes are a tempdir's. Red if either wrapper stops
    /// going through [`Sources::from_env`] (an override path of `None` makes
    /// `set_enabled` fail; a wrapper reading only the config half makes the
    /// switch vanish from `load_declared`).
    #[test]
    fn the_environment_wrappers_persist_under_the_state_home() {
        scratch_home(|home| {
            let json = home.join(".config").join(STATE_FILE_REL);
            std::fs::create_dir_all(json.parent().expect("parent")).expect("mkdir");
            std::fs::write(&json, TWO_FREE).expect("write");
            let toml = home.join(".local/state/trollshell/plugins.toml");

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async {
                set_enabled("timer", true).await.expect("persists");
                assert_eq!(
                    read_overrides(Some(&toml)).enabled.get("timer"),
                    Some(&true),
                    "set_enabled must write $XDG_STATE_HOME/trollshell/plugins.toml"
                );
                let timer = load_declared()
                    .await
                    .expect("parses")
                    .plugins
                    .remove("timer")
                    .expect("declared");
                assert!(timer.enabled, "load_declared must fold the override in");
            });
        });
    }

    /// #1400 review, finding 6 (R1): every reader of the declared state but
    /// `set_enabled_in` reads the *effective* state, through the fold. The
    /// whole feature rests on it: a `reconcile_listing` that read nix's
    /// declaration alone would forget the switch on every restart and stop a
    /// switched-on plugin on every rebuild tick, and all the behavioural
    /// tests above would stay green, since `load_declared_from` is tested but
    /// its callers are not. A source scan, on
    /// `launch_at_startup_spawns_the_supervised_watch`'s precedent, because
    /// a pass that gets past the listing with a plugin switched on launches
    /// it for real (#1404's seam fakes the listing, not the launch).
    ///
    /// Falsified by `reconcile_listing` reading `load_nix_declared(&sources.config)`.
    #[test]
    fn every_reader_but_set_enabled_goes_through_the_fold() {
        let src = include_str!("plugin_launcher.rs");
        let prod = &src[..src.find("#[cfg(test)]\nmod tests").expect("tests module")];
        let body = |sig: &str| {
            let start = prod.find(sig).unwrap_or_else(|| panic!("{sig} is defined"));
            let len = prod[start..].find("\n}\n").expect("its body ends");
            &prod[start..start + len]
        };
        for sig in [
            "async fn reconcile_listing<",
            "pub async fn list()",
            "pub async fn start(",
        ] {
            let b = body(sig);
            assert!(
                !b.contains("load_nix_declared("),
                "{sig} must read the effective state:\n{b}"
            );
            assert!(
                b.contains("load_declared"),
                "{sig} must go through load_declared(_from):\n{b}"
            );
        }
        // Definition + `load_declared_from` + `set_enabled_in`, nothing else.
        assert_eq!(prod.matches("load_nix_declared(").count(), 3);
    }

    /// #1400 review, finding 6 (R3): the switch's read-modify-write of the
    /// override file runs under [`CONVERGE_LOCK`], so a reconcile cannot read
    /// the file between its two halves, and two quick switches cannot lose
    /// one. Nothing else pins that the lock is taken at all.
    ///
    /// Falsified by `let guard = ();` in `set_enabled_in`.
    #[tokio::test]
    async fn the_switch_waits_for_the_converge_lock() {
        let (_dir, sources, toml) = scratch_sources(TWO_FREE);
        let held = CONVERGE_LOCK.lock().await;
        let write = set_enabled_in(&sources, "timer", true);
        tokio::pin!(write);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut write)
                .await
                .is_err(),
            "set_enabled_in must not write while a reconcile holds the lock"
        );
        assert!(!toml.exists(), "nothing written under someone else's lock");
        drop(held);
        write.await.expect("persists once the lock is free");
        assert!(toml.exists());
    }

    // ── systemd-run argv ─────────────────────────────────────────────────────

    #[test]
    fn systemd_run_args_pin_the_invocation() {
        let mut s = spec("/nix/store/abc/bin/demo", true);
        s.env.insert("A".to_owned(), "1".to_owned());
        s.env.insert("B".to_owned(), "x=y".to_owned());
        let args = run_argv(
            "demo",
            &s,
            &[("PLUGIN_API_KEY".to_owned(), "s3cret".to_owned())],
            DEFAULT_TARGET,
        );
        // The description carries the spec fingerprint (#695) so a later
        // reconcile can diff this unit against the declared spec.
        let description = format!("--description={}", unit_description("demo", &fp(&s)));
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
                "--property=TimeoutStopSec=10s",
                "--setenv=A=1",
                // '=' in a *value* is fine (systemd splits on the first '=').
                "--setenv=B=x=y",
                // #392 hook: extra_env rides after the declared env — and since
                // #984 as the *bare* name, so the value never enters argv.
                "--setenv=PLUGIN_API_KEY",
                "--",
                "/nix/store/abc/bin/demo",
            ]
        );
    }

    // ── Secrets never enter the argv (#984) ──────────────────────────────────

    /// The one invocation builder, under a short name. There is deliberately no
    /// other way to construct it — `crate::launch::args` is private to its
    /// module (#984, review MEDIUM-2; the rule moved with the builder in #1071
    /// phase 2 and is unchanged), so every argv assertion below necessarily
    /// describes a command that also carries the matching environment.
    ///
    /// The only thing #1071 changed here is *where* the argv is assembled: this
    /// helper now maps the plugin's four parameters onto a
    /// [`Launch`](crate::launch::Launch) and hands that to the shared builder.
    /// Every assertion below is byte-for-byte what it was.
    fn run_command(
        id: &str,
        spec: &PluginSpec,
        extra_env: &[(String, String)],
        target: &str,
    ) -> tokio::process::Command {
        launch::command(
            launch::SYSTEMD_RUN,
            &plugin_launch(id, spec, extra_env, target),
        )
    }

    /// The argv of the invocation [`launch()`] would actually run, read back off
    /// the built `Command`. Replaces the old habit of pinning the pure argv
    /// builder: same assertions, but on the command that really runs rather than
    /// on a function a second launch path could sidestep.
    fn run_argv(
        id: &str,
        spec: &PluginSpec,
        extra_env: &[(String, String)],
        target: &str,
    ) -> Vec<String> {
        launch::argv_of(&run_command(id, spec, extra_env, target))
    }

    /// The variables a `Command` built by [`run_command`] sets **explicitly** on
    /// the child (`get_envs` reports only the delta over the inherited
    /// environment, which is exactly what we want to pin), as owned strings
    /// sorted by name — `get_envs`'s own order is unspecified.
    fn command_envs(cmd: &tokio::process::Command) -> Vec<(String, Option<String>)> {
        launch::envs_of(cmd)
    }

    #[test]
    fn a_secret_reaches_systemd_run_through_the_environment_not_the_argv() {
        // #984: `/proc/<pid>/environ` is 0400 owner-only, `/proc/<pid>/cmdline`
        // is 0444 — any local user could read a plugin's API key off the
        // `systemd-run` process for as long as it lived. The value must be in
        // the child's environment and the argv must name the variable only.
        const SECRET: &str = "sk-do-not-put-me-in-argv";
        let mut s = spec("/nix/store/abc/bin/pet", true);
        s.env.insert("PET_NAME".to_owned(), "nisse".to_owned());
        let extra = [("OPENROUTER_API_KEY".to_owned(), SECRET.to_owned())];

        let args = run_argv("pet", &s, &extra, DEFAULT_TARGET);
        assert!(
            args.contains(&"--setenv=OPENROUTER_API_KEY".to_owned()),
            "the bare form must name the variable: {args:?}"
        );
        // Nothing in the argv — under any spelling — carries the value.
        assert!(
            !args.iter().any(|a| a.contains(SECRET)),
            "secret value found in argv: {args:?}"
        );
        // Specifically not the `NAME=value` rendering this issue is about.
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("--setenv=OPENROUTER_API_KEY=")),
            "argv still assigns the secret inline: {args:?}"
        );

        // …and the value is really there, in the child's environment.
        let cmd = run_command("pet", &s, &extra, DEFAULT_TARGET);
        assert_eq!(
            command_envs(&cmd),
            vec![("OPENROUTER_API_KEY".to_owned(), Some(SECRET.to_owned()))],
            "the secret must be set on the systemd-run process's environment"
        );
        // The command's argv is the pure builder's, unchanged.
        assert_eq!(
            cmd.as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            args
        );
    }

    #[test]
    fn the_declared_env_deliberately_stays_inline_on_the_argv() {
        // The counterpart to the test above, pinning the scope of #984: only
        // secrets moved. `spec.env` is nix-rendered into the world-readable
        // `plugins.json`, so argv discloses nothing a `cat` doesn't, and an
        // explicitly-passed value can't be shadowed by an inherited one.
        let mut s = spec("/bin/demo", true);
        s.env.insert("PET_NAME".to_owned(), "nisse".to_owned());
        let cmd = run_command("demo", &s, &[], DEFAULT_TARGET);
        let args = run_argv("demo", &s, &[], DEFAULT_TARGET);
        assert!(
            args.contains(&"--setenv=PET_NAME=nisse".to_owned()),
            "{args:?}"
        );
        assert!(
            command_envs(&cmd).is_empty(),
            "a declared env var must not be moved into the process environment"
        );
    }

    #[test]
    fn no_extra_env_pair_is_ever_rendered_with_a_value() {
        // Belt and braces over the *set* of extra_env entries: whatever the
        // caller passes, every one of them must render bare. `relaunch_for_secret`
        // and `reconcile` reach `systemd-run` through the same builder, so this
        // covers every injection path (see `launch`'s doc comment).
        let s = spec("/bin/demo", true);
        let extra = [
            ("OPENROUTER_API_KEY".to_owned(), "sk-a".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), "sk-b".to_owned()),
        ];
        let args = run_argv("demo", &s, &extra, DEFAULT_TARGET);
        for (k, v) in &extra {
            assert!(args.contains(&format!("--setenv={k}")), "{args:?}");
            assert!(!args.iter().any(|a| a.contains(v)), "{args:?}");
        }
        let cmd = run_command("demo", &s, &extra, DEFAULT_TARGET);
        assert_eq!(
            command_envs(&cmd),
            vec![
                ("ANTHROPIC_API_KEY".to_owned(), Some("sk-b".to_owned())),
                ("OPENROUTER_API_KEY".to_owned(), Some("sk-a".to_owned())),
            ]
        );
    }

    #[test]
    fn an_injected_secret_still_overrides_a_stale_declared_value() {
        // #392's precedence rule, re-pinned because the rendering changed: the
        // bare `--setenv=K` must still come *after* the declared `--setenv=K=V`.
        // systemd's own semantics do the rest — measured on systemd 260.2, a
        // later bare `--setenv=K` replaces an earlier `--setenv=K=declared`.
        let mut s = spec("/bin/demo", true);
        s.env
            .insert("OPENROUTER_API_KEY".to_owned(), "stale".to_owned());
        let extra = [("OPENROUTER_API_KEY".to_owned(), "fresh".to_owned())];
        let args = run_argv("demo", &s, &extra, DEFAULT_TARGET);
        let declared = args
            .iter()
            .position(|a| a == "--setenv=OPENROUTER_API_KEY=stale")
            .expect("the declared value is still passed inline");
        let injected = args
            .iter()
            .position(|a| a == "--setenv=OPENROUTER_API_KEY")
            .expect("the injected secret is passed bare");
        assert!(declared < injected, "{args:?}");

        // …and the value that wins is actually SET. Pinning only the argv order
        // leaves the collision case — a name both declared and injected, which is
        // exactly the claude-bridge billing scrub, the one configuration that ships —
        // unguarded on the environment side: a `systemd_run_command` that skipped
        // `cmd.env` for a name already in `spec.env` would pass every other test here
        // while the child silently received `""` (a bare `--setenv=K` for a name
        // absent from systemd-run's own environment resolves to the empty string, not
        // an error).
        let cmd = run_command("demo", &s, &extra, DEFAULT_TARGET);
        assert_eq!(
            command_envs(&cmd),
            vec![("OPENROUTER_API_KEY".to_owned(), Some("fresh".to_owned()))],
            "the injected value must be set even when the same name is declared inline"
        );
    }

    // ── spec fingerprint (#695) ──────────────────────────────────────────────

    #[test]
    fn fingerprint_is_pinned_and_stable() {
        // The digest crosses process (and build) boundaries — one shell writes
        // it into a unit description, a later one reads it back — so the exact
        // value is pinned, not just its properties. Changing the algorithm here
        // means every running plugin restarts once on upgrade.
        assert_eq!(fp(&spec("/bin/demo", true)), "963338d3f7f67d63");
        assert_eq!(
            fp(&spec_env("/bin/demo", &[("A", "1")])),
            "01eabc3f30afe012"
        );
    }

    #[test]
    fn fingerprint_covers_exec_env_and_secrets() {
        let base = spec_env("/nix/store/aaa/bin/pet", &[("PET_NAME", "nisse")]);
        let base_fp = fp(&base);

        // A rebuilt package (new store path) is the #695 `package` half.
        let rebuilt = spec_env("/nix/store/bbb/bin/pet", &[("PET_NAME", "nisse")]);
        assert_ne!(fp(&rebuilt), base_fp);

        // A changed env value, an added key, and a dropped key all differ.
        assert_ne!(
            fp(&spec_env("/nix/store/aaa/bin/pet", &[("PET_NAME", "kat")])),
            base_fp
        );
        assert_ne!(
            fp(&spec_env(
                "/nix/store/aaa/bin/pet",
                &[("PET_NAME", "nisse"), ("PET_LLM_URL", "http://x")]
            )),
            base_fp
        );
        assert_ne!(fp(&spec("/nix/store/aaa/bin/pet", true)), base_fp);

        // Opting into a secret slot changes which key gets injected at spawn.
        let mut with_slot = base.clone();
        with_slot.secrets = vec!["openrouter".to_owned()];
        assert_ne!(fp(&with_slot), base_fp);
    }

    #[test]
    fn fingerprint_separators_keep_env_pairs_unambiguous() {
        // Without field separators these two would digest identically.
        let a = spec_env("/bin/x", &[("AB", "C")]);
        let b = spec_env("/bin/x", &[("A", "BC")]);
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn fingerprint_ignores_enablement_and_map_order() {
        // `enabled` drives stop/launch, never a restart.
        let mut disabled = spec_env("/bin/x", &[("A", "1")]);
        disabled.enabled = false;
        assert_eq!(fp(&disabled), fp(&spec_env("/bin/x", &[("A", "1")])));
        // The env is a BTreeMap, so insertion order can't perturb the digest.
        assert_eq!(
            fp(&spec_env("/bin/x", &[("A", "1"), ("B", "2")])),
            fp(&spec_env("/bin/x", &[("B", "2"), ("A", "1")]))
        );
    }

    #[test]
    fn description_round_trips_the_fingerprint() {
        let s = spec_env("/bin/pet", &[("PET_NAME", "nisse")]);
        let digest = fp(&s);
        let desc = unit_description("pet", &digest);
        assert_eq!(desc, format!("trollshell plugin: pet [cfg:{digest}]"));
        assert_eq!(parse_fingerprint(&desc), Some(digest.as_str()));
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
            description: unit_description(id, &fp(spec)),
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

    /// `{id: spec}` from pairs, on the default session target, for the diff
    /// tests. (The target's own effect on the diff has its own test above.)
    fn declared(entries: &[(&str, PluginSpec)]) -> Declared {
        Declared {
            plugins: entries
                .iter()
                .map(|(id, spec)| ((*id).to_owned(), spec.clone()))
                .collect(),
            ..Declared::default()
        }
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
                (
                    "PET_LLM_URL",
                    "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock",
                ),
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
            plan(&Declared::default(), &units),
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
        assert!(plan(&Declared::default(), &units).is_empty());
    }

    /// #1400 review, finding 4: `availablePlugins` declares every bundled id,
    /// off, so the two hand-written units above are now *declared* disabled.
    /// A unit without the launcher's stamp is still not the launcher's to
    /// stop, and the same unit carrying the stamp still is.
    ///
    /// Falsified by folding the stamp back out of the disabled arm (`(false,
    /// Some(_)) => Stop`): the unstamped half returns both units as stops.
    #[test]
    fn plan_leaves_an_unstamped_unit_for_a_declared_off_id_alone() {
        let mut timer = spec("/bin/timer", true);
        timer.enabled = false;
        let mut terminal = spec("/bin/terminal", true);
        terminal.enabled = false;
        let d = declared(&[("timer", timer.clone()), ("terminal", terminal.clone())]);

        let unstamped = vec![
            unit("timer", "active", true),
            systemd::PluginUnit {
                description: "Hand-written plugin unit".to_owned(),
                ..unit("terminal", "active", true)
            },
        ];
        assert!(
            plan(&d, &unstamped).is_empty(),
            "a declared-off id's unstamped unit is left alone"
        );

        let stamped = vec![
            unit_for("timer", "active", &timer),
            unit_for("terminal", "active", &terminal),
        ];
        assert_eq!(
            plan(&d, &stamped),
            vec![
                ("terminal".to_owned(), Action::Stop),
                ("timer".to_owned(), Action::Stop),
            ],
            "a declared-off id's stamped unit is the launcher's, and stops"
        );
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

    // ── the outstanding-secret watcher (#866) ────────────────────────────────

    fn slots(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    /// A slot that couldn't be resolved is recorded against the plugin that
    /// wanted it, and that is what arms the watcher.
    #[test]
    fn fold_resolution_records_a_missing_slot_against_its_plugin() {
        let mut out = Outstanding::new();
        assert!(
            fold_resolution(&mut out, "pet", &slots(&["openrouter"]), &[]),
            "something outstanding → the watcher is wanted"
        );
        assert_eq!(outstanding_slots(&out), slots(&["openrouter"]));
        assert_eq!(out["openrouter"], BTreeSet::from(["pet".to_owned()]));
    }

    /// The other half: a slot that *did* resolve clears that plugin's record, so
    /// a relaunch which finally got its key stops being waited on. Only when the
    /// last waiter drops does the slot itself leave — another plugin still
    /// missing the same key keeps the watch alive.
    #[test]
    fn fold_resolution_drops_a_plugin_that_got_its_key_and_keeps_the_others() {
        let mut out = Outstanding::new();
        fold_resolution(&mut out, "pet", &slots(&["openrouter"]), &[]);
        fold_resolution(&mut out, "caw", &slots(&["openrouter"]), &[]);
        assert_eq!(
            out["openrouter"],
            BTreeSet::from(["caw".to_owned(), "pet".to_owned()])
        );

        // The pet relaunched and got its key; caw still hasn't.
        assert!(
            fold_resolution(&mut out, "pet", &[], &slots(&["openrouter"])),
            "caw is still waiting, so the watcher stays armed"
        );
        assert_eq!(out["openrouter"], BTreeSet::from(["caw".to_owned()]));

        // Now caw gets it too — the slot leaves the map entirely and the
        // watcher is told to stand down.
        assert!(
            !fold_resolution(&mut out, "caw", &[], &slots(&["openrouter"])),
            "nothing outstanding → no watcher wanted"
        );
        assert!(out.is_empty(), "an empty slot entry is removed, not kept");
    }

    /// A plugin resolving a slot it was never waiting on is a no-op, not a
    /// panic — every launch reports its whole `present` list.
    #[test]
    fn fold_resolution_ignores_a_resolution_for_an_unwatched_slot() {
        let mut out = Outstanding::new();
        assert!(!fold_resolution(
            &mut out,
            "pet",
            &[],
            &slots(&["openrouter"])
        ));
        assert!(out.is_empty());
    }

    /// `waiters_on` reports who is waiting **without** dropping them — the
    /// watcher logs the list before the relaunch, and a relaunch that fails has
    /// to leave the slot outstanding (F7).
    #[test]
    fn waiters_on_reads_the_entry_without_consuming_it() {
        let mut out = Outstanding::new();
        fold_resolution(&mut out, "pet", &slots(&["openrouter"]), &[]);
        fold_resolution(&mut out, "caw", &slots(&["openrouter"]), &[]);
        fold_resolution(&mut out, "bridge", &slots(&["anthropic"]), &[]);

        assert_eq!(waiters_on(&out, "openrouter"), slots(&["caw", "pet"]));
        assert_eq!(
            outstanding_slots(&out),
            slots(&["anthropic", "openrouter"]),
            "reading must not drop the slot"
        );
        assert!(waiters_on(&out, "nosuch").is_empty());
    }

    /// **F7.** A relaunch that fails keeps its plugin under watch; the ones that
    /// came back stop being watched, and a slot with no failures leaves entirely.
    ///
    /// The `pet` id is *re-inserted* here, not retained: by the time `settle_slot`
    /// runs, a successful `restart` has already dropped every id it resolved via
    /// `fold_resolution`, so the failures have to be written back explicitly.
    #[test]
    fn settle_slot_keeps_only_the_plugins_whose_relaunch_failed() {
        let mut out = Outstanding::new();
        let mut failures = FailureCounts::new();
        fold_resolution(&mut out, "pet", &slots(&["openrouter"]), &[]);
        fold_resolution(&mut out, "caw", &slots(&["openrouter"]), &[]);
        // `restart` already cleared both ids on its way through resolve_secret_env.
        fold_resolution(&mut out, "pet", &[], &slots(&["openrouter"]));
        fold_resolution(&mut out, "caw", &[], &slots(&["openrouter"]));
        assert!(
            out.is_empty(),
            "the precondition this test is written against"
        );

        let dropped = settle_slot(
            &mut out,
            &mut failures,
            "openrouter",
            &[("pet".to_owned(), "boom".to_owned())],
        );
        assert!(dropped.is_empty(), "one failure is well under the cap");
        assert_eq!(
            out["openrouter"],
            BTreeSet::from(["pet".to_owned()]),
            "the failed plugin keeps waiting for the next pass"
        );

        // Next pass: pet comes back up. Nothing failed → the slot is gone, and
        // its failure streak is forgotten with it.
        let dropped = settle_slot(&mut out, &mut failures, "openrouter", &[]);
        assert!(dropped.is_empty());
        assert!(out.is_empty());
        assert!(failures.is_empty(), "a success resets the streak");
    }

    /// **#880.** A `(slot, id)` pair that fails every attempt is dropped once
    /// it hits [`MAX_RELAUNCH_FAILURES`] consecutive failures, instead of
    /// being retried forever — the issue's exact scenario: an id both
    /// declared in `plugins.json` *and* hand-installed as a static unit under
    /// the same name can only ever fail its transient relaunch, and pre-#880
    /// that meant `settle_slot` re-inserted it every `SECRET_POLL_INTERVAL`
    /// for the life of the session.
    #[test]
    fn settle_slot_drops_a_pair_after_max_consecutive_failures() {
        let mut out = Outstanding::new();
        let mut failures = FailureCounts::new();
        let fail = |out: &mut Outstanding, failures: &mut FailureCounts| {
            settle_slot(
                out,
                failures,
                "anthropic",
                &[("bridge".to_owned(), "unit already exists".to_owned())],
            )
        };

        for attempt in 1..MAX_RELAUNCH_FAILURES {
            let dropped = fail(&mut out, &mut failures);
            assert!(
                dropped.is_empty(),
                "attempt {attempt} is still under the cap"
            );
            assert_eq!(
                out["anthropic"],
                BTreeSet::from(["bridge".to_owned()]),
                "still retried while under the cap (attempt {attempt})"
            );
        }

        // The cap-th failure drops it instead of retrying again.
        let dropped = fail(&mut out, &mut failures);
        assert_eq!(
            dropped,
            vec![(
                "bridge".to_owned(),
                MAX_RELAUNCH_FAILURES,
                "unit already exists".to_owned()
            )]
        );
        assert!(
            !out.contains_key("anthropic"),
            "dropped rather than kept under watch for another pass"
        );
        assert!(
            !failures.contains_key(&("anthropic".to_owned(), "bridge".to_owned())),
            "the streak is forgotten once it triggers the drop"
        );
    }

    /// A success partway through a failure streak resets the count — the cap
    /// is on *consecutive* failures, so an intermittent one never trips it.
    #[test]
    fn settle_slot_resets_the_streak_on_a_success() {
        let mut out = Outstanding::new();
        let mut failures = FailureCounts::new();

        for _ in 0..MAX_RELAUNCH_FAILURES - 1 {
            settle_slot(
                &mut out,
                &mut failures,
                "openrouter",
                &[("pet".to_owned(), "connection refused".to_owned())],
            );
        }
        assert_eq!(
            failures[&("openrouter".to_owned(), "pet".to_owned())],
            MAX_RELAUNCH_FAILURES - 1
        );

        // A pass where pet isn't reported failed — the relaunch succeeded —
        // resets the streak.
        settle_slot(&mut out, &mut failures, "openrouter", &[]);
        assert!(failures.is_empty());

        // A fresh run of failures afterwards starts from zero again, so it
        // does not trip the cap even though five failures have now happened
        // in total.
        for _ in 0..MAX_RELAUNCH_FAILURES - 1 {
            let dropped = settle_slot(
                &mut out,
                &mut failures,
                "openrouter",
                &[("pet".to_owned(), "connection refused".to_owned())],
            );
            assert!(
                dropped.is_empty(),
                "the reset streak is still under the cap"
            );
        }
        assert_eq!(
            failures[&("openrouter".to_owned(), "pet".to_owned())],
            MAX_RELAUNCH_FAILURES - 1
        );
    }

    /// **F10.** A plugin dropped from `plugins.json`, or one that no longer
    /// declares the slot, stops being waited on — otherwise it keeps the 30s
    /// poll alive for a key nothing would consume.
    #[test]
    fn prune_undeclared_forgets_plugins_the_config_no_longer_justifies() {
        let mut out = Outstanding::new();
        fold_resolution(&mut out, "pet", &slots(&["openrouter"]), &[]);
        fold_resolution(&mut out, "caw", &slots(&["openrouter"]), &[]);
        fold_resolution(&mut out, "bridge", &slots(&["anthropic"]), &[]);

        // `pet` still declares the slot; `caw` dropped it; `bridge` is gone.
        let mut with_slot = spec("/bin/pet", true);
        with_slot.secrets = slots(&["openrouter"]);
        let declared = BTreeMap::from([
            ("pet".to_owned(), with_slot),
            ("caw".to_owned(), spec("/bin/caw", true)),
        ]);
        prune_undeclared(&mut out, &declared);

        assert_eq!(
            outstanding_slots(&out),
            slots(&["openrouter"]),
            "the anthropic slot's only waiter is no longer declared"
        );
        assert_eq!(
            out["openrouter"],
            BTreeSet::from(["pet".to_owned()]),
            "caw stopped declaring the slot"
        );

        // …and once the last waiter goes, so does the poll.
        prune_undeclared(&mut out, &BTreeMap::new());
        assert!(out.is_empty());
    }

    /// **The transition rule.** Only `Available` triggers a relaunch; a locked
    /// ring and an absent key both keep waiting — the distinction exists for the
    /// log, because either can still turn into a key (the ring unlocks, or
    /// `secret-tool` writes one).
    #[test]
    fn only_an_available_probe_triggers_the_relaunch() {
        assert!(is_now_available(SecretProbe::Available));
        assert!(
            !is_now_available(SecretProbe::Locked),
            "a locked ring may still unlock"
        );
        assert!(
            !is_now_available(SecretProbe::Absent),
            "an external tool may still store the key"
        );
    }

    #[test]
    fn plan_treats_a_stopped_orphan_as_nothing_to_do() {
        // Only *running* units are candidates; an inactive leftover is already
        // where reconcile wants it.
        let gone = spec("/bin/gone", true);
        let units = vec![systemd::PluginUnit {
            description: unit_description("gone", &fp(&gone)),
            ..unit("gone", "inactive", false)
        }];
        assert!(plan(&Declared::default(), &units).is_empty());
    }
}
