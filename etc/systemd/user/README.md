# Session units for niri+trollshell

Top-level entry point for "how do I bring up the whole DE." This directory
ships the systemd user units that, taken together, define the niri session:
trollshell (bar, drawer, the cluster of services it hosts), the idle daemon,
and the wallpaper renderer. They all hang off a single umbrella target,
`niri-session.target`, which the niri compositor pulls in at startup.

The component-level READMEs (`../../swayidle/README.md`,
`../../wallpaper/README.md`) document each piece in isolation; this README
covers the glue.

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
        │                              + bluetooth-audio + … all in-process)
        ├── trollshell-plugin-clock-demo.service  (out-of-process widget plugin)
        ├── swayidle.service          (dim → lock → suspend)
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
- The bluetooth-audio auto-switch service (#37).
- The OSD popup (#30), pipewire / brightness / mpris / network / etc.

We intentionally don't ship separate units for any of those — they're all
event loops on the same GLib main context as the bar, and splitting them
into independent services would only add D-Bus indirection and lifecycle
complexity for no gain.

The exceptions (swayidle, swaybg) are external binaries that already run
fine as their own units; wrapping them in trollshell would just add a
supervisor we don't need.

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

`trollshell-plugin-pet.service` is the second in-tree plugin (#276): a
kaomoji cat in the sidebar's top slot — poke it by clicking. It shares the
`SidebarTop` mount with the clock demo, but since #274 that mount is a
**region** holding N plugin cards (sorted by each plugin's manifest `order`),
so the two **coexist** — the old `Conflicts=` is gone; enable either or both.
Its optional brain is `trollshell-pet-brain.service`, a local `llama-server`
(nixpkgs `llama-cpp`) holding a small chat model — the unit's comments cover
fetching one; without it the pet falls back to canned lines and loses no
function beyond variety.

`trollshell-plugin-weather.service` is the third in-tree plugin (#290): the
sidebar weather card, out of process. It mounts the same `SidebarTop` region
with `order=-10`, so it renders **above** the pet (which leaves `order` unset →
sorts as `0`). It geolocates via geoclue2 (system bus) and, when geoclue is
unavailable/denied, forward-geocodes `$TROLLSHELL_WEATHER_CITY` — the same env
var the in-shell card honors (see the unit's commented `Environment=` line).
Being a separate process it links **no GTK**: it talks D-Bus over its own
`zbus` connection and fetches open-meteo directly. **Click the card to refresh
now.** During the migration transition it runs *alongside* the built-in weather
card (compare, then remove the native mount — see #290); once removed, the
plugin card takes its place at the top of the sidebar.

## Required packages

Just niri itself — the components have their own package lists:

- `niri` — the compositor. Provides `graphical-session.target` once it's
  up and running.
- `polkit-gnome` — the standalone polkit authentication agent. Provides the
  `polkit-gnome-authentication-agent-1` binary the new unit runs. (Swap for
  `mate-polkit` / `hyprpolkitagent` if you prefer; adjust the unit's
  `ExecStart` accordingly.)

Each child unit's README documents its own dependencies (swayidle pulls in
brightnessctl; swaybg pulls in swaybg the binary; trollshell is
this repo's `cargo build --release` output).

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
ln -sf "$PWD/etc/systemd/user/swayidle.service"   \
       ~/.config/systemd/user/swayidle.service
ln -sf "$PWD/etc/systemd/user/swaybg.service"     \
       ~/.config/systemd/user/swaybg.service
ln -sf "$PWD/etc/systemd/user/polkit-gnome-authentication-agent-1.service" \
       ~/.config/systemd/user/polkit-gnome-authentication-agent-1.service
ln -sf "$PWD/etc/systemd/user/trollshell-plugin-clock-demo.service" \
       ~/.config/systemd/user/trollshell-plugin-clock-demo.service

systemctl --user daemon-reload
systemctl --user enable trollshell.service swayidle.service swaybg.service \
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

After logging into niri, the umbrella target and all three child units
should be active:

```sh
systemctl --user status niri-session.target
systemctl --user list-dependencies niri-session.target
```

`status` prints `active`; `list-dependencies` shows the tree with
trollshell.service / swayidle.service / swaybg.service all marked active.

To watch the chain start in real time on the next login:

```sh
journalctl --user -f -u niri-session.target \
                    -u trollshell.service   \
                    -u swayidle.service     \
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
- **swayidle starts but the lock never fires.** swayidle runs `swaylock`,
  which authenticates via PAM — make sure `/etc/pam.d/swaylock` exists
  (NixOS: `security.pam.services.swaylock = {};`). trollshell no longer
  ships its own locker.
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
