//! Synchronous PAM authentication for screen-unlock and similar
//! "verify the current user's password" flows.
//!
//! libpam itself is C and blocking. Authenticate from a
//! `tokio::task::spawn_blocking` so the GTK main loop isn't held up
//! by the PAM stack's I/O (notably `pam_unix` ↔ `unix_chkpwd`).

use thiserror::Error;
pub use zeroize::Zeroizing;

#[derive(Debug, Error)]
pub enum PamError {
    #[error("authentication failed")]
    AuthFailed,
    #[error("PAM service error: {0}")]
    Service(String),
    #[error("PAM session error: {0}")]
    Session(String),
}

/// Verify `password` against the PAM stack configured for `service`
/// as `username`. Returns `Ok(())` on success.
///
/// Blocks the calling thread. Always call from
/// `tokio::task::spawn_blocking` or a dedicated worker thread.
pub fn authenticate(
    service: &str,
    username: &str,
    password: &Zeroizing<String>,
) -> Result<(), PamError> {
    let mut client = pam::Client::with_password(service)
        .map_err(|e| PamError::Service(e.to_string()))?;
    client
        .conversation_mut()
        .set_credentials(username, password.as_str());
    client.authenticate().map_err(|_| PamError::AuthFailed)?;
    client
        .open_session()
        .map_err(|e| PamError::Session(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_surface_compiles() {
        let _: fn(&str, &str, &Zeroizing<String>) -> Result<(), PamError> = authenticate;
    }
}
