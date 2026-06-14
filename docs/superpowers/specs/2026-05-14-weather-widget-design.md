# Weather Widget: first sidebar content

**Status:** design approved 2026-05-14
**Scope:** new `hytte-services/src/geoclue.rs`, new `hytte-services/src/weather.rs`, new `trollshell/src/widgets/weather.rs`, CSS additions, sidebar wiring, env var documentation. First real content for the left sidebar landed in `2026-05-14-sidebar-design.md`.

## Motivation

The sidebar shipped on 2026-05-14 with a `Label("sidebar")` placeholder — the mechanism works (push semantics, frame integration, toggle chip). Now we replace the placeholder with phase-2 content. Weather is a good first widget: it exercises the sidebar's vertical card layout, introduces two new services (location + HTTP-fetched data), and is genuinely useful to the user during the day.

Two new services because location is generally useful (future widgets like timezone-aware clock, sunrise/sunset OSDs, etc.). Splitting `geoclue` out of `weather` upfront means the next location-dependent widget reuses the same service.

## Design

### Services

Two new modules in `crates/hytte-services/src/`:

**`geoclue.rs`** — owns location resolution.

```rust
pub struct LocationSnapshot {
    pub lat: f64,
    pub lon: f64,
    pub label_hint: Option<String>,  // for Configured source = the env-var city as forward-geocoded
    pub source: LocationSource,
}

pub enum LocationSource { GeoClue, Configured }

pub fn current() -> impl Signal<Item = Option<LocationSnapshot>> + 'static;
pub fn refresh();
pub fn service() -> impl HytteService;  // for .with() registration in main.rs
```

D-Bus flow on the system bus, via `hytte_bus::call`:

1. `GeoClue2.Manager.GetClient()` returns a client `ObjectPath`.
2. On the client, set `DesktopId = "trollshell"` and `RequestedAccuracyLevel = 4` (City).
3. Subscribe to `GeoClue2.Client.LocationUpdated(o old, o new)` signal.
4. Call `Client.Start()`.
5. On signal, read `Latitude` + `Longitude` + (optionally) `Description` from the new Location object path. Emit `Some(LocationSnapshot { source: GeoClue, … })`.

Geoclue timeout: 10 seconds from `Start()`. If no `LocationUpdated` arrives in that window (daemon not running, denied, etc.), fall back to the env-var path.

Env-var fallback: read `TROLLSHELL_WEATHER_CITY`. If unset, emit `None`. If set, forward-geocode via Open-Meteo's geocoding endpoint:

```
GET https://geocoding-api.open-meteo.com/v1/search
  ?name=Stockholm&count=1&language=en&format=json
```

Take the first result's `latitude`/`longitude`/`name`. Emit `Some(LocationSnapshot { source: Configured, label_hint: Some("Stockholm") })`.

`refresh()` semantics: cancel any in-flight geoclue subscription, issue a new `GetClient` + `Start` cycle, re-read the env var. Lets consumers recover from transient failures.

**`weather.rs`** — owns Open-Meteo fetch + condition mapping.

```rust
pub enum WeatherState {
    Loading,
    Resolved(WeatherSnapshot),
    Error(String),
}

pub struct WeatherSnapshot {
    pub location: String,
    pub temp_c: f64,
    pub apparent_c: f64,
    pub humidity_pct: u8,
    pub wind_kmh: f64,
    pub condition: Condition,
    pub fetched_at: SystemTime,
}

pub struct Condition {
    pub code: u8,
    pub label: &'static str,
    pub icon: &'static str,  // freedesktop symbolic name
}

pub fn current() -> impl Signal<Item = WeatherState> + 'static;
pub fn refresh();
pub fn service() -> impl HytteService;
```

Open-Meteo fetch:

```
GET https://api.open-meteo.com/v1/forecast
  ?latitude={lat}&longitude={lon}
  &current=temperature_2m,apparent_temperature,relative_humidity_2m,
           wind_speed_10m,weather_code
  &timezone=auto
```

Synchronous `ureq` call (matches `calendar.rs`'s existing pattern). Parse the `current` object via serde.

Reverse-geocode for friendly name (only when `source = GeoClue`; Configured source already has the name in `label_hint`). Open-Meteo has **no** reverse endpoint — `/v1/reverse` 404s — so use OSM Nominatim, sending a descriptive `User-Agent` (its policy rejects stock library ones):

```
GET https://nominatim.openstreetmap.org/reverse
  ?lat={lat}&lon={lon}&format=jsonv2&zoom=14&accept-language=en
```

Take `name` (the district at `zoom=14`, e.g. "Oberschöneweide"), falling back to the first segment of `display_name`. Cache the result by rounded `(lat * 100, lon * 100) as (i32, i32)` so we don't re-geocode every 15-min refresh when location is unchanged — which also keeps us polite to Nominatim.

Weather-code mapping is a pure `condition_for_code(u8) -> Condition`. WMO codes:

| code(s)                                | label         | icon (freedesktop)                   |
| -------------------------------------- | ------------- | ------------------------------------ |
| 0                                      | Clear         | `weather-clear-symbolic`             |
| 1, 2, 3                                | Partly cloudy | `weather-few-clouds-symbolic`        |
| 45, 48                                 | Fog           | `weather-fog-symbolic`               |
| 51, 53, 55, 56, 57, 61, 63, 65, 66, 67 | Rain          | `weather-showers-symbolic`           |
| 71, 73, 75, 77                         | Snow          | `weather-snow-symbolic`              |
| 80, 81, 82                             | Showers       | `weather-showers-scattered-symbolic` |
| 85, 86                                 | Snow showers  | `weather-snow-symbolic`              |
| 95, 96, 99                             | Thunderstorm  | `weather-storm-symbolic`             |
| \_                                     | Unknown       | `weather-severe-alert-symbolic`      |

Polling: `glib::timeout_add_seconds(15 * 60, …)` schedules the periodic refresh. On startup, fire one immediate fetch (don't make the user wait 15 minutes for the first paint).

`refresh()` semantics:

- If the current state is `Loading` (a fetch is in flight), no-op.
- Otherwise, kick off a fresh fetch. Don't reset state to `Loading` unless the previous state was `Error` (or initial) — when we already have a `Resolved` value, we keep showing it while the new fetch is in flight and replace atomically on success. Avoids a visible flicker every 15 minutes.

Error handling:

- Network failure / non-200 status / JSON parse error → `WeatherState::Error("network error")`.
- Geoclue resolved `None` (no env var, geoclue failed) → `WeatherState::Error("set $TROLLSHELL_WEATHER_CITY")`. Tells the user how to fix.

### Widget

New `trollshell/src/widgets/weather.rs`:

```rust
pub fn widget(monitor: &Monitor) -> gtk::Widget;
```

Resolved layout (vertical `gtk::Box` inside the sidebar card):

```
.ts-weather (column)
├── .ts-weather-location           "STOCKHOLM" (small, letter-spaced, alpha 0.6)
├── .ts-weather-headline (row)
│   ├── icon  (24-28 px)
│   └── temp  "18°"  (28-32 px font)
├── .ts-weather-condition          "Clear" (medium, alpha 0.85)
└── (detail rows, gtk::Box vertical, .ts-weather-details)
    ├── row "Feels like" — "16°"
    ├── row "Wind"       — "12 km/h"
    └── row "Humidity"   — "64%"
```

Loading state: centered `gtk::Spinner` + a "Loading weather…" label.

Error state: `⚠` symbolic icon + the error text from `WeatherState::Error(msg)`.

The widget subscribes to `weather::current()` and swaps its body between Loading/Resolved/Error by hiding/showing pre-built child Boxes (one per state) so we don't rebuild widget trees on every state change.

Concrete widget structure with the CSS classes used at each level:

```
gtk::Box (vertical, .ts-weather)
├── gtk::Label             (.ts-weather-location)        "STOCKHOLM"
├── gtk::Box (horizontal, .ts-weather-headline)
│   ├── gtk::Image         (.ts-weather-icon)            condition.icon
│   └── gtk::Label         (.ts-weather-temp)            "18°"
├── gtk::Label             (.ts-weather-condition)       "Clear"
└── gtk::Box (vertical, .ts-weather-details)
    ├── gtk::Box (horizontal, .ts-weather-detail)
    │   ├── gtk::Label     (.ts-weather-detail-label)    "Feels like"
    │   └── gtk::Label     (.ts-weather-detail-value)    "16°"
    ├── gtk::Box (horizontal, .ts-weather-detail)        "Wind" / "12 km/h"
    └── gtk::Box (horizontal, .ts-weather-detail)        "Humidity" / "64%"
```

CSS additions (`trollshell/style.css`):

```css
.ts-weather {
  padding: 4px;
}
.ts-weather-location {
  font-size: 11px;
  letter-spacing: 1.5px;
  color: alpha(currentColor, 0.6);
  margin-bottom: 10px;
}
.ts-weather-headline {
  /* horizontal Box. icon left of temp, both vertically centered */
}
.ts-weather-temp {
  font-size: 32px;
  font-weight: 300;
}
.ts-weather-condition {
  color: alpha(currentColor, 0.85);
  margin-top: -4px;
  margin-bottom: 14px;
}
.ts-weather-detail {
  /* horizontal Box. label hexpands left; value pinned right */
}
.ts-weather-detail-label {
  color: alpha(currentColor, 0.55);
}
.ts-weather-detail-value {
  color: white;
}
```

(`text-transform` is not supported in GTK CSS — the `.ts-weather-location` text is uppercased at the Rust side via `s.to_uppercase()` before being put into the label.)

Inherits `color: white` from the parent `.ts-sidebar` card. No background — sidebar provides it.

### Sidebar wiring

`trollshell/src/overlays/sidebar.rs::install()` currently has:

```rust
let placeholder = gtk::Label::new(Some("sidebar"));
placeholder.add_css_class("ts-sidebar-placeholder");
// ...
card.append(&placeholder);
```

Replace with:

```rust
card.append(&crate::widgets::weather::widget(monitor));
```

Drop the now-unused `.ts-sidebar-placeholder` CSS rule. Drop the `placeholder` local.

The weather widget's `install` also subscribes to `sidebar::open_signal(monitor)`:

```rust
glib::MainContext::default().spawn_local(
    sidebar::open_signal(monitor).for_each(move |open| {
        if open && data_is_stale() {
            weather::refresh();
        }
        async {}
    }),
);
```

`data_is_stale()` returns `true` when the current `WeatherState` is `Loading` (initial), `Error`, or `Resolved` with `fetched_at < now() - 5min`. So sidebar-open always forces a fetch unless we have a `Resolved` value younger than 5 minutes.

### Service registration

`trollshell/src/main.rs` calls `.with(weather::service()).with(geoclue::service())` in the `App::new(...)` builder chain, alongside the existing `.with(theme::service())` etc.

Both services start their reactive loops when the app boots. The widget can subscribe to their signals as soon as the bar is built.

## Tests

Unit tests in `weather.rs`:

| test                                   | scenario                     | expected                                                                |
| -------------------------------------- | ---------------------------- | ----------------------------------------------------------------------- |
| `condition_clear`                      | `condition_for_code(0)`      | `Condition { code: 0, label: "Clear", icon: "weather-clear-symbolic" }` |
| `condition_partly_cloudy`              | code 1, 2, 3                 | partly cloudy condition                                                 |
| `condition_fog`                        | code 45, 48                  | fog condition                                                           |
| `condition_rain`                       | code 61, 65                  | rain condition                                                          |
| `condition_snow`                       | code 71, 75                  | snow condition                                                          |
| `condition_thunderstorm`               | code 95, 99                  | thunderstorm condition                                                  |
| `condition_unknown_code`               | code 200 (unmapped)          | unknown condition with `weather-severe-alert-symbolic`                  |
| `parse_current_response_ok`            | parse a fixture JSON payload | populated `Current` struct                                              |
| `parse_current_response_missing_field` | partial payload              | parse error returned, not panic                                         |

Unit tests in `geoclue.rs`:

| test                          | scenario                                            | expected                                                               |
| ----------------------------- | --------------------------------------------------- | ---------------------------------------------------------------------- |
| `env_var_unset_returns_none`  | `TROLLSHELL_WEATHER_CITY` unset                     | resolve_configured_city returns None                                   |
| `env_var_set_calls_geocoding` | env var = "Stockholm", mock HTTP returns one result | resolves to LocationSnapshot with that lat/lon and `Configured` source |

D-Bus geoclue interaction is NOT unit-tested (compositor-dependent). The 15-min timer + sidebar-open refresh are also covered by interactive verification.

## Touched files

- `crates/hytte-services/src/geoclue.rs` — new
- `crates/hytte-services/src/weather.rs` — new
- `crates/hytte-services/src/lib.rs` — register both new modules
- `crates/hytte-services/Cargo.toml` — no new deps (`zbus`, `ureq`, `serde_json`, `serde` already present)
- `trollshell/src/widgets/weather.rs` — new
- `trollshell/src/widgets/mod.rs` — `pub mod weather;`
- `trollshell/src/overlays/sidebar.rs` — replace placeholder label with `weather::widget(monitor)`; drop the `.ts-sidebar-placeholder` class application
- `trollshell/src/main.rs` — register the two new services in the App builder chain
- `trollshell/style.css` — add `.ts-weather*` rules, drop the unused `.ts-sidebar-placeholder` rule

## Out of scope

- **Forecast view** (hourly, daily). MVP shows current weather only.
- **Settings panel entry** for changing city / units / refresh rate. Configured via env var for now; settings UI can come later.
- **Fahrenheit / Imperial units.** Always Celsius + km/h + mm. Future settings panel can expose a unit toggle.
- **Reverse-geocode for the Configured source.** Configured city already has its name from the env var path; no need to round-trip.
- **Live geoclue accuracy updates.** We take the first `LocationUpdated` and don't re-subscribe unless `refresh()` is called.
- **HTTPS proxy / pinned certificates.** Standard ureq + rustls is fine.
- **Long-term geoclue authorization via a .desktop file** for trollshell. We pass `DesktopId = "trollshell"`; if the system policy denies, the env-var fallback covers it.
- **Network state awareness.** If the device is offline, the fetch fails and we show Error; we don't subscribe to NetworkManager to skip the fetch proactively.
- **Persistent disk cache.** The reverse-geocode cache lives in process memory; restart clears it. Acceptable for daily-driver use.

## Verification

After landing:

1. `cargo build -p trollshell` clean.
2. `cargo test -p hytte-services` — all new unit tests pass.
3. Set `TROLLSHELL_WEATHER_CITY=Stockholm` (or your preferred city), launch trollshell. Open the sidebar.
4. Confirm the widget shows: location name on top, large temperature with weather icon, condition label, three detail rows (Feels like / Wind / Humidity).
5. Disable geoclue (`systemctl stop geoclue` or similar) and restart trollshell. Confirm the widget still resolves via the env var path. Location name in the widget should match the env var value or its forward-geocoded normalization.
6. Unset `TROLLSHELL_WEATHER_CITY` AND disable geoclue. Confirm the widget shows `Set $TROLLSHELL_WEATHER_CITY` error message.
7. Restart with geoclue running and no env var. Confirm the widget resolves via geoclue (location name comes from reverse-geocoding).
8. Disconnect from network. Wait for next 15-min refresh OR force-close + reopen the sidebar. Confirm the widget shows "network error".
9. Re-connect; next refresh recovers without restart.
10. Open the sidebar with stale data (>5 min since last fetch). Confirm the widget kicks an immediate refresh — observable as a brief Loading state if previous state was Error, or as a value update if previous state was Resolved.
