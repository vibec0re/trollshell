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

**Precedence**, for a plugin that also reads a config file (e.g. usage's
`~/.config/trollshell/usage.toml`): environment wins over the file, which wins
over the plugin's built-in default. Not every plugin has all three layers —
each section below says which it actually reads.

**Infrastructure vars, not knobs.** Every plugin (via `hytte-plugin-proto`'s
`topology::socket_path`) dials the host at
`$XDG_RUNTIME_DIR/trollshell/plugin.sock`, and a couple of plugins resolve
their own state/config paths off `$XDG_STATE_HOME` / `$XDG_CONFIG_HOME` /
`$HOME`. These are standard session variables set by the login session /
systemd, not something you set per-plugin — they're omitted from the tables
below and noted inline only where a plugin's behavior depends on them in a
non-obvious way.

## Bundled plugins

Sections below follow `bundledPluginNames`' order in `flake.nix` (12 total).

### audio-widget (`hytte-plugin-audio-widget`)

No runtime knobs — configuration is entirely via the shell/wire protocol
(it renders purely off the host's `StateKey::AudioSpectrum` push).

### bar-clock-demo (`hytte-plugin-bar-clock-demo`)

No runtime knobs — configuration is entirely via the shell/wire protocol.

### caw (`hytte-plugin-caw`)

| Variable                  | Default                                                                        | Effect                                                                                                                                                                                                                                                                                                                              |
| ------------------------- | ------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CAW_BRIEFING_TIME`       | `07:00`                                                                        | Local trigger time for the once-daily morning briefing, as `H` or `H:MM`. `off`/`none` disables the briefing entirely. An unparseable value silently falls back to the default rather than erroring (`briefing.rs::parse_time`).                                                                                                    |
| `CAW_LLM_URL`             | unset (→ OpenRouter, only if a key is resolved)                                | Base URL for the briefing's voiced-composition backend. Set to a local `llama-server` URL (e.g. `http://127.0.0.1:8080`) to self-host; set to `""` explicitly to force the plain canned-template composer with no network call. Unset with no key resolved also falls back to the plain composer (`briefing.rs::resolve_provider`). |
| `CAW_LLM_MODEL`           | unset                                                                          | Model id sent in the chat request body when using the OpenRouter path (cloud endpoints require a model id — see the SDK-level note below on what happens if it's left unset). Ignored by a local `llama-server`, which uses its own loaded model.                                                                                   |
| `CAW_LLM_API_KEY`         | unset                                                                          | Fallback OpenRouter API key, used only if `hytte_ai_providers::load_key("openrouter")` (see "Shared" below) finds nothing. Prefer `plugins.caw.secrets = [ "openrouter" ];` instead — this direct env path exists but isn't the recommended one (world-readable launch-state file).                                                 |
| `CAW_EXPRESSION_PATH`     | `$XDG_STATE_HOME/caw/expression.json` (→ `~/.local/state/caw/expression.json`) | Overrides the path to the expression file caw's own `opencaw` `caw_express` tool writes and this plugin polls read-only. The default matches `opencaw`'s own default, so the two ends agree with no configuration (`expression.rs::expression_path`).                                                                               |
| `TROLLSHELL_WEATHER_CITY` | unset                                                                          | Briefing-only fallback: when the first `[[place]]` in `~/.config/trollshell/places.toml` has no `lat`/`lon`, the briefing forward-geocodes this city name for its weather ingredient (`ingredients.rs::coords`) — the same var the weather plugin itself reads (see below). Not documented anywhere for `caw` prior to this sweep.  |

Also reads `$HOME` (to locate `places.toml`) and `$XDG_STATE_HOME`/`$HOME`
(for the expression/briefing-stamp state dir) — standard XDG paths, not
per-plugin knobs.

### clock-demo (`hytte-plugin-clock-demo`)

No runtime knobs — configuration is entirely via the shell/wire protocol.

### departures (`hytte-plugin-departures`)

No environment-variable knobs. Station config (which stop, walk-time budget,
line/direction filter) comes entirely from the first `[[place]]` block of
`~/.config/trollshell/places.toml` — the same file + schema the native
`hytte-services::places` service owns and documents a default for
(`feed.rs::load_station_config`). The file is re-read on every poll, so an
edit while the board is open is picked up on the next fetch. Only `$HOME` is
read (to locate the file) — not a configurable knob.

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

### pet (`hytte-plugin-pet`)

| Variable                 | Default                                         | Effect                                                                                                                                                                                                                                            |
| ------------------------ | ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `PET_NAME`               | `nisse`                                         | The pet's display name, used in its own persona prompt when the LLM brain is active (`brain.rs::Cfg::from_env`).                                                                                                                                  |
| `PET_LLM_URL`            | unset (→ OpenRouter, only if a key is resolved) | Base URL for the pet's brain. Set to a local `llama-server` URL to self-host; set to `""` explicitly to force canned-only replies with no network call. Unset with no key resolved also falls back to canned-only (`brain.rs::resolve_provider`). |
| `PET_LLM_MODEL`          | unset                                           | Model id sent when using the OpenRouter path (required by that cloud endpoint — see the SDK-level note below). Ignored by a local `llama-server`.                                                                                                 |
| `PET_LLM_API_KEY`        | unset                                           | Fallback OpenRouter API key, used only if `hytte_ai_providers::load_key("openrouter")` finds nothing. Prefer `plugins.pet.secrets = [ "openrouter" ];` instead.                                                                                   |
| `TROLLSHELL_PET_KAOMOJI` | unset (falsy)                                   | Set to `1` or `true` (case-insensitive) to force the kaomoji fallback face at startup, bypassing whatever the normal render path would pick (`main.rs::init`).                                                                                    |

### preem-demo (`hytte-plugin-preem-demo`)

No runtime knobs — configuration is entirely via the shell/wire protocol.

### terminal (`hytte-plugin-terminal`)

No runtime knobs — configuration is entirely via the shell/wire protocol.

### timer (`hytte-plugin-timer`)

No runtime knobs — configuration is entirely via the shell/wire protocol.

### usage (`hytte-plugin-usage`)

Precedence is env → `~/.config/trollshell/usage.toml` → built-in default,
resolved once per field (`config.rs::resolve`). With no dashboard URL from
either source the plugin renders a calm empty-state card and makes no network
calls at all.

| Variable                         | Default                                     | Effect                                                                                                                           |
| -------------------------------- | ------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `TROLLSHELL_USAGE_DASHBOARD_URL` | unset (empty-state, no polling)             | The Grafana `…/public-dashboards/<token>` URL (browser or API form). The one value that flips the card from empty-state to live. |
| `TROLLSHELL_USAGE_BUDGET`        | unset (spend-only, no gauge)                | The budget denominator for the "burned ÷ budget" gauge. Must parse as a finite `f64`; an unparseable value is ignored.           |
| `TROLLSHELL_USAGE_PANEL`         | unset (auto-discover the first value panel) | Pins the dashboard panel to query by its numeric id, skipping panel discovery.                                                   |
| `TROLLSHELL_USAGE_WINDOW`        | `now-5h`                                    | The Grafana time-range `from` for the query window (e.g. `now-24h`, `now/d`); `to` is always `now`.                              |

### weather (`hytte-plugin-weather`)

| Variable                  | Default | Effect                                                                                                                                                                                                                                                                                                                                                                  |
| ------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `TROLLSHELL_WEATHER_CITY` | unset   | Forward-geocoded (via open-meteo) fallback location, used only when `GeoClue2` is absent, denied, or times out after 10s (`location.rs::resolve`). GeoClue is always tried first. Usually set session-wide via `programs.trollshell.weather.fallbackCity` rather than per-plugin here — that Nix option is what actually sets this var for the weather plugin's launch. |

## Shared / SDK-level environment variables

The `hytte-plugin` SDK itself (`crates/hytte-plugin/src`, the `Plugin`
trait + `run()`) reads **no** environment variables — env-based config is a
plugin-author idiom on top of it, not part of the runtime.

`hytte-ai-providers` (the shared OpenAI-compatible chat client `pet` and
`caw`'s brains both use) reads two, via `load_key`:

| Variable                                     | Default                   | Effect                                                                                                                                                                                                                                             |
| -------------------------------------------- | ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `<NAME>_API_KEY` (e.g. `OPENROUTER_API_KEY`) | unset                     | Overrides the on-disk key file for provider `<name>` (upper-cased). This is exactly the variable `plugins.<id>.secrets = [ "<name>" ]` injects at spawn from the login keyring — the recommended way to supply a key. Wins over the file when set. |
| `XDG_CONFIG_HOME`                            | unset (→ `$HOME/.config`) | Base directory `load_key` reads the `<name>.key` file from: `$XDG_CONFIG_HOME/trollshell/<name>.key`, e.g. `openrouter.key`. Standard XDG var, not plugin-specific.                                                                                |

For pet/caw specifically, the OpenRouter key precedence is therefore:
`OPENROUTER_API_KEY` env → `~/.config/trollshell/openrouter.key` file →
the plugin's own `PET_LLM_API_KEY`/`CAW_LLM_API_KEY` env fallback (last
resort, not recommended — see each plugin's table above).

**A note on `*_LLM_MODEL` with no value set:** when the OpenRouter path is
selected (a key is resolved and no `*_LLM_URL` override is set) but
`PET_LLM_MODEL`/`CAW_LLM_MODEL` is unset, `Provider.model` stays `None` and no
`model` field is sent in the request body. `hytte-ai-providers`' own docs note
OpenRouter _requires_ a model id for a cloud call — the code does not enforce
this or supply a default, so an unset model on the cloud path is a
misconfiguration rather than a supported "use OpenRouter's default" mode.
Consult `crates/hytte-ai-providers/src/lib.rs` and each plugin's
`brain.rs`/`briefing.rs` if this needs to change.
