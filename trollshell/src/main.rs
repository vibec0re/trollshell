mod modal;
mod widgets;

use std::cell::RefCell;

use hytte::futures_signals::signal::SignalExt;
use hytte::gtk::glib;
use hytte::prelude::*;
use hytte::services::{
    bluetooth, brightness, clock, mpris, networkd, niri, notifications, pipewire, resolved,
    sensors, tray, upower, wifi,
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
        .with(mpris::service())
        .with(bluetooth::service())
        .with(brightness::service())
        .with(sensors::service())
        .with(wifi::service())
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

            // Toast container — single layer-shell window on the primary monitor.
            // Picks the first monitor at startup; doesn't follow hot-plug in v0.4.0.
            if let Some(primary) = app.monitors().first() {
                widgets::notifications::install(primary);
            }
        })
}

fn build_bar(monitor: &Monitor) -> BarHandle {
    modal::install(monitor);
    Bar::new(monitor)
        .edge(Edge::Top)
        .exclusive(true)
        .keyboard_interactivity(KeyboardMode::OnDemand)
        .left([
            widgets::workspaces::widget(monitor),
            widgets::window_list::widget(monitor),
        ])
        .center([widgets::mpris::widget(monitor)])
        .right([
            widgets::tray::widget(),
            widgets::notif_indicator::widget(monitor),
            widgets::bluetooth::widget(monitor),
            widgets::network::widget(monitor),
            widgets::volume::widget(monitor),
            widgets::brightness::widget(monitor),
            widgets::battery::widget(monitor),
            widgets::cpu::widget(monitor),
            widgets::memory::widget(monitor),
            widgets::gpu::widget(monitor),
            widgets::disk::widget(monitor),
            widgets::clock::widget(),
        ])
        .show()
}
