//! Language detection and grammar loading.

use std::path::Path;

use tree_sitter::Language as TsLanguage;

/// A language the editor can parse.
///
/// Milestone 1 ships PHP and Blade only. Adding HTML/CSS/JS/JSON/YAML/Markdown/SQL
/// (§8 of the spec) means another variant plus a grammar dependency each — deliberately
/// deferred so the first milestone proves the parse pipeline rather than 8 grammars.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// Recognised extension with no grammar wired up yet: renders as plain text.
    PlainText,
}

impl Language {
    pub fn name(&self) -> &'static str {
        match self {
            Language::Php => "PHP",
            Language::Blade => "Blade",
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
            Language::PlainText => None,
        }
    }

    /// Whether Blade directives (`@if`, `{{ $x }}`) should be highlighted.
    pub fn has_blade_directives(&self) -> bool {
        matches!(self, Language::Blade)
    }
}

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
}
