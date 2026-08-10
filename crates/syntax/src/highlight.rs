//! Syntax highlighting: parse tree (plus Blade scanning) to styled byte ranges.

use std::ops::Range;

use elle_text::Buffer;
use tree_sitter::Node;

use crate::tree::SyntaxTree;

/// A semantic style. Themes map these to colours; the highlighter never names a colour
/// itself, so a theme change needs no reparse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HighlightStyle {
    Keyword,
    Type,
    Function,
    Variable,
    /// Member being accessed: the `name` in `$user->name` or `Foo::$bar`. Distinct from
    /// `Variable` because `$user` and `name` are different things wearing one expression.
    Property,
    String,
    Number,
    /// `=>`, `->`, `::`, `??`, arithmetic and comparison operators.
    Operator,
    /// `#[Route(...)]` — the brackets and the attribute's name.
    Attribute,
    Comment,
    /// PHP `<?php` / `?>` tags and HTML markup around them.
    Tag,
    /// Blade `@directive` and `{{ }}` delimiters.
    BladeDirective,
}

/// A styled byte range. Non-overlapping and sorted by `range.start`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    pub style: HighlightStyle,
}

impl SyntaxTree {
    /// Highlight spans intersecting `range` (a byte range, normally just the visible
    /// rows — §7 "renderizar apenas regiões necessárias/visíveis").
    ///
    /// ponytail: maps node kinds to styles directly instead of loading tree-sitter's
    /// `highlights.scm` query files. No query assets to ship or keep in sync, and it
    /// covers PHP's common kinds. Move to the query-based `tree_sitter_highlight` when
    /// a second real grammar lands and the match arms start duplicating.
    pub fn highlights(&self, buffer: &Buffer, range: Range<usize>) -> Vec<HighlightSpan> {
        let mut spans = Vec::new();

        if let Some(tree) = self.tree() {
            collect(&tree.root_node(), &range, &mut spans);
        }

        if self.language().has_blade_directives() {
            // Blade spans go in after the tree's, so on an exact tie flatten() keeps the
            // Blade one — inside a Blade file, `{{ }}` beats whatever PHP called it.
            spans.extend(blade_spans(buffer, &range));
        }

        flatten(spans)
    }
}

/// Reduces overlapping spans to a sorted, non-overlapping list.
///
/// The tree walk emits a span for every styled node, and PHP nests styled nodes inside
/// styled nodes: `variable_name "$name"` contains `$`, and `string "'ana'"` contains its
/// own quotes. Outermost wins — the renderer wants one colour per region, and the outer
/// node is the meaningful one ("this is a variable", not "this is a dollar sign").
fn flatten(mut spans: Vec<HighlightSpan>) -> Vec<HighlightSpan> {
    // Widest-first at a shared start, so the outer span is seen before its children.
    spans.sort_by(|a, b| a.range.start.cmp(&b.range.start).then(b.range.end.cmp(&a.range.end)));

    let mut out: Vec<HighlightSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        // Contained in (or duplicating) the span already kept: drop it.
        if out.last().is_some_and(|prev| span.range.start < prev.range.end) {
            continue;
        }
        out.push(span);
    }
    out
}

/// Depth-first walk, visiting only nodes that intersect the visible range.
///
/// Uses a `TreeCursor` with `goto_first_child_for_byte`, which binary-searches to the
/// first child containing an offset instead of scanning siblings. That distinction is the
/// difference between viewport cost and file cost: iterating children linearly means the
/// root's child list — one entry per top-level declaration — is walked in full on every
/// frame, so a 1000-class file pays 1000 comparisons to find the two classes on screen.
/// Measured at 50 µs / 60 µs / 156 µs across a 100× size range before this change, and
/// flat after; `highlights/viewport_80_rows` in the syntax bench is the guard.
fn collect(root: &Node, range: &Range<usize>, out: &mut Vec<HighlightSpan>) {
    if root.end_byte() <= range.start || root.start_byte() >= range.end {
        return;
    }

    let mut cursor = root.walk();

    // Ancestors of the node under the cursor, innermost last, so `name_style` can be handed
    // a parent without asking for one. `Node::parent()` is not a field read — tree-sitter
    // re-descends from the root — and `name`, the kind that needs a parent, is among the
    // most common nodes in PHP. Calling it per node measured 38 → 66 µs on
    // `highlights/viewport_80_rows`; reading it only when stepping out was worse still
    // (~125 µs, and no longer flat). Both attributed by ablation rather than guessed at.
    //
    // Depth-bounded, not size-bounded: one entry per nesting level under the cursor, so it
    // cannot reintroduce a cost that grows with the file.
    let mut ancestors: Vec<Node> = Vec::new();

    // Seek to the deepest node containing the start of the range. Each step binary-searches
    // one child list, so reaching the viewport costs O(depth · log breadth), not O(nodes).
    // Styled ancestors passed on the way are recorded: a comment or string that opens above
    // the viewport must still colour its visible remainder.
    //
    // The invariant, kept identical on both paths: `ancestors.last()` is the parent of the
    // node under the cursor. So the node we are *leaving* is pushed before descending, and
    // the node we arrive at is never pushed — the loop below re-visits it and needs its
    // parent on top, not itself. Getting this wrong styled `$user` with `$` as its own
    // parent and dropped the property in `$user->name` entirely.
    loop {
        let parent = cursor.node();
        if cursor.goto_first_child_for_byte(range.start).is_none() {
            break;
        }
        ancestors.push(parent);
        let node = cursor.node();
        if let Some(style) = node_style(node, parent) {
            out.push(HighlightSpan { range: node.start_byte()..node.end_byte(), style });
        }
    }

    // Then walk forward in document order, taking every styled node until past the range.
    // flatten() resolves the overlaps this produces (including any ancestor pushed above).
    loop {
        let node = cursor.node();
        if node.start_byte() >= range.end {
            return;
        }
        if node.end_byte() > range.start
            && let Some(style) = node_style(node, *ancestors.last().unwrap_or(root))
        {
            out.push(HighlightSpan { range: node.start_byte()..node.end_byte(), style });
        }

        if cursor.goto_first_child() {
            ancestors.push(node);
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return;
            }
            ancestors.pop();
        }
    }
}

/// Maps a tree-sitter PHP node to a style. `None` means "no style of its own".
///
/// Takes the node rather than just its kind because PHP's most common tokens are not
/// distinguishable by kind alone: the property in `$user->name`, the callee in `f()`, the
/// class in `class C`, and a bare constant are *all* the kind `name`. Only the parent (and
/// which field of the parent it occupies) separates them, so that is what `name_style`
/// looks at. Everything else still switches on the kind.
///
/// The container nodes stay unstyled on purpose. `member_access_expression` spans all of
/// `$user->name`, and `flatten()` keeps the outermost span at a shared start — styling the
/// container would swallow the variable and the property into one colour. The tokens are
/// coloured as leaves instead, which is what makes them come out different.
fn node_style(node: Node, parent: Node) -> Option<HighlightStyle> {
    use HighlightStyle::*;
    Some(match node.kind() {
        "comment" => Comment,
        "string" | "encapsed_string" | "string_content" | "heredoc" | "nowdoc" => String,
        "integer" | "float" => Number,
        // Hex/binary/octal and `1_000` are all `integer` to this grammar — no extra arms.
        "variable_name" | "$" => Variable,
        // `$this` is not its own kind: it parses as `variable_name` containing `name
        // "this"`, so it already reads as Variable and needs no arm. Checked, not assumed.
        "name" => return name_style(node, parent),
        "php_tag" | "text_interpolation" | "?>" => Tag,
        "class_declaration"
        | "interface_declaration"
        | "trait_declaration"
        | "enum_declaration" => {
            return None;
        }
        "named_type" | "primitive_type" | "optional_type" | "cast_type" => Type,
        "function_definition" | "method_declaration" => return None,
        "function_call_expression" | "member_call_expression" | "scoped_call_expression" => {
            return None;
        }
        // Attributes. `#[` and `]` are children of attribute_group, and the attribute's
        // name is reached via name_style; styling `attribute_list` itself would swallow
        // the arguments, which should keep their own string/number colours.
        "#[" => Attribute,
        // Operators. Anonymous nodes named literally, like the keywords below. `->`, `::`
        // and `=>` are the ones that carry meaning; the arithmetic set is included so a
        // line of maths does not read as undifferentiated text.
        "->" | "?->" | "::" | "=>" | "??" | "?" | "=" | "+" | "-" | "*" | "/" | "%" | "**"
        | "." | "==" | "===" | "!=" | "!==" | "<>" | "<" | ">" | "<=" | ">=" | "<=>" | "&&"
        | "||" | "!" | "??=" | "+=" | "-=" | "*=" | "/=" | ".=" | "%=" | "++" | "--" | "|"
        | "&" | "^" | "<<" | ">>" | "~" => Operator,
        // Keywords: tree-sitter-php exposes these as anonymous nodes named literally.
        "abstract" | "and" | "array" | "as" | "break" | "callable" | "case" | "catch" | "class"
        | "clone" | "const" | "continue" | "declare" | "default" | "do" | "echo" | "else"
        | "elseif" | "enum" | "extends" | "final" | "finally" | "fn" | "for" | "foreach"
        | "function" | "global" | "if" | "implements" | "include" | "include_once"
        | "instanceof" | "insteadof" | "interface" | "match" | "namespace" | "new" | "or"
        | "print" | "private" | "protected" | "public" | "readonly" | "require"
        | "require_once" | "return" | "static" | "switch" | "throw" | "trait" | "try" | "use"
        | "var" | "while" | "yield" | "xor" | "null" | "true" | "false" => Keyword,
        _ => return None,
    })
}

/// Disambiguates a bare `name` node by what contains it.
///
/// tree-sitter-php spends one kind, `name`, on nearly every identifier: property, method,
/// function, class, parameter label, constant. The parent is the only thing separating
/// them, so `collect` passes it in — see there for why it is not looked up here.
///
/// Anything not listed stays `None` (plain text) rather than guessing. That is deliberate:
/// a wrong colour is worse than no colour, and the ambiguous cases below are ambiguous in
/// the tree, not merely unimplemented.
fn name_style(node: Node, parent: Node) -> Option<HighlightStyle> {
    use HighlightStyle::*;

    Some(match parent.kind() {
        // `$user->name` / `$user?->name` — the `name` field is the property. The object is
        // a sibling `variable_name` and keeps Variable, which is the whole point.
        "member_access_expression" | "nullsafe_member_access_expression" => Property,
        // `$user->name()` — the callee reads as a call, not a field.
        "member_call_expression" | "nullsafe_member_call_expression" => Function,
        // `f()` and `Foo::bar()`: only the `function`/`name` field is the callee. In
        // `Foo::bar()` the `scope` field is also a `name` and must stay a Type.
        "function_call_expression" | "scoped_call_expression" => {
            match parent.child_by_field_name("scope") {
                Some(scope) if scope.id() == node.id() => Type,
                _ => Function,
            }
        }
        // Declaration sites.
        "function_definition" | "method_declaration" => Function,
        "class_declaration"
        | "interface_declaration"
        | "trait_declaration"
        | "enum_declaration" => Type,
        // `new User()`, `use App\Models\User`, `implements Foo`, a type annotation.
        "object_creation_expression"
        | "qualified_name"
        | "namespace_name"
        | "namespace_use_clause"
        | "base_clause"
        | "class_interface_clause"
        | "named_type" => Type,
        // `Foo::$bar` — the scope is the class, the property is the sibling variable_name.
        "scoped_property_access_expression" => Type,
        // `#[Route(...)]` — the attribute's own name. Its arguments are separate nodes and
        // keep their string/number colours.
        "attribute" => Attribute,
        // `name: 'y'` in an argument list is a parameter label, not a value.
        "argument" => return None,
        _ => return None,
    })
}

/// Blade constructs the PHP grammar does not know about.
///
/// A scanner, not a parser: it finds `@directive`, `{{ … }}`, `{!! … !!}` and `{{-- --}}`
/// by literal search. That is enough to colour a Blade file correctly, and it cannot
/// desync from the parse tree because it reads the buffer text directly.
fn blade_spans(buffer: &Buffer, range: &Range<usize>) -> Vec<HighlightSpan> {
    // Widen slightly so a construct straddling the viewport edge still highlights.
    const PAD: usize = 256;
    let start = range.start.saturating_sub(PAD);
    let end = (range.end + PAD).min(buffer.len_bytes());
    let text = buffer.slice(start..end);

    let mut spans = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'@' => {
                // `@@` escapes a literal @ in Blade; skip both.
                if bytes.get(i + 1) == Some(&b'@') {
                    i += 2;
                    continue;
                }
                let name_end = bytes[i + 1..]
                    .iter()
                    .position(|b| !b.is_ascii_alphanumeric() && *b != b'_')
                    .map_or(bytes.len(), |p| i + 1 + p);
                if name_end > i + 1 {
                    spans.push(HighlightSpan {
                        range: start + i..start + name_end,
                        style: HighlightStyle::BladeDirective,
                    });
                }
                i = name_end;
            }
            b'{' => {
                // Longest opener first: {{-- before {{, and {!! is its own form.
                let (open, close) = if text[i..].starts_with("{{--") {
                    ("{{--", "--}}")
                } else if text[i..].starts_with("{!!") {
                    ("{!!", "!!}")
                } else if text[i..].starts_with("{{") {
                    ("{{", "}}")
                } else {
                    i += 1;
                    continue;
                };

                let style = if open == "{{--" {
                    HighlightStyle::Comment
                } else {
                    HighlightStyle::BladeDirective
                };
                // Unterminated construct (user mid-typing) highlights to end of slice
                // rather than being dropped, so colour does not flicker while typing.
                let body_end = text[i + open.len()..]
                    .find(close)
                    .map_or(text.len(), |p| i + open.len() + p + close.len());
                spans.push(HighlightSpan { range: start + i..start + body_end, style });
                i = body_end;
            }
            _ => i += 1,
        }
    }

    spans.retain(|s| s.range.end > range.start && s.range.start < range.end);
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;

    fn spans_of(lang: Language, text: &str) -> Vec<HighlightSpan> {
        let buffer = Buffer::new(text);
        let tree = SyntaxTree::new(lang, &buffer).unwrap();
        tree.highlights(&buffer, 0..buffer.len_bytes())
    }

    fn styled<'a>(text: &'a str, spans: &[HighlightSpan], style: HighlightStyle) -> Vec<&'a str> {
        spans.iter().filter(|s| s.style == style).map(|s| &text[s.range.clone()]).collect()
    }

    #[test]
    fn highlights_php_keywords_strings_and_comments() {
        let src = "<?php\n// note\nclass User { public $name = 'ana'; }\n";
        let spans = spans_of(Language::Php, src);

        assert!(styled(src, &spans, HighlightStyle::Comment).contains(&"// note"));
        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"class"));
        assert!(styled(src, &spans, HighlightStyle::Variable).contains(&"$name"));
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"'ana'"));
    }

    #[test]
    fn nested_nodes_keep_the_outermost_style() {
        // Regression: `variable_name "$name"` contains a `$` child and `string "'ana'"`
        // contains its quote children. Keeping the inner node shrank `$name` to `$`.
        let src = "<?php $name = 'ana';";
        let spans = spans_of(Language::Php, src);
        assert!(styled(src, &spans, HighlightStyle::Variable).contains(&"$name"));
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"'ana'"));
    }

    #[test]
    fn variable_and_property_get_different_styles() {
        // The case this issue exists for. `$user->name` is one member_access_expression;
        // colouring the expression would make both tokens one colour, and colouring only
        // the `$` would leave `user` plain. The variable and the property must differ.
        let src = "<?php $user->name;";
        let spans = spans_of(Language::Php, src);

        assert!(styled(src, &spans, HighlightStyle::Variable).contains(&"$user"));
        assert!(styled(src, &spans, HighlightStyle::Property).contains(&"name"));

        let style_of = |needle: &str| {
            let at = src.find(needle).unwrap();
            spans.iter().find(|s| s.range.start == at).map(|s| s.style)
        };
        assert_ne!(
            style_of("$user"),
            style_of("name"),
            "the variable and the property must not share a style"
        );
    }

    #[test]
    fn property_access_is_not_a_method_call() {
        // `$user->name` is a field, `$user->save()` is a call. Same node kind for the
        // `name` child; only the parent separates them.
        let src = "<?php $user->name; $user->save();";
        let spans = spans_of(Language::Php, src);
        assert!(styled(src, &spans, HighlightStyle::Property).contains(&"name"));
        assert!(styled(src, &spans, HighlightStyle::Function).contains(&"save"));
    }

    #[test]
    fn this_reads_as_a_variable() {
        // tree-sitter-php gives `$this` no distinct kind — it is a plain `variable_name`
        // wrapping `name "this"`. Pinned so a future arm does not accidentally reclassify.
        let src = "<?php $this->name;";
        let spans = spans_of(Language::Php, src);
        assert!(styled(src, &spans, HighlightStyle::Variable).contains(&"$this"));
    }

    #[test]
    fn highlights_numbers_in_every_base() {
        let src = "<?php $a = 42; $b = 1.5; $c = 0x1f; $d = 0b11; $e = 1_000;";
        let spans = spans_of(Language::Php, src);
        let nums = styled(src, &spans, HighlightStyle::Number);
        for want in ["42", "1.5", "0x1f", "0b11", "1_000"] {
            assert!(nums.contains(&want), "expected {want} to be a Number, got {nums:?}");
        }
    }

    #[test]
    fn highlights_operators() {
        let src = "<?php $a = ['k' => 1]; $b = $a ?? 2; $c = $a->d; $e = Foo::F; $f = 1 + 2;";
        let spans = spans_of(Language::Php, src);
        let ops = styled(src, &spans, HighlightStyle::Operator);
        for want in ["=>", "??", "->", "::", "+", "="] {
            assert!(ops.contains(&want), "expected {want} to be an Operator, got {ops:?}");
        }
    }

    #[test]
    fn attributes_are_not_comments() {
        // `#[Route(...)]` starts with `#`, which is also a PHP line-comment opener. The
        // point of the Attribute style is that these stop looking alike.
        let src = "<?php #[Route('/users')] class C {}";
        let spans = spans_of(Language::Php, src);

        let attrs = styled(src, &spans, HighlightStyle::Attribute);
        assert!(attrs.contains(&"#["), "expected the `#[` opener, got {attrs:?}");
        assert!(attrs.contains(&"Route"), "expected the attribute name, got {attrs:?}");
        assert!(
            !spans.iter().any(|s| s.style == HighlightStyle::Comment),
            "an attribute must not be styled as a comment"
        );
        // Arguments inside keep their own styles rather than being swallowed.
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"'/users'"));
    }

    #[test]
    fn scoped_call_keeps_the_class_and_the_method_apart() {
        // In `Foo::bar()` both `Foo` and `bar` are `name` nodes; the `scope` field is the
        // only thing distinguishing them.
        let src = "<?php Foo::bar();";
        let spans = spans_of(Language::Php, src);
        assert!(styled(src, &spans, HighlightStyle::Type).contains(&"Foo"));
        assert!(styled(src, &spans, HighlightStyle::Function).contains(&"bar"));
    }

    #[test]
    fn parent_tracking_survives_deep_nesting_and_backtracking() {
        // `collect` maintains an ancestor stack instead of calling `Node::parent()`, so a
        // mismatched push/pop hands `name_style` the wrong parent and mis-colours silently
        // rather than crashing. Nesting plus repeated stepping-out is what desynchronises
        // it, so assert styles still land after both.
        let src = "<?php\nclass C {\n  public function m() {\n    if (true) {\n      \
                   return $this->a->b;\n    }\n    return Foo::bar();\n  }\n}\n";
        let spans = spans_of(Language::Php, src);

        assert!(styled(src, &spans, HighlightStyle::Property).contains(&"b"));
        assert!(styled(src, &spans, HighlightStyle::Variable).contains(&"$this"));
        assert!(styled(src, &spans, HighlightStyle::Function).contains(&"bar"));
        assert!(styled(src, &spans, HighlightStyle::Type).contains(&"Foo"));
        // `m` is a declaration site several levels up from where the walk ends.
        assert!(styled(src, &spans, HighlightStyle::Function).contains(&"m"));
    }

    #[test]
    fn styles_are_the_same_whether_seeked_to_or_walked_into() {
        // Two ways into the same node: the binary-search seek (viewport starting mid-file)
        // and the forward walk (viewport starting at 0). They must agree, or the ancestor
        // stack is built differently on the two paths — which it was: the seek pushed the
        // node it landed on, so `$user` got `$` as its own parent and `name` lost Property.
        let src = "<?php\n$pad = 1;\n$user->name;\n";
        let buffer = Buffer::new(src);
        let tree = SyntaxTree::new(Language::Php, &buffer).unwrap();

        let at = src.find("$user").unwrap();
        let end = at + "$user->name".len();
        let seeked = tree.highlights(&buffer, at..end);
        let walked: Vec<_> = tree
            .highlights(&buffer, 0..buffer.len_bytes())
            .into_iter()
            .filter(|s| s.range.start >= at && s.range.end <= end)
            .collect();

        assert_eq!(seeked, walked, "seek and walk must agree on styles");
        assert!(seeked.iter().any(|s| s.style == HighlightStyle::Property));
    }

    #[test]
    fn spans_are_sorted_and_non_overlapping() {
        let src = "<?php\nfunction f(int $a) { return $a + 1; }\n";
        let spans = spans_of(Language::Php, src);
        assert!(!spans.is_empty());
        for pair in spans.windows(2) {
            assert!(
                pair[0].range.start < pair[1].range.start,
                "spans must be strictly ordered: {:?}",
                pair
            );
        }
    }

    #[test]
    fn only_visible_range_is_walked() {
        let mut src = String::from("<?php\n");
        for i in 0..500 {
            src.push_str(&format!("$v{i} = 'x';\n"));
        }
        let buffer = Buffer::new(&src);
        let tree = SyntaxTree::new(Language::Php, &buffer).unwrap();

        let window = 0..80;
        let spans = tree.highlights(&buffer, window.clone());
        assert!(!spans.is_empty());
        assert!(
            spans.iter().all(|s| s.range.start < window.end),
            "no span may start past the requested range"
        );
        assert!(spans.len() < tree.highlights(&buffer, 0..buffer.len_bytes()).len());
    }

    #[test]
    fn viewport_cost_does_not_grow_with_file_size() {
        // The property the benchmark measures, asserted structurally so it cannot regress
        // silently: the number of nodes visited for a fixed viewport must not scale with
        // the file. Counting spans is a proxy for nodes visited — a linear sibling scan
        // over top-level declarations would not change the span count, so this instead
        // asserts the *spans* stay constant while the file grows 50×, which only holds if
        // the walk is confined to the viewport.
        let head = "<?php\n$a = 1;\n$b = 2;\n$c = 3;\n";

        let small = {
            let mut s = String::from(head);
            s.push_str(&"function f() { return 'x'; }\n".repeat(20));
            s
        };
        let large = {
            let mut s = String::from(head);
            s.push_str(&"function f() { return 'x'; }\n".repeat(1000));
            s
        };

        let count = |src: &str| {
            let buffer = Buffer::new(src);
            let tree = SyntaxTree::new(Language::Php, &buffer).unwrap();
            // Same byte window in both: the first three statements.
            tree.highlights(&buffer, 0..head.len()).len()
        };

        assert_eq!(
            count(&small),
            count(&large),
            "a fixed viewport must cost the same regardless of what follows it"
        );
    }

    #[test]
    fn enclosing_span_started_above_the_viewport_still_colours() {
        // A block comment opening before the visible range must colour the visible part,
        // which is what the seek-with-ancestors step exists for.
        let src = "<?php\n/* opened up here\nstill inside\nand here too */\n$x = 1;\n";
        let buffer = Buffer::new(src);
        let tree = SyntaxTree::new(Language::Php, &buffer).unwrap();

        // Window covering only the middle of the comment.
        let start = src.find("still").unwrap();
        let spans = tree.highlights(&buffer, start..start + 5);
        assert!(
            spans.iter().any(|s| s.style == HighlightStyle::Comment),
            "expected the enclosing comment, got {spans:?}"
        );
    }

    #[test]
    fn highlights_blade_directives_and_echoes() {
        let src = "@extends('l')\n<div>{{ $user->name }}</div>\n{{-- hidden --}}\n{!! $raw !!}";
        let spans = spans_of(Language::Blade, src);

        let blade = styled(src, &spans, HighlightStyle::BladeDirective);
        assert!(blade.contains(&"@extends"));
        assert!(blade.contains(&"{{ $user->name }}"));
        assert!(blade.contains(&"{!! $raw !!}"));
        assert!(styled(src, &spans, HighlightStyle::Comment).contains(&"{{-- hidden --}}"));
    }

    #[test]
    fn blade_escape_and_unterminated_echo() {
        let src = "@@notdirective {{ open";
        let spans = spans_of(Language::Blade, src);
        let blade = styled(src, &spans, HighlightStyle::BladeDirective);
        assert!(!blade.iter().any(|s| s.starts_with("@@")));
        // Unterminated `{{` still colours, so typing does not flicker.
        assert!(blade.iter().any(|s| s.starts_with("{{")));
    }

    #[test]
    fn php_file_does_not_get_blade_spans() {
        let spans = spans_of(Language::Php, "<?php $x = 1; // @if {{ }}");
        assert!(spans.iter().all(|s| s.style != HighlightStyle::BladeDirective));
    }

    #[test]
    fn plain_text_has_no_spans() {
        assert!(spans_of(Language::PlainText, "@if {{ x }} class").is_empty());
    }
}
