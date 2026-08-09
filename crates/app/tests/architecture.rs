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
