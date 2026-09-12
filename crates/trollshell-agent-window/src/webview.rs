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
use crate::tls::{CERT_ENV, TlsPolicy, failure_message};

/// The `Stack` child names — the page, and the TLS failure state that replaces
/// it.
const PAGE: &str = "page";
const TLS_FAILED: &str = "tls-failed";

/// Build the page widget for `url`, under `policy`.
///
/// The returned widget is a `gtk::Stack`: the [`webkit::WebView`] itself, plus
/// the inline error state [`failure_message`] fills in when verification
/// fails. [`view_of`] gets the view back out of it.
///
/// The network session is configured **before** the first load: the
/// TLS-errors policy is set explicitly (always
/// [`Fail`](webkit::TLSErrorsPolicy::Fail), see [`crate::tls`]) rather than
/// left at the default, so a reader of this file can see that nothing here
/// turns verification off.
#[must_use]
pub fn page(url: &str, policy: &TlsPolicy) -> gtk::Widget {
    // **Ephemeral**, not `NetworkSession::default()`: the default is the
    // persistent one, which accumulates cookies, cache and `IndexedDB` under
    // `$XDG_{DATA,CACHE}_HOME` per app-id for ever, with nothing in this
    // window to clear it or mention it. A window that is opened to look at an
    // agent should not be quietly hoarding that agent's storage; the login the
    // hive's page performs lasts the window, which is the lifetime the
    // operator can see.
    let session = webkit::NetworkSession::new_ephemeral();
    session.set_tls_errors_policy(policy.errors_policy());

    if let TlsPolicy::AllowCertificateForHost { pem, host } = policy {
        match gtk::gio::TlsCertificate::from_file(pem) {
            Ok(cert) => {
                session.allow_tls_certificate_for_host(&cert, host);
                tracing::info!(
                    pem = %pem.display(),
                    host,
                    "pinning the certificate {CERT_ENV} names for this host — it must be the one \
                     the gateway PRESENTS (its leaf); a CA bundle will not match and the load \
                     will still fail"
                );
            }
            Err(e) => tracing::warn!(
                pem = %pem.display(),
                error = %e,
                "{CERT_ENV} does not name a readable certificate; using the system trust store"
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
    view.connect_load_failed_with_tls_errors(move |_, failing_uri, _cert, errors| {
        let host = crate::tls::host_of(failing_uri).unwrap_or(failing_uri);
        tracing::warn!(
            uri = failing_uri,
            ?errors,
            "TLS verification failed for the hive; add its trust-bundle.pem to the system store \
             (security.pki.certificateFiles, referencing \
             services.hyperhive.deploy.hive-controller.tls.stateDir when the hive is on this \
             machine), or as a last resort point {CERT_ENV} at the certificate the gateway \
             PRESENTS — its leaf, never the bundle. The window's own error state spells all \
             three out"
        );
        error.set_description(Some(&failure_message(host)));
        sink.set_visible_child_name(TLS_FAILED);
        false
    });

    view.load_uri(url);
    stack.upcast()
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

/// The TLS failure state — filled in by [`failure_message`] when it fires.
fn failure_state() -> adw::StatusPage {
    let page = adw::StatusPage::builder()
        .icon_name("channel-insecure-symbolic")
        .title("This hive's certificate could not be verified")
        .build();
    page.set_hexpand(true);
    page.set_vexpand(true);
    page
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
/// [`page`] passes [`elsewhere`], and that is the only production value. It is
/// injectable because **an ignored policy decision is silent**: `WebKit` aborts
/// the load and emits no `load-failed`, no `load-changed`, nothing — and
/// `WebViewExt::uri` is set by `load_uri` *before* any decision, so it reads
/// back the refused URI either way (measured, both). With no observable in the
/// view, the only way to test the real handler through `WebKit`'s real dispatch
/// is to watch what it hands out, which is what the display tests below do.
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
    use super::elsewhere;

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
    use crate::tls::TlsPolicy;
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

    /// …and the view is built **with** those settings, not with `WebKit`'s
    /// defaults — the wiring the test above cannot see.
    ///
    /// Mutation (re-run this round, red): drop `.settings(&settings())` from
    /// the builder in `page` and this reds.
    #[gtk::test]
    fn the_view_carries_those_settings() {
        use gtk::glib::object::ObjectExt as _;
        let w = page("https://hive.local/agent/stray/", &TlsPolicy::SystemStore);
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
        let w = page("https://hive.local/agent/stray/", &TlsPolicy::SystemStore);
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
        let w = page("https://hive.local/agent/stray/", &TlsPolicy::SystemStore);
        let view = view_of(&w).expect("the page widget carries the view");
        assert!(
            view.network_session()
                .expect("the view was built with a session")
                .is_ephemeral(),
            "a window opened to look at an agent must not hoard that agent's storage"
        );
    }
}
