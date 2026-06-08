# Calendar + Tasks ⇆ Thunderbird interop via shared Nextcloud CalDAV

**Status:** **PARKED** — brainstorm captured 2026-06-08. Awaiting two decisions
before a plan is written (see [§Open decisions](#open-decisions--the-pin)). **No
code has been written.**

**Scope (anticipated):** rework `crates/hytte-services/src/calendar.rs`
(file-poller → libecal); new EDS source-provisioning for a Nextcloud CalDAV
*collection*; `crates/hytte-ecal` likely grows a `calendars()` enumerator +
Events-type reads; NixOS module / `etc/` changes for credentials. The **tasks
service is expected to need no functional change** — see below.

**Author/setup context (load-bearing — drives the whole design):**
- User stays on **Thunderbird** (migration off it didn't take; muscle memory + "works well enough").
- Calendars **and** tasks are backed by **Nextcloud over CalDAV**.
- **Read-write tasks from the shell is a hard requirement.**
- Box runs **Niri**, not GNOME (so GOA is almost certainly not running).

---

## Problem

Both shell features talk to **evolution-data-server (EDS)**. Thunderbird talks
to its own storage / its CalDAV accounts and never writes to EDS. With no shared
backend, the shell's calendar renders empty and the task panel only sees the
empty `trollshell-tasks` local list it auto-provisions on first run. Not a bug —
the two apps are reading different databases.

"Compatible" therefore means **give the shell and Thunderbird a shared
backend.** Since both already speak CalDAV to the same Nextcloud, the shared
backend already exists; we just have to point EDS at it.

---

## Current state (what the code does today)

### Tasks — already the easy case ✅

`crates/hytte-services/src/tasks.rs` is **backend-agnostic and read-write** via
our libecal bindings (`hytte_ecal::CalClient`). Its module header explicitly
notes it works against "ANY EDS backend… local files, CalDAV (Nextcloud), Google
Tasks via goa, EWS."

- Dedicated EDS worker thread owns one `Registry` + a `HashMap<uid, CalClient>`; public fns enqueue `Op`s and return immediately.
- Read: `tasks() -> Signal<Vec<Task>>` (tasks.rs:203), `task_lists()` (:215), `refresh()` (:226).
- Write (fire-and-forget): `create_task` (:235), `set_completed` (:249), `edit_task` (:259), `delete_task` (:270).
- Auto-provisions a local list if none exist: writes `~/.config/evolution/sources/trollshell-tasks.source` (`[Data Source] … Parent=local-stub`) then pings `org.gnome.evolution.dataserver.SourceManager.Reload` over D-Bus (tasks.rs:666–700).

**Implication:** the instant EDS has the Nextcloud task list as a source, the
existing task UI reads *and writes* it over CalDAV with **zero new task code**.
Ticking a box → libecal `PUT` → Nextcloud + Thunderbird both see it.

### Calendar — the weak link ❌

`crates/hytte-services/src/calendar.rs` does **not** use libecal at all. It
file-polls EDS's **local** `.ics` cache —
`~/.local/share/evolution/calendar/<uid>/calendar.ics` (calendar.rs:6, :198,
:225) — read-only, 60 s poll.

- Public surface: `events() -> Signal<Vec<CalendarEvent>>` (:136), `refresh()` (:148), `format_when()` (:468).
- **CalDAV-blind by construction:** CalDAV-backed EDS sources do *not* cache as a `calendar.ics` under `~/.local/share/…`; they cache in an SQLite `cache.db` under `~/.cache/evolution/calendar/<uid>/`. The current reader walks the wrong tree, so it cannot see Nextcloud calendars **even once EDS has them.** This must change regardless of provisioning choices.

### hytte-ecal — the bindings we already have

`crates/hytte-ecal/src/lib.rs` (the only `unsafe`-allowed crate):
- `Registry::task_lists()` (:57), `Registry::ref_source()` (:63) — source enumeration. (Needs a sibling `calendars()` for the "Calendar" extension; trivial parallel.)
- `CalClient::connect(source, source_type, wait)` (:182) — `source_type` already lets us open **Events** clients, not just Tasks.
- `CalClient::create_from_ical` (:209), `modify_from_ical` (:241), `get_object_as_string` (:266), `remove` (:323), `get_object_strings(sexp)` (:358).
- The internal `parse_component` already handles **both VEVENT and VTODO**, so VEVENT reads need no new parsing.

---

## Decision: Route A — shared CalDAV

```
Thunderbird ─┐
             ├──► Nextcloud CalDAV ◄── EDS ──► trollshell (calendar + tasks)
   (you) ────┘        (source of truth)
```

| Route | Shape | Write? | Cost | Fits design? | Verdict |
|---|---|---|---|---|---|
| **A — shared CalDAV** | TB → Nextcloud ← EDS → shell | ✅ keeps it | low — calendar→libecal refactor + provision one source | ✅ EDS *is* the "thin client to a persistent daemon" the design wants | **CHOSEN** |
| B — read TB's SQLite | shell → TB `local.sqlite`/`cache.sqlite` | ❌ realistically read-only | medium — reverse-engineer a private schema | ✗ reaches into another app's private DB | rejected — kills task write |
| C — sync daemon | TB-store ↔ EDS | ✅ | high, conflict resolution | ✗ two sources of truth | rejected — A is this, done right |

**Why A wins outright here:** the user already syncs both apps to one Nextcloud,
so the shared backend is free; tasks stay read-write (the hard requirement) with
no new code; it's fully bidirectional; and it keeps the existing
EDS-as-state-store architecture instead of inventing a parallel persistence path.

---

## Shape of the work (once the open decisions are made)

### 1. Point EDS at Nextcloud — one *Collection* source, not one per calendar

Provision a single EDS **collection** source aimed at the Nextcloud principal
(`https://<cloud>/remote.php/dav/`) with CalDAV discovery enabled. EDS then
auto-enumerates every calendar **and** task list under the principal and spawns a
child source per collection. Both services read whatever EDS discovered → one
source in, N calendars + M task lists out, and new Nextcloud calendars appear
automatically.

Reuse the in-repo provisioning pattern from `tasks.rs` (write a `.source` file →
`SourceManager.Reload`). The collection `.source` is more involved than the local
task list (it needs `[Authentication]`, `[Security]`, `[WebDAV Backend]` /
`[Collection]` groups), and **credentials are the genuinely hard part** — see D1.

### 2. Refactor `calendar.rs`: file-poller → libecal (mirror `tasks.rs`)

- Same dedicated-EDS-thread + `CalClient` pattern the task service uses.
- Add `Registry::calendars()` (extension "Calendar") paralleling `task_lists()`.
- Open Events-type `CalClient`s; query VEVENTs (`get_object_strings`); reuse `parse_component`.
- **Keep the public surface identical** — `events()`, `refresh()`, `format_when`, and the `CalendarEvent` type — so `widgets/calendar.rs`, `panels/calendar.rs`, and the sidebar widget need **no changes**.
- Delete the `.ics` directory walk.

---

## Open decisions — THE PIN 📌

These two block writing the plan. Everything above is settled; these are not.

### D1. Credentials / provisioning approach

EDS will **not** keep the Nextcloud app-password in the `.source` file (by
design). It wants the secret in the **Secret Service keyring**, keyed to the
source UID. So provisioning = write the collection `.source` + get the password
into the keyring + `SourceManager.Reload`.

| Option | How | Pros | Cons |
|---|---|---|---|
| **(a) Fully declarative** *(lean)* | NixOS module reads app-password from a sops/agenix secret, seeds the keyring (libsecret, `e-source-uid` attr) at session start; trollshell writes the `.source` | Most on-brand; `.source`-writing already solved in `tasks.rs` | Needs a Secret Service daemon running + the exact EDS keyring-attribute set; only new muscle is keyring-seeding |
| (b) Once via Evolution | Add the Nextcloud account by hand in Evolution's assistant once; EDS + keyring remember it; trollshell just consumes | Zero provisioning code | Imperative, un-Nix-y; means keeping `evolution` installed |
| (c) GOA | Run goa-daemon; its Nextcloud provider creates sources + holds the secret | Least trollshell code | A whole daemon on a Niri box; also drags in contacts/files |

**Sub-questions for (a):** Is a Secret Service daemon already running in the Niri
session (gnome-keyring / oo7-daemon / kwallet)? Would the app-password flow
through sops/agenix?

### D2. Calendar write scope

Tasks are read-write (required). For the **calendar**: read-only display, or also
**create/edit events** from the shell? The calendar widget has no create UI today;
assumption is **read-only view**. Adding event-write later is cheap (libecal
already exposes `create_from_ical`/`modify_from_ical`) — just naming the scope so
the plan doesn't silently assume it.

---

## Out of scope / future

- **RRULE expansion.** The longstanding "no recurrence expansion" limitation in `calendar.rs` becomes *feasible* once on libecal: `e_cal_client_generate_instances[_sync]` expands a time range server-side. Would need a new hytte-ecal binding. Not v1, but the refactor is what unlocks it.
- **Shell-as-EDS-credential-prompter.** trollshell already has an `overlays/prompt.rs`; it could answer EDS's `e-credentials-prompter` over D-Bus and *be* the keyring prompt UI. On-brand and cute; explicitly a v2.
- **Per-source calendar color + friendly names.** More valuable now that Nextcloud exposes several calendars; already a pre-existing v2 TODO in `calendar.rs`. Fold in opportunistically when touching the file, or defer.
- **Source-selection UI** (which discovered calendars/lists to show). Collection discovery surfaces all of them; a hide/show toggle is a later nicety.

---

## Resume here ▶

1. Answer **D1** (credentials: a/b/c + keyring-daemon question) and **D2** (calendar read-only vs event-write).
2. Write the paired plan: `docs/superpowers/plans/2026-MM-DD-caldav-interop.md`.
3. Likely first code step regardless of D1/D2: the §2 calendar→libecal refactor (self-contained, independently valuable, unblocks CalDAV visibility) — plus `Registry::calendars()` in hytte-ecal.
4. Then the §1 collection-source provisioning per the D1 choice.
5. Verify: Nextcloud calendars appear in the sidebar/drawer; a task ticked in the shell shows up in Thunderbird + the Nextcloud Tasks web UI, and vice-versa.
