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
        ├── trollshell.service        (bar + drawer + polkit + screensaver
        │                              + bluetooth-audio + … all in-process)
        ├── swayidle.service          (dim → lock → suspend)
        └── swaybg.service            (wallpaper)
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
- The polkit auth agent (#27) — registered via `.with(polkit::service())`
  inside the trollshell binary, so it lives in the same process.
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

## Required packages

Just niri itself — the components have their own package lists:

- `niri` — the compositor. Provides `graphical-session.target` once it's
  up and running.

Each child unit's README documents its own dependencies (swayidle pulls in
gtklock + brightnessctl; swaybg pulls in swaybg the binary; trollshell is
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

systemctl --user daemon-reload
systemctl --user enable trollshell.service swayidle.service swaybg.service
```

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
  + `RestartSec=2` will rate-limit retries; if it's a real bug, the unit
  goes into `failed` state after systemd's default start-limit kicks in.
- **swaybg restart-loops.** Almost always a bad / missing
  `~/.config/trollshell/wallpaper.path`. See `../../wallpaper/README.md`.
- **swayidle starts but the lock never fires.** gtklock (or your chosen
  locker) isn't installed. See `../../swayidle/README.md`.
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
- It does NOT add a polkit-agent unit. The polkit agent is part of
  trollshell itself (`.with(polkit::service())` in the trollshell binary),
  so it lives inside `trollshell.service`'s process. Same for screensaver,
  bluetooth-audio, OSD, and the various media-key services — splitting any
  of them into their own unit would just add D-Bus round-trips between
  things that already share a main loop.
- It does NOT install niri itself. Use your distro package or the niri
  upstream's install instructions; this directory only wires niri up
  *after* it's running.
- It does NOT set `XDG_CURRENT_DESKTOP` or other env vars. That's
  `environment.d`'s job — see the README for task #26.
