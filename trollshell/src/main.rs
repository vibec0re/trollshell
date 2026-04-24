mod widgets;

use hytte::prelude::*;
use hytte::services::{clock, niri};

fn main() -> hytte::ui::Result<()> {
    tracing_subscriber::fmt::init();

    App::new("cc.hannig.trollshell")
        .with(clock::service())
        .with(niri::service())
        .with_user_style(concat!(env!("CARGO_MANIFEST_DIR"), "/style.css"))
        .run(|app| {
            for monitor in app.monitors() {
                Bar::new(&monitor)
                    .edge(Edge::Top)
                    .exclusive(true)
                    .left([widgets::workspaces::widget(&monitor)])
                    .right([widgets::clock::widget()])
                    .show()
                    // Leak the handle: bars live for the app lifetime.
                    .into_long_lived();
            }
        })
}
