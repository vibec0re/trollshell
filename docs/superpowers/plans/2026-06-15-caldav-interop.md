# CalDAV Interop — Calendar→libecal + GOA provisioning

> **For agentic workers:** this plan was implemented in one pass on 2026-06-15;
> boxes are checked to record what shipped. The remaining unchecked items need a
> live Niri session + Nextcloud account and can't be done from CI.

**Goal:** Make the trollshell Calendar page see CalDAV (Nextcloud) calendars,
and settle how accounts get into EDS. Tasks already worked over CalDAV; the
calendar was the weak link because it file-polled the local `.ics` cache, which
CalDAV sources never populate (they cache to SQLite under `~/.cache/evolution`).

**Decisions** (from `specs/2026-06-08-caldav-interop-design.md`):

- **D1 → (c) GOA.** goa-daemon + the GNOME Online Accounts panel provision the
  EDS sources. Least code; the stack was already wired in the NixOS module.
- **D2 → read-only calendar.** Display only. Tasks stay read-write.

**Architecture:** `crates/hytte-services/src/calendar.rs` is rebuilt to mirror
`tasks.rs` — a dedicated EDS worker thread owning one `hytte_ecal::Registry` and
a per-source `CalClient` cache, reading VEVENTs via `get_object_strings("#t")`.
The public surface (`events()`, `refresh()`, `format_when`, `CalendarEvent`) is
held identical so `widgets/calendar.rs`, `panels/calendar.rs`, and `modal.rs`
need no edits.

**Tech Stack:** Rust 2024, libecal FFI (`hytte-ecal`), `icalendar` for parsing,
hytte reactive layer, `chrono`. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-06-08-caldav-interop-design.md`

---

## File Structure

- **Modify:** `crates/hytte-ecal/src/lib.rs` — add `Registry::calendars()`
  (extension `"Calendar"`), sibling of `task_lists()`.
- **Rewrite:** `crates/hytte-services/src/calendar.rs` — file-poller → libecal
  worker. Keep the parsing/formatting helpers (`dpt_to_local`,
  `parse_iso8601_duration`, `format_when`, `event_to_calendar_event`).
- **Modify:** `nix/nixos-module.nix` — add the `trollshell-online-accounts`
  launcher wrapper next to the existing `gnome-control-center` package.
- **Modify:** `etc/calendar/README.md` — describe the libecal backend, the Niri
  launch fix, and what changed vs. the old poller.

No changes to `trollshell/src/**` (public surface preserved). GOA/EDS/keyring
NixOS wiring was already present — D1=(c) is the status quo, so **no
collection-source provisioning code** (§1 of the spec) is needed.

---

### Task 1: `Registry::calendars()` in hytte-ecal — ✅

- [x] Add `pub fn calendars(&self) -> Vec<Source>` calling
      `self.sources_by_extension(c"Calendar")`, documented as the Events sibling of
      `task_lists()`. (`crates/hytte-ecal/src/lib.rs`)

### Task 2: Rewrite `calendar.rs` onto libecal — ✅

- [x] **2.1** Replace the tokio poll-loop + filesystem walk (`cache_root`,
      `scan_cache_dir`, `parse_ics_file`) with the `tasks.rs` worker shape:
      `OnceLock<mpsc::Sender<()>>` + a `hytte-eds-cal` thread running `run_worker`,
      plus a tokio ticker that sends a unit "refresh now" every `POLL_INTERVAL`.
- [x] **2.2** `Worker` owns `Registry` + `HashMap<uid, CalClient>`;
      `scan_all()` enumerates `registry.calendars()`, opens an
      `ECalClientSourceType::Events` client per source (5 s connect budget, cached),
      queries `"#t"`, and parses each returned body.
- [x] **2.3** Factor the per-VEVENT conversion into
      `event_to_calendar_event(...) -> Option<CalendarEvent>` and a
      `parse_ics_body(...)` that wraps a bare-VEVENT body when the first parse
      fails (mirrors `tasks::parse_one`). DTEND/DURATION/all-day logic unchanged.
- [x] **2.4** `calendar_name` now comes from `Source::display_name()` — the
      human-readable calendar title, not the UUID dir-name the poller was stuck
      with. (Partial fix of the long-standing "no friendly names" TODO.)
- [x] **2.5** Keep `events()`, `refresh()`, `format_when`, `CalendarEvent`
      identical. `refresh()` now just enqueues onto the worker channel.
- [x] **2.6** Port the unit tests to call `parse_ics_body` directly (no temp
      files); add a bare-VEVENT-without-wrapper case. 12 tests green.

### Task 3: Niri launch fix for gnome-control-center — ✅

- [x] Add `trollshell-online-accounts` (`pkgs.writeShellScriptBin`) to the
      module's `environment.systemPackages`, exec-ing control-center's
      `online-accounts` panel with `XDG_CURRENT_DESKTOP=GNOME` so it doesn't
      hard-exit with "only supported under GNOME and Unity" under Niri.
      (`nix/nixos-module.nix`)

### Task 4: Docs — ✅

- [x] Update `etc/calendar/README.md` (backend, launch fix, what changed).
- [x] Update the spec status block + decisions + resume checklist.

### Task 5: Gate — ✅

- [x] `cargo clippy --workspace --all-targets` clean (pedantic + deny).
- [x] `cargo test -p hytte-services calendar` — 12/12 green.
- [x] `nix-instantiate --parse nix/nixos-module.nix` OK.

---

## Verify (needs a live session — not done here)

1. `trollshell-online-accounts` → add the Nextcloud account, enable Calendar +
   Tasks.
2. Within ~60 s the Nextcloud calendars' upcoming events appear in the
   trollshell sidebar + drawer Calendar page; the calendar labels read as the
   real calendar names.
3. A task ticked in the shell round-trips to Thunderbird + Nextcloud Tasks web
   (unchanged behaviour; sanity check the shared backend).

## Out of scope / unlocked follow-ups

- **RRULE expansion** via `e_cal_client_generate_instances_sync` — now feasible
  on libecal; needs a new hytte-ecal binding. The current path is still
  master-only (same limitation as before).
- **Per-source calendar colour** — `ESourceExtension` colour read; opportunistic.
- **Event-write from the shell** (D2 said read-only) — cheap to add later;
  libecal already exposes `create_from_ical`/`modify_from_ical`.
