# ADR-0011: WKWebView for the preview pane, and nowhere else

**Status:** Accepted · 2026-08-13

## Context

Editing a Blade template means alt-tabbing to a browser to see the result. Issue #31 asks
for a preview pane so the edit→refresh loop stays inside the IDE.

This runs straight into [ADR-0001](0001-rust-and-native-desktop.md), which rules out a
webview. That decision is not stale and is not being softened here, so the tension has to be
named precisely rather than waved through: **ADR-0001 rejects a webview as the substrate of
the editor** — the thing that draws text, handles every keystroke, and owns the startup path.
A pane that renders a website the user asked to see is a different claim on the process, and
it deserves its own decision rather than inheriting either answer by analogy.

## Decision

Use the platform webview — `WKWebView`, via the `objc2-web-kit` bindings — for a preview
pane only. The editor, its text rendering, and every part of the startup path stay exactly as
ADR-0001 requires.

Three constraints make this a different decision from the one ADR-0001 refused:

1. **The engine is not ours to ship.** WebKit is a system framework, dynamically linked. It
   costs approximately nothing in the binary, which is the constraint that actually binds
   here — see below.
2. **It is lazily loaded.** Nothing about a browser belongs in the startup path. The pane
   constructs its webview the first time it is opened, and a user who never opens it pays
   for it neither in startup time nor in idle memory.
3. **It renders the user's app, not our UI.** No part of the IDE's own chrome moves into
   HTML. The moment a webview draws something that is not the previewed site, this ADR has
   been exceeded and ADR-0001 governs again.

## Why not the alternatives

**CEF** is portable and would survive a future Linux port, but it ships a whole browser
engine: tens of megabytes, against a binary gate with roughly 0.14 MB of headroom. It is not
a close call today.

**Servo** is promising and immature. Betting the feature on it means inheriting its bugs in
a pane whose entire value is showing the site faithfully.

**WKWebView** is macOS-only, which matches where the app runs today. The cost of that choice
is paid on the day Linux support is real (#63), and it is paid in a leaf: the pane is
replaceable without touching the editor, which is the property that makes accepting the
lock-in reasonable rather than reckless.

## The binary cost, measured rather than assumed

The gate is 19 MB and the binary sat at 18.86 MB, so "does this fit" was the question that
had to be answered before any feature work — and answered with a number, not a guess.

Measured by adding `objc2-web-kit` plus `objc2` to `crates/app`, constructing a real
`WKWebViewConfiguration` (a declared-but-unused dependency is discarded by the linker and
would have measured a misleading zero), and building `--release`:

|                                     | binary       |
| ----------------------------------- | ------------ |
| before                              | 18.86 MB     |
| with a real `WKWebView` constructed | **18.85 MB** |

No measurable cost, because the engine lives in the system and only the bindings are linked.
Two new direct dependencies are needed — `objc2-web-kit`, and `objc2` itself, which reaches
`crates/app` only transitively today.

One API detail found while probing, recorded because it shapes the threading story:
`WKWebViewConfiguration::new` requires a `MainThreadMarker`. The webview is main-thread-bound
by construction, which is where GPUI's UI work already runs.

## Consequences

The preview pane can be built without spending the binary budget, which was the open
question.

`crates/app` gains two direct dependencies and a second rendering technology inside the
process. The containment is the whole point: the pane is a leaf, it loads lazily, and it
never draws the IDE's own interface. If a future change wants HTML anywhere else, that is a
new decision and this ADR does not authorise it.

Linux support later means replacing the pane, not unpicking it from the editor.
