# ADR-0005: Tree-sitter for incremental parsing; node-kind highlighting for now

**Status:** Accepted · 2026-08-09

## Context

Highlighting and, later, structural navigation need a parse tree that survives editing
without a full reparse per keystroke (§8, §27).

## Decision

Use `tree-sitter` 0.26 with `tree-sitter-php` 0.24, replaying buffer edits as `InputEdit`s
and reparsing with the previous tree. For Milestone 1, derive highlight styles by mapping
**node kinds** directly, rather than loading `highlights.scm` query files.

## Consequences

**Why tree-sitter.** It is the only mature incremental parser with a PHP grammar, error
recovery good enough to highlight code mid-edit, and a coordinate model (`InputEdit`) that
maps cleanly onto our `Edit` type.

**Version skew, checked rather than assumed.** `tree-sitter-php` 0.24 targets tree-sitter
0.24 while we are on 0.26, which is a plausible ABI break. We compiled and parsed with the
pair before committing to it: it works, and a test pins the behaviour so an upgrade that
breaks it fails loudly.

**Why node kinds instead of query files.** The query-based `tree_sitter_highlight` path is
the eventual right answer, but it means shipping and versioning `.scm` assets per grammar
and a capture-name-to-style mapping — infrastructure that pays off across many grammars.
With one grammar, a `match` on node kind is fewer moving parts and no assets to keep in
sync. The upgrade trigger is explicit: **when the second real grammar lands and the match
arms start duplicating.** Recorded in a `ponytail:` comment at the call site.

**One non-obvious correctness point.** PHP nests styled nodes inside styled nodes:
`variable_name "$name"` contains a `$` child, and `string "'ana'"` contains its own quote
children. Emitting a span per styled node therefore produces overlaps, and the first
implementation resolved them innermost-first — which shrank `$name` to `$`. The rule is
**outermost wins**, since the renderer wants one colour per region and the outer node
carries the meaning. A regression test pins both cases.

Highlighting is computed per **visible byte range**, and subtrees that cannot intersect it
are pruned, so cost tracks the viewport rather than the file.
