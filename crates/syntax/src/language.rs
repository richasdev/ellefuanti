//! Language detection and grammar loading.

use std::path::Path;

use tree_sitter::Language as TsLanguage;

/// A language the editor can parse.
///
/// PHP and Blade are hand-mapped in `highlight.rs`; everything below them is driven by a
/// `highlights.scm` query (see [`Language::highlight_query`]). Adding a language is
/// therefore a grammar dependency, a variant, an extension, and a query file — no new
/// Rust match arms over node kinds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Language {
    Php,
    /// `*.blade.php`. Uses the PHP grammar (which parses interleaved HTML and `<?php`
    /// regions natively) plus a Blade directive scanner layered on top.
    ///
    /// ponytail: a real Blade grammar with tree-sitter injections is the correct
    /// long-term answer and is what §8's "tratamento especializado" ultimately means.
    /// Upgrade when Blade-specific navigation (component tags, slots) needs a real
    /// parse tree rather than highlight spans. Tracked in ADR-0006.
    Blade,
    Json,
    JavaScript,
    TypeScript,
    Css,
    /// Recognised extension with no grammar wired up yet: renders as plain text.
    PlainText,
}

impl Language {
    pub fn name(&self) -> &'static str {
        match self {
            Language::Php => "PHP",
            Language::Blade => "Blade",
            Language::Json => "JSON",
            Language::JavaScript => "JavaScript",
            Language::TypeScript => "TypeScript",
            Language::Css => "CSS",
            Language::PlainText => "Plain Text",
        }
    }

    /// The tree-sitter grammar, or `None` for plain text.
    pub fn grammar(&self) -> Option<TsLanguage> {
        match self {
            // LANGUAGE_PHP (not PHP_ONLY) so text outside `<?php` tags parses as HTML
            // text nodes instead of erroring — required for both Blade and plain
            // templates that mix markup with PHP.
            Language::Php | Language::Blade => Some(tree_sitter_php::LANGUAGE_PHP.into()),
            Language::Json => Some(tree_sitter_json::LANGUAGE.into()),
            Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
            Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            Language::Css => Some(tree_sitter_css::LANGUAGE.into()),
            Language::PlainText => None,
        }
    }

    /// The `highlights.scm` source for this language, or `None` if it is highlighted by
    /// hand instead (PHP and Blade) or not at all (plain text).
    ///
    /// Embedded with `include_str!` rather than read from `assets/` at runtime: the
    /// queries are compiled into a `Query` once per language and cached, and a missing
    /// or malformed asset file would be a startup failure with no colour and no obvious
    /// cause. A query that does not compile is a build-time problem here, and the
    /// `every_query_compiles` test is what turns it into one.
    ///
    /// TypeScript is JavaScript's query plus its own: upstream ships the TS file as an
    /// extension that assumes the JS patterns precede it. Concatenating is what upstream
    /// means, and it keeps the shared half in exactly one file.
    pub fn highlight_query(&self) -> Option<&'static str> {
        match self {
            Language::Json => Some(include_str!("../queries/json.scm")),
            Language::JavaScript => Some(include_str!("../queries/javascript.scm")),
            Language::TypeScript => Some(concat!(
                include_str!("../queries/javascript.scm"),
                include_str!("../queries/typescript.scm")
            )),
            Language::Css => Some(include_str!("../queries/css.scm")),
            Language::Php | Language::Blade | Language::PlainText => None,
        }
    }

    /// Whether Blade directives (`@if`, `{{ $x }}`) should be highlighted.
    pub fn has_blade_directives(&self) -> bool {
        matches!(self, Language::Blade)
    }
}

/// Every language, listed once.
///
/// Exists so tests can assert a property holds for *all* of them rather than for
/// whichever ones someone remembered — `viewport_cost_does_not_grow_with_file_size` is
/// the one that matters, and it was PHP-only before this list existed.
///
/// [`Language::name`] is what keeps it complete: an exhaustive match, so a new variant
/// is a compile error pointing at the function, and the length assertion in
/// `the_list_covers_every_language` catches the half the compiler cannot.
pub const ALL_LANGUAGES: [Language; 7] = [
    Language::Php,
    Language::Blade,
    Language::Json,
    Language::JavaScript,
    Language::TypeScript,
    Language::Css,
    Language::PlainText,
];

/// Picks a language from a file path.
///
/// Blade is checked before PHP because `.blade.php` also ends in `.php`.
pub fn language_for_path(path: &Path) -> Language {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_ascii_lowercase();

    if name.ends_with(".blade.php") {
        return Language::Blade;
    }
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("php" | "phtml") => Language::Php,
        // `.jsonc` and `.json5` are deliberately absent: the JSON grammar rejects the
        // comments and trailing commas that are the entire reason those extensions
        // exist, so they would parse as errors rather than fall back to plain text.
        Some("json") => Language::Json,
        // `.cjs`/`.mjs` are the same grammar; `.jsx` parses too, though this grammar is
        // built without the JSX highlight patterns.
        Some("js" | "cjs" | "mjs" | "jsx") => Language::JavaScript,
        // `.tsx` is NOT here: it needs LANGUAGE_TSX, a separate grammar, and parsing it
        // with the TypeScript one silently mangles every JSX tag.
        Some("ts" | "cts" | "mts") => Language::TypeScript,
        // `.scss`/`.less` are supersets this grammar does not accept — nesting and `$vars`
        // parse as errors. Plain text is the honest answer until a SCSS grammar lands.
        Some("css") => Language::Css,
        _ => Language::PlainText,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detects_blade_before_php() {
        assert_eq!(language_for_path(&PathBuf::from("a/show.blade.php")), Language::Blade);
        assert_eq!(language_for_path(&PathBuf::from("a/User.php")), Language::Php);
        assert_eq!(language_for_path(&PathBuf::from("a/old.phtml")), Language::Php);
        assert_eq!(language_for_path(&PathBuf::from("README.md")), Language::PlainText);
        assert_eq!(language_for_path(&PathBuf::from("Makefile")), Language::PlainText);
    }

    #[test]
    fn detection_is_case_insensitive() {
        assert_eq!(language_for_path(&PathBuf::from("Show.Blade.PHP")), Language::Blade);
    }

    #[test]
    fn php_grammar_loads() {
        assert!(Language::Php.grammar().is_some());
        assert!(Language::PlainText.grammar().is_none());
    }

    #[test]
    fn detects_the_languages_a_laravel_project_is_full_of() {
        // The files #53 was reported against: everything in a Laravel project that is not
        // PHP and used to open with no colour at all.
        let cases = [
            ("composer.json", Language::Json),
            ("package.json", Language::Json),
            ("app.js", Language::JavaScript),
            ("vite.config.mjs", Language::JavaScript),
            ("bootstrap.cjs", Language::JavaScript),
            ("Component.jsx", Language::JavaScript),
            ("app.ts", Language::TypeScript),
            ("types.d.ts", Language::TypeScript),
            ("app.css", Language::Css),
        ];
        for (name, want) in cases {
            assert_eq!(language_for_path(&PathBuf::from(name)), want, "{name}");
        }
    }

    #[test]
    fn extensions_whose_grammar_would_be_wrong_stay_plain_text() {
        // Each of these *looks* like a language that just landed, and mapping it to the
        // nearest grammar would parse it into an error tree — which renders worse than no
        // colour, because a broken parse also breaks the styles around it. Priority 1 of
        // #53 stops here on purpose; the rest is follow-up work, not an oversight.
        for name in [
            "tsconfig.jsonc",  // comments and trailing commas; the JSON grammar rejects both
            "data.json5",      // same
            "App.tsx",         // needs LANGUAGE_TSX, not LANGUAGE_TYPESCRIPT
            "app.scss",        // nesting and $vars are not CSS
            "theme.less",      // likewise
            "docker-compose.yml",
            "README.md",
            "schema.sql",
            "Cargo.toml",
            "main.rs",
            ".env",
        ] {
            assert_eq!(language_for_path(&PathBuf::from(name)), Language::PlainText, "{name}");
        }
    }

    #[test]
    fn the_list_covers_every_language() {
        // `name()` is an exhaustive match, so the compiler catches a new variant that is
        // never named. What it cannot catch is a new variant missing from ALL_LANGUAGES,
        // which would silently shrink the coverage of every test that iterates it —
        // including the viewport-cost one, which is the expensive property to lose.
        assert_eq!(ALL_LANGUAGES.len(), 7, "a new Language needs adding to ALL_LANGUAGES");
        for (i, a) in ALL_LANGUAGES.iter().enumerate() {
            assert!(!ALL_LANGUAGES[i + 1..].contains(a), "{} is listed twice", a.name());
        }
    }

    #[test]
    fn every_language_either_has_a_grammar_or_is_plain_text() {
        for language in ALL_LANGUAGES {
            match language {
                Language::PlainText => {
                    assert!(language.grammar().is_none());
                    assert!(language.highlight_query().is_none());
                }
                // PHP and Blade are hand-mapped in highlight.rs; see the comment on
                // `SyntaxTree::highlights` for why they are not on the query path.
                Language::Php | Language::Blade => {
                    assert!(language.grammar().is_some(), "{}", language.name());
                    assert!(language.highlight_query().is_none(), "{}", language.name());
                }
                _ => {
                    assert!(language.grammar().is_some(), "{}", language.name());
                    assert!(
                        language.highlight_query().is_some(),
                        "{}: a query-highlighted language needs a query",
                        language.name()
                    );
                }
            }
        }
    }
}
