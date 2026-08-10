; TypeScript highlighting — the TypeScript-only additions.
;
; Adapted from tree-sitter-typescript's own queries/highlights.scm (MIT), which is
; written as an *extension*: upstream expects the JavaScript query to be concatenated
; ahead of it, and on its own it would leave strings, comments and calls unstyled.
; highlight.rs does that concatenation, so this file stays a diff against JavaScript
; rather than a copy of it that then has to be kept in sync.
;
; Being appended also means these patterns come last and therefore win, which is what
; makes `type_identifier` beat the `(identifier) @variable` that JavaScript opens with.

(type_identifier) @type
(predefined_type) @type

((identifier) @type
 (#match? @type "^[A-Z]"))

; Parameters are `variable` rather than a parameter style of their own: this editor
; has no Parameter, and Variable is the honest approximation.
(required_parameter (identifier) @variable)
(optional_parameter (identifier) @variable)

[
  "abstract" "declare" "enum" "export" "implements" "interface" "keyof" "namespace"
  "private" "protected" "public" "type" "readonly" "override" "satisfies"
] @keyword
