[
  (export_clause)
  (named_imports)
  (statement_block)
  (for_statement)
  (for_in_statement)
  (switch_body)
  (catch_clause "(" ")")
  (parenthesized_expression)
  (object)
  (object_pattern)
  (array)
  (array_pattern)
  (subscript_expression)
  (template_substitution)
  (arguments)
  (class_body)
  (formal_parameters)
  (computed_property_name)
] @rainbow.scope

[
  "(" ")"
  "${" "{" "}"
  "[" "]"
] @rainbow.bracket

(regex "/" @rainbow.bracket) @rainbow.scope


[
  (jsx_expression)
] @rainbow.scope

(jsx_opening_element ["<" ">"] @rainbow.bracket) @rainbow.scope
(jsx_closing_element ["</" ">"] @rainbow.bracket) @rainbow.scope
(jsx_self_closing_element ["<" "/>"] @rainbow.bracket) @rainbow.scope