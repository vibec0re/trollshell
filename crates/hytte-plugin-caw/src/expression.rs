//! Reading caw's published *expression* — the small JSON her `caw_express` tool
//! (in `opencaw`) writes whenever she wants to emote. This plugin is a thin,
//! read-only client: it polls the file and renders whatever she last published.
//!
//! Privacy note: this is the ONLY thing the plugin reads. It never touches caw's
//! board or DMs — she publishes exactly what she chooses to express here.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

/// One published expression — mirrors what `opencaw`'s `caw_express` writes.
/// Every field defaults, so a partial/older file still parses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Expression {
    #[serde(default)]
    pub mood: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub chaos_level: f64,
    #[serde(default)]
    pub ts: u64,
}

/// The file caw writes and we read. `CAW_EXPRESSION_PATH` overrides; default
/// `$XDG_STATE_HOME/caw/expression.json` (→ `~/.local/state/caw/expression.json`)
/// — the same default `opencaw`'s `caw_expression_path()` uses, so the two ends
/// agree with no configuration.
pub fn expression_path() -> PathBuf {
    if let Ok(p) = std::env::var("CAW_EXPRESSION_PATH")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local").join("state"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("caw").join("expression.json")
}

/// Read + parse the current expression, or `None` if the file is missing or
/// unparseable (e.g. caught mid-write — the caller just keeps its last good one).
pub fn read(path: &Path) -> Option<Expression> {
    let raw = std::fs::read(path).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Seconds since `ts` was published (0 for a future/zero timestamp).
#[must_use]
pub fn staleness_secs(ts: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    now.saturating_sub(ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_expression() {
        let e: Expression = serde_json::from_str(
            r#"{"mood":"gremlin","action":"*ruffles feathers*","message":"hej","chaos_level":0.7,"ts":42}"#,
        )
        .unwrap();
        assert_eq!(e.mood, "gremlin");
        assert_eq!(e.message, "hej");
        assert!((e.chaos_level - 0.7).abs() < 1e-9);
        assert_eq!(e.ts, 42);
    }

    #[test]
    fn tolerates_a_partial_file() {
        let e: Expression = serde_json::from_str(r#"{"mood":"smug"}"#).unwrap();
        assert_eq!(e.mood, "smug");
        assert_eq!(e.message, "");
        assert!(e.chaos_level.abs() < 1e-9);
    }

    #[test]
    fn read_missing_file_is_none() {
        assert!(read(std::path::Path::new("/nonexistent/caw/expression.json")).is_none());
    }
}
