//! Cursor and selection logic, with no gpui in sight.
//!
//! Split from the view so editing semantics — the part that is easy to get subtly wrong
//! — can be tested at full speed without opening a window.

use std::ops::Range;
use std::path::{Path, PathBuf};

use elle_syntax::{Language, SyntaxTree, language_for_path};
use elle_text::{Buffer, Point, Version};

/// A cursor with an optional selection anchor.
///
/// One cursor for Milestone 1.
/// ponytail: multi-cursor is a `Vec<Selection>` plus merge-on-overlap. Deliberately not
/// built yet — every motion below would need to fan out over the vec, which is a lot of
/// code for a feature no milestone-1 flow uses.
#[derive(Clone, Copy, Debug, Default)]
pub struct Selection {
    /// Where the selection started; equals `head` when there is no selection.
    pub anchor: usize,
    /// Where the cursor is.
    pub head: usize,
}

impl Selection {
    pub fn at(offset: usize) -> Self {
        Self { anchor: offset, head: offset }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Selected byte range, low to high.
    pub fn range(&self) -> Range<usize> {
        if self.anchor <= self.head { self.anchor..self.head } else { self.head..self.anchor }
    }
}

/// Bytes destined for disk, plus the buffer version they were taken from.
///
/// See [`Document::snapshot_for_save`] for why the version rides along.
pub struct SaveSnapshot {
    pub text: String,
    pub version: Version,
}

/// One open document: text, parse state, cursor, and where it came from.
pub struct Document {
    pub path: Option<PathBuf>,
    pub buffer: Buffer,
    pub syntax: SyntaxTree,
    pub selection: Selection,
    /// Column the cursor "wants" during vertical motion, so moving down through a short
    /// line and back out returns to the original column instead of sticking.
    goal_column: Option<usize>,
    /// Whether the file had a trailing newline when loaded; preserved on save.
    pub trailing_newline: bool,
}

impl Document {
    pub fn new(path: Option<PathBuf>, text: &str, trailing_newline: bool) -> anyhow::Result<Self> {
        let language = path.as_deref().map(language_for_path).unwrap_or(Language::PlainText);
        let buffer = Buffer::new(text);
        let syntax = SyntaxTree::new(language, &buffer)?;
        Ok(Self {
            path,
            buffer,
            syntax,
            selection: Selection::at(0),
            goal_column: None,
            trailing_newline,
        })
    }

    /// An empty buffer with no path yet. ⌘S on one routes to save-as.
    ///
    /// `trailing_newline` is false because the file has never been on disk: there is no
    /// original behaviour to preserve, so the save writes exactly what the user typed.
    pub fn untitled() -> anyhow::Result<Self> {
        Self::new(None, "", false)
    }

    /// Gives the document a path, re-detecting its language.
    ///
    /// Used by save-as: a buffer saved as `User.php` must start highlighting as PHP, which
    /// means a fresh parse tree — the old one has no grammar at all if the buffer began as
    /// plain text. Returns an error only if the new language's grammar fails to load, in
    /// which case the path is still adopted and the document falls back to plain text
    /// rather than losing the save.
    pub fn set_path(&mut self, path: PathBuf) -> anyhow::Result<()> {
        let language = language_for_path(&path);
        self.path = Some(path);

        if language == self.syntax.language() {
            return Ok(());
        }

        match SyntaxTree::new(language, &self.buffer) {
            Ok(syntax) => {
                self.syntax = syntax;
                Ok(())
            }
            Err(err) => {
                self.syntax = SyntaxTree::new(Language::PlainText, &self.buffer)
                    .expect("plain text needs no grammar");
                Err(err)
            }
        }
    }

    pub fn language(&self) -> Language {
        self.syntax.language()
    }

    /// File name for a tab label.
    pub fn title(&self) -> String {
        self.path
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "untitled".to_string())
    }

    pub fn cursor_point(&self) -> Point {
        self.buffer.offset_to_point(self.selection.head)
    }

    /// Text to write to disk, restoring the trailing newline the file was loaded with.
    pub fn text_for_save(&self) -> String {
        let mut text = self.buffer.text();
        if self.trailing_newline && !text.ends_with('\n') {
            text.push('\n');
        }
        text
    }

    /// The bytes to write, tagged with the buffer version they came from.
    ///
    /// The two travel together deliberately. A save serialises here and finishes much
    /// later — after a background write, and on save-as after a file dialog the user may
    /// sit in for a minute — and the buffer can move in between. Handing the caller a
    /// bare `String` invites it to clear the dirty flag for text that is already stale;
    /// carrying the version makes that mistake impossible to write.
    pub fn snapshot_for_save(&self) -> SaveSnapshot {
        SaveSnapshot { text: self.text_for_save(), version: self.buffer.version() }
    }

    /// Pushes buffer edits into the parse tree. Called after any mutation.
    fn sync_syntax(&mut self) {
        let edits = self.buffer.drain_pending();
        if !edits.is_empty() {
            self.syntax.apply_edits(&self.buffer, &edits);
        }
    }

    // --- mutation ----------------------------------------------------------------

    /// Replaces the selection (or inserts at the cursor) with `text`.
    pub fn insert(&mut self, text: &str) {
        let range = self.selection.range();
        let edit = self.buffer.replace(range, text);
        self.selection = Selection::at(edit.new_range().end);
        self.goal_column = None;
        self.sync_syntax();
    }

    /// Backspace: deletes the selection, or one character before the cursor.
    pub fn backspace(&mut self) {
        let range = if self.selection.is_empty() {
            let head = self.selection.head;
            if head == 0 {
                return;
            }
            // Step back one *character*, not one byte, or a multi-byte char corrupts.
            self.prev_char_offset(head)..head
        } else {
            self.selection.range()
        };
        let edit = self.buffer.replace(range, "");
        self.selection = Selection::at(edit.range.start);
        self.goal_column = None;
        self.sync_syntax();
    }

    /// Forward delete.
    pub fn delete_forward(&mut self) {
        let range = if self.selection.is_empty() {
            let head = self.selection.head;
            if head >= self.buffer.len_bytes() {
                return;
            }
            head..self.next_char_offset(head)
        } else {
            self.selection.range()
        };
        let edit = self.buffer.replace(range, "");
        self.selection = Selection::at(edit.range.start);
        self.goal_column = None;
        self.sync_syntax();
    }

    pub fn undo(&mut self) {
        if let Some(edits) = self.buffer.undo() {
            if let Some(last) = edits.last() {
                self.selection = Selection::at(last.new_range().end);
            }
            self.goal_column = None;
            self.sync_syntax();
        }
    }

    pub fn redo(&mut self) {
        if let Some(edits) = self.buffer.redo() {
            if let Some(last) = edits.last() {
                self.selection = Selection::at(last.new_range().end);
            }
            self.goal_column = None;
            self.sync_syntax();
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        (!self.selection.is_empty()).then(|| self.buffer.slice(self.selection.range()))
    }

    // --- motion ------------------------------------------------------------------

    /// Moves the cursor, collapsing or extending the selection.
    pub fn move_to(&mut self, offset: usize, extend: bool) {
        let offset = offset.min(self.buffer.len_bytes());
        if extend {
            self.selection.head = offset;
        } else {
            self.selection = Selection::at(offset);
        }
        // A cursor jump ends the undo coalescing run, so ctrl-z after clicking elsewhere
        // does not merge the two edits into one step.
        self.buffer.break_undo_group();
    }

    pub fn move_horizontal(&mut self, forward: bool, extend: bool) {
        let head = self.selection.head;

        // Collapsing a selection with an unmodified arrow key goes to its edge, which is
        // what every editor does and what users expect after shift-selecting.
        if !extend && !self.selection.is_empty() {
            let range = self.selection.range();
            self.move_to(if forward { range.end } else { range.start }, false);
            self.goal_column = None;
            return;
        }

        let target =
            if forward { self.next_char_offset(head) } else { self.prev_char_offset(head) };
        self.move_to(target, extend);
        self.goal_column = None;
    }

    pub fn move_vertical(&mut self, down: bool, extend: bool) {
        let point = self.buffer.offset_to_point(self.selection.head);
        let goal = self.goal_column.unwrap_or(point.column);

        let row = if down {
            (point.row + 1).min(self.buffer.len_lines().saturating_sub(1))
        } else {
            point.row.saturating_sub(1)
        };

        let offset = self.buffer.point_to_offset(Point::new(row, goal));
        self.move_to(offset, extend);
        // Remember the goal *after* moving: move_to clears nothing, but insert/delete do.
        self.goal_column = Some(goal);
    }

    pub fn move_line_start(&mut self, extend: bool) {
        let row = self.buffer.offset_to_point(self.selection.head).row;
        self.move_to(self.buffer.point_to_offset(Point::new(row, 0)), extend);
        self.goal_column = None;
    }

    pub fn move_line_end(&mut self, extend: bool) {
        let row = self.buffer.offset_to_point(self.selection.head).row;
        let column = self.buffer.line_len(row);
        self.move_to(self.buffer.point_to_offset(Point::new(row, column)), extend);
        self.goal_column = None;
    }

    pub fn select_all(&mut self) {
        self.selection = Selection { anchor: 0, head: self.buffer.len_bytes() };
        self.goal_column = None;
    }

    /// Offset of the character before `offset`, respecting UTF-8.
    fn prev_char_offset(&self, offset: usize) -> usize {
        let rope = self.buffer.rope();
        let char_idx = rope.byte_to_char(offset);
        rope.char_to_byte(char_idx.saturating_sub(1))
    }

    /// Offset of the character after `offset`, respecting UTF-8.
    fn next_char_offset(&self, offset: usize) -> usize {
        let rope = self.buffer.rope();
        let char_idx = rope.byte_to_char(offset);
        rope.char_to_byte((char_idx + 1).min(rope.len_chars()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Document {
        Document::new(Some(PathBuf::from("t.php")), text, true).unwrap()
    }

    #[test]
    fn insert_replaces_selection_and_leaves_cursor_after() {
        let mut d = doc("hello world");
        d.selection = Selection { anchor: 0, head: 5 };
        d.insert("bye");
        assert_eq!(d.buffer.text(), "bye world");
        assert_eq!(d.selection.head, 3);
        assert!(d.selection.is_empty());
    }

    #[test]
    fn backspace_deletes_a_char_not_a_byte() {
        let mut d = doc("ação");
        d.move_to(d.buffer.len_bytes(), false);
        d.backspace();
        assert_eq!(d.buffer.text(), "açã");
        d.backspace();
        assert_eq!(d.buffer.text(), "aç");
    }

    #[test]
    fn backspace_at_start_and_delete_at_end_are_no_ops() {
        let mut d = doc("a");
        d.move_to(0, false);
        d.backspace();
        assert_eq!(d.buffer.text(), "a");
        d.move_to(1, false);
        d.delete_forward();
        assert_eq!(d.buffer.text(), "a");
    }

    #[test]
    fn backspace_with_a_selection_deletes_the_selection() {
        let mut d = doc("abcdef");
        d.selection = Selection { anchor: 1, head: 4 };
        d.backspace();
        assert_eq!(d.buffer.text(), "aef");
        assert_eq!(d.selection.head, 1);
    }

    #[test]
    fn horizontal_motion_steps_over_multibyte_chars() {
        let mut d = doc("aéb");
        d.move_to(0, false);
        d.move_horizontal(true, false);
        assert_eq!(d.selection.head, 1);
        d.move_horizontal(true, false);
        assert_eq!(d.selection.head, 3, "must skip both bytes of é");
        d.move_horizontal(false, false);
        assert_eq!(d.selection.head, 1);
    }

    #[test]
    fn arrow_key_collapses_a_selection_to_its_edge() {
        let mut d = doc("abcdef");
        d.selection = Selection { anchor: 4, head: 1 };
        d.move_horizontal(true, false);
        assert_eq!(d.selection.head, 4, "right collapses to the high edge");

        d.selection = Selection { anchor: 4, head: 1 };
        d.move_horizontal(false, false);
        assert_eq!(d.selection.head, 1, "left collapses to the low edge");
    }

    #[test]
    fn shift_arrow_extends_the_selection() {
        let mut d = doc("abcdef");
        d.move_to(2, false);
        d.move_horizontal(true, true);
        d.move_horizontal(true, true);
        assert_eq!(d.selection.anchor, 2);
        assert_eq!(d.selection.head, 4);
        assert_eq!(d.selected_text().unwrap(), "cd");
    }

    #[test]
    fn vertical_motion_remembers_the_goal_column() {
        // Down through a short line and back out must return to column 5.
        let mut d = doc("aaaaaaaa\nbb\ncccccccc");
        d.move_to(d.buffer.point_to_offset(Point::new(0, 5)), false);
        d.move_vertical(true, false);
        assert_eq!(d.cursor_point(), Point::new(1, 2), "clamped to the short line");
        d.move_vertical(true, false);
        assert_eq!(d.cursor_point(), Point::new(2, 5), "goal column restored");
    }

    #[test]
    fn editing_clears_the_goal_column() {
        let mut d = doc("aaaaaaaa\nbb\ncccccccc");
        d.move_to(d.buffer.point_to_offset(Point::new(0, 5)), false);
        d.move_vertical(true, false);
        d.insert("X");
        d.move_vertical(true, false);
        assert_eq!(d.cursor_point(), Point::new(2, 3), "column follows the edit, not the old goal");
    }

    #[test]
    fn vertical_motion_stops_at_the_document_edges() {
        let mut d = doc("one\ntwo");
        d.move_to(0, false);
        d.move_vertical(false, false);
        assert_eq!(d.cursor_point().row, 0);
        d.move_to(d.buffer.len_bytes(), false);
        d.move_vertical(true, false);
        assert_eq!(d.cursor_point().row, 1);
    }

    #[test]
    fn line_start_and_end() {
        let mut d = doc("  hello\nworld");
        d.move_to(4, false);
        d.move_line_end(false);
        assert_eq!(d.cursor_point(), Point::new(0, 7));
        d.move_line_start(false);
        assert_eq!(d.cursor_point(), Point::new(0, 0));
    }

    #[test]
    fn undo_redo_moves_the_cursor_with_the_text() {
        let mut d = doc("abc");
        d.move_to(3, false);
        d.insert("def");
        assert_eq!(d.buffer.text(), "abcdef");
        d.undo();
        assert_eq!(d.buffer.text(), "abc");
        assert!(d.selection.head <= d.buffer.len_bytes());
        d.redo();
        assert_eq!(d.buffer.text(), "abcdef");
    }

    #[test]
    fn select_all_then_type_replaces_everything() {
        let mut d = doc("old content");
        d.select_all();
        d.insert("new");
        assert_eq!(d.buffer.text(), "new");
    }

    #[test]
    fn syntax_tree_tracks_edits() {
        let mut d = doc("<?php\nclass A {}\n");
        assert!(!d.syntax.has_error());
        d.move_to(d.buffer.len_bytes(), false);
        d.insert("class B {}\n");
        assert!(!d.syntax.has_error(), "incremental reparse should still be valid PHP");
        assert!(
            d.syntax.tree().unwrap().root_node().to_sexp().matches("class_declaration").count()
                >= 2
        );
    }

    #[test]
    fn save_text_restores_the_trailing_newline() {
        let mut d = Document::new(Some(PathBuf::from("a.php")), "x\n", true).unwrap();
        d.move_to(d.buffer.len_bytes(), false);
        d.backspace();
        assert_eq!(d.buffer.text(), "x");
        assert_eq!(d.text_for_save(), "x\n");

        let d = Document::new(Some(PathBuf::from("a.php")), "x", false).unwrap();
        assert_eq!(d.text_for_save(), "x", "a file without one must not gain one");
    }

    #[test]
    fn typing_while_the_save_dialog_is_open_keeps_the_buffer_dirty() {
        // Save-as: ⌘S on an untitled buffer opens gpui's save panel, which is *not*
        // app-modal (`beginWithCompletionHandler:`, not `runModal`), so the editor keeps
        // taking keystrokes for as long as the user browses for a folder. The write lands
        // the snapshot taken before the panel opened.
        let mut d = Document::new(None, "<?php\n", false).unwrap();
        d.move_to(d.buffer.len_bytes(), false);
        d.insert("$a = 1;");

        let snapshot = d.snapshot_for_save();

        // ...panel is up, user keeps typing...
        d.insert("\n$b = 2;");

        // The write succeeded, but for `snapshot.text`, not for what the buffer holds now.
        assert!(!d.buffer.mark_saved_at(snapshot.version));
        assert!(d.buffer.is_dirty(), "the newer text has never reached disk");
        assert_ne!(snapshot.text, d.text_for_save());
    }

    #[test]
    fn a_snapshot_saved_with_no_intervening_edit_marks_the_buffer_clean() {
        let mut d = doc("<?php\n");
        d.move_to(d.buffer.len_bytes(), false);
        d.insert("$a = 1;");
        let snapshot = d.snapshot_for_save();
        assert!(d.buffer.mark_saved_at(snapshot.version));
        assert!(!d.buffer.is_dirty());
    }

    #[test]
    fn title_falls_back_to_untitled() {
        assert_eq!(Document::new(None, "", false).unwrap().title(), "untitled");
        assert_eq!(doc("").title(), "t.php");
    }

    #[test]
    fn set_path_adopts_the_path_and_redetects_the_language() {
        // Save-as on an untitled buffer: it starts as plain text with no grammar, and must
        // begin highlighting as PHP once it has a .php name.
        let mut d = Document::new(None, "<?php\nclass A {}\n", false).unwrap();
        assert_eq!(d.language(), Language::PlainText);
        assert!(d.syntax.tree().is_none(), "plain text has no parse tree");

        d.set_path(PathBuf::from("/tmp/User.php")).unwrap();

        assert_eq!(d.language(), Language::Php);
        assert_eq!(d.title(), "User.php");
        assert!(d.syntax.tree().is_some(), "adopting a .php path must produce a parse tree");
        assert!(!d.syntax.has_error());
    }

    #[test]
    fn set_path_to_the_same_language_keeps_working() {
        let mut d = doc("<?php $x = 1;");
        d.set_path(PathBuf::from("/tmp/Other.php")).unwrap();
        assert_eq!(d.language(), Language::Php);
        assert_eq!(d.title(), "Other.php");
        // Still editable and still parsing after the swap.
        d.move_to(d.buffer.len_bytes(), false);
        d.insert(" // note");
        assert!(!d.syntax.has_error());
    }

    #[test]
    fn set_path_to_an_unknown_extension_falls_back_to_plain_text() {
        let mut d = doc("<?php $x = 1;");
        d.set_path(PathBuf::from("/tmp/notes.txt")).unwrap();
        assert_eq!(d.language(), Language::PlainText);
        assert!(d.syntax.tree().is_none());
    }

    /// `set_path` swaps the `SyntaxTree` for a freshly built one. That is only safe because
    /// the edit log is empty at the time: a fresh tree has never seen any edit, so replaying
    /// already-applied edits into it afterwards would shift byte ranges that were never
    /// stale and desync highlighting from the text.
    ///
    /// The invariant that makes it safe is that every mutating method on `Document` drains
    /// the log through `sync_syntax` before returning, so no caller can reach `set_path`
    /// with edits outstanding. `Document::buffer` is public, so nothing in the type system
    /// enforces that — this test does.
    #[test]
    fn a_path_swap_never_happens_with_edits_outstanding() {
        let mut d = Document::new(None, "<?php\n", false).unwrap();

        // Exercise every mutating path, checking the log is drained after each. The text is
        // left as valid PHP so the parse assertion below is about sync, not about syntax.
        d.move_to(d.buffer.len_bytes(), false);
        d.insert("$a = 1;X");
        assert!(!d.buffer.has_pending(), "insert must drain");
        d.backspace();
        assert!(!d.buffer.has_pending(), "backspace must drain");
        d.insert("Y");
        d.move_to(d.buffer.len_bytes() - 1, false);
        d.delete_forward();
        assert!(!d.buffer.has_pending(), "delete_forward must drain");
        d.undo();
        assert!(!d.buffer.has_pending(), "undo must drain");
        d.redo();
        assert!(!d.buffer.has_pending(), "redo must drain");
        assert_eq!(d.buffer.text(), "<?php\n$a = 1;", "fixture must end as valid PHP");

        // Save-as onto a .php name now swaps in a PHP tree over the current text.
        let before = d.buffer.text();
        d.set_path(PathBuf::from("/tmp/Adopted.php")).unwrap();
        assert_eq!(d.language(), Language::Php);
        assert!(!d.buffer.has_pending(), "the swap itself must not leave edits behind");

        // And the new tree agrees with the text it was built from: keep editing and the
        // incremental reparse stays valid rather than working from shifted coordinates.
        d.move_to(d.buffer.len_bytes(), false);
        d.insert("\nclass A {}\n");
        assert!(!d.syntax.has_error(), "the swapped-in tree must be in sync with the buffer");
        assert!(d.buffer.text().starts_with(&before));
    }

    #[test]
    fn blade_document_detects_its_language() {
        let d = Document::new(Some(PathBuf::from("v/show.blade.php")), "@if(1)", true).unwrap();
        assert_eq!(d.language(), Language::Blade);
    }

    // --- byte-exact round trip ---------------------------------------------------
    //
    // Everything above tests `text_for_save` against a `Document` built by hand. That is
    // only half the pipeline: it cannot see `read_file` dropping bytes on the way in, nor
    // `write_file` reshaping them on the way out. These go through real files and compare
    // **bytes**, because issue #15's promise is that open-then-save leaves git seeing no
    // change at all.

    use elle_workspace::{read_file, write_file};

    /// Writes `original`, opens it the way the app does, applies `edit`, saves through the
    /// real save path, and returns the bytes that actually landed on disk.
    fn round_trip(original: &[u8], edit: impl FnOnce(&mut Document)) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Round.php");
        std::fs::write(&path, original).unwrap();

        let file = read_file(&path).unwrap();
        let mut d = Document::new(Some(path.clone()), &file.text, file.trailing_newline).unwrap();
        edit(&mut d);

        write_file(&path, &d.text_for_save()).unwrap();
        std::fs::read(&path).unwrap()
    }

    #[test]
    fn an_untouched_file_saves_back_byte_for_byte() {
        // The zero-edit case first: if this drifts, nothing below means anything.
        for original in [
            &b"<?php\n$x = 1;\n"[..],
            b"<?php\n$x = 1;",
            b"",
            b"\n",
            b"<?php\n// a\xc3\xa7\xc3\xa3o\n",
        ] {
            assert_eq!(round_trip(original, |_| {}), original, "{original:?} changed on save");
        }
    }

    #[test]
    fn a_file_without_a_trailing_newline_does_not_gain_one() {
        // The diff-noise case: an editor that "helpfully" adds a final newline turns a
        // one-line change into a two-line change in review.
        let out = round_trip(b"<?php\n$x = 1;", |d| {
            d.move_to(d.buffer.len_bytes(), false);
            d.insert(" // note");
        });
        assert_eq!(out, b"<?php\n$x = 1; // note");
    }

    #[test]
    fn a_file_with_a_trailing_newline_keeps_exactly_one() {
        // Both directions: deleting the final newline must not lose it, and it must not
        // silently double up either.
        let out = round_trip(b"<?php\n$x = 1;\n", |d| {
            d.move_to(d.buffer.len_bytes(), false);
            d.backspace();
        });
        assert_eq!(out, b"<?php\n$x = 1;\n");

        let out = round_trip(b"<?php\n$x = 1;\n", |d| {
            d.move_to(d.buffer.len_bytes(), false);
            d.insert("\n");
        });
        assert_eq!(out, b"<?php\n$x = 1;\n\n", "a newline the user typed is content, not padding");
    }

    #[test]
    fn crlf_line_endings_survive_an_edit() {
        // Nothing in the pipeline normalises line endings — the rope stores the bytes it was
        // handed — and this pins that down. A save that rewrote a Windows checkout to LF
        // would show every line of the file as changed.
        let out = round_trip(b"<?php\r\n$x = 1;\r\n", |d| {
            d.move_to(d.buffer.len_bytes(), false);
            d.insert("$y = 2;\r\n");
        });
        assert_eq!(out, b"<?php\r\n$x = 1;\r\n$y = 2;\r\n");
    }

    #[test]
    fn a_crlf_file_without_a_final_newline_does_not_gain_a_bare_lf() {
        // `trailing_newline` is a bool, so the restore appends a bare `\n`. On a CRLF file
        // whose last line has no ending that would introduce a lone LF into an otherwise
        // uniform file — so the restore must fire only when the buffer genuinely lost the
        // newline, never as unconditional padding.
        let out = round_trip(b"<?php\r\n$x = 1;", |d| {
            d.move_to(d.buffer.len_bytes(), false);
            d.insert(" // note");
        });
        assert_eq!(out, b"<?php\r\n$x = 1; // note");
    }

    #[test]
    fn a_bom_is_dropped_on_open_and_stays_dropped_on_save() {
        // Deliberately *not* byte-exact: `read_file` strips a UTF-8 BOM because a BOM in a
        // PHP file emits bytes before `<?php` and breaks headers. Asserting the intended
        // asymmetry so nobody folds it back into the round trip by accident.
        let out = round_trip("\u{feff}<?php\n".as_bytes(), |_| {});
        assert_eq!(out, b"<?php\n");
    }

    #[test]
    fn an_untitled_buffer_saves_exactly_what_was_typed() {
        // A new buffer has never been on disk, so there is no original newline behaviour to
        // preserve and save-as must not invent one.
        let mut d = Document::untitled().unwrap();
        d.insert("<?php");
        assert_eq!(d.text_for_save(), "<?php");
        assert_eq!(d.title(), "untitled");
        assert!(d.path.is_none(), "an untitled buffer must have no path, or ⌘S skips save-as");
    }
}
