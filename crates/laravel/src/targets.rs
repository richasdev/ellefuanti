//! Turning a [`Reference`] into a place on disk.
//!
//! Split from the reader in `references.rs` because only this half touches the filesystem.
//! Everything here answers the same question — *does this exist, and where* — and answers
//! it by looking, never by inferring. A path that does not exist yields `None`; it is never
//! returned on the theory that Laravel's conventions say it should be there.
//!
//! That asymmetry is the whole design. **A `None` here means "we could not find it", never
//! "it does not exist"** — the two are different claims and only the first is one we can
//! make. Laravel resolves views through a `ViewFinder` with configurable paths, components
//! through registered namespaces and `Blade::component()` calls, and routes through
//! anything a service provider ran. So a caller may use `None` to stay silent, and must not
//! use it to tell the user something is missing (RISKS.md #4).

use std::path::{Path, PathBuf};

use crate::references::{Reference, ReferenceKind};
use crate::routes::extract_routes;

/// Where a reference points: a file, and a line within it when we know one.
///
/// `line` is 1-based, matching [`crate::Route::line`] and what an editor shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    pub line: Option<usize>,
}

/// Resolves a reference against a project root, reading the filesystem.
///
/// Blocking, like the rest of this crate — the caller runs it on the background executor
/// (ADR-0007). The cost is a handful of `exists()` calls for a view or component, and for a
/// route, parsing `routes/*.php`. Nothing is cached here: caching a *click* would be
/// solving a problem nobody has, and the caller already runs this off the UI thread.
pub fn resolve(root: &Path, reference: &Reference) -> Option<Target> {
    match reference.kind {
        ReferenceKind::Route => route_target(root, &reference.name),
        ReferenceKind::Config => config_target(root, &reference.name),
        ReferenceKind::View => view_target(root, &reference.name).map(file),
        ReferenceKind::Component => component_target(root, &reference.name).map(file),
    }
}

fn file(path: PathBuf) -> Target {
    Target { path, line: None }
}

/// Every route name this project statically declares, for completion.
///
/// Deliberately returns only `Known` names: a completion list may be incomplete without
/// harming anyone, and offering `<$legacyName>` as something to type would not be a
/// completion at all. The list is sorted and deduplicated so two files declaring the same
/// name do not produce two identical rows.
pub fn route_names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = route_files(root)
        .into_iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .flat_map(|source| {
            extract_routes(&source)
                .routes
                .into_iter()
                .filter_map(|route| route.name?.known().cloned())
                .collect::<Vec<_>>()
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The `routes/*.php` files, in a stable order.
fn route_files(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root.join("routes")) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "php"))
        .collect();
    files.sort();
    files
}

/// The declaration of a named route, by parsing `routes/*.php`.
///
/// The same extractor the route palette uses, so a route the palette can jump to is exactly
/// a route a ⌘click can jump to — two answers to "where is this route" that could disagree
/// is a bug waiting to be reported as "sometimes it works".
fn route_target(root: &Path, name: &str) -> Option<Target> {
    for path in route_files(root) {
        let Ok(source) = std::fs::read_to_string(&path) else { continue };
        for route in extract_routes(&source).routes {
            if route.name.as_ref().and_then(|n| n.known()).is_some_and(|n| n == name) {
                return Some(Target { path, line: Some(route.line) });
            }
        }
    }
    None
}

/// `config('app.name')` → `config/app.php`, at the line declaring `name` when we find it.
///
/// The first segment is the file; the rest is a path through nested arrays. Only the file
/// has to resolve — landing at the top of `config/app.php` is a useful jump even when the
/// key is nested somewhere this does not follow.
fn config_target(root: &Path, key: &str) -> Option<Target> {
    let (file_name, rest) = key.split_once('.').unwrap_or((key, ""));
    // Reject a segment that is not a plain file name before joining it: `config('../x')`
    // is not a config key, and `join` would happily walk out of the project.
    if file_name.is_empty()
        || !file_name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let path = root.join("config").join(format!("{file_name}.php"));
    if !path.is_file() {
        return None;
    }
    let line = rest.split('.').next().filter(|s| !s.is_empty()).and_then(|first| {
        let source = std::fs::read_to_string(&path).ok()?;
        key_line(&source, first)
    });
    Some(Target { path, line })
}

/// The 1-based line declaring `'key' =>` in a config file.
///
/// A text scan, not a parse. It can be defeated — a key inside a nested array with the same
/// name as the one wanted wins if it comes first — and that is acceptable because the
/// fallback is landing on a different line of *the right file*, not the wrong file. What it
/// must not do is match a key inside a comment, hence skipping `//` and `#` lines.
///
/// A parse would be more precise and needs the tree plus array-shape traversal; the payoff
/// over "right file, roughly right line" did not justify it (§21: no work without a reason).
fn key_line(source: &str, key: &str) -> Option<usize> {
    let single = format!("'{key}'");
    let double = format!("\"{key}\"");
    source.lines().enumerate().find_map(|(index, line)| {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
            return None;
        }
        let start = trimmed.find(&single).or_else(|| trimmed.find(&double))?;
        // `=>` after the literal is what makes it a key rather than a value.
        trimmed[start..].contains("=>").then_some(index + 1)
    })
}

/// `view('partials.header')` → `resources/views/partials/header.blade.php`.
///
/// Both extensions are tried because Laravel's default finder registers `.blade.php` and
/// plain `.php` templates. A dotted name is a directory path — that is Laravel's convention
/// and not an inference.
fn view_target(root: &Path, name: &str) -> Option<PathBuf> {
    let relative = dotted_to_path(name)?;
    let views = root.join("resources").join("views");
    ["blade.php", "php"].into_iter().find_map(|extension| {
        let path = views.join(format!("{relative}.{extension}"));
        path.is_file().then_some(path)
    })
}

/// `<x-forms.input>` → the component template, checking both places Laravel looks.
///
/// Two directories, in Laravel's own order: `components/` for an anonymous component, then
/// the `views/` root for one whose class names a view outside that tree. A namespaced tag
/// (`<x-mail::button>`) is not resolved — the namespace maps through a registration this
/// cannot see, and guessing a directory for it would be exactly the wrong kind of help.
fn component_target(root: &Path, name: &str) -> Option<PathBuf> {
    if name.contains(':') {
        return None;
    }
    let relative = dotted_to_path(name)?;
    let views = root.join("resources").join("views");
    [views.join("components"), views].into_iter().find_map(|directory| {
        // `<x-forms.input>` may be `forms/input.blade.php` or `forms/input/index.blade.php`
        // — Laravel accepts both, and `index` is how a component with slots is usually laid
        // out.
        [format!("{relative}.blade.php"), format!("{relative}/index.blade.php")]
            .into_iter()
            .map(|suffix| directory.join(suffix))
            .find(|path| path.is_file())
    })
}

/// `forms.input` → `forms/input`, refusing anything that could escape the views directory.
///
/// The traversal check is not paranoia about a hostile file: it is that `view($x)` with a
/// user-controlled `$x` is a real Laravel bug class, and an editor that helpfully opens
/// `/etc/passwd` because a template name said `../../../../etc/passwd` is doing the wrong
/// favour. Segments are also required to be non-empty, so `a..b` resolves to nothing rather
/// than to `a/b`.
fn dotted_to_path(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let segments: Vec<&str> = name.split('.').collect();
    if segments.iter().any(|s| s.is_empty() || *s == ".." || s.contains('/') || s.contains('\\')) {
        return None;
    }
    Some(segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dotted_name_becomes_a_directory_path() {
        assert_eq!(dotted_to_path("partials.header").as_deref(), Some("partials/header"));
        assert_eq!(dotted_to_path("welcome").as_deref(), Some("welcome"));
    }

    /// `view($userInput)` is a real Laravel bug class, and an editor that follows it out of
    /// the project is amplifying the bug rather than showing it.
    #[test]
    fn a_name_that_could_escape_the_views_directory_resolves_to_nothing() {
        assert_eq!(dotted_to_path("..header"), None);
        assert_eq!(dotted_to_path("a..b"), None);
        assert_eq!(dotted_to_path("../../etc/passwd"), None);
        assert_eq!(dotted_to_path(""), None);
    }

    #[test]
    fn a_config_key_is_found_by_its_arrow() {
        let source = "<?php\nreturn [\n    'name' => env('APP_NAME'),\n    'debug' => false,\n];\n";
        assert_eq!(key_line(source, "name"), Some(3));
        assert_eq!(key_line(source, "debug"), Some(4));
        assert_eq!(key_line(source, "missing"), None);
    }

    #[test]
    fn a_commented_key_is_not_where_the_key_is_declared() {
        let source = "<?php\nreturn [\n    // 'name' => 'old',\n    'name' => 'new',\n];\n";
        assert_eq!(key_line(source, "name"), Some(4));
    }

    /// A key appearing as a *value* is not a declaration of it.
    #[test]
    fn a_string_without_an_arrow_is_not_a_key() {
        let source = "<?php\nreturn [\n    'driver' => 'name',\n];\n";
        assert_eq!(key_line(source, "name"), None);
    }
}
