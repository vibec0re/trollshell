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
/// for. Everything else is handed to the desktop's default handler and
/// cancelled in the view, which keeps a legitimate link in the feed working —
/// just not here.
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
#[must_use]
pub fn navigable_in_place(embedded: &str, candidate: &str) -> bool {
    candidate.starts_with("https://")
        && crate::tls::host_of(embedded).is_some()
        && crate::tls::host_of(candidate) == crate::tls::host_of(embedded)
}

#[cfg(test)]
mod tests {
    use super::{HIDE_QUERY, embed_url, navigable_in_place};

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
    /// Mutation (re-run this round, red): drop
    /// `crate::tls::host_of(embedded).is_some()` and the `https://` pair
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
    /// too, because that — not the constant — is the string WebKit loads.
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
}
