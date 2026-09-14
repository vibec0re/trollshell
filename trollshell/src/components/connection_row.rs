//! Per-process connection row — used by the Connections panel to render
//! one socket from `hytte::services::netconn::connections()`.

use hytte::adw::{self, prelude::*};
use hytte::services::netconn::{ConnState, Connection, Proto};

use crate::components::format::truncate_for_row;
use crate::components::markup;

/// Top-N cap for each bucket of the active-connections section.
pub(crate) const CONN_BUCKET_CAP: usize = 30;

/// Single-line render of an active connection: program (or "(unknown)")
/// + monospace `proto local→remote (state)` subtitle.
pub(crate) fn build_connection_row(c: &Connection) -> adw::ActionRow {
    let title = match c.program.as_deref() {
        Some(p) => match c.pid {
            Some(pid) => format!("{p} · pid {pid}"),
            None => p.to_string(),
        },
        None => "(unknown)".to_string(),
    };
    let row = adw::ActionRow::builder()
        .title(truncate_for_row(&title))
        .build();
    // The program name is read out of `/proc`, so any process on the box
    // picks it — and the subtitle carries peer addresses (#753).
    markup::plain_text(&row);
    // A process name (or the "name · pid N" title above) has no bound in
    // principle — capped (see `truncate_for_row`) rather than letting it push
    // the whole drawer wider (#1302); `title_lines(1)` is a second line of
    // defense.
    row.set_title_lines(1);
    row.set_tooltip_text(Some(&title));
    let proto = match c.proto {
        Proto::Tcp => "tcp",
        Proto::Tcp6 => "tcp6",
        Proto::Udp => "udp",
        Proto::Udp6 => "udp6",
    };
    let state = match c.state {
        ConnState::Established => "ESTAB",
        ConnState::Listen => "LISTEN",
        ConnState::TimeWait => "TIME-WAIT",
        ConnState::Close => "CLOSE",
        ConnState::Other => "·",
    };
    let remote = c.remote.map(|a| format!(" → {a}")).unwrap_or_default();
    row.set_subtitle(&format!("{proto} {}{remote} ({state})", c.local));
    row.add_css_class("ts-mono");
    row
}

/// Needs a real display server (the row has to be constructible for
/// `measure` to mean anything), hence the `system-tests` gate, like the rest
/// of this bug class.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use std::net::SocketAddr;

    use hytte::adw::{self, prelude::*};
    use hytte::gtk;
    use hytte::services::netconn::{ConnState, Connection, Proto};

    use super::build_connection_row;

    /// One own-user socket named `program`.
    fn conn(program: &str) -> Connection {
        Connection {
            proto: Proto::Tcp,
            local: SocketAddr::from(([127, 0, 0, 1], 9001)),
            remote: None,
            state: ConnState::Established,
            pid: Some(1234),
            program: Some(program.to_string()),
        }
    }

    /// **The #1302 fix, pinned**: two `/proc` program names both past
    /// `truncate_for_row`'s cap, of different lengths, must render the row at
    /// the exact same width — `title_lines(1)` alone does not bound
    /// `AdwActionRow`'s natural width (see
    /// `components::format::truncate_for_row`'s doc) — and the tooltip must
    /// still carry the full "name · pid N" title.
    ///
    /// Falsified by reverting the `truncate_for_row` call in
    /// [`build_connection_row`] to the raw title: measured `left: 2463 /
    /// right: 806`.
    #[gtk::test]
    fn connection_row_title_ellipsises_a_long_program_name() {
        adw::init().expect("libadwaita init");
        let far_over = "x".repeat(300);
        let over = "x".repeat(90);

        let row_far = build_connection_row(&conn(&far_over));
        let (_, nat_far, _, _) = row_far.measure(gtk::Orientation::Horizontal, -1);
        let row_over = build_connection_row(&conn(&over));
        let (_, nat_over, _, _) = row_over.measure(gtk::Orientation::Horizontal, -1);
        let row_short = build_connection_row(&conn("abcde"));
        let (_, nat_short, _, _) = row_short.measure(gtk::Orientation::Horizontal, -1);

        assert_eq!(
            nat_far, nat_over,
            "two over-the-cap program names must render the row at the exact same width — \
             growth must stop at the cap"
        );
        assert!(
            nat_short < nat_far,
            "sanity: a 5-char name must measure narrower"
        );
        let tooltip = row_far
            .tooltip_text()
            .expect("the row's tooltip must be set");
        assert!(
            tooltip.contains(&far_over),
            "the tooltip must still carry the full, untruncated title, got {tooltip:?}"
        );
    }
}
