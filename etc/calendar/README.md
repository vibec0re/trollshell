# Calendar (evolution-data-server) for niri+trollshell

trollshell's "Calendar" drawer page surfaces upcoming events from the
calendars you've added in **GNOME Settings → Online Accounts**. The
configuration UI is GNOME's; the actual sync runs under
[evolution-data-server](https://gitlab.gnome.org/GNOME/evolution-data-server)
(EDS), and trollshell reads EDS through **libecal** (the same client API
gnome-calendar / Evolution use) via the in-tree `hytte-ecal` bindings — the
same path the Tasks page already uses.

> **This changed (2026-06-15).** trollshell used to file-poll EDS's local
> `~/.local/share/evolution/calendar/*/calendar.ics` cache, which only local
> calendars populate — **CalDAV (Nextcloud) and Google calendars were
> invisible**, because EDS caches those in an SQLite `cache.db` under
> `~/.cache/evolution`, not as `.ics`. Reading via libecal sees every backend
> uniformly. See `docs/superpowers/specs/2026-06-08-caldav-interop-design.md`.

## Required packages

Install on Arch:

```sh
sudo pacman -S evolution-data-server gnome-online-accounts gnome-control-center
```

`evolution-data-server` provides the `evolution-calendar-factory` service
and the on-disk cache; `gnome-online-accounts` is the GOA backend that EDS
consumes; `gnome-control-center` exposes the **Online Accounts** panel
that lets you add Google / iCloud / CalDAV accounts.

> If you already have GNOME installed, all three are typically pulled in.

### NixOS

The flake's `nixosModules.default` wires this whole stack behind
`programs.trollshell.enableRecommendedServices` (default `true`):
`services.gnome.gnome-online-accounts`, `services.gnome.evolution-data-server`,
`services.gnome.gnome-keyring` (token storage), `programs.dconf` (the GSettings
backend the panel persists through), and `gnome-control-center` in
`environment.systemPackages`. Just set `programs.trollshell.enable = true;` — no
extra calendar config needed. Each is `mkDefault`, so an explicit
`services.gnome.<svc>.enable = false;` still wins.

## One-time setup

1. Make sure GOA / EDS user services start under your session. The `dbus`
   user bus auto-activates them on first call, so this is usually a no-op
   on a stock Arch + GNOME install. Verify with:

   ```sh
   systemctl --user status evolution-calendar-factory
   ```

2. Open Online Accounts and add your provider:

   ```sh
   trollshell-online-accounts        # NixOS module installs this wrapper
   # or, by hand:
   XDG_CURRENT_DESKTOP=GNOME gnome-control-center online-accounts
   ```

   > **Niri gotcha.** Bare `gnome-control-center online-accounts` exits with
   > _"Running gnome-control-center is only supported under GNOME and Unity,
   > exiting"_ — it hard-checks `XDG_CURRENT_DESKTOP`, which is `niri` in this
   > session. The `trollshell-online-accounts` wrapper (from the NixOS module)
   > spoofs `XDG_CURRENT_DESKTOP=GNOME` for that one launch; the one-liner
   > above does the same by hand.

   For Google: sign in via the embedded WebView, grant Calendar access on
   the account-detail page. iCloud: sign in with an
   [app-specific password](https://support.apple.com/en-us/HT204397).
   Generic CalDAV / Nextcloud: the "Calendar" tile (or the Nextcloud
   provider) on the New Account screen.

3. Wait a minute for the first sync, then open the trollshell Calendar page.
   The libecal worker re-scans every 60 s (and on page-open), so newly synced
   events show up within a minute of EDS pulling them.

## How it works

- trollshell ships a `calendar` service that talks to EDS over **libecal**
  on a dedicated worker thread (one `Registry` + a per-source `CalClient`
  cache), re-scanning every 60 seconds and on Calendar-page open. This
  mirrors the Tasks service.
- It enumerates every `"Calendar"` source EDS knows about
  (`Registry::calendars()`) and queries each with the `"#t"` (match-all)
  S-expression, then parses the returned VEVENTs via the `icalendar` crate.
  We extract UID, SUMMARY, DTSTART/DTEND (or DURATION), LOCATION, and STATUS.
- Events with `STATUS:CANCELLED` are filtered out.
- The signal exposes events whose start lies in the next 7 days, sorted
  ascending. Multi-day events that started in the past but haven't ended
  yet are also surfaced.
- Sync lag: trollshell re-queries EDS once a minute, but EDS itself
  typically pulls upstream every 5–30 minutes (configurable per source via
  Settings → Online Accounts). New events can take tens of minutes to
  appear after they're created on Google / iCloud / CalDAV.
- Both `DTEND` and `DURATION` are honoured. The duration parser covers
  the PT-form (e.g. `PT15M`, `PT1H30M`, `PT4H`) for timed events and the
  P-form (e.g. `P1D`, `P3D`, `P1W`) for all-day events, plus combined
  forms like `P1DT2H`. Google Calendar in particular emits `DURATION`
  for many events, especially recurring instances.

## Verification

1. Confirm EDS knows about your calendar sources (these are what
   `Registry::calendars()` enumerates — backend-agnostic, so this works for
   local, CalDAV, and Google alike):

   ```sh
   ls ~/.config/evolution/sources/        # one .source per account/calendar
   systemctl --user status evolution-calendar-factory
   ```

   `gnome-calendar` (if installed) is a quick cross-check — anything it shows,
   trollshell should now show too.

2. Open the trollshell drawer's **Calendar** page (the stack name is
   `calendar`). The current month grid shows on top; events for the next
   week populate the "Upcoming" boxed list below, labelled by their real
   calendar names.

3. If the list is empty:
   - No GOA accounts? Add one as above.
   - Sources exist but no events? Likely all of your events are more than 7
     days in the future (the window), or EDS hasn't finished its first
     upstream pull yet — give it a few minutes.
   - Run with logs to see per-source connect/query results:
     `RUST_LOG=hytte_services=debug cargo run -p trollshell` and watch for
     `calendar: client connect failed` / `calendar: query failed`.
   - Still nothing? Check `journalctl --user -u
     evolution-calendar-factory.service` for sync errors, and confirm the
     account is "Calendar"-enabled in the GOA panel.

## What this does NOT do

- **No event creation, editing, or deletion.** trollshell is read-only.
  Use GNOME Calendar (`gnome-calendar`) or your provider's web UI for
  changes.
- **No notifications or reminders.** EDS-generated reminders go through
  the GNOME notification daemon — trollshell's notification widget will
  surface them if `gnome-shell-extension-something` raises them, but the
  calendar service itself never emits notifications.
- **No recurring-event expansion past the master entry.** The master
  VEVENT's DTSTART is the only instance considered. A weekly meeting
  whose master DTSTART was last year won't show up — even though next
  Monday's instance would. Now that we read via libecal this is *feasible*
  (`e_cal_client_generate_instances_sync` expands a range server-side), but
  it needs a new `hytte-ecal` binding — a follow-up, not yet wired.
- **No GtkCalendar day marking.** The month-grid shows the current month
  as a static reference; days with events aren't visually highlighted.
  v2 task once the bind path is wired.
- **No timezone re-display.** Times are converted to local time at parse
  time. Events authored in another timezone show the local equivalent
  (via `chrono-tz`). Floating (no-TZ) DTSTARTs are interpreted as local
  time, per the spec's "follows current timezone of the attendee" note.
