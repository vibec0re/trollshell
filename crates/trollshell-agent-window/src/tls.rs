//! What the embedded view is willing to trust, and — just as much — what it
//! is not.
//!
//! [`crate::verify`] decides **where the material comes from** (the four
//! routes, the probe, the flag names); this module turns whatever it found
//! into the one thing `WebKit` understands — a [`TlsPolicy`] — and writes the
//! card the operator reads when it was not enough.
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
//! trust").
//!
//! And `host.sock` does not carry it either way. `HiveUrls` is `domain` /
//! `home` / `forge` / `matrix` and nothing else
//! (`hive-host-sock/src/lib.rs`), so there is no CA path or PEM on the wire
//! for this window to pick up — the ask for one is a follow-up on #948.
//!
//! # Since #1234, the same-host case needs no setting
//!
//! Mara asked for that directly (#1224, 2026-09-13 12:12Z: *"i dont want to
//! run some cmd for manual setup tho"*), and both files the window needs are
//! already on disk on a same-host deploy. [`resolve`] reads them — see
//! [`crate::verify`] for the four-route precedence and why it is that order.
//! The window still never adds an anchor to anything: route 2 **verifies** the
//! chain the gateway presents against the hive's bundle and then pins the leaf
//! that verification accepted, which is the only shape `webkit6` 0.6 offers.
//!
//! # …and every override is a *pin*, not an anchor
//!
//! [`CERT_ENV`] exists for when the hive is on another machine: a gateway
//! whose certificate you can read but whose host configuration you do not own.
//! It is deliberately **scoped to the agent's own host**.
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
//! certificate with the rest as its issuer chain. `trust-bundle.pem` starts
//! with the hive CA — so handing *that* here produces a value that never
//! matches what the gateway sends, and the load fails anyway;
//! `a_bundle_is_not_the_certificate_the_gateway_presents` pins the difference
//! against real PEMs. hyperhive's `gateway.pem` starts with the **leaf** (it is
//! built `cat leaf-only ca.pem`), which is exactly why route 3 can read it
//! straight into the pin.
//!
//! Get the right value for a remote hive with:
//!
//! ```text
//! openssl s_client -connect <hive-domain>:443 -showcerts </dev/null \
//!   | openssl x509 > /tmp/hive-gateway.pem
//! ```
//!
//! …or, better, point [`CA_ENV`](crate::verify::CA_ENV) at a copy of that
//! hive's `trust-bundle.pem` and let route 2 track the leaf for you.
//!
//! # And it never widens
//!
//! There is no "ignore TLS errors" setting anywhere in this window, and
//! [`TlsPolicy::errors_policy`] is the mechanism that keeps it that way: it
//! returns [`webkit::TLSErrorsPolicy::Fail`] on **both** arms, so adding an
//! override cannot weaken the session it is added to. A failure renders
//! [`failure_message`] in place, naming the file this launch actually tried,
//! what happened to it, and every route in order.

use std::path::{Path, PathBuf};

use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib;

use crate::verify::{
    self, Env, EnvOwned, PROBE_DEADLINE, PROBE_IO_TIMEOUT_SECS, Presented, Route, describe_flags,
};

/// Points the window at the certificate to accept for the agent's host.
///
/// The value is a path to **the gateway's own leaf certificate**, in PEM — not
/// a CA and not a bundle. See this module's docs for why, and for the
/// `openssl s_client` line that produces it. Since #1234 it is the *last*
/// resort rather than the only one: it outranks the automatic routes when set
/// (a variable someone typed wins), but a same-host deploy needs none of them.
pub const CERT_ENV: &str = "TROLLSHELL_AGENT_WINDOW_CERT";

/// The certificate a policy pins, and where its bytes come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pinned {
    /// A PEM file whose **first block** is the certificate to pin — routes 1
    /// ([`CERT_ENV`]) and 3 (hyperhive's `gateway.pem`, leaf first).
    File(PathBuf),
    /// The leaf the gateway presented, captured during the launch-time probe
    /// and **already verified** against the hive's anchors — route 2.
    ///
    /// A PEM string rather than a `gio::TlsCertificate` so that [`TlsPolicy`]
    /// stays plain data: comparable, printable, and constructible in a test
    /// with no TLS backend loaded.
    VerifiedPem(String),
}

/// How the embedded view decides whether it trusts the hive's certificate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TlsPolicy {
    /// Verify against the machine's trust store, like any other client. The
    /// default, and what a hive behind a real (or system-installed) CA needs.
    #[default]
    SystemStore,
    /// Additionally accept **exactly one certificate**, and only when `host`
    /// presents it
    /// (`webkit_network_session_allow_tls_certificate_for_host`). Every other
    /// host on the session keeps the system store's answer, and so does this
    /// host for any other certificate.
    AllowCertificateForHost {
        /// What to pin, and where it came from.
        cert: Pinned,
        /// The agent URL's host — the only host this can affect.
        host: String,
    },
}

impl TlsPolicy {
    /// The session-wide TLS-error policy, which is
    /// [`Fail`](webkit::TLSErrorsPolicy::Fail) on **every** arm.
    ///
    /// An override adds one certificate to what one host may present; it never
    /// turns checking off. This function exists so that "never a global
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

/// What one launch's trust decision came to.
///
/// [`Default`] is route 4 with nothing to report — the value a caller that
/// does no resolution at all (a display test building a page widget) wants.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    /// What the network session is configured with.
    pub policy: TlsPolicy,
    /// One sentence naming the file this launch tried and what came of it, for
    /// the card to show if the page still fails.
    ///
    /// `None` only when nothing was tried — route 4, or a URL with no host to
    /// scope anything to.
    pub tried: Option<String>,
}

impl Resolved {
    /// Nothing was added to the session's trust.
    fn system_store(tried: Option<String>) -> Self {
        Self {
            policy: TlsPolicy::SystemStore,
            tried,
        }
    }
}

/// Decide this launch's trust policy for `url`, reading the environment and
/// the hive's own files.
///
/// This is the whole of #1234's ask 1 and the amendment's precedence, in the
/// order [`crate::verify::route`] states. It performs I/O — a file open per
/// candidate path, and on route 2 a bounded TLS handshake to the gateway — so
/// it is called once per window, from the point where `host.sock` has already
/// handed over this agent's URL.
///
/// # It runs on a worker thread, not the GTK main thread (#1246)
///
/// Every call this makes blocks, and [`PROBE_DEADLINE`] bounds the worst of
/// them rather than removing it. So `window.rs`'s `begin_probe` runs this on a
/// thread of its own and hands the [`Resolved`] back over a
/// `tokio::sync::oneshot` the main context awaits; nothing here touches a
/// widget, which is what makes that legal. Call it from the main thread and
/// the window freezes for as long as the hive takes — the shape #1246 exists
/// to retire.
#[must_use]
pub fn resolve(url: &str) -> Resolved {
    let owned = EnvOwned::from_process();
    let route = verify::route(&Env::of(&owned), &verify::is_readable);
    resolve_route(&route, url)
}

/// [`resolve`] with the route already decided — the seam the fixture tests
/// drive, so the probe and the pin are exercised by the same code a launch
/// runs rather than by a copy of it.
#[must_use]
pub fn resolve_route(route: &Route, url: &str) -> Resolved {
    resolve_route_within(route, url, PROBE_DEADLINE)
}

/// [`resolve_route`] with the probe's wall-clock budget named too.
///
/// The same seam [`crate::verify::probe`] already carries one level down, and
/// for the same reason: a test that is *about* the deadline has to be able to
/// pick a budget it can wait out, and a test that is not about the deadline
/// has to be able to pick one a slow CI box cannot trip. A launch always takes
/// [`PROBE_DEADLINE`] through [`resolve_route`]; nothing outside `cfg(test)`
/// passes anything else.
#[must_use]
pub fn resolve_route_within(route: &Route, url: &str, budget: std::time::Duration) -> Resolved {
    if matches!(route, Route::SystemStore) {
        return Resolved::system_store(None);
    }
    let Some(host) = host_of(url) else {
        // An override that cannot be **scoped** is the one thing this must
        // never widen into a session-wide grant, so it degrades to the default
        // instead.
        tracing::warn!(
            url,
            "no host in the agent's URL to scope a certificate exception to; using the system \
             trust store"
        );
        return Resolved::system_store(None);
    };
    let port = port_of(url);

    match route {
        Route::SystemStore => Resolved::system_store(None),
        Route::PinLeaf { pem, source } => pin_from_file(pem, &source.describe(), host, port),
        Route::VerifyAgainstBundle { bundle, source } => {
            verify_then_pin(bundle, &source.describe(), host, port, budget)
        }
    }
}

/// Routes 1 and 3: pin the first certificate in `pem` for `host`.
///
/// The certificate is loaded here as well as at the sink, for one reason: to
/// **say something true on the card**. `gio::TlsCertificate::from_file` reads
/// the first PEM block as the certificate, so a file in the wrong order — a
/// bundle where a `gateway.pem` was expected, or a `gateway.pem` written
/// CA-first — loads perfectly and then never matches. Verifying the identity
/// (and only the identity: `trusted_ca` is `None`, so `UNKNOWN_CA` is expected
/// and ignored) turns that silence into "the certificate in this file does not
/// name this host".
///
/// It still pins: an identity check can be wrong about an exotic SAN, and a
/// pin that does not match costs nothing but the card that was coming anyway.
fn pin_from_file(pem: &Path, source: &str, host: &str, port: u16) -> Resolved {
    let shown = pem.display();
    let cert = match gio::TlsCertificate::from_file(pem) {
        Ok(cert) => cert,
        Err(e) => {
            tracing::warn!(pem = %shown, error = %e, "the pinned certificate could not be read");
            return Resolved::system_store(Some(format!(
                "read {shown} ({source}), which could not be parsed as a certificate ({e}); the \
                 machine's own trust store was used instead"
            )));
        }
    };

    let identity = gio::NetworkAddress::new(identity_host(host), port);
    let named_host = !cert
        .verify(Some(&identity), None::<&gio::TlsCertificate>)
        .contains(gio::TlsCertificateFlags::BAD_IDENTITY);

    let tried = if named_host {
        tracing::info!(
            pem = %shown,
            host,
            "pinned the certificate in this file for this host"
        );
        format!("pinned the certificate in {shown} ({source}) for {host}")
    } else {
        tracing::warn!(
            pem = %shown,
            host,
            "the first certificate in this file does not name the agent's host — in hyperhive's \
             gateway.pem the LEAF comes first with the CA appended, and a trust bundle is the \
             other way round"
        );
        format!(
            "pinned the first certificate in {shown} ({source}), but it does not name {host} — in \
             hyperhive's gateway.pem the leaf comes first with the CA appended, and a trust \
             bundle is the other way round, so this is probably a CA where a leaf was expected"
        )
    };

    Resolved {
        policy: TlsPolicy::AllowCertificateForHost {
            cert: Pinned::File(pem.to_path_buf()),
            host: host.to_owned(),
        },
        tried: Some(tried),
    }
}

/// Route 2: verify what `host` presents against `bundle`, then pin it.
fn verify_then_pin(
    bundle: &Path,
    source: &str,
    host: &str,
    port: u16,
    budget: std::time::Duration,
) -> Resolved {
    let shown = bundle.display();
    match verify::probe(
        bundle,
        identity_host(host),
        port,
        PROBE_IO_TIMEOUT_SECS,
        budget,
    ) {
        Presented::Trusted(pem) => {
            tracing::info!(
                bundle = %shown,
                host,
                "the hive's anchors signed what the gateway presented; pinning that leaf for this \
                 launch"
            );
            Resolved {
                policy: TlsPolicy::AllowCertificateForHost {
                    cert: Pinned::VerifiedPem(pem),
                    host: host.to_owned(),
                },
                tried: Some(format!(
                    "verified {host}'s certificate against the anchors in {shown} ({source}) and \
                     pinned the leaf it presented"
                )),
            }
        }
        Presented::Refused { flags } => {
            let why = describe_flags(flags);
            tracing::warn!(
                bundle = %shown,
                host,
                %why,
                "the hive's anchors refused what the gateway presented; using the system trust \
                 store"
            );
            Resolved::system_store(Some(format!(
                "checked {host}'s certificate against the anchors in {shown} ({source}) and they \
                 refused it: {why}"
            )))
        }
        Presented::Unreachable(why) => {
            tracing::warn!(
                bundle = %shown,
                host,
                %why,
                "could not obtain a certificate from the gateway to check — this is a reachability \
                 problem, not a trust one"
            );
            Resolved::system_store(Some(format!(
                "could not check {host} against the anchors in {shown} ({source}): {why}. That is \
                 a connection problem rather than a certificate one — the hive's gateway may be \
                 down while its host.sock is up"
            )))
        }
        // **No connection was attempted**, so the sentence above would be a
        // lie: it is the anchors file itself that is unusable, and naming it
        // plus the parse error is the whole of what an operator with a typo
        // needs (#1242 review, finding 2).
        Presented::UnusableAnchors(why) => {
            tracing::warn!(
                bundle = %shown,
                host,
                %why,
                "the anchors file could not be used, so nothing was contacted — this says nothing \
                 about the hive"
            );
            Resolved::system_store(Some(format!(
                "could not use the anchors in {shown} ({source}): {why}. Nothing was contacted, \
                 so this says nothing about whether the hive is up — fix or re-point that file"
            )))
        }
    }
}

/// What the window shows **in place of the page** when TLS verification fails.
///
/// It names the failing host, what this launch actually tried (`tried`, from
/// [`Resolved`] — the file, where it came from, and what happened to it), and
/// then the routes in the order to try them:
///
/// 1. **nothing at all**, when the hive is on this machine — the window reads
///    hyperhive's own `trust-bundle.pem` / `gateway.pem` and the nix module
///    points it at that directory (#1234, Mara's "no manual setup");
/// 2. [`CA_ENV`](crate::verify::CA_ENV), for a hive on another host whose
///    anchors you can copy — still rotation-safe, because what gets pinned is
///    whatever the gateway presents today;
/// 3. the machine-wide route, which is `security.pki.certificateFiles` over a
///    copy of the bundle **committed into the flake** — never the
///    `/var/lib/hive-tls` path, which `cacert`'s derivation cannot read inside
///    the build sandbox (#1234 finding 1: the advice this card used to give
///    broke the reader's `nixos-rebuild`);
/// 4. the per-window pin, last — with the warning that it wants the gateway's
///    own certificate, since handing it a bundle is the mistake this text
///    exists to prevent.
///
/// # Not markup — escape before handing this to `AdwStatusPage`
///
/// The window shows this text via `AdwStatusPage::set_description`, and
/// libadwaita's own docs say that property "is parsed as Pango markup". This
/// string is not valid markup: the `openssl` one-liner's `</dev/null`
/// (redirecting `stdin`, not closing a tag) reads to the parser as a closing
/// tag with no matching open, so the parse fails and `AdwStatusPage` renders
/// an **empty** description — the card had a title and nothing else (#1224).
/// Use [`failure_description`] at the sink; this function returns the raw
/// text so tests can pin the bug's mechanism against it.
#[must_use]
pub fn failure_message(host: &str, tried: Option<&str>) -> String {
    let ca_env = verify::CA_ENV;
    let dir_env = verify::TLS_DIR_ENV;
    let default_dir = verify::DEFAULT_TLS_DIR;
    let bundle = verify::BUNDLE_NAME;
    let gateway = verify::GATEWAY_NAME;
    let attempt = tried.map_or_else(String::new, |t| format!("This launch {t}.\n\n"));
    format!(
        "Could not verify {host}'s certificate.\n\n\
         {attempt}\
         A hyperhive gateway serves a self-signed certificate by default, and this machine's \
         trust store does not carry the hive's anchor.\n\n\
         • If the hive runs on this machine there is nothing to set: the window reads \
         {bundle} and {gateway} out of hyperhive's own TLS directory, and the NixOS / \
         home-manager module points {dir_env} at \
         config.services.hyperhive.deploy.hive-controller.tls.stateDir for you \
         (default {default_dir}). If this card is showing anyway, neither file was readable \
         — check that directory.\n\n\
         • For a hive on another host, copy its {bundle} over and name it:\n\
         \u{a0}\u{a0}{ca_env}=/etc/ssl/hive/{bundle}\n\
         \u{a0}\u{a0}The window verifies what the gateway presents against those anchors and \
         pins the leaf, so a re-signed leaf keeps working.\n\n\
         • To trust the hive machine-wide (every browser too), COPY the bundle into your NixOS \
         flake and reference the copy — a /var/lib path here fails the build, because cacert's \
         derivation opens it inside the sandbox:\n\
         \u{a0}\u{a0}cp {default_dir}/{bundle} ./hive-ca.pem\n\
         \u{a0}\u{a0}security.pki.certificateFiles = [ ./hive-ca.pem ];\n\
         \u{a0}\u{a0}Re-copy after the hive rotates its CA.\n\n\
         • Or, as a last resort, pin this one certificate for this one host — {CERT_ENV} must \
         name the certificate the gateway PRESENTS (its leaf), never the CA bundle:\n\
         \u{a0}\u{a0}openssl s_client -connect {host}:443 -showcerts </dev/null | openssl x509 \
         > hive-gateway.pem\n\n\
         The header above still follows this agent's live status over host.sock, which does not \
         use TLS."
    )
}

/// [`failure_message`], escaped so it is safe to hand to
/// `AdwStatusPage::set_description` — the sink's **one** call. See
/// [`failure_message`]'s docs for why the raw text is not valid Pango markup
/// on its own (#1224).
#[must_use]
pub fn failure_description(host: &str, tried: Option<&str>) -> String {
    glib::markup_escape_text(&failure_message(host, tried)).to_string()
}

/// The host part of an `http(s)` URL — no port, no userinfo, IPv6 literal
/// **kept in its brackets**, which is the form a URL spells it in and the form
/// to show a human.
///
/// It is **not** the form to hand any API: `GNetworkAddress` and
/// `WebKit`'s per-host certificate exception both want the bare literal
/// ([`identity_host`]), and `g_socket_client_connect_to_host` wants the
/// bracketed one *with* the port ([`crate::verify::connect_target`]). This
/// function is the single source all three derive from; none of them takes its
/// output unchanged.
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

/// The same host **without** its brackets — what `GNetworkAddress` wants, and
/// what `WebKit`'s per-host certificate exception wants **too**.
///
/// # Both, not one each (#1242 review, finding 3)
///
/// This function's docs used to say `WebKit` keyed that exception on the
/// *bracketed* form. It does not, and the mistake was load-bearing: the pin
/// was stored under `[::1]` and looked up as `::1`, so an IPv6 hive got a dead
/// pin on every route and nothing anywhere reported it.
/// `SoupNetworkSession.cpp:314-327` (webkitgtk 2.52.6) carries a comment
/// written for exactly this error —
///
/// ```text
/// // If the host component of the URL is an IPv6 address, it will be
/// // surrounded by [ ] brackets. We have to remove them because they're part
/// // of the WTF::URL's host component … but not part of the host passed to
/// // allowSpecificHTTPSCertificateForHost.
/// ```
///
/// — while `allowSpecificHTTPSCertificateForHost` (`:341-344`) stores under
/// whatever string it is handed, verbatim. So this spelling is the one to
/// store; see [`crate::webview`]'s `pin_for`, which is where that happens.
///
/// The bracketed [`host_of`] spelling is for the **card**, where a human reads
/// it; [`crate::verify::connect_target`] is the third form.
#[must_use]
pub fn identity_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// The port to connect to when probing `url` — explicit if it has one, else
/// the scheme's default.
///
/// Only the probe uses this; `WebKit`'s own exception is per **host**, not per
/// host-and-port, which is why [`host_of`] drops it.
#[must_use]
pub fn port_of(url: &str) -> u16 {
    const HTTPS: u16 = 443;
    const HTTP: u16 = 80;
    let trimmed = url.trim();
    let default = if trimmed.starts_with("http://") {
        HTTP
    } else {
        HTTPS
    };
    let Some(rest) = trimmed.split_once("://").map(|(_, r)| r) else {
        return default;
    };
    let Some(authority) = rest.split(['/', '?', '#']).next() else {
        return default;
    };
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // `[::1]:8443` — the port is what follows the **bracket**, never a colon
    // inside the literal; `hive.local:8443` has no bracket, so it is what
    // follows the last colon.
    let port = match authority.rfind(']') {
        Some(end) => authority[end + 1..].strip_prefix(':'),
        None => authority.rsplit_once(':').map(|(_, p)| p),
    };
    port.and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p != 0)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use gtk::pango;

    use super::{
        CERT_ENV, Pinned, Resolved, TlsPolicy, failure_description, failure_message, host_of,
        identity_host, port_of, resolve_route,
    };
    use crate::verify::{CA_ENV, Route, Source, TLS_DIR_ENV};
    use std::path::PathBuf;

    /// A policy that pins the file at `pem` for `host`.
    fn pin(pem: &str, host: &str) -> TlsPolicy {
        TlsPolicy::AllowCertificateForHost {
            cert: Pinned::File(PathBuf::from(pem)),
            host: host.to_owned(),
        }
    }

    /// Route 4 changes nothing and has nothing to report.
    #[test]
    fn the_system_store_route_adds_no_policy_and_no_diagnosis() {
        assert_eq!(
            resolve_route(&Route::SystemStore, "https://hive.local/agent/stray/"),
            Resolved {
                policy: TlsPolicy::SystemStore,
                tried: None,
            }
        );
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
            pin("/tmp/hive-gateway.pem", "hive.local"),
            TlsPolicy::AllowCertificateForHost {
                cert: Pinned::VerifiedPem("-----BEGIN CERTIFICATE-----\n".to_owned()),
                host: "hive.local".to_owned(),
            },
        ] {
            assert_eq!(p.errors_policy(), webkit::TLSErrorsPolicy::Fail, "{p:?}");
        }
        assert_eq!(TlsPolicy::SystemStore.scoped_host(), None);
        assert_eq!(
            pin("/tmp/x.pem", "hive.local").scoped_host(),
            Some("hive.local")
        );
    }

    /// A route with no host to scope it to degrades to the system store rather
    /// than widening — and does **no** I/O on the way, which is what keeps
    /// this test hermetic.
    ///
    /// Mutation (re-run this round, red): fall back to a session-wide grant
    /// when the host cannot be read, and this reds.
    #[test]
    fn an_unscopable_route_degrades_to_the_system_store() {
        for url in ["", "not a url", "https://", "file:///tmp/x"] {
            assert_eq!(
                resolve_route(
                    &Route::PinLeaf {
                        pem: PathBuf::from("/tmp/cert.pem"),
                        source: Source::Env(CERT_ENV),
                    },
                    url
                ),
                Resolved {
                    policy: TlsPolicy::SystemStore,
                    tried: None,
                },
                "{url}"
            );
        }
    }

    /// **A CA bundle is not the certificate the gateway presents**, which is
    /// what `allow_tls_certificate_for_host` pins — so [`CERT_ENV`] must name
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
    /// That shape now exists, as
    /// `verify::tls_tests::gateway_pem_pins_the_leaf_and_a_ca_first_file_does_not`
    /// — #1234 put `glib-networking` in both the devShell and the
    /// `system-tests` closure, so `from_file` finally has a backend. This one
    /// stays as the **hermetic** half: it asserts the same fact one layer
    /// down, where it actually lives — **the first PEM block of the bundle is
    /// not the leaf** — and so keeps running in the default `cargo test`,
    /// which has no display and no GIO modules.
    ///
    /// # Only the first assertion is load-bearing
    ///
    /// `trust-bundle.pem` is a **byte-for-byte copy** of `hive-ca.pem` in
    /// these fixtures — the generator does `cp` — so the second assertion is
    /// true by construction and proves nothing on its own (#1130's
    /// re-verification said so, correctly). It is kept as a statement of what
    /// the bundle *is*: a real hyperhive bundle is the hive CA plus whatever
    /// it is rooted at, and the property that matters for [`CERT_ENV`] is only
    /// ever about the **first** block, which is the CA in both shapes. The
    /// first assertion is the one a change can break.
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

    /// The inline error state names the failing host and **every** route, and
    /// warns that the pin wants the leaf.
    ///
    /// Mutation (re-run this round, red): drop any route, or the host, and
    /// this reds. A TLS failure is the one moment the operator has no other
    /// source of instructions — the window has no address bar and `WebKit`'s
    /// own error page says only "load failed".
    #[test]
    fn the_failure_state_names_the_host_and_every_route() {
        let m = failure_message("hive.local", None);
        assert!(m.contains("hive.local"), "{m}");
        assert!(m.contains(TLS_DIR_ENV), "{m}");
        assert!(m.contains(CA_ENV), "{m}");
        assert!(m.contains("security.pki.certificateFiles"), "{m}");
        assert!(m.contains("trust-bundle.pem"), "{m}");
        assert!(m.contains("gateway.pem"), "{m}");
        assert!(m.contains(CERT_ENV), "{m}");
        assert!(m.contains("openssl s_client"), "{m}");
        assert!(
            m.contains("PRESENTS"),
            "the leaf-not-bundle warning is the point of the last route: {m}"
        );
    }

    /// **#1234 finding 1**: the machine-wide route the card used to give does
    /// not build. `security.pki.certificateFiles` splices its argument into
    /// `cacert`'s derivation, which opens it **inside the build sandbox**;
    /// `/var/lib/hive-tls` is not in there, so the reader's `nixos-rebuild`
    /// failed at build — and with the sandbox off it pinned the bytes at first
    /// build and never noticed a CA rotation. hyperhive's own agents copy the
    /// bundle into the flake's git tree (`hive_c0re::meta`), which is what
    /// this card now says.
    ///
    /// Mutation (re-run this round, red): put the `"${…stateDir}/…"` form back
    /// and this reds on the `starts_with`.
    #[test]
    fn the_machine_wide_route_references_a_copy_in_the_flake_not_a_runtime_path() {
        let m = failure_message("hive.local", None);
        let at = m
            .find("security.pki.certificateFiles")
            .expect("the machine-wide route is named");
        assert!(
            m[at..].starts_with("security.pki.certificateFiles = [ ./hive-ca.pem ];"),
            "the only certificateFiles form that builds names a path inside the flake: {}",
            &m[at..]
        );
        let cp = m.find("cp /var/lib/hive-tls/trust-bundle.pem ./hive-ca.pem");
        assert!(
            cp.is_some_and(|c| c < at),
            "…and the copy that produces it comes first: {m}"
        );
    }

    /// The **zero-setup** route leads (Mara's ask on #1224: no manual setup),
    /// and the per-window pin is last.
    ///
    /// Mutation (re-run this round, red): move the `CERT_ENV` bullet to the
    /// top — the shape the card had before #1234, when it was the only
    /// automatic route there was — and the ordering assertions red.
    #[test]
    fn the_failure_state_ranks_zero_setup_first_and_the_pin_last() {
        let m = failure_message("hive.local", None);
        let same_host = m
            .find("config.services.hyperhive.deploy.hive-controller.tls.stateDir")
            .expect("the same-host route names hyperhive's option");
        let remote = m.find(CA_ENV).expect("…then the remote-hive bundle");
        let machine_wide = m
            .find("security.pki.certificateFiles")
            .expect("…then the machine-wide copy");
        let pin_route = m.find(CERT_ENV).expect("…and the per-window pin is named");

        assert!(
            same_host < remote,
            "nothing-to-set comes before copy-a-bundle: {m}"
        );
        assert!(
            remote < machine_wide,
            "the window's own route comes before changing the machine's trust store: {m}"
        );
        assert!(
            machine_wide < pin_route,
            "and every trust route comes before the per-window pin: {m}"
        );
    }

    /// The card says **which file this launch tried and what happened to it**
    /// — the half #1234 added, and the difference between "it did not work"
    /// and "the anchors in /var/lib/hive-tls/trust-bundle.pem refused it
    /// because nothing in them signs that chain".
    ///
    /// Mutation (re-run this round, red): drop the `tried` interpolation from
    /// [`failure_message`] and this reds while every other message test stays
    /// green.
    #[test]
    fn the_failure_state_repeats_what_this_launch_actually_tried() {
        let without = failure_message("hive.local", None);
        let with = failure_message(
            "hive.local",
            Some("checked hive.local's certificate against the anchors in /var/lib/x.pem"),
        );
        assert!(with.contains("/var/lib/x.pem"), "{with}");
        assert!(with.len() > without.len());
        assert!(
            !without.contains("This launch"),
            "nothing tried, nothing to report: {without}"
        );
    }

    /// The bug (#1224): `AdwStatusPage:description` is parsed as Pango
    /// markup, and [`failure_message`]'s raw text is not valid markup — its
    /// `openssl … </dev/null` reads as an unopened closing tag. This pins the
    /// mechanism, not just the symptom, so nobody "simplifies" the escape
    /// away at the sink: if a future edit removes the `</dev/null` fragment
    /// and the raw message happens to become valid markup on its own, THIS
    /// test's failure is the signal to delete it, not to reach for a `<` to
    /// keep it red.
    #[test]
    fn the_raw_failure_message_is_not_valid_markup_which_is_why_the_sink_escapes_it() {
        assert!(
            pango::parse_markup(&failure_message("hive.local", None), '\0').is_err(),
            "if this now parses, the escape at the sink is no longer covering a real bug — see \
             this test's doc before deleting it"
        );
    }

    /// The escaped form the sink actually uses is valid Pango markup, and
    /// round-trips back to exactly the text [`failure_message`] produced —
    /// so escaping did not lose or corrupt any of the instructions, including
    /// the `tried` sentence a hostile-looking path could hide in.
    #[test]
    fn the_failure_description_is_valid_pango_markup_and_round_trips() {
        for tried in [None, Some("read </dev/null & <b>/tmp/x.pem</b>")] {
            let (_, text, _) = pango::parse_markup(&failure_description("hive.local", tried), '\0')
                .expect("escaped text must be valid markup");
            assert_eq!(text, failure_message("hive.local", tried));
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

    /// The probe connects to the port the URL names, or the scheme's default —
    /// and the URL's bracketed IPv6 host becomes the bare literal both
    /// `GNetworkAddress` and `WebKit`'s pin lookup want.
    ///
    /// Mutation (re-run this round, red): take the **first** colon in the
    /// authority as the port separator and the `[::1]` rows red with a parse
    /// failure that silently becomes 443.
    #[test]
    fn the_probe_port_follows_the_url_and_the_scheme() {
        assert_eq!(port_of("https://hive.local/agent/x/"), 443);
        assert_eq!(port_of("http://hive.local/agent/x/"), 80);
        assert_eq!(port_of("https://hive.local:8443/x"), 8443);
        assert_eq!(port_of("http://hive.local:8080/x"), 8080);
        assert_eq!(port_of("https://user:pw@hive.local:8443/x"), 8443);
        assert_eq!(port_of("https://[::1]:8443/x"), 8443);
        assert_eq!(port_of("https://[::1]/x"), 443);
        assert_eq!(
            port_of("https://hive.local:0/x"),
            443,
            "port 0 is not a port"
        );
        assert_eq!(port_of("https://hive.local:nope/x"), 443);
        assert_eq!(port_of("nonsense"), 443);

        assert_eq!(identity_host("[::1]"), "::1");
        assert_eq!(identity_host("hive.local"), "hive.local");
    }
}
