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
//! # Explicit identity (#704)
//!
//! That derivation is a *fallback*, and it is only ever as good as its luck:
//! two plugins whose transcripts happen to agree land on one title, and #693
//! measured what that costs — concurrent `claude --resume` calls do not
//! serialise, they fork the session, silently. So a client may instead **say
//! who it is**, in `OpenAI`'s own `user` field ([`crate::wire::ChatRequest`]),
//! and the title is derived from that rather than from the transcript.
//!
//! The two derivations live in **disjoint** title spaces, and the disjointness
//! is structural rather than probabilistic — see [`Key`]. That distinction is
//! the point: a caller picks its own `user` string, so were the spaces merely
//! adjacent it could name itself into another conversation's session. It
//! cannot, at any string it is able to pick.
//!
//! **An absent `user` is byte-identical to the pre-#704 behaviour** — every
//! function below reduces to exactly what it computed before, which is what
//! makes this safe to land ahead of any client opting in, and is pinned by
//! `an_absent_identity_is_byte_identical_to_the_hash_path`.
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

/// Marks a title whose digest came from a caller-supplied identity rather than
/// from a transcript (#704). Sits between [`TITLE_PREFIX`] and the digest, so
/// an identity title reads `hytte-bridge-u-<16 hex>`.
///
/// `u` is not a hex digit, which is the whole mechanism: a hash-derived title
/// has exactly [`KEY_HEX_LEN`] hex characters after the prefix, so no
/// transcript can ever produce a string starting `u-` there and no identity
/// title can ever be mistaken for one. See [`Key`].
const USER_MARKER: &str = "u-";

/// Domain tag folded in ahead of a caller-supplied identity, so the digest of
/// the identity `"pet"` is not the digest of anything else that happens to
/// hash one field. Belt to [`USER_MARKER`]'s braces: the marker already makes
/// the two title spaces disjoint, and this makes their *digests* differ too.
const USER_DOMAIN: &str = "identity";

/// Width of the hex digest in a title, fixed by the `{:016x}` in [`title_for`].
const KEY_HEX_LEN: usize = 16;

/// The status [`crate::backend::map_error`] gives `hive_claude::Error::PromptTooLong`
/// — i.e. the one failure a rotation can fix.
///
/// Shared between the mapping and [`rotation_for`] (rather than written `413`
/// at both ends) so the link between the driver's typed sentinel and the
/// rotation trigger is one constant, and pinned by a test in `backend.rs`.
///
/// [`crate::messages::map_status`] reports the same status for the API
/// backend's equivalent condition, so a plugin sees one failure vocabulary
/// whichever backend answered. Nothing rotates on it there — that backend is
/// not persisted — so it is a label for the human, not a trigger.
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

/// A digest **paired with the space it was computed in** (#704).
///
/// The bridge derives conversation identity two ways — from the transcript, or
/// from a caller-supplied `user` — and the two must never be confused, because
/// the caller controls one of them and not the other. Carrying the space in the
/// type rather than in a naming convention means the separation is enforced by
/// construction at every point it matters:
///
/// - **In a title.** [`title_for`] renders `Transcript` as `hytte-bridge-<hex>`
///   and `User` as `hytte-bridge-u-<hex>`. A hash title's digest is exactly
///   [`KEY_HEX_LEN`] *hex* characters, and `u` is not one, so the two renderings
///   cannot coincide — for *any* identity string, not merely for likely ones.
///   This is the security-relevant half: titles are the on-disk names that
///   `claude --resume` resolves against, shared with the human's own sessions.
/// - **In the maps.** [`Titles`]' two [`Bounded`] maps are keyed on `Key`, so a
///   64-bit collision between an identity digest and a transcript digest is not
///   a lookup hit — the variants differ, and `Eq` compares them.
///
/// Note what this deliberately does *not* claim: two different `user` strings
/// still share one space, and collide on a genuine 64-bit FNV collision, same
/// as two transcripts do. That is not a weakness worth closing here, because a
/// caller who wants another caller's session can simply *send its `user`
/// string* — identity is asserted, never authenticated, exactly as it is in the
/// `OpenAI` field this borrows. The bridge listens on loopback only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// Derived from the conversation's transcript — the pre-#704 fallback.
    Transcript(u64),
    /// Derived from the caller's own `user` identity (#704).
    User(u64),
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

/// A restart-stable digest of a caller-supplied identity (#704).
///
/// Hashed rather than embedded verbatim, for three reasons: a title must be a
/// bounded, well-behaved string whatever the caller sends; an identity
/// containing [`GENERATION_SEP`] would otherwise be indistinguishable from
/// another identity's *rotated* title (the caller `"pet-g1"` would land on the
/// second generation of `"pet"`); and a fixed-width digest keeps
/// [`split_generation`] the same strict shape check for both spaces.
#[must_use]
pub fn user_key(user: &str) -> u64 {
    fnv1a_field(fnv1a_field(FNV_OFFSET, USER_DOMAIN), user)
}

/// Normalise a caller-supplied identity: **blank is absent.**
///
/// `Some("")` and `Some("   ")` are not identities, they are a client filling
/// the field in without meaning anything by it — and taking them at face value
/// would be worse than the hash fallback they displace, because *every* such
/// client would share one session. That is the cross-caller bleed #704 exists
/// to close, arriving through the front door.
///
/// Blank ≡ absent is the idiom everywhere else in this tree: `env_nonempty`,
/// the plugins' `resolve_provider` (`Some("")` = unset), `owner_or`'s trim.
///
/// A non-blank identity is passed through **untrimmed**: whitespace inside or
/// around it is part of the string the caller chose, and trimming would quietly
/// merge `"pet"` with `" pet"` — two callers who did not ask to be one.
#[must_use]
pub fn identity(user: Option<&str>) -> Option<&str> {
    user.filter(|u| !u.trim().is_empty())
}

/// **The identity decision.** Which conversation a request belongs to: the one
/// it named, else the one its transcript implies (#704).
///
/// With `user` absent — or blank, which [`identity`] treats the same — this is
/// exactly [`conversation_key`] in a `Transcript` wrapper: the pre-#704 value,
/// unchanged.
#[must_use]
pub fn identity_key(messages: &[Message], user: Option<&str>) -> Key {
    match identity(user) {
        Some(user) => Key::User(user_key(user)),
        None => Key::Transcript(conversation_key(messages)),
    }
}

/// The key one *turn* collapses onto for single-flight (`Bridge::complete`).
///
/// Distinct from [`identity_key`]: that names the conversation across turns,
/// this names one exact prompt within it, so two different questions from one
/// identity are still two turns. It folds **both**, because sharing a turn is
/// sharing an answer — and after #704 two callers with the same transcript are
/// two conversations in two sessions, so handing one the other's reply would
/// reintroduce, at the answer, precisely the cross-caller bleed the identity
/// removed at the session.
///
/// With `user` absent — or blank, per [`identity`] — this is exactly
/// [`transcript_key`], as before #704. (Its `Key::User` digests share a variant
/// with [`identity_key`]'s but never a map — `inflight` and [`Titles`] are
/// separate — so the two never meet.)
#[must_use]
pub fn flight_key(messages: &[Message], user: Option<&str>) -> Key {
    match identity(user) {
        Some(user) => Key::User(fnv1a(
            user_key(user),
            &transcript_key(messages).to_le_bytes(),
        )),
        None => Key::Transcript(transcript_key(messages)),
    }
}

/// The session title for a conversation key — generation 0, no suffix.
///
/// The two [`Key`] variants render into disjoint spaces; see that type for why
/// the disjointness is structural and why it has to be.
#[must_use]
pub fn title_for(key: Key) -> String {
    match key {
        Key::Transcript(key) => format!("{TITLE_PREFIX}{key:016x}"),
        Key::User(key) => format!("{TITLE_PREFIX}{USER_MARKER}{key:016x}"),
    }
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

/// Split a bridge title into its base (`hytte-bridge-[u-]<16 hex>`) and
/// generation. Strict: anything that is not exactly the shape [`title_for`] and
/// [`next_title`] mint is rejected rather than guessed at.
///
/// Both spaces are accepted, deliberately. A title carrying an explicit
/// identity fills up exactly like a hash-derived one, and #667's rotation is
/// the only thing standing between a long-lived session and a permanent
/// `PromptTooLong`; refusing to rotate the identity space would have made
/// opting in to #704 a downgrade.
fn split_generation(title: &str) -> Option<(&str, u32)> {
    let rest = title.strip_prefix(TITLE_PREFIX)?;
    // `u` is not a hex digit, so this never strips part of a hash digest.
    let (marker, rest) = match rest.strip_prefix(USER_MARKER) {
        Some(after) => (USER_MARKER.len(), after),
        None => (0, rest),
    };
    let (hex, generation) = match rest.split_once(GENERATION_SEP) {
        Some((hex, suffix)) => (hex, suffix.parse::<u32>().ok()?),
        None => (rest, 0),
    };
    if hex.len() != KEY_HEX_LEN || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        &title[..TITLE_PREFIX.len() + marker + KEY_HEX_LEN],
        generation,
    ))
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

/// A bounded, insertion-ordered [`Key`] → title map.
///
/// Not a true LRU: the working set is a handful of desktop plugin
/// conversations, and evicting the oldest simply means the next turn of a
/// long-dormant conversation starts a fresh session.
///
/// Keyed on [`Key`] rather than a bare `u64` (#704) so an entry written in one
/// identity space can never be *read* from the other, whatever the digests do.
#[derive(Debug)]
struct Bounded {
    cap: usize,
    by_key: HashMap<Key, String>,
    order: VecDeque<Key>,
}

impl Bounded {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            by_key: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: Key) -> Option<&String> {
        self.by_key.get(&key)
    }

    fn insert(&mut self, key: Key, title: &str) {
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
    /// it continues, else a fresh one derived from its own identity.
    ///
    /// `user` is the caller's `OpenAI` identity when it sent one (#704). With
    /// `None` this is the pre-#704 function exactly: the prefix hash, the same
    /// two lookups, the same minting.
    #[must_use]
    pub fn resolve(&self, messages: &[Message], user: Option<&str>) -> String {
        let key = identity_key(messages, user);
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
    ///
    /// A caller that supplied its own identity (#704) needs none of this and
    /// writes nothing: its next turn resolves from the identity alone, so there
    /// is no minted title to recover. Writing the prefixes anyway would be
    /// worse than redundant — a later request from a *different*, identity-less
    /// caller presenting the same transcript would find this entry and inherit
    /// this conversation's session, which is exactly the cross-caller bleed the
    /// identity was sent to prevent, and would break the no-op guarantee for
    /// clients that never opted in.
    pub fn remember(&mut self, title: &str, messages: &[Message], user: Option<&str>, reply: &str) {
        // [`identity`], not `user.is_some()`: a blank `user` resolved in the
        // transcript space, so it must leave the forward prefixes that space
        // needs — otherwise it would mint a fresh title every single turn.
        if identity(user).is_some() {
            return;
        }
        let mut echoed = messages.to_vec();
        echoed.push(Message::assistant(reply));
        self.by_prefix
            .insert(Key::Transcript(transcript_key(&echoed)), title);
        self.by_prefix
            .insert(Key::Transcript(transcript_key(messages)), title);
    }

    /// Record that this conversation's session has been retired and that it now
    /// lives under `title` (#667).
    ///
    /// Keyed on the **conversation key**, not on a forward prefix: a plugin
    /// like pet re-sends the same persona prefix with a different newest
    /// message every turn, so the conversation key is the only thing about it
    /// that is stable — and it is exactly what [`Titles::resolve`] looks up.
    ///
    /// For a caller with an explicit identity (#704) that stable thing is the
    /// identity instead, which is what [`identity_key`] returns; the retirement
    /// is recorded in that space and read back from it.
    pub fn rotate_to(&mut self, messages: &[Message], user: Option<&str>, title: &str) {
        self.retired.insert(identity_key(messages, user), title);
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
        AttachKind, Key, OVERFLOW_STATUS, Rotation, TITLE_PREFIX, Titles, USER_MARKER,
        conversation_key, flight_key, identity, identity_key, next_title, prompt_for,
        render_transcript, rotation_for, split_delta, title_for, transcript_key, user_key,
    };
    use crate::http::Failure;
    use crate::wire::Message;

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.to_owned(),
            content: content.to_owned(),
        }
    }

    /// The generation-0 title a transcript-derived conversation mints — i.e.
    /// the whole of the pre-#704 derivation, spelled once.
    fn hash_title(messages: &[Message]) -> String {
        title_for(Key::Transcript(conversation_key(messages)))
    }

    /// The hash must be stable across *builds and restarts*, not merely within
    /// one process — a title that moves orphans every on-disk claude session.
    /// This literal is the regression pin: swapping in `DefaultHasher` (whose
    /// output carries no stability guarantee) or reordering the field feed
    /// breaks it.
    #[test]
    fn title_derivation_is_pinned_to_a_literal() {
        let transcript = [msg("system", "you are a cat"), msg("user", "poke")];
        assert_eq!(hash_title(&transcript), "hytte-bridge-5a36669b9a22d7cc");
    }

    /// Same prefix ⇒ same title, run after run.
    #[test]
    fn same_prefix_yields_the_same_title() {
        let a = [msg("system", "persona"), msg("user", "one")];
        let b = [msg("system", "persona"), msg("user", "two")];
        // Different newest message, identical prefix — same conversation.
        assert_eq!(conversation_key(&a), conversation_key(&b));
        assert!(hash_title(&a).starts_with(TITLE_PREFIX));
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
        let root = titles.resolve(&turn1, None);
        titles.remember(&root, &turn1, None, "hi there");

        // Standard OpenAI loop: the client echoes the assistant reply back.
        let mut turn2 = turn1.clone();
        turn2.push(msg("assistant", "hi there"));
        turn2.push(msg("user", "again"));
        assert_eq!(titles.resolve(&turn2, None), root);

        titles.remember(&root, &turn2, None, "sure");
        let mut turn3 = turn2.clone();
        turn3.push(msg("assistant", "sure"));
        turn3.push(msg("user", "and again"));
        assert_eq!(titles.resolve(&turn3, None), root);
    }

    /// A client that appends its next user message *without* echoing the
    /// assistant turn still continues the same session.
    #[test]
    fn continuation_without_an_echoed_reply_still_resolves() {
        let mut titles = Titles::new(64);
        let turn1 = vec![msg("system", "persona"), msg("user", "hello")];
        let root = titles.resolve(&turn1, None);
        titles.remember(&root, &turn1, None, "hi there");

        let mut turn2 = turn1.clone();
        turn2.push(msg("user", "again"));
        assert_eq!(titles.resolve(&turn2, None), root);
    }

    /// A rewritten history is a different conversation — degraded (a fresh
    /// session), never a wrong resume.
    #[test]
    fn rewritten_history_forks_a_new_conversation() {
        let mut titles = Titles::new(64);
        let turn1 = vec![msg("system", "persona"), msg("user", "hello")];
        let root = titles.resolve(&turn1, None);
        titles.remember(&root, &turn1, None, "hi there");

        let forked = vec![
            msg("system", "persona"),
            msg("user", "hello"),
            msg("assistant", "SOMETHING ELSE ENTIRELY"),
            msg("user", "again"),
        ];
        assert_ne!(titles.resolve(&forked, None), root);
    }

    /// An unmapped request mints its title deterministically rather than
    /// erroring — a bridge restart must not break a live conversation any worse
    /// than starting a new session.
    #[test]
    fn an_unknown_prefix_mints_deterministically() {
        let titles = Titles::new(4);
        let convo = vec![msg("system", "persona"), msg("user", "hello")];
        assert_eq!(titles.resolve(&convo, None), titles.resolve(&convo, None));
        assert!(titles.is_empty());
    }

    /// The map is bounded — a chatty plugin cannot grow it without limit.
    #[test]
    fn the_map_is_bounded() {
        let mut titles = Titles::new(4);
        for n in 0..50 {
            let convo = vec![msg("user", &format!("conversation {n}"))];
            let title = titles.resolve(&convo, None);
            titles.remember(&title, &convo, None, "ok");
        }
        assert!(titles.len() <= 4, "map grew to {}", titles.len());
    }

    // ── rotation (#667) ────────────────────────────────────────────────────

    /// A failure that is not a context overflow leaves the session alone. This
    /// is the arm that runs on every rate-limit, auth failure and spawn error,
    /// so getting it wrong would retire a healthy session on the first blip.
    #[test]
    fn a_non_overflow_failure_does_not_rotate() {
        let title = hash_title(&[msg("system", "persona")]);
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
        let title = title_for(Key::Transcript(key));
        assert_eq!(
            rotation_for(&Failure::new(OVERFLOW_STATUS, "too long"), &title),
            Rotation::To(format!("{title}-g1"))
        );
    }

    /// Rotation chains: a session that fills up again moves on again, rather
    /// than bouncing back to the generation-0 title that is already full.
    #[test]
    fn rotation_chains_across_generations() {
        let base = hash_title(&[msg("system", "persona")]);
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
        let title = hash_title(&[msg("user", "hello")]);
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
        let title = titles.resolve(&convo, None);
        let Rotation::To(next) = rotation_for(&Failure::new(OVERFLOW_STATUS, "x"), &title) else {
            panic!("not rotated");
        };
        titles.rotate_to(&convo, None, &next);

        assert_eq!(titles.resolve(&convo, None), next);
        // A *different* newest message is the same conversation, so it must
        // land in the replacement too.
        let later = vec![msg("system", "persona"), msg("user", "poke again")];
        assert_eq!(titles.resolve(&later, None), next);
    }

    /// A retirement must outlive the traffic that follows it. `remember`
    /// writes two entries per turn, so if retirements shared that map a busy
    /// plugin would evict its own rotation within `cap/2` turns and start
    /// overflowing all over again — once every hundred quips, for ever.
    #[test]
    fn a_retirement_survives_the_churn_of_later_turns() {
        let mut titles = Titles::new(8);
        let convo = vec![msg("system", "persona"), msg("user", "poke")];
        let retired = format!("{}-g1", titles.resolve(&convo, None));
        titles.rotate_to(&convo, None, &retired);

        for n in 0..200 {
            let other = vec![msg("system", "persona"), msg("user", &format!("poke {n}"))];
            titles.remember(&retired, &other, None, "quip");
        }
        assert_eq!(titles.resolve(&convo, None), retired);
    }

    /// Retirements are bounded like everything else — a client that forks a new
    /// conversation per request cannot grow the map without limit.
    #[test]
    fn the_retirement_map_is_bounded() {
        let mut titles = Titles::new(4);
        for n in 0..50 {
            let convo = vec![msg("user", &format!("conversation {n}"))];
            let next = format!("{}-g1", titles.resolve(&convo, None));
            titles.rotate_to(&convo, None, &next);
        }
        assert!(titles.len() <= 8, "map grew to {}", titles.len());
    }

    /// `render_transcript` labels every role and separates turns.
    #[test]
    fn transcript_rendering_is_labelled() {
        let rendered = render_transcript(&[msg("system", "s"), msg("user", "u")]);
        assert_eq!(rendered, "[system]\ns\n\n[user]\nu");
    }

    // ── explicit identity (#704) ───────────────────────────────────────────

    /// Whether `title` lives in the **transcript** space — the space every
    /// pre-#704 session on disk was minted into, and the one a caller must
    /// never be able to reach.
    fn in_transcript_space(title: &str) -> bool {
        title
            .strip_prefix(TITLE_PREFIX)
            .is_some_and(|rest| !rest.starts_with(USER_MARKER))
    }

    /// **The backward-compatibility pin.** With no `user`, every part of the
    /// derivation must produce exactly the pre-#704 value — not merely an
    /// equivalent one.
    ///
    /// This is the test the whole change rests on: it is what makes #704 safe
    /// to land before any client opts in, because it says in one place that a
    /// client which never sends `user` cannot tell the difference. The title
    /// literal is the same one `title_derivation_is_pinned_to_a_literal`
    /// carries, repeated here deliberately: if a future refactor namespaces the
    /// *fallback* too, both fail and the orphaning is caught before it ships.
    #[test]
    fn an_absent_identity_is_byte_identical_to_the_hash_path() {
        let convo = [msg("system", "you are a cat"), msg("user", "poke")];

        // The minted title, byte for byte.
        assert_eq!(
            Titles::new(64).resolve(&convo, None),
            "hytte-bridge-5a36669b9a22d7cc"
        );
        // The identity key is the conversation key, in the transcript space.
        assert_eq!(
            identity_key(&convo, None),
            Key::Transcript(conversation_key(&convo))
        );
        // The single-flight key is the transcript key, unchanged.
        assert_eq!(
            flight_key(&convo, None),
            Key::Transcript(transcript_key(&convo))
        );
        // And the whole multi-turn continuation still lands on the root title.
        let mut titles = Titles::new(64);
        let root = titles.resolve(&convo, None);
        titles.remember(&root, &convo, None, "mrrp");
        let mut turn2 = convo.to_vec();
        turn2.push(msg("assistant", "mrrp"));
        turn2.push(msg("user", "poke again"));
        assert_eq!(titles.resolve(&turn2, None), root);
    }

    /// A caller that names itself gets a title derived from the name, in the
    /// identity namespace — and it is stable across turns whose transcripts
    /// have nothing whatever in common, which is the entire point.
    #[test]
    fn an_explicit_identity_derives_a_namespaced_title() {
        let titles = Titles::new(64);
        let title = titles.resolve(&[msg("user", "hello")], Some("pet"));

        assert!(
            title.starts_with(&format!("{TITLE_PREFIX}{USER_MARKER}")),
            "not namespaced: {title}"
        );
        assert!(!in_transcript_space(&title), "leaked into the hash space");
        // Same identity, unrelated transcript → the same conversation.
        assert_eq!(
            titles.resolve(
                &[msg("system", "wholly"), msg("user", "different")],
                Some("pet")
            ),
            title
        );
        // A different identity is a different conversation.
        assert_ne!(titles.resolve(&[msg("user", "hello")], Some("caw")), title);
    }

    /// **The security-relevant case.** A caller picks its own `user` string, so
    /// it will try to pick one that lands on somebody else's session. It must
    /// not be able to — not for the obvious imitations below, and not for any
    /// string at all, which is what the second assertion states: the marker
    /// makes the identity space *structurally* unreachable from the transcript
    /// space rather than merely improbable to hit.
    #[test]
    fn an_adversarial_identity_cannot_imitate_a_hash_derived_title() {
        let convo = [msg("system", "you are a cat"), msg("user", "poke")];
        let real = hash_title(&convo);
        assert_eq!(real, "hytte-bridge-5a36669b9a22d7cc");
        let titles = Titles::new(64);

        for imitation in [
            // The whole title, verbatim.
            "hytte-bridge-5a36669b9a22d7cc",
            // The bare digest — what a naive `PREFIX + user` would concatenate
            // straight back into a valid hash-derived title.
            "5a36669b9a22d7cc",
            // A rotated generation of it (#667).
            "hytte-bridge-5a36669b9a22d7cc-g1",
            "5a36669b9a22d7cc-g1",
            // The marker, re-supplied, in case it could be doubled or stripped.
            "u-5a36669b9a22d7cc",
            "hytte-bridge-u-5a36669b9a22d7cc",
            // Some other conversation's digest shape.
            "0000000000000000",
            "ffffffffffffffff",
        ] {
            let claimed = titles.resolve(&convo, Some(imitation));
            assert_ne!(claimed, real, "identity {imitation:?} stole the hash title");
            assert!(
                !in_transcript_space(&claimed),
                "identity {imitation:?} minted into the transcript space: {claimed}"
            );
        }
    }

    /// The two spaces are disjoint at the digest as well as at the rendering:
    /// the same string hashed as an identity and as a transcript field is not
    /// the same number, so nothing rests on the marker alone.
    #[test]
    fn the_identity_digest_is_domain_separated() {
        assert_ne!(user_key("pet"), transcript_key(&[msg("user", "pet")]));
        assert_ne!(user_key("pet"), user_key("caw"));
        // Stability across runs and builds is pinned by
        // `identity_title_derivation_is_pinned_to_a_literal` below. It cannot be
        // shown here: `user_key` is a pure function, so comparing it to itself
        // in one process holds for every possible implementation.
    }

    /// The identity space's answer to
    /// `title_derivation_is_pinned_to_a_literal` — and it needs one for the
    /// same reason, since the title is the on-disk name `claude --resume`
    /// resolves against. Without this literal, mutating [`USER_MARKER`],
    /// `USER_DOMAIN`, or the order of the two `fnv1a_field` folds in
    /// [`user_key`] leaves the whole suite green while silently orphaning every
    /// deployed plugin's session. (Verified by mutation: flipping
    /// `USER_MARKER` to `"v-"` turns this test — and only this test — red.)
    ///
    /// Spelled as the finished title rather than the raw digest so it pins the
    /// *rendering* too: the prefix, the marker and the `{:016x}` width are all
    /// inside the literal. Driven through [`identity_key`] with a real
    /// transcript, so it also pins that no byte of the transcript reaches an
    /// identity title.
    #[test]
    fn identity_title_derivation_is_pinned_to_a_literal() {
        let convo = [msg("system", "you are a cat"), msg("user", "poke")];
        assert_eq!(
            title_for(identity_key(&convo, Some("pet"))),
            "hytte-bridge-u-fbbad4ae7d47bf01"
        );
        // The same literal reached the other way, through the raw digest, so
        // the pin survives whichever of the two entry points a refactor keeps.
        assert_eq!(
            title_for(Key::User(user_key("pet"))),
            "hytte-bridge-u-fbbad4ae7d47bf01"
        );
    }

    /// A blank identity is no identity (#704 review): `Some("")` and a
    /// whitespace-only string behave in **every** respect as `None` does, or
    /// each client that fills the field in without meaning anything by it would
    /// land in one shared session — a cross-caller bleed strictly worse than
    /// the hash fallback it displaced.
    #[test]
    fn a_blank_identity_is_exactly_an_absent_one() {
        let convo = [msg("system", "you are a cat"), msg("user", "poke")];

        for blank in [Some(""), Some("   "), Some("\t\n")] {
            assert_eq!(identity(blank), None, "{blank:?}");
            assert_eq!(identity_key(&convo, blank), identity_key(&convo, None));
            assert_eq!(flight_key(&convo, blank), flight_key(&convo, None));
            assert_eq!(
                Titles::new(64).resolve(&convo, blank),
                Titles::new(64).resolve(&convo, None),
            );

            // The forward prefixes an identity-less turn needs are still
            // written, so the next turn resolves to the same session rather
            // than minting a new one every time.
            let mut titles = Titles::new(64);
            let title = titles.resolve(&convo, blank);
            titles.remember(&title, &convo, blank, "meow");
            assert!(!titles.is_empty(), "{blank:?} left no forward prefix");
        }

        // A non-blank identity is untouched — including one with surrounding
        // whitespace, which is the caller's own string, not ours to trim.
        assert_eq!(identity(Some("pet")), Some("pet"));
        assert_eq!(identity(Some(" pet ")), Some(" pet "));
        assert_ne!(user_key(" pet "), user_key("pet"));
    }

    /// One transcript sent with and without an identity is two conversations,
    /// and — the subtler half — answering the identified one must not leave a
    /// prefix entry that the identity-less one would later inherit. That leak
    /// would be a cross-caller session bleed *and* a break of the no-op
    /// guarantee, arriving one turn late.
    #[test]
    fn an_identified_turn_leaves_no_trail_for_an_anonymous_one() {
        let mut titles = Titles::new(64);
        let convo = vec![msg("system", "persona"), msg("user", "hello")];

        let identified = titles.resolve(&convo, Some("pet"));
        titles.remember(&identified, &convo, Some("pet"), "hi there");

        // The turn an anonymous client would present next, had it been the one
        // talking: prefix = the transcript just answered.
        let mut next = convo.clone();
        next.push(msg("assistant", "hi there"));
        next.push(msg("user", "again"));

        let anonymous = titles.resolve(&next, None);
        assert_ne!(
            anonymous, identified,
            "the anonymous turn inherited pet's session"
        );
        assert!(in_transcript_space(&anonymous));
        // The anonymous conversation resolves exactly as if pet had never run.
        assert_eq!(anonymous, Titles::new(64).resolve(&next, None));
    }

    /// Single-flight collapses a *turn*, so it must not collapse two callers.
    /// Sharing an answer between identities would put back at the reply the
    /// cross-caller bleed the identity removed at the session.
    #[test]
    fn single_flight_does_not_merge_two_identities() {
        let convo = [msg("system", "persona"), msg("user", "same question")];

        assert_ne!(
            flight_key(&convo, Some("pet")),
            flight_key(&convo, Some("caw"))
        );
        assert_ne!(flight_key(&convo, Some("pet")), flight_key(&convo, None));
        // …while one identity asking two different things is still two turns.
        let other = [msg("system", "persona"), msg("user", "other question")];
        assert_ne!(
            flight_key(&convo, Some("pet")),
            flight_key(&other, Some("pet"))
        );
        // …and asking the same thing twice is still one.
        assert_eq!(
            flight_key(&convo, Some("pet")),
            flight_key(&convo, Some("pet"))
        );
    }

    /// An identity title fills up like any other, so #667's rotation has to
    /// reach it — otherwise opting in to #704 would trade a rare silent
    /// collision for a guaranteed permanent 413.
    #[test]
    fn an_identity_title_rotates_and_stays_in_its_namespace() {
        let base = Titles::new(64).resolve(&[msg("user", "hello")], Some("pet"));
        let mut title = base.clone();
        for generation in 1..=3u32 {
            let Rotation::To(next) = rotation_for(&Failure::new(OVERFLOW_STATUS, "x"), &title)
            else {
                panic!("generation {generation} refused to rotate");
            };
            assert_eq!(next, format!("{base}-g{generation}"));
            assert!(
                !in_transcript_space(&next),
                "rotation changed space: {next}"
            );
            title = next;
        }
    }

    /// A retirement in the identity space is read back from it — and does not
    /// touch the transcript-space conversation that happens to share a
    /// transcript.
    #[test]
    fn a_retired_identity_resolves_to_its_replacement() {
        let mut titles = Titles::new(64);
        let convo = vec![msg("system", "persona"), msg("user", "poke")];
        let user = Some("pet");
        let next = format!("{}-g1", titles.resolve(&convo, user));
        titles.rotate_to(&convo, user, &next);

        assert_eq!(titles.resolve(&convo, user), next);
        // A different transcript under the same identity is the same
        // conversation, so it lands in the replacement too.
        assert_eq!(titles.resolve(&[msg("user", "unrelated")], user), next);
        // The anonymous conversation is untouched.
        assert_ne!(titles.resolve(&convo, None), next);
        assert_eq!(titles.resolve(&convo, None), hash_title(&convo));
    }

    /// A malformed identity title is as foreign as a malformed hash one — the
    /// marker widens the accepted shape, it does not loosen it.
    #[test]
    fn a_malformed_identity_title_cannot_be_rotated() {
        for foreign in [
            "hytte-bridge-u-",
            "hytte-bridge-u",
            "hytte-bridge-u-nothex0123456789",
            "hytte-bridge-u-5a36669b9a22d7c",   // 15 hex digits
            "hytte-bridge-u-5a36669b9a22d7cc0", // 17
            "hytte-bridge-u-5a36669b9a22d7cc-gx",
            "hytte-bridge-u-u-5a36669b9a22d7cc",
        ] {
            assert_eq!(next_title(foreign), None, "{foreign} was rotated");
            assert_eq!(
                rotation_for(&Failure::new(OVERFLOW_STATUS, "x"), foreign),
                Rotation::Stuck,
                "{foreign}"
            );
        }
    }
}
