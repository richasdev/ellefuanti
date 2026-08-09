//! Colours and metrics.
//!
//! One struct of values, not a theme engine. Loading themes from `assets/themes` is a
//! later milestone; what matters now is that no widget hardcodes a colour, so swapping
//! this out later touches one file.

use elle_syntax::HighlightStyle;
use elle_terminal::CellColor;
use gpui::{Hsla, Pixels, Rgba, px, rgb};

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

    /// The sixteen ANSI slots the terminal renders: 0-7 normal, 8-15 bright.
    ///
    /// An array rather than sixteen named fields because the terminal indexes it
    /// numerically — `Ansi(9)` comes straight off the wire — and naming each one would
    /// mean a sixteen-arm match that does nothing but arithmetic.
    pub ansi: [Hsla; 16],
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

    /// Height of the terminal panel, including its tab strip.
    /// ponytail: fixed, not draggable. A splitter needs a drag-handle element and a
    /// persisted layout; both arrive with the settings crate.
    pub const TERMINAL_HEIGHT: Pixels = px(260.0);
    /// Line height inside the terminal grid. Tighter than the editor's, which is what
    /// makes a terminal look like a terminal rather than a document.
    pub const TERMINAL_LINE_HEIGHT: Pixels = px(16.0);
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

            // Tuned to this theme rather than the hardware VT100 palette: a literal
            // 0x0000ff blue is unreadable on a dark background, and `ls` uses it for
            // directories on every Laravel project.
            ansi: [
                rgb(0x3b4048).into(), // 0 black — lifted off the background so it is visible
                rgb(0xff5c8a).into(), // 1 red
                rgb(0xa3d977).into(), // 2 green
                rgb(0xffb86c).into(), // 3 yellow
                rgb(0x82aaff).into(), // 4 blue
                rgb(0xc77dff).into(), // 5 magenta
                rgb(0x7dd3fc).into(), // 6 cyan
                rgb(0xd7dae0).into(), // 7 white
                rgb(0x5c6370).into(), // 8 bright black
                rgb(0xff8fab).into(), // 9 bright red
                rgb(0xbdea9a).into(), // 10 bright green
                rgb(0xffd29b).into(), // 11 bright yellow
                rgb(0xa8c4ff).into(), // 12 bright blue
                rgb(0xdcaaff).into(), // 13 bright magenta
                rgb(0xa9e4fd).into(), // 14 bright cyan
                rgb(0xffffff).into(), // 15 bright white
            ],
        }
    }

    /// Colour for a terminal cell.
    ///
    /// The terminal crate resolves the 256-colour cube and palette overrides itself and
    /// hands back either a literal RGB or a symbolic slot; only the symbolic ones reach
    /// the theme, which is what lets a theme restyle `ls` output without the parser
    /// knowing about themes.
    pub fn terminal(&self, color: CellColor) -> Hsla {
        match color {
            CellColor::Foreground => self.text,
            CellColor::Background => self.background,
            // Defensive index: the slot comes off the wire as a u8. A malformed SGR must
            // not panic the render (§24), so an out-of-range slot falls back to text.
            CellColor::Ansi(slot) => *self.ansi.get(slot as usize).unwrap_or(&self.text),
            CellColor::Rgb(r, g, b) => {
                Rgba { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
                    .into()
            }
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
