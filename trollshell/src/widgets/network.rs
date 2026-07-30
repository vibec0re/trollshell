use hytte::futures_signals::map_ref;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::networkd::{self, Link, LinkSource, OperationalState};

use crate::panels::network::link_status_text;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn =
        crate::components::chip::indicator("ts-network", crate::modal::Page::Network, monitor);

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    // `primary()` alone cannot tell "no routable link" from "no link manager
    // has answered yet" — both read as `None`. Combine it with `link_source()`
    // (#610) the same way the network panel does (`panels/network/connection.rs`'s
    // `status_view`), so the bar chip can't re-collapse that three-way
    // distinction into a boolean the way #620 found it doing. `.dedupe_cloned()`
    // (the `ChipView.tooltip` is an owned `String`, so `dedupe`'s `Copy` bound
    // doesn't apply — same idiom as `widgets/workspaces.rs`/`overlays/osd.rs`)
    // keeps the 5s networkd poll from rewriting the icon/tooltip when nothing
    // actually changed.
    let combined = map_ref! {
        let primary = networkd::primary(),
        let source = networkd::link_source() => chip_view(primary.as_ref(), *source)
    }
    .dedupe_cloned();

    // Bound to the button (its lifetime governs the apply-loop, matching
    // `connection.rs`'s state-row/pill pattern) with the image reached via a
    // captured clone, since the tooltip belongs on the button's full hit
    // area, not just the inner icon.
    bind(combined, &btn, {
        let icon = icon.clone();
        move |btn, view: ChipView| {
            icon.set_icon_name(Some(view.icon));
            btn.set_tooltip_text(Some(&view.tooltip));
        }
    });

    btn.upcast()
}

/// One rendering of the bar chip: icon name plus a tooltip. The tooltip is
/// always `panels::network::link_status_text` — the exact text the network
/// panel's Status row shows — so the two surfaces can never disagree about
/// what a given link/source pair means.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ChipView {
    icon: &'static str,
    tooltip: String,
}

fn chip_view(primary: Option<&Link>, source: LinkSource) -> ChipView {
    ChipView {
        icon: icon_name(primary, source),
        tooltip: link_status_text(primary, source),
    }
}

/// The chip's icon for a given primary link and link source.
///
/// An absence is only rendered as "disconnected" when a link manager
/// actually answered and named no routable link
/// ([`LinkSource::Networkd`] / [`LinkSource::NetworkManager`]). Otherwise —
/// nothing has answered yet ([`LinkSource::Unknown`]) or this host has no
/// link manager at all ([`LinkSource::Unavailable`]) — the chip uses
/// `network-idle-symbolic`: a single, fully-dimmed glyph, chosen specifically
/// because (unlike `network-*-no-route-symbolic`) it cannot be mistaken for
/// "link up, no route" at 16px.
fn icon_name(primary: Option<&Link>, source: LinkSource) -> &'static str {
    let Some(link) = primary else {
        return if source.is_answering() {
            "network-wired-disconnected-symbolic"
        } else {
            "network-idle-symbolic"
        };
    };

    // Exhaustive: a new `OperationalState` variant must be triaged here
    // explicitly rather than silently falling into a catch-all, matching how
    // `connection.rs`'s `describe_state`/`state_pill_text` are exhaustive too.
    match link.operational {
        OperationalState::Routable => "network-wired-symbolic",
        OperationalState::Degraded | OperationalState::DegradedCarrier => {
            "network-wired-acquiring-symbolic"
        }
        OperationalState::Carrier
        | OperationalState::NoCarrier
        | OperationalState::Dormant
        | OperationalState::EnslavedRouting
        | OperationalState::Off
        | OperationalState::Missing
        | OperationalState::Unknown => "network-wired-no-route-symbolic",
    }
}

#[cfg(test)]
mod tests {
    use super::{ChipView, chip_view, icon_name};
    use hytte::services::networkd::{Link, LinkSource, OperationalState};

    fn link(operational: OperationalState) -> Link {
        Link {
            name: "eth0".to_string(),
            operational,
            ..Link::default()
        }
    }

    /// The states in which the chip may legitimately claim "disconnected".
    const ANSWERED: [LinkSource; 2] = [LinkSource::Networkd, LinkSource::NetworkManager];
    /// The states in which it may not — #620's whole subject.
    const UNANSWERED: [LinkSource; 2] = [LinkSource::Unknown, LinkSource::Unavailable];
    /// Every `LinkSource` variant, for checks that must hold regardless.
    const ALL_SOURCES: [LinkSource; 4] = [
        LinkSource::Networkd,
        LinkSource::NetworkManager,
        LinkSource::Unknown,
        LinkSource::Unavailable,
    ];

    #[test]
    fn a_manager_that_reports_no_primary_link_is_shown_disconnected() {
        for source in ANSWERED {
            let view = chip_view(None, source);
            assert_eq!(
                view.icon, "network-wired-disconnected-symbolic",
                "{source:?}"
            );
            assert_eq!(view.tooltip, "Offline", "{source:?}");
        }
    }

    #[test]
    fn a_host_with_nothing_to_ask_is_never_shown_disconnected() {
        // The bug: `None` primary meant both "no routable link" and "no link
        // manager has answered", and the chip always rendered the first.
        for source in UNANSWERED {
            let view = chip_view(None, source);
            assert_eq!(view.icon, "network-idle-symbolic", "{source:?}");
        }
    }

    #[test]
    fn the_two_unanswered_states_get_distinct_tooltips() {
        // Same neutral glyph, but the reason differs and is worth surfacing —
        // matches `connection.rs`'s treatment of the same two states.
        let waiting = chip_view(None, LinkSource::Unknown).tooltip;
        let absent = chip_view(None, LinkSource::Unavailable).tooltip;
        assert_eq!(waiting, "No link manager has answered yet");
        assert_eq!(
            absent,
            "No link manager (systemd-networkd or NetworkManager)"
        );
    }

    #[test]
    fn a_routable_link_reads_online_regardless_of_source() {
        for source in ALL_SOURCES {
            let view = chip_view(Some(&link(OperationalState::Routable)), source);
            assert_eq!(
                view,
                ChipView {
                    icon: "network-wired-symbolic",
                    tooltip: "Online via eth0".to_string(),
                },
                "{source:?}"
            );
        }
    }

    #[test]
    fn a_degraded_link_shows_the_acquiring_glyph_and_matching_words() {
        for state in [
            OperationalState::Degraded,
            OperationalState::DegradedCarrier,
        ] {
            let view = chip_view(Some(&link(state)), LinkSource::Networkd);
            assert_eq!(view.icon, "network-wired-acquiring-symbolic", "{state:?}");
        }
        // But the WORDS must not cross-cut the way the icon grouping does:
        // the panel puts `DegradedCarrier` with `Carrier` ("Limited
        // connectivity"), not with `Degraded` — #620's second finding.
        let degraded = chip_view(
            Some(&link(OperationalState::Degraded)),
            LinkSource::Networkd,
        );
        let degraded_carrier = chip_view(
            Some(&link(OperationalState::DegradedCarrier)),
            LinkSource::Networkd,
        );
        let carrier = chip_view(Some(&link(OperationalState::Carrier)), LinkSource::Networkd);
        assert_eq!(degraded.tooltip, "degraded via eth0");
        assert_eq!(degraded_carrier.tooltip, carrier.tooltip);
        assert_eq!(degraded_carrier.tooltip, "Limited connectivity via eth0");
    }

    #[test]
    fn states_that_used_to_hide_behind_a_wildcard_get_the_no_route_glyph() {
        // Carrier, EnslavedRouting, and Dormant are all reachable in practice
        // and were previously swallowed by a `_` arm.
        for state in [
            OperationalState::Carrier,
            OperationalState::EnslavedRouting,
            OperationalState::Dormant,
        ] {
            assert_eq!(
                icon_name(Some(&link(state)), LinkSource::Networkd),
                "network-wired-no-route-symbolic",
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_link_we_read_stands_even_after_the_source_goes_quiet() {
        // Only *absences* need a source to vouch for them (see `icon_name`'s
        // doc comment and `connection.rs`'s matching rule). A link we actually
        // saw is a positive observation the source going quiet doesn't retract.
        for source in [LinkSource::Unknown, LinkSource::Unavailable] {
            let view = chip_view(Some(&link(OperationalState::Routable)), source);
            assert_eq!(view.icon, "network-wired-symbolic", "{source:?}");
        }
    }
}
