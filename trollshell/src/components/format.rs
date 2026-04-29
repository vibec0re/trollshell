//! Formatters used across panels and widgets. Pure functions; no side
//! effects, no allocation beyond the returned `String`.

use std::time::SystemTime;

/// Format a byte count as a human-readable string (e.g. `"7.4 GiB"`).
pub(crate) fn fmt_bytes(b: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let f = b as f64;
    if f >= 1_073_741_824.0 {
        format!("{:.1} GiB", f / 1_073_741_824.0)
    } else if f >= 1_048_576.0 {
        format!("{:.1} MiB", f / 1_048_576.0)
    } else if f >= 1024.0 {
        format!("{:.1} KiB", f / 1024.0)
    } else {
        format!("{f:.0} B")
    }
}

/// Format a byte-per-second rate as a human-readable string (e.g. `"7.4 GiB/s"`).
pub(crate) fn fmt_rate(bps: f64) -> String {
    if bps >= 1_073_741_824.0 {
        format!("{:.1} GiB/s", bps / 1_073_741_824.0)
    } else if bps >= 1_048_576.0 {
        format!("{:.1} MiB/s", bps / 1_048_576.0)
    } else if bps >= 1024.0 {
        format!("{:.1} KiB/s", bps / 1024.0)
    } else {
        format!("{bps:.0} B/s")
    }
}

/// Format a duration in microseconds as `M:SS` (used by the media panel
/// for player position / track length).
pub(crate) fn fmt_us(us: u64) -> String {
    let secs = us / 1_000_000;
    let m = secs / 60;
    let s = secs % 60;
    format!("{m}:{s:02}")
}

/// Render a `SystemTime` as a relative `Xs/m/h/d ago`, or
/// `"moments from now"` for a future timestamp. Used by the VPN panel
/// for tunnel `since` and per-peer last-handshake.
pub(crate) fn humanize_since(t: SystemTime) -> String {
    let now = SystemTime::now();
    match now.duration_since(t) {
        Ok(d) => {
            let secs = d.as_secs();
            if secs < 60 {
                format!("{secs}s ago")
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86400)
            }
        }
        Err(_) => "moments from now".to_string(),
    }
}
