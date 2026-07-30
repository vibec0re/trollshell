use hytte::futures_signals::map_ref;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::networkd::{self, Link, LinkSource, OperationalState};

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn =
        crate::components::chip::indicator("ts-network", crate::modal::Page::Network, monitor);

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    // `primary()` alone cannot tell "no routable link" from "no link manager
    // has answered yet" — both read as `None`. Combine it with `link_source()`
    // (#610) the same way the network panel does (`panels/network/connection.rs`'s
    // `status_view`), so the bar chip can't re-collapse that three-way
    // distinction into a boolean the way #620 found it doing.
    let combined = map_ref! {
        let primary = networkd::primary(),
        let source = networkd::link_source() => chip_view(primary.as_ref(), *source)
    };
    bind(combined, &icon, |w, view: ChipView| {
        w.set_icon_name(Some(view.icon));
        w.set_tooltip_text(Some(view.tooltip));
    });

    btn.upcast()
}

/// One rendering of the bar chip: icon name plus a tooltip. The tooltip text
/// for the no-primary-link cases is copied verbatim from `connection.rs`'s
/// `status_view` — keep the two in sync if that wording changes there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ChipView {
    icon: &'static str,
    tooltip: &'static str,
}

/// What the bar chip should show for a given primary link and link source.
///
/// Mirrors the panel's `status_view`: an absence is only reportable as
/// "disconnected" when a link manager actually answered and named no
/// routable link ([`LinkSource::Networkd`] / [`LinkSource::NetworkManager`]).
/// Otherwise — nothing has answered yet ([`LinkSource::Unknown`]) or this host
/// has no link manager at all ([`LinkSource::Unavailable`]) — the chip uses a
/// neutral glyph rather than affirmatively claiming the host is offline.
fn chip_view(primary: Option<&Link>, source: LinkSource) -> ChipView {
    let Some(link) = primary else {
        return match source {
            // A manager answered and named no routable link. This — and only
            // this — is Offline (#608/#620).
            LinkSource::Networkd | LinkSource::NetworkManager => ChipView {
                icon: "network-wired-disconnected-symbolic",
                tooltip: "Offline",
            },
            LinkSource::Unknown => ChipView {
                icon: "network-no-route-symbolic",
                tooltip: "No link manager has answered yet",
            },
            LinkSource::Unavailable => ChipView {
                icon: "network-no-route-symbolic",
                tooltip: "No link manager (systemd-networkd or NetworkManager)",
            },
        };
    };

    match link.operational {
        OperationalState::Routable => ChipView {
            icon: "network-wired-symbolic",
            tooltip: "Online",
        },
        OperationalState::Degraded | OperationalState::DegradedCarrier => ChipView {
            icon: "network-wired-acquiring-symbolic",
            tooltip: "Acquiring connectivity",
        },
        _ => ChipView {
            icon: "network-wired-no-route-symbolic",
            tooltip: "Limited connectivity",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{ChipView, chip_view};
    use hytte::services::networkd::{Link, LinkSource, OperationalState};

    fn link(operational: OperationalState) -> Link {
        Link {
            operational,
            ..Link::default()
        }
    }

    /// The states in which the chip may legitimately claim "disconnected".
    const ANSWERED: [LinkSource; 2] = [LinkSource::Networkd, LinkSource::NetworkManager];
    /// The states in which it may not — #620's whole subject.
    const UNANSWERED: [LinkSource; 2] = [LinkSource::Unknown, LinkSource::Unavailable];

    #[test]
    fn a_manager_that_reports_no_primary_link_is_shown_disconnected() {
        for source in ANSWERED {
            let view = chip_view(None, source);
            assert_eq!(
                view.icon, "network-wired-disconnected-symbolic",
                "{source:?}"
            );
        }
    }

    #[test]
    fn a_host_with_nothing_to_ask_is_never_shown_disconnected() {
        // The bug: `None` primary meant both "no routable link" and "no link
        // manager has answered", and the chip always rendered the first.
        for source in UNANSWERED {
            let view = chip_view(None, source);
            assert_ne!(
                view.icon, "network-wired-disconnected-symbolic",
                "{source:?} must not claim disconnected"
            );
            assert_eq!(view.icon, "network-no-route-symbolic", "{source:?}");
        }
    }

    #[test]
    fn the_two_unanswered_states_get_distinct_tooltips() {
        // Same neutral glyph, but the reason differs and is worth surfacing —
        // matches `connection.rs`'s treatment of the same two states.
        let waiting = chip_view(None, LinkSource::Unknown);
        let absent = chip_view(None, LinkSource::Unavailable);
        assert_ne!(waiting.tooltip, absent.tooltip);
        assert!(absent.tooltip.contains("No link manager"));
    }

    #[test]
    fn a_routable_link_reads_online_regardless_of_source() {
        for source in [
            LinkSource::Networkd,
            LinkSource::Unknown,
            LinkSource::Unavailable,
        ] {
            let view = chip_view(Some(&link(OperationalState::Routable)), source);
            assert_eq!(
                view,
                ChipView {
                    icon: "network-wired-symbolic",
                    tooltip: "Online",
                },
                "{source:?}"
            );
        }
    }

    #[test]
    fn a_degraded_link_shows_the_acquiring_glyph() {
        for state in [
            OperationalState::Degraded,
            OperationalState::DegradedCarrier,
        ] {
            let view = chip_view(Some(&link(state)), LinkSource::Networkd);
            assert_eq!(view.icon, "network-wired-acquiring-symbolic", "{state:?}");
        }
    }

    #[test]
    fn a_link_we_read_stands_even_after_the_source_goes_quiet() {
        // Only *absences* need a source to vouch for them (see `chip_view`'s
        // doc comment and `connection.rs`'s matching rule). A link we actually
        // saw is a positive observation the source going quiet doesn't retract.
        for source in [LinkSource::Unknown, LinkSource::Unavailable] {
            let view = chip_view(Some(&link(OperationalState::Routable)), source);
            assert_eq!(view.icon, "network-wired-symbolic", "{source:?}");
        }
    }
}
