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
toasts, password prompts, and an `ext-session-lock-v1` lock screen. (Polkit
authentication is delegated to a standalone agent — see the flake / `etc/`.) See `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md` for
the founding design and the dated specs/plans alongside it for each feature.

## Build & run

Development uses the Nix flake's devShell, which provides the Rust toolchain
and the GTK/PipeWire/PAM/EDS native deps **and** sets the env the build and
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

This installs the package and the lock-screen PAM service, plus a bundle of
recommended-but-optional daemons — the agent-name D-Bus policy, the polkit
agent, UPower, power-profiles-daemon, and geoclue — gated behind
`programs.trollshell.enableRecommendedServices` (default `true`). Set it to
`false` for a bare bar, where each chip simply hides when its daemon is absent.

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
swayidle, kanshi, the PAM file, …) ships under `etc/` —
see [etc/README.md](etc/README.md).

The Appearance drawer applies a picked wallpaper by restarting the bundled
`swaybg` unit. To drive a different daemon instead — `swww`/`awww`, `hyprpaper`,
… — set `programs.trollshell.wallpaper.reloadCommand`; a `{}` in it is replaced
with the chosen path (shell-quoted):

```nix
programs.trollshell.wallpaper.reloadCommand = "awww img {}";
```

## Repo layout

- `crates/hytte-reactive/` — `Service` trait, thread-local handle registry,
  process-wide tokio runtime, and the `bind*` GTK↔`futures-signals` helpers.
- `crates/hytte-ui/` — `App`, `Bar`, `LayerWindow`, `Popup`, `Monitor`
  primitives (layer-shell + `ext-session-lock-v1`) + the default stylesheet.
- `crates/hytte-bus/` — shared D-Bus capability layer (pooled session/system
  connections; `call`/`property`/`proxy`/`signals`/`own_name` builders).
- `crates/hytte-services/` — async clients to system daemons exposed as
  services (clock, niri, pipewire, networkd/resolved/wifi, bluetooth, upower,
  mpris, tray, notifications, calendar, sensors, …).
- `crates/hytte-pam/` — synchronous PAM authentication for the lock screen.
- `crates/hytte-ecal/` — hand-written FFI to evolution-data-server (calendar).
- `crates/hytte/` — umbrella re-export crate (`bus`, `reactive`, `services`,
  `ui`) + a `prelude`.
- `trollshell/` — the binary: `widgets/` (bar chips), `panels/` (drawer
  pages), `overlays/` (lock screen, OSD, notifications, dialogs, sidebar),
  `modal.rs` (the drawer), `components/` (shared building blocks).

`CLAUDE.md` documents the architecture (the handle/work reactive split, the
per-service pattern, the bus layer, the strict lint gate) in more depth.

## Logs

```sh
RUST_LOG=hytte_services=debug,trollshell=debug cargo run -p trollshell
```
