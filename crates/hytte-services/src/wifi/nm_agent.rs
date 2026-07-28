//! `NetworkManager` `org.freedesktop.NetworkManager.SecretAgent` D-Bus
//! interface implementation.
//!
//! `NetworkManager` asks a registered secret agent for the credentials it can't
//! find in its own keyring (a freshly-typed Wi-Fi passphrase, for example). The
//! agent registers an object with NM's `AgentManager`; NM then records our
//! connection's *unique* name and issues `GetSecrets` callbacks on it — no
//! well-known bus name is involved, unlike the iwd agent. So this agent needs
//! no `<allow own=...>` system-bus policy entry; NM's own policy already lets a
//! console user register a secret agent.
//!
//! It reuses the exact same waiter/prompt plumbing as [`super::agent::IwdAgent`]
//! (`NEXT_ID` → oneshot into `waiters` → surface a [`PromptRequest`] → await),
//! so `submit_prompt`/`cancel_prompt` and the prompt overlay work unchanged.

use futures_channel::oneshot;
use futures_signals::signal::Mutable;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::types::{PromptKind, PromptRequest};
use super::{NEXT_ID, WaitersMap};

// ── Agent error ────────────────────────────────────────────────────────────────

/// Errors returned to `NetworkManager` from the secret agent.
///
/// The `#[zbus(prefix)]` maps each variant to a fully-qualified D-Bus error
/// name under `org.freedesktop.NetworkManager.SecretAgent.Error`, matching the
/// `NMSecretAgentError` quark NM expects:
///   * `NoSecrets` → fall through to another agent / fail cleanly (we hold none).
///   * `UserCanceled` → the user dismissed the passphrase prompt.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.NetworkManager.SecretAgent.Error")]
enum NmAgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NoSecrets(String),
    UserCanceled(String),
}

// ── Constants ──────────────────────────────────────────────────────────────────

/// The 802-11 wireless-security setting NM asks us to fill for a PSK network.
const WIRELESS_SECURITY_SETTING: &str = "802-11-wireless-security";

/// The `vpn` setting NM asks us to fill for a VPN connection's credentials.
const VPN_SETTING: &str = "vpn";

/// Default VPN secret key when NM gives no usable hint. The overwhelmingly
/// common single-secret VPN case (`OpenVPN` / PPTP / L2TP user auth, many
/// plugins) keys the credential under `"password"`.
const VPN_DEFAULT_SECRET_KEY: &str = "password";

/// Default wireless-security secret key when NM's `hints` are absent, empty,
/// or contain nothing we recognise. WPA/WPA2/WPA3-Personal (`psk`) is the
/// overwhelmingly common case and today's pre-existing behaviour — an
/// unrecognised hint must fall back here, never be passed through blindly.
const WIRELESS_DEFAULT_SECRET_KEY: &str = "psk";

/// Wireless-security secret keys we recognise out of NM's `hints`, each
/// naming the credential for a different `key-mgmt`:
///   * `"psk"` — WPA/WPA2/WPA3-Personal (`wpa-psk`, `sae`).
///   * `"wep-key0"` — static WEP (`key-mgmt = "none"`).
///   * `"leap-password"` — dynamic WEP / Cisco LEAP (`key-mgmt = "ieee8021x"`).
const RECOGNISED_WIRELESS_SECRET_KEYS: &[&str] = &["psk", "wep-key0", "leap-password"];

/// `NMSecretAgentGetSecretsFlags` bit 0 — `ALLOW_INTERACTION`. When unset, NM
/// only wants secrets it can return without prompting the user, so we must not
/// pop a passphrase dialog.
const FLAG_ALLOW_INTERACTION: u32 = 0x1;

/// `NMSecretAgentGetSecretsFlags` bit 1 — `REQUEST_NEW`. NM sets this when it
/// is re-asking because the secret we (or another agent) last supplied was
/// rejected — a stateless, per-call, authoritative "the last secret was
/// wrong" bit. Mapped into [`PromptRequest::prior_failure`] so the overlay can
/// render error feedback on the reopened prompt.
const FLAG_REQUEST_NEW: u32 = 0x2;

/// A connection settings dict: `a{sa{sv}}` — setting name → (key → value).
type ConnectionDict = HashMap<String, HashMap<String, OwnedValue>>;

// ── Agent ─────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(super) struct NmAgent {
    pub(super) prompts: Mutable<Option<PromptRequest>>,
    pub(super) waiters: WaitersMap,
}

// ── Connection-dict helpers ────────────────────────────────────────────────────

/// Read a `String`-typed value out of one of the connection dict's settings.
fn setting_str(connection: &ConnectionDict, setting: &str, key: &str) -> Option<String> {
    connection
        .get(setting)
        .and_then(|s| s.get(key))
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| String::try_from(v).ok())
}

/// Derive the SSID from a connection dict's `802-11-wireless.ssid` byte array,
/// falling back to `connection.id` (the profile name) and finally the empty
/// string. NM stores the SSID as `ay` (a byte array).
fn ssid_from_connection(connection: &ConnectionDict) -> String {
    let from_ssid = connection
        .get("802-11-wireless")
        .and_then(|w| w.get("ssid"))
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| <Vec<u8>>::try_from(v).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        .filter(|s| !s.is_empty());

    from_ssid
        // Fall back to the human-readable connection id.
        .or_else(|| setting_str(connection, "connection", "id"))
        .unwrap_or_default()
}

/// Derive a security string from the connection's
/// `802-11-wireless-security.key-mgmt`, normalised to the same vocabulary the
/// iwd backend uses (`"psk"`, `"8021x"`, `"wep"`, `"open"`). Defaults to
/// `"psk"` when NM is asking for wireless-security secrets but no key-mgmt is
/// present (the overwhelmingly common case is WPA-PSK).
fn security_from_connection(connection: &ConnectionDict) -> String {
    let key_mgmt =
        setting_str(connection, WIRELESS_SECURITY_SETTING, "key-mgmt").unwrap_or_default();
    match key_mgmt.as_str() {
        "wpa-eap" | "wpa-eap-suite-b-192" => "8021x".to_string(),
        "none" | "ieee8021x" => "wep".to_string(),
        // "wpa-psk", "sae", "wpa-none", or unknown → treat as PSK.
        _ => "psk".to_string(),
    }
}

/// Decide which key to nest the wireless-security secret under.
///
/// NM names the secret it actually wants in `hints` — mirrors
/// [`vpn_secret_key_to_prompt`]'s precedent of trusting NM's hint list, but
/// simpler: wireless has no equivalent of `vpn.secrets` to check off, and NM
/// doesn't qualify these hints as `"<setting>.<key>"` the way it sometimes
/// does for VPN, so hints here are always bare candidate secret names.
///
/// **Conservative default:** hints absent, empty, or containing nothing in
/// [`RECOGNISED_WIRELESS_SECRET_KEYS`] all fall back to
/// [`WIRELESS_DEFAULT_SECRET_KEY`] (`"psk"`) — correct for the overwhelmingly
/// common WPA/WPA2/WPA3-Personal case and today's pre-existing behaviour, so
/// an unrecognised hint value is never passed through blindly.
fn wireless_secret_key_from_hints(hints: &[String]) -> String {
    hints
        .iter()
        .map(String::as_str)
        .find(|h| RECOGNISED_WIRELESS_SECRET_KEYS.contains(h))
        .map_or_else(
            || WIRELESS_DEFAULT_SECRET_KEY.to_string(),
            ToString::to_string,
        )
}

/// Build the nested reply dict `{ setting_name: { <secret_key>: <passphrase> } }`.
fn build_secret_reply(setting_name: &str, secret_key: &str, passphrase: &str) -> ConnectionDict {
    let mut setting: HashMap<String, OwnedValue> = HashMap::new();
    if let Ok(v) = Value::from(passphrase).try_to_owned() {
        setting.insert(secret_key.to_string(), v);
    }
    let mut out: ConnectionDict = HashMap::new();
    out.insert(setting_name.to_string(), setting);
    out
}

// ── VPN secrets ────────────────────────────────────────────────────────────────

/// The VPN connection name shown in the prompt: `connection.id`, falling back to
/// the empty string. (VPN connections have no SSID; the id is the display name.)
fn vpn_name_from_connection(connection: &ConnectionDict) -> String {
    setting_str(connection, "connection", "id").unwrap_or_default()
}

/// Read the `vpn.secrets` sub-dict (`a{ss}` — already-known secret values) out
/// of the connection. NM nests VPN secrets one level deeper than other settings:
/// `connection["vpn"]["secrets"]` is itself a string→string map. Returns the
/// keys already present (so we only prompt for the genuinely-missing ones).
fn existing_vpn_secret_keys(connection: &ConnectionDict) -> std::collections::HashSet<String> {
    connection
        .get(VPN_SETTING)
        .and_then(|vpn| vpn.get("secrets"))
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| <HashMap<String, String>>::try_from(v).ok())
        .map(|m| m.into_keys().collect())
        .unwrap_or_default()
}

/// Decide which VPN secret key to prompt for.
///
/// Preference: the first `hints` entry NM passed (NM names the wanted secret in
/// the hints, e.g. `["password"]` or `["Gateway Password"]`) that isn't already
/// present in the connection's stored `vpn.secrets`; otherwise
/// [`VPN_DEFAULT_SECRET_KEY`] (`"password"`) — the common single-secret case.
///
/// **Limitation:** when NM asks for *several* missing VPN secrets at once we
/// only prompt for one and return that single key. The common case (a single
/// user password) is fully covered; multi-secret VPNs (e.g. password + OTP in
/// one round) would need a multi-field dialog, which is out of scope here.
fn vpn_secret_key_to_prompt(connection: &ConnectionDict, hints: &[String]) -> String {
    let existing = existing_vpn_secret_keys(connection);
    hints
        .iter()
        .map(|h| {
            // NM sometimes qualifies a hint as "<setting>.<key>"; take the key.
            h.rsplit_once('.').map_or(h.as_str(), |(_, k)| k)
        })
        .find(|key| !existing.contains(*key))
        .map_or_else(|| VPN_DEFAULT_SECRET_KEY.to_string(), ToString::to_string)
}

/// Build the VPN reply dict `{ "vpn": { "secrets": { <key>: <value> } } }`.
///
/// This is the exact shape NM expects back from `GetSecrets` for the `vpn`
/// setting: the `vpn` setting carries a single `"secrets"` key whose value is an
/// `a{ss}` (string→string) sub-dict of the credential(s). It is **distinct**
/// from the Wi-Fi PSK shape (`{ "802-11-wireless-security": { "psk": … } }`),
/// where the secret sits directly under the setting.
fn build_vpn_secret_reply(secret_key: &str, secret_value: &str) -> ConnectionDict {
    // Inner a{ss}: the secret key → value map.
    let mut secrets: HashMap<String, String> = HashMap::new();
    secrets.insert(secret_key.to_string(), secret_value.to_string());

    let mut vpn_setting: HashMap<String, OwnedValue> = HashMap::new();
    if let Ok(v) = Value::from(secrets).try_to_owned() {
        vpn_setting.insert("secrets".to_string(), v);
    }

    let mut out: ConnectionDict = HashMap::new();
    out.insert(VPN_SETTING.to_string(), vpn_setting);
    out
}

/// How to shape the `GetSecrets` reply once the user supplies the secret. The
/// two kinds differ in the nested dict NM expects back (see
/// [`build_secret_reply`] vs [`build_vpn_secret_reply`]).
enum ReplyShape {
    /// Wi-Fi: `{ <setting_name>: { <secret_key>: <secret> } }`.
    WirelessSecurity {
        setting_name: String,
        secret_key: String,
    },
    /// VPN: `{ "vpn": { "secrets": { <secret_key>: <secret> } } }`.
    Vpn { secret_key: String },
}

impl ReplyShape {
    fn build(&self, secret: &str) -> ConnectionDict {
        match self {
            ReplyShape::WirelessSecurity {
                setting_name,
                secret_key,
            } => build_secret_reply(setting_name, secret_key, secret),
            ReplyShape::Vpn { secret_key } => build_vpn_secret_reply(secret_key, secret),
        }
    }
}

// ── Interface ───────────────────────────────────────────────────────────────

// zbus's `#[interface]` macro requires every method to be `async fn`; the
// Save/Delete stubs and the no-op branches do not await. `unused_variables`
// silences the deliberately-ignored connection dicts in the stubs.
#[allow(clippy::unused_async, unused_variables)]
#[zbus::interface(name = "org.freedesktop.NetworkManager.SecretAgent")]
impl NmAgent {
    /// Called by NM when it needs secrets it doesn't already hold.
    ///
    /// `connection` is the full `a{sa{sv}}` settings dict, `setting_name` the
    /// setting whose secrets are wanted (e.g. `"802-11-wireless-security"` or
    /// `"vpn"`), `hints` the specific keys (e.g. `["psk"]` / `["password"]`),
    /// and `flags` the `NMSecretAgentGetSecretsFlags`. We handle the
    /// wireless-security PSK case and the VPN-secret case interactively;
    /// everything else returns the NM "no secrets" error so NM can fall through
    /// to another agent or fail the activation cleanly.
    async fn get_secrets(
        &self,
        connection: ConnectionDict,
        connection_path: OwnedObjectPath,
        setting_name: String,
        hints: Vec<String>,
        flags: u32,
    ) -> Result<ConnectionDict, NmAgentError> {
        // Classify the request: wireless-security PSK, VPN secret, or neither.
        let wants_wireless =
            setting_name == WIRELESS_SECURITY_SETTING || hints.iter().any(|h| h == "psk");
        let wants_vpn = setting_name == VPN_SETTING;
        if !wants_wireless && !wants_vpn {
            return Err(NmAgentError::NoSecrets(
                "only 802-11-wireless-security and vpn secrets supported".into(),
            ));
        }

        // If NM didn't grant interaction, we can't pop a dialog — and we hold
        // no stored secrets, so there is nothing to return.
        if flags & FLAG_ALLOW_INTERACTION == 0 {
            return Err(NmAgentError::NoSecrets(
                "interaction not allowed; no stored secret".into(),
            ));
        }

        let conn_path = connection_path.as_str().to_string();
        let prior_failure = flags & FLAG_REQUEST_NEW != 0;

        // Build the prompt request and remember how to shape the reply, branching
        // on the setting kind. Both kinds share the waiter/oneshot plumbing.
        let (prompt, reply_key): (PromptRequest, ReplyShape) = if wants_vpn {
            let name = vpn_name_from_connection(&connection);
            let secret_key = vpn_secret_key_to_prompt(&connection, &hints);
            tracing::info!(
                name = %name,
                secret_key = %secret_key,
                path = %conn_path,
                prior_failure,
                "NM SecretAgent::GetSecrets — requesting VPN secret",
            );
            (
                PromptRequest {
                    id: 0, // filled in below
                    network_path: conn_path.clone(),
                    ssid: name,
                    security: String::new(),
                    kind: PromptKind::VpnSecret,
                    prior_failure,
                },
                ReplyShape::Vpn { secret_key },
            )
        } else {
            let ssid = ssid_from_connection(&connection);
            let security = security_from_connection(&connection);
            let secret_key = wireless_secret_key_from_hints(&hints);
            tracing::info!(
                ssid = %ssid,
                security = %security,
                secret_key = %secret_key,
                path = %conn_path,
                prior_failure,
                "NM SecretAgent::GetSecrets — requesting passphrase",
            );
            (
                PromptRequest {
                    id: 0, // filled in below
                    network_path: conn_path,
                    ssid,
                    security,
                    kind: PromptKind::WifiPassphrase,
                    prior_failure,
                },
                ReplyShape::WirelessSecurity {
                    setting_name,
                    secret_key,
                },
            )
        };

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<Result<String, String>>();
        {
            let mut waiters = self.waiters.lock().await;
            waiters.insert(id, tx);
        }
        self.prompts.set(Some(PromptRequest { id, ..prompt }));

        if let Ok(Ok(secret)) = rx.await {
            self.prompts.set(None);
            Ok(reply_key.build(&secret))
        } else {
            self.prompts.set(None);
            Err(NmAgentError::UserCanceled("user dismissed prompt".into()))
        }
    }

    /// Called by NM to abort an in-flight `GetSecrets` (e.g. the activation was
    /// cancelled). Resolve every outstanding waiter with a cancel and clear the
    /// prompt so the overlay dismisses.
    async fn cancel_get_secrets(&self, connection_path: OwnedObjectPath, setting_name: String) {
        tracing::info!(
            path = %connection_path.as_str(),
            setting = %setting_name,
            "NM SecretAgent::CancelGetSecrets",
        );
        let mut waiters = self.waiters.lock().await;
        for (_, tx) in waiters.drain() {
            let _ = tx.send(Err("cancelled".into()));
        }
        self.prompts.set(None);
    }

    /// No-op: we never persist secrets — NM stores its own connection profiles.
    async fn save_secrets(
        &self,
        connection: ConnectionDict,
        connection_path: OwnedObjectPath,
    ) -> Result<(), NmAgentError> {
        Ok(())
    }

    /// No-op: we hold no secrets of our own to delete.
    async fn delete_secrets(
        &self,
        connection: ConnectionDict,
        connection_path: OwnedObjectPath,
    ) -> Result<(), NmAgentError> {
        Ok(())
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn val(v: impl Into<Value<'static>>) -> OwnedValue {
        v.into().try_to_owned().expect("to OwnedValue")
    }

    fn setting(pairs: &[(&str, OwnedValue)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.try_clone().expect("clone")))
            .collect()
    }

    // -- ssid_from_connection -------------------------------------------------

    #[test]
    fn ssid_from_wireless_ssid_bytes() {
        let mut conn: ConnectionDict = HashMap::new();
        conn.insert(
            "802-11-wireless".to_string(),
            setting(&[("ssid", val(b"FRITZ!Box".to_vec()))]),
        );
        assert_eq!(ssid_from_connection(&conn), "FRITZ!Box");
    }

    #[test]
    fn ssid_falls_back_to_connection_id() {
        let mut conn: ConnectionDict = HashMap::new();
        // Wireless ssid present but empty → fall through to connection.id.
        conn.insert(
            "802-11-wireless".to_string(),
            setting(&[("ssid", val(Vec::<u8>::new()))]),
        );
        conn.insert(
            "connection".to_string(),
            setting(&[("id", val("My Home Net"))]),
        );
        assert_eq!(ssid_from_connection(&conn), "My Home Net");
    }

    #[test]
    fn ssid_empty_when_nothing_present() {
        let conn: ConnectionDict = HashMap::new();
        assert_eq!(ssid_from_connection(&conn), "");
    }

    // -- security_from_connection ---------------------------------------------

    #[test]
    fn security_wpa_psk_is_psk() {
        let mut conn: ConnectionDict = HashMap::new();
        conn.insert(
            WIRELESS_SECURITY_SETTING.to_string(),
            setting(&[("key-mgmt", val("wpa-psk"))]),
        );
        assert_eq!(security_from_connection(&conn), "psk");
    }

    #[test]
    fn security_sae_is_psk() {
        let mut conn: ConnectionDict = HashMap::new();
        conn.insert(
            WIRELESS_SECURITY_SETTING.to_string(),
            setting(&[("key-mgmt", val("sae"))]),
        );
        assert_eq!(security_from_connection(&conn), "psk");
    }

    #[test]
    fn security_wpa_eap_is_8021x() {
        let mut conn: ConnectionDict = HashMap::new();
        conn.insert(
            WIRELESS_SECURITY_SETTING.to_string(),
            setting(&[("key-mgmt", val("wpa-eap"))]),
        );
        assert_eq!(security_from_connection(&conn), "8021x");
    }

    #[test]
    fn security_none_is_wep() {
        let mut conn: ConnectionDict = HashMap::new();
        conn.insert(
            WIRELESS_SECURITY_SETTING.to_string(),
            setting(&[("key-mgmt", val("none"))]),
        );
        assert_eq!(security_from_connection(&conn), "wep");
    }

    #[test]
    fn security_missing_key_mgmt_defaults_to_psk() {
        let conn: ConnectionDict = HashMap::new();
        assert_eq!(security_from_connection(&conn), "psk");
    }

    // -- build_secret_reply ---------------------------------------------------

    #[test]
    fn reply_nests_psk_under_setting_name() {
        let reply = build_secret_reply(WIRELESS_SECURITY_SETTING, "psk", "hunter2");
        let inner = reply
            .get(WIRELESS_SECURITY_SETTING)
            .expect("setting present");
        let psk = inner.get("psk").expect("psk present");
        assert_eq!(
            String::try_from(psk.try_clone().unwrap()).unwrap(),
            "hunter2"
        );
    }

    #[test]
    fn reply_nests_under_the_requested_secret_key() {
        let reply = build_secret_reply(WIRELESS_SECURITY_SETTING, "wep-key0", "abcde");
        let inner = reply
            .get(WIRELESS_SECURITY_SETTING)
            .expect("setting present");
        assert!(!inner.contains_key("psk"), "must not use the psk key");
        let wep_key0 = inner.get("wep-key0").expect("wep-key0 present");
        assert_eq!(
            String::try_from(wep_key0.try_clone().unwrap()).unwrap(),
            "abcde"
        );
    }

    // -- wireless_secret_key_from_hints ----------------------------------------

    #[test]
    fn wireless_secret_key_defaults_to_psk_when_hints_absent() {
        assert_eq!(wireless_secret_key_from_hints(&[]), "psk");
    }

    #[test]
    fn wireless_secret_key_defaults_to_psk_when_hints_empty() {
        let hints = vec![String::new()];
        assert_eq!(wireless_secret_key_from_hints(&hints), "psk");
    }

    #[test]
    fn wireless_secret_key_recognises_psk_hint() {
        let hints = vec!["psk".to_string()];
        assert_eq!(wireless_secret_key_from_hints(&hints), "psk");
    }

    #[test]
    fn wireless_secret_key_recognises_wep_key0_hint() {
        let hints = vec!["wep-key0".to_string()];
        assert_eq!(wireless_secret_key_from_hints(&hints), "wep-key0");
    }

    #[test]
    fn wireless_secret_key_recognises_leap_password_hint() {
        let hints = vec!["leap-password".to_string()];
        assert_eq!(wireless_secret_key_from_hints(&hints), "leap-password");
    }

    #[test]
    fn wireless_secret_key_falls_back_to_psk_on_unrecognised_hint() {
        let hints = vec!["some-unrecognised-hint".to_string()];
        assert_eq!(wireless_secret_key_from_hints(&hints), "psk");
    }

    // -- VPN secrets ----------------------------------------------------------

    /// Decode the `vpn.secrets` `a{ss}` sub-dict out of a reply dict and return
    /// the value stored under `key` (panics if the nested shape is wrong — which
    /// is exactly what we're asserting against).
    fn vpn_reply_secret(reply: &ConnectionDict, key: &str) -> String {
        let vpn = reply.get("vpn").expect("vpn setting present in reply");
        let secrets_val = vpn
            .get("secrets")
            .expect("secrets sub-dict present")
            .try_clone()
            .expect("clone secrets value");
        let secrets =
            <HashMap<String, String>>::try_from(secrets_val).expect("secrets decodes as a{ss}");
        secrets.get(key).cloned().expect("secret key present")
    }

    #[test]
    fn vpn_reply_nests_secret_under_vpn_secrets() {
        let reply = build_vpn_secret_reply("password", "s3cr3t");
        // The top-level setting must be exactly "vpn" — not the bare key.
        assert!(reply.contains_key("vpn"), "vpn setting present");
        assert!(
            !reply.contains_key("password"),
            "secret must be nested, not top-level",
        );
        assert_eq!(vpn_reply_secret(&reply, "password"), "s3cr3t");
    }

    #[test]
    fn vpn_reply_uses_requested_secret_key() {
        // A non-default key (e.g. a per-gateway password) is preserved verbatim.
        let reply = build_vpn_secret_reply("Gateway Password", "abc");
        assert_eq!(vpn_reply_secret(&reply, "Gateway Password"), "abc");
    }

    /// Build a minimal VPN connection dict: a `connection.id` plus an optional
    /// `vpn.secrets` `a{ss}` of already-stored secret keys.
    fn vpn_connection(id: &str, existing_secrets: &[(&str, &str)]) -> ConnectionDict {
        let mut conn: ConnectionDict = HashMap::new();
        conn.insert(
            "connection".to_string(),
            setting(&[("id", val(id.to_string()))]),
        );
        let mut vpn: HashMap<String, OwnedValue> = HashMap::new();
        if !existing_secrets.is_empty() {
            let secrets: HashMap<String, String> = existing_secrets
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            vpn.insert("secrets".to_string(), val(secrets));
        }
        conn.insert("vpn".to_string(), vpn);
        conn
    }

    #[test]
    fn vpn_name_from_connection_id() {
        let conn = vpn_connection("Work VPN", &[]);
        assert_eq!(vpn_name_from_connection(&conn), "Work VPN");
    }

    #[test]
    fn vpn_name_empty_when_no_id() {
        let conn: ConnectionDict = HashMap::new();
        assert_eq!(vpn_name_from_connection(&conn), "");
    }

    #[test]
    fn vpn_secret_key_defaults_to_password_without_hints() {
        let conn = vpn_connection("Work VPN", &[]);
        assert_eq!(vpn_secret_key_to_prompt(&conn, &[]), "password");
    }

    #[test]
    fn vpn_secret_key_uses_first_hint() {
        let conn = vpn_connection("Work VPN", &[]);
        let hints = vec!["Gateway Password".to_string()];
        assert_eq!(vpn_secret_key_to_prompt(&conn, &hints), "Gateway Password",);
    }

    #[test]
    fn vpn_secret_key_strips_setting_prefix_from_hint() {
        // NM sometimes qualifies the hint as "<setting>.<key>".
        let conn = vpn_connection("Work VPN", &[]);
        let hints = vec!["vpn.password".to_string()];
        assert_eq!(vpn_secret_key_to_prompt(&conn, &hints), "password");
    }

    #[test]
    fn vpn_secret_key_skips_already_stored_secret() {
        // The first hint is already present → prompt for the next missing one.
        let conn = vpn_connection("Work VPN", &[("password", "stored")]);
        let hints = vec!["password".to_string(), "otp".to_string()];
        assert_eq!(vpn_secret_key_to_prompt(&conn, &hints), "otp");
    }

    #[test]
    fn existing_vpn_secret_keys_reads_stored_secrets() {
        let conn = vpn_connection("Work VPN", &[("password", "stored"), ("otp", "123")]);
        let keys = existing_vpn_secret_keys(&conn);
        assert!(keys.contains("password"));
        assert!(keys.contains("otp"));
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn existing_vpn_secret_keys_empty_when_none_stored() {
        let conn = vpn_connection("Work VPN", &[]);
        assert!(existing_vpn_secret_keys(&conn).is_empty());
    }
}

// ── System tests: dbus-daemon-backed GetSecrets round-trip ───────────────────
//
// The unit tests above cover the pure-logic helpers. These tests cover the part
// that *can't* be exercised in-process: the real `GetSecrets` D-Bus round trip.
// We spawn an ephemeral `dbus-daemon` (one fresh broker per test, mirroring
// `hytte-bus/tests/common/mod.rs`), mount the real `NmAgent` `#[zbus::interface]`
// on it, then call it from a second connection acting as `NetworkManager`. This
// exercises the actual `a{sa{sv}}` marshalling, method dispatch, error-name
// mapping (`org.freedesktop.NetworkManager.SecretAgent.Error.*`), and the
// prompt/waiter handshake over the wire — the closest-to-end-to-end the agent
// can be tested without a live `NetworkManager` + a real Wi-Fi radio + the GTK
// prompt overlay. That true hardware path stays live-verified on Annika's NM
// host.
//
// Gated behind the `system-tests` cargo feature (whole-module `cfg`) so the
// default `cargo test` doesn't even compile it, keeping the default run
// hermetic (no `dbus-daemon` dependency). Run with:
//   cargo test -p hytte-services --features system-tests --lib nm_agent
//
// Every await that could hang on a wrong wire signature is wrapped in
// `tokio::time::timeout`, so a marshalling regression fails fast instead of
// hanging the suite. `#[tokio::test(flavor = "multi_thread")]` is mandatory:
// the bus guard's Drop calls `block_in_place`, which panics on a current-thread
// runtime.
#[cfg(all(test, feature = "system-tests"))]
mod system_tests {
    use super::*;

    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::{Child, Command};
    use tokio::sync::Mutex as AsyncMutex;
    use zbus::Connection;
    use zbus::connection::Builder;

    /// Path the agent is mounted at on the ephemeral bus. NM uses a fixed
    /// well-known path in production; any valid object path works for the test.
    const AGENT_PATH: &str = "/org/freedesktop/NetworkManager/SecretAgent";
    const AGENT_IFACE: &str = "org.freedesktop.NetworkManager.SecretAgent";

    /// Kills the spawned `dbus-daemon` on drop. Mirrors `hytte-bus`'s `BusGuard`:
    /// SIGKILL + a `block_in_place` wait so the socket `TempDir` outlives the
    /// process (hence the multi-thread runtime requirement).
    struct BusGuard {
        child: Option<Child>,
        _tmp: TempDir,
        address: String,
    }

    impl Drop for BusGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.start_kill();
                tokio::task::block_in_place(|| {
                    let handle = tokio::runtime::Handle::current();
                    let _ = handle.block_on(child.wait());
                });
            }
        }
    }

    /// Spawn a fresh `dbus-daemon`, return a connected `zbus::Connection` plus a
    /// guard that kills the daemon on drop. Replicates
    /// `hytte-bus/tests/common/mod.rs::ephemeral_bus` (we can't import that
    /// crate's test-only module across crate boundaries).
    async fn ephemeral_bus() -> (Connection, BusGuard) {
        let tmp = TempDir::new().expect("create tempdir for dbus-daemon");
        let socket: PathBuf = tmp.path().join("bus");
        let address = format!("unix:path={}", socket.display());

        let config = tmp.path().join("session.conf");
        std::fs::write(
            &config,
            format!(
                r#"<?xml version="1.0"?>
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>{address}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
            ),
        )
        .expect("write dbus-daemon config");

        let mut child = Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&config)
            .arg("--print-address=1")
            .arg("--nofork")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn dbus-daemon — install package `dbus` if missing");

        // Read the printed address from stdout to confirm the daemon is up.
        let stdout = child.stdout.take().expect("dbus-daemon stdout");
        let mut lines = BufReader::new(stdout).lines();
        let printed = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
            .await
            .expect("dbus-daemon address timeout")
            .expect("dbus-daemon read address")
            .expect("dbus-daemon closed stdout");
        assert!(
            printed.contains("unix:path="),
            "unexpected dbus-daemon address: {printed}"
        );

        let conn = Builder::address(address.as_str())
            .expect("parse bus address")
            .build()
            .await
            .expect("connect to ephemeral bus");

        (
            conn,
            BusGuard {
                child: Some(child),
                _tmp: tmp,
                address,
            },
        )
    }

    /// Build a fresh `NmAgent` with empty handles; return it plus clones of its
    /// `prompts`/`waiters` so the test can drive the handshake after the agent
    /// is moved into the object server.
    fn fresh_agent() -> (NmAgent, Mutable<Option<PromptRequest>>, WaitersMap) {
        let waiters: WaitersMap = Arc::new(AsyncMutex::new(HashMap::new()));
        let prompts: Mutable<Option<PromptRequest>> = Mutable::new(None);
        let agent = NmAgent {
            prompts: prompts.clone(),
            waiters: waiters.clone(),
        };
        (agent, prompts, waiters)
    }

    /// Mount `agent` on `server`, open a second connection to the same bus
    /// acting as "`NetworkManager`", and return a proxy targeting the agent by the
    /// server's unique name. Uses `Builder::address` (NOT the clippy-banned
    /// `Connection::session`/`::system`).
    async fn mount_and_proxy(
        server: &Connection,
        guard: &BusGuard,
        agent: NmAgent,
    ) -> zbus::Proxy<'static> {
        server
            .object_server()
            .at(AGENT_PATH, agent)
            .await
            .expect("mount NmAgent on object server");

        let dest = server
            .unique_name()
            .expect("server has a unique name")
            .to_string();

        let client = Builder::address(guard.address.as_str())
            .expect("parse ephemeral bus address")
            .build()
            .await
            .expect("connect client (NetworkManager) to ephemeral bus");

        zbus::Proxy::new(&client, dest, AGENT_PATH, AGENT_IFACE)
            .await
            .expect("build SecretAgent proxy")
    }

    /// A realistic `a{sa{sv}}` connection dict for a WPA-PSK network, with the
    /// SSID stored as `ay` bytes the way NM marshals it.
    fn wpa_connection(ssid: &[u8], key_mgmt: &str) -> ConnectionDict {
        let to_owned = |v: Value<'static>| v.try_to_owned().expect("to OwnedValue");
        let mut wireless: HashMap<String, OwnedValue> = HashMap::new();
        wireless.insert("ssid".to_string(), to_owned(Value::from(ssid.to_vec())));
        let mut security: HashMap<String, OwnedValue> = HashMap::new();
        security.insert(
            "key-mgmt".to_string(),
            to_owned(Value::from(key_mgmt.to_string())),
        );
        let mut conn: ConnectionDict = HashMap::new();
        conn.insert("802-11-wireless".to_string(), wireless);
        conn.insert(WIRELESS_SECURITY_SETTING.to_string(), security);
        conn
    }

    /// A realistic `a{sa{sv}}` VPN connection dict: a `connection.id` and a
    /// `vpn` setting (with `service-type`), the way NM marshals one. No stored
    /// `vpn.secrets`, so the agent must prompt.
    fn vpn_connection_dict(id: &str, service_type: &str) -> ConnectionDict {
        let to_owned = |v: Value<'static>| v.try_to_owned().expect("to OwnedValue");
        let mut connection: HashMap<String, OwnedValue> = HashMap::new();
        connection.insert("id".to_string(), to_owned(Value::from(id.to_string())));
        connection.insert("type".to_string(), to_owned(Value::from("vpn".to_string())));
        let mut vpn: HashMap<String, OwnedValue> = HashMap::new();
        vpn.insert(
            "service-type".to_string(),
            to_owned(Value::from(service_type.to_string())),
        );
        let mut conn: ConnectionDict = HashMap::new();
        conn.insert("connection".to_string(), connection);
        conn.insert("vpn".to_string(), vpn);
        conn
    }

    /// Poll `prompts` until it holds `Some`, up to `deadline`. Returns the
    /// surfaced [`PromptRequest`] or panics on timeout.
    async fn await_prompt(
        prompts: &Mutable<Option<PromptRequest>>,
        deadline: Duration,
    ) -> PromptRequest {
        let end = tokio::time::Instant::now() + deadline;
        while tokio::time::Instant::now() < end {
            if let Some(req) = prompts.get_cloned() {
                return req;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("prompt was never surfaced within {deadline:?}");
    }

    // -- 1. happy path --------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn happy_path_returns_psk_over_the_bus() {
        let (server, guard) = ephemeral_bus().await;
        let (agent, prompts, waiters) = fresh_agent();
        let proxy = mount_and_proxy(&server, &guard, agent).await;

        // Fire the GetSecrets call as a task — it blocks inside the agent
        // awaiting the oneshot, so we must drive the handshake concurrently.
        let conn = wpa_connection(b"FRITZ!Box", "wpa-psk");
        let call: tokio::task::JoinHandle<Result<ConnectionDict, zbus::Error>> =
            tokio::spawn(async move {
                proxy
                    .call(
                        "GetSecrets",
                        &(
                            conn,
                            OwnedObjectPath::try_from(
                                "/org/freedesktop/NetworkManager/Connection/1",
                            )
                            .unwrap(),
                            "802-11-wireless-security".to_string(),
                            vec!["psk".to_string()],
                            FLAG_ALLOW_INTERACTION,
                        ),
                    )
                    .await
            });

        // The agent should surface a prompt carrying the SSID + security.
        let req = await_prompt(&prompts, Duration::from_secs(3)).await;
        assert_eq!(req.ssid, "FRITZ!Box");
        assert_eq!(req.security, "psk");

        // Resolve the waiter as the prompt overlay would on submit.
        waiters
            .lock()
            .await
            .remove(&req.id)
            .expect("waiter registered")
            .send(Ok("hunter2".to_string()))
            .expect("send passphrase to waiter");

        let reply = tokio::time::timeout(Duration::from_secs(3), call)
            .await
            .expect("GetSecrets did not return in time")
            .expect("GetSecrets task panicked")
            .expect("GetSecrets returned a D-Bus error");

        let psk = reply
            .get("802-11-wireless-security")
            .expect("security setting present in reply")
            .get("psk")
            .expect("psk present in reply");
        assert_eq!(
            String::try_from(psk.try_clone().unwrap()).unwrap(),
            "hunter2"
        );

        // The agent clears the prompt once resolved.
        assert!(
            prompts.get_cloned().is_none(),
            "prompt should be cleared after the passphrase is returned"
        );
    }

    // -- 1b. VPN happy path: nested vpn.secrets reply over the bus -------------

    #[tokio::test(flavor = "multi_thread")]
    async fn vpn_happy_path_returns_nested_secret_over_the_bus() {
        let (server, guard) = ephemeral_bus().await;
        let (agent, prompts, waiters) = fresh_agent();
        let proxy = mount_and_proxy(&server, &guard, agent).await;

        let conn = vpn_connection_dict("Work VPN", "org.freedesktop.NetworkManager.openvpn");
        let call: tokio::task::JoinHandle<Result<ConnectionDict, zbus::Error>> =
            tokio::spawn(async move {
                proxy
                    .call(
                        "GetSecrets",
                        &(
                            conn,
                            OwnedObjectPath::try_from(
                                "/org/freedesktop/NetworkManager/Connection/2",
                            )
                            .unwrap(),
                            "vpn".to_string(),
                            vec!["password".to_string()],
                            FLAG_ALLOW_INTERACTION,
                        ),
                    )
                    .await
            });

        // The prompt should carry the VPN name and be flagged as a VPN secret.
        let req = await_prompt(&prompts, Duration::from_secs(3)).await;
        assert_eq!(req.ssid, "Work VPN");
        assert_eq!(req.kind, PromptKind::VpnSecret);

        waiters
            .lock()
            .await
            .remove(&req.id)
            .expect("waiter registered")
            .send(Ok("vpnpass".to_string()))
            .expect("send VPN secret to waiter");

        let reply = tokio::time::timeout(Duration::from_secs(3), call)
            .await
            .expect("GetSecrets did not return in time")
            .expect("GetSecrets task panicked")
            .expect("GetSecrets returned a D-Bus error");

        // The reply must be `{ "vpn": { "secrets": { "password": "vpnpass" } } }`
        // — assert the full nesting survives the round trip on the wire.
        let vpn = reply.get("vpn").expect("vpn setting present in reply");
        let secrets_val = vpn
            .get("secrets")
            .expect("secrets sub-dict present")
            .try_clone()
            .expect("clone secrets");
        let secrets = <HashMap<String, String>>::try_from(secrets_val)
            .expect("secrets decodes as a{ss} over the wire");
        assert_eq!(secrets.get("password").map(String::as_str), Some("vpnpass"));

        assert!(
            prompts.get_cloned().is_none(),
            "prompt should be cleared after the VPN secret is returned"
        );
    }

    // -- 1c. WEP: a `wep-key0` hint is honoured over the bus -------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn wep_hint_returns_secret_under_wep_key0_over_the_bus() {
        let (server, guard) = ephemeral_bus().await;
        let (agent, prompts, waiters) = fresh_agent();
        let proxy = mount_and_proxy(&server, &guard, agent).await;

        // Static WEP: key-mgmt "none", and NM names the wanted secret
        // "wep-key0" in hints instead of "psk".
        let conn = wpa_connection(b"OldRouter", "none");
        let call: tokio::task::JoinHandle<Result<ConnectionDict, zbus::Error>> =
            tokio::spawn(async move {
                proxy
                    .call(
                        "GetSecrets",
                        &(
                            conn,
                            OwnedObjectPath::try_from(
                                "/org/freedesktop/NetworkManager/Connection/3",
                            )
                            .unwrap(),
                            "802-11-wireless-security".to_string(),
                            vec!["wep-key0".to_string()],
                            FLAG_ALLOW_INTERACTION,
                        ),
                    )
                    .await
            });

        let req = await_prompt(&prompts, Duration::from_secs(3)).await;
        assert_eq!(req.ssid, "OldRouter");
        assert_eq!(req.security, "wep");

        waiters
            .lock()
            .await
            .remove(&req.id)
            .expect("waiter registered")
            .send(Ok("abcde".to_string()))
            .expect("send WEP key to waiter");

        let reply = tokio::time::timeout(Duration::from_secs(3), call)
            .await
            .expect("GetSecrets did not return in time")
            .expect("GetSecrets task panicked")
            .expect("GetSecrets returned a D-Bus error");

        let setting = reply
            .get("802-11-wireless-security")
            .expect("security setting present in reply");
        assert!(
            !setting.contains_key("psk"),
            "a wep-key0 hint must not be answered under psk"
        );
        let wep_key0 = setting.get("wep-key0").expect("wep-key0 present in reply");
        assert_eq!(
            String::try_from(wep_key0.try_clone().unwrap()).unwrap(),
            "abcde"
        );

        assert!(
            prompts.get_cloned().is_none(),
            "prompt should be cleared after the WEP key is returned"
        );
    }

    // -- 2. no-interaction flag → NoSecrets -----------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn no_interaction_flag_is_rejected() {
        let (server, guard) = ephemeral_bus().await;
        let (agent, _prompts, _waiters) = fresh_agent();
        let proxy = mount_and_proxy(&server, &guard, agent).await;

        let conn = wpa_connection(b"FRITZ!Box", "wpa-psk");
        let result: Result<ConnectionDict, zbus::Error> = tokio::time::timeout(
            Duration::from_secs(3),
            proxy.call(
                "GetSecrets",
                &(
                    conn,
                    OwnedObjectPath::try_from("/org/freedesktop/NetworkManager/Connection/1")
                        .unwrap(),
                    "802-11-wireless-security".to_string(),
                    Vec::<String>::new(),
                    0u32, // no ALLOW_INTERACTION
                ),
            ),
        )
        .await
        .expect("GetSecrets did not return in time");

        assert_error_name_contains(&result, "NoSecrets");
    }

    // -- 3. non-wireless setting → NoSecrets ----------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn non_wireless_setting_is_rejected() {
        let (server, guard) = ephemeral_bus().await;
        let (agent, _prompts, _waiters) = fresh_agent();
        let proxy = mount_and_proxy(&server, &guard, agent).await;

        let conn = wpa_connection(b"FRITZ!Box", "wpa-psk");
        let result: Result<ConnectionDict, zbus::Error> = tokio::time::timeout(
            Duration::from_secs(3),
            proxy.call(
                "GetSecrets",
                &(
                    conn,
                    OwnedObjectPath::try_from("/org/freedesktop/NetworkManager/Connection/1")
                        .unwrap(),
                    "ipv4".to_string(),
                    Vec::<String>::new(),
                    FLAG_ALLOW_INTERACTION,
                ),
            ),
        )
        .await
        .expect("GetSecrets did not return in time");

        assert_error_name_contains(&result, "NoSecrets");
    }

    // -- 4. CancelGetSecrets → UserCanceled -----------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_resolves_with_user_canceled() {
        // This drives the *real* `CancelGetSecrets` D-Bus method (not the
        // waiter-drop shortcut): we start an interactive GetSecrets, wait for
        // the prompt, then call CancelGetSecrets over the wire. The agent drains
        // its waiters with Err, so the in-flight GetSecrets resolves to the
        // UserCanceled error name.
        let (server, guard) = ephemeral_bus().await;
        let (agent, prompts, _waiters) = fresh_agent();
        let proxy = mount_and_proxy(&server, &guard, agent).await;
        let proxy_for_call = proxy.clone();

        let conn = wpa_connection(b"FRITZ!Box", "wpa-psk");
        let call: tokio::task::JoinHandle<Result<ConnectionDict, zbus::Error>> =
            tokio::spawn(async move {
                proxy_for_call
                    .call(
                        "GetSecrets",
                        &(
                            conn,
                            OwnedObjectPath::try_from(
                                "/org/freedesktop/NetworkManager/Connection/1",
                            )
                            .unwrap(),
                            "802-11-wireless-security".to_string(),
                            vec!["psk".to_string()],
                            FLAG_ALLOW_INTERACTION,
                        ),
                    )
                    .await
            });

        // Wait until the agent is parked on the oneshot.
        let _req = await_prompt(&prompts, Duration::from_secs(3)).await;

        // Cancel over the wire.
        let cancel: Result<(), zbus::Error> = tokio::time::timeout(
            Duration::from_secs(3),
            proxy.call(
                "CancelGetSecrets",
                &(
                    OwnedObjectPath::try_from("/org/freedesktop/NetworkManager/Connection/1")
                        .unwrap(),
                    "802-11-wireless-security".to_string(),
                ),
            ),
        )
        .await
        .expect("CancelGetSecrets did not return in time");
        cancel.expect("CancelGetSecrets returned a D-Bus error");

        let result = tokio::time::timeout(Duration::from_secs(3), call)
            .await
            .expect("GetSecrets did not resolve after cancel")
            .expect("GetSecrets task panicked");

        assert_error_name_contains(&result, "UserCanceled");
        assert!(
            prompts.get_cloned().is_none(),
            "prompt should be cleared after cancel"
        );
    }

    // -- helpers --------------------------------------------------------------

    /// Assert the result is a `MethodError` whose error name contains `needle`
    /// (e.g. `"NoSecrets"` / `"UserCanceled"`). The full name is
    /// `org.freedesktop.NetworkManager.SecretAgent.Error.<variant>`.
    fn assert_error_name_contains(result: &Result<ConnectionDict, zbus::Error>, needle: &str) {
        match result {
            Err(zbus::Error::MethodError(name, _detail, _reply)) => {
                let name = name.as_str();
                assert!(
                    name.contains(needle),
                    "expected error name containing {needle:?}, got {name:?}"
                );
            }
            Err(other) => panic!("expected MethodError containing {needle:?}, got {other:?}"),
            Ok(dict) => panic!("expected MethodError containing {needle:?}, got Ok({dict:?})"),
        }
    }
}
