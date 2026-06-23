//! iwd `net.connman.iwd.Agent` D-Bus interface implementation.

use futures_channel::oneshot;
use futures_signals::signal::Mutable;
use std::sync::atomic::Ordering;

use super::types::PromptRequest;
use super::{NEXT_ID, WaitersMap};
use crate::wifi::client::read_network_metadata;

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
        let (tx, rx) = oneshot::channel::<Result<String, String>>();
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
        }));

        if let Ok(Ok(pass)) = rx.await {
            self.prompts.set(None);
            Ok(pass)
        } else {
            self.prompts.set(None);
            Err(zbus::fdo::Error::Failed("agent cancelled".into()))
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
