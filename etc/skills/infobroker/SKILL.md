---
name: infobroker
description: >-
  Read scoped data from the trollshell desktop (public-transport
  departures, weather, calendar) through the consent-gated infobroker. Use when a
  task needs live data from the desktop owner's machine — "when's my next train", "what's the
  weather", "what's on my calendar" — that only the desktop can source. Every
  datasource is behind an explicit human grant; ask for the minimum and handle a
  denial gracefully.
---

# infobroker — the trollshell data broker

`hytte-infobroker` is a small CLI that lets you read **scoped, consent-gated**
data from the running trollshell desktop. Its owner — a human — decides, per agent and
per datasource, whether you may read it. You never touch their files or daemons
directly — you ask the broker, and it answers only what you've been granted.

The whole surface is one CLI (this skill's `bin/hytte-infobroker`, a pointer to
the installed binary). There is no MCP server and no network endpoint — it talks
to a local Unix socket the desktop owns.

## Installing the CLI

`hytte-infobroker` is its own flake package, `packages.hytte-infobroker`
(#562) — built via `nix/plugin.nix`, the same warm-artifact derivation the
bundled widget plugins use. It is **not** shipped by the `trollshell` package
itself, which prunes `$out/bin` down to just the shell binary (#530), so it
needs its own line on PATH. Simplest is a `home.packages` entry in your
home-manager config:

```nix
home.packages = [ trollshell.packages.${system}.hytte-infobroker ];
```

(`trollshell` is the flake input, same as wherever `programs.trollshell` is
already configured from.) Once installed, this skill's `bin/hytte-infobroker`
finds it on PATH automatically — no per-skill setup needed.

## The model (read this once)

- **Identity is a token.** You authenticate as an _agent name_ (pick a stable
  one, e.g. `claude`). `hytte-infobroker auth` mints a short-lived **session
  token** and prints an `export HYTTE_INFOBROKER_TOKEN=…` line. `eval` it;
  subsequent `get`s read that env var.
- **Tokens are ephemeral, grants are durable.** A token lives ≤ 12 h and dies on
  a desktop restart or an explicit revoke — re-auth is cheap. The _grant_ (your
  agent × a datasource) is what persists; a human sets it once.
- **Consent is required.** If no grant covers you, `auth` (and `get`) are
  **denied** with a hint, and the human gets a desktop toast about your knock.
  Interactive "Allow this once?" prompting is not in this phase — a denial means
  the human must grant you first (in the infobroker panel, or by editing
  `grants.toml`). Don't retry in a loop; surface the hint and move on.

## The flow

```sh
# 1. Authenticate once per session (pick a stable agent name):
eval "$(hytte-infobroker auth --agent claude)"

# 2. Read scoped data (uses $HYTTE_INFOBROKER_TOKEN):
hytte-infobroker get departures            # next catchable S-Bahn departures (JSON)
hytte-infobroker get departures --limit 3  # just the next few
hytte-infobroker get weather               # current conditions (one JSON object)
hytte-infobroker get calendar --limit 5    # upcoming calendar events (JSON)

# 3. See what you're allowed to read:
hytte-infobroker grants list
```

### `auth --agent <name>`

Mints a session token. On success, stdout is exactly one eval-able line:

```
export HYTTE_INFOBROKER_TOKEN=3be6622c0256634e6839949e9631063e
```

On denial it exits non-zero and prints the reason + a how-to-grant hint to
stderr. **Do not** keep re-running it — a denial is a standing "not yet".

### `get <datasource> [--limit N]`

Fetches scoped data as JSON on stdout, authenticated by
`$HYTTE_INFOBROKER_TOKEN`. Three datasources are served:

- **`departures`** — the next catchable S-Bahn departures from the desktop
  owner's configured home station. Each row: `{ line, direction, hhmm, in_minutes,
delay_minutes, cancelled }`. Example:

  ```json
  [
    {
      "line": "S9",
      "direction": "Spandau",
      "hhmm": "16:42",
      "in_minutes": 7,
      "delay_minutes": 0,
      "cancelled": false
    }
  ]
  ```

  `in_minutes` is whole minutes from now; `delay_minutes` is lateness (0 = on
  time); a cancelled run has `cancelled: true`. `--limit` caps the rows.

- **`weather`** — the current reading for the desktop's configured location, as a
  single JSON **object** (not an array): `{ location, temp_c, apparent_c,
temp_max_c, temp_min_c, humidity_pct, wind_kmh, condition_code, condition_label,
condition_icon }`. Temperatures are °C, `wind_kmh` is km/h, `condition_code` is
  the raw WMO weather code with a human `condition_label`/`condition_icon`
  alongside. `--limit` doesn't apply (it's one reading).

- **`calendar`** — the upcoming calendar events. Each row: `{ start_unix,
end_unix, title, calendar }`; times are Unix seconds (format them yourself),
  `calendar` is the source calendar's name. `--limit` caps how many events come
  back.

**Datasource dependencies — read this.** Since #509 the broker no longer fetches
`departures` and `weather` itself; they are **routed through their provider widget
plugins** (`hytte-plugin-departures` / `hytte-plugin-weather`) over the desktop's
host query protocol. So each needs its plugin enabled and running: if it isn't,
the `get` fails with a message like

```
departures: no connected provider for datasource 'departures'
```

(likewise `weather: no connected provider for datasource 'weather'`). That's a
standing "the provider isn't up", **not** a consent denial — surface it and stop;
retrying won't help until the plugin is enabled. `calendar` has no such
dependency: the desktop pushes its upcoming events into the broker (the host's
`CalendarUpcoming` feed), so `get calendar` reads the broker's own live copy
directly.

### `grants list`

Prints your durable grants, one per line
(`agent  datasource  scope  decision`). Read-only introspection — handy to check
what you may read before you `get`.

## Consent etiquette (please follow)

- **Ask for the minimum.** Only `get` the datasource the task actually needs.
  Don't speculatively pull everything.
- **Auth once per session**, then reuse the env token. Don't re-`auth` before
  every `get`.
- **Handle denial gracefully.** If `auth`/`get` is denied, tell the human what
  you tried and that they can allow it in the _infobroker panel_ (the infobroker
  chip on the trollshell bar → the drawer's "Pending requests") or by adding a
  line to `grants.toml`. Then stop — do not spin.
- **A token can vanish mid-task** (TTL, restart, or revoke). If a `get` says the
  token is invalid/expired, re-`auth` once and retry; if _that_ is denied,
  you've been revoked — stop and surface it.

## The wire (for reference / other clients)

The CLI is the only supported client, but the protocol is deliberately boring —
newline-delimited JSON over `$XDG_RUNTIME_DIR/hytte-infobroker.sock` (0600,
same-user-only). One request object per line in, one response per line out:

```text
→ {"op":"auth","agent":"claude"}
← {"ok":true,"token":"…","expires_unix":1750000000,"agent":"claude"}

→ {"op":"get","token":"…","datasource":"departures","limit":5}
← {"ok":true,"datasource":"departures","departures":[ … ]}

→ {"op":"get","token":"…","datasource":"weather"}
← {"ok":true,"datasource":"weather","weather":{ … }}

→ {"op":"get","token":"…","datasource":"calendar","limit":5}
← {"ok":true,"datasource":"calendar","calendar":[ … ]}

→ {"op":"grants"}
← {"ok":true,"grants":[{"agent":"claude","datasource":"departures","scope":"*","decision":"always"}]}

← {"ok":false,"error":"…","hint":"…"}        (any denied request)
```

`get` is the same op for every datasource; only the `datasource` field and which
payload field the answer populates (`departures` / `weather` / `calendar`) differ.
A `departures`/`weather` `get` whose provider plugin isn't running comes back as
`{"ok":false,"error":"departures: no connected provider for datasource 'departures'"}`.
