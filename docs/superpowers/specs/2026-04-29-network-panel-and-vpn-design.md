# Network panel polish + VPN indicator + per-process connections

**Status:** design
**Date:** 2026-04-29
**Author:** Claude (with annika)

## Goal

Three coordinated improvements to network UX in trollshell:

1. **Network drawer page** reflow into a two-column "Config left, Live right" layout, with traffic sparklines and a per-process active-connections list. Tighten the over-large state pills.
2. **VPN indicator + panel** — new bar chip (visible only when a tunnel is up) and a dedicated drawer page showing per-tunnel detail. WireGuard, Tailscale, and generic tun/tap supported.
3. **Per-process connections (read-only)** — service exposing currently-open sockets (`ss -tunipnH`) with PID + program names, surfaced as a section at the bottom of the network page. eBPF byte-counts deferred to a future spec.

UI/service separation is a hard constraint: every fact the UI displays is read from a `hytte-services` signal. The bar widgets and drawer pages contain zero polling, parsing, or process-spawning logic.

## Motivation

- The current network page renders state pills at `.ts-net-pill { padding: 2px 10px; font-size: 0.8em }`, which dominates the rows visually. Other panels feel tighter; this one feels chunky.
- The traffic group renders only text ("↓ 1.2 MB/s ↑ 320 KB/s") even though the codebase already has `hytte-ui::Sparkline` used by the stats page. There's a missing graph here.
- There is no first-class VPN visibility. A user who toggles a WireGuard tunnel on/off has to inspect `wg show` or `ip a` from a terminal to confirm state. Tailscale users similarly. A bar indicator + dedicated panel is the obvious fix and the user's most-wanted feature in this round.
- "What process has my radio busy right now" is an everyday question that a shell ought to answer. `ss -tipn` makes the connections side cheap; per-PID byte counts are eBPF territory and out of scope.

## Scope

### In scope

- New module `crates/hytte-services/src/vpn.rs` exposing `tunnels() -> Signal<Vec<Tunnel>>` and `is_active() -> Signal<bool>`. Polls every 5s.
- New module `crates/hytte-services/src/netconn.rs` exposing `connections() -> Signal<Vec<Connection>>`. Polls every 2s.
- Both services registered in `main.rs` alongside existing services.
- New bar widget `trollshell/src/widgets/vpn.rs`. Chip visible only when `vpn::is_active()` is true; click opens `Page::Vpn`.
- New `Page::Vpn` variant in `trollshell/src/modal.rs` with `stack_name(self) → "vpn"`.
- New page builder `page_vpn()` in `trollshell/src/widgets/pages.rs`.
- `page_network()` reflow into two columns + sparkline-augmented traffic group + active-connections section.
- CSS tweaks in `trollshell/style.css`: tighten `.ts-net-pill`; add `.ts-pill-vpn` variant.
- New `docs/FUTURE.md` tracking deferred ideas (initial entry: eBPF per-PID byte counts).
- Parser unit tests for `wg show all dump`, `ip -d -j link show` JSON, `tailscale status --json`, and `ss -tunipnH` output. Tests live in the service modules and use frozen sample fixtures inline.

### Out of scope

- **eBPF per-PID byte counts.** Tracked in `docs/FUTURE.md`. Requires `aya`, kernel ≥ 5.13, CAP_BPF on the trollshell binary. Distinct enough to warrant its own spec when the work happens.
- **VPN connect/disconnect actions.** Read-only panel. Toggling tunnels is vendor- and config-specific (`wg-quick up X`, `nmcli con up X`, `tailscale up`); each takes its own UX work.

  > **Retracted (2026-07-30).** Shipped in #169 (2026-06-23):
  > `crates/hytte-services/src/wifi/mod.rs`'s `vpn_activate`/`vpn_deactivate`
  > drive the `Activate`/`Deactivate` buttons in `trollshell/src/panels/vpn.rs`.
  > The bullet above is kept for history; the code now does exactly what it
  > says is out of scope. `docs/FUTURE.md`'s matching entry has been removed.

- **Connections filtering / search.** Top-N sorted by program is sufficient for v1. Search box is later if the list proves unwieldy.
- **Port-name resolution.** Show numeric ports. Resolving via `/etc/services` is cheap but adds noise; defer.
- **Wi-Fi roaming history, signal-strength graph, hidden-network entry.** User explicitly out-of-scoped Wi-Fi UX work for this pass.
- **Mobile/cellular modems.**
- **`networkd` Link kind extension.** The new `vpn` service stands on its own; no need to thread a `kind` field through the existing networkd Link struct.

## Architecture

```
                    ┌─────────────────────────────┐
                    │  hytte-services (library)   │
                    │                             │
   ip -d -j link    │   vpn::tunnels() ─────┐     │
   wg show          │   vpn::is_active() ───┤     │
   tailscale status │                       │     │
                    │                       │     │
   ss -tunipnH      │   netconn::conns() ───┤     │
                    │                       │     │
                    └───────────────────────│─────┘
                                            │
                                  Signals (futures-signals)
                                            │
                    ┌───────────────────────│─────┐
                    │  trollshell (binary)  │     │
                    │                       ▼     │
                    │  widgets::vpn ─── bar chip  │
                    │  widgets::network — bar     │
                    │  pages::page_vpn ── modal   │
                    │  pages::page_network — modal│
                    └─────────────────────────────┘
```

The two new services are independent of each other and of `networkd`. Each does its own polling, its own parsing, its own signal emission. UI binds to the signals; UI never spawns a process.

## Service: `hytte-services::vpn`

### Public API

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TunnelKind {
    Wireguard,
    Tailscale,
    Tun,
    Tap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// WireGuard public key, redacted to first 8 chars in renderings.
    pub public_key: String,
    pub endpoint: Option<String>,
    pub allowed_ips: Vec<String>,
    /// `None` if the peer has not yet completed a handshake.
    pub last_handshake: Option<SystemTime>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tunnel {
    pub name: String,                 // e.g. "wg0", "tailscale0", "tun0"
    pub kind: TunnelKind,
    /// Tunnel went up at this time. Best-effort and often `None`.
    /// For WireGuard we surface the oldest peer's `last_handshake` as a
    /// proxy (closest thing to a tunnel birth time without reading
    /// netlink IFLA_OPERSTATE_CHANGE_NS, which is unstable). For other
    /// kinds, left as `None` in v1; the renderer just omits the row.
    pub since: Option<SystemTime>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub peers: Vec<Peer>,             // empty for non-Wireguard
    /// Free-form summary line for non-Wireguard kinds — e.g. for
    /// Tailscale, the exit-node name; for plain tun, currently `None`.
    pub summary: Option<String>,
}

pub fn service() -> VpnService;
pub fn tunnels() -> impl Signal<Item = Vec<Tunnel>>;
pub fn is_active() -> impl Signal<Item = bool>;
```

### Implementation

- Tokio task started by `service()`; polls every 5 s.
- Each poll:
  1. `ip -d -j link show` (json) → enumerate links with kind in {`wireguard`, `tun`, `tap`}.
  2. For WireGuard: `wg show all dump` parses to peer list per interface.
  3. For Tailscale (interface name `tailscale0` AND `tailscale` binary on PATH): `tailscale status --json` enriches the tunnel with `summary` (e.g. exit-node, online/offline state).
  4. For generic tun/tap: just rx/tx from `/sys/class/net/<n>/statistics/{rx,tx}_bytes`.
- Failures (binary missing, parse error, permission denied) log at `tracing::warn!` once per kind per session and degrade gracefully — e.g. WireGuard listed as a tunnel with empty peers if `wg` isn't installed.

### Tests

- `parse_wg_show_dump` — frozen sample with two peers (one with handshake, one without).
- `parse_ip_link_json` — frozen sample with three links: wg0, tailscale0, eth0 (latter must be filtered out).
- `parse_tailscale_status_json` — frozen sample.
- `is_active_when_any_tunnel_up` — pure-data test on the signal.

## Service: `hytte-services::netconn`

### Public API

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto { Tcp, Tcp6, Udp, Udp6 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnState { Established, Listen, TimeWait, Close, Other }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    pub proto: Proto,
    pub local: SocketAddr,
    pub remote: Option<SocketAddr>,    // None for LISTEN
    pub state: ConnState,
    pub pid: Option<u32>,              // None when ss can't see the owning process
    pub program: Option<String>,       // None when pid is None
}

pub fn service() -> NetconnService;
pub fn connections() -> impl Signal<Item = Vec<Connection>>;
```

### Implementation

- Tokio task; polls every 2 s.
- Spawns `ss -tunipnH` (TCP+UDP, IPv4+IPv6, with PID, numeric, no header) and parses the line-oriented output.
- The PID/program column (`users:(("name",pid=N,fd=M))`) is parsed if present; absent for sockets owned by other UIDs unless trollshell runs as root (we don't).
- Sorted by `(program ?? "~unknown")` ascending, then by `local`.

### Tests

- `parse_ss_line_with_users` — frozen sample of an `ss` output line including the `users:((..))` column.
- `parse_ss_line_without_users` — same, omitted.
- `parse_ss_listen_state` — `LISTEN` rows with no remote.

## Bar widget: `widgets::vpn`

```rust
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-vpn");
    let icon = gtk::Image::from_icon_name("network-vpn-symbolic");
    btn.set_child(Some(&icon));

    bind_visible(vpn::is_active(), &btn);

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Vpn);
    });
    btn.upcast()
}
```

Mounted in `main.rs::build_bar` immediately after `widgets::network::widget`, inside the same group:

```rust
group([
    widgets::bluetooth::widget(monitor),
    widgets::network::widget(monitor),
    widgets::vpn::widget(monitor),
]),
```

## Modal page: `page_vpn()`

Layout:

- Header preferences-group description: live count, e.g. "2 tunnels up" or "No VPN active".
- One `adw::PreferencesGroup` per active tunnel, titled `<name>` ("wg0"), subtitle `<kind>` ("WireGuard").
  - Action rows: Since · RX · TX.
  - Optional sparkline row (rx+tx) — single 60-sample sparkline, same `hytte_ui::Sparkline` reused.
  - For WireGuard: nested `adw::ExpanderRow` "Peers (N)" with one row per peer: redacted public key (first 8 chars), endpoint, allowed-ips, last-handshake humanized.
  - For Tailscale: extra summary row (exit-node, MagicDNS suffix).
  - For Tun/Tap: just the rx/tx and optional summary.
- Empty state: when `tunnels()` is empty, single `adw::ActionRow` "No VPN active".

## Modal page: `page_network()` reflow

Top of the page becomes a `gtk::Grid` (via existing `page_grid()` helper used by stats page):

- **Left column** (configuration): existing `build_connection_group_v2()` (Primary expander, no-connection placeholder, all-links expander, DNS expander).
- **Right column** (live):
  - `build_traffic_group_v2()` extended with sparklines: per non-loopback interface a row built like `build_history_row(name)` with the interface name on the left, sparkline middle, current `↓X ↑Y` value on the right.
  - Existing `Total` row stays as a summary at the bottom of this group.
  - Existing `TCP` row stays.
  - `build_wifi_group_v2()` — moves to under traffic in the right column.

Below the grid (full width):

- New `adw::PreferencesGroup` "Active connections" with description bound to a count ("47 sockets, 12 with PID"). Each row built by a new `build_connection_row(c: &Connection)` helper:
  - Title: `program` (or "(unknown)") · pid in muted suffix.
  - Subtitle: `proto local→remote (state)` in monospace.
- Top-N rendering with N=30, sorted by program ascending. Connections with `pid=None` rendered in a separate `adw::ExpanderRow` "Other users (N)" so they don't dominate.

## CSS

```css
/* tightened from padding: 2px 10px; font-size: 0.8em */
.ts-net-pill {
  padding: 1px 8px;
  border-radius: 9999px;
  font-size: 0.72em;
  font-weight: 600;
}

/* New: VPN tunnel-state pill. */
.ts-pill-vpn {
  background: alpha(@success_color, 0.18);
  color: @success_color;
}
```

No other CSS touches. The two-column reflow is a `page_grid()` consequence; spacing is handled by the helper.

## Future tracking: `docs/FUTURE.md`

New top-level doc, plain markdown bullet list under H2 headings. Created with this spec, evolves over time. Initial content:

```markdown
# Future ideas

Tracked-but-unscheduled work. When something here gets a spec, move it
to `docs/superpowers/specs/` and remove the entry.

## Networking

- **eBPF per-PID byte counts.** Augment `hytte-services::netconn` with
  cgroup-attached eBPF (aya crate) to deliver real per-process rx/tx.
  Requires CAP_BPF on the trollshell binary; kernel ≥ 5.13. Distinct
  enough to warrant its own spec when scheduled.

## VPN

- **VPN connect/disconnect actions.** Read-only panel only for now;
  toggling tunnels is vendor- and config-specific.

## Network drawer

- **Connections search/filter UI.** Defer until top-N sorted-by-program
  proves insufficient.
- **Port-name resolution** via `/etc/services`.

## Wi-Fi

- **Hidden-network entry, signal-strength graph, roaming history.**
```

## Decomposition

The plan will sequence three independent commits/PRs, each independently testable and shippable:

1. **UI polish** — `page_network` two-column reflow + traffic sparklines + pill CSS. No new services. Smallest, fastest to ship; immediate visible win.
2. **VPN service + chip + page** — `vpn` module, `widgets::vpn`, `page_vpn`, `Page::Vpn` enum entry, `main.rs` registration.
3. **Per-process connections** — `netconn` module, `Active connections` section in `page_network`. Service registration in `main.rs`.

`docs/FUTURE.md` is created as part of (1) so subsequent commits can append to it freely.

## File touch summary

| File                                   | Change                                                                                     |
| -------------------------------------- | ------------------------------------------------------------------------------------------ |
| `crates/hytte-services/src/vpn.rs`     | new — ~250 LOC including parser tests                                                      |
| `crates/hytte-services/src/netconn.rs` | new — ~150 LOC including parser tests                                                      |
| `crates/hytte-services/src/lib.rs`     | `pub mod vpn;` `pub mod netconn;`                                                          |
| `trollshell/src/widgets/vpn.rs`        | new — ~30 LOC                                                                              |
| `trollshell/src/widgets/mod.rs`        | `mod vpn;` + `pub use`                                                                     |
| `trollshell/src/modal.rs`              | `Page::Vpn` enum variant + `stack_name` arm                                                |
| `trollshell/src/widgets/pages.rs`      | `page_vpn` (~120 LOC), `page_network` reflow + active-connections section (~150 LOC delta) |
| `trollshell/src/main.rs`               | `.with(vpn::service())` `.with(netconn::service())`, vpn chip in bar group                 |
| `trollshell/style.css`                 | `.ts-net-pill` tightened, `.ts-pill-vpn` added                                             |
| `docs/FUTURE.md`                       | new                                                                                        |

Net: roughly +700 / −30 LOC across services + UI + docs.

## Risks

- **`ss` output format drift.** `iproute2` is the source. Output is stable in practice but not formally specified. The parser uses lenient regex/split; an unknown column produces a `Connection` with the unknown bits as `None`. Frozen-sample tests catch a regression on the user's distro.
- **Tailscale not installed / not running.** The `tailscale status --json` call fails fast; the service still reports the `tailscale0` link as a generic tun if it exists.
- **`wg` requires read on `/var/run/wireguard/<name>.sock`** — owned by root. On Arch the wireguard-tools package ships `wg` setuid-root or via a polkit-aware wrapper; this is the user's existing setup. If `wg` cannot be invoked, peers list is empty and a one-time warn is logged.
- **`ip -d -j` JSON shape varies by iproute2 version.** Parsing is targeted at the `linkinfo.info_kind` field which has been stable since iproute2 4.x. A future iproute2 4.x release that drops or renames the field will need a parser bump; tests will catch it.
- **Connections poll cost.** Every 2s, the shell forks `ss`. On a workstation this is negligible (~1ms); on extremely small embedded targets it would be more. Out of scope.
- **Per-process connections privacy.** `ss -tunipn` only shows other users' sockets if trollshell is root, which it isn't. We surface what we see and label the rest "Other users (N)". No surprise.
