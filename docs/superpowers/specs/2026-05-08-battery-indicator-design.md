# Battery indicator: tiered colors + critical blink

## Goal

Make the bar's battery chip more glanceable: the icon takes a color that
reflects charge tier, and blinks when the battery is critically low and
the system is on battery (not plugged in).

## Scope

- Bar chip only (`trollshell/src/widgets/battery.rs`).
- Drawer panel (`trollshell/src/panels/power.rs`) is unchanged.

## Tiers

A pure function maps `(percentage, state)` to one of four tiers:

| Tier       | Condition                                        | Icon color | Animation |
| ---------- | ------------------------------------------------ | ---------- | --------- |
| `Good`     | `percentage >= 60`                               | `#66e07a`  | —         |
| `Warn`     | `30 <= percentage < 60`                          | `#ffce4d`  | —         |
| `Low`      | `10 <= percentage < 30`                          | `#ff8a3d`  | —         |
| `Critical` | `percentage < 10` **and** `state == Discharging` | `#ff3b5c`  | pulse     |

`Critical` requires `Discharging` so that being plugged in at 5% does
not blink — `tier()` falls through to `Low` (orange icon, steady). The
existing UPower `icon_name` already swaps to a `*-charging-symbolic`
glyph when on AC, so the charging state stays visually clear.

If `state == Unknown` (no battery present) the chip is hidden by the
existing `bind_visible` and tier never matters.

## Implementation

### Tier enum + pure function

In `trollshell/src/widgets/battery.rs`:

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier { Good, Warn, Low, Critical }

fn tier(percentage: f64, state: BatteryState) -> Tier {
    if percentage < 10.0 && state == BatteryState::Discharging {
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
        Tier::Good     => "ts-battery-good",
        Tier::Warn     => "ts-battery-warn",
        Tier::Low      => "ts-battery-low",
        Tier::Critical => "ts-battery-critical",
    }
}
```

### Binding

Add a third binding on the existing `btn`:

```rust
bind(
    upower::battery().map(|b| tier(b.percentage, b.state)),
    &btn,
    |btn, t| {
        for c in ["ts-battery-good", "ts-battery-warn",
                  "ts-battery-low", "ts-battery-critical"] {
            btn.remove_css_class(c);
        }
        btn.add_css_class(class_name(t));
    },
);
```

Idempotent on rebind: every update strips all four classes before
adding the active one.

### CSS

Append to `trollshell/style.css` after the existing indicator block
(near the `ts-microphone-recording` rule, which uses the same
"color the icon only" pattern):

```css
.ts-battery-good image {
  color: #66e07a;
}
.ts-battery-warn image {
  color: #ffce4d;
}
.ts-battery-low image {
  color: #ff8a3d;
}
.ts-battery-critical image {
  color: #ff3b5c;
  animation: ts-battery-pulse 1s ease-in-out infinite;
}

@keyframes ts-battery-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.3;
  }
}
```

The chip background and the percentage label keep their default
styling — only the symbolic icon takes the tier color.

## Testing

Unit tests in `widgets/battery.rs` covering boundaries of `tier()`:

- `tier(100.0, Discharging) == Good`
- `tier(60.0,  Discharging) == Good`
- `tier(59.9,  Discharging) == Warn`
- `tier(30.0,  Discharging) == Warn`
- `tier(29.9,  Discharging) == Low`
- `tier(10.0,  Discharging) == Low`
- `tier(9.9,   Discharging) == Critical`
- `tier(0.0,   Discharging) == Critical`
- `tier(5.0,   Charging)    == Low` — no blink while charging
- `tier(5.0,   FullyCharged) == Low` — non-Discharging never blinks
- `tier(5.0,   Unknown)     == Low` — hidden anyway, but well-defined

Manual visual verification:

- Run `trollshell` and confirm icon color matches the current charge
  bucket.
- On a low-battery laptop, confirm `< 10%` discharging produces a
  visible 1s opacity pulse on the icon, that the pulse stops on plug-in,
  and that the color drops back to orange/amber/green as charge
  recovers.

## Out of scope

- Drawer panel coloring (`panels/power.rs`).
- Background-color or chip-shape changes.
- User-configurable thresholds or colors.
- Notifications / OSDs at low battery (handled elsewhere).
