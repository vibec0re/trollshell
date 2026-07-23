# trollshell + hytte

A library-first Rust toolkit (`hytte`) for composing GTK4 + libadwaita +
layer-shell desktop shells, and `trollshell` — the personal shell built on it,
targeting the [Niri](https://github.com/YaLTeR/niri) compositor.

"Composable, not configurable": there is no config DSL. The shell is wired up
in plain Rust in `trollshell/src/main.rs`. `hytte` services are thin async
clients to existing system daemons (systemd-networkd, BlueZ, PipeWire, UPower,
logind, niri-ipc, …), so persistent state lives in those daemons and a
`cargo run` restart of the shell reconnects without losing system state.

## What it does

A top-edge bar on every Niri monitor — workspaces, window list, media
controls, system tray, network/Wi-Fi/VPN, bluetooth, volume/mic/brightness,
battery, CPU/memory/GPU/disk stats, clock, and notification/settings/power
chips. Clicking a chip opens a slide-out **drawer** with a matching panel.
Plus a left **sidebar**, on-screen displays (OSD), a notification daemon +
toasts, password prompts, and out-of-process **widget plugins** (a clock, a
kaomoji pet, a timer, a terminal, transit departures, weather, …) that mount
into the bar and sidebar over a local socket. (Polkit authentication is
delegated to a standalone agent — see the flake / `etc/`.)

trollshell ships **no in-shell lock screen**: an idle → dim → lock → suspend
timeline runs natively in-process, but the actual locking is delegated to
`swaylock` via logind's `Lock` signal (see `etc/README.md`).

A separate windowed companion app, **`trollshell-control-center`**, handles
heavier settings/management (picking the weather place, toggling plugin
units, …) over a D-Bus link to the running shell — gnome-control-center-style,
never linked into the shell itself.

See `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md` for
the founding design and the dated specs/plans alongside it for each feature.

## Build & run

Development uses the Nix flake's devShell, which provides the Rust toolchain
and the GTK/PipeWire/EDS native deps **and** sets the env the build and
runtime need (libclang for bindgen, icon-theme + GSettings-schema paths).
`.envrc` is `use flake`, so [direnv](https://direnv.net/) enters it
automatically; otherwise run `nix develop` first.

```sh
cargo build --release -p trollshell    # build the binary (inside the devShell)
cargo run -p trollshell                # run it — requires a live Niri session
nix build                              # build the packaged binary (.#trollshell)
```

`trollshell` is a real Wayland shell: it connects to `$NIRI_SOCKET` and to
live system daemons, so it only does anything meaningful **inside a Niri
session**.

## Install

The flake exposes the package (`packages.default`) plus NixOS and home-manager
modules. To try the binary without installing it (still needs a live Niri
session):

```sh
nix run github:vibec0re/trollshell
```

**NixOS** — add the flake as an input and import its module:

```nix
{
  inputs.trollshell.url = "github:vibec0re/trollshell";

  # …then in your nixosConfigurations' modules list:
  imports = [ inputs.trollshell.nixosModules.default ];
  programs.trollshell.enable = true;
}
```

This installs the package — and, by default, the `trollshell-control-center`
companion app alongside it (`programs.trollshell.controlCenter.enable`,
default `true`) — plus a bundle of recommended-but-optional daemons — the
agent-name D-Bus policy, the polkit agent, UPower, power-profiles-daemon,
geoclue, and the GNOME Online Accounts → evolution-data-server stack (with
`gnome-control-center` to add accounts) that the calendar + tasks panels read
from — gated behind `programs.trollshell.enableRecommendedServices` (default
`true`). Set it to `false` for a bare bar, where each chip simply hides when
its daemon is absent.

Calendars/tasks have no in-shell account UI: add an account in **Settings →
Online Accounts** (`gnome-control-center online-accounts`) and trollshell reads
it back from evolution-data-server — see
[etc/calendar/README.md](etc/calendar/README.md).

**Home-manager** — import the module to run the shell as a user service:

```nix
{
  inputs.trollshell.url = "github:vibec0re/trollshell";

  imports = [ inputs.trollshell.homeModules.default ];
  programs.trollshell.enable = true;
}
```

When home-manager runs as a NixOS module, the NixOS module wires the
home-manager one in automatically, so per-user `programs.trollshell` settings
just work. Non-NixOS session integration (systemd user units, niri binds,
kanshi, cliphist, …) ships under `etc/` —
see [etc/README.md](etc/README.md). (The idle → dim → lock → suspend pipeline
is native to trollshell — #204 retired swayidle — so it needs no `etc/`
config; `swaylock`'s own PAM stack, wired separately, is what actually
authenticates an unlock — see `etc/README.md`'s "Idle & screen locking".)

### Wallpaper

The Appearance drawer page writes the picked image to
`~/.config/trollshell/wallpaper.path`, then tells a wallpaper daemon to reload.
`wallpaper.backend` picks the daemon and the reload command from a single enum
value (so two daemons can never run at once):

```nix
programs.trollshell.wallpaper.backend = "swaybg";  # the default — today's behavior
# or "awww" / "none"
```

- **`swaybg`** (default): the bundled `swaybg` unit; the picker restarts it. No
  reload command needed.
- **`awww`**: the swww successor (upstream renamed swww → awww at 0.12; swww
  itself is deprecated). Sets `wallpaper.reloadCommand` to `awww img {}`. The
  daemon is run by home-manager's `services.awww`; the NixOS module only exports
  the reload command (a NixOS-only user without home-manager runs the awww
  daemon themselves). Needs a home-manager channel with `services.awww` (0.12+)
  — otherwise the build fails with a clear assertion.
- **`none`**: trollshell manages no daemon; the picker only writes the path file
  (and runs `reloadCommand` if you set one). Use this to wire your own daemon.

You can still set `wallpaper.reloadCommand` by hand to override the per-backend
default; a `{}` in it is replaced with the chosen path (shell-quoted, **not** a
`$VAR` — sessionVariables would expand it away at login):

```nix
programs.trollshell.wallpaper.reloadCommand = "awww img {}";
```

The legacy home-manager `programs.trollshell.swaybg.enable` still works as a
standalone swaybg toggle (gently deprecated in favor of `backend = "swaybg"`).

## Repo layout

- `crates/hytte-reactive/` — `Service` trait, thread-local handle registry,
  process-wide tokio runtime, and the `bind*` GTK↔`futures-signals` helpers.
- `crates/hytte-ui/` — `App`, `Bar`, `LayerWindow`, `Popup`, `Monitor`
  primitives (layer-shell) + the default stylesheet.
- `crates/hytte-bus/` — shared D-Bus capability layer (pooled session/system
  connections; `call`/`property`/`proxy`/`signals`/`own_name` builders).
- `crates/hytte-services/` — async clients to system daemons exposed as
  services (clock, niri, pipewire, networkd/resolved/wifi, bluetooth, upower,
  mpris, tray, notifications, calendar, sensors, idle_notify, …).
- `crates/hytte-ecal/` — hand-written FFI to evolution-data-server (calendar);
  the only crate allowed `unsafe`.
- `crates/hytte-ai-providers/` — shared OpenAI-compatible chat client +
  key-file loader for the plugins that talk to an LLM (e.g. the pet plugin).
- `crates/hytte/` — umbrella re-export crate (`bus`, `reactive`, `services`,
  `ui`) + a `prelude`.
- `crates/hytte-plugin-proto/` + `crates/hytte-plugin/` — the plugin side
  (out-of-process, never linked into the shell): a GTK-free wire protocol and
  the Rust plugin runtime SDK a plugin binary depends on.
- `crates/hytte-plugin-*/` (nine binaries: `clock-demo`, `bar-clock-demo`,
  `pet`, `preem-demo`, `timer`, `terminal`, `caw`, `departures`, `weather`) —
  the plugin binaries themselves, each its own systemd user unit.
- `crates/trollshell-control-center/` — the external settings/management
  companion app (see "What it does" above); a separate GTK4/libadwaita
  binary, not linked into the shell.
- `trollshell/` — the binary: `widgets/` (bar chips), `panels/` (drawer
  pages), `overlays/` (frame, OSD, notifications, dialogs, sidebar),
  `modal.rs` (the drawer), `plugins.rs` (the plugin host transport),
  `control.rs` (the control-center's D-Bus endpoint), `commands.rs` (the
  keybind-facing command surface), `components/` (shared building blocks).

`CLAUDE.md` documents the architecture (the handle/work reactive split, the
per-service pattern, the bus layer, the strict lint gate) in more depth.

## Logs

```sh
RUST_LOG=hytte_services=debug,trollshell=debug cargo run -p trollshell
```
