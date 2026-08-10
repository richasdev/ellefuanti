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
//! # The advance ratio, and why 0.6 was not good enough (#92)
//!
//! #85 recorded *whether* families are monospaced but not *how wide* they are, and the
//! terminal used an assumed `0.6`. Measured through `text_system().advance()` on `m`, the
//! ratio is identical at 13, 16 and 20px — it is a property of the face, not the size:
//!
//! ```text
//!  Menlo        0.602051    9.6328px at 16px
//!  Monaco       0.600098    9.6016px at 16px
//!  Courier New  0.600098    9.6016px at 16px
//!  Andale Mono  0.600098    9.6016px at 16px
//!  SF Mono          —       not installed (no Xcode)
//! ```
//!
//! **Every one is above 0.6.** The assumption therefore always over-estimates how many
//! characters fit, never under — and `sync_size` floors a division by it to decide how many
//! columns to tell the PTY it has. At 13px Menlo that hands out a column too many at 53% of
//! window widths, and since the *shell* does the wrapping, `ls` and `git log --graph` format
//! to a width that is not there. That is #92, reported as "it breaks lines out of nowhere".
//!
//! So the ratio is measured once in [`resolve`] and cached on [`Fonts`]. The objection in
//! [`Fonts::gutter_width`] to measuring — a text-system round trip per frame — was about
//! measuring *per render*, and caching removes it for both callers.
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

/// Character-cell width as a fraction of the font size, for when nothing has been measured.
///
/// A starting point, not the answer: [`Fonts::advance_ratio`] holds the measured one and is
/// what every caller actually gets. This is the value for a `Fonts::default()` — render
/// tests, and the moment in `main` before the text system has been asked anything.
///
/// **0.6 is an assumption and it is wrong for every monospace family on macOS**, which is
/// #92. Measured through `text_system().advance()`: Menlo 0.602051, Monaco / Courier New /
/// Andale Mono 0.600098. All of them are *above* 0.6, so assuming 0.6 always over-estimates
/// how many characters fit — see [`Fonts::advance_ratio`] for what that costs.
///
/// Lives here rather than in `terminal_view` because the *editor* gutter derives from the
/// same ratio, and two copies of "how wide is a character" is how they drift apart.
const CELL_WIDTH_RATIO: f32 = 0.6;

/// The character the advance is measured from.
///
/// `m` is the em reference and is one of the three [`PROBE_CHARS`] a family must agree on to
/// be accepted at all, so on any family that got this far it is *the* advance rather than
/// one of several.
const ADVANCE_PROBE_CHAR: char = 'm';

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
    /// The resolved family's real glyph advance, as a fraction of the font size — measured
    /// once in [`resolve`], never per frame. `None` until something has measured it, which
    /// is [`Fonts::default`] and nothing else on the real startup path.
    ///
    /// Read through [`Fonts::advance_ratio`], which supplies [`CELL_WIDTH_RATIO`] when this
    /// is `None`, so no caller has to know the difference.
    pub measured_advance_ratio: Option<f32>,
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
            // Nothing has been measured: `Default` has no `App` and so no text system, the
            // same reason the family here is a name rather than a chain walk.
            measured_advance_ratio: None,
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
    /// How wide one character is, as a fraction of the font size — measured if anything has
    /// measured it, [`CELL_WIDTH_RATIO`] if not.
    ///
    /// Free to call: the measurement happened once in [`resolve`], and this reads a field.
    /// That is the whole point of caching it on the struct — see [`Fonts::cell_size`] for
    /// why the terminal cannot use the assumed number.
    pub fn advance_ratio(&self) -> f32 {
        self.measured_advance_ratio.unwrap_or(CELL_WIDTH_RATIO)
    }

    pub fn cell_size(&self) -> (Pixels, Pixels) {
        // The *measured* advance, not [`CELL_WIDTH_RATIO`] — this is the #92 fix. `sync_size`
        // divides the panel width by this and floors to get the column count it hands the
        // PTY, so a cell even slightly narrower than the real glyph buys the shell a column
        // that cannot be drawn. Menlo advances 0.602051 em against the assumed 0.6, and at
        // 13px that one part in 350 hands out one column too many at 53% of window widths.
        (self.size * self.advance_ratio(), self.size * TERMINAL_LINE_HEIGHT_RATIO)
    }

    /// Width of the gutter, derived from the font size.
    ///
    /// The old `GUTTER_WIDTH: px(52.0)` was sized for 13px Menlo and is simply wrong at
    /// 20px — a five-digit line number overflows it. Menlo's advance is 0.6 em, so five
    /// digits plus the `pr_3` (12px) padding the gutter already has, plus a little slack:
    /// `size * 0.6 * 5 + 12 + 8`. At 13px that gives 59px against the old 52 — six digits
    /// of headroom instead of five, which is the direction to be wrong in.
    ///
    /// Still one shared answer to "how wide is a character" — [`Fonts::advance_ratio`], the
    /// same call [`cell_size`](Self::cell_size) makes. What changed in #92 is where that
    /// answer comes from: it is measured once in [`resolve`] and cached, so this is a field
    /// read and the old objection to measuring is gone rather than worked around.
    ///
    /// That objection — *"this is called during render, and a text-system round trip per
    /// frame to save a few pixels of a gutter is the wrong trade"* — was right about the
    /// gutter and wrong about the terminal, and the number was shared, so the gutter's
    /// tolerance set the terminal's. A few pixels of gutter is cosmetic; the same few pixels
    /// in `cell_size` decide what the shell believes its width is. Measuring once serves
    /// both without the round trip either was avoiding.
    pub fn gutter_width(&self) -> Pixels {
        self.size * self.advance_ratio() * 5.0 + px(20.0)
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

    // Measured here and nowhere else: `resolve` runs at startup and on a settings change,
    // which is exactly when the family can change. Zoom does not come through here — it
    // rebuilds `Fonts` from the current one in `settings::adjust_font_size`, keeping this
    // ratio, which is correct because a ratio is per-family and scale-free. Verified: Menlo
    // measures 0.602051 at 13, 16 and 20px.
    let measured_advance_ratio = measure_advance_ratio(&family, cx);

    Fonts {
        family,
        size: px(settings.font_size()),
        ui_size: px(settings.ui_font_size()),
        line_height_ratio: settings.line_height(),
        measured_advance_ratio,
    }
}

/// The family's real advance as a fraction of the font size, or `None` if it cannot be
/// measured.
///
/// The honest number behind [`CELL_WIDTH_RATIO`], and #92's fix. Measured at one size and
/// stored as a ratio because that is what it is: advance scales linearly with size, and
/// Menlo returns 0.602051 at 13, 16 and 20px to six figures.
///
/// `None` rather than a guess when the text system cannot answer — the caller falls back to
/// the assumed constant, which is the behaviour that shipped, so a font that cannot be
/// measured is no worse off than before.
fn measure_advance_ratio(family: &str, cx: &App) -> Option<f32> {
    let text_system = cx.text_system();
    let font_id = text_system.resolve_font(&gpui::font(family.to_string()));

    // 16px rather than the configured size: a fixed probe size keeps this one number per
    // family, and the ratio is what gets stored anyway.
    let size = px(16.0);
    let advance = text_system.advance(font_id, size, ADVANCE_PROBE_CHAR).ok()?;
    let ratio = f32::from(advance.width) / f32::from(size);

    // A zero or negative advance is a text system that answered without measuring — the
    // `NoopTextSystem` under `#[gpui::test]` returns 600.0 for every character, which is a
    // ratio of 37.5 and would hand the terminal one column. Anything outside a plausible
    // monospace range means the answer is not about this font, so decline it and let the
    // constant stand.
    (0.4..=0.9).contains(&ratio).then_some(ratio)
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

    /// #92: the columns handed to the PTY must all be drawable.
    ///
    /// This is the bug the user reported as *"quebra a linha do nada"*. `sync_size` divides
    /// the panel width by `cell_size().0` and floors, and sends that count to the PTY via
    /// `resize_all` — so the *shell* does the wrapping. Claim one column more than fits and
    /// everything that formats to its own width (`ls`, `git log --graph`, `top`, a progress
    /// bar) breaks a line at a width that does not exist, with nothing visible to explain it.
    ///
    /// The check is the arithmetic, not a font: given a real advance ratio, the count derived
    /// from `cell_size` must fit inside the panel when drawn at that real advance. No window,
    /// no text system — which matters, because the text system that *is* available headlessly
    /// measures every font as 600.0 and would make this pass on anything. See the module docs.
    ///
    /// **Fails against the assumed `CELL_WIDTH_RATIO = 0.6`.** With Menlo's measured 0.602051
    /// at 13px it over-claims a column at 53% of the widths swept below.
    #[test]
    fn the_pty_is_never_told_it_has_a_column_that_cannot_be_drawn() {
        // Measured through `text_system().advance()` on macOS, one glyph at 16px. Every
        // monospace family on the machine is *above* the assumed 0.6, so the assumption
        // always over-estimates how many characters fit — it never errs the safe way.
        for (family, real_ratio) in
            [("Menlo", 0.602_051_f32), ("Monaco", 0.600_098), ("Courier New", 0.600_098)]
        {
            for size in [6.0f32, 13.0, 16.0, 20.0, 32.0] {
                let fonts = Fonts {
                    size: px(size),
                    measured_advance_ratio: Some(real_ratio),
                    ..Fonts::default()
                };
                let cell_width = f32::from(fonts.cell_size().0);

                // A sweep rather than the three widths in the issue: the bug appears and
                // disappears with window size, which is what made it look like it came from
                // nowhere. Anything that only fires at some widths has to be checked at many.
                for width in (400..=2000).step_by(1).map(|w| w as f32) {
                    let cols = (width / cell_width).floor().max(1.0);

                    // What those columns actually occupy when drawn with the real glyph.
                    let drawn = cols * size * real_ratio;
                    assert!(
                        drawn <= width,
                        "{family} at {size}px in a {width}px panel: the PTY is told {cols} \
                         columns, which draw {drawn}px — the shell wraps at a width that is \
                         not there"
                    );
                }
            }
        }
    }

    /// The other half of the same trade: not so conservative that a column goes missing.
    ///
    /// Rounding down is the right direction — an unused pixel column is invisible and a
    /// column too many is #92 — but "tell the PTY it has one column" would also satisfy
    /// that. The cell must stay within a pixel of the real glyph, so at most one column is
    /// ever given up.
    #[test]
    fn rounding_down_costs_at_most_one_column() {
        let real_ratio = 0.602_051_f32;
        for size in [13.0f32, 16.0, 20.0] {
            let fonts = Fonts {
                size: px(size),
                measured_advance_ratio: Some(real_ratio),
                ..Fonts::default()
            };
            let cell_width = f32::from(fonts.cell_size().0);

            for width in (400..=2000).step_by(7).map(|w| w as f32) {
                let cols = (width / cell_width).floor().max(1.0);
                let fits = (width / (size * real_ratio)).floor().max(1.0);
                assert!(
                    cols >= fits - 1.0,
                    "at {size}px in {width}px the PTY is told {cols} columns where {fits} fit \
                     — giving up more than one column is a visible margin, not a rounding"
                );
            }
        }
    }

    /// The measured ratio must actually reach the callers, or the fix is decoration.
    ///
    /// Both consumers, because they share the number by design (#85) and #92 changed where
    /// it comes from rather than splitting it in two.
    #[test]
    fn a_measured_advance_reaches_both_the_cell_and_the_gutter() {
        let assumed = Fonts { size: px(13.0), ..Fonts::default() };
        let measured = Fonts { measured_advance_ratio: Some(0.602_051), ..assumed.clone() };

        assert_eq!(assumed.advance_ratio(), CELL_WIDTH_RATIO, "unmeasured falls back");
        assert_eq!(measured.advance_ratio(), 0.602_051);

        assert!(measured.cell_size().0 > assumed.cell_size().0, "the cell must widen");
        assert!(measured.gutter_width() > assumed.gutter_width(), "so must the gutter");

        // The row height is untouched: it is set directly from the size rather than divided
        // out of a glyph, which is why the vertical axis never had this bug.
        assert_eq!(measured.cell_size().1, assumed.cell_size().1);
    }

    /// An unmeasurable font is no worse off than before the fix.
    ///
    /// The fallback path matters more than it looks: under `#[gpui::test]` the noop text
    /// system reports 600.0px for every glyph — a ratio of 37.5 — and taking that at face
    /// value would tell the PTY it has one column. Implausible ratios are declined, so the
    /// assumed constant stands and behaviour matches what shipped.
    #[test]
    fn an_implausible_measurement_is_declined_rather_than_used() {
        let fonts = Fonts { measured_advance_ratio: None, ..Fonts::default() };
        assert_eq!(fonts.advance_ratio(), CELL_WIDTH_RATIO);
        assert_eq!(fonts.cell_size().0, px(13.0) * CELL_WIDTH_RATIO, "the shipped behaviour");
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
