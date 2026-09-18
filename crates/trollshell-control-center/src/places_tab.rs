//! The **Places** tab (#640 / #703) — a real editor for
//! `~/.config/trollshell/places.toml`.
//!
//! Before this, the tab called "Place" edited a *different* thing from what
//! `places.toml` calls a place: three widgets over the shell's session-only
//! `PlaceOverride`, whose whole job is steering the weather widget. The ten
//! fields that actually drive departures, Wi-Fi place detection and walk time
//! had no UI at all — which is why #641 (a wrong `station` in the shipped
//! default) was a bug no in-app action could fix. Those three widgets are still
//! here, demoted into their own group and named for what they do.
//!
//! # Why this writes the file directly
//!
//! Every other tab round-trips over the shell's `Control` D-Bus endpoint,
//! because every other tab manages something that *is* the running shell:
//! systemd units, the login keyring, a runtime `Mutable`. This one doesn't.
//! `places.toml` is the state store and the shell is a client of it — the
//! "system-daemon-as-state-store" constraint read correctly — so:
//!
//! * **The editor keeps working while the shell is down.** That is the case
//!   #641 is: a config wrong enough to break a widget is exactly when you need
//!   to fix it, and a D-Bus-only editor would be dead in precisely that state.
//! * **No reload plumbing.** The shell polls the file's mtime every 3 s (9 s on
//!   battery) and re-reads only when the content actually differs, so a save
//!   lands live with nothing to notify.
//! * **No new D-Bus surface**, and therefore no version skew between a running
//!   shell and a newer control center.
//!
//! What it emphatically does *not* mean is a second copy of the write logic.
//! The validation, the mutation rules and the format-preserving writer all live
//! in [`hytte_config::places`], which is the same code `hytte-services::places`
//! writes through. Two writers over one file agreeing byte for byte is the
//! whole reason that crate exists.
//!
//! # Concurrency
//!
//! `places.toml` has a third writer too: `$EDITOR`. Every save therefore checks
//! that the file still holds the set this tab last read
//! ([`hytte_config::places::check_base`]) and refuses rather than clobbering a
//! hand edit; the tab offers a reload instead. A poll on the file's mtime keeps
//! the list live in the meantime, so an edit made in a terminal shows up here
//! without reopening the app.
//!
//! # Deliberately not here (yet)
//!
//! No "capture visible networks" picker and no station verify/search button.
//! Both need the running shell (they read `wifiscan`'s live AP list and
//! departures' transit agent), so both are `Control` methods and later phases;
//! `trollshell --scan-aps` stays the documented way to collect a fingerprint,
//! and the station field is a plain entry.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use hytte_config::places::{self, Place, PlacesError};

use crate::spawn_on_runtime;

/// How often the tab re-checks `places.toml` for an out-of-band edit. Matches
/// the Plugins tab's cadence; each tick is a single `stat` on a cached inode,
/// and the file is only re-read when the mtime moves *and* the reparse differs.
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Upper bound for the walk-time spinner, in minutes. Past a two-hour walk the
/// leave-by countdown has stopped being about catching a train.
const MAX_WALK_MINUTES: f64 = 120.0;

/// Upper bound for the `GeoClue` fallback radius, in km. The shipped default is
/// 12; the ceiling is generous because `GeoClue` is city-level and a large
/// radius is a legitimate "anywhere in this city" fingerprint-less place.
const MAX_RADIUS_KM: f64 = 500.0;

/// Ceiling for the `match_min` spinner's own clamp. The row's real range is
/// `1..=ssids.len()`; this only bounds the `f64` → `usize` conversion so it is
/// total for any value a widget could hand back.
const MAX_SSIDS: f64 = 4096.0;

/// Which of a place's three string lists a row belongs to. They edit
/// identically and differ only in labels and in what an empty list *means*, so
/// one builder covers all three.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListField {
    /// The Wi-Fi fingerprint. Empty = never matches by fingerprint.
    Ssids,
    /// Departures line filter. Empty = every line.
    Lines,
    /// Departures destination filter. Empty = every direction.
    Directions,
}

impl ListField {
    fn get(self, place: &Place) -> &Vec<String> {
        match self {
            Self::Ssids => &place.ssids,
            Self::Lines => &place.lines,
            Self::Directions => &place.directions,
        }
    }

    fn get_mut(self, place: &mut Place) -> &mut Vec<String> {
        match self {
            Self::Ssids => &mut place.ssids,
            Self::Lines => &mut place.lines,
            Self::Directions => &mut place.directions,
        }
    }

    /// What one entry is called, for the per-item row titles.
    fn item(self) -> &'static str {
        match self {
            Self::Ssids => "Network",
            Self::Lines => "Line",
            Self::Directions => "Direction",
        }
    }

    /// `(group title, group description, add-row title)`.
    fn labels(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Ssids => (
                "Wi-Fi fingerprint",
                "Networks you reliably see HERE but not at your other places — usually the \
                 neighbours'. Matched by SSID, so it survives a router swap. Collect them by \
                 standing here and running `trollshell --scan-aps`. An empty list never matches, \
                 and detection falls through to the GeoClue radius below.",
                "Add a network",
            ),
            Self::Lines => (
                "Lines",
                "Which lines to show. Empty means every line through the station — which is the \
                 safer default: a wrong filter fails invisibly (an empty board forever, \
                 indistinguishable from a quiet evening), while no filter fails visibly.",
                "Add a line",
            ),
            Self::Directions => (
                "Directions",
                "Destination substrings to keep. Empty means every direction.",
                "Add a direction",
            ),
        }
    }
}

/// Which of this file's two nix-lockable keys the base layer pinned (#1227
/// item 2).
///
/// Two independent booleans rather than one "managed by nix" flag, because
/// `programs.trollshell.config.places.place` and
/// `…places.departures.endpoint` lock independently: an operator can hand the
/// place list to nix and still pick their own departures backend, or the other
/// way round.
///
/// A locked key is not merely "a save would be refused": the merge keeps the
/// nix value on the *next load*, so a row offered as editable here would be
/// silently reverted a poll tick later. The rows are therefore made
/// insensitive, which is exactly the "greyed row, subtitle `set in nix`"
/// surface #1331 had nowhere to build (that PR's own note: "the control-center
/// has no editable surface for any `Subsystem` family today … Places is item
/// 2's own writer").
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Locks {
    /// `_locked = ["place"]` — the whole `[[place]]` array is nix's. Atomic,
    /// because rule 3 replaces arrays whole: there is no "nix set two places
    /// and you may add a third".
    places: bool,
    /// `_locked = ["departures.endpoint"]` — the backend is nix's.
    endpoint: bool,
}

impl Locks {
    /// What a load actually enforced.
    fn of(layered: &places::Layered) -> Self {
        Self {
            places: layered.places_are_locked(),
            endpoint: layered.endpoint_is_locked(),
        }
    }
}

/// What the list's description says when the set is the operator's own.
const LIST_DESCRIPTION: &str = "Somewhere you frequent, how the shell recognises it, and what departures to show there. \
     Saved straight to ~/.config/trollshell/places.toml, which the shell re-reads within a few \
     seconds — so this works whether or not trollshell is running, and hand edits to that file \
     are preserved.";

/// What it says when nix owns the set — the sentence the operator needs,
/// naming the option they have to edit instead.
const NIX_MANAGED_PLACES: &str = "Set in nix: these places come from programs.trollshell.config.places.place, which cannot be \
     overridden from ~/.config/trollshell/places.toml. Edit them there and rebuild.";

/// Everything the tab's handlers share. Cheap to clone (all handles), which is
/// what lets each widget's closure own one.
#[derive(Clone)]
struct Editor {
    /// The set as `places.toml` last read back. Every save is checked against
    /// this, so an `$EDITOR` save landing under us is caught rather than
    /// clobbered, and it is re-read from the file after each successful write
    /// so it always says exactly what is on disk.
    base: Rc<RefCell<Vec<Place>>>,
    /// What the nix base layer pinned (#1227 item 2). Refreshed wherever
    /// [`Self::base`] is, because a `nixos-rebuild` can add or drop the lock
    /// under a running window and the poll below sees the base layer move.
    locked: Rc<Cell<Locks>>,
    /// The entry row `[departures].endpoint` (#1124) is edited through — a
    /// whole-shell setting, not per-place, so it lives beside `base` rather
    /// than inside it. Kept so a successful save or an out-of-band
    /// [`Self::reload`] can push the file's own word for it back into the
    /// displayed text (the group it lives in isn't part of [`Self::rebuild`]'s
    /// per-place teardown, unlike `station`'s row, which is simply rebuilt
    /// fresh every time); the widget's own text is the state, so there's no
    /// separate cell to keep in sync the way `base` needs one.
    departures_endpoint_row: adw::EntryRow,
    /// `[departures].endpoint` **as the merged config last read back** — what
    /// [`Self::base`] is for the place list, and the thing
    /// [`Self::apply_view`] compares against.
    ///
    /// Separate from the row's own text on purpose: that text is the
    /// operator's draft until they press apply, and a refresh that compared
    /// the widget would treat a half-typed backend as a difference and
    /// overwrite it with the file's value.
    endpoint: Rc<RefCell<Option<String>>>,
    /// The stack detail pages are pushed onto.
    nav: adw::NavigationView,
    /// The root page's list of places, rebuilt whenever the set changes.
    list: adw::PreferencesGroup,
    /// Every child currently in `list`, for teardown before a rebuild.
    ///
    /// Never held borrowed across a `list.remove()`: GTK emits synchronously
    /// into handlers that re-enter these cells, and a `BorrowMutError` inside a
    /// glib callback aborts the process rather than failing gracefully (#643).
    ///
    /// Held **weakly** per entry (#1384 item 2): a place row's own
    /// `connect_activated` closure captures a [`WeakEditor`] that carries
    /// this very `Rc` strongly (it is bookkeeping, not a widget, so
    /// `WeakEditor`'s own doc's "cloned strongly on purpose" applies) — so a
    /// **strong** `gtk::Widget` here would have this row's own closure
    /// reach back through `rows` to the row itself, a self-sustaining loop
    /// with no external anchor, exactly the shape a strong `list`/`nav` had
    /// before this. `list` still owns the row strongly (as its child); this
    /// is only the second, bookkeeping-only reference, and it must not also
    /// be a strong one.
    rows: Rc<RefCell<Vec<glib::WeakRef<gtk::Widget>>>>,
    /// The place name the running shell currently resolves to, for the list's
    /// "you are here" badge. `None` when the shell isn't running.
    resolved: Rc<RefCell<Option<String>>>,
    /// The page's status line: what the shell resolves to right now. The one
    /// thing on this tab sourced from the running shell rather than the file.
    status_row: adw::ActionRow,
    /// The weather override's auto/manual switch, kept in step by the same
    /// `GetPlace` read that fills [`Self::status_row`].
    auto_switch: adw::SwitchRow,
    /// Where save failures surface.
    toasts: adw::ToastOverlay,
    /// Guard so programmatically setting a widget from the model doesn't loop
    /// back into a save (mirrors the other tabs' `syncing`).
    syncing: Rc<Cell<bool>>,
}

impl Editor {
    /// The set as it currently stands on disk.
    fn places(&self) -> Vec<Place> {
        self.base.borrow().clone()
    }

    /// Write `next`, then re-read what the file actually says.
    ///
    /// The re-read is not paranoia: [`places::save`] normalises its input (a
    /// padded name is trimmed, blank list entries dropped) and an emptied set
    /// reads back as the built-in default, so the file can legitimately hold
    /// something other than what was handed in. Taking the file's word for it
    /// keeps `base` exact, which is what the next save's `check_base` compares.
    ///
    /// Returns whether the save landed; a failure toasts and leaves both the
    /// file and `base` untouched.
    fn save(&self, next: Vec<Place>) -> bool {
        let base = self.places();
        match places::save(&base, next) {
            Ok(()) => {
                // #1338 review, L3: `apply_view` rather than a hand-rolled
                // `load_places()` + `Locks::read()` pair, so this site keeps
                // the endpoint row's sensitivity in step like the other three.
                self.refresh_from_disk();
                self.rebuild();
                true
            }
            Err(err) => {
                self.report(&err);
                false
            }
        }
    }

    /// Replace one place through `f` and save the result. Each control edits
    /// its own field off the *saved* set rather than off a shared draft, so a
    /// value another field rejected can't poison an unrelated edit.
    fn edit(&self, index: usize, f: impl FnOnce(&mut Place)) -> bool {
        let mut next = self.places();
        let Some(place) = next.get_mut(index) else {
            return false;
        };
        f(place);
        self.save(next)
    }

    /// Write the departures endpoint (#1124), then re-read what the file
    /// actually says — the endpoint-only counterpart to [`Self::save`]. No
    /// `check_base` here (as [`Self::save`] has via [`places::save`]): the key
    /// is a single scalar with nothing to have drifted underneath an edit the
    /// way the place set can.
    ///
    /// Returns whether the save landed; a failure toasts and leaves both the
    /// file and the displayed text untouched.
    fn save_departures_endpoint(&self, next: Option<&str>) -> bool {
        match places::save_departures_endpoint(next) {
            Ok(()) => {
                // The whole view, not just this row's text: the endpoint save
                // creates the overlay when there is none, and under a nix base
                // layer the seed it creates is a filtered one (#1338 review,
                // H1) — so the list can legitimately have moved too.
                self.refresh_from_disk();
                true
            }
            Err(err) => {
                self.report(&err);
                false
            }
        }
    }

    /// Surface a rejected save. `ChangedOnDisk` gets a Reload action instead of
    /// a bare complaint — it is the one failure the user can clear with one
    /// click, and the only one where *this* window is holding the stale copy.
    fn report(&self, err: &PlacesError) {
        let toast = adw::Toast::new(&err.to_string());
        toast.set_timeout(if matches!(err, PlacesError::ChangedOnDisk) {
            0
        } else {
            6
        });
        if matches!(err, PlacesError::ChangedOnDisk) {
            toast.set_button_label(Some("Reload"));
            // Weakly (#1384 item 2): this toast's `timeout` is 0 (never
            // auto-dismissed), so `toasts` owns it for as long as the
            // operator leaves it up — a strong `Editor` here (carrying
            // `toasts` itself) would close exactly that cycle.
            let editor = self.downgrade();
            toast.connect_button_clicked(move |_| {
                if let Some(editor) = editor.upgrade() {
                    editor.reload();
                }
            });
        }
        self.toasts.add_toast(toast);
    }

    /// Re-read the file and rebuild the list — after an out-of-band edit, or
    /// when the user dismisses a "changed on disk" toast.
    ///
    /// Pops back to the root first: a detail page addresses its place by index,
    /// and the reload may have removed or reordered it.
    fn reload(&self) {
        self.refresh_from_disk();
        while self.nav.pop() {}
        self.rebuild();
    }

    /// Re-read every layer and push the result into the model and the widgets
    /// that [`Self::rebuild`] does not own. Returns whether anything the tab
    /// shows actually moved.
    ///
    /// One read for all three answers (#1338 review, H2/L3): before this, each
    /// refresh site composed its own `load_places()` + `Locks::read()` +
    /// `load_departures_endpoint()`, which is three passes over the search
    /// path that can disagree with each other, and three places to forget the
    /// endpoint row's sensitivity — which three of four sites did.
    fn refresh_from_disk(&self) -> bool {
        let loaded = places::load_layered();
        let locks = Locks::of(&loaded);
        self.apply_view(loaded.places, loaded.endpoint, locks)
    }

    /// [`Self::refresh_from_disk`]'s pure-ish half: apply an already-read view.
    ///
    /// Split out so the GTK tests can drive the refresh with a fabricated view
    /// instead of the real `$HOME`/`$XDG_CONFIG_DIRS` — the same argument the
    /// `gtk_tests` module doc makes for seeding `base` directly.
    ///
    /// **The return value is the whole point.** The poll below used to sit
    /// behind `ConfigWatcher::poll`, which dedups on the merged *list*; a lock
    /// is not a list, so a `nixos-rebuild` that adds or drops
    /// `programs.trollshell.config.places` while the overlay already holds the
    /// same list reported nothing and the greyed rows outlived the option that
    /// greyed them. The watcher now only says *a layer moved*
    /// (`ConfigWatcher::moved`) and this says *whether it mattered*, over all
    /// three things the tab renders.
    fn apply_view(&self, places: Vec<Place>, endpoint: Option<String>, locks: Locks) -> bool {
        // Compared against **the endpoint the file last said**, never against
        // the row's own text: that text is the operator's *draft* until they
        // press apply, and this now runs on every layer move (including the
        // tab's own place save), so comparing the widget would overwrite a
        // half-typed backend with the file's value two seconds after they
        // started typing it.
        let endpoint_moved = *self.endpoint.borrow() != endpoint;
        let changed = *self.base.borrow() != places || self.locked.get() != locks || endpoint_moved;

        *self.base.borrow_mut() = places;
        self.locked.set(locks);
        self.departures_endpoint_row.set_sensitive(!locks.endpoint);
        if endpoint_moved {
            self.departures_endpoint_row
                .set_text(endpoint.as_deref().unwrap_or_default());
            *self.endpoint.borrow_mut() = endpoint;
        }
        changed
    }

    /// Read the shell's resolved place (`GetPlace`) into the status line, the
    /// list's "you are here" badge, and the override switch.
    ///
    /// The only call this tab makes to the running shell. Failing it is not an
    /// error state for the *editor* — the file is still editable and the shell
    /// will pick the edits up next launch — so the row says that rather than
    /// the other tabs' bare "Unavailable".
    fn refresh_place(&self) {
        let editor = self.clone();
        spawn_on_runtime(crate::get_place(), move |res| match res {
            Ok((label, auto)) => {
                // `ActionRow` subtitles are Pango markup, and a place name is
                // whatever the user typed.
                editor
                    .status_row
                    .set_subtitle(&glib::markup_escape_text(&label));
                // Suppress the switch's notify handler during the sync, or the
                // programmatic set loops straight back into SetAutoLocation.
                editor.syncing.set(true);
                editor.auto_switch.set_active(auto);
                editor.syncing.set(false);
                let changed = editor.resolved.borrow().as_deref() != Some(label.as_str());
                *editor.resolved.borrow_mut() = Some(label);
                if changed {
                    editor.rebuild();
                }
            }
            Err(err) => {
                tracing::info!(%err, "GetPlace failed");
                editor.status_row.set_subtitle(
                    "trollshell isn't running — edits here apply the next time it starts",
                );
                // Bound the `RefMut` rather than leaving it a temporary in the
                // condition: `rebuild` borrows the same cell, and "when exactly
                // does this drop" is not a question to leave to the reader
                // inside a glib callback (#643).
                let was_badged = editor.resolved.borrow_mut().take().is_some();
                if was_badged {
                    editor.rebuild();
                }
            }
        });
    }

    /// Re-read now and once more after the shell's resolve lag — a
    /// forward-geocode plus a re-resolve takes a beat — so the status line
    /// catches up to a just-applied override without the user refreshing.
    fn refresh_place_soon(&self) {
        self.refresh_place();
        let editor = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            editor.refresh_place();
        });
    }

    /// Rebuild the root page's list of places from `base`.
    fn rebuild(&self) {
        // `take()`, not a chained `borrow_mut()`: the `RefMut` would stay live
        // across every `list.remove()`, which can emit synchronously into a
        // handler that re-enters this cell (#643).
        for row in self.rows.take() {
            if let Some(row) = row.upgrade() {
                self.list.remove(&row);
            }
        }
        let places = self.places();
        let resolved = self.resolved.borrow().clone();
        let locked = self.locked.get().places;
        // #1227 item 2: say whose the list is, right where the list is. The
        // detail pages still open (reading a nix-set place is useful); every
        // control on them is insensitive, and "Add a place" is not offered at
        // all — there is nothing to add to, since the overlay's array would
        // replace nix's whole (rule 3) and then be refused.
        if locked {
            self.list.set_description(Some(NIX_MANAGED_PLACES));
        } else {
            self.list.set_description(Some(LIST_DESCRIPTION));
        }
        let mut rows = Vec::with_capacity(places.len() + 1);
        for (index, place) in places.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&place.name))
                .subtitle(glib::markup_escape_text(&summarize(place)))
                .activatable(true)
                .build();
            // "You are here": the one thing on this page sourced from the
            // running shell, and the only way to tell whether a fingerprint you
            // just typed actually works.
            if resolved.as_deref() == Some(place.name.as_str()) {
                let badge = gtk::Image::from_icon_name("find-location-symbolic");
                badge.set_tooltip_text(Some("Where the shell thinks you are right now"));
                badge.add_css_class("accent");
                row.add_prefix(&badge);
            }
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            // Weakly (#1384 item 2): `list` owns `row` owns this closure, and
            // a strong `Editor` here carries `list` itself — the same cycle
            // `WeakEditor`'s own doc names.
            let editor = self.downgrade();
            row.connect_activated(move |_| {
                if let Some(editor) = editor.upgrade() {
                    editor.open(index);
                }
            });
            self.list.add(&row);
            rows.push(row.upcast::<gtk::Widget>().downgrade());
        }

        if !locked {
            let add = adw::ActionRow::builder()
                .title("Add a place")
                .subtitle("Somewhere you frequent: home, the office, a regular haunt")
                .activatable(true)
                .build();
            add.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
            // Weakly, for the place row's reason above.
            let editor = self.downgrade();
            // Deferred to an idle tick, unlike the place rows above. `add`
            // saves, and a save rebuilds this group — which would mean removing
            // *this row* from inside its own `row-activated` emission. The
            // place rows only push a page, so they can run inline.
            add.connect_activated(move |_| {
                let Some(editor) = editor.upgrade() else {
                    return;
                };
                glib::idle_add_local_once(move || editor.add());
            });
            self.list.add(&add);
            rows.push(add.upcast::<gtk::Widget>().downgrade());
        }

        *self.rows.borrow_mut() = rows;
    }

    /// Append a fresh place and open it, so the first thing the user does is
    /// name it. Coordinates start at 0/0 rather than at a guess — the shell has
    /// a "use my location" path, but it needs the running shell and is a later
    /// phase.
    fn add(&self) {
        let mut next = self.places();
        next.push(Place::new(unused_name(&next), 0.0, 0.0));
        if self.save(next) {
            self.open(self.places().len().saturating_sub(1));
        }
    }

    /// Push the detail page for the place at `index`.
    fn open(&self, index: usize) {
        let places = self.places();
        let Some(place) = places.get(index) else {
            return;
        };
        self.nav.push(&self.detail(index, place));
    }

    /// The detail page: four groups, ordered to mirror the resolver's own
    /// priority (fingerprint beats radius beats nothing), so the page teaches
    /// why a fingerprint is worth capturing.
    fn detail(&self, index: usize, place: &Place) -> adw::NavigationPage {
        let page = adw::PreferencesPage::new();
        page.add(&self.identity_group(index, place));
        page.add(&self.list_group(
            index,
            ListField::Ssids,
            Some(self.match_min_row(index, place)),
        ));
        page.add(&self.location_group(index, place));
        page.add(&self.departures_group(index, place));
        page.add(&self.list_group(index, ListField::Lines, None));
        page.add(&self.list_group(index, ListField::Directions, None));

        // The shared no-window-controls header bar (#944, `plugins_tab`'s
        // `tab_header_bar`): this page is pushed into the tab's own
        // `AdwNavigationView`, itself mounted inside `crate::build_window`'s
        // `AdwApplicationWindow`, which already draws the window's real
        // controls in its own header bar. `adw::HeaderBar::new()` here would
        // draw a second `GtkWindowControls` cluster — the #943 shape,
        // reused rather than duplicated.
        let header = crate::plugins_tab::tab_header_bar();
        let locked = self.locked.get().places;
        if locked {
            // #1227 item 2: the page still opens — reading a nix-set place is
            // the point of having it on screen — but every control on it is
            // insensitive and there is no Delete, because the whole array is
            // nix's and the next load would refuse anything written here. One
            // `set_sensitive` on the page rather than a per-row flag threaded
            // through six group builders: the rule is about the file, not
            // about any one row, and a per-row version would have to be got
            // right six times.
            page.set_sensitive(false);
        } else {
            let delete = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Delete this place")
                .build();
            delete.add_css_class("flat");
            delete.add_css_class("destructive-action");
            {
                // Weakly (#1384 item 2): this button lives on the pushed
                // detail page, itself a child of `nav` while it is on
                // screen — a strong `Editor` here carries `nav` right back.
                let (editor, name) = (self.downgrade(), place.name.clone());
                delete.connect_clicked(move |btn| {
                    if let Some(editor) = editor.upgrade() {
                        editor.confirm_delete(btn, index, &name);
                    }
                });
            }
            header.pack_end(&delete);
        }

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        if locked {
            let banner = adw::Banner::new(NIX_MANAGED_PLACES);
            banner.set_revealed(true);
            toolbar.add_top_bar(&banner);
        }
        toolbar.set_content(Some(&page));
        adw::NavigationPage::new(&toolbar, &place.name)
    }

    /// Identity: the name, which is also this place's identity everywhere else
    /// (the `place-changed` hook dedups on it, and the editing API addresses
    /// places by it).
    fn identity_group(&self, index: usize, place: &Place) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("Identity")
            .description(
                "The name shown in the bar, and what the place-changed hook reports. Must be \
                 unique.",
            )
            .build();
        let name = adw::EntryRow::builder()
            .title("Name")
            .text(&place.name)
            .show_apply_button(true)
            .build();
        // Weakly (#1384 item 2): `name` lives on a pushed detail page, a
        // child of `nav` while it is on screen.
        let editor = self.downgrade();
        name.connect_apply(move |entry| {
            let Some(editor) = editor.upgrade() else {
                return;
            };
            let text = entry.text().to_string();
            if editor.edit(index, |place| place.name = text) {
                editor.refresh_detail_title(index);
            }
        });
        group.add(&name);
        group
    }

    /// Location: the `GeoClue` fallback. Coordinates get *pasted*, so they are
    /// entries with an error state rather than spinners — a stepper is useless
    /// at 1e-4 degrees. The radius genuinely is a spinner.
    fn location_group(&self, index: usize, place: &Place) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("Location")
            .description(
                "Used when no Wi-Fi fingerprint matches: the nearest place within its radius \
                 wins. Also the coordinates the weather widget uses while you are here.",
            )
            .build();
        group.add(
            &self.coord_row(index, "Latitude", place.lat, places::MAX_LAT, |p, v| {
                p.lat = v;
            }),
        );
        group.add(
            &self.coord_row(index, "Longitude", place.lon, places::MAX_LON, |p, v| {
                p.lon = v;
            }),
        );

        // Floor 0.1 rather than a round 0.5: the model only requires a positive
        // radius, and a `SpinRow` silently *clamps* a value outside its range,
        // so too high a floor would show a hand-written `radius_km = 0.2` as
        // something the file doesn't say.
        let radius = adw::SpinRow::with_range(0.1, MAX_RADIUS_KM, 0.5);
        radius.set_title("Radius (km)");
        radius.set_subtitle("How close counts as \"here\" when falling back to GeoClue");
        radius.set_digits(1);
        radius.set_value(place.radius_km);
        // Weakly, for the identity row's reason above.
        let editor = self.downgrade();
        radius.connect_value_notify(move |row| {
            let Some(editor) = editor.upgrade() else {
                return;
            };
            if editor.syncing.get() {
                return;
            }
            let value = row.value();
            editor.edit(index, |place| place.radius_km = value);
        });
        group.add(&radius);
        group
    }

    /// One coordinate entry. Unparseable or out-of-range input paints the row
    /// with the `error` class and is not written — the model would reject it
    /// anyway, and a toast per keystroke would be worse than a red border.
    fn coord_row(
        &self,
        index: usize,
        title: &str,
        value: f64,
        limit: f64,
        set: impl Fn(&mut Place, f64) + 'static,
    ) -> adw::EntryRow {
        let row = adw::EntryRow::builder()
            .title(title)
            .text(format!("{value}"))
            .show_apply_button(true)
            .build();
        // Weakly, for the identity row's reason above.
        let editor = self.downgrade();
        row.connect_apply(move |entry| {
            let Some(editor) = editor.upgrade() else {
                return;
            };
            match entry.text().trim().parse::<f64>() {
                Ok(parsed) if (-limit..=limit).contains(&parsed) => {
                    entry.remove_css_class("error");
                    editor.edit(index, |place| set(place, parsed));
                }
                // Deliberately not saved and deliberately not reverted: the
                // half-typed value stays visible so it can be corrected.
                _ => entry.add_css_class("error"),
            }
        });
        row
    }

    /// How many of the listed SSIDs must be visible to call it a match.
    ///
    /// Clamped to the number of SSIDs actually listed: `match_min` above that
    /// is a fingerprint that can never match, which today is a load-time
    /// `warn!` nobody reads. The floor is 1 rather than 0 because the matcher
    /// itself does `match_min.max(1)`, so offering 0 would silently mean 1.
    fn match_min_row(&self, index: usize, place: &Place) -> adw::SpinRow {
        let ceiling = to_f64(place.ssids.len().max(1));
        let row = adw::SpinRow::with_range(1.0, ceiling, 1.0);
        row.set_title("Networks that must match");
        row.set_subtitle("How many of the networks above have to be visible");
        row.set_value(to_f64(place.match_min.clamp(1, place.ssids.len().max(1))));
        row.set_sensitive(!place.ssids.is_empty());
        // Weakly, for the identity row's reason above.
        let editor = self.downgrade();
        row.connect_value_notify(move |row| {
            let Some(editor) = editor.upgrade() else {
                return;
            };
            if editor.syncing.get() {
                return;
            }
            let value = as_usize(row.value());
            editor.edit(index, |place| place.match_min = value);
        });
        row
    }

    /// Departures: the station id and the walk budget. The station is a plain
    /// entry — verifying that an id names the station you think it does needs
    /// the shell's transit agent, and is a later phase.
    fn departures_group(&self, index: usize, place: &Place) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("Departures")
            // Every string that reaches a `PreferencesGroup` description or an
            // `ActionRow` title/subtitle is parsed as Pango markup, so an
            // unescaped angle bracket makes the whole label fail to render (an
            // `Element "markup" was closed` warning and a blank description).
            // Hence "?query=" plus prose rather than a literal placeholder.
            .description(
                "Optional. Look an id up at https://v6.bvg.transport.rest/locations?query= plus \
                 the station name, and check it names the same station this place is called — \
                 the two silently drifting apart (#641) is what made this widget never work: the \
                 fetch succeeds against a real, nearby, wrong station and the board is empty \
                 forever. Leave the id blank for no departures here.",
            )
            .build();

        let station = adw::EntryRow::builder()
            .title("Station id")
            .text(place.station.clone().unwrap_or_default())
            .show_apply_button(true)
            .build();
        // Weakly, for the identity row's reason above.
        let editor = self.downgrade();
        station.connect_apply(move |entry| {
            let Some(editor) = editor.upgrade() else {
                return;
            };
            let text = entry.text().trim().to_string();
            editor.edit(index, |place| {
                place.station = (!text.is_empty()).then_some(text);
            });
        });
        group.add(&station);

        let walk = adw::SpinRow::with_range(0.0, MAX_WALK_MINUTES, 1.0);
        walk.set_title("Walk to the platform (minutes)");
        walk.set_subtitle(
            "Above 0, the list shows a leave-by countdown and fades trains you can \
                           no longer make",
        );
        walk.set_value(f64::from(place.walk_minutes));
        // Weakly, for the identity row's reason above.
        let editor = self.downgrade();
        walk.connect_value_notify(move |row| {
            let Some(editor) = editor.upgrade() else {
                return;
            };
            if editor.syncing.get() {
                return;
            }
            let value = as_u32(row.value());
            editor.edit(index, |place| place.walk_minutes = value);
        });
        group.add(&walk);
        group
    }

    /// One of the three string-list groups: an editable row per entry with a
    /// remove button, then a row that appends a new one.
    ///
    /// Every child here is an `adw::EntryRow`, i.e. a real `GtkListBoxRow` —
    /// `PreferencesGroup::add` routes anything else *below* the boxed list,
    /// outside the card and separator-less, which type-checks and renders
    /// wrong. `extra` is appended after the list for the same reason (the
    /// `match_min` spinner belongs to the fingerprint, not beside it).
    fn list_group(
        &self,
        index: usize,
        field: ListField,
        extra: Option<adw::SpinRow>,
    ) -> adw::PreferencesGroup {
        let (title, description, add_title) = field.labels();
        let group = adw::PreferencesGroup::builder()
            .title(title)
            .description(description)
            .build();

        let places = self.places();
        let items = places
            .get(index)
            .map(|p| field.get(p).clone())
            .unwrap_or_default();
        for (slot, item) in items.iter().enumerate() {
            let row = adw::EntryRow::builder()
                .title(format!("{} {}", field.item(), slot + 1))
                .text(item)
                .show_apply_button(true)
                .build();
            {
                // Weakly, for the identity row's reason above.
                let editor = self.downgrade();
                row.connect_apply(move |entry| {
                    let Some(editor) = editor.upgrade() else {
                        return;
                    };
                    let text = entry.text().trim().to_string();
                    editor.edit(index, |place| {
                        let list = field.get_mut(place);
                        // A blank edit means "remove", matching what the model
                        // does with one anyway (blanks are dropped on write).
                        // `slot` is indexed against the set this page was built
                        // from, which the mtime poll can have replaced since —
                        // so it is checked, not trusted.
                        if slot >= list.len() {
                        } else if text.is_empty() {
                            list.remove(slot);
                        } else {
                            list[slot] = text;
                        }
                    });
                    editor.reopen(index);
                });
            }
            let remove = gtk::Button::builder()
                .icon_name("list-remove-symbolic")
                .tooltip_text("Remove")
                .valign(gtk::Align::Center)
                .build();
            remove.add_css_class("flat");
            {
                // Weakly, for the identity row's reason above.
                let editor = self.downgrade();
                remove.connect_clicked(move |_| {
                    let Some(editor) = editor.upgrade() else {
                        return;
                    };
                    editor.edit(index, |place| {
                        let list = field.get_mut(place);
                        if slot < list.len() {
                            list.remove(slot);
                        }
                    });
                    editor.reopen(index);
                });
            }
            row.add_suffix(&remove);
            group.add(&row);
        }

        let add = adw::EntryRow::builder()
            .title(add_title)
            .show_apply_button(true)
            .build();
        // Weakly, for the identity row's reason above.
        let editor = self.downgrade();
        add.connect_apply(move |entry| {
            let Some(editor) = editor.upgrade() else {
                return;
            };
            let text = entry.text().trim().to_string();
            if text.is_empty() {
                return;
            }
            entry.set_text("");
            editor.edit(index, |place| field.get_mut(place).push(text));
            editor.reopen(index);
        });
        group.add(&add);

        if let Some(extra) = extra {
            group.add(&extra);
        }
        group
    }

    /// Rebuild the open detail page. Adding or removing a list entry changes
    /// how many rows the page has, and `match_min`'s ceiling with it, so the
    /// page is re-derived from the saved set rather than patched.
    ///
    /// Deferred to an idle tick because every caller is a widget *on* that
    /// page: popping it inline would tear the page down inside its own button's
    /// `clicked` (or entry's `apply`) emission, and it would race the push that
    /// immediately follows.
    fn reopen(&self, index: usize) {
        let editor = self.clone();
        glib::idle_add_local_once(move || {
            if editor.nav.pop() {
                editor.open(index);
            }
        });
    }

    /// Re-title the open detail page after a rename, so the header and the back
    /// button don't keep showing the old name.
    fn refresh_detail_title(&self, index: usize) {
        let places = self.places();
        let (Some(page), Some(place)) = (self.nav.visible_page(), places.get(index)) else {
            return;
        };
        page.set_title(&place.name);
    }

    /// Confirm before deleting — the only destructive action on this page, and
    /// the fingerprint behind a place can represent real effort to reconstruct.
    ///
    /// `adw::MessageDialog` rather than `AlertDialog`: libadwaita is pinned to
    /// `v1_4` here, and `AlertDialog` is 1.5.
    fn confirm_delete(&self, anchor: &gtk::Button, index: usize, name: &str) {
        let parent = anchor.root().and_downcast::<gtk::Window>();
        let dialog = adw::MessageDialog::new(
            parent.as_ref(),
            Some(&format!("Delete \u{201c}{name}\u{201d}?")),
            Some(
                "Its coordinates, Wi-Fi fingerprint and departures settings are removed from \
                 places.toml. Comments you wrote around the entry go with it.",
            ),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("delete", "Delete");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        // Weakly (#1384 item 2): the dialog is transient-for the window, not
        // owned by `nav`, but it is still a widget on the pushed detail page's
        // own tree (`anchor.root()` above resolves through it), so a strong
        // `Editor` here carries `nav` right back the same way.
        let editor = self.downgrade();
        dialog.connect_response(None, move |_, response| {
            if response != "delete" {
                return;
            }
            let Some(editor) = editor.upgrade() else {
                return;
            };
            let mut next = editor.places();
            if index < next.len() {
                next.remove(index);
            }
            if editor.save(next) {
                while editor.nav.pop() {}
            }
        });
        dialog.present();
    }
}

/// [`Editor`] with its widget handles held **weakly** — what a widget's own
/// handler captures, so a row cannot keep the tab it belongs to alive forever
/// (#1384 item 2; `plugins_tab`'s `WeakPluginsState` is the same fix on the
/// same shape).
///
/// GTK owns a signal handler for as long as it owns the widget it is
/// connected to, so a handler that captures a strong [`Editor`] closes a
/// cycle wherever that `Editor` carries a widget the handler's own widget
/// descends from: `list` is the direct parent of every place row (and the
/// "Add a place" row), `nav` of every pushed detail page and every widget on
/// it, `toasts` of the one toast that shows a button, and
/// `departures_endpoint_row`/`auto_switch` are their own handler's widget.
/// Once that loop closes it has no external anchor left to break it — the
/// same shape `collection.rs`'s module doc works through for
/// `MapPage`/`ListPage`/`RecordsPage`, one level up: there, the page's own
/// `Rc` holds the widget that (via a row's closure) holds it back; here, this
/// tab's `toasts`/`nav`/`list`/etc. are never behind an `Rc` of their own, but
/// a widget descending from one of them holding a *strong* `Editor` closes
/// the identical loop through the `GObject` the field names.
///
/// The `Rc` cells are cloned strongly, as `WeakPluginsState`'s are and for
/// the same reason: none of them is a widget, so none can be an ancestor of
/// one — except `rows`, whose *content* is widgets, which is why its own
/// field type carries a `WeakRef` per entry rather than the widget itself
/// (#1384 item 2 review). A place row's own `connect_activated` closure
/// captures this whole struct, `rows` included: a **strong** entry would
/// have `rows` hold the very row whose closure holds `rows`, which is the
/// identical self-sustaining loop the widget fields above are guarded
/// against, just one field deeper — `rebuild`'s own `self.rows.take()` frees
/// stale entries only on the *next* rebuild, which a tab that is simply
/// closed never gets, so nothing else would ever break that one.
#[derive(Clone)]
struct WeakEditor {
    base: Rc<RefCell<Vec<Place>>>,
    locked: Rc<Cell<Locks>>,
    departures_endpoint_row: glib::WeakRef<adw::EntryRow>,
    endpoint: Rc<RefCell<Option<String>>>,
    nav: glib::WeakRef<adw::NavigationView>,
    list: glib::WeakRef<adw::PreferencesGroup>,
    rows: Rc<RefCell<Vec<glib::WeakRef<gtk::Widget>>>>,
    resolved: Rc<RefCell<Option<String>>>,
    status_row: glib::WeakRef<adw::ActionRow>,
    auto_switch: glib::WeakRef<adw::SwitchRow>,
    toasts: glib::WeakRef<adw::ToastOverlay>,
    syncing: Rc<Cell<bool>>,
}

impl Editor {
    /// The handler-side view of this state — what every widget-owned closure
    /// captures instead of `self.clone()`.
    fn downgrade(&self) -> WeakEditor {
        WeakEditor {
            base: self.base.clone(),
            locked: self.locked.clone(),
            departures_endpoint_row: self.departures_endpoint_row.downgrade(),
            endpoint: self.endpoint.clone(),
            nav: self.nav.downgrade(),
            list: self.list.downgrade(),
            rows: self.rows.clone(),
            resolved: self.resolved.clone(),
            status_row: self.status_row.downgrade(),
            auto_switch: self.auto_switch.downgrade(),
            toasts: self.toasts.downgrade(),
            syncing: self.syncing.clone(),
        }
    }
}

impl WeakEditor {
    /// Rebuild the strong state for the duration of one callback, or `None`
    /// once the tab has been torn down — in which case there is nothing to
    /// update and the handler returns. All-or-nothing, `WeakPluginsState`'s
    /// own reasoning: these widgets live and die as one tree, so a partial
    /// upgrade would mean a torn tab, not a case worth handling.
    fn upgrade(&self) -> Option<Editor> {
        Some(Editor {
            base: self.base.clone(),
            locked: self.locked.clone(),
            departures_endpoint_row: self.departures_endpoint_row.upgrade()?,
            endpoint: self.endpoint.clone(),
            nav: self.nav.upgrade()?,
            list: self.list.upgrade()?,
            rows: self.rows.clone(),
            resolved: self.resolved.clone(),
            status_row: self.status_row.upgrade()?,
            auto_switch: self.auto_switch.upgrade()?,
            toasts: self.toasts.upgrade()?,
            syncing: self.syncing.clone(),
        })
    }
}

/// The weather-location override (#391), preserved verbatim from the tab this
/// one replaces — and demoted, retitled and described for what it actually is.
///
/// It shares this tab because it answers the same question ("where does the
/// shell think I am?") and splitting it onto a page of its own would leave two
/// tabs both meaning some flavour of "place", which is the conflation #640
/// filed. Keeping it adjacent under wording that spells out the difference is
/// what fixes it: this steers **only** the weather widget, it is session-only,
/// and it has nothing to do with the `places.toml` entries above.
///
/// That session-only limitation is triage option (B) on #640 and is
/// deliberately *not* fixed here — persisting it is a `geoclue.rs` change this
/// tab doesn't touch. What this does is make it visible instead of silent.
/// Build the departures backend group (#1124): a free-form entry for
/// `[departures].endpoint` — a short name or a full URL — following the same
/// plain-entry shape `station`'s own row uses. The row itself
/// (`editor.departures_endpoint_row`) and its apply handler are built by the
/// caller ([`build_page`]), alongside `Editor`'s own construction; this
/// function only lays it out in its own titled group.
fn build_departures_endpoint_group(editor: &Editor) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Departures backend")
        .description(
            "Which transport.rest deployment the departures widget fetches from. Leave blank \
             for bvg (Berlin). Short names: bvg, vbb, db — or paste a full https://... base URL \
             for another transport.rest deployment. Not per-place: the whole shell fetches from \
             one backend. VBB and BVG share the VBB station id space; DB uses its own EVA ids, \
             so switching usually means finding a new station id from the new backend's own \
             /locations?query= route.",
        )
        .build();
    group.add(&editor.departures_endpoint_row);
    group
}

fn build_override_group(editor: &Editor) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Weather location override")
        .description(
            "Overrides the resolved place for the weather widget only — the places above are \
             unaffected. Automatic uses GeoClue; manual forward-geocodes a city you name. \
             Session-only: the shell reverts to automatic when it restarts.",
        )
        .build();
    group.add(&editor.auto_switch);

    let city = adw::EntryRow::builder()
        .title("Set city manually")
        .show_apply_button(true)
        .build();
    {
        // Weakly (#1384 item 2): `city` is a permanent part of the root
        // page, itself a child of `nav` — a strong `Editor` here carries
        // `nav` right back.
        let editor = editor.downgrade();
        city.connect_apply(move |entry| {
            let Some(editor) = editor.upgrade() else {
                return;
            };
            let city = entry.text().trim().to_owned();
            if city.is_empty() {
                return;
            }
            spawn_on_runtime(crate::set_manual_city(city), move |res| {
                if let Err(err) = res {
                    tracing::info!(%err, "SetManualCity failed");
                }
                editor.refresh_place_soon();
            });
        });
    }
    group.add(&city);
    group
}

/// Build the **Places** tab, and the poll timer that keeps it honest against
/// out-of-band edits. The caller ties the timer to the window so a closed
/// window stops polling (#542).
pub(crate) fn build_page() -> (adw::ToastOverlay, glib::SourceId) {
    let nav = adw::NavigationView::new();
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&nav));

    let status = adw::PreferencesGroup::new();
    let status_row = adw::ActionRow::builder()
        .title("Current place")
        .subtitle("Resolving…")
        .build();
    status_row.add_prefix(&gtk::Image::from_icon_name("mark-location-symbolic"));
    status.add(&status_row);

    let list = adw::PreferencesGroup::builder()
        .title("Places")
        .description(LIST_DESCRIPTION)
        .build();

    // One read for the list, the endpoint and the lock set — `load_places` on
    // top of it only for its first-run seed, which `load_layered` deliberately
    // does not do.
    let seeded = places::load_places();
    let loaded = places::load_layered();
    let locks = Locks::of(&loaded);
    let departures_endpoint_row = adw::EntryRow::builder()
        .title("Endpoint")
        .text(loaded.endpoint.clone().unwrap_or_default())
        .show_apply_button(true)
        .sensitive(!locks.endpoint)
        .build();

    let editor = Editor {
        base: Rc::new(RefCell::new(seeded)),
        locked: Rc::new(Cell::new(locks)),
        endpoint: Rc::new(RefCell::new(loaded.endpoint.clone())),
        departures_endpoint_row: departures_endpoint_row.clone(),
        nav: nav.clone(),
        list: list.clone(),
        rows: Rc::new(RefCell::new(Vec::new())),
        resolved: Rc::new(RefCell::new(None)),
        status_row,
        // Default to "auto" so the pre-connection state matches the shell
        // default; `GetPlace` corrects it once the shell answers.
        auto_switch: adw::SwitchRow::builder()
            .title("Automatic location")
            .subtitle("Detect your location automatically (GeoClue)")
            .active(true)
            .build(),
        toasts: toasts.clone(),
        syncing: Rc::new(Cell::new(false)),
    };
    editor.rebuild();

    // The departures backend entry → `save_departures_endpoint`; blank means
    // "use the default" (#1124), the same convention `station`'s row uses.
    {
        // Weakly (#1384 item 2): this row's own handler carries `Editor`,
        // which carries `departures_endpoint_row` right back — a self-loop
        // with no external anchor to break it, the same shape `WeakEditor`'s
        // own doc names.
        let editor = editor.downgrade();
        departures_endpoint_row.connect_apply(move |entry| {
            if let Some(editor) = editor.upgrade() {
                editor.save_departures_endpoint(endpoint_from_entry(&entry.text()).as_deref());
            }
        });
    }

    // Auto/manual toggle → SetAutoLocation, then re-read the resolved place.
    {
        // Weakly, for the departures-endpoint row's reason above —
        // `auto_switch` is its own handler's widget too.
        let handler = editor.downgrade();
        editor.auto_switch.connect_active_notify(move |sw| {
            let Some(handler) = handler.upgrade() else {
                return;
            };
            if handler.syncing.get() {
                return;
            }
            spawn_on_runtime(crate::set_auto_location(sw.is_active()), move |res| {
                if let Err(err) = res {
                    tracing::info!(%err, "SetAutoLocation failed");
                }
                handler.refresh_place_soon();
            });
        });
    }

    let page = adw::PreferencesPage::new();
    page.add(&status);
    page.add(&list);
    page.add(&build_departures_endpoint_group(&editor));
    page.add(&build_override_group(&editor));
    nav.add(&adw::NavigationPage::new(&page, "Places"));

    editor.refresh_place();

    // Live-follow an `$EDITOR` save. `ConfigWatcher` is mtime-gated *and*
    // content-checked, so our own writes — which move the mtime — don't churn
    // a rebuild, and a `touch` doesn't either.
    let poll = {
        let editor = editor.clone();
        let mut watcher = places::ConfigWatcher::new();
        glib::timeout_add_local(CONFIG_POLL_INTERVAL, move || {
            // `moved()`, not `poll(&current)` (#1338 review, H2): the watcher
            // stamps the nix base layer too since #1227 item 2, and the most
            // likely thing a `nixos-rebuild` changed is the **lock** — which
            // `poll`'s list dedup cannot see, because a lock is not a list.
            // `refresh_from_disk` does the comparing, over everything the tab
            // actually renders.
            if watcher.moved() && editor.refresh_from_disk() {
                while editor.nav.pop() {}
                editor.rebuild();
            }
            glib::ControlFlow::Continue
        })
    };

    (toasts, poll)
}

/// The list row's subtitle: coordinates, then whichever of the two detection
/// inputs and the departures config are actually set. Written so the row says
/// *why* a place would or wouldn't match, which is the question someone opens
/// this tab with.
fn summarize(place: &Place) -> String {
    let mut parts = vec![format!("{:.4}, {:.4}", place.lat, place.lon)];
    parts.push(match place.ssids.len() {
        0 => format!("no fingerprint · {:.0} km radius", place.radius_km),
        1 => "1 network".to_string(),
        n => format!("{} of {n} networks", place.match_min.clamp(1, n)),
    });
    if let Some(station) = &place.station {
        let filter = place.lines.len() + place.directions.len();
        parts.push(if filter == 0 {
            format!("station {station}")
        } else {
            format!("station {station} ({filter} filters)")
        });
    }
    parts.join(" · ")
}

/// A place name not already taken, for "Add a place".
///
/// Names are unique case-insensitively (they are the identity the rest of the
/// system addresses a place by), so the obvious `"New place"` collides the
/// second time. Suffixing rather than failing keeps the button always usable.
fn unused_name(places: &[Place]) -> String {
    let taken = |candidate: &str| {
        places
            .iter()
            .any(|p| p.name.trim().eq_ignore_ascii_case(candidate))
    };
    if !taken("New place") {
        return "New place".to_string();
    }
    // Bounded by construction: at most `places.len()` of the candidates can be
    // taken, so the first free one is inside this range whatever the set holds.
    (2..=places.len() + 2)
        .map(|n| format!("New place {n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| "New place".to_string())
}

/// A count as a spinner value.
fn to_f64(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// A spinner value as a count.
///
/// The cast is a clamp, not a truncation: every caller's `SpinRow` is
/// constructed with an integral step over a range this crate chose, so the
/// value is already a small non-negative whole number, and the guards below
/// make the conversion total regardless.
///
/// NaN is checked *first* and explicitly. `f64::min` returns the non-NaN
/// operand, so a NaN falling through to the clamp would come back as the
/// ceiling — the largest possible value, from the least meaningful input.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn as_usize(value: f64) -> usize {
    let value = value.round();
    if value.is_nan() || value <= 0.0 {
        0
    } else {
        value.min(MAX_SSIDS) as usize
    }
}

/// A spinner value as a minute count — see [`as_usize`] for why the cast is
/// total and why NaN is checked first.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn as_u32(value: f64) -> u32 {
    let value = value.round();
    if value.is_nan() || value <= 0.0 {
        0
    } else {
        value.min(MAX_WALK_MINUTES) as u32
    }
}

/// What the departures-endpoint entry (#1124) hands back, normalized: blank
/// means "use the default" (`None`) rather than an empty string, the same
/// convention `station`'s row uses. `places::save_departures_endpoint`
/// validates the non-blank case (an unknown short name or a non-URL string is
/// rejected there, surfaced as a toast) — this is purely the text-box-to-value
/// mapping, kept as its own function so it's unit-testable independently of a
/// live `EntryRow`.
fn endpoint_from_entry(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(name: &str) -> Place {
        Place::new(name, 52.4556, 13.5085)
    }

    #[test]
    fn summary_reports_why_a_place_would_match() {
        // No fingerprint: the radius is the only thing that can match, so say
        // so rather than showing "0 networks".
        let bare = place("Home");
        assert_eq!(
            summarize(&bare),
            "52.4556, 13.5085 · no fingerprint · 12 km radius"
        );

        let mut fingerprinted = place("Home");
        fingerprinted.ssids = vec!["a".into(), "b".into(), "c".into()];
        fingerprinted.match_min = 2;
        assert_eq!(
            summarize(&fingerprinted),
            "52.4556, 13.5085 · 2 of 3 networks"
        );
    }

    #[test]
    fn summary_reports_the_station_and_whether_it_is_filtered() {
        let mut with_station = place("Home");
        with_station.station = Some("900192001".into());
        assert!(summarize(&with_station).ends_with("· station 900192001"));

        with_station.lines = vec!["S8".into(), "S85".into()];
        with_station.directions = vec!["Spandau".into()];
        assert!(summarize(&with_station).ends_with("· station 900192001 (3 filters)"));
    }

    /// An unsatisfiable `match_min` (more than there are networks) is a
    /// fingerprint that can never match. The editor clamps it, and the summary
    /// must not advertise the impossible number in the meantime.
    #[test]
    fn summary_clamps_an_unsatisfiable_match_min() {
        let mut broken = place("Home");
        broken.ssids = vec!["a".into()];
        broken.match_min = 5;
        assert_eq!(summarize(&broken), "52.4556, 13.5085 · 1 network");
    }

    #[test]
    fn new_place_names_dodge_the_ones_already_taken() {
        assert_eq!(unused_name(&[]), "New place");
        assert_eq!(unused_name(&[place("Home")]), "New place");
        assert_eq!(unused_name(&[place("New place")]), "New place 2");
        // Names are unique case-insensitively and trimmed, so the check has to
        // be too — otherwise "Add a place" proposes a name the model rejects.
        assert_eq!(
            unused_name(&[place("  new PLACE  "), place("New Place 2")]),
            "New place 3"
        );
    }

    #[test]
    fn spinner_conversions_are_total() {
        assert_eq!(as_usize(3.0), 3);
        assert_eq!(as_usize(2.6), 3);
        assert_eq!(as_usize(-1.0), 0);
        assert_eq!(as_usize(f64::MAX), 4096);
        assert_eq!(as_u32(0.0), 0);
        assert_eq!(as_u32(10.4), 10);
        assert_eq!(as_u32(f64::MAX), 120);
        assert!((to_f64(7) - 7.0).abs() < f64::EPSILON);
        // NaN must floor, not ceiling. `f64::min` returns the non-NaN operand,
        // so a NaN reaching the clamp would come back as the *maximum* — a
        // 120-minute walk budget out of a meaningless input.
        assert_eq!(as_u32(f64::NAN), 0);
        assert_eq!(as_usize(f64::NAN), 0);
        assert_eq!(as_u32(f64::NEG_INFINITY), 0);
        assert_eq!(as_usize(f64::INFINITY), 4096);
    }

    #[test]
    fn endpoint_entry_blank_means_default() {
        assert_eq!(endpoint_from_entry(""), None);
        assert_eq!(endpoint_from_entry("   "), None);
    }

    #[test]
    fn endpoint_entry_trims_a_configured_value() {
        assert_eq!(endpoint_from_entry("  vbb  "), Some("vbb".to_owned()));
        assert_eq!(
            endpoint_from_entry("https://v6.hvv.transport.rest"),
            Some("https://v6.hvv.transport.rest".to_owned())
        );
    }
}

/// The window-controls-duplication regression for the Places tab's pushed
/// detail page (#944, `#943`'s residual #2) — the same shape as
/// `plugins_tab`'s `the_tab_draws_no_window_controls`, needing the same
/// `system-tests` gate for the same reason (geometry/mapped-state assertions
/// need a real display).
///
/// Drives [`Editor`] directly with one fabricated place rather than going
/// through [`build_page`]: that function calls
/// [`hytte_config::places::load_places`], which reads (and, on a missing
/// file, *writes*) the real `$HOME/.config/trollshell/places.toml` — a test
/// process must never touch a user's actual config file. `Editor::open` is
/// exactly the method under test (it builds the pushed page, header bar and
/// all), and it only ever reads `self.base`, so seeding that directly is
/// enough.
#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use adw::prelude::*;
    use gtk::glib;
    use hytte_config::places::Place;

    use super::{Editor, Locks};

    /// Run the GTK main loop until it has nothing left to dispatch, so a
    /// queued push/allocation actually happens.
    fn pump() {
        while glib::MainContext::default().iteration(false) {}
    }

    /// [`build_editor_with`] with nothing locked — the ordinary case, and what
    /// every test written before #1227 item 2 wants.
    fn build_editor() -> (adw::ToastOverlay, Editor) {
        build_editor_with(Locks::default())
    }

    /// Build the Places tab's `Editor` around one fabricated place, with no
    /// file I/O and no `Control` call — see the module doc above for why this
    /// doesn't call [`super::build_page`].
    ///
    /// `locks` is injected rather than read from the environment for exactly
    /// the reason the module doc gives for seeding `base` directly:
    /// [`Locks::read`] goes through `hytte_config::places::load_layered`, which
    /// resolves the **real** `$HOME` and `$XDG_CONFIG_DIRS`, and a test process
    /// must not depend on (or be perturbed by) whether the developer's own box
    /// declares `programs.trollshell.config.places`. What `Locks::read` itself
    /// answers is pinned in `hytte-config` (`places_are_locked` /
    /// `endpoint_is_locked`); what this file owns is the wiring, and that is
    /// what these tests drive.
    fn build_editor_with(locks: Locks) -> (adw::ToastOverlay, Editor) {
        let nav = adw::NavigationView::new();
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&nav));

        let list = adw::PreferencesGroup::new();
        let editor = Editor {
            base: Rc::new(RefCell::new(vec![Place::new("Home", 52.4556, 13.5085)])),
            locked: Rc::new(Cell::new(locks)),
            endpoint: Rc::new(RefCell::new(None)),
            // No file I/O here either (see the doc comment above) — a detached
            // row, never read back.
            departures_endpoint_row: adw::EntryRow::builder().title("Endpoint").build(),
            nav: nav.clone(),
            list: list.clone(),
            rows: Rc::new(RefCell::new(Vec::new())),
            resolved: Rc::new(RefCell::new(None)),
            status_row: adw::ActionRow::builder().title("Current place").build(),
            auto_switch: adw::SwitchRow::builder()
                .title("Automatic location")
                .build(),
            toasts: toasts.clone(),
            syncing: Rc::new(Cell::new(false)),
        };
        editor.rebuild();

        let page = adw::PreferencesPage::new();
        page.add(&list);
        nav.add(&adw::NavigationPage::new(&page, "Places"));

        (toasts, editor)
    }

    /// #1384 item 2: dropping the tab actually frees the `Editor`'s state.
    ///
    /// Before this, every widget-owned closure (`list`'s own place rows, the
    /// pushed detail page's rows, `departures_endpoint_row`'s and
    /// `auto_switch`'s own handlers, the "changed on disk" toast's button)
    /// captured a strong `Editor` — and since `Editor` itself carries `list`/
    /// `nav`/`departures_endpoint_row`/`auto_switch`/`toasts`, each of those
    /// closures held a strong reference back to a widget that (directly or
    /// transitively) owns it. That loop has no external anchor once `toasts`
    /// (returned to the caller) and this test's own `editor` handle are both
    /// dropped, so nothing ever reached refcount zero — the same shape
    /// `collection.rs`'s module doc works through for `MapPage`/`ListPage`/
    /// `RecordsPage`, one level up.
    ///
    /// **Red if any capture converted to `self.downgrade()`/
    /// `editor.downgrade()` in this file goes back to a strong `.clone()`**:
    /// that one widget's own handler alone reopens the loop through the
    /// field it carries, and `base`'s `Rc::strong_count` below never falls to
    /// one.
    #[gtk::test]
    fn dropping_the_tab_frees_the_editor() {
        adw::init().expect("libadwaita init");
        let (toasts, editor) = build_editor();

        // A `WeakRef` to a place row itself — #1384 item 2's second finding:
        // `rows`' own entries must not be held strongly either, or a row's
        // own closure (capturing `WeakEditor`, which carries `rows`) reaches
        // back through `rows`' content to the very row that closure is on.
        let row_weak = editor.rows.borrow()[0].clone();

        // Exercise a pushed detail page and its own rows too, not just the
        // root list — those rows' closures carry `nav` back just as strongly.
        editor.open(0);
        pump();
        while editor.nav.pop() {}
        pump();

        let base = Rc::clone(&editor.base);
        drop(editor);
        drop(toasts);
        pump();

        assert_eq!(
            Rc::strong_count(&base),
            1,
            "every widget-owned closure should have dropped its captured Editor by now"
        );
        assert!(
            row_weak.upgrade().is_none(),
            "a place row is still reachable after the tab was dropped"
        );
    }

    /// Every `GtkWindowControls` under `root`, at any depth — the same walk
    /// `plugins_tab`'s test of the same name uses. Kept file-local rather than
    /// shared: it's a few lines of private tree-walking and each test module
    /// already owns its own fixtures (`build_editor` above vs. `build_tab`
    /// over there).
    fn window_controls_under(root: &gtk::Widget) -> Vec<gtk::WindowControls> {
        let mut found = Vec::new();
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Ok(controls) = widget.clone().downcast::<gtk::WindowControls>() {
                found.push(controls);
            }
            found.extend(window_controls_under(&widget));
            child = widget.next_sibling();
        }
        found
    }

    /// Mounted the way `crate::build_window` mounts every tab: inside an
    /// `AdwApplicationWindow` at the real 760 × 560 default, under an
    /// `AdwToolbarView` whose top bar is the app's own header bar (with a view
    /// switcher, standing in for the real one's three tabs). That context is
    /// the entire bug (#943/#944): a second `AdwHeaderBar` left on its
    /// defaults draws its own `GtkWindowControls` inside a window that
    /// already has one from the app's own header.
    ///
    /// The root "Places" list page carries no header bar of its own at all
    /// (see the module doc), so only the **pushed** per-place detail page can
    /// be the offender — hence `editor.open(0)` before the assertions.
    ///
    /// Falsified by reverting `Editor::detail`'s header back to
    /// `adw::HeaderBar::new()`: the mutation reports a duplicate
    /// `GtkWindowControls` cluster **108 px** wide, not 0 — this asserts
    /// presence-and-mapped rather than width regardless, because presence is
    /// the stronger claim (a narrower or differently-sized duplicate is still
    /// a duplicate) — same reasoning `plugins_tab`'s sibling test documents.
    #[gtk::test]
    fn the_tab_draws_no_window_controls() {
        adw::init().expect("libadwaita init");
        let (bin, editor) = build_editor();

        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(&bin, Some("places"), "Places", "mark-location-symbolic");
        let switcher = adw::ViewSwitcher::builder()
            .stack(&stack)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build();
        let header = adw::HeaderBar::builder().title_widget(&switcher).build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&stack));
        let window = adw::ApplicationWindow::builder()
            .title("trollshell Control Center")
            .default_width(760)
            .default_height(560)
            .content(&toolbar)
            .build();
        window.present();
        pump();

        // Push the one place's detail page — after the window is already up,
        // matching how a real click would arrive once the app is on screen.
        editor.open(0);
        pump();

        let app_controls: Vec<gtk::WindowControls> = window_controls_under(header.upcast_ref())
            .into_iter()
            .filter(gtk::prelude::WidgetExt::is_mapped)
            .collect();
        assert!(
            !app_controls.is_empty(),
            "the window's own header bar must keep its controls — without them this test cannot \
             tell 'no controls in the tab' from 'no controls anywhere'"
        );

        let stray: Vec<gtk::WindowControls> = window_controls_under(bin.upcast_ref())
            .into_iter()
            .filter(gtk::prelude::WidgetExt::is_mapped)
            .collect();
        assert!(
            stray.is_empty(),
            "the Places tab's pushed detail page draws {} mapped GtkWindowControls of its own \
             (first is {}px wide) — a second close/minimise/maximise cluster inside the window",
            stray.len(),
            stray.first().map_or(0, gtk::prelude::WidgetExt::width)
        );

        // The one start-side control the pushed page's header bar *should*
        // grow despite `show_start_title_buttons(false)`: the navigation back
        // button, `AdwNavigationView`'s own business and not the title
        // buttons' — the same pairing `plugins_tab`'s sibling test pins
        // (#945 review, finding 4).
        let back = has_mapped_back_button(&editor.nav.visible_page().expect("a pushed page"));
        assert!(
            back,
            "turning the title buttons off must not cost the pushed page's back button"
        );

        window.set_content(None::<&gtk::Widget>);
        window.destroy();
        pump();
    }

    /// Whether `page`'s subtree has a mapped `go-previous-symbolic` button —
    /// what `AdwHeaderBar` grows inside a navigation stack. The same walk
    /// `plugins_tab`'s test of the same name uses; kept file-local for the
    /// reason `window_controls_under` above is.
    fn has_mapped_back_button(page: &adw::NavigationPage) -> bool {
        fn walk(root: &gtk::Widget) -> bool {
            let mut child = root.first_child();
            while let Some(widget) = child {
                if let Ok(button) = widget.clone().downcast::<gtk::Button>()
                    && button.icon_name().as_deref() == Some("go-previous-symbolic")
                    && button.is_mapped()
                {
                    return true;
                }
                if walk(&widget) {
                    return true;
                }
                child = widget.next_sibling();
            }
            false
        }
        walk(page.upcast_ref())
    }

    /// Every widget under `root` of type `W`, at any depth — the generic form
    /// of the two hand-rolled walks above, added for #1227 item 2's assertions
    /// (a page's sensitivity, a banner's presence) rather than a third copy.
    fn descendants<W: glib::object::IsA<gtk::Widget>>(root: &gtk::Widget) -> Vec<W> {
        let mut found = Vec::new();
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Ok(matched) = widget.clone().downcast::<W>() {
                found.push(matched);
            }
            found.extend(descendants::<W>(&widget));
            child = widget.next_sibling();
        }
        found
    }

    /// #1227 item 2: when nix owns the `[[place]]` array, the tab says so and
    /// offers nothing that would be refused.
    ///
    /// Three claims, each the consequence of a different line in `rebuild`
    /// and `detail`, and each with the unlocked control asserted beside it so
    /// a mutation that simply stopped building anything cannot pass:
    ///
    /// 1. no "Add a place" row (`rows` holds one widget per place and nothing
    ///    else) — an overlay array would replace nix's whole and then be
    ///    refused, so there is nothing to add to;
    /// 2. the list's description is the "Set in nix" sentence naming the
    ///    option, not the "saved straight to ~/.config" one;
    /// 3. a place's detail page still opens (reading a nix-set place is the
    ///    point of having it on screen) but is **insensitive**, and carries the
    ///    banner.
    #[gtk::test]
    fn a_nix_locked_place_list_is_shown_read_only() {
        adw::init().expect("libadwaita init");

        let (_open, unlocked) = build_editor();
        assert_eq!(
            unlocked.rows.borrow().len(),
            2,
            "one place row plus the 'Add a place' row"
        );
        assert_eq!(
            unlocked.list.description().as_deref(),
            Some(super::LIST_DESCRIPTION)
        );

        let (_toasts, editor) = build_editor_with(Locks {
            places: true,
            endpoint: false,
        });

        assert_eq!(
            editor.rows.borrow().len(),
            1,
            "the place row only — 'Add a place' must not be offered while the array is nix's"
        );
        assert_eq!(
            editor.list.description().as_deref(),
            Some(super::NIX_MANAGED_PLACES),
            "the list has to say whose it is, right where the list is"
        );

        editor.open(0);
        pump();
        let page = editor
            .nav
            .visible_page()
            .expect("the detail page was pushed");
        let prefs: Vec<adw::PreferencesPage> = descendants(page.upcast_ref());
        assert_eq!(prefs.len(), 1, "one AdwPreferencesPage per detail page");
        assert!(
            !prefs[0].is_sensitive(),
            "every control on a nix-owned place must be insensitive — the merge keeps the nix \
             value on the next load, so an editable row would be silently reverted"
        );
        assert_eq!(
            descendants::<adw::Banner>(page.upcast_ref()).len(),
            1,
            "…and the page says why"
        );
        assert!(
            descendants::<gtk::Button>(page.upcast_ref())
                .iter()
                .all(|b| b.icon_name().as_deref() != Some("user-trash-symbolic")),
            "no Delete either: the whole array is nix's"
        );
    }

    /// The control for the test above, as its own case so a mutation that
    /// simply made *everything* read-only is caught rather than absorbed: with
    /// nothing locked the detail page is sensitive and keeps its Delete button.
    #[gtk::test]
    fn an_unlocked_place_keeps_its_editable_detail_page() {
        adw::init().expect("libadwaita init");

        let (_toasts, editor) = build_editor();
        editor.open(0);
        pump();

        let page = editor
            .nav
            .visible_page()
            .expect("the detail page was pushed");
        let prefs: Vec<adw::PreferencesPage> = descendants(page.upcast_ref());
        assert_eq!(prefs.len(), 1);
        assert!(prefs[0].is_sensitive());
        assert!(
            descendants::<adw::Banner>(page.upcast_ref()).is_empty(),
            "nothing to announce when the set is the operator's own"
        );
        assert!(
            descendants::<gtk::Button>(page.upcast_ref())
                .iter()
                .any(|b| b.icon_name().as_deref() == Some("user-trash-symbolic")),
            "Delete is there when the place is yours"
        );
    }

    /// #1338 review, H2, the GTK half: a lock that appears or vanishes without
    /// moving the place list has to reach the running window.
    ///
    /// This drives the poll closure's body — `apply_view`, which is what
    /// `refresh_from_disk` hands the freshly-read view to — with the *same*
    /// list and a changed lock, in both directions, and asserts both that it
    /// reports the change and that a rebuild then renders the other surface.
    /// Before the fix the poll sat behind `ConfigWatcher::poll`, whose dedup
    /// is on the list, so neither direction ever got here at all.
    ///
    /// **Mutation:** drop `|| self.locked.get() != locks` from `apply_view`'s
    /// `changed` and this reds on the first assertion.
    #[gtk::test]
    fn a_lock_that_changes_without_moving_the_list_re_renders_the_tab() {
        adw::init().expect("libadwaita init");
        let (_toasts, editor) = build_editor();
        let same_list = editor.places();

        // The lock appears. Not one place moved.
        assert!(
            editor.apply_view(
                same_list.clone(),
                None,
                Locks {
                    places: true,
                    endpoint: false,
                },
            ),
            "a lock that appears is a change the tab must render"
        );
        editor.rebuild();
        assert_eq!(
            editor.rows.borrow().len(),
            same_list.len(),
            "…and 'Add a place' is gone"
        );
        assert_eq!(
            editor.list.description().as_deref(),
            Some(super::NIX_MANAGED_PLACES)
        );

        // Idempotent: the same view again is not a change.
        assert!(
            !editor.apply_view(
                same_list.clone(),
                None,
                Locks {
                    places: true,
                    endpoint: false,
                },
            ),
            "nothing moved, so nothing to re-render — otherwise the poll would \
             pop the user back to the root list every two seconds"
        );

        // And the lock vanishes again — the `nixos-rebuild` that drops the
        // option while the overlay already holds the same list.
        assert!(editor.apply_view(same_list.clone(), None, Locks::default()));
        editor.rebuild();
        assert_eq!(
            editor.rows.borrow().len(),
            same_list.len() + 1,
            "the greyed row must not outlive the option that greyed it"
        );
        assert_eq!(
            editor.list.description().as_deref(),
            Some(super::LIST_DESCRIPTION)
        );
    }

    /// The endpoint half of the same defect, and the common one: a
    /// `[departures]`-only base layer never renders a `[[place]]`, so adding it
    /// moves no place at all — and the row it governs is the one left
    /// deliberately sensitive while the list is nix's.
    ///
    /// Also L3: the row's *sensitivity* is now applied wherever the view is,
    /// which is what `apply_view` being the single site buys.
    #[gtk::test]
    fn an_endpoint_lock_flips_the_rows_sensitivity_without_touching_the_list() {
        adw::init().expect("libadwaita init");
        let (_toasts, editor) = build_editor();
        let same_list = editor.places();
        assert!(editor.departures_endpoint_row.is_sensitive());

        assert!(editor.apply_view(
            same_list.clone(),
            Some("vbb".to_owned()),
            Locks {
                places: false,
                endpoint: true,
            },
        ));
        assert!(
            !editor.departures_endpoint_row.is_sensitive(),
            "the backend is nix's now"
        );
        assert_eq!(
            editor.departures_endpoint_row.text(),
            "vbb",
            "…and showing nix's value, not the stale one"
        );
        assert_eq!(
            editor.places(),
            same_list,
            "not one place moved, which is why `poll` could not see this"
        );

        assert!(editor.apply_view(same_list, Some("vbb".to_owned()), Locks::default()));
        assert!(editor.departures_endpoint_row.is_sensitive());
    }

    /// The refresh now runs on **every** layer move rather than only when the
    /// list changed, which puts it in reach of a row the operator is still
    /// typing into. A draft must survive an unrelated refresh — otherwise the
    /// backend they are halfway through entering is replaced by the file's two
    /// seconds after they started.
    ///
    /// **Mutation:** compare `self.departures_endpoint_row.text()` instead of
    /// `*self.endpoint.borrow()` in `apply_view` and this reds.
    #[gtk::test]
    fn a_half_typed_endpoint_survives_an_unrelated_refresh() {
        adw::init().expect("libadwaita init");
        let (_toasts, editor) = build_editor();
        editor.apply_view(editor.places(), Some("bvg".to_owned()), Locks::default());

        // The operator starts typing a new backend and has not pressed apply.
        editor.departures_endpoint_row.set_text("vb");

        // Something else moves a layer — their own place save, say — and the
        // poll refreshes. The file still says `bvg`.
        let mut next = editor.places();
        next.push(Place::new("Office", 1.0, 2.0));
        assert!(
            editor.apply_view(next, Some("bvg".to_owned()), Locks::default()),
            "the list moved, so the tab does re-render"
        );

        assert_eq!(
            editor.departures_endpoint_row.text(),
            "vb",
            "…but the draft is theirs until they press apply"
        );
    }
}
