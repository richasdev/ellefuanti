//! Syntax highlighting: parse tree (plus Blade scanning) to styled byte ranges.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Mutex, OnceLock};

use elle_text::Buffer;
use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

use crate::language::Language;
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
    /// Two paths, and the split is measured rather than stylistic.
    ///
    /// Languages with a `highlights.scm` go through [`query_spans`]: one query file per
    /// language instead of one `node_style` function per language, which is what makes
    /// the fifth grammar as cheap as the second.
    ///
    /// PHP and Blade keep the hand-written walk below. The upstream PHP `highlights.scm`
    /// was tried and **cannot** reproduce what this editor's PHP tests assert — it
    /// captures no `=`/`=>`/`->`/`::` operators, no `#[` attribute bracket and no
    /// attribute name, and it tags a class property `$name` as both variable and
    /// property. Three existing tests fail against it. Rewriting the query to match, then
    /// pinning it with those same tests, is a real option; swapping in the upstream file
    /// and relaxing the assertions is not, so PHP stays on the code that is already
    /// pinned. It is also 2.7× cheaper (32 µs against 87 µs on the 80-row viewport
    /// bench), which is not the reason but is not nothing.
    pub fn highlights(&self, buffer: &Buffer, range: Range<usize>) -> Vec<HighlightSpan> {
        let mut spans = Vec::new();

        if let Some(tree) = self.tree() {
            match self.language().highlight_query() {
                Some(source) => query_spans(
                    self.language(),
                    source,
                    &tree.root_node(),
                    buffer,
                    &range,
                    &mut spans,
                ),
                None => collect(&tree.root_node(), &range, &mut spans),
            }
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

/// Maps a `highlights.scm` capture name to a style, or `None` to ignore the capture.
///
/// Capture names are the tree-sitter ecosystem's shared vocabulary, and they are dotted
/// and open-ended: `@function.method`, `@string.special.key`, `@variable.builtin`. Only
/// the part before the first dot is matched, so a query using a more specific name than
/// this editor has a style for still colours — `@function.method` lands on Function
/// rather than falling through to no colour at all.
///
/// Unknown names return `None` on purpose: a query capture this editor has no style for
/// should render as plain text, not as a guess. That is the same rule `name_style`
/// follows for PHP.
fn capture_style(name: &str) -> Option<HighlightStyle> {
    use HighlightStyle::*;
    Some(match name.split('.').next().unwrap_or(name) {
        "keyword" => Keyword,
        "type" | "constructor" => Type,
        "function" => Function,
        "variable" => Variable,
        "property" => Property,
        "string" => String,
        "number" | "constant" => Number,
        "operator" | "escape" => Operator,
        "attribute" => Attribute,
        "comment" => Comment,
        "tag" => Tag,
        _ => return None,
    })
}

/// The compiled `Query` for each language, built once.
///
/// Compiling a query parses and optimises the whole `.scm` file, which is far too
/// expensive to redo per frame. Keyed by `Language` rather than held on `SyntaxTree`
/// because every open buffer of the same language wants the same query, and a `Query` is
/// read-only once built.
///
/// A `Mutex` around the map and a leaked `&'static Query` out of it, rather than handing
/// back a guard: the query outlives every borrow anyway (it is never evicted), and
/// leaking one small allocation per language beats holding a lock across the whole
/// highlight pass. Bounded by the number of languages, so it is not a growing leak.
fn compiled_query(language: Language, source: &str) -> Option<&'static Query> {
    static CACHE: OnceLock<Mutex<HashMap<Language, Option<&'static Query>>>> = OnceLock::new();

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());

    *cache.entry(language).or_insert_with(|| {
        let grammar = language.grammar()?;
        match Query::new(&grammar, source) {
            Ok(query) => Some(&*Box::leak(Box::new(query))),
            // A query that does not compile means no colour for that language, not a
            // crash in the renderer. `every_query_compiles` is what stops this from
            // being how anyone finds out.
            Err(error) => {
                tracing::warn!(language = language.name(), %error, "highlight query failed to compile");
                None
            }
        }
    })
}

/// Highlight spans from a `highlights.scm` query, restricted to `range`.
///
/// `set_byte_range` is what keeps this viewport-scoped: the query engine skips subtrees
/// that cannot intersect the range instead of scanning the file. Measured flat at 1.00×
/// across a 100× file-size range before this was written, because "the API should prune"
/// is exactly the kind of assumption BASELINE.md exists to warn about.
///
/// Later captures win. That is the convention upstream query files are written against —
/// `(identifier) @variable` first, then the specific patterns that overwrite it — and
/// getting it backwards paints every call in a JS file as a plain variable. `flatten`
/// keeps the *outermost* span at a shared start, which is the opposite rule, so the
/// override is resolved here by last-write-wins into a map before flatten ever sees it.
fn query_spans(
    language: Language,
    source: &str,
    root: &Node,
    buffer: &Buffer,
    range: &Range<usize>,
    out: &mut Vec<HighlightSpan>,
) {
    let Some(query) = compiled_query(language, source) else {
        return;
    };
    let names = query.capture_names();

    // The query engine needs the source text to evaluate `#match?` and `#eq?`
    // predicates. Slicing the whole buffer would be a per-frame copy of the file, so
    // the rope's own chunks are handed over instead — the same trick `parse_rope` uses,
    // for the same reason.
    let rope = buffer.rope();
    let text_provider = |node: Node| {
        let mut offset = node.start_byte();
        let end = node.end_byte();
        std::iter::from_fn(move || {
            if offset >= end {
                return None;
            }
            let (chunk, chunk_start, _, _) = rope.chunk_at_byte(offset);
            let from = offset - chunk_start;
            let to = (end - chunk_start).min(chunk.len());
            offset = chunk_start + to;
            Some(&chunk.as_bytes()[from..to])
        })
    };

    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(range.clone());

    // Keyed by span, so a later capture on the same node replaces an earlier one.
    let mut styles: HashMap<(usize, usize), HighlightStyle> = HashMap::new();
    let mut captures = cursor.captures(query, *root, text_provider);
    while let Some((m, _)) = captures.next() {
        for capture in m.captures {
            let node = capture.node;
            let span = (node.start_byte(), node.end_byte());
            if span.0 >= span.1 {
                continue;
            }
            match capture_style(names[capture.index as usize]) {
                Some(style) => {
                    styles.insert(span, style);
                }
                // An unmapped capture must still *clear* a mapped one it overrides,
                // or a general pattern's colour survives a specific pattern that
                // deliberately declined to colour the node.
                None => {
                    styles.remove(&span);
                }
            }
        }
    }

    out.extend(
        styles.into_iter().map(|((start, end), style)| HighlightSpan { range: start..end, style }),
    );
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
    use crate::language::{ALL_LANGUAGES, Language};

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
        //
        // Every language, not just PHP (#53). The two highlight paths fail this
        // differently and both failures are easy to introduce: the hand-written walk
        // regresses by iterating siblings instead of seeking, and the query path
        // regresses by forgetting `set_byte_range`, which makes the engine scan the whole
        // tree. Neither changes what the viewport *looks* like, so only this notices.
        //
        // Each case is (head, filler): the head is the fixed window that gets measured
        // and the filler is what grows underneath it.
        let cases: &[(Language, &str, &str)] = &[
            (Language::Php, "<?php\n$a = 1;\n$b = 2;\n$c = 3;\n", "function f() { return 'x'; }\n"),
            (
                Language::Blade,
                "@extends('l')\n{{ $a }}\n{{ $b }}\n",
                "<div>@if($x) {{ $y }} @endif</div>\n",
            ),
            // JSON has no top-level statement list, so the growth has to go inside the
            // one root object. The head is the first few pairs of that object.
            (Language::Json, "{\n\"a\": 1,\n\"b\": 2,\n", "\"k\": \"v\",\n"),
            (
                Language::JavaScript,
                "// h\nlet a = 1;\nlet b = 2;\n",
                "function f() { return 'x'; }\n",
            ),
            (
                Language::TypeScript,
                "// h\nlet a: number = 1;\nlet b = 2;\n",
                "function f(): string { return 'x'; }\n",
            ),
            (Language::Css, "/* h */\n.a { color: red; }\n", ".cls { margin: 1px; }\n"),
            (
                Language::Html,
                "<!-- h -->\n<p class=\"a\">x</p>\n",
                "<div id=\"k\"><span>y</span></div>\n",
            ),
            // TOML's filler has to be pairs at the top level, not tables: a `[table]`
            // header opens a node that every following pair nests inside, so growth would
            // deepen the tree the head sits next to rather than lengthen it.
            (Language::Toml, "# h\na = 1\nb = \"x\"\n", "k = \"v\"\n"),
            (Language::Yaml, "# h\na: 1\nb: \"x\"\n", "k: v\n"),
            (Language::Shell, "# h\nA=1\nB=\"x\"\n", "echo \"y\" > /dev/null\n"),
        ];

        for (language, head, filler) in cases {
            let build = |repeats: usize| {
                let mut s = String::from(*head);
                s.push_str(&filler.repeat(repeats));
                // JSON's growth lives inside the root object, so it has to be closed.
                if *language == Language::Json {
                    s.push_str("\"z\": 0\n}\n");
                }
                s
            };

            let count = |src: &str| {
                let buffer = Buffer::new(src);
                let tree = SyntaxTree::new(*language, &buffer).unwrap();
                // Same byte window in both: the head.
                tree.highlights(&buffer, 0..head.len()).len()
            };

            let small = count(&build(20));
            let large = count(&build(1000));

            assert!(small > 0, "{}: fixture produced no spans to compare", language.name());
            assert_eq!(
                small,
                large,
                "{}: a fixed viewport must cost the same regardless of what follows it",
                language.name()
            );
        }
    }

    #[test]
    fn every_language_highlights_something() {
        // A grammar that loads but produces no spans is the exact failure #53 is about,
        // and it is invisible from the outside — the file just renders grey. Each sample
        // is ordinary code for its language, so zero spans means the wiring is broken.
        let samples: &[(Language, &str)] = &[
            (Language::Php, "<?php class C { public $x = 1; }"),
            (Language::Blade, "@if($a) {{ $b }} @endif"),
            (Language::Json, "{\"a\": 1}"),
            (Language::JavaScript, "const a = 1; // c"),
            (Language::TypeScript, "const a: number = 1; // c"),
            (Language::Css, ".a { color: red; }"),
            (Language::Html, "<div class=\"a\">x</div>"),
            (Language::Toml, "[t]\na = 1"),
            (Language::Yaml, "a: 1\nb: \"x\""),
            (Language::Shell, "NAME=app # c"),
        ];

        for (language, src) in samples {
            assert!(
                !spans_of(*language, src).is_empty(),
                "{}: produced no highlight spans at all",
                language.name()
            );
        }

        // And the one that must stay empty, because the fallback is deliberate.
        assert!(spans_of(Language::PlainText, "anything at all").is_empty());
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

    /// Every language that claims a query must have one that compiles.
    ///
    /// `compiled_query` deliberately degrades to "no colour" on a malformed query rather
    /// than panicking a render. That is right at runtime and useless in development —
    /// a typo in a `.scm` would ship as a silently grey file type. This is the test that
    /// makes it a build failure instead.
    #[test]
    fn every_query_compiles() {
        for language in ALL_LANGUAGES {
            let Some(source) = language.highlight_query() else {
                continue;
            };
            let grammar = language.grammar().expect("a language with a query has a grammar");
            if let Err(error) = tree_sitter::Query::new(&grammar, source) {
                panic!("{}: highlight query does not compile: {error}", language.name());
            }
        }
    }

    #[test]
    fn json_keys_and_values_are_different_colours() {
        // The point of colouring JSON at all. Every leaf in composer.json is a string,
        // so if keys and values share a style the file is one undifferentiated block.
        // Also the regression test for query pattern order: `(pair key:)` has to come
        // *after* the general `(string)` arm or it is overwritten and this fails.
        let src = "{\n  \"name\": \"app\",\n  \"version\": 2,\n  \"ok\": true\n}\n";
        let spans = spans_of(Language::Json, src);

        assert!(styled(src, &spans, HighlightStyle::Property).contains(&"\"name\""));
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"\"app\""));
        assert!(styled(src, &spans, HighlightStyle::Number).contains(&"2"));
        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"true"));
        assert!(
            !styled(src, &spans, HighlightStyle::String).contains(&"\"name\""),
            "a key must not also read as a value"
        );
    }

    #[test]
    fn javascript_separates_calls_declarations_and_data() {
        let src = "// c\nimport x from 'y';\nconst f = (a) => { return Foo.bar(a + 1); };\n";
        let spans = spans_of(Language::JavaScript, src);

        assert!(styled(src, &spans, HighlightStyle::Comment).contains(&"// c"));
        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"import"));
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"'y'"));
        assert!(styled(src, &spans, HighlightStyle::Number).contains(&"1"));
        // `f` is declared as an arrow function, so it reads as a function, not a variable
        // — one of the specific patterns that must override `(identifier) @variable`.
        assert!(styled(src, &spans, HighlightStyle::Function).contains(&"f"));
        assert!(styled(src, &spans, HighlightStyle::Function).contains(&"bar"));
        assert!(styled(src, &spans, HighlightStyle::Type).contains(&"Foo"));
        assert!(styled(src, &spans, HighlightStyle::Operator).contains(&"=>"));
    }

    #[test]
    fn javascript_members_are_properties_not_variables() {
        let src = "class K { m() { this.n = 1; } }\n";
        let spans = spans_of(Language::JavaScript, src);

        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"class"));
        assert!(styled(src, &spans, HighlightStyle::Type).contains(&"K"));
        assert!(styled(src, &spans, HighlightStyle::Function).contains(&"m"));
        assert!(styled(src, &spans, HighlightStyle::Property).contains(&"n"));
    }

    #[test]
    fn typescript_types_beat_the_javascript_variable_default() {
        // TypeScript's query is appended to JavaScript's, so its `(type_identifier) @type`
        // lands after `(identifier) @variable` and wins. If the concatenation order in
        // `highlight_query` is ever flipped, every annotation here goes back to Variable.
        let src = "interface P { id: number; }\nconst g = (p: P): string => p.id;\nenum E { A }\n";
        let spans = spans_of(Language::TypeScript, src);

        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"interface"));
        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"enum"));
        assert!(styled(src, &spans, HighlightStyle::Type).contains(&"P"));
        assert!(styled(src, &spans, HighlightStyle::Type).contains(&"number"));
        assert!(styled(src, &spans, HighlightStyle::Type).contains(&"string"));
        assert!(styled(src, &spans, HighlightStyle::Property).contains(&"id"));
    }

    #[test]
    fn typescript_still_highlights_the_javascript_it_is_built_on() {
        // The other half of the concatenation: TypeScript's own file has no string,
        // comment or number patterns at all. If only it were loaded, a .ts file would
        // show types and keywords and nothing else.
        let src = "// c\nconst s = 'x';\nconst n = 42;\n";
        let spans = spans_of(Language::TypeScript, src);

        assert!(styled(src, &spans, HighlightStyle::Comment).contains(&"// c"));
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"'x'"));
        assert!(styled(src, &spans, HighlightStyle::Number).contains(&"42"));
    }

    #[test]
    fn css_separates_selectors_properties_and_values() {
        let src = "/* c */\n@media screen {\n  .cls a:hover { color: #fff; margin: 10px; }\n}\n";
        let spans = spans_of(Language::Css, src);

        assert!(styled(src, &spans, HighlightStyle::Comment).contains(&"/* c */"));
        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"@media"));
        assert!(styled(src, &spans, HighlightStyle::Property).contains(&"cls"));
        assert!(styled(src, &spans, HighlightStyle::Tag).contains(&"a"));
        assert!(styled(src, &spans, HighlightStyle::Attribute).contains(&"hover"));
        assert!(styled(src, &spans, HighlightStyle::Property).contains(&"color"));
        // A colour literal reads as a value, not as a quoted string.
        assert!(styled(src, &spans, HighlightStyle::Number).contains(&"#fff"));
    }

    #[test]
    fn css_custom_properties_read_as_variables() {
        // The `#match? "^--"` predicate arm, which is the one thing in these queries that
        // needs the text provider to work. If the rope chunking in `query_spans` were
        // broken, this is the assertion that notices.
        let src = ":root { --brand: red; }\n";
        let spans = spans_of(Language::Css, src);
        assert!(styled(src, &spans, HighlightStyle::Variable).contains(&"--brand"));
    }

    /// A `#match?` predicate on a node that spans a rope chunk boundary.
    ///
    /// `query_spans` feeds the query engine the rope's own chunks rather than one slice,
    /// the same trade `parse_rope` makes. An off-by-one in that chunking corrupts the
    /// text the predicate sees, and the failure mode is a silently unstyled token rather
    /// than a crash — so it needs a fixture big enough to actually span chunks.
    #[test]
    fn predicates_work_across_rope_chunk_boundaries() {
        let mut src = String::from(":root {\n");
        for i in 0..500 {
            src.push_str(&format!("  --custom-property-number-{i}: {i}px;\n"));
        }
        src.push_str("}\n");

        let buffer = Buffer::new(&src);
        assert!(buffer.rope().chunks().count() > 1, "fixture must span rope chunks");

        let tree = SyntaxTree::new(Language::Css, &buffer).unwrap();
        let spans = tree.highlights(&buffer, 0..buffer.len_bytes());
        let vars: Vec<_> = spans.iter().filter(|s| s.style == HighlightStyle::Variable).collect();
        assert_eq!(vars.len(), 500, "every custom property must match the ^-- predicate");
    }

    #[test]
    fn html_separates_tags_attributes_and_values() {
        let src = "<!DOCTYPE html>\n<!-- c -->\n<div class=\"a\" id=x>hi</div>\n";
        let spans = spans_of(Language::Html, src);

        assert!(styled(src, &spans, HighlightStyle::Comment).contains(&"<!-- c -->"));
        assert!(styled(src, &spans, HighlightStyle::Tag).contains(&"div"));
        assert!(styled(src, &spans, HighlightStyle::Attribute).contains(&"class"));
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"\"a\""));
        // An unquoted attribute value is still a value, and it is a different node kind
        // from the quoted one — both arms are needed or `id=x` loses its colour.
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"x"));
        // The doctype reads as markup, not as a number. Upstream calls it @constant, which
        // this editor's `capture_style` would land on Number; html.scm overrides it.
        assert!(styled(src, &spans, HighlightStyle::Tag).contains(&"<!DOCTYPE html>"));
        // Text content between tags stays plain — colouring prose is not the job.
        assert!(!styled(src, &spans, HighlightStyle::String).contains(&"hi"));
    }

    #[test]
    fn toml_keys_do_not_swallow_their_values() {
        // The regression this query was rewritten for. Upstream's `(pair (bare_key))
        // @property` puts the capture on the *pair*, so under this editor's last-wins rule
        // the whole `name = "app"` came back as one Property span and the string vanished.
        let src = "# c\n[package]\nname = \"app\"\nver = 2\nok = true\n";
        let spans = spans_of(Language::Toml, src);

        assert!(styled(src, &spans, HighlightStyle::Comment).contains(&"# c"));
        // A table header is the structure; a key is content. They must not be one colour.
        assert!(styled(src, &spans, HighlightStyle::Type).contains(&"package"));
        assert!(styled(src, &spans, HighlightStyle::Property).contains(&"name"));
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"\"app\""));
        assert!(styled(src, &spans, HighlightStyle::Number).contains(&"2"));
        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"true"));
        assert!(styled(src, &spans, HighlightStyle::Operator).contains(&"="));
    }

    #[test]
    fn toml_handles_dotted_keys_and_table_arrays() {
        let src = "[[bin]]\na.b = 1.5\n\"quoted\" = 1\n";
        let spans = spans_of(Language::Toml, src);

        assert!(styled(src, &spans, HighlightStyle::Type).contains(&"bin"));
        // A dotted key nests bare_keys, so the plain `(pair (bare_key))` arm never reaches
        // them — both halves have to come out coloured.
        let props = styled(src, &spans, HighlightStyle::Property);
        assert!(props.contains(&"a"), "got {props:?}");
        assert!(props.contains(&"b"), "got {props:?}");
        assert!(props.contains(&"\"quoted\""), "got {props:?}");
        assert!(styled(src, &spans, HighlightStyle::Number).contains(&"1.5"));
    }

    #[test]
    fn yaml_keys_are_a_different_colour_from_their_values() {
        // docker-compose.yml and .github/workflows/*.yml are why YAML is here, and in both
        // almost every token is an unquoted scalar. If keys and values share a style the
        // file is one flat block — the same failure json.scm exists to avoid.
        let src = "# c\nversion: \"3\"\nservices:\n  web:\n    ports:\n      - 80\n    \
                   tty: true\n    x: null\n";
        let spans = spans_of(Language::Yaml, src);

        assert!(styled(src, &spans, HighlightStyle::Comment).contains(&"# c"));
        let props = styled(src, &spans, HighlightStyle::Property);
        assert!(props.contains(&"version"), "got {props:?}");
        assert!(props.contains(&"services"), "got {props:?}");
        assert!(props.contains(&"ports"), "got {props:?}");
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"\"3\""));
        assert!(styled(src, &spans, HighlightStyle::Number).contains(&"80"));
        // Upstream calls these @boolean and @constant.builtin, neither of which this
        // editor maps — `true` would render plain and `null` would render as a Number.
        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"true"));
        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"null"));
    }

    #[test]
    fn yaml_anchors_and_aliases_read_as_variables() {
        // The `&defaults` / `*defaults` pair is how a docker-compose file avoids repeating
        // itself. Upstream captures them as @label, which this editor has no style for, so
        // without the remap they are invisible.
        let src = "base: &anc v\nuse: *anc\n";
        let spans = spans_of(Language::Yaml, src);
        let vars = styled(src, &spans, HighlightStyle::Variable);
        assert_eq!(vars, vec!["anc", "anc"], "both the anchor and the alias must colour");
    }

    #[test]
    fn shell_separates_commands_variables_and_strings() {
        let src = "#!/bin/bash\n# c\nNAME=\"app\"\nif [ -f x ]; then\n  echo \"$NAME\"\nfi\n\
                   f() { echo hi; }\n";
        let spans = spans_of(Language::Shell, src);

        assert!(styled(src, &spans, HighlightStyle::Comment).contains(&"# c"));
        // A shebang is a comment to this grammar, which is the right answer for colour.
        assert!(styled(src, &spans, HighlightStyle::Comment).contains(&"#!/bin/bash"));
        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"if"));
        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"fi"));
        assert!(styled(src, &spans, HighlightStyle::Function).contains(&"echo"));
        // A function's declaration site, which needs its own arm — the name is a bare
        // `word`, the same kind as every command argument in the file.
        assert!(styled(src, &spans, HighlightStyle::Function).contains(&"f"));
        // Upstream gives variable_name @property; in shell there is nothing else competing
        // for the word "variable", so it reads as one.
        assert!(styled(src, &spans, HighlightStyle::Variable).contains(&"NAME"));
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"\"$NAME\""));
        assert!(styled(src, &spans, HighlightStyle::Operator).contains(&"="));
    }

    #[test]
    fn env_files_colour_both_sides_of_the_assignment() {
        // `.env` has no grammar of its own and rides on bash (see `Language::Shell`). Its
        // values are bare words, which upstream's query captures nowhere — so before the
        // `value: (word)` arm this file came out with keys coloured and values plain,
        // which is exactly half a feature.
        let src = "# c\nAPP_NAME=Laravel\nDB_PORT=3306\nAPP_KEY=\n";
        let spans = spans_of(Language::Shell, src);

        assert!(styled(src, &spans, HighlightStyle::Variable).contains(&"APP_NAME"));
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"Laravel"));
        assert!(styled(src, &spans, HighlightStyle::Number).contains(&"3306"));
        // An empty value is valid `.env` and must not break the parse around it.
        assert!(styled(src, &spans, HighlightStyle::Variable).contains(&"APP_KEY"));
    }

    #[test]
    fn artisan_highlights_as_php_despite_the_shebang() {
        // `artisan` is detected by filename (it has no extension), and it opens with
        // `#!/usr/bin/env php` — which is not PHP. The grammar is loaded with LANGUAGE_PHP
        // rather than PHP_ONLY precisely so text outside `<?php` parses as inline HTML
        // instead of erroring, so the shebang should cost nothing. Asserted rather than
        // assumed: if it did error, the whole file would come back with no spans, which is
        // the #53 symptom wearing a different hat.
        let src = "#!/usr/bin/env php\n<?php\n\nrequire __DIR__.'/vendor/autoload.php';\n\
                   define('LARAVEL_START', microtime(true));\n";
        let spans = spans_of(Language::Php, src);

        assert!(styled(src, &spans, HighlightStyle::Keyword).contains(&"require"));
        assert!(styled(src, &spans, HighlightStyle::Function).contains(&"define"));
        assert!(styled(src, &spans, HighlightStyle::String).contains(&"'LARAVEL_START'"));
        assert!(styled(src, &spans, HighlightStyle::Function).contains(&"microtime"));

        // The shebang itself stays plain. It is not PHP, and the grammar parses it as inline
        // text — which is the point: it does not become an error node that unstyles the rest.
        assert!(spans.iter().all(|s| !src[s.range.clone()].starts_with("#!")));
    }

    #[test]
    fn unknown_extensions_still_fall_back_to_plain_text() {
        // The `PlainText` fallback is deliberate (#53) and adding four languages must not
        // have turned an unrecognised file into a parse error.
        assert!(spans_of(Language::PlainText, "{ \"a\": 1 }").is_empty());
    }
}
