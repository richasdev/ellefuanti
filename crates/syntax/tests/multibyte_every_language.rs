//! Every grammar, every viewport, against text that is not ASCII.
//!
//! # Why this exists
//!
//! The 2026-08-14 crash was a byte-vs-char slice in the Blade scanner, reached from
//! `render_rows` — so merely *showing* an accented template killed the window. It survived
//! 1600 passing tests because every fixture in this crate was written in ASCII by people
//! typing in ASCII, and because the second half of it only appeared once a padded viewport
//! straddled a character partway down a file longer than the padding.
//!
//! Blade was fixed. The question this file answers is whether the other nine grammars have
//! the same hole, and it asks it the way the bug was actually found: real multi-byte text,
//! every scroll position, checking that the spans coming back can be used to slice the
//! source without panicking.
//!
//! Highlighting is *rendering*, so anything that fails here is a crash a user reaches by
//! opening a file — the highest-severity shape this codebase has.

use elle_syntax::{ALL_LANGUAGES, Language, SyntaxTree};
use elle_text::Buffer;

/// Multi-byte text in each language's own syntax, so the scanner reaches its interesting
/// paths (strings, comments, identifiers) with characters that are not one byte wide.
///
/// The characters are chosen to cover the ways UTF-8 goes wrong: two-byte Latin accents,
/// three-byte CJK, four-byte emoji, a ZWJ sequence that is several characters rendering as
/// one, a combining mark whose boundary is not where it looks, and an RTL run.
fn sample_for(language: Language) -> String {
    let body = match language {
        Language::Php => {
            "<?php\n// comentário com acento\n$café = 'ação';\n$日本 = \"テキスト\";\n// مرحبا\n"
        }
        Language::Blade => {
            "<div class=\"caixa\">Configuração {{ $índice }}</div>\n{{-- comentário --}}\n"
        }
        Language::Json => "{\n  \"título\": \"ação\",\n  \"日本語\": \"テキスト\",\n  \"emoji\": \"👨‍👩‍👧‍👦\"\n}\n",
        Language::JavaScript => {
            "// comentário\nconst café = 'ação';\nconst 日本 = `テキスト ${café}`;\n"
        }
        Language::TypeScript => {
            "// comentário\nconst café: string = 'ação';\ninterface Configuração {日本: string }\n"
        }
        Language::Css => ".caixa-ação {\n  /* comentário */\n  content: \"日本語\";\n}\n",
        Language::Html => {
            "<!-- comentário -->\n<div title=\"ação\">Configuração 日本語 👨‍👩‍👧‍👦</div>\n"
        }
        Language::Toml => "# comentário\ntítulo = \"ação\"\n日本 = \"テキスト\"\n",
        Language::Yaml => "# comentário\ntítulo: ação\n日本: テキスト\nemoji: 👨‍👩‍👧‍👦\n",
        Language::Shell => "# comentário\nAPP_NAME=\"Configuração\"\nDB_SENHA='ação'\n",
        Language::Rust => {
            "// comentário\nfn ação() -> &'static str { \"日本語\" }\nconst E\u{0301}: u8 = 1;\n"
        }
        Language::Markdown => {
            "# Configuração\n\nUm parágrafo com ação e 日本語.\n\n```php\n$café = 1;\n```\n"
        }
        Language::PlainText => "Configuração, ação, 日本語, 👨‍👩‍👧‍👦, e\u{0301}poca, مرحبا\n",
    };

    // Repeated so the file is comfortably longer than any internal padding — the second
    // Blade bug was invisible in short inputs, and a fixture that fits inside the padding
    // would hide the same class of bug here.
    body.repeat(40)
}

#[test]
fn every_grammar_survives_multibyte_text_at_every_viewport() {
    for language in ALL_LANGUAGES {
        let source = sample_for(language);
        let buffer = Buffer::new(&source);
        let Ok(tree) = SyntaxTree::new(language, &buffer) else {
            panic!("{} must load its grammar", language.name());
        };

        let len = source.len();
        // The whole file, then a window walked across it. A prime stride so the window
        // lands on varied offsets rather than marching in step with the repeated body.
        let mut viewports = vec![0..len];
        let mut at = 0;
        while at < len {
            viewports.push(at..(at + 400).min(len));
            at += 137;
        }

        for range in viewports {
            let spans = tree.highlights(&buffer, range.clone());
            for span in &spans {
                // The exact property whose absence crashed the renderer: a span is used to
                // slice the line being painted, so a range splitting a character panics
                // there rather than here.
                assert!(
                    source.is_char_boundary(span.range.start)
                        && source.is_char_boundary(span.range.end),
                    "{}: span {:?} splits a character (viewport {range:?})",
                    language.name(),
                    span.range,
                );
                assert!(
                    span.range.end <= len,
                    "{}: span {:?} runs past the end of a {len}-byte document",
                    language.name(),
                    span.range,
                );
            }
        }
    }
}
