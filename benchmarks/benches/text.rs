//! Buffer edit cost across file sizes.
//!
//! The claim these benches defend: **edit cost does not grow with file size**. A rope is
//! only worth its complexity if that holds, so the sizes span four orders of magnitude and
//! the same operation is measured at each. If the 10 MB numbers track the 1 KB numbers,
//! the rope is doing its job; if they scale linearly, something is copying the document.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use elle_text::{Buffer, Point};
use std::hint::black_box;

/// Synthetic PHP of roughly `target_bytes`, so the benchmark text resembles what the
/// editor actually holds rather than random bytes.
fn php_source(target_bytes: usize) -> String {
    let mut text = String::with_capacity(target_bytes + 128);
    text.push_str("<?php\n\nnamespace App\\Models;\n\n");
    let mut i = 0;
    while text.len() < target_bytes {
        text.push_str(&format!(
            "class Model{i} extends Model\n{{\n    protected $fillable = ['name', 'email'];\n\n    public function posts()\n    {{\n        return $this->hasMany(Post::class);\n    }}\n}}\n\n"
        ));
        i += 1;
    }
    text
}

const SIZES: [(&str, usize); 4] =
    [("1KB", 1_024), ("100KB", 100 * 1_024), ("1MB", 1_024 * 1_024), ("10MB", 10 * 1_024 * 1_024)];

fn insert_positions(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");

    for (label, size) in SIZES {
        let text = php_source(size);
        let len = text.len();

        // Start, middle and end: a String would show a huge spread here (memmove of the
        // tail), a rope should not.
        for (position_label, offset) in [("start", 0), ("middle", len / 2), ("end", len)] {
            group.bench_with_input(
                BenchmarkId::new(position_label, label),
                &offset,
                |b, &offset| {
                    b.iter_batched(
                        || Buffer::new(&text),
                        |mut buffer| {
                            buffer.insert(black_box(offset), "x");
                            buffer
                        },
                        criterion::BatchSize::SmallInput,
                    )
                },
            );
        }
    }
    group.finish();
}

/// A run of typing, which is the latency the user actually feels.
fn typing_run(c: &mut Criterion) {
    let mut group = c.benchmark_group("typing_100_chars");

    for (label, size) in SIZES {
        let text = php_source(size);
        let start = text.len() / 2;

        group.bench_function(label, |b| {
            b.iter_batched(
                || Buffer::new(&text),
                |mut buffer| {
                    for i in 0..100 {
                        buffer.insert(start + i, "a");
                    }
                    buffer
                },
                criterion::BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

/// Offset conversion, called per visible row per frame — so it is on the frame-time path.
fn offset_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("point_to_offset");

    for (label, size) in SIZES {
        let buffer = Buffer::new(&php_source(size));
        let last_row = buffer.len_lines() - 1;

        group.bench_function(label, |b| {
            b.iter(|| {
                // Sample across the document rather than one hot line, so a cached
                // lookup cannot flatter the result.
                let mut total = 0usize;
                for n in 0..50 {
                    let row = last_row * n / 50;
                    total += buffer.point_to_offset(black_box(Point::new(row, 0)));
                }
                total
            })
        });
    }
    group.finish();
}

fn undo_depth(c: &mut Criterion) {
    // Undo stores inverse edits, not snapshots, so deep history should stay cheap even on
    // a large document.
    let text = php_source(1_024 * 1_024);

    c.bench_function("undo_1000_edits/1MB", |b| {
        b.iter_batched(
            || {
                let mut buffer = Buffer::new(&text);
                let start = text.len() / 2;
                for i in 0..1000 {
                    buffer.break_undo_group();
                    buffer.insert(start + i, "z");
                }
                buffer
            },
            |mut buffer| {
                while buffer.undo().is_some() {}
                buffer
            },
            criterion::BatchSize::LargeInput,
        )
    });
}

criterion_group!(benches, insert_positions, typing_run, offset_conversion, undo_depth);
criterion_main!(benches);
