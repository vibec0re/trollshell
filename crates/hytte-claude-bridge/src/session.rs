//! Conversation identity, derived — the subtle half of the bridge.
//!
//! # Why identity has to be derived at all
//!
//! `hytte_ai_providers`' `ChatRequest` carries **no conversation id**: it is
//! `model` / `messages` / `max_tokens` / `temperature` / `enable_thinking` and
//! nothing else. Every request arrives with its full transcript and no handle.
//! Meanwhile `hive-claude` resolves sessions **by title** (`Attach::Create`
//! persists one, `Attach::Resume` resolves against it), so the bridge never
//! tracks a UUID — **the title *is* the identity**, and it has to be computed
//! from the only thing the request carries: the transcript.
//!
//! # The rule
//!
//! A conversation is identified by its **transcript prefix** — every message
//! except the newest one. Turn 1 of `[system, user]` has prefix `[system]`;
//! that prefix's hash mints the session title. The prefix *grows* every turn,
//! so the mapping from "the prefix I see now" to "the title I minted at turn 1"
//! is carried in [`Titles`]: after each turn the bridge registers the prefixes
//! the *next* request would plausibly present (the transcript it just answered,
//! with and without the assistant reply appended) against the same title. A
//! client that rewrites history, or a bridge restart, simply misses the map and
//! starts a new session — degraded, never wrong.
//!
//! # The constraint that must not regress
//!
//! On the subscription path the prompt sent into a **resumed** session is
//! **only the newest message**. Re-sending the whole transcript into a session
//! that already contains it duplicates the conversation inside the session,
//! which is strictly worse than a one-off — and it *looks like it works* while
//! silently doubling context every turn. [`prompt_for`] is the single place
//! that decision lives, and it is pinned by tests.
//!
//! # Rotation — why a title is not forever (#667)
//!
//! A plugin with a stable persona and no history rewriting (pet, exactly) maps
//! to **one** title, and therefore one claude session that grows every turn and
//! that nothing prunes. Left alone it eventually returns `PromptTooLong`
//! **permanently**, recoverable only by deleting session state by hand.
//!
//! So a title carries a **generation**: `hytte-bridge-<hash>` is generation 0
//! and `hytte-bridge-<hash>-g1`, `-g2`, … are its successors. When a turn
//! overflows, the conversation is retired to the next generation — a title
//! nothing has ever resumed, so it is created fresh — and [`Titles`] pins the
//! successor so later turns of the same conversation go straight there.
//! [`rotation_for`] is that decision as a pure function, and it is pinned by
//! tests because nothing in CI can drive a real session into an overflow.
//!
//! Generation 0 renders with **no suffix**, so every session already on disk
//! keeps resolving to the title it was created under.
//!
//! # The hash
//!
//! **FNV-1a 64**, hand-rolled. `std::collections::hash_map::DefaultHasher` is
//! explicitly documented as *not* guaranteed stable across releases, and a
//! title that changes when the toolchain moves would silently orphan every
//! on-disk session. FNV-1a is a fixed published algorithm with no per-process
//! seed, so the same transcript maps to the same title forever. Each field is
//! length-prefixed before hashing so `["ab", "c"]` and `["a", "bc"]` cannot
//! collide by concatenation. A genuine 64-bit collision would resume the wrong
//! session — over a handful of local desktop conversations that is negligible,
//! and the failure is a confused reply rather than data loss.

use std::collections::{HashMap, VecDeque};

use crate::http::Failure;
use crate::wire::Message;

/// Prefix on every session title the bridge mints, so a human running
/// `claude --resume` can tell bridge-owned sessions from their own.
pub const TITLE_PREFIX: &str = "hytte-bridge-";

/// Separator between a title's base and its generation (see the module docs).
/// A hex digest never contains it, so the split is unambiguous.
const GENERATION_SEP: &str = "-g";

/// Width of the hex digest in a title, fixed by the `{:016x}` in [`title_for`].
const KEY_HEX_LEN: usize = 16;

/// The status [`crate::backend::map_error`] gives `hive_claude::Error::PromptTooLong`
/// — i.e. the one failure a rotation can fix.
///
/// Shared between the mapping and [`rotation_for`] (rather than written `413`
/// at both ends) so the link between the driver's typed sentinel and the
/// rotation trigger is one constant, and pinned by a test in `backend.rs`.
///
/// `http.rs` also answers 413 for an oversized *request body*, but that failure
/// is raised while reading the request and never reaches the turn path, so it
/// can never be mistaken for a full session.
pub const OVERFLOW_STATUS: u16 = 413;

/// FNV-1a 64 offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64 prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Which `hive-claude` attach a prompt is being built for. The bridge's own
/// mirror of the two `Attach` arms it uses, kept local so [`prompt_for`] is a
/// pure function that tests can drive without a `claude` binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachKind {
    /// `Attach::Resume(title)` — the session already holds the prefix.
    Resume,
    /// `Attach::Create(title)` — a brand-new session that holds nothing.
    Create,
}

/// Fold `bytes` into an FNV-1a 64 accumulator.
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A length, as the eight fixed bytes the hash feeds on.
///
/// Explicitly `u64` rather than `usize::to_le_bytes`, which is four bytes wide
/// on a 32-bit target — a title that changed with the pointer width would not
/// be the stable identity this whole module rests on. A length that does not
/// fit in a `u64` cannot exist on any real target; saturating is a formality.
fn len_bytes(len: usize) -> [u8; 8] {
    u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes()
}

/// Fold one length-prefixed field in, so field boundaries are part of the hash.
fn fnv1a_field(hash: u64, field: &str) -> u64 {
    let hash = fnv1a(hash, &len_bytes(field.len()));
    fnv1a(hash, field.as_bytes())
}

/// A restart-stable digest of an exact message sequence.
#[must_use]
pub fn transcript_key(messages: &[Message]) -> u64 {
    let mut hash = fnv1a(FNV_OFFSET, &len_bytes(messages.len()));
    for m in messages {
        hash = fnv1a_field(hash, &m.role);
        hash = fnv1a_field(hash, &m.content);
    }
    hash
}

/// The key a request looks itself up under: the hash of its **prefix**
/// (everything but the newest message).
///
/// A single-message request has an empty prefix; keying on that would collapse
/// every one-shot conversation in the process onto one title (and therefore one
/// claude session). So an empty prefix falls back to hashing the whole
/// transcript, which is still exactly "the context this conversation started
/// from".
#[must_use]
pub fn conversation_key(messages: &[Message]) -> u64 {
    match split_delta(messages) {
        Some((prefix, _)) if !prefix.is_empty() => transcript_key(prefix),
        _ => transcript_key(messages),
    }
}

/// The session title for a conversation key — generation 0, no suffix.
#[must_use]
pub fn title_for(key: u64) -> String {
    format!("{TITLE_PREFIX}{key:016x}")
}

/// What to do with a conversation whose turn just failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rotation {
    /// Not a context overflow. The failure stands and the session is healthy.
    Keep,
    /// The session is full: retire it and continue under this title.
    To(String),
    /// An overflow no rotation can fix, because the title is not one this
    /// module minted. (Generation exhaustion lands here too, but that needs
    /// ~4×10⁹ rotations — each of them a whole context window of turns — so it
    /// is a formality, not a case.)
    Stuck,
}

/// **The rotation decision**: given a failed turn and the title it ran under,
/// should the session be retired, and what does the conversation continue as?
///
/// Deliberately pure. Nothing in CI can drive a real claude session into an
/// overflow (there is no `claude` binary in the sandbox and a genuine overflow
/// takes ~10³ turns), so this is the only part of the recovery that *can* be
/// tested — and therefore the part every other piece is arranged around.
///
/// It keys on the [`Failure`] rather than on `hive_claude::Error` because the
/// typed sentinel is already funnelled through exactly one mapping
/// ([`crate::backend::map_error`], via [`OVERFLOW_STATUS`]) and because the
/// caller of this decision must sit *above* the backend — see
/// `Bridge::retire`'s docs for why the pin has to outlive a cancelled retry.
#[must_use]
pub fn rotation_for(failure: &Failure, title: &str) -> Rotation {
    if failure.status != OVERFLOW_STATUS {
        return Rotation::Keep;
    }
    next_title(title).map_or(Rotation::Stuck, Rotation::To)
}

/// The title a conversation continues under once `title`'s session is retired:
/// the same base at the next generation.
///
/// `None` for a title this module did not mint — there is no safe successor to
/// invent for a name we do not own, and inventing one could collide with a
/// human's own session.
#[must_use]
pub fn next_title(title: &str) -> Option<String> {
    let (base, generation) = split_generation(title)?;
    Some(format!(
        "{base}{GENERATION_SEP}{}",
        generation.checked_add(1)?
    ))
}

/// Split a bridge title into its base (`hytte-bridge-<16 hex>`) and generation.
/// Strict: anything that is not exactly the shape [`title_for`] and
/// [`next_title`] mint is rejected rather than guessed at.
fn split_generation(title: &str) -> Option<(&str, u32)> {
    let rest = title.strip_prefix(TITLE_PREFIX)?;
    let (hex, generation) = match rest.split_once(GENERATION_SEP) {
        Some((hex, suffix)) => (hex, suffix.parse::<u32>().ok()?),
        None => (rest, 0),
    };
    if hex.len() != KEY_HEX_LEN || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((&title[..TITLE_PREFIX.len() + KEY_HEX_LEN], generation))
}

/// Split a transcript into `(prefix, newest)`. `None` for an empty transcript —
/// a request with no messages is a bad request, not an empty conversation.
#[must_use]
pub fn split_delta(messages: &[Message]) -> Option<(&[Message], &Message)> {
    messages.split_last().map(|(last, rest)| (rest, last))
}

/// Render one message as prompt text. A `user` turn is its bare content (the
/// natural thing to type at claude); any other role is labelled, so a system
/// re-statement or a replayed assistant turn is not mistaken for the user
/// speaking.
fn render_message(m: &Message) -> String {
    if m.role == "user" {
        m.content.clone()
    } else {
        format!("[{}]\n{}", m.role, m.content)
    }
}

/// Render a whole transcript as one prompt — every turn labelled, blank line
/// separated. Used **only** when creating a session, which by definition holds
/// nothing yet.
#[must_use]
pub fn render_transcript(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| format!("[{}]\n{}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// **The delta rule.** Resuming a session sends only the newest message;
/// creating one sends the whole transcript.
///
/// This is the single most important function in the crate: sending the full
/// transcript on the resume arm duplicates the conversation inside the session
/// and doubles its context every turn, while still producing plausible replies.
#[must_use]
pub fn prompt_for(attach: AttachKind, transcript: &[Message], delta: &Message) -> String {
    match attach {
        AttachKind::Resume => render_message(delta),
        AttachKind::Create => render_transcript(transcript),
    }
}

/// A bounded, insertion-ordered `u64 → title` map.
///
/// Not a true LRU: the working set is a handful of desktop plugin
/// conversations, and evicting the oldest simply means the next turn of a
/// long-dormant conversation starts a fresh session.
#[derive(Debug)]
struct Bounded {
    cap: usize,
    by_key: HashMap<u64, String>,
    order: VecDeque<u64>,
}

impl Bounded {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            by_key: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: u64) -> Option<&String> {
        self.by_key.get(&key)
    }

    fn insert(&mut self, key: u64, title: &str) {
        if self.by_key.insert(key, title.to_owned()).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > self.cap {
            if let Some(evicted) = self.order.pop_front() {
                self.by_key.remove(&evicted);
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_key.len()
    }
}

/// The bridge's conversation → title map.
///
/// Two bounded maps, deliberately **not** one:
///
/// - `by_prefix` — "a transcript prefix I have seen" → "the title I answered it
///   under". Two entries are written per answered turn, so a chatty plugin
///   churns it: an entry survives roughly `cap/2` turns.
/// - `retired` — "this conversation's session was retired" → "the title it
///   continues under" (#667). Written **only** by [`Titles::rotate_to`], i.e.
///   about once per context window, so it is not churned out by the traffic it
///   has to outlive. Putting a retirement in `by_prefix` would let a rotation
///   be forgotten a hundred turns later, sending the conversation straight back
///   to the session that had already overflowed.
///
/// `retired` therefore wins on lookup: it is the only entry that records that a
/// session must never be resumed again, and a stale `by_prefix` entry pointing
/// at a retired session would overflow on every turn.
#[derive(Debug)]
pub struct Titles {
    by_prefix: Bounded,
    retired: Bounded,
}

impl Titles {
    /// A map holding at most `cap` conversations.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            by_prefix: Bounded::new(cap),
            retired: Bounded::new(cap),
        }
    }

    /// The title for this request: the successor of a session this
    /// conversation retired, else the one already minted for the conversation
    /// it continues, else a fresh one derived from its own prefix.
    #[must_use]
    pub fn resolve(&self, messages: &[Message]) -> String {
        let key = conversation_key(messages);
        self.retired
            .get(key)
            .or_else(|| self.by_prefix.get(key))
            .cloned()
            .unwrap_or_else(|| title_for(key))
    }

    /// Register the prefixes the *next* request of this conversation would
    /// present, so the turn after it resolves to the same `title`.
    ///
    /// Two forward keys, because clients differ in how they carry history:
    /// - `messages ++ assistant(reply)` — the standard `OpenAI` loop, which
    ///   echoes the assistant turn back;
    /// - `messages` — a client that appends only its own next user message.
    pub fn remember(&mut self, title: &str, messages: &[Message], reply: &str) {
        let mut echoed = messages.to_vec();
        echoed.push(Message::assistant(reply));
        self.by_prefix.insert(transcript_key(&echoed), title);
        self.by_prefix.insert(transcript_key(messages), title);
    }

    /// Record that this conversation's session has been retired and that it now
    /// lives under `title` (#667).
    ///
    /// Keyed on the **conversation key**, not on a forward prefix: a plugin
    /// like pet re-sends the same persona prefix with a different newest
    /// message every turn, so the conversation key is the only thing about it
    /// that is stable — and it is exactly what [`Titles::resolve`] looks up.
    pub fn rotate_to(&mut self, messages: &[Message], title: &str) {
        self.retired.insert(conversation_key(messages), title);
    }

    /// How many prefixes and retirements are currently mapped. Test-only: the
    /// bound is the thing worth asserting, and nothing in the running bridge
    /// needs to ask.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_prefix.len() + self.retired.len()
    }

    /// Whether nothing is mapped yet. Test-only, as [`Titles::len`].
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AttachKind, OVERFLOW_STATUS, Rotation, TITLE_PREFIX, Titles, conversation_key, next_title,
        prompt_for, render_transcript, rotation_for, split_delta, title_for, transcript_key,
    };
    use crate::http::Failure;
    use crate::wire::Message;

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.to_owned(),
            content: content.to_owned(),
        }
    }

    /// The hash must be stable across *builds and restarts*, not merely within
    /// one process — a title that moves orphans every on-disk claude session.
    /// This literal is the regression pin: swapping in `DefaultHasher` (whose
    /// output carries no stability guarantee) or reordering the field feed
    /// breaks it.
    #[test]
    fn title_derivation_is_pinned_to_a_literal() {
        let transcript = [msg("system", "you are a cat"), msg("user", "poke")];
        assert_eq!(
            title_for(conversation_key(&transcript)),
            "hytte-bridge-5a36669b9a22d7cc"
        );
    }

    /// Same prefix ⇒ same title, run after run.
    #[test]
    fn same_prefix_yields_the_same_title() {
        let a = [msg("system", "persona"), msg("user", "one")];
        let b = [msg("system", "persona"), msg("user", "two")];
        // Different newest message, identical prefix — same conversation.
        assert_eq!(conversation_key(&a), conversation_key(&b));
        assert!(title_for(conversation_key(&a)).starts_with(TITLE_PREFIX));
    }

    /// A changed prefix is a different conversation.
    #[test]
    fn changed_prefix_yields_a_different_title() {
        let a = [msg("system", "persona"), msg("user", "one")];
        let b = [msg("system", "OTHER persona"), msg("user", "one")];
        assert_ne!(conversation_key(&a), conversation_key(&b));
    }

    /// Length-prefixing each field: `["ab","c"]` must not hash like
    /// `["a","bc"]`.
    #[test]
    fn field_boundaries_are_part_of_the_hash() {
        assert_ne!(
            transcript_key(&[msg("user", "ab"), msg("user", "c")]),
            transcript_key(&[msg("user", "a"), msg("user", "bc")])
        );
        assert_ne!(
            transcript_key(&[msg("user", "ab")]),
            transcript_key(&[msg("us", "erab")])
        );
    }

    /// A single-message request has an empty prefix; keying on the empty slice
    /// would collapse every one-shot onto one session.
    #[test]
    fn single_message_conversations_do_not_collide() {
        let a = [msg("user", "what time is it")];
        let b = [msg("user", "how is the weather")];
        assert_ne!(conversation_key(&a), conversation_key(&b));
    }

    /// An empty transcript has no delta — the caller must 400 it.
    #[test]
    fn empty_transcript_has_no_delta() {
        assert!(split_delta(&[]).is_none());
    }

    /// **The delta constraint.** A resumed session receives ONLY the newest
    /// message: none of the earlier turns may appear in the prompt.
    #[test]
    fn resume_sends_only_the_newest_message() {
        let transcript = [
            msg("system", "PERSONA-MARKER"),
            msg("user", "FIRST-MARKER"),
            msg("assistant", "REPLY-MARKER"),
            msg("user", "NEWEST-MARKER"),
        ];
        let (_, delta) = split_delta(&transcript).expect("non-empty");
        let prompt = prompt_for(AttachKind::Resume, &transcript, delta);

        assert_eq!(prompt, "NEWEST-MARKER");
        assert!(!prompt.contains("PERSONA-MARKER"));
        assert!(!prompt.contains("FIRST-MARKER"));
        assert!(!prompt.contains("REPLY-MARKER"));
    }

    /// A non-`user` newest message is labelled, but is still the ONLY thing
    /// sent on a resume.
    #[test]
    fn resume_labels_a_non_user_delta_but_still_sends_only_it() {
        let transcript = [msg("user", "OLD-MARKER"), msg("system", "NEW-RULES")];
        let (_, delta) = split_delta(&transcript).expect("non-empty");
        let prompt = prompt_for(AttachKind::Resume, &transcript, delta);
        assert_eq!(prompt, "[system]\nNEW-RULES");
        assert!(!prompt.contains("OLD-MARKER"));
    }

    /// Creating a session holds nothing yet, so it gets the whole transcript —
    /// the other arm of the same decision.
    #[test]
    fn create_sends_the_whole_transcript() {
        let transcript = [
            msg("system", "PERSONA-MARKER"),
            msg("user", "FIRST-MARKER"),
            msg("assistant", "REPLY-MARKER"),
            msg("user", "NEWEST-MARKER"),
        ];
        let (_, delta) = split_delta(&transcript).expect("non-empty");
        let prompt = prompt_for(AttachKind::Create, &transcript, delta);
        for marker in [
            "PERSONA-MARKER",
            "FIRST-MARKER",
            "REPLY-MARKER",
            "NEWEST-MARKER",
        ] {
            assert!(prompt.contains(marker), "create prompt lost {marker}");
        }
        assert!(prompt.contains("[system]"));
        assert!(prompt.contains("[assistant]"));
    }

    /// Every turn of one conversation must resolve to the title minted at turn
    /// 1 — otherwise each turn would create a new session and the resume path
    /// would never fire.
    #[test]
    fn continuation_resolves_to_the_root_title() {
        let mut titles = Titles::new(64);

        let turn1 = vec![msg("system", "persona"), msg("user", "hello")];
        let root = titles.resolve(&turn1);
        titles.remember(&root, &turn1, "hi there");

        // Standard OpenAI loop: the client echoes the assistant reply back.
        let mut turn2 = turn1.clone();
        turn2.push(msg("assistant", "hi there"));
        turn2.push(msg("user", "again"));
        assert_eq!(titles.resolve(&turn2), root);

        titles.remember(&root, &turn2, "sure");
        let mut turn3 = turn2.clone();
        turn3.push(msg("assistant", "sure"));
        turn3.push(msg("user", "and again"));
        assert_eq!(titles.resolve(&turn3), root);
    }

    /// A client that appends its next user message *without* echoing the
    /// assistant turn still continues the same session.
    #[test]
    fn continuation_without_an_echoed_reply_still_resolves() {
        let mut titles = Titles::new(64);
        let turn1 = vec![msg("system", "persona"), msg("user", "hello")];
        let root = titles.resolve(&turn1);
        titles.remember(&root, &turn1, "hi there");

        let mut turn2 = turn1.clone();
        turn2.push(msg("user", "again"));
        assert_eq!(titles.resolve(&turn2), root);
    }

    /// A rewritten history is a different conversation — degraded (a fresh
    /// session), never a wrong resume.
    #[test]
    fn rewritten_history_forks_a_new_conversation() {
        let mut titles = Titles::new(64);
        let turn1 = vec![msg("system", "persona"), msg("user", "hello")];
        let root = titles.resolve(&turn1);
        titles.remember(&root, &turn1, "hi there");

        let forked = vec![
            msg("system", "persona"),
            msg("user", "hello"),
            msg("assistant", "SOMETHING ELSE ENTIRELY"),
            msg("user", "again"),
        ];
        assert_ne!(titles.resolve(&forked), root);
    }

    /// An unmapped request mints its title deterministically rather than
    /// erroring — a bridge restart must not break a live conversation any worse
    /// than starting a new session.
    #[test]
    fn an_unknown_prefix_mints_deterministically() {
        let titles = Titles::new(4);
        let convo = vec![msg("system", "persona"), msg("user", "hello")];
        assert_eq!(titles.resolve(&convo), titles.resolve(&convo));
        assert!(titles.is_empty());
    }

    /// The map is bounded — a chatty plugin cannot grow it without limit.
    #[test]
    fn the_map_is_bounded() {
        let mut titles = Titles::new(4);
        for n in 0..50 {
            let convo = vec![msg("user", &format!("conversation {n}"))];
            let title = titles.resolve(&convo);
            titles.remember(&title, &convo, "ok");
        }
        assert!(titles.len() <= 4, "map grew to {}", titles.len());
    }

    // ── rotation (#667) ────────────────────────────────────────────────────

    /// A failure that is not a context overflow leaves the session alone. This
    /// is the arm that runs on every rate-limit, auth failure and spawn error,
    /// so getting it wrong would retire a healthy session on the first blip.
    #[test]
    fn a_non_overflow_failure_does_not_rotate() {
        let title = title_for(conversation_key(&[msg("system", "persona")]));
        for status in [429, 502, 503, 504, 400] {
            assert_eq!(
                rotation_for(&Failure::new(status, "nope"), &title),
                Rotation::Keep,
                "status {status} rotated"
            );
        }
    }

    /// The first overflow moves a generation-0 title to `-g1`.
    #[test]
    fn an_overflow_retires_the_session_to_the_next_generation() {
        let key = conversation_key(&[msg("system", "you are a cat"), msg("user", "poke")]);
        let title = title_for(key);
        assert_eq!(
            rotation_for(&Failure::new(OVERFLOW_STATUS, "too long"), &title),
            Rotation::To(format!("{title}-g1"))
        );
    }

    /// Rotation chains: a session that fills up again moves on again, rather
    /// than bouncing back to the generation-0 title that is already full.
    #[test]
    fn rotation_chains_across_generations() {
        let base = title_for(conversation_key(&[msg("system", "persona")]));
        let mut title = base.clone();
        for generation in 1..=4u32 {
            let Rotation::To(next) = rotation_for(&Failure::new(OVERFLOW_STATUS, "x"), &title)
            else {
                panic!("generation {generation} refused to rotate");
            };
            assert_eq!(next, format!("{base}-g{generation}"));
            title = next;
        }
    }

    /// A rotated title is still recognisably bridge-owned, and never collides
    /// with the generation-0 title it replaced.
    #[test]
    fn a_rotated_title_is_distinct_and_still_bridge_owned() {
        let title = title_for(conversation_key(&[msg("user", "hello")]));
        let next = next_title(&title).expect("rotatable");
        assert_ne!(next, title);
        assert!(next.starts_with(TITLE_PREFIX));
    }

    /// A title the bridge did not mint has no safe successor to invent — an
    /// overflow on one is reported, not guessed at. (It cannot arise from
    /// `resolve`; this is the arm that keeps it that way.)
    #[test]
    fn a_foreign_title_cannot_be_rotated() {
        for foreign in [
            "my-own-session",
            "hytte-bridge-",
            "hytte-bridge-nothex0123456789",
            "hytte-bridge-5a36669b9a22d7c",   // 15 hex digits
            "hytte-bridge-5a36669b9a22d7cc0", // 17
            "hytte-bridge-5a36669b9a22d7cc-g",
            "hytte-bridge-5a36669b9a22d7cc-gx",
            "hytte-bridge-5a36669b9a22d7cc-g-1",
            "hytte-bridge-5a36669b9a22d7cc-g1-g2",
        ] {
            assert_eq!(next_title(foreign), None, "{foreign} was rotated");
            assert_eq!(
                rotation_for(&Failure::new(OVERFLOW_STATUS, "x"), foreign),
                Rotation::Stuck,
                "{foreign}"
            );
        }
    }

    /// After a retirement the conversation resolves to the replacement — the
    /// whole point, since pet re-sends the same prefix every turn and would
    /// otherwise walk straight back into the session that just overflowed.
    #[test]
    fn a_retired_conversation_resolves_to_its_replacement() {
        let mut titles = Titles::new(64);
        let convo = vec![msg("system", "persona"), msg("user", "poke")];
        let title = titles.resolve(&convo);
        let Rotation::To(next) = rotation_for(&Failure::new(OVERFLOW_STATUS, "x"), &title) else {
            panic!("not rotated");
        };
        titles.rotate_to(&convo, &next);

        assert_eq!(titles.resolve(&convo), next);
        // A *different* newest message is the same conversation, so it must
        // land in the replacement too.
        let later = vec![msg("system", "persona"), msg("user", "poke again")];
        assert_eq!(titles.resolve(&later), next);
    }

    /// A retirement must outlive the traffic that follows it. `remember`
    /// writes two entries per turn, so if retirements shared that map a busy
    /// plugin would evict its own rotation within `cap/2` turns and start
    /// overflowing all over again — once every hundred quips, for ever.
    #[test]
    fn a_retirement_survives_the_churn_of_later_turns() {
        let mut titles = Titles::new(8);
        let convo = vec![msg("system", "persona"), msg("user", "poke")];
        let retired = format!("{}-g1", titles.resolve(&convo));
        titles.rotate_to(&convo, &retired);

        for n in 0..200 {
            let other = vec![msg("system", "persona"), msg("user", &format!("poke {n}"))];
            titles.remember(&retired, &other, "quip");
        }
        assert_eq!(titles.resolve(&convo), retired);
    }

    /// Retirements are bounded like everything else — a client that forks a new
    /// conversation per request cannot grow the map without limit.
    #[test]
    fn the_retirement_map_is_bounded() {
        let mut titles = Titles::new(4);
        for n in 0..50 {
            let convo = vec![msg("user", &format!("conversation {n}"))];
            let next = format!("{}-g1", titles.resolve(&convo));
            titles.rotate_to(&convo, &next);
        }
        assert!(titles.len() <= 8, "map grew to {}", titles.len());
    }

    /// `render_transcript` labels every role and separates turns.
    #[test]
    fn transcript_rendering_is_labelled() {
        let rendered = render_transcript(&[msg("system", "s"), msg("user", "u")]);
        assert_eq!(rendered, "[system]\ns\n\n[user]\nu");
    }
}
