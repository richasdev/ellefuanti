# Changelog

All notable changes to this project are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
