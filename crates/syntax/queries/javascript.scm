; JavaScript highlighting.
;
; Adapted from tree-sitter-javascript's own queries/highlights.scm (MIT).
;
; Order is load-bearing. `(identifier) @variable` matches nearly every name in the
; file, so it comes first and the specific patterns below overwrite it — the resolver
; in highlight.rs keeps the *last* capture for a node, which is the convention these
; upstream query files are written against. Reversing this file paints every function
; call as a plain variable.
;
; Upstream's punctuation and bracket arms are dropped: no punctuation style here, and
; folding them into Operator would colour every brace and semicolon in the file.

(identifier) @variable

(property_identifier) @property

; Definitions.
(function_expression name: (identifier) @function)
(function_declaration name: (identifier) @function)
(method_definition name: (property_identifier) @function)

(pair
  key: (property_identifier) @function
  value: [(function_expression) (arrow_function)])

(variable_declarator
  name: (identifier) @function
  value: [(function_expression) (arrow_function)])

(assignment_expression
  left: (identifier) @function
  right: [(function_expression) (arrow_function)])

; Calls.
(call_expression function: (identifier) @function)
(call_expression
  function: (member_expression property: (property_identifier) @function))

; A capitalised bare identifier is a constructor by convention — this is how upstream
; gets `new Foo()` and JSX component names to read as types.
((identifier) @type
 (#match? @type "^[A-Z]"))

(this) @variable
(super) @variable

[
  (true)
  (false)
  (null)
  (undefined)
] @keyword

(comment) @comment

[
  (string)
  (template_string)
] @string

(regex) @string
(number) @number

[
  "-" "--" "-=" "+" "++" "+=" "*" "*=" "**" "**=" "/" "/=" "%" "%="
  "<" "<=" "<<" "<<=" "=" "==" "===" "!" "!=" "!==" "=>" ">" ">=" ">>" ">>=" ">>>" ">>>="
  "~" "^" "&" "|" "^=" "&=" "|=" "&&" "||" "??" "&&=" "||=" "??="
] @operator

[
  "as" "async" "await" "break" "case" "catch" "class" "const" "continue" "debugger"
  "default" "delete" "do" "else" "export" "extends" "finally" "for" "from" "function"
  "get" "if" "import" "in" "instanceof" "let" "new" "of" "return" "set" "static"
  "switch" "target" "throw" "try" "typeof" "var" "void" "while" "with" "yield"
] @keyword
