# caw's morning briefing — design note (#407)

**caw caws the morning news.** Once a day the caw plugin composes the day's
shape — weather, the first useful departure, and (eventually) the next calendar
events — into two-to-three sentences in her voice and delivers it sticky in her
speech bubble, mirrored as a toast.

This note exists because #407 flagged the **data path** as the real design
question and asked for a small spec before code. It records what shipped and
what is deliberately deferred.

## Home

The briefing extends `hytte-plugin-caw` (no new plugin): caw already has the
persona, the preem pixel speech bubble, and the out-of-process plumbing. The
voice is one `chat()` call through `hytte-ai-providers`, exactly the pet-brain
pattern (#276), including the #438/#472 rule: **no provider configured → the
plain templated brief, zero network round-trips to a brain that isn't there.**

## The data path — issue option (a)+(c), resolved against reality

The issue leaned "(a) subscribe existing StateKeys + (c) grow the StateKey set".
One correction against current `main`: **no domain StateKeys exist today** —
`hytte-plugin-proto`'s vocabulary is `Clock` / `SlotVisible` / `Accent` /
`AudioSpectrum` only, so the (a) leg is empty (the weather _plugin_ fetches
open-meteo itself; nothing weather-shaped is host-pushed). That leaves, per
ingredient:

- **Weather — fetched by caw, one shot.** Same source and idiom as the weather
  plugin (open-meteo, blocking `ureq` on `spawn_blocking`), but _located from
  config, not GeoClue_: the first `[[place]]`'s `lat`/`lon` in
  `~/.config/trollshell/places.toml` (home), falling back to a forward geocode
  of `$TROLLSHELL_WEATHER_CITY`. A once-a-day reading doesn't warrant a D-Bus
  client, and "weather at home in the morning" is the semantic you want even
  when the laptop wakes up elsewhere.
- **Departures — fetched by caw, one shot.** The HAFAS endpoint the departures
  plugin uses, for the same first place's `station`, honoring its
  `lines`/`directions` filter and `walk_minutes`. The _first catchable_ row
  (suburban, not cancelled, still reachable after the walk) becomes "S9 to
  Spandau in 12 — leave in 2." / "— move, choom." when it's tight.
- **Calendar — deferred to the host (option (c)).** evolution-data-server sits
  behind libecal + D-Bus; an out-of-process plugin can't read it without
  linking half the shell or duplicating the EDS client. The composition
  (`briefing::compose_plain` / the LLM facts block) already folds events in and
  is unit-tested with synthetic ones, so when the host grows a
  briefing-shaped `StateKey` (e.g. `CalendarUpcoming` → next N events as
  `{start_hhmm, summary}`, additive under the #305 rules), only caw's gather
  side and manifest change. A follow-up issue tracks the host-side key.

Why not fetch-everything-via-host now: the host push (`plugins.rs`) and the
native calendar service are active work in other lanes, and a state key is an
API — it deserves its own review rather than riding a plugin PR.

These are deliberately **one-shot fetches at briefing time**, not new pollers:
the issue's argument against (b) is duplicate _pollers_, and a single
GET-per-ingredient-per-day keeps the always-on polling exactly where it already
lives (the weather/departures plugins, the native services).

## Trigger

`$CAW_BRIEFING_TIME` (default `07:00`; `H` or `H:MM`; `off` disables), checked
on the plugin's existing 2 s heartbeat:

- **Once per local date**, persisted as `~/.local/state/caw/briefing-stamp`
  (a bare `YYYY-MM-DD`), written _before_ composing — a restart or a crash
  mid-compose can never re-caw the same morning.
- **Due window**: `[T, T+6h)` same-day. A box suspended across the hour briefs
  on wake (the heartbeat only runs while awake) — the practical stand-in for
  the issue's "first unlock" until the host pushes logind session state to
  plugins; a shell started at 22:00 does _not_ get a stale "morning" drop.
- True first-unlock-of-the-day needs host help (logind's `Lock`/`Unlock` is
  D-Bus): a `SessionLocked`-style StateKey is the same follow-up ticket as the
  calendar key.

## Delivery

- The composed text lands as `CawMsg::Briefing` and renders through a
  **taller preem speech box** (8 rows vs the ordinary 4, same violet palette,
  extra `caw-briefing` class), mood `Chirp`, stage direction
  "_caws the morning news_".
- **Sticky until poked** — the poke is the ack (and still plays a poke
  reaction). One exception: an expression caw publishes _after_ the briefing
  retires it; the plugin is her voice first, a news desk second.
- **Toast mirror** via `Effect::Notify` + `Capability::Notify` (#406/#414) —
  the sidebar is usually closed at 07:00 and trollshell is the notification
  daemon, so the news lands regardless.

## Voice

System persona: sardonic cybercrow news desk; facts-only ("never invent
events, weather, or trains"), 2–3 sentences, lowercase, no emoji. The facts
block states absent ingredients honestly (`weather: unavailable`) so the model
has nothing to hallucinate around. `sanitize` enforces the format mechanically
(join lines, strip quotes/emoji, clamp ~220 chars) and an empty reply falls
back to the deterministic template — caw always caws _something_, worst case
"no data on the wire. suspicious. fly careful out there."

Config: `CAW_LLM_URL` (local llama-server) / shared
`~/.config/trollshell/openrouter.key` + `CAW_LLM_MODEL` / `CAW_LLM_API_KEY`,
resolved with the pet's exact provider semantics.

## Follow-ups

- Host-side briefing StateKeys (option (c)): `CalendarUpcoming` (+ optionally a
  session-unlock signal) pushed to subscribed plugins; caw's gather side then
  drops its own fetches ingredient by ingredient. Tracked in a follow-up issue.
- #320 (Claude usage number) slots in as one more optional ingredient line once
  its source exists.
