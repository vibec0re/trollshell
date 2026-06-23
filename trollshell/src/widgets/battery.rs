use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::upower::{self, BatteryState};

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
/// Charge below this (%) makes the red icon pulse.
const FLASH_PCT: f64 = 3.0;

/// Pure mapping of charge level to a color tier.
///
/// Green at or above [`YELLOW_PCT`], yellow down to [`RED_PCT`], then red.
/// Red appears below [`RED_PCT`] regardless of charging state; the pulse
/// ([`Tier::Emergency`]) only kicks in below [`FLASH_PCT`] **and** while
/// discharging — so a battery plugged in at a low charge shows steady red,
/// not a blink. The `UPower` `icon_name` already swaps to a
/// `*-charging-symbolic` glyph on AC, so the charging state stays clear.
fn tier(percentage: f64, state: &BatteryState) -> Tier {
    if percentage < FLASH_PCT && *state == BatteryState::Discharging {
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
    // pulsing below 3% while discharging. Strip all tier classes before adding
    // the active one so the apply is idempotent across re-emissions.
    bind(
        upower::battery().map(|b| tier(b.percentage, &b.state)),
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
    fn tier_boundaries_discharging() {
        assert_eq!(tier(100.0, &BatteryState::Discharging), Tier::Good);
        // Regression: a healthy mid charge must read green, not yellow.
        assert_eq!(tier(57.0, &BatteryState::Discharging), Tier::Good);
        assert_eq!(tier(25.0, &BatteryState::Discharging), Tier::Good);
        assert_eq!(tier(24.9, &BatteryState::Discharging), Tier::Warn);
        assert_eq!(tier(10.0, &BatteryState::Discharging), Tier::Warn);
        // Red steady from just under 10% down to the flash threshold…
        assert_eq!(tier(9.9, &BatteryState::Discharging), Tier::Critical);
        assert_eq!(tier(3.0, &BatteryState::Discharging), Tier::Critical);
        // …then pulsing red below it.
        assert_eq!(tier(2.9, &BatteryState::Discharging), Tier::Emergency);
        assert_eq!(tier(0.0, &BatteryState::Discharging), Tier::Emergency);
    }

    #[test]
    fn red_shows_when_charging_but_never_pulses() {
        // Below 10% while not discharging → steady red, never Emergency.
        assert_eq!(tier(9.0, &BatteryState::Charging), Tier::Critical);
        assert_eq!(tier(2.0, &BatteryState::Charging), Tier::Critical);
        assert_eq!(tier(2.0, &BatteryState::FullyCharged), Tier::Critical);
        // Unknown means no battery (chip hidden anyway), but well-defined.
        assert_eq!(tier(2.0, &BatteryState::Unknown), Tier::Critical);
    }
}
