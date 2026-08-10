//! Mechanical enforcement of #48's first rule: no view constructs its own theme.
//!
//! The failure this exists to prevent already happened once. `Theme::dark()` was called
//! inline in four `render` methods, so the theme was not a value the app held — it was a
//! constructor four files happened to call. Adding a second theme would have changed
//! nothing in any of them, and the bug would have presented as "the light theme mostly
//! works", which is the hardest kind to notice.
//!
//! Written as a test for the same reason as `crates/lsp/tests/substitutability.rs` and
//! `crates/app/tests/architecture.rs`: a rule stated in prose survives until the first
//! hurry. The check is crude — a constructor's *name* appearing in shipped code — and that
//! crudeness is the point, because the name appearing is exactly the failure mode.
//!
//! What it does **not** check is whether the colours are any good. Theme correctness is
//! something a person looks at, and nobody has (#35).

use std::fs;
use std::path::{Path, PathBuf};

fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else { return files };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    files
}

/// The shipped logic of a file: comments and `#[cfg(test)]` modules removed.
///
/// Both exclusions are deliberate, and the same reasoning as the LSP crate's scanner:
///
/// - **Comments** must be able to name the constructors. This module's own doc comment
///   explains the rule by naming `Theme::dark()`, and `theme.rs` documents why the light
///   theme's ANSI table is not a copy of the dark one. A check that forbade the words
///   would delete the explanation of the constraint it enforces.
/// - **Test modules** must be able to build a concrete theme. `row_runs` and `line_runs`
///   take a `&Theme`; testing them against a fixture is not a view reaching for a colour,
///   and there is no context to read one from in a plain `#[test]`.
///
/// Deliberately naive parsing: line comments and a brace-counted `mod tests` are the only
/// forms this crate uses.
///
/// ponytail: if this ever needs block comments or a nested test module, parse with `syn`.
/// Today that is a dependency and 50 lines to answer what a grep already answers.
fn shipped_code(text: &str) -> String {
    let without_comments: Vec<&str> = text
        .lines()
        .map(|line| match line.find("//") {
            Some(position) => &line[..position],
            None => line,
        })
        .collect();

    let mut kept = Vec::new();
    let mut test_module_depth: Option<i32> = None;

    for line in without_comments {
        match test_module_depth {
            None => {
                if line.trim_start().starts_with("mod tests") {
                    test_module_depth = Some(count_braces(line));
                    continue;
                }
                // The attribute itself sits on the line before `mod tests`.
                if line.trim() == "#[cfg(test)]" {
                    continue;
                }
                kept.push(line);
            }
            Some(depth) => {
                let depth = depth + count_braces(line);
                test_module_depth = if depth <= 0 { None } else { Some(depth) };
            }
        }
    }

    kept.join("\n")
}

fn count_braces(line: &str) -> i32 {
    line.chars().filter(|c| *c == '{').count() as i32
        - line.chars().filter(|c| *c == '}').count() as i32
}

/// A guard that fails an empty check: a scanner that stripped too much would pass the
/// assertion below while detecting nothing, which is the one way this file could give
/// false confidence.
#[test]
fn the_scanner_keeps_real_code_and_drops_only_tests_and_comments() {
    let source = r#"
// Theme::dark() is discussed here
/// and here, in a doc comment
fn real_logic() {
    let theme = cx.theme();
}

#[cfg(test)]
mod tests {
    #[test]
    fn uses_a_theme_as_a_fixture() {
        let theme = Theme::dark();
        if true { let other = Theme::light(); }
    }
}
"#;

    let shipped = shipped_code(source);
    assert!(shipped.contains("fn real_logic"), "real code must survive: {shipped:?}");
    assert!(shipped.contains("cx.theme()"), "real calls must survive: {shipped:?}");
    assert!(
        !shipped.contains("Theme::dark()"),
        "comments and test modules must be stripped: {shipped:?}"
    );
    assert!(
        !shipped.contains("Theme::light()"),
        "nested braces inside a test module must stay stripped: {shipped:?}"
    );

    // And the check genuinely fires on a violation in shipped code.
    let violating = "fn render() { let theme = Theme::dark(); }";
    assert!(shipped_code(violating).contains("Theme::dark()"));
}

#[test]
fn no_view_constructs_its_own_theme() {
    // The constructors, plus the escape hatch someone would reach for next. `build` is on
    // `ThemeVariant` and is how `set_theme` makes a `Theme`; a view calling it would be
    // constructing a theme by another name.
    const CONSTRUCTORS: [&str; 3] = ["Theme::dark()", "Theme::light()", ".build()"];

    let mut violations = Vec::new();

    for file in rust_files(&src_dir()) {
        // theme.rs is where the constructors are defined and where `set_theme` calls
        // `build`. Excluding it is not a loophole: the rule is that *views* do not reach
        // for a theme, and the module that owns the theme is the one place that must.
        if file.file_name().is_some_and(|name| name == "theme.rs") {
            continue;
        }
        let code = shipped_code(&fs::read_to_string(&file).unwrap());
        for constructor in CONSTRUCTORS {
            if code.contains(constructor) {
                violations.push(format!("{} calls {constructor}", file.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "#48: there is one theme and it is read from the context, not built where it is \
         needed. Found: {violations:?}. Use `cx.theme()` (the `Themed` trait) instead — a \
         constructor call here is a surface a theme switch would never reach."
    );
}
