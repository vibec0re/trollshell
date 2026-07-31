//! iwd `net.connman.iwd.Agent` D-Bus interface implementation.

use futures_channel::oneshot;
use futures_signals::signal::Mutable;
use std::sync::atomic::Ordering;

use super::types::PromptRequest;
use super::{NEXT_ID, WaitersMap};
use crate::wifi::client::read_network_metadata;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Placeholder [`PromptRequest::secret_keys`] entry for the iwd backend. iwd's
/// `RequestPassphrase` takes a bare string with no key name, so this exists
/// only to keep the prompt's "one value per key" contract well-formed; the
/// overlay never renders it (a one-key prompt shows a single unlabelled entry).
const IWD_PASSPHRASE_KEY: &str = "passphrase";

// ── Agent ─────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(super) struct IwdAgent {
    pub(super) prompts: Mutable<Option<PromptRequest>>,
    pub(super) waiters: WaitersMap,
}

// zbus's `#[interface]` macro requires every method to be `async fn` even
// when the body doesn't await; the EAP stubs also have unused parameters
// since they reject the request without inspecting it. Allowing at the
// impl-block keeps the noise out of each method.
#[allow(clippy::unused_async, unused_variables)]
#[zbus::interface(name = "net.connman.iwd.Agent")]
impl IwdAgent {
    async fn release(&self) {
        tracing::info!("iwd Agent::Release");
    }

    async fn request_passphrase(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<String> {
        let path = network.as_str().to_string();
        let (ssid, security) = read_network_metadata(&path).await;

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<Result<Vec<String>, String>>();
        {
            let mut waiters = self.waiters.lock().await;
            waiters.insert(id, tx);
        }
        self.prompts.set(Some(PromptRequest {
            id,
            network_path: path,
            ssid,
            security,
            kind: super::types::PromptKind::WifiPassphrase,
            // iwd's RequestPassphrase carries no retry/failure signal (unlike
            // NM's REQUEST_NEW flag) — see the field doc on PromptRequest.
            prior_failure: false,
            // iwd has no secret-key vocabulary: RequestPassphrase returns one
            // bare string. One key keeps the overlay on its single-entry,
            // unlabelled path, so this placeholder is never rendered.
            secret_keys: vec![IWD_PASSPHRASE_KEY.to_string()],
        }));

        // Exactly one key was requested, so exactly one value comes back. An
        // empty vector can only mean a malformed submit; treat it as a cancel
        // rather than handing iwd an empty passphrase it would just reject.
        match rx.await {
            Ok(Ok(secrets)) if !secrets.is_empty() => {
                self.prompts.set(None);
                Ok(secrets.into_iter().next().unwrap_or_default())
            }
            _ => {
                self.prompts.set(None);
                Err(zbus::fdo::Error::Failed("agent cancelled".into()))
            }
        }
    }

    async fn request_private_key_passphrase(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::Failed(
            "hytte wifi agent does not support EAP".into(),
        ))
    }

    async fn request_user_name_and_password(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<(String, String)> {
        Err(zbus::fdo::Error::Failed(
            "hytte wifi agent does not support EAP".into(),
        ))
    }

    async fn request_user_password(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
        username: String,
    ) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::Failed(
            "hytte wifi agent does not support EAP".into(),
        ))
    }

    async fn cancel(&self, reason: String) {
        tracing::info!(%reason, "iwd Agent::Cancel");
        let mut waiters = self.waiters.lock().await;
        for (_, tx) in waiters.drain() {
            let _ = tx.send(Err("cancelled".into()));
        }
        self.prompts.set(None);
    }
}
