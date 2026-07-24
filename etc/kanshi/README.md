# kanshi config for niri+trollshell

[`kanshi`](https://gitlab.freedesktop.org/emersion/kanshi) auto-applies an
output configuration whenever the set of connected displays changes. Plug in
a dock with an external monitor and the right layout (mode, position, scale,
rotation) loads automatically; unplug it and you fall back to the laptop
profile.

It speaks the `wlr-output-management` protocol, which niri implements, so it
works under our compositor without extra glue.

trollshell shows a read-only view of niri's current output state in the
**Displays** drawer page; persistent layout decisions live in this config
file, not in trollshell.

## Required packages

- `kanshi` — the daemon. Arch: `sudo pacman -S kanshi`.

## Install the config

kanshi reads `~/.config/kanshi/config` by default. Symlink the shipped
example:

```sh
mkdir -p ~/.config/kanshi
ln -s "$PWD/etc/kanshi/config" ~/.config/kanshi/config
```

(Run from the repo root, or substitute the absolute repo path. The symlink
target is absolute — recreate it if you move the repo.)

## Adapt the profile to your hardware

The shipped `config` is illustrative — connector names, modes, positions and
scales **will not match your machine**. Run

```sh
niri msg outputs
```

while logged in to see the connectors niri knows about and the modes each
one advertises. Then edit `etc/kanshi/config` to replace:

- **Connector names** (`eDP-1`, `HDMI-A-1`) with what `niri msg outputs`
  prints. They typically follow `<connector-type>-<index>`: `eDP-N`,
  `HDMI-A-N`, `DP-N`, `USB-C-N`, `HDMI-B-N`, etc.
- **Mode** (`mode WxH@R`) with one of the modes the monitor advertises.
  Refresh rates are in Hz; you can include the decimal (`60.000`) but
  whole-number rates work fine.
- **Position** (`position X,Y`) with the top-left corner of that output in
  the global logical coordinate space. The X offset of the right-of-laptop
  HDMI in the example is `1440` because the laptop's logical width is
  `2880 / 2 = 1440` (mode width divided by scale).
- **Scale** (`scale N`) with whatever Wayland HiDPI scale you want for that
  output. Fractional scales (e.g. `1.5`) are supported by niri.

For each docking scenario you'd like a different layout for, add another
`profile <name> { … }` block. kanshi picks the first profile whose `output`
lines all match the currently connected set, so order more-specific
profiles before less-specific ones.

Optional `output … transform <T>` (rotation: `normal`, `90`, `180`, `270`),
and per-profile `exec '<shell>'` hook lines are documented in `kanshi(5)`.

## Verify the config syntactically

Before enabling the unit, dry-run the config to catch typos:

```sh
kanshi -c ~/.config/kanshi/config -v
```

`-v` runs kanshi in verbose mode — it parses the config, prints the
profiles it found, and then keeps running in the foreground waiting for
output changes. `Ctrl+C` once the parse log lines confirm the config is
valid; any syntax errors show up immediately and exit non-zero.

## Enable the systemd unit

```sh
mkdir -p ~/.config/systemd/user
ln -s "$PWD/etc/systemd/user/kanshi.service" \
      ~/.config/systemd/user/kanshi.service
systemctl --user daemon-reload
systemctl --user enable --now kanshi.service
```

The unit is `PartOf=niri-session.target` and `WantedBy=niri-session.target`,
matching swaybg, so it starts when niri brings up its session
target and stops cleanly on session exit.

Confirm:

```sh
systemctl --user status kanshi.service
```

Expected: `active (running)`. The journal (`journalctl --user -u kanshi`)
prints which profile kanshi selected on each output-set change.

## Manual smoke test

1. `systemctl --user status kanshi.service` reports `active (running)`.
2. With only the laptop display connected, `kanshi` should pick the
   `laptop` profile (or whichever single-output profile you named).
3. Plug in an external monitor that matches the second profile's connector
   name. Watch `journalctl --user -u kanshi -f`; kanshi should log
   "applying profile docked" (or your profile name) within a second.
4. The trollshell **Displays** drawer page should show the new monitor in
   the list within ~2 s (the polling cadence — see
   `crates/hytte-services/src/displays.rs`).

## kanshi vs niri's own output config

niri's `~/.config/niri/config` also has a `[output]` section that can pin
modes, scales, and positions. **Don't try to drive the same connector
from both** — pick one. The recommended split:

- **kanshi** owns connector-set-aware _profiles_ (laptop alone, docked,
  multi-monitor desk, …).
- **niri** owns nothing output-related when kanshi is enabled. Comment out
  `[output …]` blocks in `~/.config/niri/config` to be safe; kanshi's
  decisions will just override them otherwise, but the duplication is
  confusing.

If trollshell's Displays page shows a state you didn't expect, check
**both** configs — the last writer wins, and `niri-msg outputs` reports
the final state regardless of who set it.

## kanshi vs the gnome-control-center Displays shim (#393)

trollshell serves the `org.gnome.Mutter.DisplayConfig` D-Bus API over niri-ipc
(`crates/hytte-services/src/display_config.rs`), so **gnome-control-center's own
Displays panel** drives your niri outputs live — resolution, scale, rotation,
arrangement, on/off. It is a _live_ control surface, not a persistence store:

- g-c-c's **Apply → "Keep Changes"** flow works end-to-end. The apply takes
  effect immediately on niri.
- On "Keep Changes" the shell posts a toast — _"Display configuration applied …
  save it as a kanshi profile"_ — because the shim **does not write any config
  file**. niri-ipc applies are inherently temporary (niri forgets them on its
  next output-config reload), and this file is hand-owned.

So the split is: **g-c-c** for quick live tweaks, **this file** for what
survives a session restart. When a live g-c-c arrangement is one you want to
keep, transcribe it into a `profile` block here (use `niri msg outputs` to read
back the exact mode/scale/position the shim applied). The shim deliberately
won't edit this file for you — it can't know which connector-set profile you
mean, and clobbering your hand-tuned blocks (positions, `exec` hooks, ordering)
would be worse than an honest nudge.

## What this does NOT do

- No GUI editor for kanshi profiles. Edit the config file by hand for now;
  task #41 (Settings page) may add one later.
- No per-output wallpaper. The shipped wallpaper service paints a single
  image across every output via `swaybg`; per-output backgrounds are a
  follow-up.
- No rotation UI in the trollshell drawer. The drawer's switch only
  toggles the output on/off (via `niri msg output <name> on|off`).
  Rotation is set in the kanshi profile (`transform 90` etc.).
- No mode/scale picker in the drawer. Same reason: persistent layout
  belongs in `kanshi/config`, not in transient drawer state.
