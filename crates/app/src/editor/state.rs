//! Cursor and selection logic, with no gpui in sight.
//!
//! Split from the view so editing semantics — the part that is easy to get subtly wrong
//! — can be tested at full speed without opening a window.

use std::ops::Range;
use std::path::{Path, PathBuf};

use elle_syntax::{Language, SyntaxTree, language_for_path};
use elle_text::{Buffer, Point, Version};

use crate::editor::find::{Matches, SearchQuery};

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
///
/// The variant order is load-bearing and matches Zed's `CharKind`
/// (`crates/language/src/buffer.rs:581`), which derives `Ord` in the order
/// `Whitespace, Punctuation, Word`. [`Document::select_word_at`] relies on that ordering —
/// see its doc comment.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum CharClass {
    Whitespace,
    Punctuation,
    Word,
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

/// Find state for one document (#80).
///
/// Lives beside the cursor rather than in the find bar because the *matches* are a fact
/// about the buffer, not about the widget: they have to survive the bar losing focus, and
/// they have to be invalidated when the text changes underneath them. A find bar owning
/// them would have to be told about every edit.
#[derive(Default)]
pub struct Search {
    pub query: SearchQuery,
    matches: Matches,
    /// Buffer version `matches` was computed from. A mismatch means a rescan is due, which
    /// is what makes typing in the *document* while the find bar is open still correct.
    version: Version,
    /// Which match is the "current" one, for `3 of 17` and for ⌘G. `None` before the first
    /// next/prev, so opening the bar highlights every hit without claiming one is selected.
    current: Option<usize>,
}

impl Search {
    pub fn matches(&self) -> &Matches {
        &self.matches
    }

    /// The `(current, total)` a find bar shows, 1-based. `None` when there is nothing to
    /// count — an empty query — so the bar can render nothing rather than "0 of 0".
    pub fn position(&self) -> Option<(Option<usize>, usize)> {
        if self.query.pattern.is_empty() {
            return None;
        }
        Some((self.current.map(|index| index + 1), self.matches.len()))
    }

    pub fn current_range(&self) -> Option<Range<usize>> {
        self.matches.get(self.current?).cloned()
    }
}

/// One open document: text, parse state, cursor, and where it came from.
pub struct Document {
    pub path: Option<PathBuf>,
    pub buffer: Buffer,
    pub syntax: SyntaxTree,
    pub selection: Selection,
    /// Additional cursors beyond [`selection`](Self::selection), for ⌘D (#82).
    ///
    /// The primary stays in its own field on purpose: 51 call sites read
    /// `self.selection` as "the cursor" — the LSP offset, the completion anchor, the
    /// status bar — and for all of them the primary *is* the right answer. Extras are
    /// consulted only by the operations that apply everywhere (typing, backspace,
    /// delete) and by the renderer; kept sorted by position and never overlapping the
    /// primary or each other, which [`Document::add_selection`] enforces at the door.
    ///
    /// Motions apply per cursor since stage 2 ([`Document::for_each_cursor`]): arrows,
    /// word and line moves keep the pack, with colliding cursors merging. What still
    /// collapses is a *placed* cursor — a plain click, a jump — through [`move_to`]'s
    /// rule, plus Escape, which is the deliberate way out.
    extra_selections: Vec<Selection>,
    /// Column the cursor "wants" during vertical motion, so moving down through a short
    /// line and back out returns to the original column instead of sticking.
    goal_column: Option<usize>,
    /// Whether the file had a trailing newline when loaded; preserved on save.
    pub trailing_newline: bool,
    /// Find and replace state. Empty query means find is off, so this costs nothing until
    /// ⌘F is pressed.
    pub search: Search,
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
            extra_selections: Vec::new(),
            trailing_newline,
            search: Search::default(),
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

    /// Sets the language explicitly, without giving the document a path.
    ///
    /// # Why this exists separately from `set_path`
    ///
    /// #127: `untitled()` produces a pathless buffer, which is correct — it is what makes
    /// ⌘S fall through to save-as — but language detection runs off the path, so an
    /// untitled buffer is plain text with no way to become anything else. The only way to
    /// get syntax colour into a scratch buffer was to save it first, which is backwards:
    /// people open a scratch buffer to try something *before* deciding where it lives.
    ///
    /// This is also the override for a file whose name lies about its contents, which
    /// detection cannot get right in principle — a `.txt` holding SQL, a config file with
    /// no extension. Every comparable editor puts that control in the status bar.
    ///
    /// The parse tree is rebuilt because the old one was produced by a different grammar,
    /// or by none at all. On failure the document falls back to plain text rather than
    /// keeping a tree that does not match the language it now claims — the same trade
    /// `set_path` makes, and for the same reason: a stale tree colours confidently and
    /// wrongly, which is worse than no colour.
    pub fn set_language(&mut self, language: Language) -> anyhow::Result<()> {
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

    /// File name for a tab label.
    pub fn title(&self) -> String {
        self.path
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "untitled".to_string())
    }

    /// Converts an LSP position — line plus UTF-16 character — into a byte-column [`Point`].
    ///
    /// # Why this lives on the document
    ///
    /// The server counts columns in UTF-16 code units and a `Point` column is a byte
    /// offset; the two agree only on ASCII. Converting needs the line's actual text, so
    /// for a long time definition jumps landed at column 0 with a comment explaining that
    /// the buffer "does not exist until the file has loaded" — true at the call site it
    /// was written for, and the reason the fix is *here*: by the time anything reveals a
    /// position, a document exists, and it can do the conversion the free function could
    /// not. Landing at line start was correct but read as "almost worked" next to every
    /// IDE that puts the cursor on the identifier.
    ///
    /// Clamps in both axes: a line past EOF becomes the last line, a character past the
    /// end of its line becomes the line's end. Servers answer from *their* copy of the
    /// text, and a stale answer must land somewhere sane rather than panic on a slice.
    pub fn point_from_lsp(&self, line: usize, character_utf16: u32) -> Point {
        let row = line.min(self.buffer.len_lines().saturating_sub(1));
        let text = self.buffer.line(row);

        let mut utf16_seen: u32 = 0;
        let mut byte_column = 0;
        for c in text.chars() {
            if utf16_seen >= character_utf16 {
                break;
            }
            utf16_seen += c.len_utf16() as u32;
            byte_column += c.len_utf8();
        }
        Point::new(row, byte_column)
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

    /// Replaces `range` with `text` and reports where it landed, applying no typing rules.
    ///
    /// The splice behind IME composition (#18). "Plainly" is the entire specification and
    /// is a deliberate contrast with [`Self::insert`] and [`Self::insert_with_pairs`]: no
    /// auto-closing, no `=` → `=>`, no selection wrapping. Text the OS is still composing is
    /// provisional — the user has not decided it is a `"` yet — and a convenience applied to
    /// it inserts characters *outside* the marked range, which the next composition step
    /// then fails to replace, stranding them in the buffer.
    ///
    /// Returns the byte offset the text starts at, which is what the caller needs to record
    /// the new marked range. `Buffer::replace` clamps, so a stale range from a platform
    /// whose copy of the document is one edit behind splices at the end rather than
    /// panicking.
    ///
    /// The cursor is left at the end of the inserted text; the IME usually moves it again
    /// immediately, to its own idea of the caret within the composition.
    pub fn replace_range_plainly(&mut self, range: Range<usize>, text: &str) -> usize {
        let edit = self.buffer.replace(range, text);
        let new_range = edit.new_range();
        self.selection = Selection::at(new_range.end);
        self.goal_column = None;
        self.sync_syntax();
        new_range.start
    }

    /// Backspace: deletes the selection, or one character before the cursor.
    pub fn backspace(&mut self) {
        let range = if self.selection.is_empty() {
            let head = self.selection.head;
            if head == 0 {
                return;
            }
            match self.indent_backspace_target(head) {
                // Inside a line's leading whitespace: back up to the previous tab stop
                // rather than one space at a time, which is what every editor does and what
                // makes a blank indented line one keystroke to clear instead of four.
                Some(target) => target..head,
                // Step back one *character*, not one byte, or a multi-byte char corrupts.
                None => self.prev_char_offset(head)..head,
            }
        } else {
            self.selection.range()
        };
        let edit = self.buffer.replace(range, "");
        self.selection = Selection::at(edit.range.start);
        self.goal_column = None;
        self.sync_syntax();
    }

    /// Where backspace should land when the cursor sits inside a line's leading whitespace.
    ///
    /// `None` means "delete one character", the ordinary case.
    ///
    /// The rule is Zed's, and it is not "jump to column zero": back up to the previous
    /// multiple of the indent width. `((column - 1) / width) * width` — so from column 8 you
    /// land on 4, from 7 on 4, from 4 on 0. A line of nothing but indentation clears in one
    /// keystroke per level instead of one per space, and a cursor sitting at an odd column
    /// (someone typed a space by hand) is pulled back into alignment rather than staying
    /// off-grid.
    ///
    /// Only *leading* whitespace qualifies. A space in the middle of a line is a space
    /// someone typed between words, and eating four of them would be the editor guessing.
    /// Tabs are one column each here, matching how `indent_guide_columns` counts them —
    /// expanding them properly needs a tab-width setting (#60), and both must move together.
    fn indent_backspace_target(&self, head: usize) -> Option<usize> {
        const WIDTH: usize = 4;

        let point = self.buffer.offset_to_point(head);
        if point.column == 0 {
            return None;
        }

        // Everything before the cursor on this line must be indentation. `line` includes the
        // whole row, so it is sliced to the cursor first — a cursor in the middle of `x = 1`
        // is not indentation even though the line starts with spaces.
        let line = self.buffer.line(point.row);
        let before = line.get(..point.column)?;
        if !before.chars().all(|c| c == ' ' || c == '\t') {
            return None;
        }

        let target_column = ((point.column - 1) / WIDTH) * WIDTH;
        if target_column == point.column {
            return None;
        }
        Some(self.buffer.point_to_offset(Point::new(point.row, target_column)))
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
        if !self.has_multiple_cursors() {
            return (!self.selection.is_empty()).then(|| self.buffer.slice(self.selection.range()));
        }
        // Multiple selections copy as one string, newline-joined in buffer order — what
        // VS Code puts on the clipboard for the same gesture, and the only join that
        // pastes back readably. Empty carets contribute nothing rather than blank lines.
        let pieces: Vec<String> = self
            .all_selections()
            .iter()
            .filter(|selection| !selection.is_empty())
            .map(|selection| self.buffer.slice(selection.range()))
            .collect();
        (!pieces.is_empty()).then(|| pieces.join("\n"))
    }

    // --- motion ------------------------------------------------------------------

    /// Moves the cursor, collapsing or extending the selection.
    pub fn move_to(&mut self, offset: usize, extend: bool) {
        // The collapse rule (#82): *placing* the cursor — a click, a definition jump, a
        // palette landing — returns to one. Motions no longer come through here as
        // collapses; `for_each_cursor` empties the extras before running a motion per
        // cursor, so this clear is a no-op on that path and the policy for every other.
        self.extra_selections.clear();
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
        if self.has_multiple_cursors() {
            return self.for_each_cursor(|d| d.move_horizontal(forward, extend));
        }
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
        if self.has_multiple_cursors() {
            return self.for_each_cursor(|d| d.move_vertical(down, extend));
        }
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
        if self.has_multiple_cursors() {
            return self.for_each_cursor(|d| d.move_line_end(extend));
        }
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
        if self.has_multiple_cursors() {
            return self.for_each_cursor(|d| d.move_line_home(extend));
        }
        let row = self.buffer.offset_to_point(self.selection.head).row;
        let line_start = self.buffer.point_to_offset(Point::new(row, 0));
        let indent_end = self.first_non_whitespace(row);

        let target = if self.selection.head == indent_end { line_start } else { indent_end };
        self.move_to(target, extend);
        self.goal_column = None;
    }

    /// ⌘↑ / ⌘↓: the two ends of the document.
    pub fn move_document_edge(&mut self, end: bool, extend: bool) {
        if self.has_multiple_cursors() {
            return self.for_each_cursor(|d| d.move_document_edge(end, extend));
        }
        self.move_to(if end { self.buffer.len_bytes() } else { 0 }, extend);
        self.goal_column = None;
    }

    /// ⌥← / ⌥→, with `extend` for the ⇧ variants.
    pub fn move_word(&mut self, forward: bool, extend: bool) {
        if self.has_multiple_cursors() {
            return self.for_each_cursor(|d| d.move_word(forward, extend));
        }
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

    /// Double-click: selects the run of one character class around `offset`.
    ///
    /// Ported from Zed's `BufferSnapshot::surrounding_word`
    /// (`crates/language/src/buffer.rs:4179`), whose whole shape is three lines that are
    /// easy to get wrong by reasoning:
    ///
    /// ```text
    /// let word_kind = cmp::max(
    ///     prev_chars.peek().copied().map(|c| classifier.kind(c)),
    ///     next_chars.peek().copied().map(|c| classifier.kind(c)),
    /// );
    /// ```
    ///
    /// **`max`, not "the class to the right".** A click lands *between* two characters, so
    /// there are two candidate classes, and `CharKind`'s ordering
    /// (`Whitespace < Punctuation < Word`, `buffer.rs:581`) makes `max` mean "prefer the
    /// more interesting side". Double-clicking just past the end of `name` therefore selects
    /// `name` rather than the space after it, and double-clicking between `->` and `name`
    /// selects `name` rather than `->`. Picking one side unconditionally gets one of those
    /// two cases wrong, and which one depends on which side you picked — this is exactly the
    /// kind of rule the task brief means by "read, not reasoned about".
    ///
    /// **`&& ch != '\n'`** in both loops (`buffer.rs:4195`, `4089`) is the other detail: a
    /// newline is whitespace, so a double-click in the indentation of a line would otherwise
    /// run through the line break and swallow the blank lines around it. Selection stays on
    /// one line.
    ///
    /// The `.take(128)` cap Zed applies at `buffer.rs:4186` is adopted too — it bounds the
    /// scan on a minified line, the same concern `MAX_MEASURE_BYTES` covers for rendering.
    /// A 128-character "word" is not one, so the cap changes no real outcome.
    ///
    /// Not adopted: Zed's `CharScopeContext` / language-scope `word_characters`
    /// (`buffer.rs:5845`), which is how PHP declares `$` a word character. This editor
    /// hardcodes `$` in [`CharClass::of`] instead and that limitation is already documented
    /// there; wiring per-language scopes is the same much-bigger change.
    pub fn select_word_at(&mut self, offset: usize) {
        let Some((_, start, end)) = self.class_run_at(offset) else {
            // Empty buffer: there is no run to select.
            self.selection = Selection::at(0);
            self.goal_column = None;
            return;
        };

        let rope = self.buffer.rope();
        self.selection =
            Selection { anchor: rope.char_to_byte(start), head: rope.char_to_byte(end) };
        self.goal_column = None;
        // A click is a jump: the same reason `move_to` breaks the run.
        self.buffer.break_undo_group();
    }

    /// The run of one character class around `offset`: `(class, start, end)` in **chars**.
    ///
    /// The shared core of double-click selection and the ⌘-hover link hint — extracted so
    /// the two cannot disagree about what "the word here" means. All of `select_word_at`'s
    /// documented subtleties (the `max` of both sides, the newline stop, the 128 cap) live
    /// here now; see that method's comment for why each is the way it is.
    fn class_run_at(&self, offset: usize) -> Option<(CharClass, usize, usize)> {
        let rope = self.buffer.rope();
        let len = rope.len_chars();
        let mid = rope.byte_to_char(offset.min(self.buffer.len_bytes()));

        let prev = (mid > 0).then(|| CharClass::of(rope.char(mid - 1)));
        let next = (mid < len).then(|| CharClass::of(rope.char(mid)));
        // `Option`'s own `Ord` puts `None` below every `Some`, which is what Zed's
        // `cmp::max` over two `Option<CharKind>` relies on: one side being past the end of
        // the buffer must not win.
        let kind = prev.max(next)?;

        const MAX_SCAN: usize = 128;

        let mut start = mid;
        while start > 0 && mid - start < MAX_SCAN {
            let ch = rope.char(start - 1);
            if CharClass::of(ch) != kind || ch == '\n' {
                break;
            }
            start -= 1;
        }

        let mut end = mid;
        while end < len && end - mid < MAX_SCAN {
            let ch = rope.char(end);
            if CharClass::of(ch) != kind || ch == '\n' {
                break;
            }
            end += 1;
        }

        Some((kind, start, end))
    }

    // --- multiple cursors (#82, stage 1) -----------------------------------------

    /// Every selection, primary last, sorted by position for the callers that edit.
    pub fn all_selections(&self) -> Vec<Selection> {
        let mut all = self.extra_selections.clone();
        all.push(self.selection);
        all.sort_by_key(|selection| selection.range().start);
        all
    }

    /// The extra cursors' head offsets, for the renderer's caret pass.
    pub fn extra_selection_heads(&self) -> Vec<usize> {
        self.extra_selections.iter().map(|selection| selection.head).collect()
    }

    /// Whether more than one cursor is live — what the renderer and Escape ask.
    pub fn has_multiple_cursors(&self) -> bool {
        !self.extra_selections.is_empty()
    }

    /// Collapses back to the primary cursor. Escape's half of ⌘D.
    pub fn clear_extra_selections(&mut self) {
        self.extra_selections.clear();
    }

    /// Runs a single-cursor motion once per cursor, keeping all of them (#82, stage 2).
    ///
    /// # How a one-cursor method becomes an every-cursor method
    ///
    /// Each motion's arithmetic reads `self.selection` — so this swaps every cursor into
    /// that seat in turn, runs the unchanged motion, and collects where it landed. The
    /// motion body never learns multiple cursors exist, which is the entire trick: the
    /// six movement methods gained one guard line each instead of a rewrite.
    ///
    /// The extras are emptied first, because the motions funnel through [`move_to`],
    /// whose stage-1 rule clears extras — with the list already empty the clear is a
    /// no-op, and the collapse rule stays intact for genuinely single-cursor calls.
    ///
    /// Cursors that land on the same spot merge, keeping one — arrow-right at the end of
    /// two adjacent words herds both cursors to the same boundary, and two carets in one
    /// place are indistinguishable on screen while typing twice the text.
    ///
    /// **Motions only.** An *editing* action routed through here would corrupt: each
    /// edit shifts the offsets the queued cursors were captured at. Edits go through
    /// `splice_at`, which was built for exactly that.
    fn for_each_cursor(&mut self, motion: impl Fn(&mut Self)) {
        let mut pending = std::mem::take(&mut self.extra_selections);
        pending.push(self.selection);

        let mut landed: Vec<Selection> = Vec::with_capacity(pending.len());
        for cursor in pending {
            self.selection = cursor;
            motion(self);
            landed.push(self.selection);
        }

        // The last seat run was the old primary; it stays primary.
        let primary = landed.pop().unwrap_or(Selection::at(0));
        landed.sort_by_key(|selection| selection.range().start);
        landed.dedup_by_key(|selection| (selection.anchor, selection.head));
        landed.retain(|selection| {
            (selection.anchor, selection.head) != (primary.anchor, primary.head)
        });
        self.selection = primary;
        self.extra_selections = landed;
    }

    /// Replaces every cursor at once — the column-selection door (#82).
    ///
    /// The primary is the caller's choice (the row under the pointer, for a drag);
    /// extras are sorted and deduplicated here so no caller can construct the
    /// overlapping state the editing paths assume away.
    pub fn set_selections(&mut self, primary: Selection, mut extras: Vec<Selection>) {
        extras.retain(|extra| extra.range() != primary.range());
        extras.sort_by_key(|selection| selection.range().start);
        extras.dedup_by_key(|selection| selection.range().start);
        self.selection = primary;
        self.extra_selections = extras;
        self.goal_column = None;
        self.buffer.break_undo_group();
    }

    /// ⌥click: adds a caret at `offset`, or removes the one already there.
    ///
    /// The toggle is VS Code's rule and the right one: an accidental extra caret must be
    /// removable by the same gesture that made it, not only by Escape-and-rebuild. The
    /// clicked position becomes the primary either way — it is where the user is looking.
    ///
    /// Clicking a caret away when it is the *last* extra simply collapses to one cursor;
    /// clicking away the only cursor is refused, because zero cursors is not a state.
    pub fn add_cursor_at(&mut self, offset: usize) {
        let offset = offset.min(self.buffer.len_bytes());

        // On an existing caret: remove it (the primary swaps in an extra if needed).
        if self.selection.is_empty() && self.selection.head == offset {
            if let Some(next_primary) = self.extra_selections.pop() {
                self.selection = next_primary;
            }
            return;
        }
        if let Some(at) =
            self.extra_selections.iter().position(|sel| sel.is_empty() && sel.head == offset)
        {
            self.extra_selections.remove(at);
            return;
        }

        // Somewhere new: the old primary joins the extras, the click leads.
        self.extra_selections.push(self.selection);
        self.selection = Selection::at(offset);
        self.extra_selections.sort_by_key(|selection| selection.range().start);
        self.buffer.break_undo_group();
        self.goal_column = None;
    }

    /// ⌘D: selects the word under the cursor, or adds the next occurrence of it.
    ///
    /// The two presses are one gesture in every editor that has this: the first names
    /// *what* to match (the word under the caret, or whatever is already selected), each
    /// further press adds the next place it occurs, wrapping at the end of the buffer.
    /// The newest match becomes the primary — it is the one the user is looking at, so it
    /// is the one the viewport should follow.
    ///
    /// Matching is literal bytes, not word-bounded: selecting `name` also finds the
    /// `name` inside `username`, which is what VS Code does with an explicit selection
    /// and the simplest honest rule. Nothing is added when the buffer holds no further
    /// occurrence — ⌘D at saturation is a no-op, not a wrap into duplicates.
    pub fn select_next_occurrence(&mut self) {
        if self.selection.is_empty() {
            // First press: name the needle without adding a cursor.
            if let Some(span) = self.word_span_at(self.selection.head) {
                self.selection = Selection { anchor: span.start, head: span.end };
            }
            return;
        }

        let text = self.buffer.text();
        let needle = text[self.selection.range()].to_string();
        if needle.is_empty() {
            return;
        }

        // Search starts after the *last* selection in buffer order and wraps once; every
        // existing selection is skipped so saturation terminates.
        let taken: Vec<std::ops::Range<usize>> =
            self.all_selections().iter().map(|selection| selection.range()).collect();
        let last_end = taken.iter().map(|range| range.end).max().unwrap_or(0);

        let found = find_from(&text, &needle, last_end)
            .or_else(|| find_from(&text, &needle, 0))
            .filter(|start| !taken.iter().any(|range| range.start == *start));

        if let Some(start) = found {
            // The old primary joins the extras; the new match leads.
            self.extra_selections.push(self.selection);
            self.selection = Selection { anchor: start, head: start + needle.len() };
            self.extra_selections.sort_by_key(|selection| selection.range().start);
            self.buffer.break_undo_group();
        }
    }

    /// Replaces every selection with `text`, one undo step, all cursors kept.
    ///
    /// # Order of application
    ///
    /// Descending buffer order, so each replacement leaves every *earlier* selection's
    /// offsets untouched; the cursors are then rebuilt in one ascending pass, carrying
    /// the cumulative size delta. Applying ascending and patching as you go is the same
    /// arithmetic with more places to get it wrong.
    ///
    /// One undo group deliberately: ⌘Z after typing through five cursors must restore
    /// all five sites, not peel them one at a time — the user made one edit.
    pub fn insert_at_all_cursors(&mut self, text: &str) {
        if self.extra_selections.is_empty() {
            self.insert(text);
            return;
        }

        let ordered = self.all_selections();
        let ranges: Vec<std::ops::Range<usize>> =
            ordered.iter().map(|selection| selection.range()).collect();
        self.splice_at(&ranges, text);
    }

    /// Backspace across every cursor: deletes each selection, or one character back.
    ///
    /// The same order and rebuild as [`Self::insert_at_all_cursors`]. Indent-aware
    /// backspace (the tab-stop rule) applies per cursor, exactly as it would alone.
    pub fn backspace_at_all_cursors(&mut self) {
        if self.extra_selections.is_empty() {
            self.backspace();
            return;
        }

        let ordered = self.all_selections();
        // Resolve each cursor's deletion range *before* any edit, in the pre-edit
        // coordinate space the descending application preserves.
        let ranges: Vec<std::ops::Range<usize>> = ordered
            .iter()
            .map(|selection| {
                if selection.is_empty() {
                    let head = selection.head;
                    if head == 0 {
                        return 0..0;
                    }
                    match self.indent_backspace_target(head) {
                        Some(target) => target..head,
                        None => self.prev_char_offset(head)..head,
                    }
                } else {
                    selection.range()
                }
            })
            .collect();

        self.splice_at(&ranges, "");
    }

    /// Replaces every range with `replacement` as **one** buffer edit — one undo step.
    ///
    /// # Why one `replace` over the whole span, not one per range
    ///
    /// The documented trap, walked into anyway and caught by this file's own undo test:
    /// `Edit::extends` coalesces only contiguous typing, so a loop of per-range replaces
    /// inside a `break_undo_group` sandwich is N undo steps no matter where the breaks
    /// go — ⌘Z after typing through five cursors peeled one site at a time. The shape
    /// that works is the one `indent_lines` and replace-all already use: a single
    /// `replace` spanning first-to-last, with the untouched text between ranges spliced
    /// back in around each replacement.
    ///
    /// `ranges` must be sorted and non-overlapping, which `all_selections` guarantees
    /// and the debug assertion states.
    fn splice_at(&mut self, ranges: &[std::ops::Range<usize>], replacement: &str) {
        debug_assert!(
            ranges.windows(2).all(|pair| pair[0].end <= pair[1].start),
            "splice ranges must be sorted and disjoint"
        );
        let text = self.buffer.text();
        let span =
            ranges.first().map(|r| r.start).unwrap_or(0)..ranges.last().map(|r| r.end).unwrap_or(0);

        // The span's new content: replacement at each range, original text in the gaps.
        let mut combined = String::new();
        let mut heads: Vec<usize> = Vec::with_capacity(ranges.len());
        let mut cursor = span.start;
        for range in ranges {
            combined.push_str(&text[cursor..range.start]);
            combined.push_str(replacement);
            heads.push(span.start + combined.len());
            cursor = range.end;
        }

        self.buffer.break_undo_group();
        self.buffer.replace(span, &combined);
        self.buffer.break_undo_group();

        let mut rebuilt: Vec<Selection> = heads.into_iter().map(Selection::at).collect();
        let primary = rebuilt.pop().unwrap_or(Selection::at(0));
        self.extra_selections = rebuilt;
        self.selection = primary;
        self.goal_column = None;
        self.sync_syntax();
    }

    /// Applies a set of independent byte-range edits as **one** buffer edit — one undo
    /// step. The formatting shape (#19): `splice_at`'s one-spanning-replace form,
    /// generalised to a different new text per range.
    ///
    /// Order in the input is meaningless (LSP leaves it unspecified — only the ranges
    /// are the truth), so the edits are sorted here; overlapping edits are a protocol
    /// violation and the whole batch is dropped rather than half-applied.
    pub fn apply_edits(&mut self, edits: Vec<(std::ops::Range<usize>, String)>) {
        // `None` keeps the historic landing: the same offset clamped into the new text,
        // which is right for formatting because nothing was "just typed" to land after.
        self.apply_edits_landing_after(edits, None);
    }

    /// [`Self::apply_edits`], but the cursor lands at the **end of the edit that replaced
    /// `land_after`** rather than at its old offset.
    ///
    /// Auto-import (#61 follow-up) is why this exists. Accepting `User` from the popup is
    /// two edits at once — the identifier at the cursor *and* the `use App\Models\User;`
    /// the server sent in `additionalTextEdits` — and they have to be one undo step, so
    /// they go through one batch. But the import is inserted *above* the cursor, so it
    /// grows the text before it and the "same offset" rule of `apply_edits` would leave
    /// the caret that many bytes short of the identifier it just accepted.
    ///
    /// The caller names the range it cares about instead of trying to predict the shift,
    /// because the shift depends on every other edit in the batch and only this function
    /// has seen them all.
    pub fn apply_edits_landing_after(
        &mut self,
        mut edits: Vec<(std::ops::Range<usize>, String)>,
        land_after: Option<std::ops::Range<usize>>,
    ) {
        if edits.is_empty() {
            return;
        }
        edits.sort_by_key(|(range, _)| (range.start, range.end));
        if edits.windows(2).any(|pair| pair[0].0.end > pair[1].0.start) {
            return;
        }

        // Where the named range ends once every earlier edit has resized the text before
        // it. Computed against the *sorted* batch, so it is the real post-edit offset and
        // not an assumption about which edit came first off the wire.
        let landing = land_after.and_then(|target| {
            let mut drift: isize = 0;
            for (range, new_text) in &edits {
                if *range == target {
                    return Some((range.start as isize + drift) as usize + new_text.len());
                }
                // Only edits strictly before the target move it.
                if range.end <= target.start {
                    drift += new_text.len() as isize - (range.end - range.start) as isize;
                }
            }
            None
        });

        let text = self.buffer.text();
        let span = edits.first().map(|(r, _)| r.start).unwrap_or(0)
            ..edits.last().map(|(r, _)| r.end).unwrap_or(0);

        let mut combined = String::new();
        let mut cursor = span.start;
        for (range, new_text) in &edits {
            combined.push_str(&text[cursor..range.start]);
            combined.push_str(new_text);
            cursor = range.end;
        }

        self.buffer.break_undo_group();
        self.buffer.replace(span.clone(), &combined);
        self.buffer.break_undo_group();

        // Formatting moves text under the cursor; the least surprising landing is the
        // same offset clamped into the new document — snapped back to a char boundary,
        // because the old offset may now point into the middle of a codepoint. A caller
        // that named a range gets the end of that range's replacement instead.
        let after = self.buffer.text();
        let mut head = landing.unwrap_or(self.selection.head).min(after.len());
        while head > 0 && !after.is_char_boundary(head) {
            head -= 1;
        }
        self.extra_selections.clear();
        self.selection = Selection::at(head);
        self.goal_column = None;
        self.sync_syntax();
    }

    /// The byte span of the *word* under `offset`, or `None` when what is there is not one.
    ///
    /// The ⌘-hover link hint: only a word earns an underline and a pointing hand, because
    /// only a word is something go-to-definition can answer about. Whitespace and
    /// punctuation return `None` — ⌘ held over `->` must not promise a jump the server
    /// will refuse, which is the same honesty rule as everywhere else, applied to a hint.
    pub fn word_span_at(&self, offset: usize) -> Option<std::ops::Range<usize>> {
        let (kind, start, end) = self.class_run_at(offset)?;
        if kind != CharClass::Word || start == end {
            return None;
        }
        let rope = self.buffer.rope();
        Some(rope.char_to_byte(start)..rope.char_to_byte(end))
    }

    /// Triple-click: selects the whole line `row` sits on, including its line ending.
    ///
    /// Zed's third click (`crates/editor/src/selection.rs:1294-1305`) ends the selection at
    /// `next_line_boundary(position).0 + Point::new(1, 0)`, clipped — i.e. **column zero of
    /// the following row**, not the end of this row's content. That is why copying a
    /// triple-clicked line in Zed pastes as a whole line rather than as a fragment that
    /// needs a newline typed after it.
    ///
    /// `line_span` already computes exactly that range (line ending included, whatever the
    /// ending is, and stopping at content on the last row, which is Zed's `clip_point`).
    pub fn select_line_at(&mut self, row: usize) {
        let row = row.min(self.buffer.len_lines().saturating_sub(1));
        let span = self.line_span(row..=row);
        self.selection = Selection { anchor: span.start, head: span.end };
        self.goal_column = None;
        self.buffer.break_undo_group();
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

    // --- find and replace (#80) --------------------------------------------------------

    /// Sets the find query, rescanning if it or the buffer changed.
    ///
    /// Called on every keystroke in the find field. The rescan is the whole cost of
    /// search (see `editor/find.rs` for the measurement and why it is eager); the guard
    /// below means retyping the same query, or asking again for a buffer that has not
    /// moved, costs a comparison.
    ///
    /// Returns whether it actually rescanned, so a caller can repaint only then. That
    /// return value is what lets `WorkspaceView::render` call this every frame without
    /// notifying itself into an infinite repaint loop.
    pub fn set_search_query(&mut self, query: SearchQuery) -> bool {
        let unchanged = query == self.search.query && self.search.version == self.buffer.version();
        if unchanged {
            return false;
        }
        // The current match index is an index into a list that is about to be replaced.
        // Keeping it would make "3 of 17" point at a different hit than the highlight.
        self.search.current = None;
        self.search.query = query;
        self.rescan();
        true
    }

    /// Recomputes matches if the buffer has moved since the last scan.
    ///
    /// Separate from [`Document::set_search_query`] so the view can call it before reading
    /// matches for a frame: an edit invalidates the list, and a stale range painted over
    /// edited text is a highlight in the wrong place at best and an out-of-bounds slice at
    /// worst. Cheap when nothing changed, which is the common case.
    pub fn refresh_search(&mut self) {
        if self.search.version != self.buffer.version() {
            self.rescan();
        }
    }

    fn rescan(&mut self) {
        self.search.matches = Matches::new(&self.buffer.text(), &self.search.query);
        self.search.version = self.buffer.version();
        // An edit can delete the match the user was on. Clamp rather than clear, so a
        // replace-then-next sequence keeps its place in the list.
        if let Some(current) = self.search.current
            && current >= self.search.matches.len()
        {
            self.search.current = None;
        }
    }

    /// Clears the query, which turns highlighting off. Called when the find bar closes.
    pub fn clear_search(&mut self) {
        self.search = Search::default();
        // A fresh `Search` has version 0, which would look current for an untouched
        // buffer and skip the first rescan after reopening. Not a problem: the query is
        // empty, so `set_search_query` sees a *different* query and rescans anyway.
    }

    /// ⌘G / ⌘⇧G: moves the cursor to the next or previous match, wrapping.
    ///
    /// The match becomes the selection, which is what makes typing over it replace it and
    /// ⌘C copy it — the behaviour every editor has, and the answer to "how does a match
    /// interact with an existing selection": it *becomes* the selection.
    ///
    /// Returns whether a match was found, so the caller can say "no results" rather than
    /// silently doing nothing.
    pub fn select_match(&mut self, forward: bool) -> bool {
        self.refresh_search();

        // Search from the far edge of the current selection, so repeated ⌘G advances
        // instead of re-finding the match the cursor is already sitting inside. Backwards
        // uses the low edge for the mirror reason.
        let selection = self.selection.range();
        let index = if forward {
            self.search.matches.index_at_or_after(selection.end.max(selection.start))
        } else {
            self.search.matches.index_before(selection.start)
        };

        let Some(index) = index else { return false };
        let Some(range) = self.search.matches.get(index).cloned() else { return false };

        self.search.current = Some(index);
        self.select_range(range);
        true
    }

    /// Replaces the current match, then advances to the next one.
    ///
    /// Advancing is what makes ⌘⌥F, replace, replace, replace a usable loop rather than
    /// something that needs an alternating ⌘G. If nothing is current yet, the first press
    /// selects a match instead of editing — pressing "Replace" should never edit a hit the
    /// user has not been shown.
    pub fn replace_current(&mut self, replacement: &str) -> bool {
        self.refresh_search();
        if self.search.matches.is_invalid() {
            return false;
        }

        let Some(range) = self.search.current_range() else {
            return self.select_match(true);
        };

        // Regex mode expands `$1` against *this* match's captures, so it cannot reuse a
        // precomputed string. `replacements` re-runs the regex and finds the entry whose
        // range is the one being replaced.
        let text = self.buffer.text();
        let replacement = if self.search.query.regex {
            match self
                .search
                .matches
                .replacements(&text, &self.search.query, replacement)
                .into_iter()
                .find(|(candidate, _)| *candidate == range)
            {
                Some((_, expanded)) => expanded,
                None => return false,
            }
        } else {
            replacement.to_string()
        };

        self.replace_as_one_step(range.clone(), &replacement);
        self.rescan();

        // Land on the match after the text just inserted, not inside it: replacing `a`
        // with `aa` would otherwise find the replacement itself, forever.
        let after = range.start + replacement.len();
        self.search.current = self.search.matches.index_at_or_after(after);
        if let Some(range) = self.search.current_range() {
            self.select_range(range);
        }
        true
    }

    /// Replaces every match as **exactly one undo step**.
    ///
    /// One ⌘Z must undo the whole operation (#73). A loop over
    /// [`Document::replace_current`] cannot do that, and neither can a loop over
    /// `Buffer::replace` inside a `break_undo_group` sandwich — I wrote that version
    /// first and the test caught it. `Buffer::replace` only coalesces when `Edit::extends`
    /// holds, and that is deliberately true *only* for contiguous typing with nothing
    /// deleted (`crates/text/src/edit.rs`). Twenty replacements are twenty deletions, so
    /// they are twenty groups no matter where the breaks go.
    ///
    /// So this is **one** `replace` over the span from the first match to the last, with
    /// the replacements spliced into a copy of that span — the same shape
    /// [`Document::indent_lines`] already uses for the same reason, and it gives the
    /// syntax tree one edit instead of N. The rewritten span is bounded by the matches,
    /// not by the file: replacing in the last two lines of a 10 MB file rewrites two lines.
    ///
    /// Returns how many were replaced.
    pub fn replace_all(&mut self, replacement: &str) -> usize {
        self.refresh_search();
        if self.search.matches.is_invalid() || self.search.matches.is_empty() {
            return 0;
        }

        let text = self.buffer.text();
        // Reverse order from `replacements`; the splice below wants them front to back.
        let mut edits = self.search.matches.replacements(&text, &self.search.query, replacement);
        edits.reverse();
        let (Some(first), Some(last)) = (edits.first(), edits.last()) else { return 0 };

        let span = first.0.start..last.0.end;
        let mut spliced = String::with_capacity(span.len());
        // Where in `text` the untouched run before the next match begins.
        let mut cursor = span.start;
        for (range, text_for_match) in &edits {
            spliced.push_str(&text[cursor..range.start]);
            spliced.push_str(text_for_match);
            cursor = range.end;
        }
        // `cursor == span.end` after the loop, so there is no tail to append — the span
        // ends at the last match by construction.

        let count = edits.len();
        // The cursor lands at the end of the *first* replacement, at the top of the
        // affected region, rather than wherever the rewrite happened to end.
        let cursor_target = span.start + edits[0].1.len();
        self.replace_as_one_step(span, &spliced);
        self.selection = Selection::at(cursor_target);

        self.rescan();
        self.search.current = None;
        count
    }

    /// Selects `range`, for a render test seeding the find bar from a selection.
    #[cfg(test)]
    pub fn select_range_for_test(&mut self, range: Range<usize>) {
        self.select_range(range);
    }

    /// Selects `range`, with the cursor at its end.
    pub(crate) fn select_range(&mut self, range: Range<usize>) {
        // `break_undo_group` for the same reason `move_to` does it: a jump ends a typing
        // run so ⌘Z after ⌘G does not merge the two.
        self.buffer.break_undo_group();
        self.selection = Selection { anchor: range.start, head: range.end };
        self.goal_column = None;
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

    // --- brackets --------------------------------------------------------------------

    /// The pairs auto-close inserts, and that [`Document::matching_bracket`] pairs up.
    ///
    /// Quotes are here as *wrapping* pairs only — see [`Document::insert_with_pairs`] for
    /// why typing a bare `'` does not auto-close. The list is ASCII on purpose: typographic
    /// quotes and CJK brackets are a keyboard-layout question, not a syntax one.
    const PAIRS: [(char, char); 5] = [('(', ')'), ('[', ']'), ('{', '}'), ('\'', '\''), ('"', '"')];

    /// The two ends of the bracket pair the cursor is touching, if any.
    ///
    /// **This is a node lookup, not a scan.** The tree-sitter tree is already parsed and
    /// already in memory, so the bracket at the cursor is found with
    /// `descendant_for_byte_range` — one descent from the root, O(depth) — and its partner
    /// is a *sibling of the same parent node*, which the parse tree already knows because
    /// pairing brackets is what parsing them means. Scanning the buffer counting depth
    /// would be O(distance to the partner), which on the `{` of a 2000-line class is the
    /// whole class, every frame the cursor sits there.
    ///
    /// What that rules out: a file with no grammar (plain text) and a file whose parse is
    /// broken get no highlight at all, because there is no tree to ask. That is the right
    /// failure — a bracket "match" derived from a broken parse points at the wrong
    /// character, which is worse than pointing at nothing.
    ///
    /// Both offsets are returned so the renderer can highlight the pair; the cursor may be
    /// on either end, and on the character *before* the cursor as well as under it, which
    /// is what makes it feel right after typing a closer.
    pub fn matching_bracket(&self) -> Option<(usize, usize)> {
        let tree = self.syntax.tree()?;
        let head = self.selection.head;
        let root = tree.root_node();

        // Under the cursor first, then just before it. Typing `)` leaves the cursor after
        // it, and that is exactly when the user wants to see what it closed.
        let candidates = [head, self.prev_char_offset(head)];
        for at in candidates {
            let text = self.buffer.slice(at..self.next_char_offset(at));
            let Some(ch) = text.chars().next() else { continue };
            let Some((open, close)) =
                Self::PAIRS.iter().copied().find(|&(o, c)| (ch == o || ch == c) && o != c)
            else {
                continue;
            };

            // The smallest named node covering this byte is the bracket token itself. Its
            // parent is the construct the pair delimits, and the partner is that parent's
            // matching child — so the lookup never leaves the two nodes tree-sitter has
            // already associated.
            let node = root.descendant_for_byte_range(at, at + text.len())?;
            let parent = node.parent().unwrap_or(node);

            let wanted = if ch == open { close } else { open };
            let mut cursor = parent.walk();
            let partner = parent.children(&mut cursor).find(|child| {
                child.start_byte() != at
                    && child.end_byte() - child.start_byte() == wanted.len_utf8()
                    && self.buffer.slice(child.start_byte()..child.end_byte()) == wanted.to_string()
            });

            if let Some(partner) = partner {
                return Some((at, partner.start_byte()));
            }
        }
        None
    }

    /// Whether the byte offset sits inside a comment or string, per the parse tree.
    ///
    /// The gate that lets quotes auto-close in *code* while leaving prose alone: `don't`
    /// in a comment or a string must never become `don''t`, and the tree already knows
    /// which one the cursor is in. No tree (plain text, broken parse) answers `true` —
    /// prose is exactly where auto-closing quotes does damage, so the conservative
    /// reading is the safe one.
    fn in_comment_or_string(&self, at: usize) -> bool {
        let Some(tree) = self.syntax.tree() else { return true };
        let Some(node) = tree.root_node().descendant_for_byte_range(at, at) else {
            return true;
        };
        let mut current = Some(node);
        while let Some(n) = current {
            let kind = n.kind();
            if kind.contains("comment") || kind.contains("string") || kind.contains("heredoc") {
                return true;
            }
            current = n.parent();
        }
        false
    }

    /// Whether the offset sits inside a PHP array literal — and not inside a nested
    /// call's argument list, where `=` is an assignment again.
    ///
    /// Walking up from the cursor, the first structural ancestor decides: an
    /// `array_creation_expression` means `['name' |]`, where `=` can only sensibly mean
    /// `=>`; hitting an argument list or parentheses first means `['a' => foo($x |)]`,
    /// where it cannot.
    fn in_array_literal(&self, at: usize) -> bool {
        let Some(tree) = self.syntax.tree() else { return false };
        let Some(node) = tree.root_node().descendant_for_byte_range(at, at) else {
            return false;
        };
        let mut current = Some(node);
        while let Some(n) = current {
            match n.kind() {
                "array_creation_expression" => return true,
                "arguments" | "parenthesized_expression" | "formal_parameters" => return false,
                _ => {}
            }
            current = n.parent();
        }
        false
    }

    /// Inserts typed text, auto-closing brackets and typing over a closer.
    ///
    /// Three behaviours, all of which every editor has and none of which the user thinks
    /// about until they are missing:
    ///
    /// 1. **Wrap.** With a selection, typing an opener (or a quote) surrounds the selection
    ///    rather than replacing it. This is the one that is actively destructive to get
    ///    wrong — replacing a paragraph with `"` loses work.
    /// 2. **Type over.** Typing the closer when it is already the next character just steps
    ///    past it, so `foo()` typed in full does not become `foo())`.
    /// 3. **Auto-close.** Typing an opener inserts the pair and leaves the cursor between.
    ///
    /// Returns false when none applied, so the caller falls back to a plain insert.
    ///
    /// Quotes auto-close **in code only**: the parse tree vetoes comments, strings, and
    /// grammarless files, so `it's` and `don't` in prose stay untouched — the reason the
    /// old rule was "never". Two PHP-specific smart keys ride along (owner request):
    /// `=` inside an array literal completes to ` => `, and the `>` typed right after is
    /// swallowed.
    pub fn insert_with_pairs(&mut self, text: &str) -> bool {
        let mut chars = text.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            // Multi-character input is a paste or an IME commit, never a keystroke to pair.
            return false;
        };

        // 1. Wrap a selection.
        if !self.selection.is_empty()
            && let Some(&(open, close)) = Self::PAIRS.iter().find(|&&(o, _)| o == ch)
        {
            let range = self.selection.range();
            let selected = self.buffer.slice(range.clone());
            // One replace, so wrapping is one undo step and ⌘Z gives back the bare
            // selection rather than peeling off a quote at a time.
            self.replace_as_one_step(range.clone(), &format!("{open}{selected}{close}"));
            // Keep the text selected so wrapping twice nests, matching every editor.
            self.selection = Selection {
                anchor: range.start + open.len_utf8(),
                head: range.start + open.len_utf8() + selected.len(),
            };
            return true;
        }

        if !self.selection.is_empty() {
            return false;
        }

        let head = self.selection.head;
        let next = self.buffer.slice(head..self.next_char_offset(head));

        // 2. Type over a closer this editor just inserted. Closing brackets always;
        //    quotes only when the very next character is the same quote — stepping past
        //    is what makes `['name']` typed in full come out with two quotes, not four.
        if Self::PAIRS.iter().any(|&(o, c)| c == ch && o != c) && next == ch.to_string() {
            self.move_to(self.next_char_offset(head), false);
            return true;
        }
        if (ch == '\'' || ch == '"') && next == ch.to_string() {
            self.move_to(self.next_char_offset(head), false);
            return true;
        }

        // PHP array smart keys (owner request, the PhpStorm behaviours):
        // `=` inside an array literal completes to ` => ` — `['name' =` is only ever the
        // start of an arrow — and a `>` typed right after is swallowed so the habit of
        // typing `=>` in full does not produce `=>>`.
        if ch == '=' && self.in_array_literal(head) {
            let before = self.buffer.slice(head.saturating_sub(32)..head);
            let prev_non_space = before.chars().rev().find(|c| !c.is_whitespace());
            let after_key = matches!(prev_non_space, Some('\'' | '"' | ']' | ')'))
                || prev_non_space.is_some_and(|c| c.is_alphanumeric() || c == '_');
            if after_key {
                let lead = if before.chars().next_back().is_some_and(char::is_whitespace) {
                    ""
                } else {
                    " "
                };
                self.buffer.break_undo_group();
                let edit = self.buffer.replace(head..head, &format!("{lead}=> "));
                self.selection = Selection::at(edit.new_range().end);
                self.goal_column = None;
                self.buffer.break_undo_group();
                self.sync_syntax();
                return true;
            }
        }
        if ch == '>' && self.in_array_literal(head) {
            let before = self.buffer.slice(head.saturating_sub(3)..head);
            if before.ends_with("=> ") || before.ends_with("=>") {
                return true; // swallowed — the arrow is already there
            }
        }

        // 3. Auto-close quotes, in code only. The old rule was "never", because `don't`
        //    in prose must not become `don''t` — the parse tree now draws that line
        //    exactly: inside comments and strings (and in files with no grammar at all,
        //    which is what prose is) nothing changes; in code, `['` gets its partner.
        //    Word-adjacency still vetoes, same as brackets.
        if ch == '\'' || ch == '"' {
            let prev = self.buffer.slice(self.prev_char_offset(head)..head);
            let prev_ok = prev
                .chars()
                .next_back()
                .is_none_or(|p| !p.is_alphanumeric() && p != '_' && p != ch);
            let next_ok = next.chars().next().is_none_or(|n| !n.is_alphanumeric() && n != '_');
            if prev_ok && next_ok && !self.in_comment_or_string(head) {
                self.buffer.break_undo_group();
                let edit = self.buffer.replace(head..head, &format!("{ch}{ch}"));
                self.selection = Selection::at(edit.new_range().start + ch.len_utf8());
                self.goal_column = None;
                self.buffer.break_undo_group();
                self.sync_syntax();
                return true;
            }
            return false;
        }

        // 4. Auto-close brackets. Openers only.
        if let Some(&(_, close)) = Self::PAIRS.iter().find(|&&(o, c)| o == ch && o != c) {
            // Not in front of a word: `(` typed before `foo` means the user is wrapping by
            // hand, and a `)` landing between `(` and `foo` is in the way.
            let closes_here = next.chars().next().is_none_or(|n| !n.is_alphanumeric() && n != '_');
            if closes_here {
                // The pair is its own undo step, in both directions. The leading break
                // stops it joining a typing run already on the stack; the trailing one is
                // belt-and-braces, because `Edit::extends` would refuse to coalesce the
                // *next* keystroke anyway — it needs the insertion to begin where the group
                // ended, and this group ends after the `)` while the cursor sits before it.
                // Making that explicit beats relying on a coincidence of `extends`. See
                // `typing_inside_an_auto_closed_pair_undoes_before_the_pair_appeared`.
                self.buffer.break_undo_group();
                let edit = self.buffer.replace(head..head, &format!("{ch}{close}"));
                // Cursor between the two, which is the whole point.
                self.selection = Selection::at(edit.new_range().start + ch.len_utf8());
                self.goal_column = None;
                self.buffer.break_undo_group();
                self.sync_syntax();
                return true;
            }
        }

        false
    }

    /// Enter, keeping the new line's indentation.
    ///
    /// Matches the current line's indent, and adds one level when the cursor sits just
    /// after an opening brace. When the cursor is between a pair (`{|}`), the closer is
    /// pushed to a third line at the outer indent — the shape every editor produces and the
    /// reason auto-close and auto-indent have to be built together.
    ///
    /// One `replace` throughout, so a newline that produces three lines is still one undo
    /// step.
    pub fn newline_with_indent(&mut self) {
        let head = self.selection.head;
        let row = self.buffer.offset_to_point(head).row;
        let ending = self.line_ending(row);
        let indent = self.indent_of(row);

        // Only look at the characters either side of the cursor: the decision is local, so
        // this stays O(1) rather than reading the line.
        let before = self.buffer.slice(self.prev_char_offset(head)..head);
        let after = self.buffer.slice(head..self.next_char_offset(head));
        let opened = matches!(before.as_str(), "{" | "[" | "(");
        let closed =
            matches!((before.as_str(), after.as_str()), ("{", "}") | ("[", "]") | ("(", ")"));

        // ponytail: four spaces, the same hardcoded indent as `indent_lines` and
        // `EditorView::tab`. All three read the setting together once there is one (#60).
        const INDENT: &str = "    ";

        let text = if closed {
            format!("{ending}{indent}{INDENT}{ending}{indent}")
        } else if opened {
            format!("{ending}{indent}{INDENT}")
        } else {
            format!("{ending}{indent}")
        };

        let range = self.selection.range();
        self.replace_as_one_step(range.clone(), &text);

        // On the split-pair case the cursor belongs on the *middle* line, not after the
        // closer that `replace_as_one_step` left it on.
        if closed {
            self.move_to(range.start + ending.len() + indent.len() + INDENT.len(), false);
        }
    }

    // --- comments ----------------------------------------------------------------------

    /// ⌘/: comments the selected lines, or uncomments them if they are all commented.
    ///
    /// A toggle rather than two commands, and "all commented" rather than "any", because
    /// that is what makes it reversible: pressing it twice on a mixed block leaves the
    /// block commented, and pressing it again uncomments everything. Deciding per line
    /// instead would make the second press a no-op on half the lines.
    ///
    /// Returns false when the language has no comment syntax, which the caller reports —
    /// see [`Language::line_comment`] for why JSON is the language that means.
    ///
    /// The comment marker goes at the block's *shallowest* indent rather than at column
    /// zero, so commenting a method body keeps it visually inside the method. Blank lines
    /// are skipped, for the same reason `indent_lines` skips them: a marker on a blank line
    /// is trailing whitespace a linter then flags.
    pub fn toggle_comment(&mut self) -> bool {
        let language = self.language();

        if let Some(marker) = language.line_comment() {
            self.toggle_line_comment(marker);
            true
        } else if let Some((open, close)) = language.block_comment() {
            self.toggle_block_comment(open, close);
            true
        } else {
            false
        }
    }

    fn toggle_line_comment(&mut self, marker: &str) {
        let (first, last) = self.selected_rows();
        let span = self.line_span(first..=last);
        let text = self.buffer.slice(span.clone());

        // Content lines only: a block of blank lines has nothing to toggle, and letting
        // them vote would make "all commented" false forever.
        let content: Vec<&str> =
            text.split_inclusive('\n').filter(|l| !strip_newline(l).trim().is_empty()).collect();
        if content.is_empty() {
            return;
        }

        let all_commented = content.iter().all(|l| l.trim_start().starts_with(marker));
        // Column to insert at, as a *character* count of leading whitespace, so a line
        // indented with a tab and one with four spaces still line up with each other.
        let column = content.iter().map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);

        let mut out = String::with_capacity(text.len() + content.len() * (marker.len() + 1));
        for line in text.split_inclusive('\n') {
            if strip_newline(line).trim().is_empty() {
                out.push_str(line);
            } else if all_commented {
                // Remove the marker and the single space this function adds after it, but
                // only that one space — a `//    aligned` comment keeps its alignment.
                let at = line.len() - line.trim_start().len();
                let (indent, rest) = line.split_at(at);
                let rest = &rest[marker.len()..];
                out.push_str(indent);
                out.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            } else {
                let (indent, rest) = line.split_at(column);
                out.push_str(indent);
                out.push_str(marker);
                out.push(' ');
                out.push_str(rest);
            }
        }

        self.replace_as_one_step(span, &out);
        self.select_rows(first, last);
    }

    /// The block-comment form, for CSS, HTML and Blade — languages with no line comment.
    ///
    /// Wraps the whole selected block in one pair rather than one pair per line: `<!-- -->`
    /// per line on a ten-line template is noise, and HTML comments do not nest, so a
    /// per-line form would break the moment a line already had one.
    fn toggle_block_comment(&mut self, open: &str, close: &str) {
        let (first, last) = self.selected_rows();
        let span = self.line_span(first..=last);
        let text = self.buffer.slice(span.clone());
        let body = strip_newline(&text);
        let terminator = &text[body.len()..];
        let trimmed = body.trim();

        if trimmed.is_empty() {
            return;
        }

        let out = if trimmed.starts_with(open) && trimmed.ends_with(close) {
            // Uncomment: take the delimiters and the one space each side this adds.
            let inner = trimmed[open.len()..trimmed.len() - close.len()].trim();
            let indent = &body[..body.len() - body.trim_start().len()];
            format!("{indent}{inner}{terminator}")
        } else {
            let indent = &body[..body.len() - body.trim_start().len()];
            format!("{indent}{open} {trimmed} {close}{terminator}")
        };

        self.replace_as_one_step(span, &out);
        self.select_rows(first, last);
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

/// The next occurrence of `needle` at or after `from`, as a byte offset.
///
/// Guarded against slicing mid-character: `from` is clamped forward to a boundary,
/// because the caller derives it from selection ends that always sit on boundaries —
/// but a helper that would panic if that ever stopped being true is a trap.
fn find_from(text: &str, needle: &str, from: usize) -> Option<usize> {
    let mut from = from.min(text.len());
    while from < text.len() && !text.is_char_boundary(from) {
        from += 1;
    }
    text[from..].find(needle).map(|at| from + at)
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

    // --- click selection -------------------------------------------------------------

    /// The text a `select_*` call left selected, so a test reads as the user's outcome
    /// rather than as two offsets.
    fn selected(d: &Document) -> String {
        d.buffer.slice(d.selection.range())
    }

    #[test]
    fn a_double_click_inside_a_word_takes_the_whole_word() {
        let mut d = doc("let name = 1;");
        // Byte 5 is inside `name`.
        d.select_word_at(5);
        assert_eq!(selected(&d), "name");
    }

    #[test]
    fn a_double_click_between_a_word_and_a_space_prefers_the_word() {
        // This is the `cmp::max` in Zed's `surrounding_word` (`language/src/buffer.rs:4190`)
        // and the reason it is a max rather than "look right". A click lands between two
        // characters; with `let |name`, looking right gives `name` and looking left gives
        // the space. `CharKind`'s ordering makes Word win both times.
        //             0123456789
        let mut d = doc("let name = 1;");
        // Byte 4 is between the space at 3 and `n` at 4: the word is to the *right*.
        d.select_word_at(4);
        assert_eq!(selected(&d), "name", "the word is to the right of the click");
        // Byte 8 is between `e` at 7 and the space at 8: the word is to the *left*. This is
        // the direction that fails if the rule is "classify the character after the click" —
        // that reads the space and selects " " instead.
        d.select_word_at(8);
        assert_eq!(selected(&d), "name", "the word is to the left of the click");
    }

    #[test]
    fn a_double_click_on_punctuation_takes_the_punctuation_run() {
        // Three classes, so `->` is a run of its own — the same rule word *motion* already
        // follows. Double-clicking the arrow in `$user->name` must not select `$user->name`
        // (punctuation lumped with words) nor a single `-` (punctuation split per char).
        let mut d = doc("$user->name;");
        d.select_word_at(6); // between `-` and `>`
        assert_eq!(selected(&d), "->");
        d.select_word_at(2);
        assert_eq!(selected(&d), "$user", "`$` is a word character here");
    }

    #[test]
    fn a_double_click_never_crosses_a_line_break() {
        // A newline is whitespace, so without Zed's `&& ch != '\n'` guard
        // (`language/src/buffer.rs:4195`) a double-click in this indentation would run
        // through the blank line above and below and select all three line breaks.
        let mut d = doc("a\n\n    x\n\nb");
        let indent = d.buffer.point_to_offset(Point::new(2, 2)); // inside the four spaces
        d.select_word_at(indent);
        assert_eq!(selected(&d), "    ");
        assert!(!selected(&d).contains('\n'));
    }

    #[test]
    fn a_double_click_on_a_multibyte_word_keeps_whole_characters() {
        // `ação` is 6 bytes and 4 characters. Selecting by byte arithmetic would cut `ç`
        // in half, which panics on the next slice — the failure mode every other multibyte
        // test in this file exists to catch.
        let mut d = doc("uma ação boa");
        d.select_word_at(6); // inside `ação`, mid-word
        assert_eq!(selected(&d), "ação");
    }

    #[test]
    fn a_double_click_in_an_empty_buffer_selects_nothing() {
        let mut d = doc("");
        d.select_word_at(0);
        assert!(d.selection.is_empty());
    }

    #[test]
    fn a_triple_click_takes_the_line_including_its_ending() {
        // Zed ends the third-click selection at column zero of the *next* row
        // (`editor/src/selection.rs:1296`), not at the end of this row's content — which is
        // what makes a triple-clicked line paste back as a line.
        let mut d = doc("one\ntwo\nthree");
        d.select_line_at(1);
        assert_eq!(selected(&d), "two\n");
    }

    #[test]
    fn a_triple_click_on_the_last_line_stops_at_the_content() {
        // There is no following row to reach column zero of. Zed's `clip_point` does this;
        // here `line_span` already did.
        let mut d = doc("one\ntwo");
        d.select_line_at(1);
        assert_eq!(selected(&d), "two");
        // And a row past the end clamps rather than panicking, the way a stale coordinate
        // from an index built before the file shrank has to.
        d.select_line_at(99);
        assert_eq!(selected(&d), "two");
    }

    #[test]
    fn a_triple_click_keeps_a_crlf_ending_intact() {
        // Nothing in this pipeline normalises line endings, so the selected line ending has
        // to be whatever the file uses — otherwise cut-and-paste rewrites it.
        let mut d = doc("one\r\ntwo\r\nthree");
        d.select_line_at(1);
        assert_eq!(selected(&d), "two\r\n");
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
    fn backspace_in_indentation_goes_to_the_previous_tab_stop() {
        // The behaviour every editor has and this one did not: a blank indented line takes
        // one keystroke per level, not one per space.
        let mut d = doc("        \nx");
        d.move_to(8, false);
        d.backspace();
        assert_eq!(d.buffer.text(), "    \nx", "column 8 lands on 4, not 7");
        d.backspace();
        assert_eq!(d.buffer.text(), "\nx", "and 4 lands on 0");
    }

    #[test]
    fn backspace_off_the_tab_grid_pulls_back_into_alignment() {
        // Five spaces: someone typed one by hand. The first backspace aligns to 4 rather
        // than removing the whole level, which is `((5 - 1) / 4) * 4`.
        let mut d = doc("     x");
        d.move_to(5, false);
        d.backspace();
        assert_eq!(d.buffer.text(), "    x");
    }

    #[test]
    fn backspace_after_code_deletes_one_character() {
        // The rule is *leading* whitespace only. A space between words is one someone typed,
        // and eating four of them would be the editor guessing.
        // Column 7 is the space between `$a` and `=`. Backspace removes that one space —
        // if the leading-whitespace check were missing it would jump to column 4 and eat
        // `$a` with it.
        let mut d = doc("    $a = 1;");
        d.move_to(7, false);
        d.backspace();
        assert_eq!(d.buffer.text(), "    $a= 1;", "one space, not back to column 4");
    }

    #[test]
    fn backspace_still_steps_over_a_multibyte_character() {
        // The path this feature routes around: outside indentation nothing changed, and a
        // byte-wise step here would corrupt the codepoint.
        let mut d = doc("ação");
        d.move_to(d.buffer.len_bytes(), false);
        d.backspace();
        assert_eq!(d.buffer.text(), "açã");
    }

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
    fn applying_edits_rewrites_each_range_and_undoes_in_one_step() {
        // The formatting shape (#19): several ranges, each with its own new text —
        // splice_at's one-spanning-replace form, generalised past one replacement.
        let mut d = doc("<?php\nif($x){\nreturn;\n}\n");
        let text = d.buffer.text();
        let after_if = text.find("if(").unwrap() + 2;
        let before_return = text.find("return").unwrap();
        let edits = vec![
            (after_if..after_if, " ".to_string()),
            (before_return..before_return, "    ".to_string()),
        ];
        d.apply_edits(edits);
        assert_eq!(d.buffer.text(), "<?php\nif ($x){\n    return;\n}\n");
        d.undo();
        assert_eq!(d.buffer.text(), "<?php\nif($x){\nreturn;\n}\n", "one undo, not one per edit");
    }

    #[test]
    fn edits_arriving_out_of_order_are_applied_where_they_say() {
        // LSP leaves edit order unspecified; only the ranges are the truth.
        let mut d = doc("abcdef");
        d.apply_edits(vec![(4..5, "E".to_string()), (1..2, "B".to_string())]);
        assert_eq!(d.buffer.text(), "aBcdEf");
    }

    #[test]
    fn an_import_inserted_above_the_cursor_still_lands_the_caret_after_the_word() {
        // The auto-import shape, and the exact offset trap it carries: the `use` line goes
        // in *above* the identifier, so every byte below it moves down. Landing the caret
        // at its old offset would leave it short by the length of the import — inside the
        // word the user just accepted, which is where the next keystroke would go.
        let mut d = doc("<?php\nnamespace App;\n\n$u = new Us;\n");
        let text = d.buffer.text();
        let word = text.find("Us;").unwrap();
        let main = word..word + 2;
        let import_at = text.find("\n\n$u").unwrap() + 1;
        let import = "\nuse App\\Models\\User;\n";

        d.apply_edits_landing_after(
            vec![(main.clone(), "User".to_string()), (import_at..import_at, import.to_string())],
            Some(main),
        );

        assert_eq!(
            d.buffer.text(),
            "<?php\nnamespace App;\n\nuse App\\Models\\User;\n\n$u = new User;\n"
        );
        let after = d.buffer.text();
        assert_eq!(
            &after[..d.selection.head],
            "<?php\nnamespace App;\n\nuse App\\Models\\User;\n\n$u = new User",
            "the caret sits just past the accepted word, not short of it by the import"
        );

        // And the whole thing is one ⌘Z, not one per edit — an import the user has to undo
        // separately from the word that needed it is two edits pretending to be one.
        d.undo();
        assert_eq!(d.buffer.text(), "<?php\nnamespace App;\n\n$u = new Us;\n");
    }

    #[test]
    fn a_completion_with_no_import_lands_exactly_where_a_plain_insert_would() {
        // The common case. Auto-import must be invisible when there is nothing to import.
        let mut d = doc("$u = new Us;");
        d.apply_edits_landing_after(vec![(9..11, "User".to_string())], Some(9..11));
        assert_eq!(d.buffer.text(), "$u = new User;");
        assert_eq!(d.selection.head, 13, "just past the `r`");
    }

    #[test]
    fn formatting_keeps_the_old_landing_rule() {
        // The regression guard for the split: `apply_edits` names no range, so it must
        // still clamp the cursor to its old offset the way #19's formatting relies on.
        let mut d = doc("<?php\nif($x){\nreturn;\n}\n");
        d.move_to(3, false);
        d.apply_edits(vec![(8..8, " ".to_string())]);
        assert_eq!(d.selection.head, 3, "an unnamed batch does not move the caret");
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

    // --- multiple cursors (#82, stage 1) ------------------------------------------

    #[test]
    fn cmd_d_selects_the_word_then_adds_each_occurrence() {
        let mut d = doc("name = name + name;");

        d.move_to(1, false);
        d.select_next_occurrence();
        assert_eq!(d.selection.range(), 0..4, "first press names the needle");
        assert!(!d.has_multiple_cursors(), "and adds no cursor yet");

        d.select_next_occurrence();
        assert_eq!(d.selection.range(), 7..11, "second press takes the next occurrence");
        assert_eq!(d.all_selections().len(), 2);

        d.select_next_occurrence();
        assert_eq!(d.selection.range(), 14..18);
        assert_eq!(d.all_selections().len(), 3);

        // Saturation: every occurrence taken, a further press adds nothing — and must
        // not wrap into duplicating an existing selection.
        d.select_next_occurrence();
        assert_eq!(d.all_selections().len(), 3, "no duplicates at saturation");
    }

    #[test]
    fn typing_replaces_every_selection_and_undo_restores_all_in_one_step() {
        let mut d = doc("name = name + name;");
        d.move_to(1, false);
        d.select_next_occurrence();
        d.select_next_occurrence();
        d.select_next_occurrence();

        d.insert_at_all_cursors("id");
        assert_eq!(d.buffer.text(), "id = id + id;", "all three sites replaced");
        assert_eq!(d.all_selections().len(), 3, "the cursors survive the edit");
        // Each cursor sits after its own insertion.
        let heads: Vec<usize> = d.all_selections().iter().map(|selection| selection.head).collect();
        assert_eq!(heads, vec![2, 7, 12]);

        d.undo();
        assert_eq!(
            d.buffer.text(),
            "name = name + name;",
            "one ⌘Z restores all sites — the user made one edit"
        );
    }

    #[test]
    fn backspace_at_all_cursors_deletes_one_character_at_each() {
        let mut d = doc("name = name + name;");
        d.move_to(1, false);
        d.select_next_occurrence();
        d.select_next_occurrence();
        d.select_next_occurrence();
        // Type first so each cursor is a bare caret after its own text.
        d.insert_at_all_cursors("id");
        d.backspace_at_all_cursors();

        assert_eq!(d.buffer.text(), "i = i + i;", "one character gone at every cursor");
        let heads: Vec<usize> = d.all_selections().iter().map(|selection| selection.head).collect();
        assert_eq!(heads, vec![1, 5, 9]);
    }

    #[test]
    fn a_plain_motion_collapses_to_one_cursor() {
        // The stage-1 rule, and the funnel that enforces it: every arrow and click goes
        // through move_to.
        let mut d = doc("name = name;");
        d.move_to(1, false);
        d.select_next_occurrence();
        d.select_next_occurrence();
        assert!(d.has_multiple_cursors());

        d.move_to(0, false);
        assert!(!d.has_multiple_cursors(), "a plain motion returns to one cursor");
    }

    #[test]
    fn arrows_move_every_cursor_and_shift_extends_every_cursor() {
        // Stage 2: plain motions stopped collapsing. Two cursors, one keystroke, both move.
        let mut d = doc("aaa bbb\nccc ddd\n");
        d.set_selections(Selection::at(8), vec![Selection::at(0)]);

        d.move_horizontal(true, false);
        let heads: Vec<usize> = d.all_selections().iter().map(|s| s.head).collect();
        assert_eq!(heads, vec![1, 9], "both cursors advanced one character");

        d.move_word(true, true);
        let ranges: Vec<std::ops::Range<usize>> =
            d.all_selections().iter().map(|s| s.range()).collect();
        assert_eq!(ranges, vec![1..3, 9..11], "shift-word extended each from its own spot");
    }

    #[test]
    fn cursors_that_land_together_merge() {
        // Arrow-left from offsets 1 and 2 herds both toward 0; at the wall they collide,
        // and two carets in one place would type twice the text.
        let mut d = doc("abc");
        d.set_selections(Selection::at(2), vec![Selection::at(1)]);

        d.move_horizontal(false, false);
        assert_eq!(d.all_selections().len(), 2, "still apart after one step");
        d.move_horizontal(false, false);
        assert_eq!(d.all_selections().len(), 1, "merged at the left edge");

        // And the case the primary cannot cover for free: two *extras* colliding with
        // each other while the primary is elsewhere. A first draft of this test only
        // collided extras into the primary, and deleting the extra-vs-extra dedup line
        // passed it — the merge rule needs both halves, so both are pinned.
        let mut d = doc("abcdefgh");
        d.set_selections(Selection::at(7), vec![Selection::at(1), Selection::at(2)]);
        d.move_horizontal(false, false);
        d.move_horizontal(false, false);
        let heads: Vec<usize> = d.all_selections().iter().map(|s| s.head).collect();
        assert_eq!(heads, vec![0, 5], "the two extras merged at the wall; the primary is apart");
    }

    #[test]
    fn document_edges_collapse_the_pack_by_arithmetic() {
        // ⌘↑ sends every cursor to offset 0; they all land together and merge to one —
        // the same outcome VS Code produces, falling out of the merge rule rather than
        // being special-cased.
        let mut d = doc("aaa\nbbb\n");
        d.set_selections(Selection::at(5), vec![Selection::at(1)]);
        d.move_document_edge(false, false);
        assert_eq!(d.all_selections().len(), 1);
        assert_eq!(d.selection.head, 0);
    }

    #[test]
    fn set_selections_sorts_dedupes_and_keeps_the_primary() {
        // The column-drag door. The caller hands rows in drag order; editing assumes
        // sorted-and-disjoint, so the setter is where that becomes true.
        let mut d = doc("aa\nbb\ncc\n");
        let primary = Selection { anchor: 6, head: 7 };
        d.set_selections(
            primary,
            vec![
                Selection { anchor: 3, head: 4 },
                Selection { anchor: 0, head: 1 },
                Selection { anchor: 3, head: 4 }, // duplicate
                Selection { anchor: 6, head: 7 }, // the primary again
            ],
        );

        assert_eq!(d.selection.range(), 6..7, "the primary is the caller's choice");
        let starts: Vec<usize> =
            d.all_selections().iter().map(|selection| selection.range().start).collect();
        assert_eq!(starts, vec![0, 3, 6], "sorted, deduped, primary not doubled");

        // And typing through the column works — the whole point of setting them.
        d.insert_at_all_cursors("x");
        assert_eq!(d.buffer.text(), "xa\nxb\nxc\n");
    }

    #[test]
    fn alt_click_adds_toggles_and_never_reaches_zero_cursors() {
        let mut d = doc("um dois tres");

        d.add_cursor_at(3);
        assert_eq!(d.all_selections().len(), 2, "a new position adds a caret");
        assert_eq!(d.selection.head, 3, "the click leads");

        // The same gesture on an existing caret removes it.
        d.add_cursor_at(3);
        assert_eq!(d.all_selections().len(), 1, "clicking a caret away toggles it off");

        // And the only cursor cannot be clicked away — zero cursors is not a state.
        let head = d.selection.head;
        d.add_cursor_at(head);
        assert_eq!(d.all_selections().len(), 1);
    }

    #[test]
    fn multi_selection_copy_joins_in_buffer_order() {
        // ⌘C with three ⌘D selections: one clipboard string, newline-joined, in the order
        // the text reads — not the order the cursors were added.
        let mut d = doc("name = name + name;");
        d.move_to(1, false);
        d.select_next_occurrence();
        d.select_next_occurrence();
        d.select_next_occurrence();

        assert_eq!(d.selected_text().as_deref(), Some("name\nname\nname"));
    }

    #[test]
    fn multibyte_needles_survive_the_occurrence_walk() {
        // `ação` twice: the needle and the offsets both cross multibyte boundaries, and a
        // byte-sloppy find_from would slice mid-`ç` and panic.
        let mut d = doc("ação e ação;");
        d.move_to(1, false);
        d.select_next_occurrence();
        assert_eq!(&d.buffer.text()[d.selection.range()], "ação");

        d.select_next_occurrence();
        assert_eq!(d.all_selections().len(), 2);
        d.insert_at_all_cursors("x");
        assert_eq!(d.buffer.text(), "x e x;");
    }

    #[test]
    fn the_link_hint_answers_only_for_words() {
        // The ⌘-hover underline promises a jump; only a word can keep that promise.
        let d = doc("<?php \n$this->name;\n");

        // On `name` (bytes 14..18): the word, whole.
        assert_eq!(d.word_span_at(15), Some(14..18));
        // On `$this`: `$` is a word character here (see CharClass::of), so the span
        // includes it — the same answer double-click gives.
        assert_eq!(d.word_span_at(8), Some(7..12));
        // Inside the `->` (both neighbours punctuation): nothing to promise.
        assert_eq!(d.word_span_at(13), None);
        // At the boundary between `$this` and `->`, the word side wins — the same `max`
        // rule double-click uses, so hint and selection cannot disagree there.
        assert_eq!(d.word_span_at(12), Some(7..12));
        // On whitespace: nothing.
        assert_eq!(d.word_span_at(6), None);
    }

    #[test]
    fn lsp_positions_convert_through_the_lines_real_text() {
        // `ação` is the fixture for a reason: `ç` and `ã` are one UTF-16 unit but two
        // UTF-8 bytes each, so a position after them differs between the server's count
        // and ours — exactly the Portuguese source this editor exists for.
        let d = Document::new(
            None,
            "<?php
$ação = 1;
",
            false,
        )
        .unwrap();

        // Line 1, UTF-16 character 5 — just past `$ação`. Bytes: $ + a + ç(2) + ã(2) + o = 7.
        assert_eq!(d.point_from_lsp(1, 5), Point::new(1, 7));
        // ASCII agrees in both units.
        assert_eq!(d.point_from_lsp(0, 3), Point::new(0, 3));
        // Past the end of the line clamps to its end, not beyond.
        assert_eq!(d.point_from_lsp(0, 99), Point::new(0, 5));
        // Past EOF clamps to the last line.
        assert_eq!(d.point_from_lsp(99, 0).row, 2);
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
    fn an_untitled_buffer_can_be_given_a_language_without_being_saved() {
        // #127, exactly as reported: ⌘N produces a buffer with no path, so nothing detects
        // a language and there is no syntax colour. Before this the only way out was to
        // save the file, which is backwards — a scratch buffer exists to try something
        // *before* deciding where it lives.
        let mut d = Document::untitled().unwrap();
        assert_eq!(d.language(), Language::PlainText);
        assert!(d.syntax.tree().is_none(), "plain text has no parse tree");

        d.insert("<?php\nclass A {}\n");
        d.set_language(Language::Php).unwrap();

        assert_eq!(d.language(), Language::Php);
        assert!(d.syntax.tree().is_some(), "choosing PHP must produce a real parse tree");
        assert!(!d.syntax.has_error());
        // And it is still untitled, so ⌘S still routes to save-as. Choosing how to colour a
        // buffer must not invent a path for it.
        assert_eq!(d.path, None);
        assert_eq!(d.title(), "untitled");
    }

    #[test]
    fn setting_a_language_reparses_text_that_was_already_there() {
        // The tree has to be rebuilt against the *existing* buffer, not left empty until the
        // next keystroke — otherwise choosing a language appears to do nothing until you
        // type, which is how this would most plausibly be got wrong.
        let mut d = Document::new(None, "{\"a\": 1}\n", false).unwrap();
        d.set_language(Language::Json).unwrap();

        assert_eq!(d.language(), Language::Json);
        assert!(d.syntax.tree().is_some());
        assert!(!d.syntax.has_error(), "the existing text must parse, not just future edits");
    }

    #[test]
    fn a_detected_language_can_be_overridden() {
        // The other half of #127: detection runs off the extension and cannot be right in
        // principle for a file whose name lies about its contents.
        let mut d = doc("<?php\n");
        assert_eq!(d.language(), Language::Php);

        d.set_language(Language::PlainText).unwrap();
        assert_eq!(d.language(), Language::PlainText);
        assert!(d.syntax.tree().is_none());
    }

    #[test]
    fn choosing_the_language_a_buffer_already_has_changes_nothing() {
        let mut d = doc("<?php $x = 1;\n");
        let before = d.buffer.text();

        d.set_language(Language::Php).unwrap();

        assert_eq!(d.language(), Language::Php);
        assert_eq!(d.buffer.text(), before, "the text must not be touched");
    }

    #[test]
    fn saving_re_detects_and_overrides_the_chosen_language() {
        // Deliberate, and the reason the override is not persisted: once the buffer has a
        // path, the extension is the answer the user just gave. A language choice that
        // outlived the save would mean a file called `.php` refusing to highlight as PHP
        // because of a menu choice made ten minutes earlier.
        let mut d = Document::untitled().unwrap();
        d.set_language(Language::Json).unwrap();
        assert_eq!(d.language(), Language::Json);

        d.set_path(PathBuf::from("/tmp/User.php")).unwrap();
        assert_eq!(d.language(), Language::Php, "the saved name wins over the earlier choice");
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

    // --- bracket matching ------------------------------------------------------------
    //
    // The lookup goes through the parse tree, so these fixtures are valid PHP: an
    // unparseable file has no tree and correctly matches nothing, which is asserted below
    // rather than worked around.

    #[test]
    fn the_bracket_under_the_cursor_finds_its_partner() {
        let mut d = doc("<?php\nfunction f() { return 1; }\n");
        let open = d.buffer.text().find('{').unwrap();
        d.move_to(open, false);

        let (a, b) = d.matching_bracket().expect("a match on the opening brace");
        assert_eq!(a, open);
        assert_eq!(&d.buffer.text()[b..b + 1], "}");
    }

    #[test]
    fn matching_works_from_either_end_of_the_pair() {
        let mut d = doc("<?php\nfunction f() { return 1; }\n");
        let text = d.buffer.text();
        let open = text.find('{').unwrap();
        let close = text.find('}').unwrap();

        d.move_to(close, false);
        let (a, b) = d.matching_bracket().expect("a match on the closing brace");
        assert_eq!((a.min(b), a.max(b)), (open, close), "the closer must find its opener");
    }

    #[test]
    fn the_bracket_just_before_the_cursor_matches_too() {
        // Typing `)` leaves the cursor *after* it, and that is the moment the user most
        // wants to see what it closed. Without this the highlight never appears while typing.
        let mut d = doc("<?php\n$a = f(1);\n");
        let close = d.buffer.text().find(')').unwrap();
        d.move_to(close + 1, false);

        let (_, b) = d.matching_bracket().expect("a match on the bracket behind the cursor");
        assert_eq!(&d.buffer.text()[b..b + 1], "(");
    }

    #[test]
    fn nested_brackets_pair_with_the_right_partner() {
        // The case a naive "next closer" scan gets wrong.
        let mut d = doc("<?php\n$a = f(g(1), 2);\n");
        let text = d.buffer.text();
        let outer = text.find('(').unwrap();
        d.move_to(outer, false);

        let (_, partner) = d.matching_bracket().expect("a match");
        assert_eq!(partner, text.rfind(')').unwrap(), "the outer ( pairs with the *last* )");
    }

    #[test]
    fn a_cursor_on_no_bracket_matches_nothing() {
        let mut d = doc("<?php\n$name = 1;\n");
        d.move_to(d.buffer.text().find("name").unwrap() + 1, false);
        assert_eq!(d.matching_bracket(), None);
    }

    #[test]
    fn plain_text_has_no_tree_and_therefore_no_match() {
        // Deliberate: the match comes from the parse tree, so a file with no grammar gets
        // no highlight rather than a scan-based guess. Stated as a test because "brackets
        // do not highlight in a .md file" would otherwise read as a bug.
        let mut d = Document::new(Some(PathBuf::from("notes.txt")), "a (b) c", false).unwrap();
        d.move_to(2, false);
        assert!(d.syntax.tree().is_none());
        assert_eq!(d.matching_bracket(), None);
    }

    #[test]
    fn bracket_matching_is_multibyte_safe() {
        // Slicing one byte either side of a bracket next to a multi-byte character would
        // land mid-codepoint and panic in a debug build before it could return a wrong
        // answer. Accented text in a real Laravel string is where this happens.
        let mut d = doc("<?php\n$m = f('ação', 'çé');\n");
        let open = d.buffer.text().find('(').unwrap();
        d.move_to(open, false);
        let (a, b) = d.matching_bracket().expect("a match past the accented arguments");
        assert_eq!(&d.buffer.text()[a..a + 1], "(");
        assert_eq!(&d.buffer.text()[b..b + 1], ")");

        // And with the cursor sitting *on* a multi-byte character, where the "before the
        // cursor" probe reads backwards over two bytes.
        let at = d.buffer.text().find('ç').unwrap();
        d.move_to(at, false);
        assert_eq!(d.matching_bracket(), None, "no bracket here, and no panic getting there");
    }

    // --- auto-close ------------------------------------------------------------------

    #[test]
    fn typing_an_opener_inserts_the_pair_and_sits_between_them() {
        let mut d = doc("");
        assert!(d.insert_with_pairs("("));
        assert_eq!(d.buffer.text(), "()");
        assert_eq!(d.selection.head, 1, "the cursor belongs between the two");

        for (open, want) in [("[", "[]"), ("{", "{}")] {
            let mut d = doc("");
            assert!(d.insert_with_pairs(open));
            assert_eq!(d.buffer.text(), want);
        }
    }

    #[test]
    fn typing_the_closer_types_over_it_rather_than_doubling() {
        // `foo()` typed in full: the `)` must step over the one auto-close inserted.
        let mut d = doc("");
        d.insert("foo");
        d.insert_with_pairs("(");
        assert_eq!(d.buffer.text(), "foo()");
        assert!(d.insert_with_pairs(")"));
        assert_eq!(d.buffer.text(), "foo()", "no second )");
        assert_eq!(d.selection.head, 5, "and the cursor moved past it");
    }

    #[test]
    fn a_closer_with_no_pair_in_front_of_it_is_typed_normally() {
        // Type-over must not swallow a `)` the user genuinely means, or closing a paren
        // opened on an earlier line silently does nothing.
        let mut d = doc("");
        d.insert("f(1");
        assert!(!d.insert_with_pairs(")"), "nothing to type over, so the caller inserts");
    }

    #[test]
    fn an_opener_before_a_word_does_not_auto_close() {
        // Wrapping an existing call by hand: `(` typed before `foo` must not drop a `)`
        // between the two, which is exactly where it would be in the way.
        let mut d = doc("foo");
        d.move_to(0, false);
        assert!(!d.insert_with_pairs("("));
    }

    #[test]
    fn quotes_wrap_a_selection_and_auto_close_in_code() {
        let mut d = doc("hello");
        d.select_all();
        assert!(d.insert_with_pairs("'"), "quotes wrap a selection");
        assert_eq!(d.buffer.text(), "'hello'");

        // In code — here, an expression position in PHP — a quote closes itself.
        let mut d = doc("<?php\n$a = ;\n");
        d.move_to(d.buffer.text().find(';').unwrap(), false);
        assert!(d.insert_with_pairs("'"), "a quote in code auto-closes");
        assert_eq!(d.buffer.text(), "<?php\n$a = '';\n");
    }

    #[test]
    fn quotes_do_not_auto_close_in_prose() {
        // The apostrophe rule survives the new behaviour: `don't` in a comment or in a
        // grammarless file must stay `don't` — the parse tree is what draws the line.
        let mut d = doc("<?php\n// don\n");
        d.move_to(d.buffer.text().find("don").unwrap() + 3, false);
        assert!(!d.insert_with_pairs("'"), "a comment is prose");

        let mut d = Document::new(Some(PathBuf::from("notes.txt")), "don", false).unwrap();
        d.move_to(3, false);
        assert!(!d.insert_with_pairs("'"), "no grammar is prose too");

        // Inside a string: typing a *different* quote must not pair either.
        let mut d = doc("<?php\n$a = \"it\";\n");
        d.move_to(d.buffer.text().find("it").unwrap() + 2, false);
        assert!(!d.insert_with_pairs("'"), "inside a string is prose");
    }

    #[test]
    fn a_quote_next_to_a_word_stays_bare() {
        // `$a = it` + `'` — adjacency vetoes, same as brackets before a word.
        let mut d = doc("<?php\n$a = it;\n");
        d.move_to(d.buffer.text().find(';').unwrap(), false);
        assert!(!d.insert_with_pairs("'"));
    }

    #[test]
    fn typing_the_closing_quote_types_over_it() {
        let mut d = doc("<?php\n$a = ;\n");
        d.move_to(d.buffer.text().find(';').unwrap(), false);
        d.insert_with_pairs("'");
        assert!(d.insert_with_pairs("'"), "the second quote steps past");
        assert_eq!(d.buffer.text(), "<?php\n$a = '';\n", "still two quotes, not four");
    }

    // --- PHP array smart keys ----------------------------------------------------------

    #[test]
    fn equals_inside_an_array_becomes_an_arrow() {
        // `['name' =` is only ever the start of `=>` — the PhpStorm behaviour the owner
        // asked for, with the spacing of the example: `['name' => 'Ricardo']`.
        let mut d = doc("<?php\n$a = ['name'];\n");
        d.move_to(d.buffer.text().find("']").unwrap() + 1, false);
        assert!(d.insert_with_pairs("="));
        assert_eq!(d.buffer.text(), "<?php\n$a = ['name' => ];\n");

        // With the space already typed, no double space.
        let mut d = doc("<?php\n$a = ['name' ];\n");
        d.move_to(d.buffer.text().find(" ]").unwrap() + 1, false);
        assert!(d.insert_with_pairs("="));
        assert_eq!(d.buffer.text(), "<?php\n$a = ['name' => ];\n");
    }

    #[test]
    fn a_greater_than_right_after_the_arrow_is_swallowed() {
        // The habit of typing `=>` in full must not produce `=>>`.
        let mut d = doc("<?php\n$a = ['name'];\n");
        d.move_to(d.buffer.text().find("']").unwrap() + 1, false);
        d.insert_with_pairs("=");
        assert!(d.insert_with_pairs(">"), "swallowed");
        assert_eq!(d.buffer.text(), "<?php\n$a = ['name' => ];\n", "no second >");
    }

    #[test]
    fn equals_outside_an_array_is_just_equals() {
        let mut d = doc("<?php\n$a ;\n");
        d.move_to(d.buffer.text().find(';').unwrap(), false);
        assert!(!d.insert_with_pairs("="), "assignment is not an arrow");
    }

    #[test]
    fn equals_inside_a_nested_call_is_not_an_arrow() {
        // `['k' => f($x )]` — inside the call's arguments `=` is an assignment again.
        let mut d = doc("<?php\n$a = ['k' => f($x )];\n");
        d.move_to(d.buffer.text().find(" )").unwrap() + 1, false);
        assert!(!d.insert_with_pairs("="));
    }

    #[test]
    fn wrapping_a_selection_keeps_it_selected_so_wrapping_twice_nests() {
        let mut d = doc("x");
        d.select_all();
        d.insert_with_pairs("(");
        assert_eq!(d.buffer.text(), "(x)");
        assert_eq!(d.selected_text().unwrap(), "x", "the text stays selected, not the brackets");
        d.insert_with_pairs("\"");
        assert_eq!(d.buffer.text(), "(\"x\")");
    }

    #[test]
    fn wrapping_a_selection_is_multibyte_safe() {
        let mut d = doc("ação");
        d.select_all();
        d.insert_with_pairs("'");
        assert_eq!(d.buffer.text(), "'ação'");
        assert_eq!(d.selected_text().unwrap(), "ação", "byte arithmetic must not clip the ç");
    }

    #[test]
    fn a_paste_is_never_treated_as_a_pair() {
        // Multi-character input is a paste or an IME commit. Wrapping on it would corrupt
        // the paste, and auto-closing on it makes no sense.
        let mut d = doc("");
        assert!(!d.insert_with_pairs("(abc)"));
    }

    // --- auto-close undo granularity ---------------------------------------------------
    //
    // The trap the brief names: `()` appearing from one keystroke must not make ⌘Z behave
    // strangely. Both directions, the same shape #73 established for the line mutators.

    #[test]
    fn typing_inside_an_auto_closed_pair_undoes_before_the_pair_appeared() {
        // The granularity question the brief raises, answered explicitly because it is not
        // the obvious one.
        //
        // `Buffer::replace` coalesces only when `Edit::extends` holds, which needs the next
        // insertion to begin exactly where the previous group ended. Auto-close ends its
        // group *after* the `)` while leaving the cursor *before* it, so the following
        // keystroke can never extend it — the pair is structurally its own undo step, and no
        // amount of `break_undo_group` placement changes that.
        //
        // So ⌘Z peels: first the typing inside the pair, then the pair itself. That is two
        // presses to get back to `f`, and it is the *right* two: the pair is a thing the
        // editor did on its own, and an undo that silently removed both would give no way to
        // keep `()` while dropping what was typed in it. What must never happen is a ⌘Z that
        // leaves the buffer in a state the user never saw, and neither step does.
        let mut d = doc("");
        d.insert("f");
        d.insert_with_pairs("(");
        d.insert("name");
        assert_eq!(d.buffer.text(), "f(name)");

        d.undo();
        assert_eq!(d.buffer.text(), "f()", "the typing inside the pair undoes on its own");
        d.undo();
        assert_eq!(d.buffer.text(), "f", "and the pair itself is the step before it");
        d.undo();
        assert_eq!(d.buffer.text(), "", "nothing unexpected left on the stack");
    }

    #[test]
    fn typing_a_whole_call_undoes_in_steps_the_user_can_follow() {
        // The end-to-end shape: `foo()` typed in full, with the closer typed over. Every
        // intermediate state below is one the user actually saw on screen, which is the
        // property that matters more than the step count.
        let mut d = doc("");
        d.insert("foo");
        d.insert_with_pairs("(");
        d.insert("1");
        d.insert_with_pairs(")");
        assert_eq!(d.buffer.text(), "foo(1)");

        d.undo();
        assert_eq!(d.buffer.text(), "foo()");
        d.undo();
        assert_eq!(d.buffer.text(), "foo");
        d.undo();
        assert_eq!(d.buffer.text(), "");
    }

    #[test]
    fn wrapping_a_selection_undoes_in_one_step() {
        let mut d = doc("hello world");
        d.selection = Selection { anchor: 0, head: 5 };
        d.insert_with_pairs("(");
        assert_eq!(d.buffer.text(), "(hello) world");
        d.undo();
        assert_eq!(d.buffer.text(), "hello world", "one undo, not an insert plus an insert");
    }

    #[test]
    fn typing_before_an_auto_close_is_a_separate_undo_step() {
        // The other direction. A typing run already on the stack must not absorb the pair
        // and leave one ⌘Z removing both.
        let mut d = doc("");
        d.insert("alpha");
        d.buffer.break_undo_group();
        d.insert_with_pairs("(");
        assert_eq!(d.buffer.text(), "alpha()");
        d.undo();
        assert_eq!(d.buffer.text(), "alpha", "the pair undid on its own");
    }

    #[test]
    fn auto_close_keeps_the_syntax_tree_in_sync() {
        // The invariant `a_path_swap_never_happens_with_edits_outstanding` pins for every
        // other mutator: a new mutating path must drain the edit log.
        let mut d = doc("<?php\n$a = f");
        d.move_to(d.buffer.len_bytes(), false);
        d.insert_with_pairs("(");
        assert!(!d.buffer.has_pending(), "insert_with_pairs must drain");
        d.insert("1");
        d.move_to(d.buffer.len_bytes(), false);
        d.insert(";");
        assert!(!d.syntax.has_error(), "the tree must still agree with the text");
    }

    // --- auto-indent on newline --------------------------------------------------------

    #[test]
    fn newline_keeps_the_current_indentation() {
        let mut d = doc("    $a = 1;");
        d.move_to(d.buffer.len_bytes(), false);
        d.newline_with_indent();
        assert_eq!(d.buffer.text(), "    $a = 1;\n    ");
        assert_eq!(d.cursor_point(), Point::new(1, 4));
    }

    #[test]
    fn newline_after_an_opening_brace_indents_one_level_further() {
        let mut d = doc("    function f() {");
        d.move_to(d.buffer.len_bytes(), false);
        d.newline_with_indent();
        assert_eq!(d.buffer.text(), "    function f() {\n        ");
    }

    #[test]
    fn newline_between_a_pair_pushes_the_closer_to_its_own_line() {
        // The shape auto-close makes common: the cursor is `{|}` and Enter has to produce
        // three lines, with the closer back at the outer indent.
        let mut d = doc("    if (x) {}");
        d.move_to(d.buffer.len_bytes() - 1, false);
        d.newline_with_indent();
        assert_eq!(d.buffer.text(), "    if (x) {\n        \n    }");
        assert_eq!(d.cursor_point(), Point::new(1, 8), "the cursor sits on the middle line");
    }

    #[test]
    fn newline_undoes_in_one_step_even_when_it_makes_three_lines() {
        let mut d = doc("if (x) {}");
        d.move_to(d.buffer.len_bytes() - 1, false);
        d.newline_with_indent();
        assert_eq!(d.buffer.text(), "if (x) {\n    \n}");
        d.undo();
        assert_eq!(d.buffer.text(), "if (x) {}", "one undo, not three");
    }

    #[test]
    fn newline_keeps_crlf_endings_crlf() {
        let mut d = doc("    $a = 1;\r\n$b = 2;\r\n");
        d.move_to(d.buffer.point_to_offset(Point::new(0, 11)), false);
        d.newline_with_indent();
        assert_eq!(d.buffer.text(), "    $a = 1;\r\n    \r\n$b = 2;\r\n");
    }

    #[test]
    fn newline_is_multibyte_safe() {
        // The `before`/`after` probes read one character either side of the cursor; byte
        // arithmetic there would land mid-codepoint next to accented text.
        let mut d = doc("    $m = 'ação';");
        d.move_to(d.buffer.len_bytes(), false);
        d.newline_with_indent();
        assert_eq!(d.buffer.text(), "    $m = 'ação';\n    ");

        // And with the cursor immediately after a multi-byte character.
        let mut d = doc("çé");
        d.move_to(d.buffer.len_bytes(), false);
        d.newline_with_indent();
        assert_eq!(d.buffer.text(), "çé\n");
    }

    #[test]
    fn newline_replaces_a_selection() {
        let mut d = doc("    keep DROP");
        d.selection = Selection { anchor: 9, head: 13 };
        d.newline_with_indent();
        assert_eq!(d.buffer.text(), "    keep \n    ");
    }

    // --- comment toggle ----------------------------------------------------------------

    #[test]
    fn comment_toggle_comments_and_uncomments_a_php_line() {
        let mut d = doc("$a = 1;\n");
        d.move_to(0, false);
        assert!(d.toggle_comment());
        assert_eq!(d.buffer.text(), "// $a = 1;\n");
        assert!(d.toggle_comment());
        assert_eq!(d.buffer.text(), "$a = 1;\n", "and back, so it is a toggle");
    }

    #[test]
    fn comment_toggle_puts_the_marker_at_the_blocks_own_indent() {
        // At column zero the comment would visually leave the method it is inside.
        let mut d = doc("    $a = 1;\n    $b = 2;\n");
        d.select_all();
        d.toggle_comment();
        assert_eq!(d.buffer.text(), "    // $a = 1;\n    // $b = 2;\n");
        d.toggle_comment();
        assert_eq!(d.buffer.text(), "    $a = 1;\n    $b = 2;\n");
    }

    #[test]
    fn a_mixed_block_comments_rather_than_toggling_line_by_line() {
        // "All commented" and not "any", which is what makes the second press reversible.
        let mut d = doc("// $a = 1;\n$b = 2;\n");
        d.select_all();
        d.toggle_comment();
        assert_eq!(d.buffer.text(), "// // $a = 1;\n// $b = 2;\n", "a mixed block commutes up");
        d.toggle_comment();
        assert_eq!(
            d.buffer.text(),
            "// $a = 1;\n$b = 2;\n",
            "and the next press undoes exactly that"
        );
    }

    #[test]
    fn comment_toggle_leaves_blank_lines_alone() {
        // A marker on a blank line is trailing whitespace a linter then flags — the same
        // rule `indent_lines` follows.
        let mut d = doc("$a = 1;\n\n$b = 2;\n");
        d.select_all();
        d.toggle_comment();
        assert_eq!(d.buffer.text(), "// $a = 1;\n\n// $b = 2;\n");
        d.toggle_comment();
        assert_eq!(d.buffer.text(), "$a = 1;\n\n$b = 2;\n");
    }

    #[test]
    fn uncommenting_keeps_alignment_past_the_first_space() {
        // `//    aligned` is a deliberate layout; only the single space this function adds
        // comes back off.
        let mut d = doc("//    aligned\n");
        d.move_to(0, false);
        d.toggle_comment();
        assert_eq!(d.buffer.text(), "   aligned\n");
    }

    #[test]
    fn comment_toggle_uses_each_languages_own_marker() {
        for (name, source, want) in [
            ("t.yml", "key: 1\n", "# key: 1\n"),
            ("t.toml", "key = 1\n", "# key = 1\n"),
            ("t.sh", "echo x\n", "# echo x\n"),
            ("t.js", "let a = 1;\n", "// let a = 1;\n"),
            ("t.ts", "let a: number = 1;\n", "// let a: number = 1;\n"),
        ] {
            let mut d = Document::new(Some(PathBuf::from(name)), source, true).unwrap();
            d.move_to(0, false);
            assert!(d.toggle_comment(), "{name}");
            assert_eq!(d.buffer.text(), want, "{name}");
            d.toggle_comment();
            assert_eq!(d.buffer.text(), source, "{name} must round trip");
        }
    }

    #[test]
    fn comment_toggle_does_nothing_at_all_in_json() {
        // The deliberate decision. JSON has no comment syntax: `//` in composer.json is a
        // parse error, and the user finds out at `composer install`, not here. So ⌘/ makes
        // no edit and pushes nothing onto the undo stack.
        let mut d = Document::new(
            Some(PathBuf::from("composer.json")),
            "{\n  \"name\": \"a/b\"\n}\n",
            true,
        )
        .unwrap();
        d.select_all();

        assert!(!d.toggle_comment(), "JSON must report that it has no comment syntax");
        assert_eq!(d.buffer.text(), "{\n  \"name\": \"a/b\"\n}\n", "and the file is untouched");
        assert!(!d.buffer.is_dirty(), "an untouched file must not be marked dirty");
    }

    #[test]
    fn block_comment_languages_wrap_the_whole_block_once() {
        // CSS, HTML and Blade have no line comment. One pair around the block, not one per
        // line: HTML comments do not nest, so a per-line form breaks on the second press.
        for (name, source, want) in [
            ("t.css", ".a { color: red; }\n", "/* .a { color: red; } */\n"),
            ("t.html", "<p>x</p>\n", "<!-- <p>x</p> -->\n"),
            ("v.blade.php", "@if($x)\n", "{{-- @if($x) --}}\n"),
        ] {
            let mut d = Document::new(Some(PathBuf::from(name)), source, true).unwrap();
            d.move_to(0, false);
            assert!(d.toggle_comment(), "{name}");
            assert_eq!(d.buffer.text(), want, "{name}");
            d.toggle_comment();
            assert_eq!(d.buffer.text(), source, "{name} must round trip");
        }
    }

    #[test]
    fn comment_toggle_undoes_in_one_step() {
        let mut d = doc("$a = 1;\n$b = 2;\n$c = 3;\n");
        d.select_all();
        d.toggle_comment();
        assert_eq!(d.buffer.text(), "// $a = 1;\n// $b = 2;\n// $c = 3;\n");
        d.undo();
        assert_eq!(d.buffer.text(), "$a = 1;\n$b = 2;\n$c = 3;\n", "three lines, one undo");
    }

    #[test]
    fn comment_toggle_is_multibyte_safe() {
        // The marker is inserted at a byte offset derived from the indent; accented text on
        // the line would make a wrong offset land mid-codepoint and panic.
        let mut d = doc("    $m = 'ação';\n    $n = 'çé';\n");
        d.select_all();
        d.toggle_comment();
        assert_eq!(d.buffer.text(), "    // $m = 'ação';\n    // $n = 'çé';\n");
        d.toggle_comment();
        assert_eq!(d.buffer.text(), "    $m = 'ação';\n    $n = 'çé';\n");
    }

    #[test]
    fn comment_toggle_keeps_crlf_endings_crlf() {
        let mut d = doc("$a = 1;\r\n$b = 2;\r\n");
        d.select_all();
        d.toggle_comment();
        assert_eq!(d.buffer.text(), "// $a = 1;\r\n// $b = 2;\r\n");
        d.toggle_comment();
        assert_eq!(d.buffer.text(), "$a = 1;\r\n$b = 2;\r\n");
    }

    #[test]
    fn comment_toggle_keeps_the_syntax_tree_in_sync() {
        let mut d = doc("<?php\n$a = 1;\n");
        d.move_to(d.buffer.point_to_offset(Point::new(1, 0)), false);
        d.toggle_comment();
        assert!(!d.buffer.has_pending(), "toggle_comment must drain");
        d.toggle_comment();
        assert!(!d.buffer.has_pending());
        assert!(!d.syntax.has_error());
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

    // --- find and replace (#80) --------------------------------------------------------

    fn search(d: &mut Document, pattern: &str) {
        d.set_search_query(SearchQuery::literal(pattern));
    }

    #[test]
    fn setting_a_query_counts_matches_without_moving_the_cursor() {
        let mut d = doc("$user = 1;\n$user = 2;\n$other = 3;");
        search(&mut d, "$user");

        assert_eq!(d.search.position(), Some((None, 2)), "no match is current until ⌘G");
        assert_eq!(d.selection.head, 0, "opening find must not move the cursor");
        assert!(d.search.current_range().is_none());
    }

    #[test]
    fn setting_the_same_query_twice_does_not_rescan() {
        // The return value is load-bearing: `WorkspaceView::render` calls
        // `set_search_query` every frame and notifies only when this says `true`. A
        // version that always returned `true` would be an infinite repaint loop, which is
        // a hang rather than a wrong pixel.
        let mut d = doc("$user = 1;");
        assert!(d.set_search_query(SearchQuery::literal("$user")), "the first call scans");
        assert!(
            !d.set_search_query(SearchQuery::literal("$user")),
            "an identical query on an unchanged buffer must not rescan"
        );
        assert!(d.set_search_query(SearchQuery::literal("$other")), "a new query scans");

        // An edit invalidates it even when the query is byte-identical.
        d.insert("x");
        assert!(
            d.set_search_query(SearchQuery::literal("$other")),
            "the buffer moved, so the cached matches are stale"
        );
    }

    #[test]
    fn an_empty_query_reports_no_position_at_all() {
        // "0 of 0" in the find bar the instant it opens would be noise; there is nothing
        // to count yet.
        let mut d = doc("anything");
        search(&mut d, "");
        assert_eq!(d.search.position(), None);
        assert!(d.search.matches().is_empty());
    }

    #[test]
    fn a_query_with_no_matches_reports_zero_rather_than_nothing() {
        // The no-match state the brief asks for: distinct from an empty query, because the
        // bar has to say "No results" rather than stay blank.
        let mut d = doc("alpha beta");
        search(&mut d, "gamma");
        assert_eq!(d.search.position(), Some((None, 0)));
        assert!(!d.select_match(true), "next must report failure, not move the cursor");
        assert_eq!(d.selection.head, 0);
    }

    #[test]
    fn next_selects_the_match_and_wraps_at_the_end() {
        let mut d = doc("a__a__a");
        search(&mut d, "a");
        assert_eq!(d.search.matches().len(), 3);

        assert!(d.select_match(true));
        assert_eq!(d.selection.range(), 0..1);
        assert_eq!(d.search.position(), Some((Some(1), 3)));

        assert!(d.select_match(true));
        assert_eq!(d.selection.range(), 3..4);
        assert!(d.select_match(true));
        assert_eq!(d.selection.range(), 6..7);
        assert_eq!(d.search.position(), Some((Some(3), 3)));

        assert!(d.select_match(true), "the fourth press wraps");
        assert_eq!(d.selection.range(), 0..1);
        assert_eq!(d.search.position(), Some((Some(1), 3)));
    }

    #[test]
    fn prev_walks_backwards_and_wraps_at_the_start() {
        let mut d = doc("a__a__a");
        search(&mut d, "a");

        assert!(d.select_match(false), "the first ⌘⇧G from the top wraps to the last");
        assert_eq!(d.selection.range(), 6..7);
        assert!(d.select_match(false));
        assert_eq!(d.selection.range(), 3..4);
        assert!(d.select_match(false));
        assert_eq!(d.selection.range(), 0..1);
        assert!(d.select_match(false));
        assert_eq!(d.selection.range(), 6..7, "and wraps again");
    }

    #[test]
    fn next_starts_from_the_cursor_rather_than_the_top() {
        // ⌘F with the cursor halfway down a file must find the next hit below it, not
        // scroll back to the first one — the single most noticeable way this goes wrong.
        let mut d = doc("needle\n\n\n\nneedle\n\n\n\nneedle");
        d.move_to(10, false);
        search(&mut d, "needle");

        assert!(d.select_match(true));
        assert_eq!(d.selection.range().start, 10);
    }

    #[test]
    fn a_match_becomes_the_selection_and_replaces_an_existing_one() {
        // How a match interacts with a selection: it *is* the new selection, discarding
        // whatever was selected before, so ⌘C after ⌘G copies the hit.
        let mut d = doc("alpha needle omega");
        d.selection = Selection { anchor: 0, head: 5 };
        search(&mut d, "needle");
        d.select_match(true);

        assert_eq!(d.selection.range(), 6..12);
        assert_eq!(d.selected_text().as_deref(), Some("needle"));
    }

    #[test]
    fn matches_survive_an_edit_by_rescanning() {
        let mut d = doc("one two one");
        search(&mut d, "one");
        assert_eq!(d.search.matches().len(), 2);

        // Typing in the *document* while the find bar is open. A stale list here would
        // paint a highlight over bytes that have moved.
        d.move_to(0, false);
        d.insert("one ");
        d.refresh_search();
        assert_eq!(d.search.matches().len(), 3);
        assert_eq!(d.search.matches().all()[0], 0..3);
    }

    #[test]
    fn a_multibyte_match_selects_whole_characters() {
        // The panic this rules out: a selection boundary mid-codepoint. `Buffer::slice`
        // rounds, so a wrong offset shows up as the wrong *text* rather than a crash —
        // which is worse, because it is silent.
        let mut d = doc("função ação função");
        search(&mut d, "ção");
        assert_eq!(d.search.matches().len(), 3);

        d.select_match(true);
        assert_eq!(d.selected_text().as_deref(), Some("ção"));
        d.select_match(true);
        assert_eq!(d.selected_text().as_deref(), Some("ção"));
        // `função ` is 8 bytes (ç and ã are two each), so the second `ção` starts at 10 —
        // a char-offset implementation would say 8.
        assert_eq!(d.selection.range(), 10..15, "byte offsets, not char offsets");
    }

    #[test]
    fn replacing_a_multibyte_match_keeps_the_rest_of_the_text_intact() {
        let mut d = doc("função ação");
        search(&mut d, "ção");
        assert_eq!(d.replace_all("cao"), 2);
        assert_eq!(d.buffer.text(), "funcao acao");
    }

    #[test]
    fn replace_current_edits_only_the_current_match_and_advances() {
        let mut d = doc("cat cat cat");
        search(&mut d, "cat");
        d.select_match(true);

        assert!(d.replace_current("dog"));
        assert_eq!(d.buffer.text(), "dog cat cat");
        assert_eq!(d.selection.range(), 4..7, "and the next match is now selected");

        assert!(d.replace_current("dog"));
        assert_eq!(d.buffer.text(), "dog dog cat");
    }

    #[test]
    fn replace_with_nothing_current_selects_instead_of_editing() {
        // Pressing Replace before ever pressing Next must not silently edit a hit the
        // user has not been shown.
        let mut d = doc("cat cat");
        search(&mut d, "cat");
        assert!(d.replace_current("dog"));
        assert_eq!(d.buffer.text(), "cat cat", "nothing was replaced");
        assert_eq!(d.selection.range(), 0..3, "the first match is now current");
    }

    #[test]
    fn replacing_a_match_with_text_containing_it_terminates() {
        // The infinite-loop shape: replacing `a` with `aa` and then searching forward
        // from the *start* of the edit would find the replacement itself, forever.
        let mut d = doc("a a");
        search(&mut d, "a");
        d.select_match(true);
        d.replace_current("aa");
        assert_eq!(d.buffer.text(), "aa a");
        assert_eq!(d.selection.range(), 3..4, "the cursor moved past what it just wrote");
    }

    #[test]
    fn replace_all_replaces_every_match() {
        let mut d = doc("$user = $user + $user;");
        search(&mut d, "$user");
        assert_eq!(d.replace_all("$account"), 3);
        assert_eq!(d.buffer.text(), "$account = $account + $account;");
        assert_eq!(d.search.position(), Some((None, 0)), "the old matches are gone");
    }

    #[test]
    fn replace_all_is_one_undo_step() {
        // #73's rule, and the one the brief singles out: twenty replacements must not take
        // twenty ⌘Z. Twenty, not three, because a coalescing bug that merges pairs would
        // pass at three and fail in real use.
        let mut d = doc(&"cat\n".repeat(20));
        search(&mut d, "cat");
        assert_eq!(d.replace_all("dog"), 20);
        assert_eq!(d.buffer.text(), "dog\n".repeat(20));

        d.undo();
        assert_eq!(d.buffer.text(), "cat\n".repeat(20), "one ⌘Z restored all twenty");

        d.undo();
        assert_eq!(d.buffer.text(), "cat\n".repeat(20), "and nothing else was on the stack");
    }

    #[test]
    fn replace_all_does_not_swallow_the_edit_before_it() {
        // The other direction of the same guard: an edit made just before replace-all is
        // its own step and must survive the undo that reverses the replacement.
        let mut d = doc("cat cat");
        d.move_to(d.buffer.len_bytes(), false);
        d.insert("!");
        search(&mut d, "cat");
        assert_eq!(d.replace_all("dog"), 2);
        assert_eq!(d.buffer.text(), "dog dog!");

        d.undo();
        assert_eq!(d.buffer.text(), "cat cat!", "the replacement undid on its own");
        d.undo();
        assert_eq!(d.buffer.text(), "cat cat", "and the typing is a separate step");
    }

    #[test]
    fn replace_all_with_text_of_a_different_length_lands_on_the_right_bytes() {
        // The bug reverse ordering exists to prevent: applying front to back without
        // rebasing shifts every later edit by the accumulated length delta.
        let mut d = doc("a b a b a");
        search(&mut d, "a");
        assert_eq!(d.replace_all("LONGER"), 3);
        assert_eq!(d.buffer.text(), "LONGER b LONGER b LONGER");
    }

    #[test]
    fn replace_all_on_no_matches_changes_nothing() {
        let mut d = doc("alpha");
        search(&mut d, "zzz");
        assert_eq!(d.replace_all("beta"), 0);
        assert_eq!(d.buffer.text(), "alpha");
        assert!(!d.buffer.is_dirty(), "a no-op replace must not dirty the buffer");
    }

    #[test]
    fn a_whole_word_query_skips_substrings() {
        let mut d = doc("user username $user");
        d.set_search_query(SearchQuery { whole_word: true, ..SearchQuery::literal("user") });
        assert_eq!(d.search.matches().len(), 2);
        assert_eq!(d.replace_all("member"), 2);
        assert_eq!(d.buffer.text(), "member username $member");
    }

    #[test]
    fn a_case_sensitive_query_replaces_only_the_exact_case() {
        let mut d = doc("User user USER");
        d.set_search_query(SearchQuery { case_sensitive: true, ..SearchQuery::literal("user") });
        assert_eq!(d.replace_all("member"), 1);
        assert_eq!(d.buffer.text(), "User member USER");
    }

    #[test]
    fn a_regex_replacement_can_reorder_capture_groups() {
        let mut d = doc("$a = 1;\n$b = 2;");
        d.set_search_query(SearchQuery {
            regex: true,
            ..SearchQuery::literal(r"\$(\w+) = (\d+);")
        });
        assert_eq!(d.replace_all("$$$1 === $2;"), 2);
        assert_eq!(d.buffer.text(), "$a === 1;\n$b === 2;");
    }

    #[test]
    fn an_invalid_regex_refuses_to_replace() {
        // Mid-typing, `[a-` is what the pattern looks like. Replace-all must be a no-op
        // rather than matching nothing and silently reporting success.
        let mut d = doc("abc");
        d.set_search_query(SearchQuery { regex: true, ..SearchQuery::literal("[a-") });
        assert!(d.search.matches().is_invalid());
        assert_eq!(d.replace_all("x"), 0);
        assert!(!d.replace_current("x"));
        assert_eq!(d.buffer.text(), "abc");
    }

    #[test]
    fn clearing_the_search_turns_highlighting_off() {
        let mut d = doc("cat cat");
        search(&mut d, "cat");
        assert_eq!(d.search.matches().len(), 2);

        d.clear_search();
        assert!(d.search.matches().is_empty());
        assert_eq!(d.search.position(), None);

        // And reopening with the same query works: the reset version must not make the
        // rescan look unnecessary.
        search(&mut d, "cat");
        assert_eq!(d.search.matches().len(), 2);
    }
}

#[cfg(test)]
mod utf8_robustness_tests {
    use super::*;

    /// Text that has broken editors before: accents, CJK, emoji with a ZWJ sequence, a
    /// combining mark, and an RTL run. Every one is multi-byte, so any code that treats a
    /// byte offset as a character offset panics somewhere in here.
    const HOSTILE: &str =
        "<?php\n$café = 'ação';\n// 日本語のコメント\n$x = '👨‍👩‍👧‍👦';\n$e\u{0301} = 1;\n// مرحبا\n";

    /// Every byte offset, including the ones inside a character, on every API that takes
    /// one. A user reaches these by clicking, and a click lands wherever it lands.
    #[test]
    fn byte_offsets_inside_characters_never_panic() {
        let doc = Document::new(None, HOSTILE, false).expect("plain text parses");

        for offset in 0..=HOSTILE.len() + 4 {
            // Read-only queries first: these run on hover and on render.
            let _ = doc.word_span_at(offset);

            // Then the mutating ones, on a fresh document so each is independent.
            let mut probe = Document::new(None, HOSTILE, false).expect("plain text parses");
            probe.move_to(offset, false);
            probe.select_word_at(offset);
            probe.add_cursor_at(offset);
        }

        // The document must still be intact after all that.
        assert_eq!(doc.text_for_save(), HOSTILE, "probing must not have mutated the document");
    }

    /// Inserting at a boundary that is *inside* a multi-byte character is the same class of
    /// bug as reading at one, and it is reachable by paste with a stale cursor.
    #[test]
    fn inserting_at_every_offset_never_panics() {
        for offset in 0..=HOSTILE.len() + 4 {
            let mut doc = Document::new(None, HOSTILE, false).expect("plain text parses");
            doc.move_to(offset, false);
            doc.insert("ç");
            assert!(
                doc.text_for_save().contains('ç'),
                "the insert must have landed at offset {offset}"
            );
        }
    }
}
