# ADR-0003: Rope text storage, byte-offset API

**Status:** Accepted · 2026-08-09

## Context

The editor must edit multi-megabyte files without rebuilding the document per keystroke
(§7), and must feed byte-accurate edit coordinates to tree-sitter and later to LSP.

## Decision

Store text in a `ropey::Rope`. Expose **byte offsets** in the public API of `elle-text`,
converting to ropey's char indices internally. Snap out-of-boundary offsets **down** to
the nearest char boundary. Store undo history as inverse `Edit`s, not snapshots.

## Consequences

**Why a rope.** A `String` makes every insert O(n) with a memmove of the file tail; at a
few megabytes that is visible latency on every keystroke. A rope splices in O(log n) and,
just as importantly, gives cheap line/byte/char conversions — which the renderer needs per
frame to map visible rows to text.

**Why bytes at the boundary.** Ropey counts chars; tree-sitter's `InputEdit` and gpui's
text layout count bytes. Any code path that mixes the two units corrupts files containing
multibyte characters — in a Portuguese-language Laravel codebase, effectively all of them.
Picking one unit at the API boundary and converting exactly once, internally, is what makes
the failure mode impossible rather than merely unlikely.

**Two ropey behaviours that bit the first implementation**, recorded so nobody rediscovers
them the hard way:

- `Rope::chars()` is not a `DoubleEndedIterator`, so `.rev()` does not compile. Index from
  the end by char instead.
- `try_byte_to_char` is **not** a boundary check. It returns `Ok` for a byte in the middle
  of a codepoint, silently rounding down. A guard written on top of it does nothing. The
  working normalisation is a round trip: `char_to_byte(byte_to_char(offset))`.

**Snap, don't panic.** A click or a column calculation legitimately lands mid-codepoint.
Placing the cursor at the start of that character is correct behaviour; crashing is not.

**Why inverse edits, not snapshots.** Undo memory then scales with what changed rather than
file size × history depth, and the same `Edit` values drive tree-sitter reparsing and, later,
LSP change notifications. One representation, three consumers.

Coalescing is by intent: a run of typing undoes as one step; a newline, cursor jump or save
breaks the run.
