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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

/// The API host the usage endpoint lives on. A parameter of [`fetch`] rather
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
    /// Any other non-2xx.
    Http(u16),
    /// The request never completed (DNS, TLS, connect, read).
    Io(String),
    /// A 2xx body that did not parse as [`Usage`].
    Parse(String),
}

impl UsageError {
    /// The one line a human reads. Never contains the token — see the module
    /// docs and `an_unauthorized_sentence_carries_no_token_bytes`.
    #[must_use]
    pub fn sentence(&self) -> String {
        match self {
            Self::NoCredentials(path) => format!(
                "no Claude login found at {} — run `claude` once to sign in",
                path.display()
            ),
            Self::Unauthorized => "usage stale — run `claude` once to refresh the login".to_owned(),
            Self::Http(status) => {
                format!("usage unavailable — the usage endpoint answered HTTP {status}")
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
/// `~/.claude` directory when it is set (the same variable
/// [`crate::envguard`] refuses for the `claude` child, because it moves where
/// the login credential is read from), otherwise `$HOME/.claude`.
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
/// generic fields.
///
/// Blocking — call it from `spawn_blocking`, the way [`poll_forever`] does and
/// [`crate::messages`] already does for its own `ureq` client.
///
/// # Errors
///
/// Every arm of [`UsageError`]; each carries a one-line
/// [`sentence`](UsageError::sentence) and **never** the bearer token.
pub fn fetch(base_url: &str, credentials_path: &Path) -> Result<Usage, UsageError> {
    let token = read_access_token(credentials_path)?;

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(TIMEOUT))
        // Read the endpoint's own body on a non-2xx rather than collapsing it
        // into a transport error: the status is what decides the arm below.
        .http_status_as_error(false)
        .build()
        .into();

    let url = format!("{}{USAGE_PATH}", base_url.trim_end_matches('/'));
    let mut resp = agent
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| UsageError::Io(truncate(&scrub(&e.to_string(), &token))))?;

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

/// The last poll: when it ran and how it went.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Unix seconds at which the attempt completed.
    pub at: i64,
    /// What it produced.
    pub outcome: Outcome,
}

impl Report {
    /// The payload, if the last attempt succeeded.
    #[must_use]
    pub fn usage(&self) -> Option<&Usage> {
        match &self.outcome {
            Outcome::Ok(usage) => Some(usage),
            Outcome::Failed(_) => None,
        }
    }

    /// The failure sentence, if the last attempt failed.
    #[must_use]
    pub fn error(&self) -> Option<String> {
        match &self.outcome {
            Outcome::Ok(_) => None,
            Outcome::Failed(e) => Some(e.sentence()),
        }
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

/// Poll forever: one fetch immediately, then one every [`POLL_EVERY`].
///
/// Never returns. Spawned by `main` on the HTTP runtime — deliberately *not*
/// from the plugin SDK's session, so the numbers keep arriving while the shell
/// is down and the chip's dial/backoff is running.
pub async fn poll_forever(base_url: String, credentials: PathBuf) {
    loop {
        let (base, creds) = (base_url.clone(), credentials.clone());
        let outcome = match tokio::task::spawn_blocking(move || fetch(&base, &creds)).await {
            Ok(Ok(usage)) => Outcome::Ok(usage),
            Ok(Err(e)) => Outcome::Failed(e),
            Err(e) => Outcome::Failed(UsageError::Io(truncate(&format!(
                "the usage poll task did not finish: {e}"
            )))),
        };
        if let Outcome::Failed(ref e) = outcome {
            // The sentence, never the cause verbatim and never the token.
            tracing::debug!(reason = %e.sentence(), "usage poll produced no numbers");
        }
        publish(Report {
            at: now_unix(),
            outcome,
        });
        tokio::time::sleep(POLL_EVERY).await;
    }
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
        DEFAULT_BASE_URL, ExtraUsage, Limit, Outcome, Report, SEVERITY_NORMAL, Usage, UsageError,
        credentials_path_in, fetch, format_utc, humanise_kind, humanise_since, humanise_until,
        parse_rfc3339, percent_label, reset_phrase, reset_short, scrub, truncate,
    };
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, PoisonError};

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
        let sentence = err.sentence();
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
        assert!(err.sentence().contains("HTTP 500"));
        drop(handle.join());
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
        assert!(err.sentence().starts_with("usage unavailable"));
        assert!(!err.sentence().contains(FAKE_TOKEN));
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
        assert!(err.sentence().contains(&missing.display().to_string()));
        assert!(err.sentence().contains("run `claude` once"));

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
        assert!(!err.sentence().contains(FAKE_TOKEN), "nor does an Io error");

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
        let before = super::version();
        super::publish(Report {
            at: 1_000,
            outcome: Outcome::Failed(UsageError::Unauthorized),
        });
        assert!(super::version() > before);
        let latest = super::latest().expect("something is published");
        assert_eq!(latest.usage(), None);
        assert_eq!(
            latest.error().as_deref(),
            Some("usage stale — run `claude` once to refresh the login")
        );

        super::publish(Report {
            at: 2_000,
            outcome: Outcome::Ok(Usage::default()),
        });
        let latest = super::latest().expect("published");
        assert_eq!(latest.usage(), Some(&Usage::default()));
        assert_eq!(latest.error(), None);
    }

    /// The default base URL is the real API host, spelled once.
    #[test]
    fn the_default_base_url_is_the_api_host() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.anthropic.com");
    }
}
