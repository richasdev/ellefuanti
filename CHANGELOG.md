# Changelog

All notable changes to this project are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — unreleased

First release. A working editor foundation for PHP, Laravel, Livewire and Blade — native,
GPU-accelerated, written in Rust on [GPUI](https://gpui.rs).

**This is a 0.1.0 in the literal sense.** The editor works and is well tested at the domain
layer; the parts that need a human at a screen have not been confirmed. Read
[Known limitations](#known-limitations) before installing — they are not footnotes.

### Added

**Editor**

- Rope-backed text buffer with byte-offset API, correct on multibyte text throughout
- Undo/redo grouped by intent — a run of typing undoes as one step; a newline, cursor jump
  or save breaks the run
- Incremental tree-sitter parsing: a keystroke costs an edit-sized reparse, not a file-sized
  one, verified byte-identical against a cold parse
- Syntax highlighting for PHP and Blade, computed per visible viewport
- Cursor and selection with UTF-8-correct motion and goal-column memory on vertical movement
- Virtualised rendering, so a 55k-line file costs the same per frame as a 50-line one

**Workspace**

- Lazy file tree: opening a Laravel project reads one directory level, not `vendor/`
- Atomic saves via temp-file-plus-rename, so a crash mid-write cannot truncate your source
- Save-as for buffers with no path, re-detecting the language on save
- Unsaved-changes prompt before closing a modified tab
- Tabs with dirty indicators and per-tab close
- Command palette (⌘⇧P) and quick open (⌘P) over a background, cancellable project walk

**Terminal**

- Multiple independent PTY sessions with real VT/ANSI emulation via `alacritty_terminal`
- Resize, scrollback, workspace cwd, and teardown without zombie processes

**Language server client**

- Generic LSP client: JSON-RPC over stdio, lifecycle, capability negotiation, incremental
  document sync, cancellation
- Backends are configuration rather than code, so Intelephense is substitutable — enforced
  by a test that scans shipped code for backend names
- Position encoding negotiated (utf-8/utf-16/utf-32) against the codebase's byte offsets

**Engineering**

- 258 tests across 17 suites; `cargo clippy --all-targets` clean
- Layering enforced by test: only `crates/app` may depend on gpui
- Criterion benchmarks with a recorded baseline (`benchmarks/BASELINE.md`)
- Startup and frame instrumentation via `ELLE_PERF=1`
- 8 ADRs recording every irreversible technology decision

### Performance

Measured, not estimated. Full detail and methodology in `benchmarks/BASELINE.md`.

| Metric                                     | Target     | Measured                   |
| ------------------------------------------ | ---------- | -------------------------- |
| Cold startup (shaders precompiled)         | < 500 ms   | not yet measured, see note |
| Cold startup (runtime shaders, cold cache) | < 500 ms   | 520 ms                     |
| Cold startup (runtime shaders, warm cache) | < 500 ms   | **380 ms**                 |
| Warm startup                               | < 150 ms   | ~200 ms ❌                 |
| Idle RAM                                   | 100–200 MB | **69 MB**                  |
| Keystroke → pixel, 55k-line file           | < 8.3 ms   | **2.65 ms**                |
| Frame render, 55k-line file                | < 8.3 ms   | **0.08–0.77 ms**           |
| Folder open, 5000 vendor files             | —          | **64 µs**                  |

The release build compiles Metal shaders ahead of time, which should remove the ~432 ms of
runtime shader compilation that dominates a local build's cold start on a cold cache.

**That improvement is expected, not measured, and the distinction matters.** The precompiled
path requires full Xcode to build and cannot be compiled on the machine these numbers came
from — `xcrun metal` is absent, and the build fails outright. So the figure above is left
blank rather than filled with an estimate. Somebody with Xcode should run `ELLE_PERF=1` on a
release build and replace it with a real number.

### Known limitations

Stated plainly, because discovering them yourself would be worse.

- **The UI has never been visually verified** ([#35](https://github.com/richasdev/ellefuanti/issues/35)).
  The paint computation is unit-tested — the right bytes get the right colour — but nobody
  has confirmed that those colours land in the right pixels. Screen capture was unavailable
  in the environment this was built in. Panel geometry, column alignment, click-to-cursor
  accuracy and overlay positioning are unconfirmed.
- **Warm startup misses its target** — ~200 ms against 150 ms. The cause is not yet
  attributed, and will not be optimised by guessing.
- **No real language server has been run against the LSP client.** Every test uses a mock
  over real OS pipes. The client is not wired into the UI at all yet.
- **No Laravel intelligence.** Model/route/migration indexing is Milestone 3. Despite the
  project's framing, this release has no framework awareness beyond Blade syntax.
- **No Git, database, Docker, or debugger.** Milestones 5–6.
- **The binary is unsigned and un-notarised**, so Gatekeeper blocks it on first launch. See
  `RUNNING.md` in the release archive.
- **No IME or dead-key composition** ([#18](https://github.com/richasdev/ellefuanti/issues/18)),
  so CJK input does not work. Plain typing, including accented Latin characters, does.
- **macOS only.** The domain crates are portable; the UI layer is not, because GPUI is not.

### Notes on two bugs worth recording

Both were found by measurement rather than review, and both are documented in
`benchmarks/BASELINE.md`:

- Viewport-scoped syntax highlighting **was not actually viewport-scoped**. It range-checked
  correctly but scanned every sibling node, so cost grew with file size (50 → 156 µs across
  a 100× range) while reading as correct. Now flat at ~46 µs.
- A reported "24.8 ms per keystroke" turned out to be a **benchmark artifact**: criterion's
  `iter_batched` charged tree-sitter arena teardown to every sample. The real figure was
  already near budget. The lesson — suspect the measurement before the code — is recorded
  at the top of the baseline document.

[unreleased]: https://github.com/richasdev/ellefuanti/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/richasdev/ellefuanti/releases/tag/v0.1.0
