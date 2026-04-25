mod modal;
mod widgets;

use std::cell::RefCell;

use hytte::futures_signals::signal::SignalExt;
use hytte::gtk;
use hytte::gtk::{glib, prelude::*};
use hytte::prelude::*;
use hytte::services::{
    bluetooth, bluetooth_audio, brightness, calendar, clipboard, clock, displays, dnd, mpris,
    networkd, niri, notifications, notifications_mute, pipewire, polkit, resolved, screensaver,
    sensors, tray, upower, wallpaper, wifi,
};

fn main() -> hytte::ui::Result<()> {
    tracing_subscriber::fmt::init();

    App::new("cc.hannig.trollshell")
        .with(clock::service())
        .with(niri::service())
        .with(upower::service())
        .with(pipewire::service())
        .with(networkd::service())
        .with(resolved::service())
        .with(tray::service())
        .with(notifications::service())
        .with(notifications_mute::service())
        .with(dnd::service())
        .with(mpris::service())
        .with(bluetooth::service())
        .with(bluetooth_audio::service())
        .with(brightness::service())
        .with(sensors::service())
        .with(wifi::service())
        .with(polkit::service())
        .with(screensaver::service())
        .with(wallpaper::service())
        .with(displays::service())
        .with(clipboard::service())
        .with(calendar::service())
        .with_user_style(concat!(env!("CARGO_MANIFEST_DIR"), "/style.css"))
        .run(|app| {
            // Spawn a task on the GTK main loop that owns the live set of
            // bars. Each emission of monitors_changed (initial + every
            // hot-plug) tears down the old bars and rebuilds for the
            // current monitor set. Dropping a BarHandle closes its window.
            let monitors_signal = app.monitors_changed();
            glib::MainContext::default().spawn_local(async move {
                let bars: RefCell<Vec<BarHandle>> = RefCell::new(Vec::new());
                monitors_signal
                    .for_each(move |monitors| {
                        // Close all existing modals before rebuilding.
                        modal::close_all();
                        *bars.borrow_mut() = monitors.iter().map(build_bar).collect();
                        std::future::ready(())
                    })
                    .await;
            });

            // Spawn the bluetooth-audio auto-switch reactor on the GTK main
            // loop. Must run after services are registered so it can pull
            // bluetooth + pipewire signals out of the registry.
            bluetooth_audio::init();

            // Notification toasts: transient layer-shell window pinned
            // top-right on the primary monitor. Suppressed when DND is on
            // (drawer history still records). Critical notifications bypass
            // DND per freedesktop spec.
            //
            // Password prompt overlay — reacts to wifi::active_prompt() signal.
            // Polkit auth dialog — reacts to polkit::auth_prompts() signal.
            if let Some(primary) = app.monitors().first() {
                widgets::notifications::install(primary);
                widgets::prompt::install(primary);
                widgets::polkit_dialog::install(primary);
                widgets::osd::install(primary);
            }
        })
}

fn build_bar(monitor: &Monitor) -> BarHandle {
    modal::install(monitor);
    let bar = Bar::new(monitor)
        .edge(Edge::Top)
        .exclusive(true)
        .keyboard_interactivity(KeyboardMode::OnDemand)
        .left([
            widgets::workspaces::widget(monitor),
            widgets::window_list::widget(monitor),
        ])
        .center([widgets::mpris::widget(monitor)])
        .right([
            group([widgets::tray::widget()]),
            group([
                widgets::bluetooth::widget(monitor),
                widgets::network::widget(monitor),
            ]),
            group([
                widgets::volume::widget(monitor),
                widgets::microphone::widget(monitor),
                widgets::brightness::widget(monitor),
            ]),
            group([widgets::battery::widget(monitor)]),
            group([
                widgets::cpu::widget(monitor),
                widgets::memory::widget(monitor),
                widgets::gpu::widget(monitor),
                widgets::disk::widget(monitor),
            ]),
            group([
                widgets::settings_chip::widget(monitor),
                widgets::power_chip::widget(monitor),
            ]),
            group([widgets::clock::widget(monitor)]),
            group([widgets::notif_indicator::widget(monitor)]),
        ])
        .show();

    // When the drawer is open on this monitor, mark the bar window so CSS
    // can square off the bottom-right corner (seam between bar and drawer).
    if let Some(signal) = modal::drawer_open_signal(monitor) {
        bind_class(signal, bar.window(), "drawer-open");
    }

    bar
}

/// Wrap a set of related bar chips in a dark-pill subgroup. Rainbow from
/// the outer `.hytte-bar-group-right` pill shows through the gap between
/// adjacent groups.
fn group<const N: usize>(widgets: [gtk::Widget; N]) -> gtk::Widget {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    b.add_css_class("ts-bar-group");
    for w in widgets {
        b.append(&w);
    }
    b.upcast()
}
