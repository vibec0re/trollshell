//! Where the window looks for the hive's own TLS material, and what it does
//! with what it finds — the launch-time half of [`crate::tls`].
//!
//! # The problem this module exists for
//!
//! A hyperhive gateway serves a **self-signed leaf** under a host-held hive CA
//! (`services.hyperhive.deploy.hive-controller.tls`, `/var/lib/hive-tls` by
//! default). `WebKitGTK` 6 has exactly two trust knobs — the session-wide errors
//! policy, and `allow_tls_certificate_for_host`, which pins **one
//! certificate**, compared against the leaf the server presents. There is no
//! `GTlsDatabase` seam on `webkit6` 0.6 (#1234, verified: zero hits). So
//! "trust the hive's `trust-bundle.pem`" **cannot be one call** — a CA handed
//! to that pin never matches what the gateway sends, which
//! `tls::tests::a_bundle_is_not_the_certificate_the_gateway_presents` has
//! pinned since #1130.
//!
//! It can be two calls, and that is what this module does: verify the chain
//! the gateway presents against the bundle **with GIO**, then pin the leaf
//! that verification accepted. Per launch, so a CA rotation is picked up by
//! reopening the window; no change to the machine's trust store; the session's
//! errors policy still `Fail` on every arm.
//!
//! # The precedence, and why it is this way round
//!
//! Mara, on #1224 (2026-09-13 12:12Z): *"i dont want to run some cmd for
//! manual setup tho."* @the-sword-above, two minutes later: hyperhive's own
//! TLS module already writes `${stateDir}/gateway.pem` as
//! `cat leaf-only ca.pem` — **leaf first, CA appended**, `0644`, the file nginx
//! serves verbatim, re-signed weekly. Both files are on disk on a same-host
//! deploy, so the zero-setup default is to read them.
//!
//! | | route | what it needs |
//! | --- | --- | --- |
//! | 1 | [`CERT_ENV`](crate::tls::CERT_ENV) → pin that leaf | an operator who extracted one |
//! | 2 | [`CA_ENV`], else `<dir>/trust-bundle.pem` → **verify, then pin** | anchors |
//! | 3 | `<dir>/gateway.pem` → pin its first block | hyperhive on this machine |
//! | 4 | the machine's trust store | — |
//!
//! `<dir>` is [`TLS_DIR_ENV`] when set and [`DEFAULT_TLS_DIR`] otherwise; the
//! nix module renders it from `tls.stateDir` when hyperhive is enabled on the
//! same host, which is what makes routes 2 and 3 need **no setting at all**
//! there.
//!
//! Route 2 outranks route 3 deliberately: it survives the hive's weekly
//! re-sign without re-reading anything from disk, because what it pins is what
//! the gateway *just presented*, checked against an anchor that does not
//! rotate with the leaf. Route 3 pins a file, so a `gateway.pem` that has gone
//! stale under an already-open window simply fails to match — the pin is per
//! launch, so reopening fixes it, and the card names the path it tried.
//!
//! An **explicitly set** env variable always takes its route, readable or not:
//! a variable someone typed is a statement of intent, and falling through it
//! silently is how an operator ends up debugging the wrong file. The
//! well-known paths are the ones that must be readable to be taken.
//!
//! # What it is not
//!
//! Nothing here widens anything. Route 2's verification can only ever conclude
//! "pin this one leaf" or "pin nothing". Route 3 reads a world-readable file
//! the hive maintains. And every arm ends at [`crate::tls::TlsPolicy`], whose
//! [`errors_policy`](crate::tls::TlsPolicy::errors_policy) returns `Fail`
//! unconditionally.

use std::path::{Path, PathBuf};

use gtk::gio;
use gtk::gio::prelude::*;

/// Points the window at a **PEM bundle of trust anchors** to verify the
/// gateway's chain against — hyperhive's `trust-bundle.pem`, or a copy of it.
///
/// Unlike [`CERT_ENV`](crate::tls::CERT_ENV) this *is* the file a trust store
/// wants: the hive CA plus whatever it is rooted at. The window never hands it
/// to `allow_tls_certificate_for_host` (that could never match); it verifies
/// the presented chain against it and pins what came back.
pub const CA_ENV: &str = "TROLLSHELL_AGENT_WINDOW_CA";

/// Points the window at hyperhive's TLS **state directory**, from which both
/// well-known filenames are derived ([`BUNDLE_NAME`], [`GATEWAY_NAME`]).
///
/// This is the one the nix module renders, from
/// `config.services.hyperhive.deploy.hive-controller.tls.stateDir`, so that a
/// non-default `stateDir` moves both defaults at once and an operator on a
/// same-host deploy sets nothing.
pub const TLS_DIR_ENV: &str = "TROLLSHELL_AGENT_WINDOW_TLS_DIR";

/// hyperhive's own default for that directory
/// (`services.hyperhive.deploy.hive-controller.tls.stateDir`).
pub const DEFAULT_TLS_DIR: &str = "/var/lib/hive-tls";

/// The anchors file inside the TLS directory — the hive CA **plus what it is
/// rooted at**, which is what a verifier needs and what `ca.pem` alone is not.
pub const BUNDLE_NAME: &str = "trust-bundle.pem";

/// The file nginx serves verbatim inside the TLS directory: `0644`, **leaf
/// first with the CA appended**, re-signed weekly by `hive-tls-ca.service`.
pub const GATEWAY_NAME: &str = "gateway.pem";

/// How long the route-2 probe may spend reaching the gateway, in seconds.
///
/// It is a blocking handshake on the GTK main thread, so the bound is what
/// keeps a gateway that is down from freezing the window. It is affordable
/// because of *when* it runs: the window only resolves a policy after
/// `host.sock` has answered with this agent's URL, so the hive daemon is up
/// and the connection is normally a loopback handshake measured in
/// milliseconds. The timeout covers the one case left — the daemon up and the
/// gateway not.
pub const PROBE_TIMEOUT_SECS: u32 = 5;

/// Where a route's file came from, for the sentence the card shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// An environment variable named it, by name.
    Env(&'static str),
    /// It was found at a well-known name under the hive's TLS directory.
    HiveDir,
}

impl Source {
    /// How to say it in the diagnosis line.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Env(name) => format!("named by {name}"),
            Self::HiveDir => "found where hyperhive keeps it".to_owned(),
        }
    }
}

/// Which of the four routes this launch takes, and with which file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// Routes 1 and 3: pin the **first certificate** in this PEM, as
    /// `gio::TlsCertificate::from_file` reads it (everything after the first
    /// block becomes its issuer chain — which is exactly `gateway.pem`'s
    /// shape).
    PinLeaf {
        /// The PEM whose first block is the certificate to pin.
        pem: PathBuf,
        /// Which of the two got us here.
        source: Source,
    },
    /// Route 2: open a TLS connection to the gateway, verify the chain it
    /// presents against these anchors, and pin the leaf if they accept it.
    VerifyAgainstBundle {
        /// The anchors file.
        bundle: PathBuf,
        /// Which of the two got us here.
        source: Source,
    },
    /// Route 4: nothing to add — the machine's trust store decides, like any
    /// other client.
    SystemStore,
}

/// The three environment variables, read once so [`route`] stays pure.
#[derive(Clone, Copy, Debug, Default)]
pub struct Env<'a> {
    /// [`crate::tls::CERT_ENV`] — an explicit leaf to pin.
    pub cert: Option<&'a str>,
    /// [`CA_ENV`] — an explicit anchors bundle.
    pub ca: Option<&'a str>,
    /// [`TLS_DIR_ENV`] — hyperhive's TLS state directory.
    pub tls_dir: Option<&'a str>,
}

/// The three variables as `std::env::var` hands them over.
///
/// A named type rather than a tuple so the borrowing step ([`Env::of`]) cannot
/// silently swap two `Option<String>`s of identical type.
#[derive(Clone, Debug, Default)]
pub struct EnvOwned {
    /// [`crate::tls::CERT_ENV`].
    pub cert: Option<String>,
    /// [`CA_ENV`].
    pub ca: Option<String>,
    /// [`TLS_DIR_ENV`].
    pub tls_dir: Option<String>,
}

impl EnvOwned {
    /// Read the three from the process environment.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            cert: std::env::var(crate::tls::CERT_ENV).ok(),
            ca: std::env::var(CA_ENV).ok(),
            tls_dir: std::env::var(TLS_DIR_ENV).ok(),
        }
    }
}

impl<'a> Env<'a> {
    /// Borrow what [`EnvOwned::from_process`] read.
    #[must_use]
    pub fn of(owned: &'a EnvOwned) -> Self {
        Self {
            cert: owned.cert.as_deref(),
            ca: owned.ca.as_deref(),
            tls_dir: owned.tls_dir.as_deref(),
        }
    }
}

/// A value that is set to something other than whitespace.
fn stated(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// The hive's TLS directory for this launch — [`TLS_DIR_ENV`] when stated,
/// [`DEFAULT_TLS_DIR`] otherwise.
#[must_use]
pub fn tls_dir(env: &Env<'_>) -> PathBuf {
    PathBuf::from(stated(env.tls_dir).unwrap_or(DEFAULT_TLS_DIR))
}

/// Decide the route, given the environment and a predicate saying which
/// well-known files can be read.
///
/// Pure: `readable` is a parameter precisely so the precedence is testable
/// without a `/var/lib/hive-tls` — see the tests below, which walk all four
/// routes and the two "explicit env wins even when unreadable" cases.
#[must_use]
pub fn route(env: &Env<'_>, readable: &dyn Fn(&Path) -> bool) -> Route {
    if let Some(pem) = stated(env.cert) {
        return Route::PinLeaf {
            pem: PathBuf::from(pem),
            source: Source::Env(crate::tls::CERT_ENV),
        };
    }
    if let Some(bundle) = stated(env.ca) {
        return Route::VerifyAgainstBundle {
            bundle: PathBuf::from(bundle),
            source: Source::Env(CA_ENV),
        };
    }
    let dir = tls_dir(env);
    let bundle = dir.join(BUNDLE_NAME);
    if readable(&bundle) {
        return Route::VerifyAgainstBundle {
            bundle,
            source: Source::HiveDir,
        };
    }
    let gateway = dir.join(GATEWAY_NAME);
    if readable(&gateway) {
        return Route::PinLeaf {
            pem: gateway,
            source: Source::HiveDir,
        };
    }
    Route::SystemStore
}

/// Whether a path names a file this process can open for reading.
///
/// Deliberately an *open*, not `Path::exists`: `trust-bundle.pem` is
/// root-owned `0644` in a `0755` directory, and the failure mode worth
/// distinguishing is "there but not ours to read", which `exists` would call a
/// hit and then fail later with no route left.
#[must_use]
pub fn is_readable(path: &Path) -> bool {
    std::fs::File::open(path).is_ok()
}

/// What the route-2 probe concluded.
#[derive(Clone, Debug)]
pub enum Presented {
    /// The anchors accepted the chain the gateway presented. The payload is
    /// the **leaf's** PEM, which is what gets pinned.
    Trusted(String),
    /// The gateway answered and the anchors refused what it sent.
    Refused {
        /// Which checks failed — [`describe_flags`] turns it into words.
        flags: gio::TlsCertificateFlags,
    },
    /// No chain was ever judged: no route to the host, no TLS on the port, the
    /// timeout, or an anchors file that could not be opened as a database.
    ///
    /// **This is not a trust failure** and the card says so — it is the
    /// difference between "the hive's certificate is wrong" and "the hive's
    /// gateway did not answer".
    Unreachable(String),
}

/// Open a TLS connection to `host:port`, verify the chain it presents against
/// `bundle`, and hand back the leaf if the anchors accept it.
///
/// # Why a connection at all
///
/// `allow_tls_certificate_for_host` needs the **exact certificate the server
/// presents**. Nothing on `host.sock` carries it (`HiveUrls` is `domain` /
/// `home` / `forge` / `matrix`, #948), and the bundle by construction is not
/// it. So the leaf has to come off the wire.
///
/// # How the verdict is reached
///
/// The connection's own `GTlsDatabase` is replaced with a
/// [`gio::TlsFileDatabase`] over `bundle`, so the verification GIO performs
/// during the handshake *is* the verification we want — including the identity
/// check against `host`, which a bare `TlsCertificateExt::verify` against a
/// single anchor would not cover for a chain.
///
/// `accept-certificate` returns **`false`** on every call. It is there to
/// capture the flags, never to overrule them: this function has no way to ask
/// a human, and a probe that could say yes to a certificate the anchors
/// refused would be trust on first use with the human taken out.
///
/// The connection is closed either way; nothing is sent over it and nothing is
/// read from it.
#[must_use]
pub fn probe(bundle: &Path, host: &str, port: u16, timeout_secs: u32) -> Presented {
    let database = match gio::TlsFileDatabase::new(bundle) {
        Ok(db) => db,
        Err(e) => {
            return Presented::Unreachable(format!(
                "the anchors in {} could not be loaded ({e})",
                bundle.display()
            ));
        }
    };

    let client = gio::SocketClient::new();
    // Covers the connect *and* the handshake reads: `g_socket_client_set_timeout`
    // sets the I/O timeout on the sockets it creates, which the TLS connection
    // then wraps.
    client.set_timeout(timeout_secs);
    let connection = match client.connect_to_host(
        &connect_target(host, port),
        port,
        None::<&gio::Cancellable>,
    ) {
        Ok(c) => c,
        Err(e) => {
            return Presented::Unreachable(format!("could not reach {host}:{port} ({e})"));
        }
    };

    let identity = gio::NetworkAddress::new(host, port);
    let tls = match gio::TlsClientConnection::new(&connection, Some(&identity)) {
        Ok(t) => t,
        Err(e) => {
            return Presented::Unreachable(format!("no TLS backend to check {host} with ({e})"));
        }
    };
    tls.set_database(Some(&database));
    // Nothing is written on this connection, so a peer that drops without a
    // close_notify is not truncating anything.
    tls.set_require_close_notify(false);

    let refused: std::rc::Rc<std::cell::Cell<Option<gio::TlsCertificateFlags>>> =
        std::rc::Rc::new(std::cell::Cell::new(None));
    let captured = std::rc::Rc::clone(&refused);
    tls.connect_accept_certificate(move |_, _cert, errors| {
        captured.set(Some(errors));
        // Never yes. See this function's docs.
        false
    });

    let verdict = match tls.handshake(None::<&gio::Cancellable>) {
        Ok(()) => match tls.peer_certificate() {
            Some(leaf) => match leaf.certificate_pem() {
                Some(pem) => Presented::Trusted(pem.to_string()),
                None => Presented::Unreachable(format!(
                    "{host} was verified but its certificate could not be re-encoded"
                )),
            },
            None => {
                Presented::Unreachable(format!("{host} completed a handshake with no certificate"))
            }
        },
        Err(e) => match refused.get() {
            Some(flags) => Presented::Refused { flags },
            None => Presented::Unreachable(format!("the handshake with {host} failed ({e})")),
        },
    };

    // Best effort: the verdict is already in hand, and a close error on a
    // connection nothing was written to says nothing worth reporting.
    drop(tls.close(None::<&gio::Cancellable>));
    verdict
}

/// The `host:port` string `g_socket_client_connect_to_host` parses, with an
/// **IPv6 literal put back in its brackets**.
///
/// [`crate::tls::host_of`] hands `WebKit`'s bracketed spelling, and
/// [`crate::tls::identity_host`] strips the brackets for `GNetworkAddress`,
/// which wants a bare hostname. This is the third spelling: `connect_to_host`
/// parses a *host-and-port*, so `::1:8443` is ambiguous and
/// `g_network_address_parse` resolves it wrong — the brackets are what make
/// the last colon the port separator.
///
/// Only the probe needs it, and only an IPv6 hive would ever exercise it —
/// which is exactly why it is a named function with a test rather than a
/// `format!` at the call site nobody would ever run.
#[must_use]
pub fn connect_target(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Name the checks a set of [`gio::TlsCertificateFlags`] failed, in words plus
/// the GIO spelling.
///
/// The card shows this, and it is the whole point of verifying at launch
/// rather than letting `WebKit` fail with "load failed": the operator learns
/// *which* thing is wrong — an anchor that does not sign this chain reads very
/// differently from a leaf that expired last week.
#[must_use]
pub fn describe_flags(flags: gio::TlsCertificateFlags) -> String {
    let mut named: Vec<&str> = Vec::new();
    if flags.contains(gio::TlsCertificateFlags::UNKNOWN_CA) {
        named.push("nothing in that file signs the chain it presented (UNKNOWN_CA)");
    }
    if flags.contains(gio::TlsCertificateFlags::BAD_IDENTITY) {
        named.push("the certificate does not name this host (BAD_IDENTITY)");
    }
    if flags.contains(gio::TlsCertificateFlags::NOT_ACTIVATED) {
        named.push("the certificate is not valid yet (NOT_ACTIVATED)");
    }
    if flags.contains(gio::TlsCertificateFlags::EXPIRED) {
        named.push("the certificate has expired (EXPIRED)");
    }
    if flags.contains(gio::TlsCertificateFlags::REVOKED) {
        named.push("the certificate was revoked (REVOKED)");
    }
    if flags.contains(gio::TlsCertificateFlags::INSECURE) {
        named.push("the certificate's algorithm is not considered secure (INSECURE)");
    }
    if flags.contains(gio::TlsCertificateFlags::GENERIC_ERROR) {
        named.push("verification failed for a reason GIO did not name (GENERIC_ERROR)");
    }
    if named.is_empty() {
        // Reachable only if GIO grows a flag this build has no arm for, which
        // is exactly when saying the raw value beats saying nothing.
        return format!("verification failed ({flags:?})");
    }
    named.join("; ")
}

#[cfg(test)]
mod tests {
    use super::{
        BUNDLE_NAME, CA_ENV, DEFAULT_TLS_DIR, Env, GATEWAY_NAME, Route, Source, describe_flags,
        route, tls_dir,
    };
    use crate::tls::CERT_ENV;
    use gtk::gio;
    use std::path::{Path, PathBuf};

    /// A `readable` predicate that says yes to exactly the paths listed.
    fn only(paths: &[&str]) -> impl Fn(&Path) -> bool + use<> {
        let owned: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |p: &Path| owned.iter().any(|o| o == p)
    }

    /// Nothing set and nothing on disk: the machine's trust store, unchanged.
    #[test]
    fn with_no_material_at_all_the_system_store_decides() {
        assert_eq!(
            route(&Env::default(), &only(&[])),
            Route::SystemStore,
            "a window on a machine with no hive must change nothing about trust"
        );
    }

    /// The zero-setup same-host case (Mara, #1224 12:12Z): **both** files are
    /// there and the bundle wins, because verifying-then-pinning survives the
    /// hive's weekly re-sign and a pinned `gateway.pem` does not.
    ///
    /// Mutation (re-run this round, red): swap the two `readable` arms in
    /// `route` so `gateway.pem` is checked first, and this reds.
    #[test]
    fn on_a_same_host_deploy_the_bundle_outranks_the_gateway_file() {
        let bundle = format!("{DEFAULT_TLS_DIR}/{BUNDLE_NAME}");
        let gateway = format!("{DEFAULT_TLS_DIR}/{GATEWAY_NAME}");
        assert_eq!(
            route(&Env::default(), &only(&[&bundle, &gateway])),
            Route::VerifyAgainstBundle {
                bundle: PathBuf::from(&bundle),
                source: Source::HiveDir,
            }
        );
    }

    /// …and with only `gateway.pem` there — a deploy that does not publish the
    /// bundle — the leaf on disk is pinned, still with no setting.
    #[test]
    fn gateway_pem_alone_is_enough_to_pin_without_a_probe() {
        let gateway = format!("{DEFAULT_TLS_DIR}/{GATEWAY_NAME}");
        assert_eq!(
            route(&Env::default(), &only(&[&gateway])),
            Route::PinLeaf {
                pem: PathBuf::from(&gateway),
                source: Source::HiveDir,
            }
        );
    }

    /// [`TLS_DIR_ENV`](super::TLS_DIR_ENV) moves **both** well-known names at
    /// once — the property that lets the nix module render one value from
    /// `tls.stateDir` instead of two file paths that could drift.
    ///
    /// Mutation (re-run this round, red): derive one of the two from
    /// `DEFAULT_TLS_DIR` instead of from `tls_dir`, and one of these reds.
    #[test]
    fn the_tls_dir_moves_both_well_known_names() {
        let dir = "/srv/hive-tls";
        let env = Env {
            tls_dir: Some(dir),
            ..Env::default()
        };
        assert_eq!(tls_dir(&env), PathBuf::from(dir));
        assert_eq!(
            route(&env, &only(&[&format!("{dir}/{BUNDLE_NAME}")])),
            Route::VerifyAgainstBundle {
                bundle: PathBuf::from(format!("{dir}/{BUNDLE_NAME}")),
                source: Source::HiveDir,
            }
        );
        assert_eq!(
            route(&env, &only(&[&format!("{dir}/{GATEWAY_NAME}")])),
            Route::PinLeaf {
                pem: PathBuf::from(format!("{dir}/{GATEWAY_NAME}")),
                source: Source::HiveDir,
            }
        );
        // …and the *default* directory's files are no longer consulted.
        assert_eq!(
            route(&env, &only(&[&format!("{DEFAULT_TLS_DIR}/{BUNDLE_NAME}")])),
            Route::SystemStore,
            "a stated directory replaces the default rather than adding to it"
        );
    }

    /// The full precedence, in one table: the explicit leaf beats the explicit
    /// bundle beats the well-known bundle beats the well-known leaf.
    ///
    /// Mutation (re-run this round, red): move the `CERT_ENV` arm below the
    /// `CA_ENV` arm and the first row reds.
    #[test]
    fn the_four_routes_rank_in_the_documented_order() {
        let bundle = format!("{DEFAULT_TLS_DIR}/{BUNDLE_NAME}");
        let gateway = format!("{DEFAULT_TLS_DIR}/{GATEWAY_NAME}");
        let everything = only(&[&bundle, &gateway]);

        assert_eq!(
            route(
                &Env {
                    cert: Some("/tmp/leaf.pem"),
                    ca: Some("/tmp/anchors.pem"),
                    tls_dir: None,
                },
                &everything
            ),
            Route::PinLeaf {
                pem: PathBuf::from("/tmp/leaf.pem"),
                source: Source::Env(CERT_ENV),
            },
            "an explicit leaf is the operator's last resort and outranks everything"
        );
        assert_eq!(
            route(
                &Env {
                    ca: Some("/tmp/anchors.pem"),
                    ..Env::default()
                },
                &everything
            ),
            Route::VerifyAgainstBundle {
                bundle: PathBuf::from("/tmp/anchors.pem"),
                source: Source::Env(CA_ENV),
            }
        );
    }

    /// An env variable someone **typed** takes its route even when the file is
    /// not readable, rather than falling through to a well-known path.
    ///
    /// Falling through would be the worse failure: the card would name a file
    /// the operator never mentioned while the one they did name sat there
    /// misspelled.
    ///
    /// Mutation (re-run this round, red): gate the two env arms on `readable`
    /// and both assertions here red.
    #[test]
    fn a_stated_variable_keeps_its_route_even_when_the_file_is_missing() {
        let gateway = format!("{DEFAULT_TLS_DIR}/{GATEWAY_NAME}");
        let disk = only(&[&gateway]);
        assert!(matches!(
            route(
                &Env {
                    ca: Some("/tmp/typo.pem"),
                    ..Env::default()
                },
                &disk
            ),
            Route::VerifyAgainstBundle {
                source: Source::Env(CA_ENV),
                ..
            }
        ));
        assert!(matches!(
            route(
                &Env {
                    cert: Some("/tmp/typo.pem"),
                    ..Env::default()
                },
                &disk
            ),
            Route::PinLeaf {
                source: Source::Env(CERT_ENV),
                ..
            }
        ));
    }

    /// A blank or whitespace-only variable is not a statement of intent.
    #[test]
    fn a_blank_variable_is_not_set() {
        for blank in ["", "   ", "\t"] {
            assert_eq!(
                route(
                    &Env {
                        cert: Some(blank),
                        ca: Some(blank),
                        tls_dir: Some(blank),
                    },
                    &only(&[])
                ),
                Route::SystemStore,
                "{blank:?}"
            );
        }
    }

    /// Every flag GIO can raise gets words, and a combination names all of
    /// them — the card's whole value over `WebKit`'s "load failed".
    ///
    /// Mutation (re-run this round, red): drop the `BAD_IDENTITY` arm and the
    /// combined assertion reds.
    #[test]
    fn every_failed_check_is_named_in_words_and_in_gios_spelling() {
        for (flag, word) in [
            (gio::TlsCertificateFlags::UNKNOWN_CA, "UNKNOWN_CA"),
            (gio::TlsCertificateFlags::BAD_IDENTITY, "BAD_IDENTITY"),
            (gio::TlsCertificateFlags::NOT_ACTIVATED, "NOT_ACTIVATED"),
            (gio::TlsCertificateFlags::EXPIRED, "EXPIRED"),
            (gio::TlsCertificateFlags::REVOKED, "REVOKED"),
            (gio::TlsCertificateFlags::INSECURE, "INSECURE"),
            (gio::TlsCertificateFlags::GENERIC_ERROR, "GENERIC_ERROR"),
        ] {
            let described = describe_flags(flag);
            assert!(described.contains(word), "{word} unnamed: {described}");
            assert!(
                described.len() > word.len() + 2,
                "{word} needs words around it, not just the constant: {described}"
            );
        }
        let both = describe_flags(
            gio::TlsCertificateFlags::UNKNOWN_CA | gio::TlsCertificateFlags::BAD_IDENTITY,
        );
        assert!(
            both.contains("UNKNOWN_CA") && both.contains("BAD_IDENTITY"),
            "{both}"
        );
    }

    /// An IPv6 hive is reachable: the literal goes back in its brackets before
    /// `connect_to_host` parses it, because that function takes a
    /// *host-and-port* and `::1:8443` has no unambiguous port separator.
    ///
    /// Mutation (re-run this round, red): make [`connect_target`] a plain
    /// `format!("{host}:{port}")` — the spelling this replaced — and the two
    /// literal rows red.
    ///
    /// [`connect_target`]: super::connect_target
    #[test]
    fn an_ipv6_literal_is_bracketed_before_it_is_parsed_as_a_host_and_port() {
        use super::connect_target;
        assert_eq!(connect_target("::1", 8443), "[::1]:8443");
        assert_eq!(connect_target("fd00::1", 443), "[fd00::1]:443");
        assert_eq!(connect_target("hive.local", 8443), "hive.local:8443");
        assert_eq!(connect_target("127.0.0.1", 443), "127.0.0.1:443");
    }

    /// An empty flag set never reaches [`describe_flags`] in practice, and if a
    /// future GIO raises one this build has no arm for, the raw value is still
    /// better than an empty sentence.
    #[test]
    fn an_unnamed_flag_still_says_something() {
        let described = describe_flags(gio::TlsCertificateFlags::empty());
        assert!(!described.is_empty());
        assert!(described.contains("verification failed"), "{described}");
    }
}

#[cfg(all(test, feature = "system-tests"))]
mod tls_tests {
    //! The launch-time verify, run for real against a **local TLS server**.
    //!
    //! # Why these are gated, and what #1234 had to change to make them run
    //!
    //! Until #1234 the note at the top of `tls.rs`'s
    //! `a_bundle_is_not_the_certificate_the_gateway_presents` recorded a
    //! measurement: `gio::TlsCertificate::from_file` answers **"TLS support is
    //! not available"** in this repo's devShell and in `checks.system-tests`,
    //! because neither closure carried `glib-networking` — GIO's TLS backend
    //! is a loadable module, not part of libgio. Every assertion about
    //! verification was therefore a live-verify item.
    //!
    //! Ask 3 of #1234 put `glib-networking` in both (`nix/devshell.nix`,
    //! `nix/checks/system-tests.nix`, through `GIO_EXTRA_MODULES` the way
    //! `nix/probe.nix` already injects dconf's `GSettings` backend), and this
    //! module is what that buys: a `GTlsServerConnection` on one end of a
    //! loopback socket, the window's own [`probe`] on the other, and the
    //! four verdicts asserted through the same code a launch runs.
    //!
    //! **No `WebView` is constructed here and no page is loaded.** That
    //! remains impossible under the check (`webview.rs`'s `gtk_tests` module
    //! doc has the measurement: the web process dies even with the sandbox
    //! disabled). What is testable is everything *before* `WebKit` — which is
    //! where all of #1234's logic lives.
    //!
    //! [`probe`]: super::probe

    use super::{Presented, Route, Source, probe};
    use crate::tls::{Pinned, Resolved, TlsPolicy, resolve_route};
    use gtk::gio;
    use gtk::gio::prelude::*;
    use std::path::PathBuf;
    use std::time::Duration;

    /// A PEM from `tests/fixtures/tls` — see that directory's README for what
    /// each one is and `generate.sh` for how they were minted.
    fn fixture(name: &str) -> PathBuf {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tls")
            .join(name);
        assert!(path.is_file(), "missing fixture {}", path.display());
        path
    }

    /// The anchors a launch would be pointed at: **the fixture CA alone**,
    /// which signs `server-leaf` and `expired-leaf` and does not sign
    /// `other-leaf`.
    fn anchors() -> PathBuf {
        fixture("fixture-ca.pem")
    }

    /// Bind a TLS server on an ephemeral loopback port presenting
    /// `cert`/`key`, and hand back the port.
    ///
    /// Every GIO object lives on the server thread — only the `u16` crosses —
    /// because none of them is `Send` and the crate forbids `unsafe`, so
    /// handing a listener over is not an option. `add_any_inet_port` is what
    /// makes that work: the port is chosen inside the thread and reported out.
    ///
    /// The handshake's result is deliberately dropped. The client refuses
    /// every certificate it is not happy with (see [`probe`]'s docs), so for
    /// three of the four cases below a server-side error **is** the expected
    /// outcome.
    fn serve(cert: &str, key: &str) -> u16 {
        let (cert, key) = (fixture(cert), fixture(key));
        let (tx, rx) = std::sync::mpsc::channel::<Result<u16, String>>();
        std::thread::spawn(move || {
            let identity = match gio::TlsCertificate::from_files(&cert, &key) {
                Ok(c) => c,
                Err(e) => {
                    drop(tx.send(Err(format!("{} + key: {e}", cert.display()))));
                    return;
                }
            };
            let listener = gio::SocketListener::new();
            let port = match listener.add_any_inet_port(None::<&gtk::glib::Object>) {
                Ok(p) => p,
                Err(e) => {
                    drop(tx.send(Err(format!("no loopback port: {e}"))));
                    return;
                }
            };
            if tx.send(Ok(port)).is_err() {
                return;
            }
            let Ok((connection, _)) = listener.accept(None::<&gio::Cancellable>) else {
                return;
            };
            let Ok(tls) = gio::TlsServerConnection::new(&connection, Some(&identity)) else {
                return;
            };
            tls.set_require_close_notify(false);
            drop(tls.handshake(None::<&gio::Cancellable>));
            drop(tls.close(None::<&gio::Cancellable>));
        });
        rx.recv_timeout(Duration::from_secs(30))
            .expect("the fixture server reported a port")
            .expect("the fixture server started")
    }

    /// A loopback port with nothing behind it: bound, read back, released.
    fn closed_port() -> u16 {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port to borrow");
        let port = listener.local_addr().expect("…with an address").port();
        drop(listener);
        port
    }

    /// **#1234 finding 2's note is now false, and that is the point of ask 3.**
    ///
    /// `gio::TlsCertificate::from_file` answered "TLS support is not
    /// available" in both the devShell and `checks.system-tests` before
    /// `glib-networking` was added to their closures; every trust assertion in
    /// this crate was live-verify because of it. If someone drops the
    /// `GIO_EXTRA_MODULES` line from either file, **this** is what goes red —
    /// with the reason in the message — rather than four inscrutable
    /// handshake failures below.
    #[test]
    fn gio_has_a_tls_backend_here_which_is_what_glib_networking_buys() {
        let bundle = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/trust-bundle.pem");
        gio::TlsCertificate::from_file(&bundle).unwrap_or_else(|e| {
            panic!(
                "no GIO TLS backend ({e}) — glib-networking must be in this closure with \
                 GIO_EXTRA_MODULES pointing at its gio/modules; see nix/devshell.nix and \
                 nix/checks/system-tests.nix (#1234 ask 3)"
            )
        });
        gio::TlsFileDatabase::new(anchors())
            .expect("…and an anchors file opens as a GTlsFileDatabase");
    }

    /// **Route 3, the zero-setup pin**: hyperhive's `gateway.pem` is
    /// `cat leaf-only ca.pem`, so its first block — the one
    /// `from_file` takes as *the* certificate — is the leaf the gateway
    /// presents, and pinning it is one call with no probe.
    ///
    /// The same two certificates the other way round is what a *trust bundle*
    /// is, and it pins the CA instead, which can never match. That is
    /// #1130's fact, now asserted in the shape its reviewer originally asked
    /// for (`is_same`) rather than by comparing PEM text.
    ///
    /// Mutation (re-run this round, red): swap the two `cat` operands in
    /// `tests/fixtures/tls/generate.sh` and regenerate — both halves red.
    #[test]
    fn gateway_pem_pins_the_leaf_and_a_ca_first_file_does_not() {
        let leaf = gio::TlsCertificate::from_file(fixture("server-leaf.pem"))
            .expect("the fixture leaf parses");
        let ca = gio::TlsCertificate::from_file(anchors()).expect("the fixture anchor parses");

        let gateway = gio::TlsCertificate::from_file(fixture("gateway.pem"))
            .expect("hyperhive's gateway.pem parses");
        assert!(
            gateway.is_same(&leaf),
            "gateway.pem leads with the LEAF, which is why route 3 can pin its first block"
        );
        assert!(!gateway.is_same(&ca));

        let wrong_way = gio::TlsCertificate::from_file(fixture("gateway-ca-first.pem"))
            .expect("a CA-first file parses too — that is the whole problem");
        assert!(
            wrong_way.is_same(&ca) && !wrong_way.is_same(&leaf),
            "a CA-first file pins the anchor, which is never what the server sends"
        );
    }

    /// **Route 2, the whole of ask 1**: the hive's anchors verify what the
    /// gateway presents, and what comes back is the **leaf** — the one value
    /// `allow_tls_certificate_for_host` can use.
    ///
    /// Mutation (re-run this round, red): point `anchors()` at
    /// `other-leaf.pem`, or make [`probe`] return the *database's* certificate
    /// instead of the peer's, and this reds.
    #[test]
    fn the_hives_anchors_verify_the_gateway_and_the_leaf_is_what_gets_pinned() {
        let port = serve("server-leaf.pem", "server-leaf-key.pem");
        let Presented::Trusted(pem) = probe(&anchors(), "localhost", port, 30) else {
            panic!("the fixture CA signs the fixture leaf; this must verify");
        };
        let pinned =
            gio::TlsCertificate::from_pem(&pem).expect("the captured PEM round-trips through GIO");
        let leaf = gio::TlsCertificate::from_file(fixture("server-leaf.pem"))
            .expect("the fixture leaf parses");
        assert!(
            pinned.is_same(&leaf),
            "what a probe pins must be the certificate the gateway PRESENTED, not the anchor"
        );
    }

    /// A leaf for the right host under the **wrong CA** is refused, and the
    /// refusal is named in the card's words.
    ///
    /// This is the case that matters most: it is what a gateway on a *different*
    /// hive looks like, and accepting it would make the whole launch-time
    /// verification decorative.
    ///
    /// Mutation (re-run this round, red): return `true` from [`probe`]'s
    /// `accept-certificate` handler — the one-character "make it work" change —
    /// and this reds with a `Trusted`.
    #[test]
    fn a_leaf_under_another_ca_is_refused_and_the_reason_is_named() {
        let port = serve("other-leaf.pem", "other-leaf-key.pem");
        let Presented::Refused { flags } = probe(&anchors(), "localhost", port, 30) else {
            panic!("the fixture CA does not sign other-leaf; this must be refused");
        };
        assert!(
            flags.contains(gio::TlsCertificateFlags::UNKNOWN_CA),
            "{flags:?}"
        );
        assert!(
            super::describe_flags(flags).contains("nothing in that file signs"),
            "the card must say which check failed: {}",
            super::describe_flags(flags)
        );
    }

    /// A leaf under the **right** CA whose only fault is the clock is refused
    /// too, and named differently — the distinction the operator needs.
    #[test]
    fn an_expired_leaf_is_refused_and_named_as_expired() {
        let port = serve("expired-leaf.pem", "expired-leaf-key.pem");
        let Presented::Refused { flags } = probe(&anchors(), "localhost", port, 30) else {
            panic!("an expired leaf must be refused");
        };
        assert!(
            flags.contains(gio::TlsCertificateFlags::EXPIRED),
            "{flags:?}"
        );
        let described = super::describe_flags(flags);
        assert!(described.contains("EXPIRED"), "{described}");
        assert!(
            !described.contains("UNKNOWN_CA"),
            "the anchor DID sign it; saying otherwise sends the operator after the wrong file: \
             {described}"
        );
    }

    /// **A probe failure is not a trust failure**, and the two must not read
    /// the same — a gateway that is down while `host.sock` is up is the
    /// likeliest way to see this card at all.
    ///
    /// Mutation (re-run this round, red): collapse `Unreachable` into
    /// `Refused { flags: empty }` and the second assertion reds.
    #[test]
    fn a_gateway_that_does_not_answer_is_a_reachability_failure_not_a_trust_one() {
        let Presented::Unreachable(why) = probe(&anchors(), "127.0.0.1", closed_port(), 5) else {
            panic!("nothing is listening there");
        };
        assert!(why.contains("127.0.0.1"), "{why}");

        let resolved = resolve_route(
            &Route::VerifyAgainstBundle {
                bundle: anchors(),
                source: Source::HiveDir,
            },
            &format!("https://127.0.0.1:{}/agent/stray/", closed_port()),
        );
        assert_eq!(resolved.policy, TlsPolicy::SystemStore);
        let tried = resolved.tried.expect("the card says what was tried");
        assert!(
            tried.contains("connection problem"),
            "a card that blames the certificate here sends the operator after the wrong thing: \
             {tried}"
        );
    }

    /// Ask 1 end to end, through the function a launch calls: the route-2
    /// bundle verifies, the policy pins the **verified** leaf (not a path),
    /// and the card's sentence names the anchors file.
    #[test]
    fn resolving_the_bundle_route_pins_the_verified_leaf_and_says_which_file_did_it() {
        let port = serve("server-leaf.pem", "server-leaf-key.pem");
        let resolved = resolve_route(
            &Route::VerifyAgainstBundle {
                bundle: anchors(),
                source: Source::HiveDir,
            },
            &format!("https://localhost:{port}/agent/stray/?hide=header,input"),
        );
        let TlsPolicy::AllowCertificateForHost {
            cert: Pinned::VerifiedPem(pem),
            host,
        } = &resolved.policy
        else {
            panic!("route 2 pins what it verified: {:?}", resolved.policy);
        };
        assert_eq!(host, "localhost");
        assert!(pem.contains("BEGIN CERTIFICATE"), "{pem}");

        let tried = resolved.tried.as_deref().expect("…and says so");
        assert!(tried.contains("fixture-ca.pem"), "{tried}");
        assert!(tried.contains("verified"), "{tried}");
    }

    /// The refusal arm of the same function: the policy falls back to the
    /// system store — never a widened session — and the card carries the flag
    /// name.
    #[test]
    fn resolving_a_refused_bundle_route_keeps_the_system_store_and_names_the_check() {
        let port = serve("other-leaf.pem", "other-leaf-key.pem");
        let resolved = resolve_route(
            &Route::VerifyAgainstBundle {
                bundle: anchors(),
                source: Source::HiveDir,
            },
            &format!("https://localhost:{port}/agent/stray/"),
        );
        assert_eq!(
            resolved.policy,
            TlsPolicy::SystemStore,
            "a refused chain must never be pinned anyway"
        );
        let tried = resolved.tried.expect("the card says which check failed");
        assert!(tried.contains("UNKNOWN_CA"), "{tried}");
        assert!(tried.contains("fixture-ca.pem"), "{tried}");
    }

    /// Route 3 through the same function, both orderings: `gateway.pem` pins
    /// silently, a CA-first file pins and **says the first certificate does
    /// not name this host** — which is the card text the amendment asked for.
    ///
    /// Mutation (re-run this round, red): drop the identity check in
    /// `tls::pin_from_file` and the second half reds, leaving a launch that
    /// pins a CA and a card that cannot say why nothing matched.
    #[test]
    fn resolving_the_gateway_file_names_the_wrong_order_on_the_card() {
        let good = resolve_route(
            &Route::PinLeaf {
                pem: fixture("gateway.pem"),
                source: Source::HiveDir,
            },
            "https://hive.local/agent/stray/",
        );
        assert_eq!(
            good.policy,
            TlsPolicy::AllowCertificateForHost {
                cert: Pinned::File(fixture("gateway.pem")),
                host: "hive.local".to_owned(),
            }
        );
        let tried = good
            .tried
            .as_deref()
            .expect("the card knows what was pinned");
        assert!(tried.starts_with("pinned the certificate in"), "{tried}");
        assert!(
            !tried.contains("does not name"),
            "a leaf-first gateway.pem is exactly right; complaining about it is the false \
             positive that would make this diagnosis noise: {tried}"
        );

        let wrong = resolve_route(
            &Route::PinLeaf {
                pem: fixture("gateway-ca-first.pem"),
                source: Source::HiveDir,
            },
            "https://hive.local/agent/stray/",
        );
        let tried = wrong.tried.as_deref().expect("…and what went wrong");
        assert!(tried.contains("gateway-ca-first.pem"), "{tried}");
        assert!(tried.contains("does not name hive.local"), "{tried}");
        assert!(tried.contains("leaf comes first"), "{tried}");
    }

    /// An explicitly named file that is not a certificate at all degrades to
    /// the system store and names the path — the failure an operator with a
    /// typo actually hits.
    #[test]
    fn an_unreadable_pin_degrades_to_the_system_store_and_names_the_path() {
        let resolved: Resolved = resolve_route(
            &Route::PinLeaf {
                pem: PathBuf::from("/nonexistent/hive-gateway.pem"),
                source: Source::Env(crate::tls::CERT_ENV),
            },
            "https://hive.local/agent/stray/",
        );
        assert_eq!(resolved.policy, TlsPolicy::SystemStore);
        let tried = resolved
            .tried
            .expect("the card names the file it could not read");
        assert!(tried.contains("/nonexistent/hive-gateway.pem"), "{tried}");
        assert!(tried.contains(crate::tls::CERT_ENV), "{tried}");
    }
}
