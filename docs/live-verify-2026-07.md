# Live-verify checklist — 2026-07 merge wave

Recent merges (#458 through #496, the code-climate + idle-cutover + plugin-runtime
wave) each carry their own "needs live-verify in a Niri session" list in the PR
body, because the build agents that wrote them run in sandboxed worktrees with no
compositor, no display server, and (for the EDS/HAFAS/LLM items) no live daemons
to hit. Individually those lists are easy to lose once a PR merges. This doc pulls
every such item out of `gh pr view <N> --json body` for #458–#496, dedupes and
groups them by subsystem, and gives each one a concrete command or gesture so a
verify pass can be run mechanically rather than re-deriving from memory. Source:
the "Live-verify" / "Needs live-verify" sections of the individual PR bodies —
re-run `gh pr view <N> --json body` on the PR number in parens if you need full
context on _why_.

Checked items are ones Annika has already implicitly verified — noted inline.
Everything else is unchecked and still wants a pass in a real Niri session.

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

## Plugins & launcher

- [ ] **(#489)** Plugins now launch via `systemd-run --user` transient units
      from a declarative enabled-state instead of pre-installed static units.
      `systemctl --user list-units 'trollshell-plugin-*'` at shell start should
      show only the enabled set as transient units; stop/start from the
      control-center Plugins tab should work; a disabled plugin should stay
      down across a shell restart.
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
      agent (`infobroker auth --agent <name>` with no grant) and confirm the
      `Effect::Notify` toast actually lands ("agent X requested departures —
      denied").
- [ ] **(#493)** Live datasource fetch: `infobroker auth --agent claude` →
      `export HYTTE_INFOBROKER_TOKEN=…` → `infobroker get departures` against
      a real, configured `places.toml` — confirm the HAFAS round-trip and
      scoped JSON response.
- [ ] **(#493)** Icon check: confirm `channel-secure-symbolic` /
      `dialog-warning-symbolic` render as real icons in the live Adwaita
      theme, not `image-missing`.

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

## Audio & media

- [ ] **(#470)** Drag-safe seek slider: open the Media drawer on an active
      player, **drag the seek bar and hold**. The thumb should follow your
      finger and no longer snap back to the mpris poller's stale position
      mid-drag, then settle to the seeked position on release.
- [ ] **(#494)** Audio panel row dedup (`panels/audio/{sinks,sources}.rs` →
      shared row constructor): confirm sink/source rows render and behave
      identically to before (no visual or interaction change expected).

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

## Shell chrome (regression-only, pixel-identical refactors)

- [ ] **(#494)** Drawer visibility gating: the triplicated gate pattern was
      extracted into `components/visibility_gate.rs` and adopted by
      `modal.rs`. Confirm drawer show/hide gating is unchanged (no new
      flicker, no page staying mounted/unmounted incorrectly).

## Not carrying a live-verify list, noted for context

- **(#488)** Dependency-hygiene / MSRV PR — bumped the workspace MSRV
  1.85 → 1.91 (`clippy::incompatible_msrv` flagged real 1.87/1.91 API use once
  `rust-version.workspace = true` was wired up). No live-verify list; flagged
  here only because it's a project-wide toolchain-floor change worth being
  aware of, not something to click through in a Niri session.
