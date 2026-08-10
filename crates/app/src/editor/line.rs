//! One rendered editor row, painted rather than laid out.
//!
//! # Why this is an `Element` and `StyledText` was not
//!
//! Rows used to be `div().h(line_height).child(StyledText)`. That reads well and is wrong in
//! a way that took four attempts to pin down (#106, #107, #109 and this):
//!
//! `StyledText` resolves the height it lays text out at from **`window.text_style()` at
//! layout time**, not from the `div` wrapping it. Setting `.line_height()` on an ancestor
//! therefore does nothing for a row built inside a `uniform_list` callback — the callback
//! runs outside that style scope. Setting it per row does reach it, but only makes the two
//! numbers agree by accident, and they can still disagree: gpui *rounds* a text line height
//! (`line_height_in_pixels` ends in `.round()`) while a `div`'s `.h()` keeps the fraction.
//!
//! Painting removes the question. [`ShapedLine::paint`] takes the line height as an
//! **argument**, and gpui centres the glyphs inside it:
//!
//! ```text
//! let padding_top = (line_height - layout.ascent - layout.descent) / 2.;
//! ```
//!
//! One number, passed explicitly, used for both the row's box and its text. Nothing to keep
//! in sync and nothing to inherit.
//!
//! This is what Zed does — it builds on the same gpui, and its `EditorElement` places rows
//! arithmetically (`line_height * row`) and paints each with `ShapedLine::paint`, never
//! wrapping a row in a `div` (`crates/editor/src/element.rs`, `LineWithInvisibles::draw`).

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, LayoutId, Pixels,
    SharedString, Style, TextRun, Window,
};

/// A single row: its text, its colour runs, and the height it must occupy.
pub struct Line {
    text: SharedString,
    runs: Vec<TextRun>,
    font_size: Pixels,
    line_height: Pixels,
    /// Byte offset to draw a caret before, and its colour. `None` on every row but one.
    ///
    /// Painted *here* rather than as a sibling element, which is what an earlier attempt did
    /// and why the caret appeared below the text: an absolutely-positioned child with no
    /// inset is still placed by the flex pass, so it stacked under the line instead of over
    /// it. Zed paints cursors in their own pass with an explicit origin for the same reason
    /// (`EditorElement::paint_cursors`). Inside one element there is no layout to fight —
    /// the caret's x comes from the same shaped line the glyphs came from.
    caret: Option<(usize, gpui::Hsla)>,
    /// Forces a fixed advance per glyph. `Some` for the terminal grid, `None` for text.
    cell_width: Option<Pixels>,
}

impl Line {
    pub fn new(
        text: impl Into<SharedString>,
        runs: Vec<TextRun>,
        font_size: Pixels,
        line_height: Pixels,
    ) -> Self {
        Self { text: text.into(), runs, font_size, line_height, caret: None, cell_width: None }
    }

    /// Forces every glyph to one cell wide, for the terminal grid.
    ///
    /// Without it a font whose glyphs advance slightly differently drifts across a long
    /// line, and a grid that drifts is not a grid. The editor leaves it unset: proportional
    /// fallback there should look like text, not like a table.
    pub fn with_cell_width(mut self, width: Pixels) -> Self {
        self.cell_width = Some(width);
        self
    }

    /// Draws a caret before `byte`, snapped to a character boundary.
    pub fn with_caret(mut self, byte: usize, color: gpui::Hsla) -> Self {
        let mut byte = byte.min(self.text.len());
        while byte > 0 && !self.text.is_char_boundary(byte) {
            byte -= 1;
        }
        self.caret = Some((byte, color));
        self
    }
}

impl Element for Line {
    type RequestLayoutState = ();
    /// The shaped line, kept from prepaint so paint does not shape twice.
    type PrepaintState = Option<gpui::ShapedLine>;

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
        // The row occupies exactly the height it will paint text at. Width is the full line
        // so the row fills its container and mouse hit-testing covers the whole width.
        let mut style = Style::default();
        style.size.height = gpui::Length::Definite(gpui::DefiniteLength::Absolute(
            gpui::AbsoluteLength::Pixels(self.line_height),
        ));
        style.size.width = gpui::Length::Definite(gpui::DefiniteLength::Fraction(1.0));
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) -> Option<gpui::ShapedLine> {
        // Shaped here rather than in paint: prepaint is where measurement belongs, and a
        // shaped line is what both the width and the glyph positions come from.
        Some(window.text_system().shape_line(
            self.text.clone(),
            self.font_size,
            &self.runs,
            self.cell_width,
        ))
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        shaped: &mut Option<gpui::ShapedLine>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(shaped) = shaped else { return };
        // Backgrounds first, and as a separate call: `ShapedLine::paint` draws glyphs only.
        // Every background a run carries — the terminal cursor, a selection, a search match,
        // a diagnostic — lives in `decoration_runs` and is drawn by `paint_background`.
        //
        // Missing this is why the terminal cursor vanished after rows moved to this element:
        // the cursor is an *inverted cell*, so its glyph is painted in the background colour
        // and is only visible against the cursor's own background. With that background
        // never drawn, the character under the cursor read as a hole — which is what "não
        // vejo cursor nenhum, só a lacuna" was.
        let _ = shaped.paint_background(bounds.origin, self.line_height, window, cx);

        // `line_height` is passed, not inherited. gpui centres the glyphs within it, so the
        // text sits in the middle of exactly the box `request_layout` asked for.
        let _ = shaped.paint(bounds.origin, self.line_height, window, cx);

        // After the text, so the caret is over it rather than under. `x_for_index` measures
        // the same shaped line, so the caret lands on a real glyph boundary even on a
        // proportional fallback font — which is the case arithmetic on a column index gets
        // wrong and is why this is measured rather than computed.
        if let Some((byte, color)) = self.caret {
            let x = shaped.x_for_index(byte);
            window.paint_quad(gpui::fill(
                Bounds::new(
                    gpui::point(bounds.origin.x + x, bounds.origin.y),
                    gpui::size(gpui::px(2.0), self.line_height),
                ),
                color,
            ));
        }
    }
}

impl gpui::IntoElement for Line {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
