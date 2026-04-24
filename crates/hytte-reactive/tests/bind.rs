//! Integration test: drive a `Mutable<String>` from the GTK main loop and
//! assert the bound `gtk::Label`'s text follows.
//!
//! Requires a display server. Run with `xvfb-run cargo test -p hytte-reactive
//! --test bind -- --ignored` or under an existing X/Wayland session.

use futures_signals::signal::Mutable;
use gtk::glib;
use gtk::prelude::*;
use hytte_reactive::bind::{bind_text, bind_visible};
use std::time::Duration;

fn run_briefly(ms: u64) {
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + Duration::from_millis(ms);
    while std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[gtk::test]
#[ignore = "requires a display server"]
fn bind_text_follows_mutable_updates() {
    let label = gtk::Label::new(None);
    let m = Mutable::new(String::from("hello"));

    bind_text(m.signal_cloned(), &label);
    run_briefly(50);
    assert_eq!(label.text().as_str(), "hello");

    m.set(String::from("world"));
    run_briefly(50);
    assert_eq!(label.text().as_str(), "world");
}

#[gtk::test]
#[ignore = "requires a display server"]
fn bind_visible_toggles_widget() {
    let label = gtk::Label::new(Some("x"));
    let m = Mutable::new(false);

    bind_visible(m.signal(), &label);
    run_briefly(50);
    assert!(!label.is_visible());

    m.set(true);
    run_briefly(50);
    assert!(label.is_visible());
}
