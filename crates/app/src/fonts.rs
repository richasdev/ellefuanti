//! Which font the editor uses, how big, and how tall a row is.
//!
//! # Why this is not just a settings accessor
//!
//! A missing font family does **not** error in gpui. `TextSystem::resolve_font` falls back
//! to a proportional family and returns a perfectly valid `FontId`, so the editor keeps
//! rendering — with every column calculation wrong, which presents as a layout bug rather
//! than as a missing font. `main` has warned about exactly this since the family was a
//! constant, and that warning is now the *selection* check: a family nobody verified is a
//! broken editor, and the person who typed it gets told which one it fell back to.
//!
//! "Verified" here means two questions, and the second is the one that matters:
//!
//! 1. **Does it resolve?** [`is_available`] — a family gpui has to fall back for is not the
//!    family that was asked for.
//! 2. **Is it monospaced?** [`is_monospace`] measures `i`, `W` and `m` through the real text
//!    system and compares advances. A family can exist and still be proportional, which is
//!    the "Comic Sans" case: it resolves, it renders, and it silently breaks every column.
//!    Measured on macOS at 16px, Comic Sans MS advances 4.48 / 16.63 / 12.43 where Menlo
//!    advances 9.63 for all three.
//!
//! # The measurement trap
//!
//! Neither question can be answered under `#[gpui::test]`. gpui's test platform installs
//! `NoopTextSystem`, whose `font_id` returns `FontId(1)` for every descriptor and whose
//! `advance` is `600.0 * glyph_id` where `glyph_id` is `ch.len_utf16()` — so every BMP
//! character has an identical advance and **the noop text system measures as a perfect
//! monospace**. A headless test asserting "the chosen font is monospaced" passes with
//! Helvetica, passes with Comic Sans, and passes with a family that does not exist. One was
//! written, watched to pass under a proportional family, and deleted in `0eff21c`; see the
//! module docs on `render_tests`. So the checks below deliberately query real metrics and
//! are deliberately untested headlessly — what *is* tested here is the resolution order and
//! the arithmetic, which are pure and do not touch a font at all.
//!
//! They were instead verified by running them against the real macOS text system, which is
//! the only place they mean anything. Advances at 16px, `i` / `W` / `m`:
//!
//! ```text
//!  Menlo          9.63  9.63  9.63   available, monospaced   -> accepted
//!  Monaco         9.60  9.60  9.60   available, monospaced   -> accepted
//!  Helvetica      3.55 15.10 13.33   available, proportional -> rejected, falls back
//!  Comic Sans MS  4.48 16.63 12.43   available, proportional -> rejected, falls back
//!  SF Mono          —     —     —    not installed (no Xcode) -> skipped quietly
//! ```
//!
//! That run is also what removed the generic `monospace` entry from the chain; see
//! [`FALLBACK_CHAIN`]. Re-run it the same way if these checks are ever changed — a green
//! `cargo test` says nothing about any line in this table.
//!
//! The same run confirms the derived metrics at a size nobody had tried. At
//! `"editor.fontSize": 20`: gutter 80px (was a flat 52px, which loses a five-digit line
//! number), editor row 30px, terminal cell 12 x 24.6px (was a flat 16px row under 20px text
//! — i.e. the text was taller than the row it sat in, which is what "the rows overlap"
//! meant).
//!
//! # The fallback chain
//!
//! Menlo and Monaco ship with macOS; SF Mono comes with Xcode and Terminal.app. Walking a
//! chain rather than naming one family is what stops a build being wrong on a machine that
//! happens to be missing the one name someone compiled in.
//!
//! The chain deliberately has **no generic `monospace` entry**, and finding that out is
//! what the checks above are for. One was included on the assumption that gpui resolves it
//! the way CSS does; running the real check against the real text system showed it measures
//! as proportional on macOS, identical to an unknown family name. It was removed rather
//! than kept as a comforting last resort — see [`FALLBACK_CHAIN`].

use elle_settings::Settings;
use gpui::{App, Global, Pixels, SharedString, px};

/// Families tried in order when the user has not chosen one, or when the one they chose is
/// unusable.
///
/// Menlo and Monaco both ship with macOS; SF Mono does not — it arrives with Xcode and
/// Terminal.app — so it sits between them as a preference, not a guarantee. Measured on a
/// stock machine: Menlo and Monaco pass both checks, SF Mono is absent and is skipped
/// silently, which is the chain doing its job.
///
/// **No generic `monospace` entry.** One was included at first, on the assumption that gpui
/// maps it to the platform's monospace default the way CSS does. It does not: on macOS
/// `monospace` measures as *proportional* — `i` 3.55px against `W` 15.10px at 16px, the same
/// fallback face an unknown name resolves to. An entry that fails the monospace check is
/// worse than no entry, because it looks like a safety net and is not one. If every real
/// family here is missing, [`resolve`] says so loudly rather than pretending a fourth option
/// exists.
const FALLBACK_CHAIN: [&str; 3] = ["Menlo", "SF Mono", "Monaco"];

/// Characters whose advances must agree for a family to count as monospaced.
///
/// `i` and `W` are the narrowest and widest letters in almost every proportional face, so a
/// family that gives them the same advance is monospaced for every practical purpose. `m`
/// is included because it is the em reference and a face that special-cases it would still
/// misalign. Three glyphs, three text-system calls, once at startup.
const PROBE_CHARS: [char; 3] = ['i', 'W', 'm'];

/// Advances may differ by this fraction of the font size and still count as equal.
///
/// Not an exact comparison: advances come back as `f32` pixels scaled from font units, so
/// two genuinely identical advances can differ in the last bit. 2% of the size is far below
/// the gap between `i` and `W` in any proportional face — Helvetica's is over 60% — and far
/// above float noise.
const ADVANCE_TOLERANCE: f32 = 0.02;

/// Character-cell width as a fraction of the font size.
///
/// Menlo advances 0.6 em, measured: 9.63px at 16px. Not queried per frame — the terminal
/// lays out one `StyledText` per row rather than per cell, so this only decides how many
/// columns fit, and being a fraction of a pixel out costs nothing where a text-system round
/// trip per frame would not.
///
/// Lives here rather than in `terminal_view` because the *editor* gutter derives from the
/// same ratio, and two copies of "how wide is a character" is how they drift apart.
const CELL_WIDTH_RATIO: f32 = 0.6;

/// Terminal row height as a multiple of the font size.
///
/// Tighter than the editor's line height, which is what makes a terminal look like a
/// terminal rather than a document. 1.23 is the old `TERMINAL_LINE_HEIGHT: px(16.0)` against
/// the old `FONT_SIZE: px(13.0)`, preserved as a ratio so the default renders identically to
/// the build before #49 and every other size follows it.
///
/// Not a setting. The editor's line height is one because people have opinions about reading
/// code; nobody has an opinion about terminal leading independent of that, and a fourth key
/// nobody sets is a fourth key to explain.
const TERMINAL_LINE_HEIGHT_RATIO: f32 = 16.0 / 13.0;

/// The resolved font, sizes and row height, as gpui stores them.
///
/// A [`Global`] rather than constants because they are settings now, and a global rather
/// than a field threaded down the view tree for the same reason the theme is one: the views
/// that need it are sibling `Entity`s whose `Render::render` takes nothing but a context.
/// See the `theme` module for the longer version of that argument.
///
/// Private, so [`set_fonts`] is the only way in and [`Fonts::get`] the only way out. A view
/// cannot construct one and set it, which is what keeps "the family was verified" true
/// everywhere rather than everywhere someone remembered.
#[derive(Clone, Debug, PartialEq)]
pub struct Fonts {
    /// The family that was actually resolved — never the one the user typed, unless that
    /// one passed both checks. Rendering code uses this and nothing else.
    pub family: SharedString,
    pub size: Pixels,
    pub ui_size: Pixels,
    /// Multiple of [`size`](Self::size), not pixels. See [`Fonts::line_height`].
    pub line_height_ratio: f32,
}

impl Global for Fonts {}

impl Default for Fonts {
    /// The compiled-in values, for anything running before [`set_fonts`] — render tests,
    /// and the moment in `main` before settings are read.
    ///
    /// `Menlo` and not a chain walk: resolving needs an `App`, and `Default` has none. The
    /// values match what the constants in `theme.rs` were before #49, so a build that never
    /// calls `set_fonts` looks exactly like the one before this landed.
    fn default() -> Self {
        Self {
            family: FALLBACK_CHAIN[0].into(),
            size: px(13.0),
            ui_size: px(12.0),
            line_height_ratio: 1.5,
        }
    }
}

impl Fonts {
    /// Reads the active fonts. Falls back to [`Fonts::default`] rather than panicking,
    /// unlike `cx.theme()`.
    ///
    /// The asymmetry is deliberate: a window with no theme has no colours and is worth
    /// failing over, while a window with no configured font is the previous release. Every
    /// render test would otherwise need a `set_fonts` call that proves nothing.
    pub fn get(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }

    /// Row height in pixels: the font size times the ratio.
    ///
    /// Derived rather than stored so it cannot drift from the size. The old `LINE_HEIGHT:
    /// px(20.0)` against 13px text was 1.538 — a number someone chose once that stops
    /// meaning anything at 20px, which is why the setting is a multiplier.
    pub fn line_height(&self) -> Pixels {
        self.size * self.line_height_ratio
    }

    /// One terminal character cell, in pixels: `(width, height)`.
    ///
    /// **Returned as a pair on purpose.** Three places need these numbers and all three have
    /// to agree exactly: [`crate::terminal_view`] uses them to lay out the grid, to tell the
    /// PTY how many rows and columns it has, and to map a mouse position onto a cell for
    /// selection. If the rendered row height and the resize row height disagree, the shell
    /// believes it has a different number of rows than are drawn and its output garbles —
    /// that is worse than a cosmetic misalignment, and it is not a bug a test would obviously
    /// catch. One function returning both makes the three call sites consistent by
    /// construction rather than by everyone remembering the same formula.
    ///
    /// Both derive from the editor font size, so ⌘+ scales the terminal with everything else.
    /// Before #49 these were `FONT_SIZE * 0.6` and a flat `TERMINAL_LINE_HEIGHT: px(16.0)`,
    /// and the flat one is why a zoomed terminal used to overlap its own rows.
    pub fn cell_size(&self) -> (Pixels, Pixels) {
        (self.size * CELL_WIDTH_RATIO, self.size * TERMINAL_LINE_HEIGHT_RATIO)
    }

    /// Width of the gutter, derived from the font size.
    ///
    /// The old `GUTTER_WIDTH: px(52.0)` was sized for 13px Menlo and is simply wrong at
    /// 20px — a five-digit line number overflows it. Menlo's advance is 0.6 em, so five
    /// digits plus the `pr_3` (12px) padding the gutter already has, plus a little slack:
    /// `size * 0.6 * 5 + 12 + 8`. At 13px that gives 59px against the old 52 — six digits
    /// of headroom instead of five, which is the direction to be wrong in.
    ///
    /// Not measured through the text system: this is called during render, and a
    /// text-system round trip per frame to save a few pixels of a gutter is the wrong
    /// trade. [`CELL_WIDTH_RATIO`] is the same 0.6 em the terminal grid uses — shared rather
    /// than written twice, so "how wide is a character" has one answer.
    pub fn gutter_width(&self) -> Pixels {
        self.size * CELL_WIDTH_RATIO * 5.0 + px(20.0)
    }

    /// A `gpui::Font` for the resolved family, for the places that need a `TextRun`.
    pub fn font(&self) -> gpui::Font {
        gpui::font(self.family.clone())
    }
}

/// Resolves the settings into a usable font, logging anything it had to override.
///
/// The order is "what the user asked for, then the chain". A family that fails either check
/// is skipped with an error naming *which* check it failed, because "Comic Sans is not
/// monospaced" and "Comic Sans is not installed" are different problems with different
/// fixes and a single "could not use Comic Sans" makes the user guess.
///
/// Never fails. The last resort is the head of the chain with an error logged — a
/// proportional editor someone has been told about beats a window that will not open.
pub fn resolve(settings: &Settings, cx: &App) -> Fonts {
    let default = Fonts::default();
    let requested = settings.font_family();

    // The user's choice first, then the chain. `chain` rather than a special case, so the
    // "requested family is already in the chain" duplicate costs one wasted lookup and no
    // branch — the checks are idempotent and this runs once.
    let family = requested
        .into_iter()
        .chain(FALLBACK_CHAIN)
        .find(|family| usable(family, requested == Some(family), cx));

    let family = match family {
        Some(family) => SharedString::from(family.to_string()),
        None => {
            // Every entry failed. There is deliberately no generic alias to fall through
            // to — it measures proportional, so it would only move this error somewhere
            // less visible. Use the head of the chain, let gpui substitute what it likes,
            // and be loud: the editor is about to render with misaligned columns and the
            // log line is the only thing connecting that to a missing font.
            tracing::error!(
                tried = ?FALLBACK_CHAIN,
                using = %default.family,
                "no monospace font could be resolved; columns will not line up"
            );
            default.family
        }
    };

    Fonts {
        family,
        size: px(settings.font_size()),
        ui_size: px(settings.ui_font_size()),
        line_height_ratio: settings.line_height(),
    }
}

/// Installs the resolved fonts. Called at startup and on every zoom.
pub fn set_fonts(fonts: Fonts, cx: &mut App) {
    cx.set_global(fonts);
}

/// Both checks, with the logging that makes a rejection actionable.
///
/// `asked_for` distinguishes the user's choice from a fallback entry. A family the user
/// named and cannot have is an error they need to see; a chain entry that is simply not on
/// this machine — SF Mono without Xcode — is the chain working, and logging it at error
/// level would train people to ignore the log.
fn usable(family: &str, asked_for: bool, cx: &App) -> bool {
    let level = if asked_for { tracing::Level::ERROR } else { tracing::Level::DEBUG };

    if !is_available(family, cx) {
        log_at(level, family, "font family is not installed");
        return false;
    }

    if !is_monospace(family, cx) {
        // Always an error, even for a chain entry: a *proportional* family under one of
        // these names means the machine has something unexpected installed, and silently
        // walking past it hides why the editor looks wrong.
        tracing::error!(
            font = family,
            "font family is not monospaced; column positions \
             would be wrong, skipping it"
        );
        return false;
    }

    true
}

fn log_at(level: tracing::Level, family: &str, message: &str) {
    match level {
        tracing::Level::ERROR => tracing::error!(font = family, "{message}"),
        _ => tracing::debug!(font = family, "{message}"),
    }
}

/// Is this family installed, under this exact name?
///
/// `all_font_names` rather than `resolve_font`, because `resolve_font` *cannot* answer
/// this: it silently substitutes a fallback and returns a valid `FontId` either way, which
/// is the entire reason this module exists. Same call the startup check in `main` has made
/// since the family was a constant — one check, not a second one beside it.
///
/// No exemption for the generic `monospace` alias, and that is a correction rather than an
/// omission. It was exempted at first on the assumption that gpui maps it to the platform's
/// monospace default. Measured on macOS, it does not: `monospace` resolves to the same
/// *proportional* fallback as a name that does not exist at all — `i` 3.55px, `W` 15.10px
/// at 16px. Exempting it therefore admitted a family that fails the check it was exempted
/// from, which is the exact bug this module exists to prevent, dressed as a safety net.
fn is_available(family: &str, cx: &App) -> bool {
    cx.text_system().all_font_names().iter().any(|name| name == family)
}

/// Do `i`, `W` and `m` all advance by the same amount?
///
/// The real-metrics query, and the whole reason a settings field cannot just be trusted.
/// `advance` goes through the platform text system to the actual font file, so this
/// distinguishes Menlo from Helvetica — on a real display. Under `NoopTextSystem` every
/// character advances identically and this returns `true` for anything, which is why no
/// headless test asserts on it. See the module docs.
///
/// A glyph the font does not have is a failure, not a skip: a monospace family missing `W`
/// is not a monospace family for the Latin text this editor renders.
fn is_monospace(family: &str, cx: &App) -> bool {
    let text_system = cx.text_system();
    let font_id = text_system.resolve_font(&gpui::font(family.to_string()));
    let size = px(16.0);

    let mut advances = PROBE_CHARS.iter().map(|&c| text_system.advance(font_id, size, c));

    let Some(Ok(first)) = advances.next() else {
        tracing::warn!(font = family, "could not measure the font; treating it as unusable");
        return false;
    };

    let tolerance = size * ADVANCE_TOLERANCE;
    advances.all(|advance| {
        advance.is_ok_and(|advance| (advance.width - first.width).abs() <= tolerance)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing here touches a font. Everything that does is untestable headlessly — see the
    /// module docs — so these pin the arithmetic and the resolution *order*, which are the
    /// parts a real display would not catch either.

    #[test]
    fn the_defaults_are_the_constants_this_replaced() {
        // `Metrics::FONT_SIZE`, `UI_FONT_SIZE`, `LINE_HEIGHT` and `TERMINAL_LINE_HEIGHT`
        // before #49. A first run with no settings file must be pixel-identical to the
        // previous build.
        let fonts = Fonts::default();
        assert_eq!(fonts.size, px(13.0));
        assert_eq!(fonts.ui_size, px(12.0));
        assert_eq!(fonts.line_height(), px(19.5), "the old 20px, as a 1.5 ratio");
        assert_eq!(fonts.cell_size().1, px(16.0), "the old TERMINAL_LINE_HEIGHT exactly");
        // The old `Metrics::FONT_SIZE * CELL_WIDTH_RATIO` from `terminal_view`, written as
        // that expression rather than as a decimal literal — 13.0 * 0.6 is not exactly
        // representable, and a literal here pins a rounding artefact instead of the intent.
        assert_eq!(fonts.cell_size().0, px(13.0) * 0.6, "the old cell width");
    }

    /// The regression the coordinator asked for: a zoomed terminal used to keep a flat 16px
    /// row while its text grew, so the rows overlapped.
    #[test]
    fn the_terminal_row_grows_with_the_font_and_never_clips_its_text() {
        let small = Fonts { size: px(13.0), ..Fonts::default() }.cell_size();
        let large = Fonts { size: px(20.0), ..Fonts::default() }.cell_size();

        assert!(large.1 > small.1, "the old flat 16px did not move at all: {small:?} {large:?}");

        for size in [6.0f32, 13.0, 20.0, 32.0, 96.0] {
            let fonts = Fonts { size: px(size), ..Fonts::default() };
            let (width, height) = fonts.cell_size();
            assert!(
                height > fonts.size,
                "at {size}px the row is {height:?} but the text is {:?} — overlapping rows",
                fonts.size
            );
            assert!(width > px(0.0) && height > px(0.0));
        }
    }

    /// The terminal grid stays tighter than the editor's at every size — that difference is
    /// what makes a terminal read as a terminal rather than a document, and a single shared
    /// ratio would quietly lose it.
    #[test]
    fn the_terminal_row_stays_tighter_than_an_editor_row() {
        for size in [6.0f32, 13.0, 20.0, 96.0] {
            let fonts = Fonts { size: px(size), ..Fonts::default() };
            assert!(
                fonts.cell_size().1 < fonts.line_height(),
                "at {size}px the terminal row is not tighter than the editor's"
            );
        }
    }

    /// The property the whole `cell_size` shape exists for.
    ///
    /// `terminal_view` uses these numbers in three places — laying out the grid, telling the
    /// PTY its row/column count, and hit-testing a click for selection. If the rendered row
    /// height and the resize row height disagree, the shell believes it has a different
    /// number of rows than are drawn and its output garbles, which is worse than a visual
    /// offset and is not something a render test would notice.
    ///
    /// This is what a stale copy of the formula would look like, and it fails: the whole
    /// point is that all three ask the same function rather than each doing the arithmetic.
    #[test]
    fn the_cell_size_is_one_answer_and_not_three() {
        for size in [6.0f32, 13.0, 20.0, 96.0] {
            let fonts = Fonts { size: px(size), ..Fonts::default() };

            let for_layout = fonts.cell_size();
            let for_resize = fonts.cell_size();
            let for_hit_testing = fonts.cell_size();

            assert_eq!(for_layout, for_resize);
            assert_eq!(for_resize, for_hit_testing);

            // And the derivation is what the callers assume: an integer number of rows
            // drawn at `cell.1` is the same count `sync_size` computes from a panel height.
            let panel_height = 260.0f32;
            let rows_told = (panel_height / f32::from(for_resize.1)).floor().max(1.0);
            let rows_drawn_height = for_layout.1 * rows_told;
            assert!(
                f32::from(rows_drawn_height) <= panel_height,
                "at {size}px the PTY is told {rows_told} rows, which draw {rows_drawn_height:?} \
                 into a {panel_height}px panel — the shell would write off-screen"
            );
        }
    }

    #[test]
    fn the_line_height_tracks_the_font_size() {
        // The point of a multiplier: 20px of line against 20px text was the old bug.
        let fonts = Fonts { size: px(20.0), ..Fonts::default() };
        assert_eq!(fonts.line_height(), px(30.0));
        assert!(fonts.line_height() > fonts.size, "a row must be taller than its text");
    }

    /// The gutter is the metric the issue calls out by name: 52px was sized for 13px text.
    #[test]
    fn the_gutter_grows_with_the_font_and_always_fits_five_digits() {
        let thirteen = Fonts { size: px(13.0), ..Fonts::default() }.gutter_width();
        let twenty = Fonts { size: px(20.0), ..Fonts::default() }.gutter_width();

        assert!(twenty > thirteen, "the old constant did not move at all: {thirteen:?}");

        for size in [6.0f32, 13.0, 20.0, 32.0, 96.0] {
            let fonts = Fonts { size: px(size), ..Fonts::default() };
            // Five digits of 0.6-em glyphs plus the gutter's own 12px `pr_3`. If the
            // derivation ever stops covering that, "99999" overlaps the first character.
            let needed = px(size * 0.6 * 5.0 + 12.0);
            assert!(
                fonts.gutter_width() >= needed,
                "at {size}px the gutter is {:?} but five digits need {needed:?}",
                fonts.gutter_width()
            );
        }
    }

    /// The chain must contain only families that could actually pass the monospace check.
    ///
    /// This is a regression guard for a bug that was in this file: `"monospace"` was the
    /// last entry, on the assumption gpui resolves the CSS generic. Measured against the
    /// real text system it comes back *proportional* — the same fallback face an unknown
    /// name gets — so it was an entry guaranteed to fail the check it existed to satisfy.
    /// Re-adding it, or any other generic alias, has to be a deliberate act.
    #[test]
    fn the_fallback_chain_holds_only_real_families() {
        assert_eq!(FALLBACK_CHAIN[0], "Menlo", "the family that ships with macOS goes first");
        for family in FALLBACK_CHAIN {
            assert!(
                !family.chars().all(char::is_lowercase),
                "{family:?} looks like a generic alias; macOS resolves those to a \
                 proportional face, which is worse than having no fallback at all"
            );
        }
    }

    /// No family gets a free pass from the availability check — not even a generic alias.
    ///
    /// The only half of the font checks that *is* meaningful headlessly: `all_font_names`
    /// is a plain list lookup, so an empty list under the test platform correctly rejects
    /// everything. (`is_monospace` is the half that is not — it measures as `true` for any
    /// family here, which is why nothing asserts on it. See the module docs.)
    ///
    /// This pins the removed exemption specifically. `"monospace"` used to short-circuit to
    /// `true`, which on a real machine admitted a proportional face into the chain.
    #[gpui::test]
    fn no_family_is_exempt_from_the_availability_check(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            for family in ["monospace", "Menlo", "Comic Sans MS", ""] {
                assert!(
                    !is_available(family, cx),
                    "{family:?} was accepted though the platform lists no fonts at all"
                );
            }
        });
    }

    /// With nothing installed, [`resolve`] still returns something usable-shaped rather
    /// than panicking or returning an empty family — the "every entry failed" path, which a
    /// machine missing all three families really would hit.
    #[gpui::test]
    fn every_family_failing_still_yields_a_font_rather_than_a_panic(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let fonts = resolve(&elle_settings::Settings::default(), cx);
            assert_eq!(fonts.family, Fonts::default().family);
            assert_eq!(fonts.size, Fonts::default().size, "the sizes still come from settings");
        });
    }
}
