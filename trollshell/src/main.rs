mod assets;
mod components;
mod modal;
mod overlays;
mod panels;
mod widgets;

use std::cell::RefCell;

use hytte::futures_signals::signal::SignalExt;
use hytte::gtk;
use hytte::gtk::{glib, prelude::*};
use hytte::prelude::*;
use hytte::services::{
    bluetooth, bluetooth_audio, brightness, calendar, clipboard, clock, departures, displays,
    dnd, mpris, netconn, networkd, niri, notifications, notifications_mute, pipewire, polkit,
    power_profiles, resolved, screensaver, sensors, systemd, tasks, tray, upower, vpn, wallpaper,
    wifi,
};

fn main() -> hytte::ui::Result<()> {
    tracing_subscriber::fmt::init();

    App::new("cc.hannig.trollshell")
        .with(clock::service())
        .with(departures::service())
        .with(niri::service())
        .with(upower::service())
        .with(vpn::service())
        .with(pipewire::service())
        .with(networkd::service())
        .with(resolved::service())
        .with(tray::service())
        .with(notifications::service())
        .with(notifications_mute::service())
        .with(dnd::service())
        .with(mpris::service())
        .with(netconn::service())
        .with(bluetooth::service())
        .with(bluetooth_audio::service())
        .with(brightness::service())
        .with(sensors::service())
        .with(wifi::service())
        .with(polkit::service())
        .with(power_profiles::service())
        .with(screensaver::service())
        .with(systemd::service())
        .with(wallpaper::service())
        .with(displays::service())
        .with(clipboard::service())
        .with(calendar::service())
        .with(tasks::service())
        .with_user_style(assets::path("style.css"))
        .run(|app| {
            // GSettings schemas often aren't visible to `cargo run` from the
            // devShell (Nix puts them under share/gsettings-schemas/<pkg>/...,
            // not share/glib-2.0/schemas/). Without the schemas GTK can't
            // read `org.gnome.desktop.interface icon-theme` and falls back
            // to a hicolor-only scan — most Adwaita symbolics render as
            // image-missing because only the subset libadwaita bundles as
            // a gresource is found. Force the theme name so GTK scans
            // adwaita-icon-theme's filesystem Adwaita/ directory directly.
            if let Some(s) = gtk::Settings::default() {
                s.set_gtk_icon_theme_name(Some("Adwaita"));
            }

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
                        overlays::sidebar::close_all();
                        *bars.borrow_mut() = monitors.iter().map(build_bar).collect();
                        std::future::ready(())
                    })
                    .await;
            });

            // Spawn the bluetooth-audio auto-switch reactor on the GTK main
            // loop. Must run after services are registered so it can pull
            // bluetooth + pipewire signals out of the registry.
            bluetooth_audio::init();

            // Password prompt overlay — reacts to wifi::active_prompt() signal.
            // Polkit auth dialog — reacts to polkit::auth_prompts() signal.
            if let Some(primary) = app.monitors().first() {
                overlays::prompt::install(primary);
                overlays::polkit_dialog::install(primary);
            }

            // Lock screen: ext-session-lock-v1 client. The subscription
            // reacts to screensaver::is_locked() and creates/destroys a
            // session-lock Instance each cycle. Monitor enumeration +
            // hot-plug coverage are handled by the compositor's
            // connect_monitor fires; no monitor list needed at install.
            overlays::lock_screen::install();

            // Notifications + OSD mount on every monitor; routing picks the focused one.
            for monitor in &app.monitors() {
                overlays::frame::install(monitor);
                overlays::notifications::install(monitor);
                overlays::osd::install(monitor);
            }
        })
}

fn build_bar(monitor: &Monitor) -> BarHandle {
    modal::install(monitor);
    overlays::sidebar::install(monitor);
    let bar = Bar::new(monitor)
        .edge(Edge::Top)
        .exclusive(true)
        .keyboard_interactivity(KeyboardMode::OnDemand)
        .left([
            widgets::sidebar_toggle::widget(monitor),
            widgets::workspaces::widget(monitor),
            widgets::window_list::widget(monitor),
        ])
        .center([widgets::mpris::widget(monitor)])
        .right([
            group([widgets::tray::widget()]),
            group([
                widgets::bluetooth::widget(monitor),
                widgets::network::widget(monitor),
                widgets::vpn::widget(monitor),
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
            group([widgets::clock::widget(monitor)]),
            group([
                widgets::notif_indicator::widget(monitor),
                widgets::settings_chip::widget(monitor),
                widgets::power_chip::widget(monitor),
            ]),
        ])
        .show();

    // When the drawer is open on this monitor, mark the bar window so CSS
    // can square off the bottom-right corner (seam between bar and drawer).
    bind_class(modal::drawer_open_signal(monitor), bar.window(), "drawer-open");

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
