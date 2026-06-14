# Clipboard history for niri+trollshell

Clipboard history is provided by [cliphist](https://github.com/sentriz/cliphist),
fed by `wl-paste --watch` from [wl-clipboard](https://github.com/bugaevc/wl-clipboard).
trollshell's "Clipboard" drawer page is a thin reader on top — it re-runs
`cliphist list` whenever the page opens and lets you click an entry to
re-paste it via `cliphist decode <id> | wl-copy`.

## Required packages

- `cliphist` — the storage daemon (it's actually just a CLI, fed by wl-paste).
- `wl-clipboard` — provides `wl-paste` and `wl-copy`.

Install on Arch:

```sh
sudo pacman -S cliphist wl-clipboard
```

## Install the systemd units

Two units, one for text, one for images. Both run a long-lived
`wl-paste --watch cliphist store` under `niri-session.target`.

The units live at `etc/systemd/user/cliphist-text.service` and
`etc/systemd/user/cliphist-image.service` in this repo. Symlink them into
`~/.config/systemd/user/`, then enable + start:

```sh
mkdir -p ~/.config/systemd/user
ln -sf "$PWD/etc/systemd/user/cliphist-text.service" \
       ~/.config/systemd/user/cliphist-text.service
ln -sf "$PWD/etc/systemd/user/cliphist-image.service" \
       ~/.config/systemd/user/cliphist-image.service
systemctl --user daemon-reload
systemctl --user enable --now cliphist-text.service cliphist-image.service
```

Both units are bound to `graphical-session.target` — they start after the
Wayland session is up and stop when it ends.

## Verify

Copy something (text or an image), then either:

- **From the trollshell drawer**: open the bar's drawer → Clipboard page.
  The entry should appear at the top.
- **From the CLI** (ground truth):

  ```sh
  cliphist list | head
  ```

  Each line is `<id>\t<preview>`. Images render as
  `[[ binary data 12.3 KiB png ]]`.

If nothing shows up, check the units:

```sh
systemctl --user status cliphist-text.service cliphist-image.service
```

A common failure is `wl-paste: Failed to connect to a Wayland server` —
that means the unit started before the compositor published
`graphical-session.target`. The units' `Requisite=` should prevent that;
if you see it anyway, restart them by hand and the next clip should land.

## Tuning

Cliphist holds the last 750 entries by default. To raise / lower:

```sh
cliphist store --max-items 200 < /dev/null   # one-shot probe; doesn't help long-running watcher
```

For the watcher, pass `--max-items` (or `--max-dedupe-search`) inside the
unit's `ExecStart`. Edit `etc/systemd/user/cliphist-text.service` and add
the flag after `cliphist store`:

```
ExecStart=/bin/sh -c 'exec wl-paste --watch cliphist store --max-items 200'
```

Then `systemctl --user daemon-reload && systemctl --user restart cliphist-text.service`.

To clear the history entirely:

```sh
cliphist wipe
```

The trollshell drawer caps the _visible_ list at 50 entries regardless of
how many cliphist holds — that's purely a UI cap, not a storage cap.

## Opening the clipboard page

There is no shipped niri keybind for the clipboard drawer. trollshell
doesn't expose an IPC channel for "open drawer at page X" yet, so the
only way in is the bar's drawer chip — open the drawer normally and
switch to the **Clipboard** page. A `Mod+V`-style binding awaits a
trollshell IPC follow-up; once that lands, niri can spawn a small
helper to toggle the page directly.

## How it composes

- `wl-paste --watch cliphist store` is one long-running process. Every
  time wl-paste detects a clipboard change, it spawns `cliphist store`
  with the new payload on stdin.
- The image variant uses `wl-paste --type image --watch …` so PNG/JPEG
  blobs land alongside text in the same database.
- trollshell never writes to cliphist's database. It only reads via
  `cliphist list` and re-pastes via `cliphist decode <id> | wl-copy`.
- The "Clipboard" drawer page calls `clipboard::refresh()` on open, which
  re-runs `cliphist list` off the GTK thread and updates the bound
  signal. There is no background polling.

## What this does NOT do

- **No clip pinning.** Every entry rotates out as new clips push it past
  the cap. Upstream cliphist has no pinning concept.
- **No search / filter UI.** The page is a flat list of the last ~50 clips;
  use `cliphist list | grep …` from a terminal if you need to search.
- **No multi-select / batch paste.**
- **No rich-format paste.** wl-copy receives whatever bytes `cliphist
decode` produced — for text, that's text; for images, the original blob
  goes back to the clipboard with cliphist's recorded MIME type.
- **No delete-from-UI.** cliphist's `delete` reads `<id>\t<preview>` lines
  on stdin (not an id argument), which makes per-row delete from the
  drawer awkward. Deferred to a follow-up. Use `cliphist wipe` (clears
  everything) or shell out manually for now:

  ```sh
  cliphist list | grep -F 'something I want gone' | cliphist delete
  ```
