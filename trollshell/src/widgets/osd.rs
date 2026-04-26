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
//! Multi-monitor: `install` mounts one layer-shell window per monitor,
//! keyed by `Monitor.connector()`. Module-level signal subscriptions
//! run exactly once across all mounts; `route_show` picks the OSD on
//! niri's focused output, falling back to the first mounted OSD when
//! the focused output is unknown or missing from the map.
//!
//! CSS hooks (intentionally bar-prefixed `ts-`):
//! - window root: `.ts-osd`
//! - inner card: `.ts-osd-card`
//! - header (icon + label/value column): `.ts-osd-header`
//! - icon: `.ts-osd-icon`
//! - kind label: `.ts-osd-label`
//! - value readout: `.ts-osd-value`
//! - progress bar: `.ts-osd-progress`
//! - kind modifier: `.volume`, `.mic`, `.brightness`
//! - state modifier: `.muted` (when applicable)
//! - shown modifier: `.shown` (toggled by Rust to drive CSS transitions)

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use hytte::futures_signals::map_ref;
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::brightness::{self, Brightness};
use hytte::services::niri;
use hytte::services::pipewire::{self, Source, Volume};
use hytte::ui::layer_window;

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
/// progress fraction, label/value text, and kind/muted CSS classes
/// in place.
struct OsdView {
    window: gtk::Window,
    card: gtk::Box,
    icon: gtk::Image,
    label: gtk::Label,        // kind name: "Volume" / "Microphone" / "Brightness"
    value: gtk::Label,        // percent / "Muted"
    progress: gtk::ProgressBar,
    /// Pending hide timeout. Held so each new event can cancel and
    /// re-arm it (latest-wins debounce).
    timeout: Cell<Option<glib::SourceId>>,
    /// Pending fade-out → `set_visible(false)` timeout. Cancelled if a
    /// new event arrives mid-fade-out.
    fade_out_timeout: Cell<Option<glib::SourceId>>,
    /// CSS modifier classes currently set on the card so we can clean
    /// them up on each update without growing a leaky class list.
    current_kind: Cell<Option<&'static str>>,
    current_muted: Cell<bool>,
}

/// Mount one OSD on `monitor` and lazily wire the module-level signal
/// subscriptions (volume / mic / brightness / focused-output) on the
/// first call. Subsequent calls only insert into the per-monitor map;
/// subscriptions stay singletons.
pub fn install(monitor: &Monitor) {
    let connector = match monitor.connector() {
        Some(c) if !c.is_empty() => c,
        _ => {
            tracing::debug!("osd::install: monitor has no connector name; skipping");
            return;
        }
    };

    let view = build_osd_view(monitor);
    OSDS.with(|map| {
        map.borrow_mut().insert(connector, view);
    });

    if !SUBS_INSTALLED.with(Cell::get) {
        SUBS_INSTALLED.with(|c| c.set(true));
        install_subscriptions();
    }
}

/// Construct one `OsdView` for `monitor`. Pure widget construction —
/// signal wiring lives in [`install_subscriptions`] and runs once
/// regardless of monitor count.
#[allow(clippy::too_many_lines)]
fn build_osd_view(monitor: &Monitor) -> Rc<OsdView> {
    let window = layer_window(monitor)
        .layer(Layer::Overlay)
        .anchor(Anchor::Top)
        .margin(Margin {
            top: TOP_MARGIN,
            ..Margin::default()
        })
        .namespace("hytte-osd")
        .exclusive(false)
        .keyboard_mode(KeyboardMode::None)
        .build();
    window.add_css_class("ts-osd");
    window.set_visible(false);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.add_css_class("ts-osd-card");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    header.add_css_class("ts-osd-header");

    let icon = gtk::Image::new();
    icon.add_css_class("ts-osd-icon");
    icon.set_pixel_size(32);
    header.append(&icon);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
    column.set_hexpand(true);
    let label = gtk::Label::new(None);
    label.add_css_class("ts-osd-label");
    label.set_xalign(0.0);
    column.append(&label);
    let value = gtk::Label::new(None);
    value.add_css_class("ts-osd-value");
    value.set_xalign(0.0);
    column.append(&value);
    header.append(&column);

    card.append(&header);

    let progress = gtk::ProgressBar::new();
    progress.add_css_class("ts-osd-progress");
    card.append(&progress);

    window.set_child(Some(&card));

    Rc::new(OsdView {
        window,
        card,
        icon,
        label,
        value,
        progress,
        timeout: Cell::new(None),
        fade_out_timeout: Cell::new(None),
        current_kind: Cell::new(None),
        current_muted: Cell::new(false),
    })
}

/// Wire the four module-level signal subscriptions exactly once on the
/// first [`install`] call, regardless of monitor count.
///
/// Bootstrap suppression: each signal's first emission carries the
/// snapshot at install time. We don't want the OSD flashing on
/// startup, so silently consume that first event per signal.
fn install_subscriptions() {
    // ── Volume ────────────────────────────────────────────────────────
    {
        let first = Cell::new(true);
        glib::MainContext::default().spawn_local(
            pipewire::default_sink().for_each(move |v: Volume| {
                if first.replace(false) {
                    return std::future::ready(());
                }
                let state = render_volume(v);
                route_show(&state);
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
                    route_show(&state);
                }
                std::future::ready(())
            },
        ));
    }

    // ── Brightness ────────────────────────────────────────────────────
    {
        let first = Cell::new(true);
        glib::MainContext::default().spawn_local(brightness::current().for_each(
            move |b: Option<Brightness>| {
                if first.replace(false) {
                    return std::future::ready(());
                }
                if let Some(b) = b {
                    let state = render_brightness(b);
                    route_show(&state);
                }
                std::future::ready(())
            },
        ));
    }

    // ── Focused output ────────────────────────────────────────────────
    //
    // Updates `FOCUSED_OUTPUT` used by `route_show`. No bootstrap
    // suppression: we want the latest known focused output even before
    // any media/brightness event lands.
    glib::MainContext::default().spawn_local(niri::focused_output().for_each(|out| {
        FOCUSED_OUTPUT.with(|c| *c.borrow_mut() = out);
        std::future::ready(())
    }));
}

/// Route `state` to the OSD on the focused monitor. Falls back to the
/// first mounted OSD when the focused output is unknown or not in the
/// map (e.g. niri startup, monitor disconnect).
fn route_show(state: &State) {
    let target_name: Option<String> = FOCUSED_OUTPUT.with(|c| c.borrow().clone());
    OSDS.with(|map| {
        let map = map.borrow();
        if map.is_empty() {
            return;
        }
        let view = target_name
            .as_ref()
            .and_then(|name| map.get(name))
            .or_else(|| map.values().next());
        if let Some(view) = view {
            show(view, state);
        }
    });
}

thread_local! {
    /// Mounted OSD instances keyed by `gtk::Monitor.connector()`.
    /// `connector()` matches niri's `Workspace.output` (KMS connector
    /// names like `"DP-1"`, `"eDP-1"`).
    static OSDS: RefCell<HashMap<String, Rc<OsdView>>> =
        RefCell::new(HashMap::new());

    /// Most recent focused-output name from
    /// [`hytte::services::niri::focused_output`]. Updated by the
    /// module-level subscription.
    static FOCUSED_OUTPUT: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Set after the first `install()` call to ensure module-level
    /// signal subscriptions are wired exactly once across all
    /// per-monitor mounts.
    static SUBS_INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Rendered OSD state — what to display once a signal fires.
struct State {
    kind: Kind,
    icon: &'static str,
    fraction: f64,
    /// Kind name shown in `.ts-osd-label` ("Volume" / "Microphone" /
    /// "Brightness").
    label: &'static str,
    /// Percent / "Muted" text shown in `.ts-osd-value`.
    value: String,
    muted: bool,
}

fn volume_icon(v: &Volume) -> &'static str {
    if v.muted {
        return "audio-volume-muted-symbolic";
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct = (clamp01(v.linear) * 100.0).round() as u32;
    match pct {
        0..=33 => "audio-volume-low-symbolic",
        34..=66 => "audio-volume-medium-symbolic",
        _ => "audio-volume-high-symbolic",
    }
}

fn mic_icon(s: &Source) -> &'static str {
    if s.muted {
        return "microphone-sensitivity-muted-symbolic";
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct = (clamp01(s.volume) * 100.0).round() as u32;
    if pct >= 50 {
        "microphone-sensitivity-high-symbolic"
    } else {
        "microphone-sensitivity-medium-symbolic"
    }
}

fn render_volume(v: Volume) -> State {
    let icon = volume_icon(&v);
    // Boosted volume can exceed 100%; show the true value in the label
    // while still clamping the progress bar to [0.0, 1.0].
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct = (v.linear * 100.0).round() as u32;
    let value = if v.muted {
        "Muted".to_string()
    } else {
        format!("{pct}%")
    };
    State {
        kind: Kind::Volume,
        icon,
        fraction: clamp01(v.linear),
        label: "Volume",
        value,
        muted: v.muted,
    }
}

fn render_mic(source: Option<&Source>) -> Option<State> {
    let s = source?;
    let icon = mic_icon(s);
    let pct = pct(s.volume);
    let value = if s.muted {
        "Muted".to_string()
    } else {
        format!("{pct}%")
    };
    Some(State {
        kind: Kind::Mic,
        icon,
        fraction: clamp01(s.volume),
        label: "Microphone",
        value,
        muted: s.muted,
    })
}

fn render_brightness(b: Brightness) -> State {
    let pct = pct(b.level);
    State {
        kind: Kind::Brightness,
        icon: "display-brightness-symbolic",
        fraction: clamp01(b.level),
        label: "Brightness",
        value: format!("{pct}%"),
        muted: false,
    }
}

/// Populate the OSD view with `state`, mark it visible, and arm the
/// auto-hide timeout. If a previous timeout was still pending, cancel
/// it so the OSD stays visible for another full `HIDE_AFTER_MS`.
fn show(view: &Rc<OsdView>, state: &State) {
    view.icon.set_icon_name(Some(state.icon));
    view.progress.set_fraction(state.fraction);
    view.label.set_text(state.label);
    view.value.set_text(&state.value);

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

    // Make the window visible, then defer the .shown class flip by one
    // frame so GTK4's CSS engine sees the transition's "from" state
    // (opacity 0, margin-top 0) before the "to" state. Without the
    // idle defer, the same-frame visibility-and-class flip skips the
    // transition entirely.
    //
    // Cancel any in-flight fade-out: a new event landed before the
    // previous fade-out timer fired, so we keep the window visible
    // and re-arm.
    if let Some(prev) = view.fade_out_timeout.take() {
        prev.remove();
    }
    view.window.set_visible(true);
    let card_for_idle = view.card.clone();
    glib::idle_add_local_once(move || {
        card_for_idle.add_css_class("shown");
    });

    // Reset auto-hide. When the timer fires we DON'T set_visible(false)
    // directly; instead remove the .shown class to start the CSS fade,
    // then schedule a second timer to flip visibility once the
    // transition has finished.
    if let Some(prev) = view.timeout.take() {
        prev.remove();
    }
    let view_for_timeout = view.clone();
    let id = glib::timeout_add_local_once(
        Duration::from_millis(u64::from(HIDE_AFTER_MS)),
        move || {
            view_for_timeout.timeout.set(None);
            view_for_timeout.card.remove_css_class("shown");
            // Wait for the 200ms CSS transition + 20ms safety buffer
            // before actually hiding the layer-shell window.
            let view_for_fade = view_for_timeout.clone();
            let fade_id = glib::timeout_add_local_once(
                Duration::from_millis(220),
                move || {
                    view_for_fade.fade_out_timeout.set(None);
                    view_for_fade.window.set_visible(false);
                },
            );
            view_for_timeout.fade_out_timeout.set(Some(fade_id));
        },
    );
    view.timeout.set(Some(id));
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn pct(linear: f64) -> u32 {
    (clamp01(linear) * 100.0).round() as u32
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hytte::services::pipewire::{Source, Volume};

    fn vol(linear: f64, muted: bool) -> Volume {
        Volume { linear, muted }
    }

    fn src(volume: f64, muted: bool) -> Source {
        Source {
            id: 0,
            name: String::new(),
            description: String::new(),
            volume,
            muted,
            is_default: false,
        }
    }

    #[test]
    fn volume_icon_muted() {
        assert_eq!(volume_icon(&vol(0.5, true)), "audio-volume-muted-symbolic");
    }

    #[test]
    fn volume_icon_low_band() {
        assert_eq!(volume_icon(&vol(0.0, false)), "audio-volume-low-symbolic");
        assert_eq!(volume_icon(&vol(0.33, false)), "audio-volume-low-symbolic");
    }

    #[test]
    fn volume_icon_medium_band() {
        assert_eq!(volume_icon(&vol(0.5, false)), "audio-volume-medium-symbolic");
        assert_eq!(volume_icon(&vol(0.66, false)), "audio-volume-medium-symbolic");
    }

    #[test]
    fn volume_icon_high_band() {
        assert_eq!(volume_icon(&vol(0.67, false)), "audio-volume-high-symbolic");
        assert_eq!(volume_icon(&vol(1.0, false)), "audio-volume-high-symbolic");
    }

    #[test]
    fn mic_icon_muted() {
        assert_eq!(mic_icon(&src(0.5, true)), "microphone-sensitivity-muted-symbolic");
    }

    #[test]
    fn mic_icon_high_band() {
        assert_eq!(mic_icon(&src(0.5, false)), "microphone-sensitivity-high-symbolic");
        assert_eq!(mic_icon(&src(1.0, false)), "microphone-sensitivity-high-symbolic");
    }

    #[test]
    fn mic_icon_medium_band() {
        assert_eq!(mic_icon(&src(0.0, false)), "microphone-sensitivity-medium-symbolic");
        assert_eq!(mic_icon(&src(0.49, false)), "microphone-sensitivity-medium-symbolic");
    }
}
