mod widgets;

use std::cell::RefCell;

use hytte::futures_signals::signal::SignalExt;
use hytte::gtk::glib;
use hytte::prelude::*;
use hytte::services::{clock, networkd, niri, pipewire, resolved, tray, upower};

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
                        *bars.borrow_mut() = monitors.iter().map(build_bar).collect();
                        std::future::ready(())
                    })
                    .await;
            });
        })
}

fn build_bar(monitor: &Monitor) -> BarHandle {
    Bar::new(monitor)
        .edge(Edge::Top)
        .exclusive(true)
        .keyboard_interactivity(KeyboardMode::OnDemand)
        .left([
            widgets::workspaces::widget(monitor),
            widgets::window_list::widget(monitor),
        ])
        .right([
            widgets::tray::widget(),
            widgets::network::widget(),
            widgets::volume::widget(),
            widgets::battery::widget(),
            widgets::clock::widget(),
        ])
        .show()
}
