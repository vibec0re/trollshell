# Network Panel + VPN + Per-Process Connections Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Three coordinated network UX improvements: (1) network drawer two-column reflow with traffic sparklines and tighter pill CSS, (2) VPN indicator + dedicated panel covering WireGuard/Tailscale/generic tun, (3) per-process active-connections section. Strict UI/service separation throughout.

**Architecture:** Two new `hytte-services` modules (`vpn`, `netconn`) own all polling, parsing, and signal emission — UI binds. Two new UI surfaces (`widgets::vpn` bar chip, `Page::Vpn` modal page) consume those signals. Existing `page_network()` is reflowed into a `page_grid()` two-column layout with sparkline-augmented traffic and an active-connections list at the bottom.

**Tech Stack:** Rust 1.94, `tokio` (poll loops), `futures-signals` (reactive state), `hytte-reactive::Service` trait, `hytte-ui::Sparkline`, `gtk4` + `libadwaita`. External tools shelled-out: `ip` (iproute2), `wg` (wireguard-tools), `tailscale` (optional), `ss` (iproute2). All process spawns are owned by the new services — UI never spawns.

---

## Phases

The 11 tasks below ship in three independent phases. Each phase produces a working, revertable state.

- **Phase 1 (Tasks 1-4):** UI polish + scaffolding. Pure UI-side work — no new services, no behavior change beyond layout/CSS/sparkline rendering.
- **Phase 2 (Tasks 5-8):** VPN service + bar chip + dedicated panel. WireGuard, Tailscale, generic tun/tap.
- **Phase 3 (Tasks 9-11):** Per-process connections via `ss -tunipnH`. Active-connections section in `page_network`.

Spec reference: `/home/choom/src/trollshell/docs/superpowers/specs/2026-04-29-network-panel-and-vpn-design.md`.

---

## File Structure

| File                                            | Responsibility                                                                                                                                                                         |
| ----------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `docs/FUTURE.md`                                | **New.** Index of deferred ideas. Initial entries: eBPF per-PID byte counts, VPN connect/disconnect, connections search, port-name resolution, Wi-Fi UX.                               |
| `trollshell/style.css`                          | **Modify.** Tighten `.ts-net-pill`. Add `.ts-pill-vpn`.                                                                                                                                |
| `trollshell/src/widgets/pages.rs::page_network` | **Reflow.** Two-column via existing `page_grid()`/`panel()` helpers. Traffic group gains per-interface sparklines. New "Active connections" section appended below the grid (Phase 3). |
| `trollshell/src/widgets/pages.rs::page_vpn`     | **New (Phase 2).** Per-tunnel cards. WireGuard peers expander. Tailscale special-casing.                                                                                               |
| `crates/hytte-services/src/vpn.rs`              | **New (Phase 2).** `Tunnel`/`TunnelKind`/`Peer` types. `tunnels()`, `is_active()` signals. Polls `ip -d -j link show`, `wg show all dump`, `tailscale status --json`.                  |
| `crates/hytte-services/src/netconn.rs`          | **New (Phase 3).** `Connection`/`Proto`/`ConnState` types. `connections()` signal. Polls `ss -tunipnH`.                                                                                |
| `crates/hytte-services/src/lib.rs`              | **Modify.** Add `pub mod vpn;` (Phase 2) and `pub mod netconn;` (Phase 3) alphabetically.                                                                                              |
| `trollshell/src/widgets/vpn.rs`                 | **New (Phase 2).** Bar chip. Visible only when `vpn::is_active()` is true.                                                                                                             |
| `trollshell/src/widgets/mod.rs`                 | **Modify.** `mod vpn;` (Phase 2).                                                                                                                                                      |
| `trollshell/src/modal.rs`                       | **Modify.** `Page::Vpn` enum variant. `stack_name` arm. Mount `pages::page_vpn()` keyed `"vpn"`.                                                                                       |
| `trollshell/src/main.rs`                        | **Modify.** `.with(vpn::service())` (Phase 2), `.with(netconn::service())` (Phase 3). Add `widgets::vpn::widget(monitor)` to the network/bluetooth bar group.                          |

---

## Phase 1: UI polish + scaffolding

### Task 1: Create `docs/FUTURE.md`

**Files:**

- Create: `docs/FUTURE.md`

- [ ] **Step 1: Write the new file**

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

- [ ] **Step 2: Verify the file is plain markdown and renders correctly**

Run: `head -5 docs/FUTURE.md`
Expected: shows the H1 heading and intro line.

- [ ] **Step 3: Commit**

```bash
git add docs/FUTURE.md
git commit -m "$(cat <<'EOF'
docs: add FUTURE.md for tracking deferred ideas

First inhabitants: eBPF per-PID byte counts, VPN connect/disconnect,
connections search, port-name resolution, Wi-Fi UX. Spec drove this.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Tighten `.ts-net-pill` and add `.ts-pill-vpn`

**Files:**

- Modify: `trollshell/style.css` (around line 559 for `.ts-net-pill`; add `.ts-pill-vpn` after the existing pill rules)

- [ ] **Step 1: Update `.ts-net-pill` and append `.ts-pill-vpn`**

Open `trollshell/style.css`. Find the block:

```css
.ts-net-pill {
  padding: 2px 10px;
  border-radius: 9999px;
  font-size: 0.8em;
  font-weight: 600;
}
```

Replace it with:

```css
.ts-net-pill {
  padding: 1px 8px;
  border-radius: 9999px;
  font-size: 0.72em;
  font-weight: 600;
}
```

Find the block ending with `.ts-pill-known` (around line 571):

```css
.ts-pill-known {
  background: alpha(@accent_color, 0.08);
  color: alpha(@accent_color, 0.7);
}
```

Insert immediately after it (with one blank line separator):

```css
/* New: VPN tunnel-state pill (used by page_vpn). */
.ts-pill-vpn {
  background: alpha(@success_color, 0.18);
  color: @success_color;
}
```

- [ ] **Step 2: Build to confirm CSS-load errors don't surface**

Run: `cargo build -p trollshell --message-format=short 2>&1 | tail -5`
Expected: `Finished` cleanly. (CSS is bundled at build time via `with_user_style`; compile errors here are exceptionally rare.)

- [ ] **Step 3: Commit**

```bash
git add trollshell/style.css
git commit -m "$(cat <<'EOF'
style(network): tighten .ts-net-pill, add .ts-pill-vpn

Pills were visually dominating panel rows. Drop padding to 1px 8px and
font-size to 0.72em. Adds a green .ts-pill-vpn variant the upcoming VPN
panel will use.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Reflow `page_network` top section into two-column

**Files:**

- Modify: `trollshell/src/widgets/pages.rs::page_network` (around lines 310-330)

- [ ] **Step 1: Inspect the current page_network entry point**

Run: `awk 'NR>=308 && NR<=332 {printf "%d: %s\n", NR, $0}' trollshell/src/widgets/pages.rs`
Expected: shows a single-column `page_box()` with three `column.append(...)` calls (connection group, traffic group, wifi group).

- [ ] **Step 2: Rewrite `page_network` to use a two-column grid**

Find this block:

```rust
pub fn page_network() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    column.append(build_connection_group_v2().upcast_ref::<gtk::Widget>());
    column.append(build_traffic_group_v2().upcast_ref::<gtk::Widget>());

    let wifi_group = build_wifi_group_v2();
    // Hide the Wi-Fi section entirely when no adapter is present (e.g. a
    // desktop machine with no wireless hardware) so the popup doesn't show
    // dead pixels / an empty group.
    bind(
        wifi::adapter().map(|a| a.is_some()),
        &wifi_group,
        gtk::prelude::WidgetExt::set_visible,
    );
    column.append(wifi_group.upcast_ref::<gtk::Widget>());

    finish_page(&column)
}
```

Replace with:

```rust
pub fn page_network() -> gtk::Widget {
    // Outer container holds the two-column grid up top, then full-width
    // sections (Phase 3 will append "Active connections" here too).
    let outer = page_box();
    outer.add_css_class("ts-popup-column");
    outer.set_spacing(16);

    let grid = page_grid();

    // Left column: configuration.
    let left = panel("Configuration");
    left.append(&build_connection_group_v2());
    grid.attach(&left, 0, 0, 1, 1);

    // Right column: live stats.
    let right = panel("Live");
    right.append(&build_traffic_group_v2());

    let wifi_group = build_wifi_group_v2();
    // Hide the Wi-Fi section entirely when no adapter is present (e.g. a
    // desktop machine with no wireless hardware).
    bind(
        wifi::adapter().map(|a| a.is_some()),
        &wifi_group,
        gtk::prelude::WidgetExt::set_visible,
    );
    right.append(&wifi_group);
    grid.attach(&right, 1, 0, 1, 1);

    outer.append(&grid);

    finish_page(&outer)
}
```

- [ ] **Step 3: Build the binary**

Run: `cargo build -p trollshell --message-format=short 2>&1 | tail -10`
Expected: `Finished` cleanly. Type errors here usually mean a `.upcast_ref::<gtk::Widget>()` was lost — `panel()` returns `gtk::Box`, which `Box::append` accepts directly.

- [ ] **Step 4: Run clippy on the binary**

Run: `cargo clippy -p trollshell --message-format=short 2>&1 | grep pages.rs`
Expected: no new warnings on `pages.rs`.

- [ ] **Step 5: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
refactor(network-page): two-column layout (Config left, Live right)

Reflows page_network's top section using the existing page_grid() /
panel() helpers (already used by page_stats and page_media). Wi-Fi
moves under Traffic in the right column. No content removed; this is
purely a layout change to make the panel less of a vertical wall.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Add per-interface traffic sparklines

**Files:**

- Modify: `trollshell/src/widgets/pages.rs::build_traffic_group_v2` (around lines 596-665)

- [ ] **Step 1: Inspect the current traffic group**

Run: `awk 'NR>=596 && NR<=665 {printf "%d: %s\n", NR, $0}' trollshell/src/widgets/pages.rs`
Expected: shows the live row, totals row, and TCP row.

- [ ] **Step 2: Replace `build_traffic_group_v2` with the sparkline-augmented version**

Find the existing function (lines 596-665). Replace its body so that the live "rate_row" is replaced by per-interface sparkline rows. Totals and TCP rows stay as-is.

```rust
fn build_traffic_group_v2() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Traffic").build();

    // Per-interface sparkline rows. The set of interfaces is dynamic
    // (hot-plug, VPN tunnels coming and going), so we drain & rebuild
    // on every emission of `sensors::network()` rather than holding
    // permanent per-interface state.
    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let group_for_bind = group.clone();
    let rows_for_bind = rows_track.clone();
    bind(
        sensors::network(),
        &group,
        move |_g, net| {
            // Drain previous rows.
            let mut tracked = rows_for_bind.borrow_mut();
            for row in tracked.drain(..) {
                group_for_bind.remove(&row);
            }
            // Build a fresh row per non-loopback interface, ordered by
            // name for stability.
            let mut interfaces: Vec<&sensors::NetInterface> =
                net.interfaces.iter().filter(|i| i.name != "lo").collect();
            interfaces.sort_by(|a, b| a.name.cmp(&b.name));
            for iface in interfaces {
                let row = build_iface_traffic_row(iface);
                group_for_bind.add(&row);
                tracked.push(row);
            }
        },
    );

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
            format!(
                "\u{2193} {} \u{2191} {}",
                fmt_bytes(rx),
                fmt_bytes(tx),
            )
        }),
        &totals_row,
        |row, text| row.set_subtitle(&text),
    );
    group.add(&totals_row);

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
    group.add(&tcp_row);

    group
}

/// One per-interface traffic row: name on the left, sparkline center,
/// current ↓rx ↑tx on the right. Built fresh each `sensors::network()`
/// emission since the interface set is dynamic.
fn build_iface_traffic_row(iface: &sensors::NetInterface) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(&iface.name).build();
    let suffix_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let spark = Sparkline::new(60);
    spark.set_width_request(120);
    // Seed with the current combined rate so the row isn't empty on
    // first paint. We push only one sample per emission below.
    spark.push((iface.rx_rate_bps + iface.tx_rate_bps) as f32);
    suffix_box.append(spark.widget());
    let value = gtk::Label::new(Some(&format!(
        "\u{2193} {} \u{2191} {}",
        fmt_rate(iface.rx_rate_bps),
        fmt_rate(iface.tx_rate_bps),
    )));
    value.add_css_class("ts-mono");
    suffix_box.append(&value);
    row.add_suffix(&suffix_box);
    row
}
```

- [ ] **Step 3: Build the binary**

Run: `cargo build -p trollshell --message-format=short 2>&1 | tail -10`
Expected: `Finished` cleanly. If `Sparkline::widget()` returns a wrong type, check the import — `Sparkline` is already imported at `pages.rs:37`.

- [ ] **Step 4: Run workspace tests**

Run: `cargo test --workspace --message-format=short 2>&1 | grep -E '(test result|FAILED)' | head -20`
Expected: every line is `test result: ok.`. No `FAILED`.

- [ ] **Step 5: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(network-page): per-interface traffic sparklines

The live row was text-only; the codebase already had hytte_ui::Sparkline
(used by page_stats). Each non-loopback interface now gets a row with
[name | sparkline(60) | ↓rx ↑tx]. Rebuilt on each sensors::network()
emission so hot-plug + VPN-tunnel coming/going just works.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 2: VPN service + indicator + page

### Task 5: `vpn` module — types + parsers + parser tests

**Files:**

- Create: `crates/hytte-services/src/vpn.rs`

- [ ] **Step 1: Create the new file with public types and parser fns**

Write `crates/hytte-services/src/vpn.rs`:

```rust
//! VPN tunnel service — surfaces active WireGuard, Tailscale, and
//! generic tun/tap tunnels as reactive signals.
//!
//! All process spawns live in this module; UI binds to the signals.
//! Polls every 5s. On each poll we:
//!
//! 1. Run `ip -d -j link show` and parse the JSON to find links whose
//!    `linkinfo.info_kind` is one of {wireguard, tun, tap}.
//! 2. For WireGuard: enrich with `wg show all dump` (peers, transfer,
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
    /// Full WireGuard public key. Renderers should redact to first 8 chars.
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
    /// Best-effort and often `None`. For WireGuard, derived from the
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
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Vec::new(),
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
            Some("wireguard") if name == "tailscale0" => TunnelKind::Tailscale,
            Some("wireguard") => TunnelKind::Wireguard,
            // Some Tailscale builds report info_kind=tun for tailscale0.
            Some("tun") if name == "tailscale0" => TunnelKind::Tailscale,
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
        .and_then(|b| b.as_bool())
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

// ── Service stub (filled in by Task 6) ───────────────────────────────────────

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

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        // Poll loop lands in Task 6.
        VpnHandles::default()
    }
}

#[must_use]
pub fn service() -> VpnService {
    VpnService
}

pub fn tunnels() -> impl Signal<Item = Vec<Tunnel>> {
    use futures_signals::signal::SignalExt;
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
```

- [ ] **Step 2: Verify `serde_json` is already a dep**

Run: `grep '^serde_json' crates/hytte-services/Cargo.toml`
Expected: `serde_json = "1"` (the existing dep, used by other services).

- [ ] **Step 3: Build (file is not yet wired into lib.rs, so this only checks syntax via the compiler when used elsewhere — skip and rely on Step 4)**

- [ ] **Step 4: Add `pub mod vpn;` to lib.rs (alphabetical position)**

Open `crates/hytte-services/src/lib.rs`. Find:

```rust
pub mod tray;
pub mod upower;
```

Insert `pub mod vpn;` after `pub mod upower;`:

```rust
pub mod tray;
pub mod upower;
pub mod vpn;
pub mod wallpaper;
```

(Note: `wallpaper` and `wifi` come after `vpn` alphabetically — keep them.)

- [ ] **Step 5: Run the new tests**

Run: `cargo test -p hytte-services vpn:: --message-format=short 2>&1 | tail -20`
Expected: 5 tests pass (`parse_wg_show_dump_extracts_peers`, `parse_ip_link_json_filters_to_tunnel_kinds`, `parse_ip_link_json_handles_garbage`, `parse_tailscale_status_reads_essentials`, `parse_tailscale_status_handles_missing_exit_node`).

- [ ] **Step 6: Run clippy**

Run: `cargo clippy -p hytte-services --tests --message-format=short 2>&1 | grep vpn.rs`
Expected: no output.

- [ ] **Step 7: Commit**

```bash
git add crates/hytte-services/src/vpn.rs crates/hytte-services/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(services/vpn): types + parsers + parser tests

Lays down the public types (TunnelKind, Peer, Tunnel) and three
parsers: wg show all dump, ip -d -j link show JSON, tailscale status
--json. Five unit tests cover the realistic shapes. The Service trait
is stubbed (returns empty handles); poll loop lands in the next task.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: `vpn` poll loop + main.rs registration

**Files:**

- Modify: `crates/hytte-services/src/vpn.rs` (replace the stubbed `start` with a real poll loop)
- Modify: `trollshell/src/main.rs` (add `.with(vpn::service())`)

- [ ] **Step 1: Replace the stubbed `start` with a tokio poll loop**

Open `crates/hytte-services/src/vpn.rs`. Find:

```rust
impl Service for VpnService {
    type Handles = VpnHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        // Poll loop lands in Task 6.
        VpnHandles::default()
    }
}
```

Replace with:

```rust
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
```

- [ ] **Step 2: Register `vpn::service()` in main.rs**

Open `trollshell/src/main.rs`. Find:

```rust
use hytte::services::{
    bluetooth, bluetooth_audio, brightness, calendar, clipboard, clock, displays, dnd, mpris,
    networkd, niri, notifications, notifications_mute, pipewire, polkit, power_profiles, resolved,
    screensaver, sensors, systemd, tray, upower, wallpaper, wifi,
};
```

Add `vpn` alphabetically (between `upower` and `wallpaper`):

```rust
use hytte::services::{
    bluetooth, bluetooth_audio, brightness, calendar, clipboard, clock, displays, dnd, mpris,
    networkd, niri, notifications, notifications_mute, pipewire, polkit, power_profiles, resolved,
    screensaver, sensors, systemd, tray, upower, vpn, wallpaper, wifi,
};
```

Find the `App::new(...)` chain and locate `.with(upower::service())`. Add `.with(vpn::service())` immediately after it. The block should look like:

```rust
        .with(upower::service())
        .with(vpn::service())
        .with(pipewire::service())
```

(If your local file orders services differently, just place `vpn` next to a sibling and keep workspace alphabetical-ish.)

- [ ] **Step 3: Build the workspace**

Run: `cargo build --workspace --message-format=short 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 4: Run all vpn tests (parser tests still pass)**

Run: `cargo test -p hytte-services vpn:: --message-format=short 2>&1 | tail -10`
Expected: 5 tests pass.

- [ ] **Step 5: Smoke-run the binary briefly to confirm the poll loop doesn't panic**

Run: `timeout 8 cargo run -p trollshell 2>&1 | grep -i 'vpn\|panic' | head -20 || true`
Expected: at most one `vpn: spawn failed` line if `wg`/`tailscale` aren't installed; no `panic`. If trollshell cannot start outside a Wayland session, this step is skipped — note as such and proceed.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/vpn.rs trollshell/src/main.rs
git commit -m "$(cat <<'EOF'
feat(services/vpn): poll loop and service registration

Wires up the vpn service to actually emit Tunnel updates every 5s by
fanning out ip -d -j link show + wg show all dump + tailscale status
--json + sysfs rx/tx counters. Failures degrade silently per service
contract. Registered in main.rs.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: `Page::Vpn` enum entry + `page_vpn()` builder

**Files:**

- Modify: `trollshell/src/modal.rs` (add `Vpn` to `Page` enum and `stack_name` arm; mount `pages::page_vpn()` in the stack)
- Modify: `trollshell/src/widgets/pages.rs` (add `pub fn page_vpn() -> gtk::Widget` and use the existing `vpn` import)

- [ ] **Step 1: Add `Vpn` to the `Page` enum**

Open `trollshell/src/modal.rs`. Find the enum:

```rust
pub enum Page {
    Media,
    Network,
    Bluetooth,
    Stats,
    Audio,
    Power,
    PowerMenu,
    Notifications,
    Appearance,
    Displays,
    Clipboard,
    Calendar,
    Settings,
}
```

Add `Vpn` after `Network` (where it belongs visually, next to the network chip):

```rust
pub enum Page {
    Media,
    Network,
    Vpn,
    Bluetooth,
    Stats,
    Audio,
    Power,
    PowerMenu,
    Notifications,
    Appearance,
    Displays,
    Clipboard,
    Calendar,
    Settings,
}
```

Find the `stack_name` impl. Add the `Vpn => "vpn"` arm in the corresponding position:

```rust
impl Page {
    fn stack_name(self) -> &'static str {
        match self {
            Self::Media => "media",
            Self::Network => "network",
            Self::Vpn => "vpn",
            Self::Bluetooth => "bluetooth",
            Self::Stats => "stats",
            Self::Audio => "audio",
            Self::Power => "power",
            Self::PowerMenu => "power-menu",
            Self::Notifications => "notifications",
            Self::Appearance => "appearance",
            Self::Displays => "displays",
            Self::Clipboard => "clipboard",
            Self::Calendar => "calendar",
            Self::Settings => "settings",
        }
    }
}
```

- [ ] **Step 2: Mount `page_vpn()` in the modal stack**

Inside `modal.rs::install`, find the block that adds named children to the `gtk::Stack` (look for `stack.add_named(&pages::page_network(), Some("network"))`). Add a corresponding line for `vpn`:

Find:

```rust
    stack.add_named(&pages::page_network(), Some(Page::Network.stack_name()));
```

Insert immediately after it:

```rust
    stack.add_named(&pages::page_vpn(), Some(Page::Vpn.stack_name()));
```

(If your modal.rs uses literal strings instead of `Page::*.stack_name()`, mirror whichever style is already there.)

- [ ] **Step 3: Add `page_vpn()` to pages.rs**

Open `trollshell/src/widgets/pages.rs`. Add an import for the `vpn` service. Find the existing service-imports block (around lines 17-37) and insert `use hytte::services::vpn;` alphabetically (between `upower` and `wallpaper`):

```rust
use hytte::services::upower::{self, Battery, BatteryState};
use hytte::services::vpn;
use hytte::services::wallpaper;
```

Then add the page builder. Find the `// ── Calendar page ──` separator (around line 3479 in the post-Phase-1 file). Insert immediately above it:

```rust
// ── VPN page ──────────────────────────────────────────────────────────────────

/// Drawer page for the active VPN tunnels.
///
/// Layout: header description shows live tunnel count. Each tunnel
/// becomes one `adw::PreferencesGroup` titled `<name>` ("wg0"),
/// subtitle `<kind>` ("WireGuard"), with rx/tx rows and (for
/// WireGuard) a nested peers expander. Empty state when no tunnel up.
///
/// Backed by `hytte::services::vpn`. The page consumes the `tunnels()`
/// signal — UI layer never spawns processes.
pub fn page_vpn() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    let header = adw::PreferencesGroup::builder().title("VPN").build();
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
    let groups_track: Rc<RefCell<Vec<adw::PreferencesGroup>>> = Rc::new(RefCell::new(Vec::new()));
    let column_for_bind = column.clone();
    let groups_for_bind = groups_track.clone();
    bind(
        vpn::tunnels(),
        &column,
        move |_col, tunnels| {
            let mut tracked = groups_for_bind.borrow_mut();
            for g in tracked.drain(..) {
                column_for_bind.remove(&g);
            }
            for tunnel in &tunnels {
                let g = build_tunnel_group(tunnel);
                column_for_bind.append(&g);
                tracked.push(g);
            }
        },
    );

    finish_page(&column)
}

fn build_tunnel_group(tunnel: &vpn::Tunnel) -> adw::PreferencesGroup {
    let kind_label = match tunnel.kind {
        vpn::TunnelKind::Wireguard => "WireGuard",
        vpn::TunnelKind::Tailscale => "Tailscale",
        vpn::TunnelKind::Tun => "tun",
        vpn::TunnelKind::Tap => "tap",
    };
    let g = adw::PreferencesGroup::builder()
        .title(&tunnel.name)
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
            .title(&format!("Peers ({})", tunnel.peers.len()))
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
    row.set_subtitle(&subtitle_parts.join(" · "));
    row
}

/// Render a SystemTime as a relative "Xs/m/h ago" or "in the future".
fn humanize_since(t: SystemTime) -> String {
    use std::time::SystemTime;
    let now = SystemTime::now();
    match now.duration_since(t) {
        Ok(d) => {
            let secs = d.as_secs();
            if secs < 60 {
                format!("{secs}s ago")
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86400)
            }
        }
        Err(_) => "moments from now".to_string(),
    }
}
```

Add `use std::time::SystemTime;` to the file's import block if not already present (the existing `Local`/`DateTime` imports are from `chrono`; they don't cover this).

Run: `grep -n 'use std::time::SystemTime' trollshell/src/widgets/pages.rs`
Expected: this returns a hit after editing. If not, add `use std::time::SystemTime;` to the std imports near the top of the file.

- [ ] **Step 4: Build**

Run: `cargo build -p trollshell --message-format=short 2>&1 | tail -10`
Expected: clean. If `vpn::tunnels()` complains about `Send`-ness, the parent `bind()` callback patterns elsewhere in `pages.rs` show the right shape.

- [ ] **Step 5: Run workspace tests**

Run: `cargo test --workspace --message-format=short 2>&1 | grep -E '(test result|FAILED)' | head -20`
Expected: every line is `test result: ok.`. No `FAILED`.

- [ ] **Step 6: Commit**

```bash
git add trollshell/src/modal.rs trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(ui/vpn): Page::Vpn enum entry + page_vpn() builder

Adds the modal stack child for the upcoming bar chip. Per-tunnel
group with transfer/since/summary rows + peers expander for
WireGuard. Empty-state fallback when no tunnel is up. All data
flows from hytte::services::vpn::tunnels() — UI never spawns.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: `widgets::vpn` bar chip + bar group wiring

**Files:**

- Create: `trollshell/src/widgets/vpn.rs`
- Modify: `trollshell/src/widgets/mod.rs` (add `pub mod vpn;`)
- Modify: `trollshell/src/main.rs` (add the chip to the network/bluetooth bar group)

- [ ] **Step 1: Create the bar chip widget**

Write `trollshell/src/widgets/vpn.rs`:

```rust
//! VPN bar chip — visible only when at least one VPN tunnel is up.
//!
//! Click opens `Page::Vpn` for the chip's monitor. All state comes from
//! `hytte::services::vpn::is_active()`; the widget itself does no
//! polling, no parsing, no process spawning.

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::vpn;

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

- [ ] **Step 2: Register the module in widgets/mod.rs**

Open `trollshell/src/widgets/mod.rs`. Find the `pub mod` lines (alphabetical). Insert `pub mod vpn;` in the right spot (between `tray` and any later module — `volume`, `window_list`, `workspaces`):

Run: `grep -n '^pub mod' trollshell/src/widgets/mod.rs` to confirm placement.

Add the line:

```rust
pub mod vpn;
```

In the alphabetical order observed (typically right after `tray` and before `volume`).

- [ ] **Step 3: Wire the chip into the bar group**

Open `trollshell/src/main.rs`. Find the `build_bar` function and the group containing `widgets::network::widget`:

```rust
            group([
                widgets::bluetooth::widget(monitor),
                widgets::network::widget(monitor),
            ]),
```

Replace with:

```rust
            group([
                widgets::bluetooth::widget(monitor),
                widgets::network::widget(monitor),
                widgets::vpn::widget(monitor),
            ]),
```

- [ ] **Step 4: Build the binary**

Run: `cargo build -p trollshell --message-format=short 2>&1 | tail -10`
Expected: clean. If clippy complains about an unused `monitor_for_click`, the existing `widgets::network` widget shows the canonical capture pattern.

- [ ] **Step 5: Run clippy on the binary**

Run: `cargo clippy -p trollshell --message-format=short 2>&1 | grep -E '(vpn.rs|main.rs)'`
Expected: no new warnings.

- [ ] **Step 6: Commit**

```bash
git add trollshell/src/widgets/vpn.rs trollshell/src/widgets/mod.rs trollshell/src/main.rs
git commit -m "$(cat <<'EOF'
feat(ui/vpn): bar chip + bar group wiring

Click opens Page::Vpn for that monitor. Visible only when
vpn::is_active() is true (binds via bind_visible). Sits in the
existing bluetooth/network group, immediately after network so the
cluster still groups by domain.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 3: Per-process connections

### Task 9: `netconn` module — types + parser + parser tests

**Files:**

- Create: `crates/hytte-services/src/netconn.rs`
- Modify: `crates/hytte-services/src/lib.rs` (add `pub mod netconn;`)

- [ ] **Step 1: Create the new file**

Write `crates/hytte-services/src/netconn.rs`:

```rust
//! Per-process active-connections service.
//!
//! Polls `ss -tunipnH` every 2s and parses the output into a flat list
//! of `Connection { proto, local, remote, state, pid, program }`. The
//! PID/program columns appear only for sockets owned by the running
//! user; trollshell does not run as root, so other users' sockets show
//! up with `pid = None` and the UI groups them at the bottom.
//!
//! Failures (`ss` missing, parse error) log once and the signal stays
//! at its last known value.

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Proto { Tcp, Tcp6, Udp, Udp6 }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnState {
    Established,
    Listen,
    TimeWait,
    Close,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Connection {
    pub proto: Proto,
    pub local: SocketAddr,
    pub remote: Option<SocketAddr>,
    pub state: ConnState,
    pub pid: Option<u32>,
    pub program: Option<String>,
}

// ── Parser ───────────────────────────────────────────────────────────────────

/// Parse a single line of `ss -tunipnH`. Returns `None` for unparseable
/// lines (we'd rather lose one line than panic the poll loop).
///
/// Expected line format with -tunipnH:
///   <netid> <state> <recv-q> <send-q> <local> <peer> [users:((..pid=N..))]
///
/// Where:
/// - netid is one of: tcp, udp
/// - state is one of: ESTAB, LISTEN, TIME-WAIT, CLOSE, UNCONN, ...
/// - local/peer are `IP:PORT` (or `[IPv6]:PORT`); peer = `*:*` for LISTEN
/// - users column is optional; absent for sockets we can't see the owner of
pub(crate) fn parse_ss_line(line: &str) -> Option<Connection> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Split off any `users:((...))` tail — it's space-delimited but the
    // contents include commas/quotes. We treat it as everything from
    // the first `users:` token onward.
    let (head, users_tail) = match trimmed.find("users:") {
        Some(idx) => (trimmed[..idx].trim_end(), Some(&trimmed[idx..])),
        None => (trimmed, None),
    };
    let cols: Vec<&str> = head.split_whitespace().collect();
    if cols.len() < 6 {
        return None;
    }
    let netid = cols[0];
    let state_str = cols[1];
    let local_str = cols[4];
    let peer_str = cols[5];

    let proto = match netid {
        "tcp" if local_str.contains('[') => Proto::Tcp6,
        "tcp" => Proto::Tcp,
        "udp" if local_str.contains('[') => Proto::Udp6,
        "udp" => Proto::Udp,
        _ => return None,
    };
    let state = match state_str {
        "ESTAB" => ConnState::Established,
        "LISTEN" => ConnState::Listen,
        "TIME-WAIT" => ConnState::TimeWait,
        "CLOSE" => ConnState::Close,
        _ => ConnState::Other,
    };
    let local = parse_addr(local_str)?;
    let remote = if peer_str == "*:*" || peer_str == "[::]:*" {
        None
    } else {
        parse_addr(peer_str)
    };
    let (pid, program) = users_tail
        .and_then(parse_users_field)
        .map(|(pid, prog)| (Some(pid), Some(prog)))
        .unwrap_or((None, None));

    Some(Connection {
        proto,
        local,
        remote,
        state,
        pid,
        program,
    })
}

/// Parse a single ss endpoint string: `1.2.3.4:80` or `[::1]:80` or
/// `0.0.0.0:22`. Wildcard-port `*` is rejected.
fn parse_addr(s: &str) -> Option<SocketAddr> {
    if s.contains('*') {
        return None;
    }
    s.parse::<SocketAddr>().ok()
}

/// Pulls `(pid, program)` out of a `users:(("name",pid=N,fd=M))`-style
/// tail. Returns the first entry; ss may list multiple but the first
/// is the canonical owner for our purposes.
fn parse_users_field(tail: &str) -> Option<(u32, String)> {
    // Strip leading `users:((` and trailing `))`.
    let inner = tail.strip_prefix("users:((")?.strip_suffix("))")?;
    // Now `inner` looks like: "name",pid=1234,fd=78  optionally repeated
    // separated by `),(`. Take only the first entry.
    let first_entry = inner.split("),(").next()?;
    let parts: Vec<&str> = first_entry.splitn(3, ',').collect();
    if parts.len() < 2 {
        return None;
    }
    let name_quoted = parts[0];
    let name = name_quoted.trim_matches('"').to_string();
    let pid_kv = parts[1];
    let pid_val = pid_kv.strip_prefix("pid=")?;
    let pid = pid_val.parse::<u32>().ok()?;
    Some((pid, name))
}

pub(crate) fn parse_ss_output(output: &str) -> Vec<Connection> {
    output.lines().filter_map(parse_ss_line).collect()
}

// ── Service ──────────────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct NetconnHandles {
    pub(crate) connections: Mutable<Vec<Connection>>,
}

impl Default for NetconnHandles {
    fn default() -> Self {
        Self {
            connections: Mutable::new(Vec::new()),
        }
    }
}

pub struct NetconnService;

impl Service for NetconnService {
    type Handles = NetconnHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NetconnHandles::default();
        let writer = handles.connections.clone();
        rt.spawn(async move {
            poll_loop(writer).await;
        });
        handles
    }
}

async fn poll_loop(writer: Mutable<Vec<Connection>>) {
    loop {
        if let Some(out) = run_ss().await {
            let next = parse_ss_output(&out);
            if writer.lock_ref().clone() != next {
                writer.set(next);
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_ss() -> Option<String> {
    let result = tokio::process::Command::new("ss")
        .args(["-tunipnH"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await;
    match result {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "netconn: ss spawn failed");
            None
        }
    }
}

#[must_use]
pub fn service() -> NetconnService {
    NetconnService
}

pub fn connections() -> impl Signal<Item = Vec<Connection>> {
    use futures_signals::signal::SignalExt;
    registry::with(|r| {
        r.get::<NetconnHandles>()
            .expect("netconn::service() not registered")
            .connections
            .signal_cloned()
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn parse_ss_line_with_users_field() {
        let line = "tcp ESTAB 0 0 192.168.1.10:54321 1.2.3.4:443 users:((\"firefox\",pid=1234,fd=78))";
        let c = parse_ss_line(line).unwrap();
        assert_eq!(c.proto, Proto::Tcp);
        assert_eq!(c.state, ConnState::Established);
        assert_eq!(c.local.ip(), IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)));
        assert_eq!(c.local.port(), 54321);
        assert_eq!(c.remote.unwrap().port(), 443);
        assert_eq!(c.pid, Some(1234));
        assert_eq!(c.program.as_deref(), Some("firefox"));
    }

    #[test]
    fn parse_ss_line_without_users_field() {
        let line = "tcp ESTAB 0 0 10.0.0.5:42210 8.8.8.8:443";
        let c = parse_ss_line(line).unwrap();
        assert_eq!(c.proto, Proto::Tcp);
        assert!(c.pid.is_none());
        assert!(c.program.is_none());
    }

    #[test]
    fn parse_ss_line_listen_state_has_no_remote() {
        let line = "tcp LISTEN 0 4096 0.0.0.0:22 *:* users:((\"sshd\",pid=900,fd=3))";
        let c = parse_ss_line(line).unwrap();
        assert_eq!(c.state, ConnState::Listen);
        assert!(c.remote.is_none());
        assert_eq!(c.pid, Some(900));
    }

    #[test]
    fn parse_ss_line_v6() {
        let line = "tcp ESTAB 0 0 [2001:db8::1]:54321 [2001:db8::2]:443";
        let c = parse_ss_line(line).unwrap();
        assert_eq!(c.proto, Proto::Tcp6);
        assert_eq!(
            c.local.ip(),
            IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap())
        );
    }

    #[test]
    fn parse_ss_line_garbage_is_none() {
        assert!(parse_ss_line("").is_none());
        assert!(parse_ss_line("just one column").is_none());
        assert!(parse_ss_line("ipv9 ESTAB 0 0 a:1 b:2").is_none());
    }

    #[test]
    fn parse_ss_output_runs_through_lines() {
        let output = "tcp ESTAB 0 0 10.0.0.5:42210 8.8.8.8:443
udp UNCONN 0 0 10.0.0.5:42424 1.1.1.1:53 users:((\"systemd-resolved\",pid=500,fd=12))
garbage line that should be skipped
tcp LISTEN 0 4096 0.0.0.0:22 *:*";
        let cs = parse_ss_output(output);
        assert_eq!(cs.len(), 3);
    }
}
```

- [ ] **Step 2: Add `pub mod netconn;` to lib.rs**

Open `crates/hytte-services/src/lib.rs`. Find:

```rust
pub mod mpris;
pub mod networkd;
```

Insert `pub mod netconn;` alphabetically after `networkd`:

```rust
pub mod mpris;
pub mod netconn;
pub mod networkd;
```

- [ ] **Step 3: Run the new tests**

Run: `cargo test -p hytte-services netconn:: --message-format=short 2>&1 | tail -15`
Expected: 6 tests pass.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy -p hytte-services --tests --message-format=short 2>&1 | grep netconn.rs`
Expected: no output.

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-services/src/netconn.rs crates/hytte-services/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(services/netconn): per-process active-connections service

New `netconn` module polls `ss -tunipnH` every 2s and exposes
`connections() -> Signal<Vec<Connection>>`. The Connection struct
carries proto/local/remote/state plus optional pid/program (None
when ss can't see the owner). Six unit tests cover the parser
including v6 endpoints, LISTEN with no remote, and garbage lines.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 10: Register `netconn::service()` in main.rs

**Files:**

- Modify: `trollshell/src/main.rs`

- [ ] **Step 1: Add `netconn` to the imports**

Open `trollshell/src/main.rs`. Find the `use hytte::services::{ ... }` block. Add `netconn` alphabetically (between `mpris` and `networkd`):

```rust
use hytte::services::{
    bluetooth, bluetooth_audio, brightness, calendar, clipboard, clock, displays, dnd, mpris,
    netconn, networkd, niri, notifications, notifications_mute, pipewire, polkit, power_profiles,
    resolved, screensaver, sensors, systemd, tray, upower, vpn, wallpaper, wifi,
};
```

- [ ] **Step 2: Register the service**

Find the chained `App::new(...)...with(...)` block. Add `.with(netconn::service())` near `mpris`/`networkd`:

```rust
        .with(mpris::service())
        .with(netconn::service())
        .with(networkd::service())
```

(Order in `.with()` chains is observation-only — it doesn't affect behavior, just consistency.)

- [ ] **Step 3: Build the binary**

Run: `cargo build -p trollshell --message-format=short 2>&1 | tail -5`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add trollshell/src/main.rs
git commit -m "$(cat <<'EOF'
feat(main): register netconn service

Wires the netconn poll loop into App::new(...).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 11: Active-connections section in `page_network`

**Files:**

- Modify: `trollshell/src/widgets/pages.rs::page_network` (append a new full-width section below the two-column grid)

- [ ] **Step 1: Add the netconn import**

Open `trollshell/src/widgets/pages.rs`. Find the `use hytte::services::networkd::{...}` line and add `netconn` alphabetically before `networkd`:

```rust
use hytte::services::netconn::{self, ConnState, Connection, Proto};
use hytte::services::networkd::{self, OperationalState};
```

- [ ] **Step 2: Append the active-connections section to `page_network`**

Open `trollshell/src/widgets/pages.rs`. Find `pub fn page_network()` (modified in Task 3). Just before the final `finish_page(&outer)` line, append:

```rust
    // Active connections — full-width section below the grid.
    let conn_group = adw::PreferencesGroup::builder()
        .title("Active connections")
        .build();
    bind(
        netconn::connections().map(|cs| {
            let total = cs.len();
            let with_pid = cs.iter().filter(|c| c.pid.is_some()).count();
            format!("{total} sockets, {with_pid} with PID")
        }),
        &conn_group,
        |g, txt| g.set_description(Some(&txt)),
    );

    // Top-level rows: own-user sockets sorted by program. Other users
    // (where ss can't see PID) collapse into a single expander at the
    // bottom so they don't dominate.
    let owned_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let other_expander = adw::ExpanderRow::builder().title("Other users").build();
    let other_rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    let group_for_bind = conn_group.clone();
    let owned_for_bind = owned_track.clone();
    let other_for_bind = other_expander.clone();
    let other_rows_for_bind = other_rows_track.clone();
    bind(
        netconn::connections(),
        &conn_group,
        move |_g, mut conns| {
            // Sort: own-user (has PID) first by program, then no-PID by local addr.
            conns.sort_by(|a, b| match (a.pid.is_some(), b.pid.is_some()) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (true, true) => a
                    .program
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.program.as_deref().unwrap_or("")),
                (false, false) => a.local.to_string().cmp(&b.local.to_string()),
            });

            // Drain previous rows.
            let mut owned = owned_for_bind.borrow_mut();
            for r in owned.drain(..) {
                group_for_bind.remove(&r);
            }
            let mut others = other_rows_for_bind.borrow_mut();
            for r in others.drain(..) {
                other_for_bind.remove(&r);
            }

            // Cap at 30 owned + N others (where N is also capped at 30).
            let mut owned_count = 0;
            let mut other_count = 0;
            let cap = 30;
            for c in conns.iter() {
                if c.pid.is_some() {
                    if owned_count >= cap {
                        continue;
                    }
                    let row = build_connection_row(c);
                    group_for_bind.add(&row);
                    owned.push(row);
                    owned_count += 1;
                } else {
                    if other_count >= cap {
                        continue;
                    }
                    let row = build_connection_row(c);
                    other_for_bind.add_row(&row);
                    others.push(row);
                    other_count += 1;
                }
            }

            other_for_bind.set_subtitle(&format!("{other_count} sockets"));
            other_for_bind.set_visible(other_count > 0);
        },
    );
    conn_group.add(&other_expander);
    outer.append(&conn_group);
```

Then add the helper fn at file scope (place it next to other `build_*_row` helpers, around the existing wifi row helpers near line 835):

```rust
/// Single-line render of an active connection: program (or "(unknown)")
/// + monospace `proto local→remote (state)` subtitle. Used by the
/// network drawer's Active connections section.
fn build_connection_row(c: &Connection) -> adw::ActionRow {
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
    let remote = c
        .remote
        .map(|a| format!(" → {a}"))
        .unwrap_or_default();
    row.set_subtitle(&format!("{proto} {}{remote} ({state})", c.local));
    row.add_css_class("ts-mono");
    row
}
```

- [ ] **Step 3: Build the binary**

Run: `cargo build -p trollshell --message-format=short 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy -p trollshell --message-format=short 2>&1 | grep pages.rs`
Expected: no new warnings.

- [ ] **Step 5: Run workspace tests**

Run: `cargo test --workspace --message-format=short 2>&1 | grep -E '(test result|FAILED)' | head -20`
Expected: every line is `test result: ok.`. No `FAILED`.

- [ ] **Step 6: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(network-page): active connections section (per-process)

Full-width "Active connections" group below the two-column grid.
Own-user sockets at the top sorted by program; other users collapsed
into an expander so they don't dominate. Top-N=30 each. Backed by
hytte::services::netconn — UI just binds.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Manual integration on a running session

**Files:** none (runtime verification only)

These steps require a real niri session and may need WireGuard/Tailscale + `ss`/`wg` binaries actually present.

- [ ] **Step 1: Launch trollshell**

Run: `cargo run -p trollshell` from a niri session terminal.
Expected: bar appears, network chip visible. With no VPN up, the VPN chip is hidden. Open the network drawer; the two-column layout, traffic sparklines, and Active connections section all render. Active connections lists at least firefox/whatever you have running.

- [ ] **Step 2: Bring a WireGuard tunnel up (if available)**

Run from another terminal: `sudo wg-quick up <your-config>`.
Expected: within ~5s the VPN chip appears next to the network chip. Click it; the VPN page shows the tunnel with peer info and last-handshake.

- [ ] **Step 3: Verify Tailscale special-casing (if installed)**

Run: `tailscale up`.
Expected: the page shows `tailscale0` with kind "Tailscale" and a Status row with backend state + exit-node info.

- [ ] **Step 4: Bring tunnels down and confirm chip disappears**

Run: `sudo wg-quick down <your-config>` (and/or `tailscale down`).
Expected: VPN chip hides within 5s; VPN page goes to empty state.

- [ ] **Step 5: Capture any anomalies in `BUGS.md`**

If any of the above misbehaves, append to `BUGS.md`. Do not commit code in this step.

---

## Self-Review

Spec coverage check (against `docs/superpowers/specs/2026-04-29-network-panel-and-vpn-design.md`, "Scope > In scope"):

- ✅ New `vpn.rs` with `tunnels()`, `is_active()`, polling stack — Tasks 5-6.
- ✅ New `netconn.rs` with `connections()`, polling — Task 9, 10.
- ✅ Both registered in main.rs — Task 6 (vpn), Task 10 (netconn).
- ✅ `widgets::vpn` bar chip — Task 8.
- ✅ `Page::Vpn` enum + `stack_name` arm — Task 7.
- ✅ `page_vpn()` builder — Task 7.
- ✅ `page_network()` two-column reflow — Task 3.
- ✅ Per-interface traffic sparklines — Task 4.
- ✅ Active connections section in network page — Task 11.
- ✅ CSS pill tightening + `.ts-pill-vpn` — Task 2.
- ✅ `docs/FUTURE.md` — Task 1.
- ✅ Parser unit tests for vpn (3 parsers) and netconn — Tasks 5 and 9.

No placeholders. Type names (`Tunnel`, `TunnelKind`, `Peer`, `Connection`, `Proto`, `ConnState`) consistent across tasks. Module paths (`hytte::services::vpn::*`, `hytte::services::netconn::*`) consistent. `bind_visible`, `bind`, `Sparkline`, `page_grid`, `panel`, `fmt_bytes`, `fmt_rate` are all existing helpers verified in the project before plan was written.
