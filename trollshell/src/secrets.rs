//! AI-provider key store (#392): the shell's thin async client over the login
//! keyring (Secret Service / gnome-keyring, via [`oo7`]).
//!
//! ## Model
//!
//! A **slot** is a provider name — the same string
//! `hytte_ai_providers::load_key` takes (e.g. `"openrouter"`). Each slot's API
//! key lives as one keyring item,
//! tagged with the attributes [`base_attrs`] + `slot`, so this crate can find,
//! replace, and delete a provider's key without ever putting it on disk or in a
//! config file. The **control-center's AI Keys tab** writes these (via the
//! [`crate::control`] endpoint's `SetAiKey`/`ClearAiKey`/`ListAiKeys`), and the
//! **launcher** ([`crate::plugin_launcher`]) reads them at plugin spawn.
//!
//! ## The injection contract (why this needs no per-plugin plumbing)
//!
//! [`env_var_for`] maps a slot to the env var the launcher injects at spawn:
//! `"openrouter"` → `OPENROUTER_API_KEY`. That is *exactly* the `{NAME}_API_KEY`
//! override `hytte_ai_providers::load_key(name)` checks before its key file — so
//! an LLM-backed plugin (pet, caw) that already calls `load_key("openrouter")`
//! picks the injected key up with **zero** plugin-side changes. A plugin opts in
//! by listing the slot in its `plugins.json` `secrets` allowlist; a plugin that
//! doesn't list it never sees the key (secret hygiene — the terminal plugin
//! shouldn't get the `OpenRouter` key in its environment).
//!
//! ## Secrets never leak
//!
//! Values are only ever passed between the keyring and a spawned plugin's
//! environment; they are **never logged** (only the slot name is), never
//! returned by the list accessor, and reads fail *closed* — an unreadable or
//! absent key resolves to "no key" (the plugin runs keyless, its own fallback
//! applies) rather than surfacing anything. Every op is time-bounded so a locked
//! or wedged keyring can't hang shell startup.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context as _;
use oo7::Keyring;

/// Keyring item attribute: the owning application (namespaces trollshell's
/// items apart from every other keyring user).
const APP_ATTR: &str = "mov.vibec0re.trollshell";
/// Keyring item attribute: what kind of secret this is.
const TYPE_ATTR: &str = "ai-api-key";

/// Upper bound on any single keyring operation. Secret Service calls can block
/// on an unlock prompt or a wedged daemon; a bound keeps that from stalling a
/// plugin launch (a read that times out resolves to "no key", see [`get`]).
const OP_TIMEOUT: Duration = Duration::from_secs(10);

/// The attributes shared by every trollshell AI-key item — the search key for
/// [`list`] (all slots) and the base [`slot_attrs`] extends per slot.
fn base_attrs() -> HashMap<&'static str, &'static str> {
    HashMap::from([("application", APP_ATTR), ("type", TYPE_ATTR)])
}

/// The full attribute set identifying one provider's key.
fn slot_attrs(slot: &str) -> HashMap<&str, &str> {
    let mut attrs = base_attrs();
    attrs.insert("slot", slot);
    attrs
}

/// The environment variable a stored key for `slot` is injected as, matching
/// `hytte_ai_providers::load_key`'s `{NAME}_API_KEY` override: uppercased, with
/// `-` normalized to `_` so the result is a valid env-var name. `"openrouter"` →
/// `"OPENROUTER_API_KEY"`. Pure, so the contract is unit-testable.
#[must_use]
pub fn env_var_for(slot: &str) -> String {
    format!("{}_API_KEY", slot.to_ascii_uppercase().replace('-', "_"))
}

/// A valid slot name: a provider identifier safe to splice into an env-var name
/// (`{SLOT}_API_KEY`) — non-empty, bounded, starting with an ASCII letter and
/// otherwise lowercase-alphanumeric plus `-`/`_`. The launcher drops any
/// declared `secrets` entry that fails this (a nix-written file should never
/// trip it). Pure.
#[must_use]
pub fn is_valid_slot(slot: &str) -> bool {
    !slot.is_empty()
        && slot.len() <= 64
        && slot.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && slot
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Open + unlock the default keyring collection. `unlock` is a no-op when the
/// collection is already unlocked (gnome-keyring auto-unlocks at login via PAM,
/// the normal case); it can prompt when locked, which [`OP_TIMEOUT`] bounds.
async fn keyring() -> anyhow::Result<Keyring> {
    let kr = tokio::time::timeout(OP_TIMEOUT, Keyring::new())
        .await
        .context("opening the secret service timed out")?
        .context("opening the secret service")?;
    tokio::time::timeout(OP_TIMEOUT, kr.unlock())
        .await
        .context("unlocking the keyring timed out")?
        .context("unlocking the keyring")?;
    Ok(kr)
}

/// Store `value` as the API key for `slot`, replacing any existing key for it.
///
/// # Errors
/// A keyring that can't be opened, unlocked, or written (each time-bounded).
pub async fn set(slot: &str, value: &str) -> anyhow::Result<()> {
    let kr = keyring().await?;
    let label = format!("trollshell AI key: {slot}");
    tokio::time::timeout(
        OP_TIMEOUT,
        kr.create_item(&label, &slot_attrs(slot), value, true),
    )
    .await
    .context("storing the key timed out")?
    .context("storing the key")?;
    Ok(())
}

/// Delete the stored key for `slot`. Idempotent — clearing an absent slot is
/// `Ok` (the Secret Service delete of a no-match is a no-op).
///
/// # Errors
/// A keyring that can't be opened, unlocked, or written.
pub async fn clear(slot: &str) -> anyhow::Result<()> {
    let kr = keyring().await?;
    tokio::time::timeout(OP_TIMEOUT, kr.delete(&slot_attrs(slot)))
        .await
        .context("clearing the key timed out")?
        .context("clearing the key")?;
    Ok(())
}

/// The stored key for `slot`, or `None` if unset **or unreadable**. Fails
/// *closed*: any error resolves to `None` (logged with the slot but never the
/// value) so a plugin launch degrades to keyless rather than aborting.
pub async fn get(slot: &str) -> Option<String> {
    match read(slot).await {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(slot = %slot, %err, "reading AI key from the keyring failed; treating as unset");
            None
        }
    }
}

/// Fallible core of [`get`]: `Ok(None)` = genuinely unset, `Err` = a keyring
/// problem the caller downgrades to unset.
async fn read(slot: &str) -> anyhow::Result<Option<String>> {
    let kr = keyring().await?;
    let items = tokio::time::timeout(OP_TIMEOUT, kr.search_items(&slot_attrs(slot)))
        .await
        .context("searching the keyring timed out")?
        .context("searching the keyring")?;
    let Some(item) = items.first() else {
        return Ok(None);
    };
    let secret = tokio::time::timeout(OP_TIMEOUT, item.secret())
        .await
        .context("reading the secret timed out")?
        .context("reading the secret")?;
    // A stored-but-empty value is treated as unset (same as `load_key`).
    Ok(String::from_utf8(secret.as_bytes().to_vec())
        .ok()
        .filter(|s| !s.is_empty()))
}

/// The slots that currently have a stored key, sorted and deduped. **Values are
/// never returned** — only which providers are populated, so the control-center
/// can show "key set / not set" without ever handling the secret.
///
/// # Errors
/// A keyring that can't be opened, unlocked, or searched.
pub async fn list() -> anyhow::Result<Vec<String>> {
    let kr = keyring().await?;
    let items = tokio::time::timeout(OP_TIMEOUT, kr.search_items(&base_attrs()))
        .await
        .context("searching the keyring timed out")?
        .context("searching the keyring")?;
    let mut slots = BTreeSet::new();
    for item in items {
        // An item whose attributes we can't read is skipped rather than failing
        // the whole list — one broken item shouldn't hide the rest.
        if let Ok(attrs) = item.attributes().await
            && let Some(slot) = attrs.get("slot")
        {
            slots.insert(slot.clone());
        }
    }
    Ok(slots.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_matches_load_key_override() {
        // The contract with hytte_ai_providers::load_key: slot "openrouter" must
        // map to the OPENROUTER_API_KEY override it reads first.
        assert_eq!(env_var_for("openrouter"), "OPENROUTER_API_KEY");
        // `-` normalizes to `_` so the result stays a valid env-var name.
        assert_eq!(env_var_for("my-provider"), "MY_PROVIDER_API_KEY");
    }

    #[test]
    fn valid_slots_are_env_safe() {
        assert!(is_valid_slot("openrouter"));
        assert!(is_valid_slot("some-provider_2"));
        // Empty, leading digit, leading dash, uppercase, or a `=` (would corrupt
        // both the attribute and a `--setenv`) are all rejected.
        assert!(!is_valid_slot(""));
        assert!(!is_valid_slot("9live"));
        assert!(!is_valid_slot("-x"));
        assert!(!is_valid_slot("OpenRouter"));
        assert!(!is_valid_slot("bad=slot"));
        assert!(!is_valid_slot(&"x".repeat(65)));
    }
}
