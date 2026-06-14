# Calendar (evolution-data-server) for niri+trollshell

trollshell's "Calendar" drawer page surfaces upcoming events from the
calendars you've added in **GNOME Settings → Online Accounts**. The
configuration UI is GNOME's; the actual sync runs under
[evolution-data-server](https://gitlab.gnome.org/GNOME/evolution-data-server)
(EDS), and trollshell reads EDS's local `.ics` cache directly.

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
   gnome-control-center online-accounts
   ```

   For Google: sign in via the embedded WebView, grant Calendar access on
   the account-detail page. iCloud: sign in with an
   [app-specific password](https://support.apple.com/en-us/HT204397).
   Generic CalDAV: the "Calendar" tile on the New Account screen.

3. Wait a minute for the first sync. EDS will create
   `~/.local/share/evolution/calendar/<source-uid>/calendar.ics` for each
   provisioned calendar.

## How it works

- trollshell ships a `calendar` service that polls
  `~/.local/share/evolution/calendar/*/calendar.ics` every 60 seconds.
- Each VEVENT is parsed via the `icalendar` crate. We extract UID,
  SUMMARY, DTSTART/DTEND, LOCATION, and STATUS.
- Events with `STATUS:CANCELLED` are filtered out.
- The signal exposes events whose start lies in the next 7 days, sorted
  ascending. Multi-day events that started in the past but haven't ended
  yet are also surfaced.
- Sync lag: trollshell re-reads the `.ics` cache once a minute, but EDS
  itself typically pulls upstream every 5–30 minutes (configurable per
  source via Settings → Online Accounts). New events can take tens of
  minutes to appear after they're created on Google / iCloud / CalDAV.
- Both `DTEND` and `DURATION` are honoured. The duration parser covers
  the PT-form (e.g. `PT15M`, `PT1H30M`, `PT4H`) for timed events and the
  P-form (e.g. `P1D`, `P3D`, `P1W`) for all-day events, plus combined
  forms like `P1DT2H`. Google Calendar in particular emits `DURATION`
  for many events, especially recurring instances.

## Verification

1. Confirm the cache files exist and are non-empty:

   ```sh
   ls ~/.local/share/evolution/calendar/
   wc -l ~/.local/share/evolution/calendar/*/calendar.ics
   ```

   Each subdirectory is one source (a calendar in your account).

2. Open the trollshell drawer's **Calendar** page (the stack name is
   `calendar`). The current month grid shows on top; events for the next
   week populate the "Upcoming" boxed list below.

3. If the list is empty:
   - No GOA accounts? Add one as above.
   - Cache files exist but no events? Likely all of your events are
     more than 7 days in the future. Check the raw `.ics` file.
   - Cache files missing entirely? Check `journalctl --user -u
evolution-calendar-factory.service` for sync errors, and confirm
     the account is "Calendar" enabled in the GOA panel.

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
  Monday's instance would. Full RRULE expansion is a v2 task; the
  `icalendar` crate's `recurrence` feature exposes `rrule` for this
  but adds non-trivial dependencies.
- **No GtkCalendar day marking.** The month-grid shows the current month
  as a static reference; days with events aren't visually highlighted.
  v2 task once the bind path is wired.
- **No timezone re-display.** Times are converted to local time at parse
  time. Events authored in another timezone show the local equivalent
  (via `chrono-tz`). Floating (no-TZ) DTSTARTs are interpreted as local
  time, per the spec's "follows current timezone of the attendee" note.
- **No human-readable calendar names.** The `calendar_name` field is
  the source-directory basename — typically a UUID. A v2 helper could
  read EDS's per-source metadata to surface the user-set display name.
