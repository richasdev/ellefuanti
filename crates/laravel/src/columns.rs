//! Where a column name is being typed: the first argument of a query-builder call (#22).
//!
//! `User::where('…')` names a column of `users`, and the cursor inside that literal is
//! the one place a column completion is an answer to the question being asked. Same
//! honesty contract as `references.rs`: the tree, not a regex, so a `where(` inside a
//! comment or heredoc is not a context; and a chain whose root class cannot be read off
//! the source (`$user->where(`) reports nothing rather than guessing a type.

use elle_syntax::{Language, SyntaxTree};
use elle_text::Buffer;
use tree_sitter::Node;

use crate::references::literal;

/// Whose columns the literal names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnTarget {
    /// The chain starts at a class written in the source: `User::where(` — the short
    /// name, qualifier stripped, because that is what the index stores models under.
    Class(String),
    /// The chain starts at `$this->` — the class is whatever model the enclosing file
    /// declares, which the *caller* knows and this module does not.
    This,
}

/// What the literal is expected to name — decided by which method is being called.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Argument {
    /// `where('…')` and friends: a column of the target's table.
    Column,
    /// `with('…')` and friends: a relationship method the target declares.
    Relation,
}

/// A cursor inside the first-argument literal of a query-builder call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnContext {
    pub target: ColumnTarget,
    /// Column or relation — the caller offers a different list for each.
    pub expects: Argument,
    /// The literal's content range — what an accepted completion replaces.
    pub range: std::ops::Range<usize>,
}

/// The builder methods whose *first* argument is a column name — the set Eloquent
/// documents and `artisan` tab-completion users actually type. Methods whose first
/// argument is something else (`find` takes a key value, `select` a list) are absent
/// rather than half-right.
const COLUMN_METHODS: [&str; 11] = [
    "where",
    "orWhere",
    "firstWhere",
    "whereIn",
    "whereNotIn",
    "whereNull",
    "whereNotNull",
    "orderBy",
    "orderByDesc",
    "pluck",
    "value",
];

/// The builder methods whose first argument names a *relationship* instead. `with` takes
/// dotted nested paths (`posts.comments`); only the model's own first segment is knowable
/// from one class's index rows, which is the piece the completion offers.
const RELATION_METHODS: [&str; 8] = [
    "with",
    "load",
    "has",
    "whereHas",
    "orWhereHas",
    "doesntHave",
    "whereDoesntHave",
    "withCount",
];

/// Finds the column context containing `offset`, if the cursor sits inside the first
/// argument of a column-taking builder call whose chain root is readable.
pub fn column_context_at(source: &str, offset: usize) -> Option<ColumnContext> {
    let buffer = Buffer::new(source);
    let tree = SyntaxTree::new(Language::Php, &buffer).ok()?;
    let tree = tree.tree()?;
    let src = source.as_bytes();

    let mut node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    loop {
        if matches!(node.kind(), "member_call_expression" | "scoped_call_expression")
            && let Some(context) = call_context(node, src)
            // Inclusive of the end, like `php_reference_at`: an empty `where('')` is a
            // zero-width range and exactly the moment completion is wanted.
            && (context.range.start..=context.range.end).contains(&offset)
        {
            return Some(context);
        }
        node = node.parent()?;
    }
}

/// Reads one call node into a context, or `None` when it is not a column-taking call
/// with a plain-literal first argument and a readable chain root.
fn call_context(call: Node, src: &[u8]) -> Option<ColumnContext> {
    let name = call.child_by_field_name("name")?;
    let method = name.utf8_text(src).ok()?;
    let expects = if COLUMN_METHODS.contains(&method) {
        Argument::Column
    } else if RELATION_METHODS.contains(&method) {
        Argument::Relation
    } else {
        return None;
    };

    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let first =
        args.named_children(&mut cursor).find(|c| c.kind() == "argument")?.named_child(0)?;
    let (_, range) = literal(first, src)?;

    let target = chain_root(call, src)?;
    Some(ColumnContext { target, expects, range })
}

/// Finds a `Class::partial` being typed at `offset`: the class's short name and the
/// byte range of the partial member name — what an accepted scope completion replaces.
///
/// Mid-typing, `User::ac` parses as a `class_constant_access_expression` (often inside
/// an ERROR node at statement level), and `User::ac()` as a `scoped_call_expression`;
/// both shapes are the same user intention, so both are contexts. The tree is still the
/// judge — the same text inside a comment or a string parses as neither.
pub fn scope_context_at(source: &str, offset: usize) -> Option<(String, std::ops::Range<usize>)> {
    let buffer = Buffer::new(source);
    let tree = SyntaxTree::new(Language::Php, &buffer).ok()?;
    let tree = tree.tree()?;
    let src = source.as_bytes();

    // The cursor sits at the *end* of the partial word, so look at the byte before it —
    // `descendant_for_byte_range(offset, offset)` at a word's end lands on the next token.
    let probe = offset.checked_sub(1)?;
    let mut node = tree.root_node().descendant_for_byte_range(probe, probe)?;
    loop {
        let (scope, member) = match node.kind() {
            "class_constant_access_expression" => {
                let mut cursor = node.walk();
                let names: Vec<Node> = node.named_children(&mut cursor).collect();
                let [scope, member] = names[..] else { return None };
                (scope, member)
            }
            "scoped_call_expression" => {
                (node.child_by_field_name("scope")?, node.child_by_field_name("name")?)
            }
            _ => {
                node = node.parent()?;
                continue;
            }
        };
        if !matches!(scope.kind(), "name" | "qualified_name") {
            return None;
        }
        let range = member.start_byte()..member.end_byte();
        if !(range.start..=range.end).contains(&offset) {
            return None;
        }
        let written = scope.utf8_text(src).ok()?;
        let short = written.rsplit('\\').next().unwrap_or(written);
        return Some((short.to_string(), range));
    }
}

/// Walks a call chain to its root and reads the class there, if there is one to read.
///
/// `User::query()->latest()->where(` roots at `User`; `$this->where(` roots at `$this`.
/// A chain rooted anywhere else — another variable, a function call's return — has a
/// type this module cannot know from one file, and `None` is the honest answer.
fn chain_root(call: Node, src: &[u8]) -> Option<ColumnTarget> {
    let mut node = call;
    loop {
        match node.kind() {
            "scoped_call_expression" => {
                let scope = node.child_by_field_name("scope")?;
                if !matches!(scope.kind(), "name" | "qualified_name") {
                    return None;
                }
                let written = scope.utf8_text(src).ok()?;
                let short = written.rsplit('\\').next().unwrap_or(written);
                return Some(ColumnTarget::Class(short.to_string()));
            }
            "member_call_expression" => {
                let object = node.child_by_field_name("object")?;
                if object.kind() == "variable_name" {
                    return (object.utf8_text(src).ok()? == "$this").then_some(ColumnTarget::This);
                }
                node = object;
            }
            // A parenthesized chain, `(User::query())->where(`, unwraps.
            "parenthesized_expression" => node = node.named_child(0)?,
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte offset of `|` in a fixture, with the marker removed.
    fn at(fixture: &str) -> (String, usize) {
        let offset = fixture.find('|').expect("fixture has a cursor");
        (fixture.replace('|', ""), offset)
    }

    fn context(fixture: &str) -> Option<ColumnContext> {
        let (source, offset) = at(fixture);
        column_context_at(&source, offset)
    }

    #[test]
    fn a_static_where_names_the_class() {
        let ctx = context("<?php User::where('is_ad|min');").expect("a context");
        assert_eq!(ctx.target, ColumnTarget::Class("User".into()));
    }

    #[test]
    fn the_chain_roots_at_the_class_not_the_last_call() {
        let ctx = context("<?php User::query()->latest()->orderBy('nam|e');").expect("a context");
        assert_eq!(ctx.target, ColumnTarget::Class("User".into()));
    }

    #[test]
    fn a_qualified_class_reports_its_short_name() {
        // The index stores models by short class; the context speaks the same language.
        let ctx = context("<?php \\App\\Models\\User::where('|');").expect("a context");
        assert_eq!(ctx.target, ColumnTarget::Class("User".into()));
    }

    #[test]
    fn this_is_reported_as_this_because_the_caller_knows_the_class() {
        let ctx = context("<?php class User extends Model { function f() { $this->where('|'); } }")
            .expect("a context");
        assert_eq!(ctx.target, ColumnTarget::This);
    }

    #[test]
    fn an_untyped_variable_is_not_a_context() {
        // `$user`'s type needs inference this module does not do — None, not a guess.
        assert_eq!(context("<?php $user->where('|');"), None);
    }

    #[test]
    fn a_method_outside_the_set_is_not_a_context() {
        // `find` takes a key *value*; offering columns there would be wrong, not helpful.
        assert_eq!(context("<?php User::find('|');"), None);
    }

    #[test]
    fn only_the_first_argument_is_a_column() {
        assert_eq!(context("<?php User::where('name', 'ali|ce');"), None);
    }

    #[test]
    fn the_empty_literal_is_the_moment_completion_is_wanted() {
        let (source, _) = at("<?php User::where('|');");
        let offset = source.find("''").unwrap() + 1;
        let ctx = column_context_at(&source, offset).expect("a context");
        assert!(ctx.range.is_empty());
    }

    #[test]
    fn the_cursor_on_the_method_name_is_not_in_the_argument() {
        assert_eq!(context("<?php User::wh|ere('name');"), None);
    }

    #[test]
    fn an_interpolated_string_is_not_a_plain_literal() {
        assert_eq!(context("<?php User::where(\"{$col|umn}\");"), None);
    }

    #[test]
    fn a_with_call_expects_a_relation_not_a_column() {
        let ctx = context("<?php User::with('po|sts')->get();").expect("a context");
        assert_eq!(ctx.target, ColumnTarget::Class("User".into()));
        assert_eq!(ctx.expects, Argument::Relation);
    }

    #[test]
    fn where_has_and_with_count_expect_relations_too() {
        let has = context("<?php User::whereHas('|');").expect("a context");
        assert_eq!(has.expects, Argument::Relation);
        let count = context("<?php User::query()->withCount('|');").expect("a context");
        assert_eq!(count.expects, Argument::Relation);
        assert_eq!(count.target, ColumnTarget::Class("User".into()));
    }

    #[test]
    fn a_where_still_expects_a_column() {
        let ctx = context("<?php User::where('|');").expect("a context");
        assert_eq!(ctx.expects, Argument::Column);
    }

    fn scope(fixture: &str) -> Option<(String, std::ops::Range<usize>)> {
        let (source, offset) = at(fixture);
        scope_context_at(&source, offset)
    }

    #[test]
    fn typing_after_a_static_class_prefix_is_a_scope_context() {
        let (class, range) = scope("<?php $u = User::ac|;").expect("a context");
        assert_eq!(class, "User");
        let (source, _) = at("<?php $u = User::ac|;");
        assert_eq!(&source[range], "ac");
    }

    #[test]
    fn the_bare_statement_form_parses_through_the_error_node() {
        // `User::ac` alone is an ERROR-wrapped class_constant_access_expression while
        // being typed; the context must still be found there, because "while being
        // typed" is the only moment completion exists.
        let (class, _) = scope("<?php User::ac|").expect("a context");
        assert_eq!(class, "User");
    }

    #[test]
    fn a_qualified_prefix_reports_the_short_name() {
        let (class, _) = scope("<?php \\App\\Models\\User::po|;").expect("a context");
        assert_eq!(class, "User");
    }

    #[test]
    fn a_commented_prefix_is_not_a_context() {
        assert_eq!(scope("<?php // User::ac|"), None);
    }

    #[test]
    fn a_variable_scope_is_not_a_class() {
        assert_eq!(scope("<?php $user::ac|;"), None);
    }

    #[test]
    fn the_closure_inside_where_has_is_not_the_relation_argument() {
        // `whereHas('posts', fn ($q) => $q->where('|'))` — the inner where roots at an
        // untyped `$q`, which is None, not the outer call's relation context.
        assert_eq!(
            context("<?php User::whereHas('posts', function ($q) { $q->where('|'); });"),
            None
        );
    }
}
