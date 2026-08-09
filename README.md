# ellefuanti

A native, GPU-accelerated IDE built **for** PHP, Laravel, Livewire and Blade — not a
generic editor that happens to support them.

Written in Rust on [GPUI](https://gpui.rs). No Electron, no webview, no Monaco.

> **Status: preparing 0.1.0.** The editor foundation is built — rope buffer, incremental
> parsing, highlighting, file tree, tabs, palette, quick open, multi-session terminal, and a
> generic LSP client. 258 tests, clippy clean.
>
> **The UI has not been visually verified** ([#35](https://github.com/richasdev/ellefuanti/issues/35)),
> and that is the main thing standing between this and a release. See
> [CHANGELOG.md](CHANGELOG.md) for the honest limitations list and
> [docs/MILESTONE-1.md](docs/MILESTONE-1.md) for what works.

## Why

PhpStorm understands Laravel but is heavy. Zed is fast but framework-agnostic. VS Code is
discoverable but a browser in a trenchcoat. The goal is one tool that opens and responds
like Zed, is as approachable as VS Code, and understands Eloquent, routes, migrations and
Livewire as deeply as PhpStorm does.

Performance is treated as a feature, not a later optimisation pass:

| Metric            | Target     |
| ----------------- | ---------- |
| Cold startup      | < 500 ms   |
| Warm startup      | < 150 ms   |
| Cached completion | < 50 ms    |
| Idle RAM          | 100–200 MB |

## Build

Requires Rust stable (1.85+ for edition 2024; developed on 1.94) and macOS.

```sh
cargo run
```

Full Xcode is **not** required. GPUI's build script shells out to `xcrun metal`, which ships
only with Xcode — so `runtime-shaders` is on by default and shaders compile at launch
instead.

That default costs startup time, so release builds turn it off and precompile:

```sh
cargo build --release -p ellefuanti --no-default-features   # needs full Xcode
```

CI does this on every push, precisely because the release path can break while `cargo build`
keeps working locally. See [ADR-0002](docs/adr/0002-gpui-for-ui.md).

```sh
cargo test        # 258 tests across every crate
cargo bench       # performance baselines — see benchmarks/BASELINE.md
```

## Architecture

Three layers, one direction of dependency. **Only `crates/app` may depend on `gpui`** —
enforced by a test, not by convention, so the Laravel engine can never grow a UI dependency.

```
crates/
├── app/         gpui: window, views, keymap, editor rendering
├── core/        command registry
├── text/        rope buffer, undo/redo, edit log
├── syntax/      tree-sitter incremental parsing, highlighting
├── workspace/   filesystem, lazy file tree, project index, safe file IO
├── terminal/    PTY sessions, VT/ANSI emulation
└── lsp/         generic LSP client with substitutable backends
```

Seven crates, not the eighteen the spec sketches. Crates are added when there is code to put
in them.

Read next:

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — layers, editor design, async model, state
  management, performance strategy
- [docs/adr/](docs/adr/) — why Rust, why GPUI, why a rope, why tree-sitter, why SQLite
- [docs/RISKS.md](docs/RISKS.md) — what could sink this and what is being done about it
- [docs/MILESTONE-1.md](docs/MILESTONE-1.md) — the task breakdown

## Roadmap

| Milestone | Scope                                                                                   |
| --------- | --------------------------------------------------------------------------------------- |
| **0.1.0** | Editor foundation, shippable: open a project, browse, edit, save, run a terminal        |
| **1**     | ✅ Editor: window, file tree, tabs, custom rope editor, PHP/Blade highlighting, palette |
| **2**     | PHP: generic LSP client, completion, diagnostics, hover, definition, references         |
| **3**     | Laravel: project index, models, migrations, routes, Artisan, Laravel panel              |
| **4**     | Livewire/Blade: component indexing, PHP ⇄ Blade navigation, `wire:` completion          |
| **5**     | Tools: Git, database explorer, Docker, tests, log viewer                                |
| **6**     | Advanced: Composer UI, queues, HTTP client, Xdebug debugger                             |
| **7**     | Extensibility: plugin system, opt-in provider-agnostic AI completion                    |
| **8**     | Embedded browser preview                                                                |

Every tool integration is a leaf in the dependency graph: a broken Docker daemon cannot
break the editor, and a broken LSP cannot stop you typing.

## Sequencing, not exclusion

Plugins, AI completion, a debugger, an embedded browser and a multi-session terminal are all
committed features — Milestones 6–8. They are sequenced **after** the editor foundation, on
the principle that an IDE with a mediocre editor is not rescued by having a browser in it.

One rule holds regardless of what gets built: **nothing is sent to an external service
without explicit consent.** Source code, `.env` files, database contents, credentials and
tokens stay local. AI is opt-in and provider-agnostic — you choose the provider and supply
your own key.

## License

Apache-2.0.
