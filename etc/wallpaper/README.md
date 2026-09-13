# Wallpaper for niri+trollshell

Desktop background rendered by `swaybg` under a systemd user unit. trollshell's
"Appearance" drawer page owns the selection: a default image for all displays,
optional per-monitor overrides, and an optional morning/day/evening/night
rotation. The picker writes a small state file under `$XDG_STATE_HOME`,
re-derives swaybg's arguments, and restarts (or, on Clear, stops) the swaybg
unit so the change applies immediately.

## Required packages

- `swaybg` — the wallpaper renderer. Wayland-native; works on niri.

On NixOS the recommended-services bundle installs and manages this for you
(`programs.trollshell.enableRecommendedServices`, backend `swaybg`). To install
by hand elsewhere, use your distro's `swaybg` package.

## The files

The service owns three, split across two directories since #1226 — the
selection you make is **state** (the shell writes it when you click), the two
render files are read by an external systemd unit and stay where that unit
looks. You normally never touch any of them; the Appearance page does.

### `$XDG_STATE_HOME/trollshell/wallpaper.toml` — the source of truth

`$XDG_STATE_HOME` is `~/.local/state` unless you set it, so this is normally
`~/.local/state/trollshell/wallpaper.toml`.

```toml
default = "/home/you/Pictures/wall.png"

[outputs]
DP-1 = "/home/you/Pictures/left.png"

[rotation]
enabled = true
morning = "/home/you/Pictures/dawn.png"
night = "/home/you/Pictures/night.png"
```

- `default` — image applied to every output without a specific override.
- `outputs` — per-connector overrides, keyed by output name (`DP-1`, `eDP-1`, …).
- `rotation` — when `enabled`, the current slot's image drives **all** outputs
  (a whole-screen mode that wins over the static per-output selection). Slots
  are fixed local-clock ranges: morning `06:00–11:00`, day `11:00–17:00`,
  evening `17:00–21:00`, night otherwise. An unset active slot falls back to
  `default`; if that too is unset (rotation resolves to no image at all), the
  render falls back to the static per-output/default selection rather than
  blanking — so a per-output-only setup keeps its wallpapers when it enters an
  empty slot.

**Migrated once from `~/.config/trollshell/wallpaper.json`.** If the state
file does not exist yet and that legacy JSON document does, it is read once,
converted, and written here; the JSON file is left exactly as it was, never
deleted or rewritten. If that is _also_ absent, the even older single-line
`wallpaper.path` is used instead (see below). After the first launch the state
file is authoritative **forever** — editing `wallpaper.json` afterwards does
nothing at all, even with the shell running.

### `~/.config/trollshell/` — the two render files

These stay in the config directory on purpose: `etc/systemd/user/swaybg.service`
and its home-manager/NixOS-rendered mirrors hardcode
`%h/.config/trollshell/…`, and moving them would break the unit for anyone who
has not (and, the unit being nix-rendered, structurally cannot on their own)
redeployed a state-aware version.

- **`swaybg.args`** — derived render spec: swaybg's argument list, one argument
  per line. The swaybg unit's `ExecStart` reads this. Its presence gates the
  unit (Clear removes it, which stops swaybg). You should not edit it by hand;
  the service rewrites it on every change.
- **`wallpaper.path`** — legacy single-line path (a representative "primary"
  image), still written for backward compatibility and still the hand-off
  point for a custom `reloadCommand`. A pre-existing `wallpaper.path` from an
  older trollshell is also **migrated** on first launch, if neither the state
  file nor `wallpaper.json` exists: its single path becomes the new `default`.

## Set the initial wallpaper

- **From the trollshell drawer**, after the session is up: open the bar's
  drawer → Appearance page → pick an image under "All displays" (and optionally
  per-display overrides / a time-of-day rotation). trollshell writes the state
  and (re)starts swaybg for you.
- **By hand**, before the first launch: write a
  `$XDG_STATE_HOME/trollshell/wallpaper.toml` as above, then start the shell
  and let it render. (The two legacy routes — a single path in
  `~/.config/trollshell/wallpaper.path`, or a `wallpaper.json` — still work,
  but only while no state file exists yet: they are read **once**, on the
  first launch that finds no `wallpaper.toml`, and never again.)

If nothing is set, `swaybg.args` doesn't exist and the swaybg unit stays
inactive (the compositor shows its own default) — that's the "no wallpaper"
state the Clear button produces.

## Install the systemd unit

The unit lives at `etc/systemd/user/swaybg.service` in this repo. Symlink or
copy it into `~/.config/systemd/user/`, then enable + start:

```sh
mkdir -p ~/.config/systemd/user
ln -sf "$PWD/etc/systemd/user/swaybg.service" \
       ~/.config/systemd/user/swaybg.service
systemctl --user daemon-reload
systemctl --user enable --now swaybg.service
```

The unit is bound to `graphical-session.target` — it'll start once your Wayland
session is up (niri publishes the target after the compositor is ready) and stop
when the session ends. It has a `ConditionPathExists=%h/.config/trollshell/swaybg.args`
guard, so it stays inactive until the Appearance page has rendered a wallpaper.

## Verify

```sh
systemctl --user status swaybg.service
pgrep -a swaybg
```

If the service is `active (running)` and `pgrep` finds a `swaybg -i ...`
process, the wallpaper should be visible. If `swaybg` flapped on a bad path,
`journalctl --user -u swaybg.service -n 50` will show the error.

## How the unit reads the arguments

`ExecStart` is a `sh -c` wrapper that reads `swaybg.args` one line at a time into
the shell's positional parameters (so paths containing spaces survive) and then
`exec`s swaybg with the full list. There's no copy or symlink of the images —
swaybg sees whatever paths the state file names.

This means:

- Editing `wallpaper.toml` by hand and running
  `systemctl --user restart swaybg.service` **won't** switch images by itself:
  the service derives `swaybg.args` from `wallpaper.toml`, so the shell has to
  be running to re-render. Restart `trollshell` too, or just use the
  Appearance page. For a purely file-driven setup, write `swaybg.args`
  directly and restart the unit.
- Editing the legacy `~/.config/trollshell/wallpaper.json` does **nothing**
  once a `wallpaper.toml` exists — not even with the shell running and
  restarted. It is a migration source that is consulted exactly once (#1226),
  not a config file.
- trollshell's Appearance page does the derive-and-restart for you.

## Using another wallpaper daemon

Set `programs.trollshell.wallpaper.backend = "awww"` (or `"none"` + your own
daemon), or set `programs.trollshell.wallpaper.reloadCommand` /
`TROLLSHELL_WALLPAPER_RELOAD_CMD` directly. The reload command runs via `sh -c`
after each pick, with `{}` replaced by the primary image (shell-quoted) and
`TROLLSHELL_WALLPAPER_PATH` exported — e.g. `awww img {}`. A custom reload
command is **single-image only**: per-output overrides can't be expressed
through the placeholder, so it receives the primary image (the rotation's active
slot or the default).

Because a reload command only ever says "here's the new image" — the `{}`
placeholder has no way to express _no wallpaper_ — the Appearance page's **Clear
wallpaper** button is **disabled** when a custom backend is configured (it would
otherwise be a silent no-op, leaving the daemon painting the last image). To
remove the wallpaper on a custom backend, clear it through your daemon directly
(e.g. `awww clear`, or stop the daemon).

## What this does NOT do

- No video / animated wallpapers. swaybg renders still images only.
- No per-output scaling/mode selection — the fill mode is `fill` for every
  output. (Per-output _image_ selection is supported; per-output _fit mode_ is
  not.)
