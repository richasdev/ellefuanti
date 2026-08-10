//! A real caret: a thin quad between two characters, positioned by measuring text.
//!
//! # Why this is a custom `Element` and the block cursor was not
//!
//! The block cursor this replaces was a *background highlight on the character under the
//! cursor* — a colour run inside `line_runs`, needing no measurement and no absolute
//! layout. That is why it survived so long: it is genuinely the cheapest thing that shows
//! where the cursor is, and on a proportional fallback font it stays correct because the
//! text system positions the character, not us.
//!
//! It is also ambiguous in a way that matters. A block sits *on* the next character, so it
//! reads as "this will be replaced" rather than "text goes here", and at the end of a line
//! there is no character to highlight at all — `line_runs` had to append a padding space
//! purely so the block had something to paint on.
//!
//! A caret between characters needs a pixel x, and a pixel x needs the shaped line. Only
//! the text system can shape, and it can only be reached during an element's layout/paint
//! pass. That is the whole reason this is an `Element` and not a `div`: **`Element` is the
//! only place in gpui where measurement happens**, which is the same reason IME needs one
//! (`window.handle_input` must be called during paint — see #18).
//!
//! # Why it measures rather than multiplying by a cell width
//!
//! `Fonts::advance_ratio` exists and the editor font is verified monospaced, so
//! `column * cell_width` would put the caret in the right place for ASCII and be ~10 lines
//! shorter. It is wrong for two reasons that are already load-bearing in this codebase:
//!
//! 1. **The font may not be the one that was asked for.** `crate::fonts` documents at
//!    length that a missing family does not error — it falls back to a *proportional* face
//!    and keeps rendering. Under that fallback an arithmetic caret drifts further from the
//!    text with every column, while a measured one stays correct. The click handler in
//!    `view.rs` already measures for exactly this reason.
//! 2. **Multibyte.** `column` is a *byte* offset into the line. `ação` is 6 bytes and 4
//!    characters, so byte-multiplied arithmetic puts the caret past the end of the word.
//!    Measuring asks the shaped line where byte `n` actually is, which is the same question
//!    `closest_index_for_x` answers in the other direction for clicks.
//!
//! `x_for_index` returning `LineLayout::width` for an index past the last glyph is what
//! makes end-of-line free: the caret lands after the final character with no padding space
//! and no special case.

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Style, Window, fill, px,
};

use crate::fonts::Fonts;

/// How much of a line may be shaped to place the caret.
///
/// The same guard, and the same number, as `MAX_MEASURE_BYTES` in `view.rs`: a minified
/// asset on one line must not make a *blinking* caret shape a megabyte of text twice a
/// second. Past this the caret pins to the cap rather than tracking the cursor, which is
/// wrong-but-bounded on a line no human is editing by eye.
const MAX_MEASURE_BYTES: usize = 4096;

/// Caret thickness. Two pixels rather than one: at 2x scale a 1px caret is a half-pixel
/// quad that the rasteriser can dim to near-invisible against a mid-tone background.
const CARET_WIDTH: Pixels = px(2.0);

/// A thin vertical bar at a byte offset within one rendered line.
///
/// Holds the line's text by value because `Element` methods run after the render closure
/// that built them has returned — borrowing the buffer would outlive the borrow.
pub struct Caret {
    /// The text of the row the caret is on, already clipped by the caller.
    line: String,
    /// Byte offset into `line` the caret sits *before*. Clamped and snapped to a character
    /// boundary here rather than trusted.
    byte: usize,
    color: gpui::Hsla,
    font: gpui::Font,
    font_size: Pixels,
    line_height: Pixels,
}

impl Caret {
    pub fn new(line: &str, byte: usize, color: gpui::Hsla, fonts: &Fonts) -> Self {
        // Truncate on a character boundary. Slicing a `String` mid-codepoint panics, and a
        // 4096-byte cap lands mid-codepoint exactly when the line is multibyte — which is
        // the case this whole module has to get right.
        let capped = fonts_safe_prefix(line, MAX_MEASURE_BYTES);
        Self {
            byte: snap_to_boundary(capped, byte.min(capped.len())),
            line: capped.to_string(),
            color,
            font: fonts.font(),
            font_size: fonts.size,
            line_height: fonts.line_height(),
        }
    }
}

/// The longest prefix of `text` that is at most `max` bytes and ends on a char boundary.
fn fonts_safe_prefix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Rounds `byte` down to a character boundary.
///
/// The cursor column comes from `Document`, which keeps it on a boundary — but it is a
/// *byte* offset arriving from another module, and `layout_line` indexes by byte. A
/// mid-codepoint index would panic in debug and silently misplace in release, which is the
/// failure mode the multibyte tests elsewhere in this codebase exist to catch.
fn snap_to_boundary(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

impl Element for Caret {
    type RequestLayoutState = ();
    /// The measured x, or `None` when there was nothing to measure.
    type PrepaintState = Option<Pixels>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        // Absolute, so the caret overlays the text run rather than displacing it. A caret
        // that took part in flex layout would push the line it marks one caret to the right.
        let mut style = Style::default();
        style.position = gpui::Position::Absolute;
        style.size.width = gpui::Length::Definite(gpui::DefiniteLength::Absolute(
            gpui::AbsoluteLength::Pixels(CARET_WIDTH),
        ));
        style.size.height = gpui::Length::Definite(gpui::DefiniteLength::Absolute(
            gpui::AbsoluteLength::Pixels(self.line_height),
        ));
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Option<Pixels> {
        // Column 0 is the overwhelmingly common case on an empty or freshly-opened line,
        // and shaping an empty run to be told the answer is 0 is wasted work on every
        // blink. This is the one shortcut taken here, and it is exact rather than an
        // approximation.
        if self.byte == 0 {
            return Some(px(0.0));
        }

        let run = gpui::TextRun {
            len: self.line.len(),
            font: self.font.clone(),
            // The caret is painted as a quad; these colours never reach the screen. The
            // shaping only needs the font and the byte lengths.
            color: gpui::white(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        Some(
            window
                .text_system()
                .layout_line(self.line.as_str(), self.font_size, &[run], None)
                .x_for_index(self.byte),
        )
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        x: &mut Option<Pixels>,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let Some(x) = *x else { return };
        window.paint_quad(fill(
            Bounds::new(
                gpui::point(bounds.origin.x + x, bounds.origin.y),
                gpui::size(CARET_WIDTH, self.line_height),
            ),
            self.color,
        ));
    }
}

impl IntoElement for Caret {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These test the offset arithmetic, which is the part that can actually be wrong
    // without a GPU. What they deliberately do NOT test is where the caret lands on screen:
    // gpui's headless text system is a fake perfect monospace (see the module docs on
    // `crate::render_tests`), so an assertion about a measured x would be an assertion
    // about `600.0 * len_utf16` and would pass just as happily with the geometry inverted.
    // Caret placement against real glyphs stays on issue #35's human list.

    #[test]
    fn a_prefix_cap_lands_on_a_character_boundary() {
        // 'ç' is two bytes, so a cap in the middle of one must round down rather than
        // slice a String mid-codepoint, which panics.
        let text = "aaç";
        assert_eq!(fonts_safe_prefix(text, 4), "aaç");
        assert_eq!(fonts_safe_prefix(text, 3), "aa", "must not split ç");
        assert_eq!(fonts_safe_prefix(text, 2), "aa");
        assert_eq!(fonts_safe_prefix("short", 4096), "short");
    }

    #[test]
    fn an_offset_inside_a_multibyte_character_snaps_down() {
        // "ação" — 'ç' occupies bytes 1..3. Byte 2 is inside it. layout_line indexes by
        // byte and a mid-codepoint index panics in a debug build.
        let text = "ação";
        assert_eq!(snap_to_boundary(text, 0), 0);
        assert_eq!(snap_to_boundary(text, 1), 1);
        assert_eq!(snap_to_boundary(text, 2), 1, "byte 2 is inside ç");
        assert_eq!(snap_to_boundary(text, 3), 3);
        for byte in 0..=text.len() {
            assert!(text.is_char_boundary(snap_to_boundary(text, byte)));
        }
    }

    #[test]
    fn an_offset_past_the_end_clamps_to_the_end() {
        // A cursor at end-of-line is the case the block cursor needed a padding space for.
        // Here it must simply clamp — x_for_index then returns the line width.
        let text = "let x";
        assert_eq!(snap_to_boundary(text, text.len()), text.len());
        assert_eq!(snap_to_boundary(text, 999), text.len());
        assert_eq!(snap_to_boundary("", 5), 0, "empty line, cursor at column 0");
    }
}
