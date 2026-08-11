; Curated, not upstream-verbatim, like every query in this directory: only captures
; this editor has styles for, and the last-wins rule means order matters.

(line_comment) @comment
(block_comment) @comment

(string_literal) @string
(raw_string_literal) @string
(char_literal) @string

(integer_literal) @number
(float_literal) @number
(boolean_literal) @number

(type_identifier) @type
(primitive_type) @type

(function_item name: (identifier) @function)
(call_expression function: (identifier) @function)
(call_expression function: (field_expression field: (field_identifier) @function))
(macro_invocation macro: (identifier) @function)

(field_identifier) @property

(attribute_item) @attribute
(inner_attribute_item) @attribute

(lifetime) @keyword
(mutable_specifier) @keyword
(crate) @keyword
(super) @keyword
(self) @keyword

[
  "fn" "let" "pub" "use" "mod" "struct" "enum" "impl" "trait" "for" "while"
  "loop" "if" "else" "match" "return" "const" "static" "ref" "move" "async" "await"
  "dyn" "where" "in" "unsafe" "as" "break" "continue" "type" "extern"
] @keyword

[
  "->" "=>" "::" "=" "==" "!=" "<=" ">=" "&&" "||" "+" "-" "*" "/" "%" "&" "|" "!"
  "?" ".." "..="
] @operator
