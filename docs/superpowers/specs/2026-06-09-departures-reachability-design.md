# Departures reachability: a per-place walk budget + leave-by countdown

**Status:** design approved 2026-06-09; revised 2026-06-11
**Scope:** `crates/hytte-services/src/places.rs` (config + `ResolvedPlace`), `crates/hytte-services/src/departures.rs` (carry the budget), `trollshell/src/widgets/departures.rs` (leave-by label + fade), `trollshell/style.css` (one rule).

**Revision 2026-06-11 (field-report fixes).** (1) The leave-by token dropped the
word "leave" for a prepended `walk` icon (`icons/walk.svg`) — the row was too
wide. (2) Departed rows are now pruned live on the same clock tick, and the
sidebar re-polls every 30 s while open (`overlays/sidebar.rs`) so the board no
longer freezes on the open-time fetch — the staleness that made *every* row read
"leave now" on a 15-minute-old list.

## Motivation

The departures list is a DFI board: the next eight S-Bahn trains from your home
station. But the platform is a walk away — ~10 minutes to S Schöneweide — so a
train leaving in 4 minutes is noise, not signal. The original departures spec
([2026-05-14-sidebar-departures-design.md](2026-05-14-sidebar-departures-design.md))
listed **"Reachability / 'walk to platform' budget"** under _Out of scope_; this
closes that gap.

Each place gains a **walk time to its station**, and the list reframes its
countdown around it: instead of _when the train leaves_, show _when **you** must
leave to catch it_, and fade the trains whose window has already closed.

## Decision: leave-by countdown (not hide, not dim-only)

Three treatments were on the table:

| option                    | behaviour                                                                                                | why not                                                                                  |
| ------------------------- | -------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| **Hide unreachable**      | drop trains you can't make                                                                               | hides information; risks a short/empty list late at night; needs a bigger upstream fetch |
| **Dim + highlight**       | keep all, grey the unmakeable, accent the first catchable                                                | good, but the _number_ still shows departure time — you still do the mental subtraction  |
| **Leave-by countdown** ✅ | the relative number becomes "leave in N min" (departs − walk); negative collapses to "leave now" + faded | the number is the one you act on; no mental math; faded rows still visible for context   |

The leave-by countdown was chosen: it surfaces the single actionable number and
keeps the missed trains on screen (faded) so you still see you _just_ missed one.

## Design

### Config (`places.toml`)

A per-place `walk_minutes` in the transit block, next to `station`:

```toml
[[place]]
name = "Schöneweide"
station = "900180001"
walk_minutes = 10        # minutes on foot to the platform; 0 (default) = off
lines = ["S8", "S85", "S9"]
```

`#[serde(default)]` → **0**, which preserves today's behaviour exactly. The
"away / nearest-station" fallback (no configured place matched) also carries 0,
since we can't know your walk there.

### Data flow — the budget travels glued to the rows

```
places.toml  →  PlaceCfg.walk_minutes  →  Place.walk_minutes
                                              │ Place::resolved()
                                              ▼
                          ResolvedPlace.walk_minutes  (0 in the "away" branch)
                                              │ departures::fetch_for_place stamps each kept row
                                              ▼
                            Departure.walk_minutes  (same scalar on every row)
                                              │ widget::row() → TimeRowRef.walk_minutes
                                              ▼
                              lead_label(now, actual, walk)  on every clock tick
```

The budget rides on each `Departure` rather than on the `DeparturesState`
variants. It's the same value on ≤8 rows (trivially redundant), but it means:
`next_state` and its transition truth-table are **untouched**, and `Stale` keeps
the right budget for free (it keeps the prior `items`, which already carry it).

### Reachability is a per-tick display concern, not a fetch concern

The "can I still make it?" verdict depends on *now*, which drifts continuously;
the service's background poll is coarse (15 min). So the verdict lives in the
**widget**, on the same `clock::now()` tick that already counts the relative time
down — no new polling, no new subscription. The service stays a thin fetch+filter
that just attaches the budget. As you watch the open sidebar, trains cross the
boundary and fade live; a departed train's row is pruned on the same tick. (The
sidebar separately re-polls every 30 s while open so fresh trains keep arriving —
see the 2026-06-11 revision.)

### The label rule — `lead_label(now, departs, walk_minutes) -> (String, bool)`

```
walk_minutes == 0            → (relative_label(now, departs), false)   // unchanged "7 min" / "now"
else:
  slack   = (departs - now) - walk_minutes          // seconds you can still wait
  minutes = (slack + 30) / 60                        // nearest minute (same rounding as relative_label)
  token   = minutes <= 0 ? "now" : "{minutes} min"   // bare; a walk icon adds "leave"
  faded   = slack < 0                                // already missed
```

So `"now"` covers both "go right now to make it" (slack ≈ 0, **not** faded) and
"that window closed" (slack < 0, **faded**) — the fade is what disambiguates. The
widget prepends a `walk` symbolic (bundled `icons/walk.svg`) whenever
`walk_minutes > 0`, so the cell reads `[walk] 7 min · HH:MM` / `[walk] now · HH:MM`:
the icon carries the "leave in" sense the word "leave" used to, keeping the row
narrow. The `· HH:MM` is still the train's departure clock time.

### CSS

Two theme-independent rules (opacity, so no light-mode mirror):

```css
.ts-departure-row.ts-departure-unreachable { opacity: 0.4; }
.ts-departure-walk-icon { opacity: 0.85; }   /* match the time text's dimness */
```

Dims the whole row (badge included) so the glance lands on the first catchable
train. Distinct from `.ts-cancelled` (strikethrough + red), so a cancelled train
and an unreachable one read differently — and a row can be both.

## Tests

- `places.rs`: `walk_minutes` defaults to 0 when omitted and survives `resolved()`;
  the default config parses `walk_minutes = 10`.
- `departures.rs`: existing transition/parse tests unchanged (budget is additive);
  test constructors gain `walk_minutes: 0`.
- `widgets/departures.rs`: `lead_label` — zero-walk falls back to the plain label;
  positive slack → "N min"; zero slack → "now" not faded; negative slack → "now"
  + faded; the 1-minute-slack boundary reads "1 min". Plus `departed` — a row is
  hidden only once its train is past the 30 s grace.

## Out of scope

- Reordering/promoting the first catchable train (the fade already directs the eye).
- A walk budget for the "away / nearest station" case (we don't know it).
- Per-line or time-of-day walk variation; door-to-platform vs door-to-station nuance.
- ~~Hot-reloading `places.toml`~~ — shipped 2026-06-11 (live mtime-poll reload in `places.rs`).

## Migration note

`walk_minutes` is a new optional field. A `places.toml` written before this change
has no such line and so behaves exactly as before (budget 0). To enable it, add
`walk_minutes = N` under the place; first-run configs get `= 10` for Schöneweide.
