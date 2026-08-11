; Markdown block grammar (tree-sitter-md). Headings loud, code literal, structure quiet —
; prose itself stays uncoloured, the same rule the HTML query follows for text content.

(atx_heading) @keyword
(setext_heading) @keyword

(fenced_code_block) @string
(indented_code_block) @string

(block_quote) @comment

(thematic_break) @operator

(list_marker_minus) @operator
(list_marker_plus) @operator
(list_marker_star) @operator
(list_marker_dot) @operator

(link_destination) @attribute
(link_label) @property
