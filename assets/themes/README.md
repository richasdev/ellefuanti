# themes

Themes loaded from disk (#58). Two directories are read, in this order:

| Path                                               | What it is              |
| -------------------------------------------------- | ----------------------- |
| `assets/themes/` (this one)                        | Shipped with the binary |
| `~/Library/Application Support/ellefuanti/themes/` | The user's own          |

A user theme with the same name as a shipped one wins. A **built-in** name wins over both:
a file called `dark.json` cannot shadow `Theme::dark()`, which is what keeps "Dark is always
available" true even when this directory is missing or full of broken files.

## This directory is currently empty, on purpose

The obvious thing to ship here is the three themes the importer was built against — One Dark
Pro, GitHub Dark, GitHub Light. They are **already compiled in** (`crates/app/src/theme.rs`),
so a file of the same name would never load, and the built-in wins by design. Shipping three
files that can never be read, and taking on the redistribution obligation for them, buys
nothing.

The five built-ins cover dark, light, and the three classics. The next theme worth shipping
is one that is _not_ already compiled in, and at that point it goes here with its origin and
licence recorded in the file's own `origin` key.

**Adding the first one means one more line in `scripts/bundle-macos.sh`.** The loader looks
for `Contents/Resources/themes/` inside an `.app`, and the bundle script does not copy this
directory yet — there is nothing to copy. `cargo run` reads the repository path directly, so
disk themes work in a checkout today either way.

## Licensing

The repository is Apache-2.0. Anything in **this** directory is distribution, so it needs a
compatible licence and an `origin` string naming where it came from and under what terms:

```json
"origin": "github.github-vscode-theme v6.3.5 (MIT)"
```

MIT and BSD are compatible with Apache-2.0 and are what most published VS Code themes use.
Check before adding one — a theme with no stated licence is not a theme that can be shipped.

**A theme the user imports for themselves is their business.** The obligation is on this
directory, not on `~/Library/Application Support/ellefuanti/themes/`.

## The format

```json
{
  "version": 1,
  "name": "midnight",
  "appearance": "dark",
  "origin": "…, MIT",
  "colors": {
    "background": "#16171d",
    "text": "#d7dae0",
    "keyword": "#c77dff",
    "ansi.0": "#3b4048"
  }
}
```

`appearance` is `"dark"` or `"light"` and is not cosmetic — it decides the terminal's ANSI
defaults, and #48 established that those fixes are background-specific and actively wrong
when applied to the other kind.

Every key in `elle_theme::REQUIRED_COLORS` must be present, plus `ansi.0` through `ansi.15`.
`information` and `hint` are optional and fall back to `accent` and `comment`. A missing or
unparseable colour names the file and the key and leaves the current theme alone.

`version` is stamped from the first commit so a later format change does not have to guess
what an unlabelled file is. A version this build does not recognise is a warning and a
best-effort read, never a discard — a theme file is something a human wrote and there is no
source to rebuild it from.

## Importing a VS Code theme

```sh
cargo run -p elle-theme --example import -- \
    ~/.vscode/extensions/<publisher>.<theme>/themes/<file>.json \
    <name> '<origin>, <licence>' > assets/themes/<name>.json
```

The importer resolves TextMate scopes by **specificity, not file order** — see
`crates/theme/src/scope.rs` for why that distinction has its own module and its own test.
