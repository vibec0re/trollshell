//! Keep-awake bar chip — glanceable indicator for the caffeine hold from
//! #270/#520, visible only while it's engaged (#521, deferred out of #520
//! because it needed a bar-layout edit plus an icon pick).
//!
//! Mirrors `widgets/vpn.rs`'s hide-when-inactive shape: a `chip::indicator`
//! hidden via `bind_visible` on the authoritative `screensaver::keep_awake()`
//! signal (the same signal the Settings-drawer switch binds to), so the chip
//! and the switch can never disagree. A click opens `Page::Settings` — the
//! toggle's home since #520 (the Power-drawer copy is a mirror, not the
//! canonical one).
//!
//! Icon: `preferences-desktop-screensaver-symbolic`. The issue explicitly
//! ruled out a coffee-cup glyph (Adwaita has none) and offered this or
//! `night-light-disabled-symbolic` as candidates; the screensaver glyph reads
//! more directly as "the screensaver/idle-lock is being held off" than the
//! night-light one, which is about the blue-light filter, an unrelated
//! feature.

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::screensaver;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn =
        crate::components::chip::indicator("ts-keep-awake", crate::modal::Page::Settings, monitor);

    let icon = gtk::Image::from_icon_name("preferences-desktop-screensaver-symbolic");
    btn.set_child(Some(&icon));

    bind_visible(screensaver::keep_awake(), &btn);

    // Tooltip reuses the Settings/Power drawer's "Also awake: …" subtitle
    // helper, so the external-inhibitor list reads identically everywhere it
    // shows up rather than growing a second copy of the wording.
    bind(screensaver::other_inhibitors(), &btn, |b, others| {
        let subtitle = crate::panels::power::keep_awake_subtitle(&others);
        b.set_tooltip_text(Some(&format!("Keep awake — {subtitle}")));
    });

    btn.upcast()
}
