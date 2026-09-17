//! The **Claude usage limits** the bridge polls and its chip paints (#1236).
//!
//! # What this reads, and why it exists at all
//!
//! Nothing in the tree knew the rolling 5-hour / weekly rate-limit percentages
//! before this. #320's `hytte-plugin-usage` polled a Grafana dashboard fed by
//! Claude Code's OTEL export and — by its own module doc — could never have
//! shown them: that surface exports **spend**, and the quota percentages are not
//! derivable from it. #1200 called it dead weight and #1205 deleted it.
//!
//! This module reads the real numbers instead, from the same endpoint Claude
//! Code's own `/usage` screen draws:
//!
//! ```text
//! GET https://api.anthropic.com/api/oauth/usage
//! Authorization: Bearer <the accessToken in ~/.claude/.credentials.json>
//! ```
//!
//! The precondition — *Claude Code is logged in on this box* — is already the
//! bridge's precondition in its two subscription modes, so it adds nothing new.
//!
//! # The wire contract: render whatever the server sends (Mara, #1236)
//!
//! The endpoint is **unofficial and undocumented**, and the measured response
//! carries a fistful of codename-keyed fields (`amber_ladder`, `nimbus_quill`,
//! `juniper_tide`, …) that will churn. So this parses exactly two things —
//! [`Usage::limits`] and [`Usage::extra_usage`] — and ignores everything else,
//! including the named `five_hour` / `seven_day` objects, which are the same
//! rows spelled twice.
//!
//! [`limits`](Usage::limits) is a **list**, and it is rendered as a list: one
//! meter per entry, labelled from [`Limit::kind`], coloured from
//! [`Limit::severity`]. A bucket Anthropic adds shows up on the next poll with
//! no code change; an unknown `kind` still renders under its raw name
//! ([`humanise_kind`]); an unknown `severity` reads as normal. Nothing is keyed
//! by a name this crate hard-codes. No `deny_unknown_fields` anywhere here — a
//! field they rename is ignored, not fatal.
//!
//! # Credentials are **read-only**, and never leave this module
//!
//! The access token is short-lived (hours) and **Claude Code refreshes it; we
//! must not**. Rotating the refresh token out from under the CLI is the one way
//! to break the user's own login. So:
//!
//! - the file is **read** on every poll (never cached across polls, never
//!   written, never `chmod`ed), by [`read_access_token`];
//! - no refresh endpoint is ever called — a 401/403 is
//!   [`UsageError::Unauthorized`], whose sentence is "run `claude` once";
//! - the token never reaches a log line, a tooltip, a panel, or an error. The
//!   two error arms that carry a borrowed message ([`UsageError::Io`],
//!   [`UsageError::Parse`]) run it through [`scrub`] first, so even a transport
//!   error that somehow echoed the header cannot carry it out;
//! - [`Credentials`] and [`OauthCreds`] deliberately derive **no** `Debug`.
//!
//! # Where the poll lives
//!
//! On the bridge's own HTTP runtime, spawned by `main` next to the accept loop —
//! not from the plugin SDK's session. The chip's 5 s tick then reads the last
//! [`Report`] off the process-global board below, the same
//! two-runtimes-meet-at-a-static shape [`crate::status`] documents. A poll is a
//! blocking `ureq` call on `spawn_blocking`, exactly like [`crate::messages`].
//!
//! # A proxy is honoured, by ureq's own default
//!
//! `ureq::Config::default()` resolves `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`
//! (`NO_PROXY` respected) the same way every other `ureq` call in this crate
//! does — `fetch` sets no proxy of its own and does nothing to opt out. TLS
//! to `api.anthropic.com` stays end-to-end under rustls, so a configured proxy
//! sees a `CONNECT` and never the bearer: the same position
//! [`crate::envguard`] already takes for `HTTPS_PROXY` on the `claude` child.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

/// The API host the usage endpoint lives on. A parameter of `fetch` rather
/// than a constant it reaches for, so a test can point it at a
/// `std::net::TcpListener` serving a canned body — there is deliberately **no**
/// environment override, because an env knob on an endpoint that spends a bearer
/// token is a footgun nobody asked for.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The path appended to the base URL. Measured, not documented — see the module
/// docs.
const USAGE_PATH: &str = "/api/oauth/usage";

/// The `User-Agent` the poll identifies itself with. Says which program is
/// asking; carries no account identity of its own.
const USER_AGENT: &str = concat!("hytte-claude-bridge/", env!("CARGO_PKG_VERSION"));

/// Whole-round-trip budget for one poll.
///
/// Unlike [`crate::messages`]'s budget this bounds nothing a client is waiting
/// on — the chip renders the *previous* report while a poll is in flight — so it
/// is generous enough to ride out a slow network rather than tuned against a
/// caller's timeout.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Connect budget inside [`TIMEOUT`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the poll runs. Quota percentages move on the scale of a
/// conversation, not a frame, and this is an undocumented endpoint spending a
/// login token: five minutes is frequent enough that the chip is never
/// meaningfully behind and infrequent enough to be invisible.
pub const POLL_EVERY: Duration = Duration::from_mins(5);

/// The 429 backoff's ceiling (#1283) — [`next_wait`] never returns more than
/// this, however long a `Retry-After` asks for or however many consecutive
/// 429s there have been.
///
/// **Bounded by [`STALE_AFTER`] itself**, not chosen independently — the
/// product call taken on the #1283 thread after #1285's review found the
/// two fighting: shipped at 30 min against a 15 min staleness window, the
/// carry-forward this PR added was *guaranteed* to go stale for the back
/// half of every backoff cycle (numbers fetched at `T` are stale at
/// `T + STALE_AFTER`; the next attempt was at `T + 30 min`). Capping the
/// schedule at the window instead means the meters this PR keeps alive can
/// never go blank *because of* the schedule — only because the server has
/// genuinely refused for a whole `STALE_AFTER` straight. Under a permanent
/// 429 the bridge then polls once every `MAX_BACKOFF` (== `STALE_AFTER`),
/// four times an hour — a quarter of the ordinary [`POLL_EVERY`] cadence,
/// and comfortably inside the "at most every 3 min" ceiling #1283 was filed
/// under. See the `MAX_BACKOFF <= STALE_AFTER` assertion below, which pins
/// the direction of this dependency at compile time.
pub const MAX_BACKOFF: Duration = STALE_AFTER;

/// How old the numbers behind a [`Report`] may be before the chip stops
/// trusting them — judged against [`Report::numbers_at`], not
/// [`Report::at`] (see [`Report::is_stale`]).
///
/// Three poll periods **at the ordinary cadence**: a single slow round-trip
/// (the fetch's own [`TIMEOUT`] plus whatever the next tick catches up on)
/// never trips it, but a `poll_forever` that has stopped publishing at all —
/// it is deliberately **unsupervised** (`main`'s doc comment) — goes visibly
/// stale within one window instead of leaving confidently-wrong meters on
/// the bar forever.
///
/// [`MAX_BACKOFF`] is defined *from* this constant, not the other way
/// around (see its doc) — a *healthy* poller backing off a sustained run of
/// 429s reaches this ceiling exactly when the backoff itself hits its cap,
/// never before: the numbers really are that old, whether the reason is a
/// wedged task or an account still genuinely rate-limited.
pub const STALE_AFTER: Duration = Duration::from_secs(POLL_EVERY.as_secs() * 3);

/// The invariant [`MAX_BACKOFF`]'s doc promises, pinned so it cannot silently
/// drift back apart the way it did before #1285's review: the 429 schedule
/// must never be able to outlive the window that decides whether its own
/// carry-forward numbers are still trusted.
const _: () = assert!(MAX_BACKOFF.as_secs() <= STALE_AFTER.as_secs());

/// Longest borrowed error text kept in a [`UsageError`]. A transport error can
/// be a paragraph; a chip tooltip cannot.
const MAX_ERROR_CHARS: usize = 160;

/// The severity a row with no (or an unknown) `severity` reads as.
pub const SEVERITY_NORMAL: &str = "normal";

// ── The wire ─────────────────────────────────────────────────────────────────

/// A field that tolerates an explicit `null` as well as an absent key.
///
/// `#[serde(default)]` alone covers the missing key; a `null` in a
/// non-`Option` field is still a hard error without this. The endpoint nulls
/// fields freely (`scope`, `locked_reason`, half of `extra_usage`), so every
/// non-`Option` field here goes through it.
fn lenient<'de, D, T>(de: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(de)?.unwrap_or_default())
}

/// One poll's parsed payload — **only** the two generic things, never a named
/// bucket. See the module docs.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Usage {
    /// Every rate-limit row the server chose to send, in the server's own order.
    #[serde(default, deserialize_with = "lenient")]
    pub limits: Vec<Limit>,
    /// The pay-as-you-go overflow allowance, when the account has one.
    #[serde(default)]
    pub extra_usage: Option<ExtraUsage>,
}

/// One rate-limit row.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Limit {
    /// The server's own name for the bucket (`"session"`, `"weekly_all"`,
    /// `"weekly_scoped"`, …). Rendered through [`humanise_kind`], which falls
    /// back to the raw name — an unknown kind is a row, not a gap.
    #[serde(default, deserialize_with = "lenient")]
    pub kind: String,
    /// The coarser family the row belongs to (`"session"`, `"weekly"`). Shown in
    /// the panel caption; not interpreted.
    #[serde(default)]
    pub group: Option<String>,
    /// Percent of the bucket consumed. Arrives as a JSON **integer** today;
    /// `f64` tolerates both spellings.
    #[serde(default, deserialize_with = "lenient")]
    pub percent: f64,
    /// `"normal"` / `"warning"` / `"critical"` — what the official UI colours
    /// by. `None` (missing or null) and any unknown word read as
    /// [`SEVERITY_NORMAL`]; use [`Limit::severity`].
    #[serde(default, rename = "severity")]
    pub severity_raw: Option<String>,
    /// RFC 3339, e.g. `"2026-09-13T13:50:00.101848+00:00"`.
    #[serde(default)]
    pub resets_at: Option<String>,
    /// Whether the bucket is currently counting. `None` (an older or narrower
    /// response) is treated as **active**: a row with no flag is still a row.
    #[serde(default)]
    pub is_active: Option<bool>,
}

impl Limit {
    /// The severity word, defaulted.
    #[must_use]
    pub fn severity(&self) -> &str {
        self.severity_raw
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(SEVERITY_NORMAL)
    }

    /// Whether the bucket is counting — see [`Limit::is_active`].
    #[must_use]
    pub fn active(&self) -> bool {
        self.is_active.unwrap_or(true)
    }

    /// The meter level, `0.0..=1.0`.
    #[must_use]
    pub fn fraction(&self) -> f64 {
        if self.percent.is_finite() {
            (self.percent / 100.0).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

/// The pay-as-you-go overflow allowance.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ExtraUsage {
    /// Whether the account has it turned on. The whole row is hidden when this
    /// is false, which is the common case.
    #[serde(default, deserialize_with = "lenient")]
    pub is_enabled: bool,
    /// Percent of the allowance spent, when known.
    #[serde(default)]
    pub utilization: Option<f64>,
    /// Credits spent this period, when known.
    #[serde(default)]
    pub used_credits: Option<f64>,
    /// The ceiling those credits count against, when known.
    #[serde(default)]
    pub monthly_limit: Option<f64>,
    /// The currency `used_credits`/`monthly_limit` are denominated in.
    #[serde(default)]
    pub currency: Option<String>,
}

/// The credential file's shape — **no `Debug`**, deliberately (module docs).
#[derive(Deserialize)]
struct Credentials {
    #[serde(default, rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OauthCreds>,
}

/// The one field of it this crate reads. The refresh token sitting next to it in
/// the file is deliberately not modelled: a type that cannot hold it cannot leak
/// it, and this module must never refresh.
#[derive(Deserialize)]
struct OauthCreds {
    #[serde(default, rename = "accessToken")]
    access_token: Option<String>,
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// Why a poll produced no numbers. Every arm has a one-line, user-facing
/// [`sentence`](UsageError::sentence) — the chip tooltip and the panel header
/// show that string and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsageError {
    /// No usable `accessToken` at that path: missing file, unreadable file,
    /// unparseable JSON, or a JSON document with no token in it. One arm for all
    /// four because the user's move is the same in every case.
    NoCredentials(PathBuf),
    /// 401 or 403 — the token expired (they last hours) or was revoked. **Not**
    /// a signal to refresh: `claude` owns that rotation.
    Unauthorized,
    /// Any other non-2xx. `fetch` reports a 429 through this arm too — it
    /// carries no schedule of its own, so this is what a raw fetch call sees.
    Http(u16),
    /// A 429, **after** [`next_wait`] has picked how long to wait before
    /// trying again. Never constructed by `fetch` (which reports a 429 as
    /// plain [`Http`](Self::Http)) — only [`advance`] promotes one to this,
    /// since it is the one place that knows the wait. Carries the **deadline**
    /// it actually chose (`at + wait`, Unix seconds) rather than the wait
    /// itself: [`sentence`](Self::sentence) takes `now` and counts down to
    /// this at render time, so the same report reads "next try in 10 min" and
    /// then "next try in 9 min" as the chip keeps ticking, instead of freezing
    /// the number the schedule picked at publish time (#1285's review, NIT).
    RateLimited(i64),
    /// The request never completed (DNS, TLS, connect, read).
    Io(String),
    /// A 2xx body that did not parse as [`Usage`].
    Parse(String),
}

impl UsageError {
    /// The one line a human reads. Never contains the token — see the module
    /// docs and `an_unauthorized_sentence_carries_no_token_bytes`.
    ///
    /// `now` only feeds [`RateLimited`](Self::RateLimited)'s countdown; every
    /// other arm's wording is fixed at construction and ignores it.
    #[must_use]
    pub fn sentence(&self, now: i64) -> String {
        match self {
            Self::NoCredentials(path) => format!(
                "no Claude login found at {} — run `claude` once to sign in",
                path.display()
            ),
            Self::Unauthorized => "usage stale — run `claude` once to refresh the login".to_owned(),
            Self::Http(status) => {
                format!("usage unavailable — the usage endpoint answered HTTP {status}")
            }
            Self::RateLimited(deadline) => {
                let remaining = deadline.saturating_sub(now);
                if remaining <= 0 {
                    // The countdown reached zero (or the clock moved) between
                    // publish and render — never claim a negative wait.
                    "usage rate-limited — next try any moment now".to_owned()
                } else {
                    // `div_ceil`, never a plain divide: telling someone "next
                    // try in 1 min" when the schedule actually chose 90 s
                    // would have them retrying before the wait is over.
                    // `i64::div_ceil` is still unstable (rust-lang/rust#88581)
                    // for signed integers, so widen to `u64` first — `remaining`
                    // is checked positive above.
                    let remaining = u64::try_from(remaining).unwrap_or(u64::MAX);
                    format!(
                        "usage rate-limited — next try in {} min",
                        remaining.div_ceil(60)
                    )
                }
            }
            Self::Io(what) => format!("usage unavailable — {what}"),
            Self::Parse(what) => {
                format!("usage unavailable — the response could not be read ({what})")
            }
        }
    }
}

/// Cut `text` to [`MAX_ERROR_CHARS`] on a char boundary.
fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_ERROR_CHARS {
        return text.to_owned();
    }
    let head: String = text.chars().take(MAX_ERROR_CHARS).collect();
    format!("{head}…")
}

/// Replace every occurrence of `token` in `text` with `<redacted>`.
///
/// Belt and braces: no library here is *supposed* to echo a request header into
/// an error, and none observed does. But this module's whole contract is that
/// the token never leaves it, and a contract enforced by a pass over the string
/// costs nothing next to one enforced by trusting three dependencies.
fn scrub(text: &str, token: &str) -> String {
    if token.is_empty() {
        return text.to_owned();
    }
    text.replace(token, "<redacted>")
}

// ── Credentials ──────────────────────────────────────────────────────────────

/// The credential file Claude Code keeps its OAuth login in.
///
/// Resolved the way the CLI resolves it: `$CLAUDE_CONFIG_DIR` replaces the whole
/// `~/.claude` directory when it is set — the same variable
/// [`crate::envguard`] deliberately **allows** through for the `claude` child,
/// because it only moves where the login credential is *read from*, which is
/// itself a legitimate setting the child may need in order to find a login at
/// all — otherwise `$HOME/.claude`.
#[must_use]
pub fn credentials_path() -> PathBuf {
    credentials_path_in(
        crate::env_nonempty("CLAUDE_CONFIG_DIR"),
        crate::env_nonempty("HOME"),
    )
}

/// [`credentials_path`] with the environment stated explicitly.
///
/// With neither variable set this returns the *literal* `~/.claude/…` spelling
/// rather than an `Option` the caller would have to invent a message for: the
/// path is only ever opened (and then named in
/// [`UsageError::NoCredentials`]'s sentence), and "no Claude login found at
/// ~/.claude/.credentials.json" is exactly the right thing to tell somebody
/// whose `$HOME` is unset.
#[must_use]
pub fn credentials_path_in(config_dir: Option<String>, home: Option<String>) -> PathBuf {
    const FILE: &str = ".credentials.json";
    if let Some(dir) = config_dir {
        return PathBuf::from(dir).join(FILE);
    }
    if let Some(home) = home {
        return PathBuf::from(home).join(".claude").join(FILE);
    }
    PathBuf::from("~/.claude").join(FILE)
}

/// Read the access token out of `path`.
///
/// Read-only by construction: [`std::fs::read_to_string`] and nothing else. The
/// returned `String` is handed straight to one `Authorization` header and
/// dropped.
///
/// # Errors
///
/// [`UsageError::NoCredentials`] for every failure mode — see that variant.
fn read_access_token(path: &Path) -> Result<String, UsageError> {
    let missing = || UsageError::NoCredentials(path.to_path_buf());
    let raw = std::fs::read_to_string(path).map_err(|_| missing())?;
    let doc: Credentials = serde_json::from_str(&raw).map_err(|_| missing())?;
    doc.claude_ai_oauth
        .and_then(|oauth| oauth.access_token)
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
        .ok_or_else(missing)
}

// ── The fetch ────────────────────────────────────────────────────────────────

/// One poll: read the token, `GET {base_url}/api/oauth/usage`, parse the two
/// generic fields — the plain `Result` shape most of this module's own tests
/// want.
///
/// `poll_forever` (the one production caller) needs the `Retry-After` header
/// too, since #1283, and calls [`fetch_with_retry_after`] directly instead;
/// that is the whole reason this is `#[cfg(test)]` rather than the shared
/// entry point both paths go through — a production caller of the plain
/// `Result` shape would make this a real wrapper again, but there is none
/// today, and an always-compiled function with no caller outside `mod tests`
/// is dead code in the shipped binary.
///
/// Blocking — call it from `spawn_blocking`, the way [`fetch_with_retry_after`]
/// does and [`crate::messages`] already does for its own `ureq` client.
///
/// # Errors
///
/// Every arm of [`UsageError`]; each carries a one-line
/// [`sentence`](UsageError::sentence) and **never** the bearer token.
#[cfg(test)]
pub fn fetch(base_url: &str, credentials_path: &Path) -> Result<Usage, UsageError> {
    fetch_with_retry_after(base_url, credentials_path).0
}

/// `fetch`'s logic, also returning the response's `Retry-After` header
/// (#1283) parsed to a [`Duration`] when the server sent one — read
/// regardless of status, though only [`poll_forever`] ever looks at it, and
/// only for a 429.
///
/// Both spellings RFC 7231 §7.1.3 allows are parsed
/// ([`parse_retry_after`]): delta-seconds, and the IMF-fixdate form via
/// [`parse_http_date`], resolved against [`now_unix`] into a duration the
/// same way delta-seconds already is one.
fn fetch_with_retry_after(
    base_url: &str,
    credentials_path: &Path,
) -> (Result<Usage, UsageError>, Option<Duration>) {
    let mut retry_after = None;
    let result = (|| -> Result<Usage, UsageError> {
        let token = read_access_token(credentials_path)?;

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(TIMEOUT))
            // Read the endpoint's own body on a non-2xx rather than collapsing it
            // into a transport error: the status is what decides the arm below.
            .http_status_as_error(false)
            // This is a single constant URL with no business redirecting. Without
            // this, the bearer's only protection on a cross-host redirect is
            // `ureq::Config::default()`'s `RedirectAuthHeaders::Never` — a
            // dependency default this crate does not pin — and a redirect target
            // that happens to answer `200 {}` would read as "this account has no
            // limits" instead of an error. With `max_redirects(0)` the redirect is
            // never followed at all (no second connection, so no header can ever
            // reach it) and its own status falls through to the `other` arm below
            // as `UsageError::Http`.
            .max_redirects(0)
            .build()
            .into();

        let url = format!("{}{USAGE_PATH}", base_url.trim_end_matches('/'));
        let mut resp = agent
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| UsageError::Io(truncate(&scrub(&e.to_string(), &token))))?;

        retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| parse_retry_after(v, now_unix()));

        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| UsageError::Io(truncate(&scrub(&e.to_string(), &token))))?;

        match status {
            200..=299 => serde_json::from_str::<Usage>(&body)
                .map_err(|e| UsageError::Parse(truncate(&scrub(&e.to_string(), &token)))),
            401 | 403 => Err(UsageError::Unauthorized),
            other => Err(UsageError::Http(other)),
        }
    })();
    (result, retry_after)
}

// ── The board the chip reads ─────────────────────────────────────────────────

/// One poll's outcome.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Numbers.
    Ok(Usage),
    /// No numbers, and why.
    Failed(UsageError),
}

/// The last poll: when it ran, how it went, and — since #1283 — the last
/// numbers that actually came back, even if this attempt did not produce
/// them.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Unix seconds at which the attempt completed.
    pub at: i64,
    /// What this attempt produced.
    pub outcome: Outcome,
    /// The most recent successful poll's `(at, Usage)`, carried forward
    /// across a failure so a transient error does not blank a chip that has
    /// good numbers to show. `None` only before this process has seen a
    /// success. Set to `Some((self.at, usage))` whenever `outcome` is
    /// [`Outcome::Ok`] — redundant with the payload already sitting in
    /// `outcome` there, but it means a reader of `last_ok` alone never has to
    /// special-case which outcome produced it.
    pub last_ok: Option<(i64, Usage)>,
}

impl Report {
    /// The numbers to render, whether they came from this attempt or a
    /// previous one: this attempt's own payload on [`Outcome::Ok`], the
    /// carried-forward [`Report::last_ok`] on [`Outcome::Failed`] (`None`
    /// before any poll has ever succeeded).
    #[must_use]
    pub fn usage(&self) -> Option<&Usage> {
        match &self.outcome {
            Outcome::Ok(usage) => Some(usage),
            Outcome::Failed(_) => self.last_ok.as_ref().map(|(_, usage)| usage),
        }
    }

    /// The failure sentence, if the last attempt failed — regardless of
    /// whether [`usage`](Self::usage) still has last-known-good numbers to
    /// show alongside it. `now` is threaded through to
    /// [`UsageError::sentence`], which is the only place it matters (the
    /// [`UsageError::RateLimited`] countdown).
    #[must_use]
    pub fn error(&self, now: i64) -> Option<String> {
        match &self.outcome {
            Outcome::Ok(_) => None,
            Outcome::Failed(e) => Some(e.sentence(now)),
        }
    }

    /// When the numbers [`usage`](Self::usage) would return were actually
    /// fetched — [`Report::at`] on a success, [`last_ok`](Self::last_ok)'s
    /// own timestamp on a failure that still has one, `None` when there are
    /// no numbers at all.
    #[must_use]
    pub fn numbers_at(&self) -> Option<i64> {
        match &self.outcome {
            Outcome::Ok(_) => Some(self.at),
            Outcome::Failed(_) => self.last_ok.as_ref().map(|(at, _)| *at),
        }
    }

    /// Whether the numbers [`usage`](Self::usage) would return are too old to
    /// trust — see [`STALE_AFTER`].
    ///
    /// Judged against [`numbers_at`](Self::numbers_at), **not**
    /// [`Report::at`]: a report can be a perfectly fresh *failure* (`at` is
    /// this instant) while the numbers it is still offering are several polls
    /// old, and staleness has to track those, not the moment of the attempt
    /// that failed to refresh them. A report with no numbers at all is never
    /// "stale" — there is nothing to be stale, and every caller here already
    /// gates on [`usage`](Self::usage) being `Some` before asking.
    #[must_use]
    pub fn is_stale(&self, now: i64) -> bool {
        let Some(basis) = self.numbers_at() else {
            return false;
        };
        let ceiling = i64::try_from(STALE_AFTER.as_secs()).unwrap_or(i64::MAX);
        now.saturating_sub(basis) > ceiling
    }
}

/// The last report, or `None` before the first poll completes.
static BOARD: Mutex<Option<Report>> = Mutex::new(None);
/// Bumped on every [`publish`]. The chip compares it to the one it last read and
/// skips the lock-and-clone when nothing has moved — the poll runs every five
/// minutes and the chip ticks every five seconds, so that is 59 ticks in 60.
static VERSION: AtomicU64 = AtomicU64::new(0);

/// Publish one poll's outcome. Called only from [`poll_forever`].
///
/// A poisoned lock (some other thread panicked while holding it) is swallowed
/// rather than propagated, the same rule [`crate::status`] states: a status
/// readout must never be able to take the bridge down.
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

/// The wait before the *next* poll, given what this one did.
///
/// Only a 429 backs off at all: every other outcome — a success, or any other
/// failure (a 500, a network blip, an expired login) — answers
/// [`POLL_EVERY`], because none of those are the account's own rate limiter
/// telling us to slow down, and doubling the wait on one would only make the
/// chip slower to recover from an already-transient problem. On a 429 this
/// honours the server's own `retry_after` when it sent one, else doubles
/// `prev_wait`; either way the result is clamped to `[POLL_EVERY,
/// MAX_BACKOFF]`, so a chain of them is `POLL_EVERY` → `10 min` →
/// `MAX_BACKOFF` → `MAX_BACKOFF` — never below the ordinary cadence, never
/// above the cap (`MAX_BACKOFF` is 15 min, so doubling from `POLL_EVERY`
/// reaches it on the second step, not the third).
///
/// `outcome` is the *raw* fetch outcome — a 429 still reads as
/// [`UsageError::Http`], not yet [`UsageError::RateLimited`]; promoting it is
/// [`advance`]'s job, once this has picked the wait that promotion names.
///
/// A non-429 failure resets a 429 chain **all the way** to [`POLL_EVERY`],
/// even mid-chain — deliberate, not an oversight (#1285's review, LOW). The
/// backoff schedule is a response to the *server* asking us to slow down; a
/// transport failure (a dropped connection, a timeout, an unrelated 500)
/// carries no such instruction, and there is no way to tell from here
/// whether the rate limit that produced the earlier 429s is even still in
/// effect. Forgetting the server's own schedule on the first sign of an
/// unrelated problem costs at most one poll at the ordinary cadence; the
/// alternative — remembering a 429 chain through a failure that has nothing
/// to do with it — risks staying artificially slow for a condition that may
/// already be over, with nothing that would ever notice and correct it.
fn next_wait(prev_wait: Duration, outcome: &Outcome, retry_after: Option<Duration>) -> Duration {
    let Outcome::Failed(UsageError::Http(429)) = outcome else {
        return POLL_EVERY;
    };
    let candidate = retry_after.unwrap_or_else(|| prev_wait.saturating_mul(2));
    candidate.clamp(POLL_EVERY, MAX_BACKOFF)
}

/// One poll's book-keeping: given the wait and the last-known-good numbers
/// the previous iteration handed forward, and what this fetch just produced,
/// decide the [`Report`] to publish and the state to carry into the next
/// iteration.
///
/// Pulled out of [`poll_forever`] so both halves of #1283 — carrying
/// `last_ok` across a failure, and the 429 backoff schedule — are plain
/// function calls a test can drive without an event loop or a real clock.
///
/// Falsify the carry-forward by publishing `last_ok: None` unconditionally
/// here (the pre-#1283 "replacing" publish): the tests documenting item 1
/// (a)/(b) go red.
fn advance(
    prev_wait: Duration,
    prev_last_ok: Option<(i64, Usage)>,
    at: i64,
    result: Result<Usage, UsageError>,
    retry_after: Option<Duration>,
) -> (Report, Duration, Option<(i64, Usage)>) {
    let (outcome, last_ok) = match result {
        Ok(usage) => (Outcome::Ok(usage.clone()), Some((at, usage))),
        Err(e) => (Outcome::Failed(e), prev_last_ok),
    };
    let wait = next_wait(prev_wait, &outcome, retry_after);
    let outcome = match outcome {
        Outcome::Failed(UsageError::Http(429)) => {
            // The deadline this attempt's own clock names, not a duration
            // frozen at publish time — see `UsageError::RateLimited`'s doc.
            let wait_secs = i64::try_from(wait.as_secs()).unwrap_or(i64::MAX);
            Outcome::Failed(UsageError::RateLimited(at.saturating_add(wait_secs)))
        }
        other => other,
    };
    let report = Report {
        at,
        outcome,
        last_ok: last_ok.clone(),
    };
    (report, wait, last_ok)
}

/// One [`fetch_with_retry_after`]-shaped answer: the fetch's own `Result`,
/// plus whatever `Retry-After` it read off the wire (if any).
type FetchOutcome = (Result<Usage, UsageError>, Option<Duration>);

/// [`poll_forever`]'s actual loop, pulled out so a test can drive it against
/// a scripted `fetch_once` under a virtual clock instead of a real network
/// and a real `sleep` (#1285's review, MED — "`poll_forever`'s wiring is
/// pinned by nothing; only `advance`/`next_wait` are").
///
/// The fetcher is injected rather than hard-coded so the two shapes that
/// produce a [`FetchOutcome`] — a real `spawn_blocking` HTTP round-trip
/// ([`poll_forever`]) and a scripted in-memory sequence (the `poll_loop`
/// tests) — are interchangeable here; this function itself never knows
/// which one it was handed. Every step below routes through something a
/// test can observe from the outside — `fetch_once` (the network),
/// [`advance`] (the book-keeping), [`publish`] (the board), and
/// `tokio::time::sleep` (the schedule) — so a mutation that discards the
/// computed `wait` or the carried `last_ok` breaks the loop itself, not just
/// the pure `advance`/`next_wait` unit tests that call those functions
/// directly and would never notice `poll_forever` stopped wiring them
/// together correctly.
async fn poll_loop<F, Fut>(mut fetch_once: F) -> !
where
    F: FnMut() -> Fut,
    Fut: Future<Output = FetchOutcome>,
{
    let mut wait = POLL_EVERY;
    let mut last_ok: Option<(i64, Usage)> = None;
    loop {
        let (result, retry_after) = fetch_once().await;
        let (report, next_wait_value, next_last_ok) =
            advance(wait, last_ok, now_unix(), result, retry_after);
        wait = next_wait_value;
        last_ok = next_last_ok;
        if let Outcome::Failed(ref e) = report.outcome {
            // The sentence, never the cause verbatim and never the token.
            // `report.at` is "now" as far as this attempt is concerned — the
            // log line is emitted the instant the report is minted.
            tracing::debug!(reason = %e.sentence(report.at), "usage poll produced no numbers");
        }
        publish(report);
        tokio::time::sleep(wait).await;
    }
}

/// Poll forever: one fetch immediately, then one every [`POLL_EVERY`] —
/// longer when [`next_wait`] has backed off a 429.
///
/// Never returns. Spawned by `main` on the HTTP runtime — deliberately *not*
/// from the plugin SDK's session, so the numbers keep arriving while the shell
/// is down and the chip's dial/backoff is running. A thin wrapper over
/// [`poll_loop`]: the only thing this adds is the real fetcher —
/// `spawn_blocking`ing [`fetch_with_retry_after`], with a task-join failure
/// mapped to the same [`UsageError::Io`] shape a transport error would be.
pub async fn poll_forever(base_url: String, credentials: PathBuf) -> ! {
    poll_loop(move || {
        let (base, creds) = (base_url.clone(), credentials.clone());
        async move {
            match tokio::task::spawn_blocking(move || fetch_with_retry_after(&base, &creds)).await {
                Ok(pair) => pair,
                Err(e) => (
                    Err(UsageError::Io(truncate(&format!(
                        "the usage poll task did not finish: {e}"
                    )))),
                    None,
                ),
            }
        }
    })
    .await
}

// ── Time, without a calendar crate ───────────────────────────────────────────
//
// `resets_at` is RFC 3339 and the panel wants both an absolute time and a
// "resets in 2 h 15 min". That is two conversions — civil→epoch and back — and
// they are twenty lines of Howard Hinnant's days-from-civil algorithm. A
// `chrono`/`time` dependency would be the obvious alternative and is rejected on
// a hard constraint: it would add `Cargo.lock` entries to a crate whose whole
// dependency argument (see `Cargo.toml`) is that it adds none. Everything here
// is **UTC** — a local rendering would need a timezone database, which is a
// second dependency for a line that reads fine as `13:50 UTC`.

/// Seconds since the Unix epoch, or `0` before it (which cannot happen on a
/// machine whose clock is set, and renders as an absurd date rather than
/// panicking if it does).
#[must_use]
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date (Hinnant).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`] — `(year, month, day)`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Parse two ASCII digits at `bytes[at..at + 2]`.
fn two_digits(bytes: &[u8], at: usize) -> Option<i64> {
    let pair = bytes.get(at..at + 2)?;
    if !pair.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(i64::from(pair[0] - b'0') * 10 + i64::from(pair[1] - b'0'))
}

/// Parse an RFC 3339 timestamp to Unix seconds, or `None` if it is not one.
///
/// Handles what the endpoint actually sends
/// (`2026-09-13T13:50:00.101848+00:00`) plus the usual spellings: `Z`, a
/// `±HH:MM` or `±HHMM` offset, an absent offset (read as UTC), a space instead
/// of `T`, and any number of fractional-second digits (dropped — the panel
/// renders minutes).
#[must_use]
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.trim().as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let digits4 = bytes.get(0..4)?;
    if !digits4.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year = digits4
        .iter()
        .fold(0_i64, |acc, d| acc * 10 + i64::from(d - b'0'));
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let month = two_digits(bytes, 5)?;
    let day = two_digits(bytes, 8)?;
    if !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let hour = two_digits(bytes, 11)?;
    let minute = two_digits(bytes, 14)?;
    let second = two_digits(bytes, 17)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        // Leap seconds are a real RFC 3339 spelling; they land on :59 here.
        || second > 60
    {
        return None;
    }

    // Skip the fractional part, if any.
    let mut rest = &bytes[19..];
    if rest.first() == Some(&b'.') {
        let digits = rest[1..].iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        rest = &rest[1 + digits..];
    }

    let offset = match rest.first() {
        None => 0,
        Some(b'Z' | b'z') if rest.len() == 1 => 0,
        Some(sign @ (b'+' | b'-')) => {
            let sign = if *sign == b'-' { -1 } else { 1 };
            let hours = two_digits(rest, 1)?;
            let mins = match rest.len() {
                // `+HHMM`
                5 => two_digits(rest, 3)?,
                // `+HH:MM`
                6 if rest[3] == b':' => two_digits(rest, 4)?,
                _ => return None,
            };
            if hours > 23 || mins > 59 {
                return None;
            }
            sign * (hours * 3600 + mins * 60)
        }
        Some(_) => return None,
    };

    let secs = second.min(59);
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + secs - offset)
}

/// Parse a `Retry-After` header value (#1283): either delta-seconds (`"120"`,
/// RFC 7231 §7.1.3's first spelling) or an HTTP-date (the second — always the
/// IMF-fixdate form in practice, e.g. `Wed, 21 Oct 2015 07:28:00 GMT`, which
/// is the only one [`parse_http_date`] reads). `now` anchors the date arm,
/// which names a wall-clock deadline rather than a duration; a deadline
/// already in the past reads as a zero wait, not a negative one.
fn parse_retry_after(value: &str, now: i64) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = parse_http_date(value)?;
    let delta = at.saturating_sub(now).max(0);
    Some(Duration::from_secs(
        u64::try_from(delta).unwrap_or(u64::MAX),
    ))
}

/// Parse RFC 7231 §7.1.1.1's IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) —
/// the only date form a `Retry-After` header carries in practice, and the
/// only one this reads. Always GMT by construction, so there is no offset to
/// apply; reuses [`days_from_civil`], the same table [`parse_rfc3339`]
/// already carries for reset times, which is what keeps this cheap enough not
/// to need a date crate.
///
/// The weekday token is checked against the seven IMF abbreviations (a
/// non-word there is the cheapest sign this is not really a fixdate at all —
/// #1285's review, NIT); **day-for-month is not** (`31 Feb` parses) — the
/// result only ever feeds [`parse_retry_after`], which clamps into
/// `[POLL_EVERY, MAX_BACKOFF]` regardless, so a calendar-invalid date is
/// harmless here in a way an unrecognisable header shape is not worth
/// rejecting either.
fn parse_http_date(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 29 || &bytes[26..29] != b"GMT" {
        return None;
    }
    if !matches!(
        &bytes[0..3],
        b"Mon" | b"Tue" | b"Wed" | b"Thu" | b"Fri" | b"Sat" | b"Sun"
    ) {
        return None;
    }
    if bytes[3] != b','
        || bytes[4] != b' '
        || bytes[7] != b' '
        || bytes[11] != b' '
        || bytes[16] != b' '
        || bytes[19] != b':'
        || bytes[22] != b':'
        || bytes[25] != b' '
    {
        return None;
    }
    let day = two_digits(bytes, 5)?;
    let month: i64 = match &bytes[8..11] {
        b"Jan" => 1,
        b"Feb" => 2,
        b"Mar" => 3,
        b"Apr" => 4,
        b"May" => 5,
        b"Jun" => 6,
        b"Jul" => 7,
        b"Aug" => 8,
        b"Sep" => 9,
        b"Oct" => 10,
        b"Nov" => 11,
        b"Dec" => 12,
        _ => return None,
    };
    let digits4 = bytes.get(12..16)?;
    if !digits4.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year = digits4
        .iter()
        .fold(0_i64, |acc, d| acc * 10 + i64::from(d - b'0'));
    let hour = two_digits(bytes, 17)?;
    let minute = two_digits(bytes, 20)?;
    let second = two_digits(bytes, 23)?;
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second.min(59))
}

/// `2026-09-13 13:50 UTC` — the absolute half of a reset time.
#[must_use]
pub fn format_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute) = (secs / 3600, (secs % 3600) / 60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

/// `in 2 h 15 min` — the relative half. Coarsening as the distance grows, so the
/// string stays short enough for a chip tooltip at every scale.
#[must_use]
pub fn humanise_until(now: i64, then: i64) -> String {
    let delta = then - now;
    if delta <= 0 {
        return "any moment now".to_owned();
    }
    if delta < 60 {
        return "in under a minute".to_owned();
    }
    if delta < 3600 {
        return format!("in {} min", delta / 60);
    }
    if delta < 86_400 {
        let (hours, mins) = (delta / 3600, (delta % 3600) / 60);
        return if mins == 0 {
            format!("in {hours} h")
        } else {
            format!("in {hours} h {mins} min")
        };
    }
    let (days, hours) = (delta / 86_400, (delta % 86_400) / 3600);
    if hours == 0 {
        format!("in {days} d")
    } else {
        format!("in {days} d {hours} h")
    }
}

/// `2 min ago` — how long ago the last poll ran.
#[must_use]
pub fn humanise_since(now: i64, then: i64) -> String {
    let delta = now - then;
    if delta < 60 {
        return "just now".to_owned();
    }
    if delta < 3600 {
        return format!("{} min ago", delta / 60);
    }
    if delta < 86_400 {
        return format!("{} h ago", delta / 3600);
    }
    format!("{} d ago", delta / 86_400)
}

/// Both halves of a reset time, or `None` when the row carried no parseable
/// `resets_at`: `resets 2026-09-13 13:50 UTC · in 2 h 15 min`.
#[must_use]
pub fn reset_phrase(now: i64, resets_at: Option<&str>) -> Option<String> {
    let epoch = parse_rfc3339(resets_at?)?;
    Some(format!(
        "resets {} · {}",
        format_utc(epoch),
        humanise_until(now, epoch)
    ))
}

/// The short reset phrase for a chip tooltip — relative only.
#[must_use]
pub fn reset_short(now: i64, resets_at: Option<&str>) -> Option<String> {
    parse_rfc3339(resets_at?).map(|epoch| humanise_until(now, epoch))
}

// ── Labels ───────────────────────────────────────────────────────────────────

/// Turn a server-chosen `kind` into a label, **without** a table of known kinds.
///
/// The rule is positional, not a lookup: the first `_`-separated segment names
/// the window (`session` gets its documented length spelled out, `weekly` is
/// already a word), and whatever follows rides in parentheses. Anything whose
/// head this does not recognise falls through to the raw kind with its
/// underscores spaced and its first letter raised — so a bucket Anthropic adds
/// tomorrow renders under its own name instead of vanishing. That is Mara's
/// "render whatever the server sends back", applied to the label as well as to
/// the list.
#[must_use]
pub fn humanise_kind(kind: &str) -> String {
    let kind = kind.trim();
    if kind.is_empty() {
        return "Limit".to_owned();
    }
    let (head, tail) = kind.split_once('_').unwrap_or((kind, ""));
    let window = match head {
        // The 5-hour rolling window; its length is the thing people forget.
        "session" => Some("Session (5 h)"),
        "weekly" => Some("Weekly"),
        "monthly" => Some("Monthly"),
        _ => None,
    };
    match (window, tail) {
        (Some(window), "") => window.to_owned(),
        (Some(window), tail) => format!("{window} ({})", tail.replace('_', " ")),
        (None, _) => capitalise(&kind.replace('_', " ")),
    }
}

/// Raise the first character of `text`.
fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().chain(chars).collect(),
    }
}

/// `80%` — a percentage, rounded to whole units because that is the resolution
/// the server reports in and a fractional quota percent means nothing to anyone.
#[must_use]
pub fn percent_label(percent: f64) -> String {
    if !percent.is_finite() {
        return "—".to_owned();
    }
    format!("{}%", percent.clamp(0.0, 100.0).round())
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_BASE_URL, ExtraUsage, Limit, MAX_BACKOFF, Outcome, POLL_EVERY, Report,
        SEVERITY_NORMAL, STALE_AFTER, Usage, UsageError, advance, credentials_path_in, fetch,
        fetch_with_retry_after, format_utc, humanise_kind, humanise_since, humanise_until,
        next_wait, parse_http_date, parse_retry_after, parse_rfc3339, percent_label, poll_loop,
        reset_phrase, reset_short, scrub, truncate,
    };
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    /// The **captured response** (`tests/fixtures/usage_response.json`) — the
    /// real body this endpoint returned on 2026-09-13, with the numbers rounded
    /// and the timestamps kept, so the fixture is a shape rather than a
    /// disclosure. Every codename-keyed field the live response carried is
    /// present, because "the junk is tolerated" is one of the things it pins.
    const CAPTURED: &str = include_str!("../tests/fixtures/usage_response.json");

    /// Every test that binds an ephemeral port takes this for its whole body —
    /// `TcpListener::bind("127.0.0.1:0")` hands out whatever the kernel thinks
    /// is free, and cargo runs these as parallel threads in one process, so a
    /// port one test just released can be recycled into another's listener
    /// mid-flight (`hytte-ai-providers`' `TEST_SOCKETS`, #716). Unwrap through a
    /// poison so one panicking test does not cascade.
    static TEST_SOCKETS: Mutex<()> = Mutex::new(());

    /// Every test that publishes onto the process-global [`BOARD`] takes this
    /// for its whole body — `publishing_a_report_bumps_the_version` and the
    /// `poll_loop` wiring test below both do, and cargo runs tests in
    /// parallel threads in one process, so one test's `publish` could
    /// otherwise land between another's `publish` and its own `latest()`
    /// read. Same shape as [`TEST_SOCKETS`], for the same reason.
    static BOARD_TESTS: Mutex<()> = Mutex::new(());

    /// A token no real endpoint would issue, long enough that an accidental
    /// substring match is not the reason a test passes.
    const FAKE_TOKEN: &str = "sk-ant-oat01-TESTONLY-0123456789abcdefghijklmnopqrstuvwxyz";

    /// `value == 0.0`, spelled the way `clippy::float_cmp` accepts. Every zero
    /// asserted here is a *constructed* zero (`f64::default()`, or the clamp's
    /// output), so an exact test is the right one — the lint is about accidental
    /// equality on computed floats, not about this.
    fn is_zero(value: f64) -> bool {
        value.abs() < f64::EPSILON
    }

    /// Write a credentials file shaped like Claude Code's into `dir`.
    fn creds(dir: &Path, token: &str) -> PathBuf {
        let path = dir.join(".credentials.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"claudeAiOauth":{{"accessToken":"{token}","refreshToken":"sk-ant-ort01-NEVER-READ","expiresAt":1794000000,"scopes":["user:profile","user:inference"],"subscriptionType":"max"}}}}"#
            ),
        )
        .expect("write credentials");
        path
    }

    /// Read one HTTP request off `sock` (headers only — the poll sends no body).
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

    /// A one-shot fake usage endpoint: captures the request, answers `status`
    /// with `body`. Returns `(base_url, handle → the raw request)`.
    fn fake_endpoint(
        status: &'static str,
        body: String,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let raw = capture_request(&mut sock);
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len(),
            );
            sock.write_all(resp.as_bytes()).expect("write response");
            raw
        });
        (format!("http://{addr}"), handle)
    }

    /// Like [`fake_endpoint`], but the response also carries `extra_headers`
    /// — exists for the one test that needs a `Retry-After` on the wire.
    fn fake_endpoint_with_headers(
        status: &'static str,
        extra_headers: &[(&str, &str)],
        body: String,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let extra = extra_headers.iter().fold(String::new(), |mut acc, (k, v)| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{k}: {v}\r\n");
            acc
        });
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let raw = capture_request(&mut sock);
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{extra}\r\n{body}",
                body.len(),
            );
            sock.write_all(resp.as_bytes()).expect("write response");
            raw
        });
        (format!("http://{addr}"), handle)
    }

    // ── (a) The captured response ────────────────────────────────────────────

    /// **The wire contract, as measured.** The literal body the endpoint
    /// returned parses to exactly the rows and the overflow allowance, with the
    /// integer `percent` widening, the `+00:00` timestamps kept verbatim for
    /// [`parse_rfc3339`], and the `scope` object — which this crate deliberately
    /// does not model — costing nothing.
    #[test]
    fn the_captured_response_parses_to_its_limits() {
        let usage: Usage = serde_json::from_str(CAPTURED).expect("the captured body parses");
        assert_eq!(
            usage,
            Usage {
                limits: vec![
                    Limit {
                        kind: "session".to_owned(),
                        group: Some("session".to_owned()),
                        percent: 80.0,
                        severity_raw: Some("warning".to_owned()),
                        resets_at: Some("2026-09-13T13:50:00.101848+00:00".to_owned()),
                        is_active: Some(true),
                    },
                    Limit {
                        kind: "weekly_all".to_owned(),
                        group: Some("weekly".to_owned()),
                        percent: 40.0,
                        severity_raw: Some("normal".to_owned()),
                        resets_at: Some("2026-09-17T15:00:00.101872+00:00".to_owned()),
                        is_active: Some(true),
                    },
                    Limit {
                        kind: "weekly_scoped".to_owned(),
                        group: Some("weekly".to_owned()),
                        percent: 25.0,
                        severity_raw: Some("normal".to_owned()),
                        resets_at: Some("2026-09-17T15:00:00.102097+00:00".to_owned()),
                        is_active: Some(false),
                    },
                ],
                extra_usage: Some(ExtraUsage {
                    is_enabled: false,
                    utilization: None,
                    used_credits: None,
                    monthly_limit: None,
                    currency: None,
                }),
            }
        );
    }

    // ── (b) Tolerance ────────────────────────────────────────────────────────

    /// **The churn rule.** Eight codename-keyed top-level fields
    /// (`amber_ladder`, `nimbus_quill`, …), the named `five_hour`/`seven_day`
    /// objects this crate does not read, a `seven_day_breakdown` with a nested
    /// array, and a `scope` object inside a row — every one of them is ignored,
    /// and an unknown `kind` and an unknown `severity` still render.
    ///
    /// Falsify by adding `#[serde(deny_unknown_fields)]` to `Usage` or `Limit`:
    /// this goes red immediately, and so does
    /// `the_captured_response_parses_to_its_limits`.
    #[test]
    fn unknown_fields_and_unknown_kinds_are_tolerated() {
        let body = r#"{
            "limits": [
                {"kind":"quarterly_moonshot","percent":7,"severity":"spicy","is_active":true,
                 "resets_at":"2026-12-01T00:00:00Z","scope":{"model":{"id":null}},"rumour":42},
                {"kind":"session","percent":3}
            ],
            "extra_usage": {"is_enabled": true, "utilization": 12.5, "surprise": "hi"},
            "amber_ladder": null, "nimbus_quill": {"utilization": 0.0},
            "five_hour": {"utilization": 7.0}, "member_dashboard_available": false
        }"#;
        let usage: Usage = serde_json::from_str(body).expect("unknown fields are not fatal");
        assert_eq!(usage.limits.len(), 2);

        let exotic = &usage.limits[0];
        assert_eq!(exotic.kind, "quarterly_moonshot");
        assert_eq!(
            humanise_kind(&exotic.kind),
            "Quarterly moonshot",
            "an unrecognised kind renders under its own name"
        );
        assert_eq!(
            exotic.severity(),
            "spicy",
            "the raw word survives; the class mapping is what defaults it"
        );

        // The second row omits `severity`, `group`, `resets_at` and `is_active`
        // entirely — all four have to degrade rather than fail the document.
        let sparse = &usage.limits[1];
        assert_eq!(sparse.severity(), SEVERITY_NORMAL);
        assert_eq!(sparse.group, None);
        assert_eq!(sparse.resets_at, None);
        assert!(sparse.active(), "a row with no flag is still a row");

        let extra = usage.extra_usage.expect("extra_usage parsed");
        assert!(extra.is_enabled);
        assert_eq!(extra.utilization, Some(12.5));
    }

    /// An explicit `null` is as tolerable as an absent key — the endpoint nulls
    /// fields freely, and a non-`Option` field would otherwise hard-fail the
    /// whole document on one of them.
    #[test]
    fn explicit_nulls_degrade_rather_than_failing_the_document() {
        let usage: Usage = serde_json::from_str(
            r#"{"limits":[{"kind":null,"percent":null,"severity":null,"group":null,
                "resets_at":null,"is_active":null}],"extra_usage":null}"#,
        )
        .expect("nulls are not fatal");
        let row = &usage.limits[0];
        assert_eq!(row.kind, "");
        assert!(is_zero(row.percent), "a null percent reads as rest");
        assert_eq!(row.severity(), SEVERITY_NORMAL);
        assert!(row.active());
        assert_eq!(
            humanise_kind(&row.kind),
            "Limit",
            "a nameless row still draws"
        );
        assert_eq!(usage.extra_usage, None);
    }

    /// A response with no `limits` at all is a legal, empty readout — not an
    /// error. (The panel then says the account reported none.)
    #[test]
    fn a_response_with_no_limits_is_still_a_usage() {
        let usage: Usage = serde_json::from_str("{}").expect("an empty object parses");
        assert!(usage.limits.is_empty());
        assert_eq!(usage.extra_usage, None);
    }

    /// `percent` arrives as a JSON **integer** on the real wire and as a float
    /// in the named objects; both widen, and the derived fraction is clamped.
    #[test]
    fn percent_tolerates_an_integer_and_clamps_its_fraction() {
        let usage: Usage = serde_json::from_str(
            r#"{"limits":[{"kind":"a","percent":88},{"kind":"b","percent":88.5},
                {"kind":"c","percent":-3},{"kind":"d","percent":420}]}"#,
        )
        .expect("parses");
        let fractions: Vec<f64> = usage.limits.iter().map(Limit::fraction).collect();
        assert_eq!(fractions, vec![0.88, 0.885, 0.0, 1.0]);
        assert_eq!(percent_label(88.0), "88%");
        assert_eq!(percent_label(88.5), "89%");
        assert_eq!(percent_label(f64::NAN), "—");
        assert!(
            is_zero(
                Limit {
                    percent: f64::NAN,
                    ..Limit::default()
                }
                .fraction()
            ),
            "a NaN reading must not reach a meter — it would defeat render dedup"
        );
    }

    // ── (c) 401 ──────────────────────────────────────────────────────────────

    /// **A 401 is `Unauthorized`, and its sentence carries no token bytes.**
    ///
    /// The whole point of the arm: the token expired, `claude` refreshes it, we
    /// say so and touch nothing. The error text is checked against the token
    /// itself *and* against a couple of its substrings, because "contains no
    /// token" is the property, not "is not equal to the token".
    ///
    /// Falsify by putting the token in the arm (e.g.
    /// `UsageError::Io(format!("401 for {token}"))` in `fetch`): red here.
    #[test]
    fn a_401_is_unauthorized_and_its_sentence_carries_no_token() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = creds(dir.path(), FAKE_TOKEN);
        let (base, handle) = fake_endpoint(
            "401 Unauthorized",
            r#"{"error":{"type":"authentication_error","message":"expired"}}"#.to_owned(),
        );

        let err = fetch(&base, &path).expect_err("a 401 is an error");
        assert_eq!(err, UsageError::Unauthorized);
        // `now` is irrelevant to every arm but `RateLimited` — `0` here.
        let sentence = err.sentence(0);
        assert_eq!(
            sentence,
            "usage stale — run `claude` once to refresh the login"
        );
        assert!(!sentence.contains(FAKE_TOKEN));
        assert!(!sentence.contains("sk-ant"));
        assert!(!sentence.contains("TESTONLY"));
        assert!(!format!("{err:?}").contains("sk-ant"), "nor its Debug");

        // …and the request really did send the bearer we never printed.
        let raw = handle.join().expect("server thread");
        assert!(
            raw.contains(&format!("authorization: Bearer {FAKE_TOKEN}"))
                || raw.contains(&format!("Authorization: Bearer {FAKE_TOKEN}")),
            "the poll must actually authenticate: {raw}"
        );
        assert!(
            raw.contains("GET /api/oauth/usage "),
            "the measured path, verbatim: {raw}"
        );
        assert!(
            raw.to_ascii_lowercase().contains("hytte-claude-bridge/"),
            "and identify itself: {raw}"
        );
        assert!(
            !raw.to_ascii_lowercase().contains("anthropic-beta"),
            "measured: no beta header is needed, so none is sent"
        );
    }

    /// 403 joins 401 on the same arm; every other non-2xx keeps its number.
    #[test]
    fn a_403_is_unauthorized_and_a_500_keeps_its_status() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = creds(dir.path(), FAKE_TOKEN);

        let (base, handle) = fake_endpoint("403 Forbidden", "{}".to_owned());
        assert_eq!(
            fetch(&base, &path).expect_err("403"),
            UsageError::Unauthorized
        );
        drop(handle.join());

        let (base, handle) = fake_endpoint("500 Internal Server Error", "nope".to_owned());
        let err = fetch(&base, &path).expect_err("500");
        assert_eq!(err, UsageError::Http(500));
        assert!(err.sentence(0).contains("HTTP 500"));
        drop(handle.join());
    }

    // ── (c2) 429 and `Retry-After` (#1283) ───────────────────────────────────

    /// **A 429 is a plain `Http(429)` from [`fetch`] itself** — the schedule
    /// lives in `poll_forever`/[`advance`], not here — and its own
    /// `Retry-After` header is read off the wire and handed back alongside.
    #[test]
    fn a_429s_retry_after_header_is_read_as_delta_seconds() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = creds(dir.path(), FAKE_TOKEN);

        let (base, handle) = fake_endpoint_with_headers(
            "429 Too Many Requests",
            &[("retry-after", "120")],
            "{}".to_owned(),
        );
        let (result, retry_after) = fetch_with_retry_after(&base, &path);
        assert_eq!(result.expect_err("429"), UsageError::Http(429));
        assert_eq!(retry_after, Some(Duration::from_mins(2)));
        drop(handle.join());

        // `fetch` itself — the public entry point every other test in this
        // file uses — sees exactly the same status, just without the header.
        let (base, handle) = fake_endpoint("429 Too Many Requests", "{}".to_owned());
        assert_eq!(fetch(&base, &path).expect_err("429"), UsageError::Http(429));
        drop(handle.join());
    }

    /// No `Retry-After` header at all ⇒ `None`, not a default guess — the
    /// schedule (`next_wait`) is what picks a number when the server didn't.
    #[test]
    fn a_429_with_no_retry_after_header_reports_none() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = creds(dir.path(), FAKE_TOKEN);
        let (base, handle) = fake_endpoint("429 Too Many Requests", "{}".to_owned());
        let (result, retry_after) = fetch_with_retry_after(&base, &path);
        assert_eq!(result.expect_err("429"), UsageError::Http(429));
        assert_eq!(retry_after, None);
        drop(handle.join());
    }

    /// Delta-seconds, whitespace, garbage, and the IMF-fixdate form — the only
    /// date spelling read (module docs on [`parse_http_date`]).
    #[test]
    fn retry_after_parses_delta_seconds_and_the_http_date_form() {
        assert_eq!(
            parse_retry_after("120", 1_000),
            Some(Duration::from_mins(2))
        );
        assert_eq!(
            parse_retry_after(" 45 ", 1_000),
            Some(Duration::from_secs(45)),
            "whitespace is tolerated"
        );
        assert_eq!(parse_retry_after("not a number", 1_000), None);

        // RFC 7231 §7.1.1.1's own worked example.
        let epoch = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").expect("parses");
        assert_eq!(format_utc(epoch), "1994-11-06 08:49 UTC");
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", epoch - 30),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", epoch + 10),
            Some(Duration::ZERO),
            "a deadline already in the past is a zero wait, not an underflow"
        );
        assert_eq!(parse_http_date("garbage"), None);
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 UTC"),
            None,
            "the header's date form is always GMT; anything else is unrecognised"
        );
    }

    /// **#1285's review, NIT — a weekday token outside the seven IMF
    /// abbreviations is rejected; day-for-month stays deliberately
    /// unchecked** (the function doc says why: the result only ever feeds a
    /// clamp, so a calendar-invalid date is harmless in a way an
    /// unrecognisable header shape is not worth rejecting either).
    ///
    /// Falsify the weekday half by deleting the `matches!` guard added for
    /// this: `parse_http_date("Xyz, 06 Nov 1994 08:49:37 GMT")` starts
    /// parsing again and the first assertion goes red.
    #[test]
    fn parse_http_date_rejects_a_bad_weekday_but_not_a_bad_day_for_month() {
        assert_eq!(
            parse_http_date("Xyz, 06 Nov 1994 08:49:37 GMT"),
            None,
            "not one of the seven weekday abbreviations"
        );
        assert!(
            parse_http_date("Sun, 31 Feb 1994 08:49:37 GMT").is_some(),
            "day-for-month is deliberately unchecked — see the function doc"
        );
        // Every real weekday abbreviation is accepted, regardless of whether
        // it is the one that date actually fell on — this reads the date
        // string, not the calendar.
        for day in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"] {
            assert!(
                parse_http_date(&format!("{day}, 06 Nov 1994 08:49:37 GMT")).is_some(),
                "{day}"
            );
        }
    }

    /// The 429 backoff schedule, as a table: doubling up to the cap, a
    /// success resetting it, `Retry-After` overriding the doubling within the
    /// `[POLL_EVERY, MAX_BACKOFF]` band, and — the point of the whole
    /// exercise — every *other* outcome costing nothing.
    ///
    /// `MAX_BACKOFF` is 15 min (`== STALE_AFTER`, since #1285's review —
    /// see its doc), so doubling from `POLL_EVERY` (5 min) reaches the cap
    /// on the *second* step, not the third: `5 → 10 → 15 → 15`.
    ///
    /// Falsify by having `next_wait` ignore the 429 arm (always return
    /// `POLL_EVERY`): the chain assertion goes red immediately.
    #[test]
    fn the_429_backoff_chain_doubles_to_the_cap_and_resets_on_success() {
        let http_429 = Outcome::Failed(UsageError::Http(429));
        let mut wait = POLL_EVERY;
        let mut minutes = Vec::new();
        for _ in 0..4 {
            wait = next_wait(wait, &http_429, None);
            minutes.push(wait.as_secs() / 60);
        }
        assert_eq!(minutes, vec![10, 15, 15, 15], "5 → 10 → 15 → 15 → 15");

        assert_eq!(
            next_wait(wait, &Outcome::Ok(Usage::default()), None),
            POLL_EVERY,
            "a success resets the schedule"
        );
    }

    #[test]
    fn retry_after_is_honoured_and_clamped_to_the_poll_every_max_backoff_band() {
        let http_429 = Outcome::Failed(UsageError::Http(429));
        assert_eq!(
            next_wait(POLL_EVERY, &http_429, Some(Duration::from_mins(12))),
            Duration::from_mins(12),
            "720 s → 12 min, inside the band"
        );
        assert_eq!(
            next_wait(POLL_EVERY, &http_429, Some(Duration::from_secs(10))),
            POLL_EVERY,
            "10 s is below the floor — clamped up to 5 min"
        );
        assert_eq!(
            next_wait(POLL_EVERY, &http_429, Some(Duration::from_mins(90))),
            MAX_BACKOFF,
            "an absurdly long Retry-After is clamped down to the cap"
        );
    }

    /// **Only a 429 backs off** — a 500, an expired login, a network blip, or
    /// success all answer `POLL_EVERY`, whatever the schedule was mid-chain.
    #[test]
    fn only_a_429_backs_off_every_other_outcome_is_the_ordinary_cadence() {
        let mid_chain = Duration::from_mins(20);
        assert_eq!(
            next_wait(mid_chain, &Outcome::Ok(Usage::default()), None),
            POLL_EVERY
        );
        assert_eq!(
            next_wait(mid_chain, &Outcome::Failed(UsageError::Http(500)), None),
            POLL_EVERY,
            "a 500 does NOT back off — only 429"
        );
        assert_eq!(
            next_wait(mid_chain, &Outcome::Failed(UsageError::Unauthorized), None),
            POLL_EVERY
        );
    }

    /// **The countdown, at render time — item 11's NIT.** `RateLimited`
    /// carries a deadline, not a wait, so the sentence must recompute the
    /// remaining minutes against whatever `now` it is rendered with, and
    /// clamp to "any moment now" rather than ever naming a negative wait.
    ///
    /// Falsify by rendering the sentence with the deadline itself as `now`
    /// (i.e. freezing the number at publish time, the pre-#1285-fix shape):
    /// the second and third assertions go red.
    #[test]
    fn a_rate_limited_sentence_counts_down_to_the_deadline() {
        let published = 1_000;
        assert_eq!(
            UsageError::RateLimited(published + 600).sentence(published),
            "usage rate-limited — next try in 10 min"
        );
        assert_eq!(
            UsageError::RateLimited(published + 90).sentence(published),
            "usage rate-limited — next try in 2 min",
            "a fractional minute rounds UP — never tell the reader to retry \
             before the wait the schedule chose is actually over"
        );
        assert_eq!(
            UsageError::RateLimited(published + 600).sentence(published + 570),
            "usage rate-limited — next try in 1 min",
            "the same report reads a lower number as the chip keeps ticking"
        );
        assert_eq!(
            UsageError::RateLimited(published + 600).sentence(published + 600),
            "usage rate-limited — next try any moment now",
            "exactly at the deadline"
        );
        assert_eq!(
            UsageError::RateLimited(published + 600).sentence(published + 601),
            "usage rate-limited — next try any moment now",
            "past the deadline — never a negative countdown"
        );
    }

    // ── #1283: carrying `last_ok` across a failure ───────────────────────────

    fn some_usage() -> Usage {
        Usage {
            limits: vec![Limit {
                kind: "session".to_owned(),
                percent: 50.0,
                ..Limit::default()
            }],
            extra_usage: None,
        }
    }

    /// **Item 1(a): a failed poll after a success keeps the meters up.**
    /// `advance` is `poll_forever`'s one place that decides what to publish;
    /// this drives it directly, with no event loop.
    ///
    /// Falsify by publishing `last_ok: None` unconditionally in `advance`
    /// (the pre-#1283 "replacing" publish): the second assertion goes red.
    #[test]
    fn a_failed_poll_after_a_success_keeps_last_ok_on_the_report() {
        let usage = some_usage();
        let (ok_report, wait, last_ok) = advance(POLL_EVERY, None, 1_000, Ok(usage.clone()), None);
        assert_eq!(ok_report.usage(), Some(&usage));
        assert_eq!(wait, POLL_EVERY);

        let (failed_report, wait, last_ok) =
            advance(wait, last_ok, 1_300, Err(UsageError::Http(429)), None);
        assert_eq!(
            failed_report.usage(),
            Some(&usage),
            "the last-known-good numbers must still be there"
        );
        assert!(
            matches!(
                failed_report.outcome,
                Outcome::Failed(UsageError::RateLimited(_))
            ),
            "a 429 is promoted once `advance` knows the wait: {:?}",
            failed_report.outcome
        );
        assert_eq!(
            failed_report.error(failed_report.at).as_deref(),
            Some("usage rate-limited — next try in 10 min"),
            "doubled from the POLL_EVERY the first call answered — rendered \
             the instant the report was minted, so the countdown reads the \
             full wait"
        );
        assert_eq!(wait, Duration::from_mins(10));
        assert_eq!(last_ok, Some((1_000, usage)));
    }

    /// **#1285's review, LOW — a non-429 failure resets a 429 chain straight
    /// back to [`POLL_EVERY`], and `last_ok` survives every step of it.**
    /// [`a_failed_poll_after_a_success_keeps_last_ok_on_the_report`] only
    /// carries `last_ok` through ONE failure; this drives it through two
    /// 429s and a non-429 on top — the case no other shipped test covers.
    ///
    /// Falsify the reset half by having `next_wait` "remember" a 429 chain
    /// through an unrelated failure (e.g. resetting only on `Outcome::Ok`):
    /// the final `wait4` assertion goes red (it would still read the
    /// mid-chain cap instead of dropping to `POLL_EVERY`). Falsify the
    /// carry-forward half the same way that test's own doc does.
    #[test]
    fn a_non_429_failure_resets_a_429_chain_but_last_ok_survives_every_step() {
        let usage = some_usage();
        let (_, wait, last_ok) = advance(POLL_EVERY, None, 1_000, Ok(usage.clone()), None);

        let (r1, wait, last_ok) = advance(wait, last_ok, 1_300, Err(UsageError::Http(429)), None);
        assert_eq!(r1.usage(), Some(&usage), "last_ok through the first 429");

        let (r2, wait, last_ok) = advance(wait, last_ok, 1_600, Err(UsageError::Http(429)), None);
        assert_eq!(
            wait, MAX_BACKOFF,
            "5 → 10 → the cap (MAX_BACKOFF == 15 min)"
        );
        assert_eq!(r2.usage(), Some(&usage), "last_ok through the second 429");

        let (r3, wait4, last_ok) = advance(
            wait,
            last_ok,
            1_900,
            Err(UsageError::Io("boom".to_owned())),
            None,
        );
        assert_eq!(
            wait4, POLL_EVERY,
            "a chain at the cap drops straight back to 5 on an unrelated failure"
        );
        assert_eq!(r3.usage(), Some(&usage), "last_ok through the non-429 too");
        assert_eq!(
            last_ok,
            Some((1_000, usage)),
            "still the original success, three failures later"
        );
    }

    /// **Item 1(c): a first poll that fails has nothing to carry, and renders
    /// exactly as it did before #1283.**
    #[test]
    fn a_first_poll_failure_has_no_last_ok_and_is_unchanged() {
        let (report, wait, last_ok) =
            advance(POLL_EVERY, None, 1_000, Err(UsageError::Unauthorized), None);
        assert_eq!(report.usage(), None);
        assert_eq!(
            report.error(1_000).as_deref(),
            Some("usage stale — run `claude` once to refresh the login")
        );
        assert!(!report.is_stale(1_000), "nothing to be stale about yet");
        assert_eq!(wait, POLL_EVERY, "only a 429 backs off");
        assert_eq!(last_ok, None);
    }

    /// **Item 1(b): staleness is judged by `last_ok`'s clock, not the
    /// failure's own `at`.** A success at `t=0`, then a failure a minute
    /// later: the failure is fresh, but the numbers it is still showing are
    /// not, and 15 minutes after `t=0` — [`STALE_AFTER`] — they go stale even
    /// though the *failure* itself is only 14 minutes old.
    ///
    /// Falsify by judging staleness off `Report::at` instead of
    /// `Report::numbers_at`: at `now = ceiling + 1` the age from `at` (60) is
    /// only `ceiling - 59`, comfortably inside the ceiling, and this test
    /// goes red.
    #[test]
    fn a_stale_last_ok_outlasts_a_much_fresher_failure() {
        let ceiling = i64::try_from(STALE_AFTER.as_secs()).expect("fits");
        let report = Report {
            at: 60,
            outcome: Outcome::Failed(UsageError::Http(429)),
            last_ok: Some((0, some_usage())),
        };
        assert!(
            !report.is_stale(ceiling),
            "exactly 15 min after the numbers"
        );
        assert!(
            report.is_stale(ceiling + 1),
            "one second further — stale by last_ok's clock, though the \
             failure recorded at `at: 60` is nowhere near {ceiling}s old"
        );
    }

    /// A 2xx body that is not a usage document is a `Parse`, not a panic — and
    /// its message is serde's position, never the response text wholesale.
    #[test]
    fn a_2xx_that_is_not_json_is_a_parse_error() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = creds(dir.path(), FAKE_TOKEN);
        let (base, handle) = fake_endpoint("200 OK", "<html>maintenance</html>".to_owned());
        let err = fetch(&base, &path).expect_err("not a usage document");
        assert!(matches!(err, UsageError::Parse(_)), "{err:?}");
        assert!(err.sentence(0).starts_with("usage unavailable"));
        assert!(!err.sentence(0).contains(FAKE_TOKEN));
        drop(handle.join());
    }

    /// The happy path end to end, against the captured body over a real socket.
    #[test]
    fn a_200_fetch_returns_the_captured_limits() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = creds(dir.path(), FAKE_TOKEN);
        let (base, handle) = fake_endpoint("200 OK", CAPTURED.to_owned());
        let usage = fetch(&base, &path).expect("a 200 parses");
        assert_eq!(usage.limits.len(), 3);
        assert_eq!(usage.limits[0].kind, "session");
        drop(handle.join());
    }

    /// **A redirect is never followed, and the bearer never reaches its
    /// target.** Two real listeners: the first answers every request with a
    /// `302` pointing at the second; the second must never see a connection at
    /// all, so it cannot see the `Authorization` header either. `fetch` is a
    /// single blocking call, so by the time it returns, any redirect it was
    /// going to follow would already have reached the second listener —
    /// checking immediately after is not a race.
    ///
    /// Falsify by deleting `.max_redirects(0)` in `fetch`: the redirect is
    /// followed, the second listener's `200 {}` parses as an empty (but
    /// `Ok`) `Usage`, and this goes red on both the returned `Err` and the
    /// "never contacted" assertion.
    #[test]
    fn a_redirect_is_never_followed_and_the_bearer_never_reaches_it() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = creds(dir.path(), FAKE_TOKEN);

        // The redirect target. Bound, so a follow would have somewhere to
        // land — and so this test can prove nothing ever lands there.
        let second = TcpListener::bind("127.0.0.1:0").expect("bind second");
        let second_addr = second.local_addr().expect("addr");

        // The endpoint the poll actually calls: a 302 to `second` on every
        // request, carrying a real (small) JSON body a follow would parse as
        // a legitimate, empty readout.
        let first = TcpListener::bind("127.0.0.1:0").expect("bind first");
        let first_addr = first.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = first.accept().expect("accept");
            let raw = capture_request(&mut sock);
            let body = "{}";
            let resp = format!(
                "HTTP/1.1 302 Found\r\nlocation: http://{second_addr}/api/oauth/usage\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len(),
            );
            sock.write_all(resp.as_bytes()).expect("write response");
            raw
        });

        let err =
            fetch(&format!("http://{first_addr}"), &path).expect_err("a redirect is an error");
        assert_eq!(
            err,
            UsageError::Http(302),
            "the first listener's own status, not a followed 200"
        );

        let raw = handle.join().expect("server thread");
        assert!(raw.contains("GET /api/oauth/usage "), "{raw}");
        assert!(
            raw.to_ascii_lowercase()
                .contains(&format!("bearer {FAKE_TOKEN}").to_ascii_lowercase()),
            "the first hop must still authenticate normally: {raw}"
        );

        // The redirect target must never have been contacted — not by this
        // fetch, and not by anything else this process does concurrently
        // with the tests in this file (`TEST_SOCKETS` serialises them).
        second.set_nonblocking(true).expect("nonblocking");
        match second.accept() {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("the redirect target was contacted: {other:?}"),
        }
    }

    /// **The staleness ceiling**, at its boundary — a report inside
    /// [`STALE_AFTER`] is trusted, one past it is not. `now == at` is the
    /// freshest a report can be relative to itself.
    #[test]
    fn a_report_is_stale_only_past_the_ceiling() {
        let ceiling = i64::try_from(STALE_AFTER.as_secs()).expect("fits");
        let fresh = Report {
            at: 1_000,
            outcome: Outcome::Ok(Usage::default()),
            last_ok: None,
        };
        assert!(!fresh.is_stale(1_000), "zero age");
        assert!(!fresh.is_stale(1_000 + ceiling), "exactly the ceiling");
        assert!(fresh.is_stale(1_000 + ceiling + 1), "one past it");
        assert!(
            !fresh.is_stale(500),
            "a clock that moved backward is not stale"
        );
    }

    // ── (d) No credentials ───────────────────────────────────────────────────

    /// **Four ways to have no login, one arm** — and none of them reaches the
    /// network. The path is named in the sentence, which is the only actionable
    /// thing to say.
    #[test]
    fn every_unusable_credential_file_is_no_credentials() {
        let dir = tempfile::tempdir().expect("tempdir");

        // (1) No file at all. The base URL is a black hole on purpose: if the
        // credential check did not come first this would hang, not fail.
        let missing = dir.path().join("nope").join(".credentials.json");
        let err = fetch("http://127.0.0.1:1", &missing).expect_err("no file");
        assert_eq!(err, UsageError::NoCredentials(missing.clone()));
        assert!(err.sentence(0).contains(&missing.display().to_string()));
        assert!(err.sentence(0).contains("run `claude` once"));

        // (2) Not JSON.
        let garbage = dir.path().join(".credentials.json");
        std::fs::write(&garbage, "not json at all").expect("write");
        assert_eq!(
            fetch("http://127.0.0.1:1", &garbage).expect_err("garbage"),
            UsageError::NoCredentials(garbage.clone())
        );

        // (3) JSON, but no OAuth block.
        std::fs::write(&garbage, r#"{"somethingElse": true}"#).expect("write");
        assert_eq!(
            fetch("http://127.0.0.1:1", &garbage).expect_err("no oauth"),
            UsageError::NoCredentials(garbage.clone())
        );

        // (4) An OAuth block with a blank token.
        std::fs::write(&garbage, r#"{"claudeAiOauth":{"accessToken":"   "}}"#).expect("write");
        assert_eq!(
            fetch("http://127.0.0.1:1", &garbage).expect_err("blank token"),
            UsageError::NoCredentials(garbage)
        );
    }

    /// Reading the credential file must not **write** it. A poll runs every five
    /// minutes forever; a reader that touched mtime — let alone contents —
    /// would be fighting the CLI that owns the file.
    #[test]
    fn reading_the_credentials_never_writes_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = creds(dir.path(), FAKE_TOKEN);
        let before = std::fs::read(&path).expect("read");
        let meta_before = std::fs::metadata(&path).expect("stat");

        // A dead port: the fetch fails at connect, well after the read.
        let err = fetch("http://127.0.0.1:1", &path).expect_err("nothing listening");
        assert!(matches!(err, UsageError::Io(_)), "{err:?}");
        assert!(
            !err.sentence(0).contains(FAKE_TOKEN),
            "nor does an Io error"
        );

        assert_eq!(std::fs::read(&path).expect("read"), before);
        assert_eq!(
            std::fs::metadata(&path).expect("stat").modified().ok(),
            meta_before.modified().ok(),
            "the credential file is read-only to this crate"
        );
    }

    /// `CLAUDE_CONFIG_DIR` replaces the whole `~/.claude` directory, exactly as
    /// Claude Code resolves it; `$HOME` is the fallback; neither leaves a usable
    /// literal to name in the error.
    #[test]
    fn the_credential_path_follows_claude_code() {
        assert_eq!(
            credentials_path_in(Some("/srv/cc".to_owned()), Some("/home/a".to_owned())),
            PathBuf::from("/srv/cc/.credentials.json"),
            "the override wins"
        );
        assert_eq!(
            credentials_path_in(None, Some("/home/a".to_owned())),
            PathBuf::from("/home/a/.claude/.credentials.json"),
        );
        assert_eq!(
            credentials_path_in(None, None),
            PathBuf::from("~/.claude/.credentials.json"),
            "a nameable path beats an Option nobody could phrase"
        );
    }

    /// The scrubber that backs the "no token in any message" guarantee for the
    /// two arms that carry borrowed text.
    #[test]
    fn scrub_removes_every_occurrence_of_the_token() {
        let text = format!("connect failed for Bearer {FAKE_TOKEN} (retry {FAKE_TOKEN})");
        let cleaned = scrub(&text, FAKE_TOKEN);
        assert!(!cleaned.contains(FAKE_TOKEN));
        assert_eq!(cleaned.matches("<redacted>").count(), 2);
        assert_eq!(scrub("nothing to do", ""), "nothing to do");
    }

    /// Error text is bounded — a chip tooltip is not a log line.
    #[test]
    fn borrowed_error_text_is_truncated_on_a_char_boundary() {
        let long = "ä".repeat(500);
        let cut = truncate(&long);
        assert_eq!(cut.chars().count(), super::MAX_ERROR_CHARS + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(truncate("short"), "short");
    }

    // ── (f) Time ─────────────────────────────────────────────────────────────

    /// The timestamps the endpoint actually sends, plus every other RFC 3339
    /// spelling this could meet. The epoch values are independently checkable:
    /// `1970-01-01T00:00:00Z` is 0 and each case below states its own arithmetic.
    #[test]
    fn rfc3339_parses_the_spellings_the_endpoint_uses() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("1970-01-02T00:00:00Z"), Some(86_400));
        // The measured spelling: microseconds and an explicit +00:00.
        assert_eq!(
            parse_rfc3339("2026-09-13T13:50:00.101848+00:00"),
            parse_rfc3339("2026-09-13T13:50:00Z"),
            "fractional seconds are dropped, not fatal"
        );
        // An offset really shifts the instant.
        assert_eq!(
            parse_rfc3339("2026-09-13T15:50:00+02:00"),
            parse_rfc3339("2026-09-13T13:50:00Z"),
        );
        assert_eq!(
            parse_rfc3339("2026-09-13T11:50:00-0200"),
            parse_rfc3339("2026-09-13T13:50:00Z"),
            "the compact ±HHMM offset too"
        );
        assert_eq!(
            parse_rfc3339("2026-09-13 13:50:00"),
            parse_rfc3339("2026-09-13T13:50:00Z"),
            "a space separator and an absent offset read as UTC"
        );
        // A leap second lands on :59 rather than failing the row.
        assert_eq!(
            parse_rfc3339("2026-12-31T23:59:60Z"),
            parse_rfc3339("2026-12-31T23:59:59Z"),
        );
        for bad in [
            "",
            "yesterday",
            "2026-09-13",
            "2026-13-01T00:00:00Z",
            "2026-09-32T00:00:00Z",
            "2026-09-13T25:00:00Z",
            "2026-09-13T13:50:00+99:00",
            "2026-09-13T13:50:00.Z",
            "2026-09-13T13:50:00 lunchtime",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?} is not a timestamp");
        }
    }

    /// Civil↔epoch round-trips across leap years and century boundaries — the
    /// reason this is twenty lines of Hinnant rather than an approximation.
    #[test]
    fn the_absolute_rendering_round_trips() {
        for (text, rendered) in [
            ("1970-01-01T00:00:00Z", "1970-01-01 00:00 UTC"),
            ("2000-02-29T12:34:56Z", "2000-02-29 12:34 UTC"),
            ("2024-02-29T23:59:00Z", "2024-02-29 23:59 UTC"),
            ("2026-09-13T13:50:00.101848+00:00", "2026-09-13 13:50 UTC"),
            ("2100-03-01T00:00:00Z", "2100-03-01 00:00 UTC"),
        ] {
            let epoch = parse_rfc3339(text).expect(text);
            assert_eq!(format_utc(epoch), rendered, "{text}");
        }
    }

    /// **The humanised reset, from a fixed `now`.** Every branch, at the
    /// boundary, so a coarsening threshold cannot drift unnoticed.
    #[test]
    fn the_reset_is_humanised_from_a_fixed_now() {
        let now = parse_rfc3339("2026-09-13T11:35:00Z").expect("now");
        let at = |s: &str| parse_rfc3339(s).expect(s);

        assert_eq!(
            humanise_until(now, at("2026-09-13T13:50:00Z")),
            "in 2 h 15 min"
        );
        assert_eq!(humanise_until(now, at("2026-09-13T13:35:00Z")), "in 2 h");
        assert_eq!(humanise_until(now, at("2026-09-13T12:20:00Z")), "in 45 min");
        assert_eq!(humanise_until(now, now + 59), "in under a minute");
        assert_eq!(humanise_until(now, now), "any moment now");
        assert_eq!(humanise_until(now, now - 600), "any moment now");
        assert_eq!(humanise_until(now, at("2026-09-16T11:35:00Z")), "in 3 d");
        assert_eq!(
            humanise_until(now, at("2026-09-17T15:00:00Z")),
            "in 4 d 3 h"
        );

        assert_eq!(humanise_since(now, now), "just now");
        assert_eq!(humanise_since(now, now - 59), "just now");
        assert_eq!(humanise_since(now, now - 120), "2 min ago");
        assert_eq!(humanise_since(now, now - 7200), "2 h ago");
        assert_eq!(humanise_since(now, now - 200_000), "2 d ago");

        assert_eq!(
            reset_phrase(now, Some("2026-09-13T13:50:00.101848+00:00")),
            Some("resets 2026-09-13 13:50 UTC · in 2 h 15 min".to_owned()),
        );
        assert_eq!(
            reset_short(now, Some("2026-09-17T15:00:00.101872+00:00")),
            Some("in 4 d 3 h".to_owned()),
        );
        assert_eq!(reset_phrase(now, None), None, "a row may carry no reset");
        assert_eq!(
            reset_phrase(now, Some("soon")),
            None,
            "or an unreadable one"
        );
    }

    /// Labels are positional, not a lookup table — the point being that a bucket
    /// nobody has seen yet still renders.
    #[test]
    fn kinds_are_humanised_without_a_table_of_known_kinds() {
        assert_eq!(humanise_kind("session"), "Session (5 h)");
        assert_eq!(humanise_kind("weekly_all"), "Weekly (all)");
        assert_eq!(humanise_kind("weekly_scoped"), "Weekly (scoped)");
        assert_eq!(humanise_kind("weekly_opus_only"), "Weekly (opus only)");
        assert_eq!(humanise_kind("monthly"), "Monthly");
        assert_eq!(humanise_kind("quarterly_moonshot"), "Quarterly moonshot");
        assert_eq!(humanise_kind("écran"), "Écran", "non-ASCII heads raise too");
        assert_eq!(humanise_kind("  "), "Limit");
        assert_eq!(humanise_kind(""), "Limit");
    }

    // ── The board ────────────────────────────────────────────────────────────

    /// The board hands back what was published and bumps its version, which is
    /// what lets the chip skip the lock on 59 ticks out of 60.
    #[test]
    fn publishing_a_report_bumps_the_version() {
        let _guard = BOARD_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        let before = super::version();
        super::publish(Report {
            at: 1_000,
            outcome: Outcome::Failed(UsageError::Unauthorized),
            last_ok: None,
        });
        assert!(super::version() > before);
        let latest = super::latest().expect("something is published");
        assert_eq!(latest.usage(), None);
        assert_eq!(
            latest.error(1_000).as_deref(),
            Some("usage stale — run `claude` once to refresh the login")
        );

        super::publish(Report {
            at: 2_000,
            outcome: Outcome::Ok(Usage::default()),
            last_ok: None,
        });
        let latest = super::latest().expect("published");
        assert_eq!(latest.usage(), Some(&Usage::default()));
        assert_eq!(latest.error(2_000), None);
    }

    /// **#1285's review, MED — `poll_loop`'s wiring, pinned end to end.**
    /// `advance`/`next_wait` are unit-tested directly, but nothing drove the
    /// *loop* itself before this: the review measured that discarding the
    /// computed schedule (`sleep(POLL_EVERY)` instead of `sleep(wait)`) and
    /// discarding the carry-forward (`last_ok = None;` in the loop) were both
    /// **invisible to `cargo test`** — the exact regressions #1283 exists to
    /// prevent.
    ///
    /// Scripts `429(Retry-After: 900 s) → 429 → 200 → 429` through a fake
    /// `fetch_once` under a paused virtual clock, and checks the *published*
    /// [`Report`]s (not `advance`'s return value) plus the virtual gap each
    /// step actually waits — the loop's own board and its own sleep, which is
    /// exactly what `advance`/`next_wait` unit tests cannot reach.
    ///
    /// Falsify either mutation from the review and this goes red — see the
    /// PR comment for both transcripts.
    ///
    /// A plain `#[test]` building its own paused-clock current-thread
    /// runtime, rather than `#[tokio::test(start_paused = true)]`, so
    /// [`BOARD_TESTS`] can be held for the whole body: the guard is a local
    /// in this *synchronous* function, and `Runtime::block_on` is an
    /// ordinary blocking call from its point of view, not a suspension point
    /// inside an `async fn` — so nothing here holds a lock across an
    /// `.await` (`clippy::await_holding_lock`, part of `clippy::all`).
    #[test]
    fn poll_loop_sleeps_the_computed_wait_and_carries_last_ok_through_a_later_failure() {
        let _guard = BOARD_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        let calls = Arc::new(AtomicUsize::new(0));
        let usage = some_usage();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("a current-thread runtime with a paused clock");

        rt.block_on(async {
            let task = {
                let calls = Arc::clone(&calls);
                let usage = usage.clone();
                tokio::spawn(async move {
                    poll_loop(move || {
                        let n = calls.fetch_add(1, AtomicOrdering::SeqCst);
                        let usage = usage.clone();
                        async move {
                            match n {
                                // A 429 with an explicit Retry-After: pins
                                // the wait at exactly 15 min — ==
                                // MAX_BACKOFF since item 4, so this also
                                // probes the cap.
                                0 => (Err(UsageError::Http(429)), Some(Duration::from_mins(15))),
                                // A success: resets the schedule to
                                // POLL_EVERY and seeds `last_ok`.
                                2 => (Ok(usage), None),
                                // A second 429 with no header (doubling
                                // would overshoot the cap, so this also
                                // waits 15 min), and every failure after the
                                // success — `last_ok` must survive into
                                // that one.
                                _ => (Err(UsageError::Http(429)), None),
                            }
                        }
                    })
                    .await
                })
            };

            // The first fetch runs immediately, before any sleep.
            tokio::task::yield_now().await;
            assert_eq!(
                calls.load(AtomicOrdering::SeqCst),
                1,
                "one fetch, no sleep yet"
            );
            let report = super::latest().expect("published");
            assert!(
                matches!(report.outcome, Outcome::Failed(UsageError::RateLimited(_))),
                "{:?}",
                report.outcome
            );
            assert_eq!(report.last_ok, None, "nothing to carry yet");

            // The loop must sleep the computed 15 min wait, not a hardcoded
            // POLL_EVERY (5 min) — falsifies `sleep(POLL_EVERY)` in place of
            // `sleep(wait)`. The `yield_now` right after `advance` matters:
            // `advance` moves the paused clock and marks any now-overdue
            // timer ready, but a task made ready *during* one `advance` call
            // is not actually polled until the executor's next chance to run
            // — without flushing that here, a wrongly-short sleep (e.g. the
            // 5 min `POLL_EVERY` mutation) would have already fired well
            // before 899 s but this assertion would not observe it yet,
            // making the whole probe vacuous.
            tokio::time::advance(Duration::from_secs(15 * 60 - 1)).await;
            tokio::task::yield_now().await;
            assert_eq!(
                calls.load(AtomicOrdering::SeqCst),
                1,
                "899 s in, still waiting"
            );
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            assert_eq!(
                calls.load(AtomicOrdering::SeqCst),
                2,
                "exactly 15 min wakes it"
            );
            let report = super::latest().expect("published");
            assert!(
                matches!(report.outcome, Outcome::Failed(UsageError::RateLimited(_))),
                "{:?}",
                report.outcome
            );
            assert_eq!(report.last_ok, None, "still nothing to carry");

            // Third fetch: the success. Resets the schedule to POLL_EVERY.
            tokio::time::advance(Duration::from_mins(15)).await;
            tokio::task::yield_now().await;
            assert_eq!(calls.load(AtomicOrdering::SeqCst), 3, "the success");
            let report = super::latest().expect("published");
            assert_eq!(report.outcome, Outcome::Ok(usage.clone()));
            assert_eq!(report.last_ok, Some((report.at, usage.clone())));

            // The success resets the wait to POLL_EVERY (5 min) — not the
            // 15 min cap it was just backed off to. Same `yield_now` reason
            // as above.
            tokio::time::advance(Duration::from_secs(5 * 60 - 1)).await;
            tokio::task::yield_now().await;
            assert_eq!(
                calls.load(AtomicOrdering::SeqCst),
                3,
                "299 s in, still waiting"
            );
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            assert_eq!(
                calls.load(AtomicOrdering::SeqCst),
                4,
                "exactly POLL_EVERY wakes it"
            );

            // The failure AFTER the success: `last_ok` must have survived
            // the trip through the loop's own local state — falsifies
            // `last_ok = None;` discarding the carry-forward in `poll_loop`
            // itself.
            let report = super::latest().expect("published");
            assert!(
                matches!(report.outcome, Outcome::Failed(UsageError::RateLimited(_))),
                "{:?}",
                report.outcome
            );
            assert_eq!(
                report.usage(),
                Some(&usage),
                "last_ok must survive a failure that comes after the success"
            );

            task.abort();
        });
    }

    /// The default base URL is the real API host, spelled once.
    #[test]
    fn the_default_base_url_is_the_api_host() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.anthropic.com");
    }
}
