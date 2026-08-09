# Milestone 1 — Editor

Deliverable 9 of §28. The whole milestone exists to make one flow excellent:

```
cargo run → native macOS window → open folder → file tree
          → open text file → edit → save
```

Nothing from Laravel, Database, Docker or Git is built until this is solid (§28).

Tasks are small and independently verifiable. Each states its **done** condition, because
"implement the editor" is not a task anyone can finish.

---

## Status

| #   | Task                                | State                               |
| --- | ----------------------------------- | ----------------------------------- |
| 1   | Cargo workspace and build baseline  | ✅ done                             |
| 2   | Rope text buffer with undo/redo     | ✅ done                             |
| 3   | Command registry and palette search | ✅ done                             |
| 4   | Lazy file tree                      | ✅ done                             |
| 5   | Safe file read/write                | ✅ done                             |
| 6   | Tree-sitter incremental parsing     | ✅ done                             |
| 7   | Syntax highlighting (PHP + Blade)   | ✅ done                             |
| 8   | Cursor, selection and motion        | ✅ done                             |
| 9   | GPUI window and workspace shell     | 🟡 built, unverified visually       |
| 10  | Virtualised editor view             | 🟡 built, unverified visually       |
| 11  | Keyboard and text input             | 🟡 built, unverified visually       |
| 12  | Tabs and multiple open files        | 🟡 built, unverified visually       |
| 13  | Command palette and quick open UI   | 🟡 built, unverified visually       |
| 14  | Native open-folder dialog           | 🟡 built, unverified visually       |
| 15  | Save, dirty state and status bar    | 🟡 built, unverified visually       |
| 16  | Benchmarks                          | ✅ done                             |
| 17  | Basic terminal panel                | ✅ done (multi-session, via PR #34) |

Tasks 9–15 share one blocker: **the UI has never been seen on a screen**
([#35](https://github.com/richasdev/ellefuanti/issues/35)). Everything is implemented,
unit-tested at the logic layer, and gated by CI; what is missing is a human confirming the
pixels. That is also the main blocker for the
[0.1.0 release milestone](https://github.com/richasdev/ellefuanti/milestone/9), which now
tracks these tasks.

---

## 1. Cargo workspace and build baseline ✅

Five crates, not eighteen (§4 explicitly warns against empty crates). Workspace-level
dependency versions so two crates cannot disagree. `opt-level = 1` in dev because an
unoptimised rope plus tree-sitter is too slow to _feel_ the product while building it.

**Done:** `cargo build` succeeds from a clean checkout with only Command Line Tools
installed — no full Xcode. (This is why `runtime_shaders` is enabled; see ADR-0002.)

## 2. Rope text buffer with undo/redo ✅

`elle-text`: ropey-backed buffer, byte-offset public API, inverse-edit undo history,
intent-based coalescing, and an edit log for incremental consumers.

**Done:** 18 tests, including multibyte round-trips, coalescing boundaries, and
redo-invalidation.

## 3. Command registry and palette search ✅

`elle-core`: stable dotted command ids, registration with override, subsequence search
ranked by match tightness. Metadata only — dispatch rides gpui's action system rather than
a second dispatcher (§5's requirement is that _one_ path exists, not that we build our own).

**Done:** 5 tests; `dispatch_for` has a test asserting every registered command has a
handler, so adding a palette entry without wiring it is a test failure.

## 4. Lazy file tree ✅

`elle-workspace`: reads one directory level at a time, flattened for virtualised rendering,
`.gitignore`-aware, `.git` always hidden, directories before files.

**Done:** 7 tests. Opening a Laravel root does not stat `vendor/` — the property that makes
§13's startup budget reachable.

## 5. Safe file read/write ✅

UTF-8 only, BOM stripped, binary refused by NUL heuristic, 64 MB ceiling, and saves via
temp-file-plus-rename so a crash mid-write cannot truncate the user's source. Permissions
preserved.

**Done:** 7 tests, including "write failure does not destroy the original" and "save keeps
the executable bit".

## 6. Tree-sitter incremental parsing ✅

`elle-syntax`: edits replayed as `InputEdit`s, reparse reusing the previous tree.

**Done:** the load-bearing test asserts an incrementally-updated tree is byte-identical to
a cold parse of the same text, across insert, multiline delete, and multibyte edits.

## 7. Syntax highlighting (PHP + Blade) ✅

Node-kind mapping (ADR-0005) plus a Blade directive scanner (ADR-0006). Computed per
visible byte range, subtrees pruned, overlaps resolved outermost-wins.

**Done:** 8 tests, including a regression test for the nested-node bug that shrank `$name`
to `$`, and one asserting a viewport query returns strictly fewer spans than a whole-file
query.

## 8. Cursor, selection and motion ✅

`Document` in the app crate but gpui-free: character-wise motion that respects UTF-8,
goal-column memory for vertical movement, selection collapse-to-edge, select-all,
undo/redo cursor placement, trailing-newline preservation on save.

**Done:** 16 tests.

---

## 9. GPUI window and workspace shell ⬜

Native window; `WorkspaceView` root holding sidebar, tab bar, editor area, status bar.
Activity-bar rail present with Explorer active and later panels visibly disabled rather
than absent, so the shape of the product is legible from day one (§6).

**Done when:** `cargo run` opens a window with the full chrome and no panics on resize.

## 10. Virtualised editor view ⬜

`uniform_list` over rows; gutter with line numbers; per-line styled text runs from the
highlighter; cursor and selection painted; scroll position tracked.

**Done when:** a 50k-line file scrolls at frame rate and only visible rows are laid out.

**Watch:** this is where the §21 frame-time budget is won or lost. Instrument before
optimising.

## 11. Keyboard and text input ⬜

Character insertion, the motion and editing keymap from `actions.rs`, and focus wired so
context-scoped bindings fire (`enter` in the palette must confirm; `enter` in the editor
must insert a newline).

**Done when:** typing, arrows, shift-selection, backspace/delete, undo/redo and cmd-A all
behave, including on a multibyte line.

**Deliberately excluded:** IME and dead-key composition. That is a full input-handler
implementation, and it needs its own task once basic input is proven.

## 12. Tabs and multiple open files ⬜

One `Entity<EditorView>` per open file; click to activate, close button, dirty dot,
re-activate rather than reopen an already-open path.

**Done when:** several files can be open, switched between, and closed with cursor position
per tab preserved.

## 13. Command palette and quick open UI ⬜

Overlay driven by `CommandRegistry::search`; quick open reuses the same overlay over file
paths. `cmd-shift-p` and `cmd-p`.

**Done when:** both open, filter as you type, navigate with arrows, confirm with enter,
dismiss with escape, and every builtin command actually runs.

**Note:** quick open needs a file list, which is the first thing that wants a _recursive_
walk. It must run on the background executor and be cancellable — the first real exercise
of ADR-0007's cancellation pattern.

## 14. Native open-folder dialog ⬜

`cmd-O` → native macOS picker → `FileTree::new` on the background executor → tree renders.

**Done when:** opening a real Laravel project shows its tree and the window title updates,
with the UI never blocking during the load.

## 15. Save, dirty state and status bar ⬜

`cmd-S` writes via `write_file` off the UI thread; dirty state clears on success and
surfaces an error without losing the buffer on failure. Status bar shows path, cursor
position, language and dirty state.

**Done when:** an edited file round-trips to disk byte-exactly, including its original
trailing-newline behaviour, and a failed save leaves the buffer intact with a visible error.

## 16. Benchmarks 🟡

Criterion benches under `benchmarks/` for the pure layers: buffer edits across 1 KB → 10 MB,
cold vs incremental parse, highlight extraction for an 80-row window, file-tree open and
expand. Baseline recorded in [benchmarks/BASELINE.md](../benchmarks/BASELINE.md).

**Done:** all three suites, and the baseline is in the repo so later work has something to
regress against.

**It immediately earned its cost.** `highlights` for a fixed viewport measured 50 / 60 /
156 µs across a 100× file-size range — growing with file size, contradicting the viewport
claim in ARCHITECTURE.md. The tree walk iterated every child of every node, so the root's
child list (one entry per top-level declaration) was scanned in full on every frame.
Replacing the sibling scan with `goto_first_child_for_byte` made it flat (46.4 / 46.7 /
46.6 µs) and 3.4× faster on the large file. A unit test now pins the property.

**Remaining:** startup and frame time need `tracing` span instrumentation in the app; those
are the two §21 targets a microbenchmark cannot reach.

## 17. Basic terminal panel ⬜ deferred

§25 lists a basic terminal in Milestone 1. It is sequenced **last** and may slip to
Milestone 2 without blocking anything, because a terminal is a PTY plus an ANSI state
machine plus a grid renderer — a substantial subsystem whose absence does not weaken the
open-edit-save flow this milestone exists to prove.

**Recorded as a deliberate deviation from §25** rather than an omission. If the flow above
is not excellent, a terminal does not save it.
