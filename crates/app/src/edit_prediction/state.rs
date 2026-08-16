//! The prediction itself: validity, interpolation, partial-accept arithmetic.
//!
//! # Interpolation — the port's centre of gravity
//!
//! Zed's `interpolate_edits`: when the user types exactly what the model predicted, the
//! prediction does not die — it *shrinks*, re-stamped to the new buffer state, with no
//! new request. Typing through a suggestion feels like the editor keeping up instead of
//! flickering. Anything that is not a clean prefix insertion at the prediction's own
//! offset is divergence, and divergence means `None`: the caller dismisses and re-arms
//! its debounce.
//!
//! The pair-typing trap, caught by arithmetic rather than by threading edit metadata:
//! auto-close turns one keystroke `(` into two inserted bytes `()` with the cursor
//! between them. The typed span reads as a clean prefix, but accepting the shrunk rest
//! would double the closer. [`Prediction::buffer_len`] exists for exactly this — a pure
//! prefix insertion grows the buffer by precisely the distance the cursor advanced, and
//! a pair-close does not.

use crate::editor::Document;
use elle_text::Version;

/// One suggestion, pinned to the exact buffer state it was made for.
///
/// The stamp is the whole correctness story: a prediction is only ever *shown* or
/// *accepted* through [`Prediction::is_valid_for`], so a stale one — the buffer edited,
/// the cursor moved, a selection made — simply stops existing rather than inserting at
/// an offset that now means something else.
pub struct Prediction {
    /// The text Tab inserts at the cursor, already cleaned and trimmed.
    pub text: String,
    /// The cursor offset the suggestion was made at.
    pub at_offset: usize,
    /// The buffer version the suggestion was made against.
    pub version: Version,
    /// The buffer's byte length at that version — what tells a pure prefix insertion
    /// apart from an auto-close pair (see the module doc).
    pub buffer_len: usize,
}

impl Prediction {
    /// A prediction stamped for the document's *current* state.
    pub fn stamped(text: String, document: &Document) -> Self {
        Prediction {
            text,
            at_offset: document.selection.head,
            version: document.buffer.version(),
            buffer_len: document.buffer.len_bytes(),
        }
    }

    /// Whether the document is still exactly the one this suggestion was made for.
    ///
    /// Version covers every edit (undo included — versions only move forward); the head
    /// check covers movement without edits; the emptiness and multi-cursor checks cover
    /// states where "insert at the cursor" is not one well-defined place.
    pub fn is_valid_for(&self, document: &Document) -> bool {
        document.buffer.version() == self.version
            && document.selection.is_empty()
            && document.selection.head == self.at_offset
            && !document.has_multiple_cursors()
    }

    /// The shrunk prediction after an edit — `Some` iff the edit was the user typing a
    /// strict prefix of this prediction, `None` on any divergence.
    ///
    /// Called *after* the edit landed, so it judges the document as it now stands:
    /// cursor moved forward from `at_offset`, buffer grown by exactly that distance,
    /// and the bytes in between equal to the prediction's head. Backspace, edits
    /// elsewhere, pair-insertions, and typing the suggestion out entirely all return
    /// `None` — the last because an empty ghost must not exist (nothing to render,
    /// nothing to accept).
    pub fn interpolated(&self, document: &Document) -> Option<Prediction> {
        if !document.selection.is_empty() || document.has_multiple_cursors() {
            return None;
        }
        let head = document.selection.head;
        if head <= self.at_offset {
            return None;
        }
        let typed_len = head - self.at_offset;
        if document.buffer.len_bytes() != self.buffer_len + typed_len {
            return None; // more (or less) changed than the cursor walked over
        }
        let typed = document.buffer.slice(self.at_offset..head);
        let rest = self.text.strip_prefix(typed.as_str())?;
        if rest.is_empty() {
            return None;
        }
        Some(Prediction {
            text: rest.to_string(),
            at_offset: head,
            version: document.buffer.version(),
            buffer_len: document.buffer.len_bytes(),
        })
    }

    /// How many bytes of the prediction a word-accept takes: the leading identifier run,
    /// or — when the prediction starts with punctuation/whitespace — the leading
    /// non-identifier run (Zed's fallback, so `->` or `    ` is one step, not zero).
    pub fn word_len(&self) -> usize {
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        let leading: usize = self.text.chars().take_while(|&c| ident(c)).map(char::len_utf8).sum();
        if leading > 0 {
            return leading;
        }
        self.text.chars().take_while(|&c| !ident(c)).map(char::len_utf8).sum()
    }

    /// How many bytes a line-accept takes: through the first newline, or everything
    /// when the prediction is single-line (the fall-back-to-full rule).
    pub fn line_len(&self) -> usize {
        self.text.find('\n').map_or(self.text.len(), |index| index + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str, cursor: usize) -> Document {
        let mut document = Document::new(None, text, true).expect("plain doc");
        document.move_to(cursor, false);
        document
    }

    #[test]
    fn a_prediction_dies_with_any_edit_or_cursor_move() {
        let mut document = doc("<?php\n$a = 1;\n", 0);
        let offset = document.buffer.text().find("1;").unwrap() + 2;
        document.move_to(offset, false);

        let ghost = Prediction::stamped("\n$b = 2;".to_string(), &document);
        assert!(ghost.is_valid_for(&document), "fresh ghost at the cursor is valid");

        // A cursor move alone invalidates…
        document.move_to(0, false);
        assert!(!ghost.is_valid_for(&document));
        // …moving back revalidates (same buffer, same offset — the suggestion still
        // describes exactly this insertion point)…
        document.move_to(offset, false);
        assert!(ghost.is_valid_for(&document));
        // …a selection invalidates ("at the cursor" is no longer one place)…
        document.move_to(0, true);
        assert!(!ghost.is_valid_for(&document));
        document.move_to(offset, false);
        // …and any edit invalidates for good: versions only move forward, so even undo
        // cannot resurrect a stale ghost.
        document.insert("x");
        document.undo();
        document.move_to(offset, false);
        assert!(!ghost.is_valid_for(&document));
    }

    #[test]
    fn typing_the_predicted_prefix_shrinks_the_prediction_in_place() {
        let mut document = doc("<?php\n$user->", 13);
        let ghost = Prediction::stamped("save();".to_string(), &document);

        document.insert("sa");
        let shrunk = ghost.interpolated(&document).expect("a typed prefix interpolates");
        assert_eq!(shrunk.text, "ve();");
        assert_eq!(shrunk.at_offset, 15);
        assert!(shrunk.is_valid_for(&document), "re-stamped to the new state");

        // And again, from the shrunk one — interpolation chains.
        document.insert("ve");
        let smaller = shrunk.interpolated(&document).expect("chains");
        assert_eq!(smaller.text, "();");
    }

    #[test]
    fn divergent_typing_kills_the_prediction() {
        let mut document = doc("<?php\n$user->", 13);
        let ghost = Prediction::stamped("save();".to_string(), &document);
        document.insert("de"); // the user went for delete(), not save()
        assert!(ghost.interpolated(&document).is_none());
    }

    #[test]
    fn an_edit_that_is_not_at_the_cursor_kills_the_prediction() {
        let mut document = doc("<?php\n$user->", 13);
        let ghost = Prediction::stamped("save();".to_string(), &document);
        // An insertion at the top of the file: the head ends up at 1, behind the stamp.
        document.move_to(0, false);
        document.insert("x");
        assert!(ghost.interpolated(&document).is_none());
    }

    #[test]
    fn a_pair_insertion_is_divergence_not_a_prefix() {
        // The prediction starts with `(` — but typing `(` with auto-close inserts `()`,
        // growing the buffer by two while the cursor advances one. Interpolating would
        // leave a rest whose closer doubles the one auto-close just added.
        let mut document = doc("<?php\nfoo", 9);
        let ghost = Prediction::stamped("($bar)".to_string(), &document);
        // Simulate the pair: insert both, then park the cursor between them.
        document.insert("()");
        document.move_to(10, false);
        assert!(ghost.interpolated(&document).is_none());
    }

    #[test]
    fn typing_the_whole_prediction_out_ends_it_rather_than_leaving_an_empty_ghost() {
        let mut document = doc("<?php\n$a", 8);
        let ghost = Prediction::stamped(" = 1;".to_string(), &document);
        document.insert(" = 1;");
        assert!(ghost.interpolated(&document).is_none());
    }

    #[test]
    fn multibyte_prefixes_interpolate_on_char_boundaries() {
        let mut document = doc("<?php\n$nome = '", 15);
        let ghost = Prediction::stamped("Ação';".to_string(), &document);
        document.insert("Aç");
        let shrunk = ghost.interpolated(&document).expect("multibyte prefix");
        assert_eq!(shrunk.text, "ão';");
    }

    #[test]
    fn word_and_line_lengths_follow_zeds_rules() {
        let doc0 = doc("x", 1);
        let word = |text: &str| Prediction::stamped(text.to_string(), &doc0).word_len();
        let line = |text: &str| Prediction::stamped(text.to_string(), &doc0).line_len();

        assert_eq!(word("save();"), 4, "the identifier run");
        assert_eq!(word("->save()"), 2, "punctuation run when nothing alphanumeric leads");
        assert_eq!(word("    return"), 4, "leading indentation is one step");
        assert_eq!(word("ação()"), "ação".len(), "multibyte identifiers count in bytes");

        assert_eq!(line("$a = 1;\n$b = 2;"), 8, "through the first newline");
        assert_eq!(line("$a = 1;"), 7, "single line falls back to everything");
    }
}
