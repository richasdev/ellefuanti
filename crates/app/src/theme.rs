//! Colours and metrics.
//!
//! One struct of values, not a theme engine. Loading themes from `assets/themes` is a
//! later milestone; what matters now is that no widget hardcodes a colour, so swapping
//! this out later touches one file.

use elle_syntax::HighlightStyle;
use gpui::{Hsla, Pixels, px, rgb};

pub struct Theme {
    pub background: Hsla,
    pub panel: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
    pub status_bar: Hsla,

    pub keyword: Hsla,
    pub type_name: Hsla,
    pub function: Hsla,
    pub variable: Hsla,
    pub string: Hsla,
    pub number: Hsla,
    pub comment: Hsla,
    pub tag: Hsla,
    pub blade: Hsla,
}

/// Editor metrics. Line height is derived from font size so zoom stays consistent.
pub struct Metrics;

impl Metrics {
    pub const FONT_SIZE: Pixels = px(13.0);
    pub const UI_FONT_SIZE: Pixels = px(12.0);
    pub const LINE_HEIGHT: Pixels = px(20.0);
    pub const SIDEBAR_WIDTH: Pixels = px(240.0);
    pub const GUTTER_WIDTH: Pixels = px(52.0);
    pub const TAB_HEIGHT: Pixels = px(32.0);
    pub const STATUS_HEIGHT: Pixels = px(24.0);
    pub const ROW_HEIGHT: Pixels = px(22.0);
}

impl Theme {
    /// The default dark theme.
    pub fn dark() -> Self {
        Self {
            background: rgb(0x16171d).into(),
            panel: rgb(0x1b1d24).into(),
            border: rgb(0x2a2d36).into(),
            text: rgb(0xd7dae0).into(),
            text_muted: rgb(0x767c8a).into(),
            accent: rgb(0xff5c8a).into(),
            hover: rgb(0x24272f).into(),
            selected: rgb(0x2d313c).into(),
            cursor: rgb(0xff5c8a).into(),
            selection: rgb(0x33405c).into(),
            status_bar: rgb(0x1b1d24).into(),

            keyword: rgb(0xc77dff).into(),
            type_name: rgb(0x7dd3fc).into(),
            function: rgb(0x82aaff).into(),
            variable: rgb(0xf0a6c8).into(),
            string: rgb(0xa3d977).into(),
            number: rgb(0xffb86c).into(),
            comment: rgb(0x5c6370).into(),
            tag: rgb(0x8b93a5).into(),
            blade: rgb(0xff9e64).into(),
        }
    }

    /// Colour for a syntax style. Highlighting never names a colour itself, so a theme
    /// change needs no reparse.
    pub fn syntax(&self, style: HighlightStyle) -> Hsla {
        match style {
            HighlightStyle::Keyword => self.keyword,
            HighlightStyle::Type => self.type_name,
            HighlightStyle::Function => self.function,
            HighlightStyle::Variable => self.variable,
            HighlightStyle::String => self.string,
            HighlightStyle::Number => self.number,
            HighlightStyle::Comment => self.comment,
            HighlightStyle::Tag => self.tag,
            HighlightStyle::BladeDirective => self.blade,
        }
    }
}
