//! What Laravel symbol is under the cursor: `route('x')`, `config('a.b')`, `view('v')`,
//! `@include('v')`, `<x-forms.input>`.
//!
//! Two halves, kept apart on purpose. [`reference_at`] reads the *source* and says what the
//! cursor is on; [`resolve`] turns that into a place on disk. Only the second touches the
//! filesystem, so the interesting half — which spellings count, and which must not — is
//! testable without a fixture project, and the caller can decide whether resolving is worth
//! a stat call at all.
//!
//! **This is not Blade parsing.** ADR-0006 says Blade waits for a real grammar, and a
//! `.blade.php` file run through the PHP grammar collapses to a single `text` node — there
//! is no tree to walk. So Blade constructs are found with the same literal scanner the
//! highlighter already uses, which is enough because `@include('partials.header')` and
//! `<x-forms.input>` are *path expressions*, not structure. Nothing here needs to know what
//! a slot is, which is exactly what #24 is blocked on and this is not.
//!
//! Honesty (RISKS.md #4) is structural rather than a `Resolved<T>` here: a reference is
//! only reported when the argument is a plain single- or double-quoted literal with no
//! interpolation. `route($name)` and `view("posts.{$slug}")` produce `None` — not a guess —
//! because the point of a ⌘click is to land somewhere true.

use elle_syntax::{Language, SyntaxTree};
use elle_text::Buffer;
use tree_sitter::Node;

/// A Laravel symbol referenced from source, with the literal the user wrote.
///
/// The byte range is the *literal's content*, not the whole call, so a caller can underline
/// exactly what is clickable rather than the surrounding syntax.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub kind: ReferenceKind,
    /// The string as written: `users.show`, `app.name`, `forms.input`.
    pub name: String,
    /// Byte range of `name` within the source.
    pub range: std::ops::Range<usize>,
}

/// Which Laravel lookup a reference is.
///
/// `View` covers `view('x')`, `@include('x')`, `@extends('x')` and friends together: they
/// all name a template by dotted path and resolve identically, and splitting them would
/// produce variants no consumer branches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceKind {
    /// `route('users.show')` — a named route.
    Route,
    /// `config('app.name')` — a dotted key into `config/*.php`.
    Config,
    /// A Blade template named by dotted path.
    View,
    /// `<x-forms.input>` — a Blade component, which resolves to one of two directories.
    Component,
    /// `wire:click="sortBy"` — a Livewire action; resolves into the component's class.
    WireAction,
    /// `wire:model="search"` — a Livewire property; same class, different member shape.
    WireProperty,
}

/// The Laravel reference at `offset`, if the cursor is inside one.
///
/// `offset` is a byte offset. `blade` selects the scanner rather than the PHP tree, because
/// the two file types share none of their syntax for this — see the module docs.
///
/// A `.blade.php` file still gets the scanner *and* nothing else: `{{ route('x') }}` inside
/// a template is found by the scanner's own function-call pass, not by parsing, since there
/// is no tree to parse it with.
pub fn reference_at(source: &str, offset: usize, blade: bool) -> Option<Reference> {
    if blade { blade_reference_at(source, offset) } else { php_reference_at(source, offset) }
}

// ---------------------------------------------------------------------------
// PHP, via the tree
// ---------------------------------------------------------------------------

/// The helper functions whose first argument names something navigable.
///
/// `trans` and `__` are deliberately absent: they name a translation key, which lives in a
/// nested array inside `lang/*.php` and needs a different resolver than a file path. The
/// issue lists them; resolving them is not the same problem, so they are left out rather
/// than half-done.
fn helper_kind(name: &str) -> Option<ReferenceKind> {
    Some(match name {
        "route" => ReferenceKind::Route,
        "config" => ReferenceKind::Config,
        "view" => ReferenceKind::View,
        _ => return None,
    })
}

/// Walks the PHP tree for a `route`/`config`/`view` call containing `offset`.
///
/// Deliberately the tree and not a regex, for the reason `routes.rs` gives: `route('x')`
/// written inside a comment or a heredoc is indistinguishable to a regex, and a ⌘click that
/// navigates from commented-out code is the confident wrongness RISKS.md #4 is about.
fn php_reference_at(source: &str, offset: usize) -> Option<Reference> {
    let buffer = Buffer::new(source);
    // The same parse path the editor uses (ADR-0005), so a grammar upgrade moves both.
    let tree = SyntaxTree::new(Language::Php, &buffer).ok()?;
    let tree = tree.tree()?;
    let src = source.as_bytes();

    // Descend to the smallest node containing the cursor, then walk back out looking for a
    // call. Starting from the leaf is what makes "the cursor is on the string" the question
    // being asked, rather than "the file contains a route() somewhere".
    let mut node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    loop {
        if node.kind() == "function_call_expression"
            && let Some(reference) = call_reference(node, src)
            // Only a click *on the literal* navigates. Clicking the word `route` itself is
            // a click on a PHP function whose definition is Laravel's, which the language
            // server answers better than we can.
            //
            // Inclusive of the end, unlike `Range::contains`, for two reasons that are
            // really one: the cursor sitting just past the last character is inside the
            // literal as far as the user is concerned, and an *empty* `route('')` is a
            // zero-width range that `contains` can never match — which would turn off
            // completion in exactly the moment it is wanted.
            && (reference.range.start..=reference.range.end).contains(&offset)
        {
            return Some(reference);
        }
        node = node.parent()?;
    }
}

/// Reads a `route('x')`-shaped call into a reference, or `None` if it is not one.
fn call_reference(call: Node, src: &[u8]) -> Option<Reference> {
    let name = call.child_by_field_name("function")?;
    // `name`, not `qualified_name`: `\route('x')` is legal and identical, but so is a
    // user-defined `App\Helpers\route`, and telling them apart needs the imports.
    if name.kind() != "name" {
        return None;
    }
    let kind = helper_kind(name.utf8_text(src).ok()?)?;

    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let first =
        args.named_children(&mut cursor).find(|c| c.kind() == "argument")?.named_child(0)?;

    literal(first, src).map(|(text, range)| Reference { kind, name: text, range })
}

/// A string node's content and byte range, or `None` if it is not a plain literal.
///
/// An `encapsed_string` with an `{$x}` in it parses with `string_content` children too, so
/// the check is that the node has *exactly one* named child and that it spans the quotes —
/// anything interpolated has more, or leaves a gap. That is the whole honesty rule for this
/// module: a name we cannot read off the source is not reported at all.
pub(crate) fn literal(node: Node, src: &[u8]) -> Option<(String, std::ops::Range<usize>)> {
    if !matches!(node.kind(), "string" | "encapsed_string") {
        return None;
    }
    // An empty literal has no `string_content` child at all, the same shape `routes.rs`
    // notes. It matters more here than there: `route('')` is precisely what someone has
    // just typed when they want the completion list, so reading it as "not a literal" would
    // disable the feature in its most common moment.
    if node.named_child_count() == 0 {
        let inside = node.start_byte() + 1..node.end_byte().saturating_sub(1);
        return inside.is_empty().then(|| (String::new(), inside));
    }

    let mut cursor = node.walk();
    let contents: Vec<Node> =
        node.named_children(&mut cursor).filter(|c| c.kind() == "string_content").collect();
    let [content] = contents[..] else { return None };
    // A double-quoted string with an interpolation has an `escape_sequence` or a
    // `variable_name` sibling; requiring the content to span the quotes exactly rules
    // those out without enumerating them.
    if content.start_byte() != node.start_byte() + 1 || content.end_byte() != node.end_byte() - 1 {
        return None;
    }
    let text = content.utf8_text(src).ok()?;
    Some((text.to_string(), content.start_byte()..content.end_byte()))
}

// ---------------------------------------------------------------------------
// Blade, via a scanner
// ---------------------------------------------------------------------------

/// Blade directives that take a template name as their first argument.
///
/// `@includeIf`, `@includeWhen` and `@includeUnless` take the name in a later position or
/// behind a condition; they are omitted rather than guessed at.
const VIEW_DIRECTIVES: [&str; 4] = ["include", "extends", "component", "each"];

/// Finds a Blade construct containing `offset`.
///
/// Scanner, not parser, per ADR-0006 and the module docs. It reads the buffer directly, so
/// unlike a second tree it cannot desync from the highlighter's view of the same file.
fn blade_reference_at(source: &str, offset: usize) -> Option<Reference> {
    component_at(source, offset)
        .or_else(|| wire_at(source, offset))
        .or_else(|| directive_at(source, offset))
        // `{{ route('x') }}` and `{{ config('y') }}` in a template. The scanner cannot tell
        // a call inside an echo from one inside a `{{-- comment --}}`, which is the price
        // of having no tree; a comment is the rarer place to write a live `route()` call,
        // and the failure is a ⌘click that navigates from a commented line rather than a
        // wrong destination.
        .or_else(|| helper_call_at(source, offset))
}

/// The value of a `wire:` attribute under the cursor, as a navigable reference (#24).
///
/// `wire:click="sortBy(1)"` navigates by the method name alone — the reference is the
/// leading identifier, and arguments stay out of the name the resolver looks up.
fn wire_at(source: &str, offset: usize) -> Option<Reference> {
    let (target, range) = crate::livewire::wire_context_at(source, offset)?;
    let value = &source[range.clone()];
    let name: String =
        value.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    if name.is_empty() {
        return None;
    }
    let kind = match target {
        crate::livewire::WireTarget::Action => ReferenceKind::WireAction,
        crate::livewire::WireTarget::Property => ReferenceKind::WireProperty,
    };
    Some(Reference { kind, name, range: range.start..range.start + value.len().min(range.len()) })
}

/// `<x-forms.input>` or `<x:forms.input>`, returning the component name.
fn component_at(source: &str, offset: usize) -> Option<Reference> {
    let bytes = source.as_bytes();
    // Scan backwards for the `<x-` that opens the tag the cursor is in. Bounded by the
    // start of the line: a component tag does not span one, and an unbounded scan over a
    // large template on every click is the cost this feature must not have.
    let line_start = source[..offset.min(source.len())].rfind('\n').map_or(0, |i| i + 1);
    let region = &source[line_start..];

    let mut search = 0;
    while let Some(found) = region[search..].find("<x") {
        let tag = search + found;
        search = tag + 2;
        // `<x-` and `<x:` are both Laravel's spelling; `<xyz>` is not a component.
        let Some(separator) = region.as_bytes().get(tag + 2) else { continue };
        if *separator != b'-' && *separator != b':' {
            continue;
        }
        let name_start = line_start + tag + 3;
        // A component name is dotted or hyphenated path segments. Stopping at the first
        // character that cannot appear in one is what ends the name at `>`, whitespace, or
        // a `/`.
        let name_end = name_start
            + bytes[name_start..]
                .iter()
                .position(|b| !matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b':'))
                .unwrap_or(bytes.len() - name_start);
        if name_end == name_start {
            continue;
        }
        // Inclusive of `name_end` so a click on the character just past the name — where a
        // user aiming at the last letter often lands — still counts.
        if (name_start..=name_end).contains(&offset) {
            return Some(Reference {
                kind: ReferenceKind::Component,
                name: source[name_start..name_end].to_string(),
                range: name_start..name_end,
            });
        }
    }
    None
}

/// `@include('partials.header')` and the other directives that name a template.
fn directive_at(source: &str, offset: usize) -> Option<Reference> {
    for directive in VIEW_DIRECTIVES {
        let needle = format!("@{directive}");
        let mut search = 0;
        while let Some(found) = source[search..].find(&needle) {
            let at = search + found;
            search = at + needle.len();
            // `@includeIf` must not match `@include`: the character after the directive
            // name has to be the opening paren or whitespace before it.
            let after = &source[search..];
            let after = after.trim_start_matches([' ', '\t']);
            if !after.starts_with('(') {
                continue;
            }
            let paren = source.len() - after.len();
            if let Some(reference) = first_quoted(source, paren, ReferenceKind::View, offset) {
                return Some(reference);
            }
        }
    }
    None
}

/// `{{ route('x') }}` inside a template, found without a tree.
fn helper_call_at(source: &str, offset: usize) -> Option<Reference> {
    for (helper, kind) in [
        ("route", ReferenceKind::Route),
        ("config", ReferenceKind::Config),
        ("view", ReferenceKind::View),
    ] {
        let mut search = 0;
        while let Some(found) = source[search..].find(helper) {
            let at = search + found;
            search = at + helper.len();
            // Not a substring of a longer identifier: `myroute(` and `$route` are not this.
            let before = source[..at].chars().next_back();
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '>') {
                continue;
            }
            let after = source[search..].trim_start_matches([' ', '\t']);
            if !after.starts_with('(') {
                continue;
            }
            let paren = source.len() - after.len();
            if let Some(reference) = first_quoted(source, paren, kind, offset) {
                return Some(reference);
            }
        }
    }
    None
}

/// The first quoted literal after `paren`, if it contains `offset`.
///
/// Interpolation and concatenation are refused the same way the PHP side refuses them: the
/// literal must run from its opening quote to a matching close with no `$`, `{` or `\`
/// inside. `@include("posts.{$slug}")` therefore resolves to nothing rather than to a
/// template named with a brace in it.
fn first_quoted(
    source: &str,
    paren: usize,
    kind: ReferenceKind,
    offset: usize,
) -> Option<Reference> {
    let rest = &source[paren + 1..];
    let leading = rest.len() - rest.trim_start().len();
    let quote = rest.as_bytes().get(leading).copied()?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let start = paren + 1 + leading + 1;
    let end = start + source[start..].find(quote as char)?;
    let text = &source[start..end];
    if text.contains(['$', '{', '\\', '\n']) {
        return None;
    }
    (start..=end).contains(&offset).then(|| Reference {
        kind,
        name: text.to_string(),
        range: start..end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offset of `needle`'s first character in `source`, for readable test positions.
    fn at(source: &str, needle: &str) -> usize {
        source.find(needle).unwrap_or_else(|| panic!("{needle:?} not in source"))
    }

    /// The state a `route('')` is in the instant before someone asks for completion. Both
    /// quote styles, because the empty case parses differently from a filled one and it
    /// would be easy to fix single quotes and leave double broken.
    #[test]
    fn an_empty_literal_reads_as_an_empty_name_rather_than_no_reference() {
        for src in ["<?php route('');\n", "<?php route(\"\");\n"] {
            let offset = at(src, "(") + 2;
            let found = reference_at(src, offset, false).unwrap_or_else(|| panic!("{src}"));
            assert_eq!(found.kind, ReferenceKind::Route);
            assert_eq!(found.name, "");
            assert!(found.range.is_empty(), "an empty literal spans no bytes: {src}");
            // The range sits between the quotes, so inserting there writes inside them.
            assert_eq!(src.as_bytes()[found.range.start - 1], src.as_bytes()[found.range.end]);
        }
    }

    #[test]
    fn a_route_helper_resolves_to_its_name() {
        let src = "<?php\n$url = route('users.show', $user);\n";
        let found = reference_at(src, at(src, "users.show"), false).expect("a reference");
        assert_eq!(found.kind, ReferenceKind::Route);
        assert_eq!(found.name, "users.show");
        assert_eq!(&src[found.range], "users.show");
    }

    #[test]
    fn config_and_view_helpers_are_recognised_too() {
        let src = "<?php\nconfig('app.name');\nview('partials.header');\n";
        let config = reference_at(src, at(src, "app.name"), false).expect("config");
        assert_eq!(config.kind, ReferenceKind::Config);
        assert_eq!(config.name, "app.name");

        let view = reference_at(src, at(src, "partials.header"), false).expect("view");
        assert_eq!(view.kind, ReferenceKind::View);
        assert_eq!(view.name, "partials.header");
    }

    /// RISKS.md #4 for this module. Every one of these has a name only the runtime knows,
    /// and a ⌘click must do nothing rather than navigate somewhere plausible.
    #[test]
    fn a_name_that_is_not_a_plain_literal_is_not_reported() {
        for src in [
            "<?php route($name);\n",
            "<?php route(\"users.{$id}\");\n",
            "<?php route('users.' . $suffix);\n",
            "<?php config($key);\n",
            "<?php view($template);\n",
        ] {
            // Anywhere inside the argument list.
            let offset = at(src, "(") + 2;
            assert_eq!(reference_at(src, offset, false), None, "must not resolve: {src}");
        }
    }

    /// The reason this walks the tree instead of matching text.
    #[test]
    fn a_helper_inside_a_comment_or_a_variable_name_is_not_a_reference() {
        let commented = "<?php\n// route('users.show')\n";
        assert_eq!(reference_at(commented, at(commented, "users.show"), false), None);

        // A user-defined method named `route` on an object is not Laravel's helper.
        let method = "<?php\n$router->route('users.show');\n";
        assert_eq!(reference_at(method, at(method, "users.show"), false), None);
    }

    #[test]
    fn clicking_outside_the_literal_reports_nothing() {
        let src = "<?php\n$url = route('users.show');\n";
        // On the word `route` itself: that is a PHP function, and the language server
        // answers it better than a path resolver can.
        assert_eq!(reference_at(src, at(src, "route"), false), None);
        // On the trailing semicolon.
        assert_eq!(reference_at(src, at(src, ";"), false), None);
    }

    #[test]
    fn a_wire_value_is_a_reference_to_the_components_member() {
        let source = r#"<button wire:click="sortBy(1)">x</button>"#;
        let at = source.find("sortBy").unwrap() + 2;
        let reference = reference_at(source, at, true).expect("a reference");
        assert_eq!(reference.kind, ReferenceKind::WireAction);
        assert_eq!(reference.name, "sortBy", "arguments stay out of the looked-up name");

        let model = r#"<input wire:model="search">"#;
        let at = model.find("search").unwrap();
        let reference = reference_at(model, at, true).expect("a reference");
        assert_eq!(reference.kind, ReferenceKind::WireProperty);
        assert_eq!(reference.name, "search");
    }

    #[test]
    fn a_blade_include_resolves_to_its_template() {
        let src = "<div>\n@include('partials.header')\n</div>\n";
        let found = reference_at(src, at(src, "partials.header"), true).expect("an include");
        assert_eq!(found.kind, ReferenceKind::View);
        assert_eq!(found.name, "partials.header");
        assert_eq!(&src[found.range], "partials.header");
    }

    #[test]
    fn extends_component_and_each_directives_resolve_the_same_way() {
        for (src, name) in [
            ("@extends('layouts.app')\n", "layouts.app"),
            ("@component('mail.message')\n", "mail.message"),
            ("@each('posts.row', $posts, 'post')\n", "posts.row"),
        ] {
            let found = reference_at(src, at(src, name), true).expect(name);
            assert_eq!(found.kind, ReferenceKind::View);
            assert_eq!(found.name, name);
        }
    }

    /// `@includeIf` starts with `@include`, and matching the prefix would make a click on
    /// its argument resolve as a plain include. It takes the same first argument, but the
    /// directive is a different one and this module does not claim to handle it.
    #[test]
    fn a_directive_that_merely_starts_with_include_is_not_treated_as_one() {
        let src = "@includeIf('partials.header')\n";
        assert_eq!(reference_at(src, at(src, "partials.header"), true), None);
    }

    #[test]
    fn a_blade_component_tag_resolves_to_its_name() {
        let src = "<div>\n  <x-forms.input name=\"email\" />\n</div>\n";
        let found = reference_at(src, at(src, "forms.input"), true).expect("a component");
        assert_eq!(found.kind, ReferenceKind::Component);
        assert_eq!(found.name, "forms.input");

        // The `<x:` spelling is the same component.
        let colon = "<x:forms.input />";
        let found = reference_at(colon, at(colon, "forms.input"), true).expect("a component");
        assert_eq!(found.name, "forms.input");
    }

    #[test]
    fn a_helper_call_inside_an_echo_is_found_without_a_tree() {
        let src = "<a href=\"{{ route('users.show', $user) }}\">Show</a>\n";
        let found = reference_at(src, at(src, "users.show"), true).expect("a route");
        assert_eq!(found.kind, ReferenceKind::Route);
        assert_eq!(found.name, "users.show");
    }

    #[test]
    fn an_interpolated_blade_argument_is_not_reported() {
        let src = "@include(\"posts.{$slug}\")\n";
        assert_eq!(reference_at(src, at(src, "posts."), true), None);
    }

    #[test]
    fn clicking_ordinary_blade_text_reports_nothing() {
        let src = "<div>Hello, {{ $name }}</div>\n";
        assert_eq!(reference_at(src, at(src, "Hello"), true), None);
    }
}
