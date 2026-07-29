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

**Coverage: #458 through #596**, plus one item flagged pending merge (#598 —
still open at the time of writing). Originally created by #507 for the 2026-07
merge wave (#458–#496); refreshed by #602 to fold in everything merged since
(#497–#596). Renamed off the month-stamped filename in the same pass — this is a
living checklist that gets refreshed periodically, not a dated snapshot of one
merge wave.

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

- [ ] **(#514)** Interactive consent overlay: run `hytte-infobroker auth
  --agent claude` with no grant → a focus-grabbing card appears on niri's
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

## Pending merge (not yet on `main`)

- [ ] _(#598 — still OPEN as of this refresh)_ Night light gains a third
      `Resolving` state (spinner + "Waiting for a location fix…" subtitle) so
      the up-to-10s coordinate wait from #585/#595 is visible instead of
      silent. Per the PR body, once merged:
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
    listed as a live-verify step in the PR body — call it out explicitly once
    #598 lands.

## Not carrying a live-verify list, noted for context

- **(#488)** Dependency-hygiene / MSRV PR — bumped the workspace MSRV
  1.85 → 1.91 (`clippy::incompatible_msrv` flagged real 1.87/1.91 API use once
  `rust-version.workspace = true` was wired up). No live-verify list; flagged
  here only because it's a project-wide toolchain-floor change worth being
  aware of, not something to click through in a Niri session.
