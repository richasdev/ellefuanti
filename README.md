# ellefuanti

A native, GPU-accelerated IDE that aims for **Zed's speed with PhpStorm's understanding of
Laravel** — written in Rust on [GPUI](https://gpui.rs). No Electron, no webview, no Monaco.

> **v0.4.0 is out.** Editor, LSP intelligence, Eloquent/route/Livewire awareness, Git /
> Database / Docker / Composer / test panels, an opt-in AI chat panel with Ask and Agent
> modes, inline AI autocomplete, LSP inlay hints, an embedded browser preview, and proper
> IME / dead-key composition. 1506 tests, clippy clean.
> [Changelog](CHANGELOG.md) · [Latest release](https://github.com/richasdev/ellefuanti/releases/latest)

---

## Download

### Homebrew — the one that just works

```sh
brew tap richasdev/ellefuanti
brew install --cask ellefuanti
```

`brew upgrade` keeps it current afterwards. This is the recommended path because it avoids
both of macOS's annoyances at once: **no Gatekeeper warning**, and **no disk image left
mounted** for the Finder to trip over later.

### Or one line, without Homebrew

```sh
curl -fsSL https://raw.githubusercontent.com/richasdev/ellefuanti/main/scripts/install.sh | sh
```

Downloads the current release, installs it, clears the quarantine flag, opens the app. No
warning at any point.

### Or the .dmg, by hand

**[⬇ Download ellefuanti for macOS (.dmg)](https://github.com/richasdev/ellefuanti/releases/latest)**

Open it, drag **ellefuanti** onto **Applications**, then open it with
**right-click → Open → Open**.

That middle gesture matters. **Double-clicking the first time shows a warning with no way
past it** — macOS offers only "Move to Trash". Right-click → Open is the same act with an
Open button attached, and you only do it once.

Afterwards, **eject the disk image** (the ⏏ next to it in the Finder's sidebar). Leaving it
mounted is what produces the later, confusing _"can't be moved to the Trash because it can't
be deleted"_ — that is the mounted copy, not your installed app. The two paths above unmount
it for you, which is most of why they are listed first.

<details>
<summary><b>What the one-line installer does, step by step</b></summary>

Nothing hidden — it is the same commands you would type, and you can
[read it here](scripts/install.sh) before running it:

```sh
# The URL carries no version number, so it always resolves to the current release.
curl -fL -o /tmp/ellefuanti.dmg \
  https://github.com/richasdev/ellefuanti/releases/latest/download/ellefuanti-macos.dmg
hdiutil attach /tmp/ellefuanti.dmg -nobrowse -quiet
# `ditto`, never `cp -R`: cp drops the bundle's _CodeSignature/ directory, and macOS
# calls a bundle with a broken signature "damaged" — the exact failure this avoids.
ditto "/Volumes/ellefuanti/ellefuanti.app" /Applications/ellefuanti.app
hdiutil detach "/Volumes/ellefuanti" -quiet
xattr -dr com.apple.quarantine /Applications/ellefuanti.app
open /Applications/ellefuanti.app
```

</details>

Already installed and just want the warning gone? This one line is the whole fix:

```sh
xattr -dr com.apple.quarantine /Applications/ellefuanti.app
```

<details>
<summary><b>Why does macOS warn about this app?</b></summary>

Because the build carries no Apple Developer certificate — that needs a paid account
(US$ 99/year) — so Gatekeeper treats it as an unidentified developer and warns about anything
downloaded from the internet.

**Homebrew and the one-line installer avoid the warning entirely**, and it is worth knowing
why, because it is neither magic nor a signature. macOS stamps `com.apple.quarantine` on
files a _browser_ downloads, and Gatekeeper only blocks apps carrying that flag. `curl` never
stamps it, and both paths strip it anyway. The app is still unsigned either way — `spctl`
still answers `rejected`, and a `.dmg` you download in a browser will still warn. What
changes is the flag, not the trust.

This is how most unsigned open-source Mac apps stay pleasant to install. The ones that show
no warning at all have simply paid Apple — including the free and open-source ones.

Two different warnings exist and they are worth telling apart. **"Unidentified developer"**
is the one you should see: annoying, but right-click → Open gets through it. **"Is damaged
and can't be opened"** is a different thing — it means the app bundle itself is broken, and
no amount of clicking helps. Releases up to v0.3.0 showed that second message because the
bundle was genuinely malformed; that is fixed, and every release is now checked against a
real download before it ships (see [RELEASE.md](RELEASE.md)).

Signing and notarisation would remove the warning entirely and are on the roadmap.

</details>

**Updates install themselves.** From v0.2.1 onward the app checks for new releases and shows
`Update v0.x.y ↓` in the status bar — one click downloads and installs it, a second click
restarts into the new version. No re-downloading by hand.

### Recommended: a PHP language server

Completion, diagnostics, hover and go-to-definition come from a language server. Install
[Intelephense](https://intelephense.com/) and it is picked up automatically:

```sh
npm install -g intelephense
```

Everything else — editing, highlighting, the file tree, Git, terminal, panels — works
without it.

### Requirements

macOS (Apple Silicon or Intel). Linux and Windows are not supported yet; see
[the roadmap](#roadmap).

---

## What this is

I work in PHP, Laravel and Livewire every day, and the two editors I kept switching between
each got one half right:

- **PhpStorm** genuinely understands the framework — Eloquent, routes, migrations, Blade —
  but it is heavy, and you feel every one of those milliseconds.
- **Zed** is astonishingly fast and pleasant to type in, but it is framework-agnostic: it
  does not know what `route('users.show')` means.

**ellefuanti is an attempt to have both at once**: an editor that opens and responds like
Zed, and a project index that knows your models, routes and components like PhpStorm does.
That is the whole idea.

**Today it is deliberately narrow.** The Laravel/Livewire/Blade features are built first and
built deepest, because that is the stack I use at work and the only way to know whether the
framework awareness is actually good is to depend on it daily. Being honest about this beats
claiming general-purpose support that is not there.

**Tomorrow it does not have to stay narrow.** Nothing in the architecture is PHP-specific:
the editor, the rope buffer, the tree-sitter layer, the LSP client and the panels are all
language-agnostic by construction — the Laravel logic lives in one crate that everything
else is forbidden from depending on (a test enforces it). Adding another language means a
grammar and a language server, not a rewrite. Syntax highlighting already ships for
JavaScript, TypeScript, Rust, JSON, CSS, HTML, YAML, TOML, Bash and Markdown, and other
languages already work as an editor with LSP support.

So: **a Laravel IDE first, a multi-language IDE eventually.** In that order, on purpose.

### Performance is a feature, not a later pass

| Metric            | Target     |
| ----------------- | ---------- |
| Cold startup      | < 500 ms   |
| Warm startup      | < 150 ms   |
| Cached completion | < 50 ms    |
| Idle RAM          | 100–200 MB |

These are enforced by a gate in CI, not aspirations in a document.

---

## What's in it

**Editor** — custom rope-backed editor, multi-cursor, find & replace (project-wide too),
folding, bracket matching, PHP smart typing (quotes auto-close in code, `=` becomes `=>`
inside arrays), drag & drop from Finder and within the file tree, zen mode (⌘K Z) and
fullscreen (⌃⌘F), and thirteen themes — Dark, Light, One Dark Pro, GitHub Dark/Light,
Dracula, Nord, Catppuccin Mocha/Latte, Gruvbox, Tokyo Night and Solarized Dark/Light —
plus VS Code theme import.

**PHP & Laravel** — LSP completion, diagnostics, hover, go-to-definition and find-references;
a SQLite project index over models, migrations and routes; Artisan command palette;
Blade and Livewire awareness with `wire:` completion.

**Panels** — Git (status, diff, branches, log, push), database explorer (schema browser and
row editing), Docker, Composer, test runner, log viewer, an integrated terminal, and an
embedded browser preview so the edit→refresh loop stays in one window. The preview is loaded
lazily: never open it and it costs nothing at startup.

**AI, entirely opt-in** — a chat panel (⌘⇧A) and inline ghost-text autocomplete. You choose
the provider: an Anthropic API key, your `ant` CLI login, or any OpenAI-compatible endpoint
including a **local Ollama, where nothing leaves your machine**. Both features are off by
default. See [Privacy](#privacy-and-ai) below — this part has hard rules.

---

## Privacy and AI

**Nothing is sent anywhere without your explicit action.** This is a design constraint, not
a preference:

- AI chat and AI autocomplete are **off by default** and each has its own switch.
- You pick the provider and supply your own key. There is no vendor baked in, and no
  telemetry of any kind.
- API keys live in the **macOS Keychain**, never in the settings file.
- Chat context is attached **per item, by you** — the current selection, a specific file —
  and is visible and removable before you send. Nothing is attached automatically.
- A **denylist with no override** refuses `.env` files, SSH keys, `.pem`/`.key` material,
  sqlite databases and anything named like a credential or token.
- Autocomplete sends a window around your cursor from the **current file only** — never the
  project.

[docs/RISKS.md](docs/RISKS.md) documents this as an explicit risk, including where a
denylist is not a guarantee.

---

## Settings

`~/Library/Application Support/ellefuanti/settings.json`, created on the first change — or
edit everything in the settings panel (⌘,). The app runs fine without the file, and every key
has a sensible default.

| Key                   | Type   | Default              | Meaning                                         |
| --------------------- | ------ | -------------------- | ----------------------------------------------- |
| `theme`               | string | `"dark"`             | Any built-in or shipped theme name              |
| `font_family`         | string | system monospace     | Must measure as monospace                       |
| `font_size`           | number | `13`                 | Editor text size                                |
| `ui_font_size`        | number | `13`                 | Chrome text size                                |
| `line_height`         | number | `1.5`                | Multiplier                                      |
| `autosave`            | bool   | `true`               | Save dirty tabs when the window loses focus     |
| `ai.provider`         | string | `"anthropic"`        | `anthropic`, `ant`, or `custom`                 |
| `ai.base_url`         | string | `""`                 | For `custom` — e.g. `http://localhost:11434/v1` |
| `ai.chat`             | bool   | `false`              | The chat panel                                  |
| `ai.chat_mode`        | string | `"ask"`              | `ask` (read-only) or `agent` (proposes edits)   |
| `ai.autocomplete`     | bool   | `false`              | Inline ghost text                               |
| `ai.chat_model`       | string | `"claude-opus-5"`    | Model for the chat panel                        |
| `ai.completion_model` | string | `"claude-haiku-4-5"` | Model for autocomplete (latency matters here)   |

A file that is not valid JSON is reported with its line and column, and the app launches on
defaults **without saving over it** — your text stays there to fix. A single key of the wrong
type falls back to that key's default and costs nothing else. Unrecognised keys are preserved
on save, so downgrading never loses configuration.

Settings are global; there is no per-project file, deliberately —
[ADR-0009](docs/adr/0009-json-settings-global-only.md) explains why.

---

## Build from source

Requires Rust stable (1.85+ for edition 2024) and macOS.

```sh
cargo run
```

Full Xcode is **not** required for development. GPUI's build script shells out to
`xcrun metal`, which ships only with Xcode — so `runtime-shaders` is on by default and
shaders compile at launch instead.

That costs startup time, so release builds turn it off and precompile:

```sh
cargo build --release -p ellefuanti --no-default-features   # needs full Xcode
```

CI does this on every push, precisely because the release path can break while `cargo build`
keeps working locally. See [ADR-0002](docs/adr/0002-gpui-for-ui.md).

```sh
cargo test --workspace   # 1369 tests
cargo bench              # performance baselines — see benchmarks/BASELINE.md
```

---

## Architecture

Three layers, one direction of dependency. **Only `crates/app` may depend on `gpui`** —
enforced by a test, not by convention, so the Laravel engine can never grow a UI dependency.
That is also what keeps the door open for other languages later.

```
crates/
├── app/         gpui: window, views, keymap, editor rendering, AI panels
├── core/        command registry
├── text/        rope buffer, undo/redo, edit log
├── syntax/      tree-sitter incremental parsing, highlighting
├── workspace/   filesystem, lazy file tree, project index, safe file IO
├── terminal/    PTY sessions, VT/ANSI emulation
├── lsp/         generic LSP client with substitutable backends
├── index/       SQLite project index
├── laravel/     route extraction, framework awareness
├── git/         libgit2 status, diff, branches
├── db/          sqlite schema browsing
├── docker/      compose service state
├── test-runner/ Pest/PHPUnit process supervision
├── theme/       theme file format and VS Code theme import
└── settings/    settings.json: read, merge with defaults, write atomically
```

Read next:

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — layers, editor design, async model, state
  management, performance strategy
- [docs/adr/](docs/adr/) — why Rust, why GPUI, why a rope, why tree-sitter, why SQLite
- [docs/RISKS.md](docs/RISKS.md) — what could sink this and what is being done about it

---

## Roadmap

Shipped:

| Version   | Scope                                                                       |
| --------- | --------------------------------------------------------------------------- |
| **0.1.0** | Editor, LSP, Laravel/Livewire awareness, Git/DB/Docker/Composer/test panels |
| **0.2.0** | Drag & drop, tree auto-refresh, active-file indicator, no file-size limit   |
| **0.2.1** | Self-update from within the app                                             |
| **0.3.0** | AI chat panel and inline autocomplete, PHP smart typing, zen mode, 8 themes |
| **0.3.1** | Auto-import, quick-fix lightbulb, subscription chat, resizable panels       |
| **0.3.2** | Inlay hints, chat Agent mode with per-file diff approval, chat attachments  |
| **0.4.0** | IME and dead-key composition, embedded browser preview, universal binary    |

Next, roughly in order:

| Theme            | Scope                                                       |
| ---------------- | ----------------------------------------------------------- |
| Debugging        | Xdebug via DAP                                              |
| Extensibility    | Plugin system                                               |
| Distribution     | Code signing and notarization                               |
| **Beyond PHP**   | More first-class languages once the Laravel core is settled |
| **Beyond macOS** | Linux, when GPUI's support makes it worth doing             |

**Intel Macs:** the binary is universal from v0.4.0, but the x86_64 slice has not yet been
run on real Intel hardware — the development machine is Apple Silicon. Treat Intel support as
untested until someone confirms it.

Every tool integration is a leaf in the dependency graph: a broken Docker daemon cannot break
the editor, and a broken LSP cannot stop you typing.

---

## License

Apache-2.0.
