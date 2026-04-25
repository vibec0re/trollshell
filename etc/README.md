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
