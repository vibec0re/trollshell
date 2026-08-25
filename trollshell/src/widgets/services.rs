use hytte::futures_signals::map_ref;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::reactive::health;
use hytte::services::systemd;

/// Services bar chip: a "something on this box is broken" indicator that opens
/// the Services stats view. In the `combined`/`multicolumn` layouts it opens the
/// shared [`crate::modal::Page::Stats`] scrolled to the Services card (#508: a
/// way in rather than its own page); in the `split` layout it opens its own
/// [`crate::modal::Page::StatsServices`] page (#307's shape). The chip
/// self-hides while **both** of that card's sources are quiet — systemd has no
/// failed unit *and* no supervised shell task is flapping — and appears,
/// showing the combined count, as soon as either has something to report,
/// mirroring the swap-row / GPU-card self-hide convention so the bar stays
/// quiet until it has something to say.
///
/// Both sources have to be in that predicate (#722). The Services card grew a
/// second group for the shell's own flapping supervised tasks, and in `split`
/// layout this chip is the *only* route to the page holding it: hiding on
/// failed units alone would make the flapping list unreachable in exactly the
/// case it exists for — systemd clean, a shell task crash-looping.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = if crate::panels::stats::stats_layout() == crate::panels::stats::StatsLayout::Split {
        crate::components::chip::indicator(
            "ts-services",
            crate::modal::Page::StatsServices,
            monitor,
        )
    } else {
        let monitor_for_scroll = monitor.clone();
        crate::components::chip::indicator_scroll(
            "ts-services",
            crate::modal::Page::Stats,
            monitor,
            move || {
                crate::panels::stats::set_scroll_target(
                    &monitor_for_scroll,
                    crate::panels::stats::StatsSection::Services,
                );
            },
        )
    };

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 3);

    let icon = gtk::Image::from_file(crate::assets::path("icons/emblem-system.svg"));
    icon.set_pixel_size(crate::scale::scale(16));
    row.append(&icon);

    let count_label = gtk::Label::new(None);
    count_label.add_css_class("ts-services-count");
    row.append(&count_label);

    btn.set_child(Some(&row));

    // Project both sources down to their counts before binding: the health
    // signal emits on every supervisor transition (including the start-up burst
    // of one event per service task), and `dedupe` on the pair keeps the chip
    // from re-rendering for transitions that change neither number.
    let counts = map_ref! {
        let units = systemd::failed_units(),
        let tasks = health::signal() => {
            (
                units.len(),
                tasks
                    .iter()
                    .filter(|task| crate::panels::stats::is_flapping(task.consecutive_panics))
                    .count(),
            )
        }
    }
    .dedupe();

    let count_for_bind = count_label.clone();
    bind(counts, &btn, move |w, (failed, flapping)| {
        if let Some((total, tooltip)) = chip_summary(failed, flapping) {
            w.set_visible(true);
            count_for_bind.set_label(&total.to_string());
            w.set_tooltip_text(Some(&tooltip));
        } else {
            w.set_visible(false);
            count_for_bind.set_label("0");
            w.set_tooltip_text(None);
        }
    });

    btn.upcast()
}

/// The chip's badge count and tooltip for one pair of counts, or `None` when
/// there is nothing to report — which is the self-hide predicate itself, so the
/// badge can never read `0` on a visible chip.
///
/// The tooltip names only the sources that actually contributed: a flapping
/// shell task on an otherwise-clean box says so rather than hiding behind a
/// bare number the Services card would then have to explain.
fn chip_summary(failed_units: usize, flapping_tasks: usize) -> Option<(usize, String)> {
    let mut parts = Vec::with_capacity(2);
    if failed_units > 0 {
        parts.push(format!(
            "{failed_units} failed unit{}",
            plural_s(failed_units)
        ));
    }
    if flapping_tasks > 0 {
        parts.push(format!(
            "{flapping_tasks} flapping shell task{}",
            plural_s(flapping_tasks)
        ));
    }
    if parts.is_empty() {
        return None;
    }
    Some((failed_units + flapping_tasks, parts.join(", ")))
}

/// `"s"` unless `n` is exactly one.
fn plural_s(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::chip_summary;

    /// The self-hide half of the predicate: quiet only when *both* sources are.
    /// This is the pre-#722 behaviour for the failed-units column and the new
    /// half for the flapping column.
    #[test]
    fn the_chip_hides_only_when_both_sources_are_clean() {
        assert_eq!(chip_summary(0, 0), None, "nothing to report, no chip");
        assert!(
            chip_summary(1, 0).is_some(),
            "a failed unit alone still shows the chip"
        );
        assert!(
            chip_summary(0, 1).is_some(),
            "a flapping shell task alone must show the chip too — in `split` layout this chip is \
             the only route to the card holding the flapping list (#722)"
        );
    }

    /// Failed units alone read as they did before #722, bar the `(s)` literal.
    #[test]
    fn failed_units_alone_keep_their_own_wording() {
        assert_eq!(chip_summary(1, 0), Some((1, "1 failed unit".to_owned())));
        assert_eq!(chip_summary(2, 0), Some((2, "2 failed units".to_owned())));
    }

    /// A flapping task alone names itself rather than borrowing systemd's noun.
    #[test]
    fn a_flapping_task_alone_names_the_shell() {
        assert_eq!(
            chip_summary(0, 1),
            Some((1, "1 flapping shell task".to_owned()))
        );
    }

    /// Both sources: the badge is the sum, and the tooltip says which is which
    /// so a `3` is never ambiguous about what broke.
    #[test]
    fn both_sources_are_summed_and_spelled_out() {
        assert_eq!(
            chip_summary(2, 1),
            Some((3, "2 failed units, 1 flapping shell task".to_owned()))
        );
        assert_eq!(
            chip_summary(1, 2),
            Some((3, "1 failed unit, 2 flapping shell tasks".to_owned()))
        );
    }
}
