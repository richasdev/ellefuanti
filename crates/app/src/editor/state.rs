//! Cursor and selection logic, with no gpui in sight.
//!
//! Split from the view so editing semantics — the part that is easy to get subtly wrong
//! — can be tested at full speed without opening a window.

use std::ops::Range;
use std::path::{Path, PathBuf};

use elle_syntax::{Language, SyntaxTree, language_for_path};
use elle_text::{Buffer, Point, Version};

/// What kind of run of text a character belongs to, for word motion.
///
/// Three classes, not two. `$user->name` has to stop at `$user`, `->` and `name`, which
/// means punctuation cannot be lumped in with whitespace (⌥→ would skip straight past
/// `->` to `name`) nor with word characters (`$user->name` would be one word). A run is
/// a maximal span of one class, and whitespace runs are crossed rather than stopped in.
///
/// `is_alphanumeric` is Unicode-aware, so `função` and `имя` are single words. `_` and
/// `$` count as word characters because a PHP variable is one thing to a reader.
///
/// This rules out language-aware boundaries: `$` is a word character in a `.md` file
/// too. Getting that right needs the token stream from `crates/syntax`, which is a much
/// bigger change than a motion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CharClass {
    Whitespace,
    Word,
    Punctuation,
}

impl CharClass {
    fn of(c: char) -> Self {
        if c.is_whitespace() {
            Self::Whitespace
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            Self::Word
        } else {
            Self::Punctuation
        }
    }
}

/// `text` without its line ending, if it has one. Handles `\r\n` as well as `\n`, since
/// nothing in this pipeline normalises line endings — see `crlf_line_endings_survive_an_edit`.
fn strip_newline(text: &str) -> &str {
    text.strip_suffix('\n').map_or(text, |t| t.strip_suffix('\r').unwrap_or(t))
}

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

    pub fn move_line_end(&mut self, extend: bool) {
        let row = self.buffer.offset_to_point(self.selection.head).row;
        let column = self.buffer.line_len(row);
        self.move_to(self.buffer.point_to_offset(Point::new(row, column)), extend);
        self.goal_column = None;
    }

    /// ⌘← / ⌘→ with "smart home": ⌘← goes to the first non-whitespace character, and
    /// only to column zero when already sitting on it. Toggling is what every editor
    /// with a smart home does, and it is the only way to reach column zero on an
    /// indented line without also making the common case (jump to the code) two presses.
    pub fn move_line_home(&mut self, extend: bool) {
        let row = self.buffer.offset_to_point(self.selection.head).row;
        let line_start = self.buffer.point_to_offset(Point::new(row, 0));
        let indent_end = self.first_non_whitespace(row);

        let target = if self.selection.head == indent_end { line_start } else { indent_end };
        self.move_to(target, extend);
        self.goal_column = None;
    }

    /// ⌘↑ / ⌘↓: the two ends of the document.
    pub fn move_document_edge(&mut self, end: bool, extend: bool) {
        self.move_to(if end { self.buffer.len_bytes() } else { 0 }, extend);
        self.goal_column = None;
    }

    /// ⌥← / ⌥→, with `extend` for the ⇧ variants.
    pub fn move_word(&mut self, forward: bool, extend: bool) {
        let target = if forward {
            self.next_word_boundary(self.selection.head)
        } else {
            self.prev_word_boundary(self.selection.head)
        };
        self.move_to(target, extend);
        self.goal_column = None;
    }

    pub fn select_all(&mut self) {
        self.selection = Selection { anchor: 0, head: self.buffer.len_bytes() };
        self.goal_column = None;
    }

    // --- deletion ------------------------------------------------------------------

    /// ⌥⌫ / ⌥⌦: delete to the previous/next word boundary.
    ///
    /// One `replace` call, so it is one undo group — see [`Document::delete_range`].
    pub fn delete_word(&mut self, forward: bool) {
        if !self.selection.is_empty() {
            self.delete_range(self.selection.range());
            return;
        }
        let head = self.selection.head;
        let range = if forward {
            head..self.next_word_boundary(head)
        } else {
            self.prev_word_boundary(head)..head
        };
        self.delete_range(range);
    }

    /// ⌘⌫ / ⌘⌦: delete from the cursor to the start/end of the line.
    ///
    /// Line *content*, not the line ending: ⌘⌫ at column zero does nothing rather than
    /// joining with the line above, which matches macOS and means a mistimed press
    /// cannot silently reflow the file.
    pub fn delete_to_line_edge(&mut self, forward: bool) {
        if !self.selection.is_empty() {
            self.delete_range(self.selection.range());
            return;
        }
        let head = self.selection.head;
        let row = self.buffer.offset_to_point(head).row;
        let range = if forward {
            head..self.buffer.point_to_offset(Point::new(row, self.buffer.line_len(row)))
        } else {
            self.buffer.point_to_offset(Point::new(row, 0))..head
        };
        self.delete_range(range);
    }

    /// Deletes a byte range as exactly one undo step.
    ///
    /// The grouping is the whole point. `Buffer::replace` coalesces into the previous
    /// undo group only when `Edit::extends` holds, and a deletion never extends
    /// anything — so one call is one group. The explicit `break_undo_group` guards the
    /// other direction: without it a delete-then-type sequence could let the *typing*
    /// coalesce onto the deletion's group and undo them together.
    fn delete_range(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.replace_as_one_step(range, "");
    }

    /// Applies one replacement as exactly one undo step, leaving the cursor after it.
    ///
    /// Callers that reshape a line rely on this: the break before stops the edit joining
    /// a typing run already on the stack, and the break after stops later typing joining
    /// this one. Between them `Buffer::replace` pushes exactly one group.
    fn replace_as_one_step(&mut self, range: Range<usize>, text: &str) {
        self.buffer.break_undo_group();
        let edit = self.buffer.replace(range, text);
        self.selection = Selection::at(edit.new_range().end);
        self.goal_column = None;
        self.buffer.break_undo_group();
        self.sync_syntax();
    }

    // --- line manipulation -----------------------------------------------------------

    /// ⌥↑ / ⌥↓: swaps the selected lines with the line above/below.
    ///
    /// Implemented as one `replace` over the whole span of both, rather than a delete and
    /// an insert, so it is a single undo step and the syntax tree sees one edit.
    pub fn move_lines(&mut self, down: bool) {
        let (first, last) = self.selected_rows();
        if down && last + 1 >= self.buffer.len_lines() || !down && first == 0 {
            return;
        }

        // Rewrite the span covering the block and its neighbour in one go, swapping the
        // two halves. Moving content only — `strip_newline` — and re-joining keeps the
        // newline count right even when the block reaches the last line of the file,
        // which has no trailing newline to carry with it.
        //
        // The separator is re-read from the span rather than assumed to be `\n`, so
        // moving a line in a CRLF file does not leave one lone LF behind. See
        // `crlf_line_endings_survive_an_edit` for why that matters.
        let (upper, lower) = if down {
            (first..=last, last + 1..=last + 1)
        } else {
            (first - 1..=first - 1, first..=last)
        };
        let span = self.line_span(upper.clone()).start..self.line_span(lower.clone()).end;
        let upper_text = self.buffer.slice(self.line_span(upper));
        let lower_text = self.buffer.slice(self.line_span(lower));

        // The upper block always ends in an ending (something follows it); the lower one
        // may not, if it is the last line. Reuse each as found and they stay put.
        let separator = &upper_text[strip_newline(&upper_text).len()..];
        let terminator = &lower_text[strip_newline(&lower_text).len()..];
        let new_text = format!(
            "{}{separator}{}{terminator}",
            strip_newline(&lower_text),
            strip_newline(&upper_text)
        );

        self.replace_as_one_step(span, &new_text);

        let shift = if down { 1 } else { -1 };
        self.select_rows(first.wrapping_add_signed(shift), last.wrapping_add_signed(shift));
    }

    /// ⇧⌥↑ / ⇧⌥↓: copies the selected lines above/below, leaving the copy selected.
    pub fn duplicate_lines(&mut self, down: bool) {
        let (first, last) = self.selected_rows();
        let span = self.line_span(first..=last);
        let text = strip_newline(&self.buffer.slice(span.clone())).to_string();
        let ending = self.line_ending(first);

        // Insert at the block's start either way; whether the cursor follows the copy is
        // what makes it "up" or "down".
        self.replace_as_one_step(span.start..span.start, &format!("{text}{ending}"));

        let rows = last - first + 1;
        if down {
            self.select_rows(first + rows, last + rows);
        } else {
            self.select_rows(first, last);
        }
    }

    /// ⌘⇧K: removes the selected lines entirely, line endings included.
    pub fn delete_lines(&mut self) {
        let (first, last) = self.selected_rows();
        let mut span = self.line_span(first..=last);

        // On the last line there is no trailing newline to take, so take the *leading*
        // one instead. Otherwise deleting the final line leaves a stray blank behind.
        if span.end == self.buffer.len_bytes() && first > 0 {
            span.start =
                self.buffer.point_to_offset(Point::new(first - 1, self.buffer.line_len(first - 1)));
        }

        self.replace_as_one_step(span, "");
    }

    /// ⌘Enter / ⌘⇧Enter: opens a blank line below/above, matching the current indent.
    pub fn open_line(&mut self, above: bool) {
        let row = self.buffer.offset_to_point(self.selection.head).row;
        let indent = self.indent_of(row);
        let ending = self.line_ending(row);

        if above {
            let at = self.buffer.point_to_offset(Point::new(row, 0));
            self.replace_as_one_step(at..at, &format!("{indent}{ending}"));
            self.move_to(at + indent.len(), false);
        } else {
            let at = self.buffer.point_to_offset(Point::new(row, self.buffer.line_len(row)));
            self.replace_as_one_step(at..at, &format!("{ending}{indent}"));
            self.move_to(at + ending.len() + indent.len(), false);
        }
    }

    /// Tab / ⇧Tab on a selection, and ⌘] / ⌘[ anywhere.
    ///
    /// One `replace` over the whole block rather than one per line: it is a single undo
    /// step, and per-line edits would each shift the offsets of the ones after them.
    ///
    /// ponytail: four spaces, hardcoded, matching `EditorView::tab`. Both read the same
    /// setting once a settings crate exists (#60).
    pub fn indent_lines(&mut self, outdent: bool) {
        const INDENT: &str = "    ";

        let (first, last) = self.selected_rows();
        let span = self.line_span(first..=last);
        let text = self.buffer.slice(span.clone());

        let mut out = String::with_capacity(text.len() + (last - first + 1) * INDENT.len());
        // split_inclusive keeps each line's ending attached, so the endings pass through
        // untouched and a CRLF file stays CRLF.
        for line in text.split_inclusive('\n') {
            if outdent {
                // Remove up to one indent's worth of leading whitespace, stopping early on
                // a short indent so a two-space line loses two spaces rather than nothing.
                let mut removed = 0;
                let rest = line.trim_start_matches(|c| {
                    let keep = removed < INDENT.len() && (c == ' ' || c == '\t');
                    if keep {
                        removed += 1;
                    }
                    keep
                });
                out.push_str(rest);
            } else if strip_newline(line).is_empty() {
                // Indenting a blank line would leave trailing whitespace behind.
                out.push_str(line);
            } else {
                out.push_str(INDENT);
                out.push_str(line);
            }
        }

        if out == text {
            return;
        }

        // The block stays selected, so ⇥⇥ indents twice instead of replacing the text.
        self.replace_as_one_step(span, &out);
        self.select_rows(first, last);
    }

    /// The rows the selection touches, inclusive.
    fn selected_rows(&self) -> (usize, usize) {
        let range = self.selection.range();
        let first = self.buffer.offset_to_point(range.start).row;
        let mut last = self.buffer.offset_to_point(range.end).row;
        // A selection ending exactly at column zero visually stops on the line above, so
        // ⌘⇧K on a shift-down selection does not eat an extra untouched line.
        if last > first && self.buffer.offset_to_point(range.end).column == 0 {
            last -= 1;
        }
        (first, last)
    }

    /// Byte range covering whole rows, including the final row's line ending if it has one.
    fn line_span(&self, rows: std::ops::RangeInclusive<usize>) -> Range<usize> {
        let start = self.buffer.point_to_offset(Point::new(*rows.start(), 0));
        let last = *rows.end();
        let content_end = self.buffer.point_to_offset(Point::new(last, self.buffer.line_len(last)));
        // The next row's column zero is past the ending whatever it is, so \r\n needs no
        // special case. The last row has no ending, so the span stops at its content.
        let end = if last + 1 < self.buffer.len_lines() {
            self.buffer.point_to_offset(Point::new(last + 1, 0))
        } else {
            content_end
        };
        start..end
    }

    /// The line ending `row` uses, for building a new line that matches it.
    ///
    /// Falls back to the row above when `row` is the last line and has none, so opening a
    /// line at the end of a CRLF file does not introduce a lone LF. `\n` only when the
    /// buffer has no ending to copy at all.
    fn line_ending(&self, row: usize) -> &'static str {
        // `line_len` excludes the ending, so the byte just past the content is `\r` on a
        // CRLF line and `\n` on an LF one.
        let is_crlf = |row: usize| {
            if row + 1 >= self.buffer.len_lines() {
                return false;
            }
            let end = self.buffer.point_to_offset(Point::new(row, self.buffer.line_len(row)));
            self.buffer.slice(end..end + 1) == "\r"
        };
        if is_crlf(row) || (row > 0 && is_crlf(row - 1)) { "\r\n" } else { "\n" }
    }

    /// The leading whitespace of a row, as text to reuse for a new line.
    fn indent_of(&self, row: usize) -> String {
        let start = self.buffer.point_to_offset(Point::new(row, 0));
        self.buffer.slice(start..self.first_non_whitespace(row))
    }

    fn select_rows(&mut self, first: usize, last: usize) {
        let start = self.buffer.point_to_offset(Point::new(first, 0));
        let end = self.buffer.point_to_offset(Point::new(last, self.buffer.line_len(last)));
        self.selection = Selection { anchor: start, head: end };
        self.goal_column = None;
    }

    // --- word boundaries -------------------------------------------------------------

    /// Where ⌥→ lands: past any whitespace, then past the run that follows.
    fn next_word_boundary(&self, offset: usize) -> usize {
        let rope = self.buffer.rope();
        let len = rope.len_chars();
        let mut i = rope.byte_to_char(offset);

        while i < len && CharClass::of(rope.char(i)) == CharClass::Whitespace {
            i += 1;
        }
        if i < len {
            let class = CharClass::of(rope.char(i));
            while i < len && CharClass::of(rope.char(i)) == class {
                i += 1;
            }
        }
        rope.char_to_byte(i)
    }

    /// Where ⌥← lands: back over any whitespace, then back over the run before it.
    fn prev_word_boundary(&self, offset: usize) -> usize {
        let rope = self.buffer.rope();
        let mut i = rope.byte_to_char(offset);

        while i > 0 && CharClass::of(rope.char(i - 1)) == CharClass::Whitespace {
            i -= 1;
        }
        if i > 0 {
            let class = CharClass::of(rope.char(i - 1));
            while i > 0 && CharClass::of(rope.char(i - 1)) == class {
                i -= 1;
            }
        }
        rope.char_to_byte(i)
    }

    /// Offset of the first non-whitespace character on `row`, or the line end if the
    /// line is blank or all whitespace.
    fn first_non_whitespace(&self, row: usize) -> usize {
        let line_start = self.buffer.point_to_offset(Point::new(row, 0));
        let len = self.buffer.line_len(row);
        let rope = self.buffer.rope();
        let mut i = rope.byte_to_char(line_start);
        let end = rope.byte_to_char(line_start + len);
        while i < end && rope.char(i).is_whitespace() {
            i += 1;
        }
        rope.char_to_byte(i)
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
        d.move_line_home(false);
        assert_eq!(d.cursor_point(), Point::new(0, 2), "smart home stops at the code first");
        d.move_line_home(false);
        assert_eq!(d.cursor_point(), Point::new(0, 0));
    }

    #[test]
    fn smart_home_toggles_between_indent_and_column_zero() {
        let mut d = doc("    return $x;\nplain");
        d.move_line_end(false);
        d.move_line_home(false);
        assert_eq!(d.cursor_point(), Point::new(0, 4));
        d.move_line_home(false);
        assert_eq!(d.cursor_point(), Point::new(0, 0), "already at the indent, so go to zero");
        d.move_line_home(false);
        assert_eq!(d.cursor_point(), Point::new(0, 4), "and back, so zero is never a dead end");

        // An unindented line has nothing to toggle: both stops are column zero.
        d.move_to(d.buffer.point_to_offset(Point::new(1, 3)), false);
        d.move_line_home(false);
        assert_eq!(d.cursor_point(), Point::new(1, 0));
        d.move_line_home(false);
        assert_eq!(d.cursor_point(), Point::new(1, 0));
    }

    #[test]
    fn smart_home_on_a_whitespace_only_line_stops_at_the_end() {
        // No non-whitespace to find, so the "indent end" is the line end. Pressing again
        // must still reach column zero rather than pinning the cursor at the end.
        let mut d = doc("   \nx");
        d.move_to(0, false);
        d.move_line_home(false);
        assert_eq!(d.cursor_point(), Point::new(0, 3));
        d.move_line_home(false);
        assert_eq!(d.cursor_point(), Point::new(0, 0));
    }

    // --- word motion ---------------------------------------------------------------

    #[test]
    fn word_motion_splits_a_php_member_access_into_three_stops() {
        // The rule from the issue: `$user->name` is `$user`, `->`, `name`. Punctuation is
        // its own class, so the arrow is a stop rather than something to skip over.
        let mut d = doc("$user->name;");
        d.move_to(0, false);
        for expected in ["$user", "$user->", "$user->name", "$user->name;"] {
            d.move_word(true, false);
            assert_eq!(&d.buffer.text()[..d.selection.head], expected);
        }
        // And back out again, symmetrically.
        for expected in ["$user->name", "$user->", "$user", ""] {
            d.move_word(false, false);
            assert_eq!(&d.buffer.text()[..d.selection.head], expected);
        }
    }

    #[test]
    fn word_motion_crosses_whitespace_without_stopping_in_it() {
        let mut d = doc("one   two");
        d.move_to(0, false);
        d.move_word(true, false);
        assert_eq!(d.selection.head, 3, "stops at the end of the word, not before the gap");
        d.move_word(true, false);
        assert_eq!(d.selection.head, 9, "one press crosses the gap and the next word");
        d.move_word(false, false);
        assert_eq!(d.selection.head, 6);
    }

    #[test]
    fn word_motion_stops_at_the_document_edges() {
        let mut d = doc("word");
        d.move_to(0, false);
        d.move_word(false, false);
        assert_eq!(d.selection.head, 0);
        d.move_to(4, false);
        d.move_word(true, false);
        assert_eq!(d.selection.head, 4);
    }

    #[test]
    fn word_motion_lands_on_char_boundaries_in_multibyte_text() {
        // `função` is 8 bytes over 6 chars, `назва` is 10 bytes over 5. A boundary that
        // came out mid-codepoint would panic the slicing below in a debug build — which is
        // exactly the failure mode this guards.
        let mut d = doc("$função->назva çé");
        d.move_to(0, false);
        let mut stops = vec![];
        loop {
            let before = d.selection.head;
            d.move_word(true, false);
            if d.selection.head == before {
                break;
            }
            stops.push(d.buffer.text()[..d.selection.head].to_string());
        }
        assert_eq!(stops, ["$função", "$função->", "$função->назva", "$função->назva çé"]);
    }

    #[test]
    fn shift_option_arrow_selects_a_word() {
        let mut d = doc("alpha beta");
        d.move_to(0, false);
        d.move_word(true, true);
        assert_eq!(d.selected_text().unwrap(), "alpha");
        d.move_word(true, true);
        assert_eq!(d.selected_text().unwrap(), "alpha beta");
        assert_eq!(d.selection.anchor, 0, "the anchor stays put while the head runs");
    }

    #[test]
    fn word_motion_clears_the_goal_column() {
        // Horizontal motion must not leave a stale goal behind, or the next ↓ jumps to a
        // column the user never visited.
        let mut d = doc("aaaaaaaa\nbb\ncccccccc");
        d.move_to(d.buffer.point_to_offset(Point::new(0, 5)), false);
        d.move_vertical(true, false);
        d.move_word(false, false);
        d.move_vertical(true, false);
        assert_eq!(d.cursor_point(), Point::new(2, 0), "column comes from the word motion");
    }

    // --- document edges --------------------------------------------------------------

    #[test]
    fn document_edges_and_their_selecting_variants() {
        let mut d = doc("one\ntwo\nthree");
        d.move_to(5, false);
        d.move_document_edge(true, false);
        assert_eq!(d.selection.head, d.buffer.len_bytes());
        d.move_document_edge(false, false);
        assert_eq!(d.selection.head, 0);

        d.move_to(4, false);
        d.move_document_edge(true, true);
        assert_eq!(d.selected_text().unwrap(), "two\nthree");
        d.move_to(4, false);
        d.move_document_edge(false, true);
        assert_eq!(d.selected_text().unwrap(), "one\n");
    }

    // --- deletion --------------------------------------------------------------------

    #[test]
    fn delete_word_back_and_forward() {
        let mut d = doc("$user->name;");
        d.move_to(11, false);
        d.delete_word(false);
        assert_eq!(d.buffer.text(), "$user->;");
        d.delete_word(false);
        assert_eq!(d.buffer.text(), "$user;");

        let mut d = doc("alpha beta");
        d.move_to(0, false);
        d.delete_word(true);
        assert_eq!(d.buffer.text(), " beta");
        d.delete_word(true);
        assert_eq!(d.buffer.text(), "", "one press takes the gap and the word after it");
    }

    #[test]
    fn delete_word_with_a_selection_deletes_the_selection() {
        // ⌥⌫ on a selection must not also eat the word before it.
        let mut d = doc("one two three");
        d.selection = Selection { anchor: 4, head: 7 };
        d.delete_word(false);
        assert_eq!(d.buffer.text(), "one  three");
        assert_eq!(d.selection.head, 4);
    }

    #[test]
    fn delete_word_back_on_multibyte_text_deletes_whole_chars() {
        let mut d = doc("uma ação");
        d.move_to(d.buffer.len_bytes(), false);
        d.delete_word(false);
        assert_eq!(d.buffer.text(), "uma ");
        d.delete_word(false);
        assert_eq!(d.buffer.text(), "");
    }

    #[test]
    fn delete_to_line_start_and_end_stop_at_the_line_ending() {
        let mut d = doc("    return $x;\nnext");
        d.move_to(d.buffer.point_to_offset(Point::new(0, 11)), false);
        d.delete_to_line_edge(true);
        assert_eq!(d.buffer.text(), "    return \nnext", "the newline survives");
        d.delete_to_line_edge(false);
        assert_eq!(d.buffer.text(), "\nnext");
        assert_eq!(d.cursor_point(), Point::new(0, 0));

        // At column zero there is nothing left to take, and it must not join the lines.
        d.move_to(d.buffer.point_to_offset(Point::new(1, 0)), false);
        d.delete_to_line_edge(false);
        assert_eq!(d.buffer.text(), "\nnext");
    }

    // --- undo granularity ------------------------------------------------------------
    //
    // The trap from the issue: deleting a word must undo as ONE step, not five characters.
    // `Buffer::replace` makes a fresh undo group unless `Edit::extends` holds, and a
    // deletion never extends — so each of these is one call and therefore one group.
    // Asserting it here because it is invisible until a user presses ⌘Z and is surprised.

    #[test]
    fn deleting_a_word_undoes_in_one_step() {
        let mut d = doc("alpha beta gamma");
        d.move_to(d.buffer.len_bytes(), false);
        d.delete_word(false);
        assert_eq!(d.buffer.text(), "alpha beta ");
        d.undo();
        assert_eq!(d.buffer.text(), "alpha beta gamma", "one undo, not five");
        d.undo();
        assert_eq!(d.buffer.text(), "alpha beta gamma", "and nothing else was on the stack");
    }

    #[test]
    fn deleting_to_the_line_start_undoes_in_one_step() {
        let mut d = doc("    return $x;");
        d.move_to(d.buffer.len_bytes(), false);
        d.delete_to_line_edge(false);
        assert_eq!(d.buffer.text(), "");
        d.undo();
        assert_eq!(d.buffer.text(), "    return $x;");
    }

    #[test]
    fn typing_then_deleting_a_word_are_two_separate_undo_steps() {
        // The direction the explicit break guards. Without it the typing run and the
        // deletion could land in one group and ⌘Z would undo both at once.
        let mut d = doc("");
        d.insert("hello");
        d.insert(" world");
        d.delete_word(false);
        assert_eq!(d.buffer.text(), "hello ");
        d.undo();
        assert_eq!(d.buffer.text(), "hello world", "the deletion undid on its own");
        d.undo();
        assert_eq!(d.buffer.text(), "", "and the typing run is a separate step");
    }

    #[test]
    fn deleting_a_word_then_typing_are_two_separate_undo_steps() {
        let mut d = doc("alpha beta");
        d.move_to(d.buffer.len_bytes(), false);
        d.delete_word(false);
        d.insert("gamma");
        assert_eq!(d.buffer.text(), "alpha gamma");
        d.undo();
        assert_eq!(d.buffer.text(), "alpha ", "the typing undid without taking the deletion");
        d.undo();
        assert_eq!(d.buffer.text(), "alpha beta");
    }

    #[test]
    fn two_word_deletions_are_two_undo_steps() {
        let mut d = doc("one two three");
        d.move_to(d.buffer.len_bytes(), false);
        d.delete_word(false);
        d.delete_word(false);
        assert_eq!(d.buffer.text(), "one ");
        d.undo();
        assert_eq!(d.buffer.text(), "one two ");
        d.undo();
        assert_eq!(d.buffer.text(), "one two three");
    }

    // --- line manipulation -----------------------------------------------------------

    #[test]
    fn move_line_up_and_down_swaps_with_the_neighbour() {
        let mut d = doc("one\ntwo\nthree\n");
        d.move_to(d.buffer.point_to_offset(Point::new(1, 1)), false);
        d.move_lines(true);
        assert_eq!(d.buffer.text(), "one\nthree\ntwo\n");
        d.move_lines(false);
        assert_eq!(d.buffer.text(), "one\ntwo\nthree\n");
        d.move_lines(false);
        assert_eq!(d.buffer.text(), "two\none\nthree\n");
    }

    #[test]
    fn move_line_stops_at_the_document_edges() {
        let mut d = doc("one\ntwo");
        d.move_to(0, false);
        d.move_lines(false);
        assert_eq!(d.buffer.text(), "one\ntwo", "nothing above the first line");
        d.move_to(d.buffer.len_bytes(), false);
        d.move_lines(true);
        assert_eq!(d.buffer.text(), "one\ntwo", "nothing below the last");
    }

    #[test]
    fn moving_the_last_line_does_not_glue_two_lines_together() {
        // The last line has no trailing newline. A swap that carried the ending along
        // with the text instead of with the position would produce "twoone".
        let mut d = doc("one\ntwo");
        d.move_to(d.buffer.len_bytes(), false);
        d.move_lines(false);
        assert_eq!(d.buffer.text(), "two\none");
        d.move_lines(true);
        assert_eq!(d.buffer.text(), "one\ntwo");
    }

    #[test]
    fn move_line_carries_a_multi_line_selection_and_keeps_it_selected() {
        let mut d = doc("a\nb\nc\nd\n");
        d.selection = Selection {
            anchor: d.buffer.point_to_offset(Point::new(0, 0)),
            head: d.buffer.point_to_offset(Point::new(1, 1)),
        };
        d.move_lines(true);
        assert_eq!(d.buffer.text(), "c\na\nb\nd\n");
        assert_eq!(d.selected_text().unwrap(), "a\nb", "the same lines stay selected");
        d.move_lines(true);
        assert_eq!(d.buffer.text(), "c\nd\na\nb\n");
    }

    #[test]
    fn move_line_undoes_in_one_step() {
        let mut d = doc("one\ntwo\nthree\n");
        d.move_to(0, false);
        d.move_lines(true);
        assert_eq!(d.buffer.text(), "two\none\nthree\n");
        d.undo();
        assert_eq!(d.buffer.text(), "one\ntwo\nthree\n", "one undo, not a delete plus an insert");
    }

    #[test]
    fn duplicate_line_copies_above_or_below() {
        let mut d = doc("one\ntwo\n");
        d.move_to(d.buffer.point_to_offset(Point::new(0, 1)), false);
        d.duplicate_lines(true);
        assert_eq!(d.buffer.text(), "one\none\ntwo\n");
        assert_eq!(d.cursor_point().row, 1, "the cursor follows the copy downwards");

        let mut d = doc("one\ntwo\n");
        d.move_to(d.buffer.point_to_offset(Point::new(1, 0)), false);
        d.duplicate_lines(false);
        assert_eq!(d.buffer.text(), "one\ntwo\ntwo\n");
        assert_eq!(d.cursor_point().row, 1, "duplicating upwards leaves the cursor on top");
    }

    #[test]
    fn duplicating_the_last_line_gains_exactly_one_newline() {
        let mut d = doc("one\ntwo");
        d.move_to(d.buffer.len_bytes(), false);
        d.duplicate_lines(true);
        assert_eq!(d.buffer.text(), "one\ntwo\ntwo");
    }

    #[test]
    fn duplicate_line_undoes_in_one_step() {
        let mut d = doc("a\nb\n");
        d.move_to(0, false);
        d.duplicate_lines(true);
        assert_eq!(d.buffer.text(), "a\na\nb\n");
        d.undo();
        assert_eq!(d.buffer.text(), "a\nb\n");
    }

    #[test]
    fn delete_line_takes_the_line_ending_with_it() {
        let mut d = doc("one\ntwo\nthree\n");
        d.move_to(d.buffer.point_to_offset(Point::new(1, 2)), false);
        d.delete_lines();
        assert_eq!(d.buffer.text(), "one\nthree\n");
        d.undo();
        assert_eq!(d.buffer.text(), "one\ntwo\nthree\n", "one undo step");
    }

    #[test]
    fn deleting_the_last_line_leaves_no_stray_blank() {
        // There is no trailing newline to remove, so the *leading* one has to go instead.
        let mut d = doc("one\ntwo");
        d.move_to(d.buffer.len_bytes(), false);
        d.delete_lines();
        assert_eq!(d.buffer.text(), "one");

        // And deleting the only line empties the buffer rather than underflowing.
        let mut d = doc("only");
        d.move_to(2, false);
        d.delete_lines();
        assert_eq!(d.buffer.text(), "");
    }

    #[test]
    fn delete_line_on_a_selection_that_ends_at_column_zero_spares_the_untouched_line() {
        // Shift-down from line 0 selects "a\n", whose end sits on row 1 column 0. The user
        // never saw row 1 highlighted, so ⌘⇧K must not take it.
        let mut d = doc("a\nb\nc\n");
        d.selection = Selection { anchor: 0, head: d.buffer.point_to_offset(Point::new(1, 0)) };
        d.delete_lines();
        assert_eq!(d.buffer.text(), "b\nc\n");
    }

    #[test]
    fn open_line_below_and_above_match_the_indentation() {
        let mut d = doc("    $x = 1;\nplain\n");
        d.move_to(d.buffer.point_to_offset(Point::new(0, 6)), false);
        d.open_line(false);
        assert_eq!(d.buffer.text(), "    $x = 1;\n    \nplain\n");
        assert_eq!(d.cursor_point(), Point::new(1, 4), "cursor sits after the indent");

        let mut d = doc("    $x = 1;\n");
        d.move_to(d.buffer.point_to_offset(Point::new(0, 6)), false);
        d.open_line(true);
        assert_eq!(d.buffer.text(), "    \n    $x = 1;\n");
        assert_eq!(d.cursor_point(), Point::new(0, 4));
    }

    #[test]
    fn open_line_undoes_in_one_step() {
        let mut d = doc("  a\n");
        d.move_to(3, false);
        d.open_line(false);
        assert_eq!(d.buffer.text(), "  a\n  \n");
        d.undo();
        assert_eq!(d.buffer.text(), "  a\n");
    }

    #[test]
    fn line_manipulation_is_multibyte_safe() {
        // Byte arithmetic on these lines would land mid-codepoint; the assertions below
        // would panic before they could fail.
        let mut d = doc("  função\nação\nçé\n");
        d.move_to(d.buffer.point_to_offset(Point::new(1, 0)), false);
        d.move_lines(false);
        assert_eq!(d.buffer.text(), "ação\n  função\nçé\n");
        d.duplicate_lines(true);
        assert_eq!(d.buffer.text(), "ação\nação\n  função\nçé\n");
        d.delete_lines();
        assert_eq!(d.buffer.text(), "ação\n  função\nçé\n");

        d.move_to(d.buffer.point_to_offset(Point::new(1, 4)), false);
        d.open_line(false);
        assert_eq!(d.buffer.text(), "ação\n  função\n  \nçé\n");
        assert_eq!(d.cursor_point(), Point::new(2, 2));
    }

    // --- indentation -----------------------------------------------------------------

    #[test]
    fn indent_and_outdent_shift_every_selected_line() {
        let mut d = doc("a\nb\nc\n");
        d.selection = Selection { anchor: 0, head: d.buffer.point_to_offset(Point::new(1, 1)) };
        d.indent_lines(false);
        assert_eq!(d.buffer.text(), "    a\n    b\nc\n");
        assert_eq!(d.selected_text().unwrap(), "    a\n    b", "the block stays selected");
        d.indent_lines(false);
        assert_eq!(d.buffer.text(), "        a\n        b\nc\n", "so a second ⇥ indents again");
        d.indent_lines(true);
        d.indent_lines(true);
        assert_eq!(d.buffer.text(), "a\nb\nc\n");
    }

    #[test]
    fn outdent_removes_a_short_indent_rather_than_nothing() {
        // Two spaces is less than one indent's worth; taking what is there beats a no-op.
        let mut d = doc("  a\n\tb\nc\n");
        d.selection = Selection { anchor: 0, head: d.buffer.len_bytes() };
        d.indent_lines(true);
        assert_eq!(d.buffer.text(), "a\nb\nc\n");
        // Nothing left to remove, so the whole call is a no-op and pushes no undo step.
        d.indent_lines(true);
        assert_eq!(d.buffer.text(), "a\nb\nc\n");
        d.undo();
        assert_eq!(d.buffer.text(), "  a\n\tb\nc\n", "one undo, and nothing empty on the stack");
    }

    #[test]
    fn indent_leaves_blank_lines_alone() {
        // Indenting a blank line would leave trailing whitespace for a linter to flag.
        let mut d = doc("a\n\nb\n");
        d.selection = Selection { anchor: 0, head: d.buffer.len_bytes() };
        d.indent_lines(false);
        assert_eq!(d.buffer.text(), "    a\n\n    b\n");
    }

    #[test]
    fn indent_undoes_in_one_step_and_is_multibyte_safe() {
        let mut d = doc("função\nação\n");
        d.selection = Selection { anchor: 0, head: d.buffer.len_bytes() };
        d.indent_lines(false);
        assert_eq!(d.buffer.text(), "    função\n    ação\n");
        d.undo();
        assert_eq!(d.buffer.text(), "função\nação\n", "two lines, one undo");
    }

    #[test]
    fn indent_with_a_bare_cursor_still_shifts_the_line() {
        // ⌘] has no selection to work from, so it acts on the line the cursor is in.
        let mut d = doc("a\nb\n");
        d.move_to(d.buffer.point_to_offset(Point::new(1, 1)), false);
        d.indent_lines(false);
        assert_eq!(d.buffer.text(), "a\n    b\n");
    }

    #[test]
    fn line_manipulation_keeps_crlf_endings_crlf() {
        // Nothing in this pipeline normalises endings (see `crlf_line_endings_survive_an
        // _edit`), so a line motion must not be the thing that introduces a lone LF into
        // a Windows checkout and shows every line as changed in review.
        let mut d = doc("one\r\ntwo\r\nthree\r\n");
        d.move_to(0, false);
        d.move_lines(true);
        assert_eq!(d.buffer.text(), "two\r\none\r\nthree\r\n");
        d.duplicate_lines(true);
        assert_eq!(d.buffer.text(), "two\r\none\r\none\r\nthree\r\n");
        d.open_line(false);
        assert_eq!(d.buffer.text(), "two\r\none\r\none\r\n\r\nthree\r\n");

        // Including at the end of the file, where the last line has no ending of its own
        // to copy and the row above has to supply it.
        let mut d = doc("one\r\ntwo");
        d.move_to(d.buffer.len_bytes(), false);
        d.open_line(false);
        assert_eq!(d.buffer.text(), "one\r\ntwo\r\n");
    }

    #[test]
    fn line_manipulation_keeps_the_syntax_tree_in_sync() {
        let mut d = doc("<?php\n$a = 1;\n$b = 2;\n");
        d.move_to(d.buffer.point_to_offset(Point::new(1, 0)), false);
        d.move_lines(true);
        assert!(!d.buffer.has_pending(), "move_lines must drain");
        d.duplicate_lines(true);
        assert!(!d.buffer.has_pending(), "duplicate_lines must drain");
        d.delete_lines();
        assert!(!d.buffer.has_pending(), "delete_lines must drain");
        d.open_line(false);
        assert!(!d.buffer.has_pending(), "open_line must drain");
        d.insert("$c = 3;");
        assert!(!d.syntax.has_error(), "the tree must still agree with the text");
    }

    #[test]
    fn word_deletion_keeps_the_syntax_tree_in_sync() {
        // Same invariant `a_path_swap_never_happens_with_edits_outstanding` pins for the
        // older mutators: every new mutating path must drain the edit log too.
        let mut d = doc("<?php\n$name = 1;\n");
        d.move_to(d.buffer.point_to_offset(Point::new(1, 5)), false);
        d.delete_word(false);
        assert!(!d.buffer.has_pending(), "delete_word must drain");
        d.delete_to_line_edge(true);
        assert!(!d.buffer.has_pending(), "delete_to_line_edge must drain");
        d.insert("$x = 1;");
        assert!(!d.syntax.has_error(), "the tree must still agree with the text");
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
