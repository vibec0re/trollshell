# Network panel redesign — v0.2.2

**Status:** design
**Date:** 2026-04-25
**Author:** Claude (with annika)
**Predecessors:** `2026-04-24-hytte-trollshell-design.md` (v0.1 architecture), `2026-04-25-trollshell-v0.2.1-polish-design.md`.

## Goal

Restructure `trollshell/src/widgets/pages.rs::page_network` to follow native libadwaita conventions used by every other drawer page in trollshell, and extend the underlying `wifi`, `networkd`, and (already adequate) `resolved` services to surface IP addresses, gateway, routes, Wi-Fi adapter power, and known-network forgetting.

## Scope

### In scope

**UI work in `trollshell/src/widgets/pages.rs`:**

- Drop the 2-col `page_grid()` + `panel("…")` layout for `page_network`. Use vertical-stacked `AdwPreferencesGroup`s inside the existing `finish_page` Clamp.
- Three groups: `Connection`, `Traffic`, `Wi-Fi`.
- `Connection` uses three `AdwExpanderRow`s (Primary, All links, DNS); each expander reveals `AdwActionRow` children.
- `Traffic` keeps its current two informational rows but moves them into a native `AdwPreferencesGroup`.
- `Wi-Fi` uses an `AdwPreferencesGroup` with: live group description, header suffix carrying `Switch` + `Scan` + spinner, network list inside a bounded `ScrolledWindow`.
- Network rows: status moves to a pill suffix; ⋮ MenuButton popover holds `Connect` / `Disconnect` / `Forget` per row state.
- Status pill CSS classes added to `trollshell/style.css` using existing `@accent_color` token only.
- Helpers `panel()` and `page_grid()` stay in the file for other pages still using them.

**Service extensions:**

- `crates/hytte-services/src/networkd.rs` — extend `Link` with `addresses: Vec<IpAddr>`, `gateway_v4: Option<Ipv4Addr>`, `gateway_v6: Option<Ipv6Addr>`, `routes: Vec<RouteSummary>`. Source: `org.freedesktop.network1.Link.Describe()` JSON.
- `crates/hytte-services/src/wifi.rs` —
  - Add `Adapter { path, powered, name }` struct, `pub fn adapter() -> impl Signal<Item = Option<Adapter>>`, `pub fn set_powered(on: bool)`.
  - Surface `known_network_path: Option<String>` on `WifiNetwork` (currently parsed and discarded).
  - Add `pub fn forget(known_network_path: &str)`.
  - Migrate existing wifi command paths (`scan`, `connect_network`, `disconnect`, `submit_prompt`, `cancel_prompt`) to a shared `tokio::sync::OnceCell<Connection>` (own `CMD_CONN`, distinct from BlueZ's). The new `set_powered` and `forget` use the same shared connection.

**Tests:**

- `networkd::tests::parses_describe_json_minimal`
- `networkd::tests::handles_unknown_fields`
- `networkd::tests::default_route_populates_gateway_v4`
- `wifi::tests::known_network_path_round_trips`

(Adapter `Powered` round-trip and `Forget` aren't unit-testable without zbus mocks — leave to manual verification.)

**Cargo.toml additions for `hytte-services`:**

- `serde = { version = "1", features = ["derive"] }`
- `serde_json = "1"`

### Out of scope

- **Captive portal detection.** Active HTTP probe with retry/cache/timeout — large detour. `OperationalState::Routable` is the practical proxy; UI shows "Online via X" / "Limited connectivity via X" / "Offline" based on that.
- **DNS server editing.** Read-only; networkd / NM / VPN own the runtime servers.
- **802.1x enterprise certificate management.** Existing prompt remains; no change.
- **iwd advanced controls** (roaming, band selection, channel locking).
- **IPv6 router-advertisement specifics** (lifetime, prefixes).
- **MAC address row** on the primary expander. Adds Link field + Describe JSON parsing for one more value; out for v0.2.2 unless wanted in v0.2.3.
- **Live signal-strength bar visualization** (more than the dBm number + an icon). Static signal icon is kept.
- **Per-network detail subpage.** Drawer doesn't support subpages.

### Success criteria

- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` green; new parser tests pass.
- Manual:
  - Network drawer renders three native `AdwPreferencesGroup`s (`Connection`, `Traffic`, `Wi-Fi`) with no `panel()` wrappers.
  - Group description on Connection updates live with primary link state (`Online via wlp1s0`, etc.).
  - Primary expander reveals IPv4/IPv6 address + gateway rows.
  - All-links expander reveals one row per non-`lo` link.
  - DNS expander reveals one row per IP.
  - Wi-Fi power Switch toggles adapter via `Adapter1.Powered`; observed in `iwctl device list`.
  - Wi-Fi Scan button triggers a fresh scan; spinner shows while scanning.
  - Network row ⋮ popover offers Connect / Disconnect / Forget per state. Forget removes the known network in iwd's records (verify `iwctl known-networks list`).
  - Tapping an unknown secured network opens the existing password prompt.

## §1 — UI structure

`page_network()` returns a vertical `gtk::Box` (16px spacing) inside `finish_page` Clamp. The three groups stack:

```
finish_page(
    Box (vertical, 16px) {
        AdwPreferencesGroup [title="Connection", description=<live>]
            AdwExpanderRow primary
                AdwActionRow IPv4 address
                AdwActionRow IPv4 gateway
                AdwActionRow IPv6 address
                AdwActionRow IPv6 gateway
            AdwExpanderRow all_links
                AdwActionRow per Link
            AdwExpanderRow dns
                AdwActionRow per IpAddr

        AdwPreferencesGroup [title="Traffic"]
            AdwActionRow Live
            AdwActionRow Total

        AdwPreferencesGroup [title="Wi-Fi", description=<live>, header_suffix=Box[Switch, Scan button, Spinner]]
            ScrolledWindow (160-240) {
                PreferencesGroup container
                    AdwActionRow per WifiNetwork (rebuilt on networks() emission)
            }
    }
)
```

`AdwPreferencesGroup::set_description(&str)` is the right home for live state strings; it auto-styles dim and small.

`AdwPreferencesGroup::set_header_suffix(&impl IsA<gtk::Widget>)` accepts a horizontal Box containing the Switch/Button/Spinner cluster.

### Connection group

**Group description** (bind on `networkd::primary()`):

| Primary state                  | Description text             |
|--------------------------------|------------------------------|
| `Some(link)`, Routable          | `Online via {link.name}`     |
| `Some(link)`, Carrier or DegradedCarrier | `Limited connectivity via {link.name}` |
| `Some(link)`, anything else    | `{describe_state(op)} via {link.name}` |
| `None`                          | `Offline`                    |

**Primary expander row:**

- Title binds to primary link name; when `None`, hide the expander entirely and show a single non-activatable `AdwActionRow` titled `"No connection"` instead.
- Subtitle: op-state describing word.
- Nested `AdwActionRow`s on expand:
  - `"IPv4 address"` — suffix: comma-joined v4 addresses with prefix length (e.g. `192.168.1.42/24, 10.0.0.5/8`). Hidden when empty.
  - `"IPv4 gateway"` — suffix: gateway address. Hidden when None.
  - `"IPv6 address"` — suffix: first non-link-local v6 address `(+N more)` if there are extras. Hidden when empty.
  - `"IPv6 gateway"` — suffix: gateway address. Hidden when None.
- Suffix labels use the new `.ts-mono` CSS class for monospaced rendering.

**All-links expander row:**

- Title: `"All links"`.
- Subtitle: `"{N} interface(s)"` (count of non-`lo` links).
- Nested rows (one per non-`lo` `Link`): title=`link.name`, suffix=status pill matching network-row pill convention (`Online`, `Carrier`, `No carrier`, `Off`, etc.). Click does nothing.

**DNS expander row:**

- Title: `"DNS"`.
- Subtitle: `"{N} server(s)"` or `"Not configured"`.
- Nested rows: one `AdwActionRow` per `IpAddr` from `resolved::dns().servers`, title=monospace IP string. No prefix/suffix.

### Traffic group

Unchanged content. Existing two `AdwActionRow`s (`Live`, `Total`) move into a `AdwPreferencesGroup::builder().title("Traffic").build()`.

### Wi-Fi group

**Group description** (bind):

| Adapter / station state                             | Description text                       |
|-----------------------------------------------------|----------------------------------------|
| `wifi::adapter()` is `None`                         | `No adapter`                            |
| Adapter `powered=false`                              | `Disabled`                              |
| Connected                                            | `{ssid} · {dbm} dBm ({signal_label})`   |
| Connecting                                           | `Connecting…`                           |
| Roaming                                              | `Roaming`                               |
| Otherwise                                            | `Disconnected`                          |

`signal_label` derives from dBm bins matching the existing `signal_icon` thresholds: `excellent` (≥-50), `good` (≥-60), `ok` (≥-75), `weak` (else). Connected dBm comes from finding the `WifiNetwork` with `connected==true` in `wifi::networks()`; if no match (transient state), omit the `· {dbm} dBm (…)` fragment and just show the SSID.

**Header suffix:**

`gtk::Box` (horizontal, 8px spacing) containing:

- `gtk::Switch` — `bind_two_way` against `wifi::adapter().map(|a| a.is_some_and(|ad| ad.powered))`; user handler calls `wifi::set_powered(sw.is_active())`. `set_sensitive(adapter.is_some())` via a separate `bind`.
- `gtk::Button::with_label("Scan")` — `connect_clicked` calls `wifi::scan()`. `set_sensitive` bind on `adapter.is_some_and(powered) && !station.is_some_and(scanning)`.
- `gtk::Spinner` — visibility bind on `wifi::station().is_some_and(|s| s.scanning)`.

**Network list:**

`gtk::ScrolledWindow` (`hscrollbar_policy=Never`, `vscrollbar_policy=Automatic`, `min_content_height=160`, `max_content_height=240`, css class `ts-wifi-list`) wrapping a `gtk::Box` (vertical) containing the `PreferencesGroup` for network rows.

**Network row** (`build_network_row` rewrite):

- Prefix: `gtk::Image::from_icon_name(signal_icon(net.signal_dbm))`.
- Title: `&net.ssid`.
- Subtitle: `"{net.signal_dbm} dBm · {sec_label}"` where `sec_label` is `"Open"` for `"open"`, `"WPA2"` for `"psk"`, `"802.1x"` for `"8021x"`, otherwise the raw `security` string.
- Suffix 1 — status pill (`gtk::Label` styled with `.ts-net-pill` + variant class):
  - `connected == true` → label `"Connected"`, class `.ts-pill-connected`.
  - `known == true && !connected` → label `"Known"`, class `.ts-pill-known`.
  - else: pill omitted.
- Suffix 2 — `gtk::MenuButton`, icon `view-more-symbolic`, css `flat`, tooltip `"More actions"`. Popover contents per row state:
  - **Connected:** `Disconnect` button (`flat destructive-action`), `Forget` button (`flat destructive-action`). Both call the appropriate service fn then `popover.popdown()`.
  - **Known, not connected:** `Connect` button, `Forget` button (`flat destructive-action`).
  - **Unknown:** `Connect` button only.
- Row activation (`connect_activated`):
  - If `connected == true` → no-op.
  - Else → `wifi::connect_network(&net.path)`. iwd's existing prompt path handles passphrase requests via `wifi::active_prompt()` and `widgets/prompt.rs`.

**Empty-list placeholder:** when `wifi::networks()` is empty, render a single non-activatable `AdwActionRow` with title `"No networks found"`, subtitle `"Tap Scan to refresh"`. Removed when networks arrive.

**Power-off greying:**

When `adapter.powered == false`:
- Description shows `"Disabled"`.
- Scan button + ScrolledWindow `set_sensitive(false)`.
- Switch stays sensitive (it's how you turn it back on).

## §2 — Service extensions

### 2a — `networkd::Link` IP/gateway/routes

`Link` becomes:

```rust
#[derive(Clone, Debug, Default)]
pub struct Link {
    pub idx: i32,
    pub name: String,
    pub operational: OperationalState,
    pub addresses: Vec<IpAddr>,
    pub gateway_v4: Option<Ipv4Addr>,
    pub gateway_v6: Option<Ipv6Addr>,
    pub routes: Vec<RouteSummary>,
}

#[derive(Clone, Debug)]
pub struct RouteSummary {
    pub destination: IpAddr,
    pub prefix_len: u8,
    pub gateway: Option<IpAddr>,
    pub family: i32,
}
```

`OperationalState`, `links()`, `primary()` API unchanged.

In `read_links`, after reading `OperationalState`, also call:

```rust
let json: String = link_proxy.call("Describe", &()).await?;
```

Parse with serde_json into a small private `DescribeLink` shape mirroring the subset we need:

```rust
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct DescribeLink {
    addresses: Vec<DescribeAddress>,
    route_data: Vec<DescribeRoute>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct DescribeAddress {
    family: i32,
    address: Vec<u8>,
    prefix_length: u8,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct DescribeRoute {
    family: i32,
    destination: Vec<u8>,
    destination_prefix_length: u8,
    gateway: Option<Vec<u8>>,
}
```

`#[serde(default)]` keeps unknown fields from breaking the parse. Family 2 / 10 → IPv4 / IPv6 (same as resolved.rs). The existing `parse_addr` helper from resolved.rs is private; replicate (~10 lines) in networkd.rs rather than make resolved's pub.

A route with `destination` all-zero AND `prefix_length == 0` AND a `gateway` populates `gateway_v4` (family 2) or `gateway_v6` (family 10).

When Describe fails (parse error, missing JSON), log at `debug` and leave the new fields empty — `op_state` still works, and the listen loop continues.

### 2b — `wifi::Adapter` + `set_powered`

```rust
#[derive(Clone, Debug, Default)]
pub struct Adapter {
    pub path: String,
    pub powered: bool,
    pub name: String,
}
```

Stored on `WifiHandles` as `pub(crate) adapter: Mutable<Option<Adapter>>` next to `station`/`networks`/`prompts`.

The wifi listen loop already enumerates the iwd object tree to find the station (`/net/connman/iwd/<adapter_idx>/<phy>/<station>`). Walk one level up (path prefix `/net/connman/iwd/<adapter_idx>`) and read `net.connman.iwd.Adapter1` properties (`Powered`, `Name`, `Vendor`, `Model` — only `Powered` and `Name` retained).

`PropertiesChanged` subscription already covers `path_namespace("/net/connman/iwd")` so adapter property flips flow through the existing listener — extend the property-change handler to update the `Mutable<Option<Adapter>>` on `Adapter1` paths.

```rust
pub fn adapter() -> impl Signal<Item = Option<Adapter>> {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .adapter
            .signal_cloned()
    })
}

pub fn set_powered(on: bool) {
    runtime::handle().spawn(async move {
        let path = current_adapter_path().await;
        if path.is_empty() { return; }
        if let Err(e) = do_set_adapter_bool(&path, "Powered", on).await {
            tracing::warn!(error = %e, on, "wifi set_powered failed");
        }
    });
}
```

`do_set_adapter_bool` mirrors BlueZ's helper: shared `tokio::sync::OnceCell<Connection>` (named `CMD_CONN`, scoped to wifi.rs — distinct from BlueZ's), `Properties.Set` with iface `net.connman.iwd.Adapter1`.

### 2c — `wifi::WifiNetwork::known_network_path` + `forget()`

```rust
#[derive(Clone, Debug)]
pub struct WifiNetwork {
    pub path: String,
    pub ssid: String,
    pub security: String,
    pub known: bool,
    pub connected: bool,
    pub signal_dbm: i16,
    pub known_network_path: Option<String>,
}
```

`known_network_path` populated from the same iwd `KnownNetwork` property currently parsed at line 437 of `wifi.rs` and discarded after computing `known`. `known` stays a public `bool` derived field for ergonomics.

```rust
pub fn forget(known_network_path: &str) {
    let path = known_network_path.to_string();
    runtime::handle().spawn(async move {
        if let Err(e) = do_known_network_call(&path, "Forget").await {
            tracing::warn!(error = %e, path, "wifi forget failed");
        }
    });
}
```

`do_known_network_call` also uses `CMD_CONN`. iwd handles cascading disconnect when forgetting the active network; no chain logic needed.

### 2d — Migrate existing wifi commands to `CMD_CONN`

`scan`, `connect_network`, `disconnect`, `submit_prompt`, `cancel_prompt` each currently open their own `Connection::system().await` (mirroring the pre-Task-4 BlueZ pattern). Same identity-stability win as the BlueZ fix: route them through the new `CMD_CONN`.

The wifi listen loop's connection (used for `PropertiesChanged` subscription) stays separate — it has long-lived signal subscriptions that are independent of command identity, just like BlueZ's listen loop.

### 2e — Cargo.toml additions

```toml
[dependencies]
# ...existing...
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

(workspace policy: add only when there's no slimmer alternative; networkd's Describe JSON has no per-property accessor, so a parser is unavoidable, and serde + serde_json are conservative & well-vetted.)

## §3 — Tests

### `networkd::tests`

- `parses_describe_json_minimal` — synthetic Describe JSON containing one IPv4 address (192.168.1.42/24), one default route to 192.168.1.1, parsed into `Link.addresses=[V4(192.168.1.42)]`, `Link.gateway_v4=Some(192.168.1.1)`.
- `handles_unknown_fields` — extra unknown JSON fields don't break parsing (verifies `#[serde(default)]` tolerance).
- `default_route_populates_gateway_v4` — only routes with destination all-zero AND prefix_length==0 set the gateway; non-default routes don't.

### `wifi::tests`

- `known_network_path_round_trips` — `WifiNetwork::known_network_path` is `Some(path)` when iwd reports a non-`/` KnownNetwork path; `None` for `/`. (Use a small synthetic input map — not testing zbus end-to-end.)

Adapter Powered round-trip and Forget aren't unit-testable without zbus mocks. Manual verification step:
- `iwctl device list` should reflect Powered toggling.
- `iwctl known-networks list` should drop the SSID after Forget.

## §4 — Stylesheet

Append to `trollshell/style.css`:

```css
.ts-net-pill {
    padding: 2px 10px;
    border-radius: 9999px;
    font-size: 0.8em;
    font-weight: 600;
}

.ts-pill-connected {
    background: alpha(@accent_color, 0.20);
    color: @accent_color;
}

.ts-pill-known {
    background: alpha(@accent_color, 0.08);
    color: alpha(@accent_color, 0.70);
}

.ts-mono {
    font-family: monospace;
}
```

Existing `@accent_color` token reused — no new color tokens introduced (per project memory).

## §5 — Implementation hand-off

After approval, the writing-plans skill produces a step-by-step plan. Suggested decomposition:

1. **Service: networkd Link IP/gateway/routes** — Cargo deps + `Link` extension + Describe JSON parser + tests.
2. **Service: wifi shared CMD_CONN + Adapter + set_powered** — `OnceCell`, refactor existing commands, add adapter listening.
3. **Service: wifi known_network_path + forget** — surface path, add `forget()`.
4. **UI: Connection group** — `AdwPreferencesGroup` + Primary/All-links/DNS expanders.
5. **UI: Traffic group** — wrap existing rows in `AdwPreferencesGroup`.
6. **UI: Wi-Fi group** — header suffix Switch/Scan/Spinner, group description.
7. **UI: Network row rewrite** — pill suffix + ⋮ popover with state-driven actions.
8. **CSS additions** — `.ts-net-pill`, variants, `.ts-mono`.

Tasks 1-3 land in services; 4-8 land in `pages.rs` + `style.css`. Tasks 1-3 are independent; 4-8 are sequential (all touch `pages.rs::page_network`).
