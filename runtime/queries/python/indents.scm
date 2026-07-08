; Python newline indentation signals.
; Conservative Helix-inspired subset adapted to ee's current MVP contract.
; Supported captures today:
; - @indent
; - @dedent
;
; Exclude Helix-only richer captures such as @align, @extend, @opaque, and
; grammar-error recovery rules until ee indent engine supports them.

[
  (list)
  (tuple)
  (dictionary)
  (set)
  (if_statement)
  (for_statement)
  (while_statement)
  (with_statement)
  (try_statement)
  (match_statement)
  (case_clause)
  (import_from_statement)
  (parenthesized_expression)
  (generator_expression)
  (list_comprehension)
  (set_comprehension)
  (dictionary_comprehension)
  (tuple_pattern)
  (list_pattern)
  (argument_list)
  (parameters)
  (binary_operator)
  (function_definition)
  (class_definition)
] @indent

[
  ")"
  "]"
  "}"
] @dedent

(elif_clause "elif" @dedent)
(else_clause "else" @dedent)
(except_clause "except" @dedent)
(finally_clause "finally" @dedent)
