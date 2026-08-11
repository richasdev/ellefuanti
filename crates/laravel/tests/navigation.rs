//! Resolving a Laravel reference against a real project tree on disk.
//!
//! A real tree, in a tempdir, because resolution *is* the `is_file` check: a test against a
//! root that does not exist would pass on a resolver that only joins strings, which is the
//! one implementation that must not pass.
//!
//! The reading half is unit-tested in `src/references.rs` where it can be exercised without
//! any filesystem at all. What is here is the pairing of the two — click here, land there.

use std::fs;
use std::path::{Path, PathBuf};

use elle_laravel::{ReferenceKind, reference_at, resolve, route_names};

/// A project with the layout the resolvers expect, and nothing else.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a tempdir");
    let root = dir.path();

    write(
        root,
        "routes/web.php",
        r#"<?php
use Illuminate\Support\Facades\Route;

Route::get('/users', [UserController::class, 'index'])->name('users.index');
Route::get('/users/{user}', [UserController::class, 'show'])->name('users.show');
Route::get('/dynamic', [UserController::class, 'go'])->name($runtimeName);
"#,
    );
    write(
        root,
        "config/app.php",
        "<?php\n\nreturn [\n    'name' => env('APP_NAME'),\n    'timezone' => 'UTC',\n];\n",
    );
    write(root, "resources/views/welcome.blade.php", "<h1>Hi</h1>\n");
    write(root, "resources/views/partials/header.blade.php", "<header></header>\n");
    write(root, "resources/views/components/forms/input.blade.php", "<input>\n");
    // A component laid out as a directory, which Laravel also accepts.
    write(root, "resources/views/components/card/index.blade.php", "<div></div>\n");
    // A plain-PHP template, the finder's second extension.
    write(root, "resources/views/legacy/report.php", "<?php echo 'x';\n");

    dir
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).expect("a parent directory");
    fs::write(&path, contents).expect("writing a fixture");
}

/// Resolves the reference at the first occurrence of `needle` in `source`.
fn go_to(root: &Path, source: &str, needle: &str, blade: bool) -> Option<(PathBuf, Option<usize>)> {
    let offset = source.find(needle).unwrap_or_else(|| panic!("{needle:?} not in source"));
    let reference = reference_at(source, offset, blade)?;
    // The current-file argument matters only to wire references; these tests navigate
    // from project-level constructs, so any in-project path serves.
    resolve(root, &root.join("resources/views/any.blade.php"), &reference)
        .map(|target| (target.path, target.line))
}

#[test]
fn a_route_name_lands_on_the_line_that_declares_it() {
    let dir = project();
    let source = "<?php\n$url = route('users.show');\n";

    let (path, line) = go_to(dir.path(), source, "users.show", false).expect("the route");
    assert!(path.ends_with("routes/web.php"), "landed on {}", path.display());
    // The `->name('users.show')` line, 1-based, exactly as the route palette reports it.
    assert_eq!(line, Some(5));
}

#[test]
fn a_config_key_lands_on_the_key_inside_its_file() {
    let dir = project();
    let source = "<?php\nconfig('app.timezone');\n";

    let (path, line) = go_to(dir.path(), source, "app.timezone", false).expect("the key");
    assert!(path.ends_with("config/app.php"), "landed on {}", path.display());
    assert_eq!(line, Some(5));
}

/// The file resolves even when the nested key does not. Landing at the top of the right
/// file is a useful jump; refusing to move because a nested lookup failed is not.
#[test]
fn a_config_key_we_cannot_locate_still_opens_the_right_file() {
    let dir = project();
    let source = "<?php\nconfig('app.providers.aliases');\n";

    let (path, line) = go_to(dir.path(), source, "app.providers", false).expect("the file");
    assert!(path.ends_with("config/app.php"));
    assert_eq!(line, None, "no line is honest; a guessed one would not be");
}

#[test]
fn a_blade_include_lands_on_the_template() {
    let dir = project();
    let source = "@include('partials.header')\n";

    let (path, _) = go_to(dir.path(), source, "partials.header", true).expect("the partial");
    assert!(path.ends_with("resources/views/partials/header.blade.php"));
}

#[test]
fn a_component_tag_lands_on_its_template_in_either_layout() {
    let dir = project();

    let flat = "<x-forms.input name=\"a\" />\n";
    let (path, _) = go_to(dir.path(), flat, "forms.input", true).expect("the component");
    assert!(path.ends_with("resources/views/components/forms/input.blade.php"));

    // `<x-card>` as a directory with an `index.blade.php`.
    let nested = "<x-card>body</x-card>\n";
    let (path, _) = go_to(dir.path(), nested, "card", true).expect("the component");
    assert!(path.ends_with("resources/views/components/card/index.blade.php"));
}

#[test]
fn a_plain_php_template_resolves_as_well_as_a_blade_one() {
    let dir = project();
    let source = "<?php\nview('legacy.report');\n";

    let (path, _) = go_to(dir.path(), source, "legacy.report", false).expect("the template");
    assert!(path.ends_with("resources/views/legacy/report.php"));
}

/// A route helper written inside a template, found by the scanner rather than a tree.
#[test]
fn a_route_helper_inside_blade_resolves_the_same_as_in_php() {
    let dir = project();
    let source = "<a href=\"{{ route('users.index') }}\">Users</a>\n";

    let (path, line) = go_to(dir.path(), source, "users.index", true).expect("the route");
    assert!(path.ends_with("routes/web.php"));
    assert_eq!(line, Some(4));
}

// ---------------------------------------------------------------------------
// The half that matters more: what must resolve to nothing.
// ---------------------------------------------------------------------------

/// RISKS.md #4. Each of these is a name a static reader cannot know, and every one of them
/// must produce no jump at all — not a jump to a plausible file.
///
/// This is the test to read first if the resolvers are ever changed. A diff that makes it
/// fail has bought coverage with a lie the user navigates on.
#[test]
fn a_reference_we_cannot_read_or_cannot_find_produces_no_jump() {
    let dir = project();
    let root = dir.path();

    for (source, needle, blade) in [
        // Not statically readable — the name exists only at runtime.
        ("<?php route($name);\n", "$name", false),
        ("<?php view(\"posts.{$slug}\");\n", "posts.", false),
        ("@include(\"posts.{$slug}\")\n", "posts.", true),
        // Readable, but nothing on disk answers to it. Silence, not a claim.
        ("<?php route('nope.missing');\n", "nope.missing", false),
        ("<?php config('nonexistent.key');\n", "nonexistent.key", false),
        ("<?php view('does.not.exist');\n", "does.not.exist", false),
        ("<x-not.a.component />\n", "not.a.component", true),
        // A namespaced component maps through a registration we cannot see.
        ("<x-mail::button />\n", "mail::button", true),
    ] {
        assert_eq!(
            go_to(root, source, needle, blade),
            None,
            "must not navigate anywhere for {source:?}"
        );
    }
}

/// `view($userInput)` is a real Laravel bug class. An editor that follows a traversal out of
/// the project is amplifying the bug rather than revealing it.
#[test]
fn a_template_name_cannot_escape_the_project() {
    let dir = project();
    let source = "<?php\nview('...etc.passwd');\n";
    assert_eq!(go_to(dir.path(), source, "...etc.passwd", false), None);
}

/// A route whose `->name()` is an expression is not offered for completion, because there is
/// no name to offer. Missing from the list is harmless; `<$runtimeName>` as a suggestion is
/// not a suggestion.
#[test]
fn completion_offers_only_the_names_that_were_actually_readable() {
    let dir = project();
    let names = route_names(dir.path());

    assert_eq!(names, vec!["users.index".to_string(), "users.show".to_string()]);
}

/// A folder that is not a Laravel project is the common case, and it must be quiet rather
/// than an error — most folders anyone opens are not Laravel projects.
#[test]
fn a_project_without_the_laravel_layout_resolves_nothing_and_does_not_panic() {
    let dir = tempfile::tempdir().expect("a tempdir");
    let source = "<?php\nroute('users.show');\nview('welcome');\nconfig('app.name');\n";

    for needle in ["users.show", "welcome", "app.name"] {
        assert_eq!(go_to(dir.path(), source, needle, false), None);
    }
    assert!(route_names(dir.path()).is_empty());
}

/// The reference kinds are distinguished at the point of reading, so a caller cannot get a
/// config resolver pointed at a view name.
#[test]
fn each_helper_reports_its_own_kind() {
    let source = "<?php\nroute('a');\nconfig('b');\nview('c');\n";
    let kinds: Vec<ReferenceKind> = ["'a'", "'b'", "'c'"]
        .iter()
        .map(|needle| {
            let offset = source.find(needle).unwrap() + 1;
            reference_at(source, offset, false).expect("a reference").kind
        })
        .collect();

    assert_eq!(kinds, [ReferenceKind::Route, ReferenceKind::Config, ReferenceKind::View]);
}
