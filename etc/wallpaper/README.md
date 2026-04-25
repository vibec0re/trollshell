# Wallpaper for niri+trollshell

Single-image desktop background, rendered by `swaybg` under a systemd
user unit. The chosen path lives in `~/.config/trollshell/wallpaper.path`
(one line, plain text). trollshell's "Appearance" drawer page rewrites
that file when you Browse for a new image and restarts the swaybg unit
so the change is applied immediately.

## Required packages

- `swaybg` — the wallpaper renderer. Wayland-native; works on niri.

Install on Arch:

```sh
sudo pacman -S swaybg
```

## Set the initial wallpaper

Two options — pick whichever is easier:

- **From the trollshell drawer**, after the rest of the session is up:
  open the bar's drawer → Appearance page → Browse… and pick an image.
  trollshell writes the path file and restarts swaybg for you.
- **By hand**, before the first launch (or when you don't want the GUI):
  put the absolute path to your wallpaper in
  `~/.config/trollshell/wallpaper.path` as a single line. Example:

  ```sh
  mkdir -p ~/.config/trollshell
  echo "$HOME/Pictures/wallpaper.jpg" > ~/.config/trollshell/wallpaper.path
  ```

If the file is missing or empty when swaybg starts, swaybg fails (which
is fine — the unit logs to journal and you can re-pick).

## Install the systemd unit

The unit lives at `etc/systemd/user/swaybg.service` in this repo. Symlink
or copy it into `~/.config/systemd/user/`, then enable + start:

```sh
mkdir -p ~/.config/systemd/user
ln -sf "$PWD/etc/systemd/user/swaybg.service" \
       ~/.config/systemd/user/swaybg.service
systemctl --user daemon-reload
systemctl --user enable --now swaybg.service
```

The unit is bound to `graphical-session.target` — it'll start once your
Wayland session is up (niri publishes the target after the compositor
is ready) and stop when the session ends.

## Verify

```sh
systemctl --user status swaybg.service
pgrep -a swaybg
```

If the service is `active (running)` and `pgrep` finds a `swaybg -i ...`
process, the wallpaper should be visible. If `swaybg` flapped on a bad
path, `journalctl --user -u swaybg.service -n 50` will show the error.

## How the unit reads the path

`ExecStart` is a `sh -c` wrapper that does `cat ~/.config/trollshell/wallpaper.path`
at swaybg startup and substitutes the result as the `-i` argument. There's
no copy or symlink — swaybg sees whatever path you put in the file.

This means:

- Editing the path file by hand and running
  `systemctl --user restart swaybg.service` is enough to switch images.
- trollshell's "Browse…" button does exactly that on your behalf.

## What this does NOT do

- No per-output (per-monitor) wallpaper. swaybg can do it via the
  `--output <name>` flag, but the v1 unit + service binds a single image
  to all outputs. Multi-output is a follow-up task.
- No time-of-day / dynamic wallpaper rotation. If you want that, set a
  cron / systemd timer that overwrites `wallpaper.path` and runs
  `systemctl --user restart swaybg.service`.
- No video / animated wallpapers. swaybg renders still images only.
- No "Clear wallpaper" button — leave the file in place and re-pick via
  Browse. (Could be added later.)
