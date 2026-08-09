//! A single text replacement, and its inverse.

use std::ops::Range;
use std::sync::Arc;

/// One replacement: the bytes in `range` become `new_text`.
///
/// An insert is an empty range; a delete is empty `new_text`. Modelling all three as
/// one operation means undo, tree-sitter's `InputEdit`, and a future LSP
/// `TextDocumentContentChangeEvent` all derive from the same struct instead of three
/// parallel code paths.
///
/// `old_text` is what was there before, kept so the edit can be inverted without
/// re-reading the buffer. `Arc<str>` so pushing an edit onto the undo stack — and
/// later cloning it to invert — doesn't copy the deleted text again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// Byte range replaced, in coordinates *before* the edit applied.
    pub range: Range<usize>,
    pub new_text: Arc<str>,
    pub old_text: Arc<str>,
}

impl Edit {
    pub fn new(
        range: Range<usize>,
        new_text: impl Into<Arc<str>>,
        old_text: impl Into<Arc<str>>,
    ) -> Self {
        Self { range, new_text: new_text.into(), old_text: old_text.into() }
    }

    /// Byte range this edit occupies *after* being applied.
    pub fn new_range(&self) -> Range<usize> {
        self.range.start..self.range.start + self.new_text.len()
    }

    /// The edit that undoes this one.
    pub fn inverted(&self) -> Edit {
        Edit {
            range: self.new_range(),
            new_text: self.old_text.clone(),
            old_text: self.new_text.clone(),
        }
    }

    /// Whether this edit only appends text directly after `other` with no deletion —
    /// the "user is typing a word" case that undo coalesces into one step.
    ///
    /// A newline on *either* side breaks the run, so pressing enter always starts a
    /// fresh undo step rather than absorbing the next line into the previous one.
    pub fn extends(&self, other: &Edit) -> bool {
        self.old_text.is_empty()
            && other.old_text.is_empty()
            && !self.new_text.is_empty()
            && self.range.start == other.new_range().end
            && !self.new_text.contains('\n')
            && !other.new_text.contains('\n')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverted_round_trips() {
        let e = Edit::new(2..5, "xy", "abc");
        assert_eq!(e.new_range(), 2..4);
        let inv = e.inverted();
        assert_eq!(inv.range, 2..4);
        assert_eq!(&*inv.new_text, "abc");
        assert_eq!(inv.inverted(), e);
    }

    #[test]
    fn extends_only_for_contiguous_typing() {
        let a = Edit::new(0..0, "he", "");
        assert!(Edit::new(2..2, "l", "").extends(&a));
        // gap
        assert!(!Edit::new(5..5, "l", "").extends(&a));
        // a deletion never coalesces
        assert!(!Edit::new(2..3, "", "l").extends(&a));
        // newline starts a fresh undo step
        assert!(!Edit::new(2..2, "\n", "").extends(&a));
        // and typing after a newline does not absorb into the newline's group
        let newline = Edit::new(2..2, "\n", "");
        assert!(!Edit::new(3..3, "c", "").extends(&newline));
    }
}
