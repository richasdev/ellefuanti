//! File tree cost on a Laravel-shaped project.
//!
//! The claim under test: **opening a folder costs one directory level, not the project.**
//! The fixture therefore includes a `vendor/` with thousands of files. If `FileTree::new`
//! grows with total project size rather than with the root's entry count, the startup
//! budget is gone and this bench is where that shows up.

use criterion::{Criterion, criterion_group, criterion_main};
use elle_workspace::{FileTree, read_file, write_file};
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};

/// A synthetic Laravel project: a realistic root, plus a deliberately huge `vendor/`.
fn laravel_fixture(vendor_packages: usize, files_per_package: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    for path in [
        "app/Http/Controllers",
        "app/Models",
        "app/Livewire",
        "config",
        "database/migrations",
        "resources/views/components",
        "routes",
        "tests/Feature",
        "storage/logs",
        "public",
    ] {
        fs::create_dir_all(root.join(path)).unwrap();
    }

    fs::write(root.join("artisan"), "#!/usr/bin/env php\n").unwrap();
    fs::write(root.join("composer.json"), "{}\n").unwrap();
    fs::write(root.join(".env"), "APP_KEY=base64:x\n").unwrap();
    // vendor/ ignored, which is what makes the root listing small despite its size.
    fs::write(root.join(".gitignore"), "/vendor\n/node_modules\n").unwrap();

    for i in 0..40 {
        fs::write(
            root.join(format!("app/Models/Model{i}.php")),
            "<?php\n\nnamespace App\\Models;\n\nclass Model extends Model {}\n",
        )
        .unwrap();
    }
    for i in 0..30 {
        fs::write(
            root.join(format!("resources/views/page-{i}.blade.php")),
            "@extends('layouts.app')\n@section('content')\n{{ $x }}\n@endsection\n",
        )
        .unwrap();
    }

    // The part that would dominate any recursive walk.
    for p in 0..vendor_packages {
        let package = root.join(format!("vendor/package-{p}/src"));
        fs::create_dir_all(&package).unwrap();
        for f in 0..files_per_package {
            fs::write(package.join(format!("File{f}.php")), "<?php\nclass X {}\n").unwrap();
        }
    }

    dir
}

fn open_folder(c: &mut Criterion) {
    // ~5000 files in vendor/, none of which may be touched at open time.
    let fixture = laravel_fixture(250, 20);
    let root = fixture.path().to_path_buf();

    c.bench_function("file_tree/open_root_with_5000_vendor_files", |b| {
        b.iter(|| FileTree::new(black_box(&root)).unwrap())
    });
}

fn expand_directory(c: &mut Criterion) {
    let fixture = laravel_fixture(50, 10);
    let root = fixture.path().to_path_buf();

    c.bench_function("file_tree/expand_app_models_40_files", |b| {
        b.iter_batched(
            || {
                let mut tree = FileTree::new(&root).unwrap();
                // Expand `app` first so `app/Models` is a visible row to expand.
                let app = tree.entries().iter().position(|e| e.name == "app").unwrap();
                tree.toggle(app).unwrap();
                let models = tree.entries().iter().position(|e| e.name == "Models").unwrap();
                (tree, models)
            },
            |(mut tree, index)| {
                tree.toggle(index).unwrap();
                tree
            },
            criterion::BatchSize::SmallInput,
        )
    });
}

fn file_io(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();

    let mut group = c.benchmark_group("file_io");
    for (label, size) in [("10KB", 10 * 1024), ("1MB", 1024 * 1024)] {
        let text: String = "<?php\nclass Model extends Model {}\n".repeat(size / 34);
        let path: PathBuf = dir.path().join(format!("bench-{label}.php"));
        fs::write(&path, &text).unwrap();

        group.bench_function(format!("read/{label}"), |b| {
            b.iter(|| read_file(black_box(&path)).unwrap())
        });

        // Includes the temp-file-plus-rename and the fsync that makes a save crash-safe:
        // the cost of not truncating the user's source, measured rather than assumed free.
        group.bench_function(format!("write_atomic/{label}"), |b| {
            b.iter(|| write_file(black_box(&path), black_box(&text)).unwrap())
        });
    }
    group.finish();
}

/// Sanity check that the fixture is actually as large as claimed — a bench that silently
/// measures an empty directory would look excellent and mean nothing.
fn assert_fixture_is_large(root: &Path) {
    let count = walkdir_count(root);
    assert!(count > 4000, "fixture should be large, found {count} files");
}

fn walkdir_count(dir: &Path) -> usize {
    let mut count = 0;
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            count += walkdir_count(&path);
        } else {
            count += 1;
        }
    }
    count
}

fn fixture_check(_c: &mut Criterion) {
    let fixture = laravel_fixture(250, 20);
    assert_fixture_is_large(fixture.path());
}

criterion_group!(benches, fixture_check, open_folder, expand_directory, file_io);
criterion_main!(benches);
