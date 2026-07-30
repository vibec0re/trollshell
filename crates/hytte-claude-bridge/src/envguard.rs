//! The billing guard: refuse to run if the environment could silently move
//! Claude Code off the subscription and onto metered API credits.
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
//! `etc/systemd/user/trollshell-claude-bridge.service` — and this module makes
//! that unit setting non-optional by refusing to start when it did not take
//! effect. Failing closed is the correct direction for a billing control: a
//! bridge that will not start is loud, whereas a bridge that quietly bills to
//! metered credits is not.

/// Environment variables that would redirect the `claude` child away from the
/// subscription:
///
/// - `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` — bill to metered API credits
///   instead of the OAuth subscription session.
/// - `CLAUDE_CODE_USE_BEDROCK` / `CLAUDE_CODE_USE_VERTEX` — bill to a cloud
///   provider account entirely.
pub const BILLING_REDIRECTS: [&str; 4] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
];

/// Variables in [`BILLING_REDIRECTS`] that are booleans rather than
/// credentials, so an explicit off-value is genuinely harmless and must not
/// trip the guard (a unit or profile that pins `CLAUDE_CODE_USE_BEDROCK=0` is
/// asserting the *right* thing).
const BOOLEAN_FLAGS: [&str; 2] = ["CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX"];

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

/// The subset of [`BILLING_REDIRECTS`] that `lookup` reports as set to a
/// redirecting value.
///
/// Takes the lookup as a parameter so it is testable without mutating the
/// process environment (which is `unsafe` under edition 2024) — the same shape
/// `hytte_ai_providers::load_key_from` uses for the identical reason.
pub fn offenders(lookup: impl Fn(&str) -> Option<String>) -> Vec<&'static str> {
    BILLING_REDIRECTS
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
         These would move `claude` off the Claude subscription and onto metered \
         API credits (or Bedrock/Vertex) without any visible sign.\n\
         The shipped unit scrubs them with `UnsetEnvironment=`; if you are running \
         the bridge by hand, unset them first.",
        found.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::{BILLING_REDIRECTS, offenders, refusal};

    /// The list itself is the security control — pin it so a variable cannot be
    /// dropped from it in passing.
    #[test]
    fn the_scrub_list_is_exactly_the_four_billing_redirects() {
        assert_eq!(
            BILLING_REDIRECTS,
            [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX",
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

    /// The Bedrock/Vertex flags are booleans: an explicit off-value asserts the
    /// right thing and must not block startup.
    #[test]
    fn explicitly_disabled_bedrock_and_vertex_are_not_offenders() {
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
