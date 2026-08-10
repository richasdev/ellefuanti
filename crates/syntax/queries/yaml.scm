; YAML highlighting.
;
; Adapted from tree-sitter-yaml's own queries/highlights.scm (MIT).
;
; Three upstream capture names have no style here and would silently render as plain
; text, so each is remapped to the nearest thing this editor does colour:
;   @boolean         -> @keyword   (`true`/`false`, as json.scm and toml.scm do)
;   @constant.builtin-> @keyword   (`null`; `constant` would land on Number, which is worse)
;   @label           -> @variable  (`&anchor` / `*alias`: a name bound and referred to)
;
; Upstream's @punctuation.delimiter, @punctuation.bracket and @punctuation.special arms
; are dropped. `-`, `:` and `---` are most of the visible characters in a docker-compose
; file; colouring them would make the structure harder to read, not easier — the same
; call css.scm makes.

(comment) @comment

; Values first, keys after: the resolver keeps the last capture for a node, so the
; key patterns below have to follow (string_scalar) or every key goes back to being a
; plain value. Same ordering constraint as json.scm, and the same reason.
[
  (double_quote_scalar)
  (single_quote_scalar)
  (block_scalar)
  (string_scalar)
] @string

[
  (integer_scalar)
  (float_scalar)
] @number

(boolean_scalar) @keyword
(null_scalar) @keyword

[
  (anchor_name)
  (alias_name)
] @variable

; `!!str`, `!Ref` — a type tag, and upstream already calls it one.
(tag) @type

; `%YAML 1.2`, `%TAG`.
[
  (yaml_directive)
  (tag_directive)
  (reserved_directive)
] @attribute

; Mapping keys, in both block (`key: value`) and flow (`{key: value}`) form. This is the
; whole reason to colour YAML: in a docker-compose.yml or a GitHub workflow nearly every
; token is an unquoted scalar, so without this the file is one flat colour.
(block_mapping_pair
  key: (flow_node
    [
      (double_quote_scalar)
      (single_quote_scalar)
    ] @property))

(block_mapping_pair
  key: (flow_node
    (plain_scalar
      (string_scalar) @property)))

(flow_mapping
  (_
    key: (flow_node
      [
        (double_quote_scalar)
        (single_quote_scalar)
      ] @property)))

(flow_mapping
  (_
    key: (flow_node
      (plain_scalar
        (string_scalar) @property))))
