//! Per-process connection row — used by the Connections panel to render
//! one socket from `hytte::services::netconn::connections()`.

use hytte::adw::{self, prelude::*};
use hytte::services::netconn::{ConnState, Connection, Proto};

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
    let row = adw::ActionRow::builder().title(&title).build();
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
