//! `trollshell-control-center` — the external, launch-on-demand settings &
//! management companion app for trollshell (#381; walking skeleton from #390).
//!
//! Modelled on gnome-control-center: the shell stays the lean always-on bar +
//! overlays, and heavier management UI lives here in a **separate windowed**
//! GTK4 + libadwaita app that talks to the running shell over D-Bus. It is
//! **never linked into the shell** — it only dials the shell's
//! `mov.vibec0re.trollshell.Control` session-bus endpoint (see the shell's
//! `control.rs`).
//!
//! This is the skeleton: an `adw::ViewStack` with four placeholder tabs
//! (Plugins · Place · AI Keys · Display) — each a "coming soon" `StatusPage`
//! stub — and a banner that reports whether the shell answered `Ping`/`Version`.
//! Each tab is a follow-up issue (#391 / #392 / #393 / #348), all blocked on
//! this landing. When the shell isn't running the app degrades gracefully to a
//! "not running" banner rather than panicking.

use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use hytte_bus::RetryPolicy;

/// Distinct app-id — this is its own application, not the shell.
const APP_ID: &str = "mov.vibec0re.trollshell.ControlCenter";
/// The shell's dedicated control endpoint (owned by the shell's `control.rs`).
const CONTROL_NAME: &str = "mov.vibec0re.trollshell.Control";
const CONTROL_PATH: &str = "/mov/vibec0re/trollshell/Control";
const CONTROL_IFACE: &str = "mov.vibec0re.trollshell.Control";

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt::init();

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_window);
    app.run()
}

/// Build the main window: a view-switcher over the four placeholder tabs plus a
/// connection-status banner, then kick off the async shell probe.
fn build_window(app: &adw::Application) {
    let stack = adw::ViewStack::new();
    add_placeholder(
        &stack,
        "plugins",
        "Plugins",
        "application-x-addon-symbolic",
        "Enable, disable, and configure widget plugins. Coming soon (#391).",
    );
    add_placeholder(
        &stack,
        "place",
        "Place",
        "mark-location-symbolic",
        "Manage your location for the weather and departures stack. Coming soon (#392).",
    );
    add_placeholder(
        &stack,
        "ai-keys",
        "AI Keys",
        "dialog-password-symbolic",
        "Store and rotate API keys for the LLM-backed plugins. Coming soon (#393).",
    );
    add_placeholder(
        &stack,
        "display",
        "Display",
        "video-display-symbolic",
        "Arrange monitors, resolution, and scaling. Coming soon (#348).",
    );

    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    let header = adw::HeaderBar::builder().title_widget(&switcher).build();

    let banner = adw::Banner::builder()
        .title("Connecting to trollshell…")
        .revealed(true)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&stack));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("trollshell Control Center")
        .default_width(760)
        .default_height(560)
        .content(&toolbar)
        .build();

    check_shell_connection(&banner);
    window.present();
}

/// Add a "coming soon" placeholder tab to `stack`.
fn add_placeholder(stack: &adw::ViewStack, name: &str, title: &str, icon: &str, description: &str) {
    let page = adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .description(description)
        .build();
    stack.add_titled_with_icon(&page, Some(name), title, icon);
}

/// Probe the running shell's control endpoint on the shared tokio runtime, then
/// update `banner` back on the GTK main thread with the result. Never blocks the
/// UI and never panics when the shell is absent.
fn check_shell_connection(banner: &adw::Banner) {
    let (tx, rx) = tokio::sync::oneshot::channel();

    // The D-Bus call runs on the process-wide hytte tokio runtime; the reply is
    // carried back over a oneshot the GTK main loop awaits below. Awaiting a
    // tokio oneshot receiver needs no runtime context, so it polls cleanly on
    // glib's executor.
    hytte_reactive::runtime::handle().spawn(async move {
        // The receiver is dropped if the window closed first — ignore the send
        // error in that case.
        let _ = tx.send(probe_shell().await);
    });

    let banner = banner.clone();
    glib::spawn_future_local(async move {
        match rx.await {
            Ok(Ok((pong, version))) => {
                banner.set_title(&format!(
                    "Connected to trollshell {version} (Ping → {pong})"
                ));
            }
            Ok(Err(err)) => {
                tracing::info!(%err, "trollshell control endpoint unreachable");
                banner.set_title("trollshell is not running — start the shell to manage it");
            }
            Err(_) => {
                // Sender dropped without sending (task cancelled) — unreachable
                // in practice, but degrade to the disconnected message.
                banner.set_title("Could not reach trollshell");
            }
        }
        banner.set_revealed(true);
    });
}

/// Call `Ping` then `Version` on the shell's control interface. Returns the
/// `(pong, version)` pair, or the first `BusError` (e.g. the shell isn't
/// running, so the name has no owner).
async fn probe_shell() -> Result<(String, String), hytte_bus::BusError> {
    let pong = control_call("Ping").await?;
    let version = control_call("Version").await?;
    Ok((pong, version))
}

/// One typed String-returning method call against the control interface. Short
/// timeout + no retry: the companion is interactive, so a missing shell should
/// resolve to "not running" quickly rather than hang the banner.
async fn control_call(method: &str) -> Result<String, hytte_bus::BusError> {
    hytte_bus::call(CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method(method)
        .timeout(Duration::from_secs(3))
        .retry(RetryPolicy::Never)
        .send::<String>()
        .await
}
