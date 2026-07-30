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
        │                              + … all in-process)
        ├── trollshell-plugin-clock-demo.service  (out-of-process widget plugin)
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

`trollshell-plugin-clock-demo.service` is the **reference plugin** for the
"frontend B" plugin architecture (#35). Unlike everything above, a plugin is a
_separate_ process that links **no GTK** — it speaks a small MessagePack wire
protocol (`hytte-plugin-proto`) to trollshell over the same-user socket
`$XDG_RUNTIME_DIR/trollshell/plugin.sock`. trollshell is the host: it renders
the plugin's declarative widget tree, brokers its effects, and pushes back a
subscribed subset of shell state. The demo mounts a clock in the sidebar's top
slot and opens the power menu when its button is clicked.

"Enable a plugin" is just "enable its unit" — the host discovers plugins by who
connects, not by scanning a directory. The unit is ordered `After=`
`trollshell.service` so the host socket is usually up first, but the plugin
also dials with a bounded backoff, so a host that isn't up yet (or a host
restart) is ridden out in-process rather than crash-looping; systemd's
`Restart=on-failure` is the outer supervisor. Run more plugins by shipping one
unit per plugin binary on the same pattern.

> **Nix-managed sessions** don't hand-install these units: since #419 the
> `programs.trollshell.plugins` option renders to a declarative state file
> (`~/.config/trollshell/plugins.json` or `/etc/xdg/…`), and the running shell
> launches each enabled plugin itself as a **transient** user unit of the same
> `trollshell-plugin-<id>.service` name via `systemd-run --user`
> (`trollshell/src/plugin_launcher.rs`). Supervision is still systemd's
> (`Restart=on-failure`, `PartOf=graphical-session.target`), and the static
> units below keep working as the manual path — the socket doesn't care who
> spawned the plugin.
>
> Since #558 each bundled plugin also ships as its own flake package
> (`hytte-plugin-<id>`), so the declarative path needs no out-of-tree
> derivation — point `package` straight at it:
> `programs.trollshell.plugins.pet.package =
trollshell.packages.${system}.hytte-plugin-pet;`. The static units below
> remain the legacy fallback (they exec hand-copied binaries, not a nix path).

`trollshell-plugin-pet.service` is the second in-tree plugin (#276): a
kaomoji cat in the sidebar's top slot — poke it by clicking. It shares the
`SidebarTop` mount with the clock demo, but since #274 that mount is a
**region** holding N plugin cards (sorted by each plugin's manifest `order`),
so the two **coexist** — the old `Conflicts=` is gone; enable either or both.
Its optional brain is `trollshell-pet-brain.service`, a local `llama-server`
(nixpkgs `llama-cpp`) holding a small chat model — the unit's comments cover
fetching one; without it the pet falls back to canned lines and loses no
function beyond variety.

`trollshell-plugin-weather.service` is the weather card, migrated
out-of-process (#290): it mounts `SidebarLead` — the leading region above the
built-in cards, rendering above calendar/tasks (the after-tasks `SidebarTop`
region can't reach that high) — where the native card used to anchor. It
geolocates via geoclue2 (system bus) and, when geoclue is unavailable/denied,
forward-geocodes `$TROLLSHELL_WEATHER_CITY` — the same env var the retired
in-shell card honored (see the unit's commented `Environment=` line). Being a
separate process it links **no GTK**: it talks D-Bus over its own `zbus`
connection and fetches open-meteo directly. **Click the card to refresh now.**
The native card is gone — its widget mount and open-edge refresh call were
removed in the #290 follow-up — so bringing up weather is just enabling this
unit.

`trollshell-plugin-departures.service` is the departures board, migrated
out-of-process (#289): it mounts `SidebarBottom` — where the native board used
to anchor — and renders the same S-Bahn list. It reads its station from the
same `~/.config/trollshell/places.toml` the shell already writes, and it is
**visibility-gated**: the poller parks (no HTTP) while the sidebar is closed
and does an immediate refresh on open, matching the retired native board's
poll-only-while-open energy behavior. The native board is gone — its widget
mount and open-edge refresh wiring were removed in the #289 follow-up — so
bringing up departures is just enabling this unit.

`trollshell-plugin-usage.service` is the Claude usage-limits monitor (#320): a
`SidebarTop` card that renders how much of a spend budget has been **burned
within a window** as an accent-tinted gauge — a slow, **visibility-gated**
`ureq` poll (60 s, parked while the sidebar is closed) of the exponentials
Grafana **public dashboard**. The metric is honestly _spend_, not rolling
rate-limit headroom (Claude Code's OTEL surface exports `claude_code.cost.usage`
/ `token.usage`, not the `/usage` window percentages), so the card reads
"burned ÷ a budget you set". **The dashboard URL is configuration, not a build
input**: with none set the card renders a calm "no dashboard configured" empty
state and makes zero network calls — set `TROLLSHELL_USAGE_DASHBOARD_URL` (the
`…/public-dashboards/<token>` link, a secret kept in the unit) to go live.
`TROLLSHELL_USAGE_BUDGET` is the optional gauge denominator; `_PANEL` pins the
panel id (else the first value panel is discovered); `_WINDOW` sets the range
(default `now-5h`). All four also read from `~/.config/trollshell/usage.toml`
(`dashboard_url` / `budget` / `panel` / `window`) as an env fallback. Clicking
the card opens its own drawer panel (the gauge + the window figures +
last-updated). It links no GTK and asks the host for nothing but `OpenPage`.

`trollshell-plugin-preem-demo.service` is the showcase for the
`hytte_plugin::preem` raster kit (#356): a sidebar card cycling the retro
display widgets — a 7-seg HH:MM clock, a dot-matrix ticker, and an 8-bit
textbox — through the VFD / LCD / OLED skins (rotating every 10 s; click the
clock face to cycle a skin by hand). It runs no timers of its own: every
animation frame derives from the Clock snapshots the host already pushes. It
mounts `SidebarTop` alongside the clock demo and the pet, and doubles as the
visual reference for kit consumers.

`trollshell-plugin-terminal.service` is the micro-terminal demo (#357): a
sidebar card composing the two pieces the preem-widgets breakout landed — the
`Node::Entry` text input and the `hytte_plugin::preem` raster kit — into a
retro VFD "screen" of scrollback over a single entry line. Type a line, press
Enter and it is echoed onto the screen behind a `> ` prompt (scrolling when the
screen fills) and the entry clears. It is **pure local echo**: submitted text is
only ever painted back — the plugin executes nothing, spawns no process, and
requests no capabilities. It mounts `SidebarTop` after the preem showcase.

`trollshell-plugin-caw.service` is caw's desktop body (#359): a chunky-pixel
cybercrow mounting `SidebarTop` alongside the clock demo, pet, and terminal
(order-sorted, so it coexists — no `Conflicts=` needed). Unlike the pet, there
is **no separate brain service** — caw herself is the brain: the opencaw agent
publishes her live mood, a line, and a chaos level via its `caw_express` tool
into a small JSON file, and this plugin polls it and renders her as a
procedural LCD corvid face (7 moods, glowing chaos-scaled eyes) plus
pixel-font speech in her own palette. Poke her for a reaction; she dozes off
when she hasn't expressed in a while. The expression file path is
`CAW_EXPRESSION_PATH` (default `~/.local/state/caw/expression.json`) — point
both this unit and opencaw at the same path if you override it.

`trollshell-plugin-timer.service` is a pomodoro / kitchen timer (#406) and the
first bundled plugin to mount `Mount::BarRight` — a **bar chip** (a
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

`trollshell-plugin-infobroker.service` is the **infobroker** (#487): the
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

`trollshell-plugin-audio-widget.service` is the audio-reactive sidebar card
(#506): a `Mount::SidebarTop` card composing three `hytte_plugin::preem` raster
widgets off the `StateKey::AudioSpectrum` push (#405) — a dot-matrix marquee, a
16-band spectrum scope, and the LED peak/level strip this issue adds to the kit
(a row of discrete LEDs lighting with the overall level, topped by a peak-hold
dot that floats and decays). It runs its own ~20 Hz frame timer for the
animation and parks it while the sidebar is closed (`StateKey::SlotVisible`).
The shell only runs the PipeWire monitor tap while at least one spectrum
subscriber is connected, so an idle desktop with this plugin off pays nothing.
The marquee scrolls the current track (`title — artist`) off the
`StateKey::NowPlaying` push (#528, projecting `hytte_services::mpris` the way
#405 did for the spectrum), falling back to a decorative banner when nothing
is playing. No environment variables to set.

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
ln -sf "$PWD/etc/systemd/user/trollshell-plugin-clock-demo.service" \
       ~/.config/systemd/user/trollshell-plugin-clock-demo.service

systemctl --user daemon-reload
systemctl --user enable trollshell.service swaybg.service \
                        polkit-gnome-authentication-agent-1.service \
                        trollshell-plugin-clock-demo.service
```

The `trollshell-plugin-clock-demo.service` unit's `ExecStart` hardcodes
`/usr/local/bin/hytte-plugin-clock-demo` (what `cargo install --root
/usr/local --path crates/hytte-plugin-clock-demo` produces); override it the
same way as `trollshell.service` (below) to run a dev build.

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
