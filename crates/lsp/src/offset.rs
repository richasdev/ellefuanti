//! Byte offsets ↔ LSP positions.
//!
//! This module exists because the two sides of the boundary count differently:
//!
//! - **This codebase counts bytes.** `elle-text` is bytes-only by design (ADR-0003), and
//!   so are tree-sitter and gpui's layout.
//! - **LSP counts UTF-16 code units by default**, per the specification's `Position`.
//!
//! For ASCII the two agree, which is exactly what makes this dangerous: a conversion
//! that simply uses the byte column passes every test written in English and corrupts
//! the first file containing `ação`. In a Portuguese-language Laravel codebase that is
//! most files, so the conversion is explicit, centralised here, and tested against
//! multibyte fixtures rather than trusted.
//!
//! Three encodings are possible, and the server chooses during capability negotiation
//! (`positionEncoding`, LSP 3.17). We advertise all three and honour whatever comes
//! back, so a server that supports UTF-8 gets the cheap path and one that does not
//! still gets correct positions.
//!
//! | encoding | column counts             | `é` (2 bytes) | `😀` (4 bytes) |
//! | -------- | ------------------------- | ------------- | -------------- |
//! | UTF-8    | bytes                     | 2             | 4              |
//! | UTF-16   | UTF-16 code units         | 1             | 2 (surrogates) |
//! | UTF-32   | Unicode scalar values     | 1             | 1              |

use lsp_types::{Position, PositionEncodingKind, Range};

/// How a server wants `Position.character` counted.
///
/// Defaults to UTF-16: a server that omits `positionEncoding` is a pre-3.17 server, and
/// the specification's original wording made UTF-16 the only option. Guessing UTF-8
/// here would silently corrupt positions against every older server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OffsetEncoding {
    Utf8,
    #[default]
    Utf16,
    Utf32,
}

impl OffsetEncoding {
    /// Reads the encoding a server selected in its `InitializeResult`.
    ///
    /// An unrecognised value falls back to UTF-16 rather than erroring: an unknown
    /// encoding is a server we cannot position correctly against, and the specified
    /// default is the safest of the three guesses.
    pub fn from_capability(kind: Option<&PositionEncodingKind>) -> Self {
        match kind.map(|k| k.as_str()) {
            Some("utf-8") => Self::Utf8,
            Some("utf-32") => Self::Utf32,
            _ => Self::Utf16,
        }
    }

    pub fn as_kind(self) -> PositionEncodingKind {
        match self {
            Self::Utf8 => PositionEncodingKind::UTF8,
            Self::Utf16 => PositionEncodingKind::UTF16,
            Self::Utf32 => PositionEncodingKind::UTF32,
        }
    }

    /// Width of one character in this encoding's units.
    fn width(self, ch: char) -> usize {
        match self {
            Self::Utf8 => ch.len_utf8(),
            Self::Utf16 => ch.len_utf16(),
            Self::Utf32 => 1,
        }
    }
}

/// Line-start byte offsets for a document, so position conversion is a binary search
/// rather than a scan from byte zero.
///
/// Rebuilt per notification rather than incrementally maintained. A `didChange` already
/// costs a rope splice and a reparse; one more linear pass over the text is not what
/// makes typing slow, and an incrementally-patched index that drifts out of sync
/// produces exactly the silent corruption this module exists to prevent.
///
/// ponytail: if profiling ever shows this pass mattering on large files, the upgrade is
/// to shift the line table by the edit delta instead of rebuilding — correct but
/// fiddlier, and not worth the bug surface until a measurement demands it.
#[derive(Clone, Debug)]
pub struct LineIndex {
    /// Byte offset of the start of each line. Always begins with 0.
    line_starts: Vec<usize>,
    len_bytes: usize,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self { line_starts, len_bytes: text.len() }
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Byte offset where `line` starts, clamped to the end of the document.
    fn line_start(&self, line: usize) -> usize {
        self.line_starts.get(line).copied().unwrap_or(self.len_bytes)
    }

    /// Byte range of `line` including its trailing newline, if any.
    fn line_range(&self, line: usize) -> std::ops::Range<usize> {
        let start = self.line_start(line);
        let end = self.line_starts.get(line + 1).copied().unwrap_or(self.len_bytes);
        start..end
    }

    /// Converts a byte offset into an LSP position.
    ///
    /// Offsets past the end of the document clamp to the end, and an offset landing
    /// mid-codepoint snaps **down** to the start of that character — the same rule
    /// `elle-text` applies, so the two layers never disagree about where a character
    /// begins.
    pub fn position(&self, text: &str, offset: usize, encoding: OffsetEncoding) -> Position {
        // Clamp against `text`, not just the cached `len_bytes`. The index and the text are
        // separate parameters, so nothing prevents a caller from passing text that has since
        // shrunk — a server replying about a document version the buffer has already moved
        // past. Trusting the cached length there indexes out of range and panics.
        let offset = snap_down(text, offset.min(self.len_bytes).min(text.len()));
        let line = self.line_of(offset);
        let line_start = self.line_start(line).min(offset);

        // Count only within the line, so cost is proportional to the column rather
        // than to the file.
        let character = text[line_start..offset].chars().map(|c| encoding.width(c)).sum::<usize>();

        Position { line: line as u32, character: character as u32 }
    }

    /// Converts an LSP position into a byte offset.
    ///
    /// A `character` past the end of the line clamps to the line's end rather than
    /// spilling onto the next one. Servers do send such positions — notably as the
    /// exclusive end of a whole-line range — and spilling would silently shift an edit
    /// onto the following line.
    pub fn offset(&self, text: &str, position: Position, encoding: OffsetEncoding) -> usize {
        let line = position.line as usize;
        if line >= self.line_starts.len() {
            // `len_bytes` is the index's idea of the end, which may exceed the text it is
            // being applied to. Return an offset that is valid for `text`.
            return snap_down(text, self.len_bytes.min(text.len()));
        }

        // Clamp to `text` for the same reason as `position`: the index may describe a
        // longer, older version of the document, and slicing on its word would panic.
        let mut range = self.line_range(line);
        range.start = snap_down(text, range.start.min(text.len()));
        range.end = snap_down(text, range.end.min(text.len())).max(range.start);

        // Exclude the line terminator: a column may not address it, and counting it
        // would let a large `character` walk into the next line.
        let line_text = trim_terminator(&text[range.clone()]);

        let mut remaining = position.character as usize;
        let mut offset = range.start;
        for ch in line_text.chars() {
            if remaining == 0 {
                return offset;
            }
            let width = encoding.width(ch);
            // A position landing *inside* a character (possible in UTF-16 when a client
            // addresses the low surrogate of an emoji) snaps to that character's start,
            // never to a byte that would split it.
            if remaining < width {
                return offset;
            }
            remaining -= width;
            offset += ch.len_utf8();
        }
        offset
    }

    pub fn range(
        &self,
        text: &str,
        range: std::ops::Range<usize>,
        encoding: OffsetEncoding,
    ) -> Range {
        Range {
            start: self.position(text, range.start, encoding),
            end: self.position(text, range.end, encoding),
        }
    }

    pub fn byte_range(
        &self,
        text: &str,
        range: Range,
        encoding: OffsetEncoding,
    ) -> std::ops::Range<usize> {
        let start = self.offset(text, range.start, encoding);
        let end = self.offset(text, range.end, encoding);
        // Servers occasionally emit inverted ranges; normalising beats slicing panics.
        if start <= end { start..end } else { end..start }
    }

    /// Index of the line containing `offset`, by binary search over line starts.
    fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(line) => line,
            // `Err(i)` is the insertion point, so the offset sits inside line `i - 1`.
            Err(insertion) => insertion - 1,
        }
    }
}

/// Snaps a byte offset down to the nearest UTF-8 character boundary.
fn snap_down(text: &str, mut offset: usize) -> usize {
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Strips a trailing `\n` or `\r\n` from a line slice.
fn trim_terminator(line: &str) -> &str {
    line.strip_suffix('\n').map_or(line, |l| l.strip_suffix('\r').unwrap_or(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fixture here is deliberately non-ASCII. An ASCII test cannot distinguish
    /// the three encodings, so it proves nothing about the conversion.
    const PT: &str = "<?php\n// ação e coração\n$ç = 'não';\n";

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn ascii_offsets_are_identical_in_every_encoding() {
        let text = "<?php\n$x = 1;\n";
        let index = LineIndex::new(text);
        for enc in [OffsetEncoding::Utf8, OffsetEncoding::Utf16, OffsetEncoding::Utf32] {
            assert_eq!(index.position(text, 8, enc), pos(1, 2), "{enc:?}");
            assert_eq!(index.offset(text, pos(1, 2), enc), 8, "{enc:?}");
        }
    }

    #[test]
    fn two_byte_characters_count_differently_per_encoding() {
        // Line 1 is "// ação e coração"; 'ç' and 'ã' are two bytes each in UTF-8 but
        // one code unit each in UTF-16 and UTF-32.
        let index = LineIndex::new(PT);
        let line_start = PT.find("//").unwrap();
        // Byte offset just past "// ação" — 7 characters, 9 bytes.
        let offset = line_start + "// ação".len();
        assert_eq!(&PT[line_start..offset], "// ação");

        assert_eq!(index.position(PT, offset, OffsetEncoding::Utf8), pos(1, 9));
        assert_eq!(index.position(PT, offset, OffsetEncoding::Utf16), pos(1, 7));
        assert_eq!(index.position(PT, offset, OffsetEncoding::Utf32), pos(1, 7));
    }

    #[test]
    fn round_trips_through_every_encoding_at_every_boundary() {
        let index = LineIndex::new(PT);
        for enc in [OffsetEncoding::Utf8, OffsetEncoding::Utf16, OffsetEncoding::Utf32] {
            for (offset, _) in PT.char_indices() {
                let position = index.position(PT, offset, enc);
                assert_eq!(
                    index.offset(PT, position, enc),
                    offset,
                    "{enc:?} failed to round-trip byte {offset}"
                );
            }
        }
    }

    #[test]
    fn astral_characters_are_surrogate_pairs_in_utf16_only() {
        // U+1F600 GRINNING FACE: 4 bytes UTF-8, 2 UTF-16 code units, 1 scalar value.
        let text = "a😀b";
        let index = LineIndex::new(text);
        let after_emoji = 5;

        assert_eq!(index.position(text, after_emoji, OffsetEncoding::Utf8), pos(0, 5));
        assert_eq!(index.position(text, after_emoji, OffsetEncoding::Utf16), pos(0, 3));
        assert_eq!(index.position(text, after_emoji, OffsetEncoding::Utf32), pos(0, 2));
    }

    #[test]
    fn a_position_inside_a_surrogate_pair_snaps_to_the_character_start() {
        // Character 2 in UTF-16 addresses the *low surrogate* of the emoji — a position
        // that cannot map to a byte without splitting the codepoint.
        let text = "a😀b";
        let index = LineIndex::new(text);
        assert_eq!(index.offset(text, pos(0, 2), OffsetEncoding::Utf16), 1);
        // And the byte it returns is a real char boundary.
        assert!(text.is_char_boundary(index.offset(text, pos(0, 2), OffsetEncoding::Utf16)));
    }

    #[test]
    fn mid_codepoint_byte_offsets_snap_down() {
        let text = "ção";
        let index = LineIndex::new(text);
        // Byte 1 is the second byte of 'ç'.
        assert_eq!(index.position(text, 1, OffsetEncoding::Utf16), pos(0, 0));
        assert_eq!(index.position(text, 2, OffsetEncoding::Utf16), pos(0, 1));
    }

    #[test]
    fn columns_past_the_end_of_a_line_clamp_to_that_line() {
        let index = LineIndex::new(PT);
        // Line 1 is "// ação e coração"; ask for a wildly out-of-range column.
        let offset = index.offset(PT, pos(1, 9999), OffsetEncoding::Utf16);
        // Must land at the line's end, before the newline — not on line 2.
        assert_eq!(&PT[offset..offset + 1], "\n");
    }

    #[test]
    fn positions_past_the_document_clamp_to_its_end() {
        let index = LineIndex::new(PT);
        assert_eq!(index.offset(PT, pos(9999, 0), OffsetEncoding::Utf16), PT.len());
        assert_eq!(
            index.position(PT, PT.len() + 100, OffsetEncoding::Utf16),
            index.position(PT, PT.len(), OffsetEncoding::Utf16)
        );
    }

    #[test]
    fn carriage_returns_are_not_addressable_columns() {
        let text = "a\r\nb";
        let index = LineIndex::new(text);
        // Column 5 on line 0 clamps to before the \r, not into the line terminator.
        assert_eq!(index.offset(text, pos(0, 5), OffsetEncoding::Utf16), 1);
    }

    #[test]
    fn line_of_finds_the_right_line_for_every_byte() {
        let index = LineIndex::new(PT);
        assert_eq!(index.line_count(), 4);
        assert_eq!(index.position(PT, 0, OffsetEncoding::Utf16).line, 0);
        assert_eq!(index.position(PT, 6, OffsetEncoding::Utf16), pos(1, 0));
        // Start of line 2 ("$ç = 'não';").
        let line2 = PT.find("$\u{e7}").unwrap();
        assert_eq!(index.position(PT, line2, OffsetEncoding::Utf16), pos(2, 0));
    }

    #[test]
    fn a_stale_index_against_shorter_text_does_not_panic() {
        // `LineIndex::new` caches line starts and a length, but `position`/`offset` take
        // `text` as a separate parameter — so nothing structurally prevents a caller from
        // passing text that has since shrunk. A document edited between building the index
        // and converting a position (a server replying about a version the buffer has moved
        // past) hits exactly this.
        //
        // Before the clamp against `text.len()`, this panicked on an out-of-range slice.
        let index = LineIndex::new("line one\nline two\nline three\n");
        let shrunk = "short";

        // Offsets valid for the old text but past the end of the new one.
        for offset in [6, 12, 25, 999] {
            let position = index.position(shrunk, offset, OffsetEncoding::Utf16);
            // The answer is necessarily approximate — the index is stale — but it must be
            // in range and must not crash.
            assert!(position.line as usize <= index.line_count());
        }

        // The reverse direction too: a position from the old document against new text.
        for (line, character) in [(0, 99), (2, 0), (99, 0)] {
            let offset = index.offset(shrunk, pos(line, character), OffsetEncoding::Utf16);
            assert!(
                offset <= shrunk.len(),
                "offset {offset} escapes text of length {}",
                shrunk.len()
            );
        }
    }

    #[test]
    fn empty_document_is_position_zero() {
        let index = LineIndex::new("");
        assert_eq!(index.position("", 0, OffsetEncoding::Utf16), pos(0, 0));
        assert_eq!(index.offset("", pos(0, 0), OffsetEncoding::Utf16), 0);
        assert_eq!(index.offset("", pos(50, 50), OffsetEncoding::Utf16), 0);
    }

    #[test]
    fn encoding_negotiation_defaults_to_utf16() {
        assert_eq!(OffsetEncoding::from_capability(None), OffsetEncoding::Utf16);
        assert_eq!(
            OffsetEncoding::from_capability(Some(&PositionEncodingKind::UTF8)),
            OffsetEncoding::Utf8
        );
        assert_eq!(
            OffsetEncoding::from_capability(Some(&PositionEncodingKind::UTF32)),
            OffsetEncoding::Utf32
        );
        // An encoding we do not know must not be guessed at as UTF-8.
        assert_eq!(
            OffsetEncoding::from_capability(Some(&PositionEncodingKind::new("utf-7"))),
            OffsetEncoding::Utf16
        );
    }

    #[test]
    fn byte_range_normalises_inverted_ranges() {
        let index = LineIndex::new(PT);
        let inverted = Range { start: pos(1, 5), end: pos(1, 2) };
        let range = index.byte_range(PT, inverted, OffsetEncoding::Utf16);
        assert!(range.start <= range.end);
    }
}
