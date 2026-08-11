//! Layering enforcement.
//!
//! ADR-0004 says only `crates/app` may depend on gpui. Stated in prose, that erodes on the
//! first deadline; stated as a test, it cannot. This is the cheapest possible enforcement:
//! read the manifests, look for the dependency.

use std::fs;
use std::path::{Path, PathBuf};

fn crates_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/app.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn manifests() -> Vec<PathBuf> {
    fs::read_dir(crates_dir())
        .expect("crates/ should exist")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join("Cargo.toml"))
        .filter(|path| path.exists())
        .collect()
}

fn crate_name(manifest: &Path) -> String {
    manifest.parent().unwrap().file_name().unwrap().to_string_lossy().to_string()
}

#[test]
fn only_the_app_crate_depends_on_gpui() {
    let mut violations = Vec::new();

    for manifest in manifests() {
        let name = crate_name(&manifest);
        if name == "app" {
            continue;
        }
        let text = fs::read_to_string(&manifest).unwrap();
        // Matches both `gpui = ...` and `gpui.workspace = true`.
        if text.lines().any(|line| {
            let line = line.trim();
            line.starts_with("gpui ") || line.starts_with("gpui.") || line.starts_with("gpui=")
        }) {
            violations.push(name);
        }
    }

    assert!(
        violations.is_empty(),
        "ADR-0004: only crates/app may depend on gpui, but these do: {violations:?}. \
         Move the UI-facing code into crates/app and keep the domain layer plain Rust."
    );
}

/// RISKS.md #2, on this side of the client boundary.
///
/// `crates/lsp/tests/substitutability.rs` forbids a backend name in the LSP client's
/// logic, and scans only that crate. Wiring the client up put a backend name in *this*
/// crate for the first time — `lsp_session::DEFAULT_SERVER` is `"intelephense --stdio"` —
/// and that is allowed: somebody has to name a default, and a `ServerConfig` value is data
/// the client cannot tell apart from any other.
///
/// What is not allowed is a **branch** on it. The moment `if server == "intelephense"`
/// exists anywhere, the substitutability the whole design is arranged around is gone on
/// paper only, which is precisely the failure the risk register describes. So: the name may
/// appear once, in the constant, and nowhere else in shipped code.
#[test]
fn a_backend_is_named_only_where_the_default_is_declared() {
    const BACKENDS: [&str; 5] = ["intelephense", "phpactor", "psalm", "phpstan", "serenata"];

    let mut violations = Vec::new();

    for file in walk_rust_files(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")) {
        let text = fs::read_to_string(&file).unwrap();

        for (number, line) in shipped_code(&text).lines().enumerate() {
            let lowered = line.to_lowercase();
            if !BACKENDS.iter().any(|backend| lowered.contains(backend)) {
                continue;
            }
            // The one permitted mention: the constant that declares the default command.
            if line.contains("pub const DEFAULT_SERVER") {
                continue;
            }
            violations.push(format!("{}:{}: {}", file.display(), number + 1, line.trim()));
        }
    }

    assert!(
        violations.is_empty(),
        "RISKS.md #2: a backend may be named where the default `ServerConfig` is declared \
         and nowhere else — anything more is the start of special-casing, and the point of \
         the LSP client is that it cannot tell one server from another. Found: {violations:#?}"
    );
}

/// RISKS.md #2 again, at the new place #61 opened up.
///
/// Trigger characters used to be inert data: `Capabilities::completion_triggers` was read
/// off the handshake and nothing consulted it. #61 made it **behavioural** — it now decides
/// when a popup opens — and that creates a leak the two tests above cannot see. Neither
/// `no_php_specific_behaviour_is_baked_in` (which scans `crates/lsp`) nor
/// `a_backend_is_named_only_where_the_default_is_declared` (which looks for backend *names*)
/// would notice `if text == "->"` in `crates/app`: it names no server and lives in the crate
/// where naming one is permitted.
///
/// But it would be exactly the failure the risk register describes. `->` and `::` are PHP's
/// answer, and a phpactor or a future first-party engine declaring a different set must work
/// unchanged. The measured evidence that hardcoding is wrong is not subtle: a real
/// Intelephense declares ten **single** characters (`$ > : \ / ' " * . <`), so an
/// implementation matching the two-character `->` would never fire at all.
///
/// So: the PHP operators may not appear as literals in the completion path. They may be
/// discussed in comments — the explanation above is itself full of them — and tests may use
/// them as fixtures, which is what `shipped_code` strips.
#[test]
fn php_operators_are_not_hardcoded_where_triggers_are_decided() {
    // The files that decide when a completion popup opens. Scoped rather than crate-wide
    // because `->` is legitimate elsewhere: the Laravel crate's callers, PHP fixtures in
    // render tests, and doc examples all contain it honestly.
    const TRIGGER_PATH: [&str; 2] = ["workspace_view.rs", "completion.rs"];
    // How a hardcoded PHP trigger would actually be spelled.
    const OPERATORS: [&str; 4] = ["\"->\"", "\"::\"", "'->'", "'::'"];

    let mut violations = Vec::new();
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");

    for file in walk_rust_files(&src) {
        let name = file.file_name().unwrap().to_string_lossy().to_string();
        if !TRIGGER_PATH.contains(&name.as_str()) {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        for (number, line) in shipped_code(&text).lines().enumerate() {
            for operator in OPERATORS {
                if line.contains(operator) {
                    violations.push(format!("{}:{}: {}", file.display(), number + 1, line.trim()));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "RISKS.md #2: PHP's completion triggers must come from the server's declared \
         `completion_triggers`, never from a literal here — a different backend declares a \
         different set and must work unchanged. A real Intelephense declares single \
         characters, so a hardcoded `->` would not even fire. Found: {violations:#?}"
    );
}

/// The shipped logic of a file: comments and `#[cfg(test)]` modules removed.
///
/// Same scanner, and the same two exclusions, as `crates/lsp/tests/substitutability.rs` —
/// comments must be able to *discuss* a backend (this file's own doc comments do), and
/// tests must be able to name two real servers to show the code cannot tell them apart.
///
/// ponytail: this is now the third copy of this scanner (here, `substitutability.rs`,
/// `theming.rs`). A fourth is the point at which it should become a small shared dev
/// dependency rather than a paste; three near-identical 30-line functions is still cheaper
/// than a crate that exists to hold one of them.
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
                if line.trim_start().starts_with("mod tests")
                    || line.trim_start().starts_with("mod lsp_status_tests")
                    || line.trim_start().starts_with("mod route_label_tests")
                {
                    test_module_depth = Some(count_braces(line));
                    continue;
                }
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

#[test]
fn domain_crates_have_no_platform_conditionals() {
    // RISKS.md #8: platform-specific code stays in the app crate, so the domain layers
    // remain portable to Linux and Windows even where gpui is not.
    let mut violations = Vec::new();

    for manifest in manifests() {
        let name = crate_name(&manifest);
        if name == "app" {
            continue;
        }
        let src = manifest.parent().unwrap().join("src");
        for file in walk_rust_files(&src) {
            let text = fs::read_to_string(&file).unwrap();
            if text.contains("target_os") {
                violations.push(format!("{}", file.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "platform conditionals belong in crates/app, but appear in: {violations:?}"
    );
}

fn walk_rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else { return files };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    files
}
