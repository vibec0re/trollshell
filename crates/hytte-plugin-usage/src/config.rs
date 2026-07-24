//! Configuration: the dashboard URL, the budget denominator, and the query
//! window — resolved from the environment first, then
//! `~/.config/trollshell/usage.toml`.
//!
//! The **URL is configuration, not a build input** (the #320 unblock): the
//! plugin ships without one and, until it is set, renders a calm empty-state
//! card and makes **no** network calls — the same keyless short-circuit the
//! pet's brain uses (#438/#472) so an unconfigured plugin never spams errors.
//!
//! Precedence per field is env → file → default. Reading env vars and one small
//! TOML file is cheap and synchronous, so both [`crate::Usage::init`] (for the
//! seed render's mode) and the [`crate::fetch`] worker call [`load`]; env is
//! stable across a session, so they agree.

use std::path::PathBuf;

/// Env var: the Grafana `…/public-dashboards/<token>` URL (browser or API
/// form). The single value that flips the plugin from empty-state to live.
const ENV_URL: &str = "TROLLSHELL_USAGE_DASHBOARD_URL";
/// Env var: the budget denominator for the "burned ÷ budget" gauge. Optional —
/// without it the card shows the raw spend figure and no gauge.
const ENV_BUDGET: &str = "TROLLSHELL_USAGE_BUDGET";
/// Env var: pin the dashboard panel to query by its numeric id (skips panel
/// discovery). Optional.
const ENV_PANEL: &str = "TROLLSHELL_USAGE_PANEL";
/// Env var: the Grafana time-range `from` for the window (e.g. `now-5h`,
/// `now-24h`, `now/d`). The `to` is always `now`.
const ENV_WINDOW: &str = "TROLLSHELL_USAGE_WINDOW";

/// The config file, relative to `$HOME`.
const CONFIG_REL_PATH: &str = ".config/trollshell/usage.toml";

/// The default query window when none is configured: the rolling 5-hour window,
/// matching the cadence Claude Code's own `/usage` reports against.
pub(crate) const DEFAULT_WINDOW: &str = "now-5h";

/// A resolved, ready-to-poll configuration.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Config {
    /// The public-dashboard URL (never blank — a blank one reads as unset).
    pub(crate) dashboard_url: String,
    /// The budget denominator for the gauge, or `None` for spend-only.
    pub(crate) budget: Option<f64>,
    /// A pinned panel id, or `None` to discover the first value panel.
    pub(crate) panel: Option<String>,
    /// The Grafana time-range `from` (the window the spend is summed over).
    pub(crate) window: String,
}

/// Whether the plugin has a dashboard to poll.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ConfigState {
    /// No dashboard URL from env or file — empty-state, no network.
    Unconfigured,
    /// A live dashboard to poll.
    Configured(Config),
}

/// The raw values pulled from the TOML file (all optional), before the env
/// overlay. `Default` = an absent/empty file.
#[derive(Default)]
struct FileValues {
    dashboard_url: Option<String>,
    budget: Option<f64>,
    panel: Option<String>,
    window: Option<String>,
}

/// Resolve the configuration: env overlaid on the file, with the URL deciding
/// [`ConfigState`]. Never panics; a malformed/missing file degrades to "no file
/// values" and the env still wins.
pub(crate) fn load() -> ConfigState {
    let file = load_file().unwrap_or_default();
    resolve(&env_values(), file)
}

/// The env half of the overlay, gathered up front so [`resolve`] is a pure
/// function of `(env, file)` and unit-testable.
#[derive(Default)]
pub(crate) struct EnvValues {
    dashboard_url: Option<String>,
    budget: Option<f64>,
    panel: Option<String>,
    window: Option<String>,
}

fn env_values() -> EnvValues {
    EnvValues {
        dashboard_url: env_nonblank(ENV_URL),
        budget: env_nonblank(ENV_BUDGET).and_then(|s| s.trim().parse::<f64>().ok()),
        panel: env_nonblank(ENV_PANEL),
        window: env_nonblank(ENV_WINDOW),
    }
}

/// Overlay env over file and decide the state. Pure (no I/O) so the precedence
/// is testable without touching the real environment or disk.
fn resolve(env: &EnvValues, file: FileValues) -> ConfigState {
    let Some(dashboard_url) = env
        .dashboard_url
        .clone()
        .or(file.dashboard_url)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    else {
        return ConfigState::Unconfigured;
    };
    ConfigState::Configured(Config {
        dashboard_url,
        budget: env.budget.or(file.budget).filter(|b| b.is_finite()),
        panel: env
            .panel
            .clone()
            .or(file.panel)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
        window: env
            .window
            .clone()
            .or(file.window)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_WINDOW.to_owned()),
    })
}

fn env_nonblank(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

fn config_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(CONFIG_REL_PATH))
}

/// Read + parse the TOML file, or `None` if there's no file (or it won't
/// parse — logged, never fatal).
fn load_file() -> Option<FileValues> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    match parse_file(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("[usage] ignoring {}: {e}", path.display());
            None
        }
    }
}

/// Parse the config file body leniently: unknown keys are ignored, `budget`
/// accepts an integer or float, and `panel` accepts an integer or string (so
/// `panel = 2` and `panel = "2"` both work). Pure, so the schema is testable.
fn parse_file(toml_text: &str) -> Result<FileValues, String> {
    let table: toml::Table = toml::from_str(toml_text).map_err(|e| e.to_string())?;
    Ok(FileValues {
        dashboard_url: table.get("dashboard_url").and_then(value_as_string),
        budget: table.get("budget").and_then(value_as_f64),
        panel: table.get("panel").and_then(value_as_string),
        window: table.get("window").and_then(value_as_string),
    })
}

/// A TOML string, or an integer stringified (so a bare `panel = 2` is accepted).
fn value_as_string(v: &toml::Value) -> Option<String> {
    v.as_str()
        .map(str::to_owned)
        .or_else(|| v.as_integer().map(|i| i.to_string()))
        .filter(|s| !s.trim().is_empty())
}

/// A TOML float, or an integer widened to one (so `budget = 30` is accepted).
#[allow(clippy::cast_precision_loss)]
fn value_as_f64(v: &toml::Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
}

/// Turn a Grafana time-range `from` into a short human label for the card
/// ("last 5h", "today", …). Falls back to the raw range so an exotic value is
/// still shown honestly rather than hidden.
pub(crate) fn humanize_window(window: &str) -> String {
    let w = window.trim();
    match w {
        "now/d" => "today".to_owned(),
        "now/w" => "this week".to_owned(),
        "now/M" => "this month".to_owned(),
        _ => match w.strip_prefix("now-") {
            Some(span) if !span.is_empty() => format!("last {span}"),
            _ => w.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Config, ConfigState, EnvValues, FileValues, humanize_window, parse_file, resolve,
        value_as_f64,
    };

    fn env(url: Option<&str>) -> EnvValues {
        EnvValues {
            dashboard_url: url.map(str::to_owned),
            ..EnvValues::default()
        }
    }

    #[test]
    fn no_url_anywhere_is_unconfigured() {
        assert_eq!(
            resolve(&EnvValues::default(), FileValues::default()),
            ConfigState::Unconfigured,
        );
        // A blank/whitespace URL reads as unset, not as a configured empty URL.
        assert_eq!(
            resolve(&env(Some("   ")), FileValues::default()),
            ConfigState::Unconfigured,
        );
    }

    #[test]
    fn env_url_alone_configures_with_defaults() {
        let ConfigState::Configured(cfg) = resolve(
            &env(Some("https://x/public-dashboards/tok")),
            FileValues::default(),
        ) else {
            panic!("configured");
        };
        assert_eq!(cfg.dashboard_url, "https://x/public-dashboards/tok");
        assert_eq!(cfg.budget, None, "no budget → spend-only");
        assert_eq!(cfg.panel, None, "no pinned panel → discover");
        assert_eq!(cfg.window, super::DEFAULT_WINDOW);
    }

    #[test]
    fn env_overrides_file_per_field() {
        let file = FileValues {
            dashboard_url: Some("https://file/public-dashboards/f".to_owned()),
            budget: Some(10.0),
            panel: Some("9".to_owned()),
            window: Some("now-1h".to_owned()),
        };
        let env = EnvValues {
            dashboard_url: Some("https://env/public-dashboards/e".to_owned()),
            budget: Some(30.0),
            panel: None,
            window: None,
        };
        let ConfigState::Configured(cfg) = resolve(&env, file) else {
            panic!("configured");
        };
        assert_eq!(
            cfg.dashboard_url, "https://env/public-dashboards/e",
            "env URL wins"
        );
        assert!((cfg.budget.unwrap() - 30.0).abs() < 1e-9, "env budget wins");
        assert_eq!(cfg.panel.as_deref(), Some("9"), "file panel fills the gap");
        assert_eq!(cfg.window, "now-1h", "file window fills the gap");
    }

    #[test]
    fn file_only_configuration() {
        let file = FileValues {
            dashboard_url: Some("https://file/public-dashboards/f".to_owned()),
            budget: Some(42.5),
            ..FileValues::default()
        };
        let ConfigState::Configured(cfg) = resolve(&EnvValues::default(), file) else {
            panic!("configured");
        };
        assert_eq!(cfg.dashboard_url, "https://file/public-dashboards/f");
        assert!((cfg.budget.unwrap() - 42.5).abs() < 1e-9);
    }

    #[test]
    fn parse_file_is_lenient_over_types_and_unknown_keys() {
        // Integer budget + integer panel + an unknown key the resolver ignores.
        let toml = "\
            dashboard_url = \"https://x/public-dashboards/tok\"\n\
            budget = 30\n\
            panel = 2\n\
            window = \"now-24h\"\n\
            future_key = \"ignored\"\n";
        let v = parse_file(toml).expect("parses");
        assert_eq!(
            v.dashboard_url.as_deref(),
            Some("https://x/public-dashboards/tok")
        );
        assert!(
            (v.budget.unwrap() - 30.0).abs() < 1e-9,
            "integer budget widens to f64"
        );
        assert_eq!(v.panel.as_deref(), Some("2"), "integer panel stringifies");
        assert_eq!(v.window.as_deref(), Some("now-24h"));
    }

    #[test]
    fn parse_file_float_budget_and_string_panel() {
        let v = parse_file("budget = 12.75\npanel = \"5\"\n").expect("parses");
        assert!((v.budget.unwrap() - 12.75).abs() < 1e-9);
        assert_eq!(v.panel.as_deref(), Some("5"));
    }

    #[test]
    fn parse_file_empty_and_malformed() {
        assert_eq!(parse_file("").unwrap().dashboard_url, None);
        assert!(parse_file("not = = toml").is_err());
    }

    #[test]
    fn value_as_f64_accepts_int_and_float_but_not_string() {
        assert_eq!(value_as_f64(&toml::Value::Integer(7)), Some(7.0));
        assert_eq!(value_as_f64(&toml::Value::Float(1.5)), Some(1.5));
        assert_eq!(value_as_f64(&toml::Value::String("x".to_owned())), None);
    }

    #[test]
    fn humanize_window_maps_the_common_ranges() {
        assert_eq!(humanize_window("now-5h"), "last 5h");
        assert_eq!(humanize_window("now-24h"), "last 24h");
        assert_eq!(humanize_window("now-7d"), "last 7d");
        assert_eq!(humanize_window("now/d"), "today");
        assert_eq!(humanize_window("now/w"), "this week");
        // Unknown forms pass through verbatim (honest, not hidden).
        assert_eq!(humanize_window("2026-01-01"), "2026-01-01");
    }

    #[test]
    fn config_is_cloneable_debug() {
        // A trivial guard that the public shape stays usable by the model.
        let cfg = Config {
            dashboard_url: "u".to_owned(),
            budget: Some(1.0),
            panel: None,
            window: "now-5h".to_owned(),
        };
        assert_eq!(cfg.clone(), cfg);
    }
}
