# ADR-0008: SQLite for the Laravel project index

**Status:** Accepted · 2026-08-09 · *Not yet implemented — Milestone 3*

## Context

§12 requires a persistent incremental index of models, columns, relationships, routes,
migrations, Livewire components and Blade views, with a dependency graph so an edit
reanalyses only what it must. §13 requires the editor to be usable before indexing
finishes.

## Decision

Persist the index in **SQLite**, in a single file under the project's local state
directory. Recorded now, before implementation, because the choice shapes Milestone 3's
design.

## Consequences

**Why persistent at all.** Rebuilding a large Laravel project's index on every launch is
incompatible with a 500 ms cold start. The index must survive restarts, which means a file
format; SQLite is a file format with transactions and a query planner already attached.

**Why SQLite over a hand-rolled format.** The queries this index must answer are
relational — "columns of the table this model maps to", "routes referencing this
controller", "views this component renders". Those are joins. Writing them by hand over
custom binary structures means reimplementing indexes and crash-safe writes, which is where
a hand-rolled format quietly loses.

Its ACID guarantees also matter more than they first appear: an index half-written when the
process dies must not be silently wrong on next launch, because a wrong index produces
wrong autocomplete, which is worse than no autocomplete.

**Consequences for the design.** The index is a cache, never a source of truth: it can be
deleted at any time and rebuilt from the filesystem, which is also the recovery path for
§24 ("Laravel index quebrado não pode impedir PHP básico"). It is populated on a background
thread and queried without blocking the UI. Schema migrations need a version table from the
first commit — retrofitting one is painful.

Open question deferred to implementation: whether writes batch per file or per analysis
pass. That is a profiling question, and §21 forbids guessing at it.
