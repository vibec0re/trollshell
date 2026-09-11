# Live-verify checklist

Merged PRs each carry their own "needs live-verify in a Niri session" list in the
PR body, because the build agents that write them run in sandboxed worktrees with
no compositor, no display server, and (for the EDS/HAFAS/LLM/GeoClue/
NetworkManager items) no live daemons to hit. Individually those lists are easy to
lose once a PR merges. This doc pulls every such item out of
`gh pr view <N> --json body`, dedupes and groups them by subsystem, and gives each
one a concrete command or gesture so a verify pass can be run mechanically rather
than re-deriving from memory. Source: the "Live-verify" / "Needs live-verify"
sections of the individual PR bodies — re-run `gh pr view <N> --json body` on the
PR number in parens if you need full context on _why_.

Checked items are ones Annika has already implicitly verified — noted inline.
Everything else is unchecked and still wants a pass in a real Niri session.

**Coverage: #458 through #668** (#606, #611, #622, and #628 carry no
live-verify list of their own — noted in the closing section instead).
Originally created by #507 for the 2026-07 merge wave (#458–#496); refreshed
by #602 to fold in everything merged since (#497–#596), renaming off the
month-stamped filename in that pass; refreshed again (parent effort #602) for
the #598/#604/#606/#609 burst that merged right behind it; folded in #610
immediately after the 2026-07-30 merge that landed it; folded in
#616/#622/#623/#624/#625 immediately after the next 2026-07-30 merge burst
(via #628); folded in #629/#630/#634 in the pass after that, which also
**corrected** two entries #634 made false (the Network panel and Wi-Fi
sections below) rather than merely adding to them; and now folds in
#637/#639/#642 plus the further #644/#645/#662/#663/#664/#666/#668 burst that
landed right behind them (#660), which also **corrects** the #630 entry
below: #637 changed `modal::close_all`'s teardown call from `window.close()`
to `window.destroy()` after #630 had already merged, so the entry's citation
of `close()` went stale the moment #637 landed, not from later drift. A wrong
verification step is worse than a missing one, per #635. This is a living
checklist that gets refreshed periodically, not a dated snapshot of one merge
wave — #707's entries under "Plugins & launcher" were added by the PR that
shipped them rather than waiting for the next refresh pass, so the coverage
range above is a floor, not a ceiling.

## Idle & lock

- [ ] **(#463)** Idle-suspend gate now honors `idle` OR `sleep` inhibitors (not
      just `sleep`). Turn on "Keep awake" in the Power drawer (or start a video
      that holds an `idle` inhibitor — confirm with `systemd-inhibit --list`
      showing `idle`), then leave the seat idle past 600 s (or temporarily lower
      `SUSPEND_SECS`). Expect dim/lock to skip **and** no suspend, with
      `native idle action skipped — logind inhibitor held` in the logs at the
      suspend threshold. Release the inhibitor and confirm idle-suspend fires
      normally again.
- [x] **(#486)** Fullscreen auto-inhibit — hold a logind idle inhibitor while a
      window is genuinely fullscreen. _Checked: Annika held this PR open ~14.5h
      (opened 15:53, merged 06:24 the next day — well outside the same-wave
      batch the rest of #478/#480/#483/#485 merged in) rather than fast-tracking
      it, consistent with running the live-verify pass herself before merging.
      Re-verify only if fullscreen-idle behavior regresses._ Sub-items for
      reference if a re-check is ever needed:
  - Fullscreen a video (`mpv`/Firefox/`niri msg action fullscreen-window`) and
    confirm `systemd-inhibit --list` shows a trollshell `idle` inhibitor
    appearing on fullscreen-enter and disappearing on exit; screen doesn't dim
    at 240 s while fullscreen.
  - A **maximize-to-edges** window (`niri msg action maximize-column`) does
    **not** hold the inhibitor — only true fullscreen.
  - Multi-monitor: fullscreen on the non-focused output still holds; a
    fullscreen window scrolled to a non-active workspace does not.
  - Toggle "Keep awake when fullscreen" off in the Power panel while
    fullscreen — inhibitor drops immediately; the setting persists across a
    shell restart.
  - Hot-unplug the output that had the fullscreen window — inhibitor
    releases, not stuck on.
- [ ] **(#490)** Idle observer reconnect resilience post-cutover. Kill/restart
      the compositor connection path (e.g. restart the niri session) and
      confirm idle dim/lock/suspend still fire afterward, and sleep/wake still
      relocks.
- [ ] **(#520)** "Keep awake" toggle now also lives in the **Settings** drawer
      (previously only reachable via the Power drawer, which hides itself on
      desktops with no battery/backlight). Open Settings → flip **Keep awake**
      on → `systemd-inhibit --list` shows a `trollshell` `idle` block
      inhibitor; leave the seat idle past the dim/lock thresholds — it must
      not dim or lock; flip off → locks normally again.
- [ ] **(#535)** Keep-awake now **survives a shell restart** (previously the
      logind fd was process-owned and silently dropped on restart, despite a
      doc comment claiming otherwise). Settings → Keep awake **on** →
      `systemctl --user restart trollshell` → the switch comes back **on** and
      `systemd-inhibit --list` shows the `idle` inhibitor **again**
      (re-acquired). Flip off → `~/.config/trollshell/keep-awake.toml` becomes
      `enabled = false` and it stays off across a restart.
- [ ] **(#545)** A glanceable bar chip (`preferences-desktop-screensaver-symbolic`)
      appears next to the recording/settings chips **only** while keep-awake is
      engaged, and disappears the instant it's switched off. Click it → opens
      the Settings drawer on the Keep-awake toggle. Hover → tooltip shows
      "Also awake: …" when another app (mpv/Firefox) also holds an inhibitor.

## Plugin host & protocol

- [ ] **(#514)** Interactive consent overlay: run
      `hytte-infobroker auth --agent claude` with no grant → a focus-grabbing
      card appears on niri's
      **focused output** — _"⟨agent⟩ wants: ⟨scope⟩ from ⟨datasource⟩"_ with
      **Allow once / This session / Always / Deny**. **Always** persists to
      `grants.toml` (next `auth` is silent); **Deny** persists a standing no;
      **This session** authorizes `get departures` without a durable grant;
      **Allow once** authorizes exactly one `get` then the next is denied. Let
      a prompt sit **60 s** unanswered → resolves to Deny with a denied toast.
      Multi-monitor: the prompt lands on the focused output; hot-plug re-keys
      cleanly.
- [ ] **(#996)** Two shells racing for the socket. With the deployed unit up,
      start a dev shell (`cargo run -p trollshell`) — its journal must say
      _"another trollshell instance holds the plugin host lock; not taking the
      socket over"_ (or the older _"already has a live listener"_ line, if it
      won the lock but found a pre-#996 incumbent) and **never** _"plugin host
      listening"_. Then the harder one: `systemctl --user stop trollshell` and
      race two starts off one barrier. **Build first, then race the built
      binary** — two concurrent `cargo run`s cannot race, they serialise on
      cargo's package-cache/build-directory lock (the second prints _"Blocking
      waiting for file lock on package cache"_ and only starts once the first
      has already bound):
      `cargo build -p trollshell && (./target/debug/trollshell & ./target/debug/trollshell & wait)`
      — and check `ss -xl | grep -c plugin.sock` reports **1**, with
      `stat -c %i "$XDG_RUNTIME_DIR/trollshell/plugin.sock"` matching the
      process that logged "plugin host listening". The lock file lives beside
      it at `plugin.sock.lock` — 0-byte, `0600`, and never unlinked;
      `fuser`/`lsof` on it names the holder. Kill the winner: the lock releases
      with the process and the next start binds cleanly, with no stale-lock
      recovery step.
- [ ] **(#995)** A duplicate infobroker leaves the incumbent alone. With the
      deployed `hytte-plugin-infobroker` unit serving
      `$XDG_RUNTIME_DIR/hytte-infobroker.sock`, note
      `stat -c %i` on that path, then start a second one by hand
      (`cargo run -p hytte-plugin-infobroker`). Expect: **one** stand-down line
      from the duplicate — `already has a live broker listening` — logged once
      and **not** once per ≤5 s redial; the inode unchanged;
      `ss -xl | grep -c hytte-infobroker` still **1**, and
      `hytte-infobroker get departures` still working with a valid token
      throughout, and the **duplicate's own panel** carrying the one-line
      _"Not serving: another info broker already owns …"_ notice under its
      title (the chip is not silently empty). Then stop the incumbent, leaving
      the stale socket file, and restart it: it must reclaim the path rather
      than refuse.
- [ ] **(#995 / the session handover)** A restart _during an in-flight request_
      must not make the broker stand down against itself. Park a connection on
      the broker and send nothing (`nc -U "$XDG_RUNTIME_DIR/hytte-infobroker.sock"`,
      leave it open), then `systemctl --user restart trollshell` within the 5 s
      request timeout. Expect: **no** _"already has a live broker listening"_
      line, `stat -c %i` on the socket **unchanged** across the restart (the
      process keeps the listener it bound — it is not rebound per session),
      `hytte-infobroker get departures` answering straight through, and the
      panel's Revoke/Allow buttons still live afterwards. Before the fix this
      sequence left the broker permanently socket-less with an empty panel and
      a misleading duplicate warning, recoverable only by another restart.
- [ ] **(#544)** A plugin granted `Capability::RunCommand` emits
      `Effect::RunCommand` → the host spawns the argv and the plugin gets back
      an `EffectResult` with the exit status + captured stdout. A missing
      binary / non-zero exit / a command running past 10s all still return
      `ok: false` (no hang), with a warn in `RUST_LOG=trollshell=info`.
      `~/.local/state/trollshell/effects-audit.log` accrues one line per
      brokered/dropped effect and rotates to `.log.1` past the 256 KiB cap.
- [ ] **(#953)** The **detached** spawn mode (`Effect::RunCommand` with
      `detached: true`, i.e. `Effect::launch(...)`): click a sidebar/panel row
      on a plugin that launches a terminal that way → the terminal opens, and
      `systemctl --user list-units 'trollshell-launch-*'` shows a
      `trollshell-launch-<plugin>-<id>-<pid>-<seq>.service` transient **service**
      unit (not a scope) for it, under `trollshell-launch.slice`. Then
      `systemctl --user restart trollshell.service` → **the terminal survives**
      (the attached mode's child would die with the shell, and would already have
      been killed at 10 s). The plugin's `EffectResult` arrives **immediately**,
      with `ok: true` and `output` naming the unit — never the program's exit
      status, which the host deliberately never learns.
  - [ ] **Repeat ids (#953 M2).** Launch once, then — with the first terminal
        still open — restart the plugin (control-center Plugins tab stop/start,
        which resets its effect-id counter) and launch again. **Both terminals
        open**: the host, not the plugin, owns the unit name's uniqueness. Kill
        one by hand → `--collect` releases its unit.
  - [ ] **Cleanup (#953 L6).** `systemctl --user stop trollshell-launch.slice`
        closes every launched program at once; nothing else in the session goes
        with it.
  - [ ] **Audit (#953 M1).** `~/.local/state/trollshell/effects-audit.log` shows
        `effect=RunCommand(detached) decision=allowed id=<n>
unit=<the unit above> slice=trollshell-launch.slice` — distinct from the
        attached mode's `effect=RunCommand … id=<n>` with no unit, and the
        `unit=` is what lets you reconcile the log against
        `systemctl --user list-units 'trollshell-launch-*'`.
  - [ ] **Environment (#953 L5).** The launched terminal finds the display, and
        a launched program that speaks niri IPC finds `$NIRI_SOCKET`.
        Production leans on the session's
        `systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP`
        (`etc/niri/session.kdl`) — that is the load-bearing mechanism; the host
        additionally forwards its own `WAYLAND_DISPLAY`/`NIRI_SOCKET`/`DISPLAY`/
        `XDG_RUNTIME_DIR` with `--setenv=`, which is what makes a hand-started
        `cargo run -p trollshell` inside a nested compositor launch onto the
        _nested_ one rather than the outer session.
  - [ ] **Old shell, new plugin (#953 L2).** A plugin built against this SDK,
        run against a **pre-#953** shell, silently degrades: the shell doesn't
        know `detached`, runs the terminal attached, and kills it at 10 s with
        `ok: false`. Nothing warns — that is the accepted price of not bumping
        `VOCAB` (a bump would make the old shell refuse the plugin outright).
- [ ] **(#553)** Generic `Datasource` capability: `hytte-plugin-departures` /
      `hytte-plugin-weather` now answer `get departures` / `get weather`
      routed **through the running provider plugins** over the host protocol
      (not the broker's old internal fetch), under the same grant/token flow
      as `calendar`. Confirm both still resolve correctly through
      `hytte-infobroker get <name>` with a grant in place.
- [ ] **(#539)** Domain `StateKey`s (calendar / session-lock / now-playing) —
      see the Caw and Infobroker sections below for the per-consumer checks;
      the underlying push only fires when a plugin **both** subscribes the key
      **and** declares the gating capability — a subscribe-only plugin should
      be refused the push with a warn, not silently given the data.
- [ ] **(#565)** Now-playing re-seed on unpark: play a track, **close** the
      sidebar, change/stop the track, then **reopen** the sidebar — the
      audio-widget marquee should show the _current_ track immediately, not a
      stale one from before the sidebar closed.
- [ ] **(#966)** The list-card layout vocabulary, on a real card (the agents
      plugin, #963, is the motivating consumer). Three checks, one card:
  - [ ] **Row spacing.** A `Node::Row` built with `spacing: 6` (or
        `nodes::row(..).spacing(6)`) puts a visible gap between its children —
        the `⚙argus` collision that opened #966 is gone — and re-rendering the
        same row id with a different spacing **re-spaces in place**, no flicker,
        no rebuild.
  - [ ] **Dense grouped list.** Render a `boxed-list` `Node::ListBox` of ~12
        one-line rows in the sidebar, once with `dense: false` and once with
        `dense: true`. Dense is materially shorter and the rows are as tall as
        their text; the card's rounded frame and hairline separators survive.
        Toggle `dense` on a live re-render — the height changes without the
        rows blinking (they are reused, not rebuilt). Headless the same shape
        measures 12 rows at **251 px → 203 px**, and one row at
        **20 px → 16 px**, which is exactly its label
        (`hytte_ui::widget_tree::gtk_tests::a_dense_list_is_exactly_as_tall_as_its_rows_content`,
        which prints those numbers). Confirm the on-glass saving is bigger, not
        smaller: the CI theme charges 4 px a row where the real Adwaita row
        floor is much taller.
  - [ ] **A bounded card, inside a scrolling sidebar.** Wrap the list in
        `Node::Scrolled { max_height: 240 }`. Short list → the card is its
        content's height, **not** padded out to 240 px. Long list → the card
        stops at 240 px and scrolls **inside itself**, with everything below it
        (the pet card) still reachable.
    - [ ] **Which scroller wins.** Point at the card and wheel: the **card**
          scrolls. Keep wheeling past its end: the **sidebar** takes over
          (`GtkScrolledWindow`'s standard kinetic chaining — innermost first,
          handed outward at the ends). Point outside the card: the sidebar
          scrolls, always. There is no fight — the two hold separate
          adjustments, pinned headless by
          `a_bounded_card_inside_a_scrolling_surface_keeps_its_own_adjustment`;
          **this row is the gesture half**, which the headless suite cannot
          synthesize, so it is only ever verified here.
  - [ ] **Old shell, new plugin.** A plugin built against this SDK, run against
        a **pre-#966** shell, must still connect: `Row::spacing` and
        `ListBox::dense` are skipped as unknown fields (the card lays out as it
        did before), and `nodes::scrolled(..).build()` sees no
        `SCROLLED_VOCAB` in `Hello` and emits the bare child — an unbounded
        card, i.e. exactly the pre-#966 rendering. Nothing warns, nothing
        crash-loops. The **wrong** outcome to watch for is a 5 s reconnect loop
        in `journalctl --user -u trollshell-plugin-<id>`, which is what
        emitting `Node::Scrolled` unnegotiated would cause.
- [ ] **(#961)** The tooltip property on the list-card three
      (`Node::{Row, Text, Expander}`), on the same card #966 is verified with.
      Everything here is hover-only, which is exactly why it lives in this file:
      the headless suite reads `tooltip_text()` off widgets, it never puts a
      pointer on glass.
  - [ ] **An ellipsized `Text` explains itself.** Render a `Node::Text` with
        `ellipsize: true` and **no** tooltip, long enough to truncate — a status
        line in an agent row is the real case. Hover it: the popup shows the
        **full** text, not the `…`-clipped version. Re-render the same id with
        new text and hover again — the popup follows the text, it does not stay
        on the first string.
  - [ ] **Explicit wins, and short strings are legends.** Give that same node a
        `tooltip: Some(…)` — the popup shows _that_, not the text. Then render a
        **short** ellipsizing `Text` (nothing actually truncated) with no
        tooltip: it still hovers its own text. That is the deliberate
        simplification — the host does not consult the allocation — and this row
        is the confirmation that it reads as a legend rather than as a bug. A
        `Text` with `ellipsize: false` and no tooltip must show **nothing**.
  - [ ] **The weather card, which changed under you.** No new plugin needed:
        `hytte-plugin-weather`'s `text_line` is the tree's only `ellipsize: true`
        producer, so from #961 on the card's **location** and **condition** lines
        carry hover text equal to their own content. Open the weather card and
        hover each: the popup must show the full string (the point, when a long
        place name or condition is truncated) and must not be blank or stale
        after a refresh moves the condition. This is the "legend, not a bug"
        trade-off landing on a shipped card, so it is the row that says whether
        the call was right.
  - [ ] **A derived hover beats an expander's legend — by design.** Build a
        `Node::Expander` whose **header is an ellipsizing `Text`** and which
        also carries its own `tooltip`. Hover the header title: you get the
        **title** (the derived string), not the legend, because GTK answers from
        the deepest widget upward; the legend survives over the chevron and the
        header padding. Confirm that reads as reasonable rather than broken —
        and that the documented way out works: put the legend in an explicit
        `tooltip` on the header `Text` and it wins.
  - [ ] **Row legend, child override.** Put `tooltip` on a `Node::Row` whose
        children carry none: hovering anywhere along the row shows the row's
        string. Give **one** child (a `Label`/`Icon`) its own tooltip and hover
        it: the child's string wins there, the row's still shows either side of
        it.
  - [ ] **The expander header, not its body.** Give a `Node::Expander` a
        tooltip and **expand** it. Hovering the **header** shows it; hovering
        the revealed **body** shows nothing (or whatever the body's own children
        say). The wrong outcome — the one the host arms on the header button
        specifically to avoid — is the header's legend following the pointer
        down over every body row.
  - [ ] **Old shell, new plugin.** A plugin built against this SDK, run against
        a **pre-#961** shell, must still connect and render: all three
        `tooltip`s are skipped as unknown fields, and the card looks exactly as
        it did before — no hover text, no warning, no 5 s reconnect loop in
        `journalctl --user -u trollshell-plugin-<id>`.
- [ ] **(#1045)** The `OpenUri` effect — a plugin opening a link **without**
      `Capability::RunCommand`. Nothing in CI can see this: it ends in the
      desktop's default handler, and the hermetic tests inject a stub launcher
      precisely so `cargo test` never starts a browser.
  - [ ] **The happy path.** Give a plugin `capabilities: vec![Capability::OpenUri]`
        (and nothing else that can launch) and have it emit
        `Effect::open_uri(id, "https://pr1ma.darkest.space/")` from `update` on a
        click. The **browser opens** on the focused output, and
        `journalctl --user -u trollshell` logs
        `plugin effect: OpenUri … scheme=https uri=https://pr1ma.darkest.space/`
        at info. The audit log (`$XDG_STATE_HOME/trollshell/effects-audit.log`)
        gains a matching
        `effect=OpenUri decision=allowed id=<id> uri=https://pr1ma.darkest.space/`
        line — with **no** `unit=` (nothing was handed to systemd; that field
        belongs to a detached `RunCommand`).
  - [ ] **A refused scheme is toastable, not silent.** Same plugin, emit
        `Effect::open_uri(id, "mailto:annika@hannig.cc")`. **Nothing launches**;
        the journal warns `plugin effect: OpenUri refused` carrying
        `reason=refused: scheme "mailto" is not openable`; and the plugin receives
        `Input::EffectResult { ok: false, output: Some(reason) }` it can render
        (the point of the round-trip — confirm the plugin's own toast/label
        actually shows it, not just that the host logged it). Repeat with
        `ssh://box.example/` and a bare `pr1ma.darkest.space/agents` (no
        scheme).
  - [ ] **`file:` really opens.** `Effect::open_uri(id, "file:///…/shot.png")`
        opens the image viewer — the same handler resolution the shell's own
        screenshot toast uses.
  - [ ] **An uppercase scheme really resolves.** `check_uri` accepts
        `HTTPS://pr1ma.darkest.space/` because RFC 3986 says schemes are
        case-insensitive, but whether **GLib** then finds a handler for it is a
        runtime question no unit test can answer. Emit one: the browser must
        open exactly as for the lowercase form. If it does not, the allow-list
        is accepting something the desktop cannot resolve and the case-folding
        belongs in the host, not just in the comparison.
  - [ ] **A hung launch is bounded, not silently open-ended** (the reason the
        launch is asynchronous — review F1 on PR #1049 — and, since #1060, the
        reason it is also timed out). No code path reaches the real
        `launch_default_for_uri_async` in CI — every hermetic test injects its
        own launcher stub, by design — so **this row is the only place
        anywhere that observes the production wiring**; a build that silently
        reverted to the pre-#1060 `Cancellable::NONE` (no timer, no bound)
        would pass every other check in the repo and only be caught here.
        Point one at a hung mount:
        `sudo mount -t nfs 10.0.0.254:/nowhere /mnt/hang -o hard,timeo=600` on
        an address that black-holes, then emit
        `Effect::open_uri(id, "file:///mnt/hang/x.png")`. The bar clock must
        keep ticking, the drawer must still open, and other plugins must keep
        rendering, for as long as that launch is outstanding — GLib's
        _synchronous_ entry point does content-type I/O on the URI and would
        have frozen all of it. Within **~10 s** (`OPEN_URI_TIMEOUT`) — not
        "whenever the mount gives up", which can be minutes — the plugin must
        receive an `EffectResult` with `ok: false` and
        `output: Some("launch failed: launch timed out")`, and the journal
        must warn `plugin effect: OpenUri failed to launch a handler`
        carrying `error=launch timed out`. If the result never arrives, or
        only arrives once the mount itself times out, the #1060 bound has
        regressed. (`umount -f -l /mnt/hang` after.)
  - [ ] **The capability is load-bearing.** Remove `Capability::OpenUri` from
        the plugin's manifest, keep the effect, restart it: the click does
        nothing, and the journal warns
        `plugin effect requires a capability it didn't declare; dropped`, with
        a matching `decision=dropped(ungranted-capability)` audit line. Then give it
        `Capability::RunCommand` **instead** — still dropped, since the two caps
        do not substitute for each other.
  - [ ] **Old shell, new plugin** (the compat claim `OPEN_URI_VOCAB`'s docs
        make). Run a plugin built against this SDK and declaring
        `Capability::OpenUri` against a **pre-#1045** shell: it must be dropped
        at the handshake with a `plugin handshake read failed` warn naming the
        undecodable variant — _not_ mount a card that silently ignores clicks.
        A plugin rebuilt on this SDK that does **not** declare the cap must
        still connect and render normally against that same old shell (this is
        what `VOCAB_UNCONDITIONAL` staying at 1 buys, and it is the half worth
        checking).
  - [ ] **The named residual, seen once** (review F2 on PR #1049). Same old
        shell; this time run a plugin that **emits** `Effect::open_uri` while
        declaring only `Capability::Notify`. It must `Register` successfully and
        mount — and then, on the first click that emits the effect, the old
        shell logs a decode failure and the SDK redials on its 5 s backoff:
        the #437 crash-loop. Confirm it looks exactly like that, because this is
        the failure mode `VOCAB_UNCONDITIONAL` was deliberately left unable to
        catch, and the docs claim it is a plugin bug rather than a wire hazard.
- [ ] _(dormant — #555)_ The wire-vocabulary generation counter (`VOCAB`) is
      armed but untested against a real newer-vocab plugin (this PR appended
      no wire variant, so `VOCAB` stays at 1 and nothing exercises the reject
      path yet). Once a future PR appends a `Node`/`Effect`/`StateKey` variant
      and bumps `VOCAB`, confirm an old plugin built against the prior
      generation gets rejected at `Register` with a "plugin … built against a
      newer wire vocabulary … update the shell" warn, instead of crash-looping
      silently.

## Plugins & launcher

- [ ] **(#489)** Plugins now launch via `systemd-run --user` transient units
      from a declarative enabled-state instead of pre-installed static units.
      `systemctl --user list-units 'trollshell-plugin-*'` at shell start should
      show only the enabled set as transient units; stop/start from the
      control-center Plugins tab should work; a disabled plugin should stay
      down across a shell restart.
- [ ] **(#707)** Config recycle end to end — the "how a config change reaches a
      running plugin" chain (#419 → #695 → #707), which nothing in CI can
      exercise because it needs a live user manager and a live session bus.
      With the shell running and a declared plugin up, change one visible knob:
      `programs.trollshell.plugins.pet.env.PET_NAME = "nisse";` →
      `home-manager switch`. Without touching anything else: `cat ~/.config/trollshell/plugins.json` shows the new value; the journal
      (`journalctl --user -u trollshell -f`) logs "declared spec changed;
      restarting"; `systemctl --user show -p ExecMainStartTimestamp trollshell-plugin-pet` shows a fresh start; and `tr '\0' '\n' < /proc/$(systemctl --user show -p MainPID --value trollshell-plugin-pet)/environ | grep PET_NAME` shows the new value. The
      card should re-render with it. Then verify the manual path works with the
      shell running but no switch: `busctl --user call mov.vibec0re.trollshell.Control /mov/vibec0re/trollshell/Control mov.vibec0re.trollshell.Control ReloadPlugins` is a clean no-op when
      nothing changed.
- [ ] **(#707)** Session target — the plugin units now bind `PartOf=` the same
      target the shell does, instead of a hardcoded `graphical-session.target`.
      With `programs.trollshell.systemd.target = "niri-session.target";` (what
      `etc/` ships), switch and confirm `systemctl --user show -p PartOf trollshell-plugin-<id>` reports `niri-session.target` — matching
      `systemctl --user show -p PartOf trollshell`. Then confirm the recycle:
      the **first** reconcile after upgrading a session that was already using a
      non-default target should restart each plugin exactly once (the old units
      digest without the target), and reconciles after that should be no-ops. On
      a session that never set `systemd.target`, the upgrade must restart
      **nothing** — the default canonicalizes as absent in the fingerprint on
      purpose. Finally `systemctl --user stop niri-session.target` should take
      the plugins down with the shell rather than leaving them orphaned.
- [ ] **(#707)** Plugin control errors — `StartPlugin` / `StopPlugin` /
      `SetPluginEnabled` now return a D-Bus error instead of an empty reply on
      failure. Start a plugin that is already running:
      `busctl --user call mov.vibec0re.trollshell.Control /mov/vibec0re/trollshell/Control mov.vibec0re.trollshell.Control StartPlugin s pet` should now print an error naming the plugin (it printed
      nothing before). A successful call must still print nothing — the wire
      shape is unchanged for the success path, and the control-center's Plugins
      tab must keep working exactly as it did.
- [ ] **(#495)** Socket single-instancing — run the deployed shell, then
      `cargo run -p trollshell` beside it: the dev instance should log
      "not taking it over" and the running shell should keep its plugins
      (rather than the dev instance stealing the socket).
- [ ] **(#495)** Duplicate plugin ids — launch a plugin twice with the same id
      (dev binary + systemd unit both up). The second registration should log
      "rejecting the duplicate"; the bar/panel card should not flap between
      the two.
- [ ] **(#495)** Capability enforcement — get a plugin to emit an effect it
      didn't declare a capability for. The host should log
      "requires a capability it didn't declare; dropped" and no
      drawer/OSD/toast should fire.
- [ ] **(#1019)** Niri layouts — nothing about this plugin can be verified
      without a live niri session, so every leg is live-only. **The chip:** on a
      workspace with three tiled columns, click each of the three
      buttons on the bar (`equal` / `golden` / `split`, left to right) and check
      the columns land on the **stated widths**, not merely that they moved —
      `equal` a third of the screen each, `golden` a first column at **75 %** of
      the screen with **25 %** ones after it, `split` half the screen each (so
      the third scrolls off). Measuring the width is the point: `SetProportion` is a
      percentage, and the pre-review build sent fractions, which niri clamped to
      each window's **minimum width** — every button "resized the columns" while
      doing the same wrong thing. If all three snap columns to a thin sliver,
      that regression is back. Each button should show an **Adwaita symbolic
      icon** — `view-grid-symbolic` (equal), `sidebar-show-right-symbolic`
      (golden: a wide area with a narrow right panel), `view-dual-symbolic`
      (split) — and **not** an `image-missing` box, and not the preem LED panels
      #1026 shipped (round 2 replaced those). Hovering a glyph should show its
      legend, and the golden one should read "first column 75 %, the rest 25 %
      on screens 2560 px wide or more; 61.8 % / 38.2 % (the golden cut)
      narrower" (#1052 — the tooltip names both pairs and the breakpoint since
      it can't dial niri to say which one applies to the screen it's hovered
      on).
      (**`equal` and `split` are the same plan at exactly two columns** — halves
      either way; they differ from a third column on. Deliberate, but say so if
      two identical-looking buttons read badly on glass.)
      **Stacked columns count once:** stack three windows into one
      column beside a single other window and click `split` — you should get
      **two** half-width columns, not four quarter-width ones.
      **Show/hide (#1019 round 2):** on a workspace with **one** window the chip
      should not be on the bar at all; open a second window and it should appear
      within a frame or two; close back to one and it should go again. The
      _window_ count is what matters, not the column count — two windows
      **stacked in one column** must show the chip, and a floating window counts
      too. Switch to a workspace with two windows and back, without opening
      anything: the chip should follow the switch. Do the same on a **second
      monitor** — changing workspace on the _unfocused_ output must not move the
      chip. Then `systemctl --user restart niri` (or restart the compositor how
      you normally would) and confirm the chip comes back without restarting the
      plugin, and does not blink off and on if you land on the same workspace.
      **Restart the shell, not the plugin** two or three times
      (`systemctl --user restart trollshell`) and check the plugin's unit:
      `systemctl --user status trollshell-plugin-niri-layouts` should still show
      one process and, if you look, one `niri-layouts-watch` thread — not one per
      restart (that leak is what #1038's review found). The chip should be back
      and correct after each restart. With niri unreachable
      (`env -u NIRI_SOCKET`, or the plugin started before niri) the journal
      should carry exactly **one** `WARNING: cannot watch niri …` line per
      outage, saying the chip stays hidden — that line is the only signal that
      an absent niri, rather than a one-window workspace, is why the chip is
      gone.
      Known cosmetic residual to look for and report: while hidden, the plugin
      renders an empty tree but the shell still draws its own `.ts-plugin-chip`
      pill, so a few pixels of translucent rounded background may remain where
      the chip was. If that is visible enough to bother you, say so — closing it
      is a host-side change (`trollshell/src/plugins/region.rs`), not a plugin
      one. **The CLI:**
      from a terminal in the session, `hytte-plugin-niri-layouts apply golden`
      should do the same thing and exit `0`; on an empty workspace it should
      print "no tiled columns" and still exit `0`; with `NIRI_SOCKET` unset
      (`env -u NIRI_SOCKET hytte-plugin-niri-layouts apply equal`) it should
      print niri's own error and exit non-zero. **The bind:** merge the three
      `Mod+Alt+{E,G,S}` binds from `etc/niri/binds.kdl` and confirm they work
      with the **shell stopped** (`systemctl --user stop trollshell`) — that is
      the whole point of the standalone hat. Finally, make niri refuse a
      request (an old niri without `--id` on `set-window-width`) and confirm the
      chip raises a toast carrying niri's own error text.
- [ ] **(#1039)** Empty-tree chip hide — a plugin whose rendered root is a
      childless container (`Row`/`Box`/`ListBox`) must not leave a nub in the
      bar. With #1038's `hytte-plugin-niri-layouts` chip (which renders an
      empty `Row` under its root id whenever the focused workspace has one
      window or fewer) enabled, focus a workspace with exactly **one** window:
      the chip should disappear from the bar entirely — no pill, no gap, nothing
      to hover or click — not merely lose its buttons. Add a second window to
      the workspace and the chip should reappear with its three buttons. The
      host-side behavior is unit-tested under `xvfb-run`
      (`trollshell/src/plugins/region.rs`'s `gtk_tests`); this entry only
      confirms it reads right on real glass, since niri-layouts' own empty-root
      trigger (#1038) has no niri session to test against in CI.
- [ ] **(#1052)** Golden's adaptive pair — needs two outputs of different
      logical widths straddling 2560 px (an ultrawide/external monitor plus a
      laptop panel works; `kanshi`/`niri msg output <name> scale <n>` can also
      push a HiDPI panel's _logical_ width across the line without new
      hardware). On the **wide** output (>= 2560 logical px) click `golden` on
      a three-column workspace and confirm the same 75 %/25 %/25 % split
      #1019 already shipped. Focus the **narrow** output's workspace instead
      and click `golden` there: the columns should land at **61.8 %/38.2
      %/38.2 %**, not 75/25 — this is the whole point, a narrow column that is
      still usable rather than a sliver. The decision follows the **focused
      workspace's own output**, not whichever monitor the shell started on:
      switch focus between the two outputs and click `golden` on each without
      restarting anything, and confirm each pick tracks its own screen.
      Unplug the external monitor (or otherwise leave only a headless/no-info
      output) and click `golden` again: it should fall back to the wide pair
      (75/25) and the journal should carry one `golden: no logical width …`
      debug line explaining why, not a crash or a silent 61.8/38.2. The CLI
      hat picks up the same rule: `hytte-plugin-niri-layouts apply golden` run
      from a terminal on each output should match its chip's split exactly.
      **Known boundary, flag rather than fix (#1056 review, MED-1):** a
      screen reporting **exactly** 2560 logical px — a 1440p monitor at 1x
      scale, or a 4K panel read back at 1.5x — lands on the wide side today
      (`>= 2560` → 75/25), not the golden cut. If that reads wrong on real
      glass, say so on #1052 rather than changing `GOLDEN_BREAKPOINT`
      yourself — Annika hasn't picked a side of that boundary yet.
- [ ] **(#1050)** Per-screen chip visibility and per-screen clicks — needs two
      monitors for legs 1–7 and 9; on one screen those are vacuously true, and
      leg 8 is the one-screen case (which is the everyday one).
      **What ships, and what you need.** #1068 put the two fields on the wire
      (`Render.hidden_on`, `Event.output`) and taught the host to act on them;
      the plugin round taught `hytte-plugin-niri-layouts` to emit them. So this
      is walkable with the **shipped** plugin set — enable `niri-layouts` and
      no scratch plugin is needed (the last leg is the one exception).
      `niri msg outputs` prints the connector names the legs below refer to.
      **Regression legs (the host arm, do these first):** with the usual plugin
      set enabled on two monitors, every _other_ chip and sidebar card should
      still appear on **both** screens exactly as before, clicks should still
      work on both, and the #1039 empty-tree hide should still hide a chip on
      both. Any chip except the layouts one vanishing from one screen is a bug.
      **1. Per-screen visibility.** Put ≥ 2 windows on the active workspace of
      screen A and ≤ 1 on screen B's. The layouts chip must appear on **A
      only**. Before the plugin round both screens followed whichever screen
      held keyboard focus — that is the symptom Annika reported.
      **2. The gap, not just the pill.** On the screen where the chip is
      hidden, look at the _spacing between its bar neighbours_, not only for
      the absent pill: the group must close up with no sliver left behind.
      **3. A click acts on the screen it came from.** With the chip up on both
      screens, click one of the three glyphs on **screen B's** chip. B's
      workspace must re-lay out and A's must not move — **whichever screen
      holds keyboard focus**. Focus A and click B: that is the discriminator,
      and it is the whole of `Event.output`.
      **4. Per-screen workspace switching.** Change workspace on B alone. Only
      B's chip may appear or disappear; A's must not flicker.
      **5. The keybind is unchanged.** `hytte-plugin-niri-layouts apply equal`
      from a niri bind (or a terminal) still targets the **focused** output — a
      keybind has no screen to have been clicked on. Run it with focus on each
      screen in turn and confirm each lays out its own.
      **6. Unplug/replug.** Unplug a monitor while the chip is up, then replug
      it: no blink, no stale chip, and the chip settles on whichever screens
      qualify once the bars rebuild. A sub-second wrong-screen flash right
      after a hot-plug is expected (#1083 review INFO-4) — GDK can add the
      monitor before niri's workspace list reaches the plugin. It must
      self-correct on the next verdict, not persist.
      **7. Both screens below the threshold.** Drop _both_ screens to ≤ 1
      window. The pill **and** its bar-group gap must vanish on **both**. This
      is the collapsed-tree branch — a different code path from legs 1–2, and
      nothing else on this list walks it.
      **8. The single-screen everyday case.** With one monitor (or one
      disabled), the chip must behave exactly as it did before #1050: appear at
      two windows, hide at one, click to lay out. A single-screen frame is
      byte-identical on the wire (nothing is ever below threshold _and_ shown
      elsewhere), so this is the worst thing to regress and no two-screen leg
      covers it.
      **9. One journal read.** With `RUST_LOG=trollshell=debug` and the chip
      hidden on B the journal must carry exactly one
      `plugin card hidden on this output (#1050)` line naming B, and **zero**
      `plugin hidden_on names outputs that are not attached` lines. That
      second half is five seconds of work and the only empirical check of the
      assumption below.
      **Optional, if the two screens differ in logical width (#1052):** click
      **golden** on the _narrower_ screen while the wider one holds keyboard
      focus, and confirm the pair comes from the **clicked** screen (61.8/38.2
      below 2560 logical px, 75/25 at or above). It is the one leg where
      targeting the wrong screen produces visibly different _numbers_ rather
      than merely the wrong monitor moving.
      **Load-bearing assumption, uncited (#1083 review INFO-1):** this whole
      feature rests on niri's `Workspace.output` string and GDK's
      `monitor.connector()` string being the same namespace, and the host
      compares them byte-exactly with no normalisation. niri-ipc documents the
      field only as the "Name of the output" and **guarantees no such
      equality** — it holds because niri names the `wl_output` after the DRM
      connector and GDK4-Wayland reports that. If the two ever diverged the
      only symptom would be _the chip never hides_, which is why leg 9 is on
      this list.
      **Hidden animation costs nothing (#1068 review, MEDIUM-1).** This one
      still needs a scratch plugin, because it wants an _animating_ chip hidden
      by `hidden_on` and the layouts chip is three static icons: take
      `hytte-plugin-preem-demo` (marquee) or `hytte-plugin-caw`, add
      `.hidden_on(["<one of your connectors>"])` to its `view()`, and confirm
      the hidden screen's frame clock is not kept armed on its account — with
      `RUST_LOG=trollshell=debug` there should be no steady stream of preem
      repaint activity for that plugin id while it is hidden there, and a
      wakeup count on that output over a few seconds (the `perf`/wakeup method
      #883/#926 used for the sidebar-closed and empty-region cases) should read
      the same as with the plugin absent entirely, not the ~30 Hz/monitor a
      still-armed tick callback would cost. Re-show it (drop the `hidden_on`
      call) and confirm the animation resumes on that screen.

## Infobroker

- [ ] **(#493)** Chip + panel rendering: confirm the infobroker bar chip
      (shield; warning triangle + badge when an agent is knocking) and its
      drawer panel — grants, pending knocks, datasource status, live
      sessions, audit trail — render correctly in a real session (only
      unit-tested via the node tree so far, never GTK-reconciled live).
- [ ] **(#493)** Toast delivery: trigger a denied `auth` from an unauthorized
      agent (`hytte-infobroker auth --agent <name>` with no grant) and confirm
      the `Effect::Notify` toast actually lands ("agent X requested departures —
      denied").
- [ ] **(#493)** Live datasource fetch: `hytte-infobroker auth --agent claude` →
      `export HYTTE_INFOBROKER_TOKEN=…` → `hytte-infobroker get departures`
      against a real, configured `places.toml` — confirm the HAFAS round-trip and
      scoped JSON response.
- [ ] **(#493)** Icon check: confirm `emblem-shared-symbolic` /
      `dialog-warning-symbolic` render as real icons in the live Adwaita
      theme, not `image-missing`.
- [ ] **(#525)** CLI renamed `infobroker` → `hytte-infobroker`, and the panel
      was rebuilt (grouped sections, two-line rows, hairline dividers,
      replacing concatenated text like `fnorddeparturesalways`) with a neutral
      `emblem-shared-symbolic` chip icon. Confirm the panel renders legibly
      with no concatenated fields — same live session as the #493 checks
      above, now against the rebuilt panel.
- [ ] **(#539)** With a `calendar` grant/token, `hytte-infobroker get calendar`
      returns the shell's upcoming events. Open the infobroker panel, then
      `loginctl lock-session` → the panel blanks to "Hidden while the session
      is locked"; `unlock-session` → the content returns.
- [ ] **(#553)** `hytte-infobroker get departures` / `get weather` now route
      through the live `hytte-plugin-departures` / `hytte-plugin-weather`
      provider plugins instead of the broker's own internal fetch — confirm
      both still resolve under the normal grant flow (see also "Plugin host &
      protocol" above).

## Claude bridge (`hytte-claude-bridge`)

- [ ] **(#666)** New standalone binary + systemd unit: a keyless,
      same-uid-socket (`$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock`, `0600`
      since #993) OpenAI-compatible shim over headless
      `claude`, so an LLM-backed plugin (`pet`, `caw`) can ride a Claude Code
      subscription instead of a paid API key with a one-line `Environment=`
      change on _its own_ unit. This is a brand-new service nobody has run
      live yet, so every check below is genuinely first-run, not a
      regression check.
  1. **Starts and refuses correctly.** With `plugins.claude-bridge` enabled (or a hand-written `plugins.json` entry), restart it via the control-center's Plugins tab, or the `Control.ReloadPlugins` D-Bus call (see `etc/systemd/user/README.md`'s "How a config change reaches a running plugin" section), then check `journalctl --user -u trollshell-plugin-claude-bridge` — expect `hytte-claude-bridge listening (no inbound auth; same-uid socket, 0600)`. Then run it by hand with `ANTHROPIC_API_KEY=x hytte-claude-bridge` — expect a refusal naming the offending variable, exit 1 (the billing guard that stops metered credits leaking in from an inherited env).
  2. **A round trip:**
     `curl -s --unix-socket "$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock" http://localhost/v1/chat/completions -H 'content-type: application/json' -d '{"messages":[{"role":"system","content":"you are a cat"},{"role":"user","content":"say hi"}]}'`
     — expect a `chat.completion` body with text in
     `choices[0].message.content`, inside 8s.
  3. **(#993) Not reachable by a second local account.** This replaced the old
     "not reachable off-box on the LAN" step, because the LAN direction was
     never the hole. The bridge validates no bearer token at all (it
     structurally can't — see the PR body) and it spends the owner's Claude
     subscription, so whoever can reach the socket is authorized. Three
     observations, in order.
     First the modes:
     `stat -c '%a %U' "$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock" "$XDG_RUNTIME_DIR/trollshell"`
     → `600 <you>` and `700 <you>`.
     Then, as a **second local user** (`sudo -u nobody -- …`, or any other
     account on the box):
     `curl -sv --unix-socket /run/user/<your-uid>/trollshell/claude-bridge.sock http://localhost/v1/chat/completions -d '{"messages":[{"role":"user","content":"hi"}]}'`
     → must fail with **permission denied** on `connect()`, not a
     `chat.completion`. Before #993 the equivalent
     `curl 127.0.0.1:8787/…` from that account returned a completion billed
     to you.
     Finally `ss -ltnp | grep 8787` → **nothing**: no TCP port is bound any
     more, on loopback or anywhere else.
     One caveat worth stating out loud for the #947/#949 hive agents: a
     container sharing the host network namespace now sees no endpoint at all,
     but one that also bind-mounts your `$XDG_RUNTIME_DIR` is the same uid by
     construction and is _inside_ the boundary. Check what your cage actually
     mounts before concluding either way.
  4. **The delta rule for real:** send turn 1, then turn 2 carrying
     `[system, user, assistant(reply), user]`. Check
     `~/.claude/projects/<slug>/` — there should be **one** `.jsonl` with a
     `customTitle` starting `hytte-bridge-…`, and turn 2 should have appended
     only the one new user message, not replayed the whole transcript. A
     hermetic test only approximates this — it's the one behaviour that
     needs a real `claude` session to see directly.
  5. **The riskiest live assumption, flagged by the PR itself:** the
     resume-then-create fallback depends on `hive-claude` classifying a
     failed `--resume` as the typed `SessionNotFound`. If that marker ever
     drifts, **every first turn 502s** instead of creating a session. The
     journal tell is `claude exited …` with `does not match any session
title` in the stderr tail — worth a deliberate look on first run, since
     a silent drift here would read as "the bridge is broken" rather than
     naming the actual cause.
  6. **pet end-to-end:** set
     `PET_LLM_URL='unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock'`
     (single-quoted — the variable is expanded by the plugin, not by nix or
     your shell; `programs.trollshell.claudeBridge.baseUrl` renders exactly
     this) and
     `OPENROUTER_API_KEY=local-bridge` on the `pet` plugin's declared env
     (`programs.trollshell.plugins.pet.env`, or the matching entry in
     `~/.config/trollshell/plugins.json` plus `Control.ReloadPlugins`) and
     poke the cat. Confirm via
     `journalctl` that the pet never sends the real OpenRouter key — the
     dummy env var winning is the whole point (`load_key_from` checks the
     `OPENROUTER_API_KEY` env override before
     `~/.config/trollshell/openrouter.key`).
  7. **caw's briefing** composes calendar + weather + departures into a
     larger prompt than anything measured against the bridge. If it exceeds
     the 8s budget it 504s and caw falls back to canned output — that's the
     designed outcome, not a bug. If it happens routinely on a real briefing,
     that's a tuning question (raise `CLAUDE_BRIDGE_TIMEOUT_SECS`, staying
     under 10, or point caw at a faster model via `CLAUDE_BRIDGE_MODEL`), not
     a regression to file.

- [ ] **(#855)** A session rotation now survives a bridge restart. Nothing in
      CI can drive a real overflow (no `claude` binary, and a genuine one takes
      ~10³ turns), so this is the half only a live session can show. After the
      journal has logged a rotation
      (`claude session is past the context window: retiring it`), confirm
      `~/.local/state/hytte-claude-bridge/retired-sessions.json` exists and
      names the `-g1` title. Then restart the unit and check the journal for
      `restored the retired-session map` at startup. The next turn of that
      conversation must answer normally — **not** 413 and rotate again, which
      is exactly the failed turn (a canned fallback line on glass) this
      removes. Deleting the file by hand must also be harmless: the bridge
      starts clean and costs at most that one turn back.

## Caw (morning briefing)

- [ ] **(#483)** Poke caw and confirm the taller 8-row briefing bubble renders
      correctly under the 128 px face at real sidebar width.
- [ ] **(#483)** Confirm the briefing mirrors as a toast through the shell's
      own notification daemon (`Effect::Notify`, `Capability::Notify`).
- [ ] **(#483)** End-to-end trigger: let a real 07:00 fire, or set
      `CAW_BRIEFING_TIME=<now+1min>` and wait — check both with a configured
      LLM (`CAW_LLM_URL` or `openrouter.key` + `CAW_LLM_MODEL`) and without
      (deterministic template fallback).
- [ ] **(#483)** Confirm the open-meteo (weather) and HAFAS (departures)
      ingredients resolve correctly against Annika's actual `places.toml`
      (first `[[place]]`'s `lat`/`lon`/`station`).
- [ ] **(#539)** The briefing now fires on **first unlock** rather than a
      suspend-window stand-in, and includes today's calendar events. With the
      screen **locked** at the briefing hour, no news fires; **unlock** →
      the morning brief caws once/day, calendar events included. Already
      unlocked at the hour → fires on the next ~2 s heartbeat.

## Calendar & tasks (EDS)

- [ ] **(#485)** Boot race: mask/stop `evolution-data-server`, start
      trollshell, then start EDS. Calendar/tasks should populate within the
      backoff window — log: "EDS worker init succeeded after retries".
- [ ] **(#485)** EDS restart mid-session — restart the EDS units (or kill the
      factory process directly):

  ```sh
  systemctl --user restart evolution-source-registry evolution-calendar-factory
  ```

  Events/tasks should recover within one poll (60 s calendar / next op or
  ≤5 min tasks), log: "cached EDS client failed; reconnecting"; task push
  notifications should resume after the following scan re-watches.

- [ ] **(#485)** Remove a calendar/task list at runtime — its cached client
      handles should drop (no stale rows lingering, no repeated error spam).
- [ ] **(#524)** TZID double-shift fix: an event authored at a specific local
      time (e.g. **12:30**) in a `TZID=Europe/Berlin` (or other zoned)
      calendar should now show at that same local time in the sidebar
      Upcoming list — not shifted by the zone offset a second time (was
      showing +2h in CEST).

## Displays / compositor geometry & overlays

- [ ] **(#475)** Switch a monitor's resolution via a kanshi profile change
      (mode switch, no unplug/replug) and confirm: the frame overlay resizes
      to the new output, its fullscreen-hide (edge-span) threshold still
      triggers correctly, and a bar-chip drawer opens centered under its chip
      at the new resolution (not offset by the stale width).
- [ ] **(#479)** Bump `gtk-font-name`'s point size (or the bar font-size
      config from #135) live and confirm the frame overlay's cutout top edge
      tracks the bar's actual new height, with no seam or overlap (the fix
      replaced a hardcoded 44px bar-height constant with a live read).
- [ ] **(#462)** Open a tray/tasks menu, then unplug/replug a monitor while it
      is open. Confirm clicks still land afterward — i.e. no orphan
      invisible click-eating popup catcher left behind from the mid-show
      teardown.
- [ ] **(#480)** Popup dismiss-catcher fold-in: confirm the catcher now
      covers **every** connected monitor (a click on a _different_ monitor
      than the one the popup opened on still dismisses it), that scrolling
      on the covered output dismisses rather than being silently swallowed,
      and that blur/input-region timing under niri looks right for
      catcher surfaces.
- [ ] **(#496)** Focused-output routing consolidation (three duplicated
      resolvers → one shared component). Confirm volume/mic/brightness/
      battery OSD toasts, notification toasts, and the
      `open-page`/`power-menu`/`toggle-sidebar` niri keybinds all still land
      on whichever monitor niri currently has focused, with the same
      fallback-to-first-mounted-surface behavior on startup / when the
      focused output is unmounted. Should be pixel-identical to pre-refactor
      behavior — this is a regression check, not a new-behavior check.
- [ ] **(#511)** Open **gnome-control-center → Displays**, change a
      mode/scale/arrangement, hit **Apply**, then **Keep Changes**
      (`method=persistent`): the layout stays live _and_ a trollshell toast
      appears — _"Display configuration applied … save it as a kanshi profile
      (etc/kanshi/)."_ Let a different Apply's countdown **expire** (or
      **Revert**) instead — no persistence toast fires. Confirm no file under
      `etc/kanshi/` (or niri `config.kdl`) was ever written by the shell.
- [ ] **(#604)** Drawer centering was solving against the monitor's full
      extent instead of the bar surface's own live extent, so with the
      sidebar **expanded** (reserving its exclusive zone) the drawer opened
      offset from the chip that triggered it. With the sidebar **collapsed**,
      open a drawer from a bar chip — it still centers under its chip (regression
      check). **Expand** the sidebar, open a drawer from a bar chip — it now
      centers correctly (this is the fix). Switch a kanshi mode/resolution
      (#442) while a drawer is open or between opens — still centers
      afterward. Worth an eyeball on more than just the Top bar edge if
      convenient — the math is shared across all four edges but only Top gets
      everyday exercise.
- [ ] **(#624)** OSD silenced by a hot-plug: open a drawer on any monitor and
      leave it open, then trigger a `monitors_changed` cycle (a kanshi
      profile switch, or a physical unplug/replug), then press the
      volume/brightness/mic keys. The OSD card must appear as normal and the
      bar's corner must round off again — before this fix, that output's OSD
      stayed silent for the rest of the session and the bar kept its
      squared-off `drawer-open` seam corner with no drawer attached. A
      connector-less/virtual output is worth trying too, but only for the
      bar-corner half of this check (`main.rs`'s `drawer-open` class bind is
      unconditional) — `osd::install` skips any monitor with no connector
      name by design, so no OSD card is ever expected to appear there, on
      either side of this fix.
- [ ] **(#737)** The sidebar reserves the width it actually paints rather
      than a flat 320. `exclusive_zone` was committed as the `SIDEBAR_WIDTH`
      constant while the surface itself is `em`-sized (`.ts-sidebar`'s padding
      and every child card grow with the font) and `set_size_request` sets a
      _minimum_, not a width — so at a larger font the sidebar painted wider
      than it reserved and tiled windows ran underneath it. **Raise
      `gtk-font-name`'s point size well up**, open the sidebar, and confirm
      tiled windows are pushed fully clear with neither overlap nor a dead
      gap; then lower the font again and confirm the reserved strip shrinks
      back to match instead of leaving a gap.
- [ ] **(#965)** The sidebar **scrolls**. Found by @kaesaecracker on #963: a
      hive of 12 agents grew the agents card past the bottom of the screen and
      every card below it (the pet) was cut off with no way to reach it, because
      the card stack sat in no scroller at all. Run the agents plugin with a
      hive big enough to overflow (or shrink the output's height), open the
      sidebar, and confirm the stack scrolls — wheel, drag, and keyboard — with
      the pet card reachable at the bottom, and the scrollbar drawn **over** the
      cards rather than beside them (overlay scrolling; a scrollbar taking
      layout width would widen the reserved strip). Then re-check the two
      properties this must not have disturbed, both of which are the #737 check
      above in miniature: with a font bumped well up, the open sidebar still
      pushes tiled windows fully clear with no overlap and no dead gap (the open
      width is measured through the scroller unchanged), and the open/close
      slide is still width-only — no vertical stretch. Nothing tracks the
      output's height on the shell side and nothing needs to: the surface is
      anchored `Top + Bottom` with `exclusive_zone = 0`, so its allocation
      already _is_ the work area minus the bar, and the viewport rides it on
      every output. (An earlier round of this PR carried a
      `max-content-height` cap plus a `notify::height` subscription for that
      job; measured, the cap moved only the natural request — which nothing on a
      both-edges-anchored axis reads — so it was deleted rather than left as a
      live-verify item that could not fail.)

## Audio & media

- [ ] **(#470)** Drag-safe seek slider: open the Media drawer on an active
      player, **drag the seek bar and hold**. The thumb should follow your
      finger and no longer snap back to the mpris poller's stale position
      mid-drag, then settle to the seeked position on release.
- [ ] **(#494)** Audio panel row dedup (`panels/audio/{sinks,sources}.rs` →
      shared row constructor): confirm sink/source rows render and behave
      identically to before (no visual or interaction change expected).
- [ ] **(#512)** Perceptual dBFS spectrum scaling: play music with an
      audio-reactive plugin (preem-demo's scope tile) subscribed — bars should
      use the **full tile height** (bass/transients near the top, mids/highs
      moving mid-height, quiet passages still visible, silence flat), not
      hugging the baseline as before.
- [ ] **(#529)** Install `hytte-plugin-audio-widget`
      (`cargo install --root /usr/local --path crates/hytte-plugin-audio-widget`),
      enable the unit, open the sidebar: play music → the spectrum bars and
      the new **LED peak/level strip** dance, the peak dot floats above the
      bar and decays back down after each transient. Pause → bars/LEDs fall to
      rest (no freeze). Close the sidebar → the card parks (no renders).
- [ ] **(#557)** `preem::Scope` oscilloscope tile (the marquee's neighbor):
      confirm the glow trace, phosphor decay trail, and graticule render
      correctly against a live `AudioSpectrum` feed — the decay should look
      like a fading beam-trail, not a redraw-from-black.
- [ ] **(#539)** The audio-widget's marquee now scrolls the live
      `title — artist` from mpris when a track is playing (supersedes #529's
      decorative "~ NOW VIBING ~" banner, which is now only the fallback when
      nothing plays).
- [ ] **(#563)** Perf check: the marquee is now rasterized only when its text
      changes (not on every ~43 Hz `view()` call) — with the sidebar open and
      audio playing, CPU load from the audio-widget process should be
      noticeably lower than before this PR.
- [ ] **(#840/#845)** Audio-widget y2k rework — the entry #845 could not carry
      itself (its builder's lane deliberately excluded this file). With a
      player running:
  1. **Placement:** the card now sits at the **bottom** of the sidebar (below
     the departures board; pet keeps the very bottom edge company), not in the
     top region.
  2. **Transport row:** prev / play-pause / next buttons under the LED strip —
     chunky, circular, play-pause accented. The play-pause glyph mirrors the
     player's real state (it flips when you pause from the _player_, not
     optimistically on click).
  3. **Song position:** a fixed-width `MM:SS/MM:SS` dot-matrix readout
     (`--:--` when the player reports no length). The card must never reflow
     as digits tick.
  4. **Power claim:** with the sidebar **closed** and the Media drawer away,
     `busctl` / journal should show no 250 ms `Position` polling — the poller
     parks when no consumer is on screen and resumes on sidebar open.
- [ ] **(#565)** Spectrum tap now activates **only** while a subscriber is
      actually on-screen (sidebar open, or bar-mounted) rather than for the
      whole session — see the `#583` item below for the PipeWire-side check
      (the demand gate landed here; #583 fixed the node itself lingering).
      Separately, the SDK's ~30 Hz view-rate cap should relieve single-core
      saturation while the sidebar is open with audio playing — no visible
      lag in the meters at the coalesced rate.
- [ ] **(#583)** With the shell running and the sidebar **closed**:
      `wpctl status` (or pavucontrol's Recording tab, or Helvum) should show
      **no** `trollshell-spectrum` client at all (previously it sat in the
      PipeWire graph from login regardless of demand). **Open** the sidebar →
      the node appears and the spectrum animates; **close** it → the node
      disappears again. Repeat several open/close cycles → exactly one node
      while open, zero while closed, no accumulation
      (`RUST_LOG=hytte_services=debug` shows a matched "spectrum capture
      built"/"spectrum capture torn down" pair per cycle). Also worth an
      eyeball: audio keeps working normally across cycles, and the spectrum
      doesn't flash a stale frame on re-open.
- [ ] **(#422)** Plugin-side park on the hide edge — the consumer half of the
      previous two items. With music playing and the sidebar **open**, let the
      audio-widget's bars/LEDs get loud and the preem-demo's scope draw a
      strong trace, then **close** the sidebar, **pause the music**, wait a few
      seconds and **reopen**. Both cards should come back at rest — flat
      spectrum bars, LEDs and peak dot at zero, a dark scope face — and then
      light up again from live audio, rather than re-appearing frozen on the
      loud frame from before the close (the scope previously re-appeared as a
      saturated constant waveform, since its 1 Hz heartbeat kept re-stamping
      the last-known bands while hidden). The preem-demo's **clock/ticker/
      marquee must _not_ park**: reopening should show the current time, not
      the time at which you closed it.
- [ ] **(#664)** The Media panel's **Auto** source chip couldn't stay
      pressed — clicking it re-pressed then immediately un-pressed itself as
      soon as any player existed, so "revert to automatic" had no visible
      feedback — and a **pinned** player was indistinguishable from one
      merely picked by the automatic heuristic. Needs a real Niri session
      with **two** MPRIS players running (e.g. `mpv` plus a browser tab or
      Spotify) — the switcher row hides below 2 players, so testing with one
      player exercises none of this — and a **shell restart first**, since
      the new `ts-media-source-pinned` ring/bold-weight styling is CSS and
      only applies on (re)load:
  1. Open the Media drawer page — **Auto** should be pressed on open,
     alongside the heuristically-picked player chip (which is pressed but has
     no ring).
  2. Click a player chip → Auto releases, that chip stays pressed and gains
     the ring + bold weight.
  3. Click **Auto** → it presses and _stays_ pressed (this is the click that
     used to undo itself); the ring disappears; the heuristic chip stays lit.
  4. Click **Auto** again while already pressed → it must stay pressed, not
     flip released.
  5. Quit the pinned player → Auto lights back up and the surviving player's
     chip takes over with no ring (a stale pin reads as automatic).
  6. Start/stop a third player while pinned → the roster rebuilds and the pin
     survives with its ring intact.

  Separately, a bare-string (rather than array) `xesam:artist` no longer
  comes through as an empty artist — needs a player that actually emits the
  metadata that way to exercise live; otherwise it's covered by hermetic
  parse tests only.

- [ ] **(#838/#851/#854)** The bar's media control picking full-row vs mini
      chip. **Seven** fixes shipped against this one bug and six of them
      failed, so walk all of it rather than glancing at the bar. The shape on
      `main` pins the title label to `TITLE_CHARS = 24` in both directions,
      which makes the full transport row's natural width a constant, and hands
      the full-vs-mini decision to an `AdwBreakpointBin` whose single
      `max-width` breakpoint is measured once from the built row and then
      frozen. Nothing measures neighbours at runtime any more —
      `components/center_budget.rs` is deleted. Since round seven the
      rendition-independent request comes from a frozen-size `GtkLayoutManager`
      (`FrozenSize`, in `widgets/mpris.rs`) rather than a `set_size_request`
      pin, which is what lets both renditions sit against the bar's right-hand
      cluster. What to check: 0. **First, work out which generation you are running.** Three shapes have
      shipped and each is wrong in its own way, so start with the journal, not
      the eyeballs:
      `journalctl --user -u trollshell | grep "exceeds AdwBreakpointBin"`.
      That line prints once per allocation on **both** #851 and #854 builds —
      the warning is about the width pin, which #854 kept, so the earlier claim
      here that it stopped at #854 was wrong — and prints **nothing at all** on
      this build, where the pin is gone (measured: 3 forced collapsed
      allocations → 3 warnings with the pin, 0 with the frozen-size shim). So
      any such line means you are on an older build and the rest of this list
      will mislead you. On #851 the chip was allocated outside the bin's
      `GTK_OVERFLOW_HIDDEN` clip rect and drawn nowhere at all — the centre slot
      went _empty_, a dead ~250–290 px hole with nothing to click. On #854 it
      was drawn, but stuck to the **left** edge of the slot.
  1. **The original bug, and "keep right plz".** With a player **stopped**
     (not merely paused — stopped, so there is no position to show), the
     centre slot must show the **mini chip**, not the full transport row, and
     must not crowd the app-switcher buttons beside it. This is the symptom
     the issue was filed on. Then watch the right-hand edge: the **collapsed
     chip hugs the right cluster; the expanded row hugs it too; neither ever
     overlaps the window list**. Open and close windows until the slot flips
     between the two renditions — the right edge must not move, and the chip
     must never drift leftwards with the window list as it grows and shrinks.
     That drift was #854's `halign: Start`, and it is what Annika came back
     about on the seventh round.
  2. **Sidebar open/close — the case that failed twice.** Open and close the
     sidebar while a track plays. The centre slot must settle into the right
     rendition **immediately and stay there**. Iterations 2 and 4 both failed
     the same way: they published a slot width computed from the bar's size
     _before_ the compositor's configure had landed, so the widget sat in the
     wrong rendition until unrelated niri traffic (a window-title tick)
     happened to trigger a remeasure. If the right answer arrives only after
     you click around, that class is back.
  3. **No blinking.** Play a track with the screen full of windows whose
     titles change often (a browser tab, or a terminal running something
     chatty) and watch the centre slot for a while. It must never flip
     between full row and mini chip on its own. Iteration 3's failure was
     exactly this: the fit was recomputed off the window list's natural
     width, which tracks live window titles, so the chip blinked several
     times a second.
  4. **Title truncation is deliberate.** Track titles now ellipsize at 24
     characters and the row no longer grows with a long title. That is the
     mechanism rather than a regression — a constant row width is what makes
     the frozen breakpoint threshold correct. `TITLE_CHARS` in
     `trollshell/src/widgets/mpris.rs` is a one-line taste knob if 24 reads
     as too tight or too wide.
  5. **Text scaling, without a restart.** Change the system text scale (or
     the interface font) while the shell is running — the thing
     `install_scaled_base_font` exists to make work. The media chip must
     still flip renditions at the width the row actually needs, and the
     title must not end up clipped mid-word with
     `journalctl --user -u trollshell | grep "Allocation width too small"`
     printing a line per allocation. The three numbers this widget freezes
     (its minimum, its natural, and the breakpoint threshold) are re-derived
     on exactly those two GTK settings; if this leg fails, that re-derive is
     what to look at, not the alignment.
  6. **One expected regression, disclosed.** The mini chip still _requests_
     the full row's width — that request-stability is what makes the blink
     loop structurally impossible — so the space it gives up falls into the
     bar's mid-gap rather than back to the window list. Long **window**
     titles therefore ellipsize slightly sooner than before #851. Tunable,
     but only by re-coupling the widget to its neighbours, which is the thing
     that failed four times.

## Preem raster kit (`hytte-plugin::preem`)

The kit's own widget skins, which CI can only check as byte patterns. Every
item below is exercised by `hytte-plugin-preem-demo` (install with
`cargo install --root /usr/local --path crates/hytte-plugin-preem-demo`, enable
its unit, open the sidebar) — one card stacking every widget, rotating
VFD → LCD → OLED → CRT every 10 s, and re-skinning immediately when you tap the
clock. The two audio-fed preem items live under "Audio & media" above (the
`#557` scope tile and `#422`'s park), because what needs verifying there is the
audio feed, not the raster.

- [ ] **(#843/#839)** Marquee on a fixed dot grid — the entry #843 could not
      write for itself, since its whole result is visual. With the sidebar open,
      watch the scrolling marquee row (third widget down):
  1. **The dots never move.** The unlit dot matrix behind the text is a fixed
     grid nailed to the buffer: it must sit perfectly still while text passes
     over it. Before #839 the ghost dots travelled with the message, with the
     per-char-cell gaps sliding along — if you see the background pattern
     drifting or breathing, the regression is back.
  2. **The text steps dot by dot.** Each step should land the message exactly
     one dot column over — crisp, chunky, on-grid. It must never look smeared,
     doubled or blurred between two dot positions (the pre-#839 symptom, from
     panning a pre-rendered strip by 3 px against a 4 px dot pitch).
  3. **The seam wraps clean.** Let the message loop. The join between its end
     and its restart should pass through with the same one-dot cadence as the
     rest — no jump, no stutter, no partial dot at the wrap.
  4. Worth a look on all three skins: on LCD the ghost grid is at its most
     visible (so 1 is easiest to judge there), and on VFD/OLED the bloom should
     glow off the lit dots without dragging the grid with it.
- [ ] **(#1091)** A marquee with `dot_px = 2` fits the bar without growing it.
      The pitch is a runtime knob now, and `9 * dot_px` is the whole height, so
      a bar-mounted ticker asks for `2` and gets 18 px against the bar's 32 —
      which CI can check as arithmetic but not as glass. Point a bar-mounted
      plugin at `Marquee::new(…).dot_px(2)` (the `hytte-plugin-bar-clock-demo`
      shape, or `preem-demo` with `Mount::Bar`), then:
  1. **The bar does not grow.** Its height stays wherever `assets/trollshell/style.css`
     puts it — if the bar gets taller, the chip is asking for more than 32 px
     and the pitch did not reach the kit.
  2. **Is a 5×7 bitmap font acceptable here?** That is the real question, not
     "are the dots visible". At pitch 2 the falloff plateau covers the whole
     2×2 cell, so a dot is a solid block and two adjacent lit font pixels merge
     with **no seam**: the top of an `8` is one 6 px bar, not three dots. It is
     deliberate (a 2×2 cell has no room for a rim, and a dimmed one would read
     as a grey smear) and documented on `MIN_DOT_PX` — but it means what lands
     in the bar is legible pixel-font text rather than a visible dot matrix.
     Compare against `dot_px = 3`, the smallest pitch that still reads as
     separated dots, which is 27 px and so leaves nothing for chip padding in a
     32 px bar. Judging that trade is the whole point of this item.
  3. **The ticker behaves differently at a finer pitch, and both are correct.**
     The finer grid fits more dot columns in the same window (94 at pitch 2
     where 4 fits 46), so expect these rather than reporting them as bugs:
     - A **short message stops scrolling.** A 10-char title that scrolls in a
       192 px window at pitch 4 fits the grid at 3 and at 2, so it holds
       static. Use a longer message if you want to watch it move.
     - A fixed `speed_dots_per_sec` is **half the on-screen speed**: the scroll
       steps whole dots, so 12 dots/s is 48 px/s at pitch 4 and 24 px/s at
       pitch 2. Scale the speed with the pitch if you want the rate held.
  4. **Nothing that did not ask for a pitch moved.** The sidebar preem-demo card
     still renders its 36 px dot-matrix and marquee rows exactly as before —
     the byte-identity tests cover this, but it is the cheapest possible
     eyeball check that the defaults really are untouched.
  5. **On the CRT skin the comb follows the pitch.** It should read as a raster
     at 2 and at 3 exactly as it does at 4 — one dark line per dot row, sitting
     in the seam _below_ each row, with no brightness beat from one glyph row to
     the next. A comb that skips a row, or that darkens a dot's bright middle
     instead of the gap under it, means the re-phasing (`Mask::with_pitch`) is
     not reaching this surface.
- [ ] **(#397)** Split-flap and nixie boards (the two bottom rows of the
      preem-demo card, `HH:MM:SS` on both, deliberately running in slow motion
      so the mechanisms are legible at the shell's ~1 Hz heartbeat):
  1. **Split-flap:** the upper card visibly hinges _down_ over the lower one.
     The outgoing character's top half squashes away, revealing the incoming
     character's top behind it; then the incoming character's bottom half folds
     in over the outgoing one. It should read as a card falling — slow at the
     top, whipping through horizontal — not as a shutter closing at a constant
     rate, and not as a cross-fade.
  2. **The hinge slot stays dark on every skin**, including VFD and OLED where
     the bloom would otherwise fill it in: a hard one-pixel gap straight across
     each card, cut through the glyph.
  3. **The ripple:** on a whole-face change (watch the minute or hour roll over,
     or just after the card first mounts) the cards should start left-to-right
     in a wave, not all at once.
  4. **The falling card's leading edge lights up** as it passes horizontal — a
     bright rule across the full card width — and there is _no_ such edge on a
     board at rest.
  5. **Nixie:** the whole row cross-fades **simultaneously** (no ripple). The
     outgoing digit should linger, then collapse, while the incoming one strikes
     fast — both visibly alight at once mid-switch, with a broad soft halo over
     the skin's own glow on VFD/OLED. On LCD there is deliberately no halo (a
     reflective panel does not bloom) but the fade still runs.
  6. **Accent tracking:** both boards take the desktop accent like every other
     kit widget (#376) — change the accent and the lit glyphs should re-tint
     while the card faces / cathode stacks keep their per-skin panel colour.
  7. **The fixture never moves:** bezel, card row and the gaps between cards
     must be rock-steady through every flip — only the cards move.
- [ ] **(#397)** The **CRT pass** — the fourth skin in the rotation, and the
      only one that is a _pass_: wait for the whole card to turn P31 green
      (or tap the clock until it does) and judge **every widget at once**,
      because the point is that none of them had to be changed for it:
  1. **Scanlines are there and are not eating the glyphs.** A dark horizontal
     line every fourth row, threaded through the _gaps_ between dot rows. Text
     on the ticker, marquee and boards must stay exactly as legible as it is on
     VFD — if a scan line is running through the middle of the dots, thinning
     or halving them, the comb's phase has slipped and that is the regression.
  2. **The whole card is on one tube.** Marquee, scope, gauge, both boards, the
     7seg clock, the LED strip: all of them scanlined, all of them green. A
     widget that stayed VFD-cyan or stayed clean is one that stopped routing
     through the shared composite.
  3. **Curved glass:** the picture is brightest in the middle and falls off
     toward the rim, with the **corners** darker than the edges beside them —
     the ends of the 268 px ticker should be noticeably dimmer than its centre.
     It must read as light falling away, not as a black border drawn on.
  4. **Nothing is warped.** This is deliberately a vignette, not a barrel
     distortion: straight lines stay straight, the dot grid stays square and
     on-grid, and no glyph edge is blurred or resampled. A bowed graticule or a
     softened dot means someone added distortion.
  5. **The phosphor bloom is broader than VFD's** — lit dots wear a soft halo
     that spills further than the VFD glow does, and the scanline gaps read
     _through_ that halo rather than being filled in by it.
  6. **Accent tracking (#376):** change the desktop accent — the CRT's ink
     follows it like every other skin, while the near-black tube face and the
     scanlines stay put.
  7. **No flicker, no crawl.** The pass is stateless: hold still and watch a
     static widget (the 7seg clock between minutes). The scanlines must be
     perfectly frozen — they must not creep, shimmer, or breathe between
     frames. Only the scope's own phosphor trail decays over time.
- [ ] **(#930)** **The gauge's halo, halved.** The gauge now wears half the
      bloom radius its skin asks for (`BLOOM_RADIUS_DIV` in
      `crates/hytte-preem/src/gauge.rs`) — a taste change picked off renders,
      so eyes on glass is the only verification there is. Watch the needle
      gauge on the preem-demo card through a full skin rotation:
  1. **VFD is the one to judge** (the default, and where the complaint came
     from): the pointer should read as a drawn needle with a glow, not as a
     lit smudge. Its lit cross-section on the default 144×64 face at ×2 is 20
     output px, down from 28.
  2. **The core is not dimmer or thinner.** Halving the bloom dims the halo's
     shoulder; a handful of near-black pixels beside thin elements gain
     1–2/255 as the blur concentrates, imperceptible. Fully-saturated ink —
     the bright centre of the blade, the hub and the value arc — should be
     exactly as bright and exactly as wide as before. A needle that got
     fainter is a different bug.
  3. **CRT keeps a visibly broader halo than VFD** (radius 3 → 2 there, so it
     is narrowed too but stays the widest of the four). If VFD and CRT now
     read the same, the per-skin spread has collapsed.
  4. **LCD and OLED are pixel-identical** — LCD never blooms, and OLED's radius
     was already at the floor. Any visible change on those two skins is not
     this PR; it is a change to the shared composite path.
  5. **The scope beside it is untouched.** The change is gauge-local (the
     palette is copied per frame), so the scope's phosphor halo on the same
     card, in the same skin, must keep exactly the spread it had. Judge them
     side by side on VFD — that contrast is the point of the change.
  6. **Small dials are unaffected.** A square 48/64 gauge was already at #931's
     proportional cap, so nothing there should move.

## Preem state over the wire

The pivot epic (#881): a plugin stops rasterising its own preem widgets and
sends typed `Node::Preem` state instead — the shell owns the kit instance,
the animation clock, and (feeding into it) style resolution. None of the
handshake, the frame timing, the multi-monitor render path, the
texture-upload optimisation, or a colour judgement can be proven from a
hermetic test; each needs a real plugin over a real socket in a real Niri
session.

- [ ] **(#883/#896)** The shell-side renderer and its 20 Hz animation clock:
  1. **The `Hello` handshake against a plugin binary built before #895.** Any
     bundled plugin you haven't rebuilt (or a stale deployed unit) declares
     no `vocab_max`, so it must never receive a `Hello` frame at all. Restart
     the shell and that plugin together and watch
     `journalctl --user -u 'trollshell-plugin-*'` for a while: a MessagePack
     decode failure, or a unit stuck restart-looping, means the negotiation
     gate leaked and re-created the #437 crash loop on every deployed plugin
     at once.
  2. **Motion, not steps.** Open `hytte-plugin-preem-demo`'s card: the
     marquee scroll, needle swing and phosphor decay should run at the
     shell's own frame rate, not step once per the plugin's old ~1 Hz
     heartbeat. **Superseded by #897/#926 below**, which replaced this
     20 Hz timer with the display's own frame clock — the motion should now
     read smooth rather than merely faster, and the idle drain this entry
     used to warn about ("it never parks when nothing is animating") is
     gone. Judge #883's own contribution here only against a shell that
     predates #926.
  3. **Two real outputs, not one thread mapping the same tree twice.** With
     two monitors attached and an animating preem widget on screen, confirm
     it neither runs at double speed nor decays its phosphor twice as fast —
     each output gets its own reconciler and frame clock, the case a
     hermetic idempotence test cannot cover.
  4. **The clock's arm under load**, not just the functions it calls — there
     is no GTK main loop under `cargo test`, so the `dt` measurement and the
     repaint request are only ever exercised on glass. With several plugin
     panels animating at once, motion should stay smooth rather than the step
     drifting into visible jitter, and resuming from a stall (e.g. suspend)
     should catch up smoothly rather than snapping. #897/#926 moved this onto
     each mount's frame clock; the arm/park half of it is checked there.
- [ ] **(#884/#898)** The SDK's display seam, against a shell carrying #896:
  1. Start (or restart) `hytte-plugin-preem-demo` and
     `hytte-plugin-bar-clock-demo`. The card's eight widgets and the bar
     chip's seven-segment clock should render through the shell's own kit;
     check the journal for **no** "plugin sent a `Node::Preem`, but this
     shell does not advertise…" warning, which would mean the negotiation
     gate leaked the other way from the #896 check above.
  2. **The one-frame swap at plugin start.** Both demos render one `Pixels`
     frame before the shell's `Hello` arrives, then switch to `Preem` in
     place. Watch closely for a flash or reflow, and — the easiest thing to
     miss — a scroll jump or needle snap: the plugin has already advanced
     its own marquee offset / gauge deflection by then, and the shell starts
     animating from its own resting state instead.
  3. **The wire goes quiet.** With the card on screen and nothing changing,
     `RUST_LOG=trollshell=debug` (or a socket trace) should show no `Render`
     frames between state changes — the old rasterising path sent one per
     widget on every heartbeat regardless.
  4. **Against a shell that predates #896**, run both demos unchanged:
     `Node::Pixels` throughout, looking exactly as they did before either PR
     existed.
- [ ] **(#902/#907 — lands with that PR, open at the time of writing)**
      `PixelSurface`'s texture-upload equality guard. Put an animating preem
      widget (the preem-demo marquee, or caw's face) in the same bar/sidebar
      region as a **static** `Pixels` chip — `hytte-plugin-pet` idle, or the
      Stats drawer's per-core LED panel — and watch `trollshell`'s CPU with
      `top -p "$(pgrep -x trollshell)"` or
      `perf top -p "$(pgrep -x trollshell)"`. Before this lands, the static
      chip's texture re-uploads on every one of the animating widget's ~20
      ticks a second; after, it uploads once and then nothing until its own
      pixels actually change. Nothing on screen should look different — same
      picture, same crispness; a chip visibly freezing on a stale frame, or a
      resize snapping late, would mean the guard is over-eager.
- [ ] **(#903/#908)** Hot-plugging a monitor with a plugin panel open:
  1. Open a plugin panel with a moving preem widget (`preem-demo`, or `pet`)
     in the drawer, then hot-plug — unplug/replug the second output, or
     force a kanshi profile switch. The drawer should close with the
     rebuild as before; reopening that panel afterward should show its
     animations starting **from rest** (needle at zero, phosphor dark, flip
     boards blank) rather than resuming mid-swing, which is the panel's
     preem scope having actually been released rather than leaked.
  2. Repeat the hot-plug several times with the drawer **closed** and that
     panel never reopened: nothing should accumulate —
     `RUST_LOG=trollshell=debug` should show no growth in per-emission
     render work across cycles.
  3. **Layout regression check**, since the fix mounts each panel's content
     in a new inner box: reopen a few different plugin panels and confirm
     each still fills the drawer card exactly as before — no new gap at the
     top, no collapsed-height panel, and a panel using its own boxed-list
     still renders flat on the drawer surface rather than nested inside a
     second card.
- [ ] **(#885/#912 — lands with that PR, open at the time of writing)**
      Shell-side styling, with `hytte-plugin-preem-demo`'s sidebar card open
      (it grows a `ROLE` and a `PIN.` cell alongside the original eight):
  1. Change the desktop accent (Settings → Appearance) with the plugin still
     running: every widget on the card — the seven-segment clock, ticker,
     marquee, textbox, scope, gauge, both boards, and `ROLE` — should
     re-tint immediately, with **no plugin restart**. `PIN.` must stay hot
     pink through this; that difference is the whole #396 feature.
  2. Flip the system between light and dark: same expectation, since both
     the accent and the success colour can resolve differently per scheme.
  3. `ROLE` should read as the theme's own success colour, not the desktop
     accent — under a non-green accent the two should look visibly
     different side by side.
  4. Tap the seven-segment clock (or wait ~10 s) to cycle skins (#397):
     every widget including the two new cells should change skin together —
     `PIN.` changes its field/ghost/bloom with the rest but keeps its pink
     ink, which is the "ink only" rule on glass.
  5. Against a shell that predates #896/#883 (or with `Hello` otherwise
     suppressed), `PIN.` should still render pink — the raster (`Pixels`)
     arm honours the pin too, not only the state arm.
- [ ] **(#909/#920)** A destroyed plugin region now actually releases its
      reconciler subscription instead of pinning itself against its own
      teardown — `build_region` used to hold a strong container clone that
      only its own `destroy` handler could abort, and that handler could
      never fire. On a two-monitor Niri session, run at least two plugins
      that mount cards/chips — one in a sidebar region, one in a bar region
      — preferably with a moving preem widget (`hytte-plugin-preem-demo`,
      `pet`, `departures`) so a stranded region would keep animating. Open
      the sidebar on both outputs, then hot-plug (unplug/replug the second
      monitor, or force a kanshi profile switch) several times: expect **no
      visible change** — no stranded regions, chips return to the bar in
      the same order and spacing, sidebar cards come back where they were.
      The one thing a weak handle could plausibly disturb: since the
      container's visibility now runs behind a `WeakRef` upgrade inside
      `bind` rather than a guaranteed strong closure, an emptied region
      must still hide itself (no phantom 6px gap left in the bar group)
      and must reappear once it fills again. `RUST_LOG=trollshell=debug`
      across several hot-plug cycles should show no growth in per-emission
      render work. Card scopes are still not released once every region
      for a plugin is gone (an output-less session, or one monitor's panel
      close dropping a scope a sibling monitor is still painting) —
      that residual is deliberate and tracked separately as #921, not a
      regression from this fix.
- [ ] **(#901/#918/#919)** Merged as `afc152c`. Three new caps on a
      `Node::Preem` wire tree, none reachable through any bundled plugin
      today — needs a patched local plugin build (e.g. of
      `hytte-plugin-preem-demo`) pointed at oversized trees, watched with
      `journalctl --user -u trollshell -f`:
  1. **The per-scope instance cap.** A tree of 65 preem nodes: the first 64
     draw and animate normally, the 65th renders as an empty placeholder
     that keeps its CSS chrome, and exactly **one** "asks for more preem
     renderer instances…" line appears — not one per frame.
  2. **The node-count and depth caps.** A tree of 4097 nodes is walked to
     4095 and warns once with "…exceeds the host's node cap"; a chain of
     65 nested boxes is walked to 64 levels and warns once with "…nests
     deeper than the host will walk" — never both lines for the same tree.
     A truncated container should read as a container with pieces missing,
     not as a crash or a frozen frame.
  3. **Two nodes, one id.** Two preem nodes sharing one explicit id in the
     same tree collapse onto a single renderer instance: one "two preem
     nodes in this tree share an id" line naming both widget kinds, and
     last writer wins — nothing goes missing. The line should not repeat
     if the pair is closed and reopened, or dropped and brought back.

     Worth one extra check on the instance cap specifically, since it is
     the one the review round found a real bug in: a tree pinned exactly at
     the cap with one id rotating every frame must never blank on either
     output of a two-monitor session. The stock desktop renders nowhere
     near any of these caps, so none of these lines should appear against
     an unpatched plugin set.

- [ ] **(#921/#923)** Merged as `bf2b266`. Preem renderer scopes were
      released from places tied to a monitor's widgets: with zero live
      regions (every output unplugged) a departing plugin's card scope
      stayed resident forever, and destroying one of two monitors' drawer
      children dropped the shared panel scope out from under a sibling
      still painting it. Both releases now ride a monitor-independent
      watcher of the live plugin set instead.
  1. **Every output unplugged, with an animating plugin.** Start a plugin
     with a scrolling marquee or phosphor scope on a bar mount. Unplug —
     or `niri msg output … off` — **every** output, stop the plugin's
     unit while the session is output-less, then replug. The chip must
     come back with fresh animation state and no stale instances behind
     it; before this fix the scope stayed resident for the rest of the
     session.
  2. **Two monitors, one drawer closed.** Open the same plugin's drawer
     panel on both outputs, then close/destroy one monitor's drawer: the
     surviving monitor's panel must keep painting and keep animating, not
     go blank or freeze. Close the last one too and the panel's animation
     should restart from rest on the next open — the pre-existing #883
     trade, unchanged.
  3. **A documented residual, not expected to reproduce.** The panel
     refcount's rustdoc notes one theoretical gap: a `shown` cell that
     outlived its plugin's departure, released only after a _later_
     session of the same plugin id took a hold, would decrement that new
     session's count instead. Today's wiring makes it unreachable — every
     live child blanks on the departure emission, and the releaser runs
     before any drawer child exists — so there is nothing to chase on
     glass, just don't be alarmed if you go looking for it in the code.
- [ ] **(#884/#885/#924)** Merged as `f5168c8`. The workspace's last
      two `.colors()` consumers move onto the state path: `pet`'s and
      `caw`'s speech bubbles now pin a full palette (field + ink +
      notdef) instead of only an ink. Against a **rebuilt** shell (behind
      #896):
  1. **`pet`.** Open the sidebar: the bubble beside the face should look
     exactly as it does today — same lilac field, same bright-lilac
     glyphs, same fixed-width slot that wraps _down_ rather than
     sideways. The journal should show no "does not advertise" warning
     for `trollshell-plugin-pet`, and a socket trace should carry the
     _message_ rather than a `Pixels` buffer when the cat speaks. Poke
     it: the bubble text changes, the colors do not.
  2. **`caw`.** Same check beneath her face, plus the once-a-day briefing
     box (or force one): taller, same violet, same wrap. A short caw
     should still hug into a compact chip.
  3. **The point of the whole change: change the desktop accent** while
     either bubble is on screen. Neither may move — they are pinned, and
     pinning excludes them from the re-tint. Anything else on screen that
     is _not_ pinned (`preem-demo`'s `ROLE` cell, `timer`'s readout)
     should re-tint in the same instant, which is what says the shell is
     re-rendering rather than sitting on a cache.
  4. **`preem-demo`'s palette row** — `ROLE` / `PIN.` / `FLD` side by
     side — is the one-glance version of the above and needs no bubbles:
     on an accent change `ROLE` re-tints wholly, `PIN.` not at all, and
     `FLD`'s glyphs move while its lilac ground (the bubbles' own
     `3a2250`) stays put. That last one is the field pin doing its job.
  5. **Both faces** (`pet`'s and `caw`'s hand-drawn `Frame`) must be
     pixel-unchanged throughout — they never left the `Pixels` escape
     hatch.
  6. **Against a shell that predates #896**, both plugins should render
     unchanged, byte for byte, exactly as before either PR existed — the
     raster-parity tests pin this, so any difference here would mean the
     kit call and the state-arm pin have drifted apart.

     One thing to know rather than chase: `field` and `ink` are forced
     fully opaque like every kit palette slot, but `notdef` is not — a
     translucent `notdef` pin punches see-through holes where an
     uncovered glyph falls back to the box. `pet` and `caw` both pin
     `notdef` fully opaque, so this alpha difference has no visible
     effect on the bundled plugins as shipped.

- [ ] **(#897/#926)** Merged as `4cde9bb`. Preem animation now rides each
      **mount's GTK frame clock** instead of one process-wide 20 Hz timer
      that was armed at startup and never broke. CI cannot see any of this:
      there is no compositor delivering frames under `cargo test`, so the
      hermetic tests drive the decision the tick makes and one
      `#[gtk::test]` drives a real `GdkFrameClock` under `xvfb` — but the
      shell's actual mounts, on real outputs, are only observable here.
  1. **No periodic wakeup when everything is settled.** With every preem
     widget at rest — or, better, with no preem plugin running at all —
     `strace -c -e clock_nanosleep,ppoll -p "$(pgrep -x trollshell)"` over
     ~10 s should show no 20 Hz drumbeat from this path. Before #926 that
     was 200 wakeups in the window regardless of what was on screen, on
     every session including one with no plugins installed.
  2. **A marquee text change (or a gauge value change) resumes without a
     hitch.** The single most important item: the mount had parked, and only
     the mapping pass can re-arm it. A wrong re-arm point freezes every
     preem animation in the shell **silently, with CI green** — that is the
     exact failure mode #897's body calls out. Watch `preem-demo`'s card go
     still and then move again on its next state change.
  3. **Smooth at the panel's refresh.** A gauge sweep should no longer read
     steppy and a marquee faster than 20 dots/s should no longer skip dots
     — the whole point of Annika's "isn't 20 Hz a bit rough?".
  4. **A closed drawer, and a closed sidebar, cost nothing.** Open a plugin
     panel with something animating, close the drawer, and confirm the
     wakeups stop (same `strace`); reopen and confirm it resumes rather than
     jumping ahead by the whole closed interval. Then repeat with a
     **sidebar** card — the harder case, because the sidebar is a layer
     surface presented once for the process lifetime and "closing" it only
     flips a `GtkRevealer`, which GTK does **not** treat as a reason to stop
     ticking. The review round measured the first cut of this PR still
     ticking at full refresh there.
  5. **Two monitors, one chip.** With the same plugin chip on both bars, its
     animation must run at normal speed, not double — the renderer instances
     are shared across outputs while the frame clocks are not.
  6. **A slow or throttled clock keeps the right speed.** Load the shell up
     (several animating panels at once, or a compositor throttling an
     occluded surface) and confirm a marquee still crosses at its stated
     dots/second rather than visibly slowing. The `dt` clamp is the resume
     cap, not a per-frame cap, precisely so this cannot drift.
- [ ] **(#911/#927)** Merged as `bfe7275`. Preem frames now travel to every
      monitor's texture as one shared `Arc<[u8]>` instead of a fresh copy
      per output. On a two-output Niri session, mirror an animating preem
      widget (the preem-demo plugin's marquee, or `caw`) on both outputs and
      watch `perf top -p "$(pgrep -x trollshell)"`: the RGBA fan-out
      (`__memmove_avx_unaligned_erms` climbing with the number of outputs
      showing the widget) should be gone from the profile — one
      rasterisation per tick remains. Nothing should look different on
      screen — same picture, same pixels; this PR moves ownership only,
      never a byte of content.
- [ ] **(#893 stage B / #886 / #863 / #1072)** **`Scope` renders on a
      `GtkGLArea`, and that is the default again.** #1067 got the GL loader
      actually resolving entry points for the first time, and the day it did,
      `preem_gl_diff` reported 12/12 cases over the parity ceiling — so #1070
      parked the shipped default on **CPU** rather than switch every preem
      chip's renderer on glass with no parity evidence behind it. #1072
      classified those twelve: every one of them was the **harness** reading
      the wrong framebuffer (`gtk_gl_area_snapshot` hands its texture to GSK
      and the next `attach_buffers` takes a different one out of the area's
      pool, so each case was scored against its predecessor's picture). Read
      after the pool settles, and required to be stable across two renders of
      the same state, the two arms are **byte-identical — max |Δ| 0 of 255 on
      every channel of all twelve cases, under llvmpipe + Xvfb** (Mesa 26.2.2).
      So GL is the default and `TROLLSHELL_PREEM_RENDERER=cpu` is the kill
      switch. This entry used to say nothing here could be gated in CI —
      stale since #1077 put Mesa llvmpipe into `nix flake check`'s
      system-tests bucket, and wrong outright after #1080: that check now
      builds and runs `preem_gl_diff` itself under that same llvmpipe
      context, with `TROLLSHELL_PARITY_EXACT=1` pinning it to the bit-exact
      result measured above — any non-zero delta on any channel fails the
      check, not just a ceiling breach. What still can't be gated is the one
      question llvmpipe's bit-exactness can't answer: colour space on a real
      driver (item 1 below) — everything up to and including the draw call
      under software rendering is now CI's; the draw on _your_ GPU is yours.
  1. **The GL arm's picture is right.** Start the shell (no environment
     variable needed now) and open `hytte-plugin-preem-demo`'s card. The scope
     must look like the scope did: same graticule, same beam, same phosphor
     trail length, same skin colours. A gamma-shifted or washed-out trace
     means the `GtkGLArea` framebuffer is being treated as linear where
     `PixelSurface`'s texture was sRGB — the one colour-space question this
     design could not settle from the sources, and the one llvmpipe's
     byte-identical result does not transfer to your driver for free.
     `RUST_LOG=hytte_gl=debug journalctl --user -u trollshell | grep 'resolved'`
     names which loader route won (glvnd or libepoxy, #1067);
     `journalctl --user -u trollshell | grep -i 'GlSurface\|GL context'` should
     otherwise be silent — a line there names the fallback that fired.
  2. **The kill switch still forces the kit.** Restart with
     `TROLLSHELL_PREEM_RENDERER=cpu` in the unit's environment and confirm the
     scope draws the CPU kit's own picture. That identity is byte-checked in
     CI (`the_cpu_arm_still_emits_the_kits_own_bytes_as_a_pixels_node`, plus
     every existing `*_renders_at_parity_with_the_kit` test, which all run on
     the CPU arm by default). What only glass can confirm is that the switch is
     actually _read_: the variable is consumed once at the first `Scope`
     build, so it has to be in the unit's environment, not just your shell's.
  3. **A bar's worth of scopes.**
     `cargo run -p hytte-ui --example gl_probe -- --layer --areas 8` (stage A's
     probe, on layer-shell): **jank 0**, and p95 within 0.5 ms of the 16.67 ms
     idle baseline. The number to beat is stage A's `gl-x3` layer result,
     16.77 ms. `--areas 3` was all stage A measured, so this is the
     extrapolation being checked rather than re-confirmed.
  4. **The parity numbers, on your driver.** `preem_gl_diff` prints a
     per-channel mean / p99 / max against the CPU kit for each skin at three
     points in the fade, plus a per-column peak-row structural check, the worst
     pixel's coordinates and channel, an edge/field/lit region split, and three
     netpbm images per case under `gates/` (`pnmtopng` them to look). The
     ceiling is **mean ≤ 2 / p99 ≤ 8 / max ≤ 32** of 255 (#893, Annika's
     answer 4). Under llvmpipe every case reads **0 / 0 / 0** — the span-quad
     design does hold, exactly — so on real hardware anything above zero is
     worth reading rather than shrugging at, and the region split says which of
     #1072's four buckets it is: only on edges is rasterisation coverage, flat
     across the field is gamma/sRGB, in the lit interior is shader math. It
     runs headless too, which is how the #1072 numbers were taken:

     ```sh
     nix develop --command bash -c '
       MESA=$(nix build nixpkgs#mesa --no-link --print-out-paths)
       export LIBGL_ALWAYS_SOFTWARE=1 GDK_BACKEND=x11 \
              LD_LIBRARY_PATH="$MESA/lib:$LD_LIBRARY_PATH" \
              __EGL_VENDOR_LIBRARY_FILENAMES="$MESA/share/glvnd/egl_vendor.d/50_mesa.json"
       xvfb-run -a cargo run -p trollshell --example preem_gl_diff'
     ```

     `__EGL_VENDOR_LIBRARY_FILENAMES` is the one that is definitely
     load-bearing: glvnd's default vendor directories are
     `/usr/share/glvnd/egl_vendor.d` and `/run/opengl-driver/share/…`, neither
     of which exists in a nix sandbox, so without it `eglInitialize` finds no
     vendor at all (the #1036 spike's finding). `LIBGL_DRIVERS_PATH` was in the
     spike's recipe and is **not** kept here — #1072's review ran it as
     `$MESA/lib`, as `$MESA/lib/dri` and omitted entirely, and all three pass;
     it is the GLX-era knob and EGL resolves without it. The other three have
     not been bisected individually. Exit status is the verdict — it is `1` on
     any failure, including `FAIL(nothing)`: a GL arm that drew literally
     nothing, which the deltas alone cannot catch against a dark skin.

  5. **CPU and GL side by side.** Two shells cannot share the session, so do it
     in sequence on the same preem-demo card and compare screenshots — or put a
     GL scope next to a CPU-only kit widget (the gauge, which has no GL arm in
     this PR) and check the skin reads as one device: same field, same ink,
     same bloom character.
  6. **Two monitors.** A scope on both outputs accumulates its phosphor twice,
     once per `GtkGLArea` — accepted and documented (#893, answer 3). Fed the
     same batches they stay visually equivalent; a monitor that was unmapped
     and remaps resumes from its own trail and converges within the settle
     window (17 steps at the default persistence). What must _not_ happen is
     the animation running at double speed: the renderer instances are shared
     across outputs even though the surfaces are not.
  7. **The fallback, if you can provoke it.** Force a context failure (a
     session with no GL, or a stack that refuses `LIBGL_ALWAYS_SOFTWARE`) and
     confirm the scope falls back to the CPU kit with one journal line rather
     than showing a blank chip. The phosphor restarts from black, which is the
     honest outcome — the GL arm never drew a trail to inherit.

- [ ] **(#893)** **The shader widget: a plugin's own GLSL on the GPU.** A
      plugin ships a fragment body plus a data buffer; the shell compiles the
      body once and per frame re-uploads only the buffer. Everything up to the
      draw call is gated in CI — the wire round-trip, the capability check, the
      two size caps, the per-axis grid cap (#977), the kill-switch refusal
      (#978), the program-reuse rule, the six reconciler sites, and the demo's
      own shader body compiled by `nix/lint-glsl.py`. The draw, again, is
      yours.
  1. **It animates on glass.** Open `hytte-plugin-preem-demo`'s card: directly
     under the `Scope` there is now a second tile of the same shape, drawn by
     the plugin's own `spectrum.frag` — bars from the audio spectrum, a cap line
     at each band's top, and a beam sweeping across every four seconds. Play
     something: the bars must move with the audio and the beam must cross
     smoothly. In silence the beam steps once a second, and **that is the
     contract, not a bug** — a shader surface renders when its plugin pushes
     state and at no other time, so the motion rate _is_ the push rate
     (~20 Hz with audio, 1 Hz on the clock heartbeat alone).
  2. **It is themed.** Change the desktop accent with the card open: the warm
     half of the shader's colour cycle and its cap lines must move with it, with
     no plugin restart — the theme bag is rebuilt on every mapping pass, like
     every other preem widget's palette. `u_bg` / `u_fg` / `u_accent` /
     `u_success` / `u_warning` / `u_error` are the published set.
  3. **A broken shader shows the placeholder, once.** Edit
     `crates/hytte-plugin-preem-demo/shaders/spectrum.frag` to something that
     cannot compile (`fragColour` for `fragColor` is enough), rebuild and
     restart just that plugin. The tile must go **empty** — an empty rect where
     it was, the rest of the card untouched — and
     `journalctl --user -u trollshell | grep -i "shader"` must show **exactly
     one** line carrying the driver's first error line. Not one per frame: the
     failed source is latched by its own hash, so the driver is asked once. Put
     the file back afterwards. (`nix flake check` will refuse the broken
     version, which is the point of the `glsl` check — build the plugin
     directly with `cargo build -p hytte-plugin-preem-demo` for this.)
  4. **The same source does not recompile.** With the tile running, watch
     `RUST_LOG=hytte_ui=debug journalctl --user -u trollshell -f`. Every actual
     compile emits **one** `compiling a plugin shader` line carrying a
     `source_key`, so the evidence is countable rather than an absence. The unit
     is **one per surface**, not one per shell: the program cache "lives on the
     instance and dies with it" (`shader_surface.rs`), and `build_node` mounts
     one `ShaderSurface` per monitor, so a healthy two-monitor shell prints
     **two** and each unmap/remap of the tile adds one more. What must not
     happen is a line per _frame_: across thousands of frames of an unchanged
     shader the count must not move at all, and editing the body and restarting
     the plugin must add exactly one per live surface, with a _different_ key.
     (This check asked for "exactly one" across the whole shell until #978
     re-read it; on a two-monitor session that gate failed on a correct shell.)
     The tile must not flicker as frames arrive,
     and the beam must not jump back to the left edge — a rebuilt widget
     restarts `u_time`, which is what that would look like. The hermetic half is
     `the_same_source_is_compiled_once` in
     `crates/hytte-ui/src/shader_surface.rs`; glass adds that the reconciler
     really is reusing the widget.
  5. **The capability really gates it.** Delete
     `m.capabilities = vec![Capability::Shader];` from the demo's `manifest()`,
     rebuild and restart the plugin: the tile must become the same empty
     placeholder, the rest of the card must render normally, and the journal
     must carry **one** line naming the missing capability. This is the whole of
     #893's trust boundary that is enforced — the socket's own file mode is the
     rest of it (route 0), and there is no source validator by design.
  6. **A shader hanging the GPU takes the shell with it.** Stated so it is not
     discovered: GTK requests no robust context and there is one GL share group
     per display, so a runaway plugin shader is the same trust as the plugin's
     own native code and the realistic worst case is a shell restart. Nothing to
     verify — this is the risk being accepted, with route 3 (an out-of-process
     shader host) named as the upgrade if a plugin is ever not trusted. The same
     goes for a **failed** context: the latch is sticky for the process and the
     shader widget has no CPU arm, so every shader stays a placeholder until the
     shell restarts (`Refusal::NoGl` says why). Since #981 that line has its own
     warn-once slot (`Warned::ShaderNoGpu`), so an earlier plugin-side refusal
     can no longer swallow it — before that, one plugin shipping one over-cap
     source at startup spent the slot for the shell's whole run and a context
     failure an hour later wrote nothing at all.
  7. **Alpha is premultiplied — the one contract clause no test can reach.**
     Everything else on this page is either gated hermetically or visible in the
     demo; this is neither, because `spectrum.frag` is opaque. Replace its body
     temporarily with two lines:

     ```glsl
     void main() { fragColor = vec4(0.5, 0.0, 0.0, 0.5); }
     ```

     `cargo build -p hytte-plugin-preem-demo`, restart the plugin, and look at
     the tile against the card behind it. **Premultiplied is what the contract
     says**, so this must read as an evenly half-transparent red — the card
     showing through. If it reads as _full_ red (or as a dark, muddy red), the
     framebuffer is being treated as straight alpha and the contract wording in
     `Node::Shader`'s docs, `hytte-ui`'s `shader_surface` module docs and the
     SDK's `shader` module docs is wrong in the same three places — fix the text,
     not the shader. Then put `spectrum.frag` back.

     Stated plainly: this was read off GDK's memory format
     (`GDK_MEMORY_DEFAULT`), not off a screen. It is the claim in this PR with
     the least evidence behind it.

  8. **(#978)** **The kill switch turns plugin shaders off too.** This is the
     one item on this page that #893's own list carried and the page then lost:
     the spec calls `TROLLSHELL_PREEM_RENDERER=cpu` "the kill switch, forcing
     CPU regardless of GL availability", and until #978 the one widget in the
     shell running a _plugin's_ GPU code was the one widget that ignored it.
     Restart the shell with `TROLLSHELL_PREEM_RENDERER=cpu` in the **unit's**
     environment (`systemctl --user edit trollshell`, not your login shell — the
     variable is read once, at the first `Scope` build) and open the
     preem-demo card. The `Scope` chip must still draw, on the CPU kit
     (that is check 2 of the entry above). The spectrum tile directly under it
     must be **empty** — the broken-widget placeholder — and
     `journalctl --user -u trollshell | grep -i 'kill switch'` must carry
     exactly one line naming `TROLLSHELL_PREEM_RENDERER`. The negative half is
     the part only glass can show:
     `RUST_LOG=hytte_ui=debug journalctl --user -u trollshell | grep 'compiling a plugin shader'`
     must be **empty** — no `GtkGLArea`, no GLES context, no plugin GLSL
     compiled anywhere in the process. Unset it and restart: the tile comes
     back and the line reappears.
  9. **(#977)** **A data grid too wide for the GPU is refused, loudly.** Only
     glass has a driver. Patch the demo plugin to send a 1-D grid over the
     per-axis cap — in `crates/hytte-plugin-preem-demo`, set the shader node's
     `data_width` to `32768` with a matching `vec![0u8; 32768]` `R8` buffer
     (32 KiB, three orders of magnitude _under_ the 4 MiB byte cap, which is the
     whole point) — then `cargo build -p hytte-plugin-preem-demo` and restart
     just that plugin. The tile must go empty and
     `journalctl --user -u trollshell | grep -i 'per-axis cap'` must carry one
     line naming `data_width=32768` and `cap=4096`. Before #977 this was the
     silent case: every host check passed, `glTexStorage2D` failed with
     `GL_INVALID_VALUE`, `Texture::new` returned `Ok` on a texture with no
     storage, and the widget sampled black for its whole life with **zero**
     journal lines. To see the second half — the driver check rather than the
     host cap — raise `MAX_SHADER_DATA_EXTENT` past your part's
     `GL_MAX_TEXTURE_SIZE` (`glxinfo -l | grep GL_MAX_TEXTURE_SIZE`, typically 16384) and repeat with a grid between the two: the line then comes from
     `hytte-ui` instead, saying the data texture could not be allocated and
     carrying the driver's own limit. Put both back afterwards.

## Screen recording

- [ ] **(#458)** Rebuild the NixOS/home-manager config with `wf-recorder` +
      `slurp` provisioned; confirm `which wf-recorder && which slurp` on the
      target session's `PATH`.
- [ ] **(#458)** Trigger `toggle-recording` (bar chip or the `toggle-recording`
      GAction) — confirm a region-select prompt appears and an `.mp4` lands in
      `$XDG_VIDEOS_DIR`/`~/Videos`.
- [ ] **(#458)** Toggle "Record audio" in the Settings drawer and confirm the
      _next_ recording's file has an audio track (e.g. `ffprobe`); a
      recording already in progress should be unaffected by the toggle.
- [ ] **(#523)** Click the record chip when `slurp`/`wf-recorder` are missing
      from the systemd-user PATH (e.g. a deploy predating #458's provisioning)
      — expect a **Critical-urgency toast** naming exactly what's missing
      ("slurp isn't installed or isn't on PATH"), not silent nothing.

## Shell chrome (regression-only, pixel-identical refactors)

- [ ] **(#494)** Drawer visibility gating: the triplicated gate pattern was
      extracted into `components/visibility_gate.rs` and adopted by
      `modal.rs`. Confirm drawer show/hide gating is unchanged (no new
      flicker, no page staying mounted/unmounted incorrectly).
- [ ] **(#630)** `modal::close_all` no longer holds the `PANELS` `RefCell`
      borrow across each `window.destroy()` call — a latent reentrant-borrow
      hazard fix, not a behavior change. **Corrected (#660):** this entry
      used to say `window.close()`; #637 (below) changed that teardown call
      to `destroy()` after #630 had already merged, so the entry cited a call
      the code no longer makes. There is no new behavior to click through
      here, and the honest verification is that **nothing changes**: monitor
      hot-plug with a drawer open still behaves exactly as before, including
      mid-retract, and the #624 `reset_drawer_open_states()` ordering still
      runs after every panel is dropped. Absence of any observable
      difference is the pass condition, not a ritual to perform.
- [ ] **(#637)** All five per-monitor overlay `close_all` functions
      (`modal.rs`, `overlays/{sidebar,frame,osd,notifications}.rs`) now tear
      down with `gtk::Window::destroy()` instead of `close()`. `close()` is a
      _request_ routed through `close-request` and doesn't drop GTK's
      internal toplevel reference on a window that was never realized — these
      layer-shell windows are built but never shown until first opened
      (`modal.rs`'s `EAGER_PAGES` is empty), so a drawer/overlay never opened
      on a given monitor survived a hot-plug `close_all` under the old code,
      leaking its widget tree and (via `plugins/region.rs`'s
      `connect_destroy`) its plugin-panel reconcile subscription. On a
      two-monitor Niri session: hold a `glib::WeakRef` to (or otherwise
      track) a drawer/sidebar/frame/OSD/toast window that has never been
      opened on the secondary output, force several kanshi profile switches
      (hot-plug/hot-unplug cycles), and confirm the tracked `WeakRef` no
      longer upgrades after the corresponding `close_all` — or, short of
      instrumenting a `WeakRef`, that `gtk::Window::toplevels()`'s count
      doesn't grow per cycle for surfaces that were never opened. Also
      confirm plugin panels mounted in the sidebar/drawer still reconcile
      correctly (respond to plugin state changes) after several such
      switches.
- [ ] **(#639)** The three remaining `close()`-teardown sites in `hytte-ui`
      itself now use `destroy()` too, for the same reason as #637:
      `bar.rs`'s `BarHandle::close()` and its `Drop` impl, and `popup.rs`'s
      dismiss-catcher teardown in `close_catchers`. A **lack of change** is
      the pass condition: bars on removed monitors should still disappear on
      multi-monitor hot-plug/kanshi switches, rebuilt bars on
      remaining/re-added monitors should behave normally, and popup
      dismiss-catchers (click-outside, Escape, autohide, scroll) should still
      disappear on every output with no leftover invisible click-eater.
- [ ] **(#644)** Nine more `RefCell` borrows released before the GTK call
      that could re-enter them, all inside `modal.rs`/`overlays/`: the four
      sibling `close_all`s (`sidebar.rs`, `frame.rs`, `notifications.rs`,
      `osd.rs`) now `take()` their map before calling `destroy()` on each
      entry (same shape #630 fixed for `modal.rs`); `prompt.rs`'s
      `close_prompt` and `consent.rs`'s `close_all`/supersede-on-`request`
      bind the taken value before acting on it; and two more in `modal.rs`
      itself — `reset_drawer_open_states` (no longer holds `DRAWER_OPEN`
      borrowed across each `Mutable::set_neq`) and `install` (the replaced
      panel now drops after the `PANELS` borrow ends, not inside it). Same
      honest verification as #630: **nothing changes**. Hot-plug/kanshi
      switches with a drawer, sidebar, frame overlay, OSD toast, or
      notification open (or mid-retract) should behave exactly as before;
      the consent prompt's Allow/Deny/timeout flow and a second `request()`
      superseding a pending one should be unaffected.
- [ ] **(#663)** Twenty-two more sites with the same `RefCell`-across-GTK-call
      pattern, this time outside `overlays/`: `hytte-ui`'s `popup.rs`
      catcher teardown, the shared `components/reactive_list.rs` helper
      (backs `panels/{appearance,vpn,clipboard,displays,stats}.rs` and
      `panels/network/{wired,wifi,connection}.rs` — eight panels),
      `components/power_profile.rs`, `panels/stats.rs` (top-apps expander,
      per-app rows, live per-core bars), `widgets/{calendar,tasks}.rs`
      (upcoming-list/task-list rebuilds, day-click highlight, create-popover
      list sync), `panels/connections.rs`, and the control-center's
      `apply_plugins`/`clear_rows`. Same pass condition as #630/#644:
      **nothing changes**. Given the breadth, worth an eyeball on more than
      one panel — open the Appearance/VPN/Clipboard/Displays/Stats drawers
      and the Wired/Wi-Fi/Connections network sub-pages, add/remove a
      calendar or task entry, and open the control-center's Plugins tab and
      toggle a plugin — all list add/remove/rebuild behavior should look
      identical to before. The one site in this PR with an actual
      synchronous-reentrancy proof (not just an unverified hazard) is
      `popup.rs`'s `close_catchers`, confirmed by a new gated test that
      reproduces the pre-fix `SIGABRT` — nothing extra to click through for
      that one beyond the regular dismiss-catcher check already covered by
      #639 above.

## Stats drawer

- [ ] **(#518)** The five-card Stats drawer (CPU/Memory/Disks/GPU/Services) is
      back to one combined page. Any of the CPU/memory/disk/GPU/services bar
      chips should open the full stacked drawer, and switching chips while the
      drawer is already open on a different page should swap cleanly.
- [ ] **(#547)** Scroll-to-section: opening the drawer from a specific
      resource chip should land that card at the **top** of the (now
      scrollable) drawer. Clicking a _different_ resource chip while the page
      is already open should jump to that card instead of closing the drawer;
      re-clicking the same chip re-applies the same target (harmless no-op).
- [ ] **(#564)** `TROLLSHELL_STATS_LAYOUT` / `programs.trollshell.stats.layout`
      selects between three shapes — `combined` (stacked), `multicolumn`
      (2-column grid), `split` (#307's five separate pages). Set each and
      confirm it renders correctly and the deep-link scroll from #547 still
      works in both `combined` and `multicolumn`.
- [ ] **(#568)** The **default** layout is now `multicolumn` (was `combined`)
      — with `TROLLSHELL_STATS_LAYOUT` unset, the Stats drawer should open
      straight into the 2-column grid.
- [ ] **(#582)** Panel card order now matches the bar's chip order — **CPU,
      Memory, GPU, Disks, Services** (was CPU, Memory, Disks, GPU, Services).
      In all three layouts, clicking the **GPU** bar chip should land on the
      **GPU** card, not Disks. On GPU-less hardware (or with `sensors::gpu()`
      faked to return `None`), confirm the multicolumn grid's row 2 has no
      visible hole — Disks should slide into column 0 when the GPU card hides
      itself.
- [ ] **(#857)** **Blinken Lichten** — the CPU card's per-core row is now an
      LED **panel** (one lamp per core, each lit to that core's load) instead
      of a `GtkFlowBox` of vertical progress bars. This is a look-and-feel
      feature: CI can only check byte patterns, so everything below wants
      eyes.
  - Open the Stats drawer's CPU card. Expect a grid of glowing lamps that
    visibly breathes with load — hammer a core (`yes >/dev/null`, one per core
    you want lit) and watch that lamp go from blue toward red on the default
    `heat` map. The panel is centred in its row. **Its shape changed after the
    first pass** — see the #857 rectangle entry below.
  - **The #702 check.** Drag the drawer / shrink the output as narrow as it
    goes. The panel must **letterbox down**, never force the drawer wider. Its
    reported minimum width is 0 px at any core count (one `PixelSurface`, whose
    `measure` hard-codes a 0 minimum, replaces 64 bars each with an 8 px CSS
    floor). If the drawer's minimum width grew, this regressed.
  - **The colour axis is orthogonal to the skin.** In
    `~/.config/trollshell/core-leds.toml` set `style = "crt"` **and**
    `color = "heat"` and save (or restart): expect heat-coloured lamps
    _through_ the CRT's scanline comb and curved-glass vignette — both at
    once, not one instead of the other. That composition is the whole design
    claim of #857 and the single most valuable thing to eyeball.
  - Sweep the four knobs. **Since #869 these live in
    `~/.config/trollshell/core-leds.toml`** (`style` / `color` / `rows` /
    `fill`), where an edit takes effect **live** — see the #869 entry below.
    The four `TROLLSHELL_CORE_LEDS_*` variables that used to carry them are
    **gone** (#1041 step 3): setting one now does **nothing** to the resolved
    value and logs exactly **one** `tracing::warn` at startup naming the file
    key to use instead — see the #1041 entry below for that line's exact
    shape.
    - `style` = `vfd` (default) / `lcd` / `oled` / `crt`
    - `color` = `heat` (default) / `style` / `rainbow` / `transpride` /
      `#rrggbb`. `style` should give the plain single-ink panel — the
      pre-#857 look, and the guarantee the byte-identity tests pin.
    - `rows` = `0` or `"rect"` (default — a **wide rectangle** since the
      second #857 pass, near-square before it) or a row count **from 1 to
      64**. `3` on a many-core box makes a wide, short strip — check it does
      not push the drawer wider (it is 247 px at 1× on a 64-core box, and the
      scale deliberately refuses to blow it up past the budget). `65` (or a
      negative value) is rejected with one warning and falls back to the
      built-in default: the budget box shows nine rows at 1×, so a bigger
      number is a typo, and an unbounded one used to ask for a
      multi-gigabyte frame.
    - `fill` = `spare` (default) / `blank` — only visible when the last row
      is ragged **and** the skin ghosts, so pair it with `rows = 3` and
      `style = "lcd"` (or `"vfd"`). `spare` shows unlit lamps filling the
      tail, `blank` leaves the gap bare. On `oled`/`crt` the two are
      identical by construction (no ghost to differ on).
  - Hover the panel: the tooltip should read
    `N cores · avg X% · max Y% (core K)`. This **replaces** the old per-bar
    `"42%"` tooltip — the per-lamp readout is gone (a pointer-precise version
    needs the inverse of `PixelSurface`'s letterbox transform; noted as a
    follow-up).
  - Sanity: on a 1-core VM / container the panel is a single blown-up lamp,
    not an empty row. Verified only against synthetic level slices in tests.
- [ ] **(#857, second pass)** **Two-column Stats grid + a rectangular LED
      panel.** Annika's on-glass verdict on the first pass was "default looks
      weird now. Too much free space. I guess this can now be regular two
      column flexbox. no need for stretched cpu anymore. rectangle for led view
      would be still more preem tho." Both halves are look-and-feel, so both
      want eyes.
  - **The grid.** Open the Stats drawer (multicolumn is the default layout).
    The CPU card must **no longer span both columns**: expect `CPU | Memory`
    on the first row, `GPU | Disks` on the second, and Services alone across
    the third — #582's bar-chip reading order, restored. The empty space to
    the right of the LED panel that prompted the complaint should be gone. The
    drawer should be **no taller** than before (still three rows).
  - **The GPU-hidden reflow, again.** This is the same check as the #582 entry
    above and it is worth redoing, because the arrangement it protects moved.
    On GPU-less hardware (or with `sensors::gpu()` returning `None`), the GPU
    card hides and **Disks must slide left into column 0**, leaving the empty
    cell at the _right_ edge of that row rather than as a hole between CPU and
    Disks. `hiding_the_gpu_card_leaves_no_hole` asserts this headlessly, but
    only the eye can confirm the column widths still look right.
  - **The rectangle.** The panel is now meaningfully wider than tall at every
    core count — 16×4 on a 64-thread box (181×49 px at 1×), 8×2 at 16 cores,
    4×1 at 4. It is picked automatically from the core count, so on a machine
    with a different core count than the one that shipped this, confirm it
    still reads as a rectangle rather than a square or a hairline. Heights now
    run 49–96 px (they used to run 70–105) — shorter is the point.
  - Deep-links still follow the cards: click each of the five bar chips in
    turn and confirm each lands on **its own** card at the top of the
    viewport. The scroll is coordinate-based (`compute_bounds`), so it should
    be indifferent to the rearrangement — this check exists to prove that,
    not because a break is expected.
  - A pinned `rows = 8` in `core-leds.toml` still overrides the automatic
    shape (and on a 64-core box gives back roughly #861's square). `"rect"`,
    `0`, or an absent key is the new rectangle.
- [ ] **(#869)** **`core-leds.toml` — the config-file pilot.** Phase 1 of
      #866: the LED panel's four knobs are the first subsystem read through
      the #868 layering — `places.toml` has reloaded live since long before
      this, but it goes through none of the layering, so this is the shape the
      other nine subsystems copy. Everything below wants a live shell; nothing
      about it can be judged headlessly.
  - **The payoff, in one move.** With the shell running and the Stats drawer
    open, create `~/.config/trollshell/core-leds.toml` containing
    `style = "crt"` and save. Within ~3 s the panel should re-skin to the CRT
    tube — scanlines and vignette — with **no restart**. Now add
    `color = "transpride"` on a second line _beside_ it (don't replace the
    file, or `style` reverts to VFD in the same reload and the CRT check will
    look like it regressed), save, and watch the lamps re-band while the
    scanlines stay. This is the whole point of the phase; if it needs a
    restart, the pilot failed.
  - **A missing file is silent.** With no `core-leds.toml` anywhere, the panel
    must look exactly as it did before #869 (VFD skin, heat map, automatic
    rectangle, spare fill) and the journal must carry **no** config warning at
    all. First run must not complain about an absent config.
  - **The documented default reads well.** Nothing writes your overlay yet
    (seeding it on a first save is #888's business), so the commented default
    lives in `CoreLedsConfig::DEFAULT_TOML` in
    `trollshell/src/config/core_leds.rs` — copy it into your overlay as a
    starting point and check that the comments actually tell you what to type,
    since that block is the only place a key is explained.
  - **(#1041) The environment used to win — now it does nothing, once,
    loudly.** Start the shell with `TROLLSHELL_CORE_LEDS_STYLE=oled` while
    `core-leds.toml` says `style = "crt"`. Expect the **CRT** panel — the
    file's own value, never the variable's, regardless of whether the
    variable's value would ever have parsed under the old scheme — and
    **exactly one** journal line of this shape, with the real resolved path in
    it, not a literal `~`:

    ```text
    TROLLSHELL_CORE_LEDS_STYLE does nothing any more; set `style` in /home/annika/.config/trollshell/core-leds.toml instead — it accepts one of vfd/lcd/oled/crt
    ```

    One for the life of the shell, not one per reload: edit and save the file
    a few times with the variable still set, confirm the panel keeps tracking
    the file (never OLED) and the line does **not** repeat. Then try
    `TROLLSHELL_CORE_LEDS_STYLE=plasma` (a value nothing ever accepted) and
    confirm the line and the behaviour are identical — there is no separate
    "unusable value" case any more, since the value is never read. Finally
    unset the variable and restart: the journal must carry **no** line for it
    at all.

  - **A file caught mid-edit keeps the last good skin.** With the shell
    running and a working `core-leds.toml`, save a deliberately broken one —
    `style = "crt` with the closing quote missing, i.e. bytes that are not
    TOML. The panel must keep rendering the **last good** skin, not snap back
    to the default, and the journal gets one warning per save (not one per
    poll) — leave the file broken for a minute with `journalctl -f` open and
    confirm the line does **not** come back every ~3 s. The same holds for the
    per-key line two bullets down: one per save, whichever kind of mistake it
    was. Repair the file and save: the panel picks it up again. Note this
    bullet is _only_ about bytes that are not TOML; a file that parses with one
    unusable value is a different case, two bullets down.
  - **Deleting the file gives the defaults back.** With a working
    `core-leds.toml` applied, `rm` it. Within ~3 s the panel must return to the
    built-in look (VFD, heat, automatic rectangle, spare fill) — a delete is an
    intent, not a mistake, and it is the only way to get the stock look back
    without hand-restoring every key. Note the asymmetry with the line above:
    a file caught mid-edit keeps the last good skin, an absent one does not.
  - **An unknown key is loud and harmless.** Add `colour = "rainbow"` (British
    spelling) alongside a valid `style`. Expect **one** `unknown key in config`
    warning naming `colour` (one, not two — the config is loaded exactly once
    per startup), the panel unchanged in colour, and the `style` beside it
    still applied.
  - **`rows` takes both spellings, and is capped.** `rows = "rect"` — the word
    the deprecated variable took — must work exactly as `rows = 0` does.
    `rows = 65` is rejected; the budget box shows nine rows at 1×, so anything
    past 64 is a typo rather than an intent.
  - **A bad value costs its own key, and only its own key.** Put
    `style = "crt"` and `rows = "many"` in the file together and save. The
    panel must go **CRT** with the automatic rectangle — the good key applied,
    the bad key back to its built-in default — and the journal must carry
    exactly one line, naming `rows` and saying what happened to it:

    ```text
    rows = "many" is not valid; expected 0 or "rect" for the automatic rectangle, or a row count from 1 to 64 — ignoring this key and using the built-in default
    ```

    Not a whole-file failure that drops the panel to stock VFD with `style`
    silently gone. The same holds for `rows = 65`, `rows = true`, `rows = 4.0`,
    `color = "puce"` — anything a known key holds that no parser takes — and,
    since #1040 T1, for a value of the wrong **type** on any key too: try
    `style = 5` or `color = 0xff0000` (a hex colour _is_ a number, after all)
    beside a good key and expect the same one line naming the one key, quoting
    the value as TOML holds it. What is still whole-file is a file that is not
    TOML _at all_ (an unterminated string, or an integer too big for TOML's i64
    like `rows = 9223372036854775808`) — the "caught mid-edit" bullet above.

  - **The base layer, nix-rendered (#1041).** Set
    `programs.trollshell.config.core-leds.style = "lcd";` (home-manager or the
    NixOS module — see `nix/module-common.nix`'s description for the full
    field list) and rebuild. With no overlay file at all
    (`~/.config/trollshell/core-leds.toml` absent), the panel must come up
    **LCD** with no restart of the shell needed beyond the one the rebuild
    itself does — the base layer is read at startup the same as any other
    layer. Then create a bare overlay
    (`echo 'style = "crt"' > ~/.config/trollshell/core-leds.toml`) and confirm
    it beats the nix-set base live within ~3 s, while a key only the base
    states (e.g. `color`, if you set one) still applies through the overlay.
  - **Whether a later rebuild reaches an already-running shell depends on
    which module rendered the base file (#1041 M5).** The NixOS module writes
    `/etc/xdg/trollshell/core-leds.toml` as a symlink that `nixos-rebuild
switch` repoints atomically to a new store path every rebuild; the running
    shell keeps re-reading that same stable path, so — now that the watcher's
    stamp hashes content instead of trusting a nix-store file's permanently
    frozen mtime (below) — a `nixos-rebuild switch` alone, no shell restart,
    reaches the panel within ~3 s/~15 s. home-manager is different:
    `programs.trollshell.config.core-leds` renders into a _fresh_ nix-store
    directory every `home-manager switch`, and that path is baked into the
    trollshell unit's own `Environment=` at process start — the same reason a
    changed `TROLLSHELL_*` value needs a restart under `systemd.enable`
    (#568) — so a home-manager rebuild needs
    `systemctl --user restart trollshell` to reach a running shell; only a
    hand-edited **overlay** reloads live under that module.
  - **Battery-aware reload cadence (#1041).** On battery, the poll cadence
    stretches from 3 s to 15 s (`trollshell/src/config/core_leds.rs`'s
    `BATTERY_CONFIG_POLL_INTERVAL`). Unplug, wait for the battery chip to
    confirm `on battery`, then edit `core-leds.toml` and save: the panel
    should still re-skin, but expect up to ~15 s rather than ~3 s before it
    does. Plug back in and the very next edit should be back to ~3 s. A
    laptop with no battery, or with
    `programs.trollshell.enableRecommendedServices = false` (no UPower),
    should stay at the AC cadence the whole time — `upower::on_battery_now`
    degrades to "on AC" whenever the real state isn't known, never to the
    slower one.
  - **The one edit the poller used to be able to miss, fixed (#1041 M5).** The
    watcher's stamp is a modification time _and a content hash_ — no longer
    just a byte length — because a same-length edit landing inside one mtime
    granule (`style = "vfd"` → `style = "lcd"`, 14 bytes either way) used to be
    invisible to it, permanently, and every file the nix base layer renders
    carries the _same frozen mtime_ regardless of granule (every Nix store
    file's mtime is the constant `1970-01-01T00:00:01Z`, measured). Confirm by
    hand: with the shell up, save `style = "vfd"` then immediately
    `style = "lcd"` — same byte count both times — and the panel must re-skin
    within ~3 s. What is still, honestly, not covered: two edits landing in
    one granule that also **hash** identically (a content collision, not a
    length one) — astronomically unlikely for a four-key TOML file, not
    mathematically impossible, and a real inotify watch remains the eventual
    answer for that residue.

- [ ] **(#862)** **Accent tracking for the shell's own preem surfaces** — the
      Stats drawer's per-core LED panel is rasterised in-process, and until
      #864 nothing called `hytte_preem::set_accent`, so it drew with the kit
      default while every out-of-process plugin board correctly followed the
      desktop accent. Open the Stats drawer and change the desktop accent: the
      LED panel's lit lamps should re-tint to match, live, without a shell
      restart — the fix sits in `publish_accent`, which is also #396's live
      re-tint funnel, so startup and live re-tint are covered by the same call.
      Compare against a plugin board (e.g. the preem demo's), which has always
      tracked — the two should now agree.
- [ ] **(#722)** Flapping supervised tasks appear in the **Stats → Services**
      card. Make a user unit restart-loop (a `Restart=always` unit whose
      `ExecStart` exits non-zero will do) and confirm **both** halves, since
      the fix deliberately shipped them together: a second group lists the
      task with its consecutive-panic count, **and the bar's services chip
      becomes visible even though no unit is in the `failed` state** — the
      chip's predicate widened from "no failed units" to "no failed units
      _and_ no flapping tasks", so before this a flapping-only system showed
      nothing at all. Stop the loop and confirm both the entry and the chip
      disappear again.
- [ ] **(#701)** The Stats drawer scrolls only when it genuinely runs out of
      screen. Both scrolling layouts used to cap their viewport at a hardcoded
      560 design-baseline px, which `scale()` could not rescue — the page
      content rides the same font factor, so the ratio was font-invariant and
      the drawer scrolled even on a tall monitor with room to spare. On a
      **tall** output the Stats page should now show its content with no
      scrollbar; on a **short** one it must still scroll rather than overflow.
      Worth checking at two font sizes, and on a second output of a different
      height if you have one — the cap is derived per live monitor now, so it
      should differ between them.

## Weather & location

- [ ] **(#532)** A weather card sourced from a raw GeoClue fix with no
      configured-place match should now name the **same** location the
      control-center's Place tab shows for that fix (both read
      `places::shared_place()`) — they should no longer be able to disagree.

## Wallpaper & appearance

- [ ] **(#550)** Per-output wallpaper: Appearance drawer → set a **default**
      image, then set a different image on one connected output's
      **Per-display** row — confirm swaybg renders the per-output override on
      that monitor and the default elsewhere.
- [ ] **(#550)** Time-of-day rotation: enable rotation with morning/day/
      evening/night images set, and confirm the active slot's image renders
      and re-renders as the clock crosses a slot boundary (fixed local
      boundaries: morning 06–11, day 11–17, evening 17–21, night otherwise).
- [ ] **(#550)** Clear wallpaper: hit **Clear wallpaper** with nothing left
      selected — the swaybg unit should **stop** (not restart on an empty arg
      list).
- [ ] **(#552)** Rotation-empty-slot fallback: with rotation on and per-output
      overrides but **no** default image, let the clock tick into a slot with
      no image configured — the per-output wallpapers should stay on-screen
      rather than blanking.
- [ ] **(#552)** With `TROLLSHELL_WALLPAPER_RELOAD_CMD` set (a custom backend
      like `awww`), the **Clear wallpaper** button should be **disabled** with
      an explanatory tooltip, rather than silently no-op'ing or erroring.

## Notifications

- [ ] **(#569)** Hover-pause: pop a finite-timeout notification, put the
      pointer on it and **hold**. The countdown should pause while hovered and
      resume with the **remaining** time (not a full restart) once the
      pointer leaves. A toast dismissed/replaced while hovered shouldn't
      strand a paused timer on some other toast.
- [ ] **(#596)** The decisive regression case #569's fix broke and this PR
      re-fixes: `notify-send "A" "park the pointer here" -t 5000`, put the
      pointer on **A** and **do not move it**. From another terminal,
      `notify-send "B" "unrelated"` (or let any other toast arrive/expire).
      **A must not dismiss** while the pointer sits still. Move the pointer
      off A — it expires within roughly its remaining time. Also: a
      `notify-send -r <id>` update to the toast under a stationary pointer
      should update in place (same stack position, not re-appended to the
      bottom) and still not expire.
- [ ] **(#625)** The hover hold now survives a sticky/finite re-post — the
      third defect in this mechanism after #569/#596. `notify-send -t0 hold`,
      park the pointer on the toast and don't move it, then from another
      terminal `notify-send -r <id> -t -1 changed` (id from
      `notify-send -p`). The toast must update in place and **not** expire
      while the pointer sits on it. Move the pointer off — it expires
      roughly 5 s after the leave. Check the symmetric direction too (finite
      to sticky re-post under a parked pointer stays held, not expiring a
      few seconds later), and the ordinary regressions: an unhovered toast
      still expires normally, a plain hover still pauses and resumes with
      the remainder, and two monitors showing the same toast only resume on
      the last leave.

## D-Bus name ownership

- [ ] **(#668)** A contested well-known bus name — another daemon already
      owns it and refuses replacement, the shape of mako/dunst holding
      `org.freedesktop.Notifications` — now backs off and logs instead of
      retrying silently at ~4 `RequestName` calls/second forever. **Nothing
      changes in the UI yet**; this is `Refs #653`, not `Closes` — the
      visible bar tell is a separate, still-open follow-up. Verify with:

  ```sh
  systemctl --user start mako          # or dunst
  systemctl --user restart trollshell
  journalctl --user -u trollshell -f | grep -i 'D-Bus name'
  ```

  Expect exactly **three** warns within the first second or so
  (`consecutive=1`, `2`, then `3` latching `PermanentlyTaken` with
  `retry_in_secs=300`), each naming the actual holder (`holder=:1.NN`) via a
  best-effort `GetNameOwner` lookup — then **one warn every 5 minutes**, not
  a flood. **The slow cadence is the fix, not a bug**: if you then stop the
  squatter (`systemctl --user stop mako`) and trollshell doesn't reclaim the
  name for up to 5 minutes, that is expected — recovery went from ~250ms to
  up to 5 minutes as the direct cost of cutting `RequestName` calls from
  ~14,400/hour to 12/hour. Filing that delay as a regression would be
  exactly backwards. To confirm the call-rate drop itself directly:

  ```sh
  busctl --user monitor --match \
    "type='method_call',member='RequestName',arg0='org.freedesktop.Notifications'"
  ```

  should show 3 calls quickly, then one per 5-minute cooldown — not a
  continuous stream.

## Network panel (link status)

- [ ] **(#610)** _Unchanged path_ — on a host with a working NetworkManager or
      systemd-networkd, open the network drawer: Status reads **Online via
      `<iface>`** with the accent pill, "All links" shows the real interface
      count, and there is no "No connection" row. Now take the link down
      (`ip link set <iface> down`, as root) or unplug it: within ~5 s Status
      reads **Offline** with the muted pill and the "No connection" row
      appears — i.e. the word Offline still shows up where it is earned.
- [ ] **(#610)** _New path_ — on a host with **no link manager at all**
      (`systemctl stop NetworkManager systemd-networkd`, or a container /
      bridge-only box like #607's): Status must read **Unknown** with the muted
      pill and "No link manager (systemd-networkd or NetworkManager)" — **not**
      Offline. "All links" reads **Unknown**, not "0 interface(s)", and the "No
      connection" row stays hidden. The traffic card next door should still
      show the live interfaces, which is the whole point: the panel no longer
      contradicts it.
- [ ] **(#610)** Restart NetworkManager and confirm the card promotes to the
      real link and count within ~5 s. Caveat worth knowing before you call
      this a failure: as of **#634** the _backend probe_ no longer runs only
      once at startup — it retries at capped backoff while inconclusive, so a
      manager that was merely slow to answer (bus still coming up, a
      transient `ListNames` failure) is picked up without a restart. What is
      still deliberately untouched: a manager that appears **after** the
      probe has already committed to a verdict — e.g. installing
      NetworkManager mid-session on a host that booted with only iwd, or vice
      versa. That gap is now tracked as **#633**, not #613 — #613 is closed;
      #633 split off it as the genuinely-deferred half, and is blocked on a
      cancellation primitive `spawn_supervised` doesn't have yet.
- [ ] **(#610)** With `RUST_LOG=hytte_services=debug`, confirm no new log noise
      — `link_source()` uses `set_neq`, so it must not re-emit on every 5 s
      poll.
- [ ] **(#623)** The bar's network chip now honors `link_source`, not just
      `primary` — #610's fix relocated from the panel to the bar. On a host
      with no link manager at all (or before one has answered), the chip must
      show the dimmed `network-idle-symbolic` glyph — **not**
      `network-wired-disconnected-symbolic` — and it must look visibly
      different from the "no route" glyph. Hover the chip (not just the
      icon) — the tooltip must read "No link manager has answered yet" or
      "No link manager (systemd-networkd or NetworkManager)" as appropriate,
      matching the panel's Status row **verbatim** (both now share
      `link_status_text`). Also check the ordinary pre-DHCP `Degraded` state:
      bar and panel tooltips must agree there too. On a host with a working
      link manager, online/degraded/carrier/no-route/disconnected should all
      render exactly as before.
- [ ] **(#645)** A transient failure on the **first** `ListLinks` seed after
      `probe_link_backend` has already elected `LinkBackend::Networkd` no
      longer latches the panel at "no link manager has answered yet" for the
      rest of the process lifetime — previously curable only by
      `systemctl --user restart trollshell`. This is the networkd-side
      sibling of #634's wifi-backend-probe retry, not the same code path:
      #610/#623/#634 above cover the _backend-choice_ probe (NetworkManager
      vs. networkd vs. neither); this covers the first real `ListLinks` call
      once networkd has already been chosen. Provoke the race:
      `systemctl restart systemd-networkd` and, in the same moment,
      `systemctl --user restart trollshell` (or catch it early in a fresh
      boot's session, before networkd has settled). Confirm the network
      panel's link list populates **on its own, without a shell restart**,
      and that the journal carries both halves:

  ```sh
  journalctl --user -u trollshell | grep -E 'startup refresh (FAILED|RECOVERED)'
  ```

  Expect at least one `FAILED` line (`attempt=`, `retry_in_secs=`) followed
  by a `RECOVERED` line (`attempts=`). Can't be exercised by CI, for the same
  reason #634's wifi-side retry can't — nothing in `nix flake check` can make
  `ListLinks` fail and then succeed on demand. Sanity-check the two
  unchanged paths too: a normal boot where the first refresh succeeds should
  show **no** `startup refresh` lines at all, and a host with neither daemon
  should still log `networkd: no link backend available; service inert` once,
  promptly, with no retry lines.

## Wi-Fi (NetworkManager)

- [ ] **(#579)** Join a **never-before-seen WPA2 network** from the Wi-Fi
      panel — the passphrase prompt should now appear (it didn't before) and
      the join should succeed; `nmcli connection show` lists exactly one new
      profile named after the SSID. An **open** network connects with no
      prompt. A **previously-saved** network behaves exactly as before (no
      duplicate profile, no prompt unless NM asks for re-auth). A wrong
      passphrase still re-prompts.
- [ ] **(#586)** A **WEP** or **LEAP**-secured network's passphrase prompt now
      actually authenticates (previously the secret was nested under the
      wrong D-Bus key — `psk` — regardless of key-management type, so the
      join silently failed even with the right key typed). Rare hardware;
      flagged by the PR itself as possibly never getting a live check.
- [ ] _(sourced from #602's tracked list, not present in either #579's or
      #586's PR body)_ If a **WPA3-only** network fails to join, check `pmf`
      (protected management frames). Neither PR mentions PMF and no code in
      either touches it, so this looks like an environmental check against a
      real WPA3 AP rather than something #579/#586 specifically fixed —
      worth confirming on real hardware, but don't expect it to be explained
      by either PR's diff.
- [ ] **(#609)** A wireless backend probe that fails outright (e.g. the system
      bus briefly unreachable at shell start) no longer reads as "no wireless
      hardware". Force a transient `ListNames`/`ListActivatableNames` failure
      at startup (or catch it landing naturally on a slow boot) and confirm
      the logs show a `wifi: backend probe was INCONCLUSIVE` line — explicit
      that this is not the same as "no Wi-Fi daemon present". **Corrected
      expected outcome after #634:** do not expect a follow-up `error!`
      pointing at `systemctl --user restart trollshell` — the shipped retry
      policy is unbounded, so the probe keeps retrying at capped backoff
      instead of giving up, and a transient failure now self-heals into a
      `wifi: backend probe RECOVERED` line. Reading that recovery as a
      regression would be exactly backwards: the give-up-and-restart path
      still exists in the code (`ProbeStep::GiveUp`) but is unreachable under
      the shipped policy, so it should never actually fire live. The
      network-panel link list should still attempt NetworkManager rather
      than going permanently inert.
- [ ] **(#634)** The retry itself, end to end: start the shell while the
      system bus or NetworkManager is still coming up (e.g.
      `systemctl --user restart trollshell` right after
      `systemctl restart NetworkManager`, or early in a fresh boot's
      session). Confirm the Wi-Fi card populates **on its own, without a
      shell restart**, and that the journal carries both halves of the pair —
      the log pair is the whole point, not just the UI outcome:

  ```sh
  journalctl --user -u trollshell | grep -E 'backend probe (was INCONCLUSIVE|RECOVERED)'
  ```

  Expect at least one `INCONCLUSIVE` line (`attempt=`, `retry_in_secs=`)
  followed by a `RECOVERED` line (`attempts=`). This can't be exercised by
  CI — nothing in `nix flake check` can make `ListNames` fail and then
  succeed on demand — so it's a genuinely manual check. Sanity-check the
  negative case stays unchanged too: on a host with neither daemon,
  `Ok(None)` still commits immediately with a `no Wi-Fi backend present`
  warn and no retry lines. Also worth confirming as a side effect, not the
  headline fix: shell startup should no longer
  freeze while the probe works — pre-#634, `select_backend` ran
  `rt.block_on` on the GTK main thread, so a slow bus blocked the entire
  shell for the probe's duration (~10 s with the socket down, up to ~50 s
  against a wedged peer).

- [ ] **(#873)** In range of a **multi-AP SSID** (a mesh, a repeater, or a
      plain dual-band router advertising one name on 2.4 and 5 GHz), the
      Wi-Fi panel's scan list should show **exactly one row** for it, not one
      per BSSID, and the expander header should count networks rather than
      APs. The row must carry the **strongest** member throughout: its sort
      position, its signal icon and its `-NN dBm` subtitle all read that same
      number, and the subtitle gains a `· N APs` tail naming the group size.
      Cross-check against `nmcli -f SSID,BSSID,SIGNAL device wifi list`.
      Hidden APs (blank SSID) must still not appear at all, rather than
      collapsing into one blank row.
- [ ] **(#874)** While associated to a **non-strongest** member of such a
      group, the Wi-Fi card's description line must name the AP you are
      actually on. The concrete case: associated at −74 dBm with a −53 dBm
      member of the same SSID in range — the card must read `−74 dBm (ok)`,
      not `−53 dBm (good)`. The row itself is unchanged and must stay that
      way: it still sorts on −53 and still draws the −53 icon and subtitle.
      Confirm the associated BSSID with `iw dev <iface> link` (or
      `nmcli -f active,bssid,signal device wifi list`) rather than trusting
      the panel. On a single-AP network, or when you happen to be on the
      strongest member, nothing should look different from before.

## Night light

- [ ] **(#585)** Night light with **no** configured coordinates
      (`programs.trollshell.nightlight.{latitude,longitude}` unset) and
      GeoClue running: flip the switch on in the Appearance drawer — the
      screen should now actually warm (previously a no-op). With coordinates
      explicitly configured and GeoClue **not** running, the configured values
      should be what reaches `wlsunset`. With **neither**, toggling on should
      degrade cleanly: the unit stays inactive and the switch snaps back off.
- [ ] **(#595)** The race #585 left open: ensure **no** coordinates are
      configured and GeoClue is **cold** (`systemctl restart geoclue.service`
      immediately before, so the first fix is still in flight). Appearance
      drawer → flip **Night light on** → wait ~2 s (nothing visible happens,
      expected) → flip it **back off**. It must **stay off** — the switch
      must not move again, the screen must never warm, and
      `systemctl --user is-active wlsunset.service` must stay `inactive` past
      the 10 s coordinate-wait deadline.
- [ ] **(#598)** Night light gains a third `Resolving` state (spinner +
      "Waiting for a location fix…" subtitle) so the up-to-10s coordinate wait
      from #585/#595 is visible instead of silent.
  1. No configured coordinates + a just-restarted (cold) GeoClue → flip Night
     light on: switch stays **on**, a spinner appears, subtitle reads
     "Waiting for a location fix…"; both clear when the fix lands and the
     screen warms.
  2. Flip **off** during the wait → spinner/subtitle clear immediately (not
     after a systemctl round-trip), and #595's guarantee still holds (the
     unit never starts).
  3. With coordinates configured, or a warm GeoClue → straight to on, no
     spinner blip.
  4. No coordinates at all → after the 10 s deadline, the spinner clears and
     the switch snaps back off with the "no coordinates" warning.
  5. Multi-monitor: toggling on from one output's Appearance drawer should put
     **both** drawers into the pending state together.
  - _(sourced from #602's tracked list, not spelled out as a manual step in
    the PR body)_ Annika specifically wants: flick the switch **off then back
    on while the spinner is still up** — it should stay on and keep spinning
    rather than snapping off. This matches the PR's own
    `a_second_toggle_on_takes_over_the_pending_notice` unit test, but isn't
    listed as a live-verify step in the PR body.

## Widgets (tray / screenshot / screencast)

- [ ] **(#590)** Tray keyed-diff fold: with several tray apps running,
      right-click a tray item, **leave its menu open**, and let another app
      emit a `NewIcon`/`NewTitle` — the popover should **survive** (a rebuilt
      button would drop it). Confirm ordering still matches service order
      after a middle item is removed.
- [ ] **(#590)** Screencast chip stop-click: start a screen share (OBS's
      PipeWire capture, or any portal screen-share consumer). Hover the chip →
      tooltip names the target and ends with "Click to stop". **Click it** →
      the cast should actually stop and the chip disappear. Separately, run a
      damage-tracked `wf-recorder` capture (wlr-screencopy) — the tooltip
      should read "Cannot be stopped from here (wlr-screencopy)" and clicking
      should be an honest no-op (a single debug line, not a crash or a fake
      success).

## Workspaces page (#1071)

- [ ] **(#1071 phase 1)** The read-only Workspaces drawer page. Everything here
      needs a live niri session — the page is built entirely out of
      `niri::workspaces()` + `niri::windows()`, both of which are empty in a
      sandbox.
  - **Open it.** Settings drawer → **More** → **Workspaces**, or (since #1108,
    below) the dedicated grid-icon chip right of the mpris chip in the bar —
    the numbered `[1 2 3 …]` switcher itself is untouched (epic revision 5:
    no name ever shows there). It also opens from a niri keybind, via the
    command surface:
    ```sh
    busctl --user call mov.vibec0re.trollshell /mov/vibec0re/trollshell \
        org.gtk.Actions Activate 'sava{sv}' open-page 1 s workspaces 0
    ```
  - **A named workspace becomes a card.** With the page open, name the focused
    workspace and watch the card appear without reopening the drawer:
    ```sh
    niri msg action set-workspace-name chat
    ```
    Expect a card titled `chat` in the column headed by that screen's connector,
    carrying one icon per app with a window open there — the real desktop-entry
    icons (Firefox's, the terminal's), not `application-x-executable`. Hovering
    an icon should tooltip the app's display name. Then
    `niri msg action unset-workspace-name` → the card goes away and the column
    stays, showing "No named workspaces on this screen".
  - **The icons follow the windows.** With the card on screen, open a new app on
    that workspace → its icon joins the strip within a beat; close it → the icon
    goes. Two windows of the _same_ app must stay **one** icon. Moving a window
    to another workspace (`niri msg action move-window-to-workspace 3`) should
    move its icon to that workspace's card, if that one is named.
  - **Two screens, one column each.** On a multi-monitor setup, confirm one
    column per connected output, side by side, headed by the connector name and
    ordered lexically (`DP-1` left of `HDMI-A-1`) — the same order the Displays
    page lists them in. Name a workspace on each screen and confirm each card
    lands in its own screen's column. Hot-unplug one output → its column goes;
    plug it back → it returns.
  - **Column order under niri.** Cards inside a column follow niri's workspace
    index, so `niri msg action move-workspace-down` on a named workspace should
    re-order the cards to match. App icons on a card follow niri's _column_
    order, so `move-column-left` should re-order the icons.
  - **Nothing else appeared.** _Historical, for phase 1 only:_ it was read-only,
    with no `+` button, no Start/Stop, no Edit and no `workspaces.toml`. Phase 2
    adds Start/Stop, ephemeral cards and the file — but **still no `+` button**
    (§3.7 settles that Edit → Save is what persists a workspace), which the
    phase-2 block below re-checks.
  - **Regression check on the two pages this touched.** The Stats drawer's
    "Top apps" expanders resolve their icons through the same helper, which
    moved to `components/app_meta.rs` — confirm CPU/Memory top-apps rows still
    show real app icons and display names, not the generic fallback. And the
    Settings page's **More** group moved into its own function — confirm all
    four rows (Wallpaper, Displays, Clipboard history, Workspaces — in that
    order since #1071 phase 2, which appended rather than inserting) are there
    and each still deep-links to its page.

- [ ] **(#1071 phase 2)** The file, the primitives, and Start/Stop. Most rows
      here need a live niri session **and** a running `systemd --user`; the
      hermetic suite drives both through scripted fakes, so what it proves is
      that the shell _would_ issue the right calls, not that the compositor and
      the manager answer them as expected. Four rows below **do** have CI
      coverage of their logic and are re-checked here only end to end: the
      no-change save, the layered base, the `chat--dev`/`Chat` half of the name
      rules, and the card-list scroll — that last one most of all, since its
      first fix passed its own test while doing nothing in the real drawer.
  - **The file is live.** With the shell up, hand-write
    `~/.config/trollshell/workspaces.toml`:
    `toml
    [workspace.chat]
    monitor = "DP-1"
    apps = [{ id = "Alacritty" }]
    `
    Within ~3 s a greyed `chat` card appears in `DP-1`'s column saying **"Not on
    a screen"**, with one dim Alacritty icon and a **▶** button. No shell
    restart. Add a typo'd key (`layuot = "golden"`) → the journal gets exactly
    one line, `workspace.* = chat.layuot is not valid; expected monitor,
autostart, layout or apps`, and every other key still applies. Put a bad
    value on a real key (`layout = "gilded"`) → one line naming _that_ key, the
    stack still loads, and `monitor` still applies.
  - **Names are slice names.** Try `[workspace."chat--dev"]` and
    `[workspace.Chat]`. The first is dropped with one line (a doubled dash
    cannot be a systemd unit — measured: `trollshell-ws--foo.slice` is
    _"Invalid argument"_); the second is **folded** to `chat`, because niri
    matches workspace names case-insensitively and the two would otherwise be
    one workspace with two slices.
  - **Start, on a busy current workspace.** Focus a workspace that has windows
    on it, then hit **▶** on `chat`. A **new** workspace appears (Annika's
    ruling: _"if you start an entire stopped stack a new workspace is in
    order"_), is named `chat` — check `niri msg workspaces` — and Alacritty
    opens on it. The button becomes ⏹ and the greying lifts.
  - **Start, on an empty current workspace.** Switch to an empty workspace and
    hit ▶ on a second stack. It **adopts** the current workspace rather than
    creating another (_"if nothing is on the current workspace, adopt current
    workspace"_). `niri msg workspaces` shows no extra workspace.
  - **The slice, and what Stop takes down.**
    ```sh
    systemd-cgls --user-unit trollshell-ws-chat.slice
    systemctl --user list-units 'trollshell-ws-*'
    ```
    One `trollshell-ws-chat-0.service` per app of the stack, inside
    `trollshell-ws-chat.slice`. Now open something on that workspace **by hand**
    (from a terminal already on it, so it gets no unit of its own, and also
    something launched from a launcher, which niri puts in an `app-niri-*.scope`)
    and hit **⏹**. Everything goes: the slice takes the stack's own units, the
    `app-niri-*.scope` is stopped by unit, and the hand-started one is closed
    through niri. Then `niri msg workspaces` shows the workspace **unnamed**
    again.
  - **Two stacks whose names share a dash prefix — the nesting hazard.** Save or
    write both `chat` and `chat-dev`, start both, then stop `chat` only.
    `chat-dev` must still be running. (`-` is systemd's slice-hierarchy
    separator, so a naive `trollshell-ws-chat-dev.slice` would be a _child_ of
    `chat`'s and go down with it — measured. The name's own dashes are written
    `\x2d`, so check `systemctl --user list-units 'trollshell-ws-*'` really
    shows `trollshell-ws-chat\x2ddev.slice` and that it renders as
    `Slice /trollshell/ws/chat-dev`.)
  - **The naming hazard the housekeeping closes.** Start `chat`, then close
    every one of its windows **by hand** without pressing ⏹. The card goes
    Inactive within a beat (the workspace still carries the name at this point —
    `niri msg workspaces`). Wait a moment, then press **▶** again: it must
    start normally. If it silently launched onto the focused workspace and left
    the card grey, the housekeeping that releases the lingering name has
    regressed — `SetWorkspaceName` returns success either way, so there is no
    error to look for.
  - **`Starting` is visible.** Put something slow in a stack — a browser — and
    watch the button while it comes up: a spinner, not an icon, and **not
    clickable**. Clicking it during that window must do nothing and must not
    start a second copy.
  - **The stray-window reconcile.** Add an app that opens its window on whatever
    workspace was focused rather than the new one. Within the 10 s grace window
    it should be **moved onto** the stack's workspace. An app that never opens a
    window at all must not wedge the Start — it finishes at the end of the
    grace window with the others in place.
  - **A name that did not land never launches.** The refusal lives in the
    service layer, not on the card: there is no "this button is disabled because
    the name is taken" state, and a card **does** offer ▶ when its name is held
    by a lingering empty workspace. So drive it and check the outcome. Name a
    workspace `chat` by hand on a _different_, empty workspace
    (`niri msg action set-workspace-name chat`) while a `chat` stack is Inactive,
    then press **▶** on the card. Expect: **nothing launches**, and a
    "Workspaces" notification saying the name is already on a workspace. If apps
    appear anyway, the read-back after the naming batch has regressed — niri
    reports success for a `SetWorkspaceName` it silently ignored, so that
    read-back is the only thing standing between this and a stack dropped onto
    whatever workspace happened to be focused.
  - **Start and Stop say when they fail.** Every refusal above should surface as
    a toast, not only a journal line: `journalctl --user -u trollshell -f` and
    the on-screen notification should say the same thing.
  - **Start does not steal your windows.** Open Firefox (or whatever an existing
    stack lists) on workspace 2. Press **▶** on that stack from another
    workspace. Your existing window must **stay where it is** — only the copies
    the Start launched land on the new workspace. Before the fix round the grace
    window matched on `app_id` alone and yanked it across, focus and all.
  - **Stop does not stop the shell.** The nastiest one, and worth doing once.
    Open a link from the shell itself (a notification action, or the
    control-center's help link) so the browser is forked into
    `trollshell.service`'s own cgroup — confirm with
    `systemctl --user status trollshell | grep -c firefox` or
    `busctl --user call org.freedesktop.systemd1 /org/freedesktop/systemd1 org.freedesktop.systemd1.Manager GetUnitByPID u <pid>`,
    which should answer `trollshell.service` rather than an `app-niri-*.scope`.
    Move that window onto a started stack's workspace and press **⏹**. Expect:
    the **window closes**, the shell keeps running, and the journal carries
    _"not a unit this workspace may stop; closing the window instead"_ naming
    `trollshell.service`. If the shell exits, the allowlist has regressed.
  - **Ephemeral card → Edit → Save.** Open a couple of apps on an **unnamed**
    workspace. A card titled **"Unsaved workspace"** appears in that screen's
    column with the live app icons and a name field — and **no `+` button
    anywhere on the page** (§3.7). Type `Chat Room` → the field goes red, the
    text you typed is **kept**, and hovering it offers `chat-room`; type one
    character of a correction and the red clears at once. Type an existing
    stack's name → red again, saying it already exists. Type `chat2` →
    `~/.config/trollshell/workspaces.toml` grows `[workspace.chat2]` with one
    `apps` entry per app, **in niri's left-to-right column order** (open the apps
    in an order that is not alphabetical and check the file, since that order is
    what phase 3 restores), and **no `order` key** (arrays replace whole, so
    inventing one would discard a base-pinned order).
  - **…and the saved workspace becomes the Active card.** Immediately after that
    Save, `niri msg workspaces` must show the workspace **named `chat2`**, and
    the page must show **one** card — saved, Active, with a ⏹ — not two. If you
    see an "Unsaved workspace" card _and_ a greyed `chat2` card, the
    `SetWorkspaceName` half of Save has regressed, and pressing ▶ on the greyed
    one would launch a second copy of everything.
  - **A no-change save touches no bytes.** Hand-annotate the file with comments,
    `md5sum` it, save an unrelated ephemeral workspace, and confirm the existing
    stacks' lines and every comment are byte-identical.
  - **The layered base.** Put a stack in `$XDG_CONFIG_DIRS`' copy (the
    home-manager base) and a _different_ one in your overlay: both cards show.
    Change one key of the base stack in the overlay: only that key changes.
    Remove the base stack with `[workspace]` + `_unset = ["<name>"]` in the
    overlay: its card goes and its siblings stay.
  - **The trailing column.** Set `monitor = "DP-9"` (a connector you do not
    have). The card moves to a greyed **"Not connected"** column at the right.
    Its ▶ still works and starts the stack on the focused screen.
  - **The card list scrolls — in the real drawer.** The row that most needs a
    live pass: the scroller's first cut had no height cap, so it grew to fit its
    content and the drawer clipped it exactly as before, while its own test
    passed by supplying a 220 px window the drawer never supplies. Save enough
    stacks that a column overflows (a dozen or so on 1080p), open the drawer
    **full height**, and confirm a scrollbar appears and the bottom card is
    reachable by scrolling. If the column just runs off the bottom of the
    screen, the cap is gone again.
  - **Nothing from phase 4 appeared.** No edit sub-page — the only Edit is the
    ephemeral card's name field, and it takes the name only. (Autostart, the
    column-order restore, the layout and the drag between columns landed in
    phase 3; they have their own rows below.)
  - **The generalised launcher did not change what it launches.** Regression
    check on the two call sites that moved: `systemctl --user status
trollshell-plugin-<id>.service` still shows `Restart=on-failure`,
    `PartOf=…target` and the plugin's declared env; a plugin with a keyring slot
    still receives its key (`systemctl --user show -p Environment
trollshell-plugin-<id>.service`) while `tr '\0' '\n' < /proc/<pid>/cmdline`
    on the `systemd-run` process shows the **bare** `--setenv=<NAME>` with no
    value. And a plugin's detached `RunCommand` still lands in
    `trollshell-launch.slice`.

- [ ] **(#1071 phase 3)** Autostart, column order, layout, card order and the
      drag between screens. Everything here needs a real niri session and a
      hand-written `~/.config/trollshell/workspaces.toml`; none of it can be
      checked from a test.
  - **Column order is the stack order.** Write a stack whose apps are listed in
    an order the apps will _not_ open in — a slow one first:

    ```toml
    [workspace.dev]
    apps = [
      { id = "firefox" },
      { id = "Alacritty" },
    ]
    ```

    Press **▶**. Alacritty will map first; when Firefox's window arrives, the
    columns must end up **firefox, Alacritty** left to right. `niri msg windows`
    reports each window's `pos_in_scrolling_layout` if the eye is not enough.
    If the order is whatever they opened in, the one batch after the grace
    window has regressed.

  - **…and a gap closes up.** Put an app in the middle of the list that will
    never open a window (`{ id = "definitely-not-a-command" }`). The other two
    must still end up adjacent, in order, with nothing between them — and the
    journal must carry **one** warning naming the missing app, not one per
    launch and not none.
  - **The layout runs once, and only at the end.** Add `layout = "golden"` and
    watch: the columns must be re-proportioned **after** the last window
    arrives, in one go. `journalctl --user -u trollshell -f | grep -i layout`
    should show nothing at all for `layout = "none"` (or a stack with no
    `layout` key). If `hytte-plugin-niri-layouts` is not on `PATH`, expect one
    warning and a Start that still succeeds.
  - **Autostart.** Set `autostart = true` on two stacks and restart the shell
    (`systemctl --user restart trollshell`). Both must come up **one after the
    other, in the file's `order`** — not at once. Watching is the test: if two
    stacks' windows interleave on one workspace, the sequential runner is gone
    and each Start is fighting the other for the focus.
  - **…exactly once.** Leave the shell running for a few minutes and open and
    close windows. The stacks must **not** start again. The tell for a
    regression here is dramatic: every workspace change relaunches everything.
  - **…and a stack whose screen is not plugged in is skipped.** Set
    `monitor = "DP-9"` on an autostarting stack. At login it must **not** start
    anywhere, and the journal must carry exactly one `info` line naming the
    stack and `DP-9` — arriving about **five seconds** after the first screen
    appears, not immediately. Its card keeps its ▶; plugging the screen in after
    that line has been logged does **not** start it (hot-plug autostart is
    deliberately out of scope).
  - **…but a screen that arrives a beat late still counts.** The five seconds
    are the settle window (review LOW 7): at login `trollshell.service` and
    `kanshi` come up together under `niri-session.target`, so niri's first
    `WorkspacesChanged` can easily predate the kanshi profile enabling a screen.
    With a kanshi profile that enables a second output, an autostarting stack
    pinned to that output **must** come up on it. If it lands on `plan.skipped`
    instead, the settle window has regressed to a latch on the first snapshot.
  - **…and a restart does not toast you.** The one to run after any change
    here: with two autostarting stacks **running**, `systemctl --user restart
trollshell`. Expect the cards to come back Active and **no notification at
    all**. A toast per stack reading "… did not start: that name is already on a
    workspace" means the already-on-screen check is gone; nothing is
    double-launched either way, so the toast is the only symptom.
  - **Card order decides where a started workspace lands.** With
    `order = ["chat", "dev"]` and both pinned to the same screen, start `dev`
    first and then `chat`. `niri msg workspaces` must show `chat` at a **lower
    index than `dev` on that monitor**, and the bar's numbered switcher must
    agree. The index is counted per screen: starting a stack on `DP-1` must
    never move anything on `HDMI-A-1`.
  - **Drag a card to the other screen.** With two monitors, drag a saved card
    from one column into the other. The column under the pointer outlines while
    you are over it, the card dims while it is in flight, and on drop:
    `~/.config/trollshell/workspaces.toml` gains (or changes) that stack's
    `monitor` key — **and nothing else in the file moves**. `md5sum` the file
    first, hand-annotate it with comments, and confirm every comment and every
    other stack is byte-identical afterwards.
  - **…and an Active stack's workspace goes with it.** Repeat the drag on a
    **started** stack: its windows must move to the other screen in the same
    beat (`niri msg workspaces` shows the named workspace's `output` changed).
    Repeat on a **stopped** one: the file changes and **niri must not move
    anything** — in particular no workspace should shuffle on either screen.
    The next ▶ is what puts it on the new screen.
  - **Dropping a card back on its own column does nothing.** `md5sum` the file,
    pick up a card and drop it where it already is: the file must be
    byte-identical. A two-pixel accidental drag must not rewrite config.
  - **In-column reordering is still phase 4.** Dragging a card up or down
    _within_ one column does nothing — §5 puts the order's drag handles in the
    Edit sub-page, which does not exist yet. Only the screen changes.
  - **A drag can be cancelled by a rebuild, and that is not a bug.** The page
    rebuilds every column on any of the five signals it maps over, `windows`
    included, and GTK cancels a drag whose source widget goes away. So a window
    opening anywhere while a card is mid-flight ends the drag. Drag again. It
    reads as flakiness and is a rebuild (review INFO 9).
  - **Two of a stack's apps in one niri column.** A known limit, not a
    regression (review LOW 6): niri decides column membership, and two of the
    stack's windows stacked in one column get one `MoveColumnToIndex` each
    against that same column, so the second immediately moves what the first
    placed. A freshly Started workspace opens each window in its own column, so
    you need an adopted workspace or a niri config that consumes to see it. A
    **floating** window is different and is handled: it takes no column index at
    all, so the tiled apps around it are not shifted — open a stack app floating
    (`niri msg action toggle-window-floating`) and confirm the rest still land
    at 1, 2, 3.

- [ ] **(#1108)** The workspace-manager bar chip, and the Workspaces page
      filling the drawer's existing wide width cap instead of shrinking to
      its content. No niri/systemd session required — this is pure bar-chip
      and drawer-sizing behavior on top of whatever phase 1-3 state already
      exists.
  - **The chip.** A grid-icon-only chip sits in the bar right of the mpris
    chip (still left of the plugin center slot), tooltip "Workspaces". Click
    it → the Workspaces page opens, same as the Settings → More route above.
    Click it again while the page is already open → the drawer retracts, same
    as every other chip-opened page.
  - **Only that page fills the cap.** With one or two monitors, the
    Workspaces page used to shrink to its content's natural width (visibly
    narrower than the Stats multicolumn page even though both share the same
    wide clamp). Open the Workspaces page and compare its card width against
    the Stats page (any layout with two side-by-side history graphs) — the
    two should now measure the same width, the shared cap, regardless of how
    many monitor columns Workspaces actually has. Open Settings (any chip),
    then reopen Workspaces (the new chip, or Settings → More → Workspaces) —
    the filled width must still hold on the second show, not just the first
    build. Every other page's width must be visually unchanged from before
    #1108 (still shrinking to its own content, not filling any cap).

## Control-center

- [ ] **(#515)** AI Keys tab: set an OpenRouter key → the row flips to "Key
      stored". Declare an LLM plugin (e.g. `pet`) with
      `programs.trollshell.plugins.pet.secrets = [ "openrouter" ];` and
      confirm the running unit gets `OPENROUTER_API_KEY` in its environment
      (`systemctl --user show-environment` / `cat /proc/<pid>/environ`) and
      answers with model-backed lines, not canned. Rotate the key → the unit
      relaunches with the new key; **Clear** → the row flips to "No key set"
      and the plugin drops back to canned. The key should never land in
      `plugins.json`, in a **shipped** unit file (nix store / `etc/`), or in the
      logs. It _does_ land, same-user only, in the transient unit fragment
      systemd writes at
      `/run/user/<uid>/systemd/transient/trollshell-plugin-<id>.service` — that
      file is `0644` and carries `Environment="OPENROUTER_API_KEY=sk-…"`, held
      in by `/run/user/<uid>`'s `0700`. In scope per #956 and true before #984
      as well as after; don't read it as a regression when you look.
- [ ] **(#984)** …nor in the launch **argv**. The channel the list above left
      out: `/proc/<pid>/environ` is `0400` but `/proc/<pid>/cmdline` is `0444`,
      so a `--setenv=OPENROUTER_API_KEY=sk-…` argument was readable by any local
      user for as long as the `systemd-run` process lived. With a key stored and
      a plugin declaring the slot, **from a second local account** poll every
      process's argv across a `Control.ReloadPlugins`:

  ```sh
  # second account, while a ReloadPlugins / key rotation runs in the first.
  # Three details, each of which cost a false result when got wrong:
  #  - one arg per line: a glob on `<` is an ambiguous redirect, and cmdline is
  #    NUL-separated, so `[^ ]*` would run past the argument boundary;
  #  - `2>/dev/null` BEFORE `< "$f"`: bash applies redirections left to right,
  #    so a process exiting mid-sweep is reported unless stderr is already gone;
  #  - anchor the pattern: a bare `grep API_KEY` matches its own argv as the
  #    sweep reads it back out of /proc, one spurious hit per iteration.
  while :; do
    for f in /proc/[0-9]*/cmdline; do tr '\0' '\n' 2>/dev/null < "$f"; done |
      grep '^--setenv=.*API_KEY'
    sleep 0.2
  done
  ```

  A bare `--setenv=OPENROUTER_API_KEY` line means the fix is holding; anything
  with `=sk-…` after it is the defect. Sweep across startup `reconcile`, a
  control-center start/stop, and a key rotation (`relaunch_for_secret`) — the
  three launch paths. Confirm the plugin still actually **got** the key:
  `systemctl --user show -p Environment trollshell-plugin-<id>` shows
  `OPENROUTER_API_KEY=sk-…` (same-user, in scope per #956) and the plugin
  answers with model-backed lines. The bare-`--setenv` mechanism itself checks
  out with no shell running:
  `SEKRIT=v systemd-run --user --pty --setenv=SEKRIT -- printenv SEKRIT`
  prints `v`.

- [ ] **(#538)** Plugins tab runtime overlay: run the shell with a declared
      plugin, open the control-center Plugins tab → the row shows a live
      "Connected · rendering in <mount>" badge. Stop/crash the plugin unit →
      within ~2 s the badge flips to "Active but not connected" without
      reopening the tab. A plugin that trips the effect rate cap shows a
      "· N dropped" violation count.
- [ ] **(#616)** Build revision reachable at runtime — the D-Bus half of
      #601. (#836 later added the UI surface and closed the issue; the
      footer entry below is that half.)

  ```sh
  busctl --user call mov.vibec0re.trollshell.Control \
    /mov/vibec0re/trollshell/Control mov.vibec0re.trollshell.Control Revision
  ```

  Should return the short git hash your `flake.lock` pins trollshell at (a
  `-dirty` suffix if built from an uncommitted tree), not `dev`/`unknown`.
  Confirm the payoff by rebuilding (`nix flake update trollshell` + rebuild)
  and checking the hash actually changes. **Live-verify hazard worth
  recording:** there are now **two** independent `TROLLSHELL_REV` values —
  one injected into each of the `trollshell` and `trollshell-control-center`
  wrapper slices' own `preFixup` — so a consumer that reads its own env
  instead of calling `Control.Revision` over D-Bus reports _itself_, the
  exact false conclusion #601 exists to prevent. The wrapper-set env is also
  inherited by forked children (a terminal opened via
  `gio::AppInfo::launch_default_for_uri`, or the plugin `RunCommand` effect)
  — a shell descended from trollshell reports the **deployed** revision, not
  `dev`, even from a dev `cargo run` run inside it.

- [ ] **(#640/#703)** Control-center **Places** tab: add, edit and remove a
      location row, and confirm each change reaches
      `~/.config/trollshell/places.toml` and that the running shell picks it
      up (the shell mtime-polls the file — there is deliberately no new D-Bus
      surface for this). The write path is format-preserving, so the thing to
      check on glass is what a plain re-render would have destroyed:
      **hand-edit the file first** — put a comment above a place, reorder some
      keys — then make an edit in the tab and confirm your comment, key order
      and preamble all survive. The editor must also work with the shell
      **stopped**, since it writes the file directly rather than asking the
      shell to.
- [ ] **(#887)** Plugins tab **adaptive drill-down**. Open the control-center
      at its default 760 px width: the tab is a two-pane split — plugin list on
      the left, the selected plugin's detail on the right — and each row carries
      a status word (`Rendering` / `Connected` / `Not connected` / `Stopped` /
      `Failed`) beside the existing `Running · enabled` subtitle. Now **drag the
      window narrower, across ~520 px**: the detail pane must disappear, leaving
      the list alone; tapping a row pushes its page (titled with the plugin id),
      and the header's back arrow returns. Widen again and both panes come back
      with the selection intact. The parts the tests can't reach: the two
      resize directions on a real compositor (tests present a fixed-size window
      each time), the swipe-back gesture, and the **journal** — the
      control-center's own `journalctl --user` output, grepped for `exceeds`,
      must stay empty while the window sits collapsed, which is the #856 contract
      (`AdwBreakpointBin` warns once per allocation when a child's minimum
      exceeds the bin's width, and #856 records that the warning is suppressed
      in tests, so only a long-lived window proves it). Then **start and stop a
      plugin from the detail page** — the switch moved there from the row — and
      confirm the list's status column and the detail's `Host connection` row
      both catch up within ~2 s without the selection jumping or a pushed page
      popping. Finally, `systemctl --user stop trollshell` with the tab open:
      the list must drop to a single non-selectable "Unavailable" row and the
      detail pane to its empty state, with no panic — and then
      `systemctl --user start trollshell` again: within ~2 s the same plugin
      must be selected again, with its detail page still pushed if you had
      drilled in, rather than the list settling on whichever plugin sorts
      first (the review round's parked selection; the unit test drives the
      placeholder directly, a real timeout is what a human can confirm).
      Also worth an eye on every one of these steps: the tab must show
      **exactly one** set of window buttons — the app's own, top right.
      Switching between Plugins, Places and AI Keys must not make a second
      close/minimise/maximise cluster appear or disappear. **No version
      column** — that half of #887 is held pending the source decision on the
      issue.
- [ ] **(#601/#836)** The control-center's **footer** reports the running
      _shell's_ revision, not its own. The companion app is a separate binary
      with its **own** `TROLLSHELL_REV` baked in by `nix/control-center.nix`,
      so a local read would report the wrong build in exactly the case #601
      exists to catch. Get the two to **diverge** — rebuild the shell only
      (`nix flake update trollshell` + rebuild), leaving the companion on its
      old store path — then open the control-center: the footer must show the
      **shell's** hash, cross-checkable against the `Control.Revision` busctl
      call above, which is the same source. With the shell **stopped** the
      footer must read `Shell revision: unavailable (trollshell not running)`
      rather than silently falling back to the companion's own hash. A `dev`
      value is rendered as-is on purpose — an unstamped local build is correct
      information, not a state to hide.
- [ ] **(#959)** Control-center **connection banner** shows only when it
      carries information, and the **footer** now reports version alongside
      revision. With the shell running, launch (or reload) the control-center:
      the top banner must **not** appear at all — no "Connected to trollshell
      …" notice sitting for the whole session. Stop the shell
      (`systemctl --user stop trollshell`) while the window is open, or
      launch the control-center with the shell already down: the banner must
      appear reading "trollshell is not running — start the shell to manage
      it". Start it again (`systemctl --user start trollshell`) and relaunch
      the control-center: the banner is gone once more. The footer should now
      show both version and revision — e.g.
      `trollshell 0.1.0 · revision 34e3d96` — rather than the bare revision
      line it used to show (a `-dirty`/`dev` revision still passes through
      honestly, per the #601/#836 entry above). With the shell stopped it
      still falls back to exactly
      `Shell revision: unavailable (trollshell not running)` (kept
      byte-for-byte — nothing else needed updating for the change).
- [ ] **(#983/#989)** The control-center's polls **agree with each other and
      with reality**, both directions, without a relaunch.
      **(a) #989 — banner and footer follow the shell.** Stop the shell
      (`systemctl --user stop trollshell`), then launch the control-center
      _while it is down_ — the ordinary order after login or a
      `home-manager switch`. The banner reads "trollshell is not running" and
      the footer reads `Shell revision: unavailable (trollshell not running)`,
      as before. Now, **without touching the window**, run
      `systemctl --user start trollshell`: within ~2 s the banner must
      **disappear on its own** and the footer must fill in with
      `trollshell <version> · revision <hash>`. Before #989 both were probed
      once at window build, so the banner stayed pinned for the whole session
      while the Plugins tab happily listed live plugins underneath it. Then
      `systemctl --user stop trollshell` again with the window still open: the
      banner must **come back** within ~2 s and the footer return to
      "unavailable" — the reappearance is the half nothing did before. Repeat
      the start/stop a couple of times; it must track every time. Also check
      the app's own `journalctl --user` (or its terminal): while the shell is
      down there must be **one** `trollshell control endpoint unreachable`
      line per outage, not one every two seconds — the probe logs on
      transitions only.
      **(b) #983 — the Plugins tab under a slow shell.** The generation guard
      only shows itself when a poll is genuinely slow, which the hermetic
      tests fabricate. To provoke it live, put the machine under load (a
      `nix build` of something large works) and, with the Plugins tab open on
      a plugin's detail page, **flip its switch on and off a few times**. The
      switch must settle on what you asked for and the sidebar's status column
      must move forwards only — `Stopped → Starting… → Rendering` — never
      flicking back to `Stopped` a second _after_ it already read `Rendering`.
      A single stale-looking blink of the switch or the status word is the
      defect; before #983 it was reproducible whenever one `ListPlugins`
      round trip overran the next tick.
- [ ] **(#1003)** The **AI Keys** tab follows the shell too — it was the one
      tab #989/#983 left probed once at window build. Unlike the banner and
      the Plugins tab, this one carries **no timer of its own**: it re-reads
      only when `main.rs`'s shell probe reports a reachability change, so
      what to watch for is tied to shell start/stop, not to a fixed cadence.
      Stop the shell with `systemctl --user stop trollshell`, then **open the
      control-center while it is down** — the ordinary order after login. The
      AI Keys tab shows "Unavailable" on every row, as before. Now, **without
      reopening the tab**, start the shell again (with a key already stored
      from an earlier run, or set one now over `busctl` and the tab once it
      recovers): within ~2 s (the shell probe's own cadence) the rows must
      **flip on their own** to "Key stored"/"No key set", with no need to
      switch away to Plugins and back. Then stop the shell again with the tab
      still open: the rows must return to "Unavailable" within ~2 s — this
      direction costs no `ListAiKeys` call at all (the probe already knows
      the shell is gone), so it should if anything be _faster_ than the
      up-direction. Repeat the start/stop a couple of times — it must track
      every time, matching whatever the Plugins tab shows at the same moment.
      Also check the control-center's own output (it is normally launched by
      hand or from the launcher rather than as a systemd user unit, so its
      `info!` lines land on its terminal, not `journalctl`) — grep
      specifically for `ListAiKeys`: while the shell is down there must be
      one `ListAiKeys failed` line per outage, not one every couple of
      seconds, and the shell coming back must log exactly one
      `ListAiKeys recovered` line — transitions only, same as #989's banner.
      Grepping unqualified will also catch the Plugins tab's own
      `ListPlugins failed`/`ListPlugins recovered` lines — since #1017 that
      poller got the same transitions guard, so an unfiltered grep now shows
      one line per outage from _each_ tab rather than a `ListPlugins` flood
      to filter past. Finally,
      with the shell running, **set or clear a key from the tab** right as a
      shell restart could plausibly land a reachability-triggered read at the
      same instant (restart the shell, then immediately click Apply/Clear) —
      the row must settle on what you asked for, never blink back to the
      pre-change value a moment after.
- [ ] **(#1017)** The **Plugins** tab's own `ListPlugins` poll gets the same
      transitions-only guard #989/#1003 already gave the banner and the AI
      Keys tab — it was the one poller left logging on every 2 s tick. Stop
      the shell (`systemctl --user stop trollshell`), then open the
      control-center **while it is down**: watching the app's own output
      (launched by hand or from the launcher, so `info!` lines land on its
      terminal, not `journalctl`), there must be **exactly one**
      `ListPlugins failed` line, then silence across every subsequent 2 s
      tick for as long as the shell stays down — not one line per tick.
      Start the shell again: within ~2 s there must be **one**
      `ListPlugins recovered` line, and then silence again while it stays up.
      Finally, stop the control-center, start the shell first, and **then**
      open the control-center (the ordinary case — a healthy session): the
      first poll succeeds, and there must be **no** `ListPlugins` line at
      all, not even a spurious "recovered" on the very first tick. All three
      are pinned hermetically by `plugins_tab::gtk_tests`
      (`n_failing_polls_emit_exactly_one_failed_line`,
      `down_up_down_logs_two_failures_and_one_recovery`,
      `a_first_poll_that_succeeds_is_silent`) against a real `tracing`
      subscriber; this bullet is the on-machine confirmation the #1035 PR
      body flagged as not yet run.

## Documentation site (GitHub Pages)

- [ ] **(#629)** `docs/plugin-env.md` is now published on the Pages
      options-doc site (rendered by the `options-doc` derivation via
      `cmark-gfm`, the same mechanism `options.html` uses), and the site
      root (`index.html`) is a real landing page linking both documents —
      it used to be a byte-copy of `options.html`. `nix flake check` now
      builds `options-doc` as part of `checks`, closing the #449-class gap
      where a broken derivation here would only surface as a red Pages
      deploy after merge, but the **deploy step itself** stays outside
      `nix flake check` by design — the Pages workflow only copies files out
      of the built derivation, it never runs live in CI. So a human still
      has to confirm the published site once this reaches `main`:
      `https://vibec0re.github.io/trollshell/` shows the landing page (not
      the options doc directly) and both its links work; `plugin-env.html`
      renders its env-var reference tables as real tables, not literal `|`
      characters; and `options.html`'s `plugins.<name>.env` option
      description now links out to the styled `plugin-env.html` page rather
      than raw Markdown on GitHub.

## Not carrying a live-verify list, noted for context

- **(#488)** Dependency-hygiene / MSRV PR — bumped the workspace MSRV
  1.85 → 1.91 (`clippy::incompatible_msrv` flagged real 1.87/1.91 API use once
  `rust-version.workspace = true` was wired up). No live-verify list; flagged
  here only because it's a project-wide toolchain-floor change worth being
  aware of, not something to click through in a Niri session.
- **(#606)** Adds the `LICENSE` file (verbatim MPL-2.0 text) that
  `Cargo.toml`'s `[workspace.package] license = "MPL-2.0"` had been declaring
  with nothing to back it. No code, no config, no CI surface — genuinely
  nothing to verify in a Niri session; noted explicitly so a future reader
  doesn't mistake the silence for an oversight.
- **(#611)** A refresh of _this_ document. Docs-only, so it carries nothing to
  verify itself. Worth recording that it was written before #610 merged and so
  shipped without it — #610's entries were folded in immediately afterwards,
  which is why the coverage line moved twice on 2026-07-30. The general lesson
  is in the header: this file goes stale within minutes of a merge burst, so
  updating it belongs to the burst rather than to a later pass.
- **(#622)** Docs-only spec-drift retraction sweep: the VPN connect/disconnect
  non-goal in the network-panel design spec (shipped via #169), a stale
  `overlays/` roster in the src-reorg spec, and one Rust doc-comment line in
  `overlays/mod.rs` naming overlays that no longer exist. Also splits the
  plugin-widgets design spec's original three-clause "not in v1" non-goal:
  **both** two-way inputs (`Node::Slider` #315, `Node::Entry` #363) **and**
  arbitrary pixels/images (`Node::Pixels` #284 — an arbitrary RGBA8 buffer
  whose `data`/`scale` are mutable per-`id`, not a one-shot fixed image) have
  shipped. The only clause still standing as a non-goal is custom drawing
  (cairo/snapshot calls executed in-process) — out-of-process frontend-B
  plugins structurally can't do that, they only ever ship a validated pixel
  buffer for the host to paint. No behavior change anywhere in the diff —
  nothing to verify in a Niri session.
- **(#628)** A refresh of _this_ document — folded in #616/#622/#623/#624/#625
  (the burst right behind #611's refresh) and bumped the coverage line to
  #625. Docs-only, so it carries nothing to verify itself; noted here for the
  same reason #611 is — it's the state of this file immediately before the
  #629/#630/#634 burst that this pass folds in, so a future reader can see
  where the handoff was.
- **(#662)** Docs-only claim-correction sweep across `CLAUDE.md`,
  `docs/CHOOM-INIT.md`, and the `etc/{calendar,kanshi,niri,systemd}/README.md`
  deployment docs — stale overlay/module-layout references, a false "CI
  doesn't run clippy" claim, a dead `blur.kdl` citation, an outdated
  now-playing/consent-prompt description, and two shipped-feature bullets
  (recurring-event expansion, per-output wallpaper) still marked as
  follow-ups. No code changed and no behavior to click through in a Niri
  session; noted here so its absence from the sections above doesn't read as
  an oversight.
