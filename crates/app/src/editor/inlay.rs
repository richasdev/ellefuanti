//! Inlay hints (#93 follow-up): the server's type and parameter-name annotations, resolved
//! to byte offsets and ready to splice into a shaped line.
//!
//! # Why this module is pure
//!
//! Everything here is a function over data — no gpui, no client, no view. The parts that can
//! silently be *wrong* are all arithmetic: which byte a `Position` names, which hints land on
//! a given row, and where in the line's runs each one goes. A hint drawn at the wrong column
//! is worse than no hint at all, because it misattributes a type to a variable that does not
//! have it, so that arithmetic is tested rather than eyeballed against a running server.
//!
//! # Resolved once, not per frame
//!
//! LSP positions become byte offsets when the response lands, not while rendering — the same
//! rule `lsp_session::FileDiagnostics` follows, and for the same reason: the editor repaints
//! these on every frame, and a UTF-16 conversion in the render pass would put a line-index
//! build on the frame budget for nothing.

use std::ops::Range;

use elle_lsp::lsp_types::{self, InlayHint, InlayHintKind, InlayHintLabel};

/// One hint, already in this codebase's units and with its label flattened.
///
/// Deliberately not `lsp_types::InlayHint`: that carries a `Position` in the server's
/// encoding, a label that is one of two shapes, and five fields for features that are
/// explicitly out of scope (resolve, tooltips, commands, navigation). Reducing at the
/// boundary is what keeps the renderer from re-deciding any of that per frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedHint {
    /// The byte offset in *our* buffer that this hint sits before.
    pub offset: usize,
    /// The text to draw, padding already applied — see [`hint_text`].
    pub text: String,
    /// What the server called it. Kept because Type and Parameter are told apart visually.
    pub kind: Option<HintKind>,
}

/// The two kinds the protocol defines, closed so the renderer matches exhaustively.
///
/// Not `lsp_types::InlayHintKind` for `Severity`'s reason: that is an open integer newtype
/// with no exhaustive match, and an unknown kind arriving as a third value must be a
/// deliberate decision here rather than a fallthrough that paints it as something it is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintKind {
    Type,
    Parameter,
}

impl HintKind {
    fn from_lsp(kind: InlayHintKind) -> Option<Self> {
        match kind {
            InlayHintKind::TYPE => Some(Self::Type),
            InlayHintKind::PARAMETER => Some(Self::Parameter),
            // An unknown kind still renders — the label is the useful part, and dropping a
            // hint because its kind is unrecognised would hide information the server did
            // send. It just gets the undifferentiated style.
            _ => None,
        }
    }
}

/// Flattens a label into the string to draw.
///
/// The protocol allows two shapes and servers use both: Intelephense sends plain strings,
/// rust-analyzer sends parts. The parts carry per-part tooltips, locations and commands —
/// all of which are the interactive features #93's scope explicitly excludes — so joining
/// their `value`s is the whole of what renders today. Written as one function so the two
/// shapes cannot drift apart in the caller.
pub fn flatten_label(label: &InlayHintLabel) -> String {
    match label {
        InlayHintLabel::String(text) => text.clone(),
        // No separator: the parts are a *split* of one label, not a list. `["Vec", "<", "T",
        // ">"]` is `Vec<T>`, and joining with anything would corrupt every composite type.
        InlayHintLabel::LabelParts(parts) => parts.iter().map(|part| part.value.as_str()).collect(),
    }
}

/// The label with `padding_left`/`padding_right` applied.
///
/// The padding flags are the server asking for breathing room *outside* the label, which
/// matters because a hint butts directly against real code: `$x: int` needs the space after
/// the colon that the server signals rather than one this client invents. Baking it into the
/// text rather than into the layout keeps the splice a single run — one string, one width.
pub fn hint_text(hint: &InlayHint) -> String {
    let label = flatten_label(&hint.label);
    let left = if hint.padding_left.unwrap_or(false) { " " } else { "" };
    let right = if hint.padding_right.unwrap_or(false) { " " } else { "" };
    format!("{left}{label}{right}")
}

/// Turns the server's hints into byte-offset ones against `text`.
///
/// `to_offset` converts one `Position` in the server's encoding — the caller passes the
/// client's own converter, so this function stays free of both the encoding question and the
/// document it would need to answer it.
///
/// Hints whose label is empty after flattening are dropped: an empty run would shape to zero
/// width and cost a run for nothing. Sorted by offset, because the renderer walks a line's
/// hints left to right and a server is under no obligation to send them in order.
pub fn resolve(
    hints: &[InlayHint],
    mut to_offset: impl FnMut(lsp_types::Position) -> usize,
) -> Vec<ResolvedHint> {
    let mut resolved: Vec<ResolvedHint> = hints
        .iter()
        .filter_map(|hint| {
            let text = hint_text(hint);
            if text.trim().is_empty() {
                return None;
            }
            Some(ResolvedHint {
                offset: to_offset(hint.position),
                text,
                kind: hint.kind.and_then(HintKind::from_lsp),
            })
        })
        .collect();
    resolved.sort_by_key(|hint| hint.offset);
    resolved
}

/// The hints falling inside one line, as `(offset_within_line, hint)`.
///
/// `line` is the byte range of the row in the buffer, end-exclusive of the newline. The
/// returned offsets are **relative to the line start**, which is the unit `styled_line`
/// splices in — converting here rather than at the call site keeps the subtraction, and the
/// chance of doing it twice, in one tested place.
///
/// A hint exactly at `line.end` belongs to this line, not the next: that is the
/// end-of-line position, where a return-type hint sits. A hint past the end is clamped out
/// rather than clamped *to* the end — a hint whose offset no longer lies on this row
/// describes text that has moved, and drawing it somewhere plausible would be a guess.
pub fn hints_on_line(
    hints: &[ResolvedHint],
    line: Range<usize>,
) -> impl Iterator<Item = (usize, &ResolvedHint)> {
    hints
        .iter()
        .filter(move |hint| hint.offset >= line.start && hint.offset <= line.end)
        .map(move |hint| (hint.offset - line.start, hint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use elle_lsp::lsp_types::{InlayHintLabelPart, Position};

    fn hint(line: u32, character: u32, label: InlayHintLabel) -> InlayHint {
        InlayHint {
            position: Position { line, character },
            label,
            kind: None,
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        }
    }

    fn part(value: &str) -> InlayHintLabelPart {
        InlayHintLabelPart { value: value.to_string(), ..Default::default() }
    }

    // --- label flattening, both shapes -------------------------------------------------

    #[test]
    fn a_string_label_is_its_own_text() {
        assert_eq!(flatten_label(&InlayHintLabel::String("int".into())), "int");
    }

    #[test]
    fn label_parts_join_with_nothing_between_them() {
        // The parts are a split of one label, not a list of labels. Any separator here
        // would render `array<string>` as `array< string >` or worse.
        let label =
            InlayHintLabel::LabelParts(vec![part("array"), part("<"), part("string"), part(">")]);
        assert_eq!(flatten_label(&label), "array<string>");
    }

    #[test]
    fn an_empty_parts_list_flattens_to_nothing() {
        assert_eq!(flatten_label(&InlayHintLabel::LabelParts(Vec::new())), "");
    }

    // --- padding -----------------------------------------------------------------------

    #[test]
    fn padding_flags_become_spaces_around_the_label() {
        let mut h = hint(0, 0, InlayHintLabel::String("int".into()));
        assert_eq!(hint_text(&h), "int", "no flags means no padding");

        h.padding_left = Some(true);
        assert_eq!(hint_text(&h), " int");

        h.padding_right = Some(true);
        assert_eq!(hint_text(&h), " int ");

        h.padding_left = Some(false);
        assert_eq!(hint_text(&h), "int ", "an explicit false is the same as absent");
    }

    #[test]
    fn padding_applies_to_a_parts_label_too() {
        // The two shapes must not diverge: this is why padding is applied after flattening
        // rather than inside either branch.
        let mut h = hint(0, 0, InlayHintLabel::LabelParts(vec![part("string"), part("|null")]));
        h.padding_left = Some(true);
        assert_eq!(hint_text(&h), " string|null");
    }

    // --- position → offset --------------------------------------------------------------

    #[test]
    fn resolve_converts_positions_and_sorts_by_offset() {
        // A deliberately out-of-order response: the protocol does not promise ordering, and
        // the renderer walks a line left to right.
        let hints = vec![
            hint(0, 20, InlayHintLabel::String("b".into())),
            hint(0, 5, InlayHintLabel::String("a".into())),
        ];
        // A stand-in converter: character *is* the byte offset for ASCII, which is all this
        // test is about — the real encoding conversion is `elle_lsp::offset`'s own tested job.
        let resolved = resolve(&hints, |position| position.character as usize);
        assert_eq!(resolved.iter().map(|h| h.offset).collect::<Vec<_>>(), vec![5, 20]);
        assert_eq!(resolved.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn an_empty_label_is_dropped_rather_than_drawn() {
        // A zero-width run costs a run and shapes to nothing. Servers do send these when a
        // hint's whole content was meant to arrive via `inlayHint/resolve`, which is out of
        // scope — so the honest rendering is no hint, not an empty one.
        let hints = vec![
            hint(0, 1, InlayHintLabel::String(String::new())),
            hint(0, 2, InlayHintLabel::String("   ".into())),
            hint(0, 3, InlayHintLabel::String("real".into())),
        ];
        let resolved = resolve(&hints, |position| position.character as usize);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].text, "real");
    }

    #[test]
    fn kinds_are_reduced_and_an_unknown_one_still_renders() {
        let mut type_hint = hint(0, 1, InlayHintLabel::String("int".into()));
        type_hint.kind = Some(InlayHintKind::TYPE);
        let mut param_hint = hint(0, 2, InlayHintLabel::String("name:".into()));
        param_hint.kind = Some(InlayHintKind::PARAMETER);
        let mut odd = hint(0, 3, InlayHintLabel::String("?".into()));
        // Built through serde rather than a literal: the newtype's field is private, and a
        // kind this client does not know about is exactly something that arrives on the
        // wire from a server implementing a newer specification.
        odd.kind = Some(serde_json::from_str::<InlayHintKind>("99").expect("a kind is an integer"));

        let resolved = resolve(&[type_hint, param_hint, odd], |p| p.character as usize);
        assert_eq!(resolved[0].kind, Some(HintKind::Type));
        assert_eq!(resolved[1].kind, Some(HintKind::Parameter));
        // Unknown kind: still drawn, just undifferentiated. Dropping it would hide a label
        // the server did send.
        assert_eq!(resolved[2].kind, None);
        assert_eq!(resolved[2].text, "?");
    }

    // --- which hints fall on a line -----------------------------------------------------

    fn at(offset: usize) -> ResolvedHint {
        ResolvedHint { offset, text: format!("h{offset}"), kind: None }
    }

    #[test]
    fn hints_on_line_selects_the_row_and_rebases_to_the_line_start() {
        let hints = vec![at(2), at(12), at(15), at(30)];
        // Line two spans bytes 10..20.
        let found: Vec<_> = hints_on_line(&hints, 10..20).collect();
        assert_eq!(found.iter().map(|(column, _)| *column).collect::<Vec<_>>(), vec![2, 5]);
        assert_eq!(found[0].1.offset, 12, "the hint itself keeps its document offset");
    }

    #[test]
    fn a_hint_at_the_line_end_belongs_to_that_line() {
        // End-of-line is where a return-type hint sits — `function f()|: int`. Excluding it
        // would drop exactly the hint kind most likely to be there.
        let hints = vec![at(20)];
        let found: Vec<_> = hints_on_line(&hints, 10..20).collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, 10, "rebased to the very end of the line");
        // …and it is not also claimed by the following line.
        assert_eq!(hints_on_line(&hints, 21..30).count(), 0);
    }

    #[test]
    fn a_hint_outside_the_line_is_left_alone_not_clamped_into_it() {
        // Clamping would draw a hint about other text at this row's edge — a wrong column,
        // which is the failure this module exists to avoid.
        let hints = vec![at(5), at(50)];
        assert_eq!(hints_on_line(&hints, 10..20).count(), 0);
    }

    #[test]
    fn an_empty_line_can_still_carry_a_hint() {
        // A blank row is `start..start`; a hint there is at column zero.
        let hints = vec![at(10)];
        let found: Vec<_> = hints_on_line(&hints, 10..10).collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, 0);
    }
}
