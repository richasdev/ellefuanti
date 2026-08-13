//! The UTF-16 boundary, and the bookkeeping for text the OS has not committed yet.
//!
//! # Why this module exists at all
//!
//! macOS drives text input through `NSTextInputClient`, and every offset in that protocol
//! counts **UTF-16 code units**. Everything else in this codebase — `Selection`, `Point`,
//! every range in `Document` — counts **bytes**. The two agree only on ASCII, which is
//! precisely why the disagreement survives casual testing: it is invisible until someone
//! types `ã`, which is the first thing the owner of this editor types.
//!
//! The rule this module exists to enforce is that the conversion happens **once, here, at
//! the door**. A UTF-16 offset that reaches `Document` is a bug that will surface as a
//! panic on a non-boundary slice or, worse, as silently misplaced text. So the platform
//! side of the boundary is the only place that speaks UTF-16, and everything below
//! [`EditorView`](crate::editor::EditorView)'s input-handler methods receives bytes.
//!
//! # Why the conversion goes through the rope rather than the text
//!
//! `Rope` already indexes UTF-16 code units, so `utf16_to_byte` is a tree descent rather
//! than a scan from the start of the file. The obvious implementation —
//! `text.chars().take_while(...)` summing `len_utf16()` — is O(offset), and the offsets the
//! IME asks about are absolute document offsets. On a 20k-line file that is a linear scan
//! per candidate-window query, on the UI thread, while someone is mid-composition. Ropey's
//! index costs nothing extra: `crates/text` already stores the buffer this way.
//!
//! # No gpui here
//!
//! This is deliberate and is the same split `state.rs` documents: the part that is easy to
//! get subtly wrong is the arithmetic, and arithmetic should be testable without opening a
//! window. The gpui-facing `EntityInputHandler` implementation lives in `view.rs`; what is
//! here is the conversion and the marked-range bookkeeping it manipulates.

use std::ops::Range;

use elle_text::Buffer;

/// Converts a UTF-16 code-unit offset from the platform into a byte offset.
///
/// Clamped rather than fallible. The offsets arrive from AppKit, which is describing *its*
/// idea of the document, and that idea can be one edit stale — a candidate window asking
/// about a range that shrank under it is normal traffic, not a programming error. Landing
/// at the end of the buffer is the answer that keeps composition working; a panic on a
/// slice is not.
///
/// An offset that lands *inside* a surrogate pair (possible when the platform splits an
/// emoji, and the only way this can name a non-character position) resolves to the start of
/// the character containing it, which is what `utf16_cu_to_char` already does and the only
/// answer that is a valid byte boundary.
pub fn utf16_to_byte(buffer: &Buffer, offset_utf16: usize) -> usize {
    let rope = buffer.rope();
    let clamped = offset_utf16.min(rope.len_utf16_cu());
    let char_index = rope.utf16_cu_to_char(clamped);
    rope.char_to_byte(char_index)
}

/// Converts a byte offset from the document into the UTF-16 offset the platform expects.
///
/// Clamped for the same reason its inverse is: callers pass offsets taken from a
/// `Selection`, and a selection can name a position in a buffer that has since been
/// replaced wholesale (a reload from disk, an undo of a large edit).
///
/// A byte offset that is mid-character — which `Document` should never produce, but which
/// costs nothing to survive — resolves to the character containing it: `byte_to_char`
/// rounds down by definition, so no boundary snapping is needed here.
pub fn byte_to_utf16(buffer: &Buffer, offset: usize) -> usize {
    let rope = buffer.rope();
    let clamped = offset.min(rope.len_bytes());
    let char_index = rope.byte_to_char(clamped);
    rope.char_to_utf16_cu(char_index)
}

/// Converts a whole UTF-16 range to bytes, in one place so the two ends cannot diverge.
///
/// The `max` is not defensive noise: AppKit is free to hand over a reversed or degenerate
/// range, and a `Range` whose end precedes its start panics the moment anything slices with
/// it. Normalising here means no caller has to remember.
pub fn utf16_range_to_bytes(buffer: &Buffer, range: Range<usize>) -> Range<usize> {
    let start = utf16_to_byte(buffer, range.start);
    let end = utf16_to_byte(buffer, range.end);
    start..end.max(start)
}

/// Converts a byte range to UTF-16, the direction the platform reads answers in.
pub fn byte_range_to_utf16(buffer: &Buffer, range: Range<usize>) -> Range<usize> {
    let start = byte_to_utf16(buffer, range.start);
    let end = byte_to_utf16(buffer, range.end);
    start..end.max(start)
}

/// Text the OS has placed in the buffer but the user has not committed.
///
/// # Why the range is stored rather than the text
///
/// Marked text *is already in the buffer*. That is not an implementation shortcut — it is
/// what `NSTextInputClient` specifies, and it is what makes the candidate window land in
/// the right place: the platform asks for the on-screen bounds of the marked range, and
/// there is nothing to measure unless the characters are really there being laid out by the
/// same shaper as everything else. Storing a copy of the text alongside would create a
/// second source of truth that an undo, a reload, or a concurrent LSP edit could contradict.
///
/// So what is tracked is *where* the uncommitted text is. Everything that has to treat
/// composition differently from typing — and the list is short but load-bearing — asks this
/// range.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Marked {
    /// Byte range of the uncommitted text, or `None` when nothing is being composed.
    ///
    /// Byte, not UTF-16: the moment this crosses the door it is an ordinary document range
    /// like any other, and keeping it in the platform's units would mean converting it back
    /// on every read — including inside the renderer, which has no business knowing that
    /// UTF-16 exists.
    range: Option<Range<usize>>,
}

impl Marked {
    /// The composing range, if the user is mid-composition.
    pub fn range(&self) -> Option<Range<usize>> {
        self.range.clone()
    }

    /// Whether text is being composed right now.
    ///
    /// The question every edit path has to ask before applying a typing convenience — see
    /// [`Marked`]'s note on why auto-pairing must stand down during composition.
    pub fn is_composing(&self) -> bool {
        self.range.is_some()
    }

    /// Records where the newly marked text landed.
    ///
    /// An empty replacement clears the mark rather than storing an empty range. The
    /// platform signals "composition over, nothing committed" by marking the empty string
    /// — pressing Escape mid-composition, or a dead key cancelled by an incompatible
    /// second key — and an empty `Some(range)` would leave `is_composing` true forever,
    /// which quietly disables auto-pairing for the rest of the session. That is the exact
    /// shape of bug this type exists to make impossible to write by accident.
    pub fn set(&mut self, range: Range<usize>) {
        self.range = if range.is_empty() { None } else { Some(range) };
    }

    /// Composition is over: whatever was marked is now ordinary committed text.
    pub fn clear(&mut self) {
        self.range = None;
    }
}

// ponytail: nothing here tracks the mark across an edit that did not come from the IME —
// an LSP format-on-save, or a rename applied from the workspace while a candidate window
// happens to be open. Those paths live in `workspace_view.rs` and go through
// `Document::apply_edits`, which collapses the selection anyway, so today the composition
// simply ends up describing bytes that moved. Fixing it properly means the mark becoming an
// anchor that survives edits rather than a byte range, which is `crates/text`'s job and a
// much larger change than this issue. Left alone deliberately: the window is a few hundred
// milliseconds wide and needs a background edit to land inside it.

#[cfg(test)]
mod tests {
    use super::*;

    /// `ã` is the everyday case for this editor's owner: 2 bytes, 1 UTF-16 unit. If the
    /// conversion were the identity — which it is for the whole ASCII test suite — the
    /// offsets past it would be wrong by one per accented character on the line.
    #[test]
    fn a_two_byte_character_is_one_utf16_unit() {
        let buffer = Buffer::new("ação");

        // Bytes:  a(1) ç(2) ã(2) o(1) = 6.  UTF-16: 4 units, one per character.
        assert_eq!(buffer.len_bytes(), 6);
        assert_eq!(byte_to_utf16(&buffer, 6), 4);

        // Walking the string: after `a`, after `ç`, after `ã`, after `o`.
        assert_eq!(byte_to_utf16(&buffer, 1), 1);
        assert_eq!(byte_to_utf16(&buffer, 3), 2);
        assert_eq!(byte_to_utf16(&buffer, 5), 3);

        // And back, which is the direction the platform's offsets travel.
        assert_eq!(utf16_to_byte(&buffer, 1), 1);
        assert_eq!(utf16_to_byte(&buffer, 2), 3);
        assert_eq!(utf16_to_byte(&buffer, 3), 5);
        assert_eq!(utf16_to_byte(&buffer, 4), 6);
    }

    /// An emoji is 4 bytes and **2** UTF-16 units — the surrogate pair. This is the case
    /// where a "characters, surely" mental model diverges from UTF-16 as well as from
    /// bytes, so neither shortcut survives it.
    #[test]
    fn an_emoji_is_four_bytes_and_two_utf16_units() {
        let buffer = Buffer::new("a🎉b");

        assert_eq!(buffer.len_bytes(), 6);
        assert_eq!(byte_to_utf16(&buffer, 6), 4, "a=1, emoji=2, b=1");

        assert_eq!(byte_to_utf16(&buffer, 1), 1, "before the emoji");
        assert_eq!(byte_to_utf16(&buffer, 5), 3, "after the emoji, before b");

        assert_eq!(utf16_to_byte(&buffer, 1), 1);
        assert_eq!(utf16_to_byte(&buffer, 3), 5);
        assert_eq!(utf16_to_byte(&buffer, 4), 6);
    }

    /// A UTF-16 offset landing between the two halves of a surrogate pair names no
    /// character at all. It must resolve to a byte boundary — the start of the emoji —
    /// because every consumer below slices with it.
    #[test]
    fn an_offset_inside_a_surrogate_pair_lands_on_a_character_boundary() {
        let buffer = Buffer::new("a🎉b");

        // UTF-16 offset 2 is the low surrogate: mid-character.
        let byte = utf16_to_byte(&buffer, 2);
        assert_eq!(byte, 1, "resolves to the start of the emoji, not into it");
        assert!(buffer.text().is_char_boundary(byte));
    }

    /// CJK: 3 bytes, 1 UTF-16 unit. The other direction the ASCII assumption fails in, and
    /// the case the candidate window exists for.
    #[test]
    fn a_three_byte_character_is_one_utf16_unit() {
        let buffer = Buffer::new("日本語");

        assert_eq!(buffer.len_bytes(), 9);
        assert_eq!(byte_to_utf16(&buffer, 9), 3);
        assert_eq!(utf16_to_byte(&buffer, 1), 3);
        assert_eq!(utf16_to_byte(&buffer, 2), 6);
    }

    /// Offsets past the end are ordinary traffic from a platform whose copy of the document
    /// is one edit behind. They clamp; they do not panic.
    #[test]
    fn offsets_past_the_end_clamp_rather_than_panic() {
        let buffer = Buffer::new("ação");

        assert_eq!(utf16_to_byte(&buffer, 999), 6, "clamps to the end in bytes");
        assert_eq!(byte_to_utf16(&buffer, 999), 4, "clamps to the end in UTF-16");
    }

    /// The round trip is what the whole module is for: a byte offset handed out to the
    /// platform and handed back must name the same position.
    #[test]
    fn the_conversion_round_trips_through_multibyte_text() {
        let buffer = Buffer::new("olá 日本 🎉 ação");
        let text = buffer.text();

        for (byte, _) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
            let there_and_back = utf16_to_byte(&buffer, byte_to_utf16(&buffer, byte));
            assert_eq!(there_and_back, byte, "byte offset {byte} did not survive the round trip");
        }
    }

    /// AppKit is free to hand over a range whose end precedes its start, and a `Range` like
    /// that panics the moment anything slices with it. Built from variables rather than
    /// written literally so it survives `clippy::reversed_empty_ranges`, which is right to
    /// object to a reversed literal and beside the point for a value arriving from C.
    #[test]
    fn a_reversed_range_from_the_platform_is_normalised_rather_than_panicking() {
        let buffer = Buffer::new("ação");
        let (start, end) = (3, 1);

        let range = utf16_range_to_bytes(&buffer, start..end);

        assert!(range.start <= range.end, "a range that slices must not be inverted");
    }

    #[test]
    fn ranges_convert_in_both_directions() {
        let buffer = Buffer::new("a🎉b");

        // The emoji alone: bytes 1..5, UTF-16 1..3.
        assert_eq!(utf16_range_to_bytes(&buffer, 1..3), 1..5);
        assert_eq!(byte_range_to_utf16(&buffer, 1..5), 1..3);
    }

    #[test]
    fn a_fresh_mark_is_not_composing() {
        let marked = Marked::default();
        assert!(!marked.is_composing());
        assert_eq!(marked.range(), None);
    }

    #[test]
    fn marking_text_starts_composition_and_clearing_ends_it() {
        let mut marked = Marked::default();

        marked.set(4..6);
        assert!(marked.is_composing());
        assert_eq!(marked.range(), Some(4..6));

        marked.clear();
        assert!(!marked.is_composing());
    }

    /// The bug this prevents: an empty mark that reads as "still composing" forever, which
    /// would leave auto-pairing disabled for the rest of the session.
    #[test]
    fn marking_the_empty_string_ends_composition_rather_than_starting_an_empty_one() {
        let mut marked = Marked::default();
        marked.set(4..6);

        marked.set(4..4);

        assert!(!marked.is_composing(), "an empty mark is the platform saying composition is over");
        assert_eq!(marked.range(), None);
    }
}
