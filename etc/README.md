# trollshell shipped configs

Directory of niri+trollshell session config files. Symlink or copy each
into your user config directory per the per-feature README.

| Subdirectory | Purpose | Setup |
|---|---|---|
| [xdg-desktop-portal](xdg-desktop-portal/README.md) | File-picker + screen-share routing | symlink portals.conf |
| [swayidle](swayidle/README.md) | Idle dim/lock/suspend pipeline | symlink + enable unit |
| [niri](niri/README.md) | Media-key bindings + autostart snippet | merge into config.kdl |
| [fuzzel](fuzzel/README.md) | App launcher | symlink |
| [kanshi](kanshi/README.md) | Display profiles | symlink + enable unit |
| [wallpaper](wallpaper/README.md) | swaybg wallpaper service | enable unit |
| [cliphist](cliphist/README.md) | Clipboard history | enable units |
| [calendar](calendar/README.md) | EDS calendar sync (no config; just a setup pointer) | use gnome-control-center |
| [systemd/user](systemd/user/README.md) | The umbrella `niri-session.target` and unit topology | symlink units + enable target |

For the entire stack: see [systemd/user/README.md](systemd/user/README.md) for the
full install sequence.

## PAM lock screen

Install the screen-unlock PAM service file:

```sh
sudo install -m 644 etc/pam.d/trollshell /etc/pam.d/trollshell
```

Without this file the lock UI mounts but authentication fails with
"Authentication unavailable" — there's no PAM service named
`trollshell` for libpam to consult.

NixOS users using the flake's `nixosModules.default` get this
automatically (`security.pam.services.trollshell` is declared when
`programs.trollshell.enable = true;`). Skip the `install` step and
rebuild.

Build-time deps: `libpam` headers (Arch `pam` package, Nix
`pkgs.pam`). Runtime deps: standard `pam_unix` stack (default on
every distro that has working login).
