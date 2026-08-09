# ADR-0004: Only the app crate may depend on GPUI

**Status:** Accepted · 2026-08-09

## Context

§3 requires presentation, domain and infrastructure to be separated: Laravel intelligence
must not depend on GPUI, the database layer must not depend on UI components, Git must not
depend on the editor. Layering stated only in prose erodes on the first deadline.

## Decision

`crates/app` is the **only** crate permitted to depend on `gpui`. Every other crate is a
plain Rust library. This is enforced by a test that walks every `Cargo.toml` in the
workspace and fails if `gpui` appears outside `crates/app`.

## Consequences

The domain layers are testable with `cargo test` in milliseconds, with no window, no GPU
and no display — which is why `elle-text`, `elle-syntax` and `elle-workspace` carry real
test suites instead of being validated by clicking around.

It also bounds ADR-0002's main risk. GPUI is pre-1.0 and will break; a breaking change
becomes a bounded refactor of one crate instead of a rewrite of the Laravel engine.

Two further payoffs, both required by later milestones: the same crates can back a
headless CLI or an LSP server for CI, and the domain layers are already portable to Linux
and Windows even where GPUI is not (§1).

**The cost** is real: view state must be translated at the boundary rather than shared, so
the presentation layer holds adapter code that would not exist in a monolith. Accepted —
that translation layer is exactly where UI concerns belong, and the enforcement test means
the line cannot quietly move under deadline pressure.
