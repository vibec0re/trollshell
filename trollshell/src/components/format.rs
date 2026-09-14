//! Formatters used across panels and widgets. Pure functions; no side
//! effects, no allocation beyond the returned `String`.

use std::time::SystemTime;

use crate::components::cast;

/// Inner formatter: given a non-negative float representing bytes, return the
/// magnitude string without any suffix (e.g. `"7.4 GiB"`). Called by both
/// [`fmt_bytes`] and [`fmt_rate`] so the threshold logic lives in one place.
fn fmt_bytes_f64(f: f64) -> String {
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

/// Format a byte count as a human-readable string (e.g. `"7.4 GiB"`).
pub(crate) fn fmt_bytes(b: u64) -> String {
    fmt_bytes_f64(cast::u64_to_f64(b))
}

/// Format a byte-per-second rate as a human-readable string (e.g. `"7.4 GiB/s"`).
pub(crate) fn fmt_rate(bps: f64) -> String {
    format!("{}/s", fmt_bytes_f64(bps))
}

/// Format a clock frequency in Hz as a human-readable string (e.g. `"3.8 GHz"`).
/// Used by the CPU-clock history row (`sensors::cpu_freq()`); modern CPU
/// clocks land in the GHz range, but lower magnitudes degrade gracefully.
pub(crate) fn fmt_hz(hz: f64) -> String {
    if hz >= 1_000_000_000.0 {
        format!("{:.1} GHz", hz / 1_000_000_000.0)
    } else if hz >= 1_000_000.0 {
        format!("{:.0} MHz", hz / 1_000_000.0)
    } else if hz >= 1_000.0 {
        format!("{:.0} kHz", hz / 1_000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

/// Format a [`std::time::Duration`] as a human-readable string with a caller-
/// supplied suffix (e.g. `"1h 30m until full"`).
pub(crate) fn fmt_dur(d: std::time::Duration, suffix: &str) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    if h > 0 {
        format!("{h}h {m}m {suffix}")
    } else {
        format!("{m}m {suffix}")
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
    humanize_since_at(t, SystemTime::now())
}

/// Core of [`humanize_since`], parameterized on "now" so the s/m/h/d ladder
/// and the future-timestamp branch are testable without depending on the
/// wall clock.
fn humanize_since_at(t: SystemTime, now: SystemTime) -> String {
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

/// The character cap [`truncate_for_row`] applies — long enough that an
/// ordinary title still reads in full, short enough to keep a boxed-list row
/// from dominating the drawer's width. Roughly double `widgets/window_list.rs`'s
/// 30-char cap on its narrow bar pill, sized instead for a full-width drawer
/// row.
const ROW_TEXT_CAP: usize = 60;

/// Cap `s` to [`ROW_TEXT_CAP`] characters, replacing anything past that with
/// a single ellipsis character. A no-op when `s` already fits.
///
/// # Why this exists (#1302)
///
/// `AdwActionRow`/`AdwExpanderRow`'s `title-lines`/`subtitle-lines`
/// (`gtk_label_set_lines` + `Pango::EllipsizeMode::End` on the row's internal
/// label — libadwaita's `adw_action_row_set_title_lines`) only change how the
/// label *renders* once it's given less width than it naturally wants; they do
/// not shrink the label's own natural-width request, which is exactly what
/// feeds `modal.rs`'s drawer sizing on the way up through the row's ancestors.
/// Measured directly: an `AdwActionRow` titled with a 300-character
/// single-token name reports the same ~2400px natural width whether
/// `title-lines` is 0 or 1 — Pango lays out an unconstrained (`for_size = -1`)
/// wrapping label as if it could have the whole string on one line, and
/// `title-lines` only caps what happens if it *doesn't* get that. A bare
/// `gtk::Label` has an escape hatch for this (`set_max_width_chars`, already
/// used by `widgets/window_list.rs` and `panels/media.rs`'s
/// `ellipsized_label`), but neither row type exposes an equivalent property
/// for its built-in title/subtitle labels — so the string itself is capped
/// before it ever reaches `set_title`/`set_subtitle`, which bounds the row's
/// natural width directly (there's nothing longer than the cap to lay out).
///
/// Every call site pairs this with `title_lines(1)`/`subtitle_lines(1)` as a
/// second line of defense (an unusual font or DPI could still make even a
/// capped string wider than the space actually allocated) and keeps the
/// *untruncated* original string in a tooltip — this function only ever
/// touches what's drawn on the row, never what a hover reveals.
pub(crate) fn truncate_for_row(s: &str) -> String {
    if s.chars().count() <= ROW_TEXT_CAP {
        return s.to_string();
    }
    let mut out: String = s.chars().take(ROW_TEXT_CAP - 1).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── fmt_bytes ──────────────────────────────────────────────────────────

    #[test]
    fn fmt_bytes_just_below_kib_stays_in_bytes() {
        assert_eq!(fmt_bytes(1023), "1023 B");
    }

    #[test]
    fn fmt_bytes_at_kib_boundary_switches_unit() {
        assert_eq!(fmt_bytes(1024), "1.0 KiB");
    }

    #[test]
    fn fmt_bytes_just_below_mib_boundary() {
        // 1_048_575 / 1024 = 1023.9990234375, which the `{:.1}` formatter
        // rounds up to "1024.0" even though the value is still one byte
        // short of the MiB threshold and stays in the KiB bucket.
        assert_eq!(fmt_bytes(1_048_575), "1024.0 KiB");
    }

    #[test]
    fn fmt_bytes_at_mib_boundary_switches_unit() {
        assert_eq!(fmt_bytes(1_048_576), "1.0 MiB");
    }

    #[test]
    fn fmt_bytes_at_gib_boundary_switches_unit() {
        assert_eq!(fmt_bytes(1_073_741_824), "1.0 GiB");
    }

    #[test]
    fn fmt_rate_appends_per_second_suffix() {
        assert_eq!(fmt_rate(1024.0), "1.0 KiB/s");
    }

    // ── fmt_hz ─────────────────────────────────────────────────────────────

    #[test]
    fn fmt_hz_below_khz_stays_in_hz() {
        assert_eq!(fmt_hz(500.0), "500 Hz");
    }

    #[test]
    fn fmt_hz_at_khz_boundary_switches_unit() {
        assert_eq!(fmt_hz(1_000.0), "1 kHz");
    }

    #[test]
    fn fmt_hz_at_mhz_boundary_switches_unit() {
        assert_eq!(fmt_hz(1_000_000.0), "1 MHz");
    }

    #[test]
    fn fmt_hz_at_ghz_boundary_switches_unit() {
        assert_eq!(fmt_hz(1_000_000_000.0), "1.0 GHz");
    }

    #[test]
    fn fmt_hz_typical_cpu_clock() {
        assert_eq!(fmt_hz(3_800_000_000.0), "3.8 GHz");
    }

    // ── fmt_dur ────────────────────────────────────────────────────────────

    #[test]
    fn fmt_dur_just_under_an_hour_has_no_hour_component() {
        assert_eq!(
            fmt_dur(Duration::from_mins(59), "until full"),
            "59m until full"
        );
    }

    #[test]
    fn fmt_dur_at_exactly_one_hour() {
        assert_eq!(
            fmt_dur(Duration::from_hours(1), "until full"),
            "1h 0m until full"
        );
    }

    #[test]
    fn fmt_dur_at_ninety_minutes() {
        assert_eq!(
            fmt_dur(Duration::from_mins(90), "until full"),
            "1h 30m until full"
        );
    }

    // ── fmt_us ─────────────────────────────────────────────────────────────

    #[test]
    fn fmt_us_zero_pads_seconds_under_ten() {
        assert_eq!(fmt_us(5_000_000), "0:05");
    }

    #[test]
    fn fmt_us_zero() {
        assert_eq!(fmt_us(0), "0:00");
    }

    #[test]
    fn fmt_us_minutes_and_seconds() {
        assert_eq!(fmt_us(65_000_000), "1:05");
    }

    #[test]
    fn fmt_us_does_not_wrap_minutes_into_hours() {
        // fmt_us renders M:SS, not H:MM:SS — 61 minutes stays as "61:01".
        assert_eq!(fmt_us(3_661_000_000), "61:01");
    }

    // ── humanize_since_at ─────────────────────────────────────────────────

    fn at_offset(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn humanize_since_seconds_ago() {
        let now = at_offset(1_000_000);
        assert_eq!(humanize_since_at(at_offset(999_970), now), "30s ago");
    }

    #[test]
    fn humanize_since_just_under_a_minute_stays_seconds() {
        let now = at_offset(1_000_000);
        assert_eq!(humanize_since_at(at_offset(999_941), now), "59s ago");
    }

    #[test]
    fn humanize_since_at_one_minute_switches_to_minutes() {
        let now = at_offset(1_000_000);
        assert_eq!(humanize_since_at(at_offset(999_940), now), "1m ago");
    }

    #[test]
    fn humanize_since_just_under_an_hour_stays_minutes() {
        let now = at_offset(1_000_000);
        assert_eq!(humanize_since_at(at_offset(996_401), now), "59m ago");
    }

    #[test]
    fn humanize_since_at_one_hour_switches_to_hours() {
        let now = at_offset(1_000_000);
        assert_eq!(humanize_since_at(at_offset(996_400), now), "1h ago");
    }

    #[test]
    fn humanize_since_just_under_a_day_stays_hours() {
        let now = at_offset(1_000_000);
        assert_eq!(humanize_since_at(at_offset(913_601), now), "23h ago");
    }

    #[test]
    fn humanize_since_at_one_day_switches_to_days() {
        let now = at_offset(1_000_000);
        assert_eq!(humanize_since_at(at_offset(913_600), now), "1d ago");
    }

    #[test]
    fn humanize_since_future_timestamp() {
        let now = at_offset(1_000_000);
        assert_eq!(
            humanize_since_at(at_offset(1_000_100), now),
            "moments from now"
        );
    }

    // ── truncate_for_row ──────────────────────────────────────────────────

    #[test]
    fn truncate_for_row_is_a_no_op_under_the_cap() {
        assert_eq!(truncate_for_row("short-unit.service"), "short-unit.service");
    }

    #[test]
    fn truncate_for_row_is_a_no_op_exactly_at_the_cap() {
        let s = "x".repeat(ROW_TEXT_CAP);
        assert_eq!(truncate_for_row(&s), s);
    }

    #[test]
    fn truncate_for_row_caps_a_long_name_with_a_trailing_ellipsis() {
        let out = truncate_for_row(&"x".repeat(300));
        assert_eq!(out.chars().count(), ROW_TEXT_CAP);
        assert!(out.ends_with('\u{2026}'));
        assert_eq!(
            &out[..out.len() - '\u{2026}'.len_utf8()],
            "x".repeat(ROW_TEXT_CAP - 1)
        );
    }

    #[test]
    fn truncate_for_row_renders_any_two_over_cap_inputs_of_the_same_character_identically() {
        // The #1302 invariant that actually matters: once a name is over the
        // cap, growing it further must not change what's displayed at all —
        // that's what stops the row's width from tracking name length.
        assert_eq!(
            truncate_for_row(&"x".repeat(90)),
            truncate_for_row(&"x".repeat(300))
        );
    }
}
