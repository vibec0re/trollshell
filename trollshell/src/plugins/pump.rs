//! GTK-side state publishers: the clock / accent / audio-spectrum projections
//! [`super::install`] wires into `watch` channels, plus the slot-visibility
//! (#288) aggregation fed from `sidebar.rs`. Each `publish_*` writes a
//! [`super::PluginHandles`] `watch::Sender`; the per-conn tasks in
//! [`super::session`] subscribe the matching receiver.
//!
//! Since #883 this module also owns the **preem animation clock** — the one
//! timer that advances every shell-side preem renderer and asks the affected
//! reconcilers to repaint. See [`install_preem_clock`].

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use hytte::futures_signals::map_ref;
use hytte::futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte::gtk::{self, glib, prelude::*};
use hytte::reactive::registry;
use hytte::services::calendar::CalendarEvent;
use hytte::services::mpris::{PlaybackStatus, Player};
use hytte::services::pipewire;
use hytte_plugin_proto::{
    AudioSpectrum, ClockState, MAX_UPCOMING_EVENTS, NowPlaying, UpcomingEvent,
};

use super::preem_render::{self, Role, Scope};
use super::{PluginHandles, SlotRender};

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
    // The shell's *plugin* preem surfaces (#883) cache their rasterised bytes,
    // and those were produced under the old ink — drop them so the next mapping
    // pass re-renders in the new accent. Registry-free, like the call above, so
    // the same test covers both halves of the re-tint.
    preem_render::invalidate_cached_frames();
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
    // A shell-rendered preem widget has no plugin to wake it: the re-tint only
    // reaches the screen once its reconciler re-maps the tree (#883). Every
    // cached frame was dropped, so this is the one path that wants the whole
    // fan-out rather than the animation clock's per-scope targeting.
    request_preem_repaint_all();
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

// ── Preem animation clock (#883) ─────────────────────────────────────────────

/// Install the single GTK-thread timer that animates every shell-side preem
/// renderer: marquee scroll, phosphor decay, needle physics, flip clocks and
/// peak-hold fall.
///
/// **One clock, not one per widget and not one per monitor.** The renderer
/// instances are shared across monitors (`preem_render`'s table is keyed by
/// plugin + tree, not by output), so advancing them from each monitor's
/// reconcile would run every animation at N× speed. This drives them once with
/// the *real* elapsed time and then asks the affected reconcilers to re-map.
///
/// Its per-tick *work* idles to nothing (the wakeup itself does not — see "The
/// standing cost" below). [`preem_render::any_animating`] is a handful of enum
/// matches, and with no preem widget on screen — the state of every session
/// until a plugin built on the new SDK dials in — the callback does nothing but
/// re-stamp its baseline.
///
/// ## Visibility gating, and the tradeoff that is *not* gated
///
/// The **repaint** is gated twice over. First on *which scopes moved*: only a
/// mailbox actually holding an advanced plugin's render is nudged, so one
/// animating widget no longer re-uploads a texture for every other plugin's
/// chips (see [`request_preem_repaint`]). Then on the two visibility signals the
/// host already keeps: a sidebar region only while some monitor's sidebar is
/// open (#288's aggregate), the drawer panel only while that plugin's panel is
/// the active one. Bar chips have no visibility signal and rely on the scope
/// gate alone.
///
/// The **advance** is deliberately not gated: it is a few floating-point
/// operations per instance, and skipping it would make a hidden gauge resume
/// mid-swing rather than settled. What that costs is one 20 Hz timer callback
/// while an animation is live but off-screen — the CPU-expensive half
/// (rasterising, and the reconcile pass) is what the gate above actually stops.
///
/// ## The standing cost, stated plainly
///
/// The timer is armed once here and never breaks, so **every** session pays 20
/// timer wakeups a second — including one with no plugins installed at all,
/// where the callback finds an empty instance table and returns. The work per
/// wakeup is negligible; the wakeup itself is not free on battery, and calling
/// that "idles cheaply" would only be true of the CPU half.
///
/// Parking it is possible and cheap — break out of the timer when
/// [`preem_render::any_animating`] goes false and re-arm from the mapping pass,
/// which is the only place an instance can *start* animating — and is
/// deliberately left out of this PR: the timer's arm/break behaviour is the one
/// part of this module no hermetic test can observe (there is no GTK main loop
/// under `cargo test`), so getting it wrong would freeze every preem animation
/// with CI still green. It belongs in a follow-up that can be verified on glass.
pub(super) fn install_preem_clock() {
    let last = Cell::new(Instant::now());
    glib::timeout_add_local(
        Duration::from_secs_f32(preem_render::ANIM_STEP_SECS),
        move || {
            let now = Instant::now();
            let dt = now.duration_since(last.replace(now)).as_secs_f32();
            // The `any_animating` probe first, so a session with no animated
            // preem widget never walks the instance table's mutable half — and
            // the baseline is re-stamped either way, so the tick that *does*
            // find work carries one frame's `dt`, not the idle period's.
            if preem_render::any_animating() {
                request_preem_repaint(&preem_render::advance_all(dt));
            }
            glib::ControlFlow::Continue
        },
    );
}

// ── Monitor-independent preem scope release (#921) ───────────────────────────

/// The number of render mailboxes a plugin id can appear in: the six mount
/// regions (`sidebar_{lead,top,bottom}` + `bar_{left,center,right}`) plus the
/// single shared `panels` list. A fixed-size array rather than a slice so a new
/// mailbox is a compile error at every call site, instead of a union that
/// silently stops covering it — which would look exactly like the defect this
/// module is fixing.
pub(super) const RENDER_MAILBOXES: usize = 7;

/// The set of plugin ids **any** render mailbox currently holds.
///
/// This is the host's answer to "which plugins are still here", and it is the
/// one [`drive_scope_releaser`] watches. A connection's teardown clears its
/// entry from all seven mailboxes (`session.rs:812-824`), so an id leaving this
/// union is exactly "the plugin left" — the same event
/// `region::reconcile_region`'s retain loop reacts to, read from a place that
/// does not need a region (or a monitor, or any widget) to exist.
///
/// **Membership only.** Each mailbox is projected to its ids through
/// [`ReadOnlyMutable::signal_ref`](hytte::futures_signals::signal::ReadOnlyMutable::signal_ref)
/// — no `signal_cloned` of the `Vec<SlotRender>`, which would deep-clone every
/// plugin's whole `wire::Node` tree — and the union is `dedupe_cloned`d. That
/// matters because these mailboxes are *deliberately* re-emitted with unchanged
/// contents up to 20 times a second: [`request_remap`] nudges them to drive the
/// preem animation clock's repaints. Without the dedupe this subscriber would
/// recompute a set and find no difference 20 times a second forever; with it,
/// the nudge wakes the projection, the id list compares equal, and nothing
/// downstream runs.
pub(super) fn live_plugin_ids_signal(
    mailboxes: [Mutable<Vec<SlotRender>>; RENDER_MAILBOXES],
) -> impl Signal<Item = HashSet<String>> {
    let [lead, top, bottom, left, center, right, panels] = mailboxes;
    map_ref! {
        let lead = mailbox_ids(&lead),
        let top = mailbox_ids(&top),
        let bottom = mailbox_ids(&bottom),
        let left = mailbox_ids(&left),
        let center = mailbox_ids(&center),
        let right = mailbox_ids(&right),
        let panels = mailbox_ids(&panels) => {
            let mut live: HashSet<String> = HashSet::new();
            for ids in [lead, top, bottom, left, center, right, panels] {
                live.extend(ids.iter().cloned());
            }
            live
        }
    }
    .dedupe_cloned()
}

/// One mailbox's plugin ids — the cheap projection [`live_plugin_ids_signal`]
/// unions, taken under the mailbox's own read lock so the render trees are
/// never cloned.
///
/// `use<>` because the returned signal owns its `ReadOnlyMutable` handle and
/// borrows nothing: without it, edition 2024's capture rules would tie the
/// `impl Signal` to `mailbox`'s lifetime and the `'static` subscription above
/// could not be built from a local.
fn mailbox_ids(mailbox: &Mutable<Vec<SlotRender>>) -> impl Signal<Item = Vec<String>> + use<> {
    mailbox
        .read_only()
        .signal_ref(|list| list.iter().map(|r| r.plugin_id.clone()).collect())
}

/// Forget a departed plugin's preem renderer instances — **both** of its scopes,
/// from a subscriber that exists whether or not any monitor does (#921).
///
/// ## Why this is not the regions' job
///
/// Before this, a card scope was released only by
/// [`region::reconcile_region`](super::region)'s retain loop and a panel scope
/// only by a drawer child's teardown — both of which are *per monitor widgets*.
/// Scope lifetime is not: `Scope::card(plugin_id)` and `Scope::panel(plugin_id)`
/// are keyed without a connector precisely because every monitor's copy of a
/// card (and every monitor's drawer child) shares one set of renderer instances.
/// So with **zero** live regions — a `monitors_changed` carrying an empty list,
/// which is a docked laptop's lid closing or every output unplugged — nothing
/// observed the leave and the instances stayed resident for the session. The
/// #920 review measured it (probe P2): `instance_count == 1` after the plugin
/// left, and a region rebuilt afterwards never had that card, so its retain loop
/// does not reclaim it either. Until #920 the corner was masked by the region
/// self-pin (#909) — a stranded-but-still-subscribed region released the scope
/// by accident; unpinning the region is what exposed it.
///
/// It matters beyond the memory: a leaked *animating* scope keeps
/// [`preem_render::any_animating`] true forever, so the 20 Hz
/// [`install_preem_clock`] tick keeps walking the instance table with nothing on
/// screen — and that predicate is exactly what #897's frame-clock parking is
/// built on.
///
/// ## Why it is a hand-written apply-loop and not `hytte::reactive::bind`
///
/// `bind` is the house helper for GTK-thread subscriptions and
/// `region::build_region` uses it — but every `bind*` takes a **widget** and
/// scopes the subscription to that widget's life. Anchoring *this* subscriber to
/// a widget would re-introduce the exact monitor-shaped lifetime it exists to
/// fix: the last bar torn down would take the releaser with it, on the very
/// emission it is there to catch. So it is a plain `spawn_local` for the process
/// lifetime, the same shape as the six other publishers `super::install` wires
/// up (clock, spectrum, calendar, now-playing, locked, effect broker), and it
/// holds no widget at all — nothing for a `WeakRef` to guard.
///
/// The consequence worth stating out loud: being outside `bind` puts this site
/// outside `nix/lint-bind-pins.py`'s reach (it finds its work by scanning
/// `bind*(` call sites, `nix/lint-bind-pins.py:211`), so a green `0 pin(s)` says
/// nothing about this function in either direction. What answers the pin
/// question here instead is that the loop captures no widget in the first place,
/// and `region::gtk_tests`'
/// `a_plugin_leaving_with_no_live_region_still_releases_its_card_scope` drives
/// the whole path with every region already destroyed.
///
/// ## Not a double-release
///
/// While a region *is* alive both this loop and that region's retain loop
/// forget the same `Scope::card`. [`preem_render::forget_scope`] is a
/// `HashMap::remove`, so the second is a miss — asserted, not assumed, by
/// `the_releaser_and_a_live_regions_retain_loop_may_both_release_one_scope`.
pub(super) async fn drive_scope_releaser(live: impl Signal<Item = HashSet<String>>) {
    // What the previous emission said was here. Starts empty rather than seeded
    // from the first emission, so the first emission — which lands at install
    // time, before any plugin has dialled in — can only *add*.
    let mut resident: HashSet<String> = HashSet::new();
    let mut live = std::pin::pin!(live);
    while let Some(next) = std::future::poll_fn(|cx| live.as_mut().poll_change(cx)).await {
        for gone in resident.difference(&next) {
            // Both of the plugin's trees. A plugin that never opened its panel
            // has no `Scope::panel` instances, and forgetting a scope that holds
            // none is a `HashMap` miss — cheaper than asking first.
            for scope in [Scope::card(gone), Scope::panel(gone)] {
                preem_render::forget_scope(&scope);
            }
        }
        resident = next;
    }
}

/// Subscribe [`drive_scope_releaser`] to the production mailboxes. Called once
/// from [`super::install`], on the GTK main thread — next to
/// [`install_preem_clock`], whose `any_animating` gate is the thing a leaked
/// animating scope corrupts.
///
/// The authoritative "plugin left" site is the connection teardown in
/// `session.rs:812-824`, which runs on a **tokio** task — `preem_render`'s
/// `STORE` is a GTK-thread `thread_local!`, so it cannot forget anything from
/// there. Riding the render mailboxes instead is the marshalling: the teardown
/// already writes them (that is how the regions learn), and this loop reads them
/// where the store lives.
pub(super) fn install_scope_releaser() {
    let live = registry::with(|r| {
        let handles = r
            .get::<PluginHandles>()
            .expect("plugins::service() not registered");
        live_plugin_ids_signal([
            handles.sidebar_lead.clone(),
            handles.sidebar_top.clone(),
            handles.sidebar_bottom.clone(),
            handles.bar_left.clone(),
            handles.bar_center.clone(),
            handles.bar_right.clone(),
            handles.panels.clone(),
        ])
    });
    glib::MainContext::default().spawn_local(drive_scope_releaser(live));
}

/// Ask the mount reconcilers holding one of `moved`'s scopes to re-map their
/// current trees, which is how an advanced preem renderer reaches the screen.
///
/// A render mailbox is a `Mutable<Vec<SlotRender>>` and the regions subscribe to
/// it, so nudging it re-runs `reconcile_region` over the *same* trees —
/// `to_ui_node` then rasterises the preem nodes from their advanced instances.
///
/// ## Why this is per-scope and not a blanket nudge
///
/// A nudge is not free downstream, and it is not free for the *other* plugins in
/// the region either. `Reconciler::render` has no descriptor-equality
/// short-circuit, so every `Pixels` node in a re-mapped mailbox reaches
/// `hytte-ui`'s `PixelSurface::set_pixels` on every nudge. Since #907
/// `set_pixels` itself compares against the last accepted frame and skips the
/// `glib::Bytes` + `gdk::MemoryTexture` + `queue_draw` when the bytes are
/// unchanged, but that guard is per-surface and per-call — it does not stop the
/// call from happening, only its GTK cost when nothing moved. This per-scope
/// targeting is the guard that stops the call from happening at all: without
/// it, nudging every bar mailbox because *something somewhere* animated would
/// still walk and compare a full frame for every `Pixels` node of every plugin
/// chip on every monitor at 20 Hz, legacy self-rasterising plugins included —
/// cheaper than an unconditional re-upload, but not free at the wire's buffer
/// cap and instance count.
///
/// [`preem_render::advance_all`] therefore names the scopes that moved, and a
/// mailbox is nudged only when it actually carries one of their plugins.
/// Visibility still gates on top of that: a sidebar region only while some
/// monitor's sidebar is open (#288's aggregate), the panel mailbox only when the
/// moved panel belongs to the plugin whose panel is on screen.
///
/// The remaining over-repaint is within a region: a nudge re-maps every card in
/// the mailbox, not just the mover's. Splitting that needs a per-plugin mailbox
/// or a descriptor diff in `hytte-ui`, neither of which is this PR.
fn request_preem_repaint(moved: &[Scope]) {
    if moved.is_empty() {
        return;
    }
    let cards: HashSet<&str> = moved
        .iter()
        .filter(|scope| scope.role() == Role::Card)
        .map(Scope::plugin_id)
        .collect();
    let sidebar_visible = slot_visible_mutable().get();
    registry::with(|r| {
        let handles = r
            .get::<PluginHandles>()
            .expect("plugins::service() not registered");
        if !cards.is_empty() {
            for mailbox in [&handles.bar_left, &handles.bar_center, &handles.bar_right] {
                request_remap_holding(mailbox, &cards);
            }
            if sidebar_visible {
                for mailbox in [
                    &handles.sidebar_lead,
                    &handles.sidebar_top,
                    &handles.sidebar_bottom,
                ] {
                    request_remap_holding(mailbox, &cards);
                }
            }
        }
        // The panel mailbox carries every plugin's panel tree but only the
        // active one is on screen, so a moved panel scope for any other plugin
        // would repaint nothing. Read the selection out and drop its guard
        // before taking the mailbox's write lock.
        let active = handles.active_panel_id.lock_ref().clone();
        if let Some(active) = active
            && moved
                .iter()
                .any(|scope| scope.role() == Role::Panel && scope.plugin_id() == active)
        {
            request_remap(&handles.panels);
        }
    });
}

/// Re-map every mount mailbox, whoever is in it — the accent path.
///
/// A skin change re-tints *every* shell-rendered preem surface at once
/// ([`tint_in_process_surfaces`] drops all the cached frames), so there is no
/// subset to narrow to and the per-scope targeting above would only cost a
/// comparison. Rare (an accent or color-scheme change), unlike the 20 Hz clock.
fn request_preem_repaint_all() {
    let sidebar_visible = slot_visible_mutable().get();
    registry::with(|r| {
        let handles = r
            .get::<PluginHandles>()
            .expect("plugins::service() not registered");
        for mailbox in [&handles.bar_left, &handles.bar_center, &handles.bar_right] {
            request_remap(mailbox);
        }
        if sidebar_visible {
            for mailbox in [
                &handles.sidebar_lead,
                &handles.sidebar_top,
                &handles.sidebar_bottom,
            ] {
                request_remap(mailbox);
            }
        }
        if handles.active_panel_id.lock_ref().is_some() {
            request_remap(&handles.panels);
        }
    });
}

/// [`request_remap`], but only if `mailbox` actually holds a render for one of
/// `movers` — the plugins whose renderer instances just advanced.
pub(super) fn request_remap_holding(mailbox: &Mutable<Vec<SlotRender>>, movers: &HashSet<&str>) {
    let holds = {
        let list = mailbox.lock_ref();
        list.iter()
            .any(|render| movers.contains(render.plugin_id.as_str()))
    };
    if !holds {
        return;
    }
    let mut guard = mailbox.lock_mut();
    let _ = guard.as_mut_slice();
}

/// Notify a render mailbox's subscribers without changing its contents.
///
/// `Mutable`'s write guard only arms its wake-on-drop once something goes
/// through `DerefMut`, so the deliberately-inert `as_mut_slice` below is what
/// turns this into a repaint request rather than a silent no-op. The read probe
/// runs in its own statement so its guard is released before the write lock is
/// taken (holding both would deadlock the `RwLock`).
pub(super) fn request_remap(mailbox: &Mutable<Vec<SlotRender>>) {
    let empty = mailbox.lock_ref().is_empty();
    if empty {
        return;
    }
    let mut guard = mailbox.lock_mut();
    let _ = guard.as_mut_slice();
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

// ── #906 item 1: `request_preem_repaint` end-to-end, not just its predicate ──

/// Colocated with this module (via `#[path]`, so the file sits beside
/// `pump.rs` as `pump_tests.rs` rather than under a `pump/` subdirectory —
/// the default resolution for a `mod` declared from a non-`mod.rs` file)
/// rather than folded into `super::tests`: [`request_preem_repaint`] is
/// private to this module, and `super::tests` is a different builder's lane
/// while #906 is split across two PRs.
#[cfg(test)]
#[path = "pump_tests.rs"]
mod pump_tests;
