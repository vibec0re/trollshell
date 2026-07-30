//! Configuration column: primary link details, all-links overview, DNS.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::networkd::{self, Link, LinkSource, OperationalState};
use hytte::services::resolved;

use super::pill_label;
use crate::components::layout::boxed_list;
use crate::components::reactive_list::reactive_list;

/// Accent-coloured pill: a live, routable connection.
const PILL_CONNECTED: &str = "ts-pill-connected";
/// Muted pill, used for every non-affirmative state — "Offline" and "Unknown"
/// alike. No new CSS class was needed for #608's neutral rendering: the two
/// existing variants are already "accent" and "muted", and offline was always
/// the muted one. What was dishonest was the *word*, not the colour.
const PILL_NEUTRAL: &str = "ts-pill-known";

pub(super) fn build_connection_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Connection").build();

    // Online/offline state as a card row (replaces the old floating group
    // description). The pill suffix updates in-place; CSS classes are
    // toggled rather than recreating a new label each emission.
    let state_row = adw::ActionRow::builder()
        .title("Status")
        .activatable(false)
        .selectable(false)
        .build();
    // Starts at "Unknown", which is what is true before the first emission.
    let state_pill = pill_label(UNKNOWN_PILL, PILL_NEUTRAL);
    state_row.add_suffix(&state_pill);

    // Bound to the link *and* to whether anything answered for it — see
    // `status_view`. `primary()` alone cannot tell "there is no routable link"
    // from "there is no link manager", and only the first is Offline (#608).
    let status = map_ref! {
        let primary = networkd::primary(),
        let source = networkd::link_source() => status_view(primary.as_ref(), *source)
    };
    bind(status, &state_row, {
        let pill = state_pill.clone();
        move |row, view| {
            row.set_subtitle(&view.subtitle);
            pill.set_text(view.pill);
            // Reset to the two variants and re-apply so the class set can't
            // accumulate across state changes.
            pill.remove_css_class(PILL_CONNECTED);
            pill.remove_css_class(PILL_NEUTRAL);
            pill.add_css_class(view.pill_class);
        }
    });
    group.add(&state_row);

    // Three expanders in vertical order; placeholder row replaces
    // Primary when no connection is active.
    group.add(&build_primary_expander());
    group.add(&build_no_connection_placeholder_row());
    group.add(&build_all_links_expander());
    group.add(&build_dns_expander());

    group
}

// ── Honest rendering of an unanswered question (#608) ────────────────────────
//
// Every helper below exists because `links()`/`primary()` describe the link
// picture, not whether anyone drew it. The rule they all follow is the same: a
// *positive* reading stands on its own (a link we saw is a link, even if the
// daemon has since gone quiet), but an *absence* is only reportable as a fact
// when a link manager answered. Otherwise we say we don't know.

/// Pill text for "no link manager answered, so we cannot say".
const UNKNOWN_PILL: &str = "Unknown";

/// One rendering of the Status row: the subtitle line plus the pill's text and
/// variant class.
#[derive(Clone, Debug, Eq, PartialEq)]
struct StatusView {
    subtitle: String,
    pill: &'static str,
    pill_class: &'static str,
}

/// What the Status row should say for a given primary link and link source.
///
/// The `None` arms are the fix: pre-#608 they all read "Offline", so a host with
/// neither systemd-networkd nor `NetworkManager` was told it was offline while
/// its interfaces moved traffic. A link we *did* read is reported as before,
/// even if the source has since stopped answering — that is a positive
/// observation, and dropping it would trade one falsehood for a blank.
fn status_view(primary: Option<&Link>, source: LinkSource) -> StatusView {
    let subtitle = link_status_text(primary, source);

    let Some(link) = primary else {
        // Note the `Unavailable` case is *not* "offline" either: interfaces
        // configured outside a manager (bridges, static config, containers)
        // can be routing perfectly well — the traffic card next door reads
        // the kernel and will happily show them.
        let pill = match source {
            LinkSource::Networkd | LinkSource::NetworkManager => "Offline",
            LinkSource::Unknown | LinkSource::Unavailable => UNKNOWN_PILL,
        };
        return StatusView {
            subtitle,
            pill,
            pill_class: PILL_NEUTRAL,
        };
    };

    let routable = link.operational == OperationalState::Routable;
    StatusView {
        subtitle,
        pill: if routable { "Online" } else { "Offline" },
        pill_class: if routable {
            PILL_CONNECTED
        } else {
            PILL_NEUTRAL
        },
    }
}

/// The status text for a primary link / link source pair — shared with the
/// bar chip's tooltip (`widgets/network.rs`, re-exported as
/// `panels::network::link_status_text`) so the two surfaces can never
/// describe the same state with different words. #620 found the chip using
/// its own, differently-grouped wording for exactly this reason.
pub(crate) fn link_status_text(primary: Option<&Link>, source: LinkSource) -> String {
    let Some(link) = primary else {
        return match source {
            // A manager answered and named no routable link. This — and only
            // this — is Offline.
            LinkSource::Networkd | LinkSource::NetworkManager => "Offline".to_string(),
            LinkSource::Unknown => "No link manager has answered yet".to_string(),
            LinkSource::Unavailable => {
                "No link manager (systemd-networkd or NetworkManager)".to_string()
            }
        };
    };

    match link.operational {
        OperationalState::Routable => format!("Online via {}", link.name),
        OperationalState::Carrier | OperationalState::DegradedCarrier => {
            format!("Limited connectivity via {}", link.name)
        }
        other => format!("{} via {}", describe_state(other), link.name),
    }
}

/// Whether the "No connection" placeholder row belongs on screen.
///
/// Only when a manager actually told us there is no primary link. With nothing
/// answering, the Status row already says so honestly and this row would just
/// restate the falsehood in bigger letters ("No connection / No primary network
/// link").
fn shows_no_connection(primary: Option<&Link>, source: LinkSource) -> bool {
    primary.is_none() && source.is_answering()
}

/// Subtitle for the "All links" expander.
///
/// A count we obtained is always reportable, however stale. A count of **zero**
/// with nothing answering is not a count at all — it is the absence of an
/// answer, and "0 interface(s)" would make a claim about the host out of it.
fn all_links_subtitle(count: usize, source: LinkSource) -> String {
    if count == 0 && !source.is_answering() {
        UNKNOWN_PILL.to_string()
    } else {
        format!("{count} interface(s)")
    }
}

fn build_primary_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("Primary").build();

    bind(
        networkd::primary().map(|p| p.map_or(String::new(), |link| link.name)),
        &expander,
        |w, name| w.set_title(&name),
    );
    bind(
        networkd::primary().map(|p| match p {
            Some(link) => describe_state(link.operational).to_string(),
            None => String::new(),
        }),
        &expander,
        |w, sub| w.set_subtitle(&sub),
    );
    bind(
        networkd::primary().map(|p| p.is_some()),
        &expander,
        gtk::prelude::WidgetExt::set_visible,
    );

    // Each row reactively re-derives its value from `primary()`.
    expander.add_row(&addr_row("IPv4 address", ipv4_addresses));
    expander.add_row(&addr_row("IPv4 gateway", |link| {
        link.gateway_v4.map(|g| g.to_string()).unwrap_or_default()
    }));
    expander.add_row(&addr_row("IPv6 address", ipv6_addresses));
    expander.add_row(&addr_row("IPv6 gateway", |link| {
        link.gateway_v6.map(|g| g.to_string()).unwrap_or_default()
    }));

    expander
}

/// All IPv4 addresses of `link`, one per line, as `addr/prefix`.
fn ipv4_addresses(link: &Link) -> String {
    link.addresses
        .iter()
        .filter_map(|a| match a.addr {
            std::net::IpAddr::V4(v) => Some(format!("{v}/{}", a.prefix_len)),
            std::net::IpAddr::V6(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// All routable IPv6 addresses of `link` (link-local excluded), one per line,
/// as `addr/prefix`.
fn ipv6_addresses(link: &Link) -> String {
    link.addresses
        .iter()
        .filter_map(|a| match a.addr {
            std::net::IpAddr::V6(v) if !v.is_unicast_link_local() => {
                Some(format!("{v}/{}", a.prefix_len))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build one address/gateway row bound to the primary link. The value is
/// rendered as the row *subtitle* (full width, wraps freely) so long IPv6
/// values can never collapse the title into one char per line. The row
/// auto-hides when the derived value is empty.
fn addr_row(title: &str, derive: impl Fn(&Link) -> String + 'static) -> adw::ActionRow {
    let row = build_addr_row(title);
    bind(
        networkd::primary().map(move |p| p.as_ref().map(&derive).unwrap_or_default()),
        &row,
        |row, txt| apply_addr_row(row, &txt),
    );
    row
}

/// Build an empty address/gateway row with the wrapping-subtitle layout shared
/// by the primary block and the per-link expanders.
fn build_addr_row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    // Keep the title on a single line; let the subtitle (the value) wrap.
    row.set_title_lines(1);
    row.set_subtitle_lines(0);
    row.add_css_class("ts-mono");
    row
}

/// Apply a derived value to an address/gateway row: set it as the subtitle and
/// hide the row when empty.
fn apply_addr_row(row: &adw::ActionRow, value: &str) {
    row.set_subtitle(value);
    row.set_visible(!value.is_empty());
}

/// The four address/gateway detail row titles for a per-link disclosure, in
/// display order. Index-aligned with [`link_detail_values`].
const LINK_DETAIL_TITLES: [&str; 4] = [
    "IPv4 address",
    "IPv4 gateway",
    "IPv6 address",
    "IPv6 gateway",
];

/// The four address/gateway detail values for `link`, index-aligned with
/// [`LINK_DETAIL_TITLES`]. Empty strings mean the corresponding row hides
/// itself.
fn link_detail_values(link: &Link) -> [String; 4] {
    [
        ipv4_addresses(link),
        link.gateway_v4.map(|g| g.to_string()).unwrap_or_default(),
        ipv6_addresses(link),
        link.gateway_v6.map(|g| g.to_string()).unwrap_or_default(),
    ]
}

fn build_no_connection_placeholder_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("No connection")
        .activatable(false)
        .selectable(false)
        .build();
    row.set_subtitle("No primary network link");
    bind(
        map_ref! {
            let primary = networkd::primary(),
            let source = networkd::link_source() => shows_no_connection(primary.as_ref(), *source)
        },
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );
    row
}

/// The widgets making up one link's disclosure row inside "All links". Kept in
/// a stable per-link cache (keyed by link name) so the bind handler can diff
/// the list in place across `networkd::links()` emissions — rather than
/// drain-rebuilding — and an expanded link survives carrier blips with its
/// chevron and revealer state intact (#152).
struct LinkRow {
    /// The `AdwActionRow` shown in the "All links" expander (name + pill +
    /// chevron). Added to / removed from the parent expander via `add_row` /
    /// `remove`.
    action: adw::ActionRow,
    /// The boxed-list wrapper holding this link's detail rows, slid open/shut
    /// by the chevron. A separate row in the parent expander, kept directly
    /// beneath `action` by the diff's re-ordering pass.
    detail_holder: gtk::ListBoxRow,
    /// The connection-state pill suffix, updated in place each emission.
    pill: gtk::Label,
    /// The four address/gateway detail rows (index-aligned with
    /// [`LINK_DETAIL_TITLES`]), updated in place — they hide themselves when
    /// their value is empty.
    detail_rows: [adw::ActionRow; 4],
}

fn build_all_links_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("All links").build();
    bind(
        map_ref! {
            let links = networkd::links(),
            let source = networkd::link_source() => {
                all_links_subtitle(links.iter().filter(|l| l.name != "lo").count(), *source)
            }
        },
        &expander,
        |w, sub| w.set_subtitle(&sub),
    );

    // Per-link rows keyed by link name, diffed in place on every
    // `networkd::links()` emission. networkd emits on any link change (carrier
    // flap, address renewal, a `vb-h-*` veth blinking); draining and rebuilding
    // collapsed rows each time destroyed any link you had expanded (#152), so
    // we instead remove only links that disappeared, add only new ones, and
    // update survivors in place — leaving their chevron/revealer untouched.
    let cache: Rc<RefCell<HashMap<String, LinkRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let expander_for_bind = expander.clone();
    let cache_for_bind = cache.clone();
    bind(networkd::links(), &expander, move |_, links| {
        let mut named: Vec<&Link> = links.iter().filter(|l| l.name != "lo").collect();
        named.sort_by(|a, b| a.name.cmp(&b.name));

        let mut cache_mut = cache_for_bind.borrow_mut();

        // Remove links that disappeared.
        let live: HashSet<String> = named.iter().map(|l| l.name.clone()).collect();
        cache_mut.retain(|name, row| {
            let keep = live.contains(name);
            if !keep {
                expander_for_bind.remove(&row.action);
                expander_for_bind.remove(&row.detail_holder);
            }
            keep
        });

        // Update survivors in place; build rows for newly-seen links.
        let mut added_new = false;
        for link in &named {
            if let Some(row) = cache_mut.get(&link.name) {
                update_link_row(row, link);
            } else {
                let row = build_link_row(&expander_for_bind, link);
                cache_mut.insert(link.name.clone(), row);
                added_new = true;
            }
        }

        // Only re-order when the membership changed: detach every surviving
        // row, then re-add in sorted name order so a new link lands in the
        // right slot and each link's detail holder stays directly beneath its
        // action row. Survivor updates never touch ordering, so an expanded
        // link does not jump or re-collapse on a carrier blip.
        if added_new {
            for row in cache_mut.values() {
                expander_for_bind.remove(&row.action);
                expander_for_bind.remove(&row.detail_holder);
            }
            for link in &named {
                if let Some(row) = cache_mut.get(&link.name) {
                    expander_for_bind.add_row(&row.action);
                    expander_for_bind.add_row(&row.detail_holder);
                }
            }
        }
    });

    expander
}

/// Build one link's disclosure: an activatable `AdwActionRow` (name + state
/// pill + chevron we drive ourselves) over a `gtk::Revealer` holding the
/// link's address/gateway rows, added to `parent`. Activating the row toggles
/// the revealer and flips the chevron between `pan-end` (collapsed) and
/// `pan-down` (expanded) — so the disclosure state is ours, sidestepping
/// libadwaita's stuck nested-expander arrow (#152).
fn build_link_row(parent: &adw::ExpanderRow, link: &Link) -> LinkRow {
    let action = adw::ActionRow::builder()
        .title(&link.name)
        .activatable(true)
        .build();
    action.set_title_lines(1);

    let pill = pill_label(
        state_pill_text(link.operational),
        state_pill_class(link.operational),
    );
    action.add_suffix(&pill);

    let chevron = gtk::Image::from_icon_name("pan-end-symbolic");
    chevron.set_valign(gtk::Align::Center);
    action.add_suffix(&chevron);

    // Detail rows live in their own boxed-list inside the revealer, wrapped in a
    // `gtk::ListBoxRow` so the parent expander renders them as a row rather than
    // a separator-less child below the list (the adw routing gotcha).
    let detail_list = boxed_list();

    let values = link_detail_values(link);
    let detail_rows = std::array::from_fn(|i| {
        let row = build_addr_row(LINK_DETAIL_TITLES[i]);
        row.set_activatable(false);
        row.set_selectable(false);
        apply_addr_row(&row, &values[i]);
        detail_list.append(&row);
        row
    });

    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    revealer.set_reveal_child(false);
    revealer.set_child(Some(&detail_list));

    let detail_holder = gtk::ListBoxRow::new();
    detail_holder.set_activatable(false);
    detail_holder.set_selectable(false);
    detail_holder.set_child(Some(&revealer));

    // Activation drives the disclosure: flip the revealer, swap the chevron.
    let chevron_for_click = chevron.clone();
    action.connect_activated(move |_| {
        let now = !revealer.reveals_child();
        revealer.set_reveal_child(now);
        chevron_for_click.set_icon_name(Some(if now {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        }));
    });

    parent.add_row(&action);
    parent.add_row(&detail_holder);

    LinkRow {
        action,
        detail_holder,
        pill,
        detail_rows,
    }
}

/// Update a surviving link's row in place: refresh the state pill and the four
/// detail values. Never touches the chevron or revealer, so the link's
/// expanded/collapsed disclosure state persists across refreshes.
fn update_link_row(row: &LinkRow, link: &Link) {
    row.pill.set_text(state_pill_text(link.operational));
    // Reset to the two pill variants and re-apply the current one so the class
    // set doesn't accumulate across state changes.
    row.pill.remove_css_class(PILL_CONNECTED);
    row.pill.remove_css_class(PILL_NEUTRAL);
    row.pill.add_css_class(state_pill_class(link.operational));

    let values = link_detail_values(link);
    for (detail, value) in row.detail_rows.iter().zip(values.iter()) {
        apply_addr_row(detail, value);
    }
}

fn state_pill_text(state: OperationalState) -> &'static str {
    match state {
        OperationalState::Routable => "Online",
        OperationalState::Carrier | OperationalState::DegradedCarrier => "Carrier",
        OperationalState::Degraded => "Degraded",
        OperationalState::EnslavedRouting => "Enslaved",
        OperationalState::NoCarrier => "No carrier",
        OperationalState::Dormant => "Dormant",
        OperationalState::Off => "Off",
        OperationalState::Missing => "Missing",
        OperationalState::Unknown => "Unknown",
    }
}

fn state_pill_class(state: OperationalState) -> &'static str {
    match state {
        OperationalState::Routable => PILL_CONNECTED,
        _ => PILL_NEUTRAL,
    }
}

fn build_dns_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("DNS").build();
    bind(
        resolved::dns().map(|state| {
            if state.configured() {
                format!("{} server(s)", state.servers.len())
            } else {
                "Not configured".to_string()
            }
        }),
        &expander,
        |w, sub| w.set_subtitle(&sub),
    );

    reactive_list(
        &expander,
        resolved::dns().map(|state| state.servers),
        |ip: &std::net::IpAddr| {
            let row = adw::ActionRow::builder()
                .title(ip.to_string())
                .activatable(false)
                .build();
            row.set_title_lines(1);
            row.add_css_class("ts-mono");
            row
        },
        None::<fn() -> adw::ActionRow>,
    );

    expander
}

fn describe_state(s: OperationalState) -> &'static str {
    match s {
        OperationalState::Routable => "routable",
        OperationalState::Degraded => "degraded",
        OperationalState::DegradedCarrier => "degraded carrier",
        OperationalState::Carrier => "carrier",
        OperationalState::EnslavedRouting => "enslaved",
        OperationalState::NoCarrier => "no carrier",
        OperationalState::Dormant => "dormant",
        OperationalState::Off => "off",
        OperationalState::Missing => "missing",
        OperationalState::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LinkSource, PILL_CONNECTED, PILL_NEUTRAL, UNKNOWN_PILL, all_links_subtitle,
        shows_no_connection, status_view,
    };
    use hytte::services::networkd::{Link, OperationalState};

    fn link(name: &str, operational: OperationalState) -> Link {
        Link {
            name: name.to_string(),
            operational,
            ..Link::default()
        }
    }

    /// The states in which the card may legitimately say "Offline".
    const ANSWERED: [LinkSource; 2] = [LinkSource::Networkd, LinkSource::NetworkManager];
    /// The states in which it may not — #608's whole subject.
    const UNANSWERED: [LinkSource; 2] = [LinkSource::Unknown, LinkSource::Unavailable];

    #[test]
    fn a_manager_that_reports_no_primary_link_is_still_offline() {
        // The behaviour that must survive the fix: on a host whose link manager
        // answers, "no primary link" is a fact and "Offline" is the right word.
        for source in ANSWERED {
            let view = status_view(None, source);
            assert_eq!(view.pill, "Offline", "{source:?}");
            assert_eq!(view.subtitle, "Offline", "{source:?}");
            assert_eq!(view.pill_class, PILL_NEUTRAL, "{source:?}");
        }
    }

    #[test]
    fn a_host_with_nothing_to_ask_is_never_called_offline() {
        // The bug: `None` meant both "no routable link" and "no link manager",
        // and the panel rendered the second as the first.
        for source in UNANSWERED {
            let view = status_view(None, source);
            assert_eq!(view.pill, UNKNOWN_PILL, "{source:?}");
            assert_eq!(view.pill_class, PILL_NEUTRAL, "{source:?}");
            assert!(
                !view.subtitle.contains("Offline"),
                "{source:?} must not assert offline: {:?}",
                view.subtitle
            );
        }
    }

    #[test]
    fn the_two_unanswered_states_are_told_apart_for_the_user() {
        // Both are "we don't know", but why differs, and the difference is
        // actionable: one is transient, the other is how this host is built.
        let waiting = status_view(None, LinkSource::Unknown);
        let absent = status_view(None, LinkSource::Unavailable);
        assert_ne!(waiting.subtitle, absent.subtitle);
        assert!(absent.subtitle.contains("No link manager"));
    }

    #[test]
    fn a_routable_link_still_reads_online() {
        let view = status_view(
            Some(&link("wlp1s0", OperationalState::Routable)),
            LinkSource::Networkd,
        );
        assert_eq!(view.pill, "Online");
        assert_eq!(view.pill_class, PILL_CONNECTED);
        assert_eq!(view.subtitle, "Online via wlp1s0");
    }

    #[test]
    fn a_link_we_read_stands_even_after_the_source_goes_quiet() {
        // Only *absences* need a source to vouch for them. A link we actually saw
        // is a positive observation; a source that has stopped answering (or was
        // never confirmed) does not retract it, it just stops adding to it.
        for source in [LinkSource::Unknown, LinkSource::Unavailable] {
            let view = status_view(Some(&link("hive-br0", OperationalState::Routable)), source);
            assert_eq!(view.subtitle, "Online via hive-br0", "{source:?}");
            assert_eq!(view.pill, "Online", "{source:?}");
        }
    }

    #[test]
    fn a_non_routable_primary_keeps_its_descriptive_subtitle() {
        let view = status_view(
            Some(&link("eth0", OperationalState::Carrier)),
            LinkSource::Networkd,
        );
        assert_eq!(view.subtitle, "Limited connectivity via eth0");
        assert_eq!(view.pill_class, PILL_NEUTRAL);
    }

    #[test]
    fn zero_interfaces_is_only_claimed_when_something_answered() {
        for source in ANSWERED {
            assert_eq!(
                all_links_subtitle(0, source),
                "0 interface(s)",
                "{source:?}"
            );
        }
        for source in UNANSWERED {
            assert_eq!(
                all_links_subtitle(0, source),
                UNKNOWN_PILL,
                "{source:?} has no basis for a count of zero"
            );
        }
    }

    #[test]
    fn a_count_we_obtained_is_reported_whatever_the_source_says_now() {
        // Six interfaces on screen under "0 interface(s)" was #607's screenshot.
        // A non-zero count came from somewhere, so it is never suppressed.
        for source in [
            LinkSource::Networkd,
            LinkSource::NetworkManager,
            LinkSource::Unknown,
            LinkSource::Unavailable,
        ] {
            assert_eq!(
                all_links_subtitle(6, source),
                "6 interface(s)",
                "{source:?}"
            );
        }
    }

    #[test]
    fn the_no_connection_placeholder_needs_a_source_to_vouch_for_it() {
        for source in ANSWERED {
            assert!(shows_no_connection(None, source), "{source:?}");
        }
        for source in UNANSWERED {
            assert!(
                !shows_no_connection(None, source),
                "{source:?} would restate the falsehood the Status row just avoided"
            );
        }
    }

    #[test]
    fn the_no_connection_placeholder_stays_hidden_while_a_link_is_up() {
        let up = link("eth0", OperationalState::Routable);
        for source in [LinkSource::Networkd, LinkSource::Unknown] {
            assert!(!shows_no_connection(Some(&up), source), "{source:?}");
        }
    }
}
