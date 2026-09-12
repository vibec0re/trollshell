//! What the embedded view is willing to trust, and — just as much — what it
//! is not.
//!
//! # The finding
//!
//! A hyperhive gateway serves **self-signed TLS by default**: a leaf issued by
//! a host-held hive CA under `services.hyperhive.deploy.hive-controller.tls`
//! (`/var/lib/hive-tls`). What a *trust store* wants there is
//! `trust-bundle.pem` in that directory, **not** `ca.pem` — the hive CA is
//! itself an intermediate under the swarm root, and an intermediate is not a
//! chain a verifier can terminate at, so the bundle carries the CA plus what
//! it is rooted at (hyperhive `docs/networking/gateway.md`, "What consumers
//! trust"). Agents get it through `security.pki.certificateFiles`; the
//! operator's own machine does not, and the same doc says so: "browsers still
//! warn once per host until the hive's `trust-bundle.pem` is added to the
//! browser/OS trust store".
//!
//! And `host.sock` does not carry it either way. `HiveUrls` is `domain` /
//! `home` / `forge` / `matrix` and nothing else
//! (`hive-host-sock/src/lib.rs`), so there is no CA path or PEM on the wire
//! for this window to pick up — the ask for one is a follow-up on #948.
//!
//! # The real fix is the system trust store
//!
//! On NixOS, where the hive is deployed on **this machine** — which is the
//! ordinary case, since `singleHostSwarm` puts it on the laptop — reference
//! hyperhive's own option rather than typing the path (Mara via
//! @the-sword-above on #948, 2026-09-11 20:41Z):
//!
//! ```text
//! security.pki.certificateFiles = [
//!   "${config.services.hyperhive.deploy.hive-controller.tls.stateDir}/trust-bundle.pem"
//! ];
//! ```
//!
//! so the two sides cannot drift when that `stateDir` moves. The literal
//! `/var/lib/hive-tls/trust-bundle.pem` is only that option's *default*, and
//! spelling it out is the **fallback** — for a hive on another host, or a host
//! whose configuration you do not own. Either way this window, and every
//! browser on the machine, then picks it up like any other client. That is the
//! route to prefer, and the one the inline error state names first.
//!
//! # …and the override is a *pin*, not an anchor
//!
//! [`CERT_ENV`] exists for when changing the system store is not available: a
//! hive whose certificate you can read but whose host configuration you do not
//! own. It is deliberately **scoped to the agent's own host**.
//!
//! **It must name the certificate the gateway PRESENTS — its leaf — and never
//! a CA or a bundle.** `webkit_network_session_allow_tls_certificate_for_host`
//! is documented as *"Ignore further TLS errors on the @host for the
//! certificate present in @info"*, and the `load-failed-with-tls-errors` doc
//! spells out where that value is meant to come from: *"to continue loading
//! use `…allow_tls_certificate_for_host()` with the certificate \[from the
//! signal\] and the host of `failing_uri`"* — i.e. the server's own certificate
//! (`WebKit-6.0.gir:10812-10820` and `:28594-28601`). It is an exception for
//! one certificate, not a trust anchor.
//!
//! `gio::TlsCertificate::from_file` on a multi-PEM bundle yields the **first**
//! certificate with the rest as its issuer chain, and `trust-bundle.pem`
//! starts with the hive CA — so handing it here produces a value that never
//! matches what the gateway sends, and the load fails anyway. The variable is
//! named `…_CERT` rather than `…_CA` for exactly that reason, and
//! [`a_bundle_is_not_the_certificate_the_gateway_presents`] pins the
//! difference against real PEMs.
//!
//! Get the right value with:
//!
//! ```text
//! openssl s_client -connect <hive-domain>:443 -showcerts </dev/null \
//!   | openssl x509 > /tmp/hive-gateway.pem
//! ```
//!
//! # And it never widens
//!
//! There is no "ignore TLS errors" setting anywhere in this window, and
//! [`TlsPolicy::errors_policy`] is the mechanism that keeps it that way: it
//! returns [`webkit::TLSErrorsPolicy::Fail`] on **both** arms, so adding the
//! override cannot weaken the session it is added to. A failure renders
//! [`failure_message`] in place, naming both routes and the host that failed.
//!
//! [`a_bundle_is_not_the_certificate_the_gateway_presents`]: tests::a_bundle_is_not_the_certificate_the_gateway_presents

use std::path::PathBuf;

/// Points the window at the certificate to accept for the agent's host.
///
/// The value is a path to **the gateway's own leaf certificate**, in PEM — not
/// a CA and not a bundle. See this module's docs for why, and for the
/// `openssl s_client` line that produces it.
pub const CERT_ENV: &str = "TROLLSHELL_AGENT_WINDOW_CERT";

/// How the embedded view decides whether it trusts the hive's certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TlsPolicy {
    /// Verify against the machine's trust store, like any other client. The
    /// default, and what a hive behind a real (or system-installed) CA needs.
    SystemStore,
    /// Additionally accept **exactly the certificate** in `pem`, and only when
    /// `host` presents it
    /// (`webkit_network_session_allow_tls_certificate_for_host`). Every other
    /// host on the session keeps the system store's answer, and so does this
    /// host for any other certificate.
    AllowCertificateForHost {
        /// The leaf PEM [`CERT_ENV`] named.
        pem: PathBuf,
        /// The agent URL's host — the only host this can affect.
        host: String,
    },
}

impl TlsPolicy {
    /// The session-wide TLS-error policy, which is
    /// [`Fail`](webkit::TLSErrorsPolicy::Fail) on **every** arm.
    ///
    /// The override adds one certificate to what one host may present; it
    /// never turns checking off. This function exists so that "never a global
    /// ignore" is a value a test can read rather than a claim in a comment.
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
/// **scoped**: an unset or blank [`CERT_ENV`], or a URL with no host to scope
/// it to. An unscopable override is the one thing this must never widen into a
/// session-wide grant, so it degrades to the default instead.
#[must_use]
pub fn policy(cert: Option<&str>, url: &str) -> TlsPolicy {
    let Some(pem) = cert.map(str::trim).filter(|c| !c.is_empty()) else {
        return TlsPolicy::SystemStore;
    };
    let Some(host) = host_of(url) else {
        tracing::warn!(
            env = CERT_ENV,
            url,
            "no host in the agent's URL to scope the certificate exception to; using the system \
             trust store"
        );
        return TlsPolicy::SystemStore;
    };
    TlsPolicy::AllowCertificateForHost {
        pem: PathBuf::from(pem),
        host: host.to_owned(),
    }
}

/// What the window shows **in place of the page** when TLS verification fails.
///
/// It names the failing host and the routes in the order to try them:
///
/// 1. the system trust store, referencing hyperhive's own `stateDir` option —
///    the same-host form Mara asked for (#948, 2026-09-11 20:41Z), so the two
///    sides cannot drift when that directory moves;
/// 2. the literal path, for a **remote** hive or a host whose configuration
///    you do not own;
/// 3. the per-window pin, last — with the warning that it wants the gateway's
///    own certificate, since handing it a bundle is the mistake this text
///    exists to prevent.
#[must_use]
pub fn failure_message(host: &str) -> String {
    format!(
        "Could not verify {host}'s certificate.\n\n\
         A hyperhive gateway serves a self-signed certificate by default, and this machine's \
         trust store does not carry the hive's anchor.\n\n\
         • If the hive runs on this machine (preferred — every client then works, and the path \
         follows hyperhive's own option):\n\
         \u{a0}\u{a0}security.pki.certificateFiles = [\n\
         \u{a0}\u{a0}\u{a0}\u{a0}\"${{config.services.hyperhive.deploy.hive-controller.tls.stateDir}}/trust-bundle.pem\"\n\
         \u{a0}\u{a0}];\n\n\
         • For a remote hive, or a host you do not configure, copy that file over and name it \
         literally (the option's default is /var/lib/hive-tls):\n\
         \u{a0}\u{a0}security.pki.certificateFiles = [ \"/etc/ssl/hive/trust-bundle.pem\" ];\n\n\
         • Or, as a last resort, pin this one certificate for this one host — {CERT_ENV} must \
         name the certificate the gateway PRESENTS (its leaf), never the CA bundle:\n\
         \u{a0}\u{a0}openssl s_client -connect {host}:443 -showcerts </dev/null | openssl x509 \
         > hive-gateway.pem\n\n\
         The header above still follows this agent's live status over host.sock, which does not \
         use TLS."
    )
}

/// The host part of an `http(s)` URL — no port, no userinfo, IPv6 literal kept
/// with its brackets (which is the form `WebKit`'s host matching uses).
///
/// Hand-rolled rather than a `url` crate dependency: this is the only URL
/// parsing in the window, it runs on one string the hive produced, and a
/// resolved package for it would be the crate's fifth.
///
/// It is also what [`crate::page::navigable_in_place`] compares, so read that
/// function's docs for what the port-stripping means for the origin rule.
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
    use super::{CERT_ENV, TlsPolicy, failure_message, host_of, policy};
    use std::path::PathBuf;

    /// With no override, nothing about the session's trust changes.
    #[test]
    fn no_override_means_the_system_store() {
        for cert in [None, Some(""), Some("   ")] {
            assert_eq!(
                policy(cert, "https://hive.local/agent/stray/"),
                TlsPolicy::SystemStore,
                "{cert:?}"
            );
        }
    }

    /// The override is scoped to **the agent's own host**, and to nothing
    /// else.
    ///
    /// Mutation (re-run this round, red): make the policy global — drop the
    /// `host` field and hand `WebKit` a session-wide grant, or return `Ignore`
    /// from `errors_policy` — and both assertions here red. That is the whole
    /// guard: "the certificate is trusted programmatically" (spec §7.1) must
    /// never become "TLS errors are ignored".
    #[test]
    fn the_override_is_scoped_to_the_agents_host() {
        let p = policy(
            Some("/tmp/hive-gateway.pem"),
            "https://hive.local/agent/stray/?hide=header,input",
        );
        assert_eq!(
            p,
            TlsPolicy::AllowCertificateForHost {
                pem: PathBuf::from("/tmp/hive-gateway.pem"),
                host: "hive.local".to_owned(),
            }
        );
        assert_eq!(p.scoped_host(), Some("hive.local"));
    }

    /// Whatever the policy, the session still **fails** on a TLS error.
    ///
    /// Mutation (re-run this round, red): return `TLSErrorsPolicy::Ignore` on
    /// the override arm — the shortcut that would make a self-signed hive
    /// "just work" — and this reds.
    #[test]
    fn tls_errors_always_fail_on_every_policy() {
        for p in [
            TlsPolicy::SystemStore,
            TlsPolicy::AllowCertificateForHost {
                pem: PathBuf::from("/tmp/hive-gateway.pem"),
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
    /// Mutation (re-run this round, red): fall back to a session-wide grant
    /// when the host cannot be read, and this reds.
    #[test]
    fn an_unscopable_override_degrades_to_the_system_store() {
        for url in ["", "not a url", "https://", "file:///tmp/x"] {
            assert_eq!(
                policy(Some("/tmp/cert.pem"), url),
                TlsPolicy::SystemStore,
                "{url}"
            );
        }
    }

    /// **A CA bundle is not the certificate the gateway presents**, which is
    /// what `allow_tls_certificate_for_host` pins — so `CERT_ENV` must name
    /// the leaf, and the docs must say leaf.
    ///
    /// The reviewer's test, against two real PEMs generated for it
    /// (`tests/fixtures/README.md` has the `openssl` lines): a self-signed CA
    /// and a leaf it issued, plus the two-certificate bundle. It is the
    /// assertion the prose is written against — without it, "point the
    /// variable at trust-bundle.pem" reads plausible and sends the operator
    /// round in circles behind an INFO line that says it worked.
    ///
    /// # On the shape of the assertion
    ///
    /// The reviewer wrote it as `gio::TlsCertificate::from_file(…).is_same(…)`.
    /// Measured, that cannot run here: `from_file` needs glib-networking's TLS
    /// backend module, and both the devShell and `checks.system-tests` answer
    /// `"TLS support is not available"`. Rather than add a `GIO_EXTRA_MODULES`
    /// to a check's closure for one comparison, this asserts the same fact one
    /// layer down, where it actually lives — **the first PEM block of the
    /// bundle is not the leaf**. That is precisely what `from_file` would read
    /// ("the first certificate, with the rest as its issuer chain") and
    /// precisely what `WebKit` then fails to match against the server's own.
    ///
    /// Mutation (re-run this round, red): compare `gateway-leaf.pem` with
    /// itself — i.e. assert the thing the old docs implied — and it reds.
    #[test]
    fn a_bundle_is_not_the_certificate_the_gateway_presents() {
        /// The first PEM block — what `gio::TlsCertificate::from_file` takes as
        /// *the* certificate, treating everything after it as the chain.
        fn first_block(pem: &str) -> &str {
            let start = pem
                .find("-----BEGIN CERTIFICATE-----")
                .expect("a PEM fixture has a certificate");
            let end = pem[start..]
                .find("-----END CERTIFICATE-----")
                .expect("…and it is terminated")
                + start;
            &pem[start..end]
        }

        let fixture = |name: &str| {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name);
            std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("missing fixture {} ({e})", path.display()))
        };

        let bundle = fixture("trust-bundle.pem");
        let leaf = fixture("gateway-leaf.pem");
        let ca = fixture("hive-ca.pem");

        assert_ne!(
            first_block(&bundle),
            first_block(&leaf),
            "allow_tls_certificate_for_host pins THIS certificate; a CA bundle never matches \
             what the server sends, so {CERT_ENV} must name the leaf"
        );
        assert_eq!(
            first_block(&bundle),
            first_block(&ca),
            "…because what the bundle leads with is the anchor, which is the whole point of a \
             trust bundle and the whole reason it is the wrong file for this variable"
        );
        // …and the leaf compared with itself *is* equal, so the assertion above
        // is about the bundle and not about the comparison being unable to say
        // yes.
        assert_eq!(
            first_block(&leaf),
            first_block(&fixture("gateway-leaf.pem"))
        );
    }

    /// The inline error state names the failing host and **both** routes, and
    /// warns that the pin wants the leaf.
    ///
    /// Mutation (re-run this round, red): drop either route, or the host, and
    /// this reds. A TLS failure is the one moment the operator has no other
    /// source of instructions — the window has no address bar and `WebKit`'s own
    /// error page says only "load failed".
    #[test]
    fn the_failure_state_names_the_host_and_both_routes() {
        let m = failure_message("hive.local");
        assert!(m.contains("hive.local"), "{m}");
        assert!(m.contains("security.pki.certificateFiles"), "{m}");
        assert!(m.contains("trust-bundle.pem"), "{m}");
        assert!(m.contains(CERT_ENV), "{m}");
        assert!(m.contains("openssl s_client"), "{m}");
        assert!(
            m.contains("PRESENTS"),
            "the leaf-not-bundle warning is the point of the last route: {m}"
        );
    }

    /// The **same-host** route leads, and it references hyperhive's own option
    /// rather than a path this window typed out (Mara on #948, 2026-09-11
    /// 20:41Z) — so the two sides cannot drift when that `stateDir` moves.
    ///
    /// Mutation (re-run this round, red): put the literal
    /// `/var/lib/hive-tls/trust-bundle.pem` back as the first bullet and the
    /// ordering assertion reds.
    #[test]
    fn the_failure_state_leads_with_hyperhives_own_option() {
        let m = failure_message("hive.local");
        let option_form = m
            .find("config.services.hyperhive.deploy.hive-controller.tls.stateDir")
            .expect("the same-host route names hyperhive's option");
        let literal = m
            .find("/var/lib/hive-tls")
            .expect("…and the literal default is still mentioned, as the fallback");
        let pin = m.find(CERT_ENV).expect("…and the pin is named");

        assert!(
            option_form < literal,
            "the option-referencing form comes first; the literal path is the fallback for a \
             remote hive or a host you do not own: {m}"
        );
        assert!(
            literal < pin,
            "and the trust store — either spelling — comes before the per-window pin: {m}"
        );
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
