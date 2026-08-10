# assets/icons

In-app UI glyphs for the activity bar, the file tree and the tab bar (issues #50, #115).
Different from `../app-icon.svg`, which is the product's own identity and is consumed by the
operating system — see `../README.md`.

**Two sets, working in opposite ways.** Know which you are editing:

- **this directory** — monochrome Codicons, coloured by the theme (#50, #67). Every glyph in
  the window except the one below.
- **`file-types/`** — Ayu file-type icons, each painted in its own identifying colour and
  deliberately ignoring the theme (#115). Tried first for a file; the Codicons here are the
  fallback for the types Ayu has no SVG for. See the section at the bottom.

## Activity bar

| File           | Source                    | Licence    |
| -------------- | ------------------------- | ---------- |
| `explorer.svg` | Codicons `files`          | CC BY 4.0  |
| `search.svg`   | Codicons `search`         | CC BY 4.0  |
| `git.svg`      | Codicons `source-control` | CC BY 4.0  |
| `database.svg` | Codicons `database`       | CC BY 4.0  |
| `tests.svg`    | Codicons `beaker`         | CC BY 4.0  |
| `laravel.svg`  | Drawn for this repository | Apache-2.0 |
| `docker.svg`   | Drawn for this repository | Apache-2.0 |

## File tree and tabs

All Codicons, all taken from tag **0.0.36** — the same tag `explorer.svg` and `git.svg` are
pinned to, so the whole in-app set is one drawing generation and no glyph reads lighter than
the one beside it. Renamed on the way in to say what they mean _here_ rather than what they
are called upstream, because the mapping in `crates/app/src/icons.rs` is by role: nothing in
this app is "the `code` icon", it is "the icon for markup".

| File                | Codicons source | Drawn for                               |
| ------------------- | --------------- | --------------------------------------- |
| `chevron-down.svg`  | `chevron-down`  | an expanded directory                   |
| `chevron-right.svg` | `chevron-right` | a collapsed directory                   |
| `folder.svg`        | `folder`        | a collapsed directory                   |
| `folder-opened.svg` | `folder-opened` | an expanded directory                   |
| `file.svg`          | `file`          | anything unrecognised — never _no_ icon |
| `file-code.svg`     | `file-code`     | `.php`, and js/ts/rs/py/rb/go/java/sql  |
| `file-markup.svg`   | `code`          | `.blade.php`, html, xml, vue, svg       |
| `file-json.svg`     | `json`          | `.json`                                 |
| `file-markdown.svg` | `markdown`      | `.md`                                   |
| `file-style.svg`    | `symbol-color`  | css, scss, sass, less                   |
| `file-media.svg`    | `file-media`    | png, jpg, gif, webp, ico, bmp, avif     |
| `file-lock.svg`     | `lock`          | `composer.lock`, `package-lock.json`    |
| `file-shell.svg`    | `terminal`      | sh, bash, zsh, fish                     |
| `file-config.svg`   | `gear`          | yml, toml, ini, conf, and `.env`        |

### PHP and Blade get Codicons, not language logos

This is a Laravel IDE and `.php` is the most common file in the tree, so it is worth being
explicit about why neither the PHP elephant nor the Laravel mark is here. Same three reasons
#67 rejected them for the activity bar:

1. **Licence.** Laravel's logo is not CC-licensed and the elephant carries its own terms.
   Neither is granted by this repository's Apache-2.0.
2. **Rendering.** gpui keeps only the alpha channel and fills it with one flat theme colour
   (see below). A brand mark is defined by its colour; flattened it is wrong, and against the
   lighter variants it is unreadable.
3. **Consistency.** Every other glyph in the window is a 16px monochrome Codicon. One logo
   among them reads as a mistake.

So PHP takes `file-code` and Blade takes `file-markup`. That does mean PHP shares a glyph
with JavaScript and TypeScript — which is a real loss and worth stating plainly. It is not
the `D`/`D` collision #67 fixed, though: there, two activity-bar panels had _nothing else_
to tell them apart. Here the extension is written in the row beside the icon, so the glyph
is reinforcement for scanning and never the only signal.

## Attribution

Nineteen of the twenty-one are **Codicons**, the icon set Visual Studio Code itself uses:

> Codicons — Copyright (c) Microsoft Corporation.
> <https://github.com/microsoft/vscode-codicons>
> Icons licensed under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/).

The full licence text is in `LICENSE-codicons.txt`. CC BY 4.0 requires attribution, which is
what this file and that copy are for. Codicons' _code_ is MIT; none of it is vendored here,
only the icon artwork.

The files are unmodified upstream SVGs — renamed, never redrawn. Every one of them is from
tag **0.0.36** rather than `main`, deliberately — see below.

## Why 0.0.36 and not `main`

Upstream is part-way through redrawing the set, and `files` and `source-control` on `main`
have been redrawn with noticeably lighter strokes than the 16x16 icons beside them. Rendered
together, the explorer and git glyphs read as faded next to `database` and `beaker`.

The 0.0.36 drawings are heavier and match. Pinning two files to an older tag was the sort of
thing that looks like an accident later, so it was documented here; the file-tree set then
took the same tag for the same reason, which turns the pin from an exception into the rule.
The mismatch is reproducible with `rsvg-convert` at 96px against `theme.panel`.

When upstream finishes the redraw, take the whole set from one newer tag at once — not a
file at a time, which is how a set ends up with two drawing weights in it.

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

Both sets go through this same path. The difference is only which colour fills the mask:
these seven take `theme.text`, `file-types/` takes a colour declared per icon — see below.

gpui rasterises these with resvg and uses only the **alpha channel**, filling the resulting
mask with `style.text.color`. Two consequences:

1. `fill="currentColor"` is required. A fill of `none` renders an empty mask and the icon
   disappears. `crates/app/src/icons.rs` has a test asserting this on every file.
2. The colour in the file is irrelevant — the theme supplies it. That is what lets one set of
   glyphs work across all five theme variants.

The SVGs are `include_str!`-ed into the binary rather than read from this directory at
runtime, so a missing or renamed file is a compile error rather than a silently blank square.
See the module comment in `crates/app/src/icons.rs`.

### Measured at 16px

Every file here was rasterised through gpui's exact path — usvg 0.45.1 parse, resvg render
scaled to 16px, `pixels().map(|p| p.alpha())` — and checked for the three ways an SVG fails
after it has already parsed: it inks nothing (a blank square), it inks nearly everything (a
solid box), or it has no fully-opaque pixel (a ghost). All twenty-one ink between 11% and
48% of their box with solid pixels in each, and no two rasterise to an identical mask.

The chevrons are the lightest at 10.9%, which is inherent to a 1px diagonal at this size and
matches upstream. They are only ever drawn on directory rows, which take `theme.text`
(7.2–17.4:1 against the panel across the five variants) rather than the `text_muted` a file
row takes (4.0–4.4:1) — so the thinnest glyph in the set sits on the strongest colour.

This is not a substitute for looking at the running app. It proves the bytes rasterise to a
legible distinct shape; it cannot prove the layout around them is right.

---

# assets/icons/file-types

File-type icons for the file tree and the tab bar (issue #115), from the **Ayu** icon theme.

## Attribution

> Ayu — Copyright (c) 2016 Ike Kurghinyan
> <https://github.com/dempfi/ayu>
> Licensed under the **MIT Licence**.

The full licence text is in `LICENSE-ayu.txt`, which is what MIT's "the above copyright
notice and this permission notice shall be included in all copies" requires. The files are
unmodified upstream SVGs from `teabyii.ayu` 1.1.12, renamed from `file_type_<name>.svg` to
`<name>.svg` and nothing else. MIT is compatible with this repository's Apache-2.0.

## Why 19 and not all 53

Ayu ships 53 SVGs. An icon nothing maps to is bytes in the binary that can never appear on
screen, so only the set a Laravel project can actually reach is vendored.
`crates/app/src/file_icons.rs` has a test in each direction — every mapping points at a
vendored icon, and every vendored icon is reachable from some mapping — so this set cannot
silently drift in either direction.

The rest of Ayu's 53 are for languages nobody opens here (Haxe, Haskell, Groovy, Dart,
Kotlin, Swift, Perl, C#, Jupyter, …). They are one `cp` away if that changes.

Ayu's own `default.svg` is **not** among them: the Codicon `file.svg` in the parent directory
is the fallback, so Ayu's generic sheet became unreachable the moment the two sets met, and
the reachability test above is what said so.

**`npm` and `svelte` were vendored and then removed**, and the reason is specific to how
these are painted: both draw their identity as a lighter shape _on top of_ a darker one
(svelte's white `S` over a red blob, npm's `n` inside a red square). Filled as a
single-colour mask, both come out as a solid coloured rectangle with no glyph in it. Every
test passed on them — this was caught by rendering the set and looking at it. So
`package.json` gets the JSON icon and `.svelte` falls through to the Codicons.

## What Ayu has no SVG for — which is why both sets exist

Ayu ships these as **`@2x.png` only**, and a PNG cannot go through gpui's SVG path:

`html` · `blade` · `composer` · `docker` · `sql` · `xml` · `text` · `log` · `settings`

Those are exactly the files a Laravel project is full of, which is the argument for keeping
the Codicons rather than replacing them: **Ayu first for identity, Codicons behind it for
coverage.** `.html` and `.blade.php` get `file-markup`, `.sql` gets `file-code`,
`phpunit.xml` gets `file-markup`, and an unknown file still gets `file.svg`.

Three cases worth calling out, all decided against an earlier draft of this PR:

- **`composer.json` shows the JSON icon**, not a Composer icon — correct, just less specific
  than VS Code.
- **`.blade.php` shows `file-markup` (`</>`)**, not the PHP elephant. Ayu has no Blade glyph,
  so the choice was PHP-or-nothing until the Codicons arrived; a Blade file is markup with
  PHP in it, which is what #119 argued when it picked that glyph. The check still runs first,
  because `.blade.php` also ends in `.php`.
- **`.env` and lock files stay on Codicons.** `language_for_path` reads `.env` with the bash
  grammar, so Ayu's shell icon would have been _consistent_ — but `.env` is configuration you
  edit, not a script you run, and `file-config` says that. `package-lock.json` declines Ayu
  before the `.json` rule can claim it, so a lock file still reads as a generated artefact.

## These ignore the theme, on purpose

Every icon in the parent directory is monochrome and coloured by the theme. **These are the
opposite: their colour is their identity.** Yellow JS and purple PHP are recognisable
_because_ of the colour, and flattening them to one theme foreground would leave 19
identical grey shapes — which is precisely the generic Codicon they sit in front of, bought
for 50 KB.

They are still drawn with `svg()`, which keeps only the alpha channel (see Rendering above) —
so each one is painted as a mask filled with **one** colour, declared per icon in
`crates/app/src/file_icons.rs` and taken from Ayu's own dominant fill for that glyph. The
internal detail is lost; the silhouette and the identifying hue are not, and at 16 px that
detail was never visible anyway.

gpui's `img()` _does_ keep the full multi-colour buffer, and this was built that way first.
It costs **+1.86 MB** — 15.73 MB → 17.55 MB as measured before #119 landed, past the 17 MB limit `scripts/perf-gate.sh`
blocks on. Not because of the icons (all 19 are 50 KB) but because the first `img()` call
makes gpui's JPEG/GIF/WebP/PNG decoders reachable, so LTO stops stripping them: **3 decoder
symbols in the binary before, 592 after**. That is the trade recorded in `file_icons.rs`; if
the full-colour version is ever wanted, it needs the perf gate raised or a pre-rasterisation
path that skips the decoders.

## Legibility on the light themes

Ayu's palette was drawn for dark backgrounds, so this was measured, against the panel colour
the tree actually draws on (`#f6f8fa` GitHub Light, `#f1f2f6` Light):

|                                    | contrast   |
| ---------------------------------- | ---------- |
| best (`toml`)                      | 5.28:1     |
| median                             | ~2.7:1     |
| **worst — `js`, `yaml`, `python`** | **1.85:1** |

The yellows are genuinely faint on a light panel. They are kept because **nothing here is
signalled by colour alone** — the file name sits beside the icon and is the real label, so a
washed-out glyph loses a redundant cue, not information. Using these icons anywhere the name
is absent would break that argument.

> > > > > > > 0688a89 (feat(ui): Ayu file-type icons in the tree and tabs, one colour each)
