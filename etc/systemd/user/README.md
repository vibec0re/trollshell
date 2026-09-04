# Session units for niri+trollshell

Top-level entry point for "how do I bring up the whole DE." This directory
ships the systemd user units that, taken together, define the niri session:
trollshell (bar, drawer, the cluster of services it hosts — including the
native idle → dim → lock → suspend manager) and the wallpaper renderer. They
all hang off a single umbrella target, `niri-session.target`, which the niri
compositor pulls in at startup.

The component-level README (`../../wallpaper/README.md`) documents the
wallpaper piece in isolation; this README covers the glue. The idle pipeline is
in-process (`crates/hytte-services/src/idle_notify.rs`), not a separate unit.

## Dependency graph

```
graphical-session.target              (systemd's wayland-session anchor)
        │
        │ Requires + After
        ▼
niri-session.target                   (this directory's umbrella target)
        │
        │ pulls in (WantedBy)
        ├── trollshell.service        (bar + drawer + screensaver
        │                              + idle manager + bluetooth-audio
        │                              + … all in-process; also launches each
        │                              declared plugin as a transient
        │                              trollshell-plugin-<id>.service — see
        │                              "Out-of-process widget plugins" below)
        ├── swaybg.service            (wallpaper)
        └── polkit-gnome-authentication-agent-1.service  (polkit prompts)
```

`niri-session.target` is `Requires=graphical-session.target`, so the chain
bottoms out cleanly at systemd's standard wayland-session anchor. The child
units are `PartOf=niri-session.target` — when the target stops (niri exits),
they all stop. They're also `Requisite=graphical-session.target` and
`After=graphical-session.target` so the actual ordering is anchored on the
real wayland-session target, not our umbrella.

## What's inside trollshell.service

A deliberately incomplete list, because the answer is "almost everything":

- The bar and the drawer (GTK4 + libadwaita).
- The ScreenSaver D-Bus interface (#29).
- The native idle → dim → lock → suspend manager (#204) — an
  `ext-idle-notify-v1` client that replaced swayidle. Needs `brightnessctl`
  on `PATH` for the dim step.
- The bluetooth-audio auto-switch service (#37).
- The OSD popup (#30), pipewire / brightness / mpris / network / etc.

We intentionally don't ship separate units for any of those — they're all
event loops on the same GLib main context as the bar, and splitting them
into independent services would only add D-Bus indirection and lifecycle
complexity for no gain. The idle pipeline lived in swayidle before #204; owning
it in-process is what lets it honor logind inhibitors natively.

The one exception (swaybg) is an external binary that already runs fine as its
own unit; wrapping it in trollshell would just add a supervisor we don't need.

## Out-of-process widget plugins

A widget plugin is a _separate_ process that links **no GTK** — it speaks a
small MessagePack wire protocol (`hytte-plugin-proto`) to trollshell over the
same-user socket `$XDG_RUNTIME_DIR/trollshell/plugin.sock`. trollshell is the
host: it renders the plugin's declarative widget tree, brokers its effects,
and pushes back a subscribed subset of shell state. `hytte-plugin-clock-demo`
is the **reference plugin** for the "frontend B" plugin architecture (#35): it
mounts a clock in the sidebar's top slot and opens the power menu when its
button is clicked.

**The declarative launcher is the path plugins run through (#872).** This
directory does not ship a unit per plugin. Instead: `programs.trollshell.plugins`
(home-manager/NixOS) renders a small state file
(`~/.config/trollshell/plugins.json` or `/etc/xdg/…`), and the running shell
launches each enabled entry itself as a **transient** user unit named
`trollshell-plugin-<id>.service`, via `systemd-run --user`
(`trollshell/src/plugin_launcher.rs`). Supervision is still systemd's
(`Restart=on-failure`, `PartOf=` the session target) — only the unit's origin
changed. The host discovers a plugin by who connects to the socket, not by
scanning a directory, so "enable a plugin" is just "declare it enabled and let
the shell (re)launch it."

Without nix, the same state file is hand-editable: add a `plugins.json` entry
(`exec`, `env`, `enabled`, …) and either restart trollshell or poke
`Control.ReloadPlugins` (below) to converge, or drive a plugin at runtime from
the control-center's Plugins tab (start/stop — a runtime action, not a
declaration). A hand-written static unit still works too, as long as its id
isn't _also_ declared in `plugins.json`: the launcher's reconcile fingerprints
only the units it spawns, and deliberately leaves alone any running unit that
carries no fingerprint and isn't declared
(`trollshell/src/plugin_launcher.rs:565-597`,
`plan_never_touches_units_it_did_not_launch`) — but the repo no longer ships
templates for one; the 11 that used to live in this directory were retired in
#872.

> Since #707 that `PartOf=` is the state file's top-level `"target"`, which
> home-manager renders from `programs.trollshell.systemd.target` — **the same
> target the shell's own unit binds to**. It used to be hardcoded to
> `graphical-session.target`, so a session using the documented
> `niri-session.target` (what this directory ships) had the shell on one target
> and its plugins on another, and teardown reached them out of step. A
> `plugins.json` with no `"target"` still means `graphical-session.target`, so
> nothing older breaks. Check what a running plugin actually got with
> `systemctl --user show -p PartOf trollshell-plugin-<id>`.
>
> ### How a config change reaches a running plugin (#419 → #695 → #707)
>
> The units are **transient** — created by the shell at runtime — so there is no
> unit file on disk for activation to diff or `systemctl daemon-reload` to pick
> up. Editing `programs.trollshell.plugins.<id>.env` and switching therefore
> can't reach the running process the way a normal unit change would. The chain
> that makes it work instead:
>
> 1. **nix rewrites the state file.** A switch renders
>    `~/.config/trollshell/plugins.json` afresh from the option — every plugin's
>    `exec` (a store path, so a rebuilt `package` is a new value), `env`,
>    `secrets`, `enabled`, plus the session `target`.
> 2. **something pokes `Control.ReloadPlugins`.** The home-manager module does
>    it from its activation script (`busctl --user call … ReloadPlugins`, with a
>    `|| true` so a switch outside a graphical session is a no-op). The shell
>    also reconciles on its own at startup, which is the only path a
>    NixOS-module deployment gets — root activation has no user session bus.
> 3. **the shell reconciles** (#695): it lists the live `trollshell-plugin-*`
>    units, diffs them against the freshly read file, and starts what was added
>    or enabled, stops what was disabled or removed, and **restarts** anything
>    whose declared spec changed.
> 4. **the diff runs on a fingerprint** stamped into each unit's `Description=`
>    at spawn, covering `exec` + `env` + `secrets` + a non-default `target`.
>    `systemctl --user show -p Description trollshell-plugin-pet` reads it
>    back; a unit the shell never stamped (a hand-written static unit whose id
>    isn't declared in `plugins.json`) is never touched.
>
> So: **under home-manager a `switch` applies live.** Under the NixOS module,
> or if the shell wasn't running when you switched, a changed `env`/`package`
> lands on the next `systemctl --user restart trollshell` or login. To force it
> by hand at any time:
>
> ```sh
> busctl --user call mov.vibec0re.trollshell.Control \
>   /mov/vibec0re/trollshell/Control \
>   mov.vibec0re.trollshell.Control ReloadPlugins
> ```
>
> Two things deliberately do **not** ride this path: a **secret** rotated in the
> control-center's AI Keys tab (that has its own precise relaunch, #392 — the
> values never enter `plugins.json`), and the control-center Plugins tab's
> start/stop, which is a runtime action rather than a declaration. Since #707
> that tab's `StartPlugin`/`StopPlugin`/`SetPluginEnabled` **report failure**
> back over D-Bus instead of only logging it, so a start whose unit never came
> up surfaces as an error rather than as apparent success — check the journal
> for the cause either way.
>
> Since #558 each bundled plugin also ships as its own flake package
> (`hytte-plugin-<id>`), so the declarative path needs no out-of-tree
> derivation — point `package` straight at it:
> `programs.trollshell.plugins.pet.package =
trollshell.packages.${system}.hytte-plugin-pet;`. Outside nix, point `exec`
> in a hand-edited `plugins.json` at wherever you installed the binary; there
> is no static-unit fallback to fall back to (#872).

The **pet** plugin (`hytte-plugin-pet`, #276) is the second in-tree plugin: a
kaomoji cat in the sidebar's top slot — poke it by clicking. It shares the
`SidebarTop` mount with the clock demo, but since #274 that mount is a
**region** holding N plugin cards (sorted by each plugin's manifest `order`),
so the two **coexist** — the old `Conflicts=` is gone; enable either or both.
Its optional brain is `trollshell-pet-brain.service`, a local `llama-server`
(nixpkgs `llama-cpp`) holding a small chat model — the unit's comments cover
fetching one; without it the pet falls back to canned lines and loses no
function beyond variety. Under home-manager that unit is
`programs.trollshell.petBrain.enable` (#694) rather than a hand-install; see
["The declarative path"](#the-declarative-path-home-manager) below, which covers
it alongside the Claude bridge — the other backend `PET_LLM_URL` can point at.

The **weather** plugin (`hytte-plugin-weather`) is the weather card, migrated
out-of-process (#290): it mounts `SidebarLead` — the leading region above the
built-in cards, rendering above calendar/tasks (the after-tasks `SidebarTop`
region can't reach that high) — where the native card used to anchor. It
geolocates via geoclue2 (system bus) and, when geoclue is unavailable/denied,
forward-geocodes `$TROLLSHELL_WEATHER_CITY` — the same env var the retired
in-shell card honored (set it via the plugin's declared `env`, e.g.
`programs.trollshell.plugins.weather.env.TROLLSHELL_WEATHER_CITY` or the
matching `plugins.json` entry). Being a separate process it links **no GTK**:
it talks D-Bus over its own `zbus` connection and fetches open-meteo directly.
**Click the card to refresh now.** The native card is gone — its widget mount
and open-edge refresh call were removed in the #290 follow-up — so bringing up
weather is just enabling the plugin.

The **departures** plugin (`hytte-plugin-departures`) is the departures
board, migrated out-of-process (#289): it mounts `SidebarBottom` — where the
native board used to anchor — and renders the same S-Bahn list. It reads its
station from the same `~/.config/trollshell/places.toml` the shell already
writes, and it is **visibility-gated**: the poller parks (no HTTP) while the
sidebar is closed and does an immediate refresh on open, matching the retired
native board's poll-only-while-open energy behavior. The native board is
gone — its widget mount and open-edge refresh wiring were removed in the #289
follow-up — so bringing up departures is just enabling the plugin.

The **usage** plugin (`hytte-plugin-usage`) is the Claude usage-limits
monitor (#320): a `SidebarTop` card that renders how much of a spend budget
has been **burned within a window** as an accent-tinted gauge — a slow,
**visibility-gated** `ureq` poll (60 s, parked while the sidebar is closed) of
the exponentials Grafana **public dashboard**. The metric is honestly
_spend_, not rolling rate-limit headroom (Claude Code's OTEL surface exports
`claude_code.cost.usage` / `token.usage`, not the `/usage` window
percentages), so the card reads "burned ÷ a budget you set". **The dashboard
URL is configuration, not a build input**: with none set the card renders a
calm "no dashboard configured" empty state and makes zero network calls — set
`TROLLSHELL_USAGE_DASHBOARD_URL` (the `…/public-dashboards/<token>` link, a
secret kept in the plugin's declared env, not in a nix store path) to go
live.
`TROLLSHELL_USAGE_BUDGET` is the optional gauge denominator; `_PANEL` pins the
panel id (else the first value panel is discovered); `_WINDOW` sets the range
(default `now-5h`). All four also read from `~/.config/trollshell/usage.toml`
(`dashboard_url` / `budget` / `panel` / `window`) as an env fallback. Clicking
the card opens its own drawer panel (the gauge + the window figures +
last-updated). It links no GTK and asks the host for nothing but `OpenPage`.

The **preem-demo** plugin (`hytte-plugin-preem-demo`) is the showcase for the
`hytte_plugin::preem` raster kit (#356): a sidebar card cycling the retro
display widgets — a 7-seg HH:MM clock, a dot-matrix ticker, and an 8-bit
textbox — through the VFD / LCD / OLED skins (rotating every 10 s; click the
clock face to cycle a skin by hand). It runs no timers of its own: every
animation frame derives from the Clock snapshots the host already pushes. It
mounts `SidebarTop` alongside the clock demo and the pet, and doubles as the
visual reference for kit consumers.

The **terminal** plugin (`hytte-plugin-terminal`) is the micro-terminal demo
(#357): a sidebar card composing the two pieces the preem-widgets breakout
landed — the
`Node::Entry` text input and the `hytte_plugin::preem` raster kit — into a
retro VFD "screen" of scrollback over a single entry line. Type a line, press
Enter and it is echoed onto the screen behind a `> ` prompt (scrolling when the
screen fills) and the entry clears. It is **pure local echo**: submitted text is
only ever painted back — the plugin executes nothing, spawns no process, and
requests no capabilities. It mounts `SidebarTop` after the preem showcase.

The **caw** plugin (`hytte-plugin-caw`) is caw's desktop body (#359): a chunky-pixel
cybercrow mounting `SidebarTop` alongside the clock demo, pet, and terminal
(order-sorted, so it coexists — no `Conflicts=` needed). Unlike the pet, there
is **no separate brain service** — caw herself is the brain: the opencaw agent
publishes her live mood, a line, and a chaos level via its `caw_express` tool
into a small JSON file, and this plugin polls it and renders her as a
procedural LCD corvid face (7 moods, glowing chaos-scaled eyes) plus
pixel-font speech in her own palette. Poke her for a reaction; she dozes off
when she hasn't expressed in a while. The expression file path is
`CAW_EXPRESSION_PATH` (default `~/.local/state/caw/expression.json`) — point
both this plugin and opencaw at the same path if you override it.

The **timer** plugin (`hytte-plugin-timer`) is a pomodoro / kitchen timer
(#406) and the first bundled plugin to mount `Mount::BarRight` — a **bar
chip** (a
seven-segment `MM:SS` countdown) rather than a sidebar card, so there's no
region to coexist in. Clicking the chip opens its own drawer panel: the big
readout, a duration entry (`25`, `25m`, `5:00`, `1:30:00`, or `pomo`), the
25/5/15 presets, and pause/reset. The countdown lives entirely in the plugin —
ticking at 1 Hz whether or not the chip is on screen, so a host restart just
re-seeds a fresh idle 25:00 timer, nothing to persist. At zero it posts one
toast through trollshell's own notification path via `Effect::Notify` (#406's
payoff, the first customer of that effect) — attributed to the plugin id as
the app name, no extra unit or D-Bus setup needed. No environment variables to
set.

The **infobroker** plugin (`hytte-plugin-infobroker`, #487) is the
consent-gated data broker that lets a local AI agent read scoped desktop data.
Like the timer it's a `Mount::BarRight` bar chip (a shield; a warning triangle +
badge when an agent is knocking), and clicking it opens its own drawer panel —
the durable grants (with per-row **Revoke**), the pending knocks (with one-click
**Allow**), the datasource status, the live sessions, and a recent-requests
audit trail. Unlike the other plugins it **also binds its own socket**,
`$XDG_RUNTIME_DIR/hytte-infobroker.sock` (0600, same-user-only), which the
separate `hytte-infobroker` CLI dials so an agent can `auth` (mint a session
token) and `get` scoped data. Durable grants persist to
`$XDG_STATE_HOME/hytte-infobroker/grants.toml` (or `~/.local/state/…`); session
tokens are in-memory only (12 h TTL, killed by a panel revoke or a restart of
this unit / trollshell). The first request from an agent with no standing
grant fires an interactive Allow/Deny consent prompt at the human
(`Effect::RequestConsent`, rendered by `trollshell/src/overlays/consent.rs`);
a settled standing **deny** instead answers silently with one informational
toast via `Effect::Notify` so the knock stays visible without re-prompting.
The agent-facing skill folder lives at
`etc/skills/infobroker/` (`SKILL.md` + a `bin/hytte-infobroker` wrapper). No
environment variables to set.

The **audio-widget** plugin (`hytte-plugin-audio-widget`) is the
audio-reactive sidebar card (#506), reworked into a y2k USB-MP3-player face
plate by #840: a
`Mount::SidebarBottom` card (`order = 1`, so it sits below pet and departures)
composing four `hytte_plugin::preem` raster widgets off the
`StateKey::AudioSpectrum` push (#405) — a dot-matrix marquee, a `MM:SS/MM:SS`
time readout, a 16-band spectrum scope, and the LED peak/level strip #506 adds
to the kit (a row of discrete LEDs lighting with the overall level, topped by a
peak-hold dot that floats and decays) — over a prev / play-pause / next
transport row. It runs its own ~20 Hz frame timer for the animation and parks it
while the sidebar is closed (`StateKey::SlotVisible`). The shell only runs the
PipeWire monitor tap while at least one spectrum subscriber is connected, so an
idle desktop with this plugin off pays nothing. The marquee scrolls the current
track (`title — artist`) off the `StateKey::NowPlaying` push (#528, projecting
`hytte_services::mpris` the way #405 did for the spectrum), falling back to a
decorative banner when nothing is playing; the readout comes off that same
push's `position_us`/`length_us` (#840). The transport row needs
`Capability::Media` — the buttons emit `Effect::Media`, which the shell's broker
routes to whichever player is active (#648), so the card never names a player.
Because the card consumes `position_us`, an open sidebar also un-parks the
shell's 250 ms mpris position poller (#228's gate is the OR of that and the
Media drawer page); closing the sidebar parks it again. No environment variables
to set.

## The Claude bridge (`trollshell-claude-bridge.service`)

`trollshell-claude-bridge.service` is **not** a plugin — it's a small daemon
that puts an OpenAI-compatible face on headless Claude Code (#584), so the
LLM-backed plugins can ride a Claude Code subscription instead of a metered
cloud key. It serves exactly one route, `POST /v1/chat/completions`, on
**`127.0.0.1:8787` — loopback only** (8080 is `trollshell-pet-brain.service`'s
llama-server; the two are meant to be swappable). Each request spawns
`claude --print` and returns its answer as `choices[0].message.content`.

**Nothing in pet or caw changed for this.** `hytte_ai_providers::Provider` is
already just a base URL, so opting a plugin in is two env vars on _its_
declared env (`programs.trollshell.plugins.pet.env`, or the matching
`plugins.json` entry):

```nix
programs.trollshell.plugins.pet.env = {
  PET_LLM_URL = "http://127.0.0.1:8787";
  OPENROUTER_API_KEY = "local-bridge";
};
```

That second line is a **security control, not cosmetics**. The bridge is
_keyless_ — it validates no bearer token at all, because `brain.rs` resolves
`load_key("openrouter")` _before_ `PET_LLM_API_KEY`, so a bridge demanding its
own token would 401 every request forever. `load_key` checks the
`OPENROUTER_API_KEY` env override _before_ `~/.config/trollshell/openrouter.key`,
so the dummy value is what stops the real cloud key being shipped to a loopback
port. With no auth, reachability is the authorization boundary — hence the
loopback bind.

The bridge's own unit carries
`UnsetEnvironment=ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN CLAUDE_CODE_USE_BEDROCK CLAUDE_CODE_USE_VERTEX`,
because any of those would move billing off the subscription with nothing
visible to show for it. In the two `claude` modes the binary **refuses to
start** if it still finds one set (it can't scrub them itself:
`std::env::remove_var` is unsafe under edition 2024 and this workspace forbids
unsafe). If the unit fails with "refusing to start", that message names the
variable to unset. The refusal is **mode-scoped** (#730): it guards the `claude`
child, and `api` mode spawns none — see below.

Prerequisite: the `claude` CLI on the user manager's `PATH`, already logged in
(`api` mode excepted — it spawns no `claude` at all). Optional knobs —
`CLAUDE_BRIDGE_{MODEL,MODE,PORT,TIMEOUT_SECS,STATE_DIR,THINKING}` — are
documented in the unit's comments. `CLAUDE_BRIDGE_MODE` picks between **three**
backends:

| `CLAUDE_BRIDGE_MODE`               | what it runs                                                                                                             |
| ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `subscription` (default)           | a persisted `claude` session per conversation, resumed with only the newest message so the prompt prefix stays cacheable |
| `reprompt`                         | a one-off `claude` session per turn, with the bridge holding the transcript and nothing persisted to disk                |
| `api` (also `api-key`, `messages`) | no `claude` child at all: `POST /v1/messages` against the Anthropic API, **billed per token** (#730/#751)                |

`api` mode is the reason the `UnsetEnvironment=` line above is worth
understanding rather than just keeping. That mode needs an Anthropic key, and
the shipped unit strips `ANTHROPIC_API_KEY` from its environment — deliberately
(#752): an inherited key that quietly starts billing is a worse failure than a
mode that has to read its key from a file. So under this unit the **only** place
the key can come from is

```
~/.config/trollshell/anthropic.key      # chmod 600; trimmed, empty == unset
```

Adding `Environment=ANTHROPIC_API_KEY=…` to the unit is a no-op — the scrub runs
after it, the bridge finds no key, and refuses to start. The env override exists
for `cargo run`, not for systemd. `CLAUDE_BRIDGE_THINKING` (`disabled` by
default, or `adaptive` / `auto`) is `api`-mode-only and load-bearing there:
`max_tokens` bounds thinking _plus_ answer text, and these consumers ask for
only a couple of hundred tokens of kaomoji.

**Two timeouts, and their order matters.** `CLAUDE_BRIDGE_TIMEOUT_SECS` (8s by
default) is the bridge's per-request budget; the _client_'s budget is the pet's
`PET_LLM_TIMEOUT_SECS` (10s by default, #699 — before that it was a hardcoded
10s in `hytte-ai-providers`). The bridge's has to stay strictly **under** the
client's, or the client hangs up mid-read and the plugin sees a torn connection
instead of a clean 504 it can fall back from. A cold `claude --print` turn on a
fresh session routinely runs past 10s, so the fix is to raise **both, in that
order**:

```ini
# on the pet plugin's declared env — the client's ceiling goes up first
PET_LLM_TIMEOUT_SECS=30
# in trollshell-claude-bridge.service — then the bridge, still below it
Environment=CLAUDE_BRIDGE_TIMEOUT_SECS=25
```

When it's down, the plugins degrade to their canned output — the same failure
mode as a missing llama-server — with the cause only in the journal.

### The declarative path (home-manager)

Since #694 both this unit and `trollshell-pet-brain.service` have a module
surface, so home-manager users don't hand-install either:

```nix
programs.trollshell.claudeBridge = {
  enable = true;
  # Worth pinning: left null the child runs on whatever ~/.claude/settings.json
  # picks — usually Opus — which routinely overruns the 8s budget below and
  # comes back to the plugin as a 504.
  model = "claude-haiku-4-5";
  # port = 8787;          # default; must match the plugin's *_LLM_URL
  # timeoutSeconds = 8;   # default; see "Two timeouts" above
};

# The client half, on the plugin that talks to it.
programs.trollshell.plugins.pet.env = {
  PET_LLM_URL = "http://127.0.0.1:8787";
  OPENROUTER_API_KEY = "local-bridge";
};
```

The generated unit carries the same `UnsetEnvironment=` scrub as the file in
this directory, and the module **asserts the two-timeout ordering** at
evaluation time whenever both halves are declared — so a `timeoutSeconds` that
isn't strictly below the pet's `PET_LLM_TIMEOUT_SECS` fails the build instead of
the request.

`programs.trollshell.petBrain.{enable,package,port,model,extraArgs}` is the same
deal for the llama-server brain. Its `model` is the path to a GGUF you fetch
yourself (the option's description carries the download snippet); the unit is
`ConditionPathExists`-gated on it, so declaring it before downloading is inert
rather than a crash loop.

Anything not promoted to an option stays reachable the ordinary home-manager
way — the list is concatenated onto the one the module already sets, and
systemd lets the later assignment win:

```nix
systemd.user.services.trollshell-claude-bridge.Service.Environment = [
  "CLAUDE_BRIDGE_MODE=reprompt"
  # …and PATH, if your `claude` lives somewhere the systemd user manager's PATH
  # doesn't reach. The module deliberately sets no PATH= of its own: it would
  # *replace* the inherited one, which on NixOS already covers the system and
  # per-user profiles.
];
```

llama-server reads no environment configuration at all, so `petBrain.extraArgs`
(rather than a `Service.Environment` override) is that unit's escape hatch.

Both option groups are **home-manager only**, the same call `nightlight` made in
#657: a NixOS-only deployment has no per-user `claude` login — or per-user model
download — for these to drive, so the NixOS module asserts rather than accepting
the setting and starting nothing. The static units in this directory remain the
supported path for a hand-installed (non-home-manager) deployment.

## Required packages

Just niri itself — the components have their own package lists:

- `niri` — the compositor. Provides `graphical-session.target` once it's
  up and running.
- `polkit-gnome` — the standalone polkit authentication agent. Provides the
  `polkit-gnome-authentication-agent-1` binary the new unit runs. (Swap for
  `mate-polkit` / `hyprpolkitagent` if you prefer; adjust the unit's
  `ExecStart` accordingly.)

Each child unit's README documents its own dependencies (swaybg pulls in
swaybg the binary; trollshell is this repo's `cargo build --release` output and
wants `brightnessctl` on `PATH` for the idle manager's dim step).

Install niri on Arch:

```sh
sudo pacman -S niri
```

(Or use the `niri-git` AUR package if you want recent main.)

## Install sequence

From the repo root:

```sh
mkdir -p ~/.config/systemd/user

ln -sf "$PWD/etc/systemd/user/niri-session.target" \
       ~/.config/systemd/user/niri-session.target
ln -sf "$PWD/etc/systemd/user/trollshell.service" \
       ~/.config/systemd/user/trollshell.service
ln -sf "$PWD/etc/systemd/user/swaybg.service"     \
       ~/.config/systemd/user/swaybg.service
ln -sf "$PWD/etc/systemd/user/polkit-gnome-authentication-agent-1.service" \
       ~/.config/systemd/user/polkit-gnome-authentication-agent-1.service

systemctl --user daemon-reload
systemctl --user enable trollshell.service swaybg.service \
                        polkit-gnome-authentication-agent-1.service
```

This sequence installs no plugin — plugins aren't hand-installed units
anymore (#872). See "Out-of-process widget plugins" above for how to declare
one; the shell launches it itself once trollshell.service is up.

`enable` (no `--now`) is correct here — these units start when
`niri-session.target` is started by niri at session login. Starting them
manually outside a niri session will fail the `Requisite=` check, which is
the desired behaviour. If you've installed but not yet logged into niri,
just log in.

## Niri config

Add the spawn-at-startup line from `../../niri/session.kdl` to your
`~/.config/niri/config.kdl` as a top-level node (NOT inside `binds { }`):

```
spawn-at-startup "/bin/sh" "-c" "systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP && systemctl --user start niri-session.target"
```

The `import-environment` half copies `WAYLAND_DISPLAY` and
`XDG_CURRENT_DESKTOP` from niri's process env into the user systemd
manager's environment, so D-Bus-activated services and `.desktop` launchers
see them. Without it, GTK clients started via a launcher can't find the
display. `XDG_CURRENT_DESKTOP=niri:GNOME` itself is set elsewhere — see
the README for #26 (xdg-desktop-portal) which configures `environment.d`.

We chose to ship this snippet as a separate file (`etc/niri/session.kdl`)
rather than appending to `etc/niri/binds.kdl` because the two are
structurally distinct: binds live in a `binds { }` block, while
`spawn-at-startup` is a top-level node. Keeping them in separate snippets
makes it obvious which goes where.

## Trollshell binary path

The shipped `trollshell.service` hardcodes `/usr/local/bin/trollshell`,
which is what `cargo install --root /usr/local --path trollshell` produces
and is the documented install location. To run a different build (dev
checkout, `~/.cargo/bin`, `~/.local/bin`, etc.) without editing the unit
in place, drop in an override:

```sh
systemctl --user edit trollshell.service
```

…and add:

```ini
[Service]
ExecStart=
ExecStart=%h/src/trollshell-workspace/target/release/trollshell
```

The empty `ExecStart=` clears the unit's value before the override; without
it systemd appends and refuses to start with two ExecStart= lines on a
non-`Type=oneshot` service. After saving, `systemctl --user daemon-reload`
and restart the session (or just the service).

## Verification

After logging into niri, the umbrella target and its child units
should be active:

```sh
systemctl --user status niri-session.target
systemctl --user list-dependencies niri-session.target
```

`status` prints `active`; `list-dependencies` shows the tree with
trollshell.service / swaybg.service all marked active.

To watch the chain start in real time on the next login:

```sh
journalctl --user -f -u niri-session.target \
                    -u trollshell.service   \
                    -u swaybg.service
```

## Troubleshooting

- **trollshell crashes on startup.** `systemctl --user status
trollshell.service` shows the exit code; `journalctl --user -u
trollshell.service -b` has the panic / log output. `Restart=on-failure`
  - `RestartSec=2` will rate-limit retries; if it's a real bug, the unit
    goes into `failed` state after systemd's default start-limit kicks in.
- **swaybg restart-loops.** Almost always a bad / missing
  `~/.config/trollshell/wallpaper.path`. See `../../wallpaper/README.md`.
- **The idle lock never fires.** The native idle manager runs inside
  `trollshell.service` and locks via `loginctl lock-session`, which the
  session's `swaylock` picks up — make sure `/etc/pam.d/swaylock` exists
  (NixOS: `security.pam.services.swaylock = {};`), or `swaylock` can never
  verify a password. Check `journalctl --user -u trollshell.service` for the
  `hytte_services::idle_notify` log lines. (Dim needs `brightnessctl` on
  `PATH`.)
- **`niri-session.target` is `inactive`.** niri's spawn-at-startup didn't
  fire, or `import-environment` failed. Log into niri however you normally
  do (greetd, TTY exec, etc.), then watch `journalctl --user -f` for the
  target startup and the spawn-at-startup line.
- **A unit refuses to start with `Failed condition`.** That's the
  `Requisite=` check — `niri-session.target` (or `graphical-session.target`)
  isn't active. Make sure you're inside a niri session; these units
  intentionally won't start outside one.
- **GTK apps from a launcher complain about `WAYLAND_DISPLAY`.** The
  `import-environment` half of the spawn-at-startup didn't run. Check
  the env on the running shell: `systemctl --user show-environment` should
  list `WAYLAND_DISPLAY=wayland-1` (or similar) and `XDG_CURRENT_DESKTOP`.

## What this does NOT do

- It does NOT ship a complete `niri/config.kdl`. Only the spawn-at-startup
  fragment relevant to session bring-up lives here; everything else
  (layout, output, input) is the user's call. Keybinds are in
  `../../niri/binds.kdl`, also as a snippet.
- It runs polkit's auth agent as its OWN unit
  (`polkit-gnome-authentication-agent-1.service`), not inside trollshell.
  trollshell used to register an in-process agent; it now delegates to the
  standard standalone polkit-gnome agent. The remaining in-process pieces
  (screensaver, bluetooth-audio, OSD, the media-key services) DO stay inside
  `trollshell.service` — splitting them into their own units would just add
  D-Bus round-trips between things that already share a main loop.
- It does NOT install niri itself. Use your distro package or the niri
  upstream's install instructions; this directory only wires niri up
  _after_ it's running.
- It does NOT set `XDG_CURRENT_DESKTOP` or other env vars. That's
  `environment.d`'s job — see the README for task #26.
