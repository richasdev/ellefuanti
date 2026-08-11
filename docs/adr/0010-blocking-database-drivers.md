# ADR-0010: Blocking database drivers; rusqlite first, SQLx rejected

**Status:** Accepted · 2026-08-12 · _Governs #65 (database viewer), not yet built_

## Context

#65 wants a schema browser and query editor for the project's database. #25 named SQLx
(MySQL, PostgreSQL, SQLite) and flagged the check that decides everything: **SQLx is
async and expects a runtime**, and ADR-0007 rules out adding tokio to the app. That
question had to be resolved before any viewer code exists, because it shapes the crate
choice, the panel's threading, and the domain crate's API.

## Decision

**Blocking drivers, behind an ordinary synchronous API in a domain crate, exactly like
every other subsystem (ADR-0007).** Queries run in `cx.background_spawn`; cancellation is
dropping the task; the driver never knows which executor runs it.

Concretely, in dependency order:

1. **SQLite via `rusqlite` — already in the tree.** The index (ADR-0008) made rusqlite a
   dependency long ago, it is synchronous, bundled, and battle-tested here. Laravel's
   default `DB_CONNECTION` has been `sqlite` since v11, so the first slice of #65 covers
   the default Laravel project with **zero new dependencies**.
2. **MySQL and PostgreSQL later, behind the same trait, when someone asks.** Candidates
   at that point: the `mysql` crate (genuinely synchronous) and `postgres` (a synchronous
   facade that embeds a private runtime — acceptable under ADR-0007's own escape hatch:
   _"it gets its own runtime at the edge without the rest of the codebase having been
   written against async traits"_). Neither is added now; a driver nothing exercises is
   a dependency bought for nobody (the ADR-0008 rule about tables nothing writes to).

**SQLx is rejected**, on two grounds that survive any future driver addition:

- Its API is async end to end, which would either pull tokio into the app (ADR-0007) or
  force a block-on bridge at every call site — a worse shape than a blocking driver even
  where a bridge is feasible.
- Its headline feature, compile-time query checking, is **structurally useless here**:
  every query the viewer runs is user-typed at runtime. The usual argument for SQLx does
  not apply, which is what widened the field to blocking drivers in the first place.

## Consequences

**The viewer's threading is already designed.** It is the same pattern as every panel:
blocking call on the background pool, one `Task` slot per query so a new query drops the
old one, no connection at startup (#25's "never loaded at startup" — a down database must
not block or error-dialog a project open).

**Credentials stay out of the crate boundary's debt.** A blocking domain API takes a
connection config struct; the `.env` reading and the do-not-echo rules (#65's hard
constraint) live in one place on the app side, and the driver crate never formats a
connection string for display or logs.

**The reversibility ADR-0007 promised is exercised, not just claimed.** If a driver with
no synchronous facade ever becomes necessary, it brings its own runtime at that edge; the
domain trait stays blocking and nothing else in the codebase changes.

**SQL highlighting (#53) becomes the query editor's prerequisite**, as #65 already notes
— the reason to add the grammar is this panel, and it should land with it.
