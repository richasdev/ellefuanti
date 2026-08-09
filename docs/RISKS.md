# Risks

Deliverable 8 of §28. Ordered by expected damage, not by likelihood.

---

## 1. GPUI is pre-1.0 and will break under us

**Likelihood: high. Impact: medium.**

GPUI's own README warns of breaking changes between versions, and validation already found
the git `main` branch has moved entry-point setup into a separate `gpui_platform` crate
while still reporting version `0.2.2` — so a version number does not distinguish the two
APIs.

**Mitigation.** Pin the crates.io release; if we ever depend on git, pin a **tag**, never a
branch. gpui appears in exactly one crate (ADR-0004), so an upgrade is a bounded refactor
of the presentation layer, not a rewrite. Upgrades happen deliberately, on their own
branch, never bundled with feature work.

**Residual risk.** A GPUI change we cannot absorb — an abandoned framework, or a rewrite in
a direction we cannot follow. The escape hatch is that the domain layers are UI-agnostic
and survive a UI replacement, which is exactly why that boundary is enforced by a test.

---

## 2. Intelephense is proprietary and we depend on it for PHP intelligence

**Likelihood: medium. Impact: high.**

§9 permits Intelephense as the initial PHP LSP; §1 requires it be substitutable. It is
closed-source, licence-gated for its premium features, and its terms could change.

**Mitigation.** Milestone 2 builds a **generic LSP client** first, with Intelephense as one
configured backend behind that interface — not an integration written against its
specifics. Alternatives (phpactor, php-language-server, an eventual first-party engine)
must be drop-in.

**The discipline that makes this real:** no Intelephense-specific behaviour may leak past
the LSP client boundary. The moment Laravel intelligence starts depending on an
Intelephense quirk, substitutability is gone on paper only.

---

## 3. Performance targets are stated but not yet measured

**Likelihood: medium. Impact: high.**

Cold start < 500 ms, idle RAM 100–200 MB, cached completion < 50 ms. Milestone 1 defends
these architecturally — lazy tree, virtualised lists, incremental parsing — but no
end-to-end number has been recorded, and the targets must hold once LSP, indexing and
Docker exist.

**Mitigation.** Criterion benches for the pure layers land with Milestone 1 so regressions
are attributable to a commit. Startup and frame time are instrumented with `tracing`
spans, not estimated. §21's rule is followed literally: no optimisation without a profile,
and no benchmark that measures something other than what it claims.

**Watch item.** `runtime_shaders` compiles Metal shaders at process start. It is enabled for
build portability (ADR-0002) and is a prime suspect if cold start misses budget — measure
before blaming, and reconsider the trade if it is the cause.

---

## 4. Laravel intelligence is static analysis of a dynamic framework

**Likelihood: high. Impact: high.**

This is the product's differentiator and its hardest problem. Eloquent models declare
almost nothing: columns live in migrations, relationships are method bodies returning
builder chains, magic accessors are `__get`, and half the container is resolved at runtime.
Facades, macros and service providers are dynamic by design.

**Mitigation.** Index what is statically derivable — migrations for columns, method
signatures for relationships, route files and attributes for routes — and treat the result
as **best-effort suggestions, never assertions**. §9 already requires showing the origin of
each completion item, which doubles as honesty about confidence: a column from a migration
is a different kind of claim than a guess from a method name.

Never block editing on analysis (§13), and never report a diagnostic we are not sure of —
a false "method does not exist" on working code destroys trust faster than a missing
completion.

**Residual risk.** Some Laravel code is simply not statically knowable. The product's
answer is graceful degradation, not eventual completeness.

---

## 5. Blade and Livewire need structure the scanner cannot provide

**Likelihood: certain by Milestone 4. Impact: medium.**

The scanner in ADR-0006 colours Blade correctly but produces no tree, so it cannot support
`wire:click` completion, component-tag navigation or slot awareness — all required by §11.

**Mitigation.** The limitation is recorded in ADR-0006 with an explicit revisit trigger
(Milestone 4) and a `ponytail:` comment at the code site. A tree-sitter Blade grammar with
injections is the planned upgrade, scheduled where the requirement actually lands rather
than built speculatively now.

---

## 6. Scope: this specification is several products

**Likelihood: high. Impact: high.**

§14–§20 span a database GUI, a Git client, Docker management, a terminal emulator, a test
runner, a log viewer, an HTTP client and a debugger. Each is a product with a full-time
team elsewhere in the industry. A team that starts all of them finishes none.

**Mitigation.** The milestone order in §25 is treated as binding, and §28's instruction —
foundation first, no Laravel/Database/Docker/Git before the editor is solid — is followed
literally. Every tool integration is a leaf in the dependency graph so it can slip without
blocking anything.

The honest framing: **the editor and Laravel intelligence are the product; the tool panels
are features.** If the editor is not excellent, no amount of Docker integration saves it.

---

## 7. Unvalidated dependencies for later milestones

**Likelihood: low. Impact: low–medium.**

Current versions are known but nothing has been compiled against them yet, because no code
needs them before Milestone 2:

| Crate                | Version | For                                                            |
| -------------------- | ------- | -------------------------------------------------------------- |
| `sqlx`               | 0.9.0   | Database explorer (§14)                                        |
| `rusqlite`           | 0.40.2  | Project index (§12)                                            |
| `git2`               | 0.21.0  | Git (§15)                                                      |
| `portable-pty`       | 0.9.0   | Terminal (§17)                                                 |
| `alacritty_terminal` | 0.26.0  | Terminal emulation (§17)                                       |
| `lsp-types`          | 0.97.0  | LSP client (§8)                                                |
| `async-lsp`          | 0.2.4   | LSP transport — preferred over the less-maintained `tower-lsp` |
| `notify`             | 8.2.0   | File watcher (§12)                                             |
| `grep` / `ignore`    | 0.4.x   | Search (§8)                                                    |

**Mitigation.** Validate by compiling a spike at the start of the milestone that needs it —
the same method that caught the `xcrun metal` and ropey boundary problems before they cost
a day of debugging.

---

## 8. macOS-first could become macOS-only

**Likelihood: medium. Impact: low near-term.**

§1 asks for architectural readiness for Linux and Windows. GPUI does not currently list
Windows as a supported target, so the constraint is the UI layer's, not ours.

**Mitigation.** Keep platform-specific code in the app crate, behind narrow interfaces. The
domain and infrastructure layers are already portable. Concretely: no `#[cfg(target_os)]`
outside `crates/app`, and no absolute macOS paths anywhere.

**Honest statement of position:** the domain layers will port cleanly; the UI ports when
GPUI does. That is a bet on GPUI's roadmap, and it is the same bet ADR-0002 already makes.
