# Changelog

All notable changes to this project are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Navigation**: go to definition (F12, ⌘click), find usages (⇧F12), go to symbol in file
  (⌘⇧O) and back/forward (⌃- / ⌃⇧-). `elle-lsp` has had the typed methods since #45 and #74
  wired a real server; none of them was called by anything, so this is the UI over work that
  already existed.

  The blocker all of them shared went first: **`open_path` was asynchronous with nowhere to
  put a cursor target**, so a caller that knew where it wanted to land could only open the
  file and give up on the line. `open_path_at` takes an optional position and
  `EditorView::reveal` is the single place a jump lands, so the tab-already-open case and the
  just-loaded case cannot drift apart. #68's route palette had been extracting `route.line`
  and discarding it — it now jumps to the line that declares the route (#68, #81).

  Symbol responses are flattened from **both** shapes the protocol allows. A server chooses
  between `DocumentSymbol` and `SymbolInformation`, and handling only the modern one is a
  palette that silently goes empty the day somebody swaps their server — RISKS.md #2 in
  miniature. Definition responses likewise handle all three shapes, and a `LocationLink` uses
  its _selection_ range, so the cursor lands on the identifier rather than on the first line
  of the doc comment the enclosing range starts at.

  Every query shares one job slot, so a second navigation cancels the first rather than
  racing it, and each is silent when no server is running — §24's rule and #74's established
  behaviour, since nobody has a language server on a fresh machine and most folders anyone
  opens are not PHP projects.

  `Connection::poll` is new and is what keeps this responsive. `wait` cannot be polled: a
  zero timeout _drops the pending entry_, so asking "answered yet?" that way would cancel the
  request on the second call. The UI needs a non-destructive check because the `Client` lives
  in the view and reaching it happens on the main thread, where blocking for the seconds a
  cold server takes is precisely what ADR-0007 forbids. `uri_to_path` is the counterpart to
  `path_to_uri`, needed now the server _answers_ with locations instead of only being told
  about them; a non-`file:` URI is refused rather than guessed at, because opening the wrong
  file is worse than opening none (#81)

- **Find and replace in a file (#80).** ⌘F opens a find bar, ⌘⌥F opens it with the replace
  row. Incremental as you type, with a live count (`3 of 17`), ⌘G / ⌘⇧G or ↵ / ⇧↵ to walk
  the matches with wrap-around at both ends, and case-sensitive, whole-word and regex
  toggles. Replace one, or replace all. ⌘F with text selected seeds the query from the
  selection; escape closes the bar, clears the highlights and puts focus back in the text
  **without closing the tab**. The bar has its own key context (`Find`) precisely so escape
  there cannot reach anything else.

  **Replace-all is one undo step**, which is the part that took two attempts. The obvious
  implementation — a loop over `Buffer::replace` inside a `break_undo_group` sandwich —
  does not work, and the test caught it: `Buffer::replace` coalesces only when
  `Edit::extends` holds, and that is deliberately true just for contiguous typing with
  nothing deleted. Twenty replacements are twenty deletions and therefore twenty groups no
  matter where the breaks go. It is now **one** `replace` over the span from the first
  match to the last, with the replacements spliced in — the same shape `indent_lines`
  already uses, and the syntax tree sees one edit instead of N.

  **Match highlighting composes with syntax colours rather than replacing them.** A match
  paints a background and the token underneath keeps its foreground, so searching for
  `return` does not turn every keyword the colour of a hit — the same discipline the
  diagnostic underlines follow, and `merge_underline` was generalised to `merge_over` so
  both use one run-splitting implementation. The current match is a different colour from
  the rest, both derived from per-variant theme values rather than added as fields, because
  a tint chosen against `#282c34` is invisible on `#ffffff` and `selected`/`selection`
  already solve exactly that problem in all five themes.

  **The performance shape is stated rather than assumed.** Matches are computed once per
  (query, buffer version) over the whole file — the count is a whole-file fact and ⌘G that
  stops at the bottom of the viewport is not ⌘G — but the **per-frame** cost still tracks
  the viewport: `Matches::in_range` is two binary searches over a sorted list, pinned by
  `match_lookup_cost_does_not_grow_with_file_size`, the search counterpart to #52's
  `viewport_cost_does_not_grow_with_file_size`. The rescan itself was measured, not
  guessed: **1.8 ms** for a 50k-line file with a hit on every line, 2.4 ms for the same
  with a regex, 235 µs with no hits (the cost tracks the _hit count_, not the file size).
  That fits inside a 8.3 ms frame and only fires on a keystroke in the find field — but it
  is a fifth of the budget, so `benchmarks/BASELINE.md` says so plainly instead of claiming
  search is free. Past `MAX_SEARCH_BYTES` (4 MB, derived from that table) the bar says
  "File too large to search" rather than dropping frames silently.

  `regex` is now a direct dependency. It is not a new one in any meaningful sense — it was
  already compiled for every build through `gpui` (via `gpui_util`) and `tree-sitter`, so
  declaring it costs no build time and adds nothing to the supply chain. The alternative
  was hand-rolling Unicode-aware case folding and word boundaries, which are the two rules
  most likely to be subtly wrong, to avoid a crate that was already linked in.

  **Find in project (⌘⇧F) is not in this change.** It is the expensive one, it wants the
  `crates/index` walk, `CancelFlag` and streaming results the issue describes, and it
  deserves its own PR built on what this one learned. The Search entry in the activity bar
  stays disabled until then.

- Themes load from disk, and VS Code themes can be imported. `elle-theme` is a new plain-Rust
  crate holding a native format — flat, one key per colour, versioned from the first commit —
  plus an importer for VS Code's `colors` and `tokenColors`. Themes are read from
  `assets/themes/` and `~/Library/Application Support/ellefuanti/themes/`, the user's copy
  winning a name collision and a **built-in winning over both**, so no file can shadow
  `Theme::dark()` and the "Dark is always available" guarantee holds through a directory full
  of broken files. A theme that fails to load names the file and the problem and costs one
  theme, never the launch; nothing writes a theme file back, so ADR-0009's "malformed file
  saved over with defaults" trap has no equivalent here. Disk themes join the existing
  `theme.toggle` cycle rather than getting a command of their own (#58)

- **Scope resolution by specificity, not file order**, which is the half of the importer with
  the actual work in it. A TextMate scope like `entity.other.attribute-name` is matched by
  `entity`, `entity.other` and itself, and the longest wins. A script written during #53 got
  this wrong and reported One Dark Pro's `attribute` as `#e06c75`, because `entity.name.tag`
  is listed earlier in the file; the published value is `#d19a66`. Descendant selectors
  (`string variable`) are skipped rather than approximated — they need the scope stack at a
  position in a document, which an importer resolving a name in the abstract does not have.
  Where a theme says nothing, the fallback names another key in the same theme, so an
  unstyled concept lands in the theme's own palette and never at black (#58)
- Configurable fonts, on the settings layer #60 added: `editor.fontFamily`,
  `editor.fontSize`, `ui.fontSize` and `editor.lineHeight`. Line height is a **multiplier**
  rather than pixels — the old `20px` against `13px` text was a ratio someone chose once
  that stops meaning anything at 20px text. ⌘+ / ⌘- / ⌘0 adjust the editor size live and
  persist it.

  Two metrics that were fixed pixel values are now **derived** from the font size, because a
  constant is not merely unfashionable there, it is _wrong_ at any other size: the gutter's
  `52px` loses a five-digit line number at 20px, and the terminal's `16px` row overlaps its
  own text as soon as anything zooms. The terminal's cell width and row height come from one
  function returning both, because three places need them and all three have to agree — the
  grid layout, the PTY resize, and the mouse hit-test that anchors a selection. A drawn row
  height that disagrees with the resize row height means the shell believes it has a
  different number of rows than are on screen and its output garbles, which is a worse
  failure than a visual offset and is not one a render test would catch.

  The part that is not cosmetic: **a configured family is verified monospace at selection
  time, against real glyph metrics.** gpui does not error on a missing family — it
  substitutes a proportional face and returns a valid `FontId`, and every column
  calculation in the editor and terminal silently goes wrong. So each candidate is checked
  for availability and then measured (`i`, `W`, `m` advances through `cx.text_system()`),
  and a family that fails either check is skipped with an error saying _which_ — "not
  installed" and "not monospaced" are different problems with different fixes. Falling back
  walks a chain (Menlo → SF Mono → Monaco) rather than one hardcoded name.

  Those checks are deliberately **not** unit-tested, and that is the point. gpui's test
  platform gives every character an identical advance, so a headless "is it monospaced"
  assertion passes with Helvetica — the same mistake as the test deleted in `0eff21c`. They
  were verified by running them against the real macOS text system instead: Menlo and Monaco
  accepted at 9.63/9.60px uniform advance, Helvetica and Comic Sans MS rejected at
  3.55–16.63px. That run also removed a generic `monospace` entry from the fallback chain,
  which had been added on the assumption gpui resolves the CSS generic — measured, it comes
  back _proportional_, i.e. an entry that would have failed the check it existed to satisfy
  (#49)

- A settings layer: `~/Library/Application Support/ellefuanti/settings.json`, read at
  startup and written atomically. JSON rather than TOML so #58's `.vscode/settings.json`
  importer is a key mapping over an already-parsed document instead of a second parser
  (ADR-0009). **Global only** — there is no per-project settings file, stated as a decision
  because the read path is a merge with precedence or it is not, and retrofitting one
  touches every accessor. Unlike the index, settings are not a cache and nothing may be
  discarded: the in-memory value _is_ the parsed document, so a key written by a newer build
  is still in the file after an older build saves. A malformed file names the file and the
  position, launches on defaults, and is left untouched on disk — saving is disabled for
  that session rather than overwriting a config file with defaults as the side effect of a
  keystroke. A single key of the wrong type falls back to that key's default and costs
  nothing else. Version field from the first commit. Exactly one key is wired to a real consumer — `theme`, which persists through
  `set_theme` rather than through the toggle handler, so any future way of switching themes
  gets persistence without asking for it (#60)

- The modifier layer of the editor keymap, which was missing entirely. Word motions (⌥←/⌥→
  and their ⇧ variants), line and document motions (⌘←/⌘→ with a smart home, ⌘↑/⌘↓),
  deletions (⌥⌫/⌥⌦, ⌘⌫/⌘⌦), line manipulation (⌥↑/⌥↓ to move, ⇧⌥↑/⇧⌥↓ to duplicate, ⌘⇧K to
  delete, ⌘⏎/⌘⇧⏎ to open a line) and indentation (⇥/⇧⇥ on a selection, ⌘]/⌘[). Word
  boundaries use three character classes so `$user->name` stops at `$user`, `->` and
  `name` rather than treating the whole expression as one word. Every one of these edits is
  a single undo step, which is the part that is invisible until someone presses ⌘Z: the
  deletions and line operations each apply as one `Buffer::replace` between explicit
  `break_undo_group` calls, so a deleted word comes back whole and never drags the
  surrounding typing with it (part of #69)

### Fixed

- **Twelve wrong colours in the three ported themes**, found by pointing the new importer at
  the same files they were hand-extracted from and comparing. Every disagreement was
  investigated against the file on disk; in all twelve the file was right.

  - **One Dark Pro's ANSI table had eight wrong slots.** The extraction substituted the
    theme's _syntax_ palette for its _terminal_ palette: slot 1 was `#e06c75`, the keyword
    red, where `terminal.ansiRed` is `#e05561`; slot 2 was the string green; slot 10 was
    `#4cd137`, a colour that appears nowhere in the file. Easy to make and hard to see,
    because the substituted values are a hair off the real ones and the terminal is not where
    anyone checks a theme.
  - **GitHub Dark and Light coloured attributes as tags.** `attribute` was recorded as
    following `entity.name.tag`, which it cannot: an attribute's scope is
    `entity.other.attribute-name`, and the two diverge at the second segment, so the tag rule
    never matches. The only selector in either file that does match is the bare `entity` —
    `#79c0ff` dark, `#0550ae` light.
  - **GitHub Dark and Light painted operators as body text.** Recorded as "no scope at all,
    so it inherits `editor.foreground`". There is no `keyword.operator` _rule_, but the bare
    `keyword` selector matches `keyword.operator` and everything under it — and VS Code's own
    PHP grammar scopes `=`, `->` and `??` as `keyword.operator.assignment.php` and nineteen
    more, all beginning `keyword.`. Both themes paint PHP operators their keyword red.
  - GitHub Dark's ANSI slot 15 was `#f0f6fc`; the file says `#ffffff`.

  Two doc comments also credited `editorError.foreground` and `editorWarning.foreground` for
  GitHub's diagnostic colours. **Neither key exists in either file** — GitHub leaves both to
  VS Code's built-in defaults. The values were right and the attribution was not (#58)

### Changed

- The file index and `settings.json` now resolve their directory through one
  `elle_settings::support_dir` helper rather than two copies of the same hardcoded string,
  so they cannot drift onto different roots. Same path as before — no cache is invalidated
  (part of #60)

- The theme is a value the app holds rather than a constructor each view calls. It lives in
  gpui's global state behind a private newtype, so `cx.theme()` is the only way to read one
  and no view can build its own — enforced by `crates/app/tests/theming.rs`, not by
  convention. Previously `Theme::dark()` was called inline in four `render` methods, which
  meant a second theme would have reached only the ones someone remembered to update
  (part of #48)

### Added

- Syntax highlighting for **HTML, TOML, YAML and shell**, and with them the extensionless
  files a Laravel project keeps at its root: `artisan` (PHP, detected by name — it has no
  extension to match on and used to open grey) and `.env` / `.env.example` (#53)
- `.env` rides on the bash grammar rather than getting one of its own. It is `KEY=value`
  with `#` comments, which is a subset of what bash already parses, and shells source these
  files literally. What it rules out: a value containing shell metacharacters colours as a
  pipeline. `Dockerfile` was considered and **skipped** — `tree-sitter-dockerfile` is at
  0.2.0, unmaintained, and outside the org that keeps the rest of these current; `.xml`
  (phpunit.xml) was skipped because the HTML grammar hard-codes HTML's void and raw-text
  elements, so it parses XML wrong rather than approximately (#53)
- Syntax highlighting for **JSON, JavaScript, TypeScript and CSS**. Before this, every file
  that was not `.php`/`.phtml`/`.blade.php` resolved to `PlainText` and rendered with no
  colour at all — in a Laravel project that is `composer.json`, `package.json`, `app.js`,
  `app.css` and every config file (#53)
- Those four languages are driven by tree-sitter `highlights.scm` query files in
  `crates/syntax/queries/`, adapted from each grammar's own upstream query (all MIT).
  Adding a language is now a grammar dependency, an enum variant, an extension and a query
  file — no new Rust match arms over node kinds (#53)
- Three themes ported verbatim from published VS Code themes, bringing the `theme.toggle`
  cycle to five: **One Dark Pro** (`zhuangtongfa.material-theme`, MIT) and **GitHub Dark**
  and **GitHub Light** (`github.github-vscode-theme`, MIT). Colours are read out of the
  theme files rather than reconstructed, and are pinned by test. Where upstream paints two
  of this editor's styles the same colour — One Dark Pro gives `variable` and `property`
  both `#e06c75` — the port reproduces it rather than correcting it, so those themes are
  exempt from the distinctness rule that still binds this project's own (#53)
- A light theme, and a `Switch Theme` (`theme.toggle`) palette command that cycles between
  it and the dark one at runtime, repainting every surface including the terminal. The
  light theme exists as proof the plumbing works rather than as a finished design; its ANSI
  table is darkened rather than inheriting the dark theme's `0x0000ff` readability fix,
  which is a dark-background fix and would be backwards here (part of #48)
- `elle-laravel`: static route extraction from `routes/*.php` via tree-sitter — HTTP method,
  URI, name, controller/action, middleware and line, including `Route::resource`
  expansion and `Route::group` prefix/middleware/name inheritance (part of #23)
- Every route field that can be dynamic is a `Resolved<T>`, so "we could not determine
  this" is a distinct value rather than an empty string. Routes registered from variables,
  concatenation, interpolation or loops come back `Unknown` carrying the source expression
  that defeated the reader, and registrations that resolve to nothing at all are reported
  separately instead of being dropped (RISKS.md #4)

### Not included

- **PHP was not migrated to a query file.** The plan for #53 was for every language to go
  through `highlights.scm`, PHP included. Tried, and the upstream `tree-sitter-php`
  query cannot reproduce what this editor's PHP tests already assert: it captures none of
  the `=`/`=>`/`->`/`::` operators, neither the `#[` nor the attribute name in
  `#[Route(...)]`, and it tags a class property `$name` as both variable and property.
  Three existing tests fail against it. Rewriting the query until it matches is a real
  option and is left as follow-up; weakening the assertions to fit the query is not, so PHP
  and Blade keep the hand-written walk and the two paths coexist (#53)
- **Only priority 1 of #53.** No YAML, Markdown, HTML, SQL, TOML or Rust — deliberately
  stopped after landing the mechanism plus four languages, since each remaining one is now
  a dependency and a query file rather than a design question.
- No `.jsonc`, `.json5`, `.tsx`, `.scss` or `.less`. Each would need a grammar that accepts
  it — mapping them to the nearest one parses them into an error tree, which renders worse
  than no colour. They stay `PlainText`, which is the deliberate fallback (#53).
- **Nobody has seen any of the new colours on a screen** (#35). The tests assert that the
  ported hex values match their source and that every language produces spans; neither is
  the same as looking at it.
- No Artisan integration, no `route('` completion, no command-palette or other UI, and no
  persistence — the extractor returns plain in-memory values. SQLite storage waits on #21.
- None of the classic themes (Monokai, Dracula, Solarized, Nord, Gruvbox, One Dark/Light),
  no on-disk theme format, and no persistence of the chosen theme across restarts — #48
  defers the file-format decision until there are real themes to inform it, and remembering
  the choice needs the settings crate. **Nobody has looked at the light theme on a screen**;
  the tests assert that every style has a distinct colour and that nothing is invisible
  against its own background, which is not the same as readable (#35).

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

- 276 tests across 17 suites; `cargo clippy --all-targets` clean, gated by CI
- Layering enforced by test: only `crates/app` may depend on gpui
- Criterion benchmarks with a recorded baseline (`benchmarks/BASELINE.md`)
- Startup and frame instrumentation via `ELLE_PERF=1`
- 8 ADRs recording every irreversible technology decision, including one recording a
  diagnosis that turned out to be wrong

### Performance

Measured, not estimated. Full detail and methodology in `benchmarks/BASELINE.md`.

| Metric                                     | Target     | Measured         |
| ------------------------------------------ | ---------- | ---------------- |
| Cold startup, first launch of a new binary | < 500 ms   | 520–536 ms ❌    |
| Cold startup, later launches               | < 500 ms   | **~195–380 ms**  |
| Warm startup                               | < 150 ms   | ~200 ms ❌       |
| Idle RAM                                   | 100–200 MB | **69 MB**        |
| Keystroke → pixel, 55k-line file           | < 8.3 ms   | **2.65 ms**      |
| Frame render, 55k-line file                | < 8.3 ms   | **0.08–0.77 ms** |
| Folder open, 5000 vendor files             | —          | **64 µs**        |

**A note on the startup numbers, because an earlier draft of this file got them wrong.**

The release build compiles Metal shaders ahead of time, and we predicted that would remove
the ~432 ms first-launch spike. It did not. Running the CI-built precompiled binary gives the
same warm figures (152 ms window phase, ~195 ms total) and a first launch that is still over
budget at 536 ms.

So the first-launch cost is OS-level overhead every new Mach-O pays — dyld closure
construction, signature validation, page-in — not shader compilation. **Any launch after the
first clears the 500 ms budget comfortably**; the first one after downloading does not, and no
build flag we control changes that.

Warm startup at ~195 ms against a 150 ms target remains missed and unattributed. It will be
profiled rather than theorised about — see `benchmarks/BASELINE.md`.

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

### Bugs worth recording

Each was found by measurement or adversarial review rather than by reading the code, and each
had passing tests over it beforehand:

- Viewport-scoped syntax highlighting **was not actually viewport-scoped**. It range-checked
  correctly but scanned every sibling node, so cost grew with file size (50 → 156 µs across
  a 100× range) while reading as correct. Now flat at ~46 µs.
- A reported "24.8 ms per keystroke" turned out to be a **benchmark artifact**: criterion's
  `iter_batched` charged tree-sitter arena teardown to every sample. The real figure was
  already near budget. The lesson — suspect the measurement before the code — is recorded
  at the top of the baseline document.
- **Click-to-column was off by 284 px on every click.** The hit-test subtracted only the
  gutter from a window-relative x, ignoring the activity bar and sidebar the row sits inside,
  so column 0 was unreachable by any click. The in-code comment claiming it was "exact enough"
  was simply false.
- **Save-as could silently lose edits.** gpui's save panel is not app-modal, so the editor
  keeps accepting keystrokes while the user browses folders. Those edits went nowhere, and
  marking the buffer clean afterwards made ⌘S a no-op — leaving them unrecoverable. Saves now
  only mark clean if the buffer still holds the text that was written.
- **One shared task slot made every async operation cancel the last one.** ⌘O then ⌘S dropped
  the folder load, so the tree never appeared and no error was shown anywhere.

[unreleased]: https://github.com/richasdev/ellefuanti/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/richasdev/ellefuanti/releases/tag/v0.1.0
