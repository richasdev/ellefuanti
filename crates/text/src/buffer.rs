//! Rope-backed text buffer with undo/redo and an edit log for incremental consumers.

use std::ops::Range;
use std::sync::Arc;

use ropey::Rope;

use crate::edit::Edit;

/// A zero-based (row, column) position. `column` is a **byte** offset within the row,
/// matching the crate-wide byte convention and tree-sitter's `Point`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Point {
    pub row: usize,
    pub column: usize,
}

impl Point {
    pub const fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

/// Monotonic version counter. Consumers (syntax tree, LSP, diagnostics) compare it to
/// decide whether their cached view of the text is stale.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Version(pub u64);

/// The text of one open file.
///
/// Edits are O(log n) in the rope and never rebuild the document, which is the whole
/// reason for a rope over a `String` (ADR-0003). Undo/redo store `Edit`s rather than
/// document snapshots, so memory stays proportional to *what changed*, not to file
/// size times history depth.
pub struct Buffer {
    rope: Rope,
    version: Version,
    undo: Vec<Vec<Edit>>,
    redo: Vec<Vec<Edit>>,
    /// Edits applied since a consumer last drained them, for incremental reparsing.
    pending: Vec<Edit>,
    /// False when the buffer matches what is on disk.
    dirty: bool,
    /// Whether the next edit may coalesce into the top undo group.
    coalesce: bool,
}

impl Buffer {
    pub fn new(text: &str) -> Self {
        Self {
            rope: Rope::from_str(text),
            version: Version(0),
            undo: Vec::new(),
            redo: Vec::new(),
            pending: Vec::new(),
            dirty: false,
            coalesce: false,
        }
    }

    pub fn empty() -> Self {
        Self::new("")
    }

    pub fn version(&self) -> Version {
        self.version
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Marks the buffer as matching disk. Called after a successful save.
    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    /// Marks the buffer clean **only if** it still holds the text that was written.
    ///
    /// A save serialises the buffer, hands the bytes to a background write, and comes back
    /// later. Nothing stops the user typing in between — the save panel gpui opens is not
    /// app-modal, so on save-as that window is as long as the user takes to choose a
    /// folder. Clearing `dirty` unconditionally at that point claims the file on disk
    /// matches the buffer when it does not, and since ⌘S on a clean buffer is a no-op those
    /// keystrokes are then unreachable: silent data loss.
    ///
    /// Returns whether the buffer was marked clean, so a caller can tell the difference
    /// between "saved" and "saved, but there is newer text".
    pub fn mark_saved_at(&mut self, version: Version) -> bool {
        let current = self.version == version;
        if current {
            self.dirty = false;
        }
        current
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// One line, without its trailing newline.
    ///
    /// Returns `""` past the end rather than panicking: the renderer legitimately asks
    /// for rows slightly past EOF while scrolling, and a blank line is the right answer.
    pub fn line(&self, row: usize) -> String {
        if row >= self.rope.len_lines() {
            return String::new();
        }
        let line = self.rope.line(row);
        let mut s = line.to_string();
        while s.ends_with('\n') || s.ends_with('\r') {
            s.pop();
        }
        s
    }

    /// Byte length of a row, excluding its line ending.
    pub fn line_len(&self, row: usize) -> usize {
        if row >= self.rope.len_lines() {
            return 0;
        }
        let line = self.rope.line(row);
        let mut len = line.len_bytes();
        // Ropey's Chars is not DoubleEndedIterator, so index from the end by char.
        let last = |offset: usize| -> Option<char> {
            let chars = line.len_chars();
            chars.checked_sub(offset).map(|i| line.char(i))
        };
        if last(1) == Some('\n') {
            len -= 1;
            if last(2) == Some('\r') {
                len -= 1;
            }
        }
        len
    }

    pub fn slice(&self, range: Range<usize>) -> String {
        let range = self.clamp_range(range);
        let start = self.rope.byte_to_char(range.start);
        let end = self.rope.byte_to_char(range.end);
        self.rope.slice(start..end).to_string()
    }

    // --- offset conversion -------------------------------------------------------

    /// Byte offset of a (row, column) point, clamped into the document.
    pub fn point_to_offset(&self, point: Point) -> usize {
        let row = point.row.min(self.rope.len_lines().saturating_sub(1));
        let line_start = self.rope.line_to_byte(row);
        let column = point.column.min(self.line_len(row));
        self.round_to_boundary(line_start + column)
    }

    /// Inverse of [`Buffer::point_to_offset`].
    pub fn offset_to_point(&self, offset: usize) -> Point {
        let offset = self.round_to_boundary(offset.min(self.rope.len_bytes()));
        let row = self.rope.byte_to_line(offset);
        Point::new(row, offset - self.rope.line_to_byte(row))
    }

    /// Snaps `offset` down to the nearest UTF-8 char boundary.
    ///
    /// Guards against a caller landing mid-codepoint (arithmetic on a multi-byte line,
    /// a click resolved to a byte column). Note that ropey's `try_byte_to_char` is *not*
    /// a boundary check — it returns `Ok` for a mid-codepoint byte, silently rounding
    /// down — so the round trip through char space below is what actually normalises.
    /// Public because a caller that slices *and* reports offsets into the result needs the
    /// same snapped value the slice used. `blade_spans` widens a viewport by a fixed byte
    /// padding and then labels its spans `start + i`; when the unsnapped `start` sat inside
    /// a character, `slice` silently rounded but the labels did not, and the spans came out
    /// a few bytes off — splitting characters and panicking the renderer that painted them.
    pub fn round_to_boundary(&self, offset: usize) -> usize {
        let offset = offset.min(self.rope.len_bytes());
        self.rope.char_to_byte(self.rope.byte_to_char(offset))
    }

    fn clamp_range(&self, range: Range<usize>) -> Range<usize> {
        let start = self.round_to_boundary(range.start.min(self.rope.len_bytes()));
        let end = self.round_to_boundary(range.end.min(self.rope.len_bytes()));
        if start <= end { start..end } else { end..start }
    }

    // --- editing -----------------------------------------------------------------

    /// Replaces `range` with `new_text`, pushing the inverse onto the undo stack.
    ///
    /// Returns the applied edit so callers can move cursors and consumers can reparse.
    pub fn replace(&mut self, range: Range<usize>, new_text: &str) -> Edit {
        let range = self.clamp_range(range);
        let old_text: Arc<str> = self.slice(range.clone()).into();
        let edit = Edit::new(range, new_text, old_text);

        self.apply(&edit);

        // Coalesce consecutive typing into one undo step so ctrl-z removes a word,
        // not a letter. Any non-contiguous edit (or a save) breaks the run.
        let coalesced = self.coalesce
            && self
                .undo
                .last()
                .and_then(|group| group.last())
                .is_some_and(|last| edit.extends(last));

        if coalesced {
            self.undo.last_mut().expect("checked above").push(edit.clone());
        } else {
            self.undo.push(vec![edit.clone()]);
        }
        self.coalesce = true;
        self.redo.clear();

        edit
    }

    pub fn insert(&mut self, offset: usize, text: &str) -> Edit {
        self.replace(offset..offset, text)
    }

    pub fn delete(&mut self, range: Range<usize>) -> Edit {
        self.replace(range, "")
    }

    /// Forces the next edit to start a fresh undo group. Call on cursor jumps, focus
    /// changes and saves, where coalescing with earlier typing would surprise the user.
    pub fn break_undo_group(&mut self) {
        self.coalesce = false;
    }

    /// Writes the edit into the rope and records it for incremental consumers.
    /// Does not touch the undo/redo stacks — callers decide where it belongs.
    fn apply(&mut self, edit: &Edit) {
        let start = self.rope.byte_to_char(edit.range.start);
        let end = self.rope.byte_to_char(edit.range.end);
        if start != end {
            self.rope.remove(start..end);
        }
        if !edit.new_text.is_empty() {
            self.rope.insert(start, &edit.new_text);
        }
        self.version.0 += 1;
        self.dirty = true;
        self.pending.push(edit.clone());
    }

    /// Reverts the most recent undo group. Returns the edits applied to do so
    /// (already inverted), or `None` when there is nothing to undo.
    pub fn undo(&mut self) -> Option<Vec<Edit>> {
        let group = self.undo.pop()?;
        // Inverting a group means replaying it backwards: later edits' coordinates
        // assume earlier ones are still applied.
        let applied: Vec<Edit> = group.iter().rev().map(Edit::inverted).collect();
        for edit in &applied {
            self.apply(edit);
        }
        self.redo.push(group);
        self.coalesce = false;
        Some(applied)
    }

    /// Re-applies the most recently undone group.
    pub fn redo(&mut self) -> Option<Vec<Edit>> {
        let group = self.redo.pop()?;
        for edit in &group {
            self.apply(edit);
        }
        self.undo.push(group.clone());
        self.coalesce = false;
        Some(group)
    }

    /// Takes the edits applied since the last drain, for incremental reparsing.
    ///
    /// ponytail: a single shared queue works because there is one consumer today (the
    /// syntax tree). Give each consumer its own cursor into a shared log when LSP and
    /// diagnostics also need the stream.
    pub fn drain_pending(&mut self) -> Vec<Edit> {
        std::mem::take(&mut self.pending)
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_delete_and_slice() {
        let mut b = Buffer::new("hello world");
        b.insert(5, ",");
        assert_eq!(b.text(), "hello, world");
        b.delete(0..7);
        assert_eq!(b.text(), "world");
        assert_eq!(b.slice(0..3), "wor");
    }

    #[test]
    fn new_buffer_is_clean_and_edits_dirty_it() {
        let mut b = Buffer::new("x");
        assert!(!b.is_dirty());
        b.insert(1, "y");
        assert!(b.is_dirty());
        b.mark_saved();
        assert!(!b.is_dirty());
    }

    #[test]
    fn version_advances_per_edit() {
        let mut b = Buffer::new("");
        assert_eq!(b.version(), Version(0));
        b.insert(0, "a");
        b.insert(1, "b");
        assert_eq!(b.version(), Version(2));
    }

    #[test]
    fn lines_exclude_line_endings() {
        let b = Buffer::new("one\ntwo\r\nthree");
        assert_eq!(b.line(0), "one");
        assert_eq!(b.line(1), "two");
        assert_eq!(b.line(2), "three");
        assert_eq!(b.line_len(0), 3);
        assert_eq!(b.line_len(1), 3);
        // Past EOF is blank, not a panic: the renderer asks while scrolling.
        assert_eq!(b.line(99), "");
        assert_eq!(b.line_len(99), 0);
    }

    #[test]
    fn point_offset_round_trip_with_multibyte_text() {
        // "é" is two bytes, so char and byte offsets diverge after it.
        let b = Buffer::new("héllo\nwörld");
        let p = Point::new(1, 3); // after "wö" (w=1 byte, ö=2)
        let off = b.point_to_offset(p);
        assert_eq!(b.offset_to_point(off), p);
        assert_eq!(b.slice(off..off + 1), "r");
    }

    #[test]
    fn offsets_snap_off_boundary_instead_of_corrupting() {
        let mut b = Buffer::new("aéb");
        // Offset 2 is the middle of "é". It must snap to a boundary (down, to 1) rather
        // than splitting the codepoint into invalid UTF-8.
        b.insert(2, "X");
        assert_eq!(b.text(), "aXéb");
        assert!(b.text().is_char_boundary(1));
    }

    #[test]
    fn point_clamps_past_end_of_line_and_document() {
        let b = Buffer::new("ab\ncd");
        assert_eq!(b.point_to_offset(Point::new(0, 99)), 2);
        assert_eq!(b.point_to_offset(Point::new(99, 0)), 3);
    }

    #[test]
    fn undo_redo_restores_text_exactly() {
        let mut b = Buffer::new("abc");
        b.break_undo_group();
        b.replace(1..2, "XYZ");
        assert_eq!(b.text(), "aXYZc");
        b.undo();
        assert_eq!(b.text(), "abc");
        b.redo();
        assert_eq!(b.text(), "aXYZc");
        b.undo();
        assert_eq!(b.text(), "abc");
        assert!(b.undo().is_none());
    }

    #[test]
    fn typing_coalesces_into_one_undo_step() {
        let mut b = Buffer::new("");
        for (i, c) in "hello".chars().enumerate() {
            b.insert(i, &c.to_string());
        }
        assert_eq!(b.text(), "hello");
        b.undo();
        assert_eq!(b.text(), "", "a run of typing should undo as one step");
    }

    #[test]
    fn newline_and_cursor_jumps_break_coalescing() {
        let mut b = Buffer::new("");
        b.insert(0, "ab");
        b.insert(2, "\n");
        b.insert(3, "cd");
        b.undo();
        assert_eq!(b.text(), "ab\n");
        b.undo();
        assert_eq!(b.text(), "ab");

        let mut b = Buffer::new("xy");
        b.insert(0, "a");
        b.break_undo_group();
        b.insert(1, "b");
        b.undo();
        assert_eq!(b.text(), "axy");
    }

    #[test]
    fn undo_after_edit_clears_redo() {
        let mut b = Buffer::new("a");
        b.insert(1, "b");
        b.undo();
        b.break_undo_group();
        b.insert(1, "c");
        assert!(b.redo().is_none(), "a new edit must invalidate the redo stack");
    }

    #[test]
    fn pending_edits_drain_once() {
        let mut b = Buffer::new("");
        b.insert(0, "a");
        b.insert(1, "b");
        assert!(b.has_pending());
        assert_eq!(b.drain_pending().len(), 2);
        assert!(!b.has_pending());
        assert!(b.drain_pending().is_empty());
    }

    #[test]
    fn a_save_of_stale_text_does_not_mark_the_buffer_clean() {
        // The save-as sequence: serialise the buffer, open a non-modal save panel, and the
        // user keeps typing while it is up. The write lands the *old* bytes, so the buffer
        // must stay dirty — otherwise ⌘S becomes a no-op and those keystrokes are lost.
        let mut b = Buffer::new("<?php\n");
        b.insert(6, "$a = 1;");
        let saved_version = b.version();

        // Typed while the dialog was open.
        b.insert(b.len_bytes(), "\n$b = 2;");

        assert!(!b.mark_saved_at(saved_version), "the write is stale, so it cannot mark clean");
        assert!(b.is_dirty(), "the newer text is not on disk yet");
    }

    #[test]
    fn a_save_of_current_text_marks_the_buffer_clean() {
        let mut b = Buffer::new("<?php\n");
        b.insert(6, "$a = 1;");
        let saved_version = b.version();
        assert!(b.mark_saved_at(saved_version));
        assert!(!b.is_dirty());
    }

    #[test]
    fn undo_produces_pending_edits_too() {
        // The syntax tree must see undo as edits, or highlighting desyncs.
        let mut b = Buffer::new("abc");
        b.replace(0..1, "Z");
        b.drain_pending();
        b.undo();
        assert_eq!(b.drain_pending().len(), 1);
    }
}
