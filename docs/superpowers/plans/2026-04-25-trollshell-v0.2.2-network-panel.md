# trollshell v0.2.2 network panel redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure `page_network` to native libadwaita conventions and extend wifi/networkd services with IP/gateway/routes data, Wi-Fi adapter power, and known-network forget.

**Architecture:** Three vertically-stacked `AdwPreferencesGroup`s (Connection / Traffic / Wi-Fi) inside `finish_page` Clamp. Connection uses three `AdwExpanderRow`s (Primary, All-links, DNS) revealing IP/gateway/server detail rows. Wi-Fi group's header suffix carries the adapter Switch + Scan button + spinner. Network rows expose state-driven Connect/Disconnect/Forget in a ⋮ popover (per project memory: destructive actions in popovers).

**Tech Stack:** Rust 1.94 stable, GTK4 + libadwaita via `gtk4-rs`, `futures-signals`, `zbus`, `tokio`, plus new deps `serde` + `serde_json` for parsing networkd's `Describe()` JSON.

**Conventions used in every task:**
- TDD where unit tests are practical. UI tasks verify via `cargo build` + `cargo clippy --workspace --all-targets -- -D warnings` and a deferred manual smoke-test note.
- Commits use the existing project prefixes (`feat(de):`, `fix(de):`, `feat(...)` for service work, `polish:`, `style:`).
- Co-author trailer on every commit:
  `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`

**Spec backing this plan:** `docs/superpowers/specs/2026-04-25-network-panel-redesign-design.md`

---

## File Structure

**Modified files:**

- `crates/hytte-services/Cargo.toml` — add `serde` + `serde_json` deps.
- `crates/hytte-services/src/networkd.rs` — extend `Link` struct + add `RouteSummary` + `Describe()` JSON parser + tests.
- `crates/hytte-services/src/wifi.rs` — `Adapter` struct + listen-loop adapter discovery + `adapter()` signal + `set_powered()` + `forget()` + `WifiNetwork.known_network_path` + shared `CMD_CONN` command connection.
- `trollshell/src/widgets/pages.rs` — rewrite `page_network` + `build_connection_group` + `build_traffic_group` + `append_wifi_section` + `build_network_row`. Add small helpers (`describe_state_label`, `pill_class_for_link_state`, etc.).
- `trollshell/style.css` — append `.ts-net-pill`, `.ts-pill-connected`, `.ts-pill-known`, `.ts-mono`.

**No new files.**

---

## Task 1: Extend `networkd::Link` with IP / gateway / routes

**Files:**
- Modify: `crates/hytte-services/Cargo.toml`
- Modify: `crates/hytte-services/src/networkd.rs`

**Background:** `org.freedesktop.network1.Link.Describe()` returns a JSON `String` containing the link's full state — addresses, routes, neighbours, etc. networkd has no per-property accessors for these; `Describe` is the canonical source. We parse a small subset with `serde_json` to populate `Link.addresses`, `Link.gateway_v4`, `Link.gateway_v6`, `Link.routes`. Defaults are forgiving (`#[serde(default)]`) so unknown JSON fields don't break the listen tick.

- [ ] **Step 1: Add serde / serde_json to `hytte-services` Cargo.toml**

Edit `crates/hytte-services/Cargo.toml`. In the `[dependencies]` section (after the existing entries, alphabetical-ish), add:

```toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

Run `cargo build -p hytte-services` to confirm the workspace resolves the new deps (no other changes yet).

- [ ] **Step 2: Write the failing tests**

Append (or create) a `#[cfg(test)] mod tests` block at the bottom of `crates/hytte-services/src/networkd.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DESCRIBE: &str = r#"{
        "Index": 3,
        "Name": "wlp1s0",
        "OperationalState": "routable",
        "Addresses": [
            {"Family": 2, "Address": [192, 168, 1, 42], "PrefixLength": 24}
        ],
        "RouteData": [
            {
                "Family": 2,
                "Destination": [0, 0, 0, 0],
                "DestinationPrefixLength": 0,
                "Gateway": [192, 168, 1, 1]
            },
            {
                "Family": 2,
                "Destination": [192, 168, 1, 0],
                "DestinationPrefixLength": 24
            }
        ]
    }"#;

    #[test]
    fn parses_describe_json_minimal() {
        let parsed = parse_describe(SAMPLE_DESCRIBE).expect("parse");
        assert_eq!(parsed.addresses, vec![IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 42))]);
        assert_eq!(parsed.gateway_v4, Some(std::net::Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(parsed.gateway_v6, None);
        assert_eq!(parsed.routes.len(), 2);
    }

    #[test]
    fn handles_unknown_fields() {
        let json = r#"{
            "Index": 1,
            "FutureField": "anything",
            "Addresses": [{"Family": 2, "Address": [10, 0, 0, 1], "PrefixLength": 8, "ExtraJunk": 99}]
        }"#;
        let parsed = parse_describe(json).expect("parse");
        assert_eq!(parsed.addresses.len(), 1);
    }

    #[test]
    fn default_route_populates_gateway_v4() {
        let json = r#"{
            "RouteData": [
                {"Family": 2, "Destination": [10, 0, 0, 0], "DestinationPrefixLength": 8, "Gateway": [10, 0, 0, 1]},
                {"Family": 2, "Destination": [0, 0, 0, 0], "DestinationPrefixLength": 0, "Gateway": [192, 168, 0, 1]}
            ]
        }"#;
        let parsed = parse_describe(json).expect("parse");
        assert_eq!(parsed.gateway_v4, Some(std::net::Ipv4Addr::new(192, 168, 0, 1)));
    }
}
```

- [ ] **Step 3: Run tests and verify failure**

Run: `cargo test -p hytte-services networkd::tests -- --nocapture`
Expected: compile error — `parse_describe`, `RouteSummary`, etc. not defined.

- [ ] **Step 4: Add `RouteSummary` + extend `Link` + add the parser**

In `crates/hytte-services/src/networkd.rs`:

a) At the top of the file, ensure `IpAddr`, `Ipv4Addr`, `Ipv6Addr` are imported. Add to the existing `use std::...` block:

```rust
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
```

(The file currently has no `std::net` imports.)

b) Replace the existing `Link` struct with:

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

c) Add the private parse types + `parse_describe` function. Place them above the `#[cfg(test)] mod tests` block (or anywhere private; suggested location: just below `read_links`):

```rust
#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct DescribeLink {
    addresses: Vec<DescribeAddress>,
    route_data: Vec<DescribeRoute>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct DescribeAddress {
    family: i32,
    address: Vec<u8>,
    prefix_length: u8,
}

#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct DescribeRoute {
    family: i32,
    destination: Vec<u8>,
    destination_prefix_length: u8,
    gateway: Option<Vec<u8>>,
}

#[derive(Debug, Default)]
pub(crate) struct ParsedDescribe {
    pub addresses: Vec<IpAddr>,
    pub gateway_v4: Option<Ipv4Addr>,
    pub gateway_v6: Option<Ipv6Addr>,
    pub routes: Vec<RouteSummary>,
}

pub(crate) fn parse_describe(json: &str) -> anyhow::Result<ParsedDescribe> {
    let raw: DescribeLink = serde_json::from_str(json).context("parse Describe JSON")?;
    let mut out = ParsedDescribe::default();

    for a in raw.addresses {
        if let Some(ip) = bytes_to_ip(a.family, &a.address) {
            out.addresses.push(ip);
        }
    }

    for r in raw.route_data {
        let dest = match bytes_to_ip(r.family, &r.destination) {
            Some(d) => d,
            None => continue,
        };
        let gw = r.gateway.as_ref().and_then(|g| bytes_to_ip(r.family, g));
        let is_default = r.destination_prefix_length == 0
            && match dest {
                IpAddr::V4(v4) => v4.is_unspecified(),
                IpAddr::V6(v6) => v6.is_unspecified(),
            };
        if is_default {
            if let Some(IpAddr::V4(g4)) = gw {
                out.gateway_v4 = Some(g4);
            } else if let Some(IpAddr::V6(g6)) = gw {
                out.gateway_v6 = Some(g6);
            }
        }
        out.routes.push(RouteSummary {
            destination: dest,
            prefix_len: r.destination_prefix_length,
            gateway: gw,
            family: r.family,
        });
    }

    Ok(out)
}

fn bytes_to_ip(family: i32, bytes: &[u8]) -> Option<IpAddr> {
    match (family, bytes.len()) {
        (2, 4) => Some(IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]))),
        (10, 16) => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}
```

- [ ] **Step 5: Verify tests pass**

Run: `cargo test -p hytte-services networkd::tests -- --nocapture`
Expected: 3 passed.

- [ ] **Step 6: Wire `parse_describe` into the listen loop**

In `crates/hytte-services/src/networkd.rs`'s `read_links` function, after the existing `OperationalState` read (around line 151-159), add the Describe call before pushing to `out`:

Replace:

```rust
out.push(Link {
    idx,
    name,
    operational: OperationalState::parse(&op_state),
});
```

with:

```rust
let describe_json: String = link_proxy
    .call("Describe", &())
    .await
    .unwrap_or_default();
let parsed = parse_describe(&describe_json).unwrap_or_default();

out.push(Link {
    idx,
    name,
    operational: OperationalState::parse(&op_state),
    addresses: parsed.addresses,
    gateway_v4: parsed.gateway_v4,
    gateway_v6: parsed.gateway_v6,
    routes: parsed.routes,
});
```

If `Describe` returns an error or an empty string, `unwrap_or_default()` keeps the listen loop ticking with empty fields. Logging at debug for stale-data debugging is optional; v0.2.2 doesn't require it.

- [ ] **Step 7: Build + clippy**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add crates/hytte-services/Cargo.toml crates/hytte-services/src/networkd.rs
git commit -m "$(cat <<'EOF'
feat(networkd): expose IP addresses, gateway, and routes per Link

Parses the JSON returned by org.freedesktop.network1.Link.Describe()
into Link.addresses, gateway_v4, gateway_v6, and routes. Adds
serde + serde_json to hytte-services. Tolerant of unknown JSON
fields via #[serde(default)].

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Wifi shared `CMD_CONN`

**Files:**
- Modify: `crates/hytte-services/src/wifi.rs`

**Background:** Mirroring the BlueZ fix landed in v0.2.1 (`bluetooth.rs` Task 4), each wifi command (`scan`, `connect_network`, `disconnect`) currently opens a fresh `Connection::system().await`. iwd doesn't have the same per-client session model as BlueZ (so this isn't a *bug* like BUGS.md was), but consolidating to a single connection per service is the same correctness/efficiency win. The new `set_powered` and `forget` will use the same shared connection.

- [ ] **Step 1: Add the shared command connection accessor**

In `crates/hytte-services/src/wifi.rs`:

a) Confirm `tokio::sync::OnceCell` is importable. The crate enables tokio's `sync` feature already (added in v0.2.1 Task 4 for BlueZ); no Cargo change needed. Add at the top of the file alongside other tokio imports:

```rust
use tokio::sync::OnceCell;
```

b) Add a private accessor at the top of the `// ── Command helpers ───` section (around line 288, replacing the existing helpers):

```rust
/// Shared command-channel connection. Avoids opening a fresh system bus
/// connection on every iwd call. The listen loop keeps its own
/// connection because its long-lived signal subscriptions are
/// independent of command identity.
static CMD_CONN: OnceCell<Connection> = OnceCell::const_new();

async fn cmd_conn() -> Result<&'static Connection> {
    CMD_CONN
        .get_or_try_init(|| async {
            Connection::system()
                .await
                .context("open shared wifi command connection")
        })
        .await
}
```

- [ ] **Step 2: Refactor `do_station_call` to use the shared connection**

Replace the existing `do_station_call` (around line 290):

```rust
async fn do_station_call(station_path: &str, method: &str) -> Result<()> {
    let conn = cmd_conn().await?;
    conn.call_method(
        Some("net.connman.iwd"),
        station_path,
        Some("net.connman.iwd.Station"),
        method,
        &(),
    )
    .await
    .with_context(|| format!("call Station.{method}"))?;
    Ok(())
}
```

- [ ] **Step 3: Refactor `do_network_call` to use the shared connection**

Replace the existing `do_network_call` (around line 306):

```rust
async fn do_network_call(network_path: &str, method: &str) -> Result<()> {
    let conn = cmd_conn().await?;
    conn.call_method(
        Some("net.connman.iwd"),
        network_path,
        Some("net.connman.iwd.Network"),
        method,
        &(),
    )
    .await
    .with_context(|| format!("call Network.{method}"))?;
    Ok(())
}
```

(No public-API changes; the public commands `scan`, `connect_network`, `disconnect` already invoke these helpers.)

- [ ] **Step 4: Audit remaining `Connection::system()` calls in the file**

Run: `grep -n 'Connection::system' crates/hytte-services/src/wifi.rs`

Expected: exactly **two** remaining call sites — the listen loop's own `Connection::system()` (around line 759 in the `listen()` function, which subscribes to PropertiesChanged and ObjectManager signals) and any other long-lived stream consumer. The command-side `do_station_call` / `do_network_call` should now show NO `Connection::system()`.

If the grep shows more than 2 hits in command paths, locate them and migrate to `cmd_conn()`.

- [ ] **Step 5: Build + clippy + tests**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p hytte-services`
Expected: clean + 51+ tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/wifi.rs
git commit -m "$(cat <<'EOF'
refactor(wifi): share a single command connection across calls

Mirrors the v0.2.1 BlueZ CMD_CONN pattern: lift a shared
tokio::sync::OnceCell<Connection> and route do_station_call /
do_network_call through it. The listen loop keeps its own
connection; signal subscriptions and command identity are
independent.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Wifi `Adapter` listening + `set_powered`

**Files:**
- Modify: `crates/hytte-services/src/wifi.rs`

**Background:** iwd exposes `net.connman.iwd.Adapter1` on `/net/connman/iwd/<adapter_idx>` (e.g. `/net/connman/iwd/0`). Properties: `Powered: bool`, `Name: String`, `Vendor`, `Model`. We store `path`, `powered`, and `name` only. The wifi listen loop already enumerates the iwd object tree; extending it to capture the adapter object is straightforward — the adapter path is the prefix of the station path.

- [ ] **Step 1: Add the `Adapter` struct + `WifiHandles.adapter`**

In `crates/hytte-services/src/wifi.rs`:

a) After the `Station` struct (around line 64), add:

```rust
/// Snapshot of the iwd Adapter (`net.connman.iwd.Adapter1`).
#[derive(Clone, Debug, Default)]
pub struct Adapter {
    /// D-Bus object path, e.g. `"/net/connman/iwd/0"`.
    pub path: String,
    pub powered: bool,
    pub name: String,
}
```

b) In `WifiHandles` (around line 129), add:

```rust
pub(crate) adapter: Mutable<Option<Adapter>>,
```

(Other fields stay: `station`, `networks`, `prompts`.)

c) Update `WifiHandles::default()` (around line 137) to initialize `adapter: Mutable::new(None)`.

- [ ] **Step 2: Add the `adapter()` public signal**

Below the existing `station()` function (around line 190), add:

```rust
/// Signal emitting the current Adapter snapshot, or `None` when no
/// adapter is present.
pub fn adapter() -> impl Signal<Item = Option<Adapter>> {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .adapter
            .signal_cloned()
    })
}
```

- [ ] **Step 3: Add `set_powered` + adapter command helper**

Below `disconnect()` (around line 250), add:

```rust
/// Fire-and-forget: set `Powered` on the iwd Adapter1.
pub fn set_powered(on: bool) {
    runtime::handle().spawn(async move {
        let path = current_adapter_path().await;
        if path.is_empty() {
            tracing::warn!("wifi::set_powered: no adapter path known");
            return;
        }
        if let Err(e) = do_set_adapter_bool(&path, "Powered", on).await {
            tracing::warn!(error = %e, on, "wifi set_powered failed");
        }
    });
}
```

In the command-helpers section (around line 288, near `do_station_call`), add:

```rust
async fn do_set_adapter_bool(adapter_path: &str, prop: &str, on: bool) -> Result<()> {
    let conn = cmd_conn().await?;
    conn.call_method(
        Some("net.connman.iwd"),
        adapter_path,
        Some("org.freedesktop.DBus.Properties"),
        "Set",
        &(
            "net.connman.iwd.Adapter1",
            prop,
            zbus::zvariant::Value::from(on),
        ),
    )
    .await
    .with_context(|| format!("call Properties.Set Adapter1.{prop}"))?;
    Ok(())
}
```

- [ ] **Step 4: Add the adapter path cache**

The wifi service already has a `STATION_PATH` cache pattern using `OnceLock<Arc<RwLock<String>>>` (around line 101-113). Mirror it for adapters. Add below the station-path block:

```rust
static ADAPTER_PATH: OnceLock<Arc<tokio::sync::RwLock<String>>> = OnceLock::new();

fn adapter_path_store() -> &'static Arc<tokio::sync::RwLock<String>> {
    ADAPTER_PATH.get_or_init(|| Arc::new(tokio::sync::RwLock::new(String::new())))
}

async fn current_adapter_path() -> String {
    adapter_path_store().read().await.clone()
}

async fn set_current_adapter_path(path: &str) {
    *adapter_path_store().write().await = path.to_string();
}
```

- [ ] **Step 5: Capture the adapter from the listen loop's initial enumeration**

In `crates/hytte-services/src/wifi.rs`, locate the `listen()` function (around line 672) and the initial `GetManagedObjects` walk that finds the station (around line 683-712). The station path looks like `/net/connman/iwd/0/3/6`. The adapter path is the prefix `/net/connman/iwd/0` (the first numeric segment).

Add a helper near the other path utilities:

```rust
/// Given a station path like "/net/connman/iwd/0/3/6", return the
/// adapter path "/net/connman/iwd/0". Returns empty string if the
/// path doesn't match the expected shape.
fn adapter_path_from_station(station_path: &str) -> String {
    // Expected layout: /net/connman/iwd/<adapter_idx>/<phy>/<station_idx>
    let parts: Vec<&str> = station_path.split('/').collect();
    // parts = ["", "net", "connman", "iwd", "<adapter>", "<phy>", "<station>"]
    if parts.len() < 5 || parts[1] != "net" || parts[2] != "connman" || parts[3] != "iwd" {
        return String::new();
    }
    format!("/net/connman/iwd/{}", parts[4])
}
```

In `listen()`, after the station path is determined and stored, also derive and store the adapter path:

```rust
// Inside listen(), after `set_station_path(&station_path).await;` (locate this call;
// it follows the existing GetManagedObjects iteration), add:
let adapter_path = adapter_path_from_station(&station_path);
if !adapter_path.is_empty() {
    set_current_adapter_path(&adapter_path).await;

    // Read initial Adapter1 properties from the managed-objects map (we
    // already have it from GetManagedObjects above).
    if let Some(ifaces) = managed_objects.get(adapter_path.as_str())
        && let Some(props) = ifaces.get("net.connman.iwd.Adapter1")
    {
        let adapter_snapshot = Adapter {
            path: adapter_path.clone(),
            powered: prop_bool(props, "Powered"),
            name: prop_str(props, "Name"),
        };
        adapter_mutable.set(Some(adapter_snapshot));
    }
}
```

(If `managed_objects` is named differently in the real code — verify by reading the surrounding lines — match the local name. The intent: read Adapter1 props out of the same dictionary we already have.)

The function signature of `listen` needs `adapter_mutable: &Mutable<Option<Adapter>>`. Update both:
- The `listen` signature (around line 672) to accept `adapter_mutable`.
- The `start()` impl (around line 161) where `listen` is called inside the retry loop — pass the new `adapter_mutable` argument cloned from `WifiHandles.adapter`.

- [ ] **Step 6: Update PropertiesChanged handler for Adapter1 paths**

Locate the PropertiesChanged subscription's body in `listen()` (around line 820-850). It currently dispatches based on the changed-iface name (e.g. `net.connman.iwd.Station`). Add a branch for `net.connman.iwd.Adapter1`:

```rust
// Inside the PropertiesChanged dispatch arm, alongside the Station handling:
if iface == "net.connman.iwd.Adapter1" {
    let mut current = adapter_mutable.lock_mut();
    if let Some(adapter) = current.as_mut() {
        if changed_props.contains_key("Powered") {
            adapter.powered = prop_bool(&changed_props, "Powered");
        }
        if changed_props.contains_key("Name") {
            adapter.name = prop_str(&changed_props, "Name");
        }
        // No need to reassign — Mutable::lock_mut is a write guard.
    }
    // Force-emit on guard drop happens automatically; if the existing
    // code uses .set() instead, build a fresh Adapter and call
    // adapter_mutable.set(Some(updated)) instead.
    continue;
}
```

(Match the existing dispatch style. If the existing code uses `set` everywhere instead of `lock_mut`, mirror that — read the surrounding handler and follow its convention.)

- [ ] **Step 7: Build + clippy + tests**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p hytte-services`
Expected: clean + tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/hytte-services/src/wifi.rs
git commit -m "$(cat <<'EOF'
feat(wifi): expose iwd Adapter1 + set_powered

Adds Adapter { path, powered, name } and wifi::adapter() signal.
The listen loop now derives the adapter path from the station path
and reads Adapter1 properties out of the GetManagedObjects map.
PropertiesChanged on Adapter1 paths is dispatched alongside the
existing Station handler.

set_powered routes through the shared CMD_CONN added in the
previous commit.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `WifiNetwork.known_network_path` + `forget()`

**Files:**
- Modify: `crates/hytte-services/src/wifi.rs`

**Background:** The listen loop's `read_networks` already extracts the iwd `KnownNetwork` object path (around line 437) and uses it to derive the boolean `known`. The path is then discarded. Surfacing it on `WifiNetwork` lets the UI call iwd's `KnownNetwork.Forget()`.

- [ ] **Step 1: Add the field to `WifiNetwork`**

Update the existing struct (around line 67) to:

```rust
#[derive(Clone, Debug)]
pub struct WifiNetwork {
    pub path: String,
    pub ssid: String,
    pub security: String,
    pub known: bool,
    pub connected: bool,
    pub signal_dbm: i16,
    /// iwd KnownNetwork object path when stored credentials exist;
    /// `None` otherwise. Used by `forget()` to call
    /// `net.connman.iwd.KnownNetwork.Forget()`.
    pub known_network_path: Option<String>,
}
```

- [ ] **Step 2: Populate it in `read_networks`**

Locate the network construction in `read_networks` (around line 430-445). The current code parses `KnownNetwork` and computes `known`. Replace:

```rust
let known_network_path = props
    .get("KnownNetwork")
    .and_then(/* existing extraction */);
let known = !known_network_path.is_empty() && known_network_path != "/";
```

(if the existing shape is slightly different, follow the same logic to also keep the path string as a `String`.)

with code that yields BOTH the bool and the `Option<String>`:

```rust
let known_network_path_raw: String = props
    .get("KnownNetwork")
    .and_then(|v| v.try_clone().ok())
    .and_then(|v| zbus::zvariant::ObjectPath::try_from(v).ok())
    .map(|p| p.to_string())
    .unwrap_or_default();
let known_network_path: Option<String> = if known_network_path_raw.is_empty()
    || known_network_path_raw == "/"
{
    None
} else {
    Some(known_network_path_raw)
};
let known = known_network_path.is_some();
```

(The existing code may already extract the path as a String — verify and adapt the snippet to match. The intent: `Option<String>` instead of bool-only.)

Then in the `WifiNetwork { ... }` constructor below, add `known_network_path,` to the field list.

- [ ] **Step 3: Add `forget()` + the helper**

Below `disconnect()` / `set_powered()` (around line 270), add:

```rust
/// Fire-and-forget: call `Forget` on the given iwd KnownNetwork object.
/// iwd handles cascading disconnect when forgetting the active network.
pub fn forget(known_network_path: &str) {
    let path = known_network_path.to_string();
    runtime::handle().spawn(async move {
        if let Err(e) = do_known_network_call(&path, "Forget").await {
            tracing::warn!(error = %e, path, "wifi forget failed");
        }
    });
}
```

In the command-helpers section, add:

```rust
async fn do_known_network_call(known_network_path: &str, method: &str) -> Result<()> {
    let conn = cmd_conn().await?;
    conn.call_method(
        Some("net.connman.iwd"),
        known_network_path,
        Some("net.connman.iwd.KnownNetwork"),
        method,
        &(),
    )
    .await
    .with_context(|| format!("call KnownNetwork.{method}"))?;
    Ok(())
}
```

- [ ] **Step 4: Add the unit test**

Append to the existing `#[cfg(test)] mod tests` block in `wifi.rs` (or create one if missing):

```rust
#[test]
fn known_network_path_round_trips() {
    // Smoke-test the Option<String> derivation logic as a pure function.
    // The real extraction lives in read_networks; replicate the
    // canonicalization here so we lock in the "/ → None" rule.
    fn derive(raw: &str) -> Option<String> {
        if raw.is_empty() || raw == "/" {
            None
        } else {
            Some(raw.to_string())
        }
    }
    assert_eq!(derive(""), None);
    assert_eq!(derive("/"), None);
    assert_eq!(derive("/net/connman/iwd/0/3/6/known"), Some("/net/connman/iwd/0/3/6/known".to_string()));
}
```

(This is a deliberate documentation-test: it canonicalizes the slash-path-or-None convention. Real zbus round-trip isn't unit-testable without a mock.)

- [ ] **Step 5: Run tests**

Run: `cargo test -p hytte-services wifi::tests::known_network_path_round_trips -- --nocapture`
Expected: pass.

- [ ] **Step 6: Build + clippy**

Run: `cargo build -p hytte-services && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/hytte-services/src/wifi.rs
git commit -m "$(cat <<'EOF'
feat(wifi): surface known_network_path + forget()

Adds Option<String> known_network_path to WifiNetwork (was previously
parsed and discarded after computing the `known` bool). Adds
wifi::forget(path) calling net.connman.iwd.KnownNetwork.Forget()
through the shared CMD_CONN. iwd handles cascading disconnect when
forgetting the active network.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: UI — `page_network` skeleton + Connection group

**Files:**
- Modify: `trollshell/src/widgets/pages.rs`

**Background:** Restructure `page_network` from the 2-col grid layout into a vertical stack of three `AdwPreferencesGroup`s. This task lands the new skeleton AND the full Connection group. Traffic and Wi-Fi will be migrated in Tasks 6 & 7; until those land, `page_network` will call the existing legacy `build_traffic_group` and `append_wifi_section` so the page keeps rendering.

- [ ] **Step 1: Rewrite `page_network`**

Locate `page_network` (around line 307) and replace its body. The new shape:

```rust
pub fn page_network() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    column.append(&build_connection_group_v2().upcast_ref::<gtk::Widget>());

    let traffic_panel = panel("Traffic");
    build_traffic_group_legacy(&traffic_panel);
    column.append(&traffic_panel);

    let wifi_panel = panel("Wi-Fi");
    append_wifi_section(&wifi_panel);
    column.append(&wifi_panel);

    finish_page(&column)
}
```

The existing `build_connection_group` and `build_traffic_group` will be deprecated step by step. For Task 5 we add a NEW `build_connection_group_v2` and rename the existing `build_traffic_group` body into a temporary `build_traffic_group_legacy(&panel)` so it can still feed the legacy `panel("Traffic")`. (Tasks 6/7 will swap the legacy callers.)

a) Rename the existing `fn build_traffic_group()` to take a `panel: &gtk::Box` and append rows to it:

```rust
fn build_traffic_group_legacy(panel: &gtk::Box) {
    // Move the existing function body here, replacing
    // `let group = adw::PreferencesGroup::new();` with operating
    // directly on the `panel` Box for now. Each `group.add(&row)`
    // becomes `panel.append(&row)`.
}
```

(If the existing `build_traffic_group` already returns an `adw::PreferencesGroup`, simpler: keep it, and write `panel.append(&build_traffic_group().upcast_ref::<gtk::Widget>())` inside `page_network`. Choose whichever shape is closer to the existing code.)

- [ ] **Step 2: Add `build_connection_group_v2`**

Below the existing helpers, add the new builder. Returns `adw::PreferencesGroup`:

```rust
fn build_connection_group_v2() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Connection").build();

    // Live description on the group itself.
    bind(
        networkd::primary().map(|p| match p {
            Some(link) => match link.operational {
                OperationalState::Routable => format!("Online via {}", link.name),
                OperationalState::Carrier | OperationalState::DegradedCarrier =>
                    format!("Limited connectivity via {}", link.name),
                other => format!("{} via {}", describe_state(other), link.name),
            },
            None => "Offline".to_string(),
        }),
        &group,
        |g, text| g.set_description(Some(&text)),
    );

    // Three expanders in vertical order.
    group.add(&build_primary_expander());
    group.add(&build_all_links_expander());
    group.add(&build_dns_expander());

    group
}
```

- [ ] **Step 3: Add `build_primary_expander`**

```rust
fn build_primary_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder()
        .title("Primary")
        .build();

    bind(
        networkd::primary().map(|p| match p {
            Some(link) => link.name,
            None => "No connection".to_string(),
        }),
        &expander,
        |w, name| w.set_title(&name),
    );
    bind(
        networkd::primary().map(|p| match p {
            Some(link) => describe_state(link.operational).to_string(),
            None => String::new(),
        }),
        &expander,
        |w, sub| w.set_subtitle(&sub),
    );

    let v4_addr_row = adw::ActionRow::builder().title("IPv4 address").build();
    let v4_value = gtk::Label::new(None);
    v4_value.add_css_class("ts-mono");
    v4_addr_row.add_suffix(&v4_value);
    bind(
        networkd::primary().map(|p| match p {
            Some(link) => link
                .addresses
                .iter()
                .filter_map(|ip| match ip {
                    std::net::IpAddr::V4(v) => Some(v.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(", "),
            None => String::new(),
        }),
        &v4_addr_row,
        move |row, txt| {
            v4_value.set_text(&txt);
            row.set_visible(!txt.is_empty());
        },
    );
    expander.add_row(&v4_addr_row);

    let v4_gw_row = adw::ActionRow::builder().title("IPv4 gateway").build();
    let v4_gw_value = gtk::Label::new(None);
    v4_gw_value.add_css_class("ts-mono");
    v4_gw_row.add_suffix(&v4_gw_value);
    bind(
        networkd::primary().map(|p| {
            p.and_then(|l| l.gateway_v4.map(|g| g.to_string())).unwrap_or_default()
        }),
        &v4_gw_row,
        move |row, txt| {
            v4_gw_value.set_text(&txt);
            row.set_visible(!txt.is_empty());
        },
    );
    expander.add_row(&v4_gw_row);

    let v6_addr_row = adw::ActionRow::builder().title("IPv6 address").build();
    let v6_value = gtk::Label::new(None);
    v6_value.add_css_class("ts-mono");
    v6_addr_row.add_suffix(&v6_value);
    bind(
        networkd::primary().map(|p| match p {
            Some(link) => {
                let v6: Vec<String> = link
                    .addresses
                    .iter()
                    .filter_map(|ip| match ip {
                        std::net::IpAddr::V6(v) if !v.is_unicast_link_local() => Some(v.to_string()),
                        _ => None,
                    })
                    .collect();
                if v6.is_empty() {
                    String::new()
                } else if v6.len() == 1 {
                    v6[0].clone()
                } else {
                    format!("{} (+{} more)", v6[0], v6.len() - 1)
                }
            }
            None => String::new(),
        }),
        &v6_addr_row,
        move |row, txt| {
            v6_value.set_text(&txt);
            row.set_visible(!txt.is_empty());
        },
    );
    expander.add_row(&v6_addr_row);

    let v6_gw_row = adw::ActionRow::builder().title("IPv6 gateway").build();
    let v6_gw_value = gtk::Label::new(None);
    v6_gw_value.add_css_class("ts-mono");
    v6_gw_row.add_suffix(&v6_gw_value);
    bind(
        networkd::primary().map(|p| {
            p.and_then(|l| l.gateway_v6.map(|g| g.to_string())).unwrap_or_default()
        }),
        &v6_gw_row,
        move |row, txt| {
            v6_gw_value.set_text(&txt);
            row.set_visible(!txt.is_empty());
        },
    );
    expander.add_row(&v6_gw_row);

    expander
}
```

`Ipv6Addr::is_unicast_link_local` is a stable method since 1.84; the workspace toolchain (1.94) supports it.

- [ ] **Step 4: Add `build_all_links_expander`**

```rust
fn build_all_links_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("All links").build();
    bind(
        networkd::links().map(|ls| {
            let count = ls.iter().filter(|l| l.name != "lo").count();
            format!("{count} interface(s)")
        }),
        &expander,
        |w, sub| w.set_subtitle(&sub),
    );

    // Track child rows so we can drain & rebuild on each emission.
    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(networkd::links(), &expander, move |_, links| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        let mut new_rows = Vec::new();
        for link in links.iter().filter(|l| l.name != "lo") {
            let row = adw::ActionRow::builder().title(&link.name).build();
            let pill = build_link_state_pill(link.operational);
            row.add_suffix(&pill);
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
}

/// Build the pill label for a link's operational state.
fn build_link_state_pill(state: OperationalState) -> gtk::Label {
    let label = gtk::Label::new(Some(state_pill_text(state)));
    label.add_css_class("ts-net-pill");
    label.add_css_class(state_pill_class(state));
    label
}

fn state_pill_text(state: OperationalState) -> &'static str {
    match state {
        OperationalState::Routable => "Online",
        OperationalState::Carrier | OperationalState::DegradedCarrier => "Carrier",
        OperationalState::Degraded => "Degraded",
        OperationalState::EnslavedRouting => "Enslaved",
        OperationalState::NoCarrier => "No carrier",
        OperationalState::Dormant => "Dormant",
        OperationalState::Off => "Off",
        OperationalState::Missing => "Missing",
    }
}

fn state_pill_class(state: OperationalState) -> &'static str {
    match state {
        OperationalState::Routable => "ts-pill-connected",
        _ => "ts-pill-known",
    }
}
```

(The pill class taxonomy is intentionally narrow: Routable → connected/accent, everything else → dim/known. CSS rules for both classes land in Task 9.)

- [ ] **Step 5: Add `build_dns_expander`**

```rust
fn build_dns_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("DNS").build();
    bind(
        resolved::dns().map(|state| {
            if state.configured() {
                format!("{} server(s)", state.servers.len())
            } else {
                "Not configured".to_string()
            }
        }),
        &expander,
        |w, sub| w.set_subtitle(&sub),
    );

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(resolved::dns(), &expander, move |_, state| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        let mut new_rows = Vec::new();
        for ip in &state.servers {
            let row = adw::ActionRow::builder()
                .title(&ip.to_string())
                .activatable(false)
                .build();
            row.set_title_lines(1);
            row.add_css_class("ts-mono");
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
}
```

- [ ] **Step 6: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. The Connection group renders via the new code; Traffic + Wi-Fi still render via the legacy panels.

- [ ] **Step 7: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(de): network — Adwaita-conformant Connection group

Drops the 2-col grid layout for page_network in favor of a vertical
stack of AdwPreferencesGroups. Adds build_connection_group_v2 with
three AdwExpanderRows (Primary / All links / DNS) revealing IP,
gateway, link, and DNS-server detail rows. Traffic + Wi-Fi keep
their legacy shape until subsequent commits swap them.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: UI — Traffic group as `AdwPreferencesGroup`

**Files:**
- Modify: `trollshell/src/widgets/pages.rs`

**Background:** Wrap the existing two Traffic rows (`Live`, `Total`) in a native `AdwPreferencesGroup` titled "Traffic", replacing the legacy `panel("Traffic")` wrapper. Row contents are unchanged.

- [ ] **Step 1: Add `build_traffic_group_v2`**

Add to `pages.rs` near `build_connection_group_v2`:

```rust
fn build_traffic_group_v2() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Traffic").build();

    let rate_row = adw::ActionRow::builder().title("Live").build();
    rate_row.set_subtitle_lines(0);
    bind(
        sensors::network().map(|net| {
            let parts: Vec<String> = net
                .interfaces
                .iter()
                .filter(|i| i.name != "lo")
                .map(|i| {
                    format!(
                        "{}: \u{2193} {} \u{2191} {}",
                        i.name,
                        fmt_rate(i.rx_rate_bps),
                        fmt_rate(i.tx_rate_bps),
                    )
                })
                .collect();
            if parts.is_empty() {
                "(no active interfaces)".to_string()
            } else {
                parts.join("\n")
            }
        }),
        &rate_row,
        |row, text| row.set_subtitle(&text),
    );
    group.add(&rate_row);

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

    group
}
```

(The body is functionally identical to the legacy `build_traffic_group_legacy` — same binds, same labels.)

- [ ] **Step 2: Update `page_network` to use the new builder**

In `page_network` (Task 5 left it calling the legacy panel), replace the Traffic block:

Replace:

```rust
let traffic_panel = panel("Traffic");
build_traffic_group_legacy(&traffic_panel);
column.append(&traffic_panel);
```

with:

```rust
column.append(build_traffic_group_v2().upcast_ref::<gtk::Widget>());
```

- [ ] **Step 3: Delete `build_traffic_group_legacy`**

It's now unused. Remove the function body and any related imports that go unused as a consequence (clippy will flag).

- [ ] **Step 4: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(de): network — Traffic group via AdwPreferencesGroup

Wraps the existing Live + Total rows in a native AdwPreferencesGroup
with title "Traffic", replacing the legacy panel() wrapper. Row
contents and bind shapes unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: UI — Wi-Fi group structure (header suffix + description)

**Files:**
- Modify: `trollshell/src/widgets/pages.rs`

**Background:** Replace the legacy `panel("Wi-Fi") + append_wifi_section(...)` with a native `AdwPreferencesGroup`. The group's title is `"Wi-Fi"`; description binds to a live SSID + state + dBm string. Header suffix carries a `gtk::Switch` (adapter Powered, via `bind_two_way`), a `gtk::Button("Scan")`, and a `gtk::Spinner` (visible-bind on `station.scanning`).

The network list itself stays in this task: a `ScrolledWindow` wrapping a `PreferencesGroup` of network rows. The row builder is rewritten in Task 8.

- [ ] **Step 1: Add `build_wifi_group_v2`**

```rust
fn build_wifi_group_v2() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Wi-Fi").build();

    // Live description.
    let combined = futures_signals::map_ref! {
        let adapter = wifi::adapter(),
        let station = wifi::station(),
        let networks = wifi::networks() => {
            (adapter.clone(), station.clone(), networks.clone())
        }
    };
    bind(combined, &group, |g, (adapter, station, networks)| {
        let text = wifi_description_text(&adapter, &station, &networks);
        g.set_description(Some(&text));
    });

    // Header suffix: power switch + scan button + spinner.
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.set_valign(gtk::Align::Center);

    let power_switch = gtk::Switch::new();
    power_switch.set_valign(gtk::Align::Center);
    bind(
        wifi::adapter().map(|a| a.is_some()),
        &power_switch,
        gtk::prelude::WidgetExt::set_sensitive,
    );
    bind_two_way(
        wifi::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
        &power_switch,
        gtk::Switch::set_active,
        |w| w.connect_active_notify(|sw| wifi::set_powered(sw.is_active())),
    );
    header.append(&power_switch);

    let scan_btn = gtk::Button::with_label("Scan");
    scan_btn.connect_clicked(|_| wifi::scan());
    let scan_sensitive_signal = futures_signals::map_ref! {
        let adapter = wifi::adapter(),
        let station = wifi::station() => {
            let powered = adapter.as_ref().is_some_and(|a| a.powered);
            let scanning = station.as_ref().is_some_and(|s| s.scanning);
            powered && !scanning
        }
    };
    bind(scan_sensitive_signal, &scan_btn, gtk::prelude::WidgetExt::set_sensitive);
    header.append(&scan_btn);

    let spinner = gtk::Spinner::new();
    spinner.set_valign(gtk::Align::Center);
    bind(
        wifi::station().map(|s| s.is_some_and(|st| st.scanning)),
        &spinner,
        |w, scanning| {
            w.set_spinning(scanning);
            w.set_visible(scanning);
        },
    );
    header.append(&spinner);

    group.set_header_suffix(Some(&header));

    // Network list.
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_min_content_height(160);
    scrolled.set_max_content_height(240);
    scrolled.add_css_class("ts-wifi-list");
    let networks_group = adw::PreferencesGroup::new();
    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let placeholder_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));
    scrolled.set_child(Some(&networks_group));
    group.add(&scrolled);

    // Power-off greying for the scrolled list (Switch stays sensitive).
    bind(
        wifi::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
        &scrolled,
        gtk::prelude::WidgetExt::set_sensitive,
    );

    let group_for_bind = networks_group.clone();
    let rows_for_bind = rows_track.clone();
    let placeholder_for_bind = placeholder_track.clone();
    bind(wifi::networks(), &networks_group, move |_, nets| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            group_for_bind.remove(&row);
        }
        if let Some(p) = placeholder_for_bind.borrow_mut().take() {
            group_for_bind.remove(&p);
        }
        if nets.is_empty() {
            let placeholder = adw::ActionRow::builder()
                .title("No networks found")
                .subtitle("Tap Scan to refresh")
                .activatable(false)
                .build();
            group_for_bind.add(&placeholder);
            *placeholder_for_bind.borrow_mut() = Some(placeholder);
        } else {
            let mut new_rows = Vec::with_capacity(nets.len());
            for net in &nets {
                let row = build_network_row_v2(net);
                group_for_bind.add(&row);
                new_rows.push(row);
            }
            *rows_for_bind.borrow_mut() = new_rows;
        }
    });

    group
}

fn wifi_description_text(
    adapter: &Option<wifi::Adapter>,
    station: &Option<wifi::Station>,
    networks: &[wifi::WifiNetwork],
) -> String {
    let Some(a) = adapter else {
        return "No adapter".to_string();
    };
    if !a.powered {
        return "Disabled".to_string();
    }
    let Some(st) = station else {
        return "Disconnected".to_string();
    };
    match st.state {
        wifi::StationState::Connecting => "Connecting\u{2026}".to_string(),
        wifi::StationState::Roaming => "Roaming".to_string(),
        wifi::StationState::Connected => {
            if let Some(ssid) = &st.connected_ssid {
                if let Some(n) = networks.iter().find(|n| n.connected) {
                    format!("{ssid} \u{00b7} {} dBm ({})", n.signal_dbm, dbm_label(n.signal_dbm))
                } else {
                    ssid.clone()
                }
            } else {
                "Connected".to_string()
            }
        }
        _ => "Disconnected".to_string(),
    }
}

fn dbm_label(dbm: i16) -> &'static str {
    if dbm >= -50 {
        "excellent"
    } else if dbm >= -60 {
        "good"
    } else if dbm >= -75 {
        "ok"
    } else {
        "weak"
    }
}
```

`build_network_row_v2` is added in Task 8 — for now, alias it to the existing `build_network_row` so this task compiles:

```rust
fn build_network_row_v2(net: &wifi::WifiNetwork) -> adw::ActionRow {
    build_network_row(net)
}
```

(Task 8 will replace the body with the new pill+popover form.)

- [ ] **Step 2: Update `page_network` to use `build_wifi_group_v2`**

Replace the Wi-Fi block in `page_network`:

```rust
let wifi_panel = panel("Wi-Fi");
append_wifi_section(&wifi_panel);
column.append(&wifi_panel);
```

with:

```rust
column.append(build_wifi_group_v2().upcast_ref::<gtk::Widget>());
```

- [ ] **Step 3: Delete `append_wifi_section` and the legacy Connection helper**

`append_wifi_section` (around line 460) is now unused. Remove it.

The original `build_connection_group` (around line 325) is also now unused (Task 5 added the v2). Remove it too. Same for any helpers that were only used by these legacy builders.

Run: `cargo build -p trollshell` to surface any orphaned symbols. Clippy will flag dead code.

- [ ] **Step 4: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(de): network — Wi-Fi group via AdwPreferencesGroup

Replaces the legacy panel("Wi-Fi") + append_wifi_section block with
a native AdwPreferencesGroup. Header suffix carries the adapter
power Switch (bind_two_way), Scan button, and an inline spinner
on station.scanning. The network list lives in a bounded
ScrolledWindow with an empty-state placeholder row.

The network row body is unchanged in this commit (the v2 builder
delegates to the existing build_network_row); the rewrite to a
pill + ⋮ popover form lands next.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: UI — Network row rewrite (pill suffix + ⋮ popover)

**Files:**
- Modify: `trollshell/src/widgets/pages.rs`

**Background:** Replace the row's subtitle-as-status with a status pill suffix; subtitle becomes `"{dbm} dBm · {sec}"`. Add a ⋮ MenuButton suffix with a popover containing state-driven Connect / Disconnect / Forget. Row activation calls connect (only when not connected). Per project memory, Disconnect / Forget live in the popover, never on the primary click target.

- [ ] **Step 1: Replace `build_network_row_v2`**

Replace the temporary alias added in Task 7 with the full new body:

```rust
fn build_network_row_v2(net: &wifi::WifiNetwork) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&net.ssid)
        .subtitle(&format!("{} dBm \u{00b7} {}", net.signal_dbm, security_label(&net.security)))
        .activatable(true)
        .build();

    let icon = gtk::Image::from_icon_name(signal_icon(net.signal_dbm));
    row.add_prefix(&icon);

    // Pill suffix (only for connected / known states).
    if net.connected {
        row.add_suffix(&pill_label("Connected", "ts-pill-connected"));
    } else if net.known {
        row.add_suffix(&pill_label("Known", "ts-pill-known"));
    }

    // ⋮ popover.
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

    let net_path = net.path.clone();
    let known_path_opt = net.known_network_path.clone();

    if net.connected {
        let pop_for_disc = popover.clone();
        let disconnect_btn = gtk::Button::with_label("Disconnect");
        disconnect_btn.add_css_class("flat");
        disconnect_btn.add_css_class("destructive-action");
        disconnect_btn.connect_clicked(move |_| {
            wifi::disconnect();
            pop_for_disc.popdown();
        });
        popover_box.append(&disconnect_btn);

        if let Some(known_path) = known_path_opt.clone() {
            let pop_for_forget = popover.clone();
            let forget_btn = gtk::Button::with_label("Forget");
            forget_btn.add_css_class("flat");
            forget_btn.add_css_class("destructive-action");
            forget_btn.connect_clicked(move |_| {
                wifi::forget(&known_path);
                pop_for_forget.popdown();
            });
            popover_box.append(&forget_btn);
        }
    } else {
        let pop_for_conn = popover.clone();
        let connect_path = net_path.clone();
        let connect_btn = gtk::Button::with_label("Connect");
        connect_btn.add_css_class("flat");
        connect_btn.connect_clicked(move |_| {
            wifi::connect_network(&connect_path);
            pop_for_conn.popdown();
        });
        popover_box.append(&connect_btn);

        if let Some(known_path) = known_path_opt {
            let pop_for_forget = popover.clone();
            let forget_btn = gtk::Button::with_label("Forget");
            forget_btn.add_css_class("flat");
            forget_btn.add_css_class("destructive-action");
            forget_btn.connect_clicked(move |_| {
                wifi::forget(&known_path);
                pop_for_forget.popdown();
            });
            popover_box.append(&forget_btn);
        }
    }

    popover.set_child(Some(&popover_box));
    menu_btn.set_popover(Some(&popover));
    row.add_suffix(&menu_btn);

    // Row activation: connect only when not currently connected.
    let connected = net.connected;
    let act_path = net.path.clone();
    row.connect_activated(move |_| {
        if !connected {
            wifi::connect_network(&act_path);
        }
    });

    row
}

fn pill_label(text: &str, variant_class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_valign(gtk::Align::Center);
    label.add_css_class("ts-net-pill");
    label.add_css_class(variant_class);
    label
}

fn security_label(security: &str) -> &'static str {
    match security {
        "open" => "Open",
        "psk" => "WPA2",
        "8021x" => "802.1x",
        "wep" => "WEP",
        _ => "Secured",
    }
}
```

- [ ] **Step 2: Delete the legacy `build_network_row` and its helpers**

The original `build_network_row` (around line 520) is now superseded. Remove it.

If `signal_icon` is still referenced from `build_network_row_v2`, keep it. If any other helpers (e.g. an old `connection_status_text`) are unused, clippy will flag them — remove on its prompts.

- [ ] **Step 3: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add trollshell/src/widgets/pages.rs
git commit -m "$(cat <<'EOF'
feat(de): network — pill suffix + ⋮ popover on Wi-Fi rows

Status moves from the row's subtitle to a pill suffix
(Connected / Known); subtitle becomes "{dbm} dBm · {sec}". A
⋮ MenuButton popover holds Connect / Disconnect / Forget,
state-driven per row. Per project memory, destructive actions
(Disconnect, Forget) live in the popover, never on the row's
primary click target. Row activation connects only when not
currently connected.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: CSS — pill classes + monospace helper

**Files:**
- Modify: `trollshell/style.css`

**Background:** Three pill classes plus a monospace helper. All use existing `@accent_color` token; per project memory, no new color tokens are introduced.

- [ ] **Step 1: Append the rules to `trollshell/style.css`**

At the bottom of `trollshell/style.css`:

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

- [ ] **Step 2: Build + clippy**

Run: `cargo build -p trollshell && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. (CSS isn't compiled; this just confirms nothing else broke.)

- [ ] **Step 3: Manual smoke test (deferred)**

In a Niri session: `cargo run --release -p trollshell`. Open the Network drawer.

- Three groups stack vertically: Connection, Traffic, Wi-Fi.
- Connection's description shows live primary state.
- Primary expander reveals IPv4 address + gateway rows; expand and verify they populate (e.g. `192.168.1.42/24`, `192.168.1.1`). IPv6 rows hidden if no v6.
- All-links expander reveals one row per non-`lo` link, each with a status pill.
- DNS expander reveals one monospace row per server.
- Traffic group shows `Live` and `Total` rows.
- Wi-Fi group title bar shows: title `"Wi-Fi"`, description with current state, header-suffix Switch + Scan + (spinning when scanning).
- Toggle the Wi-Fi power Switch off → list dims, description says "Disabled". Toggle back on.
- Tap Scan → spinner appears, list refreshes.
- Each network row shows signal icon prefix, SSID, `"-XX dBm · WPA2"` (or Open/802.1x/WEP) subtitle, optional pill (Connected / Known), and a ⋮ MenuButton.
- Open ⋮ on the connected network → Disconnect + Forget actions.
- Open ⋮ on a known network → Connect + Forget.
- Open ⋮ on an unknown secured network → Connect only.
- Tap an unknown secured row → existing password prompt overlay appears.

(Verify with `iwctl device list` that the adapter Powered toggle works and `iwctl known-networks list` that Forget removes the SSID.)

- [ ] **Step 4: Commit**

```bash
git add trollshell/style.css
git commit -m "$(cat <<'EOF'
style: network panel — pill classes + monospace helper

Adds .ts-net-pill, .ts-pill-connected, .ts-pill-known,
and .ts-mono. All use the existing @accent_color token; no
new color tokens introduced.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review notes

- **Spec coverage:**
  - Spec §1 UI structure → Tasks 5–8 + 9 (CSS).
  - Spec §2 service extensions:
    - 2a networkd Link → Task 1.
    - 2b wifi Adapter → Task 3.
    - 2c wifi forget → Task 4.
    - 2d shared CMD_CONN → Task 2.
    - 2e Cargo.toml → Task 1 step 1.
  - Spec §3 tests → Tasks 1 (3 networkd tests), 4 (1 wifi test).
  - Spec §4 stylesheet → Task 9.

- **Final verification:**
  - `cargo clippy --workspace --all-targets -- -D warnings` clean.
  - `cargo test --workspace` green.
  - Manual smoke test (deferred) covers every spec success criterion.
