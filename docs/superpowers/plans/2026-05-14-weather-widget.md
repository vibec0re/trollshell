# Weather Widget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the first phase-2 content for trollshell's left sidebar — a weather widget that resolves location via geoclue (env var fallback `TROLLSHELL_WEATHER_CITY`) and fetches current weather from Open-Meteo every 15 minutes.

**Architecture:** Two new services — `hytte-services/src/geoclue.rs` owns location (D-Bus + env var fallback + forward-geocoding for the configured case) and `hytte-services/src/weather.rs` subscribes to it, fetches Open-Meteo current weather, maps WMO codes to icon+label, and exposes a `WeatherState` signal (Loading | Resolved | Error). The sidebar's placeholder label is replaced with a new `trollshell/src/widgets/weather.rs` widget that subscribes to `weather::current()` and renders three different layouts per state. Opening the sidebar with stale data (>5 min old, or Loading, or Error) kicks an immediate refresh.

**Tech Stack:** Rust 1.94+, GTK4 + libadwaita via the `hytte` workspace, `ureq` 3 + rustls for HTTP (already a workspace dep), `serde` + `serde_json` for parsing, `zbus` 5 via the `hytte_bus` wrapper for geoclue D-Bus calls, futures-signals `Mutable` + tokio runtime for service state.

**Spec:** `docs/superpowers/specs/2026-05-14-weather-widget-design.md`

---

## File Map

| file                                   | role                                                                                        | new/edit |
| -------------------------------------- | ------------------------------------------------------------------------------------------- | -------- |
| `crates/hytte-services/src/weather.rs` | weather service: types, condition mapping, Open-Meteo fetch, polling, public API            | new      |
| `crates/hytte-services/src/geoclue.rs` | location service: D-Bus + env var fallback, forward-geocode                                 | new      |
| `crates/hytte-services/src/lib.rs`     | `pub mod weather; pub mod geoclue;`                                                         | edit     |
| `trollshell/src/widgets/weather.rs`    | sidebar weather widget — Loading/Resolved/Error rendering                                   | new      |
| `trollshell/src/widgets/mod.rs`        | `pub mod weather;`                                                                          | edit     |
| `trollshell/src/overlays/sidebar.rs`   | replace placeholder label with `weather::widget(monitor)`; add on-open refresh subscription | edit     |
| `trollshell/src/main.rs`               | register both services via `.with(weather::service()).with(geoclue::service())`             | edit     |
| `trollshell/style.css`                 | `.ts-weather*` rules; drop `.ts-sidebar-placeholder`                                        | edit     |

No new workspace dependencies. `ureq`, `serde`, `serde_json`, `zbus`, `hytte_bus`, `tokio`, `chrono` are all already present in `hytte-services/Cargo.toml`.

---

## Task 1: Weather condition mapping (pure function, TDD)

**Files:**

- Create: `crates/hytte-services/src/weather.rs`
- Modify: `crates/hytte-services/src/lib.rs`

- [ ] **Step 1: Wire the new module into `lib.rs`**

Open `crates/hytte-services/src/lib.rs`. Add `pub mod weather;` in alphabetical position (between `vpn` and `wallpaper`, or wherever the alphabetical position lands — match the existing order).

- [ ] **Step 2: Create `weather.rs` with the condition type and mapping function**

Create `crates/hytte-services/src/weather.rs` with this initial content:

```rust
//! Weather service. Subscribes to `geoclue` for location, fetches current
//! conditions from Open-Meteo every 15 minutes, exposes a `WeatherState`
//! signal consumed by the sidebar weather widget.
//!
//! See `docs/superpowers/specs/2026-05-14-weather-widget-design.md`.

/// Human-friendly condition resolved from an Open-Meteo WMO weather code.
/// The icon is a freedesktop symbolic name (resolved by GTK's icon theme
/// lookup at render time).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Condition {
    pub code: u8,
    pub label: &'static str,
    pub icon: &'static str,
}

/// Map an Open-Meteo WMO `weather_code` to its human label + icon.
/// Pure function — testable in isolation.
#[must_use]
pub fn condition_for_code(code: u8) -> Condition {
    match code {
        0 => Condition { code, label: "Clear", icon: "weather-clear-symbolic" },
        1 | 2 | 3 => Condition { code, label: "Partly cloudy", icon: "weather-few-clouds-symbolic" },
        45 | 48 => Condition { code, label: "Fog", icon: "weather-fog-symbolic" },
        51 | 53 | 55 | 56 | 57 | 61 | 63 | 65 | 66 | 67 => {
            Condition { code, label: "Rain", icon: "weather-showers-symbolic" }
        }
        71 | 73 | 75 | 77 => {
            Condition { code, label: "Snow", icon: "weather-snow-symbolic" }
        }
        80 | 81 | 82 => {
            Condition { code, label: "Showers", icon: "weather-showers-scattered-symbolic" }
        }
        85 | 86 => {
            Condition { code, label: "Snow showers", icon: "weather-snow-symbolic" }
        }
        95 | 96 | 99 => {
            Condition { code, label: "Thunderstorm", icon: "weather-storm-symbolic" }
        }
        _ => Condition { code, label: "Unknown", icon: "weather-severe-alert-symbolic" },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_clear() {
        let c = condition_for_code(0);
        assert_eq!(c.label, "Clear");
        assert_eq!(c.icon, "weather-clear-symbolic");
    }

    #[test]
    fn condition_partly_cloudy_branches() {
        for code in [1, 2, 3] {
            assert_eq!(condition_for_code(code).label, "Partly cloudy");
        }
    }

    #[test]
    fn condition_fog_branches() {
        for code in [45, 48] {
            assert_eq!(condition_for_code(code).label, "Fog");
        }
    }

    #[test]
    fn condition_rain_branches() {
        for code in [51, 53, 55, 56, 57, 61, 63, 65, 66, 67] {
            assert_eq!(condition_for_code(code).label, "Rain");
        }
    }

    #[test]
    fn condition_snow_branches() {
        for code in [71, 73, 75, 77] {
            assert_eq!(condition_for_code(code).label, "Snow");
        }
    }

    #[test]
    fn condition_thunderstorm_branches() {
        for code in [95, 96, 99] {
            assert_eq!(condition_for_code(code).label, "Thunderstorm");
        }
    }

    #[test]
    fn condition_unknown_code() {
        let c = condition_for_code(200);
        assert_eq!(c.label, "Unknown");
        assert_eq!(c.icon, "weather-severe-alert-symbolic");
    }
}
```

- [ ] **Step 3: Run the tests**

```bash
cd /home/choom/src/trollshell
cargo test -p hytte-services --tests weather
```

Expected: 7 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/weather.rs crates/hytte-services/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(weather): module skeleton + condition mapping

New hytte-services/src/weather.rs holding the Condition type and the
pure condition_for_code(u8) helper that maps Open-Meteo WMO codes to
human labels + freedesktop symbolic icon names. Service skeleton and
HTTP fetch land in follow-ups.
EOF
)"
```

---

## Task 2: Open-Meteo response parser (TDD)

**Files:**

- Modify: `crates/hytte-services/src/weather.rs`

- [ ] **Step 1: Add the response types + parser**

Append to `crates/hytte-services/src/weather.rs`, above the `#[cfg(test)] mod tests` block:

```rust
use serde::Deserialize;

/// Raw Open-Meteo current-weather response. Field names match the API's
/// `current=` query parameters.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OpenMeteoResponse {
    pub current: OpenMeteoCurrent,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OpenMeteoCurrent {
    pub temperature_2m: f64,
    pub apparent_temperature: f64,
    pub relative_humidity_2m: u8,
    pub wind_speed_10m: f64,
    pub weather_code: u8,
}

/// Parse a raw Open-Meteo response body. Thin wrapper around `serde_json`
/// so the call site doesn't need to import serde.
pub(crate) fn parse_open_meteo(body: &str) -> Result<OpenMeteoResponse, serde_json::Error> {
    serde_json::from_str(body)
}
```

- [ ] **Step 2: Add tests for the parser**

Inside the existing `#[cfg(test)] mod tests` block, append:

```rust
    #[test]
    fn parse_current_response_ok() {
        let body = r#"{
            "current": {
                "temperature_2m": 18.3,
                "apparent_temperature": 16.1,
                "relative_humidity_2m": 64,
                "wind_speed_10m": 12.5,
                "weather_code": 0
            }
        }"#;
        let parsed = parse_open_meteo(body).expect("should parse");
        let c = parsed.current;
        assert!((c.temperature_2m - 18.3).abs() < 1e-6);
        assert!((c.apparent_temperature - 16.1).abs() < 1e-6);
        assert_eq!(c.relative_humidity_2m, 64);
        assert!((c.wind_speed_10m - 12.5).abs() < 1e-6);
        assert_eq!(c.weather_code, 0);
    }

    #[test]
    fn parse_current_response_missing_field_errors() {
        // No `weather_code` field → parse should fail, not panic.
        let body = r#"{
            "current": {
                "temperature_2m": 18.3,
                "apparent_temperature": 16.1,
                "relative_humidity_2m": 64,
                "wind_speed_10m": 12.5
            }
        }"#;
        assert!(parse_open_meteo(body).is_err());
    }
```

- [ ] **Step 3: Run the tests**

```bash
cargo test -p hytte-services --tests weather
```

Expected: 9 tests pass (7 from Task 1 + 2 new).

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/weather.rs
git commit -m "$(cat <<'EOF'
feat(weather): Open-Meteo response parser

OpenMeteoResponse + OpenMeteoCurrent serde types matching the
current= query parameters. Thin parse_open_meteo helper to keep
serde out of the call site. Crate-private (pub(crate)) — the
service translates to the public WeatherSnapshot shape.
EOF
)"
```

---

## Task 3: Weather service skeleton (state, Service impl, public API)

**Files:**

- Modify: `crates/hytte-services/src/weather.rs`

This task adds `WeatherSnapshot`, `WeatherState`, the public API (`current()`, `refresh()`, `service()`), and the `Service` trait impl. The actual fetch isn't wired yet — `refresh()` is a no-op stub until Task 8.

- [ ] **Step 1: Add the public types**

Append to `crates/hytte-services/src/weather.rs`, just above the parser section (so type ordering goes: Condition → public state types → response parser → tests):

```rust
use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{registry, Service};
use std::time::SystemTime;

/// Top-level state observable by the widget.
#[derive(Clone, Debug)]
pub enum WeatherState {
    /// Initial state — first fetch hasn't completed yet.
    Loading,
    /// Latest authoritative snapshot.
    Resolved(WeatherSnapshot),
    /// Last fetch (or location resolution) failed. The string is shown to
    /// the user; keep it short and actionable.
    Error(String),
}

/// One authoritative weather sample. All scalar units fixed at MVP:
/// Celsius for temperature, percentage for humidity, km/h for wind.
#[derive(Clone, Debug)]
pub struct WeatherSnapshot {
    pub location: String,
    pub temp_c: f64,
    pub apparent_c: f64,
    pub humidity_pct: u8,
    pub wind_kmh: f64,
    pub condition: Condition,
    pub fetched_at: SystemTime,
}

#[doc(hidden)]
pub struct WeatherHandles {
    pub(crate) state: Mutable<WeatherState>,
}

impl Default for WeatherHandles {
    fn default() -> Self {
        Self {
            state: Mutable::new(WeatherState::Loading),
        }
    }
}

/// Marker type for the weather service.
pub struct WeatherService;

impl Service for WeatherService {
    type Handles = WeatherHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        // Real fetch loop wired up in a follow-up. For now the service
        // just publishes the initial Loading state.
        WeatherHandles::default()
    }
}

#[must_use]
pub fn service() -> WeatherService {
    WeatherService
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Signal of the current weather state. Subscribers see every transition
/// between Loading / Resolved / Error.
pub fn current() -> impl Signal<Item = WeatherState> + 'static {
    registry::with(|r| {
        r.get::<WeatherHandles>()
            .expect("weather::service() not registered")
            .state
            .signal_cloned()
    })
}

/// Force a fresh fetch. No-op stub for now — Task 8 wires this to the
/// actual fetch loop via an mpsc channel.
pub fn refresh() {
    // Intentional no-op until Task 8.
}
```

- [ ] **Step 2: Build the crate**

```bash
cargo build -p hytte-services
```

Expected: builds clean. (Some `dead_code` warnings on the unused `pub(crate)` parser may appear — acceptable, will clear when Task 4 calls `parse_open_meteo`.)

- [ ] **Step 3: Run the tests**

```bash
cargo test -p hytte-services --tests weather
```

Expected: 9 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/weather.rs
git commit -m "$(cat <<'EOF'
feat(weather): service skeleton with WeatherState signal

Adds WeatherState (Loading | Resolved | Error), WeatherSnapshot, the
WeatherHandles registry entry, Service impl, and the public current()
+ refresh() API. refresh() is a no-op stub until the fetch loop lands.
Initial state is Loading; widget will render a spinner until the first
fetch resolves.
EOF
)"
```

---

## Task 4: Blocking HTTP helper for current weather

**Files:**

- Modify: `crates/hytte-services/src/weather.rs`

The Open-Meteo fetch is a blocking call (`ureq`), wrapped here so the runtime task can use `tokio::task::spawn_blocking`.

- [ ] **Step 1: Add the helper**

Append to `crates/hytte-services/src/weather.rs`, after the existing public API and before the response parser section:

```rust
/// Fetch the current weather for `(lat, lon)` from Open-Meteo. Blocking
/// `ureq` call — run on a tokio blocking pool thread.
pub(crate) fn fetch_current_blocking(lat: f64, lon: f64) -> Result<OpenMeteoCurrent, String> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast\
         ?latitude={lat}&longitude={lon}\
         &current=temperature_2m,apparent_temperature,relative_humidity_2m,\
         wind_speed_10m,weather_code\
         &timezone=auto"
    );

    let mut resp = ureq::get(&url)
        .call()
        .map_err(|e| format!("network error: {e}"))?;

    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read error: {e}"))?;

    let parsed = parse_open_meteo(&body)
        .map_err(|e| format!("parse error: {e}"))?;

    Ok(parsed.current)
}
```

- [ ] **Step 2: Build to confirm compilation**

```bash
cargo build -p hytte-services
```

Expected: builds clean.

- [ ] **Step 3: Commit**

```bash
git add crates/hytte-services/src/weather.rs
git commit -m "$(cat <<'EOF'
feat(weather): blocking ureq fetch helper for current conditions

fetch_current_blocking(lat, lon) hits Open-Meteo's /v1/forecast with
current=temperature_2m,apparent_temperature,relative_humidity_2m,
wind_speed_10m,weather_code and timezone=auto. Returns the parsed
OpenMeteoCurrent or a short error string. Intended for spawn_blocking
in Task 8's fetch loop.
EOF
)"
```

---

## Task 5: Reverse-geocode helper + in-process cache

**Files:**

- Modify: `crates/hytte-services/src/weather.rs`

When location source is `GeoClue`, the snapshot has no friendly name — we reverse-geocode lat/lon via Open-Meteo's geocoding API. Cache by rounded `(i32, i32)` coordinates so we don't refetch on every 15-min weather poll.

- [ ] **Step 1: Add the helper and cache**

Append to `crates/hytte-services/src/weather.rs`, after `fetch_current_blocking`:

```rust
use std::collections::HashMap;
use std::sync::Mutex;

/// Geocoding response — only the fields we use.
#[derive(Debug, Clone, Deserialize)]
struct GeocodingResponse {
    #[serde(default)]
    results: Vec<GeocodingResult>,
}

#[derive(Debug, Clone, Deserialize)]
struct GeocodingResult {
    name: String,
}

/// In-process cache keyed by rounded (lat * 100, lon * 100) so changes
/// within ~1 km don't re-hit the geocoding API. Cleared on process restart;
/// acceptable for daily-driver use.
static REVERSE_GEOCODE_CACHE: Mutex<Option<HashMap<(i32, i32), String>>> = Mutex::new(None);

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn cache_key(lat: f64, lon: f64) -> (i32, i32) {
    ((lat * 100.0).round() as i32, (lon * 100.0).round() as i32)
}

/// Reverse-geocode `(lat, lon)` to a friendly name via Open-Meteo. Cached.
/// Blocking `ureq` — run via spawn_blocking.
pub(crate) fn reverse_geocode_blocking(lat: f64, lon: f64) -> Result<String, String> {
    let key = cache_key(lat, lon);

    if let Ok(guard) = REVERSE_GEOCODE_CACHE.lock()
        && let Some(map) = guard.as_ref()
        && let Some(cached) = map.get(&key)
    {
        return Ok(cached.clone());
    }

    let url = format!(
        "https://geocoding-api.open-meteo.com/v1/reverse\
         ?latitude={lat}&longitude={lon}&count=1&language=en"
    );

    let mut resp = ureq::get(&url)
        .call()
        .map_err(|e| format!("reverse-geocode network error: {e}"))?;

    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("reverse-geocode read error: {e}"))?;

    let parsed: GeocodingResponse = serde_json::from_str(&body)
        .map_err(|e| format!("reverse-geocode parse error: {e}"))?;

    let name = parsed
        .results
        .into_iter()
        .next()
        .map(|r| r.name)
        .ok_or_else(|| "reverse-geocode: no results".to_string())?;

    if let Ok(mut guard) = REVERSE_GEOCODE_CACHE.lock() {
        guard.get_or_insert_with(HashMap::new).insert(key, name.clone());
    }

    Ok(name)
}
```

- [ ] **Step 2: Add a cache key test**

Inside the `#[cfg(test)] mod tests` block, append:

```rust
    #[test]
    fn cache_key_rounds_to_centidegrees() {
        // Stockholm-ish: lat 59.3293, lon 18.0686
        let k1 = cache_key(59.3293, 18.0686);
        // Tiny drift (~50 m) should hit the same cache key.
        let k2 = cache_key(59.3294, 18.0687);
        assert_eq!(k1, k2);
        // A meaningful change (~1 km) should miss the cache.
        let k3 = cache_key(59.34, 18.08);
        assert_ne!(k1, k3);
    }
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p hytte-services --tests weather
```

Expected: 10 tests pass (9 + new cache test).

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/weather.rs
git commit -m "$(cat <<'EOF'
feat(weather): reverse-geocode helper with in-process cache

reverse_geocode_blocking(lat, lon) hits Open-Meteo's geocoding /v1/reverse
endpoint and returns the first result's name. Cached by rounded
(lat * 100, lon * 100) so 15-min poll cycles for a stationary device
don't repeatedly hit the geocoding API. Cache cleared on process
restart — acceptable for daily-driver use.
EOF
)"
```

---

## Task 6: Geoclue service skeleton + env-var fallback

**Files:**

- Create: `crates/hytte-services/src/geoclue.rs`
- Modify: `crates/hytte-services/src/lib.rs`

Public API for location + the env-var path. D-Bus implementation lands in Task 7.

- [ ] **Step 1: Wire `pub mod geoclue;` into `lib.rs`**

Open `crates/hytte-services/src/lib.rs`. Add `pub mod geoclue;` in alphabetical position (after `displays`, before `hooks`).

- [ ] **Step 2: Create `geoclue.rs` with types + env-var path**

Create `crates/hytte-services/src/geoclue.rs`:

```rust
//! Location service. Resolves `(lat, lon)` from either:
//! - `org.freedesktop.GeoClue2` over the system D-Bus, OR
//! - the `TROLLSHELL_WEATHER_CITY` env var, forward-geocoded via Open-Meteo
//!   when geoclue is unavailable or times out.
//!
//! See `docs/superpowers/specs/2026-05-14-weather-widget-design.md`.

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{registry, runtime, Service};
use serde::Deserialize;

const ENV_VAR: &str = "TROLLSHELL_WEATHER_CITY";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocationSource {
    GeoClue,
    Configured,
}

#[derive(Clone, Debug)]
pub struct LocationSnapshot {
    pub lat: f64,
    pub lon: f64,
    /// For `Configured`: the env-var value, normalized by Open-Meteo's
    /// geocoding (e.g. "stockholm" → "Stockholm"). For `GeoClue`: `None` —
    /// weather service reverse-geocodes for the displayed name.
    pub label_hint: Option<String>,
    pub source: LocationSource,
}

#[doc(hidden)]
pub struct GeoClueHandles {
    pub(crate) snapshot: Mutable<Option<LocationSnapshot>>,
}

impl Default for GeoClueHandles {
    fn default() -> Self {
        Self {
            snapshot: Mutable::new(None),
        }
    }
}

pub struct GeoClueService;

impl Service for GeoClueService {
    type Handles = GeoClueHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = GeoClueHandles::default();
        let snapshot = handles.snapshot.clone();
        // Kick the initial resolution. D-Bus + env-var fallback both run
        // from this task. Task 7 fills in the D-Bus path.
        rt.spawn(async move {
            try_resolve(snapshot).await;
        });
        handles
    }
}

#[must_use]
pub fn service() -> GeoClueService {
    GeoClueService
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Signal of the current location, or None if neither geoclue nor the
/// env var resolved. Consumers (e.g. `weather`) subscribe and react.
pub fn current() -> impl Signal<Item = Option<LocationSnapshot>> + 'static {
    registry::with(|r| {
        r.get::<GeoClueHandles>()
            .expect("geoclue::service() not registered")
            .snapshot
            .signal_cloned()
    })
}

/// Synchronous snapshot of the current location. Useful for the weather
/// fetch loop which needs to read the value without going through the
/// async signal pipeline.
#[must_use]
pub fn current_snapshot() -> Option<LocationSnapshot> {
    registry::with(|r| {
        r.get::<GeoClueHandles>()
            .and_then(|h| h.snapshot.get_cloned())
    })
}

/// Re-attempt geoclue (and env-var fallback). Use when a previous resolution
/// returned None or you want the freshest fix available.
pub fn refresh() {
    let Some(snapshot) = registry::with(|r| {
        r.get::<GeoClueHandles>().map(|h| h.snapshot.clone())
    }) else {
        return;
    };
    runtime::handle().spawn(async move {
        try_resolve(snapshot).await;
    });
}

// ── Resolution flow ──────────────────────────────────────────────────────────

/// One resolution attempt. For now: env-var only (Task 7 adds D-Bus first).
async fn try_resolve(snapshot: Mutable<Option<LocationSnapshot>>) {
    if let Some(loc) = resolve_from_env().await {
        snapshot.set(Some(loc));
    } else {
        snapshot.set(None);
    }
}

/// Read `TROLLSHELL_WEATHER_CITY`, forward-geocode via Open-Meteo.
/// Returns None when the env var is unset OR the geocoding lookup fails.
async fn resolve_from_env() -> Option<LocationSnapshot> {
    let city = std::env::var(ENV_VAR).ok()?;
    let city = city.trim();
    if city.is_empty() {
        return None;
    }
    let city_owned = city.to_string();
    let result = tokio::task::spawn_blocking(move || forward_geocode_blocking(&city_owned))
        .await
        .ok()?
        .ok()?;
    Some(LocationSnapshot {
        lat: result.latitude,
        lon: result.longitude,
        label_hint: Some(result.name),
        source: LocationSource::Configured,
    })
}

#[derive(Debug, Clone, Deserialize)]
struct GeocodingResponse {
    #[serde(default)]
    results: Vec<GeocodingHit>,
}

#[derive(Debug, Clone, Deserialize)]
struct GeocodingHit {
    name: String,
    latitude: f64,
    longitude: f64,
}

fn forward_geocode_blocking(name: &str) -> Result<GeocodingHit, String> {
    let url = format!(
        "https://geocoding-api.open-meteo.com/v1/search\
         ?name={name}&count=1&language=en&format=json"
    );
    let mut resp = ureq::get(&url)
        .call()
        .map_err(|e| format!("forward-geocode network error: {e}"))?;
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("forward-geocode read error: {e}"))?;
    let parsed: GeocodingResponse = serde_json::from_str(&body)
        .map_err(|e| format!("forward-geocode parse error: {e}"))?;
    parsed
        .results
        .into_iter()
        .next()
        .ok_or_else(|| format!("forward-geocode: no result for {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_geocode_parses_first_result() {
        let body = r#"{
            "results": [
                {"name": "Stockholm", "latitude": 59.32938, "longitude": 18.06871}
            ]
        }"#;
        let parsed: GeocodingResponse = serde_json::from_str(body).unwrap();
        let first = parsed.results.into_iter().next().unwrap();
        assert_eq!(first.name, "Stockholm");
        assert!((first.latitude - 59.32938).abs() < 1e-5);
        assert!((first.longitude - 18.06871).abs() < 1e-5);
    }

    #[test]
    fn forward_geocode_handles_no_results() {
        let body = r#"{"results": []}"#;
        let parsed: GeocodingResponse = serde_json::from_str(body).unwrap();
        assert!(parsed.results.is_empty());
    }

    #[test]
    fn forward_geocode_handles_missing_results_field() {
        let body = r#"{}"#;
        let parsed: GeocodingResponse = serde_json::from_str(body).unwrap();
        assert!(parsed.results.is_empty());
    }
}
```

- [ ] **Step 3: Build + test**

```bash
cargo build -p hytte-services
cargo test -p hytte-services --tests geoclue
```

Expected: build clean, 3 geoclue tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/geoclue.rs crates/hytte-services/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(geoclue): service skeleton with env-var fallback path

Public API: current() signal of Option<LocationSnapshot>, refresh()
trigger. Service spawns one resolution attempt at start; refresh()
spawns another. Currently env-var only — TROLLSHELL_WEATHER_CITY is
forward-geocoded via Open-Meteo's /v1/search. D-Bus geoclue path lands
in the follow-up.
EOF
)"
```

---

## Task 7: Geoclue D-Bus client

**Files:**

- Modify: `crates/hytte-services/src/geoclue.rs`

Add the actual D-Bus path that takes precedence over the env var. Failure (no service, timeout, permission denied) falls back to `resolve_from_env`.

- [ ] **Step 1: Add the D-Bus helper**

In `crates/hytte-services/src/geoclue.rs`, replace the existing `try_resolve` function with one that tries D-Bus first:

```rust
use std::time::Duration;

const GEOCLUE_DEST: &str = "org.freedesktop.GeoClue2";
const MANAGER_PATH: &str = "/org/freedesktop/GeoClue2/Manager";
const MANAGER_IFACE: &str = "org.freedesktop.GeoClue2.Manager";
const CLIENT_IFACE: &str = "org.freedesktop.GeoClue2.Client";
const LOCATION_IFACE: &str = "org.freedesktop.GeoClue2.Location";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";
const DESKTOP_ID: &str = "trollshell";
/// Geoclue accuracy level 4 = City. Higher = more precise; we don't need it.
const ACCURACY_CITY: u32 = 4;
const GEOCLUE_TIMEOUT: Duration = Duration::from_secs(10);

/// Try geoclue first (10 s budget); on timeout/failure, try the env var.
async fn try_resolve(snapshot: Mutable<Option<LocationSnapshot>>) {
    match tokio::time::timeout(GEOCLUE_TIMEOUT, resolve_via_geoclue()).await {
        Ok(Some(loc)) => {
            snapshot.set(Some(loc));
            return;
        }
        Ok(None) => {
            tracing::info!("geoclue: no usable fix; trying env var fallback");
        }
        Err(_) => {
            tracing::warn!(
                timeout_s = GEOCLUE_TIMEOUT.as_secs(),
                "geoclue: timed out; trying env var fallback"
            );
        }
    }
    snapshot.set(resolve_from_env().await);
}

/// One geoclue resolution attempt. Returns None on any D-Bus error so the
/// caller can fall through to the env var.
async fn resolve_via_geoclue() -> Option<LocationSnapshot> {
    use hytte_bus::{call, signals, BusKind};
    use zbus::zvariant::OwnedObjectPath;

    // 1. Manager.GetClient → ObjectPath
    let client_path: OwnedObjectPath = call(GEOCLUE_DEST)
        .bus(BusKind::System)
        .path(MANAGER_PATH)
        .interface(MANAGER_IFACE)
        .method("GetClient")
        .send()
        .await
        .map_err(|e| tracing::warn!(error = %e, "geoclue: GetClient failed"))
        .ok()?;

    let client_path_str = client_path.as_str().to_string();

    // 2. Set DesktopId via Properties.Set (s ss v)
    let _: () = call(GEOCLUE_DEST)
        .bus(BusKind::System)
        .path(&client_path_str)
        .interface(PROPERTIES_IFACE)
        .method("Set")
        .args((
            CLIENT_IFACE.to_string(),
            "DesktopId".to_string(),
            zbus::zvariant::Value::new(DESKTOP_ID).try_to_owned().ok()?,
        ))
        .send()
        .await
        .map_err(|e| tracing::warn!(error = %e, "geoclue: set DesktopId failed"))
        .ok()?;

    // 3. Set RequestedAccuracyLevel = 4 (City)
    let _: () = call(GEOCLUE_DEST)
        .bus(BusKind::System)
        .path(&client_path_str)
        .interface(PROPERTIES_IFACE)
        .method("Set")
        .args((
            CLIENT_IFACE.to_string(),
            "RequestedAccuracyLevel".to_string(),
            zbus::zvariant::Value::new(ACCURACY_CITY).try_to_owned().ok()?,
        ))
        .send()
        .await
        .map_err(|e| tracing::warn!(error = %e, "geoclue: set RequestedAccuracyLevel failed"))
        .ok()?;

    // 4. Subscribe to LocationUpdated before calling Start, so we don't miss
    //    a fast first emission.
    let sub = signals::subscribe(GEOCLUE_DEST)
        .bus(BusKind::System)
        .path(&client_path_str)
        .interface(CLIENT_IFACE)
        .member("LocationUpdated")
        .start()
        .await
        .map_err(|e| tracing::warn!(error = %e, "geoclue: subscribe failed"))
        .ok()?;

    // 5. Client.Start()
    let _: () = call(GEOCLUE_DEST)
        .bus(BusKind::System)
        .path(&client_path_str)
        .interface(CLIENT_IFACE)
        .method("Start")
        .send()
        .await
        .map_err(|e| tracing::warn!(error = %e, "geoclue: Start failed"))
        .ok()?;

    // 6. Wait for the first LocationUpdated. The body is (old: o, new: o).
    let mut events = sub.events();
    use futures_util::StreamExt;
    let event = events.next().await?;
    let (_old, new_path): (OwnedObjectPath, OwnedObjectPath) =
        event.body.body().deserialize().ok()?;
    let new_path_str = new_path.as_str().to_string();

    // 7. Read Latitude + Longitude from the new Location object.
    let lat: f64 = read_double_property(&new_path_str, "Latitude").await?;
    let lon: f64 = read_double_property(&new_path_str, "Longitude").await?;

    Some(LocationSnapshot {
        lat,
        lon,
        label_hint: None,
        source: LocationSource::GeoClue,
    })
}

async fn read_double_property(path: &str, prop: &str) -> Option<f64> {
    use hytte_bus::{call, BusKind};
    use zbus::zvariant::OwnedValue;

    let val: OwnedValue = call(GEOCLUE_DEST)
        .bus(BusKind::System)
        .path(path)
        .interface(PROPERTIES_IFACE)
        .method("Get")
        .args((LOCATION_IFACE.to_string(), prop.to_string()))
        .send()
        .await
        .ok()?;

    f64::try_from(&val).ok()
}
```

Notes for the implementer:

- The exact `hytte_bus::call(...).args(...).send()` API may differ slightly from this pseudocode. Check `crates/hytte-bus/src/call.rs` for the actual builder methods and `crates/hytte-services/src/upower.rs` / `logind.rs` for working call examples. Adapt the call shape but preserve the sequence.
- `hytte_bus::signals::subscribe` similarly — check `crates/hytte-bus/src/signals.rs` for the actual builder.
- If `OwnedObjectPath` is awkward, `String` may be acceptable for `path()` parameters.
- The two-step Properties.Set / Properties.Get dance is verbose. If `hytte_bus` exposes property setters/getters more directly (search for `set_property` or similar), use those instead and note the deviation in your report.

- [ ] **Step 2: Build the crate**

```bash
cargo build -p hytte-services
```

Expected: builds clean. If there are API mismatches with `hytte_bus`, adapt the call shape; if unsure, report DONE_WITH_CONCERNS and let the reviewer flag.

- [ ] **Step 3: Run tests**

```bash
cargo test -p hytte-services --tests geoclue
```

Expected: 3 geoclue tests still pass (the D-Bus path is not unit-tested; integration is interactive).

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/geoclue.rs
git commit -m "$(cat <<'EOF'
feat(geoclue): D-Bus client with 10s timeout + env var fallback

try_resolve now races GeoClue2 (10 s) against falling back to the env
var path. The geoclue flow: Manager.GetClient → set DesktopId +
RequestedAccuracyLevel via Properties.Set → subscribe to
LocationUpdated → Client.Start → read Latitude+Longitude from the new
Location object via Properties.Get. Any failure or the timeout drops
through to the existing resolve_from_env() path.
EOF
)"
```

---

## Task 8: Weather fetch loop + on-demand refresh

**Files:**

- Modify: `crates/hytte-services/src/weather.rs`

Wire `geoclue::current()` to drive Open-Meteo fetches. 15-min periodic timer; `refresh()` sends an immediate fetch via an mpsc channel.

- [ ] **Step 1: Add the mpsc channel + fetch loop**

In `crates/hytte-services/src/weather.rs`, find the `WeatherHandles` struct and the `Service` impl. Update them:

Replace:

```rust
#[doc(hidden)]
pub struct WeatherHandles {
    pub(crate) state: Mutable<WeatherState>,
}

impl Default for WeatherHandles {
    fn default() -> Self {
        Self {
            state: Mutable::new(WeatherState::Loading),
        }
    }
}

/// Marker type for the weather service.
pub struct WeatherService;

impl Service for WeatherService {
    type Handles = WeatherHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        // Real fetch loop wired up in a follow-up. For now the service
        // just publishes the initial Loading state.
        WeatherHandles::default()
    }
}
```

with:

```rust
#[doc(hidden)]
pub struct WeatherHandles {
    pub(crate) state: Mutable<WeatherState>,
    pub(crate) refresh_tx: tokio::sync::mpsc::Sender<()>,
}

pub struct WeatherService;

impl Service for WeatherService {
    type Handles = WeatherHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let state = Mutable::new(WeatherState::Loading);
        let (refresh_tx, refresh_rx) = tokio::sync::mpsc::channel::<()>(8);
        let state_for_task = state.clone();
        rt.spawn(async move {
            run_fetch_loop(state_for_task, refresh_rx).await;
        });
        WeatherHandles { state, refresh_tx }
    }
}

#[must_use]
pub fn service() -> WeatherService {
    WeatherService
}
```

Then replace the old `refresh()` no-op stub with:

```rust
/// Force a fresh fetch. Coalesces — if a fetch is already queued, this is
/// a no-op. The sidebar's open-handler calls this; the fetch loop also
/// fires every 15 minutes on its own.
pub fn refresh() {
    registry::with(|r| {
        if let Some(h) = r.get::<WeatherHandles>() {
            // try_send: never block the caller; if the channel is full
            // (8 pending refreshes already queued), drop this one.
            let _ = h.refresh_tx.try_send(());
        }
    });
}

/// Synchronous snapshot of the `fetched_at` timestamp from the last
/// Resolved value. Returns None when the state is Loading or Error.
/// Used by the sidebar's open-handler to decide whether to force a refresh.
#[must_use]
pub fn last_fetched() -> Option<SystemTime> {
    registry::with(|r| {
        r.get::<WeatherHandles>().and_then(|h| match h.state.get_cloned() {
            WeatherState::Resolved(snap) => Some(snap.fetched_at),
            _ => None,
        })
    })
}
```

Then add the fetch loop itself. Append after the `pub fn refresh()` function:

```rust
use crate::geoclue;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);

async fn run_fetch_loop(
    state: Mutable<WeatherState>,
    mut refresh_rx: tokio::sync::mpsc::Receiver<()>,
) {
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    // First tick fires immediately — wanted (don't make the user wait
    // 15 minutes for the first paint).
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = refresh_rx.recv() => {}
        }

        // Drain any extra refresh signals so we don't double-fetch.
        while refresh_rx.try_recv().is_ok() {}

        fetch_once(&state).await;
    }
}

async fn fetch_once(state: &Mutable<WeatherState>) {
    // Don't flicker Loading mid-poll when we already have a Resolved value.
    let was_resolved = matches!(state.get_cloned(), WeatherState::Resolved(_));
    if !was_resolved {
        state.set(WeatherState::Loading);
    }

    // Resolve location synchronously via the sync accessor exposed by
    // geoclue.rs. If no snapshot is available, kick the geoclue service
    // and surface a config error to the user. The next geoclue resolution
    // will trigger our subscriber loop via a periodic tick if/when ready.
    let Some(loc) = geoclue::current_snapshot() else {
        geoclue::refresh();
        state.set(WeatherState::Error("set $TROLLSHELL_WEATHER_CITY".into()));
        return;
    };

    let (lat, lon) = (loc.lat, loc.lon);
    let label_hint = loc.label_hint.clone();
    let source = loc.source.clone();

    let fetch_result = tokio::task::spawn_blocking(move || fetch_current_blocking(lat, lon))
        .await;

    let current = match fetch_result {
        Ok(Ok(c)) => c,
        Ok(Err(msg)) => {
            state.set(WeatherState::Error(msg));
            return;
        }
        Err(e) => {
            state.set(WeatherState::Error(format!("fetch task panicked: {e}")));
            return;
        }
    };

    // Friendly location name. Configured source already has it; GeoClue
    // source needs a reverse-geocode lookup.
    let location_name = match (source, label_hint) {
        (geoclue::LocationSource::Configured, Some(name)) => name,
        _ => {
            match tokio::task::spawn_blocking(move || reverse_geocode_blocking(lat, lon)).await {
                Ok(Ok(n)) => n,
                Ok(Err(msg)) => {
                    tracing::warn!(error = %msg, "weather: reverse-geocode failed; falling back to lat/lon");
                    format!("{lat:.2}, {lon:.2}")
                }
                Err(e) => {
                    tracing::warn!(error = %e, "weather: reverse-geocode task panicked");
                    format!("{lat:.2}, {lon:.2}")
                }
            }
        }
    };

    state.set(WeatherState::Resolved(WeatherSnapshot {
        location: location_name,
        temp_c: current.temperature_2m,
        apparent_c: current.apparent_temperature,
        humidity_pct: current.relative_humidity_2m,
        wind_kmh: current.wind_speed_10m,
        condition: condition_for_code(current.weather_code),
        fetched_at: SystemTime::now(),
    }));
}
```

Note for the implementer: `geoclue::current().to_future().await` reads the _current_ Mutable value via the futures-signals first-emission semantic. If `signal_cloned()` doesn't expose `to_future()`, use:

```rust
use futures_util::StreamExt;
let mut once = geoclue::current().to_stream();
let loc = once.next().await.flatten();
```

(Adapt to whatever's available in the codebase; check how `dnd.rs` or `upower.rs` read current values from their own Mutables — they may just call `.get_cloned()` on the underlying Mutable rather than going through the signal.)

A simpler alternative if the above proves fiddly: expose a `pub(crate) fn current_snapshot() -> Option<LocationSnapshot>` in `geoclue.rs` that does the registry lookup + `.get_cloned()` synchronously, and use that here instead of subscribing to the signal.

- [ ] **Step 2: Build the crate**

```bash
cargo build -p hytte-services
```

Expected: builds clean. If `to_future` or `to_stream` aren't available on this signal type, follow the simpler alternative — add a sync `current_snapshot()` helper in `geoclue.rs` and use it.

- [ ] **Step 3: Run tests**

```bash
cargo test -p hytte-services
```

Expected: all weather + geoclue tests still pass.

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/weather.rs crates/hytte-services/src/geoclue.rs
git commit -m "$(cat <<'EOF'
feat(weather): 15-min fetch loop + refresh channel

WeatherHandles now owns a tokio mpsc Sender<()>. The Service's start()
spawns a fetch loop that races interval.tick() against the refresh
channel; either fires fetch_once, which resolves location via geoclue,
hits Open-Meteo, reverse-geocodes when needed, and commits the snapshot.
refresh() try_send()s into the channel — coalesces multiple rapid
calls so we don't double-fetch.

The fetch_once flow protects against flickering Loading mid-poll when
a Resolved value is already in place: only sets Loading on
initial/Error states. Errors map to WeatherState::Error with short
actionable strings.
EOF
)"
```

---

## Task 9: Widget renders WeatherState

**Files:**

- Create: `trollshell/src/widgets/weather.rs`
- Modify: `trollshell/src/widgets/mod.rs`

Build the widget with three pre-mounted state Boxes (Loading / Resolved / Error). Toggle their visibility based on the signal — no widget-tree rebuilds.

- [ ] **Step 1: Wire `pub mod weather;` into `widgets/mod.rs`**

Open `trollshell/src/widgets/mod.rs` and add `pub mod weather;` in alphabetical position (between `volume` and `vpn`, or wherever the alphabetical position lands).

- [ ] **Step 2: Create the widget file**

Create `trollshell/src/widgets/weather.rs`:

```rust
//! Sidebar weather widget. Subscribes to `services::weather::current()` and
//! shows one of three layouts: Loading (spinner), Resolved (location +
//! headline + details), or Error (warning icon + message).
//!
//! See `docs/superpowers/specs/2026-05-14-weather-widget-design.md`.

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::weather::{self, WeatherSnapshot, WeatherState};

pub fn widget(_monitor: &Monitor) -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.add_css_class("ts-weather");

    // Pre-built children for each state; visibility toggled on signal change.
    let loading = build_loading();
    let resolved = build_resolved_skeleton();
    let error_box = build_error_skeleton();

    column.append(&loading.root);
    column.append(&resolved.root);
    column.append(&error_box.root);

    // Initial visibility: Loading.
    loading.root.set_visible(true);
    resolved.root.set_visible(false);
    error_box.root.set_visible(false);

    let loading_clone = loading.root.clone();
    let resolved_clone = resolved.clone();
    let error_clone = error_box.clone();
    glib::MainContext::default().spawn_local(weather::current().for_each(move |state| {
        match state {
            WeatherState::Loading => {
                loading_clone.set_visible(true);
                resolved_clone.root.set_visible(false);
                error_clone.root.set_visible(false);
            }
            WeatherState::Resolved(snap) => {
                resolved_clone.apply(&snap);
                loading_clone.set_visible(false);
                resolved_clone.root.set_visible(true);
                error_clone.root.set_visible(false);
            }
            WeatherState::Error(msg) => {
                error_clone.message.set_text(&msg);
                loading_clone.set_visible(false);
                resolved_clone.root.set_visible(false);
                error_clone.root.set_visible(true);
            }
        }
        async {}
    }));

    column.upcast()
}

// ── State sub-widgets ────────────────────────────────────────────────────────

struct LoadingBox {
    root: gtk::Box,
}

fn build_loading() -> LoadingBox {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.set_halign(gtk::Align::Center);
    root.set_valign(gtk::Align::Center);
    root.set_vexpand(true);

    let spinner = gtk::Spinner::new();
    spinner.set_size_request(24, 24);
    spinner.start();

    let label = gtk::Label::new(Some("Loading weather…"));
    label.add_css_class("ts-weather-condition");

    root.append(&spinner);
    root.append(&label);

    LoadingBox { root }
}

#[derive(Clone)]
struct ResolvedBox {
    root: gtk::Box,
    location: gtk::Label,
    icon: gtk::Image,
    temp: gtk::Label,
    condition: gtk::Label,
    apparent_value: gtk::Label,
    wind_value: gtk::Label,
    humidity_value: gtk::Label,
}

fn build_resolved_skeleton() -> ResolvedBox {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let location = gtk::Label::new(None);
    location.set_halign(gtk::Align::Start);
    location.add_css_class("ts-weather-location");

    let headline = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    headline.add_css_class("ts-weather-headline");
    headline.set_halign(gtk::Align::Start);

    let icon = gtk::Image::from_icon_name("weather-clear-symbolic");
    icon.set_pixel_size(28);
    icon.add_css_class("ts-weather-icon");

    let temp = gtk::Label::new(None);
    temp.add_css_class("ts-weather-temp");

    headline.append(&icon);
    headline.append(&temp);

    let condition = gtk::Label::new(None);
    condition.set_halign(gtk::Align::Start);
    condition.add_css_class("ts-weather-condition");

    let details = gtk::Box::new(gtk::Orientation::Vertical, 4);
    details.add_css_class("ts-weather-details");

    let (apparent_row, apparent_value) = build_detail_row("Feels like");
    let (wind_row, wind_value) = build_detail_row("Wind");
    let (humidity_row, humidity_value) = build_detail_row("Humidity");
    details.append(&apparent_row);
    details.append(&wind_row);
    details.append(&humidity_row);

    root.append(&location);
    root.append(&headline);
    root.append(&condition);
    root.append(&details);

    ResolvedBox {
        root,
        location,
        icon,
        temp,
        condition,
        apparent_value,
        wind_value,
        humidity_value,
    }
}

fn build_detail_row(label_text: &str) -> (gtk::Box, gtk::Label) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.add_css_class("ts-weather-detail");

    let label = gtk::Label::new(Some(label_text));
    label.set_halign(gtk::Align::Start);
    label.set_hexpand(true);
    label.add_css_class("ts-weather-detail-label");

    let value = gtk::Label::new(None);
    value.set_halign(gtk::Align::End);
    value.add_css_class("ts-weather-detail-value");

    row.append(&label);
    row.append(&value);

    (row, value)
}

impl ResolvedBox {
    fn apply(&self, snap: &WeatherSnapshot) {
        self.location.set_text(&snap.location.to_uppercase());
        self.icon.set_icon_name(Some(snap.condition.icon));
        self.temp.set_text(&format!("{:.0}°", snap.temp_c));
        self.condition.set_text(snap.condition.label);
        self.apparent_value
            .set_text(&format!("{:.0}°", snap.apparent_c));
        self.wind_value
            .set_text(&format!("{:.0} km/h", snap.wind_kmh));
        self.humidity_value
            .set_text(&format!("{}%", snap.humidity_pct));
    }
}

#[derive(Clone)]
struct ErrorBox {
    root: gtk::Box,
    message: gtk::Label,
}

fn build_error_skeleton() -> ErrorBox {
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    root.set_halign(gtk::Align::Center);
    root.set_valign(gtk::Align::Center);
    root.set_vexpand(true);

    let icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
    icon.set_pixel_size(20);

    let message = gtk::Label::new(None);
    message.add_css_class("ts-weather-condition");

    root.append(&icon);
    root.append(&message);

    ErrorBox { root, message }
}
```

- [ ] **Step 3: Build the crate**

```bash
cargo build -p trollshell
```

Expected: builds clean. (Unused-warning on `widget` is acceptable — Task 10 mounts it.)

- [ ] **Step 4: Commit**

```bash
git add trollshell/src/widgets/weather.rs trollshell/src/widgets/mod.rs
git commit -m "$(cat <<'EOF'
feat(weather): sidebar widget with Loading/Resolved/Error layouts

Three pre-built sub-widgets (loading spinner, resolved column,
error message); visibility is toggled per WeatherState emission so
the widget tree never rebuilds — only labels and the icon update.
Location text is uppercased Rust-side because GTK CSS doesn't
support text-transform.
EOF
)"
```

---

## Task 10: Wire into sidebar + main.rs + CSS

**Files:**

- Modify: `trollshell/src/overlays/sidebar.rs`
- Modify: `trollshell/src/main.rs`
- Modify: `trollshell/style.css`

- [ ] **Step 1: Replace the sidebar's placeholder with the weather widget**

Open `trollshell/src/overlays/sidebar.rs`. Find the placeholder label construction (look for `gtk::Label::new(Some("sidebar"))` or `ts-sidebar-placeholder`). Replace the block that builds and appends the placeholder with:

```rust
    card.append(&crate::widgets::weather::widget(monitor));
```

Remove the local `placeholder` variable and the `placeholder.add_css_class("ts-sidebar-placeholder")` etc.

- [ ] **Step 2: Add the on-open refresh subscription**

Still in `sidebar.rs::install`, after the existing subscription that drives open/close on the mutable, add a second `spawn_local` that listens for sidebar-open events and triggers weather refresh when data is stale. Place it next to the other subscriptions so a future reader sees both:

```rust
    // When the sidebar opens, kick a fresh weather fetch if data is stale.
    // `weather::refresh()` coalesces — multiple rapid opens won't multi-fetch.
    glib::MainContext::default().spawn_local(open_state.signal().for_each(move |open| {
        if open && weather_is_stale() {
            hytte::services::weather::refresh();
        }
        async {}
    }));
```

Add this helper at the bottom of `sidebar.rs`:

```rust
fn weather_is_stale() -> bool {
    use std::time::{Duration, SystemTime};
    const FRESH_FOR: Duration = Duration::from_secs(5 * 60);

    let Some(last) = hytte::services::weather::last_fetched() else {
        // No Resolved value: either Loading (first paint not in yet) or
        // Error. In both cases the sidebar-open should force a refresh.
        return true;
    };
    SystemTime::now()
        .duration_since(last)
        .map_or(true, |age| age >= FRESH_FOR)
}
```

This uses the synchronous `weather::last_fetched()` accessor added in Task 8 — no async or `block_on`. Returns true (stale) when there's no Resolved value yet OR the snapshot is at least 5 minutes old.

- [ ] **Step 3: Register the services in main.rs**

Open `trollshell/src/main.rs`. Find the `.run(|app| { … })` builder chain near the top (where other services are registered via `.with(theme::service()).with(dnd::service())` etc.). Add both new services:

```rust
        .with(hytte::services::geoclue::service())
        .with(hytte::services::weather::service())
```

Order matters slightly: register `geoclue` first so its handles are available when `weather`'s start() runs and tries to subscribe.

- [ ] **Step 4: Add CSS**

Open `trollshell/style.css`. The current `.ts-sidebar-placeholder` rule is now unused — drop it. Add the weather rules right after the `.ts-sidebar` block:

Find:

```css
.ts-sidebar-placeholder {
  color: alpha(currentColor, 0.5);
  font-style: italic;
  font-size: 13px;
}
```

Replace it with the weather rules:

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
.ts-weather-temp {
  font-size: 32px;
  font-weight: 300;
}
.ts-weather-condition {
  color: alpha(currentColor, 0.85);
  margin-top: -4px;
  margin-bottom: 14px;
}
.ts-weather-detail-label {
  color: alpha(currentColor, 0.55);
}
.ts-weather-detail-value {
  color: white;
}
```

- [ ] **Step 5: Build the crate**

```bash
cargo build -p trollshell
```

Expected: builds clean.

- [ ] **Step 6: Run the full test suite**

```bash
cargo test -p hytte-services -p trollshell
```

Expected: all tests pass (10 weather + 3 geoclue + sidebar/frame + whatever else).

- [ ] **Step 7: Commit**

```bash
git add trollshell/src/overlays/sidebar.rs trollshell/src/main.rs trollshell/style.css
git commit -m "$(cat <<'EOF'
feat(weather): wire widget into sidebar, register services, add CSS

The sidebar's placeholder is replaced with weather::widget(monitor).
Both services (geoclue + weather) are registered in main.rs's .run()
builder chain; geoclue first so its handles are live when weather's
start() runs. CSS rules drop the now-unused .ts-sidebar-placeholder
and add the .ts-weather* hierarchy. The sidebar's on-open handler
calls weather::refresh() when data is stale (>5 min, or Loading, or
Error).
EOF
)"
```

---

## Task 11: End-to-end manual verification

**Why:** the D-Bus geoclue path, the 15-min poll timer, and the on-sidebar-open refresh are all interactive — they need a running trollshell on niri.

- [ ] **Step 1: Build a release binary**

```bash
cargo build -p trollshell --release
```

Expected: clean build.

- [ ] **Step 2: Restart trollshell with the configured city set**

```bash
# Match your actual city. Stockholm is the spec's example.
export TROLLSHELL_WEATHER_CITY=Stockholm
pkill -x trollshell; sleep 1
./target/release/trollshell &> /tmp/trollshell.log & disown
```

- [ ] **Step 3: Open the sidebar; verify Resolved state**

Click the leftmost bar chip. Sidebar slides out. Within ~10 seconds the widget should transition from the Loading spinner to the Resolved layout:

- Location label at top, uppercased (e.g. "STOCKHOLM")
- Big temperature reading (e.g. "18°")
- Condition label below ("Clear", "Cloudy", etc.)
- Three detail rows: "Feels like / Wind / Humidity" with values

Confirm: location matches your env var. Temperature looks plausible for the area. No flickers.

- [ ] **Step 4: Close + reopen with stale data → re-fetch**

Wait at least 6 minutes after step 3 (so the snapshot is "stale"). Close the sidebar. Reopen. The widget should briefly flicker through Loading (only if the previous state was Error) or just update its values atomically (if Resolved is current). `tail -f /tmp/trollshell.log` should show a fresh fetch in this window.

- [ ] **Step 5: Restart with geoclue disabled, env var still set**

```bash
sudo systemctl stop geoclue 2>/dev/null || true
export TROLLSHELL_WEATHER_CITY=Stockholm
pkill -x trollshell; sleep 1
./target/release/trollshell &> /tmp/trollshell.log & disown
```

Within ~10 seconds (geoclue timeout + env var fallback), widget should reach Resolved using the env var path. Location label should match the env var.

- [ ] **Step 6: Restart with no env var and no geoclue → Error state**

```bash
sudo systemctl stop geoclue 2>/dev/null || true
unset TROLLSHELL_WEATHER_CITY
pkill -x trollshell; sleep 1
./target/release/trollshell &> /tmp/trollshell.log & disown
```

Open sidebar. After ~10 seconds, widget should show the error layout: warning icon + "set $TROLLSHELL_WEATHER_CITY" message.

- [ ] **Step 7: Re-enable geoclue, restart without env var → geoclue path**

```bash
sudo systemctl start geoclue 2>/dev/null || true
unset TROLLSHELL_WEATHER_CITY
pkill -x trollshell; sleep 1
./target/release/trollshell &> /tmp/trollshell.log & disown
```

Open sidebar. Widget should reach Resolved via geoclue. The location label comes from reverse-geocoding lat/lon — it should match your actual locality.

If geoclue refuses to authorize trollshell (no .desktop file with `X-Geoclue-Reason`), the widget will show the error message. This is acceptable for MVP — the env var path covers it.

- [ ] **Step 8: Offline → Error → recover**

Disconnect Wi-Fi. Wait up to 15 minutes for the next poll (or close + reopen the sidebar to force one). Widget should show "network error" or similar.

Reconnect. Force a refresh by closing + reopening the sidebar. Widget should recover to Resolved on the next fetch.

- [ ] **Step 9: Visual regression check**

The sidebar's surface, the frame's cutout, and the bar should all look identical to the pre-weather MVP. The only visible difference is the widget's content inside the sidebar card. No layout regressions.

---

## Done

After Task 11 passes, the weather widget MVP is complete. Phase-3 candidates:

- Settings panel entry: switch units, change refresh rate, override location label
- Forecast (hourly + daily) — collapsible section below the current weather block
- An active-state CSS rule on `.ts-sidebar-toggle.active` so the chip lights up while sidebar is open (`bind_class(sidebar::open_signal(monitor), &btn, "active")`)
- Persist last successful snapshot to `~/.config/trollshell/weather-cache.json` for instant first paint after restart
