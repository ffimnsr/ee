[
  (import_spec_list)
  (const_declaration)
  (var_declaration)
  (type_parameter_list)
  (parameter_list)
  (type_declaration)
  (parenthesized_type)
  (type_arguments)
  (array_type)
  (implicit_length_array_type)
  (slice_type)
  (field_declaration_list)
  (interface_type)
  (map_type)
  (block)
  (expression_switch_statement)
  (type_switch_statement)
  (select_statement)
  (parenthesized_expression)
  (argument_list)
  (index_expression)
  (slice_expression)
  (type_assertion_expression)
  (type_conversion_expression)
  (literal_value)
] @rainbow.scope

[
  "(" ")"
  "[" "]"
  "{" "}"
] @rainbow.bracket


[
  (expression)
  (component_switch_statement)
  (component_render)
  (component_children_expression)
  (doctype)
  (spread_attributes)
  (conditional_attribute_block)
  (css_declaration)
  (dynamic_class_attribute_value)
  (component_block)
  (script_block)
  (rawgo_block)
  (literal_value)
] @rainbow.scope

[
  "{"
  "}"
  "{!"
  "}"
  "{{"
  "}}"
  "<"
  ">"
  "</"
  "/>"
  "<!"
] @rainbow.bracket

([
    (element)
    (style_element)
    (script_element)
  ] @rainbow.scope
  (#set! rainbow.include-children))