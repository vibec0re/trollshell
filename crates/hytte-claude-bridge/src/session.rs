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

use crate::wire::Message;

/// Prefix on every session title the bridge mints, so a human running
/// `claude --resume` can tell bridge-owned sessions from their own.
pub const TITLE_PREFIX: &str = "hytte-bridge-";

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

/// The session title for a conversation key.
#[must_use]
pub fn title_for(key: u64) -> String {
    format!("{TITLE_PREFIX}{key:016x}")
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

/// The bridge's map from "a transcript prefix I have seen" to "the title I
/// minted for that conversation".
///
/// Bounded and insertion-ordered rather than a true LRU: the working set is a
/// handful of desktop plugin conversations, and evicting the oldest simply
/// means the next turn of a long-dormant conversation starts a fresh session.
#[derive(Debug)]
pub struct Titles {
    cap: usize,
    by_key: HashMap<u64, String>,
    order: VecDeque<u64>,
}

impl Titles {
    /// A map holding at most `cap` conversations.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            by_key: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// The title for this request: the one already minted for the conversation
    /// it continues, or a fresh one derived from its own prefix.
    #[must_use]
    pub fn resolve(&self, messages: &[Message]) -> String {
        let key = conversation_key(messages);
        self.by_key
            .get(&key)
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
        self.insert(transcript_key(&echoed), title);
        self.insert(transcript_key(messages), title);
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

    /// How many prefixes are currently mapped. Test-only: the map's bound is
    /// the thing worth asserting, and nothing in the running bridge needs to
    /// ask.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Whether no prefix is mapped yet. Test-only, as [`Titles::len`].
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AttachKind, TITLE_PREFIX, Titles, conversation_key, prompt_for, render_transcript,
        split_delta, title_for, transcript_key,
    };
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

    /// `render_transcript` labels every role and separates turns.
    #[test]
    fn transcript_rendering_is_labelled() {
        let rendered = render_transcript(&[msg("system", "s"), msg("user", "u")]);
        assert_eq!(rendered, "[system]\ns\n\n[user]\nu");
    }
}
