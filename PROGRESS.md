# Progress — autonomous loop (2026-08-11)

Written per the owner's instruction: keep looping through issues by milestone and
record everything before context runs out. `CONTEXT.md` holds the durable lessons;
this file is the session ledger.

## Merged to main this loop

| PR | What |
|---|---|
| #128 | The completion saga: LSP starts (folder/file/Finder), PATH+shebang fixes, doc resync, popup height — closes #123 #125 #126 #127 |
| #129 | Terminal ⌘-click links; definition lands on the identifier (UTF-16); ⌘-hover underline+hand — closes #70 |
| #130 | Settings panel (⌘,) + release-config resolution — closes #57 #100 |
| #131 | Multi-cursor stage 1: ⌘D, type-everywhere, Escape |
| #132 | ⌥click carets; multi-cursor ⌘C/⌘X; CONTEXT refresh |
| #133 | ⌥-drag column selection |
| #134 | Stage 2: motions move every cursor, collisions merge |
| #135 | Blade/Volt templates stop rendering one-colour (HTML lexer in text regions) |
| #136 | Tree modified tint+●, themed titlebar |
| #137 | `ellefuanti .` detaches like `code .`; titlebar strip fixes traffic-light collision |

## On branch `loop/autonomous` (pushed, unmerged)

- **#53**: Rust + Markdown grammars, curated queries, gate 17→19MB with per-grammar
  cost attributed in `perf-gate.sh`. Titlebar strip reads "ellefuanti".
- **#64 items 3–4**: stage/unstage (git2, index-only, no confirm needed) + commit via
  **CLI** (hooks run; refusal stderr reaches the user; box empties only on success).
  Tested against real repositories including a refusing pre-commit hook.

## Issues closed this loop (with evidence in each)

#47 #48 #49 #50 #53 #54 #58 #60 #62 #69 #71 #81 #125 — plus #57 #70 #100 #123 #126
#127 via PR merges. #112 got a written decision (two-layer verification: debug_bounds
for boxes, owner-eyes for ink) and stays open for the owner's ack.

## Open, with state

- **#64** — items 1–4 shipped; **item 5 (push/pull/branches/stash) deliberately
  unbuilt** behind its danger note.
- **#82** — multi-cursor essentially complete (⌘D/⌥click/column/motions/copy);
  **folding remains**, and the issue's own warning stands: it breaks the
  uniform_list row↔line mapping, "worth doing carefully or not at all".
- **#83/#23** — route palette + navigation + route-name completion shipped earlier;
  Artisan-through-palette not started.
- **#21 → #22 → #20** — the Milestone-3 keystone chain (SQLite Laravel index →
  Eloquent completion → merge engine). Not started; biggest open value.
- **#65** — DB viewer; the ADR-0007 conflict has an obvious resolution nobody has
  written down: **rusqlite is already in the tree (elle-index) and is synchronous** —
  SQLx was the wrong question. Needs a decision note + design.
- **#99** — AI chat; #29/#28/#30/#31/#24–26 — scope items per RISKS #6.
- **#35** — the owner's two-minute eyes check; steps updated during the sessions.
- **#63** — Linux, deferred by design.

## Verification state

~1150 tests, 37 suites green (one PTY test load-flaky under full suite, solo-green —
pre-existing). Clippy clean of new warnings. Binary 17.64MB under the new 19MB gate.
Every new guarantee this loop was mutation-verified; three vacuous tests were caught
by mutation and either strengthened (extras-collide, Volt fixture) or deleted with
the reason recorded (row-height ink).

## The recurring traps, counted

- `/var` vs `/private/var` spelling: **4 appearances** (tabs-on-delete, rename
  retarget, and now git stage). Assume differing spellings whenever two paths meet.
- Measurement blamed before code: **3** (perf #79 history, detach pty harness, and
  the flatten/fixture mismatch in the Blade mutation run).
- flex_1-in-non-flex-parent zero-height: **3** (popup, tree wrapper near-miss, —
  guarded by two debug_bounds tests now).

## For whoever continues

Branch `loop/autonomous` is push-ready for a PR. The highest-value next moves, in
order: **#21** (unblocks two issues and is the milestone), **#82 folding** (careful),
**#65 decision note**, **Artisan palette** (#23). The app is installed at
`~/.local/bin/ellefuanti` and detaches like `code .`; debug with
`ELLE_FOREGROUND=1 ellefuanti . > log 2>&1`.
