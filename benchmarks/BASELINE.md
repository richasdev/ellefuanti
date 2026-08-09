# Performance baseline

Recorded 2026-08-09 on an Apple Silicon Mac, `cargo bench` (release, `lto = "thin"`).

These are the numbers later work regresses against. **Nothing is optimised without a
profile**, and no benchmark here claims to measure something it does not — where a number
is dominated by something other than our code, that is stated.

Reproduce with:

```sh
cargo bench -p elle-benchmarks
```

## How to read a disagreement

The most expensive mistake made in this file's history was not a slow function — it was
**trusting a benchmark**. A reported 24.8 ms per keystroke (#27) turned out to be criterion
charging destructor time to every sample; the code was already near budget.

What surfaced it was not profiling. It was noticing that two measurements of the same thing
disagreed, and treating that disagreement as evidence about the **harness** rather than as
noise to average away. Twice:

1. A fresh reading of 62.6 ms contradicted the recorded 24.8 ms → the harness was suspect →
   found the teardown being timed.
2. Two runs of identical code differed by 2× → absolute numbers here are not comparable
   across runs → built an interleaved A/B in one process, which is the only figure worth
   quoting for a before/after.

So: **when a number surprises you, suspect the measurement before the code.** If two runs of
the same code disagree, that is a fact about the measurement, and averaging it away destroys
the only signal you had. Optimising against a number you have not interrogated is how a
codebase acquires complexity that buys nothing.

---

## Text buffer — `benches/text.rs`

The claim under test: **edit cost does not grow with file size.** A rope is only worth its
complexity if that holds.

| Operation              | 1 KB    | 100 KB  | 1 MB    | 10 MB   |
| ---------------------- | ------- | ------- | ------- | ------- |
| insert at start        | 271 ns  | 722 ns  | 813 ns  | 1.31 µs |
| insert at middle       | 256 ns  | 910 ns  | 1.31 µs | 1.50 µs |
| insert at end          | 500 ns  | 887 ns  | 1.14 µs | 1.46 µs |
| type 100 chars         | 37.8 µs | 66.1 µs | 78.1 µs | 75.8 µs |
| 50 × `point_to_offset` | 11.5 µs | 18.9 µs | 22.7 µs | 22.8 µs |

**Verdict: the rope is doing its job.** A 10,000× increase in file size costs ~5× in edit
time — logarithmic, as expected. A `String` buffer would memmove the document tail on every
insert, making the 10 MB column roughly four orders of magnitude worse.

Typing latency is the number the user feels: **0.76 µs per keystroke in a 10 MB file**, well
inside a 120 fps frame budget (8.3 ms).

`undo_1000_edits/1MB`: **283 µs** to unwind a thousand separate undo groups — inverse edits
rather than snapshots, so history depth is cheap.

## Syntax — `benches/syntax.rs`

Two claims: incremental reparse must beat a cold parse, and highlight cost must track the
viewport rather than the file.

|                                    | 10 classes  | 100 classes | 1000 classes |
| ---------------------------------- | ----------- | ----------- | ------------ |
| cold parse                         | 294 µs      | 2.90 ms     | 29.1 ms      |
| **incremental reparse, 1 char**    | **38.3 µs** | **64.0 µs** | **313 µs**   |
| **highlights, 80-row viewport**    | **46.4 µs** | **46.7 µs** | **46.6 µs**  |
| highlights, whole file _(control)_ | 97.9 µs     | 1.00 ms     | 11.0 ms      |

**Incremental reparse is 93× cheaper than a cold parse** at 1000 classes (313 µs vs 29 ms).
Edit replay is working.

**Viewport highlighting is flat** — 46.4 / 46.7 / 46.6 µs across a 100× size range. The
whole-file row is the control: it is the cost being avoided per frame, not a cost paid.

Blade viewport highlighting: **2.35 µs** (PHP tree walk plus the directive scanner).

### A regression this benchmark caught

The first implementation of `highlights` measured **50 / 60 / 156 µs** across those same
three sizes — growing with file size, which contradicted the viewport claim in the
architecture docs.

Cause: the tree walk iterated _every_ child of each node and range-checked it, so the
root's child list — one entry per top-level declaration — was scanned in full on every
frame. Finding the two visible classes in a 1000-class file cost a thousand comparisons.

Fix: seek with `TreeCursor::goto_first_child_for_byte`, which binary-searches a child list,
then walk forward only until past the range. Result: flat, and **3.4× faster** in the
1000-class case. Pinned by the `viewport_cost_does_not_grow_with_file_size` unit test so it
cannot silently return.

This is the entire argument for writing benchmarks before optimising: the walk _looked_
viewport-scoped, and the range check at the top of the function made it read as correct.

## Workspace — `benches/workspace.rs`

Fixture: a synthetic Laravel project with ~5000 files under `vendor/`.

| Operation                             | Time        |
| ------------------------------------- | ----------- |
| open root (5000 vendor files present) | **64.3 µs** |
| expand `app/Models` (40 files)        | 59.1 µs     |
| read 10 KB                            | 17.5 µs     |
| read 1 MB                             | 122 µs      |
| write 10 KB (atomic)                  | 5.83 ms     |
| write 1 MB (atomic)                   | 6.41 ms     |

**Opening a folder costs one directory level, not the project.** 64 µs with 5000 files
present is the property that makes the startup budget reachable.

**The atomic write number needs its caveat stated plainly.** ~5.8 ms is almost entirely
`fsync`, not our code — it is nearly identical for 10 KB and 1 MB, which is the signature of
a durability barrier rather than a data-volume cost. It buys the guarantee that a crash
mid-save cannot truncate the user's source, and it is off the UI thread. Do not "optimise"
it by dropping the `fsync`; that trades a user's file for milliseconds they never notice.

---

## Application — measured with `ELLE_PERF=1`

Release build, measured from process entry (`ELLE_PERF=1 ./target/release/ellefuanti`).

| Metric                           | Target (§21) | Measured                           | Verdict                   |
| -------------------------------- | ------------ | ---------------------------------- | ------------------------- |
| Cold startup (warm shader cache) | < 500 ms     | **380 ms**                         | ✅                        |
| Cold startup (cold shader cache) | < 500 ms     | **520 ms**                         | ❌ worst case — see below |
| Warm startup                     | < 150 ms     | **191–213 ms**                     | ❌ **~50 ms over**        |
| Idle RAM (no project open)       | 100–200 MB   | **69 MB**                          | ✅ well under             |
| Idle CPU                         | —            | **0.0%**                           | ✅                        |
| Frame render, 55k-line file      | < 8.3 ms     | **0.08–0.77 ms**                   | ✅                        |
| Keystroke → pixel, 55k-line file | < 8.3 ms     | **2.6–5.3 ms** †                   | ✅                        |
| Sustained typing, 55k-line file  | < 8.3 ms     | **~4.3 ms** †                      | ✅                        |
| Cached completion                | < 50 ms      | no completion engine (Milestone 2) | —                         |

**† Do not read these as precise figures.** The same code measured 5.28 ms on a loaded
machine and 2.65 ms on a quiet one — a 2× spread from machine conditions alone. If you run
this on a busy laptop and see ~5 ms, that is **not** a regression against the 2.65 ms above.

Absolute single-run numbers on this fixture are not comparable across runs, which is exactly
the trap that produced the bogus 24.8 ms in the first place, pointed the other way. The
durable claims are the **relative** ones, which held in both environments:

- the removed allocation cost ~1.55 ms/keystroke (~29% of the reparse), measured with both
  arms interleaved **in one process**
- `drop(SyntaxTree)` was 8–10 ms and was never editor work
- the reparse itself is ~2 ms, so the 8.3 ms budget has genuine headroom

To compare a change against this baseline, run an interleaved A/B in a single process. Do not
diff two separate `cargo bench` invocations.

Startup phase breakdown:

| Phase           | Cold       | Warm        |
| --------------- | ---------- | ----------- |
| logging init    | 0.3 ms     | 0.3 ms      |
| gpui init       | 87 ms      | ~38 ms      |
| keymap          | 0.2 ms     | 0.2 ms      |
| **window open** | **432 ms** | **152 ms**  |
| **total**       | **520 ms** | **~200 ms** |

### Startup: the shader diagnosis was wrong

The window phase dominates the first launch, and this document previously stated it **was**
Metal shader compilation from the `runtime_shaders` feature — named as the prime suspect in
ADR-0002 before being measured, then treated as confirmed once the numbers fit.

**They fit a theory that turned out to be false.** CI now builds the release binary with
`--no-default-features`, so its shaders are compiled at build time on a runner with Xcode.
Running that binary here:

| Precompiled-shader binary | Window phase | Total      |
| ------------------------- | ------------ | ---------- |
| first launch              | 496 ms       | **536 ms** |
| subsequent launches       | 152 ms       | ~195 ms    |

Same 152 ms warm window phase, same ~195 ms warm total, first launch still over budget.
**Precompiling the shaders changed nothing measurable**, on a binary whose shaders were
compiled hours earlier on another machine.

The first-launch cost is therefore OS-level first-launch overhead that any new Mach-O pays —
dyld closure construction, signature validation, page-in of a binary not yet cached — not
shader compilation. The release split is kept regardless (it is free and keeps the release
path exercised in CI), but it is a buildability convenience, not the startup fix it was
introduced as.

**The methodological point, since this is the second time in this file:** a mechanism that
plausibly explains the numbers is not the mechanism. The only way to tell is to remove the
suspected cause and re-measure. The first instance was a benchmark harness charging destructor
time to every sample; this one was a shader theory that survived because nobody had yet built
the binary that would refute it.

**Warm startup at ~200 ms against a 150 ms target** is the residual after shader caching,
and would need profiling of gpui init (38 ms) plus the remaining 152 ms of window setup to
attribute further. Not yet investigated — and per §21 it will not be optimised by guessing.

### The keystroke measurement that was wrong

`benches/frame.rs` originally reported **24.8 ms** for keystroke-to-pixel on a 55k-line file
and I filed it as a 3×-over-budget defect (#27). **Most of it was the benchmark's fault.**

`iter_batched` hands the fixture to the routine **by value**, so its destructors run inside
the timed region — and freeing a 1 MB file's tree-sitter node arena costs 8–10 ms. A real
editor never destroys the buffer and tree on each keystroke, so that cost does not exist in
the product. `iter_batched_ref` borrows instead, and drops after `measurement.end()`
(confirmed in criterion 0.7's `bencher.rs`).

The underlying allocation fix was real but secondary: an interleaved A/B in one process
measured it at **1.55 ms/keystroke (~29%)**, against the 24.6 ms originally attributed. It
was worth removing — it scaled with file size rather than edit size on the hot path — but
the honest story is that the alarming number was measurement error.

Two lessons, both worth more than the fix: a benchmark can be wrong in the _expensive_
direction and still look plausible, and single bench runs on a loaded machine were too noisy
to trust here (a pre-fix rerun read 2.74 ms, a post-fix one 5.28 ms — pure load drift).
Interleaved A/B in a single process is what produced a number worth believing.

### Frame timing in the running app

Instrumented as worst-of-120-frames against the 8.3 ms budget (`ELLE_PERF=1`).

The microbenchmarks above cover the domain-layer work per frame, and it is comfortably
inside budget. What remains unmeasured is gpui's own layout and GPU submission — the part
this bench deliberately excludes, so that a fast number here cannot be mistaken for proof
that scrolling feels smooth. Exercising that needs someone to actually scroll a large file in
the running app, which is the remaining part of issue #10.
