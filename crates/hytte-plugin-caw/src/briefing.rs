//! caw's **morning briefing** — trigger, composition, and voice (#407).
//!
//! Once a day caw caws the news: the day's shape (weather, the first useful
//! departure, and — once the host shares them — calendar events) composed into
//! two or three short sentences and delivered sticky in her speech bubble,
//! mirrored as a toast.
//!
//! - **Trigger** ([`is_due`]): at/after a configured local time
//!   (`$CAW_BRIEFING_TIME`, default 07:00; `off` disables), checked on the
//!   plugin's 2 s heartbeat, at most once per local date (persisted via
//!   [`Stamp`] so a plugin/session restart never re-caws). The check only runs
//!   while the box is awake, so a machine suspended across the hour briefs on
//!   wake — the practical stand-in for "first unlock" until the host pushes
//!   logind state to plugins. A same-day window ([`WINDOW_MINS`]) keeps an
//!   evening cold-start from cawing "morning news" at 22:00.
//! - **Composition**: [`compose_plain`] is the deterministic, canned template —
//!   the keyless path *and* the fallback. With a provider configured
//!   ([`Cfg::from_env`], the pet-brain pattern: `$CAW_LLM_URL` for a local
//!   `llama-server`, else the shared `openrouter.key` + `$CAW_LLM_MODEL`),
//!   [`compose_llm`] asks for the same facts in caw's own voice — one
//!   `chat()` call through [`hytte_ai_providers`]. Keyless and URL-less
//!   resolves to the plain path up front: no doomed network round-trips
//!   (the #438/#472 rule).
//! - The reducer side (sticky-until-poked, the toast) lives in `main.rs`.

use std::path::PathBuf;

use chrono::{Datelike, NaiveDate, Timelike};
use hytte_ai_providers::{ChatOpts, Message, Provider};

use crate::ingredients::{self, EventBrief, Ingredients};

/// Default briefing time: 07:00 local.
const DEFAULT_TIME_MINS: u16 = 7 * 60;

/// The briefing stays due from the configured time until this many minutes
/// later (same local date). Late in the window still briefs (wake-from-suspend
/// at 09:30 with a 07:00 setting should caw); *past* it the day is skipped, so
/// starting the shell at night never delivers a stale "morning" news drop.
const WINDOW_MINS: u16 = 6 * 60;

/// Bubble budget for the composed briefing, in chars — roughly the 8-line
/// pixel-font speech box (`speech::briefing_node`); [`sanitize`] enforces it
/// whatever the model does.
const MAX_BRIEF: usize = 220;

/// caw's standing news-desk persona. Facts-only by instruction; [`sanitize`]
/// still enforces the format mechanically.
const PERSONA: &str = "You are caw, a sardonic cybercrow who lives in the sidebar of Annika's \
     Linux desktop and delivers the morning news. Compose one tiny briefing \
     from the facts given. Style: dry, lowercase, cyberpunk corvid snark. \
     Format: 2-3 short sentences, under 200 characters total, plain text, no \
     emoji, no quotes, no lists. Mention only facts you were given; never \
     invent events, weather, or trains.";

/// Canned openers, cycled by day-of-year so consecutive mornings differ.
const GREETINGS: &[&str] = &[
    "morning, meat-computer.",
    "dawn patrol report:",
    "*shakes dew off feathers* news:",
    "another day on the grid.",
    "rise and grind, choom.",
];

/// What a data-less morning sounds like.
const NO_DATA_LINE: &str = "no data on the wire. suspicious. fly careful out there.";

// ── Configuration ────────────────────────────────────────────────────────────

/// Briefing configuration, resolved once per session from the environment.
pub(crate) struct Cfg {
    /// Local briefing time as minutes since midnight; `None` = disabled.
    pub time: Option<u16>,
    /// The chat provider for the voiced path; `None` = the plain template.
    pub provider: Option<Provider>,
}

impl Cfg {
    pub(crate) fn from_env() -> Self {
        let time = parse_time(std::env::var("CAW_BRIEFING_TIME").ok().as_deref());
        // caw's key: the shared key file (`openrouter.key` / its
        // `OPENROUTER_API_KEY` override) first, then the caw-specific env.
        let key = hytte_ai_providers::load_key("openrouter").or_else(|| {
            std::env::var("CAW_LLM_API_KEY")
                .ok()
                .filter(|s| !s.is_empty())
        });
        let model = std::env::var("CAW_LLM_MODEL")
            .ok()
            .filter(|s| !s.is_empty());
        let provider = resolve_provider(std::env::var("CAW_LLM_URL").ok().as_deref(), key, model);
        Self { time, provider }
    }
}

/// Parse `$CAW_BRIEFING_TIME`: unset/empty → the 07:00 default; `off`/`none` →
/// disabled; `"H"` or `"H:MM"` → that local time. An unparseable value falls
/// back to the default (fail-open: a typo shouldn't silently kill the news).
pub(crate) fn parse_time(raw: Option<&str>) -> Option<u16> {
    let raw = raw.map(str::trim).unwrap_or_default();
    if raw.is_empty() {
        return Some(DEFAULT_TIME_MINS);
    }
    if raw.eq_ignore_ascii_case("off") || raw.eq_ignore_ascii_case("none") {
        return None;
    }
    let (h, m) = raw.split_once(':').unwrap_or((raw, "0"));
    match (h.parse::<u16>(), m.parse::<u16>()) {
        (Ok(h), Ok(m)) if h < 24 && m < 60 => Some(h * 60 + m),
        _ => Some(DEFAULT_TIME_MINS),
    }
}

/// Resolve caw's briefing [`Provider`] — the pet's #438 semantics verbatim:
/// an explicitly empty `$CAW_LLM_URL` disables the model; an explicit URL is a
/// local/self-hosted backend (keyless OK); no URL defaults to `OpenRouter`
/// **only when a key exists** (a keyless cloud call would just 401, so it
/// short-circuits to the plain path instead of a doomed round-trip).
fn resolve_provider(
    url_env: Option<&str>,
    key: Option<String>,
    model: Option<String>,
) -> Option<Provider> {
    match url_env {
        Some("") => None,
        Some(url) => Some(Provider {
            base_url: url.to_owned(),
            api_key: key,
            model,
        }),
        None => key.map(|key| Provider {
            base_url: "https://openrouter.ai/api".to_owned(),
            api_key: Some(key),
            model,
        }),
    }
}

// ── The trigger ──────────────────────────────────────────────────────────────

/// `t`'s minutes since local midnight (`0..=1439`).
pub(crate) fn minutes_of_day(t: &impl Timelike) -> u16 {
    u16::try_from(t.hour() * 60 + t.minute()).unwrap_or(0)
}

/// Whether the briefing is due **now**: not yet briefed `today`, and `now_mins`
/// sits inside the `[at_mins, at_mins + WINDOW_MINS)` same-day window. Pure —
/// the caller supplies the clock.
pub(crate) fn is_due(
    now_mins: u16,
    today: NaiveDate,
    at_mins: u16,
    last_briefed: Option<NaiveDate>,
) -> bool {
    if last_briefed == Some(today) {
        return false;
    }
    now_mins >= at_mins && now_mins < at_mins.saturating_add(WINDOW_MINS)
}

/// The full briefing trigger (#484): [`is_due`] **and** the session is unlocked.
/// The heartbeat evaluates this every 2 s, so — because it runs while locked too —
/// gating on `!locked` makes the briefing fire on the **first unlock** inside the
/// window (the human returning) rather than announcing the morning news to a
/// locked screen. If the session is already unlocked when the hour arrives, the
/// very next heartbeat fires it (there is no unlock edge to wait for). Pure — the
/// caller supplies the clock and the live lock state.
pub(crate) fn should_brief(
    now_mins: u16,
    today: NaiveDate,
    at_mins: u16,
    last_briefed: Option<NaiveDate>,
    locked: bool,
) -> bool {
    !locked && is_due(now_mins, today, at_mins, last_briefed)
}

/// The once-a-day guard, persisted as a bare `YYYY-MM-DD` next to caw's
/// expression file (`$XDG_STATE_HOME/caw/briefing-stamp`) so a plugin or
/// session restart never re-caws the same morning.
pub(crate) struct Stamp {
    path: PathBuf,
    last: Option<NaiveDate>,
}

impl Stamp {
    /// Load the stamp from its state-dir home (missing/garbage file = never
    /// briefed).
    pub(crate) fn load() -> Self {
        Self::at(
            crate::expression::state_dir()
                .join("caw")
                .join("briefing-stamp"),
        )
    }

    /// A stamp at an explicit path (the seam the tests use).
    pub(crate) fn at(path: PathBuf) -> Self {
        let last = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| parse_stamp(&s));
        Self { path, last }
    }

    /// The last briefed local date, if any.
    pub(crate) fn last(&self) -> Option<NaiveDate> {
        self.last
    }

    /// Record `date` as briefed — write-through, so even a crash mid-compose
    /// can't turn into a re-caw loop.
    pub(crate) fn mark(&mut self, date: NaiveDate) {
        self.last = Some(date);
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&self.path, format!("{date}\n")) {
            eprintln!("[caw] briefing stamp write failed: {e}");
        }
    }
}

fn parse_stamp(s: &str) -> Option<NaiveDate> {
    s.trim().parse().ok()
}

// ── Composition ──────────────────────────────────────────────────────────────

/// The deterministic, canned briefing — the keyless path and the fallback.
/// `day_seed` (day-of-year) cycles the greeting pool.
pub(crate) fn compose_plain(ing: &Ingredients, day_seed: usize) -> String {
    let mut parts: Vec<String> = vec![GREETINGS[day_seed % GREETINGS.len()].to_owned()];
    if let Some(w) = &ing.weather {
        parts.push(format!(
            "{:.0}° {}, high {:.0}°.",
            w.temp_c, w.label, w.high_c
        ));
    }
    match ing.events.as_slice() {
        [] => {}
        [e] => parts.push(format!("{} at {}, then nothing.", e.summary, e.hhmm)),
        [e, rest @ ..] => parts.push(format!(
            "{} at {}, then {} more.",
            e.summary,
            e.hhmm,
            rest.len()
        )),
    }
    if let Some(d) = &ing.departure {
        let tail = match d.leave_in {
            Some(l) if l <= 2 => " — move, choom.".to_owned(),
            Some(l) => format!(" — leave in {l}."),
            None => ".".to_owned(),
        };
        parts.push(format!("{} to {} in {}{tail}", d.line, d.direction, d.mins));
    }
    if ing.weather.is_none() && ing.departure.is_none() && ing.events.is_empty() {
        parts.push(NO_DATA_LINE.to_owned());
    }
    parts.join(" ")
}

/// The facts block for the model — plain labeled lines, with absent
/// ingredients stated as unavailable (so the persona's "never invent" rule has
/// something honest to lean on).
pub(crate) fn facts(ing: &Ingredients, now_stamp: &str) -> String {
    let mut lines = vec![format!("local time: {now_stamp}")];
    match &ing.weather {
        Some(w) => lines.push(format!(
            "weather: {:.0}°C {}, high {:.0}°C",
            w.temp_c, w.label, w.high_c
        )),
        None => lines.push("weather: unavailable".to_owned()),
    }
    if ing.events.is_empty() {
        lines.push("calendar: unavailable".to_owned());
    } else {
        for e in &ing.events {
            lines.push(format!("event: {} at {}", e.summary, e.hhmm));
        }
    }
    match &ing.departure {
        Some(d) => {
            let leave = d
                .leave_in
                .map_or_else(String::new, |l| format!(" (leave in {l} min)"));
            lines.push(format!(
                "next train: {} to {} in {} min{leave}",
                d.line, d.direction, d.mins
            ));
        }
        None => lines.push("next train: none catchable".to_owned()),
    }
    lines.join("\n")
}

/// One blocking chat-completion call (run on a `spawn_blocking` thread): the
/// persona plus the facts, sanitized into one bubble-sized paragraph. An empty
/// result is an error so the caller falls back to [`compose_plain`].
pub(crate) fn compose_llm(provider: &Provider, facts: &str) -> Result<String, String> {
    let messages = [
        Message::system(PERSONA),
        Message::user(format!("{facts}\n\ncaw the morning news now:")),
    ];
    let opts = ChatOpts {
        max_tokens: 160,
        temperature: 0.8,
    };
    let raw = hytte_ai_providers::chat(provider, &messages, &opts)?;
    let brief = sanitize(&raw);
    if brief.is_empty() {
        Err("model produced an empty briefing".to_owned())
    } else {
        Ok(brief)
    }
}

/// Force whatever the model said into one clean bubble paragraph: lines joined,
/// wrapping quotes stripped, emoji and double-quote lookalikes dropped (the
/// pixel font boxes them anyway), whitespace collapsed, clamped to
/// [`MAX_BRIEF`] chars.
pub(crate) fn sanitize(raw: &str) -> String {
    let joined = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let joined = joined
        .trim_matches(|c| c == '"' || c == '\'' || c == '“' || c == '”')
        .trim();
    let cleaned: String = joined.chars().filter(|&c| !is_dropped(c)).collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = cleaned.chars().take(MAX_BRIEF).collect();
    if cleaned.chars().count() > MAX_BRIEF {
        while out.chars().last().is_some_and(is_combining) {
            out.pop();
        }
        out.push('…');
    }
    out
}

/// Common combining-mark ranges (kept in sync with the pet's bubble rule; a
/// full grapheme segmenter would be a dep for nothing).
fn is_combining(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{20D0}'..='\u{20FF}')
}

/// Codepoints to drop: emoji blocks plus every double-quote lookalike — the
/// same policy as the pet's bubble (tiny models ignore "no emoji").
fn is_dropped(c: char) -> bool {
    matches!(
        c,
        '\u{1F000}'..='\u{1FAFF}'
            | '\u{2600}'..='\u{27BF}'
            | '\u{FE0F}'
            | '\u{200D}'
            | '"'
            | '\u{201c}'
            | '\u{201d}'
            | '\u{201e}'
            | '\u{ff02}'
    )
}

// ── The pipeline ─────────────────────────────────────────────────────────────

/// Compose today's briefing: gather the reachable ingredients (blocking weather /
/// departures I/O), fold in the host-pushed `events` (#484), then voice them
/// through the provider — or hand back the plain template keyless / on any model
/// failure. Always returns *something*; run on a `spawn_blocking` thread.
pub(crate) fn brief_now(provider: Option<&Provider>, events: Vec<EventBrief>) -> String {
    let mut ing = ingredients::gather();
    // The calendar slot no longer fetches itself — the host shares it (#484).
    ing.events = events;
    let now = chrono::Local::now();
    let plain = compose_plain(&ing, usize::try_from(now.ordinal()).unwrap_or(0));
    let Some(provider) = provider else {
        return plain;
    };
    let now_stamp = now.format("%A %H:%M").to_string();
    match compose_llm(provider, &facts(&ing, &now_stamp)) {
        Ok(brief) => brief,
        Err(e) => {
            eprintln!("[caw] briefing brain offline ({e}); cawing the plain version");
            plain
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingredients::{DepartureBrief, EventBrief, WeatherBrief};
    use std::io::{Read, Write};

    fn date(s: &str) -> NaiveDate {
        s.parse().expect("valid date")
    }

    fn full() -> Ingredients {
        Ingredients {
            weather: Some(WeatherBrief {
                temp_c: 2.6,
                label: "rain",
                high_c: 8.4,
            }),
            departure: Some(DepartureBrief {
                line: "S9".to_owned(),
                direction: "Spandau".to_owned(),
                mins: 12,
                leave_in: Some(2),
            }),
            events: Vec::new(),
        }
    }

    // ── Config parsing ───────────────────────────────────────────────────────

    #[test]
    fn parse_time_covers_default_off_and_formats() {
        assert_eq!(parse_time(None), Some(7 * 60), "unset → the 07:00 default");
        assert_eq!(parse_time(Some("")), Some(7 * 60));
        assert_eq!(parse_time(Some("off")), None, "explicitly disabled");
        assert_eq!(parse_time(Some("NONE")), None);
        assert_eq!(parse_time(Some("7")), Some(7 * 60));
        assert_eq!(parse_time(Some("7:30")), Some(7 * 60 + 30));
        assert_eq!(parse_time(Some(" 22:05 ")), Some(22 * 60 + 5));
        assert_eq!(parse_time(Some("0:00")), Some(0), "midnight is valid");
        // Fail-open: nonsense and out-of-range fall back to the default.
        assert_eq!(parse_time(Some("25")), Some(7 * 60));
        assert_eq!(parse_time(Some("7:60")), Some(7 * 60));
        assert_eq!(parse_time(Some("breakfast")), Some(7 * 60));
    }

    #[test]
    fn resolve_provider_matches_the_pet_semantics() {
        // Explicitly empty URL → disabled.
        assert!(resolve_provider(Some(""), None, None).is_none());
        // Explicit URL → local backend, keyless OK, model layered.
        let p = resolve_provider(Some("http://host:1"), None, Some("m".to_owned())).expect("local");
        assert_eq!(p.base_url, "http://host:1");
        assert!(p.api_key.is_none());
        assert_eq!(p.model.as_deref(), Some("m"));
        // No URL + key → the OpenRouter default.
        let p = resolve_provider(None, Some("sk-1".to_owned()), None).expect("cloud");
        assert_eq!(p.base_url, "https://openrouter.ai/api");
        assert_eq!(p.api_key.as_deref(), Some("sk-1"));
        // No URL, no key → plain-only; no doomed 401 round-trips (#438/#472).
        assert!(resolve_provider(None, None, None).is_none());
        assert!(resolve_provider(None, None, Some("m".to_owned())).is_none());
    }

    // ── Time windowing ───────────────────────────────────────────────────────

    #[test]
    fn is_due_only_inside_the_same_day_window_and_once_per_date() {
        let today = date("2026-07-23");
        let at = 7 * 60;
        // Before the hour → not due.
        assert!(!is_due(at - 1, today, at, None));
        // At and after the hour → due (until the window closes).
        assert!(is_due(at, today, at, None));
        assert!(is_due(at + 359, today, at, None), "late wake still briefs");
        // Past the window → the day is skipped (no 22:00 "morning" news).
        assert!(!is_due(at + 360, today, at, None));
        assert!(!is_due(22 * 60, today, at, None));
        // Already briefed today → never again, even inside the window.
        assert!(!is_due(at + 5, today, at, Some(today)));
        // A *previous* date's stamp doesn't block today.
        assert!(is_due(at + 5, today, at, Some(date("2026-07-22"))));
    }

    #[test]
    fn should_brief_gates_on_unlocked_inside_the_window() {
        let today = date("2026-07-23");
        let at = 7 * 60;
        // Due + unlocked → brief.
        assert!(should_brief(at, today, at, None, false));
        // Due but locked → hold (the human is away; wait for the first unlock).
        assert!(
            !should_brief(at, today, at, None, true),
            "a locked screen doesn't get the morning news"
        );
        // Unlocking inside the window → the next heartbeat fires it.
        assert!(should_brief(at + 30, today, at, None, false));
        // Not due (before the hour) → unlocked doesn't matter.
        assert!(!should_brief(at - 1, today, at, None, false));
        // Already briefed today → locked or not, never again.
        assert!(!should_brief(at + 5, today, at, Some(today), false));
    }

    #[test]
    fn is_due_late_configured_time_caps_at_midnight() {
        // 23:30 + the 6h window saturates; same-date minutes can't roll over,
        // so due-ness simply runs to 23:59.
        let today = date("2026-07-23");
        let at = 23 * 60 + 30;
        assert!(is_due(23 * 60 + 45, today, at, None));
        assert!(!is_due(23 * 60 + 15, today, at, None));
    }

    #[test]
    fn minutes_of_day_counts_from_midnight() {
        let t = chrono::NaiveTime::from_hms_opt(7, 30, 15).expect("valid");
        assert_eq!(minutes_of_day(&t), 7 * 60 + 30);
        let mid = chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("valid");
        assert_eq!(minutes_of_day(&mid), 0);
    }

    // ── Stamp persistence ────────────────────────────────────────────────────

    #[test]
    fn stamp_round_trips_and_tolerates_garbage() {
        let dir = std::env::temp_dir().join(format!("caw-briefing-stamp-{}", std::process::id()));
        let path = dir.join("caw").join("briefing-stamp");
        // Missing file → never briefed.
        let mut stamp = Stamp::at(path.clone());
        assert_eq!(stamp.last(), None);
        // Mark writes through (creating the dir) and a fresh load agrees.
        stamp.mark(date("2026-07-23"));
        assert_eq!(stamp.last(), Some(date("2026-07-23")));
        assert_eq!(Stamp::at(path.clone()).last(), Some(date("2026-07-23")));
        // Garbage content → never briefed (fail toward one extra caw, not a
        // permanently silenced one).
        std::fs::write(&path, "not a date\n").expect("write garbage");
        assert_eq!(Stamp::at(path).last(), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── Plain composition ────────────────────────────────────────────────────

    #[test]
    fn compose_plain_composes_every_ingredient() {
        let text = compose_plain(&full(), 0);
        assert!(text.starts_with("morning, meat-computer."), "{text}");
        assert!(text.contains("3° rain, high 8°."), "{text}");
        assert!(
            text.contains("S9 to Spandau in 12 — move, choom."),
            "a tight leave-by turns urgent: {text}"
        );
    }

    #[test]
    fn compose_plain_departure_tail_variants() {
        let mut ing = full();
        ing.weather = None;
        // A comfortable margin counts down instead of barking.
        ing.departure.as_mut().expect("departure").leave_in = Some(9);
        assert!(
            compose_plain(&ing, 1).contains("S9 to Spandau in 12 — leave in 9."),
            "{}",
            compose_plain(&ing, 1)
        );
        // No walk budget → a plain sentence.
        ing.departure.as_mut().expect("departure").leave_in = None;
        assert!(
            compose_plain(&ing, 1).contains("S9 to Spandau in 12."),
            "{}",
            compose_plain(&ing, 1)
        );
    }

    #[test]
    fn compose_plain_events_fold_in_when_present() {
        // The calendar slot is compose-ready even though gather() leaves it
        // empty today (the (c) StateKey follow-up only touches the I/O side).
        let mut ing = full();
        ing.events = vec![
            EventBrief {
                hhmm: "10:00".to_owned(),
                summary: "standup".to_owned(),
            },
            EventBrief {
                hhmm: "15:30".to_owned(),
                summary: "the thing".to_owned(),
            },
        ];
        let text = compose_plain(&ing, 2);
        assert!(text.contains("standup at 10:00, then 1 more."), "{text}");
        let mut one = full();
        one.events = vec![EventBrief {
            hhmm: "10:00".to_owned(),
            summary: "standup".to_owned(),
        }];
        assert!(
            compose_plain(&one, 2).contains("standup at 10:00, then nothing."),
            "{}",
            compose_plain(&one, 2)
        );
    }

    #[test]
    fn compose_plain_empty_day_still_caws() {
        let text = compose_plain(&Ingredients::default(), 3);
        assert!(text.contains(NO_DATA_LINE), "{text}");
    }

    #[test]
    fn compose_plain_greeting_cycles_by_day() {
        let a = compose_plain(&full(), 0);
        let b = compose_plain(&full(), 1);
        assert_ne!(a, b, "consecutive mornings open differently");
        assert_eq!(
            compose_plain(&full(), 0),
            compose_plain(&full(), GREETINGS.len()),
            "the pool cycles deterministically"
        );
    }

    // ── Facts for the model ──────────────────────────────────────────────────

    #[test]
    fn facts_state_present_and_absent_ingredients_honestly() {
        let text = facts(&full(), "Thursday 07:30");
        assert!(text.contains("local time: Thursday 07:30"), "{text}");
        assert!(text.contains("weather: 3°C rain, high 8°C"), "{text}");
        assert!(text.contains("calendar: unavailable"), "{text}");
        assert!(
            text.contains("next train: S9 to Spandau in 12 min (leave in 2 min)"),
            "{text}"
        );
        let empty = facts(&Ingredients::default(), "Friday 08:00");
        assert!(empty.contains("weather: unavailable"), "{empty}");
        assert!(empty.contains("next train: none catchable"), "{empty}");
    }

    // ── Sanitize ─────────────────────────────────────────────────────────────

    #[test]
    fn sanitize_joins_lines_strips_quotes_and_clamps() {
        assert_eq!(
            sanitize("\"cold out.\nmove, choom.\"\n"),
            "cold out. move, choom."
        );
        assert_eq!(sanitize("  \n\n one line \n"), "one line");
        assert_eq!(
            sanitize("no emoji \u{1F426}\u{200D}\u{2764}\u{FE0F} ok"),
            "no emoji ok"
        );
        let long = "caw ".repeat(200);
        let out = sanitize(&long);
        assert_eq!(out.chars().count(), MAX_BRIEF + 1, "clamped + ellipsis");
        assert!(out.ends_with('…'));
        assert_eq!(sanitize(""), "");
    }

    // ── The model path (fake server; no network) ─────────────────────────────

    /// A one-shot fake OpenAI-compatible server answering with `body`.
    fn spawn_fake(body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len(),
            );
            sock.write_all(resp.as_bytes()).expect("write");
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn compose_llm_parses_and_sanitizes() {
        let (base, server) = spawn_fake(
            r#"{"choices":[{"message":{"role":"assistant","content":"\"cold out, chrome-rain at 8.\nS9 in 12 — move.\"\n"}}]}"#,
        );
        let brief = compose_llm(&Provider::llama(base), &facts(&full(), "Thursday 07:30"))
            .expect("parses + sanitizes");
        assert_eq!(brief, "cold out, chrome-rain at 8. S9 in 12 — move.");
        server.join().expect("server thread");
    }

    #[test]
    fn compose_llm_blank_reply_is_an_error() {
        let (base, server) = spawn_fake(r#"{"choices":[{"message":{"content":"  \n"}}]}"#);
        let err = compose_llm(&Provider::llama(base), "facts").expect_err("blank → fallback");
        assert!(err.contains("empty briefing"), "{err}");
        server.join().expect("server thread");
    }
}
