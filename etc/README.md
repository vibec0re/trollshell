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
full install sequence. That README's "Out-of-process widget plugins" section
also covers the widget-plugin architecture (#35) — separate, GTK-free binaries
that dial the shell over a Unix socket and that trollshell itself launches as
transient units per `programs.trollshell.plugins` (#872); this directory ships
no per-plugin unit files.

## Local AI agent data broker (infobroker)

`skills/infobroker/` is the agent-facing half of the **infobroker** (#487) — the
consent-gated broker that lets a local AI agent read _scoped_ desktop data
(public-transport departures today; more later) without touching the desktop
owner's files or daemons. It follows the "mcp is dead" shape: **a skill folder,
not an MCP server** — a `SKILL.md` teaching the flow plus a `bin/hytte-infobroker`
wrapper the agent shells out to.

The broker itself is an ordinary out-of-process trollshell plugin
(`hytte-plugin-infobroker`, launched as a transient `trollshell-plugin-infobroker.service`
by the declarative launcher — see
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
established, security-audited tool. This config uses `swaylock`. Wire swaylock's
own PAM stack (`/etc/pam.d/swaylock`; NixOS: `security.pam.services.swaylock =
{};`) — without it swaylock can never verify a password.

**trollshell emits logind's `Lock` signal; it does not draw or spawn a locker.**
The native idle timer (300 s), the bar's Power → Lock button, the D-Bus `Lock()`
method and the relock before suspend all end in `loginctl lock-session`, which
only makes logind emit `Lock` on the session. `swaylock` does not subscribe to
logind, so **something in the session must translate `Lock` into the locker** —
without that bridge the idle timer "fires" and nothing appears on screen. Use
either:

- `swayidle` with **only** the `lock` and `before-sleep` handlers — no
  timeouts. The idle timeouts are native now; giving swayidle any would
  double-fire them. `before-sleep` is worth having for a second reason: it also
  takes a logind `sleep` _delay_ inhibitor, which is what makes suspend wait for
  the locker to be up. (trollshell's own `PrepareForSleep` relock takes none.)
- an equivalent bridge such as `systemd-lock-handler`.

home-manager, for example:

```nix
services.swayidle = {
  enable = true;
  events = [
    { event = "lock"; command = "${pkgs.swaylock}/bin/swaylock -f"; }
    { event = "before-sleep"; command = "${pkgs.swaylock}/bin/swaylock -f"; }
  ];
  timeouts = [ ]; # idle → dim → lock → suspend is trollshell's, not swayidle's
};
```

Nothing here sets logind's `LockedHint` either: swaylock never talks to logind
and swayidle only ever calls `SetIdleHint` (and only with an `idlehint` event),
so `LockedHint` stays `false` unless your locker or bridge sets it.
