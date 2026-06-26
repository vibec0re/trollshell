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

Copy the nine binding lines (everything between `binds {` and the closing
`}`) into your existing `binds { }` block. Don't nest `binds` inside
`binds` — niri parses one block per name.

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
  shutdown). Those actions live on a drawer page (`page_power_menu` in
  `trollshell/src/widgets/pages.rs`); reaching it from a keybind would need
  a tiny trollshell IPC channel that doesn't exist yet. For v1 the page is
  reachable through the existing modal infrastructure only — a follow-up
  task will add a chip and/or a `Mod+Escape` binding once the IPC lands.

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

## Frosted-glass blur

trollshell's chrome surfaces (bar, sidebar, drawer) are translucent so the
compositor can blur the wallpaper behind them for a frosted-glass look. This
needs **niri ≥ 26.04** (the `ext-background-effect` protocol / `layer-rule`
`background-effect` block) and **two coupled pieces**:

1. **Translucency (already in the shell):** `@shell_background` in
   `assets/trollshell/style.css` carries an alpha < 1
   (`rgba(25, 15, 45, 0.85)` — mostly opaque, so the frost reads as a subtle
   dark vibrancy rather than a washed-out panel). Blur is only visible
   _through_ a translucent surface — set the alpha back to `1.0` to turn the
   frost off entirely, or lower it for a stronger (lighter) frost.
2. **The blur rules:** `etc/niri/blur.kdl` holds the `layer-rule { }` blocks
   that tell niri to blur trollshell's surfaces, matched by their
   layer-shell namespace (`hytte-bar*`, `hytte-sidebar*`, `hytte-modal*`).

niri does **not** support `include`, so — as with `binds.kdl` and
`frame.kdl` — merge the blocks by hand. Paste every `layer-rule { }` block
from `etc/niri/blur.kdl` at the **top level** of `~/.config/niri/config.kdl`
(not nested inside another block), then reload (auto, or
`niri msg action reload-config`).

### Why only those three namespaces

The `hytte-frame` (fullscreen frame overlay) and `hytte-popup-catcher`
(invisible dismiss catcher) surfaces span the whole output — blurring them
would frost the **entire screen**, so they're deliberately excluded. The
`hytte-osd` / `hytte-toasts` / `hytte-prompt` surfaces aren't backed by
`@shell_background` yet (transparent wrappers / `@card_bg_color`), so they
won't frost until their card backgrounds gain an alpha — a follow-up.

### Client-side region scoping (the frost hugs the content)

The `layer-rule` blur frosts the **whole layer-shell surface geometry**, not the
painted content. That's fine for the bar (the surface _is_ the content), but it
bit the sidebar and drawer (#192/#193):

- the sidebar surface is **always mapped** (it stays below the bar in z-order),
  so an `xray` strip lingered where the sidebar was even when closed; and
- the drawer is **one fullscreen surface** (the card + an invisible dismiss
  catcher in one window, #109), so opening it frosted the entire screen.

trollshell now also scopes the frost from the **client** side via niri 26.04's
`ext-background-effect-v1` `set_blur_region` (the `hytte-blur` crate). The shell
hands niri the visible card's rectangle — "blur only here" — driven off the same
slide animation, so the frost tracks the sidebar as it slides in/out and hugs the
drawer card instead of the whole surface. Closed → empty region → no strip.

**The `layer-rule` blocks above stay — they are required, not just a fallback.**
They are what _enables_ blur for those namespaces (the client region only
_narrows_ it). Critically, `xray false` on the sidebar and drawer can only be
set via a layer-rule — the client `set_blur_region` protocol forces `xray true`,
so "use the client protocol only and drop blur.kdl" is NOT viable for the
overlays. On **niri < 26.04** (no `ext-background-effect` global), the client
scoping is a graceful no-op and the layer-rule blur is the (un-scoped) fallback
— an additional reason to keep the blocks merged.

### Tuning

- **Frost strength:** the `@shell_background` alpha in `style.css` (lower =
  more wallpaper shows through, less readable; higher = subtler frost).
- **`xray`:** the bar ships `xray true` (frosts the wallpaper only — cheap,
  computed once; the bar sits over wallpaper so nothing occludes it). The
  sidebar and drawer ship `xray false` (frost the actual window content behind
  them — they overlay the window stack, so `xray true` would frost only the
  occluded wallpaper and leave windows sharp, giving no visible frost).
  Trade-off: `xray false` reads a touch brighter/off-colour next to the bar
  and is pricier (niri recomputes on movement), but it is the only way the
  overlay frost is visible over windows. **Note:** the client
  `set_blur_region` protocol forces `xray true` — `xray false` can only be
  set via a layer-rule, so the blur.kdl rules for the sidebar and drawer
  cannot be dropped in favour of the client protocol alone.
- **`geometry-corner-radius`:** rounds the blur clip. `0` for the flush
  bar/sidebar; bump the `hytte-modal` value to match the drawer's drawn
  corner radius if it shows a square halo.
- `noise` and `saturation` keys are also accepted inside `background-effect`
  if you want grain or a colour boost.

### Verification

1. On niri ≥ 26.04, merge the blocks and reload. The bar should show the
   wallpaper softly blurred through it instead of flat dark purple.
2. Toggle the sidebar / open the drawer — same frost.
3. No frost? Check (a) niri version, (b) the blocks landed at top level, and
   (c) `@shell_background` still has its alpha (a rebuild reverts CSS edits).
