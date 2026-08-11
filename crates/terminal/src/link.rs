//! Finding the thing under a ⌘-click: a file path, maybe with a line, or a URL.
//!
//! # Why this is text scanning and not a regex
//!
//! The crate has no regex dependency and one pattern to find. A stack trace names files as
//! `app/Models/User.php:42`, a Laravel error page as `/srv/app/routes/web.php:12:3`, and a
//! shell prompt wraps them in whatever it likes — quotes, parentheses, trailing commas.
//! Splitting on whitespace and trimming punctuation covers all of those in ~40 lines; a
//! regex would cover the same cases behind a new dependency and a pattern nobody can read
//! at a glance.
//!
//! # What this deliberately does not decide
//!
//! Whether the path exists. This module sees one line of terminal text and answers "what
//! did the user click on", never "is that a real file" — the caller holds the filesystem
//! and the working directory, and only it can check without lying (RISKS.md #4: `None`
//! means "we could not find it", and a link that opens nothing is handled by the caller
//! declining to open, not by this module guessing).

/// What sat under the click, if anything link-shaped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// A path-shaped token, with the `:line[:column]` suffix parsed off if present.
    /// The path may be relative — resolving it against a working directory is the
    /// caller's job, because only the caller knows one.
    Path { path: String, line: Option<u32> },
    /// An `http://` or `https://` URL, complete as written.
    Url(String),
}

/// The link under character `column` of one row of terminal text, if any.
///
/// `column` is a character index — the same unit as a grid cell — not a byte offset,
/// because that is what the mouse hit-test produces and rows can hold multibyte output.
pub fn link_at(row: &str, column: usize) -> Option<Link> {
    let chars: Vec<char> = row.chars().collect();
    if column >= chars.len() || chars[column].is_whitespace() {
        return None;
    }

    // The whitespace-delimited word containing the click.
    let start = chars[..column].iter().rposition(|c| c.is_whitespace()).map_or(0, |i| i + 1);
    let end =
        chars[column..].iter().position(|c| c.is_whitespace()).map_or(chars.len(), |i| column + i);
    let word: String = chars[start..end].iter().collect();

    // Wrapping punctuation is the prompt's, not the token's: `(app/User.php:10)`,
    // `"web.php",` and `<file.php>` all name the same file. Looped to a fixed point
    // because the layers nest — `"web.php",` carries a comma *outside* the quote, and a
    // single pass of each trim leaves whichever layer was inside the other.
    let mut token = word.as_str();
    loop {
        let trimmed = token
            .trim_matches(|c: char| {
                matches!(c, '(' | ')' | '[' | ']' | '<' | '>' | '"' | '\'' | '`')
            })
            .trim_end_matches([',', ';', '.', ':']);
        if trimmed == token {
            break;
        }
        token = trimmed;
    }
    if token.is_empty() {
        return None;
    }

    if token.starts_with("http://") || token.starts_with("https://") {
        return Some(Link::Url(token.to_string()));
    }

    // `path:line[:column]` — the trailing numbers split off, the column half discarded
    // because the editor's jump takes a line and nobody reads terminal columns.
    let (path, line) = split_line_suffix(token);

    // A bare word is not a path. Requiring a separator or an extension dot is what keeps
    // ⌘-clicking the word `error` from being answered with a file called `error` — the
    // caller would refuse it anyway when it does not exist, but a `None` here means the
    // click can fall through to selection instead of dying on a failed lookup.
    if !path.contains('/') && !path.contains('.') {
        return None;
    }
    // Purely numeric tokens like `2.5` or version-shaped `1.2.3` are not paths either.
    if path.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '/') {
        return None;
    }

    Some(Link::Path { path: path.to_string(), line })
}

/// Splits `path:12:3` into (`path`, `Some(12)`); `path` alone comes back unchanged.
fn split_line_suffix(token: &str) -> (&str, Option<u32>) {
    let mut path = token;
    let mut numbers: Vec<u32> = Vec::new();

    // At most two numeric suffixes — line and column. A third `:123` is part of the name.
    for _ in 0..2 {
        let Some(colon) = path.rfind(':') else { break };
        let suffix = &path[colon + 1..];
        if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        let Ok(number) = suffix.parse() else { break };
        numbers.push(number);
        path = &path[..colon];
    }

    // The *first* number after the path is the line: `web.php:12:3` parsed right-to-left
    // pushed [3, 12], and 12 is the one the user means.
    (path, numbers.last().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(p: &str, line: Option<u32>) -> Option<Link> {
        Some(Link::Path { path: p.to_string(), line })
    }

    #[test]
    fn a_stack_trace_path_with_a_line_parses() {
        let row = "  at app/Models/User.php:42";
        // Anywhere on the token finds it, not just its first character.
        assert_eq!(link_at(row, 8), path("app/Models/User.php", Some(42)));
        assert_eq!(link_at(row, 25), path("app/Models/User.php", Some(42)));
    }

    #[test]
    fn line_and_column_keep_the_line_and_drop_the_column() {
        // Laravel and PHPUnit both print `path:line:column`; the jump takes a line.
        assert_eq!(link_at("/srv/routes/web.php:12:3", 5), path("/srv/routes/web.php", Some(12)));
    }

    #[test]
    fn prompt_punctuation_is_not_part_of_the_name() {
        assert_eq!(link_at("(app/User.php:10)", 3), path("app/User.php", Some(10)));
        assert_eq!(link_at("\"web.php\",", 3), path("web.php", None));
        assert_eq!(link_at("see app/web.php.", 6), path("app/web.php", None));
    }

    #[test]
    fn urls_are_urls_not_paths() {
        assert_eq!(
            link_at("open https://laravel.com/docs now", 10),
            Some(Link::Url("https://laravel.com/docs".to_string()))
        );
        // The URL's own colon-number shape must not be parsed as a line suffix.
        assert_eq!(
            link_at("http://localhost:8000", 3),
            Some(Link::Url("http://localhost:8000".to_string()))
        );
    }

    #[test]
    fn plain_words_and_numbers_are_nothing() {
        // A bare word is not a path — the click falls through to selection.
        assert_eq!(link_at("error in test", 2), None);
        // Version-shaped tokens are not files.
        assert_eq!(link_at("php 8.2.1 ready", 5), None);
        // Clicking whitespace is clicking nothing.
        assert_eq!(link_at("a  b", 1), None);
        // Past the end of the row.
        assert_eq!(link_at("ab", 10), None);
    }

    #[test]
    fn multibyte_rows_use_character_columns() {
        // The grid is cell-addressed and cells are characters; a byte-indexed scan would
        // split the `ã` and panic or mis-span.
        let row = "ação app/União.php:7 fim";
        assert_eq!(link_at(row, 8), path("app/União.php", Some(7)));
    }

    #[test]
    fn a_windows_looking_token_with_one_colon_still_splits() {
        // `artisan:42` — no slash, but the dot rule already rejected bare words; with a
        // dot it is a candidate and the caller's exists() check has the final word.
        assert_eq!(link_at("web.php:42", 0), path("web.php", Some(42)));
    }
}
