# assets

| Path           | What it is                                                                                     |
| -------------- | ---------------------------------------------------------------------------------------------- |
| `app-icon.svg` | The application's own identity icon, square and transparent.                                   |
| `macos/`       | Everything needed to turn that into a macOS `.app` icon.                                       |
| `icons/`       | **Different thing.** In-app UI glyphs for the activity bar (issue #50); see `icons/README.md`. |
| `themes/`      | Themes loaded from disk (#58); see `themes/README.md`. Empty, on purpose — that file says why. |
| `grammars/`    | Empty placeholder.                                                                             |

`app-icon.svg` and `icons/` are deliberately kept apart. One is the product's identity and
is consumed by the operating system; the others are interface furniture consumed by the
renderer. They have different sizes, different colours, and no reason to share a decision.

## The artwork

A pixel-art elePHPant — the PHP mascot, which is the right symbol for a Laravel IDE.

Its real resolution is **18 x 10 pixels**. The source file was a 1342x894 SVG of bezier
blobs, but every blob sits on an 18x10 grid, so the matrices in `macos/` state that grid
directly:

- `macos/art-18x10.txt` — the full elephant, including the detached tail tip
- `macos/art-16x10.txt` — the same elephant with the tail dropped, for the smallest sizes

`K` is the near-black of the eyes, `D` the darker grey of ears and feet, `M` the body grey,
and `.` is transparent. Edit these to change the artwork; they are the source of truth.

## Why the small sizes are drawn, not scaled

The obvious pipeline — render `app-icon.svg` at each size — was tried first and measured.
It fails at 16pt, badly.

18 art pixels resampled onto ~13 device pixels means no art pixel lands on a device pixel.
The eyes are single dark pixels, so they blurred into two faint grey smudges. The legs are
single-pixel columns, so they disappeared completely. The result was a pale grey blob with
no elephant in it.

So `scripts/gen-iconset.py` draws every size at an **integer** pixel scale instead: one art
pixel becomes an NxN block of device pixels at a whole-pixel offset. Nothing is ever
resampled, and 16x16 comes out as crisp as 1024x1024.

That is also why the tail is dropped below 32pt. Those two floating pixels read as a speck
of dirt at that size, and losing them frees the two columns needed to draw the body at an
exact 1:1 scale rather than a blurred 0.89:1.

## Safe area

Sizes from 64 device pixels up put the art in 77-84% of the canvas, the Big Sur+ safe area,
so the icon does not look oversized next to others in the Dock.

The 16 and 32 device-pixel entries fill the frame instead. 80% of 16 pixels is 12, and an
elephant is not legible 12 pixels wide. Filling the frame at menu-bar and list-view sizes is
normal practice; the alternative is not a smaller elephant but an unreadable one.

## Contrast

The palette is three greys on transparent, and it is **low contrast on light backgrounds**.
It reads well on a dark Dock and acceptably on a light one, but it does not pop the way a
saturated icon does. Darkening the body grey, or giving the elephant the PHP-purple it
usually wears, would fix that — deliberately not done here, because recolouring someone
else's artwork is a design decision for the author, not a build step.

## Licensing — unresolved

This repository is Apache-2.0, but the elePHPant is **not** original to it. The mascot was
created by **Vincent Pontier** in 1998 and has its own licensing history, separate from the
PHP project's own marks.

This derivative reproduces the mascot's recognisable design. Before shipping ellefuanti as a
product, someone needs to confirm the terms actually permit that use. This note exists so
the question is not shipped unexamined; it is not a legal determination and nobody here made
one.

## Regenerating

```sh
python3 scripts/gen-iconset.py      # matrices -> assets/macos/ellefuanti.iconset
scripts/bundle-macos.sh             # binary + iconset -> target/ellefuanti.app
```

`gen-iconset.py` is standard library only, so it runs on a clean macOS box.

The `.icns` itself is **not** committed: it is an undiffable binary fully derived from the
`.iconset`, which is reviewable. `bundle-macos.sh` builds it with `iconutil` on demand.
