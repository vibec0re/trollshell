//! The URL the WebKitGTK view loads: hyperhive's own agent page, with the
//! chrome it would draw for itself suppressed.
//!
//! # `?hide=header,input`
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
//! > for the WebKitGTK-embedded view). Unknown values in the list are ignored.
//!
//! So this is **one constant**, and everything else about the page is the
//! hive's business. The window's own chrome — the header with the live status
//! read from `host.sock`, start/stop/pause, the settings tab — replaces
//! exactly the two things it hides.
//!
//! Unknown values being ignored is what let the flag ship before the feature
//! did; it is also why a rename on the hive's side costs one line here.

/// The query the embedded view appends — the whole of this window's contract
/// with hyperhive's frontend.
pub const HIDE_QUERY: &str = "hide=header,input";

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

#[cfg(test)]
mod tests {
    use super::{HIDE_QUERY, embed_url};

    /// The ordinary case: the hive's `https://<domain>/agent/<name>/`.
    ///
    /// Mutation (verified red): drop the parameter (return `base` unchanged)
    /// and this reds — which is the point, because the window would then embed
    /// hyperhive's full page inside our chrome and draw two headers.
    #[test]
    fn a_plain_agent_url_gains_the_hide_query() {
        assert_eq!(
            embed_url("https://hive.local/agent/trollshell-choom/"),
            "https://hive.local/agent/trollshell-choom/?hide=header,input"
        );
    }

    /// A URL that **already** carries a query gets `&`, not a second `?`.
    ///
    /// Mutation (verified red): hard-code `?` and this reds; the server would
    /// see one parameter named `…?hide` and hide nothing.
    #[test]
    fn an_existing_query_gets_an_ampersand() {
        assert_eq!(
            embed_url("https://hive.local/agent/stray/?tab=todos"),
            "https://hive.local/agent/stray/?tab=todos&hide=header,input"
        );
        assert_eq!(
            embed_url("https://hive.local/agent/stray/?"),
            "https://hive.local/agent/stray/?&hide=header,input",
            "a bare trailing ? is still a query, so it is still an &"
        );
    }

    /// A fragment stays at the end — a query written after one never reaches
    /// the server.
    ///
    /// Mutation (verified red): append unconditionally and the parameter lands
    /// inside the fragment.
    #[test]
    fn a_fragment_stays_last() {
        assert_eq!(
            embed_url("https://hive.local/agent/stray/#turn-3"),
            "https://hive.local/agent/stray/?hide=header,input#turn-3"
        );
        assert_eq!(
            embed_url("https://hive.local/agent/stray/?a=1#turn-3"),
            "https://hive.local/agent/stray/?a=1&hide=header,input#turn-3"
        );
    }

    /// Surrounding whitespace is dropped — the hive's own value is trimmed
    /// everywhere else it is read (`model::agent_url`), and a URL with a
    /// leading space loads nothing.
    #[test]
    fn the_hives_value_is_trimmed() {
        assert_eq!(
            embed_url("  https://hive.local/agent/stray/  "),
            "https://hive.local/agent/stray/?hide=header,input"
        );
    }

    /// The constant is the two values the hive shipped, in the spelling it
    /// shipped them — no spaces, comma-separated, `hide=` first.
    ///
    /// A reader would otherwise have to trust the prose above; this is the
    /// line @the-sword-above's answer pins.
    #[test]
    fn the_constant_is_the_parameter_the_hive_shipped() {
        assert_eq!(HIDE_QUERY, "hide=header,input");
    }
}
