/// Format a byte count as a human-readable string (e.g. `"7.4 GiB"`).
pub fn fmt_bytes(b: u64) -> String {
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
pub fn fmt_rate(bps: f64) -> String {
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
