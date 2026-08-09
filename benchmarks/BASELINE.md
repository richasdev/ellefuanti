# Performance baseline

Recorded 2026-08-09 on an Apple Silicon Mac, `cargo bench` (release, `lto = "thin"`).

These are the numbers later work regresses against. **Nothing is optimised without a
profile**, and no benchmark here claims to measure something it does not — where a number
is dominated by something other than our code, that is stated.

Reproduce with:

```sh
cargo bench -p elle-benchmarks
```

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

## Not yet measured

The §21 targets that need the full app, not a microbenchmark:

| Metric            | Target     | Status                                           |
| ----------------- | ---------- | ------------------------------------------------ |
| Cold startup      | < 500 ms   | not measured                                     |
| Warm startup      | < 150 ms   | not measured                                     |
| Idle RAM          | 100–200 MB | **~65 MB** observed at idle with no project open |
| Frame time        | < 8.3 ms   | not measured                                     |
| Cached completion | < 50 ms    | no completion engine yet (Milestone 2)           |

Startup and frame time need `tracing` span instrumentation in the app — the remainder of
issue #16. Watch item: `runtime_shaders` compiles Metal shaders at process start and is the
prime suspect if cold start misses budget. Measure before blaming it.
