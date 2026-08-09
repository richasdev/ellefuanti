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
CLT alone, so a fresh checkout builds without a 40 GB install. If runtime shader
compilation ever proves to cost startup time we measure it and reconsider; correctness of
the build for contributors comes first.

**The real risk.** GPUI is pre-1.0 and openly warns of breaking changes between versions.
We accept churn on upgrades in exchange for years of saved work, and we contain it: gpui
appears in exactly one crate (`crates/app`, see ADR-0004), so a breaking change is a
bounded refactor of the presentation layer rather than a rewrite. Windows is not currently
a supported GPUI target, which bounds §1's "prepared for Windows" to the domain layers.
