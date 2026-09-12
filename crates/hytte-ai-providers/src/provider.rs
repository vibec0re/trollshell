//! Resolving a plugin's chat [`Provider`] from its env inputs (#1168).
//!
//! `pet` and `caw` each hand-rolled this **verbatim** (same match arms, same
//! `OpenRouter` URL, same struct shape — only the doc-comment wording and the
//! plugin's own `PLUGIN_ID` differed), with #438's semantics repeated in both
//! doc comments rather than stated once. Read both before touching this: if
//! they had drifted, that drift would be the finding to report, but they had
//! not — this is a byte-for-byte hoist.

use crate::Provider;

/// Resolve a plugin's [`Provider`] from its env inputs (#438's semantics).
///
/// `url_env` is the raw `$FOO_LLM_URL` value: `None` = unset, `Some("")` =
/// set-but-empty = model explicitly disabled. With no explicit URL, the
/// default is the **`OpenRouter`** cloud endpoint, but **only when `key` is
/// `Some`** — a keyless `OpenRouter` call always 401s, so with neither a URL
/// nor a key this short-circuits to `None` (canned/plain-only) rather than
/// burning a doomed round-trip per call. An explicit `$FOO_LLM_URL` selects a
/// local/self-hosted backend (e.g. a `llama-server`, which needs no key) as
/// the base, kept even without a key, with any `key`/`model` layered on.
///
/// `plugin_id` is stamped onto [`Provider::user`] so `hytte-claude-bridge`
/// can tell this caller's conversation from any other plugin's (#704) —
/// callers must keep their `plugin_id` distinct from every other plugin's;
/// see the pet's and caw's own `PLUGIN_ID` docs for why that can only be
/// enforced by convention, not a test, across separate crates.
#[must_use]
pub fn resolve(
    url_env: Option<&str>,
    key: Option<String>,
    model: Option<String>,
    plugin_id: &str,
) -> Option<Provider> {
    match url_env {
        // Explicitly empty `$FOO_LLM_URL` → model disabled.
        Some("") => None,
        // An explicit URL is a local/self-hosted backend that needs no key —
        // keep it even keyless.
        Some(url) => Some(Provider {
            base_url: url.to_owned(),
            api_key: key,
            model,
            user: Some(plugin_id.to_owned()),
        }),
        // No URL → the OpenRouter cloud default, but ONLY with a key. Keyless
        // → `None` (canned/plain-only): the call would just 401, so skip it
        // (#438).
        None => key.map(|key| Provider {
            base_url: "https://openrouter.ai/api".to_owned(),
            api_key: Some(key),
            model,
            user: Some(plugin_id.to_owned()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::resolve;

    /// Falsification target: comment out the `Some("")` arm (or the `key.map`
    /// short-circuit) and either this test or the pet's/caw's own
    /// `resolve_provider_*` test reds — see the PR for the falsification run.
    #[test]
    fn empty_url_disables_the_model() {
        assert!(resolve(Some(""), None, None, "x").is_none());
        assert!(resolve(Some(""), Some("k".to_owned()), None, "x").is_none());
    }

    #[test]
    fn explicit_url_is_kept_even_keyless() {
        let p = resolve(Some("http://host:1"), None, Some("m".to_owned()), "x").unwrap();
        assert_eq!(p.base_url, "http://host:1");
        assert!(p.api_key.is_none());
        assert_eq!(p.model.as_deref(), Some("m"));
    }

    #[test]
    fn no_url_defaults_to_openrouter_only_with_a_key() {
        let p = resolve(None, Some("sk-1".to_owned()), Some("gpt".to_owned()), "x").unwrap();
        assert_eq!(p.base_url, "https://openrouter.ai/api");
        assert_eq!(p.api_key.as_deref(), Some("sk-1"));
        assert!(resolve(None, None, None, "x").is_none());
        assert!(resolve(None, None, Some("gpt".to_owned()), "x").is_none());
    }

    #[test]
    fn user_is_stamped_with_the_plugin_id() {
        let p = resolve(Some("http://host:1"), None, None, "pet").unwrap();
        assert_eq!(p.user.as_deref(), Some("pet"));
        let p = resolve(None, Some("sk-1".to_owned()), None, "caw").unwrap();
        assert_eq!(p.user.as_deref(), Some("caw"));
    }
}
