; Shell (sh/bash) highlighting.
;
; Adapted from tree-sitter-bash's own queries/highlights.scm (MIT), with two changes and
; two additions.
;
; Changed: upstream gives `(variable_name) @property`, which in this editor's palette is
; the colour of a struct field. In shell, `$FOO` and `FOO=1` are the language's variables
; and nothing else competes for the name, so they read as @variable.
;
; Changed: upstream's `@embedded` on command/process substitution and `${...}` expansion
; has no style here, and it captures the *whole* expansion — which under last-wins would
; blank out the variable inside it. Dropped; the pieces inside already colour themselves.
;
; Added: `=` and the test/arithmetic operators, and `(number)`. Upstream colours neither,
; which leaves an assignment-heavy script (which is what a deploy script is) with the
; assignments invisible.

(comment) @comment

[
  (string)
  (raw_string)
  (ansi_c_string)
  (translated_string)
  (heredoc_body)
  (heredoc_start)
] @string

(number) @number
(file_descriptor) @number

; An unquoted right-hand side: `APP_NAME=Laravel`. Upstream captures nothing here, which
; is defensible for a script — a bare word is usually a path or a flag — but a `.env` file
; is *entirely* unquoted assignments, so without this the format that motivated reusing
; this grammar comes out with only its keys coloured. Scoped to the value field so a bare
; word anywhere else (a command argument, a filename) is unaffected.
(variable_assignment value: (word) @string)

(command_name) @function
(function_definition name: (word) @function)

; General first, then the specific arm: `$1`, `$@`, `$?` are their own kind and should not
; be undone by the plain-name pattern. Both land on Variable here, but keeping the order
; right means changing one of them later does what it looks like it does.
(variable_name) @variable
(special_variable_name) @variable

[
  "case" "do" "done" "elif" "else" "esac" "export" "fi" "for" "function"
  "if" "in" "local" "readonly" "select" "then" "unset" "until" "while"
] @keyword

[
  "$" "&&" "||" "|" "|&" ">" ">>" "<" "<<" "<<<" "&" ";;" "=" "=~" "==" "!="
] @operator

; A long-form flag (`--force`) on a command line. Upstream calls it @constant, which maps
; to Number here — wrong colour, but the alternative is leaving every flag in a shell
; script plain, and a flag really is closer to a literal than to prose. Kept as upstream
; has it rather than invented differently.
((command
  (_) @constant)
  (#match? @constant "^-"))
