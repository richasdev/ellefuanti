//! Tracking the documents a server has been told about.
//!
//! The server keeps its own copy of every open file, rebuilt from the `didChange`
//! notifications we send. If our idea of the text and the server's ever diverge, every
//! position we exchange afterwards refers to a different document than the user is
//! looking at — completions land in the wrong place, diagnostics underline the wrong
//! span, and nothing reports an error. So this module's real job is keeping the two
//! copies byte-identical.
//!
//! That is why [`TrackedDocument`] holds the text at all: not because the client needs
//! it, but because converting a byte range into an LSP position requires knowing the
//! text *as it was before the edit*, and because it makes the divergence testable.

use lsp_types::{
    Range, TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem, Uri,
    VersionedTextDocumentIdentifier,
};

use elle_text::Edit;

use crate::offset::{LineIndex, OffsetEncoding};

/// How a server wants document changes delivered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncKind {
    /// The server does not want `didChange` at all.
    None,
    /// The whole document, every time.
    Full,
    /// Only what changed. What we prefer, and what `Edit` already describes exactly.
    #[default]
    Incremental,
}

/// One file the server has been told about.
#[derive(Clone, Debug)]
pub struct TrackedDocument {
    pub uri: Uri,
    pub language_id: String,
    /// Incremented on every change. The server rejects out-of-order versions, and a
    /// mismatch is the first symptom of the two copies diverging.
    pub version: i32,
    /// Our copy of what the server should now hold.
    text: String,
    index: LineIndex,
}

impl TrackedDocument {
    pub fn new(uri: Uri, language_id: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        let index = LineIndex::new(&text);
        Self { uri, language_id: language_id.into(), version: 1, text, index }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn line_index(&self) -> &LineIndex {
        &self.index
    }

    pub fn identifier(&self) -> TextDocumentIdentifier {
        TextDocumentIdentifier { uri: self.uri.clone() }
    }

    pub fn versioned_identifier(&self) -> VersionedTextDocumentIdentifier {
        VersionedTextDocumentIdentifier { uri: self.uri.clone(), version: self.version }
    }

    pub fn item(&self) -> TextDocumentItem {
        TextDocumentItem {
            uri: self.uri.clone(),
            language_id: self.language_id.clone(),
            version: self.version,
            text: self.text.clone(),
        }
    }

    /// Converts a byte offset in this document into an LSP position.
    pub fn position(&self, offset: usize, encoding: OffsetEncoding) -> lsp_types::Position {
        self.index.position(&self.text, offset, encoding)
    }

    /// Converts an LSP position back into a byte offset.
    pub fn offset(&self, position: lsp_types::Position, encoding: OffsetEncoding) -> usize {
        self.index.offset(&self.text, position, encoding)
    }

    pub fn byte_range(&self, range: Range, encoding: OffsetEncoding) -> std::ops::Range<usize> {
        self.index.byte_range(&self.text, range, encoding)
    }

    /// Applies an edit locally and returns the change events to send.
    ///
    /// The position conversion happens against the text *before* the splice, because
    /// `Edit::range` is in pre-edit coordinates — that ordering is the single easiest
    /// thing to get backwards here, and getting it backwards corrupts the server's copy
    /// silently rather than loudly.
    pub fn apply(
        &mut self,
        edit: &Edit,
        sync: SyncKind,
        encoding: OffsetEncoding,
    ) -> Vec<TextDocumentContentChangeEvent> {
        // Clamped and snapped so a caller's stale offset cannot slice mid-codepoint and
        // panic. Matches `elle-text`'s own rule: snap down to a char boundary.
        let start = snap_down(&self.text, edit.range.start.min(self.text.len()));
        let end = snap_down(&self.text, edit.range.end.min(self.text.len()));
        let (start, end) = if start <= end { (start, end) } else { (end, start) };

        let range = self.index.range(&self.text, start..end, encoding);

        self.text.replace_range(start..end, &edit.new_text);
        self.index = LineIndex::new(&self.text);
        self.version += 1;

        match sync {
            SyncKind::None => Vec::new(),
            SyncKind::Full => vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: self.text.clone(),
            }],
            SyncKind::Incremental => vec![TextDocumentContentChangeEvent {
                range: Some(range),
                // Deprecated in the specification and ignored by servers that honour
                // `range`; omitted rather than computed wrongly in UTF-16 units.
                range_length: None,
                text: edit.new_text.to_string(),
            }],
        }
    }

    /// Replaces the whole text, for a reload from disk.
    pub fn reset(&mut self, text: impl Into<String>) -> Vec<TextDocumentContentChangeEvent> {
        self.text = text.into();
        self.index = LineIndex::new(&self.text);
        self.version += 1;
        vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: self.text.clone(),
        }]
    }
}

fn snap_down(text: &str, mut offset: usize) -> usize {
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn uri() -> Uri {
        "file:///srv/app/Model.php".parse().unwrap()
    }

    fn doc(text: &str) -> TrackedDocument {
        TrackedDocument::new(uri(), "php", text)
    }

    fn edit(range: std::ops::Range<usize>, new_text: &str, old_text: &str) -> Edit {
        Edit::new(range, new_text, Arc::from(old_text))
    }

    /// Replays the change events the way a server would, so a divergence between our
    /// copy and the server's shows up as a failing assertion rather than as mysterious
    /// off-by-one completions weeks later.
    fn replay(
        original: &str,
        events: &[TextDocumentContentChangeEvent],
        encoding: OffsetEncoding,
    ) -> String {
        let mut text = original.to_string();
        for event in events {
            match event.range {
                None => text = event.text.clone(),
                Some(range) => {
                    let index = LineIndex::new(&text);
                    let byte_range = index.byte_range(&text, range, encoding);
                    text.replace_range(byte_range, &event.text);
                }
            }
        }
        text
    }

    #[test]
    fn open_carries_version_one_and_the_full_text() {
        let document = doc("<?php\n");
        let item = document.item();
        assert_eq!(item.version, 1);
        assert_eq!(item.text, "<?php\n");
        assert_eq!(item.language_id, "php");
    }

    #[test]
    fn an_insert_becomes_an_empty_range_change() {
        let mut document = doc("<?php\n");
        let events = document.apply(
            &edit(6..6, "$x = 1;", ""),
            SyncKind::Incremental,
            OffsetEncoding::Utf16,
        );

        assert_eq!(events.len(), 1);
        let range = events[0].range.unwrap();
        assert_eq!(range.start, range.end, "an insert must be a zero-width range");
        assert_eq!(events[0].text, "$x = 1;");
        assert_eq!(document.text(), "<?php\n$x = 1;");
        assert_eq!(document.version, 2);
    }

    #[test]
    fn a_delete_becomes_an_empty_text_change() {
        let mut document = doc("<?php\n$x = 1;");
        let events = document.apply(
            &edit(6..13, "", "$x = 1;"),
            SyncKind::Incremental,
            OffsetEncoding::Utf16,
        );
        assert_eq!(events[0].text, "");
        assert_eq!(document.text(), "<?php\n");
    }

    #[test]
    fn incremental_changes_reproduce_the_document_exactly() {
        // The property that matters: replaying our events onto the original text must
        // give the same string we hold. Anything else is silent divergence.
        let original = "<?php\n// ação\n$coração = 'não';\n";
        let mut document = doc(original);

        let edits = [edit(6..6, "// olá 😀\n", ""), edit(0..5, "<?PHP", "<?php")];

        let mut all_events = Vec::new();
        for e in &edits {
            all_events.extend(document.apply(e, SyncKind::Incremental, OffsetEncoding::Utf16));
        }

        assert_eq!(replay(original, &all_events, OffsetEncoding::Utf16), document.text());
    }

    #[test]
    fn incremental_changes_stay_exact_in_every_encoding() {
        let original = "função() {\n  return 'ção';\n}\n";
        for encoding in [OffsetEncoding::Utf8, OffsetEncoding::Utf16, OffsetEncoding::Utf32] {
            let mut document = doc(original);
            // An edit whose start sits *after* a multibyte character, so a wrong
            // encoding shifts the range and corrupts the replay.
            let events =
                document.apply(&edit(12..18, "yield", "return"), SyncKind::Incremental, encoding);
            assert_eq!(
                replay(original, &events, encoding),
                document.text(),
                "divergence under {encoding:?}"
            );
        }
    }

    #[test]
    fn an_edit_after_an_emoji_maps_to_the_right_position() {
        // Regression shape for the classic UTF-16 bug: everything after an astral
        // character is off by one per emoji if surrogate pairs are miscounted.
        let original = "// 😀 fim\nx";
        let mut document = doc(original);
        let offset = original.find('x').unwrap();
        let events = document.apply(
            &edit(offset..offset + 1, "y", "x"),
            SyncKind::Incremental,
            OffsetEncoding::Utf16,
        );

        assert_eq!(events[0].range.unwrap().start, lsp_types::Position { line: 1, character: 0 });
        assert_eq!(document.text(), "// 😀 fim\ny");
    }

    #[test]
    fn full_sync_sends_the_whole_document() {
        let mut document = doc("<?php\n");
        let events = document.apply(&edit(6..6, "x", ""), SyncKind::Full, OffsetEncoding::Utf16);
        assert_eq!(events.len(), 1);
        assert!(events[0].range.is_none(), "full sync must not carry a range");
        assert_eq!(events[0].text, "<?php\nx");
    }

    #[test]
    fn no_sync_sends_nothing_but_still_tracks_locally() {
        let mut document = doc("<?php\n");
        let events = document.apply(&edit(6..6, "x", ""), SyncKind::None, OffsetEncoding::Utf16);
        assert!(events.is_empty());
        // Our copy still advances, so re-enabling sync later resyncs from the truth.
        assert_eq!(document.text(), "<?php\nx");
    }

    #[test]
    fn versions_increase_monotonically() {
        let mut document = doc("a");
        let versions: Vec<i32> = (0..5)
            .map(|i| {
                document.apply(&edit(0..0, "x", ""), SyncKind::Incremental, OffsetEncoding::Utf16);
                let _ = i;
                document.version
            })
            .collect();
        assert_eq!(versions, [2, 3, 4, 5, 6]);
    }

    #[test]
    fn an_out_of_range_edit_is_clamped_rather_than_panicking() {
        // A stale offset from a racing caller must not take the process down (§24).
        let mut document = doc("abc");
        document.apply(&edit(99..200, "x", ""), SyncKind::Incremental, OffsetEncoding::Utf16);
        assert_eq!(document.text(), "abcx");
    }

    #[test]
    fn a_mid_codepoint_edit_range_snaps_to_a_boundary() {
        let mut document = doc("ção");
        // Byte 1 is inside 'ç'.
        document.apply(&edit(1..1, "X", ""), SyncKind::Incremental, OffsetEncoding::Utf16);
        assert_eq!(document.text(), "Xção");
    }

    #[test]
    fn reset_sends_a_full_replacement() {
        let mut document = doc("old");
        let events = document.reset("new");
        assert!(events[0].range.is_none());
        assert_eq!(document.text(), "new");
        assert_eq!(document.version, 2);
    }
}
