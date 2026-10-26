//! The URL the `WebKitGTK` view loads: hyperhive's own agent page, with the
//! chrome it would draw for itself suppressed.
//!
//! # `?hide=header`
//!
//! Mara's constraint on [#947](https://github.com/vibec0re/trollshell/issues/947)
//! (2026-09-11): hyperhive's agent terminal page is slated for a swarm-level
//! rewrite, so this window must **not** render its own agent page, and must
//! not depend on that page's DOM either. What it depends on instead is one
//! query parameter, which @the-sword-above shipped on
//! [#950](https://github.com/vibec0re/trollshell/issues/950):
//!
//! > **`?hide=` on the per-agent page**, comma-separated list of elements to
//! > suppress: `header` hides the header chrome, `input` the composer/input
//! > footer, `header,input` both, leaving just the live feed (what P2 wants
//! > for the `WebKitGTK`-embedded view). Unknown values in the list are ignored.
//!
//! So this is **one constant**, and everything else about the page is the
//! hive's business. The window's own chrome — the header with the live status
//! read from `host.sock`, start/stop/pause, the settings tab — replaces
//! exactly the thing it hides.
//!
//! Unknown values being ignored is what let the flag ship before the feature
//! did; it is also why a rename on the hive's side costs one line here.
//!
//! # Why `input` came back
//!
//! It was in the list until
//! [#1282](https://github.com/vibec0re/trollshell/issues/1282) item 1
//! (@kaesaecracker, "enable input in agent term"). P2 specced this window as a
//! **monitor** — our chrome, their feed, nothing to type into — and hiding the
//! composer was how that was spelled. That premise is what changed: a window
//! you can only read is a worse browser tab, and #1282 item 2 makes the card's
//! row open this window instead of a tab, so it has to be able to do
//! everything the tab could.
//!
//! Showing it crosses **no boundary a browser tab does not already cross**:
//! the composer is hyperhive's own, served from the hive's own origin, posting
//! to the hive over its own session — this window neither reads nor writes the
//! DOM (Mara's constraint above, unchanged) and adds no input surface of its
//! own. The origin stays pinned by [`navigable_in_place`], so what the page
//! does with a typed message cannot move the viewport off the hive.
//!
//! `header` stays hidden for the reason it always was: our header replaces it,
//! and two headers reading the same agent — one of them ours, one of them the
//! page's, disagreeing for a poll interval — is the confusion #950 removed.

/// The query the embedded view appends — the whole of this window's contract
/// with hyperhive's frontend.
///
/// **`header` only** since #1282 item 1: the composer stays, so the embedded
/// page can be typed into. See the module doc for why that is not a widening
/// of this window's trust.
pub const HIDE_QUERY: &str = "hide=header";

/// Point [`HIDE_QUERY`] at an agent's own page URL.
///
/// The URL comes from `AgentStatusRow::url` (hyperhive#4073 — "present here
/// \[…\] so a client never has to build this path itself"), so it is the
/// hive's string, not a path this window derives. All this does is add the
/// parameter, with the separator the URL's existing shape calls for.
///
/// A fragment is preserved in place (`…/#tail` → `…/?hide=…#tail`) rather than
/// appended past, because a query after a fragment is part of the fragment and
/// would never reach the server. No agent URL carries one today; getting it
/// wrong silently would cost the whole feature, and the branch is two lines.
#[must_use]
pub fn embed_url(base: &str) -> String {
    let base = base.trim();
    let (before, fragment) = match base.split_once('#') {
        Some((before, frag)) => (before, Some(frag)),
        None => (base, None),
    };
    let separator = if before.contains('?') { '&' } else { '?' };
    let mut out = format!("{before}{separator}{HIDE_QUERY}");
    if let Some(frag) = fragment {
        out.push('#');
        out.push_str(frag);
    }
    out
}

/// May `candidate` load **in this window**, or must it go to the browser?
///
/// # Why this exists
///
/// The embedded page is hyperhive's **agent turn stream**: rendered model
/// output and tool output, which is content the agent's own inputs can
/// influence — a hostile file it read, a page it fetched, a prompt-injected
/// tool result. Without a policy handler the page owns the viewport after the
/// first load: one `<a href>` or one `window.location =` moves this window to
/// an arbitrary origin **in place**, inside chrome that still reads
/// `<agent> — agent · running` with a start/stop/pause row under it and no
/// address bar to contradict it. The host-scoped TLS grant is no help — it
/// correctly does not apply to the attacker's host, so a valid public
/// certificate there loads clean.
///
/// So this window navigates to **exactly one origin**: the one it was opened
/// for. Everything else is cancelled in the view, and a link the operator
/// actually **clicked** is handed to the desktop's default handler, which
/// keeps a legitimate link in the feed working — just not here. Which
/// refusals reach the desktop is [`decide`]'s business, not this function's.
///
/// This is the half of Mara's "never depend on the page's DOM" that the
/// `?hide=` contract does not cover: the window does not read the DOM, but it
/// does hand the DOM its viewport.
///
/// # The predicate
///
/// Scheme **and** host must match, and the scheme must be `https` — the hive's
/// own URL always is (`https://<domain>/agent/<name>/`, hyperhive#4073), so
/// admitting `http` would only ever admit a downgrade. `file:`, `data:` and
/// every other scheme are refused by the same test.
///
/// [`crate::tls::host_of`] strips the port with the userinfo, so a page on
/// `hive.local:8443` and a link to `hive.local:9999` compare equal here. That
/// is deliberate and it is the weaker half of the rule: a second service on
/// another port of the *same host* is the hive operator's own machine, where
/// this window's premise ("the hive is trusted") already holds, and the
/// attacker this guards against controls a different **name**.
///
/// # Case: the hive's spelling is folded, `WebKit`'s is taken as given
///
/// The two arguments do not come from the same place. `candidate` is what
/// `decide-policy` hands over, and that is `WebKit`'s **canonical** URL: its
/// WHATWG parser lower-cases an `http(s)` host before anything sees it.
/// `embedded` is the hive's own string (`AgentStatusRow::url`), which
/// `WebKit` has not touched. Compared byte for byte, a hive whose URL says
/// `https://Hive.Example/…` refused the window's **own first load** — the
/// initial `load_uri` comes back as `https://hive.example/…`, which then
/// went to the desktop's browser while the view stayed blank (the October
/// 2026 audit of this crate, M2).
///
/// So the hive's host is put into the spelling `WebKit` hands back — ASCII
/// lower case, which is all a DNS name's case ever was (RFC 4343) — and the
/// candidate is compared exactly as given. That is deliberately one-sided:
/// folding the candidate too would change nothing `WebKit` can produce, and
/// would only admit spellings that did not come from the engine, which this
/// rule refuses rather than guesses at (the same reasoning that keeps the
/// `https://` test above case-sensitive).
///
/// Nothing else is folded **here**. A trailing dot is kept on both sides
/// because `WebKit` keeps it (`hive.local.` is its own origin there too).
///
/// # A name in Unicode: both sides in A-labels (#1557 item 4)
///
/// `WebKit` hands `decide-policy` a non-ASCII host in its IDNA A-label
/// spelling (`https://bücher.example/` arrives as
/// `https://xn--bcher-kva.example/`), and the hive's own string is compared
/// in that spelling too. The conversion is not this function's:
/// [`crate::tls::host_of`] reads through `GUri`, which IDNA-encodes any
/// non-ASCII DNS host it parses, so both arguments arrive here in A-labels
/// and the comparison stays byte for byte. This paragraph used to say the
/// opposite — that a Unicode hive refused its own page — and it had been
/// false since #1488 moved `host_of` onto `GUri`, with nothing pinning
/// either reading; `an_idn_hive_url_loads_its_own_page` pins the true one.
///
/// `GLib`'s IDNA is not `WebKit`'s in every corner (it drops a zero-width
/// joiner WHATWG refuses, for one), and that is safe here for a reason worth
/// writing down: this predicate gates the window's **own first load** too —
/// [`decide`]'s `InPlace` is the only way the hive's page gets into the
/// view. So a name the two parsers read differently refuses the hive's own
/// page, and with no hive document in the view there is nothing left to
/// issue the navigation such a disagreement would admit. It fails closed; it
/// cannot widen.
#[must_use]
pub fn navigable_in_place(embedded: &str, candidate: &str) -> bool {
    // A page with no host of its own admits nothing: there is no origin to
    // compare against, and two missing hosts are not a match.
    let Some(ours) = crate::tls::host_of(embedded) else {
        return false;
    };
    let ours = ours.to_ascii_lowercase();
    candidate.starts_with("https://")
        && crate::tls::host_of(candidate).is_some_and(|theirs| theirs == ours)
}

/// Where the page asked for a URI to go — the one fact about a request,
/// besides the URI and its [user gesture](decide), that `webkit6` 0.6 lets
/// [`decide`] see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// `decide-policy`'s `NavigationAction`: a load in this view's own
    /// viewport — **the main frame or any subframe**, which this API cannot
    /// tell apart (see [`decide`]).
    ThisView,
    /// `decide-policy`'s `NewWindowAction` (`target="_blank"`), or `create`
    /// (the request to build the window itself): a window this view will
    /// never build.
    NewWindow,
}

/// What the view does with one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Let `WebKit` load it here: the agent's own origin, in this view.
    InPlace,
    /// Cancel it here and hand the URI to the desktop's default handler.
    Desktop,
    /// Cancel it here and do nothing else.
    Refuse(Refusal),
}

/// Why a [`Decision::Refuse`] went nowhere — the whole of its log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Nobody clicked: a script, a `<meta http-equiv=refresh>`, an iframe's
    /// own load, a `window.open` loop. The case this rule exists for.
    NoGesture,
    /// Somebody clicked, on a scheme no launcher should ever be handed — see
    /// [`may_leave`].
    Scheme,
}

/// Load `candidate` here, hand it to the desktop, or drop it?
///
/// # Why the desktop needs a click
///
/// [`navigable_in_place`] keeps the viewport on the hive, and until a
/// security review of `main` (October 2026) caught it, every request it
/// refused went to `launch_default_for_uri`. The page is the
/// agent's turn stream — content the agent's inputs can influence — so that
/// made the **desktop** the thing a prompt-injected page could drive:
/// `<iframe src="https://evil/…">`, a `<meta http-equiv=refresh>`, a
/// scripted `location =` or a `window.open` loop would each open a browser
/// tab on an attacker-chosen URL, or a hundred of them, with nobody touching
/// anything. So a refused request reaches the desktop only when
/// `WebKit` says a **user gesture** started it, and is otherwise dropped
/// with one debug line. A click on a link in the feed still opens it in the
/// browser; nothing else does.
///
/// # The rule
///
/// 1. [`Target::ThisView`] on the agent's own origin loads **in place**,
///    gesture or not — the initial `load_uri` carries none, and neither do
///    the hive's own scripted navigations or its frames.
/// 2. Anything else with no gesture is [`Refusal::NoGesture`].
/// 3. A gesture on a scheme [`may_leave`] refuses is [`Refusal::Scheme`]:
///    `file:`, `javascript:`, `data:`, a custom handler — never a launcher.
/// 4. Everything left — a click, on http(s), off the hive or asking for a
///    window — goes to the [desktop](Decision::Desktop).
///
/// A [`Target::NewWindow`] never loads in place, even on the hive's own
/// origin: there is no second window here for it to become, so a clicked one
/// goes to the browser and an unclicked one goes nowhere.
///
/// # Main frame or subframe: `webkit6` 0.6 cannot say
///
/// The review asked for "a user-initiated **main-frame** navigation", and
/// the frame half is not answerable here. `decide-policy`'s
/// `NavigationAction` fires *"when `WebKit` is about to navigate to a new page
/// in either the main frame or a subframe"* (`WebKit-6.0.gir`,
/// `WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION`), and the
/// `NavigationAction` it carries names no frame: `frame_name` is the
/// **target** name of a `target=` link and `NULL` *"in all other cases"* —
/// which is what an unnamed iframe and the main frame both read. The one
/// main-frame predicate in the API,
/// `ResponsePolicyDecision::is_main_frame_main_resource`, is on the
/// *response* decision, which a refused navigation never reaches.
///
/// So this rule is written not to need it, and treats every navigation as
/// though it might be a subframe's. The vector a frame test would close — a
/// frame's own load, `<iframe src>` — carries no gesture and is refused by
/// rule 2. What is left, a gesture-bearing navigation *from* a subframe,
/// needs the operator to click inside a document already in one of this
/// view's frames — and since rule 2 refuses every off-hive frame load,
/// whatever is in one was put there by the hive's own page. That is a click
/// on the hive's content: the main-frame case under another name. The more
/// conservative reading — refuse every `ThisView` request, frames unknown —
/// would leave only `target="_blank"` links working and make every plain
/// link in the feed silently dead, the "small lie" `webview.rs`'s `create`
/// handler exists to avoid for `_blank`.
///
/// What a gesture cannot rule out is a script **on the hive's origin**
/// spending the operator's click on a URL of its choosing. That still takes
/// a click, and a script that runs on the hive's origin can already post to
/// the hive as the operator, so the browser tab is not the worst of it.
#[must_use]
pub fn decide(embedded: &str, candidate: &str, target: Target, user_gesture: bool) -> Decision {
    if target == Target::ThisView && navigable_in_place(embedded, candidate) {
        return Decision::InPlace;
    }
    if !user_gesture {
        return Decision::Refuse(Refusal::NoGesture);
    }
    if !may_leave(candidate) {
        return Decision::Refuse(Refusal::Scheme);
    }
    Decision::Desktop
}

/// May `uri` be handed to the desktop's launcher at all?
///
/// `http`/`https` only — the same judgement the shell's own `OpenUri` broker
/// makes, minus `file:`, because nothing in an agent's turn stream has a
/// reason to open a local path and the launcher is the one place a hostile
/// page's string becomes a program's argument. [`decide`] asks first, and
/// `webview.rs`'s `elsewhere` asks again at the launcher itself.
///
/// Case-sensitive on purpose: `WebKit` hands the policy handlers its own
/// canonical, lower-cased scheme, so an `HTTPS://` here did not come from
/// there and is refused rather than guessed at.
#[must_use]
pub fn may_leave(uri: &str) -> bool {
    uri.starts_with("https://") || uri.starts_with("http://")
}

/// Why one of the view's own loads failed, as far as asking again goes —
/// what `webview.rs` makes of the `GError` that `load-failed` carries
/// (#1557 item 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadFailure {
    /// The certificate. That is the TLS card's to explain, and asking again
    /// would be handed the same certificate: never retried, so the route a
    /// TLS failure takes is exactly the one it took before #1557.
    Tls,
    /// Nothing answered: the connection was refused, the name did not
    /// resolve, the route or the wait ran out. A gateway that is down — the
    /// one failure that can mend itself while the window stays open.
    NoAnswer,
    /// Anything else: a cancellation (a newer load superseding this one, a
    /// policy decision), a protocol error, a scheme `WebKit` cannot load.
    /// Asking again would get the same answer, or was never asked for.
    Other,
}

/// Load `failing` again, after a load of it failed with `failure`? —
/// #1557 item 2.
///
/// Only when **nothing answered**, and only for the **hive's own origin**,
/// by the very test [`navigable_in_place`] puts every navigation to. The
/// second half is what makes a reload loop impossible off the hive: a URI
/// the policy would cancel can never be the one reloaded, whatever its load
/// failed with, so the window cannot be made to hammer a host it never
/// agreed to show. (`decide` already cancels such a navigation before it
/// can fail; this does not rely on that.)
///
/// How *often* is not this function's: the window queues the reload on the
/// same backoff its probe retry uses (`window.rs`'s `retry_delay`), from the
/// poll interval doubling up to a minute.
#[must_use]
pub fn reload_after(embedded: &str, failing: &str, failure: LoadFailure) -> bool {
    failure == LoadFailure::NoAnswer && navigable_in_place(embedded, failing)
}

#[cfg(test)]
mod tests {
    use super::{
        Decision, HIDE_QUERY, LoadFailure, Refusal, Target, decide, embed_url, may_leave,
        navigable_in_place, reload_after,
    };

    /// **Only the agent's own origin loads in our chrome.** The reviewer's
    /// test, taken verbatim and then widened.
    ///
    /// Mutation (re-run this round, red): make `navigable_in_place` return
    /// `true` unconditionally — the allow-all this window shipped with — and
    /// every negative case here reds.
    #[test]
    fn only_the_agents_own_origin_loads_in_our_chrome() {
        let page = "https://hive.local/agent/stray/?hide=header";
        assert!(navigable_in_place(
            page,
            "https://hive.local/agent/stray/turn/3"
        ));
        assert!(!navigable_in_place(page, "https://evil.example/login"));
        assert!(!navigable_in_place(page, "http://hive.local/agent/stray/"));
        assert!(!navigable_in_place(page, "file:///etc/passwd"));
    }

    /// The shapes an attacker actually reaches for: a subdomain of the hive's
    /// name, the hive's name as a **subdomain** of theirs, and the schemes a
    /// rendered turn stream can carry.
    ///
    /// `https://hive.local.evil.example/` is the one the review names by
    /// hand — a prefix test would admit it.
    #[test]
    fn a_lookalike_host_is_not_the_hive() {
        let page = "https://hive.local/agent/stray/?hide=header";
        for hostile in [
            "https://hive.local.evil.example/login",
            "https://evil.example/hive.local/login",
            "https://hive.local@evil.example/login",
            "https://xn--hive-local/",
            "data:text/html,<h1>hi",
            "javascript:alert(1)",
            "about:blank",
            "",
        ] {
            assert!(
                !navigable_in_place(page, hostile),
                "{hostile} must not load in this window"
            );
        }
        // …and the hive's own pages still do, including a bare root and a
        // deep link with its own query.
        for ours in [
            "https://hive.local/",
            "https://hive.local/agent/other/",
            "https://hive.local/agent/stray/?tab=todos",
        ] {
            assert!(navigable_in_place(page, ours), "{ours} is the hive");
        }
    }

    /// A page URL with no host of its own admits **nothing** — the window
    /// would otherwise have no origin to compare against, and a naive
    /// `None == None` would let a hostless candidate through.
    ///
    /// The last pair is the one that makes the `host_of(embedded).is_some()`
    /// conjunct load-bearing, and it is here because #1130's re-verification
    /// measured the claim without it: every other candidate in this file is
    /// already excluded by the `starts_with("https://")` guard, so dropping
    /// the conjunct left the suite **green**. `https://` is the shape that
    /// slips past the guard and has no host — `None == None` — so it is the
    /// case the conjunct exists for.
    ///
    /// Since the October 2026 audit's M2 that conjunct is a `let … else` that
    /// returns before anything is compared — the hive's host has to be in
    /// hand to be lower-cased — so the hole is closed by shape rather than by
    /// a term someone could drop: there is no `Option` left to compare.
    ///
    /// Mutation (run this round, red): go back to comparing the two `Option`s
    /// with no early return (`host_of(candidate).map(…) ==
    /// host_of(embedded).map(…)`, both lower-cased) and the `https://` pair
    /// passes.
    #[test]
    fn a_page_with_no_origin_admits_nothing() {
        for page in ["", "not a url", "file:///tmp/x"] {
            assert!(!navigable_in_place(page, "https://hive.local/"));
            assert!(!navigable_in_place(page, "file:///tmp/x"));
            assert!(
                !navigable_in_place(page, "https://"),
                "{page:?} has no host and neither does the candidate — two Nones are not a match"
            );
        }
        // …and a page that *does* have an origin still refuses a hostless
        // candidate, so the conjunct is not doing this one's work for it.
        assert!(!navigable_in_place(
            "https://hive.local/agent/stray/",
            "https://"
        ));
    }

    /// **A hive URL in mixed case still loads its own page** — the October
    /// 2026 audit's M2.
    ///
    /// Each candidate below is what `decide-policy` is handed for the URL
    /// beside it: `WebKit`'s canonical form, host lower-cased. The first row
    /// is the window's own first `load_uri`, so before this a hive configured
    /// as `Hive.Local` never showed its page in the window at all. The
    /// `:8443` and IPv6 rows say the fold is the host's and nothing else's;
    /// the trailing-dot rows say a dot is kept, on both sides, as `WebKit`
    /// keeps it.
    ///
    /// Mutation (run this round, red): drop the `to_ascii_lowercase` in
    /// `navigable_in_place` and this reds on its first row — every positive
    /// row spells the hive with upper case — as does the second row of
    /// [`a_candidate_not_in_webkits_spelling_is_still_refused`], which a
    /// comparison folding nothing admits.
    #[test]
    fn a_mixed_case_hive_url_still_loads_its_own_page() {
        let page = "https://Hive.Local/agent/stray/?hide=header";
        for (embedded, canonical) in [
            (page, "https://hive.local/agent/stray/?hide=header"),
            (page, "https://hive.local/agent/other/"),
            (
                "https://HIVE.LOCAL:8443/agent/stray/",
                "https://hive.local:8443/agent/stray/",
            ),
            (
                "https://[FD00::1]/agent/stray/",
                "https://[fd00::1]/agent/stray/",
            ),
            (
                "https://Hive.Local./agent/stray/",
                "https://hive.local./agent/stray/",
            ),
        ] {
            assert!(
                navigable_in_place(embedded, canonical),
                "{canonical} is what WebKit makes of {embedded}, so it is the hive"
            );
        }
        // …and the fold admits nothing that was not the hive already.
        for hostile in [
            "https://hive.local.evil.example/login",
            "https://evil.example/hive.local/",
            "https://hive.local@evil.example/login",
            "https://hive.local./agent/stray/",
            "http://hive.local/agent/stray/",
        ] {
            assert!(
                !navigable_in_place(page, hostile),
                "{hostile} must not load in a window opened on {page}"
            );
        }
    }

    /// **A hive named in Unicode loads its own page** — #1557 item 4.
    ///
    /// Each candidate is what `decide-policy` is handed for the hive URL
    /// beside it: `WebKit`'s WHATWG spelling, the host in IDNA A-labels
    /// (`xn--bcher-kva` is RFC 3492's own worked example for `bücher`, so the
    /// literal is the standard's, not a recomputation). The first row is the
    /// window's own first `load_uri`; the others say the conversion composes
    /// with the M2 case fold, with a port, and with a trailing dot, which
    /// `WebKit` keeps and so must the hive's side.
    ///
    /// Mutations (run this round, red): parse with `UriFlags::NON_DNS` in
    /// `tls::authority_of` and the first row reds — the hive's host stays
    /// `bücher.example` while `WebKit`'s is `xn--bcher-kva.example`, which is
    /// the refusal the issue describes; drop `tls::host_of`'s trailing-dot
    /// re-append and the trailing-dot row reds.
    #[test]
    fn an_idn_hive_url_loads_its_own_page() {
        for (embedded, canonical) in [
            (
                "https://bücher.example/agent/stray/?hide=header",
                "https://xn--bcher-kva.example/agent/stray/?hide=header",
            ),
            (
                "https://Bücher.Example/agent/stray/?hide=header",
                "https://xn--bcher-kva.example/agent/other/",
            ),
            (
                "https://bücher.example:8443/agent/stray/",
                "https://xn--bcher-kva.example:8443/agent/stray/turn/3",
            ),
            (
                "https://bücher.example./agent/stray/",
                "https://xn--bcher-kva.example./agent/stray/",
            ),
            (
                "https://XN--BCHER-KVA.example/agent/stray/",
                "https://xn--bcher-kva.example/agent/stray/",
            ),
        ] {
            assert!(
                navigable_in_place(embedded, canonical),
                "{canonical} is what WebKit makes of {embedded}, so it is the hive"
            );
            assert_eq!(
                decide(embedded, canonical, Target::ThisView, false),
                Decision::InPlace,
                "…and the window's own gesture-less load of it goes in place"
            );
        }
    }

    /// …and **one origin still means one**: the A-label fold admits the
    /// hive's own name in `WebKit`'s spelling and nothing else.
    ///
    /// The rows are the shapes the ASCII rule already refuses, spelled for an
    /// IDN hive: the plain-ASCII look-alike (`bucher`, the homograph a reader
    /// might expect a "normalisation" to fold in), another IDN name
    /// (`münchen`), the trailing dot the hive did not write, a subdomain
    /// either way round, userinfo, a path that names the hive, and the
    /// downgrade — plus the converse, an ASCII hive refusing its IDN
    /// look-alike.
    ///
    /// Mutation (run this round, red): the issue's other suggested fix,
    /// "IDNA-encode both sides" — run the candidate's host through
    /// `glib::hostname_to_ascii` as well — and the `xn--bcher-kva.example.`
    /// row reds: the conversion drops the dot, so a second origin by
    /// `WebKit`'s reckoning compares equal. (It also folds the candidate's
    /// case, which `a_candidate_not_in_webkits_spelling_is_still_refused`
    /// catches.) Converting only the hive's side, to the spelling `WebKit`
    /// already uses, is what keeps the rule one origin wide.
    #[test]
    fn another_idn_host_is_still_another_host() {
        let page = "https://bücher.example/agent/stray/?hide=header";
        for hostile in [
            "https://bucher.example/agent/stray/",
            "https://xn--mnchen-3ya.example/agent/stray/",
            "https://xn--bcher-kva.example./agent/stray/",
            "https://www.xn--bcher-kva.example/agent/stray/",
            "https://xn--bcher-kva.example.evil.example/login",
            "https://xn--bcher-kva.example@evil.example/login",
            "https://evil.example/xn--bcher-kva.example/",
            "http://xn--bcher-kva.example/agent/stray/",
        ] {
            assert!(
                !navigable_in_place(page, hostile),
                "{hostile} must not load in a window opened on {page}"
            );
        }
        assert!(!navigable_in_place(
            "https://bucher.example/agent/stray/",
            "https://xn--bcher-kva.example/agent/stray/"
        ));
    }

    /// The fold is **one-sided**: the hive's spelling is put into `WebKit`'s,
    /// and the candidate is taken exactly as given — so a candidate that is
    /// not in `WebKit`'s spelling is still refused, even when it is the
    /// hive's own raw string. `WebKit` never hands `decide-policy` one; if
    /// something ever does, "not here" is the answer, not a guess.
    ///
    /// Mutation (run this round, red): lower-case the candidate's host too
    /// and this reds on its first row (the second falls with it: folded, the
    /// raw spelling is the hive).
    #[test]
    fn a_candidate_not_in_webkits_spelling_is_still_refused() {
        assert!(!navigable_in_place(
            "https://hive.local/agent/stray/",
            "https://HIVE.local/agent/stray/"
        ));
        let mixed = "https://Hive.Local/agent/stray/";
        assert!(
            !navigable_in_place(mixed, mixed),
            "the hive's raw spelling is not what WebKit hands back, so it is not compared as if it were"
        );
    }

    /// The ordinary case: the hive's `https://<domain>/agent/<name>/`.
    ///
    /// Mutation (verified red, #1130 review M1): drop the parameter (return `base` unchanged)
    /// and this reds — which is the point, because the window would then embed
    /// hyperhive's full page inside our chrome and draw two headers.
    #[test]
    fn a_plain_agent_url_gains_the_hide_query() {
        assert_eq!(
            embed_url("https://hive.local/agent/trollshell-choom/"),
            "https://hive.local/agent/trollshell-choom/?hide=header"
        );
    }

    /// A URL that **already** carries a query gets `&`, not a second `?`.
    ///
    /// Falsification: hard-code `?` and this reds; the server would
    /// see one parameter named `…?hide` and hide nothing.
    #[test]
    fn an_existing_query_gets_an_ampersand() {
        assert_eq!(
            embed_url("https://hive.local/agent/stray/?tab=todos"),
            "https://hive.local/agent/stray/?tab=todos&hide=header"
        );
        assert_eq!(
            embed_url("https://hive.local/agent/stray/?"),
            "https://hive.local/agent/stray/?&hide=header",
            "a bare trailing ? is still a query, so it is still an &"
        );
    }

    /// A fragment stays at the end — a query written after one never reaches
    /// the server.
    ///
    /// Falsification: append unconditionally and the parameter lands
    /// inside the fragment.
    #[test]
    fn a_fragment_stays_last() {
        assert_eq!(
            embed_url("https://hive.local/agent/stray/#turn-3"),
            "https://hive.local/agent/stray/?hide=header#turn-3"
        );
        assert_eq!(
            embed_url("https://hive.local/agent/stray/?a=1#turn-3"),
            "https://hive.local/agent/stray/?a=1&hide=header#turn-3"
        );
    }

    /// Surrounding whitespace is dropped — the hive's own value is trimmed
    /// everywhere else it is read (`model::agent_url`), and a URL with a
    /// leading space loads nothing.
    #[test]
    fn the_hives_value_is_trimmed() {
        assert_eq!(
            embed_url("  https://hive.local/agent/stray/  "),
            "https://hive.local/agent/stray/?hide=header"
        );
    }

    /// The constant is the value the hive shipped, in the spelling it shipped
    /// it — no spaces, `hide=` first.
    ///
    /// A reader would otherwise have to trust the prose above; this is the
    /// line @the-sword-above's answer pins.
    #[test]
    fn the_constant_is_the_parameter_the_hive_shipped() {
        assert_eq!(HIDE_QUERY, "hide=header");
    }

    /// **The composer is not hidden** — #1282 item 1, and the one assertion
    /// that would catch the old value coming back by a merge or a revert.
    ///
    /// It is deliberately written against the *substring* rather than against
    /// the whole constant: the hive's `?hide=` is a comma-separated list whose
    /// unknown values are ignored, so `header,input`, `input,header` and
    /// `input` all hide the footer, and only "no `input` anywhere in what we
    /// send" says what this window now promises. The embedded URL is asserted
    /// too, because that — not the constant — is the string `WebKit` loads.
    ///
    /// Falsification (run this round, red): restore `"hide=header,input"` and
    /// both halves red on the rendered URL and on the constant.
    #[test]
    fn the_embedded_page_keeps_its_input() {
        assert!(
            !HIDE_QUERY.contains("input"),
            "the composer must not be suppressed: {HIDE_QUERY}"
        );
        let url = embed_url("https://hive.local/agent/stray/");
        assert!(!url.contains("input"), "{url}");
        assert_eq!(url, "https://hive.local/agent/stray/?hide=header");
    }

    /// The agent's page, as the window embeds it.
    const PAGE: &str = "https://hive.local/agent/stray/?hide=header";

    /// URIs an injected turn stream would aim at — the review's
    /// `https://evil/…`, its plain-http twin, and the lookalikes
    /// [`a_lookalike_host_is_not_the_hive`] already refuses in place.
    const OFF_THE_HIVE: [&str; 5] = [
        "https://evil.example/phish",
        "http://evil.example/phish",
        "https://hive.local.evil.example/login",
        "https://hive.local@evil.example/login",
        "http://hive.local/agent/stray/",
    ];

    /// **Nothing reaches the desktop without a click** — the review's
    /// finding, as a table: an `<iframe src>`, a `<meta http-equiv=refresh>`
    /// or a scripted `location =` (all [`Target::ThisView`], no gesture) and a
    /// `window.open` loop or a script-clicked `target="_blank"` (both
    /// [`Target::NewWindow`], no gesture) each used to open a browser tab on
    /// whatever the page chose.
    ///
    /// Mutation (run this round, red): make `decide`'s `!user_gesture` test
    /// `false` — the hand-everything-over behaviour this window shipped with
    /// — and this reds with `Desktop` on its first row.
    #[test]
    fn nothing_reaches_the_desktop_without_a_click() {
        for uri in OFF_THE_HIVE {
            for target in [Target::ThisView, Target::NewWindow] {
                assert_eq!(
                    decide(PAGE, uri, target, false),
                    Decision::Refuse(Refusal::NoGesture),
                    "{target:?} to {uri} with nobody clicking must open nothing"
                );
            }
        }
    }

    /// …and a click still does: a link in the feed that points off the hive
    /// opens in the browser, whether it targets this view or a new window.
    ///
    /// Mutation (run this round, red): return `Refuse(NoGesture)` where
    /// `decide` returns `Desktop` — "refuse everything off the hive", the
    /// over-correction that would make every link in the feed dead — and
    /// this reds (as does [`a_new_window_never_loads_in_place`]).
    #[test]
    fn a_click_off_the_hive_goes_to_the_desktop() {
        for uri in OFF_THE_HIVE {
            for target in [Target::ThisView, Target::NewWindow] {
                assert_eq!(
                    decide(PAGE, uri, target, true),
                    Decision::Desktop,
                    "a clicked {target:?} link to {uri} opens in the browser"
                );
            }
        }
    }

    /// The hive's own origin loads **in place, gesture or not** — the
    /// window's first `load_uri` carries no gesture, and neither do the
    /// hive's own scripted navigations or its frames.
    ///
    /// Mutation (run this round, red): require a gesture for the in-place
    /// answer too (`user_gesture && target == …`), and this reds with
    /// `Refuse(NoGesture)` on the first ungestured row — a window that never
    /// loads its own page.
    #[test]
    fn the_hive_loads_in_place_with_or_without_a_gesture() {
        for uri in [
            PAGE,
            "https://hive.local/",
            "https://hive.local/agent/other/",
            "https://hive.local:8443/agent/stray/turn/3",
        ] {
            for gesture in [false, true] {
                assert_eq!(
                    decide(PAGE, uri, Target::ThisView, gesture),
                    Decision::InPlace,
                    "{uri} (gesture: {gesture}) is the hive"
                );
            }
        }
    }

    /// A **new window** never loads in place, even on the hive's own origin:
    /// there is no second window here for it to become. Clicked, it goes to
    /// the browser — what a `target="_blank"` link into the hive did before
    /// this, through `create` — and unclicked it goes nowhere.
    ///
    /// Mutation (run this round, red): drop `decide`'s
    /// `target == Target::ThisView` conjunct and this reds with `InPlace` —
    /// which `create` carries out as nothing at all, so a clicked `_blank`
    /// link into the hive would silently go nowhere.
    #[test]
    fn a_new_window_never_loads_in_place() {
        let ours = "https://hive.local/agent/other/";
        assert_eq!(
            decide(PAGE, ours, Target::NewWindow, true),
            Decision::Desktop
        );
        assert_eq!(
            decide(PAGE, ours, Target::NewWindow, false),
            Decision::Refuse(Refusal::NoGesture)
        );
    }

    /// Even a click never hands a launcher anything but http(s): the review
    /// kept `file:` and custom schemes out, and the gesture does not let them
    /// back in.
    ///
    /// Mutation (run this round, red): make `decide`'s `may_leave` test
    /// `false` and this reds with `Desktop` on `file:///etc/passwd` — at
    /// which point `elsewhere`'s own check is the only thing between the
    /// string and the launcher.
    #[test]
    fn a_click_hands_the_desktop_only_http() {
        for uri in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,<h1>hi",
            "mailto:someone@example.org",
            "ssh://hive.local/",
            "steam://run/1",
            "about:blank",
            "HTTPS://evil.example/",
            "",
        ] {
            for target in [Target::ThisView, Target::NewWindow] {
                assert_eq!(
                    decide(PAGE, uri, target, true),
                    Decision::Refuse(Refusal::Scheme),
                    "{uri:?} must never reach a launcher"
                );
            }
            // …and without a click it is the gesture that says no first, so
            // an `about:blank` frame logs a debug line, not a warning.
            assert_eq!(
                decide(PAGE, uri, Target::ThisView, false),
                Decision::Refuse(Refusal::NoGesture)
            );
        }
        assert!(may_leave("https://evil.example/"));
        assert!(may_leave("http://evil.example/"));
    }

    /// The host comparison is **exact** and fails closed: another spelling of
    /// the hive's name is not the hive, and a URL whose authority two parsers
    /// read differently has no host at all.
    ///
    /// - Upper case never reaches this from the view — `WebKit` lower-cases
    ///   the host of what it passes `decide-policy` — so the row says only
    ///   that if one ever does, the answer is "not here" rather than a guess.
    /// - A trailing dot does reach it, and `hive.local.` is a different
    ///   origin to `WebKit` as well, so refusing it in place agrees with the
    ///   engine.
    /// - The backslash row is the one that matters: RFC 3986 (`GUri`) reads
    ///   `hive.local` as its host, WHATWG (`WebKit`) reads `evil.example`,
    ///   so `tls::host_of` answers `None` for it. `WebKit` would hand the
    ///   policy its canonical form, `https://evil.example/@hive.local/`,
    ///   which both parsers read the same way — the last row.
    #[test]
    fn an_uncanonical_spelling_of_the_hive_is_not_the_hive() {
        for spelled in [
            "https://HIVE.local/agent/stray/",
            "https://hive.local./agent/stray/",
            "https://evil.example\\@hive.local/",
            "https://evil.example/@hive.local/",
        ] {
            assert!(
                !navigable_in_place(PAGE, spelled),
                "{spelled} must not load in this window"
            );
        }
    }

    /// **A load of the hive's own page that nothing answered is asked
    /// again** — #1557 item 2.
    ///
    /// The rows are what `load-failed` hands over on the routes with no
    /// probe: the window's own first load (the embed URL, as `WebKit` spells
    /// it), a page the hive's frontend moved to, a deep link with its own
    /// query, and the same hive on its own port. Before this nothing reloaded
    /// any of them, so a gateway that was down at open left `WebKit`'s error
    /// page up for good.
    ///
    /// Mutation (run this round, red): make `reload_after` answer `false`
    /// for `NoAnswer` — no reload, the bug as it stood — and the first row
    /// reds.
    #[test]
    fn a_hive_page_nothing_answered_is_loaded_again() {
        for failing in [
            PAGE,
            "https://hive.local/agent/stray/turn/3",
            "https://hive.local/agent/other/?tab=todos",
            "https://hive.local:8443/agent/stray/",
        ] {
            assert!(
                reload_after(PAGE, failing, LoadFailure::NoAnswer),
                "{failing} is the hive's own page and nothing answered it: ask again"
            );
        }
    }

    /// …and **nothing else is**: not a certificate failure, not a
    /// cancellation, and not anything off the hive, whatever its load failed
    /// with.
    ///
    /// The certificate rows are the route the TLS card owns — asking again
    /// would be handed the same certificate, and the card is the operator's
    /// to act on — and a cancellation is a newer load or the policy having
    /// already decided. The off-hive rows are the reload-loop guard: a URI
    /// the policy cancels can never be the one the window keeps reloading,
    /// even if a failure for it somehow reached `load-failed`.
    ///
    /// Mutations (run this round, red): drop the `failure ==
    /// LoadFailure::NoAnswer` conjunct and the `Tls` row reds; drop the
    /// `navigable_in_place` conjunct and the first off-hive row reds.
    #[test]
    fn only_no_answer_on_the_hive_is_loaded_again() {
        for failure in [LoadFailure::Tls, LoadFailure::Other] {
            assert!(
                !reload_after(PAGE, PAGE, failure),
                "{failure:?} is not asked again — the answer would be the same"
            );
        }
        for hostile in OFF_THE_HIVE.into_iter().chain([
            "file:///etc/passwd",
            "about:blank",
            "data:text/html,<h1>hi",
            "",
        ]) {
            assert!(
                !reload_after(PAGE, hostile, LoadFailure::NoAnswer),
                "{hostile:?} is not the hive: it must never be the URI the window reloads"
            );
        }
        assert!(
            !reload_after("", PAGE, LoadFailure::NoAnswer),
            "a window with no origin of its own reloads nothing"
        );
    }
}
