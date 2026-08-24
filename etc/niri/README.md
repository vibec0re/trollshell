# niri media-key bindings for trollshell

Wires the laptop's `XF86Audio*` and `XF86MonBrightness*` keys to volume,
brightness, and media-transport actions, with the OSD picking up the
resulting state changes automatically (see task #30 for the OSD widget,
task #8 for the event-driven pipewire service, and the brightness service's
sysfs `inotify` watch).

niri delegates keybinds to external commands via its `spawn` action, so
these bindings shell out to `wpctl` (wireplumber), `brightnessctl`, and
`playerctl`. trollshell does not need a custom CLI helper — its services
listen on `pactl subscribe` and the backlight sysfs path, so external
mutations show up in the bar and OSD without further wiring.

## Required packages

- `wireplumber` — provides `wpctl`, the volume / mute control used here.
  Most pipewire desktops already pull this in.
- `brightnessctl` — backlight control via sysfs (no root / no logind dance
  needed once the user is in the `video` group, or the udev rule from the
  package is installed).
- `playerctl` — MPRIS client for play/pause/next/prev across players
  (mpv, Firefox, Spotify, etc.).

Install on Arch:

```sh
sudo pacman -S wireplumber brightnessctl playerctl
```

If `playerctl` is missing the play/next/prev keys silently no-op (niri
still fires the spawn, the binary just doesn't exist). Same for the others.

## Install the bindings

niri does **not** support an `include` directive, so the snippet at
`etc/niri/binds.kdl` has to be merged into your config by hand. Two cases:

### You don't have a `binds { }` block yet

Open `~/.config/niri/config.kdl` and paste the entire `binds { … }` block
from `etc/niri/binds.kdl` near the top of the file. Reload niri (it picks
up config changes automatically; otherwise `niri msg action reload-config`).

### You already have a `binds { }` block

Copy every binding line (everything between `binds {` and the closing `}`)
into your existing `binds { }` block. Don't nest `binds` inside `binds` —
niri parses one block per name.

A quick way to view the bindings without leaving the repo:

```sh
cat etc/niri/binds.kdl
```

Then paste the inner lines.

## Verification

1. Press `XF86AudioRaiseVolume` (often `Fn+F3` on a laptop). The trollshell
   OSD should pop up showing the new volume level. Hold the key to confirm
   it ramps — niri auto-repeats spawn binds by default.

2. Confirm the actual sink moved:

   ```sh
   wpctl get-volume @DEFAULT_AUDIO_SINK@
   # e.g. Volume: 0.55
   ```

3. Press `XF86AudioMute`. The OSD should show the muted icon, and the
   volume chip in the bar should pick up the mute state. Press again to
   unmute.

4. Press `XF86MonBrightnessUp` / `Down`. The OSD should reflect the new
   brightness; `cat /sys/class/backlight/*/brightness` should match.

5. Start a player (`mpv some.mp3` is enough). `XF86AudioPlay` toggles
   pause; `XF86AudioNext` / `Prev` move tracks. The MPRIS bar widget
   updates in lockstep.

6. Lock the screen (`loginctl lock-session` or the trollshell bar Power button).
   The media-transport keys and mute keys should still work; volume and
   brightness keys should not — that's the `allow-when-locked=true` split.

If the OSD doesn't appear but `wpctl get-volume` reports a change, the
issue is in the OSD widget, not these bindings — check task #30's wiring.

## Niri version notes

- Niri auto-repeats spawn binds by default — no explicit `repeat=true` is
  needed (and no such keyword exists). To DISABLE auto-repeat on a specific
  bind (rare; not used here), use `repeat=false` — that keyword landed in
  niri ≥ 0.1.8.
- `allow-when-locked=true` requires niri ≥ 0.1.5. On older niri the binding
  is parsed but the keyword is treated as a single-fire binding that simply
  does not fire while the screen-lock client holds focus — i.e. the keys
  won't work from the lock screen, but nothing else breaks.
- `cooldown-ms=N` is also available on recent niri if you find a flaky media
  key double-triggers; not used in the shipped snippet.

If your niri is older than the above and the parser rejects the keywords
outright, drop them from the affected lines and re-load.

## Tuning

- The 5 % step on volume / brightness is conservative; bump to `10%+` if
  your hardware buttons feel sluggish.
- `wpctl set-volume … -l 1.5` caps the boost at 150 %. Drop the `-l 1.5`
  if you want to allow the full pipewire range, or lower it (e.g. `-l 1.0`)
  to forbid any boost above 100 %.
- For multi-MPRIS setups where `playerctl` picks the wrong player, add
  `--player=mpv,firefox,spotify` (priority order) to the playerctl spawns.

## What this does NOT do

- It does NOT call into trollshell over D-Bus. Volume / brightness state is
  read back via `pactl subscribe` (pipewire service) and sysfs `inotify`
  (brightness service); those are the existing event paths and the OSD
  hooks off them. A custom IPC channel was considered and rejected
  — `wpctl` and `brightnessctl` already do the right thing.
- It does NOT bind a power-menu shortcut (lock / logout / suspend / reboot /
  shutdown). Those actions live on a drawer page (`panel_power_menu` in
  `trollshell/src/panels/power_menu.rs`); reaching it from a keybind no
  longer needs new IPC — `trollshell/src/commands.rs` already registers
  `open-page`, `power-menu`, `toggle-sidebar`, and `toggle-recording` as
  `org.gtk.Actions` on the shell's existing session-bus name (#219):

  ```sh
  busctl --user call mov.vibec0re.trollshell /mov/vibec0re/trollshell \
      org.gtk.Actions Activate 'sava{sv}' power-menu 0 0
  ```

  Whether/how to actually bind this in `binds.kdl` is left to the deployer —
  this file mirrors Annika's personal chords, and which key (if any) maps to
  which verb is a preference call, not something this doc should default.

- It does NOT bind the keyboard-backlight keys (`XF86KbdBrightnessUp/Down`).
  Those typically don't have a sysfs uniformity story; add them by hand if
  your hardware exposes them and you want them.
- It does NOT handle the airplane-mode key (`XF86RFKill`). That's a
  rfkill / NetworkManager concern, not media.
- It does NOT ship a complete niri `config.kdl`. Only the `binds { }`
  fragment relevant to media keys lives here; the rest of niri configuration
  (layout, output, input) is the user's call.

## Frame struts

trollshell's frame overlay (added in 2026-05-06) draws a dark gradient
border around the workspace and rounds the inner corners. For the
border to align with niri's tiling area, niri needs matching struts on
the left, right, and bottom — top is already reserved by the bar's
exclusive zone.

The snippet at `etc/niri/frame.kdl` defines those struts. niri does
**not** support `include`, so the snippet has to be merged into your
config by hand. Two cases:

### You don't have a `layout { }` block yet

Open `~/.config/niri/config.kdl` and paste the entire `layout { … }`
block from `etc/niri/frame.kdl` near the top of the file (or anywhere
at the top level). Reload niri (it picks up config changes
automatically; otherwise `niri msg action reload-config`).

### You already have a `layout { }` block

Copy only the `struts { … }` sub-block into your existing `layout { }`.
If you already have a `struts { }` block of your own, merge values: any
existing inset on left / right / bottom should be the larger of the two,
or 8 to match the frame.

### Verification

1. Restart trollshell (or wait for auto-reload). The bar should be a
   flush full-width strip at top, with an 8px dark border on the left,
   right, and bottom of the workspace and rounded corners on all four
   sides of the cutout.
2. Open a window and snap it into a corner. The window's edge should
   stop 8px inside the screen edge (struts working) and the visible
   corner should appear rounded (frame overlay working).
3. If the window touches the screen edge, the strut isn't in effect —
   re-check the merged `layout { }` block.

### Tuning

Both numbers (frame thickness in trollshell, struts in niri) MUST match.
If you change one, change the other:

- niri: `etc/niri/frame.kdl` → `struts { left N; right N; bottom N }`
- trollshell: `trollshell/src/overlays/frame.rs::FRAME_THICKNESS`
