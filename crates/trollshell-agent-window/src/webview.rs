//! The embedded engine — **the one place this crate names `WebKitGTK`** — and
//! the policy that decides what it is allowed to do.
//!
//! Spec #955 §7.1 decided the engine for v1 (`webkitgtk_6_0` through the
//! `webkit6` gtk-rs crate) and recorded Servo as a *re-check*, not a someday:
//! its embedding API is the unfinished part (servo/servo#27579 open since
//! 2020; the one GTK4 integration re-executes your binary in a subprocess
//! rather than rendering in-process; Verso archived 2025-10), and CSS Grid is
//! the known web-platform gap. The trigger for revisiting is concrete — a
//! maintained crate offering in-process `GtkGLArea` rendering with input wired
//! up.
//!
//! Until then a **cargo feature per engine would be a second code path nobody
//! compiles**, which is worse than a module boundary: everything outside this
//! file talks about "the page widget" and a URL, so the day there is a second
//! engine to try, this file is the whole surface it has to satisfy.
//!
//! # The view is not free to go where it likes
//!
//! This window has no address bar, no back button, and a header that asserts
//! an identity. So the page it embeds gets **one origin** and no ability to
//! open windows: [`crate::page::decide`] decides, `decide-policy` and `create`
//! enforce, and a refused link the operator **clicked** is handed to the
//! desktop's default handler so it still works — in a browser, where the
//! operator can see where they are. A refused load nobody clicked goes
//! nowhere: the page is agent output, and the desktop is not its to drive.
//! See `navigable_in_place`'s and `decide`'s docs for the failures these
//! prevent.

use adw::prelude::*;
use webkit::prelude::*;

use crate::page::{Decision, LoadFailure, Refusal, Target, decide, may_leave, reload_after};
use crate::tls::{CERT_ENV, Pinned, Resolved, TlsPolicy, failure_description};

/// The `Stack` child names — the page, the TLS failure state that replaces
/// it, and (#1557 item 2) the card that stands in front of it while a gateway
/// that did not answer is asked again.
const PAGE: &str = "page";
const TLS_FAILED: &str = "tls-failed";
const UNANSWERED: &str = "unanswered";

/// What the page reports to the window holding it about its own loads —
/// #1557 item 2. The window owns the backoff, the timer and the parking;
/// this file only says what happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gateway {
    /// A main-frame load of the hive's own page got no answer
    /// ([`reload_after`] said to ask again). The page's stack now shows
    /// [`show_unanswered`]'s card when the window hands it one; `uri` is the
    /// URI to load again, `why` the engine's own words for the failure.
    Unanswered {
        /// The URI whose load failed — `WebKit`'s spelling, on the hive's
        /// own origin.
        uri: String,
        /// The `GError`'s message, for the card and the journal.
        why: String,
    },
    /// A main-frame load committed while that card was up: something
    /// answered. The card is already down when this is sent.
    Answered,
}

/// The widget name [`verifying`] stamps on its state, and the whole of how
/// [`is_verifying`] recognises it.
///
/// A name rather than a type test: the failure card is an `adw::StatusPage`
/// too, so `downcast_ref::<adw::StatusPage>()` would answer yes to both, and
/// the one thing a caller ever wants to know here is *which* of the two the
/// page slot is showing.
const VERIFYING: &str = "tls-verifying";

/// The widget name [`probe_died`] stamps on its state, and the whole of how
/// [`is_probe_died`] recognises it — the same idiom as [`VERIFYING`], for the
/// same reason: the widget itself is an `adw::StatusPage`, like every other
/// card this file builds.
const PROBE_DIED: &str = "tls-probe-died";

/// The widget name [`unreachable()`] stamps on its state, and the whole of how
/// [`is_unreachable`] recognises it — [`VERIFYING`]'s idiom once more.
const UNREACHABLE: &str = "tls-unreachable";

/// Build the page widget for `url`, under the policy
/// [`crate::tls::resolve`] settled on.
///
/// The returned widget is a `gtk::Stack`: the [`webkit::WebView`] itself, plus
/// the inline error state [`failure_description`] fills in when verification
/// fails. [`view_of`] gets the view back out of it.
///
/// `trust.tried` — the sentence naming the file this launch read and what came
/// of it — is carried here rather than recomputed, because by the time
/// `load-failed-with-tls-errors` runs the probe is long over and its verdict
/// is the single most useful thing the card can say (#1234).
///
/// The network session is configured **before** the first load: the
/// TLS-errors policy is set explicitly (always
/// [`Fail`](webkit::TLSErrorsPolicy::Fail), see [`crate::tls`]) rather than
/// left at the default, so a reader of this file can see that nothing here
/// turns verification off.
///
/// `gateway` hears about the page's own loads ([`Gateway`]) — how the window
/// learns that a gateway that was down at open has to be asked again, on a
/// route where no probe ever ran (#1557 item 2).
#[must_use]
pub fn page(url: &str, trust: &Resolved, gateway: impl Fn(Gateway) + 'static) -> gtk::Widget {
    let policy = &trust.policy;
    // **Ephemeral**, not `NetworkSession::default()`: the default is the
    // persistent one, which accumulates cookies, cache and `IndexedDB` under
    // `$XDG_{DATA,CACHE}_HOME` per app-id for ever, with nothing in this
    // window to clear it or mention it. A window that is opened to look at an
    // agent should not be quietly hoarding that agent's storage; the login the
    // hive's page performs lasts the window, which is the lifetime the
    // operator can see.
    let session = webkit::NetworkSession::new_ephemeral();
    session.set_tls_errors_policy(policy.errors_policy());

    if let Some((cert, host)) = pin_for(policy) {
        // `from_file` takes the FIRST PEM block as the certificate and the rest
        // as its issuer chain — which is exactly hyperhive's `gateway.pem`
        // (`cat leaf-only ca.pem`) and exactly why a `trust-bundle.pem`, which
        // leads with the CA, can never match. `from_pem` is the route-2 arm:
        // the leaf the gateway itself presented, already verified against the
        // hive's anchors before it got here.
        let loaded = match cert {
            Pinned::File(pem) => gtk::gio::TlsCertificate::from_file(pem)
                .map_err(|e| format!("{} could not be read ({e})", pem.display())),
            Pinned::VerifiedPem(pem) => gtk::gio::TlsCertificate::from_pem(pem)
                .map_err(|e| format!("the verified leaf could not be re-parsed ({e})")),
        };
        match loaded {
            Ok(cert) => {
                session.allow_tls_certificate_for_host(&cert, host);
                tracing::info!(
                    host,
                    "pinning one certificate for this host — it must be the one the gateway \
                     PRESENTS (its leaf); a CA bundle will not match and the load will still \
                     fail, which is why {CERT_ENV} is documented as the leaf"
                );
            }
            Err(why) => tracing::warn!(
                %why,
                "no certificate to pin for this host; using the system trust store"
            ),
        }
    }

    let view = webkit::WebView::builder()
        .network_session(&session)
        .settings(&settings())
        .hexpand(true)
        .vexpand(true)
        .build();

    let failed = failure_state();
    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.add_named(&view, Some(PAGE));
    stack.add_named(&failed, Some(TLS_FAILED));
    stack.set_visible_child_name(PAGE);

    install_policy(&view, url, std::rc::Rc::new(elsewhere));

    // Deliberately **not** "accept and reload": that would be trust on first
    // use with no human in it. Returning `false` lets `WebKit` render its own
    // failure page underneath, and we put ours in front of it — the operator
    // has no other source of instructions here, since the window has no
    // address bar and `WebKit`'s own text says only that the load failed.
    let sink = stack.clone();
    let error = failed.clone();
    let tried = trust.tried.clone();
    view.connect_load_failed_with_tls_errors(move |_, failing_uri, _cert, errors| {
        let host = crate::tls::host_of(failing_uri).unwrap_or_else(|| failing_uri.to_owned());
        tracing::warn!(
            uri = failing_uri,
            ?errors,
            tried = tried
                .as_deref()
                .unwrap_or("nothing — the system trust store decided"),
            "TLS verification failed for the hive. On a same-host deploy the window reads \
             hyperhive's own trust-bundle.pem / gateway.pem and needs no setting; otherwise \
             point TROLLSHELL_AGENT_WINDOW_CA at a copy of that bundle, or as a last resort \
             {CERT_ENV} at the certificate the gateway PRESENTS — its leaf, never the bundle. \
             The window's own error state spells them all out"
        );
        // `AdwStatusPage:description` is parsed as Pango markup, and
        // `failure_message` is not valid markup on its own (its `openssl …
        // </dev/null` reads as an unopened closing tag) — escape at this
        // sink, the one place this text becomes a description. See
        // `tls::failure_message`'s docs (#1224).
        error.set_description(Some(&failure_description(&host, tried.as_deref())));
        sink.set_visible_child_name(TLS_FAILED);
        false
    });

    watch_loads(&view, &stack, url, gateway);

    view.load_uri(url);
    stack.upcast()
}

/// Wire the view's own load signals to `gateway` — #1557 item 2.
///
/// # `load-failed`: which failures are asked again
///
/// [`load_failure`] reads the `GError` and [`reload_after`] decides, so the
/// whole decision is two pure functions and this is only their sink. Only a
/// load of the hive's own page that **nothing answered** is reported; for it
/// the handler returns `true`, so `WebKit` draws no error page of its own —
/// the window's card stands in front of the view instead, and whatever
/// document was there (if the failure was a later navigation, not the first
/// load) is left alone underneath.
///
/// Everything else returns `false`, which is exactly what happened before
/// #1557: `WebKit`'s default handler draws its error page, or draws nothing
/// for a cancellation. In particular a **TLS** failure: `WebKit` emits
/// `load-failed` for it as well, right after `load-failed-with-tls-errors`
/// returns `false`, and by then that handler has put the TLS card up — so
/// [`load_failure`] reads the card as well as the error, and a certificate
/// failure takes the TLS card's route and no other.
///
/// # `load-changed`: when the gateway is back
///
/// A main-frame load **committing** means something answered it with a
/// response (`load-changed` is main-frame only, as `load-failed` is). If the
/// unanswered card is up at that moment, it comes down here — the view is the
/// page again — and the window is told, so the backoff starts over. It is the
/// card's own state that is checked, not the URI: whichever load got through,
/// the page is back.
///
/// That includes the error page `WebKit` draws for a failure asking again
/// cannot mend — a reload that is refused for a reason other than silence:
/// `load_alternate_html` commits like any load. The card then comes down onto
/// `WebKit`'s own explanation of that failure, which is the right thing to
/// show for it, and nothing is left queued, since that failure queued
/// nothing. A certificate failure is the exception, and stays one: its card
/// is up by then, so this check never sees the unanswered card at all.
fn watch_loads(
    view: &webkit::WebView,
    stack: &gtk::Stack,
    embedded: &str,
    gateway: impl Fn(Gateway) + 'static,
) {
    let gateway = std::rc::Rc::new(gateway);

    // Weakly: the stack owns the view these handlers live on, so a strong
    // clone would be a cycle through the view's own signal table.
    let failed = stack.downgrade();
    let origin = embedded.to_owned();
    let report = std::rc::Rc::clone(&gateway);
    view.connect_load_failed(move |_, _, failing_uri, error| {
        let Some(stack) = failed.upgrade() else {
            return false;
        };
        let tls_card_up = stack.visible_child_name().as_deref() == Some(TLS_FAILED);
        let failure = load_failure(error, tls_card_up);
        if !reload_after(&origin, failing_uri, failure) {
            tracing::debug!(
                uri = failing_uri,
                ?failure,
                error = %error,
                "the page's load failed; not one that asking again can mend"
            );
            return false;
        }
        report(Gateway::Unanswered {
            uri: failing_uri.to_owned(),
            why: error.message().to_owned(),
        });
        true
    });

    let answered = stack.downgrade();
    view.connect_load_changed(move |_, event| {
        let Some(stack) = answered.upgrade() else {
            return;
        };
        if event == webkit::LoadEvent::Committed
            && stack.visible_child_name().as_deref() == Some(UNANSWERED)
        {
            stack.set_visible_child_name(PAGE);
            gateway(Gateway::Answered);
        }
    });
}

/// `WebKit`'s own load timeout: domain `WebKitNetworkError`, code **-1001**,
/// which is no `WEBKIT_NETWORK_ERROR_*` value (`ResourceErrorSoup.cpp`'s
/// `timeoutError`, raised by `NetworkDataTaskSoup.cpp`; webkitgtk 2.54.1).
const WEBKIT_TIMEOUT: i32 = -1001;

/// Classify the `GError` a `load-failed` carries, for [`reload_after`].
///
/// `tls_card_up` is whether `load-failed-with-tls-errors` has already put the
/// TLS card in front of the view — see [`watch_loads`] for why a certificate
/// failure arrives here at all.
///
/// # What `WebKit` actually hands over (read in webkitgtk 2.54.1's source)
///
/// - A **connection** failure keeps the domain and code `GLib` gave it:
///   libsoup returns `GSocketClient`'s error as is, `ResourceError::httpError`
///   copies its domain string and code, and the UI process rebuilds the
///   `GError` from them (`WebKitNavigationClient.cpp`), passing every code
///   that is not one of `WebKit`'s own straight through (`toWebKitError`). So
///   a refused connection is `g-io-error-quark` 39, an unreachable host 37 or
///   38, a name that did not resolve `g-resolver-error-quark`. These, and
///   `WebKit`'s own timeout ([`WEBKIT_TIMEOUT`]), are the failures nothing
///   answered: [`LoadFailure::NoAnswer`].
/// - A **certificate** failure is `g-tls-error-quark`
///   (`G_TLS_ERROR_BAD_CERTIFICATE`), emitted as `load-failed` right after
///   `load-failed-with-tls-errors` returns `false`:
///   [`LoadFailure::Tls`], as is anything at all once the TLS card is up.
/// - A **cancellation** (a newer load superseding this one) is
///   `WebKitNetworkError` 302, an ignored redirect or response
///   `WebKitPolicyError` 102, a refused port `WebKitPolicyError` 103, too
///   many redirects a `SoupSessionError`: all [`LoadFailure::Other`], with
///   everything not named above. An allow-list, so a failure this has never
///   seen is never retried on a guess.
fn load_failure(error: &gtk::glib::Error, tls_card_up: bool) -> LoadFailure {
    use gtk::gio::{IOErrorEnum, ResolverError, TlsError};
    use gtk::glib::error::ErrorDomain as _;
    if tls_card_up || error.kind::<TlsError>().is_some() {
        return LoadFailure::Tls;
    }
    if let Some(io) = error.kind::<IOErrorEnum>() {
        return match io {
            IOErrorEnum::ConnectionRefused
            | IOErrorEnum::HostUnreachable
            | IOErrorEnum::NetworkUnreachable
            | IOErrorEnum::HostNotFound
            | IOErrorEnum::TimedOut
            | IOErrorEnum::BrokenPipe
            | IOErrorEnum::NotConnected
            | IOErrorEnum::ProxyFailed => LoadFailure::NoAnswer,
            _ => LoadFailure::Other,
        };
    }
    if let Some(resolver) = error.kind::<ResolverError>() {
        return match resolver {
            ResolverError::NotFound | ResolverError::TemporaryFailure => LoadFailure::NoAnswer,
            _ => LoadFailure::Other,
        };
    }
    if error.domain() == webkit::NetworkError::domain() && error.code() == WEBKIT_TIMEOUT {
        return LoadFailure::NoAnswer;
    }
    LoadFailure::Other
}

/// Put `card` in front of the view inside a widget [`page`] returned — the
/// window's unanswered card (#1557 item 2), replacing the one an earlier
/// failure put there.
///
/// The view stays in the stack underneath, so the page the operator had (if
/// the failure was a later navigation) is still there when the gateway
/// answers. A no-op for anything that is not a [`page`] widget, and while the
/// TLS card is up: that card is the operator's to act on, and nothing here
/// takes it down.
pub fn show_unanswered(page: &gtk::Widget, card: &gtk::Widget) {
    let Some(stack) = page.downcast_ref::<gtk::Stack>() else {
        return;
    };
    if stack.visible_child_name().as_deref() == Some(TLS_FAILED) {
        return;
    }
    if let Some(old) = stack.child_by_name(UNANSWERED) {
        stack.remove(&old);
    }
    stack.add_named(card, Some(UNANSWERED));
    stack.set_visible_child_name(UNANSWERED);
}

/// Whether a widget [`page`] returned is showing the unanswered card.
///
/// `None`-free and total, like [`view_of`].
#[must_use]
pub fn shows_unanswered(page: &gtk::Widget) -> bool {
    page.downcast_ref::<gtk::Stack>()
        .is_some_and(|stack| stack.visible_child_name().as_deref() == Some(UNANSWERED))
}

/// Load `uri` in the view inside a widget [`page`] returned — the window's
/// queued reload (#1557 item 2).
///
/// Through `load_uri`, so the request meets `decide-policy` like any other
/// navigation; `uri` is one [`reload_after`] already found on the hive's own
/// origin. Not while the TLS card is up: a reload there would only be handed
/// the same certificate, and that card is the operator's to act on.
pub fn reload(page: &gtk::Widget, uri: &str) {
    let Some(stack) = page.downcast_ref::<gtk::Stack>() else {
        return;
    };
    if stack.visible_child_name().as_deref() == Some(TLS_FAILED) {
        tracing::debug!(%uri, "not reloading behind the TLS card");
        return;
    }
    if let Some(view) = view_of(page) {
        view.load_uri(uri);
    }
}

/// What to pin and **under which spelling of the host**, or `None` when the
/// policy pins nothing.
///
/// # `WebKit` strips the brackets before it looks the pin up
///
/// This function exists because the obvious call — passing
/// [`crate::tls::host_of`]'s host straight through — silently produces a dead
/// pin for an IPv6 hive, on every route. `webkitgtk` 2.52.6 stores the
/// exception under the host **verbatim**
/// (`WebKitNetworkSession.cpp:477-486` → `WebsiteDataStore.cpp:1789-1792` →
/// `NetworkSessionSoup.cpp:128-131` → `SoupNetworkSession.cpp:341-344`), but
/// looks it up through `hostForComparison`
/// (`SoupNetworkSession.cpp:314-327`), whose own comment says why:
///
/// ```text
/// // If the host component of the URL is an IPv6 address, it will be
/// // surrounded by [ ] brackets. We have to remove them because they're part
/// // of the WTF::URL's host component … but not part of the host passed to
/// // allowSpecificHTTPSCertificateForHost.
/// ```
///
/// So `[::1]` goes in and `::1` is asked for, and the pin never applies. The
/// **bare** spelling is the one to store — the same one `GNetworkAddress`
/// wants, which is why this is [`crate::tls::identity_host`] and not a third
/// helper. The bracketed spelling stays for the card, where a human reads it.
///
/// # What the comparison actually is (#1242 review)
///
/// `HostTLSCertificateSet` (`SoupNetworkSession.cpp:68-97`) hashes the
/// certificate's **own DER** — the `"certificate"` property — with SHA-256,
/// not the chain. So route 2's leaf-only `VerifiedPem` and route 3's
/// leaf-plus-CA `gateway.pem` both match a gateway presenting a full chain,
/// and chain length is irrelevant to whether a pin takes. That is the one
/// thing that could have made the launch-time verify decorative, and it does
/// not.
///
/// # Case does not matter here, and that is `WebKit`'s doing
///
/// The October 2026 audit of this crate (M2) suspected the pin of the same
/// fault the navigation policy had — a hive URL spelled `Hive.Example`
/// storing its exception under a host `WebKit` would look up as
/// `hive.example`. It does not have it: `SoupNetworkSession.h` declares the
/// map `HashMap<String, HostTLSCertificateSet, ASCIICaseInsensitiveHash>`,
/// and `checkTLSErrors` looks it up through
/// `find<ASCIICaseInsensitiveStringViewHashTranslator>(hostForComparison(…))`
/// (read on `webkitglib/2.52` and on `main`). So the host goes in as the hive
/// spelled it, and only the brackets above are this function's business.
/// [`crate::page::navigable_in_place`] is where case did matter.
fn pin_for(policy: &TlsPolicy) -> Option<(&Pinned, &str)> {
    match policy {
        TlsPolicy::SystemStore => None,
        TlsPolicy::AllowCertificateForHost { cert, host } => {
            Some((cert, crate::tls::identity_host(host)))
        }
    }
}

/// The [`webkit::WebView`] inside a widget [`page`] returned.
///
/// `None` for anything else, so a caller cannot mistake another widget for the
/// view.
#[must_use]
pub fn view_of(widget: &gtk::Widget) -> Option<webkit::WebView> {
    widget
        .downcast_ref::<gtk::Stack>()?
        .child_by_name(PAGE)?
        .downcast::<webkit::WebView>()
        .ok()
}

/// What the embedded page is allowed to do, **stated** rather than inherited.
///
/// Every one of these is off because this window is a viewer for one page, not
/// a browser: there is no second window for a popup to become, no
/// operator-facing way to dismiss a modal dialog that blocks the whole
/// process, and no reason for a page served over https to reach a `data:` or
/// `file:` origin.
///
/// # These are pins, not changes — measured
///
/// **All six already default to `false`** in webkitgtk 2.52.6; a diagnostic
/// run of `Settings::new()` printed exactly that for each property
/// (2026-09-11, while fixing #1130 H1). So deleting any line here changes
/// nothing today, and `the_settings_are_the_ones_we_state` would **not** go
/// red for it — a fact that test's own doc states rather than implying a
/// falsification it does not have.
///
/// They are worth writing anyway, and worth asserting: the *effective* value
/// is what matters, and this window embeds content an agent's inputs can
/// influence. A default that flips in a future webkitgtk — or a `Settings`
/// built from somewhere else later — then reds the test instead of quietly
/// widening what that page may do. Stating a policy you already have is the
/// cheap half of keeping it.
fn settings() -> webkit::Settings {
    let s = webkit::Settings::new();
    // No popups: `create` is refused below anyway, and this stops the page
    // from trying rather than letting it try and silently fail.
    s.set_javascript_can_open_windows_automatically(false);
    // A modal dialog from an embedded page would be modal to a window whose
    // chrome is ours, with no way to tell the two apart.
    s.set_allow_modal_dialogs(false);
    // `data:` at the top level is an origin-laundering primitive, and nothing
    // the hive serves needs it.
    s.set_allow_top_navigation_to_data_urls(false);
    // The page is https; these two only ever widen a `file://` document, which
    // this window will not load in the first place.
    s.set_allow_file_access_from_file_urls(false);
    s.set_allow_universal_access_from_file_urls(false);
    // A swipe that navigates back inside a one-page viewer is a swipe that
    // leaves the page the window exists to show.
    s.set_enable_back_forward_navigation_gestures(false);
    s
}

/// The TLS failure state — filled in by [`failure_description`] when it
/// fires.
fn failure_state() -> adw::StatusPage {
    let page = adw::StatusPage::builder()
        .icon_name("channel-insecure-symbolic")
        .title("This hive's certificate could not be verified")
        .build();
    page.set_hexpand(true);
    page.set_vexpand(true);
    page
}

/// What the page slot shows **while the launch-time TLS probe runs** — #1246.
///
/// The same inline-state widget the failure card is (`failure_state`, an
/// `adw::StatusPage` in the same slot), so the window has one place where it
/// explains itself instead of the page and a second, differently-shaped
/// "loading" thing. Which one is showing is the whole difference between
/// "this is taking a while" and "this will not work", and a probe against a
/// dead hive ends at the second.
///
/// It deliberately names **no number**. The bound is
/// [`PROBE_DEADLINE`](crate::verify::PROBE_DEADLINE) on a launch, but the
/// budget is a parameter (`tls::resolve_route_within`), and a card that
/// printed "8.0s" while a test ran a 700 ms probe would be stating something
/// it does not know. What it can promise is what changed: the window is usable
/// while this is up, and a probe that runs out of time is replaced by the card
/// rather than by nothing.
///
/// `gtk::Spinner` rather than `adw::Spinner`: the latter is libadwaita 1.7 and
/// this crate declares `v1_4`, so using it would link a symbol the declared
/// floor does not have.
#[must_use]
pub fn verifying(host: &str) -> gtk::Widget {
    let page = adw::StatusPage::builder()
        .title("Verifying the hive's certificate…")
        .description(gtk::glib::markup_escape_text(&format!(
            "Checking {host} against the hive's own anchors before the page loads. The window \
             stays usable while this runs, and if the hive does not answer in time this card says \
             so instead."
        )))
        .build();
    let spinner = gtk::Spinner::new();
    spinner.start();
    spinner.set_size_request(32, 32);
    spinner.set_halign(gtk::Align::Center);
    page.set_child(Some(&spinner));
    page.set_widget_name(VERIFYING);
    page.set_hexpand(true);
    page.set_vexpand(true);
    page.upcast()
}

/// Whether `widget` is the state [`verifying`] built.
///
/// `None`-free and total, like [`view_of`]: a caller asks the widget it has
/// rather than remembering what it put there.
#[must_use]
pub fn is_verifying(widget: &gtk::Widget) -> bool {
    widget.widget_name() == VERIFYING
}

/// What the page slot shows when the launch-time probe **could not reach the
/// gateway** — [`crate::tls::Resolved::unreachable`] — while the window waits
/// to ask again.
///
/// # Why this is a card and not the page (the October 2026 audit, M1)
///
/// That verdict used to mount the page anyway, under the system store, so
/// `WebKit`'s own load could fail into the TLS card. Two things were wrong
/// with that. With the gateway still down, `WebKit`'s load fails on the
/// *connection*, not on TLS, so `load-failed-with-tls-errors` never fires and
/// the operator got the engine's generic error page rather than any card of
/// ours. And either way `page_loaded` latched, so the gateway coming back
/// changed nothing: the view had no pin, nothing re-probed, and the only way
/// out was to close the window. Now the page waits for a verdict the gateway
/// actually gave, and this card says why it is waiting.
///
/// `tried` is the sentence [`crate::tls::Resolved::tried`] carries — what the
/// probe dialled and what came back — and it opens the description the same
/// way [`crate::tls::failure_message`] opens its own (`This launch …`), so the
/// two cards read as one voice. `retry_in` is when the window will ask on its
/// own; it is printed as "about" because the timer is the main loop's, not a
/// stopwatch. `on_retry` is the button's — `Window::retry_now` — so an
/// operator who has just restarted nginx does not sit out a backoff step.
///
/// Escaped, like every other description in this file: `tried` carries a path
/// and the GIO error's own words, and neither is markup.
///
/// # The same card for a mounted page (#1557 item 2)
///
/// On the routes with no probe there is no verdict to wait for, so the page
/// mounts and `WebKit`'s own load is what finds the gateway down. The window
/// puts this card in front of the view then too ([`show_unanswered`]), with
/// `tried` naming the URI that got no answer and the engine's own words for
/// it, and its button skips the backoff of the queued reload instead of a
/// queued probe. One card for one condition — the gateway did not answer —
/// whichever step found it out.
#[must_use]
pub fn unreachable(
    tried: Option<&str>,
    retry_in: std::time::Duration,
    on_retry: impl Fn() + 'static,
) -> gtk::Widget {
    let attempt = tried.map_or_else(String::new, |t| format!("This launch {t}.\n\n"));
    let page = adw::StatusPage::builder()
        .icon_name("network-offline-symbolic")
        .title("The hive's gateway did not answer")
        .description(gtk::glib::markup_escape_text(&format!(
            "{attempt}The page is not loaded until the gateway answers. The window asks again in \
             about {} s, and loads the page the first time it does. The header above still \
             follows this agent's live status over host.sock, which does not go through the \
             gateway.",
            retry_in.as_secs().max(1)
        )))
        .build();
    let again = gtk::Button::builder()
        .label("Try again now")
        .halign(gtk::Align::Center)
        .build();
    again.add_css_class("pill");
    again.connect_clicked(move |_| on_retry());
    page.set_child(Some(&again));
    page.set_widget_name(UNREACHABLE);
    page.set_hexpand(true);
    page.set_vexpand(true);
    page.upcast()
}

/// Whether `widget` is the state [`unreachable()`] built.
///
/// `None`-free and total, like [`is_verifying`].
#[must_use]
pub fn is_unreachable(widget: &gtk::Widget) -> bool {
    widget.widget_name() == UNREACHABLE
}

/// What the page slot shows when the launch-time TLS probe's worker thread
/// died without ever producing a verdict — #1274 item 1, the other half of
/// `Window::begin_probe`'s `Err` arm alongside releasing the probe latch.
///
/// Before this, that arm cleared the latch and returned, leaving
/// [`verifying`] on screen: the spinner kept spinning and the sentence kept
/// promising a verdict that would never come, with the only evidence in the
/// journal. This card replaces it with a plain statement of what actually
/// happened and what the window does next — one scheduled retry, not a
/// promise the code cannot keep. `tls::resolve` is total, so a worker only
/// ends up here on a panic or an abort; naming that plainly is more honest
/// than a card that guesses at a cause it cannot know.
#[must_use]
pub fn probe_died(host: &str) -> gtk::Widget {
    let page = adw::StatusPage::builder()
        .icon_name("dialog-warning-symbolic")
        .title("The certificate check did not finish")
        .description(gtk::glib::markup_escape_text(&format!(
            "The background check of {host}'s certificate ended without an answer, so the page \
             was not loaded. The window will try again shortly; if that attempt fails the same \
             way, this card stays up."
        )))
        .build();
    page.set_widget_name(PROBE_DIED);
    page.set_hexpand(true);
    page.set_vexpand(true);
    page.upcast()
}

/// Whether `widget` is the state [`probe_died`] built.
///
/// `None`-free and total, like [`is_verifying`].
#[must_use]
pub fn is_probe_died(widget: &gtk::Widget) -> bool {
    widget.widget_name() == PROBE_DIED
}

/// Wire the navigation policy: [`decide`] answers, and this carries the
/// answer out — in place, `hand_off` for a refused link the operator clicked,
/// nothing at all for one nobody did.
///
/// Both `NavigationAction` (a click, a `window.location =`, a frame's own
/// load) and `NewWindowAction` (`target="_blank"`) come through
/// `decide-policy`; `create` is the one that would actually build a second
/// `WebView`, and it refuses by returning `None` — after handing a clicked
/// request off, because a new-window click that silently does nothing at all
/// is its own small lie.
///
/// # Each request is handed off at most once
///
/// A `NewWindowAction` is **always ignored** in `decide-policy`, because
/// [`decide`] never answers [`Decision::InPlace`] for one: `use` would only
/// lead `WebKit` on to `create`, which builds nothing. So a `_blank` link is
/// decided — and, if clicked, handed off — there, and never reaches `create`.
/// `create` still asks [`decide`] itself, for a request that arrives there
/// without passing through `decide-policy` first; either way one request is
/// one decision and at most one `hand_off`.
///
/// # Why `hand_off` is a parameter
///
/// [`page`] passes [`elsewhere`], and that is the **only** call site — nothing
/// in the tree passes anything else today (#1130 N5, which caught this doc
/// claiming otherwise).
///
/// It is a seam rather than a hard-coded call because **an ignored policy
/// decision is silent**: `WebKit` aborts the load and emits no `load-failed`,
/// no `load-changed`, nothing — and `WebViewExt::uri` is set by `load_uri`
/// *before* any decision, so it reads back the refused URI either way
/// (measured, both). So the day a web process can start where these tests run,
/// watching what this hands out is the only way to observe the real handler
/// through `WebKit`'s real dispatch; until then it costs one parameter and
/// nothing else. See `gtk_tests`' module doc for why that day is not today.
fn install_policy(view: &webkit::WebView, embedded: &str, hand_off: std::rc::Rc<dyn Fn(&str)>) {
    let origin = std::rc::Rc::<str>::from(embedded);
    let (policy_origin, policy_hand_off) =
        (std::rc::Rc::clone(&origin), std::rc::Rc::clone(&hand_off));
    view.connect_decide_policy(move |_, decision, kind| {
        use webkit::PolicyDecisionType as Kind;
        let target = match kind {
            Kind::NavigationAction => Target::ThisView,
            Kind::NewWindowAction => Target::NewWindow,
            // A `Response` decision is about how to handle a body we already
            // asked for, on a URI this handler has already admitted. Let the
            // default run.
            _ => return false,
        };
        let action = decision
            .downcast_ref::<webkit::NavigationPolicyDecision>()
            .and_then(webkit::NavigationPolicyDecision::navigation_action);
        let Some((uri, gesture)) = action.as_ref().and_then(|a| {
            let uri = a.request().and_then(|r| r.uri())?;
            Some((uri, a.is_user_gesture()))
        }) else {
            // No URI to judge: refuse rather than guess. `decide-policy` is
            // the only gate, so "unknown" has to mean "not here".
            tracing::warn!("a navigation with no URI was refused");
            decision.ignore();
            return true;
        };

        let answer = decide(&policy_origin, &uri, target, gesture);
        if answer == Decision::InPlace {
            return false;
        }
        decision.ignore();
        carry_out(answer, &uri, &*policy_hand_off);
        true
    });

    view.connect_create(move |_, action| {
        if let Some(uri) = action.request().and_then(|r| r.uri()) {
            let answer = decide(&origin, &uri, Target::NewWindow, action.is_user_gesture());
            carry_out(answer, &uri, &*hand_off);
        }
        // Never a second `WebView`: this window is one agent's viewer, and a
        // popup inside it would carry this window's chrome and none of its
        // meaning.
        None
    });
}

/// Do what a refusing [`Decision`] says, with the one log line it gets.
///
/// [`Decision::InPlace`] is a no-op here: letting `WebKit` load is the
/// *absence* of a refusal, which the `decide-policy` handler expresses by
/// returning `false` before it ever gets here.
fn carry_out(answer: Decision, uri: &str, hand_off: &dyn Fn(&str)) {
    match answer {
        Decision::InPlace => {}
        Decision::Desktop => {
            tracing::info!(
                %uri,
                "a clicked link that cannot open in this window — handing it to the desktop's \
                 default handler"
            );
            hand_off(uri);
        }
        // Debug, not warn: a page that tries this does so on a loop, and the
        // point of refusing it is that it achieves nothing — including a
        // journal full of it.
        Decision::Refuse(Refusal::NoGesture) => tracing::debug!(
            %uri,
            "refused: nobody clicked this, and it cannot load in this window (a script, a \
             redirect, a frame's own load) — it goes nowhere"
        ),
        Decision::Refuse(Refusal::Scheme) => {
            tracing::warn!(%uri, "refused: only http(s) links leave this window");
        }
    }
}

/// Hand a URI to the desktop, so a refused link still goes somewhere.
///
/// [`may_leave`] asks again here even though [`decide`] already did: this is
/// the one place a hostile page's string reaches a launcher, so it carries
/// the scheme test itself rather than trusting every caller to have run it.
fn elsewhere(uri: &str) {
    if !may_leave(uri) {
        tracing::warn!(%uri, "refused: only http(s) links leave this window");
        return;
    }
    if let Err(e) =
        gtk::gio::AppInfo::launch_default_for_uri(uri, None::<&gtk::gio::AppLaunchContext>)
    {
        tracing::warn!(%uri, error = %e, "the desktop could not open it");
    }
}

#[cfg(test)]
mod tests {
    use super::{WEBKIT_TIMEOUT, elsewhere, load_failure, pin_for};
    use crate::page::LoadFailure;
    use crate::tls::{Pinned, TlsPolicy};
    use gtk::gio::{IOErrorEnum, ResolverError, TlsError};
    use gtk::glib;
    use std::path::PathBuf;

    /// **Which load failures mean "nothing answered"** — #1557 item 2's
    /// classifier, against `GError`s in the shapes `WebKit` hands
    /// `load-failed` (domains and codes read in webkitgtk 2.54.1's source;
    /// see [`load_failure`]'s docs for where each comes from).
    ///
    /// A gateway that is down answers `NoAnswer`: refused, unreachable, a
    /// name that did not resolve, a timeout of `GLib`'s or of `WebKit`'s own
    /// (`WebKitNetworkError` -1001, which no enum value names). A certificate
    /// answers `Tls`, and so does **anything** once the TLS card is up. The
    /// rest — a cancellation, a policy decision, a redirect loop, an error
    /// this has never seen — answers `Other`, because an allow-list is what
    /// keeps a guess from turning into a reload loop.
    ///
    /// Mutations (run this round, red): drop the `tls_card_up` term and the
    /// card-up row reads `NoAnswer`; return `NoAnswer` for every
    /// `IOErrorEnum` and the `Cancelled` row does; drop the
    /// [`WEBKIT_TIMEOUT`] arm and `WebKit`'s own timeout reads `Other`.
    #[test]
    fn a_load_nothing_answered_is_told_from_one_that_was_refused() {
        let webkit_timeout = glib::Error::with_domain(
            <webkit::NetworkError as glib::error::ErrorDomain>::domain(),
            WEBKIT_TIMEOUT,
            "Load request timed out",
        );
        for (error, expected) in [
            (
                glib::Error::new(IOErrorEnum::ConnectionRefused, "Connection refused"),
                LoadFailure::NoAnswer,
            ),
            (
                glib::Error::new(IOErrorEnum::HostUnreachable, "No route to host"),
                LoadFailure::NoAnswer,
            ),
            (
                glib::Error::new(IOErrorEnum::NetworkUnreachable, "Network is unreachable"),
                LoadFailure::NoAnswer,
            ),
            (
                glib::Error::new(IOErrorEnum::TimedOut, "Socket I/O timed out"),
                LoadFailure::NoAnswer,
            ),
            (
                glib::Error::new(ResolverError::NotFound, "Name not known"),
                LoadFailure::NoAnswer,
            ),
            (
                glib::Error::new(ResolverError::TemporaryFailure, "Temporary failure"),
                LoadFailure::NoAnswer,
            ),
            (webkit_timeout, LoadFailure::NoAnswer),
            (
                glib::Error::new(TlsError::BadCertificate, "Unacceptable TLS certificate"),
                LoadFailure::Tls,
            ),
            (
                glib::Error::new(webkit::NetworkError::Cancelled, "Load request cancelled"),
                LoadFailure::Other,
            ),
            (
                glib::Error::new(
                    webkit::PolicyError::FrameLoadInterruptedByPolicyChange,
                    "Frame load was interrupted",
                ),
                LoadFailure::Other,
            ),
            (
                glib::Error::new(IOErrorEnum::Cancelled, "Operation was cancelled"),
                LoadFailure::Other,
            ),
            (
                glib::Error::with_domain(
                    glib::Quark::from_str("soup-session-error-quark"),
                    3,
                    "Too many redirects",
                ),
                LoadFailure::Other,
            ),
            (
                glib::Error::new(ResolverError::Internal, "Internal resolver error"),
                LoadFailure::Other,
            ),
        ] {
            assert_eq!(load_failure(&error, false), expected, "{error:?}");
        }
        assert_eq!(
            load_failure(
                &glib::Error::new(IOErrorEnum::ConnectionRefused, "Connection refused"),
                true
            ),
            LoadFailure::Tls,
            "once the TLS card is up, every failure is the card's"
        );
    }

    /// **An IPv6 pin is stored under the spelling `WebKit` looks it up by** —
    /// the bare literal, not the bracketed one (#1242 review, finding 3;
    /// `SoupNetworkSession.cpp`'s `hostForComparison`, quoted in
    /// [`pin_for`]'s docs).
    ///
    /// Before this, `[::1]` went in and `::1` was asked for, so the pin was
    /// dead on **every** route for an IPv6 hive and the failure card was all
    /// such a hive ever got — silently, since nothing in `WebKit` reports an
    /// exception that is never consulted.
    ///
    /// Mutation (re-run this round, red): drop `identity_host` from
    /// [`pin_for`] and hand the host through unchanged — the spelling this
    /// PR shipped at first — and the two literal rows red.
    ///
    /// [`pin_for`]: super::pin_for
    #[test]
    fn an_ipv6_pin_is_stored_under_the_spelling_webkit_looks_it_up_by() {
        let policy = |host: &str| TlsPolicy::AllowCertificateForHost {
            cert: Pinned::File(PathBuf::from("/tmp/hive-gateway.pem")),
            host: host.to_owned(),
        };
        for (stored, expected) in [
            ("[::1]", "::1"),
            ("[fd00::1]", "fd00::1"),
            ("hive.local", "hive.local"),
            ("127.0.0.1", "127.0.0.1"),
        ] {
            let p = policy(stored);
            let (_, pinned_under) = pin_for(&p).expect("this policy pins something");
            assert_eq!(pinned_under, expected, "stored as {stored}");
        }
        assert!(
            pin_for(&TlsPolicy::SystemStore).is_none(),
            "the system store pins nothing, so there is no host to spell"
        );
    }

    /// A non-http(s) scheme never reaches a launcher.
    ///
    /// This is the one place a string the embedded page chose is handed to
    /// something that runs a program, so the scheme test lives here as well as
    /// in `navigable_in_place`. It cannot assert "nothing launched" without a
    /// desktop; what it can assert is that the call is total — no panic, no
    /// early return skipped — and the display test below covers the refusal.
    #[test]
    fn a_non_http_scheme_is_refused_without_panicking() {
        for uri in ["", "file:///etc/passwd", "javascript:alert(1)", "data:,x"] {
            elsewhere(uri);
        }
    }
}

#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use super::{PAGE, TLS_FAILED, page, settings, view_of};
    use crate::tls::Resolved;
    use std::cell::Cell;
    use std::rc::Rc;
    use webkit::prelude::*;

    /// # Why there is no end-to-end "the view refused it" test here
    ///
    /// The #1130 review supplied one — `load_uri` a foreign origin, iterate the
    /// main context, assert the view did not take it — and it cannot run in any
    /// environment this repo's CI has. Measured, in this container and with the
    /// same constraints the nix build sandbox imposes:
    ///
    /// - With `WebKit`'s sandbox on, creating a `WebView` **aborts the whole test
    ///   binary**: `bwrap: Can't mount proc on /newroot/proc: Operation not
    ///   permitted`, then `Failed to fully launch dbus-proxy`, SIGABRT. Nested
    ///   user namespaces are not available here.
    /// - With `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1` (which
    ///   `nix/checks/system-tests.nix` sets for the test derivation, so the
    ///   suite survives at all) the view is constructed — and the web process
    ///   still dies: a diagnostic run recorded exactly one signal,
    ///   `web-process-terminated: Crashed`, with no `load-changed` and no
    ///   `load-failed`, over a 20 s blocking main-loop budget.
    ///
    /// No web process means no navigation, which means `decide-policy` is never
    /// dispatched, which means the handler cannot be observed firing. The
    /// reviewer's own note that "`decide-policy` runs before any request, so
    /// this needs no network" is right about the *network* and does not save
    /// it: the decision is still made by a process that cannot start.
    ///
    /// (The **network** process is a different story, and it does dial —
    /// `window.rs`'s `gtk_tests::a_second_activation_during_the_probe_opens_no_second_connection`
    /// asserts it reaching a fixture gateway once, strictly pre-mount (its
    /// `seen` snapshot is only ever taken while the probe is still in
    /// flight); that test's own doc additionally records, measured under
    /// mutation, a second connection once the page mounts — `WebKit`'s
    /// network process opening the embedded view's `load_uri`, #1274 N3.
    /// That process carries no navigation policy of its own; it is
    /// `decide-policy`, dispatched on the *web* process above, that this file
    /// cannot observe.)
    ///
    /// (`WebViewExt::uri` is no substitute either, and that is worth recording
    /// separately: it is set by `load_uri` *before* any decision, so it reads
    /// back a refused URI unchanged. Measured — it was the first shape tried.)
    ///
    /// So the H1 evidence is split, with nothing claimed that was not run:
    ///
    /// | claim | where |
    /// | --- | --- |
    /// | which URIs may load in place | `page::navigable_in_place`'s tests — hermetic, and the mutation reds |
    /// | which refusals reach the desktop (a click on http(s), nothing else) | `page::decide`'s tests — hermetic, and the mutations red |
    /// | the page cannot open windows or reach `data:`/`file:` | [`the_settings_are_the_ones_we_state`] below, on the real `Settings` object |
    /// | the handler is installed, refuses through `WebKit`'s dispatch, and `is_user_gesture` reads true for a click and false for a frame or a script | **live-verify** (`docs/live-verify.md`, "The view stays on the hive" and "Nothing opens without a click") — it needs a machine with a working web process |
    ///
    /// [`the_settings_are_the_ones_we_state`]: gtk_tests::the_settings_are_the_ones_we_state
    /// **The page's permissions are the ones this file states**, read back off
    /// the real `Settings` object rather than trusted to the constructor.
    ///
    /// This is the half of H1 that is observable without a web process.
    ///
    /// **Deleting a `set_*` call in `settings()` does NOT red this** — measured
    /// (the `allow-modal-dialogs` line was removed and the suite stayed green),
    /// because all six of these already default to `false` in webkitgtk 2.52.6.
    /// Saying so is the point: the alternative was a doc comment claiming a
    /// falsification it does not have, which is one of the things #1130's
    /// review was about.
    ///
    /// What it *does* catch is an upstream default flipping, or a `Settings`
    /// arriving from somewhere that does not state them — either of which
    /// would widen what an agent-output page may do inside chrome that names
    /// an agent, with nothing else in the tree to notice.
    #[gtk::test]
    fn the_settings_are_the_ones_we_state() {
        use gtk::glib::object::ObjectExt as _;
        let s = settings();
        // Four of these have a `set_*` in the bindings but no generated
        // getter, so they are read back off the GObject property — the same
        // value, one layer down.
        let off = |name: &str| {
            assert!(
                !s.property::<bool>(name),
                "{name} must be off: a viewer for one page is not a browser"
            );
        };
        assert!(
            !s.is_javascript_can_open_windows_automatically(),
            "a viewer for one page has no second window for a popup to become"
        );
        off("allow-modal-dialogs");
        off("allow-top-navigation-to-data-urls");
        off("allow-file-access-from-file-urls");
        off("allow-universal-access-from-file-urls");
        assert!(
            !s.enables_back_forward_navigation_gestures(),
            "a swipe that navigates back leaves the page this window exists to show"
        );
    }

    /// …and the view reads back those values too.
    ///
    /// **This does not pin the wiring, and saying so is the point** (#1130
    /// N3): dropping `.settings(&settings())` from the builder in `page`
    /// leaves it **green** — measured by the re-verification — for the same
    /// reason the sibling above cannot be falsified, which is that all six
    /// properties already default to `false`, so a view built with no
    /// `Settings` at all reads back identically.
    ///
    /// It is kept as the other half of the same pin: if an upstream default
    /// flips, this reds whether the cause is the constructor or the builder.
    /// The wiring itself is genuinely unpinned here, and there is no honest
    /// way to pin it until a `Settings` value diverges from `WebKit`'s
    /// defaults.
    #[gtk::test]
    fn the_view_carries_those_settings() {
        use gtk::glib::object::ObjectExt as _;
        let w = page(
            "https://hive.local/agent/stray/",
            &Resolved::default(),
            |_| {},
        );
        let view = view_of(&w).expect("the page widget carries the view");
        let s =
            webkit::prelude::WebViewExt::settings(&view).expect("the view was built with settings");
        assert!(!s.is_javascript_can_open_windows_automatically());
        assert!(!s.property::<bool>("allow-modal-dialogs"));
        assert!(!s.property::<bool>("allow-top-navigation-to-data-urls"));
    }

    /// The page widget is a stack whose visible child is the view until TLS
    /// fails — i.e. the error state exists and is *not* what the operator sees
    /// on a normal open.
    #[gtk::test]
    fn the_page_starts_on_the_view_not_on_the_error_state() {
        let w = page(
            "https://hive.local/agent/stray/",
            &Resolved::default(),
            |_| {},
        );
        let stack = w
            .downcast_ref::<gtk::Stack>()
            .expect("the page widget is a stack");
        assert_eq!(stack.visible_child_name().as_deref(), Some(PAGE));
        assert!(
            stack.child_by_name(TLS_FAILED).is_some(),
            "the failure state is built up front, so a TLS error has somewhere to go"
        );
    }

    /// What [`page`] hands its `gateway` callback, collected.
    type Heard = std::rc::Rc<std::cell::RefCell<Vec<super::Gateway>>>;

    /// A page on `url` whose reports land in the returned log, and a counter
    /// of the `load-failed` emissions that got **past** this file's handler.
    ///
    /// The counter is a second handler connected after `page` connected its
    /// own: `load-failed` stops at the first handler that returns `true`
    /// (`g_signal_accumulator_true_handled`), so it moves exactly when ours
    /// declined the failure — which is when `WebKit`'s own class handler
    /// would draw its error page. The emission's return value cannot say
    /// that: the class handler answers `true` itself after drawing one.
    fn listening_page(url: &str) -> (gtk::Widget, webkit::WebView, Heard, Rc<Cell<u32>>) {
        let heard: Heard = std::rc::Rc::default();
        let log = std::rc::Rc::clone(&heard);
        let w = page(url, &Resolved::default(), move |event| {
            log.borrow_mut().push(event);
        });
        let view = view_of(&w).expect("the page widget carries the view");
        let declined = Rc::new(Cell::new(0_u32));
        let count = Rc::clone(&declined);
        view.connect_load_failed(move |_, _, _, _| {
            count.set(count.get() + 1);
            // Declined here too, so the emission carries on to `WebKit`'s
            // own handler exactly as it would without this probe.
            false
        });
        (w, view, heard, declined)
    }

    /// Emit `load-failed` on `view` the way `WebKit` does, and answer whether
    /// this file's handler **took** it (so `WebKit` draws no error page).
    ///
    /// No web process runs here (this module's doc), so `WebKit` itself never
    /// emits it; `GLib`'s own dispatch reaches the very closure
    /// `watch_loads` connected, which is the wiring under test.
    fn fail(
        view: &webkit::WebView,
        declined: &Cell<u32>,
        uri: &str,
        error: &gtk::glib::Error,
    ) -> bool {
        use gtk::glib::object::ObjectExt as _;
        let before = declined.get();
        let _: bool = view.emit_by_name("load-failed", &[&webkit::LoadEvent::Started, &uri, error]);
        declined.get() == before
    }

    /// **A load of the hive's page that nothing answered reaches the window,
    /// and the card comes down when one commits** — #1557 item 2, the
    /// signal half (`window.rs`'s
    /// `an_unanswered_page_is_reloaded_on_the_shared_backoff` is the
    /// window's).
    ///
    /// Mutations (run this round, red): make the `load-failed` handler return
    /// `false` without reporting — no general handler, the bug as it stood —
    /// and the first assertion reds, the failure going on to `WebKit`'s own
    /// error page with nothing reported; drop the `load-changed` flip and the
    /// card stays up over a page that answered.
    #[gtk::test]
    fn a_hive_page_nothing_answered_is_reported_and_its_card_lifts_on_commit() {
        let url = "https://hive.local/agent/stray/?hide=header";
        let (w, view, heard, declined) = listening_page(url);
        let refused = gtk::glib::Error::new(
            gtk::gio::IOErrorEnum::ConnectionRefused,
            "Could not connect to hive.local: Connection refused",
        );

        assert!(
            fail(&view, &declined, url, &refused),
            "the window takes this failure — WebKit draws no error page over the card"
        );
        assert_eq!(
            heard.borrow().as_slice(),
            [super::Gateway::Unanswered {
                uri: url.to_owned(),
                why: "Could not connect to hive.local: Connection refused".to_owned(),
            }]
        );

        super::show_unanswered(&w, &gtk::Label::new(Some("card")).upcast());
        assert!(super::shows_unanswered(&w));

        {
            use gtk::glib::object::ObjectExt as _;
            view.emit_by_name::<()>("load-changed", &[&webkit::LoadEvent::Committed]);
        }
        assert!(
            !super::shows_unanswered(&w),
            "something answered: the page is back"
        );
        let stack = w.downcast_ref::<gtk::Stack>().expect("a stack");
        assert_eq!(stack.visible_child_name().as_deref(), Some(PAGE));
        assert_eq!(heard.borrow().last(), Some(&super::Gateway::Answered));

        // A commit with no card up is the ordinary case and says nothing.
        let before = heard.borrow().len();
        {
            use gtk::glib::object::ObjectExt as _;
            view.emit_by_name::<()>("load-changed", &[&webkit::LoadEvent::Committed]);
        }
        assert_eq!(heard.borrow().len(), before);
    }

    /// …and **nothing else reaches it**: a failure off the hive, a
    /// certificate failure (by its error, or because the TLS card is already
    /// up), and a cancellation all go `WebKit`'s own way, as every failure did
    /// before #1557 — and behind the TLS card neither a new card nor a reload
    /// gets in.
    ///
    /// Mutations (run this round, red): drop `load_failure`'s `tls_card_up`
    /// term and the TLS-card row reports; reload regardless of the TLS card
    /// in [`super::reload`] and the view's URI moves behind it.
    #[gtk::test]
    fn only_the_hives_own_unanswered_load_is_reported() {
        use gtk::gio::{IOErrorEnum, TlsError};
        let url = "https://hive.local/agent/stray/?hide=header";
        let (w, view, heard, declined) = listening_page(url);
        let refused = gtk::glib::Error::new(IOErrorEnum::ConnectionRefused, "refused");

        assert!(
            !fail(&view, &declined, "https://evil.example/", &refused),
            "off the hive"
        );
        assert!(
            !fail(
                &view,
                &declined,
                url,
                &gtk::glib::Error::new(TlsError::BadCertificate, "bad certificate")
            ),
            "a certificate failure is the TLS card's"
        );
        assert!(
            !fail(
                &view,
                &declined,
                url,
                &gtk::glib::Error::new(webkit::NetworkError::Cancelled, "cancelled")
            ),
            "a cancellation is a newer load or the policy, not an outage"
        );
        assert!(heard.borrow().is_empty(), "{:?}", heard.borrow());

        let stack = w.downcast_ref::<gtk::Stack>().expect("a stack");
        stack.set_visible_child_name(TLS_FAILED);
        assert!(
            !fail(&view, &declined, url, &refused),
            "behind the TLS card even a refused connection takes the card's route"
        );
        assert!(heard.borrow().is_empty(), "{:?}", heard.borrow());

        super::show_unanswered(&w, &gtk::Label::new(Some("card")).upcast());
        assert_eq!(
            stack.visible_child_name().as_deref(),
            Some(TLS_FAILED),
            "nothing takes the TLS card down"
        );
        let before = view.uri();
        super::reload(&w, "https://hive.local/agent/stray/turn/3");
        assert_eq!(view.uri(), before, "and nothing reloads behind it");
    }

    /// The session is **ephemeral**: no cookies, cache or `IndexedDB` left under
    /// `$XDG_{DATA,CACHE}_HOME` after the window closes (#1130 L6).
    ///
    /// Mutation (re-run this round, red): go back to
    /// `NetworkSession::default()` — the persistent one — and this reds.
    #[gtk::test]
    fn the_network_session_is_ephemeral() {
        let w = page(
            "https://hive.local/agent/stray/",
            &Resolved::default(),
            |_| {},
        );
        let view = view_of(&w).expect("the page widget carries the view");
        assert!(
            view.network_session()
                .expect("the view was built with a session")
                .is_ephemeral(),
            "a window opened to look at an agent must not hoard that agent's storage"
        );
    }
}
