# Plugin pages open in a dialog, not the drawer (#1010)

**Status:** proposed — waiting for @annikahannig to read. Nothing here is built.
**Decision so far (issue #1010):** plugin pages opened from a sidebar card get a modal dialog, not the drawer (Annika, 2026-09-10 07:02Z). Keep it simple, **no dimming** (Annika, 2026-09-10 11:02Z).
**Origin:** #963's live-verify — the agents card's "open the roster" lands in the drawer, which hangs off the bar's top-right corner; Mara's retest (PR #963, 10:56Z) hit exactly that.
**Revision 2:** corrected after a line-by-line claim check on PR #1047 (every `file:line` below was re-verified; the first draft had a design hole around the drawer's global selection, fixed in §2.1).

## 1. What changes

One sentence: `Effect::OpenPage(Page::PluginSelf)` from a **sidebar-mounted** plugin opens the plugin's panel in a centered overlay on the focused output; from a **bar-mounted** plugin it keeps opening the drawer, as today. The plugin does not change a line.

Everything else — the shell's own pages (Wi-Fi, Bluetooth, power, …), the drawer, the sidebar cards, the wire vocabulary — stays as it is.

## 2. The surface: `overlays/dialog.rs`

Two shipped shapes are combined, and nothing new is invented:

- the **window** is the drawer's: one fullscreen layer surface whose _main_ child is a transparent click-catcher and whose _overlay_ child is a positioner carrying the card (`modal.rs:658-701`, `.ts-modal-catcher` at `style.css:670-673`). That is how the drawer already gets "click outside dismisses" with **no dimming** — the catcher paints nothing; its cost is that pointer events on that output go to it while the dialog is up, which is the drawer's cost today. The one difference: `Layer::Overlay` + `KeyboardMode::Exclusive` (the secret prompt's choice, `prompt.rs:319-323`, relied on at `prompt.rs:322` / `consent.rs:138`) instead of the drawer's `Layer::Top`, so Escape always lands and a plugin `Entry` gets the keys.
- the **body** is the drawer's plugin child — `plugins::plugin_panel_slot()`'s shape (`region.rs:452-454`, `pub`), but over the dialog's _own_ selection (§2.1), not the drawer's.

| property      | value                                                                                                                                                                                                                                                                                                                                                                                                                                                       | precedent                               |
| ------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------- |
| window        | `layer_window(monitor).layer(Layer::Overlay).exclusive(false).keyboard_mode(KeyboardMode::Exclusive).namespace("hytte-dialog")`, fullscreen; `gtk::Overlay` with the transparent catcher as main child and a centered positioner as overlay child                                                                                                                                                                                                           | `prompt.rs:319-323`, `modal.rs:658-701` |
| which output  | **one global window**, presented on the focused output (`components::focused_output::current()`, the value the effect broker already passes for the drawer at `plugins/effects.rs:118-135`); rebuilt on the new focused monitor per show the way `consent.rs` keeps a connector map and one window, not one window per monitor (neither `prompt::install` nor `consent::install` is per-monitor: `main.rs:296-298` installs the prompt once on the primary) | `consent.rs`, #499/#517                 |
| card size     | `adw::Clamp` (`maximum-size` ≈ twice the sidebar card width) around a bounded-height `gtk::ScrolledWindow` — the #967 shape that also ends the "a long URL widens the surface" class                                                                                                                                                                                                                                                                        | #967                                    |
| body          | the plugin's panel tree through `region::build_panel_child(panels_render_signal(), dialog_panel_signal())` — same reconciler, same event routing to the plugin's live connection, same #903 destroy story; exposed as a new `pub fn plugin_dialog_slot()` next to `plugin_panel_slot()`, because `build_panel_child` and the two signal accessors are private to `plugins::region` (`region.rs:391-470`)                                                    | `region.rs:452-470`                     |
| header        | one row: the plugin **id** (the manifest has no display name and `SlotRender` carries none — a `name` field would be a proto change, a non-goal; the plugin's own panel tree carries its title) and a close button. New — neither the prompt nor the consent card has a close button                                                                                                                                                                        | —                                       |
| dismiss       | **Escape** (`EventControllerKey` on the window, `prompt.rs:388-397`), the **close button**, a **click on the catcher** (`modal.rs:672-682`), and a `dialog-close` `gio` action in `commands.rs` (#219; the name is free) so a niri bind can drop it. Closing hides the window and clears the dialog's selection                                                                                                                                             | `prompt.rs:391`, `modal.rs:678`         |
| one at a time | opening while one is up swaps the selection (same plugin: no-op) — never a second window                                                                                                                                                                                                                                                                                                                                                                    | `close_prompt()` before `show_prompt()` |
| CSS           | `.ts-dialog` on the window, `.ts-dialog-card` on the positioner's card; the plugin tree is reached through the existing `.ts-plugin-panel` **descendant** rule (`style.css:575`; `:570` is the root rule), so plugin styling does not fork between drawer and dialog                                                                                                                                                                                        | `style.css:570-575`                     |

**What "modal" means here.** `KeyboardMode::Exclusive` makes the dialog own the keyboard while it is up; the catcher makes any outside click close it. That is all. There is **no backdrop, no dimming**: other windows stay visible; they are not clickable _through_ the catcher while the dialog is up, exactly as with the drawer today. When the window unmaps, niri returns keyboard focus to the previous toplevel (what happens after the secret prompt closes).

### 2.1 The dialog owns its selection — the hole the first draft had

`active_panel_id` (`plugins/mod.rs:412`) is **one global** `Mutable<Option<String>>`: every monitor's drawer child mirrors it (`region.rs:820`), `modal.rs` sets it when a drawer opens a plugin page (`modal.rs:1402`) and clears it when a drawer showing a plugin page closes (`modal.rs:1130-1132`) or when the drawers are torn down (`modal.rs:1176`). A dialog mirroring the _same_ handle would be hijacked by the first of those and blanked by the other two, with no new line written.

So the dialog gets its **own** handle: `dialog_panel_id: Mutable<Option<String>>` in `PluginHandles`, `dialog_panel_signal()` / `set_dialog_panel(Option<&str>)` beside the drawer's pair in `region.rs:400-420`. The drawer's three sites never touch it; the dialog's close path never touches the drawer's. A drawer and a dialog can show two different plugins at once, and each routes its panel events to its own plugin's live connection, because `build_panel_child` keys routing on the signal it is handed, not on a global (the claim check confirmed it carries no drawer assumptions).

Two readers of the drawer's handle must read the **union** of both: `pump.rs:865` (which plugin's panel renders are pumped) and `pump.rs:920` (the "some panel is active" gate). Both become "drawer's id or dialog's id"; the builder pins each with a test that opens a dialog with every drawer closed.

## 3. The routing rule

Today `plugins/effects.rs:118-135` sends every `OpenPage(PluginSelf)` to `modal::open_plugin_on_focused` (`:132`). The change is one pure function and one call site:

```rust
/// Which surface a plugin's own page opens on. Bar chips hang off the bar,
/// so their page belongs in the drawer beneath it; sidebar cards do not, and
/// a drawer opening top-right for a card the user clicked mid-screen reads
/// as the wrong surface (#1010).
pub(super) fn page_surface(mount: Mount) -> PageSurface {
    match mount {
        Mount::SidebarLead | Mount::SidebarTop | Mount::SidebarBottom => PageSurface::Dialog,
        Mount::BarLeft | Mount::BarCenter | Mount::BarRight => PageSurface::Drawer,
    }
}
```

`Mount` has exactly those six variants (`manifest.rs:232-237`), so a seventh fails to compile here rather than silently picking a side. The broker learns the mount from a new `mount: Mount` field on `BrokeredEffect` (`plugins/mod.rs:378-382`), set where the session strips effects off a render frame — the same place `route_render` already dispatches by mount (`session.rs:122-135`). Built-in pages (`OpenPage(Page::Wifi)` etc.) are untouched: `resolve_open_page`'s `OpenBuiltin` arm still opens the drawer.

**Not in this cut:** an explicit `Effect::OpenDialog` for a plugin that wants to choose. The mount rule covers every plugin in the tree; an appended variant (a `VOCAB` bump) waits until a plugin asks for the other surface.

## 4. Visibility gating

What a plugin receives as `SlotVisible` today is **one global bool**: "any monitor's sidebar is open" (`pump.rs:982-1005`, `any_sidebar_open` over a per-monitor map, published by `publish_visibility` at `pump.rs:1035`). It is not per plugin, and the drawer contributes nothing to it (bar mounts are seeded `true` and never edged, per the `StateKey::SlotVisible` doc).

The dialog adds one contributor to that OR: `visible = any_sidebar_open || dialog_open`. Without it, a sidebar plugin whose page is up in the dialog with the sidebar closed would park its poller mid-view. Still global, still one bool; the plugin sees the same edges it sees today, plus `true` while its page is up in the dialog. (Per-plugin scoping would be a different feature and is not proposed.)

## 5. What it changes for #963

Nothing in the plugin. The agents card's in-place unfold stays (it is the right answer for one agent); the hive overview / full roster panel it opens with `OpenPage(PluginSelf)` lands in the dialog the day this merges, because the card is `Mount::Sidebar*`. Its live-verify entry moves from "opens in the drawer" to "opens centered, Escape / outside click closes".

## 6. Tests

Hermetic, on `trollshell`:

- `page_surface`: sidebar mounts → `Dialog`, bar mounts → `Drawer`, a table over all six variants.
- The broker's `OpenPage(PluginSelf)` arm calls the dialog opener for a sidebar plugin and the drawer opener for a bar one (inject the two openers the way `run_command` injects its executor).
- Visibility: sidebar closed, dialog open → `SlotVisible` is `true`; dialog closed → `false` once. Mutation: drop the dialog contributor → the first assertion reds.
- The two `pump.rs` readers see the dialog's id with every drawer closed (one test each; mutation: read only the drawer's handle → red).

GTK (`system-tests`, `xvfb-run -a`), in `overlays/dialog.rs`'s own `mod tests` and `region.rs`'s `gtk_tests`:

- The window has the family shape (layer overlay, keyboard exclusive, namespace, catcher main child, positioner overlay child).
- Escape, the close button and a catcher press each hide it and clear `dialog_panel_id`; none touches `active_panel_id`. Mutation: route the close through `set_active_panel(None)` instead → the "drawer untouched" assertion reds.
- A drawer showing plugin X and a dialog showing plugin Y coexist; closing the drawer (`modal.rs:1130-1132`'s path) leaves the dialog's content in place.
- Opening plugin Y while X is shown swaps the content, no second window.
- Destroying the window aborts its render subscription (the #903 assertion, copied from `region.rs`'s `gtk_tests`).

Live-verify (`docs/live-verify.md`): the #963 card → centered dialog; Escape, the close button and an outside click each close it; a bar chip's page still opens the drawer; two monitors → the dialog is on the focused one; the drawer open on monitor A does not blank a dialog on monitor B.

## 7. Plan

One PR, two commits, opus-built once Annika has read this:

1. `dialog_panel_id` + `plugin_dialog_slot()` + the two `pump.rs` union reads; `overlays/dialog.rs` + `install` in `main.rs` + the `dialog-close` action + CSS + GTK tests.
2. `mount` on `BrokeredEffect` + `page_surface` + the broker call site + the visibility contributor + hermetic tests + live-verify.

Follow-ups filed only if glass asks for them: `Effect::OpenDialog`; a manifest display name for the header.

## 8. Open for Annika (decide on the PR, or leave the defaults)

- **Sidebar while the dialog is up:** default — it stays as it is; the catcher covers it like everything else, and the first outside click closes the dialog only. Alternative: close the sidebar when a card opens a dialog.
- **Header text:** default — the plugin id, dim. Alternative: no header at all, just the close button.

## 9. Non-goals, stated so nobody builds them

- No dimming, no backdrop, no session-modal behaviour. (The catcher is transparent and is already what the drawer ships.)
- No change to where the shell's own pages open.
- No proto change: no `OpenDialog`, no manifest `name`.
- No per-plugin `SlotVisible`.
