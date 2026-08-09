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
    String,
    Number,
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
    spans.sort_by(|a, b| {
        a.range.start.cmp(&b.range.start).then(b.range.end.cmp(&a.range.end))
    });

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

/// Depth-first walk, skipping subtrees that cannot intersect the visible range.
fn collect(node: &Node, range: &Range<usize>, out: &mut Vec<HighlightSpan>) {
    if node.end_byte() <= range.start || node.start_byte() >= range.end {
        return;
    }

    if let Some(style) = style_for(node.kind()) {
        out.push(HighlightSpan { range: node.start_byte()..node.end_byte(), style });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(&child, range, out);
    }
}

/// Maps a tree-sitter PHP node kind to a style. `None` means "no style of its own".
fn style_for(kind: &str) -> Option<HighlightStyle> {
    use HighlightStyle::*;
    Some(match kind {
        "comment" => Comment,
        "string" | "encapsed_string" | "string_content" | "heredoc" | "nowdoc" => String,
        "integer" | "float" => Number,
        "variable_name" | "$" => Variable,
        "name" => return None, // too generic on its own; parent context decides
        "php_tag" | "text_interpolation" | "?>" => Tag,
        "class_declaration" | "interface_declaration" | "trait_declaration" | "enum_declaration" => {
            return None;
        }
        "named_type" | "primitive_type" | "optional_type" | "cast_type" => Type,
        "function_definition" | "method_declaration" => return None,
        "function_call_expression" | "member_call_expression" | "scoped_call_expression" => {
            return None;
        }
        // Keywords: tree-sitter-php exposes these as anonymous nodes named literally.
        "abstract" | "and" | "array" | "as" | "break" | "callable" | "case" | "catch" | "class"
        | "clone" | "const" | "continue" | "declare" | "default" | "do" | "echo" | "else"
        | "elseif" | "enum" | "extends" | "final" | "finally" | "fn" | "for" | "foreach"
        | "function" | "global" | "if" | "implements" | "include" | "include_once" | "instanceof"
        | "insteadof" | "interface" | "match" | "namespace" | "new" | "or" | "print" | "private"
        | "protected" | "public" | "readonly" | "require" | "require_once" | "return" | "static"
        | "switch" | "throw" | "trait" | "try" | "use" | "var" | "while" | "yield" | "xor"
        | "null" | "true" | "false" => Keyword,
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
