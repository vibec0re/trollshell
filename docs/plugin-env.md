# Plugin environment-variable reference

Every bundled `hytte-plugin-*` binary is configured the same way: no config
DSL, just environment variables read at startup. This page inventories every
one of them, swept from source (`std::env::var` / `var_os` / `option_env!`
call sites in `crates/hytte-plugin-*/src`, the `hytte-plugin` SDK, and
`hytte-ai-providers`) rather than hand-maintained — see #573.

## How to set these

Declaratively, per plugin, via the Nix option:

```nix
programs.trollshell.plugins.<name>.env = {
  PET_NAME = "nisse";
  PET_LLM_MODEL = "google/gemini-3.5-flash";
};
```

This renders into the launch-state file the shell reads at startup and is
applied as `--setenv=K=V` when it launches the plugin as a transient
`trollshell-plugin-<id>` user unit (`trollshell/src/plugin_launcher.rs`). Set
the same vars in a shell profile for an ad hoc / hand-launched run — the
plugin reads its environment directly either way.

**Secrets never go in `env`.** The launch-state file is world-readable, so API
keys must go through `programs.trollshell.plugins.<name>.secrets` instead
(e.g. `secrets = [ "openrouter" ];`), which injects the key from the login
keyring as `<SLOT>_API_KEY` at spawn time — never written to disk. See
`nix/module-common.nix`'s `secrets` option description for the full mechanism.

A secret also never rides the launch **argv** (#984): the launcher sets the
value in the `systemd-run` process's own environment and passes the bare
`--setenv=<SLOT>_API_KEY` form, which `systemd-run(1)` resolves from there.
`/proc/<pid>/cmdline` is world-readable (`0444`) and `/proc/<pid>/environ` is
not (`0400`), so the key stops being readable by other local users.

"Never written to disk" above means never in `plugins.json`, never in a shipped
unit file, and never in the plugin's own config. It does **not** mean the value
exists only in memory: once the unit is running, the key is readable by the
owning user through the transient unit fragment systemd writes at
`/run/user/<uid>/systemd/transient/trollshell-plugin-<id>.service` (the file is
`0644` and carries `Environment="<SLOT>_API_KEY=<value>"`; what contains it is
`/run/user/<uid>` being `0700`), through
`systemctl --user show -p Environment trollshell-plugin-<id>`, and through the
plugin's own `/proc/<pid>/environ`. All three are same-user and in scope per
#956 — that is the boundary this design defends, and the argv was the one
channel that crossed it. The non-secret `env` above stays inline on the argv on
purpose: it is already public in the state file.

**Precedence**, for a plugin that also reads a config file for the same
setting: environment wins over the file, which wins over the plugin's
built-in default. Not every plugin has all three layers — each section below
says which it actually reads.

**Infrastructure vars, not knobs.** Every plugin (via `hytte-plugin-proto`'s
`topology::socket_path`) dials the host at
`$XDG_RUNTIME_DIR/trollshell/plugin.sock`, and a couple of plugins resolve
their own state/config paths off `$XDG_STATE_HOME` / `$XDG_CONFIG_HOME` /
`$HOME`. These are standard session variables set by the login session /
systemd, not something you set per-plugin — they're omitted from the tables
below and noted inline only where a plugin's behavior depends on them in a
non-obvious way.

## New-install checklist

Most of what's below is already owner-neutral out of the box — unset, or a
documented neutral fallback. A couple of things default instead to values
that only ever fit this project's own first deployment, and won't fit
anyone else's. Check these on a fresh install:

- **Departures' home station.** `hytte-services::places` seeds
  `~/.config/trollshell/places.toml` on first run with one `[[place]]` for
  Berlin-Schöneweide (`station = "900192001"`, the BVG id for S Schöneweide
  Bhf) — see the `departures` section below, which reads this file's station
  config directly. Outside Berlin, also set the `[departures].endpoint` key
  (#1124): a short name (`bvg`, `vbb`, `db`) or a full `https://…`
  transport.rest base URL — absent means `bvg`, so an existing config with no
  key behaves exactly as before. VBB and BVG share the VBB station id space;
  DB uses its own EVA ids, so switching backend usually means finding a new
  `station` id from that backend's own `/locations?query=<name>` route rather
  than reusing the Berlin one. This is a `places.toml` key, not an env var —
  see `crates/hytte-config/src/places.rs`'s `DEFAULT_CONFIG` for the
  documented example — so it isn't one of the tables below either; it's noted
  here because it's exactly the gap this checklist used to flag as unfixable.
- **Weather's fallback city**, `TROLLSHELL_WEATHER_CITY` — genuinely global
  (open-meteo, not BVG-scoped), unset by default, and GeoClue2 is tried
  first regardless. Only needs setting if GeoClue2 isn't available on your
  box; see the `weather` section below and
  `programs.trollshell.weather.fallbackCity`.
- **The desktop owner's name**, `TROLLSHELL_OWNER` — one session-wide var
  read by every plugin persona that refers to whoever is running the shell:
  today `caw`'s morning briefing and `pet`'s cat (both through the shared
  `hytte_ai_providers::owner`). See those two sections below. Already
  owner-neutral by default (falls back to "your human"), so this one is
  optional polish, not something a fresh install must set. Set it once via
  `programs.trollshell.ownerName` rather than per-plugin `env`.

## Bundled plugins

Sections below follow `bundledPluginNames`' order in `flake.nix` (14 total).

### agents (`hytte-plugin-agents`)

**Turning it on.** There is no `programs.trollshell.plugins.agents.enable`
option to find — `plugins` is an `attrsOf` submodule, so nothing named
`agents` exists in the rendered option docs until you write the attr, and
`package` is **required** with no default. Writing the entry is what enables
it:

```nix
programs.trollshell.plugins.agents = {
  package = trollshell.packages.${system}.hytte-plugin-agents;
  # optional; the plugin has no other env knobs
  env.RUST_LOG = "hytte_plugin_agents=debug";
};
```

Then `systemctl --user status trollshell-plugin-agents` should show a
transient unit, and the card appears at the top of the sidebar.

**Reaching the hive is group membership, not configuration.** The plugin
declares **no secret slot and reads no credential**: its entire authority is
the desktop user's membership of `hive-admin`
(`services.hyperhive.adminUsers = [ "you" ];`) against hyperhive's
`0660 root:hive-admin` socket — no root, no polkit, no `sudo`. Note the
non-obvious half: secondary group membership is applied **at login**, so a
shell (or a session) that predates the change still gets `EACCES`. The card
then renders one "no hive — permission denied" row, which is the correct
unprivileged outcome rather than a bug. `id -nG` shows what the running
session actually has.

No environment-variable knobs beyond `RUST_LOG`. The whole desktop-side
surface is
`~/.config/trollshell/agents.toml` (issue #947, spec §9): the hive's
`host.sock` path, the poll cadence, and per-agent display overrides
(`label` / `icon` / `project`, the last being the sidebar group header). It
rides `hytte-config`'s layered `Subsystem`, so it merges
`XDG_CONFIG_DIRS` → `XDG_CONFIG_HOME`, warns on an unknown key rather than
failing, and is re-read on the next poll after an edit — no plugin restart,
the same live-reload `places.toml` gets.

**Approvals have no knob either (#947 P3).** Pending hive approvals raise the
shell's consent prompt, and that rides the same `host.sock` and the same
`poll_seconds` cadence — there is nothing to enable, no key, and no way to
turn the prompt off short of not running the plugin. `poll_seconds` is the one
thing that moves it: it bounds how long a freshly-queued approval waits before
the card appears. Note that the poll **parks while the sidebar is closed**
(spec §5.4), so a card can be up to one sidebar-open away rather than one
cadence; the row's badge is what carries it in the meantime.

### audio-widget (`hytte-plugin-audio-widget`)

No runtime knobs — configuration is entirely via the shell/wire protocol
(it renders purely off the host's `StateKey::AudioSpectrum` push).

### bar-clock-demo (`hytte-plugin-bar-clock-demo`)

No runtime knobs — configuration is entirely via the shell/wire protocol.

### caw (`hytte-plugin-caw`)

| Variable                  | Default                                                                        | Effect                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| ------------------------- | ------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CAW_BRIEFING_TIME`       | `07:00`                                                                        | Local trigger time for the once-daily morning briefing, as `H` or `H:MM`. `off`/`none` disables the briefing entirely. An unparseable value silently falls back to the default rather than erroring (`briefing.rs::parse_time`).                                                                                                                                                                                                                                          |
| `CAW_LLM_URL`             | unset (→ OpenRouter, only if a key is resolved)                                | Base URL for the briefing's voiced-composition backend. Set to a local `llama-server` URL (e.g. `http://127.0.0.1:8080`) to self-host; set to `""` explicitly to force the plain canned-template composer with no network call. Unset with no key resolved also falls back to the plain composer (`briefing.rs::resolve_provider`).                                                                                                                                       |
| `CAW_LLM_MODEL`           | unset                                                                          | Model id sent in the chat request body when using the OpenRouter path (cloud endpoints require a model id — see the SDK-level note below on what happens if it's left unset). Ignored by a local `llama-server`, which uses its own loaded model.                                                                                                                                                                                                                         |
| `CAW_LLM_API_KEY`         | unset                                                                          | Fallback OpenRouter API key, used only if `hytte_ai_providers::load_key("openrouter")` (see "Shared" below) finds nothing. Prefer `plugins.caw.secrets = [ "openrouter" ];` instead — this direct env path exists but isn't the recommended one (world-readable launch-state file).                                                                                                                                                                                       |
| `CAW_EXPRESSION_PATH`     | `$XDG_STATE_HOME/caw/expression.json` (→ `~/.local/state/caw/expression.json`) | Overrides the path to the expression file caw's own `opencaw` `caw_express` tool writes and this plugin polls read-only. The default matches `opencaw`'s own default, so the two ends agree with no configuration (`expression.rs::expression_path`).                                                                                                                                                                                                                     |
| `TROLLSHELL_OWNER`        | unset (→ `"your human"`)                                                       | Name (or descriptor) caw's persona uses for the desktop owner in the morning briefing, e.g. `Jordan` or `the household`. Trimmed; blank counts as unset. Never guesses a name from `$USER`/GECOS — falls back to the neutral default instead (`hytte_ai_providers::owner`, #706). Session-wide, not caw-specific: `pet` reads the same variable through the same resolver (see "Shared" below). Not to be confused with `PET_NAME`, which names the _pet_, not the owner. |
| `TROLLSHELL_WEATHER_CITY` | unset                                                                          | Briefing-only fallback: when the first `[[place]]` in `~/.config/trollshell/places.toml` has no `lat`/`lon`, the briefing forward-geocodes this city name for its weather ingredient (`ingredients.rs::coords`) — the same var the weather plugin itself reads (see below). Not documented anywhere for `caw` prior to this sweep.                                                                                                                                        |

Also reads `$HOME` (to locate `places.toml`) and `$XDG_STATE_HOME`/`$HOME`
(for the expression/briefing-stamp state dir) — standard XDG paths, not
per-plugin knobs.

### clock-demo (`hytte-plugin-clock-demo`)

No runtime knobs — configuration is entirely via the shell/wire protocol.

### departures (`hytte-plugin-departures`)

No environment-variable knobs. Station config (which stop, walk-time budget,
line/direction filter) comes from the first `[[place]]` block of
`~/.config/trollshell/places.toml`, and which transport.rest backend to fetch
from comes from that same file's whole-shell `[departures].endpoint` (#1124,
absent → `bvg`) — the same file + schema the native `hytte-services::places`
service owns and documents a default for (`feed.rs::load_station_config`,
`feed.rs::resolve_endpoint`). The file is re-read on every poll, so an edit
while the board is open is picked up on the next fetch. Only `$HOME` is read
(to locate the file) — not a configurable knob.

### infobroker (`hytte-plugin-infobroker`)

No launch-time env knobs for the plugin daemon itself. Its own broker socket
and durable grant store are located from `$XDG_RUNTIME_DIR` and
`$XDG_STATE_HOME`/`$HOME` respectively (`paths.rs`) — standard session paths,
always set, not meant to be overridden via `plugins.infobroker.env`.

The crate also ships a separate CLI binary, `hytte-infobroker` (the client the
skill folder's agents shell out to — _not_ what `plugins.<name>.env` launches,
which is the `hytte-plugin-infobroker` daemon binary). That CLI reads
`HYTTE_INFOBROKER_TOKEN`, the session token `hytte-infobroker auth --agent
<name>` mints and prints as an `export` line for `eval`; `hytte-infobroker
get` then requires it be set (`cli.rs`). This is CLI-side, per-invocation
state, not a plugin-launch knob.

Argument parsing is `clap` (#1116), with a hidden `completions <shell>`
subcommand nix's `installShellCompletion` invokes at build time — not a
runtime knob either, just how `hytte-infobroker`'s bash/zsh/fish completions
ship.

### niri-layouts (`hytte-plugin-niri-layouts`)

No runtime knobs — the three layouts and their proportions are compiled in
(`layout.rs::Layout::proportions`), and the compositor is located through
`$NIRI_SOCKET`, which niri sets for every process in the session (standard
session path, not a per-plugin knob).

The same binary doubles as a CLI — `hytte-plugin-niri-layouts apply <equal |
golden | split>` applies one layout and exits — which is what a niri `spawn`
bind invokes. That path takes its layout as an argument, not from the
environment, so `plugins.niri-layouts.env` has nothing to set either way.

Argument parsing is `clap` (#1116), same as `hytte-infobroker` above, with the
same hidden `completions <shell>` subcommand backing its nix-installed
bash/zsh/fish completions.

### pet (`hytte-plugin-pet`)

| Variable                 | Default                                         | Effect                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ------------------------ | ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `PET_NAME`               | `nisse`                                         | The pet's display name, used in its own persona prompt when the LLM brain is active (`brain.rs::Cfg::from_env`).                                                                                                                                                                                                                                                                                                                                 |
| `PET_LLM_URL`            | unset (→ OpenRouter, only if a key is resolved) | Base URL for the pet's brain. Set to a local `llama-server` URL to self-host; set to `""` explicitly to force canned-only replies with no network call. Unset with no key resolved also falls back to canned-only (`brain.rs::resolve_provider`).                                                                                                                                                                                                |
| `PET_LLM_MODEL`          | unset                                           | Model id sent when using the OpenRouter path (required by that cloud endpoint — see the SDK-level note below). Ignored by a local `llama-server`.                                                                                                                                                                                                                                                                                                |
| `PET_LLM_API_KEY`        | unset                                           | Fallback OpenRouter API key, used only if `hytte_ai_providers::load_key("openrouter")` finds nothing. Prefer `plugins.pet.secrets = [ "openrouter" ];` instead.                                                                                                                                                                                                                                                                                  |
| `PET_LLM_MIN_GAP_SECS`   | `15`                                            | Whole seconds of throttle between two real model calls; pokes inside the gap get a canned line. Blank, unparsable or `0` → the default (`brain.rs::Cfg::from_env`).                                                                                                                                                                                                                                                                              |
| `PET_LLM_TIMEOUT_SECS`   | `10`                                            | Whole seconds a single model call may take before the client hangs up (the shared `hytte-ai-providers` request budget). Raise it for a slow backend — and see the ordering note below. Blank, unparsable or `0` → the default.                                                                                                                                                                                                                   |
| `PET_PERSONA`            | unset (→ `"playful, a little sassy"`)           | A style/tone clause spliced into the persona prompt's `Style:` line when the LLM brain is active. Only that clause is overridable — the `{name}`/`{hour}`/`{mood}` interpolation and the trailing `Format:` rules (including the derived word budget) stay fixed, so a bad value can't make replies overrun the sidebar bubble. Empty or whitespace-only → the default (`brain.rs::Cfg::from_env`, `brain.rs::persona`).                         |
| `TROLLSHELL_OWNER`       | unset (→ `"your human"`)                        | Name (or descriptor) the pet uses for the desktop owner when the LLM brain is active — both in its persona ("lives in the sidebar of X's Linux desktop") and in the poke stimulus ("\*X pokes you\*"). Trimmed; blank counts as unset. Never guesses a name from `$USER`/GECOS (`hytte_ai_providers::owner`, #696). The same session-wide variable caw reads — see "Shared" below; it names the _owner_, where `PET_NAME` above names the _pet_. |
| `TROLLSHELL_PET_KAOMOJI` | unset (falsy)                                   | Set to `1` or `true` (case-insensitive) to force the kaomoji fallback face at startup, bypassing whatever the normal render path would pick (`main.rs::init`).                                                                                                                                                                                                                                                                                   |

### preem-demo (`hytte-plugin-preem-demo`)

No runtime knobs — configuration is entirely via the shell/wire protocol.

### terminal (`hytte-plugin-terminal`)

No runtime knobs — configuration is entirely via the shell/wire protocol.

### timer (`hytte-plugin-timer`)

No runtime knobs — configuration is entirely via the shell/wire protocol.

### weather (`hytte-plugin-weather`)

| Variable                  | Default | Effect                                                                                                                                                                                                                                                                                                                                                                  |
| ------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `TROLLSHELL_WEATHER_CITY` | unset   | Forward-geocoded (via open-meteo) fallback location, used only when `GeoClue2` is absent, denied, or times out after 10s (`location.rs::resolve`). GeoClue is always tried first. Usually set session-wide via `programs.trollshell.weather.fallbackCity` rather than per-plugin here — that Nix option is what actually sets this var for the weather plugin's launch. |

## Shared / SDK-level environment variables

The `hytte-plugin` SDK itself (`crates/hytte-plugin/src`, the `Plugin`
trait + `run()`) reads exactly **one** environment variable. Everything else is
a plugin-author idiom on top of the runtime, not part of it.

| Variable             | Default | Effect                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| -------------------- | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `HYTTE_PLUGIN_MOUNT` | unset   | **Where the plugin's card or chip mounts**, overriding the mount its own `manifest()` asked for (#1159). One of the nine wire mount names: `SidebarLead`, `SidebarTop`, `SidebarBottom`, `SidebarRightLead`, `SidebarRightTop`, `SidebarRightBottom`, `BarLeft`, `BarCenter`, `BarRight`. Read once in `run()` before the first dial and applied to every `Register` frame the process sends, reconnects included; the plugin's own code never sees it. An unknown or empty value is a **startup failure** naming all nine spellings — never a silent fallback to the manifest, which would put the card on the other sidebar and leave the plugin looking healthy. Surrounding whitespace is trimmed; the match is otherwise exact, case included. |

Set it per plugin like any other var — `programs.trollshell.plugins.<id>.env.HYTTE_PLUGIN_MOUNT = "SidebarRightTop";`
works today, and #1161 adds a checked `plugins.<id>.mount` option that renders
the same variable from an enum of the nine names. It is the one knob here that is
a _deployment_ decision rather than a plugin-author one, which is why it lives on
the launch instead of in a config file (settled on #866; see epic #1158).

**Prefer an override _within_ a family — bar↔sidebar changes a plugin's
visibility semantics and it cannot adapt.** The nine names are not
interchangeable. `Mount::is_bar` is what decides whether the host runs the
sidebar visibility push for a connection: a bar chip is effectively always
on-screen, so the host reports a constant `SlotVisible` of `true` for one
(#288/#422), and a bar-mounted plugin therefore declares no
`StateKey::SlotVisible` subscription at all — `hytte-plugin-timer` is the live
example ("it subscribes no host state"). Move such a plugin to a sidebar with
this variable and the host starts running the real visibility task for it, but
the plugin never subscribed, so under the #305 send-gate it receives nothing and
keeps polling at full rate behind a closed sidebar. The plugin's own code never
sees the override (that is the point of it), so it cannot compensate. A plugin
that wants to be movable across families must declare
`StateKey::SlotVisible` in its manifest's `subscribes` and park on the
`SlotVisibility` frames it then receives — `hytte_plugin::poll`'s
visibility-gated helpers are the shape for that. Moving a card between the two
sidebars, or a chip between the three bar regions, changes nothing about
visibility and needs no such care.

`hytte-ai-providers` (the shared OpenAI-compatible chat client `pet` and
`caw`'s brains both use) reads no timeout of its own — the per-request budget
is `ChatOpts::timeout`, which each plugin resolves (`PET_LLM_TIMEOUT_SECS`;
`caw` has no knob yet and takes the 10s `DEFAULT_TIMEOUT`). It reads three
variables — two via `load_key`, one via `owner`:

| Variable                                     | Default                   | Effect                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| -------------------------------------------- | ------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `<NAME>_API_KEY` (e.g. `OPENROUTER_API_KEY`) | unset                     | Overrides the on-disk key file for provider `<name>` (upper-cased). This is exactly the variable `plugins.<id>.secrets = [ "<name>" ]` injects at spawn from the login keyring — the recommended way to supply a key. Wins over the file when set.                                                                                                                                                                                                                                                                          |
| `XDG_CONFIG_HOME`                            | unset (→ `$HOME/.config`) | Base directory `load_key` reads the `<name>.key` file from: `$XDG_CONFIG_HOME/trollshell/<name>.key`, e.g. `openrouter.key`. Standard XDG var, not plugin-specific. Since #1169, a key file that grants any access to group or other (mode & 0o077 != 0 — anything looser than `0600`/`0400`) is refused rather than read; `chmod 600` it.                                                                                                                                                                                  |
| `TROLLSHELL_OWNER`                           | unset (→ `"your human"`)  | How a plugin persona refers to whoever is running the shell. Resolved by `hytte_ai_providers::owner` (trimmed, blank counts as unset, neutral `DEFAULT_OWNER` fallback, **never** guessed from `$USER`/GECOS) and read by both `caw` and `pet` — set it once for the session, not per plugin (#696/#706). Usually set session-wide via `programs.trollshell.ownerName` rather than per-plugin here — that Nix option is what actually sets this var for both plugins' launch (null, the default, leaves it unset entirely). |

For pet/caw specifically, the OpenRouter key precedence is therefore:
`OPENROUTER_API_KEY` env → `~/.config/trollshell/openrouter.key` file →
the plugin's own `PET_LLM_API_KEY`/`CAW_LLM_API_KEY` env fallback (last
resort, not recommended — see each plugin's table above).

**The `<NAME>_API_KEY` env override is unaffected by the mode check above** —
`plugins.<id>.secrets = [ "<name>" ]`'s keyring injection (#392) never touches
the file, so the recommended deployment path keeps working exactly as before.
The check only ever applies to the on-disk `<name>.key` fallback. One
consequence worth knowing about before it surprises you: `load_key` stats the
file with `metadata` (which follows symlinks), so a key declared via
home-manager's `home.file` — a symlink into the Nix store at `0444` — is now
refused too. That refusal is correct (the store is world-readable), but the
fix is agenix/sops-nix or a real `0600` file, not a `home.file` symlink.

**A note on `*_LLM_MODEL` with no value set:** when the OpenRouter path is
selected (a key is resolved and no `*_LLM_URL` override is set) but
`PET_LLM_MODEL`/`CAW_LLM_MODEL` is unset, `Provider.model` stays `None` and no
`model` field is sent in the request body. `hytte-ai-providers`' own docs note
OpenRouter _requires_ a model id for a cloud call — the code does not enforce
this or supply a default, so an unset model on the cloud path is a
misconfiguration rather than a supported "use OpenRouter's default" mode.
Consult `crates/hytte-ai-providers/src/lib.rs` and each plugin's
`brain.rs`/`briefing.rs` if this needs to change.

**Ordering the two timeouts (client vs. backend):** the client budget
(`ChatOpts::timeout`, i.e. `PET_LLM_TIMEOUT_SECS`) is the outer bound. A
backend that enforces its own per-request budget must stay strictly **under**
it, or a slow turn tears the connection mid-read instead of coming back as an
error the plugin can fall back from. `hytte-claude-bridge` is the live case:
its `CLAUDE_BRIDGE_TIMEOUT_SECS` defaults to 8s _because_ the client defaults
to 10s. So to give a cold `claude --print` turn more room, raise the **client**
first — e.g. `plugins.pet.env.PET_LLM_TIMEOUT_SECS = "30";` — and only then the
bridge's budget, to something still below it. Doing it in the other order buys
nothing: the client hangs up first regardless.
