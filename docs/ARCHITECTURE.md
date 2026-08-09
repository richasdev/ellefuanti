# Architecture

Ellefuanti is a native, GPU-accelerated IDE for PHP, Laravel, Livewire and Blade.
It is not a generic editor with Laravel plugins; framework knowledge is a first-class
subsystem, not an extension.

This document covers deliverables 1–7 of the master prompt (§28). Risks are in
[RISKS.md](RISKS.md), the Milestone 1 task breakdown is in [MILESTONE-1.md](MILESTONE-1.md),
and each irreversible technology choice has an ADR in [adr/](adr/).

---

## 1. Architecture overview

Three layers, one direction of dependency. Nothing below points upward.

```
┌──────────────────────────────────────────────────────────┐
│ Presentation            crates/app  (gpui)               │
│   window, panels, editor view, palette, keymap           │
└───────────────────────────┬──────────────────────────────┘
                            │ calls, holds Entity<T>
┌───────────────────────────▼──────────────────────────────┐
│ Domain              core · text · syntax                 │
│   commands, rope buffer, cursors, parse trees            │
│   (later: laravel, blade, livewire, project_index)       │
└───────────────────────────┬──────────────────────────────┘
                            │ calls
┌───────────────────────────▼──────────────────────────────┐
│ Infrastructure          workspace                        │
│   filesystem, file tree, file IO                         │
│   (later: lsp, git, database, docker, terminal)          │
└──────────────────────────────────────────────────────────┘
```

The rule that keeps this honest: **only `crates/app` may depend on `gpui`.** Every other
crate is a plain Rust library, testable with `cargo test` and no window. That is what
§3's "Laravel Intelligence não deve depender de GPUI" means in practice, and it is
enforced mechanically — see [ADR-0004](adr/0004-ui-independent-domain-crates.md) and the
`no_gpui_outside_app` test in `crates/app/tests/architecture.rs`.

### Current crate graph

```
ellefuanti (bin, gpui)
├── elle-workspace ──── ignore
├── elle-syntax ─────── tree-sitter, tree-sitter-php
│   └── elle-text ───── ropey
├── elle-text
└── elle-core          (no deps beyond error types)
```

Five crates, not the eighteen sketched in §4. That sketch is a direction, and §4 says so
explicitly ("não crie dezenas de crates sem necessidade atual"). Crates are added when
code exists to put in them: `elle-lsp` at Milestone 2, `elle-laravel` and
`elle-project-index` at Milestone 3, and so on. An empty crate is a maintenance cost with
no reader.

### Where the later subsystems attach

| Milestone | New crate                                                   | Attaches to                                                   |
| --------- | ----------------------------------------------------------- | ------------------------------------------------------------- |
| 2         | `elle-lsp`                                                  | `elle-syntax` sits beside it under a `LanguageService` facade |
| 3         | `elle-laravel`, `elle-project-index`                        | reads `elle-syntax` trees; owns its SQLite index              |
| 4         | `elle-blade`, `elle-livewire`                               | consume `elle-project-index`                                  |
| 5         | `elle-git`, `elle-database`, `elle-docker`, `elle-terminal` | independent leaves; app-level only                            |

Each is a leaf or near-leaf on purpose: §24's fault isolation is a _graph_ property. Docker
cannot break the editor if nothing the editor needs depends on Docker.

---

## 2. Technology validation

Every choice below was verified by compiling and running it on this machine, not from
documentation. Findings that changed the plan are called out.

| Area          | Choice                                      | Validation result                                                                                                                                                                                                                                                                                        |
| ------------- | ------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| UI            | `gpui` 0.2.2 (crates.io)                    | ✅ Builds and links standalone. **Finding:** git `main` has moved entry-point setup into a separate `gpui_platform` crate, so `main`'s examples do not compile against the release. Pin the crates.io version or a git tag, never `branch = "main"`.                                                     |
| Metal shaders | `runtime_shaders` feature                   | ✅ **Finding:** the default build shells out to `xcrun metal`, which ships only with full Xcode, not Command Line Tools. The `runtime_shaders` feature compiles shaders at runtime instead and builds with CLT alone. Enabled in the workspace so a fresh checkout builds without a 40 GB Xcode install. |
| Text storage  | `ropey` 1.6                                 | ✅ O(log n) edits confirmed. **Finding:** `Rope::chars()` is not `DoubleEndedIterator`, and `try_byte_to_char` is _not_ a boundary check — it silently rounds a mid-codepoint byte down. Both bit the first implementation; see [ADR-0003](adr/0003-rope-text-storage.md).                               |
| Parsing       | `tree-sitter` 0.26 + `tree-sitter-php` 0.24 | ✅ Compiled and parsed together despite the minor-version skew. The ABI concern was real enough to check and turned out fine; a test pins it.                                                                                                                                                            |
| Blade         | PHP grammar + directive scanner             | ✅ `LANGUAGE_PHP` (not `PHP_ONLY`) parses interleaved HTML. See [ADR-0006](adr/0006-blade-strategy.md).                                                                                                                                                                                                  |
| Filesystem    | `ignore` 0.4                                | ✅ `.gitignore` matching without a recursive walk.                                                                                                                                                                                                                                                       |
| Async         | gpui's own executor                         | ✅ `cx.background_spawn` + `cx.spawn` verified. No tokio: see [ADR-0007](adr/0007-gpui-executor-not-tokio.md).                                                                                                                                                                                           |
| Rust          | 1.94.1 stable, edition 2024                 | ✅ No nightly required.                                                                                                                                                                                                                                                                                  |

Not yet validated, because no code needs them until Milestone 2+: `sqlx`, `git2`,
`portable-pty`, DAP. Their versions are noted in [RISKS.md](RISKS.md) as
unvalidated-but-current.

---

## 3. Editor architecture

The editor is written from scratch in Rust (§7 — no Monaco, no CodeMirror), and split so
that the part that is easy to get wrong is testable without a window.

```
EditorView            crates/app/src/editor/view.rs      gpui: render, input, scroll
  └── Document        crates/app/src/editor/state.rs     cursor, selection, motion  ← plain Rust
        ├── Buffer    crates/text                        rope, undo/redo, edit log  ← plain Rust
        └── SyntaxTree crates/syntax                     incremental parse tree     ← plain Rust
```

`Document` holds every editing _semantic_ — where the cursor lands, what backspace
deletes, how vertical motion remembers its goal column — and imports no gpui. All of it is
covered by unit tests. `EditorView` is deliberately thin: translate input into `Document`
calls, translate `Document` state into elements.

### The invariants that make it fast

**Edits are incremental end to end.** A keystroke does three bounded things: a rope splice
(O(log n)), an `InputEdit` replayed onto the parse tree, and a tree-sitter reparse that
reuses unchanged subtrees. Nothing is proportional to file size. The test
`incremental_edit_matches_full_reparse` asserts the incremental tree is byte-identical to a
cold parse — the check that keeps this from silently drifting into corruption.

**Only visible rows are rendered.** Both the editor and the file tree use gpui's
`uniform_list`, which calls back only for the visible index range. Highlighting takes the
same shape: `SyntaxTree::highlights(buffer, range)` walks only nodes intersecting the
requested byte range and prunes whole subtrees that cannot. A 50k-line file costs the same
per frame as a 50-line one.

**Byte offsets everywhere, snapped to char boundaries.** Ropey indexes chars; tree-sitter
and gpui text layout want bytes. Mixing the two units corrupts any file containing `é` —
which, in a Portuguese-language Laravel codebase, is every file. The public API of
`elle-text` is bytes-only, converting once internally, and offsets snap down to a char
boundary rather than splitting a codepoint. Tested with multibyte fixtures throughout.

**Undo coalesces by intent, not by keystroke.** Consecutive typing merges into one undo
step; a newline, a cursor jump, or a save breaks the run. Undo stores inverse `Edit`s, not
document snapshots, so history costs what changed rather than file-size × depth.

### Deliberately not built yet

Multi-cursor, folding, inline hints, completion UI, minimap. Each is listed in §7 as an
eventual component, and each is a `Vec<Selection>`-shaped or overlay-shaped extension of
what exists. Building them now would mean maintaining them through the Milestone 2–3
refactors with no user reaching them.

---

## 4. Async architecture

**The UI thread is sacred** (§22). Anything that can touch a disk, a socket, or a
subprocess runs on the background executor.

```
main thread (gpui)                      background executor (thread pool)
──────────────────                      ────────────────────────────────
render, input, layout
    │
    │ cx.background_spawn(work)
    ├──────────────────────────────────▶ blocking read_dir / read_file / write_file
    │                                    (later: LSP, git, index, docker)
    │ ◀── .await yields the result ─────┘
    │
    └─ entity.update(cx, ..); cx.notify()   ← state change and repaint, on the main thread
```

Three rules:

1. **Blocking crates stay honest.** `elle-workspace` functions are plain blocking
   functions. They do not spawn, and they do not know which executor they run on. The
   caller wraps them. This is why they are testable without a runtime, and why the async
   choice is reversible.
2. **One executor, gpui's.** No tokio. Adding a second runtime means two thread pools
   competing for cores and a bridge at every boundary. See
   [ADR-0007](adr/0007-gpui-executor-not-tokio.md).
3. **Stale work is dropped, not awaited.** Dropping a gpui `Task` cancels it. Every
   request-shaped operation stores its `Task` handle in the view, so issuing a new one
   drops the old — the mechanism §22 asks for ("se o usuário digitar rapidamente, buscas
   anteriores não precisam terminar"). Milestone 1 uses this for folder loading; Milestone
   2 uses the same handle-replacement pattern for completion requests.

---

## 5. State management

gpui's `Entity<T>` is the unit of shared, observable state. It is reference-counted,
mutated only through `update`, and repaints observers on `cx.notify()`.

```
Entity<WorkspaceView>          the window root — owns the layout and the keymap context
├── FileTree                   plain struct: lazily-read, flattened rows
├── Vec<Entity<EditorView>>    one per open tab
│     └── Document             plain struct: buffer + syntax + cursor
└── Option<Entity<Palette>>    Some only while open — an overlay is state, not a flag
```

Two decisions worth stating:

**Plain structs unless sharing demands otherwise.** `Document` and `FileTree` are ordinary
Rust values owned by their view, not entities. An `Entity` costs indirection and a borrow
discipline that only pays for itself when something is shared or observed across views.
`EditorView` is an entity because tabs share and swap it; `Document` is not, because
exactly one view owns it.

**No global mutable state.** No singletons, no `static mut`, no God Object (§27). The
`CommandRegistry` is built at startup and read-only thereafter. Everything else hangs off
the window root, which means two windows in a later milestone are two independent trees,
not a coordination problem.

---

## 6. Performance and benchmark strategy

Targets from §21, treated as engineering budgets rather than aspirations:

| Metric            | Target     |
| ----------------- | ---------- |
| Cold startup      | < 500 ms   |
| Warm startup      | < 150 ms   |
| Cached completion | < 50 ms    |
| Idle RAM          | 100–200 MB |

### How they are defended architecturally

- **Startup**: nothing scans the project. `FileTree::new` reads exactly one directory
  level; expanding reads one more. Opening a Laravel root does not stat `vendor/`.
- **Frame time**: `uniform_list` for both lists; highlight spans computed per visible byte
  range.
- **Input latency**: rope splice + incremental reparse, both bounded by edit size.
- **RAM**: undo stores inverse edits, not snapshots; no parse tree for files without a
  grammar.

### How they are measured

`benchmarks/` holds Criterion benches for the pure-Rust layers, where a regression is
unambiguous and CI-comparable:

- `text`: insert/delete at document start, middle and end, across 1 KB → 10 MB buffers.
- `syntax`: cold parse vs incremental reparse after a one-character edit; highlight
  extraction for an 80-row window.
- `workspace`: `FileTree::new` and expand on a synthetic Laravel-shaped tree.

Startup and frame time are measured in-app with `tracing` spans rather than guessed at.

§21's other instruction is followed literally: **no optimisation without a profile, and no
benchmark that measures something other than what it claims.** Where a shortcut has a
known ceiling, the code says so in a `ponytail:` comment naming the upgrade path — for
instance, reparsing from a full text copy instead of reading the rope through a callback.
That is a measured decision to revisit, not an oversight.

---

## 7. Fault isolation

§24 is a dependency-graph property, not a `try/catch` policy:

- A malformed `.gitignore` logs and is skipped; the folder still opens.
- An unreadable directory entry is skipped; the listing still renders.
- A file with no grammar gets no parse tree, and the editor still edits it.
- A binary or over-large file is refused with a clear message instead of freezing the UI.
- Saving writes a temp file and renames over the target, so a crash mid-write cannot
  truncate the user's source.

`panic!` is not used for recoverable errors (§26). `unwrap` appears only where the
invariant is local and provable, with the reason stated.
