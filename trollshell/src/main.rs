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
    app_usage, bluetooth, bluetooth_audio, brightness, calendar, clipboard, clock, departures,
    displays, dnd, geoclue, mpris, netconn, networkd, niri, notifications, notifications_mute,
    pipewire, places, power_profiles, resolved, screensaver, sensors, systemd, tasks, tray, upower,
    vpn, wallpaper, weather, wifi, wifiscan,
};

fn main() -> hytte::ui::Result<()> {
    // `trollshell --scan-aps`: one-shot dump of visible Wi-Fi networks as a
    // paste-ready `ssids = [...]` block for ~/.config/trollshell/places.toml,
    // then exit. Runs before the App, so it needs no Wayland session.
    if std::env::args().any(|a| a == "--scan-aps") {
        let aps = wifiscan::scan_aps_blocking();
        if aps.is_empty() {
            // Distinguish "scan failed / NM down" from "genuinely nothing" —
            // a hint to stderr keeps stdout paste-clean.
            eprintln!(
                "trollshell --scan-aps: no Wi-Fi APs visible — is NetworkManager running and Wi-Fi on? (a scan may also just need a retry)"
            );
        }
        print!("{}", wifiscan::format_scan_block(&aps));
        return Ok(());
    }

    tracing_subscriber::fmt::init();

    App::new("cc.hannig.trollshell")
        .with(clock::service())
        .with(wifiscan::service())
        // wifiscan + geoclue feed `places` (the location resolver); `places`
        // must precede departures + weather, which read its shared handles in
        // their start() to wire re-fetch-on-place-change.
        .with(geoclue::service())
        .with(places::service())
        .with(departures::service())
        .with(weather::service())
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
        .with(app_usage::service())
        .with(wifi::service())
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

            // Gate netconn's always-on `ss -tunpH` poller on drawer
            // visibility (#50): it only feeds the Connections/Network drawer
            // pages, so park it whenever none of those is on-screen. The modal
            // signal is global (true iff a netconn-backed page is visible on
            // *any* monitor) and survives bar rebuilds, so a single
            // subscription on the main loop suffices — no per-monitor wiring.
            glib::MainContext::default().spawn_local(modal::netconn_visible_signal().for_each(
                |visible| {
                    netconn::set_active(visible);
                    std::future::ready(())
                },
            ));

            // Password prompt overlay — reacts to wifi::active_prompt() signal.
            if let Some(primary) = app.monitors().first() {
                overlays::prompt::install(primary);
            }

            // Notifications + OSD mount on every monitor; routing picks the focused one.
            for monitor in &app.monitors() {
                overlays::frame::install(monitor);
                overlays::notifications::install(monitor);
                overlays::osd::install(monitor);
            }
        })
}

fn build_bar(monitor: &Monitor) -> BarHandle {
    // The bar's edge + its margin on that edge. Plumbed into the modal so the
    // drawer anchors to the bar's actual edge with a perpendicular margin
    // derived from the bar's real offset + measured thickness (replacing the
    // old hardcoded top/59). Keep `BAR_EDGE`/`BAR_EDGE_OFFSET` in sync with
    // the `Bar::new(...)` builder below.
    const BAR_EDGE: Edge = Edge::Top;
    const BAR_EDGE_OFFSET: i32 = 0;

    overlays::sidebar::install(monitor);
    let bar = Bar::new(monitor)
        .edge(BAR_EDGE)
        .exclusive(true)
        .keyboard_interactivity(KeyboardMode::OnDemand)
        .left([
            widgets::sidebar_toggle::widget(monitor),
            widgets::workspaces::widget(monitor),
            widgets::window_list::widget(monitor),
        ])
        .center([widgets::mpris::widget(monitor)])
        .right([
            group([widgets::tray::widget(monitor)]),
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

    // Install the drawer *after* the bar so its window exists to be measured
    // for the perpendicular (bar-thickness) margin at open time.
    modal::install(monitor, &bar, BAR_EDGE, BAR_EDGE_OFFSET);

    // When the drawer is open on this monitor, mark the bar window so CSS
    // can square off the bottom-right corner (seam between bar and drawer).
    bind_class(
        modal::drawer_open_signal(monitor),
        bar.window(),
        "drawer-open",
    );

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
