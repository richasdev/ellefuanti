; HTML highlighting.
;
; Adapted from tree-sitter-html's own queries/highlights.scm (MIT).
;
; Upstream's @punctuation.bracket arm on `<`, `>`, `</`, `/>` is dropped for the same
; reason CSS's is: this editor has no punctuation style, and folding the angle brackets
; into Operator would colour them differently from the tag name they belong to, which
; reads as noise rather than structure.
;
; @tag.error on a mismatched closing tag is kept as plain @tag. `capture_style` matches
; on the segment before the first dot, so it lands on Tag either way — but an editor that
; renders a typo'd `</dvi>` in the same colour as a correct one is the honest state of
; things here: nothing downstream distinguishes them, so pretending to would be a lie in
; the query file.

(tag_name) @tag
(erroneous_end_tag_name) @tag

; `<!DOCTYPE html>` — upstream calls it @constant, which this editor maps to Number.
; A doctype is markup, not a value, so it reads as a Tag instead.
(doctype) @tag

(attribute_name) @attribute
(attribute_value) @string
(quoted_attribute_value) @string

(comment) @comment
