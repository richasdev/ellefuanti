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

Measured on this machine:

|                                        | Result                                                    |
| -------------------------------------- | --------------------------------------------------------- |
| Default build, CLT only                | **fails**: `xcrun: error: unable to find utility "metal"` |
| `runtime_shaders`, first launch        | **520 ms** — 432 ms of it in the window phase             |
| `runtime_shaders`, subsequent launches | **~200 ms** — 152 ms in the window phase                  |

That first-launch spike looked like runtime shader compilation. It is not — see the
correction below, which is the more important half of this ADR.

## Correction: the shader theory was wrong

The paragraph above originally attributed that 432 ms → 152 ms drop to the OS caching
compiled shaders, and predicted that precompiling them in CI would remove it. **Both claims
were wrong, and the measurement that settled it is worth recording.**

CI now builds the release binary with `--no-default-features` (no `runtime_shaders`, shaders
compiled at build time by a runner that has Xcode). Running _that_ binary on this machine:

| Precompiled-shader binary | Window phase | Total      |
| ------------------------- | ------------ | ---------- |
| first launch              | 496 ms       | **536 ms** |
| subsequent launches       | 152 ms       | ~195 ms    |

Identical to the `runtime_shaders` build within noise — same 152 ms warm window phase, same
~195 ms warm total, and a first launch that is still **over the 500 ms budget**. Precompiling
the shaders changed nothing measurable.

So the first-launch cost is **not shader compilation**. It is OS-level first-launch overhead
that any new Mach-O pays — dyld closure construction, code-signature validation, page-in of a
binary not yet in the file cache. It reproduces on a binary whose shaders were compiled hours
earlier on a different machine, which shader caching cannot explain.

**What this changes.** Nothing about the decision: `runtime_shaders` still stays on by
default, because a build that fails without a 40 GB Xcode install is worse than a first launch
20 ms over budget. But the _reason_ is now honest — it is a buildability convenience, not a
startup optimisation, and the release split does not buy the 432 ms it was introduced to
recover. The split is retained anyway: it is free, it keeps the release path exercised in CI,
and it removes a real if smaller cost.

**The residual is unattributed and will not be guessed at.** Warm startup is ~195 ms against a
150 ms target, with 152 ms of it inside window creation. Attributing that needs a profiler on
the gpui window path, not another theory. The lesson from getting this wrong once: a plausible
mechanism that fits the numbers is not the same as the mechanism, and the way to tell them
apart is to remove the suspected cause and re-measure.

**The real risk.** GPUI is pre-1.0 and openly warns of breaking changes between versions.
We accept churn on upgrades in exchange for years of saved work, and we contain it: gpui
appears in exactly one crate (`crates/app`, see ADR-0004), so a breaking change is a
bounded refactor of the presentation layer rather than a rewrite. Windows is not currently
a supported GPUI target, which bounds §1's "prepared for Windows" to the domain layers.

## Resolution of #57: the default build is the release configuration

`cargo build --release --no-default-features` fails on any machine with only the Command
Line Tools:

```
xcrun: error: unable to find utility "metal", not a developer tool or in PATH
```

The Metal shader compiler ships with full Xcode, not the CLT, and this project demands
Xcode nowhere else. Combined with the measurement above — precompiled shaders changed
nothing measurable, on cold or warm launches — the precompiled path costs a toolchain and
buys nothing. It stays in gpui for anyone who has Xcode and wants it; it is not this
project's release configuration, and `scripts/bundle-macos.sh` no longer suggests it.

**The release build is `cargo build --release -p ellefuanti`, default features.** The
`runtime_shaders` flag it enables compiles shaders during the startup window phase already
counted in the budget above.
