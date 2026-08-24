//! Drawer panel for VPN connections.
//!
//! Two stacked sections:
//!  1. **Saved profiles** (NM `vpn` connection profiles) — an activate /
//!     deactivate control group at the top, sourced from
//!     [`wifi::vpn_profiles`]. Hidden entirely when there are none (and on the
//!     iwd / no-backend host, which surfaces no NM VPN profiles).
//!  2. **Live tunnels** — the read-only live-tunnel view sourced from
//!     [`vpn::tunnels`]: header description shows the live count, each tunnel
//!     becomes one `adw::PreferencesGroup` titled by name ("wg0"), subtitle by
//!     kind (e.g. `WireGuard`), with rx/tx rows and (for `WireGuard`) a nested
//!     peers expander. Empty state when no tunnel up.
//!
//! Backed by `hytte::services::{vpn, wifi}`. The page consumes signals only —
//! the UI layer never spawns processes or talks D-Bus directly.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::signal::Signal;
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::vpn;
use hytte::services::wifi;

use crate::components::format::{fmt_bytes, humanize_since};
use crate::components::layout::{finish_page, page_box};
use crate::components::markup;
use crate::components::reactive_list::reactive_list;

pub fn panel_vpn() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    // Saved NM VPN profiles (activate / deactivate) above the live view.
    // Hidden when there are no saved profiles, exactly like the wired card.
    let profiles_group = build_vpn_profiles_group();
    bind(
        wifi::vpn_profiles().map(|p| !p.is_empty()),
        &profiles_group,
        gtk::prelude::WidgetExt::set_visible,
    );
    column.append(&profiles_group);

    let header = adw::PreferencesGroup::builder()
        .title("Active tunnels")
        .build();
    bind(
        vpn::tunnels().map(|ts| match ts.len() {
            0 => "No VPN active".to_string(),
            1 => "1 tunnel up".to_string(),
            n => format!("{n} tunnels up"),
        }),
        &header,
        |g, txt| g.set_description(Some(&txt)),
    );
    column.append(&header);

    // Empty-state row, only visible when tunnels list is empty.
    let empty_group = adw::PreferencesGroup::new();
    let empty_row = adw::ActionRow::builder()
        .title("No VPN active")
        .activatable(false)
        .selectable(false)
        .build();
    empty_row.set_subtitle("Bring a WireGuard, OpenVPN, or Tailscale tunnel up to see it here.");
    empty_group.add(&empty_row);
    bind(
        vpn::tunnels().map(|ts| ts.is_empty()),
        &empty_group,
        gtk::prelude::WidgetExt::set_visible,
    );
    column.append(&empty_group);

    // Per-tunnel groups. Set is dynamic; drain & rebuild on each emission.
    bind_tunnel_groups(&column, vpn::tunnels());

    finish_page(&column)
}

/// Drain-rebuild the per-tunnel `PreferencesGroup`s into `column` from
/// `signal`. Split out of [`panel_vpn`] so this `bind` call site's `WeakRef`
/// contract (#772) can be driven with a synthetic signal in tests, the same
/// way `reactive_list` is (#761/#771).
fn bind_tunnel_groups<S>(column: &gtk::Box, signal: S)
where
    S: Signal<Item = Vec<vpn::Tunnel>> + 'static,
{
    let groups: Rc<RefCell<Vec<adw::PreferencesGroup>>> = Rc::new(RefCell::new(Vec::new()));
    bind(signal, column, move |column, tunnels| {
        let mut tracked = groups.borrow_mut();
        for g in tracked.drain(..) {
            column.remove(&g);
        }
        for tunnel in &tunnels {
            let g = build_tunnel_group(tunnel);
            column.append(&g);
            tracked.push(g);
        }
    });
}

// ── Saved NM VPN profiles ──────────────────────────────────────────────────────

/// Pill-styled state label, mirroring `panels::network::pill_label` (which is
/// `pub(super)` to the network module). Always vertically centered for use as an
/// `ActionRow` suffix.
fn pill_label(text: &str, variant_class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_valign(gtk::Align::Center);
    label.add_css_class("ts-net-pill");
    label.add_css_class(variant_class);
    label
}

/// Build the "Saved profiles" group of NM VPN connection profiles, each with an
/// Activate / Deactivate control. Drain-rebuilds its rows on every emission,
/// matching the wired card (cheap: VPN profiles are few and change rarely).
fn build_vpn_profiles_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Saved profiles")
        .build();

    bind(
        wifi::vpn_profiles().map(|p| match p.len() {
            0 => "No saved profiles".to_string(),
            1 => "1 saved profile".to_string(),
            n => format!("{n} saved profiles"),
        }),
        &group,
        |g, sub| g.set_description(Some(&sub)),
    );

    reactive_list(
        &group,
        wifi::vpn_profiles(),
        |profile: &wifi::VpnProfile| build_vpn_profile_row(profile),
        None::<fn() -> adw::ActionRow>,
    );

    group
}

fn build_vpn_profile_row(profile: &wifi::VpnProfile) -> adw::ActionRow {
    let subtitle = if profile.active {
        "Connected"
    } else {
        "Available"
    };

    let row = adw::ActionRow::builder()
        .title(&profile.name)
        .subtitle(subtitle)
        .activatable(false)
        .build();
    // Profile names come from NetworkManager's connection store; markup is
    // never wanted here (#753).
    markup::plain_text(&row);

    let icon = gtk::Image::from_icon_name("network-vpn-symbolic");
    row.add_prefix(&icon);

    if profile.active {
        row.add_suffix(&pill_label("Active", "ts-pill-connected"));
    } else {
        row.add_suffix(&pill_label("Inactive", "ts-pill-known"));
    }

    row.add_suffix(&build_vpn_row_menu(profile));
    row
}

fn build_vpn_row_menu(profile: &wifi::VpnProfile) -> gtk::MenuButton {
    let menu_btn = gtk::MenuButton::new();
    menu_btn.set_icon_name("view-more-symbolic");
    menu_btn.set_valign(gtk::Align::Center);
    menu_btn.add_css_class("flat");
    menu_btn.set_tooltip_text(Some("More actions"));

    let popover = gtk::Popover::new();
    let popover_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    popover_box.set_margin_top(4);
    popover_box.set_margin_bottom(4);
    popover_box.set_margin_start(4);
    popover_box.set_margin_end(4);

    if profile.active {
        // Deactivate targets the active-connection object path; only offered
        // when the profile is up (and that path was captured).
        if let Some(active) = profile.active_connection_path.clone() {
            let pop = popover.clone();
            let deactivate_btn = gtk::Button::with_label("Deactivate");
            deactivate_btn.add_css_class("flat");
            deactivate_btn.add_css_class("destructive-action");
            deactivate_btn.connect_clicked(move |_| {
                wifi::vpn_deactivate(&active);
                pop.popdown();
            });
            popover_box.append(&deactivate_btn);
        }
    } else {
        // Activate passes the saved-connection path (NM resolves the rest).
        let pop = popover.clone();
        let conn = profile.connection_path.clone();
        let activate_btn = gtk::Button::with_label("Activate");
        activate_btn.add_css_class("flat");
        activate_btn.connect_clicked(move |_| {
            wifi::vpn_activate(&conn);
            pop.popdown();
        });
        popover_box.append(&activate_btn);
    }

    popover.set_child(Some(&popover_box));
    menu_btn.set_popover(Some(&popover));
    menu_btn
}

fn build_tunnel_group(tunnel: &vpn::Tunnel) -> adw::PreferencesGroup {
    let kind_label = match tunnel.kind {
        vpn::TunnelKind::Wireguard => "WireGuard",
        vpn::TunnelKind::Tailscale => "Tailscale",
        vpn::TunnelKind::Tun => "tun",
        vpn::TunnelKind::Tap => "tap",
    };
    // `AdwPreferencesGroup` has no `use-markup` property to turn off — its
    // title and description labels are hardcoded `use-markup="True"` in
    // libadwaita's template — so escaping is the only fix here (#753).
    let g = adw::PreferencesGroup::builder()
        .title(markup::escape(&tunnel.name))
        .description(kind_label)
        .build();

    let transfer_row = adw::ActionRow::builder().title("Transfer").build();
    transfer_row.set_subtitle(&format!(
        "\u{2193} {} \u{2191} {}",
        fmt_bytes(tunnel.rx_bytes),
        fmt_bytes(tunnel.tx_bytes),
    ));
    g.add(&transfer_row);

    if let Some(summary) = tunnel.summary.as_ref() {
        let summary_row = adw::ActionRow::builder().title("Status").build();
        summary_row.set_subtitle(summary);
        g.add(&summary_row);
    }

    if let Some(since) = tunnel.since {
        let since_row = adw::ActionRow::builder().title("Since").build();
        since_row.set_subtitle(&humanize_since(since));
        g.add(&since_row);
    }

    if !tunnel.peers.is_empty() {
        let peers_expander = adw::ExpanderRow::builder()
            .title(format!("Peers ({})", tunnel.peers.len()))
            .build();
        for peer in &tunnel.peers {
            peers_expander.add_row(&build_peer_row(peer));
        }
        g.add(&peers_expander);
    }

    g
}

fn build_peer_row(peer: &vpn::Peer) -> adw::ActionRow {
    let key_short: String = peer.public_key.chars().take(8).collect();
    let row = adw::ActionRow::builder().title(&key_short).build();
    // Key, endpoint and allowed-IPs all come off the wire; the subtitle is
    // assembled from them below, and `plain_text` covers it too (#753).
    markup::plain_text(&row);
    row.add_css_class("ts-mono");
    let mut subtitle_parts: Vec<String> = Vec::new();
    if let Some(ep) = peer.endpoint.as_deref() {
        subtitle_parts.push(format!("via {ep}"));
    }
    if !peer.allowed_ips.is_empty() {
        subtitle_parts.push(format!("allowed: {}", peer.allowed_ips.join(", ")));
    }
    if let Some(hs) = peer.last_handshake {
        subtitle_parts.push(format!("handshake {}", humanize_since(hs)));
    } else {
        subtitle_parts.push("never handshaken".to_string());
    }
    subtitle_parts.push(format!(
        "\u{2193} {} \u{2191} {}",
        fmt_bytes(peer.rx_bytes),
        fmt_bytes(peer.tx_bytes),
    ));
    row.set_subtitle(&subtitle_parts.join(" \u{00b7} "));
    row
}

/// #772 regression coverage: the hand-rolled `bind` call site behind
/// [`bind_tunnel_groups`] must hold its container only weakly, exactly like
/// `reactive_list`'s own #761/#771 regression test.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::{bind_tunnel_groups, vpn};
    use hytte::adw::{self, prelude::*};
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk;

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// `bind_tunnel_groups` must not keep its `column` container alive by
    /// itself, per the #224 `WeakRef` contract at
    /// `hytte-reactive/src/bind.rs:16-22`. Falsified by reintroducing the
    /// `column_for_bind` strong clone the apply closure used to capture.
    #[gtk::test]
    fn tunnel_groups_binding_does_not_pin_column() {
        adw::init().expect("libadwaita init");
        let column = gtk::Box::new(gtk::Orientation::Vertical, 16);
        let weak = column.downgrade();
        let tunnels: Mutable<Vec<vpn::Tunnel>> = Mutable::new(Vec::new());
        bind_tunnel_groups(&column, tunnels.signal_cloned());
        pump();

        drop(column);

        assert!(
            weak.upgrade().is_none(),
            "bind_tunnel_groups must not pin its column: a strong clone captured by the apply \
             closure (rather than taking the closure's own `column` argument from `bind`) would \
             keep this alive for the life of the binding, defeating #224's WeakRef contract"
        );
    }
}
