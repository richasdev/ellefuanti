//! Highlights a corpus of real Blade templates, if one is present.
//!
//! # Why a corpus test rather than more fixtures
//!
//! The 2026-08-14 crash — `core::str::slice_error_fail` inside
//! `html_markup_spans::blade_skip`, reached from `render_rows` — was found by opening a
//! real Laravel project, not by reading the scanner. It reproduces on
//! `<div>Configuração</div>`, a line that appears in ordinary templates and in none of
//! this crate's fixtures, because fixtures get written in ASCII by people typing in
//! ASCII. A second bug (padded viewports labelling spans a few bytes off) needed a file
//! *longer than the 256-byte padding* to show up at all, which no fixture was.
//!
//! The unit tests in `highlight.rs` pin both shapes and run everywhere. This adds the
//! thing they cannot: volume and variety that nobody chose on purpose.
//!
//! Point `ELLE_BLADE_CORPUS` at a directory to check it. With the variable unset the test
//! reports that it checked nothing and passes — a test that fails on a clean checkout for
//! want of somebody's home directory is a broken test, not a finding.

use elle_syntax::{Language, SyntaxTree};
use elle_text::Buffer;

/// Bounded so an unlucky `ELLE_BLADE_CORPUS=/` cannot turn this into a full disk walk.
const MAX_DEPTH: usize = 8;
const MAX_FILES: usize = 400;

fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>, depth: usize) {
    if depth > MAX_DEPTH || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            // `vendor` is other people's templates and dwarfs the project's own.
            if name != "vendor" && name != "node_modules" && !name.starts_with('.') {
                collect(&path, out, depth + 1);
            }
        } else if path.to_string_lossy().ends_with(".blade.php") {
            out.push(path);
        }
    }
}

#[test]
fn every_real_blade_template_highlights_without_panicking() {
    let Some(root) = std::env::var_os("ELLE_BLADE_CORPUS") else {
        eprintln!("ELLE_BLADE_CORPUS unset; skipping the corpus check");
        return;
    };

    let mut files = Vec::new();
    collect(std::path::Path::new(&root), &mut files, 0);
    assert!(!files.is_empty(), "no .blade.php files under {}", root.to_string_lossy());

    let mut checked = 0;
    for path in &files {
        // Not every file on disk is valid UTF-8 or parses; those are not this test's
        // subject and skipping them keeps the failure message meaningful.
        let Ok(source) = std::fs::read_to_string(path) else { continue };
        let buffer = Buffer::new(&source);
        let Ok(tree) = SyntaxTree::new(Language::Blade, &buffer) else { continue };

        // The whole file, then a sliding viewport. Both crashes needed both: one fired on
        // any accented template, the other only once the padded window straddled a
        // character partway down a file longer than the padding.
        let len = source.len();
        let mut viewports = vec![0..len];
        let mut at = 0;
        while at < len {
            viewports.push(at..(at + 512).min(len));
            // A prime stride, so windows land on varied offsets instead of marching in
            // step with any structure in the file.
            at += 251;
        }

        for range in viewports {
            let spans = tree.highlights(&buffer, range.clone());
            for span in &spans {
                // A span that splits a character panics the renderer that slices the line
                // to paint it — the second of the two bugs, and silent until it renders.
                assert!(
                    source.is_char_boundary(span.range.start)
                        && source.is_char_boundary(span.range.end),
                    "span {:?} splits a character in {} (viewport {range:?})",
                    span.range,
                    path.display()
                );
            }
        }
        checked += 1;
    }

    eprintln!("highlighted {checked} real Blade templates");
    assert!(checked > 0, "found {} files but could read none", files.len());
}
