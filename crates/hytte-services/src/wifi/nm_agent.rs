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

use super::types::PromptRequest;
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

/// `NMSecretAgentGetSecretsFlags` bit 0 — `ALLOW_INTERACTION`. When unset, NM
/// only wants secrets it can return without prompting the user, so we must not
/// pop a passphrase dialog.
const FLAG_ALLOW_INTERACTION: u32 = 0x1;

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

/// Build the nested reply dict `{ setting_name: { "psk": <passphrase> } }`.
fn build_secret_reply(setting_name: &str, passphrase: &str) -> ConnectionDict {
    let mut setting: HashMap<String, OwnedValue> = HashMap::new();
    if let Ok(v) = Value::from(passphrase).try_to_owned() {
        setting.insert("psk".to_string(), v);
    }
    let mut out: ConnectionDict = HashMap::new();
    out.insert(setting_name.to_string(), setting);
    out
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
    /// setting whose secrets are wanted (e.g. `"802-11-wireless-security"`),
    /// `hints` the specific keys (e.g. `["psk"]`), and `flags` the
    /// `NMSecretAgentGetSecretsFlags`. We only handle the wireless-security PSK
    /// case interactively; everything else returns the NM "no secrets" error so
    /// NM can fall through to another agent or fail the activation cleanly.
    async fn get_secrets(
        &self,
        connection: ConnectionDict,
        connection_path: OwnedObjectPath,
        setting_name: String,
        hints: Vec<String>,
        flags: u32,
    ) -> Result<ConnectionDict, NmAgentError> {
        // Only wireless-security secrets are something we can prompt for.
        let wants_wireless =
            setting_name == WIRELESS_SECURITY_SETTING || hints.iter().any(|h| h == "psk");
        if !wants_wireless {
            return Err(NmAgentError::NoSecrets(
                "only 802-11-wireless-security supported".into(),
            ));
        }

        // If NM didn't grant interaction, we can't pop a dialog — and we hold
        // no stored secrets, so there is nothing to return.
        if flags & FLAG_ALLOW_INTERACTION == 0 {
            return Err(NmAgentError::NoSecrets(
                "interaction not allowed; no stored secret".into(),
            ));
        }

        let ssid = ssid_from_connection(&connection);
        let security = security_from_connection(&connection);
        let conn_path = connection_path.as_str().to_string();

        tracing::info!(
            ssid = %ssid,
            security = %security,
            path = %conn_path,
            "NM SecretAgent::GetSecrets — requesting passphrase",
        );

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<Result<String, String>>();
        {
            let mut waiters = self.waiters.lock().await;
            waiters.insert(id, tx);
        }
        self.prompts.set(Some(PromptRequest {
            id,
            network_path: conn_path,
            ssid,
            security,
        }));

        if let Ok(Ok(pass)) = rx.await {
            self.prompts.set(None);
            Ok(build_secret_reply(&setting_name, &pass))
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
        let reply = build_secret_reply(WIRELESS_SECURITY_SETTING, "hunter2");
        let inner = reply
            .get(WIRELESS_SECURITY_SETTING)
            .expect("setting present");
        let psk = inner.get("psk").expect("psk present");
        assert_eq!(
            String::try_from(psk.try_clone().unwrap()).unwrap(),
            "hunter2"
        );
    }
}
