mod widgets;

use hytte::prelude::*;
use hytte::services::{clock, networkd, niri, pipewire, resolved, upower};

fn main() -> hytte::ui::Result<()> {
    tracing_subscriber::fmt::init();

    App::new("cc.hannig.trollshell")
        .with(clock::service())
        .with(niri::service())
        .with(upower::service())
        .with(pipewire::service())
        .with(networkd::service())
        .with(resolved::service())
        .with_user_style(concat!(env!("CARGO_MANIFEST_DIR"), "/style.css"))
        .run(|app| {
            for monitor in app.monitors() {
                Bar::new(&monitor)
                    .edge(Edge::Top)
                    .exclusive(true)
                    .keyboard_interactivity(KeyboardMode::OnDemand)
                    .left([widgets::workspaces::widget(&monitor)])
                    .right([
                        widgets::network::widget(),
                        widgets::volume::widget(),
                        widgets::battery::widget(),
                        widgets::clock::widget(),
                    ])
                    .show()
                    .into_long_lived();
            }
        })
}
