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
- [ ] **(#544)** A plugin granted `Capability::RunCommand` emits
      `Effect::RunCommand` → the host spawns the argv and the plugin gets back
      an `EffectResult` with the exit status + captured stdout. A missing
      binary / non-zero exit / a command running past 10s all still return
      `ok: false` (no hang), with a warn in `RUST_LOG=trollshell=info`.
      `~/.local/state/trollshell/effects-audit.log` accrues one line per
      brokered/dropped effect and rotates to `.log.1` past the 256 KiB cap.
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
      loopback-only (`127.0.0.1:8787`) OpenAI-compatible shim over headless
      `claude`, so an LLM-backed plugin (`pet`, `caw`) can ride a Claude Code
      subscription instead of a paid API key with a one-line `Environment=`
      change on _its own_ unit. This is a brand-new service nobody has run
      live yet, so every check below is genuinely first-run, not a
      regression check.
  1. **Starts and refuses correctly.**
     `systemctl --user start trollshell-claude-bridge`, then
     `journalctl --user -u trollshell-claude-bridge` — expect
     `hytte-claude-bridge listening (keyless by design; loopback only)`. Then
     run it by hand with `ANTHROPIC_API_KEY=x hytte-claude-bridge` — expect a
     refusal naming the offending variable, exit 1 (the billing guard that
     stops metered credits leaking in from an inherited env).
  2. **A round trip:**
     `curl -s localhost:8787/v1/chat/completions -H 'content-type: application/json' -d '{"messages":[{"role":"system","content":"you are a cat"},{"role":"user","content":"say hi"}]}'`
     — expect a `chat.completion` body with text in
     `choices[0].message.content`, inside 8s.
  3. **Not reachable off-box** — from another host on the LAN,
     `curl http://<box>:8787/…` must fail to connect. The bridge validates no
     bearer token at all (it structurally can't — see the PR body), so
     loopback-only is the entire authorization boundary; the IP is
     hard-coded, only the port is configurable.
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
  6. **pet end-to-end:** add
     `Environment=PET_LLM_URL=http://127.0.0.1:8787` and
     `Environment=OPENROUTER_API_KEY=local-bridge` to
     `trollshell-plugin-pet.service`, restart, poke the cat. Confirm via
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
      chip. **Six** fixes shipped against this one bug and five of them
      failed, so walk all of it rather than glancing at the bar. The shape on
      `main` pins the title label to `TITLE_CHARS = 24` in both directions,
      which makes the full transport row's natural width a constant, and hands
      the full-vs-mini decision to an `AdwBreakpointBin` whose single
      `max-width` breakpoint is measured once from the built row and then
      frozen. Nothing measures neighbours at runtime any more —
      `components/center_budget.rs` is deleted. What to check: 0. **First, confirm you are running #854.** Between #851 and #854 the mini
      chip was allocated outside the bin's `GTK_OVERFLOW_HIDDEN` clip rect and
      was drawn nowhere at all — the centre slot went _empty_, leaving a dead
      ~250-290 px hole with nothing to click. If you see that, you are on a
      pre-#854 build and the rest of this list will mislead you: an absent chip
      is the old breakage, a _mispositioned_ or _flickering_ one would be new.
      `journalctl --user -u trollshell | grep "exceeds AdwBreakpointBin"`
      prints one line per allocation on a pre-#854 build and nothing after.
  1. **The original bug.** With a player **stopped** (not merely paused —
     stopped, so there is no position to show), the centre slot must show the
     **mini chip**, not the full transport row, and must not crowd the
     app-switcher buttons beside it. This is the symptom the issue was filed
     on. The chip sits at the **left** edge of the centre slot when collapsed
     (`halign: Start`), while the full row hugs the right-hand cluster when
     there is room — that asymmetry is deliberate, and is what keeps the chip
     inside the rectangle the bin paints.
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
  5. **One expected regression, disclosed.** The mini chip still _requests_
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
  - **The colour axis is orthogonal to the skin.** Set
    `TROLLSHELL_CORE_LEDS_STYLE=crt` **and** `TROLLSHELL_CORE_LEDS_COLOR=heat`
    and restart the shell: expect heat-coloured lamps _through_ the CRT's
    scanline comb and curved-glass vignette — both at once, not one instead of
    the other. That composition is the whole design claim of #857 and the
    single most valuable thing to eyeball.
  - Sweep the four knobs (each takes effect on shell restart; an unrecognized
    value logs one `tracing::warn` and falls back):
    - `TROLLSHELL_CORE_LEDS_STYLE` = `vfd` (default) / `lcd` / `oled` / `crt`
    - `TROLLSHELL_CORE_LEDS_COLOR` = `heat` (default) / `style` / `rainbow` /
      `transpride` / `#rrggbb`. `style` should give the plain single-ink panel
      — the pre-#857 look, and the guarantee the byte-identity tests pin.
    - `TROLLSHELL_CORE_LEDS_ROWS` = `rect` (default — a **wide rectangle**
      since the second #857 pass, near-square before it) or a row
      count. `=3` on a many-core box makes a wide, short strip — check it does
      not push the drawer wider (it is 247 px at 1× on a 64-core box, and the
      scale deliberately refuses to blow it up past the budget).
    - `TROLLSHELL_CORE_LEDS_FILL` = `spare` (default) / `blank` — only visible
      when the last row is ragged **and** the skin ghosts, so pair it with
      `…_ROWS=3` and `…_STYLE=lcd` (or `vfd`). `spare` shows unlit lamps
      filling the tail, `blank` leaves the gap bare. On `oled`/`crt` the two
      are identical by construction (no ghost to differ on).
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
  - `TROLLSHELL_CORE_LEDS_ROWS=8` still overrides the automatic shape (and on
    a 64-core box gives back roughly #861's square). `=rect` or unset is the
    new rectangle.
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

## Control-center

- [ ] **(#515)** AI Keys tab: set an OpenRouter key → the row flips to "Key
      stored". Declare an LLM plugin (e.g. `pet`) with
      `programs.trollshell.plugins.pet.secrets = [ "openrouter" ];` and
      confirm the running unit gets `OPENROUTER_API_KEY` in its environment
      (`systemctl --user show-environment` / `cat /proc/<pid>/environ`) and
      answers with model-backed lines, not canned. Rotate the key → the unit
      relaunches with the new key; **Clear** → the row flips to "No key set"
      and the plugin drops back to canned. The key should never land in
      `plugins.json`, the unit file, or the logs.
- [ ] **(#538)** Plugins tab runtime overlay: run the shell with a declared
      plugin, open the control-center Plugins tab → the row shows a live
      "Connected · rendering in <mount>" badge. Stop/crash the plugin unit →
      within ~2 s the badge flips to "Active but not connected" without
      reopening the tab. A plugin that trips the effect rate cap shows a
      "· N dropped" violation count.
- [ ] **(#616)** Build revision reachable at runtime (refs #601, which stays
      **open** — only the surface-agnostic plumbing landed, the UI-surface
      decision is still pending):

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
