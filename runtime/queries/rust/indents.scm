; Rust newline indentation signals.
; Conservative Helix-inspired subset adapted to ee's current MVP contract.
; Supported captures today:
; - @indent  => following line gains one indent level
; - @dedent  => current closer line drops one indent level
;
; Keep scope bounded. Do not use Helix-only captures such as @align/@opaque
; until ee indent engine supports richer outcomes.

[
  (use_list)
  (block)
  (match_block)
  (arguments)
  (parameters)
  (declaration_list)
  (field_declaration_list)
  (field_initializer_list)
  (struct_pattern)
  (tuple_pattern)
  (unit_expression)
  (enum_variant_list)
  (call_expression)
  (binary_expression)
  (field_expression)
  (await_expression)
  (tuple_expression)
  (array_expression)
  (where_clause)
  (type_cast_expression)
  (token_tree)
  (macro_definition)
  (token_tree_pattern)
  (token_repetition)
] @indent

[
  "}"
  "]"
  ")"
] @dedent
