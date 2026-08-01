# trollshell shipped configs

Directory of niri+trollshell session config files. Symlink or copy each
into your user config directory per the per-feature README.

| Subdirectory                                       | Purpose                                              | Setup                         |
| -------------------------------------------------- | ---------------------------------------------------- | ----------------------------- |
| [xdg-desktop-portal](xdg-desktop-portal/README.md) | File-picker + screen-share routing                   | symlink portals.conf          |
| [niri](niri/README.md)                             | Media-key bindings + autostart snippet               | merge into config.kdl         |
| [fuzzel](fuzzel/README.md)                         | App launcher                                         | symlink                       |
| [kanshi](kanshi/README.md)                         | Display profiles                                     | symlink + enable unit         |
| [wallpaper](wallpaper/README.md)                   | swaybg wallpaper service                             | enable unit                   |
| [cliphist](cliphist/README.md)                     | Clipboard history                                    | enable units                  |
| [calendar](calendar/README.md)                     | EDS calendar sync (no config; just a setup pointer)  | use gnome-control-center      |
| [systemd/user](systemd/user/README.md)             | The umbrella `niri-session.target` and unit topology | symlink units + enable target |
| [skills/infobroker](skills/infobroker/SKILL.md)    | Agent skill folder for the #487 data broker CLI      | point an agent at the folder  |

For the entire stack: see [systemd/user/README.md](systemd/user/README.md) for the
full install sequence. That directory also ships
`trollshell-plugin-clock-demo.service`, the reference out-of-process widget
plugin (#35) — a separate, GTK-free binary that dials the shell over a Unix
socket; see its "Out-of-process widget plugins" section.

## Local AI agent data broker (infobroker)

`skills/infobroker/` is the agent-facing half of the **infobroker** (#487) — the
consent-gated broker that lets a local AI agent read _scoped_ desktop data
(public-transport departures today; more later) without touching the desktop
owner's files or daemons. It follows the "mcp is dead" shape: **a skill folder,
not an MCP server** — a `SKILL.md` teaching the flow plus a `bin/hytte-infobroker`
wrapper the agent shells out to.

The broker itself is an ordinary out-of-process trollshell plugin
(`trollshell-plugin-infobroker.service`, see
[systemd/user/README.md](systemd/user/README.md)): a bar-chip shield whose drawer
panel manages the durable **grants** (agent × datasource → allow/deny) and shows
a live audit trail. An agent authenticates with
`hytte-infobroker auth --agent <name>` → a short-lived session token exported
into its environment (`HYTTE_INFOBROKER_TOKEN`) → scoped
`hytte-infobroker get departures`. Access is
**denied until a human grants it** (in the panel or by editing
`grants.toml`); a denied knock pops a desktop toast so the desktop's owner
sees it.

## Idle & screen locking

The idle → dim → lock → suspend pipeline is **native to trollshell** — it runs
in-process as an `ext-idle-notify-v1` client (see
`crates/hytte-services/src/idle_notify.rs`), so there is no separate idle daemon
to install. It dims the backlight at 4 min, locks at 5, suspends at 10, and
relocks just before any suspend (logind `PrepareForSleep`), each gated on logind
inhibitors (a held `idle` inhibitor — e.g. the Power drawer's "Keep awake"
toggle — skips dim/lock). This replaced swayidle (#204).

trollshell does **not** ship a lock screen — locking is delegated to an
established, security-audited tool. This config uses `swaylock`; the native lock
timer, the trollshell bar's Power → Lock button, and any `loginctl lock-session`
all trigger it via logind's `Lock` signal. Wire swaylock's own PAM stack
(`/etc/pam.d/swaylock`; NixOS: `security.pam.services.swaylock = {};`) — without
it swaylock can never verify a password.
