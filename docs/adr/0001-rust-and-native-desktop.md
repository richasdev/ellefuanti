# ADR-0001: Rust, native desktop, no webview

**Status:** Accepted · 2026-08-09

## Context

The product goal is an IDE that opens and responds like Zed on a large Laravel codebase:
cold start under 500 ms, idle RAM near 100–200 MB, no input latency the user can feel.

## Decision

Write the IDE in Rust as a genuinely native desktop application. No Electron, no webview,
no browser engine anywhere in the process.

## Consequences

**Why this and not the easy path.** Electron would make the UI trivial and the performance
targets unreachable: the runtime alone exceeds the entire RAM budget before any project is
open, and startup is dominated by spinning up a browser. There is no amount of application
optimisation that recovers that, because the cost is the platform's, not ours.

Rust also removes the class of bug that kills an editor's credibility — a data race
between an indexer and the render loop, a use-after-free on a buffer being edited while
parsed. The ownership model makes "indexing runs off the UI thread" a compile-time fact
rather than a code-review convention.

**What it costs.** Everything the browser gave away free is now ours to build: text
layout, the editor, virtualised lists, tabs, menus. Milestone 1 is therefore mostly
foundation rather than features. Accepted, because performance is the product's reason to
exist (§2) and it cannot be retrofitted later.

The team must be fluent in Rust; there is no HTML/CSS escape hatch for UI work.
