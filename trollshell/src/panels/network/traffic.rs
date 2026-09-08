//! Live column: per-interface byte rates in one group, totals + TCP summary
//! in a second group.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::sensors;
use hytte::ui::Sparkline;

use crate::components::format::{fmt_bytes, fmt_rate};
use crate::components::history_row::build_history_row;

/// Number of 1 Hz ticks an interface may stay flatlined before it collapses
/// into the idle bucket. Matches the 60-sample sparkline window: an interface
/// is "active" while it has shown a non-zero combined rate within the last
/// ~60 samples (~60 s), then falls idle. The counter resets on any non-zero
/// rate, so a bursty interface never flaps buckets tick-to-tick (hysteresis).
const GRACE_TICKS: u32 = 60;

/// Which of the two activity buckets a row currently lives in. `Active` rows
/// render inline in the per-interface group; `Idle` rows collapse behind the
/// expander.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bucket {
    Active,
    Idle,
}

/// Sticky activity classification: active while the interface has reported a
/// non-zero combined rate within the grace window.
fn bucket_for(ticks_since_nonzero: u32) -> Bucket {
    if ticks_since_nonzero < GRACE_TICKS {
        Bucket::Active
    } else {
        Bucket::Idle
    }
}

/// Returns `(iface_group, totals_group)` — callers add them in order.
pub(super) fn build_traffic_groups() -> (adw::PreferencesGroup, adw::PreferencesGroup) {
    // ── Per-interface sparkline group ──────────────────────────────────────
    let iface_group = adw::PreferencesGroup::new();

    // Idle interfaces collapse behind this single expander, which is added to
    // / removed from `iface_group` as the idle set becomes non-empty / empty
    // (no empty expander is ever shown). Built once and reparented — never
    // rebuilt — so it keeps its expanded/collapsed state.
    let idle_expander = adw::ExpanderRow::new();

    // Per-interface rows keyed by interface name. We keep widgets across
    // emissions so the Sparkline accumulates history; only the set of
    // keys changes (hot-plug, VPN tunnels coming/going) and which bucket a
    // row sits in (active ↔ idle).
    let cache: Rc<RefCell<HashMap<String, IfaceRow>>> = Rc::new(RefCell::new(HashMap::new()));
    bind_iface_rows(&iface_group, &idle_expander, sensors::network(), &cache);

    // ── Totals + TCP group ─────────────────────────────────────────────────
    let totals_group = adw::PreferencesGroup::new();

    // Totals row: sum across non-loopback interfaces.
    let totals_row = adw::ActionRow::builder().title("Total").build();
    bind(
        sensors::network().map(|net| {
            let (rx, tx) = net
                .interfaces
                .iter()
                .filter(|i| i.name != "lo")
                .fold((0u64, 0u64), |(rx, tx), i| {
                    (rx + i.rx_bytes_total, tx + i.tx_bytes_total)
                });
            format!("\u{2193} {} \u{2191} {}", fmt_bytes(rx), fmt_bytes(tx))
        }),
        &totals_row,
        |row, text| row.set_subtitle(&text),
    );
    totals_group.add(&totals_row);

    let tcp_row = adw::ActionRow::builder().title("TCP").build();
    bind(
        sensors::net_connections().map(|c| {
            format!(
                "{} established, {} listening",
                c.established_total(),
                c.tcp_listen + c.tcp6_listen,
            )
        }),
        &tcp_row,
        |row, text| row.set_subtitle(&text),
    );
    totals_group.add(&tcp_row);

    (iface_group, totals_group)
}

/// Diff `signal`'s per-interface rates into the two activity buckets:
/// `iface_group` holds the active rows inline, `idle_expander` collapses the
/// idle ones. Rows persist in `cache` so a `Sparkline` keeps its history
/// across bucket moves.
///
/// Split out of [`build_traffic_groups`] so this `bind` call site's `WeakRef`
/// contract (#224, `hytte-reactive/src/bind.rs:16-22`) can be driven with a
/// synthetic signal in tests, the same extraction #772 made for
/// `bind_device_groups`. The builder reads `sensors::network()` inline, which
/// `.expect()`s without a registered `Registry` (#831).
///
/// `idle_expander` is `bind`'s second-widget carve-out (#772): it is a sibling
/// of the bound `iface_group`, never the group itself, so capturing it
/// strongly is correct rather than a pin — `nix/lint-bind-pins.py`'s header
/// names this exact site.
fn bind_iface_rows<S>(
    iface_group: &adw::PreferencesGroup,
    idle_expander: &adw::ExpanderRow,
    signal: S,
    cache: &Rc<RefCell<HashMap<String, IfaceRow>>>,
) where
    S: Signal<Item = sensors::NetIo> + 'static,
{
    let idle_expander_for_bind = idle_expander.clone();
    let cache_for_bind = cache.clone();
    bind(signal, iface_group, move |iface_group, net| {
        let mut cache_mut = cache_for_bind.borrow_mut();

        // Whether the bucket layout needs rebuilding this tick (a row was
        // added, removed, or crossed the active/idle boundary). When nothing
        // structural changed we only refresh sparkline + rate text and leave
        // the widget tree untouched — no per-tick teardown.
        let mut structure_changed = false;

        // Remove interfaces that disappeared, detaching from their bucket.
        let live: HashSet<&str> = net
            .interfaces
            .iter()
            .map(|i| i.name.as_str())
            .filter(|&n| n != "lo")
            .collect();
        cache_mut.retain(|name, entry| {
            if live.contains(name.as_str()) {
                return true;
            }
            match entry.attached {
                Some(Bucket::Active) => iface_group.remove(&entry.container),
                Some(Bucket::Idle) => idle_expander_for_bind.remove(&entry.container),
                None => {}
            }
            structure_changed = true;
            false
        });

        // Update existing rows; create new ones for unseen names.
        for iface in net.interfaces.iter().filter(|i| i.name != "lo") {
            let combined = iface.rx_rate_bps + iface.tx_rate_bps;
            let value_text = format!(
                "\u{2193} {} \u{2191} {}",
                fmt_rate(iface.rx_rate_bps),
                fmt_rate(iface.tx_rate_bps),
            );
            if let Some(entry) = cache_mut.get_mut(&iface.name) {
                entry.spark.push(combined);
                entry.value.set_text(&value_text);
                // Sticky activity: reset on any traffic, else age toward idle.
                // Strictly `> 0.0` — any positive combined rate counts as
                // activity (the recommended default; a small rate floor is the
                // open knob).
                entry.ticks_since_nonzero = if combined > 0.0 {
                    0
                } else {
                    entry.ticks_since_nonzero.saturating_add(1)
                };
            } else {
                // A brand-new interface is bucketed by its *first* observed
                // rate, not defaulted to active: a flatlined new iface starts
                // idle so the panel is decluttered the instant it opens (no
                // ~60 s soft-start where every container veth shows inline);
                // an active one starts at ticks = 0. It arrives unplaced
                // (attached = None); the relayout below drops it into the
                // right bucket in sorted order.
                let mut entry = build_iface_traffic_row(iface);
                entry.spark.push(combined);
                entry.value.set_text(&value_text);
                entry.ticks_since_nonzero = if combined > 0.0 { 0 } else { GRACE_TICKS };
                cache_mut.insert(iface.name.clone(), entry);
                structure_changed = true;
            }
        }

        // If no row was added/removed, a relayout is still needed when any
        // surviving row crossed the active/idle boundary this tick.
        if !structure_changed {
            structure_changed = cache_mut
                .values()
                .any(|e| e.attached != Some(bucket_for(e.ticks_since_nonzero)));
        }

        if structure_changed {
            relayout(iface_group, &idle_expander_for_bind, &mut cache_mut);
        }
    });
}

/// Rebuild the two-bucket layout: inline active rows (name-sorted) in `group`,
/// then the idle rows (name-sorted) collapsed behind `expander`, which is only
/// present when at least one interface is idle.
///
/// Rows are **reparented, never rebuilt** — each `IfaceRow` widget persists in
/// the cache, so its `Sparkline` keeps its accumulated history across bucket
/// moves. Only called when membership actually changed (add/remove/boundary
/// crossing), never every tick.
fn relayout(
    group: &adw::PreferencesGroup,
    expander: &adw::ExpanderRow,
    cache: &mut HashMap<String, IfaceRow>,
) {
    // Detach every currently-placed row from its bucket, then the expander
    // itself, so we can re-add in the correct order below.
    for entry in cache.values() {
        match entry.attached {
            Some(Bucket::Active) => group.remove(&entry.container),
            Some(Bucket::Idle) => expander.remove(&entry.container),
            None => {}
        }
    }
    if expander.parent().is_some() {
        group.remove(expander);
    }

    // Name-sorted so display order is stable across emissions.
    let mut names: Vec<String> = cache.keys().cloned().collect();
    names.sort();

    // Inline active rows first, in sorted order.
    let mut idle_count = 0usize;
    for name in &names {
        let Some(entry) = cache.get_mut(name) else {
            continue;
        };
        if bucket_for(entry.ticks_since_nonzero) == Bucket::Active {
            group.add(&entry.container);
            entry.attached = Some(Bucket::Active);
        } else {
            idle_count += 1;
        }
    }

    // No idle interfaces → no expander at all (never an empty one).
    if idle_count == 0 {
        return;
    }

    expander.set_title(&format!(
        "{idle_count} idle interface{}",
        if idle_count == 1 { "" } else { "s" }
    ));
    for name in &names {
        let Some(entry) = cache.get_mut(name) else {
            continue;
        };
        if bucket_for(entry.ticks_since_nonzero) == Bucket::Idle {
            expander.add_row(&entry.container);
            entry.attached = Some(Bucket::Idle);
        }
    }
    // Added last so the idle bucket sorts after the inline active rows.
    group.add(expander);
}

/// Per-interface traffic row holding the widgets the bind updates each
/// `sensors::network()` emission. Stored in the network drawer's
/// interface cache.
///
/// `container` is a `gtk::ListBoxRow` wrapper — adding *that* (rather than a
/// bare `gtk::Box`) to the `AdwPreferencesGroup` makes each interface a member
/// of the group's boxed-list, so the rows pick up the standard card section
/// separators. A non-`GtkListBoxRow` child added to an `AdwPreferencesGroup`
/// instead renders below the boxed-list with no separators — which is why the
/// live interfaces previously had none.
struct IfaceRow {
    container: gtk::ListBoxRow,
    spark: Sparkline,
    value: gtk::Label,
    /// Ticks (1 Hz samples) since this interface last showed a non-zero
    /// combined rate. Drives the sticky active/idle classification via
    /// [`bucket_for`]; a freshly built row starts at 0 (active).
    ticks_since_nonzero: u32,
    /// Which bucket the row is currently parented into, or `None` if it has
    /// not been laid out yet. Lets [`relayout`] detach the row from the right
    /// container and detect real bucket changes without churning the tree.
    attached: Option<Bucket>,
}

fn build_iface_traffic_row(iface: &sensors::NetInterface) -> IfaceRow {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 2);
    outer.set_hexpand(true);

    let (top_row, spark, top_value) = build_history_row(&iface.name);
    // Hide the right-hand label slot — units are shown in the detail row
    // below the graph, so the "B/s" label here is redundant (#83).
    top_value.set_visible(false);
    top_row.set_hexpand(true);
    outer.append(&top_row);

    let detail = gtk::Label::new(None);
    detail.add_css_class("ts-stat-value");
    detail.set_xalign(0.0);
    detail.set_margin_start(88);
    detail.set_margin_bottom(4);
    outer.append(&detail);

    // Wrap in a non-interactive ListBoxRow so the group renders it inside the
    // boxed-list (with separators) rather than as a separator-less child below
    // the list.
    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    row.set_hexpand(true);
    row.set_child(Some(&outer));

    IfaceRow {
        container: row,
        spark,
        value: detail,
        ticks_since_nonzero: 0,
        attached: None,
    }
}

/// #831 regression coverage for this file's widget-pinning `bind` call site,
/// in the shape `panels/connections.rs` established for #772: the apply
/// closure must take the `&adw::PreferencesGroup` `bind` hands it rather than
/// a strong clone captured from the enclosing scope, or the binding keeps the
/// group alive for its own lifetime and defeats #224's `WeakRef` contract
/// (`hytte-reactive/src/bind.rs:16-22`).
#[cfg(all(test, feature = "system-tests"))]
mod pin_tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use hytte::adw::{self, prelude::*};
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk;
    use hytte::services::sensors::{NetInterface, NetIo};

    use super::{IfaceRow, bind_iface_rows};

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// One interface carrying traffic, so it lands in the **active** bucket
    /// and the idle expander is never parented into the group.
    fn active(name: &str) -> NetInterface {
        NetInterface {
            name: name.to_owned(),
            rx_bytes_total: 1,
            tx_bytes_total: 1,
            rx_rate_bps: 1.0,
            tx_rate_bps: 1.0,
        }
    }

    /// Anti-vacuity guard for the pin test below: the binding must actually
    /// apply, or "the widget died" would prove nothing about the closure.
    #[gtk::test]
    fn iface_rows_binding_applies_a_value() {
        adw::init().expect("libadwaita init");
        let iface_group = adw::PreferencesGroup::new();
        let idle_expander = adw::ExpanderRow::new();
        let cache: Rc<RefCell<HashMap<String, IfaceRow>>> = Rc::new(RefCell::new(HashMap::new()));
        let net: Mutable<NetIo> = Mutable::new(NetIo::default());
        bind_iface_rows(&iface_group, &idle_expander, net.signal_cloned(), &cache);
        pump();

        // `lo` is filtered out by the apply body, so it doubles as a check
        // that the moved body is the one running.
        net.set(NetIo {
            interfaces: vec![active("lo"), active("eth0")],
        });
        pump();

        assert_eq!(
            cache.borrow().keys().collect::<Vec<_>>(),
            vec!["eth0"],
            "the emitted NetIo's interfaces must reach the group, minus loopback"
        );
    }

    /// Falsified by reintroducing the `iface_group_for_bind` strong clone the
    /// apply closure used to capture: with it, `drop(iface_group)` is not the
    /// last strong ref and the weak upgrade still succeeds.
    ///
    /// The `idle_expander` clone the closure *does* keep is the #772 carve-out
    /// and must not affect this — it is a sibling widget, not the bind target.
    #[gtk::test]
    fn iface_rows_binding_does_not_pin_iface_group() {
        adw::init().expect("libadwaita init");
        let iface_group = adw::PreferencesGroup::new();
        let idle_expander = adw::ExpanderRow::new();
        let weak = iface_group.downgrade();
        let cache: Rc<RefCell<HashMap<String, IfaceRow>>> = Rc::new(RefCell::new(HashMap::new()));
        let net: Mutable<NetIo> = Mutable::new(NetIo {
            interfaces: vec![active("eth0")],
        });
        bind_iface_rows(&iface_group, &idle_expander, net.signal_cloned(), &cache);
        pump();

        // The per-interface rows are children of the group; a GTK child holds
        // no reference to its parent, and the cache is what would otherwise
        // outlive the drop below.
        drop(cache);
        drop(idle_expander);
        drop(iface_group);

        assert!(
            weak.upgrade().is_none(),
            "bind_iface_rows must not pin its iface_group: a strong clone captured by the apply \
             closure (rather than taking the closure's own `&adw::PreferencesGroup` argument from \
             `bind`) would keep this alive for the life of the binding, defeating #224's WeakRef \
             contract"
        );
    }
}
