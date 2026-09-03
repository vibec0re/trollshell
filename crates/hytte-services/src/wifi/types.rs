//! Public data types exposed by the Wi-Fi service.

// ── Public data shapes ────────────────────────────────────────────────────────

/// Current state of the Wi-Fi station as reported by iwd.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StationState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
    Roaming,
}

/// Snapshot of the iwd station.
#[derive(Clone, Debug, Default)]
pub struct Station {
    /// D-Bus object path, e.g. `"/net/connman/iwd/0/3/6"`.
    pub path: String,
    pub state: StationState,
    pub scanning: bool,
    /// Object path of the currently-connected network, if any.
    pub connected_network: Option<String>,
    /// Convenience: SSID of the currently-connected network.
    pub connected_ssid: Option<String>,
}

/// Snapshot of the iwd Adapter (`net.connman.iwd.Adapter1`).
#[derive(Clone, Debug, Default)]
pub struct Adapter {
    /// D-Bus object path, e.g. `"/net/connman/iwd/0"`.
    pub path: String,
    pub powered: bool,
    pub name: String,
}

/// Snapshot of one visible Wi-Fi network.
#[derive(Clone, Debug)]
pub struct WifiNetwork {
    /// D-Bus object path.
    pub path: String,
    pub ssid: String,
    /// `"open"` | `"psk"` | `"8021x"` | `"wep"`
    pub security: String,
    /// `true` when iwd has stored credentials for this network.
    pub known: bool,
    /// `true` when this is the currently-connected network.
    pub connected: bool,
    /// Signal strength in dBm (iwd reports dBm × 100; we divide before storing).
    pub signal_dbm: i16,
    /// iwd `KnownNetwork` object path when stored credentials exist;
    /// `None` otherwise. Used by `forget()` to call
    /// `net.connman.iwd.KnownNetwork.Forget()`.
    pub known_network_path: Option<String>,
    /// How many access points this row collapsed (#871). `1` for a plain
    /// single-radio network; higher on a mesh, a repeater, or a dual-band
    /// router advertising one SSID on both 2.4 and 5 GHz.
    ///
    /// Only the `NetworkManager` backend can ever exceed `1`: NM publishes one
    /// `org.freedesktop.NetworkManager.AccessPoint` object per **BSSID**, so
    /// `wifi_nm` merges them per SSID and records the group size here. iwd
    /// enumerates `net.connman.iwd.Network` objects, which are per-SSID by
    /// construction, so its rows are always `1`.
    pub ap_count: usize,
}

// ── Prompt request ────────────────────────────────────────────────────────────

/// What kind of secret the overlay is asking the user for. Lets the overlay
/// title/label the dialog correctly without changing the existing Wi-Fi fields.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PromptKind {
    /// A Wi-Fi passphrase (iwd `RequestPassphrase` or NM `GetSecrets` for
    /// `802-11-wireless-security`). The overlay reads [`PromptRequest::ssid`] /
    /// [`PromptRequest::security`].
    #[default]
    WifiPassphrase,
    /// A VPN secret (NM `GetSecrets` for the `vpn` setting). The overlay reads
    /// [`PromptRequest::ssid`] as the VPN connection name.
    VpnSecret,
}

/// A pending passphrase / secret prompt request.
#[derive(Clone, Debug)]
pub struct PromptRequest {
    /// Unique per request; echo back into `submit_prompt` or `cancel_prompt`.
    pub id: u64,
    /// iwd / NM connection object path.
    pub network_path: String,
    /// SSID from Network.Name (best-effort, falls back to last path segment) for
    /// a Wi-Fi prompt, or the VPN connection name for a [`PromptKind::VpnSecret`].
    pub ssid: String,
    /// Network security type ("psk", "8021x", etc.). Empty for a VPN prompt.
    pub security: String,
    /// Which kind of secret is being requested — drives the overlay's wording.
    pub kind: PromptKind,
    /// The secret keys this prompt must collect, in the order the daemon named
    /// them. **Always non-empty**, and the overlay must return exactly one
    /// value per entry (in this order) to `submit_prompt`.
    ///
    /// Length 1 is the overwhelmingly common case — a Wi-Fi passphrase, or a
    /// VPN with a single password — and renders as one unlabelled entry, the
    /// pre-existing look. A `GetSecrets` round that needs several secrets at
    /// once (e.g. a VPN wanting a password *and* a one-time code) lists them
    /// all here, so the overlay can collect the whole set in one dialog and
    /// the agent can answer `NetworkManager`'s request in a single reply. NM
    /// re-asks (or fails the activation) for any key it requested and did not
    /// get back, so a partial answer is not a degraded success — it is a
    /// failed round.
    ///
    /// The keys are daemon vocabulary (`"psk"`, `"wep-key0"`, `"password"`,
    /// whatever a VPN plugin hinted); turning them into human labels is the
    /// overlay's job. The iwd backend has no key vocabulary at all — its
    /// `RequestPassphrase` returns a bare string — so it uses a single
    /// placeholder key that is never rendered.
    pub secret_keys: Vec<String>,
    /// `true` when this prompt is a reopen after a previously-submitted secret
    /// was rejected. Only the NM backend can populate this reliably — it maps
    /// `NM_SECRET_AGENT_GET_SECRETS_FLAG_REQUEST_NEW` on `GetSecrets` (a
    /// stateless, per-call, authoritative "the last secret was rejected" bit).
    /// The iwd backend has no equivalent signal on `RequestPassphrase`, so it
    /// always sets this `false`. Deliberately a bool, not an attempt count —
    /// NM cannot produce a count, only this one bit.
    pub prior_failure: bool,
}
