# ADR-0012: Plugins are child processes speaking JSON-RPC, not WASM and not dylibs

**Status:** Accepted · 2026-08-13

## Context

Issue #28 asks for a real extension mechanism, and names three candidate boundaries: WASM,
dynamic libraries, or an out-of-process protocol like LSP. It also states the rule that
decides most of the question — §24's fault isolation: **a crashing plugin must not take the
editor down.**

The command system was built for this. Every action already has a stable dotted id
(`editor.save`), defined in `crates/core/src/command.rs` away from the UI, and that id is
what a plugin binds to. The extension point exists; this ADR settles what sits on the other
side of it.

## Decision

A plugin is **a separate process**, launched by the editor, speaking newline-delimited
JSON-RPC 2.0 over stdin/stdout. It is not WASM, and it is not a dynamic library loaded into
the editor's address space.

The first version extends exactly one thing: **commands**. A plugin declares commands in a
manifest, they appear in the palette, and confirming one sends the plugin a request.

## Why not WASM — measured, not assumed

The binary gate is 19 MB and `main` sits at **18.9052 MB**: about 0.09 MB of headroom. So
"does the runtime fit" is not a detail to be settled after the design — it is the first
question, and it has a number for an answer.

Measured by building a binary that constructs a real `wasmtime::Engine`, compiles a real
module and calls an exported function. A declared-but-unused dependency is discarded by the
linker and would have measured a misleading zero — ADR-0011's lesson, applied again. Both
probes use this workspace's exact release profile (`lto = "thin"`, `codegen-units = 1`,
`panic = "abort"`).

|                                   | binary         |
| --------------------------------- | -------------- |
| empty Rust binary                 | 0.4060 MB      |
| the same, plus a working wasmtime | **10.2401 MB** |
| **wasmtime's cost**               | **+9.8341 MB** |

**9.83 MB against 0.09 MB of headroom — 109× the available budget.** The binary would land
at roughly 28.7 MB, about 1.5× the gate. This is not a close call and no amount of feature
paring closes it: `wasmtime` pulls 166 crates, and `wasmer` is the same class of dependency.
WASM is ruled out on size alone, independent of its real merits — sandboxing and language
neutrality are exactly what this feature wants, and they remain unaffordable today.

Recorded so the next person does not re-measure: this verdict is a function of the gate, not
of WASM. If the gate moves substantially or a runtime an order of magnitude smaller becomes
credible, this ADR should be revisited — that is a new decision, not a silent drift.

## Why not dynamic libraries

Cheap in bytes, and rejected on the issue's own fault-isolation rule. A `dlopen`ed plugin
shares the editor's address space: a segfault in it is a segfault in the editor, and it takes
the user's unsaved buffers with it. It also means a stable C ABI across every Rust release
and every plugin, which is a promise this project would break on its first `repr(Rust)`
change.

The cheapness is real and the safety cost is not payable. §24 is the whole reason the
question was asked.

## Why out-of-process is the answer that fits

Fault isolation is _structural_ rather than promised: the plugin is a different process with
a different address space. It crashes, `read_line` returns EOF, the editor drops the handle
and keeps running with the plugin's commands removed. That is the same failure the LSP client
already survives when a language server dies.

It costs nothing in the binary — no runtime is linked, only manifest parsing and a JSON-RPC
codec, and `serde_json` is already compiled for every build. It is language-neutral for free:
anything that can read stdin and write stdout can be a plugin, which is most of what WASM was
wanted for.

The IPC cost is real and irrelevant at this scope. A command invocation happens when a human
picks a palette row; a millisecond of pipe latency is invisible against the human on the
other end. This would be a different decision for a per-keystroke API — which is precisely
why completion providers are **not** in this ADR's scope.

Two working precedents are already in the tree and this follows both rather than inventing a
third shape: `crates/lsp/` (JSON-RPC over a spawned child, with the pipes handed out
separately so tests drive the client with no server installed) and `crates/app/src/ai_codex.rs`
(newline-delimited JSON-RPC, pure parse functions tested against captured fixtures).

## The executor constraint

ADR-0007 forbids tokio and requires blocking domain APIs. This design obeys it without
strain: `elle-plugin` is plain blocking Rust that does not know what executor runs it, and
the app drives the child from `cx.background_spawn` with `std::process` and
`BufReader::read_line` — the identical pattern `ai_chat.rs` uses for the Codex child today.
No async runtime enters the tree.

## API stability

The issue is right that a plugin API is a promise, so it is versioned from this first
release. A manifest declares `api_version`, and the host refuses anything it does not
implement rather than half-running it. The supported version is a single integer, checked in
one pure function with tests — `PLUGIN_API_VERSION`.

The promise is deliberately kept small enough to be worth making: **commands only**. Panels,
themes, language support and completion providers are named in #28 and are _not_ authorised
by this ADR. Each is a materially different API — a panel means handing out rendering, a
completion provider means per-keystroke IPC — and each deserves its own decision rather than
inheriting this one by analogy.

## Consequences

Plugins can crash without taking unsaved work with them, which was the requirement.

The editor gains process lifecycle to manage: a child that hangs on shutdown must be killed,
not waited on. Discovery is a directory scan; nothing is downloaded, and there is no
marketplace, no installation UI, and no sandbox beyond what the OS gives a child process —
**a plugin runs with the user's full privileges**, which is the honest statement of the
trust model and the reason installation stays manual for now.

A palette row is the plugin's only surface. `Dispatch` in `crates/app/src/actions.rs` is a
closed enum over compile-time ids, and plugin ids are runtime strings, so plugin commands are
resolved _before_ that enum is consulted rather than by widening it — the builtin path is
left exactly as it is.
