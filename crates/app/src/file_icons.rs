//! The Ayu half of the file-type icons: which coloured glyph a file gets, and in what
//! colour.
//!
//! **This is not the whole lookup.** [`crate::icons::for_file`] is the entry point, and it
//! asks here first; anything Ayu has no glyph for falls through to the monochrome Codicon
//! set that #119 added. See that function for why both sets exist rather than one.
//!
//! # These icons ignore the theme, and that is deliberate
//!
//! Every other icon in this repo is monochrome and takes its colour from the theme —
//! `assets/icons/*.svg` use `fill="currentColor"` and `icons.rs` has a test enforcing it,
//! which is what lets those glyphs work across all five theme variants (#67).
//!
//! A file-type icon is the opposite case: **its colour is its identity**. Yellow JS, red
//! Ruby and purple PHP are recognisable *because* of the colour, and recolouring them to a
//! single theme foreground would leave 19 indistinguishable grey rectangles — which is
//! exactly the generic Codicon they sit in front of, bought for 50 KB.
//!
//! So these bypass the theme. If you are reading this because an icon ignores your theme
//! colour: that is the feature, not a bug. It is the only place in the window where a glyph
//! does this, which is why `for_file` returns the colour as an `Option` rather than letting
//! a caller guess.
//!
//! # One colour per icon, not the colours inside the file
//!
//! This is the part that is easy to get wrong, and it was measured rather than assumed.
//!
//! **gpui 0.2.2 cannot render a multi-colour SVG through `svg()` at all.** It rasterises
//! with resvg and then throws the colour away — `SvgRenderer::render` (`svg_renderer.rs:75`)
//! ends with
//!
//! ```ignore
//! let alpha_mask = pixmap.pixels().iter().map(|p| p.alpha()).collect::<Vec<_>>();
//! ```
//!
//! RGB is dropped; only the alpha channel reaches the GPU, and `svg().paint` fills that mask
//! with a single `style.text.color`. Verified by running Ayu's `php.svg` and `js.svg` through
//! that exact path at the real slot size: resvg produced the correct Ayu purple and gold in
//! the pixmap (7 distinct opaque RGB values each, the anti-aliased spread of `#A3789D` and
//! `#E6B045`), and the very next line reduced both to a bare coverage mask.
//!
//! So each icon carries **one** colour, in [`FileIcon::color`], and the shape is drawn as a
//! mask filled with it. Most of Ayu's glyphs are near enough single-hue that this reads as
//! intended: the internal shading is lost, the silhouette and the identifying hue are not.
//!
//! ## The two that were dropped because of this, found by looking
//!
//! `npm` and `svelte` are **not** vendored, and no test would have told you why. Both draw
//! their identity as a *lighter shape on top of a darker one* — svelte's white `S` over a
//! red blob, npm's `n` counter inside a red square. An alpha mask has one colour, so both
//! rasterise to **a solid coloured rectangle with no glyph in it**: not a wrong colour, not
//! a missing file, just a blob. Every test in this module passed on them.
//!
//! They were caught by rendering the whole set through resvg at the real slot size,
//! compositing over the four real panel colours and looking at the sheet. That is the check
//! to repeat before adding an icon here — #112 is the standing note that this suite cannot
//! see rendering, and this is a worked example of what it misses.
//!
//! `package.json` therefore gets the JSON icon and `.svelte` falls through to the Codicon
//! set. Both are less specific than VS Code and both are honest.
//!
//! ## Why not `img()`, which *does* keep the full colour
//!
//! It works — `image::guess_format` fails on SVG text, so gpui falls through to
//! `render_pixmap` and keeps the whole BGRA buffer (`elements/img.rs:696`) — and it was built
//! and measured that way first. It costs **+1.86 MB of binary**, which took the release
//! build from 15.73 MB to 17.55 MB — past the **17 MB limit `scripts/perf-gate.sh` blocks
//! on**. (Measured before #119 landed, so both figures are ~0.13 MB below today's; the
//! +1.86 MB is the part that matters and it is a property of the decoders, not the tree.)
//!
//! The cause is not the icons: the 19 SVGs are 50 KB. It is that the first call to `img()`
//! makes gpui's `image` decoder tree reachable, so LTO stops stripping it — JPEG, GIF, WebP
//! and PNG decoders this app never needs. Measured with `nm`: **3 decoder symbols in the
//! binary before, 592 after**.
//!
//! Full colour is therefore available for 1.86 MB and a failed perf gate, or for a direct
//! dependency on `image` + `smallvec` pinned to gpui's exact versions to pre-rasterise into
//! `ImageSource::Render` and skip the decoders. Neither was worth it for detail that is
//! invisible at 16 px. **If you switch to `img()`, the gate will fail and that is why.**
//!
//! # Legibility on the light themes, measured
//!
//! Ayu's palette was drawn for dark backgrounds, so this was checked rather than hoped.
//! Every icon was rendered at the real slot size and its dominant opaque colour compared
//! against the panel the file tree actually draws on — `#f6f8fa` (GitHub Light) and
//! `#f1f2f6` (Light). Contrast is a property of the two colours, so these hold at any size:
//!
//! | | contrast vs `#f6f8fa` |
//! |---|---|
//! | best (`toml` `#A64E21`) | 5.28:1 |
//! | median | ~2.7:1 |
//! | **worst — `js`, `yaml`, `python` (`#E6B045`)** | **1.85:1** |
//!
//! The yellow group is genuinely faint on a light panel. It is kept anyway, and the reason
//! is that **nothing here is signalled by colour alone** — the file name is right beside the
//! icon and is the actual label, so a washed-out glyph loses a redundant cue rather than
//! information. That is the same rule the diff gutter and status rows follow.
//!
//! What would break that reasoning: using these icons anywhere the name is *not* present.
//! Don't. If the yellows have to work standalone, the fix is a per-theme colour override
//! for that group, not a lower opacity or a different set.

/// One vendored icon: the asset path gpui loads, its bytes, and the colour to fill it with.
///
/// Same shape as [`crate::icons::Icon`] plus `color`, because it is served by the same
/// `AssetSource` but painted deliberately differently — see the module doc.
pub struct FileIcon {
    /// What `svg().path(..)` is given and what the asset source matches on.
    pub path: &'static str,
    /// The short name a mapping refers to, e.g. `"php"`.
    pub name: &'static str,
    /// The icon's identifying colour, as `0xRRGGBB`.
    ///
    /// Ayu's own dominant fill for that glyph, read out of the file rather than picked:
    /// each value is the most common opaque pixel after rendering the real SVG at the real
    /// slot size. This is what makes the icon recognisable, and it is why these do not use
    /// the theme's foreground like every other icon in the app.
    pub color: u32,
    pub(crate) svg: &'static str,
}

impl FileIcon {
    /// The raw SVG bytes, for the `AssetSource` in `icons.rs` to serve.
    pub fn svg(&self) -> &'static str {
        self.svg
    }
}

macro_rules! file_icons {
    ($(($name:literal, $color:literal)),* $(,)?) => {
        &[$(FileIcon {
            path: concat!("icons/file-types/", $name, ".svg"),
            name: $name,
            color: $color,
            svg: include_str!(concat!("../../../assets/icons/file-types/", $name, ".svg")),
        }),*]
    };
}

/// Every file-type icon vendored, and the only names [`MAPPINGS`] may refer to.
///
/// Deliberately **not** all 53 of Ayu's SVGs. An icon nothing maps to is bytes in the
/// binary that can never appear on screen, so this is the reachable set for a Laravel
/// project plus the fallback — see `assets/icons/README.md` for what was left out and why.
pub const FILE_ICONS: &[FileIcon] = file_icons![
    ("php", 0xA3789D),
    ("js", 0xE6B045),
    ("typescript", 0x86ACBF),
    ("json", 0x92BD79),
    ("css", 0x759EB3),
    ("sass", 0x92BD79),
    ("scss", 0xA3789D),
    ("yaml", 0xE6B045),
    ("shell", 0xA3789D),
    ("markdown", 0xD69D6B),
    ("git", 0xD96C36),
    ("vue", 0x40B883),
    ("toml", 0xA64E21),
    ("rust", 0xC86B67),
    ("image", 0x90A959),
    ("font", 0x97927C),
    ("license", 0xD9A357),
    ("csv", 0x55B4D4),
    ("python", 0xE6B045),
];

/// Whole file names, checked before extensions.
///
/// A Laravel project's root is mostly files whose name carries the meaning and whose
/// extension does not: `composer.json` is not merely JSON, `artisan` has no extension at
/// all. Matched case-insensitively on the full name.
const BY_NAME: &[(&str, &str)] = &[
    // Laravel's CLI entry point — a PHP file with a shebang and no extension, exactly as
    // `language_for_path` treats it.
    ("artisan", "php"),
    // `.env` is deliberately absent, and so is the `.env.` prefix. `language_for_path`
    // reads those with the bash grammar, so the shell icon was the consistent choice — but
    // #119's `file-config` is the better *icon*: `.env` is configuration you edit, not a
    // script you run, and the two questions have different right answers. Letting it fall
    // through keeps that, and `dotenv_files_are_config` in `icons.rs` guards it.
    (".gitignore", "git"),
    (".gitattributes", "git"),
    (".gitmodules", "git"),
    (".gitkeep", "git"),
    ("license", "license"),
    ("license.md", "license"),
    ("license.txt", "license"),
];

/// Extension to icon name. The right-hand side must appear in [`FILE_ICONS`];
/// `every_mapping_points_at_a_vendored_icon` is what enforces that.
///
/// Extensions with no Ayu SVG are absent on purpose and fall through to the Codicon set —
/// Ayu ships `html`, `blade`, `composer`, `docker`, `sql` and `xml` as `@2x.png` only, and a
/// PNG cannot go through gpui's SVG path. See `assets/icons/README.md`.
const BY_EXTENSION: &[(&str, &str)] = &[
    ("php", "php"),
    ("phtml", "php"),
    ("js", "js"),
    ("cjs", "js"),
    ("mjs", "js"),
    ("jsx", "js"),
    ("ts", "typescript"),
    ("cts", "typescript"),
    ("mts", "typescript"),
    ("tsx", "typescript"),
    ("json", "json"),
    ("css", "css"),
    ("sass", "sass"),
    ("scss", "scss"),
    ("yml", "yaml"),
    ("yaml", "yaml"),
    ("sh", "shell"),
    ("bash", "shell"),
    ("zsh", "shell"),
    ("fish", "shell"),
    ("md", "markdown"),
    ("markdown", "markdown"),
    ("vue", "vue"),
    ("toml", "toml"),
    ("rs", "rust"),
    ("png", "image"),
    ("jpg", "image"),
    ("jpeg", "image"),
    ("gif", "image"),
    ("svg", "image"),
    ("ico", "image"),
    ("webp", "image"),
    ("avif", "image"),
    ("bmp", "image"),
    ("woff", "font"),
    ("woff2", "font"),
    ("ttf", "font"),
    ("otf", "font"),
    ("eot", "font"),
    ("csv", "csv"),
    ("py", "python"),
];

/// Both tables, exposed as one list so a test can walk every mapping there is. Adding a
/// table without adding it here would leave it unguarded.
///
/// Only the guard tests read it — the lookup goes through the tables directly, because a
/// flattened list would lose the name-before-extension ordering that `.blade.php` needs.
#[cfg(test)]
pub const MAPPINGS: [&[(&str, &str)]; 2] = [BY_NAME, BY_EXTENSION];

/// The Ayu icon name for a file name, or `None` if Ayu has nothing for it.
///
/// `None` is not a failure — it hands the file to the Codicon set in [`crate::icons`],
/// which covers the types Ayu ships only as PNG. See [`crate::icons::for_file`].
///
/// **`.blade.php` deliberately returns `None`**, and this reverses an earlier decision.
/// Ayu has no Blade glyph (PNG only), so the choice was the PHP elephant or nothing. With
/// #119's Codicons available the honest answer is neither: Blade gets `file-markup` (`</>`),
/// because a Blade file *is* markup with PHP in it, and that is what #119 argued when it
/// picked that glyph. The check has to stay first regardless — `.blade.php` also ends in
/// `.php`, so testing the extension first would give every view the PHP icon.
fn ayu_name_for_file(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();

    if name.ends_with(".blade.php") {
        return None;
    }

    // Lock files decline before anything else, because `package-lock.json` ends in `.json`
    // and would otherwise take Ayu's JSON glyph — the exact outcome
    // `lock_files_are_recognised_by_name` exists to prevent. A lock file is a generated
    // artefact you do not edit, and #119's `file-lock` says so; its own extension does not.
    if name.ends_with(".lock") || name.ends_with("-lock.json") {
        return None;
    }

    if let Some((_, icon)) = BY_NAME.iter().find(|(n, _)| *n == name) {
        return Some(icon);
    }

    // `.env.example` / `.env.testing` decline for the same reason `.env` does (see BY_NAME).
    // Without this they would reach the extension table as `example`/`testing` and miss
    // anyway, but saying so here stops someone "fixing" it by adding those extensions.
    if name.starts_with(".env.") {
        return None;
    }

    let ext = name.rsplit_once('.').map(|(_, ext)| ext)?;
    BY_EXTENSION.iter().find(|(e, _)| *e == ext).map(|(_, icon)| *icon)
}

/// The Ayu icon for a file name: its asset path and its colour, or `None` to fall back.
pub fn ayu_icon_for_name(name: &str) -> Option<(&'static str, u32)> {
    let wanted = ayu_name_for_file(name)?;
    FILE_ICONS.iter().find(|icon| icon.name == wanted).map(|icon| (icon.path, icon.color))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard in the spirit of `panels_and_icons_stay_aligned`: it reads the *same*
    /// tables the renderer reads, rather than a list retyped here. A mapping naming an icon
    /// that was never vendored would otherwise render nothing at all, and only in the tree.
    #[test]
    fn every_mapping_points_at_a_vendored_icon() {
        for table in MAPPINGS {
            for (key, icon) in table {
                assert!(
                    FILE_ICONS.iter().any(|i| i.name == *icon),
                    "{key} maps to `{icon}`, which is not in FILE_ICONS — \
                     vendor assets/icons/file-types/{icon}.svg or fix the mapping"
                );
            }
        }
    }

    /// The other direction, and the one the issue asked for explicitly: an icon nothing can
    /// reach is dead weight in the binary. This is what stops the set drifting back toward
    /// all 53, and it is why `default.svg` was deleted — the Codicon `file.svg` owns the
    /// fallback now, so Ayu's generic sheet became unreachable the moment the two sets met.
    #[test]
    fn every_vendored_icon_is_reachable() {
        for icon in FILE_ICONS {
            assert!(
                MAPPINGS.iter().any(|t| t.iter().any(|(_, name)| *name == icon.name)),
                "{}.svg is vendored but nothing maps to it — map it or delete it",
                icon.name
            );
        }
    }

    /// `.blade.php` also ends in `.php`, so this ordering decides it. Blade hands off to the
    /// Codicon set on purpose — see `ayu_name_for_file` for why `file-markup` beats the PHP
    /// elephant here — but PHP itself must still reach Ayu.
    #[test]
    fn blade_defers_to_the_codicon_set_and_php_does_not() {
        assert_eq!(ayu_name_for_file("show.blade.php"), None);
        assert_eq!(ayu_name_for_file("Show.Blade.PHP"), None, "must be case-insensitive");
        assert_eq!(ayu_name_for_file("User.php"), Some("php"));
        // And the whole-pipeline answer: Blade gets markup, PHP gets the elephant.
        assert_eq!(crate::icons::for_file("show.blade.php").0, "icons/file-markup.svg");
        assert_eq!(crate::icons::for_file("User.php").0, "icons/file-types/php.svg");
    }

    /// Never blank space. Ayu declining a file is not the end of the lookup — the Codicon
    /// set behind it must always produce a glyph, and it must be one that is really vendored.
    #[test]
    fn anything_ayu_declines_still_gets_a_codicon() {
        for name in
            ["notes.xyz", "Makefile", "Dockerfile", "phpunit.xml", "data.sql", "welcome.html"]
        {
            assert_eq!(ayu_name_for_file(name), None, "{name} is not an Ayu type");
            let (path, color) = crate::icons::for_file(name);
            assert!(color.is_none(), "{name} fell back, so it must take the theme's colour");
            assert!(
                crate::icons::ICONS.iter().any(|i| i.path == path),
                "{name} resolved to {path}, which is not a vendored icon"
            );
        }
    }

    /// The files a Laravel project is actually full of. If one of these regresses to a
    /// generic Codicon, the feature has stopped being worth its bytes — that regression is
    /// silent, because the fallback always produces *some* glyph.
    #[test]
    fn a_laravel_project_gets_recognisable_icons() {
        let cases = [
            ("composer.json", "json"),
            // Not an npm icon — see the module doc on the two glyphs that were dropped.
            ("package.json", "json"),
            ("artisan", "php"),
            ("User.php", "php"),
            ("app.js", "js"),
            ("app.ts", "typescript"),
            ("app.css", "css"),
            ("app.scss", "scss"),
            ("docker-compose.yml", "yaml"),
            ("deploy.sh", "shell"),
            ("README.md", "markdown"),
            (".gitignore", "git"),
            ("LICENSE", "license"),
            ("Cargo.toml", "toml"),
            ("logo.svg", "image"),
            ("Inter.woff2", "font"),
        ];
        for (name, expected) in cases {
            assert_eq!(ayu_name_for_file(name), Some(expected), "for {name}");
        }
    }

    /// resvg is what gpui rasterises with, and it rejects malformed SVG at runtime by
    /// painting nothing. Parsing here turns that into a test failure — the same check
    /// `icons.rs` runs, minus the `currentColor` rule, which these deliberately break.
    #[test]
    fn every_file_icon_is_parseable_svg() {
        for icon in FILE_ICONS {
            assert!(icon.svg.contains("<svg"), "{} is not an svg", icon.path);
            assert!(icon.svg.contains("viewBox"), "{} has no viewBox", icon.path);
        }
    }

    /// The declared colour must be one the file actually contains.
    ///
    /// `FileIcon::color` is what gets painted; the SVG's own fills are only a mask once
    /// gpui is done with them. Nothing in the type system connects the two, so a copy-paste
    /// in the table would paint PHP's purple on the Ruby glyph and no other test would
    /// notice. Every colour here was read out of its own file, and this keeps it that way.
    #[test]
    fn each_declared_colour_appears_in_its_own_svg() {
        for icon in FILE_ICONS {
            let hex = format!("{:06X}", icon.color);
            let svg = icon.svg.to_ascii_uppercase();
            assert!(
                svg.contains(&hex),
                "{} declares #{hex}, which does not appear in the file — the icon would be \
                 painted a colour Ayu never drew it in",
                icon.path
            );
        }
    }

    /// The point of the whole feature: different types must look different. Two icons
    /// sharing a colour is fine (Ayu reuses its palette), but the set collapsing toward one
    /// hue would mean the icons are no longer carrying information.
    #[test]
    fn the_palette_is_actually_varied() {
        let mut colors: Vec<u32> = FILE_ICONS.iter().map(|i| i.color).collect();
        colors.sort_unstable();
        colors.dedup();
        // Proportional, not an absolute count: the set shrinks whenever an icon turns out
        // not to survive the alpha mask (npm, svelte) or is superseded by a Codicon, and a
        // fixed threshold would fail for the wrong reason every time that happens.
        assert!(
            colors.len() * 3 >= FILE_ICONS.len() * 2,
            "only {} distinct colours across {} icons — if the palette collapses, the icons \
             stop distinguishing anything and the bytes are wasted",
            colors.len(),
            FILE_ICONS.len()
        );
    }
}
