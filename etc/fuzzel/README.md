# fuzzel app launcher for niri+trollshell

Ships fuzzel as the application launcher, bound from niri to `Mod+D`.
fuzzel is a wayland-native `.desktop` launcher: it reads entries from
`$XDG_DATA_DIRS` (system, user, and flatpak exports) and renders a
fuzzy-matched overlay. No indexer, no daemon — it's launched on-chord
and exits when the user picks an entry or hits `Esc`.

This is config-only. No trollshell / hytte-services code is involved;
fuzzel runs in its own process and `exec`s the chosen entry directly.

## Required packages

- `fuzzel` — the launcher itself.

Install on Arch:

```sh
sudo pacman -S fuzzel
```

That's the entire dependency list. fuzzel renders with its own
cairo/pango stack, so no GTK theme packages are pulled in for it
specifically (though it will honor your system font / cursor settings
via fontconfig and the wayland cursor protocol).

## Install the config

fuzzel reads `~/.config/fuzzel/fuzzel.ini` by default. Symlink the
shipped file:

```sh
mkdir -p ~/.config/fuzzel
ln -s "$PWD/etc/fuzzel/fuzzel.ini" ~/.config/fuzzel/fuzzel.ini
```

(Run from the repo root, or substitute the absolute repo path.)
The symlink target is absolute — recreate it if you move or rename
the repo.

## Niri keybind

The `Mod+D` binding lives in the existing `etc/niri/binds.kdl`
fragment (added alongside the media keys). niri has no `include`
directive, so see `etc/niri/README.md` for how to merge that block
into your `~/.config/niri/config.kdl`. The relevant line:

```
Mod+D hotkey-overlay-title="Run an Application: fuzzel" { spawn "fuzzel"; }
```

`Mod+D` was picked to match the dmenu convention and niri's own
default config. If you prefer `Mod+Space`, swap the chord — niri's
default config leaves `Mod+Space` commented out, but some users bind
it to `switch-layout "next"` for keyboard-layout cycling, so check
your own config before changing it.

## Verification

1. Press `Mod+D`. The fuzzel overlay should appear, prompt `>` on
   the left, no entries yet.

2. Type `term`. Your terminal emulator(s) should appear in the result
   list — fuzzel matches against `Name`, `GenericName`, `Comment`, and
   the filename of the `.desktop` entry (see the `fields=` line).
   Press `Enter`, the terminal launches.

3. Confirm flatpak apps surface (if you use them):

   ```sh
   ls /var/lib/flatpak/exports/share/applications/ ~/.local/share/flatpak/exports/share/applications/ 2>/dev/null
   ```

   Anything in those directories appears in fuzzel automatically —
   they are part of `$XDG_DATA_DIRS` on a typical Arch flatpak setup.

4. Hit `Esc`. fuzzel exits without launching anything.

If the chord does nothing: `pgrep -a fuzzel` to see whether niri spawned
it (the launcher might be off-screen on a multi-output setup; fuzzel
appears on the focused output by default). If fuzzel runs but is empty,
check `echo $XDG_DATA_DIRS` from the niri session — an empty value
explains why no `.desktop` files are found.

## Tuning

- **Change the chord** — edit the `Mod+D` line in `etc/niri/binds.kdl`
  and reload niri (`niri msg action reload-config`, or save and niri
  picks it up automatically).
- **Terminal launcher fallback** — `.desktop` entries with
  `Terminal=true` need a host terminal. Uncomment the `terminal=`
  line in `fuzzel.ini` and point it at your preferred emulator
  (`foot -e`, `alacritty -e`, `kitty -e`, …).
- **Font / size** — uncomment the `font=` line. fontconfig syntax,
  e.g. `font=Inter:size=12` or `font=monospace:size=11`.
- **Width / lines** — `width` is in characters, `lines` is the result
  cap. Bumping `lines` past ~15 starts to feel like a file manager;
  the default 10 keeps the overlay compact.
- **Match strictness** — `match-mode=fuzzy` is Levenshtein-based and
  forgives typos. Switch to `match-mode=fzf` (substring, fuzzel's
  own default) if fuzzy returns too many spurious matches, or
  `match-mode=exact` for strict prefix matching.

To restyle locally without touching the shipped config, drop a
`[colors]` block into `~/.config/fuzzel/fuzzel.ini` after replacing
the symlink with a regular file (or layer overrides via
`fuzzel --config=…` if you want to keep the symlink).

## What this does NOT do

- It does NOT pin colors. Per the project rule, the shipped config has
  no `[colors]` section — fuzzel uses its own defaults / GTK system
  theme, which sits next to the Adwaita drawer fine. To pin a palette,
  add a `[colors]` block locally; see the trailing comment in
  `fuzzel.ini` for the available keys.
- It does NOT replace the trollshell drawer's app-related pages. The
  drawer is for system state (network, bluetooth, mpris, power);
  fuzzel is for "launch a program by name". They don't overlap.
- It does NOT ship a clipboard / emoji / calculator launcher mode.
  fuzzel supports a `--dmenu` mode that reads stdin lines and prints
  the selection — useful for ad-hoc menus from scripts — but no
  shipped binding uses it. See `fuzzel(1)` for that flag.
- It does NOT autostart. fuzzel is launched on-chord and exits after
  the user picks; there's nothing to keep running.
