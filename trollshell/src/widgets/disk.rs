use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors::{self, DiskUsage};

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    // Layout-dependent target (#508): `split` → own page; otherwise the shared
    // combined/multicolumn `Page::Stats` with a scroll-to-section target.
    let btn = if crate::panels::stats::stats_layout() == crate::panels::stats::StatsLayout::Split {
        crate::components::chip::indicator("ts-disk", crate::modal::Page::StatsDisks, monitor)
    } else {
        let monitor_for_scroll = monitor.clone();
        crate::components::chip::indicator_scroll(
            "ts-disk",
            crate::modal::Page::Stats,
            monitor,
            move || {
                crate::panels::stats::set_scroll_target(
                    &monitor_for_scroll,
                    crate::panels::stats::StatsSection::Disks,
                );
            },
        )
    };

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 3);

    let icon = gtk::Image::from_file(crate::assets::path("icons/disk.svg"));
    icon.set_pixel_size(crate::scale::scale(16));
    row.append(&icon);

    // One tiny vertical bar per mount.
    let mounts_container = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    mounts_container.add_css_class("ts-disk-mounts");
    mounts_container.set_valign(gtk::Align::Center);
    row.append(&mounts_container);

    btn.set_child(Some(&row));

    bind_disk_mounts(&mounts_container, sensors::disk());

    btn.upcast()
}

/// Rebuild the one-bar-per-mount strip inside `mounts_container` from
/// `signal`.
///
/// Split out of [`widget`] so this `bind` call site's `WeakRef` contract
/// (#224, `hytte-reactive/src/bind.rs:16-22`) can be driven with a synthetic
/// signal in tests, the same extraction #772 made for its four sites. The
/// builder reads `sensors::disk()` inline, which `.expect()`s without a
/// registered `Registry` — and it also needs a `Monitor`, so the chip as a
/// whole stays out of reach of a unit test even now (#831).
fn bind_disk_mounts<S>(mounts_container: &gtk::Box, signal: S)
where
    S: Signal<Item = DiskUsage> + 'static,
{
    bind(signal, mounts_container, move |mounts_container, disk| {
        while let Some(c) = mounts_container.first_child() {
            mounts_container.remove(&c);
        }
        for m in &disk.mounts {
            let bar = crate::components::chip::vertical_bar();
            bar.set_fraction(m.usage.clamp(0.0, 1.0));
            bar.set_tooltip_text(Some(&format!("{}: {:.0}%", m.path, m.usage * 100.0)));
            mounts_container.append(&bar);
        }
    });
}

/// #831 regression coverage for this file's widget-pinning `bind` call site,
/// in the shape `panels/connections.rs` established for #772: the apply
/// closure must take the `&gtk::Box` `bind` hands it rather than a strong
/// clone captured from the enclosing scope, or the binding keeps the strip
/// alive for its own lifetime and defeats #224's `WeakRef` contract
/// (`hytte-reactive/src/bind.rs:16-22`).
#[cfg(all(test, feature = "system-tests"))]
mod pin_tests {
    use hytte::adw;
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk::{self, prelude::*};
    use hytte::services::sensors::{DiskMount, DiskUsage};

    use super::bind_disk_mounts;

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    fn mount(path: &str) -> DiskMount {
        DiskMount {
            path: path.to_owned(),
            total_bytes: 100,
            used_bytes: 50,
            free_bytes: 50,
            usage: 0.5,
        }
    }

    fn two_mounts() -> DiskUsage {
        DiskUsage {
            mounts: vec![mount("/"), mount("/home")],
        }
    }

    /// Number of bars currently in the strip.
    fn bars(container: &gtk::Box) -> usize {
        let mut n = 0;
        let mut child = container.first_child();
        while let Some(c) = child {
            n += 1;
            child = c.next_sibling();
        }
        n
    }

    /// Anti-vacuity guard for the pin test below: the binding must actually
    /// apply, or "the widget died" would prove nothing about the closure.
    #[gtk::test]
    fn disk_mounts_binding_applies_a_value() {
        adw::init().expect("libadwaita init");
        let mounts_container = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        let disk: Mutable<DiskUsage> = Mutable::new(DiskUsage::default());
        bind_disk_mounts(&mounts_container, disk.signal_cloned());
        pump();

        disk.set(two_mounts());
        pump();

        assert_eq!(
            bars(&mounts_container),
            2,
            "the emitted DiskUsage's two mounts must reach the strip as two bars"
        );
    }

    /// Falsified by reintroducing the `mounts_for_signal` strong clone the
    /// apply closure used to capture: with it, `drop(mounts_container)` is not
    /// the last strong ref and the weak upgrade still succeeds.
    #[gtk::test]
    fn disk_mounts_binding_does_not_pin_container() {
        adw::init().expect("libadwaita init");
        let mounts_container = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        let weak = mounts_container.downgrade();
        let disk: Mutable<DiskUsage> = Mutable::new(two_mounts());
        bind_disk_mounts(&mounts_container, disk.signal_cloned());
        pump();

        drop(mounts_container);

        assert!(
            weak.upgrade().is_none(),
            "bind_disk_mounts must not pin its container: a strong clone captured by the apply \
             closure (rather than taking the closure's own `&gtk::Box` argument from `bind`) \
             would keep this alive for the life of the binding, defeating #224's WeakRef contract"
        );

        // The binding must release cleanly on the next emission, not panic on
        // a dead weak ref: `bind` upgrades, gets `None`, and breaks its loop.
        disk.set(two_mounts());
        pump();
    }
}
