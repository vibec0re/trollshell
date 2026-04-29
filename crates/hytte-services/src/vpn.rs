//! VPN tunnel service — surfaces active `WireGuard`, Tailscale, and
//! generic tun/tap tunnels as reactive signals.
//!
//! All process spawns live in this module; UI binds to the signals.
//! Polls every 5s. On each poll we:
//!
//! 1. Run `ip -d -j link show` and parse the JSON to find links whose
//!    `linkinfo.info_kind` is one of {wireguard, tun, tap}.
//! 2. For `WireGuard`: enrich with `wg show all dump` (peers, transfer,
//!    last-handshake).
//! 3. For an interface named `tailscale0` (and only when `tailscale`
//!    is on PATH): enrich with `tailscale status --json` (exit-node,
//!    self online state).
//! 4. For every kind: read `/sys/class/net/<n>/statistics/{rx,tx}_bytes`
//!    so we always have transfer numbers even when wg/tailscale aren't
//!    installed.
//!
//! Failures (binary missing, parse error, permission denied) log at
//! `tracing::warn!` and degrade gracefully — the tunnel is still
//! listed, just with empty peers / no summary.

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use std::time::{Duration, SystemTime};

// ── Public data shapes ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TunnelKind {
    Wireguard,
    Tailscale,
    Tun,
    Tap,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Peer {
    /// Full `WireGuard` public key. Renderers should redact to first 8 chars.
    pub public_key: String,
    pub endpoint: Option<String>,
    pub allowed_ips: Vec<String>,
    /// `None` if the peer has not yet completed a handshake.
    pub last_handshake: Option<SystemTime>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tunnel {
    pub name: String,
    pub kind: TunnelKind,
    /// Best-effort and often `None`. For `WireGuard`, derived from the
    /// oldest peer's `last_handshake`. For other kinds, left `None`.
    pub since: Option<SystemTime>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub peers: Vec<Peer>,
    /// Free-form summary line for non-Wireguard kinds — e.g. for
    /// Tailscale, the exit-node name. `None` when nothing to add.
    pub summary: Option<String>,
}

// ── Parsers ──────────────────────────────────────────────────────────────────

/// Parse a single line from `wg show all dump`. Returns
/// `Some((interface, peer))` for peer rows or `None` for the per-interface
/// "header" row that `wg` prints first for each interface.
///
/// `wg show all dump` line format:
///   header: <iface>\t<priv>\t<pub>\t<port>\t<fwmark>
///   peer:   <iface>\t<peer-pub>\t<presh>\t<endpoint>\t<allowed-ips>\t
///           <latest-handshake>\t<rx>\t<tx>\t<keepalive>
///
/// Header rows have 5 tab-separated columns; peer rows have 9. Use the
/// column count to discriminate.
pub(crate) fn parse_wg_show_dump_line(line: &str) -> Option<(String, Peer)> {
    let cols: Vec<&str> = line.split('\t').collect();
    if cols.len() != 9 {
        return None; // header row or malformed
    }
    let iface = cols[0].to_string();
    let public_key = cols[1].to_string();
    let endpoint = match cols[3] {
        "(none)" | "" => None,
        ep => Some(ep.to_string()),
    };
    let allowed_ips: Vec<String> = if cols[4] == "(none)" || cols[4].is_empty() {
        Vec::new()
    } else {
        cols[4].split(',').map(|s| s.trim().to_string()).collect()
    };
    let last_handshake = match cols[5] {
        "0" => None,
        ts => ts
            .parse::<u64>()
            .ok()
            .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
    };
    let rx_bytes = cols[6].parse::<u64>().unwrap_or(0);
    let tx_bytes = cols[7].parse::<u64>().unwrap_or(0);
    Some((
        iface,
        Peer {
            public_key,
            endpoint,
            allowed_ips,
            last_handshake,
            rx_bytes,
            tx_bytes,
        },
    ))
}

/// Parse the entire `wg show all dump` output into a per-interface peer
/// list.
pub(crate) fn parse_wg_show_dump(output: &str) -> std::collections::BTreeMap<String, Vec<Peer>> {
    let mut out: std::collections::BTreeMap<String, Vec<Peer>> = std::collections::BTreeMap::new();
    for line in output.lines() {
        if let Some((iface, peer)) = parse_wg_show_dump_line(line) {
            out.entry(iface).or_default().push(peer);
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkProbe {
    pub name: String,
    pub kind: TunnelKind,
}

/// Parse `ip -d -j link show` JSON into the subset of links we care about.
///
/// We accept any link whose `linkinfo.info_kind` is one of `wireguard`,
/// `tun`, `tap`. Any other (or missing `linkinfo`) is skipped silently.
pub(crate) fn parse_ip_link_json(json: &str) -> Vec<LinkProbe> {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in arr {
        let name = match entry.get("ifname").and_then(|s| s.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let info_kind = entry
            .get("linkinfo")
            .and_then(|li| li.get("info_kind"))
            .and_then(|k| k.as_str());
        let kind = match info_kind {
            // Some Tailscale builds report info_kind=tun for tailscale0;
            // WireGuard-based tailscale0 also maps to Tailscale.
            Some("wireguard" | "tun") if name == "tailscale0" => TunnelKind::Tailscale,
            Some("wireguard") => TunnelKind::Wireguard,
            Some("tun") => TunnelKind::Tun,
            Some("tap") => TunnelKind::Tap,
            _ => continue,
        };
        out.push(LinkProbe { name, kind });
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TailscaleStatus {
    pub backend_state: String,
    pub self_online: bool,
    pub exit_node: Option<String>,
}

/// Parse `tailscale status --json` for the bits we surface in the panel.
/// Errors yield `None`; the caller treats it as "tailscaled isn't telling
/// us anything useful" and shows the tunnel as a generic up/down.
pub(crate) fn parse_tailscale_status(json: &str) -> Option<TailscaleStatus> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let backend_state = v
        .get("BackendState")
        .and_then(|s| s.as_str())
        .unwrap_or("Unknown")
        .to_string();
    let self_online = v
        .get("Self")
        .and_then(|s| s.get("Online"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let exit_node = v
        .get("ExitNodeStatus")
        .and_then(|en| en.get("HostName"))
        .and_then(|s| s.as_str())
        .map(str::to_string);
    Some(TailscaleStatus {
        backend_state,
        self_online,
        exit_node,
    })
}

// ── Service ───────────────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct VpnHandles {
    pub(crate) tunnels: Mutable<Vec<Tunnel>>,
}

impl Default for VpnHandles {
    fn default() -> Self {
        Self {
            tunnels: Mutable::new(Vec::new()),
        }
    }
}

pub struct VpnService;

impl Service for VpnService {
    type Handles = VpnHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = VpnHandles::default();
        let writer = handles.tunnels.clone();
        rt.spawn(async move {
            poll_loop(writer).await;
        });
        handles
    }
}

async fn poll_loop(writer: Mutable<Vec<Tunnel>>) {
    loop {
        let next = collect_tunnels().await;
        // Avoid no-op re-emissions: the signal would still emit because
        // `set` always notifies, so compare and skip when unchanged.
        if writer.lock_ref().clone() != next {
            writer.set(next);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn collect_tunnels() -> Vec<Tunnel> {
    // Step 1: enumerate candidate tunnel-kind links via `ip -d -j link show`.
    let probes = match run_capture(&["ip", "-d", "-j", "link", "show"]).await {
        Some(out) => parse_ip_link_json(&out),
        None => Vec::new(),
    };
    if probes.is_empty() {
        return Vec::new();
    }

    // Step 2: enrich WireGuard with `wg show all dump`.
    let wg_peers = if probes.iter().any(|p| {
        matches!(p.kind, TunnelKind::Wireguard | TunnelKind::Tailscale)
    }) {
        match run_capture(&["wg", "show", "all", "dump"]).await {
            Some(out) => parse_wg_show_dump(&out),
            None => std::collections::BTreeMap::new(),
        }
    } else {
        std::collections::BTreeMap::new()
    };

    // Step 3: enrich Tailscale.
    let tailscale_status = if probes
        .iter()
        .any(|p| matches!(p.kind, TunnelKind::Tailscale))
    {
        run_capture(&["tailscale", "status", "--json"])
            .await
            .as_deref()
            .and_then(parse_tailscale_status)
    } else {
        None
    };

    // Step 4: build Tunnels.
    probes
        .into_iter()
        .map(|p| {
            let (rx_bytes, tx_bytes) = read_iface_stats(&p.name);
            let peers = wg_peers.get(&p.name).cloned().unwrap_or_default();
            let since = peers.iter().filter_map(|peer| peer.last_handshake).min();
            let summary = match p.kind {
                TunnelKind::Tailscale => tailscale_status.as_ref().map(|s| {
                    let exit = s.exit_node.as_deref().unwrap_or("none");
                    format!("{} · exit-node: {}", s.backend_state, exit)
                }),
                _ => None,
            };
            Tunnel {
                name: p.name,
                kind: p.kind,
                since,
                rx_bytes,
                tx_bytes,
                peers,
                summary,
            }
        })
        .collect()
}

async fn run_capture(argv: &[&str]) -> Option<String> {
    let prog = argv[0];
    let result = tokio::process::Command::new(prog)
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await;
    match result {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(_) => None, // non-zero exit; treat as "not telling us anything"
        Err(e) => {
            tracing::warn!(prog, error = %e, "vpn: spawn failed");
            None
        }
    }
}

fn read_iface_stats(name: &str) -> (u64, u64) {
    let rx = std::fs::read_to_string(format!("/sys/class/net/{name}/statistics/rx_bytes"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let tx = std::fs::read_to_string(format!("/sys/class/net/{name}/statistics/tx_bytes"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    (rx, tx)
}

#[must_use]
pub fn service() -> VpnService {
    VpnService
}

pub fn tunnels() -> impl Signal<Item = Vec<Tunnel>> {
    registry::with(|r| {
        r.get::<VpnHandles>()
            .expect("vpn::service() not registered")
            .tunnels
            .signal_cloned()
    })
}

pub fn is_active() -> impl Signal<Item = bool> {
    use futures_signals::signal::SignalExt;
    tunnels().map(|ts| !ts.is_empty())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wg_show_dump_extracts_peers() {
        // Realistic two-peer dump (one with handshake, one without).
        // First line is the interface header (5 cols, ignored).
        let output = "wg0\tprivkey=\tpubkey=\t51820\toff
wg0\tpeerA=\t(none)\t192.0.2.1:51820\t10.8.0.0/24,fd00::/64\t1714312345\t1024\t2048\t25
wg0\tpeerB=\t(none)\t(none)\t10.8.0.5/32\t0\t0\t0\t0";
        let parsed = parse_wg_show_dump(output);
        assert_eq!(parsed.len(), 1);
        let peers = &parsed["wg0"];
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].public_key, "peerA=");
        assert_eq!(peers[0].endpoint.as_deref(), Some("192.0.2.1:51820"));
        assert_eq!(peers[0].allowed_ips, vec!["10.8.0.0/24", "fd00::/64"]);
        assert!(peers[0].last_handshake.is_some());
        assert_eq!(peers[0].rx_bytes, 1024);
        assert_eq!(peers[0].tx_bytes, 2048);
        assert!(peers[1].endpoint.is_none());
        assert!(peers[1].last_handshake.is_none());
    }

    #[test]
    fn parse_ip_link_json_filters_to_tunnel_kinds() {
        let json = r#"[
            {"ifindex":1,"ifname":"lo"},
            {"ifindex":2,"ifname":"wlan0","linkinfo":{"info_kind":"vlan"}},
            {"ifindex":7,"ifname":"wg0","linkinfo":{"info_kind":"wireguard"}},
            {"ifindex":8,"ifname":"tailscale0","linkinfo":{"info_kind":"tun"}},
            {"ifindex":9,"ifname":"tun1","linkinfo":{"info_kind":"tun"}}
        ]"#;
        let probes = parse_ip_link_json(json);
        assert_eq!(probes.len(), 3);
        assert_eq!(probes[0].name, "wg0");
        assert_eq!(probes[0].kind, TunnelKind::Wireguard);
        assert_eq!(probes[1].name, "tailscale0");
        assert_eq!(probes[1].kind, TunnelKind::Tailscale);
        assert_eq!(probes[2].name, "tun1");
        assert_eq!(probes[2].kind, TunnelKind::Tun);
    }

    #[test]
    fn parse_ip_link_json_handles_garbage() {
        assert!(parse_ip_link_json("not json").is_empty());
        assert!(parse_ip_link_json("{}").is_empty());
    }

    #[test]
    fn parse_tailscale_status_reads_essentials() {
        let json = r#"{
            "BackendState": "Running",
            "Self": { "HostName": "blackforge", "Online": true },
            "ExitNodeStatus": { "HostName": "exit-via-eu", "Online": true }
        }"#;
        let s = parse_tailscale_status(json).unwrap();
        assert_eq!(s.backend_state, "Running");
        assert!(s.self_online);
        assert_eq!(s.exit_node.as_deref(), Some("exit-via-eu"));
    }

    #[test]
    fn parse_tailscale_status_handles_missing_exit_node() {
        let json = r#"{
            "BackendState": "Running",
            "Self": { "HostName": "blackforge", "Online": true }
        }"#;
        let s = parse_tailscale_status(json).unwrap();
        assert!(s.exit_node.is_none());
    }
}
