# ADR-0002: GPUI as the UI framework

**Status:** Accepted · 2026-08-09

## Context

A native, GPU-accelerated Rust UI is needed, with real text rendering and virtualised
lists. The realistic options are GPUI (Zed's framework), egui/iced, or hand-rolled Metal.

## Decision

Use `gpui` from crates.io, pinned to **0.2.2**, with the **`runtime_shaders`** feature
enabled.

## Consequences

**Why GPUI.** It is the only mature Rust UI framework built specifically to render a code
editor at 120 fps: real font shaping, `uniform_list` virtualisation, a focus and action
system, and an executor with a main/background split already designed for this shape of
app. egui redraws immediate-mode every frame and its text handling is not built for source
code; hand-rolling Metal means writing a text stack before writing an editor.

Apache-2.0, and published to crates.io for external use.

**Two findings from validation that shape how we depend on it.**

The git `main` branch has split platform setup into a separate `gpui_platform` crate and
removed `Application::new()`, while still reporting version `0.2.2` in its manifest. Code
copied from `main`'s examples does not compile against the release, and the version number
cannot distinguish them. We therefore pin the crates.io release, and if we ever move to
git we pin a **tag**, never a branch.

The default build invokes `xcrun metal`, which ships only with full Xcode — not with the
Command Line Tools. Enabling `runtime_shaders` compiles shaders at runtime and builds with
CLT alone, so a fresh checkout builds without a 40 GB install.

**This costs cold-start time, and the cost is now measured rather than assumed.** Both
sides were verified on this machine:

|                                        | Result                                                    |
| -------------------------------------- | --------------------------------------------------------- |
| Default build, CLT only                | **fails**: `xcrun: error: unable to find utility "metal"` |
| `runtime_shaders`, first launch        | **520 ms** cold start — 432 ms of it in the window phase  |
| `runtime_shaders`, subsequent launches | **~200 ms** — 152 ms in the window phase                  |

The 432 ms → 152 ms drop across launches is the OS caching the compiled shaders. So the
first launch after a build (or after the cache is evicted) **exceeds the 500 ms cold-start
budget from §21**. Later launches clear that bar, but at ~200 ms they still miss the
separate **150 ms warm-start** target — shader caching removes most of the cost, not all of
it, and the residual has not yet been attributed.

**The decision stands, deliberately.** A build that fails for any contributor without a
40 GB Xcode install is a worse product than a first launch that is 20 ms over budget.
Recorded as a known deviation rather than quietly dropped from the target.

**How it gets fixed properly:** ship the release build with shaders precompiled — a CI
machine has Xcode, so the published binary can drop `runtime_shaders` while a local
`cargo build` keeps it. That splits the trade instead of paying it. Tracked as a build-and-
release task; not done yet, because there is no release pipeline to hang it on.

**The real risk.** GPUI is pre-1.0 and openly warns of breaking changes between versions.
We accept churn on upgrades in exchange for years of saved work, and we contain it: gpui
appears in exactly one crate (`crates/app`, see ADR-0004), so a breaking change is a
bounded refactor of the presentation layer rather than a rewrite. Windows is not currently
a supported GPUI target, which bounds §1's "prepared for Windows" to the domain layers.
