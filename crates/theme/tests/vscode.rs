//! The importer against real, published VS Code theme files.
//!
//! The unit tests in `src/` use fixtures small enough to reason about. These use the actual
//! files, because the failure this issue is about — resolving a scope by file order instead
//! of by specificity — only shows up in a file with 275 rules in an awkward order.
//!
//! # Why these run against the developer's machine and skip when absent
//!
//! The three files are VS Code extensions, installed under `~/.vscode/extensions/`. They are
//! not vendored: One Dark Pro's theme file alone is 100 kB of somebody else's JSON, and
//! checking it in to test a parser means this repo redistributing it. So the tests skip when
//! the extension is not installed.
//!
//! A skipped test proves nothing, and that is a real cost — CI without these extensions runs
//! this file as a no-op. It is accepted because the property they check is *also* pinned by
//! `crates/app/src/theme.rs`'s `ported_themes_reproduce_the_colours_that_make_them_that_theme`,
//! which hardcodes the same values and runs everywhere. These tests are what proves the
//! importer agrees with those constants; that test is what keeps the constants honest.

use std::path::{Path, PathBuf};

use elle_theme::Appearance;

const GITHUB: &str = "github.github-vscode-theme-6.3.5";
const ONE_DARK: &str = "zhuangtongfa.material-theme-3.19.0";

/// A theme file in an installed extension, or `None` if the extension is not installed.
fn extension_theme(extension: &str, file: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path =
        Path::new(&home).join(".vscode/extensions").join(extension).join("themes").join(file);
    path.exists().then_some(path)
}

/// Imports a theme, or returns `None` so the caller can skip.
fn import(extension: &str, file: &str, name: &str) -> Option<elle_theme::ThemeFile> {
    let path = extension_theme(extension, file)?;
    Some(elle_theme::import(&path, name).unwrap_or_else(|error| panic!("{error}")))
}

/// One Dark Pro, and the scope-ordering bug that made this issue worth filing.
///
/// Every value here is `crates/app/src/theme.rs`'s `Theme::one_dark_pro()`, which was
/// hand-extracted from this same file during #53. The importer reading the file and
/// producing the same numbers is the end-to-end check that the extraction and the parser
/// agree.
#[test]
fn one_dark_pro_imports_to_the_colours_that_were_hand_extracted_from_it() {
    let Some(theme) = import(ONE_DARK, "OneDark-Pro.json", "one-dark-pro") else { return };

    assert_eq!(theme.appearance, Appearance::Dark, "the file declares type: dark");

    assert_eq!(theme.color("background"), Some(0x282c34));
    assert_eq!(theme.color("text"), Some(0xabb2bf));
    assert_eq!(theme.color("keyword"), Some(0xc678dd));
    assert_eq!(theme.color("type"), Some(0xe5c07b));
    assert_eq!(theme.color("function"), Some(0x61afef));
    assert_eq!(theme.color("variable"), Some(0xe06c75));
    assert_eq!(theme.color("string"), Some(0x98c379));
    assert_eq!(theme.color("number"), Some(0xd19a66));
    assert_eq!(theme.color("comment"), Some(0x7f848e));
    assert_eq!(theme.color("tag"), Some(0xe06c75));
    assert_eq!(theme.color("cursor"), Some(0x528bff));

    // **The #53 bug.** `entity.name.tag` (#e06c75) is listed before
    // `entity.other.attribute-name` (#d19a66) in this file, so a first-hit resolver reports
    // the tag colour. Only resolving by specificity gets the published value.
    assert_eq!(
        theme.color("attribute"),
        Some(0xd19a66),
        "resolved by file order instead of specificity — this is the #53 bug"
    );

    // Upstream really does paint these two alike, and a port must reproduce it rather than
    // apply this project's own distinctness rule.
    assert_eq!(theme.color("variable"), theme.color("property"), "both are #e06c75 upstream");
    assert_eq!(theme.color("operator"), theme.color("text"), "operator is the foreground");
}

#[test]
fn github_dark_imports_to_the_colours_that_were_hand_extracted_from_it() {
    let Some(theme) = import(GITHUB, "dark-default.json", "github-dark") else { return };

    // The file has no `type` key at all; this is the luminance inference.
    assert_eq!(theme.appearance, Appearance::Dark);

    assert_eq!(theme.color("background"), Some(0x0d1117));
    assert_eq!(theme.color("text"), Some(0xe6edf3));
    assert_eq!(theme.color("keyword"), Some(0xff7b72));
    assert_eq!(theme.color("string"), Some(0xa5d6ff));
    assert_eq!(theme.color("comment"), Some(0x8b949e));
    assert_eq!(theme.color("function"), Some(0xd2a8ff));
    assert_eq!(theme.color("variable"), Some(0xffa657));
    assert_eq!(theme.color("tag"), Some(0x7ee787));
    assert_eq!(theme.color("panel"), Some(0x010409));
    assert_eq!(theme.color("cursor"), Some(0x2f81f7));

    // **A disagreement with the hand-extracted `theme.rs`, and the file is right.**
    //
    // `Theme::github_dark`'s doc comment says `attribute` "follows `entity.name.tag`
    // (#7ee787)". It cannot: `entity.name.tag` is a rule about tags, and an attribute's
    // scope is `entity.other.attribute-name`. The two diverge at the second segment, so the
    // tag rule never matches an attribute. Exactly one selector in this file does — the bare
    // `entity`, grouped in with `constant` and `variable.language` — and it is `#79c0ff`.
    //
    // That is what VS Code paints for an HTML attribute in this theme, so it is what this
    // paints. The compiled-in constant is corrected in the same commit as this test.
    assert_eq!(
        theme.color("attribute"),
        Some(0x79c0ff),
        "the only selector matching entity.other.attribute-name is the bare `entity`"
    );
    assert_ne!(
        theme.color("attribute"),
        theme.color("tag"),
        "attributes and tags are different scopes and this theme colours them differently"
    );

    // **The second disagreement, and again the file is right.**
    //
    // `Theme::github_dark` says `operator` "has no scope at all, so it inherits
    // `editor.foreground` (#e6edf3). That is what VS Code renders". It is not. There is no
    // `keyword.operator` *rule* in this file, but that is not the same as no rule matching:
    // the bare `keyword` selector matches `keyword.operator` and everything under it, by the
    // same prefix rule that makes `entity` match an attribute above.
    //
    // Checked against the grammar rather than reasoned about — VS Code's own PHP grammar
    // scopes `=`, `->`, `??` and the rest as `keyword.operator.assignment.php`,
    // `keyword.operator.class.php`, `keyword.operator.null-coalescing.php` and seventeen
    // more, all of which begin `keyword.`. So GitHub Dark paints PHP operators its keyword
    // red, and `#e6edf3` is a colour they never take.
    assert_eq!(
        theme.color("operator"),
        Some(0xff7b72),
        "the bare `keyword` rule covers keyword.operator.*"
    );
    assert_eq!(theme.color("operator"), theme.color("keyword"), "which is the keyword colour");
}

#[test]
fn github_light_imports_to_the_colours_that_were_hand_extracted_from_it() {
    let Some(theme) = import(GITHUB, "light-default.json", "github-light") else { return };

    // Also no `type` key — and this is the case where getting it wrong matters, because a
    // light theme with a dark theme's ANSI defaults is #48's "actively wrong" outcome.
    assert_eq!(theme.appearance, Appearance::Light, "inferred from a #ffffff background");

    assert_eq!(theme.color("background"), Some(0xffffff));
    assert_eq!(theme.color("text"), Some(0x1f2328));
    assert_eq!(theme.color("keyword"), Some(0xcf222e));
    assert_eq!(theme.color("string"), Some(0x0a3069));
    assert_eq!(theme.color("comment"), Some(0x6e7781));
    assert_eq!(theme.color("function"), Some(0x8250df));
    assert_eq!(theme.color("variable"), Some(0x953800));
    // The light counterpart of the same correction; see `github_dark`'s note above.
    assert_eq!(theme.color("attribute"), Some(0x0550ae), "the bare `entity` rule, not the tag");
    assert_eq!(theme.color("tag"), Some(0x116329));
    // The light counterpart of the operator correction; see `github_dark`'s note.
    assert_eq!(theme.color("operator"), Some(0xcf222e), "the bare `keyword` rule, not foreground");
    assert_eq!(theme.color("panel"), Some(0xf6f8fa));
}

/// The ANSI tables, read straight from `terminal.ansi*`.
///
/// **This is where the importer disagrees with the hand-extracted `theme.rs`, and the file
/// is right.** `Theme::one_dark_pro()`'s table has eight slots that are not One Dark Pro's
/// terminal colours — they are its *syntax* colours, substituted during the manual
/// extraction. Slot 1 is `#e06c75` (the keyword red) where the file says `#e05561`; slot 10
/// is `#4cd137`, a green that appears nowhere in the file at all.
///
/// The values asserted here are the file's, verified by reading the `terminal.ansi*` keys
/// directly. The compiled-in table is corrected in the same commit as this test.
#[test]
fn the_ansi_tables_come_from_the_terminal_keys_and_not_from_the_syntax_palette() {
    if let Some(theme) = import(ONE_DARK, "OneDark-Pro.json", "one-dark-pro") {
        let ansi = theme.ansi().expect("a parsed theme has all sixteen slots");

        assert_eq!(ansi[0], 0x3f4451, "ansiBlack");
        assert_eq!(ansi[1], 0xe05561, "ansiRed — NOT the #e06c75 keyword red");
        assert_eq!(ansi[2], 0x8cc265, "ansiGreen — NOT the #98c379 string green");
        assert_eq!(ansi[3], 0xd18f52, "ansiYellow");
        assert_eq!(ansi[4], 0x4aa5f0, "ansiBlue");
        assert_eq!(ansi[5], 0xc162de, "ansiMagenta");
        assert_eq!(ansi[6], 0x42b3c2, "ansiCyan");
        assert_eq!(ansi[7], 0xd7dae0, "ansiWhite");
        assert_eq!(ansi[8], 0x4f5666, "ansiBrightBlack");
        assert_eq!(ansi[9], 0xff616e, "ansiBrightRed");
        assert_eq!(ansi[10], 0xa5e075, "ansiBrightGreen — the #4cd137 in theme.rs is invented");
        assert_eq!(ansi[11], 0xf0a45d, "ansiBrightYellow");
        assert_eq!(ansi[12], 0x4dc4ff, "ansiBrightBlue");
        assert_eq!(ansi[13], 0xde73ff, "ansiBrightMagenta");
        assert_eq!(ansi[14], 0x4cd1e0, "ansiBrightCyan");
        assert_eq!(ansi[15], 0xe6e6e6, "ansiBrightWhite");
    }

    // GitHub's two tables were extracted correctly, with one exception in the dark theme's
    // last slot: the file says #ffffff and `theme.rs` says #f0f6fc.
    if let Some(theme) = import(GITHUB, "dark-default.json", "github-dark") {
        let ansi = theme.ansi().unwrap();
        assert_eq!(
            ansi,
            [
                0x484f58, 0xff7b72, 0x3fb950, 0xd29922, 0x58a6ff, 0xbc8cff, 0x39c5cf, 0xb1bac4,
                0x6e7681, 0xffa198, 0x56d364, 0xe3b341, 0x79c0ff, 0xd2a8ff, 0x56d4dd, 0xffffff,
            ]
        );
    }

    if let Some(theme) = import(GITHUB, "light-default.json", "github-light") {
        let ansi = theme.ansi().unwrap();
        assert_eq!(
            ansi,
            [
                0x24292f, 0xcf222e, 0x116329, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
                0x57606a, 0xa40e26, 0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
            ]
        );
    }
}

/// GitHub's themes set no `editorError.foreground`, which `theme.rs` claims they do.
///
/// The doc comment on `Theme::github_dark` says the error colour is "GitHub Dark's
/// `editorError.foreground` (#ff7b72)". That key is absent from the file. #ff7b72 is right —
/// it is the theme's red, reachable as `keyword` and as `terminal.ansiRed` — but it did not
/// come from where the comment says, and the comment is corrected in this commit.
///
/// What the importer does instead is documented in `DIAGNOSTIC_KEYS` and `FALLBACKS`: try the
/// error keys, and where a theme is silent, borrow from its own palette rather than invent.
#[test]
fn a_theme_that_sets_no_diagnostic_colours_still_gets_them_from_its_own_palette() {
    let Some(theme) = import(GITHUB, "dark-default.json", "github-dark") else { return };

    let error = theme.color("error").expect("required");
    let warning = theme.color("warning").expect("required");

    // Never black, never absent — the issue's "fall back deliberately" rule.
    assert_ne!(error, 0x000000);
    assert_ne!(warning, 0x000000);

    // And in the theme's own register: both come from keys the theme does set.
    assert_ne!(error, theme.color("background").unwrap(), "an invisible error squiggle");
}

/// Every imported theme is a *complete* theme.
///
/// The importer's contract: it either produces all 26 required colours plus 16 ANSI slots,
/// or it fails naming the ones it could not fill. A theme that imports with holes in it is
/// the "80% right and impossible to debug" outcome the format is arranged to prevent.
#[test]
fn every_imported_theme_has_every_required_colour() {
    let sources = [
        (GITHUB, "dark-default.json", "github-dark"),
        (GITHUB, "light-default.json", "github-light"),
        (ONE_DARK, "OneDark-Pro.json", "one-dark-pro"),
    ];

    for (extension, file, name) in sources {
        let Some(theme) = import(extension, file, name) else { continue };

        for key in elle_theme::REQUIRED_COLORS {
            assert!(theme.color(key).is_some(), "{name}: no colour for {key}");
        }
        assert!(theme.ansi().is_some(), "{name}: an incomplete ANSI table");

        // Text on its own background is invisible; the cheapest readability check there is.
        assert_ne!(theme.color("text"), theme.color("background"), "{name}: invisible body text");
    }
}

/// An imported theme survives being written out and read back.
///
/// This is the whole point of the native format: import once, drop the result in
/// `assets/themes/`, and load it thereafter without the VS Code file.
#[test]
fn an_imported_theme_round_trips_through_the_native_format() {
    let Some(mut theme) = import(ONE_DARK, "OneDark-Pro.json", "one-dark-pro") else { return };
    theme.origin = Some("zhuangtongfa.material-theme v3.19.0, MIT".to_string());

    let text = theme.to_json();
    let read = elle_theme::ThemeFile::parse(Path::new("one-dark-pro.json"), &text)
        .unwrap_or_else(|error| panic!("{error}"));

    assert_eq!(read, theme, "the native format must not lose anything the importer produced");
}
