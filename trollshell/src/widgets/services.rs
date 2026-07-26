use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::systemd;

/// Services bar chip: a failed-unit indicator that opens the Services stats
/// view. In the `combined`/`multicolumn` layouts it opens the shared
/// [`crate::modal::Page::Stats`] scrolled to the Services card (#508: a way in
/// rather than its own page); in the `split` layout it opens its own
/// [`crate::modal::Page::StatsServices`] page (#307's shape). The chip
/// self-hides while every unit is healthy and appears — showing the
/// failed-unit count — only when something is broken, mirroring the swap-row /
/// GPU-card self-hide convention so the bar stays quiet until it has something
/// to report.
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

    let count_for_bind = count_label.clone();
    bind(systemd::failed_units(), &btn, move |w, units| {
        let n = units.len();
        w.set_visible(n > 0);
        count_for_bind.set_label(&n.to_string());
        if n > 0 {
            w.set_tooltip_text(Some(&format!("{n} failed unit(s)")));
        } else {
            w.set_tooltip_text(None);
        }
    });

    btn.upcast()
}
