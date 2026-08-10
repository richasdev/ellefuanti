# assets/icons

In-app UI glyphs for the activity bar (issue #50). Different from `../app-icon.svg`, which
is the product's own identity and is consumed by the operating system — see `../README.md`.

| File           | Source                    | Licence    |
| -------------- | ------------------------- | ---------- |
| `explorer.svg` | Codicons `files`          | CC BY 4.0  |
| `search.svg`   | Codicons `search`         | CC BY 4.0  |
| `git.svg`      | Codicons `source-control` | CC BY 4.0  |
| `database.svg` | Codicons `database`       | CC BY 4.0  |
| `tests.svg`    | Codicons `beaker`         | CC BY 4.0  |
| `laravel.svg`  | Drawn for this repository | Apache-2.0 |
| `docker.svg`   | Drawn for this repository | Apache-2.0 |

## Attribution

Five of the seven are **Codicons**, the icon set Visual Studio Code itself uses:

> Codicons — Copyright (c) Microsoft Corporation.
> <https://github.com/microsoft/vscode-codicons>
> Icons licensed under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/).

The full licence text is in `LICENSE-codicons.txt`. CC BY 4.0 requires attribution, which is
what this file and that copy are for. Codicons' _code_ is MIT; none of it is vendored here,
only the icon artwork.

The files are unmodified upstream SVGs. `explorer.svg` and `git.svg` are taken from tag
**0.0.36** rather than `main`, deliberately — see below.

## Why 0.0.36 for two of them

Upstream is part-way through redrawing the set, and `files` and `source-control` on `main`
have been redrawn with noticeably lighter strokes than the 16x16 icons beside them. Rendered
together, the explorer and git glyphs read as faded next to `database` and `beaker`.

The 0.0.36 drawings are heavier and match. Pinning two files to an older tag is the sort of
thing that looks like an accident later, so: it is not, and the contact sheet that showed the
mismatch is reproducible with `rsvg-convert` at 96px against `theme.panel`.

When upstream finishes the redraw, take all seven from one tag again.

## Why Laravel and Docker are drawn, not borrowed

Codicons has no Laravel glyph and no Docker glyph, and substituting a vaguely-related one is
how two panels end up looking alike — which is the bug this fixes. Database and Docker both
rendered `D` before this change.

The official brand marks were considered and rejected. Laravel's logo is not CC-licensed,
both marks carry trademark terms that a permissive repo licence does not grant, and the
activity bar recolours every glyph to a single flat theme colour — a brand mark flattened to
one colour is usually both a trademark problem and an ugly one.

So both are drawn on the same 16x16 grid as the Codicons around them:

- **`docker.svg`** — two stacked containers, the shape Docker's own UI and most
  infrastructure iconography use for "a running container". Not the whale.
- **`laravel.svg`** — stacked layers, for the framework layer sitting over the app. Not the
  Laravel `L` mark.

Neither is a brand mark and neither claims to be. They are placeholders in the same sense the
letters were, but they are _distinguishable_ placeholders, which the letters were not.

## Rendering

gpui rasterises these with resvg and uses only the **alpha channel**, filling the resulting
mask with `style.text.color`. Two consequences:

1. `fill="currentColor"` is required. A fill of `none` renders an empty mask and the icon
   disappears. `crates/app/src/icons.rs` has a test asserting this on every file.
2. The colour in the file is irrelevant — the theme supplies it. That is what lets one set of
   glyphs work across all five theme variants.

The SVGs are `include_str!`-ed into the binary rather than read from this directory at
runtime, so a missing or renamed file is a compile error rather than a silently blank square.
See the module comment in `crates/app/src/icons.rs`.
