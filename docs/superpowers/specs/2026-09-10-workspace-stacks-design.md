# Workspace stacks — saved, named, launchable workspaces (Discussion #1063)

**Status:** proposed — waiting for @annikahannig to read. Nothing here is built.
**Source:** Annika's paper design in Discussion #1063 (2026-09-10) and her answers in the thread the same afternoon. Where this document says "settled", it quotes those answers; where it says "default", it is my proposal and one word on the PR changes it.

## 1. What it is, in one paragraph

A **workspace** is a niri workspace. It starts ephemeral. **Saving** it gives it a name and a **stack**: the apps on it, the monitor it lives on, and the layout template that was applied. A saved workspace is a card in a new drawer page; the card is **Active** when its apps are running on a screen and **Inactive** (greyed, "not on a screen") when they are not. **Start** creates the workspace, launches the apps, applies the layout. **Stop** stops the apps; the card stays, greyed, in its place. **Edit** reuses the drawer and changes the app list, an app's launch command, and whether the workspace starts at login. Cards are ordered, per monitor, monitors side by side.

Settled with Annika: freeze / thaw is out ("too ambitious"); apps come back from their desktop entry, not their captured command line ("desktop entry might be right"); a terminal comes back as a terminal, its content does not ("restoring the terminal app sufficient for now"); the actions are exactly **start/stop** and **edit**; pinned workspaces launch eagerly at login ("create workspace and then launch apps and apply layout"); stop means stop the apps and grey the card ("not assigned to screen"); stacks are per monitor, shown side by side; inactive stacks keep their place in the order.

## 2. What the tree already gives us (verified)

| need                                     | where it is today                                                                                                                                                                                                                                                                                                                                 |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| live workspaces and windows, per monitor | `crates/hytte-services/src/niri.rs:276` `workspaces()`, `:300` `windows()`, `:290` `focused_output()`; `trollshell/src/widgets/workspaces.rs:14-24` already filters `workspaces()` by `monitor.connector()` — the per-monitor precedent                                                                                                           |
| a window's identity                      | niri-ipc 26.4 `Window { app_id, pid, workspace_id, .. }` (`lib.rs:1297-1328`); `Workspace { id, idx, name, output, is_active, is_focused }` (`lib.rs:1416-1441`)                                                                                                                                                                                  |
| naming a workspace live                  | niri-ipc `Action::SetWorkspaceName` (`lib.rs:583`), `UnsetWorkspaceName` (`:599`), `MoveWorkspaceToIndex` (`:567`), `MoveWorkspaceToMonitor` (`:793`), `MoveWindowToWorkspace` (`:508`) — the shell's `niri` service wraps `FocusWorkspace` today (`niri.rs:486`); the others are the same fire-and-forget shape                                  |
| launching into systemd units             | `trollshell/src/plugin_launcher.rs:528+` — the `systemd-run --user` invocation, built in exactly one place (`command` is the only way out of the module; `args` is private, #984)                                                                                                                                                                 |
| talking to `systemd --user`              | `crates/hytte-services/src/systemd.rs:186-196` — one-shot session-bus calls for the plugin units (`ListUnitFilesByPatterns` at `:337`); the system-bus half watches failed units                                                                                                                                                                  |
| app icons for a stack row                | `trollshell/src/widgets/window_list.rs` (`create_window_button` / `apply_window_visuals`) already turns a niri `Window` into a small icon button for the bar — the same lookup serves the stack row                                                                                                                                               |
| a config file with base + overlay        | `crates/hytte-config` (#866/#868): `Subsystem` (`NAME`, `DEFAULT_TOML`, `validate`), the four merge rules (`merge.rs:11`: scalars overlay-wins, **tables deep-merge, arrays replace whole**), `render_overlay` for a format-preserving save; #1040's `core-leds.toml` is the pilot and the template (raw `toml::Value` fields, per-key tolerance) |
| monitors                                 | `crates/hytte-ui/src/app.rs:193` `monitors()`, `:203` `monitors_changed()`, `monitor.rs:25` `connector()`                                                                                                                                                                                                                                         |
| the layout templates                     | `hytte-plugin-niri-layouts` (#1026/#1056): `equal` / `golden` / `split`, applied by its CLI (`hytte-plugin-niri-layouts apply <layout>`, the `spawn`-bind path in `etc/niri/binds.kdl`)                                                                                                                                                           |

Nothing in the list needs a new daemon or a wire change.

## 3. The model

### 3.1 Identity

A saved workspace has a **name** (unique per file; it is also the niri workspace name while active) and a **monitor** (a connector; optional — a stack without one starts on the focused monitor). While the stack is Active, the niri workspace carries the name (`SetWorkspaceName`), so the bar's workspace chip shows it and a keybind can `focus-workspace "chat"`. On Stop the name is removed (`UnsetWorkspaceName`) and the now-empty workspace disappears the way any empty niri workspace does; the card remains, greyed.

### 3.2 The stack

Ordered list of apps. Each app is a **desktop entry id** (`org.mozilla.firefox`, `Alacritty`) — the thing niri reports as `app_id` for its windows, and the thing the app switcher already resolves to an icon and a name — plus an optional `exec` override for the cases a bare entry does not cover (a terminal that should run a command, a browser that should open a profile). Restore = launch the entry's `Exec` (or the override). Content is not restored; that is settled.

### 3.3 Active / Inactive, and who owns the processes

**Start** launches every app of the stack as a transient user unit inside one **slice per workspace**: `trollshell-ws-<name>.slice`, units `trollshell-ws-<name>-<app>-<n>.service`, through the launcher's existing `systemd-run` invocation extended with `--slice=`. **Stop** is one call: `systemctl --user stop trollshell-ws-<name>.slice` stops every unit in it (SIGTERM, then systemd's own kill escalation). No pid bookkeeping, no orphan hunting.

Annika's question — "can we get the app start/stop from systemd from the pid?" — has a two-part answer. `org.freedesktop.systemd1.Manager.GetUnitByPID` exists and the session-bus call shape is already in `systemd.rs`; but an app niri itself spawned (a `spawn` bind, a launcher) sits in **niri's** cgroup, not a unit of its own, so a pid → unit lookup only helps for apps this feature launched. For a window that was not launched by the shell — the ephemeral workspace you are about to save, or a stray you opened from a terminal into an Active stack — Stop closes it through niri instead: `Action::CloseWindow { id }` for each such window (a graceful close request, the same thing the window's own close button does). So a card's Stop is: stop the slice, then `CloseWindow` for any remaining window on that workspace. The card's state is derived, not stored: **Active** = the niri workspace with that name exists and has ≥ 1 window; **Inactive** otherwise.

### 3.4 Start, step by step

1. Pick the monitor: the stack's `monitor` if connected, else the focused output (and say so in the card's subtitle).
2. Make the workspace: focus that output's empty trailing workspace (niri always keeps one), `SetWorkspaceName` it — a named workspace persists while named — and `MoveWorkspaceToIndex` so the active workspaces on that monitor follow the saved order (§3.6).
3. Launch each app into the slice with that workspace focused, so niri opens the windows there; then, for a grace window (5 s, the same class as `RUN_COMMAND_TIMEOUT`), reconcile: a window whose `app_id` belongs to the stack but landed elsewhere is moved with `MoveWindowToWorkspace`. Matching is by `app_id` first, `pid` (the unit's `MainPID`) second.
4. When every app has a window (or the grace window ends), apply the layout template by spawning `hytte-plugin-niri-layouts apply <layout>` with the workspace focused — the CLI already exists for the `spawn` binds and needs no shell-side copy of the layout code. `layout = "none"` skips it.

Autostart (§3.5) is the same sequence, run once per pinned stack at session start.

### 3.5 Autostart

`autostart = true` on a stack means: at session start — after the niri service has connected and reported its outputs, and only for stacks whose monitor is connected (or has no monitor) — run §3.4 for each such stack in saved order. Settled: eager ("create workspace and then launch apps and apply layout"). A stack whose monitor is not connected at login is skipped with one info line and starts when its monitor appears? — **default: no**, it starts only when you press Start; hot-plug autostart is a follow-up if wanted.

### 3.6 Order

The file carries the order (§4). The drawer shows cards in that order per monitor; Active and Inactive cards interleave exactly as saved, which is the "keep inactive at the right place" ask. Among the Active ones, niri's own index follows: Start places the new workspace at the position its saved order implies among the currently-active named workspaces on that monitor. Dragging a card in Edit rewrites the order in the file.

### 3.7 Save current

The **+** on the page ("Save current" on the paper): take the focused workspace, list its windows, map each `app_id` to a desktop entry (an `app_id` with no entry becomes an app with `exec` set to the process's command line, shown for you to correct in Edit), record the monitor, and the layout template if the workspace was last laid out by the niri-layouts plugin (it does not know; **default:** `none`, editable). Then name it: a small entry in the card. The niri workspace gets the name immediately (`SetWorkspaceName`), so the workspace you saved _is_ the Active card — no relaunch. Its windows were not launched by the shell, so until you Stop and Start it once, Stop uses the `CloseWindow` path (§3.3).

## 4. The file

`workspaces.toml`, a `Subsystem` through #866's layering — base from home-manager (your pinned defaults), overlay written by the drawer through `render_overlay` (format-preserving, still hand-editable, live-reloaded the way `core-leds.toml` is).

```toml
# ~/.config/trollshell/workspaces.toml
order = ["chat", "dev", "music"]      # cards top to bottom; arrays replace whole

[workspace.chat]
monitor   = "DP-1"                    # optional; absent = the focused monitor
autostart = true
layout    = "golden"                  # equal | golden | split | none
apps = [
  { id = "org.mozilla.firefox" },
  { id = "Alacritty", exec = "alacritty -e weechat" },
]

[workspace.dev]
monitor   = "DP-1"
autostart = false
layout    = "split"
apps = [{ id = "dev.zed.Zed" }, { id = "Alacritty" }]
```

Why a **table keyed by name** and not an array of workspaces: the merge rules deep-merge tables and replace arrays whole (`merge.rs:11`), so with `[workspace.<name>]` a home-manager base can pin `chat` and `dev` while the overlay adds `music` and changes `dev`'s layout — with `[[workspace]]` the overlay would wipe the base. `apps` and `order` are arrays on purpose: editing a stack's app list or the order replaces it whole, which is the semantics you want. Unknown keys warn (rule 4). A stack whose `id` names no desktop entry is not an error: the card shows the app greyed with "no desktop entry" and Start skips it with one warning.

Workspace names double as systemd slice names, so they are validated the way plugin ids are (`systemd.rs:258` `is_valid_plugin_id`): `[a-z0-9-]`, no leading dash; the Save entry refuses anything else and offers the sanitised form.

## 5. The UI

**`Page::Workspaces`** in the drawer (`modal.rs`'s `Page` enum; the `Stats*` variants at `modal.rs:314-318` are the precedent for a page with sub-pages). Body: one column per connected monitor, side by side (settled), each headed by the connector name and holding that monitor's cards in file order; a stack whose monitor is not connected is listed in a trailing "not connected" column, greyed. A card: name, the stack row (small app icons — the bar's app-switcher lookup — glowing when a window of that app is open on the workspace, dim when not), and the two actions: **start/stop** (one button, its icon by state) and **edit**. Active cards have a normal background; Inactive cards are greyed with the subtitle "not on a screen". The page header has **+** (Save current, §3.7).

**`Page::WorkspaceEdit(name)`**: the same drawer, content replaced (settled). Name, monitor (a dropdown of connected connectors + "focused"), autostart switch, layout dropdown, the app list with per-row remove and an `exec` override entry, an **Add app** row using the desktop-entry picker (the launcher's list), and drag handles that rewrite `order`. Save writes the overlay file; the live reload re-renders the page. Cancel reverts.

Nothing here needs the modal dialog of #1010: this is a shell page opened from the bar, so the drawer is the right surface.

## 6. Phases

Four PRs, each reviewable alone, opus-built once this is read:

1. **Read-only page.** `Page::Workspaces` with cards derived from live niri state per monitor (every named niri workspace is a card; unnamed ones are not), stack icons glowing by open windows, no file, no actions. Proves the per-monitor layout and the icon row.
2. **The file + start/stop.** `workspaces.toml` as a `Subsystem` (on #1040's template), `+` Save current, Start into a slice with reconcile, Stop = slice + `CloseWindow`, derived state, greyed Inactive cards. The `systemd-run` module grows `--slice=`; the niri service grows the four action wrappers.
3. **Autostart + layout + order.** Login sequence, `apply <layout>` via the CLI, `MoveWorkspaceToIndex` placement, `order` honoured.
4. **Edit.** The sub-page: app list, `exec` override, autostart, monitor, layout, drag order, desktop-entry picker.

Follow-ups filed only if glass asks: hot-plug autostart; adopting strays into the slice (needs a delegated scope — a spike); a per-window layout memory beyond the three templates.

## 7. Tests (the load-bearing ones)

- The file: base + overlay merge keeps a base stack the overlay does not mention, replaces `apps`/`order` whole, warns on an unknown key; a save is byte-identical when nothing changed (the `places.toml` pin style).
- Derived state: a named workspace with windows → Active; without → Inactive; a stack whose monitor is absent → the trailing column.
- Start: the launcher is called once per app with the slice property; a window with the stack's `app_id` on another workspace is moved within the grace window; the layout CLI is spawned once after the last window (or the grace end).
- Stop: the slice stop is issued, then `CloseWindow` for each remaining window, never before.
- Autostart: at start, only `autostart = true` stacks on connected monitors run, in file order; mutation: run all → red.
- Name validation: the same table `is_valid_plugin_id` pins, mutation per rule.
- GTK: the page renders one column per monitor with cards in file order; Inactive cards carry the greyed class; the stack row's glow follows a windows-signal change.

## 8. Non-goals, stated so nobody builds them

- No freeze / thaw (settled out).
- No restore of window content, scrollback, or exact positions beyond the layout template.
- No cross-machine sync; the file is per machine (home-manager is how you share a base).
- No wire / plugin change: this is a shell page over shell services.
- No new daemon: state is niri's and systemd's, the file is the only thing the shell owns (the system-daemon-as-state-store rule).
