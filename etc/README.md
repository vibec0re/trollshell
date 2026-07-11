# trollshell shipped configs

Directory of niri+trollshell session config files. Symlink or copy each
into your user config directory per the per-feature README.

| Subdirectory                                       | Purpose                                              | Setup                         |
| -------------------------------------------------- | ---------------------------------------------------- | ----------------------------- |
| [xdg-desktop-portal](xdg-desktop-portal/README.md) | File-picker + screen-share routing                   | symlink portals.conf          |
| [swayidle](swayidle/README.md)                     | Idle dim/lock/suspend pipeline                       | symlink + enable unit         |
| [niri](niri/README.md)                             | Media-key bindings + autostart snippet               | merge into config.kdl         |
| [fuzzel](fuzzel/README.md)                         | App launcher                                         | symlink                       |
| [kanshi](kanshi/README.md)                         | Display profiles                                     | symlink + enable unit         |
| [wallpaper](wallpaper/README.md)                   | swaybg wallpaper service                             | enable unit                   |
| [cliphist](cliphist/README.md)                     | Clipboard history                                    | enable units                  |
| [calendar](calendar/README.md)                     | EDS calendar sync (no config; just a setup pointer)  | use gnome-control-center      |
| [systemd/user](systemd/user/README.md)             | The umbrella `niri-session.target` and unit topology | symlink units + enable target |

For the entire stack: see [systemd/user/README.md](systemd/user/README.md) for the
full install sequence. That directory also ships
`trollshell-plugin-clock-demo.service`, the reference out-of-process widget
plugin (#35) — a separate, GTK-free binary that dials the shell over a Unix
socket; see its "Out-of-process widget plugins" section.

## Screen locking

trollshell does **not** ship a lock screen — locking is delegated to an
established, security-audited tool. This config uses `swaylock` (driven by
`swayidle`); the trollshell bar's Power → Lock button and any
`loginctl lock-session` both trigger it via logind's `Lock` signal. Wire
swaylock's own PAM stack (`/etc/pam.d/swaylock`; NixOS:
`security.pam.services.swaylock = {};`) — without it swaylock can never
verify a password.
