# Performance baseline

Recorded 2026-08-09 on an Apple Silicon Mac, `cargo bench` (release, `lto = "thin"`).
Idle RSS, binary size and startup re-measured 2026-08-10 on `0d39a99` (#84).

These are the numbers later work regresses against. **Nothing is optimised without a
profile**, and no benchmark here claims to measure something it does not — where a number
is dominated by something other than our code, that is stated.

Reproduce with:

```sh
cargo bench -p elle-benchmarks   # the microbenchmarks below
scripts/perf-gate.sh             # idle RSS and binary size, and it fails if they regress
```

The application-level numbers (idle RSS, binary size, startup) were re-measured on `0d39a99`
and are **gated** — see [Idle memory is now gated](#idle-memory-is-now-gated--scriptsperf-gatesh-84).
The rest of this file is still recorded-but-unenforced, which is how the idle figure managed
to be wrong by 50% for twenty PRs.

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

### Query-driven languages (JSON, JS, TS, CSS, HTML, TOML, YAML, Shell)

PHP and Blade are highlighted by a hand-written tree walk; the languages added in #53 go
through a `highlights.scm` query and a `QueryCursor`. Different mechanism, so the viewport
claim is re-measured rather than inherited — `set_byte_range` pruning the tree is exactly
the sort of "the API should do the right thing" assumption this file exists to distrust.

Measured with a **fixed byte window** (15 identical units, same text in every fixture)
while the file behind it grows 100× — 40 / 400 / 4000 units:

| Language   | small        | medium       | large        | growth    |
| ---------- | ------------ | ------------ | ------------ | --------- |
| JSON       | **39.7 µs**  | **41.2 µs**  | **42.1 µs**  | **1.06×** |
| JavaScript | **135.3 µs** | **136.4 µs** | **142.9 µs** | **1.06×** |
| TypeScript | **184.5 µs** | **185.6 µs** | **188.6 µs** | **1.02×** |
| CSS        | **53.0 µs**  | **53.4 µs**  | **53.4 µs**  | **1.01×** |
| HTML       | **54.2 µs**  | **55.2 µs**  | **55.5 µs**  | **1.02×** |
| TOML       | **44.5 µs**  | **45.7 µs**  | **46.6 µs**  | **1.05×** |
| YAML       | **49.9 µs**  | **52.1 µs**  | **53.0 µs**  | **1.06×** |
| Shell      | **74.0 µs**  | **75.7 µs**  | **76.0 µs**  | **1.03×** |

**Every language is flat**, which is the constraint. The absolute numbers vary by query
size rather than by file size: TypeScript is the JavaScript query concatenated with its
own, so it is the largest query and the slowest, and JSON's five patterns are the
cheapest. A standalone probe over the same PHP fixture put 40 patterns at 87 µs and 5 at
57 µs, so pattern count is the cost driver.

The four added in the second batch confirm that prediction rather than merely re-passing
the test. HTML, TOML and YAML have small queries and land where JSON and CSS do; Shell is
the slowest of the four at 76 µs, and its query is also the largest of the four. Nothing
here approaches TypeScript, and against an 8.3 ms frame budget the worst of them is ~0.9%.

**The query path is ~2.7× more expensive than the hand-written walk** (87 µs against
32 µs, interleaved A/B in one process over the same PHP fixture). Flatness is the
constraint and it holds; the constant factor is the price of not writing a `node_style`
function per language. Against an 8.3 ms frame budget, TypeScript's 183 µs is ~2%.

#### A benchmark that was wrong, again

The first version of this bench reported **22 → 189 → 355 µs** for JSON across the size
range — a 16× growth that looked exactly like the file-size regression the bench exists to
catch, and would have been a genuine blocker for the whole approach.

It was the measurement. The window was 80 _rows_ clamped with `.min(rows - 1)`, so the
small fixture got a much narrower window than the large one, and the numbers were tracking
**how much was on screen**, not how big the file was. Holding the window to a fixed byte
range with identical content gives **22.8 / 23.7 / 24.4 µs** over the same 100× range.

Two hypotheses were tested and discarded before the harness was suspected — that JSON's
flat root object defeats the pruning (a nested fixture of the same size grew the same way,
so no), and that the query engine ignores `set_byte_range` (a standalone probe measured
1.00× across 100×, so no). **The third thing to suspect should have been the first**,
which is what this section of the file has now said three times.

### Binary size: what eight grammars cost

Each tree-sitter grammar is compiled C, and its parse tables are static data that link in
whether or not anyone opens that file type. #53 asked for this to be measured rather than
assumed, and it is not negligible:

| Release binary                    | Size                |
| --------------------------------- | ------------------- |
| before (PHP only)                 | **7.63 MB**         |
| after #56 (+ JSON/JS/TS/CSS)      | **9.64 MB**         |
| after #53 (+ HTML/TOML/YAML/bash) | **11.21 MB**        |
| **delta, PHP-only to now**        | **+3.58 MB (+47%)** |

The second batch added **+1.57 MB for four grammars**, and it is nothing like evenly
spread. Compiled rlib size, which is what drives it:

| Grammar | rlib        |
| ------- | ----------- |
| bash    | **1.52 MB** |
| YAML    | 232 KB      |
| TOML    | 52 KB       |
| HTML    | 45 KB       |

**Shell is the whole cost.** It is 34× TOML and accounts for ~95% of the batch on its own —
bash's grammar carries word expansion, here-documents and a hand-written external scanner,
so it is genuinely that big rather than badly built. HTML and TOML are close to free.

This also corrects a prediction made here after #56: that the remaining six languages in
#53 would add "3–4 MB". Four of them added 1.57 MB, and one grammar is most of it. The
useful generalisation is not "≈500 KB per grammar" — that average was an artefact of
TypeScript and PHP, which are the two largest in the tree. **Grammar size varies by more
than 30× and should be looked up, not extrapolated.**

The standing question is unchanged: no mechanism exists to leave a grammar out of the
build, and none was added here. At 11.2 MB it is not yet a problem worth building one for.

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

## Find in file — measured out of tree (#80)

Recorded 2026-08-10. Not a `cargo bench` target: the search lives in `crates/app`, which is
a **binary** crate, so `elle-benchmarks` cannot import it. The numbers below come from a
standalone release binary reproducing `Matches::new` exactly — `Rope::to_string`, then the
same `Regex::new("(?im)…")` and `find_iter` the real code runs. Medians of 60 runs on
Laravel-shaped PHP, Apple Silicon.

The claim under test: **a rescan on every keystroke in the find field fits inside a frame.**

| file             | query                             |      total | of which scan |
| ---------------- | --------------------------------- | ---------: | ------------: |
| 33 KB / 1k lines | `$user` — a hit on every line     |     120 µs |        106 µs |
| 349 KB / 10k     | `$user` — a hit on every line     |     389 µs |        334 µs |
| 1.8 MB / 50k     | `$user` — a hit on every line     | **1.8 ms** |       1.64 ms |
| 1.8 MB / 50k     | `zzz` — no hits                   |     235 µs |        148 µs |
| 1.8 MB / 50k     | `\$user\d+` regex, hit every line | **2.4 ms** |       2.22 ms |

**Verdict: it fits, with less headroom than expected.** The 50k-line worst case is a fifth
of the 8.3 ms frame budget, on the UI thread, once per keystroke _in the find field_.
Typing in the document itself is unaffected — the rescan there is guarded by a buffer
version comparison. But "search is free" would be false, and the next change that makes it
slower should re-measure rather than assume the room is there.

**The cost tracks the hit count, not the file size.** The same 1.8 MB file with no matches
costs 235 µs, 7.5× less. That is the shape of `find_iter` doing work per match, not per
byte, and it is why the pathological case is a one-character query on a large file
(1.1 ms) rather than a large file as such.

Two components measured separately because the total disagreed with the parts at 1k lines,
which is the disagreement this file's opening section says to chase:

- `Rope::to_string` on 1.8 MB: **74 µs**. Real, not dominant.
- `Regex::new`: **33 µs** literal, **58 µs** for a real pattern. Also real, also not
  dominant — but it is a third of the whole cost at 1k lines, which is why caching the
  compiled regex is named in `editor/find.rs` as the first optimisation if one is needed.
- Dropping the `Regex` and the `String`: **~1 µs**. Not the #27 teardown trap again.

At 50k lines the parts add up (74 + 1640 + 35 ≈ 1749 against a 1756 total), so the
measurement is coherent where it matters. At 1k lines they did not, and the gap is
allocation noise at a scale where the whole operation is 120 µs — small enough that
chasing it further would be optimising against a number rather than a user.

`MAX_SEARCH_BYTES` (4 MB) is derived from this table, not chosen for roundness: ~1 ms per
megabyte of hit-dense text puts 4 MB at about half a frame, and past that the find bar says
"File too large to search" rather than dropping frames silently.

**Per-frame cost is separate and is _not_ in this table**, because it is asserted
structurally instead: `Matches::in_range` is two binary searches over a sorted list, pinned
by `match_lookup_cost_does_not_grow_with_file_size` in `crates/app/src/editor/find.rs` —
the search counterpart to `viewport_cost_does_not_grow_with_file_size` in `crates/syntax`.
A wall-clock assertion there would be a flaky test that teaches nothing when it fails.

---

## Application — measured with `ELLE_PERF=1`

Release build, measured from process entry (`ELLE_PERF=1 ./target/release/ellefuanti`).

| Metric                                | Target (§21) | Measured                           | Verdict                   |
| ------------------------------------- | ------------ | ---------------------------------- | ------------------------- |
| Startup, first launch of a new binary | < 500 ms     | **520–536 ms**                     | ❌ worst case — see below |
| Startup, later launches               | < 500 ms     | **~195–380 ms**                    | ✅                        |
| Warm startup                          | < 150 ms     | **191–213 ms**                     | ❌ **~50 ms over**        |
| Idle memory (no project open)         | 100–200 MB   | **76–79 MB** footprint             | ✅ gated at 95 MB         |
| Idle CPU                              | —            | **0.75%**, gpui's display link     | ✅ gated at 2%            |
| Frame render, 55k-line file           | < 8.3 ms     | **0.08–0.77 ms**                   | ✅                        |
| Keystroke → pixel, 55k-line file      | < 8.3 ms     | **2.6–5.3 ms** †                   | ✅                        |
| Sustained typing, 55k-line file       | < 8.3 ms     | **~4.3 ms** †                      | ✅                        |
| Cached completion                     | < 50 ms      | no completion engine (Milestone 2) | —                         |

**The idle memory and CPU rows changed metric, not value.** They previously read 69 MB and
0.0%, taken with `ps rss` and `ps %cpu`. Both were wrong to gate on, and #79 was opened,
argued for three rounds and closed on the strength of them:

- `ps rss` varies 83.9–102.5 MB across five launches of **one unchanged binary**, while
  `vmmap` Physical footprint holds at 75.7–78.8 MB. The rss spread is wider than the
  "regression" #79 reported.
- `ps %cpu` is a **decaying lifetime average**, not a rate. Sampled once after a settle it
  reports where you happen to be on the decay curve — which is why the same build read 8.7%,
  then 0.0%, then 0.7% across the session with nothing changing. The commit that recorded
  **0.0% here has never been below 0.7%** when measured as a cumulative-time delta.

The real ~0.75% is gpui's `CVDisplayLink`, started after the first paint and never stopped.
Upstream, not ours, which is why the gate sits at 2% rather than near zero.

Both are now enforced by `scripts/perf-gate.sh`, so the next drift is caught rather than
noticed by accident.

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

### Idle memory is now gated — `scripts/perf-gate.sh` (#84)

The idle-RAM row above read **69 MB** until this commit, while the real number had reached
105 MB. It drifted across roughly twenty PRs, each of which measured its own viewport cost
and none of which measured the aggregate (#79). **Per-feature measurement does not catch
aggregate drift**, so the aggregate now has its own check:

```sh
scripts/perf-gate.sh            # measures an existing release binary
scripts/perf-gate.sh --build    # builds it first
```

Re-measured on `0d39a99`, release build, no project open, machine otherwise quiet:

| Metric      | Measured                    | Limit  | Blocking |
| ----------- | --------------------------- | ------ | -------- |
| Idle RSS    | **100–103 MB**              | 125 MB | yes      |
| Binary size | **14.53 MB** (15,236,048 B) | 17 MB  | yes      |
| Startup     | 216–737 ms                  | —      | **no**   |

**Why those two block and startup does not.** Across six runs of the same binary, idle RSS
stayed inside 100–103 MB and the binary is byte-identical every time, while startup measured
737 ms on the first launch of the freshly-linked binary and 216–236 ms on every launch after
— a **3.4× move with no code change**, purely dyld and page-cache state. Gating that would
produce a check that fails on the first CI run after every build and passes thereafter, which
is the definition of a gate people learn to ignore. It is printed on every run instead.

The limits sit ~20% above the measured value. Tighter would flap; looser would let another
#79 through. They are ceilings to hold, not targets to grow into — a change that legitimately
needs more should raise the number in the same commit, so it is a decision with a reviewer
rather than a silent drift.

**One measurement disagreed, and per the top of this file that is worth recording rather than
averaging away.** One run of six read **81.8 MB** against a 100–103 MB cluster. It was taken
as a sibling build was spinning up, and the reading is a ~20% _under_-count, so the plausible
story is that it sampled before the process had finished allocating — a machine under load
takes longer to settle, and 20 s is tuned for a quiet one. That is unconfirmed. What follows
from it is already in the script: it refuses to run at all while a compiler is running, since
the first RSS figure taken for #79 was wrong for exactly that reason, and it samples three
times over 30 s and keeps the **worst** rather than the mean — an under-count is the failure
mode that would let a regression pass, so averaging it in is the one thing not to do.

The startup line is also why the gate cannot be trusted from a headless CI runner: with no
display the window never opens, so the process measured is not the one a user runs and its
RSS omits whatever the window allocates. CI runs it as a floor; the local run is authoritative.

Startup phase breakdown:

| Phase           | Cold       | Warm        | CI (headless) |
| --------------- | ---------- | ----------- | ------------- |
| logging init    | 0.3 ms     | 0.3 ms      | 0.2 ms        |
| gpui init       | 87 ms      | ~38 ms      | 92 ms         |
| keymap          | 0.2 ms     | 0.2 ms      | 0.5 ms        |
| **window open** | **432 ms** | **152 ms**  | **765 ms** ⚠  |
| **total**       | **520 ms** | **~200 ms** | **858 ms** ⚠  |

The CI column comes from the `release-build` job (`ELLE_PERF=1` on the precompiled-shader
binary) and is **reported but not trusted**. A GitHub macOS runner has no display, so window
creation there takes a path no user ever exercises — 765 ms against 432 ms locally, on the same
build. What the column is good for is the phases _before_ the window, which match the local
figures closely and are therefore display-independent.

It is wired up (rather than left to a human with Xcode) so the figure appears on every run and
a regression in the early phases would be visible. The step is `continue-on-error` and cannot
fail the build, precisely because its headline number is untrustworthy.

### Where the warm-startup miss actually is

The original `gpui_init` phase was measured _inside_ `Application::run`'s closure, so it lumped
platform construction together with starting the event loop — the kind of label that sends
someone optimising the wrong half. Split apart, a warm launch decomposes as:

| Phase              | Warm       | What it is                                                      |
| ------------------ | ---------- | --------------------------------------------------------------- |
| logging init       | 0.05 ms    | our `tracing_subscriber` setup                                  |
| `platform_init`    | **48 ms**  | `Application::new()` — NSApplication, Metal device, text system |
| `event_loop_start` | **16 ms**  | `run()` reaching our callback                                   |
| keymap             | 0.1 ms     | our action and keybinding registration                          |
| `window` open      | **152 ms** | `open_window` plus first paint                                  |
| **total**          | **216 ms** |                                                                 |

**Effectively all of it is inside gpui.** Our own code — logging setup plus the keymap — is
under 0.2 ms combined. No application-level change moves the 150 ms target; it would take
either a change in how gpui initialises the platform and opens a window, or deferring window
creation until after something the user can see, which trades a real property (the window
appears immediately) for a better number.

The miss is therefore recorded as **attributed but not actionable at this layer** — a more
useful state than "unattributed", and a reason not to keep poking at it. Revisit if gpui's
startup path changes.

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

### Indent guides and trailing whitespace (#82)

The per-row decorations added in #82, measured on the same 55k-line PHP file, 80 visible
rows:

| Arm                                 | Time       |
| ----------------------------------- | ---------- |
| `frame_80_rows/decorations/without` | 79.3 µs    |
| `frame_80_rows/decorations/with`    | 80.4 µs    |
| `frame_80_rows/decorations/alone`   | **228 ns** |
| `decorations_80_rows/1k_lines`      | 232 ns     |
| `decorations_80_rows/55k_lines`     | 229 ns     |

**The number to quote is 228 ns for 80 rows — ~2.9 ns per row, 0.003% of the 8.3 ms budget.**

The A/B arms are reported but should **not** be subtracted from each other, and that is the
interesting part. `frame_at` costs ~80 µs with a run-to-run spread of several µs, which is
an order of magnitude larger than the thing under test; across four runs the `with` arm read
80.4, 80.7, 90.8 and 95.5 µs while `without` stayed at 79–81 µs. Taking the difference on
any single run yields anything from ~1 µs to ~15 µs, all of it noise wearing a plausible
sign. This is the failure mode the top of this file describes, met head on: two runs of
identical code disagreed, so the disagreement is a fact about the harness. The `alone` arm
exists because it is the only one sharp enough to answer the question, and it is stable to
within a few ns across runs.

The last two rows are the property that actually matters, and the one
`viewport_cost_does_not_grow_with_file_size` (#52) guards for highlighting: **a 55× larger
file costs the same 230 ns**, because the pass reads only the 80 lines already sliced for
the frame and never touches the buffer. A regression making guides per-file rather than
per-viewport would show up as those two columns diverging and nowhere else.

### Frame timing in the running app

Instrumented as worst-of-120-frames against the 8.3 ms budget (`ELLE_PERF=1`).

The microbenchmarks above cover the domain-layer work per frame, and it is comfortably
inside budget. What remains unmeasured is gpui's own layout and GPU submission — the part
this bench deliberately excludes, so that a fast number here cannot be mistaken for proof
that scrolling feels smooth. Exercising that needs someone to actually scroll a large file in
the running app, which is the remaining part of issue #10.
