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

/// Everything the tab's handlers share. Cheap to clone (all handles), which is
/// what lets each widget's closure own one.
#[derive(Clone)]
struct Editor {
    /// The set as `places.toml` last read back. Every save is checked against
    /// this, so an `$EDITOR` save landing under us is caught rather than
    /// clobbered, and it is re-read from the file after each successful write
    /// so it always says exactly what is on disk.
    base: Rc<RefCell<Vec<Place>>>,
    /// The stack detail pages are pushed onto.
    nav: adw::NavigationView,
    /// The root page's list of places, rebuilt whenever the set changes.
    list: adw::PreferencesGroup,
    /// Every child currently in `list`, for teardown before a rebuild.
    ///
    /// Never held borrowed across a `list.remove()`: GTK emits synchronously
    /// into handlers that re-enter these cells, and a `BorrowMutError` inside a
    /// glib callback aborts the process rather than failing gracefully (#643).
    rows: Rc<RefCell<Vec<gtk::Widget>>>,
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
                *self.base.borrow_mut() = places::load_places();
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
            let editor = self.clone();
            toast.connect_button_clicked(move |_| editor.reload());
        }
        self.toasts.add_toast(toast);
    }

    /// Re-read the file and rebuild the list — after an out-of-band edit, or
    /// when the user dismisses a "changed on disk" toast.
    ///
    /// Pops back to the root first: a detail page addresses its place by index,
    /// and the reload may have removed or reordered it.
    fn reload(&self) {
        *self.base.borrow_mut() = places::load_places();
        while self.nav.pop() {}
        self.rebuild();
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
            self.list.remove(&row);
        }
        let places = self.places();
        let resolved = self.resolved.borrow().clone();
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
            let editor = self.clone();
            row.connect_activated(move |_| editor.open(index));
            self.list.add(&row);
            rows.push(row.upcast::<gtk::Widget>());
        }

        let add = adw::ActionRow::builder()
            .title("Add a place")
            .subtitle("Somewhere you frequent: home, the office, a regular haunt")
            .activatable(true)
            .build();
        add.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
        let editor = self.clone();
        // Deferred to an idle tick, unlike the place rows above. `add` saves,
        // and a save rebuilds this group — which would mean removing *this row*
        // from inside its own `row-activated` emission. The place rows only
        // push a page, so they can run inline.
        add.connect_activated(move |_| {
            let editor = editor.clone();
            glib::idle_add_local_once(move || editor.add());
        });
        self.list.add(&add);
        rows.push(add.upcast::<gtk::Widget>());

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

        let header = adw::HeaderBar::new();
        let delete = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Delete this place")
            .build();
        delete.add_css_class("flat");
        delete.add_css_class("destructive-action");
        {
            let (editor, name) = (self.clone(), place.name.clone());
            delete.connect_clicked(move |btn| editor.confirm_delete(btn, index, &name));
        }
        header.pack_end(&delete);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
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
        let editor = self.clone();
        name.connect_apply(move |entry| {
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
        let editor = self.clone();
        radius.connect_value_notify(move |row| {
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
        let editor = self.clone();
        row.connect_apply(move |entry| {
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
        let editor = self.clone();
        row.connect_value_notify(move |row| {
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
        let editor = self.clone();
        station.connect_apply(move |entry| {
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
        let editor = self.clone();
        walk.connect_value_notify(move |row| {
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
                let editor = self.clone();
                row.connect_apply(move |entry| {
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
                let editor = self.clone();
                remove.connect_clicked(move |_| {
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
        let editor = self.clone();
        add.connect_apply(move |entry| {
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
        let editor = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "delete" {
                return;
            }
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
        let editor = editor.clone();
        city.connect_apply(move |entry| {
            let city = entry.text().trim().to_owned();
            if city.is_empty() {
                return;
            }
            let editor = editor.clone();
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
        .description(
            "Somewhere you frequent, how the shell recognises it, and what departures to show \
             there. Saved straight to ~/.config/trollshell/places.toml, which the shell re-reads \
             within a few seconds — so this works whether or not trollshell is running, and hand \
             edits to that file are preserved.",
        )
        .build();

    let editor = Editor {
        base: Rc::new(RefCell::new(places::load_places())),
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

    // Auto/manual toggle → SetAutoLocation, then re-read the resolved place.
    {
        let handler = editor.clone();
        editor.auto_switch.connect_active_notify(move |sw| {
            if handler.syncing.get() {
                return;
            }
            let handler = handler.clone();
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
            let current = editor.places();
            if let Some(reloaded) = watcher.poll(&current) {
                *editor.base.borrow_mut() = reloaded;
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
}
