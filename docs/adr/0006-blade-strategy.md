# ADR-0006: Blade via the PHP grammar plus a directive scanner

**Status:** Accepted · 2026-08-09 · *Revisit at Milestone 4*

## Context

Blade templates mix HTML, `@directives`, `{{ echoes }}`, `{{-- comments --}}` and raw PHP.
§8 calls for "tratamento especializado"; §11 requires Livewire's `wire:click` and
`wire:model` completion, which needs structural knowledge of the template.

## Decision

For Milestones 1–3, parse `*.blade.php` with the **PHP grammar** (`LANGUAGE_PHP`, not
`PHP_ONLY`, so text outside `<?php` tags parses as HTML text rather than erroring) and
layer a **literal scanner** over the buffer for Blade-specific constructs.

Revisit at Milestone 4, when Livewire support needs real structure.

## Consequences

**Why not a real Blade grammar now.** The correct long-term answer is a Blade grammar with
tree-sitter injections for the embedded PHP and HTML regions. That is grammar authoring and
maintenance work, and Milestones 1–3 need exactly one thing from Blade files: correct
colours. A scanner delivers that in a fraction of the code.

**Why a scanner is defensible and not a hack.** It reads buffer text directly, so it cannot
desync from the parse tree — there is no second tree to keep in sync. It handles the cases
that actually appear: `@@` escapes a literal `@`; `{{--` is matched before `{{` so comments
win; and an unterminated `{{` highlights to the end of the slice so colours do not flicker
while the user is mid-keystroke. It scans the visible range plus a small pad, so a construct
straddling the viewport edge still colours.

**What it cannot do**, and therefore what forces the revisit: it has no tree, so no
component-tag navigation, no slot awareness, no `wire:` attribute structure, no
Blade-aware folding. Milestone 4's Livewire features (§11) are the trigger, and the
decision is recorded as a `ponytail:` comment on the `Language::Blade` variant so the
upgrade path is visible from the code.

## Amendment · 2026-08-12 — the Milestone 4 revisit, resolved as scan-first

The revisit this ADR scheduled has happened (#24). The decision: **extend the scanner
family, not adopt a grammar yet** — a `wire:` attribute context scanner and a Livewire
class extractor, the same single-file scan contract as `extract_routes`/`extract_model`,
which has now paid out five times (routes, models, migrations, column contexts, scopes).

Grounds, over adopting a community tree-sitter-blade grammar now: injections into this
pipeline are new ground (the highlighter is single-tree per buffer); the PHP
`highlights.scm` episode showed community grammars can be unable to reproduce what our
colour tests already pin; and a scan-shaped extractor becomes a consumer of the tree if
one arrives later — nothing is foreclosed.

**The recorded ceiling**: deep slot awareness and Blade-aware folding stay out of reach
of scans. If Milestone 4 needs those early, the fallback is the grammar (option A in
#24's analysis), and this amendment is the marker to revisit *again* at that point.
