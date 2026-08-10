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
    Html,
    Toml,
    Yaml,
    /// `sh`, `bash`, and `.env`.
    ///
    /// `.env` is not shell and has no grammar of its own, but the subset Laravel writes —
    /// `KEY=value`, `#` comments, occasional quoting — is exactly what the bash grammar
    /// already parses, and `.env` files are literally sourced by shells. Reusing the
    /// grammar costs nothing; a dedicated one would be another ~200 KB for a format with
    /// two token types. What it rules out: a value containing shell metacharacters
    /// (`DB_PASSWORD=a|b`) parses as a pipeline rather than a string, so it colours oddly.
    /// That is a worse failure than plain text only if it is common, and it is not.
    ///
    /// This is the expensive one. `tree-sitter-bash` compiles to a 1.5 MB rlib — 34× TOML,
    /// 29× HTML, and most of the 1.6 MB this batch of grammars adds to the binary. Bash's
    /// grammar is genuinely that large (word expansion, here-docs and a hand-written
    /// external scanner), so it is the price of shell at all, not something to tune. It is
    /// also what makes `.env` free rather than a fifth grammar, which is part of why the
    /// reuse above is worth doing rather than merely clever.
    Shell,
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
            Language::Html => "HTML",
            Language::Toml => "TOML",
            Language::Yaml => "YAML",
            Language::Shell => "Shell",
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
            Language::Html => Some(tree_sitter_html::LANGUAGE.into()),
            Language::Toml => Some(tree_sitter_toml_ng::LANGUAGE.into()),
            Language::Yaml => Some(tree_sitter_yaml::LANGUAGE.into()),
            Language::Shell => Some(tree_sitter_bash::LANGUAGE.into()),
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
            Language::Html => Some(include_str!("../queries/html.scm")),
            Language::Toml => Some(include_str!("../queries/toml.scm")),
            Language::Yaml => Some(include_str!("../queries/yaml.scm")),
            Language::Shell => Some(include_str!("../queries/bash.scm")),
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
pub const ALL_LANGUAGES: [Language; 11] = [
    Language::Php,
    Language::Blade,
    Language::Json,
    Language::JavaScript,
    Language::TypeScript,
    Language::Css,
    Language::Html,
    Language::Toml,
    Language::Yaml,
    Language::Shell,
    Language::PlainText,
];

/// Picks a language from a file path.
///
/// Blade is checked before PHP because `.blade.php` also ends in `.php`.
///
/// Whole-name matches come before extensions because the files a Laravel project keeps at
/// its root mostly have no extension to match on: `artisan` is PHP, `.env` is shell-shaped,
/// `Dockerfile` is neither. Before this, every one of them opened as plain text — which is
/// exactly the #53 complaint, just for the files that happen to sit where you see them
/// first.
pub fn language_for_path(path: &Path) -> Language {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_ascii_lowercase();

    if name.ends_with(".blade.php") {
        return Language::Blade;
    }

    match name.as_str() {
        // Laravel's CLI entry point: a PHP file with a `#!/usr/bin/env php` shebang and no
        // extension. The PHP grammar handles the shebang (it parses as text before the
        // `<?php` tag, same as the HTML around a template).
        "artisan" => return Language::Php,
        // `.env`, `.env.example`, `.env.testing` — see the `Shell` variant for why the bash
        // grammar and not a dedicated one.
        n if n == ".env" || n.starts_with(".env.") => return Language::Shell,
        // Dotfiles that are shell by convention rather than by extension.
        ".bashrc" | ".bash_profile" | ".profile" | ".zshrc" => return Language::Shell,
        // `Dockerfile` is conspicuously absent, and it is the other obvious candidate #53
        // named. `tree-sitter-dockerfile` is at 0.2.0, last released against a tree-sitter
        // three majors back, and sits outside the tree-sitter-grammars org that maintains
        // everything else here. One file per project is not worth an unmaintained parser.
        _ => {}
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
        // `.vue` and `.svelte` are absent: they are single-file components whose `<script>`
        // and `<style>` blocks need tree-sitter injections to parse, and the HTML grammar
        // alone renders their whole body as one attribute-less text node.
        Some("html" | "htm" | "xhtml") => Language::Html,
        // `.xml` is NOT here, and phpunit.xml is the reason to want it: the HTML grammar
        // accepts XML syntactically but hard-codes HTML's void elements and raw-text
        // elements (`<script>`, `<style>`), so an XML document using those names parses
        // wrong rather than approximately. An XML grammar is the fix, not this one.
        Some("toml") => Language::Toml,
        Some("yml" | "yaml") => Language::Yaml,
        Some("sh" | "bash" | "zsh") => Language::Shell,
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
            ("welcome.html", Language::Html),
            ("docker-compose.yml", Language::Yaml),
            (".github/workflows/ci.yaml", Language::Yaml),
            ("deploy.sh", Language::Shell),
            ("rustfmt.toml", Language::Toml),
        ];
        for (name, want) in cases {
            assert_eq!(language_for_path(&PathBuf::from(name)), want, "{name}");
        }
    }

    #[test]
    fn laravel_root_files_with_no_extension_are_still_detected() {
        // The files sitting at the top of every Laravel project, none of which the
        // extension split can see. `artisan` was the specific one called out in #53's
        // follow-up: it is PHP, it is the first thing many people open, and matching on
        // `.php` misses it entirely.
        let cases = [
            ("artisan", Language::Php),
            (".env", Language::Shell),
            (".env.example", Language::Shell),
            (".env.testing", Language::Shell),
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
            "tsconfig.jsonc", // comments and trailing commas; the JSON grammar rejects both
            "data.json5",     // same
            "App.tsx",        // needs LANGUAGE_TSX, not LANGUAGE_TYPESCRIPT
            "app.scss",       // nesting and $vars are not CSS
            "theme.less",     // likewise
            "App.vue",        // a single-file component needs injections, not the HTML grammar
            "App.svelte",     // likewise
            "phpunit.xml",    // the HTML grammar hard-codes HTML's void and raw-text elements
            "Dockerfile",     // tree-sitter-dockerfile is 0.2.0 and unmaintained
            "Dockerfile.prod",
            "README.md",
            "schema.sql",
            "main.rs",
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
        assert_eq!(ALL_LANGUAGES.len(), 11, "a new Language needs adding to ALL_LANGUAGES");
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
