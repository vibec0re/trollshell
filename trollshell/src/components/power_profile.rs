//! Shared power-profile expander widget, reused by `panels::power` and
//! `panels::settings`.
//!
//! Both panels surface the same `adw::ExpanderRow` backed by the same
//! `power_profiles::state()` signal so they stay in sync without any extra
//! coordination.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::power_profiles::{self, PowerProfilesState, humanize_profile};

/// Build an `adw::ExpanderRow` that shows the active power profile and lets
/// the user switch between available profiles.
///
/// The row hides itself when no power-profiles daemon is running (i.e.
/// `power_profiles::state().available` is empty).
pub(crate) fn build_power_profile_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("Power profile").build();

    bind(
        power_profiles::state().map(|s| !s.available.is_empty()),
        &expander,
        gtk::prelude::WidgetExt::set_visible,
    );

    bind(
        power_profiles::state().map(|s| humanize_profile(&s.active)),
        &expander,
        |row, t| row.set_subtitle(&t),
    );

    let icon = gtk::Image::new();
    icon.set_valign(gtk::Align::Center);
    bind(
        power_profiles::state().map(|s| profile_icon_name(&s.active)),
        &icon,
        |w, name| w.set_icon_name(Some(name)),
    );
    expander.add_prefix(&icon);

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    bind_profile_rows(&expander, power_profiles::state(), &rows_track);

    expander
}

/// Rebuild the per-profile `adw::ActionRow`s inside `expander` from `signal`,
/// tracking the live set in `rows_track` so the next emission can tear down
/// exactly what the previous one built.
///
/// Split out of [`build_power_profile_expander`] so this `bind` call site's
/// `WeakRef` contract (#224, `hytte-reactive/src/bind.rs:16-22`) can be driven
/// with a synthetic signal in tests — the same extraction #772 made for
/// `bind_device_groups`/`bind_tunnel_groups`. The builder reads
/// `power_profiles::state()` inline, which `.expect()`s without a registered
/// `Registry`, so before this seam existed nothing could construct the widget
/// in a test at all (#831).
fn bind_profile_rows<S>(
    expander: &adw::ExpanderRow,
    signal: S,
    rows_track: &Rc<RefCell<Vec<adw::ActionRow>>>,
) where
    S: Signal<Item = PowerProfilesState> + 'static,
{
    let rows_for_bind = rows_track.clone();
    bind(signal, expander, move |expander, state| {
        // `take()` ends the borrow before the first `remove()` — a chained
        // `borrow_mut().drain(..)` would keep the cell borrowed for the whole
        // loop, and a synchronous emission re-entering it panics (fatally, from
        // a glib callback).
        for row in rows_for_bind.take() {
            expander.remove(&row);
        }
        let mut new_rows = Vec::with_capacity(state.available.len());
        for profile in &state.available {
            let row = adw::ActionRow::builder()
                .title(humanize_profile(profile))
                .activatable(true)
                .build();
            if profile == &state.active {
                let check = gtk::Image::from_icon_name("object-select-symbolic");
                check.set_valign(gtk::Align::Center);
                row.add_suffix(&check);
            }
            let profile_owned = profile.clone();
            row.connect_activated(move |_| {
                power_profiles::set_active(&profile_owned);
            });
            expander.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });
}

/// Map a power-profile name to its Adwaita symbolic icon.
pub(crate) fn profile_icon_name(active: &str) -> &'static str {
    match active {
        "performance" => "power-profile-performance-symbolic",
        "balanced" => "power-profile-balanced-symbolic",
        "power-saver" => "power-profile-power-saver-symbolic",
        _ => "system-run-symbolic",
    }
}

/// #831 regression coverage for this file's widget-pinning `bind` call site,
/// in the shape `panels/connections.rs` established for #772: the apply
/// closure must take the `&adw::ExpanderRow` `bind` hands it rather than a
/// strong clone captured from the enclosing scope, or the binding keeps the
/// expander alive for its own lifetime and defeats #224's `WeakRef` contract
/// (`hytte-reactive/src/bind.rs:16-22`).
#[cfg(all(test, feature = "system-tests"))]
mod pin_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use hytte::adw::{self, prelude::*};
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk;
    use hytte::services::power_profiles::PowerProfilesState;

    use super::bind_profile_rows;

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    fn two_profiles() -> PowerProfilesState {
        PowerProfilesState {
            active: "balanced".to_owned(),
            available: vec!["performance".to_owned(), "balanced".to_owned()],
        }
    }

    /// Anti-vacuity guard for the pin test below: the binding must actually
    /// apply, or "the widget died" would prove nothing about the closure.
    #[gtk::test]
    fn profile_rows_binding_applies_a_value() {
        adw::init().expect("libadwaita init");
        let expander = adw::ExpanderRow::new();
        let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
        let profiles = Mutable::new(PowerProfilesState::default());
        bind_profile_rows(&expander, profiles.signal_cloned(), &rows_track);
        pump();

        profiles.set(two_profiles());
        pump();

        assert_eq!(
            rows_track.borrow().len(),
            2,
            "the emitted PowerProfilesState's two profiles must reach the expander"
        );
    }

    /// Falsified by reintroducing the `expander_for_bind` strong clone the
    /// apply closure used to capture: with it, `drop(expander)` is not the
    /// last strong ref and the weak upgrade still succeeds.
    #[gtk::test]
    fn profile_rows_binding_does_not_pin_expander() {
        adw::init().expect("libadwaita init");
        let expander = adw::ExpanderRow::new();
        let weak = expander.downgrade();
        let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
        let profiles = Mutable::new(two_profiles());
        bind_profile_rows(&expander, profiles.signal_cloned(), &rows_track);
        pump();

        drop(expander);

        assert!(
            weak.upgrade().is_none(),
            "bind_profile_rows must not pin its expander: a strong clone captured by the apply \
             closure (rather than taking the closure's own `&adw::ExpanderRow` argument from \
             `bind`) would keep this alive for the life of the binding, defeating #224's WeakRef \
             contract"
        );
    }
}
