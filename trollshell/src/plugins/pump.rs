//! GTK-side state publishers: the clock / accent / audio-spectrum projections
//! [`super::install`] wires into `watch` channels, plus the slot-visibility
//! (#288) aggregation fed from `sidebar.rs`. Each `publish_*` writes a
//! [`super::PluginHandles`] `watch::Sender`; the per-conn tasks in
//! [`super::session`] subscribe the matching receiver.

use std::cell::RefCell;
use std::collections::HashMap;

use chrono::{DateTime, Local};
use hytte::futures_signals::signal::Mutable;
use hytte::gtk::{self, prelude::*};
use hytte::reactive::registry;
use hytte::services::calendar::CalendarEvent;
use hytte::services::mpris::{PlaybackStatus, Player};
use hytte::services::pipewire;
use hytte_plugin_proto::{
    AudioSpectrum, ClockState, MAX_UPCOMING_EVENTS, NowPlaying, UpcomingEvent,
};

use super::PluginHandles;

/// The upcoming-calendar digest window (#484): events starting within the next
/// 24 h. Paired with the [`MAX_UPCOMING_EVENTS`] cap, this is the "briefing-shaped
/// slice" the host projects off the full calendar service.
const CALENDAR_WINDOW_SECS: i64 = 24 * 3600;

/// Publish the latest clock state to the per-conn snapshot tasks.
pub(super) fn set_clock(cs: ClockState) {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .clock_tx
            .send_replace(Some(cs));
    });
}

/// Tint the shell's **own** in-process `hytte-preem` surfaces to the desktop
/// accent (#862).
///
/// Out-of-process plugins learn the accent from [`publish_accent`]'s watch
/// channel and apply it inside their own process. Since #857 the shell
/// rasterises preem surfaces too — the stats drawer's per-core LED panel — and
/// those live in *this* process, where the kit reads a process-global set by
/// `hytte_preem::set_accent`. Nothing was setting it, so a shell-side surface
/// asking for palette ink got the kit default instead of the session accent.
///
/// A separate function from [`publish_accent`] only so it is reachable from a
/// test without a registered `PluginHandles` — the same reason
/// [`super::wire_map::pixels_len_ok`] and friends are split out.
///
/// `hytte_preem::Rgba` **is** `[u8; 4]`, so the quad crosses unconverted; the
/// kit forces alpha opaque on its own (a preem frame is a screen).
pub(super) fn tint_in_process_surfaces(accent: Option<[u8; 4]>) {
    hytte_preem::set_accent(accent);
}

/// Publish the resolved desktop accent to the per-conn accent tasks (#376) and
/// to the shell's own preem surfaces (#862).
pub(super) fn publish_accent(accent: Option<[u8; 4]>) {
    // Before the channel send, so an in-process surface redrawn by a plugin
    // waking on the accent push already sees the new ink.
    tint_in_process_surfaces(accent);
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .accent_tx
            .send_replace(accent);
    });
}

/// Publish the latest audio spectrum to the per-conn spectrum tasks (#405).
pub(super) fn publish_spectrum(spectrum: Option<AudioSpectrum>) {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .spectrum_tx
            .send_replace(spectrum);
    });
}

/// Project a services [`pipewire::AudioSpectrum`] onto the GTK-free plugin-proto
/// [`AudioSpectrum`] the wire carries (field-for-field, #405).
pub(super) fn to_wire_spectrum(s: pipewire::AudioSpectrum) -> AudioSpectrum {
    AudioSpectrum {
        peak: s.peak,
        bins: s.bins,
    }
}

/// Publish the upcoming-calendar digest to the per-conn calendar tasks (#484).
/// Skips a redundant re-send (the calendar signal re-emits its full window every
/// refresh even when the briefing slice is unchanged) so subscribers don't
/// re-render on identical data.
pub(super) fn set_calendar(events: Vec<UpcomingEvent>) {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .calendar_tx
            .send_if_modified(|current| {
                if *current == events {
                    false
                } else {
                    *current = events;
                    true
                }
            });
    });
}

/// Publish the now-playing digest to the per-conn now-playing tasks (#528).
/// Latest-wins; skips a redundant re-send so a metadata poll that produced the
/// same title/artist/playing doesn't wake subscribers.
pub(super) fn publish_now_playing(now_playing: NowPlaying) {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .now_playing_tx
            .send_if_modified(|current| {
                if *current == now_playing {
                    false
                } else {
                    *current = now_playing;
                    true
                }
            });
    });
}

/// Publish the session-locked hint to the per-conn locked tasks (#484). Skips a
/// redundant re-send (mirrors [`publish_visibility`]).
pub(super) fn publish_locked(locked: bool) {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .locked_tx
            .send_if_modified(|current| {
                if *current == locked {
                    false
                } else {
                    *current = locked;
                    true
                }
            });
    });
}

/// Project the calendar service's [`CalendarEvent`]s onto the GTK-free wire
/// [`UpcomingEvent`] digest (#484): the events overlapping the next
/// [`CALENDAR_WINDOW_SECS`] (not already ended, starting within the window),
/// capped at [`MAX_UPCOMING_EVENTS`]. The calendar signal is sorted ascending by
/// start, so taking the first survivors of the filter yields the *next* events.
/// Pure, so the windowing/cap is unit-testable without the calendar service.
pub(super) fn to_upcoming_events(events: &[CalendarEvent], now_unix: i64) -> Vec<UpcomingEvent> {
    let window_end = now_unix.saturating_add(CALENDAR_WINDOW_SECS);
    events
        .iter()
        .filter(|e| {
            let start = e.start.timestamp();
            let end = e.end.timestamp();
            // Not already over, and it starts inside the window.
            end > now_unix && start < window_end
        })
        .take(MAX_UPCOMING_EVENTS)
        .map(|e| UpcomingEvent {
            start_unix: e.start.timestamp(),
            end_unix: e.end.timestamp(),
            title: e.summary.clone(),
            calendar: e.calendar_name.clone(),
        })
        .collect()
}

/// Project the mpris active [`Player`] onto the GTK-free wire [`NowPlaying`]
/// digest (#528): title/artist off its metadata, `playing` iff the player is
/// actually playing (paused/stopped/absent all read as not playing), plus the
/// track timing (#840) — `position_us` as the position poller last sampled it
/// and `length_us` off `mpris:length`, both already microseconds on the service
/// side and both `0` when unknown, which is exactly the digest's own "unknown"
/// encoding. `None` (no active player) is the empty, not-playing default. Pure.
pub(super) fn to_now_playing(player: Option<&Player>) -> NowPlaying {
    player.map_or_else(NowPlaying::default, |p| NowPlaying {
        title: p.title.clone(),
        artist: p.artists.clone(),
        playing: p.status == PlaybackStatus::Playing,
        position_us: p.position_us,
        length_us: p.length_us,
    })
}

/// Resolve libadwaita's `@accent_color` to an opaque RGBA byte quad on the GTK
/// thread (#376). Mirrors what the shell's CSS already does for the sparkline
/// (`.ts-sparkline { color: @accent_color; }`), but materialized in Rust so the
/// value can be handed to out-of-process plugins that can't read GTK themselves.
///
/// libadwaita registers `@accent_color` as a display-scope named color, so a
/// throwaway, unrealized widget resolves it. The style-context color lookup is
/// deprecated in GTK4, but the pinned libadwaita is on the `v1_4` feature and
/// `StyleManager::accent_color_rgba` needs `v1_6` — so this scoped-`allow`s the
/// deprecation rather than bumping the whole adw feature surface (which would
/// also risk the sandboxed `nix build` link). `None` when the color isn't
/// defined yet (e.g. providers not loaded), so the caller falls back to the
/// kit's hard-coded default.
pub(super) fn resolve_accent_color() -> Option<[u8; 4]> {
    let probe = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    #[allow(deprecated)]
    let rgba = probe.style_context().lookup_color("accent_color")?;
    Some(rgba_to_bytes(&rgba))
}

/// A `gdk::RGBA` (channels in `0.0..=1.0`) as an opaque `[r, g, b, 0xff]` byte
/// quad — the layout `preem` and [`HostMsg::Accent`](hytte_plugin_proto::HostMsg::Accent)
/// carry. Alpha is forced opaque: preem frames are screens and the accent is used
/// as an opaque ink.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rgba_to_bytes(rgba: &gtk::gdk::RGBA) -> [u8; 4] {
    // Each channel is clamped to 0.0..=1.0 then ×255 → 0.0..=255.0 and rounded,
    // so the cast is exact (mirrors `hytte-plugin-caw`'s `intensity`).
    let chan = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [
        chan(rgba.red()),
        chan(rgba.green()),
        chan(rgba.blue()),
        0xff,
    ]
}

/// Project `clock::now()`'s `DateTime<Local>` into the GTK-/chrono-free wire
/// [`ClockState`].
pub(super) fn to_clock_state(dt: &DateTime<Local>) -> ClockState {
    ClockState {
        iso: dt.to_rfc3339(),
        unix: dt.timestamp(),
    }
}

// ── Slot visibility (#288): OR of every monitor's sidebar open flag ───────────

thread_local! {
    /// GTK-thread-only per-monitor sidebar open flag, keyed by connector. The OR
    /// across its values is the single `visible` bool pushed to every connected
    /// plugin: a plugin's card mirrors onto **every** monitor's sidebar region,
    /// so it is "visible" while any one sidebar is open. Fed by `sidebar.rs`
    /// through [`set_sidebar_visibility`] (open/close) and
    /// [`forget_sidebar_visibility`] (hot-unplug).
    static SLOT_VISIBILITY_BY_MONITOR: RefCell<HashMap<String, bool>> =
        RefCell::new(HashMap::new());

    /// The same aggregate as a GTK-side `Mutable`, so the *binary* can gate its
    /// own pollers on plugin-card visibility the way a plugin gates its own
    /// (#840). The wire push travels by `watch` to the tokio per-conn tasks;
    /// this mirror travels by signal to `main.rs`, which needs the value as an
    /// `impl Signal` to fold into the mpris position gate. Written only by
    /// [`publish_visibility`], so the two can't disagree.
    static SLOT_VISIBLE: Mutable<bool> = Mutable::new(false);
}

/// The slot-visibility aggregate as an owned [`Mutable`] — `true` while any
/// monitor's sidebar is open. Owned (not an `impl Signal` accessor) for the
/// reason [`crate::components::visibility_gate::GateRegistry::mutable`] spells
/// out: a `&self`-free `'static` signal is what the wiring site needs.
pub(super) fn slot_visible_mutable() -> Mutable<bool> {
    SLOT_VISIBLE.with(Clone::clone)
}

/// A plugin's card is visible iff **any** monitor's sidebar is open — the card
/// mirrors onto every monitor's sidebar region, so one open sidebar shows it.
/// (An empty map — no monitors tracked yet — is not visible.)
pub(super) fn any_sidebar_open(open_by_monitor: &HashMap<String, bool>) -> bool {
    open_by_monitor.values().any(|&open| open)
}

/// Record `monitor_key`'s open flag in `map`, returning the new OR-aggregate.
/// Pure so the hot-plug aggregation is unit-testable without the registry.
pub(super) fn apply_open(map: &mut HashMap<String, bool>, monitor_key: &str, open: bool) -> bool {
    map.insert(monitor_key.to_owned(), open);
    any_sidebar_open(map)
}

/// Drop `monitor_key` from `map` (hot-unplug), returning the new OR-aggregate —
/// so a disappearing monitor that held the only open sidebar flips it to `false`.
/// Pure, for the same reason as [`apply_open`].
pub(super) fn apply_forget(map: &mut HashMap<String, bool>, monitor_key: &str) -> bool {
    map.remove(monitor_key);
    any_sidebar_open(map)
}

/// Record a monitor's sidebar open-state and, if the OR-aggregate changed, push
/// the new [`HostMsg::SlotVisibility`](hytte_plugin_proto::HostMsg::SlotVisibility)
/// to every connected plugin. Called from `sidebar.rs` on each open/close edge.
/// GTK-thread-only.
pub fn set_sidebar_visibility(monitor_key: &str, open: bool) {
    let visible =
        SLOT_VISIBILITY_BY_MONITOR.with(|m| apply_open(&mut m.borrow_mut(), monitor_key, open));
    publish_visibility(visible);
}

/// Forget a monitor's sidebar on hot-unplug and push the recomputed aggregate.
/// The disappearing monitor's flag leaves the OR, so if it held the only open
/// sidebar `visible` correctly drops to `false`. GTK-thread-only.
pub fn forget_sidebar_visibility(monitor_key: &str) {
    let visible =
        SLOT_VISIBILITY_BY_MONITOR.with(|m| apply_forget(&mut m.borrow_mut(), monitor_key));
    publish_visibility(visible);
}

/// Push `visible` on the watch channel, but only when it differs from the last
/// published value (`send_if_modified`) — so redundant open/close churn on one
/// monitor while another stays open doesn't wake the per-conn tasks. Latest-wins
/// is fine either way (it's state, not a one-shot event).
///
/// Also updates the [`SLOT_VISIBLE`] GTK-side mirror, on the same
/// only-when-changed rule and from this one place, so the binary's own gate
/// (#840) can never disagree with what the plugins were told.
fn publish_visibility(visible: bool) {
    let mirror = slot_visible_mutable();
    if mirror.get() != visible {
        mirror.set(visible);
    }
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .visibility_tx
            .send_if_modified(|current| {
                if *current == visible {
                    false
                } else {
                    *current = visible;
                    true
                }
            });
    });
}
