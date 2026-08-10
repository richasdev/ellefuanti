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

/// The same viewport claim, for the query-driven languages.
///
/// PHP is measured above on a hand-written tree walk; JSON/JS/TS/CSS go through
/// `highlights.scm` and a `QueryCursor`, which is a completely different mechanism with
/// its own way of failing. `set_byte_range` is what is supposed to keep the query engine
/// off the rest of the file, and "the API should prune" is exactly the sort of assumption
/// BASELINE.md exists to distrust — so it is measured across a 100× size range.
///
/// **The viewport is a fixed number of whole units, not 80 rows.** The first version of
/// this bench took an 80-row window and clamped it to the end of the file, so the small
/// fixture got a narrower window than the large one and the numbers grew 22 → 189 → 355 µs.
/// That looked exactly like the file-size regression this bench exists to catch, and it
/// was not one: holding the window genuinely constant gives 22.8 → 23.7 → 24.4 µs over the
/// same 100× range. The measurement was wrong, not the code — BASELINE.md's standing
/// warning, collected once more. Keep the window content identical across sizes or this
/// bench measures how much is on screen instead of how big the file is.
fn query_language_viewport(c: &mut Criterion) {
    // (language, header, one repeatable unit). The unit is what grows.
    let cases: [(Language, &str, &str); 4] = [
        (Language::Json, "{\n", "  \"key_name\": [1, 2, \"three\", true, null],\n"),
        (
            Language::JavaScript,
            "// header\nimport { a } from 'b';\n",
            "export function handler(req, res) {\n  const out = Model.find({ id: req.id });\n  return res.json({ ok: true, out });\n}\n",
        ),
        (
            Language::TypeScript,
            "// header\nimport type { A } from './b';\n",
            "export function handler(req: Request, res: Response): void {\n  const out: A[] = Model.find({ id: req.id });\n  res.json({ ok: true, out });\n}\n",
        ),
        (
            Language::Css,
            "/* header */\n",
            ".card-item {\n  color: #ff8800;\n  margin: 10px 2em;\n  --local-var: 4;\n}\n",
        ),
    ];

    // How many copies of `unit` the measured window spans. Roughly a screenful, and the
    // same in every fixture — which is the whole point.
    const WINDOW_UNITS: usize = 15;

    let mut group = c.benchmark_group("highlights_query");

    for (language, header, unit) in cases {
        for (size_label, repeats) in [("small", 40usize), ("medium", 400), ("large", 4000)] {
            let mut text = String::from(header);
            text.push_str(&unit.repeat(repeats));
            if language == Language::Json {
                text.push_str("  \"last\": 0\n}\n");
            }

            let buffer = Buffer::new(&text);
            let tree = SyntaxTree::new(language, &buffer).unwrap();

            // Start at a unit boundary near the middle, and span a fixed number of units.
            // Byte arithmetic rather than rows, so the window holds exactly the same text
            // in all three sizes and only the file behind it changes.
            let start = header.len() + (repeats / 2) * unit.len();
            let end = start + WINDOW_UNITS * unit.len();
            assert!(end <= buffer.len_bytes(), "window must fit in the smallest fixture");

            let id = format!("{}/{size_label}", language.name());
            group.bench_with_input(BenchmarkId::new("fixed_viewport", &id), &(), |b, _| {
                b.iter(|| tree.highlights(black_box(&buffer), start..end))
            });
            // Control only: the cost the viewport scoping avoids per frame.
            group.bench_with_input(BenchmarkId::new("whole_file", &id), &(), |b, _| {
                b.iter(|| tree.highlights(black_box(&buffer), 0..buffer.len_bytes()))
            });
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    cold_parse,
    incremental_reparse,
    highlight_viewport_vs_whole_file,
    blade_highlighting,
    query_language_viewport
);
criterion_main!(benches);
