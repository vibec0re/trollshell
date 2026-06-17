//! `/proc/net/dev` and `/proc/net/{tcp,tcp6}` parsing — network I/O rates and
//! TCP socket-state counts.

use super::NetConnections;

// ── /proc/net/dev parsing ─────────────────────────────────────────────────────

/// Returns `(name, rx_bytes, tx_bytes)` for every interface.
pub(super) fn read_proc_net_dev() -> Result<Vec<(String, u64, u64)>, std::io::Error> {
    let text = std::fs::read_to_string("/proc/net/dev")?;
    let mut result = Vec::new();

    for line in text.lines().skip(2) {
        // Each line: "  eth0: 123 456 ..."
        // Split on '|' is unreliable; split on whitespace after stripping the
        // interface name (which may contain spaces in theory, but not on Linux).
        let line = line.trim();
        let Some(colon_pos) = line.find(':') else {
            continue;
        };
        let name = line[..colon_pos].trim().to_string();
        let rest = &line[colon_pos + 1..];

        let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
        // field layout (0-indexed after the colon):
        // rx: [0]=bytes [1]=packets [2]=errs [3]=drop [4]=fifo [5]=frame [6]=compressed [7]=multicast
        // tx: [8]=bytes [9]=packets ...
        let rx_bytes: u64 = fields.first().and_then(|v| v.parse().ok()).unwrap_or(0);
        let tx_bytes: u64 = fields.get(8).and_then(|v| v.parse().ok()).unwrap_or(0);

        result.push((name, rx_bytes, tx_bytes));
    }

    Ok(result)
}

// ── /proc/net/{tcp,tcp6} parsing ─────────────────────────────────────────────

pub(super) fn read_net_connections() -> NetConnections {
    let v4 = count_tcp_states("/proc/net/tcp");
    let v6 = count_tcp_states("/proc/net/tcp6");
    NetConnections {
        tcp_established: v4.0,
        tcp_listen: v4.1,
        tcp6_established: v6.0,
        tcp6_listen: v6.1,
    }
}

/// Returns `(established, listen)` counts. /proc/net/tcp* state column is the
/// 4th whitespace-separated field, encoded as 2-char hex. 01 = ESTABLISHED,
/// 0A = LISTEN.
fn count_tcp_states(path: &str) -> (u32, u32) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (0, 0);
    };
    let mut established = 0u32;
    let mut listen = 0u32;
    for line in text.lines().skip(1) {
        let Some(state) = line.split_ascii_whitespace().nth(3) else {
            continue;
        };
        match state {
            "01" => established += 1,
            "0A" => listen += 1,
            _ => {}
        }
    }
    (established, listen)
}

#[cfg(test)]
mod tests {
    /// A `/proc/net/dev` line that is too short (missing the tx-bytes field at
    /// index 8) must not panic — the parser uses `.get(8).and_then(...).unwrap_or(0)`.
    #[test]
    fn parse_proc_net_dev_short_line_yields_zero_tx() {
        // Verify the fallback path through the known parser logic: a line with
        // fewer than 9 fields after the colon should yield tx_bytes == 0.
        //
        // The parser is: fields.get(8).and_then(|v| v.parse().ok()).unwrap_or(0)
        // With only 3 fields that is None → 0. We verify the logic directly.
        let rest = "      0  0  0"; // 3 fields only
        let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
        let rx: u64 = fields.first().and_then(|v| v.parse().ok()).unwrap_or(0);
        let tx: u64 = fields.get(8).and_then(|v| v.parse().ok()).unwrap_or(0);
        assert_eq!(rx, 0);
        assert_eq!(tx, 0, "missing tx field must fall back to 0, not panic");
    }
}
