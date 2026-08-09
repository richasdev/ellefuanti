//! Incremental tree-sitter parse state for one buffer.

use anyhow::{Context as _, Result};
use elle_text::{Buffer, Edit};
use tree_sitter::{InputEdit, Parser, Point as TsPoint, Tree};

use crate::language::Language;

/// A parse tree kept in sync with a [`Buffer`] by replaying edits.
///
/// The point of this type is `edit()` + reparse-with-old-tree: tree-sitter reuses the
/// unchanged subtrees, so cost scales with the size of the edit rather than the file
/// (§2, §27 "não reindexe projeto inteiro por tecla").
pub struct SyntaxTree {
    language: Language,
    parser: Parser,
    tree: Option<Tree>,
}

impl SyntaxTree {
    /// Creates parse state and performs the initial full parse.
    pub fn new(language: Language, buffer: &Buffer) -> Result<Self> {
        let mut parser = Parser::new();
        let mut tree = None;

        if let Some(grammar) = language.grammar() {
            parser
                .set_language(&grammar)
                .with_context(|| format!("loading {} grammar", language.name()))?;
            tree = parser.parse(buffer.text(), None);
        }

        Ok(Self { language, parser, tree })
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// Applies buffer edits and reparses incrementally.
    ///
    /// `buffer` must already reflect every edit in `edits`, in order — this replays
    /// them onto the *tree* so tree-sitter knows which byte ranges shifted.
    pub fn apply_edits(&mut self, buffer: &Buffer, edits: &[Edit]) {
        if self.language.grammar().is_none() || edits.is_empty() {
            return;
        }

        // Walk edits oldest-first, translating each into an InputEdit. Positions are
        // resolved against the *current* buffer, which is why old_end_position is
        // reconstructed from the pre-edit text rather than read back from the rope.
        if let Some(tree) = self.tree.as_mut() {
            for edit in edits {
                tree.edit(&input_edit(buffer, edit));
            }
        }

        // ponytail: reparse from the full text string. tree-sitter can read a rope
        // through parse_with_options to avoid this allocation; measure first — for the
        // file sizes M1 targets the copy is far cheaper than the parse it feeds, and
        // the callback version is easy to swap in behind this same method.
        self.tree = self.parser.parse(buffer.text(), self.tree.as_ref());
    }

    /// Full reparse, discarding incremental state. For when a buffer is replaced
    /// wholesale (file reloaded from disk, git checkout).
    pub fn reparse(&mut self, buffer: &Buffer) {
        if self.language.grammar().is_none() {
            return;
        }
        self.tree = self.parser.parse(buffer.text(), None);
    }

    /// Whether the tree contains a syntax error. Cheap: a flag on the root node.
    pub fn has_error(&self) -> bool {
        self.tree.as_ref().is_some_and(|t| t.root_node().has_error())
    }
}

/// Translates one of our edits into tree-sitter's coordinate change.
fn input_edit(buffer: &Buffer, edit: &Edit) -> InputEdit {
    let start_byte = edit.range.start;
    let old_end_byte = edit.range.end;
    let new_end_byte = edit.new_range().end;

    // start_position is unaffected by this edit, so the current buffer gives it directly.
    let start_position = to_ts_point(buffer, start_byte);
    // new_end is also live in the current buffer.
    let new_end_position = to_ts_point(buffer, new_end_byte);
    // old_end no longer exists in the buffer: derive it by advancing start_position
    // over the text that was deleted.
    let old_end_position = advance(start_position, &edit.old_text);

    InputEdit { start_byte, old_end_byte, new_end_byte, start_position, old_end_position, new_end_position }
}

fn to_ts_point(buffer: &Buffer, offset: usize) -> TsPoint {
    let p = buffer.offset_to_point(offset);
    TsPoint::new(p.row, p.column)
}

/// Where `text` ends if it starts at `start`.
fn advance(start: TsPoint, text: &str) -> TsPoint {
    let newlines = text.matches('\n').count();
    match text.rsplit_once('\n') {
        Some((_, tail)) => TsPoint::new(start.row + newlines, tail.len()),
        None => TsPoint::new(start.row, start.column + text.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn php(text: &str) -> (Buffer, SyntaxTree) {
        let buffer = Buffer::new(text);
        let tree = SyntaxTree::new(Language::Php, &buffer).unwrap();
        (buffer, tree)
    }

    #[test]
    fn parses_valid_php() {
        let (_b, t) = php("<?php\nclass User extends Model {}\n");
        assert!(!t.has_error());
        assert!(t.tree().unwrap().root_node().to_sexp().contains("class_declaration"));
    }

    #[test]
    fn flags_broken_php() {
        let (_b, t) = php("<?php class {{{ ");
        assert!(t.has_error());
    }

    #[test]
    fn incremental_edit_matches_full_reparse() {
        // The real invariant: replaying edits must land on the same tree a cold parse
        // would produce. If InputEdit coordinates are wrong, these sexps diverge.
        let (mut buffer, mut tree) = php("<?php\nclass User {\n    public $a;\n}\n");

        let edit = buffer.insert(buffer.point_to_offset(elle_text::Point::new(2, 14)), "\n    public $b;");
        tree.apply_edits(&buffer, &[edit]);
        let incremental = tree.tree().unwrap().root_node().to_sexp();

        let mut cold = SyntaxTree::new(Language::Php, &buffer).unwrap();
        cold.reparse(&buffer);
        assert_eq!(incremental, cold.tree().unwrap().root_node().to_sexp());
        assert!(!tree.has_error());
    }

    #[test]
    fn multibyte_and_multiline_edits_keep_coordinates_sane() {
        let (mut buffer, mut tree) = php("<?php\n$name = 'José';\n");
        let offset = buffer.point_to_offset(elle_text::Point::new(1, 0));
        let edit = buffer.insert(offset, "// ação\n");
        tree.apply_edits(&buffer, &[edit]);

        let mut cold = SyntaxTree::new(Language::Php, &buffer).unwrap();
        cold.reparse(&buffer);
        assert_eq!(
            tree.tree().unwrap().root_node().to_sexp(),
            cold.tree().unwrap().root_node().to_sexp()
        );
    }

    #[test]
    fn deletion_across_lines_stays_in_sync() {
        let (mut buffer, mut tree) = php("<?php\nfunction a() {}\nfunction b() {}\n");
        let start = buffer.point_to_offset(elle_text::Point::new(1, 0));
        let end = buffer.point_to_offset(elle_text::Point::new(2, 0));
        let edit = buffer.delete(start..end);
        tree.apply_edits(&buffer, &[edit]);

        let mut cold = SyntaxTree::new(Language::Php, &buffer).unwrap();
        cold.reparse(&buffer);
        assert_eq!(
            tree.tree().unwrap().root_node().to_sexp(),
            cold.tree().unwrap().root_node().to_sexp()
        );
    }

    #[test]
    fn plain_text_has_no_tree_and_no_panic() {
        let buffer = Buffer::new("just words");
        let mut t = SyntaxTree::new(Language::PlainText, &buffer).unwrap();
        assert!(t.tree().is_none());
        t.apply_edits(&buffer, &[Edit::new(0..0, "x", "")]);
        assert!(!t.has_error());
    }

    #[test]
    fn blade_parses_markup_and_php() {
        let buffer = Buffer::new("<div>@if($a) {{ $b }} @endif</div>\n<?php $c = 1; ?>");
        let t = SyntaxTree::new(Language::Blade, &buffer).unwrap();
        assert!(t.tree().is_some());
    }
}
