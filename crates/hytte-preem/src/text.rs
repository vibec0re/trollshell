//! Character classification for cleaning a short chat-model reply before it
//! goes into a pixel-font bubble (#1168).
//!
//! The pet and caw plugins each hand-rolled `is_combining`/`is_dropped` —
//! byte-for-byte identical bar the doc comments, one of which literally said
//! "kept in sync with the pet's bubble rule" — before rendering the result
//! through [`super::TextBox`]. This lives here rather than in
//! `hytte-ai-providers` on purpose: [`hytte_ai_providers::chat`] documents
//! itself as holding "no opinion about the text" a caller gets back, and
//! these two functions are squarely an opinion about text — specifically,
//! what reads as noise in a *pixel-font bubble* (an emoji renders as a blank
//! [`super::font::NOTDEF`] box there, never a crash, but it's an ugly box) —
//! not about the chat wire protocol. Both callers already render through
//! this kit's `TextBox`, so this is where the opinion belongs.
//!
//! Neither function is a completeness claim about Unicode; both cover what a
//! small chat model realistically emits, same as before the hoist.

/// Whether `c` is a combining mark from the ranges a chat model realistically
/// emits (a full grapheme segmenter would be a dependency for nothing).
///
/// Check this before popping a trailing char off a length-clamped string, so
/// an appended ellipsis doesn't strand a diacritic on the cut edge.
#[must_use]
pub fn is_combining_mark(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{20D0}'..='\u{20FF}')
}

/// Whether `c` is noise to drop from a bubble: emoji blocks (kaomoji glyphs
/// sit far below them and survive) plus every double-quote lookalike — a
/// tiny model loves opening a quote it never closes, which end-trimming
/// can't catch.
#[must_use]
pub fn is_bubble_noise(c: char) -> bool {
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

#[cfg(test)]
mod tests {
    use super::{is_bubble_noise, is_combining_mark};

    /// Falsification target: drop the `'\u{1F000}'..='\u{1FAFF}'` arm from
    /// [`is_bubble_noise`] and this test reds (as do the pet's and caw's own
    /// `sanitize_*` tests, which exercise it through the kaomoji filter) —
    /// see the PR for the falsification run.
    #[test]
    fn drops_emoji_and_curly_quote_lookalikes() {
        assert!(is_bubble_noise('😀'));
        assert!(is_bubble_noise('\u{2764}')); // heart, U+2764
        assert!(is_bubble_noise('\u{FE0F}')); // variant selector
        assert!(is_bubble_noise('\u{200D}')); // ZWJ
        assert!(is_bubble_noise('\u{201c}')); // “
        assert!(is_bubble_noise('\u{201d}')); // ”
        assert!(is_bubble_noise('"'));
        assert!(!is_bubble_noise('a'));
        assert!(!is_bubble_noise('!'));
        // Kaomoji glyphs (plain ASCII/punctuation) survive.
        assert!(!is_bubble_noise('^'));
        assert!(!is_bubble_noise('･'));
    }

    #[test]
    fn flags_combining_marks_only() {
        assert!(is_combining_mark('\u{0301}')); // combining acute accent
        assert!(!is_combining_mark('a'));
        assert!(!is_combining_mark('é')); // precomposed, not combining
    }
}
