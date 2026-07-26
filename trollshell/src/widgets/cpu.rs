use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    // Layout-dependent target (#508): in `split` each chip opens its own
    // per-resource page; in `combined`/`multicolumn` they share `Page::Stats`
    // and stash a scroll-to-section target first.
    let btn = if crate::panels::stats::stats_layout() == crate::panels::stats::StatsLayout::Split {
        crate::components::chip::indicator("ts-cpu", crate::modal::Page::StatsCpu, monitor)
    } else {
        let monitor_for_scroll = monitor.clone();
        crate::components::chip::indicator_scroll(
            "ts-cpu",
            crate::modal::Page::Stats,
            monitor,
            move || {
                crate::panels::stats::set_scroll_target(
                    &monitor_for_scroll,
                    crate::panels::stats::StatsSection::Cpu,
                );
            },
        )
    };

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 3);

    let icon = gtk::Image::from_file(crate::assets::path("icons/cpu.svg"));
    icon.set_pixel_size(crate::scale::scale(16));
    row.append(&icon);

    let temp_label = gtk::Label::new(None);
    temp_label.add_css_class("ts-cpu-temp");
    row.append(&temp_label);

    let bar = crate::components::chip::vertical_bar();
    row.append(&bar);

    btn.set_child(Some(&row));

    bind(sensors::cpu(), &bar, |w, c| {
        w.set_fraction(c.overall.clamp(0.0, 1.0));
        w.set_tooltip_text(Some(&format!("CPU {:.0}%", c.overall * 100.0)));
    });

    bind(sensors::cpu_temp(), &temp_label, |w, t| {
        match t.package_celsius {
            Some(c) => w.set_label(&format!("{c:.0}°")),
            None => w.set_label(""),
        }
    });

    btn.upcast()
}
