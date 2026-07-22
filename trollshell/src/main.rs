mod assets;
mod commands;
mod components;
mod control;
mod modal;
mod overlays;
mod panels;
mod plugins;
mod scale;
mod widgets;

use std::cell::RefCell;

use hytte::futures_signals::signal::SignalExt;
use hytte::gtk;
use hytte::gtk::{gdk, gio, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::{
    app_usage, bluetooth, bluetooth_audio, brightness, calendar, clipboard, clock, departures,
    displays, dnd, geoclue, idle_notify, mpris, netconn, networkd, nightlight, niri, notifications,
    notifications_mute, pipewire, places, power_profiles, resolved, screensaver, sensors, systemd,
    tasks, tray, upower, vpn, wallpaper, weather, wifi, wifiscan,
};

// The service-registration builder chain + body closure make `main` one long
// flat sequence; registering the control service (#390) tips it past the
// 100-line pedantic limit. Splitting it would only obscure the linear wiring.
#[allow(clippy::too_many_lines)]
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

    App::new("mov.vibec0re.trollshell")
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
        // Native ext-idle-notify-v1 observer (#204 Phase 2, observe-only): runs
        // alongside swayidle to validate idle-timing parity. Takes no action.
        .with(idle_notify::service())
        .with(systemd::service())
        .with(wallpaper::service())
        // Night-light (color temperature): toggles a zero-state wlsunset user
        // unit — sits by wallpaper as an appearance concern (Appearance panel).
        .with(nightlight::service())
        .with(displays::service())
        .with(clipboard::service())
        .with(calendar::service())
        .with(tasks::service())
        // Out-of-process widget-plugin host transport (#35 PR 2). Listens on a
        // per-user socket; plugins dial in as systemd user units. The GTK-side
        // halves (clock pump, effect broker) are wired via `plugins::install()`
        // below; the sidebar reconciler slots mount in `build_card`.
        .with(plugins::service())
        // External control-center transport (#390): owns the dedicated
        // `mov.vibec0re.trollshell.Control` name + Ping/Version interface — see
        // control.rs (companion app: trollshell-control-center).
        .with(control::service())
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

            // Inject the CSS base `font-size` from Rust so every CSS `em`
            // rides the same scale factor as `scale::scale()` — one knob
            // rescales the whole shell, CSS text and Rust-set sizes together
            // (#135 part 2). Must run after GTK is initialized (a display +
            // gtk::Settings exist here).
            install_scaled_base_font();

            // Register the command surface (GActions on the app's already-owned
            // bus name) so niri keybinds can drive the drawer / power menu /
            // sidebar over org.gtk.Actions — see commands.rs and #219.
            commands::install(app);

            // Spawn a task on the GTK main loop that owns the live set of
            // bars AND every per-monitor overlay. Each emission of
            // monitors_changed (initial + every hot-plug) tears down the old
            // surfaces and rebuilds for the current monitor set. Dropping a
            // BarHandle closes its window; the overlays are re-keyed by
            // connector via their `close_all` + `install` pair. Folding the
            // frame/toast/OSD/prompt overlays in here (rather than installing
            // them once at startup) is what makes a hot-plugged output get
            // toasts/OSD/frame and a vanished boot-time output stop swallowing
            // them into a dead surface (#225).
            let monitors_signal = app.monitors_changed();
            glib::MainContext::default().spawn_local(async move {
                let bars: RefCell<Vec<BarHandle>> = RefCell::new(Vec::new());
                monitors_signal
                    .for_each(move |monitors| {
                        // Tear down every per-monitor surface before rebuilding.
                        // Order mirrors install: bars/drawers/sidebar first, then
                        // the overlays. Each `close_all` drains its per-connector
                        // map (and aborts the raw per-monitor subscriptions the
                        // frame/OSD spawn), so nothing lingers when an output
                        // vanishes and the re-install below re-keys cleanly.
                        modal::close_all();
                        overlays::sidebar::close_all();
                        overlays::frame::close_all();
                        overlays::notifications::close_all();
                        overlays::osd::close_all();
                        overlays::prompt::close_all();

                        *bars.borrow_mut() = monitors.iter().map(build_bar).collect();

                        // Notifications + OSD + frame mount on every monitor;
                        // routing picks niri's focused output each emission.
                        for monitor in &monitors {
                            overlays::frame::install(monitor);
                            overlays::notifications::install(monitor);
                            overlays::osd::install(monitor);
                        }

                        // Password prompt overlay on the current primary output.
                        // Guard the zero-monitor / dead-first-output case — never
                        // index `.first()` unguarded. `wifi::active_prompt()`
                        // replays its current value on subscribe, so a prompt that
                        // was live when the previous primary vanished re-presents
                        // on the new one.
                        if let Some(primary) = monitors.first() {
                            overlays::prompt::install(primary);
                        }

                        std::future::ready(())
                    })
                    .await;
            });

            // Spawn the bluetooth-audio auto-switch reactor on the GTK main
            // loop. Must run after services are registered so it can pull
            // bluetooth + pipewire signals out of the registry.
            bluetooth_audio::init();

            // Wire the GTK-thread halves of the plugin host transport: the
            // clock→wire state pump and the (global) effect broker. Must run
            // after services are registered — it pulls the plugins + clock
            // handles out of the registry. The per-monitor reconciler slots
            // mount separately in `overlays::sidebar::build_card`.
            plugins::install();

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

            // Gate app_usage's always-on `/proc` walk on Stats-drawer
            // visibility (#50, item 5 of #42): it only feeds the Stats panel's
            // most-expensive-apps lists, so park it whenever that page isn't
            // on-screen. Same global-signal / single-subscription shape as the
            // netconn gate above.
            glib::MainContext::default().spawn_local(modal::stats_visible_signal().for_each(
                |visible| {
                    app_usage::set_active(visible);
                    std::future::ready(())
                },
            ));

            // Gate mpris's per-player 250ms `Position` pollers on Media-drawer
            // visibility (#228): it's the only consumer of `position_us`, so
            // park all the pollers whenever that page isn't on-screen. Same
            // global-signal / single-subscription shape as the netconn/
            // app_usage gates above.
            glib::MainContext::default().spawn_local(modal::media_visible_signal().for_each(
                |visible| {
                    mpris::set_active(visible);
                    std::future::ready(())
                },
            ));

            // Post a plain "Screenshot saved" toast whenever niri reports a
            // completed capture. Single global subscription — see
            // `install_screenshot_toast` for why.
            install_screenshot_toast();

            // The frame / notifications / OSD / prompt overlays are installed
            // (and re-installed on hot-plug) inside the monitors_changed loop
            // above, alongside the bars — see #225.
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
    // Plugin bar-chip regions (#349): one per bar group (left/center/right),
    // built like the sidebar's plugin regions but laid out horizontally. Each is
    // a reconciler-backed container that stays empty (and hidden) until a
    // `Bar*`-mounted plugin dials in, then shows its `view()` tree as a
    // `.ts-plugin-chip` pill. Parallel to `sidebar::build_card`'s sidebar slots.
    let bar = Bar::new(monitor)
        .edge(BAR_EDGE)
        .exclusive(true)
        .keyboard_interactivity(KeyboardMode::OnDemand)
        .left([
            widgets::sidebar_toggle::widget(monitor),
            widgets::workspaces::widget(monitor),
            widgets::window_list::widget(monitor),
            plugins::bar_left_slot(),
        ])
        .center([widgets::mpris::widget(monitor), plugins::bar_center_slot()])
        .right([
            plugins::bar_right_slot(),
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
                widgets::services::widget(monitor),
            ]),
            group([widgets::clock::widget(monitor)]),
            group([
                widgets::notif_indicator::widget(monitor),
                widgets::screenshot::widget(monitor),
                widgets::settings_chip::widget(monitor),
                widgets::power_chip::widget(monitor),
            ]),
            // Privacy indicator, kept in its own group so it doesn't shift
            // when other chips hide/show — see #221.
            group([widgets::screencast::widget(monitor)]),
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

/// Install the shell's base `font-size` on `*`, computed in Rust as
/// `scale::css_base_font_px()` (`13px * scale::scale_factor()`).
///
/// This is the single source of truth for the CSS `em` base (#135 part 2): the
/// static `* { font-size: 13px }` was removed from `style.css` in favour of
/// this so the base rides the *same* factor `scale::scale()` uses. At the
/// default font the factor is `1.0` → an exact `13px`, so **1× is
/// pixel-identical**; as the GTK font / GNOME text-scaling (carried via
/// `gtk-xft-dpi`) grows, CSS text and Rust-set sizes grow together with no
/// drift.
///
/// Added at `STYLE_PROVIDER_PRIORITY_USER` — the authority the user stylesheet
/// held when it carried the base rule — and *after* that provider is installed,
/// so it wins ties over the user sheet and, by priority, over the library
/// default's `.hytte-bar` font-size. Re-applied on `gtk-xft-dpi` /
/// `gtk-font-name` changes so a live text-scaling change rescales the shell
/// without a restart.
fn install_scaled_base_font() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    apply_scaled_base_font(&provider);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER,
    );

    // Live text-scaling: re-derive the base when the effective font changes.
    // `gtk-font-name` covers a point-size change; `gtk-xft-dpi` carries the
    // text-scaling-factor / DPI. The display keeps its own ref to the provider,
    // so loading fresh CSS into it here updates the whole shell in place.
    if let Some(settings) = gtk::Settings::default() {
        let p = provider.clone();
        settings.connect_gtk_xft_dpi_notify(move |_| apply_scaled_base_font(&p));
        settings.connect_gtk_font_name_notify(move |_| apply_scaled_base_font(&provider));
    }
}

/// Load `* { font-size: <css_base_font_px>px }` into `provider`. Shared by the
/// initial install and the settings-change handlers in
/// [`install_scaled_base_font`].
fn apply_scaled_base_font(provider: &gtk::CssProvider) {
    provider.load_from_string(&format!(
        "* {{ font-size: {:.4}px; }}",
        scale::css_base_font_px()
    ));
}

/// Post a "Screenshot saved" toast whenever niri reports a completed capture
/// (`Event::ScreenshotCaptured` → `niri::screenshot_captured()`), with
/// **Open** / **Copy** action buttons when a file path came back.
///
/// A single global subscription — not one per monitor/bar — so an
/// N-monitor setup doesn't fire N toasts for one screenshot; mirrors the
/// `netconn`/`app_usage` single-subscription shape in [`main`].
///
/// Actions dispatch locally via [`notifications::post_local_with_actions`]
/// (#220's deferred half — see that function and `invoke_action`'s "Local
/// dispatch" doc for the mechanism): clicking Open/Copy never touches
/// D-Bus, it runs [`open_screenshot`]/[`copy_screenshot`] directly on the
/// GTK main thread, same as the click that invoked it.
///
/// `shot.path` is `None` when niri only copied the capture to the
/// clipboard (no `write_to_disk`, or non-UTF-8 path) — there's no file to
/// Open or re-Copy in that case, so the toast carries no actions.
fn install_screenshot_toast() {
    glib::MainContext::default().spawn_local(niri::screenshot_captured().for_each(|shot| {
        if let Some(shot) = shot {
            match shot.path {
                Some(path) => {
                    let open_path = path.clone();
                    let copy_path = path.clone();
                    notifications::post_local_with_actions(
                        "Screenshot",
                        "Screenshot saved",
                        &path,
                        notifications::Urgency::Normal,
                        vec![
                            notifications::LocalAction::new("open", "Open", move || {
                                open_screenshot(&open_path);
                            }),
                            notifications::LocalAction::new("copy", "Copy", move || {
                                copy_screenshot(&copy_path);
                            }),
                        ],
                    );
                }
                None => {
                    notifications::post_local(
                        "Screenshot",
                        "Screenshot saved",
                        "clipboard only",
                        notifications::Urgency::Normal,
                    );
                }
            }
        }
        std::future::ready(())
    }));
}

/// Open a saved screenshot with the desktop's default handler for its file
/// type (typically an image viewer). Runs on the GTK main thread — see
/// [`notifications::LocalActionCallback`]'s thread contract; this is the
/// Open action's callback in [`install_screenshot_toast`].
///
/// `gio::AppInfo::launch_default_for_uri` over shelling out to `xdg-open`:
/// same desktop-portal-backed resolution, no subprocess. Failure (no
/// handler registered, launch error) is logged at warn — mirrors
/// `clipboard::paste_entry`'s fire-and-forget-with-warn idiom.
fn open_screenshot(path: &str) {
    let uri = gio::File::for_path(path).uri();
    if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE) {
        tracing::warn!(error = %e, path, "screenshot: failed to open with default handler");
    }
}

/// Copy a saved screenshot's image *contents* (not its path) to the
/// clipboard via `GdkClipboard::set_texture` — the same native-GTK
/// mechanism `overlays::notifications::build_image` uses to decode a
/// notification thumbnail, so no `wl-copy` subprocess is needed. Runs on
/// the GTK main thread; this is the Copy action's callback in
/// [`install_screenshot_toast`].
///
/// Niri may already have copied this same image to the clipboard as part
/// of the capture itself (screen/window screenshots do this by default);
/// re-copying identical image bytes here is a harmless no-op in that case,
/// unlike the path-string copy the #220 triage flagged as a potential
/// clobber — this copies the same content, not a different representation
/// of it.
fn copy_screenshot(path: &str) {
    let Some(display) = gdk::Display::default() else {
        tracing::warn!(
            path,
            "screenshot: no default GdkDisplay, cannot copy to clipboard"
        );
        return;
    };
    match gdk::Texture::from_filename(path) {
        Ok(texture) => display.clipboard().set_texture(&texture),
        Err(e) => {
            tracing::warn!(error = %e, path, "screenshot: failed to load image for clipboard copy");
        }
    }
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
