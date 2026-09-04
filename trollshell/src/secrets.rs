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

/// Open the default keyring collection **without unlocking it**.
///
/// `Keyring::new()` only resolves the Secret Service and its default collection
/// (`oo7-0.5.0/src/keyring.rs`'s `new_inner`: `Service::new` →
/// `default_collection`); it raises no prompt and touches no secret. Every path
/// that needs a *value* goes through [`keyring`] instead — [`probe`] is the one
/// caller that must not, and the reason is in its own docs.
async fn keyring_no_prompt() -> anyhow::Result<Keyring> {
    tokio::time::timeout(OP_TIMEOUT, Keyring::new())
        .await
        .context("opening the secret service timed out")?
        .context("opening the secret service")
}

/// Open + unlock the default keyring collection. `unlock` is a no-op when the
/// collection is already unlocked (gnome-keyring auto-unlocks at login via PAM,
/// the normal case); it can prompt when locked, which [`OP_TIMEOUT`] bounds.
///
/// **Only for paths a human is waiting on** — the control-center's AI Keys tab
/// and a plugin launch. A prompt raised here is one somebody just asked for, and
/// [`OP_TIMEOUT`] abandoning it is bounded by that same human's attention. Do
/// not reach for this from anything periodic; see [`probe`].
async fn keyring() -> anyhow::Result<Keyring> {
    let kr = keyring_no_prompt().await?;
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

/// What one slot looks like right now, with the two failures [`get`] collapses
/// held apart (#866).
///
/// [`get`] answers the launch-time question — "is there a key to inject?" — and
/// fails closed, so "the ring is locked" and "nobody ever stored one" are both
/// `None` there. A *watcher* needs the distinction: the launcher polls the slots
/// a plugin launched without, and the log line for a session that started before
/// gnome-keyring was unlocked ("waiting for the keyring") is a different piece of
/// information from the one for a slot nobody has filled in yet ("no key
/// stored"). Both keep waiting — an external tool can add a key at any time —
/// which is why this only widens the *reporting*, never the policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretProbe {
    /// A non-empty key is stored and readable — injectable right now.
    Available,
    /// The keyring answered, and has no key for this slot.
    Absent,
    /// The keyring could not be read at all: locked and awaiting an unlock
    /// prompt, timed out, or no Secret Service on the bus. Says nothing about
    /// whether a key exists.
    Locked,
}

/// Probe `slot` without injecting anything, and — critically — **without ever
/// raising an unlock prompt**.
///
/// Never returns a value and never logs one; the caller learns only which of the
/// three states the slot is in.
///
/// # Why this cannot go through [`get`]
///
/// [`get`] → [`read`] → [`keyring`] calls `unlock()`, which on a locked
/// collection walks oo7's `Service::unlock` → `Prompt.Prompt("")` and **raises
/// the interactive gcr password dialog**. [`OP_TIMEOUT`] then drops that future
/// without calling `dismiss()`, abandoning the prompt. The launcher's watcher
/// polls this every 30s precisely *because* the ring is locked, so routing it
/// through `get` would put an abandoned password dialog on the user's screen
/// twice a minute for the life of the session — turning the feature into the
/// worst bug in this PR.
///
/// So the probe opens the collection with [`keyring_no_prompt`] and asks three
/// prompt-free questions instead: `SearchItems` (a collection method that works
/// on a locked collection and does not unlock — `oo7-0.5.0`'s
/// `dbus::Collection::search_items`), the item's `Locked` **property**, and only
/// then `GetSecret`, which is reached only once `Locked` is false.
///
/// One deliberate imprecision: a Secret Service implementation that hides items
/// under a locked collection would make this answer [`SecretProbe::Absent`]
/// where gnome-keyring answers [`SecretProbe::Locked`]. Both keep waiting, so
/// only the log line differs — and no prompt is raised either way, which is the
/// property that matters.
pub async fn probe(slot: &str) -> SecretProbe {
    match probe_inner(slot).await {
        Ok(state) => state,
        Err(err) => {
            tracing::debug!(slot = %slot, %err, "keyring unreadable while probing the slot");
            SecretProbe::Locked
        }
    }
}

/// Fallible core of [`probe`]. Every step is time-bounded, and none of them can
/// prompt.
async fn probe_inner(slot: &str) -> anyhow::Result<SecretProbe> {
    let kr = keyring_no_prompt().await?;
    let items = tokio::time::timeout(OP_TIMEOUT, kr.search_items(&slot_attrs(slot)))
        .await
        .context("searching the keyring timed out")?
        .context("searching the keyring")?;
    let Some(item) = items.first() else {
        return Ok(SecretProbe::Absent);
    };
    // A plain `org.freedesktop.Secret.Item.Locked` property read — this is the
    // question, asked directly, rather than inferred from a failure.
    let locked = tokio::time::timeout(OP_TIMEOUT, item.is_locked())
        .await
        .context("reading the item's lock state timed out")?
        .context("reading the item's lock state")?;
    if locked {
        return Ok(SecretProbe::Locked);
    }
    let secret = tokio::time::timeout(OP_TIMEOUT, item.secret())
        .await
        .context("reading the secret timed out")?
        .context("reading the secret")?;
    // Same "unset" definition as `read`: a stored-but-empty or non-UTF-8 value
    // is nothing to inject.
    Ok(classify_secret_bytes(secret.as_bytes()))
}

/// Whether some stored bytes are a key worth injecting. Pure, and the one part
/// of [`probe`] testable without a Secret Service — see the module's test note.
fn classify_secret_bytes(bytes: &[u8]) -> SecretProbe {
    match std::str::from_utf8(bytes) {
        Ok(s) if !s.is_empty() => SecretProbe::Available,
        _ => SecretProbe::Absent,
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

    /// The one hermetically testable piece of [`probe`]: what counts as a key
    /// worth injecting, held identical to [`read`]'s definition of "unset".
    ///
    /// **Measured negative, stated rather than faked:** the property that
    /// actually matters about `probe` — that it never raises an unlock prompt —
    /// is not assertable here. It is a claim about which D-Bus methods are
    /// called against a *live* Secret Service, and this workspace's
    /// `system-tests` bucket spawns a bare `dbus-daemon` with no
    /// `org.freedesktop.secrets` implementation on it, so there is nothing for a
    /// hermetic test to observe. The guarantee rests on [`keyring_no_prompt`]
    /// being the only opener `probe_inner` uses (`unlock()` is what prompts, and
    /// it is not on that path) plus the on-glass check in the PR: watch
    /// `journalctl --user -u trollshell` through a locked-ring session and
    /// confirm no gcr dialog appears.
    #[test]
    fn a_stored_secret_counts_as_available_only_when_it_is_a_non_empty_string() {
        assert_eq!(classify_secret_bytes(b"sk-live"), SecretProbe::Available);
        // Same as `read`'s filter: stored-but-empty is nothing to inject…
        assert_eq!(classify_secret_bytes(b""), SecretProbe::Absent);
        // …and neither is a value that isn't even text.
        assert_eq!(classify_secret_bytes(&[0xff, 0xfe]), SecretProbe::Absent);
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
