use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::upower::{self, BatteryState, WarningLevel};

/// Charge tier driving the bar chip's icon color. Mapped from
/// `(percentage, state)` by [`tier`]; see `class_name` for the CSS classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Good,
    Warn,
    /// Red, steady.
    Critical,
    /// Red, pulsing.
    Emergency,
}

/// Charge below this (%) colors the icon yellow; at or above it, green.
const YELLOW_PCT: f64 = 25.0;
/// Charge below this (%) colors the icon red.
const RED_PCT: f64 = 10.0;
/// Fallback pulse threshold (%), used **only** when `UPower` has never
/// reported a real `WarningLevel` for this device (`WarningLevel::Unknown`
/// — some drivers/hardware don't populate it). When a real `WarningLevel` is
/// known, [`upower::is_critical`] drives the pulse instead, so the chip
/// can't drift from the toast in `upower.rs` (#656; this constant used to be
/// the *only* pulse trigger).
const FLASH_PCT: f64 = 3.0;

/// Pure mapping of charge level to a color tier.
///
/// Green at or above [`YELLOW_PCT`], yellow down to [`RED_PCT`], then red.
/// Red appears below [`RED_PCT`] regardless of charging state.
///
/// The pulse ([`Tier::Emergency`]) fires when `UPower`'s own `WarningLevel`
/// reaches `Critical`/`Action` — [`upower::is_critical`], the same severity
/// split that drives the "Battery critical" toast (`upower::warning_toast`)
/// — **and** the battery is discharging, so a battery plugged in at a low
/// charge shows steady red, not a blink. If `WarningLevel` has never been
/// reported (`Unknown`), falls back to the old [`FLASH_PCT`] percentage
/// threshold rather than never pulsing. The `UPower` `icon_name` already
/// swaps to a `*-charging-symbolic` glyph on AC, so the charging state stays
/// clear.
fn tier(percentage: f64, state: &BatteryState, warning_level: WarningLevel) -> Tier {
    let discharging = *state == BatteryState::Discharging;
    let emergency = if warning_level == WarningLevel::Unknown {
        percentage < FLASH_PCT
    } else {
        upower::is_critical(warning_level)
    };
    if emergency && discharging {
        Tier::Emergency
    } else if percentage < RED_PCT {
        Tier::Critical
    } else if percentage < YELLOW_PCT {
        Tier::Warn
    } else {
        Tier::Good
    }
}

fn class_name(t: Tier) -> &'static str {
    match t {
        Tier::Good => "ts-battery-good",
        Tier::Warn => "ts-battery-warn",
        Tier::Critical => "ts-battery-critical",
        Tier::Emergency => "ts-battery-emergency",
    }
}

/// Every tier class, stripped before each re-apply so rebinds stay idempotent.
const TIER_CLASSES: [&str; 4] = [
    "ts-battery-good",
    "ts-battery-warn",
    "ts-battery-critical",
    "ts-battery-emergency",
];

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = crate::components::chip::indicator("ts-battery", crate::modal::Page::Power, monitor);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 3);

    let icon = gtk::Image::new();
    row.append(&icon);

    let label = gtk::Label::new(None);
    label.add_css_class("ts-battery-label");
    row.append(&label);

    btn.set_child(Some(&row));

    // Icon follows UPower's icon_name.
    bind(upower::battery(), &icon, |w, b| {
        let name = if b.icon_name.is_empty() {
            "battery-missing-symbolic"
        } else {
            &b.icon_name
        };
        w.set_icon_name(Some(name));
    });

    // Percentage label.
    bind_text(
        upower::battery().map(|b| format!("{:.0}%", b.percentage)),
        &label,
    );

    // Icon color tier: green at/above 25%, yellow down to 10%, then red —
    // pulsing when UPower's WarningLevel reaches Critical/Action while
    // discharging (same severity the "Battery critical" toast uses; falls
    // back to a percentage threshold only if WarningLevel is unreported).
    // Strip all tier classes before adding the active one so the apply is
    // idempotent across re-emissions.
    bind(
        upower::battery().map(|b| tier(b.percentage, &b.state, b.warning_level)),
        &btn,
        |btn, t| {
            for c in TIER_CLASSES {
                btn.remove_css_class(c);
            }
            btn.add_css_class(class_name(t));
        },
    );

    // Hide entirely when no battery is present (desktop systems). UPower
    // reports state = Unknown (discriminant 0) on such systems.
    bind_visible(
        upower::battery().map(|b| b.state != BatteryState::Unknown),
        &btn,
    );

    btn.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_boundaries_discharging_non_critical_warning_level() {
        // With a non-critical WarningLevel, tier tracks percentage exactly
        // as before #656 — YELLOW_PCT/RED_PCT are untouched by this change.
        let wl = WarningLevel::None;
        assert_eq!(tier(100.0, &BatteryState::Discharging, wl), Tier::Good);
        // Regression: a healthy mid charge must read green, not yellow.
        assert_eq!(tier(57.0, &BatteryState::Discharging, wl), Tier::Good);
        assert_eq!(tier(25.0, &BatteryState::Discharging, wl), Tier::Good);
        assert_eq!(tier(24.9, &BatteryState::Discharging, wl), Tier::Warn);
        assert_eq!(tier(10.0, &BatteryState::Discharging, wl), Tier::Warn);
        assert_eq!(tier(9.9, &BatteryState::Discharging, wl), Tier::Critical);
        assert_eq!(tier(0.0, &BatteryState::Discharging, wl), Tier::Critical);
    }

    #[test]
    fn critical_or_action_warning_level_pulses_while_discharging() {
        // The core of #656: WarningLevel::Critical/Action pulses regardless
        // of percentage — including well above the old 3% FLASH_PCT, which
        // is the behaviour change from removing the hardcoded threshold.
        assert_eq!(
            tier(50.0, &BatteryState::Discharging, WarningLevel::Critical),
            Tier::Emergency
        );
        assert_eq!(
            tier(50.0, &BatteryState::Discharging, WarningLevel::Action),
            Tier::Emergency
        );
    }

    #[test]
    fn low_warning_level_does_not_pulse() {
        // Low is a real warning (it drives the "Battery low" toast at
        // Urgency::Normal) but not the pulse-worthy tier — only
        // Critical/Action is, matching upower::is_critical exactly.
        assert_eq!(
            tier(15.0, &BatteryState::Discharging, WarningLevel::Low),
            Tier::Warn
        );
    }

    #[test]
    fn red_shows_when_charging_but_never_pulses() {
        // Below 10% while not discharging → steady red, never Emergency,
        // even at Critical/Action — the #237 Discharging guard is preserved.
        assert_eq!(
            tier(9.0, &BatteryState::Charging, WarningLevel::None),
            Tier::Critical
        );
        assert_eq!(
            tier(5.0, &BatteryState::Charging, WarningLevel::Critical),
            Tier::Critical
        );
        assert_eq!(
            tier(2.0, &BatteryState::FullyCharged, WarningLevel::Action),
            Tier::Critical
        );
        // Unknown means no battery (chip hidden anyway), but well-defined.
        assert_eq!(
            tier(2.0, &BatteryState::Unknown, WarningLevel::None),
            Tier::Critical
        );
    }

    #[test]
    fn unknown_warning_level_falls_back_to_flash_pct() {
        // Hardware/drivers that never populate WarningLevel keep the old
        // percentage-based pulse rather than losing it outright.
        assert_eq!(
            tier(2.9, &BatteryState::Discharging, WarningLevel::Unknown),
            Tier::Emergency
        );
        assert_eq!(
            tier(3.0, &BatteryState::Discharging, WarningLevel::Unknown),
            Tier::Critical
        );
    }
}
