# Context

For whoever picks this up next with no memory of how it got here.

`CHANGELOG.md` records what shipped. This records **what is known** — the things that cost
several attempts to learn, and would otherwise be re-derived at the same price. It is not a
summary of the git log; anything you can get from `git log --oneline` in thirty seconds is
deliberately absent.

Written at `64c7871` (#111), 965 tests passing, v0.1.0 untagged. Amended at `4e66dbc`:
#114 landed after this was drafted and closed #108 — see that entry below.

**Amended again at `e570cba`, 1103 tests.** The three things reported against `08d67b0` —
#125, #126 and #127 — are **fixed and closed**, along with #123. What that cost, and the two
bugs found on the way, is immediately below.

**Amended at `9b3828f`, ~1145 tests.** Since the previous amendment: #57, #59 (hover card),
#69 (audited complete), #70, #100 and most of #82 shipped; the completion popup works end to
end on a real server and has been *seen* working. The lessons worth the price of this round,
each expanded in its own commit message:

- **`ellefuanti .` exists now** (`main` read no argv at all — several diagnostic rounds were
  spent on logs from windows that had never opened anything). `open` the `.app` for the
  Finder environment; the wrapper-script trick in the git history shows how to capture a
  launched `.app`'s stderr when logs are needed.
- **A spawn that succeeds is not a server that runs.** Node LSP servers are `#!/usr/bin/env
  node` scripts; resolving the binary without giving the child a `PATH` spawns a process
  that dies before its first byte. The completion saga stacked five such almost-fixes:
  server never started → binary not found → shebang starved → document never synced → list
  zero pixels tall. Each fix exposed the next; nothing short of a live session finds these.
- **The wire is the ground truth.** `ELLE_LSP_COMMAND` pointed at a tee script captured the
  exact JSON both ways and ended three rounds of plausible-but-wrong theories. Cheaper than
  any amount of reading.
- **`debug_selector` + `VisualTestContext::debug_bounds`** (gpui test builds) give headless
  tests measured element bounds — the suite's first real geometry assertions. Know the
  limit: bounds measure **boxes, not ink**. A fixed-height row whose text wraps out of it
  measures clean while painting garbage; that test was deleted as vacuous, and the comment
  at the truncation site records why.
- **Full-row selection tint hid ⌘D entirely** on themes where `hover == selected`
  (one_dark_pro). Selections now paint precise byte ranges. When a reported "feature is
  dead" arrives, check whether it worked invisibly before checking whether it worked.
- **The multi-cursor undo trap was walked into with its warning already in this file.**
  A loop of replaces is N undo steps no matter the `break_undo_group`s; `splice_at` now does
  the one-spanning-replace shape. The warning did not prevent it; the test did.
- **macOS swallows the activating click on a background window, modifiers included** — the
  recurring "⌘-click does nothing" has been that, twice. F12-after-a-click works because
  the click already activated.

---

## What the last three fixes taught — start here

#125, #126, #127 and #123 are closed. Read this before touching the LSP startup path, the
file tree, or anything that compares two paths.

### #125 was three bugs wearing one symptom

Reported as _"popup não abriu nem quando faz `$this->`"_. It was never about the popup, and
the evidence that said so was **zero log lines** — a spawn failure would have logged.

1. **`start_lsp` was reachable only from `open_folder`.** Opening a _file_ — ⌘O on the file,
   a palette jump, a window where ⌘O never ran — started nothing. This was the cause the log
   pointed at, and the one an eye on `config_for` alone would have missed.
2. **`config_for` returned `Some` unconditionally.** The spawn failed on the background
   executor at `debug` level, so a machine with no server produced no output at all.
3. **#123**, which is real and was verified on this machine: `launchctl getenv PATH` is
   empty, intelephense exists _only_ under `~/Library/Application Support/Herd/config/nvm/…`,
   so a Finder-launched `.app` cannot see it. `config_for` now resolves the binary against
   `PATH` first and a fixed list of installer prefixes after. Checked by running the search
   with `PATH` unset: it finds the Herd copy.

The **general lesson**, which is the one worth keeping: _the absence of a log line is
evidence about which code ran, not about how well it ran._ Three plausible causes each
explained the symptom, and only one explained the silence.

§24's "a missing server is silent" now has one narrow exception: it is named when the open
file is one a server would handle. Silence is right for someone who never wanted a PHP
server and wrong for someone staring at a `.php` file waiting for a popup — that gap is
precisely why this was filed against the popup.

### Two bugs the #126 tests found that reading the code did not

Both were found by _writing the test_, not by review, and both are the shape that survives
review indefinitely.

- **`starts_with` on two paths that name the same file.** `FileTree` canonicalises its root;
  a tab's path is whatever opened it. On macOS the temp directory is a symlink, so the same
  file is `/private/var/…` from one and `/var/…` from the other. Deleting a file therefore
  left its tab open — and a tab on a deleted file **writes it back on the next ⌘S**, silently
  undoing the delete. `is_under` canonicalises as much of each path as still exists.
  Anywhere two paths from different sources are compared, assume they are spelled
  differently until proven otherwise.
- **A dialog asked after the question was settled.** Closing those tabs went through
  `close_tab_at`, which prompts _"discard unsaved changes?"_ — meaningless about a file that
  is already gone, and asked _after_ the deletion the user confirmed.

### A test that could not fail, and why it stayed

`deleting_a_symlink_removes_the_link_and_not_the_target` does **not** fail if `delete` is
rewritten to use `metadata` instead of `symlink_metadata`. Current Rust's `remove_dir_all`
refuses to recurse through a symlink — it unlinks and returns `Ok` — so both spellings leave
the target intact and no assertion can separate them. This was established by running it,
not assumed.

The guard stays anyway (correctness should not rest on an implementation detail std may
change) and the test stays as a contract guard, with a comment saying exactly what it does
not prove. **That comment is the point**: the alternative is the next reader assuming the
test is what makes the code safe. Compare the vacuous tests in "Conventions" below — the
difference is that this one is honestly labelled.

### #127, and the thing it deliberately does not do

`set_language` gives a pathless buffer a grammar without giving it a path. It rebuilds the
tree against the text already in the buffer — waiting for the next edit would make the
choice look like it did nothing. `set_path` still re-detects on save and overrides it, which
is tested: an override that outlived the save would mean a file called `.php` refusing to
highlight as PHP because of a menu choice made earlier.

---

## What this is

A native macOS IDE for PHP/Laravel/Livewire/Blade, in Rust, on gpui (Zed's UI framework).
Not a generic editor with Laravel plugins — framework knowledge is a subsystem, not an
extension.

Milestone 1 (editor foundation) is done. Milestone 2 (LSP) is mostly done. Milestone 3
(Laravel index) is partly done and out of order — routes and navigation landed before the
SQLite model index.

---

## The crates, and which of them the app can reach

Thirteen crates plus `benchmarks/`. `README.md` says ten and `docs/ARCHITECTURE.md` says
six with a crate graph that predates `elle-git`, `elle-test-runner`, `elle-theme` and
`elle-index` being wired — **both are stale on the crate list**. `Cargo.toml`'s `members`
is the authority.

| Crate         | Owns                                                               | Wired into the app        |
| ------------- | ------------------------------------------------------------------ | ------------------------- |
| `app`         | gpui: window, views, keymap, editor rendering, all panels          | — (is the app)            |
| `core`        | command registry                                                   | yes                       |
| `text`        | rope buffer (ropey), undo/redo, edit log                           | yes                       |
| `syntax`      | tree-sitter parsing and highlighting, 9 grammars                   | yes                       |
| `workspace`   | filesystem, lazy file tree, safe file IO + create/rename/delete, `CancelFlag` | yes          |
| `terminal`    | PTY sessions, alacritty VT/ANSI emulation, selection, key encoding | yes                       |
| `lsp`         | generic LSP client, hand-rolled framing, no tokio                  | yes (#74)                 |
| `index`       | SQLite file index (rusqlite, bundled)                              | yes (#72), file list only |
| `laravel`     | route/config/view extraction from source text                      | yes (#68, #91)            |
| `git`         | git2 status and diff, **read-only**                                | yes (#96)                 |
| `test-runner` | Pest/PHPUnit spawn, TeamCity streaming parse                       | yes (#101)                |
| `settings`    | `settings.json` read/merge/atomic write                            | yes (#76)                 |
| `theme`       | on-disk theme format, VS Code theme importer                       | yes (#86)                 |

`elle-index` holds a _file_ index only. The Laravel model/column/relationship index of
ADR-0008 does not exist yet — that is #21, and #22 and #20 sit behind it.

### The two ADRs that govern everything

**ADR-0004 — only `crates/app` may depend on gpui.** Enforced by
`crates/app/tests/architecture.rs::only_the_app_crate_depends_on_gpui`, which walks every
manifest. Two related tests live there: `a_backend_is_named_only_where_the_default_is_declared`
(Intelephense may be named in exactly one constant in `crates/app` and nowhere in
`crates/lsp`) and `domain_crates_have_no_platform_conditionals` (no `#[cfg(target_os)]`
outside the app crate). A `grep gpui crates/*/Cargo.toml` hits `crates/git/Cargo.toml` —
that is a _comment_ explaining that git2 arrives transitively through `gpui → gpui_util`,
not a dependency. The test is the answer, not the grep.

**ADR-0007 — gpui's executor, never tokio.** `cx.background_spawn` for blocking work,
`cx.spawn` for main-thread continuations. Domain crates are synchronous and blocking and do
not know which executor runs them. Cancellation is structural: dropping a gpui `Task`
cancels it, so a view holds one `Task` per job slot and issuing a new one drops the old.
The compiler does not enforce "never block in a render or event handler" — that is review.

This ADR is why `async-lsp` was rejected (it is tower-based and brings tokio) and the
transport is ~110 hand-rolled lines of `Content-Length` framing instead. It is also the
unresolved question in #65 (see below).

---

## What landed after this document was first written

`64c7871` → `08d67b0`, and the rendering half of it is the part worth knowing.

**Rows are painted, not laid out** (#110). A row used to be `div().h(...).child(StyledText)`.
`StyledText` resolves its line height from `window.text_style()` **at layout time**, not from
an ancestor div — so a `.line_height()` set on the root never reached rows built inside a
`uniform_list` callback. Four PRs (#106, #107, #109, #110) fixed the arithmetic before anyone
found the channel was never read. `crates/app/src/editor/line.rs` is now a custom `Element`
that shapes its own line and calls `ShapedLine::paint(origin, line_height, …)`, which takes
the height as an argument. This is what Zed does.

**`svg()` does not inherit its colour.** It fills its alpha mask from `style.text.color` **on
the element itself**. Three PRs were needed to learn this (#121 tree and tabs, #122 activity
bar) because the comment at the activity bar asserted the opposite — that the icon inherited
from its parent — and it read as a considered decision. Every rendered `svg()` in the crate
now sets a colour explicitly. **A wrong comment beside plausible code survives review.**

**`ShapedLine::paint` draws glyphs only** (#111). Run backgrounds — the terminal cursor, a
selection, a search match, a diagnostic — need `paint_background` called separately. Missing
it made the terminal cursor a hole that walked along the line while typing.

**Zed is cloned at `~/zed`** and is the reference for anything gpui-shaped. #114 took its
backspace-to-tab-stop rule (`editor.rs:5010`), #117 its `surrounding_word` classification
(`buffer.rs:4190` — `cmp::max(prev, next)`, not "the character to the right") and its
autoscroll margin. Every port that worked was **read**, not reasoned about. #117 also found a
real gpui bug on the way: `scroll_to_item_with_offset` documents shrinking the viewport by
`offset` items but only shrinks from the top (`uniform_list.rs:406` vs `:409`).

**Completion exists** (#118, #124). The popup carries provenance in the type
(`CompletionItem.source`), cancels rather than queues, and opens on the **server's own**
trigger characters. Intelephense declares ten single characters — `$ > : \ / ' " * . <` —
and **not** `->` or `::`, so the obvious hardcode has the wrong shape and would never fire.

**⌘⌥I opens it manually.** Not ⌃Space: the whole spacebar is claimed by macOS on a machine
with more than one input source (`⌃Space` previous input, `⌘Space` Spotlight, `⌥⌘Space`
Finder search — all verified enabled in `com.apple.symbolichotkeys`). A chord a keymap
accepts is not a chord the OS delivers.

## The traps

These are the ones that cost more than one attempt. Read this section before touching
rendering or performance.

### gpui `StyledText` never reads an ancestor's line height

`StyledText` resolves the height it lays text out at from **`window.text_style()` at layout
time**, not from the `div` wrapping it. A `.line_height()` on an ancestor therefore does
nothing for a row built inside a `uniform_list` callback, because the callback runs outside
that style scope.

Four PRs fixed the arithmetic before anyone found the channel was never read: #106 (editor),
#107 (terminal, same bug in the file the first fix did not check), #109 (right about gpui
rounding text line heights with `.round()` while a `div`'s `.h()` keeps the fraction —
correct, and still changed nothing on screen), then #110, which stopped trying to make two
numbers agree and made a row a custom `Element` that shapes its own line and calls
`ShapedLine::paint(origin, line_height, …)`. gpui centres glyphs inside the height it is
_given_, so one number covers box and text. This is what Zed does with the same gpui
(`LineWithInvisibles::draw`) — it never wraps a row in a div.

The reasoning is preserved in `crates/app/src/editor/line.rs`'s module doc. Read it before
proposing a layout change to a row.

### `ShapedLine::paint` draws glyphs only

Backgrounds carried in `decoration_runs` need a **separate** `paint_background` call
(`editor/line.rs:153`). #110 moved rows off `StyledText` and took half of what it did.

The terminal cursor is an _inverted cell_ — its glyph is painted in the background colour —
so with the background never drawn, the character under the cursor read as a hole walking
along the line, and there was no visible cursor anywhere. Selection and diagnostic
underlines were invisible for the same reason and went unreported because nobody had tried
them. Fixed in #111.

### The headless text system is a fake perfect monospace

gpui's test platform installs `NoopTextSystem`: `font_id` returns `FontId(1)` for **every**
descriptor, and `advance` is `600.0 * glyph_id` where `glyph_id` is `ch.len_utf16()`. So in
`#[gpui::test]`, every font is the same font and every BMP character has an identical
advance.

Consequences, all of them load-bearing:

- **No headless test can assert real geometry** — x positions, line heights, column
  alignment. An assertion about `x_for_index` is an assertion about `600.0 * len_utf16`.
- A test asserting "the editor font resolves as monospaced" **passes with Helvetica**. One
  was written, watched to pass under a proportional family, and deleted in `0eff21c`.
- gpui does not error on a missing font family: `resolve_font` substitutes a proportional
  face and returns a valid `FontId`, so the editor keeps rendering with every column
  calculation wrong. The only signal is a startup warning in `main` that merely **logs**.
  `crates/app/src/fonts.rs`'s module doc has the measured advances (Menlo 9.63/9.63/9.63 for
  `i`/`W`/`m` at 16px; Comic Sans 4.48/16.63/12.43).

This is a hard boundary, not a gap a better assertion closes.

### `ps` on macOS lies in two different ways

**`ps %cpu` is a decaying lifetime average, not a rate.** A process that burned CPU at
startup keeps reading non-zero and decays toward zero regardless of what it is doing.
Sampling once after a 20 s settle measures where you are on the decay curve. At 26 s
elapsed — almost exactly the "settle then sample" recipe — it read **0.0%** while the true
rate was 0.50%.

**`ps rss` varies ~19 MB across launches of one unchanged binary** (83.9–102.5 MB), because
it counts GPU/IOSurface mappings whose residency the kernel varies. `vmmap -summary`'s
Physical footprint over those same launches held at 75.7–76.8 MB.

#79 was opened on the strength of both, argued across three comments — a regression, then an
improvement nobody could explain, then a regression again — and **closed as a measurement
artefact with no code changed**. Neither transition happened; they were one decay curve read
at different points. Real like-for-like growth on footprint is ~70 → ~76 MB over 59 commits.

Use CPU-time deltas over a wall-clock window, and `vmmap` Physical footprint. Both are what
`scripts/perf-gate.sh` now does (#93).

**The genuine finding underneath it:** idle CPU is ~0.5–0.9% and always has been, and it is
**upstream gpui 0.2.2**, not ours. `platform/mac/window.rs`'s `display_layer` calls
`start_display_link()` unconditionally after every frame and `step` never stops it, so a
`CVDisplayLink` wakes the main thread every vsync forever after the first paint. Not
addressable from this codebase; worth filing upstream. The perf gate's 2% CPU limit sits
above that floor deliberately.

### gpui git `main` diverged from the crates.io release while reporting the same version

`main` moved entry-point setup into a separate `gpui_platform` crate and removed
`Application::new()`, while its manifest still says `0.2.2` (ADR-0002). **Code copied from
Zed's `main` may not compile here.** Pin the crates.io release; if you ever move to git, pin
a tag, never a branch.

A concrete instance: `open_about_panel` exists on `main` and not on the pinned release,
which is why the menu bar (#78) has no About item.

Zed's _source_ is still the best reference for how to use gpui correctly — #109 and #110
both resolved by reading `crates/editor/src/element.rs`. Reading it is fine; copying it is
what breaks.

### The perf gate has had three contamination holes

`scripts/perf-gate.sh` (thresholds: 95 MB footprint, 2% CPU, 17 MB binary; all blocking.
Startup is printed, never gated, because the same binary measured 737 ms on first launch and
216–236 ms after, purely dyld and page-cache state).

1. **`clippy-driver` was missing from the `pgrep` process list.** The gate printed idle CPU
   8.00% — four times its own limit — and exited 0, because two `clippy-driver` processes
   were burning 114% and `pgrep` said the machine was clear. Fixed by naming it, plus a
   load-average check as the general fix, since a machine running several agents is loaded by
   things that are not compilers.
2. **A truncation bug.** `$((bytes / 1048576))` made a 14.53 MB binary read as `14` and pass
   a 14 MB limit. Both checks compare in bytes/KB now; only the display rounds. That is the
   shape of bug that makes a gate silently enforce nothing.
3. **The one that is still open** (see #84's last comment): in a headless agent session the
   **window never renders**, so no display link runs and the pages a foreground window would
   fault in never do. The gate then reports ~42 MB and 0.00% CPU — both impossible — and
   exits 0. The first two holes reported numbers that were too _high_ (false alarms, safe);
   this one reports too _low_, so a real 90 MB regression would sail through. The proposed
   fix is a floor check: idle CPU of exactly 0.00% means the window is not drawing, so exit 2
   (contaminated), not 0 (pass).

**So: perf-gate numbers from an agent session are not evidence of anything except the binary
size**, which is read from the file and does not care whether anything renders.

### `benchmarks/BASELINE.md` opens with a warning that keeps being vindicated

> The most expensive mistake made in this file's history was not a slow function — it was
> trusting a benchmark. … when a number surprises you, suspect the measurement before the
> code. If two runs of the same code disagree, that is a fact about the measurement, and
> averaging it away destroys the only signal you had.

Instances this session, each independent:

- #27's 24.8 ms-per-keystroke was criterion charging destructor time to every sample.
- The JSON highlight bench reported 22 → 189 → 355 µs across file sizes — the window was 80
  _rows_ clamped with `.min(rows - 1)`, so the small fixture got a narrower window.
- Find-in-project's first run timed the walk cold once (10.6 ms) against searches timed warm
  five times (7.5 ms) — a container smaller than its contents — and its "no hits" query found
  two hits because the nonsense string had by then been written into the file being searched.
- #79, in full.

The person who closed #79 wrote that they had read the warning, quoted it to three agents,
and then built three comments on `ps` output anyway. Expect to do the same.

---

## Conventions that are actually enforced

- **Every PR links an issue** so it closes on merge. `Closes #N` / `Part of #N`.
- **`scripts/perf-gate.sh` runs before pushing** — with the caveat above about agent
  sessions.
- **CI is blocked on GitHub billing.** Jobs do not start; every recent run shows _"The job
  was not started because recent account payments have failed."_ `.github/workflows/ci.yml`
  is real and would run build/test/clippy `-D warnings`/fmt plus a release-config job, but
  **local verification is the only gate that exists.** Do not read "CI green" in an old issue
  comment as current.
- **`cargo test --no-fail-fast`**, always, when taking a baseline. A flake used to abort the
  run and report 154 of 355 tests as "all green" — a failure that _removes_ a third of the
  suite reads as success. Root cause was fixed (#74) but the habit is the point.
- **Tests are mutation-verified, not trusted.** Three vacuous tests were caught this session
  by reintroducing the bug and watching the test stay green: a row-count test that survived
  rendering 500 rows past EOF; a click test that passed with the 284 px bug restored (it
  rendered the editor standalone, where the buggy guess and the correct measured origin are
  the same number); the font test deleted in `0eff21c`. Also
  `splitting_does_not_start_a_second_timer`, which asserted `poll.is_some()` on an `Option`
  field and therefore could not fail. **Writing a test is not the same as knowing it works.**
- **A green test that asserted nothing is worse than a red one.** `diagnostics_notifications_become_events`
  silently timed out at 5 s and passed.
- **Nothing is optimised without a profile**, and no benchmark claims to measure something it
  does not.
- **Never a positive claim the analysis cannot support.** `resolve` returning `None` means
  "we could not find it", never "it is not there" — so Laravel navigation emits no
  diagnostics, references are reported only for plain literals (`route($name)` yields
  nothing), and a dead language server produces an error rather than "No definition found"
  (RISKS #4).
- **Nothing signalled by colour alone.** Diff lines carry `+`/`-`, status rows carry
  `M/A/D/R/?/!`; colour is redundant.
- **New theme colours are per-variant `Theme` fields**, never a reuse of an existing one — a
  tint chosen against `#282c34` is invisible on `#ffffff`, and `one_dark_pro` sets `hover`
  and `selected` to the same value.
- **`ponytail:` comments** mark a deliberate simpler-than-ideal choice with the upgrade path
  named. They are a convention, not dead TODOs.
- Commits and PR bodies explain _why_ and state what a choice **rules out**. Match that.

---

## What is verified and what is not

**#35 is the standing caveat and it is still open.** Machine-verified: views render without
panicking (9 render tests under `gpui::test`), a click lands on the right column
(mutation-checked against the 284 px bug), typing reaches the buffer including `ação`, the
text origin is measured at prepaint rather than guessed, and the paint _computation_ — which
bytes get which colour — is thoroughly tested.

Not machine-verified, and structurally not verifiable headlessly: **that any of it reaches
the screen correctly.** Panel geometry and proportions, column alignment (i.e. whether Menlo
actually resolved), file tree indentation and arrows, palette overlay position and z-order,
terminal grid alignment, theme colours as intended.

**#112 explains why the test suite cannot catch rendering bugs.** The tests assert the runs a
row produces — `line_runs`/`row_runs` return `(String, Vec<TextRun>)` and the tests check
ranges, colours and coverage, all correct in every failure below. A background that is
computed correctly and never painted is **identical** to one that works, from the test's
point of view. So is text laid out at the wrong height, a caret behind its line, and a glyph
pinned to the wrong cell width.

Five consecutive rendering fixes, every one caught by a human looking, with 965 tests green
throughout:

| PR   | Bug                                                                      | Caught by |
| ---- | ------------------------------------------------------------------------ | --------- |
| #106 | line height set where `StyledText` never reads it                        | the user  |
| #107 | same, in the terminal                                                    | the user  |
| #109 | right diagnosis, wrong channel — still never reached the rows            | the user  |
| #110 | rows fixed; caret placed _below_ the text                                | the user  |
| #111 | run backgrounds never painted — cursor, selection, diagnostics invisible | the user  |

#112 sketches three options (a `Scene` assertion if 0.2.2 exposes enough; golden images, at
the cost of a rendering environment CI cannot currently provide; or accepting it and treating
"a human has looked" as part of done) and proposes none. **Start a rendering change by
answering "how will I know this worked", not five iterations in.**

The last first-hand visual confirmation on record is at `edee706` — syntax highlighting
renders correctly, colours right. Everything since has been confirmed only by report.

### One thing to check before trusting an issue comment

#108's comment says the indent-guide grey blocks are "gone" because #110 deleted the
character-background path. **That is no longer true.** `view.rs:1158` still pushes
`indent_guide_columns` as `GpuiHighlight { background_color: Some(theme.indent_guide) }` into
`line_runs`, and #111 restored `paint_background` on the row element — so the blocks are
almost certainly back. Verify by eye before doing anything with #108. The fix it describes is
still the right one (a 1 px quad at `x_for_index`, the way the caret is painted; Zed's width
is an absolute pixel count clamped 1..=10, defaulting to 1, and its background-fill mode is
off by default — our blocks are essentially that opt-in mode arrived at by accident).

This is the general lesson: issue comments are dated snapshots. The repo is the authority.

---

## Where the work is

### Blocked, and on what

- **#24 Livewire** — hard-blocked on a **Blade tree**. The directive scanner colours Blade
  correctly and produces no tree, so no `wire:` attribute structure, no component-tag
  navigation, no slot awareness. The prerequisite is not "write Livewire indexing", it is
  **revisiting ADR-0006** and deciding whether to adopt a tree-sitter Blade grammar with
  injections for the embedded PHP and HTML. That is an architecture decision with its own
  cost. (Note: `route()`/`@include`/`<x-…>` navigation shipped in #91 _without_ a Blade
  grammar, because those are path expressions resolvable by directory lookup — that does not
  generalise.)
- **#20 Merged completion engine** — needs **two real sources** to merge. #19 supplies LSP
  (largely built). The Laravel source needs #22, which needs #21's index, which does not
  exist. Building the merge layer against one source plus a stub bakes in assumptions the
  second source breaks. Two requirements that are easy to lose in the wait: **provenance must
  be modelled in the completion type**, not attached at render time (a column from a migration
  is a different kind of claim than a guess from a method name — if added late it gets
  reconstructed by guesswork, which defeats it); and **cancellation, not queueing**.
- **#65 Database viewer** — has an unresolved conflict with ADR-0007. SQLx is async and
  expects a runtime; ADR-0007 rules out tokio. **Resolve that first, because it may change the
  crate choice entirely** — and note SQLx's headline feature (compile-time query checking) is
  useless here since queries are user-typed at runtime, which widens the field considerably.
- **#61 Completion UI** — downstream of #20.
- **#100 Settings panel** — the file layer exists (#76, ADR-0009); this is the GUI.

### Open and unblocked

- ~~**#108** indent guides as 1 px rules~~ — **closed by #114**, and the prediction made
  while drafting this document was right: the blocks _had_ returned. #110 removed the
  character-background path they used, so they vanished as a side effect; #111 restored
  `paint_background` for the terminal cursor and brought them back with it. They are now
  quads drawn by `Line` at a measured x. #114 also ported Zed's backspace-to-tab-stop rule
  (`editor.rs:5010`): `((column - 1) / width) * width`, not a jump to column zero.
- **#112** decide the rendering-verification story. Three more features have now shipped
  whose visible behaviour is asserted as state and never observed — the context menu's
  position, the confirmation's layout, and whether a language choice changes any colour.
- **#53** grammars for the remaining languages. Nine exist; `.rs`, `.md` and others still
  open with no colour, which is a coverage gap, not a bug.
- **#57** `cargo build --release --no-default-features` fails on Metal shader compilation —
  and that is the path ADR-0002 describes as _the release configuration_. Either it is
  intended to work and is broken, or it was aspirational and ADR-0002 plus
  `scripts/bundle-macos.sh`'s error hint both need correcting. Not investigated.
- **#70** terminal: clickable `file.php:42` paths, ⌘F over scrollback, replacing the 16 ms
  poll with a wake-up (`terminal_view.rs` carries a `ponytail:` note on that last one — the
  reader thread could notify the window through `AsyncApp` + a channel, dropping idle cost to
  zero while the panel is open).
- **#64** git items 3–5. **Write operations are deliberately unbuilt**: losing uncommitted
  work is one of the two things in this editor a user cannot undo, so the read-only panel
  ships before the confirmation machinery exists. Item 4 (commit) has a known problem recorded
  in the crate docs: **libgit2 does not run hooks**, so a commit would skip pre-commit.
  The confirmation machinery now exists — #126 built a modal `Overlay` with a
  destructive-action path, defaulting to Cancel — so "there is nowhere to ask" is no longer
  the blocker it was when this was written.
- **#63** Linux (deferred — the domain layers port, the UI ports when gpui does).
- **#71 / #81 / #82 / #83 / #69** are umbrella issues, each substantially delivered by the PRs
  above but still open for their remaining items.
- **#35** stays open until a human has checked the list in it.

### Milestone 3+ and scope-change

#21/#22/#23 (Laravel index, Eloquent, Artisan), #25/#26 (tool panels), #28–#31 (plugins, AI,
Xdebug, browser preview), #99 (AI chat). RISKS #6 is the standing warning: §14–§20 span
several products, each with a full-time team elsewhere. **The editor and Laravel intelligence
are the product; the tool panels are features.**

---

## Smaller things that will otherwise cost you an hour

- **`Buffer::replace` coalescing.** `Edit::extends` holds only for contiguous typing with
  nothing deleted, so a loop of replacements inside a `break_undo_group` sandwich is _not_ one
  undo step no matter where the breaks go. Replace-all is one `replace` over the span from
  first to last match with the replacements spliced in — the shape `indent_lines` already
  uses. The test that caught it uses twenty replacements, not three, because a coalescing bug
  that merges pairs passes at three.
- **A condvar is bound to a mutex by `wait`/`wait_timeout`, not by `notify_all`.** #43 looked
  like a load-dependent flake for weeks. It was `Connection` waiting on one `Condvar` with two
  different mutexes; Rust's std detects this and aborts the thread, the waiter dies holding
  `inbox`, poisoning it, and the reader then dies on `inbox.lock().unwrap()`. The `unwrap` was
  the messenger. The suggested fix at the time — explicit poison handling in the reader —
  would have converted a deterministic crash into a silent one.
- **The character advance is measured, not assumed.** `CELL_WIDTH_RATIO = 0.6` was wrong:
  Menlo is 0.602051, Monaco/Courier New/Andale Mono 0.600098, identical at 13/16/20px because
  the ratio is a property of the face. Every real ratio is **above** 0.6, so the assumption
  always over-estimated how many characters fit, and `sync_size` floors a division by it to
  decide **how many columns to tell the PTY it has** — so the shell itself wrapped at a width
  that did not exist (#92, #94). Measured once in `resolve` and cached on `Fonts`. **Round
  down, never to nearest**: an unused pixel column is invisible, one column too many is that
  bug.
- **Line height rounds; cell width must not.** gpui's `line_height_in_pixels` ends in
  `.round()`, so the editor rounds at the source and every consumer reads the integer gpui
  would have picked. The terminal deliberately does _not_ round, matching Zed, because its
  grid places rows arithmetically and the cell width's fraction is what keeps #92 fixed.
- **`Fonts::cell_size` returns width and height together on purpose.** Three consumers must
  agree — grid layout, PTY resize, mouse hit-test. If drawn row height and resize row height
  disagree, the shell believes it has a different number of rows than are drawn and its output
  garbles, which no render test would catch.
- **PHP and Blade keep a hand-written tree walk; the other seven languages go through
  `highlights.scm`.** The upstream PHP `highlights.scm` was tried and **cannot** reproduce
  what this editor's PHP tests assert (no `=`/`=>`/`->`/`::` operators, no `#[` attribute
  bracket, and it tags a class property `$name` as both variable and property — three existing
  tests fail against it). Swapping it in and relaxing the assertions is not on the table. The
  query path is also ~2.7× more expensive (87 µs vs 32 µs on the 80-row viewport bench), which
  is not the reason but is not nothing. See `crates/syntax/src/highlight.rs`'s doc on
  `highlights`.
- **Viewport highlighting must stay flat.** Pinned by
  `viewport_cost_does_not_grow_with_file_size`. The first implementation scanned the root's
  entire child list every frame; the fix is `TreeCursor::goto_first_child_for_byte`, which
  binary-searches.
- **Search cost tracks file _count_, not byte count** — the per-file metadata+read syscall
  pair dominates matching, so pruning `vendor/` at the directory boundary is worth more than
  any per-file optimisation.
- **Settings invert the index's rule.** The index (ADR-0008) is a cache and deletes what it
  cannot understand; settings (ADR-0009) may **never** delete anything, because the input is a
  file a human typed with no source to rebuild from. The in-memory value _is_ the parsed
  `serde_json::Map`, so an unknown key survives a downgrade's write. A malformed file loads as
  defaults **and disables saving for the launch** — otherwise the first theme toggle would
  write a two-key document over everything the user configured.
- **The theme importer resolves scopes by specificity, not file order.** A first-hit search
  gets `attribute` wrong on One Dark Pro. Twelve ported colours were wrong and every
  disagreement was settled by the source file, not by reasoning.
- **`markdown` conflict markers are invisible.** A `<<<<<<<` sat in `CHANGELOG.md` through
  three merges. In a Rust file that is a compile error; in markdown nothing checks.
- **Screen capture and the accessibility API are denied in the agent environment.** Nothing
  visual can be self-verified from here. Say so rather than implying otherwise.

---

## How to run it, and why it matters which way

```sh
cargo build --release -p ellefuanti && sh scripts/bundle-macos.sh
./target/ellefuanti.app/Contents/MacOS/ellefuanti     # from a shell — inherits PATH
open target/ellefuanti.app                            # from Finder — does NOT
```

**The two are not equivalent.** `open` gives the app launchd's environment, which on this
machine has no `PATH` at all, so anything installed under nvm, Herd or Homebrew's non-default
prefix is invisible to it. That was #123, and it is why the LSP appeared to do nothing when
the app was double-clicked.

**#123 is fixed** — `lsp_session::config_for` now searches a fixed list of installer prefixes
(nvm, Herd, Homebrew, the per-user npm targets) when `PATH` does not have the binary, so both
launches find a server. The two are still not equivalent for *anything else* a subprocess
might need, so a feature that shells out is still worth testing from Finder.

The `.app` is also the only way to see the icon (#55) — a bare binary gets the generic one.

## If you are about to claim something works

Run `cargo test --no-fail-fast` (~1145 expected; one PTY test is load-flaky under the full suite and solo-green). Then, for anything that touches the screen,
say plainly that it has not been seen. The five-PR sequence above is what happens otherwise.

The two-minute human check, from #35: `cargo run` → ⌘O a Laravel project → click into a PHP
file → **does a column of characters line up vertically?** → type something accented → ⌘P and
search a nested file → ⌘⇧P → open a terminal and run `ls`.

Three steps were added by the last batch, and they are the ones nothing headless can check:
**right-click a file in the tree** (does a menu appear at the pointer, and does Delete ask
before it acts?) → **⌘N then click the language cell in the status bar** (does the list open,
and does choosing PHP colour the buffer?) → **⌘O a single `.php` file with no folder open**
(does the status bar stop saying "No language server" once Intelephense starts?).
