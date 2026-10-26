//! **The navigation policy on `WebKit`'s own dispatch**, against pages a
//! loopback server really serves — #1584, the half of #1488's H1 that
//! `page::decide`'s pure tests cannot reach: that `decide-policy` is asked at
//! all, that what [`decide`](crate::page::decide) refuses is really cancelled
//! in the view, and that `is_user_gesture` reads the way that rule assumes.
//!
//! # Where this runs, and where it skips
//!
//! A `WebView` needs a web process, and the web process needs EGL. Measured
//! for #1584 against the mesa 26.2.1 the flake links:
//!
//! - **A bare devShell `xvfb-run -a`**: the web process aborts at once
//!   (`Could not create default EGL display: EGL_BAD_PARAMETER. Aborting...`)
//!   and the view reports `web-process-terminated: Crashed`. Nothing loads, and
//!   `decide-policy` is never dispatched, not even for the view's own first
//!   `load_uri`. Every test here then **skips**, and says so.
//! - **`checks.system-tests`** (`nix/checks/system-tests.nix`): that check
//!   exports llvmpipe's EGL vendor for `hytte-ui`'s GL tests (#1036), and with
//!   it the web process starts. Run locally with the same four variables and
//!   the same mesa store path, `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1`,
//!   `GIO_EXTRA_MODULES`, and a network namespace with only loopback up (the
//!   closest stand-in for the build sandbox), a view loaded a page from a
//!   loopback HTTPS server and `decide-policy` fired for the main frame, a
//!   subframe, a meta refresh, a scripted `location =` and a link click. Each
//!   of them read `is_user_gesture` false except the click.
//!
//! The skip is gated on `TROLLSHELL_REQUIRE_GL`, not on a variable of its own.
//! Whether the web process starts is whether EGL finds a vendor, which is what
//! that variable already asserts for the GL tests, and the check that exports
//! the vendor also exports the variable. So in CI a web process that does not
//! start **fails** these tests, naming why, instead of a skip that reads as a
//! pass.
//!
//! # The rig
//!
//! - **Two loopback HTTPS servers** on `verify.rs`'s TLS fixtures: the hive at
//!   [`HIVE`], and "elsewhere" at [`ELSEWHERE`], a second host, which is all
//!   *off the hive* means to `page::navigable_in_place`. Both present
//!   `server-leaf`, and the view's session pins it for both hosts. So a
//!   navigation elsewhere that the policy let through **would load**: it is
//!   the attacker's origin with a certificate that verifies, the case the
//!   policy exists for. Each server records the request lines it is sent.
//!   The pin for elsewhere is load-bearing: measured with it and the policy
//!   both removed, the frame's TLS handshake fails before any request, so
//!   elsewhere's log stays empty and only the observer below notices.
//! - Elsewhere is **https** on purpose. Measured with the policy removed: an
//!   `http://` frame inside the hive's https page is never requested at all,
//!   because `WebKit`'s mixed-content check stops it first, so an http
//!   elsewhere left the frame test green with no policy installed.
//! - **Request lines, not connections.** A link click makes `WebKit` open a
//!   connection to the link's host *before* it asks the policy (measured: one
//!   connection per main-frame link click, with nothing sent on it, and none
//!   for any navigation nobody clicked). That is the network process warming
//!   up for a load the operator asked for. What the policy has to prevent is
//!   the request, so that is what the servers count.
//! - The view is **built here** the way [`page`] builds it: the same
//!   [`settings`], an ephemeral session, the same errors policy and pin. Then
//!   [`install_policy`] gets a **recorder** in place of `elsewhere`, because a
//!   click test that passed `elsewhere` would open the developer's browser.
//!   `page`'s own wiring is pinned separately, by
//!   `the_page_widget_installs_the_policy`, on a page nobody clicks, where
//!   `elsewhere` is never reached.
//! - **An observer on each side of the policy.** A `decide-policy` handler
//!   connected before [`install_policy`]'s records every decision `WebKit`
//!   dispatched, with its kind and `is_user_gesture`. One connected after it
//!   records only what the policy let through, because `decide-policy` stops at
//!   the first handler that returns `true`.
//! - **A click is a real one.** `xdotool` sends `XTest` input to the
//!   `xvfb-run` display, and `WebKit` hit-tests it and reports
//!   `is_user_gesture` true (mouse button 1). `evaluate_javascript` cannot
//!   stand in for a click, or for a script nobody clicked: measured, a
//!   `.click()` run through it reads `is_user_gesture` **true**, because the
//!   API runs scripts as a gesture. Here it only reads `location.href` and a
//!   page's own counter.
//! - **Nothing outside the test reaches it** (#1578's lesson): the page
//!   widget's gateway hears nothing, and every load is either one of the two
//!   servers or a refusal.
//!
//! # What stays live-verify
//!
//! `elsewhere` itself (that `launch_default_for_uri` reaches a real browser)
//! needs a desktop. A click on niri's glass is Wayland input, not `XTest`. And
//! the hive's real feed markup is the hive's. `docs/live-verify.md`'s two H1
//! items say so.

use super::{install_policy, page, pin_for, settings, view_of};
use crate::tls::{Pinned, Resolved, TlsPolicy};
use crate::verify::tls_tests::{fixture, glib_ready};
use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use webkit::prelude::*;

/// The hive's host. `server-leaf` names it, but the pin is what lets it load.
const HIVE: &str = "127.0.0.1";

/// Off the hive: a second loopback host, so the policy's host test is what
/// refuses it, not its scheme.
const ELSEWHERE: &str = "127.0.0.2";

/// How long a test waits for something it needs: a page to load, a click to
/// arrive, a script to run. A bound on a failure, never a delay on a pass:
/// every wait returns the moment its condition holds.
const DEADLINE: Duration = Duration::from_secs(30);

/// How long a test waits for a request that must **not** come, after the
/// decision that would have sent it. With the policy removed, the request
/// reaches its server within milliseconds of that decision (measured), so
/// this bounds only how long a broken policy could hide.
const SETTLE: Duration = Duration::from_secs(1);

/// A loopback HTTPS server presenting `server-leaf`, recording the path of
/// every request it is sent.
struct Server {
    /// `https://<host>:<port>`.
    origin: String,
    /// Each request's path, query included, in arrival order.
    requests: Arc<Mutex<Vec<String>>>,
}

impl Server {
    /// Serve `pages` on `host` (path without the query, then body), and a 404
    /// for any other path. With no pages at all, answer every path with a
    /// small page: that is elsewhere, which a broken policy would load.
    ///
    /// One thread per connection: a connection `WebKit` opens ahead of a click
    /// and never uses must not hold up a request on another one.
    fn start(host: &str, pages: Vec<(String, String)>) -> Self {
        let listener = std::net::TcpListener::bind((host, 0)).expect("a loopback port");
        let port = listener.local_addr().expect("…with an address").port();
        let requests = Arc::<Mutex<Vec<String>>>::default();
        let log = Arc::clone(&requests);
        let pages = Arc::new(pages);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (log, pages) = (Arc::clone(&log), Arc::clone(&pages));
                std::thread::spawn(move || {
                    if let Err(e) = answer(stream, &pages, &log) {
                        eprintln!("policy_tests: a fixture connection ended early: {e}");
                    }
                });
            }
        });
        Self {
            origin: format!("https://{host}:{port}"),
            requests,
        }
    }

    /// `path` on this server.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    /// The paths requested so far.
    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("the request log").clone()
    }
}

/// Handshake, read one request, answer it, close.
fn answer(
    stream: std::net::TcpStream,
    pages: &[(String, String)],
    log: &Mutex<Vec<String>>,
) -> Result<(), glib::Error> {
    let identity = gio::TlsCertificate::from_files(
        fixture("server-leaf.pem"),
        fixture("server-leaf-key.pem"),
    )?;
    let socket = gio::Socket::from_fd(std::os::fd::OwnedFd::from(stream))?;
    // A connection opened ahead of a click carries nothing; let its thread go.
    socket.set_timeout(10);
    let tls = gio::TlsServerConnection::new(
        &socket.connection_factory_create_connection(),
        Some(&identity),
    )?;
    tls.set_require_close_notify(false);
    tls.handshake(gio::Cancellable::NONE)?;
    let Some(path) = request_path(&tls.input_stream()) else {
        return Ok(());
    };
    log.lock().expect("the request log").push(path.clone());
    let bare = path.split('?').next().unwrap_or_default();
    let response = if pages.is_empty() {
        respond("200 OK", &document("<p>elsewhere</p>"))
    } else {
        pages.iter().find(|(at, _)| at == bare).map_or_else(
            || respond("404 Not Found", ""),
            |(_, body)| respond("200 OK", body),
        )
    };
    tls.output_stream()
        .write_all(response.as_bytes(), gio::Cancellable::NONE)?;
    drop(tls.close(gio::Cancellable::NONE));
    Ok(())
}

/// The path of the request on `input`, or `None` if the peer sent no request
/// line before it closed or went quiet.
fn request_path(input: &gio::InputStream) -> Option<String> {
    let mut head = Vec::new();
    let mut chunk = [0_u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match input.read(&mut chunk, gio::Cancellable::NONE) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&head);
    let mut words = head.lines().next()?.split_whitespace();
    let (_method, path) = (words.next()?, words.next()?);
    Some(path.to_owned())
}

/// A complete HTTP/1.1 response that closes its connection.
fn respond(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// `body` as a whole HTML document.
fn document(body: &str) -> String {
    format!("<!doctype html><meta charset=utf-8><title>hive</title>{body}")
}

/// One hive page: its path, and `body` as a document.
fn at(path: &str, body: &str) -> (String, String) {
    (path.to_owned(), document(body))
}

/// The trust the page widget is given: `server-leaf` pinned for [`HIVE`], the
/// route a `gateway.pem` or `TROLLSHELL_AGENT_WINDOW_CERT` takes.
fn trust() -> Resolved {
    Resolved {
        policy: TlsPolicy::AllowCertificateForHost {
            cert: Pinned::File(fixture("server-leaf.pem")),
            host: HIVE.to_owned(),
        },
        ..Resolved::default()
    }
}

/// Pin `server-leaf` for `host` on `session`, so that host's page verifies.
fn pin(session: &webkit::NetworkSession, host: &str) {
    let cert =
        gio::TlsCertificate::from_file(fixture("server-leaf.pem")).expect("the fixture leaf");
    session.allow_tls_certificate_for_host(&cert, host);
}

/// Iterate the main context until `done`, or panic with `report()` once
/// [`DEADLINE`] has passed.
fn wait_until(what: &str, done: impl Fn() -> bool, report: impl Fn() -> String) {
    let deadline = Instant::now() + DEADLINE;
    let context = glib::MainContext::default();
    while !done() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen within {DEADLINE:?}: {}",
            report()
        );
        if !context.iteration(false) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Iterate the main context for `span`.
fn pump_for(span: Duration) {
    let end = Instant::now() + span;
    let context = glib::MainContext::default();
    while Instant::now() < end {
        if !context.iteration(false) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Evaluate `expression` in the view's main frame and return it as a string.
fn read_js(view: &webkit::WebView, expression: &str) -> String {
    let answer: Rc<RefCell<Option<Result<String, String>>>> = Rc::default();
    let slot = Rc::clone(&answer);
    view.evaluate_javascript(
        expression,
        None,
        None,
        gio::Cancellable::NONE,
        move |result| {
            *slot.borrow_mut() = Some(
                result
                    .map(|value| value.to_str().to_string())
                    .map_err(|e| e.to_string()),
            );
        },
    );
    wait_until(
        &format!("`{expression}`"),
        || answer.borrow().is_some(),
        String::new,
    );
    let answered = answer.take().expect("answered above");
    answered.unwrap_or_else(|e| panic!("`{expression}` failed: {e}"))
}

/// What happened to one view's main frame, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    /// `load-changed: Finished`, with the view's URI at the time. It follows
    /// the main frame's `load` event, which waits for every frame in it.
    Finished(String),
    /// `load-failed`, with the failing URI and the error.
    Failed(String, String),
    /// `web-process-terminated`.
    Crashed(String),
}

/// A view's [`Event`]s, recorded from its signals.
#[derive(Clone, Default)]
struct Watch(Rc<RefCell<Vec<Event>>>);

impl Watch {
    /// Record `view`'s events from now on.
    fn on(view: &webkit::WebView) -> Self {
        let watch = Self::default();
        let log = Rc::clone(&watch.0);
        view.connect_load_changed(move |view, event| {
            if event == webkit::LoadEvent::Finished {
                let uri = view.uri().map(|u| u.to_string()).unwrap_or_default();
                log.borrow_mut().push(Event::Finished(uri));
            }
        });
        let log = Rc::clone(&watch.0);
        view.connect_load_failed(move |_, _, uri, error| {
            log.borrow_mut()
                .push(Event::Failed(uri.to_owned(), error.to_string()));
            false
        });
        let log = Rc::clone(&watch.0);
        view.connect_web_process_terminated(move |_, why| {
            log.borrow_mut().push(Event::Crashed(format!("{why:?}")));
        });
        watch
    }

    /// Whether `event` has happened.
    fn saw(&self, event: &Event) -> bool {
        self.0.borrow().contains(event)
    }

    /// Why the web process went away, if it did.
    fn crash(&self) -> Option<String> {
        self.0.borrow().iter().find_map(|e| match e {
            Event::Crashed(why) => Some(why.clone()),
            _ => None,
        })
    }

    /// Everything so far, for a failure message.
    fn report(&self) -> String {
        format!("{:?}", self.0.borrow())
    }

    /// Wait for `url` to finish loading, cleanly, in the main frame — `true`
    /// when it has; `false` when no web process started and this run is
    /// allowed to skip (see this module's doc).
    fn loaded_or_skip(&self, url: &str, test: &str) -> bool {
        let finished = Event::Finished(url.to_owned());
        wait_until(
            &format!("{url} to load"),
            || self.saw(&finished) || self.crash().is_some(),
            || self.report(),
        );
        if let Some(why) = self.crash() {
            let required = std::env::var_os("TROLLSHELL_REQUIRE_GL").is_some_and(|v| v == "1");
            assert!(
                !required,
                "TROLLSHELL_REQUIRE_GL=1, but WebKit's web process did not start for {test} \
                 ({why}). It needs an EGL vendor: checks.system-tests exports llvmpipe's \
                 (nix/checks/system-tests.nix), a bare xvfb-run has none. Events: {}",
                self.report()
            );
            eprintln!(
                "SKIPPED {test}: WebKit's web process did not start ({why}); it needs an EGL \
                 vendor, which a bare xvfb-run does not have"
            );
            return false;
        }
        assert!(
            !self
                .0
                .borrow()
                .iter()
                .any(|e| matches!(e, Event::Failed(..))),
            "the hive's page must load cleanly: {}",
            self.report()
        );
        true
    }
}

/// One decision `WebKit` dispatched, as the observer ahead of the policy saw
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Asked {
    /// `NewWindowAction` rather than `NavigationAction`.
    new_window: bool,
    /// The request's URI.
    uri: String,
    /// `is_user_gesture`.
    gesture: bool,
}

impl Asked {
    /// A navigation in this view.
    fn here(uri: &str, gesture: bool) -> Self {
        Self {
            new_window: false,
            uri: uri.to_owned(),
            gesture,
        }
    }

    /// A request for a new window.
    fn new_window(uri: &str, gesture: bool) -> Self {
        Self {
            new_window: true,
            uri: uri.to_owned(),
            gesture,
        }
    }
}

/// A view built the way [`page`] builds one, with [`install_policy`] handing
/// off to a recorder, in a window on screen, loading the hive's page.
struct Rig {
    /// The window holding the view.
    window: gtk::Window,
    /// The view under test.
    view: webkit::WebView,
    /// Every navigation decision `WebKit` dispatched.
    asked: Rc<RefCell<Vec<Asked>>>,
    /// The URIs the policy let through.
    passed: Rc<RefCell<Vec<String>>>,
    /// What the policy handed to the desktop.
    handed: Rc<RefCell<Vec<String>>>,
    /// Every `create` `WebKit` emitted: a second view it asked for.
    created: Rc<RefCell<Vec<String>>>,
    /// The main frame's loads.
    watch: Watch,
}

impl Rig {
    /// Open `url` (on the hive) in a fresh view, in a window titled `title`.
    fn open(url: &str, title: &str) -> Self {
        let trust = trust();
        let session = webkit::NetworkSession::new_ephemeral();
        session.set_tls_errors_policy(trust.policy.errors_policy());
        let (_, host) = pin_for(&trust.policy).expect("this trust pins the hive");
        pin(&session, host);
        pin(&session, ELSEWHERE);
        let view = webkit::WebView::builder()
            .network_session(&session)
            .settings(&settings())
            .hexpand(true)
            .vexpand(true)
            .build();

        let asked: Rc<RefCell<Vec<Asked>>> = Rc::default();
        let log = Rc::clone(&asked);
        view.connect_decide_policy(move |_, decision, kind| {
            let new_window = match kind {
                webkit::PolicyDecisionType::NavigationAction => false,
                webkit::PolicyDecisionType::NewWindowAction => true,
                _ => return false,
            };
            if let Some(action) = decision
                .downcast_ref::<webkit::NavigationPolicyDecision>()
                .and_then(webkit::NavigationPolicyDecision::navigation_action)
            {
                log.borrow_mut().push(Asked {
                    new_window,
                    uri: uri_of(&action),
                    gesture: action.is_user_gesture(),
                });
            }
            // Decides nothing: the policy, connected next, does.
            false
        });
        let created: Rc<RefCell<Vec<String>>> = Rc::default();
        let log = Rc::clone(&created);
        view.connect_create(move |_, action| {
            log.borrow_mut().push(uri_of(action));
            None
        });

        let handed: Rc<RefCell<Vec<String>>> = Rc::default();
        let log = Rc::clone(&handed);
        install_policy(
            &view,
            url,
            Rc::new(move |uri: &str| log.borrow_mut().push(uri.to_owned())),
        );

        let passed: Rc<RefCell<Vec<String>>> = Rc::default();
        let log = Rc::clone(&passed);
        view.connect_decide_policy(move |_, decision, _| {
            if let Some(action) = decision
                .downcast_ref::<webkit::NavigationPolicyDecision>()
                .and_then(webkit::NavigationPolicyDecision::navigation_action)
            {
                log.borrow_mut().push(uri_of(&action));
            }
            false
        });

        let watch = Watch::on(&view);
        let window = show(&view, title);
        view.load_uri(url);
        Self {
            window,
            view,
            asked,
            passed,
            handed,
            created,
            watch,
        }
    }

    /// The decisions `WebKit` dispatched about `uri`.
    fn asked_about(&self, uri: &str) -> Vec<Asked> {
        self.asked
            .borrow()
            .iter()
            .filter(|a| a.uri == uri)
            .cloned()
            .collect()
    }

    /// Whether the policy let `uri` through.
    fn let_through(&self, uri: &str) -> bool {
        self.passed.borrow().iter().any(|u| u == uri)
    }

    /// The main frame's URL, read from the page itself.
    fn location(&self) -> String {
        read_js(&self.view, "location.href")
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.window.destroy();
    }
}

/// The URI a navigation action asks for.
fn uri_of(action: &webkit::NavigationAction) -> String {
    action
        .request()
        .and_then(|request| request.uri())
        .map(|uri| uri.to_string())
        .unwrap_or_default()
}

/// `child` in a window on screen, titled `title`.
fn show(child: &impl IsA<gtk::Widget>, title: &str) -> gtk::Window {
    let window = gtk::Window::builder()
        .title(title)
        .default_width(600)
        .default_height(300)
        .child(child)
        .build();
    window.present();
    window
}

/// **A frame off the hive is cancelled, and a frame on it loads.**
///
/// The hive's page carries two iframes, one elsewhere and one on its own
/// origin. `WebKit` asks about both with no gesture; the policy lets the
/// second through, and the first never reaches its server. Both directions
/// are asserted, so a policy that refused everything would red as surely as
/// one that refused nothing.
///
/// Mutations (run this round, red): remove the `install_policy` call from the
/// rig, and elsewhere is sent `GET /frame`; make `decide` refuse every
/// `ThisView` request, and the hive's own page never loads.
#[gtk::test]
fn a_frame_off_the_hive_is_cancelled_and_a_frame_on_it_loads() {
    const TEST: &str = "a_frame_off_the_hive_is_cancelled_and_a_frame_on_it_loads";
    glib_ready();
    let elsewhere = Server::start(ELSEWHERE, Vec::new());
    let off = elsewhere.url("/frame");
    let hive = Server::start(
        HIVE,
        vec![
            at(
                "/agent/stray/",
                &format!("<iframe src='{off}'></iframe><iframe src='/agent/stray/inner'></iframe>"),
            ),
            at("/agent/stray/inner", "<p>inner</p>"),
        ],
    );
    let url = hive.url("/agent/stray/?hide=header");
    let rig = Rig::open(&url, TEST);
    if !rig.watch.loaded_or_skip(&url, TEST) {
        return;
    }

    // The main frame's load has finished, and its `load` event waits for every
    // frame: a frame load the policy let through has reached its server. What
    // the servers saw is asserted first, then what the policy said: the first
    // is the outcome, the second only says why.
    assert_eq!(
        elsewhere.requests(),
        Vec::<String>::new(),
        "the off-hive frame never reached its server"
    );
    assert!(
        hive.requests().iter().any(|p| p == "/agent/stray/inner"),
        "a frame on the hive's own origin loads: {:?}",
        hive.requests()
    );
    assert_eq!(rig.location(), url, "the view is still on the hive's page");
    assert_eq!(
        rig.asked_about(&off),
        [Asked::here(&off, false)],
        "WebKit asked about the frame, and nobody clicked it"
    );
    assert!(!rig.let_through(&off), "the policy refused it");
    assert!(rig.let_through(&hive.url("/agent/stray/inner")));
    assert!(
        rig.handed.borrow().is_empty(),
        "nobody clicked, so nothing goes to the desktop: {:?}",
        rig.handed.borrow()
    );
}

/// **A meta refresh or a script cannot take the view off the hive.**
///
/// Each is a main-frame navigation nobody clicked. `WebKit` asks about it with
/// `is_user_gesture` false, the policy cancels it, nothing reaches elsewhere,
/// and the page stays where it was.
///
/// Mutations (run this round, red): remove the `install_policy` call from the
/// rig, and elsewhere is sent `GET /refresh` (with the two cases swapped,
/// `GET /script`); make `decide` skip its gesture test, and the refresh is
/// handed to the desktop.
#[gtk::test]
fn a_refresh_or_a_script_cannot_take_the_view_off_the_hive() {
    const TEST: &str = "a_refresh_or_a_script_cannot_take_the_view_off_the_hive";
    glib_ready();
    let elsewhere = Server::start(ELSEWHERE, Vec::new());
    let refresh = elsewhere.url("/refresh");
    let script = elsewhere.url("/script");
    let hive = Server::start(
        HIVE,
        vec![
            at(
                "/agent/stray/refresh",
                &format!("<meta http-equiv=refresh content='0;url={refresh}'>"),
            ),
            at(
                "/agent/stray/script",
                &format!("<script>location = '{script}';</script>"),
            ),
        ],
    );
    for (case, path, target) in [
        ("a meta refresh", "/agent/stray/refresh", &refresh),
        ("a scripted `location =`", "/agent/stray/script", &script),
    ] {
        let url = hive.url(&format!("{path}?hide=header"));
        let rig = Rig::open(&url, TEST);
        if !rig.watch.loaded_or_skip(&url, TEST) {
            return;
        }
        wait_until(
            &format!("{case}: WebKit asking about {target}"),
            || !rig.asked_about(target).is_empty(),
            || format!("{:?} / {}", rig.asked.borrow(), rig.watch.report()),
        );
        pump_for(SETTLE);

        assert_eq!(
            elsewhere.requests(),
            Vec::<String>::new(),
            "{case}: it never reached elsewhere"
        );
        assert_eq!(rig.location(), url, "{case}: the view did not move");
        assert_eq!(
            rig.asked_about(target),
            [Asked::here(target, false)],
            "{case}: one navigation, and nobody clicked it"
        );
        assert!(!rig.let_through(target), "{case}: the policy refused it");
        assert!(
            rig.handed.borrow().is_empty(),
            "{case}: nothing goes to the desktop without a click: {:?}",
            rig.handed.borrow()
        );
    }
}

/// **A script's `window.open` opens nothing**, at once or from a timer.
///
/// Measured: `WebKit` asks neither handler. Its popup blocker stops an
/// unclicked `window.open` before `decide-policy` or `create`, which is what
/// `settings`' `javascript_can_open_windows_automatically(false)` is for. So
/// the test asserts that nothing was even asked for: no `create`, nothing
/// handed to the desktop, no request elsewhere, and no decision `WebKit` did
/// dispatch carrying a gesture.
///
/// Mutation (run this round, red): set
/// `javascript_can_open_windows_automatically` to `true`, and `WebKit` sends
/// both attempts straight to `create` (not through `decide-policy`), so the
/// `create` assertion reds. The window would still build nothing there
/// (`install_policy`'s `create` returns `None` and, with no gesture, hands
/// nothing off), but a page that may ask is one step from a page that gets.
/// `the_settings_are_the_ones_we_state` reds for that mutation too; it pins
/// the value, and this pins what the value does on `WebKit`'s real dispatch.
#[gtk::test]
fn a_scripted_window_open_opens_nothing() {
    const TEST: &str = "a_scripted_window_open_opens_nothing";
    glib_ready();
    let elsewhere = Server::start(ELSEWHERE, Vec::new());
    let (now, later) = (elsewhere.url("/open"), elsewhere.url("/open-later"));
    let hive = Server::start(
        HIVE,
        vec![at(
            "/agent/stray/",
            &format!(
                "<script>window.tries = 0; window.open('{now}'); window.tries++; \
                 setTimeout(() => {{ window.open('{later}', '_blank'); window.tries++; }}, 50);\
                 </script>"
            ),
        )],
    );
    let url = hive.url("/agent/stray/?hide=header");
    let rig = Rig::open(&url, TEST);
    if !rig.watch.loaded_or_skip(&url, TEST) {
        return;
    }
    let deadline = Instant::now() + DEADLINE;
    while read_js(&rig.view, "String(window.tries)") != "2" {
        assert!(
            Instant::now() < deadline,
            "the page never made its two attempts"
        );
        pump_for(Duration::from_millis(50));
    }
    pump_for(SETTLE);

    assert!(
        rig.created.borrow().is_empty(),
        "no second view: {:?}",
        rig.created.borrow()
    );
    assert!(
        rig.handed.borrow().is_empty(),
        "nothing goes to the desktop without a click: {:?}",
        rig.handed.borrow()
    );
    assert_eq!(elsewhere.requests(), Vec::<String>::new());
    for uri in [&now, &later] {
        assert!(
            rig.asked_about(uri).iter().all(|a| !a.gesture),
            "{:?}",
            rig.asked_about(uri)
        );
    }
    assert_eq!(rig.location(), url, "the view did not move");
}

/// Whether this run can send real pointer input: `xdotool` on `PATH` and GTK
/// on an X11 display. Without either it skips, unless
/// `TROLLSHELL_REQUIRE_XDOTOOL=1` (exported by `checks.system-tests`, for
/// `trollshell`'s `plugins/region.rs`, the precedent this follows) makes that
/// a failure.
fn real_pointer_or_skip(test: &str) -> bool {
    let xdotool = std::process::Command::new("xdotool")
        .arg("version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    let on_x11 = gtk::gdk::Display::default()
        .is_some_and(|display| display.type_().name() == "GdkX11Display");
    if xdotool && on_x11 {
        return true;
    }
    let why = if xdotool {
        "GTK is not on an X11 display, and xdotool drives X11 only"
    } else {
        "xdotool is not on PATH"
    };
    let required = std::env::var_os("TROLLSHELL_REQUIRE_XDOTOOL").is_some_and(|v| v == "1");
    assert!(
        !required,
        "TROLLSHELL_REQUIRE_XDOTOOL=1, but {why} for {test}"
    );
    eprintln!("SKIPPED {test}: {why}");
    false
}

/// Run one `xdotool` command line to completion.
fn xdotool(args: &[&str]) -> String {
    let out = std::process::Command::new("xdotool")
        .args(args)
        .output()
        .expect("xdotool runs");
    assert!(out.status.success(), "xdotool {args:?} failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The X window titled `title`, as `xdotool` names it, raised.
fn x_window(title: &str) -> String {
    let pattern = format!("^{title}$");
    let found = RefCell::new(None);
    wait_until(
        &format!("an X window titled {title}"),
        || {
            let out = xdotool(&["search", "--onlyvisible", "--name", &pattern]);
            *found.borrow_mut() = out.lines().next().map(|l| l.trim().to_owned());
            found.borrow().is_some()
        },
        String::new,
    );
    let xid = found.into_inner().expect("found above");
    xdotool(&["windowraise", &xid]);
    xid
}

/// Click button 1 on the view, `fraction` of the way across it and halfway
/// down, in the X window `xid` — the window's own coordinates, offset by
/// wherever GTK put the window widget inside its surface and scaled to device
/// pixels (`trollshell`'s `plugins/region.rs` `centre_of`, the same sum).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "a point inside a 600 × 300 window"
)]
fn click(rig: &Rig, xid: &str, fraction: f32) {
    let (w, h) = (rig.view.width(), rig.view.height());
    assert!(w > 0 && h > 0, "the view must be laid out (got {w}×{h})");
    let point = gtk::graphene::Point::new(fraction * w as f32, h as f32 / 2.0);
    let at = rig
        .view
        .compute_point(&rig.window, &point)
        .expect("the view is inside its window");
    let (dx, dy) = rig.window.surface_transform();
    let scale = f64::from(rig.window.scale_factor());
    let x = ((f64::from(at.x()) + dx) * scale).round() as i32;
    let y = ((f64::from(at.y()) + dy) * scale).round() as i32;
    xdotool(&[
        "mousemove",
        "--window",
        xid,
        &x.to_string(),
        &y.to_string(),
        "click",
        "1",
    ]);
}

/// **A real click off the hive is handed off once, and the view stays** — the
/// other half of `decide`'s rule, through a real pointer.
///
/// The page is three columns, one link each: off the hive, off the hive with
/// `target="_blank"`, and on the hive. A click on each, sent as `XTest` input:
///
/// 1. The off-hive link: `WebKit` asks with `is_user_gesture` **true**, the
///    policy cancels it in the view and hands it off **once**.
/// 2. The `_blank` link: the same, as a `NewWindowAction`, and `WebKit` never
///    reaches `create`, so the hand-off is not doubled and no second view is
///    asked for.
/// 3. Neither ever reaches elsewhere's server, and the page did not move.
/// 4. The hive link: it loads **in place**, and nothing is handed off.
///
/// Mutations (run this round, red): make `install_policy`'s handler pass
/// `false` for the gesture and the first click hands nothing off; make it
/// `use` a `NewWindowAction` instead of ignoring it and the `_blank` click
/// reaches `create`.
#[gtk::test]
fn a_real_click_off_the_hive_is_handed_off_once_and_the_view_stays() {
    const TEST: &str = "a_real_click_off_the_hive_is_handed_off_once_and_the_view_stays";
    glib_ready();
    let elsewhere = Server::start(ELSEWHERE, Vec::new());
    let (off, blank) = (elsewhere.url("/clicked"), elsewhere.url("/blank"));
    let column = "position:fixed;top:0;bottom:0;width:33%;display:block";
    let hive = Server::start(
        HIVE,
        vec![
            at(
                "/agent/stray/",
                &format!(
                    "<a href='{off}' style='{column};left:0'>off</a>\
                     <a href='{blank}' target=_blank style='{column};left:33%'>blank</a>\
                     <a href='/agent/stray/turn/2' style='{column};left:66%'>in</a>"
                ),
            ),
            at("/agent/stray/turn/2", "<p>turn 2</p>"),
        ],
    );
    let url = hive.url("/agent/stray/?hide=header");
    let title = format!("{TEST}-{}", std::process::id());
    let rig = Rig::open(&url, &title);
    if !rig.watch.loaded_or_skip(&url, TEST) || !real_pointer_or_skip(TEST) {
        return;
    }
    let xid = x_window(&title);

    click(&rig, &xid, 1.0 / 6.0);
    wait_until(
        "the off-hive click to be handed off",
        || !rig.handed.borrow().is_empty(),
        || format!("{:?}", rig.asked.borrow()),
    );
    assert_eq!(
        rig.asked_about(&off),
        [Asked::here(&off, true)],
        "WebKit reports a real click as a gesture"
    );
    assert!(!rig.let_through(&off), "cancelled in the view");

    click(&rig, &xid, 0.5);
    wait_until(
        "the `_blank` click to be handed off",
        || rig.handed.borrow().len() >= 2,
        || format!("{:?}", rig.asked.borrow()),
    );
    assert_eq!(
        rig.asked_about(&blank),
        [Asked::new_window(&blank, true)]
    );
    pump_for(SETTLE);
    assert_eq!(
        *rig.handed.borrow(),
        [off.clone(), blank.clone()],
        "each click handed off exactly once"
    );
    assert!(
        rig.created.borrow().is_empty(),
        "no second view was asked for: {:?}",
        rig.created.borrow()
    );
    assert_eq!(
        elsewhere.requests(),
        Vec::<String>::new(),
        "a link handed to the desktop is not also loaded here"
    );
    assert_eq!(rig.location(), url, "the view did not move");

    let turn = hive.url("/agent/stray/turn/2");
    click(&rig, &xid, 5.0 / 6.0);
    wait_until(
        "the hive link to load",
        || rig.watch.saw(&Event::Finished(turn.clone())),
        || rig.watch.report(),
    );
    assert!(rig.let_through(&turn), "a click on the hive loads in place");
    assert_eq!(rig.location(), turn);
    assert_eq!(rig.handed.borrow().len(), 2, "and is not handed off");
}

/// **The page widget installs the policy.** Everything above builds its own
/// view so it can pass a recorder; this is the one test of [`page`] itself, on
/// a page nobody clicks, so its real hand-off (`elsewhere`, the desktop's
/// launcher) is never reached.
///
/// The page carries the off-hive frame of the first test. Built by [`page`],
/// with the hive pinned through [`trust`] (so this is also the pin route
/// loading a real page) and a gateway that hears nothing: the frame never
/// reaches elsewhere, and the hive's own frame loads.
///
/// Mutation (run this round, red): delete `page`'s `install_policy` call, and
/// elsewhere is sent `GET /frame`.
#[gtk::test]
fn the_page_widget_installs_the_policy() {
    const TEST: &str = "the_page_widget_installs_the_policy";
    glib_ready();
    let elsewhere = Server::start(ELSEWHERE, Vec::new());
    let off = elsewhere.url("/frame");
    let hive = Server::start(
        HIVE,
        vec![
            at(
                "/agent/stray/",
                &format!("<iframe src='{off}'></iframe><iframe src='/agent/stray/inner'></iframe>"),
            ),
            at("/agent/stray/inner", "<p>inner</p>"),
        ],
    );
    let url = hive.url("/agent/stray/?hide=header");
    let widget = page(&url, &trust(), |_| {});
    let view = view_of(&widget).expect("the page widget carries the view");
    // Elsewhere verifies too, so a frame the policy let through would load.
    pin(
        &view.network_session().expect("the view has a session"),
        ELSEWHERE,
    );
    let watch = Watch::on(&view);
    let _shown = Shown(show(&widget, TEST));
    if !watch.loaded_or_skip(&url, TEST) {
        return;
    }
    // As in the first test: the main frame's `load` event waited for both
    // frames.
    assert_eq!(elsewhere.requests(), Vec::<String>::new(), "the frame never left");
    assert!(
        hive.requests().iter().any(|p| p == "/agent/stray/inner"),
        "the hive's own frame loads: {:?}",
        hive.requests()
    );
}

/// A window destroyed when this drops, pass or fail, so a red test leaves
/// nothing on the display for the next one.
struct Shown(gtk::Window);

impl Drop for Shown {
    fn drop(&mut self) {
        self.0.destroy();
    }
}
