use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::upower::{self, BatteryState};

/// Charge tier driving the bar chip's icon color. Mapped from
/// `(percentage, state)` by [`tier`]; see `class_name` for the CSS classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Good,
    Warn,
    Low,
    Critical,
}

/// Pure mapping of charge level to a color tier.
///
/// `Critical` requires `Discharging` so that being plugged in at a low
/// charge does not blink — it falls through to `Low` (steady orange). The
/// `UPower` `icon_name` already swaps to a `*-charging-symbolic` glyph on AC,
/// so the charging state stays visually clear.
fn tier(percentage: f64, state: &BatteryState) -> Tier {
    if percentage < 10.0 && *state == BatteryState::Discharging {
        Tier::Critical
    } else if percentage < 30.0 {
        Tier::Low
    } else if percentage < 60.0 {
        Tier::Warn
    } else {
        Tier::Good
    }
}

fn class_name(t: Tier) -> &'static str {
    match t {
        Tier::Good => "ts-battery-good",
        Tier::Warn => "ts-battery-warn",
        Tier::Low => "ts-battery-low",
        Tier::Critical => "ts-battery-critical",
    }
}

/// Every tier class, stripped before each re-apply so rebinds stay idempotent.
const TIER_CLASSES: [&str; 4] = [
    "ts-battery-good",
    "ts-battery-warn",
    "ts-battery-low",
    "ts-battery-critical",
];

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-battery");

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

    // Icon color tier: green / amber / orange, pulsing red when critically
    // low and discharging. Strip all four tier classes before adding the
    // active one so the apply is idempotent across re-emissions.
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

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |b| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Power, b);
    });
    btn.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_boundaries_discharging() {
        assert_eq!(tier(100.0, &BatteryState::Discharging), Tier::Good);
        assert_eq!(tier(60.0, &BatteryState::Discharging), Tier::Good);
        assert_eq!(tier(59.9, &BatteryState::Discharging), Tier::Warn);
        assert_eq!(tier(30.0, &BatteryState::Discharging), Tier::Warn);
        assert_eq!(tier(29.9, &BatteryState::Discharging), Tier::Low);
        assert_eq!(tier(10.0, &BatteryState::Discharging), Tier::Low);
        assert_eq!(tier(9.9, &BatteryState::Discharging), Tier::Critical);
        assert_eq!(tier(0.0, &BatteryState::Discharging), Tier::Critical);
    }

    #[test]
    fn critical_requires_discharging() {
        // Low charge but not discharging never blinks — falls through to Low.
        assert_eq!(tier(5.0, &BatteryState::Charging), Tier::Low);
        assert_eq!(tier(5.0, &BatteryState::FullyCharged), Tier::Low);
        // Unknown means no battery (chip hidden anyway), but well-defined.
        assert_eq!(tier(5.0, &BatteryState::Unknown), Tier::Low);
    }
}
