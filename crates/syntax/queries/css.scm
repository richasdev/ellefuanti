; CSS highlighting.
;
; Adapted from tree-sitter-css's own queries/highlights.scm (MIT).
;
; Two deliberate departures from upstream. Upstream's @punctuation.delimiter and
; @punctuation.bracket arms are dropped: this editor has no punctuation style, and
; mapping them onto Operator would paint every `{`, `}`, `;` and `:` in a stylesheet —
; which is most of its characters — leaving CSS looking less structured, not more.
;
; Upstream gives (unit) @type, so `px` in `10px` is a different colour from the `10`.
; Kept, because it is upstream's choice and it reads correctly.

(comment) @comment

(tag_name) @tag
(nesting_selector) @tag
(universal_selector) @tag

[
  "~" ">" "+" "-" "*" "/" "="
  "^=" "|=" "~=" "$=" "*="
  "and" "or" "not" "only"
] @operator

(attribute_selector (plain_value) @string)

(class_name) @property
(id_name) @property
(namespace_name) @property
(property_name) @property
(feature_name) @property

; Custom properties (`--brand-color`) are the closest thing CSS has to a variable, and
; the #match? predicate is how upstream separates them from ordinary property names.
;
; Moved *below* the general (property_name) arm, which is where upstream has it above.
; Upstream's own highlighter resolves a node captured twice by taking the first match;
; this one takes the last (see `query_spans`), so the specific arm has to come second or
; `--brand` is immediately overwritten back to Property. The two conventions disagree and
; only one of them can be the file's; this is that choice, written down where the next
; person to copy a query from upstream will trip over it.
((property_name) @variable
 (#match? @variable "^--"))
((plain_value) @variable
 (#match? @variable "^--"))

(pseudo_element_selector (tag_name) @attribute)
(pseudo_class_selector (class_name) @attribute)
(attribute_name) @attribute

(function_name) @function

[
  "@media" "@import" "@charset" "@namespace" "@supports" "@keyframes"
] @keyword
(at_keyword) @keyword
(to) @keyword
(from) @keyword
(important) @keyword

(string_value) @string
; A colour literal is a value, not prose — upstream gives it @string.special. Folded
; into Number rather than String so `#fff` does not read as a quoted string.
(color_value) @number

(integer_value) @number
(float_value) @number
(unit) @type
