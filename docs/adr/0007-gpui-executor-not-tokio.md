# ADR-0007: GPUI's executor, not tokio; blocking domain APIs

**Status:** Accepted · 2026-08-09

## Context

§22 requires that filesystem, indexing, search, Git, LSP, database, Docker and test runs
all execute off the UI thread, and that obsolete work be cancellable.

## Decision

Use gpui's own executor: `cx.background_spawn` for blocking work, `cx.spawn` for
main-thread continuations. Do not add tokio. Keep the domain and infrastructure crates
**synchronous and blocking** — they do not spawn and do not know which executor runs them.

## Consequences

**One runtime.** GPUI already ships a work-stealing background pool and a main-thread
foreground executor, integrated with the platform event loop. Adding tokio would mean two
thread pools competing for the same cores, plus a bridge at every boundary — cost with no
benefit while nothing in the stack demands tokio specifically.

**Blocking APIs are a feature, not laziness.** Because `elle-workspace::read_file` is an
ordinary blocking function, it is testable without a runtime, and the async choice stays
reversible: if a future subsystem genuinely needs tokio (some database or gRPC client), it
gets its own runtime at the edge without the rest of the codebase having been written
against gpui's async traits.

**Cancellation is structural.** Dropping a gpui `Task` cancels it. Request-shaped
operations store their `Task` handle in the view, so issuing a new one drops the old — no
cancellation tokens threaded through call stacks. Milestone 1 uses this for folder loading;
Milestone 2 reuses the identical pattern for completion, which is where §22's "se o usuário
digitar rapidamente" actually bites.

**The discipline this requires:** a blocking call must never be made directly from a render
or event handler. The compiler does not catch it. Code review and the `tracing`-based frame
timing do.
