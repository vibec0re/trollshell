//! What the embedded view is willing to trust, and — just as much — what it
//! is not.
//!
//! # The finding
//!
//! A hyperhive gateway serves **self-signed TLS by default**: a leaf issued by
//! a host-held hive CA under `services.hyperhive.deploy.hive-controller.tls`
//! (`/var/lib/hive-tls`). What a consumer is supposed to trust there is
//! `trust-bundle.pem` in that directory, **not** `ca.pem` — the hive CA is
//! itself an intermediate under the swarm root, and an intermediate is not a
//! chain a verifier can terminate at, so the bundle carries the CA plus what
//! it is rooted at (hyperhive `docs/networking/gateway.md`, "What consumers
//! trust"). Agents get it through `security.pki.certificateFiles`; the
//! operator's own machine does not, and the same doc says so: "browsers still
//! warn once per host until the hive's `trust-bundle.pem` is added to the
//! browser/OS trust store".
//!
//! And `host.sock` does not carry it. `HiveUrls` is `domain` / `home` /
//! `forge` / `matrix` and nothing else (`hive-host-sock/src/lib.rs`), so there
//! is no CA path or PEM on the wire for this window to pick up — the ask for
//! one is a follow-up on #948.
//!
//! # So
//!
//! The **system store** is the default, and the right fix on NixOS is to put
//! the bundle in it:
//!
//! ```text
//! security.pki.certificateFiles = [ "/var/lib/hive-tls/trust-bundle.pem" ];
//! ```
//!
//! which this window (and every browser on the machine) then picks up for
//! free. [`CA_ENV`] is the per-window override for when that is not available
//! — a hive whose bundle you can read but whose host config you do not own —
//! and it is deliberately **scoped to the agent's own host**: `WebKit` is told
//! to accept that one certificate for that one host, never to stop checking.
//!
//! There is no "ignore TLS errors" setting anywhere in this window, and
//! [`TlsPolicy::errors_policy`] is the mechanism that keeps it that way: it
//! returns [`webkit::TLSErrorsPolicy::Fail`] on **both** arms, so adding the
//! override cannot weaken the session it is added to.

use std::path::PathBuf;

/// Points the window at a PEM to accept for the agent's host.
///
/// The value is a path to a certificate file — for a default hyperhive
/// deployment, `/var/lib/hive-tls/trust-bundle.pem`.
pub const CA_ENV: &str = "TROLLSHELL_AGENT_WINDOW_CA";

/// How the embedded view decides whether it trusts the hive's certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TlsPolicy {
    /// Verify against the machine's trust store, like any other client. The
    /// default, and what a hive behind a real (or system-installed) CA needs.
    SystemStore,
    /// Additionally accept the certificate in `pem` **for `host` only**
    /// (`webkit_network_session_allow_tls_certificate_for_host`). Every other
    /// host on the session keeps the system store's answer.
    AllowCertificateForHost {
        /// The PEM [`CA_ENV`] named.
        pem: PathBuf,
        /// The agent URL's host — the only host this can affect.
        host: String,
    },
}

impl TlsPolicy {
    /// The session-wide TLS-error policy, which is
    /// [`Fail`](webkit::TLSErrorsPolicy::Fail) on **every** arm.
    ///
    /// The override adds a certificate to what one host may present; it never
    /// turns checking off. This function exists so that "never a global ignore"
    /// is a value a test can read rather than a claim in a comment.
    #[must_use]
    pub fn errors_policy(&self) -> webkit::TLSErrorsPolicy {
        webkit::TLSErrorsPolicy::Fail
    }

    /// The host this policy can affect, if any — `None` for
    /// [`TlsPolicy::SystemStore`], which changes nothing about any host.
    #[must_use]
    pub fn scoped_host(&self) -> Option<&str> {
        match self {
            Self::SystemStore => None,
            Self::AllowCertificateForHost { host, .. } => Some(host),
        }
    }
}

/// Decide the policy from the environment and the page URL.
///
/// Falls back to [`TlsPolicy::SystemStore`] whenever the override cannot be
/// **scoped**: an unset or blank [`CA_ENV`], or a URL with no host to scope it
/// to. An unscopable override is the one thing this must never widen into a
/// session-wide grant, so it degrades to the default instead.
#[must_use]
pub fn policy(ca: Option<&str>, url: &str) -> TlsPolicy {
    let Some(pem) = ca.map(str::trim).filter(|c| !c.is_empty()) else {
        return TlsPolicy::SystemStore;
    };
    let Some(host) = host_of(url) else {
        tracing::warn!(
            env = CA_ENV,
            url,
            "no host in the agent's URL to scope the CA override to; using the system trust store"
        );
        return TlsPolicy::SystemStore;
    };
    TlsPolicy::AllowCertificateForHost {
        pem: PathBuf::from(pem),
        host: host.to_owned(),
    }
}

/// The host part of an `http(s)` URL — no port, no userinfo, IPv6 literal kept
/// with its brackets (which is the form `WebKit`'s host matching uses).
///
/// Hand-rolled rather than a `url` crate dependency: this is the only URL
/// parsing in the window, it runs on one string the hive produced, and a
/// resolved package for it would be the crate's fifth.
#[must_use]
pub fn host_of(url: &str) -> Option<&str> {
    let rest = url.trim().split_once("://")?.1;
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|a| !a.is_empty())?;
    // userinfo@host — take what follows the LAST '@', since userinfo may
    // contain one.
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if let Some(end) = host.find(']') {
        // [::1]:443 — the bracketed literal is the host.
        return Some(&host[..=end]);
    }
    let host = host.split_once(':').map_or(host, |(h, _)| h);
    if host.is_empty() { None } else { Some(host) }
}

#[cfg(test)]
mod tests {
    use super::{TlsPolicy, host_of, policy};
    use std::path::PathBuf;

    /// With no override, nothing about the session's trust changes.
    #[test]
    fn no_override_means_the_system_store() {
        for ca in [None, Some(""), Some("   ")] {
            assert_eq!(
                policy(ca, "https://hive.local/agent/stray/"),
                TlsPolicy::SystemStore,
                "{ca:?}"
            );
        }
    }

    /// The override is scoped to **the agent's own host**, and to nothing
    /// else.
    ///
    /// Mutation (verified red): make the policy global — drop the `host` field
    /// and hand `WebKit` a session-wide grant, or return `Ignore` from
    /// `errors_policy` — and both assertions here red. That is the whole
    /// guard: "the CA is trusted programmatically" (spec §7.1) must never
    /// become "TLS errors are ignored".
    #[test]
    fn the_override_is_scoped_to_the_agents_host() {
        let p = policy(
            Some("/var/lib/hive-tls/trust-bundle.pem"),
            "https://hive.local/agent/stray/?hide=header,input",
        );
        assert_eq!(
            p,
            TlsPolicy::AllowCertificateForHost {
                pem: PathBuf::from("/var/lib/hive-tls/trust-bundle.pem"),
                host: "hive.local".to_owned(),
            }
        );
        assert_eq!(p.scoped_host(), Some("hive.local"));
    }

    /// Whatever the policy, the session still **fails** on a TLS error.
    ///
    /// Mutation (verified red): return `TLSErrorsPolicy::Ignore` on the
    /// override arm — the shortcut that would make a self-signed hive "just
    /// work" — and this reds.
    #[test]
    fn tls_errors_always_fail_on_every_policy() {
        for p in [
            TlsPolicy::SystemStore,
            TlsPolicy::AllowCertificateForHost {
                pem: PathBuf::from("/tmp/ca.pem"),
                host: "hive.local".to_owned(),
            },
        ] {
            assert_eq!(p.errors_policy(), webkit::TLSErrorsPolicy::Fail, "{p:?}");
        }
        assert_eq!(TlsPolicy::SystemStore.scoped_host(), None);
    }

    /// An override with no host to scope it to degrades to the system store
    /// rather than widening.
    ///
    /// Mutation (verified red): fall back to a session-wide grant when the
    /// host cannot be read, and this reds.
    #[test]
    fn an_unscopable_override_degrades_to_the_system_store() {
        for url in ["", "not a url", "https://", "file:///tmp/x"] {
            assert_eq!(
                policy(Some("/tmp/ca.pem"), url),
                TlsPolicy::SystemStore,
                "{url}"
            );
        }
    }

    #[test]
    fn the_host_is_read_without_its_port_or_userinfo() {
        assert_eq!(host_of("https://hive.local/agent/x/"), Some("hive.local"));
        assert_eq!(host_of("https://hive.local:8443/x"), Some("hive.local"));
        assert_eq!(host_of("https://user:pw@hive.local/x"), Some("hive.local"));
        assert_eq!(host_of("https://hive.local"), Some("hive.local"));
        assert_eq!(host_of("https://hive.local?a=1"), Some("hive.local"));
        assert_eq!(host_of("https://[::1]:8443/x"), Some("[::1]"));
        assert_eq!(host_of("  https://hive.local/x  "), Some("hive.local"));
        assert_eq!(host_of("hive.local/x"), None, "no scheme, no authority");
    }
}
