# Plugin pages open in a dialog, not the drawer (#1010)

**Status:** proposed — waiting for @annikahannig to read. Nothing here is built.
**Decision so far (issue #1010):** plugin pages opened from a sidebar card get a modal dialog, not the drawer (Annika, 2026-09-10 07:02Z). Keep it simple, **no dimming** (Annika, 2026-09-10 11:02Z).
**Origin:** #963's live-verify — the agents card's "open the roster" lands in the drawer, which hangs off the bar's top-right corner; Mara's retest (PR #963, 10:56Z) hit exactly that.

## 1. What changes

One sentence: `Effect::OpenPage(Page::PluginSelf)` from a **sidebar-mounted** plugin opens the plugin's panel in a centered overlay on the focused output; from a **bar-mounted** plugin it keeps opening the drawer, as today. The plugin does not change a line.

Everything else — the shell's own pages (Wi-Fi, Bluetooth, power, …), the drawer, the sidebar cards, the wire vocabulary — stays as it is.

## 2. The surface: `overlays/dialog.rs`

The third member of a family the shell already has. `overlays/prompt.rs` (the Wi-Fi secret prompt, `show_prompt`) and `overlays/consent.rs` (the #487 consent card) both build the same window and the dialog copies it, not a new shape:

| property | value | precedent |
| --- | --- | --- |
| window | `layer_window(monitor).layer(Layer::Overlay).exclusive(false).keyboard_mode(KeyboardMode::Exclusive).namespace("hytte-dialog")` | `prompt.rs:319-323` |
| placement | centered on the **focused** output (the same `components::focused_output::current()` the effect broker already uses for the drawer, `plugins/effects.rs:118-131`) | #499/#517 |
| size | the card is the surface: `set_size_request` to a minimum, an `adw::Clamp` around the body (`maximum-size` ≈ the sidebar's card width × 2), and a bounded-height `gtk::ScrolledWindow` inside — the #967 shape that also ends the "a long URL widens the surface" class | `prompt.rs:329`, #967 |
| body | the plugin's panel tree, mounted with the drawer's own child: `plugins::region::build_panel_child(panels_render_signal(), active_panel_signal())` — the same reconciler, the same event routing to the plugin's live connection, the same #903 destroy story | `region.rs:452-470` |
| header | one row: the plugin's name (from its manifest) and a close button | `consent.rs` |
| dismiss | **Escape** (an `EventControllerKey` on the window, `prompt.rs:389-397`), the **close button**, and the `dialog-close` action registered in `commands.rs` so a niri bind can drop it; closing hides the window and clears the active panel | `prompt.rs:391`, #219 |
| one at a time | opening a dialog while one is up replaces its content (same plugin: no-op; different plugin: swap the active panel) — there is never a second dialog window per monitor | `close_prompt()` before `show_prompt()` |
| CSS | `.ts-dialog` on the window, `.ts-dialog-root` on the box; the plugin tree is reached through the existing `.ts-plugin-panel` descendant rules, so plugin styling does not fork between drawer and dialog | `style.css:570` |

**What "modal" means here, and what it does not.** `KeyboardMode::Exclusive` makes the dialog own the keyboard while it is up — Escape always works, and typing into a plugin's `Entry` lands in the plugin. That is all. There is **no backdrop, no dimming, no full-output surface**: other windows stay visible and clickable, exactly like the secret prompt today. Consequently there is no "click outside to dismiss" — that needs a full-output surface to receive the click, which is the dimming shape Annika said no to. Escape and the close button are the whole dismiss story; if that turns out to be too little on glass, the cheapest addition is dismissing on keyboard-focus loss, which niri reports to a layer surface without any extra geometry — a follow-up, not this spec.

**Per monitor, mirrored.** Like the drawer's plugin child, one dialog window is installed per monitor (`overlays::dialog::install(&monitor)` from `main.rs`'s per-monitor closure, next to `prompt::install` / `consent::install`); all mirror the same active panel, and only the one on the focused output is shown. Monitor hot-plug rebuilds it with the rest (`app.monitors_changed()`), and its render subscription dies with its window the #903/#909 way — `build_panel_child`'s inner-canvas indirection is what makes that hold, so the dialog must mount through it and not around it.

## 3. The routing rule

Today `plugins/effects.rs:118-131` sends every `OpenPage(PluginSelf)` to `modal::open_plugin_on_focused`. The change is one pure function and one call site:

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

The broker learns the plugin's `Mount` from the session that already routes its renders by it (`session.rs:122-135`, `route_render`) — the manifest's mount is passed into `broker_effect` alongside `plugin_id`. Built-in pages (`OpenPage(Page::Wifi)` etc.) are untouched: `resolve_open_page`'s `OpenBuiltin` arm still opens the drawer.

**Not in this cut:** an explicit `Effect::OpenDialog` for a plugin that wants to choose. The mount rule covers every plugin in the tree; an appended variant (a `VOCAB` bump) waits until a plugin asks for the other surface.

## 4. Visibility gating

The plugin host counts a panel as on-screen when a drawer showing it is open (`session.rs:41`, the #288/#559 `SlotVisible` push that lets a poller park). The dialog is a second such surface: the count that feeds the push becomes "a drawer showing it is open **or** the dialog showing it is open". Same `Mutable`, one more contributor; the plugin sees the same `true`/`false` edges it sees today. Without this a sidebar plugin's poller would park the moment its page opened in the dialog.

## 5. What it changes for #963

Nothing in the plugin. The agents card's in-place unfold stays (it is the right answer for one agent); the hive overview / full roster panel it opens with `OpenPage(PluginSelf)` lands in the dialog the day this merges, because the card is `Mount::Sidebar*`. Its live-verify entry moves from "opens in the drawer" to "opens centered, Escape closes".

## 6. Tests

Hermetic, on `trollshell`:

- `page_surface`: sidebar mounts → `Dialog`, bar mounts → `Drawer` (a table over every `Mount` variant, so a new variant fails to compile rather than silently picking a side).
- The broker's `OpenPage(PluginSelf)` arm calls the dialog opener for a sidebar plugin and the drawer opener for a bar one (inject the two openers the way `run_command` injects its executor).
- Visibility: with the drawer closed and the dialog open for plugin X, X's `SlotVisible` is `true`; closing the dialog sends `false` once. Mutation: drop the dialog contributor → the first assertion reds.

GTK (`system-tests`, `xvfb-run -a`), in `overlays/dialog.rs`'s own `mod tests`:

- The window has the family shape (layer overlay, keyboard exclusive, namespace) and its body is `build_panel_child` over two test `Mutable`s (the `region.rs` test seam).
- Escape hides it and clears the active panel; the close button does the same.
- Opening plugin Y while X is shown swaps the content, no second window.
- Destroying the window aborts its render subscription (the #903 assertion, copied from `region.rs`'s `gtk_tests`).

Live-verify (`docs/live-verify.md`): the #963 card → centered dialog, Escape closes, a bar chip's page still opens the drawer, two monitors → the dialog is on the one with focus.

## 7. Plan

One PR, two commits, opus-built once Annika has read this:

1. `overlays/dialog.rs` + `install` in `main.rs` + the `dialog-close` action + CSS + GTK tests.
2. `page_surface` + the broker call site + the visibility contributor + hermetic tests + live-verify.

Follow-ups filed only if glass asks for them: focus-loss dismiss; `Effect::OpenDialog`.

## 8. Non-goals, stated so nobody builds them

- No dimming, no backdrop, no full-output surface, no session-modal behaviour.
- No click-outside dismiss (needs the backdrop).
- No change to where the shell's own pages open.
- No proto change.
