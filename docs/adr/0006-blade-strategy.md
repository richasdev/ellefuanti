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
