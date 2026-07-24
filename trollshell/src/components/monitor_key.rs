//! Shared per-monitor map key, used by every per-monitor state map that
//! needs to outlive a single bar rebuild (`modal::DRAWER_OPEN`,
//! `overlays::sidebar::SIDEBAR_OPEN`, and each module's own `PANELS`).
//!
//! Connector-named monitors use their connector id (`"DP-1"`, …), which is
//! stable across a hot-plug rebuild: `monitors_changed` tears the bars down
//! and rebuilds them, but a still-connected physical monitor keeps its
//! connector name, so a subscriber wired up before the rebuild keeps working
//! after it (that's the whole reason these maps are keyed at all rather than
//! living on the per-rebuild panel struct).
//!
//! Monitors GDK reports without a connector name (headless/virtual outputs)
//! fall back to the `GdkMonitor` pointer's debug representation. That
//! address is *not* stable across a rebuild — the next `monitors_changed`
//! cycle gets a fresh `GdkMonitor` at a new address — so a fallback-keyed
//! entry can never be looked up again once its owning monitor is torn down.
//! Callers that keep a per-monitor `HashMap` keyed by [`monitor_key`] MUST
//! prune fallback-keyed entries in their `close_all` (see [`is_fallback_key`])
//! or they leak one `Mutable` per hot-plug cycle for every connector-less
//! monitor.

use hytte::prelude::*;

/// Prefix used for the connector-less fallback key. Exposed so `close_all`
/// implementations can identify (and prune) fallback entries without
/// duplicating the format string.
const FALLBACK_PREFIX: &str = "monitor:";

/// Stable-when-possible key for per-monitor state maps. Prefer the
/// connector name; fall back to the `GdkMonitor` pointer address for
/// monitors GDK doesn't name (see module docs for why that fallback is
/// inherently non-reusable across rebuilds, and must be pruned rather than
/// left to accumulate).
pub(crate) fn monitor_key(m: &Monitor) -> String {
    m.connector()
        .unwrap_or_else(|| format!("{FALLBACK_PREFIX}{:p}", m.gdk()))
}

/// True if `key` was minted by the fallback (pointer-based) path rather than
/// a real connector name. `close_all` implementations use this to prune
/// entries that can never be reused, instead of letting them accumulate one
/// per hot-plug cycle.
pub(crate) fn is_fallback_key(key: &str) -> bool {
    key.starts_with(FALLBACK_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_keys_are_recognized() {
        assert!(is_fallback_key("monitor:0x1234"));
        assert!(!is_fallback_key("DP-1"));
        assert!(!is_fallback_key("HDMI-A-1"));
    }
}
