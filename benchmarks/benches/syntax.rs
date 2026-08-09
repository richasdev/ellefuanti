//! Parsing and highlighting cost.
//!
//! Two claims are under test here, and they are the two that keep typing responsive:
//!
//! 1. **Incremental reparse after one keystroke is far cheaper than a cold parse.** If it
//!    is not, tree-sitter's edit replay is being defeated somewhere and every keystroke
//!    pays full parse cost.
//! 2. **Highlight extraction scales with the viewport, not the file.** The whole-file
//!    numbers exist only as the control: they are what we are *avoiding* per frame, not a
//!    cost we pay.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use elle_syntax::{Language, SyntaxTree};
use elle_text::{Buffer, Point};
use std::hint::black_box;

fn php_source(classes: usize) -> String {
    let mut text = String::from("<?php\n\nnamespace App\\Models;\n\n");
    for i in 0..classes {
        text.push_str(&format!(
            "class Model{i} extends Model\n{{\n    protected $table = 'model_{i}';\n    protected $fillable = ['name', 'email', 'created_at'];\n\n    public function posts()\n    {{\n        return $this->hasMany(Post::class, 'model_id');\n    }}\n\n    public function scopeActive($query)\n    {{\n        return $query->where('active', true);\n    }}\n}}\n\n"
        ));
    }
    text
}

const SIZES: [(&str, usize); 3] =
    [("10_classes", 10), ("100_classes", 100), ("1000_classes", 1000)];

fn cold_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("cold_parse");

    for (label, classes) in SIZES {
        let buffer = Buffer::new(&php_source(classes));
        group.bench_function(label, |b| {
            b.iter(|| SyntaxTree::new(Language::Php, black_box(&buffer)).unwrap())
        });
    }
    group.finish();
}

/// The number that matters: one character typed, then reparse.
fn incremental_reparse(c: &mut Criterion) {
    let mut group = c.benchmark_group("incremental_reparse_1_char");

    for (label, classes) in SIZES {
        let text = php_source(classes);

        group.bench_function(label, |b| {
            b.iter_batched(
                || {
                    let buffer = Buffer::new(&text);
                    let tree = SyntaxTree::new(Language::Php, &buffer).unwrap();
                    (buffer, tree)
                },
                |(mut buffer, mut tree)| {
                    // Edit in the middle, where the surrounding tree is deepest.
                    let offset = buffer.point_to_offset(Point::new(buffer.len_lines() / 2, 0));
                    let edit = buffer.insert(offset, "x");
                    tree.apply_edits(&buffer, &[edit]);
                    tree
                },
                criterion::BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

fn highlight_viewport_vs_whole_file(c: &mut Criterion) {
    let mut group = c.benchmark_group("highlights");

    for (label, classes) in SIZES {
        let buffer = Buffer::new(&php_source(classes));
        let tree = SyntaxTree::new(Language::Php, &buffer).unwrap();

        // 80 rows is roughly one screen at a normal window size — what a frame requests.
        let rows = buffer.len_lines();
        let mid = rows / 2;
        let start = buffer.point_to_offset(Point::new(mid, 0));
        let end = buffer.point_to_offset(Point::new((mid + 80).min(rows - 1), 0));

        group.bench_with_input(BenchmarkId::new("viewport_80_rows", label), &(), |b, _| {
            b.iter(|| tree.highlights(black_box(&buffer), start..end))
        });

        // Control only. This is the cost the viewport scoping avoids per frame.
        group.bench_with_input(BenchmarkId::new("whole_file", label), &(), |b, _| {
            b.iter(|| tree.highlights(black_box(&buffer), 0..buffer.len_bytes()))
        });
    }
    group.finish();
}

fn blade_highlighting(c: &mut Criterion) {
    let mut text = String::from("@extends('layouts.app')\n\n@section('content')\n");
    for i in 0..500 {
        text.push_str(&format!(
            "<div class=\"row-{i}\">\n    @if($item{i}->active)\n        {{{{ $item{i}->name }}}}\n        <button wire:click=\"select({i})\">Pick</button>\n    @endif\n</div>\n"
        ));
    }
    text.push_str("@endsection\n");

    let buffer = Buffer::new(&text);
    let tree = SyntaxTree::new(Language::Blade, &buffer).unwrap();
    let end = buffer.point_to_offset(Point::new(80.min(buffer.len_lines() - 1), 0));

    // Blade pays for the PHP tree walk plus the directive scanner, so it is measured
    // separately rather than assumed to match PHP.
    c.bench_function("highlights/blade_viewport_80_rows", |b| {
        b.iter(|| tree.highlights(black_box(&buffer), 0..end))
    });
}

criterion_group!(
    benches,
    cold_parse,
    incremental_reparse,
    highlight_viewport_vs_whole_file,
    blade_highlighting
);
criterion_main!(benches);
