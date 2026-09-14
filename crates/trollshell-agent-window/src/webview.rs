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
//! open windows: [`crate::page::navigable_in_place`] decides, `decide-policy`
//! enforces, and everything refused is handed to the desktop's default handler
//! so a legitimate link in the agent's feed still works — in a browser, where
//! the operator can see where they are. See `navigable_in_place`'s docs for
//! the failure this prevents.

use adw::prelude::*;
use webkit::prelude::*;

use crate::page::navigable_in_place;
use crate::tls::{CERT_ENV, Pinned, Resolved, TlsPolicy, failure_description};

/// The `Stack` child names — the page, and the TLS failure state that replaces
/// it.
const PAGE: &str = "page";
const TLS_FAILED: &str = "tls-failed";

/// The widget name [`verifying`] stamps on its state, and the whole of how
/// [`is_verifying`] recognises it.
///
/// A name rather than a type test: the failure card is an `adw::StatusPage`
/// too, so `downcast_ref::<adw::StatusPage>()` would answer yes to both, and
/// the one thing a caller ever wants to know here is *which* of the two the
/// page slot is showing.
const VERIFYING: &str = "tls-verifying";

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
#[must_use]
pub fn page(url: &str, trust: &Resolved) -> gtk::Widget {
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
        let host = crate::tls::host_of(failing_uri).unwrap_or(failing_uri);
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
        error.set_description(Some(&failure_description(host, tried.as_deref())));
        sink.set_visible_child_name(TLS_FAILED);
        false
    });

    view.load_uri(url);
    stack.upcast()
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
/// The same inline-state widget the failure card is ([`failure_state`], an
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

/// Wire the navigation policy: in-place for the agent's own origin,
/// `on_refuse` for everything else.
///
/// Both `NavigationAction` (a click or a `window.location =`) and
/// `NewWindowAction` (`target="_blank"`) come through `decide-policy`;
/// `create` is the one that would actually build a second `WebView`, and it
/// refuses by returning `None`. Without the `create` handler a `_blank` link
/// silently does nothing at all, which is its own small lie.
///
/// # Why `on_refuse` is a parameter
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
/// nothing else. See [`gtk_tests`]' module doc for why that day is not today.
fn install_policy(view: &webkit::WebView, embedded: &str, on_refuse: std::rc::Rc<dyn Fn(&str)>) {
    let origin = embedded.to_owned();
    let refuse = std::rc::Rc::clone(&on_refuse);
    view.connect_decide_policy(move |_, decision, kind| {
        use webkit::PolicyDecisionType as Kind;
        let action = match kind {
            Kind::NavigationAction | Kind::NewWindowAction => decision
                .downcast_ref::<webkit::NavigationPolicyDecision>()
                .and_then(webkit::NavigationPolicyDecision::navigation_action),
            // A `Response` decision is about how to handle a body we already
            // asked for, on a URI this handler has already admitted. Let the
            // default run.
            _ => return false,
        };
        let Some(uri) = action
            .as_ref()
            .and_then(webkit::NavigationAction::request)
            .and_then(|r| r.uri())
        else {
            // No URI to judge: refuse rather than guess. `decide-policy` is
            // the only gate, so "unknown" has to mean "not here".
            tracing::warn!("a navigation with no URI was refused");
            decision.ignore();
            return true;
        };

        if navigable_in_place(&origin, &uri) {
            return false;
        }
        tracing::info!(
            %uri,
            "not this agent's origin — handing it to the desktop's default handler"
        );
        decision.ignore();
        refuse(&uri);
        true
    });

    view.connect_create(move |_, action| {
        if let Some(uri) = action.request().and_then(|r| r.uri()) {
            tracing::info!(%uri, "a page asked for a new window — opening it elsewhere");
            on_refuse(&uri);
        }
        // Never a second `WebView`: this window is one agent's viewer, and a
        // popup inside it would carry this window's chrome and none of its
        // meaning.
        None
    });
}

/// Hand a URI to the desktop, so a refused link still goes somewhere.
///
/// `http`/`https` only — the same three-scheme judgement the shell's own
/// `OpenUri` broker makes, minus `file:`, because nothing in an agent's turn
/// stream has a reason to open a local path and this is the one place a
/// hostile page's string reaches a launcher.
fn elsewhere(uri: &str) {
    if !(uri.starts_with("https://") || uri.starts_with("http://")) {
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
    use super::{elsewhere, pin_for};
    use crate::tls::{Pinned, TlsPolicy};
    use std::path::PathBuf;

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
    /// records it reaching a fixture gateway once the page mounts, #1274 N3.
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
    /// | the page cannot open windows or reach `data:`/`file:` | [`the_settings_are_the_ones_we_state`] below, on the real `Settings` object |
    /// | the handler is installed and refuses through `WebKit`'s dispatch | **live-verify** (`docs/live-verify.md`, "The view stays on the hive") — it needs a machine with a working web process |
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
        let w = page("https://hive.local/agent/stray/", &Resolved::default());
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
        let w = page("https://hive.local/agent/stray/", &Resolved::default());
        let stack = w
            .downcast_ref::<gtk::Stack>()
            .expect("the page widget is a stack");
        assert_eq!(stack.visible_child_name().as_deref(), Some(PAGE));
        assert!(
            stack.child_by_name(TLS_FAILED).is_some(),
            "the failure state is built up front, so a TLS error has somewhere to go"
        );
    }

    /// The session is **ephemeral**: no cookies, cache or `IndexedDB` left under
    /// `$XDG_{DATA,CACHE}_HOME` after the window closes (#1130 L6).
    ///
    /// Mutation (re-run this round, red): go back to
    /// `NetworkSession::default()` — the persistent one — and this reds.
    #[gtk::test]
    fn the_network_session_is_ephemeral() {
        let w = page("https://hive.local/agent/stray/", &Resolved::default());
        let view = view_of(&w).expect("the page widget carries the view");
        assert!(
            view.network_session()
                .expect("the view was built with a session")
                .is_ephemeral(),
            "a window opened to look at an agent must not hoard that agent's storage"
        );
    }
}
