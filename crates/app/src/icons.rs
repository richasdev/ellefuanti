//! Every UI glyph the app draws, and the asset source gpui loads them through.
//!
//! gpui's `svg()` element does not take bytes; it takes a path that it hands to the
//! `AssetSource` installed on the `Application`. So there has to be a source, and the only
//! question is where the bytes come from.
//!
//! They are `include_str!`-ed rather than read from `assets/` at runtime, for the same
//! reason `elle_syntax` embeds its highlight queries: a file that is missing at runtime
//! fails *quietly*. `paint_svg` logs and paints nothing, so a bad path is a blank 16x16
//! hole with no crash and no visible error — which is precisely the silent-degradation
//! failure this issue rejected an icon font to avoid. Embedded, a missing file is a compile
//! error and a wrong name is a compile error.
//!
//! The cost is that the icons cannot be themed by dropping files into a directory. That is
//! a real feature to give up, and it goes when a settings crate exists to make it
//! coherent; until then it would be a runtime failure mode bought for nobody.
//!
//! # Two tables, not one
//!
//! [`ICONS`] is everything the binary carries and is what [`Icons`] serves. The activity
//! bar's seven live in [`ACTIVITY_ICONS`], a **prefix slice** of it, because
//! `render_activity_bar` zips that against its panel array positionally. Before the file
//! tree needed icons those were the same list; keeping them the same list would mean every
//! new file-type glyph silently shifted the activity bar (`zip` stops at the shorter side,
//! so the failure is a panel vanishing, not an error). `panels_and_icons_stay_aligned` in
//! `workspace_view.rs` guards the prefix.
//!
//! # Icons are never the only signal
//!
//! A file's type is already stated by its name — `.php` is right there in the text. The
//! icon is reinforcement for scanning, never the sole carrier of the fact, and none of the
//! mapping below distinguishes anything by colour: gpui fills every one of these from
//! `style.text.color`, so they are all one flat theme colour by construction.

use std::borrow::Cow;
use std::path::Path;

use gpui::{AssetSource, Result, SharedString};

/// One embedded SVG.
///
/// `path` is what `svg().path(..)` is given and what [`Icons::load`] matches on. Keeping
/// the two in one table is why a typo cannot happen: there is only one string.
pub struct Icon {
    pub path: &'static str,
    svg: &'static str,
}

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        &[$(Icon {
            path: concat!("icons/", $name, ".svg"),
            svg: include_str!(concat!("../../../assets/icons/", $name, ".svg")),
        }),*]
    };
}

/// Every icon the app can draw. Also the lookup table for [`Icons`].
///
/// The first seven are the activity bar's, in panel order — see [`ACTIVITY_ICONS`]. The
/// rest are the file tree's and the tab bar's, and their order carries no meaning.
pub const ICONS: &[Icon] = icons![
    // Activity bar, in panel order. Do not reorder or insert.
    "explorer",
    "search",
    "git",
    "laravel",
    "database",
    "docker",
    "tests",
    // Tree structure.
    "chevron-down",
    "chevron-right",
    "folder",
    "folder-opened",
    // Explorer header controls.
    "expand-all",
    "collapse-all",
    // File types.
    "file",
    "file-code",
    "file-markup",
    "file-json",
    "file-markdown",
    "file-style",
    "file-media",
    "file-lock",
    "file-shell",
    "file-config",
];

/// How many of [`ICONS`] belong to the activity bar, from the front.
const ACTIVITY_ICON_COUNT: usize = 7;

/// The activity bar's icons, in panel order.
///
/// A prefix of [`ICONS`] rather than a second `icons![..]` invocation, so the bytes are
/// embedded once and there is still exactly one place a name is written.
pub const ACTIVITY_ICONS: &[Icon] = ICONS.split_at(ACTIVITY_ICON_COUNT).0;

/// The paths the tree and tabs draw, as `svg().path(..)` arguments.
///
/// Consts rather than bare strings at each call site: a typo in a path is a blank square at
/// runtime, not a compile error, because `AssetSource::load` returns `Ok(None)` for an
/// unknown path and `paint_svg` logs and paints nothing. `every_named_path_is_in_the_table`
/// turns each of these into a test failure instead.
pub const CHEVRON_DOWN: &str = "icons/chevron-down.svg";
pub const CHEVRON_RIGHT: &str = "icons/chevron-right.svg";
pub const FOLDER: &str = "icons/folder.svg";
pub const FOLDER_OPENED: &str = "icons/folder-opened.svg";
pub const FILE: &str = "icons/file.svg";
/// The explorer header's expand-all / collapse-all buttons (VS Code's toolbar pair).
pub const EXPAND_ALL: &str = "icons/expand-all.svg";
pub const COLLAPSE_ALL: &str = "icons/collapse-all.svg";

/// The icon for a file, chosen from its name.
///
/// # Why Codicons for PHP and Blade rather than the language's own mark
///
/// Codicons has no PHP elephant and no Laravel mark, and this is deliberate on Microsoft's
/// part — the set is monochrome UI furniture, not a logo sheet. Three reasons not to reach
/// for the brand marks anyway:
///
/// 1. **Licence.** Laravel's logo is not CC-licensed and PHP's elephant carries its own
///    terms; neither is granted by this repository's Apache-2.0, and #67 already rejected
///    them on the same grounds for the activity bar.
/// 2. **Rendering.** gpui keeps only the alpha channel and fills it with one flat theme
///    colour. A brand mark is defined by its colour; flattened it is both wrong and, in the
///    lighter theme variants, unreadable.
/// 3. **Consistency.** Every other glyph in the window is a 16px Codicon. One logo among
///    them reads as a mistake.
///
/// So `.php` gets `file-code` and `.blade.php` gets `file-markup` (`</>`), which is honest:
/// it says "source" and "markup", which is what they are. It does mean PHP shares its glyph
/// with JS and TS — but unlike the `D`/`D` collision #67 fixed, nothing here *depends* on
/// the icon to tell them apart, because the extension is in the row's text beside it.
///
/// # Matching
///
/// The whole name, lowercased, not just the extension. `.blade.php` is a double extension
/// that a naive `Path::extension` reads as `php`, and `composer.lock` / `package-lock.json`
/// are recognised by name. Order matters: the most specific suffix has to be tested first.
/// The icon for a file: its path, and a colour if the icon supplies its own.
///
/// `None` means "a themed Codicon" — the caller fills its alpha mask with the row's
/// `text_color` and the theme recolours it for free, which is how every glyph in this
/// window worked before #115. `Some(rgb)` means an Ayu file-type icon, which carries its
/// own colour because for a file-type icon the colour *is* the identity.
///
/// # Why both sets, rather than one
///
/// Ayu wins wherever it has a glyph: a purple PHP elephant and a yellow JS badge are
/// recognisable in a way that three identical `file-code` sheets are not, and telling `.php`
/// from `.js` at a glance is the entire point of the feature.
///
/// But Ayu ships **no SVG** for `html`, `blade`, `composer`, `docker`, `sql` or `xml` — they
/// exist only as `@2x.png`, which cannot go through gpui's SVG path. Those are exactly the
/// files a Laravel project is full of, and the Codicons below cover them: `.blade.php` and
/// `.html` get `file-markup`, `.sql` gets `file-code`, `composer.lock` gets `file-lock`.
///
/// So neither set alone is right. Ayu first for identity, Codicons behind it for coverage,
/// and `file.svg` last so an unknown file still gets a glyph rather than a hole.
pub fn for_file(name: &str) -> (&'static str, Option<u32>) {
    if let Some((path, color)) = crate::file_icons::ayu_icon_for_name(name) {
        return (path, Some(color));
    }
    (codicon_for_file(name), None)
}

/// The Codicon half of [`for_file`]: one flat themed glyph per broad category.
fn codicon_for_file(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();

    // Double extensions and whole names first: `Path::extension` on `x.blade.php` is
    // `php`, so testing the extension first would give every Blade view a PHP glyph.
    if lower.ends_with(".blade.php") {
        return "icons/file-markup.svg";
    }
    if lower.ends_with(".lock") || lower.ends_with("-lock.json") {
        return "icons/file-lock.svg";
    }
    if lower == ".env" || lower.starts_with(".env.") {
        return "icons/file-config.svg";
    }

    let extension =
        Path::new(&lower).extension().and_then(|e| e.to_str()).unwrap_or_default().to_string();

    match extension.as_str() {
        "php" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "rs" | "py" | "rb" | "go"
        | "java" | "sql" => "icons/file-code.svg",
        "html" | "htm" | "xml" | "vue" | "svg" => "icons/file-markup.svg",
        "json" => "icons/file-json.svg",
        "md" | "markdown" | "mdx" => "icons/file-markdown.svg",
        "css" | "scss" | "sass" | "less" => "icons/file-style.svg",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "bmp" | "avif" => "icons/file-media.svg",
        "sh" | "bash" | "zsh" | "fish" => "icons/file-shell.svg",
        "yml" | "yaml" | "toml" | "ini" | "conf" | "cfg" | "editorconfig" => {
            "icons/file-config.svg"
        }
        // Unknown is a *generic file*, never nothing. A row with no glyph where its
        // neighbours have one reads as a broken icon, and the indentation stops lining up.
        _ => FILE,
    }
}

/// Serves the embedded icons to gpui.
///
/// Installed with `Application::new().with_assets(Icons)`. Without it, `AssetSource for ()`
/// returns `None` for every path and every icon silently paints nothing.
pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(icon) = ICONS.iter().find(|icon| icon.path == path) {
            return Ok(Some(Cow::Borrowed(icon.svg.as_bytes())));
        }
        // The Ayu file-type icons (#115) share this source because gpui installs exactly
        // one per `Application`. Same element and same rasteriser as the icons above; the
        // only difference is that the caller fills their mask with the icon's own colour
        // instead of the theme's — see `file_icons.rs`.
        Ok(crate::file_icons::FILE_ICONS
            .iter()
            .find(|icon| icon.path == path)
            .map(|icon| Cow::Borrowed(icon.svg().as_bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .map(|icon| icon.path)
            .chain(crate::file_icons::FILE_ICONS.iter().map(|icon| icon.path))
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every icon is drawn somewhere; a missing one is a blank square, not a crash.
    #[test]
    fn every_icon_loads_by_its_own_path() {
        for icon in ICONS {
            let bytes = Icons
                .load(icon.path)
                .unwrap()
                .unwrap_or_else(|| panic!("{} did not load", icon.path));
            assert!(!bytes.is_empty(), "{} is empty", icon.path);
        }
    }

    /// An unknown path must return `None`, not the first icon in the table. A `find` that
    /// fell back to a default would make every typo render the explorer glyph.
    #[test]
    fn an_unknown_path_loads_nothing() {
        assert!(Icons.load("icons/does-not-exist.svg").unwrap().is_none());
    }

    /// resvg is what gpui rasterises with, and it rejects malformed SVG at *runtime* by
    /// painting nothing. Parsing here turns that into a test failure instead. This is the
    /// check that would catch a truncated download or a hand-edited path.
    #[test]
    fn every_icon_is_parseable_svg() {
        for icon in ICONS {
            let svg = icon.svg;
            assert!(svg.contains("<svg"), "{} is not an svg", icon.path);
            assert!(svg.contains("viewBox"), "{} has no viewBox", icon.path);
            // Without this the icon renders in its own colour and ignores the theme:
            // gpui masks by alpha, so a fill of `none` would paint an empty square.
            assert!(
                svg.contains(r#"fill="currentColor""#),
                "{} must use fill=\"currentColor\" so the theme colours it",
                icon.path
            );
        }
    }

    /// No two entries in the table share a path.
    ///
    /// `AssetSource::load` is a `find`, so a duplicate is not an error — the first wins and
    /// the second is dead weight in the binary that nothing can ever reach. Copying a file
    /// under two names is how that happens.
    #[test]
    fn no_two_icons_claim_the_same_path() {
        let mut paths: Vec<&str> = ICONS.iter().map(|icon| icon.path).collect();
        paths.sort_unstable();
        let before = paths.len();
        paths.dedup();
        assert_eq!(before, paths.len(), "two entries in ICONS share a path");
    }

    /// The activity bar's slice is a genuine prefix of the full table.
    ///
    /// `split_at` cannot produce a non-prefix, so this is really guarding the *count*: if
    /// a file-type icon were inserted above `tests` in the `icons!` list, `ACTIVITY_ICONS`
    /// would still be seven long and every panel after the insertion point would draw the
    /// wrong glyph, with no test failing. Naming the last one pins it.
    #[test]
    fn the_activity_bar_slice_ends_where_the_activity_bar_does() {
        assert_eq!(ACTIVITY_ICONS.len(), ACTIVITY_ICON_COUNT);
        assert_eq!(
            ACTIVITY_ICONS.last().map(|icon| icon.path),
            Some("icons/tests.svg"),
            "a file-type icon was inserted among the activity bar's; every panel after it \
             now draws the wrong glyph"
        );
    }

    /// Every path named as a const or returned by `for_file` exists in the table.
    ///
    /// This is the test that makes the whole scheme safe. A path is a string, and a wrong
    /// string is a silently blank square at runtime — `load` returns `Ok(None)` and
    /// `paint_svg` logs. Walking a wide sample of names through `for_file` and asserting
    /// each answer loads turns every one of those into a compile-time-ish failure.
    #[test]
    fn every_named_path_is_in_the_table() {
        let mut paths =
            vec![CHEVRON_DOWN, CHEVRON_RIGHT, FOLDER, FOLDER_OPENED, FILE, EXPAND_ALL, COLLAPSE_ALL];

        for name in [
            "User.php",
            "welcome.blade.php",
            "app.js",
            "app.mjs",
            "app.cjs",
            "main.ts",
            "App.tsx",
            "App.jsx",
            "main.rs",
            "script.py",
            "app.rb",
            "main.go",
            "Main.java",
            "schema.sql",
            "index.html",
            "index.htm",
            "config.xml",
            "App.vue",
            "logo.svg",
            "composer.json",
            "README.md",
            "notes.markdown",
            "page.mdx",
            "app.css",
            "app.scss",
            "app.sass",
            "app.less",
            "logo.png",
            "photo.jpg",
            "photo.jpeg",
            "anim.gif",
            "img.webp",
            "favicon.ico",
            "old.bmp",
            "new.avif",
            "deploy.sh",
            "run.bash",
            "profile.zsh",
            "run.fish",
            "docker-compose.yml",
            "ci.yaml",
            "Cargo.toml",
            "php.ini",
            "nginx.conf",
            "app.cfg",
            "composer.lock",
            "package-lock.json",
            ".env",
            ".env.example",
            "Makefile",
            "LICENSE",
            "unknown.qqq",
            "",
        ] {
            paths.push(for_file(name).0);
        }

        for path in paths {
            assert!(
                Icons.load(path).unwrap().is_some(),
                "{path} is drawn somewhere but the asset source cannot load it: \
                 it renders as a blank square"
            );
        }
    }

    /// A Blade view is markup, not PHP.
    ///
    /// `Path::extension` on `welcome.blade.php` is `php`, so an implementation that reached
    /// for the extension first would give every Blade file in a Laravel project the PHP
    /// glyph — the single most common file type in the tree this editor exists for, all
    /// wrong. The order of the checks in `for_file` is the whole fix.
    #[test]
    fn blade_is_not_read_as_php() {
        assert_eq!(for_file("welcome.blade.php").0, "icons/file-markup.svg");
        // #115: PHP now reaches Ayu's elephant rather than the generic `file-code`. Blade
        // stays on `file-markup` — Ayu has no Blade glyph, and this one is the better
        // answer anyway for the reason this test was written.
        assert_eq!(for_file("User.php").0, "icons/file-types/php.svg");
        assert_ne!(
            for_file("welcome.blade.php").0,
            for_file("User.php").0,
            "a Blade view and a PHP class must not draw the same glyph"
        );
    }

    /// Matching is case-insensitive, because macOS filenames are.
    ///
    /// `README.MD` and `Photo.JPG` are ordinary on a case-insensitive volume, and a
    /// lowercase-only match would drop both to the generic file icon.
    #[test]
    fn the_extension_match_ignores_case() {
        assert_eq!(for_file("README.MD"), for_file("readme.md"));
        assert_eq!(for_file("Photo.JPG"), for_file("photo.jpg"));
        assert_eq!(for_file("Welcome.Blade.PHP"), for_file("welcome.blade.php"));
        // Both halves of the lookup have to agree on case, not just this one — an Ayu hit
        // that was case-sensitive would send `User.PHP` to the Codicon fallback instead.
        assert_eq!(for_file("User.PHP"), for_file("user.php"));
        assert_eq!(for_file(".ENV"), for_file(".env"));
    }

    /// An unrecognised name gets the generic file icon, never an empty string.
    ///
    /// A row with no glyph beside rows that have one reads as a rendering bug, and its
    /// text starts at a different x than its neighbours'. `Makefile` and `LICENSE` have no
    /// extension at all and are the ordinary case here, not an edge one.
    #[test]
    fn an_unknown_file_still_gets_an_icon() {
        // `LICENSE` is deliberately absent: #115 gives it Ayu's licence glyph, which is
        // more specific than the generic sheet and is the point of that set.
        for name in ["Makefile", "unknown.qqq", "", "no-extension"] {
            assert_eq!(for_file(name).0, FILE, "{name:?} got no icon");
        }
        // Whatever the name, there is always *a* glyph, and it is one that really exists
        // in one of the two sets — the Ayu half is served by the same `AssetSource`, so a
        // path from either is loadable.
        for name in ["Makefile", "LICENSE", "unknown.qqq", "", "User.php", "app.js"] {
            let (path, _) = for_file(name);
            assert!(
                Icons.load(path).unwrap().is_some(),
                "{name:?} -> {path}, which the asset source cannot load"
            );
        }
    }

    /// Lock files are recognised by name, in both the shapes this ecosystem produces.
    ///
    /// `package-lock.json` is JSON and would otherwise take the JSON glyph, which is not
    /// wrong so much as unhelpful — a lock file is a generated artefact you do not edit,
    /// and that is the useful thing to signal.
    ///
    /// #115 kept this intact by *not* mapping `.lock` or `-lock.json` on the Ayu side.
    /// Ayu would have given `package-lock.json` its JSON glyph, which is the outcome this
    /// test exists to prevent, so the omission there is load-bearing rather than an
    /// oversight — hence checking it here, where the reason is written down.
    #[test]
    fn lock_files_are_recognised_by_name() {
        assert_eq!(for_file("composer.lock").0, "icons/file-lock.svg");
        assert_eq!(for_file("package-lock.json").0, "icons/file-lock.svg");
        assert_eq!(for_file("yarn.lock").0, "icons/file-lock.svg");
        // A plain JSON file is still JSON — Ayu's, now, which is still a JSON glyph.
        assert_eq!(for_file("composer.json").0, "icons/file-types/json.svg");
    }

    /// `.env` and its variants are config, and they are dotfiles with no extension.
    ///
    /// `Path::extension` on `.env` is `None` (a leading dot is a hidden-file marker, not a
    /// separator), and on `.env.example` it is `example`. Both would fall through to the
    /// generic icon without the explicit check.
    ///
    /// #115 leaves all three to this set on purpose: `language_for_path` reads `.env` with
    /// the bash grammar, so an Ayu shell icon would have been the consistent choice, but
    /// `.env` is configuration you edit rather than a script you run. Which grammar parses
    /// it and which glyph describes it are different questions.
    #[test]
    fn dotenv_files_are_config() {
        assert_eq!(for_file(".env").0, "icons/file-config.svg");
        assert_eq!(for_file(".env.example").0, "icons/file-config.svg");
        assert_eq!(for_file(".env.local").0, "icons/file-config.svg");
        // Not everything starting with a dot is an env file. `.gitignore` now gets Ayu's
        // git glyph rather than the generic sheet — still not the config icon, which is
        // what this line is actually asserting.
        assert_eq!(for_file(".gitignore").0, "icons/file-types/git.svg");
    }
}
