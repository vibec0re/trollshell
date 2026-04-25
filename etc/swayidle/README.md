# swayidle config for niri+trollshell

Drives the idle pipeline: dim the backlight after 4 min, lock at 5 min, suspend
at 10 min, and re-lock right before any suspend so the user must auth on wake.

A native `ext-idle-notify-v1` implementation in `hytte-services` is deferred
(see task #29 for the ScreenSaver D-Bus side). This v1 is config-only.

## Required packages

- `swayidle` — the idle daemon itself (reads `ext-idle-notify-v1` from the
  Wayland compositor; niri implements it).
- `gtklock` — GTK-based screen locker. `waylock` is an acceptable substitute
  if you prefer a smaller TUI-style locker; edit the `config` accordingly.
- `brightnessctl` — used by the `timeout 240` action to save and restore
  backlight level.

Install on Arch:

```sh
sudo pacman -S swayidle gtklock brightnessctl
```

If `gtklock` is missing, swayidle will still fire the timeout but the lock
command silently fails — the screen dims and suspends but is never locked.
Install it before enabling the unit.

## Install the config

swayidle reads `~/.config/swayidle/config` by default; the systemd unit
points at it explicitly via `-C` for clarity. Symlink the shipped file:

```sh
mkdir -p ~/.config/swayidle
ln -s "$PWD/etc/swayidle/config" ~/.config/swayidle/config
```

(Run from the repo root, or substitute the absolute repo path.)
The symlink target is absolute — recreate it if you move or rename the repo.

## Enable the systemd unit

The shipped unit lives at `etc/systemd/user/swayidle.service`. Symlink it
into your user units directory and enable it:

```sh
mkdir -p ~/.config/systemd/user
ln -s "$PWD/etc/systemd/user/swayidle.service" \
      ~/.config/systemd/user/swayidle.service
systemctl --user daemon-reload
systemctl --user enable --now swayidle.service
```

The unit is `PartOf=graphical-session.target` and `WantedBy=graphical-session.target`,
so it starts when the niri session brings up the target and stops cleanly on
session exit. niri must reach `graphical-session.target` for this to fire —
see task #34 (session autostart units) if it doesn't.

## Verification

1. Confirm the service is running:

   ```sh
   systemctl --user status swayidle.service
   ```

   Expected: `active (running)`, the ExecStart line matches the shipped path.

2. Check the login session reports an idle hint as time passes:

   ```sh
   loginctl show-session "$(loginctl | awk '/seat/ {print $1; exit}')" \
     -p IdleHint -p IdleSinceHint
   ```

   `IdleHint=yes` after the first activity-free minute confirms the session
   bus is wired up to swayidle's notifications.

3. Manual smoke test:
   - Leave the laptop alone for ~4 min. Backlight should dim to 10%.
   - Move the mouse / press a key. Backlight restores to its prior level.
   - Walk away again, wait ~5 min total. gtklock should appear.
   - Wait the rest of the way to ~10 min. The system should suspend; on
     wake, gtklock is already in front (from `before-sleep`) and you must
     auth to get back in.

## Tuning

The four directives in `etc/swayidle/config` are:

| Directive                              | When        | Action                            |
| -------------------------------------- | ----------- | --------------------------------- |
| `timeout 240 ... resume ...`           | 4 min idle  | Save brightness, dim to 10%; restore on resume. |
| `timeout 300 'gtklock'`                | 5 min idle  | Lock the screen.                  |
| `timeout 600 'systemctl suspend'`      | 10 min idle | Suspend the system.               |
| `before-sleep 'gtklock'`               | pre-suspend | Lock right before any suspend.    |

To change the timeouts or swap `gtklock` for `waylock` / `hyprlock`, edit
`etc/swayidle/config`, then reload:

```sh
systemctl --user restart swayidle.service
```

A `daemon-reload` is only needed when the unit file itself
(`etc/systemd/user/swayidle.service`) changes — not when the swayidle config
changes.

Notes on syntax (see `swayidle(5)`):

- Each directive is one line: a keyword (`timeout` / `before-sleep` /
  `after-resume` / `lock` / `unlock` / `idlehint`), arguments, and one or
  more single-quoted shell command strings.
- `timeout <seconds> 'cmd'` fires `cmd` after the compositor reports that
  many seconds of inactivity. An optional `resume 'cmd'` runs when activity
  returns.
- `before-sleep 'cmd'` fires on `org.freedesktop.login1`'s `PrepareForSleep`
  signal, before the system actually suspends — use this to lock so the
  locker is already up by the time the screen comes back.

## What this does NOT do

- It does NOT implement the `org.freedesktop.ScreenSaver` D-Bus interface
  for inhibits (mpv, browsers, video calls). That's task #29 — until then,
  full-screen video will still time out.
- It does NOT handle lid-switch events. Lid-close suspend is configured via
  `logind.conf` (`HandleLidSwitch=suspend`), separate from this idle stack.
