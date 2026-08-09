//! The plain-data view of a terminal screen that the UI renders.
//!
//! The renderer must not hold alacritty's `Term` while it draws: the reader thread wants
//! that lock to keep draining the PTY, and a frame that blocks on it stalls the window.
//! So a frame takes a [`GridSnapshot`] — an owned copy of just the visible cells — and the
//! lock is released before any element is built.
//!
//! Colours are resolved here rather than in the app crate because resolution needs
//! alacritty's palette (OSC-4 overrides, the 256-colour cube, the dim/bright variants),
//! and that palette lives behind the `Term`. The app receives plain RGB and maps only the
//! sixteen *named* slots onto the theme.

/// A colour as the UI receives it: either a literal RGB the program asked for, or one of
/// the sixteen ANSI slots the theme is entitled to restyle.
///
/// Keeping the named case symbolic is what lets the theme own its own palette. Resolving
/// `NamedColor::Red` to an RGB down here would hardcode a red the theme cannot override.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CellColor {
    /// Default foreground/background — the theme's own text and panel colours.
    Foreground,
    Background,
    /// One of the sixteen ANSI slots, as an index 0..16 (0-7 normal, 8-15 bright).
    Ansi(u8),
    /// A literal colour: a 24-bit SGR, a 256-colour cube entry, or a palette override.
    Rgb(u8, u8, u8),
}

/// One rendered cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub c: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// True for the second half of a double-width glyph. The renderer skips these: the
    /// wide char in the preceding cell already covers the space, and emitting a spacer
    /// character would push the rest of the row one column right.
    pub wide_spacer: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: CellColor::Foreground,
            bg: CellColor::Background,
            bold: false,
            italic: false,
            underline: false,
            wide_spacer: false,
        }
    }
}

/// Where the cursor is, in viewport coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CursorPos {
    pub line: usize,
    pub column: usize,
}

/// An owned copy of the visible screen at one instant.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GridSnapshot {
    pub lines: Vec<Vec<Cell>>,
    pub columns: usize,
    /// None while the viewport is scrolled back, where there is no live cursor to draw.
    pub cursor: Option<CursorPos>,
    /// How many lines above the viewport exist in scrollback. Drives the scrollbar and
    /// tells the UI whether scrolling up is possible at all.
    pub history_size: usize,
    /// Lines the viewport is scrolled up by; 0 means pinned to the live bottom.
    pub display_offset: usize,
}

impl GridSnapshot {
    /// An empty screen. Rendered before the first read lands, and after a spawn failure,
    /// so the panel always has something well-formed to draw.
    pub fn empty(columns: usize, lines: usize) -> Self {
        Self {
            lines: vec![vec![Cell::default(); columns]; lines],
            columns,
            cursor: None,
            history_size: 0,
            display_offset: 0,
        }
    }

    /// The screen as plain text, one string per line with trailing blanks trimmed.
    ///
    /// This is the seam the tests assert through: comparing text is stable across the
    /// attribute and colour details a shell may or may not emit, so a test for "did
    /// `echo hello` produce hello" does not break when a prompt changes its styling.
    pub fn to_text(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|line| {
                let mut text: String =
                    line.iter().filter(|cell| !cell.wide_spacer).map(|cell| cell.c).collect();
                text.truncate(text.trim_end().len());
                text
            })
            .collect()
    }

    /// The whole screen as one newline-joined string, with trailing blank lines removed.
    /// Convenience for `contains` assertions in tests and for error reporting.
    pub fn to_string_trimmed(&self) -> String {
        let mut lines = self.to_text();
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_snapshot_has_the_requested_shape() {
        let snapshot = GridSnapshot::empty(80, 24);
        assert_eq!(snapshot.lines.len(), 24);
        assert_eq!(snapshot.lines[0].len(), 80);
        assert_eq!(snapshot.columns, 80);
        assert!(snapshot.cursor.is_none());
    }

    #[test]
    fn to_text_trims_trailing_blanks_but_keeps_interior_spaces() {
        let mut snapshot = GridSnapshot::empty(8, 2);
        for (i, c) in "a b".chars().enumerate() {
            snapshot.lines[0][i].c = c;
        }
        assert_eq!(snapshot.to_text()[0], "a b");
        assert_eq!(snapshot.to_text()[1], "");
    }

    #[test]
    fn to_string_trimmed_drops_trailing_blank_lines() {
        let mut snapshot = GridSnapshot::empty(4, 5);
        snapshot.lines[0][0].c = 'x';
        assert_eq!(snapshot.to_string_trimmed(), "x");
    }

    #[test]
    fn wide_spacers_are_not_emitted_as_characters() {
        let mut snapshot = GridSnapshot::empty(4, 1);
        snapshot.lines[0][0].c = '漢';
        snapshot.lines[0][1].wide_spacer = true;
        // The spacer cell would otherwise contribute a stray space after the glyph.
        assert_eq!(snapshot.to_text()[0], "漢");
    }
}
