//! The **`OpenRouter` credits wallet** (#1347) — the second card on the bridge's
//! drawer page, beside the Claude one.
//!
//! # What this reads
//!
//! Two documented endpoints, both plain `GET`s answering one JSON object with a
//! single `data` member, both authorised by `Authorization: Bearer <key>`:
//!
//! ```text
//! GET https://openrouter.ai/api/v1/key      → this key's own leash
//! GET https://openrouter.ai/api/v1/credits  → the account's wallet
//! ```
//!
//! - <https://openrouter.ai/docs/api-reference/limits> — `/key` answers
//!   `{"data":{ "label", "limit", "limit_reset", "limit_remaining",
//!   "include_byok_in_limit", "usage", "usage_daily", "usage_weekly",
//!   "usage_monthly", "byok_usage", …, "is_free_tier",
//!   "free_model_daily_requests" }}`. Every key can read it.
//! - <https://openrouter.ai/docs/api-reference/get-credits> — `/credits`
//!   answers `{"data":{"total_credits":100.5,"total_usage":25.75}}`, and its
//!   doc says in as many words that it needs a **management key**: a standard
//!   inference key is answered `403 Forbidden`.
//!
//! That 403 is why this polls **both** and not just the one #1347's body named.
//! An ordinary key would otherwise render a permanently-failing card; with
//! `/key` beside it the card still has a number to show (the key's own
//! `limit_remaining`), and says once, quietly, why the account balance is
//! missing.
//!
//! # The numbers are **US dollars**, as floats
//!
//! Every money field on both endpoints is a JSON `number` denominated in USD —
//! `total_credits` and `total_usage` are documented as `double` — and the
//! remaining credit is their **difference**, not a field of its own. Nothing
//! here is ever in cents, which is the single most likely way to get this wrong
//! by a factor of 100; [`usd`] is the only renderer, the fixtures under
//! `tests/fixtures/openrouter_*.json` are the recorded bytes, and
//! `the_remaining_credit_is_dollars_not_cents` is the pin (#1026's rule).
//!
//! # The key: injected, or the wallet does not exist
//!
//! [`KEY_ENV`] is read from the process environment and **nowhere else**. That
//! variable is exactly what `programs.trollshell.plugins.claude-bridge.secrets
//! = [ "openrouter" ]` injects from the login keyring at spawn (#392,
//! `trollshell/src/plugin_launcher.rs`'s `resolve_secret_env`). There is
//! deliberately no `~/.config/trollshell/openrouter.key` fallback — not even
//! through [`hytte_ai_providers::load_key`], whose own on-disk fallback
//! #1330/PR #1351 already retired ("no fallbacks", Annika on #866). Calling
//! that loader here would have quietly re-introduced the arm that removal
//! took out, in a daemon that holds a second credential already.
//!
//! With no key the wallet is **off end to end**: [`Poll::from_env`] answers
//! `None`, nothing is ever published to [`latest`], and
//! [`crate::plugin`] draws one card instead of two. There is no request, no
//! retry and no repeated log line — one `debug!` at startup, from `main`.
//!
//! The key never leaves this module: it is held in a [`Poll`] that derives no
//! `Debug`, it reaches exactly one `Authorization` header, and every borrowed
//! error string is run through [`crate::usage::scrub`] before it can be shown,
//! the same belt-and-braces [`crate::usage`] applies to the OAuth token.
//! `/key`'s response carries a `label` field that `OpenRouter`'s own docs
//! illustrate with a `sk-or-v1-…`-shaped string; it is deliberately **not
//! modelled** — a type that cannot hold it cannot leak it onto a card.
//!
//! # Cadence, and what "stale" means here
//!
//! [`POLL_EVERY`] is five minutes, the same cadence and for the same reason as
//! [`crate::usage::POLL_EVERY`]: a credit balance moves on the scale of a
//! conversation, not a frame. There is no 429 backoff schedule (#1283's, on the
//! Claude poll) — two metadata `GET`s every five minutes are nowhere near any
//! documented rate limit, and a schedule nothing exercises is a schedule
//! nothing tests.
//!
//! A failed poll does not blank the card: [`Report::last_ok`] carries the last
//! numbers forward and the card says **`stale`** over them, which is #1347's
//! own wording. [`Report::is_stale`] is true for either of the two reasons a
//! reader cares about — the latest attempt failed and these numbers are
//! carried forward, or the poller itself has stopped publishing and they have
//! aged past [`STALE_AFTER`].

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use hytte_plugin::proto::Node;
use serde::Deserialize;

use crate::usage::{lenient, now_unix, scrub, truncate};

// ── Constants ────────────────────────────────────────────────────────────────

/// The API root both paths hang off. A parameter of [`fetch`] rather than a
/// constant it reaches for, so a test can point it at a `std::net::TcpListener`
/// serving canned bodies — and, exactly as [`crate::usage::DEFAULT_BASE_URL`],
/// there is deliberately **no** environment override: an env knob on an
/// endpoint that spends a bearer token is a footgun nobody asked for.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// This key's own limit and spend — readable by every key.
const KEY_PATH: &str = "/key";

/// The account wallet — **management keys only** (see the module docs).
const CREDITS_PATH: &str = "/credits";

/// The `User-Agent` both requests identify themselves with. Says which program
/// is asking; carries no account identity of its own.
const USER_AGENT: &str = concat!("hytte-claude-bridge/", env!("CARGO_PKG_VERSION"));

/// Whole-round-trip budget for one request. Bounds nothing a client is waiting
/// on — the card renders the previous report while a poll is in flight.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Connect budget inside [`TIMEOUT`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the wallet is re-read. See the module docs.
pub const POLL_EVERY: Duration = Duration::from_mins(5);

/// How old the numbers behind a [`Report`] may be before the card stops calling
/// them current — three poll periods, [`crate::usage::STALE_AFTER`]'s rule and
/// its reasoning: one slow round-trip never trips it, a poller that has stopped
/// publishing goes visibly stale within one window.
pub const STALE_AFTER: Duration = Duration::from_secs(POLL_EVERY.as_secs() * 3);

/// The environment variable carrying the key — what
/// `plugins.claude-bridge.secrets = [ "openrouter" ]` injects (#392). Absent or
/// blank ⇒ no wallet at all (module docs).
pub const KEY_ENV: &str = "OPENROUTER_API_KEY";

/// `[openrouter] enabled` — set to one of [`OFF_VALUES`] to keep the wallet off
/// even when a key **is** injected. Anything else (including unset) is "on",
/// because the key is the real gate and a second way to say "no" that defaults
/// to "no" would surprise whoever just declared the secret.
pub const ENABLE_ENV: &str = "CLAUDE_BRIDGE_OPENROUTER";

/// `[openrouter] low_credit` — the remaining-credit threshold, in **dollars**,
/// at or below which the card's headline number is tinted `warning`.
pub const LOW_CREDIT_ENV: &str = "CLAUDE_BRIDGE_OPENROUTER_LOW_CREDIT";

/// An operator-set title for this card, [`crate::plugin`]'s `CLAUDE_BRIDGE_LABEL`
/// for the other one.
pub const LABEL_ENV: &str = "CLAUDE_BRIDGE_OPENROUTER_LABEL";

/// Values of [`ENABLE_ENV`] that read as off. The same three spellings
/// [`crate::envguard`] already treats as "not set" for its own boolean flags,
/// so an operator has one rule to remember for this daemon.
const OFF_VALUES: [&str; 3] = ["0", "false", "off"];

/// The default [`LOW_CREDIT_ENV`], in dollars. A round number rather than a
/// measured one: it is the point at which a person would want to notice, and
/// the knob exists precisely because that point is personal.
pub const DEFAULT_LOW_CREDIT: f64 = 5.0;

/// The card's title absent [`LABEL_ENV`].
pub const DEFAULT_TITLE: &str = "OpenRouter credits";

// ── The wire ─────────────────────────────────────────────────────────────────

/// Either endpoint's envelope: one `data` member and nothing this crate reads
/// beside it.
///
/// The explicit `bound` is required rather than decorative: a field with a
/// `deserialize_with` suppresses serde's automatic `T: Deserialize<'de>` bound,
/// and `#[serde(default)]` needs `T: Default` besides.
#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Default + Deserialize<'de>"))]
struct Envelope<T> {
    #[serde(default, deserialize_with = "lenient")]
    data: T,
}

/// `/credits` — the account's wallet, in USD.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
pub struct Credits {
    /// Total credits purchased (doc: `number`, format `double`).
    #[serde(default, deserialize_with = "lenient")]
    pub total_credits: f64,
    /// Total credits used.
    #[serde(default, deserialize_with = "lenient")]
    pub total_usage: f64,
}

impl Credits {
    /// What is left to spend: **the difference**, in dollars. Never clamped at
    /// zero — an account that has overspent its prepaid balance should read as
    /// having done so, and [`usd`] spells a negative as `-$1.23`.
    #[must_use]
    pub fn remaining(&self) -> f64 {
        self.total_credits - self.total_usage
    }

    /// The share of the purchased credits already spent, `0.0..=1.0`, for the
    /// card's bar. A zero (or non-finite) ceiling has no meaningful fraction
    /// and reads as empty rather than as full.
    #[must_use]
    pub fn spent_fraction(&self) -> f64 {
        if !self.total_credits.is_finite() || self.total_credits <= 0.0 {
            return 0.0;
        }
        (self.total_usage / self.total_credits).clamp(0.0, 1.0)
    }
}

/// `/key` — this key's own leash.
///
/// Only the four fields the card can use. The documented response carries ten
/// more (`usage_daily`, the `byok_*` family, `free_model_daily_requests`, …)
/// and they are ignored rather than modelled; `label` is ignored **on purpose**
/// — see the module docs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
pub struct KeyInfo {
    /// Spend against this key, in dollars.
    #[serde(default, deserialize_with = "lenient")]
    pub usage: f64,
    /// The key's own ceiling, when one is set. `null` for an unlimited key,
    /// which is the common case.
    #[serde(default)]
    pub limit: Option<f64>,
    /// What the server says is left of that ceiling, when it says.
    #[serde(default)]
    pub limit_remaining: Option<f64>,
    /// Whether this key only reaches the free models.
    #[serde(default, deserialize_with = "lenient")]
    pub is_free_tier: bool,
}

impl KeyInfo {
    /// What is left on this key's own leash: the server's own
    /// [`limit_remaining`](Self::limit_remaining) when it sent one, else the
    /// subtraction, else `None` for a key with no ceiling at all.
    #[must_use]
    pub fn remaining(&self) -> Option<f64> {
        self.limit_remaining
            .or_else(|| self.limit.map(|limit| limit - self.usage))
    }
}

/// One poll's numbers: whichever of the two endpoints answered.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Balance {
    /// `/credits`, when this key was allowed to read it.
    pub credits: Option<Credits>,
    /// `/key`, when it answered.
    pub key: Option<KeyInfo>,
    /// `/credits` answered `403`: an inference key, not a management key. Not
    /// an error — a fact about the key, said once on the card so nobody reads
    /// the missing balance as a broken poll.
    pub needs_management_key: bool,
}

impl Balance {
    /// The one number the card headlines and the chip's hover repeats, in
    /// dollars: the account wallet when it is visible, else the key's own
    /// remaining leash, else `None` (an unlimited key on a non-management
    /// credential — there is genuinely no "remaining" to report, and the card
    /// shows the spend instead).
    #[must_use]
    pub fn remaining(&self) -> Option<f64> {
        self.credits
            .map(|c| c.remaining())
            .or_else(|| self.key.and_then(|k| k.remaining()))
    }
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// Why a poll produced no numbers. Each arm has one user-facing
/// [`sentence`](WalletError::sentence) — the card's footer shows that string
/// and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalletError {
    /// 401 (or a 403 from `/key`, which every key may read — so a refusal
    /// there is about the credential, not about its tier).
    Unauthorized,
    /// Any other non-2xx.
    Http(u16),
    /// The request never completed (DNS, TLS, connect, read).
    Io(String),
    /// A 2xx body that did not parse.
    Parse(String),
}

impl WalletError {
    /// The one line a human reads. Never contains the key — see the module docs
    /// and `an_unauthorized_sentence_carries_no_key_bytes`.
    #[must_use]
    pub fn sentence(&self) -> String {
        match self {
            Self::Unauthorized => {
                "OpenRouter refused the key — re-add it to the keyring slot".to_owned()
            }
            Self::Http(status) => format!("OpenRouter answered HTTP {status}"),
            Self::Io(what) => format!("OpenRouter unavailable — {what}"),
            Self::Parse(what) => {
                format!("OpenRouter unavailable — the response could not be read ({what})")
            }
        }
    }
}

// ── The fetch ────────────────────────────────────────────────────────────────

/// One request: `GET {base_url}{path}` with the bearer, parsed as
/// `Envelope<T>`.
fn get<T>(agent: &ureq::Agent, base_url: &str, path: &str, key: &str) -> Result<T, WalletError>
where
    T: Default + serde::de::DeserializeOwned,
{
    let url = format!("{}{path}", base_url.trim_end_matches('/'));
    let mut resp = agent
        .get(&url)
        .header("Authorization", format!("Bearer {key}"))
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| WalletError::Io(truncate(&scrub(&e.to_string(), key))))?;

    let status = resp.status().as_u16();
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| WalletError::Io(truncate(&scrub(&e.to_string(), key))))?;

    match status {
        200..=299 => serde_json::from_str::<Envelope<T>>(&body)
            .map(|envelope| envelope.data)
            .map_err(|e| WalletError::Parse(truncate(&scrub(&e.to_string(), key)))),
        401 => Err(WalletError::Unauthorized),
        other => Err(WalletError::Http(other)),
    }
}

/// The agent both requests share. Identical hardening to
/// [`crate::usage`]'s, for the identical reason: `http_status_as_error(false)`
/// so the status decides the arm rather than collapsing into a transport
/// error, and `max_redirects(0)` so a redirect can never open a second
/// connection the bearer might ride.
///
/// Deliberately **not** [`hytte_ai_providers::http::agent`], which this crate
/// already depends on: that builder sets the two timeouts and nothing else, by
/// its own module doc ("default status-as-error behaviour"), so neither knob
/// above would survive it — and both of them are here to defend a credential.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into()
}

/// One poll: `/key` first, then `/credits`.
///
/// The order is load-bearing. `/key` is the endpoint **every** key may read, so
/// its refusal is the credential's own verdict and there is no point spending a
/// second request behind it. `/credits` is management-key-only, so its `403` is
/// a fact about the key's tier rather than a failure — recorded as
/// [`Balance::needs_management_key`] and shown once on the card.
///
/// The poll fails only when *neither* endpoint produced numbers, and then it
/// reports `/key`'s error, because that is the one every key should have been
/// able to read.
///
/// Blocking — call it from `spawn_blocking`, the way [`Poll::run`] does.
///
/// # Errors
///
/// Every arm of [`WalletError`]; each carries a one-line
/// [`sentence`](WalletError::sentence) and **never** the key.
pub fn fetch(base_url: &str, key: &str) -> Result<Balance, WalletError> {
    let agent = agent();
    let (info, key_error) = match get::<KeyInfo>(&agent, base_url, KEY_PATH, key) {
        Ok(info) => (Some(info), None),
        // A 403 on `/key` is not the tier fact `/credits`' is: this endpoint is
        // documented as readable by every key, so a refusal here is about the
        // credential.
        Err(WalletError::Http(403)) => (None, Some(WalletError::Unauthorized)),
        Err(e @ WalletError::Unauthorized) => return Err(e),
        Err(e) => (None, Some(e)),
    };

    let (credits, needs_management_key, credits_error) =
        match get::<Credits>(&agent, base_url, CREDITS_PATH, key) {
            Ok(credits) => (Some(credits), false, None),
            Err(WalletError::Http(403)) => (None, true, None),
            Err(e) => (None, false, Some(e)),
        };

    if info.is_none() && credits.is_none() {
        // `key_error` is always `Some` here — `info` is `None` only on an arm
        // that set it — so the fallback is unreachable; it names the
        // credential rather than inventing a status code nothing observed.
        return Err(key_error
            .or(credits_error)
            .unwrap_or(WalletError::Unauthorized));
    }
    Ok(Balance {
        credits,
        key: info,
        needs_management_key,
    })
}

// ── The board the card reads ─────────────────────────────────────────────────

/// One poll's outcome.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Numbers.
    Ok(Balance),
    /// No numbers, and why.
    Failed(WalletError),
}

/// The last poll: when it ran, how it went, and the last numbers that actually
/// came back — [`crate::usage::Report`]'s shape, one wallet over.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Unix seconds at which the attempt completed.
    pub at: i64,
    /// What this attempt produced.
    pub outcome: Outcome,
    /// The most recent successful poll's `(at, Balance)`, carried forward
    /// across a failure so a transient error does not blank a card that has
    /// good numbers to show. `None` only before this process has seen a
    /// success.
    pub last_ok: Option<(i64, Balance)>,
}

impl Report {
    /// The numbers to render, whether they came from this attempt or a previous
    /// one.
    #[must_use]
    pub fn balance(&self) -> Option<&Balance> {
        match &self.outcome {
            Outcome::Ok(balance) => Some(balance),
            Outcome::Failed(_) => self.last_ok.as_ref().map(|(_, balance)| balance),
        }
    }

    /// The failure sentence, if the last attempt failed — regardless of whether
    /// [`balance`](Self::balance) still has carried-forward numbers beside it.
    #[must_use]
    pub fn error(&self) -> Option<String> {
        match &self.outcome {
            Outcome::Ok(_) => None,
            Outcome::Failed(e) => Some(e.sentence()),
        }
    }

    /// When the numbers [`balance`](Self::balance) would return were actually
    /// fetched.
    #[must_use]
    pub fn numbers_at(&self) -> Option<i64> {
        match &self.outcome {
            Outcome::Ok(_) => Some(self.at),
            Outcome::Failed(_) => self.last_ok.as_ref().map(|(at, _)| *at),
        }
    }

    /// Whether the numbers on show are not this instant's truth — #1347's
    /// `stale`. Two reasons, both of which a reader cares about identically:
    /// the latest attempt **failed** and these numbers are carried forward, or
    /// the poller has stopped publishing and they have aged past
    /// [`STALE_AFTER`].
    ///
    /// A report with no numbers at all is never stale — there is nothing to be
    /// stale — and every caller gates on [`balance`](Self::balance) first.
    #[must_use]
    pub fn is_stale(&self, now: i64) -> bool {
        let Some(basis) = self.numbers_at() else {
            return false;
        };
        if matches!(self.outcome, Outcome::Failed(_)) {
            return true;
        }
        let ceiling = i64::try_from(STALE_AFTER.as_secs()).unwrap_or(i64::MAX);
        now.saturating_sub(basis) > ceiling
    }
}

/// The last report, or `None` before the first poll completes — and forever,
/// when no key was injected (module docs).
static BOARD: Mutex<Option<Report>> = Mutex::new(None);

/// Bumped on every [`publish`], so the chip's 5 s tick can skip the lock and
/// the clone when nothing has moved.
static VERSION: AtomicU64 = AtomicU64::new(0);

/// Every test — here **and** in [`crate::plugin`] — that publishes onto the
/// board above takes this for its whole body. The board is process-global and
/// cargo runs a binary's tests as parallel threads in one process, so one
/// test's `publish` could otherwise land between another's `publish` and its
/// own [`latest`] read. Module-level rather than inside `mod tests` precisely
/// because the two test modules must share **one** lock to serialise against
/// each other at all. ([`crate::usage`]'s `BOARD_TESTS`, whose board has only
/// one test module.)
#[cfg(test)]
pub(crate) static BOARD_TESTS: Mutex<()> = Mutex::new(());

/// Publish one poll's outcome. A poisoned lock is swallowed rather than
/// propagated, the rule [`crate::status`] states: a readout must never be able
/// to take the bridge down.
pub fn publish(report: Report) {
    *BOARD.lock().unwrap_or_else(PoisonError::into_inner) = Some(report);
    VERSION.fetch_add(1, Ordering::Relaxed);
}

/// The last report, cloned.
#[must_use]
pub fn latest() -> Option<Report> {
    BOARD.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// How many reports have been published this process.
#[must_use]
pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

/// One poll's book-keeping: given the last-known-good numbers the previous
/// iteration handed forward and what this fetch produced, decide the [`Report`]
/// to publish and the state to carry into the next iteration.
///
/// Pulled out of [`poll_loop`] so the carry-forward is a plain function a test
/// can drive without an event loop or a clock. Falsify it by publishing
/// `last_ok: None` unconditionally: `a_failed_poll_carries_the_last_numbers_forward`
/// goes red.
fn advance(
    prev_last_ok: Option<(i64, Balance)>,
    at: i64,
    result: Result<Balance, WalletError>,
) -> (Report, Option<(i64, Balance)>) {
    let (outcome, last_ok) = match result {
        Ok(balance) => (Outcome::Ok(balance), Some((at, balance))),
        Err(e) => (Outcome::Failed(e), prev_last_ok),
    };
    (
        Report {
            at,
            outcome,
            last_ok,
        },
        last_ok,
    )
}

/// [`Poll::run`]'s actual loop, with the fetcher injected so a test can drive
/// it against a scripted sequence under a virtual clock instead of a real
/// network and a real `sleep` — [`crate::usage`]'s `poll_loop`, and for the
/// reason stated there: every step routes through something a test can observe
/// (the fetcher, [`advance`], [`publish`], `tokio::time::sleep`), so a mutation
/// that drops the carried `last_ok` breaks the loop itself and not only the
/// pure helper.
async fn poll_loop<F, Fut>(mut fetch_once: F) -> !
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Balance, WalletError>>,
{
    let mut last_ok: Option<(i64, Balance)> = None;
    loop {
        let result = fetch_once().await;
        let (report, next_last_ok) = advance(last_ok, now_unix(), result);
        last_ok = next_last_ok;
        if let Outcome::Failed(ref e) = report.outcome {
            // The sentence, never the cause verbatim and never the key.
            tracing::debug!(reason = %e.sentence(), "openrouter poll produced no numbers");
        }
        publish(report);
        tokio::time::sleep(POLL_EVERY).await;
    }
}

// ── Settings ─────────────────────────────────────────────────────────────────

/// Everything the poller needs, resolved once at startup — and the **only**
/// thing that holds the key.
///
/// Derives no `Debug`, deliberately: a type that cannot be formatted cannot be
/// formatted into a log line — [`crate::usage`]'s rule for its own
/// `Credentials`/`OauthCreds`.
pub struct Poll {
    base_url: String,
    key: String,
}

impl Poll {
    /// Resolve the wallet from the process environment: [`Poll::from`] over
    /// [`std::env::var`].
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from(&|name| std::env::var(name).ok())
    }

    /// [`Poll::from_env`] with the environment injected, so the whole gate is
    /// testable without mutating the process environment (`unsafe` under
    /// edition 2024, which this workspace forbids).
    ///
    /// `None` — i.e. **no wallet, no poll, no card** — when the key is absent or
    /// blank, or when [`ENABLE_ENV`] spells one of [`OFF_VALUES`].
    #[must_use]
    pub fn from(lookup: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        if !enabled(lookup) {
            return None;
        }
        let key = lookup(KEY_ENV)?;
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        Some(Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            key: key.to_owned(),
        })
    }

    /// Poll forever: one fetch immediately, then one every [`POLL_EVERY`].
    ///
    /// Never returns. Spawned by `main` on the HTTP runtime, next to
    /// [`crate::usage::poll_forever`] and for the same reason: the numbers must
    /// keep arriving while the shell is down and the chip is in its dial
    /// backoff. A thin wrapper over [`poll_loop`] — the only thing it adds is
    /// the real fetcher, `spawn_blocking`ing [`fetch`] with a join failure
    /// mapped to the same [`WalletError::Io`] shape a transport error would be.
    pub async fn run(self) -> ! {
        poll_loop(move || {
            let (base, key) = (self.base_url.clone(), self.key.clone());
            async move {
                match tokio::task::spawn_blocking(move || fetch(&base, &key)).await {
                    Ok(result) => result,
                    Err(e) => Err(WalletError::Io(truncate(&format!(
                        "the OpenRouter poll task did not finish: {e}"
                    )))),
                }
            }
        })
        .await
    }
}

/// Whether [`ENABLE_ENV`] leaves the wallet on. See that constant.
fn enabled(lookup: &dyn Fn(&str) -> Option<String>) -> bool {
    lookup(ENABLE_ENV)
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .is_none_or(|v| !OFF_VALUES.contains(&v.as_str()))
}

/// The two render-time knobs, resolved once beside
/// [`crate::plugin`]'s own title.
#[derive(Clone, Debug, PartialEq)]
pub struct CardSettings {
    /// [`LABEL_ENV`], trimmed, else [`DEFAULT_TITLE`].
    pub title: String,
    /// [`LOW_CREDIT_ENV`] in dollars, else [`DEFAULT_LOW_CREDIT`].
    pub low_credit: f64,
}

impl Default for CardSettings {
    fn default() -> Self {
        Self {
            title: DEFAULT_TITLE.to_owned(),
            low_credit: DEFAULT_LOW_CREDIT,
        }
    }
}

impl CardSettings {
    /// Resolve both knobs from an injected environment — pure, for the same
    /// testability reason [`hytte_plugin::effective_mount_from`] takes a
    /// `lookup`.
    ///
    /// An unparseable or non-finite [`LOW_CREDIT_ENV`] falls back to
    /// [`DEFAULT_LOW_CREDIT`] rather than refusing to start: a typo in a
    /// threshold must not cost the card. A **negative** one is kept — "warn me
    /// only once I have overspent" is a coherent thing to ask for.
    #[must_use]
    pub fn resolve(lookup: &dyn Fn(&str) -> Option<String>) -> Self {
        let title = lookup(LABEL_ENV)
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| DEFAULT_TITLE.to_owned(), str::to_owned);
        let low_credit = lookup(LOW_CREDIT_ENV)
            .as_deref()
            .map(str::trim)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(DEFAULT_LOW_CREDIT);
        Self { title, low_credit }
    }

    /// Whether `remaining` dollars is at or below the threshold.
    #[must_use]
    pub fn is_low(&self, remaining: f64) -> bool {
        remaining.is_finite() && remaining <= self.low_credit
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

/// `$12.34` — the **only** money renderer, and the one place the unit is
/// spelled. Two decimals because that is how a dollar balance reads; a negative
/// (an overspent prepaid account) reads `-$1.23` rather than `$-1.23`;
/// non-finite reads as an em dash, never as `NaN`.
#[must_use]
pub fn usd(amount: f64) -> String {
    if !amount.is_finite() {
        return "—".to_owned();
    }
    if amount < 0.0 {
        return format!("-${:.2}", -amount);
    }
    format!("${amount:.2}")
}

/// The ids this wallet's card carries on one surface. Indexed by surface rather
/// than constant because the drawer page and the sidebar draw the same card
/// twice over and a shared id would collapse them onto one renderer instance
/// (`Node::Preem`'s id contract, #918, applied to boxes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ids {
    /// The card's own root — the node carrying the card class.
    pub root: &'static str,
    /// The header-plus-rows list nested inside it.
    pub list: &'static str,
}

/// This wallet's ids on the drawer page.
pub const PANEL_IDS: Ids = Ids {
    root: "claude-bridge-panel-openrouter",
    list: "claude-bridge-panel-openrouter-list",
};

/// This wallet's ids on a sidebar mount.
pub const SIDEBAR_IDS: Ids = Ids {
    root: "claude-bridge-card-openrouter",
    list: "claude-bridge-card-openrouter-list",
};

/// The header's freshness phrase: `updated 2 min ago`, or the same prefixed
/// `stale · ` when [`Report::is_stale`] says the numbers are not this instant's
/// truth (#1347's "`stale` with last-seen numbers").
///
/// With no numbers at all there is nothing to date and the failure footer is
/// the whole story, so the phrase says so rather than dating the *attempt* —
/// "updated just now" over a card with no numbers on it is the one wording
/// that could be read as a working wallet.
#[must_use]
pub fn freshness(report: &Report, now: i64) -> String {
    let Some(at) = report.numbers_at() else {
        return "no numbers yet".to_owned();
    };
    let phrase = format!("updated {}", crate::usage::humanise_since(now, at));
    if report.is_stale(now) {
        format!("stale · {phrase}")
    } else {
        phrase
    }
}

/// The wallet's card: a header, the remaining-credit headline, the spend bar
/// and its captions, and the failure footer when the last poll failed.
///
/// Called **only** with a [`Report`] in hand — there is no "fetching…" state to
/// draw, because a wallet with no key never publishes one and
/// [`crate::plugin`] then draws no card at all (module docs).
#[must_use]
pub fn card(ids: Ids, settings: &CardSettings, report: &Report, now: i64) -> Node {
    let mut children = vec![crate::plugin::wallet_header(
        &settings.title,
        &freshness(report, now),
    )];
    if let Some(balance) = report.balance() {
        children.extend(balance_rows(ids, settings, balance));
    }
    if let Some(sentence) = report.error() {
        children.push(crate::plugin::label(&sentence, &["warning"]));
    }
    crate::plugin::wallet_card(ids.root, ids.list, children)
}

/// The numbers half of [`card`].
fn balance_rows(ids: Ids, settings: &CardSettings, balance: &Balance) -> Vec<Node> {
    let mut rows = Vec::new();
    let mut captions: Vec<String> = Vec::new();

    if let Some(remaining) = balance.remaining() {
        let tint = if settings.is_low(remaining) {
            "warning"
        } else {
            "accent"
        };
        rows.push(crate::plugin::titled_row(
            crate::plugin::label("Remaining", &["heading"]),
            crate::plugin::label(&usd(remaining), &["numeric", tint]),
        ));
    } else {
        // An unlimited key on a non-management credential: there is no
        // "remaining" anybody could compute, so headline the spend instead of
        // inventing a ceiling.
        let spent = balance.key.map_or(0.0, |k| k.usage);
        rows.push(crate::plugin::titled_row(
            crate::plugin::label("Spent", &["heading"]),
            crate::plugin::label(&usd(spent), &["numeric", "accent"]),
        ));
    }

    if let Some(credits) = balance.credits {
        rows.push(crate::plugin::bar(
            format!("{}-spend", ids.root),
            credits.spent_fraction(),
            "accent",
        ));
        captions.push(format!(
            "{} of {} used",
            usd(credits.total_usage),
            usd(credits.total_credits)
        ));
    }
    if let Some(key) = balance.key {
        if let (Some(remaining), Some(limit)) = (key.remaining(), key.limit) {
            captions.push(format!("key limit: {} of {}", usd(remaining), usd(limit)));
        } else if balance.credits.is_some() {
            // The account wallet is already the headline; the key's own spend
            // is the detail that says how much of it is this key's.
            captions.push(format!("{} on this key", usd(key.usage)));
        }
        if key.is_free_tier {
            captions.push("free tier".to_owned());
        }
    }
    if balance.needs_management_key {
        // Said once, quietly: the account balance is missing because of the
        // key's tier, not because the poll is broken.
        captions.push("account balance needs a management key".to_owned());
    }
    if !captions.is_empty() {
        rows.push(crate::plugin::label(&captions.join(" · "), &["dim-label"]));
    }
    rows
}

/// The chip's hover line for this wallet (#1347: "the bar chip stays the Claude
/// one, with the `OpenRouter` remaining credit as a second line in its hover").
///
/// `None` when there is no wallet at all, which is the ordinary case — a hover
/// must not grow a line about a thing that does not exist. With numbers it
/// names them; with none it names the failure, because a hover that went silent
/// on a broken poll would be the only place the breakage could have been seen
/// (the card is a click away).
#[must_use]
pub fn hover_line(report: Option<&Report>, now: i64) -> Option<String> {
    let report = report?;
    let head = match report.balance().and_then(Balance::remaining) {
        Some(remaining) => format!("OpenRouter · {} left", usd(remaining)),
        None => match report.balance().and_then(|b| b.key).map(|k| k.usage) {
            Some(spent) => format!("OpenRouter · {} spent", usd(spent)),
            None => format!("OpenRouter · {}", report.error()?),
        },
    };
    if report.is_stale(now) {
        // Same word the card uses, in the same position, so the two surfaces
        // can never disagree about whether the number is current.
        return Some(format!("{head} (stale)"));
    }
    Some(head)
}

#[cfg(test)]
mod tests {
    use super::BOARD_TESTS;
    use super::{
        Balance, CardSettings, Credits, DEFAULT_BASE_URL, DEFAULT_LOW_CREDIT, DEFAULT_TITLE,
        ENABLE_ENV, Envelope, Ids, KEY_ENV, KeyInfo, LABEL_ENV, LOW_CREDIT_ENV, Outcome, PANEL_IDS,
        POLL_EVERY, Poll, Report, SIDEBAR_IDS, STALE_AFTER, WalletError, advance, card, fetch,
        freshness, hover_line, poll_loop, usd,
    };
    use hytte_plugin::proto::Node;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc, Mutex, PoisonError};

    /// **The recorded `/credits` body**
    /// (`tests/fixtures/openrouter_credits.json`) — the example response
    /// printed verbatim in `OpenRouter`'s own API reference,
    /// <https://openrouter.ai/docs/api-reference/get-credits>, whose schema
    /// gives both fields as `number`, format `double`.
    const CREDITS_FIXTURE: &str = include_str!("../../tests/fixtures/openrouter_credits.json");

    /// **The recorded `/key` body** (`tests/fixtures/openrouter_key.json`),
    /// built field-for-field from the `Key` type published at
    /// <https://openrouter.ai/docs/api-reference/limits> — that page documents
    /// the response as a TypeScript type rather than as an example body, so the
    /// fixture carries **every** field it names, with a `label` that carries
    /// the `sk-or-` prefix those docs illustrate one with — and, deliberately,
    /// nothing else of a real key's shape (see [`FAKE_KEY`]).
    ///
    /// It therefore pins three things at once: the four fields [`KeyInfo`]
    /// reads, that the ten it does not are tolerated rather than fatal, and —
    /// `a_keys_label_never_reaches_a_card` — that the key-shaped `label` never
    /// reaches a rendered node.
    const KEY_FIXTURE: &str = include_str!("../../tests/fixtures/openrouter_key.json");

    /// The key the fetch tests send: long and distinctive enough that an
    /// accidental substring match is not why a test passes.
    ///
    /// It deliberately does **not** have the shape of a real one (`sk-or-v1-`
    /// plus 64 hex digits): GitHub's push protection recognises that shape as
    /// an `OpenRouter` API key and refuses the push — which is exactly the
    /// behaviour you want from it, and a pattern nothing in this repository
    /// should be teaching people to work around. The recognisable `sk-or-`
    /// prefix is all `a_keys_label_never_reaches_a_card` actually needs.
    const FAKE_KEY: &str = "sk-or-EXAMPLE-TESTONLY-NOT-A-REAL-KEY-0123456789";

    /// The `label` the `/key` fixture carries — what must never be rendered.
    /// Same non-shape as [`FAKE_KEY`], for the same reason.
    const FIXTURE_LABEL: &str = "sk-or-EXAMPLE-NOT-A-REAL-KEY-SEE-THE-TEST-THAT-READS-THIS";

    /// A fixed instant every report below is fetched at, never `now_unix()`: a
    /// fixture timestamp judged against the real clock passes only while its
    /// assertions are staleness-insensitive, and half of these are not.
    /// `2026-09-17T09:00:00Z`.
    const FETCHED_AT: i64 = 1_789_635_600;

    /// Every test that binds an ephemeral port takes this for its whole body —
    /// `TcpListener::bind("127.0.0.1:0")` hands out whatever the kernel thinks
    /// is free and cargo runs these as parallel threads in one process, so a
    /// port one test just released can be recycled into another's listener
    /// mid-flight (`crate::usage`'s `TEST_SOCKETS`, #716).
    static TEST_SOCKETS: Mutex<()> = Mutex::new(());

    /// An environment, as [`Poll::from`] / [`CardSettings::resolve`] want one.
    /// `use<>` because the returned closure captures the owned map and *not*
    /// the borrowed slice — without it, edition 2024's precise capturing would
    /// have the opaque type borrow a temporary at every call site below.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn credits() -> Credits {
        Credits {
            total_credits: 100.5,
            total_usage: 25.75,
        }
    }

    fn key_info() -> KeyInfo {
        KeyInfo {
            usage: 5.75,
            limit: Some(10.0),
            limit_remaining: Some(4.25),
            is_free_tier: false,
        }
    }

    fn balance() -> Balance {
        Balance {
            credits: Some(credits()),
            key: Some(key_info()),
            needs_management_key: false,
        }
    }

    fn ok_report() -> Report {
        Report {
            at: FETCHED_AT,
            outcome: Outcome::Ok(balance()),
            last_ok: Some((FETCHED_AT, balance())),
        }
    }

    /// Every `text` in a tree, in order.
    fn texts(node: &Node) -> Vec<String> {
        classed(node).into_iter().map(|(text, _)| text).collect()
    }

    /// Every `(text, classes)` pair in a tree, in order.
    fn classed(node: &Node) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        walk(node, &mut out);
        out
    }

    fn walk(node: &Node, out: &mut Vec<(String, Vec<String>)>) {
        match node {
            Node::Label { text, classes, .. } | Node::Text { text, classes, .. } => {
                out.push((text.clone(), classes.clone()));
            }
            Node::Box { children, .. } | Node::Row { children, .. } => {
                for child in children {
                    walk(child, out);
                }
            }
            _ => {}
        }
    }

    /// The classes the node rendering `value` carries.
    fn classes_of(node: &Node, value: &str) -> Vec<String> {
        let Some((_, classes)) = classed(node).into_iter().find(|(text, _)| text == value) else {
            panic!("no node rendering {value:?} in {:?}", texts(node));
        };
        classes
    }

    /// Read one HTTP request off `sock` (headers only — neither GET has a
    /// body).
    fn capture_request(sock: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0_u8; 1024];
        loop {
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            let n = sock.read(&mut tmp).expect("read request");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// A fake `OpenRouter`: answers `/key` and `/credits` by path, over
    /// `expected` connections (`ureq` sends `connection: close`, so that is one
    /// per request).
    ///
    /// `expected` is a parameter because [`fetch`] deliberately does **not**
    /// make the second request after an unauthorized first one, and a server
    /// waiting on a connection nobody will open would hang the join.
    fn fake_openrouter(
        expected: usize,
        key: (&'static str, &'static str),
        credits: (&'static str, &'static str),
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..expected {
                let (mut sock, _) = listener.accept().expect("accept");
                let raw = capture_request(&mut sock);
                let (status, body) = if raw.contains("/key") { key } else { credits };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                );
                sock.write_all(resp.as_bytes()).expect("write response");
                seen.push(raw);
            }
            seen
        });
        (format!("http://{addr}"), handle)
    }

    // ── (a) The wire, as documented ──────────────────────────────────────────

    /// **The `/credits` contract.** The bytes `OpenRouter`'s own reference prints
    /// parse to exactly the two documented fields.
    #[test]
    fn the_recorded_credits_body_parses_to_the_documented_numbers() {
        let parsed: Envelope<Credits> =
            serde_json::from_str(CREDITS_FIXTURE).expect("the recorded body parses");
        assert_eq!(parsed.data, credits());
    }

    /// **The unit, pinned as a literal** (#1026). The remaining credit is the
    /// *difference* of the two documented fields, in **dollars**:
    /// `100.5 - 25.75 = 74.75`, rendered `$74.75`.
    ///
    /// Falsify by treating either field as cents (a `* 100.0` anywhere between
    /// the wire and [`usd`]): this reads `$7475.00` and reds. The two
    /// `assert_ne!`s are the other half — they name the wrong answers
    /// explicitly, so a change that produces one of them cannot be argued to be
    /// "a different but equivalent rendering".
    #[test]
    fn the_remaining_credit_is_dollars_not_cents() {
        let remaining = credits().remaining();
        assert_eq!(usd(remaining), "$74.75");
        assert_ne!(usd(remaining), "$7475.00", "the wire is dollars, not cents");
        assert_ne!(
            usd(remaining),
            "$0.75",
            "…and it is not cents read as dollars either"
        );
        // Spelled once more straight off the recorded bytes, so the helper
        // above cannot drift from the fixture without this going red.
        let parsed: Envelope<Credits> = serde_json::from_str(CREDITS_FIXTURE).unwrap();
        assert_eq!(usd(parsed.data.remaining()), "$74.75");
    }

    /// **The `/key` contract**, over a fixture carrying every field that page's
    /// published type names: the four this crate reads, and ten it does not.
    #[test]
    fn the_recorded_key_body_parses_to_the_keys_own_limit() {
        let parsed: Envelope<KeyInfo> =
            serde_json::from_str(KEY_FIXTURE).expect("the recorded body parses");
        assert_eq!(parsed.data, key_info());
        assert_eq!(
            usd(parsed.data.remaining().expect("a limited key")),
            "$4.25"
        );
    }

    /// A `null` `limit`/`limit_remaining` — the documented shape of an
    /// unlimited key — is an absence, not a parse error, and yields no
    /// remaining number rather than a zero anybody would read as "spent out".
    #[test]
    fn an_unlimited_key_has_no_remaining_rather_than_a_zero() {
        let body = concat!(
            r#"{"data":{"label":"x","limit":null,"limit_remaining":null,"#,
            r#""usage":3.5,"is_free_tier":true,"rate_limit":{"requests":10}}}"#,
        );
        let parsed: Envelope<KeyInfo> = serde_json::from_str(body).expect("parses");
        assert_eq!(
            parsed.data,
            KeyInfo {
                usage: 3.5,
                limit: None,
                limit_remaining: None,
                is_free_tier: true,
            }
        );
        assert!(parsed.data.remaining().is_none());
    }

    /// Without `limit_remaining` the ceiling minus the spend is the answer —
    /// the arm that exists because that field is documented nullable.
    #[test]
    fn a_key_without_limit_remaining_subtracts_its_own_spend() {
        let info = KeyInfo {
            usage: 2.5,
            limit: Some(10.0),
            limit_remaining: None,
            is_free_tier: false,
        };
        assert_eq!(usd(info.remaining().expect("a limited key")), "$7.50");
    }

    /// An absent or nulled `data` costs the numbers, never the process.
    #[test]
    fn a_missing_data_member_reads_as_zeroes_rather_than_failing() {
        let parsed: Envelope<Credits> = serde_json::from_str("{}").expect("parses");
        assert_eq!(parsed.data, Credits::default());
        let nulled: Envelope<Credits> = serde_json::from_str(r#"{"data":null}"#).expect("parses");
        assert_eq!(nulled.data, Credits::default());
    }

    // ── (b) The fetch ────────────────────────────────────────────────────────

    /// Both requests go out, each to its documented path, each carrying the
    /// bearer and this daemon's own `User-Agent`.
    #[test]
    fn a_poll_asks_both_documented_paths_with_the_bearer() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let (base, server) =
            fake_openrouter(2, ("200 OK", KEY_FIXTURE), ("200 OK", CREDITS_FIXTURE));
        let got = fetch(&base, FAKE_KEY).expect("both endpoints answered");
        let seen = server.join().expect("server thread");

        assert_eq!(got, balance());
        let joined = seen.join("\n");
        assert!(joined.contains("GET /key "), "{joined}");
        assert!(joined.contains("GET /credits "), "{joined}");
        assert_eq!(
            joined.matches(&format!("Bearer {FAKE_KEY}")).count(),
            2,
            "both requests carry the bearer: {joined}"
        );
        assert!(joined.contains("hytte-claude-bridge/"), "{joined}");
    }

    /// **The tier fact, not a failure.** `/credits` is documented as
    /// management-key-only and answers `403` to an ordinary inference key; the
    /// poll still succeeds off `/key`, records why the account balance is
    /// missing, and the card says so.
    #[test]
    fn a_403_on_credits_is_a_management_key_fact_and_not_a_failed_poll() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let (base, server) = fake_openrouter(
            2,
            ("200 OK", KEY_FIXTURE),
            (
                "403 Forbidden",
                r#"{"error":{"message":"management key required"}}"#,
            ),
        );
        let got = fetch(&base, FAKE_KEY).expect("`/key` answered, so the poll stands");
        server.join().expect("server thread");

        assert_eq!(got.credits, None);
        assert!(got.needs_management_key);
        assert_eq!(usd(got.remaining().expect("the key's own leash")), "$4.25");

        let report = Report {
            at: FETCHED_AT,
            outcome: Outcome::Ok(got),
            last_ok: None,
        };
        let text = texts(&card(
            PANEL_IDS,
            &CardSettings::default(),
            &report,
            FETCHED_AT,
        ));
        assert!(
            text.iter().any(|t| t.contains("management key")),
            "the card says why the balance is missing: {text:?}"
        );
    }

    /// A `401` on `/key` is the credential's own verdict, and the second
    /// request is **not** made — there is nothing a management-key-only
    /// endpoint could add about a key the server has already refused.
    #[test]
    fn an_unauthorized_key_fails_the_poll_without_a_second_request() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let (base, server) = fake_openrouter(
            1,
            ("401 Unauthorized", r#"{"error":{"message":"no"}}"#),
            ("200 OK", CREDITS_FIXTURE),
        );
        let err = fetch(&base, FAKE_KEY).expect_err("a refused key has no numbers");
        let seen = server.join().expect("server thread");
        assert_eq!(err, WalletError::Unauthorized);
        assert_eq!(seen.len(), 1, "exactly one request was made: {seen:?}");
        assert!(seen[0].contains("GET /key "), "{seen:?}");
    }

    /// A `403` on `/key` — an endpoint every key may read — is about the
    /// credential too, and reads as unauthorized rather than as a tier fact.
    #[test]
    fn a_403_on_key_reads_as_unauthorized_not_as_a_tier() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let (base, server) = fake_openrouter(
            2,
            ("403 Forbidden", r#"{"error":{}}"#),
            ("403 Forbidden", r#"{"error":{}}"#),
        );
        let err = fetch(&base, FAKE_KEY).expect_err("neither endpoint produced numbers");
        server.join().expect("server thread");
        assert_eq!(err, WalletError::Unauthorized);
    }

    /// A 5xx on both is a plain HTTP failure, reported from `/key` — the
    /// endpoint every key should have been able to read.
    #[test]
    fn a_server_error_on_both_reports_the_key_endpoints_status() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let (base, server) = fake_openrouter(
            2,
            ("503 Service Unavailable", "{}"),
            ("500 Internal Server Error", "{}"),
        );
        let err = fetch(&base, FAKE_KEY).expect_err("no numbers");
        server.join().expect("server thread");
        assert_eq!(err, WalletError::Http(503));
        assert!(err.sentence().contains("503"), "{}", err.sentence());
    }

    /// A 2xx body that is not JSON costs the poll its numbers and nothing else.
    #[test]
    fn an_unparseable_body_is_a_parse_error_not_a_panic() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let (base, server) = fake_openrouter(2, ("200 OK", "not json"), ("200 OK", "not json"));
        let err = fetch(&base, FAKE_KEY).expect_err("no numbers");
        server.join().expect("server thread");
        assert!(matches!(err, WalletError::Parse(_)), "{err:?}");
    }

    /// **The key never leaves this module.** Every arm's sentence is rendered
    /// and searched for the key's bytes; the `Io`/`Parse` arms are the ones
    /// that carry borrowed text at all, and [`crate::usage::scrub`] is what
    /// makes them safe.
    #[test]
    fn an_unauthorized_sentence_carries_no_key_bytes() {
        for error in [
            WalletError::Unauthorized,
            WalletError::Http(500),
            WalletError::Io(crate::usage::scrub(
                &format!("connecting with Bearer {FAKE_KEY} failed"),
                FAKE_KEY,
            )),
            WalletError::Parse(crate::usage::scrub(
                &format!("bad json near {FAKE_KEY}"),
                FAKE_KEY,
            )),
        ] {
            let sentence = error.sentence();
            assert!(
                !sentence.contains(FAKE_KEY),
                "a sentence must never carry the key: {sentence}"
            );
        }
        // …and the scrub is what did it, not luck: the raw text really does
        // contain the key before `fetch`'s error mapping runs it through.
        assert!(
            crate::usage::scrub(&format!("Bearer {FAKE_KEY}"), FAKE_KEY).contains("<redacted>")
        );
    }

    /// The key-shaped `label` the `/key` response carries is never parsed and
    /// so can never reach a node. Falsify by adding a `label` field to
    /// [`KeyInfo`] and rendering it: this reds.
    #[test]
    fn a_keys_label_never_reaches_a_card() {
        let parsed: Envelope<KeyInfo> = serde_json::from_str(KEY_FIXTURE).unwrap();
        let report = Report {
            at: FETCHED_AT,
            outcome: Outcome::Ok(Balance {
                credits: Some(credits()),
                key: Some(parsed.data),
                needs_management_key: false,
            }),
            last_ok: None,
        };
        let rendered = format!(
            "{:?}",
            card(PANEL_IDS, &CardSettings::default(), &report, FETCHED_AT)
        );
        assert!(KEY_FIXTURE.contains(FIXTURE_LABEL), "test setup");
        assert!(
            !rendered.contains(FIXTURE_LABEL),
            "a key-shaped label reached the card: {rendered}"
        );
        assert!(!rendered.contains("sk-or-"), "{rendered}");
    }

    // ── (c) The board ────────────────────────────────────────────────────────

    /// A failed poll keeps the last numbers and marks them stale rather than
    /// blanking the card. Falsify by publishing `last_ok: None` in [`advance`]:
    /// both halves of this red.
    #[test]
    fn a_failed_poll_carries_the_last_numbers_forward() {
        let (ok, carried) = advance(None, FETCHED_AT, Ok(balance()));
        assert!(!ok.is_stale(FETCHED_AT), "a fresh success is not stale");
        assert_eq!(carried, Some((FETCHED_AT, balance())));

        let (failed, still) = advance(carried, FETCHED_AT + 300, Err(WalletError::Http(500)));
        assert_eq!(failed.balance(), Some(&balance()), "the numbers survived");
        assert_eq!(
            failed.numbers_at(),
            Some(FETCHED_AT),
            "…dated when they were fetched, not when the attempt failed"
        );
        assert!(
            failed.is_stale(FETCHED_AT + 300),
            "a carried-forward number is stale by definition"
        );
        assert_eq!(still, carried, "and stays carried for the next round");
    }

    /// The other reason numbers go stale: a poller that stopped publishing.
    /// Judged off [`STALE_AFTER`] against the numbers' own clock.
    #[test]
    fn numbers_past_stale_after_are_stale_even_on_a_success() {
        let report = ok_report();
        let window = i64::try_from(STALE_AFTER.as_secs()).unwrap();
        assert!(
            !report.is_stale(FETCHED_AT + window),
            "the boundary itself is still fresh"
        );
        assert!(report.is_stale(FETCHED_AT + window + 1));
    }

    /// A report with no numbers at all is not "stale" — there is nothing to be
    /// stale — and the failure sentence is the whole card.
    #[test]
    fn a_report_with_no_numbers_is_not_stale_and_says_why() {
        let report = Report {
            at: FETCHED_AT,
            outcome: Outcome::Failed(WalletError::Unauthorized),
            last_ok: None,
        };
        assert!(!report.is_stale(FETCHED_AT));
        assert!(report.balance().is_none());
        assert_eq!(
            freshness(&report, FETCHED_AT),
            "no numbers yet",
            "never `updated just now` over a card with no numbers on it"
        );
        let text = texts(&card(
            PANEL_IDS,
            &CardSettings::default(),
            &report,
            FETCHED_AT,
        ));
        assert!(
            text.iter().any(|t| t.contains("refused the key")),
            "{text:?}"
        );
        assert!(
            !text.iter().any(|t| t.starts_with('$')),
            "no numbers means no money on the card: {text:?}"
        );
    }

    /// The loop itself wires the fetcher, [`advance`] and `publish` together —
    /// a mutation that drops the carried `last_ok` breaks *this*, not only the
    /// pure helper above. Driven under a paused clock, and `yield_now` after
    /// each `advance` because `advance` only marks a sleep ready (#1285).
    #[test]
    fn the_poll_loop_publishes_then_waits_a_whole_period() {
        let _guard = BOARD_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        let calls = Arc::new(AtomicUsize::new(0));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("a current-thread runtime with a paused clock");

        rt.block_on(async {
            let task = {
                let calls = Arc::clone(&calls);
                tokio::spawn(async move {
                    poll_loop(move || {
                        let n = calls.fetch_add(1, AtomicOrdering::SeqCst);
                        async move {
                            if n == 0 {
                                Ok(balance())
                            } else {
                                Err(WalletError::Http(500))
                            }
                        }
                    })
                    .await
                })
            };

            tokio::task::yield_now().await;
            assert_eq!(
                calls.load(AtomicOrdering::SeqCst),
                1,
                "one fetch immediately, before any sleep"
            );
            let first = super::latest().expect("the first report was published");
            assert!(matches!(first.outcome, Outcome::Ok(_)));

            tokio::time::advance(POLL_EVERY).await;
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            assert_eq!(
                calls.load(AtomicOrdering::SeqCst),
                2,
                "one per period, not more"
            );
            let second = super::latest().expect("a second report");
            assert!(matches!(second.outcome, Outcome::Failed(_)));
            assert_eq!(
                second.balance(),
                Some(&balance()),
                "the loop carried the first poll's numbers into the failure"
            );
            task.abort();
        });
    }

    // ── (d) The gate ─────────────────────────────────────────────────────────

    /// **The absent-key path.** No key ⇒ no [`Poll`] ⇒ no task, no request, no
    /// board entry, and — `plugin`'s own test — no card.
    ///
    /// Falsify by defaulting the key to an empty string instead of refusing:
    /// every arm here reds.
    #[test]
    fn no_key_means_no_wallet_at_all() {
        assert!(Poll::from(&env(&[])).is_none(), "unset");
        assert!(Poll::from(&env(&[(KEY_ENV, "")])).is_none(), "empty");
        assert!(Poll::from(&env(&[(KEY_ENV, "   ")])).is_none(), "blank");
        let poll = Poll::from(&env(&[(KEY_ENV, FAKE_KEY)])).expect("a real key is a wallet");
        assert_eq!(
            poll.base_url, DEFAULT_BASE_URL,
            "…aimed at the documented origin, with no env knob in between"
        );
        assert_eq!(poll.key, FAKE_KEY, "…carrying the injected key, trimmed");
    }

    /// `[openrouter] enabled = false`, spelled as the environment: an operator
    /// can keep the wallet off while the key stays injected for something else.
    #[test]
    fn the_enable_knob_can_switch_the_wallet_off_with_a_key_present() {
        for off in ["0", "false", "off", "OFF", " false "] {
            assert!(
                Poll::from(&env(&[(KEY_ENV, FAKE_KEY), (ENABLE_ENV, off)])).is_none(),
                "{off:?} must read as off"
            );
        }
        for on in ["1", "true", "on", "yes", ""] {
            assert!(
                Poll::from(&env(&[(KEY_ENV, FAKE_KEY), (ENABLE_ENV, on)])).is_some(),
                "{on:?} must leave the wallet on — the key is the real gate"
            );
        }
    }

    /// The two render-time knobs, and what an unusable value costs (its own
    /// key, never the card).
    #[test]
    fn the_card_settings_resolve_from_the_launch() {
        assert_eq!(CardSettings::resolve(&env(&[])), CardSettings::default());
        assert_eq!(CardSettings::default().title, DEFAULT_TITLE);

        let set = CardSettings::resolve(&env(&[
            (LABEL_ENV, "  Work router  "),
            (LOW_CREDIT_ENV, "20"),
        ]));
        assert_eq!(set.title, "Work router");
        assert!(set.is_low(19.99) && set.is_low(20.0) && !set.is_low(20.01));

        // A typo costs the threshold, not the card.
        let typo = CardSettings::resolve(&env(&[(LOW_CREDIT_ENV, "twenty")]));
        assert!(typo.is_low(DEFAULT_LOW_CREDIT) && !typo.is_low(DEFAULT_LOW_CREDIT + 0.01));
        // …as does a blank title.
        assert_eq!(
            CardSettings::resolve(&env(&[(LABEL_ENV, "   ")])).title,
            DEFAULT_TITLE
        );
    }

    // ── (e) The card ─────────────────────────────────────────────────────────

    /// The ordinary card: the title, the freshness, the remaining number, the
    /// spend caption and the key's own leash.
    #[test]
    fn the_card_headlines_the_remaining_credit_in_dollars() {
        let text = texts(&card(
            PANEL_IDS,
            &CardSettings::default(),
            &ok_report(),
            FETCHED_AT + 120,
        ));
        assert_eq!(text.first().map(String::as_str), Some(DEFAULT_TITLE));
        assert!(text.contains(&"updated 2 min ago".to_owned()), "{text:?}");
        assert!(text.contains(&"Remaining".to_owned()), "{text:?}");
        assert!(text.contains(&"$74.75".to_owned()), "{text:?}");
        assert!(
            text.iter().any(|t| t.contains("$25.75 of $100.50 used")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|t| t.contains("key limit: $4.25 of $10.00")),
            "{text:?}"
        );
    }

    /// **The low-credit tint** (#1347's `low_credit`): at or below the
    /// threshold the headline turns `warning`; above it, `accent`. Falsify by
    /// dropping the comparison — the first assertion reds.
    #[test]
    fn the_headline_is_tinted_warning_at_or_below_the_threshold() {
        let low = CardSettings {
            title: DEFAULT_TITLE.to_owned(),
            low_credit: 74.75,
        };
        assert_eq!(
            classes_of(&card(PANEL_IDS, &low, &ok_report(), FETCHED_AT), "$74.75"),
            vec!["numeric".to_owned(), "warning".to_owned()],
            "at the threshold exactly"
        );

        let high = CardSettings {
            title: DEFAULT_TITLE.to_owned(),
            low_credit: 74.74,
        };
        assert_eq!(
            classes_of(&card(PANEL_IDS, &high, &ok_report(), FETCHED_AT), "$74.75"),
            vec!["numeric".to_owned(), "accent".to_owned()],
            "a hair above it"
        );
    }

    /// **The stale path** (#1347: "`stale` with the last-seen numbers"): the
    /// numbers are still drawn, the header says `stale`, and the footer says
    /// why.
    ///
    /// Falsify by publishing while stale — drop the `stale · ` prefix in
    /// [`freshness`], or its `is_stale` branch — and the first two assertions
    /// red.
    #[test]
    fn a_failed_poll_draws_the_last_numbers_under_a_stale_header() {
        let report = Report {
            at: FETCHED_AT + 300,
            outcome: Outcome::Failed(WalletError::Http(502)),
            last_ok: Some((FETCHED_AT, balance())),
        };
        let now = FETCHED_AT + 300;
        assert_eq!(freshness(&report, now), "stale · updated 5 min ago");
        let text = texts(&card(PANEL_IDS, &CardSettings::default(), &report, now));
        assert!(
            text.iter().any(|t| t.starts_with("stale · ")),
            "the header says stale: {text:?}"
        );
        assert!(
            text.contains(&"$74.75".to_owned()),
            "…over the last-seen numbers: {text:?}"
        );
        assert!(
            text.iter().any(|t| t.contains("HTTP 502")),
            "…and says why: {text:?}"
        );
        // A fresh success says none of that.
        assert_eq!(freshness(&ok_report(), FETCHED_AT), "updated just now");
    }

    /// An unlimited key with no management access has no "remaining" anybody
    /// could compute — the card headlines the spend instead of inventing a
    /// ceiling.
    #[test]
    fn a_key_with_no_ceiling_headlines_its_spend() {
        let report = Report {
            at: FETCHED_AT,
            outcome: Outcome::Ok(Balance {
                credits: None,
                key: Some(KeyInfo {
                    usage: 12.5,
                    limit: None,
                    limit_remaining: None,
                    is_free_tier: false,
                }),
                needs_management_key: true,
            }),
            last_ok: None,
        };
        let text = texts(&card(
            PANEL_IDS,
            &CardSettings::default(),
            &report,
            FETCHED_AT,
        ));
        assert!(text.contains(&"Spent".to_owned()), "{text:?}");
        assert!(text.contains(&"$12.50".to_owned()), "{text:?}");
        assert!(!text.contains(&"Remaining".to_owned()), "{text:?}");
    }

    /// The two surfaces draw the same card under different ids — the drawer
    /// page and a sidebar mount cannot collapse onto one renderer instance.
    #[test]
    fn the_two_surfaces_carry_different_ids_for_the_same_card() {
        assert_ne!(PANEL_IDS, SIDEBAR_IDS);
        for ids in [PANEL_IDS, SIDEBAR_IDS] {
            let tree = card(ids, &CardSettings::default(), &ok_report(), FETCHED_AT);
            match &tree {
                Node::Box { id, children, .. } => {
                    assert_eq!(id.as_deref(), Some(ids.root));
                    match &children[0] {
                        Node::Box { id, .. } => assert_eq!(id.as_deref(), Some(ids.list)),
                        other => panic!("the card's list must be a box, got {other:?}"),
                    }
                }
                other => panic!("the card root must be a box, got {other:?}"),
            }
        }
        // Same content either way — only the ids differ.
        assert_eq!(
            texts(&card(
                PANEL_IDS,
                &CardSettings::default(),
                &ok_report(),
                FETCHED_AT
            )),
            texts(&card(
                SIDEBAR_IDS,
                &CardSettings::default(),
                &ok_report(),
                FETCHED_AT
            )),
        );
    }

    // ── (f) The chip's hover line ────────────────────────────────────────────

    /// No wallet, no line — a hover must not grow a sentence about a thing that
    /// does not exist. Falsify by returning a line for `None`: this reds.
    #[test]
    fn the_hover_line_is_absent_without_a_wallet() {
        assert_eq!(hover_line(None, FETCHED_AT), None);
    }

    /// With a wallet the line names the remaining credit, in the same dollars
    /// the card shows, and says `stale` in the same breath the card does.
    #[test]
    fn the_hover_line_names_the_remaining_credit() {
        assert_eq!(
            hover_line(Some(&ok_report()), FETCHED_AT).as_deref(),
            Some("OpenRouter · $74.75 left"),
        );
        let stale = Report {
            at: FETCHED_AT + 300,
            outcome: Outcome::Failed(WalletError::Http(502)),
            last_ok: Some((FETCHED_AT, balance())),
        };
        assert_eq!(
            hover_line(Some(&stale), FETCHED_AT + 300).as_deref(),
            Some("OpenRouter · $74.75 left (stale)"),
        );
        // With nothing ever fetched the line is the failure itself — the hover
        // is the only place a broken poll could be seen without a click.
        let dead = Report {
            at: FETCHED_AT,
            outcome: Outcome::Failed(WalletError::Unauthorized),
            last_ok: None,
        };
        let line = hover_line(Some(&dead), FETCHED_AT).expect("a line");
        assert!(line.contains("refused the key"), "{line}");
    }

    // ── (g) Money ────────────────────────────────────────────────────────────

    /// [`usd`]'s whole contract, including the two values a bare `{:.2}` gets
    /// wrong.
    #[test]
    fn usd_renders_dollars_and_never_a_nan() {
        assert_eq!(usd(0.0), "$0.00");
        assert_eq!(usd(1.5), "$1.50");
        assert_eq!(usd(1234.567), "$1234.57");
        assert_eq!(usd(-1.234), "-$1.23", "an overspent account, not `$-1.23`");
        assert_eq!(usd(f64::NAN), "—");
        assert_eq!(usd(f64::INFINITY), "—");
    }

    /// A zero (or absent) ceiling has no meaningful spend fraction and must not
    /// paint a full bar.
    #[test]
    fn a_zero_ceiling_reads_as_an_empty_bar() {
        assert!(
            Credits {
                total_credits: 0.0,
                total_usage: 5.0,
            }
            .spent_fraction()
                < f64::EPSILON
        );
        let half = Credits {
            total_credits: 10.0,
            total_usage: 5.0,
        };
        assert!((half.spent_fraction() - 0.5).abs() < 1e-9);
    }

    /// The ids are what the host reconciles widgets by, so a rename is a real
    /// change and this is where it is noticed.
    #[test]
    fn the_card_ids_are_these_names() {
        assert_eq!(
            PANEL_IDS,
            Ids {
                root: "claude-bridge-panel-openrouter",
                list: "claude-bridge-panel-openrouter-list",
            }
        );
        assert_eq!(
            SIDEBAR_IDS,
            Ids {
                root: "claude-bridge-card-openrouter",
                list: "claude-bridge-card-openrouter-list",
            }
        );
    }
}
