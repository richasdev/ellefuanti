//! What one frame of editor rendering costs on a very large file.
//!
//! The §21 frame budget is 8.3 ms at 120 fps. A frame's per-row work is: convert the row
//! to a byte offset, slice the line out of the rope, and clip the batched highlight spans
//! onto it. This bench reproduces exactly that for an 80-row viewport, so the claim
//! "a 50k-line file scrolls at frame rate" is measured rather than asserted.
//!
//! It deliberately does **not** go through gpui: this measures the work the editor does
//! per frame, not GPU submission. If this is fast and scrolling still stutters, the
//! problem is in the UI layer and this bench correctly exonerates the domain layer.

use criterion::{Criterion, criterion_group, criterion_main};
use elle_syntax::{Language, SyntaxTree};
use elle_text::{Buffer, Point};
use std::hint::black_box;

/// A file shaped like the one a real Laravel project would have too many of.
fn big_php(classes: usize) -> String {
    let mut text = String::from("<?php\n\nnamespace App\\Models;\n\n");
    for i in 0..classes {
        text.push_str(&format!(
            "class Model{i} extends Model\n{{\n    protected $table = 'model_{i}';\n    protected $fillable = ['name', 'email'];\n\n    public function posts()\n    {{\n        return $this->hasMany(Post::class);\n    }}\n}}\n\n"
        ));
    }
    text
}

/// One frame's worth of row preparation for an 80-row viewport.
fn frame_at(buffer: &Buffer, tree: &SyntaxTree, first_row: usize) -> usize {
    const ROWS: usize = 80;
    let last_row = (first_row + ROWS).min(buffer.len_lines().saturating_sub(1));

    // Batched highlight query for the visible band — one tree walk per frame, not per row.
    let start = buffer.point_to_offset(Point::new(first_row, 0));
    let end = buffer.point_to_offset(Point::new(last_row, buffer.line_len(last_row)));
    let spans = tree.highlights(buffer, start..end);

    // Per-row work: offset, line text, and clipping spans onto the line.
    let mut work = 0usize;
    for row in first_row..last_row {
        let line = buffer.line(row);
        let line_start = buffer.point_to_offset(Point::new(row, 0));
        let line_end = line_start + line.len();
        work += line.len();
        for span in &spans {
            if span.range.end > line_start && span.range.start < line_end {
                work += 1;
            }
        }
    }
    work
}

fn frame_on_50k_lines(c: &mut Criterion) {
    // 5000 classes ≈ 55k lines ≈ 1 MB, matching the issue #10 acceptance criterion.
    let text = big_php(5000);
    let buffer = Buffer::new(&text);
    let tree = SyntaxTree::new(Language::Php, &buffer).unwrap();
    let rows = buffer.len_lines();
    assert!(rows > 50_000, "fixture should exceed 50k lines, got {rows}");

    let mut group = c.benchmark_group("frame_80_rows/55k_lines");

    // Position matters: a naive implementation is fast at the top and slow at the bottom.
    for (label, first_row) in
        [("top", 0), ("middle", rows / 2), ("bottom", rows.saturating_sub(200))]
    {
        group.bench_function(label, |b| {
            b.iter(|| frame_at(black_box(&buffer), black_box(&tree), black_box(first_row)))
        });
    }
    group.finish();
}

/// Typing in a huge file: the edit, the incremental reparse, and the frame that follows.
/// This is the complete keystroke-to-pixel path for the domain layer.
fn keystroke_on_50k_lines(c: &mut Criterion) {
    let text = big_php(5000);

    // `iter_batched_ref`, not `iter_batched`: the fixture must be *borrowed* by the
    // routine. Taking `(Buffer, SyntaxTree)` by value moves them into the closure, so
    // criterion charges their destructors to the sample — and `drop(SyntaxTree)` on a
    // 1 MB file is ~8-10 ms of arena teardown, three times the keystroke itself. That
    // measured "24.8 ms per keystroke" and pointed the blame at the reparse; the editor
    // never destroys a buffer per keystroke, so the teardown must stay outside the clock.
    c.bench_function("keystroke_and_frame/55k_lines", |b| {
        b.iter_batched_ref(
            || {
                let buffer = Buffer::new(&text);
                let tree = SyntaxTree::new(Language::Php, &buffer).unwrap();
                (buffer, tree)
            },
            |(buffer, tree)| {
                let row = buffer.len_lines() / 2;
                let offset = buffer.point_to_offset(Point::new(row, 0));
                let edit = buffer.insert(offset, "x");
                tree.apply_edits(buffer, &[edit]);
                frame_at(buffer, tree, row.saturating_sub(40))
            },
            criterion::BatchSize::LargeInput,
        )
    });
}

/// Sustained typing: many consecutive keystrokes against one long-lived buffer and tree,
/// which is what actually happens when someone types a word into a large file. No fixture
/// is built or destroyed inside the clock, so every sample is pure keystroke-to-pixel work.
fn sustained_typing_on_50k_lines(c: &mut Criterion) {
    let text = big_php(5000);

    c.bench_function("sustained_typing/55k_lines", |b| {
        let mut buffer = Buffer::new(&text);
        let mut tree = SyntaxTree::new(Language::Php, &buffer).unwrap();
        let row = buffer.len_lines() / 2;

        b.iter(|| {
            // Column 5 keeps the insertion inside a declaration, so the file stays
            // parseable — typing that produces a transient syntax error is measured by
            // the keystroke_and_frame bench above.
            let offset = buffer.point_to_offset(Point::new(row, 5));
            let edit = buffer.insert(offset, "x");
            tree.apply_edits(&buffer, &[edit]);
            frame_at(&buffer, &tree, row.saturating_sub(40))
        })
    });
}

criterion_group!(
    benches,
    frame_on_50k_lines,
    keystroke_on_50k_lines,
    sustained_typing_on_50k_lines
);
criterion_main!(benches);
