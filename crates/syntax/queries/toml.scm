; TOML highlighting.
;
; Adapted from tree-sitter-toml-ng's own queries/highlights.scm (MIT).
;
; Upstream's key patterns are `(bare_key) @type` followed by `(pair (bare_key)) @property`
; — and that second one captures the whole **pair**, not the key: the capture sits on the
; `pair` node. Under this editor's last-capture-wins rule that paints `name = "app"` as one
; Property-coloured run, swallowing the string. Under upstream's first-wins rule it is
; harmless, because `(bare_key) @type` already claimed the key. Neither reading gives what
; a TOML file actually wants, so the arm is rewritten to capture the key child directly.
;
; The `(bare_key) @type` arm is dropped with it. It exists upstream only so a *table*
; header (`[dependencies]`) gets a colour, since a table's key is not inside a pair. That
; is reproduced below explicitly, which is clearer than relying on an arm's leftovers.

(comment) @comment

; Table and array-of-tables headers: `[package]`, `[[bin]]`. Type rather than Property so
; a section header does not read as just another key — in a Cargo.toml or a phpunit config
; the headers are the structure and the keys are the content.
(table (bare_key) @type)
(table (dotted_key) @type)
(table_array_element (bare_key) @type)
(table_array_element (dotted_key) @type)

; Keys. The dotted form (`a.b.c = 1`) nests bare_keys, so it is matched separately rather
; than relying on the outer capture reaching them.
(pair (bare_key) @property)
(pair (dotted_key (bare_key) @property))
(pair (quoted_key) @property)

(string) @string

[
  (integer)
  (float)
] @number

; Upstream gives dates @string.special, which this editor has no style for and would drop
; to plain text. A timestamp is a literal value, so it reads as one.
[
  (offset_date_time)
  (local_date_time)
  (local_date)
  (local_time)
] @number

; @boolean is not a capture name this editor maps — `true`/`false` read as keywords, the
; same choice json.scm makes for the same tokens.
(boolean) @keyword

"=" @operator
