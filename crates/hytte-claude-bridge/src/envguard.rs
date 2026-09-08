//! The billing guard: refuse to run if the environment could silently move
//! Claude Code off the subscription and onto metered API credits — or, since
//! #994, onto a different network endpoint entirely while still presenting
//! the subscription's own credential.
//!
//! # Why this fails closed instead of scrubbing
//!
//! The design calls for these variables to be **scrubbed from the child
//! environment**. Two constraints in this tree make that unreachable from
//! inside the process:
//!
//! - `std::env::remove_var` is `unsafe` under edition 2024, and this workspace
//!   is `unsafe_code = "forbid"` (only `hytte-ecal` overrides it, for FFI).
//! - `hive_claude::Config` exposes no environment hook — its driver builds the
//!   `tokio::process::Command` itself, so there is no `env_remove` seam for a
//!   consumer to reach. (Worth an upstream ask; not worth a shim script here.)
//!
//! So the scrub happens where it *can* happen — `UnsetEnvironment=` in
//! `etc/systemd/user/trollshell-claude-bridge.service`, and the home-manager
//! launcher path's equivalent empty-string `env` entries — and this module
//! makes that scrub non-optional by refusing to start when it did not take
//! effect. Failing closed is the correct direction for a billing/redirect
//! control: a bridge that will not start is loud, whereas a bridge that
//! quietly bills to metered credits, or quietly ships its traffic and
//! credential to a third-party host, is not.
//!
//! # It is scoped to the modes that spawn `claude` (#730)
//!
//! Every variable here redirects **the child**. `CLAUDE_BRIDGE_MODE=api`
//! spawns no child: it is the mode you pick *because* you want to be billed per
//! token, and `ANTHROPIC_API_KEY` is the credential it runs on rather than a
//! silent redirect away from something else. So `main` runs this guard only
//! when `Mode::spawns_claude()`, and a test there pins that scoping — if it
//! ever widened back to unconditional, the API backend could not be configured
//! through its env override at all.

/// Environment variables that would redirect the `claude` child away from the
/// subscription — either onto a different billing account, or (#994) onto a
/// different network endpoint while it keeps authenticating with the
/// subscription's own OAuth session, which is strictly worse: the traffic
/// *and* the credential leave for a third-party host with no visible sign.
///
/// - `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` — bill to metered API credits
///   instead of the OAuth subscription session.
/// - `CLAUDE_CODE_USE_BEDROCK` / `CLAUDE_CODE_USE_VERTEX` /
///   `CLAUDE_CODE_USE_FOUNDRY` — bill to a cloud provider account entirely.
///   Foundry is documented as the third member of this set, not just the
///   first two: "Cloud provider credentials, when `CLAUDE_CODE_USE_BEDROCK`,
///   `CLAUDE_CODE_USE_VERTEX`, or `CLAUDE_CODE_USE_FOUNDRY` is set." —
///   <https://code.claude.com/docs/en/authentication#authentication-precedence>
/// - `ANTHROPIC_BASE_URL` / `ANTHROPIC_BEDROCK_BASE_URL` /
///   `ANTHROPIC_VERTEX_BASE_URL` — override the endpoint host directly,
///   independent of which of the above (if any) is active. The credential
///   management docs say plainly: "Claude Code manages `.credentials.json`
///   through `/login` and `/logout`. To route requests through a custom API
///   endpoint, set the `ANTHROPIC_BASE_URL` environment variable instead." —
///   <https://code.claude.com/docs/en/authentication#credential-management>.
///   `ANTHROPIC_BEDROCK_BASE_URL` ("Override the Amazon Bedrock endpoint URL
///   … or when routing through an LLM gateway") and `ANTHROPIC_VERTEX_BASE_URL`
///   ("Override Google Cloud's Agent Platform endpoint URL … or when routing
///   through an LLM gateway") are documented the same way, in the same table:
///   <https://code.claude.com/docs/en/env-vars>. This is the exact #994
///   failing input: `ANTHROPIC_BASE_URL=http://127.0.0.1:9999` starts cleanly
///   without this entry, where `ANTHROPIC_API_KEY=x` already refuses.
pub const REDIRECT_VARS: [&str; 8] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_VERTEX_BASE_URL",
];

/// Variables in [`REDIRECT_VARS`] that are booleans rather than credentials,
/// so an explicit off-value is genuinely harmless and must not trip the guard
/// (a unit or profile that pins `CLAUDE_CODE_USE_BEDROCK=0` is asserting the
/// *right* thing). The `*_BASE_URL` family is deliberately **not** here: an
/// endpoint override has no "off" spelling short of being unset — see
/// [`redirects`].
const BOOLEAN_FLAGS: [&str; 3] = [
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Values that read as "off" for the boolean flags above.
const OFF_VALUES: [&str; 3] = ["", "0", "false"];

/// Whether one variable's value would actually redirect billing.
fn redirects(name: &str, value: &str) -> bool {
    let trimmed = value.trim();
    if BOOLEAN_FLAGS.contains(&name) {
        !OFF_VALUES
            .iter()
            .any(|off| trimmed.eq_ignore_ascii_case(off))
    } else {
        !trimmed.is_empty()
    }
}

/// The subset of [`REDIRECT_VARS`] that `lookup` reports as set to a
/// redirecting value.
///
/// Takes the lookup as a parameter so it is testable without mutating the
/// process environment (which is `unsafe` under edition 2024) — the same shape
/// `hytte_ai_providers::load_key_from` uses for the identical reason.
pub fn offenders(lookup: impl Fn(&str) -> Option<String>) -> Vec<&'static str> {
    REDIRECT_VARS
        .into_iter()
        .filter(|name| lookup(name).is_some_and(|v| redirects(name, &v)))
        .collect()
}

/// [`offenders`] against the real process environment.
#[must_use]
pub fn offenders_in_env() -> Vec<&'static str> {
    offenders(|name| std::env::var(name).ok())
}

/// The message printed before exiting when the guard trips. Spelled out because
/// the only place anyone will read it is a `systemctl status` tail.
#[must_use]
pub fn refusal(found: &[&'static str]) -> String {
    format!(
        "refusing to start: {} set in the environment.\n\
         These would move `claude` off the Claude subscription — onto metered \
         API credits, a cloud provider's billing (Bedrock/Vertex/Foundry), or a \
         different endpoint entirely — without any visible sign.\n\
         The shipped unit scrubs them with `UnsetEnvironment=`; if you are running \
         the bridge by hand, unset them first.",
        found.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::{REDIRECT_VARS, offenders, refusal};

    /// The list itself is the security control — pin it so a variable cannot be
    /// dropped from it in passing.
    #[test]
    fn the_scrub_list_is_exactly_the_eight_redirect_vars() {
        assert_eq!(
            REDIRECT_VARS,
            [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX",
                "CLAUDE_CODE_USE_FOUNDRY",
                "ANTHROPIC_BASE_URL",
                "ANTHROPIC_BEDROCK_BASE_URL",
                "ANTHROPIC_VERTEX_BASE_URL",
            ]
        );
    }

    /// A clean environment starts.
    #[test]
    fn a_clean_environment_has_no_offenders() {
        assert!(offenders(|_| None).is_empty());
    }

    /// Any non-empty credential trips the guard.
    #[test]
    fn a_set_api_key_is_an_offender() {
        let found = offenders(|n| (n == "ANTHROPIC_API_KEY").then(|| "sk-live".to_owned()));
        assert_eq!(found, vec!["ANTHROPIC_API_KEY"]);
    }

    /// An empty credential is not a redirect — an exported-but-blank variable
    /// is what a shell profile leaves behind and Claude Code ignores it.
    #[test]
    fn a_blank_credential_is_not_an_offender() {
        assert!(offenders(|n| (n == "ANTHROPIC_AUTH_TOKEN").then(|| "   ".to_owned())).is_empty());
    }

    /// The Bedrock/Vertex/Foundry flags are booleans: an explicit off-value
    /// asserts the right thing and must not block startup.
    #[test]
    fn explicitly_disabled_cloud_provider_flags_are_not_offenders() {
        for off in ["0", "false", "FALSE", ""] {
            assert!(
                offenders(|n| n.starts_with("CLAUDE_CODE_USE_").then(|| off.to_owned())).is_empty(),
                "{off:?} should read as off"
            );
        }
    }

    /// …but any on-value is a redirect.
    #[test]
    fn enabled_bedrock_is_an_offender() {
        let found = offenders(|n| (n == "CLAUDE_CODE_USE_BEDROCK").then(|| "1".to_owned()));
        assert_eq!(found, vec!["CLAUDE_CODE_USE_BEDROCK"]);
    }

    /// #994's exact failing input: a base-URL redirect used to start cleanly
    /// where an API key already refused. `ANTHROPIC_BASE_URL` in particular is
    /// the "quietest" one — traffic and credential both move, with nothing
    /// else in the environment to suggest it.
    #[test]
    fn a_set_anthropic_base_url_is_an_offender() {
        let found =
            offenders(|n| (n == "ANTHROPIC_BASE_URL").then(|| "http://127.0.0.1:9999".to_owned()));
        assert_eq!(found, vec!["ANTHROPIC_BASE_URL"]);
    }

    /// The Bedrock/Vertex endpoint overrides are credential-style too: any
    /// non-empty value is an offender, with no boolean off-value carve-out.
    #[test]
    fn the_cloud_base_url_overrides_are_offenders_when_set() {
        for name in ["ANTHROPIC_BEDROCK_BASE_URL", "ANTHROPIC_VERTEX_BASE_URL"] {
            let found = offenders(|n| (n == name).then(|| "https://evil.example".to_owned()));
            assert_eq!(found, vec![name], "{name}");
        }
    }

    /// An empty base-URL variable is what an unset-but-exported shell profile
    /// entry (or this crate's own home-manager scrub) leaves behind, and must
    /// not trip the guard.
    #[test]
    fn an_empty_base_url_is_not_an_offender() {
        for name in [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_BEDROCK_BASE_URL",
            "ANTHROPIC_VERTEX_BASE_URL",
        ] {
            assert!(
                offenders(|n| (n == name).then(|| String::new())).is_empty(),
                "{name}"
            );
        }
    }

    /// Every offender is named in the refusal, so `systemctl status` says which
    /// one to unset.
    #[test]
    fn the_refusal_names_every_offender() {
        let found = vec!["ANTHROPIC_API_KEY", "CLAUDE_CODE_USE_VERTEX"];
        let msg = refusal(&found);
        assert!(msg.contains("ANTHROPIC_API_KEY"));
        assert!(msg.contains("CLAUDE_CODE_USE_VERTEX"));
        assert!(msg.contains("UnsetEnvironment="));
    }
}
