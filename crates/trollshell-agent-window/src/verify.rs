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
//! # What the pin actually compares (and why chain length does not matter)
//!
//! Verified for #1242's review, because it is the one fact that could have
//! made route 2 decorative: `HostTLSCertificateSet`
//! (`SoupNetworkSession.cpp:68-97`, webkitgtk 2.52.6) keys on a **SHA-256 of
//! the certificate's own DER** — the `"certificate"` property — and not on the
//! chain. So route 2's leaf-only [`Presented::Trusted`] PEM and route 3's
//! leaf-plus-CA `gateway.pem` both match a gateway presenting a full chain,
//! and neither has to reconstruct what the server will send.
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

/// The probe's **per-I/O** socket timeout, in seconds
/// (`g_socket_client_set_timeout`).
///
/// This is what it says on the tin and no more: it fires when a single read or
/// write blocks this long, and **every byte that arrives resets it**. It is not
/// a bound on how long the probe takes — see [`PROBE_DEADLINE`], which is.
///
/// Measured through [`probe`] on `77b3e84f`, when this was the only limit
/// (#1242 review, finding 1):
///
/// | peer | elapsed |
/// | --- | --- |
/// | accepts TCP, never speaks | 5.16 s — this timeout, honoured |
/// | a TLS record header then one byte every 1.5 s | **21.01 s** |
///
/// A dribbling peer is not only a hostile story: a slow or lossy link to a
/// remote hive behaves exactly like that, and the remote hive is the case
/// route 2 is *recommended* for.
pub const PROBE_IO_TIMEOUT_SECS: u32 = 5;

/// The **total** wall-clock the probe may take, after which it is cancelled
/// and reported as unreachable.
///
/// Unlike [`PROBE_IO_TIMEOUT_SECS`] this one is a real bound, because it is
/// enforced by a watchdog thread holding a [`gio::Cancellable`] that every
/// blocking call here is given.
///
/// **Measured** (`tls_tests::a_dribbling_peer_cannot_hold_the_probe_past_its_deadline`):
/// the peer that held `77b3e84f` for 21.01 s returns in ~2.0 s against a 2 s
/// budget, and the card says the probe was cancelled rather than blaming the
/// peer for a verdict it never gave.
///
/// What it covers:
///
/// - the TCP connect, and the whole TLS handshake — byte-dribbling peer
///   included. This is the measured half.
/// - **name resolution**, by construction rather than by measurement. It
///   happens *inside* `g_socket_client_connect_to_host`, before the socket the
///   I/O timeout is set on exists, which is why that timeout never bounded DNS
///   at all; the cancellable goes into that same call, and GIO's threaded
///   resolver runs its blocking lookup under a `GTask` with return-on-cancel,
///   so cancelling makes *this* call return while the `getaddrinfo` behind it
///   runs to completion in GIO's thread pool and throws its answer away. That
///   is GIO's contract, not a number taken here — a resolver slow enough to
///   matter needs a stalled nameserver, which these tests have no hermetic way
///   to stand up. [#1246] resolves on a worker and retires the question.
///
/// What it does **not** cover: [`gio::TlsFileDatabase::new`], the local read
/// and parse of the anchors file, which takes no cancellable. That is a
/// `read()` of a file the caller already opened once
/// ([`is_readable`]), so it is bounded by the filesystem and nothing else.
///
/// [#1246]: https://github.com/vibec0re/trollshell/issues/1246
///
/// # It bounds the freeze, it does not remove it
///
/// The probe still runs **on the GTK main thread** (`window.rs`'s `load_page`
/// ← `apply` ← the `glib::spawn_future_local` pump), so for up to this long
/// nothing repaints and no button responds. Moving it to a worker with a
/// "verifying…" state on the card is
/// [#1246](https://github.com/vibec0re/trollshell/issues/1246); this constant
/// is the honest bound until then, not a substitute for it.
pub const PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);

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
    /// No chain was ever judged: no route to the host, no TLS on the port, or
    /// the deadline.
    ///
    /// **This is not a trust failure** and the card says so — it is the
    /// difference between "the hive's certificate is wrong" and "the hive's
    /// gateway did not answer".
    Unreachable(String),
    /// The anchors file itself could not be loaded — **nothing was
    /// contacted**.
    ///
    /// Split out of [`Presented::Unreachable`] by #1242's review (finding 2):
    /// a `TROLLSHELL_AGENT_WINDOW_CA` naming a file that is not a PEM produced
    /// *"That is a connection problem rather than a certificate one — the
    /// hive's gateway may be down"*, about a gateway no socket had been opened
    /// to. It is a third category — **your anchors file is unusable** — and it
    /// is the one an operator with a typo actually hits, so it gets its own
    /// sentence naming the file and the parse error.
    ///
    /// It is also what a launch with **no GIO TLS backend at all** takes, which
    /// is what that wrong sentence would have been permanently, had
    /// `nix/agent-window.nix` not learned to put `glib-networking` on
    /// `GIO_EXTRA_MODULES` in this same PR. `probe` detects that case
    /// explicitly (`gio::TlsError::Unavailable` on the database arm) rather
    /// than letting it fall through as an ordinary parse failure, because
    /// `TlsFileDatabase::new` needs the same backend `TlsClientConnection::new`
    /// does and fails first — so without the explicit check, the sentence
    /// naming `GIO_EXTRA_MODULES` was unreachable and every bare-session
    /// operator read "it could not be loaded as a certificate database" about
    /// a file that parses fine (#1242's re-verification, N2).
    UnusableAnchors(String),
}

/// A wall-clock bound on a run of blocking GIO calls, enforced by a watchdog
/// thread that cancels them.
///
/// GIO's own knob — `g_socket_client_set_timeout` — is per-I/O and resets on
/// every byte, so it cannot bound anything against a peer that keeps sending
/// (#1242 review, finding 1: 21.01 s measured against a documented 5 s). A
/// [`gio::Cancellable`] can, because `g_cancellable_cancel` is thread-safe —
/// gio-rs marks the type `Send + Sync` for exactly that reason — and every
/// blocking call in [`probe`] takes one.
///
/// The watchdog parks on a channel rather than sleeping the whole budget, so a
/// probe that finishes in 3 ms takes the thread down with it instead of
/// leaving one parked for the remaining seconds: dropping this value drops the
/// sender, the `recv_timeout` returns `Disconnected`, and [`Drop`] joins.
struct Deadline {
    /// Dropped on the way out, which is what wakes the watchdog early.
    done: Option<std::sync::mpsc::Sender<()>>,
    /// `None` only if the thread could not be spawned, in which case there is
    /// no deadline and [`Deadline::expired`] is always false — the per-I/O
    /// timeout is then the only limit, as it was before #1242.
    watchdog: Option<std::thread::JoinHandle<()>>,
    /// Handed to every blocking call; cancelled when the budget runs out.
    cancellable: gio::Cancellable,
}

impl Deadline {
    /// Arm a watchdog for `budget`.
    fn arm(budget: std::time::Duration) -> Self {
        let cancellable = gio::Cancellable::new();
        let (done, idle) = std::sync::mpsc::channel::<()>();
        let alarm = cancellable.clone();
        let watchdog = std::thread::Builder::new()
            .name("agent-window-probe-deadline".to_owned())
            .spawn(move || {
                if matches!(
                    idle.recv_timeout(budget),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    alarm.cancel();
                }
            })
            .map_err(|e| {
                tracing::warn!(
                    error = %e,
                    "no thread for the TLS probe's deadline; the per-I/O timeout is the only \
                     limit this launch has"
                );
            })
            .ok();
        Self {
            done: Some(done),
            watchdog,
            cancellable,
        }
    }

    /// The cancellable to hand every blocking call.
    fn cancellable(&self) -> &gio::Cancellable {
        &self.cancellable
    }

    /// Whether the budget ran out — i.e. whether the error that just came back
    /// is this deadline's doing rather than the peer's.
    fn expired(&self) -> bool {
        self.cancellable.is_cancelled()
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        // Wake the watchdog with `Disconnected` before joining it, or the join
        // waits out the whole budget on every successful probe.
        drop(self.done.take());
        if let Some(watchdog) = self.watchdog.take() {
            drop(watchdog.join());
        }
    }
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
/// The identity argument to `TlsClientConnection::new` is load-bearing in both
/// directions, measured: glib-networking treats a **missing** server identity
/// as a *failed* identity check, not a skipped one, so dropping it does not
/// quietly widen anything — it sets `BAD_IDENTITY` on every connection and
/// three of this module's tests go red. See
/// `tls_tests::a_certificate_that_does_not_name_the_host_is_refused_for_that_reason`,
/// which records that measurement against the #1242 review's contrary claim.
///
/// The connection is closed either way; nothing is sent over it and nothing is
/// read from it.
///
/// # What bounds it
///
/// `io_timeout_secs` is GIO's own per-read knob and bounds nothing on its own
/// (see [`PROBE_IO_TIMEOUT_SECS`]). `budget` is the real one: a [`Deadline`]
/// is armed before the first blocking call and its cancellable is handed to
/// every one of them, so name resolution, the connect and the whole handshake
/// together cannot exceed it. Only [`gio::TlsFileDatabase::new`] sits outside,
/// because it takes no cancellable.
#[must_use]
pub fn probe(
    bundle: &Path,
    host: &str,
    port: u16,
    io_timeout_secs: u32,
    budget: std::time::Duration,
) -> Presented {
    let database = match gio::TlsFileDatabase::new(bundle) {
        Ok(db) => db,
        // No GIO TLS backend is loaded at all (`GIO_EXTRA_MODULES` pointing
        // nowhere, or glib-networking simply not installed): every GIO TLS
        // call fails on the same grounds, and `TlsFileDatabase::new` just
        // gets there first — before this check existed, that meant the
        // sentence below (naming the actual cause) was unreachable, because
        // this arm always won first and blamed a file that parses fine
        // (#1242's re-verification, N2). Checked before the generic parse
        // failure below rather than after, so nothing is contacted either way.
        Err(e) if e.kind::<gio::TlsError>() == Some(gio::TlsError::Unavailable) => {
            return Presented::UnusableAnchors(format!(
                "there is no GIO TLS backend to check {host} with ({e}) — glib-networking must \
                 be on GIO_EXTRA_MODULES"
            ));
        }
        Err(e) => {
            // NOT `Unreachable`: nothing has been contacted yet, and saying
            // "the gateway may be down" about a file that will not parse sends
            // the operator after the wrong thing (#1242 review, finding 2).
            //
            // The path is deliberately *not* repeated here: the caller's
            // sentence opens with it and GIO's own message carries it, so
            // spelling it a third time is how a card ends up printing one long
            // path three times to say one thing.
            return Presented::UnusableAnchors(format!(
                "it could not be loaded as a certificate database ({e})"
            ));
        }
    };

    // Armed before the first blocking call, so it covers DNS too — which the
    // per-I/O timeout never did, since `connect_to_host` resolves before the
    // socket that timeout applies to exists.
    let deadline = Deadline::arm(budget);
    let unreachable = |why: String| {
        if deadline.expired() {
            Presented::Unreachable(format!(
                "{why}; the probe was cancelled after {budget:.1?} — the window blocks while it \
                 runs, so it is bounded rather than allowed to finish"
            ))
        } else {
            Presented::Unreachable(why)
        }
    };

    let client = gio::SocketClient::new();
    // GIO's per-read timeout. It is **not** the bound — every byte resets it,
    // which is how a dribbling peer held the main thread for 21 s before
    // #1242's review measured it. `deadline` is what bounds this function.
    client.set_timeout(io_timeout_secs);
    let connection = match client.connect_to_host(
        &connect_target(host, port),
        port,
        Some(deadline.cancellable()),
    ) {
        Ok(c) => c,
        Err(e) => {
            return unreachable(format!("could not reach {host}:{port} ({e})"));
        }
    };

    let identity = gio::NetworkAddress::new(host, port);
    let tls = match gio::TlsClientConnection::new(&connection, Some(&identity)) {
        Ok(t) => t,
        // A backstop, not the primary path: with no backend at all the
        // database arm above already caught it before a connection was ever
        // opened (N2). This stays for the (untested) case where a backend
        // exists — `TlsFileDatabase::new` succeeded — but this particular
        // constructor still fails; the sentence is the same either way.
        Err(e) => {
            return Presented::UnusableAnchors(format!(
                "there is no GIO TLS backend to check {host} with ({e}) — glib-networking must be \
                 on GIO_EXTRA_MODULES"
            ));
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

    let verdict = match tls.handshake(Some(deadline.cancellable())) {
        Ok(()) => match tls.peer_certificate() {
            Some(leaf) => match leaf.certificate_pem() {
                Some(pem) => Presented::Trusted(pem.to_string()),
                None => unreachable(format!(
                    "{host} was verified but its certificate could not be re-encoded"
                )),
            },
            None => unreachable(format!("{host} completed a handshake with no certificate")),
        },
        // The flags take precedence over the deadline: an `accept-certificate`
        // that already fired is a **judgement**, and reporting it as "the
        // probe ran out of time" would lose the one thing worth saying.
        Err(e) => match refused.get() {
            Some(flags) => Presented::Refused { flags },
            None => unreachable(format!("the handshake with {host} failed ({e})")),
        },
    };

    // Best effort, and still under the deadline: the verdict is already in
    // hand, and a close error on a connection nothing was written to says
    // nothing worth reporting — but a close that blocked past the budget would
    // undo the bound this function just promised.
    drop(tls.close(Some(deadline.cancellable())));
    verdict
}

/// The `host:port` string `g_socket_client_connect_to_host` parses, with an
/// **IPv6 literal put back in its brackets**.
///
/// [`crate::tls::host_of`] hands the URL's bracketed spelling (which is what a
/// human reads on the card), and [`crate::tls::identity_host`] strips the
/// brackets for `GNetworkAddress` — and for `WebKit`'s per-host pin, which
/// strips them itself before the lookup and so must be *given* the bare form.
/// This is the third spelling: `connect_to_host`
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

    /// The deadline the tests that are *not* about the deadline run under:
    /// generous, so a slow CI box never turns a trust assertion into a timeout
    /// assertion.
    const TEST_BUDGET: Duration = Duration::from_mins(1);

    /// A peer that accepts TCP, claims a long TLS record, and then **dribbles**
    /// — one byte at a time, slowly, for `dribble` in total.
    ///
    /// This is the shape #1242's review measured at 21.01 s against a
    /// documented 5 s bound: every byte resets GIO's per-I/O timeout, so that
    /// timeout can never fire while the peer keeps talking. Only
    /// [`Deadline`](super::Deadline) stops it.
    ///
    /// Plain `std::net`, not GIO: nothing here needs a GIO object, and a
    /// `TcpListener` is `Send`, so the listener can be moved into the thread
    /// after the port is read off it.
    fn serve_dribbling(dribble: Duration, step: Duration) -> u16 {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port to dribble on");
        let port = listener.local_addr().expect("…with an address").port();
        std::thread::spawn(move || {
            use std::io::Write as _;
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            // A handshake record header announcing 0x0400 bytes to follow, so
            // the client keeps waiting for a body that never completes.
            if stream.write_all(&[0x16, 0x03, 0x03, 0x04, 0x00]).is_err() {
                return;
            }
            let until = std::time::Instant::now() + dribble;
            while std::time::Instant::now() < until {
                if stream.write_all(&[0x01]).is_err() || stream.flush().is_err() {
                    return;
                }
                std::thread::sleep(step);
            }
        });
        port
    }

    /// **The probe is bounded in wall clock, not per read** — #1242's review
    /// finding 1, which measured 21.01 s against a documented 5 s.
    ///
    /// The peer here dribbles for 12 s with a 5 s per-I/O timeout, so without
    /// a deadline the probe takes ~12 s (then the timeout, ~17 s). With a 2 s
    /// budget it takes ~2 s, and the verdict says the probe was cancelled
    /// rather than blaming the peer for something it did not do.
    ///
    /// The assertion is generous (under 8 s) so it is measuring the
    /// *mechanism*, not CI's scheduler — it still sits far below what the
    /// un-deadlined shape produces.
    ///
    /// Mutation (re-run this round, red): hand `None::<&gio::Cancellable>` to
    /// `connect_to_host`/`handshake` again, or make `Deadline::arm` a no-op,
    /// and this reds on the elapsed bound.
    #[test]
    fn a_dribbling_peer_cannot_hold_the_probe_past_its_deadline() {
        let port = serve_dribbling(Duration::from_secs(12), Duration::from_millis(300));
        let budget = Duration::from_secs(2);
        let started = std::time::Instant::now();
        let verdict = probe(&anchors(), "127.0.0.1", port, 5, budget);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(8),
            "the probe took {elapsed:.2?} against a {budget:.1?} budget — the per-I/O timeout is \
             not a bound, which is the whole of #1242's finding 1"
        );
        assert!(
            elapsed >= budget,
            "…and it did not give up early either: {elapsed:.2?}"
        );
        let Presented::Unreachable(why) = verdict else {
            panic!(
                "a peer that never completes a handshake is unreachable, not judged: {verdict:?}"
            )
        };
        assert!(
            why.contains("cancelled after"),
            "the card must say the probe was cut short rather than blame the peer: {why}"
        );
    }

    /// …and the deadline does not cost anything on the happy path: a local
    /// handshake still finishes in milliseconds, and the watchdog thread goes
    /// down with it rather than parking for the rest of the budget.
    #[test]
    fn the_deadline_does_not_slow_a_gateway_that_answers() {
        let port = serve("server-leaf.pem", "server-leaf-key.pem");
        let started = std::time::Instant::now();
        let verdict = probe(&anchors(), "localhost", port, 5, Duration::from_secs(30));
        let elapsed = started.elapsed();
        assert!(
            matches!(verdict, Presented::Trusted(_)),
            "{verdict:?} — the fixture CA signs the fixture leaf"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "a 30 s budget must not become a 30 s wait: {elapsed:.2?} (if this reds, `Deadline`'s \
             Drop is sleeping out the budget instead of waking the watchdog)"
        );
    }

    /// **An anchors file that will not load is not a network problem** —
    /// #1242's review finding 2. No socket is opened, and the card must not
    /// say the gateway may be down.
    ///
    /// The fixture is this directory's own README: a real file, readable,
    /// and not a PEM — which is exactly the shape a typo'd
    /// `TROLLSHELL_AGENT_WINDOW_CA` has.
    ///
    /// Mutation (re-run this round, red): fold `UnusableAnchors` back into
    /// `Unreachable` and both halves red — the verdict here and the card
    /// sentence below.
    #[test]
    fn an_anchors_file_that_will_not_load_is_not_reported_as_the_gateway_being_down() {
        let verdict = probe(
            &fixture("README.md"),
            "127.0.0.1",
            closed_port(),
            5,
            TEST_BUDGET,
        );
        let Presented::UnusableAnchors(why) = verdict else {
            panic!("a file that is not a PEM is an anchors problem, not a network one: {verdict:?}")
        };
        // This half asserts *our* wording, not GIO's: the path belongs to the
        // caller's sentence (below), and a test that pinned GIO's message text
        // would red on a glib-networking bump saying nothing.
        assert!(why.contains("certificate database"), "{why}");

        let resolved = resolve_route(
            &Route::VerifyAgainstBundle {
                bundle: fixture("README.md"),
                source: Source::Env(super::CA_ENV),
            },
            "https://hive.local/agent/stray/",
        );
        assert_eq!(resolved.policy, TlsPolicy::SystemStore);
        let tried = resolved.tried.expect("the card names the file");
        assert!(tried.contains("README.md"), "{tried}");
        assert!(tried.contains(super::CA_ENV), "{tried}");
        assert!(
            tried.contains("Nothing was contacted"),
            "the operator must not be sent after a gateway nobody called: {tried}"
        );
        assert!(
            !tried.contains("gateway may be down"),
            "…which is the sentence this test exists to keep out of this arm: {tried}"
        );
    }

    /// **No GIO TLS backend at all is not an anchors-file problem either** —
    /// #1242's re-verification, N2. `GIO_EXTRA_MODULES` is a process-wide
    /// loader setting GIO reads once and caches for the life of the process
    /// (the devShell and `checks.system-tests` both put glib-networking on it,
    /// #1234 ask 3), so proving the *other* case — nowhere to load a backend
    /// from — needs a genuinely separate OS process with it pointed at
    /// nothing, on the `detached_launch_falls_back_without_a_user_manager`
    /// shape (`trollshell/src/plugins/tests.rs`): `std::env::set_var` is
    /// `unsafe` in edition 2024 (forbidden workspace-wide) and unsound under a
    /// multi-threaded harness regardless.
    ///
    /// Mutation (drop the `gio::TlsError::Unavailable` arm in `probe`): the
    /// child still passes as a process, but its own assertions red — the
    /// sentence goes back to naming only the anchors file, which is exactly
    /// the shape N2 measured against a bare niri session.
    #[test]
    fn no_gio_tls_backend_names_gio_extra_modules_not_the_file() {
        let exe = std::env::current_exe().expect("this test binary's own path");
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "--nocapture",
                "--test-threads=1",
                "verify::tls_tests::no_gio_tls_backend_names_gio_extra_modules_not_the_file_inner",
            ])
            .env("GIO_EXTRA_MODULES", "/nonexistent")
            .env("AGENT_WINDOW_TEST_NO_GIO_BACKEND", "1")
            .output()
            .expect("re-exec this test binary with GIO_EXTRA_MODULES pointed nowhere");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "the no-backend child must pass, not panic.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        );
        assert!(
            stdout.contains("NO_GIO_BACKEND_NAMES_GIO_EXTRA_MODULES"),
            "the child must report that the card actually names GIO_EXTRA_MODULES.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        );
    }

    /// The child half of the test above. Runs its assertions only when
    /// re-executed with `GIO_EXTRA_MODULES` pointed nowhere; an ordinary
    /// devShell/CI run has a working backend (#1234 ask 3), so there would be
    /// nothing to assert.
    #[test]
    fn no_gio_tls_backend_names_gio_extra_modules_not_the_file_inner() {
        if std::env::var_os("AGENT_WINDOW_TEST_NO_GIO_BACKEND").is_none() {
            return;
        }
        let verdict = probe(&anchors(), "127.0.0.1", closed_port(), 5, TEST_BUDGET);
        let Presented::UnusableAnchors(why) = verdict else {
            panic!("with no GIO TLS backend the anchors cannot even be opened: {verdict:?}")
        };
        assert!(
            why.contains("GIO_EXTRA_MODULES"),
            "the card must name the actual cause instead of leaving the operator staring at a \
             file that is fine: {why}"
        );

        let resolved = resolve_route(
            &Route::VerifyAgainstBundle {
                bundle: anchors(),
                source: Source::Env(super::CA_ENV),
            },
            "https://hive.local/agent/stray/",
        );
        assert_eq!(resolved.policy, TlsPolicy::SystemStore);
        let tried = resolved
            .tried
            .expect("the card names what this launch tried");
        assert!(
            tried.contains("GIO_EXTRA_MODULES"),
            "the composed card must carry the real cause too, not just `probe`'s own verdict: \
             {tried}"
        );
        println!("NO_GIO_BACKEND_NAMES_GIO_EXTRA_MODULES");
    }

    /// **The identity check is real, and it is the stated reason for probing
    /// at all** rather than calling `TlsCertificateExt::verify` on a chain.
    ///
    /// `127.0.0.2` is a loopback address the fixture leaf's SANs do not carry
    /// (they carry `127.0.0.1`, `::1`, `hive.local`, `localhost`), and the
    /// listener binds any-inet, so the connection succeeds and only the name
    /// is wrong.
    ///
    /// # What this pins, and what was already pinned — measured
    ///
    /// #1242's review asked for this test on the premise that dropping
    /// `Some(&identity)` from `TlsClientConnection::new` "reds **nothing** in
    /// the 117-test suite". **That premise is false on this tree**, and the
    /// measurement is worth keeping rather than the claim: dropping it reds
    /// three tests —
    /// [`the_hives_anchors_verify_the_gateway_and_the_leaf_is_what_gets_pinned`],
    /// [`resolving_the_bundle_route_pins_the_verified_leaf_and_says_which_file_did_it`]
    /// and [`the_deadline_does_not_slow_a_gateway_that_answers`] — because
    /// glib-networking treats *no identity to check* as a **failed** identity
    /// check rather than a skipped one, so `BAD_IDENTITY` becomes
    /// unconditional and the happy path stops being happy. The argument was
    /// never unpinned; it was pinned from the other side.
    ///
    /// So this test does **not** pin that argument (nothing it asserts changes
    /// when the argument goes), and saying so is the point — the alternative
    /// is a doc comment claiming a falsification it does not have, which is
    /// the habit `webview.rs`'s settings tests already document.
    ///
    /// What it *does* pin is the half the module doc is written against and
    /// the two refusal siblings are not: that a certificate the anchors
    /// **did** sign, failing only on the name, comes back as `BAD_IDENTITY`
    /// **and not** `UNKNOWN_CA`, and reaches the card in those words. Get that
    /// wrong and the card sends the operator after the wrong file — which is
    /// the whole of #1242's finding-2 complaint, applied to a different arm.
    ///
    /// [`the_hives_anchors_verify_the_gateway_and_the_leaf_is_what_gets_pinned`]: tls_tests::the_hives_anchors_verify_the_gateway_and_the_leaf_is_what_gets_pinned
    /// [`resolving_the_bundle_route_pins_the_verified_leaf_and_says_which_file_did_it`]: tls_tests::resolving_the_bundle_route_pins_the_verified_leaf_and_says_which_file_did_it
    /// [`the_deadline_does_not_slow_a_gateway_that_answers`]: tls_tests::the_deadline_does_not_slow_a_gateway_that_answers
    #[test]
    fn a_certificate_that_does_not_name_the_host_is_refused_for_that_reason() {
        let port = serve("server-leaf.pem", "server-leaf-key.pem");
        let verdict = probe(&anchors(), "127.0.0.2", port, 10, TEST_BUDGET);
        let Presented::Refused { flags } = verdict else {
            panic!("the fixture leaf does not name 127.0.0.2: {verdict:?}")
        };
        assert!(
            flags.contains(gio::TlsCertificateFlags::BAD_IDENTITY),
            "{flags:?}"
        );
        assert!(
            !flags.contains(gio::TlsCertificateFlags::UNKNOWN_CA),
            "the anchor DID sign it — only the name is wrong, and the card must say so: {flags:?}"
        );
        assert!(
            super::describe_flags(flags).contains("does not name this host"),
            "{}",
            super::describe_flags(flags)
        );
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
        let Presented::Trusted(pem) = probe(&anchors(), "localhost", port, 30, TEST_BUDGET) else {
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
        let Presented::Refused { flags } = probe(&anchors(), "localhost", port, 30, TEST_BUDGET)
        else {
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
        let Presented::Refused { flags } = probe(&anchors(), "localhost", port, 30, TEST_BUDGET)
        else {
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
        let Presented::Unreachable(why) =
            probe(&anchors(), "127.0.0.1", closed_port(), 5, TEST_BUDGET)
        else {
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
