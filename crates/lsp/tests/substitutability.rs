//! Mechanical enforcement of RISKS.md #2.
//!
//! The risk register says Intelephense must be substitutable, and that "no
//! Intelephense-specific behaviour may leak past the LSP client boundary". Stated in
//! prose that erodes on the first deadline — the same reasoning behind
//! `crates/app/tests/architecture.rs`, which is why this is a test rather than a note.
//!
//! What it checks is crude but exactly the failure mode described: a backend's *name*
//! appearing in the client's logic. A name in the source is how special-casing starts,
//! and once `if server == "intelephense"` exists, substitutability is gone on paper
//! only.

use std::fs;
use std::path::{Path, PathBuf};

fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return files;
    };
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
/// Both exclusions are deliberate, and each is the difference between a rule that
/// enforces the design and one that just forbids words:
///
/// - **Comments** must be able to *discuss* Intelephense. The whole reason this crate
///   is shaped the way it is comes from RISKS.md #2, and a check that forbade naming
///   the risk would delete the explanation of the constraint it enforces.
/// - **Test modules** must be able to name real backends, because the most convincing
///   evidence of substitutability is a test that configures two different servers and
///   shows the crate cannot tell them apart. That test is the proof, not a violation.
///
/// Deliberately naive parsing: line comments and a brace-counted `mod tests` are the
/// only forms this crate uses.
///
/// ponytail: if this ever needs to survive block comments, strings containing `//`, or
/// a nested test module, parse with `syn` instead of counting braces. Today that would
/// be a dependency and 50 lines to check something a grep already answers.
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

/// A guard that fails an empty check: a scanner that strips too much would pass every
/// assertion below while detecting nothing, which is the one way this file could give
/// false confidence.
#[test]
fn the_scanner_keeps_real_code_and_drops_only_tests_and_comments() {
    let source = r#"
// intelephense is discussed here
/// and here, in a doc comment
fn real_logic() {
    let language = "generic";
}

#[cfg(test)]
mod tests {
    #[test]
    fn uses_intelephense_as_a_fixture() {
        let name = "intelephense";
        if true { let nested = "phpactor"; }
    }
}
"#;

    let shipped = shipped_code(source);
    assert!(shipped.contains("fn real_logic"), "real code must survive: {shipped:?}");
    assert!(shipped.contains("\"generic\""), "real literals must survive: {shipped:?}");
    assert!(
        !shipped.to_lowercase().contains("intelephense"),
        "comments and test modules must be stripped: {shipped:?}"
    );
    assert!(
        !shipped.to_lowercase().contains("phpactor"),
        "nested braces inside a test module must stay stripped: {shipped:?}"
    );

    // And the check genuinely fires on a violation in shipped code.
    let violating = "fn pick() { if server == \"intelephense\" { } }";
    assert!(shipped_code(violating).to_lowercase().contains("intelephense"));
}

#[test]
fn no_backend_name_appears_in_the_clients_logic() {
    // Every PHP language server this crate might be pointed at. If a name shows up in
    // code, someone has started special-casing.
    const BACKENDS: [&str; 5] = ["intelephense", "phpactor", "psalm", "phpstan", "serenata"];

    let mut violations = Vec::new();

    for file in rust_files(&src_dir()) {
        let text = fs::read_to_string(&file).unwrap();
        let code = shipped_code(&text).to_lowercase();
        for backend in BACKENDS {
            if code.contains(backend) {
                violations.push(format!("{} mentions {backend}", file.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "RISKS.md #2: a backend must be describable entirely by a ServerConfig value, so no \
         backend name may appear in this crate's logic. Found: {violations:?}. Move whatever \
         needs to differ into ServerConfig (command, args, initializationOptions) instead of \
         branching on which server is running."
    );
}

#[test]
fn no_php_specific_behaviour_is_baked_in() {
    // The client must not know PHP from Rust. `language_id` is data supplied by the
    // caller; a hardcoded "php" in the logic would mean this client cannot serve
    // anything else, which is the same substitutability failure one layer down.
    //
    // Trigger characters are the concrete temptation here: `$` and `->` are PHP's
    // answer, and they must come from the server's capabilities, not from this crate.
    let mut violations = Vec::new();

    for file in rust_files(&src_dir()) {
        let text = fs::read_to_string(&file).unwrap();
        let code = shipped_code(&text);
        // Any string literal equal to "php" in real code.
        if code.contains("\"php\"") {
            violations.push(file.display().to_string());
        }
    }

    assert!(
        violations.is_empty(),
        "this is a generic LSP client: a hardcoded \"php\" literal in {violations:?} means it \
         has stopped being generic. Language ids come from the caller."
    );
}

#[test]
fn request_methods_come_from_lsp_types_not_hand_written_strings() {
    // Each typed request method is a new place a `"textDocument/…"` literal could be
    // typed by hand, and a hand-written method name is how a server-specific extension
    // (`intelephense/index`, `phpactor/…`) gets sent without anything noticing — the
    // request is spelled out in full rather than named by the protocol crate, so no
    // backend name has to appear for the client to stop being generic.
    //
    // Using `lsp_types::request::*::METHOD` keeps the set of reachable methods to the
    // ones the specification defines. `$/cancelRequest` and the `client/*` replies are
    // the exceptions: they are protocol plumbing `lsp-types` gives no constant for.
    const ALLOWED: [&str; 5] = [
        "$/cancelRequest",
        "client/registerCapability",
        "client/unregisterCapability",
        "workspace/configuration",
        "window/workDoneProgress/create",
    ];

    let mut violations = Vec::new();

    for file in rust_files(&src_dir()) {
        let text = fs::read_to_string(&file).unwrap();
        for line in shipped_code(&text).lines() {
            // A method name is a string literal containing a `/` and no whitespace,
            // which is what every LSP method and every vendor extension looks like on
            // the wire. The whitespace exclusion is what keeps prose — log messages
            // that happen to mention `$/cancelRequest` — from reading as a method.
            for literal in line.split('"').skip(1).step_by(2) {
                let looks_like_a_method =
                    literal.contains('/') && !literal.contains(char::is_whitespace);
                if looks_like_a_method && !ALLOWED.contains(&literal) {
                    violations.push(format!("{}: {literal:?}", file.display()));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "LSP method names must come from `lsp_types::request::*::METHOD`, not string literals, \
         so this client can only send requests the specification defines. Found: {violations:?}"
    );
}

#[test]
fn the_crate_does_not_depend_on_gpui_or_tokio() {
    // ADR-0004 (only crates/app may use gpui) and ADR-0007 (no tokio). The workspace
    // test in crates/app covers gpui across all crates; this one also catches tokio,
    // which is the live temptation for an LSP client specifically — every LSP example
    // on the internet is async, and `async-lsp` would have brought tokio with it.
    let manifest =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();

    for forbidden in ["gpui", "tokio", "async-lsp", "tower-lsp"] {
        assert!(
            !manifest.lines().any(|line| line.trim().starts_with(forbidden)),
            "elle-lsp must not depend on {forbidden}: it is a blocking, executor-agnostic \
             domain crate (ADR-0004, ADR-0007)"
        );
    }
}
