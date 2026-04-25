//! Transient on-screen-display popup for volume, microphone, and brightness
//! changes.
//!
//! When the user pokes a media or brightness key (the actual keybinds land
//! in #31), the relevant service signal emits a new value. This widget owns
//! one layer-shell window pinned Top-Center on the primary monitor and
//! pops it up briefly to show the new state, then auto-hides after 1.5s.
//!
//! Latest signal wins: if volume changes and brightness changes 200ms
//! later, the OSD switches kind immediately and resets its hide timer.
//!
//! Bootstrap suppression: each of the three subscribed signals fires
//! immediately on install with the service's current snapshot. Without
//! suppression, opening the laptop with the bar starting up would flash
//! three OSDs in a row. We track one `Cell<bool>` per signal and silently
//! consume the FIRST emission; subsequent emissions show the OSD.
//!
//! Mounted on the primary monitor only for v1 — multi-monitor follow-up
//! is a tiny loop in main.rs (see #30 follow-ups).
//!
//! CSS hooks (intentionally bar-prefixed `ts-`):
//! - window root: `.ts-osd`
//! - inner card: `.ts-osd-card`
//! - icon: `.ts-osd-icon`
//! - progress bar: `.ts-osd-progress`
//! - text label: `.ts-osd-text`
//! - kind modifier: `.volume`, `.mic`, `.brightness`
//! - state modifier: `.muted` (when applicable)

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use hytte::futures_signals::map_ref;
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::brightness::{self, Brightness};
use hytte::services::pipewire::{self, Source, Volume};
use hytte::ui::{layer_window, Anchor, Margin};

/// How long the OSD stays visible after the latest event, in ms. Each
/// new event resets the timer.
const HIDE_AFTER_MS: u32 = 1500;

/// Distance from top of screen so the OSD doesn't sit flush against
/// the bar.
const TOP_MARGIN: i32 = 80;

#[derive(Clone, Copy, Debug)]
enum Kind {
    Volume,
    Mic,
    Brightness,
}

impl Kind {
    fn css_class(self) -> &'static str {
        match self {
            Self::Volume => "volume",
            Self::Mic => "mic",
            Self::Brightness => "brightness",
        }
    }
}

/// Mutable widgets owned by the OSD; rebuilt content swaps icon name,
/// progress fraction, label text, and kind/muted CSS classes in place.
struct OsdView {
    window: gtk::Window,
    card: gtk::Box,
    icon: gtk::Image,
    progress: gtk::ProgressBar,
    text: gtk::Label,
    /// Pending hide timeout. Held so each new event can cancel and
    /// re-arm it (latest-wins debounce).
    timeout: Cell<Option<glib::SourceId>>,
    /// CSS modifier classes currently set on the card so we can clean
    /// them up on each update without growing a leaky class list.
    current_kind: Cell<Option<&'static str>>,
    current_muted: Cell<bool>,
}

/// Build the OSD layer-shell window for `monitor`, subscribe it to the
/// three signals, and store it in a thread-local so it lives for the
/// process lifetime.
pub fn install(monitor: &Monitor) {
    let window = layer_window(monitor)
        .layer(Layer::Overlay)
        .anchor(Anchor::Top)
        .margin(Margin {
            top: TOP_MARGIN,
            ..Margin::default()
        })
        .namespace("trollshell-osd")
        .exclusive(false)
        .keyboard_mode(KeyboardMode::None)
        .build();
    window.add_css_class("ts-osd");
    window.set_visible(false);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    card.add_css_class("ts-osd-card");

    let icon = gtk::Image::new();
    icon.add_css_class("ts-osd-icon");
    icon.set_pixel_size(48);

    let progress = gtk::ProgressBar::new();
    progress.add_css_class("ts-osd-progress");

    let text = gtk::Label::new(None);
    text.add_css_class("ts-osd-text");

    card.append(&icon);
    card.append(&progress);
    card.append(&text);
    window.set_child(Some(&card));

    let view = Rc::new(OsdView {
        window,
        card,
        icon,
        progress,
        text,
        timeout: Cell::new(None),
        current_kind: Cell::new(None),
        current_muted: Cell::new(false),
    });

    // Bootstrap suppression: each signal's first emission carries the
    // snapshot at install time. We don't want the OSD flashing on
    // startup, so silently consume that first event per signal.

    // ── Volume ────────────────────────────────────────────────────────
    {
        let view = view.clone();
        let first = Cell::new(true);
        glib::MainContext::default().spawn_local(
            pipewire::default_sink().for_each(move |v: Volume| {
                if first.replace(false) {
                    return std::future::ready(());
                }
                show(&view, &render_volume(v));
                std::future::ready(())
            }),
        );
    }

    // ── Microphone (default source) ───────────────────────────────────
    //
    // No `default_source()` signal is exposed, so derive it locally by
    // looking for `is_default=true` in `sources()`. Wrapped in
    // `dedupe_cloned` so identical snapshots don't re-fire after
    // unrelated source list changes (e.g. a USB mic plug event when
    // the system mic is still the default).
    {
        let view = view.clone();
        let first = Cell::new(true);
        let combined = map_ref! {
            let sources = pipewire::sources() => {
                sources.iter().find(|s| s.is_default).cloned()
            }
        }
        .dedupe_cloned();

        glib::MainContext::default().spawn_local(combined.for_each(
            move |source: Option<Source>| {
                if first.replace(false) {
                    return std::future::ready(());
                }
                if let Some(state) = render_mic(source.as_ref()) {
                    show(&view, &state);
                }
                std::future::ready(())
            },
        ));
    }

    // ── Brightness ────────────────────────────────────────────────────
    {
        let view = view.clone();
        let first = Cell::new(true);
        glib::MainContext::default().spawn_local(brightness::current().for_each(
            move |b: Option<Brightness>| {
                if first.replace(false) {
                    return std::future::ready(());
                }
                if let Some(b) = b {
                    show(&view, &render_brightness(b));
                }
                std::future::ready(())
            },
        ));
    }

    OSD_VIEW.with(|cell| {
        *cell.borrow_mut() = Some(view);
    });
}

thread_local! {
    /// Holds the OSD `Rc` for the process lifetime so the layer-shell
    /// window and its signal futures aren't dropped.
    static OSD_VIEW: RefCell<Option<Rc<OsdView>>> = const { RefCell::new(None) };
}

/// Rendered OSD state — what to display once a signal fires.
struct State {
    kind: Kind,
    icon: &'static str,
    fraction: f64,
    text: String,
    muted: bool,
}

fn render_volume(v: Volume) -> State {
    let icon = if v.muted {
        "audio-volume-muted-symbolic"
    } else {
        "audio-volume-high-symbolic"
    };
    let pct = pct(v.linear);
    let text = if v.muted {
        "Volume muted".to_string()
    } else {
        format!("Volume {pct}%")
    };
    State {
        kind: Kind::Volume,
        icon,
        fraction: clamp01(v.linear),
        text,
        muted: v.muted,
    }
}

fn render_mic(source: Option<&Source>) -> Option<State> {
    let s = source?;
    let icon = "audio-input-microphone-symbolic";
    let pct = pct(s.volume);
    let text = if s.muted {
        "Microphone muted".to_string()
    } else {
        format!("Microphone {pct}%")
    };
    Some(State {
        kind: Kind::Mic,
        icon,
        fraction: clamp01(s.volume),
        text,
        muted: s.muted,
    })
}

fn render_brightness(b: Brightness) -> State {
    let pct = pct(b.level);
    State {
        kind: Kind::Brightness,
        icon: "display-brightness-symbolic",
        fraction: clamp01(b.level),
        text: format!("Brightness {pct}%"),
        muted: false,
    }
}

/// Populate the OSD view with `state`, mark it visible, and arm the
/// auto-hide timeout. If a previous timeout was still pending, cancel
/// it so the OSD stays visible for another full `HIDE_AFTER_MS`.
fn show(view: &Rc<OsdView>, state: &State) {
    view.icon.set_icon_name(Some(state.icon));
    view.progress.set_fraction(state.fraction);
    view.text.set_text(&state.text);

    // Swap kind modifier class.
    let new_kind = state.kind.css_class();
    if let Some(old) = view.current_kind.replace(Some(new_kind))
        && old != new_kind
    {
        view.card.remove_css_class(old);
    }
    view.card.add_css_class(new_kind);

    // Toggle muted modifier.
    let was_muted = view.current_muted.replace(state.muted);
    if state.muted && !was_muted {
        view.card.add_css_class("muted");
    } else if !state.muted && was_muted {
        view.card.remove_css_class("muted");
    }

    view.window.set_visible(true);

    // Reset auto-hide.
    if let Some(prev) = view.timeout.take() {
        prev.remove();
    }
    let view_for_timeout = view.clone();
    let id = glib::timeout_add_local_once(Duration::from_millis(u64::from(HIDE_AFTER_MS)), move || {
        view_for_timeout.timeout.set(None);
        view_for_timeout.window.set_visible(false);
    });
    view.timeout.set(Some(id));
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn pct(linear: f64) -> u32 {
    (clamp01(linear) * 100.0).round() as u32
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}
