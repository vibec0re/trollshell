# xdg-desktop-portal config for niri

Routes portal interfaces (file pickers, screen sharing, screenshots, settings)
to working backends under niri. Without this, file dialogs in Electron / Firefox
/ flatpaks fail and screen sharing breaks in OBS / Discord.

## Required packages

- `xdg-desktop-portal` — the portal frontend.
- `xdg-desktop-portal-gnome` — preferred FileChooser + Settings backend.
  - `xdg-desktop-portal-gtk` is an acceptable fallback (covered by the
    `default=gnome;gtk` chain).
- `xdg-desktop-portal-wlr` — Screenshot + ScreenCast backend for wlroots-style
  compositors (niri qualifies).

Install on Arch:

```sh
sudo pacman -S xdg-desktop-portal xdg-desktop-portal-gnome xdg-desktop-portal-wlr
```

## Install the config

niri reads `~/.config/xdg-desktop-portal/niri-portals.conf` to decide which
backend handles each portal interface. Symlink the shipped file:

```sh
mkdir -p ~/.config/xdg-desktop-portal
ln -s "$PWD/etc/xdg-desktop-portal/niri-portals.conf" \
      ~/.config/xdg-desktop-portal/niri-portals.conf
```

(Run that from the repo root, or substitute the absolute repo path.)

## Environment

`xdg-desktop-portal` picks the per-desktop config file by matching
`XDG_CURRENT_DESKTOP`. niri must advertise itself as `niri:GNOME` so both
`niri-portals.conf` and the GNOME backend activate cleanly.

Add to your niri config (`~/.config/niri/config.kdl`):

```kdl
spawn-at-startup "dbus-update-activation-environment" "--systemd" "XDG_CURRENT_DESKTOP=niri:GNOME"
```

Alternatively, export it from your session and run
`systemctl --user import-environment XDG_CURRENT_DESKTOP` early in startup.

Verify after login:

```sh
systemctl --user show-environment | grep XDG_CURRENT_DESKTOP
# expected: XDG_CURRENT_DESKTOP=niri:GNOME
```

## Verification

1. Confirm the portal frontend is reachable on the session bus:

   ```sh
   gdbus introspect --session \
     --dest org.freedesktop.portal.Desktop \
     --object-path /org/freedesktop/portal/desktop
   ```

   You should see `org.freedesktop.portal.FileChooser`,
   `org.freedesktop.portal.Screenshot`, `org.freedesktop.portal.ScreenCast`,
   and `org.freedesktop.portal.Settings` in the introspection output.

2. Check which backends are running:

   ```sh
   systemctl --user status xdg-desktop-portal xdg-desktop-portal-gnome xdg-desktop-portal-wlr
   ```

3. Manual smoke tests:
   - Firefox: File -> Save Page As. A GTK/GNOME file dialog should appear
     (not Firefox's built-in fallback).
   - A flatpak app (e.g. `flatpak run org.mozilla.firefox`): same test.
   - OBS or Discord: start a screen share. The wlr backend should prompt for
     the output / window to capture.

## Config reference

`niri-portals.conf` follows the freedesktop INI format documented at
<https://flatpak.github.io/xdg-desktop-portal/docs/portals.conf.html>.
Each `org.freedesktop.impl.portal.*` key names a backend (matching an installed
`*.portal` file's `PortalName`); `default` is the fallback chain tried in order
when a specific interface isn't pinned.
