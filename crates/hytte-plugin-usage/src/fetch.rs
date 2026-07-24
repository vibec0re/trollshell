//! The usage I/O worker: the Grafana public-dashboard client, the defensive
//! response parsing, and the visibility-gated poll task.
//!
//! # The gate
//!
//! [`poll_task`] mirrors the departures board's reference gate (#288): it owns
//! the fetch interval and drains the command lane fed by
//! [`crate::UsageCmd::SetVisible`]. While the sidebar is hidden it **parks** —
//! no ticks, no HTTP; on a hidden→visible edge it fires an immediate refresh,
//! then re-polls every [`POLL_INTERVAL`] until hidden again. HTTP is blocking
//! `ureq` on a `spawn_blocking` thread — the house idiom.
//!
//! # The read path (defensive by design)
//!
//! Grafana public dashboards expose anonymous routes: `GET
//! /api/public/dashboards/{token}` (the dashboard model, for panel discovery)
//! and `POST …/panels/{panelId}/query` (the panel's dataframe). The exact
//! panel/series shape of the exponentials dashboard is **unknown until the live
//! URL exists**, so nothing here is typed against it: [`discover_panel_id`] and
//! [`parse_query_value`] walk `serde_json::Value` generically — pick the first
//! value panel (or the configured [`crate::config::Config::panel`]), take the
//! first numeric series' last value — and **log, never crash**, on a surprising
//! shape (returning `None` → a fetch error the reducer folds while keeping the
//! last good reading).

use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::Config;
use crate::{UsageCmd, UsageMsg};

/// The visibility-gated poll cadence. Deliberately slow (the dashboard is a
/// spend gauge, not a live meter) and parked entirely while the sidebar is
/// closed. `tokio::time::interval` fires its first tick immediately.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_mins(1);

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_LIMIT: u64 = 4 * 1024 * 1024;

/// Grafana panel `type`s that carry a value we can read, preferred during
/// discovery over an arbitrary first panel (a `row` is a layout container, not
/// a query target, so it is never selected).
const VALUE_PANEL_TYPES: &[&str] = &[
    "stat",
    "gauge",
    "bargauge",
    "timeseries",
    "graph",
    "barchart",
    "histogram",
];

// ── Endpoints ────────────────────────────────────────────────────────────────

/// The Grafana public-dashboard API endpoints derived from the configured URL.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Endpoints {
    base: String,
    token: String,
}

impl Endpoints {
    /// `GET` — the dashboard model (panel discovery).
    fn meta_url(&self) -> String {
        format!("{}/api/public/dashboards/{}", self.base, self.token)
    }

    /// `POST` — one panel's query (the dataframe).
    fn panel_query_url(&self, panel_id: &str) -> String {
        format!(
            "{}/api/public/dashboards/{}/panels/{}/query",
            self.base, self.token, panel_id
        )
    }
}

/// Derive the API base (origin + any path prefix) and access token from a
/// configured public-dashboard URL. Accepts either the browser link
/// (`…/public-dashboards/<token>`) or the API form
/// (`…/api/public/dashboards/<token>`), with or without a trailing slash or a
/// query/fragment. `None` if no token segment is present.
pub(crate) fn derive_endpoints(url: &str) -> Option<Endpoints> {
    let trimmed = url.trim().trim_end_matches('/');
    let path = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    // API form first so its longer `/public/dashboards/` doesn't get split by
    // the shorter browser marker.
    for marker in ["/api/public/dashboards/", "/public-dashboards/"] {
        if let Some(idx) = path.find(marker) {
            let base = path[..idx].trim_end_matches('/');
            let token = path[idx + marker.len()..]
                .split('/')
                .next()
                .unwrap_or("")
                .trim();
            if !base.is_empty() && !token.is_empty() {
                return Some(Endpoints {
                    base: base.to_owned(),
                    token: token.to_owned(),
                });
            }
        }
    }
    None
}

// ── Panel discovery ──────────────────────────────────────────────────────────

/// Pick a panel id to query from the dashboard model. Prefers the first
/// value-bearing panel (see [`VALUE_PANEL_TYPES`]) and falls back to the first
/// non-`row` panel; recurses into `row` panels' nested `panels`. `None` if the
/// model carries no queryable panel. Pure over the JSON body, so it's testable.
pub(crate) fn discover_panel_id(meta_body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(meta_body).ok()?;
    let panels = v.get("dashboard")?.get("panels")?.as_array()?;
    let mut leaves = Vec::new();
    collect_leaf_panels(panels, &mut leaves);

    let mut first_any: Option<i64> = None;
    let mut first_value: Option<i64> = None;
    for p in leaves {
        let Some(id) = p.get("id").and_then(Value::as_i64) else {
            continue;
        };
        first_any.get_or_insert(id);
        let ty = p.get("type").and_then(Value::as_str).unwrap_or_default();
        if VALUE_PANEL_TYPES.contains(&ty) {
            first_value.get_or_insert(id);
        }
    }
    first_value.or(first_any).map(|id| id.to_string())
}

/// Flatten the panel tree to its non-`row` leaves in document order, recursing
/// into each `row`'s nested `panels`.
fn collect_leaf_panels<'a>(panels: &'a [Value], out: &mut Vec<&'a Value>) {
    for p in panels {
        let ty = p.get("type").and_then(Value::as_str).unwrap_or_default();
        if ty == "row" {
            if let Some(nested) = p.get("panels").and_then(Value::as_array) {
                collect_leaf_panels(nested, out);
            }
        } else {
            out.push(p);
        }
    }
}

// ── Query parsing ────────────────────────────────────────────────────────────

/// Extract the latest numeric value from a Grafana panel-query response. Walks
/// the `results` map → each result's `frames` → the first frame that yields a
/// numeric column, and returns that column's last finite value. Prefers the
/// schema-declared `number` field; falls back to the last column when no schema
/// is present. `None` (never a panic) on any surprising shape.
pub(crate) fn parse_query_value(body: &str) -> Option<f64> {
    let v: Value = serde_json::from_str(body).ok()?;
    let results = v.get("results")?.as_object()?;
    for result in results.values() {
        let Some(frames) = result.get("frames").and_then(Value::as_array) else {
            continue;
        };
        for frame in frames {
            if let Some(value) = frame_last_numeric(frame) {
                return Some(value);
            }
        }
    }
    None
}

/// The last finite value of a frame's numeric column. Uses `schema.fields` to
/// find the first `number`-typed column; without a schema, takes the last
/// column (Grafana puts time first, the value last).
fn frame_last_numeric(frame: &Value) -> Option<f64> {
    let columns = frame.get("data")?.get("values")?.as_array()?;
    if columns.is_empty() {
        return None;
    }
    let idx = frame
        .get("schema")
        .and_then(|s| s.get("fields"))
        .and_then(Value::as_array)
        .and_then(|fields| {
            fields
                .iter()
                .position(|f| f.get("type").and_then(Value::as_str) == Some("number"))
        })
        .unwrap_or(columns.len() - 1);
    let column = columns.get(idx)?.as_array()?;
    column
        .iter()
        .rev()
        .find_map(|x| x.as_f64().filter(|f| f.is_finite()))
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
        .timeout_global(Some(HTTP_READ_TIMEOUT))
        .build();
    config.into()
}

fn get_body(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let mut resp = agent.get(url).call().map_err(|e| format!("http: {e}"))?;
    let status = resp.status();
    let text = resp
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map_err(|e| format!("body: {e}"))?;
    body_or_status_err(status.is_success(), status.as_u16(), text)
}

fn post_query(agent: &ureq::Agent, url: &str, window: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "intervalMs": 60_000,
        "maxDataPoints": 300,
        "timeRange": { "from": window, "to": "now" },
    });
    let mut resp = agent
        .post(url)
        .send_json(&body)
        .map_err(|e| format!("http: {e}"))?;
    let status = resp.status();
    let text = resp
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map_err(|e| format!("body: {e}"))?;
    body_or_status_err(status.is_success(), status.as_u16(), text)
}

/// Return the body on a 2xx, or a trimmed `http <code>: …` error otherwise.
/// Primitive args only — no ureq types to name — so the two callers share the
/// status handling without pinning a response type.
fn body_or_status_err(success: bool, code: u16, text: String) -> Result<String, String> {
    if success {
        Ok(text)
    } else {
        let detail: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let detail: String = detail.chars().take(160).collect();
        Err(format!("http {code}: {detail}"))
    }
}

/// One blocking fetch: resolve the panel (config-pinned or discovered), query
/// it, and pull out the latest numeric value (the spend within the window).
fn fetch_spend(cfg: &Config) -> Result<f64, String> {
    let endpoints = derive_endpoints(&cfg.dashboard_url).ok_or_else(|| {
        format!(
            "not a Grafana public-dashboard URL: {} (expected …/public-dashboards/<token>)",
            cfg.dashboard_url
        )
    })?;
    let agent = http_agent();

    let panel_id = if let Some(id) = &cfg.panel {
        id.clone()
    } else {
        let meta = get_body(&agent, &endpoints.meta_url())?;
        discover_panel_id(&meta).ok_or_else(|| {
            "no queryable panel in the dashboard; set TROLLSHELL_USAGE_PANEL".to_owned()
        })?
    };

    let body = post_query(&agent, &endpoints.panel_query_url(&panel_id), &cfg.window)?;
    parse_query_value(&body).ok_or_else(|| {
        format!("no numeric series in panel {panel_id}'s response (set TROLLSHELL_USAGE_PANEL?)")
    })
}

// ── The visibility-gated poll task ───────────────────────────────────────────

/// The visibility-gate transition: given the current visible state and a
/// requested one, return the next state and whether an **immediate** refresh is
/// owed (a hidden→visible edge). Pure, so the gate is unit-testable.
fn on_visibility(current: bool, requested: bool) -> (bool, bool) {
    (requested, requested && !current)
}

/// Run one blocking fetch and forward the outcome to the reducer.
async fn fetch_and_send(cfg: &Config, msg_tx: &UnboundedSender<UsageMsg>) {
    let cfg = cfg.clone();
    let msg = match tokio::task::spawn_blocking(move || fetch_spend(&cfg)).await {
        Ok(Ok(spend)) => UsageMsg::Reading {
            spend,
            updated: chrono::Local::now().format("%H:%M").to_string(),
        },
        Ok(Err(e)) => {
            eprintln!("[usage] fetch failed: {e}");
            UsageMsg::FetchError(e)
        }
        Err(join) => {
            eprintln!("[usage] fetch task failed: {join}");
            UsageMsg::FetchError(format!("join: {join}"))
        }
    };
    let _ = msg_tx.send(msg);
}

/// The I/O side of the visibility gate (the `sources()` task). Parks while the
/// sidebar is hidden, refreshes immediately when it opens, then re-polls every
/// [`POLL_INTERVAL`] until it closes. Exits when the command lane closes (the
/// session is tearing down).
pub(crate) async fn poll_task(
    cfg: Config,
    mut cmds: crate::CmdReceiver<UsageCmd>,
    msg_tx: UnboundedSender<UsageMsg>,
) {
    let mut visible = false;
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            // Prefer visibility changes so a close parks the poller promptly.
            biased;
            cmd = cmds.recv() => {
                let Some(UsageCmd::SetVisible(requested)) = cmd else {
                    return; // lane closed → session teardown
                };
                let (next_visible, refresh_now) = on_visibility(visible, requested);
                visible = next_visible;
                if refresh_now {
                    interval.reset();
                    fetch_and_send(&cfg, &msg_tx).await;
                }
            }
            // Disabled while hidden — the poller parks (no ticks, no HTTP).
            _ = interval.tick(), if visible => {
                fetch_and_send(&cfg, &msg_tx).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Endpoints, derive_endpoints, discover_panel_id, on_visibility, parse_query_value};

    const QUERY_FIXTURE: &str = include_str!("../tests/fixtures/grafana-query.json");
    const META_FIXTURE: &str = include_str!("../tests/fixtures/grafana-dashboard.json");

    // ── Endpoint derivation ───────────────────────────────────────────────────

    #[test]
    fn derive_endpoints_from_browser_link() {
        let e = derive_endpoints("https://exponentials.vibec0re.mov/public-dashboards/abc123")
            .expect("derives");
        assert_eq!(
            e,
            Endpoints {
                base: "https://exponentials.vibec0re.mov".to_owned(),
                token: "abc123".to_owned(),
            }
        );
        assert_eq!(
            e.meta_url(),
            "https://exponentials.vibec0re.mov/api/public/dashboards/abc123"
        );
        assert_eq!(
            e.panel_query_url("7"),
            "https://exponentials.vibec0re.mov/api/public/dashboards/abc123/panels/7/query"
        );
    }

    #[test]
    fn derive_endpoints_from_api_form_and_with_noise() {
        // API form, trailing slash, query string, and a path prefix all handled.
        let e = derive_endpoints(
            "https://host.example/grafana/api/public/dashboards/tok/?orgId=1#panel",
        )
        .expect("derives");
        assert_eq!(e.base, "https://host.example/grafana");
        assert_eq!(e.token, "tok");
    }

    #[test]
    fn derive_endpoints_rejects_a_non_dashboard_url() {
        assert!(derive_endpoints("https://host.example/some/other/path").is_none());
        assert!(derive_endpoints("").is_none());
        // Marker present but no token segment.
        assert!(derive_endpoints("https://host.example/public-dashboards/").is_none());
    }

    // ── Panel discovery ───────────────────────────────────────────────────────

    #[test]
    fn discover_panel_prefers_value_panels_and_recurses_rows() {
        // The fixture nests a `stat` (id 4) inside a `row` (id 1), then a
        // `timeseries` (id 2). The first value panel in document order is id 4.
        assert_eq!(discover_panel_id(META_FIXTURE).as_deref(), Some("4"));
    }

    #[test]
    fn discover_panel_falls_back_to_first_non_row() {
        let meta = r#"{"dashboard":{"panels":[
            {"id":10,"type":"row","panels":[{"id":11,"type":"text"}]},
            {"id":12,"type":"news"}
        ]}}"#;
        // No value-typed panel → first non-row leaf (id 11) wins.
        assert_eq!(discover_panel_id(meta).as_deref(), Some("11"));
    }

    #[test]
    fn discover_panel_none_when_no_panels() {
        assert_eq!(discover_panel_id(r#"{"dashboard":{"panels":[]}}"#), None);
        assert_eq!(discover_panel_id("not json"), None);
        assert_eq!(discover_panel_id("{}"), None);
    }

    // ── Query parsing ─────────────────────────────────────────────────────────

    #[test]
    fn parse_query_takes_last_value_of_the_numeric_series() {
        // The fixture's value column is [3.42, 7.15, 12.87]; the latest is 12.87.
        let v = parse_query_value(QUERY_FIXTURE).expect("a numeric value");
        assert!((v - 12.87).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn parse_query_uses_schema_to_skip_the_time_column() {
        // Value column ([1,2,3]) declared second; a naive last-column pick would
        // still land it, but here the numeric field is FIRST and time is second,
        // so schema guidance is what keeps us off the (huge) timestamp column.
        let body = r#"{"results":{"A":{"frames":[{
            "schema":{"fields":[
                {"name":"Value","type":"number"},
                {"name":"Time","type":"time"}
            ]},
            "data":{"values":[[1.0,2.0,3.0],[1700000000000,1700000060000,1700000120000]]}
        }]}}}"#;
        assert!((parse_query_value(body).unwrap() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn parse_query_skips_trailing_nulls() {
        let body = r#"{"results":{"A":{"frames":[{
            "schema":{"fields":[{"name":"t","type":"time"},{"name":"v","type":"number"}]},
            "data":{"values":[[1,2,3],[4.0,5.0,null]]}
        }]}}}"#;
        assert!((parse_query_value(body).unwrap() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn parse_query_no_schema_takes_last_column() {
        let body = r#"{"results":{"A":{"frames":[{
            "data":{"values":[[1700000000000],[42.5]]}
        }]}}}"#;
        assert!((parse_query_value(body).unwrap() - 42.5).abs() < 1e-9);
    }

    #[test]
    fn parse_query_surprises_are_none_not_panics() {
        assert_eq!(parse_query_value("not json"), None);
        assert_eq!(parse_query_value("{}"), None);
        assert_eq!(parse_query_value(r#"{"results":{}}"#), None);
        assert_eq!(
            parse_query_value(r#"{"results":{"A":{"frames":[]}}}"#),
            None
        );
        // A frame with an all-null value column yields nothing.
        assert_eq!(
            parse_query_value(
                r#"{"results":{"A":{"frames":[{"data":{"values":[[null,null]]}}]}}}"#
            ),
            None
        );
    }

    // ── The visibility gate ───────────────────────────────────────────────────

    #[test]
    fn on_visibility_refreshes_only_on_the_hidden_to_visible_edge() {
        assert_eq!(on_visibility(false, true), (true, true));
        assert_eq!(on_visibility(true, true), (true, false));
        assert_eq!(on_visibility(true, false), (false, false));
        assert_eq!(on_visibility(false, false), (false, false));
    }
}
