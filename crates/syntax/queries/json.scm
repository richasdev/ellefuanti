; JSON highlighting.
;
; Adapted from tree-sitter-json's own queries/highlights.scm (MIT). Trimmed to the
; capture names this editor has styles for: upstream's @string.special.key becomes
; @property and @constant.builtin becomes @keyword, because a theme here colours
; twelve things and inventing a thirteenth for `true` is not worth a theme field.
;
; The key/value distinction is the whole point of colouring JSON: in composer.json
; every line is a string, and if keys and values share a colour the file reads as one
; undifferentiated block.

; General first, specific second: the resolver keeps the last capture for a node, so
; the key pattern below must follow (string) or it is immediately overwritten and every
; key goes back to being a plain string.
(string) @string

(pair
  key: (string) @property)

(number) @number

[
  (null)
  (true)
  (false)
] @keyword

(escape_sequence) @operator

(comment) @comment
